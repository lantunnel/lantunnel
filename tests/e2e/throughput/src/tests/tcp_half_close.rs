//! V2 half-close probe.
//!
//! Test plan:
//!   1. Bind an in-process TCP echo server on `127.0.0.1:0`.
//!   2. Open a SOCKS5 CONNECT to that listener via the proxy.
//!   3. Send `--bytes` (default 4 KiB) of a deterministic byte
//!      pattern through the tunnel.
//!   4. Read exactly `--bytes` echoed back. This step proves bytes
//!      traversed both directions through the proxy; we explicitly
//!      avoid racing the FIN against the data because Step 5 is
//!      what we actually want to probe.
//!   5. `shutdown(write)` on the client side — the proxy must
//!      propagate the FIN to the upstream socket. Upstream then
//!      reads 0, calls its own `shutdown(write)`, and the proxy
//!      delivers the resulting FIN back to us.
//!   6. One more `read()` on the client side; it must return 0
//!      (EOF) immediately. A timeout here means the proxy ate the
//!      half-close.
//!
//! The failure mode is the proxy translating `shutdown(write)` into a
//! full-close (or sending RST) instead of a clean FIN, or not propagating it.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::proxy::socks5_connect;

/// Plan-spec payload: 4 KiB. Big enough to cross the SOCKS5 relay's
/// internal buffer multiple times, small enough to finish in well
/// under a second on any healthy proxy.
pub const DEFAULT_BYTES: u64 = 4 * 1024;

/// EOF read timeout. Half-close interop bugs often manifest as a
/// hung read — bound the wait so the test fails fast rather than
/// hanging.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Args<'a> {
    pub proxy: &'a str,
    pub bytes: u64,
    pub out: &'a str,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)] // emitted via `Serialize`; struct fields are not read directly
pub(crate) struct HalfCloseReport {
    pub test: &'static str,
    pub bytes_sent: u64,
    pub bytes_echoed: u64,
    pub eof_seen: bool,
    pub byte_pattern_match: bool,
}

pub async fn run(args: Args<'_>) -> Result<()> {
    if args.bytes == 0 {
        bail!("--bytes must be > 0");
    }
    if args.bytes > 1_000_000 {
        bail!(
            "--bytes too large for half-close test (cap 1 MB): {}",
            args.bytes
        );
    }
    tracing::info!(
        bytes = args.bytes,
        proxy = args.proxy,
        "tcp_half_close begin"
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind in-process TCP echo")?;
    let local = listener.local_addr().context("local_addr")?;
    let echo_handle = tokio::spawn(echo_until_eof(listener));

    let mut stream = socks5_connect(args.proxy, "127.0.0.1", local.port())
        .await
        .context("SOCKS5 CONNECT for half-close test")?
        .stream;

    let payload = build_payload(args.bytes as usize);
    stream.write_all(&payload).await.context("write payload")?;
    stream.flush().await.context("flush before echo-read")?;

    // Step 4: read the full echo first. This isolates the half-close
    // probe in step 5 — if any bytes are missing here, the proxy is
    // dropping data, not mis-handling FIN.
    let mut echoed = vec![0u8; payload.len()];
    let echo_read = async {
        let mut total = 0usize;
        while total < echoed.len() {
            let n = stream
                .read(&mut echoed[total..])
                .await
                .context("read echoed bytes")?;
            if n == 0 {
                bail!("EOF after only {} of {} echoed bytes", total, echoed.len());
            }
            total += n;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(READ_TIMEOUT, echo_read)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "echo read timed out after {:?} — proxy dropping data",
                READ_TIMEOUT
            )
        })??;
    let bytes_echoed_initial = echoed.len() as u64;
    let pattern_match = echoed == payload;

    // Step 5: half-close the write half. Upstream's read returns 0,
    // it calls shutdown() → proxy delivers FIN back to us.
    stream.shutdown().await.context("shutdown(write)")?;
    tracing::debug!(bytes = args.bytes, "shutdown(write) sent");

    // Step 6: one more read should return 0 (EOF) within the timeout.
    let mut tail = vec![0u8; 256];
    let mut eof_seen = false;
    let probe = async {
        let n = stream
            .read(&mut tail)
            .await
            .context("trailing-EOF probe read")?;
        if n == 0 {
            eof_seen = true;
        } else {
            // Anything here would be unexpected — echo loop only
            // sends back what we sent it, and we already read all of
            // that. Surface as a non-fatal warning in the report.
            echoed.extend_from_slice(&tail[..n]);
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(READ_TIMEOUT, probe)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "EOF probe timed out after {:?} — proxy did not propagate half-close",
                READ_TIMEOUT
            )
        })??;

    let _ = echo_handle.await;

    let bytes_echoed = bytes_echoed_initial;

    let report = HalfCloseReport {
        test: "tcp_half_close",
        bytes_sent: args.bytes,
        bytes_echoed,
        eof_seen,
        byte_pattern_match: pattern_match,
    };
    tracing::info!(
        bytes_sent = args.bytes,
        bytes_echoed,
        eof_seen,
        pattern_match,
        "half_close complete"
    );
    write_report(args.out, &report)?;

    if !eof_seen {
        bail!("read did not see EOF — see {}", args.out);
    }
    if bytes_echoed != args.bytes {
        bail!(
            "bytes echoed ({}) != bytes sent ({}) — see {}",
            bytes_echoed,
            args.bytes,
            args.out
        );
    }
    if !pattern_match {
        bail!("echoed bytes do not match payload — see {}", args.out);
    }
    tracing::info!(out = args.out, "PASS: tcp_half_close");
    Ok(())
}

/// Listener task: accept exactly one connection, echo bytes back as
/// they arrive, and close the write half once the peer half-closes
/// (read returns 0). This is the bidirectional "echo until EOF then
/// shut the write side too" loop, which is what the half-close
/// wire-compat test asserts the proxy must round-trip faithfully.
async fn echo_until_eof(listener: TcpListener) {
    let Ok((mut sock, _)) = listener.accept().await else {
        return;
    };
    let mut buf = vec![0u8; 4096];
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            // Peer half-closed. Drain by closing our write half so
            // the client sees EOF after the in-flight echoed bytes.
            let _ = sock.shutdown().await;
            return;
        }
        if sock.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

/// Deterministic byte pattern, same family used by the connectivity
/// tests so a captured pcap shows recognizable bytes.
fn build_payload(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((i as u32).wrapping_mul(0x9e37_79b9) ^ (i as u32 >> 8)) as u8;
    }
    buf
}

fn write_report(path: &str, report: &HalfCloseReport) -> Result<()> {
    crate::reporting::write_json_report(path, report).context("serialize half-close report")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plan_spec_constants_match() {
        assert_eq!(DEFAULT_BYTES, 4096);
    }

    #[test]
    fn build_payload_is_deterministic() {
        let a = build_payload(128);
        let b = build_payload(128);
        assert_eq!(a, b);
        assert_eq!(a.len(), 128);
    }

    #[test]
    fn build_payload_differs_at_offsets() {
        let p = build_payload(64);
        // 0x9e3779b9 multiplier guarantees distinct low bytes between
        // adjacent offsets in this seed family.
        assert_ne!(p[0], p[1]);
    }
}
