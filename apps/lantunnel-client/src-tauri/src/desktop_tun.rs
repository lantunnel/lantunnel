use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::desktop_routes::{lan_route_specs, overlay_route_specs, LanRouteSpec};

include!(concat!(env!("OUT_DIR"), "/sidecar_assets.rs"));

const TUN_MTU: u16 = 8500;
const SIDECAR_ENV: &str = "LANTUNNEL_TUN2SOCKS_BIN";
const LEGACY_SIDECAR_ENV: &str = "TUNNEL_PROXY_TUN2SOCKS_BIN";
const BUNDLED_SIDECAR_DIR: &str = "bin";
const TUN_PERMISSION_ERROR_CODE: &str = "TUN_PERMISSION_REQUIRED";
const ROUTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const ROUTE_CLEANUP_ATTEMPTS: usize = 2;
const ROUTE_CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct DesktopTunSocks5Auth {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct DesktopTunConfig {
    pub routes: Vec<String>,
    pub max_routes: usize,
    pub overlay_routes: Vec<String>,
    pub learned_lan_routes: Vec<String>,
    pub tunnel_ipv4: Ipv4Addr,
    pub socks5_addr: SocketAddr,
    pub socks5_auth: Option<DesktopTunSocks5Auth>,
    pub config_dir: PathBuf,
    pub resource_dir: Option<PathBuf>,
}

pub struct DesktopTunTask {
    backend: DesktopTunBackend,
    config_path: PathBuf,
    interface_name: String,
    tunnel_ipv4: Ipv4Addr,
    lan_routes: Vec<LanRouteSpec>,
    overlay_routes: Vec<LanRouteSpec>,
    learned_lan_routes: Vec<LanRouteSpec>,
    // Only the macOS helper backend reads this runtime ownership handle.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    helper_runtime_id: Option<String>,
    cleaned: bool,
}

enum DesktopTunBackend {
    Direct {
        child: Child,
    },
    #[cfg(target_os = "macos")]
    MacosHelper,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OverlayRouteDelta {
    add: Vec<LanRouteSpec>,
    remove: Vec<LanRouteSpec>,
}

fn overlay_route_delta(
    tunnel_ipv4: Ipv4Addr,
    installed: &[LanRouteSpec],
    desired_cidrs: &[String],
) -> Result<OverlayRouteDelta, String> {
    let desired = validated_remote_overlay_routes(tunnel_ipv4, desired_cidrs)?;

    let installed = installed
        .iter()
        .map(|route| (route.cidr.clone(), route))
        .collect::<BTreeMap<_, _>>();
    let desired = desired
        .into_iter()
        .map(|route| (route.cidr.clone(), route))
        .collect::<BTreeMap<_, _>>();

    Ok(OverlayRouteDelta {
        add: desired
            .iter()
            .filter(|(cidr, _)| !installed.contains_key(*cidr))
            .map(|(_, route)| route.clone())
            .collect(),
        remove: installed
            .iter()
            .filter(|(cidr, _)| !desired.contains_key(*cidr))
            .map(|(_, route)| (*route).clone())
            .collect(),
    })
}

pub fn validated_remote_overlay_routes(
    tunnel_ipv4: Ipv4Addr,
    desired_cidrs: &[String],
) -> Result<Vec<LanRouteSpec>, String> {
    overlay_route_specs(&[format!("{tunnel_ipv4}/32")])
        .map_err(|e| format!("invalid local Overlay address for TUN mode: {e}"))?;
    let desired = overlay_route_specs(desired_cidrs)
        .map_err(|e| format!("invalid remote Overlay routes for TUN mode: {e}"))?;
    let local_cidr = format!("{tunnel_ipv4}/32");
    if desired.iter().any(|route| route.cidr == local_cidr) {
        return Err(format!(
            "remote Overlay routes contain local TUN address {local_cidr}"
        ));
    }
    Ok(desired)
}

pub fn validated_learned_lan_routes(desired_cidrs: &[String]) -> Result<Vec<LanRouteSpec>, String> {
    let routes = lan_route_specs(desired_cidrs, desired_cidrs.len())
        .map_err(|e| format!("invalid learned V2 LAN routes for TUN mode: {e}"))?;
    for (raw, route) in desired_cidrs.iter().zip(&routes) {
        let network = route
            .network
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("invalid learned V2 LAN route {}", route.cidr))?;
        tp_client::peer_runtime::LanExportPrefixV2::new(network, route.prefix).map_err(|_| {
            format!(
                "invalid learned V2 LAN route {}: Export must be canonical RFC1918",
                route.cidr
            )
        })?;
        if raw.trim() != route.cidr {
            return Err(format!(
                "invalid learned V2 LAN route {raw}: Export must be canonical as {}",
                route.cidr
            ));
        }
    }
    Ok(routes)
}

#[cfg(test)]
fn sync_overlay_routes_with<F>(
    tunnel_ipv4: Ipv4Addr,
    installed: &mut Vec<LanRouteSpec>,
    desired_cidrs: &[String],
    mut route_action: F,
) -> Result<(), String>
where
    F: FnMut(RouteAction, &LanRouteSpec) -> Result<(), String>,
{
    let delta = overlay_route_delta(tunnel_ipv4, installed, desired_cidrs)?;
    let mut added = Vec::new();
    for route in delta.add {
        if let Err(error) = route_action(RouteAction::Add, &route) {
            let mut rollback_errors = Vec::new();
            for added_route in added.iter().rev() {
                match route_action(RouteAction::Remove, added_route) {
                    Ok(()) => installed.retain(|owned| owned.cidr != added_route.cidr),
                    Err(rollback_error) => rollback_errors.push(rollback_error),
                }
            }
            return if rollback_errors.is_empty() {
                Err(error)
            } else {
                Err(format!(
                    "{error}; Overlay route rollback failed: {}",
                    rollback_errors.join("; ")
                ))
            };
        }
        installed.push(route.clone());
        added.push(route);
    }

    let mut remove_errors = Vec::new();
    for route in delta.remove {
        match route_action(RouteAction::Remove, &route) {
            Ok(()) => installed.retain(|owned| owned.cidr != route.cidr),
            Err(error) => remove_errors.push(error),
        }
    }
    installed.sort_by(|left, right| left.cidr.cmp(&right.cidr));
    if remove_errors.is_empty() {
        Ok(())
    } else {
        Err(remove_errors.join("; "))
    }
}

pub fn sync_dynamic_routes_with<F>(
    tunnel_ipv4: Ipv4Addr,
    installed_overlays: &mut Vec<LanRouteSpec>,
    installed_learned_lan: &mut Vec<LanRouteSpec>,
    desired_overlay_cidrs: &[String],
    desired_learned_lan_cidrs: &[String],
    mut route_action: F,
) -> Result<(), String>
where
    F: FnMut(RouteAction, &LanRouteSpec) -> Result<(), String>,
{
    let desired_overlays = validated_remote_overlay_routes(tunnel_ipv4, desired_overlay_cidrs)?;
    let desired_learned_lan = validated_learned_lan_routes(desired_learned_lan_cidrs)?;
    let operations = desired_overlays
        .iter()
        .filter(|route| !installed_overlays.iter().any(|old| old.cidr == route.cidr))
        .map(|route| (RouteAction::Add, DynamicRouteSet::Overlay, route.clone()))
        .chain(
            desired_learned_lan
                .iter()
                .filter(|route| {
                    !installed_learned_lan
                        .iter()
                        .any(|old| old.cidr == route.cidr)
                })
                .map(|route| (RouteAction::Add, DynamicRouteSet::LearnedLan, route.clone())),
        )
        .chain(
            installed_overlays
                .iter()
                .filter(|route| !desired_overlays.iter().any(|new| new.cidr == route.cidr))
                .map(|route| (RouteAction::Remove, DynamicRouteSet::Overlay, route.clone())),
        )
        .chain(
            installed_learned_lan
                .iter()
                .filter(|route| !desired_learned_lan.iter().any(|new| new.cidr == route.cidr))
                .map(|route| {
                    (
                        RouteAction::Remove,
                        DynamicRouteSet::LearnedLan,
                        route.clone(),
                    )
                }),
        )
        .collect::<Vec<_>>();

    let mut applied = Vec::new();
    for (action, route_set, route) in operations {
        if let Err(error) = route_action(action, &route) {
            let mut rollback_errors = Vec::new();
            for (applied_action, applied_route_set, applied_route) in applied.iter().rev() {
                let rollback_action = match applied_action {
                    RouteAction::Add => RouteAction::Remove,
                    RouteAction::Remove => RouteAction::Add,
                };
                match route_action(rollback_action, applied_route) {
                    Ok(()) => record_dynamic_route_action(
                        rollback_action,
                        *applied_route_set,
                        applied_route,
                        installed_overlays,
                        installed_learned_lan,
                    ),
                    Err(rollback_error) => rollback_errors.push(rollback_error),
                }
            }
            sort_dynamic_route_ledger(installed_overlays, installed_learned_lan);
            return if rollback_errors.is_empty() {
                Err(error)
            } else {
                Err(format!(
                    "{error}; dynamic route rollback failed: {}",
                    rollback_errors.join("; ")
                ))
            };
        }
        record_dynamic_route_action(
            action,
            route_set,
            &route,
            installed_overlays,
            installed_learned_lan,
        );
        applied.push((action, route_set, route));
    }
    sort_dynamic_route_ledger(installed_overlays, installed_learned_lan);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicRouteSet {
    Overlay,
    LearnedLan,
}

fn record_dynamic_route_action(
    action: RouteAction,
    route_set: DynamicRouteSet,
    route: &LanRouteSpec,
    installed_overlays: &mut Vec<LanRouteSpec>,
    installed_learned_lan: &mut Vec<LanRouteSpec>,
) {
    let installed = match route_set {
        DynamicRouteSet::Overlay => installed_overlays,
        DynamicRouteSet::LearnedLan => installed_learned_lan,
    };
    match action {
        RouteAction::Add => {
            if !installed.iter().any(|owned| owned.cidr == route.cidr) {
                installed.push(route.clone());
            }
        }
        RouteAction::Remove => installed.retain(|owned| owned.cidr != route.cidr),
    }
}

fn sort_dynamic_route_ledger(
    installed_overlays: &mut [LanRouteSpec],
    installed_learned_lan: &mut [LanRouteSpec],
) {
    installed_overlays.sort_by(|left, right| left.cidr.cmp(&right.cidr));
    installed_learned_lan.sort_by(|left, right| left.cidr.cmp(&right.cidr));
}

/// Remove every route currently owned by one runtime, retiring a ledger entry
/// only after the OS confirms that exact Remove. Failed entries remain owned so
/// the same runtime can retry cleanup without guessing which forward actions
/// reached the kernel.
pub fn remove_owned_routes_with<F>(
    owned: &mut Vec<LanRouteSpec>,
    mut route_action: F,
) -> Vec<String>
where
    F: FnMut(RouteAction, &LanRouteSpec) -> Result<(), String>,
{
    let mut errors = Vec::new();
    for route in owned.clone().into_iter().rev() {
        match route_action(RouteAction::Remove, &route) {
            Ok(()) => owned.retain(|candidate| candidate.cidr != route.cidr),
            Err(error) => errors.push(error),
        }
    }
    errors
}

pub fn cleanup_owned_route_ledgers_with<F>(
    lan_routes: &mut Vec<LanRouteSpec>,
    overlay_routes: &mut Vec<LanRouteSpec>,
    learned_lan_routes: &mut Vec<LanRouteSpec>,
    attempts: usize,
    retry_delay: Duration,
    mut route_action: F,
) -> Vec<String>
where
    F: FnMut(RouteAction, &LanRouteSpec) -> Result<(), String>,
{
    let attempts = attempts.max(1);
    let mut errors = Vec::new();
    for attempt in 0..attempts {
        errors.clear();
        errors.extend(remove_owned_routes_with(
            learned_lan_routes,
            &mut route_action,
        ));
        errors.extend(remove_owned_routes_with(overlay_routes, &mut route_action));
        errors.extend(remove_owned_routes_with(lan_routes, &mut route_action));
        if lan_routes.is_empty() && overlay_routes.is_empty() && learned_lan_routes.is_empty() {
            return Vec::new();
        }
        if attempt + 1 < attempts {
            thread::sleep(retry_delay);
        }
    }
    errors
}

pub fn apply_routes_tracking_with<F>(
    routes: &[LanRouteSpec],
    owned: &mut Vec<LanRouteSpec>,
    mut route_action: F,
) -> Result<(), String>
where
    F: FnMut(RouteAction, &LanRouteSpec) -> Result<(), String>,
{
    for route in routes {
        route_action(RouteAction::Add, route)?;
        if !owned.iter().any(|candidate| candidate.cidr == route.cidr) {
            owned.push(route.clone());
        }
    }
    Ok(())
}

type ValidatedDesktopRoutes = (Vec<LanRouteSpec>, Vec<LanRouteSpec>, Vec<LanRouteSpec>);

fn validated_desktop_routes(config: &DesktopTunConfig) -> Result<ValidatedDesktopRoutes, String> {
    let lan_routes = lan_route_specs(&config.routes, config.max_routes)
        .map_err(|e| format!("invalid LAN routes for TUN mode: {e}"))?;
    let overlay_routes = overlay_route_delta(config.tunnel_ipv4, &[], &config.overlay_routes)?.add;
    let learned_lan_routes = validated_learned_lan_routes(&config.learned_lan_routes)?;
    Ok((lan_routes, overlay_routes, learned_lan_routes))
}

impl DesktopTunTask {
    pub fn start(config: DesktopTunConfig) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            crate::macos_tun_helper::start_desktop_tun(config)
        }

        #[cfg(not(target_os = "macos"))]
        {
            start_desktop_tun_direct(config)
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn macos_helper(
        config_path: PathBuf,
        interface_name: String,
        tunnel_ipv4: Ipv4Addr,
        lan_routes: Vec<LanRouteSpec>,
        overlay_routes: Vec<LanRouteSpec>,
        learned_lan_routes: Vec<LanRouteSpec>,
        helper_runtime_id: String,
    ) -> Self {
        Self {
            backend: DesktopTunBackend::MacosHelper,
            config_path,
            interface_name,
            tunnel_ipv4,
            lan_routes,
            overlay_routes,
            learned_lan_routes,
            helper_runtime_id: Some(helper_runtime_id),
            cleaned: false,
        }
    }

    pub fn stop(mut self) {
        self.cleanup();
    }

    pub fn is_running(&mut self) -> bool {
        match &mut self.backend {
            DesktopTunBackend::Direct { child } => child
                .try_wait()
                .map(|status| status.is_none())
                .unwrap_or(false),
            #[cfg(target_os = "macos")]
            DesktopTunBackend::MacosHelper => crate::macos_tun_helper::status()
                .map(|status| status.running)
                .unwrap_or(false),
        }
    }

    pub fn route_cidrs(&self) -> Vec<String> {
        self.lan_routes
            .iter()
            .chain(&self.overlay_routes)
            .chain(&self.learned_lan_routes)
            .map(|route| route.cidr.clone())
            .collect()
    }

    // Overlay-only entry point kept alongside sync_dynamic_routes; nothing calls
    // it today, so Linux reports it as dead while macOS does not.
    #[allow(dead_code)]
    pub fn sync_overlay_routes(&mut self, desired_cidrs: &[String]) -> Result<(), String> {
        let learned_lan_cidrs = self
            .learned_lan_routes
            .iter()
            .map(|route| route.cidr.clone())
            .collect::<Vec<_>>();
        self.sync_dynamic_routes(desired_cidrs, &learned_lan_cidrs)
    }

    pub fn sync_dynamic_routes(
        &mut self,
        desired_overlay_cidrs: &[String],
        desired_learned_lan_cidrs: &[String],
    ) -> Result<(), String> {
        match &mut self.backend {
            DesktopTunBackend::Direct { .. } => {
                let interface_name = self.interface_name.clone();
                sync_dynamic_routes_with(
                    self.tunnel_ipv4,
                    &mut self.overlay_routes,
                    &mut self.learned_lan_routes,
                    desired_overlay_cidrs,
                    desired_learned_lan_cidrs,
                    |action, route| run_route_action(action, &interface_name, route),
                )
            }
            #[cfg(target_os = "macos")]
            DesktopTunBackend::MacosHelper => {
                let overlay_delta = overlay_route_delta(
                    self.tunnel_ipv4,
                    &self.overlay_routes,
                    desired_overlay_cidrs,
                )?;
                let desired_learned_lan = validated_learned_lan_routes(desired_learned_lan_cidrs)?;
                let learned_lan_changed = desired_learned_lan != self.learned_lan_routes;
                if overlay_delta.add.is_empty()
                    && overlay_delta.remove.is_empty()
                    && !learned_lan_changed
                {
                    return Ok(());
                }
                let runtime_id = self
                    .helper_runtime_id
                    .as_deref()
                    .ok_or_else(|| "macOS TUN helper runtime ownership is missing".to_string())?;
                crate::macos_tun_helper::sync_dynamic_routes_blocking(
                    runtime_id,
                    desired_overlay_cidrs.to_vec(),
                    desired_learned_lan_cidrs.to_vec(),
                )?;
                self.overlay_routes = overlay_route_specs(desired_overlay_cidrs)
                    .map_err(|e| format!("invalid remote Overlay routes for TUN mode: {e}"))?;
                self.learned_lan_routes = desired_learned_lan;
                Ok(())
            }
        }
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        match &mut self.backend {
            DesktopTunBackend::Direct { child } => {
                let interface_name = self.interface_name.clone();
                let route_errors = cleanup_owned_route_ledgers_with(
                    &mut self.lan_routes,
                    &mut self.overlay_routes,
                    &mut self.learned_lan_routes,
                    ROUTE_CLEANUP_ATTEMPTS,
                    ROUTE_CLEANUP_RETRY_DELAY,
                    |action, route| run_route_action(action, &interface_name, route),
                );
                let routes_removed = self.lan_routes.is_empty()
                    && self.overlay_routes.is_empty()
                    && self.learned_lan_routes.is_empty();
                if !routes_removed {
                    tracing::warn!(
                        learned_lan_route_count = self.learned_lan_routes.len(),
                        overlay_route_count = self.overlay_routes.len(),
                        lan_route_count = self.lan_routes.len(),
                        cleanup_error_count = route_errors.len(),
                        learned_lan_route_source = "identity-bound self-reported",
                        cleanup_attempts = ROUTE_CLEANUP_ATTEMPTS,
                        "desktop TUN route cleanup incomplete after bounded retries"
                    );
                }
                let child_stopped = stop_child_bounded(child);
                self.cleaned = routes_removed && child_stopped;
            }
            #[cfg(target_os = "macos")]
            DesktopTunBackend::MacosHelper => {
                let result = self
                    .helper_runtime_id
                    .as_deref()
                    .ok_or_else(|| "macOS TUN helper runtime ownership is missing".to_string())
                    .and_then(crate::macos_tun_helper::stop_blocking);
                if let Err(e) = result {
                    tracing::warn!(error = %e, "macOS TUN helper stop failed");
                } else {
                    self.lan_routes.clear();
                    self.overlay_routes.clear();
                    self.learned_lan_routes.clear();
                    self.cleaned = true;
                }
            }
        }
        if let Err(e) = fs::remove_file(&self.config_path) {
            tracing::debug!(error = %e, path = %self.config_path.display(), "desktop TUN config cleanup skipped");
        }
    }
}

fn stop_child_bounded(child: &mut Child) -> bool {
    match child.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => {
            if let Err(error) = child.kill() {
                tracing::warn!(error = %error, "desktop TUN sidecar stop failed");
                return false;
            }
            match child.wait() {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "desktop TUN sidecar wait failed");
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "desktop TUN sidecar status check failed");
            false
        }
    }
}

