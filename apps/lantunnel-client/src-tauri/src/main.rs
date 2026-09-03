// Hide console on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::{Seek, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lantunnel_client::client_settings_v2::{
    compile_client_settings_v2, compile_client_settings_v2_with_connected_lans, ClientSettingsV2,
    CompiledClientSettingsV2,
};
use lantunnel_client::client_ui_status::{
    project_client_settings, project_client_status_read_model, project_engine_runtime_snapshot,
    project_native_routing, ClientSettingsFactsV2, ClientSettingsReadModelV2,
    ClientStatusReadModelV2, ClientUiStatusV2, LocalExportStatusV2, NativeRoutingApplyResultV2,
};
use lantunnel_client::peer_store::{
    import_peer_profile as import_peer_profile_file,
    list_peer_profiles as list_peer_profiles_from_store, load_peer_profile,
    replace_private_json_file, ImportedPeerSummaryV2,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_shell::ShellExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tp_client::status::{ConnectionPathMode, ConnectionStatus, StatusListener};
use tp_client::{Engine, EngineConfig};
use tp_core::config::ClientP2pConfig;
use tp_core::provisioning::{GatewayBootstrapV2, PeerProfileV2};
use tp_ipc::{EventBroadcaster, IpcClient, IpcHandler, Method};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter};

mod desktop_routes;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod desktop_tun;
#[cfg(target_os = "macos")]
mod macos_tun_helper;

use desktop_routes::{validate_lan_routes, DesktopNetworkMode};
use desktop_tun::{DesktopTunConfig, DesktopTunSocks5Auth, DesktopTunTask};

const LOG_BUFFER_MAX: usize = 1_000;
const LOG_FILE_PREFIX: &str = "client";
const LOG_FILE_SUFFIX: &str = "log";
const DEFAULT_PLATFORM_URL: &str = "https://lantunnel.app";
const DEFAULT_LOCAL_SOCKS5_LISTEN: &str = "127.0.0.1:1080";
const APP_CONFIG_DIR_ENV: &str = "TUNNEL_PROXY_APP_CONFIG_DIR";
const ENABLE_LAN_P2P_ENV: &str = "ENABLE_LAN_P2P";
const LOCAL_SOCKS5_LISTEN_ENV: &str = "LANTUNNEL_LOCAL_SOCKS5_LISTEN";
const DESKTOP_NETWORK_MODE_ENV: &str = "LANTUNNEL_DESKTOP_NETWORK_MODE";
const LAN_ROUTES_ENV: &str = "LANTUNNEL_LAN_ROUTES";
const DYNAMIC_ROUTE_SYNC_INTERVAL: Duration = Duration::from_millis(250);
const INSTANCE_TAKEOVER_GRACE: Duration = Duration::from_secs(5);
const INSTANCE_TAKEOVER_POLL: Duration = Duration::from_millis(100);
const REPLACE_INSTANCE_BUTTON: &str = "Close old instance and start";
const CANCEL_REPLACE_INSTANCE_BUTTON: &str = "Cancel";
#[cfg(target_os = "macos")]
const MACOS_TUN_HELPER_REQUIRED_MESSAGE: &str =
    "TUN mode requires the macOS helper. Use Local SOCKS5 for this release.";
const PUBLIC_HELP: &str = r#"Lantunnel Client 2.0

Running without a command opens the Lantunnel Client UI.

Usage:
  lantunnel-client
  lantunnel-client [OPTIONS]
  lantunnel-client connect <Tunnel ID>
  lantunnel-client disconnect
  lantunnel-client status --json
  lantunnel-client tunnel import <FILE.peer>
  lantunnel-client tunnel list

Commands:
  connect <Tunnel ID>            Connect an imported Peer using the same Client runtime without the UI
  disconnect                     Disconnect the running Client
  status --json                  Print the running Client status as JSON
  tunnel import <FILE.peer>      Import one Peer profile
  tunnel list                    List imported Peer profiles

Options:
  --headless, --no-ui            Run the same Client runtime without the UI; uses the Auto-connect Peer
  --log-level <LEVEL>            Set the Client log level
  --local-socks5-listen <ADDR>   Override the loopback Local SOCKS5 listener
  --desktop-network-mode <MODE>  Use socks5_only or lan_routes_tun
  --lan-route <CIDR>             Add one LAN route (repeatable)
  --enable-lan-p2p               Allow LAN Link Candidates
  -V, --version                  Print version
  -h, --help                     Print help
"#;

#[derive(Debug, Clone, Copy)]
struct ClipboardCommand {
    program: &'static str,
    args: &'static [&'static str],
}

// ----- serialized data file shapes -----------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProductKind;

impl ProductKind {
    fn binary_name(self) -> &'static str {
        "lantunnel-client"
    }

    /// The name the product carries on screen.
    ///
    /// Not the bundle name: `productName` stays "Lantunnel Client" so the
    /// installed `.app`, the DMG volume and the Windows exe keep their paths.
    /// The macOS helper authorises exactly one client path, and an upgrade that
    /// renamed the bundle would leave the old app beside the new one.
    fn display_name(self) -> &'static str {
        "Lantunnel"
    }

    fn default_settings(self) -> AppSettings {
        AppSettings {
            local_socks5_listen: DEFAULT_LOCAL_SOCKS5_LISTEN.into(),
            ..AppSettings::default()
        }
    }

    fn role_label(self) -> &'static str {
        "peer"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProductInfo {
    binary_name: String,
    display_name: String,
    role: String,
    version: String,
}

#[derive(Debug, Clone, Serialize)]
struct DesktopTunCapability {
    supported: bool,
    helper_required: bool,
    helper_installed: bool,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartupCommand {
    ImportPeer(PathBuf),
    ListPeers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunningInstanceCommand {
    Disconnect,
    StatusJson,
}

#[derive(Debug, Clone, Serialize)]
struct PublicRunningStatusV2 {
    connected: bool,
    connecting: bool,
    uptime_secs: u64,
    path_mode: ConnectionPathMode,
    client_ui: ClientUiStatusV2,
}

impl From<ClientStatusReadModelV2> for PublicRunningStatusV2 {
    fn from(status: ClientStatusReadModelV2) -> Self {
        Self {
            connected: status.connection.connected,
            connecting: status.connection.connecting,
            uptime_secs: status.connection.uptime_secs,
            path_mode: status.connection.path_mode,
            client_ui: status.client_ui,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum StartupCommandOutput {
    Peer(ImportedPeerSummaryV2),
    Peers(Vec<ImportedPeerSummaryV2>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StartupArgs {
    command: Option<StartupCommand>,
    instance_command: Option<RunningInstanceCommand>,
    peer_tunnel_id: Option<String>,
    show_help: bool,
    show_version: bool,
    no_ui: bool,
    log_level: Option<String>,
    local_socks5_listen: Option<String>,
    desktop_network_mode: Option<DesktopNetworkMode>,
    lan_routes: Option<Vec<String>>,
    p2p_allow_lan_candidates: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
struct StartupEnv<'a> {
    lan_p2p: Option<&'a str>,
    local_socks5_listen: Option<&'a str>,
    desktop_network_mode: Option<&'a str>,
    lan_routes: Option<&'a str>,
}

impl StartupArgs {
    fn parse_from<I, S>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let lan_p2p_env = std::env::var(ENABLE_LAN_P2P_ENV).ok();
        let local_socks5_listen_env = std::env::var(LOCAL_SOCKS5_LISTEN_ENV).ok();
        let desktop_network_mode_env = std::env::var(DESKTOP_NETWORK_MODE_ENV).ok();
        let lan_routes_env = std::env::var(LAN_ROUTES_ENV).ok();
        Self::parse_from_with_env(
            args,
            StartupEnv {
                lan_p2p: lan_p2p_env.as_deref(),
                local_socks5_listen: local_socks5_listen_env.as_deref(),
                desktop_network_mode: desktop_network_mode_env.as_deref(),
                lan_routes: lan_routes_env.as_deref(),
            },
        )
    }

    #[cfg(test)]
    fn parse_from_with_lan_p2p_env<I, S>(args: I, lan_p2p_env: Option<&str>) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::parse_from_with_env(
            args,
            StartupEnv {
                lan_p2p: lan_p2p_env,
                ..StartupEnv::default()
            },
        )
    }

    fn parse_from_with_env<I, S>(args: I, env: StartupEnv<'_>) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
            return Ok(StartupArgs {
                show_help: true,
                ..StartupArgs::default()
            });
        }
        if args.len() == 2 && matches!(args[1].as_str(), "--version" | "-V") {
            return Ok(StartupArgs {
                show_version: true,
                ..StartupArgs::default()
            });
        }

        let mut out = StartupArgs::default();
        if let Some(raw) = env.lan_p2p.map(str::trim).filter(|value| !value.is_empty()) {
            out.p2p_allow_lan_candidates = Some(parse_bool_arg(ENABLE_LAN_P2P_ENV, raw)?);
        }
        if let Some(raw) = env
            .local_socks5_listen
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            out.local_socks5_listen = Some(normalize_local_socks5_listen(raw)?);
        }
        if let Some(raw) = env
            .desktop_network_mode
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            out.desktop_network_mode = Some(parse_desktop_network_mode_arg(
                DESKTOP_NETWORK_MODE_ENV,
                raw,
            )?);
        }
        if let Some(raw) = env
            .lan_routes
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            out.lan_routes = Some(parse_lan_routes_arg(LAN_ROUTES_ENV, raw)?);
        }
        let mut iter = args.into_iter().peekable();
        let _program = iter.next();
        if iter.peek().map(String::as_str) == Some("disconnect") {
            iter.next();
            if iter.next().is_some() {
                return Err("disconnect accepts no arguments".into());
            }
            out.instance_command = Some(RunningInstanceCommand::Disconnect);
            out.no_ui = true;
            return Ok(out);
        }
        if iter.peek().map(String::as_str) == Some("status") {
            iter.next();
            if iter.next().as_deref() != Some("--json") || iter.next().is_some() {
                return Err("status requires exactly --json".into());
            }
            out.instance_command = Some(RunningInstanceCommand::StatusJson);
            out.no_ui = true;
            return Ok(out);
        }
        if iter.peek().map(String::as_str) == Some("connect") {
            iter.next();
            let tunnel_id = iter
                .next()
                .ok_or_else(|| "connect requires an imported Tunnel ID".to_string())?;
            if tunnel_id.trim().is_empty() || iter.next().is_some() {
                return Err("connect accepts exactly one imported Tunnel ID".into());
            }
            out.peer_tunnel_id = Some(tunnel_id);
            out.no_ui = true;
            return Ok(out);
        }
        if iter.peek().map(String::as_str) == Some("tunnel") {
            iter.next();
            out.command = Some(match iter.next().as_deref() {
                Some("import") => {
                    let path = iter
                        .next()
                        .ok_or_else(|| "tunnel import requires a .peer file".to_string())?;
                    if iter.next().is_some() {
                        return Err("tunnel import accepts exactly one .peer file".into());
                    }
                    StartupCommand::ImportPeer(PathBuf::from(path))
                }
                Some("list") => {
                    if iter.next().is_some() {
                        return Err("tunnel list accepts no arguments".into());
                    }
                    StartupCommand::ListPeers
                }
                _ => {
                    return Err("expected `tunnel import <file>` or `tunnel list`".into());
                }
            });
            out.no_ui = true;
            return Ok(out);
        }
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--log-level" => {
                    out.log_level = Some(
                        iter.next()
                            .ok_or_else(|| "--log-level requires a value".to_string())?,
                    );
                }
                "--local-socks5-listen" | "--socks5-listen" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--local-socks5-listen requires a value".to_string())?;
                    out.local_socks5_listen = Some(normalize_local_socks5_listen(&value)?);
                }
                "--desktop-network-mode" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--desktop-network-mode requires a value".to_string())?;
                    out.desktop_network_mode = Some(parse_desktop_network_mode_arg(
                        "--desktop-network-mode",
                        &value,
                    )?);
                }
                "--enable-desktop-tun" => {
                    out.desktop_network_mode = Some(DesktopNetworkMode::LanRoutesTun);
                }
                "--disable-desktop-tun" | "--proxy-only" => {
                    out.desktop_network_mode = Some(DesktopNetworkMode::Socks5Only);
                }
                "--lan-route" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--lan-route requires a CIDR value".to_string())?;
                    let mut routes = out.lan_routes.take().unwrap_or_default();
                    routes.push(value);
                    out.lan_routes = Some(
                        validate_lan_routes(&routes, routes.len())
                            .map_err(|e| format!("--lan-route invalid: {e}"))?
                            .routes,
                    );
                }
                "--enable-lan-p2p" => out.p2p_allow_lan_candidates = Some(true),
                "--version" | "-V" => out.show_version = true,
                "--no-ui" | "--headless" => out.no_ui = true,
                other if other.starts_with("-psn_") => {}
                other if other.starts_with("--") => {
                    return Err(format!("unknown argument {other:?}"));
                }
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(out)
    }

    fn validate_no_ui(&self) -> Result<(), String> {
        if !self.no_ui {
            return Ok(());
        }
        if self.command.is_some() {
            return Ok(());
        }
        if self.instance_command.is_some() {
            return Ok(());
        }
        if self.peer_tunnel_id.is_some() {
            return Ok(());
        }
        // Plain --headless may reuse the last explicitly selected imported
        // Peer when Auto-connect is enabled; resolution happens after settings
        // and the public selection hint have been loaded.
        Ok(())
    }

    fn early_exit_output(&self, product: ProductKind) -> Option<String> {
        if self.show_help {
            Some(PUBLIC_HELP.to_string())
        } else {
            self.show_version
                .then(|| format!("{} {}", product.binary_name(), env!("CARGO_PKG_VERSION")))
        }
    }
}

fn run_startup_command(
    command: &StartupCommand,
    product: ProductKind,
) -> anyhow::Result<StartupCommandOutput> {
    match command {
        StartupCommand::ImportPeer(path) => import_peer_profile_file(path, &config_dir(product))
            .map(StartupCommandOutput::Peer)
            .map_err(Into::into),
        StartupCommand::ListPeers => list_peer_profiles_from_store(&config_dir(product))
            .map(StartupCommandOutput::Peers)
            .map_err(Into::into),
    }
}

fn parse_bool_arg(name: &str, raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Ok(true),
        "0" | "false" | "no" | "off" | "disabled" => Ok(false),
        _ => Err(format!("{name} expects true/false, got {raw:?}")),
    }
}

fn parse_desktop_network_mode_arg(name: &str, raw: &str) -> Result<DesktopNetworkMode, String> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "socks5_only" | "socks5" | "proxy" | "proxy_only" => Ok(DesktopNetworkMode::Socks5Only),
        "lan_routes_tun" | "tun" | "tun_on" | "on" => Ok(DesktopNetworkMode::LanRoutesTun),
        _ => Err(format!(
            "{name} expects socks5_only or lan_routes_tun, got {raw:?}"
        )),
    }
}

fn parse_lan_routes_arg(name: &str, raw: &str) -> Result<Vec<String>, String> {
    let routes: Vec<String> = raw
        .split([',', ';', '\n', '\r'])
        .map(str::trim)
        .filter(|route| !route.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    validate_lan_routes(&routes, routes.len())
        .map(|validated| validated.routes)
        .map_err(|e| format!("{name} invalid: {e}"))
}

fn parse_local_socks5_listen(raw: &str) -> Result<SocketAddr, String> {
    let listen = raw.trim();
    listen
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid local_socks5_listen {listen:?}: {e}"))
}

fn normalize_local_socks5_listen(raw: &str) -> Result<String, String> {
    Ok(parse_local_socks5_listen(raw)?.to_string())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LastPeerSelection {
    #[serde(default)]
    tunnel_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InstallIdentity {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    device_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DenyUnknownSettingsFields {}

impl<'de> Deserialize<'de> for DenyUnknownSettingsFields {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DenyUnknownSettingsFields;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("no unknown Client settings fields")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // The read model hands the UI these alongside the settings, so
                // they come back on every save. Refusing them meant no setting
                // could be changed at all; refusing anything else still catches
                // a field name that does not exist.
                const PROJECTED_BY_THE_READ_MODEL: [&str; 3] =
                    ["client_ui", "exported_lan_statuses", "v2_settings_rejected"];

                while let Some(field) = map.next_key::<String>()? {
                    if !PROJECTED_BY_THE_READ_MODEL.contains(&field.as_str()) {
                        return Err(serde::de::Error::custom(format!(
                            "unknown Client settings field `{field}`"
                        )));
                    }
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
                Ok(DenyUnknownSettingsFields {})
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

/// Bumped when a saved file needs rewriting because a setting changed meaning.
/// Version 1 split native routing out of Tunnel First.
const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    auto_start: bool,
    auto_connect: bool,
    local_socks5_listen: String,
    /// Field-level default, so a file written before this field existed reads
    /// as 0 rather than as the current version. Absence is what marks a file
    /// as needing migration.
    #[serde(default)]
    settings_version: u32,
    #[serde(default = "default_true")]
    local_proxy_enabled: bool,
    desktop_network_mode: DesktopNetworkMode,
    lan_routes: Vec<String>,
    p2p_allow_lan_candidates: bool,
    local_service_exports: Vec<tp_core::config::LocalServiceExportConfig>,
    log_level: String,
    /// True when a settings file existed but could not be parsed, so none of
    /// the fields above came from it.
    ///
    /// Never serialized: it is a fact about this read, not a saved setting, and
    /// it must not reach the frontend or be written back to disk.
    #[serde(skip)]
    unreadable: bool,
    #[serde(flatten)]
    v2: ClientSettingsV2,
    #[serde(flatten)]
    unknown: DenyUnknownSettingsFields,
}

#[derive(Debug, Clone, Serialize)]
struct AppSettingsReadModelV2 {
    #[serde(flatten)]
    settings: AppSettings,
    client_ui: ClientSettingsReadModelV2,
    /// The saved V2 block does not compile, so none of it is in effect.
    v2_settings_rejected: bool,
    exported_lan_statuses: Vec<LocalExportStatusV2>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_start: false,
            auto_connect: false,
            local_socks5_listen: DEFAULT_LOCAL_SOCKS5_LISTEN.into(),
            settings_version: SETTINGS_VERSION,
            local_proxy_enabled: true,
            desktop_network_mode: DesktopNetworkMode::Socks5Only,
            lan_routes: Vec::new(),
            p2p_allow_lan_candidates: false,
            local_service_exports: Vec::new(),
            log_level: "info".into(),
            unreadable: false,
            v2: ClientSettingsV2::default(),
            unknown: DenyUnknownSettingsFields::default(),
        }
    }
}

fn compile_app_v2_settings(settings: &AppSettings) -> Result<CompiledClientSettingsV2, String> {
    let connected_lans = match tp_client::discover_connected_lan_prefixes() {
        Ok(connected_lans) => Some(connected_lans),
        Err(error) => {
            tracing::debug!(%error, "connected LAN inventory unavailable; withdrawing LAN Exports");
            None
        }
    };
    compile_client_settings_v2_with_connected_lans(&settings.v2, connected_lans.as_deref())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
fn persist_validated_app_settings(
    path: &Path,
    settings: &AppSettings,
) -> Result<CompiledClientSettingsV2, String> {
    let compiled = compile_app_v2_settings(settings)?;
    write_json(path, settings).map_err(|error| error.to_string())?;
    Ok(compiled)
}

fn compiled_app_v2_settings_or_safe_default(settings: &AppSettings) -> CompiledClientSettingsV2 {
    // Not parsing and not compiling are the same failure to the engine: either
    // way the owner configured this Client and we cannot honour it. Only the
    // second used to reach this guard, so a file with one stray character was
    // silently replaced by open defaults.
    let compiled = if settings.unreadable {
        Err("saved Client settings could not be parsed".to_owned())
    } else {
        compile_app_v2_settings(settings)
    };
    match compiled {
        Ok(compiled) => compiled,
        Err(_) => {
            tracing::warn!(
                "invalid persisted V2 Client settings; using deny-all with no LAN Exports"
            );
            // Deliberately not ClientSettingsV2::default(). A Client that was
            // never configured opens to its Tunnel, but settings that fail to
            // compile are a different thing: someone did configure this Client
            // and we cannot read what they asked for. Widening access on a read
            // failure would turn a corrupt file into an access grant.
            compile_client_settings_v2(&ClientSettingsV2 {
                client_access: tp_client::access_policy::ClientAccessPolicyV2::closed(),
                exported_lans: Vec::new(),
                auto_export_current_lan: false,
                tunnel_first: false,
            })
            .expect("built-in V2 Client settings must compile")
        }
    }
}

fn install_app_v2_settings(
    engine: &Engine,
    compiled: &CompiledClientSettingsV2,
) -> Result<(), String> {
    engine.set_v2_local_lan_export_config(
        compiled.local_export_config.clone(),
        compiled.connected_lans.as_deref(),
    );
    engine
        .set_v2_access_policy(&compiled.client_access)
        .map_err(|error| error.to_string())
}

fn default_true() -> bool {
    true
}

fn merge_product_defaults(mut settings: AppSettings, product: ProductKind) -> AppSettings {
    let defaults = product.default_settings();
    if settings.local_socks5_listen.is_empty() {
        settings.local_socks5_listen = defaults.local_socks5_listen;
    }
    if settings.log_level.trim().is_empty() {
        settings.log_level = defaults.log_level;
    }
    // Tunnel First used to be ORed into `should_run_desktop_tun`, and
    // `desktop_network_mode` has never had a control in the UI. That made
    // Tunnel First the only switch on the window that could start the TUN, so
    // turning it off — a question about which of two overlapping networks wins
    // — stopped native routing outright. The two are separate switches now, and
    // a file saved before the split still says `socks5_only` while its owner
    // has been routing natively all along. Carry that across once, rather than
    // stopping their TUN on upgrade.
    //
    // Keyed on the version and not on the values, because after the split
    // `tunnel_first` may legitimately stay on while native routing is off: the
    // switch is disabled then, and keeps the owner's answer for when they turn
    // native routing back on.
    if settings.settings_version < 1 {
        if settings.v2.tunnel_first {
            settings.desktop_network_mode = DesktopNetworkMode::LanRoutesTun;
        }
        settings.settings_version = 1;
    }
    settings
}

/// Saved settings are what the owner chose, not what a Gateway reported.
///
/// This used to copy a subscription tier and a route ceiling into the saved
/// file on every save, and rewrite the route list from a tier-to-limit table.
/// The product does not advertise or enforce such a ceiling; do not
/// reintroduce one.
fn lan_route_entitlement_from_tunnel_config(_cfg: &tp_client::TunnelConfig) -> usize {
    usize::MAX
}

fn apply_startup_overrides(mut settings: AppSettings, startup: &StartupArgs) -> AppSettings {
    if let Some(log_level) = startup.log_level.as_deref() {
        settings.log_level = log_level.to_string();
    }
    if let Some(listen) = startup.local_socks5_listen.as_deref() {
        settings.local_socks5_listen = listen.to_string();
    }
    if let Some(mode) = startup.desktop_network_mode {
        settings.desktop_network_mode = mode;
    }
    if let Some(routes) = startup.lan_routes.as_ref() {
        settings.lan_routes = routes.clone();
    }
    if let Some(p2p_allow_lan_candidates) = startup.p2p_allow_lan_candidates {
        settings.p2p_allow_lan_candidates = p2p_allow_lan_candidates;
    }
    settings
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProxyStatus {
    running: bool,
    listen_addr: String,
    tun_running: bool,
    tun_routes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogConfigInfo {
    level: String,
    file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutoStartValidation {
    os_enabled: bool,
    setting_enabled: bool,
    consistent: bool,
}

// ----- paths ---------------------------------------------------------------

fn config_dir_with_override(override_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = override_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    default_config_root_dir()
}

fn default_config_root_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lantunnel")
}

fn config_dir(product: ProductKind) -> PathBuf {
    let override_dir = std::env::var(APP_CONFIG_DIR_ENV).ok();
    product_config_dir_with_override(override_dir.as_deref(), product)
}

fn product_config_dir_with_override(override_dir: Option<&str>, product: ProductKind) -> PathBuf {
    let root = config_dir_with_override(override_dir);
    if override_dir
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .is_some()
        && override_dir_is_exact_product_config_dir(&root, product)
    {
        return root;
    }
    product_config_dir_in_root(&root, product)
}

fn override_dir_is_exact_product_config_dir(dir: &Path, product: ProductKind) -> bool {
    override_dir_name_looks_product_specific(dir, product)
        || override_dir_has_existing_product_files(dir, product)
}

fn override_dir_name_looks_product_specific(dir: &Path, product: ProductKind) -> bool {
    let Some(name) = dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())
    else {
        return false;
    };
    let _ = product;
    name.contains("app") || name.contains("remote") || name.contains("client")
}

fn override_dir_has_existing_product_files(dir: &Path, product: ProductKind) -> bool {
    let _ = product;
    let common_files = ["config.json", "settings.json", "install.json"];
    common_files.iter().any(|name| dir.join(name).is_file())
        || dir.join("client-install.json").is_file()
}

fn product_config_dir_in_root(root: &Path, product: ProductKind) -> PathBuf {
    let _ = product;
    root.join("app")
}

fn last_peer_selection_path(product: ProductKind) -> PathBuf {
    last_peer_selection_path_in_dir(&config_dir(product))
}

fn last_peer_selection_path_in_dir(dir: &Path) -> PathBuf {
    dir.join("last-peer.json")
}

fn settings_path(product: ProductKind) -> PathBuf {
    settings_path_in_dir(&config_dir(product))
}

fn settings_path_in_dir(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

fn install_identity_path_in_dir(dir: &Path) -> PathBuf {
    dir.join("install.json")
}

fn load_or_create_install_identity(product: ProductKind) -> anyhow::Result<InstallIdentity> {
    load_or_create_install_identity_in_dir(&config_dir(product))
}

fn load_or_create_install_identity_in_dir(dir: &Path) -> anyhow::Result<InstallIdentity> {
    let path = install_identity_path_in_dir(dir);
    let mut identity: InstallIdentity = read_json(&path);
    let existing_device_id = identity.device_id.trim().to_string();
    if is_valid_device_id(&existing_device_id) {
        identity.device_id = existing_device_id;
    } else {
        identity.device_id = uuid::Uuid::new_v4().to_string();
    }
    identity.device_name = identity
        .device_name
        .as_deref()
        .and_then(normalize_device_name)
        .or_else(default_device_name);
    write_json(&path, &identity)?;
    Ok(identity)
}

fn is_valid_device_id(raw: &str) -> bool {
    let value = raw.trim();
    (16..=128).contains(&value.len())
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

fn default_device_name() -> Option<String> {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .and_then(|name| normalize_device_name(&name))
}

fn normalize_device_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(120).collect())
    }
}

fn single_instance_lock_path(product: ProductKind) -> PathBuf {
    config_dir(product).join(format!("{}.lock", product.binary_name()))
}

fn local_control_path(product: ProductKind) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = product;
        PathBuf::from(r"\\.\pipe\lantunnel-client-v2")
    }
    #[cfg(not(windows))]
    {
        config_dir(product).join("control.sock")
    }
}

struct SingleInstanceGuard {
    path: PathBuf,
    _file: std::fs::File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningInstance {
    lock_path: PathBuf,
    pid: Option<u32>,
}

#[derive(Debug)]
enum SingleInstanceError {
    AlreadyRunning(RunningInstance),
    Other(String),
}

impl SingleInstanceError {
    fn message(&self, product: ProductKind) -> String {
        match self {
            Self::AlreadyRunning(_) => already_running_message(product),
            Self::Other(message) => message.clone(),
        }
    }
}

impl SingleInstanceGuard {
    fn acquire(path: PathBuf, product: ProductKind) -> Result<Self, SingleInstanceError> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent).map_err(|e| {
                SingleInstanceError::Other(format!("failed to create app config dir: {e}"))
            })?;
        }
        let mut file = open_lock_file(&path, product)?;
        lock_file(&file, &path)?;
        file.set_len(0).map_err(|e| {
            SingleInstanceError::Other(format!(
                "failed to truncate app lock {}: {e}",
                path.display()
            ))
        })?;
        file.rewind().map_err(|e| {
            SingleInstanceError::Other(format!("failed to rewind app lock {}: {e}", path.display()))
        })?;
        use std::io::Write as _;
        let _ = writeln!(file, "pid={}", std::process::id());
        Ok(Self { path, _file: file })
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn already_running_message(product: ProductKind) -> String {
    format!(
        "{} is already running. Close the existing instance before starting another one.",
        product.display_name()
    )
}

fn already_running_error(path: &Path) -> SingleInstanceError {
    SingleInstanceError::AlreadyRunning(RunningInstance {
        lock_path: path.to_path_buf(),
        pid: read_lock_pid(path),
    })
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| parse_lock_pid(&contents))
}

fn parse_lock_pid(contents: &str) -> Option<u32> {
    contents.lines().find_map(|line| {
        line.trim()
            .strip_prefix("pid=")?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid != 0)
    })
}

