//! The one projection every Lantunnel Client renders.
//!
//! This module deliberately accepts explicit runtime facts. Missing Mesh,
//! Gossip, Native, and remote-Peer consumers stay unknown/unavailable instead
//! of being guessed from legacy connection counts or process liveness.
//!
//! It lived in the desktop Tauri crate, which is why the phones could not use
//! it and re-derived every label themselves — three copies of `meshStateLabel`,
//! of a Peer's reachability wording, and of the byte and duration formats. One
//! projection, one vocabulary, whichever Client is asking.

use crate::access_policy::ClientAccessPolicyV2;
use crate::runtime_snapshot::{
    V2ExportPlacement, V2GatewayAttachmentPhase, V2GossipPhase, V2MeshPhase, V2OverallPhase,
    V2PeerDirectoryPhase, V2PeerPath, V2RemotePeerPhase, V2RoutingPhase, V2RuntimeReasonCode,
    V2RuntimeSnapshot,
};
use crate::status::ConnectionStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientSettingsSectionV2 {
    Connection,
    NetworkAndLanExport,
    ClientAccess,
    Diagnostics,
}

pub const fn client_settings_sections_v2() -> [ClientSettingsSectionV2; 4] {
    [
        ClientSettingsSectionV2::Connection,
        ClientSettingsSectionV2::NetworkAndLanExport,
        ClientSettingsSectionV2::ClientAccess,
        ClientSettingsSectionV2::Diagnostics,
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingAvailabilityV2 {
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingValueV2<T> {
    pub availability: SettingAvailabilityV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

impl<T> SettingValueV2<T> {
    fn from_optional(value: Option<T>, unavailable_reason: &'static str) -> Self {
        match value {
            Some(value) => Self {
                availability: SettingAvailabilityV2::Ready,
                value: Some(value),
                reason_code: None,
            },
            None => Self {
                availability: SettingAvailabilityV2::Unavailable,
                value: None,
                reason_code: Some(unavailable_reason.into()),
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientSettingsFactsV2 {
    pub tunnel_first: Option<bool>,
    pub exported_lans: Option<Vec<String>>,
    pub client_access: Option<ClientAccessPolicyV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientSettingsReadModelV2 {
    pub sections: [ClientSettingsSectionV2; 4],
    pub tunnel_first: SettingValueV2<bool>,
    pub exported_lans: SettingValueV2<Vec<String>>,
    pub client_access: SettingValueV2<ClientAccessPolicyV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalExportStatusV2 {
    pub prefix: String,
    pub ready: bool,
}

pub fn project_client_settings(facts: ClientSettingsFactsV2) -> ClientSettingsReadModelV2 {
    ClientSettingsReadModelV2 {
        sections: client_settings_sections_v2(),
        tunnel_first: SettingValueV2::from_optional(
            facts.tunnel_first,
            "tunnel_first_runtime_consumer_unavailable",
        ),
        exported_lans: SettingValueV2::from_optional(
            facts.exported_lans,
            "lan_export_runtime_consumer_unavailable",
        ),
        client_access: SettingValueV2::from_optional(
            facts.client_access,
            "client_access_runtime_consumer_unavailable",
        ),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientOverallStateV2 {
    #[default]
    Disconnected,
    Starting,
    WaitingForGateway,
    Connected,
    Degraded,
    Blocked,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayAttachmentStateV2 {
    #[default]
    Unknown,
    ResolvingThroughPlatform,
    ProvisioningScope,
    Connecting,
    Attached,
    Unavailable,
    Rejected,
    TlsFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayAttachmentV2 {
    pub state: GatewayAttachmentStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

impl Default for GatewayAttachmentV2 {
    fn default() -> Self {
        Self {
            state: GatewayAttachmentStateV2::Unknown,
            endpoint: None,
            reason_code: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeshStateV2 {
    #[default]
    Unknown,
    Syncing,
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GossipStateV2 {
    #[default]
    Unknown,
    Syncing,
    Ready,
    Repairing,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeRoutingStateV2 {
    #[default]
    Unknown,
    Disabled,
    Applying,
    Ready,
    NeedsHelper,
    PermissionDenied,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeRoutingActionV2 {
    InstallHelper,
    RepairPermissions,
    RetryApply,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NativeRoutingApplyResultV2 {
    #[default]
    Unavailable,
    Applying,
    Applied,
    PermissionDenied,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRoutingV2 {
    pub state: NativeRoutingStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub actions: Vec<NativeRoutingActionV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStateV2<T> {
    pub state: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

impl<T: Default> Default for RuntimeStateV2<T> {
    fn default() -> Self {
        Self {
            state: T::default(),
            reason_code: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerDirectoryStateV2 {
    Syncing,
    Ready,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDirectoryV2 {
    pub state: PeerDirectoryStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub peers: Vec<RemotePeerRowV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePeerRowV2 {
    pub peer_id: String,
    pub overlay_cidr: String,
    pub state: RemotePeerStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<PeerCurrentPathV2>,
    pub routing: RoutingStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usable_lanes: Option<u32>,
    #[serde(default)]
    pub exports: Vec<RemotePeerExportV2>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePeerStateV2 {
    Syncing,
    Ready,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerCurrentPathV2 {
    Direct,
    EncryptedRelay,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStateV2 {
    #[default]
    Unknown,
    Syncing,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePeerExportV2 {
    pub prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<ExportPlacementV2>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExportPlacementV2 {
    ActiveHere,
    StandbyHere { position: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThisPeerV2 {
    pub peer_id: String,
    pub overlay_cidr: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficCountersV2 {
    pub direct_tx_bytes: u64,
    pub direct_rx_bytes: u64,
    pub relay_tx_bytes: u64,
    pub relay_rx_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientUiStatusV2 {
    pub overall: ClientOverallStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overall_reason_code: Option<String>,
    pub gateway_attachment: GatewayAttachmentV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub this_peer: Option<ThisPeerV2>,
    pub mesh: RuntimeStateV2<MeshStateV2>,
    pub gossip: RuntimeStateV2<GossipStateV2>,
    pub native_routing: NativeRoutingV2,
    pub peer_directory: PeerDirectoryV2,
    pub traffic: TrafficCountersV2,
    /// How much of the Tunnel's Relay allowance this period has gone, as the
    /// Platform last reported it. Absent until a heartbeat answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_usage: Option<RelayUsageV2>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayUsageV2 {
    pub used_bytes: u64,
    pub allowance_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientUiFactsV2 {
    pub overall: Option<RuntimeStateV2<ClientOverallStateV2>>,
    pub gateway_attachment: Option<GatewayAttachmentV2>,
    pub this_peer: Option<ThisPeerV2>,
    pub mesh: RuntimeStateV2<MeshStateV2>,
    pub gossip: RuntimeStateV2<GossipStateV2>,
    pub native_routing: NativeRoutingV2,
    pub peer_directory: PeerDirectoryV2,
    pub traffic: Option<TrafficCountersV2>,
    pub relay_usage: Option<RelayUsageV2>,
}

impl Default for ClientUiFactsV2 {
    fn default() -> Self {
        Self {
            overall: Some(RuntimeStateV2 {
                state: ClientOverallStateV2::Disconnected,
                reason_code: Some("runtime_snapshot_unavailable".into()),
            }),
            gateway_attachment: Some(GatewayAttachmentV2 {
                state: GatewayAttachmentStateV2::Unknown,
                endpoint: None,
                reason_code: Some("runtime_snapshot_unavailable".into()),
            }),
            this_peer: None,
            mesh: RuntimeStateV2 {
                state: MeshStateV2::Unknown,
                reason_code: Some("runtime_snapshot_unavailable".into()),
            },
            gossip: RuntimeStateV2 {
                state: GossipStateV2::Unknown,
                reason_code: Some("runtime_snapshot_unavailable".into()),
            },
            native_routing: NativeRoutingV2 {
                state: NativeRoutingStateV2::Unknown,
                reason_code: Some("native_apply_result_unavailable".into()),
                actions: Vec::new(),
            },
            peer_directory: PeerDirectoryV2 {
                state: PeerDirectoryStateV2::Unavailable,
                reason_code: Some("runtime_snapshot_unavailable".into()),
                peers: Vec::new(),
            },
            traffic: Some(TrafficCountersV2::default()),
            relay_usage: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientStatusReadModelV2 {
    #[serde(flatten)]
    pub connection: ConnectionStatus,
    pub client_ui: ClientUiStatusV2,
}

pub fn project_client_status_read_model(
    status: ConnectionStatus,
    facts: ClientUiFactsV2,
) -> ClientStatusReadModelV2 {
    let client_ui = project_client_ui_status(&status, facts);
    ClientStatusReadModelV2 {
        connection: status,
        client_ui,
    }
}

pub fn project_native_routing(
    configured: bool,
    apply_result: NativeRoutingApplyResultV2,
    helper_required: bool,
    helper_installed: bool,
) -> NativeRoutingV2 {
    if !configured {
        return NativeRoutingV2 {
            state: NativeRoutingStateV2::Disabled,
            ..Default::default()
        };
    }
    if helper_required && !helper_installed {
        return NativeRoutingV2 {
            state: NativeRoutingStateV2::NeedsHelper,
            reason_code: Some("native_helper_not_installed".into()),
            actions: vec![NativeRoutingActionV2::InstallHelper],
        };
    }
    match apply_result {
        NativeRoutingApplyResultV2::Unavailable => NativeRoutingV2 {
            state: NativeRoutingStateV2::Unknown,
            reason_code: Some("native_apply_result_unavailable".into()),
            actions: vec![NativeRoutingActionV2::RetryApply],
        },
        NativeRoutingApplyResultV2::Applying => NativeRoutingV2 {
            state: NativeRoutingStateV2::Applying,
            reason_code: Some("native_apply_in_progress".into()),
            actions: Vec::new(),
        },
        NativeRoutingApplyResultV2::Applied => NativeRoutingV2 {
            state: NativeRoutingStateV2::Ready,
            reason_code: None,
            actions: Vec::new(),
        },
        NativeRoutingApplyResultV2::PermissionDenied => NativeRoutingV2 {
            state: NativeRoutingStateV2::PermissionDenied,
            reason_code: Some("native_apply_permission_denied".into()),
            actions: vec![NativeRoutingActionV2::RepairPermissions],
        },
        NativeRoutingApplyResultV2::Failed => NativeRoutingV2 {
            state: NativeRoutingStateV2::Failed,
            reason_code: Some("native_apply_failed".into()),
            actions: vec![NativeRoutingActionV2::RetryApply],
        },
    }
}

pub fn project_engine_runtime_snapshot(
    snapshot: V2RuntimeSnapshot,
    native_routing: NativeRoutingV2,
) -> ClientUiFactsV2 {
    ClientUiFactsV2 {
        overall: Some(RuntimeStateV2 {
            state: match snapshot.overall.phase {
                V2OverallPhase::Disconnected => ClientOverallStateV2::Disconnected,
                V2OverallPhase::Starting => ClientOverallStateV2::Starting,
                V2OverallPhase::WaitingForGateway => ClientOverallStateV2::WaitingForGateway,
                V2OverallPhase::Connected => ClientOverallStateV2::Connected,
                V2OverallPhase::Degraded => ClientOverallStateV2::Degraded,
                V2OverallPhase::Blocked => ClientOverallStateV2::Blocked,
            },
            reason_code: runtime_reason_code(snapshot.overall.reason_code),
        }),
        gateway_attachment: Some(GatewayAttachmentV2 {
            state: match snapshot.gateway_attachment.phase {
                V2GatewayAttachmentPhase::Inactive => GatewayAttachmentStateV2::Unknown,
                V2GatewayAttachmentPhase::ResolvingThroughPlatform => {
                    GatewayAttachmentStateV2::ResolvingThroughPlatform
                }
                V2GatewayAttachmentPhase::ProvisioningScope => {
                    GatewayAttachmentStateV2::ProvisioningScope
                }
                V2GatewayAttachmentPhase::Connecting => GatewayAttachmentStateV2::Connecting,
                V2GatewayAttachmentPhase::Attached => GatewayAttachmentStateV2::Attached,
                V2GatewayAttachmentPhase::Unavailable => GatewayAttachmentStateV2::Unavailable,
                V2GatewayAttachmentPhase::Rejected => GatewayAttachmentStateV2::Rejected,
                V2GatewayAttachmentPhase::TlsFailed => GatewayAttachmentStateV2::TlsFailed,
            },
            endpoint: snapshot.gateway_attachment.endpoint,
            reason_code: runtime_reason_code(snapshot.gateway_attachment.reason_code),
        }),
        this_peer: snapshot.this_peer.map(|peer| ThisPeerV2 {
            peer_id: peer.peer_id,
            overlay_cidr: format!("{}/32", peer.overlay_ip),
        }),
        mesh: RuntimeStateV2 {
            state: match snapshot.mesh.phase {
                V2MeshPhase::Unavailable => MeshStateV2::Unavailable,
                V2MeshPhase::Syncing => MeshStateV2::Syncing,
                V2MeshPhase::Healthy => MeshStateV2::Healthy,
                V2MeshPhase::Degraded => MeshStateV2::Degraded,
            },
            reason_code: runtime_reason_code(snapshot.mesh.reason_code),
        },
        gossip: RuntimeStateV2 {
            state: match snapshot.gossip.phase {
                V2GossipPhase::Unavailable => GossipStateV2::Unavailable,
                V2GossipPhase::Syncing => GossipStateV2::Syncing,
                V2GossipPhase::Ready => GossipStateV2::Ready,
                V2GossipPhase::Repairing => GossipStateV2::Repairing,
            },
            reason_code: runtime_reason_code(snapshot.gossip.reason_code),
        },
        native_routing,
        peer_directory: PeerDirectoryV2 {
            state: match snapshot.peer_directory.phase {
                V2PeerDirectoryPhase::Unavailable => PeerDirectoryStateV2::Unavailable,
                V2PeerDirectoryPhase::Syncing => PeerDirectoryStateV2::Syncing,
                V2PeerDirectoryPhase::Ready => PeerDirectoryStateV2::Ready,
            },
            reason_code: runtime_reason_code(snapshot.peer_directory.reason_code),
            peers: snapshot
                .peer_directory
                .peers
                .into_iter()
                .map(|peer| RemotePeerRowV2 {
                    peer_id: peer.peer_id,
                    overlay_cidr: peer
                        .overlay_ip
                        .map(|ip| format!("{ip}/32"))
                        .unwrap_or_else(|| "Unavailable".into()),
                    state: match peer.phase {
                        V2RemotePeerPhase::Syncing => RemotePeerStateV2::Syncing,
                        V2RemotePeerPhase::Ready => RemotePeerStateV2::Ready,
                        V2RemotePeerPhase::Stale => RemotePeerStateV2::Stale,
                        V2RemotePeerPhase::Unavailable => RemotePeerStateV2::Unavailable,
                    },
                    reason_code: runtime_reason_code(peer.reason_code),
                    current_path: peer.current_path.map(|path| match path {
                        V2PeerPath::Direct => PeerCurrentPathV2::Direct,
                        V2PeerPath::EncryptedRelay => PeerCurrentPathV2::EncryptedRelay,
                    }),
                    routing: match peer.routing {
                        V2RoutingPhase::Unavailable => RoutingStateV2::Unavailable,
                        V2RoutingPhase::Syncing => RoutingStateV2::Syncing,
                        V2RoutingPhase::Ready => RoutingStateV2::Ready,
                    },
                    usable_lanes: peer.usable_lanes,
                    exports: peer
                        .exports
                        .into_iter()
                        .map(|export| RemotePeerExportV2 {
                            prefix: export.prefix,
                            placement: export.placement.map(|placement| match placement {
                                V2ExportPlacement::ActiveHere => ExportPlacementV2::ActiveHere,
                                V2ExportPlacement::StandbyHere { position } => {
                                    ExportPlacementV2::StandbyHere { position }
                                }
                            }),
                        })
                        .collect(),
                })
                .collect(),
        },
        traffic: Some(TrafficCountersV2 {
            direct_tx_bytes: snapshot.traffic.direct_tx_bytes,
            direct_rx_bytes: snapshot.traffic.direct_rx_bytes,
            relay_tx_bytes: snapshot.traffic.relay_tx_bytes,
            relay_rx_bytes: snapshot.traffic.relay_rx_bytes,
        }),
        relay_usage: snapshot.relay_usage.map(|usage| RelayUsageV2 {
            used_bytes: usage.used_bytes,
            allowance_bytes: usage.allowance_bytes,
        }),
    }
}

fn runtime_reason_code(reason_code: Option<V2RuntimeReasonCode>) -> Option<String> {
    reason_code.map(|code| {
        match code {
            V2RuntimeReasonCode::RuntimeInactive => "runtime_inactive",
            V2RuntimeReasonCode::ResolvingThroughPlatform => "resolving_through_platform",
            V2RuntimeReasonCode::ConnectingToGateway => "connecting_to_gateway",
            V2RuntimeReasonCode::PlatformUnavailable => "platform_unavailable",
            V2RuntimeReasonCode::NoEligibleGateway => "no_eligible_gateway",
            V2RuntimeReasonCode::ScopeRejected => "scope_rejected",
            V2RuntimeReasonCode::GatewayTlsFailed => "gateway_tls_failed",
            V2RuntimeReasonCode::GatewayAuthenticationRejected => "gateway_authentication_rejected",
            V2RuntimeReasonCode::GatewayConnectFailed => "gateway_connect_failed",
            V2RuntimeReasonCode::GatewayUnavailable => "gateway_unavailable",
            V2RuntimeReasonCode::GatewayUnavailableDirectPreserved => {
                "gateway_unavailable_direct_preserved"
            }
            V2RuntimeReasonCode::MembershipCyclePending => "membership_cycle_pending",
            V2RuntimeReasonCode::InitialFullSyncPending => "initial_full_sync_pending",
            V2RuntimeReasonCode::PeerLinkUnavailable => "peer_link_unavailable",
            V2RuntimeReasonCode::NoUsablePeerPath => "no_usable_peer_path",
            V2RuntimeReasonCode::RuntimeFailed => "runtime_failed",
        }
        .to_string()
    })
}

pub fn project_client_ui_status(
    _status: &ConnectionStatus,
    facts: ClientUiFactsV2,
) -> ClientUiStatusV2 {
    let overall = facts.overall.unwrap_or_else(|| RuntimeStateV2 {
        state: ClientOverallStateV2::Disconnected,
        reason_code: Some("runtime_snapshot_unavailable".into()),
    });
    let gateway_attachment = facts
        .gateway_attachment
        .unwrap_or_else(|| GatewayAttachmentV2 {
            state: GatewayAttachmentStateV2::Unknown,
            endpoint: None,
            reason_code: Some("runtime_snapshot_unavailable".into()),
        });

    ClientUiStatusV2 {
        overall: overall.state,
        overall_reason_code: overall.reason_code,
        gateway_attachment,
        this_peer: facts.this_peer,
        mesh: facts.mesh,
        gossip: facts.gossip,
        native_routing: facts.native_routing,
        peer_directory: facts.peer_directory,
        traffic: facts.traffic.unwrap_or_default(),
        relay_usage: facts.relay_usage,
    }
}
