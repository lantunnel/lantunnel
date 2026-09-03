//! V2 1000-concurrent UDP echo workload via
//! SOCKS5 UDP-ASSOCIATE for 30 s. Reports total RPS (received echoes /
//! duration) and P99 round-trip time.
//!
//! Per the Phase 2 stress-curve numbers (39 % loss at 200 Mbps, 75 % at
//! 500 Mbps), this test runs hot — 1000 clients × ~30 pps × ~600 B is
//! around 140 Mbps, near the SOCKS5 UDP-relay saturation point. We
//! therefore record loss rather than asserting on it. Data-integrity /
//! I/O errors and a sanity floor on RPS remain hard failures.
//!
//! Architecture:
//!   * `clients` worker tasks. Each opens its own SOCKS5 UDP-ASSOC
//!     session, sends one packet every `SEND_INTERVAL`, awaits the
//!     echo with a per-packet timeout, and logs RTT + send/recv
//!     counters into shared atomics + a per-task local histogram.
//!   * Histograms are merged at end-of-run. Atomics bypass any
//!     per-sample lock contention while the burst is running.
//!   * After `duration_secs`, all workers stop and we emit the
//!     aggregated JSON report.
//!
//! The session-open dance lives in `crate::proxy::Socks5UdpSession`
//! (copy-localized from the latency crate so the Phase 3 build does
//! not depend on Phase 2).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hdrhistogram::Histogram;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::proxy::Socks5UdpSession;
use tp_e2e_p1_connectivity::socks5_udp::xor_fold;

/// Plan-spec client count: 1000 concurrent senders.
pub const DEFAULT_CLIENTS: u32 = 1000;

/// Plan-spec duration: 30 s minimum to reach steady state.
pub const DEFAULT_DURATION_SECS: u64 = 30;

/// Sanity floor below which we hard-fail. Set well below the expected
/// 30 k pps so a degraded run still succeeds — anything below 5 k RPS
/// likely means the gateway crashed mid-run, not just elevated loss.
pub const RPS_SANITY_FLOOR: f64 = 5_000.0;

/// Per-client send cadence — 1 packet every 30 ms (~33 pps × 1000
/// clients = ~33 k pps aggregate, matching the plan's burst profile).
const SEND_INTERVAL: Duration = Duration::from_millis(30);

/// Per-packet recv timeout. Picked well above any plausible loopback
/// p99 so a stuck packet surfaces as `lost` rather than aborting the
/// whole client.
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// Per-packet payload size. ~600 B mid-range (smaller than streaming-
/// game's 1.4 KB, larger than the latency-baseline 400 B), so the
/// aggregate hits the 100–200 Mbps proxy-saturation zone.
const PAYLOAD_BYTES: usize = 600;

/// The echo service expects a big-endian xor-fold checksum in the final
/// four bytes of every datagram.
const CHECKSUM_BYTES: usize = 4;

/// Histogram upper bound (microseconds). 5 s comfortably covers any
/// wedged-packet outlier without inflating the histogram footprint.
const HIST_MAX_US: u64 = 5_000_000;

