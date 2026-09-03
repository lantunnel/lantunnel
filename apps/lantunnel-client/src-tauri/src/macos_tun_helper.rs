use std::ffi::{c_char, c_void, CString};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::desktop_routes::lan_route_specs;
use crate::desktop_tun::{self, DesktopTunConfig, DesktopTunTask};

#[allow(dead_code)]
pub const HELPER_LABEL: &str = "app.lantunnel.tun-helper";
pub const HELPER_PLIST_NAME: &str = "app.lantunnel.tun-helper.plist";
#[allow(dead_code)]
pub const HELPER_SOCKET_NAME: &str = "TunHelperSocket";
pub const HELPER_SOCKET_PATH: &str = "/var/run/app.lantunnel.tun-helper.sock";
const HELPER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_REREGISTER_WAIT: Duration = Duration::from_millis(700);
#[cfg(test)]
pub const OVERLAY_ROUTES_CAPABILITY: &str = "overlay_routes_v1";
pub const DYNAMIC_ROUTES_CAPABILITY: &str = "dynamic_routes_v2";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunSocks5Auth {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunStartRequest {
    pub runtime_id: String,
    pub interface_name: String,
    pub tunnel_ipv4: String,
    pub socks5_address: String,
    pub socks5_port: u16,
    pub socks5_auth: Option<TunSocks5Auth>,
    pub sidecar_path: String,
    pub routes: Vec<String>,
    pub overlay_routes: Vec<String>,
    #[serde(default)]
    pub learned_lan_routes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunOverlayRoutesRequest {
    pub runtime_id: String,
    pub routes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunDynamicRoutesRequest {
    pub runtime_id: String,
    pub overlay_routes: Vec<String>,
    pub learned_lan_routes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunRuntimeRequest {
    pub runtime_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TunHelperRequest {
    Status,
    Capabilities,
    Start(TunStartRequest),
    SyncOverlayRoutes(TunOverlayRoutesRequest),
    SyncDynamicRoutes(TunDynamicRoutesRequest),
    Stop(TunRuntimeRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunHelperResponse {
    pub ok: bool,
    pub running: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunHelperStatus {
    pub installed: bool,
    pub running: bool,
    pub version: Option<String>,
    pub message: String,
}

pub fn validate_start_request(request: &TunStartRequest, max_routes: usize) -> Result<(), String> {
    validate_runtime_id(&request.runtime_id)?;
    if request.interface_name.trim().is_empty() {
        return Err("interface name is required".into());
    }
    let tunnel_ipv4 = request
        .tunnel_ipv4
        .parse::<Ipv4Addr>()
        .map_err(|e| format!("TUN Overlay IPv4 is invalid: {e}"))?;
    let socks5_ip = request
        .socks5_address
        .parse::<IpAddr>()
        .map_err(|e| format!("TUN SOCKS5 address is invalid: {e}"))?;
    if !socks5_ip.is_loopback() {
        return Err("TUN SOCKS5 address must be loopback".into());
    }
    if request.socks5_port == 0 {
        return Err("TUN SOCKS5 port is required".into());
    }
    if let Some(auth) = &request.socks5_auth {
        if auth.username.is_empty() {
            return Err("TUN SOCKS5 username is required when auth is enabled".into());
        }
        if auth.password.is_empty() {
            return Err("TUN SOCKS5 password is required when auth is enabled".into());
        }
    }
    if request.sidecar_path.trim().is_empty() {
        return Err("TUN sidecar path is required".into());
    }
    validate_macos_interface_name(&request.interface_name)?;
    crate::desktop_routes::validate_lan_routes(&request.routes, max_routes)
        .map_err(|e| e.to_string())?;
    desktop_tun::validated_remote_overlay_routes(tunnel_ipv4, &request.overlay_routes)?;
    desktop_tun::validated_learned_lan_routes(&request.learned_lan_routes)?;
    Ok(())
}

fn validate_runtime_id(runtime_id: &str) -> Result<(), String> {
    if runtime_id.is_empty() || runtime_id.len() > 128 || !runtime_id.is_ascii() {
        Err("TUN runtime ID is invalid".into())
    } else {
        Ok(())
    }
}

fn validate_macos_interface_name(interface_name: &str) -> Result<(), String> {
    let suffix = interface_name
        .strip_prefix("utun")
        .ok_or_else(|| "interface name must be a utun device".to_string())?;
    if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("interface name must be a utun device".into());
    }
    Ok(())
}

pub fn start_desktop_tun(config: DesktopTunConfig) -> Result<DesktopTunTask, String> {
    let routes = lan_route_specs(&config.routes, config.max_routes)
        .map_err(|e| format!("invalid LAN routes for TUN mode: {e}"))?;
    let overlay_routes =
        desktop_tun::validated_remote_overlay_routes(config.tunnel_ipv4, &config.overlay_routes)?;
    let learned_lan_routes = desktop_tun::validated_learned_lan_routes(&config.learned_lan_routes)?;
    fs::create_dir_all(&config.config_dir)
        .map_err(|e| format!("create TUN config directory failed: {e}"))?;

    let interface_name = desktop_tun::default_interface_name().to_string();
    let config_path = config.config_dir.join("desktop-tun.yml");
    let bundled_sidecar_dir = desktop_tun::ensure_bundled_sidecars(&config.config_dir)?;
    let sidecar = desktop_tun::resolve_sidecar_binary(
        config.resource_dir.as_deref(),
        bundled_sidecar_dir.as_deref(),
    );
    let socks5_addr = helper_socks5_addr(config.socks5_addr);
    ensure_dynamic_routes_helper()?;

    let runtime_id = uuid::Uuid::new_v4().to_string();
    let request = TunStartRequest {
        runtime_id: runtime_id.clone(),
        interface_name: interface_name.clone(),
        tunnel_ipv4: config.tunnel_ipv4.to_string(),
        socks5_address: socks5_addr.ip().to_string(),
        socks5_port: socks5_addr.port(),
        socks5_auth: config.socks5_auth.as_ref().map(|auth| TunSocks5Auth {
            username: auth.username.clone(),
            password: auth.password.clone(),
        }),
        sidecar_path: sidecar.display().to_string(),
        routes: routes.iter().map(|route| route.cidr.clone()).collect(),
        overlay_routes: overlay_routes
            .iter()
            .map(|route| route.cidr.clone())
            .collect(),
        learned_lan_routes: learned_lan_routes
            .iter()
            .map(|route| route.cidr.clone())
            .collect(),
    };
    validate_start_request(&request, config.max_routes)?;

    let mut response = start_blocking(request.clone())?;
    if !response.ok && helper_response_needs_registration_refresh(&response.message) {
        refresh_helper_registration()?;
        thread::sleep(HELPER_REREGISTER_WAIT);
        response = start_blocking(request)?;
    }
    if !response.ok {
        let _ = fs::remove_file(&config_path);
        return Err(response.message);
    }

    Ok(DesktopTunTask::macos_helper(
        config_path,
        interface_name,
        config.tunnel_ipv4,
        routes,
        overlay_routes,
        learned_lan_routes,
        runtime_id,
    ))
}

fn ensure_dynamic_routes_helper() -> Result<(), String> {
    let response = send_request(TunHelperRequest::Capabilities)?;
    if response.ok && response.message == DYNAMIC_ROUTES_CAPABILITY {
        return Ok(());
    }

    refresh_helper_registration()?;
    thread::sleep(HELPER_REREGISTER_WAIT);
    let response = send_request(TunHelperRequest::Capabilities)?;
    if response.ok && response.message == DYNAMIC_ROUTES_CAPABILITY {
        Ok(())
    } else {
        Err("macOS TUN helper does not support dynamic route synchronization".into())
    }
}

fn helper_response_needs_registration_refresh(message: &str) -> bool {
    message.contains("helper request denied for unexpected client binary")
        || message.contains("TUN sidecar path must match the bundled helper sidecar")
        || message.contains("helper is not running from a bundled app")
}

fn helper_socks5_addr(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port())
        }
        _ => addr,
    }
}

pub fn status() -> Result<TunHelperStatus, String> {
    let app_service = app_service_status();
    let running = send_request(TunHelperRequest::Status)
        .map(|response| response.ok && response.running)
        .unwrap_or(false);

    let (installed, message) = match app_service {
        Ok(SmAppServiceState::Enabled) => (true, String::new()),
        Ok(SmAppServiceState::RequiresApproval) => (
            false,
            "Approve the Lantunnel TUN helper in System Settings, then retry LAN routes via TUN."
                .into(),
        ),
        Ok(SmAppServiceState::NotRegistered) => (
            false,
            "Install the macOS helper to use LAN routes via TUN.".into(),
        ),
        Ok(SmAppServiceState::NotFound) => (
            false,
            "The macOS helper plist was not found in this app bundle.".into(),
        ),
        Err(e) => (false, e),
    };

    Ok(TunHelperStatus {
        installed,
        running,
        version: Some(env!("CARGO_PKG_VERSION").into()),
        message,
    })
}

pub async fn install() -> Result<TunHelperStatus, String> {
    tokio::task::spawn_blocking(register_helper)
        .await
        .map_err(|e| format!("install helper task failed: {e}"))??;
    status()
}

#[allow(dead_code)]
pub async fn start(request: TunStartRequest) -> Result<TunHelperResponse, String> {
    tokio::task::spawn_blocking(move || start_blocking(request))
        .await
        .map_err(|e| format!("start helper request task failed: {e}"))?
}

#[allow(dead_code)]
pub async fn stop(runtime_id: String) -> Result<TunHelperResponse, String> {
    tokio::task::spawn_blocking(move || stop_blocking(&runtime_id))
        .await
        .map_err(|e| format!("stop helper request task failed: {e}"))?
}

pub(crate) fn start_blocking(request: TunStartRequest) -> Result<TunHelperResponse, String> {
    send_request(TunHelperRequest::Start(request))
}

pub(crate) fn stop_blocking(runtime_id: &str) -> Result<TunHelperResponse, String> {
    send_request(TunHelperRequest::Stop(TunRuntimeRequest {
        runtime_id: runtime_id.to_string(),
    }))
}

pub(crate) fn sync_dynamic_routes_blocking(
    runtime_id: &str,
    overlay_routes: Vec<String>,
    learned_lan_routes: Vec<String>,
) -> Result<(), String> {
    let response = send_request(TunHelperRequest::SyncDynamicRoutes(
        TunDynamicRoutesRequest {
            runtime_id: runtime_id.to_string(),
            overlay_routes,
            learned_lan_routes,
        },
    ))?;
    if response.ok {
        Ok(())
    } else {
        Err(response.message)
    }
}

fn send_request(request: TunHelperRequest) -> Result<TunHelperResponse, String> {
    let mut stream = UnixStream::connect(HELPER_SOCKET_PATH)
        .map_err(|e| format!("connect macOS TUN helper failed: {e}"))?;
    stream
        .set_read_timeout(Some(HELPER_CONNECT_TIMEOUT))
        .map_err(|e| format!("configure helper read timeout failed: {e}"))?;
    stream
        .set_write_timeout(Some(HELPER_CONNECT_TIMEOUT))
        .map_err(|e| format!("configure helper write timeout failed: {e}"))?;

    serde_json::to_writer(&mut stream, &request)
        .map_err(|e| format!("encode helper request failed: {e}"))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("write helper request failed: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush helper request failed: {e}"))?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read helper response failed: {e}"))?;
    if line.trim().is_empty() {
        return Err("macOS TUN helper returned an empty response".into());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("decode helper response failed: {e}"))
}

fn register_helper() -> Result<(), String> {
    match app_service_status()? {
        SmAppServiceState::Enabled => return Ok(()),
        SmAppServiceState::RequiresApproval => {
            open_system_settings_login_items()?;
            return Ok(());
        }
        SmAppServiceState::NotRegistered | SmAppServiceState::NotFound => {}
    }

    register_helper_service()
}

fn refresh_helper_registration() -> Result<(), String> {
    unsafe {
        let service = sm_daemon_service()?;
        let mut error: *mut c_void = std::ptr::null_mut();
        let ok = objc_msg_send_bool_error(
            service,
            sel("unregisterAndReturnError:")?,
            &mut error as *mut *mut c_void,
        );
        if !ok {
            match app_service_status()? {
                SmAppServiceState::NotRegistered | SmAppServiceState::NotFound => {}
                _ => {
                    return Err(format!(
                        "unregister stale macOS TUN helper failed: {}",
                        ns_error_description(error)
                    ))
                }
            }
        }
    }
    thread::sleep(HELPER_REREGISTER_WAIT);
    register_helper_service()
}

fn register_helper_service() -> Result<(), String> {
    unsafe {
        let service = sm_daemon_service()?;
        let mut error: *mut c_void = std::ptr::null_mut();
        let ok = objc_msg_send_bool_error(
            service,
            sel("registerAndReturnError:")?,
            &mut error as *mut *mut c_void,
        );
        if !ok {
            if app_service_status()? == SmAppServiceState::RequiresApproval {
                open_system_settings_login_items()?;
            }
            Err(format!(
                "register macOS TUN helper failed: {}",
                ns_error_description(error)
            ))
        } else {
            if app_service_status()? == SmAppServiceState::RequiresApproval {
                open_system_settings_login_items()?;
            }
            Ok(())
        }
    }
}

fn open_system_settings_login_items() -> Result<(), String> {
    unsafe {
        let class = objc_get_class(c"SMAppService".as_ptr());
        if class.is_null() {
            return Err("macOS 13 or newer is required for helper approval settings.".into());
        }
        objc_msg_send_void(class, sel("openSystemSettingsLoginItems")?);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmAppServiceState {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

fn app_service_status() -> Result<SmAppServiceState, String> {
    unsafe {
        let service = sm_daemon_service()?;
        match objc_msg_send_isize(service, sel("status")?) {
            0 => Ok(SmAppServiceState::NotRegistered),
            1 => Ok(SmAppServiceState::Enabled),
            2 => Ok(SmAppServiceState::RequiresApproval),
            3 => Ok(SmAppServiceState::NotFound),
            other => Err(format!("unknown SMAppService status: {other}")),
        }
    }
}

unsafe fn sm_daemon_service() -> Result<*mut c_void, String> {
    let class = objc_get_class(c"SMAppService".as_ptr());
    if class.is_null() {
        return Err("macOS 13 or newer is required for the TUN helper.".into());
    }
    let plist_name = ns_string(HELPER_PLIST_NAME)?;
    let service = objc_msg_send_id_id(class, sel("daemonServiceWithPlistName:")?, plist_name);
    if service.is_null() {
        Err("create SMAppService daemon handle failed".into())
    } else {
        Ok(service)
    }
}

unsafe fn ns_string(value: &str) -> Result<*mut c_void, String> {
    let c_value = CString::new(value).map_err(|_| "string contains NUL byte".to_string())?;
    let class = objc_get_class(c"NSString".as_ptr());
    if class.is_null() {
        return Err("NSString class is unavailable".into());
    }
    let string = objc_msg_send_id_cstr(class, sel("stringWithUTF8String:")?, c_value.as_ptr());
    if string.is_null() {
        Err("create NSString failed".into())
    } else {
        Ok(string)
    }
}

unsafe fn objc_msg_send_id_id(obj: *mut c_void, sel: *mut c_void, arg: *mut c_void) -> *mut c_void {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel, arg)
}

unsafe fn objc_msg_send_id_cstr(
    obj: *mut c_void,
    sel: *mut c_void,
    arg: *const c_char,
) -> *mut c_void {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void;
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel, arg)
}

unsafe fn objc_msg_send_id(obj: *mut c_void, sel: *mut c_void) -> *mut c_void {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel)
}

unsafe fn objc_msg_send_isize(obj: *mut c_void, sel: *mut c_void) -> isize {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void) -> isize;
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel)
}

unsafe fn objc_msg_send_bool_error(
    obj: *mut c_void,
    sel: *mut c_void,
    error: *mut *mut c_void,
) -> bool {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> bool;
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel, error)
}

