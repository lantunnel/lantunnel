//! Sparse exact-Peer `/32` ownership lookup.
//!
//! This table answers only the first routing question: which logical Peer owns
//! an exact Overlay or private LAN host address? Path and lane selection happen
//! afterwards. It intentionally has no subnet-wide fallback and represents
//! duplicate ownership as an ambiguous, fail-closed result.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use thiserror::Error;

use crate::peer_runtime::{PeerGossipDirectoryV2, PeerRuntimeErrorV2, PeerRuntimeRecordV2};

pub const MAX_LAN_ALIASES_PER_PEER: usize = 255;
pub const MAX_UNIQUE_LAN_ALIAS_DESTINATIONS: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverlayRouteMatch {
    Peer { peer_id: String },
    Unmatched,
    Ambiguous,
}

#[derive(Debug, Default)]
pub struct OverlayRouteMatcher {
    peer_overlays: BTreeMap<String, Ipv4Addr>,
    overlay_owners: BTreeMap<Ipv4Addr, BTreeSet<String>>,
    peer_lan_aliases: BTreeMap<String, BTreeSet<Ipv4Addr>>,
    lan_alias_owners: BTreeMap<Ipv4Addr, BTreeSet<String>>,
    v2_lan_exports: PeerGossipDirectoryV2,
}

impl OverlayRouteMatcher {
    pub fn upsert_replica(
        &mut self,
        tunnel_id: &str,
        replica_id: &str,
    ) -> Result<Ipv4Addr, OverlayRouteInstallError> {
        let overlay =
            crate::overlay::overlay_ipv4_for_replica_id(tunnel_id, replica_id).map_err(|_| {
                OverlayRouteInstallError::ReplicaOutsideTunnel {
                    tunnel_id: tunnel_id.to_string(),
                    replica_id: replica_id.to_string(),
                }
            })?;
        let peer_id = crate::p2p::replica::replica_family_id(replica_id);
        self.upsert_peer_overlay(peer_id, overlay);
        Ok(overlay)
    }

    pub fn upsert_peer_overlay(&mut self, peer_id: impl Into<String>, overlay: Ipv4Addr) {
        // A V2 Peer ID is an opaque signed identity. The legacy
        // `upsert_replica` caller above performs family normalization before
        // entering this exact Peer-keyed table.
        let peer_id = peer_id.into();
        if let Some(previous) = self.peer_overlays.insert(peer_id.clone(), overlay) {
            if previous != overlay {
                self.remove_owner(&peer_id, previous);
            }
        }
        self.overlay_owners
            .entry(overlay)
            .or_default()
            .insert(peer_id);
    }

    /// Atomically replace one Peer's exact private-LAN host aliases. These are
    /// trusted-Tunnel aliases from authenticated stable-Peer self-publication,
    /// not arbitrary subnets or public aliases. Invalid input leaves the
    /// previous set untouched.
    pub fn replace_peer_lan_aliases<I>(
        &mut self,
        peer_id: &str,
        aliases: I,
    ) -> Result<(), LanAliasInstallError>
    where
        I: IntoIterator<Item = Ipv4Addr>,
    {
        let aliases = aliases.into_iter().collect::<BTreeSet<_>>();
        if aliases.len() > MAX_LAN_ALIASES_PER_PEER {
            return Err(LanAliasInstallError::PeerAliasLimitExceeded {
                count: aliases.len(),
                max: MAX_LAN_ALIASES_PER_PEER,
            });
        }
        if let Some(address) = aliases.iter().find(|address| !address.is_private()) {
            return Err(LanAliasInstallError::NotPrivateHost(*address));
        }

        let peer_id = crate::p2p::replica::replica_family_id(peer_id);
        let mut projected_destinations = self
            .lan_alias_owners
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        if let Some(previous) = self.peer_lan_aliases.get(&peer_id) {
            for address in previous {
                if self
                    .lan_alias_owners
                    .get(address)
                    .is_some_and(|owners| owners.len() == 1 && owners.contains(&peer_id))
                {
                    projected_destinations.remove(address);
                }
            }
        }
        projected_destinations.extend(aliases.iter().copied());
        if projected_destinations.len() > MAX_UNIQUE_LAN_ALIAS_DESTINATIONS {
            return Err(LanAliasInstallError::UniqueDestinationLimitExceeded {
                count: projected_destinations.len(),
                max: MAX_UNIQUE_LAN_ALIAS_DESTINATIONS,
            });
        }

        if let Some(previous) = self
            .peer_lan_aliases
            .insert(peer_id.clone(), aliases.clone())
        {
            for address in previous {
                self.remove_lan_owner(&peer_id, address);
            }
        }
        for address in aliases {
            self.lan_alias_owners
                .entry(address)
                .or_default()
                .insert(peer_id.clone());
        }
        Ok(())
    }

