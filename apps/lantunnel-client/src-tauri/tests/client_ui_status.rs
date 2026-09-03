use lantunnel_client::client_ui_status::{
    client_settings_sections_v2, project_client_settings, project_client_status_read_model,
    project_client_ui_status, project_engine_runtime_snapshot, project_native_routing,
    ClientSettingsFactsV2, ClientSettingsSectionV2, ClientUiFactsV2, ExportPlacementV2,
    GatewayAttachmentStateV2, MeshStateV2, NativeRoutingActionV2, NativeRoutingApplyResultV2,
    NativeRoutingStateV2, PeerCurrentPathV2, PeerDirectoryStateV2, RemotePeerExportV2,
    RemotePeerRowV2, RemotePeerStateV2, RoutingStateV2, SettingAvailabilityV2,
};
use tp_client::runtime_snapshot::{
    V2ExportPlacement, V2GatewayAttachmentPhase, V2MeshPhase, V2OverallPhase, V2PeerDirectoryPhase,
    V2PeerPath, V2RemoteExportSnapshot, V2RemotePeerPhase, V2RemotePeerSnapshot, V2RoutingPhase,
    V2RuntimeReasonCode, V2RuntimeSnapshot,
};
use tp_client::status::{ConnectionStatus, HeartbeatStatus, TrafficStats};

#[test]
fn settings_contract_has_exactly_the_four_v2_sections() {
    assert_eq!(
        client_settings_sections_v2(),
        [
            ClientSettingsSectionV2::Connection,
            ClientSettingsSectionV2::NetworkAndLanExport,
            ClientSettingsSectionV2::ClientAccess,
            ClientSettingsSectionV2::Diagnostics,
        ]
    );
}

#[test]
fn status_read_model_adds_v2_truth_without_removing_existing_status_fields() {
    let status = ConnectionStatus {
        connected: true,
        message: "Connected".into(),
        ..Default::default()
    };

    let json = serde_json::to_value(project_client_status_read_model(
        status,
        ClientUiFactsV2::default(),
    ))
    .expect("serialize status read model");

    assert_eq!(json["connected"], true);
    assert_eq!(json["message"], "Connected");
    assert_eq!(json["client_ui"]["mesh"]["state"], "unknown");
    assert_eq!(json["client_ui"]["peer_directory"]["state"], "unavailable");
}

#[test]
fn settings_projection_does_not_claim_unwired_acl_or_export_consumers_are_active() {
    let projected = project_client_settings(ClientSettingsFactsV2::default());

    assert_eq!(
        projected.client_access.availability,
        SettingAvailabilityV2::Unavailable
    );
    assert!(projected.client_access.value.is_none());
    assert_eq!(
        projected.exported_lans.availability,
        SettingAvailabilityV2::Unavailable
    );
    assert!(projected.exported_lans.value.is_none());
    assert_eq!(
        projected.tunnel_first.availability,
        SettingAvailabilityV2::Unavailable
    );
}

#[test]
fn projection_keeps_missing_runtime_facts_explicitly_unavailable() {
    let status = ConnectionStatus {
        connected: true,
        gateway_addr: Some("gateway.example:443".into()),
        transport_heartbeat: HeartbeatStatus {
            active: true,
            ..Default::default()
        },
        p2p_peer_count: 7,
        traffic: TrafficStats {
            relay_tx_bytes: 11,
            relay_rx_bytes: 13,
            p2p_tx_bytes: 17,
            p2p_rx_bytes: 19,
        },
        ..Default::default()
    };

    let projected = project_client_ui_status(&status, ClientUiFactsV2::default());

    assert_eq!(
        projected.gateway_attachment.state,
        GatewayAttachmentStateV2::Unknown
    );
    assert_eq!(
        projected.gateway_attachment.reason_code.as_deref(),
        Some("runtime_snapshot_unavailable")
    );
    assert_eq!(projected.mesh.state, MeshStateV2::Unknown);
    assert_eq!(
        projected.peer_directory.state,
        PeerDirectoryStateV2::Unavailable
    );
    assert!(projected.peer_directory.peers.is_empty());
    assert_eq!(
        projected.native_routing.state,
        NativeRoutingStateV2::Unknown
    );
    assert_eq!(projected.traffic, Default::default());
}

#[test]
fn native_projection_only_reports_ready_after_the_existing_apply_result_is_running() {
    let ready = project_native_routing(true, NativeRoutingApplyResultV2::Applied, false, true);
    assert_eq!(ready.state, NativeRoutingStateV2::Ready);
    assert!(ready.actions.is_empty());

    let missing_helper =
        project_native_routing(true, NativeRoutingApplyResultV2::Unavailable, true, false);
    assert_eq!(missing_helper.state, NativeRoutingStateV2::NeedsHelper);
    assert_eq!(
        missing_helper.actions,
        vec![NativeRoutingActionV2::InstallHelper]
    );

    let no_apply_result =
        project_native_routing(true, NativeRoutingApplyResultV2::Unavailable, false, true);
    assert_eq!(no_apply_result.state, NativeRoutingStateV2::Unknown);
    assert_ne!(no_apply_result.state, NativeRoutingStateV2::Ready);
}

