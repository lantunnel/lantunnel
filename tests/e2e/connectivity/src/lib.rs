//! Phase 1 stateless connectivity probes for Lantunnel V2.
//!
//! Test bodies live in `tests/` modules and are invoked by name from `main.rs`.
//! They connect to an already-running Client's loopback-only, no-auth SOCKS5
//! listener and never start product processes or select an implementation.
//!
//! Shape note (this crate is the Phase 1 shape-finder for 9 sibling tasks):
//!   * `socks5::connect_no_auth` — minimal RFC-1928 client. Reused by every SOCKS5
//!     task (`socks5_*`).
//!   * `parse_host_port` — tolerates either `host:port` or `[v6]:port`. Reused
//!     by every test that takes a `--target` flag.

use anyhow::{anyhow, Context, Result};

pub mod socks5;
pub mod socks5_udp;
pub mod tests;

/// Parse `host:port` (or `[v6]:port`) into its components. We split on the
/// last `:` so IPv6 literals containing colons inside brackets still work.
pub fn parse_host_port(s: &str) -> Result<(String, u16)> {
    let (host_raw, port_raw) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("missing ':' in host:port: {s}"))?;
    if host_raw.is_empty() {
        return Err(anyhow!("empty host in: {s}"));
    }
    // Strip surrounding brackets on `[v6]` literals so the inner address is
    // what's used for SOCKS5 DOMAIN ATYP.
    let host = host_raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host_raw)
        .to_string();
    let port: u16 = port_raw
        .parse()
        .with_context(|| format!("invalid port {port_raw:?} in {s}"))?;
    Ok((host, port))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod parse_tests {
    use super::parse_host_port;

    #[test]
    fn parses_host_and_port() {
        assert_eq!(
            parse_host_port("127.0.0.1:18999").unwrap(),
            ("127.0.0.1".into(), 18999)
        );
    }

    #[test]
    fn parses_dns_name() {
        assert_eq!(
            parse_host_port("example.com:443").unwrap(),
            ("example.com".into(), 443)
        );
    }

    #[test]
    fn parses_ipv6_bracketed() {
        assert_eq!(parse_host_port("[::1]:8080").unwrap(), ("::1".into(), 8080));
    }

    #[test]
    fn rejects_missing_port() {
        assert!(parse_host_port("127.0.0.1").is_err());
    }

    #[test]
    fn rejects_empty_host() {
        assert!(parse_host_port(":8080").is_err());
    }

    #[test]
    fn rejects_bad_port() {
        assert!(parse_host_port("127.0.0.1:not-a-port").is_err());
    }
}
