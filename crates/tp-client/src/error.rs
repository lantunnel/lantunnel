//! Public error type for `tp-client`.
//!
//! Library crates should not expose `anyhow::Error` — callers can't match
//! on it, and the transitive `anyhow` dependency is awkward for
//! downstream users. Internal code paths still use `anyhow::Error` (the
//! wide error type makes `?` ergonomic for the engine's deep call tree);
//! we convert at the public boundary via [`From<anyhow::Error>`].
//!
//! # Variant choices
//!
//! Each variant covers one major failure category the caller might want
//! to act on differently:
//!
//! * [`EngineError::Tls`] — rustls/webpki problems while loading certs or
//!   building the client config. Usually means a misconfigured CA path.
//! * [`EngineError::Gateway`] — QUIC dial, DNS, or handshake failed.
//!   Network-level issue; retry with backoff typically fixes it.
//! * [`EngineError::Platform`] — the control-plane HTTP API returned a
//!   non-success response or an unparseable body. Auth failures land
//!   here.
//! * [`EngineError::HostFilter`] — allow/deny pattern did not compile.
//!   Operator-fixable.
//! * [`EngineError::Other`] — everything else. Carries the original
//!   `to_string()` so logs don't lose information. We accept this
//!   bucket's existence rather than enumerate every internal `bail!`;
//!   adding more variants only pays off when callers actually branch
//!   on them.

use std::io;

use thiserror::Error;

/// Errors surfaced from the public `tp-client` API.
#[derive(Debug, Error)]
pub enum EngineError {
    /// TLS client-config construction failed (cert loading, webpki, etc).
    #[error("tls setup: {0}")]
    Tls(String),

    /// QUIC dial / DNS / handshake to the gateway failed.
    #[error("gateway: {0}")]
    Gateway(String),

    /// Platform control-plane HTTP returned an error response.
    #[error("platform: {0}")]
    Platform(String),

    /// Host allow/deny pattern did not compile.
    #[error("host filter: {0}")]
    HostFilter(String),

    /// I/O at the library boundary (reqwest client build, etc).
    #[error("io: {0}")]
    Io(String),

    /// Catch-all for internal `anyhow` errors the library hasn't
    /// categorised yet. The formatted message is preserved.
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for EngineError {
    /// Flatten any internal `anyhow::Error` into `Other` — preserves the
    /// display chain (`{:#}`) so log output is unchanged.
    fn from(e: anyhow::Error) -> Self {
        EngineError::Other(format!("{e:#}"))
    }
}

impl From<io::Error> for EngineError {
    fn from(e: io::Error) -> Self {
        EngineError::Io(e.to_string())
    }
}

impl From<reqwest::Error> for EngineError {
    fn from(e: reqwest::Error) -> Self {
        EngineError::Platform(e.to_string())
    }
}

impl From<tp_host_filter::HostFilterError> for EngineError {
    fn from(e: tp_host_filter::HostFilterError) -> Self {
        EngineError::HostFilter(e.to_string())
    }
}

/// Convenience alias used by the public API.
pub type Result<T> = std::result::Result<T, EngineError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anyhow_converts_preserving_display() {
        let inner = anyhow::anyhow!("root").context("mid").context("outer");
        let e: EngineError = inner.into();
        match e {
            EngineError::Other(msg) => {
                // anyhow's `{:#}` formatter prints the full chain
                // separated by ": " — asserts all three layers landed.
                assert!(msg.contains("outer"), "missing outer: {msg}");
                assert!(msg.contains("mid"), "missing mid: {msg}");
                assert!(msg.contains("root"), "missing root: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn io_error_lands_in_io_variant() {
        let e: EngineError = io::Error::new(io::ErrorKind::NotFound, "boom").into();
        assert!(matches!(e, EngineError::Io(_)));
    }

    #[test]
    fn display_has_variant_prefix() {
        assert_eq!(
            EngineError::Tls("cert missing".into()).to_string(),
            "tls setup: cert missing"
        );
        assert_eq!(
            EngineError::HostFilter("bad cidr".into()).to_string(),
            "host filter: bad cidr"
        );
    }
}
