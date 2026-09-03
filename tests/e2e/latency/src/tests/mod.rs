//! Phase 2 latency tests (4 tasks total).
//!
//! Tracking IDs map 1:1 onto the plan's `e2e-p2-*` task list:
//!   * latency_baseline       → e2e-p2-latency-baseline
//!   * latency_gamestream_sim → e2e-p2-latency-gamestream-sim
//!   * latency_stress_curve   → e2e-p2-latency-stress-curve
//!   * latency_cold_start     → e2e-p2-latency-cold-start

pub mod latency_baseline;
pub mod latency_gamestream_sim;
pub mod latency_stress_curve;
