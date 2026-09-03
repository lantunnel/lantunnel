//! `MultiSession`: holds the relay [`Session`] + optional P2P [`Session`]
//! and the shared conn-id maps so target sockets survive a P2P→relay path
//! flip (best-effort migration).
//!
//! The `inbound` / `udp_inbound` `DashMap`s used to live as locals inside
//! `Engine::run_replica` (see `engine.rs:1117-1118`). They are hoisted onto
//! `MultiSession` here without changing types so the conn registry outlives
//! any single `Session`. Tasks 4.9/4.10 will replace the `run_replica`
//! locals with `multi.inbound()` / `multi.udp_inbound()`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tp_core::p2p_types::SessionId;
use tp_metrics::MetricsManager;
use tp_transport::session::{Session, SessionQueueSnapshot};
use tp_transport::DropOldestSender;

use crate::p2p::flow_scheduler::{CandidateKey, CandidatePath};
use crate::p2p::scheduler::{PathKind, PathScheduler};
use crate::peer_link_manager::PeerRelationKey;
use crate::status::{TrafficCounters, TrafficPath};

/// Map the scheduler's local `PathKind` enum onto the metrics-crate one.
/// Used by `pick()` for `p2p_path_picks_total` and `p2p_path_switches_total`.
fn path_kind_to_metric(kind: PathKind) -> tp_metrics::P2pPathKind {
    match kind {
        PathKind::Relay => tp_metrics::P2pPathKind::Relay,
        PathKind::P2p => tp_metrics::P2pPathKind::P2p,
    }
}

/// Local role for the P2P negotiation. Mobile clients always initiate
/// (`Initiator`); PC clients listen (`Acceptor`). Wired through `P2pManager`
/// at construction time; Task 4.11 plumbs in the actual role from the client
/// startup config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientRole {
    Initiator,
    Acceptor,
}

/// State machine for a P2P attempt. Tracked alongside the relay so the
/// scheduler / state-tick logic can observe progress without poking inside
/// the P2P task.
#[derive(Clone, Debug)]
pub enum P2pState {
    /// P2P disabled by config or feature flag.
    Disabled,
    /// Eligible but no negotiation in flight.
    Idle,
    /// Local side has emitted a `P2pAnnounce` and is awaiting a partner.
    Announcing,
    /// Reservation/match is in flight.
    Negotiating { session_id: SessionId },
    /// UDP hole-punching with the remote endpoint.
    Punching {
        session_id: SessionId,
        started_at: Instant,
    },
    /// QUIC handshake over the punched socket.
    HandshakingQuic { session_id: SessionId },
    /// P2P session is live and serving frames.
    Active {
        session_id: SessionId,
        since: Instant,
    },
    /// Recent failure; back off until this instant before retrying.
    Cooldown { until: Instant },
}

pub const DEFAULT_MAX_P2P_SESSIONS_PER_REPLICA: usize = 128;
const PENDING_UDP_INBOUND_CONN_CAP: usize = 256;
const PENDING_UDP_INBOUND_PER_CONN_CAP: usize = crate::engine::UDP_FLOW_INBOUND_CHANNEL_CAP;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUdpInbound {
    pub path: TrafficPath,
    pub payload: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingUdpInboundBufferResult {
    Buffered { dropped_oldest: bool },
    DroppedConnCap,
}

#[must_use]
pub struct TcpFlowStreamGuard {
    active: Arc<AtomicUsize>,
    local_traffic: Arc<TrafficCounters>,
}

impl Drop for TcpFlowStreamGuard {
    fn drop(&mut self) {
        let _ = self
            .active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_sub(1)
            });
        self.local_traffic.mark_progress();
    }
}

#[derive(Clone)]
pub struct P2pSessionEntry {
    pub session_id: SessionId,
    pub peer_client_id: String,
    pub session: Arc<Session>,
    relation_key: Option<PeerRelationKey>,
    accepts_new_flows: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct P2pCandidatePath {
    pub session_id: SessionId,
    pub peer_client_id: String,
    pub peer_family: String,
    pub session: Arc<Session>,
}

#[derive(Clone)]
struct P2pLaneChangeObserver {
    fence: Arc<parking_lot::Mutex<()>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

/// Composite handle: relay + optional P2P + the shared conn-id maps.
///
/// `MultiSession` is `Arc<…>`-shared between the engine, the P2P task, and
/// every `MultiSenderRouter` clone in the data plane.
pub struct MultiSession {
    relay: Arc<Session>,
    p2p_sessions: DashMap<SessionId, P2pSessionEntry>,
    p2p_peer_index: DashMap<String, SessionId>,
    p2p_rr: AtomicUsize,
    max_p2p_sessions: usize,
    scheduler: Arc<PathScheduler>,
    state: Mutex<P2pState>,
    /// Hoisted from `engine.rs::run_replica` so the maps survive a
    /// path-flip; identical `DashMap` types.
    inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
    udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    pending_udp_inbound: DashMap<String, VecDeque<PendingUdpInbound>>,
    /// Optional metrics sink (Task 4.12). `None` = no-op; lets unit tests
    /// and pre-Task-4.13 callers skip metric construction. Set via
    /// [`MultiSession::set_metrics`] at startup.
    metrics: ArcSwapOption<MetricsManager>,
    /// Optional UI/API traffic counters. Kept separate from Prometheus
    /// metrics because app/client status needs reset-on-start snapshots.
    traffic: ArcSwapOption<TrafficCounters>,
    /// Per-replica traffic counters used by the heartbeat watchdog. The UI
    /// counters above are global for the whole tunnel, but liveness decisions
    /// must only consider progress on the session being watched.
    local_traffic: Arc<TrafficCounters>,
    active_tcp_flow_streams: Arc<AtomicUsize>,
    /// Timestamp captured the moment `set_state` first transitions
    /// to `P2pState::Negotiating`. Consumed (cleared) when the first P2P
    /// session is installed to record the `p2p_handoff_latency_ms`
    /// histogram. Stored as an `Instant`
    /// (wrapped in `Arc` so `ArcSwapOption` can hold it) instead of
    /// stuffing the value into the `Negotiating` enum variant, so the
    /// `P2pState` shape stays unchanged.
    negotiating_started_at: ArcSwapOption<Instant>,
    /// Optional Engine-owned V2 projection hook. Direct eligibility/removal
    /// has several legitimate owners (watchdog, data sender, manager
    /// teardown), so the mutation itself must share one fence instead of
    /// relying on every caller to remember a later status refresh.
    lane_change_observer: parking_lot::RwLock<Option<P2pLaneChangeObserver>>,
}

impl MultiSession {
    /// Construct with the relay session only. The P2P registry starts empty;
    /// callers populate it once P2P handshakes complete.
    pub fn new_with_relay_only(relay: Arc<Session>) -> Arc<Self> {
        Self::new_with_existing_maps_and_scheduler(
            relay,
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            Arc::new(PathScheduler::default()),
        )
    }

    /// Bridge constructor used by `engine::run_replica` (Task 4.9). The
    /// engine still owns the split `SessionReceiver`/`DatagramReceiver`
    /// halves locally for its reader tasks, but hands the
    /// `Arc<Session>` send-only shell + the existing conn-id maps to
    /// `MultiSession` so they can outlive a P2P path-flip and so external
    /// callers (`Engine::multi_session()`) can reach them.
    pub fn new_with_existing_maps(
        relay: Arc<Session>,
        inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    ) -> Arc<Self> {
        Self::new_with_existing_maps_and_scheduler(
            relay,
            inbound,
            udp_inbound,
            Arc::new(PathScheduler::default()),
        )
    }

