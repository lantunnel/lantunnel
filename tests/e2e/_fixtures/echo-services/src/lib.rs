//! Library entry point for the echo-services crate.
//!
//! The binary in `main.rs` is a thin shim — actual service logic lives
//! in these modules so integration tests under `tests/` can drive each
//! `serve()` directly without spawning a child process.

pub mod counters;
pub mod http;
pub mod tcp;
pub mod udp;
