//! YAML configuration schema shared by the gateway, client CLI, and client GUI.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const TRANSPORT_TYPE_QUIC: &str = "quic";
pub const TRANSPORT_TYPE_WEBSOCKET: &str = "websocket";
pub const TRANSPORT_TYPE_GRPC: &str = "grpc";

pub fn default_transport_type() -> String {
    TRANSPORT_TYPE_QUIC.into()
}

pub const DEFAULT_TUIC_NATIVE_NO_FRAGMENT_MAX_PAYLOAD: usize = 1392;
pub const DEFAULT_GATEWAY_MAPPING_PROBE_PORT: u16 = 8444;

pub fn default_gateway_mapping_probe_port() -> u16 {
    DEFAULT_GATEWAY_MAPPING_PROBE_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,
    #[serde(default)]
    pub client: Option<ClientConfig>,
}

// --- logging -------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub format: String,
    pub output: String,
    pub file: Option<String>,
    pub max_size: u32,
    pub max_backups: u32,
    pub max_age: u32,
    pub compress: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "text".into(),
            output: "stdout".into(),
            file: None,
            max_size: 100,
            max_backups: 3,
            max_age: 7,
            compress: true,
        }
    }
}

// --- gateway -------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub listen_addr: String,
    #[serde(default = "default_transport_type")]
    pub transport_type: String,
    #[serde(default)]
    pub tls_cert: Option<String>,
    #[serde(default)]
    pub tls_key: Option<String>,
    /// Directory containing static Lantunnel 2.0 `*.scope` files. When not
    /// configured, static V2 admission is disabled; Platform-managed scopes
    /// may still be installed from authoritative outbound-control snapshots.
    #[serde(default)]
    pub scopes_dir: Option<String>,
    #[serde(default)]
    pub auth_username: String,
    #[serde(default)]
    pub auth_password: String,
    #[serde(default)]
    pub credential: Option<CredentialConfig>,
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// UDP endpoint this Gateway's mapping service listens on. P2P clients probe
    /// it to discover the public mapping of the exact socket they will later use
    /// for hole punching. Every Gateway runs its own mapping service on its own
    /// registered port; the Gateway process probes that service rather than
    /// binding its socket.
    #[serde(default = "default_gateway_mapping_probe_port")]
    pub mapping_probe_port: u16,
    /// QUIC transport tuning (congestion control, keepalive, idle timeout).
    /// Empty section → sensible defaults for the Rust QUIC tunnel
    /// (`bbr` / 10s / 60s).
    #[serde(default)]
    pub transport: GatewayTransportConfig,
    /// P2P signaling knobs: peer-registry TTL, session-table TTL, and the
    /// gateway-stamped offset added to "now" when building a `P2pPunchSync`
    /// so both peers fire their burst at the same wall clock. Missing
    /// section → defaults tuned for stable P2P preference
    /// (120 s / 60 s / 250 ms).
    #[serde(default)]
    pub p2p: GatewayP2pConfig,
    /// Passive relay usage ledger sent and acknowledged over outbound control.
    #[serde(default)]
    pub usage_ledger: GatewayUsageLedgerConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_addr: String::new(),
            transport_type: default_transport_type(),
            tls_cert: None,
            tls_key: None,
            scopes_dir: None,
            auth_username: String::new(),
            auth_password: String::new(),
            credential: None,
            proxy: ProxyConfig::default(),
            mapping_probe_port: DEFAULT_GATEWAY_MAPPING_PROBE_PORT,
            transport: GatewayTransportConfig::default(),
            p2p: GatewayP2pConfig::default(),
            usage_ledger: GatewayUsageLedgerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayUsageLedgerConfig {
    pub wal_path: String,
}

impl Default for GatewayUsageLedgerConfig {
    fn default() -> Self {
        Self {
            wal_path: "state/relay-usage.wal".into(),
        }
    }
}

/// P2P signaling tuning. Deserialized from `gateway.p2p` section.
///
/// All three knobs have sensible defaults so dropping the section in YAML
/// keeps existing deployments working unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayP2pConfig {
    /// Direct-lane switch. The public Gateway normally requires `true`. The
    /// isolated single-Scope Static Relay acceptance clone may set `false`;
    /// it keeps V2 membership and zero-candidate PeerLink key agreement while
    /// rejecting Direct signaling and does not spawn the eviction sweeper.
    pub enabled: bool,
    /// Maximum age (seconds) for `PeerRegistry` entries before the eviction
    /// sweeper drops them. Tracks each authenticated client's last-known
    /// public/local addresses.
    pub peer_idle_secs: u64,
    /// Maximum age (seconds) for `SessionTable` entries (in-flight P2P
    /// signaling sessions) before the eviction sweeper drops them.
    pub session_idle_secs: u64,
    /// Milliseconds added to gateway "now" when stamping `P2pPunchSync.t_start_ms`.
    /// Both peers receive the same future timestamp so they can synchronize
    /// their hole-punch burst. 250 ms is the default.
    pub punch_sync_offset_ms: i64,
    /// Process-wide cap over pending plus authenticated V2 attachments.
    pub max_v2_attachments: usize,
    /// Process-wide hard cap on runtime Replica handles for one stable Peer.
    pub max_replicas_per_peer: usize,
}