pub struct Args<'a> {
    pub proxy: &'a str,
    pub udp_target: &'a str,
    pub clients: u32,
    pub duration_secs: u64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct BurstReport {
    pub test: &'static str,
    pub clients: u32,
    pub duration_secs: u64,
    pub sent: u64,
    pub received: u64,
    pub loss_pct: f64,
    pub rps: f64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.clients == 0 {
        bail!("--clients must be > 0");
    }
    if args.duration_secs == 0 {
        bail!("--duration must be > 0");
    }
    tracing::info!(
        clients = args.clients,
        duration_secs = args.duration_secs,
        proxy = args.proxy,
        target = args.udp_target,
        "udp_burst begin"
    );

    let sent = Arc::new(AtomicU64::new(0));
    let received = Arc::new(AtomicU64::new(0));
    let send_errors = Arc::new(AtomicU64::new(0));
    let corrupt_responses = Arc::new(AtomicU64::new(0));
    let diag_foreign_valid = Arc::new(AtomicU64::new(0));
    let diag_checksum_invalid = Arc::new(AtomicU64::new(0));
    let diag_length_invalid = Arc::new(AtomicU64::new(0));
    let diag_body_mismatch = Arc::new(AtomicU64::new(0));
    let merged_hist = Arc::new(Mutex::new(
        Histogram::<u64>::new_with_max(HIST_MAX_US, 3).context("init merged histogram")?,
    ));
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.duration_secs);

    let mut handles = Vec::with_capacity(args.clients as usize);
    for client_idx in 0..args.clients {
        let proxy = args.proxy.to_string();
        let target = args.udp_target.to_string();
        let sent = Arc::clone(&sent);
        let received = Arc::clone(&received);
        let send_errors = Arc::clone(&send_errors);
        let corrupt_responses = Arc::clone(&corrupt_responses);
        let diag_foreign_valid = Arc::clone(&diag_foreign_valid);
        let diag_checksum_invalid = Arc::clone(&diag_checksum_invalid);
        let diag_length_invalid = Arc::clone(&diag_length_invalid);
        let diag_body_mismatch = Arc::clone(&diag_body_mismatch);
        let merged = Arc::clone(&merged_hist);
        handles.push(tokio::spawn(async move {
            run_client(
                client_idx,
                &proxy,
                &target,
                deadline,
                sent,
                received,
                send_errors,
                corrupt_responses,
                diag_foreign_valid,
                diag_checksum_invalid,
                diag_length_invalid,
                diag_body_mismatch,
                merged,
            )
            .await
        }));
    }

    // Let every worker finish so all open failures are reported together.
    let mut open_failures = 0u32;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "client task ended with error");
                open_failures += 1;
            }
            Err(join_err) => {
                tracing::warn!(error = %join_err, "client task panicked");
                open_failures += 1;
            }
        }
    }

    let elapsed = started.elapsed();
    let sent_total = sent.load(Ordering::Relaxed);
    let recv_total = received.load(Ordering::Relaxed);
    let send_error_total = send_errors.load(Ordering::Relaxed);
    let corrupt_total = corrupt_responses.load(Ordering::Relaxed);
    let diag_foreign_valid_total = diag_foreign_valid.load(Ordering::Relaxed);
    let diag_checksum_invalid_total = diag_checksum_invalid.load(Ordering::Relaxed);
    let diag_length_invalid_total = diag_length_invalid.load(Ordering::Relaxed);
    let diag_body_mismatch_total = diag_body_mismatch.load(Ordering::Relaxed);
    let rps = if elapsed.as_secs_f64() > 0.0 {
        recv_total as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    let loss_pct = if sent_total > 0 {
        sent_total.saturating_sub(recv_total) as f64 * 100.0 / sent_total as f64
    } else {
        0.0
    };
    let hist = merged_hist.lock().await;
    let p50 = hist.value_at_quantile(0.50);
    let p95 = hist.value_at_quantile(0.95);
    let p99 = hist.value_at_quantile(0.99);
    let max = hist.max();
    drop(hist);

    let report = BurstReport {
        test: "udp_burst",
        clients: args.clients,
        duration_secs: args.duration_secs,
        sent: sent_total,
        received: recv_total,
        loss_pct,
        rps,
        p50_us: p50,
        p95_us: p95,
        p99_us: p99,
        max_us: max,
    };
    tracing::info!(
        sent = sent_total,
        received = recv_total,
        rps,
        loss_pct,
        p50_us = p50,
        p95_us = p95,
        p99_us = p99,
        max_us = max,
        open_failures,
        send_errors = send_error_total,
        corrupt_responses = corrupt_total,
        diag_udp_foreign_valid = diag_foreign_valid_total,
        diag_udp_checksum_invalid = diag_checksum_invalid_total,
        diag_udp_length_invalid = diag_length_invalid_total,
        diag_udp_body_mismatch = diag_body_mismatch_total,
        "udp_burst complete"
    );
    write_report(args.out, &report)?;

    if open_failures > 0 || send_error_total > 0 || corrupt_total > 0 {
        bail!(
            "udp_burst correctness failures: open={open_failures}, send={send_error_total}, \
             corrupt={corrupt_total} (see {})",
            args.out
        );
    }

    if rps < RPS_SANITY_FLOOR {
        bail!(
            "RPS {:.1} below sanity floor {} — gateway likely crashed (see {})",
            rps,
            RPS_SANITY_FLOOR,
            args.out
        );
    }

    tracing::info!(out = args.out, "PASS: udp_burst");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_client(
    client_idx: u32,
    proxy: &str,
    target: &str,
    deadline: Instant,
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
    send_errors: Arc<AtomicU64>,
    corrupt_responses: Arc<AtomicU64>,
    diag_foreign_valid: Arc<AtomicU64>,
    diag_checksum_invalid: Arc<AtomicU64>,
    diag_length_invalid: Arc<AtomicU64>,
    diag_body_mismatch: Arc<AtomicU64>,
    merged: Arc<Mutex<Histogram<u64>>>,
) -> Result<()> {
    let session = Socks5UdpSession::open(proxy, target).await?;
    let mut local_hist =
        Histogram::<u64>::new_with_max(HIST_MAX_US, 3).context("init local histogram")?;
    let payload = build_payload(client_idx);
    while Instant::now() < deadline {
        let send_started = Instant::now();
        if session.send(&payload).await.is_err() {
            send_errors.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(SEND_INTERVAL).await;
            continue;
        }
        sent.fetch_add(1, Ordering::Relaxed);

        match session.recv(RECV_TIMEOUT).await {
            Ok(buf) if buf == payload => {
                received.fetch_add(1, Ordering::Relaxed);
                let dt_us = send_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                let _ = local_hist.record(dt_us.min(HIST_MAX_US));
            }
            Ok(buf) => {
                corrupt_responses.fetch_add(1, Ordering::Relaxed);
                if buf.len() != PAYLOAD_BYTES {
                    diag_length_invalid.fetch_add(1, Ordering::Relaxed);
                } else {
                    let body_len = PAYLOAD_BYTES - CHECKSUM_BYTES;
                    let actual_checksum = u32::from_be_bytes(
                        buf[body_len..]
                            .try_into()
                            .expect("fixed checksum trailer length"),
                    );
                    if xor_fold(&buf[..body_len]) != actual_checksum {
                        diag_checksum_invalid.fetch_add(1, Ordering::Relaxed);
                    } else if buf[..4] != client_idx.to_be_bytes() {
                        diag_foreign_valid.fetch_add(1, Ordering::Relaxed);
                    } else {
                        diag_body_mismatch.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => {
                // Timeout / receive failure is counted as loss via sent vs received.
            }
        }

        let next = send_started + SEND_INTERVAL;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        }
    }

    let mut m = merged.lock().await;
    let _ = m.add(local_hist);
    Ok(())
}

fn build_payload(client_idx: u32) -> Vec<u8> {
    let mut buf = vec![0u8; PAYLOAD_BYTES];
    let body_len = PAYLOAD_BYTES - CHECKSUM_BYTES;
    // First 4 bytes: client idx so the recv side could disambiguate
    // shared-target replies if needed (one socket per session here, so
    // not strictly necessary — but cheap to encode).
    buf[..4].copy_from_slice(&client_idx.to_be_bytes());
    for (i, b) in buf[..body_len].iter_mut().enumerate().skip(4) {
        *b = (i as u8).wrapping_mul(31);
    }
    let checksum = xor_fold(&buf[..body_len]);
    buf[body_len..].copy_from_slice(&checksum.to_be_bytes());
    buf
}

fn write_report(path: &str, report: &BurstReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize burst report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plan_spec_constants_match() {
        assert_eq!(DEFAULT_CLIENTS, 1000);
        assert_eq!(DEFAULT_DURATION_SECS, 30);
        let floor = RPS_SANITY_FLOOR;
        assert!(floor > 0.0, "floor must be positive, got {floor}");
        assert!(
            floor < 30_000.0,
            "floor must be below expected RPS, got {floor}"
        );
    }

    #[test]
    fn build_payload_is_deterministic_per_client() {
        let a = build_payload(7);
        let b = build_payload(7);
        assert_eq!(a, b);
        assert_eq!(a.len(), PAYLOAD_BYTES);
        // First 4 bytes encode the idx in BE.
        assert_eq!(&a[..4], &7u32.to_be_bytes());
        let body_len = PAYLOAD_BYTES - CHECKSUM_BYTES;
        assert_eq!(
            &a[body_len..],
            xor_fold(&a[..body_len]).to_be_bytes().as_slice()
        );
    }

    #[test]
    fn build_payload_varies_by_client() {
        let a = build_payload(1);
        let b = build_payload(2);
        assert_ne!(a[..4], b[..4]);
    }
}
