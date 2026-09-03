//! Bytes-per-second / Mbps helper shared across Phase 3 throughput tests.
//!
//! The real measurement primitive is "bytes accumulated over a wall-clock
//! window". Tests record total bytes + total elapsed and ask for
//! `mbps()` once at the end. We deliberately keep this trivial — there
//! is no rolling window, no exponentially-weighted moving average — so
//! the reported number is the average sustained rate over the test, not
//! a peak. That is what the plan asserts against.

use std::time::{Duration, Instant};

use serde::Serialize;

/// Convert a (bytes, elapsed) pair into Mbps. Saturates rather than
/// dividing by zero when the window has no duration.
pub fn mbps(bytes: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    // 1 Mbps = 1_000_000 bits/s. Use bits/sec / 1e6 so results are
    // human-readable: a 100 MiB/s download reads as ~838 Mbps.
    (bytes as f64) * 8.0 / 1_000_000.0 / secs
}

/// Convert a (bytes, elapsed) pair into MiB/s (binary mebibytes per
/// second). Useful for the file-transfer perspective; Mbps is the
/// network-perspective number.
pub fn mib_per_s(bytes: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    (bytes as f64) / (1024.0 * 1024.0) / secs
}

/// Stable JSON shape for a single throughput measurement. Embedded in
/// every Phase 3 report so downstream tooling can pivot by metric.
#[derive(Debug, Clone, Serialize)]
pub struct Throughput {
    pub bytes: u64,
    pub elapsed_us: u64,
    pub mbps: f64,
    pub mib_per_s: f64,
}

impl Throughput {
    /// Snapshot a (bytes, elapsed) pair into a serializable record.
    pub fn from_window(bytes: u64, elapsed: Duration) -> Self {
        Self {
            bytes,
            elapsed_us: elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
            mbps: mbps(bytes, elapsed),
            mib_per_s: mib_per_s(bytes, elapsed),
        }
    }
}

/// Population std-dev of inter-arrival deltas (microseconds).
/// Returns 0 for fewer than 2 samples — a single arrival has no
/// inter-arrival. Used by `udp_streaming_game` per-stream jitter.
pub fn inter_arrival_jitter_us(arrivals: &[Instant]) -> u64 {
    if arrivals.len() < 2 {
        return 0;
    }
    let mut deltas_us: Vec<f64> = Vec::with_capacity(arrivals.len() - 1);
    for w in arrivals.windows(2) {
        let dt = w[1].saturating_duration_since(w[0]);
        deltas_us.push(dt.as_micros() as f64);
    }
    let n = deltas_us.len() as f64;
    let mean = deltas_us.iter().sum::<f64>() / n;
    let var = deltas_us.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    var.sqrt() as u64
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn mbps_known_values() {
        // 1 MB in 1 s = 8 Mbps.
        let v = mbps(1_000_000, Duration::from_secs(1));
        assert!((v - 8.0).abs() < 1e-9, "got {v}");
        // 100 MiB ≈ 104.86 MB → ~838 Mbps in 1 s.
        let v = mbps(100 * 1024 * 1024, Duration::from_secs(1));
        assert!((v - 838.86).abs() < 0.5, "got {v}");
    }

    #[test]
    fn mib_per_s_known_values() {
        let v = mib_per_s(1024 * 1024, Duration::from_secs(1));
        assert!((v - 1.0).abs() < 1e-9);
        let v = mib_per_s(2 * 1024 * 1024, Duration::from_secs(2));
        assert!((v - 1.0).abs() < 1e-9);
    }

    #[test]
    fn zero_duration_does_not_divide_by_zero() {
        assert_eq!(mbps(1024, Duration::from_secs(0)), 0.0);
        assert_eq!(mib_per_s(1024, Duration::from_secs(0)), 0.0);
    }

    #[test]
    fn jitter_is_zero_for_short_input() {
        assert_eq!(inter_arrival_jitter_us(&[]), 0);
        let now = Instant::now();
        assert_eq!(inter_arrival_jitter_us(&[now]), 0);
    }

    #[test]
    fn jitter_is_zero_for_uniform_intervals() {
        let base = Instant::now();
        let arrivals: Vec<Instant> = (0..10)
            .map(|i| base + Duration::from_millis(i * 10))
            .collect();
        // Perfectly uniform 10 ms spacing → zero std-dev.
        assert_eq!(inter_arrival_jitter_us(&arrivals), 0);
    }

    #[test]
    fn throughput_record_is_self_consistent() {
        let t = Throughput::from_window(1_000_000, Duration::from_secs(1));
        assert_eq!(t.bytes, 1_000_000);
        assert!((t.mbps - 8.0).abs() < 1e-9);
        // Serialize-deserialize round-trip just to pin field names.
        let json = serde_json::to_string(&t).unwrap();
        for f in ["bytes", "elapsed_us", "mbps", "mib_per_s"] {
            assert!(json.contains(f), "missing {f}: {json}");
        }
    }
}