fn start_desktop_tun_direct(config: DesktopTunConfig) -> Result<DesktopTunTask, String> {
    let (lan_routes, overlay_routes, learned_lan_routes) = validated_desktop_routes(&config)?;
    fs::create_dir_all(&config.config_dir)
        .map_err(|e| format!("create TUN config directory failed: {e}"))?;

    let interface_name = default_interface_name().to_string();
    let config_path = config.config_dir.join("desktop-tun.yml");
    let stdout_path = config.config_dir.join("desktop-tun.stdout.log");
    let stderr_path = config.config_dir.join("desktop-tun.stderr.log");
    let bundled_sidecar_dir = ensure_bundled_sidecars(&config.config_dir)?;
    let sidecar = resolve_sidecar_binary(
        config.resource_dir.as_deref(),
        bundled_sidecar_dir.as_deref(),
    );
    let body = render_tun2socks_config(&interface_name, &config);
    fs::write(&config_path, body).map_err(|e| format!("write TUN config failed: {e}"))?;
    let stdout = match log_file(&stdout_path) {
        Ok(file) => file,
        Err(e) => {
            let _ = fs::remove_file(&config_path);
            return Err(e);
        }
    };
    let stderr = match log_file(&stderr_path) {
        Ok(file) => file,
        Err(e) => {
            let _ = fs::remove_file(&config_path);
            return Err(e);
        }
    };

    let mut child = match Command::new(&sidecar)
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = fs::remove_file(&config_path);
            let permission_error = e.kind() == std::io::ErrorKind::PermissionDenied;
            let detail = format!(
                    "start TUN sidecar failed: {e}. Install/bundle hev-socks5-tunnel or set {SIDECAR_ENV}"
                );
            return Err(mark_tun_permission_error(
                append_privilege_hint_if_needed(detail, permission_error),
                permission_error,
            ));
        }
    };

    thread::sleep(Duration::from_millis(700));
    if let Some(status) = child
        .try_wait()
        .map_err(|e| format!("check TUN sidecar startup failed: {e}"))?
    {
        let _ = fs::remove_file(&config_path);
        let startup_detail = sidecar_startup_detail(&stdout_path, &stderr_path);
        let permission_error = is_tun_permission_error(&startup_detail);
        let detail = format!(
            "TUN sidecar exited during startup with status {status}. {}",
            startup_detail
        );
        return Err(mark_tun_permission_error(
            append_privilege_hint_if_needed(detail, permission_error),
            permission_error,
        ));
    }

    // Establish the ownership guard before the first Add reaches the kernel.
    // If startup later returns an error, `Drop` gets one final bounded cleanup
    // pass over anything the explicit rollback could not remove.
    let mut task = DesktopTunTask {
        backend: DesktopTunBackend::Direct { child },
        config_path,
        interface_name,
        tunnel_ipv4: config.tunnel_ipv4,
        lan_routes: Vec::new(),
        overlay_routes: Vec::new(),
        learned_lan_routes: Vec::new(),
        helper_runtime_id: None,
        cleaned: false,
    };
    let route_interface = task.interface_name.clone();
    let startup_routes = apply_routes(&route_interface, &lan_routes, &mut task.lan_routes)
        .and_then(|()| apply_routes(&route_interface, &overlay_routes, &mut task.overlay_routes))
        .and_then(|()| {
            apply_routes(
                &route_interface,
                &learned_lan_routes,
                &mut task.learned_lan_routes,
            )
        });
    if let Err(error) = startup_routes {
        task.cleanup();
        let remaining =
            task.lan_routes.len() + task.overlay_routes.len() + task.learned_lan_routes.len();
        return Err(startup_route_failure(error, remaining, task.cleaned));
    }

    Ok(task)
}