    /// Same as [`new_with_existing_maps`] but injects a
    /// caller-built [`PathScheduler`] so YAML overrides (`min_advantage`,
    /// `stable_cycles`) can flow through. The default-scheduler form
    /// delegates here so call sites that do not configure the scheduler
    /// stay one line.
    pub fn new_with_existing_maps_and_scheduler(
        relay: Arc<Session>,
        inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>>,
        scheduler: Arc<PathScheduler>,
    ) -> Arc<Self> {
        Arc::new(Self {
            relay,
            p2p_sessions: DashMap::new(),
            p2p_peer_index: DashMap::new(),
            p2p_rr: AtomicUsize::new(0),
            max_p2p_sessions: DEFAULT_MAX_P2P_SESSIONS_PER_REPLICA,
            scheduler,
            state: Mutex::new(P2pState::Disabled),
            inbound,
            udp_inbound,
            pending_udp_inbound: DashMap::new(),
            metrics: ArcSwapOption::new(None),
            traffic: ArcSwapOption::new(None),
            local_traffic: Arc::new(TrafficCounters::default()),
            active_tcp_flow_streams: Arc::new(AtomicUsize::new(0)),
            negotiating_started_at: ArcSwapOption::new(None),
            lane_change_observer: parking_lot::RwLock::new(None),
        })
    }

    pub(crate) fn set_p2p_lane_change_observer(
        &self,
        fence: Arc<parking_lot::Mutex<()>>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) {
        *self.lane_change_observer.write() = Some(P2pLaneChangeObserver { fence, on_change });
    }

    fn mutate_p2p_lanes<R>(&self, mutation: impl FnOnce() -> (R, bool)) -> R {
        let observer = self.lane_change_observer.read().clone();
        let _guard = observer.as_ref().map(|observer| observer.fence.lock());
        let (result, changed) = mutation();
        if changed {
            if let Some(observer) = observer.as_ref() {
                (observer.on_change)();
            }
        }
        result
    }

    /// Install (or clear) the metrics sink. `None` re-enables the no-op
    /// fast path. Atomic w.r.t. concurrent emitters via `ArcSwapOption`.
    pub fn set_metrics(&self, metrics: Option<Arc<MetricsManager>>) {
        self.metrics.store(metrics);
    }

    /// Snapshot the current metrics sink, if any. Cheap (`ArcSwapOption`
    /// load).
    pub fn metrics(&self) -> Option<Arc<MetricsManager>> {
        self.metrics.load_full()
    }

    pub fn set_traffic(&self, traffic: Option<Arc<TrafficCounters>>) {
        self.traffic.store(traffic);
    }

    pub fn local_traffic(&self) -> Arc<TrafficCounters> {
        self.local_traffic.clone()
    }

    pub fn record_traffic_tx(&self, kind: PathKind, bytes: i64) {
        let Ok(bytes) = u64::try_from(bytes) else {
            return;
        };
        self.local_traffic
            .record_tx(path_kind_to_status(kind), bytes);
        self.local_traffic.mark_progress();
        if let Some(traffic) = self.traffic.load_full() {
            traffic.record_tx(path_kind_to_status(kind), bytes);
        }
    }

    pub fn record_traffic_rx(&self, path: TrafficPath, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.local_traffic.record_rx(path, bytes);
        self.local_traffic.mark_progress();
        if let Some(traffic) = self.traffic.load_full() {
            traffic.record_rx(path, bytes);
        }
    }

    pub fn mark_progress(&self) {
        self.local_traffic.mark_progress();
    }

    pub fn active_tcp_flow_streams(&self) -> Arc<AtomicUsize> {
        self.active_tcp_flow_streams.clone()
    }

    pub fn begin_tcp_flow_stream(&self) -> TcpFlowStreamGuard {
        self.active_tcp_flow_streams.fetch_add(1, Ordering::Relaxed);
        self.local_traffic.mark_progress();
        TcpFlowStreamGuard {
            active: self.active_tcp_flow_streams.clone(),
            local_traffic: self.local_traffic.clone(),
        }
    }

    pub fn buffer_pending_udp_inbound(
        &self,
        conn_id: &str,
        path: TrafficPath,
        payload: Bytes,
    ) -> PendingUdpInboundBufferResult {
        if !self.pending_udp_inbound.contains_key(conn_id)
            && self.pending_udp_inbound.len() >= PENDING_UDP_INBOUND_CONN_CAP
        {
            return PendingUdpInboundBufferResult::DroppedConnCap;
        }

        let mut dropped_oldest = false;
        let mut pending = self
            .pending_udp_inbound
            .entry(conn_id.to_string())
            .or_default();
        if pending.len() >= PENDING_UDP_INBOUND_PER_CONN_CAP {
            pending.pop_front();
            dropped_oldest = true;
        }
        pending.push_back(PendingUdpInbound { path, payload });
        self.local_traffic.mark_progress();
        PendingUdpInboundBufferResult::Buffered { dropped_oldest }
    }

    pub fn drain_pending_udp_inbound(&self, conn_id: &str) -> Vec<PendingUdpInbound> {
        self.pending_udp_inbound
            .remove(conn_id)
            .map(|(_, pending)| pending.into_iter().collect())
            .unwrap_or_default()
    }

    pub fn clear_pending_udp_inbound(&self, conn_id: &str) {
        self.pending_udp_inbound.remove(conn_id);
    }

    /// Borrow the relay session. Always present.
    pub fn relay(&self) -> &Arc<Session> {
        &self.relay
    }

    /// Snapshot the current P2P session, if any.
    pub fn p2p(&self) -> Option<Arc<Session>> {
        self.p2p_any()
    }

    /// Snapshot the current installed P2P session, even if it has been
    /// quarantined from new flow placement.
    fn p2p_any(&self) -> Option<Arc<Session>> {
        let len = self.p2p_sessions.len();
        if len == 0 {
            return None;
        }
        let idx = self.p2p_rr.fetch_add(1, Ordering::Relaxed) % len;
        self.p2p_sessions
            .iter()
            .nth(idx)
            .map(|entry| entry.value().session.clone())
            .or_else(|| {
                self.p2p_sessions
                    .iter()
                    .next()
                    .map(|entry| entry.value().session.clone())
            })
    }

    /// Snapshot the current P2P session usable for new flow placement.
    pub fn p2p_for_new_flow(&self) -> Option<Arc<Session>> {
        let sessions: Vec<Arc<Session>> = self
            .p2p_sessions
            .iter()
            .filter(|entry| entry.value().accepts_new_flows.load(Ordering::Acquire))
            .map(|entry| entry.value().session.clone())
            .collect();
        if sessions.is_empty() {
            return None;
        }
        let idx = self.p2p_rr.fetch_add(1, Ordering::Relaxed) % sessions.len();
        sessions.get(idx).cloned()
    }

    pub fn p2p_session_count(&self) -> usize {
        self.p2p_sessions.len()
    }

    pub fn p2p_eligible_session_count(&self) -> usize {
        self.p2p_sessions
            .iter()
            .filter(|entry| entry.value().accepts_new_flows.load(Ordering::Acquire))
            .count()
    }

    pub fn p2p_peer_ids(&self) -> Vec<String> {
        let mut peers: Vec<String> = self
            .p2p_sessions
            .iter()
            .map(|entry| entry.value().peer_client_id.clone())
            .collect();
        peers.sort();
        peers.dedup();
        peers
    }

    pub fn p2p_eligible_peer_ids(&self) -> Vec<String> {
        let mut peers: Vec<String> = self
            .p2p_sessions
            .iter()
            .filter(|entry| entry.value().accepts_new_flows.load(Ordering::Acquire))
            .map(|entry| entry.value().peer_client_id.clone())
            .collect();
        peers.sort();
        peers.dedup();
        peers
    }

    pub fn p2p_candidate_paths(&self) -> Vec<P2pCandidatePath> {
        self.p2p_sessions
            .iter()
            .filter(|entry| entry.value().accepts_new_flows.load(Ordering::Acquire))
            .map(|entry| {
                let value = entry.value();
                P2pCandidatePath {
                    session_id: value.session_id,
                    peer_family: crate::p2p::replica::replica_family_id(&value.peer_client_id),
                    peer_client_id: value.peer_client_id.clone(),
                    session: value.session.clone(),
                }
            })
            .collect()
    }

