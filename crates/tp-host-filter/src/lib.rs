//! Shared host-pattern filter used by the tunnel client and gateway.
//!
//! Pattern grammar:
//! * CIDR: `192.168.1.0/24`, `192.168.1.0/24:22`, `192.168.1.0/24:*`
//! * host: `example.com` (any port)
//! * host:port: `example.com:443`
//! * host wildcard: `*.example.com` or `*.example.com:443`
//! * port wildcard: `*:80`
//! * catch-all: `*` or `*:*`
//! * regex fallback when regex metacharacters are present, including `.*`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use regex::Regex;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, HostFilterError>;

#[derive(Debug, Error)]
pub enum HostFilterError {
    #[error("invalid host filter pattern {pattern:?}: {reason}")]
    InvalidPattern { pattern: String, reason: String },
}

#[derive(Debug, Clone)]
enum Pattern {
    All,
    Regex(Regex),
    Cidr { network: IpNet, port: Option<u16> },
    Host { host: String, port: Option<u16> },
    Suffix { suffix: String, port: Option<u16> },
    Port { port: u16 },
}

#[derive(Debug, Clone, Copy)]
enum IpNet {
    V4 { base: u32, bits: u8 },
    V6 { base: u128, bits: u8 },
}

impl IpNet {
    fn parse(s: &str) -> Option<Self> {
        let (addr, bits) = s.split_once('/')?;
        let bits: u8 = bits.parse().ok()?;
        if let Ok(v4) = addr.parse::<Ipv4Addr>() {
            if bits > 32 {
                return None;
            }
            let base = u32::from(v4) & mask32(bits);
            return Some(IpNet::V4 { base, bits });
        }
        if let Ok(v6) = addr.parse::<Ipv6Addr>() {
            if bits > 128 {
                return None;
            }
            let base = u128::from(v6) & mask128(bits);
            return Some(IpNet::V6 { base, bits });
        }
        None
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (IpNet::V4 { base, bits }, IpAddr::V4(v4)) => (u32::from(v4) & mask32(*bits)) == *base,
            (IpNet::V6 { base, bits }, IpAddr::V6(v6)) => {
                (u128::from(v6) & mask128(*bits)) == *base
            }
            _ => false,
        }
    }
}

fn mask32(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        (!0u32).wrapping_shl((32 - bits) as u32)
    }
}

fn mask128(bits: u8) -> u128 {
    if bits == 0 {
        0
    } else {
        (!0u128).wrapping_shl((128 - bits) as u32)
    }
}

#[derive(Debug, Clone)]
pub struct HostFilter {
    forbidden: Vec<Pattern>,
    allowed: Vec<Pattern>,
}

impl HostFilter {
    pub fn new(forbidden: &[String], allowed: &[String]) -> Result<Self> {
        Self::new_with_defaults(&[], forbidden, allowed)
    }

    pub fn new_with_defaults(
        default_forbidden: &[&str],
        forbidden: &[String],
        allowed: &[String],
    ) -> Result<Self> {
        let mut compiled_forbidden = Vec::with_capacity(default_forbidden.len() + forbidden.len());
        for pattern in default_forbidden {
            compiled_forbidden.push(compile_pattern(pattern)?);
        }
        for pattern in forbidden {
            compiled_forbidden.push(compile_pattern(pattern)?);
        }

        let mut compiled_allowed = Vec::with_capacity(allowed.len());
        for pattern in allowed {
            compiled_allowed.push(compile_pattern(pattern)?);
        }

        Ok(Self {
            forbidden: compiled_forbidden,
            allowed: compiled_allowed,
        })
    }

    pub fn is_allowed(&self, address: &str) -> bool {
        let Some(target) = Target::parse(address) else {
            return false;
        };

        if self.forbidden.iter().any(|p| matches(p, &target)) {
            return false;
        }
        if self.allowed.is_empty() {
            return true;
        }
        self.allowed.iter().any(|p| matches(p, &target))
    }
}

