use std::collections::VecDeque;
use std::ffi::{c_char, CStr, CString};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock, RwLock,
};
use std::time::{Duration, Instant};

use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tp_client::client_ui::{
    project_client_ui_status, project_engine_runtime_snapshot, project_native_routing,
    ClientUiStatusV2, NativeRoutingApplyResultV2, NativeRoutingV2,
};
use tp_client::runtime_snapshot::V2RuntimeSnapshot;
use tp_client::status::{ConnectionStatus, StatusListener};
use tp_client::{Engine, EngineConfig};
use tp_core::config::ClientP2pConfig;
use tp_core::provisioning::{PeerBootstrapV2, PeerProfileV2};
use tracing_subscriber::fmt as tsfmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub const TP_MOBILE_OK: i32 = 0;
pub const TP_MOBILE_INVALID_ARGUMENT: i32 = -1;
pub const TP_MOBILE_INVALID_JSON: i32 = -2;
pub const TP_MOBILE_INVALID_CONFIG: i32 = -3;
pub const TP_MOBILE_ALREADY_RUNNING: i32 = -4;
pub const TP_MOBILE_START_FAILED: i32 = -5;

const DEFAULT_PLATFORM_URL: &str = "https://lantunnel.app";
const DEFAULT_LOCAL_SOCKS5_LISTEN: &str = "127.0.0.1:1080";
const DEFAULT_LOG_LEVEL: &str = "info";
const MOBILE_STAGE_TIMEOUT: Duration = Duration::from_secs(60);
const MOBILE_RELAY_READY_TIMEOUT: Duration = Duration::from_secs(30);
const MOBILE_PROXY_READY_TIMEOUT: Duration = Duration::from_secs(240);
const MOBILE_LOG_BUFFER_MAX_LINES: usize = 5_000;
const MOBILE_LOG_BUFFER_MAX_BYTES: usize = 2 * 1024 * 1024;
const START_CANCELLED_MESSAGE: &str = "mobile proxy start cancelled";

fn default_local_socks5_listen() -> String {
    DEFAULT_LOCAL_SOCKS5_LISTEN.into()
}

fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.into()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartProxyRequest {
    pub peer_profile: PeerProfileV2,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default = "default_local_socks5_listen")]
    pub local_socks5_listen: String,
    #[serde(default)]
    pub p2p_allow_lan_candidates: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub insecure_tls: bool,
    /// Who may reach this device.
    ///
    /// Absent means open, matching the desktop settings. Before this existed
    /// the mobile runtime installed nothing, so it ran on the engine's startup
    /// placeholder — which refuses everything — and a phone was unreachable
    /// whatever its owner intended.
    #[serde(default)]
    pub client_access: tp_client::access_policy::ClientAccessPolicyV2,
    /// Networks this device publishes to the Tunnel.
    ///
    /// A phone is a Peer like any other. The request carried no exports at
    /// all, so the runtime installed no local record and a phone could reach
    /// the Tunnel but never share anything with it.
    #[serde(default)]
    pub exported_lans: Vec<String>,
    /// Keep a locally reachable network on the local path when the Tunnel
    /// offers an overlapping one.
    #[serde(default)]
    pub tunnel_first: bool,
}

/// The local runtime record a start request asks for.
///
/// Mirrors the desktop: a prefix is published only when it is exactly one the
/// device is currently connected to, so an export nobody can reach is carried
/// as configured-but-withdrawn rather than advertised.
fn compiled_local_runtime_record(
    req: &StartProxyRequest,
) -> Result<tp_client::peer_runtime::PeerRuntimeRecordV2, MobileError> {
    use tp_client::peer_runtime::{LanExportPrefixV2, LanExportV2, PeerRuntimeRecordV2};

    // Only walk the interfaces when there is something to match against them.
    // A phone that publishes nothing is the common case, and enumerating every
    // adapter on every start is a system call it has no use for.
    if req.exported_lans.is_empty() {
        return PeerRuntimeRecordV2::new(Vec::new()).map_err(|error| {
            MobileError::invalid_config(format!("invalid exported LANs: {error}"))
        });
    }
    let connected = tp_client::discover_connected_lan_prefixes().ok();
    let mut exports = Vec::with_capacity(req.exported_lans.len());
    for value in &req.exported_lans {
        let invalid = || MobileError::invalid_config(format!("invalid exported LAN: {value}"));
        let (network, prefix_len) = value.split_once('/').ok_or_else(invalid)?;
        let network = network.parse().map_err(|_| invalid())?;
        let prefix_len = prefix_len.parse().map_err(|_| invalid())?;
        let prefix = LanExportPrefixV2::new(network, prefix_len).map_err(|_| invalid())?;
        if *value != format!("{}/{}", prefix.network, prefix.prefix_len) {
            return Err(invalid());
        }
        exports.push(LanExportV2 {
            prefix,
            ready: connected
                .as_deref()
                .is_some_and(|connected| connected.contains(&prefix)),
        });
    }
    PeerRuntimeRecordV2::new(exports)
        .map_err(|error| MobileError::invalid_config(format!("invalid exported LANs: {error}")))
}

/// Compiles the policy a start request carries, so an unusable rule fails the
/// start instead of being silently dropped into "refuse everything".
fn compiled_client_access(
    req: &StartProxyRequest,
) -> Result<tp_client::access_policy::ClientAccessPolicyV2, MobileError> {
    tp_client::access_policy::CompiledClientAccessPolicyV2::compile(&req.client_access).map_err(
        |error| MobileError::invalid_config(format!("invalid Client access rule: {error}")),
    )?;
    Ok(req.client_access.clone())
}

#[derive(Clone)]
struct ValidatedStartConfig {
    listen_addr: SocketAddr,
    p2p: ClientP2pConfig,
    device_id: String,
    device_name: Option<String>,
    peer_profile: PeerProfileV2,
    platform_url: String,
    client_access: tp_client::access_policy::ClientAccessPolicyV2,
    local_runtime_record: tp_client::peer_runtime::PeerRuntimeRecordV2,
    // TODO: the phone validates and carries Tunnel First but nothing reads it.
    // No mobile path calls `Engine::v2_native_lan_route_cidrs`, so the setting
    // currently decides nothing on a phone. Wire it up or drop the field.
    #[allow(dead_code)]
    tunnel_first: bool,
}

#[derive(Debug, Clone)]
struct MobileError {
    code: i32,
    message: String,
}

impl MobileError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(TP_MOBILE_INVALID_ARGUMENT, message)
    }

    fn invalid_json(message: impl Into<String>) -> Self {
        Self::new(TP_MOBILE_INVALID_JSON, message)
    }

    fn invalid_config(message: impl Into<String>) -> Self {
        Self::new(TP_MOBILE_INVALID_CONFIG, message)
    }

    fn already_running() -> Self {
        Self::new(TP_MOBILE_ALREADY_RUNNING, "mobile proxy is already running")
    }

    fn start_failed(message: impl Into<String>) -> Self {
        Self::new(TP_MOBILE_START_FAILED, message)
    }

    fn code(&self) -> i32 {
        self.code
    }
}

impl std::fmt::Display for MobileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for MobileError {}

#[derive(Clone)]
struct MobileLogBufferWriter {
    buf: Arc<Mutex<MobileLogBuffer>>,
}

struct MobileLogBufferHandle {
    buf: Arc<Mutex<MobileLogBuffer>>,
}

#[derive(Debug, Default)]
struct MobileLogBuffer {
    lines: VecDeque<String>,
    bytes: usize,
}

impl MobileLogBuffer {
    fn push(&mut self, line: String) {
        if line.len() > MOBILE_LOG_BUFFER_MAX_BYTES {
            return;
        }
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines.push_back(line);
        self.trim();
    }

    fn tail(&self, limit: usize) -> Vec<String> {
        let take = limit.min(self.lines.len());
        self.lines.iter().rev().take(take).rev().cloned().collect()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }

    fn trim(&mut self) {
        while self.lines.len() > MOBILE_LOG_BUFFER_MAX_LINES
            || self.bytes > MOBILE_LOG_BUFFER_MAX_BYTES
        {
            let Some(removed) = self.lines.pop_front() else {
                self.bytes = 0;
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.len());
        }
    }
}

impl<'a> MakeWriter<'a> for MobileLogBufferWriter {
    type Writer = MobileLogBufferHandle;

    fn make_writer(&'a self) -> Self::Writer {
        MobileLogBufferHandle {
            buf: self.buf.clone(),
        }
    }
}

impl Write for MobileLogBufferHandle {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(bytes);
        for line in text.split_terminator('\n') {
            if line.is_empty() {
                continue;
            }
            let mut buf = self
                .buf
                .lock()
                .map_err(|_| std::io::Error::other("mobile log buffer poisoned"))?;
            buf.push(line.to_string());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type MobileSetLogLevelFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

struct MobileLogging {
    buf: Arc<Mutex<MobileLogBuffer>>,
    set_level: MobileSetLogLevelFn,
    level: Arc<RwLock<String>>,
}

static MOBILE_LOGGING: OnceLock<MobileLogging> = OnceLock::new();

fn init_mobile_logging(initial_level: &str) -> Result<(), MobileError> {
    let level = sanitize_log_level(initial_level);
    if let Some(logging) = MOBILE_LOGGING.get() {
        (logging.set_level)(&level)
            .map_err(|e| MobileError::start_failed(format!("set mobile log level failed: {e}")))?;
        return Ok(());
    }

    let filter = EnvFilter::try_new(&level).unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL));
    let (filter_layer, reload_handle) = reload::Layer::new(filter);
    let buf = Arc::new(Mutex::new(MobileLogBuffer::default()));
    let writer = MobileLogBufferWriter { buf: buf.clone() };
    let level_slot = Arc::new(RwLock::new(level.clone()));
    let level_for_closure = level_slot.clone();
    let set_level: MobileSetLogLevelFn = Arc::new(move |new_level: &str| {
        let sanitized = sanitize_log_level(new_level);
        let filter = EnvFilter::try_new(&sanitized).map_err(|e| e.to_string())?;
        reload_handle.reload(filter).map_err(|e| e.to_string())?;
        *level_for_closure
            .write()
            .map_err(|_| "mobile log level lock poisoned".to_string())? = sanitized;
        Ok(())
    });

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tsfmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true)
                .compact(),
        )
        .try_init()
        .map_err(|e| MobileError::start_failed(format!("mobile logging init failed: {e}")))?;

    let _ = MOBILE_LOGGING.set(MobileLogging {
        buf,
        set_level,
        level: level_slot,
    });
    Ok(())
}

