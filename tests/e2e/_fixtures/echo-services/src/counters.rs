//! Shared atomic counters for the echo services.
//!
//! Counters are stats, not synchronisation primitives, so all increments and
//! loads use `Ordering::Relaxed`. The `Counters` handle wraps the inner block
//! in an `Arc` so each service task (and `axum::extract::State`) can clone
//! cheaply.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cheap, cloneable handle to the shared counter block.
#[derive(Clone, Default)]
pub struct Counters {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    http_requests: AtomicU64,
    http_bytes_in: AtomicU64,
    http_bytes_out: AtomicU64,
    tcp_connections: AtomicU64,
    tcp_bytes_out: AtomicU64,
    udp_packets_received: AtomicU64,
    udp_valid_packets: AtomicU64,
    udp_checksum_errors: AtomicU64,
}

/// Snapshot of counter values at one moment in time. Returned by `snapshot()`
/// for the `/stats` HTTP handler — never mutate this struct.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub http_requests: u64,
    pub http_bytes_in: u64,
    pub http_bytes_out: u64,
    pub tcp_connections: u64,
    pub tcp_bytes_out: u64,
    pub udp_packets_received: u64,
    pub udp_valid_packets: u64,
    pub udp_checksum_errors: u64,
}

impl Counters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the HTTP request counter and return the new value (used as
    /// the `<seq>` field in the response header line).
    pub fn inc_http_requests(&self) -> u64 {
        // fetch_add returns the PREVIOUS value, so add 1 to get the new one.
        self.inner.http_requests.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn add_http_bytes_in(&self, n: u64) {
        self.inner.http_bytes_in.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_http_bytes_out(&self, n: u64) {
        self.inner.http_bytes_out.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_tcp_connections(&self) {
        self.inner.tcp_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_tcp_bytes_out(&self, n: u64) {
        self.inner.tcp_bytes_out.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_udp_packets_received(&self) {
        self.inner
            .udp_packets_received
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_udp_valid_packets(&self) {
        self.inner.udp_valid_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_udp_checksum_errors(&self) {
        self.inner
            .udp_checksum_errors
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Atomic snapshot. Each load is independent — we don't claim global
    /// consistency, just per-counter coherence. That's good enough for the
    /// `/stats` endpoint, which assertions read.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            http_requests: self.inner.http_requests.load(Ordering::Relaxed),
            http_bytes_in: self.inner.http_bytes_in.load(Ordering::Relaxed),
            http_bytes_out: self.inner.http_bytes_out.load(Ordering::Relaxed),
            tcp_connections: self.inner.tcp_connections.load(Ordering::Relaxed),
            tcp_bytes_out: self.inner.tcp_bytes_out.load(Ordering::Relaxed),
            udp_packets_received: self.inner.udp_packets_received.load(Ordering::Relaxed),
            udp_valid_packets: self.inner.udp_valid_packets.load(Ordering::Relaxed),
            udp_checksum_errors: self.inner.udp_checksum_errors.load(Ordering::Relaxed),
        }
    }
}

impl Snapshot {
    /// Render in the simple `key=value\n` text format the smoke tests parse.
    pub fn to_text(&self) -> String {
        format!(
            "http_requests={}\n\
             http_bytes_in={}\n\
             http_bytes_out={}\n\
             tcp_connections={}\n\
             tcp_bytes_out={}\n\
             udp_packets_received={}\n\
             udp_valid_packets={}\n\
             udp_checksum_errors={}\n",
            self.http_requests,
            self.http_bytes_in,
            self.http_bytes_out,
            self.tcp_connections,
            self.tcp_bytes_out,
            self.udp_packets_received,
            self.udp_valid_packets,
            self.udp_checksum_errors,
        )
    }
}
