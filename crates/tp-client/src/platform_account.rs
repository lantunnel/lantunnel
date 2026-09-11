//! Owner-authenticated Platform account API, for a Client with no browser.
//!
//! Importing a `.peer` file by hand is the only way into a Tunnel today: the
//! owner signs in on the Platform, adds a Peer, downloads the profile, and
//! moves it to the machine. That is four context switches before anything
//! connects, and on a headless server it is a file copy as well.
//!
//! This module is the other half of the shorter path. The Client asks the
//! Platform for a Device Authorization Grant (RFC 8628), prints a short code,
//! and the owner approves it in whatever browser they already have. What comes
//! back is an ordinary Platform session token, so every call below is the same
//! call the Console makes — there is no second authorization model to keep
//! honest.
//!
//! Two rules hold throughout:
//!
//! - The access token is a bearer credential for the whole account. It is
//!   [`Zeroizing`], never rendered by `Debug`, and never logged.
//! - The `.peer` body the Platform returns carries a Peer private key. It is
//!   handed to the caller and never written here; storage is the app's job,
//!   under the owner-only file rules the profile store already enforces.

use std::fmt;
use std::time::Duration;

use anyhow::{bail, Context};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The one client the Platform's Device Authorization Grant will answer.
///
/// Not a secret: RFC 8628 targets devices that cannot keep one. It is the name
/// the approval screen shows the owner.
pub const DEVICE_CLIENT_ID: &str = "lantunnel-client";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON_RESPONSE_BYTES: usize = 256 * 1024;
/// A `.peer` is a few hundred bytes; the profile store's own ceiling is 1 MiB.
const MAX_PROFILE_RESPONSE_BYTES: usize = 1024 * 1024;

/// A Platform session token and the instant it stops being one.
///
/// `expires_at_unix` is what the UI reads to say "signed in until", and what
/// the app checks before it bothers making a request.
#[derive(Clone, Deserialize, Serialize)]
pub struct PlatformToken {
    pub access_token: Zeroizing<String>,
    pub expires_at_unix: u64,
}

/// Prints the expiry and nothing else. The token itself is a live credential.
impl fmt::Debug for PlatformToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlatformToken")
            .field("access_token", &"<redacted>")
            .field("expires_at_unix", &self.expires_at_unix)
            .finish()
    }
}

impl PlatformToken {
    pub fn is_expired_at(&self, now_unix: u64) -> bool {
        // A token that dies mid-request is worse than one refreshed early.
        self.expires_at_unix <= now_unix.saturating_add(60)
    }
}

/// What the owner has to be shown to approve this Client.
#[derive(Clone, Deserialize)]
pub struct DeviceLoginStart {
    /// The Client's half of the grant. Secret: whoever holds it claims the
    /// session the moment the owner approves.
    pub device_code: Zeroizing<String>,
    /// The short code the owner reads off this screen and types into theirs.
    pub user_code: String,
    /// Where to approve, without the code.
    pub verification_uri: String,
    /// Where to approve, with the code already filled in. Absent on a Platform
    /// too old to build one.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    /// Seconds the Platform asks this Client to wait between polls.
    pub interval: u64,
}

impl fmt::Debug for DeviceLoginStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceLoginStart")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// One poll of a grant the owner has not answered yet.
#[derive(Debug)]
pub enum DeviceLoginPoll {
    /// Nobody has approved or denied it. Poll again after `interval`.
    Pending,
    /// This Client polled faster than it was told to. Add a second and retry.
    SlowDown,
    Granted(PlatformToken),
    /// The owner said no. Do not retry; start a new grant.
    Denied,
    /// The code aged out before anyone approved it.
    Expired,
}

/// A Tunnel as the owner's Console lists it.
///
/// `name` is the whole point: a Tunnel ID is a UUID, and picking the wrong one
/// out of a list of UUIDs is the mistake this screen exists to prevent.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformTunnel {
    pub tunnel_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub billing: Option<PlatformTunnelBilling>,
    #[serde(default)]
    pub placement: Option<PlatformTunnelPlacement>,
}

impl PlatformTunnel {
    /// Whether adding a Peer to this Tunnel would be accepted right now.
    ///
    /// A lapsed subscription answers 402 at `create_peer`. Knowing it here lets
    /// the picker say so instead of failing after the owner has typed a name.
    pub fn accepts_new_peers(&self) -> bool {
        let enabled = self.status.as_deref().unwrap_or("enabled") == "enabled";
        let active = self
            .billing
            .as_ref()
            .and_then(|billing| billing.access.as_deref())
            .unwrap_or("active")
            == "active";
        enabled && active
    }

