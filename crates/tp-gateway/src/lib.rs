//! Gateway library: authenticates V2 Peer attachments and relays only across
//! routes bound to an exact Peer identity.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{Datelike, Utc};
use dashmap::{mapref::entry::Entry, DashMap};
use parking_lot::Mutex;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tp_core::bandwidth::BandwidthLimiter;
use tp_core::config::GatewayP2pConfig;
use tp_core::protocol::BinaryMessage;
use tp_metrics::MetricsManager;
use tp_transport::{
    AuthHandler, AuthParams, GrpcServer, QuicServer, Session, TcpFlowIncoming, TcpFlowStream,
    TrySendKind, WsServer,
};

const CLIENT_RELAY_BOUND_TTL: Duration = Duration::from_secs(15);
const TCP_FLOW_COPY_BUFFER_BYTES: usize = 64 * 1024;
const RELAY_BANDWIDTH_CHUNK: usize = 64 * 1024;
const RELAY_FLOW_READ_CHUNK: usize = 16 * 1024;
const DESTINATION_DIVERSITY_WINDOW: Duration = Duration::from_secs(3600);
const DESTINATION_DIVERSITY_WARN_CAP: usize = 1000;
const DESTINATION_DIVERSITY_HARD_CAP: usize = DESTINATION_DIVERSITY_WARN_CAP;
const V2_ATTACHMENT_AUTH_DEADLINE: Duration = Duration::from_secs(10);

mod client_conn;
pub mod datagram;
pub mod host_filter;
pub mod mapping_probe;
pub mod p2p;
pub mod scope;
pub mod tunneled;

use client_conn::AuthenticatedPeerV2;
use client_conn::ClientConn;
pub use datagram::{DatagramReceiver, DatagramSender, TunneledDatagram};
pub use tunneled::TunneledConn;

pub enum GatewayServer {
    Quic(QuicServer),
    WebSocket(WsServer),
    Grpc(GrpcServer),
}

