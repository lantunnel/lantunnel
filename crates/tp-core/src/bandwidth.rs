//! Per-Tunnel bandwidth limiting.
//!
//! The Gateway applies a limiter per authenticated attachment.

use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{
    clock::{Clock, DefaultClock},
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use nonzero_ext::nonzero;
use parking_lot::RwLock;

const BANDWIDTH_LIMITER_BURST_SECONDS: u32 = 3;

/// Token-bucket rate limiter. `mbps == 0` means "no limit".
pub struct BandwidthLimiter {
    state: RwLock<BandwidthLimiterState>,
}

struct BandwidthLimiterState {
    limiter: Option<Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>>,
    bytes_per_sec: u32,
}

impl BandwidthLimiter {
    pub fn new(mbps: u32) -> Self {
        Self {
            state: RwLock::new(Self::state_for_mbps(mbps)),
        }
    }

    fn state_for_mbps(mbps: u32) -> BandwidthLimiterState {
        if mbps == 0 {
            return BandwidthLimiterState {
                limiter: None,
                bytes_per_sec: 0,
            };
        }
        // Mbps → bytes/sec  (mbps × 131_072 = mbps × 1024 × 1024 / 8)
        let bps: u32 = mbps.saturating_mul(131_072);
        let burst = bps.saturating_mul(BANDWIDTH_LIMITER_BURST_SECONDS);
        let quota = Quota::per_second(NonZeroU32::new(bps).unwrap_or(nonzero!(1u32)))
            .allow_burst(NonZeroU32::new(burst).unwrap_or(nonzero!(1u32)));
        BandwidthLimiterState {
            limiter: Some(Arc::new(RateLimiter::direct(quota))),
            bytes_per_sec: bps,
        }
    }

    pub fn set_mbps(&self, mbps: u32) -> bool {
        let next = Self::state_for_mbps(mbps);
        let mut state = self.state.write();
        if state.bytes_per_sec == next.bytes_per_sec {
            return false;
        }
        *state = next;
        true
    }

    pub fn bytes_per_sec(&self) -> u32 {
        self.state.read().bytes_per_sec
    }

    fn limiter(&self) -> Option<Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>> {
        self.state.read().limiter.clone()
    }

    /// Acquire `n` bytes of budget. No-op when unlimited.
    pub async fn acquire(&self, n: usize) {
        let Some(lim) = self.limiter() else { return };
        let mut remaining = n as u32;
        const MAX_CHUNK: u32 = 65_536;
        while remaining > 0 {
            let want = remaining.min(MAX_CHUNK);
            if let Some(nz) = NonZeroU32::new(want) {
                lim.until_n_ready(nz).await.ok();
            }
            remaining -= want;
        }
    }

    /// Non-blocking acquire of up to one token-bucket chunk (64 KiB). Returns
    /// `Ok(())` and consumes the tokens when enough are available, or
    /// `Err(wait)` where `wait` is the minimum duration the caller must sleep
    /// before retrying. No-op (returns `Ok(())`) when unlimited.
    ///
    /// Designed for two integration points:
    ///   * `AsyncWrite::poll_write` adapters that cannot `.await`: they can
    ///     stash a `tokio::time::sleep(wait)` and poll it on the next call.
    ///   * `try_send_to`-style UDP fast paths on game-stream traffic where
    ///     "drop now" beats "queue + deliver late"; the caller treats `Err`
    ///     as a rate-limit-induced drop.
    ///
    /// Capped at 64 KiB per call — matches `acquire`'s MAX_CHUNK so a single
    /// call can never trip `InsufficientCapacity` for realistic per-group
    /// Mbps values (25 Mbps ≈ 3.2 MB/s burst >> 64 KiB).
    pub fn try_acquire(&self, n: usize) -> Result<(), std::time::Duration> {
        let Some(lim) = self.limiter() else {
            return Ok(());
        };
        const MAX_CHUNK: u32 = 65_536;
        let want = (n as u32).min(MAX_CHUNK);
        let Some(nz) = NonZeroU32::new(want) else {
            return Ok(());
        };
        match lim.check_n(nz) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(not_until)) => Err(not_until.wait_time_from(DefaultClock::default().now())),
            // `n` exceeds the burst bucket (only possible when `mbps` is
            // small enough that 64 KiB > quota/sec). Ask the caller to back
            // off a millisecond and retry; real traffic never hits this.
            Err(_) => Err(std::time::Duration::from_millis(1)),
        }
    }
}