fn replace_running_instance_message(product: ProductKind, instance: &RunningInstance) -> String {
    let mut message = format!("{} is already running.", product.display_name());
    if let Some(pid) = instance.pid {
        message.push_str(&format!("\n\nExisting process ID: {pid}."));
    }
    message.push_str("\n\nClose the existing instance and start this one?");
    message
}

#[cfg(target_os = "windows")]
fn open_lock_file(
    path: &Path,
    _product: ProductKind,
) -> Result<std::fs::File, SingleInstanceError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
                || e.kind() == std::io::ErrorKind::PermissionDenied
            {
                already_running_error(path)
            } else {
                SingleInstanceError::Other(format!(
                    "failed to open app lock {}: {e}",
                    path.display()
                ))
            }
        })
}

#[cfg(not(target_os = "windows"))]
fn open_lock_file(
    path: &Path,
    _product: ProductKind,
) -> Result<std::fs::File, SingleInstanceError> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            SingleInstanceError::Other(format!("failed to open app lock {}: {e}", path.display()))
        })
}

#[cfg(unix)]
fn lock_file(file: &std::fs::File, path: &Path) -> Result<(), SingleInstanceError> {
    use std::os::fd::AsRawFd;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
            Err(already_running_error(path))
        }
        _ => Err(SingleInstanceError::Other(format!(
            "failed to lock app lock {}: {err}",
            path.display()
        ))),
    }
}

#[cfg(target_os = "windows")]
fn lock_file(file: &std::fs::File, path: &Path) -> Result<(), SingleInstanceError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped = OVERLAPPED::default();
    let rc = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if rc != 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code)
            if code == ERROR_LOCK_VIOLATION as i32 || code == ERROR_SHARING_VIOLATION as i32 =>
        {
            Err(already_running_error(path))
        }
        _ => Err(SingleInstanceError::Other(format!(
            "failed to lock app lock {}: {err}",
            path.display()
        ))),
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
fn lock_file(_file: &std::fs::File, _path: &Path) -> Result<(), SingleInstanceError> {
    Ok(())
}

fn show_already_running_message(product: ProductKind, message: &str) {
    eprintln!("{message}");
    if std::env::var_os("TUNNEL_PROXY_SKIP_ALREADY_RUNNING_DIALOG").is_some() {
        return;
    }
    let _ = rfd::MessageDialog::new()
        .set_title(product.display_name())
        .set_description(message)
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

fn confirm_replace_running_instance(product: ProductKind, instance: &RunningInstance) -> bool {
    let message = replace_running_instance_message(product, instance);
    eprintln!("{message}");
    if std::env::var_os("TUNNEL_PROXY_SKIP_ALREADY_RUNNING_DIALOG").is_some() {
        return false;
    }

    let result = rfd::MessageDialog::new()
        .set_title(product.display_name())
        .set_description(message)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            REPLACE_INSTANCE_BUTTON.into(),
            CANCEL_REPLACE_INSTANCE_BUTTON.into(),
        ))
        .show();
    matches!(
        result,
        rfd::MessageDialogResult::Ok | rfd::MessageDialogResult::Yes
    ) || matches!(result, rfd::MessageDialogResult::Custom(label) if label == REPLACE_INSTANCE_BUTTON)
}

fn acquire_after_replacing_running_instance(
    path: PathBuf,
    product: ProductKind,
    instance: &RunningInstance,
) -> Result<SingleInstanceGuard, String> {
    terminate_running_instance(product, instance)?;
    wait_for_single_instance_lock(path, product, INSTANCE_TAKEOVER_GRACE)
}

fn wait_for_single_instance_lock(
    path: PathBuf,
    product: ProductKind,
    timeout: Duration,
) -> Result<SingleInstanceGuard, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match SingleInstanceGuard::acquire(path.clone(), product) {
            Ok(guard) => return Ok(guard),
            Err(SingleInstanceError::AlreadyRunning(_)) if Instant::now() < deadline => {
                std::thread::sleep(INSTANCE_TAKEOVER_POLL);
            }
            Err(e) => return Err(e.message(product)),
        }
    }
}

fn terminate_running_instance(
    product: ProductKind,
    instance: &RunningInstance,
) -> Result<(), String> {
    if let Some(pid) = instance.pid {
        terminate_process_id(pid)
    } else {
        terminate_product_processes(product)
    }
}

#[cfg(unix)]
fn terminate_process_id(pid: u32) -> Result<(), String> {
    if pid == std::process::id() {
        return Err("lock file points to the current process; refusing to terminate it".into());
    }

    send_signal(pid, "TERM")?;
    let deadline = Instant::now() + INSTANCE_TAKEOVER_GRACE;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(INSTANCE_TAKEOVER_POLL);
    }

    send_signal(pid, "KILL")?;
    Ok(())
}

#[cfg(unix)]
fn send_signal(pid: u32, name: &str) -> Result<(), String> {
    let status = Command::new("/bin/kill")
        .arg("-s")
        .arg(name)
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("failed to signal existing instance {pid}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to signal existing instance {pid} with {name}"
        ))
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let status = Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(status, Ok(status) if status.success())
}

#[cfg(target_os = "windows")]
fn terminate_process_id(pid: u32) -> Result<(), String> {
    if pid == std::process::id() {
        return Err("lock file points to the current process; refusing to terminate it".into());
    }

    let soft = run_taskkill(&["/PID", &pid.to_string(), "/T"])?;
    let deadline = Instant::now() + INSTANCE_TAKEOVER_GRACE;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(INSTANCE_TAKEOVER_POLL);
    }
    if windows_termination_after_soft_taskkill(&soft, process_is_alive(pid))
        == WindowsTerminationDecision::Done
    {
        return Ok(());
    }

    let force = run_taskkill(&["/PID", &pid.to_string(), "/T", "/F"])?;
    if force.success || !process_is_alive(pid) {
        Ok(())
    } else {
        Err(force.error_message("failed to terminate the existing instance"))
    }
}

