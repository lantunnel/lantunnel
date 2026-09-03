//! Pairwise Peer Runtime Gossip state for authenticated PeerLinks.
//!
//! The caller owns transport and authentication. This module only turns
//! PeerLink lifecycle, local record changes, time, and inbound control payloads
//! into process-local directory changes and outbound control payloads. It has
//! no revision, generation, tombstone, persistence, or multi-hop state.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::peer_runtime::{
    PeerGossipDirectoryV2, PeerRuntimeErrorV2, PeerRuntimeRecordV2, RuntimeRecordRepairV2,
};
use crate::relay_crypto::RelayControlPayloadV2;

/// Defensive local bound for an already-authenticated opaque Peer ID.
pub const MAX_GOSSIP_PEER_ID_BYTES_V2: usize = 256;
pub const GOSSIP_DIGEST_MIN_INTERVAL_V2: Duration = Duration::from_secs(25);
pub const GOSSIP_DIGEST_MAX_INTERVAL_V2: Duration = Duration::from_secs(35);

/// One payload for the caller to enqueue on the target authenticated PeerLink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerGossipOutboundV2 {
    pub target_peer_id: String,
    pub payload: RelayControlPayloadV2,
}

/// Process-local pairwise Gossip controller for one local Peer.
#[derive(Debug)]
pub struct PeerGossipControllerV2 {
    local_record: PeerRuntimeRecordV2,
    links: BTreeMap<String, PeerGossipLinkV2>,
    directory: PeerGossipDirectoryV2,
}

#[derive(Clone, Copy, Debug)]
struct PeerGossipLinkV2 {
    next_digest_at: Instant,
    reciprocal_pending: bool,
}

impl PeerGossipControllerV2 {
    /// Starts with no learned remote records and no ready PeerLinks.
    pub fn new(local_record: PeerRuntimeRecordV2) -> Self {
        Self {
            local_record,
            links: BTreeMap::new(),
            directory: PeerGossipDirectoryV2::default(),
        }
    }

    /// Registers an already-authenticated Ready PeerLink and returns its
    /// mandatory initial full sync payload.
    pub fn link_ready(
        &mut self,
        remote_peer_id: &str,
        now: Instant,
    ) -> Result<PeerGossipOutboundV2, PeerGossipErrorV2> {
        validate_peer_id(remote_peer_id)?;
        self.links.insert(
            remote_peer_id.to_owned(),
            PeerGossipLinkV2 {
                next_digest_at: now + digest_interval_for(remote_peer_id),
                reciprocal_pending: true,
            },
        );
        Ok(self.full_record_for(remote_peer_id))
    }

    /// Replaces local state and returns one immediate full push per Ready link.
    pub fn set_local_record(
        &mut self,
        local_record: PeerRuntimeRecordV2,
    ) -> Vec<PeerGossipOutboundV2> {
        if self.local_record == local_record {
            return Vec::new();
        }
        self.local_record = local_record;
        let encoded = self.local_record.encode();
        self.links
            .keys()
            .map(|peer_id| PeerGossipOutboundV2 {
                target_peer_id: peer_id.clone(),
                payload: RelayControlPayloadV2::RuntimeRecord(encoded.clone()),
            })
            .collect()
    }

    /// Returns at most one current Digest for each link whose deadline is due.
    pub fn poll_digests(&mut self, now: Instant) -> Vec<PeerGossipOutboundV2> {
        let hash = self.local_record.hash();
        self.links
            .iter_mut()
            .filter_map(|(peer_id, link)| {
                if now < link.next_digest_at {
                    return None;
                }
                link.next_digest_at = now + digest_interval_for(peer_id);
                Some(PeerGossipOutboundV2 {
                    target_peer_id: peer_id.clone(),
                    payload: RelayControlPayloadV2::Digest(hash),
                })
            })
            .collect()
    }

