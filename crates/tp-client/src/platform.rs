//! Internal runtime configuration derived from a verified V2 Peer profile.

use serde::{Deserialize, Serialize};
use tp_core::config::{default_transport_type, ClientLocalProxyConfig};

/// Connection facts consumed by the transport runtime.
///
/// This is not a Platform wire response. Managed Peers resolve their Gateway
/// through `managed_resolve`; static Peers carry the same facts in their
/// signed profile. Keeping one internal shape lets both sources share the
/// transport implementation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TunnelConfig {
    #[serde(default)]
    pub tunnel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_name: Option<String>,
    pub gateway_addr: String,
    pub gateway_port: u16,
    #[serde(default = "default_transport_type")]
    pub transport_type: String,
    #[serde(default)]
    pub tls_cert: String,
    /// Stable logical Peer identity. In transport protocol v4 this is the
    /// canonical Replica family ID (`...-0`).
    #[serde(default)]
    pub peer_id: String,
    /// Platform-authoritative Tunnel-scoped Overlay IPv4 `/32`.
    #[serde(default)]
    pub overlay_ipv4: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_ids: Vec<String>,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub forbidden_hosts: Vec<String>,
    #[serde(default)]
    pub local_proxy: ClientLocalProxyConfig,
    /// Local-only source URL for diagnostics.
    #[serde(default, skip)]
    pub platform_base_url: Option<String>,
}

fn default_replicas() -> u32 {
    1
}

#[cfg(test)]
pub(crate) fn normalize_local_lan_ipv4s(
    candidates: impl IntoIterator<Item = std::net::Ipv4Addr>,
) -> Vec<String> {
    let mut addresses = candidates
        .into_iter()
        .filter(|address| {
            let numeric = u32::from(*address);
            !address.is_loopback()
                && ((0x0a00_0000..=0x0aff_ffff).contains(&numeric)
                    || (0xac10_0000..=0xac1f_ffff).contains(&numeric)
                    || (0xc0a8_0000..=0xc0a8_ffff).contains(&numeric))
        })
        .collect::<Vec<_>>();
    addresses.sort_unstable_by_key(|address| u32::from(*address));
    addresses.dedup();
    addresses.truncate(crate::route_matcher::MAX_LAN_ALIASES_PER_PEER);
    addresses
        .into_iter()
        .map(|address| address.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn runtime_config_transport_defaults_to_quic() {
        let config: TunnelConfig = serde_json::from_value(serde_json::json!({
            "gateway_addr": "gateway.example.com",
            "gateway_port": 8443
        }))
        .expect("runtime config");

        assert_eq!(config.transport_type, tp_core::config::TRANSPORT_TYPE_QUIC);
    }

    #[test]
    fn local_lan_candidates_keep_only_bounded_rfc1918_addresses() {
        assert_eq!(
            normalize_local_lan_ipv4s([
                Ipv4Addr::new(192, 168, 10, 20),
                Ipv4Addr::new(10, 20, 0, 3),
                Ipv4Addr::LOCALHOST,
                Ipv4Addr::new(172, 31, 255, 254),
                Ipv4Addr::new(8, 8, 8, 8),
            ]),
            vec!["10.20.0.3", "172.31.255.254", "192.168.10.20"]
        );
    }
}