fn log_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| format!("open TUN sidecar log {} failed: {e}", path.display()))
}

fn sidecar_startup_detail(stdout_path: &Path, stderr_path: &Path) -> String {
    let mut parts = Vec::new();
    for (label, path) in [("stderr", stderr_path), ("stdout", stdout_path)] {
        if let Ok(raw) = fs::read_to_string(path) {
            let text = raw.trim();
            if !text.is_empty() {
                parts.push(format!("{label}: {}", truncate_for_error(text, 800)));
            }
        }
    }
    if parts.is_empty() {
        "No sidecar stderr/stdout captured".into()
    } else {
        parts.join("; ")
    }
}

fn truncate_for_error(value: &str, max_chars: usize) -> String {
    let mut out: String = value.chars().take(max_chars).collect();
    if out.len() < value.len() {
        out.push_str("...");
    }
    out
}

impl Drop for DesktopTunTask {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub(crate) fn ensure_bundled_sidecars(config_dir: &Path) -> Result<Option<PathBuf>, String> {
    // build.rs generates this list, so it is empty for every build that does
    // not bundle the Windows TUN sidecar and non-empty for the ones that do.
    #[allow(clippy::const_is_empty)]
    if BUNDLED_SIDECAR_ASSETS.is_empty() {
        return Ok(None);
    }

