//! Native-TUN inventory for connected LAN conflicts and exact local network
//! infrastructure exclusions. The Peer matcher remains available to explicit
//! SOCKS callers when native capture is withheld.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, thiserror::Error)]
pub enum NativeRouteGuardError {
    #[error("native route inventory is unavailable")]
    InventoryUnavailable,
}

#[derive(Debug)]
struct NativeRouteInterface {
    local_addresses: BTreeSet<IpAddr>,
    /// `None` means the inventory source could not authoritatively describe a
    /// Gateway for this interface. It must not be treated as a known-empty set.
    gateway_addresses: Option<BTreeSet<IpAddr>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DnsInventory {
    Known(BTreeSet<IpAddr>),
    Unknown,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn parse_dns_address(value: &str) -> Option<IpAddr> {
    let value = value.trim();
    if let Ok(address) = value.parse::<IpAddr>() {
        return Some(address);
    }
    let (address, scope) = value.split_once('%')?;
    if scope.is_empty() || scope.contains('%') {
        return None;
    }
    address.parse().ok().map(IpAddr::V6)
}

#[cfg(any(target_os = "macos", test))]
fn parse_scutil_dns(output: &str) -> DnsInventory {
    let nonempty_lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if nonempty_lines.clone().next().is_some_and(|_| {
        nonempty_lines
            .clone()
            .all(|line| line == "No DNS configuration available")
    }) {
        return DnsInventory::Known(BTreeSet::new());
    }
    let mut saw_configuration = false;
    let mut nameservers = BTreeSet::new();
    for line in output.lines().map(str::trim) {
        if line.starts_with("DNS configuration") {
            saw_configuration = true;
        }
        if !line.starts_with("nameserver[") {
            continue;
        }
        let Some((_, value)) = line.split_once(':') else {
            return DnsInventory::Unknown;
        };
        let Some(address) = parse_dns_address(value) else {
            return DnsInventory::Unknown;
        };
        nameservers.insert(address);
    }
    if saw_configuration && !nameservers.is_empty() {
        DnsInventory::Known(nameservers)
    } else {
        DnsInventory::Unknown
    }
}

#[cfg(any(target_os = "macos", test))]
fn dns_inventory_from_scutil_command(
    status_success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> DnsInventory {
    let (Ok(stdout), Ok(stderr)) = (std::str::from_utf8(stdout), std::str::from_utf8(stderr))
    else {
        return DnsInventory::Unknown;
    };
    let inventory = parse_scutil_dns(&format!("{stdout}\n{stderr}"));
    if status_success
        || matches!(&inventory, DnsInventory::Known(addresses) if addresses.is_empty())
    {
        inventory
    } else {
        DnsInventory::Unknown
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn parse_resolv_conf(contents: &str) -> DnsInventory {
    let mut nameservers = BTreeSet::new();
    for line in contents.lines() {
        let line = line.split(['#', ';']).next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(value) = fields.next() else {
            return DnsInventory::Unknown;
        };
        if fields.next().is_some() {
            return DnsInventory::Unknown;
        }
        let Some(address) = parse_dns_address(value) else {
            return DnsInventory::Unknown;
        };
        nameservers.insert(address);
    }
    DnsInventory::Known(nameservers)
}

#[cfg(any(target_os = "macos", test))]
fn merge_macos_dns_inventory(
    scutil: DnsInventory,
    resolv_conf: Option<DnsInventory>,
) -> DnsInventory {
    let DnsInventory::Known(mut nameservers) = scutil else {
        return DnsInventory::Unknown;
    };
    if let Some(DnsInventory::Known(additional)) = resolv_conf {
        nameservers.extend(additional);
    }
    DnsInventory::Known(nameservers)
}

#[cfg(any(target_os = "linux", test))]
fn discover_linux_dns_inventory_with(
    mut read: impl FnMut(&str) -> std::io::Result<String>,
) -> DnsInventory {
    const RESOLVER_PATHS: [&str; 3] = [
        "/etc/resolv.conf",
        "/run/systemd/resolve/stub-resolv.conf",
        "/run/systemd/resolve/resolv.conf",
    ];
    let mut saw_readable_source = false;
    let mut nameservers = BTreeSet::new();
    for path in RESOLVER_PATHS {
        let Ok(contents) = read(path) else {
            continue;
        };
        saw_readable_source = true;
        match parse_resolv_conf(&contents) {
            DnsInventory::Known(addresses) => nameservers.extend(addresses),
            DnsInventory::Unknown => return DnsInventory::Unknown,
        }
    }
    if saw_readable_source {
        DnsInventory::Known(nameservers)
    } else {
        DnsInventory::Unknown
    }
}

#[cfg(any(target_os = "windows", test))]
fn dns_inventory_from_netdev_snapshot(
    snapshot_available: bool,
    dns_servers: impl IntoIterator<Item = IpAddr>,
) -> DnsInventory {
    if snapshot_available {
        DnsInventory::Known(dns_servers.into_iter().collect())
    } else {
        DnsInventory::Unknown
    }
}

#[cfg(target_os = "macos")]
fn discover_dns_inventory(_interfaces: &[netdev::Interface]) -> DnsInventory {
    let Ok(output) = std::process::Command::new("/usr/sbin/scutil")
        .arg("--dns")
        .output()
    else {
        return DnsInventory::Unknown;
    };
    let scutil =
        dns_inventory_from_scutil_command(output.status.success(), &output.stdout, &output.stderr);
    let resolv_conf = std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .map(|contents| parse_resolv_conf(&contents));
    merge_macos_dns_inventory(scutil, resolv_conf)
}

#[cfg(target_os = "linux")]
fn discover_dns_inventory(_interfaces: &[netdev::Interface]) -> DnsInventory {
    discover_linux_dns_inventory_with(|path| std::fs::read_to_string(path))
}

#[cfg(target_os = "windows")]
fn discover_dns_inventory(interfaces: &[netdev::Interface]) -> DnsInventory {
    dns_inventory_from_netdev_snapshot(
        !interfaces.is_empty(),
        interfaces
            .iter()
            .flat_map(|interface| interface.dns_servers.iter().copied()),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn discover_dns_inventory(interfaces: &[netdev::Interface]) -> DnsInventory {
    let dns_servers = interfaces
        .iter()
        .flat_map(|interface| interface.dns_servers.iter().copied())
        .collect::<BTreeSet<_>>();
    if dns_servers.is_empty() {
        DnsInventory::Unknown
    } else {
        DnsInventory::Known(dns_servers)
    }
}

impl NativeRouteInterface {
    fn from_netdev(interface: &netdev::Interface) -> Self {
        let local_addresses = interface
            .ipv4
            .iter()
            .map(|network| IpAddr::V4(network.addr()))
            .chain(
                interface
                    .ipv6
                    .iter()
                    .map(|network| IpAddr::V6(network.addr())),
            )
            .collect();
        let gateway_addresses = interface.gateway.as_ref().and_then(|gateway| {
            let addresses = gateway
                .ipv4
                .iter()
                .copied()
                .map(IpAddr::V4)
                .chain(gateway.ipv6.iter().copied().map(IpAddr::V6))
                .collect::<BTreeSet<_>>();
            (!addresses.is_empty()).then_some(addresses)
        });
        Self {
            local_addresses,
            gateway_addresses,
        }
    }
}

fn collect_ipv4_exclusions(
    local_interfaces: impl IntoIterator<Item = IpAddr>,
    default_gateways: impl IntoIterator<Item = IpAddr>,
    dns_servers: impl IntoIterator<Item = IpAddr>,
    control_endpoints: impl IntoIterator<Item = IpAddr>,
) -> BTreeSet<Ipv4Addr> {
    local_interfaces
        .into_iter()
        .chain(default_gateways)
        .chain(dns_servers)
        .chain(control_endpoints)
        .filter_map(|address| match address {
            IpAddr::V4(address) => Some(address),
            IpAddr::V6(_) => None,
        })
        .collect()
}

fn collect_connected_lan_prefixes(
    networks: impl IntoIterator<Item = (Ipv4Addr, u8)>,
) -> Vec<crate::peer_runtime::LanExportPrefixV2> {
    let mut prefixes = networks
        .into_iter()
        .filter_map(|(address, prefix_len)| {
            if !(8..=32).contains(&prefix_len) {
                return None;
            }
            let mask = u32::MAX << (32 - prefix_len);
            crate::peer_runtime::LanExportPrefixV2::new(
                Ipv4Addr::from(u32::from(address) & mask),
                prefix_len,
            )
            .ok()
        })
        .collect::<Vec<_>>();
    prefixes.sort_by_key(|prefix| (u32::from(prefix.network), prefix.prefix_len));
    prefixes.dedup();
    prefixes
}

fn connected_lan_interface_is_eligible<'a>(
    labels: impl IntoIterator<Item = &'a str>,
    up: bool,
    running: bool,
    physical: bool,
    loopback: bool,
    point_to_point: bool,
) -> bool {
    up && running
        && physical
        && !loopback
        && !point_to_point
        && !labels.into_iter().any(virtual_interface_label)
}

fn virtual_interface_label(label: &str) -> bool {
    let label = label.trim().to_ascii_lowercase();
    [
        "lantun",
        "utun",
        "tun",
        "tap",
        "docker",
        "virbr",
        "veth",
        "vmnet",
        "vmenet",
        "bridge",
        "br-",
        "wg",
        "tailscale",
        "zerotier",
        "zt",
        "ppp",
        "ipsec",
        "awdl",
        "llw",
        "anpi",
    ]
    .into_iter()
    .any(|prefix| label.starts_with(prefix))
        || [
            "virtual",
            "hyper-v",
            "wintun",
            "wireguard",
            "vpn",
            "vmware",
            "parallels",
            "docker",
            "wsl",
            "tailscale",
            "zerotier",
        ]
        .into_iter()
        .any(|marker| label.contains(marker))
}

/// Return canonical RFC1918 prefixes owned by currently usable physical
/// interfaces. An empty raw snapshot is treated as inventory failure rather
/// than authoritative absence, so callers can fail closed.
pub fn discover_connected_lan_prefixes(
) -> Result<Vec<crate::peer_runtime::LanExportPrefixV2>, NativeRouteGuardError> {
    let interfaces = netdev::get_interfaces();
    if interfaces.is_empty() {
        return Err(NativeRouteGuardError::InventoryUnavailable);
    }
    Ok(collect_connected_lan_prefixes(
        interfaces
            .into_iter()
            .filter(|interface| {
                connected_lan_interface_is_eligible(
                    std::iter::once(interface.name.as_str())
                        .chain(interface.friendly_name.as_deref())
                        .chain(interface.description.as_deref()),
                    interface.is_up(),
                    interface.is_running(),
                    interface.is_physical(),
                    interface.is_loopback(),
                    interface.is_point_to_point(),
                )
            })
            .flat_map(|interface| interface.ipv4)
            .map(|network| (network.addr(), network.prefix_len())),
    ))
}

fn route_exclusions_from_inventory(
    generation_endpoints: impl IntoIterator<Item = IpAddr>,
    interfaces: &[NativeRouteInterface],
    dns_inventory: DnsInventory,
) -> Result<BTreeSet<Ipv4Addr>, NativeRouteGuardError> {
    let DnsInventory::Known(dns_servers) = dns_inventory else {
        return Err(NativeRouteGuardError::InventoryUnavailable);
    };
    let generation_endpoints = generation_endpoints.into_iter().collect::<BTreeSet<_>>();
    let selected_interfaces = interfaces
        .iter()
        .filter(|interface| !interface.local_addresses.is_disjoint(&generation_endpoints))
        .collect::<Vec<_>>();
    if selected_interfaces.is_empty()
        || selected_interfaces
            .iter()
            .any(|interface| interface.gateway_addresses.is_none())
    {
        return Err(NativeRouteGuardError::InventoryUnavailable);
    }

    Ok(collect_ipv4_exclusions(
        interfaces
            .iter()
            .flat_map(|interface| interface.local_addresses.iter().copied()),
        interfaces.iter().flat_map(|interface| {
            interface
                .gateway_addresses
                .iter()
                .flat_map(|addresses| addresses.iter().copied())
        }),
        dns_servers,
        generation_endpoints,
    ))
}

/// Discover addresses that a learned Peer-LAN alias must never override in
/// the host route table. `generation_endpoints` includes the selected
/// physical-interface addresses (used to identify the relevant inventory)
/// and the connected Gateway endpoint (protected as an exact address). Missing
/// Gateway metadata for the selected interface or an authoritative system DNS
/// inventory fails closed.
pub(crate) fn discover_native_route_exclusions(
    generation_endpoints: impl IntoIterator<Item = IpAddr>,
) -> Result<BTreeSet<Ipv4Addr>, NativeRouteGuardError> {
    let raw_interfaces = netdev::get_interfaces();
    let dns_inventory = discover_dns_inventory(&raw_interfaces);
    let interfaces = raw_interfaces
        .iter()
        .map(NativeRouteInterface::from_netdev)
        .collect::<Vec<_>>();
    route_exclusions_from_inventory(generation_endpoints, &interfaces, dns_inventory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_lan_inventory_keeps_only_canonical_rfc1918_prefixes() {
        assert_eq!(
            collect_connected_lan_prefixes([
                ("192.168.70.44".parse().unwrap(), 24),
                ("10.20.30.40".parse().unwrap(), 16),
                ("198.18.1.9".parse().unwrap(), 16),
                ("169.254.7.8".parse().unwrap(), 16),
                ("8.8.8.8".parse().unwrap(), 24),
            ]),
            vec![
                crate::peer_runtime::LanExportPrefixV2::new("10.20.0.0".parse().unwrap(), 16,)
                    .unwrap(),
                crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 24,)
                    .unwrap(),
            ]
        );
    }

    #[test]
    fn connected_lan_inventory_requires_a_live_physical_nonvirtual_interface() {
        assert!(connected_lan_interface_is_eligible(
            ["en0", "Wi-Fi", "Apple wireless adapter"],
            true,
            true,
            true,
            false,
            false,
        ));

        for (labels, up, running, physical, loopback, point_to_point) in [
            (["en0", "Wi-Fi", "adapter"], false, true, true, false, false),
            (["en0", "Wi-Fi", "adapter"], true, false, true, false, false),
            (["en0", "Wi-Fi", "adapter"], true, true, false, false, false),
            (
                ["lo0", "Loopback", "adapter"],
                true,
                true,
                true,
                true,
                false,
            ),
            (["ppp0", "VPN", "adapter"], true, true, true, false, true),
            (["utun4", "VPN", "adapter"], true, true, true, false, false),
            (
                ["docker0", "Ethernet", "adapter"],
                true,
                true,
                true,
                false,
                false,
            ),
            (
                ["eth0", "vEthernet (WSL)", "adapter"],
                true,
                true,
                true,
                false,
                false,
            ),
            (
                ["eth0", "Ethernet", "Hyper-V Virtual Ethernet Adapter"],
                true,
                true,
                true,
                false,
                false,
            ),
        ] {
            assert!(!connected_lan_interface_is_eligible(
                labels,
                up,
                running,
                physical,
                loopback,
                point_to_point,
            ));
        }
    }

    #[test]
    fn selected_underlay_without_gateway_metadata_is_not_authoritative() {
        let inventory = [NativeRouteInterface {
            local_addresses: ["192.168.240.44".parse().unwrap()].into_iter().collect(),
            gateway_addresses: None,
        }];

        let result = route_exclusions_from_inventory(
            [
                "192.168.240.44".parse().unwrap(),
                "203.0.113.10".parse().unwrap(),
            ],
            &inventory,
            DnsInventory::Known(["192.168.240.53".parse().unwrap()].into_iter().collect()),
        );

        assert!(matches!(
            result,
            Err(NativeRouteGuardError::InventoryUnavailable)
        ));
    }

    #[test]
    fn selected_underlay_without_dns_metadata_is_not_authoritative() {
        let inventory = [NativeRouteInterface {
            local_addresses: ["192.168.240.44".parse().unwrap()].into_iter().collect(),
            gateway_addresses: Some(["192.168.240.1".parse().unwrap()].into_iter().collect()),
        }];

        let result = route_exclusions_from_inventory(
            [
                "192.168.240.44".parse().unwrap(),
                "203.0.113.10".parse().unwrap(),
            ],
            &inventory,
            DnsInventory::Unknown,
        );

        assert!(matches!(
            result,
            Err(NativeRouteGuardError::InventoryUnavailable)
        ));
    }

    #[test]
    fn authoritative_system_dns_does_not_require_per_interface_dns_metadata() {
        let inventory = [NativeRouteInterface {
            local_addresses: ["192.168.241.44".parse().unwrap()].into_iter().collect(),
            gateway_addresses: Some(["192.168.241.1".parse().unwrap()].into_iter().collect()),
        }];
        let system_dns =
            DnsInventory::Known(["192.168.241.1".parse().unwrap()].into_iter().collect());

        let exclusions = route_exclusions_from_inventory(
            [
                "192.168.241.44".parse().unwrap(),
                "203.0.113.10".parse().unwrap(),
            ],
            &inventory,
            system_dns,
        )
        .unwrap();

        assert!(exclusions.contains(&"192.168.241.1".parse().unwrap()));
    }

    #[test]
    fn scutil_dns_parser_returns_all_numeric_nameservers() {
        let output = r#"
DNS configuration

resolver #1
  nameserver[0] : 192.168.241.1
  nameserver[1] : 2001:4860:4860::8888

DNS configuration (for scoped queries)

resolver #1
  nameserver[0] : 192.168.241.1
"#;

        assert_eq!(
            parse_scutil_dns(output),
            DnsInventory::Known(
                [
                    "192.168.241.1".parse().unwrap(),
                    "2001:4860:4860::8888".parse().unwrap(),
                ]
                .into_iter()
                .collect()
            )
        );
    }

    #[test]
    fn macos_dns_inventory_unions_scutil_and_resolv_nameservers() {
        let scutil = DnsInventory::Known(["192.168.241.1".parse().unwrap()].into_iter().collect());
        let resolv = parse_resolv_conf("nameserver 10.20.30.40\nnameserver 10.20.30.41\n");

        assert_eq!(
            merge_macos_dns_inventory(scutil, Some(resolv)),
            DnsInventory::Known(
                [
                    "10.20.30.40".parse().unwrap(),
                    "10.20.30.41".parse().unwrap(),
                    "192.168.241.1".parse().unwrap(),
                ]
                .into_iter()
                .collect()
            )
        );
    }

    #[test]
    fn macos_explicit_no_scutil_config_still_adds_valid_resolv_nameservers() {
        assert_eq!(
            merge_macos_dns_inventory(
                parse_scutil_dns("No DNS configuration available\n"),
                Some(parse_resolv_conf("nameserver 10.20.30.40\n")),
            ),
            DnsInventory::Known(["10.20.30.40".parse().unwrap()].into_iter().collect())
        );
    }

    #[test]
    fn macos_resolv_conf_never_masks_unknown_scutil_inventory() {
        assert_eq!(
            merge_macos_dns_inventory(
                DnsInventory::Unknown,
                Some(parse_resolv_conf("nameserver 10.20.30.40\n")),
            ),
            DnsInventory::Unknown
        );
    }

    #[test]
    fn scutil_explicit_no_configuration_is_authoritative_empty() {
        assert_eq!(
            parse_scutil_dns("No DNS configuration available\n"),
            DnsInventory::Known(BTreeSet::new())
        );
    }

    #[test]
    fn scutil_dns_parser_accepts_scoped_ipv6_nameserver() {
        assert_eq!(
            parse_scutil_dns("DNS configuration\nresolver #1\n  nameserver[0] : fe80::1%en0\n",),
            DnsInventory::Known(["fe80::1".parse().unwrap()].into_iter().collect())
        );
    }

    #[test]
    fn failed_scutil_command_only_accepts_explicit_no_configuration() {
        assert_eq!(
            dns_inventory_from_scutil_command(false, b"", b"No DNS configuration available\n"),
            DnsInventory::Known(BTreeSet::new())
        );
        assert_eq!(
            dns_inventory_from_scutil_command(
                false,
                b"DNS configuration\nresolver #1\n nameserver[0] : 192.168.241.1\n",
                b"",
            ),
            DnsInventory::Unknown
        );
    }

    #[test]
    fn scutil_incomplete_malformed_or_non_utf8_output_is_unknown() {
        for output in [
            "",
            "DNS configuration\nresolver #1\n",
            "DNS configuration\nresolver #1\n nameserver[0] 192.168.241.1\n",
            "DNS configuration\nresolver #1\n nameserver[0] : not-an-ip\n",
        ] {
            assert_eq!(parse_scutil_dns(output), DnsInventory::Unknown);
        }
        assert_eq!(
            dns_inventory_from_scutil_command(true, &[0xff], b""),
            DnsInventory::Unknown
        );
    }

    #[test]
    fn resolv_conf_parser_returns_all_numeric_nameservers() {
        let contents = r#"
# generated resolver configuration
search lan
nameserver 127.0.0.53
nameserver 2001:4860:4860::8888 # uplink
options edns0 trust-ad
"#;

        assert_eq!(
            parse_resolv_conf(contents),
            DnsInventory::Known(
                [
                    "127.0.0.53".parse().unwrap(),
                    "2001:4860:4860::8888".parse().unwrap(),
                ]
                .into_iter()
                .collect()
            )
        );
    }

    #[test]
    fn linux_dns_inventory_is_unknown_when_every_source_read_fails() {
        let inventory = discover_linux_dns_inventory_with(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "fixture missing",
            ))
        });

        assert_eq!(inventory, DnsInventory::Unknown);
    }

    #[test]
    fn linux_dns_inventory_uses_readable_systemd_resolved_source() {
        let inventory = discover_linux_dns_inventory_with(|path| {
            if path == "/run/systemd/resolve/resolv.conf" {
                Ok("nameserver 10.20.30.40\n".to_string())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "fixture missing",
                ))
            }
        });

        assert_eq!(
            inventory,
            DnsInventory::Known(["10.20.30.40".parse().unwrap()].into_iter().collect())
        );
    }

    #[test]
    fn readable_resolver_config_without_nameserver_is_authoritative_empty() {
        assert_eq!(
            discover_linux_dns_inventory_with(|path| {
                if path == "/etc/resolv.conf" {
                    Ok("# deliberately no resolver\noptions edns0\n".to_string())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "fixture missing",
                    ))
                }
            }),
            DnsInventory::Known(BTreeSet::new())
        );
    }

    #[test]
    fn malformed_resolver_nameserver_is_unknown() {
        assert_eq!(
            parse_resolv_conf("nameserver not-an-ip\n"),
            DnsInventory::Unknown
        );
        assert_eq!(
            parse_resolv_conf("nameserver 1.1.1.1 trailing\n"),
            DnsInventory::Unknown
        );
    }

    #[test]
    fn successful_windows_adapter_snapshot_can_authoritatively_have_no_dns() {
        assert_eq!(
            dns_inventory_from_netdev_snapshot(true, std::iter::empty()),
            DnsInventory::Known(BTreeSet::new())
        );
        assert_eq!(
            dns_inventory_from_netdev_snapshot(false, std::iter::empty()),
            DnsInventory::Unknown
        );
    }

    #[test]
    fn complete_inventory_protects_only_exact_ipv4_addresses() {
        let inventory = [
            NativeRouteInterface {
                local_addresses: [
                    "192.168.240.44".parse().unwrap(),
                    "fd00::44".parse().unwrap(),
                ]
                .into_iter()
                .collect(),
                gateway_addresses: Some(["192.168.240.1".parse().unwrap()].into_iter().collect()),
            },
            NativeRouteInterface {
                local_addresses: ["10.0.0.20".parse().unwrap()].into_iter().collect(),
                gateway_addresses: Some(["10.0.0.1".parse().unwrap()].into_iter().collect()),
            },
        ];

        let exclusions = route_exclusions_from_inventory(
            [
                "192.168.240.44".parse().unwrap(),
                "203.0.113.10".parse().unwrap(),
            ],
            &inventory,
            DnsInventory::Known(
                [
                    "10.0.0.53".parse().unwrap(),
                    "192.168.240.53".parse().unwrap(),
                    "2001:4860:4860::8888".parse().unwrap(),
                ]
                .into_iter()
                .collect(),
            ),
        )
        .unwrap();

        assert_eq!(
            exclusions,
            [
                "10.0.0.1".parse().unwrap(),
                "10.0.0.20".parse().unwrap(),
                "10.0.0.53".parse().unwrap(),
                "203.0.113.10".parse().unwrap(),
                "192.168.240.1".parse().unwrap(),
                "192.168.240.44".parse().unwrap(),
                "192.168.240.53".parse().unwrap(),
            ]
            .into_iter()
            .collect()
        );
        assert!(!exclusions.contains(&"192.168.240.99".parse().unwrap()));
    }

    #[test]
    fn inventory_combines_local_gateway_dns_and_control_ipv4_exactly() {
        let exclusions = collect_ipv4_exclusions(
            [
                "192.168.240.44".parse().unwrap(),
                "fd00::44".parse().unwrap(),
            ],
            ["192.168.240.1".parse().unwrap()],
            [
                "192.168.240.53".parse().unwrap(),
                "2001:4860:4860::8888".parse().unwrap(),
            ],
            [
                "203.0.113.10".parse().unwrap(),
                "192.168.240.44".parse().unwrap(),
            ],
        );

        assert_eq!(
            exclusions,
            [
                "203.0.113.10".parse().unwrap(),
                "192.168.240.1".parse().unwrap(),
                "192.168.240.44".parse().unwrap(),
                "192.168.240.53".parse().unwrap(),
            ]
            .into_iter()
            .collect()
        );
    }
}
