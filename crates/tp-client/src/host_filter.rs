//! Client host-pattern filter backed by the shared matcher.

pub use tp_host_filter::HostFilterError;

#[derive(Debug, Clone)]
pub struct HostFilter {
    inner: tp_host_filter::HostFilter,
}

impl HostFilter {
    /// Compile the configured allow and deny patterns.
    ///
    /// Keep the public client error type stable while sharing the matcher
    /// implementation with the gateway.
    pub fn new(forbidden: &[String], allowed: &[String]) -> crate::Result<Self> {
        Ok(Self {
            inner: tp_host_filter::HostFilter::new(forbidden, allowed)?,
        })
    }

    pub fn is_allowed(&self, address: &str) -> bool {
        self.inner.is_allowed(address)
    }
}
