//! Metrics registry: global counters + per-client stats + per-connection records.
//!
//! * Global counters are `AtomicI64` — no lock on the hot path.
//! * Client/connection tables use `DashMap` for lock-free reads.
//! * A background sweeper (optional) marks inactive clients offline after 2 min
//!   and removes them entirely after a configurable grace period.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::Serialize;

/// Write `# HELP` + `# TYPE` metadata for a Prometheus metric family.
/// Both lines are mandatory per exposition format v0.0.4 for the metric
/// to be recognised by prom/grafana-agent/otel-collector.
fn write_header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Escape a Prometheus label value. The exposition format requires `\`,
/// `"`, and `\n` to be backslash-escaped inside `{label="value"}`
/// constructs. client_id / group_id are bounded-length strings we
/// control (nanoid / UUID), so the hot path is effectively a no-op, but
/// the escape is required for any future label whose source is less
/// sanitised.
fn escape_label(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(|c| matches!(c, '\\' | '"' | '\n')) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut buf = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '\\' => buf.push_str("\\\\"),
            '"' => buf.push_str("\\\""),
            '\n' => buf.push_str("\\n"),
            _ => buf.push(ch),
        }
    }
    std::borrow::Cow::Owned(buf)
}

/// Cumulative-bucket histogram (Prometheus exposition format v0.0.4).
///
/// Each `observe(v)` call increments every bucket whose upper bound is
/// `>= v`, plus the implicit `+Inf` bucket and the total `count`. The
/// `sum` field is guarded by a `Mutex<f64>` instead of an
/// `AtomicU64::to_bits` race-prone pseudo-atomic — histograms here are
/// only emitted from cold paths (a couple per P2P session lifecycle, not
/// the per-frame data plane), so the lock is uncontended in practice.
#[derive(Debug)]
pub(crate) struct Histogram {
    /// Upper bounds in ascending order. Validated on construction.
    buckets: Vec<f64>,
    /// One counter per `buckets[i]`. Cumulative — observation `v` bumps
    /// every counter whose upper bound is `>= v`.
    bucket_counts: Vec<AtomicU64>,
    /// Sum of all observed values. `Mutex<f64>` — see struct doc.
    sum: Mutex<f64>,
    /// Total observation count, also rendered as the `+Inf` bucket.
    count: AtomicU64,
}

impl Histogram {
    /// Construct with the given upper bounds (must be ascending and
    /// non-empty). Use Prometheus convention: bounds are inclusive
    /// (`<=`). Validation runs in release builds too — silently wrong
    /// cumulative counts from an unsorted bucket vec are worse than a
    /// loud panic at construction.
    pub(crate) fn new(buckets: Vec<f64>) -> Self {
        assert!(!buckets.is_empty(), "histogram buckets must be non-empty");
        assert!(
            buckets.windows(2).all(|w| w[0] < w[1]),
            "histogram buckets must be strictly ascending"
        );
        let bucket_counts = buckets.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            buckets,
            bucket_counts,
            sum: Mutex::new(0.0),
            count: AtomicU64::new(0),
        }
    }

    /// Record one observation. Increments every cumulative bucket whose
    /// upper bound is `>= value`, plus `count` (the `+Inf` bucket) and
    /// `sum`.
    pub(crate) fn observe(&self, value: f64) {
        for (bound, slot) in self.buckets.iter().zip(self.bucket_counts.iter()) {
            if value <= *bound {
                slot.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        // Lock contention is negligible — emission sites are cold (P2P
        // lifecycle transitions, not data-plane frames). On poison we
        // skip the sum update; counters still reflect the observation
        // so dashboards stay sensible.
        if let Ok(mut s) = self.sum.lock() {
            *s += value;
        }
    }

    /// Render this histogram in Prometheus text exposition format. Emits
    /// `# HELP`, `# TYPE histogram`, one `_bucket{le="X"} N` line per
    /// upper bound, the `+Inf` bucket, `_sum`, and `_count`.
    pub(crate) fn render(&self, name: &str, help: &str) -> String {
        let mut out = String::new();
        write_header(&mut out, name, help, "histogram");
        for (bound, slot) in self.buckets.iter().zip(self.bucket_counts.iter()) {
            let n = slot.load(Ordering::Relaxed);
            // `format_bucket_bound` emits Prometheus-friendly numbers
            // (no trailing `.0` for integer-valued bounds, and `+Inf`
            // is the explicit terminal bucket below).
            let _ = writeln!(
                &mut out,
                "{name}_bucket{{le=\"{}\"}} {}",
                format_bucket_bound(*bound),
                n,
            );
        }
        let total = self.count.load(Ordering::Relaxed);
        let _ = writeln!(&mut out, "{name}_bucket{{le=\"+Inf\"}} {}", total);
        let sum = self.sum.lock().map(|g| *g).unwrap_or(0.0);
        let _ = writeln!(&mut out, "{name}_sum {}", sum);
        let _ = writeln!(&mut out, "{name}_count {}", total);
        out
    }
}

/// Format a histogram bucket bound. Integer-valued bounds render
/// without a trailing `.0` (Prometheus convention); fractional bounds
/// keep the default `{}` float format.
fn format_bucket_bound(b: f64) -> String {
    if b.is_finite() && b.fract() == 0.0 {
        format!("{}", b as i64)
    } else {
        format!("{}", b)
    }
}

/// Global counters. All fields use atomics so updates are lock-free.
#[derive(Debug)]
pub struct Global {
    pub active_connections: AtomicI64,
    pub total_connections: AtomicI64,
    pub bytes_sent: AtomicI64,
    pub bytes_received: AtomicI64,
    pub error_count: AtomicI64,
    pub udp_drops: AtomicI64,
    pub listener_rejects: AtomicI64,
    pub start_time: chrono::DateTime<chrono::Utc>,
    // ---- P2P telemetry ------------------------------------
    // Atomic-per-concrete-label counters, mirroring the rest of `Global`.
    // No DashMap-keyed label model since the label sets are small and
    // closed (defined by `P2pAttemptResult` etc.).
    pub p2p_attempts_success: AtomicI64,
    pub p2p_attempts_nat_fail: AtomicI64,
    pub p2p_attempts_cert_fail: AtomicI64,
    pub p2p_attempts_timeout: AtomicI64,
    pub p2p_path_picks_relay: AtomicI64,
    pub p2p_path_picks_p2p: AtomicI64,
    pub p2p_cert_mismatch: AtomicI64,
    pub p2p_conn_id_migrations_p2p_to_relay: AtomicI64,
    pub p2p_conn_id_migrations_relay_to_p2p: AtomicI64,
    pub p2p_conn_id_dedup: AtomicI64,
    /// Gauge of currently-installed P2P sessions. Centralized at
    /// the `MultiSession::set_p2p` `None ↔ Some` transitions so it tracks
    /// the data plane, not the negotiation state machine.
    pub p2p_active_sessions: AtomicI64,
    /// Cumulative count of Relay → P2p path-scheduler transitions.
    /// Same-kind ticks (relay → relay, p2p → p2p) are not switches and
    /// are not counted.
    pub p2p_path_switches_relay_to_p2p: AtomicI64,
    /// Cumulative count of P2p → Relay path-scheduler transitions.
    pub p2p_path_switches_p2p_to_relay: AtomicI64,
    /// Cumulative bytes routed by `MultiSenderRouter` over the
    /// relay path (sum of payload-frame byte counts on successful
    /// `send`/`try_send`).
    pub p2p_bytes_relay: AtomicI64,
    /// Cumulative bytes routed by `MultiSenderRouter` over the P2P
    /// path.
    pub p2p_bytes_p2p: AtomicI64,
}

impl Default for Global {
    fn default() -> Self {
        Self {
            active_connections: AtomicI64::new(0),
            total_connections: AtomicI64::new(0),
            bytes_sent: AtomicI64::new(0),
            bytes_received: AtomicI64::new(0),
            error_count: AtomicI64::new(0),
            udp_drops: AtomicI64::new(0),
            listener_rejects: AtomicI64::new(0),
            start_time: chrono::Utc::now(),
            p2p_attempts_success: AtomicI64::new(0),
            p2p_attempts_nat_fail: AtomicI64::new(0),
            p2p_attempts_cert_fail: AtomicI64::new(0),
            p2p_attempts_timeout: AtomicI64::new(0),
            p2p_path_picks_relay: AtomicI64::new(0),
            p2p_path_picks_p2p: AtomicI64::new(0),
            p2p_cert_mismatch: AtomicI64::new(0),
            p2p_conn_id_migrations_p2p_to_relay: AtomicI64::new(0),
            p2p_conn_id_migrations_relay_to_p2p: AtomicI64::new(0),
            p2p_conn_id_dedup: AtomicI64::new(0),
            p2p_active_sessions: AtomicI64::new(0),
            p2p_path_switches_relay_to_p2p: AtomicI64::new(0),
            p2p_path_switches_p2p_to_relay: AtomicI64::new(0),
            p2p_bytes_relay: AtomicI64::new(0),
            p2p_bytes_p2p: AtomicI64::new(0),
        }
    }
}

/// Outcome label for `p2p_attempts_total{result=...}`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P2pAttemptResult {
    Success,
    NatFail,
    CertFail,
    Timeout,
}