impl Default for GatewayP2pConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            peer_idle_secs: 120,
            session_idle_secs: 60,
            punch_sync_offset_ms: 250,
            max_v2_attachments: 4096,
            max_replicas_per_peer: 8,
        }
    }
}

/// QUIC transport knobs. Deserialized from `gateway.transport` section.
///
/// Defaults for the user-facing gateway config:
///   congestion = "bbr", keep_alive = 10s, max_idle = 60s.
///
/// The binary combines these with tp-transport's game-streaming MTU profile
/// for the tunnel QUIC server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayTransportConfig {
    /// `"bbr"` | `"cubic"` | `"new_reno"`. Unknown values fall back to `bbr`.
    pub congestion: String,
    /// QUIC PING interval in seconds. 0 disables keepalive (not recommended).
    pub keep_alive_secs: u32,
    /// Idle timeout in seconds before quinn closes the connection. Must be
    /// larger than `keep_alive_secs` so our own PINGs keep it alive.
    pub max_idle_secs: u32,
}

impl Default for GatewayTransportConfig {
    fn default() -> Self {
        // See `tp_transport::QuicTuning::default` for the rationale behind
        // the 60 s choice; keep these user-facing timeout defaults in sync.
        Self {
            congestion: "bbr".into(),
            keep_alive_secs: 10,
            max_idle_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub db: Option<CredentialDbConfig>,
}

/// Retired credential-store configuration. Retained only so that an older
/// config file still parses; `lantunnel-gateway` refuses to start when it is
/// present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialDbConfig {
    pub driver: String,
    pub data_source: String,
    #[serde(default)]
    pub table_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyConfig {
    #[serde(default)]
    pub http: Option<HttpProxyConfig>,
    #[serde(default)]
    pub socks5: Option<Socks5ProxyConfig>,
    #[serde(default)]
    pub tuic: Option<TuicProxyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpProxyConfig {
    pub listen_addr: String,
    #[serde(default)]
    pub tls_cert: Option<String>,
    #[serde(default)]
    pub tls_key: Option<String>,
    /// When true, require `Proxy-Authorization: Basic …`. The embedder supplies
    /// the validator; there is no shared proxy secret.
    #[serde(default = "default_true")]
    pub auth_enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Socks5ProxyConfig {
    pub listen_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuicProxyConfig {
    pub listen_addr: String,
    pub congestion_control: String,
    pub udp_relay_mode: String,
    pub zero_rtt: bool,
    pub alpn: Vec<String>,
    pub max_idle_timeout: u32,
    pub auth_timeout: u32,
    pub heartbeat: u32,
    /// Max bare UDP payload to force as one TUIC datagram. 0 means use
    /// [`DEFAULT_TUIC_NATIVE_NO_FRAGMENT_MAX_PAYLOAD`].
    pub native_no_fragment_max_payload: usize,
}

impl Default for TuicProxyConfig {
    fn default() -> Self {
        Self {
            listen_addr: String::new(),
            congestion_control: "bbr".into(),
            udp_relay_mode: "native".into(),
            zero_rtt: false,
            alpn: vec!["h3".into()],
            max_idle_timeout: 30,
            auth_timeout: 15,
            heartbeat: 10,
            native_no_fragment_max_payload: DEFAULT_TUIC_NATIVE_NO_FRAGMENT_MAX_PAYLOAD,
        }
    }
}

// --- client --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalServiceRouteKindConfig {
    Overlay,
    PeerLanHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalServiceProtocolConfig {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalServiceSourcePolicyConfig {
    AnyTunnelPeer,
    Only { peers: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalServiceExportConfig {
    pub route_kind: LocalServiceRouteKindConfig,
    pub protocol: LocalServiceProtocolConfig,
    pub ingress_port: u16,
    pub source_policy: LocalServiceSourcePolicyConfig,
    pub local_host: String,
    pub local_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    #[serde(rename = "id", default)]
    pub client_id: String,
    #[serde(default)]
    pub tunnel_id: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    pub gateway: ClientGatewayConfig,
    #[serde(default)]
    pub forbidden_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Explicit target-side delivery policy for addresses owned by this Peer.
    /// Missing/empty means deny all Overlay and Peer-LAN ingress.
    #[serde(default)]
    pub local_service_exports: Vec<LocalServiceExportConfig>,
    #[serde(default)]
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub local_proxy: ClientLocalProxyConfig,
    /// Always-on Direct-path tuning for the symmetric Peer runtime.
    #[serde(default)]
    pub p2p: ClientP2pConfig,
}

/// Legacy transport-wire role retained until the transport codec is upgraded.
/// Public V2 Client configuration does not expose or select this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ClientRoleConfig {
    #[default]
    #[serde(rename = "client")]
    Client,
    #[serde(rename = "app")]
    App,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientLocalProxyConfig {
    pub enabled: bool,
    pub socks5_listen: String,
}

impl Default for ClientLocalProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socks5_listen: "127.0.0.1:1080".into(),
        }
    }
}

/// Always-on Direct-path tuning. Deserialized from `client.p2p`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientP2pConfig {
    /// Wait this many seconds after the relay session is up before the
    /// initiator kicks off the first P2P attempt. Gives time for the
    /// relay path to stabilise so we don't waste a punch on a flapping
    /// connection.
    pub attempt_after_relay_uptime_secs: u64,
    /// Initial cooldown after a failed P2P attempt; doubled per failure
    /// up to `cooldown_max_secs`.
    pub cooldown_initial_secs: u64,
    /// Cooldown ceiling — exponential backoff caps here.
    pub cooldown_max_secs: u64,
    /// Path scheduler: P2P RTT must be < `min_advantage * relay_rtt` to
    /// be preferred. Values above 1.0 prefer healthy P2P even when its RTT is
    /// slightly worse than relay.
    pub scheduler_p2p_min_advantage: f64,
    /// Path scheduler: this many consecutive healthy+advantageous cycles
    /// before flipping Relay→P2p.
    pub scheduler_stable_cycles: u32,
    /// Hole-punch burst: number of UDP probes per side. Matches
    /// `p2p::punch` defaults. Note: this side of the spec is gateway-
    /// stamped via `P2pPunchSync`, so the local field is informational.
    pub nat_burst_count: u8,
    /// Hole-punch burst: port offsets to probe (relative to the peer's
    /// announced public port) for symmetric-NAT traversal. Same Gateway-
    /// stamped caveat as `nat_burst_count`.
    pub nat_port_offsets: Vec<i8>,
    /// Allow host candidates from private LAN address ranges. Off by default
    /// so Internet-facing P2P keeps publishing/dialing only globally routable
    /// candidates unless the app explicitly opts into LAN performance tests.
    pub allow_lan_candidates: bool,
    /// Publish this Peer's RFC1918 host addresses to the Platform and install
    /// the resulting authenticated stable-Peer self-publication snapshot as
    /// trusted-Tunnel LAN Route Aliases.
    pub allow_lan_route_aliases: bool,
}

impl Default for ClientP2pConfig {
    fn default() -> Self {
        Self {
            attempt_after_relay_uptime_secs: 30,
            cooldown_initial_secs: 60,
            cooldown_max_secs: 600,
            scheduler_p2p_min_advantage: 1.2,
            scheduler_stable_cycles: 3,
            nat_burst_count: 30,
            nat_port_offsets: vec![0, 1, 2, 5, -1],
            allow_lan_candidates: false,
            allow_lan_route_aliases: false,
        }
    }
}

fn default_replicas() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientGatewayConfig {
    pub addr: String,
    #[serde(default = "default_transport_type")]
    pub transport_type: String,
    #[serde(default)]
    pub tls_cert: Option<String>,
    #[serde(default)]
    pub tls_insecure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    pub listen_addr: String,
    pub static_dir: Option<String>,
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
    #[serde(default = "default_true")]
    pub auth_enabled: bool,
    pub auth_username: String,
    pub auth_password: String,
    /// Session key for cookie signing.
    pub session_key: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: String::new(),
            static_dir: None,
            cors_allowed_origins: Vec::new(),
            auth_enabled: true,
            auth_username: String::new(),
            auth_password: String::new(),
            session_key: String::new(),
        }
    }
}

// --- loading -------------------------------------------------------------

pub fn load_from_str(s: &str) -> Result<Config, serde_yaml::Error> {
    serde_yaml::from_str(s)
}

pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Config, ConfigError> {
    let body = std::fs::read_to_string(path.as_ref())?;
    Ok(load_from_str(&body)?)
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

// --- validation ----------------------------------------------------------

impl Config {
    /// Cross-check required fields and internal consistency. Parity with the
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.log.validate()?;
        if let Some(gw) = &self.gateway {
            gw.validate()?;
        }
        if let Some(cl) = &self.client {
            cl.validate()?;
        }
        if self.gateway.is_none() && self.client.is_none() {
            return Err(ConfigError::Invalid(
                "config must define either [gateway] or [client] section".into(),
            ));
        }
        Ok(())
    }
}

impl LogConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "log.level: expected one of trace|debug|info|warn|error, got {other:?}"
                )));
            }
        }
        match self.format.as_str() {
            "text" | "json" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "log.format: expected text|json, got {other:?}"
                )));
            }
        }
        match self.output.as_str() {
            "stdout" | "stderr" => {}
            "file" => {
                if self.file.as_deref().unwrap_or("").is_empty() {
                    return Err(ConfigError::Invalid(
                        "log.file is required when log.output=\"file\"".into(),
                    ));
                }
            }
            other => {
                return Err(ConfigError::Invalid(format!(
                    "log.output: expected stdout|stderr|file, got {other:?}"
                )));
            }
        }
        Ok(())
    }
}

