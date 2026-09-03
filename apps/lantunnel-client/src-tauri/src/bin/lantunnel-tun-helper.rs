#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CString, OsStr};
    use std::fs;
    use std::io::{self, BufRead, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use lantunnel_client::desktop_routes::{lan_route_specs, LanRouteSpec, MAX_HELPER_LAN_ROUTES};
    use lantunnel_client::desktop_tun::{self, DesktopTunSocks5Auth, RouteAction};
    use lantunnel_client::macos_tun_helper::{
        validate_start_request, TunDynamicRoutesRequest, TunHelperRequest, TunHelperResponse,
        TunOverlayRoutesRequest, TunStartRequest, DYNAMIC_ROUTES_CAPABILITY, HELPER_SOCKET_NAME,
    };

    const STARTUP_WAIT: Duration = Duration::from_millis(700);
    const ROUTE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
    const ROUTE_CLEANUP_ATTEMPTS: usize = 2;
    const ROUTE_CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(20);
    const SIDECAR_NAME: &str = "hev-socks5-tunnel";
    const SIDECAR_CODE_IDENTIFIER: &str = "hev-socks5-tunnel";
    const CONFIG_FILE_NAME: &str = "desktop-tun.yml";
    const RUNTIME_DIR: &str = "/var/run/lantunnel-tun";
    const APP_MAIN_BINARY: &str = "lantunnel-client";
    const APP_BUNDLE_IDENTIFIER: &str = "com.buhuipao.tunnel-proxy-app";
    /// Apple Team ID the privileged helper requires of whoever connects to it.
    ///
    /// This is a security control, not a build detail: the helper runs as root
    /// and only accepts a caller whose code signature carries this team. It has
    /// to be compiled in — a value read at runtime could be pointed at an
    /// attacker's team.
    ///
    /// A fork signs with its own team, so the value is overridable at build
    /// time via `LANTUNNEL_APPLE_TEAM_ID`. Leaving the upstream default in a
    /// fork's build means the helper would only trust upstream-signed binaries.
    /// The ID is not secret; it is recoverable from any signed release.
    const APP_DEVELOPER_TEAM_ID: &str = match option_env!("LANTUNNEL_APPLE_TEAM_ID") {
        Some(team) => team,
        None => "69VD3J69AA",
    };
    const K_CF_NUMBER_INT_TYPE: libc::c_int = 9;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_SEC_CS_DEFAULT_FLAGS: u32 = 0;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;

    type CFTypeRef = *const libc::c_void;
    type CFStringRef = *const libc::c_void;
    type CFDictionaryRef = *const libc::c_void;
    type CFMutableDictionaryRef = *mut libc::c_void;
    type CFIndex = libc::c_long;
    type SecCodeRef = *mut libc::c_void;
    type SecRequirementRef = *mut libc::c_void;
    type OSStatus = libc::c_int;

    static STATE: OnceLock<Mutex<HelperState>> = OnceLock::new();

    #[derive(Default)]
    struct HelperState {
        child: Option<Child>,
        runtime_id: String,
        tunnel_ipv4: Option<Ipv4Addr>,
        lan_routes: Vec<LanRouteSpec>,
        overlay_routes: Vec<LanRouteSpec>,
        learned_lan_routes: Vec<LanRouteSpec>,
        interface_name: String,
        config_path: Option<PathBuf>,
    }

    pub fn main() {
        if let Ok(listener) = launchd_listener() {
            serve_listener(listener);
            return;
        }
        serve_stdin();
    }

    fn serve_stdin() {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        for line in stdin.lock().lines() {
            let response = match line {
                Ok(line) => handle_line(&line),
                Err(e) => TunHelperResponse {
                    ok: false,
                    running: false,
                    message: format!("read request failed: {e}"),
                },
            };
            let _ = serde_json::to_writer(&mut stdout, &response);
            let _ = writeln!(stdout);
            let _ = stdout.flush();
        }
    }

    fn serve_listener(listener: UnixListener) {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let response = handle_stream(&mut stream);
                    let _ = serde_json::to_writer(&mut stream, &response);
                    let _ = writeln!(stream);
                    let _ = stream.flush();
                }
                Err(e) => {
                    eprintln!("accept helper connection failed: {e}");
                }
            }
        }
    }

    fn handle_stream(stream: &mut UnixStream) -> TunHelperResponse {
        if let Err(e) = validate_peer(stream) {
            return TunHelperResponse {
                ok: false,
                running: helper_running(),
                message: e,
            };
        }
        let mut line = String::new();
        let mut reader = match stream.try_clone() {
            Ok(stream) => io::BufReader::new(stream),
            Err(e) => {
                return TunHelperResponse {
                    ok: false,
                    running: helper_running(),
                    message: format!("clone helper connection failed: {e}"),
                }
            }
        };
        match reader.read_line(&mut line) {
            Ok(0) => TunHelperResponse {
                ok: false,
                running: helper_running(),
                message: "empty helper request".into(),
            },
            Ok(_) => handle_line(line.trim()),
            Err(e) => TunHelperResponse {
                ok: false,
                running: helper_running(),
                message: format!("read helper request failed: {e}"),
            },
        }
    }

    fn validate_peer(stream: &UnixStream) -> Result<(), String> {
        let (uid, _) = peer_ids(stream)?;
        let console_uid = fs::metadata("/dev/console")
            .map_err(|e| format!("read console owner failed: {e}"))?
            .uid();
        if uid != console_uid {
            return Err("helper request denied for non-console user".into());
        }
        let peer_pid = peer_pid(stream)?;
        let peer_path = process_path(peer_pid)?;
        let expected = expected_app_executable_path()
            .ok_or_else(|| "helper is not running from a bundled app".to_string())?;
        let expected = canonical_or_original(&expected);
        let peer_path = canonical_or_original(&peer_path);
        if peer_path != expected {
            return Err(format!(
                "helper request denied for unexpected client binary: {}",
                peer_path.display()
            ));
        }
        validate_peer_code_signature(peer_pid)
    }

    fn peer_pid(stream: &UnixStream) -> Result<libc::pid_t, String> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                SOL_LOCAL,
                LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if rc == 0 && pid > 0 {
            Ok(pid)
        } else {
            Err(format!(
                "read helper peer pid failed: {}",
                io::Error::last_os_error()
            ))
        }
    }

    fn process_path(pid: libc::pid_t) -> Result<PathBuf, String> {
        let mut buf = vec![0_u8; PROC_PIDPATHINFO_MAXSIZE];
        let len = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if len <= 0 {
            return Err(format!(
                "read helper peer process path failed: {}",
                io::Error::last_os_error()
            ));
        }
        buf.truncate(len as usize);
        Ok(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
    }

    fn validate_peer_code_signature(pid: libc::pid_t) -> Result<(), String> {
        let pid_number = unsafe {
            CFNumberCreate(
                std::ptr::null(),
                K_CF_NUMBER_INT_TYPE,
                (&pid as *const libc::pid_t).cast(),
            )
        };
        if pid_number.is_null() {
            return Err("helper request denied: could not build peer pid attribute".into());
        }

        let attributes = unsafe {
            CFDictionaryCreateMutable(std::ptr::null(), 1, std::ptr::null(), std::ptr::null())
        };
        if attributes.is_null() {
            unsafe { CFRelease(pid_number) };
            return Err("helper request denied: could not build peer code attributes".into());
        }

        unsafe {
            CFDictionarySetValue(attributes, kSecGuestAttributePid, pid_number);
        }

        let mut code: SecCodeRef = std::ptr::null_mut();
        let status = unsafe {
            SecCodeCopyGuestWithAttributes(
                std::ptr::null_mut(),
                attributes as CFDictionaryRef,
                K_SEC_CS_DEFAULT_FLAGS,
                &mut code,
            )
        };
        unsafe {
            CFRelease(attributes as CFTypeRef);
            CFRelease(pid_number);
        }
        if status != 0 || code.is_null() {
            return Err(format!(
                "helper request denied: could not inspect client code signature (OSStatus {status})"
            ));
        }

        let requirement_text = peer_code_requirement();
        let requirement = match create_code_requirement(&requirement_text, "helper request denied")
        {
            Ok(requirement) => requirement,
            Err(e) => {
                unsafe { CFRelease(code as CFTypeRef) };
                return Err(e);
            }
        };

        let status = unsafe { SecCodeCheckValidity(code, K_SEC_CS_DEFAULT_FLAGS, requirement) };
        unsafe {
            CFRelease(requirement as CFTypeRef);
            CFRelease(code as CFTypeRef);
        }
        if status == 0 {
            Ok(())
        } else {
            Err(format!(
                "helper request denied: client code signature does not match Lantunnel app requirement (OSStatus {status})"
            ))
        }
    }

    fn peer_code_requirement() -> String {
        developer_id_code_requirement(APP_BUNDLE_IDENTIFIER)
    }

    fn developer_id_code_requirement(identifier: &str) -> String {
        format!(
            "identifier \"{identifier}\" and anchor apple generic and certificate leaf[subject.OU] = \"{APP_DEVELOPER_TEAM_ID}\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
        )
    }

    fn create_code_requirement(
        requirement_text: &str,
        error_context: &str,
    ) -> Result<SecRequirementRef, String> {
        let requirement_cstr = CString::new(requirement_text)
            .map_err(|_| format!("{error_context}: invalid code requirement"))?;
        let requirement_text_ref = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                requirement_cstr.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if requirement_text_ref.is_null() {
            return Err(format!("{error_context}: could not build code requirement"));
        }

        let mut requirement: SecRequirementRef = std::ptr::null_mut();
        let status = unsafe {
            SecRequirementCreateWithString(
                requirement_text_ref,
                K_SEC_CS_DEFAULT_FLAGS,
                &mut requirement,
            )
        };
        unsafe { CFRelease(requirement_text_ref as CFTypeRef) };
        if status != 0 || requirement.is_null() {
            Err(format!(
                "{error_context}: invalid code requirement (OSStatus {status})"
            ))
        } else {
            Ok(requirement)
        }
    }

    fn peer_ids(stream: &UnixStream) -> Result<(u32, u32), String> {
        let mut uid = 0_u32;
        let mut gid = 0_u32;
        let rc = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc == 0 {
            Ok((uid, gid))
        } else {
            Err(format!(
                "read helper peer credentials failed: {}",
                io::Error::last_os_error()
            ))
        }
    }

    fn launchd_listener() -> Result<UnixListener, String> {
        let name = std::ffi::CString::new(HELPER_SOCKET_NAME)
            .map_err(|_| "helper socket name contains NUL byte".to_string())?;
        let mut fds: *mut libc::c_int = std::ptr::null_mut();
        let mut count: libc::size_t = 0;
        let rc = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
        if rc != 0 {
            return Err(format!("activate launchd helper socket failed: {rc}"));
        }
        if fds.is_null() || count == 0 {
            return Err("launchd did not provide a helper socket".into());
        }
        let fd = unsafe { *fds };
        unsafe { libc::free(fds.cast()) };
        Ok(unsafe { UnixListener::from_raw_fd(fd) })
    }

    pub fn handle_line(line: &str) -> TunHelperResponse {
        match serde_json::from_str::<TunHelperRequest>(line) {
            Ok(request) => handle_request(request),
            Err(e) => TunHelperResponse {
                ok: false,
                running: helper_running(),
                message: format!("invalid helper request: {e}"),
            },
        }
    }

    fn handle_request(request: TunHelperRequest) -> TunHelperResponse {
        match request {
            TunHelperRequest::Status => TunHelperResponse {
                ok: true,
                running: helper_running(),
                message: String::new(),
            },
            TunHelperRequest::Capabilities => TunHelperResponse {
                ok: true,
                running: helper_running(),
                message: DYNAMIC_ROUTES_CAPABILITY.into(),
            },
            TunHelperRequest::Start(request) => match start_tun(request) {
                Ok(()) => TunHelperResponse {
                    ok: true,
                    running: true,
                    message: String::new(),
                },
                Err(message) => TunHelperResponse {
                    ok: false,
                    running: helper_running(),
                    message,
                },
            },
            TunHelperRequest::SyncOverlayRoutes(request) => match sync_overlay_routes(request) {
                Ok(()) => TunHelperResponse {
                    ok: true,
                    running: true,
                    message: String::new(),
                },
                Err(message) => TunHelperResponse {
                    ok: false,
                    running: helper_running(),
                    message,
                },
            },
            TunHelperRequest::SyncDynamicRoutes(request) => match sync_dynamic_routes(request) {
                Ok(()) => TunHelperResponse {
                    ok: true,
                    running: true,
                    message: String::new(),
                },
                Err(message) => TunHelperResponse {
                    ok: false,
                    running: helper_running(),
                    message,
                },
            },
            TunHelperRequest::Stop(request) => match stop_tun(request.runtime_id) {
                Ok(()) => TunHelperResponse {
                    ok: true,
                    running: false,
                    message: String::new(),
                },
                Err(message) => TunHelperResponse {
                    ok: false,
                    running: helper_running(),
                    message,
                },
            },
        }
    }

    fn helper_running() -> bool {
        let mut state = state().lock().expect("helper state poisoned");
        child_running(&mut state)
    }

    fn child_running(state: &mut HelperState) -> bool {
        match state.child.as_mut() {
            Some(child) => child
                .try_wait()
                .map(|status| status.is_none())
                .unwrap_or(false),
            None => false,
        }
    }

    fn start_tun(request: TunStartRequest) -> Result<(), String> {
        validate_start_request(&request, MAX_HELPER_LAN_ROUTES)?;
        validate_helper_request(&request)?;
        let sidecar_path = expected_sidecar_path().ok_or_else(|| {
            "helper sidecar path could not be resolved from app bundle".to_string()
        })?;
        validate_sidecar_path(&sidecar_path)?;
        let config_path = runtime_config_path()?;
        let rendered_config = render_helper_config(&request)?;
        let tunnel_ipv4 = request
            .tunnel_ipv4
            .parse::<Ipv4Addr>()
            .map_err(|e| format!("TUN Overlay IPv4 is invalid: {e}"))?;
        let lan_routes = lan_route_specs(&request.routes, MAX_HELPER_LAN_ROUTES)
            .map_err(|e| format!("invalid LAN routes for helper: {e}"))?;
        let overlay_routes =
            desktop_tun::validated_remote_overlay_routes(tunnel_ipv4, &request.overlay_routes)?;
        let learned_lan_routes =
            desktop_tun::validated_learned_lan_routes(&request.learned_lan_routes)?;

        let mut state = state()
            .lock()
            .map_err(|_| "helper state poisoned".to_string())?;
        cleanup_state(&mut state)?;

        fs::write(&config_path, rendered_config)
            .map_err(|e| format!("write helper TUN config failed: {e}"))?;
        let stdout_path = runtime_log_path("stdout")?;
        let stderr_path = runtime_log_path("stderr")?;
        let stdout = log_file(&stdout_path)?;
        let stderr = log_file(&stderr_path)?;
        let mut child = Command::new(&sidecar_path)
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|e| format!("start TUN sidecar failed: {e}"))?;

        thread::sleep(STARTUP_WAIT);
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("check TUN sidecar startup failed: {e}"))?
        {
            let detail = sidecar_startup_detail(&stdout_path, &stderr_path);
            return Err(format!(
                "TUN sidecar exited during startup with status {status}. {detail}"
            ));
        }

        let mut owned_lan_routes = Vec::new();
        let mut owned_overlay_routes = Vec::new();
        let mut owned_learned_lan_routes = Vec::new();
        let startup_routes =
            apply_routes(&request.interface_name, &lan_routes, &mut owned_lan_routes)
                .and_then(|()| {
                    apply_routes(
                        &request.interface_name,
                        &overlay_routes,
                        &mut owned_overlay_routes,
                    )
                })
                .and_then(|()| {
                    apply_routes(
                        &request.interface_name,
                        &learned_lan_routes,
                        &mut owned_learned_lan_routes,
                    )
                });
        if let Err(error) = startup_routes {
            let interface_name = request.interface_name.clone();
            let cleanup_errors = desktop_tun::cleanup_owned_route_ledgers_with(
                &mut owned_lan_routes,
                &mut owned_overlay_routes,
                &mut owned_learned_lan_routes,
                ROUTE_CLEANUP_ATTEMPTS,
                ROUTE_CLEANUP_RETRY_DELAY,
                |action, route| run_route_action(action, &interface_name, route),
            );
            let remaining_route_count = owned_lan_routes.len()
                + owned_overlay_routes.len()
                + owned_learned_lan_routes.len();
            let child_stop_error = if remaining_route_count == 0 {
                stop_child(&mut child).err()
            } else {
                None
            };
            if remaining_route_count != 0 || child_stop_error.is_some() {
                state.child = Some(child);
                state.runtime_id = request.runtime_id;
                state.tunnel_ipv4 = Some(tunnel_ipv4);
                state.lan_routes = owned_lan_routes;
                state.overlay_routes = owned_overlay_routes;
                state.learned_lan_routes = owned_learned_lan_routes;
                state.interface_name = request.interface_name;
                state.config_path = Some(config_path);
                return Err(format!(
                    "{error}; startup cleanup incomplete (remaining_route_count={remaining_route_count}, cleanup_error_count={}, sidecar_stop_failed={})",
                    cleanup_errors.len(),
                    child_stop_error.is_some(),
                ));
            }
            let _ = fs::remove_file(&config_path);
            return Err(error);
        }

        state.child = Some(child);
        state.runtime_id = request.runtime_id;
        state.tunnel_ipv4 = Some(tunnel_ipv4);
        state.lan_routes = owned_lan_routes;
        state.overlay_routes = owned_overlay_routes;
        state.learned_lan_routes = owned_learned_lan_routes;
        state.interface_name = request.interface_name;
        state.config_path = Some(config_path);
        Ok(())
    }

    fn sync_overlay_routes(request: TunOverlayRoutesRequest) -> Result<(), String> {
        let mut state = state()
            .lock()
            .map_err(|_| "helper state poisoned".to_string())?;
        if !child_running(&mut state) {
            return Err("TUN helper cannot update routes while the sidecar is stopped".into());
        }
        require_runtime_owner(&state, &request.runtime_id)?;
        let tunnel_ipv4 = state
            .tunnel_ipv4
            .ok_or_else(|| "TUN helper has no active Overlay address".to_string())?;
        let desired = desktop_tun::validated_remote_overlay_routes(tunnel_ipv4, &request.routes)?;
        let additions = desired
            .iter()
            .filter(|route| {
                !state
                    .overlay_routes
                    .iter()
                    .any(|installed| installed.cidr == route.cidr)
            })
            .cloned()
            .collect::<Vec<_>>();
        let removals = state
            .overlay_routes
            .iter()
            .filter(|route| {
                !desired
                    .iter()
                    .any(|desired_route| desired_route.cidr == route.cidr)
            })
            .cloned()
            .collect::<Vec<_>>();

        let mut added = Vec::new();
        for route in additions {
            if let Err(error) = run_route_action(RouteAction::Add, &state.interface_name, &route) {
                let mut rollback_errors = Vec::new();
                for added_route in added.iter().rev() {
                    match run_route_action(RouteAction::Remove, &state.interface_name, added_route)
                    {
                        Ok(()) => state
                            .overlay_routes
                            .retain(|owned| owned.cidr != added_route.cidr),
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
            state.overlay_routes.push(route.clone());
            added.push(route);
        }

        let mut remove_errors = Vec::new();
        for route in removals {
            match run_route_action(RouteAction::Remove, &state.interface_name, &route) {
                Ok(()) => state
                    .overlay_routes
                    .retain(|owned| owned.cidr != route.cidr),
                Err(error) => remove_errors.push(error),
            }
        }
        state
            .overlay_routes
            .sort_by(|left, right| left.cidr.cmp(&right.cidr));
        if remove_errors.is_empty() {
            Ok(())
        } else {
            Err(remove_errors.join("; "))
        }
    }

    fn sync_dynamic_routes(request: TunDynamicRoutesRequest) -> Result<(), String> {
        let mut state = state()
            .lock()
            .map_err(|_| "helper state poisoned".to_string())?;
        if !child_running(&mut state) {
            return Err("TUN helper cannot update routes while the sidecar is stopped".into());
        }
        require_runtime_owner(&state, &request.runtime_id)?;
        let tunnel_ipv4 = state
            .tunnel_ipv4
            .ok_or_else(|| "TUN helper has no active Overlay address".to_string())?;
        let interface_name = state.interface_name.clone();
        let HelperState {
            overlay_routes,
            learned_lan_routes,
            ..
        } = &mut *state;
        desktop_tun::sync_dynamic_routes_with(
            tunnel_ipv4,
            overlay_routes,
            learned_lan_routes,
            &request.overlay_routes,
            &request.learned_lan_routes,
            |action, route| run_route_action(action, &interface_name, route),
        )
    }

    fn stop_tun(runtime_id: String) -> Result<(), String> {
        let mut state = state()
            .lock()
            .map_err(|_| "helper state poisoned".to_string())?;
        require_runtime_owner(&state, &runtime_id)?;
        cleanup_state(&mut state)
    }

    fn require_runtime_owner(state: &HelperState, runtime_id: &str) -> Result<(), String> {
        if runtime_id.is_empty() || state.runtime_id != runtime_id {
            Err("TUN helper request denied for stale runtime owner".into())
        } else {
            Ok(())
        }
    }

    fn cleanup_state(state: &mut HelperState) -> Result<(), String> {
        let interface_name = state.interface_name.clone();
        cleanup_state_routes_with(state, |action, route| {
            run_route_action(action, &interface_name, route)
        })?;
        if let Some(child) = state.child.as_mut() {
            stop_child(child)?;
        }
        state.child.take();
        if let Some(path) = state.config_path.take() {
            let _ = fs::remove_file(path);
        }
        state.runtime_id.clear();
        state.tunnel_ipv4 = None;
        state.lan_routes.clear();
        state.overlay_routes.clear();
        state.learned_lan_routes.clear();
        state.interface_name.clear();
        Ok(())
    }

    fn cleanup_state_routes_with<F>(state: &mut HelperState, route_action: F) -> Result<(), String>
    where
        F: FnMut(RouteAction, &LanRouteSpec) -> Result<(), String>,
    {
        let route_count =
            state.lan_routes.len() + state.overlay_routes.len() + state.learned_lan_routes.len();
        if route_count == 0 {
            return Ok(());
        }
        if state.interface_name.is_empty() {
            return Err("TUN route cleanup denied because interface ownership is missing".into());
        }
        let errors = desktop_tun::cleanup_owned_route_ledgers_with(
            &mut state.lan_routes,
            &mut state.overlay_routes,
            &mut state.learned_lan_routes,
            ROUTE_CLEANUP_ATTEMPTS,
            ROUTE_CLEANUP_RETRY_DELAY,
            route_action,
        );
        if state.lan_routes.is_empty()
            && state.overlay_routes.is_empty()
            && state.learned_lan_routes.is_empty()
        {
            Ok(())
        } else {
            Err(format!(
                "TUN route cleanup incomplete after {ROUTE_CLEANUP_ATTEMPTS} attempts (remaining_lan={}, remaining_overlay={}, remaining_learned_lan={}, errors={})",
                state.lan_routes.len(),
                state.overlay_routes.len(),
                state.learned_lan_routes.len(),
                errors.len(),
            ))
        }
    }

    fn stop_child(child: &mut Child) -> Result<(), String> {
        match child
            .try_wait()
            .map_err(|e| format!("check TUN sidecar state failed: {e}"))?
        {
            Some(_) => Ok(()),
            None => {
                child
                    .kill()
                    .map_err(|e| format!("stop TUN sidecar failed: {e}"))?;
                child
                    .wait()
                    .map_err(|e| format!("wait TUN sidecar failed: {e}"))?;
                Ok(())
            }
        }
    }

    fn validate_helper_request(request: &TunStartRequest) -> Result<(), String> {
        if request.interface_name != "utun24" {
            return Err("TUN helper only manages the Lantunnel utun24 interface".into());
        }
        let requested_sidecar = Path::new(&request.sidecar_path);
        let expected_sidecar = expected_sidecar_path().ok_or_else(|| {
            "helper sidecar path could not be resolved from app bundle".to_string()
        })?;
        if canonical_or_original(requested_sidecar) != canonical_or_original(&expected_sidecar) {
            return Err("TUN sidecar path must match the bundled helper sidecar".into());
        }
        Ok(())
    }

    fn validate_sidecar_path(sidecar_path: &Path) -> Result<(), String> {
        let metadata = fs::metadata(sidecar_path)
            .map_err(|e| format!("TUN sidecar path is not readable: {e}"))?;
        if !metadata.is_file() {
            return Err("TUN sidecar path must be a file".into());
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("TUN sidecar path must be executable".into());
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err("TUN sidecar must not be group/world writable".into());
        }

        validate_static_code_signature(sidecar_path, SIDECAR_CODE_IDENTIFIER, "TUN sidecar")?;

        Ok(())
    }

    fn validate_static_code_signature(
        path: &Path,
        expected_identifier: &str,
        label: &str,
    ) -> Result<(), String> {
        verify_static_code_with_codesign(path, label)?;
        let identity = codesign_signing_identity(path, label)?;
        validate_signing_identity(
            &identity.identifier,
            &identity.team_identifier,
            expected_identifier,
            label,
        )
    }

    fn verify_static_code_with_codesign(path: &Path, label: &str) -> Result<(), String> {
        let output = Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg("--strict")
            .arg("--verbose=2")
            .arg(path)
            .output()
            .map_err(|e| format!("{label} code signature verification could not run: {e}"))?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            Err(format!("{label} code signature is invalid"))
        } else {
            Err(format!("{label} code signature is invalid: {detail}"))
        }
    }

    struct SigningIdentity {
        identifier: String,
        team_identifier: String,
    }

    fn codesign_signing_identity(path: &Path, label: &str) -> Result<SigningIdentity, String> {
        let output = Command::new("/usr/bin/codesign")
            .arg("-dv")
            .arg("--verbose=4")
            .arg(path)
            .output()
            .map_err(|e| format!("{label} signing identity could not be inspected: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            return if detail.is_empty() {
                Err(format!("{label} signing identity could not be inspected"))
            } else {
                Err(format!(
                    "{label} signing identity could not be inspected: {detail}"
                ))
            };
        }

        let details = String::from_utf8_lossy(&output.stderr);
        let identifier = codesign_detail_value(&details, "Identifier")
            .ok_or_else(|| format!("{label} signing identifier is missing"))?;
        let team_identifier = codesign_detail_value(&details, "TeamIdentifier")
            .ok_or_else(|| format!("{label} signing team identifier is missing"))?;

        Ok(SigningIdentity {
            identifier,
            team_identifier,
        })
    }

    fn codesign_detail_value(details: &str, key: &str) -> Option<String> {
        let prefix = format!("{key}=");
        details
            .lines()
            .find_map(|line| line.strip_prefix(&prefix).map(|value| value.to_string()))
    }

    fn validate_signing_identity(
        identifier: &str,
        team_identifier: &str,
        expected_identifier: &str,
        label: &str,
    ) -> Result<(), String> {
        if identifier != expected_identifier {
            return Err(format!(
                "{label} signing identifier must be {expected_identifier}, got {identifier}"
            ));
        }
        if team_identifier != APP_DEVELOPER_TEAM_ID {
            return Err(format!(
                "{label} signing team must be {APP_DEVELOPER_TEAM_ID}, got {team_identifier}"
            ));
        }
        Ok(())
    }

    fn render_helper_config(request: &TunStartRequest) -> Result<String, String> {
        let socks5_addr = helper_socks5_addr(request)?;
        let tunnel_ipv4 = request
            .tunnel_ipv4
            .parse::<Ipv4Addr>()
            .map_err(|e| format!("TUN Overlay IPv4 is invalid: {e}"))?;
        let socks5_auth = request
            .socks5_auth
            .as_ref()
            .map(|auth| DesktopTunSocks5Auth {
                username: auth.username.clone(),
                password: auth.password.clone(),
            });
        Ok(desktop_tun::render_tun2socks_config_parts(
            &request.interface_name,
            tunnel_ipv4,
            socks5_addr,
            socks5_auth.as_ref(),
        ))
    }

    fn helper_socks5_addr(request: &TunStartRequest) -> Result<SocketAddr, String> {
        let ip = request
            .socks5_address
            .parse::<IpAddr>()
            .map_err(|e| format!("TUN SOCKS5 address is invalid: {e}"))?;
        if !ip.is_loopback() {
            return Err("TUN helper only connects to a loopback SOCKS5 proxy".into());
        }
        Ok(SocketAddr::new(ip, request.socks5_port))
    }

    fn expected_sidecar_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let launch_services = exe.parent()?;
        if launch_services.file_name() != Some(OsStr::new("LaunchServices")) {
            return None;
        }
        let library = launch_services.parent()?;
        if library.file_name() != Some(OsStr::new("Library")) {
            return None;
        }
        let contents = library.parent()?;
        Some(contents.join("Resources").join(SIDECAR_NAME))
    }

    fn expected_app_executable_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let launch_services = exe.parent()?;
        if launch_services.file_name() != Some(OsStr::new("LaunchServices")) {
            return None;
        }
        let library = launch_services.parent()?;
        if library.file_name() != Some(OsStr::new("Library")) {
            return None;
        }
        let contents = library.parent()?;
        Some(contents.join("MacOS").join(APP_MAIN_BINARY))
    }

    fn runtime_config_path() -> Result<PathBuf, String> {
        let dir = runtime_dir()?;
        Ok(dir.join(CONFIG_FILE_NAME))
    }

    fn runtime_log_path(stream: &str) -> Result<PathBuf, String> {
        let dir = runtime_dir()?;
        Ok(dir.join(format!("desktop-tun.{stream}.log")))
    }

    fn runtime_dir() -> Result<PathBuf, String> {
        let dir = PathBuf::from(RUNTIME_DIR);
        fs::create_dir_all(&dir).map_err(|e| format!("create helper runtime dir failed: {e}"))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("secure helper runtime dir failed: {e}"))?;
        Ok(dir)
    }

    fn canonical_or_original(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    fn apply_routes(
        interface_name: &str,
        routes: &[LanRouteSpec],
        owned: &mut Vec<LanRouteSpec>,
    ) -> Result<(), String> {
        desktop_tun::apply_routes_tracking_with(routes, owned, |action, route| {
            run_route_action(action, interface_name, route)
        })
    }

    fn run_route_action(
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

    fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
        run_command_with_timeout(program, args, ROUTE_COMMAND_TIMEOUT)
    }

    fn run_command_with_timeout(
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<(), String> {
        let mut child = Command::new(program)
            .args(args)
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

    fn log_file(path: &Path) -> Result<fs::File, String> {
        fs::OpenOptions::new()
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

    fn state() -> &'static Mutex<HelperState> {
        STATE.get_or_init(|| Mutex::new(HelperState::default()))
    }

    unsafe extern "C" {
        fn proc_pidpath(
            pid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: u32,
        ) -> libc::c_int;
        fn launch_activate_socket(
            name: *const libc::c_char,
            fds: *mut *mut libc::c_int,
            cnt: *mut libc::size_t,
        ) -> libc::c_int;
        fn getpeereid(fd: libc::c_int, euid: *mut u32, egid: *mut u32) -> libc::c_int;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFNumberCreate(
            allocator: CFTypeRef,
            the_type: libc::c_int,
            value_ptr: *const libc::c_void,
        ) -> CFTypeRef;
        fn CFStringCreateWithCString(
            allocator: CFTypeRef,
            c_str: *const libc::c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFDictionaryCreateMutable(
            allocator: CFTypeRef,
            capacity: CFIndex,
            key_callbacks: *const libc::c_void,
            value_callbacks: *const libc::c_void,
        ) -> CFMutableDictionaryRef;
        fn CFDictionarySetValue(
            the_dict: CFMutableDictionaryRef,
            key: *const libc::c_void,
            value: *const libc::c_void,
        );
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecGuestAttributePid: CFStringRef;

        fn SecCodeCopyGuestWithAttributes(
            host: SecCodeRef,
            attributes: CFDictionaryRef,
            flags: u32,
            guest: *mut SecCodeRef,
        ) -> OSStatus;
        fn SecRequirementCreateWithString(
            text: CFStringRef,
            flags: u32,
            requirement: *mut SecRequirementRef,
        ) -> OSStatus;
        fn SecCodeCheckValidity(
            code: SecCodeRef,
            flags: u32,
            requirement: SecRequirementRef,
        ) -> OSStatus;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn helper_rejects_invalid_json() {
            let response = handle_line("{");

            assert!(!response.ok);
            assert!(response.message.contains("invalid helper request"));
        }

        #[test]
        fn helper_reports_status() {
            let response = handle_line(r#""Status""#);

            assert!(response.ok);
        }

        #[test]
        fn failed_route_cleanup_preserves_runtime_owner_and_failed_ledger() {
            let mut state = HelperState {
                runtime_id: "runtime-owner".into(),
                interface_name: "utun24".into(),
                learned_lan_routes: lan_route_specs(&["192.168.70.11/32".into()], 1)
                    .expect("route"),
                ..HelperState::default()
            };

            cleanup_state_routes_with(&mut state, |_action, _route| {
                Err("injected route cleanup failure".into())
            })
            .expect_err("cleanup must preserve a failed OS route");

            assert_eq!(state.runtime_id, "runtime-owner");
            assert_eq!(state.interface_name, "utun24");
            assert_eq!(state.learned_lan_routes.len(), 1);

            cleanup_state_routes_with(&mut state, |_action, _route| Ok(()))
                .expect("same runtime can retry cleanup");
            assert!(state.learned_lan_routes.is_empty());
            assert_eq!(state.runtime_id, "runtime-owner");
            assert_eq!(state.interface_name, "utun24");
        }

        #[test]
        fn helper_reports_dynamic_route_capability() {
            let response = handle_line(r#""Capabilities""#);

            assert!(response.ok);
            assert_eq!(response.message, DYNAMIC_ROUTES_CAPABILITY);
        }

        #[test]
        fn helper_code_requirement_pins_bundle_and_team() {
            let requirement = peer_code_requirement();

            assert!(requirement.contains(APP_BUNDLE_IDENTIFIER));
            assert!(requirement.contains(APP_DEVELOPER_TEAM_ID));
            assert!(requirement.contains("anchor apple generic"));
            assert!(requirement.contains("1.2.840.113635.100.6.1.13"));
        }

        #[test]
        fn static_signing_identity_accepts_expected_identifier_and_team() {
            let result = validate_signing_identity(
                SIDECAR_CODE_IDENTIFIER,
                APP_DEVELOPER_TEAM_ID,
                SIDECAR_CODE_IDENTIFIER,
                "TUN sidecar",
            );

            assert!(result.is_ok());
        }

        #[test]
        fn static_signing_identity_rejects_wrong_identifier() {
            let error = validate_signing_identity(
                "com.example.other",
                APP_DEVELOPER_TEAM_ID,
                SIDECAR_CODE_IDENTIFIER,
                "TUN sidecar",
            )
            .unwrap_err();

            assert!(error.contains("signing identifier"));
        }

        #[test]
        fn static_signing_identity_rejects_wrong_team() {
            let error = validate_signing_identity(
                SIDECAR_CODE_IDENTIFIER,
                "WRONGTEAM",
                SIDECAR_CODE_IDENTIFIER,
                "TUN sidecar",
            )
            .unwrap_err();

            assert!(error.contains("signing team"));
        }

        #[test]
        fn codesign_detail_value_reads_identity_fields() {
            let details = "\
Executable=/tmp/hev-socks5-tunnel
Identifier=hev-socks5-tunnel
TeamIdentifier=69VD3J69AA
Runtime Version=26.5.0
";

            assert_eq!(
                codesign_detail_value(details, "Identifier").as_deref(),
                Some("hev-socks5-tunnel")
            );
            assert_eq!(
                codesign_detail_value(details, "TeamIdentifier").as_deref(),
                Some("69VD3J69AA")
            );
        }

        #[test]
        fn helper_renders_config_from_structured_request() {
            let request = TunStartRequest {
                runtime_id: "runtime-1".into(),
                interface_name: "utun24".into(),
                tunnel_ipv4: "198.18.44.7".into(),
                socks5_address: "127.0.0.1".into(),
                socks5_port: 1080,
                socks5_auth: Some(lantunnel_client::macos_tun_helper::TunSocks5Auth {
                    username: "group-1".into(),
                    password: "pa\"ss\\word".into(),
                }),
                sidecar_path: "/App.app/Contents/Resources/hev-socks5-tunnel".into(),
                routes: vec!["192.168.0.0/16".into()],
                overlay_routes: vec!["198.18.7.23/32".into()],
                learned_lan_routes: vec!["192.168.70.11/32".into()],
            };

            let rendered = render_helper_config(&request).unwrap();

            assert!(rendered.contains("name: \"utun24\""));
            assert!(rendered.contains("ipv4: 198.18.44.7"));
            assert!(rendered.contains("address: \"127.0.0.1\""));
            assert!(rendered.contains("port: 1080"));
            assert!(rendered.contains("username: \"group-1\""));
            assert!(rendered.contains("password: \"pa\\\"ss\\\\word\""));
        }

        #[test]
        fn helper_route_commands_have_a_bounded_runtime() {
            let error =
                run_command_with_timeout("/bin/sh", &["-c", "sleep 2"], Duration::from_millis(50))
                    .expect_err("hung privileged route command must be terminated");

            assert!(error.contains("timed out"));
        }

        #[test]
        fn helper_rejects_a_stale_runtime_owner() {
            let state = HelperState {
                runtime_id: "current-runtime".into(),
                ..HelperState::default()
            };

            assert!(require_runtime_owner(&state, "current-runtime").is_ok());
            assert!(require_runtime_owner(&state, "stale-runtime").is_err());
            assert!(require_runtime_owner(&state, "").is_err());
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("lantunnel TUN helper is available only on macOS");
}
