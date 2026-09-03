//! Process-local, origin-only Peer runtime records and LAN Export ordering.
//!
//! A record belongs to the authenticated PeerLink that delivered it. There is
//! no revision, epoch, tombstone, multi-hop store, or persistence layer here.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::Ipv4Addr;

use sha2::{Digest, Sha256};
use thiserror::Error;

const OVERLAY_BASE_V2: u32 = u32::from_be_bytes([198, 18, 0, 0]);
const OVERLAY_MASK_V2: u32 = 0xffff_0000;
pub const MAX_LAN_EXPORTS_PER_PEER_V2: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LanExportPrefixV2 {
    pub network: Ipv4Addr,
    pub prefix_len: u8,
}

impl LanExportPrefixV2 {
    pub fn new(network: Ipv4Addr, prefix_len: u8) -> Result<Self, PeerRuntimeErrorV2> {
        if !(8..=32).contains(&prefix_len) {
            return Err(PeerRuntimeErrorV2::InvalidPrefix);
        }
        let mask = ipv4_mask(prefix_len);
        let canonical = Ipv4Addr::from(u32::from(network) & mask);
        if canonical != network {
            return Err(PeerRuntimeErrorV2::InvalidPrefix);
        }
        let first = u32::from(canonical);
        let last = first | !mask;
        if !is_rfc1918(canonical)
            || !is_rfc1918(Ipv4Addr::from(last))
            || ranges_overlap(
                first,
                last,
                OVERLAY_BASE_V2,
                OVERLAY_BASE_V2 | !OVERLAY_MASK_V2,
            )
            || canonical.is_loopback()
            || canonical.is_link_local()
            || canonical.is_multicast()
            || canonical.is_unspecified()
        {
            return Err(PeerRuntimeErrorV2::InvalidPrefix);
        }
        Ok(Self {
            network: canonical,
            prefix_len,
        })
    }

    pub fn contains(self, address: Ipv4Addr) -> bool {
        u32::from(address) & ipv4_mask(self.prefix_len) == u32::from(self.network)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanExportV2 {
    pub prefix: LanExportPrefixV2,
    pub ready: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerRuntimeRecordV2 {
    pub lan_exports: Vec<LanExportV2>,
}

impl PeerRuntimeRecordV2 {
    pub fn new(mut lan_exports: Vec<LanExportV2>) -> Result<Self, PeerRuntimeErrorV2> {
        if lan_exports.len() > MAX_LAN_EXPORTS_PER_PEER_V2 {
            return Err(PeerRuntimeErrorV2::TooManyExports);
        }
        lan_exports
            .sort_by_key(|export| (u32::from(export.prefix.network), export.prefix.prefix_len));
        if lan_exports
            .windows(2)
            .any(|pair| pair[0].prefix == pair[1].prefix)
        {
            return Err(PeerRuntimeErrorV2::DuplicatePrefix);
        }
        Ok(Self { lan_exports })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(1 + self.lan_exports.len() * 6);
        encoded.push(self.lan_exports.len() as u8);
        for export in &self.lan_exports {
            encoded.extend_from_slice(&export.prefix.network.octets());
            encoded.push(export.prefix.prefix_len);
            encoded.push(u8::from(export.ready));
        }
        encoded
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, PeerRuntimeErrorV2> {
        let Some(count) = encoded.first().copied() else {
            return Err(PeerRuntimeErrorV2::InvalidEncoding);
        };
        let count = usize::from(count);
        if count > MAX_LAN_EXPORTS_PER_PEER_V2 || encoded.len() != 1 + count * 6 {
            return Err(PeerRuntimeErrorV2::InvalidEncoding);
        }
        let mut exports = Vec::with_capacity(count);
        let (chunks, _remainder) = encoded[1..].as_chunks::<6>();
        for chunk in chunks {
            let prefix = LanExportPrefixV2::new(
                Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]),
                chunk[4],
            )?;
            let ready = match chunk[5] {
                0 => false,
                1 => true,
                _ => return Err(PeerRuntimeErrorV2::InvalidEncoding),
            };
            exports.push(LanExportV2 { prefix, ready });
        }
        Self::new(exports)
    }

    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.encode()).into()
    }
}

