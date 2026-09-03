//! The single Lantunnel 2.0 target-side access policy.
//!
//! This is deliberately a small adapter around target matching. It does not
//! select a source Peer, a route, or a transport, and it has no policy version.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tp_core::Protocol;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessActionV2 {
    Allow,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ClientAccessTargetV2 {
    ThisPeer,
    Ip(IpAddr),
    Cidr(String),
    Host(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ClientAccessPortV2 {
    Any,
    Exact(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAccessProtocolV2 {
    Tcp,
    Udp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAccessRuleV2 {
    pub target: ClientAccessTargetV2,
    pub protocol: ClientAccessProtocolV2,
    pub port: ClientAccessPortV2,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAccessPolicyV2 {
    /// Empty means every Peer in the Tunnel may reach this Client. Once it
    /// names anything, it is the only way in.
    ///
    /// This replaced a separate `default_action` selector, which asked a second
    /// question — open or closed — that the list itself already answers, and
    /// which could disagree with it.
    #[serde(default)]
    pub allow: Vec<ClientAccessRuleV2>,
    /// Always checked first. A refusal here is never overridden by Allow.
    #[serde(default)]
    pub deny: Vec<ClientAccessRuleV2>,
}

impl ClientAccessPolicyV2 {
    /// The policy that refuses everything.
    ///
    /// An empty Allow list means open, so a closed Client is spelled out rather
    /// than implied: a Deny rule covering every address, for both protocols and
    /// both families. That is the same shape the UI writes when the owner asks
    /// to block all incoming traffic, so what is saved matches what was asked.
    pub fn closed() -> Self {
        let everything = |cidr: &str, protocol: ClientAccessProtocolV2| ClientAccessRuleV2 {
            target: ClientAccessTargetV2::Cidr(cidr.to_owned()),
            protocol,
            port: ClientAccessPortV2::Any,
        };
        Self {
            allow: Vec::new(),
            deny: vec![
                everything("0.0.0.0/0", ClientAccessProtocolV2::Tcp),
                everything("0.0.0.0/0", ClientAccessProtocolV2::Udp),
                everything("::/0", ClientAccessProtocolV2::Tcp),
                everything("::/0", ClientAccessProtocolV2::Udp),
            ],
        }
    }

    /// True when this policy refuses every address on both protocols.
    pub fn is_closed(&self) -> bool {
        // Both families, per protocol. Accepting either meant a policy that
        // denied only the IPv4 catch-all reported closed, so the Client said
        // nothing was reachable while every IPv6 destination still was.
        let covers = |protocol: ClientAccessProtocolV2, family: &str| {
            self.deny.iter().any(|rule| {
                rule.protocol == protocol
                    && matches!(rule.port, ClientAccessPortV2::Any)
                    && matches!(&rule.target, ClientAccessTargetV2::Cidr(cidr) if cidr == family)
            })
        };
        [ClientAccessProtocolV2::Tcp, ClientAccessProtocolV2::Udp]
            .into_iter()
            .all(|protocol| covers(protocol, "0.0.0.0/0") && covers(protocol, "::/0"))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClientAccessPolicyErrorV2 {
    #[error("invalid Client access rule at {list}[{index}]: {reason}")]
    InvalidRule {
        list: &'static str,
        index: usize,
        reason: &'static str,
    },
}

#[derive(Clone, Debug)]
pub struct CompiledClientAccessPolicyV2 {
    /// True once the Allow list names a destination other than this Peer, which
    /// is what turns the list into a whitelist. A `ThisPeer` mapping is a
    /// capability rather than a gate: exposing a local port must not silently
    /// close every other destination.
    allow_is_gate: bool,
    allow: Vec<CompiledRuleV2>,
    deny: Vec<CompiledRuleV2>,
}

#[derive(Clone, Debug)]
struct CompiledRuleV2 {
    target: CompiledTargetV2,
    protocol: ClientAccessProtocolV2,
    port: ClientAccessPortV2,
}

#[derive(Clone, Debug)]
enum CompiledTargetV2 {
    ThisPeer,
    Ip(IpAddr),
    Cidr(IpCidrV2),
    Host(HostPatternV2),
}

#[derive(Clone, Debug)]
enum HostPatternV2 {
    Exact(String),
    Suffix(String),
}

#[derive(Clone, Copy, Debug)]
enum IpCidrV2 {
    V4 { base: u32, bits: u8 },
    V6 { base: u128, bits: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientAccessTargetClassV2<'a> {
    ThisPeer { own_overlay: Ipv4Addr },
    Other { requested_host: &'a str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientAccessDecisionV2 {
    AllowDirect,
    AllowThisPeer { final_target: String },
    Deny,
}

impl CompiledClientAccessPolicyV2 {
    pub fn compile(policy: &ClientAccessPolicyV2) -> Result<Self, ClientAccessPolicyErrorV2> {
        let allow = compile_rules("allow", &policy.allow)?;
        Ok(Self {
            allow_is_gate: allow
                .iter()
                .any(|rule| !matches!(rule.target, CompiledTargetV2::ThisPeer)),
            allow,
            deny: compile_rules("deny", &policy.deny)?,
        })
    }

    /// Safe startup fallback: refuses everything.
    ///
    /// Used before any policy has been read, and when a saved one fails to
    /// compile. It cannot be spelled as a `ClientAccessPolicyV2` — an empty
    /// Allow list means open — so it is built directly, with a gate that
    /// nothing can satisfy.
    pub fn deny_all() -> Self {
        Self {
            allow_is_gate: true,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }

    pub fn decide(
        &self,
        class: ClientAccessTargetClassV2<'_>,
        protocol: Protocol,
        requested: SocketAddr,
    ) -> ClientAccessDecisionV2 {
        if self
            .deny
            .iter()
            .any(|rule| rule.matches(class, protocol, requested))
        {
            return ClientAccessDecisionV2::Deny;
        }

        let matching_allow = self
            .allow
            .iter()
            .find(|rule| rule.matches(class, protocol, requested));
        if self.allow_is_gate && matching_allow.is_none() {
            return ClientAccessDecisionV2::Deny;
        }

        match class {
            ClientAccessTargetClassV2::ThisPeer { own_overlay }
                if requested.ip() == IpAddr::V4(own_overlay) =>
            {
                // A ThisPeer mapping is itself an Allow capability. Default
                // Allow never silently creates a local-service mapping.
                let Some(rule) =
                    matching_allow.filter(|rule| matches!(rule.target, CompiledTargetV2::ThisPeer))
                else {
                    return ClientAccessDecisionV2::Deny;
                };
                let _ = rule;
                let final_target = format!("127.0.0.1:{}", requested.port());
                ClientAccessDecisionV2::AllowThisPeer { final_target }
            }
            _ => ClientAccessDecisionV2::AllowDirect,
        }
    }

    /// A `ThisPeer` mapping already carries the Allow capability for its
    /// exact requested/final pair. Once the final hostname has been resolved,
    /// only Deny rules are evaluated here; the final address never enters the
    /// independently requestable Allow set.
    pub fn mapped_final_allowed(
        &self,
        protocol: Protocol,
        final_host: &str,
        final_target: SocketAddr,
    ) -> bool {
        let class = ClientAccessTargetClassV2::Other {
            requested_host: final_host,
        };
        !self
            .deny
            .iter()
            .any(|rule| rule.matches(class, protocol, final_target))
    }
}

fn compile_rules(
    list: &'static str,
    rules: &[ClientAccessRuleV2],
) -> Result<Vec<CompiledRuleV2>, ClientAccessPolicyErrorV2> {
    rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            if matches!(rule.port, ClientAccessPortV2::Exact(0)) {
                return Err(invalid_rule(list, index, "port must be non-zero"));
            }
            Ok(CompiledRuleV2 {
                target: compile_target(list, index, &rule.target)?,
                protocol: rule.protocol,
                port: rule.port,
            })
        })
        .collect()
}

fn compile_target(
    list: &'static str,
    index: usize,
    target: &ClientAccessTargetV2,
) -> Result<CompiledTargetV2, ClientAccessPolicyErrorV2> {
    Ok(match target {
        ClientAccessTargetV2::ThisPeer => CompiledTargetV2::ThisPeer,
        ClientAccessTargetV2::Ip(ip) => CompiledTargetV2::Ip(*ip),
        ClientAccessTargetV2::Cidr(cidr) => CompiledTargetV2::Cidr(
            IpCidrV2::parse(cidr).ok_or_else(|| invalid_rule(list, index, "invalid CIDR"))?,
        ),
        ClientAccessTargetV2::Host(host) => {
            let host = host.trim().to_ascii_lowercase();
            if host.is_empty()
                || host.contains(['/', ':', '\\'])
                || host.chars().any(|ch| ch.is_whitespace())
            {
                return Err(invalid_rule(list, index, "invalid host pattern"));
            }
            if let Some(suffix) = host.strip_prefix("*.") {
                if suffix.is_empty() || suffix.contains('*') {
                    return Err(invalid_rule(list, index, "invalid host suffix"));
                }
                CompiledTargetV2::Host(HostPatternV2::Suffix(suffix.into()))
            } else {
                if host.contains('*') {
                    return Err(invalid_rule(
                        list,
                        index,
                        "only *.suffix wildcard is allowed",
                    ));
                }
                CompiledTargetV2::Host(HostPatternV2::Exact(host))
            }
        }
    })
}

fn invalid_rule(
    list: &'static str,
    index: usize,
    reason: &'static str,
) -> ClientAccessPolicyErrorV2 {
    ClientAccessPolicyErrorV2::InvalidRule {
        list,
        index,
        reason,
    }
}

impl CompiledRuleV2 {
    fn matches(
        &self,
        class: ClientAccessTargetClassV2<'_>,
        protocol: Protocol,
        requested: SocketAddr,
    ) -> bool {
        if self.protocol != ClientAccessProtocolV2::from(protocol)
            || !matches!(self.port, ClientAccessPortV2::Any)
                && !matches!(self.port, ClientAccessPortV2::Exact(port) if port == requested.port())
        {
            return false;
        }
        match (&self.target, class) {
            (CompiledTargetV2::ThisPeer, ClientAccessTargetClassV2::ThisPeer { own_overlay }) => {
                requested.ip() == IpAddr::V4(own_overlay)
            }
            (CompiledTargetV2::Ip(expected), _) => requested.ip() == *expected,
            (CompiledTargetV2::Cidr(cidr), _) => cidr.contains(requested.ip()),
            (
                CompiledTargetV2::Host(pattern),
                ClientAccessTargetClassV2::Other { requested_host },
            ) => pattern.matches(requested_host),
            _ => false,
        }
    }
}

impl From<Protocol> for ClientAccessProtocolV2 {
    fn from(value: Protocol) -> Self {
        match value {
            Protocol::Tcp => Self::Tcp,
            Protocol::Udp => Self::Udp,
        }
    }
}

impl HostPatternV2 {
    fn matches(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        match self {
            Self::Exact(expected) => host == *expected,
            Self::Suffix(suffix) => host == *suffix || host.ends_with(&format!(".{suffix}")),
        }
    }
}

impl IpCidrV2 {
    fn parse(value: &str) -> Option<Self> {
        let (address, bits) = value.split_once('/')?;
        let bits = bits.parse::<u8>().ok()?;
        if let Ok(ip) = address.parse::<Ipv4Addr>() {
            if bits > 32 {
                return None;
            }
            return Some(Self::V4 {
                base: u32::from(ip) & mask_u32(bits),
                bits,
            });
        }
        let ip = address.parse::<Ipv6Addr>().ok()?;
        if bits > 128 {
            return None;
        }
        Some(Self::V6 {
            base: u128::from(ip) & mask_u128(bits),
            bits,
        })
    }

    fn contains(self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Self::V4 { base, bits }, IpAddr::V4(ip)) => u32::from(ip) & mask_u32(bits) == base,
            (Self::V6 { base, bits }, IpAddr::V6(ip)) => u128::from(ip) & mask_u128(bits) == base,
            _ => false,
        }
    }
}

fn mask_u32(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    }
}

fn mask_u128(bits: u8) -> u128 {
    if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    }
}