    /// `free`, `pro`, … — whatever the Platform calls this Tunnel's plan.
    pub fn plan_label(&self) -> Option<&str> {
        self.billing
            .as_ref()
            .and_then(|billing| billing.effective_plan.as_deref())
    }
}

// Deliberately tolerant of unknown fields, like every other Platform reader
// here: a Client already installed cannot be updated in step with the
// Platform, so the first field the Platform adds must not break it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformTunnelBilling {
    #[serde(default)]
    pub grant_kind: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub effective_plan: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformTunnelPlacement {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// A Peer this Tunnel has already issued.
///
/// The owner may have added it from the Console, or from another device. It is
/// listed so this Client can adopt an existing identity rather than minting a
/// second one for a machine that already has one.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformPeer {
    pub peer_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub overlay_ip: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Deserialize)]
struct PeerListResponse {
    #[serde(default)]
    peers: Vec<PlatformPeer>,
}

/// A Peer the Platform just minted, and the profile that proves it.
pub struct CreatedPeer {
    pub peer_id: Option<String>,
    /// The `.peer` file body, verbatim. Carries a private key.
    pub profile: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for CreatedPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatedPeer")
            .field("peer_id", &self.peer_id)
            .field("profile", &"<redacted>")
            .finish()
    }
}

/// Who the stored token belongs to, for a UI that has to say so.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformIdentity {
    pub email: String,
}

#[derive(Deserialize)]
struct SessionEnvelope {
    user: SessionUser,
}

#[derive(Deserialize)]
struct SessionUser {
    email: String,
}

#[derive(Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Serialize)]
struct DeviceTokenRequest<'a> {
    grant_type: &'a str,
    device_code: &'a str,
    client_id: &'a str,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    access_token: Zeroizing<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct OAuthError {
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct TunnelListResponse {
    #[serde(default)]
    tunnels: Vec<PlatformTunnel>,
}

#[derive(Serialize)]
struct CreatePeerRequest<'a> {
    name: &'a str,
}

/// A Platform base URL that has been checked once, at construction.
pub struct PlatformAccountApi {
    base_url: String,
    http: reqwest::Client,
}

