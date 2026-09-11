//! Platform account calls for the two phone Clients.
//!
//! The desktop reaches `tp_client::platform_account` directly. A phone reaches
//! it through here, because the shared React UI is the same bundle on all three
//! and the account panel has to answer on all three or it is not a capability,
//! it is a difference.
//!
//! Two deliberate shapes:
//!
//! - **Stateless.** Nothing here remembers a token. The phone stores it where
//!   its own platform keeps secrets — the Keychain on iOS, app-private
//!   preferences on Android, next to the Peer profile that already carries a
//!   private key — and passes it back in on every call. A second copy cached in
//!   Rust would be a second thing to wipe on sign-out.
//! - **Blocking.** Every export runs its future to completion on a small
//!   dedicated runtime. Callers must invoke these off the UI thread; both
//!   bridges already answer late for the picker, the scanner and connect, so
//!   the mechanism to do that exists.

use std::ffi::c_char;
use std::sync::OnceLock;

use tp_client::platform_account::{DeviceLoginPoll, PlatformAccountApi, PlatformToken};
use zeroize::Zeroizing;

use crate::{result_string_to_c_ptr, string_from_c, MobileError};

/// One small runtime for short request/response calls.
///
/// Separate from the proxy runtime on purpose: signing in must not depend on a
/// tunnel being up, and a stalled HTTP request must not occupy a worker the
/// data path needs.
fn account_runtime() -> Result<&'static tokio::runtime::Runtime, MobileError> {
    static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .thread_name("tp-mobile-account")
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| MobileError::start_failed("could not start the Platform account runtime"))
}

fn api(platform_url: &str) -> Result<PlatformAccountApi, MobileError> {
    PlatformAccountApi::new(platform_url).map_err(|e| MobileError::invalid_config(e.to_string()))
}

fn token(access_token: String, expires_at_unix: u64) -> PlatformToken {
    PlatformToken {
        access_token: Zeroizing::new(access_token),
        expires_at_unix,
    }
}

pub(crate) fn start_sign_in(platform_url: &str) -> Result<String, MobileError> {
    let api = api(platform_url)?;
    let started = account_runtime()?
        .block_on(api.start_device_login())
        .map_err(|e| MobileError::start_failed(e.to_string()))?;
    Ok(serde_json::json!({
        // Secret, and the caller needs it to poll. It never leaves the app.
        "device_code": started.device_code.as_str(),
        "user_code": started.user_code,
        "verification_uri": started.verification_uri,
        "verification_uri_complete": started.verification_uri_complete,
        "expires_in": started.expires_in,
        // Never poll faster than told, and never busier than once a second.
        "interval": started.interval.max(1),
    })
    .to_string())
}

pub(crate) fn poll_sign_in(platform_url: &str, device_code: &str) -> Result<String, MobileError> {
    let api = api(platform_url)?;
    let runtime = account_runtime()?;
    let outcome = runtime
        .block_on(api.poll_device_login(device_code))
        .map_err(|e| MobileError::start_failed(e.to_string()))?;
    let json = match outcome {
        DeviceLoginPoll::Pending => serde_json::json!({ "state": "pending" }),
        // Not an error: the Platform is asking for a slower cadence. Collapsing
        // it into `pending` would keep the same cadence and never finish, which
        // is exactly what RFC 8628 §3.5 exists to prevent.
        DeviceLoginPoll::SlowDown => serde_json::json!({ "state": "slow_down" }),
        DeviceLoginPoll::Denied => serde_json::json!({ "state": "denied" }),
        DeviceLoginPoll::Expired => serde_json::json!({ "state": "expired" }),
        DeviceLoginPoll::Granted(granted) => {
            // Naming the account is worth one extra call: a Client signed into
            // the wrong account otherwise looks identical to the right one.
            let email = runtime
                .block_on(api.identity(&granted))
                .ok()
                .map(|identity| identity.email);
            serde_json::json!({
                "state": "granted",
                "access_token": granted.access_token.as_str(),
                "expires_at_unix": granted.expires_at_unix,
                "email": email,
            })
        }
    };
    Ok(json.to_string())
}

pub(crate) fn list_tunnels(
    platform_url: &str,
    access_token: String,
    expires_at_unix: u64,
) -> Result<String, MobileError> {
    let api = api(platform_url)?;
    let tunnels = account_runtime()?
        .block_on(api.list_tunnels(&token(access_token, expires_at_unix)))
        .map_err(|e| MobileError::start_failed(e.to_string()))?;
    serde_json::to_string(&tunnels).map_err(|e| MobileError::invalid_json(e.to_string()))
}

pub(crate) fn list_peers(
    platform_url: &str,
    access_token: String,
    expires_at_unix: u64,
    tunnel_id: &str,
) -> Result<String, MobileError> {
    let api = api(platform_url)?;
    let peers = account_runtime()?
        .block_on(api.list_peers(&token(access_token, expires_at_unix), tunnel_id))
        .map_err(|e| MobileError::start_failed(e.to_string()))?;
    serde_json::to_string(&peers).map_err(|e| MobileError::invalid_json(e.to_string()))
}

