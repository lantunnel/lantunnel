use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tp_client::peer_heartbeat::{
    build_peer_heartbeat_request, PeerHeartbeatClient, PeerHeartbeatSendError,
};
use tp_core::provisioning::{
    GatewayBootstrapV2, PlatformHeartbeatPathModeV2, PlatformHeartbeatProofV2, TunnelOwnerFileV2,
};

fn managed_peer() -> tp_core::provisioning::PeerProfileV2 {
    let mut owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example".into(),
        port: 8443,
        mapping_port: None,
        tls_server_name: Some("gateway.example".into()),
        trusted_certificate_pem: None,
    })
    .expect("Tunnel");
    let mut peer = owner.add_peer(None, 1, None).expect("Peer");
    peer.bootstrap = tp_core::provisioning::PeerBootstrapV2::ManagedPlatform {
        platform_url: "https://platform.example".into(),
    };
    peer
}

#[test]
fn managed_peer_builds_the_exact_signed_platform_heartbeat_contract() {
    let peer = managed_peer();
    let request_id = "018f6e84-e11b-7f3a-8cad-9f68f4480300";
    let request = build_peer_heartbeat_request(
        &peer,
        request_id,
        1_786_426_560_123,
        "2.0.0-test",
        false,
        true,
        PlatformHeartbeatPathModeV2::Relay,
    )
    .expect("heartbeat request");
    let json = serde_json::to_value(&request).expect("heartbeat JSON");

    assert_eq!(
        json.as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "client_version",
            "final",
            "path_mode",
            "peer_id",
            "proof",
            "request_id",
            "timestamp_ms",
            "transport_active",
            "tunnel_id",
        ]
    );
    assert_eq!(json["tunnel_id"], peer.tunnel_id);
    assert_eq!(json["peer_id"], peer.peer.peer_id);
    assert_eq!(json["path_mode"], "relay");
    assert!(!json
        .to_string()
        .contains(peer.peer.peer_private_key.as_str()));

    peer.public_membership()
        .verify_platform_heartbeat_proof(
            &PlatformHeartbeatProofV2 {
                tunnel_id: &peer.tunnel_id,
                peer_id: &peer.peer.peer_id,
                request_id,
                timestamp_ms: 1_786_426_560_123,
                client_version: "2.0.0-test",
                final_heartbeat: false,
                transport_active: true,
                path_mode: PlatformHeartbeatPathModeV2::Relay,
            },
            request.proof(),
        )
        .expect("Platform verifies request proof");
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let count = socket.read(&mut chunk).await.expect("read request");
        assert!(count > 0, "request closed before headers");
        request.extend_from_slice(&chunk[..count]);
        let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers_end = headers_end + 4;
        let headers = String::from_utf8_lossy(&request[..headers_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .expect("content length");
        while request.len() < headers_end + content_length {
            let count = socket.read(&mut chunk).await.expect("read request body");
            assert!(count > 0, "request closed before body");
            request.extend_from_slice(&chunk[..count]);
        }
        return request;
    }
}

#[tokio::test]
async fn managed_peer_posts_only_to_the_canonical_heartbeat_endpoint() {
    let peer = managed_peer();
    let request = build_peer_heartbeat_request(
        &peer,
        "018f6e84-e11b-7f3a-8cad-9f68f4480300",
        1_786_426_560_123,
        "2.0.0-test",
        false,
        true,
        PlatformHeartbeatPathModeV2::Relay,
    )
    .expect("heartbeat request");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_http_request(&mut socket).await;
        let request_text = String::from_utf8_lossy(&request);
        assert!(request_text.starts_with("POST /api/peers/heartbeat HTTP/1.1\r\n"));
        let body =
            r#"{"accepted_timestamp_ms":1786426560123,"server_time":"2026-08-18T08:00:00.000Z"}"#;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                )
                .as_bytes(),
            )
            .await
            .expect("write response");
    });

    let response = PeerHeartbeatClient::new()
        .post(&format!("http://{address}"), &request)
        .await
        .expect("heartbeat accepted");
    assert_eq!(response.accepted_timestamp_ms, 1_786_426_560_123);
    server.await.expect("server");
}

async fn heartbeat_server(status: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let status = status.to_string();
    let body = body.to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .as_bytes(),
            )
            .await
            .expect("write response");
    });
    (format!("http://{address}"), server)
}

#[tokio::test]
async fn forbidden_or_conflicting_peer_heartbeat_is_a_retryable_observability_failure() {
    let peer = managed_peer();
    let request = build_peer_heartbeat_request(
        &peer,
        "018f6e84-e11b-7f3a-8cad-9f68f4480300",
        1_786_426_560_123,
        "2.0.0-test",
        false,
        true,
        PlatformHeartbeatPathModeV2::Relay,
    )
    .expect("heartbeat request");
    let client = PeerHeartbeatClient::new();

    for status in ["403 Forbidden", "409 Conflict"] {
        let (platform_url, server) = heartbeat_server(status, r#"{"error":"rejected"}"#).await;
        let error = client
            .post(&platform_url, &request)
            .await
            .expect_err("heartbeat must fail");
        assert!(
            matches!(error, PeerHeartbeatSendError::Retryable(_)),
            "{status} must not own the Mesh connection lifecycle",
        );
        server.await.expect("server");
    }
}

#[tokio::test]
async fn server_errors_are_retryable_without_exposing_the_response_body() {
    let peer = managed_peer();
    let request = build_peer_heartbeat_request(
        &peer,
        "018f6e84-e11b-7f3a-8cad-9f68f4480300",
        1_786_426_560_123,
        "2.0.0-test",
        false,
        true,
        PlatformHeartbeatPathModeV2::Relay,
    )
    .expect("heartbeat request");
    let (platform_url, server) =
        heartbeat_server("503 Service Unavailable", r#"{"secret":"do-not-log"}"#).await;

    let error = PeerHeartbeatClient::new()
        .post(&platform_url, &request)
        .await
        .expect_err("heartbeat must fail");

    assert!(matches!(error, PeerHeartbeatSendError::Retryable(_)));
    assert!(!error.to_string().contains("do-not-log"));
    server.await.expect("server");
}

#[tokio::test]
async fn oversized_success_response_is_retryable() {
    let peer = managed_peer();
    let request = build_peer_heartbeat_request(
        &peer,
        "018f6e84-e11b-7f3a-8cad-9f68f4480300",
        1_786_426_560_123,
        "2.0.0-test",
        false,
        true,
        PlatformHeartbeatPathModeV2::Relay,
    )
    .expect("heartbeat request");
    let oversized_body = "x".repeat(4 * 1024 + 1);
    let (platform_url, server) = heartbeat_server("200 OK", &oversized_body).await;

    let error = PeerHeartbeatClient::new()
        .post(&platform_url, &request)
        .await
        .expect_err("oversized heartbeat response must fail");

    assert!(matches!(error, PeerHeartbeatSendError::Retryable(_)));
    assert!(error.to_string().contains("too large"));
    server.await.expect("server");
}
