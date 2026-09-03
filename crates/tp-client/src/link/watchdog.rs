use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkKind {
    Relay,
    P2p,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LinkWatchdogConfig {
    pub(crate) heartbeat_interval: Duration,
    pub(crate) ack_stale_after: Duration,
    pub(crate) check_interval: Duration,
    pub(crate) active_no_link_progress_grace: Duration,
    pub(crate) stale_log_interval: Duration,
}

impl LinkWatchdogConfig {
    pub(crate) fn production() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
            ack_stale_after: Duration::from_secs(3),
            check_interval: Duration::from_secs(1),
            active_no_link_progress_grace: Duration::from_secs(30),
            stale_log_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkWatchdogCloseReason {
    RelayActive,
    P2pActive,
    P2pIdle,
}

impl LinkWatchdogCloseReason {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::RelayActive => "relay_active_stale",
            Self::P2pActive => "p2p_active_stale",
            Self::P2pIdle => "p2p_idle_stale",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkWatchdogDecision {
    Keep,
    KeepIdleStale,
    KeepActiveTcpPinned,
    Close(LinkWatchdogCloseReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LinkWatchdogSnapshot {
    pub(crate) now_ms: u64,
    pub(crate) last_ack_ms: u64,
    pub(crate) last_link_progress_ms: u64,
    pub(crate) active_tcp_flows: usize,
    pub(crate) active_udp_flows: usize,
}

pub(crate) fn evaluate_link_watchdog(
    kind: LinkKind,
    config: LinkWatchdogConfig,
    snapshot: LinkWatchdogSnapshot,
) -> LinkWatchdogDecision {
    let ack_age_ms = snapshot.now_ms.saturating_sub(snapshot.last_ack_ms);
    let no_link_progress_ms = snapshot
        .now_ms
        .saturating_sub(snapshot.last_link_progress_ms);
    let ack_stale_ms = duration_ms(config.ack_stale_after);

    if ack_age_ms < ack_stale_ms || no_link_progress_ms < ack_stale_ms {
        return LinkWatchdogDecision::Keep;
    }

    if snapshot.active_tcp_flows > 0 {
        return LinkWatchdogDecision::KeepActiveTcpPinned;
    }

    if snapshot.active_udp_flows == 0 {
        return match kind {
            LinkKind::Relay => LinkWatchdogDecision::KeepIdleStale,
            LinkKind::P2p => LinkWatchdogDecision::Close(LinkWatchdogCloseReason::P2pIdle),
        };
    }

    match kind {
        LinkKind::P2p => LinkWatchdogDecision::Close(LinkWatchdogCloseReason::P2pActive),
        LinkKind::Relay
            if no_link_progress_ms < duration_ms(config.active_no_link_progress_grace) =>
        {
            LinkWatchdogDecision::Keep
        }
        LinkKind::Relay => LinkWatchdogDecision::Close(LinkWatchdogCloseReason::RelayActive),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_match_spec() {
        let config = LinkWatchdogConfig::production();

        assert_eq!(config.heartbeat_interval, Duration::from_secs(1));
        assert_eq!(config.ack_stale_after, Duration::from_secs(3));
        assert_eq!(config.check_interval, Duration::from_secs(1));
        assert_eq!(
            config.active_no_link_progress_grace,
            Duration::from_secs(30)
        );
        assert_eq!(config.stale_log_interval, Duration::from_secs(30));
    }

    #[test]
    fn close_reason_strings_match_spec() {
        assert_eq!(
            LinkWatchdogCloseReason::RelayActive.as_str(),
            "relay_active_stale"
        );
        assert_eq!(
            LinkWatchdogCloseReason::P2pActive.as_str(),
            "p2p_active_stale"
        );
        assert_eq!(LinkWatchdogCloseReason::P2pIdle.as_str(), "p2p_idle_stale");
    }

    #[test]
    fn fresh_ack_keeps_link_open() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 10_000,
            last_ack_ms: 8_000,
            last_link_progress_ms: 0,
            active_tcp_flows: 0,
            active_udp_flows: 0,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::Relay, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::Keep
        );
    }

    #[test]
    fn fresh_same_link_progress_keeps_stale_ack_link_open() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 10_000,
            last_ack_ms: 1_000,
            last_link_progress_ms: 8_500,
            active_tcp_flows: 0,
            active_udp_flows: 0,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::P2p, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::Keep
        );
    }

    #[test]
    fn idle_relay_remains_open_beyond_all_watchdog_thresholds() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 40_001,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 0,
            active_udp_flows: 0,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::Relay, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::KeepIdleStale
        );
    }

    #[test]
    fn idle_p2p_closes_promptly_after_ack_and_progress_become_stale() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 3_000,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 0,
            active_udp_flows: 0,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::P2p, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::Close(LinkWatchdogCloseReason::P2pIdle)
        );
    }

    #[test]
    fn relay_active_tcp_remains_pinned_beyond_udp_close_grace() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 40_001,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 1,
            active_udp_flows: 0,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::Relay, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::KeepActiveTcpPinned
        );
    }

    #[test]
    fn p2p_active_tcp_is_pinned_instead_of_closed() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 3_000,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 1,
            active_udp_flows: 0,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::P2p, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::KeepActiveTcpPinned
        );
    }

    #[test]
    fn relay_active_udp_stays_open_until_full_production_grace() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 29_999,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 0,
            active_udp_flows: 1,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::Relay, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::Keep
        );
    }

    #[test]
    fn relay_active_udp_closes_at_full_production_grace() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 30_000,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 0,
            active_udp_flows: 1,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::Relay, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::Close(LinkWatchdogCloseReason::RelayActive)
        );
    }

    #[test]
    fn p2p_active_udp_closes_without_waiting_for_relay_grace() {
        let snapshot = LinkWatchdogSnapshot {
            now_ms: 3_000,
            last_ack_ms: 0,
            last_link_progress_ms: 0,
            active_tcp_flows: 0,
            active_udp_flows: 1,
        };

        assert_eq!(
            evaluate_link_watchdog(LinkKind::P2p, LinkWatchdogConfig::production(), snapshot),
            LinkWatchdogDecision::Close(LinkWatchdogCloseReason::P2pActive)
        );
    }
}