#[derive(Debug)]
struct Target<'a> {
    address: &'a str,
    host: String,
    port: Option<u16>,
}

impl<'a> Target<'a> {
    fn parse(address: &'a str) -> Option<Self> {
        let address = address.trim();
        if address.is_empty() {
            return None;
        }

        if let Some(rest) = address.strip_prefix('[') {
            if let Some(end) = rest.find(']') {
                let host = rest[..end].to_ascii_lowercase();
                let port = rest[end + 1..]
                    .strip_prefix(':')
                    .and_then(|port| port.parse::<u16>().ok());
                return Some(Self {
                    address,
                    host,
                    port,
                });
            }
        }

        if let Some((host, port)) = address.rsplit_once(':') {
            if !host.is_empty() {
                if let Ok(port) = port.parse::<u16>() {
                    return Some(Self {
                        address,
                        host: host.to_ascii_lowercase(),
                        port: Some(port),
                    });
                }
            }
        }

        Some(Self {
            address,
            host: address.to_ascii_lowercase(),
            port: None,
        })
    }
}

fn matches(pattern: &Pattern, target: &Target<'_>) -> bool {
    match pattern {
        Pattern::All => true,
        Pattern::Regex(re) => re.is_match(target.address),
        Pattern::Cidr { network, port } => match target.host.parse::<IpAddr>() {
            Ok(ip) => network.contains(ip) && port_matches(*port, target.port),
            Err(_) => false,
        },
        Pattern::Host { host, port } => target.host == *host && port_matches(*port, target.port),
        Pattern::Suffix { suffix, port } => {
            (target.host == *suffix || target.host.ends_with(&format!(".{suffix}")))
                && port_matches(*port, target.port)
        }
        Pattern::Port { port } => target.port == Some(*port),
    }
}

fn port_matches(want: Option<u16>, got: Option<u16>) -> bool {
    want.is_none_or(|port| got == Some(port))
}

fn compile_pattern(pattern: &str) -> Result<Pattern> {
    let trimmed = pattern.trim();
    compile(trimmed).map_err(|reason| HostFilterError::InvalidPattern {
        pattern: pattern.to_string(),
        reason,
    })
}

fn compile(pattern: &str) -> std::result::Result<Pattern, String> {
    if pattern.is_empty() || pattern == "*" || pattern == "*:*" {
        return Ok(Pattern::All);
    }
    if is_regex_pattern(pattern) {
        return Regex::new(pattern)
            .map(Pattern::Regex)
            .map_err(|e| e.to_string());
    }
    if pattern.contains('/') {
        return compile_cidr(pattern);
    }
    if pattern.contains('*') {
        return compile_wildcard(pattern);
    }
    if let Some((host, port)) = split_pattern_host_port(pattern) {
        return Ok(Pattern::Host {
            host: host.to_ascii_lowercase(),
            port,
        });
    }
    Ok(Pattern::Host {
        host: pattern.to_ascii_lowercase(),
        port: None,
    })
}

fn is_regex_pattern(s: &str) -> bool {
    const META: &[char] = &['^', '$', '[', ']', '(', ')', '{', '}', '+', '?', '|', '\\'];
    s.contains(META) || s.contains(".*")
}

fn compile_cidr(pattern: &str) -> std::result::Result<Pattern, String> {
    if let Some((prefix, port)) = split_cidr_port(pattern) {
        let network = IpNet::parse(prefix).ok_or_else(|| format!("invalid CIDR: {pattern}"))?;
        return Ok(Pattern::Cidr { network, port });
    }

    let network = IpNet::parse(pattern).ok_or_else(|| format!("invalid CIDR: {pattern}"))?;
    Ok(Pattern::Cidr {
        network,
        port: None,
    })
}

