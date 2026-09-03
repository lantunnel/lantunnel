//! Phase 3 throughput tests (5 tasks total).
//!
//! Tracking IDs map 1:1 onto the plan's `e2e-p3-*` task list:
//!   * tcp_large_download      → e2e-p3-tcp-large-download
//!   * udp_burst               → e2e-p3-udp-burst
//!   * udp_streaming_game      → e2e-p3-udp-streaming-game
//!   * udp_stress_multi_stream → e2e-p3-udp-stress-multi-stream
//!   * tcp_half_close          → e2e-p3-tcp-half-close

pub mod tcp_half_close;
pub mod tcp_large_download;
pub mod udp_burst;
pub mod udp_streaming_game;
pub mod udp_stress_multi_stream;
