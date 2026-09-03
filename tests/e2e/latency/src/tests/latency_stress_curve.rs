//! V2 stepped Mbps curve. For each step we send
//! UDP-via-SOCKS5 packets at a fixed bitrate for `step_duration_secs`,
//! then report the loss rate + max RTT per step.
//!
//! Plan-spec defaults (from the plan's table update):
//!   - steps: 20, 50, 100, 200, 500, 1000 Mbps
//!   - step_duration: 30 s
//!   - packet size: 1400 B (single MTU)
//!
//! No hard pass/fail threshold — the test records measurements only.
//! `run()` always succeeds as long as the SOCKS5 session opens and
//! the per-step traffic loop completes; per-step results land in JSON.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::proxy::Socks5UdpSession;
use crate::stats::{Report, Stats};

/// Plan-spec defaults — single-MTU UDP packets, the largest size we can
/// usefully shovel without IP fragmentation entering the picture.
pub const PACKET_BYTES: usize = 1400;

/// Drain window after each step's senders stop. 250 ms is well above
/// any realistic loopback p99, even at the high-Mbps steps where the
/// proxy stack itself becomes the bottleneck.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

pub struct Args<'a> {
    pub proxy: &'a str,
    pub udp_target: &'a str,
    pub steps_mbps: &'a [u32],
    pub packet_bytes: usize,
    pub step_duration_secs: u64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
struct PerStep {
    mbps: u32,
    /// Theoretical packets per second at the requested rate.
    target_pps: u64,
    sent: u64,
    received: u64,
    loss_pct: f64,
    /// Per-step RTT histogram. Max + p99 are the headline numbers
    /// recorded in the plan's "Max latency" column.
    rtt: Report,
}

#[derive(Debug, Serialize)]
struct StressCurveReport {
    test: &'static str,
    packet_bytes: usize,
    step_duration_secs: u64,
    steps: Vec<PerStep>,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.steps_mbps.is_empty() {
        bail!("--steps must contain at least one entry");
    }
    if args.step_duration_secs == 0 {
        bail!("--step-duration must be > 0");
    }
    if !(8..=60_000).contains(&args.packet_bytes) {
        bail!("--packet-bytes must be between 8 and 60000");
    }
    tracing::info!(
        steps = ?args.steps_mbps,
        packet_bytes = args.packet_bytes,
        step_duration_secs = args.step_duration_secs,
        proxy = args.proxy,
        udp_target = args.udp_target,
        "latency_stress_curve begin"
    );

    let mut steps_out = Vec::with_capacity(args.steps_mbps.len());
    for &mbps in args.steps_mbps {
        let step = run_step(&args, mbps).await?;
        tracing::info!(
            mbps,
            target_pps = step.target_pps,
            sent = step.sent,
            received = step.received,
            loss_pct = step.loss_pct,
            max_us = step.rtt.max_us,
            p99_us = step.rtt.p99_us,
            "step complete"
        );
        steps_out.push(step);
    }

    let report = StressCurveReport {
        test: "latency_stress_curve",
        packet_bytes: args.packet_bytes,
        step_duration_secs: args.step_duration_secs,
        steps: steps_out,
    };
    write_report(args.out, &report)?;
    tracing::info!(out = args.out, "PASS: latency_stress_curve");
    Ok(())
}

/// Run a single Mbps step: open a fresh SOCKS5 UDP-ASSOC, fire packets
/// at the target rate for the configured duration, drain echoes, and
/// return the per-step result.
async fn run_step(args: &Args<'_>, mbps: u32) -> Result<PerStep> {
    let target_pps = mbps_to_pps(mbps, args.packet_bytes);
    let session = Arc::new(Socks5UdpSession::open(args.proxy, args.udp_target).await?);
    let pending: Arc<Mutex<HashMap<u64, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let stop_token = Arc::new(tokio::sync::Notify::new());

    let (recv_tx, mut recv_rx) = tokio::sync::mpsc::unbounded_channel::<Duration>();
    let recv_handle = {
        let s = Arc::clone(&session);
        let p = Arc::clone(&pending);
        let stop = Arc::clone(&stop_token);
        tokio::spawn(async move { recv_loop(s, p, recv_tx, stop).await })
    };

    let send_handle = {
        let s = Arc::clone(&session);
        let p = Arc::clone(&pending);
        let dur = Duration::from_secs(args.step_duration_secs);
        let packet_bytes = args.packet_bytes;
        tokio::spawn(async move { sender_loop(s, p, target_pps, packet_bytes, dur).await })
    };

    let sent = send_handle
        .await
        .map_err(|e| anyhow!("step {mbps} sender panicked: {e}"))?
        .with_context(|| format!("step {mbps} sender failed"))?;

    tokio::time::sleep(DRAIN_GRACE).await;
    stop_token.notify_waiters();
    recv_handle
        .await
        .map_err(|e| anyhow!("step {mbps} receiver panicked: {e}"))?;
    drop(session);

    // 60 s upper bound — covers the worst-case stuck-packet scenario at
    // high Mbps where the proxy queues briefly stall.
    let mut stats = Stats::new(60_000_000)?;
    while let Ok(dt) = recv_rx.try_recv() {
        stats.record(dt)?;
    }
    let rtt = stats.report();
    let received = rtt.count;
    let loss = if sent == 0 {
        0.0
    } else {
        (sent.saturating_sub(received) as f64) * 100.0 / sent as f64
    };
    Ok(PerStep {
        mbps,
        target_pps,
        sent,
        received,
        loss_pct: loss,
        rtt,
    })
}

