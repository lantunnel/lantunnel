//! Engine-internal helpers: time/DNS/cancellation/outcome.
//!
//! Split out of `engine.rs`.
//! These types have no dependency on [`crate::engine::Engine`] itself
//! so they sit in a separate submodule to keep the top-level engine
//! file focused on the connect/disconnect/reconnect control flow.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::lookup_host;
use tp_core::config::{TRANSPORT_TYPE_GRPC, TRANSPORT_TYPE_QUIC, TRANSPORT_TYPE_WEBSOCKET};

/// Return value from a single-attempt session run.
///
/// Produced by `Engine::run_direct_once`; consumed
/// by the reconnect loop to decide "backoff and retry" vs "exit clean".
pub(crate) enum SessionOutcome {
    /// User called `Engine::disconnect`. Do not reconnect.
    UserCancel,
    /// Session died unexpectedly — return the cause so the outer loop can
    /// log it and wait one backoff slot before retrying.
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportKind {
    Quic,
    WebSocket,
    Grpc,
}

impl TransportKind {
    pub(crate) fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | TRANSPORT_TYPE_QUIC => Ok(Self::Quic),
            TRANSPORT_TYPE_WEBSOCKET => Ok(Self::WebSocket),
            TRANSPORT_TYPE_GRPC => Ok(Self::Grpc),
            other => anyhow::bail!(
                "unsupported transport_type {other:?}; expected quic, websocket, or grpc"
            ),
        }
    }
}

pub(crate) fn gateway_endpoint(host: &str, port: u16) -> String {
    let host = host.trim();
    if split_scheme(host).is_some() || authority_has_port(host) {
        return host.to_string();
    }
    if matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub(crate) fn websocket_url(endpoint: &str, tls: bool) -> anyhow::Result<String> {
    let endpoint = endpoint.trim();
    let (scheme, rest) = match split_scheme(endpoint) {
        Some((scheme, rest)) => {
            let scheme = match scheme.to_ascii_lowercase().as_str() {
                "ws" => "ws",
                "wss" => "wss",
                "http" => "ws",
                "https" => "wss",
                other => anyhow::bail!("unsupported websocket URL scheme {other:?}"),
            };
            (scheme, rest)
        }
        None => (if tls { "wss" } else { "ws" }, endpoint),
    };
    if rest.is_empty() {
        anyhow::bail!("websocket gateway endpoint is empty");
    }
    Ok(format!("{scheme}://{}", ensure_websocket_path(rest)))
}

pub(crate) fn grpc_url(endpoint: &str, tls: bool) -> anyhow::Result<String> {
    let endpoint = endpoint.trim();
    if let Some((scheme, _)) = split_scheme(endpoint) {
        match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => return Ok(endpoint.to_string()),
            other => anyhow::bail!("unsupported grpc URL scheme {other:?}"),
        }
    }
    if endpoint.is_empty() {
        anyhow::bail!("grpc gateway endpoint is empty");
    }
    Ok(format!(
        "{}://{endpoint}",
        if tls { "https" } else { "http" }
    ))
}

pub(crate) fn has_tls_scheme(endpoint: &str) -> bool {
    split_scheme(endpoint).is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("wss") || scheme.eq_ignore_ascii_case("https")
    })
}

pub(crate) fn tls_domain(endpoint: &str) -> String {
    let rest = split_scheme(endpoint).map_or(endpoint, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest).trim();
    if let Some(bracketed) = authority.strip_prefix('[') {
        if let Some((host, _)) = bracketed.split_once(']') {
            return host.to_string();
        }
    }
    if authority.matches(':').count() == 1 {
        return authority
            .rsplit_once(':')
            .map(|(host, _)| host.to_string())
            .unwrap_or_else(|| authority.to_string());
    }
    authority.to_string()
}

fn split_scheme(raw: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = raw.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    Some((scheme, rest))
}

