//! TUIC bi-stream CONNECT handler and its half-close-safe pipe.
//!
//! Split out of `lib.rs`. This is
//! the TCP path — datagram / uni-stream packet flow lives in `lib.rs`
//! alongside `handle_packet_bytes`, and fragment reassembly lives in
//! `frag.rs`.

use std::sync::Arc;

use crate::backend::TuicBackend;
use anyhow::bail;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::addr::{format_addr, read_addr};
use crate::{CMD_CONNECT, CMD_DISSOCIATE, CMD_PACKET, TUIC_VER};

pub(crate) async fn handle_bi_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    backend: Arc<dyn TuicBackend>,
    group: Option<String>,
) -> anyhow::Result<()> {
    let mut hdr = [0u8; 2];
    recv.read_exact(&mut hdr).await?;
    if hdr[0] != TUIC_VER {
        bail!("bad TUIC version");
    }
    match hdr[1] {
        CMD_CONNECT => {
            let Some(route_key) = group else {
                bail!("not authenticated");
            };
            let addr = read_addr(&mut recv).await?;
            let target = format_addr(&addr);
            tracing::info!(%route_key, %target, "tuic CMD_CONNECT");
            let tunnel = match backend.open_tcp(&route_key, &target).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(%route_key, %target, error = %e, "tuic CMD_CONNECT failed");
                    return Err(e);
                }
            };
            pipe(send, recv, tunnel).await
        }
        CMD_PACKET | CMD_DISSOCIATE => {
            let _ = send.write_all(&[TUIC_VER, CMD_DISSOCIATE]).await;
            Ok(())
        }
        other => bail!("unsupported TUIC command: {other:#x}"),
    }
}

async fn pipe<T>(send: quinn::SendStream, recv: quinn::RecvStream, tunnel: T) -> anyhow::Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (tr, tw) = tokio::io::split(tunnel);
    // First-done-force-close pattern.
    //
    // Why: when Sunshine closes the RTSP connection, only the tunnel→client
    // half sees EOF. The client→tunnel half remains blocked in
    // `recv.read(...)` because Moonlight doesn't proactively close its
    // half-stream — it expects EOF on its read side first. If we simply
    // wait for BOTH halves to finish, the connection lingers until QUIC's
    // idle timeout and Moonlight reports a multi-second stall (the classic
    // "dropped frames" symptom on RTSP teardown).
    //
    // Fix: on first-done, abort the other task. Aborting drops the
    // `RecvStream`/`SendStream`/tunnel-half handles, which resets the QUIC
    // stream and releases the tunnel conn. The stuck direction unblocks
    // immediately rather than waiting 15 s for idle timeout.
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(2);

    let up_done = done_tx.clone();
    let up_handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        let mut recv = recv;
        let mut tw = tw;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    if tw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tw.shutdown().await;
        let _ = up_done.send(()).await;
    });

    let down_done = done_tx.clone();
    let down_handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        let mut send = send;
        let mut tr = tr;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match tr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = send.finish();
        let _ = down_done.send(()).await;
    });
    drop(done_tx);

    // Wait for whichever half finishes first, then force-cancel the other.
    let _ = done_rx.recv().await;
    up_handle.abort();
    down_handle.abort();
    Ok(())
}
