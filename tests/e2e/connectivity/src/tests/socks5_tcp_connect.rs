//! V2 SOCKS5 NO AUTH + CONNECT → echo HTTP, expect 200.

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Run the SOCKS5-CONNECT-to-HTTP-200 assertion.
///
/// Pre-conditions:
///   * `proxy_addr` is an already-running Client's loopback SOCKS5 listener.
///   * `target` echo HTTP listener responds 200 to a simple GET.
pub async fn run(proxy_addr: &str, target: &str) -> Result<()> {
    let (host, port) = crate::parse_host_port(target)?;

    tracing::info!(
        proxy = %proxy_addr,
        target = %target,
        "SOCKS5 CONNECT + HTTP GET begin"
    );

    let conn = crate::socks5::connect_no_auth(proxy_addr, &host, port).await?;
    let mut stream = conn.stream;

    let req =
        format!("GET /?probe=v2 HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| anyhow!("write HTTP request through SOCKS5 tunnel: {e}"))?;

    // Read until the server closes the connection (we sent `Connection: close`).
    let mut buf = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(|e| anyhow!("read HTTP response through SOCKS5 tunnel: {e}"))?;

    if buf.is_empty() {
        bail!("empty HTTP response — gateway may have closed the tunnel before any data");
    }

    let response = String::from_utf8_lossy(&buf);
    let status_line = response.lines().next().unwrap_or("<no status line>");
    let ok = status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200");
    if !ok {
        bail!(
            "expected 200 OK status, got status line: {status_line:?} (response={} bytes)",
            buf.len()
        );
    }

    tracing::info!(
        bytes = buf.len(),
        status = status_line,
        "PASS: SOCKS5 CONNECT + HTTP 200"
    );
    Ok(())
}