    /// Return only healthy direct candidates belonging to one logical Peer.
    /// While protocol v4 remains in use, the normalized Replica family is the
    /// temporary logical Peer identity.
    pub fn candidate_paths_for_peer(&self, peer_family: &str) -> Vec<P2pCandidatePath> {
        let peer_family = crate::p2p::replica::replica_family_id(peer_family);
        self.p2p_candidate_paths()
            .into_iter()
            .filter(|candidate| candidate.peer_family == peer_family)
            .collect()
    }

    pub fn p2p_installed_paths(&self) -> Vec<P2pCandidatePath> {
        self.p2p_sessions
            .iter()
            .map(|entry| {
                let value = entry.value();
                P2pCandidatePath {
                    session_id: value.session_id,
                    peer_family: crate::p2p::replica::replica_family_id(&value.peer_client_id),
                    peer_client_id: value.peer_client_id.clone(),
                    session: value.session.clone(),
                }
            })
            .collect()
    }

    pub fn queue_snapshot_for_candidate(&self, key: &CandidateKey) -> Option<SessionQueueSnapshot> {
        match key.path {
            CandidatePath::Relay => Some(self.relay.queue_snapshot()),
            CandidatePath::P2p => {
                let session_id = key.p2p_session_id?;
                self.p2p_sessions
                    .get(&session_id)
                    .map(|entry| entry.session.queue_snapshot())
            }
        }
    }

    pub fn has_p2p_session(&self, session_id: SessionId) -> bool {
        self.p2p_sessions.contains_key(&session_id)
    }

    pub fn install_p2p_session(
        &self,
        session_id: SessionId,
        peer_client_id: String,
        session: Arc<Session>,
    ) -> anyhow::Result<()> {
        self.install_p2p_session_for_relation(session_id, peer_client_id, session, None)
    }

    pub(crate) fn install_p2p_session_for_relation(
        &self,
        session_id: SessionId,
        peer_client_id: String,
        session: Arc<Session>,
        relation_key: Option<PeerRelationKey>,
    ) -> anyhow::Result<()> {
        let peer_client_id = if peer_client_id.trim().is_empty() {
            format!("{session_id:?}")
        } else {
            peer_client_id
        };

        if relation_key.as_ref().is_some_and(|relation_key| {
            self.p2p_sessions.iter().any(|entry| {
                entry.session_id != session_id
                    && entry.relation_key.as_ref() == Some(relation_key)
                    && entry.accepts_new_flows.load(Ordering::Relaxed)
            })
        }) {
            session.close();
            anyhow::bail!("P2P relation already active for another generation");
        }

        if !self.p2p_sessions.contains_key(&session_id)
            && self.p2p_sessions.len() >= self.max_p2p_sessions
        {
            session.close();
            anyhow::bail!(
                "P2P session registry full ({}) for relay replica",
                self.max_p2p_sessions
            );
        }

        let previous = self.p2p_sessions.insert(
            session_id,
            P2pSessionEntry {
                session_id,
                peer_client_id: peer_client_id.clone(),
                session: session.clone(),
                relation_key,
                accepts_new_flows: Arc::new(AtomicBool::new(true)),
            },
        );
        if let Some(previous) = previous {
            previous.session.close();
            if previous.peer_client_id != peer_client_id {
                self.p2p_peer_index
                    .remove_if(&previous.peer_client_id, |_, sid| *sid == session_id);
            }
        } else {
            if let Some(started) = self.negotiating_started_at.swap(None) {
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                if let Some(m) = self.metrics.load_full() {
                    m.observe_p2p_handoff_latency_ms(elapsed_ms);
                }
            }
            if let Some(m) = self.metrics.load_full() {
                m.observe_p2p_active_sessions(1);
            }
        }
        self.p2p_peer_index.insert(peer_client_id, session_id);
        Ok(())
    }

    pub(crate) fn has_eligible_p2p_relation(&self, relation_key: &PeerRelationKey) -> bool {
        self.p2p_sessions.iter().any(|entry| {
            entry.relation_key.as_ref() == Some(relation_key)
                && entry.accepts_new_flows.load(Ordering::Relaxed)
        })
    }

    pub fn close_p2p_session(&self, session_id: SessionId) -> bool {
        self.mutate_p2p_lanes(|| {
            let closed = self.close_p2p_session_without_lane_change_notification(session_id);
            (closed, closed)
        })
    }

    pub(crate) fn close_p2p_session_without_lane_change_notification(
        &self,
        session_id: SessionId,
    ) -> bool {
        let Some((_, entry)) = self.p2p_sessions.remove(&session_id) else {
            return false;
        };
        self.finish_removed_p2p_session(entry);
        true
    }

    /// Close only sessions belonging to one logical Peer family.
    ///
    /// The family predicate is checked again during removal so a concurrent
    /// replacement under the same `SessionId` cannot make this operation
    /// close a different Peer's session.
    pub fn close_p2p_sessions_for_peer(&self, peer_family: &str) -> usize {
        self.mutate_p2p_lanes(|| {
            let closed =
                self.close_p2p_sessions_for_peer_without_lane_change_notification(peer_family);
            (closed, closed > 0)
        })
    }

    pub(crate) fn close_p2p_sessions_for_peer_without_lane_change_notification(
        &self,
        peer_family: &str,
    ) -> usize {
        let peer_family = crate::p2p::replica::replica_family_id(peer_family);
        let session_ids: Vec<SessionId> = self
            .p2p_sessions
            .iter()
            .filter(|entry| {
                crate::p2p::replica::replica_family_id(&entry.value().peer_client_id) == peer_family
            })
            .map(|entry| entry.value().session_id)
            .collect();
        let mut closed = 0;
        for session_id in session_ids {
            let Some((_, entry)) = self.p2p_sessions.remove_if(&session_id, |_, entry| {
                crate::p2p::replica::replica_family_id(&entry.peer_client_id) == peer_family
            }) else {
                continue;
            };
            self.finish_removed_p2p_session(entry);
            closed += 1;
        }
        closed
    }

    fn finish_removed_p2p_session(&self, entry: P2pSessionEntry) {
        let session_id = entry.session_id;
        entry.session.close();
        if self
            .p2p_peer_index
            .remove_if(&entry.peer_client_id, |_, sid| *sid == session_id)
            .is_some()
        {
            if let Some(replacement) = self
                .p2p_sessions
                .iter()
                .find(|candidate| candidate.value().peer_client_id == entry.peer_client_id)
                .map(|candidate| candidate.value().session_id)
            {
                self.p2p_peer_index
                    .insert(entry.peer_client_id.clone(), replacement);
            }
        }

        let mut state = self.state.lock().unwrap();
        if let P2pState::Active {
            session_id: active,
            since,
        } = &*state
        {
            if *active == session_id {
                let elapsed_secs = since.elapsed().as_secs_f64();
                if let Some(m) = self.metrics.load_full() {
                    m.observe_p2p_session_duration_secs(elapsed_secs);
                }
            }
        }
        if self.p2p_sessions.is_empty() && matches!(&*state, P2pState::Active { .. }) {
            *state = P2pState::Idle;
        }
        drop(state);

        if let Some(m) = self.metrics.load_full() {
            m.observe_p2p_active_sessions(-1);
        }
    }

    pub fn close_p2p_session_for_handle(&self, session: &Arc<Session>) -> bool {
        self.mutate_p2p_lanes(|| {
            let closed =
                self.close_p2p_session_for_handle_without_lane_change_notification(session);
            (closed, closed)
        })
    }