type TunnelClientKey = (String, String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientRelayRouteKey {
    tunnel_id: String,
    conn_id: String,
}

impl ClientRelayRouteKey {
    fn new(source: &Arc<ClientConn>, conn_id: impl Into<String>) -> Self {
        Self {
            tunnel_id: source.params.tunnel_id.clone(),
            conn_id: conn_id.into(),
        }
    }
}

#[derive(Debug, Default)]
struct RelayQuotaState {
    tunnel_id: String,
    period: String,
    quota_bytes: u64,
    remaining_bytes: u64,
    last_platform_remaining_bytes: u64,
    pending_usage_by_period: BTreeMap<String, u64>,
    exhaustion_warning_emitted: bool,
}

#[derive(Debug, Default)]
pub(crate) struct RelayQuotaLimiter {
    state: Mutex<RelayQuotaState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUsageSnapshot {
    pub tunnel_id: String,
    pub period: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUsageItem {
    pub tunnel_id: String,
    pub period_yyyymm: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUsageBatchItem {
    pub seq: u64,
    pub tunnel_id: String,
    pub period_yyyymm: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUsageBatch {
    pub through_seq: u64,
    pub items: Vec<RelayUsageBatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RelayUsageWalRecord {
    Usage {
        seq: u64,
        tunnel_id: String,
        period_yyyymm: String,
        bytes: u64,
    },
    Ack {
        through_seq: u64,
    },
}

#[derive(Debug)]
struct RelayUsageWalState {
    next_seq: u64,
    ack_through: u64,
    unacked: BTreeMap<u64, RelayUsageItem>,
    file: File,
}

#[derive(Debug)]
pub struct RelayUsageWal {
    path: PathBuf,
    state: Mutex<RelayUsageWalState>,
}

impl RelayUsageWal {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut ack_through = 0u64;
        let mut max_seq = 0u64;
        let mut replayed = BTreeMap::<u64, RelayUsageItem>::new();
        if path.exists() {
            let mut contents = String::new();
            File::open(&path)?.read_to_string(&mut contents)?;
            let mut valid_end = 0usize;
            let mut repair_trailing_record = false;
            let mut append_missing_newline = false;
            let mut chunks = contents.split_inclusive('\n').peekable();
            while let Some(chunk) = chunks.next() {
                let has_newline = chunk.ends_with('\n');
                let line = chunk.trim_end_matches('\n').trim_end_matches('\r');
                if line.trim().is_empty() {
                    valid_end += chunk.len();
                    continue;
                }
                let record: RelayUsageWalRecord = match serde_json::from_str(line) {
                    Ok(record) => record,
                    Err(err) if !has_newline && chunks.peek().is_none() => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "ignoring partial trailing relay usage WAL record"
                        );
                        repair_trailing_record = true;
                        break;
                    }
                    Err(err) => return Err(std::io::Error::other(err)),
                };
                valid_end += chunk.len();
                append_missing_newline = !has_newline;
                match record {
                    RelayUsageWalRecord::Usage {
                        seq,
                        tunnel_id,
                        period_yyyymm,
                        bytes,
                    } => {
                        max_seq = max_seq.max(seq);
                        replayed.insert(
                            seq,
                            RelayUsageItem {
                                tunnel_id,
                                period_yyyymm,
                                bytes,
                            },
                        );
                    }
                    RelayUsageWalRecord::Ack { through_seq } => {
                        ack_through = ack_through.max(through_seq);
                    }
                }
            }
            if repair_trailing_record || append_missing_newline {
                let mut file = OpenOptions::new().write(true).open(&path)?;
                file.set_len(valid_end as u64)?;
                if append_missing_newline {
                    file.seek(SeekFrom::End(0))?;
                    file.write_all(b"\n")?;
                }
                file.flush()?;
                file.sync_data()?;
            }
        }
        replayed.retain(|seq, _| *seq > ack_through);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        sync_parent_dir(&path)?;
        Ok(Self {
            path,
            state: Mutex::new(RelayUsageWalState {
                next_seq: max_seq.max(ack_through).saturating_add(1).max(1),
                ack_through,
                unacked: replayed,
                file,
            }),
        })
    }

    pub fn record(&self, tunnel_id: &str, period_yyyymm: &str, bytes: u64) -> std::io::Result<()> {
        self.record_items([RelayUsageItem {
            tunnel_id: tunnel_id.to_string(),
            period_yyyymm: period_yyyymm.to_string(),
            bytes,
        }])
    }

    pub fn record_items(
        &self,
        items: impl IntoIterator<Item = RelayUsageItem>,
    ) -> std::io::Result<()> {
        let mut state = self.state.lock();
        let mut next_seq = state.next_seq;
        let mut prepared = Vec::new();
        for item in items {
            if item.bytes == 0 || item.tunnel_id.is_empty() || item.period_yyyymm.is_empty() {
                continue;
            }
            let seq = next_seq;
            next_seq = next_seq.saturating_add(1);
            let record = RelayUsageWalRecord::Usage {
                seq,
                tunnel_id: item.tunnel_id.clone(),
                period_yyyymm: item.period_yyyymm.clone(),
                bytes: item.bytes,
            };
            let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
            prepared.push((seq, item, line));
        }
        if prepared.is_empty() {
            return Ok(());
        }
        for (_, _, line) in &prepared {
            state.file.write_all(line.as_bytes())?;
            state.file.write_all(b"\n")?;
        }
        state.file.flush()?;
        state.file.sync_data()?;
        state.next_seq = next_seq;
        for (seq, item, _) in prepared {
            state.unacked.insert(seq, item);
        }
        Ok(())
    }

    pub fn snapshot(&self, max_items: usize) -> std::io::Result<RelayUsageBatch> {
        let state = self.state.lock();
        let items = state
            .unacked
            .iter()
            .take(max_items)
            .map(|(seq, item)| RelayUsageBatchItem {
                seq: *seq,
                tunnel_id: item.tunnel_id.clone(),
                period_yyyymm: item.period_yyyymm.clone(),
                bytes: item.bytes,
            })
            .collect::<Vec<_>>();
        Ok(RelayUsageBatch {
            through_seq: items
                .last()
                .map(|item| item.seq)
                .unwrap_or(state.ack_through),
            items,
        })
    }

    pub fn ack(&self, through_seq: u64) -> std::io::Result<()> {
        let mut state = self.state.lock();
        if through_seq <= state.ack_through {
            return Ok(());
        }
        let record = RelayUsageWalRecord::Ack { through_seq };
        serde_json::to_writer(&mut state.file, &record).map_err(std::io::Error::other)?;
        state.file.write_all(b"\n")?;
        state.file.flush()?;
        state.file.sync_data()?;
        state.ack_through = through_seq;
        state.unacked.retain(|seq, _| *seq > through_seq);
        drop(state);
        self.compact()
    }

    fn compact(&self) -> std::io::Result<()> {
        let mut state = self.state.lock();
        let tmp = self.path.with_extension("wal.tmp");
        {
            let mut out = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            let ack = RelayUsageWalRecord::Ack {
                through_seq: state.ack_through,
            };
            serde_json::to_writer(&mut out, &ack).map_err(std::io::Error::other)?;
            out.write_all(b"\n")?;
            for (seq, item) in &state.unacked {
                let record = RelayUsageWalRecord::Usage {
                    seq: *seq,
                    tunnel_id: item.tunnel_id.clone(),
                    period_yyyymm: item.period_yyyymm.clone(),
                    bytes: item.bytes,
                };
                serde_json::to_writer(&mut out, &record).map_err(std::io::Error::other)?;
                out.write_all(b"\n")?;
            }
            out.flush()?;
            out.sync_data()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        sync_parent_dir(&self.path)?;
        state.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

impl RelayQuotaLimiter {
    fn new_unlimited_usage_meter(tunnel_id: &str) -> Self {
        Self {
            state: Mutex::new(RelayQuotaState {
                tunnel_id: tunnel_id.to_string(),
                period: current_relay_usage_period(),
                ..RelayQuotaState::default()
            }),
        }
    }

    fn update(&self, tunnel_id: &str, period: &str, quota_bytes: u64, remaining_bytes: u64) {
        let mut state = self.state.lock();
        if period.is_empty() || quota_bytes == 0 {
            let pending_usage_by_period = std::mem::take(&mut state.pending_usage_by_period);
            *state = RelayQuotaState {
                tunnel_id: tunnel_id.to_string(),
                pending_usage_by_period,
                ..RelayQuotaState::default()
            };
            return;
        }
        let platform_remaining = remaining_bytes.min(quota_bytes);
        if state.period != period || state.quota_bytes == 0 {
            let pending_usage_by_period = std::mem::take(&mut state.pending_usage_by_period);
            *state = RelayQuotaState {
                tunnel_id: tunnel_id.to_string(),
                period: period.to_string(),
                quota_bytes,
                remaining_bytes: platform_remaining,
                last_platform_remaining_bytes: platform_remaining,
                pending_usage_by_period,
                exhaustion_warning_emitted: false,
            };
            return;
        }

        state.tunnel_id = tunnel_id.to_string();
        let previous_platform_remaining =
            state.last_platform_remaining_bytes.min(state.quota_bytes);
        let quota_growth = quota_bytes.saturating_sub(state.quota_bytes);
        let next_remaining = if quota_bytes >= state.quota_bytes {
            state.remaining_bytes.saturating_add(quota_growth)
        } else {
            state
                .remaining_bytes
                .saturating_sub(state.quota_bytes.saturating_sub(quota_bytes))
        };
        state.quota_bytes = quota_bytes;
        let expected_platform_remaining_after_quota_growth =
            previous_platform_remaining.saturating_add(quota_growth);
        let platform_correction =
            platform_remaining.saturating_sub(expected_platform_remaining_after_quota_growth);
        state.remaining_bytes = if platform_correction > 0 {
            next_remaining
                .saturating_add(platform_correction)
                .min(platform_remaining)
        } else {
            let platform_decrease =
                expected_platform_remaining_after_quota_growth.saturating_sub(platform_remaining);
            next_remaining.saturating_sub(platform_decrease)
        };
        state.last_platform_remaining_bytes = platform_remaining;
        if state.remaining_bytes > 0 {
            state.exhaustion_warning_emitted = false;
        }
    }

    pub(crate) fn try_consume(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut state = self.state.lock();
        if state.quota_bytes == 0 {
            return true;
        }
        let bytes = bytes as u64;
        if state.remaining_bytes < bytes {
            if !state.exhaustion_warning_emitted {
                tracing::warn!(
                    tunnel_id = %state.tunnel_id,
                    period = %state.period,
                    quota_bytes = state.quota_bytes,
                    remaining_bytes = state.remaining_bytes,
                    attempted_bytes = bytes,
                    "relay quota exhausted"
                );
                state.exhaustion_warning_emitted = true;
            }
            return false;
        }
        state.remaining_bytes -= bytes;
        true
    }

    pub(crate) fn commit_usage(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock();
        if state.tunnel_id.is_empty() {
            return;
        }
        if state.quota_bytes == 0 {
            state.period = current_relay_usage_period();
        }
        if state.period.is_empty() {
            return;
        }
        let period = state.period.clone();
        *state.pending_usage_by_period.entry(period).or_default() += bytes as u64;
    }

    pub(crate) fn refund(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock();
        if state.quota_bytes == 0 {
            return;
        }
        state.remaining_bytes = state
            .remaining_bytes
            .saturating_add(bytes as u64)
            .min(state.quota_bytes);
    }

    fn snapshot_pending_usage(&self) -> Vec<RelayUsageSnapshot> {
        let state = self.state.lock();
        state
            .pending_usage_by_period
            .iter()
            .filter(|(_, bytes)| **bytes > 0)
            .map(|(period, bytes)| RelayUsageSnapshot {
                tunnel_id: state.tunnel_id.clone(),
                period: period.clone(),
                bytes: *bytes,
            })
            .collect()
    }

    fn drain_pending_usage(&self) -> Vec<RelayUsageSnapshot> {
        let mut state = self.state.lock();
        let tunnel_id = state.tunnel_id.clone();
        let pending = std::mem::take(&mut state.pending_usage_by_period);
        pending
            .into_iter()
            .filter(|(_, bytes)| *bytes > 0)
            .map(|(period, bytes)| RelayUsageSnapshot {
                tunnel_id: tunnel_id.clone(),
                period,
                bytes,
            })
            .collect()
    }

    fn subtract_pending_usage(&self, period: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock();
        let Some(pending) = state.pending_usage_by_period.get_mut(period) else {
            return;
        };
        *pending = pending.saturating_sub(bytes);
        if *pending == 0 {
            state.pending_usage_by_period.remove(period);
        }
    }

    fn has_pending_usage(&self) -> bool {
        self.state
            .lock()
            .pending_usage_by_period
            .values()
            .any(|bytes| *bytes > 0)
    }

    fn mark_usage_reported(&self, period: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut state = self.state.lock();
        if state.period == period {
            state.last_platform_remaining_bytes =
                state.last_platform_remaining_bytes.saturating_sub(bytes);
        }
    }

    #[cfg(test)]
    fn remaining_bytes(&self) -> Option<u64> {
        let state = self.state.lock();
        (state.quota_bytes > 0).then_some(state.remaining_bytes)
    }
}

fn current_relay_usage_period() -> String {
    let now = Utc::now();
    format!("{:04}{:02}", now.year(), now.month())
}

/// Gateway facade for V2 Scope admission and live Peer attachments.
pub struct Gateway {
    /// Public V2 Tunnel admission scopes from static files or authoritative
    /// Platform full snapshots received over outbound control.
    scopes: Arc<crate::scope::ScopeStore>,
    tunnel_limiters: DashMap<String, Arc<BandwidthLimiter>>,
    relay_quotas: DashMap<String, Arc<RelayQuotaLimiter>>,
    destination_diversity: DestinationDiversityTracker,
    clients: DashMap<String /* tunnel_id */, Vec<Arc<ClientConn>>>,
    client_relays: DashMap<ClientRelayRouteKey, ClientRelayRoute>,
    metrics: Arc<MetricsManager>,
    relay_usage_wal: Option<Arc<RelayUsageWal>>,
    relay_usage_flush_lock: Mutex<()>,
    /// P2P endpoint registry keyed by the authenticated V2 Replica identity.
    pub peers: Arc<crate::p2p::PeerRegistry>,
    /// P2P endpoint TTL and Direct-lane policy.
    pub p2p_config: GatewayP2pConfig,
    /// One process-wide bound covering V2 attachments from transport accept
    /// through disconnect. Pending proof attempts and ready attachments use
    /// the same slots, so unauthenticated connections cannot bypass the cap.
    v2_attachment_slots: Arc<Semaphore>,
    v2_peer_attachment_limiter: Arc<V2PeerAttachmentLimiter>,
}

struct V2PeerAttachmentLimiter {
    max_per_peer: usize,
    live: Mutex<HashMap<TunnelClientKey, V2PeerFamilyLease>>,
}

struct V2PeerFamilyLease {
    family: String,
    live_replica_ids: HashSet<String>,
}

#[derive(Clone, Copy, Debug)]
enum V2PeerAttachmentReject {
    InvalidReplicaId,
    DifferentFamily,
    DuplicateReplicaId,
    Capacity,
}

impl V2PeerAttachmentReject {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidReplicaId => "invalid_runtime_replica_id",
            Self::DifferentFamily => "different_runtime_family",
            Self::DuplicateReplicaId => "duplicate_runtime_replica_id",
            Self::Capacity => "replica_capacity",
        }
    }
}

impl V2PeerAttachmentLimiter {
    fn new(max_per_peer: usize) -> Arc<Self> {
        Arc::new(Self {
            max_per_peer,
            live: Mutex::new(HashMap::new()),
        })
    }

    fn try_acquire(
        self: &Arc<Self>,
        tunnel_id: &str,
        peer_id: &str,
        replica_id: &str,
    ) -> Result<V2PeerAttachmentPermit, V2PeerAttachmentReject> {
        let key = (tunnel_id.to_string(), peer_id.to_string());
        let family = v2_replica_family_for_tunnel(tunnel_id, replica_id)
            .ok_or(V2PeerAttachmentReject::InvalidReplicaId)?;
        let mut live = self.live.lock();
        if let Some(lease) = live.get_mut(&key) {
            if lease.family != family {
                return Err(V2PeerAttachmentReject::DifferentFamily);
            }
            if lease.live_replica_ids.contains(replica_id) {
                return Err(V2PeerAttachmentReject::DuplicateReplicaId);
            }
            if lease.live_replica_ids.len() >= self.max_per_peer {
                return Err(V2PeerAttachmentReject::Capacity);
            }
            lease.live_replica_ids.insert(replica_id.to_string());
        } else {
            if self.max_per_peer == 0 {
                return Err(V2PeerAttachmentReject::Capacity);
            }
            live.insert(
                key.clone(),
                V2PeerFamilyLease {
                    family: family.to_string(),
                    live_replica_ids: HashSet::from([replica_id.to_string()]),
                },
            );
        }
        Ok(V2PeerAttachmentPermit {
            limiter: self.clone(),
            key,
            replica_id: replica_id.to_string(),
        })
    }
}

struct V2PeerAttachmentPermit {
    limiter: Arc<V2PeerAttachmentLimiter>,
    key: TunnelClientKey,
    replica_id: String,
}

impl Drop for V2PeerAttachmentPermit {
    fn drop(&mut self) {
        let mut live = self.limiter.live.lock();
        let Some(lease) = live.get_mut(&self.key) else {
            return;
        };
        lease.live_replica_ids.remove(&self.replica_id);
        if lease.live_replica_ids.is_empty() {
            live.remove(&self.key);
        }
    }
}

struct ClientRelayRoute {
    identity: Arc<()>,
    source: Weak<ClientConn>,
    target: Weak<ClientConn>,
    bound_at: Instant,
    source_to_target_bytes: AtomicU64,
    source_to_target_frames: AtomicU64,
    target_to_source_bytes: AtomicU64,
    target_to_source_frames: AtomicU64,
    source_closed: AtomicBool,
    target_closed: AtomicBool,
    opened: AtomicBool,
    sealed_v2: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
enum ClientRelayRouteConsumeError {
    #[error("relay route bind expired")]
    Expired,
    #[error("relay route bind already consumed")]
    AlreadyConsumed,
}

impl ClientRelayRoute {
    fn new(source: &Arc<ClientConn>, target: &Arc<ClientConn>, opened: bool) -> Self {
        Self {
            identity: Arc::new(()),
            source: Arc::downgrade(source),
            target: Arc::downgrade(target),
            bound_at: Instant::now(),
            source_to_target_bytes: AtomicU64::new(0),
            source_to_target_frames: AtomicU64::new(0),
            target_to_source_bytes: AtomicU64::new(0),
            target_to_source_frames: AtomicU64::new(0),
            source_closed: AtomicBool::new(false),
            target_closed: AtomicBool::new(false),
            opened: AtomicBool::new(opened),
            sealed_v2: AtomicBool::new(false),
        }
    }

    fn consume_bound(&self) -> Result<(), ClientRelayRouteConsumeError> {
        if self.bound_at.elapsed() >= CLIENT_RELAY_BOUND_TTL {
            return Err(ClientRelayRouteConsumeError::Expired);
        }
        self.opened
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| ClientRelayRouteConsumeError::AlreadyConsumed)
    }
}

struct ClientRelayRouteReservation<'a> {
    routes: &'a DashMap<ClientRelayRouteKey, ClientRelayRoute>,
    key: ClientRelayRouteKey,
    source: Weak<ClientConn>,
    target: Weak<ClientConn>,
}

impl<'a> ClientRelayRouteReservation<'a> {
    fn new(
        routes: &'a DashMap<ClientRelayRouteKey, ClientRelayRoute>,
        key: ClientRelayRouteKey,
        source: &Arc<ClientConn>,
        target: &Arc<ClientConn>,
    ) -> Self {
        Self {
            routes,
            key,
            source: Arc::downgrade(source),
            target: Arc::downgrade(target),
        }
    }
}

impl Drop for ClientRelayRouteReservation<'_> {
    fn drop(&mut self) {
        self.routes.remove_if(&self.key, |_, route| {
            Weak::ptr_eq(&route.source, &self.source) && Weak::ptr_eq(&route.target, &self.target)
        });
    }
}

struct DestinationDiversityTracker {
    cap: usize,
    window: Duration,
    windows: DashMap<String, DestinationDiversityWindow>,
}

struct DestinationDiversityWindow {
    started_at: Instant,
    destinations: HashSet<String>,
    warned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationDiversityDecision {
    Allowed,
    Warn { unique_destinations: usize },
    Block { unique_destinations: usize },
}

impl DestinationDiversityTracker {
    fn new(cap: usize, window: Duration) -> Self {
        Self {
            cap,
            window,
            windows: DashMap::new(),
        }
    }

    fn record(&self, tunnel_id: &str, address: &str) -> DestinationDiversityDecision {
        self.record_at(tunnel_id, address, Instant::now())
    }

    fn record_at(
        &self,
        tunnel_id: &str,
        address: &str,
        now: Instant,
    ) -> DestinationDiversityDecision {
        let Some(destination) = destination_key(address) else {
            return DestinationDiversityDecision::Allowed;
        };
        let mut entry = self
            .windows
            .entry(tunnel_id.to_string())
            .or_insert_with(|| DestinationDiversityWindow {
                started_at: now,
                destinations: HashSet::new(),
                warned: false,
            });
        if now.duration_since(entry.started_at) >= self.window {
            *entry = DestinationDiversityWindow {
                started_at: now,
                destinations: HashSet::new(),
                warned: false,
            };
        }
        if entry.destinations.contains(&destination) {
            return DestinationDiversityDecision::Allowed;
        }
        if entry.destinations.len() >= self.cap {
            return DestinationDiversityDecision::Block {
                unique_destinations: entry.destinations.len() + 1,
            };
        }
        entry.destinations.insert(destination);
        let unique = entry.destinations.len();
        if unique >= self.cap && !entry.warned {
            entry.warned = true;
            DestinationDiversityDecision::Warn {
                unique_destinations: unique,
            }
        } else {
            DestinationDiversityDecision::Allowed
        }
    }
}

fn canonical_destination(address: &str) -> Option<String> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        if host.trim().is_empty() || port.trim().is_empty() {
            return None;
        }
        return Some(format!(
            "[{}]:{}",
            host.trim().to_ascii_lowercase(),
            port.trim()
        ));
    }
    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return Some(format!("{}:{}", socket.ip(), socket.port()).to_ascii_lowercase());
    }
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        let port = port.trim();
        if !host.is_empty() && !host.contains(':') && !port.is_empty() {
            return Some(format!("{host}:{port}"));
        }
    }
    Some(trimmed.trim_end_matches('.').to_ascii_lowercase())
}

fn destination_key(address: &str) -> Option<String> {
    canonical_destination(address)
}

struct MeteredTcpFlowStream {
    inner: TcpFlowStream,
    limiter: Arc<BandwidthLimiter>,
    quota: Option<Arc<RelayQuotaLimiter>>,
    pending_sleep: Option<Pin<Box<Sleep>>>,
    leftover: Option<bytes::Bytes>,
}

impl MeteredTcpFlowStream {
    fn new(
        inner: TcpFlowStream,
        limiter: Arc<BandwidthLimiter>,
        quota: Option<Arc<RelayQuotaLimiter>>,
    ) -> Self {
        Self {
            inner,
            limiter,
            quota,
            pending_sleep: None,
            leftover: None,
        }
    }