#[cfg(target_os = "windows")]
fn terminate_product_processes(product: ProductKind) -> Result<(), String> {
    let current_pid = std::process::id();
    let pids: Vec<u32> = product_process_ids(product)?
        .into_iter()
        .filter(|pid| *pid != current_pid)
        .collect();
    if pids.is_empty() {
        return Err(format!(
            "could not find another {} process to close",
            product.display_name()
        ));
    }

    for pid in pids {
        terminate_process_id(pid)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn terminate_product_processes(product: ProductKind) -> Result<(), String> {
    Err(format!(
        "cannot identify the existing {} process from {}",
        product.display_name(),
        single_instance_lock_path(product).display()
    ))
}

#[cfg(target_os = "windows")]
fn run_taskkill(args: &[&str]) -> Result<TaskkillResult, String> {
    let output = Command::new("taskkill")
        .args(args)
        .output()
        .map_err(|e| format!("failed to start taskkill: {e}"))?;

    Ok(TaskkillResult {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskkillResult {
    success: bool,
    stdout: String,
    stderr: String,
}

#[cfg(any(test, target_os = "windows"))]
impl TaskkillResult {
    #[cfg(target_os = "windows")]
    fn error_message(&self, fallback: &str) -> String {
        if !self.stderr.is_empty() {
            format!("{fallback}: {}", self.stderr)
        } else if !self.stdout.is_empty() {
            format!("{fallback}: {}", self.stdout)
        } else {
            fallback.into()
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsTerminationDecision {
    Done,
    Force,
}

#[cfg(any(test, target_os = "windows"))]
fn windows_termination_after_soft_taskkill(
    _soft: &TaskkillResult,
    process_still_alive: bool,
) -> WindowsTerminationDecision {
    if process_still_alive {
        WindowsTerminationDecision::Force
    } else {
        WindowsTerminationDecision::Done
    }
}

#[cfg(target_os = "windows")]
fn process_is_alive(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let output = Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .any(|token| token == pid.to_string())
}

#[cfg(target_os = "windows")]
fn product_process_ids(product: ProductKind) -> Result<Vec<u32>, String> {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .map_err(|e| {
            format!(
                "failed to list running {} processes: {e}",
                product.display_name()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "failed to list running {} processes",
            product.display_name()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| parse_windows_product_tasklist_pid(product, line))
        .collect())
}

#[cfg(test)]
fn parse_windows_tasklist_csv_pid(line: &str) -> Option<u32> {
    let (_image_name, pid) = parse_windows_tasklist_csv_process(line)?;
    Some(pid)
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_product_tasklist_pid(product: ProductKind, line: &str) -> Option<u32> {
    let (image_name, pid) = parse_windows_tasklist_csv_process(line)?;
    if windows_tasklist_image_matches_product(product, &image_name) {
        Some(pid)
    } else {
        None
    }
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_tasklist_csv_process(line: &str) -> Option<(String, u32)> {
    let mut fields = line.trim().split("\",\"");
    let image_name = fields.next()?.trim_matches('"').trim().to_ascii_lowercase();
    let pid = fields
        .next()?
        .trim_matches('"')
        .trim()
        .parse::<u32>()
        .ok()?;
    Some((image_name, pid))
}

#[cfg(any(target_os = "windows", test))]
fn windows_tasklist_image_matches_product(product: ProductKind, image_name: &str) -> bool {
    let _ = product;
    let image_name = image_name.trim().to_ascii_lowercase();
    image_name.starts_with("lantunnel-client")
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> T {
    let value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let _ = harden_existing_config_path(path);
    value
}

/// Read saved Client settings, telling absence apart from corruption.
///
/// `read_json` cannot make that distinction: any failure becomes `Default`. For
/// most config files that is right, but settings carry the access policy, and
/// defaults open this Client to its Tunnel. A file that exists and cannot be
/// parsed means someone configured this Client and we cannot read what they
/// asked for, which is the case `compiled_app_v2_settings_or_safe_default`
/// already closes for a policy that parses but does not compile. A syntax
/// error, an unknown field, or a wrong type never reached that guard.
///
/// The parsed fields are still left at their defaults rather than substituted,
/// so nothing invented here can be written back over the owner's file.
fn read_settings_json(path: &PathBuf) -> AppSettings {
    let raw = std::fs::read_to_string(path).ok();
    let settings = match raw.as_deref().map(serde_json::from_str::<AppSettings>) {
        // No file, or not readable as text at all. Nobody configured this
        // Client, so it opens to its Tunnel like a fresh install.
        None => AppSettings::default(),
        Some(Ok(settings)) => settings,
        Some(Err(error)) => {
            tracing::warn!(
                %error,
                "saved Client settings could not be parsed; using deny-all with no LAN Exports"
            );
            AppSettings {
                unreadable: true,
                ..AppSettings::default()
            }
        }
    };
    let _ = harden_existing_config_path(path);
    settings
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    replace_private_json_file(path, value)?;
    Ok(())
}

fn harden_existing_config_path(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    if path.exists() {
        ensure_private_file(path)?;
    }
    Ok(())
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut dirs_to_harden = Vec::new();
    for ancestor in dir.ancestors() {
        if ancestor.exists() {
            break;
        }
        dirs_to_harden.push(ancestor.to_path_buf());
    }
    std::fs::create_dir_all(dir)?;
    dirs_to_harden.push(dir.to_path_buf());
    if let Some(lantunnel_dir) = dir
        .ancestors()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(".lantunnel"))
    {
        dirs_to_harden.push(lantunnel_dir.to_path_buf());
    }
    for path in dirs_to_harden {
        if path.is_dir() {
            set_owner_only_dir_permissions(&path)?;
        }
    }
    Ok(())
}

fn ensure_private_file(path: &Path) -> std::io::Result<()> {
    set_owner_only_file_permissions(path)
}

#[cfg(unix)]
fn set_owner_only_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_owner_only_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn clean_log_line(line: &str) -> String {
    mask_log_secrets(&strip_ansi_sequences(line))
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if (ch == '\u{1b}' || ch == '\u{fffd}') && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn mask_log_secrets(line: &str) -> String {
    let mut masked = line.to_string();
    for field in ["tunnel_key", "group_password"] {
        masked = mask_field_after_marker(&masked, &format!("{field}="));
        masked = mask_field_after_marker(&masked, &format!("{field} ="));
        masked = mask_field_after_marker(&masked, &format!("{field}:"));
        masked = mask_field_after_marker(&masked, &format!("\"{field}\":"));
    }
    masked
}

fn mask_field_after_marker(input: &str, marker: &str) -> String {
    let mut out = input.to_string();
    let mut search_from = 0;
    while let Some(rel_pos) = out[search_from..].find(marker) {
        let marker_start = search_from + rel_pos;
        let mut value_start = marker_start + marker.len();
        while out[value_start..].starts_with(' ') {
            value_start += 1;
        }

        let (replace_start, replace_end, replacement) = if out[value_start..].starts_with('"') {
            let content_start = value_start + 1;
            let Some(content_end) = find_closing_quote(&out, content_start) else {
                break;
            };
            (content_start, content_end, "***")
        } else {
            let value_end = out[value_start..]
                .find(|c: char| c.is_whitespace() || c == ',' || c == '}')
                .map(|idx| value_start + idx)
                .unwrap_or_else(|| out.len());
            (value_start, value_end, "***")
        };

        out.replace_range(replace_start..replace_end, replacement);
        search_from = replace_start + replacement.len();
    }
    out
}

fn find_closing_quote(input: &str, start: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, ch) in input[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            return Some(start + offset);
        }
    }
    None
}

fn local_proxy_auth_mode_from_tunnel_config(
    cfg: &tp_client::TunnelConfig,
    listen: SocketAddr,
) -> Result<tp_proxy_socks5::AuthMode, String> {
    if !listen.ip().is_loopback() {
        return Err(
            "V2 Local SOCKS5 has no shared-secret authentication and requires a loopback listener"
                .into(),
        );
    }
    Ok(tp_proxy_socks5::AuthMode::NoAuth {
        group_id: v2_local_proxy_peer_id(cfg)?,
    })
}

fn v2_local_proxy_peer_id(cfg: &tp_client::TunnelConfig) -> Result<String, String> {
    let peer_id = cfg.peer_id.trim();
    let has_v2_identity = !cfg.tunnel_id.trim().is_empty()
        && !peer_id.is_empty()
        && !cfg.overlay_ipv4.trim().is_empty()
        && cfg.group_id.is_empty();
    if !has_v2_identity {
        return Err("V2 TunnelConfig is missing a clean V2 Peer identity".into());
    }
    Ok(peer_id.to_owned())
}

fn validate_v2_local_proxy_listen(listen: Option<SocketAddr>) -> Result<(), String> {
    if listen.is_some_and(|listen| !listen.ip().is_loopback()) {
        return Err(
            "V2 Local SOCKS5 has no shared-secret authentication and requires a loopback listener"
                .into(),
        );
    }
    Ok(())
}

fn clash_overlay_yaml(
    cfg: &tp_client::TunnelConfig,
    listen_addr: SocketAddr,
    platform_url: &str,
) -> Result<String, String> {
    v2_local_proxy_peer_id(cfg)?;
    validate_v2_local_proxy_listen(Some(listen_addr))?;
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
        clash_server_host(listen_addr),
        listen_addr.port()
    ))
}

fn clash_server_host(listen_addr: SocketAddr) -> String {
    match listen_addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "::1".into(),
        ip => ip.to_string(),
    }
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

fn local_proxy_listen_addr(settings: &AppSettings) -> Result<Option<SocketAddr>, String> {
    if !settings.local_proxy_enabled {
        return Ok(None);
    }
    parse_local_socks5_listen(&settings.local_socks5_listen).map(Some)
}

fn p2p_config_from_settings(settings: &AppSettings) -> ClientP2pConfig {
    ClientP2pConfig {
        allow_lan_candidates: settings.p2p_allow_lan_candidates,
        allow_lan_route_aliases: settings.p2p_allow_lan_candidates,
        ..Default::default()
    }
}

fn p2p_settings_require_reconnect(previous: &AppSettings, next: &AppSettings) -> bool {
    previous.p2p_allow_lan_candidates != next.p2p_allow_lan_candidates
        || previous.local_service_exports != next.local_service_exports
}

fn desktop_tun_status(slot: &DesktopTunTaskSlot) -> (bool, Vec<String>) {
    let mut guard = slot.lock();
    if let Some(runtime) = guard.runtime.as_mut() {
        return (runtime.task.is_running(), runtime.task.route_cidrs());
    }
    (false, Vec::new())
}

fn proxy_status_from_parts(
    running: bool,
    settings: &AppSettings,
    tun_running: bool,
    tun_routes: Vec<String>,
) -> ProxyStatus {
    ProxyStatus {
        running,
        listen_addr: settings.local_socks5_listen.clone(),
        tun_running,
        tun_routes,
    }
}

type LocalProxyTaskSlot = Arc<RwLock<Option<LocalProxyTask>>>;
type DesktopTunTaskSlot = Arc<Mutex<DesktopTunState>>;

#[derive(Default)]
struct DesktopTunState {
    runtime: Option<DesktopTunRuntime>,
    latest_apply_result: NativeRoutingApplyResultV2,
}

struct DesktopTunRuntime {
    task: DesktopTunTask,
    route_sync_cancel: CancellationToken,
    route_sync_handle: JoinHandle<()>,
}

fn native_apply_failure_result(error: &str) -> NativeRoutingApplyResultV2 {
    let error = error.to_ascii_lowercase();
    if error.contains("permission") || error.contains("access denied") {
        NativeRoutingApplyResultV2::PermissionDenied
    } else {
        NativeRoutingApplyResultV2::Failed
    }
}

struct LocalProxyTask {
    handle: JoinHandle<()>,
    bound_addr: Arc<RwLock<Option<SocketAddr>>>,
}

impl LocalProxyTask {
    fn is_running(&self) -> bool {
        !self.handle.is_finished() && self.bound_addr.read().is_some()
    }
}

fn local_proxy_task_running(slot: &LocalProxyTaskSlot) -> bool {
    slot.read()
        .as_ref()
        .map(LocalProxyTask::is_running)
        .unwrap_or(false)
}

async fn stop_local_proxy_task(slot: &LocalProxyTaskSlot) {
    let task = slot.write().take();
    if let Some(task) = task {
        let LocalProxyTask { handle, bound_addr } = task;
        *bound_addr.write() = None;
        handle.abort();
        match handle.await {
            Err(e) if e.is_cancelled() => {}
            Err(e) => tracing::warn!(error = %e, "local SOCKS5 proxy task join failed"),
            Ok(()) => {}
        }
    }
}

async fn wait_for_local_proxy_bound(
    slot: &LocalProxyTaskSlot,
    timeout: Duration,
) -> Result<SocketAddr, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(addr) = slot
            .read()
            .as_ref()
            .and_then(|task| *task.bound_addr.read())
        {
            return Ok(addr);
        }
        if Instant::now() >= deadline {
            return Err("local SOCKS5 proxy did not become ready for TUN mode".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_local_proxy_bound_cancellable(
    slot: &LocalProxyTaskSlot,
    timeout: Duration,
    cancel: Option<CancellationToken>,
) -> Result<SocketAddr, String> {
    if let Some(cancel) = cancel {
        tokio::select! {
            _ = cancel.cancelled() => Err("connect cancelled".into()),
            result = wait_for_local_proxy_bound(slot, timeout) => result,
        }
    } else {
        wait_for_local_proxy_bound(slot, timeout).await
    }
}

async fn start_local_proxy_task(
    slot: &LocalProxyTaskSlot,
    engine: Arc<Engine>,
    listen: SocketAddr,
) {
    let started = Instant::now();
    stop_local_proxy_task(slot).await;
    let bound_addr = Arc::new(RwLock::new(None));
    let task_bound_addr = bound_addr.clone();
    let task = tokio::spawn(async move {
        let tunnel_config = wait_for_local_proxy_tunnel_config(&engine).await;
        let auth = match local_proxy_auth_mode_from_tunnel_config(&tunnel_config, listen) {
            Ok(auth) => auth,
            Err(e) => {
                tracing::warn!(error = %e, "local SOCKS5 proxy did not start");
                return;
            }
        };
        let backend = tp_client::proxy_mode::LocalEngineSocks5Backend::new(engine);
        tracing::info!(
            listen = %listen,
            auth = "loopback_no_auth",
            "local SOCKS5 proxy starting"
        );
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let ready_bound_addr = task_bound_addr.clone();
        let ready_task = tokio::spawn(async move {
            if let Ok(addr) = ready_rx.await {
                *ready_bound_addr.write() = Some(addr);
            }
        });
        if let Err(e) = tp_proxy_socks5::serve_with_backend_auth_mode_ready(
            listen,
            Arc::new(backend),
            auth,
            Some(ready_tx),
        )
        .await
        {
            tracing::warn!(error = %e, "local SOCKS5 proxy stopped");
        }
        ready_task.abort();
        *task_bound_addr.write() = None;
    });
    *slot.write() = Some(LocalProxyTask {
        handle: task,
        bound_addr,
    });
    tracing::info!(
        listen = %listen,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "local SOCKS5 proxy task scheduled"
    );
}

async fn wait_for_local_proxy_tunnel_config(engine: &Arc<Engine>) -> tp_client::TunnelConfig {
    let mut next_log_at = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(config) = engine.latest_tunnel_config() {
            return config;
        }
        if Instant::now() >= next_log_at {
            tracing::warn!("local SOCKS5 proxy waiting for V2 runtime config");
            next_log_at += Duration::from_secs(10);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn should_run_desktop_tun(settings: &AppSettings) -> bool {
    should_run_desktop_tun_if_supported(settings, desktop_tun_supported_for_runtime())
}

fn should_run_desktop_tun_if_supported(settings: &AppSettings, supported: bool) -> bool {
    // Deliberately not gated on `local_proxy_enabled`. That switch is the
    // loopback SOCKS5 listener this machine can dial out through; native
    // routing is how Peers reach what this machine publishes. Tying them meant
    // turning off the listener silently killed Tunnel First while its own
    // toggle stayed on and green.
    //
    // Deliberately not gated on `tunnel_first` either, for the same reason in
    // the other direction. Tunnel First only decides which of two overlapping
    // networks wins once routes exist; it is not a request for routes. ORing it
    // in here made it the only switch in the UI that could start the TUN, so
    // turning it off reported `Native routing: Disabled`.
    supported && settings.desktop_network_mode == DesktopNetworkMode::LanRoutesTun
}

fn desktop_tun_lan_routes(settings: &AppSettings, exact_peer_routing: bool) -> Vec<String> {
    if exact_peer_routing {
        // Broad user-configured prefixes remain outside exact Peer routing.
        // Authenticated runtime LAN routes are learned separately and
        // synchronized through DesktopTunConfig::learned_lan_routes.
        Vec::new()
    } else {
        settings.lan_routes.clone()
    }
}

fn desktop_tun_learned_lan_routes(engine: &Engine, tunnel_first: bool) -> Vec<String> {
    if engine.uses_v2_peer_profile() {
        engine.v2_native_lan_route_cidrs(tunnel_first)
    } else {
        engine.lan_alias_route_cidrs()
    }
}

fn desktop_tun_supported_for_runtime() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_tun_helper::status()
            .map(|status| status.installed)
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[tauri::command]
fn get_desktop_tun_capability() -> DesktopTunCapability {
    #[cfg(target_os = "macos")]
    {
        let status =
            macos_tun_helper::status().unwrap_or_else(|e| macos_tun_helper::TunHelperStatus {
                installed: false,
                running: false,
                version: Some(env!("CARGO_PKG_VERSION").into()),
                message: e,
            });
        let message = if !status.installed && status.message.is_empty() {
            MACOS_TUN_HELPER_REQUIRED_MESSAGE.into()
        } else {
            status.message
        };
        DesktopTunCapability {
            supported: true,
            helper_required: true,
            helper_installed: status.installed,
            message,
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        DesktopTunCapability {
            supported: true,
            helper_required: false,
            helper_installed: true,
            message: String::new(),
        }
    }
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn get_tun_helper_status() -> Result<macos_tun_helper::TunHelperStatus, String> {
    macos_tun_helper::status()
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn get_tun_helper_status() -> Result<TunHelperStatusCompat, String> {
    Ok(TunHelperStatusCompat {
        installed: true,
        running: false,
        version: Some(env!("CARGO_PKG_VERSION").into()),
        message: String::new(),
    })
}

#[cfg(target_os = "macos")]
#[tauri::command]
async fn install_tun_helper(_app: AppHandle) -> Result<macos_tun_helper::TunHelperStatus, String> {
    macos_tun_helper::install().await
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
async fn install_tun_helper(_app: AppHandle) -> Result<TunHelperStatusCompat, String> {
    get_tun_helper_status()
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Serialize)]
struct TunHelperStatusCompat {
    installed: bool,
    running: bool,
    version: Option<String>,
    message: String,
}

fn start_desktop_dynamic_route_sync(
    slot: &DesktopTunTaskSlot,
    engine: Arc<Engine>,
    task: DesktopTunTask,
    tunnel_first: bool,
) {
    let route_sync_cancel = CancellationToken::new();
    let watcher_cancel = route_sync_cancel.clone();
    let weak_slot = Arc::downgrade(slot);
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let route_sync_handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        let mut interval = tokio::time::interval(DYNAMIC_ROUTE_SYNC_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = watcher_cancel.cancelled() => break,
                _ = interval.tick() => {}
            }

            let Some(slot) = weak_slot.upgrade() else {
                break;
            };
            let desired_overlay_cidrs = engine.overlay_route_cidrs();
            let desired_learned_lan_cidrs = desktop_tun_learned_lan_routes(&engine, tunnel_first);
            let overlay_route_count = desired_overlay_cidrs.len();
            let learned_lan_route_count = desired_learned_lan_cidrs.len();
            let result = tokio::task::spawn_blocking(move || {
                let mut guard = slot.lock();
                let apply_result = {
                    let Some(runtime) = guard.runtime.as_mut() else {
                        return Ok(false);
                    };
                    runtime
                        .task
                        .sync_dynamic_routes(&desired_overlay_cidrs, &desired_learned_lan_cidrs)
                };
                guard.latest_apply_result = apply_result
                    .as_ref()
                    .map(|()| NativeRoutingApplyResultV2::Applied)
                    .unwrap_or_else(|error| native_apply_failure_result(error));
                apply_result?;
                Ok::<bool, String>(true)
            })
            .await;
            match result {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => break,
                Ok(Err(_error)) => {
                    tracing::debug!(
                        overlay_route_count,
                        learned_lan_route_count,
                        learned_lan_route_source = "identity-bound self-reported",
                        "desktop TUN dynamic route sync failed; retrying"
                    );
                }
                Err(_error) => {
                    tracing::warn!(
                        overlay_route_count,
                        learned_lan_route_count,
                        learned_lan_route_source = "identity-bound self-reported",
                        "desktop TUN dynamic route sync task failed"
                    );
                    break;
                }
            }
        }
    });

    {
        let mut state = slot.lock();
        state.runtime = Some(DesktopTunRuntime {
            task,
            route_sync_cancel,
            route_sync_handle,
        });
        state.latest_apply_result = NativeRoutingApplyResultV2::Applied;
    }
    let _ = start_tx.send(());
}

async fn stop_desktop_tun_task(slot: &DesktopTunTaskSlot) {
    let started = Instant::now();
    let take_slot = Arc::clone(slot);
    let runtime = match tokio::task::spawn_blocking(move || {
        let mut state = take_slot.lock();
        let runtime = state.runtime.take();
        // Reset unconditionally. A TUN that failed to start has no runtime to
        // take, so the old guard left "Failed" or "Repair permissions" on
        // screen for a disconnected Client with nothing to repair.
        state.latest_apply_result = NativeRoutingApplyResultV2::Unavailable;
        runtime
    })
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(%error, "desktop TUN runtime take task failed");
            return;
        }
    };
    if let Some(runtime) = runtime {
        runtime.route_sync_cancel.cancel();
        if let Err(error) = runtime.route_sync_handle.await {
            if !error.is_cancelled() {
                tracing::warn!("desktop TUN dynamic route watcher join failed");
            }
        }
        if let Err(error) = tokio::task::spawn_blocking(move || runtime.task.stop()).await {
            tracing::warn!(%error, "desktop TUN stop task failed");
        }
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "desktop TUN runtime stopped"
        );
    }
}

async fn reconcile_desktop_tun_task(
    slot: &DesktopTunTaskSlot,
    local_proxy_slot: &LocalProxyTaskSlot,
    engine: Option<Arc<Engine>>,
    settings: &AppSettings,
    product: ProductKind,
    resource_dir: Option<PathBuf>,
    cancel: Option<CancellationToken>,
) -> Result<(), String> {
    let started = Instant::now();
    if !should_run_desktop_tun(settings) {
        stop_desktop_tun_task(slot).await;
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "desktop LAN routes via TUN reconcile skipped"
        );
        return Ok(());
    }

    let engine = engine.ok_or_else(|| "connect before enabling LAN routes via TUN".to_string())?;
    let socks5_addr = wait_for_local_proxy_bound_cancellable(
        local_proxy_slot,
        Duration::from_secs(15),
        cancel.clone(),
    )
    .await?;
    if cancel
        .as_ref()
        .map(CancellationToken::is_cancelled)
        .unwrap_or(false)
    {
        return Err("connect cancelled".into());
    }
    let tunnel_config = engine
        .latest_tunnel_config()
        .ok_or_else(|| "V2 runtime config is not ready yet".to_string())?;
    let max_routes = lan_route_entitlement_from_tunnel_config(&tunnel_config);
    let socks5_auth = desktop_tun_socks5_auth_from_tunnel_config(&tunnel_config)?;

    stop_desktop_tun_task(slot).await;
    slot.lock().latest_apply_result = NativeRoutingApplyResultV2::Applying;
    let exact_peer_routing = engine.uses_exact_peer_routing();
    let lan_routes = desktop_tun_lan_routes(settings, exact_peer_routing);
    let overlay_routes = engine.overlay_route_cidrs();
    let learned_lan_routes = desktop_tun_learned_lan_routes(&engine, settings.v2.tunnel_first);
    let config = DesktopTunConfig {
        routes: lan_routes.clone(),
        max_routes,
        overlay_routes: overlay_routes.clone(),
        learned_lan_routes: learned_lan_routes.clone(),
        tunnel_ipv4: tunnel_config
            .overlay_ipv4
            .parse()
            .map_err(|_| "V2 runtime config has an invalid overlay_ipv4".to_string())?,
        socks5_addr,
        socks5_auth,
        config_dir: config_dir(product),
        resource_dir,
    };
    let start_result = tokio::task::spawn_blocking(move || DesktopTunTask::start(config))
        .await
        .map_err(|e| format!("desktop TUN start task failed: {e}"));
    let task = match start_result {
        Ok(Ok(task)) => task,
        Ok(Err(error)) => {
            slot.lock().latest_apply_result = native_apply_failure_result(&error);
            return Err(error);
        }
        Err(error) => {
            slot.lock().latest_apply_result = NativeRoutingApplyResultV2::Failed;
            return Err(error);
        }
    };
    if cancel
        .as_ref()
        .map(CancellationToken::is_cancelled)
        .unwrap_or(false)
    {
        task.stop();
        return Err("connect cancelled".into());
    }
    start_desktop_dynamic_route_sync(slot, engine, task, settings.v2.tunnel_first);
    tracing::info!(
        lan_route_count = lan_routes.len(),
        exact_peer_routing,
        overlay_route_count = overlay_routes.len(),
        learned_lan_route_count = learned_lan_routes.len(),
        learned_lan_route_source = "identity-bound self-reported",
        max_routes,
        socks5 = %socks5_addr,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "desktop LAN routes via TUN started"
    );
    Ok(())
}

fn desktop_tun_socks5_auth_from_tunnel_config(
    tunnel_config: &tp_client::TunnelConfig,
) -> Result<Option<DesktopTunSocks5Auth>, String> {
    v2_local_proxy_peer_id(tunnel_config)?;
    Ok(None)
}

async fn cleanup_cancelled_connect(
    local_proxy_slot: &LocalProxyTaskSlot,
    desktop_tun_slot: &DesktopTunTaskSlot,
    engine: Option<Arc<Engine>>,
) {
    stop_desktop_tun_task(desktop_tun_slot).await;
    stop_local_proxy_task(local_proxy_slot).await;
    if let Some(engine) = engine {
        engine.disconnect().await;
    }
}

async fn disconnect_runtime(
    local_proxy_slot: LocalProxyTaskSlot,
    desktop_tun_slot: DesktopTunTaskSlot,
    engine: Option<Arc<Engine>>,
    status_generation: StatusGenerationGate,
) {
    let started = Instant::now();
    stop_desktop_tun_task(&desktop_tun_slot).await;
    stop_local_proxy_task(&local_proxy_slot).await;
    if let Some(e) = engine {
        e.disconnect().await;
    }
    status_generation.next_generation();
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "desktop runtime disconnect complete"
    );
}

// ----- tracing / logging ---------------------------------------------------

#[derive(Clone)]
struct LogBufferWriter {
    buf: Arc<Mutex<VecDeque<String>>>,
}

struct LogBufferHandle {
    buf: Arc<Mutex<VecDeque<String>>>,
}

impl<'a> MakeWriter<'a> for LogBufferWriter {
    type Writer = LogBufferHandle;
    fn make_writer(&'a self) -> Self::Writer {
        LogBufferHandle {
            buf: self.buf.clone(),
        }
    }
}

impl std::io::Write for LogBufferHandle {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(bytes);
        for line in text.split_terminator('\n') {
            if line.is_empty() {
                continue;
            }
            let mut b = self.buf.lock();
            if b.len() >= LOG_BUFFER_MAX {
                b.pop_front();
            }
            b.push_back(clean_log_line(line));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type SetLevelFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
type LoggingInit = (
    Arc<Mutex<VecDeque<String>>>,
    SetLevelFn,
    Arc<RwLock<String>>,
    PathBuf,
    tracing_appender::non_blocking::WorkerGuard,
);

/// Path of the currently-active log file. file-rotate writes to this
/// exact path and rotates on overflow by renaming it with a timestamp
/// suffix (and optional `.gz`).
fn today_log_path(dir: &PathBuf) -> PathBuf {
    tp_core::log::default_log_path(dir, LOG_FILE_PREFIX, LOG_FILE_SUFFIX)
}

fn init_logging(
    log_dir: &PathBuf,
    product: ProductKind,
    initial_level_override: Option<&str>,
) -> anyhow::Result<LoggingInit> {
    ensure_private_dir(log_dir)?;

    let initial_level = initial_level_override
        .map(str::to_string)
        .or_else(|| std::env::var("LOG_LEVEL").ok())
        .unwrap_or_else(|| {
            merge_product_defaults(read_settings_json(&settings_path(product)), product).log_level
        });
    let initial_filter =
        EnvFilter::try_new(&initial_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, reload_handle) = reload::Layer::new(initial_filter);

    // file-rotate: size/age/compressed rotation; same knob set as the
    // gateway and CLI daemons for consistency.
    let log_path = today_log_path(log_dir);
    let file_writer =
        tp_core::log::build_rolling_writer(&log_path, &tp_core::config::LogConfig::default())?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_writer);

    let log_buffer: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(LOG_BUFFER_MAX)));
    let buffer_writer = LogBufferWriter {
        buf: log_buffer.clone(),
    };

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .compact(),
        )
        .with(
            fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_target(true),
        )
        .with(
            fmt::layer()
                .with_writer(buffer_writer)
                .with_ansi(false)
                .with_target(true),
        )
        .init();

    let level = Arc::new(RwLock::new(initial_level));
    let level_slot = level.clone();
    let set_log_level: SetLevelFn = Arc::new(move |new_level: &str| {
        let filter = EnvFilter::try_new(new_level).map_err(|e| e.to_string())?;
        reload_handle.reload(filter).map_err(|e| e.to_string())?;
        *level_slot.write() = new_level.to_string();
        Ok(())
    });

    let log_file_path = today_log_path(log_dir);

    Ok((log_buffer, set_log_level, level, log_file_path, file_guard))
}

// ----- app state -----------------------------------------------------------

#[derive(Clone)]
struct AppState {
    product: ProductKind,
    engine: Arc<RwLock<Option<Arc<Engine>>>>,
    connect_op: Arc<Mutex<ConnectOperationSlot>>,
    local_proxy: LocalProxyTaskSlot,
    desktop_tun: DesktopTunTaskSlot,
    last_status: Arc<RwLock<ConnectionStatus>>,
    status_generation: StatusGenerationGate,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
    set_log_level: SetLevelFn,
    log_level: Arc<RwLock<String>>,
    log_dir: PathBuf,
}

struct HeadlessRuntimeState {
    engine: Arc<RwLock<Option<Arc<Engine>>>>,
    connect_op: Arc<Mutex<ConnectOperationSlot>>,
    local_proxy: LocalProxyTaskSlot,
    desktop_tun: DesktopTunTaskSlot,
    last_status: Arc<RwLock<ConnectionStatus>>,
    status_generation: StatusGenerationGate,
    generation: u64,
    cancel: CancellationToken,
    settings_path: PathBuf,
}

impl HeadlessRuntimeState {
    fn begin(settings_path: PathBuf) -> Self {
        let status_generation = StatusGenerationGate::default();
        let connect_op = Arc::new(Mutex::new(ConnectOperationSlot::default()));
        let generation = match connect_op.lock().begin(&status_generation) {
            ConnectStartDecision::Started { generation } => generation,
            ConnectStartDecision::AlreadyConnecting { .. } => {
                unreachable!("new headless runtime cannot already be connecting")
            }
        };
        let cancel = connect_op
            .lock()
            .cancel_token(generation)
            .expect("new headless connect operation must have a cancel token");
        Self {
            engine: Arc::new(RwLock::new(None)),
            connect_op,
            local_proxy: Arc::new(RwLock::new(None)),
            desktop_tun: Arc::new(Mutex::new(DesktopTunState::default())),
            last_status: Arc::new(RwLock::new(ConnectionStatus {
                connecting: true,
                message: "Starting".into(),
                path_mode: ConnectionPathMode::Connecting,
                ..Default::default()
            })),
            status_generation,
            generation,
            cancel,
            settings_path,
        }
    }

    fn control_state(&self) -> LocalControlState {
        LocalControlState {
            settings_path: self.settings_path.clone(),
            engine: self.engine.clone(),
            connect_op: Some(self.connect_op.clone()),
            local_proxy: self.local_proxy.clone(),
            desktop_tun: self.desktop_tun.clone(),
            last_status: self.last_status.clone(),
            status_generation: self.status_generation.clone(),
        }
    }
}

#[derive(Clone)]
struct LocalControlState {
    settings_path: PathBuf,
    engine: Arc<RwLock<Option<Arc<Engine>>>>,
    connect_op: Option<Arc<Mutex<ConnectOperationSlot>>>,
    local_proxy: LocalProxyTaskSlot,
    desktop_tun: DesktopTunTaskSlot,
    last_status: Arc<RwLock<ConnectionStatus>>,
    status_generation: StatusGenerationGate,
}

impl LocalControlState {
    fn from_app(state: &AppState) -> Self {
        Self {
            settings_path: settings_path(state.product),
            engine: state.engine.clone(),
            connect_op: Some(state.connect_op.clone()),
            local_proxy: state.local_proxy.clone(),
            desktop_tun: state.desktop_tun.clone(),
            last_status: state.last_status.clone(),
            status_generation: state.status_generation.clone(),
        }
    }

    fn status(&self) -> ClientStatusReadModelV2 {
        let status = self.last_status.read().clone();
        let settings = merge_product_defaults(read_settings_json(&self.settings_path), ProductKind);
        let native_apply_result = self.desktop_tun.lock().latest_apply_result;
        let capability = get_desktop_tun_capability();
        let runtime_snapshot = self
            .engine
            .read()
            .as_ref()
            .map(|engine| engine.v2_runtime_snapshot())
            .unwrap_or_else(|| {
                let mut snapshot = tp_client::runtime_snapshot::V2RuntimeSnapshot::default();
                if status.connecting {
                    snapshot.overall.phase = tp_client::runtime_snapshot::V2OverallPhase::Starting;
                    snapshot.overall.reason_code = None;
                }
                snapshot
            });
        let facts = project_engine_runtime_snapshot(
            runtime_snapshot,
            project_native_routing(
                settings.desktop_network_mode == DesktopNetworkMode::LanRoutesTun,
                native_apply_result,
                capability.helper_required,
                capability.helper_installed,
            ),
        );
        project_client_status_read_model(status, facts)
    }

    fn public_running_status(&self) -> PublicRunningStatusV2 {
        self.status().into()
    }

    async fn disconnect(&self) {
        if let Some(connect_op) = &self.connect_op {
            connect_op.lock().cancel_current();
        }
        let engine = self.engine.write().take();
        disconnect_runtime(
            self.local_proxy.clone(),
            self.desktop_tun.clone(),
            engine,
            self.status_generation.clone(),
        )
        .await;
        *self.last_status.write() = ConnectionStatus {
            message: "Disconnected".into(),
            ..Default::default()
        };
    }
}

#[async_trait::async_trait]
impl IpcHandler for LocalControlState {
    async fn handle(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match method {
            value if value == Method::GetStatus.as_str() => {
                serde_json::to_value(self.public_running_status())
                    .map_err(|error| error.to_string())
            }
            value if value == Method::Disconnect.as_str() => {
                self.disconnect().await;
                Ok(serde_json::json!({ "disconnected": true }))
            }
            _ => Err(format!("unsupported local control method {method:?}")),
        }
    }
}

async fn serve_local_control(
    path: PathBuf,
    state: LocalControlState,
) -> Result<(), tp_ipc::IpcError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    tp_ipc::serve(path, Arc::new(state), EventBroadcaster::new()).await
}

async fn run_running_instance_command_at(
    command: RunningInstanceCommand,
    path: PathBuf,
) -> Result<Option<serde_json::Value>, String> {
    let client = IpcClient::connect(path, env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|error| format!("cannot contact the running lantunnel-client: {error}"))?;
    match command {
        RunningInstanceCommand::Disconnect => {
            client
                .call(Method::Disconnect.as_str(), serde_json::Value::Null)
                .await
                .map_err(|error| format!("running lantunnel-client disconnect failed: {error}"))?;
            Ok(None)
        }
        RunningInstanceCommand::StatusJson => client
            .call(Method::GetStatus.as_str(), serde_json::Value::Null)
            .await
            .map(Some)
            .map_err(|error| format!("running lantunnel-client status failed: {error}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectStartDecision {
    Started { generation: u64 },
    AlreadyConnecting { generation: u64 },
}

#[derive(Default)]
struct ConnectOperationSlot {
    current: Option<ConnectOperation>,
}

struct ConnectOperation {
    generation: u64,
    cancel: CancellationToken,
}

impl ConnectOperationSlot {
    fn begin(&mut self, status_generation: &StatusGenerationGate) -> ConnectStartDecision {
        if let Some(current) = &self.current {
            return ConnectStartDecision::AlreadyConnecting {
                generation: current.generation,
            };
        }

        let generation = status_generation.next_generation();
        self.current = Some(ConnectOperation {
            generation,
            cancel: CancellationToken::new(),
        });
        ConnectStartDecision::Started { generation }
    }

    fn cancel_current(&mut self) {
        if let Some(current) = &self.current {
            current.cancel.cancel();
        }
    }

    fn finish(&mut self, generation: u64) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.generation == generation)
        {
            self.current = None;
        }
    }

    fn cancel_token(&self, generation: u64) -> Option<CancellationToken> {
        self.current
            .as_ref()
            .filter(|current| current.generation == generation)
            .map(|current| current.cancel.clone())
    }

    fn can_publish(&self, generation: u64) -> bool {
        self.current.as_ref().is_some_and(|current| {
            current.generation == generation && !current.cancel.is_cancelled()
        })
    }
}

fn publish_connected_engine_if_current(
    connect_op: &Arc<Mutex<ConnectOperationSlot>>,
    engine_slot: &Arc<RwLock<Option<Arc<Engine>>>>,
    generation: u64,
    engine: Arc<Engine>,
) -> Result<(), Arc<Engine>> {
    let mut op = connect_op.lock();
    if !op.can_publish(generation) {
        return Err(engine);
    }
    *engine_slot.write() = Some(engine);
    op.finish(generation);
    Ok(())
}

#[derive(Clone, Default)]
struct StatusGenerationGate {
    current: Arc<AtomicU64>,
}

impl StatusGenerationGate {
    fn next_generation(&self) -> u64 {
        self.current.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current_generation(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    fn accepts(&self, generation: u64) -> bool {
        self.current_generation() == generation
    }
}

struct TauriListener {
    // TODO: carried but never read. Either the listener should vary by product
    // or the field should go; it decides nothing as it stands.
    #[allow(dead_code)]
    product: ProductKind,
    handle: AppHandle,
    last_status: Arc<RwLock<ConnectionStatus>>,
    status_generation: StatusGenerationGate,
    generation: u64,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
}
impl StatusListener for TauriListener {
    fn on_status(&self, s: &ConnectionStatus) {
        if !self.status_generation.accepts(self.generation) {
            tracing::trace!(
                listener_generation = self.generation,
                current_generation = self.status_generation.current_generation(),
                "ignoring stale app status event"
            );
            return;
        }
        *self.last_status.write() = s.clone();
        let _ = self.handle.emit("status", s);
        let _ = self.handle.emit("tray-status-text", status_text(s));
    }
    fn on_log(&self, line: &str) {
        let masked = clean_log_line(line);
        let mut buf = self.log_buffer.lock();
        if buf.len() >= LOG_BUFFER_MAX {
            buf.pop_front();
        }
        buf.push_back(masked.clone());
        drop(buf);
        let _ = self.handle.emit("log", masked);
    }
}

struct NoUiListener;

impl StatusListener for NoUiListener {
    fn on_status(&self, s: &ConnectionStatus) {
        tracing::info!(
            connected = s.connected,
            connecting = s.connecting,
            gateway = ?s.gateway_addr,
            path_mode = ?s.path_mode,
            uptime_secs = s.uptime_secs,
            error = ?s.error,
            "{}",
            s.message
        );
    }

    fn on_log(&self, line: &str) {
        tracing::info!("{}", clean_log_line(line));
    }
}

struct HeadlessControlListener {
    last_status: Arc<RwLock<ConnectionStatus>>,
    status_generation: StatusGenerationGate,
    generation: u64,
}

impl StatusListener for HeadlessControlListener {
    fn on_status(&self, status: &ConnectionStatus) {
        if !self.status_generation.accepts(self.generation) {
            return;
        }
        *self.last_status.write() = status.clone();
        NoUiListener.on_status(status);
    }

    fn on_log(&self, line: &str) {
        NoUiListener.on_log(line);
    }
}

fn status_text(s: &ConnectionStatus) -> String {
    if s.connected {
        "Status: Connected".into()
    } else if s.connecting {
        "Status: Connecting…".into()
    } else {
        "Status: Disconnected".into()
    }
}

// ----- autostart reconciliation -------------------------------------------

fn reconcile_auto_start(
    app: &AppHandle,
    product: ProductKind,
) -> Result<AutoStartValidation, String> {
    let setting: AppSettings = read_settings_json(&settings_path(product));
    let mgr = app.autolaunch();
    let os_enabled = mgr.is_enabled().map_err(|e| e.to_string())?;
    if os_enabled != setting.auto_start {
        if setting.auto_start {
            mgr.enable().map_err(|e| e.to_string())?;
        } else {
            mgr.disable().map_err(|e| e.to_string())?;
        }
    }
    let now_enabled = mgr.is_enabled().map_err(|e| e.to_string())?;
    Ok(AutoStartValidation {
        os_enabled: now_enabled,
        setting_enabled: setting.auto_start,
        consistent: now_enabled == setting.auto_start,
    })
}

fn should_reconcile_auto_start(previous: &AppSettings, next: &AppSettings) -> bool {
    previous.auto_start != next.auto_start
}

fn startup_auto_connect_peer(
    settings: &AppSettings,
    selection: LastPeerSelection,
) -> Option<String> {
    let tunnel_id = selection.tunnel_id.trim();
    (settings.auto_connect && !tunnel_id.is_empty()).then(|| tunnel_id.to_string())
}

fn spawn_startup_auto_connect(handle: AppHandle, tunnel_id: Option<String>) {
    let Some(tunnel_id) = tunnel_id else {
        return;
    };
    tracing::info!("startup auto-connect requested");
    tauri::async_runtime::spawn(async move {
        let state = handle.state::<AppState>();
        let result = connect_peer_profile(tunnel_id, handle.clone(), state).await;

        if let Err(e) = result {
            tracing::warn!(error = %e, "startup auto-connect failed");
            if let Some(window) = handle.get_webview_window("main") {
                let _ = window.emit("log", format!("Startup auto-connect failed: {e}"));
            }
        }
    });
}

// ----- Tauri commands -----------------------------------------------------

#[tauri::command]
fn list_peer_profiles(state: State<'_, AppState>) -> Result<Vec<ImportedPeerSummaryV2>, String> {
    list_peer_profiles_from_store(&config_dir(state.product)).map_err(|error| error.to_string())
}

/// Removes one imported profile.
///
/// A Tunnel could be joined but never left: nothing here could remove a
/// profile, so the only way out was finding the file on disk.
#[tauri::command]
fn forget_peer_profile(
    tunnel_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ImportedPeerSummaryV2>, String> {
    let root = config_dir(state.product);
    lantunnel_client::peer_store::forget_peer_profile(&root, &tunnel_id)
        .map_err(|error| error.to_string())?;
    list_peer_profiles_from_store(&root).map_err(|error| error.to_string())
}

#[tauri::command]
fn import_peer_profile(
    path: PathBuf,
    state: State<'_, AppState>,
) -> Result<ImportedPeerSummaryV2, String> {
    import_peer_profile_file(&path, &config_dir(state.product)).map_err(|error| error.to_string())
}

struct ConnectRuntimeSource {
    profile: PeerProfileV2,
    static_gateway_override: Option<GatewayBootstrapV2>,
}

impl ConnectRuntimeSource {
    fn platform_url(&self) -> &str {
        match &self.profile.bootstrap {
            tp_core::provisioning::PeerBootstrapV2::ManagedPlatform { platform_url } => {
                platform_url
            }
            tp_core::provisioning::PeerBootstrapV2::StaticGateway(_) => DEFAULT_PLATFORM_URL,
        }
    }

    fn kind(&self) -> &'static str {
        "peer_profile"
    }
}

struct ConnectRuntimeInput {
    source: ConnectRuntimeSource,
    app: AppHandle,
    product: ProductKind,
    generation: u64,
    cancel: CancellationToken,
    connect_op: Arc<Mutex<ConnectOperationSlot>>,
    engine_slot: Arc<RwLock<Option<Arc<Engine>>>>,
    local_proxy_slot: LocalProxyTaskSlot,
    desktop_tun_slot: DesktopTunTaskSlot,
    last_status: Arc<RwLock<ConnectionStatus>>,
    status_generation: StatusGenerationGate,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
}

fn begin_connect_runtime(
    source: ConnectRuntimeSource,
    app: AppHandle,
    state: &AppState,
) -> Result<ConnectRuntimeInput, String> {
    let connect_op = state.connect_op.clone();
    let decision = connect_op.lock().begin(&state.status_generation);
    let generation = match decision {
        ConnectStartDecision::Started { generation } => generation,
        ConnectStartDecision::AlreadyConnecting { .. } => {
            return Err("connect already in progress".into());
        }
    };
    let cancel = connect_op
        .lock()
        .cancel_token(generation)
        .ok_or_else(|| "connect operation disappeared".to_string())?;

    Ok(ConnectRuntimeInput {
        source,
        app,
        product: state.product,
        generation,
        cancel,
        connect_op,
        engine_slot: state.engine.clone(),
        local_proxy_slot: state.local_proxy.clone(),
        desktop_tun_slot: state.desktop_tun.clone(),
        last_status: state.last_status.clone(),
        status_generation: state.status_generation.clone(),
        log_buffer: state.log_buffer.clone(),
    })
}

fn ensure_connect_not_cancelled(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("connect cancelled".into())
    } else {
        Ok(())
    }
}

#[tauri::command]
async fn connect_peer_profile(
    tunnel_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // Load and verify the owner-only profile before beginning a connect
    // operation, so invalid input never tears down an existing runtime.
    let loaded = load_peer_profile(&config_dir(state.product), &tunnel_id)
        .map_err(|error| error.to_string())?;
    let static_gateway_override = match loaded.effective_bootstrap() {
        tp_core::provisioning::PeerBootstrapV2::StaticGateway(gateway) => Some(gateway.clone()),
        tp_core::provisioning::PeerBootstrapV2::ManagedPlatform { .. } => None,
    };
    let source = ConnectRuntimeSource {
        profile: loaded.profile().clone(),
        static_gateway_override,
    };
    write_json(
        &last_peer_selection_path(state.product),
        &LastPeerSelection {
            tunnel_id: tunnel_id.clone(),
        },
    )
    .map_err(|error| error.to_string())?;
    connect_runtime(begin_connect_runtime(source, app, &state)?).await
}

async fn connect_runtime(input: ConnectRuntimeInput) -> Result<(), String> {
    let started = Instant::now();
    let mut engine_for_cleanup: Option<Arc<Engine>> = None;
    let mut cleanup_needed = false;
    let result = connect_runtime_inner(&input, &mut engine_for_cleanup, &mut cleanup_needed).await;
    if result.is_err() {
        if cleanup_needed {
            cleanup_cancelled_connect(
                &input.local_proxy_slot,
                &input.desktop_tun_slot,
                engine_for_cleanup,
            )
            .await;
        }
        input.connect_op.lock().finish(input.generation);
    }
    tracing::info!(
        generation = input.generation,
        elapsed_ms = started.elapsed().as_millis() as u64,
        success = result.is_ok(),
        "lantunnel-client connect runtime complete"
    );
    result
}

async fn connect_runtime_inner(
    input: &ConnectRuntimeInput,
    engine_for_cleanup: &mut Option<Arc<Engine>>,
    cleanup_needed: &mut bool,
) -> Result<(), String> {
    let settings = merge_product_defaults(
        read_settings_json(&settings_path(input.product)),
        input.product,
    );
    let local_proxy_listen = local_proxy_listen_addr(&settings)?;
    validate_v2_local_proxy_listen(local_proxy_listen)?;
    let install_identity = load_or_create_install_identity(input.product)
        .map_err(|e| format!("install identity: {e}"))?;

    ensure_connect_not_cancelled(&input.cancel)?;

    // Take the previous engine out of the slot and wait for it to stop
    // before creating a replacement. Without this, the old Arc's background
    // run() task (spawned in Engine::connect) keeps its own QUIC session
    // and heartbeat alive in parallel with the new one — classic Tauri
    // "reconnect = double session" bug.
    *cleanup_needed = true;
    stop_desktop_tun_task(&input.desktop_tun_slot).await;
    stop_local_proxy_task(&input.local_proxy_slot).await;
    let previous = input.engine_slot.write().take();
    if let Some(prev) = previous {
        let stop_started = Instant::now();
        prev.disconnect().await;
        tracing::info!(
            elapsed_ms = stop_started.elapsed().as_millis() as u64,
            "previous lantunnel-client engine disconnected before reconnect"
        );
    }
    ensure_connect_not_cancelled(&input.cancel)?;

    let listener = Arc::new(TauriListener {
        product: input.product,
        handle: input.app.clone(),
        last_status: input.last_status.clone(),
        status_generation: input.status_generation.clone(),
        generation: input.generation,
        log_buffer: input.log_buffer.clone(),
    });
    let cfg = EngineConfig {
        platform_url: input.source.platform_url().to_string(),
        gateway_ca_path: None,
        insecure_tls: false,
        client_version: env!("CARGO_PKG_VERSION").into(),
        device_id: Some(install_identity.device_id.clone()),
        device_name: install_identity.device_name.clone(),
    };
    let engine = Engine::new(cfg, listener);
    engine
        .set_local_service_exports(&settings.local_service_exports)
        .map_err(|error| format!("invalid local service export policy: {error:?}"))?;
    install_app_v2_settings(
        &engine,
        &compiled_app_v2_settings_or_safe_default(&settings),
    )?;
    *engine_for_cleanup = Some(engine.clone());
    let p2p_cfg = p2p_config_from_settings(&settings);
    engine.set_p2p_config(Arc::new(p2p_cfg.clone()));
    tracing::info!(
        generation = input.generation,
        connection_kind = input.source.kind(),
        product = input.product.binary_name(),
        local_socks5_listen = %settings.local_socks5_listen,
        p2p_allow_lan_candidates = p2p_cfg.allow_lan_candidates,
        "lantunnel-client connect requested"
    );
    let connect_started = Instant::now();
    tokio::select! {
        _ = input.cancel.cancelled() => return Err("connect cancelled".into()),
        result = engine.connect_with_peer_profile(
            input.source.profile.clone(),
            input.source.static_gateway_override.clone(),
        ) => result.map_err(|e| e.to_string())?,
    }
    tracing::info!(
        generation = input.generation,
        elapsed_ms = connect_started.elapsed().as_millis() as u64,
        "lantunnel-client engine connect scheduled"
    );
    ensure_connect_not_cancelled(&input.cancel)?;

    let engine_for_p2p = engine.clone();
    let task_cancel = engine.task_cancel_token();
    tracing::info!(
        generation = input.generation,
        "symmetric Peer mesh bootstrap task scheduled"
    );
    engine.tasks().spawn(async move {
        if let Err(e) = tp_client::p2p::bootstrap::run(engine_for_p2p, p2p_cfg, task_cancel).await {
            tracing::warn!(error = %e, "P2P bootstrap failed; continuing relay-only");
        }
    });
    ensure_connect_not_cancelled(&input.cancel)?;

    if let Some(listen) = local_proxy_listen {
        start_local_proxy_task(&input.local_proxy_slot, engine.clone(), listen).await;
    }
    ensure_connect_not_cancelled(&input.cancel)?;

    if should_run_desktop_tun(&settings) {
        let tun_started = Instant::now();
        if let Err(error) = reconcile_desktop_tun_task(
            &input.desktop_tun_slot,
            &input.local_proxy_slot,
            Some(engine.clone()),
            &settings,
            input.product,
            input.app.path().resource_dir().ok(),
            Some(input.cancel.clone()),
        )
        .await
        {
            tracing::warn!("desktop LAN routes via TUN failed; cancelling connect and cleaning up");
            return Err(error);
        }
        tracing::info!(
            generation = input.generation,
            elapsed_ms = tun_started.elapsed().as_millis() as u64,
            "desktop LAN routes via TUN reconcile complete"
        );
    }
    ensure_connect_not_cancelled(&input.cancel)?;

    let publish_started = Instant::now();
    match publish_connected_engine_if_current(
        &input.connect_op,
        &input.engine_slot,
        input.generation,
        engine,
    ) {
        Ok(()) => {
            *engine_for_cleanup = None;
            tracing::info!(
                generation = input.generation,
                elapsed_ms = publish_started.elapsed().as_millis() as u64,
                "lantunnel-client engine published"
            );
            Ok(())
        }
        Err(engine) => {
            *engine_for_cleanup = Some(engine);
            Err("connect cancelled".into())
        }
    }
}

async fn start_headless_network_tasks(
    product: ProductKind,
    engine: Arc<Engine>,
    settings: &AppSettings,
    local_proxy_listen: Option<SocketAddr>,
    local_proxy_slot: &LocalProxyTaskSlot,
    desktop_tun_slot: &DesktopTunTaskSlot,
    cancel: &CancellationToken,
) -> Result<(), String> {
    ensure_connect_not_cancelled(cancel)?;
    if let Some(listen) = local_proxy_listen {
        start_local_proxy_task(local_proxy_slot, engine.clone(), listen).await;
    }
    ensure_connect_not_cancelled(cancel)?;
    if should_run_desktop_tun(settings) {
        if let Err(_error) = reconcile_desktop_tun_task(
            desktop_tun_slot,
            local_proxy_slot,
            Some(engine),
            settings,
            product,
            None,
            Some(cancel.clone()),
        )
        .await
        {
            ensure_connect_not_cancelled(cancel)?;
            tracing::warn!("desktop LAN routes via TUN failed; keeping SOCKS5 proxy connected");
        }
    }
    ensure_connect_not_cancelled(cancel)
}

async fn run_no_ui(product: ProductKind, startup: StartupArgs) -> anyhow::Result<()> {
    startup.validate_no_ui().map_err(anyhow::Error::msg)?;
    let settings = merge_product_defaults(read_settings_json(&settings_path(product)), product);
    let tunnel_id = startup
        .peer_tunnel_id
        .clone()
        .or_else(|| {
            startup_auto_connect_peer(&settings, read_json(&last_peer_selection_path(product)))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--headless requires `connect <Tunnel ID>` or an Auto-connect imported Peer"
            )
        })?;
    run_no_ui_peer(product, &startup, &tunnel_id).await
}

async fn run_no_ui_peer(
    product: ProductKind,
    startup: &StartupArgs,
    tunnel_id: &str,
) -> anyhow::Result<()> {
    let loaded = load_peer_profile(&config_dir(product), tunnel_id)?;
    let static_gateway_override = match loaded.effective_bootstrap() {
        tp_core::provisioning::PeerBootstrapV2::StaticGateway(gateway) => Some(gateway.clone()),
        tp_core::provisioning::PeerBootstrapV2::ManagedPlatform { .. } => None,
    };
    let settings = apply_startup_overrides(
        merge_product_defaults(read_settings_json(&settings_path(product)), product),
        startup,
    );
    let local_proxy_listen = local_proxy_listen_addr(&settings).map_err(anyhow::Error::msg)?;
    validate_v2_local_proxy_listen(local_proxy_listen).map_err(anyhow::Error::msg)?;
    let install_identity = load_or_create_install_identity(product)?;
    let runtime = HeadlessRuntimeState::begin(settings_path(product));
    let control_path = local_control_path(product);
    let control_state = runtime.control_state();
    let mut control_task = tokio::spawn(async move {
        let result = serve_local_control(control_path, control_state).await;
        if let Err(error) = &result {
            tracing::error!(%error, "local headless Client control server stopped");
        }
        result
    });
    tokio::task::yield_now().await;
    if control_task.is_finished() {
        let outcome = (&mut control_task)
            .await
            .map_err(|error| anyhow::anyhow!("local headless control task failed: {error}"))?;
        return Err(outcome
            .err()
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("local headless control server stopped")));
    }

    if let Err(error) = start_headless_peer_runtime(
        product,
        loaded.profile().clone(),
        static_gateway_override,
        &settings,
        local_proxy_listen,
        install_identity,
        &runtime,
    )
    .await
    {
        control_task.abort();
        let _ = control_task.await;
        return Err(error);
    }

    let signal_result = wait_shutdown_signal().await;
    control_task.abort();
    let _ = control_task.await;
    let signal = signal_result?;
    tracing::info!(signal, tunnel_id, "V2 headless Peer shutdown requested");
    let engine = runtime.engine.write().take();
    disconnect_runtime(
        runtime.local_proxy,
        runtime.desktop_tun,
        engine,
        runtime.status_generation,
    )
    .await;
    Ok(())
}

async fn start_headless_peer_runtime(
    product: ProductKind,
    profile: PeerProfileV2,
    static_gateway_override: Option<GatewayBootstrapV2>,
    settings: &AppSettings,
    local_proxy_listen: Option<SocketAddr>,
    install_identity: InstallIdentity,
    runtime: &HeadlessRuntimeState,
) -> anyhow::Result<()> {
    let mut engine_for_cleanup = None;
    let startup_result: anyhow::Result<()> = async {
        ensure_connect_not_cancelled(&runtime.cancel).map_err(anyhow::Error::msg)?;
        let engine = Engine::new(
            EngineConfig {
                platform_url: DEFAULT_PLATFORM_URL.into(),
                gateway_ca_path: None,
                insecure_tls: false,
                client_version: env!("CARGO_PKG_VERSION").into(),
                device_id: Some(install_identity.device_id.clone()),
                device_name: install_identity.device_name.clone(),
            },
            Arc::new(HeadlessControlListener {
                last_status: runtime.last_status.clone(),
                status_generation: runtime.status_generation.clone(),
                generation: runtime.generation,
            }),
        );
        engine_for_cleanup = Some(engine.clone());
        engine
            .set_local_service_exports(&settings.local_service_exports)
            .map_err(|error| anyhow::anyhow!("invalid local service export policy: {error:?}"))?;
        install_app_v2_settings(&engine, &compiled_app_v2_settings_or_safe_default(settings))
            .map_err(anyhow::Error::msg)?;
        let p2p_cfg = p2p_config_from_settings(settings);
        engine.set_p2p_config(Arc::new(p2p_cfg.clone()));
        tokio::select! {
            _ = runtime.cancel.cancelled() => return Err(anyhow::anyhow!("connect cancelled")),
            result = engine.connect_with_peer_profile(profile, static_gateway_override) => result?,
        }
        ensure_connect_not_cancelled(&runtime.cancel).map_err(anyhow::Error::msg)?;
        let engine_for_p2p = engine.clone();
        let task_cancel = engine.task_cancel_token();
        engine.tasks().spawn(async move {
            if let Err(error) =
                tp_client::p2p::bootstrap::run(engine_for_p2p, p2p_cfg, task_cancel).await
            {
                tracing::warn!(%error, "V2 P2P bootstrap failed; continuing Relay-only");
            }
        });
        start_headless_network_tasks(
            product,
            engine.clone(),
            settings,
            local_proxy_listen,
            &runtime.local_proxy,
            &runtime.desktop_tun,
            &runtime.cancel,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        match publish_connected_engine_if_current(
            &runtime.connect_op,
            &runtime.engine,
            runtime.generation,
            engine,
        ) {
            Ok(()) => {
                engine_for_cleanup = None;
                Ok(())
            }
            Err(engine) => {
                engine_for_cleanup = Some(engine);
                Err(anyhow::anyhow!("connect cancelled"))
            }
        }
    }
    .await;

    if let Err(error) = startup_result {
        cleanup_cancelled_connect(
            &runtime.local_proxy,
            &runtime.desktop_tun,
            engine_for_cleanup,
        )
        .await;
        runtime.connect_op.lock().finish(runtime.generation);
        if runtime.cancel.is_cancelled() {
            *runtime.last_status.write() = ConnectionStatus {
                message: "Disconnected".into(),
                ..Default::default()
            };
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

async fn wait_shutdown_signal() -> std::io::Result<&'static str> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                Ok("sigint")
            }
            _ = sigterm.recv() => Ok("sigterm"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok("ctrl_c")
    }
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    LocalControlState::from_app(&state).disconnect().await;
    Ok(())
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> ClientStatusReadModelV2 {
    LocalControlState::from_app(&state).status()
}

#[tauri::command]
fn get_proxy_status(state: State<'_, AppState>) -> ProxyStatus {
    let settings = merge_product_defaults(
        read_settings_json(&settings_path(state.product)),
        state.product,
    );
    let (tun_running, tun_routes) = desktop_tun_status(&state.desktop_tun);
    proxy_status_from_parts(
        local_proxy_task_running(&state.local_proxy),
        &settings,
        tun_running,
        tun_routes,
    )
}

#[tauri::command]
fn get_clash_config(state: State<'_, AppState>) -> Result<String, String> {
    let settings = merge_product_defaults(
        read_settings_json(&settings_path(state.product)),
        state.product,
    );
    let listen = local_proxy_listen_addr(&settings)?
        .ok_or_else(|| "local SOCKS5 proxy is disabled for this product".to_string())?;
    let engine = state
        .engine
        .read()
        .clone()
        .ok_or_else(|| "connect before copying Clash config".to_string())?;
    let cfg = engine
        .latest_tunnel_config()
        .ok_or_else(|| "V2 runtime config is not ready yet".to_string())?;
    clash_overlay_yaml(&cfg, listen, DEFAULT_PLATFORM_URL)
}

#[tauri::command]
fn write_clipboard_text(text: String) -> Result<(), String> {
    write_text_to_native_clipboard(&text)
}

fn write_text_to_native_clipboard(text: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for command in clipboard_command_candidates() {
        match write_text_with_clipboard_command(command, text) {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(format!("{}: {e}", command.program)),
        }
    }

    if errors.is_empty() {
        return Err("no native clipboard command is configured for this platform".into());
    }
    Err(format!(
        "native clipboard copy failed: {}",
        errors.join("; ")
    ))
}

#[allow(
    deprecated,
    reason = "tauri-plugin-shell remains the app's installed URL opener"
)]
fn open_platform_dashboard(app: &AppHandle) {
    let _ = app
        .shell()
        .open(format!("{DEFAULT_PLATFORM_URL}/dashboard"), None);
}

fn write_text_with_clipboard_command(command: &ClipboardCommand, text: &str) -> Result<(), String> {
    let mut child = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "stdin was not available".to_string())?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("stdin write failed: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait failed: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("exited with status {}", output.status))
    } else {
        Err(format!("exited with status {}: {stderr}", output.status))
    }
}

fn clipboard_command_candidates() -> &'static [ClipboardCommand] {
    #[cfg(target_os = "macos")]
    {
        const COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
            program: "/usr/bin/pbcopy",
            args: &[],
        }];
        COMMANDS
    }

    #[cfg(target_os = "windows")]
    {
        const COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
            program: "powershell.exe",
            args: &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ],
        }];
        COMMANDS
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        const COMMANDS: &[ClipboardCommand] = &[
            ClipboardCommand {
                program: "wl-copy",
                args: &[],
            },
            ClipboardCommand {
                program: "xclip",
                args: &["-selection", "clipboard"],
            },
            ClipboardCommand {
                program: "xsel",
                args: &["--clipboard", "--input"],
            },
        ];
        COMMANDS
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    {
        const COMMANDS: &[ClipboardCommand] = &[];
        COMMANDS
    }
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> AppSettingsReadModelV2 {
    let settings = merge_product_defaults(
        read_settings_json(&settings_path(state.product)),
        state.product,
    );
    let local_exports = state
        .engine
        .read()
        .as_ref()
        .map(|engine| engine.v2_runtime_snapshot().local_exports)
        .unwrap_or_else(|| {
            compiled_app_v2_settings_or_safe_default(&settings)
                .local_runtime_record
                .lan_exports
                .into_iter()
                .map(
                    |export| tp_client::runtime_snapshot::V2LocalExportSnapshot {
                        prefix: format!("{}/{}", export.prefix.network, export.prefix.prefix_len),
                        ready: export.ready,
                    },
                )
                .collect()
        });
    app_settings_read_model_with_exports(settings, local_exports)
}