impl GatewayConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let listen_addr = parse_socket_addr("gateway.listen_addr", &self.listen_addr)?;
        validate_transport_type("gateway.transport_type", &self.transport_type)?;
        check_tls_pair("gateway", self.tls_cert.as_deref(), self.tls_key.as_deref())?;
        let has_fixed_auth_username = !self.auth_username.trim().is_empty();
        let has_fixed_auth_password = !self.auth_password.trim().is_empty();
        if has_fixed_auth_username != has_fixed_auth_password {
            return Err(ConfigError::Invalid(
                "gateway.auth_username and gateway.auth_password must both be set or both unset"
                    .into(),
            ));
        }
        if self.mapping_probe_port == 0 {
            return Err(ConfigError::Invalid(
                "gateway.mapping_probe_port must be a non-zero UDP port".into(),
            ));
        }
        if self.transport_type == TRANSPORT_TYPE_QUIC
            && self.mapping_probe_port == listen_addr.port()
        {
            return Err(ConfigError::Invalid(format!(
                "gateway.mapping_probe_port must not reuse the QUIC gateway.listen_addr UDP port {}",
                self.mapping_probe_port
            )));
        }
        if self
            .scopes_dir
            .as_deref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "gateway.scopes_dir must be a non-empty directory when configured".into(),
            ));
        }
        if self.scopes_dir.is_some()
            && (self.tls_cert.as_deref().is_none_or(str::is_empty)
                || self.tls_key.as_deref().is_none_or(str::is_empty))
        {
            return Err(ConfigError::Invalid(
                "gateway.tls_cert/tls_key required when gateway.scopes_dir is configured".into(),
            ));
        }
        if self.p2p.max_v2_attachments == 0 || self.p2p.max_replicas_per_peer == 0 {
            return Err(ConfigError::Invalid(
                "gateway.p2p.max_v2_attachments and max_replicas_per_peer must be non-zero".into(),
            ));
        }
        if let Some(cred) = &self.credential {
            cred.validate()?;
        }
        if let Some(http) = &self.proxy.http {
            parse_socket_addr("gateway.proxy.http.listen_addr", &http.listen_addr)?;
            check_tls_pair(
                "gateway.proxy.http",
                http.tls_cert.as_deref(),
                http.tls_key.as_deref(),
            )?;
        }
        if let Some(s5) = &self.proxy.socks5 {
            parse_socket_addr("gateway.proxy.socks5.listen_addr", &s5.listen_addr)?;
        }
        if let Some(tuic) = &self.proxy.tuic {
            let tuic_addr = parse_socket_addr("gateway.proxy.tuic.listen_addr", &tuic.listen_addr)?;
            if self.mapping_probe_port == tuic_addr.port() {
                return Err(ConfigError::Invalid(format!(
                    "gateway.mapping_probe_port must not reuse gateway.proxy.tuic.listen_addr port {}",
                    self.mapping_probe_port
                )));
            }
            match tuic.congestion_control.as_str() {
                "bbr" | "cubic" | "new_reno" => {}
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "gateway.proxy.tuic.congestion_control: expected bbr|cubic|new_reno, got {other:?}"
                    )));
                }
            }
            match tuic.udp_relay_mode.as_str() {
                "native" | "quic" => {}
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "gateway.proxy.tuic.udp_relay_mode: expected native|quic, got {other:?}"
                    )));
                }
            }
            if tuic.native_no_fragment_max_payload > u16::MAX as usize {
                return Err(ConfigError::Invalid(format!(
                    "gateway.proxy.tuic.native_no_fragment_max_payload: expected <= {}, got {}",
                    u16::MAX,
                    tuic.native_no_fragment_max_payload
                )));
            }
        }
        self.usage_ledger.validate()?;
        Ok(())
    }
}

