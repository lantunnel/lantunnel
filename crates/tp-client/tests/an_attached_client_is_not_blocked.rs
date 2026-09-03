//! A Client with an attached Gateway is reachable, whoever else is offline.
//!
//! The overall state checked "is any Peer unavailable" before it checked
//! whether this device had a path at all. A Tunnel whose other devices happen
//! to be switched off left this one reading "Blocked — no way to reach this
//! device yet" with a healthy Gateway attachment on the line above it. Nothing
//! was wrong with this device; there was simply nobody else on.
use tp_client::runtime_snapshot::{
    V2GatewayAttachmentPhase, V2OverallPhase, V2RemotePeerPhase, V2RuntimeReasonCode,
};

#[test]
fn an_attached_gateway_with_every_peer_offline_is_not_blocked() {
    let phase = tp_client::overall_phase_for_test(
        V2GatewayAttachmentPhase::Attached,
        &[V2RemotePeerPhase::Unavailable],
        false,
    );

    assert_ne!(
        phase.0,
        V2OverallPhase::Blocked,
        "this device is attached and reachable; the others being off is their state"
    );
    assert_ne!(phase.1, Some(V2RuntimeReasonCode::NoUsablePeerPath));
}

#[test]
fn an_attached_gateway_with_no_peers_at_all_is_connected() {
    let phase = tp_client::overall_phase_for_test(V2GatewayAttachmentPhase::Attached, &[], false);

    assert_eq!(phase.0, V2OverallPhase::Connected);
    assert_eq!(phase.1, None);
}

#[test]
fn no_gateway_and_no_direct_path_is_still_blocked() {
    let phase = tp_client::overall_phase_for_test(
        V2GatewayAttachmentPhase::Unavailable,
        &[V2RemotePeerPhase::Unavailable],
        false,
    );

    assert_eq!(phase.0, V2OverallPhase::Blocked);
}
