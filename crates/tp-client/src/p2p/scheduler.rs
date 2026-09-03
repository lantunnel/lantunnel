//! Path scheduler: picks Relay vs P2p per pick_kind() call. Applies a
//! stable-cycle gate to prevent flapping under marginal P2P health.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tp_core::config::ClientP2pConfig;
use tp_transport::session::SessionStats;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Relay,
    P2p,
}

/// Encoding of the last pick for the `p2p_path_switches_total`
/// transition detector. `0` is the initial "no prior pick" sentinel —
/// the first call to `record_transition` sets it but reports no switch.
const LAST_PICK_UNSET: u8 = 0;
const LAST_PICK_RELAY: u8 = 1;
const LAST_PICK_P2P: u8 = 2;

fn encode(kind: PathKind) -> u8 {
    match kind {
        PathKind::Relay => LAST_PICK_RELAY,
        PathKind::P2p => LAST_PICK_P2P,
    }
}

fn decode(code: u8) -> Option<PathKind> {
    match code {
        LAST_PICK_RELAY => Some(PathKind::Relay),
        LAST_PICK_P2P => Some(PathKind::P2p),
        _ => None,
    }
}

pub struct PathScheduler {
    p2p_min_advantage: f64, // p2p_rtt <= advantage * relay_rtt to be preferred
    p2p_stable_cycles: u32, // require this many healthy+advantageous cycles in a row
    healthy_rtt_max: Duration,
    healthy_loss_max: f64,
    healthy_pto_max: u32,
    state: Mutex<SchedState>,
    /// The kind returned by the most recent `pick_kind` call,
    /// encoded via `LAST_PICK_*`. Sits alongside `state` because
    /// transition detection is read-modify-write but cheaper as a
    /// single atomic swap than as a lock-protected field.
    last_pick: AtomicU8,
}

#[derive(Default)]
struct SchedState {
    p2p_healthy_cycles: u32,
}

impl Default for PathScheduler {
    fn default() -> Self {
        // Mirrors `ClientP2pConfig::default()` for the two YAML-driven
        // knobs. Health thresholds (`healthy_rtt_max` / `healthy_loss_max`
        // / `healthy_pto_max`) are not exposed in the YAML schema, so they
        // stay defaulted here regardless of caller.
        Self {
            p2p_min_advantage: 1.2,
            p2p_stable_cycles: 3,
            healthy_rtt_max: Duration::from_millis(200),
            healthy_loss_max: 0.05,
            healthy_pto_max: 3,
            state: Mutex::new(SchedState::default()),
            last_pick: AtomicU8::new(LAST_PICK_UNSET),
        }
    }
}

impl PathScheduler {
    /// Build a scheduler from the user-facing P2P config so the YAML
    /// `scheduler_p2p_min_advantage` and `scheduler_stable_cycles` knobs
    /// flow through to runtime. Health thresholds stay defaulted —
    /// they are not exposed in the YAML schema today.
    pub fn from_config(cfg: &ClientP2pConfig) -> Self {
        Self {
            p2p_min_advantage: cfg.scheduler_p2p_min_advantage,
            p2p_stable_cycles: cfg.scheduler_stable_cycles,
            ..Self::default()
        }
    }

    pub fn pick_kind(&self, relay: &SessionStats, p2p: Option<&SessionStats>) -> PathKind {
        let Some(p2p) = p2p else {
            self.reset();
            return PathKind::Relay;
        };
        if !self.is_healthy(p2p) {
            self.reset();
            return PathKind::Relay;
        }
        let advantage = if relay.rtt.is_zero() {
            true
        } else {
            p2p.rtt.as_secs_f64() <= self.p2p_min_advantage * relay.rtt.as_secs_f64()
        };
        if !advantage {
            self.reset();
            return PathKind::Relay;
        }
        let mut st = self.state.lock().unwrap();
        st.p2p_healthy_cycles = st.p2p_healthy_cycles.saturating_add(1);
        if st.p2p_healthy_cycles >= self.p2p_stable_cycles {
            PathKind::P2p
        } else {
            PathKind::Relay
        }
    }

