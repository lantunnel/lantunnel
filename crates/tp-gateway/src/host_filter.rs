//! Gateway-side host filter policy (R1).
//!
//! The gateway uses the shared `tp-host-filter` matcher so its configured
//! `forbidden_hosts` / `allowed_hosts` grammar stays in sync with the tunnel
//! client. Gateway policy still layers a non-overridable default forbidden
//! baseline on top of per-tunnel configuration.

pub use tp_host_filter::HostFilterError;

/// Hosts the gateway must never connect to, regardless of per-tunnel
/// configuration. Covers the common SSRF abuse targets:
///
/// * `0.0.0.0/8` covers IPv4 unspecified addresses.
/// * `169.254.169.254`, `169.254.170.2`, and `100.100.100.200`
///   cover common cloud metadata endpoints without blocking the whole
///   IPv4 link-local range, which Phase 2 LAN routes may intentionally use.
/// * `metadata.google.internal`, `instance-data.ec2.internal`, and
///   `metadata.azure.com` are the named DNS endpoints for GCP / AWS /
///   Azure instance metadata.
pub const DEFAULT_FORBIDDEN: &[&str] = &[
    "0.0.0.0/8",
    "169.254.169.254",
    "169.254.170.2",
    "100.100.100.200",
    "metadata.google.internal",
    "instance-data.ec2.internal",
    "metadata.azure.com",
];

/// Compiled gateway host filter. It wraps the shared matcher and always adds
/// [`DEFAULT_FORBIDDEN`] before per-tunnel forbidden patterns.
#[derive(Debug, Clone)]
pub struct HostFilter {
    inner: tp_host_filter::HostFilter,
}

impl HostFilter {
    pub fn new(forbidden: &[String], allowed: &[String]) -> Result<Self, HostFilterError> {
        Ok(Self {
            inner: tp_host_filter::HostFilter::new_with_defaults(
                DEFAULT_FORBIDDEN,
                forbidden,
                allowed,
            )?,
        })
    }

    /// Returns `true` when the gateway is allowed to dial `address`.
    ///
    /// Semantics:
    /// 1. `address` is rejected if **any** forbidden pattern matches
    ///    (including the compile-time defaults).
    /// 2. Otherwise, if `allowed_hosts` is non-empty, at least one
    ///    allowed pattern must match.
    /// 3. With empty `allowed_hosts`, any non-forbidden host passes.
    pub fn is_allowed(&self, address: &str) -> bool {
        self.inner.is_allowed(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(forbidden: &[String], allowed: &[String]) -> HostFilter {
        HostFilter::new(forbidden, allowed).expect("host filter should compile")
    }

    fn empty() -> HostFilter {
        filter(&[], &[])
    }

    #[test]
    fn default_forbidden_blocks_cloud_metadata() {
        let f = empty();
        assert!(!f.is_allowed("metadata.google.internal:80"));
        assert!(!f.is_allowed("instance-data.ec2.internal:80"));
        assert!(!f.is_allowed("metadata.azure.com:443"));
        assert!(!f.is_allowed("169.254.169.254:80"));
        assert!(!f.is_allowed("169.254.170.2:80"));
        assert!(!f.is_allowed("100.100.100.200:80"));
    }

    #[test]
    fn default_forbidden_allows_non_metadata_link_local_lan_targets() {
        let f = empty();
        assert!(f.is_allowed("169.254.10.20:80"));
    }

    #[test]
    fn empty_lists_pass_arbitrary_hosts() {
        let f = empty();
        assert!(f.is_allowed("api.example.com:443"));
        assert!(f.is_allowed("8.8.8.8:53"));
    }

    #[test]
    fn forbidden_overrides_any_allowed_intent() {
        let f = filter(&["evil.example.com".into()], &[]);
        assert!(!f.is_allowed("evil.example.com:443"));
        assert!(f.is_allowed("good.example.com:443"));
    }

    #[test]
    fn forbidden_suffix_wildcard_matches_subdomains() {
        let f = filter(&["*.evil.example".into()], &[]);
        assert!(!f.is_allowed("api.evil.example:443"));
        assert!(!f.is_allowed("nested.api.evil.example:443"));
        assert!(!f.is_allowed("evil.example:443"));
        assert!(f.is_allowed("evil.examplexx:443"));
    }

    #[test]
    fn allowed_list_acts_as_allowlist_when_non_empty() {
        let f = filter(&[], &["10.0.0.0/8".into()]);
        assert!(f.is_allowed("10.1.2.3:22"));
        assert!(!f.is_allowed("8.8.8.8:53"));
    }

    #[test]
    fn allowed_regex_matches_target() {
        let f = filter(&[], &["^api-[0-9]+\\.example\\.com:443$".into()]);
        assert!(f.is_allowed("api-42.example.com:443"));
        assert!(!f.is_allowed("api-dev.example.com:443"));
        assert!(!f.is_allowed("api-42.example.com:80"));
    }

    #[test]
    fn forbidden_regex_overrides_allow_all() {
        let f = filter(&["^metadata-[a-z]+\\.internal:.*$".into()], &["*".into()]);
        assert!(!f.is_allowed("metadata-prod.internal:80"));
        assert!(f.is_allowed("api.example.com:443"));
    }

    #[test]
    fn ipv6_bracket_host_is_parsed() {
        let f = empty();
        assert!(f.is_allowed("[2001:db8::1]:443"));
    }

    #[test]
    fn forbidden_cidr_blocks_loopback_range() {
        let f = filter(&["127.0.0.0/8".into()], &[]);
        assert!(!f.is_allowed("127.0.0.1:22"));
        assert!(!f.is_allowed("127.42.42.42:22"));
        assert!(f.is_allowed("128.0.0.1:22"));
    }

    #[test]
    fn empty_address_is_rejected() {
        let f = empty();
        assert!(!f.is_allowed(""));
    }
}