    pub(crate) fn close_p2p_session_for_handle_without_lane_change_notification(
        &self,
        session: &Arc<Session>,
    ) -> bool {
        let session_id = self
            .p2p_sessions
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.value().session, session))
            .map(|entry| entry.value().session_id);
        let Some(session_id) = session_id else {
            return false;
        };
        let Some((_, entry)) = self
            .p2p_sessions
            .remove_if(&session_id, |_, entry| Arc::ptr_eq(&entry.session, session))
        else {
            return false;
        };
        self.finish_removed_p2p_session(entry);
        true
    }

    pub fn mark_p2p_session_unusable_for_new_flows_for_handle(
        &self,
        session: &Arc<Session>,
    ) -> bool {
        self.mutate_p2p_lanes(|| {
            let changed = self
                .p2p_sessions
                .iter()
                .find(|entry| Arc::ptr_eq(&entry.value().session, session))
                .map(|entry| {
                    entry
                        .value()
                        .accepts_new_flows
                        .swap(false, Ordering::AcqRel)
                })
                .unwrap_or(false);
            (changed, changed)
        })
    }

    pub fn mark_p2p_session_usable_for_new_flows_for_handle(&self, session: &Arc<Session>) -> bool {
        self.mutate_p2p_lanes(|| {
            let changed = self
                .p2p_sessions
                .iter()
                .find(|entry| Arc::ptr_eq(&entry.value().session, session))
                .map(|entry| !entry.value().accepts_new_flows.swap(true, Ordering::AcqRel))
                .unwrap_or(false);
            (changed, changed)
        })
    }

    fn current_state_session_id(&self) -> Option<SessionId> {
        match self.state.lock().unwrap().clone() {
            P2pState::Negotiating { session_id }
            | P2pState::Punching { session_id, .. }
            | P2pState::HandshakingQuic { session_id }
            | P2pState::Active { session_id, .. } => Some(session_id),
            _ => None,
        }
    }

    pub fn p2p_peer_client_id_for_handle(&self, session: &Arc<Session>) -> Option<String> {
        self.p2p_sessions
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.value().session, session))
            .map(|entry| entry.value().peer_client_id.clone())
            .filter(|peer| peer != "__legacy_single_p2p_peer__" && !peer.trim().is_empty())
    }

    /// Derive the remote stable Peer only from an installed canonical
    /// relation. The entry's replica id is used solely as a consistency check;
    /// callers never treat that mutable/display value as an authenticated
    /// principal.
    pub(crate) fn authenticated_remote_peer_for_handle(
        &self,
        session: &Arc<Session>,
        local_peer_id: &str,
    ) -> Option<String> {
        let entry = self
            .p2p_sessions
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.value().session, session))?;
        let relation = entry.value().relation_key.as_ref()?;
        let remote_peer_id = if relation.first_peer_family == local_peer_id {
            &relation.second_peer_family
        } else if relation.second_peer_family == local_peer_id {
            &relation.first_peer_family
        } else {
            return None;
        };
        (crate::p2p::replica::replica_family_id(&entry.value().peer_client_id) == *remote_peer_id)
            .then(|| remote_peer_id.clone())
    }

    pub fn p2p_session_id_for_handle(&self, session: &Arc<Session>) -> Option<SessionId> {
        self.p2p_sessions
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.value().session, session))
            .map(|entry| entry.value().session_id)
    }

    pub fn close_all_p2p(&self) {
        self.mutate_p2p_lanes(|| {
            let closed = self.close_all_p2p_without_lane_change_notification();
            ((), closed > 0)
        });
    }

    pub(crate) fn close_all_p2p_without_lane_change_notification(&self) -> usize {
        let session_ids: Vec<SessionId> = self
            .p2p_sessions
            .iter()
            .map(|entry| entry.value().session_id)
            .collect();
        let mut closed = 0;
        for session_id in session_ids {
            closed +=
                usize::from(self.close_p2p_session_without_lane_change_notification(session_id));
        }
        closed
    }

    /// Legacy compatibility wrapper for installing or clearing P2P sessions.
    /// Production paths use the keyed bounded registry directly via
    /// [`MultiSession::install_p2p_session`] and
    /// [`MultiSession::close_p2p_session`].
    ///
    /// Latency instrumentation:
    /// * First installed session emits `p2p_handoff_latency_ms` if a
    ///   `Negotiating` start timestamp was captured (cleared on emit).
    /// * Clearing installed sessions emits `p2p_session_duration_seconds`
    ///   only for sessions that reached `Active`.
    ///
    /// Gauge instrumentation:
    /// * Installs adjust `p2p_active_sessions` by `+1`.
    /// * Clears adjust `p2p_active_sessions` by `-1`.
    /// * Idempotent re-install leaves the gauge alone; the count is of
    ///   installed registry entries, not lifetimes.
    pub fn set_p2p(&self, sess: Option<Arc<Session>>) {
        const LEGACY_PEER: &str = "__legacy_single_p2p_peer__";
        if let Some(session) = sess {
            let session_id = self
                .current_state_session_id()
                .unwrap_or_else(|| SessionId::from_bytes([0u8; 16]));
            let _ = self.install_p2p_session(session_id, LEGACY_PEER.into(), session);
        } else {
            self.close_all_p2p();
        }
    }

    /// Snapshot the P2P state machine.
    pub fn p2p_state(&self) -> P2pState {
        self.state.lock().unwrap().clone()
    }

    /// Replace the P2P state machine value.
    ///
    /// Stamps `negotiating_started_at` when entering `Negotiating`
    /// from a non-`Negotiating` state, so `set_p2p(Some)` can later
    /// observe the handoff latency. A `Negotiating` → `Negotiating`
    /// no-op (same session) leaves the stamp untouched; a fresh attempt
    /// (e.g. after Cooldown) restamps because we want the latency for
    /// THIS attempt, not the previous failed one.
    pub fn set_state(&self, new: P2pState) {
        let mut guard = self.state.lock().unwrap();
        let was_negotiating = matches!(&*guard, P2pState::Negotiating { .. });
        let entering_negotiating = matches!(&new, P2pState::Negotiating { .. });
        let old_label = p2p_state_label(&guard);
        let new_label = p2p_state_label(&new);
        let changed = old_label != new_label;
        if changed {
            tracing::debug!(
                from = old_label,
                to = new_label,
                state = ?new,
                "p2p state transition"
            );
        }
        *guard = new;
        drop(guard);
        if entering_negotiating && !was_negotiating {
            self.negotiating_started_at
                .store(Some(Arc::new(Instant::now())));
        }
    }

    /// Borrow the path scheduler. Stateful — flap-resistance lives there.
    pub fn scheduler(&self) -> &Arc<PathScheduler> {
        &self.scheduler
    }

    /// Per-frame path selection. Returns the [`Session`] the
    /// data plane should hit for THIS call. Falls back to relay if no P2P
    /// is installed or scheduler picks Relay.
    pub fn pick(&self) -> Arc<Session> {
        self.pick_with_kind().0
    }

    /// Picker variant that also surfaces the chosen path kind so
    /// callers (`MultiSenderRouter`) can attribute byte counts to the
    /// correct `p2p_bytes_total{path=...}` label without re-running the
    /// scheduler. The returned `PathKind` is the *effective* path: if
    /// the scheduler said `P2p` but no P2P session was installed, the
    /// caller fell back to relay and the kind reflects that.
    pub fn pick_with_kind(&self) -> (Arc<Session>, PathKind) {
        let p2p = self.p2p_for_new_flow();
        let kind = self.scheduler.pick_kind(
            &self.relay.stats(),
            p2p.as_ref().map(|s| s.stats()).as_ref(),
        );
        self.record_path_pick(kind);
        match kind {
            PathKind::P2p => match p2p {
                Some(s) => (s, PathKind::P2p),
                None => (self.relay.clone(), PathKind::Relay),
            },
            PathKind::Relay => (self.relay.clone(), PathKind::Relay),
        }
    }

    /// P2P-preferred picker for local-proxy connection setup and payloads.
    /// Unlike [`Self::pick_with_kind`], this bypasses the scheduler's
    /// health and advantage gates. Bulk TCP can drive RTT/loss very high
    /// while the direct link is still making progress; fail over on actual
    /// send/connection failure instead of interpreting congestion as death.
    pub fn pick_p2p_first_with_kind(&self) -> (Arc<Session>, PathKind) {
        let (session, kind) = match self.p2p_for_new_flow() {
            Some(p2p) => (p2p, PathKind::P2p),
            None => (self.relay.clone(), PathKind::Relay),
        };
        self.record_path_pick(kind);
        (session, kind)
    }

    pub(crate) fn record_path_pick(&self, kind: PathKind) {
        // Telemetry: count every per-frame decision so dashboards can
        // graph relay-vs-p2p share over time. Atomic add per
        // call — same cost as the existing Global counters. This also
        // records the Relay↔P2p transition (if any) into
        // `p2p_path_switches_total`; same-kind ticks return `None` and
        // skip the increment.
        if let Some(m) = self.metrics.load_full() {
            let label = path_kind_to_metric(kind);
            m.incr_p2p_path_pick(label);
            if let Some(prev) = self.scheduler.record_transition(kind) {
                m.incr_p2p_path_switch(path_kind_to_metric(prev), label);
            }
        } else {
            // Keep `last_pick` updated even when metrics are absent so a
            // later `set_metrics(Some(_))` doesn't see a stale "Unset"
            // sentinel and miscount the next pick as a non-switch.
            self.scheduler.record_transition(kind);
        }
    }

    /// Shared conn-id map for TCP `Connect` flows. Same type/usage as the
    /// `run_replica` local it replaces.
    pub fn inbound(&self) -> Arc<DashMap<String, mpsc::Sender<Bytes>>> {
        self.inbound.clone()
    }

    /// Shared conn-id map for UDP flows. Same type/usage as the
    /// `run_replica` local it replaces.
    pub fn udp_inbound(&self) -> Arc<DashMap<String, DropOldestSender<Bytes>>> {
        self.udp_inbound.clone()
    }

    pub fn remove_datagram_association_from_all_paths(&self, conn_id: &str) -> usize {
        let mut removed = self.relay.remove_datagram_association(conn_id);
        for entry in self.p2p_sessions.iter() {
            removed += entry.value().session.remove_datagram_association(conn_id);
        }
        removed
    }

    /// Called when a P2P session is being torn down. Returns the number
    /// of currently-active conn_ids that will continue running over relay.
    /// Migration is best-effort, so the maps are not partitioned
    /// by path — outbound frames automatically pick the next-best path
    /// via `MultiSenderRouter`.
    pub fn report_p2p_to_relay_migration(&self) -> usize {
        let n = self.inbound.len() + self.udp_inbound.len();
        if n > 0 {
            if let Some(m) = self.metrics.load_full() {
                m.incr_p2p_conn_id_migrations(tp_metrics::P2pMigrationDir::P2pToRelay, n as i64);
            }
            tracing::debug!(
                migrated = n,
                fallback_reason = "p2p_teardown",
                "P2P teardown; conn_ids continue on relay"
            );
        }
        n
    }

    pub fn report_p2p_to_relay_migration_with_context(
        &self,
        fallback_reason: &'static str,
        conn_id: Option<&str>,
        local_client_id: Option<&str>,
        p2p_session_id: Option<SessionId>,
    ) -> usize {
        let n = self.inbound.len() + self.udp_inbound.len();
        if n > 0 {
            if let Some(m) = self.metrics.load_full() {
                m.incr_p2p_conn_id_migrations(tp_metrics::P2pMigrationDir::P2pToRelay, n as i64);
            }
            tracing::debug!(
                migrated = n,
                fallback_reason,
                conn_id = conn_id.unwrap_or(""),
                local_client_id = local_client_id.unwrap_or(""),
                selected_p2p_session_id = ?p2p_session_id,
                "P2P down; conn_ids continue on relay"
            );
        }
        n
    }

    /// Test-only helper for downstream unit tests that need to construct a
    /// `P2pManager`. A real `Session` requires the QUIC factory + live
    /// background tasks (see `tp_transport::session::Session::new_channeled`),
    /// which can't be wired up cheaply in a unit test. Returning `None` lets
    /// callers short-circuit and rely on Phase-5 e2e for real coverage. Tasks
    /// 4.3 / 4.4 already follow this deferral pattern.
    ///
    /// Marked `#[doc(hidden)] pub` so integration tests under `tests/` can
    /// reach it (Task 4.10 migration regression tests). Not part of the
    /// stable public API.
    #[doc(hidden)]
    pub fn __test_only_dummy() -> Option<Arc<Self>> {
        None
    }
}

