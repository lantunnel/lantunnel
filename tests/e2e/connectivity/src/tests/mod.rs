//! Phase 1 connectivity tests (12 tasks total).
//!
//! Sibling task modules stay commented out until each is implemented, to keep
//! `cargo check` clean of `unused`/`dead_code` warnings while the scaffold is
//! the only real code in the crate. Uncomment as each task lands.
//!
//! Tracking IDs map 1:1 onto the plan's `e2e-p1-*` task list:
//!   * socks5_tcp_connect       → e2e-p1-socks5-tcp-connect       (this task)
//!   * socks5_udp_associate     → e2e-p1-socks5-udp-associate
//!   * socks5_host_filter       → e2e-p1-socks5-host-filter
//!   * socks5_bad_auth          → e2e-p1-socks5-bad-auth
//!   * http_proxy_connect       → e2e-p1-http-proxy-connect
//!   * http_proxy_forward       → e2e-p1-http-proxy-forward
//!   * http_proxy_url_fetch     → e2e-p1-http-proxy-url-fetch
//!   * socks5_udp_dns_query     → e2e-p1-socks5-udp-dns-query
//!   * tuic_connect             → e2e-p1-tuic-connect
//!   * tuic_packet_native       → e2e-p1-tuic-packet-native
//!   * tuic_packet_quic_stream  → e2e-p1-tuic-packet-quic-stream
//!   * multi_replicas_rr        → e2e-p1-multi-replicas-rr

pub mod socks5_tcp_connect;
pub mod socks5_udp_associate;
