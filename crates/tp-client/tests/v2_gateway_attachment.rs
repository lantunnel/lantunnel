use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::oneshot;
use tp_client::status::NullListener;
use tp_client::{Engine, EngineConfig};
use tp_core::config::GatewayP2pConfig;
use tp_core::protocol::BinaryMessage;
use tp_core::provisioning::{GatewayBootstrapV2, PeerBootstrapV2, TunnelOwnerFileV2};
use tp_gateway::{Gateway, GatewayServer};
use tp_transport::{
    AuthHandler, AuthParams, GrpcServer, QuicServer, QuicTuning, Session, WsServer,
};

fn profile() -> tp_core::provisioning::PeerProfileV2 {
    TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example".into(),
        port: 8443,
        mapping_port: None,
        tls_server_name: None,
        trusted_certificate_pem: None,
    })
    .expect("Tunnel")
    .add_peer(None, 1, None)
    .expect("Peer")
}

#[tokio::test]
async fn managed_peer_rejects_a_static_gateway_override() {
    let mut profile = profile();
    profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
        platform_url: "https://platform.example".into(),
    };
    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    let static_gateway = GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example".into(),
        port: 8443,
        mapping_port: None,
        tls_server_name: None,
        trusted_certificate_pem: None,
    };

    let error = engine
        .connect_with_peer_profile(profile, Some(static_gateway))
        .await
        .expect_err("Managed bootstrap cannot be overridden locally")
        .to_string();

    assert!(error.contains("do not allow static Gateway overrides"));
    assert!(!engine.status().connecting);
}

struct ExpectedV2Auth {
    tunnel_id: String,
    tcp_flow_stream_v1: bool,
}

#[async_trait]
impl AuthHandler for ExpectedV2Auth {
    async fn authenticate(&self, auth: &AuthParams) -> Result<(), String> {
        if auth.tunnel_id == self.tunnel_id
            && auth.client_id.starts_with(&format!("{}-", self.tunnel_id))
            && auth.capabilities.peer_mesh_v2
            && auth.capabilities.route_bind_control_v1
            && auth.capabilities.tcp_flow_stream_v1 == self.tcp_flow_stream_v1
            && auth.capabilities.relay_source_attestation_v1
            && auth.group_id.is_empty()
            && auth.username.is_empty()
            && auth.password.is_empty()
            && auth.group_password.is_empty()
        {
            Ok(())
        } else {
            Err("unexpected V2 transport Auth facts".into())
        }
    }
}