unsafe fn objc_msg_send_void(obj: *mut c_void, sel: *mut c_void) {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void);
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel)
}

unsafe fn objc_msg_send_cstr(obj: *mut c_void, sel: *mut c_void) -> *const c_char {
    type MsgSend = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *const c_char;
    let f: MsgSend = std::mem::transmute(objc_msg_send as *const ());
    f(obj, sel)
}

unsafe fn ns_error_description(error: *mut c_void) -> String {
    if error.is_null() {
        return "unknown error".into();
    }
    let desc = objc_msg_send_id(
        error,
        sel("localizedDescription").unwrap_or(std::ptr::null_mut()),
    );
    if desc.is_null() {
        return "unknown error".into();
    }
    let raw = objc_msg_send_cstr(desc, sel("UTF8String").unwrap_or(std::ptr::null_mut()));
    if raw.is_null() {
        "unknown error".into()
    } else {
        std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned()
    }
}

fn sel(name: &str) -> Result<*mut c_void, String> {
    let name = CString::new(name).map_err(|_| "selector contains NUL byte".to_string())?;
    Ok(unsafe { sel_register_name(name.as_ptr()) })
}

#[link(name = "objc")]
unsafe extern "C" {
    #[link_name = "objc_getClass"]
    fn objc_get_class(name: *const c_char) -> *mut c_void;
    #[link_name = "sel_registerName"]
    fn sel_register_name(name: *const c_char) -> *mut c_void;
    #[link_name = "objc_msgSend"]
    fn objc_msg_send();
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_private_lan_routes() {
        let request = TunStartRequest {
            runtime_id: "runtime-1".into(),
            interface_name: "utun24".into(),
            tunnel_ipv4: "198.18.1.9".into(),
            socks5_address: "127.0.0.1".into(),
            socks5_port: 1080,
            socks5_auth: Some(TunSocks5Auth {
                username: "group-1".into(),
                password: "pass-1".into(),
            }),
            sidecar_path: "/Applications/Lantunnel Client.app/Contents/Resources/hev-socks5-tunnel"
                .into(),
            routes: vec!["192.168.0.0/16".into(), "10.0.0.0/8".into()],
            overlay_routes: vec!["198.18.7.23/32".into()],
            learned_lan_routes: vec!["192.168.70.11/32".into()],
        };

        validate_start_request(&request, 3).unwrap();
    }

    #[test]
    fn rejects_public_routes() {
        let request = TunStartRequest {
            runtime_id: "runtime-2".into(),
            interface_name: "utun24".into(),
            tunnel_ipv4: "198.18.1.9".into(),
            socks5_address: "127.0.0.1".into(),
            socks5_port: 1080,
            socks5_auth: None,
            sidecar_path: "/Applications/Lantunnel Client.app/Contents/Resources/hev-socks5-tunnel"
                .into(),
            routes: vec!["8.8.8.0/24".into()],
            overlay_routes: Vec::new(),
            learned_lan_routes: Vec::new(),
        };

        assert!(validate_start_request(&request, 3).is_err());
    }

    #[test]
    fn start_request_accepts_v2_learned_lan_subnet_exports() {
        let request = TunStartRequest {
            runtime_id: "runtime-learned-lan".into(),
            interface_name: "utun24".into(),
            tunnel_ipv4: "198.18.1.9".into(),
            socks5_address: "127.0.0.1".into(),
            socks5_port: 1080,
            socks5_auth: None,
            sidecar_path: "/Applications/Lantunnel Client.app/Contents/Resources/hev-socks5-tunnel"
                .into(),
            routes: Vec::new(),
            overlay_routes: Vec::new(),
            learned_lan_routes: vec!["192.168.70.0/24".into()],
        };

        validate_start_request(&request, 3)
            .expect("privileged helper accepts the same validated V2 LAN Export as the app");
    }

    #[test]
    fn start_request_keeps_validated_route_cidrs() {
        let routes = lan_route_specs(&["192.168.0.0/16".into(), "10.0.0.0/8".into()], 3).unwrap();

        assert_eq!(
            routes
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["192.168.0.0/16", "10.0.0.0/8"]
        );
    }

    #[test]
    fn wildcard_socks5_addr_normalizes_to_loopback_for_helper() {
        let addr = helper_socks5_addr("0.0.0.0:18080".parse().unwrap());

        assert_eq!(addr.to_string(), "127.0.0.1:18080");
    }

    #[test]
    fn stale_helper_errors_trigger_registration_refresh() {
        assert!(helper_response_needs_registration_refresh(
            "helper request denied for unexpected client binary: /Applications/Lantunnel Client.app/Contents/MacOS/lantunnel-client"
        ));
        assert!(helper_response_needs_registration_refresh(
            "TUN sidecar path must match the bundled helper sidecar"
        ));
        assert!(!helper_response_needs_registration_refresh(
            "TUN sidecar must not be group/world writable"
        ));
    }

    #[test]
    fn rejects_non_local_socks5_addr() {
        let request = TunStartRequest {
            runtime_id: "runtime-3".into(),
            interface_name: "utun24".into(),
            tunnel_ipv4: "198.18.1.9".into(),
            socks5_address: "192.168.1.20".into(),
            socks5_port: 1080,
            socks5_auth: None,
            sidecar_path: "/Applications/Lantunnel Client.app/Contents/Resources/hev-socks5-tunnel"
                .into(),
            routes: vec!["192.168.0.0/16".into()],
            overlay_routes: Vec::new(),
            learned_lan_routes: Vec::new(),
        };

        assert!(validate_start_request(&request, 3).is_err());
    }

    #[test]
    fn rejects_wildcard_socks5_addr_in_request() {
        let request = TunStartRequest {
            runtime_id: "runtime-4".into(),
            interface_name: "utun24".into(),
            tunnel_ipv4: "198.18.1.9".into(),
            socks5_address: "0.0.0.0".into(),
            socks5_port: 1080,
            socks5_auth: None,
            sidecar_path: "/Applications/Lantunnel Client.app/Contents/Resources/hev-socks5-tunnel"
                .into(),
            routes: vec!["192.168.0.0/16".into()],
            overlay_routes: Vec::new(),
            learned_lan_routes: Vec::new(),
        };

        assert!(validate_start_request(&request, 3).is_err());
    }

    #[test]
    fn overlay_update_request_cannot_choose_an_interface_or_local_address() {
        let encoded = serde_json::to_value(TunHelperRequest::SyncOverlayRoutes(
            TunOverlayRoutesRequest {
                runtime_id: "runtime-1".into(),
                routes: vec!["198.18.7.23/32".into()],
            },
        ))
        .unwrap()
        .to_string();

        assert!(!encoded.contains("interface_name"));
        assert!(!encoded.contains("tunnel_ipv4"));
        assert!(encoded.contains("runtime-1"));
        assert!(encoded.contains("198.18.7.23/32"));
    }

    #[test]
    fn dynamic_update_request_cannot_choose_an_interface_or_local_address() {
        let encoded = serde_json::to_value(TunHelperRequest::SyncDynamicRoutes(
            TunDynamicRoutesRequest {
                runtime_id: "runtime-1".into(),
                overlay_routes: vec!["198.18.7.23/32".into()],
                learned_lan_routes: vec!["192.168.70.11/32".into()],
            },
        ))
        .unwrap()
        .to_string();

        assert!(!encoded.contains("interface_name"));
        assert!(!encoded.contains("tunnel_ipv4"));
        assert!(encoded.contains("runtime-1"));
        assert!(encoded.contains("198.18.7.23/32"));
        assert!(encoded.contains("192.168.70.11/32"));
    }

    #[test]
    fn overlay_capability_probe_has_a_distinct_helper_request() {
        assert_eq!(
            serde_json::to_string(&TunHelperRequest::Capabilities).unwrap(),
            "\"Capabilities\""
        );
        assert_eq!(OVERLAY_ROUTES_CAPABILITY, "overlay_routes_v1");
    }

    #[test]
    fn learned_lan_sync_requires_the_dynamic_route_helper_capability() {
        assert_eq!(DYNAMIC_ROUTES_CAPABILITY, "dynamic_routes_v2");
        assert_ne!(DYNAMIC_ROUTES_CAPABILITY, OVERLAY_ROUTES_CAPABILITY);
    }
}