    let dir = config_dir.join(BUNDLED_SIDECAR_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("create bundled sidecar dir failed: {e}"))?;
    for asset in BUNDLED_SIDECAR_ASSETS {
        let path = dir.join(asset.name);
        let needs_write = fs::read(&path)
            .map(|existing| existing != asset.bytes)
            .unwrap_or(true);
        if needs_write {
            fs::write(&path, asset.bytes)
                .map_err(|e| format!("write bundled sidecar {} failed: {e}", path.display()))?;
        }
        #[cfg(unix)]
        if asset.name.ends_with(".exe") || !asset.name.ends_with(".dll") {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path)
                .map_err(|e| format!("stat bundled sidecar {} failed: {e}", path.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)
                .map_err(|e| format!("chmod bundled sidecar {} failed: {e}", path.display()))?;
        }
    }

    Ok(Some(dir))
}

pub(crate) fn resolve_sidecar_binary(
    resource_dir: Option<&Path>,
    bundled_sidecar_dir: Option<&Path>,
) -> PathBuf {
    for key in [SIDECAR_ENV, LEGACY_SIDECAR_ENV] {
        if let Ok(path) = std::env::var(key) {
            let path = path.trim();
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }

    let name = sidecar_binary_name();
    if let Some(dir) = bundled_sidecar_dir {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Some(resource_dir) = resource_dir {
        let candidate = resource_dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in sidecar_candidates(dir, name) {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }

    PathBuf::from(name)
}

fn sidecar_candidates(exe_dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = vec![exe_dir.join(name), exe_dir.join("resources").join(name)];
    if let Some(contents_dir) = exe_dir.parent() {
        out.push(contents_dir.join("Resources").join(name));
    }
    out
}

fn sidecar_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "hev-socks5-tunnel.exe"
    } else {
        "hev-socks5-tunnel"
    }
}

pub(crate) fn default_interface_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "utun24"
    } else if cfg!(target_os = "windows") {
        "Lantunnel"
    } else {
        "lantun0"
    }
}

pub fn render_tun2socks_config_parts(
    interface_name: &str,
    tunnel_ipv4: Ipv4Addr,
    socks5_addr: SocketAddr,
    socks5_auth: Option<&DesktopTunSocks5Auth>,
) -> String {
    let host = socks5_addr.ip();
    let port = socks5_addr.port();
    let mut out = format!(
        "tunnel:\n  name: {}\n  mtu: {TUN_MTU}\n  multi-queue: false\n  ipv4: {tunnel_ipv4}\nsocks5:\n  port: {port}\n  address: {}\n  udp: udp\n",
        yaml_quote(interface_name),
        yaml_quote(&host.to_string())
    );
    if let Some(auth) = socks5_auth {
        out.push_str(&format!(
            "  username: {}\n  password: {}\n",
            yaml_quote(&auth.username),
            yaml_quote(&auth.password)
        ));
    }
    out.push_str("misc:\n  task-stack-size: 131072\n  tcp-buffer-size: 65536\n  udp-recv-buffer-size: 4194304\n  udp-copy-buffer-nums: 64\n  connect-timeout: 5000\n  tcp-read-write-timeout: 300000\n  udp-read-write-timeout: 60000\n  log-level: warn\n");
    out
}

