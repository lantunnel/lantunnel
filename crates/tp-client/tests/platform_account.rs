//! What the Platform account client actually puts on the wire, and what it
//! makes of what comes back.
//!
//! The unit tests beside the module cover its pure parts — URL validation,
//! expiry arithmetic, redaction. Everything below is the part that only a
//! socket can answer: the method and path, the exact body shape the Platform's
//! route parses, the `Authorization` header, and how each documented failure
//! reads by the time it reaches the owner.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tp_client::platform_account::{DeviceLoginPoll, PlatformAccountApi};

/// One request, one canned answer, and the request text handed back.
async fn serve_once(
    status: &str,
    content_type: &str,
    body: &str,
    extra_headers: &str,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let status = status.to_string();
    let content_type = content_type.to_string();
    let body = body.to_string();
    let extra_headers = extra_headers.to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_http_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .as_bytes(),
            )
            .await
            .expect("write response");
        request
    });
    (format!("http://{address}"), server)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
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
            .unwrap_or(0);
        while request.len() < headers_end + content_length {
            let count = socket.read(&mut chunk).await.expect("read request body");
            assert!(count > 0, "request closed before body");
            request.extend_from_slice(&chunk[..count]);
        }
        return String::from_utf8_lossy(&request).into_owned();
    }
}

fn body_of(request: &str) -> &str {
    request.split_once("\r\n\r\n").expect("request body").1
}

#[tokio::test]
async fn starting_a_sign_in_posts_the_rfc_8628_body_and_reads_the_codes_back() {
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"device_code":"dev-secret","user_code":"ABCD-EFGH",
            "verification_uri":"https://lantunnel.app/device",
            "verification_uri_complete":"https://lantunnel.app/device?user_code=ABCD-EFGH",
            "expires_in":600,"interval":5,"future_field":1}"#,
        "",
    )
    .await;

    let started = PlatformAccountApi::new(&base)
        .expect("api")
        .start_device_login()
        .await
        .expect("device code");

    let request = server.await.expect("server");
    assert!(
        request.starts_with("POST /api/auth/device/code HTTP/1.1\r\n"),
        "{request}"
    );
    // better-auth's route parses `client_id`; anything else is a 400.
    assert_eq!(body_of(&request), r#"{"client_id":"lantunnel-client"}"#);

    assert_eq!(started.user_code, "ABCD-EFGH");
    assert_eq!(started.device_code.as_str(), "dev-secret");
    assert_eq!(started.expires_in, 600);
    assert_eq!(started.interval, 5);
    assert_eq!(
        started.verification_uri_complete.as_deref(),
        Some("https://lantunnel.app/device?user_code=ABCD-EFGH")
    );
}

#[tokio::test]
async fn a_platform_without_the_complete_uri_still_starts_a_sign_in() {
    // The field is optional in RFC 8628; a Client that required it would fail
    // against a Platform that simply does not build one.
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"device_code":"d","user_code":"WXYZ-1234",
            "verification_uri":"https://lantunnel.app/device","expires_in":600,"interval":5}"#,
        "",
    )
    .await;
    let started = PlatformAccountApi::new(&base)
        .expect("api")
        .start_device_login()
        .await
        .expect("device code");
    server.await.expect("server");
    assert_eq!(started.verification_uri_complete, None);
}

