use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A structural bound on one privileged helper request, not a plan ceiling.
///
/// The tier table this replaced is gone: the product enforces no route
/// ceiling, and none should be reintroduced. What remains guards how much a
/// single helper call may ask for.
///
/// TODO: nothing checks it yet, so a helper request is currently unbounded.
#[allow(dead_code)]
pub const MAX_HELPER_LAN_ROUTES: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopNetworkMode {
    #[default]
    Socks5Only,
    LanRoutesTun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLanRoutes {
    pub routes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanRouteSpec {
    pub cidr: String,
    pub network: String,
    pub prefix: u8,
    pub netmask: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayRouteValidationError {
    EmptyRoute { index: usize },
    InvalidCidr(String),
    InvalidIpv4(String),
    NotExactHost(String),
    OutsideOverlayPool(String),
    ReservedOverlayHost(String),
    DuplicateRoute(String),
}

impl fmt::Display for OverlayRouteValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoute { index } => write!(f, "Overlay route at index {index} is empty"),
            Self::InvalidCidr(route) => write!(f, "{route} is not an IPv4 CIDR"),
            Self::InvalidIpv4(route) => write!(f, "{route} is not a valid IPv4 address"),
            Self::NotExactHost(route) => write!(f, "{route} must be an exact /32 Overlay host"),
            Self::OutsideOverlayPool(route) => {
                write!(f, "{route} is outside the 198.18.0.0/16 Overlay pool")
            }
            Self::ReservedOverlayHost(route) => {
                write!(f, "{route} is reserved and cannot be an Overlay lease")
            }
            Self::DuplicateRoute(route) => write!(f, "{route} is duplicated"),
        }
    }
}

impl std::error::Error for OverlayRouteValidationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanRouteValidationError {
    TooManyRoutes { count: usize, limit: usize },
    EmptyRoute { index: usize },
    InvalidCidr(String),
    InvalidIpv4(String),
    InvalidPrefix(String),
    PublicRoute(String),
    DuplicateRoute(String),
}

impl fmt::Display for LanRouteValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyRoutes { count, limit } => write!(
                f,
                "at most {} network{} can be routed at once, got {}",
                limit,
                if *limit == 1 { "" } else { "s" },
                count
            ),
            Self::EmptyRoute { index } => write!(f, "route at index {index} is empty"),
            Self::InvalidCidr(route) => write!(f, "{route} is not an IPv4 CIDR"),
            Self::InvalidIpv4(route) => write!(f, "{route} is not a valid IPv4 address"),
            Self::InvalidPrefix(route) => {
                write!(f, "{route} must use a prefix from 0 through 32")
            }
            Self::PublicRoute(route) => write!(
                f,
                "{route} must be fully contained in a private IPv4 LAN or link-local range"
            ),
            Self::DuplicateRoute(route) => write!(f, "{route} is duplicated"),
        }
    }
}

impl std::error::Error for LanRouteValidationError {}

pub fn validate_lan_routes(
    routes: &[String],
    max_routes: usize,
) -> Result<ValidatedLanRoutes, LanRouteValidationError> {
    let specs = lan_route_specs(routes, max_routes)?;
    Ok(ValidatedLanRoutes {
        routes: specs.into_iter().map(|route| route.cidr).collect(),
    })
}

pub fn lan_route_specs(
    routes: &[String],
    max_routes: usize,
) -> Result<Vec<LanRouteSpec>, LanRouteValidationError> {
    if routes.len() > max_routes {
        return Err(LanRouteValidationError::TooManyRoutes {
            count: routes.len(),
            limit: max_routes,
        });
    }

    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(routes.len());
    for (index, route) in routes.iter().enumerate() {
        let cidr = Ipv4Cidr::parse(route, index)?;
        let route = cidr.normalized_description();
        if !seen.insert(route.clone()) {
            return Err(LanRouteValidationError::DuplicateRoute(route));
        }
        normalized.push(cidr.route_spec());
    }

    Ok(normalized)
}