fn split_cidr_port(pattern: &str) -> Option<(&str, Option<u16>)> {
    let (prefix, port) = pattern.rsplit_once(':')?;
    if !prefix.contains('/') {
        return None;
    }
    if port == "*" {
        return Some((prefix, None));
    }
    port.parse::<u16>().ok().map(|port| (prefix, Some(port)))
}

fn compile_wildcard(pattern: &str) -> std::result::Result<Pattern, String> {
    if let Some(port) = pattern.strip_prefix("*:") {
        if port == "*" {
            return Ok(Pattern::All);
        }
        return port
            .parse::<u16>()
            .map(|port| Pattern::Port { port })
            .map_err(|e| e.to_string());
    }

    if let Some((host, port)) = split_pattern_host_port(pattern) {
        if let Some(suffix) = host.strip_prefix("*.") {
            return Ok(Pattern::Suffix {
                suffix: suffix.to_ascii_lowercase(),
                port,
            });
        }
        if !host.contains('*') && port.is_none() {
            return Ok(Pattern::Host {
                host: host.to_ascii_lowercase(),
                port: None,
            });
        }
    }

    if let Some(suffix) = pattern.strip_prefix("*.") {
        return Ok(Pattern::Suffix {
            suffix: suffix.to_ascii_lowercase(),
            port: None,
        });
    }

    let escaped = regex::escape(pattern).replace("\\*", ".*");
    Regex::new(&format!("^{escaped}$"))
        .map(Pattern::Regex)
        .map_err(|e| e.to_string())
}

fn split_pattern_host_port(pattern: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = pattern.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    if port == "*" {
        return Some((host, None));
    }
    port.parse::<u16>().ok().map(|port| (host, Some(port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_then_allow() {
        let f = HostFilter::new(
            &["metadata.internal:*".into()],
            &["*.example.com:443".into(), "10.0.0.0/8".into()],
        )
        .unwrap();
        assert!(!f.is_allowed("metadata.internal:80"));
        assert!(f.is_allowed("api.example.com:443"));
        assert!(!f.is_allowed("api.example.com:80"));
        assert!(f.is_allowed("10.1.2.3:22"));
        assert!(!f.is_allowed("8.8.8.8:22"));
    }

    #[test]
    fn wildcard_port() {
        let f = HostFilter::new(&[], &["*:80".into()]).unwrap();
        assert!(f.is_allowed("foo.com:80"));
        assert!(!f.is_allowed("foo.com:81"));
    }

    #[test]
    fn wildcard_host_without_port_matches_any_port() {
        let f = HostFilter::new(&["*.evil.example".into()], &[]).unwrap();
        assert!(!f.is_allowed("api.evil.example:443"));
        assert!(!f.is_allowed("nested.api.evil.example:80"));
        assert!(!f.is_allowed("evil.example:443"));
        assert!(f.is_allowed("evil.examplexx:443"));
    }

    #[test]
    fn regex_fallback_matches_full_target() {
        let f = HostFilter::new(&[], &["^api-[0-9]+\\.example\\.com:443$".into()]).unwrap();
        assert!(f.is_allowed("api-42.example.com:443"));
        assert!(!f.is_allowed("api-dev.example.com:443"));
        assert!(!f.is_allowed("api-42.example.com:80"));
    }

    #[test]
    fn dot_star_is_treated_as_regex_catch_all() {
        let f = HostFilter::new(&[], &[".*".into()]).unwrap();
        assert!(f.is_allowed("api.example.com:443"));
    }

    #[test]
    fn no_allowed_means_allow_all_non_forbidden() {
        let f = HostFilter::new(&["bad.com:*".into()], &[]).unwrap();
        assert!(f.is_allowed("good.com:443"));
        assert!(!f.is_allowed("bad.com:443"));
    }

    #[test]
    fn invalid_regex_returns_compile_error() {
        let err = HostFilter::new(&[], &["^api-(".into()]).expect_err("invalid regex");
        assert!(err.to_string().contains("invalid host filter pattern"));
    }
}