pub(crate) fn import_peer(
    platform_url: &str,
    access_token: String,
    expires_at_unix: u64,
    tunnel_id: &str,
    peer_id: &str,
) -> Result<String, MobileError> {
    let api = api(platform_url)?;
    let fetched = account_runtime()?
        .block_on(api.download_peer(&token(access_token, expires_at_unix), tunnel_id, peer_id))
        .map_err(|e| MobileError::start_failed(e.to_string()))?;
    let profile = std::str::from_utf8(&fetched.profile)
        .map_err(|_| MobileError::invalid_json("Peer profile is not UTF-8"))?;
    Ok(serde_json::json!({
        "peer_id": fetched.peer_id,
        "profile": profile,
    })
    .to_string())
}

pub(crate) fn create_peer(
    platform_url: &str,
    access_token: String,
    expires_at_unix: u64,
    tunnel_id: &str,
    name: &str,
) -> Result<String, MobileError> {
    let api = api(platform_url)?;
    let created = account_runtime()?
        .block_on(api.create_peer(&token(access_token, expires_at_unix), tunnel_id, name))
        .map_err(|e| MobileError::start_failed(e.to_string()))?;
    let profile = std::str::from_utf8(&created.profile)
        .map_err(|_| MobileError::invalid_json("Peer profile is not UTF-8"))?;
    Ok(serde_json::json!({
        "peer_id": created.peer_id,
        // The caller adopts this immediately; it is not written to disk here.
        "profile": profile,
    })
    .to_string())
}

// ---------------------------------------------------------------- C exports

#[no_mangle]
pub extern "C" fn tp_mobile_platform_start_sign_in(platform_url: *const c_char) -> *mut c_char {
    result_string_to_c_ptr(
        string_from_c(platform_url, "Platform URL").and_then(|url| start_sign_in(&url)),
    )
}

#[no_mangle]
pub extern "C" fn tp_mobile_platform_poll_sign_in(
    platform_url: *const c_char,
    device_code: *const c_char,
) -> *mut c_char {
    result_string_to_c_ptr((|| {
        let url = string_from_c(platform_url, "Platform URL")?;
        let code = string_from_c(device_code, "device code")?;
        poll_sign_in(&url, &code)
    })())
}

#[no_mangle]
pub extern "C" fn tp_mobile_platform_list_tunnels(
    platform_url: *const c_char,
    access_token: *const c_char,
    expires_at_unix: u64,
) -> *mut c_char {
    result_string_to_c_ptr((|| {
        let url = string_from_c(platform_url, "Platform URL")?;
        let token = string_from_c(access_token, "access token")?;
        list_tunnels(&url, token, expires_at_unix)
    })())
}

#[no_mangle]
pub extern "C" fn tp_mobile_platform_list_peers(
    platform_url: *const c_char,
    access_token: *const c_char,
    expires_at_unix: u64,
    tunnel_id: *const c_char,
) -> *mut c_char {
    result_string_to_c_ptr((|| {
        let url = string_from_c(platform_url, "Platform URL")?;
        let token = string_from_c(access_token, "access token")?;
        let tunnel_id = string_from_c(tunnel_id, "Tunnel ID")?;
        list_peers(&url, token, expires_at_unix, &tunnel_id)
    })())
}

#[no_mangle]
pub extern "C" fn tp_mobile_platform_import_peer(
    platform_url: *const c_char,
    access_token: *const c_char,
    expires_at_unix: u64,
    tunnel_id: *const c_char,
    peer_id: *const c_char,
) -> *mut c_char {
    result_string_to_c_ptr((|| {
        let url = string_from_c(platform_url, "Platform URL")?;
        let token = string_from_c(access_token, "access token")?;
        let tunnel_id = string_from_c(tunnel_id, "Tunnel ID")?;
        let peer_id = string_from_c(peer_id, "Peer ID")?;
        import_peer(&url, token, expires_at_unix, &tunnel_id, &peer_id)
    })())
}

#[no_mangle]
pub extern "C" fn tp_mobile_platform_create_peer(
    platform_url: *const c_char,
    access_token: *const c_char,
    expires_at_unix: u64,
    tunnel_id: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    result_string_to_c_ptr((|| {
        let url = string_from_c(platform_url, "Platform URL")?;
        let token = string_from_c(access_token, "access token")?;
        let tunnel_id = string_from_c(tunnel_id, "Tunnel ID")?;
        let name = string_from_c(name, "Peer name")?;
        create_peer(&url, token, expires_at_unix, &tunnel_id, &name)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_url_that_would_leak_the_token_is_refused_before_any_request() {
        // The same rule the desktop enforces: an account bearer token never
        // travels in the clear outside loopback.
        assert!(start_sign_in("http://lantunnel.app").is_err());
        assert!(list_tunnels("http://lantunnel.app", "t".into(), 0).is_err());
        assert!(create_peer("ftp://lantunnel.app", "t".into(), 0, "tunnel", "phone").is_err());
    }

    #[test]
    fn an_empty_peer_name_is_refused_before_any_request() {
        assert!(create_peer("https://lantunnel.app", "t".into(), 0, "tunnel", "   ").is_err());
    }
}