/// The owner's LAN Export answer, before any interface facts are applied.
///
/// `configured` is exactly the list its owner typed. `auto_current_lan` adds
/// the private networks this machine is attached to right now; it never
/// removes, reorders, or rewrites a configured prefix, so the two answers stay
/// independent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalLanExportConfigV2 {
    pub configured: Vec<LanExportPrefixV2>,
    pub auto_current_lan: bool,
}

impl LocalLanExportConfigV2 {
    /// Resolve the record to publish against one connected-LAN snapshot.
    ///
    /// `None` means interface discovery was unavailable. Configured prefixes
    /// stay in the record but are withdrawn, and nothing is added
    /// automatically: an unreadable interface list is not evidence of a LAN,
    /// and neither is the network this machine used to be on.
    pub fn resolve(&self, connected_lans: Option<&[LanExportPrefixV2]>) -> PeerRuntimeRecordV2 {
        let mut seen = HashSet::new();
        let mut exports = Vec::new();
        for prefix in self.configured.iter().copied() {
            if exports.len() == MAX_LAN_EXPORTS_PER_PEER_V2 {
                break;
            }
            if seen.insert(prefix) {
                exports.push(LanExportV2 {
                    prefix,
                    ready: connected_lans.is_some_and(|connected| connected.contains(&prefix)),
                });
            }
        }
        if self.auto_current_lan {
            for prefix in connected_lans.unwrap_or_default().iter().copied() {
                // The typed list is admitted first and the limit is never
                // raised, so a machine on many networks cannot silently push a
                // prefix its owner asked for out of the record.
                if exports.len() == MAX_LAN_EXPORTS_PER_PEER_V2 {
                    break;
                }
                if seen.insert(prefix) {
                    exports.push(LanExportV2 {
                        prefix,
                        ready: true,
                    });
                }
            }
        }
        PeerRuntimeRecordV2::new(exports).expect("resolved exports are deduplicated and bounded")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeRecordRepairV2 {
    InSync,
    NeedFullRecord,
}

#[derive(Clone, Debug, Default)]
pub struct PeerGossipDirectoryV2 {
    records: HashMap<String, PeerRuntimeRecordV2>,
    order: HashMap<LanExportPrefixV2, VecDeque<String>>,
}

impl PeerGossipDirectoryV2 {
    pub fn replace_origin(
        &mut self,
        origin_peer_id: &str,
        record: PeerRuntimeRecordV2,
    ) -> Result<(), PeerRuntimeErrorV2> {
        validate_origin(origin_peer_id)?;
        let previous_ready = self
            .records
            .get(origin_peer_id)
            .map(ready_prefixes)
            .unwrap_or_default();
        let next_ready = ready_prefixes(&record);

        for removed in previous_ready.difference(&next_ready) {
            remove_from_order(&mut self.order, *removed, origin_peer_id);
        }
        for added in next_ready.difference(&previous_ready) {
            let peers = self.order.entry(*added).or_default();
            if !peers.iter().any(|peer| peer == origin_peer_id) {
                peers.push_back(origin_peer_id.to_owned());
            }
        }
        self.records.insert(origin_peer_id.to_owned(), record);
        Ok(())
    }

    pub fn remove_origin(&mut self, origin_peer_id: &str) -> bool {
        let Some(record) = self.records.remove(origin_peer_id) else {
            return false;
        };
        for prefix in ready_prefixes(&record) {
            remove_from_order(&mut self.order, prefix, origin_peer_id);
        }
        true
    }

    pub fn compare_digest(
        &self,
        origin_peer_id: &str,
        remote_hash: [u8; 32],
    ) -> RuntimeRecordRepairV2 {
        if self
            .records
            .get(origin_peer_id)
            .is_some_and(|record| record.hash() == remote_hash)
        {
            RuntimeRecordRepairV2::InSync
        } else {
            RuntimeRecordRepairV2::NeedFullRecord
        }
    }

    pub fn exporters(&self, prefix: LanExportPrefixV2) -> Vec<&str> {
        self.order
            .get(&prefix)
            .map(|peers| peers.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn active_exporter(&self, prefix: LanExportPrefixV2) -> Option<&str> {
        self.order
            .get(&prefix)
            .and_then(|peers| peers.front())
            .map(String::as_str)
    }

    /// Deterministic process-local view of the ready exporter selected for
    /// each prefix. Standby origins and non-ready records are omitted.
    pub fn active_export_snapshot(&self) -> Vec<(LanExportPrefixV2, String)> {
        let mut active = self
            .order
            .iter()
            .filter_map(|(prefix, peers)| {
                peers
                    .front()
                    .map(|origin_peer_id| (*prefix, origin_peer_id.clone()))
            })
            .collect::<Vec<_>>();
        active.sort_by_key(|(prefix, _)| (u32::from(prefix.network), prefix.prefix_len));
        active
    }

    /// Select the local ActiveHere exporter for the longest ready LAN prefix.
    /// Identical prefixes retain this directory's local first-seen order;
    /// there is no cross-Peer probing or globally coordinated active state.
    pub fn longest_prefix_exporter(
        &self,
        destination: Ipv4Addr,
    ) -> Option<(LanExportPrefixV2, &str)> {
        self.order
            .iter()
            .filter(|(prefix, peers)| prefix.contains(destination) && !peers.is_empty())
            .max_by_key(|(prefix, _)| prefix.prefix_len)
            .and_then(|(prefix, peers)| peers.front().map(|peer| (*prefix, peer.as_str())))
    }

    pub fn record(&self, origin_peer_id: &str) -> Option<&PeerRuntimeRecordV2> {
        self.records.get(origin_peer_id)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PeerRuntimeErrorV2 {
    #[error("invalid LAN Export prefix")]
    InvalidPrefix,
    #[error("too many LAN Exports")]
    TooManyExports,
    #[error("duplicate LAN Export prefix")]
    DuplicatePrefix,
    #[error("invalid Peer runtime record encoding")]
    InvalidEncoding,
    #[error("invalid Peer runtime record origin")]
    InvalidOrigin,
}

fn validate_origin(origin: &str) -> Result<(), PeerRuntimeErrorV2> {
    if origin.trim().is_empty() || origin.trim() != origin {
        Err(PeerRuntimeErrorV2::InvalidOrigin)
    } else {
        Ok(())
    }
}

fn ready_prefixes(record: &PeerRuntimeRecordV2) -> HashSet<LanExportPrefixV2> {
    record
        .lan_exports
        .iter()
        .filter(|export| export.ready)
        .map(|export| export.prefix)
        .collect()
}

fn remove_from_order(
    order: &mut HashMap<LanExportPrefixV2, VecDeque<String>>,
    prefix: LanExportPrefixV2,
    origin: &str,
) {
    let Some(peers) = order.get_mut(&prefix) else {
        return;
    };
    peers.retain(|peer| peer != origin);
    if peers.is_empty() {
        order.remove(&prefix);
    }
}

fn is_rfc1918(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    let in_prefix = |base: [u8; 4], bits: u8| {
        value & ipv4_mask(bits) == u32::from(Ipv4Addr::from(base)) & ipv4_mask(bits)
    };
    in_prefix([10, 0, 0, 0], 8) || in_prefix([172, 16, 0, 0], 12) || in_prefix([192, 168, 0, 0], 16)
}

fn ipv4_mask(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    }
}

fn ranges_overlap(first_a: u32, last_a: u32, first_b: u32, last_b: u32) -> bool {
    first_a <= last_b && first_b <= last_a
}
