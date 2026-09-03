use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tp_client::runtime_snapshot::V2RuntimeReasonCode;
use tp_client::runtime_snapshot::{
    V2ExportPlacement, V2GatewayAttachmentPhase, V2GossipPhase, V2MeshPhase, V2OverallPhase,
    V2PeerDirectoryPhase, V2PeerPath, V2RemotePeerPhase,
};
use tp_client::status::NullListener;
use tp_client::{Engine, EngineConfig};
use tp_core::config::{ClientP2pConfig, GatewayP2pConfig};
use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};
use tp_gateway::{Gateway, GatewayServer};
use tp_transport::{QuicServer, QuicTuning};

fn static_peer() -> tp_core::provisioning::PeerProfileV2 {
    let mut owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "127.0.0.1".into(),
        port: 9,
        mapping_port: None,
        tls_server_name: Some("gateway.example".into()),
        trusted_certificate_pem: None,
    })
    .expect("Tunnel");
    owner
        .add_peer(Some(Ipv4Addr::new(198, 18, 0, 7)), 1, None)
        .expect("Peer")
}

#[tokio::test(flavor = "current_thread")]
async fn connect_publishes_one_secret_free_v2_runtime_snapshot_before_dialing() {
    let profile = static_peer();
    let private_key = profile.peer.peer_private_key.clone();
    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));

    engine
        .connect_with_peer_profile(profile.clone(), None)
        .await
        .expect("start V2 Peer");

    let snapshot = engine.v2_runtime_snapshot();
    assert_eq!(snapshot.overall.phase, V2OverallPhase::WaitingForGateway);
    assert_eq!(
        snapshot.gateway_attachment.phase,
        V2GatewayAttachmentPhase::Connecting
    );
    assert_eq!(
        snapshot
            .this_peer
            .as_ref()
            .map(|peer| (peer.peer_id.as_str(), peer.overlay_ip)),
        Some((profile.peer.peer_id.as_str(), profile.peer.overlay_ip))
    );
    assert_eq!(snapshot.peer_directory.phase, V2PeerDirectoryPhase::Syncing);
    assert!(snapshot.peer_directory.peers.is_empty());
    assert_eq!(snapshot.traffic, Default::default());

    let public_json = serde_json::to_string(&snapshot).expect("public snapshot JSON");
    assert!(!public_json.contains(private_key.as_str()));
    assert!(!public_json.contains("peer_private_key"));

    engine.disconnect().await;
}