impl GatewayUsageLedgerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.wal_path.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "gateway.usage_ledger.wal_path required".into(),
            ));
        }
        Ok(())
    }
}

impl CredentialConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.kind.as_str() {
            "memory" => {}
            "file" => {
                if self.file_path.as_deref().unwrap_or("").is_empty() {
                    return Err(ConfigError::Invalid(
                        "credential.file_path required when credential.type=\"file\"".into(),
                    ));
                }
            }
            "db" => {
                let db = self.db.as_ref().ok_or_else(|| {
                    ConfigError::Invalid(
                        "credential.db required when credential.type=\"db\"".into(),
                    )
                })?;
                if db.driver.is_empty() {
                    return Err(ConfigError::Invalid("credential.db.driver required".into()));
                }
                match db.driver.as_str() {
                    "mysql" | "postgres" | "sqlite" => {}
                    other => {
                        return Err(ConfigError::Invalid(format!(
                            "credential.db.driver: expected mysql|postgres|sqlite, got {other:?}"
                        )));
                    }
                }
                if db.data_source.is_empty() {
                    return Err(ConfigError::Invalid(
                        "credential.db.data_source required".into(),
                    ));
                }
            }
            other => {
                return Err(ConfigError::Invalid(format!(
                    "credential.type: expected memory|file|db, got {other:?}"
                )));
            }
        }
        Ok(())
    }
}