fn p2p_state_label(state: &P2pState) -> &'static str {
    match state {
        P2pState::Disabled => "disabled",
        P2pState::Idle => "idle",
        P2pState::Announcing => "announcing",
        P2pState::Negotiating { .. } => "negotiating",
        P2pState::Punching { .. } => "punching",
        P2pState::HandshakingQuic { .. } => "handshaking_quic",
        P2pState::Active { .. } => "active",
        P2pState::Cooldown { .. } => "cooldown",
    }
}

fn path_kind_to_status(kind: PathKind) -> TrafficPath {
    match kind {
        PathKind::Relay => TrafficPath::Relay,
        PathKind::P2p => TrafficPath::P2p,
    }
}

#[cfg(test)]
mod tests {
    // The compile-time check below ensures the public API shape is what
    // Tasks 4.9 / 4.10 / 4.10b expect. The runtime test below exercises
    // the histogram emission sites — these only need a synthetic
    // `Session` (built via the same `channel_session` helper used by
    // `multi_sender::tests`).
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::sync::mpsc as tokio_mpsc;
    use tp_core::p2p_types::SessionId;
    use tp_core::protocol::{BinaryMessage, PackedMessage};

    use super::*;

    #[allow(dead_code)]
    fn _api_compile_test(multi: Arc<MultiSession>) {
        let _: Arc<dyn std::any::Any + Send + Sync> = multi.clone();
        let _: Arc<DashMap<String, mpsc::Sender<Bytes>>> = multi.inbound();
        let _: Arc<DashMap<String, DropOldestSender<Bytes>>> = multi.udp_inbound();
        let _: P2pState = multi.p2p_state();
        multi.set_state(P2pState::Idle);
        multi.set_p2p(None);
        let _: Option<Arc<Session>> = multi.p2p();
        let _: &Arc<Session> = multi.relay();
        let _: Arc<Session> = multi.pick();
        let _: PathKind = multi.scheduler().pick_kind(&Default::default(), None);
    }

    /// Build a synthetic [`Session`] backed by an `mpsc` channel pair.
    /// Mirrors `multi_sender::tests::channel_session`; duplicated here
    /// to keep the histogram test self-contained without exporting that
    /// helper across modules.
    fn channel_session() -> Arc<Session> {
        let (out_tx, _out_rx) = tokio_mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = tokio_mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        Arc::new(Session::new_channeled(
            out_tx, in_rx, peer, closer, writer, reader,
        ))
    }

    fn channel_session_with_stats(stats: tp_transport::session::SessionStats) -> Arc<Session> {
        let (out_tx, _out_rx) = tokio_mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = tokio_mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let mut session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        session.install_stats_probe(Arc::new(move || stats));
        Arc::new(session)
    }

