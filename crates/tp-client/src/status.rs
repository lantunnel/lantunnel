//! Connection status snapshot + listener trait.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPathMode {
    #[default]
    Disconnected,
    Connecting,
    Relay,
    P2p,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatStatus {
    pub active: bool,
    pub last_time: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficPath {
    Relay,
    P2p,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficStats {
    pub relay_tx_bytes: u64,
    pub relay_rx_bytes: u64,
    pub p2p_tx_bytes: u64,
    pub p2p_rx_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayQuotaStatus {
    pub period_yyyymm: String,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub remaining_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub traffic: TrafficStats,
    pub progress_epoch: u64,
}

#[derive(Debug, Default)]
pub struct TrafficCounters {
    relay_tx_bytes: AtomicU64,
    relay_rx_bytes: AtomicU64,
    p2p_tx_bytes: AtomicU64,
    p2p_rx_bytes: AtomicU64,
    progress_epoch: AtomicU64,
}

impl TrafficCounters {
    pub fn record_tx(&self, path: TrafficPath, bytes: u64) {
        match path {
            TrafficPath::Relay => saturating_atomic_add(&self.relay_tx_bytes, bytes),
            TrafficPath::P2p => saturating_atomic_add(&self.p2p_tx_bytes, bytes),
        }
    }

    pub fn record_rx(&self, path: TrafficPath, bytes: u64) {
        match path {
            TrafficPath::Relay => saturating_atomic_add(&self.relay_rx_bytes, bytes),
            TrafficPath::P2p => saturating_atomic_add(&self.p2p_rx_bytes, bytes),
        }
    }

    pub fn reset(&self) {
        self.relay_tx_bytes.store(0, Ordering::Relaxed);
        self.relay_rx_bytes.store(0, Ordering::Relaxed);
        self.p2p_tx_bytes.store(0, Ordering::Relaxed);
        self.p2p_rx_bytes.store(0, Ordering::Relaxed);
        self.progress_epoch.store(0, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> TrafficStats {
        TrafficStats {
            relay_tx_bytes: self.relay_tx_bytes.load(Ordering::Relaxed),
            relay_rx_bytes: self.relay_rx_bytes.load(Ordering::Relaxed),
            p2p_tx_bytes: self.p2p_tx_bytes.load(Ordering::Relaxed),
            p2p_rx_bytes: self.p2p_rx_bytes.load(Ordering::Relaxed),
        }
    }

    pub fn progress_snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            traffic: self.snapshot(),
            progress_epoch: self.progress_epoch.load(Ordering::Relaxed),
        }
    }

    pub fn mark_progress(&self) {
        let _ = self
            .progress_epoch
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(1))
            });
    }
}

fn saturating_atomic_add(slot: &AtomicU64, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let _ = slot.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(bytes))
    });
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionStatus {
    pub connected: bool,
    pub connecting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_name: Option<String>,
    pub gateway_addr: Option<String>,
    pub message: String,
    pub error: Option<String>,
    pub platform_heartbeat: HeartbeatStatus,
    pub transport_heartbeat: HeartbeatStatus,
    pub uptime_secs: u64,
    pub path_mode: ConnectionPathMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2p_state: Option<String>,
    #[serde(default)]
    pub p2p_active_sessions: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2p_primary_peer_id: Option<String>,
    #[serde(default)]
    pub p2p_peer_count: usize,
    #[serde(default)]
    pub traffic: TrafficStats,
}

pub fn derive_path_mode(
    connected: bool,
    connecting: bool,
    p2p_installed: bool,
) -> ConnectionPathMode {
    if connected {
        if p2p_installed {
            ConnectionPathMode::P2p
        } else {
            ConnectionPathMode::Relay
        }
    } else if connecting {
        ConnectionPathMode::Connecting
    } else {
        ConnectionPathMode::Disconnected
    }
}

pub trait StatusListener: Send + Sync + 'static {
    fn on_status(&self, status: &ConnectionStatus) {
        let _ = status;
    }
    fn on_log(&self, line: &str) {
        let _ = line;
    }
}

#[derive(Default, Clone, Copy)]
pub struct NullListener;
impl StatusListener for NullListener {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_mode_prefers_connection_lifecycle_before_p2p_state() {
        assert_eq!(
            derive_path_mode(false, false, true),
            ConnectionPathMode::Disconnected
        );
        assert_eq!(
            derive_path_mode(false, true, true),
            ConnectionPathMode::Connecting
        );
    }

    #[test]
    fn path_mode_reports_relay_until_p2p_is_installed() {
        assert_eq!(
            derive_path_mode(true, false, false),
            ConnectionPathMode::Relay
        );
        assert_eq!(derive_path_mode(true, false, true), ConnectionPathMode::P2p);
    }

    #[test]
    fn traffic_counters_accumulate_and_reset_by_path() {
        let counters = TrafficCounters::default();

        counters.record_tx(TrafficPath::Relay, 5);
        counters.record_rx(TrafficPath::Relay, 7);
        counters.record_tx(TrafficPath::P2p, 11);
        counters.record_rx(TrafficPath::P2p, 13);

        assert_eq!(
            counters.snapshot(),
            TrafficStats {
                relay_tx_bytes: 5,
                relay_rx_bytes: 7,
                p2p_tx_bytes: 11,
                p2p_rx_bytes: 13,
            }
        );

        counters.reset();
        assert_eq!(counters.snapshot(), TrafficStats::default());
    }
}