fn sanitize_log_level(level: &str) -> String {
    let trimmed = level.trim();
    if trimmed.is_empty() {
        DEFAULT_LOG_LEVEL.into()
    } else {
        trimmed.into()
    }
}

fn mobile_logs_json(limit: usize) -> Result<String, MobileError> {
    let lines = if let Some(logging) = MOBILE_LOGGING.get() {
        let buf = logging
            .buf
            .lock()
            .map_err(|_| MobileError::start_failed("mobile log buffer poisoned"))?;
        buf.tail(limit)
    } else {
        Vec::new()
    };
    serde_json::to_string(&lines)
        .map_err(|e| MobileError::start_failed(format!("logs serialize failed: {e}")))
}

fn clear_mobile_logs() -> Result<(), MobileError> {
    if let Some(logging) = MOBILE_LOGGING.get() {
        logging
            .buf
            .lock()
            .map_err(|_| MobileError::start_failed("mobile log buffer poisoned"))?
            .clear();
    }
    Ok(())
}

fn set_mobile_log_level(level: &str) -> Result<(), MobileError> {
    if let Some(logging) = MOBILE_LOGGING.get() {
        (logging.set_level)(level)
            .map_err(|e| MobileError::start_failed(format!("set log level failed: {e}")))?;
        Ok(())
    } else {
        init_mobile_logging(level)
    }
}

fn mobile_log_config_json() -> Result<String, MobileError> {
    let level = MOBILE_LOGGING
        .get()
        .and_then(|logging| logging.level.read().ok().map(|level| level.clone()))
        .unwrap_or_else(|| DEFAULT_LOG_LEVEL.into());
    serde_json::to_string(&serde_json::json!({ "level": level }))
        .map_err(|e| MobileError::start_failed(format!("log config serialize failed: {e}")))
}

trait ProxyHandle: Send {
    fn status_json(&self) -> Result<String, MobileError> {
        serde_json::to_string(&MobileRuntimeStatus {
            running: true,
            native_version: env!("CARGO_PKG_VERSION"),
            listen_addr: None,
            connection: None,
            p2p: None,
            clash_overlay_available: false,
            startup: None,
            last_error: None,
            this_peer: None,
            peer_directory: Default::default(),
            relay_usage: None,
            client_ui: project_client_ui_status(
                &ConnectionStatus::default(),
                project_engine_runtime_snapshot(
                    V2RuntimeSnapshot::default(),
                    mobile_native_routing(false),
                ),
            ),
        })
        .map_err(|e| MobileError::start_failed(format!("status serialize failed: {e}")))
    }

    fn clash_overlay_yaml(&self) -> Result<String, MobileError> {
        Err(MobileError::start_failed("V2 runtime config unavailable"))
    }

    fn runtime_config_json(&self) -> Result<String, MobileError> {
        Err(MobileError::start_failed(
            "mobile runtime config unavailable",
        ))
    }

    fn stop(self: Box<Self>);
}

#[derive(Default)]
struct MobileProxyState {
    handle: Mutex<Option<Box<dyn ProxyHandle>>>,
    starting: Mutex<bool>,
    start_cancel: Mutex<Option<StartupCancel>>,
    startup: Arc<Mutex<MobileStartupStatus>>,
    last_error: Mutex<Option<MobileRuntimeError>>,
}

#[derive(Clone, Debug, Default)]
struct StartupCancel {
    cancelled: Arc<AtomicBool>,
}