    #[tokio::test]
    async fn installed_relation_derives_the_opposite_stable_peer_principal() {
        let relay = channel_session();
        let direct = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let session_id = SessionId::from_bytes([0x61; 16]);
        let relation = crate::peer_link_manager::PeerRelationKey::from_canonical_initiator(
            "mesh-Local001-0",
            "mesh-RemoteB1-0",
        )
        .expect("canonical relation");
        multi
            .install_p2p_session_for_relation(
                session_id,
                "mesh-RemoteB1-2".into(),
                direct.clone(),
                Some(relation),
            )
            .expect("install relation-bound direct session");

        assert_eq!(
            multi.authenticated_remote_peer_for_handle(&direct, "mesh-Local001-0"),
            Some("mesh-RemoteB1-0".into()),
        );
    }

    #[tokio::test]
    async fn installed_v2_relation_uses_issuer_signed_stable_peer_ids() {
        let relay = channel_session();
        let direct = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let session_id = SessionId::from_bytes([0x62; 16]);
        let local = "11111111-1111-4111-8111-111111111111";
        let remote = "22222222-2222-4222-8222-222222222222";
        let relation =
            crate::peer_link_manager::PeerRelationKey::from_stable_peers(local, remote, 0)
                .expect("stable relation");
        multi
            .install_p2p_session_for_relation(
                session_id,
                remote.into(),
                direct.clone(),
                Some(relation),
            )
            .expect("install V2 relation-bound direct session");

        assert_eq!(
            multi.authenticated_remote_peer_for_handle(&direct, local),
            Some(remote.into()),
        );
    }

    fn stats(rtt_ms: u64, loss_rate: f64, pto_count: u32) -> tp_transport::session::SessionStats {
        tp_transport::session::SessionStats {
            rtt: std::time::Duration::from_millis(rtt_ms),
            loss_rate,
            pto_count,
        }
    }

    #[tokio::test]
    async fn p2p_first_picker_prefers_healthy_p2p_without_advantage_warmup() {
        let relay = channel_session_with_stats(stats(10, 0.0, 0));
        let p2p = channel_session_with_stats(stats(15, 0.0, 0));
        let multi = MultiSession::new_with_relay_only(relay);
        multi.set_p2p(Some(p2p.clone()));

        let (picked, kind) = multi.pick_p2p_first_with_kind();

        assert_eq!(kind, PathKind::P2p);
        assert!(Arc::ptr_eq(&picked, &p2p));
    }

    #[tokio::test]
    async fn p2p_first_picker_keeps_p2p_under_bulk_rtt_and_loss_pressure() {
        let relay = channel_session_with_stats(stats(25, 0.0, 0));
        let p2p = channel_session_with_stats(stats(2500, 0.20, 0));
        let multi = MultiSession::new_with_relay_only(relay.clone());
        multi.set_p2p(Some(p2p.clone()));

        let (picked, kind) = multi.pick_p2p_first_with_kind();

        assert_eq!(kind, PathKind::P2p);
        assert!(Arc::ptr_eq(&picked, &p2p));
    }

    #[tokio::test]
    async fn p2p_first_picker_keeps_p2p_when_relay_is_unhealthy_too() {
        let relay = channel_session_with_stats(stats(25, 0.20, 0));
        let p2p = channel_session_with_stats(stats(15, 0.20, 0));
        let multi = MultiSession::new_with_relay_only(relay);
        multi.set_p2p(Some(p2p.clone()));

        let (picked, kind) = multi.pick_p2p_first_with_kind();

        assert_eq!(kind, PathKind::P2p);
        assert!(Arc::ptr_eq(&picked, &p2p));
    }

    #[tokio::test]
    async fn local_traffic_counters_are_per_replica_even_with_shared_global_status() {
        let global = Arc::new(TrafficCounters::default());
        let primary = MultiSession::new_with_relay_only(channel_session());
        let sidecar = MultiSession::new_with_relay_only(channel_session());
        primary.set_traffic(Some(global.clone()));
        sidecar.set_traffic(Some(global.clone()));

        primary.record_traffic_tx(PathKind::Relay, 10);
        sidecar.record_traffic_rx(TrafficPath::P2p, 7);

        assert_eq!(global.snapshot().relay_tx_bytes, 10);
        assert_eq!(global.snapshot().p2p_rx_bytes, 7);
        assert_eq!(primary.local_traffic().snapshot().relay_tx_bytes, 10);
        assert_eq!(primary.local_traffic().snapshot().p2p_rx_bytes, 0);
        assert_eq!(sidecar.local_traffic().snapshot().relay_tx_bytes, 0);
        assert_eq!(sidecar.local_traffic().snapshot().p2p_rx_bytes, 7);
    }

