//! V2 multi-stream aggregate-throughput stress
//! test. Per Phase 2 numbers, UDP via SOCKS5 saturates around
//! 100–200 Mbps before loss explodes — we cannot reach 5 Gbps via
//! UDP-ASSOC. Instead this test fans out N concurrent SOCKS5
//! TCP-large-download streams (the gateway distributes them across
//! the configured replica pool) and measures the aggregate rate.
//!
//! Default config matches the loopback fixture's existing 4-replica
//! pool: 12 streams = 3 streams × 4 replicas. Plan-spec target of
//! ≥5 Gbps is recorded but not asserted — loopback is not infinite,
//! and the headline number we publish is the observed value.
//!
//! Per-stream wire format mirrors `tcp_large_download`:
//!
//! ```text
//!   GET /<bytes>\n
//!   ←
//!   HTTP/1.1 200 OK\r\nContent-Length: <bytes>\r\nX-SHA256: <hex>\r\n\r\n<bytes>
//! ```

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::meter::Throughput;
use crate::proxy::socks5_connect;

/// Plan-spec stream count: 12 parallel TCP-large-download streams.
pub const DEFAULT_STREAMS: u32 = 12;

/// Per-stream byte target. 100 MiB × 12 = 1.2 GiB transferred — enough
/// duration for the proxy hop to settle without each individual stream
/// finishing in slow-start.
pub const DEFAULT_BYTES_PER_STREAM: u64 = 100 * 1024 * 1024;

/// Plan-spec aggregate-throughput target — recorded for reference,
/// not asserted.
pub const MBPS_TARGET: f64 = 5_000.0;

/// Per-stream read chunk size. 256 KiB keeps the syscall overhead
/// negligible at multi-Gbps targets.
const READ_CHUNK: usize = 256 * 1024;

