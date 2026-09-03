//! Phase 2 latency tests for Lantunnel E2E.
//!
//! Mirrors the Phase 1 (`tp-e2e-p1-connectivity`) crate shape:
//!   * `pub mod stats` — `hdrhistogram`-backed P50/P95/P99 helper used by
//!     every Phase 2 test that records per-sample round-trip times.
//!   * `pub mod proxy` — thin wrappers that build proxied vs direct
//!     TCP/UDP transports against the loopback fixtures, so each test
//!     module focuses on the workload rather than the dial dance.
//!   * `pub mod tests::*` — one module per Phase 2 task (`latency_baseline`,
//!     `latency_gamestream_sim`, `latency_stress_curve`).
//!
//! Phase 1 SOCKS5 helpers (`tp_e2e_p1_connectivity::socks5` and
//! `tp_e2e_p1_connectivity::socks5_udp`)
//! are reused directly via `[dependencies]` rather than duplicated.

pub mod proxy;
pub mod reporting;
pub mod stats;
pub mod tests;

pub use tp_e2e_p1_connectivity::parse_host_port;
