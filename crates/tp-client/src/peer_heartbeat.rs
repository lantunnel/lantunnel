//! Signed Platform liveness for one logical Managed Peer.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tp_core::provisioning::{PeerProfileV2, PlatformHeartbeatPathModeV2, PlatformHeartbeatProofV2};

const MAX_PEER_HEARTBEAT_RESPONSE_BYTES: usize = 4 * 1024;

#[derive(Debug, Serialize)]
pub struct PeerHeartbeatRequest {
    tunnel_id: String,
    peer_id: String,
    request_id: String,
    timestamp_ms: u64,
    client_version: String,
    #[serde(rename = "final")]
    final_heartbeat: bool,
    transport_active: bool,
    path_mode: PlatformHeartbeatPathModeV2,
    proof: String,
}

impl PeerHeartbeatRequest {
    pub fn proof(&self) -> &str {
        &self.proof
    }

    fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }
}

/// How much of the Tunnel's Relay allowance this period has gone.
///
/// Reported by the Platform on every heartbeat, because it moves while the
/// Client runs. Absent from an older Platform, which is not an error.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PeerRelayUsage {
    pub used_bytes: u64,
    pub allowance_bytes: u64,
}

/// Deliberately not `deny_unknown_fields`.
///
/// Clients already installed cannot be updated in step with the Platform, so
/// the first field the Platform added would otherwise have made every
/// heartbeat fail to parse in the field.
#[derive(Debug, Deserialize)]
pub struct PeerHeartbeatResponse {
    pub accepted_timestamp_ms: u64,
    pub server_time: String,
    #[serde(default)]
    pub relay_usage: Option<PeerRelayUsage>,
}

#[derive(Debug, thiserror::Error)]
pub enum PeerHeartbeatSendError {
    #[error("{0}")]
    Retryable(String),
}

pub struct PeerHeartbeatClient {
    http: reqwest::Client,
}

impl Default for PeerHeartbeatClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerHeartbeatClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    pub async fn post(
        &self,
        platform_url: &str,
        request: &PeerHeartbeatRequest,
    ) -> Result<PeerHeartbeatResponse, PeerHeartbeatSendError> {
        let endpoint = format!("{}/api/peers/heartbeat", platform_url.trim_end_matches('/'),);
        let response = self
            .http
            .post(endpoint)
            .timeout(Duration::from_secs(10))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(CACHE_CONTROL, "no-store")
            .json(request)
            .send()
            .await
            .map_err(|error| PeerHeartbeatSendError::Retryable(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(PeerHeartbeatSendError::Retryable(format!(
                "Platform heartbeat returned HTTP {status}",
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PEER_HEARTBEAT_RESPONSE_BYTES as u64)
        {
            return Err(PeerHeartbeatSendError::Retryable(
                "Platform heartbeat response is too large".into(),
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                PeerHeartbeatSendError::Retryable(format!(
                    "could not read Platform heartbeat response: {error}"
                ))
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_PEER_HEARTBEAT_RESPONSE_BYTES {
                return Err(PeerHeartbeatSendError::Retryable(
                    "Platform heartbeat response is too large".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let accepted = serde_json::from_slice::<PeerHeartbeatResponse>(&body).map_err(|error| {
            PeerHeartbeatSendError::Retryable(format!(
                "could not decode Platform heartbeat response: {error}"
            ))
        })?;
        if accepted.accepted_timestamp_ms != request.timestamp_ms() {
            return Err(PeerHeartbeatSendError::Retryable(
                "Platform heartbeat acknowledged a different timestamp".into(),
            ));
        }
        Ok(accepted)
    }
}

pub fn build_peer_heartbeat_request(
    profile: &PeerProfileV2,
    request_id: &str,
    timestamp_ms: u64,
    client_version: &str,
    final_heartbeat: bool,
    transport_active: bool,
    path_mode: PlatformHeartbeatPathModeV2,
) -> anyhow::Result<PeerHeartbeatRequest> {
    let input = PlatformHeartbeatProofV2 {
        tunnel_id: &profile.tunnel_id,
        peer_id: &profile.peer.peer_id,
        request_id,
        timestamp_ms,
        client_version,
        final_heartbeat,
        transport_active,
        path_mode,
    };
    let proof = profile.sign_platform_heartbeat_proof(&input)?;
    Ok(PeerHeartbeatRequest {
        tunnel_id: profile.tunnel_id.clone(),
        peer_id: profile.peer.peer_id.clone(),
        request_id: request_id.into(),
        timestamp_ms,
        client_version: client_version.into(),
        final_heartbeat,
        transport_active,
        path_mode,
        proof,
    })
}