async fn verify_gateway_attachment(
    auth: AuthParams,
    mut session: Session,
    profile: tp_core::provisioning::PeerProfileV2,
    proof_tx: oneshot::Sender<Result<(), String>>,
) {
    let challenge = [0x5a; 32];
    session
        .send(BinaryMessage::AuthV2Challenge { challenge })
        .await
        .expect("send challenge");
    let result = match session.recv().await {
        Some(BinaryMessage::AuthV2Proof {
            membership,
            signature,
        }) if membership == profile.public_membership() => membership
            .verify_attachment_proof(&challenge, &auth.client_id, &signature)
            .map_err(|error| error.to_string()),
        other => Err(format!("unexpected proof: {other:?}")),
    };
    let _ = proof_tx.send(result);
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn engine_completes_v2_auth_over_quic_with_exact_self_signed_ip_san_leaf() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).expect("test certificate");
    let certificate_pem = certified.cert.pem();
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
    .expect("bind QUIC server");
    let gateway_addr = server.local_addr().expect("Gateway address");
    let gateway = GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "127.0.0.1".into(),
        port: gateway_addr.port(),
        mapping_port: None,
        tls_server_name: Some("127.0.0.1".into()),
        trusted_certificate_pem: Some(certificate_pem),
    };
    let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
    let scope = owner.scope().expect("Scope");
    let profile = owner.add_peer(None, 1, None).expect("Peer");
    let expected_tunnel = profile.tunnel_id.clone();
    let expected_peer = profile.peer.peer_id.clone();
    let gateway_runtime = Gateway::new(GatewayP2pConfig::default(), None);
    gateway_runtime
        .scopes()
        .replace_managed_snapshot(vec![scope])
        .expect("install Scope");
    let serving_gateway = gateway_runtime.clone();
    let server_task = tokio::spawn(async move {
        let _ = serving_gateway.serve(GatewayServer::Quic(server)).await;
    });

    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    engine
        .connect_with_peer_profile(profile, None)
        .await
        .expect("start V2 Client");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.v2_runtime_snapshot().gateway_attachment.phase
                == tp_client::runtime_snapshot::V2GatewayAttachmentPhase::Attached
                && engine.latest_tunnel_config().is_some_and(|config| {
                    config.client_ids.first().is_some_and(|replica_id| {
                        gateway_runtime
                            .peers
                            .stable_peer_id(&expected_tunnel, replica_id)
                            .as_deref()
                            == Some(expected_peer.as_str())
                    })
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("V2 Peer did not complete Gateway attachment");

    engine.disconnect().await;
    server_task.abort();
}

#[tokio::test]
async fn engine_completes_v2_auth_over_websocket_with_ip_dial_and_separate_sni() {
    let certified = rcgen::generate_simple_self_signed(vec!["gateway.example".into()])
        .expect("test certificate");
    let certificate_pem = certified.cert.pem();
    let server_tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certified.cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
        )
        .expect("WebSocket TLS");
    let server = WsServer::bind_tls(
        "127.0.0.1:0".parse().expect("bind address"),
        Arc::new(server_tls),
    )
    .await
    .expect("bind WebSocket server");
    let gateway_addr = server.local_addr().expect("Gateway address");
    let gateway = GatewayBootstrapV2 {
        transport: "websocket".into(),
        dial_address: "127.0.0.1".into(),
        port: gateway_addr.port(),
        mapping_port: None,
        tls_server_name: Some("gateway.example".into()),
        trusted_certificate_pem: Some(certificate_pem),
    };
    let profile = TunnelOwnerFileV2::generate(gateway)
        .expect("Tunnel")
        .add_peer(None, 1, None)
        .expect("Peer");
    let expected_profile = profile.clone();
    let expected_tunnel = profile.tunnel_id.clone();
    let (proof_tx, proof_rx) = oneshot::channel();
    let server_task = tokio::spawn(async move {
        let (auth, session) = server
            .accept(&ExpectedV2Auth {
                tunnel_id: expected_tunnel,
                tcp_flow_stream_v1: false,
            })
            .await
            .expect("WebSocket accept")
            .expect("WebSocket connection");
        verify_gateway_attachment(auth, session, expected_profile, proof_tx).await;
    });

    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    engine
        .connect_with_peer_profile(profile, None)
        .await
        .expect("start V2 Client");
    tokio::time::timeout(Duration::from_secs(5), proof_rx)
        .await
        .expect("V2 proof timeout")
        .expect("proof result channel")
        .expect("valid V2 proof");

    engine.disconnect().await;
    server_task.abort();
}

#[tokio::test]
async fn engine_completes_v2_auth_over_grpc_with_ip_dial_and_separate_sni() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("test certificate");
    let certificate_pem = certified.cert.pem();
    let private_key_pem = certified.key_pair.serialize_pem();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let gateway_addr = reservation.local_addr().expect("Gateway address");
    drop(reservation);
    let gateway = GatewayBootstrapV2 {
        transport: "grpc".into(),
        dial_address: "127.0.0.1".into(),
        port: gateway_addr.port(),
        mapping_port: None,
        tls_server_name: Some("localhost".into()),
        trusted_certificate_pem: Some(certificate_pem.clone()),
    };
    let profile = TunnelOwnerFileV2::generate(gateway)
        .expect("Tunnel")
        .add_peer(None, 1, None)
        .expect("Peer");
    let expected_profile = profile.clone();
    let expected_tunnel = profile.tunnel_id.clone();
    let (session_tx, session_rx) = oneshot::channel();
    let session_tx = Arc::new(Mutex::new(Some(session_tx)));
    let server_task = tokio::spawn({
        let session_tx = Arc::clone(&session_tx);
        async move {
            GrpcServer::new(gateway_addr)
                .with_tls(certificate_pem.into_bytes(), private_key_pem.into_bytes())
                .serve(
                    Arc::new(ExpectedV2Auth {
                        tunnel_id: expected_tunnel,
                        tcp_flow_stream_v1: false,
                    }),
                    move |auth, session| {
                        if let Some(sender) = session_tx.lock().expect("session sender").take() {
                            let _ = sender.send((auth, session));
                        }
                    },
                )
                .await
        }
    });
    let (proof_tx, proof_rx) = oneshot::channel();
    let proof_task = tokio::spawn(async move {
        let (auth, session) = session_rx.await.expect("gRPC session");
        verify_gateway_attachment(auth, session, expected_profile, proof_tx).await;
    });

    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    engine
        .connect_with_peer_profile(profile, None)
        .await
        .expect("start V2 Client");
    tokio::time::timeout(Duration::from_secs(5), proof_rx)
        .await
        .expect("V2 proof timeout")
        .expect("proof result channel")
        .expect("valid V2 proof");

    engine.disconnect().await;
    proof_task.abort();
    server_task.abort();
}
