//! V2 1 GiB SOCKS5 download from echo-services'
//! TCP large-download endpoint, with on-the-fly SHA-256 verification and
//! sustained-Mbps measurement. Plan-spec target: ≥500 Mbps loopback.
//!
//! Wire flow per request:
//!
//! ```text
//! 1.  SOCKS5 NO AUTH + CONNECT 127.0.0.1:18998 (DOMAIN ATYP).
//! 2.  Client sends `GET /<bytes>\n`.
//! 3.  Server replies:
//!         HTTP/1.1 200 OK\r\n
//!         Content-Length: <bytes>\r\n
//!         X-SHA256: <hex>\r\n
//!         \r\n
//!         <bytes>
//! 4.  Client streams body into Sha256, accumulates total bytes,
//!     records elapsed time. Asserts computed digest == X-SHA256.
//! ```
//!
//! Why 1 GiB and not 100 MiB: TCP slow-start finishes well before EOF
//! at 1 GiB on loopback. The 100 MiB toy threshold under-reports the
//! steady-state throughput by 10–30 %.

use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::meter::Throughput;
use crate::proxy::socks5_connect;

/// Plan-spec download size: 1 GiB. Picked so TCP slow-start finishes
/// well before EOF — anything smaller (the plan called out 100 MiB as
/// "the toy threshold") under-reports the steady-state throughput.
pub const DEFAULT_BYTES: u64 = 1024 * 1024 * 1024;

/// Plan-spec assertion threshold: ≥ 500 Mbps on loopback.
pub const MBPS_TARGET: f64 = 500.0;

/// Read-buffer size. 256 KiB is large enough that the per-syscall
/// overhead disappears at multi-Gbps targets but small enough that
/// the heap allocation is irrelevant at the ~100 Mbps lower bound.
const READ_CHUNK: usize = 256 * 1024;

pub struct Args<'a> {
    pub proxy: &'a str,
    pub tcp_target: &'a str,
    pub bytes: u64,
    pub min_mbps: f64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct DownloadReport {
    pub test: &'static str,
    pub bytes_requested: u64,
    pub mbps_target: f64,
    pub throughput: Option<Throughput>,
    pub sha256_match: bool,
    pub sha256_expected: Option<String>,
    pub sha256_computed: Option<String>,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.bytes == 0 {
        bail!("--bytes must be > 0");
    }
    tracing::info!(
        bytes = args.bytes,
        proxy = args.proxy,
        target = args.tcp_target,
        "tcp_large_download begin"
    );

    let outcome = perform_download(&args).await?;

    let throughput = Throughput::from_window(outcome.bytes_received, outcome.elapsed);
    tracing::info!(
        bytes_requested = args.bytes,
        bytes_received = outcome.bytes_received,
        elapsed_ms = outcome.elapsed.as_millis(),
        mbps = throughput.mbps,
        mib_per_s = throughput.mib_per_s,
        sha256_match = outcome.sha256_match,
        "download complete"
    );

    let report = DownloadReport {
        test: "tcp_large_download",
        bytes_requested: args.bytes,
        mbps_target: args.min_mbps,
        throughput: Some(throughput.clone()),
        sha256_match: outcome.sha256_match,
        sha256_expected: Some(outcome.expected_hex),
        sha256_computed: Some(outcome.computed_hex),
    };
    write_report(args.out, &report)?;

    if !outcome.sha256_match {
        bail!("SHA-256 mismatch — see {}", args.out);
    }
    if outcome.bytes_received != args.bytes {
        bail!(
            "bytes received ({}) != requested ({}) — see {}",
            outcome.bytes_received,
            args.bytes,
            args.out
        );
    }
    if throughput.mbps < args.min_mbps {
        bail!(
            "Mbps {:.1} below {} target — see {}",
            throughput.mbps,
            args.min_mbps,
            args.out
        );
    }

    tracing::info!(out = args.out, "PASS: tcp_large_download");
    Ok(())
}

struct Outcome {
    bytes_received: u64,
    elapsed: std::time::Duration,
    sha256_match: bool,
    expected_hex: String,
    computed_hex: String,
}