    fn poll_bandwidth_permit(
        &mut self,
        cx: &mut Context<'_>,
        want: usize,
    ) -> Poll<std::io::Result<()>> {
        if want == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if let Some(mut sleep) = self.pending_sleep.take() {
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => {}
                    Poll::Pending => {
                        self.pending_sleep = Some(sleep);
                        return Poll::Pending;
                    }
                }
            }
            if let Err(wait) = self.limiter.try_acquire(want) {
                let mut sleep = Box::pin(tokio::time::sleep(wait));
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => continue,
                    Poll::Pending => {
                        self.pending_sleep = Some(sleep);
                        return Poll::Pending;
                    }
                }
            }
            return Poll::Ready(Ok(()));
        }
    }
}

impl AsyncRead for MeteredTcpFlowStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if let Some(mut chunk) = self.leftover.take() {
                let n = chunk.len().min(buf.remaining());
                match self.poll_bandwidth_permit(cx, n) {
                    Poll::Pending => {
                        self.leftover = Some(chunk);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {}
                }
                if self
                    .quota
                    .as_ref()
                    .is_some_and(|quota| !quota.try_consume(n))
                {
                    return Poll::Ready(Err(std::io::Error::other("relay quota exhausted")));
                }
                if let Some(quota) = &self.quota {
                    quota.commit_usage(n);
                }
                let front = chunk.split_to(n);
                buf.put_slice(&front);
                if !chunk.is_empty() {
                    self.leftover = Some(chunk);
                }
                return Poll::Ready(Ok(()));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            let mut tmp = [0u8; RELAY_FLOW_READ_CHUNK];
            let want = tmp.len().min(buf.remaining()).min(RELAY_BANDWIDTH_CHUNK);
            let mut read_buf = ReadBuf::new(&mut tmp[..want]);
            match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
            let n = read_buf.filled().len();
            if n == 0 {
                return Poll::Ready(Ok(()));
            }
            self.leftover = Some(bytes::Bytes::copy_from_slice(read_buf.filled()));
        }
    }
}

impl AsyncWrite for MeteredTcpFlowStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Clone, Copy)]
struct ReplicaIdParts<'a> {
    family: &'a str,
    random: &'a str,
}

fn parse_replica_id(client_id: &str) -> Option<ReplicaIdParts<'_>> {
    let (family, index) = client_id.rsplit_once('-')?;
    let (_, random) = family.rsplit_once('-')?;
    if family.is_empty()
        || random.is_empty()
        || random.len() != 8
        || !random.bytes().all(|b| b.is_ascii_alphanumeric())
        || !is_replica_index(index)
    {
        return None;
    }
    Some(ReplicaIdParts { family, random })
}

fn v2_replica_family_for_tunnel<'a>(tunnel_id: &str, replica_id: &'a str) -> Option<&'a str> {
    let parts = parse_replica_id(replica_id)?;
    let random = parts.family.strip_prefix(tunnel_id)?.strip_prefix('-')?;
    (random == parts.random).then_some(parts.family)
}

fn is_replica_index(index: &str) -> bool {
    index == "0"
        || (!index.is_empty()
            && !index.starts_with('0')
            && index.bytes().all(|b| b.is_ascii_digit()))
}

fn relay_conn_id_from_wire(conn_id: &[u8; 12]) -> Option<String> {
    let end = conn_id
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(conn_id.len());
    if end == 0 || conn_id[end..].iter().any(|byte| *byte != 0) {
        return None;
    }
    std::str::from_utf8(&conn_id[..end])
        .ok()
        .map(ToOwned::to_owned)
}

impl Gateway {
    /// Construct the V2 Gateway. Every carrier must offer `peer_mesh_v2` and
    /// prove an exact signed Peer membership against a loaded Scope.
    pub fn new(
        p2p_config: GatewayP2pConfig,
        relay_usage_wal: Option<Arc<RelayUsageWal>>,
    ) -> Arc<Self> {
        Self::build(MetricsManager::new(), p2p_config, relay_usage_wal)
    }

    fn build(
        metrics: Arc<MetricsManager>,
        p2p_config: GatewayP2pConfig,
        relay_usage_wal: Option<Arc<RelayUsageWal>>,
    ) -> Arc<Self> {
        let v2_attachment_slots = Arc::new(Semaphore::new(p2p_config.max_v2_attachments));
        let v2_peer_attachment_limiter =
            V2PeerAttachmentLimiter::new(p2p_config.max_replicas_per_peer);
        Arc::new(Self {
            scopes: Arc::new(crate::scope::ScopeStore::new()),
            tunnel_limiters: DashMap::new(),
            relay_quotas: DashMap::new(),
            destination_diversity: DestinationDiversityTracker::new(
                DESTINATION_DIVERSITY_WARN_CAP,
                DESTINATION_DIVERSITY_WINDOW,
            ),
            clients: DashMap::new(),
            client_relays: DashMap::new(),
            metrics,
            relay_usage_wal,
            relay_usage_flush_lock: Mutex::new(()),
            peers: Arc::new(crate::p2p::PeerRegistry::default()),
            p2p_config,
            v2_attachment_slots,
            v2_peer_attachment_limiter,
        })
    }

    pub fn scopes(&self) -> &Arc<crate::scope::ScopeStore> {
        &self.scopes
    }

    pub fn metrics(&self) -> &Arc<MetricsManager> {
        &self.metrics
    }

    fn tunnel_bandwidth_limiter(&self, tunnel_id: &str, mbps: u32) -> Arc<BandwidthLimiter> {
        self.tunnel_limiters
            .entry(tunnel_id.to_string())
            .or_insert_with(|| Arc::new(BandwidthLimiter::new(mbps)))
            .value()
            .clone()
    }

    fn relay_bandwidth_limiter(
        &self,
        tunnel_id: &str,
        fallback: &Arc<BandwidthLimiter>,
    ) -> Arc<BandwidthLimiter> {
        self.tunnel_limiters
            .get(tunnel_id)
            .map(|entry| entry.value().clone())
            .unwrap_or_else(|| fallback.clone())
    }

    pub(crate) fn relay_quota_limiter(&self, tunnel_id: &str) -> Option<Arc<RelayQuotaLimiter>> {
        if tunnel_id.is_empty() {
            return None;
        }
        Some(
            self.relay_quotas
                .entry(tunnel_id.to_string())
                .or_insert_with(|| {
                    Arc::new(RelayQuotaLimiter::new_unlimited_usage_meter(tunnel_id))
                })
                .value()
                .clone(),
        )
    }

    pub fn snapshot_pending_relay_usage(&self) -> Vec<RelayUsageSnapshot> {
        let mut usage = self
            .relay_quotas
            .iter()
            .flat_map(|entry| entry.value().snapshot_pending_usage())
            .collect::<Vec<_>>();
        usage.sort_by(|a, b| {
            a.tunnel_id
                .cmp(&b.tunnel_id)
                .then_with(|| a.period.cmp(&b.period))
        });
        usage
    }

    pub fn drain_pending_relay_usage(&self) -> Vec<RelayUsageSnapshot> {
        let mut usage = self
            .relay_quotas
            .iter()
            .flat_map(|entry| entry.value().drain_pending_usage())
            .collect::<Vec<_>>();
        usage.sort_by(|a, b| {
            a.tunnel_id
                .cmp(&b.tunnel_id)
                .then_with(|| a.period.cmp(&b.period))
        });
        self.prune_flushed_relay_usage_meters();
        usage
    }

    /// Applies a Platform-authoritative Relay budget for one Tunnel.
    ///
    /// A zero quota removes the limit rather than setting an unreachable one:
    /// the Platform expresses "no ceiling" by sending nothing, and a Gateway
    /// that has never been told a budget must keep relaying.
    pub fn apply_relay_quota(
        &self,
        tunnel_id: &str,
        period: &str,
        quota_bytes: u64,
        remaining_bytes: u64,
    ) {
        if let Some(limiter) = self.relay_quota_limiter(tunnel_id) {
            limiter.update(tunnel_id, period, quota_bytes, remaining_bytes);
            // The only positive trace that a budget arrived. Without it the
            // exhaustion warning is the first and only evidence, which means
            // confirming a rollout takes an inference instead of a log line.
            //
            // info, not debug: a Gateway ships with `log.level: info`, so a
            // debug line here would never print on the one deployment that
            // needs it. A budget changes rarely, so this is not chatter.
            tracing::info!(
                tunnel_id,
                period,
                quota_bytes,
                remaining_bytes,
                "applied Platform relay budget"
            );
        }
    }

    pub(crate) fn try_consume_relay_quota(&self, tunnel_id: &str, bytes: usize) -> bool {
        self.relay_quota_limiter(tunnel_id)
            .map(|limiter| limiter.try_consume(bytes))
            .unwrap_or(true)
    }

    pub(crate) fn commit_relay_usage(&self, tunnel_id: &str, bytes: usize) {
        if let Some(limiter) = self.relay_quota_limiter(tunnel_id) {
            limiter.commit_usage(bytes);
        }
    }

    pub(crate) fn refund_relay_quota(&self, tunnel_id: &str, bytes: usize) {
        if let Some(limiter) = self.relay_quotas.get(tunnel_id) {
            limiter.value().refund(bytes);
        }
    }

    pub fn flush_pending_relay_usage_to_wal(&self) -> std::io::Result<usize> {
        let _flush_guard = self.relay_usage_flush_lock.lock();
        let usage = self.snapshot_pending_relay_usage();
        let count = usage.len();
        if count == 0 {
            self.prune_flushed_relay_usage_meters();
            return Ok(0);
        }
        let Some(wal) = &self.relay_usage_wal else {
            return Ok(0);
        };
        let items = usage.iter().map(|item| RelayUsageItem {
            tunnel_id: item.tunnel_id.clone(),
            period_yyyymm: item.period.clone(),
            bytes: item.bytes,
        });
        if let Err(err) = wal.record_items(items) {
            tracing::error!(error = %err, "relay usage WAL append failed");
            return Err(err);
        }
        for item in usage {
            if let Some(limiter) = self.relay_quotas.get(&item.tunnel_id) {
                limiter
                    .value()
                    .subtract_pending_usage(&item.period, item.bytes);
            }
        }
        self.prune_flushed_relay_usage_meters();
        Ok(count)
    }

    pub fn mark_relay_usage_reported(&self, items: &[RelayUsageBatchItem]) {
        for item in items {
            if let Some(limiter) = self.relay_quotas.get(&item.tunnel_id) {
                limiter
                    .value()
                    .mark_usage_reported(&item.period_yyyymm, item.bytes);
            }
        }
        self.prune_flushed_relay_usage_meters();
    }

    fn prune_flushed_relay_usage_meters(&self) {
        let candidates = self
            .relay_quotas
            .iter()
            .filter(|entry| {
                !self.scopes.contains(entry.key()) && !self.clients.contains_key(entry.key())
            })
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        for tunnel_id in candidates {
            self.relay_quotas.remove_if(&tunnel_id, |_, limiter| {
                !self.scopes.contains(&tunnel_id)
                    && !self.clients.contains_key(&tunnel_id)
                    && Arc::strong_count(limiter) == 1
                    && !limiter.has_pending_usage()
            });
        }
    }

    pub fn disconnect_tunnel_clients(&self, tunnel_id: &str) -> usize {
        self.tunnel_limiters.remove(tunnel_id);
        // Pending billable usage outlives the live Scope/attachments and is
        // retained until the normal WAL flush path durably records it.
        let victims = {
            let Some(clients) = self.clients.get(tunnel_id) else {
                return 0;
            };
            clients.iter().cloned().collect::<Vec<_>>()
        };

        for victim in &victims {
            tracing::warn!(
                tunnel_id,
                client_id = %victim.params.client_id,
                "Scope removed; closing live V2 attachment"
            );
            self.unregister(tunnel_id, victim);
            victim.sender().close();
        }

        victims.len()
    }

