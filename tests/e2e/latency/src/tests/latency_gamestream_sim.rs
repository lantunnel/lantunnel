//! Five-minute simulated game stream over a single V2 Client
//! SOCKS5 UDP-ASSOCIATE session.
//!
//! Concurrent senders mirror a real-time game profile:
//!   - video: 60 fps × 1.4 KB
//!   - audio: 100 pps × 400 B
//!   - control: 20 pps × 128 B
//!
//! Plan-spec assertions on loopback:
//!   - 0% loss (every sent packet's echo arrives within timeout)
//!   - P50 < 30 ms across the merged stream
//!   - P99 < 100 ms across the merged stream
//!
//! Architecture:
//!   * One `Arc<Socks5UdpSession>` shared across 3 sender tasks +
//!     1 receiver task.
//!   * Each packet carries a 9-byte prefix: byte 0 is the stream tag
//!     (0=video, 1=audio, 2=control), bytes 1..9 are a monotonic
//!     packet id (u64 BE). Trailing bytes are filler so the total
//!     length matches the per-stream spec.
//!   * Sender records (id → Instant) into a shared `pending` map
//!     before send. Receiver pops (id, started) on each echo and
//!     records `now - started` into the per-stream histogram.
//!   * After the configured duration, senders stop, the receiver
//!     drains for a short grace window, and any still-pending packets
//!     are counted as losses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::proxy::Socks5UdpSession;
use crate::stats::{Report, Stats};

const VIDEO_PPS: u32 = 60;
const VIDEO_BYTES: usize = 1400;

const AUDIO_PPS: u32 = 100;
const AUDIO_BYTES: usize = 400;

const CONTROL_PPS: u32 = 20;
const CONTROL_BYTES: usize = 128;

/// Plan-spec merged-stream P50 budget (microseconds).
pub const P50_BUDGET_US: u64 = 30_000;

/// Plan-spec merged-stream P99 budget (microseconds).
pub const P99_BUDGET_US: u64 = 100_000;

/// Grace window after senders stop, during which the receiver keeps
/// draining echoes. 250 ms is well above any plausible loopback p99
/// and matches the recv-timeout we'd use for a single sample.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Per-stream byte-prefix tag. Encoded as the first byte of the payload.
const TAG_VIDEO: u8 = 0;
const TAG_AUDIO: u8 = 1;
const TAG_CONTROL: u8 = 2;

#[derive(Clone, Copy, Debug)]
struct StreamSpec {
    label: &'static str,
    tag: u8,
    pps: u32,
    bytes: usize,
}

const STREAMS: [StreamSpec; 3] = [
    StreamSpec {
        label: "video",
        tag: TAG_VIDEO,
        pps: VIDEO_PPS,
        bytes: VIDEO_BYTES,
    },
    StreamSpec {
        label: "audio",
        tag: TAG_AUDIO,
        pps: AUDIO_PPS,
        bytes: AUDIO_BYTES,
    },
    StreamSpec {
        label: "control",
        tag: TAG_CONTROL,
        pps: CONTROL_PPS,
        bytes: CONTROL_BYTES,
    },
];

pub struct Args<'a> {
    pub proxy: &'a str,
    pub udp_target: &'a str,
    pub duration_secs: u64,
    pub p50_budget_us: u64,
    pub p99_budget_us: u64,
    pub loss_budget_pct: f64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
struct PerStream {
    label: &'static str,
    pps: u32,
    bytes: usize,
    sent: u64,
    received: u64,
    loss_pct: f64,
    rtt: Report,
}

#[derive(Debug, Serialize)]
struct GamestreamReport {
    test: &'static str,
    duration_secs: u64,
    p50_budget_us: u64,
    p99_budget_us: u64,
    loss_budget_pct: f64,
    streams: Vec<PerStream>,
    merged: Report,
    merged_p50_within_budget: bool,
    merged_p99_within_budget: bool,
    max_loss_pct: f64,
    loss_within_budget: bool,
    zero_loss: bool,
}

