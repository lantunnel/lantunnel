//! Explicit local service exports for destinations owned by the local Peer.

use std::net::{IpAddr, SocketAddr};

use tp_core::config::{
    LocalServiceExportConfig, LocalServiceProtocolConfig, LocalServiceRouteKindConfig,
    LocalServiceSourcePolicyConfig,
};
use tp_core::Protocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalRouteKind {
    Overlay,
    PeerLanHost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourcePeerPolicy {
    AnyTunnelPeer,
    Only(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalServiceExport {
    pub route_kind: LocalRouteKind,
    pub ingress_protocol: Protocol,
    pub ingress_port: u16,
    pub source_policy: SourcePeerPolicy,
    pub local_host: IpAddr,
    pub local_port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalRouteClaims {
    pub overlay: Option<IpAddr>,
    pub peer_lan_hosts: Vec<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLocalTarget {
    pub route_kind: LocalRouteKind,
    pub target: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalTargetDeny {
    MissingAuthenticatedSource,
    NoMatchingExport,
    SourceNotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalServiceExportError {
    ZeroIngressPort {
        export_index: usize,
    },
    ZeroLocalPort {
        export_index: usize,
    },
    EmptyOnlyPolicy {
        export_index: usize,
    },
    BlankSourcePeer {
        export_index: usize,
        peer_index: usize,
    },
    InvalidSourcePeer {
        export_index: usize,
        peer_index: usize,
    },
    InvalidLocalHost {
        export_index: usize,
        local_host: String,
    },
    UnusableLocalHost {
        export_index: usize,
        host: IpAddr,
    },
    AmbiguousExport {
        first_index: usize,
        second_index: usize,
        route_kind: LocalRouteKind,
        protocol: Protocol,
        ingress_port: u16,
    },
    AmbiguousClaim {
        address: IpAddr,
    },
}

#[derive(Debug, Clone)]
pub struct LocalTargetResolver {
    claims: LocalRouteClaims,
    exports: Vec<LocalServiceExport>,
}

impl LocalTargetResolver {
    pub fn new(
        claims: LocalRouteClaims,
        exports: Vec<LocalServiceExport>,
    ) -> Result<Self, LocalServiceExportError> {
        if let Some(address) = claims
            .overlay
            .filter(|overlay| claims.peer_lan_hosts.contains(overlay))
        {
            return Err(LocalServiceExportError::AmbiguousClaim { address });
        }
        for (export_index, export) in exports.iter().enumerate() {
            if export.ingress_port == 0 {
                return Err(LocalServiceExportError::ZeroIngressPort { export_index });
            }
            if export.local_port == 0 {
                return Err(LocalServiceExportError::ZeroLocalPort { export_index });
            }
            if export.local_host.is_unspecified() || export.local_host.is_multicast() {
                return Err(LocalServiceExportError::UnusableLocalHost {
                    export_index,
                    host: export.local_host,
                });
            }
            if matches!(&export.source_policy, SourcePeerPolicy::Only(peers) if peers.is_empty()) {
                return Err(LocalServiceExportError::EmptyOnlyPolicy { export_index });
            }
            if let SourcePeerPolicy::Only(peers) = &export.source_policy {
                for (peer_index, peer) in peers.iter().enumerate() {
                    if peer.trim().is_empty() {
                        return Err(LocalServiceExportError::BlankSourcePeer {
                            export_index,
                            peer_index,
                        });
                    }
                    if peer.trim() != peer || peer == "*" {
                        return Err(LocalServiceExportError::InvalidSourcePeer {
                            export_index,
                            peer_index,
                        });
                    }
                }
            }
            if let Some((first_index, _)) =
                exports[..export_index]
                    .iter()
                    .enumerate()
                    .find(|(_, prior)| {
                        prior.route_kind == export.route_kind
                            && prior.ingress_protocol == export.ingress_protocol
                            && prior.ingress_port == export.ingress_port
                    })
            {
                return Err(LocalServiceExportError::AmbiguousExport {
                    first_index,
                    second_index: export_index,
                    route_kind: export.route_kind,
                    protocol: export.ingress_protocol,
                    ingress_port: export.ingress_port,
                });
            }
        }
        Ok(Self { claims, exports })
    }

    /// Returns `Ok(None)` when `requested` is not owned by the local Peer.
    pub fn resolve(
        &self,
        authenticated_source_peer: Option<&str>,
        protocol: Protocol,
        requested: SocketAddr,
    ) -> Result<Option<ResolvedLocalTarget>, LocalTargetDeny> {
        let route_kind = if self.claims.overlay == Some(requested.ip()) {
            LocalRouteKind::Overlay
        } else if self.claims.peer_lan_hosts.contains(&requested.ip()) {
            LocalRouteKind::PeerLanHost
        } else {
            return Ok(None);
        };
        let source_peer = authenticated_source_peer
            .filter(|peer| !peer.trim().is_empty())
            .ok_or(LocalTargetDeny::MissingAuthenticatedSource)?;
        let Some(export) = self.exports.iter().find(|export| {
            export.route_kind == route_kind
                && export.ingress_protocol == protocol
                && export.ingress_port == requested.port()
        }) else {
            return Err(LocalTargetDeny::NoMatchingExport);
        };
        let source_allowed = match &export.source_policy {
            SourcePeerPolicy::AnyTunnelPeer => true,
            SourcePeerPolicy::Only(peers) => peers.iter().any(|peer| peer == source_peer),
        };
        if !source_allowed {
            return Err(LocalTargetDeny::SourceNotAllowed);
        }
        Ok(Some(ResolvedLocalTarget {
            route_kind,
            target: SocketAddr::new(export.local_host, export.local_port),
        }))
    }
}

pub fn compile_local_service_exports(
    configs: &[LocalServiceExportConfig],
) -> Result<Vec<LocalServiceExport>, LocalServiceExportError> {
    let mut exports = Vec::with_capacity(configs.len());
    for (export_index, config) in configs.iter().enumerate() {
        let local_host =
            config
                .local_host
                .parse()
                .map_err(|_| LocalServiceExportError::InvalidLocalHost {
                    export_index,
                    local_host: config.local_host.clone(),
                })?;
        exports.push(LocalServiceExport {
            route_kind: match config.route_kind {
                LocalServiceRouteKindConfig::Overlay => LocalRouteKind::Overlay,
                LocalServiceRouteKindConfig::PeerLanHost => LocalRouteKind::PeerLanHost,
            },
            ingress_protocol: match config.protocol {
                LocalServiceProtocolConfig::Tcp => Protocol::Tcp,
                LocalServiceProtocolConfig::Udp => Protocol::Udp,
            },
            ingress_port: config.ingress_port,
            source_policy: match &config.source_policy {
                LocalServiceSourcePolicyConfig::AnyTunnelPeer => SourcePeerPolicy::AnyTunnelPeer,
                LocalServiceSourcePolicyConfig::Only { peers } => {
                    SourcePeerPolicy::Only(peers.clone())
                }
            },
            local_host,
            local_port: config.local_port,
        });
    }
    LocalTargetResolver::new(LocalRouteClaims::default(), exports.clone())?;
    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_core::config::{
        LocalServiceExportConfig, LocalServiceProtocolConfig, LocalServiceRouteKindConfig,
        LocalServiceSourcePolicyConfig,
    };

    #[test]
    fn config_compiles_to_the_same_exact_runtime_model() {
        let compiled = compile_local_service_exports(&[LocalServiceExportConfig {
            route_kind: LocalServiceRouteKindConfig::PeerLanHost,
            protocol: LocalServiceProtocolConfig::Udp,
            ingress_port: 27015,
            source_policy: LocalServiceSourcePolicyConfig::Only {
                peers: vec!["mesh-RemoteB1-0".into()],
            },
            local_host: "127.0.0.1".into(),
            local_port: 37015,
        }])
        .expect("valid config");

        assert_eq!(
            compiled,
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::PeerLanHost,
                ingress_protocol: Protocol::Udp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::Only(vec!["mesh-RemoteB1-0".into()]),
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 37015,
            }]
        );
    }

    #[test]
    fn owned_overlay_without_an_export_is_denied() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            Vec::new(),
        )
        .expect("empty exports are valid");

        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Tcp,
                SocketAddr::new(overlay, 27015),
            ),
            Err(LocalTargetDeny::NoMatchingExport),
        );
    }

    #[test]
    fn explicit_overlay_export_maps_an_authenticated_tunnel_peer() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let local_host = "127.0.0.1".parse().expect("local IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host,
                local_port: 37015,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Tcp,
                SocketAddr::new(overlay, 27015),
            ),
            Ok(Some(ResolvedLocalTarget {
                route_kind: LocalRouteKind::Overlay,
                target: SocketAddr::new(local_host, 37015),
            })),
        );
    }

    #[test]
    fn overlay_export_denies_a_different_protocol_or_port() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 37015,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Udp,
                SocketAddr::new(overlay, 27015),
            ),
            Err(LocalTargetDeny::NoMatchingExport),
        );
        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Tcp,
                SocketAddr::new(overlay, 27016),
            ),
            Err(LocalTargetDeny::NoMatchingExport),
        );
    }

    #[test]
    fn any_tunnel_peer_still_rejects_an_unattributed_source() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 27015,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(None, Protocol::Tcp, SocketAddr::new(overlay, 27015)),
            Err(LocalTargetDeny::MissingAuthenticatedSource),
        );
    }

    #[test]
    fn only_policy_maps_an_explicitly_allowed_peer() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let local_host = "127.0.0.1".parse().expect("local IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Udp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::Only(vec!["peer-b".into()]),
                local_host,
                local_port: 37015,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Udp,
                SocketAddr::new(overlay, 27015),
            ),
            Ok(Some(ResolvedLocalTarget {
                route_kind: LocalRouteKind::Overlay,
                target: SocketAddr::new(local_host, 37015),
            })),
        );
    }

    #[test]
    fn only_policy_rejects_a_different_authenticated_peer() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::Only(vec!["peer-b".into()]),
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 27015,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(
                Some("peer-c"),
                Protocol::Tcp,
                SocketAddr::new(overlay, 27015),
            ),
            Err(LocalTargetDeny::SourceNotAllowed),
        );
    }

    #[test]
    fn explicit_peer_lan_host_export_maps_only_an_exact_active_alias() {
        let lan_alias = "192.168.40.12".parse().expect("LAN alias");
        let local_host = "127.0.0.1".parse().expect("local IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some("198.18.7.9".parse().expect("overlay IP")),
                peer_lan_hosts: vec![lan_alias],
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::PeerLanHost,
                ingress_protocol: Protocol::Udp,
                ingress_port: 39001,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host,
                local_port: 49001,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Udp,
                SocketAddr::new(lan_alias, 39001),
            ),
            Ok(Some(ResolvedLocalTarget {
                route_kind: LocalRouteKind::PeerLanHost,
                target: SocketAddr::new(local_host, 49001),
            })),
        );
        assert_eq!(
            resolver.resolve(
                Some("peer-b"),
                Protocol::Udp,
                "192.168.40.13:39001".parse().expect("adjacent host"),
            ),
            Ok(None),
        );
    }

    #[test]
    fn zero_ingress_port_is_rejected_instead_of_becoming_a_wildcard() {
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 0,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 27015,
            }],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::ZeroIngressPort { export_index: 0 })
        ));
    }

    #[test]
    fn zero_local_port_is_rejected() {
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 0,
            }],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::ZeroLocalPort { export_index: 0 })
        ));
    }

    #[test]
    fn empty_only_policy_is_rejected_instead_of_meaning_any_peer() {
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::Only(Vec::new()),
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 27015,
            }],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::EmptyOnlyPolicy { export_index: 0 })
        ));
    }

    #[test]
    fn blank_source_is_not_an_authenticated_peer() {
        let overlay = "198.18.7.9".parse().expect("overlay IP");
        let resolver = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(overlay),
                peer_lan_hosts: Vec::new(),
            },
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 27015,
            }],
        )
        .expect("valid export");

        assert_eq!(
            resolver.resolve(Some("  "), Protocol::Tcp, SocketAddr::new(overlay, 27015),),
            Err(LocalTargetDeny::MissingAuthenticatedSource),
        );
    }

    #[test]
    fn blank_peer_in_only_policy_is_rejected() {
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::Only(vec!["peer-b".into(), " ".into()]),
                local_host: "127.0.0.1".parse().expect("local IP"),
                local_port: 27015,
            }],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::BlankSourcePeer {
                export_index: 0,
                peer_index: 1,
            })
        ));
    }

    #[test]
    fn unspecified_local_host_is_rejected() {
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Tcp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "0.0.0.0".parse().expect("unspecified IP"),
                local_port: 27015,
            }],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::UnusableLocalHost {
                export_index: 0,
                host,
            }) if host == "0.0.0.0".parse::<IpAddr>().expect("unspecified IP")
        ));
    }

    #[test]
    fn multicast_local_host_is_rejected() {
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![LocalServiceExport {
                route_kind: LocalRouteKind::Overlay,
                ingress_protocol: Protocol::Udp,
                ingress_port: 27015,
                source_policy: SourcePeerPolicy::AnyTunnelPeer,
                local_host: "239.1.2.3".parse().expect("multicast IP"),
                local_port: 27015,
            }],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::UnusableLocalHost {
                export_index: 0,
                host,
            }) if host == "239.1.2.3".parse::<IpAddr>().expect("multicast IP")
        ));
    }

    #[test]
    fn duplicate_l4_export_is_rejected_as_ambiguous() {
        let export = LocalServiceExport {
            route_kind: LocalRouteKind::Overlay,
            ingress_protocol: Protocol::Tcp,
            ingress_port: 27015,
            source_policy: SourcePeerPolicy::Only(vec!["peer-b".into()]),
            local_host: "127.0.0.1".parse().expect("local IP"),
            local_port: 27015,
        };
        let result = LocalTargetResolver::new(
            LocalRouteClaims::default(),
            vec![
                export,
                LocalServiceExport {
                    route_kind: LocalRouteKind::Overlay,
                    ingress_protocol: Protocol::Tcp,
                    ingress_port: 27015,
                    source_policy: SourcePeerPolicy::Only(vec!["peer-c".into()]),
                    local_host: "127.0.0.1".parse().expect("local IP"),
                    local_port: 37015,
                },
            ],
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::AmbiguousExport {
                first_index: 0,
                second_index: 1,
                route_kind: LocalRouteKind::Overlay,
                protocol: Protocol::Tcp,
                ingress_port: 27015,
            })
        ));
    }

    #[test]
    fn address_cannot_be_both_overlay_and_peer_lan_host() {
        let address = "198.18.7.9".parse().expect("IP");
        let result = LocalTargetResolver::new(
            LocalRouteClaims {
                overlay: Some(address),
                peer_lan_hosts: vec![address],
            },
            Vec::new(),
        );

        assert!(matches!(
            result,
            Err(LocalServiceExportError::AmbiguousClaim { address: got }) if got == address
        ));
    }
}