pub(crate) fn render_tun2socks_config(interface_name: &str, config: &DesktopTunConfig) -> String {
    render_tun2socks_config_parts(
        interface_name,
        config.tunnel_ipv4,
        config.socks5_addr,
        config.socks5_auth.as_ref(),
    )
}

fn yaml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn apply_routes(
    interface_name: &str,
    routes: &[LanRouteSpec],
    owned: &mut Vec<LanRouteSpec>,
) -> Result<(), String> {
    apply_routes_tracking_with(routes, owned, |action, route| {
        run_route_action(action, interface_name, route)
    })
    .map_err(|error| {
        let permission_error = is_tun_permission_error(&error);
        mark_tun_permission_error(
            append_privilege_hint_if_needed(error, permission_error),
            permission_error,
        )
    })
}

fn startup_route_failure(
    error: String,
    remaining_route_count: usize,
    cleanup_complete: bool,
) -> String {
    if cleanup_complete {
        error
    } else {
        format!(
            "{error}; startup cleanup incomplete (remaining_route_count={remaining_route_count}, ownership_guard_will_retry=true)"
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAction {
    Add,
    Remove,
}

fn run_route_action(
    action: RouteAction,
    interface_name: &str,
    route: &LanRouteSpec,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        run_macos_route_action(action, interface_name, route)
    }

    #[cfg(target_os = "linux")]
    {
        run_linux_route_action(action, interface_name, route)
    }

    #[cfg(target_os = "windows")]
    {
        run_windows_route_action(action, interface_name, route)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (action, interface_name, route);
        Err("desktop TUN routes are supported only on macOS, Windows, and Linux".into())
    }
}

#[cfg(target_os = "macos")]
fn run_macos_route_action(
    action: RouteAction,
    interface_name: &str,
    route: &LanRouteSpec,
) -> Result<(), String> {
    let action_arg = match action {
        RouteAction::Add => "add",
        RouteAction::Remove => "delete",
    };
    run_command(
        "/sbin/route",
        &[
            "-n",
            action_arg,
            "-net",
            &route.network,
            "-netmask",
            &route.netmask,
            "-interface",
            interface_name,
        ],
    )
}

#[cfg(target_os = "linux")]
fn run_linux_route_action(
    action: RouteAction,
    interface_name: &str,
    route: &LanRouteSpec,
) -> Result<(), String> {
    let ip = if Path::new("/sbin/ip").is_file() {
        "/sbin/ip"
    } else {
        "ip"
    };
    let args = linux_route_action_args(action, interface_name, route);
    run_command(ip, &args)
}

#[cfg(any(target_os = "linux", test))]
fn linux_route_action_args<'a>(
    action: RouteAction,
    interface_name: &'a str,
    route: &'a LanRouteSpec,
) -> [&'a str; 5] {
    let action_arg = match action {
        // `add` deliberately fails if another route already owns this prefix.
        // Replacing it would make cleanup delete a route owned by another runtime.
        RouteAction::Add => "add",
        RouteAction::Remove => "del",
    };
    ["route", action_arg, &route.cidr, "dev", interface_name]
}

/// PowerShell for the Windows route helpers.
///
/// Compiled and tested on every platform even though it only runs on Windows:
/// the property that matters — no caller-supplied text is ever spliced into
/// the script — should not be verifiable on Windows alone.
///
/// `route.cidr` is always re-rendered from parsed integers and the interface
/// alias is generated locally, so neither is attacker controlled today. A
/// command assembled by string interpolation is still one refactor away from
/// being an injection point, so the values travel through the environment
/// instead; PowerShell does not re-parse `$env:` values as code.
#[cfg(any(target_os = "windows", test))]
const WINDOWS_ROUTE_PREFIX_VAR: &str = "LANTUNNEL_ROUTE_PREFIX";
#[cfg(any(target_os = "windows", test))]
const WINDOWS_ROUTE_ALIAS_VAR: &str = "LANTUNNEL_ROUTE_ALIAS";

#[cfg(any(target_os = "windows", test))]
fn windows_route_script(action: RouteAction) -> &'static str {
    match action {
        RouteAction::Add => concat!(
            "$prefix = $env:LANTUNNEL_ROUTE_PREFIX; ",
            "$alias = $env:LANTUNNEL_ROUTE_ALIAS; ",
            "$old = Get-NetRoute -DestinationPrefix $prefix -InterfaceAlias $alias ",
            "-ErrorAction SilentlyContinue; ",
            "if ($old) { $old | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue }; ",
            "New-NetRoute -DestinationPrefix $prefix -InterfaceAlias $alias ",
            "-NextHop '0.0.0.0' -PolicyStore ActiveStore -ErrorAction Stop",
        ),
        RouteAction::Remove => concat!(
            "$prefix = $env:LANTUNNEL_ROUTE_PREFIX; ",
            "$alias = $env:LANTUNNEL_ROUTE_ALIAS; ",
            "Get-NetRoute -DestinationPrefix $prefix -InterfaceAlias $alias ",
            "-ErrorAction SilentlyContinue | ",
            "Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue",
        ),
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_route_env<'a>(cidr: &'a str, interface_name: &'a str) -> [(&'static str, &'a str); 2] {
    [
        (WINDOWS_ROUTE_PREFIX_VAR, cidr),
        (WINDOWS_ROUTE_ALIAS_VAR, interface_name),
    ]
}

#[cfg(target_os = "windows")]
fn run_windows_route_action(
    action: RouteAction,
    interface_name: &str,
    route: &LanRouteSpec,
) -> Result<(), String> {
    run_command_with_env(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            windows_route_script(action),
        ],
        &windows_route_env(route.cidr.as_str(), interface_name),
    )
}

fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    run_command_with_timeout(program, args, ROUTE_COMMAND_TIMEOUT, &[])
}

#[cfg(target_os = "windows")]
fn run_command_with_env(program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
    run_command_with_timeout(program, args, ROUTE_COMMAND_TIMEOUT, env)
}

fn run_command_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
    env: &[(&str, &str)],
) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program} spawn failed: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} timed out after {} ms",
                    timeout.as_millis()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} wait failed: {error}"));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{program} output collection failed: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if detail.is_empty() {
        Err(format!("{program} exited with status {}", output.status))
    } else {
        Err(format!(
            "{program} exited with status {}: {detail}",
            output.status
        ))
    }
}

fn mark_tun_permission_error(detail: String, permission_error: bool) -> String {
    if permission_error {
        format!("{TUN_PERMISSION_ERROR_CODE}: {detail}")
    } else {
        detail
    }
}

fn append_privilege_hint_if_needed(detail: String, permission_error: bool) -> String {
    if permission_error {
        format!("{detail}. {}", privilege_hint())
    } else {
        detail
    }
}