#[tokio::test]
async fn polling_sends_the_device_grant_type_and_reads_a_granted_token() {
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"access_token":"session-token","token_type":"Bearer","expires_in":604800}"#,
        "",
    )
    .await;

    let outcome = PlatformAccountApi::new(&base)
        .expect("api")
        .poll_device_login("dev-secret")
        .await
        .expect("poll");

    let request = server.await.expect("server");
    assert!(
        request.starts_with("POST /api/auth/device/token HTTP/1.1\r\n"),
        "{request}"
    );
    let body = body_of(&request);
    assert!(
        body.contains(r#""grant_type":"urn:ietf:params:oauth:grant-type:device_code""#),
        "{body}"
    );
    assert!(body.contains(r#""device_code":"dev-secret""#), "{body}");
    assert!(body.contains(r#""client_id":"lantunnel-client""#), "{body}");

    let DeviceLoginPoll::Granted(token) = outcome else {
        panic!("expected a granted token, got {outcome:?}");
    };
    assert_eq!(token.access_token.as_str(), "session-token");
    // The Platform reports a lifetime; the Client stores a deadline.
    assert!(token.expires_at_unix > 0);
    assert!(!token.is_expired_at(0));
}

#[tokio::test]
async fn every_documented_poll_failure_maps_to_its_own_outcome() {
    for (error, expected) in [
        ("authorization_pending", "Pending"),
        ("slow_down", "SlowDown"),
        ("access_denied", "Denied"),
        ("expired_token", "Expired"),
    ] {
        let (base, server) = serve_once(
            "400 Bad Request",
            "application/json",
            &format!(r#"{{"error":"{error}","error_description":"..."}}"#),
            "",
        )
        .await;
        let outcome = PlatformAccountApi::new(&base)
            .expect("api")
            .poll_device_login("d")
            .await
            .unwrap_or_else(|e| panic!("{error} must not be an error: {e}"));
        server.await.expect("server");
        assert_eq!(format!("{outcome:?}"), expected, "for {error}");
    }
}

#[tokio::test]
async fn an_unknown_poll_failure_is_an_error_rather_than_a_silent_pending() {
    // Treating an unrecognised code as "keep waiting" would poll forever.
    let (base, server) = serve_once(
        "400 Bad Request",
        "application/json",
        r#"{"error":"invalid_grant","error_description":"..."}"#,
        "",
    )
    .await;
    let outcome = PlatformAccountApi::new(&base)
        .expect("api")
        .poll_device_login("d")
        .await;
    server.await.expect("server");
    assert!(outcome.is_err(), "{outcome:?}");
}

#[tokio::test]
async fn listing_tunnels_sends_the_bearer_header_and_reads_names_and_plans() {
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"tunnels":[
             {"tunnel_id":"a","name":"Home","status":"enabled",
              "billing":{"grant_kind":"account_free","access":"active","effective_plan":"free"},
              "placement":{"type":"platform_fleet"},"relay_usage":{"percent":3}},
             {"tunnel_id":"b","name":"Lapsed","status":"enabled",
              "billing":{"access":"inactive","effective_plan":"pro"}}
           ],"future":true}"#,
        "",
    )
    .await;

    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let tunnels = api.list_tunnels(&token).await.expect("tunnels");

    let request = server.await.expect("server");
    assert!(
        request.starts_with("GET /api/tunnels HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer session-token"),
        "{request}"
    );

    assert_eq!(tunnels.len(), 2);
    assert_eq!(tunnels[0].name.as_deref(), Some("Home"));
    assert_eq!(tunnels[0].plan_label(), Some("free"));
    assert!(tunnels[0].accepts_new_peers());
    // A lapsed subscription is visible before the owner types a Peer name.
    assert!(!tunnels[1].accepts_new_peers());
}

#[tokio::test]
async fn creating_a_peer_posts_the_name_and_returns_the_profile_and_its_id() {
    let profile = "{\n  \"version\": 2,\n  \"tunnel_id\": \"a\"\n}\n";
    let (base, server) = serve_once(
        "201 Created",
        "application/yaml",
        profile,
        "x-peer-id: peer-42\r\n",
    )
    .await;

    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let created = api
        .create_peer(&token, "018f6e84-e11b-7f3a-8cad-9f68f4482001", "  laptop  ")
        .await
        .expect("peer");

    let request = server.await.expect("server");
    assert!(
        request.starts_with(
            "POST /api/tunnels/018f6e84-e11b-7f3a-8cad-9f68f4482001/peers HTTP/1.1\r\n"
        ),
        "{request}"
    );
    // The route rejects any key it does not know, and trims nothing itself.
    assert_eq!(body_of(&request), r#"{"name":"laptop"}"#);

    // The body is the profile; the identifier needed to revoke it is a header.
    assert_eq!(created.peer_id.as_deref(), Some("peer-42"));
    assert_eq!(String::from_utf8_lossy(&created.profile), profile);
}

#[tokio::test]
async fn a_lapsed_subscription_and_an_expired_sign_in_read_differently() {
    let api_base_and_status = [
        ("402 Payment Required", "subscription"),
        ("401 Unauthorized", "sign in again"),
        ("404 Not Found", "no longer on this account"),
    ];
    for (status, expected) in api_base_and_status {
        let (base, server) =
            serve_once(status, "application/json", r#"{"error":"nope"}"#, "").await;
        let api = PlatformAccountApi::new(&base).expect("api");
        let token = granted_token(&base).await;
        let error = api
            .create_peer(&token, "a", "laptop")
            .await
            .expect_err("must fail");
        server.await.expect("server");
        assert!(
            error.to_string().contains(expected),
            "{status} read as: {error}"
        );
    }
}

#[tokio::test]
async fn an_expired_session_on_a_read_says_so_rather_than_returning_nothing() {
    let (base, server) = serve_once("401 Unauthorized", "application/json", "{}", "").await;
    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let error = api.list_tunnels(&token).await.expect_err("must fail");
    server.await.expect("server");
    assert!(error.to_string().contains("sign in again"), "{error}");
}

#[tokio::test]
async fn the_identity_call_reads_the_account_the_token_belongs_to() {
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"session":{"id":"s"},"user":{"id":"u","email":"owner@example.com","emailVerified":true}}"#,
        "",
    )
    .await;
    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let identity = api.identity(&token).await.expect("identity");
    let request = server.await.expect("server");
    assert!(
        request.starts_with("GET /api/auth/get-session HTTP/1.1\r\n"),
        "{request}"
    );
    assert_eq!(identity.email, "owner@example.com");
}

/// A token built the only way the Client can build one: from a granted poll.
async fn granted_token(_base: &str) -> tp_client::platform_account::PlatformToken {
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"access_token":"session-token","token_type":"Bearer","expires_in":604800}"#,
        "",
    )
    .await;
    let outcome = PlatformAccountApi::new(&base)
        .expect("api")
        .poll_device_login("d")
        .await
        .expect("poll");
    server.await.expect("server");
    match outcome {
        DeviceLoginPoll::Granted(token) => token,
        other => panic!("expected a token, got {other:?}"),
    }
}

#[tokio::test]
async fn listing_the_peers_a_tunnel_already_issued_reads_their_names_and_addresses() {
    let (base, server) = serve_once(
        "200 OK",
        "application/json",
        r#"{"peers":[
             {"tunnel_id":"t","peer_id":"p1","overlay_ip":"198.18.0.1","name":"macbook",
              "peer_public_key":"k","created_at":"2026-09-11T01:15:00.000Z"},
             {"tunnel_id":"t","peer_id":"p2","overlay_ip":"198.18.0.2","name":null}
           ],"future":true}"#,
        "",
    )
    .await;

    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let peers = api
        .list_peers(&token, "018f6e84-e11b")
        .await
        .expect("peers");

    let request = server.await.expect("server");
    assert!(
        request.starts_with("GET /api/tunnels/018f6e84-e11b/peers HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer session-token"),
        "{request}"
    );

    assert_eq!(peers.len(), 2);
    assert_eq!(peers[0].name.as_deref(), Some("macbook"));
    assert_eq!(peers[0].overlay_ip.as_deref(), Some("198.18.0.1"));
    // An unnamed Peer is still selectable; the picker falls back to its ID.
    assert_eq!(peers[1].name, None);
}

