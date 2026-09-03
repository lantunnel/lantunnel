//! Direct vs V2 Client proxy round-trip P50/P95/P99 across
//! three traffic shapes:
//!
//!   * TCP-128B  — HTTP POST with a 128-byte body to the echo HTTP server.
//!   * UDP-400B  — 400-byte UDP datagram via the echo UDP server.
//!   * UDP-1400B — 1400-byte UDP datagram via the echo UDP server.
//!
//! For each shape, we record `samples` (default 5000) sequential round
//! trips on both the direct path and the SOCKS5-proxy path. The asserted
//! plan-spec threshold is `proxy.p95 - direct.p95 < 5_000 µs` on
//! loopback. Final stats are written as JSON to `--out`.
//!
//! Why sequential and not pipelined: percentile latency is what we want,
//! not throughput. Concurrent senders would coalesce I/O on tokio's
//! reactor and inflate p99 with scheduling jitter that has nothing to
//! do with the proxy hop.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use tp_e2e_p1_connectivity::socks5::connect_no_auth as socks5_connect;

use crate::proxy::{fill_payload, DirectUdp, Socks5UdpSession, UDP_RTT_TIMEOUT};
use crate::stats::{Report, Stats};

/// Plan-spec sample count per shape per path.
pub const DEFAULT_SAMPLES: u32 = 5000;

/// Plan-spec assertion threshold: `proxy.p95 - direct.p95 < 5 ms`
/// (= 5000 µs) on loopback.
pub const P95_DELTA_BUDGET_US: u64 = 5_000;

/// Tiny inter-sample sleep so back-to-back round trips don't coalesce
/// on the reactor's epoll wakeup. 1 ms is well below the metric range
/// we care about and keeps p50 honest.
const INTER_SAMPLE_SLEEP: Duration = Duration::from_micros(1_000);

#[derive(Clone, Copy, Debug)]
enum Shape {
    Tcp128,
    Udp400,
    Udp1400,
}

impl Shape {
    fn label(self) -> &'static str {
        match self {
            Shape::Tcp128 => "tcp_128b",
            Shape::Udp400 => "udp_400b",
            Shape::Udp1400 => "udp_1400b",
        }
    }

    fn payload_len(self) -> usize {
        match self {
            Shape::Tcp128 => 128,
            Shape::Udp400 => 400,
            Shape::Udp1400 => 1400,
        }
    }
}

pub struct Args<'a> {
    pub proxy: &'a str,
    pub http_target: &'a str,
    pub udp_target: &'a str,
    pub samples: u32,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
struct PerShape {
    shape: &'static str,
    direct: Report,
    proxy: Report,
    p95_delta_us: i64,
    p95_delta_within_budget: bool,
}

#[derive(Debug, Serialize)]
struct BaselineReport {
    test: &'static str,
    samples_per_shape: u32,
    p95_delta_budget_us: u64,
    shapes: Vec<PerShape>,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.samples == 0 {
        bail!("--samples must be > 0");
    }
    tracing::info!(
        samples = args.samples,
        proxy = args.proxy,
        http_target = args.http_target,
        udp_target = args.udp_target,
        "latency_baseline begin"
    );

    let mut shapes_out = Vec::with_capacity(3);
    for shape in [Shape::Tcp128, Shape::Udp400, Shape::Udp1400] {
        let direct = collect_shape(&args, shape, false).await?;
        let proxy = collect_shape(&args, shape, true).await?;
        let p95_delta = proxy.p95_us as i64 - direct.p95_us as i64;
        let within = p95_delta < P95_DELTA_BUDGET_US as i64;
        tracing::info!(
            shape = shape.label(),
            direct_p50_us = direct.p50_us,
            direct_p95_us = direct.p95_us,
            direct_p99_us = direct.p99_us,
            proxy_p50_us = proxy.p50_us,
            proxy_p95_us = proxy.p95_us,
            proxy_p99_us = proxy.p99_us,
            p95_delta_us = p95_delta,
            within_budget = within,
            "shape result"
        );
        shapes_out.push(PerShape {
            shape: shape.label(),
            direct,
            proxy,
            p95_delta_us: p95_delta,
            p95_delta_within_budget: within,
        });
    }

    let report = BaselineReport {
        test: "latency_baseline",
        samples_per_shape: args.samples,
        p95_delta_budget_us: P95_DELTA_BUDGET_US,
        shapes: shapes_out,
    };
    write_report(args.out, &report)?;

    let breaches: Vec<&PerShape> = report
        .shapes
        .iter()
        .filter(|s| !s.p95_delta_within_budget)
        .collect();
    if !breaches.is_empty() {
        let names: Vec<&str> = breaches.iter().map(|s| s.shape).collect();
        bail!(
            "p95(proxy) - p95(direct) >= {} µs on shape(s): {} — see {}",
            P95_DELTA_BUDGET_US,
            names.join(", "),
            args.out,
        );
    }