#[cfg(test)]
fn app_settings_read_model(settings: AppSettings) -> AppSettingsReadModelV2 {
    let local_exports = compiled_app_v2_settings_or_safe_default(&settings)
        .local_runtime_record
        .lan_exports
        .into_iter()
        .map(
            |export| tp_client::runtime_snapshot::V2LocalExportSnapshot {
                prefix: format!("{}/{}", export.prefix.network, export.prefix.prefix_len),
                ready: export.ready,
            },
        )
        .collect();
    app_settings_read_model_with_exports(settings, local_exports)
}

fn app_settings_read_model_with_exports(
    settings: AppSettings,
    local_exports: Vec<tp_client::runtime_snapshot::V2LocalExportSnapshot>,
) -> AppSettingsReadModelV2 {
    // A saved V2 block that will not compile is replaced wholesale before it
    // reaches the engine — deny-all, no Exports, no tunnel-first — so the
    // owner has to be told, or the Settings tab describes a device that is
    // refusing everything as though it were open.
    //
    // The saved block itself is returned untouched. The frontend posts
    // `{...settings, ...patch}` back on any save, so replacing it here erased
    // the owner's rules and Exports off disk the next time they changed the
    // log level — a display fallback is not something to write back.
    // Whether the saved block compiles is a property of the saved block, so
    // this must not be compile_app_v2_settings — that one opens with
    // discover_connected_lan_prefixes(), and get_settings is a synchronous
    // Tauri command the UI calls on every status tick. Enumerating every
    // network interface once a second on the main thread stopped the Windows
    // message pump outright: the window went "not responding" while connected.
    let v2_settings_rejected =
        settings.unreadable || compile_client_settings_v2(&settings.v2).is_err();
    let client_ui = project_client_settings(ClientSettingsFactsV2 {
        tunnel_first: Some(settings.v2.tunnel_first),
        exported_lans: Some(settings.v2.exported_lans.clone()),
        client_access: Some(settings.v2.client_access.clone()),
    });
    AppSettingsReadModelV2 {
        settings,
        v2_settings_rejected,
        client_ui,
        exported_lan_statuses: local_exports
            .into_iter()
            .map(|export| LocalExportStatusV2 {
                prefix: export.prefix,
                ready: export.ready,
            })
            .collect(),
    }
}