    /// Run the selected transport server until it is closed or the owning
    /// future is dropped by the app shutdown path.
    ///
    /// Per-incoming work — transport handshake/auth and the subsequent
    /// `ClientConn::run` — runs in a spawned task per peer so that:
    ///   - a hostile or misconfigured client hitting an ALPN mismatch
    ///     (`error 120: peer doesn't support any known protocol`), TLS
    ///     failure, malformed auth data, or rejected credentials only logs
    ///     at `warn` and never propagates out of the gateway accept loop;
    ///   - a slow handshake cannot block other pending incoming clients.
    pub async fn serve(self: &Arc<Self>, server: GatewayServer) -> anyhow::Result<()> {
        // Spawn the stale-eviction sweeper. TTLs come from `p2p_config`
        // (gateway.p2p in YAML); defaults match the previously hardcoded
        // values (60 s / 30 s). When `p2p.enabled = false`, the sweeper is
        // not spawned — the registries still accept upserts but never expire
        // entries. The task lives until process exit, matching the transport
        // accept loops which also have no cancellation.
        if self.p2p_config.enabled {
            let peers = self.peers.clone();
            let peer_ttl = std::time::Duration::from_secs(self.p2p_config.peer_idle_secs);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                // First tick fires immediately; skip it so the first eviction
                // sweep happens at +15s rather than at startup.
                tick.tick().await;
                loop {
                    tick.tick().await;
                    peers.evict_older_than(peer_ttl);
                }
            });
        }
        match server {
            GatewayServer::Quic(server) => self.serve_quic(server).await,
            GatewayServer::WebSocket(server) => self.serve_websocket(server).await,
            GatewayServer::Grpc(server) => self.serve_grpc(server).await,
        }
    }

    async fn serve_quic(self: &Arc<Self>, server: QuicServer) -> anyhow::Result<()> {
        let auth = self.auth_handler();
        while let Some(incoming) = server.accept_incoming().await {
            let auth = auth.clone();
            let gw = self.clone();
            let peer = incoming.remote_address();
            tokio::spawn(async move {
                match QuicServer::complete_handshake(incoming, &*auth).await {
                    Ok((params, session)) => gw.run_client_session(params, session).await,
                    Err(e) => {
                        tracing::warn!(
                            %peer,
                            error = %e,
                            "tunnel handshake failed; gateway continues"
                        );
                    }
                }
            });
        }
        Ok(())
    }

    async fn serve_websocket(self: &Arc<Self>, server: WsServer) -> anyhow::Result<()> {
        let auth = self.auth_handler();
        loop {
            match server.accept(&*auth).await {
                Ok(Some((params, session))) => {
                    let gw = self.clone();
                    tokio::spawn(async move {
                        gw.run_client_session(params, session).await;
                    });
                }
                Ok(None) => return Ok(()),
                Err(e) => {
                    tracing::warn!(error = %e, "websocket tunnel handshake failed; gateway continues");
                }
            }
        }
    }

    async fn serve_grpc(self: &Arc<Self>, server: GrpcServer) -> anyhow::Result<()> {
        let auth = self.auth_handler();
        let gw = self.clone();
        server
            .serve(auth, move |params, session| {
                let gw = gw.clone();
                tokio::spawn(async move {
                    gw.run_client_session(params, session).await;
                });
            })
            .await?;
        Ok(())
    }

    fn auth_handler(&self) -> Arc<GatewayAuth> {
        Arc::new(GatewayAuth {
            scopes: self.scopes.clone(),
        })
    }

    async fn authenticate_v2_attachment(
        &self,
        params: &AuthParams,
        session: &mut Session,
        deadline: Duration,
    ) -> std::result::Result<AuthenticatedPeerV2, String> {
        v2_replica_family_for_tunnel(&params.tunnel_id, &params.client_id)
            .ok_or_else(|| "invalid V2 runtime Replica ID".to_string())?;
        let scope = self
            .scopes
            .get(&params.tunnel_id)
            .ok_or_else(|| "V2 tunnel Scope not found".to_string())?;
        let mut challenge = [0u8; 32];
        OsRng.fill_bytes(&mut challenge);
        session
            .send(BinaryMessage::AuthV2Challenge { challenge })
            .await
            .map_err(|_| "failed to send V2 attachment challenge".to_string())?;
        let proof = tokio::time::timeout(deadline, session.recv())
            .await
            .map_err(|_| "V2 attachment proof timed out".to_string())?
            .ok_or_else(|| "V2 attachment closed before proof".to_string())?;
        let BinaryMessage::AuthV2Proof {
            membership,
            signature,
        } = proof
        else {
            return Err("expected V2 attachment proof".into());
        };
        if membership.tunnel_id != params.tunnel_id {
            return Err("V2 membership tunnel mismatch".into());
        }
        membership
            .verify(&scope.tunnel_signing_public_key)
            .map_err(|_| "invalid V2 Tunnel membership".to_string())?;
        membership
            .verify_attachment_proof(&challenge, &params.client_id, &signature)
            .map_err(|_| "invalid V2 Peer proof".to_string())?;
        Ok(AuthenticatedPeerV2 {
            tunnel_id: membership.tunnel_id,
            peer_id: membership.peer_id,
            replica_id: params.client_id.clone(),
            overlay_ip: membership.overlay_ip,
        })
    }

    async fn run_client_session(self: Arc<Self>, params: AuthParams, session: Session) {
        self.run_client_session_with_v2_deadline(params, session, V2_ATTACHMENT_AUTH_DEADLINE)
            .await;
    }

    async fn run_client_session_with_v2_deadline(
        self: Arc<Self>,
        mut params: AuthParams,
        mut session: Session,
        v2_auth_deadline: Duration,
    ) {
        let _v2_attachment_slot = match self.v2_attachment_slots.clone().try_acquire_owned() {
            Ok(slot) => slot,
            Err(_) => {
                tracing::warn!(
                    tunnel_id = %params.tunnel_id,
                    client_id = %params.client_id,
                    "V2 attachment capacity reached; rejecting pending connection"
                );
                session.close();
                return;
            }
        };
        let authenticated_peer_v2 = match self
            .authenticate_v2_attachment(&params, &mut session, v2_auth_deadline)
            .await
        {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(
                    tunnel_id = %params.tunnel_id,
                    client_id = %params.client_id,
                    reason = %error,
                    "V2 attachment proof rejected"
                );
                session.close();
                return;
            }
        };
        let _v2_peer_attachment_slot = match self.v2_peer_attachment_limiter.try_acquire(
            &authenticated_peer_v2.tunnel_id,
            &authenticated_peer_v2.peer_id,
            &authenticated_peer_v2.replica_id,
        ) {
            Ok(slot) => slot,
            Err(reject) => {
                let max_replicas = self.p2p_config.max_replicas_per_peer;
                tracing::warn!(
                    tunnel_id = %params.tunnel_id,
                    client_id = %params.client_id,
                    peer_id = %authenticated_peer_v2.peer_id,
                    max_replicas,
                    reason = reject.as_str(),
                    "V2 Peer active runtime family rejected new attachment"
                );
                session.close();
                return;
            }
        };
        let bw = 0;
        let limiter = self.tunnel_bandwidth_limiter(&params.tunnel_id, bw);

        self.metrics
            .update_client_heartbeat(&params.client_id, &params.group_id);
        let tunnel_id = params.tunnel_id.clone();
        let group = params.group_id.clone();
        let client_id = params.client_id.clone();
        let peer_addr = params.peer_addr;
        // Never keep obsolete credential envelope values on a live
        // ClientConn after the authentication boundary.
        params.username.clear();
        params.password.clear();
        params.group_password.clear();
        let cc = ClientConn::new(
            params,
            session,
            limiter,
            self.metrics.clone(),
            self.peers.clone(),
            Arc::downgrade(&self),
            authenticated_peer_v2,
        );
        // P2P: record the client's public socket address so peer-signaling
        // (P2pOffer/P2pAnswer in tasks 2.3–2.6) can forward to it. `locals`,
        // `nat_hint`, and `cert_fp` stay empty/zero until the client sends
        // P2pAnnounce — this initial upsert just guarantees the registry has
        // an entry the moment Auth succeeds.
        // TODO(p2p-task-5.X): integration test for auth-touch — Phase 5
        // handshake e2e (Task 5.1) will exercise this path naturally.
        self.peers.upsert(
            &tunnel_id,
            &client_id,
            crate::p2p::PeerEndpoint {
                public: peer_addr,
                locals: vec![],
                nat_hint: 0,
                cert_fp: [0u8; 32],
                last_seen: std::time::Instant::now(),
            },
        );
        let identity = cc.authenticated_peer_v2();
        self.peers
            .bind_v2_identity(&identity.tunnel_id, &identity.peer_id, &identity.replica_id);
        self.register(&tunnel_id, cc.clone());
        if self.reject_stale_session_after_registration(&cc) {
            return;
        }
        let started = std::time::Instant::now();
        let (sent_before, recv_before) = self.metrics.client_byte_totals(&client_id);
        tracing::info!(
            %client_id,
            tunnel_id = %tunnel_id,
            group_id = %group,
            peer = %peer_addr,
            bandwidth_mbps = bw,
            "tunnel client connected"
        );
        cc.run().await;
        self.metrics.mark_client_offline(&client_id);
        self.unregister(&tunnel_id, &cc);
        let (sent_after, recv_after) = self.metrics.client_byte_totals(&client_id);
        tracing::info!(
            %client_id,
            tunnel_id = %tunnel_id,
            group_id = %group,
            peer = %peer_addr,
            session_secs = started.elapsed().as_secs(),
            bytes_sent = sent_after.saturating_sub(sent_before),
            bytes_recv = recv_after.saturating_sub(recv_before),
            "tunnel client disconnected"
        );
    }

    pub(crate) fn p2p_membership_peer_ids_in_tunnel(
        &self,
        tunnel_id: &str,
        announcer: &ClientConn,
    ) -> Vec<String> {
        if !self.client_scope_identity_is_current(announcer) {
            return Vec::new();
        }
        let announcing_peer = announcer.authenticated_peer_v2();
        let mut peer_ids: Vec<String> = {
            let Some(entry) = self.clients.get(tunnel_id) else {
                return Vec::new();
            };
            entry
                .value()
                .iter()
                .map(|candidate| candidate.authenticated_peer_v2())
                .filter(|candidate| {
                    candidate.peer_id != announcing_peer.peer_id
                        && self.scopes.contains(&candidate.tunnel_id)
                })
                .map(|candidate| candidate.peer_id.clone())
                .collect()
        };
        peer_ids.sort();
        peer_ids.dedup();
        peer_ids
    }

    pub(crate) fn client_scope_identity_is_current(&self, client: &ClientConn) -> bool {
        {
            let identity = client.authenticated_peer_v2();
            identity.tunnel_id == client.params.tunnel_id
                && identity.replica_id == client.params.client_id
                && self.scopes.contains(&identity.tunnel_id)
        }
    }

    /// Close the last Scope-authentication→register race window.
    fn reject_stale_session_after_registration(&self, client: &Arc<ClientConn>) -> bool {
        let tunnel_id = &client.params.tunnel_id;
        if self.client_scope_identity_is_current(client) {
            return false;
        }
        tracing::warn!(
            tunnel_id = %tunnel_id,
            client_id = %client.params.client_id,
            "Scope identity changed before registration completed; closing"
        );
        self.unregister(tunnel_id, client);
        if self.clients.get(tunnel_id).is_none() {
            self.tunnel_limiters.remove(tunnel_id);
        }
        client.sender().close();
        self.metrics.mark_client_offline(&client.params.client_id);
        true
    }

    pub(crate) fn p2p_v2_target_in_tunnel(
        &self,
        source: &Arc<ClientConn>,
        target_peer_id: &str,
    ) -> Option<Arc<ClientConn>> {
        let source_identity = source.authenticated_peer_v2();
        if source_identity.tunnel_id != source.params.tunnel_id
            || source_identity.replica_id != source.params.client_id
            || source_identity.peer_id == target_peer_id
            || self
                .peers
                .stable_peer_id(&source_identity.tunnel_id, &source_identity.replica_id)
                .as_deref()
                != Some(source_identity.peer_id.as_str())
        {
            return None;
        }
        let clients = self.clients.get(&source_identity.tunnel_id)?;
        clients.iter().rev().find_map(|candidate| {
            let target_identity = candidate.authenticated_peer_v2();
            (target_identity.tunnel_id == source_identity.tunnel_id
                && target_identity.peer_id == target_peer_id
                && target_identity.replica_id == candidate.params.client_id
                && self
                    .peers
                    .stable_peer_id(&source_identity.tunnel_id, &candidate.params.client_id)
                    .as_deref()
                    == Some(target_peer_id))
            .then(|| candidate.clone())
        })
    }

    pub(crate) fn bind_client_relay_route(
        &self,
        source: &Arc<ClientConn>,
        conn_id: String,
        target_peer_id: &str,
    ) -> anyhow::Result<()> {
        let target_peer_id = target_peer_id.trim();
        if target_peer_id.is_empty() {
            anyhow::bail!("empty relay route peer");
        }
        if target_peer_id == source.authenticated_peer_v2().peer_id {
            anyhow::bail!("relay route target loops to source Peer {target_peer_id}");
        }
        let target = self
            .p2p_v2_target_in_tunnel(source, target_peer_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "relay route peer {target_peer_id} offline in tunnel {}",
                    source.params.tunnel_id
                )
            })?;
        let route_key = ClientRelayRouteKey::new(source, conn_id);
        match self.client_relays.entry(route_key) {
            Entry::Vacant(entry) => {
                entry.insert(ClientRelayRoute::new(source, &target, false));
            }
            Entry::Occupied(mut entry) => {
                let existing_expired = !entry.get().opened.load(Ordering::Acquire)
                    && entry.get().bound_at.elapsed() >= CLIENT_RELAY_BOUND_TTL;
                let existing_source = entry.get().source.upgrade();
                let existing_target = entry.get().target.upgrade();
                let same_unconsumed_route = existing_source
                    .as_ref()
                    .zip(existing_target.as_ref())
                    .is_some_and(|(existing_source, existing_target)| {
                        Arc::ptr_eq(existing_source, source)
                            && Arc::ptr_eq(existing_target, &target)
                            && !entry.get().opened.load(Ordering::Acquire)
                    });
                if existing_expired {
                    entry.insert(ClientRelayRoute::new(source, &target, false));
                } else if !same_unconsumed_route {
                    if existing_source.is_none() || existing_target.is_none() {
                        entry.insert(ClientRelayRoute::new(source, &target, false));
                    } else {
                        anyhow::bail!(
                            "relay route conn id already bound in tunnel {}",
                            source.params.tunnel_id
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn consume_bound_client_relay_target(
        &self,
        source: &Arc<ClientConn>,
        conn_id: &str,
    ) -> anyhow::Result<Option<(ClientRelayRouteKey, Arc<ClientConn>)>> {
        let route_key = ClientRelayRouteKey::new(source, conn_id);
        let Some(route) = self.client_relays.get(&route_key) else {
            return Ok(None);
        };
        let bound_source = route
            .source
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("relay route source session closed"))?;
        if !Arc::ptr_eq(&bound_source, source)
            || bound_source.params.tunnel_id != source.params.tunnel_id
        {
            anyhow::bail!("relay route is bound to another source session");
        }
        let target = route
            .target
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("relay route target session closed"))?;
        let route_identity = route.identity.clone();
        if let Err(error) = route.consume_bound() {
            let expired = matches!(error, ClientRelayRouteConsumeError::Expired);
            drop(route);
            if expired {
                self.client_relays.remove_if(&route_key, |_, current| {
                    Arc::ptr_eq(&current.identity, &route_identity)
                });
            }
            return Err(error.into());
        }
        Ok(Some((route_key, target)))
    }

    #[cfg(test)]
    fn client_relay_route_is_sealed_v2_for_test(
        &self,
        source: &Arc<ClientConn>,
        conn_id: &str,
    ) -> bool {
        self.client_relays
            .get(&ClientRelayRouteKey::new(source, conn_id))
            .is_some_and(|route| route.sealed_v2.load(Ordering::Acquire))
    }

    fn remove_client_relay_route_if_endpoints(
        &self,
        route_key: &ClientRelayRouteKey,
        source: &Arc<ClientConn>,
        target: &Arc<ClientConn>,
    ) -> bool {
        self.remove_client_relay_route_if_weak_endpoints(
            route_key,
            &Arc::downgrade(source),
            &Arc::downgrade(target),
        )
    }

    fn remove_client_relay_route_if_weak_endpoints(
        &self,
        route_key: &ClientRelayRouteKey,
        source: &Weak<ClientConn>,
        target: &Weak<ClientConn>,
    ) -> bool {
        self.client_relays
            .remove_if(route_key, |_, route| {
                Weak::ptr_eq(&route.source, source) && Weak::ptr_eq(&route.target, target)
            })
            .is_some()
    }

    pub(crate) async fn relay_client_connect(
        &self,
        source: &Arc<ClientConn>,
        conn_id: String,
        network: String,
        address: String,
    ) -> anyhow::Result<()> {
        self.check_destination_policy(&source.params.tunnel_id, &address)?;
        let route_key = ClientRelayRouteKey::new(source, conn_id.clone());
        let (_bound_route_key, target) = self
            .consume_bound_client_relay_target(source, &conn_id)?
            .ok_or_else(|| anyhow::anyhow!("relay route is not bound"))?;
        if network == "udp"
            && target.sender().udp_data_mode() == tp_transport::UdpDataMode::QuicDatagramRequired
            && !target.sender().udp_datagram_available()
        {
            self.remove_client_relay_route_if_endpoints(&route_key, source, &target);
            anyhow::bail!("datagram transport unavailable");
        }
        let address_present = !address.is_empty();
        let address_count = usize::from(address_present);
        tracing::info!(
            conn_id = %conn_id,
            tunnel_id = %source.params.tunnel_id,
            source_client_id = %source.params.client_id,
            target_client_id = %target.params.client_id,
            relay_class = "exact_peer",
            path = "connect",
            protocol = %network,
            address_present,
            address_count,
            "client relay route created"
        );
        tracing::debug!(
            conn_id = %conn_id,
            tunnel_id = %source.params.tunnel_id,
            source_client_id = %source.params.client_id,
            target_client_id = %target.params.client_id,
            relay_class = "exact_peer",
            path = "connect",
            protocol = %network,
            address = %address,
            "client relay route destination selected"
        );
        if target
            .sender()
            .send(BinaryMessage::Connect {
                conn_id: conn_id.clone(),
                network,
                address,
            })
            .await
            .is_err()
        {
            self.remove_client_relay_route_if_endpoints(&route_key, source, &target);
            tracing::warn!(
                conn_id = %conn_id,
                tunnel_id = %source.params.tunnel_id,
                source_client_id = %source.params.client_id,
                target_client_id = %target.params.client_id,
                "client relay connect dropped: target session closed"
            );
            anyhow::bail!("relay target session closed");
        }
        Ok(())
    }

    pub(crate) async fn relay_client_tcp_flow(
        &self,
        source: &Arc<ClientConn>,
        mut incoming: TcpFlowIncoming,
    ) {
        if let Some(raw_preface) = incoming.stream.raw_preface().cloned() {
            self.relay_client_tcp_flow_v2(source, incoming, raw_preface)
                .await;
            return;
        }
        let conn_id = incoming.preface.conn_id.clone();
        let address = incoming.preface.address.clone();
        if let Err(e) = self.check_destination_policy(&source.params.tunnel_id, &address) {
            tracing::debug!(
                conn_id = %conn_id,
                source_client_id = %source.params.client_id,
                address = %address,
                error = %e,
                "client tcp flow relay destination policy rejected details"
            );
            let _ = incoming
                .stream
                .send_connect_response(false, "destination policy rejected".into())
                .await;
            return;
        }
        let (target, consumed_route_key) =
            match self.consume_bound_client_relay_target(source, &conn_id) {
                Ok(Some((route_key, target))) => (target, route_key),
                Ok(None) => {
                    let _ = incoming
                        .stream
                        .send_connect_response(false, "relay route is not bound".into())
                        .await;
                    return;
                }
                Err(e) => {
                    tracing::debug!(
                        conn_id = %conn_id,
                        source_client_id = %source.params.client_id,
                        error = %e,
                        "client tcp flow relay route unavailable details"
                    );
                    let _ = incoming
                        .stream
                        .send_connect_response(false, "relay route unavailable".into())
                        .await;
                    return;
                }
            };
        let _route_reservation = ClientRelayRouteReservation::new(
            &self.client_relays,
            consumed_route_key,
            source,
            &target,
        );
        let address_present = !address.is_empty();
        let address_count = usize::from(address_present);
        tracing::info!(
            conn_id = %conn_id,
            tunnel_id = %source.params.tunnel_id,
            source_client_id = %source.params.client_id,
            target_client_id = %target.params.client_id,
            relay_class = "exact_peer",
            path = "tcp_flow",
            protocol = "tcp",
            address_present,
            address_count,
            "client tcp flow relay opening target stream"
        );
        tracing::debug!(
            conn_id = %conn_id,
            tunnel_id = %source.params.tunnel_id,
            source_client_id = %source.params.client_id,
            target_client_id = %target.params.client_id,
            relay_class = "exact_peer",
            path = "tcp_flow",
            protocol = "tcp",
            address = %address,
            "client tcp flow relay target destination selected"
        );

        if !target.sender().capabilities().tcp_flow_stream_v1 {
            let mut target_conn = match target
                .open_framed_with_conn_id(conn_id.clone(), "tcp", &address)
                .await
            {
                Ok(conn) => conn,
                Err(e) => {
                    let _ = incoming
                        .stream
                        .send_connect_response(false, "relay target open failed".into())
                        .await;
                    tracing::debug!(
                        conn_id = %conn_id,
                        source_client_id = %source.params.client_id,
                        target_client_id = %target.params.client_id,
                        error = %e,
                        "client tcp flow relay framed target open failed"
                    );
                    return;
                }
            };
            if incoming
                .stream
                .send_connect_response(true, String::new())
                .await
                .is_err()
            {
                return;
            }
            match tokio::io::copy_bidirectional_with_sizes(
                &mut incoming.stream,
                &mut target_conn,
                TCP_FLOW_COPY_BUFFER_BYTES,
                TCP_FLOW_COPY_BUFFER_BYTES,
            )
            .await
            {
                Ok((source_to_target, target_to_source)) => {
                    tracing::info!(
                        conn_id = %conn_id,
                        source_client_id = %source.params.client_id,
                        target_client_id = %target.params.client_id,
                        source_to_target_bytes = source_to_target,
                        target_to_source_bytes = target_to_source,
                        "client tcp flow relay closed with framed target"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        conn_id = %conn_id,
                        source_client_id = %source.params.client_id,
                        target_client_id = %target.params.client_id,
                        error = %e,
                        "client tcp flow relay framed bridge ended with error"
                    );
                }
            }
            return;
        }

        let target_stream = match target
            .sender()
            .open_tcp_flow_stream(conn_id.clone(), address.clone(), Duration::from_secs(15))
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                let _ = incoming
                    .stream
                    .send_connect_response(false, "relay target open failed".into())
                    .await;
                tracing::debug!(
                    conn_id = %conn_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    error = %e,
                    "client tcp flow relay target open failed"
                );
                return;
            }
        };

        if incoming
            .stream
            .send_connect_response(true, String::new())
            .await
            .is_err()
        {
            return;
        }

        let limiter = self.relay_bandwidth_limiter(&source.params.tunnel_id, &source.limiter);
        let quota = self.relay_quota_limiter(&source.params.tunnel_id);
        let mut source_stream =
            MeteredTcpFlowStream::new(incoming.stream, limiter.clone(), quota.clone());
        let mut target_stream = MeteredTcpFlowStream::new(target_stream, limiter, quota);
        match tokio::io::copy_bidirectional_with_sizes(
            &mut source_stream,
            &mut target_stream,
            TCP_FLOW_COPY_BUFFER_BYTES,
            TCP_FLOW_COPY_BUFFER_BYTES,
        )
        .await
        {
            Ok((source_to_target, target_to_source)) => {
                tracing::info!(
                    conn_id = %conn_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    source_to_target_bytes = source_to_target,
                    target_to_source_bytes = target_to_source,
                    "client tcp flow relay closed"
                );
            }
            Err(e) => {
                tracing::debug!(
                    conn_id = %conn_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    error = %e,
                    "client tcp flow relay bridge ended with error"
                );
            }
        }
    }

    /// Relay a V2 sealed QUIC TCP Flow without terminating its endpoint
    /// protocol. The transport has parsed only `version + conn_id`; this
    /// bridge consumes that already-bound route and forwards the complete
    /// preface and stream bytes unchanged.
    async fn relay_client_tcp_flow_v2(
        &self,
        source: &Arc<ClientConn>,
        incoming: TcpFlowIncoming,
        raw_preface: bytes::Bytes,
    ) {
        let conn_id = incoming.preface.conn_id.clone();
        let (target, consumed_route_key) =
            match self.consume_bound_client_relay_target(source, &conn_id) {
                Ok(Some((route_key, target))) => (target, route_key),
                Ok(None) => return,
                Err(error) => {
                    tracing::debug!(
                        conn_id = %conn_id,
                        source_client_id = %source.params.client_id,
                        %error,
                        "sealed client tcp flow relay route unavailable"
                    );
                    return;
                }
            };
        let _route_reservation = ClientRelayRouteReservation::new(
            &self.client_relays,
            consumed_route_key,
            source,
            &target,
        );
        let target_stream = match target
            .sender()
            .open_raw_tcp_flow_stream(raw_preface, Duration::from_secs(15))
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(
                    conn_id = %conn_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    %error,
                    "sealed client tcp flow target open failed"
                );
                return;
            }
        };

        let limiter = self.relay_bandwidth_limiter(&source.params.tunnel_id, &source.limiter);
        let quota = self.relay_quota_limiter(&source.params.tunnel_id);
        let mut source_stream =
            MeteredTcpFlowStream::new(incoming.stream, limiter.clone(), quota.clone());
        let mut target_stream = MeteredTcpFlowStream::new(target_stream, limiter, quota);
        match tokio::io::copy_bidirectional_with_sizes(
            &mut source_stream,
            &mut target_stream,
            TCP_FLOW_COPY_BUFFER_BYTES,
            TCP_FLOW_COPY_BUFFER_BYTES,
        )
        .await
        {
            Ok((source_to_target, target_to_source)) => {
                tracing::info!(
                    conn_id = %conn_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    source_to_target_bytes = source_to_target,
                    target_to_source_bytes = target_to_source,
                    "sealed client tcp flow relay closed"
                );
            }
            Err(error) => {
                tracing::debug!(
                    conn_id = %conn_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    %error,
                    "sealed client tcp flow relay ended with error"
                );
            }
        }
    }

    pub(crate) async fn forward_client_relay(
        &self,
        from: &Arc<ClientConn>,
        msg: BinaryMessage,
    ) -> bool {
        let (conn_id, mut terminal, msg_kind, payload_len) = match &msg {
            BinaryMessage::ConnectResponse {
                conn_id, success, ..
            } => (conn_id.clone(), !*success, "connect_response", 0usize),
            BinaryMessage::Data { conn_id, payload } => {
                (conn_id.clone(), false, "data", payload.len())
            }
            BinaryMessage::UdpData { conn_id, payload } => {
                (conn_id.clone(), false, "udp_data", payload.len())
            }
            BinaryMessage::Close { conn_id } => (conn_id.clone(), false, "close", 0usize),
            _ => return false,
        };

        let route_key = ClientRelayRouteKey::new(from, conn_id.clone());
        let Some(route) = self.client_relays.get(&route_key) else {
            tracing::warn!(
                conn_id = %conn_id,
                from_client_id = %from.params.client_id,
                %msg_kind,
                payload_len,
                "client relay frame dropped: missing route"
            );
            return false;
        };
        if !route.opened.load(Ordering::Acquire) {
            tracing::warn!(
                conn_id = %conn_id,
                from_client_id = %from.params.client_id,
                %msg_kind,
                payload_len,
                "client relay frame dropped: route bind has not been consumed"
            );
            return false;
        }
        let source = route.source.upgrade();
        let target = route.target.upgrade();
        let route_source = route.source.clone();
        let route_target = route.target.clone();
        let mut source_to_target_bytes = None;
        let mut source_to_target_frames = None;
        let mut target_to_source_bytes = None;
        let mut target_to_source_frames = None;
        let source_to_target = source
            .as_ref()
            .zip(target.as_ref())
            .is_some_and(|(source, _)| Arc::ptr_eq(source, from));
        let target_to_source = source
            .as_ref()
            .zip(target.as_ref())
            .is_some_and(|(_, target)| Arc::ptr_eq(target, from));
        if msg_kind == "data" && payload_len > 0 {
            if source_to_target {
                let previous = route
                    .source_to_target_bytes
                    .fetch_add(payload_len as u64, Ordering::Relaxed);
                let frames = route
                    .source_to_target_frames
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                source_to_target_bytes = Some(previous + payload_len as u64);
                source_to_target_frames = Some(frames);
            } else if target_to_source {
                let previous = route
                    .target_to_source_bytes
                    .fetch_add(payload_len as u64, Ordering::Relaxed);
                let frames = route
                    .target_to_source_frames
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                target_to_source_bytes = Some(previous + payload_len as u64);
                target_to_source_frames = Some(frames);
            }
        }
        if msg_kind == "close" {
            terminal = if source_to_target {
                route.source_closed.store(true, Ordering::Release);
                route.target_closed.load(Ordering::Acquire)
            } else if target_to_source {
                route.target_closed.store(true, Ordering::Release);
                route.source_closed.load(Ordering::Acquire)
            } else {
                false
            };
        }
        drop(route);

        let dest = match (source, target) {
            (Some(source), Some(target)) if Arc::ptr_eq(&source, from) => target,
            (Some(source), Some(target)) if Arc::ptr_eq(&target, from) => source,
            (Some(source), Some(target)) => {
                tracing::warn!(
                    conn_id = %conn_id,
                    from_client_id = %from.params.client_id,
                    source_client_id = %source.params.client_id,
                    target_client_id = %target.params.client_id,
                    %msg_kind,
                    payload_len,
                    "client relay frame dropped: sender is not on route"
                );
                return false;
            }
            _ => {
                self.remove_client_relay_route_if_weak_endpoints(
                    &route_key,
                    &route_source,
                    &route_target,
                );
                tracing::warn!(
                    conn_id = %conn_id,
                    from_client_id = %from.params.client_id,
                    %msg_kind,
                    payload_len,
                    "client relay frame dropped: stale route endpoint"
                );
                return false;
            }
        };
        let dest_client_id = dest.params.client_id.clone();
        let direction = if source_to_target {
            "source_to_target"
        } else if target_to_source {
            "target_to_source"
        } else {
            "unknown"
        };
        let relay_limiter = self.relay_bandwidth_limiter(&from.params.tunnel_id, &from.limiter);
        let mut quota_charged = false;
        if payload_len > 0 {
            if msg_kind == "udp_data" {
                if relay_limiter.try_acquire(payload_len).is_err() {
                    from.record_relay_udp_forward_full();
                    self.metrics.increment_udp_drops();
                    return true;
                }
            } else if msg_kind == "data" {
                relay_limiter.acquire(payload_len).await;
            }
            if !self.try_consume_relay_quota(&from.params.tunnel_id, payload_len) {
                if msg_kind == "udp_data" {
                    from.record_relay_udp_forward_full();
                    self.metrics.increment_udp_drops();
                    return true;
                }
                tracing::debug!(
                    conn_id = %conn_id,
                    from_client_id = %from.params.client_id,
                    %msg_kind,
                    payload_len,
                    "client relay frame dropped: relay quota exhausted"
                );
                self.remove_client_relay_route_if_weak_endpoints(
                    &route_key,
                    &route_source,
                    &route_target,
                );
                return false;
            }
            quota_charged = true;
        }

        const RELAY_TCP_PROGRESS_BYTES: u64 = 8 * 1024 * 1024;
        if let Some(total_bytes) = source_to_target_bytes {
            let previous = total_bytes.saturating_sub(payload_len as u64);
            if previous / RELAY_TCP_PROGRESS_BYTES != total_bytes / RELAY_TCP_PROGRESS_BYTES {
                tracing::info!(
                    conn_id = %conn_id,
                    from_client_id = %from.params.client_id,
                    dest_client_id = %dest_client_id,
                    %direction,
                    total_bytes,
                    frames = source_to_target_frames.unwrap_or_default(),
                    "client relay tcp progress"
                );
            }
        }
        if let Some(total_bytes) = target_to_source_bytes {
            let previous = total_bytes.saturating_sub(payload_len as u64);
            if previous / RELAY_TCP_PROGRESS_BYTES != total_bytes / RELAY_TCP_PROGRESS_BYTES {
                tracing::info!(
                    conn_id = %conn_id,
                    from_client_id = %from.params.client_id,
                    dest_client_id = %dest_client_id,
                    %direction,
                    total_bytes,
                    frames = target_to_source_frames.unwrap_or_default(),
                    "client relay tcp progress"
                );
            }
        }
        if msg_kind == "connect_response" || msg_kind == "close" {
            tracing::info!(
                conn_id = %conn_id,
                from_client_id = %from.params.client_id,
                dest_client_id = %dest_client_id,
                %direction,
                %msg_kind,
                terminal,
                "client relay control forwarded"
            );
        }

        let forwarded = match msg {
            BinaryMessage::UdpData { ref payload, .. } => {
                let payload_len = payload.len();
                match dest.sender().try_send(msg) {
                    Ok(()) => {
                        if quota_charged {
                            self.commit_relay_usage(&from.params.tunnel_id, payload_len);
                        }
                        from.record_relay_udp_forward_ok(payload_len);
                        true
                    }
                    Err(TrySendKind::Full) => {
                        if quota_charged {
                            self.refund_relay_quota(&from.params.tunnel_id, payload_len);
                        }
                        from.record_relay_udp_forward_full();
                        self.metrics.increment_udp_drops();
                        true
                    }
                    Err(TrySendKind::TooLarge(len)) => {
                        if quota_charged {
                            self.refund_relay_quota(&from.params.tunnel_id, payload_len);
                        }
                        from.record_relay_udp_forward_too_large();
                        self.metrics.increment_udp_drops();
                        tracing::warn!(len, "app relay UDP frame exceeded tunnel limit; dropped");
                        true
                    }
                    Err(TrySendKind::DatagramUnavailable) => {
                        if quota_charged {
                            self.refund_relay_quota(&from.params.tunnel_id, payload_len);
                        }
                        from.record_relay_udp_forward_closed();
                        tracing::warn!("app relay UDP frame requires datagram transport; dropped");
                        false
                    }
                    Err(TrySendKind::Closed) => {
                        if quota_charged {
                            self.refund_relay_quota(&from.params.tunnel_id, payload_len);
                        }
                        from.record_relay_udp_forward_closed();
                        false
                    }
                }
            }
            other => {
                let ok = dest.sender().send(other).await.is_ok();
                if quota_charged {
                    if ok {
                        self.commit_relay_usage(&from.params.tunnel_id, payload_len);
                    } else {
                        self.refund_relay_quota(&from.params.tunnel_id, payload_len);
                    }
                }
                ok
            }
        };
        if !forwarded {
            tracing::warn!(
                conn_id = %conn_id,
                from_client_id = %from.params.client_id,
                dest_client_id = %dest_client_id,
                %direction,
                %msg_kind,
                payload_len,
                "client relay frame not forwarded: destination session closed"
            );
        }
        if terminal || !forwarded {
            self.remove_client_relay_route_if_weak_endpoints(
                &route_key,
                &route_source,
                &route_target,
            );
            tracing::info!(
                conn_id = %conn_id,
                from_client_id = %from.params.client_id,
                dest_client_id = %dest_client_id,
                %direction,
                %msg_kind,
                terminal,
                forwarded,
                "client relay route removed"
            );
        }
        forwarded
    }

    pub(crate) async fn forward_encrypted_peer_control_v2(
        &self,
        from: &Arc<ClientConn>,
        target_peer_id: String,
        peerlink_session_id: [u8; 16],
        conn_id: [u8; 12],
        route_abort: bool,
        sealed: bytes::Bytes,
    ) -> bool {
        let from_identity = from.authenticated_peer_v2();
        if from_identity.tunnel_id != from.params.tunnel_id
            || from_identity.replica_id != from.params.client_id
            || self
                .peers
                .stable_peer_id(&from_identity.tunnel_id, &from_identity.replica_id)
                .as_deref()
                != Some(from_identity.peer_id.as_str())
        {
            return false;
        }
        // On ingress this outer field names the destination. After the
        // Gateway authenticates and routes it, the receiver needs the
        // opposite stable Peer identity to select the same PeerLink/AAD.
        // Rewrite only this outer hint; the sealed bytes stay opaque.
        let remote_peer_id = from_identity.peer_id.clone();

        if conn_id == [0; 12] {
            if route_abort {
                return false;
            }
            let Some(target) = self.p2p_v2_target_in_tunnel(from, &target_peer_id) else {
                return false;
            };
            return target
                .sender()
                .send(BinaryMessage::EncryptedPeerControlV2 {
                    target_peer_id: remote_peer_id,
                    peerlink_session_id,
                    conn_id,
                    route_abort: false,
                    sealed,
                })
                .await
                .is_ok();
        }

        let Some(route_conn_id) = relay_conn_id_from_wire(&conn_id) else {
            return false;
        };
        let route_key = ClientRelayRouteKey::new(from, route_conn_id);
        let Some(route) = self.client_relays.get(&route_key) else {
            return false;
        };
        let Some(route_source) = route.source.upgrade() else {
            return false;
        };
        let Some(route_target) = route.target.upgrade() else {
            return false;
        };
        let from_is_source = Arc::ptr_eq(&route_source, from);
        let from_is_target = Arc::ptr_eq(&route_target, from);
        let dest = if from_is_source {
            route_target.clone()
        } else if from_is_target {
            route_source.clone()
        } else {
            return false;
        };
        let dest_identity = dest.authenticated_peer_v2();
        if dest_identity.tunnel_id != from_identity.tunnel_id
            || dest_identity.peer_id != target_peer_id
        {
            return false;
        }

        if !route_abort && !route.opened.load(Ordering::Acquire) {
            if !from_is_source || route.consume_bound().is_err() {
                return false;
            }
            route.sealed_v2.store(true, Ordering::Release);
        } else if !route.sealed_v2.load(Ordering::Acquire) {
            return false;
        }
        let route_source_weak = route.source.clone();
        let route_target_weak = route.target.clone();
        drop(route);

        let forwarded = dest
            .sender()
            .send(BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id: remote_peer_id,
                peerlink_session_id,
                conn_id,
                route_abort,
                sealed,
            })
            .await
            .is_ok();
        if route_abort || !forwarded {
            self.remove_client_relay_route_if_weak_endpoints(
                &route_key,
                &route_source_weak,
                &route_target_weak,
            );
        }
        forwarded
    }

    /// Apply the compile-time destination denylist to every exact-Peer relay.
    fn host_filter_for(&self, tunnel_id: &str) -> anyhow::Result<host_filter::HostFilter> {
        host_filter::HostFilter::new(&[], &[]).map_err(|error| {
            tracing::debug!(
                tunnel_id = %tunnel_id,
                error = %error,
                "destination policy host filter configuration invalid"
            );
            anyhow::anyhow!("host_filter_invalid")
        })
    }

    fn check_destination_policy(&self, tunnel_id: &str, address: &str) -> anyhow::Result<()> {
        let canonical = canonical_destination(address).unwrap_or_else(|| address.trim().into());
        let address_present = !address.is_empty();
        let address_count = usize::from(address_present);
        tracing::debug!(
            tunnel_id = %tunnel_id,
            address = %address,
            canonical = %canonical,
            "destination policy evaluating target"
        );
        if !self.host_filter_for(tunnel_id)?.is_allowed(&canonical) {
            tracing::warn!(
                tunnel_id = %tunnel_id,
                reason = "host_forbidden",
                policy_class = "host_filter",
                address_present,
                address_count,
                "destination policy rejected"
            );
            anyhow::bail!("host_forbidden");
        }
        match self.destination_diversity.record(tunnel_id, &canonical) {
            DestinationDiversityDecision::Allowed => {}
            DestinationDiversityDecision::Warn {
                unique_destinations,
            } => {
                tracing::warn!(
                    target: "audit",
                    audit_event = "gateway.destination_diversity",
                    tunnel_id = %tunnel_id,
                    reason = "destination_diversity_warn",
                    policy_class = "destination_diversity",
                    address_present,
                    address_count,
                    unique_destinations,
                    window_secs = DESTINATION_DIVERSITY_WINDOW.as_secs(),
                    cap = DESTINATION_DIVERSITY_WARN_CAP,
                    "destination_diversity_warn"
                );
            }
            DestinationDiversityDecision::Block {
                unique_destinations,
            } => {
                tracing::warn!(
                    target: "audit",
                    audit_event = "gateway.destination_diversity_block",
                    tunnel_id = %tunnel_id,
                    reason = "destination_diversity_exceeded",
                    policy_class = "destination_diversity",
                    address_present,
                    address_count,
                    unique_destinations,
                    window_secs = DESTINATION_DIVERSITY_WINDOW.as_secs(),
                    cap = DESTINATION_DIVERSITY_HARD_CAP,
                    "destination_diversity_block"
                );
                anyhow::bail!("destination_diversity_exceeded");
            }
        }
        Ok(())
    }

    fn register(&self, tunnel_id: &str, cc: Arc<ClientConn>) {
        self.clients
            .entry(tunnel_id.to_string())
            .or_default()
            .push(cc);
    }
    fn unregister(&self, tunnel_id: &str, cc: &Arc<ClientConn>) {
        // Two-step: (1) retain inside the shard write guard, snapshot emptiness,
        // drop the guard; (2) if empty, atomically remove the entry only when it
        // is still empty — `remove_if` re-checks under the shard lock so a
        // concurrent `register` that pushed a fresh `ClientConn` between step 1
        // and step 2 is not clobbered. The `rr` cursor entry never needs that
        // atomicity because fresh V2 attachments can register concurrently.
        let now_empty = {
            let Some(mut list) = self.clients.get_mut(tunnel_id) else {
                return;
            };
            list.retain(|c| !Arc::ptr_eq(c, cc));
            list.is_empty()
        };
        if now_empty {
            self.clients.remove_if(tunnel_id, |_, list| list.is_empty());
        }
        let mut stale_relay_routes = Vec::new();
        let mut relay_close_notifications = Vec::new();
        for route in self.client_relays.iter() {
            let route_key = route.key().clone();
            let conn_id = route_key.conn_id.clone();
            let source = route.source.upgrade();
            let target = route.target.upgrade();
            let source_is_leaving = source
                .as_ref()
                .is_some_and(|source| Arc::ptr_eq(source, cc));
            let target_is_leaving = target
                .as_ref()
                .is_some_and(|target| Arc::ptr_eq(target, cc));
            let stale =
                source.is_none() || target.is_none() || source_is_leaving || target_is_leaving;
            if !stale {
                continue;
            }

            if let Some(source) = source.filter(|source| !Arc::ptr_eq(source, cc)) {
                relay_close_notifications.push((conn_id.clone(), source));
            }
            if let Some(target) = target.filter(|target| !Arc::ptr_eq(target, cc)) {
                relay_close_notifications.push((conn_id.clone(), target));
            }
            stale_relay_routes.push((route_key, route.source.clone(), route.target.clone()));
        }
        for (conn_id, peer) in relay_close_notifications {
            let _ = peer.sender().try_send(BinaryMessage::Close {
                conn_id: conn_id.clone(),
            });
            self.metrics.close_connection(&conn_id);
        }
        for (route_key, source, target) in stale_relay_routes {
            self.remove_client_relay_route_if_weak_endpoints(&route_key, &source, &target);
        }
        self.peers.remove(tunnel_id, &cc.params.client_id);
    }
}

