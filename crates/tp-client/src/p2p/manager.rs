//! Orchestrates P2P state transitions on top of the relay session.
//! This skeleton handles Announce/AnnounceAck. Tasks 4.5/4.5b extend
//! handle_message with Offer/Answer/PunchSync/Probe handling.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tp_core::p2p_types::{CertFingerprint, NatHint, SessionId};
use tp_core::protocol::BinaryMessage;
use tp_metrics::{MetricsManager, P2pAttemptResult};

use crate::p2p::installer::P2pSessionInstaller;
use crate::p2p::listener::P2pUnderlayInterfaceIndexes;
use crate::p2p::replica::{replica_index, same_or_child_replica, same_replica_family};
use crate::p2p::session::{ClientRole, MultiSession, P2pState};
use crate::peer_link_manager::{
    MembershipSnapshot, MembershipState, PeerConnectivity, PeerDescriptor, PeerId, PeerLaneView,
    PeerLinkCommand, PeerLinkManager, PeerRelationKey, RelationRole,
};

type MembershipCommitSink = Arc<dyn Fn(&[String]) + Send + Sync>;
type PeerConnectivitySource = Arc<dyn Fn(&PeerId) -> PeerConnectivity + Send + Sync>;
type RetiredPeerSink = Arc<dyn Fn(&PeerId) -> bool + Send + Sync>;
type V2MembershipCycleSink = Arc<dyn Fn(&[String]) -> bool + Send + Sync>;
type V2CurrentPeerAuthoritySource = Arc<dyn Fn(&str) -> bool + Send + Sync>;
type V2MembershipSink = Arc<dyn Fn(&tp_core::provisioning::PublicPeerMembershipV2) + Send + Sync>;
type V2PeerLinkSink =
    Arc<dyn Fn(String, SessionId, tp_core::peer_link_crypto::PeerLinkSessionKeysV2) + Send + Sync>;

const V2_REJECT_MIXED_CANDIDATE_FAMILY: u8 = 2;
const V2_REJECT_RELATION_BUSY: u8 = 3;
const V2_REJECT_PUNCH_SOCKET: u8 = 4;

struct PendingV2Offer {
    offer: tp_core::peer_link_crypto::P2pOfferV2,
    ephemeral_secret: tp_core::peer_link_crypto::PeerLinkEphemeralSecretV2,
}

/// Default initial cooldown after a failed P2P attempt. Mirrors
/// `ClientP2pConfig::default().cooldown_initial_secs` so callers that
/// don't configure cooldown via [`P2pManager::set_cooldown_config`] keep
/// the original hardcoded behavior.
const DEFAULT_COOLDOWN_INITIAL: Duration = Duration::from_secs(60);

/// Default cooldown ceiling. Mirrors
/// `ClientP2pConfig::default().cooldown_max_secs`; same backwards-compat
/// rationale as [`DEFAULT_COOLDOWN_INITIAL`].
const DEFAULT_COOLDOWN_MAX: Duration = Duration::from_secs(600);

/// Re-announce period. The gateway's default `peer_idle_secs` is 120 s,
/// and a 30 s refresh keeps the registry stable even with scheduler jitter.
/// Hardcoded — `peer_idle_secs` is gateway-side config (not part of
/// `ClientP2pConfig`), so plumbing it requires a separate follow-up that
/// crosses the gateway / client boundary.
const REANNOUNCE_INTERVAL_SECS: u64 = 30;
const DEFAULT_ATTEMPT_AFTER_RELAY_UPTIME: Duration = Duration::from_secs(30);
const AUTO_INITIATOR_STATE_POLL: Duration = Duration::from_secs(1);
const MAPPING_PROBE_TIMEOUT: Duration = crate::p2p::mapping_probe::DEFAULT_MAPPING_PROBE_TIMEOUT;
const PRODUCTION_OFFER_ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(not(test))]
const OFFER_ANSWER_TIMEOUT: Duration = PRODUCTION_OFFER_ANSWER_TIMEOUT;
#[cfg(test)]
const OFFER_ANSWER_TIMEOUT: Duration = Duration::from_millis(50);
// The canonical initiator gets one complete Offer/Answer window first. If a
// freshly started canonical acceptor still has no usable lane afterwards, it
// may use the same authenticated V2 Offer/Answer in the reverse direction to
// recover from the other side retaining a process-local stale PeerLink key.
const PRODUCTION_V2_ACCEPTOR_RECOVERY_DELAY: Duration = Duration::from_secs(16);
#[cfg(not(test))]
const V2_ACCEPTOR_RECOVERY_DELAY: Duration = PRODUCTION_V2_ACCEPTOR_RECOVERY_DELAY;
#[cfg(test)]
const V2_ACCEPTOR_RECOVERY_DELAY: Duration = Duration::from_millis(75);
const PUNCH_ACK_TIMEOUT: Duration = Duration::from_secs(3);
const LAN_PUNCH_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const RESPONDER_PROBE_WINDOW: Duration = Duration::from_secs(4);
const RESPONDER_QUIC_ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoInitiatorAttempt {
    Stop,
    RetryAfter(Duration),
}

fn peer_id_for_local_replica(local_client_id: &str, peers: &[String]) -> Option<String> {
    let Some(local_index) = replica_index(local_client_id) else {
        return peers.first().cloned();
    };
    peers
        .iter()
        .find(|peer| replica_index(peer).is_some_and(|index| index == local_index))
        .cloned()
}

#[derive(Debug)]
enum P2pInternalEvent {
    InitiatorAttemptFailed {
        session_id: SessionId,
    },
    CleanupSessionAttempt {
        session_id: SessionId,
    },
    /// A previously installed direct session reached a terminal transport
    /// state.  The exact session id is the generation fence: stale duplicate
    /// notifications cannot release a replacement relation.
    RelationClosed {
        session_id: SessionId,
    },
    OfferAnswerTimedOut {
        session_id: SessionId,
        cancel: CancellationToken,
    },
    SessionInstalled {
        session_id: SessionId,
    },
    RefillRequested {
        peer_client_id: String,
    },
}

#[derive(Clone)]
pub(crate) struct P2pRefillHandle {
    tx: mpsc::UnboundedSender<P2pInternalEvent>,
}

impl P2pRefillHandle {
    pub(crate) fn request_refill(&self, peer_client_id: &str) {
        let _ = self.tx.send(P2pInternalEvent::RefillRequested {
            peer_client_id: peer_client_id.to_string(),
        });
    }

    pub(crate) fn relation_closed(&self, session_id: SessionId) {
        let _ = self
            .tx
            .send(P2pInternalEvent::RelationClosed { session_id });
    }
}

#[derive(Clone, Debug, Default)]
struct PeerContext {
    candidates: Vec<tp_core::p2p_types::Candidate>,
    cert_fp: Option<tp_core::p2p_types::CertFingerprint>,
    #[allow(dead_code)]
    peer_client_id: Option<String>,
    local_client_id: Option<String>,
    allow_parallel: bool,
    family: Option<P2pAddressFamily>,
    fallback_family: Option<P2pAddressFamily>,
    /// Temporary role for this exact Offer/Answer session. `None` is invalid
    /// for PunchSync and fails closed instead of consulting manager-wide role.
    session_role: Option<ClientRole>,
    mesh_relation_key: Option<MeshRelationKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum P2pAddressFamily {
    Ipv6,
    Ipv4,
}

type MeshRelationKey = PeerRelationKey;

impl P2pAddressFamily {
    fn label(self) -> &'static str {
        match self {
            Self::Ipv6 => "ipv6",
            Self::Ipv4 => "ipv4",
        }
    }
}

#[derive(Clone, Debug)]
struct FamilyFallbackAttempt {
    peer_client_id: String,
    local_client_id: Option<String>,
    family: P2pAddressFamily,
}

pub struct P2pManager {
    multi: Arc<MultiSession>,
    client_id: String,
    group_id: String,
    cert_fp: CertFingerprint,
    tls_identity: Option<crate::p2p::cert::CertBundle>,
    /// Legacy auto-bootstrap policy only; established session behavior is
    /// determined by `PeerContext::session_role`.
    role: ClientRole,
    inbound: mpsc::Receiver<BinaryMessage>,
    outbound: mpsc::Sender<BinaryMessage>,
    internal_tx: mpsc::UnboundedSender<P2pInternalEvent>,
    internal_rx: mpsc::UnboundedReceiver<P2pInternalEvent>,
    p2p_local_port: u16,
    peer_candidates_cache: Vec<tp_core::p2p_types::Candidate>,
    peer_cert_fp_cache: Option<tp_core::p2p_types::CertFingerprint>,
    /// Populated by Task 4.5b acceptor branch; consumed by Task 4.6 punch driver.
    #[allow(dead_code)]
    peer_client_id_cache: Option<String>,
    peer_contexts: HashMap<SessionId, PeerContext>,
    /// Legacy read-back slot retained for older callers/tests. Production
    /// listener validation uses `expected_peer_map`, keyed by session_id.
    expected_fp_handle: Option<Arc<Mutex<Option<CertFingerprint>>>>,
    /// Legacy/test hook retained for stale-session regression coverage.
    /// Production listener installs the matched session_id supplied by the
    /// keyed expected-peer map.
    expected_session_id_handle: Option<Arc<Mutex<Option<SessionId>>>>,
    expected_peer_map: Option<crate::p2p::expected::ExpectedPeerMap>,
    observed_public_addr: Option<std::net::SocketAddr>,
    failure_count: u32,
    /// Optional metrics sink (Task 4.12). `None` = no-op. Set by Task 4.11
    /// at construction via [`P2pManager::set_metrics`].
    metrics: Option<Arc<MetricsManager>>,
    /// Cancellation token for the in-flight initiator punch task,
    /// keyed by session_id. The spawned task checks the token immediately
    /// before installing the QUIC session into the bounded P2P registry.
    /// The `P2pTeardown` handler cancels the matching token so a teardown
    /// arriving mid-handshake cannot be overwritten by a late install
    /// (zombie session + bypassed cooldown). Pre-fix the install path was
    /// race-prone: the handler cleared the single P2P session and stamped
    /// Cooldown, then the spawned task completed and clobbered both.
    active_punch_cancel: HashMap<SessionId, CancellationToken>,
    /// Acceptor-side UDP sockets reserved during `P2pOffer` and later
    /// consumed by `P2pPunchSync`. The same socket answers Probe frames and
    /// then becomes the QUIC server endpoint, so the NAT mapping observed by
    /// the initiator is the mapping it actually dials.
    acceptor_punch_sockets: HashMap<SessionId, std::net::UdpSocket>,
    initiator_punch_sockets: HashMap<SessionId, std::net::UdpSocket>,
    acceptor_responder_started: HashSet<SessionId>,
    /// Clone of the long-lived P2P QUIC listener socket. Mapping probes for
    /// initiator offers must originate from this socket because its public
    /// endpoint is what gets published in `P2pOffer`.
    listener_probe_socket: Option<std::net::UdpSocket>,
    listener_observed_public_addr: Option<std::net::SocketAddr>,
    /// Optional UDP reflector used to discover the real public mapping of
    /// each P2P UDP socket before publishing server-reflexive candidates.
    mapping_probe_reflector: Option<std::net::SocketAddr>,
    mapping_probe_timeout: Duration,
    /// Per-session timeout for the Offer→Answer→PunchSync signaling phase.
    /// Both sides keep it armed until PunchSync (or a completed install) so a
    /// Gateway reconnect window cannot leave a relation reservation occupied
    /// forever.
    pending_answer_cancel: HashMap<SessionId, CancellationToken>,
    /// Structured-cancellation cohort for every task spawned by the
    /// manager (initiator punch driver, acceptor probe responder). On
    /// `run()` exit the tracker is closed + awaited so the manager
    /// doesn't return while spawned punch/responder tasks still hold
    /// `Arc<MultiSession>` and `mpsc::Sender<BinaryMessage>` clones.
    /// Pre-fix these were bare `tokio::spawn` and orphaned for ~5–6.5 s
    /// each on shutdown (bounded leak but observable in tests).
    task_tracker: TaskTracker,
    p2p_installer: Option<P2pSessionInstaller>,
    /// Initial cooldown after a failed P2P attempt; doubled per
    /// failure up to `cooldown_max`. Defaults to a hardcoded 60 s; the
    /// apps layer overrides via [`set_cooldown_config`] from
    /// `ClientP2pConfig`.
    cooldown_initial: Duration,
    /// Cooldown ceiling for exponential backoff. Defaults to 600 s;
    /// configurable via [`set_cooldown_config`].
    cooldown_max: Duration,
    /// Source-side automatic P2P target. `None` preserves acceptor/manual
    /// behavior; `Some("")` is a configured initiator still waiting for a
    /// gateway-supplied peer hint.
    auto_peer_client_id: Option<String>,
    auto_peer_client_ids: Vec<String>,
    forced_refill_peers: VecDeque<String>,
    forced_refill_peer_set: HashSet<String>,
    family_fallback_queue: VecDeque<FamilyFallbackAttempt>,
    family_fallback_set: HashSet<(String, Option<String>, P2pAddressFamily)>,
    /// Delay after the first successful relay announce before an initiator
    /// sends the first P2P offer.
    attempt_after_relay_uptime: Duration,
    empty_peer_warned: bool,
    allow_lan_candidates: bool,
    /// Set by the Gateway's announce Ack. Port zero is reserved for the
    /// isolated Static Relay clone and makes V2 PeerLink negotiation use the
    /// existing signed zero-candidate path, so no Direct socket is created.
    gateway_direct_lane_enabled: bool,
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
    underlay_host_ips: BTreeSet<IpAddr>,
    peer_link_manager: Option<PeerLinkManager>,
    membership_commit_sink: Option<MembershipCommitSink>,
    peer_connectivity_source: Option<PeerConnectivitySource>,
    retired_peer_sink: Option<RetiredPeerSink>,
    pending_membership_replicas: BTreeMap<String, BTreeSet<String>>,
    pending_peer_link_commands: VecDeque<PeerLinkCommand>,
    mesh_relation_lanes: HashMap<MeshRelationKey, PeerLaneView>,
    /// Canonical initiator is the normal writer for a PeerLink generation.
    /// A fresh acceptor arms one bounded fallback so a one-sided restart can
    /// recover even when the surviving initiator still holds the old key.
    v2_acceptor_recovery_not_before: HashMap<MeshRelationKey, std::time::Instant>,
    /// Present only for a Lantunnel 2.0 `.peer` connection. The existing
    /// manager then emits and verifies V2 signaling instead of legacy
    /// unauthenticated Offer/Answer bodies.
    v2_profile: Option<Arc<tp_core::provisioning::PeerProfileV2>>,
    pending_v2_offers: HashMap<SessionId, PendingV2Offer>,
    v2_membership_cycle_sink: Option<V2MembershipCycleSink>,
    v2_current_peer_authority_source: Option<V2CurrentPeerAuthoritySource>,
    v2_membership_sink: Option<V2MembershipSink>,
    v2_peer_link_sink: Option<V2PeerLinkSink>,
}

impl P2pManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        multi: Arc<MultiSession>,
        client_id: String,
        group_id: String,
        cert_fp: CertFingerprint,
        role: ClientRole,
        inbound: mpsc::Receiver<BinaryMessage>,
        outbound: mpsc::Sender<BinaryMessage>,
        p2p_local_port: u16,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            multi,
            client_id,
            group_id,
            cert_fp,
            tls_identity: None,
            role,
            inbound,
            outbound,
            internal_tx,
            internal_rx,
            p2p_local_port,
            peer_candidates_cache: vec![],
            peer_cert_fp_cache: None,
            peer_client_id_cache: None,
            peer_contexts: HashMap::new(),
            expected_fp_handle: None,
            expected_session_id_handle: None,
            expected_peer_map: None,
            observed_public_addr: None,
            failure_count: 0,
            metrics: None,
            active_punch_cancel: HashMap::new(),
            acceptor_punch_sockets: HashMap::new(),
            initiator_punch_sockets: HashMap::new(),
            acceptor_responder_started: HashSet::new(),
            listener_probe_socket: None,
            listener_observed_public_addr: None,
            mapping_probe_reflector: crate::p2p::mapping_probe::mapping_probe_addr_from_env(),
            mapping_probe_timeout: MAPPING_PROBE_TIMEOUT,
            pending_answer_cancel: HashMap::new(),
            task_tracker: TaskTracker::new(),
            p2p_installer: None,
            cooldown_initial: DEFAULT_COOLDOWN_INITIAL,
            cooldown_max: DEFAULT_COOLDOWN_MAX,
            auto_peer_client_id: None,
            auto_peer_client_ids: Vec::new(),
            forced_refill_peers: VecDeque::new(),
            forced_refill_peer_set: HashSet::new(),
            family_fallback_queue: VecDeque::new(),
            family_fallback_set: HashSet::new(),
            attempt_after_relay_uptime: DEFAULT_ATTEMPT_AFTER_RELAY_UPTIME,
            empty_peer_warned: false,
            allow_lan_candidates: false,
            gateway_direct_lane_enabled: true,
            underlay_interface_indexes: None,
            underlay_host_ips: BTreeSet::new(),
            peer_link_manager: None,
            membership_commit_sink: None,
            peer_connectivity_source: None,
            retired_peer_sink: None,
            pending_membership_replicas: BTreeMap::new(),
            pending_peer_link_commands: VecDeque::new(),
            mesh_relation_lanes: HashMap::new(),
            v2_acceptor_recovery_not_before: HashMap::new(),
            v2_profile: None,
            pending_v2_offers: HashMap::new(),
            v2_membership_cycle_sink: None,
            v2_current_peer_authority_source: None,
            v2_membership_sink: None,
            v2_peer_link_sink: None,
        }
    }

    pub fn set_v2_profile(&mut self, profile: Arc<tp_core::provisioning::PeerProfileV2>) {
        self.v2_profile = Some(profile);
        self.pending_v2_offers.clear();
        self.v2_acceptor_recovery_not_before.clear();
    }

    pub fn set_v2_membership_sink<F>(&mut self, sink: F)
    where
        F: Fn(&tp_core::provisioning::PublicPeerMembershipV2) + Send + Sync + 'static,
    {
        self.v2_membership_sink = Some(Arc::new(sink));
    }

    pub fn set_v2_membership_cycle_sink<F>(&mut self, sink: F)
    where
        F: Fn(&[String]) -> bool + Send + Sync + 'static,
    {
        self.v2_membership_cycle_sink = Some(Arc::new(sink));
    }

    pub fn set_v2_current_peer_authority_source<F>(&mut self, source: F)
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.v2_current_peer_authority_source = Some(Arc::new(source));
    }

    pub fn set_v2_peer_link_sink<F>(&mut self, sink: F)
    where
        F: Fn(String, SessionId, tp_core::peer_link_crypto::PeerLinkSessionKeysV2)
            + Send
            + Sync
            + 'static,
    {
        self.v2_peer_link_sink = Some(Arc::new(sink));
    }

    pub fn set_peer_link_manager(&mut self, manager: PeerLinkManager) {
        self.peer_link_manager = Some(manager);
        self.pending_membership_replicas.clear();
        self.pending_peer_link_commands.clear();
        self.mesh_relation_lanes.clear();
        self.v2_acceptor_recovery_not_before.clear();
    }

    pub fn set_membership_commit_sink<F>(&mut self, sink: F)
    where
        F: Fn(&[String]) + Send + Sync + 'static,
    {
        self.membership_commit_sink = Some(Arc::new(sink));
    }

    pub fn set_peer_connectivity_source<F>(&mut self, source: F)
    where
        F: Fn(&PeerId) -> PeerConnectivity + Send + Sync + 'static,
    {
        self.peer_connectivity_source = Some(Arc::new(source));
    }

    pub fn set_retired_peer_sink<F>(&mut self, sink: F)
    where
        F: Fn(&PeerId) -> bool + Send + Sync + 'static,
    {
        self.retired_peer_sink = Some(Arc::new(sink));
    }

    pub fn drain_peer_link_commands(&mut self) -> Vec<PeerLinkCommand> {
        self.pending_peer_link_commands.drain(..).collect()
    }

    pub fn peer_link_membership(&self, peer_id: &PeerId) -> Option<MembershipState> {
        self.peer_link_manager
            .as_ref()?
            .link(peer_id)
            .map(|link| link.membership())
    }

    fn buffer_membership_replica(&mut self, replica_id: String) {
        let replica_id = replica_id.trim().to_string();
        if replica_id.is_empty() {
            return;
        }
        let family = crate::p2p::replica::replica_family_id(&replica_id);
        self.pending_membership_replicas
            .entry(family)
            .or_default()
            .insert(replica_id);
    }

    fn commit_membership_cycle(&mut self) -> bool {
        self.commit_membership_cycle_at(std::time::Instant::now())
    }

    fn commit_membership_cycle_at(&mut self, now: std::time::Instant) -> bool {
        let replica_families = std::mem::take(&mut self.pending_membership_replicas);
        let mut committed_replica_ids = Vec::new();
        let v2_peer_link = self.v2_profile.is_some();
        let peers = replica_families
            .into_iter()
            .filter_map(|(peer_identity, replica_ids)| {
                let replica_ids = replica_ids.into_iter().collect::<Vec<_>>();
                let descriptor = if v2_peer_link {
                    PeerDescriptor::from_stable_peer_id(peer_identity)
                } else {
                    PeerDescriptor::from_replica_ids(replica_ids.clone())
                };
                match descriptor {
                    Ok(peer) => {
                        if !v2_peer_link {
                            committed_replica_ids.extend(replica_ids);
                        }
                        Some(peer)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "invalid Peer in mesh membership cycle ignored");
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        if v2_peer_link {
            let committed_peer_ids = peers
                .iter()
                .map(|peer| peer.peer_id().as_str().to_owned())
                .collect::<Vec<_>>();
            // Publish the exact authenticated Gateway cycle before retirement
            // evaluates connectivity. A Peer absent from this cycle cannot
            // keep itself alive merely because an old Relay key remains in
            // memory; a healthy Direct lane can still preserve it.
            if let Some(sink) = &self.v2_membership_cycle_sink {
                if !sink(&committed_peer_ids) {
                    return false;
                }
            }
        }
        let connectivity_source = self.peer_connectivity_source.clone();
        let mut retired_peers = Vec::new();
        let commands = self
            .peer_link_manager
            .as_mut()
            .map_or_else(Vec::new, |manager| {
                let commands =
                    manager.apply_snapshot_at(&MembershipSnapshot::new(peers), now, |peer_id| {
                        connectivity_source
                            .as_ref()
                            .map_or_else(PeerConnectivity::unavailable, |source| source(peer_id))
                    });
                retired_peers = manager.take_retired_peers();
                commands
            });
        for command in commands {
            self.enqueue_peer_link_command(command);
        }
        let committed_retirements = retired_peers
            .iter()
            .filter(|peer_id| {
                self.retired_peer_sink
                    .as_ref()
                    .is_none_or(|sink| sink(peer_id))
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(manager) = self.peer_link_manager.as_mut() {
            for peer_id in &committed_retirements {
                manager.confirm_retired_peer(peer_id);
            }
        }
        if let Some(sink) = &self.membership_commit_sink {
            sink(&committed_replica_ids);
        }
        true
    }

    fn mesh_relation_is_occupied(&self, key: &MeshRelationKey) -> bool {
        self.peer_contexts
            .values()
            .any(|context| context.mesh_relation_key.as_ref() == Some(key))
            || self
                .p2p_installer
                .as_ref()
                .is_some_and(|installer| installer.has_live_or_pending_relation(key))
    }

    fn crossed_v2_relay_only_offer(
        &self,
        incoming: &tp_core::peer_link_crypto::P2pOfferV2,
    ) -> Option<(SessionId, RelationRole)> {
        if self.peer_link_manager.is_none() || !incoming.candidates.is_empty() {
            return None;
        }
        let relation = MeshRelationKey::from_stable_peers(
            &incoming.source_peer_id,
            &incoming.target_peer_id,
            0,
        )
        .ok()?;
        let pending_session = self
            .pending_v2_offers
            .iter()
            .find_map(|(session_id, pending)| {
                (pending.offer.candidates.is_empty()
                    && pending.offer.source_peer_id == incoming.target_peer_id
                    && pending.offer.target_peer_id == incoming.source_peer_id
                    && self
                        .peer_contexts
                        .get(session_id)
                        .and_then(|context| context.mesh_relation_key.as_ref())
                        == Some(&relation))
                .then_some(*session_id)
            })?;
        let incoming_role = if incoming.source_peer_id == relation.first_peer_family {
            RelationRole::Initiator
        } else {
            RelationRole::Acceptor
        };
        Some((pending_session, incoming_role))
    }

    fn mesh_relation_key_for_lane(lane: &PeerLaneView) -> Result<MeshRelationKey, &'static str> {
        Ok(lane.relation_key())
    }

    fn enqueue_peer_link_command(&mut self, command: PeerLinkCommand) {
        let PeerLinkCommand::EnsureLane(lane) = &command;
        let Ok(key) = Self::mesh_relation_key_for_lane(lane) else {
            tracing::warn!(
                local_replica_id = %lane.local_replica_id(),
                remote_replica_id = %lane.remote_replica_id(),
                lane_index = lane.index(),
                "invalid mesh EnsureLane relation discarded"
            );
            return;
        };
        // Membership reconciliation may change the physical Replica target
        // while an exact relation is waiting for its local relay to recover.
        // Keep one queued command per canonical relation, but replace its
        // payload so the next retry cannot restore an older modulo target.
        self.mesh_relation_lanes.insert(key.clone(), lane.clone());
        let already_pending = self.pending_peer_link_commands.iter_mut().find(|pending| {
            let PeerLinkCommand::EnsureLane(pending_lane) = pending;
            Self::mesh_relation_key_for_lane(pending_lane).as_ref() == Ok(&key)
        });
        if let Some(pending) = already_pending {
            *pending = command;
        } else {
            self.pending_peer_link_commands.push_back(command);
        }
    }

    /// Wire the legacy expected-peer-fingerprint read-back slot.
    pub fn set_expected_fp_handle(&mut self, h: Arc<Mutex<Option<CertFingerprint>>>) {
        self.expected_fp_handle = Some(h);
    }

    /// Wire the legacy expected-`session_id` hook used by focused tests and
    /// older embedders. New listener code gets the session_id from the keyed
    /// expected-peer map instead.
    pub fn set_expected_session_id_handle(&mut self, h: Arc<Mutex<Option<SessionId>>>) {
        self.expected_session_id_handle = Some(h);
    }

    pub fn set_expected_peer_map(&mut self, expected: crate::p2p::expected::ExpectedPeerMap) {
        self.expected_peer_map = Some(expected);
    }

    pub fn set_listener_probe_socket(&mut self, socket: std::net::UdpSocket) {
        if let Err(e) = socket.set_nonblocking(true) {
            tracing::debug!(
                error = %e,
                "failed to set P2P listener probe socket nonblocking"
            );
        }
        self.listener_probe_socket = Some(socket);
    }

    #[cfg(test)]
    fn set_listener_probe_socket_for_test(&mut self, socket: std::net::UdpSocket) {
        self.set_listener_probe_socket(socket);
    }

    pub fn set_listener_observed_public_addr(&mut self, addr: Option<std::net::SocketAddr>) {
        self.listener_observed_public_addr = addr.filter(|addr| is_public_p2p_ip(addr.ip()));
    }

    #[cfg(test)]
    fn set_listener_observed_public_addr_for_test(&mut self, addr: Option<std::net::SocketAddr>) {
        self.set_listener_observed_public_addr(addr);
    }

    #[cfg(test)]
    fn set_mapping_probe_reflector_for_test(&mut self, reflector: Option<std::net::SocketAddr>) {
        self.set_mapping_probe_reflector(reflector);
    }

    pub(crate) fn set_mapping_probe_reflector(&mut self, reflector: Option<std::net::SocketAddr>) {
        self.mapping_probe_reflector = reflector;
    }

    #[cfg(test)]
    fn set_mapping_probe_timeout_for_test(&mut self, timeout: Duration) {
        self.mapping_probe_timeout = timeout;
    }

    /// Install an optional metrics sink. `None` = no-op. Wired by
    /// `apps/lantunnel-client/src-tauri/src/main.rs` at startup (Task 4.11).
    pub fn set_metrics(&mut self, metrics: Option<Arc<MetricsManager>>) {
        self.metrics = metrics;
    }

    pub fn set_allow_lan_candidates(&mut self, allow: bool) {
        self.allow_lan_candidates = allow;
    }

    pub fn set_underlay_interface_indexes(
        &mut self,
        ipv4: Option<NonZeroU32>,
        ipv6: Option<NonZeroU32>,
        ipv4_source_ip: Option<std::net::Ipv4Addr>,
        host_ips: impl IntoIterator<Item = IpAddr>,
    ) {
        self.underlay_interface_indexes = Some(P2pUnderlayInterfaceIndexes {
            ipv4,
            ipv6,
            ipv4_source_ip,
        });
        self.underlay_host_ips = host_ips
            .into_iter()
            .filter(|ip| !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast())
            .filter(|ip| {
                !cfg!(target_os = "macos")
                    || !ip.is_ipv4()
                    || Some(*ip) == ipv4_source_ip.map(IpAddr::V4)
            })
            .collect();
    }

    fn filter_local_candidates_for_underlay(
        &self,
        candidates: Vec<tp_core::p2p_types::Candidate>,
    ) -> Vec<tp_core::p2p_types::Candidate> {
        let selected_hosts = self
            .underlay_interface_indexes
            .map(|_| &self.underlay_host_ips);
        filter_local_host_candidates_for_underlay(candidates, selected_hosts)
    }

    pub fn set_session_installer(&mut self, installer: P2pSessionInstaller) {
        self.p2p_installer = Some(installer);
    }

    pub(crate) fn refill_handle(&self) -> P2pRefillHandle {
        P2pRefillHandle {
            tx: self.internal_tx.clone(),
        }
    }

    pub fn set_tls_identity(&mut self, identity: &crate::p2p::cert::CertBundle) {
        self.tls_identity = Some(identity.clone());
    }

    /// Override the cooldown backoff parameters from
    /// `tp_core::config::ClientP2pConfig`. Called by the apps layer once
    /// at construction by the unified headless Client. Both args are seconds;
    /// conversion to `Duration`
    /// happens here so callers don't have to import `std::time::Duration`.
    pub fn set_cooldown_config(&mut self, initial_secs: u64, max_secs: u64) {
        self.cooldown_initial = Duration::from_secs(initial_secs);
        self.cooldown_max = Duration::from_secs(max_secs);
    }

    pub fn set_auto_initiate_peer(
        &mut self,
        peer_client_id: String,
        attempt_after_relay_uptime_secs: u64,
    ) {
        self.set_auto_initiate_peers(vec![peer_client_id], attempt_after_relay_uptime_secs);
    }

    pub(crate) fn set_auto_initiate_peers(
        &mut self,
        peer_client_ids: Vec<String>,
        attempt_after_relay_uptime_secs: u64,
    ) {
        self.auto_peer_client_ids.clear();
        for peer in peer_client_ids {
            self.add_auto_peer_client_id(peer, false);
        }
        self.auto_peer_client_id = Some(
            self.auto_peer_client_ids
                .first()
                .cloned()
                .unwrap_or_default(),
        );
        self.attempt_after_relay_uptime = Duration::from_secs(attempt_after_relay_uptime_secs);
    }

    #[cfg(test)]
    fn set_auto_initiate_peers_for_test(
        &mut self,
        peer_client_ids: Vec<String>,
        attempt_after_relay_uptime_secs: u64,
    ) {
        self.set_auto_initiate_peers(peer_client_ids, attempt_after_relay_uptime_secs);
    }

    fn add_auto_peer_client_id(&mut self, peer_client_id: String, prefer_primary: bool) -> bool {
        let peer_client_id = peer_client_id.trim().to_string();
        if peer_client_id.is_empty() {
            return false;
        }
        if self
            .auto_peer_client_ids
            .iter()
            .any(|existing| existing == &peer_client_id)
        {
            if prefer_primary {
                self.auto_peer_client_id = Some(peer_client_id);
            }
            return false;
        }
        if prefer_primary {
            self.auto_peer_client_ids.insert(0, peer_client_id.clone());
            self.auto_peer_client_id = Some(peer_client_id);
        } else {
            self.auto_peer_client_ids.push(peer_client_id);
        }
        true
    }

    fn prune_stale_same_replica_peer_hint_candidates(&mut self, peer_client_id: &str) -> bool {
        let Some(peer_index) = replica_index(peer_client_id) else {
            return false;
        };

        let before_len = self.auto_peer_client_ids.len();
        self.auto_peer_client_ids.retain(|existing| {
            existing == peer_client_id
                || replica_index(existing) != Some(peer_index)
                || same_replica_family(existing, peer_client_id)
        });
        let pruned_list = self.auto_peer_client_ids.len() != before_len;

        let replace_primary = self.auto_peer_client_id.as_deref().is_some_and(|current| {
            current != peer_client_id
                && replica_index(current) == Some(peer_index)
                && !same_replica_family(current, peer_client_id)
        });
        if replace_primary {
            self.auto_peer_client_id = Some(peer_client_id.to_string());
        }

        pruned_list || replace_primary
    }

    fn auto_peer_client_ids(&self) -> Vec<String> {
        let mut peers = Vec::new();
        for peer in &self.auto_peer_client_ids {
            let peer = peer.trim();
            if !peer.is_empty() && !peers.iter().any(|existing: &String| existing == peer) {
                peers.push(peer.to_string());
            }
        }
        if let Some(peer) = self.auto_peer_client_id.as_deref() {
            let peer = peer.trim();
            if !peer.is_empty() && !peers.iter().any(|existing| existing == peer) {
                peers.insert(0, peer.to_string());
            }
        }
        peers
    }

    fn enqueue_forced_refill_peer(&mut self, peer_client_id: String) {
        let peer_client_id = peer_client_id.trim().to_string();
        if peer_client_id.is_empty() || !self.forced_refill_peer_set.insert(peer_client_id.clone())
        {
            return;
        }
        self.forced_refill_peers.push_back(peer_client_id);
    }

    fn take_forced_refill_peers(&mut self) -> Vec<String> {
        let mut peers = Vec::new();
        while let Some(peer) = self.forced_refill_peers.pop_front() {
            self.forced_refill_peer_set.remove(&peer);
            peers.push(peer);
        }
        peers
    }

    fn enqueue_family_fallback(
        &mut self,
        peer_client_id: String,
        local_client_id: Option<String>,
        family: P2pAddressFamily,
    ) {
        let key = (peer_client_id.clone(), local_client_id.clone(), family);
        if self.family_fallback_set.insert(key) {
            self.family_fallback_queue.push_back(FamilyFallbackAttempt {
                peer_client_id,
                local_client_id,
                family,
            });
        }
    }

    fn take_family_fallbacks(&mut self) -> Vec<FamilyFallbackAttempt> {
        let mut attempts = Vec::new();
        while let Some(attempt) = self.family_fallback_queue.pop_front() {
            self.family_fallback_set.remove(&(
                attempt.peer_client_id.clone(),
                attempt.local_client_id.clone(),
                attempt.family,
            ));
            attempts.push(attempt);
        }
        attempts
    }

    fn family_fallback_for_session(&self, session_id: SessionId) -> Option<FamilyFallbackAttempt> {
        let ctx = self.peer_contexts.get(&session_id)?;
        Some(FamilyFallbackAttempt {
            peer_client_id: ctx.peer_client_id.clone()?,
            local_client_id: ctx.local_client_id.clone(),
            family: ctx.fallback_family?,
        })
    }

    fn peer_has_blocking_context(
        &mut self,
        peer_client_id: &str,
        installed_sessions_block: bool,
    ) -> bool {
        let mut stale_sessions = Vec::new();
        let mut has_live_or_in_flight = false;
        for (session_id, ctx) in &self.peer_contexts {
            let matches_peer = ctx
                .peer_client_id
                .as_deref()
                .map(|peer| peer == peer_client_id)
                .unwrap_or(false);
            if !matches_peer {
                continue;
            }
            if self.session_context_is_live_or_in_flight(*session_id, installed_sessions_block) {
                has_live_or_in_flight = true;
            } else {
                stale_sessions.push(*session_id);
            }
        }
        for session_id in stale_sessions {
            self.cleanup_session_attempt(session_id);
        }
        has_live_or_in_flight
    }

    fn session_context_is_live_or_in_flight(
        &self,
        session_id: SessionId,
        installed_sessions_block: bool,
    ) -> bool {
        let state_blocks = match self.multi.p2p_state() {
            P2pState::Negotiating { session_id: active }
            | P2pState::Punching {
                session_id: active, ..
            }
            | P2pState::HandshakingQuic { session_id: active } => active == session_id,
            P2pState::Active {
                session_id: active, ..
            } => installed_sessions_block && active == session_id,
            _ => false,
        };
        state_blocks
            || self
                .p2p_installer
                .as_ref()
                .map(|installer| {
                    installer.has_reserved_session(session_id)
                        || (installed_sessions_block && installer.has_installed_session(session_id))
                })
                .unwrap_or(false)
    }

    fn p2p_install_counts(&self) -> Option<(usize, usize, usize, usize)> {
        let installer = self.p2p_installer.as_ref()?;
        let desired = installer.desired_session_count();
        let active = installer.active_session_count();
        let pending = installer.pending_session_count();
        let occupied = active.saturating_add(pending);
        Some((desired, active, pending, desired.saturating_sub(occupied)))
    }

    async fn try_initiate_missing_peers(&mut self, peers: Vec<String>) -> usize {
        self.try_initiate_missing_peers_inner(peers, true).await
    }

    async fn try_initiate_forced_refill_peers(&mut self, peers: Vec<String>) -> usize {
        self.try_initiate_missing_peers_inner(peers, false).await
    }

    async fn try_initiate_missing_peers_inner(
        &mut self,
        peers: Vec<String>,
        installed_sessions_block: bool,
    ) -> usize {
        let (desired, active, pending, mut remaining) =
            self.p2p_install_counts().unwrap_or((1, 0, 0, 1));
        if remaining == 0 {
            tracing::debug!(
                desired,
                active,
                pending,
                peer_count = peers.len(),
                peers = ?peers,
                "p2p refill skipped without install deficit"
            );
            return 0;
        }
        if let Some(installer) = self.p2p_installer.clone() {
            let mut local_slots = installer.available_install_client_ids();
            local_slots.sort_by(|a, b| {
                replica_index(a)
                    .unwrap_or(usize::MAX)
                    .cmp(&replica_index(b).unwrap_or(usize::MAX))
                    .then_with(|| a.cmp(b))
            });
            if !local_slots.is_empty() {
                tracing::debug!(
                    desired,
                    active,
                    pending,
                    remaining,
                    local_slots = ?local_slots,
                    peers = ?peers,
                    installed_sessions_block,
                    "p2p refill evaluating local install slots"
                );
                let mut attempted = 0usize;
                for local_client_id in local_slots {
                    if remaining == 0 {
                        break;
                    }
                    let Some(peer_client_id) = peer_id_for_local_replica(&local_client_id, &peers)
                    else {
                        tracing::debug!(
                            local_client_id = %local_client_id,
                            peers = ?peers,
                            "p2p refill skipped local slot without same-index peer"
                        );
                        continue;
                    };
                    if self.peer_has_blocking_context(&peer_client_id, installed_sessions_block) {
                        tracing::debug!(
                            peer_client_id = %peer_client_id,
                            local_client_id = %local_client_id,
                            installed_sessions_block,
                            "p2p refill skipped peer with blocking context"
                        );
                        continue;
                    }
                    if let Err(e) = self
                        .try_initiate_for_local_slot(&peer_client_id, Some(&local_client_id))
                        .await
                    {
                        tracing::warn!(peer_client_id = %peer_client_id, local_client_id = %local_client_id, error = %e, "p2p initiator offer failed");
                    } else {
                        attempted += 1;
                        remaining = remaining.saturating_sub(1);
                    }
                }
                if attempted == 0 {
                    tracing::debug!(
                        desired,
                        active,
                        pending,
                        remaining,
                        peers = ?peers,
                        installed_sessions_block,
                        "p2p refill made no attempts from local slots"
                    );
                }
                return attempted;
            }
            tracing::debug!(
                desired,
                active,
                pending,
                remaining,
                peers = ?peers,
                "p2p refill has no local install slots; trying primary fallback"
            );
        }

        let mut attempted = 0usize;
        if let Some(peer_client_id) = peers.into_iter().next() {
            if self.peer_has_blocking_context(&peer_client_id, installed_sessions_block) {
                tracing::debug!(
                    peer_client_id = %peer_client_id,
                    installed_sessions_block,
                    "p2p refill primary fallback skipped peer with blocking context"
                );
                return 0;
            }
            if remaining > 0 {
                if let Err(e) = self.try_initiate(&peer_client_id).await {
                    tracing::warn!(peer_client_id = %peer_client_id, error = %e, "p2p initiator offer failed");
                } else {
                    attempted += 1;
                }
            } else {
                return 0;
            }
        }
        attempted
    }

    fn fallback_observed_endpoint(&self, local_port: u16) -> Option<std::net::SocketAddr> {
        self.observed_public_addr
            .filter(|addr| is_public_p2p_ip(addr.ip()))
            .map(|addr| std::net::SocketAddr::new(addr.ip(), local_port))
    }

    #[allow(dead_code)]
    async fn probe_listener_public_endpoint(
        &self,
        session_id: SessionId,
        peer_client_id: &str,
    ) -> Option<std::net::SocketAddr> {
        if let Some(observed) = self.listener_observed_public_addr {
            tracing::info!(
                client_id = %self.client_id,
                group_id = %self.group_id,
                peer_client_id = %peer_client_id,
                ?session_id,
                local_port = self.p2p_local_port,
                observed = %observed,
                "p2p listener mapping probe cached"
            );
            return Some(observed);
        }
        let Some(socket) = self.listener_probe_socket.as_ref() else {
            if self.mapping_probe_reflector.is_some() {
                tracing::debug!(
                    client_id = %self.client_id,
                    group_id = %self.group_id,
                    peer_client_id = %peer_client_id,
                    ?session_id,
                    local_port = self.p2p_local_port,
                    "p2p mapping probe configured but listener socket is unavailable; falling back to announce-observed public ip with local port"
                );
            }
            return None;
        };
        let socket = match socket.try_clone() {
            Ok(socket) => socket,
            Err(e) => {
                tracing::debug!(
                    client_id = %self.client_id,
                    group_id = %self.group_id,
                    peer_client_id = %peer_client_id,
                    ?session_id,
                    error = %e,
                    "p2p listener socket clone failed; falling back to announce-observed public ip with local port"
                );
                return None;
            }
        };
        let label = format!("offer:{}:{}", self.client_id, session_id_hex(session_id));
        self.probe_socket_public_endpoint(&socket, "initiator", self.p2p_local_port, label)
            .await
    }

    #[allow(dead_code)]
    async fn probe_reserved_public_endpoint(
        &self,
        socket: &std::net::UdpSocket,
        session_id: SessionId,
        local_port: u16,
    ) -> Option<std::net::SocketAddr> {
        let label = format!("answer:{}:{}", self.client_id, session_id_hex(session_id));
        self.probe_socket_public_endpoint(socket, "acceptor", local_port, label)
            .await
    }

    #[allow(dead_code)]
    async fn probe_initiator_punch_public_endpoint(
        &self,
        socket: &std::net::UdpSocket,
        session_id: SessionId,
        _peer_client_id: &str,
        local_port: u16,
    ) -> Option<std::net::SocketAddr> {
        let label = format!("offer:{}:{}", self.client_id, session_id_hex(session_id));
        self.probe_socket_public_endpoint(socket, "initiator", local_port, label)
            .await
    }

    async fn probe_socket_public_endpoint(
        &self,
        socket: &std::net::UdpSocket,
        role: &'static str,
        local_port: u16,
        label: String,
    ) -> Option<std::net::SocketAddr> {
        let reflector = self.mapping_probe_reflector?;
        let socket = match socket.try_clone() {
            Ok(socket) => socket,
            Err(e) => {
                tracing::debug!(
                    role,
                    local_port,
                    reflector = %reflector,
                    error = %e,
                    "p2p mapping probe socket clone failed; falling back to announce-observed public ip with local port"
                );
                return None;
            }
        };
        let timeout = self.mapping_probe_timeout;
        let probe = tokio::task::spawn_blocking(move || {
            crate::p2p::mapping_probe::probe_std_socket_public_endpoint(
                &socket, reflector, &label, timeout,
            )
        })
        .await;
        let result = match probe {
            Ok(result) => result,
            Err(e) => {
                tracing::debug!(
                    role,
                    local_port,
                    reflector = %reflector,
                    error = %e,
                    "p2p mapping probe task failed; falling back to announce-observed public ip with local port"
                );
                return None;
            }
        };
        match result {
            Ok(Some(observed)) => {
                tracing::info!(
                    role,
                    local_port,
                    observed = %observed,
                    reflector = %reflector,
                    "p2p mapping probe ok"
                );
                Some(observed)
            }
            Ok(None) => {
                tracing::debug!(
                    role,
                    local_port,
                    reflector = %reflector,
                    "p2p mapping probe unavailable; falling back to announce-observed public ip with local port"
                );
                None
            }
            Err(e) => {
                tracing::debug!(
                    role,
                    local_port,
                    reflector = %reflector,
                    error = %e,
                    "p2p mapping probe failed; falling back to announce-observed public ip with local port"
                );
                None
            }
        }
    }

    async fn probe_socket_public_endpoint_for_family(
        &self,
        socket: &std::net::UdpSocket,
        family: P2pAddressFamily,
        role: &'static str,
        local_port: u16,
        label: String,
    ) -> Option<std::net::SocketAddr> {
        let reflector = self.mapping_probe_reflector?;
        if socket_addr_family(reflector) != family {
            tracing::info!(
                role,
                family = family.label(),
                local_port,
                reflector = %reflector,
                "p2p mapping probe skipped because reflector address family differs from attempt family"
            );
            return None;
        }
        self.probe_socket_public_endpoint(socket, role, local_port, label)
            .await
            .filter(|addr| socket_addr_family(*addr) == family)
    }

    pub async fn try_initiate(&mut self, peer_client_id: &str) -> Result<(), &'static str> {
        self.try_initiate_for_local_slot(peer_client_id, None).await
    }

    async fn try_initiate_for_local_slot(
        &mut self,
        peer_client_id: &str,
        preferred_client_id: Option<&str>,
    ) -> Result<(), &'static str> {
        self.try_initiate_for_local_slot_with_relation(peer_client_id, preferred_client_id, None)
            .await
    }

    async fn try_initiate_for_local_slot_with_relation(
        &mut self,
        peer_client_id: &str,
        preferred_client_id: Option<&str>,
        mesh_relation_key: Option<MeshRelationKey>,
    ) -> Result<(), &'static str> {
        if self.v2_profile.is_some() && !self.gateway_direct_lane_enabled {
            return self
                .try_initiate_v2_relay_only(peer_client_id, preferred_client_id, mesh_relation_key)
                .await;
        }
        let raw_candidates = crate::p2p::announce::detect_local_candidates(0);
        let families = self.preferred_attempt_families(&raw_candidates);
        match families.as_slice() {
            [P2pAddressFamily::Ipv6, P2pAddressFamily::Ipv4, ..] => {
                match self
                    .try_initiate_for_local_slot_family_with_relation(
                        peer_client_id,
                        preferred_client_id,
                        P2pAddressFamily::Ipv6,
                        Some(P2pAddressFamily::Ipv4),
                        mesh_relation_key.clone(),
                    )
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        tracing::debug!(
                            peer_client_id = %peer_client_id,
                            local_client_id = ?preferred_client_id,
                            error = %e,
                            "p2p ipv6 offer setup failed; trying ipv4 immediately"
                        );
                        self.try_initiate_for_local_slot_family_with_relation(
                            peer_client_id,
                            preferred_client_id,
                            P2pAddressFamily::Ipv4,
                            None,
                            mesh_relation_key,
                        )
                        .await
                    }
                }
            }
            [P2pAddressFamily::Ipv6, ..] => {
                self.try_initiate_for_local_slot_family_with_relation(
                    peer_client_id,
                    preferred_client_id,
                    P2pAddressFamily::Ipv6,
                    None,
                    mesh_relation_key,
                )
                .await
            }
            [P2pAddressFamily::Ipv4, ..] => {
                self.try_initiate_for_local_slot_family_with_relation(
                    peer_client_id,
                    preferred_client_id,
                    P2pAddressFamily::Ipv4,
                    None,
                    mesh_relation_key,
                )
                .await
            }
            [] => Err("no usable local p2p candidates"),
        }
    }

    async fn try_initiate_v2_relay_only(
        &mut self,
        peer_client_id: &str,
        preferred_client_id: Option<&str>,
        mesh_relation_key: Option<MeshRelationKey>,
    ) -> Result<(), &'static str> {
        let Some(profile) = self.v2_profile.clone() else {
            return Err("V2 Peer profile is unavailable");
        };
        let session_id = SessionId::new_random();
        if self.should_track_new_parallel_attempt() {
            self.multi.set_state(P2pState::Negotiating { session_id });
        }
        let ephemeral_secret = tp_core::peer_link_crypto::PeerLinkEphemeralSecretV2::generate();
        let signed_offer = match tp_core::peer_link_crypto::P2pOfferV2::sign(
            &profile,
            session_id,
            peer_client_id.to_string(),
            Vec::new(),
            self.cert_fp,
            &ephemeral_secret,
        ) {
            Ok(offer) => offer,
            Err(_) => {
                self.handle_session_attempt_cleanup(session_id);
                return Err("could not sign V2 Relay-only offer");
            }
        };
        let wire = match signed_offer.to_wire_bytes() {
            Ok(wire) => wire,
            Err(_) => {
                self.handle_session_attempt_cleanup(session_id);
                return Err("could not encode V2 Relay-only offer");
            }
        };
        if self
            .outbound
            .send(BinaryMessage::P2pOfferV2 {
                source_peer_id: profile.peer.peer_id.clone(),
                target_peer_id: peer_client_id.to_string(),
                signed_offer: bytes::Bytes::from(wire),
            })
            .await
            .is_err()
        {
            self.handle_session_attempt_cleanup(session_id);
            return Err("outbound closed");
        }
        self.pending_v2_offers.insert(
            session_id,
            PendingV2Offer {
                offer: signed_offer,
                ephemeral_secret,
            },
        );
        self.peer_contexts.insert(
            session_id,
            PeerContext {
                peer_client_id: Some(peer_client_id.to_string()),
                local_client_id: preferred_client_id.map(str::to_string),
                allow_parallel: true,
                session_role: Some(ClientRole::Initiator),
                mesh_relation_key,
                ..PeerContext::default()
            },
        );
        self.schedule_offer_answer_timeout(session_id);
        tracing::info!(
            peer_client_id,
            ?session_id,
            "V2 Relay-only PeerLink offer sent"
        );
        Ok(())
    }

    fn preferred_attempt_families(
        &self,
        raw_candidates: &[tp_core::p2p_types::Candidate],
    ) -> Vec<P2pAddressFamily> {
        let local_candidates = self.filter_local_candidates_for_underlay(raw_candidates.to_vec());
        let mut families =
            preferred_local_p2p_families(&local_candidates, self.allow_lan_candidates);
        for addr in [
            self.listener_observed_public_addr,
            self.observed_public_addr,
            self.mapping_probe_reflector,
        ]
        .into_iter()
        .flatten()
        {
            if is_public_p2p_ip(addr.ip()) {
                push_family_once(&mut families, socket_addr_family(addr));
            }
        }
        if families.is_empty() {
            families.push(P2pAddressFamily::Ipv4);
        }
        families
    }

    async fn try_initiate_for_local_slot_family(
        &mut self,
        peer_client_id: &str,
        preferred_client_id: Option<&str>,
        family: P2pAddressFamily,
        fallback_family: Option<P2pAddressFamily>,
    ) -> Result<(), &'static str> {
        self.try_initiate_for_local_slot_family_with_relation(
            peer_client_id,
            preferred_client_id,
            family,
            fallback_family,
            None,
        )
        .await
    }

    async fn try_initiate_for_local_slot_family_with_relation(
        &mut self,
        peer_client_id: &str,
        preferred_client_id: Option<&str>,
        family: P2pAddressFamily,
        fallback_family: Option<P2pAddressFamily>,
        mesh_relation_key: Option<MeshRelationKey>,
    ) -> Result<(), &'static str> {
        use tp_core::p2p_types::{P2pRole, SessionId};
        let session_id = SessionId::new_random();
        let track_attempt_state = self.should_track_new_parallel_attempt();
        if track_attempt_state {
            self.multi
                .set_state(crate::p2p::session::P2pState::Negotiating { session_id });
        }
        let initiator_socket = match bind_std_p2p_socket_for_family_on_interfaces(
            family,
            self.underlay_interface_indexes,
        ) {
            Ok(socket) => socket,
            Err(e) => {
                tracing::warn!(
                    client_id = %self.client_id,
                    group_id = %self.group_id,
                    peer_client_id = %peer_client_id,
                    ?session_id,
                    family = family.label(),
                    error = %e,
                    "p2p initiator failed to reserve family-scoped punch socket"
                );
                if track_attempt_state {
                    self.handle_session_attempt_cleanup(session_id);
                }
                return Err("p2p socket bind failed");
            }
        };
        let candidate_local_port = initiator_socket
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(0);
        let observed_public_addr = self
            .probe_socket_public_endpoint_for_family(
                &initiator_socket,
                family,
                "initiator",
                candidate_local_port,
                format!("offer:{}:{}", self.client_id, session_id_hex(session_id)),
            )
            .await
            .or_else(|| {
                self.listener_observed_public_addr
                    .filter(|addr| socket_addr_family(*addr) == family)
            })
            .or_else(|| {
                self.fallback_observed_endpoint(candidate_local_port)
                    .filter(|addr| socket_addr_family(*addr) == family)
            });
        let candidate_bind_addr = initiator_socket.local_addr().ok();
        let raw_candidates =
            self.filter_local_candidates_for_underlay(filter_candidates_for_bind_addr(
                crate::p2p::announce::detect_local_candidates(candidate_local_port),
                candidate_bind_addr,
            ));
        let candidates = local_p2p_candidates_for_family(
            candidate_local_port,
            family,
            observed_public_addr,
            raw_candidates.clone(),
            self.allow_lan_candidates,
        );
        if candidates.is_empty() {
            tracing::debug!(
                client_id = %self.client_id,
                group_id = %self.group_id,
                peer_client_id = %peer_client_id,
                ?session_id,
                family = family.label(),
                raw_candidate_count = raw_candidates.len(),
                "p2p offer has no usable local candidates; loopback/link-local candidates are ignored"
            );
            tracing::debug!(
                ?session_id,
                raw_candidates = ?raw_candidates,
                "p2p unusable local candidate details"
            );
        }
        if let Some(installer) = self.p2p_installer.as_ref() {
            if !installer.reserve_for_relation(
                session_id,
                preferred_client_id,
                Some(peer_client_id),
                mesh_relation_key.clone(),
            ) {
                self.handle_session_attempt_cleanup(session_id);
                return Err("p2p install reservation rejected");
            }
        }
        let (offer, pending_v2_offer) = if let Some(profile) = &self.v2_profile {
            let ephemeral_secret = tp_core::peer_link_crypto::PeerLinkEphemeralSecretV2::generate();
            let signed_offer = tp_core::peer_link_crypto::P2pOfferV2::sign(
                profile,
                session_id,
                peer_client_id.to_string(),
                candidates.clone(),
                self.cert_fp,
                &ephemeral_secret,
            )
            .map_err(|_| "could not sign V2 p2p offer")?;
            let wire = signed_offer
                .to_wire_bytes()
                .map_err(|_| "could not encode V2 p2p offer")?;
            (
                BinaryMessage::P2pOfferV2 {
                    source_peer_id: profile.peer.peer_id.clone(),
                    target_peer_id: peer_client_id.to_string(),
                    signed_offer: bytes::Bytes::from(wire),
                },
                Some(PendingV2Offer {
                    offer: signed_offer,
                    ephemeral_secret,
                }),
            )
        } else {
            (
                tp_core::protocol::BinaryMessage::P2pOffer {
                    session_id,
                    src_client_id: preferred_client_id.unwrap_or(&self.client_id).to_string(),
                    dst_client_id: peer_client_id.to_string(),
                    candidates: candidates.clone(),
                    src_cert_fp: self.cert_fp,
                    role: P2pRole::Initiator,
                },
                None,
            )
        };
        if self.outbound.send(offer).await.is_err() {
            if let Some(installer) = self.p2p_installer.as_ref() {
                installer.unreserve_for_session(session_id);
            }
            return Err("outbound closed");
        }
        if let Some(pending) = pending_v2_offer {
            self.pending_v2_offers.insert(session_id, pending);
        }
        self.initiator_punch_sockets
            .insert(session_id, initiator_socket);
        tracing::info!(
            client_id = %self.client_id,
            src_client_id = %preferred_client_id.unwrap_or(&self.client_id),
            group_id = %self.group_id,
            peer_client_id = %peer_client_id,
            ?session_id,
            role = ?ClientRole::Initiator,
            family = family.label(),
            fallback_family = fallback_family.map(P2pAddressFamily::label),
            candidate_local_port,
            candidate_count = candidates.len(),
            "p2p offer sent"
        );
        tracing::debug!(
            ?session_id,
            candidate_count = candidates.len(),
            candidates = ?candidates,
            "p2p offer candidate details"
        );
        self.peer_contexts.insert(
            session_id,
            PeerContext {
                candidates: Vec::new(),
                cert_fp: None,
                peer_client_id: Some(peer_client_id.to_string()),
                local_client_id: preferred_client_id.map(str::to_string),
                allow_parallel: true,
                family: Some(family),
                fallback_family,
                session_role: Some(ClientRole::Initiator),
                mesh_relation_key,
            },
        );
        self.schedule_offer_answer_timeout(session_id);
        Ok(())
    }

    /// Tear down any installed P2P session, bump `failure_count`, stamp a
    /// cooldown, and emit a single attempt-result metric tagged with the
    /// caller-supplied `reason`.
    ///
    /// Callers pass the bucket they know best. The spawn-task path
    /// emits its OWN specific result (`Timeout` / `CertFail` / `Success`)
    /// inline and routes its failure back through `P2pTeardown`, whose
    /// handler intentionally skips `fail_and_cooldown` to avoid double-
    /// counting. Anywhere else `fail_and_cooldown` IS the only emission
    /// site for that attempt, so the caller is responsible for picking
    /// the closest bucket. Today both non-spawn-task callers pass
    /// `NatFail` (no usable candidates / remote rejection ≈ NAT-class
    /// failure); a future `Rejected` variant could split the
    /// remote-rejection case off — deliberately out of scope here.
    fn fail_and_cooldown(&mut self, session_id: SessionId, reason: P2pAttemptResult) {
        // Account for any conn_ids that were riding P2P before clearing the
        // slot — they continue on relay (best-effort migration).
        // (Migration counter is incremented inside `report_p2p_to_relay_migration`
        // when the metrics sink is wired.)
        let _migrated = self.multi.report_p2p_to_relay_migration();
        self.close_installed_session(session_id);
        if let Some(m) = self.metrics.as_ref() {
            m.incr_p2p_attempt(reason);
        }
        self.failure_count = self.failure_count.saturating_add(1);
        let until = std::time::Instant::now()
            + next_cooldown(
                self.failure_count.saturating_sub(1),
                self.cooldown_initial,
                self.cooldown_max,
            );
        self.multi
            .set_state(crate::p2p::session::P2pState::Cooldown { until });
    }

    fn cleanup_session_attempt(&mut self, session_id: SessionId) {
        self.cleanup_session_attempt_inner(session_id, true);
    }

    fn cleanup_session_attempt_inner(&mut self, session_id: SessionId, retry_relation: bool) {
        self.cancel_offer_answer_timeout(session_id);
        if let Some(token) = self.active_punch_cancel.remove(&session_id) {
            token.cancel();
        }
        self.acceptor_punch_sockets.remove(&session_id);
        self.initiator_punch_sockets.remove(&session_id);
        self.acceptor_responder_started.remove(&session_id);
        self.pending_v2_offers.remove(&session_id);
        let removed_context = self.peer_contexts.remove(&session_id);
        if let Some(expected) = self.expected_peer_map.as_ref() {
            expected.remove(session_id);
        }
        if let Some(installer) = self.p2p_installer.as_ref() {
            installer.unreserve_for_session(session_id);
        }
        let retry_key = removed_context.and_then(|context| {
            (context.session_role == Some(ClientRole::Initiator))
                .then_some(context.mesh_relation_key)
                .flatten()
        });
        if let Some(key) = retry_key.filter(|_| retry_relation) {
            // Conditional release: cleanup for an older session must not
            // enqueue another generation if the same relation is still
            // occupied by a newer live/in-flight context.
            if !self.mesh_relation_is_occupied(&key) {
                if let Some(lane) = self.mesh_relation_lanes.get(&key).cloned() {
                    self.enqueue_peer_link_command(PeerLinkCommand::EnsureLane(lane));
                }
            }
        }
    }

    fn cancel_crossed_v2_relay_only_offer(&mut self, session_id: SessionId) {
        let should_clear_state = self.in_flight_state_matches_session(session_id);
        self.cleanup_session_attempt_inner(session_id, false);
        if should_clear_state {
            self.multi.set_state(P2pState::Idle);
        }
    }

    fn schedule_offer_answer_timeout(&mut self, session_id: SessionId) {
        self.cancel_offer_answer_timeout(session_id);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let internal_tx = self.internal_tx.clone();
        self.pending_answer_cancel.insert(session_id, cancel);
        self.task_tracker.spawn(async move {
            tokio::select! {
                _ = cancel_for_task.cancelled() => {}
                _ = tokio::time::sleep(OFFER_ANSWER_TIMEOUT) => {
                    if !cancel_for_task.is_cancelled() {
                        let _ = internal_tx.send(P2pInternalEvent::OfferAnswerTimedOut {
                            session_id,
                            cancel: cancel_for_task,
                        });
                    }
                }
            }
        });
    }

    fn cancel_offer_answer_timeout(&mut self, session_id: SessionId) {
        if let Some(token) = self.pending_answer_cancel.remove(&session_id) {
            token.cancel();
        }
    }

    fn handle_internal_event(&mut self, event: P2pInternalEvent) {
        match event {
            P2pInternalEvent::InitiatorAttemptFailed { session_id } => {
                self.handle_initiator_attempt_failed(session_id);
            }
            P2pInternalEvent::CleanupSessionAttempt { session_id } => {
                self.handle_session_attempt_cleanup(session_id);
            }
            P2pInternalEvent::RelationClosed { session_id } => {
                self.cleanup_session_attempt(session_id);
            }
            P2pInternalEvent::OfferAnswerTimedOut { session_id, cancel } => {
                if cancel.is_cancelled() {
                    tracing::debug!(?session_id, "cancelled signaling timeout event ignored");
                } else {
                    self.handle_offer_answer_timeout(session_id);
                }
            }
            P2pInternalEvent::SessionInstalled { session_id } => {
                self.cancel_offer_answer_timeout(session_id);
                self.failure_count = 0;
                tracing::debug!(?session_id, "p2p session installed; scheduling refill poll");
            }
            P2pInternalEvent::RefillRequested { peer_client_id } => {
                self.enqueue_forced_refill_peer(peer_client_id);
            }
        }
    }

    fn handle_initiator_attempt_failed(&mut self, session_id: SessionId) {
        let current_matches = self.current_state_matches_session(session_id);
        let fallback = self.family_fallback_for_session(session_id);
        self.cleanup_session_attempt(session_id);
        if let Some(fallback) = fallback {
            tracing::info!(
                ?session_id,
                peer_client_id = %fallback.peer_client_id,
                local_client_id = ?fallback.local_client_id,
                family = fallback.family.label(),
                current_matches,
                "p2p family attempt failed; scheduling immediate fallback family"
            );
            if current_matches {
                self.multi.set_state(P2pState::Idle);
            }
            self.enqueue_family_fallback(
                fallback.peer_client_id,
                fallback.local_client_id,
                fallback.family,
            );
            return;
        }
        if !current_matches {
            tracing::info!(
                ?session_id,
                "stale initiator failure cleaned up without touching current session"
            );
            return;
        }
        self.cooldown_matching_session_without_metric(session_id);
    }

    fn handle_offer_answer_timeout(&mut self, session_id: SessionId) {
        if self.p2p_installer.as_ref().is_some_and(|installer| {
            installer.expire_for_session(session_id)
                == crate::p2p::installer::P2pInstallExpiration::Installed
        }) {
            self.cancel_offer_answer_timeout(session_id);
            tracing::debug!(
                ?session_id,
                "stale signaling timeout ignored after P2P session installation"
            );
            return;
        }
        let current_matches = self.current_state_matches_session(session_id);
        let fallback = self.family_fallback_for_session(session_id);
        self.cleanup_session_attempt(session_id);
        if let Some(fallback) = fallback {
            tracing::info!(
                ?session_id,
                peer_client_id = %fallback.peer_client_id,
                local_client_id = ?fallback.local_client_id,
                family = fallback.family.label(),
                current_matches,
                "p2p offer answer timed out; scheduling immediate fallback family"
            );
            if current_matches {
                self.multi.set_state(P2pState::Idle);
            }
            self.enqueue_family_fallback(
                fallback.peer_client_id,
                fallback.local_client_id,
                fallback.family,
            );
            return;
        }
        if !current_matches {
            tracing::info!(
                ?session_id,
                "stale offer-answer timeout cleaned up without touching current session"
            );
            return;
        }
        self.multi.set_state(P2pState::Idle);
    }

    fn fail_initiator_attempt_before_spawn(
        &mut self,
        session_id: SessionId,
        metric: Option<P2pAttemptResult>,
    ) {
        if let (Some(metrics), Some(result)) = (self.metrics.as_ref(), metric) {
            metrics.incr_p2p_attempt(result);
        }
        self.handle_initiator_attempt_failed(session_id);
        let outbound = self.outbound.clone();
        self.task_tracker
            .spawn(async move { send_teardown(&outbound, session_id).await });
    }

    fn handle_session_attempt_cleanup(&mut self, session_id: SessionId) {
        let should_clear_state = self.in_flight_state_matches_session(session_id);
        self.cleanup_session_attempt(session_id);
        if should_clear_state {
            self.multi.set_state(P2pState::Idle);
        }
    }

    fn current_state_matches_session(&self, session_id: SessionId) -> bool {
        matches!(
            self.multi.p2p_state(),
            P2pState::Negotiating {
                session_id: current
            }
                | P2pState::Punching {
                    session_id: current,
                    ..
                }
                | P2pState::HandshakingQuic {
                    session_id: current
                }
                | P2pState::Active {
                    session_id: current,
                    ..
                } if current == session_id
        )
    }

    fn current_state_is_other_live_or_in_flight(&self, session_id: SessionId) -> bool {
        matches!(
            self.multi.p2p_state(),
            P2pState::Negotiating {
                session_id: current
            }
                | P2pState::Punching {
                    session_id: current,
                    ..
                }
                | P2pState::HandshakingQuic {
                    session_id: current
                }
                | P2pState::Active {
                    session_id: current,
                    ..
                } if current != session_id
        )
    }

    fn in_flight_state_matches_session(&self, session_id: SessionId) -> bool {
        matches!(
            self.multi.p2p_state(),
            P2pState::Negotiating {
                session_id: current
            }
                | P2pState::Punching {
                    session_id: current,
                    ..
                }
                | P2pState::HandshakingQuic {
                    session_id: current
                } if current == session_id
        )
    }

    fn should_track_attempt_state(&self, session_id: SessionId, allow_parallel: bool) -> bool {
        if !allow_parallel {
            return true;
        }
        match self.multi.p2p_state() {
            P2pState::Idle | P2pState::Announcing | P2pState::Cooldown { .. } => true,
            P2pState::Negotiating {
                session_id: current,
            }
            | P2pState::Punching {
                session_id: current,
                ..
            }
            | P2pState::HandshakingQuic {
                session_id: current,
            }
            | P2pState::Active {
                session_id: current,
                ..
            } => current == session_id,
            P2pState::Disabled => false,
        }
    }

    fn should_track_new_parallel_attempt(&self) -> bool {
        matches!(
            self.multi.p2p_state(),
            P2pState::Disabled | P2pState::Idle | P2pState::Announcing | P2pState::Cooldown { .. }
        )
    }

    fn cooldown_matching_session_without_metric(&mut self, session_id: SessionId) {
        if !self.current_state_matches_session(session_id) {
            return;
        }
        if matches!(
            self.multi.p2p_state(),
            P2pState::Active {
                session_id: current,
                ..
            } if current == session_id
        ) {
            let _migrated = self.multi.report_p2p_to_relay_migration();
            self.close_installed_session(session_id);
        }
        self.failure_count = self.failure_count.saturating_add(1);
        let until = std::time::Instant::now()
            + next_cooldown(
                self.failure_count.saturating_sub(1),
                self.cooldown_initial,
                self.cooldown_max,
            );
        self.multi.set_state(P2pState::Cooldown { until });
    }

    fn close_installed_session(&self, session_id: SessionId) -> bool {
        self.p2p_installer
            .as_ref()
            .is_some_and(|installer| installer.close_installed_session(session_id))
            || self.multi.close_p2p_session(session_id)
    }

    fn is_initiator(&self) -> bool {
        matches!(self.role, ClientRole::Initiator)
    }

    fn start_acceptor_punch_responder(
        &mut self,
        session_id: SessionId,
        t_start_ms: i64,
        reserved_socket: Option<std::net::UdpSocket>,
        warmup_candidates: Vec<std::net::SocketAddr>,
    ) {
        self.acceptor_responder_started.insert(session_id);
        self.spawn_punch_responder(session_id, t_start_ms, reserved_socket, warmup_candidates);
    }

    fn auto_initiator_enabled(&self) -> bool {
        self.peer_link_manager.is_none()
            && self.is_initiator()
            && (!self.forced_refill_peers.is_empty()
                || self.auto_peer_client_id.is_some()
                || !self.auto_peer_client_ids.is_empty())
    }

    fn auto_initiator_has_peer(&self) -> bool {
        self.peer_link_manager.is_none()
            && (!self.forced_refill_peers.is_empty() || !self.auto_peer_client_ids().is_empty())
    }

    async fn maybe_execute_peer_link_commands(&mut self) -> AutoInitiatorAttempt {
        let command_count = self.pending_peer_link_commands.len();
        for _ in 0..command_count {
            let Some(command) = self.pending_peer_link_commands.pop_front() else {
                break;
            };
            let PeerLinkCommand::EnsureLane(lane) = &command;
            let key = match Self::mesh_relation_key_for_lane(lane) {
                Ok(key) => key,
                Err(error) => {
                    tracing::warn!(
                        local_replica_id = %lane.local_replica_id(),
                        remote_replica_id = %lane.remote_replica_id(),
                        lane_index = lane.index(),
                        error,
                        "invalid mesh EnsureLane relation discarded"
                    );
                    continue;
                }
            };
            // Keep the latest desired Replica pairing even while an older
            // generation is healthy/in flight. Occupancy below prevents a
            // duplicate relation; conditional cleanup later retries from
            // this current desired lane instead of a stale modulo fallback.
            self.mesh_relation_lanes.insert(key.clone(), lane.clone());
            if lane.local_role() == RelationRole::Acceptor {
                if self.v2_profile.is_none() {
                    continue;
                }
                let connectivity =
                    PeerDescriptor::from_stable_peer_id(lane.remote_replica_id().to_string())
                        .ok()
                        .and_then(|peer| {
                            self.peer_connectivity_source
                                .as_ref()
                                .map(|source| source(peer.peer_id()))
                        })
                        .unwrap_or_else(PeerConnectivity::unavailable);
                if connectivity.healthy_direct || connectivity.usable_exact_relay {
                    self.v2_acceptor_recovery_not_before.remove(&key);
                    continue;
                }
            }
            if self.mesh_relation_is_occupied(&key) {
                self.v2_acceptor_recovery_not_before.remove(&key);
                tracing::debug!(
                    lane_index = key.lane_index,
                    "mesh EnsureLane coalesced with live or in-flight relation"
                );
                continue;
            }
            if lane.local_role() == RelationRole::Acceptor {
                let now = std::time::Instant::now();
                let not_before = self
                    .v2_acceptor_recovery_not_before
                    .entry(key.clone())
                    .or_insert(now + V2_ACCEPTOR_RECOVERY_DELAY);
                if now < *not_before {
                    self.pending_peer_link_commands.push_back(command);
                    continue;
                }
                tracing::debug!(
                    remote_peer_id = %lane.remote_replica_id(),
                    lane_index = key.lane_index,
                    "fresh V2 canonical acceptor is recovering a missing PeerLink generation"
                );
            }
            let result = self
                .try_initiate_for_local_slot_with_relation(
                    lane.remote_replica_id(),
                    Some(lane.local_replica_id()),
                    Some(key.clone()),
                )
                .await;
            match result {
                Ok(()) => {
                    // A later loss is a new generation and must grant the
                    // canonical initiator a fresh recovery window.
                    self.v2_acceptor_recovery_not_before.remove(&key);
                }
                Err(error) => {
                    tracing::debug!(
                        local_replica_id = %lane.local_replica_id(),
                        remote_replica_id = %lane.remote_replica_id(),
                        lane_index = lane.index(),
                        error,
                        "mesh EnsureLane attempt failed; retaining command for retry"
                    );
                    self.enqueue_peer_link_command(command);
                }
            }
        }
        if self.pending_peer_link_commands.is_empty() {
            AutoInitiatorAttempt::Stop
        } else {
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        }
    }

    pub async fn run(mut self) {
        // 1. Announce
        if let Err(e) = self.announce().await {
            tracing::warn!(?e, "P2P announce failed");
            // Still drain spawned tasks below before returning.
        } else {
            // 2. Pump signaling messages + periodic re-announce.
            //
            // Gateway evicts peer registry entries after 120 s idle
            // (configurable via `gateway.p2p.peer_idle_secs`). If the
            // relay flapped during the original Announce or the gateway
            // got evicted before this peer became reachable, the manager
            // would otherwise stay silent for the rest of its lifetime
            // and the peer would be invisible to incoming P2pOffers. A
            // periodic re-announce at half the eviction window keeps
            // the registry fresh through relay reconnects.
            let reannounce_interval = std::time::Duration::from_secs(REANNOUNCE_INTERVAL_SECS);
            let mut reannounce_timer = tokio::time::interval(reannounce_interval);
            reannounce_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick: we just announced.
            reannounce_timer.tick().await;
            let mut first_announce_ack_seen = false;
            let mut peer_hint_seen = false;
            let mut next_auto_attempt_at: Option<tokio::time::Instant> = None;
            loop {
                let auto_attempt_sleep: Pin<Box<dyn Future<Output = ()> + Send>> =
                    match next_auto_attempt_at {
                        Some(at) => Box::pin(tokio::time::sleep_until(at)),
                        None => Box::pin(std::future::pending()),
                    };
                tokio::select! {
                    biased;
                    msg = self.inbound.recv() => {
                        match msg {
                            Some(msg) => {
                                let announce_ack = matches!(
                                    msg,
                                    BinaryMessage::P2pAnnounceAck { .. }
                                );
                                let peer_hint = matches!(msg, BinaryMessage::P2pPeerHint { .. });
                                let rejected_answer = matches!(
                                    msg,
                                    BinaryMessage::P2pAnswer { ok: false, .. }
                                );
                                if !self.handle_message(msg).await {
                                    continue;
                                }
                                if announce_ack && self.peer_link_manager.is_some() {
                                    next_auto_attempt_at = match self.maybe_execute_peer_link_commands().await {
                                        AutoInitiatorAttempt::Stop => None,
                                        AutoInitiatorAttempt::RetryAfter(delay) => {
                                            Some(tokio::time::Instant::now() + delay)
                                        }
                                    };
                                }
                                if peer_hint && self.auto_initiator_has_peer() {
                                    peer_hint_seen = true;
                                    if first_announce_ack_seen {
                                        next_auto_attempt_at = Some(tokio::time::Instant::now());
                                    }
                                }
                                if announce_ack && !first_announce_ack_seen {
                                    first_announce_ack_seen = true;
                                    if self.auto_initiator_enabled() {
                                        let delay = if peer_hint_seen && self.auto_initiator_has_peer() {
                                            Duration::ZERO
                                        } else {
                                            self.attempt_after_relay_uptime
                                        };
                                        next_auto_attempt_at = Some(
                                            tokio::time::Instant::now() + delay,
                                        );
                                    }
                                }
                                if first_announce_ack_seen
                                    && rejected_answer
                                    && self.auto_initiator_has_peer()
                                    && next_auto_attempt_at.is_none()
                                {
                                    next_auto_attempt_at = Some(
                                        tokio::time::Instant::now() + AUTO_INITIATOR_STATE_POLL,
                                    );
                                } else if first_announce_ack_seen && next_auto_attempt_at.is_none() {
                                    if let Some(delay) = self.auto_cooldown_retry_delay() {
                                        next_auto_attempt_at =
                                            Some(tokio::time::Instant::now() + delay);
                                    }
                                }
                                if self.peer_link_manager.is_some()
                                    && !self.pending_peer_link_commands.is_empty()
                                    && next_auto_attempt_at.is_none()
                                {
                                    next_auto_attempt_at = Some(
                                        tokio::time::Instant::now() + AUTO_INITIATOR_STATE_POLL,
                                    );
                                }
                            }
                            None => break,
                        }
                    }
                    event = self.internal_rx.recv() => {
                        match event {
                            Some(event) => {
                                let wake_auto_attempt = matches!(
                                    event,
                                    P2pInternalEvent::RefillRequested { .. }
                                        | P2pInternalEvent::RelationClosed { .. }
                                        | P2pInternalEvent::SessionInstalled { .. }
                                );
                                let may_enqueue_family_fallback = matches!(
                                    event,
                                    P2pInternalEvent::OfferAnswerTimedOut { .. }
                                        | P2pInternalEvent::InitiatorAttemptFailed { .. }
                                );
                                self.handle_internal_event(event);
                                if (wake_auto_attempt
                                    || (may_enqueue_family_fallback
                                        && !self.family_fallback_queue.is_empty()))
                                    && first_announce_ack_seen
                                    && self.auto_initiator_has_peer()
                                {
                                    next_auto_attempt_at = Some(
                                        tokio::time::Instant::now(),
                                    );
                                }
                                if self.peer_link_manager.is_some()
                                    && !self.pending_peer_link_commands.is_empty()
                                    && next_auto_attempt_at.is_none()
                                {
                                    next_auto_attempt_at = Some(
                                        tokio::time::Instant::now() + AUTO_INITIATOR_STATE_POLL,
                                    );
                                }
                            }
                            None => break,
                        }
                    }
                    _ = auto_attempt_sleep => {
                        let attempt = if self.peer_link_manager.is_some() {
                            self.maybe_execute_peer_link_commands().await
                        } else {
                            self.maybe_try_initiator_attempt().await
                        };
                        next_auto_attempt_at = match attempt {
                            AutoInitiatorAttempt::Stop => None,
                            AutoInitiatorAttempt::RetryAfter(delay) => {
                                Some(tokio::time::Instant::now() + delay)
                            }
                        };
                    }
                    _ = reannounce_timer.tick() => {
                        if let Err(e) = self.announce().await {
                            tracing::warn!(?e, "P2P re-announce failed");
                        }
                    }
                }
            }
        }
        // Structured shutdown — cancel dormant signaling timers,
        // close the tracker so no further spawns are accepted, then await
        // every remaining tracked task. Without this, `run()` could return
        // while spawned punch/responder tasks (5–6.5 s each) still held
        // Arc<MultiSession> + mpsc::Sender clones.
        for (_, cancel) in self.pending_answer_cancel.drain() {
            cancel.cancel();
        }
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    fn auto_cooldown_retry_delay(&self) -> Option<Duration> {
        if !self.auto_initiator_enabled() {
            return None;
        }
        match self.multi.p2p_state() {
            P2pState::Cooldown { until } => {
                let delay = until.saturating_duration_since(std::time::Instant::now());
                Some(delay.max(Duration::from_millis(1)))
            }
            _ => None,
        }
    }

    async fn maybe_try_initiator_attempt(&mut self) -> AutoInitiatorAttempt {
        if !self.auto_initiator_enabled() {
            return AutoInitiatorAttempt::Stop;
        }

        let forced_peers = self.take_forced_refill_peers();
        if !forced_peers.is_empty() {
            if matches!(self.multi.p2p_state(), P2pState::Cooldown { .. }) {
                self.multi.set_state(P2pState::Idle);
            }
            if self.try_initiate_forced_refill_peers(forced_peers).await > 0 {
                return AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL);
            }
        }

        let family_fallbacks = self.take_family_fallbacks();
        if !family_fallbacks.is_empty() {
            if matches!(self.multi.p2p_state(), P2pState::Cooldown { .. }) {
                self.multi.set_state(P2pState::Idle);
            }
            let mut attempted = 0usize;
            for fallback in family_fallbacks {
                if self.peer_has_blocking_context(&fallback.peer_client_id, true) {
                    continue;
                }
                if let Err(e) = self
                    .try_initiate_for_local_slot_family(
                        &fallback.peer_client_id,
                        fallback.local_client_id.as_deref(),
                        fallback.family,
                        None,
                    )
                    .await
                {
                    tracing::warn!(
                        peer_client_id = %fallback.peer_client_id,
                        local_client_id = ?fallback.local_client_id,
                        family = fallback.family.label(),
                        error = %e,
                        "p2p family fallback offer failed"
                    );
                } else {
                    attempted += 1;
                }
            }
            if attempted > 0 {
                return AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL);
            }
        }

        let peers = self.auto_peer_client_ids();
        if peers.is_empty() {
            if !self.empty_peer_warned {
                tracing::debug!(
                    "p2p initiator has empty peer_client_id; waiting for gateway peer hint"
                );
                self.empty_peer_warned = true;
            }
            return AutoInitiatorAttempt::Stop;
        }

        match self.multi.p2p_state() {
            P2pState::Idle | P2pState::Active { .. } => {
                self.try_initiate_missing_peers(peers).await;
                AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
            }
            P2pState::Cooldown { until } => {
                let now = std::time::Instant::now();
                if until <= now {
                    self.multi.set_state(P2pState::Idle);
                    self.try_initiate_missing_peers(peers).await;
                    AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
                } else {
                    let delay = until
                        .saturating_duration_since(now)
                        .max(Duration::from_millis(1));
                    AutoInitiatorAttempt::RetryAfter(delay)
                }
            }
            P2pState::Announcing
            | P2pState::Negotiating { .. }
            | P2pState::Punching { .. }
            | P2pState::HandshakingQuic { .. } => {
                AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
            }
            P2pState::Disabled => AutoInitiatorAttempt::Stop,
        }
    }

    // The error is the channel's own SendError, which carries the whole
    // message; boxing it here would only move the size onto every caller.
    #[allow(clippy::result_large_err)]
    async fn announce(&self) -> Result<(), mpsc::error::SendError<BinaryMessage>> {
        // Announce obeys the same LAN policy as Offer/Answer publishing: when
        // LAN P2P is disabled, this endpoint reports only public addresses.
        let listener_bind_addr = self
            .listener_probe_socket
            .as_ref()
            .and_then(|socket| socket.local_addr().ok());
        let locals = announce_locals_from_candidates(
            self.filter_local_candidates_for_underlay(filter_candidates_for_bind_addr(
                crate::p2p::announce::detect_local_candidates(self.p2p_local_port),
                listener_bind_addr,
            )),
            self.allow_lan_candidates,
        );
        let msg = BinaryMessage::P2pAnnounce {
            client_id: self.client_id.clone(),
            group_id: self.group_id.clone(),
            locals,
            nat_hint: NatHint::Unknown,
            cert_fp: self.cert_fp,
        };
        match self.multi.p2p_state() {
            P2pState::Disabled | P2pState::Idle | P2pState::Announcing => {
                self.multi.set_state(P2pState::Announcing);
            }
            P2pState::Negotiating { .. }
            | P2pState::Punching { .. }
            | P2pState::HandshakingQuic { .. }
            | P2pState::Active { .. }
            | P2pState::Cooldown { .. } => {}
        }
        self.outbound.send(msg).await
    }

    async fn send_v2_rejection(
        &self,
        profile: &tp_core::provisioning::PeerProfileV2,
        offer: &tp_core::peer_link_crypto::P2pOfferV2,
        reason_code: u8,
    ) {
        let secret = tp_core::peer_link_crypto::PeerLinkEphemeralSecretV2::generate();
        let result = tp_core::peer_link_crypto::P2pAnswerV2::sign(
            profile,
            offer,
            false,
            reason_code,
            Vec::new(),
            self.cert_fp,
            &secret,
        )
        .and_then(|answer| answer.to_wire_bytes().map(|wire| (answer, wire)));
        let Ok((_answer, wire)) = result else {
            tracing::warn!(?reason_code, "could not sign V2 P2P rejection");
            return;
        };
        let _ = self
            .outbound
            .send(BinaryMessage::P2pAnswerV2 {
                source_peer_id: profile.peer.peer_id.clone(),
                target_peer_id: offer.source_peer_id.clone(),
                signed_answer: bytes::Bytes::from(wire),
            })
            .await;
    }

    async fn handle_v2_offer(
        &mut self,
        outer_source_peer_id: String,
        outer_target_peer_id: String,
        signed_offer: bytes::Bytes,
    ) {
        let Some(profile) = self.v2_profile.clone() else {
            return;
        };
        let Ok(offer) = tp_core::peer_link_crypto::P2pOfferV2::from_wire_bytes(&signed_offer)
        else {
            tracing::warn!("malformed V2 P2P Offer ignored");
            return;
        };
        if outer_source_peer_id != offer.source_peer_id
            || outer_target_peer_id != offer.target_peer_id
            || offer.target_peer_id != profile.peer.peer_id
            || offer.tunnel_id != profile.tunnel_id
            || offer.verify(&profile.tunnel_signing_public_key).is_err()
        {
            tracing::warn!("misbound or unauthenticated V2 P2P Offer ignored");
            return;
        }
        if self
            .v2_current_peer_authority_source
            .as_ref()
            .is_some_and(|source| !source(&offer.source_peer_id))
        {
            tracing::debug!(
                source_peer_id = %offer.source_peer_id,
                "V2 P2P Offer source is absent from current membership; dropping"
            );
            return;
        }
        if let Some(sink) = &self.v2_membership_sink {
            sink(&offer.public_peer_membership);
        }

        let session_id = offer.session_id;
        let peer_candidates =
            usable_p2p_candidates(offer.candidates.clone(), self.allow_lan_candidates);
        if peer_candidates.is_empty() {
            let crossed_offer = self.crossed_v2_relay_only_offer(&offer);
            // For crossed generations the stable-ID canonical Offer wins.
            // Keep our canonical attempt pending and reject the reverse one.
            if crossed_offer.is_some_and(|(_, role)| role == RelationRole::Acceptor) {
                self.send_v2_rejection(&profile, &offer, V2_REJECT_RELATION_BUSY)
                    .await;
                return;
            }
            let answer_secret = tp_core::peer_link_crypto::PeerLinkEphemeralSecretV2::generate();
            let Ok(answer) = tp_core::peer_link_crypto::P2pAnswerV2::sign(
                &profile,
                &offer,
                true,
                0,
                Vec::new(),
                self.cert_fp,
                &answer_secret,
            ) else {
                return;
            };
            let Ok(wire) = answer.to_wire_bytes() else {
                return;
            };
            let Ok(keys) = answer_secret.derive_session_keys(
                &offer,
                &answer,
                &profile.tunnel_signing_public_key,
            ) else {
                return;
            };
            if self
                .outbound
                .send(BinaryMessage::P2pAnswerV2 {
                    source_peer_id: profile.peer.peer_id.clone(),
                    target_peer_id: offer.source_peer_id.clone(),
                    signed_answer: bytes::Bytes::from(wire),
                })
                .await
                .is_ok()
            {
                // The canonical Answer is now durably queued. Drop our
                // reverse recovery attempt without re-enqueuing its lane,
                // then install only the canonical generation below.
                if let Some((pending_session, RelationRole::Initiator)) = crossed_offer {
                    self.cancel_crossed_v2_relay_only_offer(pending_session);
                }
                if let Ok(key) = MeshRelationKey::from_stable_peers(
                    &offer.source_peer_id,
                    &offer.target_peer_id,
                    0,
                ) {
                    self.v2_acceptor_recovery_not_before.remove(&key);
                }
                if let Some(sink) = &self.v2_peer_link_sink {
                    sink(offer.source_peer_id, session_id, keys);
                }
            }
            return;
        }
        let mesh_relation_key = if self.peer_link_manager.is_some() {
            let Ok(key) =
                MeshRelationKey::from_stable_peers(&offer.source_peer_id, &offer.target_peer_id, 0)
            else {
                return;
            };
            if self.mesh_relation_is_occupied(&key) {
                self.send_v2_rejection(&profile, &offer, V2_REJECT_RELATION_BUSY)
                    .await;
                return;
            }
            Some(key)
        } else {
            None
        };
        let Some(offer_family) = candidate_set_family(&peer_candidates) else {
            self.send_v2_rejection(&profile, &offer, V2_REJECT_MIXED_CANDIDATE_FAMILY)
                .await;
            return;
        };
        if let Some(installer) = self.p2p_installer.as_ref() {
            if !installer.reserve_for_relation(
                session_id,
                Some(&self.client_id),
                Some(&offer.source_peer_id),
                mesh_relation_key.clone(),
            ) {
                self.send_v2_rejection(&profile, &offer, V2_REJECT_RELATION_BUSY)
                    .await;
                return;
            }
        }

        let (punch_socket, punch_port) = match bind_std_p2p_socket_for_family_on_interfaces(
            offer_family,
            self.underlay_interface_indexes,
        ) {
            Ok(socket) => {
                let port = socket.local_addr().map(|addr| addr.port()).unwrap_or(0);
                (socket, port)
            }
            Err(error) => {
                tracing::warn!(?session_id, %error, "V2 P2P acceptor could not reserve punch socket");
                if let Some(installer) = self.p2p_installer.as_ref() {
                    installer.unreserve_for_session(session_id);
                }
                self.send_v2_rejection(&profile, &offer, V2_REJECT_PUNCH_SOCKET)
                    .await;
                return;
            }
        };
        let observed_public_addr = self
            .probe_socket_public_endpoint_for_family(
                &punch_socket,
                offer_family,
                "acceptor",
                punch_port,
                format!("answer:{}:{}", self.client_id, session_id_hex(session_id)),
            )
            .await
            .or_else(|| {
                self.fallback_observed_endpoint(punch_port)
                    .filter(|addr| socket_addr_family(*addr) == offer_family)
            });
        let punch_bind_addr = punch_socket.local_addr().ok();
        let raw_local_candidates =
            self.filter_local_candidates_for_underlay(filter_candidates_for_bind_addr(
                crate::p2p::announce::detect_local_candidates(punch_port),
                punch_bind_addr,
            ));
        let local_candidates = local_p2p_candidates_for_family(
            punch_port,
            offer_family,
            observed_public_addr,
            raw_local_candidates,
            self.allow_lan_candidates,
        );
        let answer_secret = tp_core::peer_link_crypto::PeerLinkEphemeralSecretV2::generate();
        let Ok(answer) = tp_core::peer_link_crypto::P2pAnswerV2::sign(
            &profile,
            &offer,
            true,
            0,
            local_candidates,
            self.cert_fp,
            &answer_secret,
        ) else {
            if let Some(installer) = self.p2p_installer.as_ref() {
                installer.unreserve_for_session(session_id);
            }
            return;
        };
        let Ok(wire) = answer.to_wire_bytes() else {
            if let Some(installer) = self.p2p_installer.as_ref() {
                installer.unreserve_for_session(session_id);
            }
            return;
        };
        let Ok(keys) =
            answer_secret.derive_session_keys(&offer, &answer, &profile.tunnel_signing_public_key)
        else {
            if let Some(installer) = self.p2p_installer.as_ref() {
                installer.unreserve_for_session(session_id);
            }
            return;
        };

        self.peer_candidates_cache = peer_candidates.clone();
        self.peer_cert_fp_cache = Some(offer.direct_certificate_fingerprint);
        self.peer_client_id_cache = Some(offer.source_peer_id.clone());
        self.peer_contexts.insert(
            session_id,
            PeerContext {
                candidates: peer_candidates.clone(),
                cert_fp: Some(offer.direct_certificate_fingerprint),
                peer_client_id: Some(offer.source_peer_id.clone()),
                local_client_id: Some(self.client_id.clone()),
                allow_parallel: true,
                family: Some(offer_family),
                fallback_family: None,
                session_role: Some(ClientRole::Acceptor),
                mesh_relation_key: mesh_relation_key.clone(),
            },
        );
        if let Some(expected) = self.expected_peer_map.as_ref() {
            expected.insert(
                session_id,
                crate::p2p::expected::ExpectedPeer {
                    peer_client_id: offer.source_peer_id.clone(),
                    cert_fp: offer.direct_certificate_fingerprint,
                    candidates: peer_candidates,
                },
            );
        }
        if self
            .outbound
            .send(BinaryMessage::P2pAnswerV2 {
                source_peer_id: profile.peer.peer_id.clone(),
                target_peer_id: offer.source_peer_id.clone(),
                signed_answer: bytes::Bytes::from(wire),
            })
            .await
            .is_err()
        {
            self.cleanup_session_attempt(session_id);
            return;
        }
        if let Some(key) = &mesh_relation_key {
            self.v2_acceptor_recovery_not_before.remove(key);
        }
        if let Some(sink) = &self.v2_peer_link_sink {
            sink(offer.source_peer_id.clone(), session_id, keys);
        }
        self.acceptor_punch_sockets.insert(session_id, punch_socket);
        if self.should_track_attempt_state(session_id, true) {
            self.multi.set_state(P2pState::Negotiating { session_id });
        }
        self.schedule_offer_answer_timeout(session_id);
        let punch_socket = self.acceptor_punch_sockets.remove(&session_id);
        let warmup_candidates = self
            .peer_contexts
            .get(&session_id)
            .map(|context| {
                usable_p2p_candidates(context.candidates.clone(), self.allow_lan_candidates)
                    .iter()
                    .filter_map(candidate_socket_addr)
                    .filter(|candidate| socket_addr_family(*candidate) == offer_family)
                    .collect()
            })
            .unwrap_or_default();
        self.start_acceptor_punch_responder(session_id, 0, punch_socket, warmup_candidates);
    }

    async fn handle_v2_answer(
        &mut self,
        outer_source_peer_id: String,
        outer_target_peer_id: String,
        signed_answer: bytes::Bytes,
    ) {
        let Some(profile) = self.v2_profile.clone() else {
            return;
        };
        let Ok(answer) = tp_core::peer_link_crypto::P2pAnswerV2::from_wire_bytes(&signed_answer)
        else {
            tracing::warn!("malformed V2 P2P Answer ignored");
            return;
        };
        if outer_target_peer_id != profile.peer.peer_id {
            return;
        }
        let pending_session = self.pending_v2_offers.iter().find_map(|entry| {
            entry
                .1
                .offer
                .signed_hash()
                .ok()
                .filter(|hash| hash == &answer.offer_hash)
                .map(|_| *entry.0)
        });
        let Some(session_id) = pending_session else {
            tracing::debug!("V2 P2P Answer for an unknown Offer ignored");
            return;
        };
        let Some(pending) = self.pending_v2_offers.get(&session_id) else {
            return;
        };
        if outer_source_peer_id != pending.offer.target_peer_id
            || outer_target_peer_id != pending.offer.source_peer_id
            || answer
                .verify_for_offer(&pending.offer, &profile.tunnel_signing_public_key)
                .is_err()
        {
            tracing::warn!(
                ?session_id,
                "misbound or unauthenticated V2 P2P Answer ignored"
            );
            return;
        }
        if !answer.accepted {
            self.cleanup_session_attempt(session_id);
            if self.current_state_matches_session(session_id) {
                self.fail_and_cooldown(session_id, P2pAttemptResult::NatFail);
            }
            return;
        }
        if self
            .v2_current_peer_authority_source
            .as_ref()
            .is_some_and(|source| !source(&pending.offer.target_peer_id))
        {
            tracing::debug!(
                ?session_id,
                source_peer_id = %pending.offer.target_peer_id,
                "V2 P2P Answer source is absent from current membership; dropping"
            );
            self.handle_session_attempt_cleanup(session_id);
            return;
        }
        let peer_candidates =
            usable_p2p_candidates(answer.candidates.clone(), self.allow_lan_candidates);
        if peer_candidates.is_empty() {
            let Some(pending) = self.pending_v2_offers.remove(&session_id) else {
                return;
            };
            let Ok(keys) = pending.ephemeral_secret.derive_session_keys(
                &pending.offer,
                &answer,
                &profile.tunnel_signing_public_key,
            ) else {
                self.cleanup_session_attempt(session_id);
                return;
            };
            if let Some(sink) = &self.v2_membership_sink {
                sink(&answer.public_peer_membership);
            }
            if let Some(sink) = &self.v2_peer_link_sink {
                sink(answer.accepted_peer_id, session_id, keys);
            }
            if let Some(context) = self.peer_contexts.get_mut(&session_id) {
                context.mesh_relation_key = None;
            }
            self.handle_session_attempt_cleanup(session_id);
            return;
        }
        let Some(answer_family) = candidate_set_family(&peer_candidates) else {
            self.cleanup_session_attempt(session_id);
            return;
        };
        if self
            .peer_contexts
            .get(&session_id)
            .and_then(|context| context.family)
            != Some(answer_family)
        {
            self.cleanup_session_attempt(session_id);
            return;
        }
        let Some(pending) = self.pending_v2_offers.remove(&session_id) else {
            return;
        };
        let Ok(keys) = pending.ephemeral_secret.derive_session_keys(
            &pending.offer,
            &answer,
            &profile.tunnel_signing_public_key,
        ) else {
            self.cleanup_session_attempt(session_id);
            return;
        };
        let Some(context) = self.peer_contexts.get_mut(&session_id) else {
            return;
        };
        context.candidates = peer_candidates.clone();
        context.cert_fp = Some(answer.direct_certificate_fingerprint);
        context.peer_client_id = Some(answer.accepted_peer_id.clone());
        self.peer_candidates_cache = peer_candidates;
        self.peer_cert_fp_cache = Some(answer.direct_certificate_fingerprint);
        self.peer_client_id_cache = Some(answer.accepted_peer_id.clone());
        if let Some(installer) = self.p2p_installer.as_ref() {
            installer.update_peer_client_id(session_id, &answer.accepted_peer_id);
        }
        if let Some(sink) = &self.v2_membership_sink {
            sink(&answer.public_peer_membership);
        }
        if let Some(sink) = &self.v2_peer_link_sink {
            sink(answer.accepted_peer_id.clone(), session_id, keys);
        }
        self.cancel_offer_answer_timeout(session_id);
        self.spawn_punch_and_handshake(session_id, 0, 30, vec![0, 1, 2, 5, -1], true);
    }

    async fn handle_message(&mut self, msg: BinaryMessage) -> bool {
        if matches!(&msg, BinaryMessage::P2pAnnounceAck { .. })
            && self.peer_link_manager.is_some()
            && !self.commit_membership_cycle()
        {
            tracing::debug!("membership Ack authority was rejected; dropping whole cycle");
            return false;
        }
        self.handle_authorized_message(msg).await;
        true
    }

    async fn handle_authorized_message(&mut self, msg: BinaryMessage) {
        match msg {
            BinaryMessage::P2pOfferV2 {
                source_peer_id,
                target_peer_id,
                signed_offer,
            } => {
                self.handle_v2_offer(source_peer_id, target_peer_id, signed_offer)
                    .await;
            }
            BinaryMessage::P2pAnswerV2 {
                source_peer_id,
                target_peer_id,
                signed_answer,
            } => {
                self.handle_v2_answer(source_peer_id, target_peer_id, signed_answer)
                    .await;
            }
            BinaryMessage::P2pAnnounceAck {
                public_ip,
                public_port,
                ..
            } => {
                self.gateway_direct_lane_enabled = public_port != 0;
                let observed = self
                    .gateway_direct_lane_enabled
                    .then(|| {
                        public_ip
                            .parse::<std::net::IpAddr>()
                            .ok()
                            .map(|ip| std::net::SocketAddr::new(ip, public_port))
                    })
                    .flatten();
                if let Some(addr) = observed {
                    self.observed_public_addr = Some(addr);
                } else if !self.gateway_direct_lane_enabled {
                    self.observed_public_addr = None;
                    self.listener_observed_public_addr = None;
                } else {
                    tracing::warn!(
                        public_ip = %public_ip,
                        public_port,
                        "p2p announce ack contained invalid public endpoint"
                    );
                }
                if matches!(self.multi.p2p_state(), P2pState::Announcing) {
                    self.multi.set_state(P2pState::Idle);
                }
                let peers = self.auto_peer_client_ids();
                tracing::info!(
                    client_id = %self.client_id,
                    group_id = %self.group_id,
                    role = ?self.role,
                    observed_public_addr = ?self.observed_public_addr,
                    p2p_listener_port = self.p2p_local_port,
                    auto_peer_count = peers.len(),
                    auto_peers = ?peers,
                    "p2p announce acknowledged"
                );
            }
            BinaryMessage::P2pPeerHint { peer_client_id } => {
                if self.peer_link_manager.is_some() {
                    self.buffer_membership_replica(peer_client_id);
                    return;
                }
                let peer_client_id = peer_client_id.trim().to_string();
                if !self.is_initiator()
                    || peer_client_id.is_empty()
                    || peer_client_id == self.client_id
                {
                    return;
                }
                let current_peer_owned = self.auto_peer_client_id.clone().unwrap_or_default();
                let current_peer = current_peer_owned.trim();
                let can_replace_peer = matches!(
                    self.multi.p2p_state(),
                    P2pState::Disabled
                        | P2pState::Announcing
                        | P2pState::Idle
                        | P2pState::Cooldown { .. }
                );
                let pruned_stale_candidates =
                    if matches!(self.multi.p2p_state(), P2pState::Active { .. }) {
                        self.prune_stale_same_replica_peer_hint_candidates(&peer_client_id)
                    } else {
                        false
                    };
                let should_update = match current_peer {
                    "" => true,
                    current if current != peer_client_id => can_replace_peer,
                    _ => false,
                };
                let added = self.add_auto_peer_client_id(peer_client_id.clone(), should_update);
                if should_update || added || pruned_stale_candidates {
                    let peer_changed = !current_peer.is_empty() && current_peer != peer_client_id;
                    if should_update
                        && peer_changed
                        && !same_replica_family(current_peer, &peer_client_id)
                    {
                        self.auto_peer_client_ids.clear();
                        self.auto_peer_client_ids.push(peer_client_id.clone());
                    }
                    tracing::info!(
                        previous_peer_client_id = current_peer,
                        peer_client_id = %peer_client_id,
                        pruned_stale_candidates,
                        "p2p initiator learned peer_client_id from gateway"
                    );
                    self.empty_peer_warned = false;
                    if peer_changed && matches!(self.multi.p2p_state(), P2pState::Cooldown { .. }) {
                        self.failure_count = 0;
                        self.multi.set_state(P2pState::Idle);
                    }
                }
            }
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id,
                dst_client_id,
                candidates,
                src_cert_fp,
                role: _,
            } => {
                if self.v2_profile.is_some() {
                    tracing::warn!(
                        ?session_id,
                        "unsigned legacy P2P Offer rejected by V2 manager"
                    );
                    return;
                }
                // Defense-in-depth check that the offer is actually
                // for us. Gateway already routes Offer to the correct peer
                // (group filter + group-scoped lookup), so this
                // is belt-and-suspenders against a future gateway bug or
                // a misrouted relay path. Reject without caching peer state
                // so a misrouted offer can't poison the acceptor.
                if !same_or_child_replica(&self.client_id, &dst_client_id) {
                    tracing::warn!(
                        ?session_id,
                        expected = %self.client_id,
                        got = %dst_client_id,
                        "P2pOffer dst_client_id mismatch; rejecting"
                    );
                    let _ = self
                        .outbound
                        .send(BinaryMessage::P2pAnswer {
                            session_id,
                            accepted_client_id: self.client_id.clone(),
                            ok: false,
                            reason: "wrong dst".into(),
                            candidates: vec![],
                            dst_cert_fp: tp_core::p2p_types::CertFingerprint::zero(),
                        })
                        .await;
                    return;
                }
                let mesh_relation_key = if self.peer_link_manager.is_some() {
                    let key = match MeshRelationKey::from_canonical_initiator(
                        &src_client_id,
                        &dst_client_id,
                    ) {
                        Ok(key) => key,
                        Err(reason) => {
                            tracing::warn!(
                                ?session_id,
                                src_client_id = %src_client_id,
                                dst_client_id = %dst_client_id,
                                reason,
                                "noncanonical mesh P2pOffer rejected"
                            );
                            let _ = self
                                .outbound
                                .send(BinaryMessage::P2pAnswer {
                                    session_id,
                                    accepted_client_id: dst_client_id,
                                    ok: false,
                                    reason: reason.into(),
                                    candidates: vec![],
                                    dst_cert_fp: self.cert_fp,
                                })
                                .await;
                            return;
                        }
                    };
                    if self.mesh_relation_is_occupied(&key) {
                        tracing::info!(
                            ?session_id,
                            src_client_id = %src_client_id,
                            dst_client_id = %dst_client_id,
                            lane_index = key.lane_index,
                            "duplicate mesh relation generation coalesced"
                        );
                        let _ = self
                            .outbound
                            .send(BinaryMessage::P2pAnswer {
                                session_id,
                                accepted_client_id: dst_client_id,
                                ok: false,
                                reason: "mesh relation busy".into(),
                                candidates: vec![],
                                dst_cert_fp: self.cert_fp,
                            })
                            .await;
                        return;
                    }
                    Some(key)
                } else {
                    None
                };
                // These are peer candidates and therefore future punch targets.
                // A non-LAN endpoint must discard LAN targets even if the peer
                // published them.
                let peer_candidates = usable_p2p_candidates(candidates, self.allow_lan_candidates);
                if peer_candidates.is_empty() {
                    tracing::debug!(
                        ?session_id,
                        src_client_id = %src_client_id,
                        dst_client_id = %dst_client_id,
                        allow_lan_candidates = self.allow_lan_candidates,
                        "P2pOffer has no usable peer candidates after local LAN policy filtering"
                    );
                    let _ = self
                        .outbound
                        .send(BinaryMessage::P2pAnswer {
                            session_id,
                            accepted_client_id: dst_client_id.clone(),
                            ok: false,
                            reason: "no usable peer candidates".into(),
                            candidates: vec![],
                            dst_cert_fp: self.cert_fp,
                        })
                        .await;
                    return;
                }
                let Some(offer_family) = candidate_set_family(&peer_candidates) else {
                    tracing::warn!(
                        ?session_id,
                        src_client_id = %src_client_id,
                        dst_client_id = %dst_client_id,
                        candidates = ?peer_candidates,
                        "P2pOffer has mixed or unparsable candidate families; rejecting"
                    );
                    let _ = self
                        .outbound
                        .send(BinaryMessage::P2pAnswer {
                            session_id,
                            accepted_client_id: dst_client_id.clone(),
                            ok: false,
                            reason: "mixed candidate family".into(),
                            candidates: vec![],
                            dst_cert_fp: self.cert_fp,
                        })
                        .await;
                    return;
                };
                // Cache initiator details for use during punch + handshake.
                self.peer_candidates_cache = peer_candidates.clone();
                self.peer_cert_fp_cache = Some(src_cert_fp);
                self.peer_client_id_cache = Some(src_client_id.clone());
                self.peer_contexts.insert(
                    session_id,
                    PeerContext {
                        candidates: peer_candidates.clone(),
                        cert_fp: Some(src_cert_fp),
                        peer_client_id: Some(src_client_id.clone()),
                        local_client_id: Some(dst_client_id.clone()),
                        allow_parallel: true,
                        family: Some(offer_family),
                        fallback_family: None,
                        session_role: Some(ClientRole::Acceptor),
                        mesh_relation_key: mesh_relation_key.clone(),
                    },
                );
                if let Some(installer) = self.p2p_installer.as_ref() {
                    if !installer.reserve_for_relation(
                        session_id,
                        Some(&dst_client_id),
                        Some(&src_client_id),
                        mesh_relation_key,
                    ) {
                        self.cleanup_session_attempt(session_id);
                        let _ = self
                            .outbound
                            .send(BinaryMessage::P2pAnswer {
                                session_id,
                                accepted_client_id: dst_client_id.clone(),
                                ok: false,
                                reason: "install reservation rejected".into(),
                                candidates: vec![],
                                dst_cert_fp: self.cert_fp,
                            })
                            .await;
                        return;
                    }
                }
                if let Some(expected) = self.expected_peer_map.as_ref() {
                    expected.insert(
                        session_id,
                        crate::p2p::expected::ExpectedPeer {
                            peer_client_id: src_client_id.clone(),
                            cert_fp: src_cert_fp,
                            candidates: peer_candidates,
                        },
                    );
                }
                if let Some(handle) = self.expected_fp_handle.as_ref() {
                    // Legacy read-back slot; listener validation now uses
                    // expected_peer_map so parallel offers do not overwrite
                    // each other.
                    match handle.lock() {
                        Ok(mut g) => *g = Some(src_cert_fp),
                        Err(e) => tracing::warn!(
                            ?e,
                            "legacy expected_fp_handle poisoned; read-back slot not updated"
                        ),
                    }
                }
                let (punch_socket, punch_port) = match bind_std_p2p_socket_for_family_on_interfaces(
                    offer_family,
                    self.underlay_interface_indexes,
                ) {
                    Ok(socket) => {
                        let port = socket.local_addr().map(|addr| addr.port()).unwrap_or(0);
                        (socket, port)
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?session_id,
                            ?e,
                            "p2p acceptor failed to reserve punch socket; rejecting offer"
                        );
                        self.cleanup_session_attempt(session_id);
                        let _ = self
                            .outbound
                            .send(BinaryMessage::P2pAnswer {
                                session_id,
                                accepted_client_id: dst_client_id.clone(),
                                ok: false,
                                reason: "punch socket bind failed".into(),
                                candidates: vec![],
                                dst_cert_fp: self.cert_fp,
                            })
                            .await;
                        return;
                    }
                };
                let observed_public_addr = self
                    .probe_socket_public_endpoint_for_family(
                        &punch_socket,
                        offer_family,
                        "acceptor",
                        punch_port,
                        format!("answer:{}:{}", self.client_id, session_id_hex(session_id)),
                    )
                    .await
                    .or_else(|| {
                        self.fallback_observed_endpoint(punch_port)
                            .filter(|addr| socket_addr_family(*addr) == offer_family)
                    });
                let punch_bind_addr = punch_socket.local_addr().ok();
                self.acceptor_punch_sockets.insert(session_id, punch_socket);
                let raw_local_cands =
                    self.filter_local_candidates_for_underlay(filter_candidates_for_bind_addr(
                        crate::p2p::announce::detect_local_candidates(punch_port),
                        punch_bind_addr,
                    ));
                let local_cands = local_p2p_candidates_for_family(
                    punch_port,
                    offer_family,
                    observed_public_addr,
                    raw_local_cands.clone(),
                    self.allow_lan_candidates,
                );
                if local_cands.is_empty() {
                    tracing::debug!(
                        ?session_id,
                        src_client_id = %src_client_id,
                        family = offer_family.label(),
                        raw_candidates = ?raw_local_cands,
                        "p2p answer has no usable same-family local candidates"
                    );
                }
                let answer_candidates = local_cands.clone();
                let answer = BinaryMessage::P2pAnswer {
                    session_id,
                    accepted_client_id: dst_client_id.clone(),
                    ok: true,
                    reason: String::new(),
                    candidates: local_cands,
                    dst_cert_fp: self.cert_fp,
                };
                if self.should_track_attempt_state(session_id, true) {
                    self.multi.set_state(P2pState::Negotiating { session_id });
                }
                tracing::info!(
                    ?session_id,
                    src_client_id = %src_client_id,
                    family = offer_family.label(),
                    candidate_count = answer_candidates.len(),
                    "p2p answer sent"
                );
                tracing::debug!(
                    ?session_id,
                    candidate_count = answer_candidates.len(),
                    candidates = ?answer_candidates,
                    "p2p answer candidate details"
                );
                if self.outbound.send(answer).await.is_err() {
                    self.cleanup_session_attempt(session_id);
                    return;
                }
                self.schedule_offer_answer_timeout(session_id);
                let punch_socket = self.acceptor_punch_sockets.remove(&session_id);
                if punch_socket.is_some() {
                    tracing::info!(
                        ?session_id,
                        "p2p acceptor starting reserved responder immediately after answer"
                    );
                    let warmup_candidates = self
                        .peer_contexts
                        .get(&session_id)
                        .map(|ctx| {
                            usable_p2p_candidates(ctx.candidates.clone(), self.allow_lan_candidates)
                                .iter()
                                .filter_map(candidate_socket_addr)
                                .filter(|candidate| socket_addr_family(*candidate) == offer_family)
                                .collect()
                        })
                        .unwrap_or_default();
                    self.start_acceptor_punch_responder(
                        session_id,
                        0,
                        punch_socket,
                        warmup_candidates,
                    );
                }
            }
            BinaryMessage::P2pAnswer {
                session_id,
                ok,
                accepted_client_id,
                candidates,
                dst_cert_fp,
                reason,
            } => {
                if self.v2_profile.is_some() {
                    tracing::warn!(
                        ?session_id,
                        "unsigned legacy P2P Answer rejected by V2 manager"
                    );
                    return;
                }
                if !ok {
                    let live_or_in_flight =
                        self.session_context_is_live_or_in_flight(session_id, true);
                    let current_state_matches = self.current_state_matches_session(session_id);
                    let other_current = self.current_state_is_other_live_or_in_flight(session_id);
                    let known_context = self.peer_contexts.contains_key(&session_id);
                    let fallback = self.family_fallback_for_session(session_id);
                    self.cleanup_session_attempt(session_id);
                    if !live_or_in_flight {
                        tracing::info!(
                            ?session_id,
                            known_context,
                            reason = %reason,
                            "stale or unknown P2pAnswer rejection ignored"
                        );
                        return;
                    }
                    if let Some(fallback) = fallback {
                        tracing::debug!(
                            ?session_id,
                            peer_client_id = %fallback.peer_client_id,
                            local_client_id = ?fallback.local_client_id,
                            family = fallback.family.label(),
                            current_state_matches,
                            reason = %reason,
                            "P2pAnswer rejected; scheduling immediate fallback family"
                        );
                        if let Some(m) = self.metrics.as_ref() {
                            m.incr_p2p_attempt(P2pAttemptResult::NatFail);
                        }
                        if current_state_matches {
                            self.multi.set_state(P2pState::Idle);
                        }
                        self.enqueue_family_fallback(
                            fallback.peer_client_id,
                            fallback.local_client_id,
                            fallback.family,
                        );
                        return;
                    }
                    if other_current && !current_state_matches {
                        tracing::debug!(
                            ?session_id,
                            reason = %reason,
                            "P2pAnswer rejected for non-current attempt; cleaned up without touching current session"
                        );
                        if let Some(m) = self.metrics.as_ref() {
                            m.incr_p2p_attempt(P2pAttemptResult::NatFail);
                        }
                        return;
                    }
                    tracing::debug!(?session_id, reason = %reason, "P2pAnswer rejected; entering cooldown");
                    // Remote rejected the offer. Closest bucket today
                    // is `NatFail` (a future `Rejected` variant could split
                    // this off — out of scope here).
                    self.fail_and_cooldown(session_id, P2pAttemptResult::NatFail);
                    return;
                }
                if !self.peer_contexts.contains_key(&session_id) {
                    tracing::debug!(?session_id, "P2pAnswer for unknown session ignored");
                    return;
                }
                let answer_phase_advanced = self.active_punch_cancel.contains_key(&session_id)
                    || matches!(
                        self.multi.p2p_state(),
                        P2pState::Punching {
                            session_id: active,
                            ..
                        } | P2pState::HandshakingQuic { session_id: active }
                            | P2pState::Active {
                                session_id: active,
                                ..
                            } if active == session_id
                    );
                if answer_phase_advanced {
                    tracing::debug!(
                        ?session_id,
                        "duplicate P2pAnswer ignored after punch phase advanced"
                    );
                    return;
                }
                if !self.session_context_is_live_or_in_flight(session_id, true) {
                    tracing::debug!(?session_id, "stale P2pAnswer ignored");
                    self.cleanup_session_attempt(session_id);
                    return;
                }
                // Answer candidates are peer dial targets for the initiator.
                // Without LAN P2P, keep the initiator's target set public-only.
                let peer_candidates = usable_p2p_candidates(candidates, self.allow_lan_candidates);
                if peer_candidates.is_empty() {
                    let current_matches = self.current_state_matches_session(session_id);
                    let fallback = self.family_fallback_for_session(session_id);
                    tracing::debug!(
                        ?session_id,
                        allow_lan_candidates = self.allow_lan_candidates,
                        "P2pAnswer has no usable peer candidates after local LAN policy filtering"
                    );
                    self.cleanup_session_attempt(session_id);
                    if let Some(fallback) = fallback {
                        if let Some(m) = self.metrics.as_ref() {
                            m.incr_p2p_attempt(P2pAttemptResult::NatFail);
                        }
                        if current_matches {
                            self.multi.set_state(P2pState::Idle);
                        }
                        self.enqueue_family_fallback(
                            fallback.peer_client_id,
                            fallback.local_client_id,
                            fallback.family,
                        );
                    } else if current_matches {
                        self.fail_and_cooldown(session_id, P2pAttemptResult::NatFail);
                    } else if let Some(m) = self.metrics.as_ref() {
                        m.incr_p2p_attempt(P2pAttemptResult::NatFail);
                    }
                    return;
                }
                let Some(ctx) = self.peer_contexts.get_mut(&session_id) else {
                    tracing::debug!(?session_id, "P2pAnswer context disappeared during handling");
                    return;
                };
                let accepted_client_id = accepted_client_id.trim().to_string();
                if !accepted_client_id.is_empty() {
                    self.peer_client_id_cache = Some(accepted_client_id.clone());
                    ctx.peer_client_id = Some(accepted_client_id.clone());
                    if let Some(installer) = self.p2p_installer.as_ref() {
                        installer.update_peer_client_id(session_id, &accepted_client_id);
                    }
                }
                self.peer_candidates_cache = peer_candidates.clone();
                self.peer_cert_fp_cache = Some(dst_cert_fp);
                ctx.candidates = peer_candidates;
                ctx.cert_fp = Some(dst_cert_fp);
                self.schedule_offer_answer_timeout(session_id);
                // State stays Negotiating until PunchSync arrives.
            }
            BinaryMessage::P2pPunchSync {
                session_id,
                t_start_ms,
                burst_count,
                port_offsets,
            } => {
                if self.v2_profile.is_some() {
                    tracing::warn!(?session_id, "legacy P2P PunchSync rejected by V2 manager");
                    return;
                }
                let (allow_parallel, session_role) = {
                    self.cancel_offer_answer_timeout(session_id);
                    let Some(ctx) = self.peer_contexts.get(&session_id) else {
                        tracing::debug!(?session_id, "P2pPunchSync for unknown session ignored");
                        send_teardown(&self.outbound, session_id).await;
                        return;
                    };
                    if ctx.candidates.is_empty() || ctx.cert_fp.is_none() {
                        tracing::warn!(
                            ?session_id,
                            has_cert_fp = ctx.cert_fp.is_some(),
                            candidates = ctx.candidates.len(),
                            "P2pPunchSync missing keyed peer context; aborting"
                        );
                        self.cleanup_session_attempt(session_id);
                        send_teardown(&self.outbound, session_id).await;
                        return;
                    }
                    let Some(session_role) = ctx.session_role else {
                        tracing::warn!(
                            ?session_id,
                            "P2pPunchSync missing session-specific role; aborting"
                        );
                        self.cleanup_session_attempt(session_id);
                        send_teardown(&self.outbound, session_id).await;
                        return;
                    };
                    (ctx.allow_parallel, session_role)
                };
                if !allow_parallel && !self.current_state_matches_session(session_id) {
                    tracing::info!(
                        ?session_id,
                        current = ?self.multi.p2p_state(),
                        "stale P2pPunchSync ignored"
                    );
                    self.cleanup_session_attempt(session_id);
                    send_teardown(&self.outbound, session_id).await;
                    return;
                }
                let track_session_state =
                    self.should_track_attempt_state(session_id, allow_parallel);
                if track_session_state {
                    let started = std::time::Instant::now();
                    self.multi
                        .set_state(crate::p2p::session::P2pState::Punching {
                            session_id,
                            started_at: started,
                        });
                }
                if session_role == ClientRole::Initiator {
                    self.spawn_punch_and_handshake(
                        session_id,
                        t_start_ms,
                        burst_count,
                        port_offsets,
                        track_session_state,
                    );
                } else {
                    // Legacy/test hook. The production listener no longer
                    // consumes this single slot; it receives the matched
                    // session_id from expected_peer_map.
                    if let Some(handle) = self.expected_session_id_handle.as_ref() {
                        match handle.lock() {
                            Ok(mut g) => *g = Some(session_id),
                            Err(e) => tracing::warn!(
                                ?e,
                                "legacy expected_session_id_handle poisoned; read-back slot not updated"
                            ),
                        }
                    }
                    let punch_socket = self.acceptor_punch_sockets.remove(&session_id);
                    if punch_socket.is_none()
                        && self.acceptor_responder_started.contains(&session_id)
                    {
                        tracing::info!(
                            ?session_id,
                            "p2p acceptor responder already started; PunchSync trigger ignored"
                        );
                        let _ = (burst_count, port_offsets, t_start_ms);
                        return;
                    }
                    let warmup_candidates = self
                        .peer_contexts
                        .get(&session_id)
                        .map(|ctx| {
                            let family = ctx.family;
                            usable_p2p_candidates(ctx.candidates.clone(), self.allow_lan_candidates)
                                .iter()
                                .filter_map(candidate_socket_addr)
                                .filter(|candidate| {
                                    family
                                        .map(|family| socket_addr_family(*candidate) == family)
                                        .unwrap_or(true)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    self.start_acceptor_punch_responder(
                        session_id,
                        t_start_ms,
                        punch_socket,
                        warmup_candidates,
                    );
                    let _ = (burst_count, port_offsets);
                }
            }
            BinaryMessage::P2pTeardown { session_id, .. } => {
                // Cancel any in-flight initiator punch for this
                // session before closing matching registry entries. Without
                // this, the spawned task could complete the handshake after
                // teardown and bypass cooldown.
                let current_matches = self.current_state_matches_session(session_id);
                self.cleanup_session_attempt(session_id);
                if !current_matches {
                    let cleared_replica = self
                        .p2p_installer
                        .as_ref()
                        .map(|installer| installer.close_installed_session(session_id))
                        .unwrap_or(false);
                    if cleared_replica {
                        tracing::debug!(
                            ?session_id,
                            "P2pTeardown cleared matching replica sidecar session"
                        );
                    } else {
                        tracing::debug!(
                            ?session_id,
                            "stale P2pTeardown cleaned up without touching current session"
                        );
                    }
                    return;
                }
                // Bump `failure_count` and stamp cooldown via the same
                // exponential-backoff path `fail_and_cooldown` uses on
                // outbound failures. Pre-fix the Teardown handler used a
                // fixed 60 s cooldown and never advanced `failure_count`,
                // so consecutive spawn-task failures cooled-down at the
                // base rate forever instead of 60→120→240→… per spec
                // The spawn-task path itself emits its own attempt-
                // result metric (NatFail / Timeout / CertFail), so this
                // path skips the metric bump to avoid double-counting.
                self.cooldown_matching_session_without_metric(session_id);
            }
            _ => {}
        }
    }

    /// Initiator-side punch + QUIC handshake driver (Task 4.6).
    ///
    /// Snapshots all per-attempt context BEFORE spawning so the future is
    /// `Send + 'static` (no `&self` / `&mut self` capture). On any failure,
    /// reports an internal event so the manager can clean local keyed state,
    /// then emits `P2pTeardown { reason: FatalError }` for the remote peer.
    fn spawn_punch_and_handshake(
        &mut self,
        session_id: tp_core::p2p_types::SessionId,
        t_start_ms: i64,
        burst_count: u8,
        port_offsets: Vec<i8>,
        track_session_state: bool,
    ) {
        use std::net::SocketAddr;
        use std::time::Duration;

        // Snapshot the per-attempt context BEFORE spawning so the future is 'static.
        let Some(context) = self.peer_contexts.get(&session_id).cloned() else {
            tracing::warn!(?session_id, "missing keyed peer context; aborting punch");
            self.fail_initiator_attempt_before_spawn(session_id, None);
            return;
        };
        // Punching dials peer candidates, not our own published candidates. The
        // local socket's public mapping is created when it sends to a public
        // peer target, so disabling LAN P2P means this target list is public-only.
        let peer_candidates =
            usable_p2p_candidates(context.candidates.clone(), self.allow_lan_candidates);
        let candidates: Vec<SocketAddr> = peer_candidates
            .iter()
            .filter_map(candidate_socket_addr)
            .collect();
        let expected_family = context.family;
        let lan_candidates = lan_p2p_socket_candidates(&candidates);
        let public_candidates: Vec<SocketAddr> = candidates
            .iter()
            .copied()
            .filter(|candidate| is_public_p2p_ip(candidate.ip()))
            .collect();
        let use_fresh_macos_ipv4_lan_socket = should_use_fresh_macos_ipv4_lan_socket(
            self.v2_profile.is_some(),
            cfg!(target_os = "macos"),
            expected_family,
            !lan_candidates.is_empty(),
            !public_candidates.is_empty(),
        );
        if candidates.is_empty() {
            tracing::debug!(
                ?session_id,
                raw_candidates = ?context.candidates,
                "no usable peer candidates parsed; aborting punch"
            );
            // No usable candidates ≈ NAT-class failure (we never even
            // got to send a probe).
            self.fail_initiator_attempt_before_spawn(session_id, Some(P2pAttemptResult::NatFail));
            return;
        }
        if let Some(expected_family) = expected_family {
            if candidate_socket_set_family(&candidates) != Some(expected_family) {
                tracing::warn!(
                    ?session_id,
                    family = expected_family.label(),
                    candidates = ?candidates,
                    "p2p punch candidate family mismatch; aborting"
                );
                self.fail_initiator_attempt_before_spawn(
                    session_id,
                    Some(P2pAttemptResult::NatFail),
                );
                return;
            }
        }
        // Snapshot announced (ip, port) tuples for the observability log
        // emitted after `wait_first_ack` returns. The acceptor's announced
        // quinn-listener port lives in this cache; the punch responder binds
        // a *different* ephemeral UDP port under the two-socket setup.
        // The log records both the probe-ack source (probe socket) and the
        // chosen dial target (announced quinn port) so ops can spot any
        // future regression where the two ports unexpectedly match again.
        let announced_candidates: Vec<(String, u16)> = peer_candidates
            .iter()
            .map(|c| (c.ip.clone(), c.port))
            .collect();
        let Some(peer_fp) = context.cert_fp else {
            tracing::warn!(
                ?session_id,
                "missing keyed peer cert fingerprint; aborting punch"
            );
            self.fail_initiator_attempt_before_spawn(session_id, None);
            return;
        };
        let tls_identity = self.tls_identity.clone();
        let client_id = context
            .local_client_id
            .clone()
            .unwrap_or_else(|| self.client_id.clone());
        let remote_peer_client_id = context
            .peer_client_id
            .clone()
            .unwrap_or_else(|| "<unknown>".to_string());
        let outbound = self.outbound.clone();
        let internal_tx = self.internal_tx.clone();
        let multi = self.multi.clone();
        let metrics = self.metrics.clone();
        let p2p_installer = self.p2p_installer.clone();
        let underlay_interface_indexes = self.underlay_interface_indexes;
        // Install a per-attempt cancellation token. The handler for
        // an inbound P2pTeardown for `session_id` cancels this token; the
        // spawned task checks it immediately before installing the QUIC
        // session so a teardown arriving mid-handshake cannot be raced.
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        // If a previous punch is still tracked (shouldn't normally happen —
        // PunchSync arrives once per Negotiation), cancel it before
        // overwriting the slot.
        if let Some(prev) = self.active_punch_cancel.insert(session_id, cancel) {
            prev.cancel();
        }
        let reserved_punch_socket = self.initiator_punch_sockets.remove(&session_id);

        self.task_tracker.spawn(async move {
            sleep_until_unix_ms(t_start_ms).await;

            let mapped_sock = match reserved_punch_socket {
                Some(socket) => match tokio::net::UdpSocket::from_std(socket) {
                    Ok(sock) => sock,
                    Err(e) => {
                        tracing::warn!(
                            ?session_id,
                            ?e,
                            "p2p initiator reserved socket tokio wrap failed"
                        );
                        report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                        return;
                    }
                },
                None => match bind_punch_socket_on_interfaces(
                    &candidates,
                    underlay_interface_indexes,
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(?session_id, ?e, "p2p initiator socket bind failed");
                        report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                        return;
                    }
                },
            };
            let mapped_socket_addr = match mapped_sock.local_addr() {
                Ok(addr) => addr,
                Err(e) => {
                    tracing::warn!(?session_id, ?e, "p2p initiator local socket query failed");
                    report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                    return;
                }
            };
            let Some(tls_identity) = tls_identity else {
                tracing::warn!(
                    ?session_id,
                    "p2p initiator missing local TLS identity; aborting punch"
                );
                report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                return;
            };

            #[cfg(target_os = "macos")]
            let mut fresh_lan_socket = if use_fresh_macos_ipv4_lan_socket {
                match bind_fresh_macos_ipv4_lan_punch_socket(underlay_interface_indexes) {
                    Ok(socket) => Some(socket),
                    Err(e) => {
                        tracing::debug!(
                            ?session_id,
                            ?e,
                            "p2p initiator fresh LAN socket bind failed; falling back to public candidates"
                        );
                        None
                    }
                }
            } else {
                None
            };
            #[cfg(not(target_os = "macos"))]
            let mut fresh_lan_socket: Option<(tokio::net::UdpSocket, SocketAddr)> = None;

            let gap = Duration::from_millis(20);
            let lan_probe_src =
                if !lan_candidates.is_empty() && lan_candidates.len() < candidates.len() {
                    let lan_sock = if use_fresh_macos_ipv4_lan_socket {
                        fresh_lan_socket.as_ref().map(|(socket, _)| socket)
                    } else {
                        Some(&mapped_sock)
                    };
                    if let Some(lan_sock) = lan_sock {
                        let lan_burst = crate::p2p::punch::BurstParams {
                            candidates: lan_candidates.clone(),
                            port_offsets: port_offsets.clone(),
                            burst_count,
                            gap,
                            session_id,
                        };
                        tracing::info!(
                            ?session_id,
                            targets = ?lan_candidates,
                            "p2p initiator LAN-preference burst starting"
                        );
                        if let Err(e) = crate::p2p::punch::send_burst(lan_sock, &lan_burst).await {
                            tracing::debug!(
                                ?session_id,
                                ?e,
                                "p2p initiator LAN-preference burst failed; falling back to remaining candidate set"
                            );
                            None
                        } else {
                            match crate::p2p::punch::wait_first_ack(
                                lan_sock,
                                session_id,
                                LAN_PUNCH_ACK_TIMEOUT,
                            )
                            .await
                            {
                                Ok(addr) => {
                                    tracing::info!(
                                        ?session_id,
                                        probe_src = %addr,
                                        "p2p initiator selected LAN candidate before public fallback"
                                    );
                                    Some(addr)
                                }
                                Err(e) => {
                                    tracing::info!(
                                        ?session_id,
                                        ?e,
                                        targets = ?lan_candidates,
                                        "p2p initiator LAN-preference window expired; falling back to remaining candidate set"
                                    );
                                    None
                                }
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

            let selected_fresh_lan_socket =
                if use_fresh_macos_ipv4_lan_socket && lan_probe_src.is_some() {
                    fresh_lan_socket.take()
                } else {
                    None
                };
            let (sock, local_socket_addr) = match selected_fresh_lan_socket {
                Some((fresh_sock, fresh_socket_addr)) => {
                    #[cfg(target_os = "macos")]
                    tune_fresh_macos_lan_punch_socket(&fresh_sock);
                    drop(mapped_sock);
                    (fresh_sock, fresh_socket_addr)
                }
                None => {
                    drop(fresh_lan_socket);
                    (mapped_sock, mapped_socket_addr)
                }
            };
            let full_candidates = if use_fresh_macos_ipv4_lan_socket && lan_probe_src.is_none() {
                public_candidates
            } else {
                candidates.clone()
            };

            let probe_src = match lan_probe_src {
                Some(addr) => addr,
                None => {
                    let burst = crate::p2p::punch::BurstParams {
                        candidates: full_candidates.clone(),
                        port_offsets,
                        burst_count,
                        gap,
                        session_id,
                    };
                    if let Err(e) = crate::p2p::punch::send_burst(&sock, &burst).await {
                        tracing::warn!(
                            ?session_id,
                            family = expected_family.map(P2pAddressFamily::label),
                            ?e,
                            "p2p initiator burst failed"
                        );
                        report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                        return;
                    }

                    match crate::p2p::punch::wait_first_ack(
                        &sock,
                        session_id,
                        PUNCH_ACK_TIMEOUT,
                    )
                    .await
                    {
                        Ok(addr) => addr,
                        Err(e) => {
                            tracing::warn!(
                                ?session_id,
                                family = expected_family.map(P2pAddressFamily::label),
                                ?e,
                                targets = ?announced_candidates,
                                "no ProbeAck arrived; trying direct announced candidates before relay fallback"
                            );
                            let mut targets =
                                probe_timeout_direct_targets_for_socket(&sock, &full_candidates);
                            if let Some(family) = expected_family {
                                targets = filter_socket_addrs_for_family(targets, family);
                            }
                            if let Some(dial_target) = targets.first().copied() {
                                tracing::info!(
                                    ?session_id,
                                    family = expected_family.map(P2pAddressFamily::label),
                                    dial_target = %dial_target,
                                    "p2p direct candidate fallback dial starting on punched socket"
                                );
                                let std_sock = match sock.into_std() {
                                    Ok(s) => s,
                                    Err(e) => {
                                        tracing::warn!(
                                            ?session_id,
                                            ?e,
                                            "p2p direct fallback into_std failed"
                                        );
                                        report_initiator_attempt_failed(
                                            &internal_tx,
                                            &outbound,
                                            session_id,
                                        )
                                        .await;
                                        return;
                                    }
                                };
                                if track_session_state {
                                    multi.set_state(
                                        crate::p2p::session::P2pState::HandshakingQuic {
                                            session_id,
                                        },
                                    );
                                }
                                match tokio::time::timeout(
                                    Duration::from_secs(3),
                                    build_p2p_client_endpoint(
                                        std_sock,
                                        peer_fp,
                                        dial_target,
                                        &tls_identity,
                                        &client_id,
                                        session_id,
                                    ),
                                )
                                .await
                                {
                                    Ok(Ok(session)) => {
                                        let _ = install_successful_p2p_session(
                                            session_id,
                                            session,
                                            &remote_peer_client_id,
                                            dial_target,
                                            local_socket_addr,
                                            &peer_candidates,
                                            underlay_interface_indexes,
                                            &multi,
                                            metrics.as_ref(),
                                            p2p_installer.as_ref(),
                                            &cancel_for_task,
                                            track_session_state,
                                            &internal_tx,
                                            &outbound,
                                        )
                                        .await;
                                        return;
                                    }
                                    Ok(Err(e)) => {
                                        tracing::warn!(
                                            ?session_id,
                                            family = expected_family.map(P2pAddressFamily::label),
                                            dial_target = %dial_target,
                                            ?e,
                                            "p2p direct candidate fallback dial failed"
                                        );
                                    }
                                    Err(_) => {
                                        tracing::debug!(
                                            ?session_id,
                                            family = expected_family.map(P2pAddressFamily::label),
                                            dial_target = %dial_target,
                                            "p2p direct candidate fallback dial timed out"
                                        );
                                    }
                                }
                            }
                            // No probe ack within the configured punch window and direct host-candidate
                            // fallback failed = timeout-class attempt failure
                            //. This is the ONLY metric emission for
                            // this attempt — the local failure event intentionally
                            // cools down without emitting another attempt metric, so
                            // `timeout` is not double-counted as `nat_fail`.
                            if let Some(m) = metrics.as_ref() {
                                m.incr_p2p_attempt(P2pAttemptResult::Timeout);
                            }
                            report_initiator_attempt_failed(&internal_tx, &outbound, session_id)
                                .await;
                            return;
                        }
                    }
                }
            };

            // Same-socket punching: the responder answers the Probe and then
            // turns that same UDP socket into the QUIC server endpoint. The
            // ProbeAck source address is therefore the exact address:port to
            // dial. `select_dial_target` returns `None` when no
            // announced candidate matches the probe-ack family + IP.
            // Treat that as a NAT-class attempt failure (we never even
            // get to attempt QUIC), bump the metric, send Teardown, and
            // abort. Mirrors the Timeout branch above: the local failure
            // event handles cleanup/cooldown, and the Teardown only notifies
            // the remote peer.
            let dial_target = match select_dial_target(probe_src, &candidates) {
                Some(target) => target,
                None => {
                    tracing::warn!(
                        ?session_id,
                        ?probe_src,
                        candidates = ?candidates,
                        "p2p dial target selection found no IP-and-family match; aborting punch as nat_fail"
                    );
                    if let Some(m) = metrics.as_ref() {
                        m.incr_p2p_attempt(P2pAttemptResult::NatFail);
                    }
                    report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                    return;
                }
            };
            let dial_target_matches_probe_src_port =
                dial_target.port() == probe_src.port();
            tracing::info!(
                ?session_id,
                family = expected_family.map(P2pAddressFamily::label),
                probe_src = %probe_src,
                dial_target = %dial_target,
                announced_candidates = ?announced_candidates,
                dial_target_matches_probe_src_port,
                "p2p same-socket check: dial target selected from ProbeAck source"
            );

            let std_sock = match sock.into_std() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(?session_id, ?e, "into_std failed");
                    report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                    return;
                }
            };

            if track_session_state {
                multi.set_state(crate::p2p::session::P2pState::HandshakingQuic { session_id });
            }
            match build_p2p_client_endpoint(
                std_sock,
                peer_fp,
                dial_target,
                &tls_identity,
                &client_id,
                session_id,
            )
            .await
            {
                Ok(session) => {
                    let _ = install_successful_p2p_session(
                        session_id,
                        session,
                        &remote_peer_client_id,
                        dial_target,
                        local_socket_addr,
                        &peer_candidates,
                        underlay_interface_indexes,
                        &multi,
                        metrics.as_ref(),
                        p2p_installer.as_ref(),
                        &cancel_for_task,
                        track_session_state,
                        &internal_tx,
                        &outbound,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::warn!(?session_id, ?e, "p2p QUIC handshake failed");
                    // QUIC handshake failure most often = cert/TLS issue;
                    // tag as `cert_fail` for visibility.
                    if let Some(m) = metrics.as_ref() {
                        m.incr_p2p_attempt(P2pAttemptResult::CertFail);
                    }
                    report_initiator_attempt_failed(&internal_tx, &outbound, session_id).await;
                }
            }
        });
    }

    /// Spawn a short-lived UDP responder that answers `P2pProbe` frames for
    /// `session_id` until either the responder probe window elapses or the task is dropped.
    /// Acceptor-side counterpart to the initiator's burst (Task 3.3).
    fn spawn_punch_responder(
        &mut self,
        session_id: tp_core::p2p_types::SessionId,
        t_start_ms: i64,
        reserved_socket: Option<std::net::UdpSocket>,
        warmup_candidates: Vec<std::net::SocketAddr>,
    ) {
        let internal_tx = self.internal_tx.clone();
        let tls_identity = self.tls_identity.clone();
        let expected_peer_map = self.expected_peer_map.clone();
        let metrics = self.metrics.clone();
        let p2p_installer = self.p2p_installer.clone();
        let underlay_interface_indexes = self.underlay_interface_indexes;
        let (remote_candidates, remote_peer_client_id) = self
            .peer_contexts
            .get(&session_id)
            .map(|context| {
                (
                    context.candidates.clone(),
                    context
                        .peer_client_id
                        .clone()
                        .unwrap_or_else(|| "<unknown>".to_string()),
                )
            })
            .unwrap_or_else(|| (Vec::new(), "<unknown>".to_string()));
        self.task_tracker.spawn(async move {
            sleep_until_unix_ms(t_start_ms).await;
            let std_sock = match reserved_socket {
                Some(socket) => socket,
                None => match bind_std_p2p_socket_for_candidates_on_interfaces(
                    &warmup_candidates,
                    underlay_interface_indexes,
                ) {
                    Ok(socket) => {
                        tracing::warn!(
                            ?session_id,
                            "p2p responder had no reserved socket; bound fallback socket"
                        );
                        socket
                    }
                    Err(e) => {
                        tracing::warn!(?e, "p2p probe responder bind failed");
                        let _ = internal_tx
                            .send(P2pInternalEvent::CleanupSessionAttempt { session_id });
                        return;
                    }
                },
            };
            let local_addr = match std_sock.local_addr() {
                Ok(addr) => addr,
                Err(e) => {
                    tracing::warn!(?session_id, ?e, "p2p responder local socket query failed");
                    let _ =
                        internal_tx.send(P2pInternalEvent::CleanupSessionAttempt { session_id });
                    return;
                }
            };
            let sock = match tokio::net::UdpSocket::from_std(std_sock) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(?e, "p2p probe responder tokio socket wrap failed");
                    let _ =
                        internal_tx.send(P2pInternalEvent::CleanupSessionAttempt { session_id });
                    return;
                }
            };
            if !warmup_candidates.is_empty() {
                let burst = crate::p2p::punch::BurstParams {
                    candidates: warmup_candidates.clone(),
                    port_offsets: vec![0],
                    burst_count: 3,
                    gap: std::time::Duration::from_millis(20),
                    session_id,
                };
                tracing::info!(
                    ?session_id,
                    local_addr = ?local_addr,
                    candidates = ?warmup_candidates,
                    "p2p acceptor warmup burst starting"
                );
                if let Err(e) = crate::p2p::punch::send_burst(&sock, &burst).await {
                    tracing::warn!(
                        ?session_id,
                        error = %e,
                        "p2p acceptor warmup burst failed"
                    );
                }
            }
            let probe_deadline = tokio::time::Instant::now() + RESPONDER_PROBE_WINDOW;
            let mut buf = [0u8; 1500];
            // Per-session replay window: drop a P2pProbe whose `seq` is
            // lower than the per-session max-seen − 64. Without the gate a
            // passive observer could replay a captured ProbeAck back to the
            // responder for a *new* session and induce work; bounded by the
            // responder's bounded lifetime and the same-session_id
            // requirement, but the gate is what actually closes it.
            let mut max_seen_seq: u32 = 0;
            let mut have_seen_any: bool = false;
            let mut answered_probe = false;
            while tokio::time::Instant::now() < probe_deadline {
                let res = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    sock.recv_from(&mut buf),
                )
                .await;
                let Ok(Ok((n, src))) = res else { continue };
                if let Ok(parsed) = tp_core::protocol::unpack(&buf[..n]) {
                    if let BinaryMessage::P2pProbe {
                        session_id: sid,
                        seq,
                        ..
                    } = &parsed
                    {
                        if *sid != session_id {
                            continue;
                        }
                        if !accept_probe_seq(*seq, &mut max_seen_seq, &mut have_seen_any) {
                            tracing::debug!(
                                ?session_id,
                                seq,
                                max_seen_seq,
                                "p2p probe replay-window reject"
                            );
                            continue;
                        }
                        tracing::info!(
                            ?session_id,
                            seq,
                            family = if src.is_ipv6() { "ipv6" } else { "ipv4" },
                            source = %src,
                            local_addr = ?local_addr,
                            "p2p probe received"
                        );
                        let _ = crate::p2p::punch::answer_probe(&sock, src, &parsed).await;
                        answered_probe = true;
                        tracing::info!(
                            ?session_id,
                            responder_addr = ?local_addr,
                            probe_src = %src,
                            "p2p probe ack sent; switching reserved socket to QUIC accept"
                        );
                        break;
                    }
                }
            }

            let std_sock = match sock.into_std() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(?session_id, ?e, "p2p responder into_std failed");
                    let _ =
                        internal_tx.send(P2pInternalEvent::CleanupSessionAttempt { session_id });
                    return;
                }
            };
            let has_tls_identity = tls_identity.is_some();
            let has_expected_peer_map = expected_peer_map.is_some();
            let has_installer = p2p_installer.is_some();
            let (Some(tls_identity), Some(expected_peer_map), Some(p2p_installer)) =
                (tls_identity, expected_peer_map, p2p_installer)
            else {
                tracing::warn!(
                    ?session_id,
                    has_tls_identity,
                    has_expected_peer_map,
                    has_installer,
                    "p2p responder missing install prerequisites; cleaning up"
                );
                let _ = internal_tx.send(P2pInternalEvent::CleanupSessionAttempt { session_id });
                return;
            };
            let endpoint = match crate::p2p::listener::endpoint_from_socket(&tls_identity, std_sock)
            {
                Ok(endpoint) => endpoint,
                Err(e) => {
                    tracing::warn!(?session_id, ?e, "p2p responder QUIC endpoint build failed");
                    let _ =
                        internal_tx.send(P2pInternalEvent::CleanupSessionAttempt { session_id });
                    return;
                }
            };
            tracing::info!(
                ?session_id,
                responder_addr = ?local_addr,
                family = socket_addr_family(local_addr).label(),
                answered_probe,
                "p2p responder waiting for QUIC on reserved socket"
            );
            let installed = match crate::p2p::listener::accept_one_session(
                endpoint,
                expected_peer_map,
                metrics,
                RESPONDER_QUIC_ACCEPT_TIMEOUT,
            )
            .await
            {
                Some((accepted_session_id, session)) if accepted_session_id == session_id => {
                    let successful_path = successful_direct_path_observation(
                        &remote_peer_client_id,
                        session.peer_addr(),
                        local_addr,
                        &remote_candidates,
                        underlay_interface_indexes,
                    );
                    match p2p_installer.install_reserved(session_id, session).await {
                        Ok(_) => {
                            log_successful_direct_path(session_id, "acceptor", &successful_path);
                            let _ =
                                internal_tx.send(P2pInternalEvent::SessionInstalled { session_id });
                            true
                        }
                        Err(e) => {
                            tracing::warn!(
                                ?session_id,
                                error = %e,
                                "failed to install accepted P2P session from reserved socket"
                            );
                            false
                        }
                    }
                }
                Some((accepted_session_id, _session)) => {
                    tracing::warn!(
                        ?session_id,
                        ?accepted_session_id,
                        "p2p responder accepted unexpected session id"
                    );
                    false
                }
                None => {
                    tracing::debug!(
                        ?session_id,
                        family = socket_addr_family(local_addr).label(),
                        answered_probe,
                        "p2p responder QUIC accept timed out"
                    );
                    false
                }
            };
            if !installed {
                let _ = internal_tx.send(P2pInternalEvent::CleanupSessionAttempt { session_id });
            }
        });
    }
}

/// Build a QUIC client endpoint over the punched UDP socket and dial the
/// remote with a cert-fingerprint-pinned client config. Returns a wrapped
/// `Session` driving the relay-style outbound/inbound pumps over a single
/// bi-directional stream (parity with the relay handshake at quic.rs:691).
async fn build_p2p_client_endpoint(
    sock: std::net::UdpSocket,
    expected_peer_fp: tp_core::p2p_types::CertFingerprint,
    peer: std::net::SocketAddr,
    identity: &crate::p2p::cert::CertBundle,
    client_id: &str,
    session_id: SessionId,
) -> Result<tp_transport::session::Session, anyhow::Error> {
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no tokio runtime in p2p endpoint build"))?;
    let mut endpoint = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, sock, runtime)?;
    let mut client_cfg =
        crate::p2p::tls::make_pinned_client_config_with_identity(expected_peer_fp, identity);
    client_cfg.transport_config(Arc::new(tp_transport::quic::tuned_transport_config(
        &crate::p2p::p2p_quic_tuning(),
    )));
    endpoint.set_default_client_config(client_cfg);
    let conn = endpoint.connect(peer, "p2p")?.await?;
    let (mut send, recv) = conn.open_bi().await?;
    write_p2p_stream_session_preface(&mut send, session_id).await?;
    let control = tp_transport::quic::open_p2p_control_lane(&conn).await;
    let session = tp_transport::quic::wrap_for_p2p_with_control(conn, send, recv, control);
    session
        .send(BinaryMessage::Heartbeat {
            client_id: client_id.to_string(),
            timestamp: unix_timestamp_secs(),
        })
        .await?;
    Ok(session)
}

async fn write_p2p_stream_session_preface(
    send: &mut quinn::SendStream,
    session_id: SessionId,
) -> Result<(), anyhow::Error> {
    let packed = tp_core::protocol::pack(&BinaryMessage::P2pSessionReady {
        session_id,
        rtt_us: 0,
        chosen_remote_ip: String::new(),
        chosen_remote_port: 0,
    })
    .to_bytes();
    if packed.len() as u64 > tp_transport::MAX_FRAME_LEN as u64 {
        anyhow::bail!("p2p stream session preface too large: {}", packed.len());
    }
    send.write_all(&(packed.len() as u32).to_be_bytes()).await?;
    send.write_all(&packed).await?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SuccessfulDirectPathObservation {
    peer_client_id: String,
    remote_candidate_ip: IpAddr,
    remote_candidate_kind: Option<tp_core::p2p_types::CandidateKind>,
    socket_family: P2pAddressFamily,
    selected_ifindex: Option<NonZeroU32>,
}

fn successful_direct_path_observation(
    peer_client_id: &str,
    remote_addr: std::net::SocketAddr,
    local_socket_addr: std::net::SocketAddr,
    remote_candidates: &[tp_core::p2p_types::Candidate],
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
) -> SuccessfulDirectPathObservation {
    let exact_kind = remote_candidates.iter().find_map(|candidate| {
        candidate_socket_addr(candidate)
            .filter(|candidate_addr| *candidate_addr == remote_addr)
            .map(|_| candidate.kind)
    });
    let remote_candidate_kind = exact_kind.or_else(|| {
        let mut matching_kinds = remote_candidates.iter().filter_map(|candidate| {
            candidate
                .ip
                .parse::<IpAddr>()
                .ok()
                .filter(|ip| *ip == remote_addr.ip())
                .map(|_| candidate.kind)
        });
        let first = matching_kinds.next()?;
        matching_kinds.all(|kind| kind == first).then_some(first)
    });
    let socket_family = socket_addr_family(local_socket_addr);
    let selected_ifindex = underlay_interface_indexes.and_then(|indexes| match socket_family {
        P2pAddressFamily::Ipv4 => indexes.ipv4,
        P2pAddressFamily::Ipv6 => indexes.ipv6,
    });

    SuccessfulDirectPathObservation {
        peer_client_id: peer_client_id.to_string(),
        remote_candidate_ip: remote_addr.ip(),
        remote_candidate_kind,
        socket_family,
        selected_ifindex,
    }
}

fn candidate_kind_label(kind: Option<tp_core::p2p_types::CandidateKind>) -> &'static str {
    match kind {
        Some(tp_core::p2p_types::CandidateKind::Host) => "host",
        Some(tp_core::p2p_types::CandidateKind::ServerReflexive) => "server_reflexive",
        None => "unknown",
    }
}

fn log_successful_direct_path(
    session_id: SessionId,
    side: &'static str,
    observation: &SuccessfulDirectPathObservation,
) {
    // Keep this as a scalar for acceptance-log parsers; zero is the stable
    // sentinel for an explicitly unpinned P2P generation.
    let selected_ifindex = observation
        .selected_ifindex
        .map(NonZeroU32::get)
        .unwrap_or(0);
    // Exact candidate addresses are useful for opt-in acceptance diagnostics,
    // but must not enter the default INFO log stream.
    tracing::debug!(
        ?session_id,
        side,
        peer_client_id = %observation.peer_client_id,
        remote_candidate_ip = %observation.remote_candidate_ip,
        kind = candidate_kind_label(observation.remote_candidate_kind),
        socket_family = observation.socket_family.label(),
        selected_ifindex,
        "P2P direct QUIC session installed on selected underlay"
    );
}

// Installation spans the negotiated transport, cancellation, metrics and signaling owners.
#[allow(clippy::too_many_arguments)]
async fn install_successful_p2p_session(
    session_id: SessionId,
    session: tp_transport::session::Session,
    peer_client_id: &str,
    dial_target: std::net::SocketAddr,
    local_socket_addr: std::net::SocketAddr,
    remote_candidates: &[tp_core::p2p_types::Candidate],
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
    multi: &Arc<MultiSession>,
    metrics: Option<&Arc<MetricsManager>>,
    p2p_installer: Option<&P2pSessionInstaller>,
    cancel_for_task: &CancellationToken,
    track_session_state: bool,
    internal_tx: &tokio::sync::mpsc::UnboundedSender<P2pInternalEvent>,
    outbound: &tokio::sync::mpsc::Sender<tp_core::protocol::BinaryMessage>,
) -> Option<SuccessfulDirectPathObservation> {
    let successful_path = successful_direct_path_observation(
        peer_client_id,
        session.peer_addr(),
        local_socket_addr,
        remote_candidates,
        underlay_interface_indexes,
    );
    // A P2pTeardown handler may have cancelled this attempt while
    // the QUIC handshake was in flight. If so, do NOT install — the
    // handler already cleared `multi.p2p` and stamped Cooldown.
    if cancel_for_task.is_cancelled() {
        tracing::debug!(
            ?session_id,
            "p2p punch cancelled mid-handshake (Teardown raced); skipping install"
        );
        return None;
    }
    let (installed, legacy_installed) = if let Some(installer) = p2p_installer {
        match try_install_p2p_session_with_installer(
            installer,
            session_id,
            session,
            cancel_for_task,
        )
        .await
        {
            Ok(Some(installed)) => (Some(installed), false),
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    ?session_id,
                    error = %e,
                    "p2p session installer failed"
                );
                report_initiator_attempt_failed(internal_tx, outbound, session_id).await;
                return None;
            }
        }
    } else {
        if !track_session_state {
            tracing::warn!(
                ?session_id,
                "parallel p2p install without installer is unsafe; aborting"
            );
            report_initiator_attempt_failed(internal_tx, outbound, session_id).await;
            return None;
        }
        if !try_install_p2p_session(multi, Arc::new(session), cancel_for_task) {
            return None;
        }
        multi.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });
        (None, true)
    };
    if cancel_for_task.is_cancelled() {
        rollback_cancelled_p2p_install(multi, session_id, installed.as_ref(), legacy_installed);
        tracing::info!(
            ?session_id,
            "p2p punch cancelled after install; rolled back session"
        );
        return None;
    }
    log_successful_direct_path(session_id, "initiator", &successful_path);
    // Telemetry: count one successful Announce → Active transition.
    // Counted here (not on P2pAnnounceAck) so the metric reflects
    // sessions that actually became serviceable.
    if let Some(m) = metrics {
        m.incr_p2p_attempt(P2pAttemptResult::Success);
    }
    let _ = internal_tx.send(P2pInternalEvent::SessionInstalled { session_id });
    let ready = tp_core::protocol::BinaryMessage::P2pSessionReady {
        session_id,
        rtt_us: 0,
        chosen_remote_ip: dial_target.ip().to_string(),
        chosen_remote_port: dial_target.port(),
    };
    let _ = outbound.send(ready).await;
    Some(successful_path)
}

pub fn probe_timeout_direct_targets(
    candidates: &[std::net::SocketAddr],
) -> Vec<std::net::SocketAddr> {
    let mut targets = Vec::new();
    for candidate in candidates {
        if !targets.contains(candidate) {
            targets.push(*candidate);
        }
    }
    targets
}

fn probe_timeout_direct_targets_for_socket(
    sock: &tokio::net::UdpSocket,
    candidates: &[std::net::SocketAddr],
) -> Vec<std::net::SocketAddr> {
    let local_addr = sock.local_addr().ok();
    filter_socket_addrs_for_bind_addr(probe_timeout_direct_targets(candidates), local_addr)
}

fn filter_candidates_for_bind_addr(
    candidates: Vec<tp_core::p2p_types::Candidate>,
    bind_addr: Option<std::net::SocketAddr>,
) -> Vec<tp_core::p2p_types::Candidate> {
    match bind_addr {
        Some(addr) => candidates
            .into_iter()
            .filter(|candidate| {
                candidate
                    .ip
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_ipv4() == addr.is_ipv4())
                    .unwrap_or(false)
            })
            .collect(),
        None => candidates,
    }
}

fn filter_socket_addrs_for_bind_addr(
    addrs: Vec<std::net::SocketAddr>,
    bind_addr: Option<std::net::SocketAddr>,
) -> Vec<std::net::SocketAddr> {
    match bind_addr {
        Some(bind_addr) => addrs
            .into_iter()
            .filter(|addr| addr.is_ipv4() == bind_addr.is_ipv4())
            .collect(),
        None => addrs,
    }
}

/// Emit a `P2pTeardown { reason: FatalError }` frame on the relay outbound.
/// Used by the initiator punch driver on any failure path.
async fn send_teardown(
    outbound: &tokio::sync::mpsc::Sender<tp_core::protocol::BinaryMessage>,
    session_id: tp_core::p2p_types::SessionId,
) {
    let _ = outbound
        .send(tp_core::protocol::BinaryMessage::P2pTeardown {
            session_id,
            reason: tp_core::p2p_types::TeardownReason::FatalError,
        })
        .await;
}

async fn report_initiator_attempt_failed(
    internal_tx: &tokio::sync::mpsc::UnboundedSender<P2pInternalEvent>,
    outbound: &tokio::sync::mpsc::Sender<tp_core::protocol::BinaryMessage>,
    session_id: tp_core::p2p_types::SessionId,
) {
    let _ = internal_tx.send(P2pInternalEvent::InitiatorAttemptFailed { session_id });
    send_teardown(outbound, session_id).await;
}

/// Filter peer P2P candidates down to globally routable addresses.
pub fn public_p2p_candidates(
    candidates: Vec<tp_core::p2p_types::Candidate>,
) -> Vec<tp_core::p2p_types::Candidate> {
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate
                .ip
                .parse::<std::net::IpAddr>()
                .map(is_public_p2p_ip)
                .unwrap_or(false)
        })
        .collect()
}

fn usable_p2p_candidates(
    candidates: Vec<tp_core::p2p_types::Candidate>,
    allow_lan_candidates: bool,
) -> Vec<tp_core::p2p_types::Candidate> {
    // This helper is used both when publishing local candidates and when
    // selecting peer punch targets. With LAN P2P disabled, both directions are
    // public-only so neither side can accidentally mix LAN and public paths.
    if !allow_lan_candidates {
        return public_p2p_candidates(candidates);
    }
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate
                .ip
                .parse::<std::net::IpAddr>()
                .map(|ip| is_public_p2p_ip(ip) || is_lan_p2p_ip(ip))
                .unwrap_or(false)
        })
        .collect()
}

fn filter_local_host_candidates_for_underlay(
    candidates: Vec<tp_core::p2p_types::Candidate>,
    selected_host_ips: Option<&BTreeSet<IpAddr>>,
) -> Vec<tp_core::p2p_types::Candidate> {
    let Some(selected_host_ips) = selected_host_ips else {
        return candidates;
    };
    candidates
        .into_iter()
        .filter(|candidate| {
            if candidate.kind != tp_core::p2p_types::CandidateKind::Host {
                return true;
            }
            candidate
                .ip
                .parse::<IpAddr>()
                .is_ok_and(|ip| selected_host_ips.contains(&ip))
        })
        .collect()
}

fn candidate_family(candidate: &tp_core::p2p_types::Candidate) -> Option<P2pAddressFamily> {
    candidate.ip.parse::<std::net::IpAddr>().ok().map(ip_family)
}

fn socket_addr_family(addr: std::net::SocketAddr) -> P2pAddressFamily {
    ip_family(addr.ip())
}

fn ip_family(ip: std::net::IpAddr) -> P2pAddressFamily {
    match ip {
        std::net::IpAddr::V6(_) => P2pAddressFamily::Ipv6,
        std::net::IpAddr::V4(_) => P2pAddressFamily::Ipv4,
    }
}

fn push_family_once(families: &mut Vec<P2pAddressFamily>, family: P2pAddressFamily) {
    if !families.contains(&family) {
        families.push(family);
    }
}

fn filter_candidates_for_family(
    candidates: Vec<tp_core::p2p_types::Candidate>,
    family: P2pAddressFamily,
) -> Vec<tp_core::p2p_types::Candidate> {
    candidates
        .into_iter()
        .filter(|candidate| candidate_family(candidate) == Some(family))
        .collect()
}

fn filter_socket_addrs_for_family(
    addrs: Vec<std::net::SocketAddr>,
    family: P2pAddressFamily,
) -> Vec<std::net::SocketAddr> {
    addrs
        .into_iter()
        .filter(|addr| socket_addr_family(*addr) == family)
        .collect()
}

fn candidate_set_family(candidates: &[tp_core::p2p_types::Candidate]) -> Option<P2pAddressFamily> {
    let mut family = None;
    for candidate in candidates {
        let candidate_family = candidate_family(candidate)?;
        match family {
            Some(existing) if existing != candidate_family => return None,
            Some(_) => {}
            None => family = Some(candidate_family),
        }
    }
    family
}

fn candidate_socket_set_family(candidates: &[std::net::SocketAddr]) -> Option<P2pAddressFamily> {
    let mut family = None;
    for candidate in candidates {
        let candidate_family = socket_addr_family(*candidate);
        match family {
            Some(existing) if existing != candidate_family => return None,
            Some(_) => {}
            None => family = Some(candidate_family),
        }
    }
    family
}

fn preferred_local_p2p_families(
    raw_candidates: &[tp_core::p2p_types::Candidate],
    allow_lan_candidates: bool,
) -> Vec<P2pAddressFamily> {
    let usable = usable_p2p_candidates(raw_candidates.to_vec(), allow_lan_candidates);
    let mut families = Vec::new();
    if usable
        .iter()
        .any(|candidate| candidate_family(candidate) == Some(P2pAddressFamily::Ipv6))
    {
        families.push(P2pAddressFamily::Ipv6);
    }
    if usable
        .iter()
        .any(|candidate| candidate_family(candidate) == Some(P2pAddressFamily::Ipv4))
    {
        families.push(P2pAddressFamily::Ipv4);
    }
    families
}

fn local_p2p_candidates(
    _p2p_listener_port: u16,
    observed_public_addr: Option<std::net::SocketAddr>,
    raw_candidates: Vec<tp_core::p2p_types::Candidate>,
    allow_lan_candidates: bool,
) -> Vec<tp_core::p2p_types::Candidate> {
    let mut candidates = usable_p2p_candidates(raw_candidates, allow_lan_candidates);
    if let Some(observed) = observed_public_addr.filter(|addr| is_public_p2p_ip(addr.ip())) {
        let srflx = tp_core::p2p_types::Candidate {
            ip: observed.ip().to_string(),
            port: observed.port(),
            kind: tp_core::p2p_types::CandidateKind::ServerReflexive,
        };
        if !candidates
            .iter()
            .any(|c| c.ip == srflx.ip && c.port == srflx.port)
        {
            candidates.push(srflx);
        }
    }
    candidates
}

fn local_p2p_candidates_for_family(
    p2p_port: u16,
    family: P2pAddressFamily,
    observed_public_addr: Option<std::net::SocketAddr>,
    raw_candidates: Vec<tp_core::p2p_types::Candidate>,
    allow_lan_candidates: bool,
) -> Vec<tp_core::p2p_types::Candidate> {
    let raw_candidates = filter_candidates_for_family(raw_candidates, family);
    let observed_public_addr =
        observed_public_addr.filter(|addr| socket_addr_family(*addr) == family);
    local_p2p_candidates(
        p2p_port,
        observed_public_addr,
        raw_candidates,
        allow_lan_candidates,
    )
}

fn announce_locals_from_candidates(
    raw_candidates: Vec<tp_core::p2p_types::Candidate>,
    allow_lan_candidates: bool,
) -> Vec<(String, u16)> {
    usable_p2p_candidates(raw_candidates, allow_lan_candidates)
        .into_iter()
        .map(|candidate| (candidate.ip, candidate.port))
        .collect()
}

fn lan_p2p_socket_candidates(candidates: &[std::net::SocketAddr]) -> Vec<std::net::SocketAddr> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| is_lan_p2p_ip(candidate.ip()))
        .collect()
}

fn should_use_fresh_macos_ipv4_lan_socket(
    has_v2_profile: bool,
    is_macos: bool,
    family: Option<P2pAddressFamily>,
    has_lan_candidate: bool,
    has_public_candidate: bool,
) -> bool {
    has_v2_profile
        && is_macos
        && family == Some(P2pAddressFamily::Ipv4)
        && has_lan_candidate
        && has_public_candidate
}

fn session_id_hex(session_id: SessionId) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(tp_core::p2p_types::SESSION_ID_SIZE * 2);
    for byte in session_id.as_bytes() {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn candidate_socket_addr(
    candidate: &tp_core::p2p_types::Candidate,
) -> Option<std::net::SocketAddr> {
    let ip = candidate.ip.parse::<std::net::IpAddr>().ok()?;
    Some(std::net::SocketAddr::new(ip, candidate.port))
}

fn required_underlay_index_for_family(
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
    family: P2pAddressFamily,
) -> std::io::Result<Option<NonZeroU32>> {
    let Some(indexes) = underlay_interface_indexes else {
        return Ok(None);
    };
    let index = match family {
        P2pAddressFamily::Ipv4 => indexes.ipv4,
        P2pAddressFamily::Ipv6 => indexes.ipv6,
    };
    index.map(Some).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "selected P2P underlay adapter has no index for the requested address family",
        )
    })
}

fn bind_std_p2p_socket_for_family_on_interfaces(
    family: P2pAddressFamily,
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
) -> std::io::Result<std::net::UdpSocket> {
    let unspecified_addr: std::net::SocketAddr = match family {
        P2pAddressFamily::Ipv6 => "[::]:0".parse().unwrap(),
        P2pAddressFamily::Ipv4 => "0.0.0.0:0".parse().unwrap(),
    };
    let interface_index = required_underlay_index_for_family(underlay_interface_indexes, family)?;
    let bind_addr = underlay_interface_indexes
        .map(|indexes| indexes.bind_addr_for(unspecified_addr))
        .transpose()?
        .unwrap_or(unspecified_addr);
    tp_transport::quic::bind_tuned_udp_on_interface(bind_addr, interface_index)
}

#[cfg(target_os = "macos")]
fn bind_fresh_macos_ipv4_lan_punch_socket(
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
) -> std::io::Result<(tokio::net::UdpSocket, std::net::SocketAddr)> {
    let indexes = underlay_interface_indexes.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "fresh LAN punch socket requires a selected underlay adapter",
        )
    })?;
    let interface_index = indexes.ipv4.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "fresh LAN punch socket requires an IPv4 underlay interface index",
        )
    })?;
    let source_ip = indexes.ipv4_source_ip.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "fresh LAN punch socket requires an exact IPv4 source address",
        )
    })?;

    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.bind_device_by_index_v4(Some(interface_index))?;
    socket.set_reuse_address(false)?;
    socket.bind(&std::net::SocketAddrV4::new(source_ip, 0).into())?;
    let local_addr = socket
        .local_addr()?
        .as_socket_ipv4()
        .filter(|addr| *addr.ip() == source_ip && addr.port() != 0)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "fresh LAN punch socket source mismatch",
            )
        })?;
    socket.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(socket.into())?;
    Ok((socket, std::net::SocketAddr::V4(local_addr)))
}

#[cfg(target_os = "macos")]
fn tune_fresh_macos_lan_punch_socket(socket: &tokio::net::UdpSocket) {
    let socket_ref = socket2::SockRef::from(socket);
    if let Err(error) = socket_ref.set_recv_buffer_size(tp_transport::UDP_SOCKET_RECV_BUF_BYTES) {
        tracing::warn!(
            error = %error,
            target = tp_transport::UDP_SOCKET_RECV_BUF_BYTES,
            "fresh LAN punch socket SO_RCVBUF setsockopt failed; using OS default"
        );
    }
    if let Err(error) = socket_ref.set_send_buffer_size(tp_transport::UDP_SOCKET_SEND_BUF_BYTES) {
        tracing::warn!(
            error = %error,
            target = tp_transport::UDP_SOCKET_SEND_BUF_BYTES,
            "fresh LAN punch socket SO_SNDBUF setsockopt failed; using OS default"
        );
    }
}

fn bind_std_p2p_socket_for_candidates_on_interfaces(
    candidates: &[std::net::SocketAddr],
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
) -> std::io::Result<std::net::UdpSocket> {
    match candidate_socket_set_family(candidates) {
        Some(family) => {
            bind_std_p2p_socket_for_family_on_interfaces(family, underlay_interface_indexes)
        }
        None if underlay_interface_indexes.is_some() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pinned P2P fallback socket requires exactly one candidate address family",
        )),
        None => tp_transport::quic::bind_tuned_udp("[::]:0".parse().unwrap())
            .or_else(|_| tp_transport::quic::bind_tuned_udp("0.0.0.0:0".parse().unwrap())),
    }
}

#[cfg(test)]
async fn bind_punch_socket(
    candidates: &[std::net::SocketAddr],
) -> std::io::Result<tokio::net::UdpSocket> {
    bind_punch_socket_on_interfaces(candidates, None).await
}

async fn bind_punch_socket_on_interfaces(
    candidates: &[std::net::SocketAddr],
    underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
) -> std::io::Result<tokio::net::UdpSocket> {
    let family = candidate_socket_set_family(candidates).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "p2p punch candidates must contain exactly one address family",
        )
    })?;
    let unspecified_addr = match family {
        P2pAddressFamily::Ipv6 => "[::]:0".parse().unwrap(),
        P2pAddressFamily::Ipv4 => "0.0.0.0:0".parse().unwrap(),
    };
    let interface_index = required_underlay_index_for_family(underlay_interface_indexes, family)?;
    let bind_addr = underlay_interface_indexes
        .map(|indexes| indexes.bind_addr_for(unspecified_addr))
        .transpose()?
        .unwrap_or(unspecified_addr);
    bind_tokio_tuned_udp_on_interface(bind_addr, interface_index)
}

fn bind_tokio_tuned_udp_on_interface(
    addr: std::net::SocketAddr,
    underlay_interface_index: Option<NonZeroU32>,
) -> std::io::Result<tokio::net::UdpSocket> {
    let std_sock = tp_transport::quic::bind_tuned_udp_on_interface(addr, underlay_interface_index)?;
    tokio::net::UdpSocket::from_std(std_sock)
}

fn is_public_p2p_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => is_public_p2p_ipv4(ip),
        std::net::IpAddr::V6(ip) => is_public_p2p_ipv6(ip),
    }
}

fn is_lan_p2p_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_private(),
        std::net::IpAddr::V6(ip) => is_unique_local_ipv6(ip),
    }
}

fn is_public_p2p_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    let shared_carrier_nat = a == 100 && (64..=127).contains(&b);
    let benchmark = a == 198 && (b == 18 || b == 19);
    let reserved = a >= 240;
    let this_network = a == 0;

    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || shared_carrier_nat
        || benchmark
        || reserved
        || this_network)
}

fn is_public_p2p_ipv6(ip: std::net::Ipv6Addr) -> bool {
    let segments = ip.segments();
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;

    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_unique_local_ipv6(ip)
        || is_unicast_link_local_ipv6(ip)
        || documentation)
}

fn is_unique_local_ipv6(ip: std::net::Ipv6Addr) -> bool {
    ip.segments()[0] & 0xfe00 == 0xfc00
}

fn is_unicast_link_local_ipv6(ip: std::net::Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

/// Pick the QUIC dial target for the initiator after the punch handshake.
///
/// Selection rule: family-aware match — accept the ProbeAck source
/// only if an announced candidate has the same IP family and IP. With
/// same-socket punching, the source port is the responder's reachable QUIC
/// endpoint, so preserve `probe_src.port()` instead of the announced port.
///
/// Returns:
///   - `None` if `candidates` is empty.
///   - `None` if no candidate matches both IP family and IP. This avoids
///     the previous silent fallback to `candidates[0]`, which could pick
///     a wrong-family entry (e.g. IPv4 probe → IPv6-only candidate) and
///     drive QUIC into an inevitable dial timeout.
///   - `Some(probe_src)` on a family-and-IP match.
///
/// Caller responsibility: treat `None` as a `NatFail`-class
/// attempt failure — bump the metric, send `P2pTeardown`, and abort the
/// punch task.
pub fn select_dial_target(
    probe_src: std::net::SocketAddr,
    candidates: &[std::net::SocketAddr],
) -> Option<std::net::SocketAddr> {
    candidates
        .iter()
        .any(|c| c.is_ipv4() == probe_src.is_ipv4() && c.ip() == probe_src.ip())
        .then_some(probe_src)
}

/// Per-session probe replay window. Returns whether the given `seq`
/// should be accepted (and answered) by the responder.
///
/// Rule: accept the first probe unconditionally; thereafter reject any
/// probe whose `seq` is more than 64 below `max_seen_seq` (replayed). On
/// accept, advance `max_seen_seq` if the new seq is higher.
///
/// This is required for replay-attack hardening: a passive observer
/// could otherwise capture a ProbeAck and replay the corresponding Probe
/// against a future session reusing seq numbers. Probe bursts are short
/// (≤150 seq per attempt) so wraparound isn't a real concern within the
/// 5s responder window; we use saturating arithmetic in the comparison
/// for defense-in-depth.
fn accept_probe_seq(seq: u32, max_seen_seq: &mut u32, have_seen_any: &mut bool) -> bool {
    if *have_seen_any && seq < max_seen_seq.saturating_sub(64) {
        return false;
    }
    if !*have_seen_any || seq > *max_seen_seq {
        *max_seen_seq = seq;
        *have_seen_any = true;
    }
    true
}

/// Install a P2P session into the bounded registry only if the
/// per-attempt cancellation token is not yet cancelled. Returns whether the
/// install happened. The acquire-then-check ordering means a teardown
/// cancellation arriving AFTER this check still races, but the manager's
/// `P2pTeardown` handler closes matching registry entries synchronously, so
/// the worst case is a transient session overwritten on the next handler tick;
/// no zombie persists.
fn try_install_p2p_session(
    multi: &MultiSession,
    session: Arc<tp_transport::session::Session>,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    multi.set_p2p(Some(session));
    true
}

async fn try_install_p2p_session_with_installer(
    installer: &P2pSessionInstaller,
    session_id: SessionId,
    session: tp_transport::session::Session,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<crate::p2p::installer::P2pInstalledSession>> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    installer
        .install_reserved(session_id, session)
        .await
        .map(Some)
}

fn rollback_cancelled_p2p_install(
    multi: &Arc<MultiSession>,
    session_id: SessionId,
    installed: Option<&crate::p2p::installer::P2pInstalledSession>,
    legacy_installed: bool,
) {
    if let Some(installed) = installed {
        installed.close_and_clear_if_current();
    } else if legacy_installed {
        crate::p2p::installer::close_current_p2p(multi);
    }
    if matches!(
        multi.p2p_state(),
        P2pState::Active {
            session_id: active_session_id,
            ..
        } if active_session_id == session_id
    ) {
        multi.set_state(P2pState::Idle);
    }
}

/// Sleep until the given absolute unix-millis instant. Returns immediately if
/// the target is already in the past. Free function (not a method) so the
/// `spawn_punch_responder` task closure doesn't have to capture `&self`.
async fn sleep_until_unix_ms(target_ms: i64) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if target_ms > now_ms {
        tokio::time::sleep(std::time::Duration::from_millis(
            (target_ms - now_ms) as u64,
        ))
        .await;
    }
}

fn unix_timestamp_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Exponential backoff for P2P retry cooldowns.
///
/// Doubles `base` per failure, capping at `cap`. `cap` is
/// configurable from `ClientP2pConfig.cooldown_max_secs`.
pub fn next_cooldown(
    failure_count: u32,
    base: std::time::Duration,
    cap: std::time::Duration,
) -> std::time::Duration {
    let factor = 1u64 << failure_count.min(5);
    let secs = base.as_secs().saturating_mul(factor);
    std::time::Duration::from_secs(secs.min(cap.as_secs()))
}

// Test fixtures in this file use public resolver addresses (1.1.1.1, 8.8.8.8,
// 9.9.9.9, 2606:4700:4700::1111, 2001:4860:4860::8888) rather than RFC 5737 /
// RFC 3849 documentation addresses on purpose: the code under test validates
// that an address is globally routable, and `is_documentation` rejects
// 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 and 2001:db8::/32. Fixtures
// that must be filtered out still use the documentation ranges, and elsewhere
// in the workspace those ranges are the right choice.
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn v2_peer_pair() -> (
        tp_core::provisioning::PeerProfileV2,
        tp_core::provisioning::PeerProfileV2,
    ) {
        use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};

        let mut owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        })
        .expect("Tunnel");
        (
            owner.add_peer(None, 1, None).expect("Peer A"),
            owner.add_peer(None, 1, None).expect("Peer B"),
        )
    }

    #[tokio::test]
    async fn v2_initiation_sends_an_end_to_end_verified_offer_for_the_stable_peer() {
        let (source, target) = v2_peer_pair();
        let source_peer_id = source.peer.peer_id.clone();
        let target_peer_id = target.peer.peer_id.clone();
        let issuer = source.tunnel_signing_public_key.clone();
        let (mut manager, mut outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "runtime-a-AbCd0001-0",
        );
        manager.set_v2_profile(Arc::new(source));
        manager.set_allow_lan_candidates(true);
        manager.set_mapping_probe_reflector_for_test(None);
        let listener_socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("listener socket");
        listener_socket
            .set_nonblocking(true)
            .expect("nonblocking listener");
        manager.set_listener_probe_socket_for_test(listener_socket);

        manager
            .try_initiate_for_local_slot_family(&target_peer_id, None, P2pAddressFamily::Ipv4, None)
            .await
            .expect("send V2 Offer");

        let BinaryMessage::P2pOfferV2 {
            source_peer_id: outer_source,
            target_peer_id: outer_target,
            signed_offer,
        } = outbound.recv().await.expect("Offer")
        else {
            panic!("expected V2 Offer");
        };
        let offer = tp_core::peer_link_crypto::P2pOfferV2::from_wire_bytes(&signed_offer)
            .expect("decode signed Offer");
        offer.verify(&issuer).expect("verify signed Offer");
        assert_eq!(outer_source, source_peer_id);
        assert_eq!(outer_target, target_peer_id);
        assert_eq!(offer.source_peer_id, outer_source);
        assert_eq!(offer.target_peer_id, outer_target);
    }

    #[tokio::test]
    async fn v2_offer_from_peer_absent_in_current_membership_is_dropped_before_sinks() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::peer_link_crypto::{P2pOfferV2, PeerLinkEphemeralSecretV2};

        let (source, target) = v2_peer_pair();
        let session_id = SessionId::from_bytes([0x31; 16]);
        let secret = PeerLinkEphemeralSecretV2::generate();
        let offer = P2pOfferV2::sign(
            &source,
            session_id,
            target.peer.peer_id.clone(),
            Vec::new(),
            CertFingerprint::from_bytes([0x32; 32]),
            &secret,
        )
        .expect("signed Offer");
        let wire = offer.to_wire_bytes().expect("wire Offer");
        let (mut acceptor, mut outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-b-AbCd0002-0",
        );
        acceptor.set_v2_profile(Arc::new(target));
        acceptor.set_v2_current_peer_authority_source(|_| false);
        let membership_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let membership_calls_for_sink = membership_calls.clone();
        acceptor.set_v2_membership_sink(move |_| {
            membership_calls_for_sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let key_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let key_calls_for_sink = key_calls.clone();
        acceptor.set_v2_peer_link_sink(move |_, _, _| {
            key_calls_for_sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        acceptor
            .handle_message(BinaryMessage::P2pOfferV2 {
                source_peer_id: offer.source_peer_id,
                target_peer_id: offer.target_peer_id,
                signed_offer: bytes::Bytes::from(wire),
            })
            .await;

        assert_eq!(
            membership_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(key_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!acceptor.peer_contexts.contains_key(&session_id));
        assert!(outbound.try_recv().is_err());
    }

    #[tokio::test]
    async fn v2_manager_rejects_legacy_offer_answer_and_punch_sync() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (source, _target) = v2_peer_pair();
        let (mut manager, mut outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "runtime-a-AbCd0001-0",
        );
        manager.set_v2_profile(Arc::new(source));
        let session_id = SessionId::from_bytes([0x41; 16]);
        let original_candidate = Candidate {
            ip: "127.0.0.1".into(),
            port: 7001,
            kind: CandidateKind::Host,
        };
        manager.peer_contexts.insert(
            session_id,
            PeerContext {
                candidates: vec![original_candidate.clone()],
                cert_fp: Some(CertFingerprint::from_bytes([0x42; 32])),
                peer_client_id: Some("legacy-remote-0".into()),
                session_role: Some(ClientRole::Initiator),
                ..PeerContext::default()
            },
        );
        manager
            .pending_answer_cancel
            .insert(session_id, CancellationToken::new());

        manager
            .handle_message(BinaryMessage::P2pAnswer {
                session_id,
                accepted_client_id: "legacy-remote-0".into(),
                ok: true,
                reason: String::new(),
                candidates: vec![Candidate {
                    ip: "127.0.0.1".into(),
                    port: 7002,
                    kind: CandidateKind::Host,
                }],
                dst_cert_fp: CertFingerprint::from_bytes([0x43; 32]),
            })
            .await;
        assert_eq!(
            manager
                .peer_contexts
                .get(&session_id)
                .expect("V2 context retained")
                .candidates,
            vec![original_candidate]
        );

        manager
            .handle_message(BinaryMessage::P2pPunchSync {
                session_id,
                t_start_ms: 0,
                burst_count: 1,
                port_offsets: vec![0],
            })
            .await;
        assert!(
            manager.pending_answer_cancel.contains_key(&session_id),
            "legacy PunchSync must not consume a V2 context timeout"
        );

        let offer_session_id = SessionId::from_bytes([0x44; 16]);
        manager
            .handle_message(BinaryMessage::P2pOffer {
                session_id: offer_session_id,
                src_client_id: "legacy-remote-0".into(),
                dst_client_id: "runtime-a-AbCd0001-0".into(),
                candidates: vec![],
                src_cert_fp: CertFingerprint::zero(),
                role: P2pRole::Initiator,
            })
            .await;
        assert!(!manager.peer_contexts.contains_key(&offer_session_id));
        assert!(outbound.try_recv().is_err());
    }

    #[tokio::test]
    async fn v2_answer_authenticates_membership_and_derives_matching_directional_keys() {
        type CapturedKeys = Arc<Mutex<Option<([u8; 32], [u8; 32])>>>;

        fn configure_v2_test_manager(
            manager: &mut P2pManager,
            profile: tp_core::provisioning::PeerProfileV2,
            captured: CapturedKeys,
        ) {
            manager.set_v2_profile(Arc::new(profile));
            manager.set_allow_lan_candidates(true);
            manager.set_mapping_probe_reflector_for_test(None);
            let listener_socket = std::net::UdpSocket::bind("0.0.0.0:0").expect("listener socket");
            listener_socket
                .set_nonblocking(true)
                .expect("nonblocking listener");
            manager.set_listener_probe_socket_for_test(listener_socket);
            manager.set_v2_peer_link_sink(move |_peer_id, _session_id, keys| {
                *captured.lock().expect("captured keys") =
                    Some((*keys.send_key(), *keys.receive_key()));
            });
        }

        let (source, target) = v2_peer_pair();
        let target_peer_id = target.peer.peer_id.clone();
        let (mut initiator, mut initiator_out, _initiator_multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "runtime-a-AbCd0001-0",
        );
        let (mut acceptor, mut acceptor_out, _acceptor_multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-b-AbCd0002-0",
        );
        let source_keys: CapturedKeys = Arc::new(Mutex::new(None));
        let target_keys: CapturedKeys = Arc::new(Mutex::new(None));
        configure_v2_test_manager(&mut initiator, source, source_keys.clone());
        configure_v2_test_manager(&mut acceptor, target, target_keys.clone());

        initiator
            .try_initiate_for_local_slot_family(&target_peer_id, None, P2pAddressFamily::Ipv4, None)
            .await
            .expect("send signed Offer");
        acceptor
            .handle_message(initiator_out.recv().await.expect("signed Offer"))
            .await;
        initiator
            .handle_message(acceptor_out.recv().await.expect("signed Answer"))
            .await;

        let (source_send, source_receive) = source_keys
            .lock()
            .expect("source keys")
            .expect("source derived keys");
        let (target_send, target_receive) = target_keys
            .lock()
            .expect("target keys")
            .expect("target derived keys");
        assert_eq!(source_send, target_receive);
        assert_eq!(source_receive, target_send);
    }

    #[tokio::test]
    async fn v2_empty_candidates_still_complete_signed_peerlink_for_relay_only() {
        type CapturedKeys = Arc<Mutex<Option<([u8; 32], [u8; 32])>>>;

        let (source, target) = v2_peer_pair();
        let target_peer_id = target.peer.peer_id.clone();
        let issuer = source.tunnel_signing_public_key.clone();
        let (mut initiator, mut initiator_out, _initiator_multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "runtime-a-AbCd0001-0",
        );
        let (mut acceptor, mut acceptor_out, _acceptor_multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-b-AbCd0002-0",
        );
        let source_keys: CapturedKeys = Arc::new(Mutex::new(None));
        let target_keys: CapturedKeys = Arc::new(Mutex::new(None));
        initiator.set_v2_profile(Arc::new(source));
        acceptor.set_v2_profile(Arc::new(target));
        initiator.set_mapping_probe_reflector_for_test(None);
        initiator.set_listener_observed_public_addr_for_test(Some(
            "198.51.100.10:42000"
                .parse()
                .expect("prior observed Direct endpoint"),
        ));
        let source_capture = source_keys.clone();
        initiator.set_v2_peer_link_sink(move |_peer_id, _session_id, keys| {
            *source_capture.lock().expect("source key capture") =
                Some((*keys.send_key(), *keys.receive_key()));
        });
        let target_capture = target_keys.clone();
        acceptor.set_v2_peer_link_sink(move |_peer_id, _session_id, keys| {
            *target_capture.lock().expect("target key capture") =
                Some((*keys.send_key(), *keys.receive_key()));
        });

        initiator
            .handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.1".into(),
                public_port: 0,
                server_time_ms: 1,
            })
            .await;
        assert!(initiator.observed_public_addr.is_none());
        assert!(initiator.listener_observed_public_addr.is_none());

        initiator
            .try_initiate(&target_peer_id)
            .await
            .expect("send candidate-free signed Offer");
        assert!(
            initiator.initiator_punch_sockets.is_empty(),
            "Direct-disabled Ack must not reserve a punch socket"
        );
        let offer_message = initiator_out.recv().await.expect("signed Offer");
        let BinaryMessage::P2pOfferV2 { signed_offer, .. } = &offer_message else {
            panic!("expected V2 Offer");
        };
        let offer = tp_core::peer_link_crypto::P2pOfferV2::from_wire_bytes(signed_offer)
            .expect("decode signed Offer");
        offer.verify(&issuer).expect("verify signed Offer");
        assert!(offer.candidates.is_empty());

        acceptor.handle_message(offer_message).await;
        let answer_message = acceptor_out.recv().await.expect("signed Answer");
        let BinaryMessage::P2pAnswerV2 { signed_answer, .. } = &answer_message else {
            panic!("expected V2 Answer");
        };
        let answer = tp_core::peer_link_crypto::P2pAnswerV2::from_wire_bytes(signed_answer)
            .expect("decode signed Answer");
        answer
            .verify_for_offer(&offer, &issuer)
            .expect("verify signed Answer");
        assert!(answer.accepted, "Relay-only PeerLink must be accepted");
        assert!(answer.candidates.is_empty());

        initiator.handle_message(answer_message).await;
        let (source_send, source_receive) = source_keys
            .lock()
            .expect("source keys")
            .expect("source derived Relay-only keys");
        let (target_send, target_receive) = target_keys
            .lock()
            .expect("target keys")
            .expect("target derived Relay-only keys");
        assert_eq!(source_send, target_receive);
        assert_eq!(source_receive, target_send);
    }

    #[tokio::test]
    async fn v2_relay_only_offer_is_not_blocked_by_an_existing_direct_relation() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::peer_link_crypto::{P2pOfferV2, PeerLinkEphemeralSecretV2};

        let (source, target) = v2_peer_pair();
        let issuer = source.tunnel_signing_public_key.clone();
        let (mut acceptor, mut outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-b-AbCd0002-0",
        );
        acceptor.set_v2_profile(Arc::new(target.clone()));
        acceptor.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_id(target.peer.peer_id.clone())
                    .expect("local stable Peer"),
                1,
            )
            .expect("PeerLink manager"),
        );
        let relation =
            MeshRelationKey::from_stable_peers(&source.peer.peer_id, &target.peer.peer_id, 0)
                .expect("canonical relation");
        acceptor.peer_contexts.insert(
            SessionId::from_bytes([0x51; 16]),
            PeerContext {
                mesh_relation_key: Some(relation),
                ..PeerContext::default()
            },
        );

        let offer_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = P2pOfferV2::sign(
            &source,
            SessionId::from_bytes([0x52; 16]),
            target.peer.peer_id.clone(),
            Vec::new(),
            CertFingerprint::from_bytes([0x53; 32]),
            &offer_secret,
        )
        .expect("signed Relay-only Offer");
        acceptor
            .handle_message(BinaryMessage::P2pOfferV2 {
                source_peer_id: source.peer.peer_id,
                target_peer_id: target.peer.peer_id,
                signed_offer: bytes::Bytes::from(offer.to_wire_bytes().expect("Offer wire")),
            })
            .await;

        let BinaryMessage::P2pAnswerV2 { signed_answer, .. } =
            outbound.recv().await.expect("signed Answer")
        else {
            panic!("expected V2 Answer");
        };
        let answer = tp_core::peer_link_crypto::P2pAnswerV2::from_wire_bytes(&signed_answer)
            .expect("decode signed Answer");
        answer
            .verify_for_offer(&offer, &issuer)
            .expect("verify signed Answer");
        assert!(
            answer.accepted,
            "Relay-only key refresh must not reserve or contend for the Direct relation"
        );
    }

    #[tokio::test]
    async fn v2_relay_only_peerlink_cleanup_does_not_immediately_requeue_direct() {
        use crate::peer_link_manager::{MembershipSnapshot, PeerDescriptor, PeerLinkManager};

        let (peer_a, peer_b) = v2_peer_pair();
        let (source, target) = if peer_a.peer.peer_id < peer_b.peer.peer_id {
            (peer_a, peer_b)
        } else {
            (peer_b, peer_a)
        };
        let source_runtime = "runtime-a-AbCd0001-0";
        let target_peer_id = target.peer.peer_id.clone();
        let (mut initiator, mut initiator_out, _initiator_multi) =
            make_test_manager_with_role(crate::p2p::session::ClientRole::Initiator, source_runtime);
        let (mut acceptor, mut acceptor_out, _acceptor_multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-b-AbCd0002-0",
        );
        initiator.set_v2_profile(Arc::new(source.clone()));
        acceptor.set_v2_profile(Arc::new(target));
        initiator.set_mapping_probe_reflector_for_test(None);

        let local_peer = PeerDescriptor::from_stable_peer_and_replica_ids(
            source.peer.peer_id,
            vec![source_runtime.into()],
        )
        .expect("local stable Peer");
        let remote_peer = PeerDescriptor::from_stable_peer_id(target_peer_id.clone())
            .expect("remote stable Peer");
        let mut link_manager = PeerLinkManager::new(local_peer, 1).expect("PeerLink manager");
        let PeerLinkCommand::EnsureLane(lane) = link_manager
            .apply_snapshot(&MembershipSnapshot::new(vec![remote_peer]))
            .pop()
            .expect("one desired PeerLink");
        assert_eq!(lane.local_role(), RelationRole::Initiator);
        let relation =
            P2pManager::mesh_relation_key_for_lane(&lane).expect("canonical stable Peer relation");
        initiator.set_peer_link_manager(link_manager);
        initiator.mesh_relation_lanes.insert(relation.clone(), lane);

        initiator
            .try_initiate_for_local_slot_family_with_relation(
                &target_peer_id,
                Some(source_runtime),
                P2pAddressFamily::Ipv4,
                None,
                Some(relation),
            )
            .await
            .expect("send Relay-only Offer");
        acceptor
            .handle_message(initiator_out.recv().await.expect("Offer"))
            .await;
        initiator
            .handle_message(acceptor_out.recv().await.expect("Answer"))
            .await;

        assert!(initiator.pending_v2_offers.is_empty());
        assert!(initiator.peer_contexts.is_empty());
        assert!(
            initiator.pending_peer_link_commands.is_empty(),
            "accepted Relay-only PeerLink must not spin another Direct handshake"
        );
    }

    #[tokio::test]
    async fn v2_crossed_relay_only_offers_converge_on_canonical_generation_without_rekey_loop() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        type CapturedKeys = Arc<Mutex<Vec<(String, SessionId, [u8; 32], [u8; 32])>>>;

        let (peer_a, peer_b) = v2_peer_pair();
        let (canonical_profile, reverse_profile) = if peer_a.peer.peer_id < peer_b.peer.peer_id {
            (peer_a, peer_b)
        } else {
            (peer_b, peer_a)
        };
        let canonical_peer_id = canonical_profile.peer.peer_id.clone();
        let reverse_peer_id = reverse_profile.peer.peer_id.clone();
        let canonical_runtime = "runtime-canonical-AbCd0001-0";
        let reverse_runtime = "runtime-reverse-AbCd0002-0";
        let (mut canonical, mut canonical_out, _canonical_multi) =
            make_test_manager_with_role(ClientRole::Initiator, canonical_runtime);
        let (mut reverse, mut reverse_out, _reverse_multi) =
            make_test_manager_with_role(ClientRole::Acceptor, reverse_runtime);
        canonical.set_v2_profile(Arc::new(canonical_profile));
        reverse.set_v2_profile(Arc::new(reverse_profile));
        canonical.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_and_replica_ids(
                    canonical_peer_id.clone(),
                    vec![canonical_runtime.into()],
                )
                .expect("canonical local stable Peer"),
                1,
            )
            .expect("canonical PeerLink manager"),
        );
        reverse.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_and_replica_ids(
                    reverse_peer_id.clone(),
                    vec![reverse_runtime.into()],
                )
                .expect("reverse local stable Peer"),
                1,
            )
            .expect("reverse PeerLink manager"),
        );
        let canonical_keys: CapturedKeys = Arc::new(Mutex::new(Vec::new()));
        let reverse_keys: CapturedKeys = Arc::new(Mutex::new(Vec::new()));
        let canonical_capture = canonical_keys.clone();
        canonical.set_v2_peer_link_sink(move |peer_id, session_id, keys| {
            canonical_capture
                .lock()
                .expect("canonical key capture")
                .push((peer_id, session_id, *keys.send_key(), *keys.receive_key()));
        });
        let reverse_capture = reverse_keys.clone();
        reverse.set_v2_peer_link_sink(move |peer_id, session_id, keys| {
            reverse_capture.lock().expect("reverse key capture").push((
                peer_id,
                session_id,
                *keys.send_key(),
                *keys.receive_key(),
            ));
        });

        canonical
            .handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: reverse_peer_id.clone(),
            })
            .await;
        canonical
            .handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 0,
                server_time_ms: 1,
            })
            .await;
        reverse
            .handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: canonical_peer_id.clone(),
            })
            .await;
        reverse
            .handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 0,
                server_time_ms: 1,
            })
            .await;

        assert_eq!(
            canonical.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let canonical_offer_message = canonical_out.recv().await.expect("canonical Offer");
        let BinaryMessage::P2pOfferV2 {
            signed_offer: canonical_offer_wire,
            ..
        } = &canonical_offer_message
        else {
            panic!("expected canonical V2 Offer");
        };
        let canonical_offer =
            tp_core::peer_link_crypto::P2pOfferV2::from_wire_bytes(canonical_offer_wire)
                .expect("decode canonical Offer");

        assert_eq!(
            reverse.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );
        tokio::time::sleep(V2_ACCEPTOR_RECOVERY_DELAY + Duration::from_millis(25)).await;
        assert_eq!(
            reverse.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let reverse_offer_message = reverse_out.recv().await.expect("reverse recovery Offer");

        // Both Offers crossed in flight. The canonical side sees the reverse
        // Offer first; the reverse side then sees the canonical Offer.
        canonical.handle_message(reverse_offer_message).await;
        let reverse_rejection = canonical_out
            .recv()
            .await
            .expect("busy rejection for reverse Offer");
        let BinaryMessage::P2pAnswerV2 {
            signed_answer: reverse_rejection_wire,
            ..
        } = &reverse_rejection
        else {
            panic!("expected reverse rejection");
        };
        let reverse_rejection_body =
            tp_core::peer_link_crypto::P2pAnswerV2::from_wire_bytes(reverse_rejection_wire)
                .expect("decode reverse rejection");
        assert!(!reverse_rejection_body.accepted);
        assert_eq!(reverse_rejection_body.reason_code, V2_REJECT_RELATION_BUSY);
        reverse.handle_message(canonical_offer_message).await;
        let canonical_answer = reverse_out.recv().await.expect("accepted canonical Answer");
        let BinaryMessage::P2pAnswerV2 {
            signed_answer: canonical_answer_wire,
            ..
        } = &canonical_answer
        else {
            panic!("expected canonical Answer");
        };
        let canonical_answer_body =
            tp_core::peer_link_crypto::P2pAnswerV2::from_wire_bytes(canonical_answer_wire)
                .expect("decode canonical Answer");
        assert!(canonical_answer_body.accepted);

        // Deliver Answers out of order: the winner completes before the late
        // loser rejection. A late loser Answer must be unknown and cannot
        // overwrite the canonical generation.
        canonical.handle_message(canonical_answer).await;
        reverse.handle_message(reverse_rejection).await;

        let canonical_installs = canonical_keys.lock().expect("canonical installs").clone();
        let reverse_installs = reverse_keys.lock().expect("reverse installs").clone();
        assert_eq!(canonical_installs.len(), 1);
        assert_eq!(reverse_installs.len(), 1);
        let (canonical_remote, canonical_session, canonical_send, canonical_receive) =
            &canonical_installs[0];
        let (reverse_remote, reverse_session, reverse_send, reverse_receive) = &reverse_installs[0];
        assert_eq!(canonical_remote, &reverse_peer_id);
        assert_eq!(reverse_remote, &canonical_peer_id);
        assert_eq!(*canonical_session, canonical_offer.session_id);
        assert_eq!(*reverse_session, canonical_offer.session_id);
        assert_eq!(canonical_send, reverse_receive);
        assert_eq!(canonical_receive, reverse_send);
        assert_eq!(
            canonical.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        assert_eq!(
            reverse.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        assert!(canonical_out.try_recv().is_err());
        assert!(reverse_out.try_recv().is_err());
    }

    #[tokio::test]
    async fn v2_fresh_canonical_acceptor_recovers_a_missing_peerlink_generation() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        type CapturedKeys = Arc<Mutex<Option<(String, SessionId, [u8; 32], [u8; 32])>>>;

        let (peer_a, peer_b) = v2_peer_pair();
        let (canonical_initiator, fresh_acceptor) = if peer_a.peer.peer_id < peer_b.peer.peer_id {
            (peer_a, peer_b)
        } else {
            (peer_b, peer_a)
        };
        let canonical_peer_id = canonical_initiator.peer.peer_id.clone();
        let fresh_peer_id = fresh_acceptor.peer.peer_id.clone();
        let local_runtime_id = "runtime-fresh-AbCd0002-0";
        let survivor_runtime_id = "runtime-survivor-AbCd0001-0";
        let (mut fresh, mut outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            local_runtime_id,
        );
        let (mut survivor, mut survivor_outbound, _survivor_multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            survivor_runtime_id,
        );
        fresh.set_v2_profile(Arc::new(fresh_acceptor));
        survivor.set_v2_profile(Arc::new(canonical_initiator));
        fresh.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_and_replica_ids(
                    fresh_peer_id.clone(),
                    vec![local_runtime_id.into()],
                )
                .expect("fresh local stable Peer"),
                1,
            )
            .expect("PeerLink manager"),
        );
        survivor.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_and_replica_ids(
                    canonical_peer_id.clone(),
                    vec![survivor_runtime_id.into()],
                )
                .expect("surviving local stable Peer"),
                1,
            )
            .expect("surviving PeerLink manager"),
        );
        let fresh_keys: CapturedKeys = Arc::new(Mutex::new(None));
        let survivor_keys: CapturedKeys = Arc::new(Mutex::new(None));
        let fresh_capture = fresh_keys.clone();
        fresh.set_v2_peer_link_sink(move |peer_id, session_id, keys| {
            *fresh_capture.lock().expect("fresh key capture") =
                Some((peer_id, session_id, *keys.send_key(), *keys.receive_key()));
        });
        let survivor_capture = survivor_keys.clone();
        survivor.set_v2_peer_link_sink(move |peer_id, session_id, keys| {
            *survivor_capture.lock().expect("survivor key capture") =
                Some((peer_id, session_id, *keys.send_key(), *keys.receive_key()));
        });

        // The surviving lower-ID Peer may still hold the previous process's
        // Relay key. The restarted higher-ID Peer has no key, but its initial
        // membership command is canonically the Acceptor side.
        fresh
            .handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: canonical_peer_id.clone(),
            })
            .await;
        fresh
            .handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 0,
                server_time_ms: 1,
            })
            .await;

        assert_eq!(
            fresh.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL),
            "the canonical acceptor must retain one recovery attempt while giving the initiator priority"
        );
        assert!(
            outbound.try_recv().is_err(),
            "the acceptor must not race the canonical initiator immediately"
        );

        tokio::time::sleep(V2_ACCEPTOR_RECOVERY_DELAY + Duration::from_millis(25)).await;
        assert_eq!(
            fresh.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let recovery_offer = outbound.recv().await.expect("reverse recovery Offer");
        let BinaryMessage::P2pOfferV2 {
            source_peer_id,
            target_peer_id,
            signed_offer,
        } = &recovery_offer
        else {
            panic!("expected V2 recovery Offer");
        };
        assert_eq!(source_peer_id, &fresh_peer_id);
        assert_eq!(target_peer_id, &canonical_peer_id);
        let offer = tp_core::peer_link_crypto::P2pOfferV2::from_wire_bytes(signed_offer)
            .expect("decode recovery Offer");
        assert!(
            offer.candidates.is_empty(),
            "forced Relay remains Relay-only"
        );
        survivor.handle_message(recovery_offer).await;
        fresh
            .handle_message(
                survivor_outbound
                    .recv()
                    .await
                    .expect("signed recovery Answer"),
            )
            .await;

        let (fresh_remote, fresh_session, fresh_send, fresh_receive) = fresh_keys
            .lock()
            .expect("fresh keys")
            .clone()
            .expect("fresh side derived keys");
        let (survivor_remote, survivor_session, survivor_send, survivor_receive) = survivor_keys
            .lock()
            .expect("survivor keys")
            .clone()
            .expect("surviving side derived keys");
        assert_eq!(fresh_remote, canonical_peer_id);
        assert_eq!(survivor_remote, fresh_peer_id);
        assert_eq!(fresh_session, offer.session_id);
        assert_eq!(survivor_session, offer.session_id);
        assert_eq!(fresh_send, survivor_receive);
        assert_eq!(fresh_receive, survivor_send);

        let relation = MeshRelationKey::from_stable_peers(&canonical_peer_id, &fresh_peer_id, 0)
            .expect("stable recovery relation");
        assert!(
            !fresh
                .v2_acceptor_recovery_not_before
                .contains_key(&relation),
            "a completed generation must consume its recovery deadline"
        );
        let lane = fresh
            .mesh_relation_lanes
            .get(&relation)
            .cloned()
            .expect("remembered recovery lane");
        fresh.enqueue_peer_link_command(PeerLinkCommand::EnsureLane(lane));
        assert_eq!(
            fresh.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL),
            "a later lane loss must grant the canonical initiator a fresh recovery window"
        );
        assert!(
            outbound.try_recv().is_err(),
            "a stale expired deadline must not trigger an immediate reverse Offer"
        );
    }

    #[tokio::test]
    async fn manager_uses_mapping_probe_default_timeout() {
        let (_out_tx, out_rx) = tokio::sync::mpsc::channel(1);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(1);
        let mgr = P2pManager::new(
            make_test_multi_arc(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            _out_tx,
            4433,
        );

        drop(out_rx);
        assert_eq!(
            mgr.mapping_probe_timeout,
            crate::p2p::mapping_probe::DEFAULT_MAPPING_PROBE_TIMEOUT
        );
    }

    #[tokio::test]
    async fn initiator_socket_reservation_fails_closed_on_invalid_underlay_interface() {
        let (mut mgr, mut out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "peer-a-AbCd0001-0",
        );
        let invalid = std::num::NonZeroU32::new(u32::MAX).unwrap();
        mgr.set_underlay_interface_indexes(
            Some(invalid),
            Some(invalid),
            Some("192.168.240.44".parse().unwrap()),
            ["192.168.240.44".parse().unwrap()],
        );

        assert_eq!(
            mgr.try_initiate("peer-b-AbCd0002-0").await,
            Err("p2p socket bind failed")
        );
        assert!(out_rx.try_recv().is_err(), "no Offer may escape unpinned");
    }

    #[tokio::test]
    async fn acceptor_socket_reservation_fails_closed_on_invalid_underlay_interface() {
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-b-AbCd0002-0",
        );
        mgr.set_mapping_probe_reflector_for_test(None);
        let invalid = std::num::NonZeroU32::new(u32::MAX).unwrap();
        mgr.set_underlay_interface_indexes(
            Some(invalid),
            Some(invalid),
            Some("192.168.1.55".parse().unwrap()),
            ["192.168.1.55".parse().unwrap()],
        );

        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: SessionId::from_bytes([0x71; 16]),
            src_client_id: "peer-a-AbCd0001-0".into(),
            dst_client_id: "peer-b-AbCd0002-0".into(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 41000,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([0x72; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        match out_rx.recv().await.expect("fail-closed Answer") {
            BinaryMessage::P2pAnswer { ok, reason, .. } => {
                assert!(!ok);
                assert_eq!(reason, "punch socket bind failed");
            }
            other => panic!("expected P2pAnswer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acceptor_does_not_borrow_the_ipv4_index_for_an_ipv6_offer() {
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-b-AbCd0002-0",
        );
        mgr.set_mapping_probe_reflector_for_test(None);
        mgr.set_underlay_interface_indexes(
            std::num::NonZeroU32::new(7),
            None,
            Some("192.168.1.55".parse().unwrap()),
            ["192.168.1.55".parse().unwrap()],
        );

        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: SessionId::from_bytes([0x76; 16]),
            src_client_id: "peer-a-AbCd0001-0".into(),
            dst_client_id: "peer-b-AbCd0002-0".into(),
            candidates: vec![Candidate {
                ip: "2606:4700:4700::1111".into(),
                port: 41000,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([0x77; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        match out_rx.recv().await.expect("fail-closed Answer") {
            BinaryMessage::P2pAnswer { ok, reason, .. } => {
                assert!(!ok);
                assert_eq!(reason, "punch socket bind failed");
            }
            other => panic!("expected P2pAnswer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn responder_fallback_socket_keeps_the_requested_underlay_interface() {
        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-b-AbCd0002-0",
        );
        let invalid = std::num::NonZeroU32::new(u32::MAX).unwrap();
        mgr.set_underlay_interface_indexes(
            Some(invalid),
            Some(invalid),
            Some("192.168.1.55".parse().unwrap()),
            ["192.168.1.55".parse().unwrap()],
        );
        let session_id = SessionId::from_bytes([0x73; 16]);

        mgr.spawn_punch_responder(session_id, 0, None, vec!["8.8.8.8:41000".parse().unwrap()]);

        let event = tokio::time::timeout(Duration::from_millis(250), mgr.internal_rx.recv())
            .await
            .expect("an invalid pinned fallback bind must fail before the probe window")
            .expect("cleanup event");
        assert!(matches!(
            event,
            P2pInternalEvent::CleanupSessionAttempt {
                session_id: cleaned
            } if cleaned == session_id
        ));
    }

    #[tokio::test]
    async fn initiator_fallback_socket_keeps_the_requested_underlay_interface() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "peer-a-AbCd0001-0",
        );
        let invalid = std::num::NonZeroU32::new(u32::MAX).unwrap();
        mgr.set_underlay_interface_indexes(
            Some(invalid),
            Some(invalid),
            Some("192.168.240.44".parse().unwrap()),
            ["192.168.240.44".parse().unwrap()],
        );
        let bundle = crate::p2p::cert::generate_self_signed_cert("peer-a-AbCd0001-0")
            .expect("test certificate");
        mgr.set_tls_identity(&bundle);
        let session_id = SessionId::from_bytes([0x74; 16]);
        mgr.peer_contexts.insert(
            session_id,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "8.8.8.8".into(),
                    port: 41000,
                    kind: CandidateKind::ServerReflexive,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([0x75; 32])),
                peer_client_id: Some("peer-b-AbCd0002-0".into()),
                local_client_id: Some("peer-a-AbCd0001-0".into()),
                family: Some(P2pAddressFamily::Ipv4),
                session_role: Some(ClientRole::Initiator),
                ..PeerContext::default()
            },
        );

        mgr.spawn_punch_and_handshake(session_id, 0, 1, vec![0], false);

        let event = tokio::time::timeout(Duration::from_millis(250), mgr.internal_rx.recv())
            .await
            .expect("an invalid pinned fallback bind must fail before punch timeout")
            .expect("failure event");
        assert!(matches!(
            event,
            P2pInternalEvent::InitiatorAttemptFailed {
                session_id: failed
            } if failed == session_id
        ));
    }

    #[tokio::test]
    async fn mesh_membership_hints_commit_only_at_announce_ack() {
        use crate::peer_link_manager::{
            PeerDescriptor, PeerLinkCommand, PeerLinkManager, RelationRole,
        };

        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-a-AbCd0001-0",
        );
        let local_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-a-AbCd0001-0".into(),
            "peer-a-AbCd0001-1".into(),
        ])
        .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );

        for peer_client_id in [
            "peer-b-AbCd0002-1",
            "peer-b-AbCd0002-0",
            "peer-b-AbCd0002-1",
        ] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        assert!(
            mgr.drain_peer_link_commands().is_empty(),
            "Hints are an incomplete cycle until its trailing Ack"
        );

        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        let lanes = mgr
            .drain_peer_link_commands()
            .into_iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) => (
                    lane.index(),
                    lane.local_replica_id().to_string(),
                    lane.remote_replica_id().to_string(),
                    lane.local_role(),
                ),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lanes,
            vec![
                (
                    0,
                    "peer-a-AbCd0001-0".into(),
                    "peer-b-AbCd0002-0".into(),
                    RelationRole::Initiator,
                ),
                (
                    1,
                    "peer-a-AbCd0001-1".into(),
                    "peer-b-AbCd0002-1".into(),
                    RelationRole::Initiator,
                ),
            ]
        );
    }

    #[tokio::test]
    async fn mesh_membership_sink_observes_valid_exact_replicas_only_after_ack() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-a-AbCd0001-0",
        );
        let local_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-a-AbCd0001-0".into(),
            "peer-a-AbCd0001-1".into(),
        ])
        .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );
        let committed = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let committed_for_sink = committed.clone();
        mgr.set_membership_commit_sink(move |replica_ids| {
            committed_for_sink
                .lock()
                .expect("membership sink lock")
                .push(replica_ids.to_vec());
        });

        for peer_client_id in [
            "peer-b-AbCd0002-1",
            "peer-b-AbCd0002-0",
            "peer-b-AbCd0002-1",
        ] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        assert!(
            committed.lock().expect("membership sink lock").is_empty(),
            "uncommitted hints must never become routable"
        );

        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert_eq!(
            *committed.lock().expect("membership sink lock"),
            vec![vec![
                "peer-b-AbCd0002-0".to_string(),
                "peer-b-AbCd0002-1".to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn mesh_membership_sink_excludes_invalid_replica_ids() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-a-AbCd0001-0",
        );
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-a-AbCd0001-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        let committed = Arc::new(Mutex::new(Vec::<String>::new()));
        let committed_for_sink = committed.clone();
        mgr.set_membership_commit_sink(move |replica_ids| {
            committed_for_sink
                .lock()
                .expect("membership sink lock")
                .extend_from_slice(replica_ids);
        });

        for peer_client_id in ["not-a-stable-replica", "peer-b-AbCd0002-0"] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert_eq!(
            *committed.lock().expect("membership sink lock"),
            vec!["peer-b-AbCd0002-0".to_string()]
        );
    }

    #[tokio::test]
    async fn membership_sink_is_inert_without_mesh_manager() {
        let (mut mgr, _out_rx, _multi) =
            make_test_manager_with_role(crate::p2p::session::ClientRole::Acceptor, "legacy-client");
        let committed = Arc::new(Mutex::new(Vec::<String>::new()));
        let committed_for_sink = committed.clone();
        mgr.set_membership_commit_sink(move |replica_ids| {
            committed_for_sink
                .lock()
                .expect("membership sink lock")
                .extend_from_slice(replica_ids);
        });

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "peer-b-AbCd0002-0".into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert!(
            committed.lock().expect("membership sink lock").is_empty(),
            "legacy Hint/Ack handling must not acquire mesh routing side effects"
        );
    }

    #[tokio::test]
    async fn mesh_membership_is_independent_of_legacy_role_and_emits_r_lanes_per_peer() {
        use crate::peer_link_manager::{
            PeerDescriptor, PeerLinkCommand, PeerLinkManager, RelationRole,
        };

        let mut role_views = Vec::new();
        for role in [
            crate::p2p::session::ClientRole::Initiator,
            crate::p2p::session::ClientRole::Acceptor,
        ] {
            let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(role, "peer-a-AbCd0001-0");
            let local_peer = PeerDescriptor::from_replica_ids(
                (0..3)
                    .map(|index| format!("peer-a-AbCd0001-{index}"))
                    .collect(),
            )
            .expect("valid local Peer");
            mgr.set_peer_link_manager(
                PeerLinkManager::new(local_peer, 3).expect("non-zero Replica count"),
            );

            for peer_client_id in [
                "peer-c-AbCd0003-2",
                "peer-b-AbCd0002-1",
                "peer-c-AbCd0003-0",
                "peer-b-AbCd0002-0",
                "peer-c-AbCd0003-1",
                "peer-b-AbCd0002-2",
            ] {
                mgr.handle_message(BinaryMessage::P2pPeerHint {
                    peer_client_id: peer_client_id.into(),
                })
                .await;
            }
            mgr.handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 4433,
                server_time_ms: 1,
            })
            .await;

            role_views.push(
                mgr.drain_peer_link_commands()
                    .into_iter()
                    .map(|command| match command {
                        PeerLinkCommand::EnsureLane(lane) => (
                            lane.index(),
                            lane.remote_replica_id().to_string(),
                            lane.local_role(),
                        ),
                    })
                    .collect::<Vec<_>>(),
            );
        }

        assert_eq!(role_views[0], role_views[1]);
        assert_eq!(role_views[0].len(), 2 * 3, "two remote Peers times R=3");
        assert!(role_views[0]
            .iter()
            .all(|(_, _, role)| *role == RelationRole::Initiator));
    }

    #[tokio::test]
    async fn mesh_membership_replay_is_idempotent_and_empty_cycle_is_soft_absence() {
        use crate::peer_link_manager::{MembershipState, PeerDescriptor, PeerLinkManager};

        let (mut mgr, mut out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "peer-a-AbCd0001-0",
        );
        let local_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-a-AbCd0001-0".into(),
            "peer-a-AbCd0001-1".into(),
        ])
        .expect("valid local Peer");
        let remote_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-b-AbCd0002-0".into(),
            "peer-b-AbCd0002-1".into(),
        ])
        .expect("valid remote Peer");
        let remote_peer_id = remote_peer.peer_id().clone();
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );

        for cycle in 0..2 {
            for peer_client_id in ["peer-b-AbCd0002-0", "peer-b-AbCd0002-1"] {
                mgr.handle_message(BinaryMessage::P2pPeerHint {
                    peer_client_id: peer_client_id.into(),
                })
                .await;
            }
            mgr.handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 4433,
                server_time_ms: cycle,
            })
            .await;
            let work = mgr.drain_peer_link_commands();
            if cycle == 0 {
                assert_eq!(work.len(), 2);
            } else {
                assert!(work.is_empty(), "replayed cycle must not duplicate work");
            }
        }

        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 2,
        })
        .await;
        assert_eq!(
            mgr.peer_link_membership(&remote_peer_id),
            Some(MembershipState::SuspectMissing)
        );
        assert!(mgr.drain_peer_link_commands().is_empty());
        assert!(
            out_rx.try_recv().is_err(),
            "soft absence must not emit teardown or other signaling"
        );

        for peer_client_id in ["peer-b-AbCd0002-1", "peer-b-AbCd0002-0"] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 3,
        })
        .await;
        assert_eq!(
            mgr.peer_link_membership(&remote_peer_id),
            Some(MembershipState::Present)
        );
        assert!(
            mgr.drain_peer_link_commands().is_empty(),
            "a softly absent Peer retains its existing PeerLink"
        );
    }

    #[tokio::test]
    async fn v2_membership_cycle_sink_receives_exact_stable_peers_and_valid_empty_cycle() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (source, target) = v2_peer_pair();
        let target_peer_id = target.peer.peer_id.clone();
        let (mut manager, _outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-a-AbCd0001-0",
        );
        manager.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_id(source.peer.peer_id.clone())
                    .expect("source stable Peer"),
                1,
            )
            .expect("one logical V2 PeerLink"),
        );
        manager.set_v2_profile(Arc::new(source));
        let cycles = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let captured = Arc::clone(&cycles);
        manager.set_v2_membership_cycle_sink(move |peer_ids| {
            captured
                .lock()
                .expect("cycle sink lock")
                .push(peer_ids.to_vec());
            true
        });

        manager
            .handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: target_peer_id.clone(),
            })
            .await;
        assert!(
            manager
                .handle_message(BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 1,
                })
                .await
        );
        manager
            .handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 4433,
                server_time_ms: 2,
            })
            .await;

        assert_eq!(
            *cycles.lock().expect("cycle sink lock"),
            vec![vec![target_peer_id], Vec::<String>::new()]
        );
    }

    #[tokio::test]
    async fn rejected_v2_membership_ack_cannot_mutate_manager_state() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (source, target) = v2_peer_pair();
        let target_peer_id = target.peer.peer_id.clone();
        let target_id =
            PeerDescriptor::from_stable_peer_id(target_peer_id.clone()).expect("target Peer");
        let (mut manager, _outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "runtime-a-AbCd0001-0",
        );
        manager.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_stable_peer_id(source.peer.peer_id.clone())
                    .expect("source Peer"),
                1,
            )
            .expect("one logical PeerLink"),
        );
        manager.set_v2_profile(Arc::new(source));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_sink = calls.clone();
        manager.set_v2_membership_cycle_sink(move |_| {
            calls_for_sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        });

        manager
            .handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: target_peer_id,
            })
            .await;
        assert!(
            manager
                .handle_message(BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 1,
                })
                .await
        );
        assert!(manager.gateway_direct_lane_enabled);
        assert_eq!(
            manager.observed_public_addr,
            Some("203.0.113.10:4433".parse().expect("endpoint"))
        );
        manager.drain_peer_link_commands();

        assert!(
            !manager
                .handle_message(BinaryMessage::P2pAnnounceAck {
                    public_ip: "198.51.100.20".into(),
                    public_port: 0,
                    server_time_ms: 2,
                })
                .await
        );

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            manager.gateway_direct_lane_enabled,
            "rejected port-zero Ack must not disable Direct"
        );
        assert_eq!(
            manager.observed_public_addr,
            Some("203.0.113.10:4433".parse().expect("endpoint")),
            "rejected Ack must not replace the observed endpoint"
        );
        assert_eq!(
            manager.peer_link_membership(target_id.peer_id()),
            Some(MembershipState::Present),
            "rejected empty cycle must not mark the current Peer absent"
        );
        assert!(manager.drain_peer_link_commands().is_empty());
    }

    #[tokio::test]
    async fn rejected_retirement_keeps_absence_evidence_and_retries_next_cycle() {
        use crate::peer_link_manager::{PeerConnectivity, PeerDescriptor, PeerLinkManager};

        let (mut manager, _outbound, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "mesh-LocalA01-0",
        );
        manager.set_peer_link_manager(
            PeerLinkManager::new(
                PeerDescriptor::from_replica_ids(vec!["mesh-LocalA01-0".into()])
                    .expect("local Peer"),
                1,
            )
            .expect("one Replica"),
        );
        manager.set_peer_connectivity_source(|_| PeerConnectivity::unavailable());
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_sink = attempts.clone();
        manager.set_retired_peer_sink(move |_| {
            attempts_for_sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
        });
        let remote =
            PeerDescriptor::from_replica_ids(vec!["mesh-RemoteB1-0".into()]).expect("remote Peer");
        let remote_id = remote.peer_id().clone();
        let started = std::time::Instant::now();

        manager.buffer_membership_replica("mesh-RemoteB1-0".into());
        manager.commit_membership_cycle_at(started);
        manager.commit_membership_cycle_at(started + Duration::from_secs(1));
        manager.commit_membership_cycle_at(started + Duration::from_secs(121));
        assert_eq!(
            manager.peer_link_membership(&remote_id),
            Some(MembershipState::Retired),
            "rejected commit must retain the provisional retirement"
        );

        manager.commit_membership_cycle_at(started + Duration::from_secs(122));
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "original first-absent time must allow immediate next-cycle retry"
        );
        assert!(manager.peer_link_membership(&remote_id).is_none());
    }

    #[tokio::test]
    async fn membership_retirement_notifies_upper_route_owner_after_grace() {
        use crate::peer_link_manager::{PeerConnectivity, PeerDescriptor, PeerLinkManager};

        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Acceptor,
            "mesh-LocalA01-0",
        );
        let local_peer = PeerDescriptor::from_replica_ids(vec!["mesh-LocalA01-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        mgr.set_peer_connectivity_source(|_| PeerConnectivity::unavailable());
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        engine.set_latest_tunnel_config_for_test(crate::platform::TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-LocalA01-0".into(),
            replicas: 1,
            client_ids: vec!["mesh-LocalA01-0".into()],
            ..crate::platform::TunnelConfig::default()
        });
        engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install remote overlay route");
        engine
            .replace_peer_lan_aliases("mesh-RemoteB1-0", &["192.168.50.9".into()])
            .expect("install remote LAN alias");
        engine.set_native_lan_route_exclusions_for_test(&[]);
        let retired = Arc::new(Mutex::new(Vec::<String>::new()));
        let retired_for_sink = retired.clone();
        let engine_for_sink = Arc::clone(&engine);
        mgr.set_retired_peer_sink(move |peer_id| {
            let committed = engine_for_sink.retire_overlay_peer(peer_id.as_str());
            retired_for_sink
                .lock()
                .expect("retired Peer sink lock")
                .push(peer_id.as_str().to_string());
            committed
        });
        let started = std::time::Instant::now();

        mgr.buffer_membership_replica("mesh-RemoteB1-0".into());
        mgr.commit_membership_cycle_at(started);
        mgr.commit_membership_cycle_at(started + Duration::from_secs(1));
        assert_eq!(engine.lan_alias_route_cidrs(), vec!["192.168.50.9/32"]);
        mgr.commit_membership_cycle_at(started + Duration::from_secs(121));

        assert_eq!(
            *retired.lock().expect("retired Peer sink lock"),
            vec!["mesh-RemoteB1-0".to_string()]
        );
        assert!(engine.overlay_route_cidrs().is_empty());
        assert!(engine.lan_alias_route_cidrs().is_empty());
    }

    #[tokio::test]
    async fn mesh_acceptor_command_does_not_reenable_legacy_auto_initiator() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "peer-b-AbCd0002-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("peer-c-AbCd0003-0".into(), 0);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-b-AbCd0002-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );

        let run_handle = tokio::spawn(mgr.run());
        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnnounce { .. })
        ));
        in_tx
            .send(BinaryMessage::P2pPeerHint {
                peer_client_id: "peer-a-AbCd0001-0".into(),
            })
            .await
            .unwrap();
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "203.0.113.10".into(),
                public_port: 4433,
                server_time_ms: 1,
            })
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
                .await
                .is_err(),
            "mesh Acceptor work must not emit an Offer or revive legacy auto-initiation"
        );
        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("manager exits when signaling closes")
            .expect("manager task does not panic");
    }

    #[tokio::test]
    async fn canonical_mesh_initiator_alone_emits_r_exact_offers_regardless_of_legacy_role() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        async fn offers_for_view(
            local_replica_ids: Vec<String>,
            remote_replica_ids: Vec<String>,
            legacy_role: ClientRole,
            expected_offer_count: usize,
        ) -> Vec<(String, String)> {
            let local_client_id = local_replica_ids[0].clone();
            let replica_count = local_replica_ids.len();
            let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
            let (in_tx, in_rx) = tokio::sync::mpsc::channel(16);
            let mut mgr = P2pManager::new(
                make_test_multi_arc(),
                local_client_id,
                "g1".into(),
                CertFingerprint::from_bytes([1u8; 32]),
                legacy_role,
                in_rx,
                out_tx,
                4433,
            );
            mgr.set_allow_lan_candidates(true);
            mgr.set_mapping_probe_reflector_for_test(None);
            let local_peer =
                PeerDescriptor::from_replica_ids(local_replica_ids).expect("valid local Peer");
            mgr.set_peer_link_manager(
                PeerLinkManager::new(local_peer, replica_count).expect("non-zero Replica count"),
            );

            let run_handle = tokio::spawn(mgr.run());
            assert!(matches!(
                out_rx.recv().await,
                Some(BinaryMessage::P2pAnnounce { .. })
            ));
            for peer_client_id in remote_replica_ids {
                in_tx
                    .send(BinaryMessage::P2pPeerHint { peer_client_id })
                    .await
                    .unwrap();
            }
            in_tx
                .send(BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 1,
                })
                .await
                .unwrap();

            let mut offers = Vec::new();
            for _ in 0..expected_offer_count {
                let message = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
                    .await
                    .expect("mesh Offer timeout")
                    .expect("mesh Offer message");
                match message {
                    BinaryMessage::P2pOffer {
                        src_client_id,
                        dst_client_id,
                        ..
                    } => offers.push((src_client_id, dst_client_id)),
                    other => panic!("expected mesh P2pOffer, got {other:?}"),
                }
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
                    .await
                    .is_err(),
                "canonical Acceptor must not emit an Offer and each lane emits at most one"
            );
            drop(in_tx);
            tokio::time::timeout(Duration::from_secs(2), run_handle)
                .await
                .expect("manager exits when signaling closes")
                .expect("manager task does not panic");
            offers
        }

        let peer_a = (0..2)
            .map(|index| format!("peer-a-AbCd0001-{index}"))
            .collect::<Vec<_>>();
        let peer_b = (0..2)
            .map(|index| format!("peer-b-AbCd0002-{index}"))
            .collect::<Vec<_>>();

        let offers_from_a =
            offers_for_view(peer_a.clone(), peer_b.clone(), ClientRole::Acceptor, 2).await;
        assert_eq!(
            offers_from_a,
            vec![
                ("peer-a-AbCd0001-0".into(), "peer-b-AbCd0002-0".into()),
                ("peer-a-AbCd0001-1".into(), "peer-b-AbCd0002-1".into()),
            ]
        );

        let offers_from_b = offers_for_view(peer_b, peer_a, ClientRole::Initiator, 0).await;
        assert!(offers_from_b.is_empty());
    }

    #[tokio::test]
    async fn mesh_rejects_offer_sent_by_noncanonical_peer_direction() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Acceptor, "peer-a-AbCd0001-0");
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-a-AbCd0001-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );

        let session_id = SessionId::from_bytes([0xD1; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id,
            src_client_id: "peer-b-AbCd0002-0".into(),
            dst_client_id: "peer-a-AbCd0001-0".into(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 41000,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([0xD2; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        let answer = out_rx.recv().await.expect("rejection answer");
        mgr.cleanup_session_attempt(session_id);
        assert!(matches!(
            answer,
            BinaryMessage::P2pAnswer {
                session_id: answer_session_id,
                ok: false,
                ..
            } if answer_session_id == session_id
        ));
        assert!(
            mgr.peer_contexts.is_empty(),
            "a reverse-direction Offer must not occupy a relation"
        );
    }

    #[tokio::test]
    async fn mesh_rejects_offer_between_replicas_of_the_same_peer() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Initiator, "peer-a-AbCd0001-0");
        let local_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-a-AbCd0001-0".into(),
            "peer-a-AbCd0001-1".into(),
        ])
        .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );

        let session_id = SessionId::from_bytes([0xD3; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id,
            src_client_id: "peer-a-AbCd0001-1".into(),
            dst_client_id: "peer-a-AbCd0001-0".into(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 41000,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([0xD4; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnswer {
                session_id: answer_session_id,
                ok: false,
                reason,
                ..
            }) if answer_session_id == session_id && reason == "same Peer family"
        ));
        assert!(mgr.peer_contexts.is_empty());
    }

    #[tokio::test]
    async fn mesh_coalesces_second_generation_for_the_same_peer_lane_before_reservation() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, multi) =
            make_test_manager_with_role(ClientRole::Initiator, "peer-b-AbCd0002-0");
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-b-AbCd0002-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("peer-b-AbCd0002-0", multi);
        mgr.set_session_installer(engine.attach_p2p_session_installer());

        let candidate = Candidate {
            ip: "8.8.8.8".into(),
            port: 41000,
            kind: CandidateKind::ServerReflexive,
        };
        let first_session = SessionId::from_bytes([0xE1; 16]);
        let second_session = SessionId::from_bytes([0xE2; 16]);
        for session_id in [first_session, second_session] {
            mgr.handle_message(BinaryMessage::P2pOffer {
                session_id,
                src_client_id: "peer-a-AbCd0001-0".into(),
                dst_client_id: "peer-b-AbCd0002-0".into(),
                candidates: vec![candidate.clone()],
                src_cert_fp: CertFingerprint::from_bytes([0xE3; 32]),
                role: P2pRole::Initiator,
            })
            .await;
        }

        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnswer {
                session_id,
                ok: true,
                ..
            }) if session_id == first_session
        ));
        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnswer {
                session_id,
                ok: false,
                reason,
                ..
            }) if session_id == second_session && reason == "mesh relation busy"
        ));
        assert_eq!(mgr.peer_contexts.len(), 1);
        assert!(mgr.peer_contexts.contains_key(&first_session));
        let relation_key = mgr.peer_contexts[&first_session]
            .mesh_relation_key
            .as_ref()
            .expect("incoming session has canonical relation key");
        assert_eq!(relation_key.first_peer_family, "peer-a-AbCd0001-0");
        assert_eq!(relation_key.second_peer_family, "peer-b-AbCd0002-0");
        assert_eq!(relation_key.lane_index, 0);
        assert!(engine.has_pending_p2p_session_install_for_test(first_session));
        assert!(!engine.has_pending_p2p_session_install_for_test(second_session));

        mgr.cleanup_session_attempt(first_session);
    }

    #[tokio::test]
    async fn mesh_initiator_releases_answered_relation_when_punch_sync_never_arrives() {
        use crate::peer_link_manager::{MembershipSnapshot, PeerDescriptor, PeerLinkManager};
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let local_id = "peer-a-AbCd0001-0";
        let remote_id = "peer-b-AbCd0002-0";
        let (mut mgr, _out_rx, multi) =
            make_test_manager_with_role(ClientRole::Initiator, local_id);
        let local_peer =
            PeerDescriptor::from_replica_ids(vec![local_id.into()]).expect("valid local Peer");
        let remote_peer =
            PeerDescriptor::from_replica_ids(vec![remote_id.into()]).expect("valid remote Peer");
        let mut link_manager = PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count");
        let PeerLinkCommand::EnsureLane(lane) = link_manager
            .apply_snapshot(&MembershipSnapshot::new(vec![remote_peer]))
            .pop()
            .expect("one desired lane");
        let relation_key =
            P2pManager::mesh_relation_key_for_lane(&lane).expect("canonical mesh relation key");
        mgr.set_peer_link_manager(link_manager);
        mgr.mesh_relation_lanes
            .insert(relation_key.clone(), lane.clone());

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test(local_id, multi.clone());
        let installer = engine.attach_p2p_session_installer();
        mgr.set_session_installer(installer.clone());

        let session_id = SessionId::from_bytes([0xE4; 16]);
        assert!(installer.reserve_for_relation(
            session_id,
            Some(local_id),
            Some(remote_id),
            Some(relation_key.clone()),
        ));
        multi.set_state(P2pState::Negotiating { session_id });
        mgr.peer_contexts.insert(
            session_id,
            PeerContext {
                peer_client_id: Some(remote_id.into()),
                local_client_id: Some(local_id.into()),
                allow_parallel: true,
                family: Some(P2pAddressFamily::Ipv4),
                session_role: Some(ClientRole::Initiator),
                mesh_relation_key: Some(relation_key.clone()),
                ..PeerContext::default()
            },
        );
        mgr.schedule_offer_answer_timeout(session_id);

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id,
            accepted_client_id: remote_id.into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 41000,
                kind: CandidateKind::ServerReflexive,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([0xE5; 32]),
        })
        .await;

        let event = tokio::time::timeout(Duration::from_secs(1), mgr.internal_rx.recv())
            .await
            .expect("answered negotiation must remain bounded while awaiting PunchSync")
            .expect("signaling timeout event");
        assert!(matches!(
            event,
            P2pInternalEvent::OfferAnswerTimedOut {
                session_id: timed_out,
                ..
            } if timed_out == session_id
        ));
        mgr.handle_internal_event(event);

        assert!(!mgr.peer_contexts.contains_key(&session_id));
        assert!(!installer.has_reserved_session(session_id));
        assert!(mgr.pending_peer_link_commands.iter().any(|command| {
            let PeerLinkCommand::EnsureLane(retried_lane) = command;
            P2pManager::mesh_relation_key_for_lane(retried_lane).as_ref() == Ok(&relation_key)
        }));
    }

    #[tokio::test]
    async fn mesh_acceptor_releases_answered_relation_when_punch_sync_never_arrives() {
        use crate::peer_link_manager::{MembershipSnapshot, PeerDescriptor, PeerLinkManager};
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let initiator_id = "peer-a-AbCd0001-0";
        let acceptor_id = "peer-b-AbCd0002-0";
        let (mut mgr, mut out_rx, multi) =
            make_test_manager_with_role(ClientRole::Acceptor, acceptor_id);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer =
            PeerDescriptor::from_replica_ids(vec![acceptor_id.into()]).expect("valid local Peer");
        let remote_peer =
            PeerDescriptor::from_replica_ids(vec![initiator_id.into()]).expect("valid remote Peer");
        let mut link_manager = PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count");
        let _ = link_manager.apply_snapshot(&MembershipSnapshot::new(vec![remote_peer]));
        mgr.set_peer_link_manager(link_manager);

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test(acceptor_id, multi);
        let installer = engine.attach_p2p_session_installer();
        mgr.set_session_installer(installer.clone());

        let candidate = Candidate {
            ip: "8.8.8.8".into(),
            port: 41000,
            kind: CandidateKind::ServerReflexive,
        };
        let first_session = SessionId::from_bytes([0xE6; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: first_session,
            src_client_id: initiator_id.into(),
            dst_client_id: acceptor_id.into(),
            candidates: vec![candidate.clone()],
            src_cert_fp: CertFingerprint::from_bytes([0xE7; 32]),
            role: P2pRole::Initiator,
        })
        .await;
        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnswer {
                session_id,
                ok: true,
                ..
            }) if session_id == first_session
        ));

        let event = tokio::time::timeout(Duration::from_secs(1), mgr.internal_rx.recv())
            .await
            .expect("acceptor reservation must be bounded while awaiting PunchSync")
            .expect("signaling timeout event");
        assert!(matches!(
            event,
            P2pInternalEvent::OfferAnswerTimedOut {
                session_id: timed_out,
                ..
            } if timed_out == first_session
        ));
        mgr.handle_internal_event(event);
        assert!(!mgr.peer_contexts.contains_key(&first_session));
        assert!(!installer.has_reserved_session(first_session));

        let replacement_session = SessionId::from_bytes([0xE8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: replacement_session,
            src_client_id: initiator_id.into(),
            dst_client_id: acceptor_id.into(),
            candidates: vec![candidate],
            src_cert_fp: CertFingerprint::from_bytes([0xE9; 32]),
            role: P2pRole::Initiator,
        })
        .await;
        assert!(matches!(
            out_rx.recv().await,
            Some(BinaryMessage::P2pAnswer {
                session_id,
                ok: true,
                ..
            }) if session_id == replacement_session
        ));
        mgr.cleanup_session_attempt(replacement_session);
    }

    #[tokio::test]
    async fn mesh_exact_lane_waits_when_its_local_replica_relay_is_unavailable() {
        use crate::peer_link_manager::{MembershipSnapshot, PeerDescriptor, PeerLinkManager};

        let local_ids = (0..3)
            .map(|index| format!("peer-a-AbCd0001-{index}"))
            .collect::<Vec<_>>();
        let remote_ids = (0..3)
            .map(|index| format!("peer-b-AbCd0002-{index}"))
            .collect::<Vec<_>>();
        let (mut mgr, mut out_rx, anchor_multi) =
            make_test_manager_with_role(ClientRole::Initiator, &local_ids[0]);
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);

        let local_peer =
            PeerDescriptor::from_replica_ids(local_ids.clone()).expect("valid local Peer");
        let remote_peer = PeerDescriptor::from_replica_ids(remote_ids).expect("valid remote Peer");
        let mut link_manager = PeerLinkManager::new(local_peer, 3).expect("non-zero Replica count");
        let commands = link_manager.apply_snapshot(&MembershipSnapshot::new(vec![remote_peer]));
        mgr.set_peer_link_manager(link_manager);
        mgr.pending_peer_link_commands = commands
            .into_iter()
            .filter(|command| {
                let PeerLinkCommand::EnsureLane(lane) = command;
                lane.index() == 2
            })
            .collect();

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test(&local_ids[0], anchor_multi);
        engine.set_replicas_for_test(3);
        mgr.set_session_installer(engine.attach_p2p_session_installer());

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL),
            "an exact mesh lane must wait for its declared local Replica instead of reserving another relay"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "the manager must not emit an Offer claiming Replica 2 over Replica 0's authenticated relay"
        );
        assert_eq!(mgr.pending_peer_link_commands.len(), 1);
        assert!(mgr.peer_contexts.is_empty());
    }

    #[tokio::test]
    async fn mesh_pending_retry_uses_latest_replica_target_after_membership_update() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let local_ids = vec![
            "peer-a-AbCd0001-0".to_string(),
            "peer-a-AbCd0001-1".to_string(),
        ];
        let remote_zero = "peer-b-AbCd0002-0";
        let remote_one = "peer-b-AbCd0002-1";
        let (mut mgr, mut out_rx, anchor_multi) =
            make_test_manager_with_role(ClientRole::Initiator, &local_ids[0]);
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer =
            PeerDescriptor::from_replica_ids(local_ids.clone()).expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test(&local_ids[0], anchor_multi);
        engine.set_replicas_for_test(2);
        mgr.set_session_installer(engine.attach_p2p_session_installer());

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: remote_zero.into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;
        mgr.pending_peer_link_commands.retain(|command| {
            let PeerLinkCommand::EnsureLane(lane) = command;
            lane.index() == 1
        });

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );
        assert!(out_rx.try_recv().is_err());
        assert_eq!(mgr.pending_peer_link_commands.len(), 1);

        engine.install_proxy_replica_session_for_test(&local_ids[1], make_test_multi_arc());
        for peer_client_id in [remote_zero, remote_one] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 2,
        })
        .await;

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let session_id = match out_rx.recv().await.expect("updated exact Offer") {
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id,
                dst_client_id,
                ..
            } => {
                assert_eq!(src_client_id, local_ids[1]);
                assert_eq!(
                    dst_client_id, remote_one,
                    "a retained retry must not restore the stale modulo target"
                );
                session_id
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        };
        assert_eq!(
            engine.pending_p2p_local_client_id_for_test(session_id),
            Some(local_ids[1].clone())
        );
        mgr.cleanup_session_attempt(session_id);
    }

    #[tokio::test]
    async fn mesh_cleanup_releases_relation_and_requeues_exact_lane() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Acceptor, "peer-a-AbCd0001-0");
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-a-AbCd0001-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "peer-b-AbCd0002-0".into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let first_session = match out_rx.recv().await.expect("first exact Offer") {
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id,
                dst_client_id,
                ..
            } => {
                assert_eq!(src_client_id, "peer-a-AbCd0001-0");
                assert_eq!(dst_client_id, "peer-b-AbCd0002-0");
                session_id
            }
            other => panic!("expected first P2pOffer, got {other:?}"),
        };

        mgr.cleanup_session_attempt(first_session);
        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        match tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("retry Offer timeout")
            .expect("retried exact Offer")
        {
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id,
                dst_client_id,
                ..
            } => {
                assert_ne!(session_id, first_session);
                assert_eq!(src_client_id, "peer-a-AbCd0001-0");
                assert_eq!(dst_client_id, "peer-b-AbCd0002-0");
                mgr.cleanup_session_attempt(session_id);
            }
            other => panic!("expected retried P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mesh_installed_relation_close_requeues_only_the_exact_canonical_lane() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Acceptor, "peer-a-AbCd0001-0");
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-a-AbCd0001-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        for peer_client_id in ["peer-b-AbCd0002-0", "peer-c-AbCd0003-0"] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let mut first_sessions = HashMap::new();
        for _ in 0..2 {
            match out_rx.recv().await.expect("initial exact Offer") {
                BinaryMessage::P2pOffer {
                    session_id,
                    dst_client_id,
                    ..
                } => {
                    first_sessions.insert(dst_client_id, session_id);
                }
                other => panic!("expected initial P2pOffer, got {other:?}"),
            }
        }
        let peer_b_session = first_sessions["peer-b-AbCd0002-0"];
        let peer_c_session = first_sessions["peer-c-AbCd0003-0"];

        let relation_events = mgr.refill_handle();
        relation_events.relation_closed(peer_b_session);
        let event = mgr
            .internal_rx
            .recv()
            .await
            .expect("exact relation close event");
        mgr.handle_internal_event(event);
        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let replacement_session =
            match tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
                .await
                .expect("replacement Offer timeout")
                .expect("replacement exact Offer")
            {
                BinaryMessage::P2pOffer {
                    session_id,
                    dst_client_id,
                    ..
                } => {
                    assert_ne!(session_id, peer_b_session);
                    assert_ne!(session_id, peer_c_session);
                    assert_eq!(dst_client_id, "peer-b-AbCd0002-0");
                    session_id
                }
                other => panic!("expected replacement P2pOffer, got {other:?}"),
            };

        // A duplicated terminal notification from an older transport task is
        // harmless and must not disturb either the B replacement or C lane.
        relation_events.relation_closed(peer_b_session);
        let event = mgr
            .internal_rx
            .recv()
            .await
            .expect("duplicate exact relation close event");
        mgr.handle_internal_event(event);
        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), out_rx.recv())
                .await
                .is_err(),
            "stale close replay must not emit another Offer"
        );

        mgr.cleanup_session_attempt(replacement_session);
        mgr.cleanup_session_attempt(peer_c_session);
    }

    #[tokio::test]
    async fn mesh_complete_replica_view_updates_retry_target_without_duplicate_relation() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, _out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Acceptor, "peer-a-AbCd0001-0");
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-a-AbCd0001-0".into(),
            "peer-a-AbCd0001-1".into(),
        ])
        .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "peer-b-AbCd0002-0".into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;
        let _ = mgr.maybe_execute_peer_link_commands().await;
        let lane_one = mgr
            .mesh_relation_lanes
            .iter()
            .find(|(key, _)| key.lane_index == 1)
            .map(|(_, lane)| lane)
            .expect("lane one desired relation");
        assert_eq!(lane_one.remote_replica_id(), "peer-b-AbCd0002-0");

        for peer_client_id in ["peer-b-AbCd0002-0", "peer-b-AbCd0002-1"] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 2,
        })
        .await;
        let occupied_before = mgr
            .peer_contexts
            .values()
            .filter(|context| {
                context
                    .mesh_relation_key
                    .as_ref()
                    .is_some_and(|key| key.lane_index == 1)
            })
            .count();
        let _ = mgr.maybe_execute_peer_link_commands().await;

        let lane_one = mgr
            .mesh_relation_lanes
            .iter()
            .find(|(key, _)| key.lane_index == 1)
            .map(|(_, lane)| lane)
            .expect("updated lane one desired relation");
        assert_eq!(
            lane_one.remote_replica_id(),
            "peer-b-AbCd0002-1",
            "when the current relation next retries it must use the newly available equal-index Replica"
        );
        assert_eq!(
            mgr.peer_contexts
                .values()
                .filter(|context| {
                    context
                        .mesh_relation_key
                        .as_ref()
                        .is_some_and(|key| key.lane_index == 1)
                })
                .count(),
            occupied_before,
            "updating desired identity must not create a second live/in-flight relation"
        );
    }

    #[tokio::test]
    async fn mesh_initial_reservation_failure_retains_command_for_bounded_retry() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, mut out_rx, multi) =
            make_test_manager_with_role(ClientRole::Initiator, "peer-a-AbCd0001-0");
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-a-AbCd0001-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "peer-b-AbCd0002-0".into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );
        assert_eq!(mgr.pending_peer_link_commands.len(), 1);
        assert!(out_rx.try_recv().is_err());

        engine.install_proxy_replica_session_for_test("peer-a-AbCd0001-0", multi);
        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let session_id = match out_rx.recv().await.expect("Offer after retry") {
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id,
                dst_client_id,
                ..
            } => {
                assert_eq!(src_client_id, "peer-a-AbCd0001-0");
                assert_eq!(dst_client_id, "peer-b-AbCd0002-0");
                session_id
            }
            other => panic!("expected retried P2pOffer, got {other:?}"),
        };
        mgr.cleanup_session_attempt(session_id);
    }

    #[tokio::test]
    async fn mesh_duplicate_ensure_for_one_relation_emits_one_offer() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Acceptor, "peer-a-AbCd0001-0");
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec!["peer-a-AbCd0001-0".into()])
            .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 1).expect("non-zero Replica count"),
        );
        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "peer-b-AbCd0002-0".into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;
        let duplicate = mgr
            .pending_peer_link_commands
            .front()
            .cloned()
            .expect("initial EnsureLane command");
        mgr.pending_peer_link_commands.push_back(duplicate);

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let first_session = match out_rx.recv().await.expect("one exact Offer") {
            BinaryMessage::P2pOffer { session_id, .. } => session_id,
            other => panic!("expected P2pOffer, got {other:?}"),
        };
        assert!(
            out_rx.try_recv().is_err(),
            "duplicate EnsureLane is coalesced"
        );
        assert_eq!(
            mgr.peer_contexts
                .values()
                .filter(|context| context.mesh_relation_key.is_some())
                .count(),
            1
        );
        mgr.cleanup_session_attempt(first_session);
    }

    #[tokio::test]
    async fn mesh_distinct_peer_and_lane_relations_start_in_parallel() {
        use crate::peer_link_manager::{PeerDescriptor, PeerLinkManager};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(ClientRole::Acceptor, "peer-a-AbCd0001-0");
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let local_peer = PeerDescriptor::from_replica_ids(vec![
            "peer-a-AbCd0001-0".into(),
            "peer-a-AbCd0001-1".into(),
        ])
        .expect("valid local Peer");
        mgr.set_peer_link_manager(
            PeerLinkManager::new(local_peer, 2).expect("non-zero Replica count"),
        );
        for peer_client_id in [
            "peer-b-AbCd0002-0",
            "peer-b-AbCd0002-1",
            "peer-c-AbCd0003-0",
            "peer-c-AbCd0003-1",
        ] {
            mgr.handle_message(BinaryMessage::P2pPeerHint {
                peer_client_id: peer_client_id.into(),
            })
            .await;
        }
        mgr.handle_message(BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms: 1,
        })
        .await;

        assert_eq!(
            mgr.maybe_execute_peer_link_commands().await,
            AutoInitiatorAttempt::Stop
        );
        let mut exact_relations = BTreeSet::new();
        let mut session_ids = Vec::new();
        for _ in 0..4 {
            match out_rx.recv().await.expect("parallel exact Offer") {
                BinaryMessage::P2pOffer {
                    session_id,
                    src_client_id,
                    dst_client_id,
                    ..
                } => {
                    session_ids.push(session_id);
                    exact_relations.insert((src_client_id, dst_client_id));
                }
                other => panic!("expected P2pOffer, got {other:?}"),
            }
        }
        assert_eq!(
            exact_relations,
            BTreeSet::from([
                ("peer-a-AbCd0001-0".into(), "peer-b-AbCd0002-0".into()),
                ("peer-a-AbCd0001-1".into(), "peer-b-AbCd0002-1".into()),
                ("peer-a-AbCd0001-0".into(), "peer-c-AbCd0003-0".into()),
                ("peer-a-AbCd0001-1".into(), "peer-c-AbCd0003-1".into()),
            ])
        );
        let relation_keys = mgr
            .peer_contexts
            .values()
            .filter_map(|context| context.mesh_relation_key.as_ref())
            .map(|key| {
                (
                    key.first_peer_family.clone(),
                    key.second_peer_family.clone(),
                    key.lane_index,
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            relation_keys,
            BTreeSet::from([
                ("peer-a-AbCd0001-0".into(), "peer-b-AbCd0002-0".into(), 0),
                ("peer-a-AbCd0001-0".into(), "peer-b-AbCd0002-0".into(), 1),
                ("peer-a-AbCd0001-0".into(), "peer-c-AbCd0003-0".into(), 0),
                ("peer-a-AbCd0001-0".into(), "peer-c-AbCd0003-0".into(), 1),
            ])
        );
        for session_id in session_ids {
            mgr.cleanup_session_attempt(session_id);
        }
    }

    #[test]
    fn p2p_runtime_timeouts_are_stable_for_file_management() {
        assert_eq!(DEFAULT_COOLDOWN_INITIAL, Duration::from_secs(60));
        assert_eq!(DEFAULT_COOLDOWN_MAX, Duration::from_secs(600));
        assert_eq!(REANNOUNCE_INTERVAL_SECS, 30);
        assert_eq!(PRODUCTION_OFFER_ANSWER_TIMEOUT, Duration::from_secs(15));
        assert_eq!(
            PRODUCTION_V2_ACCEPTOR_RECOVERY_DELAY,
            Duration::from_secs(16)
        );
        assert!(PRODUCTION_V2_ACCEPTOR_RECOVERY_DELAY > PRODUCTION_OFFER_ANSWER_TIMEOUT);
        assert_eq!(MAPPING_PROBE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(PUNCH_ACK_TIMEOUT, Duration::from_secs(3));
        assert_eq!(LAN_PUNCH_ACK_TIMEOUT, Duration::from_secs(1));
        assert_eq!(RESPONDER_PROBE_WINDOW, Duration::from_secs(4));
        assert_eq!(RESPONDER_QUIC_ACCEPT_TIMEOUT, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn p2p_client_endpoint_primes_acceptor_stream_without_business_data() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle =
            crate::p2p::cert::generate_self_signed_cert("p2p-prime-server").expect("server cert");
        let initiator_bundle = crate::p2p::cert::generate_self_signed_cert("p2p-prime-initiator")
            .expect("initiator cert");
        let listener =
            crate::p2p::listener::P2pListener::bind(&server_bundle).expect("listener bind");
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], listener.local_addr().port()));
        let (endpoint, _) = listener.into_parts();

        let expected = crate::p2p::expected::ExpectedPeerMap::default();
        let expected_session_id = SessionId::from_bytes([12u8; 16]);
        expected.insert(
            expected_session_id,
            crate::p2p::expected::ExpectedPeer {
                peer_client_id: "initiator-1".into(),
                cert_fp: initiator_bundle.fingerprint,
                candidates: vec![],
            },
        );
        let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, tp_transport::session::Session) + Send + Sync> =
            Arc::new(move |_session_id, session| {
                let capabilities = session.capabilities();
                let _ = accepted_tx.try_send(capabilities.tcp_flow_stream_v1);
            });
        let listener_task = tokio::spawn(crate::p2p::listener::run_listener_loop(
            endpoint,
            expected,
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let session = build_p2p_client_endpoint(
            sock,
            server_bundle.fingerprint,
            peer,
            &initiator_bundle,
            "initiator-1",
            expected_session_id,
        )
        .await
        .expect("build p2p client endpoint");

        let accepted_tcp_flow_stream_v1 =
            tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
                .await
                .expect("listener should accept after endpoint build, before business data")
                .expect("listener accepted channel closed");
        assert!(
            accepted_tcp_flow_stream_v1,
            "accepted P2P session must enable true TCP flow streams"
        );
        assert!(
            session.capabilities().tcp_flow_stream_v1,
            "initiator P2P session must enable true TCP flow streams"
        );
        let (_sender, mut receiver, _datagram_receiver) = session.split();
        assert!(
            receiver.take_tcp_flow_receiver().is_some(),
            "P2P session split must expose a TCP flow receiver"
        );

        listener_task.abort();
    }

    #[test]
    fn public_p2p_candidates_keep_only_public_routable_addresses() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "192.168.1.44".to_string(),
                port: 4433,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "100.64.12.7".to_string(),
                port: 4433,
                kind: CandidateKind::ServerReflexive,
            },
            Candidate {
                ip: "8.8.8.8".to_string(),
                port: 4433,
                kind: CandidateKind::ServerReflexive,
            },
            Candidate {
                ip: "1.1.1.1".to_string(),
                port: 4434,
                kind: CandidateKind::Host,
            },
        ];

        let public = public_p2p_candidates(candidates);

        assert_eq!(public.len(), 2);
        assert_eq!(public[0].ip, "8.8.8.8");
        assert_eq!(public[1].ip, "1.1.1.1");
    }

    #[test]
    fn public_p2p_candidates_drop_lan_loopback_link_local_and_documentation_addresses() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let input = [
            "10.0.0.7",
            "172.16.0.7",
            "192.168.1.44",
            "127.0.0.1",
            "169.254.1.1",
            "203.0.113.7",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ];

        let candidates = input
            .iter()
            .map(|ip| Candidate {
                ip: (*ip).to_string(),
                port: 4433,
                kind: CandidateKind::Host,
            })
            .collect();

        assert!(public_p2p_candidates(candidates).is_empty());
    }

    #[test]
    fn default_public_candidates_drop_lan_and_keep_public() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "192.168.1.44".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "1.1.1.1".to_string(),
                port: 51526,
                kind: CandidateKind::ServerReflexive,
            },
        ];

        let filtered = public_p2p_candidates(candidates);

        assert_eq!(
            filtered,
            vec![Candidate {
                ip: "1.1.1.1".to_string(),
                port: 51526,
                kind: CandidateKind::ServerReflexive,
            }]
        );
    }

    #[test]
    fn local_default_public_candidates_synthesize_only_public_observed_endpoint() {
        use std::net::SocketAddr;
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let raw = vec![Candidate {
            ip: "192.168.1.44".to_string(),
            port: 51526,
            kind: CandidateKind::Host,
        }];
        let observed: SocketAddr = "1.1.1.1:62000".parse().unwrap();

        let candidates = local_p2p_candidates(51526, Some(observed), raw, false);

        assert_eq!(
            candidates,
            vec![Candidate {
                ip: "1.1.1.1".to_string(),
                port: 62000,
                kind: CandidateKind::ServerReflexive,
            }]
        );
    }

    #[test]
    fn local_lan_candidates_are_kept_when_explicitly_allowed() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let raw = vec![
            Candidate {
                ip: "192.168.1.44".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "10.0.0.9".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "8.8.8.8".to_string(),
                port: 51526,
                kind: CandidateKind::ServerReflexive,
            },
        ];

        let candidates = local_p2p_candidates(51526, None, raw, true);

        assert_eq!(
            candidates,
            vec![
                Candidate {
                    ip: "192.168.1.44".to_string(),
                    port: 51526,
                    kind: CandidateKind::Host,
                },
                Candidate {
                    ip: "10.0.0.9".to_string(),
                    port: 51526,
                    kind: CandidateKind::Host,
                },
                Candidate {
                    ip: "8.8.8.8".to_string(),
                    port: 51526,
                    kind: CandidateKind::ServerReflexive,
                },
            ]
        );
    }

    #[test]
    fn announce_locals_drop_lan_when_lan_p2p_is_disabled() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let raw = vec![
            Candidate {
                ip: "192.168.1.44".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "8.8.8.8".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
        ];

        let locals = announce_locals_from_candidates(raw, false);

        assert_eq!(locals, vec![("8.8.8.8".to_string(), 51526)]);
    }

    #[test]
    fn announce_locals_keep_lan_when_lan_p2p_is_enabled() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let raw = vec![
            Candidate {
                ip: "192.168.1.44".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "127.0.0.1".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "8.8.8.8".to_string(),
                port: 51526,
                kind: CandidateKind::Host,
            },
        ];

        let locals = announce_locals_from_candidates(raw, true);

        assert_eq!(
            locals,
            vec![
                ("192.168.1.44".to_string(), 51526),
                ("8.8.8.8".to_string(), 51526),
            ]
        );
    }

    #[test]
    fn pinned_underlay_publishes_host_candidates_only_from_the_selected_nic() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let selected_hosts = ["192.168.240.44".parse::<IpAddr>().unwrap()]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let filtered = filter_local_host_candidates_for_underlay(
            vec![
                Candidate {
                    ip: "192.168.240.44".into(),
                    port: 51526,
                    kind: CandidateKind::Host,
                },
                Candidate {
                    ip: "10.0.0.8".into(),
                    port: 51526,
                    kind: CandidateKind::Host,
                },
                Candidate {
                    ip: "8.8.8.8".into(),
                    port: 3078,
                    kind: CandidateKind::ServerReflexive,
                },
            ],
            Some(&selected_hosts),
        );

        assert_eq!(
            filtered,
            vec![
                Candidate {
                    ip: "192.168.240.44".into(),
                    port: 51526,
                    kind: CandidateKind::Host,
                },
                Candidate {
                    ip: "8.8.8.8".into(),
                    port: 3078,
                    kind: CandidateKind::ServerReflexive,
                },
            ],
            "a socket pinned to NIC index 7 must not publish NIC index 9's 10.0.0.8 Host candidate",
        );
    }

    #[test]
    fn lan_p2p_socket_candidates_keep_only_lan_addresses() {
        use std::net::SocketAddr;

        let candidates: Vec<SocketAddr> = vec![
            "9.9.9.9:4209".parse().unwrap(),
            "192.168.1.10:38471".parse().unwrap(),
            "10.255.0.2:38471".parse().unwrap(),
            "[fc00::1]:38471".parse().unwrap(),
            "[2001:db8:1:2::1]:38471".parse().unwrap(),
        ];

        assert_eq!(
            lan_p2p_socket_candidates(&candidates),
            vec![
                "192.168.1.10:38471".parse::<SocketAddr>().unwrap(),
                "10.255.0.2:38471".parse().unwrap(),
                "[fc00::1]:38471".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn fresh_lan_sidecar_is_limited_to_v2_macos_ipv4_mixed_candidates() {
        struct Case {
            name: &'static str,
            has_v2_profile: bool,
            is_macos: bool,
            family: Option<P2pAddressFamily>,
            has_lan_candidate: bool,
            has_public_candidate: bool,
            expected: bool,
        }

        let cases = [
            Case {
                name: "v2 macos ipv4 mixed Host and public",
                has_v2_profile: true,
                is_macos: true,
                family: Some(P2pAddressFamily::Ipv4),
                has_lan_candidate: true,
                has_public_candidate: true,
                expected: true,
            },
            Case {
                name: "legacy without v2 profile",
                has_v2_profile: false,
                is_macos: true,
                family: Some(P2pAddressFamily::Ipv4),
                has_lan_candidate: true,
                has_public_candidate: true,
                expected: false,
            },
            Case {
                name: "non macos",
                has_v2_profile: true,
                is_macos: false,
                family: Some(P2pAddressFamily::Ipv4),
                has_lan_candidate: true,
                has_public_candidate: true,
                expected: false,
            },
            Case {
                name: "ipv6",
                has_v2_profile: true,
                is_macos: true,
                family: Some(P2pAddressFamily::Ipv6),
                has_lan_candidate: true,
                has_public_candidate: true,
                expected: false,
            },
            Case {
                name: "Host only",
                has_v2_profile: true,
                is_macos: true,
                family: Some(P2pAddressFamily::Ipv4),
                has_lan_candidate: true,
                has_public_candidate: false,
                expected: false,
            },
            Case {
                name: "public only",
                has_v2_profile: true,
                is_macos: true,
                family: Some(P2pAddressFamily::Ipv4),
                has_lan_candidate: false,
                has_public_candidate: true,
                expected: false,
            },
        ];

        for case in cases {
            assert_eq!(
                should_use_fresh_macos_ipv4_lan_socket(
                    case.has_v2_profile,
                    case.is_macos,
                    case.family,
                    case.has_lan_candidate,
                    case.has_public_candidate,
                ),
                case.expected,
                "{}",
                case.name,
            );
        }
    }

    #[test]
    fn local_public_candidate_uses_socket_observed_port_when_available() {
        use std::net::SocketAddr;
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let raw = Vec::<Candidate>::new();
        let observed: SocketAddr = "1.1.1.1:3078".parse().unwrap();

        let candidates = local_p2p_candidates(50102, Some(observed), raw, false);

        assert_eq!(
            candidates,
            vec![Candidate {
                ip: "1.1.1.1".to_string(),
                port: 3078,
                kind: CandidateKind::ServerReflexive,
            }]
        );
    }

    #[test]
    fn select_dial_target_prefers_matching_ip() {
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "10.0.0.5:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec![
            "192.168.1.1:4433".parse().unwrap(),
            "10.0.0.5:4433".parse().unwrap(),
            "127.0.0.1:4433".parse().unwrap(),
        ];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(target, Some(probe_src));
    }

    #[test]
    fn select_dial_target_uses_probe_ack_source_port() {
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "10.0.0.5:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec!["10.0.0.5:4433".parse().unwrap()];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(
            target,
            Some(probe_src),
            "after same-socket punching, the ProbeAck source is the reachable QUIC endpoint"
        );
    }

    #[test]
    fn select_dial_target_returns_none_on_no_ip_match() {
        // No silent fallback. With no matching IP among
        // candidates the helper returns `None` and the punch driver
        // aborts the attempt as `NatFail`.
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "203.0.113.7:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec![
            "192.168.1.1:4433".parse().unwrap(),
            "10.0.0.5:4433".parse().unwrap(),
        ];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(target, None);
    }

    #[test]
    fn select_dial_target_returns_none_on_empty_candidates() {
        // Explicit empty handling. The previous implementation
        // panicked here via `candidates[0]`; the call site has its own
        // empty-candidates pre-flight (manager.rs:~440), but the helper
        // must defend itself too.
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "10.0.0.5:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec![];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(target, None);
    }

    #[test]
    fn successful_direct_path_observation_uses_actual_peer_and_family_index() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "10.0.0.9".into(),
                port: 41000,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "198.51.100.23".into(),
                port: 42000,
                kind: CandidateKind::ServerReflexive,
            },
        ];
        let indexes = Some(P2pUnderlayInterfaceIndexes {
            ipv4: NonZeroU32::new(7),
            ipv6: NonZeroU32::new(70),
            ipv4_source_ip: Some("192.168.240.44".parse().unwrap()),
        });

        let observation = successful_direct_path_observation(
            "peer-b-AbCd0002-0",
            "198.51.100.23:42000".parse().unwrap(),
            "0.0.0.0:53000".parse().unwrap(),
            &candidates,
            indexes,
        );

        assert_eq!(
            observation.remote_candidate_ip,
            "198.51.100.23".parse::<IpAddr>().unwrap()
        );
        assert_eq!(observation.peer_client_id, "peer-b-AbCd0002-0");
        assert_eq!(
            observation.remote_candidate_kind,
            Some(CandidateKind::ServerReflexive)
        );
        assert_eq!(observation.socket_family, P2pAddressFamily::Ipv4);
        assert_eq!(observation.selected_ifindex, NonZeroU32::new(7));
    }

    #[test]
    fn successful_direct_path_observation_does_not_borrow_other_family_index() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![Candidate {
            ip: "2001:db8::23".into(),
            port: 42000,
            kind: CandidateKind::Host,
        }];
        let indexes = Some(P2pUnderlayInterfaceIndexes {
            ipv4: NonZeroU32::new(7),
            ipv6: None,
            ipv4_source_ip: Some("192.168.240.44".parse().unwrap()),
        });

        let observation = successful_direct_path_observation(
            "peer-b-AbCd0002-0",
            "[2001:db8::23]:42000".parse().unwrap(),
            "[::]:53000".parse().unwrap(),
            &candidates,
            indexes,
        );

        assert_eq!(observation.socket_family, P2pAddressFamily::Ipv6);
        assert_eq!(observation.selected_ifindex, None);
    }

    #[test]
    fn select_dial_target_returns_none_on_family_mismatch_ipv4_probe_ipv6_candidates() {
        // IPv4 probe-ack with only IPv6 candidates must NOT
        // silently dial an IPv6 candidate — that drives QUIC into an
        // inevitable handshake timeout.
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "10.0.0.5:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec![
            "[2001:db8::1]:4433".parse().unwrap(),
            "[2001:db8::2]:4433".parse().unwrap(),
        ];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(target, None);
    }

    #[test]
    fn select_dial_target_returns_none_on_family_mismatch_ipv6_probe_ipv4_candidates() {
        // Symmetric case: IPv6 probe-ack with only IPv4 candidates.
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "[2001:db8::1]:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec![
            "192.168.1.1:4433".parse().unwrap(),
            "10.0.0.5:4433".parse().unwrap(),
        ];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(target, None);
    }

    #[test]
    fn select_dial_target_picks_correct_family_when_mixed_candidates() {
        // With mixed-family candidates, an IPv4 probe must select
        // the matching IPv4 candidate even when an IPv6 candidate sits
        // earlier in the list (no positional fallback).
        use std::net::SocketAddr;
        let probe_src: SocketAddr = "10.0.0.5:54321".parse().unwrap();
        let candidates: Vec<SocketAddr> = vec![
            "[2001:db8::1]:4433".parse().unwrap(),
            "192.168.1.1:4433".parse().unwrap(),
            "10.0.0.5:4433".parse().unwrap(),
        ];
        let target = select_dial_target(probe_src, &candidates);
        assert_eq!(target, Some(probe_src));
    }

    #[test]
    fn probe_timeout_direct_targets_preserve_announced_listener_order() {
        use std::net::SocketAddr;
        let candidates: Vec<SocketAddr> = vec![
            "192.168.0.12:4433".parse().unwrap(),
            "192.168.0.13:4434".parse().unwrap(),
            "192.168.0.12:4433".parse().unwrap(),
        ];
        let targets = probe_timeout_direct_targets(&candidates);
        assert_eq!(
            targets,
            vec![
                "192.168.0.12:4433".parse::<SocketAddr>().unwrap(),
                "192.168.0.13:4434".parse::<SocketAddr>().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn probe_timeout_direct_targets_for_ipv4_socket_skip_ipv6_targets() {
        use std::net::SocketAddr;
        let candidates: Vec<SocketAddr> = vec![
            "[2001:db8:240::44]:63718".parse().unwrap(),
            "[2001:db8:240::45]:63718".parse().unwrap(),
            "198.51.100.23:11693".parse().unwrap(),
        ];
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("bind ipv4");

        let targets = probe_timeout_direct_targets_for_socket(&sock, &candidates);

        assert_eq!(
            targets,
            vec!["198.51.100.23:11693".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn ipv4_bound_candidate_publish_drops_ipv6_host_candidates() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "2001:db8:240::44".to_string(),
                port: 63718,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "198.51.100.23".to_string(),
                port: 11693,
                kind: CandidateKind::ServerReflexive,
            },
        ];

        let filtered =
            filter_candidates_for_bind_addr(candidates, Some("0.0.0.0:51778".parse().unwrap()));

        assert_eq!(
            filtered,
            vec![Candidate {
                ip: "198.51.100.23".to_string(),
                port: 11693,
                kind: CandidateKind::ServerReflexive,
            }]
        );
    }

    #[test]
    fn ipv6_bound_candidate_publish_drops_ipv4_candidates_even_if_dual_stack() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "2001:db8:240::44".to_string(),
                port: 63718,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "198.51.100.23".to_string(),
                port: 11693,
                kind: CandidateKind::ServerReflexive,
            },
        ];

        let filtered =
            filter_candidates_for_bind_addr(candidates, Some("[::]:51778".parse().unwrap()));

        assert_eq!(
            filtered,
            vec![Candidate {
                ip: "2001:db8:240::44".to_string(),
                port: 63718,
                kind: CandidateKind::Host,
            }],
            "P2P publication must match the socket's guaranteed family even when V6ONLY=0"
        );
    }

    #[test]
    fn cooldown_grows_with_failure_count() {
        let base = Duration::from_secs(60);
        let cap = Duration::from_secs(1800);
        assert_eq!(next_cooldown(0, base, cap), base);
        assert_eq!(next_cooldown(1, base, cap), Duration::from_secs(120));
        assert_eq!(next_cooldown(2, base, cap), Duration::from_secs(240));
        assert_eq!(next_cooldown(3, base, cap), Duration::from_secs(480));
        assert_eq!(next_cooldown(4, base, cap), Duration::from_secs(960));
        assert_eq!(next_cooldown(5, base, cap), Duration::from_secs(1800));
        assert_eq!(next_cooldown(99, base, cap), Duration::from_secs(1800));
    }

    #[test]
    fn cooldown_respects_configurable_cap() {
        // `cap` must come from `ClientP2pConfig.cooldown_max_secs`,
        // not be hardcoded. With a 300 s ceiling the high-failure paths
        // saturate at 300 s instead of the previous 1800 s.
        let base = Duration::from_secs(60);
        let cap = Duration::from_secs(300);
        assert_eq!(next_cooldown(99, base, cap), Duration::from_secs(300));
        // Lower failure counts still grow normally up to the cap.
        assert_eq!(next_cooldown(2, base, cap), Duration::from_secs(240));
        assert_eq!(next_cooldown(3, base, cap), Duration::from_secs(300));
    }

    fn make_test_multi_arc() -> Arc<crate::p2p::session::MultiSession> {
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<PackedMessage>(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        let inbound: Arc<DashMap<String, tokio::sync::mpsc::Sender<Bytes>>> =
            Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        crate::p2p::session::MultiSession::new_with_existing_maps(
            Arc::new(session),
            inbound,
            udp_inbound,
        )
    }

    fn make_test_manager_with_role(
        role: crate::p2p::session::ClientRole,
        client_id: &str,
    ) -> (
        P2pManager,
        tokio::sync::mpsc::Receiver<BinaryMessage>,
        Arc<crate::p2p::session::MultiSession>,
    ) {
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mgr = P2pManager::new(
            multi.clone(),
            client_id.to_string(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            role,
            in_rx,
            out_tx,
            4433,
        );
        (mgr, out_rx, multi)
    }

    #[test]
    fn preferred_local_p2p_families_are_ipv6_first() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "9.9.9.9".into(),
                port: 40000,
                kind: CandidateKind::ServerReflexive,
            },
            Candidate {
                ip: "2606:4700:4700::1111".into(),
                port: 40001,
                kind: CandidateKind::Host,
            },
        ];

        assert_eq!(
            preferred_local_p2p_families(&candidates, false),
            vec![P2pAddressFamily::Ipv6, P2pAddressFamily::Ipv4]
        );
    }

    #[test]
    fn mixed_candidate_set_has_no_single_family() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let candidates = vec![
            Candidate {
                ip: "9.9.9.9".into(),
                port: 40000,
                kind: CandidateKind::ServerReflexive,
            },
            Candidate {
                ip: "2606:4700:4700::1111".into(),
                port: 40001,
                kind: CandidateKind::Host,
            },
        ];

        assert_eq!(candidate_set_family(&candidates), None);
    }

    #[test]
    fn local_p2p_candidates_for_ipv6_family_never_publish_ipv4() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let raw = vec![
            Candidate {
                ip: "2606:4700:4700::1111".into(),
                port: 53721,
                kind: CandidateKind::Host,
            },
            Candidate {
                ip: "9.9.9.9".into(),
                port: 53721,
                kind: CandidateKind::ServerReflexive,
            },
        ];

        let candidates =
            local_p2p_candidates_for_family(53721, P2pAddressFamily::Ipv6, None, raw, false);

        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].ip.contains(':'));
    }

    #[tokio::test]
    async fn ipv6_attempt_failure_queues_ipv4_fallback_without_cooldown() {
        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "mobile-AbC12345-0",
        );
        let sid = SessionId::new_random();
        mgr.multi
            .set_state(P2pState::Negotiating { session_id: sid });
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                peer_client_id: Some("peer-0".into()),
                local_client_id: Some("local-0".into()),
                family: Some(P2pAddressFamily::Ipv6),
                fallback_family: Some(P2pAddressFamily::Ipv4),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_initiator_attempt_failed(sid);

        assert!(matches!(mgr.multi.p2p_state(), P2pState::Idle));
        assert_eq!(mgr.failure_count, 0);
        assert_eq!(mgr.family_fallback_queue.len(), 1);
        assert_eq!(mgr.family_fallback_queue[0].family, P2pAddressFamily::Ipv4);
    }

    #[tokio::test]
    async fn active_parallel_ipv6_failure_still_queues_ipv4_fallback() {
        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "mobile-AbC12345-0",
        );
        let active_sid = SessionId::new_random();
        let failed_sid = SessionId::new_random();
        mgr.multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });
        mgr.peer_contexts.insert(
            failed_sid,
            PeerContext {
                peer_client_id: Some("peer-1".into()),
                local_client_id: Some("local-1".into()),
                family: Some(P2pAddressFamily::Ipv6),
                fallback_family: Some(P2pAddressFamily::Ipv4),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_initiator_attempt_failed(failed_sid);

        assert!(
            matches!(mgr.multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid)
        );
        assert_eq!(mgr.failure_count, 0);
        assert_eq!(mgr.family_fallback_queue.len(), 1);
        assert_eq!(mgr.family_fallback_queue[0].family, P2pAddressFamily::Ipv4);
    }

    #[tokio::test]
    async fn unanswered_ipv6_offer_queues_ipv4_fallback() {
        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "mobile-AbC12345-0",
        );
        let sid = SessionId::new_random();
        mgr.multi
            .set_state(P2pState::Negotiating { session_id: sid });
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                peer_client_id: Some("peer-0".into()),
                local_client_id: Some("local-0".into()),
                family: Some(P2pAddressFamily::Ipv6),
                fallback_family: Some(P2pAddressFamily::Ipv4),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_offer_answer_timeout(sid);

        assert!(matches!(mgr.multi.p2p_state(), P2pState::Idle));
        assert_eq!(mgr.family_fallback_queue.len(), 1);
        assert_eq!(mgr.family_fallback_queue[0].family, P2pAddressFamily::Ipv4);
    }

    #[tokio::test]
    async fn active_parallel_ipv6_offer_timeout_still_queues_ipv4_fallback() {
        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "mobile-AbC12345-0",
        );
        let active_sid = SessionId::new_random();
        let timed_out_sid = SessionId::new_random();
        mgr.multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });
        mgr.peer_contexts.insert(
            timed_out_sid,
            PeerContext {
                peer_client_id: Some("peer-1".into()),
                local_client_id: Some("local-1".into()),
                family: Some(P2pAddressFamily::Ipv6),
                fallback_family: Some(P2pAddressFamily::Ipv4),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_offer_answer_timeout(timed_out_sid);

        assert!(
            matches!(mgr.multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid)
        );
        assert_eq!(mgr.family_fallback_queue.len(), 1);
        assert_eq!(mgr.family_fallback_queue[0].family, P2pAddressFamily::Ipv4);
    }

    #[tokio::test]
    async fn bind_punch_socket_rejects_mixed_family_candidates() {
        let candidates = vec![
            "8.8.8.8:50000".parse().unwrap(),
            "[2606:4700:4700::1111]:50001".parse().unwrap(),
        ];

        let err = bind_punch_socket(&candidates)
            .await
            .expect_err("mixed family candidates must fail");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn acceptor_rejects_mixed_family_offer() {
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, _multi) =
            make_test_manager_with_role(crate::p2p::session::ClientRole::Acceptor, "pc-XyZ98765-0");
        let session_id = SessionId::new_random();

        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id,
            src_client_id: "app-0".into(),
            dst_client_id: mgr.client_id.clone(),
            candidates: vec![
                Candidate {
                    ip: "8.8.8.8".into(),
                    port: 50000,
                    kind: CandidateKind::ServerReflexive,
                },
                Candidate {
                    ip: "2606:4700:4700::1111".into(),
                    port: 50001,
                    kind: CandidateKind::Host,
                },
            ],
            src_cert_fp: CertFingerprint::zero(),
            role: P2pRole::Initiator,
        })
        .await;

        match out_rx.recv().await.expect("answer") {
            BinaryMessage::P2pAnswer { ok, reason, .. } => {
                assert!(!ok);
                assert_eq!(reason, "mixed candidate family");
            }
            other => panic!("expected P2pAnswer, got {other:?}"),
        }
    }

    fn make_test_session_arc() -> Arc<tp_transport::session::Session> {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<PackedMessage>(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        Arc::new(Session::new_channeled(
            out_tx, in_rx, peer, closer, writer, reader,
        ))
    }

    async fn spawn_mapping_probe_reflector(observed: std::net::SocketAddr) -> std::net::SocketAddr {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mapping probe reflector bind");
        let reflector = sock.local_addr().expect("reflector local addr");
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, src)) = sock.recv_from(&mut buf).await {
                let text = String::from_utf8_lossy(&buf[..n]);
                let label = text
                    .split_whitespace()
                    .find_map(|token| token.strip_prefix("label="))
                    .unwrap_or("-");
                let reply = format!(
                    "OBS label={label} via={} ip={} port={}",
                    reflector.port(),
                    observed.ip(),
                    observed.port()
                );
                let _ = sock.send_to(reply.as_bytes(), src).await;
            }
        });
        reflector
    }

    #[tokio::test]
    async fn announce_ack_does_not_overwrite_in_flight_or_active_state() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        async fn assert_preserved(initial: P2pState, label: &str) {
            let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
            let multi = make_test_multi_arc();
            multi.set_state(initial);
            let mut mgr = P2pManager::new(
                multi.clone(),
                "mobile-1".into(),
                "g1".into(),
                CertFingerprint::from_bytes([1u8; 32]),
                crate::p2p::session::ClientRole::Initiator,
                in_rx,
                out_tx,
                4433,
            );

            mgr.handle_message(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await;

            let preserved = match multi.p2p_state() {
                P2pState::Negotiating { .. } => label == "negotiating",
                P2pState::Punching { .. } => label == "punching",
                P2pState::Active { .. } => label == "active",
                _ => false,
            };
            assert!(
                preserved,
                "P2pAnnounceAck must preserve {label} state, got {:?}",
                multi.p2p_state()
            );
        }

        let sid = SessionId::from_bytes([3u8; 16]);
        assert_preserved(P2pState::Negotiating { session_id: sid }, "negotiating").await;
        assert_preserved(
            P2pState::Punching {
                session_id: sid,
                started_at: std::time::Instant::now(),
            },
            "punching",
        )
        .await;
        assert_preserved(
            P2pState::Active {
                session_id: sid,
                since: std::time::Instant::now(),
            },
            "active",
        )
        .await;
    }

    #[tokio::test]
    async fn reannounce_does_not_overwrite_in_flight_or_active_state() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        async fn assert_preserved(initial: P2pState, label: &str) {
            let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
            let multi = make_test_multi_arc();
            multi.set_state(initial);
            let mgr = P2pManager::new(
                multi.clone(),
                "mobile-1".into(),
                "g1".into(),
                CertFingerprint::from_bytes([1u8; 32]),
                crate::p2p::session::ClientRole::Initiator,
                in_rx,
                out_tx,
                4433,
            );

            mgr.announce().await.expect("announce send");

            let preserved = match multi.p2p_state() {
                P2pState::Negotiating { .. } => label == "negotiating",
                P2pState::Punching { .. } => label == "punching",
                P2pState::Active { .. } => label == "active",
                _ => false,
            };
            assert!(
                preserved,
                "periodic announce must preserve {label} state, got {:?}",
                multi.p2p_state()
            );
        }

        let sid = SessionId::from_bytes([4u8; 16]);
        assert_preserved(P2pState::Negotiating { session_id: sid }, "negotiating").await;
        assert_preserved(
            P2pState::Punching {
                session_id: sid,
                started_at: std::time::Instant::now(),
            },
            "punching",
        )
        .await;
        assert_preserved(
            P2pState::Active {
                session_id: sid,
                since: std::time::Instant::now(),
            },
            "active",
        )
        .await;
    }

    #[tokio::test]
    async fn try_install_p2p_session_skips_when_cancelled() {
        let multi = make_test_multi_arc();
        let session = make_test_session_arc();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let installed = try_install_p2p_session(&multi, session, &cancel);
        assert!(!installed, "must not install when token already cancelled");
        assert!(
            multi.p2p().is_none(),
            "multi.p2p must remain None after cancelled install attempt"
        );
    }

    #[tokio::test]
    async fn try_install_p2p_session_installs_when_active() {
        let multi = make_test_multi_arc();
        let session = make_test_session_arc();
        let cancel = CancellationToken::new();
        let installed = try_install_p2p_session(&multi, session, &cancel);
        assert!(installed, "must install when token not cancelled");
        assert!(
            multi.p2p().is_some(),
            "multi.p2p must be Some after successful install"
        );
    }

    #[tokio::test]
    async fn manager_installer_rejects_late_session_after_timeout_expiration() {
        let multi = make_test_multi_arc();
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("peer-b-AbCd0002-0", multi.clone());
        let installer = engine.attach_p2p_session_installer();
        let session_id = SessionId::from_bytes([0xB8; 16]);
        assert!(installer.reserve_for_session(
            session_id,
            Some("peer-b-AbCd0002-0"),
            Some("peer-a-AbCd0001-0"),
        ));
        assert_eq!(
            installer.expire_for_session(session_id),
            crate::p2p::installer::P2pInstallExpiration::Expired,
        );
        let session = match Arc::try_unwrap(make_test_session_arc()) {
            Ok(session) => session,
            Err(_) => panic!("test session should have one owner"),
        };

        let error = match try_install_p2p_session_with_installer(
            &installer,
            session_id,
            session,
            &CancellationToken::new(),
        )
        .await
        {
            Ok(_) => panic!("manager-owned install must require its live reservation"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("reservation"));
        assert!(!multi.has_p2p_session(session_id));
    }

    #[tokio::test]
    async fn rollback_cancelled_legacy_install_clears_slot_and_active_state() {
        let multi = make_test_multi_arc();
        let session_id = SessionId::from_bytes([7u8; 16]);
        multi.set_p2p(Some(make_test_session_arc()));
        multi.set_state(P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });

        rollback_cancelled_p2p_install(&multi, session_id, None, true);

        assert!(
            multi.p2p().is_none(),
            "legacy cancellation rollback must clear the installed P2P slot"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Idle),
            "legacy cancellation rollback must not leave state Active"
        );
    }

    #[tokio::test]
    async fn p2p_teardown_cancels_matching_active_punch() {
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([42u8; 16]);
        let token = CancellationToken::new();
        mgr.active_punch_cancel.insert(sid, token.clone());

        // Drive the Teardown handler directly.
        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: sid,
            reason: TeardownReason::FatalError,
        })
        .await;

        assert!(
            token.is_cancelled(),
            "P2pTeardown for matching session must cancel the active punch token"
        );
        assert!(
            mgr.active_punch_cancel.is_empty(),
            "active_punch_cancel slot must be cleared after Teardown"
        );
    }

    #[tokio::test]
    async fn initiator_local_failure_cleans_in_flight_state_and_cooldowns_current_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x43; 16]);
        let token = CancellationToken::new();
        mgr.active_punch_cancel.insert(sid, token.clone());
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: Vec::new(),
                cert_fp: None,
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );
        multi.set_state(P2pState::HandshakingQuic { session_id: sid });

        mgr.handle_initiator_attempt_failed(sid);

        assert!(
            token.is_cancelled(),
            "failed attempt must cancel punch task token"
        );
        assert!(
            mgr.active_punch_cancel.is_empty(),
            "failed attempt must remove active punch tracking"
        );
        assert!(
            mgr.peer_contexts.is_empty(),
            "failed attempt must remove keyed peer context so refill can retry"
        );
        assert_eq!(
            mgr.failure_count, 1,
            "failed current attempt must advance cooldown backoff"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Cooldown { .. }),
            "failed current attempt must enter cooldown"
        );
    }

    #[tokio::test]
    async fn initiator_local_failure_unreserves_pending_install() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", make_test_multi_arc());
        let installer = engine.attach_p2p_session_installer();

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(installer.clone());

        let sid = SessionId::from_bytes([0x44; 16]);
        installer.reserve_for_session(sid, None, Some("peer"));
        assert!(
            engine.has_pending_p2p_session_install_for_test(sid),
            "test setup must create a pending install reservation"
        );
        multi.set_state(P2pState::Punching {
            session_id: sid,
            started_at: std::time::Instant::now(),
        });

        mgr.handle_initiator_attempt_failed(sid);

        assert!(
            !engine.has_pending_p2p_session_install_for_test(sid),
            "manager cleanup must release pending install reservation"
        );
    }

    #[tokio::test]
    async fn parallel_initiator_punch_sync_does_not_overwrite_existing_active_session() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let active_sid = SessionId::from_bytes([0x51; 16]);
        let active_session = make_test_session_arc();
        multi.set_p2p(Some(active_session.clone()));
        multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });

        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let sidecar_sid = SessionId::from_bytes([0x52; 16]);
        mgr.peer_contexts.insert(
            sidecar_sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "127.0.0.1".into(),
                    port: 4433,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("pc-1-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
            + 60_000;

        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: sidecar_sid,
            t_start_ms: future_ms,
            burst_count: 1,
            port_offsets: vec![],
        })
        .await;

        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "parallel sidecar PunchSync must not overwrite existing Active state"
        );
        assert!(
            multi
                .p2p()
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &active_session))
                .unwrap_or(false),
            "parallel sidecar PunchSync must not clear the existing direct session"
        );

        mgr.handle_initiator_attempt_failed(sidecar_sid);

        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "sidecar failure must leave existing Active state intact"
        );
        assert!(
            multi
                .p2p()
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &active_session))
                .unwrap_or(false),
            "sidecar failure must not close an unrelated active P2P session"
        );
    }

    #[tokio::test]
    async fn initiator_malformed_candidates_cleanup_unreserves_pending_install() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", make_test_multi_arc());
        let installer = engine.attach_p2p_session_installer();

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(installer.clone());

        let sid = SessionId::from_bytes([0x53; 16]);
        installer.reserve_for_session(sid, None, Some("peer"));
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "not-an-ip-address".into(),
                    port: 4433,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );
        multi.set_state(P2pState::Negotiating { session_id: sid });

        mgr.spawn_punch_and_handshake(sid, 0, 1, vec![], true);

        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "malformed candidate pre-spawn failure must clean keyed peer context"
        );
        assert!(
            !engine.has_pending_p2p_session_install_for_test(sid),
            "malformed candidate pre-spawn failure must unreserve pending install"
        );
    }

    #[tokio::test]
    async fn initiator_punch_accepts_global_ipv6_peer_candidate() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x55; 16]);
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "2001:4860:4860::8888".into(),
                    port: 62144,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );
        multi.set_state(P2pState::Negotiating { session_id: sid });

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
            + 60_000;
        mgr.spawn_punch_and_handshake(sid, future_ms, 1, vec![], true);

        assert!(
            mgr.active_punch_cancel.contains_key(&sid),
            "global IPv6 candidates must be parsed and allowed into the punch driver"
        );
        mgr.cleanup_session_attempt(sid);
    }

    #[tokio::test]
    async fn initiator_punch_rejects_lan_peer_candidate_by_default() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x56; 16]);
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "192.168.0.104".into(),
                    port: 62144,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );
        multi.set_state(P2pState::Negotiating { session_id: sid });

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
            + 60_000;
        mgr.spawn_punch_and_handshake(sid, future_ms, 1, vec![], true);

        assert!(
            !mgr.active_punch_cancel.contains_key(&sid),
            "LAN candidates must be rejected by default so public P2P is the product path"
        );
        mgr.cleanup_session_attempt(sid);
    }

    #[tokio::test]
    async fn acceptor_probe_responder_completion_emits_cleanup_event() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("pc-1", make_test_multi_arc());
        let installer = engine.attach_p2p_session_installer();

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(installer.clone());

        let sid = SessionId::from_bytes([0x54; 16]);
        installer.reserve_for_session(sid, Some("pc-1"), Some("mobile-1"));
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "127.0.0.1".into(),
                    port: 4433,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("mobile-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.spawn_punch_responder(sid, 0, None, Vec::new());
        let event = tokio::time::timeout(Duration::from_secs(7), mgr.internal_rx.recv())
            .await
            .expect("responder should finish within cleanup timeout")
            .expect("cleanup event channel should remain open");
        mgr.handle_internal_event(event);

        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "acceptor responder completion must clean keyed peer context"
        );
        assert!(
            !engine.has_pending_p2p_session_install_for_test(sid),
            "acceptor responder completion must unreserve pending install"
        );
    }

    #[tokio::test]
    async fn acceptor_sidecar_offer_and_cleanup_preserve_existing_active_anchor() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let active_sid = SessionId::from_bytes([0x61; 16]);
        let active_session = make_test_session_arc();
        multi.set_p2p(Some(active_session.clone()));
        multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });

        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        let sidecar_sid = SessionId::from_bytes([0x62; 16]);

        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sidecar_sid,
            src_client_id: "mobile-1-1".into(),
            dst_client_id: "pc-1-1".into(),
            candidates: vec![Candidate {
                ip: "127.0.0.1".into(),
                port: 4433,
                kind: CandidateKind::Host,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;
        let _answer = out_rx.recv().await.expect("P2pAnswer should be sent");

        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "sidecar offer must not overwrite existing Active anchor state"
        );

        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: sidecar_sid,
            t_start_ms: 0,
            burst_count: 1,
            port_offsets: vec![],
        })
        .await;

        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "sidecar PunchSync must not overwrite existing Active anchor state"
        );

        mgr.handle_session_attempt_cleanup(sidecar_sid);

        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "sidecar cleanup must preserve existing Active anchor state"
        );
        assert!(
            multi
                .p2p()
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &active_session))
                .unwrap_or(false),
            "sidecar cleanup must not close the existing anchor P2P session"
        );
    }

    #[tokio::test]
    async fn p2p_teardown_clears_matching_replica_sidecar_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let anchor_multi = make_test_multi_arc();
        let replica_multi = make_test_multi_arc();
        let replica_session = make_test_session_arc();
        let sidecar_sid = SessionId::from_bytes([0x63; 16]);
        replica_multi.set_state(P2pState::Active {
            session_id: sidecar_sid,
            since: std::time::Instant::now(),
        });
        replica_multi.set_p2p(Some(replica_session));

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("pc-1", anchor_multi.clone());
        engine.install_proxy_replica_session_for_test("pc-1-1", replica_multi.clone());

        let mut mgr = P2pManager::new(
            anchor_multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());

        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: sidecar_sid,
            reason: TeardownReason::FatalError,
        })
        .await;

        assert!(
            replica_multi.p2p().is_none(),
            "teardown must clear matching replica sidecar P2P slot"
        );
        assert!(
            matches!(replica_multi.p2p_state(), P2pState::Idle),
            "teardown must return matching replica sidecar state to Idle"
        );
        assert!(
            anchor_multi.p2p().is_none(),
            "replica teardown must not install or clear an unrelated anchor slot"
        );
        assert_eq!(
            mgr.failure_count, 0,
            "replica sidecar teardown must not cooldown the anchor manager"
        );
    }

    #[tokio::test]
    async fn current_teardown_closes_only_its_exact_peer_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let peer_b = make_test_session_arc();
        let peer_c = make_test_session_arc();
        let sid_b = SessionId::from_bytes([0x64; 16]);
        let sid_c = SessionId::from_bytes([0x65; 16]);
        multi
            .install_p2p_session(sid_b, "peer-b".into(), peer_b.clone())
            .expect("install Peer B Direct");
        multi
            .install_p2p_session(sid_c, "peer-c".into(), peer_c)
            .expect("install Peer C Direct");
        multi.set_state(P2pState::Active {
            session_id: sid_c,
            since: std::time::Instant::now(),
        });

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("local-runtime", multi.clone());
        let mut mgr = P2pManager::new(
            multi.clone(),
            "local-runtime".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());

        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: sid_c,
            reason: TeardownReason::FatalError,
        })
        .await;

        assert!(!multi.has_p2p_session(sid_c));
        assert!(multi.has_p2p_session(sid_b));
        assert_eq!(multi.p2p_session_count(), 1);
        assert!(
            multi
                .p2p()
                .is_some_and(|remaining| Arc::ptr_eq(&remaining, &peer_b)),
            "Peer C teardown must preserve Peer B's healthy Direct Lane"
        );
    }

    #[tokio::test]
    async fn report_initiator_attempt_failed_emits_local_cleanup_event_and_remote_teardown() {
        use tp_core::p2p_types::{SessionId, TeardownReason};

        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(1);
        let sid = SessionId::from_bytes([0x45; 16]);

        report_initiator_attempt_failed(&internal_tx, &out_tx, sid).await;

        match internal_rx.recv().await {
            Some(P2pInternalEvent::InitiatorAttemptFailed { session_id }) => {
                assert_eq!(session_id, sid);
            }
            other => panic!("expected local cleanup event, got {other:?}"),
        }
        match out_rx.recv().await {
            Some(BinaryMessage::P2pTeardown { session_id, reason }) => {
                assert_eq!(session_id, sid);
                assert_eq!(reason, TeardownReason::FatalError);
            }
            other => panic!("expected remote teardown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2p_teardown_advances_failure_count_for_exponential_cooldown() {
        // Each P2pTeardown must bump `failure_count` and stamp a
        // cooldown that grows exponentially (60s → 120s → 240s → …).
        // Pre-fix the handler stamped a fixed 60s cooldown forever; spec
        // The metric must grow so a chronically broken NAT path doesn't
        // hammer signaling.
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        for expected_count in 1..=3u32 {
            let session_id = SessionId::from_bytes([expected_count as u8; 16]);
            multi.set_state(P2pState::Negotiating { session_id });
            mgr.handle_message(BinaryMessage::P2pTeardown {
                session_id,
                reason: TeardownReason::FatalError,
            })
            .await;
            assert_eq!(
                mgr.failure_count, expected_count,
                "Teardown #{expected_count} must advance failure_count"
            );
        }
    }

    #[tokio::test]
    async fn set_cooldown_config_flows_into_teardown_cooldown_stamp() {
        // Exercises `set_cooldown_config` end-to-end.
        // Confirms a non-default `cooldown_initial_secs` lands in the
        // `Cooldown { until }` stamped by `P2pTeardown`, AND that
        // `cooldown_max_secs` clamps later doublings. Pre-fix, the call
        // sites embedded literal cooldown values; this
        // test would catch a regression that re-hardcoded those values.
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_cooldown_config(2, 5);

        let first_sid = SessionId::from_bytes([1u8; 16]);
        multi.set_state(P2pState::Negotiating {
            session_id: first_sid,
        });
        let before = std::time::Instant::now();
        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: first_sid,
            reason: TeardownReason::FatalError,
        })
        .await;
        let until_first = match multi.p2p_state() {
            crate::p2p::session::P2pState::Cooldown { until } => until,
            other => panic!("expected Cooldown after first Teardown, got {other:?}"),
        };
        let dur_first = until_first.saturating_duration_since(before);
        assert!(
            dur_first >= Duration::from_millis(1900) && dur_first <= Duration::from_millis(2200),
            "first Teardown must stamp ~2s cooldown (configured initial), got {dur_first:?}"
        );

        // Drive enough additional Teardowns that the doubling would
        // exceed the configured 5s cap if uncapped (2 → 4 → 8 → 16 → …).
        for n in 2..=6u32 {
            let session_id = SessionId::from_bytes([n as u8; 16]);
            multi.set_state(P2pState::Negotiating { session_id });
            mgr.handle_message(BinaryMessage::P2pTeardown {
                session_id,
                reason: TeardownReason::FatalError,
            })
            .await;
        }
        let now = std::time::Instant::now();
        let until_capped = match multi.p2p_state() {
            crate::p2p::session::P2pState::Cooldown { until } => until,
            other => panic!("expected Cooldown after capped Teardown, got {other:?}"),
        };
        let dur_capped = until_capped.saturating_duration_since(now);
        assert!(
            dur_capped <= Duration::from_millis(5200),
            "configured cooldown cap (5s) must clamp doublings, got {dur_capped:?}"
        );
    }

    #[tokio::test]
    async fn p2p_answer_rejected_increments_nat_fail_metric() {
        // When the remote returns `P2pAnswer { ok: false }`,
        // `fail_and_cooldown` must emit exactly ONE `p2p_attempts_total`
        // increment, tagged with the caller-supplied reason (today
        // `NatFail` — closest bucket for a remote rejection). Earlier
        // the bucket was hardcoded inside `fail_and_cooldown`; this test
        // pins the explicit-reason path against regressions that would
        // route the rejection through some other bucket. We assert via
        // `prometheus_text()` because the raw atomics aren't pub.
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_metrics::MetricsManager;

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let metrics = MetricsManager::new();
        mgr.set_metrics(Some(metrics.clone()));

        let sid = SessionId::from_bytes([1u8; 16]);
        mgr.peer_contexts.insert(sid, PeerContext::default());
        multi.set_state(P2pState::Negotiating { session_id: sid });
        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid,
            accepted_client_id: "pc-1".into(),
            ok: false,
            reason: "remote rejected".into(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::from_bytes([0u8; 32]),
        })
        .await;

        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_attempts_total{result=\"nat_fail\"} 1"),
            "rejected P2pAnswer must bump nat_fail exactly once:\n{text}"
        );
        // Separation-of-concerns: the spawn-task-only buckets must NOT
        // be touched by the non-spawn-task `fail_and_cooldown` path.
        assert!(
            text.contains("p2p_attempts_total{result=\"timeout\"} 0"),
            "rejected P2pAnswer must not bump timeout bucket:\n{text}"
        );
        assert!(
            text.contains("p2p_attempts_total{result=\"cert_fail\"} 0"),
            "rejected P2pAnswer must not bump cert_fail bucket:\n{text}"
        );
    }

    #[tokio::test]
    async fn reserved_rejected_p2p_answer_cleans_pending_install_and_cooldowns() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_metrics::MetricsManager;

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", make_test_multi_arc());
        let installer = engine.attach_p2p_session_installer();

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let metrics = MetricsManager::new();
        mgr.set_metrics(Some(metrics.clone()));
        mgr.set_session_installer(installer.clone());

        let sid = SessionId::from_bytes([0x93; 16]);
        installer.reserve_for_session(sid, Some("mobile-1"), Some("pc-1"));
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![],
                cert_fp: None,
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid,
            accepted_client_id: "pc-abc12345-1".into(),
            ok: false,
            reason: "remote rejected".into(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::from_bytes([0u8; 32]),
        })
        .await;

        assert!(
            !engine.has_pending_p2p_session_install_for_test(sid),
            "reserved rejected Answer must clear pending install"
        );
        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "reserved rejected Answer must clear keyed context"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Cooldown { .. }),
            "reserved rejected Answer must cooldown the attempt"
        );
        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_attempts_total{result=\"nat_fail\"} 1"),
            "reserved rejected Answer must count as nat_fail:\n{text}"
        );
    }

    #[tokio::test]
    async fn reserved_rejected_p2p_answer_preserves_existing_active_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_metrics::MetricsManager;

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        let multi = make_test_multi_arc();
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());
        let installer = engine.attach_p2p_session_installer();

        let active_sid = SessionId::from_bytes([0x71; 16]);
        let active_session = make_test_session_arc();
        multi.set_p2p(Some(active_session.clone()));
        multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let metrics = MetricsManager::new();
        mgr.set_metrics(Some(metrics.clone()));
        mgr.set_session_installer(installer.clone());

        let pending_sid = SessionId::from_bytes([0x72; 16]);
        installer.reserve_for_session(pending_sid, Some("mobile-1"), Some("pc-1"));
        mgr.peer_contexts.insert(
            pending_sid,
            PeerContext {
                candidates: vec![],
                cert_fp: None,
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: pending_sid,
            accepted_client_id: "pc-abc12345-1".into(),
            ok: false,
            reason: "remote rejected".into(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::from_bytes([0u8; 32]),
        })
        .await;

        assert!(
            !engine.has_pending_p2p_session_install_for_test(pending_sid),
            "rejected sidecar Answer must clear pending install"
        );
        assert!(
            !mgr.peer_contexts.contains_key(&pending_sid),
            "rejected sidecar Answer must clear keyed context"
        );
        assert!(
            multi
                .p2p()
                .as_ref()
                .map(|current| Arc::ptr_eq(current, &active_session))
                .unwrap_or(false),
            "rejected sidecar Answer must not close the existing active P2P session"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "rejected sidecar Answer must preserve Active({active_sid:?}), got {:?}",
            multi.p2p_state()
        );
        assert_eq!(
            mgr.failure_count, 0,
            "rejected sidecar Answer must not apply global cooldown"
        );
        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_attempts_total{result=\"nat_fail\"} 1"),
            "rejected sidecar Answer must still count as nat_fail:\n{text}"
        );
    }

    #[tokio::test]
    async fn active_parallel_rejected_ipv6_answer_queues_ipv4_fallback() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_metrics::MetricsManager;

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        let multi = make_test_multi_arc();
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());
        let installer = engine.attach_p2p_session_installer();

        let active_sid = SessionId::from_bytes([0x81; 16]);
        multi.set_p2p(Some(make_test_session_arc()));
        multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let metrics = MetricsManager::new();
        mgr.set_metrics(Some(metrics.clone()));
        mgr.set_session_installer(installer.clone());

        let pending_sid = SessionId::from_bytes([0x82; 16]);
        installer.reserve_for_session(pending_sid, Some("mobile-1"), Some("pc-1"));
        mgr.peer_contexts.insert(
            pending_sid,
            PeerContext {
                candidates: vec![],
                cert_fp: None,
                peer_client_id: Some("pc-1".into()),
                local_client_id: Some("mobile-1".into()),
                family: Some(P2pAddressFamily::Ipv6),
                fallback_family: Some(P2pAddressFamily::Ipv4),
                allow_parallel: true,
                session_role: Some(ClientRole::Initiator),
                mesh_relation_key: None,
            },
        );

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: pending_sid,
            accepted_client_id: "pc-abc12345-1".into(),
            ok: false,
            reason: "peer offline".into(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::from_bytes([0u8; 32]),
        })
        .await;

        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "rejected sidecar Answer must preserve Active({active_sid:?}), got {:?}",
            multi.p2p_state()
        );
        assert_eq!(
            mgr.family_fallback_queue.len(),
            1,
            "rejected IPv6 sidecar Answer must enqueue IPv4 fallback"
        );
        assert_eq!(mgr.family_fallback_queue[0].family, P2pAddressFamily::Ipv4);
        assert_eq!(mgr.family_fallback_queue[0].peer_client_id, "pc-1");
        assert_eq!(
            mgr.family_fallback_queue[0].local_client_id.as_deref(),
            Some("mobile-1")
        );
        assert_eq!(
            mgr.failure_count, 0,
            "sidecar fallback scheduling must not apply global cooldown"
        );
        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_attempts_total{result=\"nat_fail\"} 1"),
            "rejected sidecar Answer must still count as nat_fail:\n{text}"
        );
    }

    #[tokio::test]
    async fn p2p_punch_sync_acceptor_writes_session_id_handle() {
        // Legacy read-back hook: keep proving stale SessionIds do not
        // overwrite each other while production listener routing uses the
        // keyed expected-peer map.
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        let handle: Arc<Mutex<Option<SessionId>>> = Arc::new(Mutex::new(None));
        mgr.set_expected_session_id_handle(handle.clone());

        let sid = SessionId::from_bytes([7u8; 16]);
        mgr.multi
            .set_state(P2pState::Negotiating { session_id: sid });
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "127.0.0.1".into(),
                    port: 4433,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("mobile-1".into()),
                local_client_id: None,
                allow_parallel: false,
                session_role: Some(ClientRole::Acceptor),
                ..PeerContext::default()
            },
        );
        // Drive PunchSync directly. `t_start_ms = 0` means the spawned
        // responder won't sleep before binding; the assert below runs
        // synchronously after `handle_message` returns and only checks
        // the handle write, not the spawned task's progress.
        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: sid,
            t_start_ms: 0,
            burst_count: 0,
            port_offsets: vec![],
        })
        .await;

        let stamped = *handle.lock().expect("lock");
        assert_eq!(
            stamped,
            Some(sid),
            "Acceptor PunchSync arm must stamp the negotiated session_id into the listener handle"
        );
    }

    #[tokio::test]
    async fn p2p_punch_sync_initiator_does_not_write_session_id_handle() {
        // The Initiator branch never accepts incoming P2P
        // connections, so the listener handle is irrelevant on that
        // side and must remain untouched.
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let handle: Arc<Mutex<Option<SessionId>>> = Arc::new(Mutex::new(None));
        mgr.set_expected_session_id_handle(handle.clone());

        let sid = SessionId::from_bytes([9u8; 16]);
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "1.1.1.1".into(),
                    port: 4433,
                    kind: CandidateKind::ServerReflexive,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("pc-1".into()),
                local_client_id: Some("mobile-1".into()),
                allow_parallel: true,
                family: Some(P2pAddressFamily::Ipv4),
                session_role: Some(ClientRole::Initiator),
                ..PeerContext::default()
            },
        );
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64
            + 60_000;
        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: sid,
            t_start_ms: future_ms,
            burst_count: 1,
            port_offsets: vec![0],
        })
        .await;

        let stamped = *handle.lock().expect("lock");
        assert_eq!(
            stamped, None,
            "Initiator PunchSync arm must NOT touch the listener session_id handle"
        );
        mgr.cleanup_session_attempt(sid);
    }

    #[tokio::test]
    async fn outgoing_and_incoming_sessions_use_independent_punch_roles() {
        use tp_core::p2p_types::{Candidate, CandidateKind, P2pRole};

        let (mut mgr, mut out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "peer-a-AbCd0001-0",
        );
        mgr.set_allow_lan_candidates(true);
        mgr.set_mapping_probe_reflector_for_test(None);
        let accepted_session: Arc<Mutex<Option<SessionId>>> = Arc::new(Mutex::new(None));
        mgr.set_expected_session_id_handle(accepted_session.clone());

        mgr.try_initiate_for_local_slot_family(
            "peer-b-AbCd0002-0",
            None,
            P2pAddressFamily::Ipv4,
            None,
        )
        .await
        .expect("outgoing B offer");
        let outgoing_b = match out_rx.recv().await.expect("outgoing B offer message") {
            BinaryMessage::P2pOffer {
                session_id,
                dst_client_id,
                ..
            } => {
                assert_eq!(dst_client_id, "peer-b-AbCd0002-0");
                session_id
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        };
        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: outgoing_b,
            accepted_client_id: "peer-b-AbCd0002-0".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "1.1.1.1".into(),
                port: 41001,
                kind: CandidateKind::ServerReflexive,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([0xB2; 32]),
        })
        .await;

        let incoming_c = SessionId::from_bytes([0xC2; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: incoming_c,
            src_client_id: "peer-c-AbCd0003-0".into(),
            dst_client_id: "peer-a-AbCd0001-0".into(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 41002,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([0xC3; 32]),
            role: P2pRole::Initiator,
        })
        .await;
        assert!(matches!(
            out_rx.recv().await.expect("incoming C answer"),
            BinaryMessage::P2pAnswer {
                session_id,
                ok: true,
                ..
            } if session_id == incoming_c
        ));
        assert!(mgr.peer_contexts.contains_key(&outgoing_b));
        assert!(mgr.peer_contexts.contains_key(&incoming_c));

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64
            + 60_000;
        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: outgoing_b,
            t_start_ms: future_ms,
            burst_count: 0,
            port_offsets: vec![],
        })
        .await;
        assert_eq!(
            *accepted_session.lock().expect("accepted session lock"),
            None,
            "outgoing B must take the Initiator punch branch"
        );

        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: incoming_c,
            t_start_ms: future_ms,
            burst_count: 0,
            port_offsets: vec![],
        })
        .await;
        assert_eq!(
            *accepted_session.lock().expect("accepted session lock"),
            Some(incoming_c),
            "incoming C must take the Acceptor punch branch even though the manager's legacy role is Initiator"
        );

        mgr.cleanup_session_attempt(outgoing_b);
        mgr.cleanup_session_attempt(incoming_c);
    }

    #[tokio::test]
    async fn p2p_offer_with_wrong_dst_client_id_is_rejected() {
        // Defense-in-depth. If a P2pOffer arrives at the acceptor
        // with `dst_client_id` not matching `self.client_id` (e.g. via a
        // future gateway-routing bug), reject with `P2pAnswer { ok=false,
        // reason="wrong dst" }` and DO NOT cache peer state. Pre-fix the
        // acceptor relied entirely on gateway routing and
        // would silently start a punch with the wrong peer if the gateway
        // misrouted.
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([5u8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-7".into(),
            dst_client_id: "someone-else".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "127.0.0.1".into(),
                port: 4433,
                kind: CandidateKind::Host,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        let reply = out_rx.try_recv().expect("acceptor must reply");
        match reply {
            BinaryMessage::P2pAnswer {
                session_id,
                ok,
                reason,
                ..
            } => {
                assert_eq!(session_id, sid);
                assert!(!ok, "wrong-dst offer must be rejected");
                assert_eq!(reason, "wrong dst");
            }
            other => panic!("expected P2pAnswer rejection, got {other:?}"),
        }

        // Acceptor state must NOT have been polluted by the misrouted offer.
        assert!(
            mgr.peer_candidates_cache.is_empty(),
            "wrong-dst offer must not poison peer_candidates_cache"
        );
        assert!(
            mgr.peer_cert_fp_cache.is_none(),
            "wrong-dst offer must not poison peer_cert_fp_cache"
        );
        assert!(
            mgr.peer_client_id_cache.is_none(),
            "wrong-dst offer must not poison peer_client_id_cache"
        );
    }

    #[tokio::test]
    async fn p2p_offer_with_matching_dst_client_id_is_accepted() {
        // Control case: matching dst_client_id flows through to the
        // existing accept path and emits `P2pAnswer { ok=true }`. Guards
        // against an over-zealous check that rejects valid offers.
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([6u8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-7".into(),
            dst_client_id: "pc-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "1.1.1.1".into(),
                port: 4433,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        let reply = out_rx.try_recv().expect("acceptor must reply");
        match reply {
            BinaryMessage::P2pAnswer { ok, .. } => {
                assert!(ok, "matching dst_client_id must be accepted (control case)");
            }
            other => panic!("expected P2pAnswer acceptance, got {other:?}"),
        }
        assert_eq!(
            mgr.peer_client_id_cache.as_deref(),
            Some("mobile-7"),
            "matching dst_client_id must populate peer cache"
        );
    }

    #[tokio::test]
    async fn p2p_answer_uses_reserved_punch_socket_port_for_public_candidate() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        mgr.observed_public_addr = Some("8.8.8.8:62000".parse().unwrap());

        let sid = SessionId::from_bytes([0x8Bu8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-7".into(),
            dst_client_id: "pc-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "1.1.1.1".into(),
                port: 50001,
                kind: CandidateKind::Host,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        let reply = out_rx.try_recv().expect("acceptor must reply");
        let answer_port = match reply {
            BinaryMessage::P2pAnswer { ok, candidates, .. } => {
                assert!(ok, "matching offer must be accepted");
                candidates
                    .iter()
                    .find(|c| c.ip == "8.8.8.8")
                    .expect("answer should synthesize a public candidate from observed IP")
                    .port
            }
            other => panic!("expected P2pAnswer acceptance, got {other:?}"),
        };
        assert_ne!(
            answer_port, 4433,
            "answer must not publish the long-lived listener port for same-socket punching"
        );
    }

    #[tokio::test]
    async fn p2p_offer_uses_mapping_probe_public_port_for_listener_socket() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint};

        let observed = "8.8.8.8:3078".parse().unwrap();
        let reflector = spawn_mapping_probe_reflector(observed).await;
        let listener_sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        listener_sock.set_nonblocking(true).unwrap();
        let p2p_local_port = listener_sock.local_addr().unwrap().port();

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            p2p_local_port,
        );
        mgr.set_listener_probe_socket_for_test(listener_sock);
        mgr.set_mapping_probe_reflector_for_test(Some(reflector));
        mgr.set_mapping_probe_timeout_for_test(Duration::from_millis(500));

        mgr.try_initiate("pc-1").await.expect("offer should send");

        let offer = out_rx.recv().await.expect("P2pOffer should be sent");
        match offer {
            BinaryMessage::P2pOffer { candidates, .. } => {
                assert!(candidates.iter().any(|candidate| {
                    candidate.ip == "8.8.8.8"
                        && candidate.port == 3078
                        && candidate.kind == CandidateKind::ServerReflexive
                }));
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2p_offer_uses_cached_listener_mapping_probe_public_port() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint};

        let observed = "8.8.8.8:4078".parse().unwrap();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            40001,
        );
        mgr.set_listener_observed_public_addr_for_test(Some(observed));

        mgr.try_initiate("pc-1").await.expect("offer should send");

        let offer = out_rx.recv().await.expect("P2pOffer should be sent");
        match offer {
            BinaryMessage::P2pOffer { candidates, .. } => {
                assert!(candidates.iter().any(|candidate| {
                    candidate.ip == "8.8.8.8"
                        && candidate.port == 4078
                        && candidate.kind == CandidateKind::ServerReflexive
                }));
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2p_answer_uses_mapping_probe_public_port_for_reserved_socket() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};

        let observed = "8.8.8.8:3287".parse().unwrap();
        let reflector = spawn_mapping_probe_reflector(observed).await;
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_mapping_probe_reflector_for_test(Some(reflector));
        mgr.set_mapping_probe_timeout_for_test(Duration::from_millis(500));

        let sid = SessionId::from_bytes([0x8Cu8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-7".into(),
            dst_client_id: "pc-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "1.1.1.1".into(),
                port: 50001,
                kind: CandidateKind::Host,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        let reply = out_rx.try_recv().expect("acceptor must reply");
        match reply {
            BinaryMessage::P2pAnswer { ok, candidates, .. } => {
                assert!(ok, "matching offer must be accepted");
                assert!(candidates.iter().any(|candidate| {
                    candidate.ip == "8.8.8.8"
                        && candidate.port == 3287
                        && candidate.kind == CandidateKind::ServerReflexive
                }));
            }
            other => panic!("expected P2pAnswer acceptance, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2p_offer_for_local_numeric_suffix_family_id_is_rejected() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x8Au8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-1".into(),
            dst_client_id: "pc-1-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "127.0.0.1".into(),
                port: 4433,
                kind: CandidateKind::Host,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        let reply = out_rx.try_recv().expect("acceptor must reply");
        match reply {
            BinaryMessage::P2pAnswer { ok, reason, .. } => {
                assert!(
                    !ok,
                    "bare numeric suffix dst_client_id must not be accepted as a local replica"
                );
                assert_eq!(reason, "wrong dst");
            }
            other => panic!("expected P2pAnswer rejection, got {other:?}"),
        }
        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "rejected numeric-suffix offer must not create a keyed context"
        );
    }

    #[tokio::test]
    async fn p2p_offer_for_local_new_replica_family_id_is_accepted() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};
        let (tx, mut rx) = mpsc::channel(8);
        let (_in_tx, in_rx) = mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Idle);
        let mut mgr = P2pManager::new(
            multi,
            "seed-7Neb0000-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            tx,
            34567,
        );
        let sid = SessionId::from_bytes([0x5au8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-1".into(),
            dst_client_id: "seed-7Neb0000-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "1.1.1.1".into(),
                port: 12345,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        match rx.recv().await.expect("answer") {
            BinaryMessage::P2pAnswer { ok, .. } => {
                assert!(
                    ok,
                    "local new replica-family dst_client_id must be accepted"
                );
            }
            other => panic!("expected P2pAnswer acceptance, got {other:?}"),
        }
        assert!(
            mgr.peer_contexts.contains_key(&sid),
            "accepted -rN replica offer must create a keyed context"
        );
    }

    #[tokio::test]
    async fn p2p_offer_for_local_new_replica_family_id_is_accepted_from_sidecar_anchor() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};
        let (tx, mut rx) = mpsc::channel(8);
        let (_in_tx, in_rx) = mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Idle);
        let mut mgr = P2pManager::new(
            multi,
            "seed-7Neb0000-4".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            tx,
            34567,
        );
        let sid = SessionId::from_bytes([0x5cu8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-1".into(),
            dst_client_id: "seed-7Neb0000-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "1.1.1.1".into(),
                port: 12345,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        match rx.recv().await.expect("answer") {
            BinaryMessage::P2pAnswer { ok, .. } => {
                assert!(
                    ok,
                    "sidecar P2P anchor must accept other local replicas in the same family"
                );
            }
            other => panic!("expected P2pAnswer acceptance, got {other:?}"),
        }
        assert!(
            mgr.peer_contexts.contains_key(&sid),
            "accepted same-family offer must create a keyed context"
        );
    }

    #[tokio::test]
    async fn p2p_offer_with_only_lan_peer_candidate_is_rejected_when_lan_p2p_is_disabled() {
        use tp_core::p2p_types::{CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x5bu8; 16]);
        mgr.handle_message(BinaryMessage::P2pOffer {
            session_id: sid,
            src_client_id: "mobile-1".into(),
            dst_client_id: "pc-1".into(),
            candidates: vec![tp_core::p2p_types::Candidate {
                ip: "192.168.1.44".into(),
                port: 12345,
                kind: CandidateKind::Host,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        })
        .await;

        match out_rx.recv().await.expect("answer") {
            BinaryMessage::P2pAnswer { ok, reason, .. } => {
                assert!(!ok, "LAN-only peer offer must be rejected by default");
                assert_eq!(reason, "no usable peer candidates");
            }
            other => panic!("expected P2pAnswer rejection, got {other:?}"),
        }
        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "rejected LAN-only offer must not create a keyed context"
        );
        assert!(
            mgr.peer_candidates_cache.is_empty(),
            "rejected LAN-only offer must not poison peer candidate cache"
        );
    }

    #[tokio::test]
    async fn p2p_offers_keep_expected_and_context_per_session() {
        use crate::p2p::expected::ExpectedPeerMap;
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        let expected = ExpectedPeerMap::default();
        mgr.set_expected_peer_map(expected.clone());

        let sid_a = SessionId::from_bytes([10u8; 16]);
        let sid_b = SessionId::from_bytes([11u8; 16]);
        let fp_a = CertFingerprint::from_bytes([20u8; 32]);
        let fp_b = CertFingerprint::from_bytes([21u8; 32]);
        let cand_a = Candidate {
            ip: "1.1.1.1".into(),
            port: 1001,
            kind: CandidateKind::ServerReflexive,
        };
        let cand_b = Candidate {
            ip: "8.8.8.8".into(),
            port: 1002,
            kind: CandidateKind::ServerReflexive,
        };

        for (session_id, src_client_id, candidates, src_cert_fp) in [
            (sid_a, "mobile-a", vec![cand_a.clone()], fp_a),
            (sid_b, "mobile-b", vec![cand_b.clone()], fp_b),
        ] {
            mgr.handle_message(BinaryMessage::P2pOffer {
                session_id,
                src_client_id: src_client_id.into(),
                dst_client_id: "pc-1".into(),
                candidates,
                src_cert_fp,
                role: P2pRole::Initiator,
            })
            .await;
            let _ = out_rx.try_recv().expect("acceptor must reply");
        }

        assert_eq!(
            mgr.peer_contexts.get(&sid_a).unwrap().candidates,
            vec![cand_a]
        );
        assert_eq!(
            mgr.peer_contexts.get(&sid_b).unwrap().candidates,
            vec![cand_b]
        );
        assert_eq!(expected.get(sid_a).unwrap().cert_fp, fp_a);
        assert_eq!(expected.get(sid_b).unwrap().cert_fp, fp_b);
    }

    #[tokio::test]
    async fn p2p_answers_update_matching_session_context_out_of_order() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());
        let installer = engine.attach_p2p_session_installer();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(installer.clone());

        let sid_a = SessionId::from_bytes([30u8; 16]);
        let sid_b = SessionId::from_bytes([31u8; 16]);
        installer.reserve_for_session(sid_a, Some("mobile-1"), Some("pc-a"));
        installer.reserve_for_session(sid_b, Some("mobile-1"), Some("pc-b"));
        mgr.peer_contexts.insert(sid_a, PeerContext::default());
        mgr.peer_contexts.insert(sid_b, PeerContext::default());

        let cand_a = Candidate {
            ip: "1.1.1.1".into(),
            port: 3001,
            kind: CandidateKind::ServerReflexive,
        };
        let cand_b = Candidate {
            ip: "8.8.8.8".into(),
            port: 3002,
            kind: CandidateKind::ServerReflexive,
        };
        let fp_a = CertFingerprint::from_bytes([40u8; 32]);
        let fp_b = CertFingerprint::from_bytes([41u8; 32]);

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid_b,
            accepted_client_id: "pc-b".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![cand_b.clone()],
            dst_cert_fp: fp_b,
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid_a,
            accepted_client_id: "pc-a".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![cand_a.clone()],
            dst_cert_fp: fp_a,
        })
        .await;

        assert_eq!(mgr.peer_contexts.get(&sid_a).unwrap().cert_fp, Some(fp_a));
        assert_eq!(
            mgr.peer_contexts.get(&sid_a).unwrap().candidates,
            vec![cand_a]
        );
        assert_eq!(mgr.peer_contexts.get(&sid_b).unwrap().cert_fp, Some(fp_b));
        assert_eq!(
            mgr.peer_contexts.get(&sid_b).unwrap().candidates,
            vec![cand_b]
        );
    }

    #[tokio::test]
    async fn p2p_answer_filters_lan_candidates_when_lan_p2p_is_disabled() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x5cu8; 16]);
        multi.set_state(P2pState::Negotiating { session_id: sid });
        mgr.peer_contexts.insert(sid, PeerContext::default());
        let public_candidate = Candidate {
            ip: "1.1.1.1".into(),
            port: 3001,
            kind: CandidateKind::ServerReflexive,
        };

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid,
            accepted_client_id: "pc-1".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![
                Candidate {
                    ip: "192.168.1.44".into(),
                    port: 3000,
                    kind: CandidateKind::Host,
                },
                public_candidate.clone(),
            ],
            dst_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
        })
        .await;

        assert_eq!(mgr.peer_candidates_cache, vec![public_candidate.clone()]);
        assert_eq!(
            mgr.peer_contexts.get(&sid).unwrap().candidates,
            vec![public_candidate]
        );
    }

    #[tokio::test]
    async fn p2p_answer_with_only_lan_candidate_cooldowns_when_lan_p2p_is_disabled() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid = SessionId::from_bytes([0x5du8; 16]);
        multi.set_state(P2pState::Negotiating { session_id: sid });
        mgr.peer_contexts.insert(sid, PeerContext::default());

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid,
            accepted_client_id: "pc-1".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "192.168.1.44".into(),
                port: 3000,
                kind: CandidateKind::Host,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
        })
        .await;

        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "LAN-only Answer must clean up the in-flight context by default"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Cooldown { .. }),
            "LAN-only Answer must fail the current P2P attempt quickly"
        );
    }

    #[tokio::test]
    async fn p2p_answer_uses_accepted_client_id_for_pending_install_peer() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", make_test_multi_arc());
        let installer = engine.attach_p2p_session_installer();

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(installer.clone());

        let sid = SessionId::from_bytes([0x91; 16]);
        installer.reserve_for_session(sid, Some("mobile-1"), Some("pc-1"));
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![],
                cert_fp: None,
                peer_client_id: Some("pc-1".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid,
            accepted_client_id: "pc-abc12345-1".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 40000,
                kind: CandidateKind::ServerReflexive,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
        })
        .await;

        let pending_peer = engine
            .pending_p2p_peer_client_id_for_test(sid)
            .expect("pending install must still exist");
        assert_eq!(pending_peer, "pc-abc12345-1");
        assert_eq!(
            mgr.peer_contexts
                .get(&sid)
                .unwrap()
                .peer_client_id
                .as_deref(),
            Some("pc-abc12345-1")
        );
        assert_eq!(mgr.peer_client_id_cache.as_deref(), Some("pc-abc12345-1"));
    }

    #[tokio::test]
    async fn stale_ok_p2p_answer_does_not_update_peer_identity() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        let installer = engine.attach_p2p_session_installer();

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(installer);

        let sid = SessionId::from_bytes([0x92; 16]);
        mgr.peer_client_id_cache = Some("pc-old".into());
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                candidates: vec![],
                cert_fp: None,
                peer_client_id: Some("pc-old".into()),
                local_client_id: None,
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid,
            accepted_client_id: "pc-new".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 40001,
                kind: CandidateKind::ServerReflexive,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
        })
        .await;

        assert_eq!(mgr.peer_client_id_cache.as_deref(), Some("pc-old"));
        assert!(
            !mgr.peer_contexts.contains_key(&sid),
            "stale ok Answer must clean up stale context without applying accepted_client_id"
        );
        assert!(
            engine.pending_p2p_peer_client_id_for_test(sid).is_none(),
            "stale ok Answer must not create or update pending install peer id"
        );
    }

    #[tokio::test]
    async fn p2p_teardown_cancels_only_matching_session_token() {
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let sid_a = SessionId::from_bytes([50u8; 16]);
        let sid_b = SessionId::from_bytes([51u8; 16]);
        let token_a = CancellationToken::new();
        let token_b = CancellationToken::new();
        mgr.active_punch_cancel.insert(sid_a, token_a.clone());
        mgr.active_punch_cancel.insert(sid_b, token_b.clone());

        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: sid_a,
            reason: TeardownReason::FatalError,
        })
        .await;

        assert!(token_a.is_cancelled());
        assert!(!token_b.is_cancelled());
        assert!(!mgr.active_punch_cancel.contains_key(&sid_a));
        assert!(mgr.active_punch_cancel.contains_key(&sid_b));
    }

    #[tokio::test]
    async fn p2p_teardown_does_not_cancel_mismatched_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId, TeardownReason};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        let active_sid = SessionId::from_bytes([1u8; 16]);
        let teardown_sid = SessionId::from_bytes([2u8; 16]);
        let token = CancellationToken::new();
        mgr.active_punch_cancel.insert(active_sid, token.clone());

        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: teardown_sid,
            reason: TeardownReason::FatalError,
        })
        .await;

        assert!(
            !token.is_cancelled(),
            "P2pTeardown for a different session must not cancel the active punch token"
        );
        assert!(
            !mgr.active_punch_cancel.is_empty(),
            "active_punch_cancel slot must be preserved on session mismatch"
        );
    }

    #[tokio::test]
    async fn p2p_teardown_for_stale_session_does_not_clear_active_session() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::{
            Candidate, CandidateKind, CertFingerprint, SessionId, TeardownReason,
        };

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let active_session = make_test_session_arc();
        let sid_a = SessionId::from_bytes([61u8; 16]);
        let sid_b = SessionId::from_bytes([62u8; 16]);
        multi.set_p2p(Some(active_session));
        multi.set_state(P2pState::Active {
            session_id: sid_b,
            since: std::time::Instant::now(),
        });

        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let token_a = CancellationToken::new();
        mgr.active_punch_cancel.insert(sid_a, token_a.clone());
        mgr.peer_contexts.insert(sid_a, PeerContext::default());
        let expected = ExpectedPeerMap::default();
        expected.insert(
            sid_a,
            ExpectedPeer {
                peer_client_id: "stale-peer".into(),
                cert_fp: CertFingerprint::from_bytes([2u8; 32]),
                candidates: vec![Candidate {
                    ip: "127.0.0.1".into(),
                    port: 6161,
                    kind: CandidateKind::Host,
                }],
            },
        );
        mgr.set_expected_peer_map(expected.clone());

        mgr.handle_message(BinaryMessage::P2pTeardown {
            session_id: sid_a,
            reason: TeardownReason::FatalError,
        })
        .await;

        assert!(
            token_a.is_cancelled(),
            "stale attempt token should be cancelled"
        );
        assert!(
            !mgr.active_punch_cancel.contains_key(&sid_a),
            "stale attempt token should be removed"
        );
        assert!(
            !mgr.peer_contexts.contains_key(&sid_a),
            "stale attempt context should be removed"
        );
        assert!(
            expected.get(sid_a).is_none(),
            "stale expected entry should be removed"
        );
        assert!(
            multi.p2p().is_some(),
            "stale Teardown must not clear current P2P session"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid_b),
            "stale Teardown must preserve Active(B), got {:?}",
            multi.p2p_state()
        );
        assert_eq!(mgr.failure_count, 0, "stale Teardown must not cooldown");
    }

    #[tokio::test]
    async fn p2p_rejected_answer_for_stale_session_does_not_clear_active_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_p2p(Some(make_test_session_arc()));
        let sid_a = SessionId::from_bytes([63u8; 16]);
        let sid_b = SessionId::from_bytes([64u8; 16]);
        multi.set_state(P2pState::Active {
            session_id: sid_b,
            since: std::time::Instant::now(),
        });
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: sid_a,
            accepted_client_id: "pc-a".into(),
            ok: false,
            reason: "stale reject".into(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
        })
        .await;

        assert!(
            multi.p2p().is_some(),
            "stale rejected Answer must not clear current P2P session"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid_b),
            "stale rejected Answer must preserve Active(B), got {:?}",
            multi.p2p_state()
        );
        assert_eq!(
            mgr.failure_count, 0,
            "stale rejected Answer must not cooldown"
        );
        assert!(
            !mgr.peer_contexts.contains_key(&sid_a),
            "unknown rejected Answer must not create context"
        );
    }

    #[tokio::test]
    async fn p2p_answer_for_unknown_session_does_not_create_context_or_overwrite_cache() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        let cached_candidate = Candidate {
            ip: "127.0.0.1".into(),
            port: 7001,
            kind: CandidateKind::Host,
        };
        let answer_candidate = Candidate {
            ip: "127.0.0.1".into(),
            port: 7002,
            kind: CandidateKind::Host,
        };
        let cached_fp = CertFingerprint::from_bytes([7u8; 32]);
        let answer_fp = CertFingerprint::from_bytes([8u8; 32]);
        mgr.peer_candidates_cache = vec![cached_candidate.clone()];
        mgr.peer_cert_fp_cache = Some(cached_fp);

        let unknown_sid = SessionId::from_bytes([65u8; 16]);
        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id: unknown_sid,
            accepted_client_id: "pc-unknown".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![answer_candidate],
            dst_cert_fp: answer_fp,
        })
        .await;

        assert!(
            !mgr.peer_contexts.contains_key(&unknown_sid),
            "unknown ok Answer must not create a context"
        );
        assert_eq!(
            mgr.peer_candidates_cache,
            vec![cached_candidate],
            "unknown ok Answer must not overwrite legacy candidate cache"
        );
        assert_eq!(
            mgr.peer_cert_fp_cache,
            Some(cached_fp),
            "unknown ok Answer must not overwrite legacy cert cache"
        );
    }

    #[tokio::test]
    async fn p2p_punch_sync_for_unknown_session_does_not_use_global_cache() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.peer_candidates_cache = vec![Candidate {
            ip: "127.0.0.1".into(),
            port: 7101,
            kind: CandidateKind::Host,
        }];
        mgr.peer_cert_fp_cache = Some(CertFingerprint::from_bytes([9u8; 32]));

        let unknown_sid = SessionId::from_bytes([66u8; 16]);
        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: unknown_sid,
            t_start_ms: 0,
            burst_count: 1,
            port_offsets: vec![0],
        })
        .await;

        assert!(
            !mgr.active_punch_cancel.contains_key(&unknown_sid),
            "unknown PunchSync must not spawn an initiator punch from global caches"
        );
        assert!(
            !matches!(multi.p2p_state(), P2pState::Punching { session_id, .. } if session_id == unknown_sid),
            "unknown PunchSync must not move state into Punching for that session"
        );
    }

    #[tokio::test]
    async fn p2p_punch_sync_for_stale_known_session_does_not_override_current_state() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_p2p(Some(make_test_session_arc()));
        let stale_sid = SessionId::from_bytes([67u8; 16]);
        let active_sid = SessionId::from_bytes([68u8; 16]);
        multi.set_state(P2pState::Active {
            session_id: active_sid,
            since: std::time::Instant::now(),
        });

        let mut mgr = P2pManager::new(
            multi.clone(),
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );
        mgr.peer_contexts.insert(
            stale_sid,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "127.0.0.1".into(),
                    port: 4433,
                    kind: CandidateKind::Host,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([2u8; 32])),
                peer_client_id: Some("mobile-1".into()),
                local_client_id: None,
                allow_parallel: false,
                ..PeerContext::default()
            },
        );

        mgr.handle_message(BinaryMessage::P2pPunchSync {
            session_id: stale_sid,
            t_start_ms: 0,
            burst_count: 1,
            port_offsets: vec![0],
        })
        .await;

        assert!(
            !mgr.peer_contexts.contains_key(&stale_sid),
            "stale known PunchSync context should be cleaned"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == active_sid),
            "stale known PunchSync must preserve Active(B), got {:?}",
            multi.p2p_state()
        );
        let teardown = out_rx.try_recv().expect("stale PunchSync should teardown");
        assert!(
            matches!(teardown, BinaryMessage::P2pTeardown { session_id, .. } if session_id == stale_sid),
            "expected teardown for stale session, got {teardown:?}"
        );
    }

    #[test]
    fn accept_probe_seq_first_probe_always_accepted() {
        let mut max = 0u32;
        let mut seen = false;
        assert!(accept_probe_seq(0, &mut max, &mut seen));
        assert!(seen);
        assert_eq!(max, 0);
    }

    #[test]
    fn accept_probe_seq_advances_max_on_higher_seq() {
        let mut max = 0u32;
        let mut seen = false;
        assert!(accept_probe_seq(10, &mut max, &mut seen));
        assert!(accept_probe_seq(20, &mut max, &mut seen));
        assert_eq!(max, 20);
    }

    #[test]
    fn accept_probe_seq_keeps_max_when_in_window() {
        let mut max = 0u32;
        let mut seen = false;
        accept_probe_seq(100, &mut max, &mut seen);
        // seq within window of max (max - 64 .. max] are still accepted
        // for out-of-order arrivals.
        assert!(accept_probe_seq(50, &mut max, &mut seen));
        assert_eq!(max, 100, "in-window low seq must not lower max");
    }

    #[test]
    fn accept_probe_seq_rejects_replay_below_window() {
        let mut max = 0u32;
        let mut seen = false;
        accept_probe_seq(200, &mut max, &mut seen);
        assert!(
            !accept_probe_seq(100, &mut max, &mut seen),
            "seq more than 64 below max must be rejected"
        );
        assert_eq!(max, 200);
    }

    #[test]
    fn accept_probe_seq_no_overflow_panic_on_max_seq() {
        // Seq near u32::MAX must not panic in debug builds. Previous
        // `seq + 64` overflowed; new path uses saturating_sub on max so the
        // arithmetic stays bounded.
        let mut max = 100u32;
        let mut seen = true;
        // Adversarial: u32::MAX. With saturating bounds the comparison
        // evaluates `MAX < max.saturating_sub(64) == 36` which is false →
        // accept (the seq is "above" max, not below the window).
        assert!(accept_probe_seq(u32::MAX, &mut max, &mut seen));
        assert_eq!(max, u32::MAX);
        // Now a probe with seq 0 must reject (well below the window).
        assert!(!accept_probe_seq(0, &mut max, &mut seen));
    }

    #[tokio::test]
    async fn manager_shutdown_drains_spawned_tasks() {
        // Tasks spawned through `task_tracker` must be awaited by
        // `run()` before it returns. Pre-fix bare `tokio::spawn` orphaned
        // them on shutdown.
        use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
        use tp_core::p2p_types::CertFingerprint;

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mgr = P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let counter = Arc::new(AtomicU8::new(0));
        let c2 = counter.clone();
        mgr.task_tracker.spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            c2.fetch_add(1, AtomicOrdering::SeqCst);
        });

        // Close the inbound channel so `run()` exits its pump loop.
        drop(in_tx);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish within 2s")
            .expect("run() task did not panic");

        assert_eq!(
            counter.load(AtomicOrdering::SeqCst),
            1,
            "spawned task must complete before run() returns"
        );
    }

    #[tokio::test]
    async fn manager_shutdown_cancels_pending_signaling_timeouts() {
        let (mut mgr, _out_rx, _multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "mobile-AbC12345-0",
        );
        mgr.schedule_offer_answer_timeout(SessionId::from_bytes([0xEB; 16]));

        tokio::time::timeout(Duration::from_millis(20), mgr.run())
            .await
            .expect("manager shutdown must cancel pending signaling timers");
    }

    #[tokio::test]
    async fn unanswered_initiator_offer_cleans_up_negotiating_state() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi.clone(),
            "mobile-AbC12345-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );

        mgr.try_initiate("pc-XyZ98765-0")
            .await
            .expect("offer should send");
        let offer = out_rx.recv().await.expect("offer");
        let session_id = match offer {
            BinaryMessage::P2pOffer {
                session_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-XyZ98765-0");
                assert_eq!(role, P2pRole::Initiator);
                session_id
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        };

        let event = tokio::time::timeout(Duration::from_secs(1), mgr.internal_rx.recv())
            .await
            .expect("offer answer timeout event")
            .expect("event");
        match event {
            P2pInternalEvent::OfferAnswerTimedOut {
                session_id: cleaned,
                ..
            } => assert_eq!(cleaned, session_id),
            other => panic!("expected OfferAnswerTimedOut, got {other:?}"),
        }
        mgr.handle_offer_answer_timeout(session_id);

        assert!(
            matches!(multi.p2p_state(), P2pState::Idle),
            "unanswered offer must not leave source stuck in Negotiating"
        );
        assert!(
            !mgr.peer_contexts.contains_key(&session_id),
            "stale unanswered offer context should be removed"
        );
    }

    #[tokio::test]
    async fn cancelled_queued_signaling_timeout_cannot_cleanup_advanced_session() {
        let (mut mgr, _out_rx, multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "mobile-AbC12345-0",
        );
        let session_id = SessionId::from_bytes([0xEA; 16]);
        multi.set_state(P2pState::Negotiating { session_id });
        mgr.peer_contexts.insert(
            session_id,
            PeerContext {
                session_role: Some(ClientRole::Initiator),
                ..PeerContext::default()
            },
        );
        mgr.schedule_offer_answer_timeout(session_id);

        let queued_timeout = tokio::time::timeout(Duration::from_secs(1), mgr.internal_rx.recv())
            .await
            .expect("timeout event should be queued")
            .expect("timeout event");
        mgr.cancel_offer_answer_timeout(session_id);
        multi.set_state(P2pState::Punching {
            session_id,
            started_at: std::time::Instant::now(),
        });

        mgr.handle_internal_event(queued_timeout);

        assert!(
            mgr.peer_contexts.contains_key(&session_id),
            "an event cancelled by PunchSync must not tear down the advanced attempt"
        );
        assert!(matches!(
            multi.p2p_state(),
            P2pState::Punching {
                session_id: active,
                ..
            } if active == session_id
        ));
        mgr.cleanup_session_attempt(session_id);
    }

    #[tokio::test]
    async fn duplicate_success_answer_after_punch_sync_does_not_rearm_timeout() {
        use tp_core::p2p_types::{Candidate, CandidateKind};

        let (mut mgr, _out_rx, multi) = make_test_manager_with_role(
            crate::p2p::session::ClientRole::Initiator,
            "peer-a-AbCd0001-0",
        );
        let session_id = SessionId::from_bytes([0xEC; 16]);
        multi.set_state(P2pState::Punching {
            session_id,
            started_at: std::time::Instant::now(),
        });
        mgr.peer_contexts.insert(
            session_id,
            PeerContext {
                candidates: vec![Candidate {
                    ip: "1.1.1.1".into(),
                    port: 41000,
                    kind: CandidateKind::ServerReflexive,
                }],
                cert_fp: Some(CertFingerprint::from_bytes([0xED; 32])),
                peer_client_id: Some("peer-b-AbCd0002-0".into()),
                local_client_id: Some("peer-a-AbCd0001-0".into()),
                session_role: Some(ClientRole::Initiator),
                ..PeerContext::default()
            },
        );
        assert!(!mgr.pending_answer_cancel.contains_key(&session_id));

        mgr.handle_message(BinaryMessage::P2pAnswer {
            session_id,
            accepted_client_id: "peer-b-AbCd0002-0".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "8.8.8.8".into(),
                port: 42000,
                kind: CandidateKind::ServerReflexive,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([0xEE; 32]),
        })
        .await;

        assert!(
            !mgr.pending_answer_cancel.contains_key(&session_id),
            "a duplicate Answer after PunchSync must not create a new signaling timeout"
        );
        mgr.cleanup_session_attempt(session_id);
    }

    #[tokio::test]
    async fn parallel_initiator_offers_do_not_overwrite_first_inflight_state() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi.clone(),
            "mobile-AbC12345-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_mapping_probe_reflector_for_test(None);

        mgr.try_initiate("pc-XyZ98765-0")
            .await
            .expect("first offer");
        let first_session_id = match out_rx.recv().await.expect("first offer message") {
            BinaryMessage::P2pOffer {
                session_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-XyZ98765-0");
                assert_eq!(role, P2pRole::Initiator);
                session_id
            }
            other => panic!("expected first P2pOffer, got {other:?}"),
        };
        assert!(
            matches!(
                multi.p2p_state(),
                P2pState::Negotiating { session_id } if session_id == first_session_id
            ),
            "first in-flight offer should own the anchor state"
        );

        mgr.try_initiate("pc-XyZ98765-1")
            .await
            .expect("second offer");
        let second_session_id = match out_rx.recv().await.expect("second offer message") {
            BinaryMessage::P2pOffer {
                session_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-XyZ98765-1");
                assert_eq!(role, P2pRole::Initiator);
                session_id
            }
            other => panic!("expected second P2pOffer, got {other:?}"),
        };
        assert_ne!(first_session_id, second_session_id);
        assert!(
            matches!(
                multi.p2p_state(),
                P2pState::Negotiating { session_id } if session_id == first_session_id
            ),
            "parallel sidecar offers must not overwrite the anchor in-flight state"
        );
        assert!(mgr.peer_contexts.contains_key(&first_session_id));
        assert!(mgr.peer_contexts.contains_key(&second_session_id));
    }

    #[tokio::test]
    async fn initiator_run_sends_offer_after_first_announce_ack() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("pc-1".into(), 0);

        let run_handle = tokio::spawn(async move { mgr.run().await });

        let first = out_rx.recv().await.expect("initial announce");
        assert!(
            matches!(first, BinaryMessage::P2pAnnounce { .. }),
            "manager must announce before attempting P2P"
        );

        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();

        let offer = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("offer timeout")
            .expect("offer received");
        match offer {
            BinaryMessage::P2pOffer {
                src_client_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(src_client_id, "mobile-1");
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn initiator_with_empty_peer_id_does_not_send_offer() {
        use tp_core::p2p_types::CertFingerprint;
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer(String::new(), 0);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();

        let no_offer = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(
            no_offer.is_err(),
            "empty peer_client_id must leave initiator relay-only"
        );

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn initiator_with_empty_peer_id_sends_offer_after_peer_hint() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer(String::new(), 0);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
            .await
            .expect_err("empty peer_client_id must not send an offer before hint");

        in_tx
            .send(BinaryMessage::P2pPeerHint {
                peer_client_id: "pc-1".into(),
            })
            .await
            .unwrap();

        let offer = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("offer timeout")
            .expect("offer received");
        match offer {
            BinaryMessage::P2pOffer {
                src_client_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(src_client_id, "mobile-1");
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn initiator_with_empty_peer_id_sends_only_one_offer_for_multiple_peer_hints() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer(String::new(), 0);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
            .await
            .expect_err("empty peer_client_id must not send an offer before hints");

        for peer_client_id in ["pc-AbC12345-0", "pc-AbC12345-1"] {
            in_tx
                .send(BinaryMessage::P2pPeerHint {
                    peer_client_id: peer_client_id.into(),
                })
                .await
                .unwrap();
        }

        let offer = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("offer timeout")
            .expect("offer received");
        match offer {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(role, P2pRole::Initiator);
                assert!(
                    dst_client_id == "pc-AbC12345-0" || dst_client_id == "pc-AbC12345-1",
                    "offer must target one hinted peer, got {dst_client_id}"
                );
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(150), out_rx.recv())
                .await
                .is_err(),
            "initiator must not open an alternate P2P offer for the second hint"
        );

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn initiator_ignores_late_second_peer_hint_while_first_is_negotiating() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer(String::new(), 0);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
            .await
            .expect_err("empty peer_client_id must not send an offer before hints");

        in_tx
            .send(BinaryMessage::P2pPeerHint {
                peer_client_id: "pc-1".into(),
            })
            .await
            .unwrap();
        let first_offer = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("first offer timeout")
            .expect("first offer received");
        match first_offer {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected first P2pOffer, got {other:?}"),
        }

        in_tx
            .send(BinaryMessage::P2pPeerHint {
                peer_client_id: "pc-1-1".into(),
            })
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), out_rx.recv())
                .await
                .is_err(),
            "late second hint must not trigger an alternate P2P offer while first peer is negotiating"
        );

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn initiator_peer_hint_bypasses_relay_uptime_delay() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer(String::new(), 30);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
            .await
            .expect_err("empty peer_client_id must not offer before hint");

        in_tx
            .send(BinaryMessage::P2pPeerHint {
                peer_client_id: "pc-1".into(),
            })
            .await
            .unwrap();

        let offer = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("peer hint should trigger immediate offer")
            .expect("offer received");
        match offer {
            BinaryMessage::P2pOffer {
                src_client_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(src_client_id, "mobile-1");
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn initiator_updates_cached_peer_hint_while_idle() {
        use tp_core::p2p_types::CertFingerprint;
        use tp_core::protocol::BinaryMessage;

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Idle);
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer(String::new(), 0);

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "stale-mobile".into(),
        })
        .await;
        assert_eq!(mgr.auto_peer_client_id.as_deref(), Some("stale-mobile"));

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "pc-1".into(),
        })
        .await;

        assert_eq!(
            mgr.auto_peer_client_id.as_deref(),
            Some("pc-1"),
            "initiator must not stay pinned to the first gateway peer hint"
        );
        assert_eq!(
            mgr.auto_peer_client_ids,
            vec!["pc-1".to_string()],
            "unrelated stale peer hint must be replaced, not kept as a P2P target"
        );
    }

    #[tokio::test]
    async fn active_initiator_prunes_stale_same_replica_peer_hints() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let sid = SessionId::from_bytes([0x89; 16]);
        multi.set_state(P2pState::Active {
            session_id: sid,
            since: std::time::Instant::now(),
        });
        let mut mgr = super::P2pManager::new(
            multi.clone(),
            "app-APP12345-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peers_for_test(
            vec![
                "client-OLD12345-0".into(),
                "client-OLD12345-1".into(),
                "client-OLD12345-2".into(),
            ],
            0,
        );

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "client-NEW12345-0".into(),
        })
        .await;
        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "client-NEW12345-1".into(),
        })
        .await;

        let peers = mgr.auto_peer_client_ids();
        assert_eq!(
            peer_id_for_local_replica("app-APP12345-0", &peers).as_deref(),
            Some("client-NEW12345-0"),
            "fresh same-replica hint should replace stale primary target"
        );
        assert_eq!(
            peer_id_for_local_replica("app-APP12345-1", &peers).as_deref(),
            Some("client-NEW12345-1"),
            "fresh same-replica hint should replace stale refill target"
        );
        assert_eq!(
            peer_id_for_local_replica("app-APP12345-2", &peers).as_deref(),
            Some("client-OLD12345-2"),
            "unhinted replica candidates should be left alone"
        );
        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid),
            "candidate cleanup must not tear down the active P2P session"
        );
    }

    #[tokio::test]
    async fn initiator_peer_hint_change_clears_cooldown() {
        use tp_core::p2p_types::CertFingerprint;
        use tp_core::protocol::BinaryMessage;

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Cooldown {
            until: std::time::Instant::now() + Duration::from_secs(60),
        });
        let mut mgr = super::P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("pc-1-1".into(), 0);

        mgr.handle_message(BinaryMessage::P2pPeerHint {
            peer_client_id: "pc-1".into(),
        })
        .await;

        assert_eq!(mgr.auto_peer_client_id.as_deref(), Some("pc-1"));
        assert!(
            matches!(multi.p2p_state(), P2pState::Idle),
            "new anchor hint should clear stale-replica cooldown"
        );
    }

    #[tokio::test]
    async fn auto_initiator_retries_after_cooldown_expires() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi.clone(),
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("pc-1".into(), 0);
        multi.set_state(P2pState::Cooldown {
            until: std::time::Instant::now() - Duration::from_millis(1),
        });

        let action = mgr.maybe_try_initiator_attempt().await;
        assert_eq!(
            action,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL),
            "expired cooldown should retry and keep polling for future refills"
        );

        let offer = out_rx
            .try_recv()
            .expect("expired cooldown must retry offer");
        match offer {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_installed_resets_auto_initiator_failure_count() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.failure_count = 4;

        mgr.handle_internal_event(P2pInternalEvent::SessionInstalled {
            session_id: SessionId::from_bytes([0x33; 16]),
        });

        assert_eq!(
            mgr.failure_count, 0,
            "a successful P2P install must reset retry backoff"
        );
    }

    #[tokio::test]
    async fn auto_initiator_keeps_polling_after_starting_offer() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Idle);
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("pc-1".into(), 0);

        let action = mgr.maybe_try_initiator_attempt().await;

        assert_eq!(
            action,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL),
            "after sending an offer the auto initiator must keep polling for later P2P drops"
        );
        let offer = out_rx.try_recv().expect("offer should still be sent");
        match offer {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_initiator_attempts_only_primary_configured_peer_when_idle() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Idle);
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peers_for_test(vec!["pc-1".into(), "pc-1-1".into()], 0);

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );

        match out_rx.try_recv().expect("expected P2P offer") {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(role, P2pRole::Initiator);
                assert_eq!(dst_client_id, "pc-1");
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
        assert!(
            out_rx.try_recv().is_err(),
            "single-replica initiator must not send alternate peer offers"
        );
    }

    #[tokio::test]
    async fn auto_initiator_keeps_polling_while_active_to_refill_lost_session() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let sid = SessionId::from_bytes([0x65; 16]);
        multi.set_state(P2pState::Active {
            session_id: sid,
            since: std::time::Instant::now(),
        });
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("pc-1".into(), 0);
        mgr.peer_contexts.insert(
            sid,
            PeerContext {
                peer_client_id: Some("pc-1".into()),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );
        assert!(
            out_rx.try_recv().is_err(),
            "polling a healthy active context must not send a duplicate offer"
        );
    }

    #[tokio::test]
    async fn forced_refill_bypasses_quarantined_installed_p2p_context() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let stale_sid = SessionId::from_bytes([0x67; 16]);
        let stale_p2p = make_test_session_arc();
        multi
            .install_p2p_session(stale_sid, "pc-1".into(), stale_p2p.clone())
            .expect("install stale p2p");
        multi.set_state(P2pState::Active {
            session_id: stale_sid,
            since: std::time::Instant::now(),
        });
        assert!(
            multi.mark_p2p_session_unusable_for_new_flows_for_handle(&stale_p2p),
            "test setup must quarantine the installed P2P session"
        );

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());
        engine.set_replicas_for_test(1);

        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.peer_contexts.insert(
            stale_sid,
            PeerContext {
                peer_client_id: Some("pc-1".into()),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );
        mgr.handle_internal_event(P2pInternalEvent::RefillRequested {
            peer_client_id: "pc-1".into(),
        });

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );

        match out_rx.try_recv().expect("forced refill should send offer") {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn successful_p2p_install_notifies_manager_to_schedule_refill_poll() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tokio_util::sync::CancellationToken;
        use tp_core::p2p_types::{Candidate, CandidateKind, SessionId};
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;

        let multi = make_test_multi_arc();
        let sid = SessionId::from_bytes([0x66; 16]);
        multi.set_state(P2pState::HandshakingQuic { session_id: sid });
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::channel::<PackedMessage>(8);
        let (_inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 55)), 4444);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            outbound_tx,
            inbound_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );

        let observation = install_successful_p2p_session(
            sid,
            session,
            "peer-b-AbCd0002-0",
            "198.51.100.23:4433".parse().unwrap(),
            "0.0.0.0:53000".parse().unwrap(),
            &[Candidate {
                ip: "192.0.2.55".into(),
                port: 4444,
                kind: CandidateKind::ServerReflexive,
            }],
            Some(P2pUnderlayInterfaceIndexes {
                ipv4: NonZeroU32::new(7),
                ipv6: NonZeroU32::new(70),
                ipv4_source_ip: Some("192.168.240.44".parse().unwrap()),
            }),
            &multi,
            None,
            None,
            &cancel,
            true,
            &internal_tx,
            &out_tx,
        )
        .await
        .expect("successful install returns the direct path observation");

        assert_eq!(observation.remote_candidate_ip, peer.ip());
        assert_eq!(observation.peer_client_id, "peer-b-AbCd0002-0");
        assert_eq!(
            observation.remote_candidate_kind,
            Some(CandidateKind::ServerReflexive)
        );
        assert_eq!(observation.socket_family, P2pAddressFamily::Ipv4);
        assert_eq!(observation.selected_ifindex, NonZeroU32::new(7));

        match internal_rx
            .try_recv()
            .expect("successful install must notify manager")
        {
            P2pInternalEvent::SessionInstalled { session_id } => assert_eq!(session_id, sid),
            other => panic!("expected SessionInstalled event, got {other:?}"),
        }
        assert!(
            matches!(
                out_rx.try_recv().expect("session ready should be emitted"),
                BinaryMessage::P2pSessionReady { session_id, .. } if session_id == sid
            ),
            "successful install should still report P2pSessionReady"
        );
    }

    #[tokio::test]
    async fn auto_initiator_refills_after_local_replica_p2p_loss_with_stale_context() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let anchor_multi = make_test_multi_arc();
        anchor_multi.set_state(P2pState::Idle);
        let replica_multi = make_test_multi_arc();
        let stale_sid = SessionId::from_bytes([0x64; 16]);
        replica_multi.set_p2p(Some(make_test_session_arc()));
        replica_multi.set_state(P2pState::Active {
            session_id: stale_sid,
            since: std::time::Instant::now(),
        });

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", anchor_multi.clone());
        engine.install_proxy_replica_session_for_test("mobile-1-1", replica_multi.clone());

        let mut mgr = super::P2pManager::new(
            anchor_multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.set_auto_initiate_peer("pc-1".into(), 0);
        mgr.peer_contexts.insert(
            stale_sid,
            PeerContext {
                peer_client_id: Some("pc-1".into()),
                allow_parallel: true,
                ..PeerContext::default()
            },
        );

        crate::p2p::installer::close_current_p2p(&replica_multi);
        assert!(
            mgr.peer_contexts.contains_key(&stale_sid),
            "test setup needs a stale retained peer context"
        );

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );

        let offer = out_rx
            .try_recv()
            .expect("local P2P loss must trigger refill offer");
        match offer {
            BinaryMessage::P2pOffer {
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(dst_client_id, "pc-1");
                assert_eq!(role, P2pRole::Initiator);
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        }
        assert!(
            !mgr.peer_contexts.contains_key(&stale_sid),
            "stale local-loss context must be removed before refill"
        );
    }

    #[tokio::test]
    async fn auto_initiator_does_not_offer_when_p2p_pool_is_full() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let anchor_multi = make_test_multi_arc();
        let replica_multi = make_test_multi_arc();
        for (multi, byte) in [(&anchor_multi, 0x71), (&replica_multi, 0x72)] {
            multi.set_p2p(Some(make_test_session_arc()));
            multi.set_state(P2pState::Active {
                session_id: SessionId::from_bytes([byte; 16]),
                since: std::time::Instant::now(),
            });
        }

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-AbC12345-0", anchor_multi.clone());
        engine.install_proxy_replica_session_for_test("mobile-AbC12345-1", replica_multi);
        engine.set_replicas_for_test(2);

        let mut mgr = super::P2pManager::new(
            anchor_multi,
            "mobile-AbC12345-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.set_auto_initiate_peers_for_test(
            vec![
                "pc-new-XyZ98765-0".into(),
                "pc-new-XyZ98765-1".into(),
                "pc-old-AbC12345-0".into(),
            ],
            0,
        );

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );
        assert!(
            out_rx.try_recv().is_err(),
            "a full P2P pool must not keep sending duplicate offers"
        );
    }

    #[tokio::test]
    async fn rejected_answer_retries_are_throttled() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mut mgr = super::P2pManager::new(
            multi,
            "mobile-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_auto_initiate_peer("pc-1".into(), 0);

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();

        let session_id = match out_rx.recv().await.expect("first offer") {
            BinaryMessage::P2pOffer {
                session_id, role, ..
            } => {
                assert_eq!(role, P2pRole::Initiator);
                session_id
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        };
        in_tx
            .send(BinaryMessage::P2pAnswer {
                session_id,
                accepted_client_id: String::new(),
                ok: false,
                reason: "peer offline".into(),
                candidates: vec![],
                dst_cert_fp: CertFingerprint::zero(),
            })
            .await
            .unwrap();

        let immediate_retry = tokio::time::timeout(Duration::from_millis(150), out_rx.recv()).await;
        assert!(
            immediate_retry.is_err(),
            "rejected answers must not schedule an immediate retry storm"
        );

        run_handle.abort();
    }

    #[tokio::test]
    async fn auto_initiator_refills_sidecar_direct_sessions_when_primary_is_active() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let anchor_multi = make_test_multi_arc();
        anchor_multi.set_p2p(Some(make_test_session_arc()));
        anchor_multi.set_state(P2pState::Active {
            session_id: SessionId::from_bytes([0x73; 16]),
            since: std::time::Instant::now(),
        });
        let replica_a = make_test_multi_arc();
        let replica_b = make_test_multi_arc();

        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("mobile-AbC12345-0", anchor_multi.clone());
        engine.install_proxy_replica_session_for_test("mobile-AbC12345-1", replica_a);
        engine.install_proxy_replica_session_for_test("mobile-AbC12345-2", replica_b);
        engine.set_replicas_for_test(3);

        let mut mgr = super::P2pManager::new(
            anchor_multi,
            "mobile-AbC12345-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.set_auto_initiate_peers_for_test(
            vec![
                "pc-new-XyZ98765-0".into(),
                "pc-new-XyZ98765-1".into(),
                "pc-new-XyZ98765-2".into(),
                "pc-old-AbC12345-0".into(),
            ],
            0,
        );

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );

        let mut offered = Vec::new();
        while let Ok(msg) = out_rx.try_recv() {
            match msg {
                BinaryMessage::P2pOffer {
                    session_id,
                    dst_client_id,
                    role,
                    ..
                } => {
                    assert_eq!(role, P2pRole::Initiator);
                    offered.push((
                        dst_client_id,
                        engine.pending_p2p_local_client_id_for_test(session_id),
                    ));
                }
                other => panic!("expected P2pOffer, got {other:?}"),
            }
        }
        assert_eq!(
            offered,
            vec![
                (
                    "pc-new-XyZ98765-1".to_string(),
                    Some("mobile-AbC12345-1".to_string())
                ),
                (
                    "pc-new-XyZ98765-2".to_string(),
                    Some("mobile-AbC12345-2".to_string())
                ),
            ],
            "active primary P2P must not suppress same-index sidecar offers"
        );
    }

    #[tokio::test]
    async fn auto_initiator_opens_primary_pair_before_sidecar_pairs() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let primary = make_test_multi_arc();
        primary.set_state(P2pState::Idle);
        let sidecar = make_test_multi_arc();
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-1", sidecar);
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-0", primary.clone());
        engine.set_replicas_for_test(3);
        let mut mgr = super::P2pManager::new(
            primary,
            "tunA-AppRnd01-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.set_auto_initiate_peers_for_test(
            vec![
                "tunA-CliRnd01-0".into(),
                "tunA-CliRnd01-1".into(),
                "tunA-CliRnd01-2".into(),
            ],
            0,
        );

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );

        let mut offered = Vec::new();
        while let Ok(msg) = out_rx.try_recv() {
            match msg {
                BinaryMessage::P2pOffer {
                    session_id,
                    dst_client_id,
                    role,
                    ..
                } => {
                    assert_eq!(role, P2pRole::Initiator);
                    offered.push((
                        dst_client_id,
                        engine.pending_p2p_local_client_id_for_test(session_id),
                    ));
                }
                other => panic!("expected P2pOffer, got {other:?}"),
            }
        }
        assert_eq!(
            offered,
            vec![
                (
                    "tunA-CliRnd01-0".to_string(),
                    Some("tunA-AppRnd01-0".to_string())
                ),
                (
                    "tunA-CliRnd01-1".to_string(),
                    Some("tunA-AppRnd01-1".to_string())
                ),
            ],
            "initiator should fill available local slots in replica-index order"
        );
    }

    #[tokio::test]
    async fn auto_initiator_pins_peer_target_to_local_replica_index() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        multi.set_state(P2pState::Idle);
        let engine = crate::Engine::new(
            crate::EngineConfig::default(),
            Arc::new(crate::status::NullListener),
        );
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-1", multi.clone());
        let mut mgr = super::P2pManager::new(
            multi,
            "tunA-AppRnd01-0".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Initiator,
            in_rx,
            out_tx,
            4433,
        );
        mgr.set_session_installer(engine.attach_p2p_session_installer());
        mgr.set_auto_initiate_peers_for_test(
            vec![
                "tunA-CliRnd01-0".into(),
                "tunA-CliRnd01-1".into(),
                "tunA-CliRnd01-2".into(),
            ],
            0,
        );

        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );

        let first_sid = match out_rx.try_recv().expect("expected P2P offer") {
            BinaryMessage::P2pOffer {
                session_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_eq!(role, P2pRole::Initiator);
                assert_eq!(dst_client_id, "tunA-CliRnd01-1");
                session_id
            }
            other => panic!("expected P2pOffer, got {other:?}"),
        };
        assert_eq!(
            engine
                .pending_p2p_local_client_id_for_test(first_sid)
                .as_deref(),
            Some("tunA-AppRnd01-1")
        );
        assert!(out_rx.try_recv().is_err());

        mgr.handle_session_attempt_cleanup(first_sid);
        assert_eq!(
            mgr.maybe_try_initiator_attempt().await,
            AutoInitiatorAttempt::RetryAfter(AUTO_INITIATOR_STATE_POLL)
        );
        match out_rx.try_recv().expect("expected retry P2P offer") {
            BinaryMessage::P2pOffer {
                session_id,
                dst_client_id,
                role,
                ..
            } => {
                assert_ne!(session_id, SessionId::from_bytes([0; 16]));
                assert_eq!(role, P2pRole::Initiator);
                assert_eq!(dst_client_id, "tunA-CliRnd01-1");
                assert_eq!(
                    engine
                        .pending_p2p_local_client_id_for_test(session_id)
                        .as_deref(),
                    Some("tunA-AppRnd01-1")
                );
            }
            other => panic!("expected retry P2pOffer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acceptor_run_does_not_auto_offer_after_announce_ack() {
        use tp_core::p2p_types::CertFingerprint;
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);
        let multi = make_test_multi_arc();
        let mgr = super::P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let run_handle = tokio::spawn(async move { mgr.run().await });
        let _announce = out_rx.recv().await.expect("initial announce");
        in_tx
            .send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 4433,
                server_time_ms: 0,
            })
            .await
            .unwrap();

        let no_offer = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(
            no_offer.is_err(),
            "acceptor role must not send source-side P2pOffer"
        );

        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("run() must finish after inbound closes")
            .expect("run() task did not panic");
    }

    #[tokio::test]
    async fn acceptor_replies_to_offer_with_answer() {
        use tp_core::p2p_types::{Candidate, CandidateKind, CertFingerprint, P2pRole, SessionId};
        use tp_core::protocol::BinaryMessage;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(8);

        // `MultiSession` requires a real `Session` (QUIC factory + live tasks).
        // The dummy helper returns `None` so this unit test short-circuits and
        // Phase-5 e2e exercises the acceptor flow end-to-end. Same deferral
        // pattern as Tasks 4.3 / 4.4.
        let multi = match crate::p2p::session::MultiSession::__test_only_dummy() {
            Some(m) => m,
            None => return,
        };

        let mgr = super::P2pManager::new(
            multi,
            "pc-1".into(),
            "g1".into(),
            CertFingerprint::from_bytes([1u8; 32]),
            crate::p2p::session::ClientRole::Acceptor,
            in_rx,
            out_tx,
            4433,
        );

        let offer = BinaryMessage::P2pOffer {
            session_id: SessionId::from_bytes([1u8; 16]),
            src_client_id: "mobile-1".into(),
            dst_client_id: "pc-1".into(),
            candidates: vec![Candidate {
                ip: "1.2.3.4".into(),
                port: 5555,
                kind: CandidateKind::ServerReflexive,
            }],
            src_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            role: P2pRole::Initiator,
        };
        in_tx.send(offer).await.unwrap();
        drop(in_tx);

        tokio::spawn(async move { mgr.run().await });

        // First message manager emits is the Announce. Skip it.
        let _ann = out_rx.recv().await.expect("announce");
        let reply = tokio::time::timeout(Duration::from_millis(500), out_rx.recv())
            .await
            .expect("reply timeout")
            .expect("reply received");
        match reply {
            BinaryMessage::P2pAnswer { ok, .. } => assert!(ok, "expected ok=true"),
            other => panic!("expected P2pAnswer, got {other:?}"),
        }
    }

    #[test]
    fn p2p_client_endpoint_uses_tuned_transport_keepalive() {
        let source = include_str!("manager.rs");
        let needle = [
            "client_cfg",
            ".transport_config(Arc::new(tp_transport::quic::tuned_transport_config(",
        ]
        .concat();

        assert!(
            source.contains(&needle),
            "P2P client QUIC config must use the shared keepalive/idle tuning"
        );
    }
}