#[tokio::test]
async fn attachment_snapshot_becomes_attached_only_after_real_v2_gateway_auth() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("certificate");
    let server_tls = tp_transport::tls::server_config(
        vec![CertificateDer::from(certified.cert.der().to_vec())],
        PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
    )
    .expect("server TLS");
    let server = QuicServer::bind(
        "127.0.0.1:0".parse().expect("bind address"),
        server_tls,
        QuicTuning::game_streaming(),
    )
    .expect("bind Gateway");
    let gateway_addr = server.local_addr().expect("Gateway address");
    let gateway_facts = GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "127.0.0.1".into(),
        port: gateway_addr.port(),
        mapping_port: None,
        tls_server_name: Some("localhost".into()),
        trusted_certificate_pem: Some(certified.cert.pem()),
    };
    let mut owner = TunnelOwnerFileV2::generate(gateway_facts).expect("Tunnel");
    let scope = owner.scope().expect("Scope");
    let profile = owner.add_peer(None, 1, None).expect("Peer");
    let gateway = Gateway::new(GatewayP2pConfig::default(), None);
    gateway
        .scopes()
        .replace_managed_snapshot(vec![scope])
        .expect("install Scope");
    let serving_gateway = gateway.clone();
    let server_task = tokio::spawn(async move {
        let _ = serving_gateway.serve(GatewayServer::Quic(server)).await;
    });

    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    engine
        .connect_with_peer_profile(profile, None)
        .await
        .expect("start V2 Peer");

    let attached = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = engine.v2_runtime_snapshot();
            if snapshot.gateway_attachment.phase == V2GatewayAttachmentPhase::Attached {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Gateway attachment did not become ready");
    assert!(attached.gateway_attachment.endpoint.is_some());
    assert!(attached.gateway_attachment.reason_code.is_none());

    engine.disconnect().await;
    let disconnected = engine.v2_runtime_snapshot();
    assert_eq!(disconnected.overall.phase, V2OverallPhase::Disconnected);
    assert_eq!(
        disconnected.gateway_attachment.phase,
        V2GatewayAttachmentPhase::Inactive
    );
    assert!(disconnected.this_peer.is_none());
    server_task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn managed_resolve_failure_is_typed_without_legacy_heartbeat_inference() {
    let mut profile = static_peer();
    profile.bootstrap = tp_core::provisioning::PeerBootstrapV2::ManagedPlatform {
        platform_url: "https://127.0.0.1:9".into(),
    };
    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));

    engine
        .connect_with_peer_profile(profile, None)
        .await
        .expect("schedule Managed resolve");
    let resolving = engine.v2_runtime_snapshot();
    assert_eq!(
        resolving.gateway_attachment.phase,
        V2GatewayAttachmentPhase::ResolvingThroughPlatform
    );
    assert_eq!(
        resolving.gateway_attachment.reason_code,
        Some(V2RuntimeReasonCode::ResolvingThroughPlatform)
    );
    assert!(resolving.gateway_attachment.endpoint.is_none());

    let failed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = engine.v2_runtime_snapshot();
            if snapshot.gateway_attachment.phase == V2GatewayAttachmentPhase::Unavailable
                && snapshot.overall.phase == V2OverallPhase::Blocked
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Managed resolve failure did not reach runtime snapshot");
    assert_eq!(
        failed.gateway_attachment.reason_code,
        Some(V2RuntimeReasonCode::PlatformUnavailable)
    );
    assert_eq!(failed.overall.phase, V2OverallPhase::Blocked);
    assert!(failed.gateway_attachment.endpoint.is_none());

    engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_peerlink_full_sync_publishes_remote_runtime_and_local_placement() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("certificate");
    let server_tls = tp_transport::tls::server_config(
        vec![CertificateDer::from(certified.cert.der().to_vec())],
        PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
    )
    .expect("server TLS");
    let server = QuicServer::bind(
        "127.0.0.1:0".parse().expect("bind address"),
        server_tls,
        QuicTuning::game_streaming(),
    )
    .expect("bind Gateway");
    let gateway_addr = server.local_addr().expect("Gateway address");
    let gateway_facts = GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "127.0.0.1".into(),
        port: gateway_addr.port(),
        mapping_port: None,
        tls_server_name: Some("localhost".into()),
        trusted_certificate_pem: Some(certified.cert.pem()),
    };
    let mut owner = TunnelOwnerFileV2::generate(gateway_facts).expect("Tunnel");
    let scope = owner.scope().expect("Scope");
    let peer_a = owner.add_peer(None, 1, None).expect("Peer A");
    let peer_b = owner.add_peer(None, 1, None).expect("Peer B");
    let gateway = Gateway::new(GatewayP2pConfig::default(), None);
    gateway
        .scopes()
        .replace_managed_snapshot(vec![scope])
        .expect("install Scope");
    let serving_gateway = gateway.clone();
    let server_task = tokio::spawn(async move {
        let _ = serving_gateway.serve(GatewayServer::Quic(server)).await;
    });

    let engine_a = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    let engine_b = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    let export_prefix = tp_client::discover_connected_lan_prefixes()
        .expect("connected LAN inventory for full-sync integration test")
        .into_iter()
        .next()
        .expect("a live physical RFC1918 LAN is required for the full-sync integration test");
    engine_a
        .set_v2_local_runtime_record(
            tp_client::peer_runtime::PeerRuntimeRecordV2::new(vec![
                tp_client::peer_runtime::LanExportV2 {
                    prefix: export_prefix,
                    ready: true,
                },
            ])
            .expect("runtime record"),
        )
        .expect("set local runtime record");
    let p2p = ClientP2pConfig {
        attempt_after_relay_uptime_secs: 0,
        cooldown_initial_secs: 1,
        cooldown_max_secs: 1,
        ..Default::default()
    };
    for (engine, profile) in [(&engine_a, peer_a.clone()), (&engine_b, peer_b.clone())] {
        engine.set_p2p_config(Arc::new(p2p.clone()));
        engine
            .connect_with_peer_profile(profile, None)
            .await
            .expect("connect V2 Peer");
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if [&engine_a, &engine_b].iter().all(|engine| {
                engine.v2_runtime_snapshot().gateway_attachment.phase
                    == V2GatewayAttachmentPhase::Attached
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both Peers must attach before the first membership announce");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if [(&engine_a, &peer_a), (&engine_b, &peer_b)]
                .iter()
                .all(|(engine, profile)| {
                    engine.latest_tunnel_config().is_some_and(|config| {
                        config.client_ids.first().is_some_and(|replica_id| {
                            gateway
                                .peers
                                .stable_peer_id(&profile.tunnel_id, replica_id)
                                .as_deref()
                                == Some(profile.peer.peer_id.as_str())
                        })
                    })
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both V2 Replica attachments must bind to their exact Peer identities");

    let (bootstrap_result_tx, mut bootstrap_result_rx) = tokio::sync::mpsc::channel(2);
    for engine in [&engine_a, &engine_b] {
        let bootstrap_engine = engine.clone();
        let bootstrap_cancel = engine.task_cancel_token();
        let bootstrap_config = p2p.clone();
        let bootstrap_result_tx = bootstrap_result_tx.clone();
        engine.tasks().spawn(async move {
            let result = tp_client::p2p::bootstrap::run(
                bootstrap_engine,
                bootstrap_config,
                bootstrap_cancel,
            )
            .await;
            let _ = bootstrap_result_tx.send(result).await;
        });
    }
    drop(bootstrap_result_tx);

    tokio::time::timeout(Duration::from_secs(60), async {
        for _ in 0..2 {
            bootstrap_result_rx
                .recv()
                .await
                .expect("P2P bootstrap result")
                .expect("P2P bootstrap");
        }
    })
    .await
    .expect("both P2P managers must start before waiting for full sync");

    let snapshot_b = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let snapshot = engine_b.v2_runtime_snapshot();
            if snapshot.peer_directory.phase == V2PeerDirectoryPhase::Ready
                && snapshot.peer_directory.peers.iter().any(|peer| {
                    peer.peer_id == peer_a.peer.peer_id
                        && peer.phase == V2RemotePeerPhase::Ready
                        && peer.overlay_ip == Some(peer_a.peer.overlay_ip)
                        && !peer.exports.is_empty()
                })
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "PeerLink initial full sync did not become ready: {:?}; p2p_a={:?}; p2p_b={:?}",
            engine_b.v2_runtime_snapshot(),
            engine_a.multi_session().map(|multi| multi.p2p_state()),
            engine_b.multi_session().map(|multi| multi.p2p_state()),
        )
    });
    assert_eq!(
        snapshot_b.mesh.phase,
        V2MeshPhase::Healthy,
        "snapshot after full sync: {snapshot_b:#?}"
    );
    assert_eq!(snapshot_b.gossip.phase, V2GossipPhase::Ready);
    let remote_a = snapshot_b
        .peer_directory
        .peers
        .iter()
        .find(|peer| peer.peer_id == peer_a.peer.peer_id)
        .expect("known Peer A");
    assert_eq!(remote_a.overlay_ip, Some(peer_a.peer.overlay_ip));
    assert_eq!(remote_a.phase, V2RemotePeerPhase::Ready);
    assert_eq!(remote_a.current_path, Some(V2PeerPath::EncryptedRelay));
    assert_eq!(remote_a.usable_lanes, Some(1));
    assert_eq!(remote_a.exports.len(), 1);
    assert_eq!(
        remote_a.exports[0].prefix,
        format!("{}/{}", export_prefix.network, export_prefix.prefix_len)
    );
    assert_eq!(
        remote_a.exports[0].placement,
        Some(V2ExportPlacement::ActiveHere)
    );

    engine_a.disconnect().await;
    engine_b.disconnect().await;
    server_task.abort();
}