    /// A Negotiating → set_p2p(Some) transition emits exactly one
    /// `p2p_handoff_latency_ms` observation; a subsequent Active →
    /// set_p2p(None) transition emits exactly one
    /// `p2p_session_duration_seconds` observation. We only assert the
    /// `_count` lines (not exact bucket placement) so the test is
    /// robust under wall-clock variance on slow CI hosts.
    #[tokio::test]
    async fn set_p2p_emits_handoff_and_duration_histograms() {
        let relay = channel_session();
        let p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));

        // Kick off a P2P attempt: Negotiating stamps the start instant.
        let session_id = SessionId::from_bytes([1u8; 16]);
        multi.set_state(P2pState::Negotiating { session_id });
        // Tiny pause so elapsed > 0; bucket placement is incidental.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // Install the P2P session: handoff latency must fire.
        multi.set_p2p(Some(p2p));
        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_handoff_latency_ms_count 1"),
            "handoff histogram should have one observation; got:\n{text}"
        );
        assert!(
            !text.contains("p2p_session_duration_seconds_count 1"),
            "session duration must NOT fire on set_p2p(Some); got:\n{text}"
        );

        // Move into Active so the duration emission has a valid origin.
        multi.set_state(P2pState::Active {
            session_id,
            since: Instant::now(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        // Tear down: session-duration histogram must fire.
        multi.set_p2p(None);
        let text2 = metrics.prometheus_text();
        assert!(
            text2.contains("p2p_session_duration_seconds_count 1"),
            "session-duration histogram should have one observation; got:\n{text2}"
        );
        // Handoff count must not double-fire.
        assert!(
            text2.contains("p2p_handoff_latency_ms_count 1"),
            "handoff must remain at 1 across the duration transition; got:\n{text2}"
        );
    }

    /// `set_p2p` flips the `p2p_active_sessions` gauge between
    /// `0` and `1` on `None ↔ Some` transitions. A `Some → Some`
    /// re-install MUST be idempotent (no double-count).
    #[tokio::test]
    async fn set_p2p_inc_dec_active_sessions_gauge() {
        let relay = channel_session();
        let p2p1 = channel_session();
        let p2p2 = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));

        // Baseline: 0.
        let text0 = metrics.prometheus_text();
        assert!(
            text0.contains("p2p_active_sessions 0"),
            "initial gauge must be 0:\n{text0}"
        );

        // Install: 0 → 1.
        multi.set_p2p(Some(p2p1));
        let text1 = metrics.prometheus_text();
        assert!(
            text1.contains("p2p_active_sessions 1"),
            "gauge must be 1 after install:\n{text1}"
        );

        // Idempotent re-install (Some → Some): gauge stays at 1.
        multi.set_p2p(Some(p2p2));
        let text2 = metrics.prometheus_text();
        assert!(
            text2.contains("p2p_active_sessions 1"),
            "Some→Some must not change the gauge:\n{text2}"
        );

        // Clear: 1 → 0.
        multi.set_p2p(None);
        let text3 = metrics.prometheus_text();
        assert!(
            text3.contains("p2p_active_sessions 0"),
            "gauge must drop to 0 after clear:\n{text3}"
        );
    }

    #[tokio::test]
    async fn p2p_peer_ids_are_deduped_and_sorted() {
        let relay = channel_session();
        let source_a = channel_session();
        let source_b = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let sid_a = SessionId::from_bytes([0xA1; 16]);
        let sid_b = SessionId::from_bytes([0xB2; 16]);

        multi
            .install_p2p_session(sid_b, "peer-b".into(), source_b)
            .expect("install source-b");
        multi
            .install_p2p_session(sid_a, "peer-a".into(), source_a)
            .expect("install source-a");

        assert_eq!(multi.p2p_peer_ids(), vec!["peer-a", "peer-b"]);
    }

    #[tokio::test]
    async fn close_p2p_session_for_old_handle_does_not_close_replacement_same_session_id() {
        let relay = channel_session();
        let old_p2p = channel_session();
        let replacement_p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let sid = SessionId::from_bytes([0xC7; 16]);

        multi
            .install_p2p_session(sid, "peer-a".into(), old_p2p.clone())
            .expect("install old p2p");
        multi
            .install_p2p_session(sid, "peer-a".into(), replacement_p2p.clone())
            .expect("install replacement p2p");

        assert!(
            !multi.close_p2p_session_for_handle(&old_p2p),
            "stale old handle must not close a replacement registered under the same session id"
        );
        assert_eq!(
            multi.p2p_session_id_for_handle(&replacement_p2p),
            Some(sid),
            "replacement P2P handle must remain installed"
        );
        assert!(multi.close_p2p_session_for_handle(&replacement_p2p));
        assert!(multi.p2p().is_none(), "replacement close should clear P2P");
    }

    #[tokio::test]
    async fn stale_p2p_session_stays_installed_but_is_excluded_from_new_flows() {
        let relay = channel_session();
        let p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay.clone());
        let sid = SessionId::from_bytes([0xD4; 16]);

        multi
            .install_p2p_session(sid, "peer-a".into(), p2p.clone())
            .expect("install p2p");

        assert_eq!(multi.p2p_session_count(), 1);
        assert_eq!(multi.p2p_eligible_session_count(), 1);
        assert!(multi.p2p_for_new_flow().is_some());
        assert_eq!(multi.p2p_candidate_paths().len(), 1);

        assert!(multi.mark_p2p_session_unusable_for_new_flows_for_handle(&p2p));
        assert_eq!(multi.p2p_session_count(), 1);
        assert_eq!(multi.p2p_eligible_session_count(), 0);
        assert!(
            multi.p2p().is_some(),
            "old pinned flows can keep the handle"
        );
        assert!(
            multi.p2p_for_new_flow().is_none(),
            "new flow placement must not select stale P2P"
        );
        assert!(multi.p2p_candidate_paths().is_empty());
        assert_eq!(multi.pick_p2p_first_with_kind().1, PathKind::Relay);

        assert!(multi.mark_p2p_session_usable_for_new_flows_for_handle(&p2p));
        assert_eq!(multi.p2p_eligible_session_count(), 1);
        assert!(multi.p2p_for_new_flow().is_some());
    }

    #[tokio::test]
    async fn installing_replacement_for_same_peer_does_not_close_stale_p2p_session() {
        let relay = channel_session();
        let old_p2p = channel_session();
        let new_p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let old_session_id = SessionId::from_bytes([6; 16]);
        let new_session_id = SessionId::from_bytes([7; 16]);
        let relation = crate::peer_link_manager::PeerRelationKey::from_canonical_initiator(
            "peer-a-AbCd0001-0",
            "peer-b-AbCd0002-0",
        )
        .expect("canonical relation");

        multi
            .install_p2p_session_for_relation(
                old_session_id,
                "peer-a".into(),
                old_p2p.clone(),
                Some(relation.clone()),
            )
            .expect("old p2p install");
        assert!(multi.mark_p2p_session_unusable_for_new_flows_for_handle(&old_p2p));
        multi
            .install_p2p_session_for_relation(
                new_session_id,
                "peer-a".into(),
                new_p2p.clone(),
                Some(relation),
            )
            .expect("replacement install");

        assert_eq!(multi.p2p_session_count(), 2);
        assert_eq!(multi.p2p_eligible_session_count(), 1);
        assert_eq!(
            multi.p2p_session_id_for_handle(&old_p2p),
            Some(old_session_id)
        );
        assert_eq!(
            multi.p2p_session_id_for_handle(&new_p2p),
            Some(new_session_id)
        );
    }

    #[tokio::test]
    async fn installing_second_healthy_generation_for_one_relation_is_rejected() {
        let relay = channel_session();
        let first_p2p = channel_session();
        let duplicate_p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let first_session_id = SessionId::from_bytes([0x71; 16]);
        let duplicate_session_id = SessionId::from_bytes([0x72; 16]);
        let relation = crate::peer_link_manager::PeerRelationKey::from_canonical_initiator(
            "peer-a-AbCd0001-0",
            "peer-b-AbCd0002-0",
        )
        .expect("canonical relation");

        multi
            .install_p2p_session_for_relation(
                first_session_id,
                "peer-b-AbCd0002-0".into(),
                first_p2p.clone(),
                Some(relation.clone()),
            )
            .expect("first generation install");

        let error = multi
            .install_p2p_session_for_relation(
                duplicate_session_id,
                "peer-b-AbCd0002-0".into(),
                duplicate_p2p,
                Some(relation.clone()),
            )
            .expect_err("healthy relation must reject a duplicate generation");

        assert!(error.to_string().contains("relation already active"));
        assert!(multi.has_eligible_p2p_relation(&relation));
        assert_eq!(multi.p2p_session_count(), 1);
        assert_eq!(
            multi.p2p_session_id_for_handle(&first_p2p),
            Some(first_session_id)
        );
        assert!(!multi.has_p2p_session(duplicate_session_id));
    }

    #[tokio::test]
    async fn p2p_candidate_paths_include_peer_family() {
        let relay = channel_session();
        let source_a = channel_session();
        let source_b = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let sid_a = SessionId::from_bytes([0xA3; 16]);
        let sid_b = SessionId::from_bytes([0xB4; 16]);

        multi
            .install_p2p_session(sid_a, "pc-AbC12345-0".into(), source_a.clone())
            .expect("install source-a");
        multi
            .install_p2p_session(sid_b, "pc-AbC12345-1".into(), source_b.clone())
            .expect("install source-b");

        let candidates = multi.p2p_candidate_paths();
        assert_eq!(candidates.len(), 2);
        let anchor = candidates
            .iter()
            .find(|candidate| candidate.session_id == sid_a)
            .expect("anchor candidate");
        assert_eq!(anchor.peer_client_id, "pc-AbC12345-0");
        assert_eq!(anchor.peer_family, "pc-AbC12345-0");
        assert!(Arc::ptr_eq(&anchor.session, &source_a));
        let replica = candidates
            .iter()
            .find(|candidate| candidate.session_id == sid_b)
            .expect("replica candidate");
        assert_eq!(replica.peer_client_id, "pc-AbC12345-1");
        assert_eq!(replica.peer_family, "pc-AbC12345-0");
        assert!(Arc::ptr_eq(&replica.session, &source_b));
    }

    #[tokio::test]
    async fn candidate_paths_for_peer_excludes_faster_sessions_from_other_peers() {
        let relay = channel_session();
        let peer_b = channel_session_with_stats(stats(40, 0.0, 0));
        let peer_c_0 = channel_session_with_stats(stats(5, 0.0, 0));
        let peer_c_1 = channel_session_with_stats(stats(4, 0.0, 0));
        let multi = MultiSession::new_with_relay_only(relay);
        let sid_b = SessionId::from_bytes([0xB1; 16]);
        let sid_c_0 = SessionId::from_bytes([0xC1; 16]);
        let sid_c_1 = SessionId::from_bytes([0xC2; 16]);

        multi
            .install_p2p_session(sid_b, "peer-b-AbCd0002-0".into(), peer_b)
            .expect("install B");
        multi
            .install_p2p_session(sid_c_0, "peer-c-AbCd0003-0".into(), peer_c_0)
            .expect("install C replica 0");
        multi
            .install_p2p_session(sid_c_1, "peer-c-AbCd0003-1".into(), peer_c_1)
            .expect("install C replica 1");

        let all_candidates = multi.p2p_candidate_paths();
        assert_eq!(all_candidates.len(), 3);
        let b_rtt = all_candidates
            .iter()
            .find(|candidate| candidate.session_id == sid_b)
            .expect("B candidate")
            .session
            .stats()
            .rtt;
        assert!(
            all_candidates
                .iter()
                .filter(|candidate| candidate.peer_family == "peer-c-AbCd0003-0")
                .all(|candidate| candidate.session.stats().rtt < b_rtt),
            "both C sessions are deliberately faster than B"
        );
        let candidates = multi.candidate_paths_for_peer("peer-b-AbCd0002-0");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, sid_b);
        assert_eq!(candidates[0].peer_family, "peer-b-AbCd0002-0");
    }

    #[tokio::test]
    async fn candidate_paths_for_peer_groups_different_replica_ids_in_one_family() {
        let multi = MultiSession::new_with_relay_only(channel_session());
        let sid_b_0 = SessionId::from_bytes([0xB3; 16]);
        let sid_b_1 = SessionId::from_bytes([0xB4; 16]);
        let sid_c = SessionId::from_bytes([0xC3; 16]);

        multi
            .install_p2p_session(sid_b_0, "peer-b-AbCd0002-0".into(), channel_session())
            .expect("install B replica 0");
        multi
            .install_p2p_session(sid_b_1, "peer-b-AbCd0002-1".into(), channel_session())
            .expect("install B replica 1");
        multi
            .install_p2p_session(sid_c, "peer-c-AbCd0003-0".into(), channel_session())
            .expect("install C");

        let candidates = multi.candidate_paths_for_peer("peer-b-AbCd0002-7");
        assert_eq!(candidates.len(), 2);
        assert!(candidates
            .iter()
            .all(|candidate| candidate.peer_family == "peer-b-AbCd0002-0"));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.session_id == sid_b_0));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.session_id == sid_b_1));
    }

    #[tokio::test]
    async fn closing_one_peer_family_keeps_other_peer_healthy_and_selectable() {
        let multi = MultiSession::new_with_relay_only(channel_session());
        let peer_b_0 = channel_session();
        let peer_b_1 = channel_session();
        let peer_c = channel_session_with_stats(stats(7, 0.0, 0));
        let sid_b_0 = SessionId::from_bytes([0xB5; 16]);
        let sid_b_1 = SessionId::from_bytes([0xB6; 16]);
        let sid_c = SessionId::from_bytes([0xC5; 16]);

        multi
            .install_p2p_session(sid_b_0, "peer-b-AbCd0002-0".into(), peer_b_0)
            .expect("install B replica 0");
        multi
            .install_p2p_session(sid_b_1, "peer-b-AbCd0002-1".into(), peer_b_1)
            .expect("install B replica 1");
        multi
            .install_p2p_session(sid_c, "peer-c-AbCd0003-0".into(), peer_c.clone())
            .expect("install C");

        let closed = multi.close_p2p_sessions_for_peer("peer-b-AbCd0002-7");

        assert_eq!(closed, 2);
        assert!(multi
            .candidate_paths_for_peer("peer-b-AbCd0002-0")
            .is_empty());
        let remaining_c = multi.candidate_paths_for_peer("peer-c-AbCd0003-0");
        assert_eq!(remaining_c.len(), 1);
        assert_eq!(remaining_c[0].session_id, sid_c);
        assert!(Arc::ptr_eq(&remaining_c[0].session, &peer_c));
        assert_eq!(multi.p2p_session_count(), 1);
        assert_eq!(multi.p2p_eligible_session_count(), 1);
    }

    #[tokio::test]
    async fn tcp_flow_stream_guard_decrements_active_count_on_drop() {
        let multi = MultiSession::new_with_relay_only(channel_session());
        let active = multi.active_tcp_flow_streams();

        {
            let _guard = multi.begin_tcp_flow_stream();
            assert_eq!(active.load(Ordering::SeqCst), 1);
        }

        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "dropping the guard must balance the active TCP stream counter"
        );
    }

    #[tokio::test]
    async fn pending_udp_inbound_caps_unknown_conn_ids() {
        let multi = MultiSession::new_with_relay_only(channel_session());

        for idx in 0..PENDING_UDP_INBOUND_CONN_CAP {
            assert_eq!(
                multi.buffer_pending_udp_inbound(
                    &format!("udp-{idx}"),
                    TrafficPath::Relay,
                    Bytes::from_static(b"x"),
                ),
                PendingUdpInboundBufferResult::Buffered {
                    dropped_oldest: false
                }
            );
        }

        assert_eq!(
            multi.buffer_pending_udp_inbound(
                "udp-over-cap",
                TrafficPath::Relay,
                Bytes::from_static(b"x"),
            ),
            PendingUdpInboundBufferResult::DroppedConnCap,
            "random unknown conn_ids must not grow the pending UDP buffer without bound"
        );
        assert!(multi.drain_pending_udp_inbound("udp-over-cap").is_empty());
    }

    #[tokio::test]
    async fn pending_udp_inbound_preserves_path_and_drops_oldest_per_conn() {
        let multi = MultiSession::new_with_relay_only(channel_session());

        for idx in 0..PENDING_UDP_INBOUND_PER_CONN_CAP {
            assert_eq!(
                multi.buffer_pending_udp_inbound(
                    "udp-race",
                    TrafficPath::Relay,
                    Bytes::from(vec![idx as u8]),
                ),
                PendingUdpInboundBufferResult::Buffered {
                    dropped_oldest: false
                }
            );
        }
        assert_eq!(
            multi.buffer_pending_udp_inbound(
                "udp-race",
                TrafficPath::P2p,
                Bytes::from_static(b"new"),
            ),
            PendingUdpInboundBufferResult::Buffered {
                dropped_oldest: true
            }
        );

        let drained = multi.drain_pending_udp_inbound("udp-race");
        assert_eq!(drained.len(), PENDING_UDP_INBOUND_PER_CONN_CAP);
        assert_eq!(drained[0].payload, Bytes::from(vec![1u8]));
        assert_eq!(
            drained.last().expect("last pending datagram"),
            &PendingUdpInbound {
                path: TrafficPath::P2p,
                payload: Bytes::from_static(b"new"),
            }
        );
    }

    /// A `set_p2p(Some) → set_p2p(None)` cycle that never reached
    /// `Active` must NOT emit a session-duration sample. The lifetime
    /// of a failed attempt is not a meaningful "session duration" — it
    /// would skew the dashboard with sub-second values.
    #[tokio::test]
    async fn set_p2p_skips_duration_when_never_active() {
        let relay = channel_session();
        let p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));

        let session_id = SessionId::from_bytes([2u8; 16]);
        multi.set_state(P2pState::Negotiating { session_id });
        multi.set_p2p(Some(p2p));
        // State stays at HandshakingQuic — never reached Active.
        multi.set_state(P2pState::HandshakingQuic { session_id });
        multi.set_p2p(None);

        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_session_duration_seconds_count 0"),
            "duration must remain 0 when Active was never reached; got:\n{text}"
        );
    }

    #[tokio::test]
    async fn clearing_active_p2p_session_returns_state_to_idle() {
        let relay = channel_session();
        let p2p = channel_session();
        let multi = MultiSession::new_with_relay_only(relay);
        let session_id = SessionId::from_bytes([3u8; 16]);

        multi.set_p2p(Some(p2p));
        multi.set_state(P2pState::Active {
            session_id,
            since: Instant::now(),
        });

        multi.set_p2p(None);

        assert!(
            matches!(multi.p2p_state(), P2pState::Idle),
            "clearing an active direct session must make it eligible for reannounce, got {:?}",
            multi.p2p_state()
        );
    }
}