pub type SharedLimiter = Arc<RwLock<BandwidthLimiter>>;

pub fn new_shared_limiter(mbps: u32) -> SharedLimiter {
    Arc::new(RwLock::new(BandwidthLimiter::new(mbps)))
}

#[cfg(test)]
mod bandwidth_limiter_tests {
    use super::*;

    /// `mbps = 0` is the documented "unlimited" sentinel — every path must
    /// short-circuit to `Ok(())` regardless of `n`.
    #[test]
    fn try_acquire_unlimited_always_ok() {
        let lim = BandwidthLimiter::new(0);
        assert!(lim.try_acquire(0).is_ok());
        assert!(lim.try_acquire(1).is_ok());
        assert!(lim.try_acquire(64 * 1024).is_ok());
        assert!(lim.try_acquire(10 * 1024 * 1024).is_ok());
    }

    /// Small draws under the burst cap must succeed immediately. 25 Mbps is a
    /// representative deployment limit, so the test stays aligned with real
    /// sizing.
    #[test]
    fn try_acquire_small_under_burst_is_ok() {
        let lim = BandwidthLimiter::new(25);
        // 4 KiB is well below the 64 KiB chunk cap AND below 25 Mbps / sec,
        // so the bucket has plenty of tokens on a fresh limiter.
        assert!(lim.try_acquire(4096).is_ok());
    }

    #[test]
    fn try_acquire_allows_three_seconds_of_burst() {
        let lim = BandwidthLimiter::new(1);

        for chunk in 0..6 {
            lim.try_acquire(65_536)
                .unwrap_or_else(|_| panic!("chunk {chunk} should be within the 3s burst budget"));
        }

        assert!(
            lim.try_acquire(65_536).is_err(),
            "the chunk after the 3s burst budget must be rate limited"
        );
    }

    /// Draining the bucket must flip subsequent calls to `Err(wait)` with a
    /// positive wait — that's the Pending signal `poll_write` needs to park
    /// a Sleep. We drain in 64 KiB chunks (matching MAX_CHUNK) until the
    /// limiter says to back off, and assert the returned wait is non-zero.
    #[test]
    fn try_acquire_over_budget_returns_positive_wait() {
        let lim = BandwidthLimiter::new(1); // 1 Mbps = 131_072 B/s.
                                            // A 1 Mbps bucket holds 3 seconds of tokens — drain it.
        for _ in 0..6 {
            let _ = lim.try_acquire(65_536);
        }
        // The next 64 KiB chunk must overflow and return a positive wait.
        let got = lim.try_acquire(65_536);
        match got {
            Err(wait) => assert!(
                !wait.is_zero(),
                "over-budget try_acquire must return a non-zero wait"
            ),
            Ok(()) => panic!("expected Err after bucket drain, got Ok"),
        }
    }

    #[test]
    fn set_mbps_updates_existing_limiter_in_place() {
        let lim = BandwidthLimiter::new(0);
        assert_eq!(lim.bytes_per_sec(), 0);
        assert!(lim.try_acquire(65_536).is_ok());

        assert!(lim.set_mbps(1));
        assert_eq!(lim.bytes_per_sec(), 131_072);
        for chunk in 0..6 {
            lim.try_acquire(65_536)
                .unwrap_or_else(|_| panic!("drain 3s burst chunk {chunk}"));
        }
        assert!(
            lim.try_acquire(65_536).is_err(),
            "updated limiter must enforce the new finite cap"
        );

        assert!(!lim.set_mbps(1), "same Mbps update should not reset bucket");
        assert!(
            lim.try_acquire(65_536).is_err(),
            "same-cap refresh must not refill the bucket"
        );
    }
}
