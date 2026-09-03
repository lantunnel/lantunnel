//! HTTP CONNECT + forward HTTP proxy frontend. Two entry points: `serve`
//! (plain HTTP/1.1) and `serve_tls` (HTTPS).
//!
//! NOT WIRED INTO ANY SHIPPED BINARY. It is kept as the starting point for a
//! future opt-in Gateway proxy listener. `lantunnel-gateway` refuses to start
//! when `gateway.proxy.*` is configured; see
//! `apps/lantunnel-gateway/src/main.rs`.
//!
//! Every request must present `Proxy-Authorization: Basic <base64(user:pass)>`.
//! Checking those credentials is entirely the embedder's job: the crate takes
//! an [`AuthValidator`] closure and treats whatever it returns as an opaque
//! routing key. There is no shared proxy secret, and introducing one is a
//! decision for whoever wires this up.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
pub mod backend;

use backend::HttpProxyBackend;

/// Validator for `(username, password)` from the `Proxy-Authorization: Basic`
/// header. Returns `Some(route_key)` to accept, `None` to reject.
///
/// The route key is passed straight to [`backend::HttpProxyBackend::open_tcp`]
/// and is never interpreted here.
pub type AuthValidator = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

const REALM: &str = "lantunnel";
const MAX_HTTP_SESSIONS: usize = 4096;

/// Start the plain-HTTP proxy. All requests require Basic auth.
pub async fn serve(
    listen_addr: SocketAddr,
    backend: Arc<dyn HttpProxyBackend>,
    auth: AuthValidator,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("http proxy bind {listen_addr}"))?;
    tracing::info!(addr = %listen_addr, "http proxy listening (Proxy-Authorization required)");
    let permits = Arc::new(Semaphore::new(MAX_HTTP_SESSIONS));

    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept error");
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            backend.increment_listener_rejects();
            tracing::warn!(peer = %peer, max_sessions = MAX_HTTP_SESSIONS, "http proxy session limit reached; rejecting");
            drop(socket);
            continue;
        };
        let backend = backend.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle(socket, backend, auth).await {
                tracing::debug!(peer = %peer, error = %e, "http proxy session ended");
            }
        });
    }
}

/// Start the HTTPS proxy (TLS-wrapped HTTP CONNECT / forward proxy). Used when
/// cert+key are set.
pub async fn serve_tls(
    listen_addr: SocketAddr,
    backend: Arc<dyn HttpProxyBackend>,
    auth: AuthValidator,
    tls_cfg: Arc<rustls::ServerConfig>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("https proxy bind {listen_addr}"))?;
    let acceptor = TlsAcceptor::from(tls_cfg);
    tracing::info!(addr = %listen_addr, "https proxy listening (Proxy-Authorization required)");
    let permits = Arc::new(Semaphore::new(MAX_HTTP_SESSIONS));

    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept error");
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            backend.increment_listener_rejects();
            tracing::warn!(peer = %peer, max_sessions = MAX_HTTP_SESSIONS, "https proxy session limit reached; rejecting");
            drop(socket);
            continue;
        };
        let backend = backend.clone();
        let auth = auth.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let tls_stream = match acceptor.accept(socket).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %peer, error = %e, "tls accept failed");
                    return;
                }
            };
            if let Err(e) = handle(tls_stream, backend, auth).await {
                tracing::debug!(peer = %peer, error = %e, "https proxy session ended");
            }
        });
    }
}

async fn handle<IO>(
    mut socket: IO,
    backend: Arc<dyn HttpProxyBackend>,
    auth: AuthValidator,
) -> anyhow::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    // Bound the header-reading loop so a slowloris peer can't pin a
    // handler (+ fd + stack) by dripping bytes below the 64 KiB cap.
    // The outer pipe phase (CONNECT tunneling / HTTP forwarding) is
    // intentionally NOT covered — those are long-lived flows.
    let read_hdrs = async {
        loop {
            let n = socket.read(&mut tmp).await?;
            if n == 0 {
                anyhow::bail!("peer closed before sending request");
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_double_crlf(&buf) {
                return Ok::<usize, anyhow::Error>(pos + 4);
            }
            if buf.len() > 64 * 1024 {
                anyhow::bail!("http header too large");
            }
        }
    };
    let head_end = match tokio::time::timeout(std::time::Duration::from_secs(10), read_hdrs).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("http header read timed out"),
    };

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let _ = req.parse(&buf[..head_end])?;
    let method = req.method.unwrap_or("").to_ascii_uppercase();
    let path = req.path.unwrap_or("").to_string();

    // Extract Proxy-Authorization and validate before doing any routing work.
    let route_key = match extract_proxy_auth(req.headers) {
        Some((user, pass)) => match (auth)(&user, &pass) {
            Some(route_key) => {
                tracing::debug!(%user, "http proxy auth accepted");
                route_key
            }
            None => {
                tracing::debug!(%user, "http proxy auth rejected (bad credentials)");
                write_407(&mut socket).await.ok();
                anyhow::bail!("proxy auth rejected");
            }
        },
        None => {
            tracing::debug!("http proxy auth missing");
            write_407(&mut socket).await.ok();
            anyhow::bail!("missing Proxy-Authorization");
        }
    };

    if method == "CONNECT" {
        tracing::debug!(route_key = %route_key, target = %path, "http CONNECT");
        let tunnel = match backend.open_tcp(&route_key, &path).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(route_key = %route_key, target = %path, error = %e, "http CONNECT failed");
                return Err(e);
            }
        };
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        pipe(socket, tunnel).await
    } else {
        let (host, rewritten) = rewrite_absolute_uri(&buf[..head_end])?;
        let target = if host.contains(':') {
            host.clone()
        } else {
            format!("{host}:80")
        };
        tracing::debug!(route_key = %route_key, %target, %method, "http forward");
        let mut tunnel = backend.open_tcp(&route_key, &target).await?;
        let remaining = &buf[head_end..];
        tunnel.write_all(&rewritten).await?;
        if !remaining.is_empty() {
            tunnel.write_all(remaining).await?;
        }
        pipe(socket, tunnel).await
    }
}

