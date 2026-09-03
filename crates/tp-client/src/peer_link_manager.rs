//! Process-local logical links between tunnel Peers.
//!
//! This module plans Peer relations only. The existing P2P manager and engine
//! remain responsible for signaling, transports, and data-plane installation.
//! While transport protocol v4 remains unchanged, a normalized stable Replica
//! family is the temporary process-local Peer identity.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use thiserror::Error;

/// Tunnel-scoped logical Peer identity, temporarily derived from a Replica family.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeerId(String);

impl PeerId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerDescriptor {
    peer_id: PeerId,
    replicas: Vec<ReplicaDescriptor>,
}

impl PeerDescriptor {
    pub fn from_replica_ids(replica_ids: Vec<String>) -> Result<Self, PeerDescriptorError> {
        if replica_ids.is_empty() {
            return Err(PeerDescriptorError::EmptyReplicaSet);
        }
        let family = crate::p2p::replica::replica_family_id(&replica_ids[0]);
        let mut replicas = Vec::with_capacity(replica_ids.len());
        for replica_id in replica_ids {
            if crate::p2p::replica::replica_family_id(&replica_id) != family {
                return Err(PeerDescriptorError::MixedPeerFamilies);
            }
            let Some(index) = crate::p2p::replica::replica_index(&replica_id) else {
                return Err(PeerDescriptorError::InvalidReplicaId(replica_id));
            };
            replicas.push(ReplicaDescriptor { replica_id, index });
        }
        replicas.sort_by_key(|replica| replica.index);
        if replicas
            .windows(2)
            .any(|pair| pair[0].index == pair[1].index)
        {
            return Err(PeerDescriptorError::DuplicateReplicaIndex);
        }
        Ok(Self {
            peer_id: PeerId(family),
            replicas,
        })
    }

    /// Build a V2 Peer from its issuer-signed stable identity and this
    /// process's actual runtime Replica handles.
    pub fn from_stable_peer_and_replica_ids(
        peer_id: String,
        replica_ids: Vec<String>,
    ) -> Result<Self, PeerDescriptorError> {
        if peer_id.trim().is_empty() {
            return Err(PeerDescriptorError::InvalidPeerId);
        }
        if replica_ids.is_empty() {
            return Err(PeerDescriptorError::EmptyReplicaSet);
        }
        let mut replicas = Vec::with_capacity(replica_ids.len());
        for replica_id in replica_ids {
            let Some(index) = crate::p2p::replica::replica_index(&replica_id) else {
                return Err(PeerDescriptorError::InvalidReplicaId(replica_id));
            };
            replicas.push(ReplicaDescriptor { replica_id, index });
        }
        replicas.sort_by_key(|replica| replica.index);
        if replicas
            .windows(2)
            .any(|pair| pair[0].index == pair[1].index)
        {
            return Err(PeerDescriptorError::DuplicateReplicaIndex);
        }
        Ok(Self {
            peer_id: PeerId(peer_id),
            replicas,
        })
    }