/// Path label for `p2p_path_picks_total{kind=...}`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P2pPathKind {
    Relay,
    P2p,
}

/// Direction label for `p2p_conn_id_migrations_total{direction=...}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P2pMigrationDir {
    P2pToRelay,
    RelayToP2p,
}

/// Per-client aggregates. Individual fields are atomic; `last_seen`/`is_online`
/// are guarded by `DashMap`'s entry lock since they write structural state.
#[derive(Debug)]
pub struct ClientEntry {
    pub client_id: String,
    pub group_id: parking_lot::RwLock<String>,
    pub active_connections: AtomicI64,
    pub total_connections: AtomicI64,
    pub bytes_sent: AtomicI64,
    pub bytes_received: AtomicI64,
    pub error_count: AtomicI64,
    pub last_seen: parking_lot::RwLock<chrono::DateTime<chrono::Utc>>,
    pub is_online: parking_lot::RwLock<bool>,
}

impl ClientEntry {
    fn new(client_id: String, group_id: String) -> Self {
        Self {
            client_id,
            group_id: parking_lot::RwLock::new(group_id),
            active_connections: AtomicI64::new(0),
            total_connections: AtomicI64::new(0),
            bytes_sent: AtomicI64::new(0),
            bytes_received: AtomicI64::new(0),
            error_count: AtomicI64::new(0),
            last_seen: parking_lot::RwLock::new(chrono::Utc::now()),
            is_online: parking_lot::RwLock::new(true),
        }
    }
}

/// Per-connection record.
#[derive(Debug)]
pub struct ConnectionEntry {
    pub connection_id: String,
    pub client_id: String,
    pub target_host: String,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub bytes_sent: AtomicI64,
    pub bytes_received: AtomicI64,
    pub status: parking_lot::RwLock<String>,
}