impl StartupCancel {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

fn start_cancelled_error() -> MobileError {
    MobileError::start_failed(START_CANCELLED_MESSAGE)
}

fn is_start_cancelled_error(error: &MobileError) -> bool {
    error.code() == TP_MOBILE_START_FAILED && error.message == START_CANCELLED_MESSAGE
}

async fn wait_for_start_cancel(cancel: &StartupCancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn sleep_or_start_cancel(
    duration: Duration,
    cancel: &StartupCancel,
) -> Result<(), MobileError> {
    tokio::select! {
        _ = tokio::time::sleep(duration) => Ok(()),
        _ = wait_for_start_cancel(cancel) => Err(start_cancelled_error()),
    }
}

#[derive(Debug, Default)]
struct MobileProxyHandleState {
    listen_addr: Option<SocketAddr>,
    platform_url: Option<String>,
    latest_tunnel_config: Option<tp_client::TunnelConfig>,
    connection_status: Option<ConnectionStatus>,
    p2p: Option<MobileP2pRuntimeStatus>,
}

#[derive(Debug, Serialize)]
struct MobileRuntimeStatus {
    running: bool,
    native_version: &'static str,
    listen_addr: Option<String>,
    connection: Option<ConnectionStatus>,
    p2p: Option<MobileP2pRuntimeStatus>,
    clash_overlay_available: bool,
    startup: Option<MobileStartupStatus>,
    last_error: Option<MobileRuntimeError>,
    /// This Peer's own identity in the Tunnel, so the app can name itself
    /// without reading the Peer profile back.
    #[serde(skip_serializing_if = "Option::is_none")]
    this_peer: Option<tp_client::runtime_snapshot::V2ThisPeerSnapshot>,
    /// Everyone else in the Tunnel, with the path and exports the runtime knows
    /// about. The shared crate already maintains this; publishing it is what
    /// lets a phone show the mesh instead of a Peer count.
    peer_directory: tp_client::runtime_snapshot::V2PeerDirectorySnapshot,
    /// What the Platform last reported about the Tunnel's Relay allowance, so
    /// running out is not the first the owner hears of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_usage: Option<tp_client::runtime_snapshot::V2RelayUsageSnapshot>,
    /// The projection every Client renders, computed once in Rust.
    ///
    /// The fields above stay because the packet tunnel derives its route set
    /// from them; this is what the UI reads, and it is byte-for-byte what the
    /// desktop reads.
    client_ui: ClientUiStatusV2,
}

/// A phone always routes through its VPN service.
///
/// There is no privileged helper that can be missing and no permission to
/// repair, so of the desktop's seven Native-routing states only two can arise.
fn mobile_native_routing(running: bool) -> NativeRoutingV2 {
    project_native_routing(running, NativeRoutingApplyResultV2::Applied, false, true)
}

#[derive(Debug, Clone, Serialize)]
struct MobileP2pRuntimeStatus {
    bootstrap_state: String,
    bootstrap_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct MobileRuntimeError {
    code: i32,
    error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct MobileStartupStatus {
    active: bool,
    phase: String,
    detail: Option<String>,
}

#[derive(Clone)]
struct StartupProgress {
    inner: Arc<Mutex<MobileStartupStatus>>,
}

impl StartupProgress {
    fn new(inner: Arc<Mutex<MobileStartupStatus>>) -> Self {
        Self { inner }
    }

    fn set(&self, phase: impl Into<String>) {
        self.set_with_detail(phase, None::<String>);
    }

    fn set_with_detail(&self, phase: impl Into<String>, detail: Option<impl Into<String>>) {
        if let Ok(mut status) = self.inner.lock() {
            status.active = true;
            status.phase = phase.into();
            status.detail = detail.map(Into::into);
        }
    }

    fn clear(&self) {
        if let Ok(mut status) = self.inner.lock() {
            *status = MobileStartupStatus::default();
        }
    }
}

impl From<&MobileError> for MobileRuntimeError {
    fn from(error: &MobileError) -> Self {
        Self {
            code: error.code(),
            error: error.to_string(),
        }
    }
}

#[derive(Clone)]
struct MobileStatusListener {
    shared: Arc<Mutex<MobileProxyHandleState>>,
}

impl StatusListener for MobileStatusListener {
    fn on_status(&self, status: &ConnectionStatus) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.connection_status = Some(status.clone());
        }
    }
}

fn global_state() -> &'static MobileProxyState {
    static STATE: OnceLock<MobileProxyState> = OnceLock::new();
    STATE.get_or_init(MobileProxyState::default)
}

#[no_mangle]
pub extern "C" fn tp_mobile_start_proxy(json_ptr: *const c_char) -> i32 {
    start_proxy_from_c(json_ptr)
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "C" fn tp_mobile_stop_proxy() -> i32 {
    stop_proxy_with_state(global_state())
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "C" fn tp_mobile_status_json() -> *mut c_char {
    result_string_to_c_ptr(status_json_with_state(global_state()))
}

#[no_mangle]
pub extern "C" fn tp_mobile_logs_json(limit: usize) -> *mut c_char {
    result_string_to_c_ptr(mobile_logs_json(limit))
}

#[no_mangle]
pub extern "C" fn tp_mobile_clear_logs() -> i32 {
    clear_mobile_logs()
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "C" fn tp_mobile_set_log_level(level_ptr: *const c_char) -> i32 {
    let level = match string_from_c(level_ptr, "log level") {
        Ok(level) => level,
        Err(e) => return e.code(),
    };
    set_mobile_log_level(&level)
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "C" fn tp_mobile_log_config_json() -> *mut c_char {
    result_string_to_c_ptr(mobile_log_config_json())
}

#[no_mangle]
pub extern "C" fn tp_mobile_clash_overlay_yaml() -> *mut c_char {
    result_string_to_c_ptr(clash_overlay_with_state(global_state()))
}

#[no_mangle]
pub extern "C" fn tp_mobile_runtime_config_json() -> *mut c_char {
    result_string_to_c_ptr(runtime_config_with_state(global_state()))
}

#[no_mangle]
/// # Safety
///
/// `ptr` must be null or a pointer returned by this library from a function
/// whose result is documented to be released with `tp_mobile_free_string`.
pub unsafe extern "C" fn tp_mobile_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(ptr);
    }
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_startProxy(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    request_json: JString<'_>,
) -> jint {
    let raw = match env.get_string(&request_json) {
        Ok(raw) => raw.to_string_lossy().into_owned(),
        Err(e) => {
            return MobileError::invalid_argument(format!("start request is not a string: {e}"))
                .code();
        }
    };
    start_proxy_from_str(&raw)
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_stopProxy(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jint {
    tp_mobile_stop_proxy()
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_statusJson(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    jstring_from_result(env, status_json_with_state(global_state()))
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_logsJson(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    limit: jint,
) -> jstring {
    let limit = usize::try_from(limit).unwrap_or(0);
    jstring_from_result(env, mobile_logs_json(limit))
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_clearNativeLogs(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jint {
    clear_mobile_logs()
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_setLogLevel(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    level: JString<'_>,
) -> jint {
    let level = match env.get_string(&level) {
        Ok(level) => level.to_string_lossy().into_owned(),
        Err(e) => {
            return MobileError::invalid_argument(format!("log level is not a string: {e}")).code();
        }
    };
    set_mobile_log_level(&level)
        .map(|_| TP_MOBILE_OK)
        .unwrap_or_else(|e| e.code())
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_logConfigJson(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    jstring_from_result(env, mobile_log_config_json())
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_clashOverlayYaml(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    jstring_from_result(env, clash_overlay_with_state(global_state()))
}

#[no_mangle]
pub extern "system" fn Java_com_buhuipao_tunnelproxy_TunnelProxyNative_runtimeConfigJson(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    jstring_from_result(env, runtime_config_with_state(global_state()))
}

fn jstring_from_result(env: JNIEnv<'_>, result: Result<String, MobileError>) -> jstring {
    let raw = result.unwrap_or_else(|e| error_json(&e));
    match env.new_string(raw) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn result_string_to_c_ptr(result: Result<String, MobileError>) -> *mut c_char {
    let raw = result.unwrap_or_else(|e| error_json(&e));
    CString::new(raw)
        .expect("mobile FFI strings must not contain interior NUL")
        .into_raw()
}

fn error_json(error: &MobileError) -> String {
    serde_json::json!({
        "ok": false,
        "code": error.code(),
        "error": error.to_string(),
    })
    .to_string()
}

fn start_proxy_from_c(json_ptr: *const c_char) -> Result<(), MobileError> {
    if json_ptr.is_null() {
        return Err(MobileError::invalid_argument(
            "start request pointer is null",
        ));
    }
    let raw = unsafe { CStr::from_ptr(json_ptr) }
        .to_str()
        .map_err(|e| MobileError::invalid_argument(format!("start request is not UTF-8: {e}")))?;
    start_proxy_from_str(raw)
}

fn string_from_c(ptr: *const c_char, label: &str) -> Result<String, MobileError> {
    if ptr.is_null() {
        return Err(MobileError::invalid_argument(format!(
            "{label} pointer is null"
        )));
    }
    let raw = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|e| MobileError::invalid_argument(format!("{label} is not UTF-8: {e}")))?;
    Ok(raw.to_string())
}

fn start_proxy_from_str(raw: &str) -> Result<(), MobileError> {
    let req = parse_start_request(raw)?;
    init_mobile_logging(&req.log_level)?;
    tracing::info!(log_level = %sanitize_log_level(&req.log_level), "mobile native logging ready");
    start_proxy_with_runner(global_state(), req, spawn_mobile_proxy)
}

fn parse_start_request(raw: &str) -> Result<StartProxyRequest, MobileError> {
    serde_json::from_str(raw)
        .map_err(|e| MobileError::invalid_json(format!("invalid start request JSON: {e}")))
}

impl StartProxyRequest {
    fn validated(&self) -> Result<ValidatedStartConfig, MobileError> {
        require_non_empty("device_id", &self.device_id)?;
        require_non_empty("local_socks5_listen", &self.local_socks5_listen)?;
        self.peer_profile.verify().map_err(|error| {
            MobileError::invalid_config(format!("invalid Peer profile: {error}"))
        })?;
        if self.insecure_tls
            && matches!(
                &self.peer_profile.bootstrap,
                PeerBootstrapV2::ManagedPlatform { .. }
            )
        {
            return Err(MobileError::invalid_config(
                "insecure_tls is not allowed for an imported Managed Peer profile",
            ));
        }
        let platform_url = match &self.peer_profile.bootstrap {
            PeerBootstrapV2::ManagedPlatform { platform_url } => platform_url.clone(),
            PeerBootstrapV2::StaticGateway(_) => DEFAULT_PLATFORM_URL.into(),
        };

        let listen_addr = self
            .local_socks5_listen
            .trim()
            .parse::<SocketAddr>()
            .map_err(|e| {
                MobileError::invalid_config(format!(
                    "local_socks5_listen must be a socket address: {e}"
                ))
            })?;
        let p2p = ClientP2pConfig {
            allow_lan_candidates: self.p2p_allow_lan_candidates,
            attempt_after_relay_uptime_secs: 0,
            ..Default::default()
        };

        Ok(ValidatedStartConfig {
            listen_addr,
            p2p,
            device_id: self.device_id.trim().to_string(),
            device_name: self
                .device_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| name.chars().take(120).collect()),
            peer_profile: self.peer_profile.clone(),
            platform_url,
            // Validated here so an unusable rule fails the start with a message
            // instead of leaving the runtime on its refuse-everything placeholder.
            client_access: compiled_client_access(self)?,
            local_runtime_record: compiled_local_runtime_record(self)?,
            tunnel_first: self.tunnel_first,
        })
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<(), MobileError> {
    if value.trim().is_empty() {
        return Err(MobileError::invalid_config(format!("{field} is required")));
    }
    Ok(())
}

fn start_proxy_with_runner<F>(
    state: &MobileProxyState,
    req: StartProxyRequest,
    runner: F,
) -> Result<(), MobileError>
where
    F: FnOnce(
        StartProxyRequest,
        StartupProgress,
        StartupCancel,
    ) -> Result<Box<dyn ProxyHandle>, MobileError>,
{
    let progress = StartupProgress::new(state.startup.clone());
    progress.set("validating request");
    if let Err(e) = req.validated() {
        record_last_error(state, &e);
        progress.clear();
        return Err(e);
    }
    {
        let mut starting = state
            .starting
            .lock()
            .map_err(|_| MobileError::start_failed("mobile proxy starting state lock poisoned"))?;
        if *starting {
            let e = MobileError::already_running();
            record_last_error(state, &e);
            progress.clear();
            return Err(e);
        }
        let handle = state
            .handle
            .lock()
            .map_err(|_| MobileError::start_failed("mobile proxy state lock poisoned"))?;
        if handle.is_some() {
            let e = MobileError::already_running();
            record_last_error(state, &e);
            progress.clear();
            return Err(e);
        }
        drop(handle);
        let mut start_cancel = state.start_cancel.lock().map_err(|_| {
            MobileError::start_failed("mobile proxy start cancellation lock poisoned")
        })?;
        let cancel = StartupCancel::default();
        *start_cancel = Some(cancel.clone());
        *starting = true;
        drop(start_cancel);
        drop(starting);

        progress.set("starting runtime");
        let result = runner(req, progress.clone(), cancel.clone());
        if let Ok(mut starting) = state.starting.lock() {
            *starting = false;
        }
        if let Ok(mut start_cancel) = state.start_cancel.lock() {
            start_cancel.take();
        }
        progress.clear();

        match result {
            Ok(next) if cancel.is_cancelled() => {
                next.stop();
                clear_last_error(state);
                Err(start_cancelled_error())
            }
            Ok(next) => {
                let mut handle = state
                    .handle
                    .lock()
                    .map_err(|_| MobileError::start_failed("mobile proxy state lock poisoned"))?;
                *handle = Some(next);
                clear_last_error(state);
                Ok(())
            }
            Err(e) if cancel.is_cancelled() && is_start_cancelled_error(&e) => {
                clear_last_error(state);
                Err(e)
            }
            Err(e) => {
                record_last_error(state, &e);
                Err(e)
            }
        }
    }
}

fn stop_proxy_with_state(state: &MobileProxyState) -> Result<(), MobileError> {
    let start_cancel = state
        .start_cancel
        .lock()
        .map_err(|_| MobileError::start_failed("mobile proxy start cancellation lock poisoned"))?
        .clone();
    if let Some(cancel) = start_cancel {
        cancel.cancel();
    }

    let handle = {
        let mut guard = state
            .handle
            .lock()
            .map_err(|_| MobileError::start_failed("mobile proxy state lock poisoned"))?;
        guard.take()
    };
    if let Some(handle) = handle {
        handle.stop();
    }
    clear_last_error(state);
    Ok(())
}

fn status_json_with_state(state: &MobileProxyState) -> Result<String, MobileError> {
    let guard = state
        .handle
        .lock()
        .map_err(|_| MobileError::start_failed("mobile proxy state lock poisoned"))?;
    let status = if let Some(handle) = guard.as_ref() {
        return handle.status_json();
    } else {
        MobileRuntimeStatus {
            running: false,
            native_version: env!("CARGO_PKG_VERSION"),
            listen_addr: None,
            connection: None,
            p2p: None,
            clash_overlay_available: false,
            startup: startup_status(state)?,
            last_error: last_error(state)?,
            // Nothing is running, so there is no mesh to report — said in the
            // same words the desktop uses for an idle Client.
            this_peer: None,
            peer_directory: Default::default(),
            relay_usage: None,
            client_ui: project_client_ui_status(
                &ConnectionStatus::default(),
                project_engine_runtime_snapshot(
                    V2RuntimeSnapshot::default(),
                    mobile_native_routing(false),
                ),
            ),
        }
    };
    serde_json::to_string(&status)
        .map_err(|e| MobileError::start_failed(format!("status serialize failed: {e}")))
}

fn record_last_error(state: &MobileProxyState, error: &MobileError) {
    if let Ok(mut guard) = state.last_error.lock() {
        *guard = Some(MobileRuntimeError::from(error));
    }
}

fn clear_last_error(state: &MobileProxyState) {
    if let Ok(mut guard) = state.last_error.lock() {
        *guard = None;
    }
}

fn last_error(state: &MobileProxyState) -> Result<Option<MobileRuntimeError>, MobileError> {
    state
        .last_error
        .lock()
        .map(|guard| guard.clone())
        .map_err(|_| MobileError::start_failed("mobile proxy error state lock poisoned"))
}

fn startup_status(state: &MobileProxyState) -> Result<Option<MobileStartupStatus>, MobileError> {
    state
        .startup
        .lock()
        .map(|guard| guard.active.then(|| guard.clone()))
        .map_err(|_| MobileError::start_failed("mobile proxy startup state lock poisoned"))
}

fn update_mobile_p2p_status(
    shared: &Arc<Mutex<MobileProxyHandleState>>,
    f: impl FnOnce(&mut MobileP2pRuntimeStatus),
) {
    if let Ok(mut shared_state) = shared.lock() {
        if let Some(status) = shared_state.p2p.as_mut() {
            f(status);
        }
    }
}

fn clash_overlay_with_state(state: &MobileProxyState) -> Result<String, MobileError> {
    let guard = state
        .handle
        .lock()
        .map_err(|_| MobileError::start_failed("mobile proxy state lock poisoned"))?;
    let Some(handle) = guard.as_ref() else {
        return Err(MobileError::start_failed("mobile proxy is not running"));
    };
    handle.clash_overlay_yaml()
}

fn runtime_config_with_state(state: &MobileProxyState) -> Result<String, MobileError> {
    let guard = state
        .handle
        .lock()
        .map_err(|_| MobileError::start_failed("mobile proxy state lock poisoned"))?;
    let Some(handle) = guard.as_ref() else {
        return Err(MobileError::start_failed("mobile proxy is not running"));
    };
    handle.runtime_config_json()
}

struct ThreadProxyHandle {
    stop_tx: Option<oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Mutex<MobileProxyHandleState>>,
    status_provider: MobileStatusProvider,
    mesh_provider: MobileMeshProvider,
}

impl ProxyHandle for ThreadProxyHandle {
    fn status_json(&self) -> Result<String, MobileError> {
        let connection_status = (self.status_provider)();
        let shared = self
            .shared
            .lock()
            .map_err(|_| MobileError::start_failed("mobile proxy handle lock poisoned"))?;
        let runtime = (self.mesh_provider)();
        let this_peer = runtime.this_peer.clone();
        let peer_directory = runtime.peer_directory.clone();
        let relay_usage = runtime.relay_usage;
        let client_ui = project_client_ui_status(
            &connection_status,
            project_engine_runtime_snapshot(runtime, mobile_native_routing(true)),
        );
        let status = MobileRuntimeStatus {
            running: true,
            native_version: env!("CARGO_PKG_VERSION"),
            listen_addr: shared.listen_addr.map(|addr| addr.to_string()),
            connection: Some(connection_status),
            p2p: shared.p2p.clone(),
            clash_overlay_available: shared.latest_tunnel_config.is_some(),
            startup: None,
            last_error: None,
            this_peer,
            peer_directory,
            relay_usage,
            client_ui,
        };
        serde_json::to_string(&status)
            .map_err(|e| MobileError::start_failed(format!("status serialize failed: {e}")))
    }

    fn clash_overlay_yaml(&self) -> Result<String, MobileError> {
        let shared = self
            .shared
            .lock()
            .map_err(|_| MobileError::start_failed("mobile proxy handle lock poisoned"))?;
        let cfg = shared
            .latest_tunnel_config
            .as_ref()
            .ok_or_else(|| MobileError::start_failed("V2 runtime config unavailable"))?;
        let platform_url = shared
            .platform_url
            .as_deref()
            .unwrap_or(DEFAULT_PLATFORM_URL);
        let listen_addr = shared
            .listen_addr
            .ok_or_else(|| MobileError::start_failed("local SOCKS5 listen address unavailable"))?;
        clash_overlay_yaml(cfg, listen_addr, platform_url)
    }

    fn runtime_config_json(&self) -> Result<String, MobileError> {
        let shared = self
            .shared
            .lock()
            .map_err(|_| MobileError::start_failed("mobile proxy handle lock poisoned"))?;
        let cfg = shared
            .latest_tunnel_config
            .as_ref()
            .ok_or_else(|| MobileError::start_failed("V2 runtime config unavailable"))?;
        let listen_addr = shared
            .listen_addr
            .ok_or_else(|| MobileError::start_failed("local SOCKS5 listen address unavailable"))?;
        runtime_config_json(cfg, listen_addr)
    }

    fn stop(mut self: Box<Self>) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

type MobileStatusProvider = Arc<dyn Fn() -> ConnectionStatus + Send + Sync + 'static>;
type MobileMeshProvider = Arc<dyn Fn() -> V2RuntimeSnapshot + Send + Sync + 'static>;

struct MobileProxyReady {
    status_provider: MobileStatusProvider,
    mesh_provider: MobileMeshProvider,
}

fn spawn_mobile_proxy(
    req: StartProxyRequest,
    progress: StartupProgress,
    cancel: StartupCancel,
) -> Result<Box<dyn ProxyHandle>, MobileError> {
    progress.set("spawning mobile runtime thread");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<MobileProxyReady, MobileError>>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let shared = Arc::new(Mutex::new(MobileProxyHandleState::default()));
    let shared_for_thread = shared.clone();
    let progress_for_thread = progress.clone();
    let cancel_for_thread = cancel.clone();
    let join = std::thread::Builder::new()
        .name("tp-mobile-proxy".into())
        .spawn(move || {
            run_mobile_proxy_thread(
                req,
                ready_tx,
                stop_rx,
                shared_for_thread,
                progress_for_thread,
                cancel_for_thread,
            )
        })
        .map_err(|e| MobileError::start_failed(format!("failed to spawn runtime thread: {e}")))?;

    progress.set("waiting for mobile runtime ready");
    let deadline = Instant::now() + MOBILE_PROXY_READY_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            let _ = stop_tx.send(());
            let _ = join.join();
            return Err(start_cancelled_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = stop_tx.send(());
            let _ = join.join();
            return Err(MobileError::start_failed(
                "mobile proxy runtime did not report ready: timed out waiting on channel",
            ));
        }
        let wait = remaining.min(Duration::from_millis(100));
        match ready_rx.recv_timeout(wait) {
            Ok(Ok(ready)) => {
                return Ok(Box::new(ThreadProxyHandle {
                    stop_tx: Some(stop_tx),
                    join: Some(join),
                    shared,
                    status_provider: ready.status_provider,
                    mesh_provider: ready.mesh_provider,
                }));
            }
            Ok(Err(e)) => {
                let _ = stop_tx.send(());
                let _ = join.join();
                return Err(e);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = stop_tx.send(());
                let _ = join.join();
                return Err(MobileError::start_failed(
                    "mobile proxy runtime did not report ready: channel disconnected",
                ));
            }
        }
    }
}

fn run_mobile_proxy_thread(
    req: StartProxyRequest,
    ready_tx: std::sync::mpsc::Sender<Result<MobileProxyReady, MobileError>>,
    stop_rx: oneshot::Receiver<()>,
    shared: Arc<Mutex<MobileProxyHandleState>>,
    progress: StartupProgress,
    cancel: StartupCancel,
) {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    progress.set("building Tokio runtime");
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("tp-mobile-runtime")
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = ready_tx.send(Err(MobileError::start_failed(format!(
                "failed to build Tokio runtime: {e}"
            ))));
            return;
        }
    };

    runtime.block_on(async move {
        let cancel_for_stop = cancel.clone();
        let stop_watch = tokio::spawn(async move {
            let _ = stop_rx.await;
            cancel_for_stop.cancel();
        });
        match start_mobile_proxy(req, shared, progress.clone(), cancel.clone()).await {
            Ok(ctx) => {
                progress.set("mobile runtime ready");
                let engine = ctx.engine.clone();
                let mesh_engine = ctx.engine.clone();
                let ready = MobileProxyReady {
                    status_provider: Arc::new(move || engine.status()),
                    mesh_provider: Arc::new(move || mesh_engine.v2_runtime_snapshot()),
                };
                let _ = ready_tx.send(Ok(ready));
                wait_for_start_cancel(&cancel).await;
                ctx.shutdown().await;
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
        stop_watch.abort();
    });
}

struct MobileProxyContext {
    engine: Arc<Engine>,
    local_proxy_task: JoinHandle<()>,
}

impl MobileProxyContext {
    async fn shutdown(self) {
        self.local_proxy_task.abort();
        match self.local_proxy_task.await {
            Err(e) if e.is_cancelled() => {}
            Err(e) => eprintln!("local SOCKS5 proxy task join failed: {e}"),
            Ok(()) => {}
        }
        self.engine.disconnect().await;
    }
}

async fn start_mobile_proxy(
    req: StartProxyRequest,
    shared: Arc<Mutex<MobileProxyHandleState>>,
    progress: StartupProgress,
    cancel: StartupCancel,
) -> Result<MobileProxyContext, MobileError> {
    progress.set("validating mobile config");
    if cancel.is_cancelled() {
        return Err(start_cancelled_error());
    }
    let cfg = req.validated()?;
    if let Ok(mut shared_state) = shared.lock() {
        shared_state.listen_addr = Some(cfg.listen_addr);
        shared_state.platform_url = Some(cfg.platform_url.clone());
        shared_state.p2p = Some(MobileP2pRuntimeStatus {
            bootstrap_state: "waiting_for_gateway_attachment".into(),
            bootstrap_error: None,
        });
    }
    progress.set_with_detail("creating engine", Some(cfg.platform_url.clone()));
    let engine = Engine::new(
        EngineConfig {
            platform_url: cfg.platform_url.clone(),
            gateway_ca_path: None,
            insecure_tls: req.insecure_tls,
            client_version: env!("CARGO_PKG_VERSION").into(),
            device_id: Some(cfg.device_id.clone()),
            device_name: cfg.device_name.clone(),
        },
        Arc::new(MobileStatusListener {
            shared: shared.clone(),
        }),
    );

    engine.set_p2p_config(Arc::new(cfg.p2p.clone()));
    engine
        .set_v2_access_policy(&cfg.client_access)
        .map_err(|error| {
            MobileError::invalid_config(format!("invalid Client access rule: {error}"))
        })?;
    // What this device publishes. Without this the runtime carried no local
    // record, so a phone could reach the Tunnel but never share with it.
    engine
        .set_v2_local_runtime_record(cfg.local_runtime_record.clone())
        .map_err(|error| MobileError::invalid_config(format!("invalid exported LANs: {error}")))?;
    progress.set("connecting engine");
    let connect_result = tokio::select! {
        result = engine.connect_with_peer_profile(cfg.peer_profile.clone(), None) => result,
        _ = wait_for_start_cancel(&cancel) => {
            engine.disconnect().await;
            return Err(start_cancelled_error());
        }
    };
    connect_result.map_err(|e| MobileError::start_failed(format!("engine start failed: {e}")))?;

    progress.set("waiting for V2 runtime config");
    let runtime_cfg = match wait_for_v2_runtime_config(&engine, &progress, &cancel).await {
        Ok(cfg) => cfg,
        Err(e) => {
            engine.disconnect().await;
            return Err(e);
        }
    };
    if let Ok(mut shared_state) = shared.lock() {
        shared_state.latest_tunnel_config = Some(runtime_cfg.clone());
    }
    progress.set_with_detail(
        "V2 runtime config resolved",
        Some(format!(
            "gateway={}:{}",
            runtime_cfg.gateway_addr, runtime_cfg.gateway_port
        )),
    );

    progress.set("starting P2P bootstrap");
    update_mobile_p2p_status(&shared, |status| {
        status.bootstrap_state = "spawned".into();
        status.bootstrap_error = None;
    });
    let engine_for_p2p = engine.clone();
    let shared_for_p2p = shared.clone();
    let task_cancel = engine.task_cancel_token();
    let p2p_cfg = cfg.p2p.clone();
    engine.tasks().spawn(async move {
        update_mobile_p2p_status(&shared_for_p2p, |status| {
            status.bootstrap_state = "running".into();
        });
        if let Err(e) = tp_client::p2p::bootstrap::run(engine_for_p2p, p2p_cfg, task_cancel).await {
            let error = e.to_string();
            update_mobile_p2p_status(&shared_for_p2p, |status| {
                status.bootstrap_state = "failed".into();
                status.bootstrap_error = Some(error);
            });
            eprintln!("P2P bootstrap failed; continuing relay-only: {e}");
        } else {
            update_mobile_p2p_status(&shared_for_p2p, |status| {
                status.bootstrap_state = "stopped".into();
            });
        }
    });

    progress.set("waiting for relay session");
    if let Err(e) = wait_for_local_proxy_engine_ready(&engine, &progress, &cancel).await {
        engine.disconnect().await;
        return Err(MobileError::start_failed(format!(
            "local SOCKS5 engine not ready: {e:#}"
        )));
    }
    let tunnel_config = engine.latest_tunnel_config().ok_or_else(|| {
        MobileError::start_failed("local SOCKS5 proxy did not start: V2 runtime config unavailable")
    })?;
    let auth = local_proxy_auth_mode_from_tunnel_config(&tunnel_config, cfg.listen_addr)?;

    let listen_addr = cfg.listen_addr;
    let engine_for_proxy = engine.clone();
    let (local_ready_tx, local_ready_rx) = oneshot::channel();
    progress.set_with_detail("binding local SOCKS5", Some(listen_addr.to_string()));
    let local_proxy_task = tokio::spawn(async move {
        let backend = tp_client::proxy_mode::LocalEngineSocks5Backend::new(engine_for_proxy);
        if let Err(e) = tp_proxy_socks5::serve_with_backend_auth_mode_ready(
            listen_addr,
            Arc::new(backend),
            auth,
            Some(local_ready_tx),
        )
        .await
        {
            eprintln!("local SOCKS5 proxy stopped: {e:#}");
        }
    });

    progress.set("waiting for local SOCKS5 ready");
    match tokio::select! {
        result = tokio::time::timeout(MOBILE_STAGE_TIMEOUT, local_ready_rx) => result,
        _ = wait_for_start_cancel(&cancel) => {
            local_proxy_task.abort();
            engine.disconnect().await;
            return Err(start_cancelled_error());
        }
    } {
        Ok(Ok(bound_addr)) => {
            if let Ok(mut shared_state) = shared.lock() {
                shared_state.listen_addr = Some(bound_addr);
            }
            progress.set_with_detail("local SOCKS5 ready", Some(bound_addr.to_string()));
        }
        Ok(Err(_)) => {
            local_proxy_task.abort();
            return Err(MobileError::start_failed(
                "local SOCKS5 proxy did not report ready",
            ));
        }
        Err(_) => {
            local_proxy_task.abort();
            return Err(MobileError::start_failed(
                "local SOCKS5 proxy ready timeout",
            ));
        }
    }

    let ready_status = match wait_for_mobile_vpn_start_ready(
        || engine.status(),
        &progress,
        MOBILE_RELAY_READY_TIMEOUT,
        &cancel,
    )
    .await
    {
        Ok(status) => status,
        Err(e) => {
            local_proxy_task.abort();
            engine.disconnect().await;
            return Err(e);
        }
    };
    progress.set("relay ready");
    if let Ok(mut shared_state) = shared.lock() {
        shared_state.connection_status = Some(ready_status);
    }

    Ok(MobileProxyContext {
        engine,
        local_proxy_task,
    })
}

fn local_proxy_auth_mode_from_tunnel_config(
    cfg: &tp_client::TunnelConfig,
    listen_addr: SocketAddr,
) -> Result<tp_proxy_socks5::AuthMode, MobileError> {
    if !listen_addr.ip().is_loopback() {
        return Err(MobileError::invalid_config(
            "V2 Local SOCKS5 requires a loopback listener",
        ));
    }
    Ok(tp_proxy_socks5::AuthMode::NoAuth {
        group_id: v2_local_proxy_peer_id(cfg)?,
    })
}

fn v2_local_proxy_peer_id(cfg: &tp_client::TunnelConfig) -> Result<String, MobileError> {
    let peer_id = cfg.peer_id.trim();
    if cfg.tunnel_id.trim().is_empty() || peer_id.is_empty() || cfg.overlay_ipv4.trim().is_empty() {
        return Err(MobileError::start_failed(
            "V2 TunnelConfig is missing a verified Peer identity",
        ));
    }
    Ok(peer_id.to_string())
}

fn clash_overlay_yaml(
    cfg: &tp_client::TunnelConfig,
    listen_addr: SocketAddr,
    platform_url: &str,
) -> Result<String, MobileError> {
    let _ = local_proxy_auth_mode_from_tunnel_config(cfg, listen_addr)?;
    let mut direct_rules = Vec::new();
    if let Some(host) = host_from_url(platform_url) {
        direct_rules.push(format!("  - DOMAIN,{host},DIRECT"));
    }
    if let Some(rule) = gateway_direct_rule(&cfg.gateway_addr) {
        direct_rules.push(rule);
    }
    let direct_rules = direct_rules.join("\n");
    Ok(format!(
        "# HomeLAN is for private LAN destinations only. Do not put it in a\n# public URL-test/fallback group. Exclude Lantunnel from the Clash VPN\n# when your Clash client cannot bypass apps from rules.\nproxies:\n  - name: HomeLAN\n    type: socks5\n    server: {}\n    port: {}\n    udp: true\n\nrules:\n{direct_rules}\n  - IP-CIDR,192.168.0.0/16,HomeLAN,no-resolve\n  - IP-CIDR,10.0.0.0/8,HomeLAN,no-resolve\n  - IP-CIDR,172.16.0.0/12,HomeLAN,no-resolve\n  - IP-CIDR,169.254.0.0/16,HomeLAN,no-resolve\n",
        listen_addr.ip(),
        listen_addr.port(),
    ))
}

fn runtime_config_json(
    cfg: &tp_client::TunnelConfig,
    listen_addr: SocketAddr,
) -> Result<String, MobileError> {
    let _ = local_proxy_auth_mode_from_tunnel_config(cfg, listen_addr)?;
    let mut socks5 = serde_json::Map::new();
    socks5.insert(
        "host".into(),
        serde_json::Value::String(listen_addr.ip().to_string()),
    );
    socks5.insert("port".into(), serde_json::Value::from(listen_addr.port()));
    socks5.insert("auth_enabled".into(), serde_json::Value::Bool(false));
    let mut root = serde_json::Map::new();
    root.insert("local_socks5".into(), serde_json::Value::Object(socks5));
    serde_json::to_string(&serde_json::Value::Object(root))
        .map_err(|e| MobileError::start_failed(format!("runtime config serialize failed: {e}")))
}

fn gateway_direct_rule(gateway_addr: &str) -> Option<String> {
    let gateway = gateway_addr.trim();
    if gateway.is_empty() {
        return None;
    }
    match gateway.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => Some(format!("  - IP-CIDR,{ip}/32,DIRECT,no-resolve")),
        Ok(std::net::IpAddr::V6(ip)) => Some(format!("  - IP-CIDR6,{ip}/128,DIRECT,no-resolve")),
        Err(_) => Some(format!("  - DOMAIN,{gateway},DIRECT")),
    }
}

fn host_from_url(raw: &str) -> Option<String> {
    let mut rest = raw.trim();
    if let Some((_, suffix)) = rest.split_once("://") {
        rest = suffix;
    }
    let authority = rest.split('/').next()?.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(stripped) = authority.strip_prefix('[') {
        return stripped
            .split_once(']')
            .map(|(host, _)| host.to_string())
            .filter(|host| !host.is_empty());
    }
    Some(
        authority
            .split(':')
            .next()
            .unwrap_or(authority)
            .trim()
            .to_string(),
    )
    .filter(|host| !host.is_empty())
}

async fn wait_for_local_proxy_engine_ready(
    engine: &Arc<Engine>,
    progress: &StartupProgress,
    cancel: &StartupCancel,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + MOBILE_STAGE_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("mobile proxy start cancelled");
        }
        if engine.has_proxy_sessions() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "engine did not install MultiSession within {}s",
                MOBILE_STAGE_TIMEOUT.as_secs()
            );
        }
        let status = engine.status();
        progress.set_with_detail(
            "waiting for relay session",
            Some(format!(
                "connected={} connecting={} message={}",
                status.connected, status.connecting, status.message
            )),
        );
        sleep_or_start_cancel(Duration::from_millis(200), cancel)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
}

async fn wait_for_v2_runtime_config(
    engine: &Arc<Engine>,
    progress: &StartupProgress,
    cancel: &StartupCancel,
) -> Result<tp_client::TunnelConfig, MobileError> {
    let deadline = Instant::now() + MOBILE_STAGE_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            return Err(start_cancelled_error());
        }
        if let Some(cfg) = engine.latest_tunnel_config() {
            return Ok(cfg);
        }
        if Instant::now() >= deadline {
            return Err(v2_runtime_config_unavailable_error());
        }
        let status = engine.status();
        progress.set_with_detail(
            "waiting for V2 runtime config",
            Some(format!(
                "connected={} connecting={} message={} error={}",
                status.connected,
                status.connecting,
                status.message,
                status.error.unwrap_or_default()
            )),
        );
        sleep_or_start_cancel(Duration::from_millis(200), cancel).await?;
    }
}

#[cfg(test)]
fn mobile_connected_health_ready(status: &ConnectionStatus) -> bool {
    status.connected && status.transport_heartbeat.active && status.platform_heartbeat.active
}

fn mobile_vpn_start_ready(status: &ConnectionStatus) -> bool {
    status.connected
}

async fn wait_for_mobile_vpn_start_ready(
    mut status_fn: impl FnMut() -> ConnectionStatus,
    progress: &StartupProgress,
    timeout_duration: Duration,
    cancel: &StartupCancel,
) -> Result<ConnectionStatus, MobileError> {
    let deadline = Instant::now() + timeout_duration;
    loop {
        if cancel.is_cancelled() {
            return Err(start_cancelled_error());
        }
        let status = status_fn();
        if mobile_vpn_start_ready(&status) {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(MobileError::start_failed(format!(
                "relay connection unavailable after {}ms",
                timeout_duration.as_millis()
            )));
        }
        progress.set_with_detail(
            "waiting for relay ready",
            Some(format!(
                "connected={} connecting={} mode={:?} message={}",
                status.connected, status.connecting, status.path_mode, status.message
            )),
        );
        sleep_or_start_cancel(Duration::from_millis(100), cancel).await?;
    }
}

fn v2_runtime_config_unavailable_error() -> MobileError {
    MobileError::start_failed("V2 runtime config unavailable after connect")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    fn static_peer_profile() -> PeerProfileV2 {
        let gateway = tp_core::provisioning::GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner =
            tp_core::provisioning::TunnelOwnerFileV2::generate(gateway).expect("Tunnel owner");
        owner.add_peer(None, 1, None).expect("Peer profile")
    }

    fn managed_peer_profile() -> PeerProfileV2 {
        let mut profile = static_peer_profile();
        profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
            platform_url: "https://platform.example".into(),
        };
        profile
    }

    fn start_request_json(device_id: Option<&str>, log_level: Option<&str>) -> String {
        let mut value = serde_json::json!({
            "peer_profile": managed_peer_profile(),
        });
        if let Some(device_id) = device_id {
            value["device_id"] = device_id.into();
        }
        if let Some(log_level) = log_level {
            value["log_level"] = log_level.into();
        }
        serde_json::to_string(&value).expect("start request JSON")
    }

    #[test]
    fn start_request_json_derives_runtime_config() {
        let raw = serde_json::to_string(&serde_json::json!({
            "peer_profile": managed_peer_profile(),
            "device_id": "d7c5d4bb-7db3-49e6-9165-c87f9c6722cf",
            "device_name": "Pixel",
            "local_socks5_listen": "127.0.0.1:18080",
            "p2p_allow_lan_candidates": true
        }))
        .expect("start request JSON");
        let req = parse_start_request(&raw).expect("valid JSON should parse");

        let cfg = req.validated().expect("request should validate");

        assert_eq!(cfg.listen_addr.to_string(), "127.0.0.1:18080");
        assert_eq!(cfg.device_id, "d7c5d4bb-7db3-49e6-9165-c87f9c6722cf");
        assert_eq!(cfg.device_name.as_deref(), Some("Pixel"));
        assert_eq!(cfg.p2p.attempt_after_relay_uptime_secs, 0);
        assert_eq!(cfg.p2p.cooldown_initial_secs, 60);
        assert_eq!(cfg.p2p.cooldown_max_secs, 600);
        assert_eq!(cfg.p2p.scheduler_p2p_min_advantage, 1.2);
        assert_eq!(cfg.p2p.scheduler_stable_cycles, 3);
        assert!(cfg.p2p.allow_lan_candidates);
        assert_eq!(cfg.platform_url, "https://platform.example");
    }

    #[test]
    fn mobile_contract_start_request_defaults_without_peer_client_id() {
        let raw = start_request_json(Some("cad807b6-e2c6-4a36-81ce-fce544246512"), None);
        let req = parse_start_request(&raw).expect("minimal mobile JSON should parse");

        let cfg = req
            .validated()
            .expect("mobile start should not require peer_client_id");

        assert_eq!(cfg.listen_addr.to_string(), "127.0.0.1:1080");
        assert_eq!(cfg.device_id, "cad807b6-e2c6-4a36-81ce-fce544246512");
        assert_eq!(cfg.p2p.attempt_after_relay_uptime_secs, 0);
    }

    #[test]
    fn mobile_contract_rejects_removed_mesh_role_fields() {
        for field in ["p2p_enabled", "peer_client_id"] {
            let mut value = serde_json::json!({
                "peer_profile": managed_peer_profile(),
                "device_id": "cad807b6-e2c6-4a36-81ce-fce544246512",
            });
            value[field] = serde_json::json!(false);

            let error = parse_start_request(&value.to_string())
                .err()
                .expect("removed role field must not parse");

            assert_eq!(error.code(), TP_MOBILE_INVALID_JSON, "field={field}");
        }
    }

    #[test]
    fn mobile_runtime_mesh_status_has_no_role_split_fields() {
        let value = serde_json::to_value(MobileP2pRuntimeStatus {
            bootstrap_state: "running".into(),
            bootstrap_error: None,
        })
        .expect("mesh status JSON");

        assert!(value.get("enabled").is_none());
        assert!(value.get("requested_peer_client_id").is_none());
        assert!(value.get("resolved_peer_client_id").is_none());
        assert_eq!(value["bootstrap_state"], "running");
    }

    #[test]
    fn mobile_contract_requires_device_id() {
        let raw = start_request_json(None, None);
        let req = parse_start_request(&raw).expect("minimal mobile JSON should parse");
        let err = req.validated().err().expect("device_id is required");

        assert_eq!(err.code(), TP_MOBILE_INVALID_CONFIG);
        assert!(err.to_string().contains("device_id is required"));
    }

    #[test]
    fn managed_start_rejects_insecure_tls_before_the_runtime_runner() {
        let state = MobileProxyState::default();
        let mut req = valid_request();
        req.insecure_tls = true;
        let runner_called = Arc::new(AtomicBool::new(false));
        let runner_called_by_start = runner_called.clone();

        let err = start_proxy_with_runner(&state, req, move |_, _, _| {
            runner_called_by_start.store(true, Ordering::SeqCst);
            Ok(Box::new(TestProxyHandle {
                stopped: Arc::new(AtomicBool::new(false)),
            }))
        })
        .expect_err("Managed profiles must reject insecure TLS before EngineConfig");

        assert_eq!(err.code(), TP_MOBILE_INVALID_CONFIG);
        assert!(err.to_string().contains("Managed Peer profile"));
        assert!(!runner_called.load(Ordering::SeqCst));
    }

    #[test]
    fn static_profile_keeps_explicit_insecure_tls_behavior() {
        let mut req = valid_request();
        req.peer_profile = static_peer_profile();
        req.insecure_tls = true;

        req.validated()
            .expect("Static profile may retain explicit insecure TLS behavior");
    }

    #[test]
    fn mobile_contract_rejects_the_removed_tunnel_key_start_request() {
        let error = parse_start_request(r#"{"tunnel_id":"tid-1","tunnel_key":"secret"}"#)
            .err()
            .expect("V1 tunnel-key request must not parse");

        assert_eq!(error.code(), TP_MOBILE_INVALID_JSON);
    }

    #[test]
    fn mobile_contract_clash_overlay_uses_loopback_no_auth() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            gateway_addr: "203.0.113.88".into(),
            ..Default::default()
        };
        let yaml = clash_overlay_yaml(
            &cfg,
            "127.0.0.1:19080".parse().unwrap(),
            "https://lantunnel.app",
        )
        .expect("V2 loopback proxy should generate overlay without credentials");

        assert!(yaml.contains("name: HomeLAN"));
        assert!(yaml.contains("type: socks5"));
        assert!(yaml.contains("server: 127.0.0.1"));
        assert!(yaml.contains("port: 19080"));
        assert!(!yaml.contains("username:"));
        assert!(!yaml.contains("password:"));
        assert!(yaml.contains("udp: true"));
        assert!(!yaml.contains("PROCESS-NAME"));
        assert!(yaml.contains("DOMAIN,lantunnel.app,DIRECT"));
        assert!(yaml.contains("IP-CIDR,203.0.113.88/32,DIRECT,no-resolve"));
        assert!(yaml.contains("IP-CIDR,192.168.0.0/16,HomeLAN,no-resolve"));
        assert!(yaml.contains("IP-CIDR,10.0.0.0/8,HomeLAN,no-resolve"));
        assert!(yaml.contains("IP-CIDR,172.16.0.0/12,HomeLAN,no-resolve"));
        assert!(yaml.contains("IP-CIDR,169.254.0.0/16,HomeLAN,no-resolve"));
        assert!(yaml.contains("Exclude Lantunnel from the Clash VPN"));
    }

    #[test]
    fn mobile_contract_clash_overlay_directs_gateway_domain() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            gateway_addr: "gateway.example.com".into(),
            ..Default::default()
        };
        let yaml = clash_overlay_yaml(
            &cfg,
            "127.0.0.1:19080".parse().unwrap(),
            "https://platform.example:8443/api",
        )
        .expect("domain gateway should generate overlay");

        assert!(yaml.contains("DOMAIN,platform.example,DIRECT"));
        assert!(yaml.contains("DOMAIN,gateway.example.com,DIRECT"));
    }

    #[test]
    fn mobile_contract_clash_overlay_omits_empty_gateway_direct_rule() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            gateway_addr: "".into(),
            ..Default::default()
        };
        let yaml = clash_overlay_yaml(
            &cfg,
            "127.0.0.1:19080".parse().unwrap(),
            "https://platform.example",
        )
        .expect("empty gateway should still generate overlay");

        assert!(yaml.contains("DOMAIN,platform.example,DIRECT"));
        assert!(!yaml.contains("DOMAIN,,DIRECT"));
    }

    #[test]
    fn mobile_contract_runtime_config_uses_loopback_no_auth_without_credentials() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            ..Default::default()
        };
        let raw = runtime_config_json(&cfg, "127.0.0.1:1080".parse().unwrap())
            .expect("V2 loopback config should not require credentials");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("runtime config is JSON");

        assert_eq!(value["local_socks5"]["host"], "127.0.0.1");
        assert_eq!(value["local_socks5"]["port"], 1080);
        assert_eq!(value["local_socks5"]["auth_enabled"], false);
        assert!(value["local_socks5"].get("username").is_none());
        assert!(value["local_socks5"].get("password").is_none());
    }