#[tokio::test]
async fn adopting_an_existing_peer_posts_to_its_own_path_and_returns_its_profile() {
    let profile = "{\n  \"version\": 2\n}\n";
    let (base, server) = serve_once(
        "200 OK",
        "application/yaml",
        profile,
        "x-peer-id: existing-peer\r\n",
    )
    .await;

    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let fetched = api
        .download_peer(&token, "tunnel-a", "existing-peer")
        .await
        .expect("profile");

    let request = server.await.expect("server");
    // A POST, not a GET: re-issuing is the Platform's own verb for this, and
    // the body carries a private key that must never sit in a URL or a log.
    assert!(
        request.starts_with("POST /api/tunnels/tunnel-a/peers/existing-peer HTTP/1.1\r\n"),
        "{request}"
    );
    assert_eq!(fetched.peer_id.as_deref(), Some("existing-peer"));
    assert_eq!(String::from_utf8_lossy(&fetched.profile), profile);
}

#[tokio::test]
async fn a_peer_issued_before_profiles_were_kept_says_so_rather_than_failing_blankly() {
    let (base, server) = serve_once(
        "409 Conflict",
        "application/json",
        r#"{"error":"Peer profile is not available"}"#,
        "",
    )
    .await;
    let api = PlatformAccountApi::new(&base).expect("api");
    let token = granted_token(&base).await;
    let error = api
        .download_peer(&token, "t", "old-peer")
        .await
        .expect_err("must fail");
    server.await.expect("server");
    assert!(error.to_string().contains("Add a new one"), "{error}");
}