impl ClientConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.gateway.addr.is_empty() {
            return Err(ConfigError::Invalid(
                "client.gateway.addr is required".into(),
            ));
        }
        if !has_explicit_port(&self.gateway.addr) {
            return Err(ConfigError::Invalid(format!(
                "client.gateway.addr: expected host:port or URL with explicit port, got {:?}",
                self.gateway.addr
            )));
        }
        if self.replicas == 0 {
            return Err(ConfigError::Invalid("client.replicas must be >= 1".into()));
        }
        validate_transport_type(
            "client.gateway.transport_type",
            &self.gateway.transport_type,
        )?;
        if let Some(web) = &self.web {
            if web.enabled && !web.listen_addr.is_empty() {
                parse_socket_addr("client.web.listen_addr", &web.listen_addr)?;
                validate_cors_allowed_origins(
                    "client.web.cors_allowed_origins",
                    &web.cors_allowed_origins,
                )?;
            }
        }
        if self.local_proxy.enabled {
            parse_socket_addr(
                "client.local_proxy.socks5_listen",
                &self.local_proxy.socks5_listen,
            )?;
        }
        let mut local_export_keys = HashSet::new();
        for (i, export) in self.local_service_exports.iter().enumerate() {
            if export.ingress_port == 0 {
                return Err(ConfigError::Invalid(format!(
                    "client.local_service_exports[{i}].ingress_port must be > 0"
                )));
            }
            if export.local_port == 0 {
                return Err(ConfigError::Invalid(format!(
                    "client.local_service_exports[{i}].local_port must be > 0"
                )));
            }
            let local_host = export.local_host.parse::<std::net::IpAddr>().map_err(|_| {
                ConfigError::Invalid(format!(
                    "client.local_service_exports[{i}].local_host must be a literal IP address"
                ))
            })?;
            if export.local_host.trim() != export.local_host
                || local_host.is_unspecified()
                || local_host.is_multicast()
            {
                return Err(ConfigError::Invalid(format!(
                    "client.local_service_exports[{i}].local_host is not a usable literal target"
                )));
            }
            if let LocalServiceSourcePolicyConfig::Only { peers } = &export.source_policy {
                if peers.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "client.local_service_exports[{i}].source_policy.only requires at least one Peer"
                    )));
                }
                for (peer_index, peer) in peers.iter().enumerate() {
                    if peer.trim() != peer || peer.is_empty() || peer == "*" {
                        return Err(ConfigError::Invalid(format!(
                            "client.local_service_exports[{i}].source_policy.only.peers[{peer_index}] must be one explicit Peer"
                        )));
                    }
                }
            }
            let key = (export.route_kind, export.protocol, export.ingress_port);
            if !local_export_keys.insert(key) {
                return Err(ConfigError::Invalid(format!(
                    "client.local_service_exports[{i}] duplicates route_kind/protocol/ingress_port"
                )));
            }
        }
        Ok(())
    }
}

fn validate_transport_type(field: &str, raw: &str) -> Result<(), ConfigError> {
    match raw {
        "" | TRANSPORT_TYPE_QUIC | TRANSPORT_TYPE_WEBSOCKET | TRANSPORT_TYPE_GRPC => Ok(()),
        other => Err(ConfigError::Invalid(format!(
            "{field}: expected quic|websocket|grpc, got {other:?}"
        ))),
    }
}

fn parse_socket_addr(field: &str, raw: &str) -> Result<std::net::SocketAddr, ConfigError> {
    normalize_listen_addr(raw)
        .parse::<std::net::SocketAddr>()
        .map_err(|e| {
            ConfigError::Invalid(format!(
                "{field}: expected host:port socket address, got {raw:?} ({e})"
            ))
        })
}

fn validate_cors_allowed_origins(field: &str, origins: &[String]) -> Result<(), ConfigError> {
    for (idx, origin) in origins.iter().enumerate() {
        validate_cors_origin(&format!("{field}[{idx}]"), origin)?;
    }
    Ok(())
}

fn validate_cors_origin(field: &str, raw: &str) -> Result<(), ConfigError> {
    let origin = raw.trim();
    if origin.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "{field}: origin must not be empty"
        )));
    }
    if origin == "*" {
        return Err(ConfigError::Invalid(format!(
            "{field}: wildcard origin is not allowed"
        )));
    }
    if origin.eq_ignore_ascii_case("null") {
        return Err(ConfigError::Invalid(format!(
            "{field}: null origin is not allowed"
        )));
    }
    if !origin.is_ascii()
        || origin
            .bytes()
            .any(|byte| byte <= 0x1f || byte == 0x7f || byte == b',')
    {
        return Err(ConfigError::Invalid(format!(
            "{field}: expected one ASCII origin header value"
        )));
    }
    Ok(())
}

fn has_explicit_port(raw: &str) -> bool {
    let rest = raw
        .split_once("://")
        .map_or(raw, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    if let Some(bracketed) = rest.strip_prefix('[') {
        let Some((_, tail)) = bracketed.split_once(']') else {
            return false;
        };
        return tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .is_some_and(|p| p > 0);
    }
    rest.matches(':').count() == 1
        && rest
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .is_some_and(|p| p > 0)
}

/// Accept the shorthand `":port"` (bind all interfaces) by normalizing it to
/// `0.0.0.0:port`.
pub fn normalize_listen_addr(raw: &str) -> String {
    if let Some(port) = raw.strip_prefix(':') {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return format!("0.0.0.0:{port}");
        }
    }
    raw.to_string()
}

