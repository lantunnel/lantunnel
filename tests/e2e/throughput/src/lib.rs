//! Phase 3 throughput tests for Lantunnel E2E.
//!
//! Mirrors the Phase 1 (`tp-e2e-p1-connectivity`) and Phase 2
//! (`tp-e2e-p2-latency`) crate shapes:
//!   * `pub mod meter` — bytes-per-second / Mbps helper used by every
//!     Phase 3 test that needs to report sustained-throughput numbers.
//!   * `pub mod tests::*` — one module per Phase 3 task
//!     (`tcp_large_download`, `udp_burst`, `udp_streaming_game`,
//!     `udp_stress_multi_stream`, `tcp_half_close`).
//!
//! Phase 1 SOCKS5 helpers (`tp_e2e_p1_connectivity::socks5` and
//! `tp_e2e_p1_connectivity::socks5_udp`)
//! are reused directly via `[dependencies]` rather than duplicated.

pub mod meter;
pub mod proxy;
pub mod reporting;
pub mod tests;

pub use tp_e2e_p1_connectivity::parse_host_port;