    /// Atomically replace every private-LAN host alias in the matcher from a
    /// Platform heartbeat full snapshot. All limits and address classes are
    /// validated against a detached candidate table before live state moves.
    pub fn replace_lan_alias_snapshot(
        &mut self,
        snapshot: &[(String, Vec<Ipv4Addr>)],
    ) -> Result<(), LanAliasInstallError> {
        let mut peer_lan_aliases = BTreeMap::<String, BTreeSet<Ipv4Addr>>::new();
        for (peer_id, aliases) in snapshot {
            peer_lan_aliases
                .entry(crate::p2p::replica::replica_family_id(peer_id))
                .or_default()
                .extend(aliases.iter().copied());
        }

        for aliases in peer_lan_aliases.values() {
            if aliases.len() > MAX_LAN_ALIASES_PER_PEER {
                return Err(LanAliasInstallError::PeerAliasLimitExceeded {
                    count: aliases.len(),
                    max: MAX_LAN_ALIASES_PER_PEER,
                });
            }
            if let Some(address) = aliases.iter().find(|address| !address.is_private()) {
                return Err(LanAliasInstallError::NotPrivateHost(*address));
            }
        }

        let mut lan_alias_owners = BTreeMap::<Ipv4Addr, BTreeSet<String>>::new();
        for (peer_id, aliases) in &peer_lan_aliases {
            for address in aliases {
                lan_alias_owners
                    .entry(*address)
                    .or_default()
                    .insert(peer_id.clone());
            }
        }
        if lan_alias_owners.len() > MAX_UNIQUE_LAN_ALIAS_DESTINATIONS {
            return Err(LanAliasInstallError::UniqueDestinationLimitExceeded {
                count: lan_alias_owners.len(),
                max: MAX_UNIQUE_LAN_ALIAS_DESTINATIONS,
            });
        }

        self.peer_lan_aliases = peer_lan_aliases;
        self.lan_alias_owners = lan_alias_owners;
        Ok(())
    }

    /// Explicit retirement only. A soft-missing membership cycle does not call
    /// this method, so a healthy retained PeerLink keeps its stable `/32`.
    pub fn remove_peer(&mut self, peer_id: &str) -> bool {
        let mut removed = false;
        if let Some(overlay) = self.peer_overlays.remove(peer_id) {
            self.remove_owner(peer_id, overlay);
            removed = true;
        }
        if let Some(aliases) = self.peer_lan_aliases.remove(peer_id) {
            for address in aliases {
                self.remove_lan_owner(peer_id, address);
            }
            removed = true;
        }
        removed
    }

    pub fn match_destination(&self, destination: IpAddr) -> OverlayRouteMatch {
        let IpAddr::V4(destination) = destination else {
            return OverlayRouteMatch::Unmatched;
        };
        let mut owners = BTreeSet::new();
        if let Some(overlay_owners) = self.overlay_owners.get(&destination) {
            owners.extend(overlay_owners.iter().cloned());
        }
        if let Some(alias_owners) = self.lan_alias_owners.get(&destination) {
            owners.extend(alias_owners.iter().cloned());
        }
        if owners.is_empty() {
            return OverlayRouteMatch::Unmatched;
        }
        if owners.len() != 1 {
            return OverlayRouteMatch::Ambiguous;
        }
        OverlayRouteMatch::Peer {
            peer_id: owners
                .into_iter()
                .next()
                .expect("one owner was checked above"),
        }
    }

    /// Full-replace one authenticated origin's V2 runtime LAN Export record.
    /// Validation happens before the directory changes local ActiveHere order.
    pub fn replace_v2_lan_export_origin(
        &mut self,
        origin_peer_id: &str,
        record: PeerRuntimeRecordV2,
    ) -> Result<(), PeerRuntimeErrorV2> {
        self.v2_lan_exports.replace_origin(origin_peer_id, record)
    }