#[tauri::command]
async fn save_settings(
    settings: AppSettings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let (
        product,
        previous,
        settings,
        next_local_proxy_listen,
        previous_local_proxy_listen,
        local_proxy_changed,
        desktop_tun_changed,
        engine,
        local_proxy_slot,
        desktop_tun_slot,
        compiled_v2,
        previous_compiled_v2,
    ) = {
        let product = state.product;
        let previous = merge_product_defaults(read_settings_json(&settings_path(product)), product);
        let previous_compiled_v2 = compiled_app_v2_settings_or_safe_default(&previous);
        let mut settings = merge_product_defaults(settings, product);
        settings.local_socks5_listen =
            normalize_local_socks5_listen(&settings.local_socks5_listen)?;
        // No tier, no ceiling, no tier-derived rewrite. The product does not
        // advertise or enforce a LAN-route ceiling, and a reported tier must
        // not silently truncate a list the owner wrote. Routes are still
        // checked for being well-formed and private.
        settings.lan_routes = validate_lan_routes(&settings.lan_routes, usize::MAX)
            .map_err(|e| e.to_string())?
            .routes;
        let next_local_proxy_listen = local_proxy_listen_addr(&settings)?;
        validate_v2_local_proxy_listen(next_local_proxy_listen)?;
        let previous_local_proxy_listen = local_proxy_listen_addr(&previous)?;
        let local_proxy_changed = previous_local_proxy_listen != next_local_proxy_listen;
        let desktop_tun_changed = local_proxy_changed
            || previous.desktop_network_mode != settings.desktop_network_mode
            || previous.lan_routes != settings.lan_routes
            || previous.v2.tunnel_first != settings.v2.tunnel_first;
        let engine = state.engine.read().clone();
        if engine.is_some() && p2p_settings_require_reconnect(&previous, &settings) {
            return Err(
                "Disconnect before changing P2P or LAN mesh settings so every socket and route uses one connection generation"
                    .into(),
            );
        }
        let local_proxy_slot = state.local_proxy.clone();
        let desktop_tun_slot = state.desktop_tun.clone();
        let compiled_v2 = compile_app_v2_settings(&settings)?;
        (state.set_log_level)(&settings.log_level)?;
        write_json(&settings_path(product), &settings).map_err(|error| error.to_string())?;
        if should_reconcile_auto_start(&previous, &settings) {
            let mgr = app.autolaunch();
            if settings.auto_start {
                mgr.enable().map_err(|e| e.to_string())?;
            } else {
                mgr.disable().map_err(|e| e.to_string())?;
            }
        }

        (
            product,
            previous,
            settings,
            next_local_proxy_listen,
            previous_local_proxy_listen,
            local_proxy_changed,
            desktop_tun_changed,
            engine,
            local_proxy_slot,
            desktop_tun_slot,
            compiled_v2,
            previous_compiled_v2,
        )
    };
    if local_proxy_changed {
        if let Some(engine) = engine.clone() {
            if let Some(listen) = next_local_proxy_listen {
                start_local_proxy_task(&local_proxy_slot, engine.clone(), listen).await;
            } else {
                stop_local_proxy_task(&local_proxy_slot).await;
            }
        }
    }
    if desktop_tun_changed && engine.is_some() {
        if let Err(e) = reconcile_desktop_tun_task(
            &desktop_tun_slot,
            &local_proxy_slot,
            engine.clone(),
            &settings,
            product,
            app.path().resource_dir().ok(),
            None,
        )
        .await
        {
            let _ = write_json(&settings_path(product), &previous);
            if local_proxy_changed {
                if let Some(engine) = engine.clone() {
                    if let Some(listen) = previous_local_proxy_listen {
                        start_local_proxy_task(&local_proxy_slot, engine.clone(), listen).await;
                    } else {
                        stop_local_proxy_task(&local_proxy_slot).await;
                    }
                }
            }
            let _ = reconcile_desktop_tun_task(
                &desktop_tun_slot,
                &local_proxy_slot,
                engine,
                &previous,
                product,
                app.path().resource_dir().ok(),
                None,
            )
            .await;
            return Err(e);
        }
    }
    if let Some(engine) = engine.as_ref() {
        if let Err(error) = install_app_v2_settings(engine, &compiled_v2) {
            let runtime_rollback = install_app_v2_settings(engine, &previous_compiled_v2);
            let disk_rollback = write_json(&settings_path(product), &previous);
            return Err(match (runtime_rollback, disk_rollback) {
                (Ok(()), Ok(())) => {
                    format!("apply V2 Client settings failed; previous settings restored: {error}")
                }
                (runtime, disk) => format!(
                    "apply V2 Client settings failed and rollback was incomplete (runtime={runtime:?}, disk={disk:?}): {error}"
                ),
            });
        }
    }
    Ok(())
}

