//! Rolling stats for one TUIC association's outbound (gateway→phone) path.
//!
//! Split out of `lib.rs`. Keeps the
//! `tracing::info!` line shape stable so operator greps / dashboards keep
//! working.

/// Rolling counters for one TUIC association's inbound (gateway→phone)
/// path. Emitted every [`TuicOutboundStats::LOG_EVERY`] inbound payloads so
/// the operator gets a real-time view of what's actually happening on the
/// wire without having to enable TRACE (which would flood at gameplay
/// rates).
///
/// All fields are absolute counters over the lifetime of the association.
/// `min_max_dg` / `max_max_dg` track the observed range of quinn's current
/// `max_datagram_size`, which varies with PMTUD.
#[derive(Debug)]
pub(crate) struct TuicOutboundStats {
    pub(crate) in_count: u64,
    pub(crate) frag_count: u64,
    pub(crate) max_frags_seen: usize,
    pub(crate) min_max_dg: usize,
    pub(crate) max_max_dg: usize,
    /// Smallest value of `conn.datagram_send_buffer_space()` observed in the
    /// window. Getting close to zero means the native TUIC response path is
    /// applying backpressure on Quinn's datagram queue.
    pub(crate) min_buffer_space: usize,
    /// `send_datagram` returning `Err` is rare (terminal conditions only);
    /// we still split by variant for completeness.
    pub(crate) drop_toolarge: u64,
    pub(crate) drop_closed: u64,
    pub(crate) drop_other: u64,
    pub(crate) drop_unfragmentable: u64,
    /// Payloads sent as one native TUIC datagram due to
    /// `native_no_fragment_max_payload`, bypassing app-level fragmentation.
    pub(crate) forced_single_datagram: u64,
    last_logged_in_count: u64,
}

impl Default for TuicOutboundStats {
    fn default() -> Self {
        Self {
            in_count: 0,
            frag_count: 0,
            max_frags_seen: 0,
            min_max_dg: usize::MAX,
            max_max_dg: 0,
            min_buffer_space: usize::MAX,
            drop_toolarge: 0,
            drop_closed: 0,
            drop_other: 0,
            drop_unfragmentable: 0,
            forced_single_datagram: 0,
            last_logged_in_count: 0,
        }
    }
}

impl TuicOutboundStats {
    const LOG_EVERY: u64 = 10_000;

    pub(crate) fn maybe_log(&mut self, assoc_id: u16) {
        if self.in_count.saturating_sub(self.last_logged_in_count) < Self::LOG_EVERY {
            return;
        }
        self.last_logged_in_count = self.in_count;
        // Observable drops — send errors + payloads we couldn't
        // fragment at all. NOT the same as the real loss rate, because
        // quinn 0.11.9 silently evicts older queued datagrams when its
        // buffer is full. Use `min_buffer_space` alongside this to infer
        // internal pressure.
        let err_drops = self
            .drop_toolarge
            .saturating_add(self.drop_closed)
            .saturating_add(self.drop_other)
            .saturating_add(self.drop_unfragmentable);
        let err_pct = if self.in_count == 0 {
            0.0
        } else {
            err_drops as f64 * 100.0 / self.in_count as f64
        };
        let avg_frags = if self.in_count == 0 {
            0.0
        } else {
            self.frag_count as f64 / self.in_count as f64
        };
        let min_max_dg = if self.min_max_dg == usize::MAX {
            0
        } else {
            self.min_max_dg
        };
        let min_buffer_space = if self.min_buffer_space == usize::MAX {
            0
        } else {
            self.min_buffer_space
        };
        tracing::info!(
            assoc_id,
            in_count = self.in_count,
            frag_count = self.frag_count,
            avg_frags = format!("{avg_frags:.2}"),
            max_frags = self.max_frags_seen,
            max_dg_min = min_max_dg,
            max_dg_max = self.max_max_dg,
            min_buffer_space,
            drop_toolarge = self.drop_toolarge,
            drop_closed = self.drop_closed,
            drop_other = self.drop_other,
            drop_unfragmentable = self.drop_unfragmentable,
            forced_single_datagram = self.forced_single_datagram,
            err_drops,
            err_pct = format!("{err_pct:.2}%"),
            "tuic outbound stats (assoc)"
        );
        // Reset the windowed min so each log line reflects the last ~1000
        // payloads' minimum rather than being stuck forever at the lowest
        // ever observed.
        self.min_buffer_space = usize::MAX;
        self.min_max_dg = usize::MAX;
        self.max_max_dg = 0;
    }
}