async fn perform_download(args: &Args<'_>) -> Result<Outcome> {
    let (host, port) = tp_e2e_p1_connectivity::parse_host_port(args.tcp_target)?;
    let mut stream = socks5_connect(args.proxy, &host, port)
        .await
        .context("SOCKS5 CONNECT to TCP large-download endpoint")?
        .stream;

    // Send the size request line. The server's protocol is "GET
    // /<bytes>\n" — newline-terminated, no headers.
    let request = format!("GET /{}\n", args.bytes);
    stream
        .write_all(request.as_bytes())
        .await
        .context("write size request")?;

    let header = read_response_header(&mut stream).await?;
    if header.content_length != args.bytes {
        bail!(
            "Content-Length {} != requested {}",
            header.content_length,
            args.bytes
        );
    }

    let started = Instant::now();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut received: u64 = 0;
    while received < header.content_length {
        let n = stream.read(&mut buf).await.context("read body chunk")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        received += n as u64;
    }
    let elapsed = started.elapsed();

    if received != header.content_length {
        bail!(
            "short read: got {} bytes, expected {}",
            received,
            header.content_length
        );
    }

    let computed = hasher.finalize();
    let computed_hex = hex_lower(&computed);
    let sha256_match = computed_hex.eq_ignore_ascii_case(&header.x_sha256);

    Ok(Outcome {
        bytes_received: received,
        elapsed,
        sha256_match,
        expected_hex: header.x_sha256,
        computed_hex,
    })
}

struct ResponseHeader {
    content_length: u64,
    x_sha256: String,
}

/// Read the HTTP response head until the blank-line terminator. Keep
/// this hand-rolled (rather than dragging in `httparse`) so the test
/// crate's dependency surface stays tiny — the server emits exactly
/// three lines plus the terminator and never sends chunked encoding.
async fn read_response_header(stream: &mut tokio::net::TcpStream) -> Result<ResponseHeader> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .context("read response header byte")?;
        if n == 0 {
            bail!(
                "EOF while reading response header (got {} bytes)",
                buf.len()
            );
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 4096 {
            bail!("response header exceeds 4 KiB cap");
        }
    }
    let head = std::str::from_utf8(&buf).context("response header not UTF-8")?;
    parse_response_header(head)
}

fn parse_response_header(head: &str) -> Result<ResponseHeader> {
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .ok_or_else(|| anyhow!("empty response header"))?;
    if !status.starts_with("HTTP/1.1 200") {
        bail!("non-200 status line: {status:?}");
    }
    let mut content_length: Option<u64> = None;
    let mut x_sha256: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed header line: {line:?}"))?;
        let v = v.trim();
        if k.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(
                v.parse::<u64>()
                    .with_context(|| format!("parse Content-Length: {v:?}"))?,
            );
        } else if k.eq_ignore_ascii_case("X-SHA256") {
            x_sha256 = Some(v.to_string());
        }
    }
    Ok(ResponseHeader {
        content_length: content_length.ok_or_else(|| anyhow!("response missing Content-Length"))?,
        x_sha256: x_sha256.ok_or_else(|| anyhow!("response missing X-SHA256"))?,
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn write_report(path: &str, report: &DownloadReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize download report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plan_spec_constants_match() {
        assert_eq!(DEFAULT_BYTES, 1_073_741_824);
        // 500 Mbps in Mbps units (not Mb/s, not MiB/s).
        assert!((MBPS_TARGET - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_response_header_well_formed() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nX-SHA256: deadbeef\r\n\r\n";
        let h = parse_response_header(head).unwrap();
        assert_eq!(h.content_length, 1024);
        assert_eq!(h.x_sha256, "deadbeef");
    }

    #[test]
    fn parse_response_header_missing_field() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n";
        assert!(parse_response_header(head).is_err());
    }

    #[test]
    fn parse_response_header_rejects_non_200() {
        let head = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
        assert!(parse_response_header(head).is_err());
    }

    #[test]
    fn hex_lower_pads_two_chars() {
        assert_eq!(hex_lower(&[0x00, 0xff, 0x12]), "00ff12");
    }
}
