//! V2 120-second mixed-stream game-style
//! workload. Three concurrent senders run for the configured duration:
//!
//!   * Video — ~50 Mbps UDP via SOCKS5 UDP-ASSOC (4500 pps × 1.4 KB).
//!   * Audio — ~1 Mbps UDP via SOCKS5 UDP-ASSOC (250 pps × 500 B).
//!   * Control — ~1 Mbps TCP via SOCKS5 CONNECT (125 pps × 1 KB,
//!     framed by a 4-byte length prefix; the HTTP echo target's
//!     POST-and-echo handler does not give us packet boundaries
//!     so we run our own TCP echo over a length-prefixed framing).
//!
//! Phase 2 numbers: 50 Mbps via SOCKS5 UDP-ASSOC already exceeds
//! the relay's sustained throughput (75 % loss at 500 Mbps). We
//! therefore record loss + jitter rather than asserting against
//! either — the JSON report is the artifact, the run-to-completion
//! is the pass condition.
//!
//! Architecture per UDP stream:
//!   * One sender task per stream feeds an async-mpsc with the
//!     `Instant` at which it sent each packet's monotonic id.
//!   * One receiver task per stream polls `Socks5UdpSession::recv`
//!     and matches the echoed id back to the sent timestamp.
//!   * Inter-arrival deltas (jitter) are computed from the receiver
//!     `Instant`s as they come in.
//!
//! The TCP control channel is in-process: we bind a local listener
//! on `127.0.0.1:0`, route a SOCKS5 CONNECT through the proxy back
//! to that port, and run a length-prefixed echo. This is what
//! the streaming-game test actually wants — a real end-to-end
//! round-trip — without depending on echo-services growing a new
//! TCP-echo endpoint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::meter::inter_arrival_jitter_us;
use crate::proxy::{
    echo_one_framed_connection, socks5_connect, Socks5UdpSession, FRAMED_LEN_PREFIX,
};

/// Plan-spec duration: 120 s.
pub const DEFAULT_DURATION_SECS: u64 = 120;

pub const VIDEO_PPS: u32 = 4500;
pub const VIDEO_BYTES: usize = 1400;
pub const AUDIO_PPS: u32 = 250;
pub const AUDIO_BYTES: usize = 500;
pub const CONTROL_PPS: u32 = 125;
pub const CONTROL_BYTES: usize = 1024;

/// UDP recv timeout per packet — well above any plausible loopback
/// p99 so a stuck packet surfaces as `lost`, not as a hung worker.
const UDP_RECV_TIMEOUT: Duration = Duration::from_millis(150);