struct GatewayAuth {
    scopes: Arc<crate::scope::ScopeStore>,
}

#[async_trait]
impl AuthHandler for GatewayAuth {
    async fn authenticate(&self, p: &AuthParams) -> std::result::Result<(), String> {
        if p.tunnel_id.is_empty() {
            return Err("missing tunnel_id".into());
        }
        if !p.capabilities.peer_mesh_v2 {
            return Err("peer_mesh_v2 capability is required".into());
        }
        if p.client_id.trim().is_empty() {
            return Err("missing runtime replica_id".into());
        }
        if !p.group_id.is_empty()
            || !p.username.is_empty()
            || !p.password.is_empty()
            || !p.group_password.is_empty()
        {
            return Err("V2 auth must not carry legacy credentials".into());
        }
        self.scopes
            .contains(&p.tunnel_id)
            .then_some(())
            .ok_or_else(|| "V2 tunnel Scope not found".into())
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tp_core::protocol::TransportCapabilities;
    use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};

    fn params(tunnel_id: &str) -> AuthParams {
        AuthParams {
            tunnel_id: tunnel_id.into(),
            capabilities: TransportCapabilities {
                peer_mesh_v2: true,
                ..Default::default()
            },
            client_id: format!("{tunnel_id}-AbC12345-0"),
            group_id: String::new(),
            username: String::new(),
            password: String::new(),
            group_password: String::new(),
            role: tp_core::config::ClientRoleConfig::Client,
            peer_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4242),
        }
    }

    fn auth_with_scope() -> (GatewayAuth, String) {
        let owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "127.0.0.1".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: None,
            trusted_certificate_pem: None,
        })
        .expect("generate V2 Tunnel");
        let scope = owner.scope().expect("derive Scope");
        let tunnel_id = scope.tunnel_id.clone();
        let scopes = Arc::new(crate::scope::ScopeStore::new());
        scopes
            .replace_managed_snapshot(vec![scope])
            .expect("load Scope snapshot");
        (GatewayAuth { scopes }, tunnel_id)
    }

    #[tokio::test]
    async fn rejects_carrier_without_peer_mesh_v2() {
        let (auth, tunnel_id) = auth_with_scope();
        let mut p = params(&tunnel_id);
        p.capabilities.peer_mesh_v2 = false;

        let error = auth.authenticate(&p).await.expect_err("V2 is mandatory");
        assert_eq!(error, "peer_mesh_v2 capability is required");
    }

    #[tokio::test]
    async fn rejects_legacy_credentials_on_v2_carrier() {
        let (auth, tunnel_id) = auth_with_scope();
        let mut p = params(&tunnel_id);
        p.password = "shared-secret".into();

        let error = auth
            .authenticate(&p)
            .await
            .expect_err("secrets fail closed");
        assert_eq!(error, "V2 auth must not carry legacy credentials");
    }

    #[tokio::test]
    async fn rejects_unknown_scope() {
        let auth = GatewayAuth {
            scopes: Arc::new(crate::scope::ScopeStore::new()),
        };

        let error = auth
            .authenticate(&params("unknown-tunnel"))
            .await
            .expect_err("Scope is mandatory");
        assert_eq!(error, "V2 tunnel Scope not found");
    }

    #[tokio::test]
    async fn accepts_secret_free_v2_carrier_for_loaded_scope() {
        let (auth, tunnel_id) = auth_with_scope();
        auth.authenticate(&params(&tunnel_id))
            .await
            .expect("loaded Scope admits the proof-capable carrier");
    }
}
#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::sync::mpsc;
    use tp_core::protocol::{unpack, BinaryMessage, PackedMessage, TransportCapabilities};
    use tp_core::provisioning::{GatewayBootstrapV2, PeerProfileV2, TunnelOwnerFileV2};
    use tp_transport::{AuthParams, Session};

    fn gateway() -> Arc<Gateway> {
        Gateway::new(GatewayP2pConfig::default(), None)
    }

    fn auth(tunnel_id: &str, replica_id: &str, peer_addr: SocketAddr) -> AuthParams {
        AuthParams {
            tunnel_id: tunnel_id.into(),
            capabilities: TransportCapabilities {
                peer_mesh_v2: true,
                ..Default::default()
            },
            client_id: replica_id.into(),
            group_id: String::new(),
            username: String::new(),
            password: String::new(),
            group_password: String::new(),
            role: tp_core::config::ClientRoleConfig::Client,
            peer_addr,
        }
    }

    fn session(
        peer_addr: SocketAddr,
    ) -> (
        Session,
        mpsc::Sender<BinaryMessage>,
        mpsc::Receiver<PackedMessage>,
    ) {
        let (out_tx, out_rx) = mpsc::channel(16);
        let (in_tx, in_rx) = mpsc::channel(4);
        (
            Session::new_channeled(
                out_tx,
                in_rx,
                peer_addr,
                Arc::new(|| {}),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ),
            in_tx,
            out_rx,
        )
    }

    fn profiles() -> (
        tp_core::provisioning::GatewayScopeFileV2,
        PeerProfileV2,
        PeerProfileV2,
    ) {
        let mut owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "127.0.0.1".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: None,
            trusted_certificate_pem: None,
        })
        .expect("generate V2 Tunnel");
        let scope = owner.scope().expect("derive Scope");
        let first = owner.add_peer(None, 2, None).expect("first Peer");
        let second = owner.add_peer(None, 2, None).expect("second Peer");
        (scope, first, second)
    }

    fn v2_conn(
        gateway: &Arc<Gateway>,
        tunnel_id: &str,
        peer_id: &str,
        replica_id: &str,
    ) -> (Arc<ClientConn>, mpsc::Receiver<PackedMessage>) {
        let peer_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345);
        let (session, _inbound, outbound) = session(peer_addr);
        let conn = ClientConn::new(
            auth(tunnel_id, replica_id, peer_addr),
            session,
            Arc::new(BandwidthLimiter::new(0)),
            MetricsManager::new(),
            gateway.peers.clone(),
            Arc::downgrade(gateway),
            AuthenticatedPeerV2 {
                tunnel_id: tunnel_id.into(),
                peer_id: peer_id.into(),
                replica_id: replica_id.into(),
                overlay_ip: Ipv4Addr::new(198, 18, 0, 9),
            },
        );
        gateway
            .peers
            .bind_v2_identity(tunnel_id, peer_id, replica_id);
        gateway.register(tunnel_id, conn.clone());
        (conn, outbound)
    }

    async fn recv_message(outbound: &mut mpsc::Receiver<PackedMessage>) -> BinaryMessage {
        let packed = tokio::time::timeout(Duration::from_secs(1), outbound.recv())
            .await
            .expect("message timeout")
            .expect("session output");
        unpack(&packed.to_bytes()).expect("valid packed message")
    }

    fn registered(gateway: &Gateway, tunnel_id: &str, replica_id: &str) -> Option<Arc<ClientConn>> {
        gateway.clients.get(tunnel_id).and_then(|clients| {
            clients
                .iter()
                .rev()
                .find(|client| client.params.client_id == replica_id)
                .cloned()
        })
    }

    async fn wait_registered(gateway: &Gateway, tunnel_id: &str, replica_id: &str) {
        for _ in 0..100 {
            if registered(gateway, tunnel_id, replica_id).is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{replica_id} did not register");
    }

    #[tokio::test]
    async fn valid_v2_proof_registers_exact_authenticated_identity() {
        let gateway = gateway();
        let (scope, profile, _) = profiles();
        gateway
            .scopes()
            .replace_managed_snapshot(vec![scope])
            .expect("load Scope snapshot");
        let replica_id = format!("{}-Valid001-0", profile.tunnel_id);
        let peer_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4242);
        let (session, inbound, mut outbound) = session(peer_addr);
        let task = tokio::spawn(
            gateway
                .clone()
                .run_client_session(auth(&profile.tunnel_id, &replica_id, peer_addr), session),
        );

        let challenge = match recv_message(&mut outbound).await {
            BinaryMessage::AuthV2Challenge { challenge } => challenge,
            other => panic!("unexpected message: {other:?}"),
        };
        inbound
            .send(BinaryMessage::AuthV2Proof {
                membership: profile.public_membership(),
                signature: profile
                    .sign_attachment_proof(&challenge, &replica_id)
                    .expect("sign proof"),
            })
            .await
            .expect("send proof");

        wait_registered(&gateway, &profile.tunnel_id, &replica_id).await;
        let conn =
            registered(&gateway, &profile.tunnel_id, &replica_id).expect("registered attachment");
        assert_eq!(conn.authenticated_peer_v2().peer_id, profile.peer.peer_id);
        assert_eq!(
            gateway
                .peers
                .stable_peer_id(&profile.tunnel_id, &replica_id)
                .as_deref(),
            Some(profile.peer.peer_id.as_str())
        );

        drop(inbound);
        task.await.expect("attachment exits");
    }

    #[tokio::test]
    async fn v2_proof_is_bound_to_the_runtime_replica_id() {
        let gateway = gateway();
        let (scope, profile, _) = profiles();
        gateway
            .scopes()
            .replace_managed_snapshot(vec![scope])
            .expect("load Scope snapshot");
        let replica_id = format!("{}-Valid001-0", profile.tunnel_id);
        let wrong_replica_id = format!("{}-Other001-0", profile.tunnel_id);
        let peer_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4243);
        let (session, inbound, mut outbound) = session(peer_addr);
        let task = tokio::spawn(
            gateway
                .clone()
                .run_client_session(auth(&profile.tunnel_id, &replica_id, peer_addr), session),
        );

        let challenge = match recv_message(&mut outbound).await {
            BinaryMessage::AuthV2Challenge { challenge } => challenge,
            other => panic!("unexpected message: {other:?}"),
        };
        inbound
            .send(BinaryMessage::AuthV2Proof {
                membership: profile.public_membership(),
                signature: profile
                    .sign_attachment_proof(&challenge, &wrong_replica_id)
                    .expect("sign mismatched proof"),
            })
            .await
            .expect("send proof");

        task.await.expect("rejected attachment exits");
        assert!(registered(&gateway, &profile.tunnel_id, &replica_id).is_none());
    }

    #[test]
    fn v2_attachment_cap_is_scoped_to_one_stable_peer_family() {
        let limiter = V2PeerAttachmentLimiter::new(2);
        let first = limiter
            .try_acquire("tun", "peer-a", "tun-Aaaaaaaa-0")
            .expect("first replica");
        let second = limiter
            .try_acquire("tun", "peer-a", "tun-Aaaaaaaa-1")
            .expect("second replica");
        assert!(matches!(
            limiter.try_acquire("tun", "peer-a", "tun-Aaaaaaaa-2"),
            Err(V2PeerAttachmentReject::Capacity)
        ));
        assert!(matches!(
            limiter.try_acquire("tun", "peer-a", "tun-Bbbbbbbb-2"),
            Err(V2PeerAttachmentReject::DifferentFamily)
        ));
        drop(first);
        limiter
            .try_acquire("tun", "peer-a", "tun-Aaaaaaaa-2")
            .expect("released slot is reusable");
        drop(second);
    }

    #[tokio::test]
    async fn p2p_target_is_exact_peer_and_tunnel_bound() {
        let gateway = gateway();
        let (source, _) = v2_conn(&gateway, "tun-a", "peer-a", "tun-a-Aaaaaaaa-0");
        let (target, _) = v2_conn(&gateway, "tun-a", "peer-b", "tun-a-Bbbbbbbb-0");
        let (_other_tunnel, _) = v2_conn(&gateway, "tun-b", "peer-c", "tun-b-Cccccccc-0");

        let selected = gateway
            .p2p_v2_target_in_tunnel(&source, "peer-b")
            .expect("exact target");
        assert!(Arc::ptr_eq(&selected, &target));
        assert!(gateway.p2p_v2_target_in_tunnel(&source, "peer-a").is_none());
        assert!(gateway.p2p_v2_target_in_tunnel(&source, "peer-c").is_none());
    }

    #[tokio::test]
    async fn membership_hints_deduplicate_replicas_to_stable_peers() {
        let gateway = gateway();
        let (scope, source_profile, target_profile) = profiles();
        gateway
            .scopes()
            .replace_managed_snapshot(vec![scope])
            .expect("load Scope snapshot");
        let tunnel_id = source_profile.tunnel_id.as_str();
        let (source, _) = v2_conn(
            &gateway,
            tunnel_id,
            &source_profile.peer.peer_id,
            &format!("{tunnel_id}-Source01-0"),
        );
        let _ = v2_conn(
            &gateway,
            tunnel_id,
            &target_profile.peer.peer_id,
            &format!("{tunnel_id}-Target01-0"),
        );
        let _ = v2_conn(
            &gateway,
            tunnel_id,
            &target_profile.peer.peer_id,
            &format!("{tunnel_id}-Target01-1"),
        );

        assert_eq!(
            gateway.p2p_membership_peer_ids_in_tunnel(tunnel_id, &source),
            vec![target_profile.peer.peer_id]
        );
    }

    #[tokio::test]
    async fn exact_relay_binding_is_single_use() {
        let gateway = gateway();
        let (source, _) = v2_conn(&gateway, "tun", "peer-a", "tun-Aaaaaaaa-0");
        let (target, _) = v2_conn(&gateway, "tun", "peer-b", "tun-Bbbbbbbb-0");
        gateway
            .bind_client_relay_route(&source, "flow-1".into(), "peer-b")
            .expect("bind exact Peer");

        let (_, selected) = gateway
            .consume_bound_client_relay_target(&source, "flow-1")
            .expect("consume route")
            .expect("bound route");
        assert!(Arc::ptr_eq(&selected, &target));
        assert!(gateway
            .consume_bound_client_relay_target(&source, "flow-1")
            .is_err());
    }

    #[tokio::test]
    async fn ordinary_relay_connect_requires_a_prebound_exact_peer() {
        let gateway = gateway();
        let (source, _) = v2_conn(&gateway, "tun", "peer-a", "tun-Aaaaaaaa-0");
        let (_target, mut target_outbound) = v2_conn(&gateway, "tun", "peer-b", "tun-Bbbbbbbb-0");

        let error = gateway
            .relay_client_connect(
                &source,
                "unbound".into(),
                "tcp".into(),
                "example.com:443".into(),
            )
            .await
            .expect_err("unbound routing must fail closed");
        assert!(error.to_string().contains("relay route is not bound"));
        assert!(target_outbound.try_recv().is_err());

        gateway
            .bind_client_relay_route(&source, "bound".into(), "peer-b")
            .expect("bind route");
        gateway
            .relay_client_connect(
                &source,
                "bound".into(),
                "tcp".into(),
                "example.com:443".into(),
            )
            .await
            .expect("bound relay");
        assert!(matches!(
            recv_message(&mut target_outbound).await,
            BinaryMessage::Connect { conn_id, network, address }
                if conn_id == "bound" && network == "tcp" && address == "example.com:443"
        ));
    }

    #[tokio::test]
    async fn encrypted_direct_control_routes_exact_peer_and_preserves_ciphertext() {
        let gateway = gateway();
        let (source, _) = v2_conn(&gateway, "tun", "peer-a", "tun-Aaaaaaaa-0");
        let (_target, mut target_outbound) = v2_conn(&gateway, "tun", "peer-b", "tun-Bbbbbbbb-0");
        let sealed = bytes::Bytes::from_static(b"opaque ciphertext");

        assert!(
            gateway
                .forward_encrypted_peer_control_v2(
                    &source,
                    "peer-b".into(),
                    [7; 16],
                    [0; 12],
                    false,
                    sealed.clone(),
                )
                .await
        );
        assert!(matches!(
            recv_message(&mut target_outbound).await,
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id,
                peerlink_session_id,
                conn_id,
                route_abort: false,
                sealed: forwarded,
            } if target_peer_id == "peer-a"
                && peerlink_session_id == [7; 16]
                && conn_id == [0; 12]
                && forwarded == sealed
        ));
    }

    #[tokio::test]
    async fn encrypted_relay_control_consumes_exact_bound_route() {
        let gateway = gateway();
        let (source, _) = v2_conn(&gateway, "tun", "peer-a", "tun-Aaaaaaaa-0");
        let (_target, mut target_outbound) = v2_conn(&gateway, "tun", "peer-b", "tun-Bbbbbbbb-0");
        let conn_id = *b"flow00000001";
        gateway
            .bind_client_relay_route(&source, "flow00000001".into(), "peer-b")
            .expect("bind exact route");

        assert!(
            gateway
                .forward_encrypted_peer_control_v2(
                    &source,
                    "peer-b".into(),
                    [9; 16],
                    conn_id,
                    false,
                    bytes::Bytes::from_static(b"sealed open"),
                )
                .await
        );
        assert!(gateway.client_relay_route_is_sealed_v2_for_test(&source, "flow00000001"));
        assert!(matches!(
            recv_message(&mut target_outbound).await,
            BinaryMessage::EncryptedPeerControlV2 { target_peer_id, conn_id: forwarded, .. }
                if target_peer_id == "peer-a" && forwarded == conn_id
        ));
    }

    #[test]
    fn destination_diversity_blocks_only_new_destinations_at_cap() {
        let tracker = DestinationDiversityTracker::new(2, Duration::from_secs(60));
        assert_eq!(
            tracker.record("tun", "a.example:443"),
            DestinationDiversityDecision::Allowed
        );
        assert_eq!(
            tracker.record("tun", "b.example:443"),
            DestinationDiversityDecision::Warn {
                unique_destinations: 2
            }
        );
        assert_eq!(
            tracker.record("tun", "a.example:443"),
            DestinationDiversityDecision::Allowed
        );
        assert_eq!(
            tracker.record("tun", "c.example:443"),
            DestinationDiversityDecision::Block {
                unique_destinations: 3
            }
        );
    }

    #[test]
    fn v2_relay_usage_is_metered_without_legacy_quota_authority() {
        let dir = tempfile::tempdir().expect("usage WAL directory");
        let wal = Arc::new(RelayUsageWal::open(dir.path().join("relay.wal")).expect("open WAL"));
        let gateway = Gateway::new(GatewayP2pConfig::default(), Some(wal.clone()));

        gateway.commit_relay_usage("tun-v2", 37);

        let usage = gateway.snapshot_pending_relay_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].tunnel_id, "tun-v2");
        assert_eq!(usage[0].bytes, 37);
        assert_eq!(usage[0].period.len(), 6);
        assert!(usage[0].period.bytes().all(|byte| byte.is_ascii_digit()));

        assert_eq!(
            gateway
                .flush_pending_relay_usage_to_wal()
                .expect("flush V2 usage"),
            1
        );
        assert!(gateway.snapshot_pending_relay_usage().is_empty());
        let batch = wal.snapshot(10).expect("snapshot V2 usage WAL");
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.items[0].tunnel_id, "tun-v2");
        assert_eq!(batch.items[0].bytes, 37);
    }

    #[test]
    fn scope_disconnect_preserves_pending_v2_usage_until_wal_flush() {
        let dir = tempfile::tempdir().expect("usage WAL directory");
        let wal = Arc::new(RelayUsageWal::open(dir.path().join("relay.wal")).expect("open WAL"));
        let gateway = Gateway::new(GatewayP2pConfig::default(), Some(wal.clone()));
        gateway.commit_relay_usage("tun-removed", 19);

        assert_eq!(gateway.disconnect_tunnel_clients("tun-removed"), 0);
        assert_eq!(
            gateway
                .flush_pending_relay_usage_to_wal()
                .expect("flush usage after Scope disconnect"),
            1
        );

        let batch = wal.snapshot(10).expect("snapshot usage WAL");
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.items[0].tunnel_id, "tun-removed");
        assert_eq!(batch.items[0].bytes, 19);
        assert!(
            gateway.relay_quotas.get("tun-removed").is_none(),
            "flushed meter for a removed Scope must not leak process-local state"
        );
    }

    #[test]
    fn relay_usage_wal_replays_only_unacknowledged_records() {
        let path = std::env::temp_dir().join(format!(
            "tp-gateway-v2-wal-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        {
            let wal = RelayUsageWal::open(&path).expect("open WAL");
            wal.record("tun-a", "202608", 10).expect("record first");
            wal.record("tun-b", "202608", 20).expect("record second");
            let first = wal.snapshot(1).expect("snapshot prefix");
            assert_eq!(first.items.len(), 1);
            wal.ack(first.through_seq).expect("ack prefix");
        }
        {
            let wal = RelayUsageWal::open(&path).expect("reopen WAL");
            let remaining = wal.snapshot(10).expect("replay");
            assert_eq!(remaining.items.len(), 1);
            assert_eq!(remaining.items[0].tunnel_id, "tun-b");
            assert_eq!(remaining.items[0].bytes, 20);
        }
        std::fs::remove_file(path).expect("remove test WAL");
    }

    #[tokio::test]
    async fn disconnect_scope_removes_all_live_attachments() {
        let gateway = gateway();
        let _ = v2_conn(&gateway, "tun", "peer-a", "tun-Aaaaaaaa-0");
        let _ = v2_conn(&gateway, "tun", "peer-b", "tun-Bbbbbbbb-0");

        assert_eq!(gateway.disconnect_tunnel_clients("tun"), 2);
        assert!(gateway.clients.get("tun").is_none());
    }
}