impl PlatformAccountApi {
    pub fn new(platform_url: &str) -> anyhow::Result<Self> {
        let base_url = validate_platform_url(platform_url)?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("could not initialize the Platform account HTTPS client")?;
        Ok(Self { base_url, http })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Opens a Device Authorization Grant and returns what to show the owner.
    pub async fn start_device_login(&self) -> anyhow::Result<DeviceLoginStart> {
        let response = self
            .http
            .post(format!("{}/api/auth/device/code", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(CACHE_CONTROL, "no-store")
            .json(&DeviceCodeRequest {
                client_id: DEVICE_CLIENT_ID,
            })
            .send()
            .await
            .context("could not reach the Platform to start sign-in")?;
        let status = response.status();
        let bytes = bounded_body(response, MAX_JSON_RESPONSE_BYTES, "Platform sign-in").await?;
        if !status.is_success() {
            tracing::warn!(%status, "Platform refused to start a device sign-in");
            bail!("{}", platform_request_error(status));
        }
        serde_json::from_slice(&bytes).context("Platform sign-in response was not understood")
    }

    /// Asks once whether the owner has answered yet.
    ///
    /// The caller owns the waiting: the Platform's `interval` is a floor, and
    /// [`DeviceLoginPoll::SlowDown`] means back off further.
    pub async fn poll_device_login(&self, device_code: &str) -> anyhow::Result<DeviceLoginPoll> {
        let response = self
            .http
            .post(format!("{}/api/auth/device/token", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(CACHE_CONTROL, "no-store")
            .json(&DeviceTokenRequest {
                grant_type: DEVICE_GRANT_TYPE,
                device_code,
                client_id: DEVICE_CLIENT_ID,
            })
            .send()
            .await
            .context("could not reach the Platform while waiting for approval")?;
        let status = response.status();
        let bytes = bounded_body(response, MAX_JSON_RESPONSE_BYTES, "Platform sign-in").await?;

        if status.is_success() {
            let granted: DeviceTokenResponse = serde_json::from_slice(&bytes)
                .context("Platform approval response was not understood")?;
            // A Platform that omits `expires_in` is treated as the shortest
            // useful session rather than an unbounded one.
            let lifetime = granted.expires_in.unwrap_or(3600);
            return Ok(DeviceLoginPoll::Granted(PlatformToken {
                access_token: granted.access_token,
                expires_at_unix: now_unix()?.saturating_add(lifetime),
            }));
        }

        let error = serde_json::from_slice::<OAuthError>(&bytes)
            .ok()
            .and_then(|body| body.error)
            .unwrap_or_default();
        match error.as_str() {
            "authorization_pending" => Ok(DeviceLoginPoll::Pending),
            "slow_down" => Ok(DeviceLoginPoll::SlowDown),
            "access_denied" => Ok(DeviceLoginPoll::Denied),
            "expired_token" => Ok(DeviceLoginPoll::Expired),
            "" => {
                tracing::warn!(%status, "Platform approval check failed");
                bail!("{}", platform_request_error(status))
            }
            other => bail!("Platform approval check failed: {other}"),
        }
    }

    /// The account the stored token belongs to.
    pub async fn identity(&self, token: &PlatformToken) -> anyhow::Result<PlatformIdentity> {
        let bytes = self
            .authorized_get(token, "/api/auth/get-session", MAX_JSON_RESPONSE_BYTES)
            .await?;
        let envelope: SessionEnvelope =
            serde_json::from_slice(&bytes).context("Platform session was not understood")?;
        Ok(PlatformIdentity {
            email: envelope.user.email,
        })
    }

    /// Every Tunnel this account owns, newest Platform fields included.
    pub async fn list_tunnels(&self, token: &PlatformToken) -> anyhow::Result<Vec<PlatformTunnel>> {
        let bytes = self
            .authorized_get(token, "/api/tunnels", MAX_JSON_RESPONSE_BYTES)
            .await?;
        let listed: TunnelListResponse =
            serde_json::from_slice(&bytes).context("Platform Tunnel list was not understood")?;
        Ok(listed.tunnels)
    }

    /// Every Peer this Tunnel has already issued.
    pub async fn list_peers(
        &self,
        token: &PlatformToken,
        tunnel_id: &str,
    ) -> anyhow::Result<Vec<PlatformPeer>> {
        let path = format!("/api/tunnels/{}/peers", urlencode_path_segment(tunnel_id));
        let bytes = self
            .authorized_get(token, &path, MAX_JSON_RESPONSE_BYTES)
            .await?;
        let listed: PeerListResponse =
            serde_json::from_slice(&bytes).context("Platform Peer list was not understood")?;
        Ok(listed.peers)
    }

    /// Fetches the `.peer` profile of a Peer that already exists.
    ///
    /// The Platform keeps the key sealed, so this hands back the same file the
    /// Peer already has rather than replacing it — a device using it is not
    /// disturbed. That is what makes adopting an existing Peer safe, instead of
    /// minting a second identity for a machine that already had one.
    pub async fn download_peer(
        &self,
        token: &PlatformToken,
        tunnel_id: &str,
        peer_id: &str,
    ) -> anyhow::Result<CreatedPeer> {
        let response = self
            .http
            .post(format!(
                "{}/api/tunnels/{}/peers/{}",
                self.base_url,
                urlencode_path_segment(tunnel_id),
                urlencode_path_segment(peer_id)
            ))
            .header(AUTHORIZATION, bearer(token))
            .header(CACHE_CONTROL, "no-store")
            .send()
            .await
            .context("could not reach the Platform to fetch the Peer profile")?;
        let status = response.status();
        let returned_peer_id = response
            .headers()
            .get("x-peer-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = bounded_body(response, MAX_PROFILE_RESPONSE_BYTES, "Peer profile").await?;
        if !status.is_success() {
            bail!("{}", peer_download_error(status, &bytes));
        }
        Ok(CreatedPeer {
            peer_id: returned_peer_id.or_else(|| Some(peer_id.to_owned())),
            profile: Zeroizing::new(bytes),
        })
    }

    /// Adds one Peer to a Tunnel and returns its `.peer` profile.
    ///
    /// The response body *is* the profile — the Peer ID travels in a header,
    /// because the body had nowhere to put it.
    pub async fn create_peer(
        &self,
        token: &PlatformToken,
        tunnel_id: &str,
        name: &str,
    ) -> anyhow::Result<CreatedPeer> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a Peer needs a name");
        }
        let response = self
            .http
            .post(format!(
                "{}/api/tunnels/{}/peers",
                self.base_url,
                urlencode_path_segment(tunnel_id)
            ))
            .header(AUTHORIZATION, bearer(token))
            .header(CONTENT_TYPE, "application/json")
            .header(CACHE_CONTROL, "no-store")
            .json(&CreatePeerRequest { name })
            .send()
            .await
            .context("could not reach the Platform to add a Peer")?;
        let status = response.status();
        let peer_id = response
            .headers()
            .get("x-peer-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = bounded_body(response, MAX_PROFILE_RESPONSE_BYTES, "Peer profile").await?;
        if !status.is_success() {
            bail!("{}", peer_creation_error(status, &bytes));
        }
        Ok(CreatedPeer {
            peer_id,
            profile: Zeroizing::new(bytes),
        })
    }

    async fn authorized_get(
        &self,
        token: &PlatformToken,
        path: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .header(AUTHORIZATION, bearer(token))
            .header(ACCEPT, "application/json")
            .header(CACHE_CONTROL, "no-store")
            .send()
            .await
            .with_context(|| format!("could not reach the Platform at {path}"))?;
        let status = response.status();
        let bytes = bounded_body(response, max_bytes, "Platform response").await?;
        if !status.is_success() {
            // The status code is for the log, not for the person looking at a
            // panel. "HTTP 500 Internal Server Error" names nothing they can
            // act on and reads as though the Client broke.
            tracing::warn!(%status, path, "Platform request failed");
            bail!("{}", platform_request_error(status));
        }
        Ok(bytes)
    }
}

fn bearer(token: &PlatformToken) -> String {
    format!("Bearer {}", token.access_token.as_str())
}

/// What a failed request says to the person looking at it.
///
/// Every arm names something the owner can do, or says plainly that the fault
/// is not theirs. None of them prints a status code.
fn platform_request_error(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        401 | 403 => "Platform sign-in has expired; sign in again".into(),
        404 => "This Platform does not offer that yet; update the Client".into(),
        408 | 429 => "The Platform is busy; try again in a moment".into(),
        500..=599 => "The Platform is having trouble; try again in a moment".into(),
        _ => "The Platform could not answer; try again".into(),
    }
}

/// Why an existing Peer's profile could not be fetched.
fn peer_download_error(status: reqwest::StatusCode, bytes: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        error: Option<String>,
    }
    let detail = serde_json::from_slice::<ErrorBody>(bytes)
        .ok()
        .and_then(|body| body.error);
    match (status.as_u16(), detail.as_deref()) {
        (401 | 403, _) => "Platform sign-in has expired; sign in again".into(),
        // Peers issued before the Platform kept the sealed key have nothing to
        // hand back, which is a different problem from a missing Peer.
        (404 | 409, Some(detail)) if detail.contains("not available") => {
            "That Peer was issued before profiles could be downloaded again. Add a new one.".into()
        }
        (404, _) => "That Peer is no longer in this Tunnel".into(),
        _ => platform_request_error(status),
    }
}

/// Turns the Platform's own words into something an owner can act on.
fn peer_creation_error(status: reqwest::StatusCode, bytes: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        error: Option<String>,
    }
    let detail = serde_json::from_slice::<ErrorBody>(bytes)
        .ok()
        .and_then(|body| body.error);
    match (status.as_u16(), detail) {
        (401 | 403, _) => "Platform sign-in has expired; sign in again".into(),
        (402, _) => {
            "This Tunnel needs an active subscription before it can take another Peer".into()
        }
        (404, _) => "That Tunnel is no longer on this account".into(),
        // A 5xx is the Platform's problem, and its body carries an internal
        // message rather than anything the owner chose or can change.
        (500..=599, _) => platform_request_error(status),
        (_, Some(detail)) => format!("The Platform refused to add the Peer: {detail}"),
        (_, None) => format!(
            "The Platform refused to add the Peer. {}",
            platform_request_error(status)
        ),
    }
}

/// Rejects a base URL that would send an account token somewhere unprotected.
///
/// Plain HTTP is allowed only against loopback, which is how the Platform is
/// run during development; anything else has to be TLS, because the token is
/// enough to add a Peer to every Tunnel on the account.
fn validate_platform_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("Platform URL is empty");
    }
    if let Some(host) = trimmed.strip_prefix("https://") {
        if host.is_empty() {
            bail!("Platform URL has no host");
        }
        return Ok(trimmed.to_owned());
    }
    if let Some(host) = trimmed.strip_prefix("http://") {
        let authority = host.split('/').next().unwrap_or_default();
        // `[::1]:8787` splits on the colons inside the brackets, so the
        // bracketed form is peeled off before the port is.
        let hostname = match authority.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        if hostname == "localhost" || hostname == "127.0.0.1" || hostname == "::1" {
            return Ok(trimmed.to_owned());
        }
        bail!("Platform URL must use HTTPS outside loopback");
    }
    bail!("Platform URL must start with https://")
}