pub struct Args<'a> {
    pub proxy: &'a str,
    pub tcp_target: &'a str,
    pub streams: u32,
    pub bytes_per_stream: u64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct PerStream {
    pub stream_idx: u32,
    pub throughput: Option<Throughput>,
    pub sha256_match: bool,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct MultiStreamReport {
    pub test: &'static str,
    pub streams: u32,
    pub bytes_per_stream: u64,
    pub mbps_target: f64,
    pub aggregate: Option<Throughput>,
    pub per_stream: Vec<PerStream>,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.streams == 0 {
        bail!("--streams must be > 0");
    }
    if args.bytes_per_stream == 0 {
        bail!("--bytes-per-stream must be > 0");
    }
    tracing::info!(
        streams = args.streams,
        bytes_per_stream = args.bytes_per_stream,
        proxy = args.proxy,
        target = args.tcp_target,
        mbps_target = MBPS_TARGET,
        "udp_stress_multi_stream begin"
    );

    let proxy = Arc::new(args.proxy.to_string());
    let target = Arc::new(args.tcp_target.to_string());

    let started = Instant::now();
    let mut handles = Vec::with_capacity(args.streams as usize);
    for idx in 0..args.streams {
        let proxy = Arc::clone(&proxy);
        let target = Arc::clone(&target);
        let bytes = args.bytes_per_stream;
        handles.push(tokio::spawn(async move {
            run_stream(idx, &proxy, &target, bytes).await
        }));
    }

    let mut per_stream = Vec::with_capacity(args.streams as usize);
    let mut total_bytes: u64 = 0;
    let mut all_sha256_ok = true;
    for h in handles {
        let outcome = h
            .await
            .map_err(|e| anyhow!("stream task panicked: {e}"))?
            .context("stream task failed")?;
        total_bytes += outcome.bytes_received;
        if !outcome.sha256_match {
            all_sha256_ok = false;
        }
        per_stream.push(PerStream {
            stream_idx: outcome.idx,
            throughput: Some(Throughput::from_window(
                outcome.bytes_received,
                outcome.elapsed,
            )),
            sha256_match: outcome.sha256_match,
        });
    }
    let elapsed = started.elapsed();

    let aggregate = Throughput::from_window(total_bytes, elapsed);
    tracing::info!(
        streams = args.streams,
        total_bytes,
        elapsed_ms = elapsed.as_millis(),
        mbps = aggregate.mbps,
        mbps_target = MBPS_TARGET,
        all_sha256_ok,
        "aggregate complete"
    );

    let report = MultiStreamReport {
        test: "udp_stress_multi_stream",
        streams: args.streams,
        bytes_per_stream: args.bytes_per_stream,
        mbps_target: MBPS_TARGET,
        aggregate: Some(aggregate),
        per_stream,
    };
    write_report(args.out, &report)?;

    if !all_sha256_ok {
        bail!(
            "at least one stream's SHA-256 mismatched — see {}",
            args.out
        );
    }

    tracing::info!(out = args.out, "PASS: udp_stress_multi_stream");
    Ok(())
}

struct StreamOutcome {
    idx: u32,
    bytes_received: u64,
    elapsed: std::time::Duration,
    sha256_match: bool,
}

async fn run_stream(idx: u32, proxy: &str, target: &str, bytes: u64) -> Result<StreamOutcome> {
    let (host, port) = tp_e2e_p1_connectivity::parse_host_port(target)?;
    let mut stream = socks5_connect(proxy, &host, port)
        .await
        .with_context(|| format!("SOCKS5 CONNECT for stream {idx}"))?
        .stream;
    let request = format!("GET /{bytes}\n");
    stream
        .write_all(request.as_bytes())
        .await
        .with_context(|| format!("write request stream {idx}"))?;

    let header = read_header(&mut stream).await?;
    if header.content_length != bytes {
        bail!(
            "stream {idx}: Content-Length {} != requested {}",
            header.content_length,
            bytes
        );
    }
    let started = Instant::now();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut received: u64 = 0;
    while received < bytes {
        let n = stream
            .read(&mut buf)
            .await
            .with_context(|| format!("stream {idx} read body"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        received += n as u64;
    }
    let elapsed = started.elapsed();
    if received != bytes {
        bail!("stream {idx}: short read — {received}/{bytes}");
    }
    let computed = hasher.finalize();
    let computed_hex = hex_lower(&computed);
    let sha256_match = computed_hex.eq_ignore_ascii_case(&header.x_sha256);

    Ok(StreamOutcome {
        idx,
        bytes_received: received,
        elapsed,
        sha256_match,
    })
}

struct Header {
    content_length: u64,
    x_sha256: String,
}

async fn read_header(stream: &mut tokio::net::TcpStream) -> Result<Header> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.context("read header byte")?;
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
    let mut lines = head.split("\r\n");
    let status = lines.next().ok_or_else(|| anyhow!("empty header"))?;
    if !status.starts_with("HTTP/1.1 200") {
        bail!("non-200 status line: {status:?}");
    }
    let mut content_length: Option<u64> = None;
    let mut x_sha256: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| anyhow!("bad line"))?;
        let v = v.trim();
        if k.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(v.parse().context("Content-Length")?);
        } else if k.eq_ignore_ascii_case("X-SHA256") {
            x_sha256 = Some(v.to_string());
        }
    }
    Ok(Header {
        content_length: content_length.ok_or_else(|| anyhow!("missing Content-Length"))?,
        x_sha256: x_sha256.ok_or_else(|| anyhow!("missing X-SHA256"))?,
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn write_report(path: &str, report: &MultiStreamReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize multi-stream report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plan_spec_constants_match() {
        assert_eq!(DEFAULT_STREAMS, 12);
        assert_eq!(DEFAULT_BYTES_PER_STREAM, 100 * 1024 * 1024);
        assert!((MBPS_TARGET - 5_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn hex_lower_is_lowercase_hex() {
        assert_eq!(hex_lower(&[0x00, 0xff]), "00ff");
        assert_eq!(hex_lower(&[0xab, 0xcd]), "abcd");
    }
}