fn check_tls_pair(prefix: &str, cert: Option<&str>, key: Option<&str>) -> Result<(), ConfigError> {
    match (cert, key) {
        (Some(c), Some(k)) if !c.is_empty() && !k.is_empty() => Ok(()),
        (None, None) => Ok(()),
        (Some(c), Some(k)) if c.is_empty() && k.is_empty() => Ok(()),
        _ => Err(ConfigError::Invalid(format!(
            "{prefix}.tls_cert and {prefix}.tls_key must both be set or both unset"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_local_service_exports_are_additive_and_explicit() {
        let config = load_from_str(
            r#"
client:
  gateway:
    addr: 127.0.0.1:8443
  local_service_exports:
    - route_kind: overlay
      protocol: tcp
      ingress_port: 27015
      source_policy:
        type: any_tunnel_peer
      local_host: 127.0.0.1
      local_port: 37015
"#,
        )
        .expect("parse explicit local service export");
        let client = config.client.expect("client config");

        assert_eq!(client.local_service_exports.len(), 1);
        assert_eq!(
            client.local_service_exports[0].route_kind,
            LocalServiceRouteKindConfig::Overlay,
        );
        assert_eq!(
            client.local_service_exports[0].source_policy,
            LocalServiceSourcePolicyConfig::AnyTunnelPeer,
        );
    }

    #[test]
    fn valid_gateway_only() {
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "0.0.0.0:8443".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn invalid_log_level() {
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "0.0.0.0:8443".into(),
                ..Default::default()
            }),
            log: LogConfig {
                level: "loud".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn tls_pair_must_match() {
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "0.0.0.0:8443".into(),
                tls_cert: Some("/c".into()),
                tls_key: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn client_addr_required() {
        let cfg = Config {
            client: Some(ClientConfig {
                gateway: ClientGatewayConfig {
                    addr: "".into(),
                    ..Default::default()
                },
                replicas: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn neither_section_rejected() {
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_bare_port_shorthand() {
        // `listen_addr: ":8443"` binds 0.0.0.0:8443.
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: ":8443".into(),
                proxy: ProxyConfig {
                    socks5: Some(Socks5ProxyConfig {
                        listen_addr: ":1080".into(),
                    }),
                    http: Some(HttpProxyConfig {
                        listen_addr: ":8080".into(),
                        tls_cert: None,
                        tls_key: None,
                        auth_enabled: true,
                    }),
                    tuic: None,
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        cfg.validate().unwrap();
        assert_eq!(normalize_listen_addr(":8443"), "0.0.0.0:8443");
        assert_eq!(normalize_listen_addr("127.0.0.1:18443"), "127.0.0.1:18443");
        // Reject non-numeric or empty port — caller sees the original string in the error.
        assert!(":abc".parse::<std::net::SocketAddr>().is_err());
        assert_eq!(normalize_listen_addr(":abc"), ":abc");
    }

    #[test]
    fn gateway_fixed_transport_auth_requires_complete_pair() {
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "127.0.0.1:8443".into(),
                auth_username: "dev-tunnel".into(),
                auth_password: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("gateway.auth_username"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn transport_type_defaults_to_quic_when_missing() {
        let cfg = load_from_str(
            r#"
log:
  level: "info"
  format: "text"
  output: "stdout"
gateway:
  listen_addr: "127.0.0.1:8443"
client:
  id: "client-1"
  group_id: "group-1"
  gateway:
    addr: "gateway.example.com:8443"
"#,
        )
        .expect("parse");
        assert_eq!(
            cfg.gateway.as_ref().unwrap().transport_type,
            TRANSPORT_TYPE_QUIC
        );
        assert_eq!(
            cfg.client.as_ref().unwrap().gateway.transport_type,
            TRANSPORT_TYPE_QUIC
        );
        cfg.validate().expect("valid default transport type");
    }

    #[test]
    fn gateway_mapping_probe_port_defaults_near_transport_port() {
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: "127.0.0.1:8443"
"#,
        )
        .expect("parse");
        let gateway = cfg.gateway.unwrap();
        assert_eq!(
            gateway.mapping_probe_port,
            DEFAULT_GATEWAY_MAPPING_PROBE_PORT
        );
        gateway
            .validate()
            .expect("valid default mapping probe port");
    }

    #[test]
    fn gateway_v2_scope_and_attachment_limits_deserialize() {
        let raw = r#"
gateway:
  listen_addr: "127.0.0.1:8443"
  mapping_probe_port: 8444
  tls_cert: "certs/server.crt"
  tls_key: "certs/server.key"
  scopes_dir: "/etc/lantunnel/scopes.d"
  p2p:
    max_v2_attachments: 512
    max_replicas_per_peer: 4
"#;
        let config: Config = serde_yaml::from_str(raw).expect("parse config");
        let gateway = config.gateway.expect("gateway");
        assert_eq!(
            gateway.scopes_dir.as_deref(),
            Some("/etc/lantunnel/scopes.d")
        );
        assert_eq!(gateway.p2p.max_v2_attachments, 512);
        assert_eq!(gateway.p2p.max_replicas_per_peer, 4);
        gateway.validate().expect("valid V2 Gateway config");
    }

    #[test]
    fn gateway_static_v2_scopes_require_persistent_tls_identity() {
        let config: Config = serde_yaml::from_str(
            r#"
gateway:
  listen_addr: "127.0.0.1:8443"
  mapping_probe_port: 8444
  scopes_dir: "/etc/lantunnel/scopes.d"
"#,
        )
        .expect("parse config");

        let error = config
            .gateway
            .expect("gateway")
            .validate()
            .expect_err("Static V2 admission must not use an ephemeral dev certificate");
        assert!(error
            .to_string()
            .contains("gateway.tls_cert/tls_key required when gateway.scopes_dir is configured"));
    }

    #[test]
    fn gateway_usage_ledger_deserializes() {
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: ":8443"
  usage_ledger:
    wal_path: "/root/lantunnel/state/relay-usage.wal"
"#,
        )
        .expect("config parses");
        let gateway = cfg.gateway.expect("gateway config");
        assert_eq!(
            gateway.usage_ledger.wal_path,
            "/root/lantunnel/state/relay-usage.wal"
        );
        gateway.validate().expect("valid usage ledger config");
    }

    #[test]
    fn gateway_usage_ledger_defaults_to_state_wal() {
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: ":8443"
"#,
        )
        .expect("config parses");
        let gateway = cfg.gateway.expect("gateway config");
        assert_eq!(gateway.usage_ledger.wal_path, "state/relay-usage.wal");
    }

    #[test]
    fn gateway_mapping_probe_port_explicit_value_parse() {
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: "127.0.0.1:8443"
  mapping_probe_port: 18444
"#,
        )
        .expect("parse");
        let gateway = cfg.gateway.unwrap();
        assert_eq!(gateway.mapping_probe_port, 18444);
        gateway
            .validate()
            .expect("valid explicit mapping probe port");
    }

    #[test]
    fn gateway_mapping_probe_port_cannot_be_zero_or_reused() {
        let zero = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "127.0.0.1:8443".into(),
                mapping_probe_port: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = zero.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(msg) if msg.contains("mapping_probe_port")));

        let reused_transport = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "127.0.0.1:8443".into(),
                mapping_probe_port: 8443,
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = reused_transport.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(msg) if msg.contains("gateway.listen_addr")));

        let reused_tuic = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "127.0.0.1:8443".into(),
                mapping_probe_port: 9443,
                proxy: ProxyConfig {
                    tuic: Some(TuicProxyConfig {
                        listen_addr: "127.0.0.1:9443".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = reused_tuic.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(msg) if msg.contains("gateway.proxy.tuic.listen_addr"))
        );
    }

    #[test]
    fn gateway_mapping_udp_port_may_share_its_number_with_tcp_transports() {
        for transport_type in [TRANSPORT_TYPE_WEBSOCKET, TRANSPORT_TYPE_GRPC] {
            let config = GatewayConfig {
                listen_addr: "127.0.0.1:8444".into(),
                transport_type: transport_type.into(),
                mapping_probe_port: DEFAULT_GATEWAY_MAPPING_PROBE_PORT,
                ..Default::default()
            };

            config
                .validate()
                .unwrap_or_else(|error| panic!("{transport_type} must share 8444/tcp: {error}"));
        }
    }

    #[test]
    fn accepts_go_compatible_transport_types() {
        for transport_type in [
            TRANSPORT_TYPE_QUIC,
            TRANSPORT_TYPE_WEBSOCKET,
            TRANSPORT_TYPE_GRPC,
        ] {
            let cfg = Config {
                gateway: Some(GatewayConfig {
                    listen_addr: "127.0.0.1:8443".into(),
                    transport_type: transport_type.into(),
                    ..Default::default()
                }),
                client: Some(ClientConfig {
                    gateway: ClientGatewayConfig {
                        addr: "gateway.example.com:8443".into(),
                        transport_type: transport_type.into(),
                        ..Default::default()
                    },
                    replicas: 1,
                    ..Default::default()
                }),
                ..Default::default()
            };
            cfg.validate().expect("valid transport type");
        }
    }

    #[test]
    fn client_gateway_addr_accepts_url_with_explicit_port() {
        let cfg = Config {
            client: Some(ClientConfig {
                gateway: ClientGatewayConfig {
                    addr: "wss://gateway.example.com:8443/ws".into(),
                    transport_type: TRANSPORT_TYPE_WEBSOCKET.into(),
                    ..Default::default()
                },
                replicas: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        cfg.validate().expect("valid websocket URL");
    }

    #[test]
    fn rejects_unknown_transport_type() {
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "127.0.0.1:8443".into(),
                transport_type: "tcp".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(msg) if msg.contains("transport_type")));
    }

    #[test]
    fn rejects_tuic_native_no_fragment_payload_over_u16() {
        let cfg = Config {
            gateway: Some(GatewayConfig {
                listen_addr: "127.0.0.1:8443".into(),
                proxy: ProxyConfig {
                    tuic: Some(TuicProxyConfig {
                        listen_addr: "127.0.0.1:9443".into(),
                        native_no_fragment_max_payload: u16::MAX as usize + 1,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid(msg) if msg.contains("native_no_fragment_max_payload"))
        );
    }

    #[test]
    fn p2p_section_missing_yields_defaults() {
        // Existing deployments without [p2p] continue working unchanged.
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: "127.0.0.1:8443"
"#,
        )
        .expect("parse");
        let p2p = cfg.gateway.unwrap().p2p;
        assert!(p2p.enabled);
        assert_eq!(p2p.peer_idle_secs, 120);
        assert_eq!(p2p.session_idle_secs, 60);
        assert_eq!(p2p.punch_sync_offset_ms, 250);
    }

    #[test]
    fn gateway_transport_defaults_are_tolerant_for_file_management() {
        let gw = GatewayConfig::default();
        assert_eq!(gw.transport.congestion, "bbr");
        assert_eq!(gw.transport.keep_alive_secs, 10);
        assert_eq!(gw.transport.max_idle_secs, 60);
    }

    #[test]
    fn p2p_section_values_parse() {
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: "127.0.0.1:8443"
  p2p:
    enabled: false
    peer_idle_secs: 120
    session_idle_secs: 45
    punch_sync_offset_ms: 500
"#,
        )
        .expect("parse");
        let p2p = cfg.gateway.unwrap().p2p;
        assert!(!p2p.enabled);
        assert_eq!(p2p.peer_idle_secs, 120);
        assert_eq!(p2p.session_idle_secs, 45);
        assert_eq!(p2p.punch_sync_offset_ms, 500);
    }

    #[test]
    fn client_p2p_config_loads_current_tuning_defaults() {
        let cfg = load_from_str(
            r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
"#,
        )
        .expect("parse");
        let p2p = &cfg.client.unwrap().p2p;
        assert_eq!(p2p.attempt_after_relay_uptime_secs, 30);
        assert_eq!(p2p.cooldown_initial_secs, 60);
        assert_eq!(p2p.cooldown_max_secs, 600);
        assert_eq!(p2p.scheduler_p2p_min_advantage, 1.2);
        assert_eq!(p2p.scheduler_stable_cycles, 3);
        assert_eq!(p2p.nat_burst_count, 30);
        assert_eq!(p2p.nat_port_offsets, vec![0, 1, 2, 5, -1]);
        assert!(!p2p.allow_lan_route_aliases);
    }

    #[test]
    fn client_config_rejects_removed_role_and_shared_secret_fields() {
        for removed_field in ["role: app", "group_password: secret"] {
            let yaml = format!(
                r#"
client:
  group_id: g1
  {removed_field}
  gateway:
    addr: "127.0.0.1:8443"
"#,
            );
            let error = load_from_str(&yaml).expect_err("removed Client field must be rejected");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }

        for removed_field in ["auth_username: client", "auth_password: secret"] {
            let yaml = format!(
                r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
    {removed_field}
"#,
            );
            let error =
                load_from_str(&yaml).expect_err("removed Gateway credential must be rejected");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn client_p2p_config_rejects_removed_toggle_and_manual_target() {
        for removed_field in ["enabled: true", "peer_client_id: pc-client"] {
            let yaml = format!(
                r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
  p2p:
    {removed_field}
"#,
            );
            let error = load_from_str(&yaml).expect_err("removed P2P field must be rejected");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn client_local_proxy_defaults_parse() {
        let cfg = load_from_str(
            r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
"#,
        )
        .expect("parse");
        let client = cfg.client.unwrap();
        assert!(!client.local_proxy.enabled);
        assert_eq!(client.local_proxy.socks5_listen, "127.0.0.1:1080");
    }

    #[test]
    fn client_local_proxy_and_p2p_tuning_explicit_values_parse() {
        let cfg = load_from_str(
            r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
  local_proxy:
    enabled: true
    socks5_listen: "127.0.0.1:18080"
  p2p:
    cooldown_initial_secs: 120
"#,
        )
        .expect("parse");
        let client = cfg.client.unwrap();
        assert!(client.local_proxy.enabled);
        assert_eq!(client.local_proxy.socks5_listen, "127.0.0.1:18080");
        assert_eq!(client.p2p.cooldown_initial_secs, 120);
    }

    #[test]
    fn client_p2p_config_explicit_values_parse() {
        // Partial section: explicit fields override, omitted fields keep
        // their defaults via `#[serde(default)]` on `ClientP2pConfig`.
        let cfg = load_from_str(
            r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
  p2p:
    cooldown_initial_secs: 120
    nat_port_offsets: [0, 3, -2]
"#,
        )
        .expect("parse");
        let p2p = &cfg.client.unwrap().p2p;
        assert_eq!(p2p.cooldown_initial_secs, 120);
        assert_eq!(p2p.nat_port_offsets, vec![0, 3, -2]);
        // Unspecified fields keep their defaults.
        assert_eq!(p2p.cooldown_max_secs, 600);
        assert_eq!(p2p.scheduler_stable_cycles, 3);
    }

    #[test]
    fn client_p2p_config_explicitly_enables_lan_route_aliases() {
        let cfg = load_from_str(
            r#"
client:
  group_id: g1
  gateway:
    addr: "127.0.0.1:8443"
  p2p:
    allow_lan_route_aliases: true
"#,
        )
        .expect("parse");

        assert!(cfg.client.unwrap().p2p.allow_lan_route_aliases);
    }

    #[test]
    fn tuic_native_no_fragment_defaults_to_moonlight_lan_packet_size() {
        let cfg = load_from_str(
            r#"
gateway:
  listen_addr: "127.0.0.1:8443"
  proxy:
    tuic:
      listen_addr: "127.0.0.1:9443"
"#,
        )
        .expect("parse");
        let tuic = cfg.gateway.unwrap().proxy.tuic.unwrap();
        assert_eq!(
            tuic.native_no_fragment_max_payload,
            DEFAULT_TUIC_NATIVE_NO_FRAGMENT_MAX_PAYLOAD
        );
    }
}