    /// Remove an origin when its PeerLink closes or membership is retired.
    pub fn remove_v2_lan_export_origin(&mut self, origin_peer_id: &str) -> bool {
        self.v2_lan_exports.remove_origin(origin_peer_id)
    }

    pub fn v2_active_lan_export_snapshot(
        &self,
    ) -> Vec<(crate::peer_runtime::LanExportPrefixV2, String)> {
        self.v2_lan_exports.active_export_snapshot()
    }

    /// Process-local position of one currently routable Export origin.
    /// `None` means the origin is not eligible for new Flows, even when its
    /// authenticated Gossip record is still retained elsewhere for repair.
    pub fn v2_lan_export_position(
        &self,
        prefix: crate::peer_runtime::LanExportPrefixV2,
        origin_peer_id: &str,
    ) -> Option<usize> {
        self.v2_lan_exports
            .exporters(prefix)
            .iter()
            .position(|origin| *origin == origin_peer_id)
    }

    /// Resolve V2 destinations without changing the legacy host-alias matcher:
    /// exact signed Overlay `/32` ownership wins, then ready RFC1918 LAN
    /// Exports use longest-prefix match and the directory's local order.
    pub fn match_v2_destination(&self, destination: IpAddr) -> OverlayRouteMatch {
        let IpAddr::V4(destination) = destination else {
            return OverlayRouteMatch::Unmatched;
        };
        if let Some(owners) = self.overlay_owners.get(&destination) {
            return if owners.len() == 1 {
                OverlayRouteMatch::Peer {
                    peer_id: owners.first().expect("one owner was checked above").clone(),
                }
            } else {
                OverlayRouteMatch::Ambiguous
            };
        }
        self.v2_lan_exports
            .longest_prefix_exporter(destination)
            .map(|(_prefix, peer_id)| OverlayRouteMatch::Peer {
                peer_id: peer_id.to_owned(),
            })
            .unwrap_or(OverlayRouteMatch::Unmatched)
    }

    pub fn peer_count(&self) -> usize {
        self.peer_overlays.len()
    }

    pub fn has_peer_overlay(&self, peer_id: &str) -> bool {
        self.peer_overlays.contains_key(peer_id)
    }

    /// Stable snapshot for installing sparse OS routes. Ambiguous addresses
    /// are deliberately omitted, matching `match_destination`'s fail-closed
    /// behavior.
    pub fn route_snapshot(&self) -> Vec<(Ipv4Addr, String)> {
        self.overlay_owners
            .iter()
            .filter(|(_, owners)| owners.len() == 1)
            .map(|(overlay, owners)| {
                (
                    *overlay,
                    owners.first().expect("one owner was checked above").clone(),
                )
            })
            .collect()
    }

    /// Stable private host destinations for sparse TUN capture. Ambiguous
    /// aliases stay in this list: the packet must reach `match_destination`
    /// and fail closed instead of escaping through the machine's physical LAN.
    pub fn lan_alias_destinations(&self) -> Vec<Ipv4Addr> {
        self.lan_alias_owners.keys().copied().collect()
    }

    fn remove_owner(&mut self, peer_id: &str, overlay: Ipv4Addr) {
        let mut remove_overlay = false;
        if let Some(owners) = self.overlay_owners.get_mut(&overlay) {
            owners.remove(peer_id);
            remove_overlay = owners.is_empty();
        }
        if remove_overlay {
            self.overlay_owners.remove(&overlay);
        }
    }

    fn remove_lan_owner(&mut self, peer_id: &str, address: Ipv4Addr) {
        let mut remove_address = false;
        if let Some(owners) = self.lan_alias_owners.get_mut(&address) {
            owners.remove(peer_id);
            remove_address = owners.is_empty();
        }
        if remove_address {
            self.lan_alias_owners.remove(&address);
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OverlayRouteInstallError {
    #[error("Replica ID {replica_id:?} is not a stable member of Tunnel {tunnel_id:?}")]
    ReplicaOutsideTunnel {
        tunnel_id: String,
        replica_id: String,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LanAliasInstallError {
    #[error("LAN host alias {0} is not an RFC1918 private IPv4 address")]
    NotPrivateHost(Ipv4Addr),
    #[error("logical Peer requested {count} LAN host aliases; maximum is {max}")]
    PeerAliasLimitExceeded { count: usize, max: usize },
    #[error("LAN alias table would contain {count} unique destinations; maximum is {max}")]
    UniqueDestinationLimitExceeded { count: usize, max: usize },
}
