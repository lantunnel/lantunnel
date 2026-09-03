//! Tunnel client engine — headless. Dials the gateway via the configured transport, handles
//! `Connect` requests by dialing the target locally, and pipes bytes back.
//!
//! UI-agnostic: `StatusListener` callbacks let CLI and GUI render live status.

pub mod access_policy;
pub mod client_ui;
pub mod engine;
pub mod error;
pub mod host_filter;
pub(crate) mod link;
pub mod local_target;
pub mod managed_resolve;
pub(crate) mod native_route_guard;
pub mod overlay;
pub mod p2p;
pub mod peer_gossip;
pub mod peer_heartbeat;
pub mod peer_link_manager;
pub mod peer_runtime;
pub mod platform;
pub mod proxy_mode;
pub mod proxy_tunnel;
pub mod relay_crypto;
pub mod route_matcher;
pub mod runtime_snapshot;
pub mod status;
mod v2_attachment;

pub use engine::{Engine, EngineConfig};
pub use error::{EngineError, Result};
pub use host_filter::HostFilter;
pub use managed_resolve::resolve_managed_gateway;
pub use native_route_guard::{discover_connected_lan_prefixes, NativeRouteGuardError};
pub use platform::TunnelConfig;
pub use status::{ConnectionPathMode, ConnectionStatus, HeartbeatStatus, StatusListener};

/// The settled overall-state decision, exposed so its ordering can be tested.
///
/// A Client with an attached Gateway is reachable whoever else is offline; the
/// old order asked about other Peers first and called this device Blocked.
pub fn overall_phase_for_test(
    gateway: runtime_snapshot::V2GatewayAttachmentPhase,
    peers: &[runtime_snapshot::V2RemotePeerPhase],
    any_direct: bool,
) -> (
    runtime_snapshot::V2OverallPhase,
    Option<runtime_snapshot::V2RuntimeReasonCode>,
) {
    use runtime_snapshot::V2RemotePeerPhase;
    let any_unavailable = peers
        .iter()
        .any(|p| matches!(p, V2RemotePeerPhase::Stale | V2RemotePeerPhase::Unavailable));
    let any_usable = peers.iter().any(|p| matches!(p, V2RemotePeerPhase::Ready));
    engine::settled_overall_phase(
        gateway,
        any_direct,
        any_unavailable,
        any_usable,
        !peers.is_empty(),
        None,
    )
}