/// DTO shapes for the HTTP API (JSON-serializable snapshots).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalSnapshot {
    pub active_connections: i64,
    pub total_connections: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub error_count: i64,
    pub udp_drops: i64,
    pub listener_rejects: i64,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub uptime_secs: i64,
    pub success_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientSnapshot {
    pub client_id: String,
    pub group_id: String,
    pub active_connections: i64,
    pub total_connections: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub error_count: i64,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub is_online: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionSnapshot {
    pub connection_id: String,
    pub client_id: String,
    pub target_host: String,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub status: String,
}

pub struct MetricsManager {
    global: Global,
    clients: DashMap<String, Arc<ClientEntry>>,
    connections: DashMap<String, Arc<ConnectionEntry>>,
    client_conn_ids: DashMap<String, Arc<DashMap<String, ()>>>,
    /// P2P handoff latency in milliseconds — observed at the
    /// `MultiSession::set_p2p(Some)` transition. Buckets cover the
    /// expected hole-punch + QUIC handshake range; >10s = `+Inf`.
    p2p_handoff_latency_ms: Histogram,
    /// P2P session lifetime in seconds — observed at the
    /// `MultiSession::set_p2p(None)` transition AFTER an `Active`
    /// state. Buckets cover seconds-to-hours; >4h = `+Inf`.
    p2p_session_duration_seconds: Histogram,
}

impl MetricsManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            global: Global::default(),
            clients: DashMap::new(),
            connections: DashMap::new(),
            client_conn_ids: DashMap::new(),
            p2p_handoff_latency_ms: Histogram::new(vec![
                50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0,
            ]),
            p2p_session_duration_seconds: Histogram::new(vec![
                30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0,
            ]),
        })
    }

    // --- connection lifecycle ---

    pub fn create_connection(&self, conn_id: &str, client_id: &str, target_host: &str) {
        let entry = Arc::new(ConnectionEntry {
            connection_id: conn_id.into(),
            client_id: client_id.into(),
            target_host: target_host.into(),
            start_time: chrono::Utc::now(),
            bytes_sent: AtomicI64::new(0),
            bytes_received: AtomicI64::new(0),
            status: parking_lot::RwLock::new("active".into()),
        });
        match self.connections.entry(conn_id.into()) {
            Entry::Occupied(_) => {
                tracing::warn!(conn_id, client_id, "duplicate create_connection");
                return;
            }
            Entry::Vacant(v) => {
                v.insert(entry);
            }
        }
        let ids = self
            .client_conn_ids
            .entry(client_id.into())
            .or_insert_with(|| Arc::new(DashMap::new()))
            .clone();
        ids.insert(conn_id.into(), ());
        self.global
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
        self.global
            .total_connections
            .fetch_add(1, Ordering::Relaxed);
        self.ensure_client(client_id, "");
        if let Some(c) = self.clients.get(client_id) {
            c.active_connections.fetch_add(1, Ordering::Relaxed);
            c.total_connections.fetch_add(1, Ordering::Relaxed);
            *c.last_seen.write() = chrono::Utc::now();
            *c.is_online.write() = true;
        }
    }

    pub fn update_connection_bytes(
        &self,
        conn_id: &str,
        client_id: &str,
        bytes_sent: i64,
        bytes_received: i64,
    ) {
        if bytes_sent > 0 {
            self.global
                .bytes_sent
                .fetch_add(bytes_sent, Ordering::Relaxed);
        }
        if bytes_received > 0 {
            self.global
                .bytes_received
                .fetch_add(bytes_received, Ordering::Relaxed);
        }
        if let Some(c) = self.connections.get(conn_id) {
            if bytes_sent > 0 {
                c.bytes_sent.fetch_add(bytes_sent, Ordering::Relaxed);
            }
            if bytes_received > 0 {
                c.bytes_received
                    .fetch_add(bytes_received, Ordering::Relaxed);
            }
        }
        if let Some(cl) = self.clients.get(client_id) {
            if bytes_sent > 0 {
                cl.bytes_sent.fetch_add(bytes_sent, Ordering::Relaxed);
            }
            if bytes_received > 0 {
                cl.bytes_received
                    .fetch_add(bytes_received, Ordering::Relaxed);
            }
        }
    }

    pub fn close_connection(&self, conn_id: &str) {
        if let Some((_, entry)) = self.connections.remove(conn_id) {
            self.global
                .active_connections
                .fetch_sub(1, Ordering::Relaxed);
            if let Some(c) = self.clients.get(&entry.client_id) {
                c.active_connections.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(ids) = self.client_conn_ids.get(&entry.client_id) {
                ids.remove(conn_id);
            }
        }
    }

    // --- client lifecycle ---

    fn ensure_client(&self, client_id: &str, group_id: &str) {
        if self.clients.contains_key(client_id) {
            return;
        }
        self.clients.insert(
            client_id.into(),
            Arc::new(ClientEntry::new(client_id.into(), group_id.into())),
        );
    }

    pub fn update_client_heartbeat(&self, client_id: &str, group_id: &str) {
        self.ensure_client(client_id, group_id);
        if let Some(c) = self.clients.get(client_id) {
            *c.last_seen.write() = chrono::Utc::now();
            *c.is_online.write() = true;
            if !group_id.is_empty() {
                *c.group_id.write() = group_id.into();
            }
        }
    }

    pub fn mark_client_offline(&self, client_id: &str) {
        if let Some(c) = self.clients.get(client_id) {
            *c.is_online.write() = false;
            *c.last_seen.write() = chrono::Utc::now();
        }
    }

    pub fn increment_errors(&self, client_id: Option<&str>) {
        self.global.error_count.fetch_add(1, Ordering::Relaxed);
        if let Some(cid) = client_id {
            if let Some(c) = self.clients.get(cid) {
                c.error_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn increment_udp_drops(&self) {
        self.global.udp_drops.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_listener_rejects(&self) {
        self.global.listener_rejects.fetch_add(1, Ordering::Relaxed);
    }

    // ---- P2P counters -----------------------------------------
    //
    // Always-on if compiled in. Task 4.13 will add a config gate; until
    // then the cost is one relaxed atomic add per call site, so cheap
    // enough to leave unconditional.

    /// Record the outcome of a P2P attempt (Announce → Active or failure).
    pub fn incr_p2p_attempt(&self, result: P2pAttemptResult) {
        let slot = match result {
            P2pAttemptResult::Success => &self.global.p2p_attempts_success,
            P2pAttemptResult::NatFail => &self.global.p2p_attempts_nat_fail,
            P2pAttemptResult::CertFail => &self.global.p2p_attempts_cert_fail,
            P2pAttemptResult::Timeout => &self.global.p2p_attempts_timeout,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a per-frame path-scheduler decision.
    pub fn incr_p2p_path_pick(&self, kind: P2pPathKind) {
        let slot = match kind {
            P2pPathKind::Relay => &self.global.p2p_path_picks_relay,
            P2pPathKind::P2p => &self.global.p2p_path_picks_p2p,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a P2P TLS cert-fingerprint mismatch on inbound connect.
    pub fn incr_p2p_cert_mismatch(&self) {
        self.global
            .p2p_cert_mismatch
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record `n` conn_ids migrating between paths during a teardown event.
    pub fn incr_p2p_conn_id_migrations(&self, dir: P2pMigrationDir, n: i64) {
        if n <= 0 {
            return;
        }
        let slot = match dir {
            P2pMigrationDir::P2pToRelay => &self.global.p2p_conn_id_migrations_p2p_to_relay,
            P2pMigrationDir::RelayToP2p => &self.global.p2p_conn_id_migrations_relay_to_p2p,
        };
        slot.fetch_add(n, Ordering::Relaxed);
    }

    /// Record a `Connect`-dedup hit (gateway re-issued Connect after a
    /// path-flip; the local target socket was already live).
    pub fn incr_p2p_conn_id_dedup(&self) {
        self.global
            .p2p_conn_id_dedup
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record one P2P handoff-latency sample (Negotiating →
    /// data-plane carrying a P2P session). Caller passes elapsed
    /// milliseconds.
    pub fn observe_p2p_handoff_latency_ms(&self, ms: f64) {
        self.p2p_handoff_latency_ms.observe(ms);
    }

    /// Record one P2P session-duration sample (Active →
    /// `set_p2p(None)`). Caller passes elapsed seconds.
    pub fn observe_p2p_session_duration_secs(&self, secs: f64) {
        self.p2p_session_duration_seconds.observe(secs);
    }

    /// Adjust the `p2p_active_sessions` gauge by `delta` (typically
    /// `+1` on `MultiSession::set_p2p` install, `-1` on clear). Idempotent
    /// re-installs (`Some → Some`) MUST pass `0` or skip the call.
    pub fn observe_p2p_active_sessions(&self, delta: i64) {
        if delta == 0 {
            return;
        }
        self.global
            .p2p_active_sessions
            .fetch_add(delta, Ordering::Relaxed);
    }

    /// Record a path-scheduler transition. Caller already filtered
    /// out same-kind ticks; `from == to` is a logic error and is
    /// debug-asserted (release builds silently no-op).
    pub fn incr_p2p_path_switch(&self, from: P2pPathKind, to: P2pPathKind) {
        debug_assert_ne!(
            from, to,
            "incr_p2p_path_switch called with from == to ({from:?}); same-kind is not a switch"
        );
        let slot = match (from, to) {
            (P2pPathKind::Relay, P2pPathKind::P2p) => &self.global.p2p_path_switches_relay_to_p2p,
            (P2pPathKind::P2p, P2pPathKind::Relay) => &self.global.p2p_path_switches_p2p_to_relay,
            _ => return,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    /// Record `n` bytes sent on `kind`. Caller passes the byte
    /// count of the just-sent frame; only the data-plane byte fields
    /// (`Data` / `UdpData` payload) are summed since they dominate
    /// bandwidth and control frames would skew the per-path ratio.
    pub fn incr_p2p_bytes(&self, kind: P2pPathKind, n: i64) {
        if n <= 0 {
            return;
        }
        let slot = match kind {
            P2pPathKind::Relay => &self.global.p2p_bytes_relay,
            P2pPathKind::P2p => &self.global.p2p_bytes_p2p,
        };
        slot.fetch_add(n, Ordering::Relaxed);
    }

    /// Cheap accessor: current (bytes_sent, bytes_received) for a client,
    /// or `(0, 0)` if the client has no entry yet. Useful for callers that
    /// want to log a per-session delta without holding a full `ClientSnapshot`.
    pub fn client_byte_totals(&self, client_id: &str) -> (i64, i64) {
        self.clients
            .get(client_id)
            .map(|c| {
                (
                    c.bytes_sent.load(Ordering::Relaxed),
                    c.bytes_received.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0))
    }

    // --- snapshots ---

    pub fn global(&self) -> GlobalSnapshot {
        let total = self.global.total_connections.load(Ordering::Relaxed);
        let errors = self.global.error_count.load(Ordering::Relaxed);
        let success_rate = if total == 0 {
            100.0
        } else {
            (total.saturating_sub(errors)) as f64 / total as f64 * 100.0
        };
        let uptime = (chrono::Utc::now() - self.global.start_time).num_seconds();
        GlobalSnapshot {
            active_connections: self.global.active_connections.load(Ordering::Relaxed),
            total_connections: total,
            bytes_sent: self.global.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.global.bytes_received.load(Ordering::Relaxed),
            error_count: errors,
            udp_drops: self.global.udp_drops.load(Ordering::Relaxed),
            listener_rejects: self.global.listener_rejects.load(Ordering::Relaxed),
            start_time: self.global.start_time,
            uptime_secs: uptime,
            success_rate,
        }
    }

    pub fn client(&self, client_id: &str) -> Option<ClientSnapshot> {
        self.clients.get(client_id).map(|entry| {
            let is_online = *entry.is_online.read()
                && (chrono::Utc::now() - *entry.last_seen.read()).num_seconds() <= 120;
            ClientSnapshot {
                client_id: entry.client_id.clone(),
                group_id: entry.group_id.read().clone(),
                active_connections: if is_online {
                    entry.active_connections.load(Ordering::Relaxed)
                } else {
                    0
                },
                total_connections: entry.total_connections.load(Ordering::Relaxed),
                bytes_sent: entry.bytes_sent.load(Ordering::Relaxed),
                bytes_received: entry.bytes_received.load(Ordering::Relaxed),
                error_count: entry.error_count.load(Ordering::Relaxed),
                last_seen: *entry.last_seen.read(),
                is_online,
            }
        })
    }

    pub fn all_clients(&self) -> Vec<ClientSnapshot> {
        let mut out = Vec::with_capacity(self.clients.len());
        for entry in self.clients.iter() {
            let is_online = *entry.is_online.read()
                && (chrono::Utc::now() - *entry.last_seen.read()).num_seconds() <= 120;
            out.push(ClientSnapshot {
                client_id: entry.client_id.clone(),
                group_id: entry.group_id.read().clone(),
                active_connections: if is_online {
                    entry.active_connections.load(Ordering::Relaxed)
                } else {
                    0
                },
                total_connections: entry.total_connections.load(Ordering::Relaxed),
                bytes_sent: entry.bytes_sent.load(Ordering::Relaxed),
                bytes_received: entry.bytes_received.load(Ordering::Relaxed),
                error_count: entry.error_count.load(Ordering::Relaxed),
                last_seen: *entry.last_seen.read(),
                is_online,
            });
        }
        out
    }

    pub fn all_connections(&self) -> Vec<ConnectionSnapshot> {
        self.connections
            .iter()
            .map(|c| ConnectionSnapshot {
                connection_id: c.connection_id.clone(),
                client_id: c.client_id.clone(),
                target_host: c.target_host.clone(),
                start_time: c.start_time,
                bytes_sent: c.bytes_sent.load(Ordering::Relaxed),
                bytes_received: c.bytes_received.load(Ordering::Relaxed),
                status: c.status.read().clone(),
            })
            .collect()
    }

    pub fn connection(&self, conn_id: &str) -> Option<ConnectionSnapshot> {
        self.connections.get(conn_id).map(|c| ConnectionSnapshot {
            connection_id: c.connection_id.clone(),
            client_id: c.client_id.clone(),
            target_host: c.target_host.clone(),
            start_time: c.start_time,
            bytes_sent: c.bytes_sent.load(Ordering::Relaxed),
            bytes_received: c.bytes_received.load(Ordering::Relaxed),
            status: c.status.read().clone(),
        })
    }

    pub fn connections_for_client(&self, client_id: &str) -> Vec<ConnectionSnapshot> {
        let Some(ids) = self.client_conn_ids.get(client_id) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| self.connection(id.key()))
            .collect()
    }

    // --- sweeper ---

    /// Mark clients offline after 2 min idle; remove after `max_offline` idle.
    /// Also drops stale connections owned by offline clients.
    pub fn cleanup(&self, max_offline: Duration) {
        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::from_std(max_offline).unwrap_or_default();
        let mut to_remove = Vec::new();
        for e in self.clients.iter() {
            let last = *e.last_seen.read();
            let mut online = e.is_online.write();
            if *online && (now - last).num_seconds() > 120 {
                *online = false;
            }
            if !*online && last < cutoff {
                to_remove.push(e.client_id.clone());
            }
        }
        for cid in &to_remove {
            self.clients.remove(cid);
            if let Some((_, ids)) = self.client_conn_ids.remove(cid) {
                let stale: Vec<String> = ids.iter().map(|c| c.key().clone()).collect();
                for cn in stale {
                    self.close_connection(&cn);
                }
            }
        }
    }

    /// Spawn a sweeper that calls `cleanup` every 10s.
    pub fn spawn_sweeper(self: &Arc<Self>, max_offline: Duration) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(10));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                t.tick().await;
                me.cleanup(max_offline);
            }
        })
    }

    /// Render every counter in Prometheus text exposition format v0.0.4
    /// (https://prometheus.io/docs/instrumenting/exposition_formats/).
    ///
    /// Emitted families:
    ///   * Global gauges/counters (active_connections, total_connections,
    ///     bytes_{sent,received}, error_count, uptime_seconds).
    ///   * Per-client counters labelled by (client_id, group_id) so the
    ///     standard Prometheus grouping operators (sum by / topk) just
    ///     work.
    ///
    /// Per-connection snapshots are **intentionally omitted**: connection
    /// cardinality is unbounded in a busy gateway and blows up Prometheus'
    /// TSDB. Use the existing JSON `/api/metrics/connections` endpoint for
    /// that level of inspection.
    pub fn prometheus_text(&self) -> String {
        let g = self.global();
        let clients = self.all_clients();

        // Rough capacity: header pairs (~80 B each) * ~12 lines + per-client
        // lines (~120 B each * 5 metrics) + fixed values. One alloc keeps
        // the handler off the hot path.
        let mut out = String::with_capacity(2048 + clients.len() * 600);

        write_header(
            &mut out,
            "tp_active_connections",
            "In-flight tunnel connections right now",
            "gauge",
        );
        writeln!(&mut out, "tp_active_connections {}", g.active_connections).ok();

        write_header(
            &mut out,
            "tp_total_connections",
            "All tunnel connections since process start",
            "counter",
        );
        writeln!(&mut out, "tp_total_connections {}", g.total_connections).ok();

        write_header(
            &mut out,
            "tp_bytes_sent",
            "Bytes pushed by this process to downstream targets",
            "counter",
        );
        writeln!(&mut out, "tp_bytes_sent {}", g.bytes_sent).ok();

        write_header(
            &mut out,
            "tp_bytes_received",
            "Bytes received by this process from downstream targets",
            "counter",
        );
        writeln!(&mut out, "tp_bytes_received {}", g.bytes_received).ok();

        write_header(
            &mut out,
            "tp_errors",
            "Tunnel-level errors since process start",
            "counter",
        );
        writeln!(&mut out, "tp_errors {}", g.error_count).ok();

        write_header(
            &mut out,
            "tp_udp_drops",
            "UDP packets intentionally dropped by bounded queues or rate limits",
            "counter",
        );
        writeln!(&mut out, "tp_udp_drops {}", g.udp_drops).ok();

        write_header(
            &mut out,
            "tp_listener_rejects",
            "Connections rejected by listener backpressure limits",
            "counter",
        );
        writeln!(&mut out, "tp_listener_rejects {}", g.listener_rejects).ok();

        write_header(
            &mut out,
            "tp_uptime_seconds",
            "Process uptime in seconds",
            "gauge",
        );
        writeln!(&mut out, "tp_uptime_seconds {}", g.uptime_secs).ok();

        // ---- P2P counters -------------------------------------
        // Single family per metric name; `result` / `kind` / `direction`
        // labels expand to one line per concrete value so PromQL can sum
        // by(label) just like a real label-typed counter.
        let p2p_attempts_success = self.global.p2p_attempts_success.load(Ordering::Relaxed);
        let p2p_attempts_nat_fail = self.global.p2p_attempts_nat_fail.load(Ordering::Relaxed);
        let p2p_attempts_cert_fail = self.global.p2p_attempts_cert_fail.load(Ordering::Relaxed);
        let p2p_attempts_timeout = self.global.p2p_attempts_timeout.load(Ordering::Relaxed);
        write_header(
            &mut out,
            "p2p_attempts_total",
            "P2P attempt outcomes labelled by result",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_attempts_total{{result=\"success\"}} {}",
            p2p_attempts_success
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_attempts_total{{result=\"nat_fail\"}} {}",
            p2p_attempts_nat_fail
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_attempts_total{{result=\"cert_fail\"}} {}",
            p2p_attempts_cert_fail
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_attempts_total{{result=\"timeout\"}} {}",
            p2p_attempts_timeout
        )
        .ok();

        let p2p_path_picks_relay = self.global.p2p_path_picks_relay.load(Ordering::Relaxed);
        let p2p_path_picks_p2p = self.global.p2p_path_picks_p2p.load(Ordering::Relaxed);
        write_header(
            &mut out,
            "p2p_path_picks_total",
            "Per-frame path-scheduler decisions",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_path_picks_total{{kind=\"relay\"}} {}",
            p2p_path_picks_relay
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_path_picks_total{{kind=\"p2p\"}} {}",
            p2p_path_picks_p2p
        )
        .ok();

        write_header(
            &mut out,
            "p2p_cert_mismatch_total",
            "P2P TLS cert-fingerprint mismatches on inbound connect",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_cert_mismatch_total {}",
            self.global.p2p_cert_mismatch.load(Ordering::Relaxed)
        )
        .ok();

        let p2p_mig_p2r = self
            .global
            .p2p_conn_id_migrations_p2p_to_relay
            .load(Ordering::Relaxed);
        let p2p_mig_r2p = self
            .global
            .p2p_conn_id_migrations_relay_to_p2p
            .load(Ordering::Relaxed);
        write_header(
            &mut out,
            "p2p_conn_id_migrations_total",
            "conn_ids migrating between paths during a teardown event",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_conn_id_migrations_total{{direction=\"p2p_to_relay\"}} {}",
            p2p_mig_p2r
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_conn_id_migrations_total{{direction=\"relay_to_p2p\"}} {}",
            p2p_mig_r2p
        )
        .ok();

        write_header(
            &mut out,
            "p2p_conn_id_dedup_total",
            "Connect-dedup hits when gateway re-issues Connect after a path-flip",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_conn_id_dedup_total {}",
            self.global.p2p_conn_id_dedup.load(Ordering::Relaxed)
        )
        .ok();

        // P2P lifecycle histograms. Render after the counter
        // family so the `# HELP`/`# TYPE histogram` directive groups
        // the bucket+sum+count lines together.
        out.push_str(&self.p2p_handoff_latency_ms.render(
            "p2p_handoff_latency_ms",
            "Time from P2P negotiation start to first installed P2P session, in milliseconds",
        ));
        out.push_str(&self.p2p_session_duration_seconds.render(
            "p2p_session_duration_seconds",
            "Lifetime of a P2P session that reached Active, in seconds",
        ));

        // Live P2P session count.
        write_header(
            &mut out,
            "p2p_active_sessions",
            "Currently-installed P2P sessions across MultiSession instances",
            "gauge",
        );
        writeln!(
            &mut out,
            "p2p_active_sessions {}",
            self.global.p2p_active_sessions.load(Ordering::Relaxed)
        )
        .ok();

        // Path-scheduler transitions. One line per `(from, to)`
        // pair we count; same-kind ticks are intentionally absent.
        let p2p_switch_r2p = self
            .global
            .p2p_path_switches_relay_to_p2p
            .load(Ordering::Relaxed);
        let p2p_switch_p2r = self
            .global
            .p2p_path_switches_p2p_to_relay
            .load(Ordering::Relaxed);
        write_header(
            &mut out,
            "p2p_path_switches_total",
            "Path-scheduler transitions between Relay and P2P (same-kind ticks excluded)",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_path_switches_total{{from=\"relay\",to=\"p2p\"}} {}",
            p2p_switch_r2p
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_path_switches_total{{from=\"p2p\",to=\"relay\"}} {}",
            p2p_switch_p2r
        )
        .ok();

        // Per-path data-plane bytes routed by MultiSenderRouter.
        let p2p_bytes_relay = self.global.p2p_bytes_relay.load(Ordering::Relaxed);
        let p2p_bytes_p2p = self.global.p2p_bytes_p2p.load(Ordering::Relaxed);
        write_header(
            &mut out,
            "p2p_bytes_total",
            "Data-plane bytes sent by MultiSenderRouter labelled by chosen path",
            "counter",
        );
        writeln!(
            &mut out,
            "p2p_bytes_total{{path=\"relay\"}} {}",
            p2p_bytes_relay
        )
        .ok();
        writeln!(
            &mut out,
            "p2p_bytes_total{{path=\"p2p\"}} {}",
            p2p_bytes_p2p
        )
        .ok();

        write_header(
            &mut out,
            "tp_client_active_connections",
            "Per-client in-flight connections",
            "gauge",
        );
        for c in &clients {
            writeln!(
                &mut out,
                "tp_client_active_connections{{client_id=\"{}\",group_id=\"{}\"}} {}",
                escape_label(&c.client_id),
                escape_label(&c.group_id),
                c.active_connections,
            )
            .ok();
        }

        write_header(
            &mut out,
            "tp_client_bytes_sent",
            "Per-client bytes sent",
            "counter",
        );
        for c in &clients {
            writeln!(
                &mut out,
                "tp_client_bytes_sent{{client_id=\"{}\",group_id=\"{}\"}} {}",
                escape_label(&c.client_id),
                escape_label(&c.group_id),
                c.bytes_sent,
            )
            .ok();
        }

        write_header(
            &mut out,
            "tp_client_bytes_received",
            "Per-client bytes received",
            "counter",
        );
        for c in &clients {
            writeln!(
                &mut out,
                "tp_client_bytes_received{{client_id=\"{}\",group_id=\"{}\"}} {}",
                escape_label(&c.client_id),
                escape_label(&c.group_id),
                c.bytes_received,
            )
            .ok();
        }

        write_header(&mut out, "tp_client_errors", "Per-client errors", "counter");
        for c in &clients {
            writeln!(
                &mut out,
                "tp_client_errors{{client_id=\"{}\",group_id=\"{}\"}} {}",
                escape_label(&c.client_id),
                escape_label(&c.group_id),
                c.error_count,
            )
            .ok();
        }

        write_header(
            &mut out,
            "tp_client_online",
            "Per-client online flag (1 = online, 0 = offline)",
            "gauge",
        );
        for c in &clients {
            writeln!(
                &mut out,
                "tp_client_online{{client_id=\"{}\",group_id=\"{}\"}} {}",
                escape_label(&c.client_id),
                escape_label(&c.group_id),
                if c.is_online { 1 } else { 0 },
            )
            .ok();
        }

        out
    }

    /// Periodically emit a single `info!` line summarizing global counters so
    /// operators can eyeball gateway health from the log without polling the
    /// HTTP metrics API. Each tick reports the delta since the previous tick
    /// (bytes/s, new conns, new errors) alongside absolute totals.
    pub fn spawn_summary_logger(
        self: &Arc<Self>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(interval);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; skip it so the first emitted line
            // has a real delta window.
            t.tick().await;
            let mut prev_sent: i64 = 0;
            let mut prev_recv: i64 = 0;
            let mut prev_total: i64 = 0;
            let mut prev_errors: i64 = 0;
            let period_secs = interval.as_secs().max(1) as i64;
            loop {
                t.tick().await;
                let g = me.global();
                let client_count = me.clients.len();
                let online_count = me.clients.iter().filter(|e| *e.is_online.read()).count();
                let d_sent = (g.bytes_sent - prev_sent).max(0);
                let d_recv = (g.bytes_received - prev_recv).max(0);
                let d_total = (g.total_connections - prev_total).max(0);
                let d_err = (g.error_count - prev_errors).max(0);
                tracing::info!(
                    active_conns = g.active_connections,
                    total_conns = g.total_connections,
                    new_conns = d_total,
                    bytes_sent = g.bytes_sent,
                    bytes_recv = g.bytes_received,
                    sent_bps = d_sent / period_secs,
                    recv_bps = d_recv / period_secs,
                    errors = g.error_count,
                    new_errors = d_err,
                    udp_drops = g.udp_drops,
                    listener_rejects = g.listener_rejects,
                    success_rate = format!("{:.2}%", g.success_rate),
                    uptime_secs = g.uptime_secs,
                    clients = client_count,
                    clients_online = online_count,
                    "metrics summary"
                );
                prev_sent = g.bytes_sent;
                prev_recv = g.bytes_received;
                prev_total = g.total_connections;
                prev_errors = g.error_count;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_counters() {
        let m = MetricsManager::new();
        m.create_connection("c1", "client-a", "example.com:443");
        assert_eq!(m.global().active_connections, 1);
        assert_eq!(m.global().total_connections, 1);
        m.update_connection_bytes("c1", "client-a", 100, 50);
        let g = m.global();
        assert_eq!(g.bytes_sent, 100);
        assert_eq!(g.bytes_received, 50);
        m.close_connection("c1");
        assert_eq!(m.global().active_connections, 0);
    }

    #[test]
    fn success_rate_with_errors() {
        let m = MetricsManager::new();
        m.create_connection("a", "c", "x");
        m.create_connection("b", "c", "x");
        m.increment_errors(Some("c"));
        let g = m.global();
        assert_eq!(g.error_count, 1);
        assert!((g.success_rate - 50.0).abs() < 0.001);
    }

    /// Prometheus exposition format v0.0.4 requires `# HELP` + `# TYPE`
    /// lines for every family and the metric names/values on their own
    /// lines. The parser rejects families missing either directive, so
    /// this test is a canary against any future refactor that drops a
    /// header pair.
    #[test]
    fn prometheus_text_shape_is_parseable() {
        let m = MetricsManager::new();
        m.create_connection("conn-1", "client-a", "example.com:443");
        m.update_connection_bytes("conn-1", "client-a", 1024, 2048);
        m.update_client_heartbeat("client-a", "group-z");

        let out = m.prometheus_text();

        // Global shape — every gauge/counter has matching HELP+TYPE+value.
        for name in &[
            "tp_active_connections",
            "tp_total_connections",
            "tp_bytes_sent",
            "tp_bytes_received",
            "tp_errors",
            "tp_uptime_seconds",
        ] {
            assert!(
                out.contains(&format!("# HELP {name} ")),
                "missing HELP for {name}:\n{out}"
            );
            assert!(
                out.contains(&format!("# TYPE {name} ")),
                "missing TYPE for {name}:\n{out}"
            );
        }

        // Per-client values carry the client_id + group_id labels we
        // promised so grafana can group by client/group.
        assert!(
            out.contains("tp_client_bytes_sent{client_id=\"client-a\",group_id=\"group-z\"} 1024"),
            "per-client bytes_sent line missing/wrong:\n{out}"
        );
        assert!(
            out.contains(
                "tp_client_bytes_received{client_id=\"client-a\",group_id=\"group-z\"} 2048"
            ),
            "per-client bytes_received line missing/wrong:\n{out}"
        );
    }

    /// Render-test for the P2P counter family. Bumps the
    /// success counter twice and verifies both the line shape (label
    /// included) and the count are visible in `prometheus_text`. Regression
    /// guard against future refactors of the P2P render block.
    #[test]
    fn p2p_attempt_counter_renders_in_prometheus_text() {
        let m = MetricsManager::new();
        m.incr_p2p_attempt(P2pAttemptResult::Success);
        m.incr_p2p_attempt(P2pAttemptResult::Success);
        m.incr_p2p_attempt(P2pAttemptResult::CertFail);
        m.incr_p2p_path_pick(P2pPathKind::P2p);
        m.incr_p2p_cert_mismatch();
        m.incr_p2p_conn_id_dedup();
        m.incr_p2p_conn_id_migrations(P2pMigrationDir::P2pToRelay, 3);

        let text = m.prometheus_text();

        assert!(
            text.contains("# TYPE p2p_attempts_total counter"),
            "missing type header for p2p_attempts_total:\n{text}"
        );
        assert!(
            text.contains("p2p_attempts_total{result=\"success\"} 2"),
            "expected success=2 line:\n{text}"
        );
        assert!(
            text.contains("p2p_attempts_total{result=\"cert_fail\"} 1"),
            "expected cert_fail=1 line:\n{text}"
        );
        assert!(
            text.contains("p2p_path_picks_total{kind=\"p2p\"} 1"),
            "expected path_picks p2p=1 line:\n{text}"
        );
        assert!(
            text.contains("p2p_cert_mismatch_total 1"),
            "expected cert_mismatch_total=1 line:\n{text}"
        );
        assert!(
            text.contains("p2p_conn_id_dedup_total 1"),
            "expected conn_id_dedup_total=1 line:\n{text}"
        );
        assert!(
            text.contains("p2p_conn_id_migrations_total{direction=\"p2p_to_relay\"} 3"),
            "expected migration p2p_to_relay=3 line:\n{text}"
        );
    }

    /// Cumulative-bucket semantics — observation `v` increments
    /// EVERY bucket whose upper bound `>= v`, plus `+Inf` and count.
    #[test]
    fn histogram_observes_into_correct_buckets() {
        let h = Histogram::new(vec![100.0, 200.0, 500.0]);
        h.observe(50.0); // hits 100, 200, 500
        h.observe(100.0); // hits 100, 200, 500 (`<=` semantics)
        h.observe(250.0); // hits 500 only

        assert_eq!(h.bucket_counts[0].load(Ordering::Relaxed), 2, "le=100");
        assert_eq!(h.bucket_counts[1].load(Ordering::Relaxed), 2, "le=200");
        assert_eq!(h.bucket_counts[2].load(Ordering::Relaxed), 3, "le=500");
        assert_eq!(h.count.load(Ordering::Relaxed), 3);
        let sum = *h.sum.lock().unwrap();
        assert!((sum - 400.0).abs() < 1e-9, "sum should be 400.0; got {sum}");
    }

    /// An observation above the largest bucket only lands in
    /// `+Inf` / count. The named buckets must stay at 0.
    #[test]
    fn histogram_observe_above_max_bucket_only_inf() {
        let h = Histogram::new(vec![100.0, 200.0, 500.0]);
        h.observe(10000.0);

        assert_eq!(h.bucket_counts[0].load(Ordering::Relaxed), 0, "le=100");
        assert_eq!(h.bucket_counts[1].load(Ordering::Relaxed), 0, "le=200");
        assert_eq!(h.bucket_counts[2].load(Ordering::Relaxed), 0, "le=500");
        assert_eq!(h.count.load(Ordering::Relaxed), 1, "+Inf == count");
    }

    /// Render output must match the Prometheus histogram text
    /// schema (HELP/TYPE/_bucket/_sum/_count), without pinning exact
    /// float formatting.
    #[test]
    fn histogram_render_matches_prometheus_format() {
        let h = Histogram::new(vec![100.0, 500.0]);
        h.observe(50.0);
        h.observe(200.0);
        h.observe(1000.0);

        let out = h.render("test_hist", "doc");

        assert!(out.contains("# HELP test_hist doc"), "missing HELP:\n{out}");
        assert!(
            out.contains("# TYPE test_hist histogram"),
            "missing TYPE:\n{out}"
        );
        assert!(
            out.contains("test_hist_bucket{le=\"100\"} 1"),
            "le=100 row wrong:\n{out}"
        );
        assert!(
            out.contains("test_hist_bucket{le=\"500\"} 2"),
            "le=500 row wrong:\n{out}"
        );
        assert!(
            out.contains("test_hist_bucket{le=\"+Inf\"} 3"),
            "+Inf row wrong:\n{out}"
        );
        assert!(out.contains("test_hist_count 3"), "_count wrong:\n{out}");
        assert!(out.contains("test_hist_sum "), "_sum line missing:\n{out}");
    }

    /// `MetricsManager` exposes the two P2P histograms and renders
    /// them under their canonical names in `prometheus_text`.
    #[test]
    fn p2p_histograms_render_in_prometheus_text() {
        let m = MetricsManager::new();
        m.observe_p2p_handoff_latency_ms(450.0);
        m.observe_p2p_session_duration_secs(120.0);
        let text = m.prometheus_text();
        assert!(
            text.contains("# TYPE p2p_handoff_latency_ms histogram"),
            "missing handoff TYPE:\n{text}"
        );
        assert!(
            text.contains("p2p_handoff_latency_ms_count 1"),
            "handoff count wrong:\n{text}"
        );
        assert!(
            text.contains("# TYPE p2p_session_duration_seconds histogram"),
            "missing duration TYPE:\n{text}"
        );
        assert!(
            text.contains("p2p_session_duration_seconds_count 1"),
            "duration count wrong:\n{text}"
        );
    }

    /// The `p2p_active_sessions` gauge tracks centralized
    /// inc/dec calls and renders the live count between transitions.
    /// Pre-fix nothing exposed P2P slot occupancy without inspecting
    /// every `MultiSession`; the gauge is a single number per process.
    #[test]
    fn p2p_active_sessions_gauge_increments_and_decrements() {
        let m = MetricsManager::new();

        // Initial state: rendered as 0 with the right header pair.
        let text0 = m.prometheus_text();
        assert!(
            text0.contains("# TYPE p2p_active_sessions gauge"),
            "missing gauge TYPE header:\n{text0}"
        );
        assert!(
            text0.contains("p2p_active_sessions 0"),
            "initial gauge value must be 0:\n{text0}"
        );

        // Two installs: gauge climbs to 2.
        m.observe_p2p_active_sessions(1);
        m.observe_p2p_active_sessions(1);
        let text1 = m.prometheus_text();
        assert!(
            text1.contains("p2p_active_sessions 2"),
            "gauge must reflect two installs:\n{text1}"
        );

        // One clear: gauge drops to 1.
        m.observe_p2p_active_sessions(-1);
        let text2 = m.prometheus_text();
        assert!(
            text2.contains("p2p_active_sessions 1"),
            "gauge must drop to 1 after one clear:\n{text2}"
        );

        // delta=0 is a no-op (e.g. Some→Some idempotent path).
        m.observe_p2p_active_sessions(0);
        let text3 = m.prometheus_text();
        assert!(
            text3.contains("p2p_active_sessions 1"),
            "delta=0 must not change the gauge:\n{text3}"
        );
    }

    /// `incr_p2p_path_switch` only counts the two cross-kind
    /// transitions; same-kind ticks (relay→relay, p2p→p2p) are not
    /// switches. The setter is exercised directly to keep the metric
    /// independent of scheduler internals.
    #[test]
    fn p2p_path_switches_increments_only_on_transition() {
        let m = MetricsManager::new();

        m.incr_p2p_path_switch(P2pPathKind::Relay, P2pPathKind::P2p);
        m.incr_p2p_path_switch(P2pPathKind::Relay, P2pPathKind::P2p);
        m.incr_p2p_path_switch(P2pPathKind::P2p, P2pPathKind::Relay);

        let text = m.prometheus_text();
        assert!(
            text.contains("# TYPE p2p_path_switches_total counter"),
            "missing counter TYPE header:\n{text}"
        );
        assert!(
            text.contains("p2p_path_switches_total{from=\"relay\",to=\"p2p\"} 2"),
            "expected 2 relay→p2p switches:\n{text}"
        );
        assert!(
            text.contains("p2p_path_switches_total{from=\"p2p\",to=\"relay\"} 1"),
            "expected 1 p2p→relay switch:\n{text}"
        );
    }

    /// `incr_p2p_bytes` totals bytes per chosen path. Negative or
    /// zero `n` is dropped (matches `incr_p2p_conn_id_migrations`
    /// convention) so a frame with empty payload doesn't pollute the
    /// counter.
    #[test]
    fn p2p_bytes_total_per_path_label() {
        let m = MetricsManager::new();
        m.incr_p2p_bytes(P2pPathKind::Relay, 1024);
        m.incr_p2p_bytes(P2pPathKind::Relay, 256);
        m.incr_p2p_bytes(P2pPathKind::P2p, 4096);
        m.incr_p2p_bytes(P2pPathKind::P2p, 0); // no-op
        m.incr_p2p_bytes(P2pPathKind::Relay, -8); // no-op

        let text = m.prometheus_text();
        assert!(
            text.contains("# TYPE p2p_bytes_total counter"),
            "missing counter TYPE header:\n{text}"
        );
        assert!(
            text.contains("p2p_bytes_total{path=\"relay\"} 1280"),
            "relay total must be 1280 (1024+256):\n{text}"
        );
        assert!(
            text.contains("p2p_bytes_total{path=\"p2p\"} 4096"),
            "p2p total must be 4096:\n{text}"
        );
    }

    /// Label values containing `\`, `"`, or `\n` must be escaped per the
    /// exposition format. Regression guard for operators who name clients
    /// via an external billing or CRM system where quote/newline
    /// injection is possible.
    #[test]
    fn prometheus_label_values_are_escaped() {
        let m = MetricsManager::new();
        m.update_client_heartbeat("weird\"id\\", "g\nroup");
        let out = m.prometheus_text();
        assert!(
            out.contains("client_id=\"weird\\\"id\\\\\""),
            "client_id escape wrong:\n{out}"
        );
        assert!(
            out.contains("group_id=\"g\\nroup\""),
            "group_id newline escape wrong:\n{out}"
        );
    }
}