fn authority_has_port(authority: &str) -> bool {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest
            .rsplit_once("]:")
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    }
    authority.matches(':').count() == 1
        && authority
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
}

fn ensure_websocket_path(rest: &str) -> String {
    let (before_query, query) = rest
        .split_once('?')
        .map(|(head, tail)| (head, format!("?{tail}")))
        .unwrap_or_else(|| (rest, String::new()));
    if let Some((authority, path)) = before_query.split_once('/') {
        if path.is_empty() {
            format!("{authority}/ws{query}")
        } else {
            format!("{authority}/{path}{query}")
        }
    } else {
        format!("{before_query}/ws{query}")
    }
}

/// Seconds since Unix epoch. Wraps [`chrono::Utc::now`] so callers don't
/// have to import chrono.
pub(crate) fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Resolve `host:port` to a `SocketAddr`, accepting IPv4 literals, IPv6
/// literals, or DNS hostnames. The platform control-plane returns
/// `gateway_addr` as an opaque string which in production is typically a
/// hostname — `SocketAddr::from_str` only accepts numeric IP literals and
/// fails with "invalid socket address syntax" on hostnames, so we go through
/// the async resolver.
///
/// Bounded at 5 s: hostile / captive DNS can otherwise stall the dial up to
/// ~30 s (Linux glibc default resolver budget). The caller's outer backoff
/// loop re-attempts on failure, so a short cap here translates a bad DNS
/// state into fast retries instead of a long blocked dial.
pub(crate) async fn resolve_gateway_addr(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let (host, port) = host_port_for_resolution(host, port)?;
    // `(host, port)` impls ToSocketAddrs and handles IPv6 literals without
    // requiring the caller to bracket them (unlike a "host:port" string).
    let mut iter = match tokio::time::timeout(
        Duration::from_secs(5),
        lookup_host((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(it)) => it,
        Ok(Err(e)) => anyhow::bail!("resolve gateway {host}:{port}: {e}"),
        Err(_) => anyhow::bail!("resolve gateway {host}:{port}: dns timed out (>5s)"),
    };
    iter.next()
        .ok_or_else(|| anyhow::anyhow!("resolve gateway {host}:{port}: no addresses"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedTargetAddr {
    pub(crate) original_host: String,
    pub(crate) socket: SocketAddr,
}

/// Resolve one Peer-provided target exactly once. DNS results are sorted so
/// route selection and target authorization never depend on resolver return
/// order; callers must dial `socket` rather than resolving `original_host`
/// again.
pub(crate) async fn resolve_target_addr_once(
    address: &str,
    ipv4_only: bool,
) -> anyhow::Result<ResolvedTargetAddr> {
    if let Ok(socket) = address.parse::<SocketAddr>() {
        if ipv4_only && !socket.is_ipv4() {
            anyhow::bail!("target has no IPv4 address");
        }
        return Ok(ResolvedTargetAddr {
            original_host: socket.ip().to_string(),
            socket,
        });
    }

    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("target must be host-or-IP:port"))?;
    if host.is_empty()
        || host.contains([':', '[', ']', '/', '\\'])
        || host.chars().any(char::is_whitespace)
    {
        anyhow::bail!("target must be host-or-IP:port");
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("target must be host-or-IP:port"))?;
    let resolved = tokio::time::timeout(Duration::from_secs(5), lookup_host((host, port)))
        .await
        .map_err(|_| anyhow::anyhow!("target DNS resolution timed out"))?
        .map_err(|error| anyhow::anyhow!("target DNS resolution failed: {error}"))?;
    let mut candidates = resolved
        .filter(|candidate| !ipv4_only || candidate.is_ipv4())
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates.dedup();
    let socket = candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("target has no usable address"))?;
    Ok(ResolvedTargetAddr {
        original_host: host.to_string(),
        socket,
    })
}

fn host_port_for_resolution(raw: &str, default_port: u16) -> anyhow::Result<(String, u16)> {
    let rest = split_scheme(raw).map_or(raw, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest).trim();
    if authority.is_empty() {
        anyhow::bail!("gateway endpoint is empty");
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("invalid bracketed IPv6 gateway {authority:?}"))?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Ok((host.to_string(), port));
    }
    if authority.matches(':').count() == 1 {
        if let Some((host, port)) = authority.rsplit_once(':') {
            if let Ok(port) = port.parse::<u16>() {
                return Ok((host.to_string(), port));
            }
        }
    }
    Ok((authority.to_string(), default_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_transport_types() {
        assert_eq!(TransportKind::parse("").unwrap(), TransportKind::Quic);
        assert_eq!(
            TransportKind::parse("websocket").unwrap(),
            TransportKind::WebSocket
        );
        assert_eq!(TransportKind::parse("grpc").unwrap(), TransportKind::Grpc);
        assert!(TransportKind::parse("tcp").is_err());
    }

    #[test]
    fn gateway_endpoint_adds_port_without_breaking_urls_or_ipv6() {
        assert_eq!(
            gateway_endpoint("gateway.example.com", 8443),
            "gateway.example.com:8443"
        );
        assert_eq!(
            gateway_endpoint("gateway.example.com:9443", 8443),
            "gateway.example.com:9443"
        );
        assert_eq!(
            gateway_endpoint("wss://gateway.example.com/ws", 8443),
            "wss://gateway.example.com/ws"
        );
        assert_eq!(gateway_endpoint("::1", 8443), "[::1]:8443");
        assert_eq!(gateway_endpoint("[::1]:9443", 8443), "[::1]:9443");
    }

    #[test]
    fn websocket_url_normalizes_bare_endpoints() {
        assert_eq!(
            websocket_url("gateway.example.com:8443", false).unwrap(),
            "ws://gateway.example.com:8443/ws"
        );
        assert_eq!(
            websocket_url("gateway.example.com:8443", true).unwrap(),
            "wss://gateway.example.com:8443/ws"
        );
        assert_eq!(
            websocket_url("wss://gateway.example.com/", false).unwrap(),
            "wss://gateway.example.com/ws"
        );
        assert_eq!(
            websocket_url("https://gateway.example.com/tunnel", false).unwrap(),
            "wss://gateway.example.com/tunnel"
        );
    }

    #[test]
    fn grpc_url_normalizes_bare_endpoints() {
        assert_eq!(
            grpc_url("gateway.example.com:8443", false).unwrap(),
            "http://gateway.example.com:8443"
        );
        assert_eq!(
            grpc_url("gateway.example.com:8443", true).unwrap(),
            "https://gateway.example.com:8443"
        );
        assert_eq!(
            grpc_url("https://gateway.example.com:8443", false).unwrap(),
            "https://gateway.example.com:8443"
        );
    }

    #[test]
    fn tls_domain_strips_scheme_port_and_brackets() {
        assert_eq!(
            tls_domain("https://gateway.example.com:8443"),
            "gateway.example.com"
        );
        assert_eq!(
            tls_domain("gateway.example.com:8443"),
            "gateway.example.com"
        );
        assert_eq!(tls_domain("[::1]:8443"), "::1");
        assert_eq!(tls_domain("::1"), "::1");
    }

    #[test]
    fn host_port_for_resolution_accepts_host_port_and_urls() {
        assert_eq!(
            host_port_for_resolution("gateway.example.com", 8443).unwrap(),
            ("gateway.example.com".into(), 8443)
        );
        assert_eq!(
            host_port_for_resolution("gateway.example.com:9443", 8443).unwrap(),
            ("gateway.example.com".into(), 9443)
        );
        assert_eq!(
            host_port_for_resolution("wss://gateway.example.com:9443/ws", 8443).unwrap(),
            ("gateway.example.com".into(), 9443)
        );
        assert_eq!(
            host_port_for_resolution("[::1]:9443", 8443).unwrap(),
            ("::1".into(), 9443)
        );
    }
}