pub struct Args<'a> {
    pub proxy: &'a str,
    pub udp_target: &'a str,
    pub tcp_echo_target: &'a str,
    pub duration_secs: u64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct PerStream {
    pub label: &'static str,
    pub transport: &'static str,
    pub target_pps: u32,
    pub bytes_per_packet: usize,
    pub sent: u64,
    pub received: u64,
    pub loss_pct: f64,
    /// Inter-arrival jitter (microseconds) — std-dev of receive-side
    /// delta-t for packets that did arrive.
    pub jitter_us: u64,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct StreamingReport {
    pub test: &'static str,
    pub duration_secs: u64,
    pub streams: Vec<PerStream>,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.duration_secs == 0 {
        bail!("--duration must be > 0");
    }
    tracing::info!(
        duration_secs = args.duration_secs,
        proxy = args.proxy,
        udp_target = args.udp_target,
        "udp_streaming_game begin"
    );

    let total_dur = Duration::from_secs(args.duration_secs);
    let video = run_udp_stream(
        "video",
        VIDEO_PPS,
        VIDEO_BYTES,
        args.proxy,
        args.udp_target,
        total_dur,
    );
    let audio = run_udp_stream(
        "audio",
        AUDIO_PPS,
        AUDIO_BYTES,
        args.proxy,
        args.udp_target,
        total_dur,
    );
    let control = run_tcp_control(args.proxy, CONTROL_PPS, CONTROL_BYTES, total_dur);

    let (video_res, audio_res, control_res) = tokio::join!(video, audio, control);
    let video = video_res.context("video stream")?;
    let audio = audio_res.context("audio stream")?;
    let control = control_res.context("control stream")?;

    let streams = vec![video, audio, control];
    for s in &streams {
        tracing::info!(
            label = s.label,
            transport = s.transport,
            sent = s.sent,
            received = s.received,
            loss_pct = s.loss_pct,
            jitter_us = s.jitter_us,
            "stream complete"
        );
    }

    let report = StreamingReport {
        test: "udp_streaming_game",
        duration_secs: args.duration_secs,
        streams,
    };
    write_report(args.out, &report)?;
    tracing::info!(out = args.out, "PASS: udp_streaming_game");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_udp_stream(
    label: &'static str,
    pps: u32,
    payload_bytes: usize,
    proxy: &str,
    target: &str,
    total_dur: Duration,
) -> Result<PerStream> {
    let session = Arc::new(Socks5UdpSession::open(proxy, target).await?);
    let sent_total = Arc::new(AtomicU64::new(0));
    let recv_total = Arc::new(AtomicU64::new(0));
    let recv_arrivals = Arc::new(tokio::sync::Mutex::new(Vec::<Instant>::new()));
    let stop = Arc::new(tokio::sync::Notify::new());

    let recv_handle = {
        let s = Arc::clone(&session);
        let r = Arc::clone(&recv_total);
        let arrivals = Arc::clone(&recv_arrivals);
        let stop = Arc::clone(&stop);
        let payload_len = payload_bytes;
        tokio::spawn(async move {
            recv_loop(s, r, arrivals, stop, payload_len).await;
        })
    };

    let send_handle = {
        let s = Arc::clone(&session);
        let sent = Arc::clone(&sent_total);
        tokio::spawn(async move { sender_loop(s, pps, payload_bytes, total_dur, sent).await })
    };

    send_handle
        .await
        .map_err(|e| anyhow::anyhow!("sender panicked: {e}"))??;
    // Drain grace — let the receiver pick up any in-flight echoes.
    tokio::time::sleep(Duration::from_millis(250)).await;
    stop.notify_waiters();
    let _ = recv_handle.await;
    drop(session);

    let sent = sent_total.load(Ordering::Relaxed);
    let received = recv_total.load(Ordering::Relaxed);
    let loss_pct = if sent > 0 {
        sent.saturating_sub(received) as f64 * 100.0 / sent as f64
    } else {
        0.0
    };
    let arrivals = recv_arrivals.lock().await;
    let jitter_us = inter_arrival_jitter_us(&arrivals);

    Ok(PerStream {
        label,
        transport: "udp",
        target_pps: pps,
        bytes_per_packet: payload_bytes,
        sent,
        received,
        loss_pct,
        jitter_us,
    })
}

async fn sender_loop(
    session: Arc<Socks5UdpSession>,
    pps: u32,
    payload_bytes: usize,
    total_dur: Duration,
    sent: Arc<AtomicU64>,
) -> Result<()> {
    if pps == 0 {
        return Ok(());
    }
    let interval = Duration::from_secs_f64(1.0 / pps as f64);
    let started = Instant::now();
    let mut next = Instant::now();
    let mut id: u64 = 0;
    let mut payload = vec![0u8; payload_bytes];
    while started.elapsed() < total_dur {
        payload[..8].copy_from_slice(&id.to_be_bytes());
        for (i, b) in payload.iter_mut().enumerate().skip(8) {
            *b = (i as u8).wrapping_mul(31);
        }
        if session.send(&payload).await.is_ok() {
            sent.fetch_add(1, Ordering::Relaxed);
        }
        id += 1;

        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
    Ok(())
}

async fn recv_loop(
    session: Arc<Socks5UdpSession>,
    recv_total: Arc<AtomicU64>,
    arrivals: Arc<tokio::sync::Mutex<Vec<Instant>>>,
    stop: Arc<tokio::sync::Notify>,
    expected_len: usize,
) {
    let stop_fut = async { stop.notified().await };
    tokio::pin!(stop_fut);
    loop {
        tokio::select! {
            _ = &mut stop_fut => break,
            r = session.recv(UDP_RECV_TIMEOUT) => {
                match r {
                    Ok(buf) if buf.len() == expected_len => {
                        recv_total.fetch_add(1, Ordering::Relaxed);
                        let mut a = arrivals.lock().await;
                        a.push(Instant::now());
                    }
                    _ => { /* timeout / short-reply — counted as loss */ }
                }
            }
        }
    }
}

async fn run_tcp_control(
    proxy: &str,
    pps: u32,
    payload_bytes: usize,
    total_dur: Duration,
) -> Result<PerStream> {
    // 1. Bind a local TCP listener — its port becomes the SOCKS5
    //    CONNECT target so the request flows through the proxy back
    //    to us.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind in-process TCP echo for control channel")?;
    let local = listener.local_addr().context("local_addr")?;
    let echo_handle = tokio::spawn(echo_one_framed_connection(listener));

    // 2. Dial it via SOCKS5 CONNECT.
    let mut stream = socks5_connect(proxy, "127.0.0.1", local.port())
        .await
        .context("SOCKS5 CONNECT for control channel")?
        .stream;

    let interval = Duration::from_secs_f64(1.0 / pps as f64);
    let started = Instant::now();
    let mut next = Instant::now();
    let mut id: u64 = 0;
    let mut sent: u64 = 0;
    let mut received: u64 = 0;
    let mut arrivals: Vec<Instant> = Vec::new();
    let mut payload = vec![0u8; payload_bytes];
    let mut frame = vec![0u8; FRAMED_LEN_PREFIX + payload_bytes];

    while started.elapsed() < total_dur {
        payload[..8].copy_from_slice(&id.to_be_bytes());
        for (i, b) in payload.iter_mut().enumerate().skip(8) {
            *b = (i as u8).wrapping_mul(31);
        }
        let len_be = (payload_bytes as u32).to_be_bytes();
        frame[..FRAMED_LEN_PREFIX].copy_from_slice(&len_be);
        frame[FRAMED_LEN_PREFIX..].copy_from_slice(&payload);
        if stream.write_all(&frame).await.is_ok() {
            sent += 1;
        }

        // Read a single echoed frame back.
        let mut len_buf = [0u8; FRAMED_LEN_PREFIX];
        if stream.read_exact(&mut len_buf).await.is_ok() {
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == payload_bytes {
                let mut body = vec![0u8; len];
                if stream.read_exact(&mut body).await.is_ok() {
                    received += 1;
                    arrivals.push(Instant::now());
                }
            }
        }
        id += 1;

        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        } else {
            tokio::task::yield_now().await;
        }
    }

    drop(stream);
    let _ = echo_handle.await;

    let loss_pct = if sent > 0 {
        sent.saturating_sub(received) as f64 * 100.0 / sent as f64
    } else {
        0.0
    };
    let jitter_us = inter_arrival_jitter_us(&arrivals);
    Ok(PerStream {
        label: "control",
        transport: "tcp",
        target_pps: pps,
        bytes_per_packet: payload_bytes,
        sent,
        received,
        loss_pct,
        jitter_us,
    })
}

fn write_report(path: &str, report: &StreamingReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize streaming report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plan_spec_constants_match() {
        assert_eq!(DEFAULT_DURATION_SECS, 120);
        // 4500 pps × 1400 B × 8 bits / 1e6 = 50.4 Mbps — close enough.
        let video_mbps = (VIDEO_PPS as u64) * (VIDEO_BYTES as u64) * 8 / 1_000_000;
        assert!((48..=52).contains(&video_mbps), "video={video_mbps}");
    }
}
