//! Smoke tests for the echo services.
//!
//! Each test starts the service-under-test on a free ephemeral port, drives
//! it via the public API (`reqwest` for HTTP, `tokio::net` for TCP/UDP),
//! and asserts the response shape and counter side-effects.
//!
//! We don't spawn the binary — the service modules are not a public lib
//! crate, but `cargo test` builds the integration test against the bin
//! crate's modules so we can call `tp_e2e_echo_services::http::serve` (etc.)
//! directly. This keeps the test fast and decouples it from binary path /
//! signal handling concerns.
//!
//! Free-port allocation: bind a transient `TcpListener` (or `UdpSocket`) on
//! 127.0.0.1:0, read `local_addr()`, drop the listener, then hand the addr
//! to `serve(...)`. Tiny TOCTOU window but on loopback it's effectively
//! zero — and tests run sequentially per `cargo test --test smoke` anyway.

#![allow(clippy::unwrap_used, clippy::expect_used)] // tests are allowed to panic

use std::net::SocketAddr;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

// The bin crate name has hyphens; the rust path uses underscores. The
// integration test is compiled against the bin crate, so its modules are
// reachable via the crate-name path.
use tp_e2e_echo_services::counters::Counters;
use tp_e2e_echo_services::{http as http_svc, tcp as tcp_svc, udp as udp_svc};

/// Build a reqwest client that ignores environment proxies — CI runners
/// (and dev machines like this one) sometimes have HTTP_PROXY pointing at
/// Privoxy, which would intercept our loopback request.
fn loopback_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build reqwest client")
}

/// Reserve a free TCP port by binding ephemeral, reading addr, dropping.
async fn free_tcp_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    a
}

/// Reserve a free UDP port the same way.
async fn free_udp_addr() -> SocketAddr {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let a = s.local_addr().unwrap();
    drop(s);
    a
}

/// Wait until a TCP connect to `addr` succeeds (server has started serving).
/// Times out after ~1 second of failed connects.
async fn wait_tcp_ready(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("tcp service at {addr} never became reachable");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn http_echo_returns_seq_size_header_then_body() {
    let counters = Counters::new();
    let addr = free_tcp_addr().await;

    let serve_handle = tokio::spawn({
        let counters = counters.clone();
        async move { http_svc::serve(addr, counters).await }
    });
    wait_tcp_ready(addr).await;

    let body = vec![0xa5u8; 1024];
    let client = loopback_http_client();
    let resp = client
        .post(format!("http://{addr}/"))
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.bytes().await.unwrap();
    let expected_header = b"#1 size=1024\n";
    assert!(
        bytes.starts_with(expected_header),
        "response should start with `{}`, got {:?}",
        std::str::from_utf8(expected_header).unwrap(),
        &bytes[..bytes.len().min(32)],
    );
    assert_eq!(&bytes[expected_header.len()..], body.as_slice());

    // Verify counters via /stats.
    let stats = client
        .get(format!("http://{addr}/stats"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // /stats is a separate handler and doesn't bump http_requests, so we
    // expect exactly 1 echo request reflected in the counter.
    assert!(stats.contains("http_requests=1"), "stats: {stats}");
    assert!(stats.contains("http_bytes_in=1024"), "stats: {stats}");

    serve_handle.abort();
}

#[tokio::test]
async fn tcp_large_download_returns_correct_sha256_and_length() {
    let counters = Counters::new();
    let addr = free_tcp_addr().await;
    let bytes_to_download: u64 = 4096;

    let serve_handle = tokio::spawn({
        let counters = counters.clone();
        async move { tcp_svc::serve(addr, counters).await }
    });
    wait_tcp_ready(addr).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET /{bytes_to_download}\n").as_bytes())
        .await
        .unwrap();

    // Read everything until EOF (server closes after one response).
    let mut buf = Vec::with_capacity(8192);
    stream.read_to_end(&mut buf).await.unwrap();

    // Split header from body at the CRLF CRLF boundary.
    let sep_pos = find_subslice(&buf, b"\r\n\r\n").expect("response had no header terminator");
    let header_text = std::str::from_utf8(&buf[..sep_pos]).unwrap();
    let body = &buf[sep_pos + 4..];

    assert!(
        header_text.starts_with("HTTP/1.1 200 OK"),
        "header: {header_text}"
    );
    assert!(
        header_text.contains(&format!("Content-Length: {bytes_to_download}")),
        "header: {header_text}",
    );
    let sha_line = header_text
        .lines()
        .find(|l| l.starts_with("X-SHA256: "))
        .expect("missing X-SHA256 header");
    let advertised_hex = sha_line.trim_start_matches("X-SHA256: ").trim();

    assert_eq!(body.len(), bytes_to_download as usize);
    let mut h = Sha256::new();
    h.update(body);
    let actual_hex = hex_lower(&h.finalize());
    assert_eq!(actual_hex, advertised_hex);

    serve_handle.abort();
}

#[tokio::test]
async fn udp_echo_tracks_valid_and_invalid_checksums() {
    let counters = Counters::new();
    let udp_addr = free_udp_addr().await;
    let http_addr = free_tcp_addr().await;

    let udp_handle = tokio::spawn({
        let counters = counters.clone();
        async move { udp_svc::serve(udp_addr, counters).await }
    });
    let http_handle = tokio::spawn({
        let counters = counters.clone();
        async move { http_svc::serve(http_addr, counters).await }
    });
    wait_tcp_ready(http_addr).await;
    // Loopback UDP bind is essentially instant; one short yield gives the
    // recv_from loop a tick to enter the wait state.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Build a 64-byte datagram: 60 payload bytes + 4-byte BE checksum.
    let payload: Vec<u8> = (0..60u8).collect();
    let checksum = xor_fold(&payload);
    let mut good = payload.clone();
    good.extend_from_slice(&checksum.to_be_bytes());
    assert_eq!(good.len(), 64);

    client.send_to(&good, udp_addr).await.unwrap();
    let mut rbuf = [0u8; 64];
    let (n, _) = client.recv_from(&mut rbuf).await.unwrap();
    assert_eq!(n, 64);
    assert_eq!(&rbuf[..n], good.as_slice());

    // Now a packet with a deliberately wrong checksum.
    let mut bad = payload.clone();
    bad.extend_from_slice(&(checksum ^ 0xdead_beef).to_be_bytes());
    client.send_to(&bad, udp_addr).await.unwrap();
    let (n, _) = client.recv_from(&mut rbuf).await.unwrap();
    assert_eq!(n, 64);

    // Give the server a beat to bump the counters before we read /stats.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let stats = loopback_http_client()
        .get(format!("http://{http_addr}/stats"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(stats.contains("udp_packets_received=2"), "stats: {stats}");
    assert!(stats.contains("udp_valid_packets=1"), "stats: {stats}");
    assert!(stats.contains("udp_checksum_errors=1"), "stats: {stats}");

    udp_handle.abort();
    http_handle.abort();
}

// ---- helpers --------------------------------------------------------------

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Mirror of `tp_e2e_echo_services::udp::xor_fold` so the test stays
/// self-contained (the upstream fn is `pub(crate)`, not `pub`).
fn xor_fold(payload: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for &b in payload {
        acc = acc.rotate_left(8) ^ (b as u32);
    }
    acc
}