    pub fn is_healthy(&self, s: &SessionStats) -> bool {
        s.rtt < self.healthy_rtt_max
            && s.loss_rate < self.healthy_loss_max
            && s.pto_count < self.healthy_pto_max
    }

    fn reset(&self) {
        self.state.lock().unwrap().p2p_healthy_cycles = 0;
    }

    /// Update `last_pick` to `new` and report the previous kind iff
    /// it differed (i.e. this call represents a Relay↔P2p transition).
    /// `None` covers two cases: this is the first pick recorded, or the
    /// previous pick was the same kind. Same-kind ticks are not switches
    /// and are intentionally swallowed here so callers can blindly emit
    /// the metric on `Some(_)`.
    pub fn record_transition(&self, new: PathKind) -> Option<PathKind> {
        let new_code = encode(new);
        let prev_code = self.last_pick.swap(new_code, Ordering::Relaxed);
        if prev_code == new_code {
            return None;
        }
        decode(prev_code) // None when prev_code == LAST_PICK_UNSET
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tp_transport::session::SessionStats;

    fn stats(rtt_ms: u64, loss: f64, pto: u32) -> SessionStats {
        SessionStats {
            rtt: Duration::from_millis(rtt_ms),
            loss_rate: loss,
            pto_count: pto,
        }
    }

    #[test]
    fn picks_relay_when_p2p_unavailable() {
        let s = PathScheduler::default();
        let pick = s.pick_kind(&stats(50, 0.0, 0), None);
        assert_eq!(pick, PathKind::Relay);
    }

    #[test]
    fn picks_relay_until_stable_cycles() {
        let s = PathScheduler::default();
        let r = stats(100, 0.0, 0);
        let p = stats(40, 0.0, 0);
        let pick1 = s.pick_kind(&r, Some(&p));
        assert_eq!(pick1, PathKind::Relay);
        let pick2 = s.pick_kind(&r, Some(&p));
        assert_eq!(pick2, PathKind::Relay);
        let pick3 = s.pick_kind(&r, Some(&p));
        assert_eq!(pick3, PathKind::P2p);
    }

    #[test]
    fn default_scheduler_prefers_healthy_p2p_even_when_rtt_is_slightly_worse_than_relay() {
        let s = PathScheduler::default();
        let r = stats(100, 0.0, 0);
        let p = stats(110, 0.0, 0);
        assert_eq!(s.pick_kind(&r, Some(&p)), PathKind::Relay);
        assert_eq!(s.pick_kind(&r, Some(&p)), PathKind::Relay);
        assert_eq!(s.pick_kind(&r, Some(&p)), PathKind::P2p);
    }

    #[test]
    fn picks_relay_when_p2p_unhealthy() {
        let s = PathScheduler::default();
        let r = stats(100, 0.0, 0);
        let unhealthy = stats(50, 0.10, 0); // loss > 5%
        for _ in 0..5 {
            assert_eq!(s.pick_kind(&r, Some(&unhealthy)), PathKind::Relay);
        }
    }

    #[test]
    fn resets_cycles_on_unhealthy() {
        let s = PathScheduler::default();
        let r = stats(100, 0.0, 0);
        let healthy = stats(40, 0.0, 0);
        let unhealthy = stats(50, 0.10, 0);
        s.pick_kind(&r, Some(&healthy));
        s.pick_kind(&r, Some(&unhealthy));
        let pick_after_reset = s.pick_kind(&r, Some(&healthy));
        assert_eq!(pick_after_reset, PathKind::Relay);
    }

    #[test]
    fn from_config_threads_min_advantage_and_stable_cycles() {
        // Both YAML knobs must reach runtime. Override stable_cycles
        // to 5 (default 3) and verify pick_kind returns Relay until the
        // 5th healthy+advantageous cycle, then P2p on the 5th. The
        // p2p_min_advantage knob is exercised implicitly: p2p RTT must be
        // <= 0.5 * relay RTT to be advantageous (default 1.2 would also
        // accept this pair, so the assertion is on the cycle count).
        let cfg = ClientP2pConfig {
            scheduler_p2p_min_advantage: 0.5,
            scheduler_stable_cycles: 5,
            ..ClientP2pConfig::default()
        };
        let s = PathScheduler::from_config(&cfg);
        let r = stats(100, 0.0, 0);
        let p = stats(40, 0.0, 0); // 40 < 0.5 * 100 = 50 → advantageous
        for cycle in 1..=4 {
            assert_eq!(
                s.pick_kind(&r, Some(&p)),
                PathKind::Relay,
                "cycle {cycle} must stay Relay (need 5 stable cycles)"
            );
        }
        assert_eq!(
            s.pick_kind(&r, Some(&p)),
            PathKind::P2p,
            "cycle 5 must flip to P2p"
        );
    }

    /// A Relay → P2p flip (after `stable_cycles` warm-up) followed
    /// by a P2p → Relay flip (after one unhealthy `pick_kind`) must
    /// emit exactly one increment per direction in
    /// `p2p_path_switches_total`. Same-kind ticks (the warm-up cycles
    /// returning Relay) do NOT count as switches.
    #[test]
    fn path_scheduler_emits_switch_metric_on_transition() {
        use tp_metrics::{MetricsManager, P2pPathKind};

        let s = PathScheduler::default();
        let metrics = MetricsManager::new();

        let r = stats(100, 0.0, 0);
        let healthy = stats(40, 0.0, 0);
        let unhealthy = stats(50, 0.10, 0); // loss > 5% → unhealthy

        // Helper that mirrors `MultiSession::pick`'s metric-emit block
        // without dragging the whole MultiSession setup into a unit
        // test. Same call order: pick_kind, then record_transition,
        // then incr_p2p_path_switch on Some(prev).
        let tick = |stats: &SessionStats| {
            let kind = s.pick_kind(&r, Some(stats));
            if let Some(prev) = s.record_transition(kind) {
                let from = match prev {
                    PathKind::Relay => P2pPathKind::Relay,
                    PathKind::P2p => P2pPathKind::P2p,
                };
                let to = match kind {
                    PathKind::Relay => P2pPathKind::Relay,
                    PathKind::P2p => P2pPathKind::P2p,
                };
                metrics.incr_p2p_path_switch(from, to);
            }
            kind
        };

        // Cycle 1: Relay (warm-up). Records Relay as last_pick (was
        // Unset → no transition emitted).
        assert_eq!(tick(&healthy), PathKind::Relay);
        // Cycle 2: Relay (still warming up).
        assert_eq!(tick(&healthy), PathKind::Relay);
        // Cycle 3: P2p (warm-up complete). Relay → P2p switch.
        assert_eq!(tick(&healthy), PathKind::P2p);
        // Cycle 4: unhealthy → Relay. P2p → Relay switch.
        assert_eq!(tick(&unhealthy), PathKind::Relay);
        // Cycle 5: still Relay. Same-kind, no switch.
        assert_eq!(tick(&unhealthy), PathKind::Relay);

        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_path_switches_total{from=\"relay\",to=\"p2p\"} 1"),
            "expected exactly one Relay→P2p switch:\n{text}"
        );
        assert!(
            text.contains("p2p_path_switches_total{from=\"p2p\",to=\"relay\"} 1"),
            "expected exactly one P2p→Relay switch:\n{text}"
        );
    }

    #[test]
    fn from_config_min_advantage_gates_p2p() {
        // Scheduler_p2p_min_advantage must reach runtime. With a
        // tight 0.3 advantage requirement, a 40 ms p2p vs 100 ms relay
        // (ratio 0.4) is NOT advantageous, so pick_kind must stay Relay
        // forever regardless of stable_cycles.
        let cfg = ClientP2pConfig {
            scheduler_p2p_min_advantage: 0.3,
            scheduler_stable_cycles: 1,
            ..ClientP2pConfig::default()
        };
        let s = PathScheduler::from_config(&cfg);
        let r = stats(100, 0.0, 0);
        let p = stats(40, 0.0, 0); // 40 >= 0.3 * 100 = 30 → NOT advantageous
        for _ in 0..3 {
            assert_eq!(s.pick_kind(&r, Some(&p)), PathKind::Relay);
        }
    }
}