#[tauri::command]
fn set_auto_start(app: AppHandle, enabled: bool, state: State<'_, AppState>) -> Result<(), String> {
    let mut s: AppSettings = read_settings_json(&settings_path(state.product));
    s.auto_start = enabled;
    write_json(&settings_path(state.product), &s).map_err(|e| e.to_string())?;
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())?;
    } else {
        mgr.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn set_auto_connect(enabled: bool, state: State<'_, AppState>) -> Result<(), String> {
    let mut s: AppSettings = read_settings_json(&settings_path(state.product));
    s.auto_connect = enabled;
    write_json(&settings_path(state.product), &s).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_logs(limit: Option<usize>, state: State<'_, AppState>) -> Vec<String> {
    let buf = state.log_buffer.lock();
    let limit = limit.unwrap_or(500).min(buf.len());
    buf.iter()
        .rev()
        .take(limit)
        .rev()
        .map(|line| clean_log_line(line))
        .collect()
}

#[tauri::command]
fn clear_logs(state: State<'_, AppState>) {
    state.log_buffer.lock().clear();
}

#[tauri::command]
fn set_log_level(level: String, state: State<'_, AppState>) -> Result<(), String> {
    (state.set_log_level)(&level)
}

#[tauri::command]
fn get_log_config(state: State<'_, AppState>) -> LogConfigInfo {
    LogConfigInfo {
        level: state.log_level.read().clone(),
        file_path: today_log_path(&state.log_dir).to_string_lossy().to_string(),
    }
}

#[tauri::command]
fn get_log_file_path(state: State<'_, AppState>) -> String {
    today_log_path(&state.log_dir).to_string_lossy().to_string()
}

#[tauri::command]
fn get_product_info(state: State<'_, AppState>) -> ProductInfo {
    product_info_for_product(state.product)
}

/// What this Client can do, for a UI that is shared with two phones.
///
/// Every Client draws the same screens in the same order; a flag here means
/// the platform genuinely cannot offer the thing, not that it looks different
/// somewhere. The desktop offers all four.
#[tauri::command]
fn get_capabilities() -> serde_json::Value {
    serde_json::json!({
        // A desktop has no camera flow worth the surface; a `.peer` file
        // arrives as a file here.
        "qrScanner": false,
        "startAtLogin": true,
        "localProxy": true,
        "exportReadiness": true,
        "nativeRoutingSwitch": true,
    })
}

fn product_info_for_product(product: ProductKind) -> ProductInfo {
    ProductInfo {
        binary_name: product.binary_name().into(),
        display_name: product.display_name().into(),
        role: product.role_label().into(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changing the log level must not take the app down.
    ///
    /// reload::Handle::reload holds the reload layer's write lock while the
    /// callsite interest cache is rebuilt, and that calls back into the
    /// subscriber. std's RwLock is not reentrant, so the shape is worth
    /// holding still even though it does not reproduce the SIGABRT this was
    /// written to chase: that crash is still unexplained, and the panic
    /// location recovered from the binary belonged to a sibling branch of the
    /// faulting function rather than to the faulting call itself.
    #[test]
    fn changing_the_log_level_does_not_abort() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log_dir = dir.path().to_path_buf();
        let (_buffer, set_level, level, _path, guard) =
            init_logging(&log_dir, ProductKind, Some("info")).expect("init logging");
        std::mem::forget(guard);

        for next in ["debug", "trace", "warn", "info", "error", "debug"] {
            set_level(next).unwrap_or_else(|error| panic!("set {next} failed: {error}"));
            assert_eq!(*level.read(), next);
            // An event on the same thread right after the swap is what the
            // running app does on every status tick.
            tracing::info!(target: "lantunnel_client", level = next, "level changed");
        }
    }

    /// A tray callback runs on the main thread, where there is no Tokio
    /// runtime. tokio::spawn needs one and panics without it — and the release
    /// profile aborts on panic, so clicking Disconnect or Quit in the tray
    /// killed the app. Five identical SIGABRT reports across 2.0.3 and 2.0.4
    /// were this, unreadable until a panic hook wrote the line out.
    #[test]
    fn tray_callbacks_do_not_reach_for_a_runtime_they_are_not_in() {
        let source = include_str!("main.rs");
        let tray = source
            .split_once("TrayIconBuilder::with_id(\"main\")")
            .expect("the tray is built here")
            .1;
        let handlers = tray
            .split_once(".on_tray_icon_event(")
            .map(|(before, _)| before)
            .unwrap_or(tray);
        // Strip comments first: the explanation of this bug names the call it
        // warns about, and a guard that matches its own comment is blind.
        let code: String = handlers
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("tokio::spawn"),
            "a tray menu handler still calls tokio::spawn, which panics off the runtime"
        );
        assert!(
            code.contains("tauri::async_runtime::spawn"),
            "tray work must go through Tauri's runtime handle"
        );
    }

    /// The window title is the product, not the product plus its category.
    ///
    /// TAURI_PRODUCT_NAME names the bundle, the DMG volume, the AppImage and
    /// the exe as well, so it stays put; only the title is ours to set here.
    #[test]
    fn the_window_title_does_not_say_client() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json parses");
        let title = conf["app"]["windows"][0]["title"]
            .as_str()
            .expect("the first window has a title");
        assert_eq!(title, "Lantunnel");
        assert_eq!(
            conf["productName"].as_str(),
            Some("Lantunnel Client"),
            "productName names the bundle and must not move with the title"
        );

        // The build overwrites this file with a generated config, so the file
        // alone proves nothing about the shipped window.
        let makefile = include_str!("../../../../Makefile");
        assert!(
            !makefile.contains(r#""title":"$(TAURI_PRODUCT_NAME)""#),
            "the build-time config still takes the window title from the product name"
        );
        assert!(
            makefile.contains(r#""title":"$(TAURI_WINDOW_TITLE)""#),
            "the build-time config must set the window title from its own variable"
        );
        assert!(
            makefile.contains("TAURI_WINDOW_TITLE  := Lantunnel\n")
                || makefile.contains("TAURI_WINDOW_TITLE := Lantunnel\n"),
            "TAURI_WINDOW_TITLE must be defined as Lantunnel"
        );
    }

    /// Help must not name a command the binary refuses.
    ///
    /// Removing the Static Gateway override took the subcommand out and left
    /// both help lines in, so `tunnel gateway set` was advertised and then
    /// answered with "expected `tunnel import <file>` or `tunnel list`".
    #[test]
    fn help_names_no_command_the_parser_rejects() {
        assert!(!PUBLIC_HELP.contains("gateway set"));
        assert!(!PUBLIC_HELP.contains("Static Gateway"));
        for advertised in [
            "tunnel import",
            "tunnel list",
            "connect",
            "disconnect",
            "status",
        ] {
            assert!(
                PUBLIC_HELP.contains(advertised),
                "help stopped naming {advertised}"
            );
        }
    }

    /// The whole point of the hook is that a crash leaves a trace, so an
    /// untested one is worse than none: it would look like coverage while
    /// still telling nobody. Five SIGABRT reports carried nothing because
    /// the default hook writes to a stderr a Finder launch does not have.
    #[test]
    fn a_panic_writes_its_location_into_the_client_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log_dir = dir.path().to_path_buf();
        let previous = std::panic::take_hook();
        install_panic_logger(log_dir.clone());

        let panicked = std::panic::catch_unwind(|| {
            panic!("a rule the app itself wrote");
        });
        std::panic::set_hook(previous);
        assert!(panicked.is_err());

        let written = std::fs::read_to_string(today_log_path(&log_dir))
            .expect("the hook must have created the log");
        assert!(
            written.contains("a rule the app itself wrote"),
            "the panic message is missing: {written}"
        );
        assert!(
            written.contains("PANIC lantunnel_client"),
            "the line is not recognisable as a panic: {written}"
        );
        assert!(
            written.contains("main.rs:"),
            "the location is missing, which is the only reason to log it: {written}"
        );
    }

    /// The Settings tab must describe the policy that is running.
    ///
    /// A saved policy that will not compile is replaced with deny-all before
    /// it reaches the engine — but the read model projected the saved file, so
    /// the UI said Peers could reach this device while nothing could.
    #[test]
    fn settings_that_do_not_compile_are_described_as_closed() {
        use tp_client::access_policy::{
            ClientAccessPolicyV2, ClientAccessPortV2, ClientAccessProtocolV2, ClientAccessRuleV2,
            ClientAccessTargetV2,
        };

        let mut settings = AppSettings::default();
        settings.v2.client_access = ClientAccessPolicyV2 {
            allow: vec![ClientAccessRuleV2 {
                target: ClientAccessTargetV2::ThisPeer,
                protocol: ClientAccessProtocolV2::Tcp,
                port: ClientAccessPortV2::Exact(0),
            }],
            deny: Vec::new(),
        };
        assert!(
            compile_app_v2_settings(&settings).is_err(),
            "test needs a policy that fails to compile"
        );
        let saved = settings.v2.client_access.clone();

        let model = app_settings_read_model(settings);

        // The saved block comes back untouched. The frontend posts
        // `{...settings, ...patch}` back on any save, so replacing it here
        // meant one bad character plus one unrelated setting change erased
        // the owner's rules and Exports off disk with nothing said.
        assert_eq!(
            model.settings.v2.client_access, saved,
            "the saved policy must survive being displayed"
        );

        // ...and the UI is told plainly that none of it is running.
        assert!(
            model.v2_settings_rejected,
            "the owner was not told their settings are not in effect"
        );
    }

    /// The same reasoning one layer earlier. A policy that parses but does not
    /// compile is replaced with deny-all, yet a file that does not parse at all
    /// never reached that guard: `read_json` turned it into `Default`, whose
    /// empty Allow list opens this Client to its Tunnel. A file we cannot read
    /// is not a Client nobody configured.
    #[test]
    fn settings_that_do_not_parse_are_closed_and_reported() {
        let dir = std::env::temp_dir().join(format!(
            "lantunnel-unreadable-settings-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp settings dir");
        let path = dir.join("settings.json");

        // The exact shape that went stale in tests/e2e/v2_docker: a field the
        // policy no longer has, under `deny_unknown_fields`.
        std::fs::write(
            &path,
            r#"{"log_level":"debug","client_access":{"default_action":"deny","allow":[],"deny":[]}}"#,
        )
        .expect("write corrupt settings");

        let settings = read_settings_json(&path);
        assert!(
            settings.unreadable,
            "a settings file that exists and does not parse must be marked unreadable"
        );

        let compiled = compiled_app_v2_settings_or_safe_default(&settings);
        assert!(
            compiled.client_access.is_closed(),
            "an unreadable settings file must not widen this Client to its Tunnel"
        );

        assert!(
            app_settings_read_model(settings).v2_settings_rejected,
            "the owner was not told their settings could not be read"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Absence is not corruption: a Client nobody has configured still opens to
    /// its Tunnel, which is what makes a fresh install reachable.
    #[test]
    fn absent_settings_stay_open() {
        let path = std::env::temp_dir().join(format!(
            "lantunnel-absent-settings-{}-does-not-exist.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let settings = read_settings_json(&path);
        assert!(!settings.unreadable, "a missing file is not a corrupt one");
        assert!(
            !compiled_app_v2_settings_or_safe_default(&settings)
                .client_access
                .is_closed(),
            "a Client nobody configured must stay reachable inside its Tunnel"
        );
    }

    #[test]
    fn public_running_status_preserves_projected_client_ui_status() {
        let mut client_ui = ClientUiStatusV2::default();
        client_ui.gateway_attachment.endpoint = Some("relay.example.test:443".into());
        client_ui.overall = lantunnel_client::client_ui_status::ClientOverallStateV2::Degraded;
        client_ui.overall_reason_code = Some("mesh_sync_incomplete".into());

        let public = PublicRunningStatusV2::from(ClientStatusReadModelV2 {
            connection: ConnectionStatus {
                path_mode: ConnectionPathMode::Connecting,
                ..Default::default()
            },
            client_ui,
        });

        assert_eq!(
            public.client_ui.gateway_attachment.endpoint.as_deref(),
            Some("relay.example.test:443")
        );
        assert_eq!(
            public.client_ui.overall,
            lantunnel_client::client_ui_status::ClientOverallStateV2::Degraded
        );
        assert_eq!(
            public.client_ui.overall_reason_code.as_deref(),
            Some("mesh_sync_incomplete")
        );
    }

    /// A Client nobody has configured is reachable by the Tunnel it joined.
    ///
    /// Reaching this Client already requires issued Peer membership in the same
    /// Tunnel, so denying by default did not add a trust boundary — it only made
    /// a fresh install silently unreachable until someone found this setting.
    /// An Allow rule, or any Deny rule, still closes it.
    /// Nothing writes a plan tier or a route ceiling into saved settings.
    ///
    /// The product does not advertise or enforce a LAN-route ceiling; do not
    /// reintroduce one. A Gateway that still reports a tier must not put one
    /// back.
    /// What `get_settings` hands out must be something `save_settings` accepts.
    ///
    /// The read model adds `client_ui` and `exported_lan_statuses` on top of
    /// `AppSettings`, which denies unknown fields — so a UI that stored the
    /// read model and posted it back had every save rejected. The two halves
    /// were tested separately and never composed, which is why nobody noticed
    /// that no setting could be changed.
    /// The status poll must not enumerate network interfaces.
    ///
    /// get_settings is a synchronous Tauri command, so it runs on the main
    /// thread, and the UI calls it on every status tick. Compiling through
    /// compile_app_v2_settings opened with discover_connected_lan_prefixes(),
    /// which walks every adapter — on Windows, with a TUN and the usual
    /// virtual switches present, that is slow enough to stop the message pump.
    /// The window read "not responding" on a healthy connection.
    #[test]
    fn the_settings_read_model_does_not_walk_the_network_interfaces() {
        let source = include_str!("main.rs");
        let body = source
            .split("fn app_settings_read_model_with_exports(")
            .nth(1)
            .expect("the read model builder is still named this");
        let body = &body[..body.find("\n}\n").expect("function has an end")];
        // Without this the assertion matches the comment that explains it.
        let code: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !code.contains("compile_app_v2_settings"),
            "the read model compiles through the interface-discovering path again"
        );
        assert!(
            code.contains("compile_client_settings_v2("),
            "the read model no longer validates the saved block at all"
        );
    }

    /// The same round trip the way Tauri actually does it.
    ///
    /// A Tauri command deserializes its arguments from the raw JSON string, so
    /// keys arrive in serialization order. `from_value` sorts them, which is a
    /// different order into two `#[serde(flatten)]` fields — and that is not
    /// the order the running Client sees.
    #[test]
    fn the_settings_read_model_round_trips_as_a_json_string() {
        let read_model = app_settings_read_model(AppSettings::default());
        let handed_out = serde_json::to_string(&read_model).expect("serialize read model");

        let posted_back: Result<AppSettings, _> = serde_json::from_str(&handed_out);

        assert!(
            posted_back.is_ok(),
            "settings handed to the UI must be acceptable back: {:?}",
            posted_back.err()
        );
    }

    #[test]
    fn the_settings_read_model_round_trips_through_save() {
        let read_model = app_settings_read_model(AppSettings::default());
        let handed_out = serde_json::to_value(read_model).expect("serialize read model");

        let posted_back: Result<AppSettings, _> = serde_json::from_value(handed_out);

        assert!(
            posted_back.is_ok(),
            "settings handed to the UI must be acceptable back: {:?}",
            posted_back.err()
        );
    }

    #[test]
    fn app_settings_defaults_allow_peers_in_the_same_tunnel() {
        let json = serde_json::to_value(AppSettings::default()).expect("serialize settings");

        assert!(json["client_access"].get("default_action").is_none());
        assert_eq!(json["client_access"]["allow"], serde_json::json!([]));
        assert_eq!(json["client_access"]["deny"], serde_json::json!([]));
        assert_eq!(json["exported_lans"], serde_json::json!([]));
        assert_eq!(json["auto_export_current_lan"], true);
        assert_eq!(json["tunnel_first"], false);
        assert!(json.get("v2").is_none());
    }

    #[test]
    fn a_settings_file_saved_before_this_release_exports_its_current_lan() {
        // The switch is flattened into the same file the previous release
        // wrote, so an upgrade reads it as absent. Absent has to mean on, or
        // upgrading would be the one way to get a Client that never shares the
        // network it is on and never says so.
        let saved = serde_json::json!({
            "auto_start": false,
            "auto_connect": false,
            "local_socks5_listen": "127.0.0.1:1080",
            "settings_version": 1,
            "local_proxy_enabled": true,
            "desktop_network_mode": "socks5_only",
            "lan_routes": [],
            "p2p_allow_lan_candidates": false,
            "local_service_exports": [],
            "log_level": "info",
            "client_access": { "allow": [], "deny": [] },
            "exported_lans": ["10.40.0.0/16"],
            "tunnel_first": false,
        });

        let settings: AppSettings =
            serde_json::from_value(saved).expect("read a file written before the switch existed");

        assert!(settings.v2.auto_export_current_lan);
        assert_eq!(settings.v2.exported_lans, vec!["10.40.0.0/16".to_string()]);
    }

    #[test]
    fn settings_read_model_returns_saved_v2_values_as_ready() {
        let mut settings = AppSettings::default();
        settings.v2.tunnel_first = true;
        settings.v2.exported_lans = vec!["10.40.0.0/16".into()];
        let read_model = app_settings_read_model(settings);
        let json = serde_json::to_value(read_model).expect("serialize settings read model");

        assert_eq!(json["client_ui"]["client_access"]["availability"], "ready");
        assert_eq!(
            json["client_ui"]["client_access"]["value"]["allow"],
            serde_json::json!([])
        );
        assert_eq!(json["client_ui"]["exported_lans"]["availability"], "ready");
        assert_eq!(
            json["client_ui"]["exported_lans"]["value"],
            serde_json::json!(["10.40.0.0/16"])
        );
        assert_eq!(json["client_ui"]["tunnel_first"]["availability"], "ready");
        assert_eq!(json["client_ui"]["tunnel_first"]["value"], true);
    }

    #[test]
    fn settings_read_model_exposes_backend_owned_lan_export_readiness() {
        let mut settings = AppSettings::default();
        settings.v2.exported_lans = vec!["10.40.0.0/16".into(), "192.168.44.0/24".into()];
        let read_model = app_settings_read_model_with_exports(
            settings,
            vec![
                tp_client::runtime_snapshot::V2LocalExportSnapshot {
                    prefix: "10.40.0.0/16".into(),
                    ready: true,
                },
                tp_client::runtime_snapshot::V2LocalExportSnapshot {
                    prefix: "192.168.44.0/24".into(),
                    ready: false,
                },
            ],
        );
        let json = serde_json::to_value(read_model).expect("serialize settings read model");

        assert_eq!(
            json["exported_lan_statuses"],
            serde_json::json!([
                { "prefix": "10.40.0.0/16", "ready": true },
                { "prefix": "192.168.44.0/24", "ready": false },
            ])
        );
    }

    #[test]
    fn invalid_v2_settings_leave_the_persisted_last_good_unchanged() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        let last_good = AppSettings::default();
        write_json(&path, &last_good).expect("write last-good settings");
        let before = std::fs::read(&path).expect("read last-good settings");
        let mut invalid = last_good;
        invalid.v2.exported_lans = vec!["192.168.4.7/24".into()];

        assert!(persist_validated_app_settings(&path, &invalid).is_err());
        assert_eq!(
            std::fs::read(path).expect("read settings after rejection"),
            before
        );
    }

    #[test]
    fn invalid_persisted_v2_settings_compile_to_safe_runtime_defaults() {
        let mut settings = AppSettings::default();
        settings.v2.exported_lans = vec!["203.0.113.0/24".into()];

        let compiled = compiled_app_v2_settings_or_safe_default(&settings);

        // Settings that fail to compile fall back to refusing everything, not
        // to the fresh-install default: someone did configure this Client and
        // we cannot read what they asked for.
        assert!(compiled.client_access.is_closed());
        assert!(compiled.client_access.allow.is_empty());
        assert!(compiled.local_runtime_record.lan_exports.is_empty());
    }

    #[test]
    fn credentials_storage_does_not_depend_on_os_keyring() {
        let manifest = include_str!("../Cargo.toml");

        assert!(
            !manifest.contains("keyring"),
            "lantunnel-client must not use OS keychain/keyring storage"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_clipboard_uses_pbcopy_on_macos() {
        let commands = clipboard_command_candidates();

        assert_eq!(commands[0].program, "/usr/bin/pbcopy");
        assert!(commands[0].args.is_empty());
    }

    #[test]
    fn client_settings_defaults_include_local_ingress_fields() {
        let settings: AppSettings = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(settings.local_socks5_listen, "127.0.0.1:1080");
        assert!(settings.local_proxy_enabled);
        assert!(!settings.p2p_allow_lan_candidates);
        assert_eq!(settings.log_level, "info");
    }

    #[test]
    fn public_settings_reject_removed_mesh_role_fields() {
        for (field, value) in [
            ("p2p_enabled", serde_json::json!(false)),
            ("peer_client_id", serde_json::json!("manual-target")),
        ] {
            let mut json = serde_json::json!({});
            json[field] = value;

            assert!(
                serde_json::from_value::<AppSettings>(json).is_err(),
                "removed field must be rejected: {field}"
            );
        }
    }

    #[test]
    fn public_settings_reject_historical_product_mode() {
        assert!(serde_json::from_value::<AppSettings>(serde_json::json!({
            "mode": "app"
        }))
        .is_err());
    }

    #[test]
    fn app_settings_default_to_socks5_only_with_no_lan_routes() {
        let settings: AppSettings = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(
            settings.desktop_network_mode,
            DesktopNetworkMode::Socks5Only
        );
        assert!(settings.lan_routes.is_empty());
    }

    #[test]
    fn a_pre_split_file_with_tunnel_first_keeps_its_native_routes() {
        // Written by a build where Tunnel First was ORed into
        // `should_run_desktop_tun`: the owner had native routing, the file
        // says `socks5_only`, and nothing records the version.
        let saved: AppSettings = serde_json::from_value(serde_json::json!({
            "desktop_network_mode": "socks5_only",
            "tunnel_first": true,
        }))
        .unwrap();
        assert_eq!(saved.settings_version, 0, "absence marks a file to migrate");

        let migrated = merge_product_defaults(saved, ProductKind);

        assert_eq!(
            migrated.desktop_network_mode,
            DesktopNetworkMode::LanRoutesTun
        );
        assert!(should_run_desktop_tun_if_supported(&migrated, true));
        assert!(migrated.v2.tunnel_first, "the answer itself is untouched");
        assert_eq!(migrated.settings_version, SETTINGS_VERSION);
    }

    #[test]
    fn a_pre_split_file_without_tunnel_first_gains_no_native_routes() {
        let saved: AppSettings = serde_json::from_value(serde_json::json!({
            "desktop_network_mode": "socks5_only",
            "tunnel_first": false,
        }))
        .unwrap();

        let migrated = merge_product_defaults(saved, ProductKind);

        assert_eq!(
            migrated.desktop_network_mode,
            DesktopNetworkMode::Socks5Only,
            "migration carries native routing across, it does not turn it on"
        );
        assert_eq!(migrated.settings_version, SETTINGS_VERSION);
    }

    #[test]
    fn turning_native_routing_off_survives_a_reload_with_tunnel_first_on() {
        // The one state the migration must not fight: after the split, Tunnel
        // First keeps the owner's answer while its switch is disabled.
        let saved: AppSettings = serde_json::from_value(serde_json::json!({
            "settings_version": SETTINGS_VERSION,
            "desktop_network_mode": "socks5_only",
            "tunnel_first": true,
        }))
        .unwrap();

        let reloaded = merge_product_defaults(saved, ProductKind);

        assert_eq!(
            reloaded.desktop_network_mode,
            DesktopNetworkMode::Socks5Only
        );
        assert!(!should_run_desktop_tun_if_supported(&reloaded, true));
    }

    #[test]
    fn desktop_tun_starts_for_overlay_mode_even_before_remote_routes_arrive() {
        let mut settings = AppSettings {
            desktop_network_mode: DesktopNetworkMode::LanRoutesTun,
            lan_routes: vec!["192.168.0.0/16".into()],
            ..AppSettings::default()
        };

        assert_eq!(
            should_run_desktop_tun(&settings),
            desktop_tun_supported_for_runtime()
        );

        settings.lan_routes.clear();
        assert!(should_run_desktop_tun_if_supported(&settings, true));
        assert_eq!(
            should_run_desktop_tun(&settings),
            desktop_tun_supported_for_runtime()
        );

        settings.lan_routes = vec!["192.168.0.0/16".into()];
        settings.desktop_network_mode = DesktopNetworkMode::Socks5Only;
        assert!(!should_run_desktop_tun(&settings));

        settings.v2.tunnel_first = true;
        assert!(
            !should_run_desktop_tun_if_supported(&settings, true),
            "Tunnel First decides which overlapping network wins; it does not ask for native routes"
        );
        settings.v2.tunnel_first = false;

        settings.desktop_network_mode = DesktopNetworkMode::LanRoutesTun;
        assert!(
            should_run_desktop_tun_if_supported(&settings, true),
            "the unified Peer ingress must start its configured native route consumer"
        );
        // The claim the function's own comment makes. Asserting through
        // `should_run_desktop_tun` instead only ever passed because this Mac
        // has no TUN helper installed, so `supported` was false and the switch
        // under test was never reached — on Linux, where support is
        // unconditional, the same line failed.
        settings.local_proxy_enabled = false;
        assert!(
            should_run_desktop_tun_if_supported(&settings, true),
            "the loopback listener is what this machine dials out through, not what Peers reach it by"
        );
    }

    #[test]
    fn headless_v2_peer_uses_common_network_startup_lifecycle() {
        let source = include_str!("main.rs");
        let peer_start = source
            .find("async fn run_no_ui_peer(")
            .expect("V2 Peer headless runtime");
        let signal_start = source
            .find("async fn wait_shutdown_signal(")
            .expect("headless signal waiter");
        let peer_runtime = &source[peer_start..signal_start];
        assert!(peer_runtime.contains("start_headless_network_tasks("));
        let control_server = peer_runtime
            .find("serve_local_control(")
            .expect("headless control server");
        let engine_start = peer_runtime
            .find("Engine::new(")
            .expect("headless engine start");
        let network_start = peer_runtime
            .find("start_headless_network_tasks(")
            .expect("headless network startup");
        assert!(
            control_server < engine_start && control_server < network_start,
            "headless status/disconnect must be available before engine, proxy, or TUN startup"
        );

        let helper_start = source
            .find("async fn start_headless_network_tasks(")
            .expect("common headless network startup helper");
        let helper = &source[helper_start..peer_start];
        assert!(helper.contains("start_local_proxy_task("));
        assert!(helper.contains("reconcile_desktop_tun_task("));
    }

    #[test]
    fn exact_peer_tun_defers_unowned_legacy_lan_routes() {
        let settings = AppSettings {
            lan_routes: vec!["192.168.0.0/16".into(), "10.0.0.0/8".into()],
            ..AppSettings::default()
        };

        assert!(desktop_tun_lan_routes(&settings, true).is_empty());
        assert_eq!(
            desktop_tun_lan_routes(&settings, false),
            settings.lan_routes,
            "fixed/direct legacy mode retains its explicitly configured target seam"
        );
    }

    #[test]
    fn get_desktop_tun_capability_matches_release_gate() {
        let capability = get_desktop_tun_capability();

        if cfg!(target_os = "macos") {
            assert!(capability.supported);
            assert!(capability.helper_required);
            if !capability.helper_installed {
                assert!(!capability.message.is_empty());
            }
        } else {
            assert!(capability.supported);
            assert!(!capability.helper_required);
            assert!(capability.helper_installed);
            assert!(capability.message.is_empty());
        }
    }

    #[test]
    fn desktop_lan_routes_accept_private_and_link_local_cidrs() {
        let routes = validate_lan_routes(
            &[
                "192.168.1.42/24".to_string(),
                "10.1.2.3/8".to_string(),
                "172.16.9.9/12".to_string(),
                "169.254.7.8/16".to_string(),
            ],
            10,
        )
        .expect("private LAN and link-local routes should validate");

        assert_eq!(
            routes.routes,
            vec![
                "192.168.1.0/24",
                "10.0.0.0/8",
                "172.16.0.0/12",
                "169.254.0.0/16",
            ]
        );
    }

    #[test]
    fn desktop_lan_route_specs_include_route_command_fields() {
        let routes = desktop_routes::lan_route_specs(&["192.168.1.42/24".to_string()], 1)
            .expect("private route should parse");

        assert_eq!(routes[0].cidr, "192.168.1.0/24");
        assert_eq!(routes[0].network, "192.168.1.0");
        assert_eq!(routes[0].prefix, 24);
        assert_eq!(routes[0].netmask, "255.255.255.0");
    }

    #[test]
    fn desktop_lan_routes_reject_public_ranges() {
        let err = validate_lan_routes(&["8.8.8.0/24".to_string()], 10)
            .expect_err("public route must be rejected");

        assert!(err.to_string().contains("private IPv4 LAN or link-local"));
    }

    #[test]
    fn desktop_lan_routes_reject_link_local_route_that_escapes_range() {
        let err = validate_lan_routes(&["169.254.0.0/15".to_string()], 10)
            .expect_err("link-local route must stay inside 169.254/16");

        assert!(err.to_string().contains("private IPv4 LAN or link-local"));
    }

    #[test]
    fn autostart_reconcile_is_skipped_for_log_level_only_changes() {
        let previous = AppSettings::default();
        let next = AppSettings {
            log_level: "debug".into(),
            ..previous.clone()
        };

        assert!(!should_reconcile_auto_start(&previous, &next));
    }

    #[test]
    fn autostart_reconcile_runs_when_auto_start_changes() {
        let previous = AppSettings::default();
        let next = AppSettings {
            auto_start: true,
            ..previous.clone()
        };

        assert!(should_reconcile_auto_start(&previous, &next));
    }

    #[test]
    fn public_product_is_one_symmetric_client() {
        let product = ProductKind;

        assert_eq!(product.binary_name(), "lantunnel-client");
        assert_eq!(product.display_name(), "Lantunnel");
        assert_eq!(product.role_label(), "peer");
        assert!(product.default_settings().local_proxy_enabled);
        assert!(!product.default_settings().p2p_allow_lan_candidates);
    }

    #[test]
    fn p2p_config_from_settings_threads_explicit_lan_mesh_opt_in() {
        let settings = AppSettings {
            p2p_allow_lan_candidates: true,
            ..AppSettings::default()
        };

        let p2p = p2p_config_from_settings(&settings);

        assert!(p2p.allow_lan_candidates);
        assert!(p2p.allow_lan_route_aliases);
    }

    #[test]
    fn changing_lan_mesh_policy_requires_a_fresh_connection_generation() {
        let previous = AppSettings::default();
        let mut next = previous.clone();
        next.p2p_allow_lan_candidates = !previous.p2p_allow_lan_candidates;

        assert!(p2p_settings_require_reconnect(&previous, &next));

        next = previous.clone();
        next.log_level = "debug".into();
        assert!(!p2p_settings_require_reconnect(&previous, &next));

        next = previous.clone();
        next.local_service_exports = vec![tp_core::config::LocalServiceExportConfig {
            route_kind: tp_core::config::LocalServiceRouteKindConfig::Overlay,
            protocol: tp_core::config::LocalServiceProtocolConfig::Tcp,
            ingress_port: 27015,
            source_policy: tp_core::config::LocalServiceSourcePolicyConfig::AnyTunnelPeer,
            local_host: "127.0.0.1".into(),
            local_port: 27015,
        }];
        assert!(p2p_settings_require_reconnect(&previous, &next));
    }

    #[test]
    fn product_info_exposes_unified_peer_identity() {
        let info = product_info_for_product(ProductKind);

        assert_eq!(info.binary_name, "lantunnel-client");
        assert_eq!(info.display_name, "Lantunnel");
        assert_eq!(info.role, "peer");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn app_config_dir_override_separates_local_multi_instance_paths() {
        let a = product_config_dir_with_override(Some("/tmp/tp-root-a"), ProductKind);
        let b = product_config_dir_with_override(Some("/tmp/tp-root-b"), ProductKind);

        assert_ne!(a, b);
        assert_eq!(
            last_peer_selection_path_in_dir(&a),
            PathBuf::from("/tmp/tp-root-a/app/last-peer.json")
        );
        assert_eq!(
            settings_path_in_dir(&b),
            PathBuf::from("/tmp/tp-root-b/app/settings.json")
        );
    }

    #[test]
    fn app_config_dir_override_treats_product_specific_dirs_as_exact() {
        assert_eq!(
            product_config_dir_with_override(Some("/tmp/lantunnel/app"), ProductKind),
            PathBuf::from("/tmp/lantunnel/app")
        );
        assert_eq!(
            product_config_dir_with_override(Some("/tmp/lantunnel/client"), ProductKind),
            PathBuf::from("/tmp/lantunnel/client")
        );
        assert_eq!(
            product_config_dir_with_override(Some("/tmp/tp-app-a"), ProductKind),
            PathBuf::from("/tmp/tp-app-a")
        );
    }

    #[test]
    fn client_config_dir_override_keeps_existing_app_data_directory() {
        let root = PathBuf::from("/tmp/lantunnel-root");

        assert_eq!(
            product_config_dir_with_override(Some("/tmp/lantunnel-root"), ProductKind),
            root.join("app")
        );
    }

    #[test]
    fn unified_client_keeps_existing_app_config_path() {
        let root = std::env::temp_dir().join(format!(
            "lantunnel-product-config-test-{}",
            std::process::id()
        ));

        assert_eq!(
            product_config_dir_in_root(&root, ProductKind),
            root.join("app")
        );
    }

    #[test]
    fn install_identity_is_created_once_per_product_config_dir() {
        let root = std::env::temp_dir().join(format!(
            "lantunnel-install-identity-test-{}",
            std::process::id()
        ));
        let dir = product_config_dir_in_root(&root, ProductKind);

        let first =
            load_or_create_install_identity_in_dir(&dir).expect("first identity should write");
        let second =
            load_or_create_install_identity_in_dir(&dir).expect("second identity should read");

        assert!(is_valid_device_id(&first.device_id));
        assert_eq!(first.device_id, second.device_id);
        assert!(install_identity_path_in_dir(&dir).exists());
    }

    #[test]
    fn startup_args_parse_peer_import_as_one_shot_headless_command() {
        let args =
            StartupArgs::parse_from(["lantunnel-client", "tunnel", "import", "/tmp/friends.peer"])
                .expect("Peer import command should parse");

        assert_eq!(
            args.command,
            Some(StartupCommand::ImportPeer(PathBuf::from(
                "/tmp/friends.peer"
            )))
        );
        assert!(args.no_ui);
        assert!(args.validate_no_ui().is_ok());
    }

    #[test]
    fn startup_version_command_returns_exact_public_version_line() {
        let args = StartupArgs::parse_from(["lantunnel-client", "--version"])
            .expect("public version command should parse");
        let expected = format!("lantunnel-client {}", env!("CARGO_PKG_VERSION"));

        assert_eq!(
            args.early_exit_output(ProductKind).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn startup_version_command_does_not_initialize_runtime_settings() {
        let args = StartupArgs::parse_from_with_env(
            ["lantunnel-client", "--version"],
            StartupEnv {
                local_socks5_listen: Some("not-a-socket-address"),
                ..StartupEnv::default()
            },
        )
        .expect("version command should bypass runtime settings");
        let expected = format!("lantunnel-client {}", env!("CARGO_PKG_VERSION"));

        assert_eq!(
            args.early_exit_output(ProductKind).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn startup_short_version_alias_returns_exact_public_version_line() {
        let args = StartupArgs::parse_from(["lantunnel-client", "-V"])
            .expect("public short version command should parse");
        let expected = format!("lantunnel-client {}", env!("CARGO_PKG_VERSION"));

        assert_eq!(
            args.early_exit_output(ProductKind).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn startup_args_parse_imported_peer_connect_as_headless_runtime() {
        let args = StartupArgs::parse_from([
            "lantunnel-client",
            "connect",
            "2c53c210-28b0-4cd3-909a-e8440f2dad03",
        ])
        .expect("Peer connect command should parse");

        assert_eq!(
            args.peer_tunnel_id.as_deref(),
            Some("2c53c210-28b0-4cd3-909a-e8440f2dad03")
        );
        assert!(args.no_ui);
        assert!(args.validate_no_ui().is_ok());
    }

    #[test]
    fn startup_args_parse_disconnect_as_running_instance_command() {
        let args = StartupArgs::parse_from(["lantunnel-client", "disconnect"])
            .expect("disconnect command should parse");

        assert_eq!(
            args.instance_command,
            Some(RunningInstanceCommand::Disconnect)
        );
        assert!(args.no_ui);
        assert!(args.validate_no_ui().is_ok());
    }

    #[test]
    fn startup_args_parse_status_json_as_running_instance_command() {
        let args = StartupArgs::parse_from(["lantunnel-client", "status", "--json"])
            .expect("status --json command should parse");

        assert_eq!(
            args.instance_command,
            Some(RunningInstanceCommand::StatusJson)
        );
        assert!(args.no_ui);
        assert!(args.validate_no_ui().is_ok());
    }

    #[test]
    fn startup_args_reject_status_without_json_or_extra_disconnect_args() {
        assert_eq!(
            StartupArgs::parse_from(["lantunnel-client", "status"])
                .expect_err("status without --json must fail"),
            "status requires exactly --json"
        );
        assert_eq!(
            StartupArgs::parse_from(["lantunnel-client", "disconnect", "now"])
                .expect_err("disconnect extras must fail"),
            "disconnect accepts no arguments"
        );
    }

    #[test]
    fn startup_args_reject_peer_connect_without_tunnel_id() {
        let error = StartupArgs::parse_from(["lantunnel-client", "connect"])
            .expect_err("Peer connect must identify an imported Tunnel");

        assert_eq!(error, "connect requires an imported Tunnel ID");
    }

    #[test]
    fn startup_args_parse_log_level() {
        let args = StartupArgs::parse_from(["lantunnel-client", "--log-level", "debug"])
            .expect("log-level args should parse");

        assert_eq!(args.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn startup_args_parse_local_socks5_listen() {
        let args =
            StartupArgs::parse_from(["lantunnel-client", "--local-socks5-listen", "0.0.0.0:18080"])
                .expect("local socks5 args should parse");

        assert_eq!(args.local_socks5_listen.as_deref(), Some("0.0.0.0:18080"));
    }

    #[test]
    fn startup_args_reject_mesh_enable_disable_overrides() {
        for args in [
            vec!["lantunnel-client", "--p2p-enabled", "false"],
            vec!["lantunnel-client", "--disable-p2p"],
            vec!["lantunnel-client", "--no-p2p"],
            vec!["lantunnel-client", "--enable-p2p"],
        ] {
            let error = StartupArgs::parse_from(args)
                .expect_err("Lantunnel 2.0 Mesh is fixed on and has no public toggle");

            assert!(error.contains("unknown argument"));
        }
    }

    #[test]
    fn startup_args_parse_lan_p2p_override() {
        let args = StartupArgs::parse_from(["lantunnel-client", "--enable-lan-p2p"])
            .expect("lan p2p args should parse");

        assert_eq!(args.p2p_allow_lan_candidates, Some(true));

        let args = StartupArgs::parse_from_with_lan_p2p_env(["lantunnel-client"], Some("true"))
            .expect("lan p2p env should parse");

        assert_eq!(args.p2p_allow_lan_candidates, Some(true));
    }

    #[test]
    fn startup_args_parse_desktop_tun_env_overrides() {
        let args = StartupArgs::parse_from_with_env(
            ["lantunnel-client"],
            StartupEnv {
                local_socks5_listen: Some("0.0.0.0:18080"),
                desktop_network_mode: Some("lan-routes-tun"),
                lan_routes: Some("192.168.0.0/16,10.0.0.0/8"),
                ..StartupEnv::default()
            },
        )
        .expect("desktop tun env should parse");

        assert_eq!(args.local_socks5_listen.as_deref(), Some("0.0.0.0:18080"));
        assert_eq!(
            args.desktop_network_mode,
            Some(DesktopNetworkMode::LanRoutesTun)
        );
        assert_eq!(
            args.lan_routes,
            Some(vec!["192.168.0.0/16".to_string(), "10.0.0.0/8".to_string()])
        );
    }

    #[test]
    fn startup_args_parse_desktop_tun_cli_overrides() {
        let args = StartupArgs::parse_from([
            "lantunnel-client",
            "--enable-desktop-tun",
            "--lan-route",
            "192.168.1.42/24",
            "--lan-route",
            "10.0.0.0/8",
        ])
        .expect("desktop tun cli args should parse");

        assert_eq!(
            args.desktop_network_mode,
            Some(DesktopNetworkMode::LanRoutesTun)
        );
        assert_eq!(
            args.lan_routes,
            Some(vec!["192.168.1.0/24".to_string(), "10.0.0.0/8".to_string()])
        );
    }

    #[test]
    fn startup_args_reject_bad_desktop_tun_env_routes() {
        let err = StartupArgs::parse_from_with_env(
            ["lantunnel-client"],
            StartupEnv {
                lan_routes: Some("8.8.8.0/24"),
                ..StartupEnv::default()
            },
        )
        .expect_err("public LAN route env should be rejected");

        assert!(err.contains(LAN_ROUTES_ENV));
    }

    #[test]
    fn startup_args_reject_lan_p2p_aliases() {
        for args in [
            vec!["lantunnel-client", "--disable-lan-p2p"],
            vec!["lantunnel-client", "--p2p-allow-lan", "true"],
            vec!["lantunnel-client", "--allow-lan-p2p", "true"],
        ] {
            let err = StartupArgs::parse_from(args).expect_err("lan p2p aliases must be rejected");

            assert!(err.contains("unknown argument"));
        }
    }

    #[test]
    fn startup_args_parse_no_ui_aliases() {
        let args = StartupArgs::parse_from(["lantunnel-client", "--no-ui"])
            .expect("no-ui args should parse");

        assert!(args.no_ui);
        assert!(args.validate_no_ui().is_ok());

        let args = StartupArgs::parse_from(["lantunnel-client", "--headless"])
            .expect("headless alias should parse");

        assert!(args.no_ui);
        assert!(args.validate_no_ui().is_ok());
    }

    #[test]
    fn startup_args_reject_unknown_positionals_instead_of_auto_connecting() {
        for args in [
            vec!["lantunnel-client", "unexpected"],
            vec!["lantunnel-client", "--headless", "unexpected"],
        ] {
            let error = StartupArgs::parse_from(args)
                .expect_err("an unknown positional must not fall through to saved Auto-connect");

            assert!(error.contains("unknown argument"));
        }
    }

    #[test]
    fn explicit_arguments_require_the_parent_console() {
        assert!(invocation_requires_parent_console(2));
        assert!(invocation_requires_parent_console(3));
        assert!(!invocation_requires_parent_console(1));
    }

    #[test]
    fn startup_args_reject_legacy_seed_options() {
        for flag in [
            concat!("--", "seed"),
            concat!("--", "seed", "-file"),
            concat!("--", "platform", "-url"),
        ] {
            let error = StartupArgs::parse_from(["lantunnel-client", flag, "value"])
                .expect_err("legacy V1 option must be rejected");
            assert!(error.contains("unknown argument"));
        }
    }

    #[test]
    fn startup_args_reject_bad_local_socks5_listen() {
        let err =
            StartupArgs::parse_from(["lantunnel-client", "--local-socks5-listen", "not-an-addr"])
                .expect_err("bad local socks5 listen must fail");

        assert!(err.contains("invalid local_socks5_listen"));
    }

    #[test]
    fn startup_auto_connect_uses_non_secret_last_peer_selection() {
        let settings = AppSettings {
            auto_connect: true,
            ..AppSettings::default()
        };
        let selection = LastPeerSelection {
            tunnel_id: "saved-tid".into(),
        };
        assert_eq!(
            startup_auto_connect_peer(&settings, selection).as_deref(),
            Some("saved-tid")
        );
    }

    #[test]
    fn startup_overrides_apply_local_socks5_listen_to_settings() {
        let startup = StartupArgs {
            local_socks5_listen: Some("0.0.0.0:18080".into()),
            ..StartupArgs::default()
        };
        let settings = AppSettings {
            local_socks5_listen: "127.0.0.1:1080".into(),
            ..AppSettings::default()
        };

        let next = apply_startup_overrides(settings, &startup);

        assert_eq!(next.local_socks5_listen, "0.0.0.0:18080");
    }

    #[test]
    fn startup_overrides_apply_desktop_tun_settings() {
        let startup = StartupArgs {
            desktop_network_mode: Some(DesktopNetworkMode::LanRoutesTun),
            lan_routes: Some(vec!["192.168.0.0/16".into()]),
            ..StartupArgs::default()
        };
        let settings = AppSettings {
            desktop_network_mode: DesktopNetworkMode::Socks5Only,
            lan_routes: Vec::new(),
            ..AppSettings::default()
        };

        let next = apply_startup_overrides(settings, &startup);

        assert_eq!(next.desktop_network_mode, DesktopNetworkMode::LanRoutesTun);
        assert_eq!(next.lan_routes, vec!["192.168.0.0/16".to_string()]);
    }

    #[test]
    fn startup_overrides_apply_lan_p2p_to_settings() {
        let startup = StartupArgs {
            p2p_allow_lan_candidates: Some(true),
            ..StartupArgs::default()
        };
        let settings = AppSettings {
            p2p_allow_lan_candidates: false,
            ..AppSettings::default()
        };

        let next = apply_startup_overrides(settings, &startup);

        assert!(next.p2p_allow_lan_candidates);
    }

    #[test]
    fn single_instance_lock_rejects_second_process() {
        let dir = std::env::temp_dir().join(format!(
            "lantunnel-client-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.lock");

        let first = SingleInstanceGuard::acquire(path.clone(), ProductKind)
            .expect("first lock should succeed");
        assert!(
            SingleInstanceGuard::acquire(path.clone(), ProductKind).is_err(),
            "second app process must not acquire the same lock"
        );
        drop(first);
        SingleInstanceGuard::acquire(path, ProductKind)
            .expect("lock should be reusable after first exits");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn single_instance_lock_reuses_stale_lock_file() {
        let dir = std::env::temp_dir().join(format!(
            "lantunnel-stale-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("client.lock");
        std::fs::write(&path, "pid=1\n").unwrap();

        SingleInstanceGuard::acquire(path, ProductKind)
            .expect("stale lock file should not block a new client launch");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn running_instance_ipc_reports_status_and_disconnects_without_stopping_server() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::Builder::new()
            .prefix("lantunnel-control-")
            .tempdir_in("/tmp")
            .expect("temporary control directory");
        let socket_path = root.path().join("control.sock");
        let settings_path = root.path().join("settings.json");
        std::fs::write(&settings_path, "{}").expect("test settings");
        let engine_slot = Arc::new(RwLock::new(None));
        let control = LocalControlState {
            settings_path,
            engine: engine_slot.clone(),
            connect_op: None,
            local_proxy: Arc::new(RwLock::new(None)),
            desktop_tun: Arc::new(Mutex::new(DesktopTunState::default())),
            last_status: Arc::new(RwLock::new(ConnectionStatus {
                connected: true,
                message: "Connected".into(),
                gateway_addr: Some(
                    "https://operator:gateway-password@example.invalid/connect?token=gateway-token"
                        .into(),
                ),
                error: Some(
                    "request failed for https://peer:peer-password@example.invalid/status?api_key=peer-token"
                        .into(),
                ),
                platform_heartbeat: tp_client::status::HeartbeatStatus {
                    last_error: Some("platform bearer platform-secret".into()),
                    ..Default::default()
                },
                transport_heartbeat: tp_client::status::HeartbeatStatus {
                    last_error: Some("transport credential transport-secret".into()),
                    ..Default::default()
                },
                ..Default::default()
            })),
            status_generation: StatusGenerationGate::default(),
        };
        let server = tokio::spawn(serve_local_control(socket_path.clone(), control));
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists(), "control socket should become ready");
        assert_eq!(
            std::fs::metadata(&socket_path)
                .expect("control socket metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "local control socket must be owner-only"
        );

        let status = run_running_instance_command_at(
            RunningInstanceCommand::StatusJson,
            socket_path.clone(),
        )
        .await
        .expect("status command")
        .expect("status JSON");
        assert_eq!(status["connected"], true);
        let encoded = serde_json::to_string(&status).expect("status serialization");
        for forbidden in [
            "peer_private_key",
            "tunnel_signing_private_key",
            "membership_signature",
            "password",
            "gateway-token",
            "peer-token",
            "platform-secret",
            "transport-secret",
            "gateway_addr",
            "platform_heartbeat",
            "transport_heartbeat",
            "\"error\"",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "status JSON must not expose {forbidden}: {encoded}"
            );
        }

        assert_eq!(
            run_running_instance_command_at(
                RunningInstanceCommand::Disconnect,
                socket_path.clone(),
            )
            .await
            .expect("disconnect command"),
            None
        );
        assert!(
            !server.is_finished(),
            "disconnect must leave the process alive"
        );
        assert!(engine_slot.read().is_none());

        let status = run_running_instance_command_at(
            RunningInstanceCommand::StatusJson,
            socket_path.clone(),
        )
        .await
        .expect("status after disconnect")
        .expect("status JSON after disconnect");
        assert_eq!(status["connected"], false);

        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn headless_startup_control_reports_connecting_and_cancels_without_stopping_server() {
        let root = tempfile::Builder::new()
            .prefix("lantunnel-headless-control-")
            .tempdir_in("/tmp")
            .expect("temporary headless control directory");
        let socket_path = root.path().join("control.sock");
        let settings_path = root.path().join("settings.json");
        std::fs::write(&settings_path, "{}").expect("test settings");
        let runtime = HeadlessRuntimeState::begin(settings_path);
        let startup_cancel = runtime.cancel.clone();
        let mut server = tokio::spawn(serve_local_control(
            socket_path.clone(),
            runtime.control_state(),
        ));
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            if server.is_finished() {
                let outcome = (&mut server).await;
                panic!("headless control server exited early: {outcome:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            socket_path.exists(),
            "headless control socket was not ready"
        );

        let status = run_running_instance_command_at(
            RunningInstanceCommand::StatusJson,
            socket_path.clone(),
        )
        .await
        .expect("status during headless startup")
        .expect("headless startup status JSON");
        assert_eq!(status["connected"], false);
        assert_eq!(status["connecting"], true);
        assert_eq!(status["client_ui"]["overall"], "starting");
        assert!(status["client_ui"]["overall_reason_code"].is_null());
        assert!(status["client_ui"]["gateway_attachment"]["endpoint"].is_null());

        assert_eq!(
            run_running_instance_command_at(
                RunningInstanceCommand::Disconnect,
                socket_path.clone(),
            )
            .await
            .expect("disconnect headless startup"),
            None
        );
        assert!(startup_cancel.is_cancelled());
        assert!(!server.is_finished(), "disconnect must keep headless alive");

        let status = run_running_instance_command_at(
            RunningInstanceCommand::StatusJson,
            socket_path.clone(),
        )
        .await
        .expect("status after cancelling headless startup")
        .expect("cancelled headless startup status JSON");
        assert_eq!(status["connected"], false);
        assert_eq!(status["connecting"], false);

        server.abort();
        let _ = (&mut server).await;
    }

    #[test]
    fn lock_pid_parser_reads_current_lock_format() {
        assert_eq!(parse_lock_pid("pid=12345\n"), Some(12345));
        assert_eq!(parse_lock_pid("note=ignored\npid=678\n"), Some(678));
    }

    #[test]
    fn lock_pid_parser_rejects_invalid_values() {
        assert_eq!(parse_lock_pid("pid=\n"), None);
        assert_eq!(parse_lock_pid("pid=abc\n"), None);
        assert_eq!(parse_lock_pid("pid=0\n"), None);
        assert_eq!(parse_lock_pid(""), None);
    }

    #[test]
    fn replace_running_instance_message_warns_before_takeover() {
        let instance = RunningInstance {
            lock_path: PathBuf::from("client.lock"),
            pid: Some(42),
        };

        let message = replace_running_instance_message(ProductKind, &instance);

        assert!(message.contains("Lantunnel is already running"));
        assert!(message.contains("process ID: 42"));
        assert!(message.contains("Close the existing instance and start this one?"));
    }

    #[test]
    fn windows_tasklist_csv_pid_parser_reads_process_id() {
        assert_eq!(
            parse_windows_tasklist_csv_pid(
                "\"lantunnel-client.exe\",\"1234\",\"Console\",\"1\",\"45,672 K\""
            ),
            Some(1234)
        );
        assert_eq!(
            parse_windows_tasklist_csv_pid("INFO: No tasks are running"),
            None
        );
    }

    #[test]
    fn windows_tasklist_product_match_accepts_versioned_release_name() {
        assert_eq!(
            parse_windows_product_tasklist_pid(
                ProductKind,
                "\"lantunnel-client-2.0.0-windows-amd64.exe\",\"1234\",\"Console\",\"1\",\"45,672 K\""
            ),
            Some(1234)
        );
    }

    #[test]
    fn windows_tasklist_product_match_rejects_other_products() {
        assert_eq!(
            parse_windows_product_tasklist_pid(
                ProductKind,
                "\"unrelated-app.exe\",\"5678\",\"Console\",\"1\",\"45,672 K\""
            ),
            None
        );
    }

    #[test]
    fn windows_taskkill_failure_while_process_is_alive_escalates_to_force() {
        let soft = TaskkillResult {
            success: false,
            stdout: String::new(),
            stderr: "This process can only be terminated forcefully".into(),
        };

        assert_eq!(
            windows_termination_after_soft_taskkill(&soft, true),
            WindowsTerminationDecision::Force
        );
    }

    #[test]
    fn windows_taskkill_failure_after_process_exit_is_success() {
        let soft = TaskkillResult {
            success: false,
            stdout: String::new(),
            stderr: "not found".into(),
        };

        assert_eq!(
            windows_termination_after_soft_taskkill(&soft, false),
            WindowsTerminationDecision::Done
        );
    }

    #[test]
    fn already_running_message_names_the_product() {
        // The name on screen, not the bundle name: `productName` is asserted
        // separately and deliberately still says "Lantunnel Client".
        assert!(already_running_message(ProductKind).contains("Lantunnel"));
    }

    #[test]
    fn status_generation_gate_rejects_superseded_listener() {
        let gate = StatusGenerationGate::default();
        let first = gate.next_generation();

        assert!(gate.accepts(first));

        let second = gate.next_generation();

        assert!(!gate.accepts(first));
        assert!(gate.accepts(second));
    }

    #[test]
    fn connect_operation_slot_duplicate_is_rejected_while_active() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();

        let first = slot.begin(&gate);
        let second = slot.begin(&gate);

        assert_eq!(first, ConnectStartDecision::Started { generation: 1 });
        assert_eq!(
            second,
            ConnectStartDecision::AlreadyConnecting { generation: 1 }
        );
    }

    #[test]
    fn connect_operation_slot_finish_allows_next_connect() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();

        let ConnectStartDecision::Started { generation } = slot.begin(&gate) else {
            panic!("first connect should start");
        };
        slot.finish(generation);

        assert_eq!(
            slot.begin(&gate),
            ConnectStartDecision::Started { generation: 2 }
        );
    }

    #[test]
    fn connect_operation_slot_cancel_current_cancels_token() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();
        let ConnectStartDecision::Started { generation } = slot.begin(&gate) else {
            panic!("connect should start");
        };
        let token = slot
            .cancel_token(generation)
            .expect("token should exist for active generation");

        slot.cancel_current();

        assert!(token.is_cancelled());
        assert!(!slot.can_publish(generation));
    }

    #[test]
    fn connect_operation_slot_stale_generation_cannot_publish() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();
        let ConnectStartDecision::Started { generation } = slot.begin(&gate) else {
            panic!("connect should start");
        };

        assert!(!slot.can_publish(generation + 1));
    }

    #[test]
    fn connect_operation_slot_finishing_old_generation_keeps_newer_generation() {
        let mut slot = ConnectOperationSlot {
            current: Some(ConnectOperation {
                generation: 2,
                cancel: CancellationToken::new(),
            }),
        };

        slot.finish(1);

        assert!(slot.can_publish(2));
    }

    #[test]
    fn connect_publish_rejects_cancelled_generation() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();
        let ConnectStartDecision::Started { generation } = slot.begin(&gate) else {
            panic!("connect should start");
        };
        slot.cancel_current();
        let connect_op = Arc::new(Mutex::new(slot));
        let engine_slot = Arc::new(RwLock::new(None));
        let engine = Engine::new(EngineConfig::default(), Arc::new(NoUiListener));

        let rejected =
            publish_connected_engine_if_current(&connect_op, &engine_slot, generation, engine);

        assert!(rejected.is_err());
        assert!(engine_slot.read().is_none());
    }

    #[test]
    fn disconnect_cancel_prevents_inflight_connect_publish() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();
        let ConnectStartDecision::Started { generation } = slot.begin(&gate) else {
            panic!("connect should start");
        };
        slot.cancel_current();
        let connect_op = Arc::new(Mutex::new(slot));
        let engine_slot = Arc::new(RwLock::new(None));
        let engine = Engine::new(EngineConfig::default(), Arc::new(NoUiListener));

        let rejected =
            publish_connected_engine_if_current(&connect_op, &engine_slot, generation, engine);

        assert!(rejected.is_err(), "cancelled connect must not publish");
        assert!(engine_slot.read().is_none());
        assert!(
            !connect_op.lock().can_publish(generation),
            "disconnect cancellation must make the in-flight generation stale"
        );
    }

    #[test]
    fn connect_publish_stores_engine_and_finishes_current_generation() {
        let gate = StatusGenerationGate::default();
        let mut slot = ConnectOperationSlot::default();
        let ConnectStartDecision::Started { generation } = slot.begin(&gate) else {
            panic!("connect should start");
        };
        let connect_op = Arc::new(Mutex::new(slot));
        let engine_slot = Arc::new(RwLock::new(None));
        let engine = Engine::new(EngineConfig::default(), Arc::new(NoUiListener));

        assert!(
            publish_connected_engine_if_current(&connect_op, &engine_slot, generation, engine)
                .is_ok()
        );

        assert!(engine_slot.read().is_some());
        assert!(connect_op.lock().current.is_none());
    }

    #[tokio::test]
    async fn tun_local_proxy_wait_exits_promptly_when_cancelled() {
        let slot: LocalProxyTaskSlot = Arc::new(RwLock::new(None));
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_local_proxy_bound_cancellable(&slot, Duration::from_secs(15), Some(cancel)),
        )
        .await
        .expect("cancelled wait should not wait for readiness timeout");

        assert!(result
            .expect_err("wait should report cancellation")
            .contains("cancelled"));
    }

    #[tokio::test]
    async fn cleanup_cancelled_connect_stops_started_local_proxy() {
        let bound_addr = Arc::new(RwLock::new(Some(
            "127.0.0.1:12345"
                .parse()
                .expect("test socket addr should parse"),
        )));
        let slot: LocalProxyTaskSlot = Arc::new(RwLock::new(Some(LocalProxyTask {
            handle: tokio::spawn(std::future::pending::<()>()),
            bound_addr: bound_addr.clone(),
        })));
        let tun_slot: DesktopTunTaskSlot = Arc::new(Mutex::new(DesktopTunState::default()));

        cleanup_cancelled_connect(&slot, &tun_slot, None).await;

        assert!(slot.read().is_none());
        assert!(bound_addr.read().is_none());
    }

    #[test]
    fn v2_loopback_local_proxy_uses_no_auth_with_peer_identity() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            ..Default::default()
        };

        let auth =
            local_proxy_auth_mode_from_tunnel_config(&cfg, "127.0.0.1:1080".parse().unwrap())
                .expect("V2 loopback proxy must not need a shared secret");

        match auth {
            tp_proxy_socks5::AuthMode::NoAuth { group_id } => assert_eq!(group_id, "peer-v2"),
            tp_proxy_socks5::AuthMode::UserPass(_) => panic!("V2 has no shared-secret auth"),
        }
    }

    #[test]
    fn v2_local_proxy_refuses_a_non_loopback_listener() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            ..Default::default()
        };

        let error =
            match local_proxy_auth_mode_from_tunnel_config(&cfg, "0.0.0.0:1080".parse().unwrap()) {
                Ok(_) => panic!("V2 NoAuth must never bind outside loopback"),
                Err(error) => error,
            };

        assert!(error.contains("loopback"));
    }

    #[test]
    fn v2_profile_connect_preflight_rejects_non_loopback_before_runtime_start() {
        let error = validate_v2_local_proxy_listen(Some("192.168.1.20:1080".parse().unwrap()))
            .expect_err("V2 profile connect must fail before runtime start");

        assert!(error.contains("loopback"));
        assert!(validate_v2_local_proxy_listen(Some("[::1]:1080".parse().unwrap())).is_ok());
        assert!(validate_v2_local_proxy_listen(None).is_ok());
    }

    #[test]
    fn v2_desktop_tun_and_clash_do_not_require_shared_proxy_credentials() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            gateway_addr: "gateway.example.com".into(),
            ..Default::default()
        };

        assert!(desktop_tun_socks5_auth_from_tunnel_config(&cfg)
            .expect("V2 TUN must use loopback NoAuth")
            .is_none());
        let yaml = clash_overlay_yaml(
            &cfg,
            "127.0.0.1:1080".parse().unwrap(),
            "https://lantunnel.example",
        )
        .expect("V2 Clash config must render without legacy credentials");
        assert!(!yaml.contains("username:"));
        assert!(!yaml.contains("password:"));
    }

    #[test]
    fn clash_overlay_yaml_is_v2_loopback_no_auth() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            gateway_addr: "gateway.example.com".into(),
            ..Default::default()
        };

        let yaml = clash_overlay_yaml(
            &cfg,
            "127.0.0.1:1080".parse().unwrap(),
            "https://lantunnel.app",
        )
        .expect("clash overlay should render");

        assert!(yaml.contains("name: HomeLAN"));
        assert!(yaml.contains("type: socks5"));
        assert!(yaml.contains("server: 127.0.0.1"));
        assert!(yaml.contains("port: 1080"));
        assert!(!yaml.contains("username:"));
        assert!(!yaml.contains("password:"));
        assert!(yaml.contains("udp: true"));
        assert!(yaml.contains("DOMAIN,lantunnel.app,DIRECT"));
        assert!(yaml.contains("DOMAIN,gateway.example.com,DIRECT"));
        assert!(yaml.contains("Exclude Lantunnel from the Clash VPN"));
        assert!(yaml.contains("IP-CIDR,169.254.0.0/16,HomeLAN,no-resolve"));
    }

    #[test]
    fn clash_overlay_yaml_rejects_a_non_v2_tunnel_config() {
        let cfg = tp_client::TunnelConfig::default();

        let err = clash_overlay_yaml(
            &cfg,
            "127.0.0.1:1080".parse().unwrap(),
            DEFAULT_PLATFORM_URL,
        )
        .expect_err("missing V2 Peer identity should be rejected");

        assert!(err.contains("V2 Peer identity"));
    }

    #[test]
    fn clash_overlay_yaml_rejects_wildcard_listen_addr() {
        let cfg = tp_client::TunnelConfig {
            tunnel_id: "tunnel-v2".into(),
            peer_id: "peer-v2".into(),
            overlay_ipv4: "198.18.0.9".into(),
            ..Default::default()
        };

        let error = clash_overlay_yaml(
            &cfg,
            "0.0.0.0:1080".parse().unwrap(),
            "https://platform.example",
        )
        .expect_err("V2 Clash config must stay loopback-only");

        assert!(error.contains("loopback"));
    }

    #[test]
    fn local_proxy_listen_addr_accepts_wildcard_bind() {
        let settings = AppSettings {
            local_socks5_listen: "0.0.0.0:18080".into(),
            ..Default::default()
        };

        let listen = local_proxy_listen_addr(&settings)
            .expect("wildcard bind should parse")
            .expect("the Client should have a local proxy listen addr");

        assert_eq!(listen, "0.0.0.0:18080".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn local_proxy_listen_addr_uses_explicit_ingress_setting() {
        let mut settings = AppSettings {
            local_socks5_listen: "0.0.0.0:18080".into(),
            ..Default::default()
        };

        let listen = local_proxy_listen_addr(&settings)
            .expect("unified Peer listen should parse")
            .expect("Client-branded Peer may originate local traffic");
        assert_eq!(listen, "0.0.0.0:18080".parse::<SocketAddr>().unwrap());

        settings.local_proxy_enabled = false;
        let listen = local_proxy_listen_addr(&settings).expect("disabled ingress should parse");
        assert!(listen.is_none());
    }

    #[test]
    fn proxy_status_reports_configured_listen_addr() {
        let settings = AppSettings {
            local_socks5_listen: "127.0.0.1:18080".into(),
            ..Default::default()
        };

        let status =
            proxy_status_from_parts(true, &settings, true, vec!["192.168.0.0/16".to_string()]);

        assert!(status.running);
        assert_eq!(status.listen_addr, "127.0.0.1:18080");
        assert!(status.tun_running);
        assert_eq!(status.tun_routes, vec!["192.168.0.0/16".to_string()]);
    }

    #[tokio::test]
    async fn local_proxy_task_running_requires_bound_listener() {
        let bound_addr = Arc::new(RwLock::new(None));
        let task = tokio::spawn(std::future::pending::<()>());
        let slot = Arc::new(RwLock::new(Some(LocalProxyTask {
            handle: task,
            bound_addr: bound_addr.clone(),
        })));

        assert!(
            !local_proxy_task_running(&slot),
            "waiting local proxy task should not report SOCKS listener running"
        );

        *bound_addr.write() = Some("127.0.0.1:18080".parse::<SocketAddr>().unwrap());

        assert!(
            local_proxy_task_running(&slot),
            "bound local proxy task should report SOCKS listener running"
        );

        stop_local_proxy_task(&slot).await;
    }

    #[test]
    fn gui_log_lines_mask_sensitive_fields() {
        let line = r#"level=INFO tunnel_key="tk-secret" group_password=gp-secret tunnel_key: "debug-secret" tunnel_key = spaced-secret other=ok"#;

        let masked = mask_log_secrets(line);

        assert!(!masked.contains("tk-secret"));
        assert!(!masked.contains("gp-secret"));
        assert!(!masked.contains("debug-secret"));
        assert!(!masked.contains("spaced-secret"));
        assert!(masked.contains(r#"tunnel_key="***""#));
        assert!(masked.contains(r#"tunnel_key: "***""#));
        assert!(masked.contains("tunnel_key = ***"));
        assert!(masked.contains("group_password=***"));
        assert!(masked.contains("other=ok"));
    }

    #[test]
    fn gui_log_lines_strip_ansi_sequences_before_masking() {
        let line = "\u{1b}[3mtunnel_key\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"tk-secret\" \u{1b}[3mgroup_password\u{1b}[0m\u{1b}[2m=\u{1b}[0mgp-secret auth_username=\"client\"";

        let cleaned = clean_log_line(line);

        assert!(!cleaned.contains('\u{1b}'));
        assert!(!cleaned.contains("[3m"));
        assert!(!cleaned.contains("tk-secret"));
        assert!(!cleaned.contains("gp-secret"));
        assert!(cleaned.contains(r#"tunnel_key="***""#));
        assert!(cleaned.contains("group_password=***"));
        assert!(cleaned.contains(r#"auth_username="client""#));
    }

    #[test]
    fn desktop_tun_route_logs_expose_counts_not_route_values_or_errors() {
        let source = include_str!("main.rs");
        let start = source
            .find("fn start_desktop_dynamic_route_sync")
            .expect("dynamic route watcher");
        let end = source[start..]
            .find("async fn cleanup_cancelled_connect")
            .map(|offset| start + offset)
            .expect("route reconciliation end");
        let route_logging = &source[start..end];

        for forbidden in [
            "routes = ?lan_routes",
            "overlay_routes = ?overlay_routes",
            "learned_lan_routes = ?learned_lan_routes",
            "tracing::warn!(%error, \"desktop TUN dynamic route sync",
        ] {
            assert!(
                !route_logging.contains(forbidden),
                "desktop TUN logs must not expose route values or route-bearing errors: {forbidden}"
            );
        }
        for required in [
            "lan_route_count = lan_routes.len()",
            "overlay_route_count = overlay_routes.len()",
            "learned_lan_route_count = learned_lan_routes.len()",
            "learned_lan_route_source = \"identity-bound self-reported\"",
        ] {
            assert!(
                route_logging.contains(required),
                "desktop TUN logs must retain privacy-safe diagnostics: {required}"
            );
        }
    }

    #[test]
    fn desktop_tun_failure_logs_do_not_embed_route_errors() {
        let source = include_str!("main.rs");

        for marker in [
            "desktop TUN dynamic route sync failed; retrying",
            "desktop TUN dynamic route sync task failed",
            "desktop TUN dynamic route watcher join failed",
            "desktop LAN routes via TUN failed; cancelling connect and cleaning up",
            "desktop LAN routes via TUN failed; keeping SOCKS5 proxy connected",
        ] {
            let message_at = source.find(marker).expect("desktop TUN failure log");
            let log_start = source[..message_at]
                .rfind("tracing::warn!(")
                .expect("desktop TUN warning call");
            let log_call = &source[log_start..message_at + marker.len()];
            assert!(
                !log_call.contains("%error") && !log_call.contains("error ="),
                "route errors can contain full private addresses and must not be logged: {marker}"
            );
        }
    }
}

#[tauri::command]
fn validate_auto_start(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AutoStartValidation, String> {
    reconcile_auto_start(&app, state.product)
}

#[cfg(target_os = "windows")]
fn attach_parent_console_for_cli() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_parent_console_for_cli() {}

fn invocation_requires_parent_console(arg_count: usize) -> bool {
    arg_count > 1
}

// ----- main ----------------------------------------------------------------

/// Write a panic into the log the Logs tab reads, before the process dies.
///
/// The release profile sets panic = "abort", so a panic is a SIGABRT and the
/// default hook has written the message to stderr — which a window launched
/// from Finder does not have. Five identical crash reports carried nothing to
/// act on because of it. The hook appends the message and its location to the
/// same file everything else logs to, whether or not the tracing sink is up.
fn install_panic_logger(log_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with no message".to_string());
        let line = format!(
            "{}  PANIC lantunnel_client: {} at {}\n",
            chrono::Utc::now().to_rfc3339(),
            payload,
            location,
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(today_log_path(&log_dir))
        {
            let _ = file.write_all(line.as_bytes());
        }
        previous(info);
    }));
}

fn main() {
    let process_args: Vec<String> = std::env::args().collect();
    if invocation_requires_parent_console(process_args.len()) {
        attach_parent_console_for_cli();
    }
    let startup_args = StartupArgs::parse_from(process_args).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    if let Some(output) = startup_args.early_exit_output(ProductKind) {
        println!("{output}");
        return;
    }

    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    if let Err(e) = startup_args.validate_no_ui() {
        eprintln!("{e}");
        std::process::exit(2);
    }
    let product = ProductKind;
    if let Some(command) = startup_args.command.as_ref() {
        match run_startup_command(command, product) {
            Ok(summary) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summary)
                        .expect("public Peer summary must serialize")
                );
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(command) = startup_args.instance_command {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build local control runtime");
        match rt.block_on(run_running_instance_command_at(
            command,
            local_control_path(product),
        )) {
            Ok(Some(status)) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status)
                        .expect("running Client status must serialize")
                );
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    let single_instance_path = single_instance_lock_path(product);
    let _single_instance_guard =
        match SingleInstanceGuard::acquire(single_instance_path.clone(), product) {
            Ok(guard) => guard,
            Err(SingleInstanceError::AlreadyRunning(instance)) if !startup_args.no_ui => {
                if !confirm_replace_running_instance(product, &instance) {
                    return;
                }
                match acquire_after_replacing_running_instance(
                    single_instance_path,
                    product,
                    &instance,
                ) {
                    Ok(guard) => guard,
                    Err(e) => {
                        show_already_running_message(product, &e);
                        return;
                    }
                }
            }
            Err(e) => {
                let message = e.message(product);
                if startup_args.no_ui {
                    eprintln!("{message}");
                    std::process::exit(1);
                } else {
                    show_already_running_message(product, &message);
                    return;
                }
            }
        };

    let log_dir = config_dir(product);
    install_panic_logger(log_dir.clone());
    let (log_buffer, set_log_level_fn, log_level, _log_file_path, log_guard) =
        init_logging(&log_dir, product, startup_args.log_level.as_deref())
            .expect("failed to init logging");
    // Keep the non_blocking guard alive for the process lifetime.
    std::mem::forget(log_guard);
    tracing::info!(
        mode = "tauri",
        binary = product.binary_name(),
        log_file = %today_log_path(&log_dir).display(),
        level = %log_level.read(),
        "lantunnel-client log sink"
    );

    if startup_args.no_ui {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build no-ui runtime");
        if let Err(e) = rt.block_on(run_no_ui(product, startup_args)) {
            tracing::error!(error = %e, "lantunnel-client no-ui runtime exited");
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }

    let state = AppState {
        product,
        engine: Arc::new(RwLock::new(None)),
        connect_op: Arc::new(Mutex::new(ConnectOperationSlot::default())),
        local_proxy: Arc::new(RwLock::new(None)),
        desktop_tun: Arc::new(Mutex::new(DesktopTunState::default())),
        last_status: Arc::new(RwLock::new(ConnectionStatus {
            message: "Disconnected".into(),
            ..Default::default()
        })),
        status_generation: StatusGenerationGate::default(),
        log_buffer,
        set_log_level: set_log_level_fn,
        log_level,
        log_dir: log_dir.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .manage(state)
        .setup(move |app| {
            let control = LocalControlState::from_app(&app.state::<AppState>());
            let control_path = local_control_path(product);
            tauri::async_runtime::spawn(async move {
                if let Err(error) = serve_local_control(control_path, control).await {
                    tracing::error!(%error, "local Client control server stopped");
                }
            });

            // Reconcile autostart on launch: settings.json is the source of
            // truth, and the OS-level toggle (launchd/Run/.desktop) is brought
            // into sync.
            let handle = app.handle().clone();
            if let Err(e) = reconcile_auto_start(&handle, product) {
                tracing::warn!(error = %e, "autostart reconciliation failed");
            }

            let startup_settings =
                merge_product_defaults(read_settings_json(&settings_path(product)), product);
            let startup_auto_connect = startup_auto_connect_peer(
                &startup_settings,
                read_json(&last_peer_selection_path(product)),
            );

            // Build tray menu:
            //   Show / Hide (toggle) | Status (disabled) | Open Dashboard |
            //   Disconnect | Quit
            let show = MenuItemBuilder::with_id("show", "Show Window").build(app)?;
            let hide = MenuItemBuilder::with_id("hide", "Hide Window").build(app)?;
            let status = MenuItemBuilder::with_id("status", "Status: Disconnected")
                .enabled(false)
                .build(app)?;
            let open_dashboard =
                MenuItemBuilder::with_id("open_dashboard", "Open Dashboard").build(app)?;
            let disconnect = MenuItemBuilder::with_id("disconnect", "Disconnect").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&show)
                .item(&hide)
                .item(&PredefinedMenuItem::separator(app)?)
                .item(&status)
                .item(&open_dashboard)
                .item(&PredefinedMenuItem::separator(app)?)
                .item(&disconnect)
                .item(&PredefinedMenuItem::separator(app)?)
                .item(&quit)
                .build()?;

            let mut tray_builder = TrayIconBuilder::with_id("main")
                .tooltip(product.display_name())
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    "hide" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.hide();
                        }
                    }
                    "open_dashboard" => {
                        open_platform_dashboard(app);
                    }
                    "disconnect" => {
                        let handle = app.clone();
                        // A tray callback runs on the main thread, which is not
                        // inside the Tokio runtime. tokio::spawn panics there.
                        tauri::async_runtime::spawn(async move {
                            let (local_proxy_slot, desktop_tun_slot, status_generation, engine) = {
                                let state = handle.state::<AppState>();
                                state.connect_op.lock().cancel_current();
                                let engine = state.engine.write().take();
                                (
                                    state.local_proxy.clone(),
                                    state.desktop_tun.clone(),
                                    state.status_generation.clone(),
                                    engine,
                                )
                            };
                            disconnect_runtime(
                                local_proxy_slot,
                                desktop_tun_slot,
                                engine,
                                status_generation,
                            )
                            .await;
                        });
                    }
                    "quit" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let (local_proxy_slot, desktop_tun_slot, status_generation, engine) = {
                                let state = handle.state::<AppState>();
                                state.connect_op.lock().cancel_current();
                                let engine = state.engine.write().take();
                                (
                                    state.local_proxy.clone(),
                                    state.desktop_tun.clone(),
                                    state.status_generation.clone(),
                                    engine,
                                )
                            };
                            disconnect_runtime(
                                local_proxy_slot,
                                desktop_tun_slot,
                                engine,
                                status_generation,
                            )
                            .await;
                            handle.exit(0);
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(win) = tray.app_handle().get_webview_window("main") {
                            if win.is_visible().unwrap_or(false) {
                                let _ = win.hide();
                            } else {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray_builder = tray_builder.icon(icon);
            }
            let _tray = tray_builder.build(app)?;
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
            spawn_startup_auto_connect(app.handle().clone(), startup_auto_connect);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Intercept close and hide, so the tray keeps the app alive.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_peer_profiles,
            forget_peer_profile,
            import_peer_profile,
            connect_peer_profile,
            disconnect,
            get_status,
            get_proxy_status,
            get_clash_config,
            write_clipboard_text,
            get_settings,
            save_settings,
            set_auto_start,
            set_auto_connect,
            get_logs,
            clear_logs,
            set_log_level,
            get_log_config,
            get_log_file_path,
            get_product_info,
            get_capabilities,
            get_desktop_tun_capability,
            get_tun_helper_status,
            install_tun_helper,
            validate_auto_start,
        ])
        .run(tauri::generate_context!())
        .expect("error running tauri application");
}