    #[test]
    fn mobile_ffi_exports_status_json_when_stopped() {
        let state = MobileProxyState::default();
        let raw = status_json_with_state(&state).expect("status should serialize");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");

        assert_eq!(value["running"], false);
        assert_eq!(value["listen_addr"], serde_json::Value::Null);
        assert!(!raw.contains("secret"));
    }

    #[test]
    fn mobile_ffi_exports_c_status_string_can_be_freed() {
        let ptr = tp_mobile_status_json();
        assert!(!ptr.is_null());
        let raw = unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .expect("status should be UTF-8")
            .to_string();
        unsafe { tp_mobile_free_string(ptr) };

        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");
        assert_eq!(value["running"], false);
    }

    #[test]
    fn mobile_ffi_runtime_config_returns_error_when_stopped() {
        let state = MobileProxyState::default();
        let err = runtime_config_with_state(&state).expect_err("stopped proxy has no config");

        assert_eq!(err.code(), TP_MOBILE_START_FAILED);
        assert!(err.to_string().contains("mobile proxy is not running"));
    }

    #[test]
    fn v2_loopback_local_proxy_uses_no_auth_with_peer_identity() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            ..Default::default()
        };
        let mode =
            local_proxy_auth_mode_from_tunnel_config(&cfg, "127.0.0.1:1080".parse().unwrap())
                .expect("V2 loopback proxy must not need a shared secret");

        match mode {
            tp_proxy_socks5::AuthMode::NoAuth { group_id } => assert_eq!(group_id, "peer-v2"),
            tp_proxy_socks5::AuthMode::UserPass(_) => panic!("V2 has no shared-secret auth"),
        }
    }

    #[test]
    fn v2_local_proxy_rejects_non_loopback_listener() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            ..Default::default()
        };

        let error =
            match local_proxy_auth_mode_from_tunnel_config(&cfg, "0.0.0.0:1080".parse().unwrap()) {
                Ok(_) => panic!("V2 no-auth SOCKS must remain loopback-only"),
                Err(error) => error,
            };

        assert_eq!(error.code(), TP_MOBILE_INVALID_CONFIG);
        assert!(error.to_string().contains("loopback"));
    }

    #[test]
    fn ffi_start_rejects_null_and_bad_json() {
        assert_eq!(
            tp_mobile_start_proxy(std::ptr::null()),
            TP_MOBILE_INVALID_ARGUMENT
        );

        let bad = CString::new("{").unwrap();
        assert_eq!(tp_mobile_start_proxy(bad.as_ptr()), TP_MOBILE_INVALID_JSON);
    }

    #[test]
    fn start_state_rejects_duplicate_and_stop_is_idempotent() {
        let state = MobileProxyState::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_for_handle = stopped.clone();
        let req = valid_request();

        start_proxy_with_runner(&state, req.clone(), |_, _, _| {
            Ok(Box::new(TestProxyHandle {
                stopped: stopped_for_handle,
            }))
        })
        .expect("first start should be accepted");

        let duplicate = start_proxy_with_runner(&state, req, |_, _, _| {
            panic!("runner must not be invoked while already running")
        })
        .expect_err("second start should fail");

        assert_eq!(duplicate.code(), TP_MOBILE_ALREADY_RUNNING);
        assert!(!stopped.load(Ordering::SeqCst));

        stop_proxy_with_state(&state).expect("stop should succeed");
        assert!(stopped.load(Ordering::SeqCst));
        stop_proxy_with_state(&state).expect("second stop should be a no-op");
    }

    #[test]
    fn start_state_records_last_error_when_runner_fails() {
        let state = MobileProxyState::default();
        let err = start_proxy_with_runner(&state, valid_request(), |_, _, _| {
            Err(MobileError::start_failed("platform connect timed out"))
        })
        .expect_err("start should fail");

        assert_eq!(err.code(), TP_MOBILE_START_FAILED);

        let raw = status_json_with_state(&state).expect("status should serialize");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");
        assert_eq!(value["running"], false);
        assert_eq!(value["last_error"]["code"], TP_MOBILE_START_FAILED);
        assert!(value["last_error"]["error"]
            .as_str()
            .unwrap()
            .contains("platform connect timed out"));
    }

    #[test]
    fn status_json_does_not_block_while_start_runner_waits() {
        let state = Arc::new(MobileProxyState::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let state_for_thread = state.clone();
        let start_thread = std::thread::spawn(move || {
            start_proxy_with_runner(&state_for_thread, valid_request(), |_, progress, _| {
                progress.set("test runner waiting");
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Err(MobileError::start_failed("forced delayed failure"))
            })
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("runner should enter");
        let started = Instant::now();
        let raw = status_json_with_state(&state).expect("status should serialize while starting");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "status_json should not wait for the start runner"
        );
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["running"], false);
        assert_eq!(value["startup"]["active"], true);
        assert_eq!(value["startup"]["phase"], "test runner waiting");

        release_tx.send(()).unwrap();
        let err = start_thread.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("forced delayed failure"));
    }

    #[test]
    fn stop_during_start_cancels_start_runner() {
        let state = Arc::new(MobileProxyState::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let state_for_thread = state.clone();
        let start_thread = std::thread::spawn(move || {
            start_proxy_with_runner(
                &state_for_thread,
                valid_request(),
                |_, _progress, cancel| {
                    entered_tx.send(()).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancel.is_cancelled() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if cancel.is_cancelled() {
                        Err(start_cancelled_error())
                    } else {
                        Err(MobileError::start_failed("runner was not cancelled"))
                    }
                },
            )
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("runner should enter");
        stop_proxy_with_state(&state).expect("stop should cancel in-flight start");

        let err = start_thread
            .join()
            .expect("start thread should exit")
            .expect_err("cancelled start should fail");
        assert!(err.to_string().contains("cancelled"));

        let raw = status_json_with_state(&state).expect("status should serialize");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");
        assert_eq!(value["running"], false);
        assert_eq!(value["startup"], serde_json::Value::Null);
        assert_eq!(value["last_error"], serde_json::Value::Null);
    }

    #[test]
    fn mobile_proxy_ready_timeout_allows_slow_mobile_networks() {
        assert!(MOBILE_STAGE_TIMEOUT >= Duration::from_secs(30));
        assert!(MOBILE_PROXY_READY_TIMEOUT >= MOBILE_STAGE_TIMEOUT * 3);
    }

    #[test]
    fn mobile_connected_health_accepts_relay_when_p2p_enabled() {
        let status = ConnectionStatus {
            connected: true,
            path_mode: tp_client::status::ConnectionPathMode::Relay,
            platform_heartbeat: tp_client::status::HeartbeatStatus {
                active: true,
                last_time: Some(1),
                last_error: None,
            },
            transport_heartbeat: tp_client::status::HeartbeatStatus {
                active: true,
                last_time: Some(1),
                last_error: None,
            },
            ..Default::default()
        };

        assert!(mobile_connected_health_ready(&status));
    }

    #[test]
    fn mobile_vpn_start_ready_does_not_require_heartbeat_health() {
        let status = ConnectionStatus {
            connected: true,
            path_mode: tp_client::status::ConnectionPathMode::Relay,
            ..Default::default()
        };

        assert!(mobile_vpn_start_ready(&status));
        assert!(
            !mobile_connected_health_ready(&status),
            "startup readiness must be earlier than full heartbeat health"
        );
    }

    #[tokio::test]
    async fn wait_for_mobile_vpn_start_ready_fails_when_not_connected() {
        let progress = StartupProgress::new(Arc::new(Mutex::new(MobileStartupStatus::default())));
        let cancel = StartupCancel::default();
        let err = wait_for_mobile_vpn_start_ready(
            || ConnectionStatus {
                connected: false,
                connecting: true,
                message: "reconnecting relay".into(),
                ..Default::default()
            },
            &progress,
            Duration::from_millis(15),
            &cancel,
        )
        .await
        .expect_err("VPN ready must be gated on an active relay connection");

        assert_eq!(err.code(), TP_MOBILE_START_FAILED);
        assert!(err.to_string().contains("relay connection unavailable"));
    }

    #[test]
    fn mobile_relay_ready_timeout_allows_slow_real_device_startup() {
        assert!(
            MOBILE_RELAY_READY_TIMEOUT >= Duration::from_secs(30),
            "iOS Packet Tunnel startup must allow slow first relay readiness"
        );
    }

    #[test]
    fn start_request_log_level_defaults_to_info() {
        let raw = start_request_json(Some("device-1"), None);
        let req = parse_start_request(&raw).expect("parse start request");

        assert_eq!(req.log_level, "info");
    }

    #[test]
    fn start_request_accepts_log_level_override() {
        let raw = start_request_json(Some("device-1"), Some("tp_client=debug,tp_transport=info"));
        let req = parse_start_request(&raw).expect("parse start request");

        assert_eq!(req.log_level, "tp_client=debug,tp_transport=info");
    }

    #[test]
    fn mobile_log_buffer_is_bounded_by_line_count() {
        let mut buf = MobileLogBuffer::default();
        for i in 0..=MOBILE_LOG_BUFFER_MAX_LINES {
            buf.push(format!("line-{i}"));
        }

        assert_eq!(buf.lines.len(), MOBILE_LOG_BUFFER_MAX_LINES);
        assert_eq!(buf.lines.front().map(String::as_str), Some("line-1"));
    }

    #[test]
    fn mobile_log_buffer_drops_oversized_content() {
        let mut buf = MobileLogBuffer::default();
        buf.push("x".repeat(MOBILE_LOG_BUFFER_MAX_BYTES + 1));

        assert!(buf.lines.is_empty());
        assert_eq!(buf.bytes, 0);
    }

    #[test]
    fn missing_v2_runtime_config_is_start_failure() {
        let err = v2_runtime_config_unavailable_error();
        assert_eq!(err.code(), TP_MOBILE_START_FAILED);
        assert!(err.to_string().contains("V2 runtime config unavailable"));
    }

    #[test]
    fn thread_proxy_handle_status_json_uses_live_connection_snapshot() {
        let shared = Arc::new(Mutex::new(MobileProxyHandleState {
            listen_addr: Some("127.0.0.1:1080".parse().unwrap()),
            connection_status: Some(ConnectionStatus {
                connected: true,
                traffic: tp_client::status::TrafficStats {
                    p2p_tx_bytes: 0,
                    p2p_rx_bytes: 0,
                    relay_tx_bytes: 0,
                    relay_rx_bytes: 0,
                },
                ..Default::default()
            }),
            ..Default::default()
        }));
        let live_status = Arc::new(Mutex::new(ConnectionStatus {
            connected: true,
            traffic: tp_client::status::TrafficStats {
                p2p_tx_bytes: 4096,
                p2p_rx_bytes: 8192,
                relay_tx_bytes: 1024,
                relay_rx_bytes: 2048,
            },
            ..Default::default()
        }));
        let status_provider = {
            let live_status = live_status.clone();
            Arc::new(move || live_status.lock().unwrap().clone())
        };
        let handle = ThreadProxyHandle {
            stop_tx: None,
            join: None,
            shared,
            status_provider,
            mesh_provider: Arc::new(V2RuntimeSnapshot::default),
        };

        let raw = handle.status_json().expect("status should serialize");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");

        assert_eq!(value["connection"]["traffic"]["p2p_tx_bytes"], 4096);
        assert_eq!(value["connection"]["traffic"]["p2p_rx_bytes"], 8192);
        assert_eq!(value["connection"]["traffic"]["relay_tx_bytes"], 1024);
        assert_eq!(value["connection"]["traffic"]["relay_rx_bytes"], 2048);
    }

    /// The phone cannot show who else is in the Tunnel unless the status it
    /// polls says so. The shared crate already produces the directory; this is
    /// only about it reaching the app.
    #[test]
    fn status_json_carries_this_peer_and_the_peer_directory() {
        use tp_client::runtime_snapshot::{
            V2PeerDirectoryPhase, V2PeerDirectorySnapshot, V2RemotePeerPhase, V2RemotePeerSnapshot,
            V2RoutingPhase, V2ThisPeerSnapshot,
        };

        let shared = Arc::new(Mutex::new(MobileProxyHandleState::default()));
        let mesh = Arc::new(Mutex::new(V2RuntimeSnapshot {
            this_peer: Some(V2ThisPeerSnapshot {
                peer_id: "peer-local".into(),
                overlay_ip: "198.18.0.5".parse().unwrap(),
            }),
            peer_directory: V2PeerDirectorySnapshot {
                phase: V2PeerDirectoryPhase::Ready,
                reason_code: None,
                peers: vec![V2RemotePeerSnapshot {
                    peer_id: "peer-remote".into(),
                    overlay_ip: Some("198.18.0.6".parse().unwrap()),
                    phase: V2RemotePeerPhase::Ready,
                    reason_code: None,
                    current_path: None,
                    usable_lanes: None,
                    routing: V2RoutingPhase::Ready,
                    exports: Vec::new(),
                }],
            },
            ..Default::default()
        }));
        let handle = ThreadProxyHandle {
            stop_tx: None,
            join: None,
            shared,
            status_provider: Arc::new(ConnectionStatus::default),
            mesh_provider: {
                let mesh = mesh.clone();
                Arc::new(move || mesh.lock().unwrap().clone())
            },
        };

        let raw = handle.status_json().expect("status should serialize");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");

        assert_eq!(value["this_peer"]["overlay_ip"], "198.18.0.5");
        assert_eq!(value["peer_directory"]["phase"], "ready");
        assert_eq!(
            value["peer_directory"]["peers"][0]["peer_id"],
            "peer-remote"
        );
        assert_eq!(
            value["peer_directory"]["peers"][0]["overlay_ip"],
            "198.18.0.6"
        );
    }

    /// The projection the desktop renders is the projection a phone renders.
    ///
    /// It lived in the desktop Tauri crate, out of a phone's reach, so Kotlin
    /// and Swift each re-derived the same vocabulary and drifted apart doing
    /// it. Publishing it here is what lets one UI serve every Client.
    #[test]
    fn status_json_carries_the_shared_client_ui_projection() {
        use tp_client::runtime_snapshot::{
            V2OverallPhase, V2PeerDirectoryPhase, V2PeerDirectorySnapshot, V2PeerPath,
            V2RemotePeerPhase, V2RemotePeerSnapshot, V2RoutingPhase, V2RuntimePhase,
            V2RuntimeSnapshot, V2ThisPeerSnapshot,
        };

        let runtime = V2RuntimeSnapshot {
            overall: V2RuntimePhase {
                phase: V2OverallPhase::Connected,
                reason_code: None,
            },
            this_peer: Some(V2ThisPeerSnapshot {
                peer_id: "peer-local".into(),
                overlay_ip: "198.18.0.5".parse().unwrap(),
            }),
            peer_directory: V2PeerDirectorySnapshot {
                phase: V2PeerDirectoryPhase::Ready,
                reason_code: None,
                peers: vec![V2RemotePeerSnapshot {
                    peer_id: "peer-remote".into(),
                    overlay_ip: Some("198.18.0.6".parse().unwrap()),
                    phase: V2RemotePeerPhase::Ready,
                    reason_code: None,
                    current_path: Some(V2PeerPath::EncryptedRelay),
                    usable_lanes: None,
                    routing: V2RoutingPhase::Ready,
                    exports: Vec::new(),
                }],
            },
            ..Default::default()
        };

        let handle = ThreadProxyHandle {
            stop_tx: None,
            join: None,
            shared: Arc::new(Mutex::new(MobileProxyHandleState::default())),
            status_provider: Arc::new(ConnectionStatus::default),
            mesh_provider: Arc::new(move || runtime.clone()),
        };

        let raw = handle.status_json().expect("status should serialize");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("status is JSON");

        assert_eq!(value["client_ui"]["overall"], "connected");
        // The desktop renders a Peer as a /32 overlay address, not a bare IP.
        assert_eq!(
            value["client_ui"]["peer_directory"]["peers"][0]["overlay_cidr"],
            "198.18.0.6/32"
        );
        assert_eq!(
            value["client_ui"]["peer_directory"]["peers"][0]["current_path"],
            "encrypted_relay"
        );
        assert_eq!(
            value["client_ui"]["this_peer"]["overlay_cidr"],
            "198.18.0.5/32"
        );
        // A phone always routes through its VPN service and has no helper that
        // can be missing, so the desktop's Needs-helper state cannot arise.
        assert_eq!(value["client_ui"]["native_routing"]["state"], "ready");
    }

    /// A phone was unreachable no matter what its owner wanted.
    ///
    /// The mobile runtime never installed an access policy, so it ran on the
    /// engine's startup placeholder, which refuses everything. Desktop Peers
    /// could be reached and phones could not, with nothing in either UI saying
    /// so. The request carries the policy now, and an absent one means open —
    /// the same rule the desktop settings use.
    #[test]
    fn a_start_request_without_a_policy_is_open() {
        let req: StartProxyRequest =
            serde_json::from_str(&serde_json::to_string(&valid_request()).unwrap()).unwrap();

        assert!(req.client_access.allow.is_empty());
        assert!(req.client_access.deny.is_empty());
        assert!(!req.client_access.is_closed());
    }

    #[test]
    fn a_start_request_carries_the_rules_it_was_given() {
        let raw = serde_json::json!({
            "peer_profile": serde_json::to_value(managed_peer_profile()).unwrap(),
            "device_id": "f3b59e76-4f3d-43da-83f9-2f7bec248fb0",
            "client_access": {
                "deny": [{
                    "target": { "type": "cidr", "value": "10.0.0.0/8" },
                    "protocol": "tcp",
                    "port": { "type": "any" }
                }]
            }
        });

        let req: StartProxyRequest = serde_json::from_value(raw).expect("request parses");

        assert_eq!(req.client_access.deny.len(), 1);
        assert!(req.client_access.allow.is_empty());
    }

    #[test]
    fn a_policy_that_cannot_compile_stops_the_start() {
        let raw = serde_json::json!({
            "peer_profile": serde_json::to_value(managed_peer_profile()).unwrap(),
            "device_id": "f3b59e76-4f3d-43da-83f9-2f7bec248fb0",
            "client_access": {
                "allow": [{
                    "target": { "type": "cidr", "value": "not-a-network" },
                    "protocol": "tcp",
                    "port": { "type": "any" }
                }]
            }
        });
        let req: StartProxyRequest = serde_json::from_value(raw).expect("request parses");

        assert!(
            compiled_client_access(&req).is_err(),
            "a rule the runtime cannot compile must fail the start, not be dropped"
        );
    }

    /// A phone can publish a network, like every other Client.
    ///
    /// The start request carried no exports at all, so the mobile runtime
    /// installed no local runtime record — a phone could reach the Tunnel but
    /// could never share anything with it, and the Settings screen said so as
    /// though it were a decision rather than a missing field.
    #[test]
    fn a_start_request_can_publish_a_network() {
        let json = r#"{
            "peer_profile": PROFILE,
            "exported_lans": ["192.168.7.0/24"],
            "tunnel_first": true
        }"#
        .replace(
            "PROFILE",
            &serde_json::to_string(&managed_peer_profile()).unwrap(),
        );

        let req: StartProxyRequest = serde_json::from_str(&json).expect("request parses");

        assert_eq!(req.exported_lans, vec!["192.168.7.0/24".to_string()]);
        assert!(req.tunnel_first);
    }

    /// The field has to reach the engine, not just parse.
    ///
    /// A start request that carries exports but never installs a local runtime
    /// record leaves the phone publishing nothing, which is the same failure
    /// as not having the field at all.
    #[test]
    fn exports_become_a_local_runtime_record() {
        let mut req = valid_request();
        req.exported_lans = vec!["192.168.7.0/24".into()];

        let cfg = req.validated().expect("request validates");

        assert_eq!(cfg.local_runtime_record.lan_exports.len(), 1);
        assert_eq!(
            cfg.local_runtime_record.lan_exports[0].prefix.prefix_len,
            24
        );
    }

    #[test]
    fn an_export_that_is_not_a_canonical_prefix_fails_the_start() {
        let mut req = valid_request();
        req.exported_lans = vec!["192.168.7.42/24".into()];

        assert!(req.validated().is_err(), "a host address is not a network");
    }

    #[test]
    fn a_start_request_publishes_nothing_by_default() {
        let req = valid_request();

        assert!(req.exported_lans.is_empty());
        assert!(!req.tunnel_first);
    }

    fn valid_request() -> StartProxyRequest {
        StartProxyRequest {
            client_access: Default::default(),
            peer_profile: managed_peer_profile(),
            device_id: "f3b59e76-4f3d-43da-83f9-2f7bec248fb0".into(),
            device_name: None,
            local_socks5_listen: "127.0.0.1:1080".into(),
            p2p_allow_lan_candidates: false,
            log_level: DEFAULT_LOG_LEVEL.into(),
            insecure_tls: false,
            exported_lans: Vec::new(),
            tunnel_first: false,
        }
    }

    struct TestProxyHandle {
        stopped: Arc<AtomicBool>,
    }

    impl ProxyHandle for TestProxyHandle {
        fn stop(self: Box<Self>) {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }
}