/// Per-receiver shared state. The senders only ever insert; the
/// receiver only ever removes. A single `Mutex<HashMap>` is fine at
/// our combined ~180 pps target — fast enough that lock contention
/// is well below the percentile resolution.
type Pending = Arc<Mutex<HashMap<u64, Instant>>>;

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.duration_secs == 0 {
        bail!("--duration must be > 0");
    }
    if !(0.0..=100.0).contains(&args.loss_budget_pct) {
        bail!("--loss-budget-pct must be between 0 and 100");
    }
    tracing::info!(
        duration_secs = args.duration_secs,
        p50_budget_us = args.p50_budget_us,
        p99_budget_us = args.p99_budget_us,
        loss_budget_pct = args.loss_budget_pct,
        proxy = args.proxy,
        udp_target = args.udp_target,
        "latency_gamestream_sim begin"
    );

    let session = Arc::new(Socks5UdpSession::open(args.proxy, args.udp_target).await?);
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    // 60 s upper bound — plan asserts P99 < 100 ms, but we allow up to
    // 60 s in the histogram so an unexpectedly stuck packet is recorded
    // accurately rather than saturated.
    let mut per_stream_stats: Vec<Stats> = Vec::with_capacity(STREAMS.len());
    for _ in 0..STREAMS.len() {
        per_stream_stats.push(Stats::new(60_000_000)?);
    }
    let mut sent_counts = vec![0u64; STREAMS.len()];

    // Spawn the receiver before any sender so no echo is missed.
    let recv_session = Arc::clone(&session);
    let recv_pending = Arc::clone(&pending);
    let (recv_tx, mut recv_rx) = tokio::sync::mpsc::unbounded_channel::<(u8, Duration)>();
    let stop_token = Arc::new(tokio::sync::Notify::new());
    let recv_stop = Arc::clone(&stop_token);
    let recv_handle =
        tokio::spawn(
            async move { recv_loop(recv_session, recv_pending, recv_tx, recv_stop).await },
        );

    // Spawn one sender per stream.
    let total_dur = Duration::from_secs(args.duration_secs);
    let started_at = Instant::now();
    let mut sender_handles = Vec::with_capacity(STREAMS.len());
    for spec in STREAMS {
        let s_session = Arc::clone(&session);
        let s_pending = Arc::clone(&pending);
        let h =
            tokio::spawn(async move { sender_loop(s_session, s_pending, spec, total_dur).await });
        sender_handles.push(h);
    }

    // Block on senders finishing. Each returns its final `sent` count.
    for (i, h) in sender_handles.into_iter().enumerate() {
        let sent = h
            .await
            .map_err(|e| anyhow!("sender {} panicked: {e}", STREAMS[i].label))?
            .map_err(|e| anyhow!("sender {} failed: {e}", STREAMS[i].label))?;
        sent_counts[i] = sent;
    }
    tracing::info!(
        elapsed_secs = started_at.elapsed().as_secs(),
        sent_video = sent_counts[0],
        sent_audio = sent_counts[1],
        sent_control = sent_counts[2],
        "senders complete; draining receiver"
    );

    // Let the receiver drain in-flight echoes, then signal stop.
    tokio::time::sleep(DRAIN_GRACE).await;
    stop_token.notify_waiters();
    recv_handle
        .await
        .map_err(|e| anyhow!("receiver task panicked: {e}"))?;
    drop(session); // tear down SOCKS5 mapping (closes TCP control channel).

    // Drain (tag, dt) results into per-stream + merged stats. Both are
    // populated from the same raw points so the merged percentiles are
    // exact, not estimated from per-stream bin midpoints.
    let mut merged = Stats::new(60_000_000)?;
    while let Ok((tag, dt)) = recv_rx.try_recv() {
        if let Some(idx) = STREAMS.iter().position(|s| s.tag == tag) {
            per_stream_stats[idx].record(dt)?;
            merged.record(dt)?;
        }
    }

    let mut streams_out = Vec::with_capacity(STREAMS.len());
    for (i, spec) in STREAMS.iter().enumerate() {
        let st = per_stream_stats[i].report();
        let received = st.count;
        let sent = sent_counts[i];
        let loss = if sent == 0 {
            0.0
        } else {
            (sent.saturating_sub(received) as f64) * 100.0 / sent as f64
        };
        tracing::info!(
            stream = spec.label,
            sent,
            received,
            loss_pct = loss,
            p50_us = st.p50_us,
            p99_us = st.p99_us,
            "stream result"
        );
        streams_out.push(PerStream {
            label: spec.label,
            pps: spec.pps,
            bytes: spec.bytes,
            sent,
            received,
            loss_pct: loss,
            rtt: st,
        });
    }
    let merged_report = merged.report();
    let p50_ok = merged_report.p50_us < args.p50_budget_us;
    let p99_ok = merged_report.p99_us < args.p99_budget_us;
    let max_loss_pct = streams_out
        .iter()
        .map(|s| s.loss_pct)
        .fold(0.0_f64, f64::max);
    let loss_ok = max_loss_pct <= args.loss_budget_pct;
    let zero_loss = streams_out.iter().all(|s| s.sent == s.received);

    let final_report = GamestreamReport {
        test: "latency_gamestream_sim",
        duration_secs: args.duration_secs,
        p50_budget_us: args.p50_budget_us,
        p99_budget_us: args.p99_budget_us,
        loss_budget_pct: args.loss_budget_pct,
        streams: streams_out,
        merged: merged_report.clone(),
        merged_p50_within_budget: p50_ok,
        merged_p99_within_budget: p99_ok,
        max_loss_pct,
        loss_within_budget: loss_ok,
        zero_loss,
    };
    write_report(args.out, &final_report)?;

    let mut breaches = Vec::new();
    if !loss_ok {
        breaches.push(format!(
            "max loss = {:.3}% > {:.3}%",
            max_loss_pct, args.loss_budget_pct
        ));
    }
    if !p50_ok {
        breaches.push(format!(
            "merged p50 = {} µs >= {} µs",
            merged_report.p50_us, args.p50_budget_us
        ));
    }
    if !p99_ok {
        breaches.push(format!(
            "merged p99 = {} µs >= {} µs",
            merged_report.p99_us, args.p99_budget_us
        ));
    }
    if !breaches.is_empty() {
        bail!(
            "gamestream-sim breach(es): {} — see {}",
            breaches.join("; "),
            args.out
        );
    }
    tracing::info!(out = args.out, "PASS: latency_gamestream_sim");
    Ok(())
}