    /// Build the remote V2 membership view. Gateway hints intentionally name
    /// only the stable Peer; the one logical lane targets that identity and
    /// lets the Gateway select one currently attached runtime Replica.
    pub fn from_stable_peer_id(peer_id: String) -> Result<Self, PeerDescriptorError> {
        if peer_id.trim().is_empty() {
            return Err(PeerDescriptorError::InvalidPeerId);
        }
        Ok(Self {
            peer_id: PeerId(peer_id.clone()),
            replicas: vec![ReplicaDescriptor {
                replica_id: peer_id,
                index: 0,
            }],
        })
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReplicaDescriptor {
    replica_id: String,
    index: usize,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PeerDescriptorError {
    #[error("a Peer must contain at least one Replica")]
    EmptyReplicaSet,
    #[error("all Replicas in a Peer descriptor must share one stable family")]
    MixedPeerFamilies,
    #[error("Replica ID does not contain a valid stable-family index: {0}")]
    InvalidReplicaId(String),
    #[error("Replica indexes must be unique within a Peer")]
    DuplicateReplicaIndex,
    #[error("a stable Peer identity must not be empty")]
    InvalidPeerId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MembershipSnapshot {
    peers: Vec<PeerDescriptor>,
}

impl MembershipSnapshot {
    pub fn new(peers: Vec<PeerDescriptor>) -> Self {
        Self { peers }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeerLinkKey {
    first: PeerId,
    second: PeerId,
}

impl PeerLinkKey {
    fn canonical(a: PeerId, b: PeerId) -> Self {
        if a <= b {
            Self {
                first: a,
                second: b,
            }
        } else {
            Self {
                first: b,
                second: a,
            }
        }
    }

    pub fn initiator(&self) -> &PeerId {
        &self.first
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerLinkView {
    key: PeerLinkKey,
    lanes: Vec<PeerLaneView>,
    local_is_initiator: bool,
    membership: MembershipState,
    consecutive_retry_failures: u32,
}

impl PeerLinkView {
    pub fn key(&self) -> &PeerLinkKey {
        &self.key
    }

    pub fn lanes(&self) -> &[PeerLaneView] {
        &self.lanes
    }

    pub fn local_is_initiator(&self) -> bool {
        self.local_is_initiator
    }

    pub fn membership(&self) -> MembershipState {
        self.membership
    }

    pub fn consecutive_retry_failures(&self) -> u32 {
        self.consecutive_retry_failures
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipState {
    Present,
    SuspectMissing,
    Retired,
}

/// Current exact-path availability for one remote Peer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PeerConnectivity {
    pub healthy_direct: bool,
    pub usable_exact_relay: bool,
}

impl PeerConnectivity {
    pub fn unavailable() -> Self {
        Self::default()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeerLaneKey {
    link: PeerLinkKey,
    index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerLaneView {
    key: PeerLaneKey,
    local_replica_id: String,
    remote_replica_id: String,
    local_role: RelationRole,
}

impl PeerLaneView {
    pub fn key(&self) -> &PeerLaneKey {
        &self.key
    }

    pub fn index(&self) -> usize {
        self.key.index
    }

    pub fn local_replica_id(&self) -> &str {
        &self.local_replica_id
    }

    pub fn remote_replica_id(&self) -> &str {
        &self.remote_replica_id
    }

    pub fn local_role(&self) -> RelationRole {
        self.local_role
    }

    pub(crate) fn relation_key(&self) -> PeerRelationKey {
        PeerRelationKey {
            first_peer_family: self.key.link.first.0.clone(),
            second_peer_family: self.key.link.second.0.clone(),
            lane_index: self.key.index,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelationRole {
    Initiator,
    Acceptor,
}

/// Stable identity for one replica-indexed lane in an unordered PeerLink.
///
/// `SessionId` identifies a transport generation; this key survives that
/// generation so pending and installed registries can reject duplicates.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PeerRelationKey {
    pub(crate) first_peer_family: String,
    pub(crate) second_peer_family: String,
    pub(crate) lane_index: usize,
}

impl PeerRelationKey {
    pub(crate) fn from_stable_peers(
        first: &str,
        second: &str,
        lane_index: usize,
    ) -> Result<Self, &'static str> {
        if first.trim().is_empty() || second.trim().is_empty() {
            return Err("empty stable Peer identity");
        }
        if first == second {
            return Err("same stable Peer identity");
        }
        let (first_peer_family, second_peer_family) = if first < second {
            (first.to_string(), second.to_string())
        } else {
            (second.to_string(), first.to_string())
        };
        Ok(Self {
            first_peer_family,
            second_peer_family,
            lane_index,
        })
    }

    pub(crate) fn from_canonical_initiator(
        initiator_replica_id: &str,
        acceptor_replica_id: &str,
    ) -> Result<Self, &'static str> {
        let lane_index = crate::p2p::replica::replica_index(initiator_replica_id)
            .ok_or("invalid initiator Replica")?;
        crate::p2p::replica::replica_index(acceptor_replica_id)
            .ok_or("invalid acceptor Replica")?;
        let initiator_family = crate::p2p::replica::replica_family_id(initiator_replica_id);
        let acceptor_family = crate::p2p::replica::replica_family_id(acceptor_replica_id);
        if initiator_family == acceptor_family {
            return Err("same Peer family");
        }
        if initiator_family > acceptor_family {
            return Err("reverse canonical direction");
        }
        Ok(Self {
            first_peer_family: initiator_family,
            second_peer_family: acceptor_family,
            lane_index,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerLinkCommand {
    EnsureLane(PeerLaneView),
}

#[derive(Debug)]
pub struct PeerLinkManager {
    local_peer: PeerDescriptor,
    configured_replica_count: usize,
    links: BTreeMap<PeerId, PeerLinkView>,
    first_absent_at: BTreeMap<PeerId, Instant>,
    retired_peers: Vec<PeerId>,
}

const ABSENCE_GRACE: Duration = Duration::from_secs(120);

impl PeerLinkManager {
    pub fn new(
        local_peer: PeerDescriptor,
        configured_replica_count: usize,
    ) -> Result<Self, PeerLinkManagerError> {
        if configured_replica_count == 0 {
            return Err(PeerLinkManagerError::ZeroReplicaCount);
        }
        Ok(Self {
            local_peer,
            configured_replica_count,
            links: BTreeMap::new(),
            first_absent_at: BTreeMap::new(),
            retired_peers: Vec::new(),
        })
    }

    pub fn apply_snapshot(&mut self, snapshot: &MembershipSnapshot) -> Vec<PeerLinkCommand> {
        self.apply_snapshot_at(snapshot, Instant::now(), |_| {
            PeerConnectivity::unavailable()
        })
    }

    pub fn apply_snapshot_at<F>(
        &mut self,
        snapshot: &MembershipSnapshot,
        now: Instant,
        connectivity_for: F,
    ) -> Vec<PeerLinkCommand>
    where
        F: Fn(&PeerId) -> PeerConnectivity,
    {
        let mut commands = Vec::new();
        let present: BTreeSet<_> = snapshot
            .peers
            .iter()
            .filter(|peer| peer.peer_id != self.local_peer.peer_id)
            .map(|peer| peer.peer_id.clone())
            .collect();
        for peer in &snapshot.peers {
            if peer.peer_id == self.local_peer.peer_id {
                continue;
            }
            if let Some(link) = self.links.get_mut(&peer.peer_id) {
                let desired =
                    build_peer_link(&self.local_peer, peer, self.configured_replica_count);
                for desired_lane in &desired.lanes {
                    if link
                        .lanes
                        .get(desired_lane.index())
                        .is_none_or(|current_lane| current_lane != desired_lane)
                    {
                        commands.push(PeerLinkCommand::EnsureLane(desired_lane.clone()));
                    }
                }
                // These are desired relation targets, not transport handles.
                // Reconciliation may therefore remember a newly available
                // equal-index Replica without tearing down a healthy lane;
                // the lower manager coalesces an occupied relation and uses
                // the new target only when that relation next needs work.
                link.lanes = desired.lanes;
                link.membership = MembershipState::Present;
                self.first_absent_at.remove(&peer.peer_id);
                continue;
            }
            let link = build_peer_link(&self.local_peer, peer, self.configured_replica_count);
            commands.extend(link.lanes.iter().cloned().map(PeerLinkCommand::EnsureLane));
            self.links.insert(peer.peer_id.clone(), link);
            self.first_absent_at.remove(&peer.peer_id);
        }
        let mut retired = Vec::new();
        for (peer_id, link) in &mut self.links {
            if !present.contains(peer_id) {
                link.membership = MembershipState::SuspectMissing;
                let first_absent_at = self.first_absent_at.entry(peer_id.clone()).or_insert(now);
                let connectivity = connectivity_for(peer_id);
                if now.saturating_duration_since(*first_absent_at) >= ABSENCE_GRACE
                    && !connectivity.healthy_direct
                    && !connectivity.usable_exact_relay
                {
                    link.membership = MembershipState::Retired;
                    if !self.retired_peers.contains(peer_id) {
                        retired.push(peer_id.clone());
                    }
                }
            }
        }
        for peer_id in retired {
            self.retired_peers.push(peer_id);
        }
        commands
    }

    pub fn take_retired_peers(&mut self) -> Vec<PeerId> {
        std::mem::take(&mut self.retired_peers)
    }

    /// Confirm that the upper authority committed this retirement. Until
    /// then, keep both the retired link and its original absence timestamp so
    /// a rejected commit is retried by the next authenticated snapshot.
    pub fn confirm_retired_peer(&mut self, peer_id: &PeerId) -> bool {
        if self
            .links
            .get(peer_id)
            .is_none_or(|link| link.membership != MembershipState::Retired)
        {
            return false;
        }
        self.links.remove(peer_id);
        self.first_absent_at.remove(peer_id);
        self.retired_peers.retain(|pending| pending != peer_id);
        true
    }

    pub fn links(&self) -> Vec<PeerLinkView> {
        self.links.values().cloned().collect()
    }

    pub fn link(&self, peer_id: &PeerId) -> Option<&PeerLinkView> {
        self.links.get(peer_id)
    }

    pub fn record_retry_failure(&mut self, peer_id: &PeerId) -> bool {
        let Some(link) = self.links.get_mut(peer_id) else {
            return false;
        };
        link.consecutive_retry_failures = link.consecutive_retry_failures.saturating_add(1);
        true
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PeerLinkManagerError {
    #[error("configured tunnel replica count must be greater than zero")]
    ZeroReplicaCount,
}

fn build_peer_link(
    local_peer: &PeerDescriptor,
    remote_peer: &PeerDescriptor,
    configured_replica_count: usize,
) -> PeerLinkView {
    let key = PeerLinkKey::canonical(local_peer.peer_id.clone(), remote_peer.peer_id.clone());
    let local_is_initiator = key.first == local_peer.peer_id;
    let (initiator, acceptor) = if local_is_initiator {
        (local_peer, remote_peer)
    } else {
        (remote_peer, local_peer)
    };
    let lanes = (0..configured_replica_count)
        .map(|lane_index| {
            let initiator_replica = replica_for_lane(initiator, lane_index);
            let acceptor_replica = replica_for_lane(acceptor, lane_index);
            let (local_replica_id, remote_replica_id, local_role) = if local_is_initiator {
                (
                    initiator_replica.replica_id.clone(),
                    acceptor_replica.replica_id.clone(),
                    RelationRole::Initiator,
                )
            } else {
                (
                    acceptor_replica.replica_id.clone(),
                    initiator_replica.replica_id.clone(),
                    RelationRole::Acceptor,
                )
            };
            PeerLaneView {
                key: PeerLaneKey {
                    link: key.clone(),
                    index: lane_index,
                },
                local_replica_id,
                remote_replica_id,
                local_role,
            }
        })
        .collect();
    PeerLinkView {
        key,
        lanes,
        local_is_initiator,
        membership: MembershipState::Present,
        consecutive_retry_failures: 0,
    }
}

fn replica_for_lane(peer: &PeerDescriptor, lane_index: usize) -> &ReplicaDescriptor {
    peer.replicas
        .iter()
        .find(|replica| replica.index == lane_index)
        .unwrap_or_else(|| &peer.replicas[lane_index % peer.replicas.len()])
}