    /// Consumes a payload attributed by the caller to its authenticated link.
    pub fn receive(
        &mut self,
        remote_peer_id: &str,
        payload: RelayControlPayloadV2,
    ) -> Result<Option<PeerGossipOutboundV2>, PeerGossipErrorV2> {
        validate_peer_id(remote_peer_id)?;
        if !self.links.contains_key(remote_peer_id) {
            return Err(PeerGossipErrorV2::LinkNotReady);
        }
        match payload {
            RelayControlPayloadV2::RuntimeRecord(encoded) => {
                let record = PeerRuntimeRecordV2::decode(&encoded)?;
                self.directory.replace_origin(remote_peer_id, record)?;
                let reciprocal_pending = self
                    .links
                    .get_mut(remote_peer_id)
                    .is_some_and(|link| std::mem::take(&mut link.reciprocal_pending));
                // Relay-only signaling and encrypted Gossip use independent
                // senders, so the peer's initial full record can overtake the
                // Answer that installs its key. Reciprocating the first
                // authenticated record of each link generation closes that
                // race without waiting for the periodic Digest/Need repair
                // loop. A repeated record does not reply, so two simultaneous
                // initial pushes cannot form an echo loop.
                Ok(reciprocal_pending.then(|| self.full_record_for(remote_peer_id)))
            }
            RelayControlPayloadV2::Digest(hash) => {
                match self.directory.compare_digest(remote_peer_id, hash) {
                    RuntimeRecordRepairV2::InSync => Ok(None),
                    RuntimeRecordRepairV2::NeedFullRecord => Ok(Some(PeerGossipOutboundV2 {
                        target_peer_id: remote_peer_id.to_owned(),
                        payload: RelayControlPayloadV2::Need,
                    })),
                }
            }
            RelayControlPayloadV2::Need => Ok(Some(self.full_record_for(remote_peer_id))),
            _ => Err(PeerGossipErrorV2::UnexpectedPayload),
        }
    }

    /// Returns the process-local learned remote directory.
    pub fn directory(&self) -> &PeerGossipDirectoryV2 {
        &self.directory
    }

    /// Stops accepting the link's payloads and removes its learned origin.
    pub fn link_closed(&mut self, remote_peer_id: &str) -> Result<bool, PeerGossipErrorV2> {
        validate_peer_id(remote_peer_id)?;
        let was_ready = self.links.remove(remote_peer_id).is_some();
        self.directory.remove_origin(remote_peer_id);
        Ok(was_ready)
    }

    fn full_record_for(&self, remote_peer_id: &str) -> PeerGossipOutboundV2 {
        PeerGossipOutboundV2 {
            target_peer_id: remote_peer_id.to_owned(),
            payload: RelayControlPayloadV2::RuntimeRecord(self.local_record.encode()),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PeerGossipErrorV2 {
    #[error("invalid Peer Gossip peer ID")]
    InvalidPeerId,
    #[error("Peer Gossip payload did not arrive on a ready authenticated PeerLink")]
    LinkNotReady,
    #[error("payload is not valid for Peer Runtime Gossip")]
    UnexpectedPayload,
    #[error(transparent)]
    PeerRuntime(#[from] PeerRuntimeErrorV2),
}

fn validate_peer_id(peer_id: &str) -> Result<(), PeerGossipErrorV2> {
    if peer_id.is_empty()
        || peer_id.trim() != peer_id
        || peer_id.len() > MAX_GOSSIP_PEER_ID_BYTES_V2
    {
        Err(PeerGossipErrorV2::InvalidPeerId)
    } else {
        Ok(())
    }
}

fn digest_interval_for(peer_id: &str) -> Duration {
    // A tiny deterministic jitter is sufficient here: it spreads simultaneous
    // link establishment without creating an RNG/state dependency or a wire
    // field. Reconnect may choose the same point, which is harmless.
    let spread_secs =
        GOSSIP_DIGEST_MAX_INTERVAL_V2.as_secs() - GOSSIP_DIGEST_MIN_INTERVAL_V2.as_secs();
    let hash = peer_id.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(16777619).wrapping_add(u64::from(byte))
    });
    GOSSIP_DIGEST_MIN_INTERVAL_V2 + Duration::from_secs(hash % (spread_secs + 1))
}