/// Sender loop. Fires `target_pps` packets/sec for `total_dur`. If the
/// runtime falls behind the schedule (likely at the 1 Gbps step) we
/// keep firing as fast as possible and yield between bursts so the
/// receiver can keep draining.
async fn sender_loop(
    session: Arc<Socks5UdpSession>,
    pending: Arc<Mutex<HashMap<u64, Instant>>>,
    target_pps: u64,
    packet_bytes: usize,
    total_dur: Duration,
) -> Result<u64> {
    if target_pps == 0 {
        return Ok(0);
    }
    let interval = Duration::from_secs_f64(1.0 / target_pps as f64);
    let started = Instant::now();
    let mut sent: u64 = 0;
    let mut next = Instant::now();
    let mut payload = vec![0u8; packet_bytes];
    while started.elapsed() < total_dur {
        // First 8 bytes = monotonic id (BE); rest is filler.
        let id = sent;
        payload[..8].copy_from_slice(&id.to_be_bytes());
        // Cheap "filler" pattern — distinguishable in a pcap if needed.
        for (i, b) in payload.iter_mut().enumerate().skip(8) {
            *b = (i as u8).wrapping_mul(31);
        }
        {
            let mut p = pending.lock().await;
            p.insert(id, Instant::now());
        }
        session.send(&payload).await.context("stress send")?;
        sent += 1;

        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        } else {
            // Behind schedule — yield so the receiver can drain.
            tokio::task::yield_now().await;
        }
    }
    Ok(sent)
}

/// Receiver loop. Pops sender-Instant from `pending` for each echoed
/// packet's id and forwards the elapsed Duration to the channel.
async fn recv_loop(
    session: Arc<Socks5UdpSession>,
    pending: Arc<Mutex<HashMap<u64, Instant>>>,
    tx: tokio::sync::mpsc::UnboundedSender<Duration>,
    stop_token: Arc<tokio::sync::Notify>,
) {
    let stop_fut = async { stop_token.notified().await };
    tokio::pin!(stop_fut);
    loop {
        tokio::select! {
            _ = &mut stop_fut => break,
            r = session.recv(Duration::from_millis(50)) => {
                match r {
                    Ok(buf) if buf.len() >= 8 => {
                        let mut id_b = [0u8; 8];
                        id_b.copy_from_slice(&buf[..8]);
                        let id = u64::from_be_bytes(id_b);
                        let started_at = {
                            let mut p = pending.lock().await;
                            p.remove(&id)
                        };
                        if let Some(t0) = started_at {
                            let _ = tx.send(t0.elapsed());
                        }
                    }
                    Ok(_) => { /* short reply */ }
                    Err(_) => { /* timeout — keep polling */ }
                }
            }
        }
    }
}

/// Convert a Mbps rate into the corresponding packets-per-second
/// stream rate for the configured packet size. Saturates at the high
/// end so a hypothetical 100 Gbps step doesn't overflow.
pub(crate) fn mbps_to_pps(mbps: u32, packet_bytes: usize) -> u64 {
    let bits_per_packet = (packet_bytes as u64) * 8;
    if bits_per_packet == 0 {
        return 0;
    }
    let bits_per_sec = (mbps as u64).saturating_mul(1_000_000);
    bits_per_sec.div_ceil(bits_per_packet)
}

fn write_report(path: &str, report: &StressCurveReport) -> Result<()> {
    crate::reporting::write_json_report(path, report)
        .map_err(|e| anyhow!("serialize stress-curve report: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn mbps_to_pps_known_values() {
        // 100 Mbps × 1400 B/packet → 100e6 / (1400*8) ≈ 8929 pps.
        assert_eq!(mbps_to_pps(100, 1400), 8929);
        // 1 Gbps × 1400 B/packet → 1e9 / (1400*8) ≈ 89286 pps.
        assert_eq!(mbps_to_pps(1000, 1400), 89286);
        // Smaller packets → higher rate.
        assert!(mbps_to_pps(100, 128) > mbps_to_pps(100, 1400));
    }

    #[test]
    fn mbps_to_pps_handles_edges() {
        assert_eq!(mbps_to_pps(0, 1400), 0);
        assert_eq!(mbps_to_pps(100, 0), 0);
    }
}