fn is_tun_permission_error(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("operation not permitted")
        || lower.contains("permission denied")
        || lower.contains("must be root")
        || lower.contains("not have permissions")
}

fn privilege_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS requires administrator approval for utun and route changes"
    } else if cfg!(target_os = "windows") {
        "Windows requires administrator approval plus the Wintun driver/DLL"
    } else if cfg!(target_os = "linux") {
        "Linux requires root or CAP_NET_ADMIN for /dev/net/tun and routes"
    } else {
        "administrator approval is required"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_route_add_never_replaces_an_existing_owner() {
        let route = LanRouteSpec {
            cidr: "192.168.70.11/32".into(),
            network: "192.168.70.11".into(),
            prefix: 32,
            netmask: "255.255.255.255".into(),
        };

        assert_eq!(
            linux_route_action_args(RouteAction::Add, "lantun0", &route),
            ["route", "add", "192.168.70.11/32", "dev", "lantun0"]
        );
        assert_eq!(
            linux_route_action_args(RouteAction::Remove, "lantun0", &route),
            ["route", "del", "192.168.70.11/32", "dev", "lantun0"]
        );
    }

    #[test]
    fn learned_lan_cleanup_log_reports_count_without_route_bearing_error() {
        let source = include_str!("desktop_tun.rs");
        let marker = "desktop TUN route cleanup incomplete after bounded retries";
        let message_at = source.find(marker).expect("learned LAN cleanup log");
        let log_start = source[..message_at]
            .rfind("tracing::warn!(")
            .expect("learned LAN cleanup tracing call");
        let log_call = &source[log_start..message_at + marker.len()];

        assert!(
            !log_call.contains("error = %e"),
            "route-command errors can contain the complete learned private address"
        );
        assert!(
            log_call.contains("learned_lan_route_count = self.learned_lan_routes.len()"),
            "cleanup diagnostics should retain only the learned route count"
        );
        assert!(
            log_call.contains("learned_lan_route_source = \"identity-bound self-reported\""),
            "cleanup diagnostics should retain the fixed learned-route source"
        );
    }

    #[test]
    fn tun2socks_config_threads_socks5_auth() {
        let config = DesktopTunConfig {
            routes: vec!["192.168.0.0/16".into()],
            max_routes: 1,
            overlay_routes: Vec::new(),
            learned_lan_routes: Vec::new(),
            tunnel_ipv4: "198.18.44.7".parse().unwrap(),
            socks5_addr: "127.0.0.1:1080".parse().unwrap(),
            socks5_auth: Some(DesktopTunSocks5Auth {
                username: "group-1".into(),
                password: "pa\"ss\\word".into(),
            }),
            config_dir: PathBuf::from("/tmp/lantunnel-test"),
            resource_dir: None,
        };

        let rendered = render_tun2socks_config("lantun0", &config);

        assert!(rendered.contains("name: \"lantun0\""));
        assert!(rendered.contains("ipv4: 198.18.44.7"));
        assert!(rendered.contains("address: \"127.0.0.1\""));
        assert!(rendered.contains("port: 1080"));
        assert!(rendered.contains("username: \"group-1\""));
        assert!(rendered.contains("password: \"pa\\\"ss\\\\word\""));
    }

    #[test]
    fn tun2socks_config_omits_socks5_auth_when_disabled() {
        let config = DesktopTunConfig {
            routes: vec!["192.168.0.0/16".into()],
            max_routes: 1,
            overlay_routes: Vec::new(),
            learned_lan_routes: Vec::new(),
            tunnel_ipv4: "198.18.1.9".parse().unwrap(),
            socks5_addr: "127.0.0.1:1080".parse().unwrap(),
            socks5_auth: None,
            config_dir: PathBuf::from("/tmp/lantunnel-test"),
            resource_dir: None,
        };

        let rendered = render_tun2socks_config("lantun0", &config);

        assert!(rendered.contains("address: \"127.0.0.1\""));
        assert!(rendered.contains("port: 1080"));
        assert!(!rendered.contains("username:"));
        assert!(!rendered.contains("password:"));
    }

    #[test]
    fn overlay_route_sync_is_sparse_and_idempotent() {
        let local = "198.18.1.9".parse().unwrap();
        let desired = vec!["198.18.7.23/32".into(), "198.18.200.9/32".into()];

        let first = overlay_route_delta(local, &[], &desired).expect("valid sparse routes");
        assert_eq!(
            first
                .add
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["198.18.200.9/32", "198.18.7.23/32"]
        );
        assert!(first.remove.is_empty());

        let replay = overlay_route_delta(local, &first.add, &desired).expect("idempotent replay");
        assert!(replay.add.is_empty());
        assert!(replay.remove.is_empty());
    }

    #[test]
    fn learned_lan_routes_accept_rfc1918_exact_hosts() {
        let routes = validated_learned_lan_routes(&[
            "10.23.45.67/32".into(),
            "172.20.30.40/32".into(),
            "192.168.70.11/32".into(),
        ])
        .expect("identity-bound self-reported RFC1918 host aliases");

        assert_eq!(
            routes
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["10.23.45.67/32", "172.20.30.40/32", "192.168.70.11/32"]
        );
    }

    #[test]
    fn learned_lan_routes_accept_canonical_rfc1918_subnets() {
        let routes = validated_learned_lan_routes(&[
            "10.20.0.0/16".into(),
            "172.20.30.0/24".into(),
            "192.168.70.0/24".into(),
        ])
        .expect("V2 LAN Exports are canonical RFC1918 prefixes");

        assert_eq!(
            routes
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["10.20.0.0/16", "172.20.30.0/24", "192.168.70.0/24"]
        );
    }

    #[test]
    fn learned_lan_routes_reject_noncanonical_subnets() {
        let error = validated_learned_lan_routes(&["192.168.70.44/24".into()])
            .expect_err("Gossip/native route input must keep the canonical Export prefix");

        assert!(error.contains("canonical as 192.168.70.0/24"));
    }

    #[test]
    fn learned_lan_routes_reject_link_local_hosts() {
        let error = validated_learned_lan_routes(&["169.254.7.8/32".into()])
            .expect_err("learned aliases are restricted to RFC1918");

        assert!(error.contains("RFC1918"));
    }

    #[test]
    fn learned_lan_routes_reject_public_hosts() {
        let error = validated_learned_lan_routes(&["8.8.8.8/32".into()])
            .expect_err("public hosts must never be captured as learned LAN aliases");

        assert!(error.contains("private IPv4 LAN"));
    }

    #[test]
    fn dynamic_route_sync_tracks_overlay_and_learned_lan_separately() {
        let local = "198.18.1.9".parse().unwrap();
        let mut overlays = Vec::new();
        let mut learned_lan = Vec::new();
        let mut actions = Vec::new();

        sync_dynamic_routes_with(
            local,
            &mut overlays,
            &mut learned_lan,
            &["198.18.7.23/32".into()],
            &["192.168.70.11/32".into()],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            actions,
            vec![
                (RouteAction::Add, "198.18.7.23/32".into()),
                (RouteAction::Add, "192.168.70.11/32".into()),
            ]
        );
        assert_eq!(overlays[0].cidr, "198.18.7.23/32");
        assert_eq!(learned_lan[0].cidr, "192.168.70.11/32");
    }

    #[test]
    fn dynamic_route_sync_captures_an_ambiguous_alias_key() {
        let local = "198.18.1.9".parse().unwrap();
        let mut overlays = Vec::new();
        let mut learned_lan = Vec::new();
        let mut actions = Vec::new();

        // Engine exposes one known key even when that key has multiple owners.
        // TUN must capture it; the later exact matcher remains fail-closed.
        sync_dynamic_routes_with(
            local,
            &mut overlays,
            &mut learned_lan,
            &[],
            &["192.168.70.11/32".into()],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(actions, vec![(RouteAction::Add, "192.168.70.11/32".into())]);
        assert_eq!(learned_lan[0].cidr, "192.168.70.11/32");
    }

    #[test]
    fn dynamic_route_sync_removes_retired_routes_from_both_sets() {
        let local = "198.18.1.9".parse().unwrap();
        let mut overlays =
            validated_remote_overlay_routes(local, &["198.18.7.23/32".into()]).unwrap();
        let mut learned_lan = validated_learned_lan_routes(&["192.168.70.11/32".into()]).unwrap();
        let mut actions = Vec::new();

        sync_dynamic_routes_with(
            local,
            &mut overlays,
            &mut learned_lan,
            &[],
            &[],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            actions,
            vec![
                (RouteAction::Remove, "198.18.7.23/32".into()),
                (RouteAction::Remove, "192.168.70.11/32".into()),
            ]
        );
        assert!(overlays.is_empty());
        assert!(learned_lan.is_empty());
    }

    #[test]
    fn dynamic_route_sync_rolls_back_the_whole_update_on_failure() {
        let local = "198.18.1.9".parse().unwrap();
        let mut overlays =
            validated_remote_overlay_routes(local, &["198.18.7.23/32".into()]).unwrap();
        let mut learned_lan = validated_learned_lan_routes(&["192.168.70.0/24".into()]).unwrap();
        let mut actions = Vec::new();

        let error = sync_dynamic_routes_with(
            local,
            &mut overlays,
            &mut learned_lan,
            &["198.18.8.24/32".into()],
            &["10.1.0.0/16".into()],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                if action == RouteAction::Remove && route.cidr == "198.18.7.23/32" {
                    Err("injected route removal failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("a partial dynamic update must fail");

        assert!(error.contains("injected route removal failure"));
        assert_eq!(
            actions,
            vec![
                (RouteAction::Add, "198.18.8.24/32".into()),
                (RouteAction::Add, "10.1.0.0/16".into()),
                (RouteAction::Remove, "198.18.7.23/32".into()),
                (RouteAction::Remove, "10.1.0.0/16".into()),
                (RouteAction::Remove, "198.18.8.24/32".into()),
            ]
        );
        assert_eq!(overlays[0].cidr, "198.18.7.23/32");
        assert_eq!(learned_lan[0].cidr, "192.168.70.0/24");
    }

    #[test]
    fn dynamic_route_ledger_tracks_partial_state_when_rollback_fails() {
        let local = "198.18.1.9".parse().unwrap();
        let mut overlays =
            validated_remote_overlay_routes(local, &["198.18.7.23/32".into()]).unwrap();
        let mut learned_lan = validated_learned_lan_routes(&["192.168.70.11/32".into()]).unwrap();

        let error = sync_dynamic_routes_with(
            local,
            &mut overlays,
            &mut learned_lan,
            &["198.18.8.24/32".into()],
            &["10.1.2.3/32".into()],
            |action, route| {
                let injected_failure = (action == RouteAction::Remove
                    && route.cidr == "192.168.70.11/32")
                    || (action == RouteAction::Add && route.cidr == "198.18.7.23/32")
                    || (action == RouteAction::Remove && route.cidr == "10.1.2.3/32");
                if injected_failure {
                    Err("injected route transaction failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("a failed rollback must fail the update");

        assert!(error.contains("dynamic route rollback failed"));
        assert!(
            overlays.is_empty(),
            "the old Overlay route was removed and its rollback Add failed"
        );
        assert_eq!(
            learned_lan
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["10.1.2.3/32", "192.168.70.11/32"],
            "the failed rollback Remove remains runtime-owned for later cleanup"
        );
    }

    #[test]
    fn owned_route_cleanup_retires_only_successes_and_retries_failures() {
        let mut owned = validated_learned_lan_routes(&[
            "10.1.2.3/32".into(),
            "172.20.3.4/32".into(),
            "192.168.70.11/32".into(),
        ])
        .unwrap();
        let mut fail_once = true;

        let first = remove_owned_routes_with(&mut owned, |_action, route| {
            if route.cidr == "172.20.3.4/32" && fail_once {
                fail_once = false;
                Err("injected cleanup failure".into())
            } else {
                Ok(())
            }
        });

        assert_eq!(first.len(), 1);
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].cidr, "172.20.3.4/32");

        let second = remove_owned_routes_with(&mut owned, |_action, _route| Ok(()));
        assert!(second.is_empty());
        assert!(owned.is_empty());
    }

    #[test]
    fn startup_route_apply_leaves_successful_adds_in_ownership_ledger() {
        let routes =
            validated_learned_lan_routes(&["10.1.2.3/32".into(), "172.20.3.4/32".into()]).unwrap();
        let mut owned = Vec::new();

        let error = apply_routes_tracking_with(&routes, &mut owned, |_action, route| {
            if route.cidr == "172.20.3.4/32" {
                Err("injected add failure".into())
            } else {
                Ok(())
            }
        })
        .expect_err("second Add must fail");

        assert_eq!(error, "injected add failure");
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].cidr, "10.1.2.3/32");
    }

    #[test]
    fn overlay_route_sync_applies_add_remove_deltas_and_tracks_owned_routes() {
        let local = "198.18.1.9".parse().unwrap();
        let mut installed = Vec::new();
        let mut actions = Vec::new();

        sync_overlay_routes_with(
            local,
            &mut installed,
            &["198.18.7.23/32".into(), "198.18.200.9/32".into()],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                Ok(())
            },
        )
        .unwrap();
        sync_overlay_routes_with(
            local,
            &mut installed,
            &["198.18.7.23/32".into(), "198.18.200.9/32".into()],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                Ok(())
            },
        )
        .unwrap();
        sync_overlay_routes_with(
            local,
            &mut installed,
            &["198.18.200.9/32".into()],
            |action, route| {
                actions.push((action, route.cidr.clone()));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            actions,
            vec![
                (RouteAction::Add, "198.18.200.9/32".into()),
                (RouteAction::Add, "198.18.7.23/32".into()),
                (RouteAction::Remove, "198.18.7.23/32".into()),
            ]
        );
        assert_eq!(
            installed
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["198.18.200.9/32"]
        );
    }

    #[test]
    fn overlay_routes_do_not_consume_the_user_lan_route_quota() {
        let config = DesktopTunConfig {
            routes: vec!["192.168.0.0/16".into()],
            max_routes: 1,
            overlay_routes: vec!["198.18.7.23/32".into(), "198.18.200.9/32".into()],
            learned_lan_routes: Vec::new(),
            tunnel_ipv4: "198.18.1.9".parse().unwrap(),
            socks5_addr: "127.0.0.1:1080".parse().unwrap(),
            socks5_auth: None,
            config_dir: PathBuf::from("/tmp/lantunnel-test"),
            resource_dir: None,
        };

        let (lan, overlay, learned_lan) =
            validated_desktop_routes(&config).expect("independent route sets");

        assert_eq!(lan.len(), 1);
        assert_eq!(overlay.len(), 2);
        assert!(learned_lan.is_empty());
    }

    #[test]
    fn learned_lan_routes_are_independent_from_static_lan_and_overlay_routes() {
        let config = DesktopTunConfig {
            routes: vec!["192.168.0.0/16".into()],
            max_routes: 1,
            overlay_routes: vec!["198.18.7.23/32".into()],
            learned_lan_routes: vec!["10.23.45.67/32".into(), "172.20.30.40/32".into()],
            tunnel_ipv4: "198.18.1.9".parse().unwrap(),
            socks5_addr: "127.0.0.1:1080".parse().unwrap(),
            socks5_auth: None,
            config_dir: PathBuf::from("/tmp/lantunnel-test"),
            resource_dir: None,
        };

        let (lan, overlay, learned_lan) =
            validated_desktop_routes(&config).expect("three independent route sets");

        assert_eq!(lan.len(), 1);
        assert_eq!(overlay.len(), 1);
        assert_eq!(learned_lan.len(), 2);
    }

    #[test]
    fn local_tun_address_must_belong_to_the_overlay_pool() {
        let error =
            validated_remote_overlay_routes("8.8.8.8".parse().unwrap(), &Vec::<String>::new())
                .expect_err("public local TUN address must fail closed");

        assert!(error.contains("198.18.0.0/16"));
    }

    #[cfg(unix)]
    #[test]
    fn route_commands_have_a_bounded_runtime() {
        let error = run_command_with_timeout(
            "/bin/sh",
            &["-c", "sleep 2"],
            Duration::from_millis(50),
            &[],
        )
        .expect_err("hung route command must be terminated");

        assert!(error.contains("timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn route_command_timeout_wrapper_preserves_success() {
        run_command_with_timeout(
            "/bin/sh",
            &["-c", "printf route-ok"],
            Duration::from_secs(1),
            &[],
        )
        .expect("successful route command");

        // Values handed to a child through the environment must arrive intact,
        // including characters that would terminate a quoted shell string.
        run_command_with_timeout(
            "/bin/sh",
            &["-c", r#"test "$LANTUNNEL_TEST_VALUE" = "a'b\"c \$(id)""#],
            Duration::from_secs(1),
            &[("LANTUNNEL_TEST_VALUE", r#"a'b"c $(id)"#)],
        )
        .expect("environment values reach the child verbatim");
    }

    #[test]
    fn windows_route_scripts_never_splice_caller_text() {
        for action in [RouteAction::Add, RouteAction::Remove] {
            let script = windows_route_script(action);
            // Both values are read from the environment and nothing else is
            // interpolated, so no caller string can reach the parser.
            assert!(script.contains("$env:LANTUNNEL_ROUTE_PREFIX"));
            assert!(script.contains("$env:LANTUNNEL_ROUTE_ALIAS"));
            // `{` appears as PowerShell block syntax, so the thing to rule out
            // is an unfilled Rust format placeholder, not the brace itself.
            assert!(!script.contains("{}"), "no format placeholder may survive");
            // Neither the prefix nor the alias is quoted into the script, so a
            // quote character has nothing to escape out of.
            assert!(!script.contains("'$prefix"));
            assert!(!script.contains("'$alias"));
        }
    }

    #[test]
    fn windows_route_env_carries_the_values_verbatim() {
        let hostile = r#"'; Start-Process calc; '"#;
        let env = windows_route_env("10.0.0.0/8", hostile);
        assert_eq!(env[0], ("LANTUNNEL_ROUTE_PREFIX", "10.0.0.0/8"));
        assert_eq!(env[1], ("LANTUNNEL_ROUTE_ALIAS", hostile));
        // The payload stays data: it is never concatenated into the script.
        assert!(!windows_route_script(RouteAction::Add).contains(hostile));
    }

    #[test]
    fn sidecar_candidates_include_macos_resources_sibling() {
        let candidates =
            sidecar_candidates(Path::new("/App.app/Contents/MacOS"), "hev-socks5-tunnel");

        assert!(candidates.contains(&PathBuf::from(
            "/App.app/Contents/Resources/hev-socks5-tunnel"
        )));
    }

    #[test]
    fn sidecar_candidates_include_explicit_resource_dir() {
        let temp_dir =
            std::env::temp_dir().join(format!("lantunnel-sidecar-test-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).expect("create temp resource dir");
        let sidecar = temp_dir.join(sidecar_binary_name());
        fs::write(&sidecar, b"test").expect("write sidecar marker");

        assert_eq!(resolve_sidecar_binary(Some(&temp_dir), None), sidecar);

        let _ = fs::remove_file(&sidecar);
        let _ = fs::remove_dir(&temp_dir);
    }

    #[test]
    fn bundled_sidecars_extract_under_product_config_bin_dir() {
        let config_dir = Path::new("/home/example/.lantunnel/app");

        assert_eq!(config_dir.join(BUNDLED_SIDECAR_DIR), config_dir.join("bin"));
    }

    #[test]
    fn marks_tun_permission_errors_for_user_guidance() {
        let marked = mark_tun_permission_error(
            "socks5 tunnel open (Operation not permitted). macOS requires administrator approval"
                .into(),
            true,
        );

        assert!(marked.starts_with(TUN_PERMISSION_ERROR_CODE));
    }

    #[test]
    fn leaves_non_permission_tun_errors_unmarked() {
        let detail = "invalid LAN routes for TUN mode: route is outside private LAN ranges";

        assert_eq!(mark_tun_permission_error(detail.into(), false), detail);
    }

    #[test]
    fn generic_privilege_hint_does_not_mark_non_permission_errors() {
        let detail = format!("TUN sidecar exited during startup. {}", privilege_hint());

        assert!(!is_tun_permission_error(&detail));
    }

    #[test]
    fn privilege_hint_is_only_appended_to_permission_errors() {
        let non_permission = append_privilege_hint_if_needed("bad sidecar config".into(), false);
        assert_eq!(non_permission, "bad sidecar config");

        let permission = append_privilege_hint_if_needed("Operation not permitted".into(), true);
        assert!(permission.contains(privilege_hint()));
    }
}