/// Extract `(username, password)` from the first matching
/// `Proxy-Authorization: Basic <b64>` header, if any.
fn extract_proxy_auth(headers: &[httparse::Header<'_>]) -> Option<(String, String)> {
    for h in headers {
        if h.name.eq_ignore_ascii_case("proxy-authorization") {
            let v = std::str::from_utf8(h.value).ok()?.trim();
            let rest = v
                .strip_prefix("Basic ")
                .or_else(|| v.strip_prefix("basic "))?;
            let raw = STANDARD.decode(rest.trim()).ok()?;
            let s = String::from_utf8(raw).ok()?;
            let mut parts = s.splitn(2, ':');
            let u = parts.next()?.to_string();
            let p = parts.next().unwrap_or("").to_string();
            return Some((u, p));
        }
    }
    None
}

async fn write_407<IO: AsyncWrite + Unpin>(socket: &mut IO) -> anyhow::Result<()> {
    let body = b"Proxy Authentication Required\n";
    let reply = format!(
        "HTTP/1.1 407 Proxy Authentication Required\r\n\
         Proxy-Authenticate: Basic realm=\"{realm}\"\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        realm = REALM,
        len = body.len(),
    );
    socket.write_all(reply.as_bytes()).await?;
    socket.write_all(body).await?;
    Ok(())
}

async fn pipe<A, B>(a: A, b: B) -> anyhow::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin + Send,
    B: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);
    let a_to_b = tokio::io::copy(&mut ar, &mut bw);
    let b_to_a = tokio::io::copy(&mut br, &mut aw);
    let _ = tokio::try_join!(a_to_b, b_to_a);
    Ok(())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn rewrite_absolute_uri(head: &[u8]) -> anyhow::Result<(String, Vec<u8>)> {
    let s = std::str::from_utf8(head)?;
    let header = s
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("http head missing final CRLFCRLF"))?;
    let mut lines = header.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let uri = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let (host, path) = if let Some(rest) = uri.strip_prefix("http://") {
        let slash = rest.find('/').unwrap_or(rest.len());
        (
            rest[..slash].to_string(),
            if slash == rest.len() {
                "/".to_string()
            } else {
                rest[slash..].to_string()
            },
        )
    } else {
        return Err(anyhow::anyhow!("non-absolute URI in forward proxy request"));
    };
    let new_first = format!("{method} {path} {version}");
    let mut rest = String::with_capacity(head.len());
    rest.push_str(&new_first);
    rest.push_str("\r\n");
    for line in lines {
        // Strip hop-by-hop headers, including Proxy-Authorization which must
        // not be forwarded to the origin server.
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-authorization:") || lower.starts_with("proxy-connection:") {
            continue;
        }
        rest.push_str(line);
        rest.push_str("\r\n");
    }
    rest.push_str("\r\n");
    Ok((host, rest.into_bytes()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_absolute_uri_keeps_single_header_body_boundary() {
        let (_host, rewritten) = rewrite_absolute_uri(
            b"POST http://example.test/path?q=1 HTTP/1.1\r\n\
              Host: example.test\r\n\
              Proxy-Authorization: Basic abc\r\n\
              Content-Length: 4\r\n\
              \r\n",
        )
        .unwrap();

        let text = std::str::from_utf8(&rewritten).unwrap();
        assert!(text.starts_with("POST /path?q=1 HTTP/1.1\r\n"));
        assert!(text.contains("Host: example.test\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(!text.to_ascii_lowercase().contains("proxy-authorization:"));
        assert!(text.ends_with("\r\n\r\n"));
        assert!(!text.contains("\r\n\r\n\r\n"));
    }

    #[test]
    fn rewrite_absolute_uri_extracts_host_without_path() {
        let (host, rewritten) =
            rewrite_absolute_uri(b"GET http://example.test HTTP/1.1\r\nHost: example.test\r\n\r\n")
                .unwrap();

        assert_eq!(host, "example.test");
        let text = std::str::from_utf8(&rewritten).unwrap();
        assert!(text.starts_with("GET / HTTP/1.1\r\n"));
    }
}
