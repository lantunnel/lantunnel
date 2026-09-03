use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use tp_core::p2p_types::SessionId;
use tp_transport::session::SessionQueueSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FlowKind {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CandidatePath {
    Relay,
    P2p,
}

impl CandidatePath {
    fn as_str(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            Self::P2p => "p2p",
        }
    }
}

impl From<crate::p2p::scheduler::PathKind> for CandidatePath {
    fn from(value: crate::p2p::scheduler::PathKind) -> Self {
        match value {
            crate::p2p::scheduler::PathKind::Relay => Self::Relay,
            crate::p2p::scheduler::PathKind::P2p => Self::P2p,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CandidateKey {
    pub local_client_id: String,
    pub path: CandidatePath,
    pub p2p_session_id: Option<SessionId>,
    pub peer_client_id: Option<String>,
    pub peer_family: Option<String>,
    pub transport_generation: u64,
}

impl Ord for CandidateKey {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (
            &self.local_client_id,
            self.path,
            self.p2p_session_id.map(|id| *id.as_bytes()),
            &self.peer_client_id,
            &self.peer_family,
            self.transport_generation,
        )
            .cmp(&(
                &other.local_client_id,
                other.path,
                other.p2p_session_id.map(|id| *id.as_bytes()),
                &other.peer_client_id,
                &other.peer_family,
                other.transport_generation,
            ))
    }
}

impl PartialOrd for CandidateKey {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl CandidateKey {
    pub fn relay(local_client_id: impl Into<String>, transport_generation: u64) -> Self {
        Self {
            local_client_id: local_client_id.into(),
            path: CandidatePath::Relay,
            p2p_session_id: None,
            peer_client_id: None,
            peer_family: None,
            transport_generation,
        }
    }

    pub fn relay_to_peer(
        local_client_id: impl Into<String>,
        transport_generation: u64,
        peer_client_id: impl Into<String>,
    ) -> Self {
        let peer_client_id = peer_client_id.into();
        Self {
            local_client_id: local_client_id.into(),
            path: CandidatePath::Relay,
            p2p_session_id: None,
            peer_family: Some(crate::p2p::replica::replica_family_id(&peer_client_id)),
            peer_client_id: Some(peer_client_id),
            transport_generation,
        }
    }

    pub fn p2p(
        local_client_id: impl Into<String>,
        p2p_session_id: SessionId,
        peer_client_id: impl Into<String>,
        transport_generation: u64,
    ) -> Self {
        let peer_client_id = peer_client_id.into();
        Self {
            local_client_id: local_client_id.into(),
            path: CandidatePath::P2p,
            p2p_session_id: Some(p2p_session_id),
            peer_family: Some(crate::p2p::replica::replica_family_id(&peer_client_id)),
            peer_client_id: Some(peer_client_id),
            transport_generation,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaneLoadSnapshot {
    pub active_tcp: usize,
    pub active_udp: usize,
    pub tx_mbps_ewma: f64,
    pub udp_tx_mbps_ewma: f64,
    pub stream_queue_used_ratio: Option<f64>,
    pub datagram_send_buffer_space_ratio: Option<f64>,
    pub recent_udp_dropped_delta: u64,
}

impl LaneLoadSnapshot {
    pub fn with_queue_snapshot(
        mut self,
        snapshot: &SessionQueueSnapshot,
        recent_udp_dropped_delta: u64,
    ) -> Self {
        self.stream_queue_used_ratio = snapshot.stream_queue_used_ratio();
        self.datagram_send_buffer_space_ratio = snapshot.datagram_send_buffer_space_ratio();
        self.recent_udp_dropped_delta = recent_udp_dropped_delta;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaneScoreBreakdown {
    pub active_tcp_cost: f64,
    pub active_udp_cost: f64,
    pub bandwidth_cost: f64,
    pub stream_pressure_cost: f64,
    pub datagram_pressure_cost: f64,
    pub recent_udp_drop_cost: f64,
    pub attempt_cost: f64,
    pub total_score: f64,
}

impl LaneScoreBreakdown {
    pub fn for_load(
        flow_kind: FlowKind,
        load: &LaneLoadSnapshot,
        excluded_reason: PlacementExcludedReason,
    ) -> Self {
        let attempt_cost = match excluded_reason {
            PlacementExcludedReason::None => 0.0,
            PlacementExcludedReason::AttemptTimeout => f64::INFINITY,
        };
        let mut out = match flow_kind {
            FlowKind::Tcp => Self {
                active_tcp_cost: load.active_tcp as f64 * 100.0,
                active_udp_cost: load.active_udp as f64 * 200.0,
                bandwidth_cost: (load.tx_mbps_ewma * 2.0).min(400.0),
                stream_pressure_cost: stream_pressure_cost(load.stream_queue_used_ratio),
                attempt_cost,
                ..Self::default()
            },
            FlowKind::Udp => Self {
                active_tcp_cost: load.active_tcp as f64 * 25.0,
                active_udp_cost: load.active_udp as f64 * 1000.0,
                bandwidth_cost: (load.udp_tx_mbps_ewma * 4.0).min(400.0),
                datagram_pressure_cost: datagram_pressure_cost(
                    load.datagram_send_buffer_space_ratio,
                ),
                recent_udp_drop_cost: (load.recent_udp_dropped_delta as f64 * 10.0).min(500.0),
                attempt_cost,
                ..Self::default()
            },
        };
        out.total_score = out.active_tcp_cost
            + out.active_udp_cost
            + out.bandwidth_cost
            + out.stream_pressure_cost
            + out.datagram_pressure_cost
            + out.recent_udp_drop_cost
            + out.attempt_cost;
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementExcludedReason {
    None,
    AttemptTimeout,
}

impl PlacementExcludedReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AttemptTimeout => "attempt_timeout",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlacementCandidate {
    pub key: CandidateKey,
    pub load: LaneLoadSnapshot,
    pub excluded_reason: PlacementExcludedReason,
}

#[derive(Clone, Debug)]
pub struct PlacementDecisionRecord {
    pub decision_id: u64,
    pub flow_kind: FlowKind,
    pub key: CandidateKey,
    pub load: LaneLoadSnapshot,
    pub breakdown: LaneScoreBreakdown,
    pub selected: bool,
    pub excluded_reason: PlacementExcludedReason,
}

#[derive(Clone, Debug)]
pub struct PlacementDecision {
    pub decision_id: u64,
    pub selected: Option<CandidateKey>,
    pub records: Vec<PlacementDecisionRecord>,
}

#[derive(Default)]
pub struct FlowPlacementRegistry {
    placements: Mutex<HashMap<String, FlowPlacementEntry>>,
    traffic: Mutex<HashMap<CandidateKey, CandidateTraffic>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayAttachmentSnapshot {
    pub active_tcp: usize,
    pub active_udp: usize,
    pub last_link_io_progress_ms: u64,
}

#[derive(Clone, Debug)]
struct FlowPlacementEntry {
    kind: FlowKind,
    key: CandidateKey,
    state: FlowPlacementState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlowPlacementState {
    Pending,
    Established,
}

#[derive(Clone, Debug, Default)]
struct CandidateTraffic {
    total_payload_bytes: u64,
    udp_payload_bytes: u64,
    last_link_io_progress_ms: u64,
    sample: Option<EwmaSample>,
}

#[derive(Clone, Copy, Debug)]
struct EwmaSample {
    at: Instant,
    total_payload_bytes: u64,
    udp_payload_bytes: u64,
    ewma_initialized: bool,
    tx_mbps_ewma: f64,
    udp_tx_mbps_ewma: f64,
}

impl FlowPlacementRegistry {
    pub fn record_pending(&self, conn_id: impl Into<String>, kind: FlowKind, key: CandidateKey) {
        self.placements.lock().insert(
            conn_id.into(),
            FlowPlacementEntry {
                kind,
                key,
                state: FlowPlacementState::Pending,
            },
        );
    }

    pub fn mark_established(&self, conn_id: &str) {
        if let Some(entry) = self.placements.lock().get_mut(conn_id) {
            entry.state = FlowPlacementState::Established;
        }
    }

    pub fn replace(&self, conn_id: &str, kind: FlowKind, key: CandidateKey) {
        if let Some(entry) = self.placements.lock().get_mut(conn_id) {
            entry.kind = kind;
            entry.key = key;
        }
    }

    pub fn remove(&self, conn_id: &str) {
        self.placements.lock().remove(conn_id);
    }

    pub fn candidate_key(&self, conn_id: &str) -> Option<CandidateKey> {
        self.placements
            .lock()
            .get(conn_id)
            .map(|entry| entry.key.clone())
    }

    pub fn clear(&self) {
        self.placements.lock().clear();
        self.traffic.lock().clear();
    }

    /// Drop state owned by a dead Gateway attachment while retaining Direct
    /// flows that are still carried by an independently healthy P2P session.
    pub fn clear_relay(&self) {
        self.placements
            .lock()
            .retain(|_, entry| entry.key.path == CandidatePath::P2p);
        self.traffic
            .lock()
            .retain(|key, _| key.path == CandidatePath::P2p);
    }

    pub fn lane_load_snapshot(&self, key: &CandidateKey) -> LaneLoadSnapshot {
        let (active_tcp, active_udp) = self.active_counts(key);
        let (tx_mbps_ewma, udp_tx_mbps_ewma) = self.current_ewma(key);
        LaneLoadSnapshot {
            active_tcp,
            active_udp,
            tx_mbps_ewma,
            udp_tx_mbps_ewma,
            ..LaneLoadSnapshot::default()
        }
    }

    pub fn active_counts_for_candidate(&self, key: &CandidateKey) -> (usize, usize) {
        self.active_counts(key)
    }

    pub fn relay_attachment_snapshot(
        &self,
        local_client_id: &str,
        transport_generation: u64,
    ) -> RelayAttachmentSnapshot {
        let belongs_to_attachment = |key: &CandidateKey| {
            key.path == CandidatePath::Relay
                && key.local_client_id == local_client_id
                && key.transport_generation == transport_generation
        };

        let mut snapshot = RelayAttachmentSnapshot::default();
        for entry in self.placements.lock().values() {
            if !belongs_to_attachment(&entry.key) {
                continue;
            }
            match entry.kind {
                FlowKind::Tcp => snapshot.active_tcp += 1,
                FlowKind::Udp => snapshot.active_udp += 1,
            }
        }
        for (key, traffic) in self.traffic.lock().iter() {
            if belongs_to_attachment(key) {
                snapshot.last_link_io_progress_ms = snapshot
                    .last_link_io_progress_ms
                    .max(traffic.last_link_io_progress_ms);
            }
        }
        snapshot
    }

    pub fn record_outbound_payload_bytes(
        &self,
        key: &CandidateKey,
        kind: FlowKind,
        payload_bytes: u64,
    ) {
        let mut traffic = self.traffic.lock();
        let entry = traffic.entry(key.clone()).or_default();
        entry.total_payload_bytes = entry.total_payload_bytes.saturating_add(payload_bytes);
        if kind == FlowKind::Udp {
            entry.udp_payload_bytes = entry.udp_payload_bytes.saturating_add(payload_bytes);
        }
    }

    pub fn record_link_io_progress_ms(&self, key: &CandidateKey, now_ms: u64) {
        let mut traffic = self.traffic.lock();
        let entry = traffic.entry(key.clone()).or_default();
        entry.last_link_io_progress_ms = entry.last_link_io_progress_ms.max(now_ms);
    }

    pub fn last_link_io_progress_ms_for_candidate(&self, key: &CandidateKey) -> u64 {
        self.traffic
            .lock()
            .get(key)
            .map(|entry| entry.last_link_io_progress_ms)
            .unwrap_or(0)
    }

    pub fn sample_ewma_at(&self, key: &CandidateKey, now: Instant) -> (f64, f64) {
        let mut traffic = self.traffic.lock();
        let entry = traffic.entry(key.clone()).or_default();
        let Some(previous) = entry.sample else {
            entry.sample = Some(EwmaSample {
                at: now,
                total_payload_bytes: entry.total_payload_bytes,
                udp_payload_bytes: entry.udp_payload_bytes,
                ewma_initialized: false,
                tx_mbps_ewma: 0.0,
                udp_tx_mbps_ewma: 0.0,
            });
            return (0.0, 0.0);
        };

        let dt = now.duration_since(previous.at).as_secs_f64();
        if dt <= 0.0 {
            return (previous.tx_mbps_ewma, previous.udp_tx_mbps_ewma);
        }

        let total_delta = entry
            .total_payload_bytes
            .saturating_sub(previous.total_payload_bytes);
        let udp_delta = entry
            .udp_payload_bytes
            .saturating_sub(previous.udp_payload_bytes);
        let alpha = 1.0 - (-dt / 3.0).exp();
        let tx_instant = mbps(total_delta, dt);
        let udp_instant = mbps(udp_delta, dt);
        let (tx_mbps_ewma, udp_tx_mbps_ewma) = if previous.ewma_initialized {
            (
                previous.tx_mbps_ewma + alpha * (tx_instant - previous.tx_mbps_ewma),
                previous.udp_tx_mbps_ewma + alpha * (udp_instant - previous.udp_tx_mbps_ewma),
            )
        } else {
            (tx_instant, udp_instant)
        };
        entry.sample = Some(EwmaSample {
            at: now,
            total_payload_bytes: entry.total_payload_bytes,
            udp_payload_bytes: entry.udp_payload_bytes,
            ewma_initialized: true,
            tx_mbps_ewma,
            udp_tx_mbps_ewma,
        });
        (tx_mbps_ewma, udp_tx_mbps_ewma)
    }

    fn active_counts(&self, key: &CandidateKey) -> (usize, usize) {
        let mut active_tcp = 0;
        let mut active_udp = 0;
        for entry in self.placements.lock().values() {
            if &entry.key != key {
                continue;
            }
            match entry.kind {
                FlowKind::Tcp => active_tcp += 1,
                FlowKind::Udp => active_udp += 1,
            }
        }
        (active_tcp, active_udp)
    }

    fn current_ewma(&self, key: &CandidateKey) -> (f64, f64) {
        self.traffic
            .lock()
            .get(key)
            .and_then(|entry| entry.sample)
            .map(|sample| (sample.tx_mbps_ewma, sample.udp_tx_mbps_ewma))
            .unwrap_or((0.0, 0.0))
    }
}

pub struct ReplicaFlowScheduler {
    next_decision_id: AtomicU64,
    p2p_tcp_rr: AtomicUsize,
    p2p_udp_rr: AtomicUsize,
    relay_tcp_rr: AtomicUsize,
    relay_udp_rr: AtomicUsize,
}

impl Default for ReplicaFlowScheduler {
    fn default() -> Self {
        Self {
            next_decision_id: AtomicU64::new(1),
            p2p_tcp_rr: AtomicUsize::new(0),
            p2p_udp_rr: AtomicUsize::new(0),
            relay_tcp_rr: AtomicUsize::new(0),
            relay_udp_rr: AtomicUsize::new(0),
        }
    }
}

impl ReplicaFlowScheduler {
    pub fn place_proxy_flow(
        &self,
        flow_kind: FlowKind,
        registry: &FlowPlacementRegistry,
        candidates: Vec<PlacementCandidate>,
    ) -> PlacementDecision {
        let mut p2p_candidates = Vec::new();
        let mut relay_candidates = Vec::new();
        for candidate in candidates {
            match candidate.key.path {
                CandidatePath::P2p => p2p_candidates.push(candidate),
                CandidatePath::Relay => relay_candidates.push(candidate),
            }
        }

        if p2p_candidates
            .iter()
            .any(|candidate| candidate.excluded_reason == PlacementExcludedReason::None)
        {
            return self.place_candidates_with_tie_break(
                flow_kind,
                CandidatePath::P2p,
                registry,
                p2p_candidates,
            );
        }
        self.place_candidates_with_tie_break(
            flow_kind,
            CandidatePath::Relay,
            registry,
            relay_candidates,
        )
    }

    pub fn place_flow(
        &self,
        flow_kind: FlowKind,
        registry: &FlowPlacementRegistry,
        candidates: Vec<PlacementCandidate>,
    ) -> PlacementDecision {
        let decision_id = self.next_decision_id.fetch_add(1, Ordering::Relaxed);
        let mut selected = None;
        let mut selected_score = f64::INFINITY;
        let mut records: Vec<PlacementDecisionRecord> = candidates
            .into_iter()
            .map(|candidate| {
                let mut load = registry.lane_load_snapshot(&candidate.key);
                load.stream_queue_used_ratio = candidate.load.stream_queue_used_ratio;
                load.datagram_send_buffer_space_ratio =
                    candidate.load.datagram_send_buffer_space_ratio;
                load.recent_udp_dropped_delta = candidate.load.recent_udp_dropped_delta;
                let breakdown =
                    LaneScoreBreakdown::for_load(flow_kind, &load, candidate.excluded_reason);
                if candidate.excluded_reason == PlacementExcludedReason::None
                    && breakdown.total_score < selected_score
                {
                    selected_score = breakdown.total_score;
                    selected = Some(candidate.key.clone());
                }
                PlacementDecisionRecord {
                    decision_id,
                    flow_kind,
                    key: candidate.key,
                    load,
                    breakdown,
                    selected: false,
                    excluded_reason: candidate.excluded_reason,
                }
            })
            .collect();
        for record in &mut records {
            record.selected = selected.as_ref() == Some(&record.key);
            log_score_record(record);
        }
        PlacementDecision {
            decision_id,
            selected,
            records,
        }
    }

    fn place_candidates_with_tie_break(
        &self,
        flow_kind: FlowKind,
        path: CandidatePath,
        registry: &FlowPlacementRegistry,
        mut candidates: Vec<PlacementCandidate>,
    ) -> PlacementDecision {
        candidates.sort_by(|a, b| a.key.cmp(&b.key));
        let decision_id = self.next_decision_id.fetch_add(1, Ordering::Relaxed);
        let mut records: Vec<PlacementDecisionRecord> = candidates
            .into_iter()
            .map(|candidate| {
                let mut load = registry.lane_load_snapshot(&candidate.key);
                load.stream_queue_used_ratio = candidate.load.stream_queue_used_ratio;
                load.datagram_send_buffer_space_ratio =
                    candidate.load.datagram_send_buffer_space_ratio;
                load.recent_udp_dropped_delta = candidate.load.recent_udp_dropped_delta;
                let breakdown =
                    LaneScoreBreakdown::for_load(flow_kind, &load, candidate.excluded_reason);
                PlacementDecisionRecord {
                    decision_id,
                    flow_kind,
                    key: candidate.key,
                    load,
                    breakdown,
                    selected: false,
                    excluded_reason: candidate.excluded_reason,
                }
            })
            .collect();
        let selected = select_lowest_score_with_rr(&records, self.tie_cursor(path, flow_kind));
        for record in &mut records {
            record.selected = selected.as_ref() == Some(&record.key);
            log_score_record(record);
        }
        PlacementDecision {
            decision_id,
            selected,
            records,
        }
    }

    fn tie_cursor(&self, path: CandidatePath, flow_kind: FlowKind) -> &AtomicUsize {
        match (path, flow_kind) {
            (CandidatePath::P2p, FlowKind::Tcp) => &self.p2p_tcp_rr,
            (CandidatePath::P2p, FlowKind::Udp) => &self.p2p_udp_rr,
            (CandidatePath::Relay, FlowKind::Tcp) => &self.relay_tcp_rr,
            (CandidatePath::Relay, FlowKind::Udp) => &self.relay_udp_rr,
        }
    }
}

fn select_lowest_score_with_rr(
    records: &[PlacementDecisionRecord],
    tie_cursor: &AtomicUsize,
) -> Option<CandidateKey> {
    let min_score = records
        .iter()
        .filter(|record| record.excluded_reason == PlacementExcludedReason::None)
        .map(|record| record.breakdown.total_score)
        .min_by(|a, b| a.total_cmp(b))?;
    let tied: Vec<&PlacementDecisionRecord> = records
        .iter()
        .filter(|record| {
            record.excluded_reason == PlacementExcludedReason::None
                && record.breakdown.total_score == min_score
        })
        .collect();
    if tied.is_empty() {
        return None;
    }
    let idx = if tied.len() == 1 {
        0
    } else {
        tie_cursor.fetch_add(1, Ordering::Relaxed) % tied.len()
    };
    Some(tied[idx].key.clone())
}

fn log_score_record(record: &PlacementDecisionRecord) {
    tracing::info!(
        decision_id = record.decision_id,
        network = flow_kind_label(record.flow_kind),
        candidate_class = record.key.path.as_str(),
        local_client_id = %record.key.local_client_id,
        path = record.key.path.as_str(),
        p2p_session_id = ?record.key.p2p_session_id,
        peer_client_id = record.key.peer_client_id.as_deref().unwrap_or(""),
        peer_family = record.key.peer_family.as_deref().unwrap_or(""),
        score = record.breakdown.total_score,
        active_tcp = record.load.active_tcp,
        active_udp = record.load.active_udp,
        tx_mbps_ewma = record.load.tx_mbps_ewma,
        udp_tx_mbps_ewma = record.load.udp_tx_mbps_ewma,
        stream_pressure_cost = record.breakdown.stream_pressure_cost,
        datagram_pressure_cost = record.breakdown.datagram_pressure_cost,
        recent_udp_drop_cost = record.breakdown.recent_udp_drop_cost,
        attempt_penalty = record.breakdown.attempt_cost,
        selected = record.selected,
        excluded_reason = record.excluded_reason.as_str(),
        "selected replica lane for flow"
    );
}

fn flow_kind_label(kind: FlowKind) -> &'static str {
    match kind {
        FlowKind::Tcp => "tcp",
        FlowKind::Udp => "udp",
    }
}

fn stream_pressure_cost(ratio: Option<f64>) -> f64 {
    match ratio {
        Some(ratio) if ratio > 0.90 => 300.0,
        Some(ratio) if ratio > 0.75 => 150.0,
        _ => 0.0,
    }
}

fn datagram_pressure_cost(ratio: Option<f64>) -> f64 {
    match ratio {
        Some(0.0) => 600.0,
        Some(ratio) if ratio < 0.10 => 300.0,
        Some(ratio) if ratio < 0.25 => 100.0,
        _ => 0.0,
    }
}

fn mbps(bytes: u64, dt_seconds: f64) -> f64 {
    if dt_seconds <= 0.0 {
        return 0.0;
    }
    bytes as f64 * 8.0 / dt_seconds / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn sid(byte: u8) -> SessionId {
        SessionId::from_bytes([byte; 16])
    }

    fn candidate(byte: u8, local_client_id: &str) -> CandidateKey {
        CandidateKey::p2p(local_client_id, sid(byte), format!("pc-{byte}"), 1)
    }

    fn placement_candidate(
        key: &CandidateKey,
        registry: &FlowPlacementRegistry,
    ) -> PlacementCandidate {
        PlacementCandidate {
            key: key.clone(),
            load: registry.lane_load_snapshot(key),
            excluded_reason: PlacementExcludedReason::None,
        }
    }

    #[test]
    fn flow_scheduler_udp_score_spreads_large_udp_associations() {
        let registry = FlowPlacementRegistry::default();
        let busy = candidate(0x01, "replica-a");
        let idle = candidate(0x02, "replica-b");
        registry.record_pending("udp-existing", FlowKind::Udp, busy.clone());
        registry.mark_established("udp-existing");

        let decision = ReplicaFlowScheduler::default().place_flow(
            FlowKind::Udp,
            &registry,
            vec![
                placement_candidate(&busy, &registry),
                placement_candidate(&idle, &registry),
            ],
        );

        assert_eq!(
            decision.selected,
            Some(idle),
            "a new UDP association should avoid a candidate with an active UDP flow"
        );
    }

    #[test]
    fn flow_scheduler_tcp_score_counts_active_tcp_and_udp() {
        let registry = FlowPlacementRegistry::default();
        let key = candidate(0x03, "replica-a");
        let same_multi_different_p2p = candidate(0x04, "replica-a");
        registry.record_pending("tcp-existing", FlowKind::Tcp, key.clone());
        registry.mark_established("tcp-existing");
        registry.record_pending("udp-existing", FlowKind::Udp, key.clone());
        registry.mark_established("udp-existing");
        registry.record_pending(
            "udp-other-p2p-session",
            FlowKind::Udp,
            same_multi_different_p2p,
        );
        registry.mark_established("udp-other-p2p-session");

        let load = registry.lane_load_snapshot(&key);
        let decision = ReplicaFlowScheduler::default().place_flow(
            FlowKind::Tcp,
            &registry,
            vec![PlacementCandidate {
                key: key.clone(),
                load,
                excluded_reason: PlacementExcludedReason::None,
            }],
        );
        let record = decision.records.first().expect("one score record");

        assert_eq!(record.breakdown.active_tcp_cost, 100.0);
        assert_eq!(record.breakdown.active_udp_cost, 200.0);
        assert_eq!(
            record.breakdown.total_score, 300.0,
            "TCP score must count exact-key active TCP and UDP flows, not every flow on the MultiSession"
        );
    }

    #[test]
    fn flow_scheduler_logs_score_components() {
        let registry = FlowPlacementRegistry::default();
        let selected = candidate(0x05, "replica-a");
        let excluded = candidate(0x06, "replica-b");
        registry.record_pending("tcp-existing", FlowKind::Tcp, selected.clone());
        registry.mark_established("tcp-existing");

        let decision = ReplicaFlowScheduler::default().place_flow(
            FlowKind::Udp,
            &registry,
            vec![
                placement_candidate(&selected, &registry),
                PlacementCandidate {
                    key: excluded.clone(),
                    load: registry.lane_load_snapshot(&excluded),
                    excluded_reason: PlacementExcludedReason::AttemptTimeout,
                },
            ],
        );

        assert_eq!(decision.records.len(), 2);
        assert!(
            decision
                .records
                .iter()
                .all(|record| record.decision_id == decision.decision_id),
            "all candidate score lines must share one decision_id"
        );
        let scored = decision
            .records
            .iter()
            .find(|record| record.key == selected)
            .expect("selected candidate scored");
        assert_eq!(scored.flow_kind, FlowKind::Udp);
        assert_eq!(scored.breakdown.active_tcp_cost, 25.0);
        assert_eq!(scored.breakdown.datagram_pressure_cost, 0.0);
        assert_eq!(scored.breakdown.recent_udp_drop_cost, 0.0);
        assert!(scored.selected);

        let excluded_record = decision
            .records
            .iter()
            .find(|record| record.key == excluded)
            .expect("excluded candidate logged");
        assert_eq!(
            excluded_record.excluded_reason,
            PlacementExcludedReason::AttemptTimeout
        );
        assert!(!excluded_record.selected);
    }

    #[test]
    fn flow_scheduler_uses_ewma_not_cumulative_bytes() {
        let registry = FlowPlacementRegistry::default();
        let key = candidate(0x07, "replica-a");
        let start = Instant::now();
        registry.sample_ewma_at(&key, start);

        registry.record_outbound_payload_bytes(&key, FlowKind::Tcp, 3_000_000);
        let (first_tx_mbps, _) = registry.sample_ewma_at(&key, start + Duration::from_secs(3));
        let (second_tx_mbps, _) = registry.sample_ewma_at(&key, start + Duration::from_secs(6));

        assert_eq!(first_tx_mbps, 8.0);
        assert!(
            second_tx_mbps < first_tx_mbps,
            "the second sample has no new bytes and must decay instead of reusing cumulative bytes as the delta"
        );
    }

    #[test]
    fn flow_scheduler_initializes_udp_ewma_from_first_real_sample() {
        let registry = FlowPlacementRegistry::default();
        let key = candidate(0x08, "replica-a");
        let start = Instant::now();
        registry.sample_ewma_at(&key, start);

        registry.record_outbound_payload_bytes(&key, FlowKind::Udp, 1_500_000);
        let (first_tx_mbps, first_udp_mbps) =
            registry.sample_ewma_at(&key, start + Duration::from_secs(3));

        assert_eq!(first_tx_mbps, 4.0);
        assert_eq!(first_udp_mbps, 4.0);
    }

    #[test]
    fn flow_registry_tracks_link_io_progress_per_candidate() {
        let registry = FlowPlacementRegistry::default();
        let p2p = candidate(0x0d, "replica-a");
        let relay = relay_candidate("replica-a");

        registry.record_outbound_payload_bytes(&p2p, FlowKind::Tcp, 1024);
        assert_eq!(
            registry.last_link_io_progress_ms_for_candidate(&p2p),
            0,
            "local payload accounting alone is not link I/O progress"
        );

        registry.record_link_io_progress_ms(&p2p, 1200);
        registry.record_link_io_progress_ms(&relay, 2200);
        registry.record_link_io_progress_ms(&p2p, 1100);

        assert_eq!(registry.last_link_io_progress_ms_for_candidate(&p2p), 1200);
        assert_eq!(
            registry.last_link_io_progress_ms_for_candidate(&relay),
            2200
        );
    }

    fn relay_candidate(local_client_id: &str) -> CandidateKey {
        CandidateKey::relay(local_client_id, 1)
    }

    fn scored_candidate(
        key: &CandidateKey,
        registry: &FlowPlacementRegistry,
    ) -> PlacementCandidate {
        PlacementCandidate {
            key: key.clone(),
            load: registry.lane_load_snapshot(key),
            excluded_reason: PlacementExcludedReason::None,
        }
    }

    fn excluded_candidate(
        key: &CandidateKey,
        registry: &FlowPlacementRegistry,
    ) -> PlacementCandidate {
        PlacementCandidate {
            key: key.clone(),
            load: registry.lane_load_snapshot(key),
            excluded_reason: PlacementExcludedReason::AttemptTimeout,
        }
    }

    #[test]
    fn replica_scheduler_prefers_live_p2p_candidates_before_relay() {
        let registry = FlowPlacementRegistry::default();
        let p2p = candidate(0x09, "replica-a");
        let relay = relay_candidate("replica-b");
        registry.record_pending("existing-udp", FlowKind::Udp, p2p.clone());
        registry.mark_established("existing-udp");

        let decision = ReplicaFlowScheduler::default().place_proxy_flow(
            FlowKind::Udp,
            &registry,
            vec![
                scored_candidate(&relay, &registry),
                scored_candidate(&p2p, &registry),
            ],
        );

        assert_eq!(decision.selected, Some(p2p.clone()));
        assert_eq!(decision.records.len(), 1);
        assert_eq!(decision.records[0].key, p2p);
    }

    #[test]
    fn replica_scheduler_uses_relay_only_when_no_live_p2p_candidate() {
        let registry = FlowPlacementRegistry::default();
        let p2p = candidate(0x0a, "replica-a");
        let relay = relay_candidate("replica-b");

        let decision = ReplicaFlowScheduler::default().place_proxy_flow(
            FlowKind::Tcp,
            &registry,
            vec![
                excluded_candidate(&p2p, &registry),
                scored_candidate(&relay, &registry),
            ],
        );

        assert_eq!(decision.selected, Some(relay.clone()));
        assert_eq!(decision.records.len(), 1);
        assert_eq!(decision.records[0].key, relay);
    }

    #[test]
    fn replica_scheduler_keeps_high_rtt_loss_p2p_eligible() {
        let registry = FlowPlacementRegistry::default();
        let p2p = candidate(0x0b, "replica-a");
        let relay = relay_candidate("replica-b");

        let decision = ReplicaFlowScheduler::default().place_proxy_flow(
            FlowKind::Tcp,
            &registry,
            vec![
                scored_candidate(&relay, &registry),
                scored_candidate(&p2p, &registry),
            ],
        );

        assert_eq!(
            decision.selected,
            Some(p2p),
            "proxy flow placement candidates do not carry RTT, loss, PTO, heartbeat, or idle state"
        );
    }

    #[test]
    fn replica_scheduler_attempt_exclude_is_local() {
        let registry = FlowPlacementRegistry::default();
        let excluded = candidate(0x0c, "replica-a");
        let same_session_other_local = CandidateKey::p2p(
            "replica-b",
            excluded.p2p_session_id.expect("p2p session id"),
            "pc-other",
            1,
        );
        let relay = relay_candidate("replica-c");

        let decision = ReplicaFlowScheduler::default().place_proxy_flow(
            FlowKind::Tcp,
            &registry,
            vec![
                excluded_candidate(&excluded, &registry),
                scored_candidate(&same_session_other_local, &registry),
                scored_candidate(&relay, &registry),
            ],
        );

        assert_eq!(decision.selected, Some(same_session_other_local.clone()));
        assert_eq!(decision.records.len(), 2);
        let excluded_record = decision
            .records
            .iter()
            .find(|record| record.key == excluded)
            .expect("excluded P2P candidate must be logged");
        assert_eq!(
            excluded_record.excluded_reason,
            PlacementExcludedReason::AttemptTimeout
        );
        assert!(!excluded_record.selected);
        let selected_record = decision
            .records
            .iter()
            .find(|record| record.key == same_session_other_local)
            .expect("selected P2P candidate must be logged");
        assert!(selected_record.selected);
        assert_eq!(
            decision
                .records
                .iter()
                .filter(|record| record.key.path == CandidatePath::Relay)
                .count(),
            0,
            "relay candidates must not be scored while an eligible P2P candidate exists"
        );
    }

    #[test]
    fn replica_scheduler_lowest_score_wins_inside_candidate_class() {
        let registry = FlowPlacementRegistry::default();
        let busy = candidate(0x0d, "replica-a");
        let idle = candidate(0x0e, "replica-b");
        registry.record_pending("existing-tcp", FlowKind::Tcp, busy.clone());
        registry.mark_established("existing-tcp");

        let decision = ReplicaFlowScheduler::default().place_proxy_flow(
            FlowKind::Tcp,
            &registry,
            vec![
                scored_candidate(&busy, &registry),
                scored_candidate(&idle, &registry),
            ],
        );

        assert_eq!(decision.selected, Some(idle));
    }

    #[test]
    fn replica_scheduler_tie_breaks_round_robin() {
        let registry = FlowPlacementRegistry::default();
        let first = candidate(0x0f, "replica-a");
        let second = candidate(0x10, "replica-b");
        let scheduler = ReplicaFlowScheduler::default();

        let decision_a = scheduler.place_proxy_flow(
            FlowKind::Udp,
            &registry,
            vec![
                scored_candidate(&second, &registry),
                scored_candidate(&first, &registry),
            ],
        );
        let decision_b = scheduler.place_proxy_flow(
            FlowKind::Udp,
            &registry,
            vec![
                scored_candidate(&second, &registry),
                scored_candidate(&first, &registry),
            ],
        );

        assert_eq!(decision_a.selected, Some(first));
        assert_eq!(decision_b.selected, Some(second));
    }

    #[test]
    fn relay_attachment_snapshot_aggregates_peer_scoped_exact_keys_only() {
        let registry = FlowPlacementRegistry::default();
        let peer_a = CandidateKey::relay_to_peer("local-a", 7, "peer-a");
        let peer_b = CandidateKey::relay_to_peer("local-a", 7, "peer-b");
        let wrong_generation = CandidateKey::relay_to_peer("local-a", 8, "peer-c");
        let wrong_local = CandidateKey::relay_to_peer("local-b", 7, "peer-d");
        let p2p = CandidateKey::p2p("local-a", sid(0x33), "peer-a", 7);

        registry.record_pending("relay-a-tcp", FlowKind::Tcp, peer_a.clone());
        registry.record_pending("relay-b-udp", FlowKind::Udp, peer_b.clone());
        registry.record_pending("other-generation", FlowKind::Tcp, wrong_generation.clone());
        registry.record_pending("other-local", FlowKind::Udp, wrong_local.clone());
        registry.record_pending("same-attachment-p2p", FlowKind::Tcp, p2p.clone());
        registry.record_link_io_progress_ms(&peer_a, 110);
        registry.record_link_io_progress_ms(&peer_b, 220);
        registry.record_link_io_progress_ms(&wrong_generation, 330);
        registry.record_link_io_progress_ms(&wrong_local, 440);
        registry.record_link_io_progress_ms(&p2p, 550);
        assert_eq!(
            registry.relay_attachment_snapshot("local-a", 7),
            RelayAttachmentSnapshot {
                active_tcp: 1,
                active_udp: 1,
                last_link_io_progress_ms: 220,
            }
        );
        assert_eq!(registry.active_counts_for_candidate(&peer_a), (1, 0));
        assert_eq!(registry.active_counts_for_candidate(&peer_b), (0, 1));
        assert_eq!(
            registry.active_counts_for_candidate(&CandidateKey::relay("local-a", 7)),
            (0, 0),
            "scheduler-facing exact-key counts must stay peer-scoped"
        );
    }
}