/// Percent-encodes the characters that would let an ID escape its path segment.
///
/// Tunnel IDs are UUIDs today, so nothing here fires; it fires the day one is
/// not, and a `../` in a path segment is not a bug worth discovering later.
fn urlencode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn now_unix() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time is before the Unix epoch")?
        .as_secs())
}

async fn bounded_body(
    response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("{label} is too large");
    }
    read_bytes_bounded(response.bytes_stream(), max_bytes, label).await
}

async fn read_bytes_bounded<S, E>(
    mut chunks: S,
    max_bytes: usize,
    label: &str,
) -> anyhow::Result<Vec<u8>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.with_context(|| format!("could not read {label}"))?;
        let remaining = max_bytes.saturating_add(1).saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() > max_bytes {
            bail!("{label} is too large");
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_url_must_carry_tls_unless_it_is_loopback() {
        assert_eq!(
            validate_platform_url("https://lantunnel.app/").unwrap(),
            "https://lantunnel.app"
        );
        assert_eq!(
            validate_platform_url("  https://lantunnel.app  ").unwrap(),
            "https://lantunnel.app"
        );
        assert_eq!(
            validate_platform_url("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            validate_platform_url("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        // An IPv6 loopback authority carries brackets the port split would eat.
        assert_eq!(
            validate_platform_url("http://[::1]:8787").unwrap(),
            "http://[::1]:8787"
        );
        assert_eq!(
            validate_platform_url("http://[::1]").unwrap(),
            "http://[::1]"
        );
        // Only loopback. A bracketed public address is still refused.
        assert!(validate_platform_url("http://[2606:4700::1111]:80").is_err());

        // An account bearer token must never travel in the clear.
        assert!(validate_platform_url("http://lantunnel.app").is_err());
        assert!(validate_platform_url("http://127.0.0.1.evil.example").is_err());
        assert!(validate_platform_url("ftp://lantunnel.app").is_err());
        assert!(validate_platform_url("lantunnel.app").is_err());
        assert!(validate_platform_url("   ").is_err());
        assert!(validate_platform_url("https://").is_err());
    }

    #[test]
    fn an_identifier_cannot_escape_its_path_segment() {
        assert_eq!(
            urlencode_path_segment("018f6e84-e11b-7f3a-8cad-9f68f4482001"),
            "018f6e84-e11b-7f3a-8cad-9f68f4482001"
        );
        assert_eq!(urlencode_path_segment("../../admin"), "..%2F..%2Fadmin");
        assert_eq!(urlencode_path_segment("a b"), "a%20b");
        assert_eq!(urlencode_path_segment("a?b#c"), "a%3Fb%23c");
    }

    #[test]
    fn a_token_never_renders_itself() {
        let token = PlatformToken {
            access_token: Zeroizing::new("super-secret-session-token".into()),
            expires_at_unix: 1_800_000_000,
        };
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains("super-secret-session-token"),
            "{rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(rendered.contains("1800000000"), "{rendered}");
    }

    #[test]
    fn a_device_code_never_renders_itself() {
        let start = DeviceLoginStart {
            device_code: Zeroizing::new("device-secret".into()),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://lantunnel.app/device".into(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
        };
        let rendered = format!("{start:?}");
        assert!(!rendered.contains("device-secret"), "{rendered}");
        assert!(rendered.contains("ABCD-EFGH"), "{rendered}");
    }

    #[test]
    fn a_token_is_treated_as_spent_a_minute_before_it_dies() {
        let token = PlatformToken {
            access_token: Zeroizing::new("t".into()),
            expires_at_unix: 1_000,
        };
        assert!(!token.is_expired_at(0));
        assert!(!token.is_expired_at(939));
        // Inside the last minute a request would likely outlive the token.
        assert!(token.is_expired_at(940));
        assert!(token.is_expired_at(1_000));
        assert!(token.is_expired_at(2_000));
    }

    #[test]
    fn a_tunnel_that_cannot_take_a_peer_says_so_before_the_owner_types_a_name() {
        let listed: Vec<PlatformTunnel> = serde_json::from_str(
            r#"[
              {"tunnel_id":"a","name":"Home","status":"enabled",
               "billing":{"grant_kind":"account_free","access":"active","effective_plan":"free"}},
              {"tunnel_id":"b","name":"Lapsed","status":"enabled",
               "billing":{"grant_kind":"paid","access":"inactive","effective_plan":"pro"}},
              {"tunnel_id":"c","name":"Disabled","status":"disabled",
               "billing":{"grant_kind":"paid","access":"active","effective_plan":"pro"}},
              {"tunnel_id":"d","name":"Older Platform"}
            ]"#,
        )
        .expect("tunnel list");

        assert!(listed[0].accepts_new_peers());
        assert_eq!(listed[0].plan_label(), Some("free"));
        assert!(!listed[1].accepts_new_peers());
        assert!(!listed[2].accepts_new_peers());
        // A Platform that has not sent these fields yet must not be read as a
        // dead Tunnel; the create call remains the authority.
        assert!(listed[3].accepts_new_peers());
        assert_eq!(listed[3].plan_label(), None);
    }

    #[test]
    fn an_unknown_platform_field_does_not_break_an_older_client() {
        let listed: TunnelListResponse = serde_json::from_str(
            r#"{"tunnels":[{"tunnel_id":"a","name":"Home","future_field":42}],"future":true}"#,
        )
        .expect("tunnel list");
        assert_eq!(listed.tunnels.len(), 1);
        assert_eq!(listed.tunnels[0].name.as_deref(), Some("Home"));
    }

    #[test]
    fn peer_creation_failures_say_what_the_owner_can_do() {
        let payment = peer_creation_error(reqwest::StatusCode::PAYMENT_REQUIRED, b"{}");
        assert!(payment.contains("subscription"), "{payment}");

        let unauthorized = peer_creation_error(reqwest::StatusCode::UNAUTHORIZED, b"{}");
        assert!(unauthorized.contains("sign in again"), "{unauthorized}");

        let detailed = peer_creation_error(
            reqwest::StatusCode::CONFLICT,
            br#"{"error":"Overlay IP is already allocated"}"#,
        );
        assert!(
            detailed.contains("Overlay IP is already allocated"),
            "{detailed}"
        );

        let bare = peer_creation_error(reqwest::StatusCode::BAD_GATEWAY, b"not json");
        assert!(bare.contains("try again"), "{bare}");
    }

    #[test]
    fn a_failure_never_shows_the_owner_a_status_code() {
        // "Platform returned HTTP 500 Internal Server Error" names nothing the
        // owner can do and reads as though the Client broke.
        for code in [400, 401, 402, 403, 404, 408, 429, 500, 502, 503] {
            let status = reqwest::StatusCode::from_u16(code).expect("status");
            for message in [
                platform_request_error(status),
                peer_creation_error(status, b"{}"),
                peer_creation_error(status, br#"{"error":"Internal server error"}"#),
            ] {
                assert!(
                    !message.contains(&code.to_string()),
                    "{code} leaked into: {message}"
                );
                assert!(
                    !message.to_ascii_lowercase().contains("http"),
                    "{code} produced a protocol-shaped message: {message}"
                );
            }
        }

        // Each class still says something different, so the copy is not just
        // one shrug for every failure.
        assert!(platform_request_error(reqwest::StatusCode::UNAUTHORIZED).contains("sign in again"));
        assert!(platform_request_error(reqwest::StatusCode::TOO_MANY_REQUESTS).contains("busy"));
        assert!(
            platform_request_error(reqwest::StatusCode::NOT_FOUND).contains("update the Client")
        );
        assert!(
            peer_creation_error(reqwest::StatusCode::PAYMENT_REQUIRED, b"{}")
                .contains("subscription")
        );
    }

    #[tokio::test]
    async fn a_response_past_the_limit_is_refused_rather_than_buffered() {
        let chunks = futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"0123456789")),
            Ok(Bytes::from_static(b"0123456789")),
        ]);
        assert!(read_bytes_bounded(chunks, 15, "test body").await.is_err());

        let exact = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
            b"0123456789",
        ))]);
        assert_eq!(
            read_bytes_bounded(exact, 10, "test body").await.unwrap(),
            b"0123456789".to_vec()
        );
    }
}
