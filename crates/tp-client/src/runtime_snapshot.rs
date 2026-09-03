//! Public, secret-free Lantunnel 2.0 runtime truth.
//!
//! `Engine` owns one copy of this read model behind one lock.  It is not a
//! protocol or a second controller: lifecycle callbacks update the copy that
//! the app reads atomically, while payload counters remain the existing
//! lock-free counters and are sampled as the snapshot is cloned.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2OverallPhase {
    #[default]
    Disconnected,
    Starting,
    WaitingForGateway,
    Connected,
    Degraded,
    Blocked,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2GatewayAttachmentPhase {
    #[default]
    Inactive,
    ResolvingThroughPlatform,
    ProvisioningScope,
    Connecting,
    Attached,
    Unavailable,
    Rejected,
    TlsFailed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2MeshPhase {
    #[default]
    Unavailable,
    Syncing,
    Healthy,
    Degraded,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2GossipPhase {
    #[default]
    Unavailable,
    Syncing,
    Ready,
    Repairing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2PeerDirectoryPhase {
    #[default]
    Unavailable,
    Syncing,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2RemotePeerPhase {
    Syncing,
    Ready,
    Stale,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2PeerPath {
    Direct,
    EncryptedRelay,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2RoutingPhase {
    #[default]
    Unavailable,
    Syncing,
    Ready,
}

/// Stable codes only.  Raw transport errors, hostnames, ACL rules, LAN
/// topology, and key material never enter the public runtime read model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V2RuntimeReasonCode {
    RuntimeInactive,
    ResolvingThroughPlatform,
    ConnectingToGateway,
    PlatformUnavailable,
    NoEligibleGateway,
    ScopeRejected,
    GatewayTlsFailed,
    GatewayAuthenticationRejected,
    GatewayConnectFailed,
    GatewayUnavailable,
    GatewayUnavailableDirectPreserved,
    MembershipCyclePending,
    InitialFullSyncPending,
    PeerLinkUnavailable,
    NoUsablePeerPath,
    RuntimeFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2RuntimePhase<T> {
    pub phase: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<V2RuntimeReasonCode>,
}

impl<T: Default> Default for V2RuntimePhase<T> {
    fn default() -> Self {
        Self {
            phase: T::default(),
            reason_code: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2GatewayAttachmentSnapshot {
    pub phase: V2GatewayAttachmentPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<V2RuntimeReasonCode>,
}

impl Default for V2GatewayAttachmentSnapshot {
    fn default() -> Self {
        Self {
            phase: V2GatewayAttachmentPhase::Inactive,
            endpoint: None,
            reason_code: Some(V2RuntimeReasonCode::RuntimeInactive),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2ThisPeerSnapshot {
    pub peer_id: String,
    pub overlay_ip: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum V2ExportPlacement {
    ActiveHere,
    StandbyHere { position: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2RemoteExportSnapshot {
    pub prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<V2ExportPlacement>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2LocalExportSnapshot {
    pub prefix: String,
    pub ready: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2RemotePeerSnapshot {
    pub peer_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<Ipv4Addr>,
    pub phase: V2RemotePeerPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<V2RuntimeReasonCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<V2PeerPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usable_lanes: Option<u32>,
    pub routing: V2RoutingPhase,
    #[serde(default)]
    pub exports: Vec<V2RemoteExportSnapshot>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2PeerDirectorySnapshot {
    pub phase: V2PeerDirectoryPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<V2RuntimeReasonCode>,
    #[serde(default)]
    pub peers: Vec<V2RemotePeerSnapshot>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2TrafficSnapshot {
    pub direct_tx_bytes: u64,
    pub direct_rx_bytes: u64,
    pub relay_tx_bytes: u64,
    pub relay_rx_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2RuntimeSnapshot {
    pub overall: V2RuntimePhase<V2OverallPhase>,
    pub gateway_attachment: V2GatewayAttachmentSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub this_peer: Option<V2ThisPeerSnapshot>,
    pub mesh: V2RuntimePhase<V2MeshPhase>,
    pub gossip: V2RuntimePhase<V2GossipPhase>,
    #[serde(default)]
    pub local_exports: Vec<V2LocalExportSnapshot>,
    pub peer_directory: V2PeerDirectorySnapshot,
    pub traffic: V2TrafficSnapshot,
    /// How much of the Tunnel's Relay allowance this period has gone, as the
    /// Platform last reported it.
    ///
    /// An observation, not an entitlement the Client enforces: the allowance is
    /// applied in the data plane, and this is only what the owner is shown so
    /// running out is not the first they hear of it. Absent until a heartbeat
    /// answers, and absent from a Platform that does not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_usage: Option<V2RelayUsageSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct V2RelayUsageSnapshot {
    pub used_bytes: u64,
    pub allowance_bytes: u64,
}

impl Default for V2RuntimeSnapshot {
    fn default() -> Self {
        Self {
            overall: V2RuntimePhase {
                phase: V2OverallPhase::Disconnected,
                reason_code: Some(V2RuntimeReasonCode::RuntimeInactive),
            },
            gateway_attachment: V2GatewayAttachmentSnapshot::default(),
            this_peer: None,
            mesh: V2RuntimePhase {
                phase: V2MeshPhase::Unavailable,
                reason_code: Some(V2RuntimeReasonCode::RuntimeInactive),
            },
            gossip: V2RuntimePhase {
                phase: V2GossipPhase::Unavailable,
                reason_code: Some(V2RuntimeReasonCode::RuntimeInactive),
            },
            local_exports: Vec::new(),
            peer_directory: V2PeerDirectorySnapshot {
                phase: V2PeerDirectoryPhase::Unavailable,
                reason_code: Some(V2RuntimeReasonCode::RuntimeInactive),
                peers: Vec::new(),
            },
            traffic: V2TrafficSnapshot::default(),
            relay_usage: None,
        }
    }
}
