//! Latency histogram helper.
//!
//! Backed by `hdrhistogram` so per-sample insertion is O(1) and percentile
//! queries are O(log N) with sub-microsecond precision. Every Phase 2 test
//! that records a round-trip time funnels through `Stats::record` and
//! emits its final JSON via `Stats::report`.
//!
//! Why a wrapper instead of using `Histogram` directly:
//!   * Centralizes the choice of resolution (3 sigfigs is the
//!     `hdrhistogram` recommended default — costs ~16 KB for the worst
//!     case 60s upper bound, a non-issue at our sample volumes).
//!   * Keeps the JSON schema for `Report` stable across all 4 tests,
//!     so downstream tooling (CI dashboards, plan-spec regression
//!     graphs) sees the same shape regardless of which test emitted it.

use std::time::Duration;

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde::Serialize;

/// Streaming percentile recorder. One per (path, traffic-shape) pair
/// in the multi-shape baseline / gamestream-sim tests.
pub struct Stats {
    hist: Histogram<u64>,
}

impl Stats {
    /// `max_us` = upper bound for percentile precision. 3 sigfigs.
    ///
    /// Sized values used in this crate:
    ///   * 60_000_000 (60 s) — covers cold-start probe + full-pipe
    ///     stress steps where a single round-trip can stretch into
    ///     the seconds.
    ///   * 1_000_000 (1 s) — comfortably above any loopback p99 and
    ///     keeps the histogram footprint smaller for hot inner loops.
    pub fn new(max_us: u64) -> Result<Self> {
        let hist = Histogram::<u64>::new_with_max(max_us, 3)
            .context("init hdrhistogram (check max_us > 0)")?;
        Ok(Self { hist })
    }

    /// Record a single round-trip duration. Saturates rather than
    /// erroring on out-of-range so a single slow outlier doesn't
    /// abort a 5000-sample run.
    pub fn record(&mut self, dt: Duration) -> Result<()> {
        let us = dt.as_micros().min(u128::from(u64::MAX)) as u64;
        // `record` only fails when the value is outside the configured
        // tracked range. We saturate to the histogram's high bound so
        // the run keeps making progress; the high bound is recoverable
        // in `Report::max_us`.
        if let Err(_e) = self.hist.record(us) {
            let high = self.hist.high();
            self.hist
                .record(high)
                .context("record saturated value at hist.high()")?;
        }
        Ok(())
    }

    /// Snapshot the current percentiles. Cheap (microseconds) so callers
    /// may safely call this only at end-of-run.
    pub fn report(&self) -> Report {
        Report {
            count: self.hist.len(),
            min_us: self.hist.min(),
            max_us: self.hist.max(),
            mean_us: self.hist.mean() as u64,
            p50_us: self.hist.value_at_quantile(0.50),
            p90_us: self.hist.value_at_quantile(0.90),
            p95_us: self.hist.value_at_quantile(0.95),
            p99_us: self.hist.value_at_quantile(0.99),
        }
    }
}

/// JSON-stable percentile report. Field names are camel-snake to match
/// the rest of the repo's metric exports.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub count: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub mean_us: u64,
    pub p50_us: u64,
    pub p90_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_match_uniform_distribution() {
        // Sampling 1µs..=1000µs uniformly should pin p50≈500, p99≈990.
        let mut s = Stats::new(60_000_000).unwrap();
        for v in 1u64..=1000 {
            s.record(Duration::from_micros(v)).unwrap();
        }
        let r = s.report();
        assert_eq!(r.count, 1000);
        assert!(r.min_us <= 1, "min={}", r.min_us);
        assert!(r.max_us >= 999, "max={}", r.max_us);
        assert!(
            (495..=505).contains(&r.p50_us),
            "p50={} (expected ~500)",
            r.p50_us,
        );
        assert!(
            (985..=1000).contains(&r.p99_us),
            "p99={} (expected ~990)",
            r.p99_us,
        );
    }

    #[test]
    fn out_of_range_saturates_instead_of_panicking() {
        let mut s = Stats::new(1_000).unwrap();
        // 5 ms is well over the 1ms upper bound. Should saturate rather
        // than error so a single outlier doesn't abort the run.
        s.record(Duration::from_millis(5)).unwrap();
        let r = s.report();
        assert_eq!(r.count, 1);
        assert!(
            r.max_us <= 1_000,
            "saturated to high bound, got {}",
            r.max_us
        );
    }

    #[test]
    fn report_serializes_to_stable_json() {
        let mut s = Stats::new(60_000_000).unwrap();
        s.record(Duration::from_micros(100)).unwrap();
        let json = serde_json::to_string(&s.report()).unwrap();
        // Field names are part of the public schema — sanity-check
        // that none of them got renamed by an absent-minded refactor.
        for f in [
            "count", "min_us", "max_us", "mean_us", "p50_us", "p90_us", "p95_us", "p99_us",
        ] {
            assert!(json.contains(f), "{} missing from JSON: {}", f, json);
        }
    }

    #[test]
    fn rejects_zero_max_us() {
        assert!(Stats::new(0).is_err());
    }
}