#[test]
fn one_engine_snapshot_projects_typed_v2_truth_and_real_counters() {
    let mut snapshot = V2RuntimeSnapshot::default();
    snapshot.overall.phase = V2OverallPhase::Degraded;
    snapshot.overall.reason_code = Some(V2RuntimeReasonCode::GatewayUnavailableDirectPreserved);
    snapshot.gateway_attachment.phase = V2GatewayAttachmentPhase::Unavailable;
    snapshot.gateway_attachment.endpoint = Some("gateway.example:443".into());
    snapshot.gateway_attachment.reason_code =
        Some(V2RuntimeReasonCode::GatewayUnavailableDirectPreserved);
    snapshot.mesh.phase = V2MeshPhase::Healthy;
    snapshot.mesh.reason_code = None;
    snapshot.peer_directory.phase = V2PeerDirectoryPhase::Ready;
    snapshot.peer_directory.reason_code = None;
    snapshot.peer_directory.peers = vec![V2RemotePeerSnapshot {
        peer_id: "peer-b".into(),
        overlay_ip: Some("198.18.42.11".parse().expect("Overlay IP")),
        phase: V2RemotePeerPhase::Ready,
        reason_code: None,
        current_path: Some(V2PeerPath::Direct),
        usable_lanes: Some(1),
        routing: V2RoutingPhase::Ready,
        exports: vec![V2RemoteExportSnapshot {
            prefix: "192.168.0.0/24".into(),
            placement: Some(V2ExportPlacement::StandbyHere { position: 1 }),
        }],
    }];
    snapshot.traffic.direct_tx_bytes = 101;
    snapshot.traffic.direct_rx_bytes = 103;
    snapshot.traffic.relay_tx_bytes = 107;
    snapshot.traffic.relay_rx_bytes = 109;

    let facts = project_engine_runtime_snapshot(
        snapshot,
        project_native_routing(true, NativeRoutingApplyResultV2::Applied, false, true),
    );
    let projected = project_client_ui_status(
        &ConnectionStatus {
            connected: true,
            transport_heartbeat: HeartbeatStatus {
                active: true,
                ..Default::default()
            },
            traffic: TrafficStats {
                relay_tx_bytes: 1,
                relay_rx_bytes: 2,
                p2p_tx_bytes: 3,
                p2p_rx_bytes: 4,
            },
            ..Default::default()
        },
        facts,
    );

    assert_eq!(
        projected.gateway_attachment.state,
        GatewayAttachmentStateV2::Unavailable
    );
    assert_eq!(
        projected.gateway_attachment.reason_code.as_deref(),
        Some("gateway_unavailable_direct_preserved")
    );
    assert_eq!(projected.mesh.state, MeshStateV2::Healthy);
    assert_eq!(
        projected.peer_directory.peers[0].overlay_cidr,
        "198.18.42.11/32"
    );
    assert_eq!(
        projected.peer_directory.peers[0].current_path,
        Some(PeerCurrentPathV2::Direct)
    );
    assert_eq!(projected.peer_directory.peers[0].usable_lanes, Some(1));
    assert_eq!(projected.traffic.direct_tx_bytes, 101);
    assert_eq!(projected.traffic.direct_rx_bytes, 103);
    assert_eq!(projected.traffic.relay_tx_bytes, 107);
    assert_eq!(projected.traffic.relay_rx_bytes, 109);
}

#[test]
fn inactive_engine_snapshot_does_not_infer_attached_from_legacy_heartbeat() {
    let projected = project_client_ui_status(
        &ConnectionStatus {
            connected: true,
            transport_heartbeat: HeartbeatStatus {
                active: true,
                ..Default::default()
            },
            ..Default::default()
        },
        project_engine_runtime_snapshot(
            V2RuntimeSnapshot::default(),
            project_native_routing(true, NativeRoutingApplyResultV2::Unavailable, false, true),
        ),
    );

    assert_eq!(
        projected.gateway_attachment.state,
        GatewayAttachmentStateV2::Unknown
    );
    assert_eq!(
        projected.gateway_attachment.reason_code.as_deref(),
        Some("runtime_inactive")
    );
    assert_eq!(
        projected.overall,
        lantunnel_client::client_ui_status::ClientOverallStateV2::Disconnected
    );
    assert_eq!(projected.traffic, Default::default());
}

#[test]
fn projection_preserves_backend_owned_peer_path_and_local_export_placement() {
    let facts = ClientUiFactsV2 {
        peer_directory: lantunnel_client::client_ui_status::PeerDirectoryV2 {
            state: PeerDirectoryStateV2::Ready,
            reason_code: None,
            peers: vec![RemotePeerRowV2 {
                peer_id: "peer-b".into(),
                overlay_cidr: "198.18.42.11/32".into(),
                state: RemotePeerStateV2::Ready,
                reason_code: None,
                current_path: Some(PeerCurrentPathV2::Direct),
                routing: RoutingStateV2::Ready,
                usable_lanes: Some(3),
                exports: vec![RemotePeerExportV2 {
                    prefix: "192.168.0.0/24".into(),
                    placement: Some(ExportPlacementV2::ActiveHere),
                }],
            }],
        },
        ..Default::default()
    };

    let projected = project_client_ui_status(&ConnectionStatus::default(), facts);
    let peer = &projected.peer_directory.peers[0];

    assert_eq!(peer.current_path, Some(PeerCurrentPathV2::Direct));
    assert_eq!(peer.routing, RoutingStateV2::Ready);
    assert_eq!(
        peer.exports[0].placement,
        Some(ExportPlacementV2::ActiveHere)
    );
}