/// Validate Platform-derived remote Overlay host routes independently from the
/// user LAN-route entitlement. The parent pool is never accepted as a route.
pub fn overlay_route_specs(
    routes: &[String],
) -> Result<Vec<LanRouteSpec>, OverlayRouteValidationError> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(routes.len());
    for (index, raw_route) in routes.iter().enumerate() {
        let route = raw_route.trim();
        if route.is_empty() {
            return Err(OverlayRouteValidationError::EmptyRoute { index });
        }
        let Some((raw_address, raw_prefix)) = route.split_once('/') else {
            return Err(OverlayRouteValidationError::InvalidCidr(route.to_string()));
        };
        if raw_prefix != "32" {
            return Err(OverlayRouteValidationError::NotExactHost(route.to_string()));
        }
        let address = parse_ipv4(raw_address, route)
            .map_err(|_| OverlayRouteValidationError::InvalidIpv4(route.to_string()))?;
        if address >> 16 != u32::from_be_bytes([198, 18, 0, 0]) >> 16 {
            return Err(OverlayRouteValidationError::OutsideOverlayPool(
                route.to_string(),
            ));
        }
        if matches!(address & 0xffff, 0 | 0xffff) {
            return Err(OverlayRouteValidationError::ReservedOverlayHost(
                route.to_string(),
            ));
        }
        let cidr = format!("{}/32", format_ipv4(address));
        if !seen.insert(cidr.clone()) {
            return Err(OverlayRouteValidationError::DuplicateRoute(cidr));
        }
        normalized.push(LanRouteSpec {
            network: format_ipv4(address),
            cidr,
            prefix: 32,
            netmask: format_ipv4(u32::MAX),
        });
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv4Cidr {
    network: u32,
    prefix: u8,
}

impl Ipv4Cidr {
    fn parse(raw_route: &str, index: usize) -> Result<Self, LanRouteValidationError> {
        let route = raw_route.trim();
        if route.is_empty() {
            return Err(LanRouteValidationError::EmptyRoute { index });
        }

        let Some((raw_address, raw_prefix)) = route.split_once('/') else {
            return Err(LanRouteValidationError::InvalidCidr(route.to_string()));
        };
        let address = parse_ipv4(raw_address, route)?;
        let prefix = raw_prefix
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= 32)
            .ok_or_else(|| LanRouteValidationError::InvalidPrefix(route.to_string()))?;

        let mask = ipv4_mask(prefix);
        let network = address & mask;
        let broadcast = network | !mask;
        if !is_private_lan(network, broadcast) {
            return Err(LanRouteValidationError::PublicRoute(route.to_string()));
        }

        Ok(Self { network, prefix })
    }

    fn normalized_description(&self) -> String {
        format!("{}/{}", format_ipv4(self.network), self.prefix)
    }

    fn route_spec(&self) -> LanRouteSpec {
        LanRouteSpec {
            cidr: self.normalized_description(),
            network: format_ipv4(self.network),
            prefix: self.prefix,
            netmask: format_ipv4(ipv4_mask(self.prefix)),
        }
    }
}

fn parse_ipv4(raw_address: &str, route: &str) -> Result<u32, LanRouteValidationError> {
    let octets: Vec<&str> = raw_address.split('.').collect();
    if octets.len() != 4 {
        return Err(LanRouteValidationError::InvalidIpv4(route.to_string()));
    }

    let mut address = 0u32;
    for octet in octets {
        let value = octet
            .parse::<u8>()
            .map_err(|_| LanRouteValidationError::InvalidIpv4(route.to_string()))?;
        address = (address << 8) | u32::from(value);
    }
    Ok(address)
}

fn ipv4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn is_private_lan(network: u32, broadcast: u32) -> bool {
    private_lan_ranges()
        .iter()
        .any(|range| range.contains(&network) && range.contains(&broadcast))
}

fn private_lan_ranges() -> [std::ops::RangeInclusive<u32>; 4] {
    [
        make_range("10.0.0.0", 8),
        make_range("172.16.0.0", 12),
        make_range("192.168.0.0", 16),
        make_range("169.254.0.0", 16),
    ]
}

fn make_range(address: &str, prefix: u8) -> std::ops::RangeInclusive<u32> {
    let start =
        parse_ipv4(address, address).expect("static route range must parse") & ipv4_mask(prefix);
    let end = start | !ipv4_mask(prefix);
    start..=end
}

fn format_ipv4(address: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (address >> 24) & 0xff,
        (address >> 16) & 0xff,
        (address >> 8) & 0xff,
        address & 0xff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_routes_accept_exact_remote_hosts_without_a_lan_quota() {
        let routes = overlay_route_specs(&["198.18.7.23/32".into(), "198.18.200.9/32".into()])
            .expect("valid Overlay hosts");

        assert_eq!(
            routes
                .iter()
                .map(|route| route.cidr.as_str())
                .collect::<Vec<_>>(),
            vec!["198.18.7.23/32", "198.18.200.9/32"]
        );
    }

    #[test]
    fn overlay_routes_reject_the_reserved_pool_edges() {
        for route in ["198.18.0.0/32", "198.18.255.255/32"] {
            assert!(
                overlay_route_specs(&[route.into()]).is_err(),
                "{route} is not a Platform Overlay lease"
            );
        }
    }

    #[test]
    fn overlay_routes_reject_non_host_and_non_pool_prefixes() {
        for route in ["198.18.0.0/16", "198.19.7.23/32", "10.0.0.7/32"] {
            assert!(
                overlay_route_specs(&[route.into()]).is_err(),
                "{route} must never become an Overlay OS route"
            );
        }
    }
}