/// Sender for one stream. Sends packets at `spec.pps` for `total_dur`,
/// recording each packet's `(id → Instant)` into `pending` before send.
/// Returns the total number of packets sent.
async fn sender_loop(
    session: Arc<Socks5UdpSession>,
    pending: Pending,
    spec: StreamSpec,
    total_dur: Duration,
) -> Result<u64> {
    let interval = Duration::from_secs_f64(1.0 / spec.pps as f64);
    let started = Instant::now();
    let mut sent: u64 = 0;
    let mut next = Instant::now();
    while started.elapsed() < total_dur {
        // Build payload: [tag][8-byte-id-be][filler...].
        let id = next_id(spec.tag, sent);
        let mut payload = Vec::with_capacity(spec.bytes);
        payload.push(spec.tag);
        payload.extend_from_slice(&id.to_be_bytes());
        // Filler bytes (deterministic, but content is irrelevant — we
        // never inspect them, only the prefix tag + id).
        let filler_len = spec.bytes.saturating_sub(payload.len());
        payload.extend(std::iter::repeat_n(0xa5u8, filler_len));

        {
            let mut p = pending.lock().await;
            p.insert(id, Instant::now());
        }
        session
            .send(&payload)
            .await
            .with_context(|| format!("{} sender send", spec.label))?;
        sent += 1;

        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        } else {
            // Behind schedule — keep firing as fast as possible. Don't
            // spin forever though: yield to the runtime so the receiver
            // can keep draining.
            tokio::task::yield_now().await;
        }
    }
    Ok(sent)
}

/// Pack stream-tag + per-stream sequence into a globally unique u64.
/// Top byte = tag, low 56 bits = sequence. Senders never collide because
/// each owns its tag.
fn next_id(tag: u8, seq: u64) -> u64 {
    ((tag as u64) << 56) | (seq & 0x00ff_ffff_ffff_ffff)
}

/// Receiver loop. Reads echoes off the shared session's UDP socket,
/// extracts the (tag, id) prefix, looks up `id` in `pending`, and
/// pushes `(tag, dt)` to `tx`. Stops when `stop_token` is notified.
async fn recv_loop(
    session: Arc<Socks5UdpSession>,
    pending: Pending,
    tx: tokio::sync::mpsc::UnboundedSender<(u8, Duration)>,
    stop_token: Arc<tokio::sync::Notify>,
) {
    let stop_fut = async { stop_token.notified().await };
    tokio::pin!(stop_fut);
    loop {
        // Race: stop OR a fresh recv. The recv timeout is intentionally
        // short so we re-check the stop signal frequently — DRAIN_GRACE
        // (250 ms) is comfortable.
        tokio::select! {
            _ = &mut stop_fut => break,
            r = session.recv(Duration::from_millis(50)) => {
                match r {
                    Ok(buf) if buf.len() >= 9 => {
                        let tag = buf[0];
                        let mut id_b = [0u8; 8];
                        id_b.copy_from_slice(&buf[1..9]);
                        let id = u64::from_be_bytes(id_b);
                        let started_at = {
                            let mut p = pending.lock().await;
                            p.remove(&id)
                        };
                        if let Some(t0) = started_at {
                            let _ = tx.send((tag, t0.elapsed()));
                        }
                    }
                    Ok(_) => { /* short reply — ignore */ }
                    Err(_) => { /* timeout or transient — keep going */ }
                }
            }
        }
    }
}

fn write_report(path: &str, report: &GamestreamReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize gamestream report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn stream_spec_matches_plan() {
        assert_eq!(STREAMS[0].label, "video");
        assert_eq!(STREAMS[0].pps, 60);
        assert_eq!(STREAMS[0].bytes, 1400);
        assert_eq!(STREAMS[1].label, "audio");
        assert_eq!(STREAMS[1].pps, 100);
        assert_eq!(STREAMS[1].bytes, 400);
        assert_eq!(STREAMS[2].label, "control");
        assert_eq!(STREAMS[2].pps, 20);
        assert_eq!(STREAMS[2].bytes, 128);
    }

    #[test]
    fn budgets_match_plan() {
        assert_eq!(P50_BUDGET_US, 30_000);
        assert_eq!(P99_BUDGET_US, 100_000);
    }

    #[test]
    fn next_id_partitions_by_tag() {
        let v0 = next_id(TAG_VIDEO, 0);
        let a0 = next_id(TAG_AUDIO, 0);
        let c0 = next_id(TAG_CONTROL, 0);
        assert_ne!(v0, a0);
        assert_ne!(a0, c0);
        assert_ne!(v0, c0);
        // Top byte is the tag.
        assert_eq!((v0 >> 56) as u8, TAG_VIDEO);
        assert_eq!((a0 >> 56) as u8, TAG_AUDIO);
        assert_eq!((c0 >> 56) as u8, TAG_CONTROL);
    }
}