    tracing::info!(out = args.out, "PASS: latency_baseline");
    Ok(())
}

/// Collect `samples` round-trip measurements for one shape × path.
async fn collect_shape(args: &Args<'_>, shape: Shape, via_proxy: bool) -> Result<Report> {
    // 1 second high bound is comfortably above any loopback p99.
    let mut stats = Stats::new(1_000_000)?;
    match shape {
        Shape::Tcp128 => {
            run_tcp_loop(args, via_proxy, args.samples, &mut stats).await?;
        }
        Shape::Udp400 | Shape::Udp1400 => {
            run_udp_loop(args, shape, via_proxy, args.samples, &mut stats).await?;
        }
    }
    Ok(stats.report())
}

/// HTTP POST/echo loop. One TCP connection per sample (mirrors the
/// production "fresh CONNECT per request" worst case for a proxy hop;
/// pipelining would hide proxy queue dynamics).
async fn run_tcp_loop(
    args: &Args<'_>,
    via_proxy: bool,
    samples: u32,
    stats: &mut Stats,
) -> Result<()> {
    let (host, port) = tp_e2e_p1_connectivity::parse_host_port(args.http_target)?;
    let payload = fill_payload(128, b'T');
    for _ in 0..samples {
        let stream = if via_proxy {
            socks5_connect(args.proxy, &host, port)
                .await
                .context("SOCKS5 CONNECT for TCP-128 sample")?
                .stream
        } else {
            TcpStream::connect(args.http_target)
                .await
                .with_context(|| format!("direct dial {}", args.http_target))?
        };
        let dt = http_post_round_trip(stream, &host, port, &payload).await?;
        stats.record(dt)?;
        tokio::time::sleep(INTER_SAMPLE_SLEEP).await;
    }
    Ok(())
}

/// Single-sample HTTP/1.1 POST → read full reply → return wall-clock dt.
async fn http_post_round_trip(
    mut s: TcpStream,
    host: &str,
    port: u16,
    body: &[u8],
) -> Result<Duration> {
    let req_head = format!(
        "POST / HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let started = Instant::now();
    s.write_all(req_head.as_bytes())
        .await
        .map_err(|e| anyhow!("write HTTP head: {e}"))?;
    s.write_all(body)
        .await
        .map_err(|e| anyhow!("write HTTP body: {e}"))?;
    let mut buf = Vec::with_capacity(512);
    s.read_to_end(&mut buf)
        .await
        .map_err(|e| anyhow!("read HTTP echo reply: {e}"))?;
    let dt = started.elapsed();
    if buf.is_empty() {
        bail!("empty echo reply on TCP-128 sample");
    }
    Ok(dt)
}

/// UDP echo loop. Reuses a single SOCKS5 UDP-ASSOCIATE session (proxy
/// path) or a single bound UDP socket (direct) across all samples —
/// the cost we're measuring is per-datagram round-trip, not the
/// session-establishment overhead.
async fn run_udp_loop(
    args: &Args<'_>,
    shape: Shape,
    via_proxy: bool,
    samples: u32,
    stats: &mut Stats,
) -> Result<()> {
    let payload = fill_payload(shape.payload_len(), b'U');
    if via_proxy {
        let sess = Socks5UdpSession::open(args.proxy, args.udp_target).await?;
        for _ in 0..samples {
            let started = Instant::now();
            sess.send(&payload).await?;
            let echoed = sess.recv(UDP_RTT_TIMEOUT).await?;
            let dt = started.elapsed();
            if echoed != payload {
                bail!(
                    "UDP echo mismatch via proxy ({} bytes sent, {} got)",
                    payload.len(),
                    echoed.len()
                );
            }
            stats.record(dt)?;
            tokio::time::sleep(INTER_SAMPLE_SLEEP).await;
        }
    } else {
        let sock = DirectUdp::connect(args.udp_target).await?;
        for _ in 0..samples {
            let started = Instant::now();
            sock.send(&payload).await?;
            let echoed = sock.recv(UDP_RTT_TIMEOUT).await?;
            let dt = started.elapsed();
            if echoed != payload {
                bail!(
                    "direct UDP echo mismatch ({} bytes sent, {} got)",
                    payload.len(),
                    echoed.len()
                );
            }
            stats.record(dt)?;
            tokio::time::sleep(INTER_SAMPLE_SLEEP).await;
        }
    }
    Ok(())
}

fn write_report(path: &str, report: &BaselineReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize baseline report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn shape_payload_lengths_match_plan() {
        assert_eq!(Shape::Tcp128.payload_len(), 128);
        assert_eq!(Shape::Udp400.payload_len(), 400);
        assert_eq!(Shape::Udp1400.payload_len(), 1400);
    }

    #[test]
    fn budget_constant_matches_plan() {
        // Plan threshold is "5 ms" on loopback; double-check we encode
        // it as microseconds, not milliseconds.
        assert_eq!(P95_DELTA_BUDGET_US, 5_000);
    }
}
