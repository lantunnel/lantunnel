//! What the HTTP proxy frontend needs from whatever routes its traffic.
//!
//! The frontend deliberately knows nothing about Gateways, Tunnels, or how a
//! route key is authenticated. It hands the key its validator returned to
//! `open_tcp` and writes bytes. Mirrors `tp_proxy_socks5::backend`.

use std::pin::Pin;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait TcpTunnel: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> TcpTunnel for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub type BoxTcpTunnel = Pin<Box<dyn TcpTunnel>>;

#[async_trait]
pub trait HttpProxyBackend: Send + Sync + 'static {
    /// Open a TCP tunnel to `target` on behalf of `route`.
    ///
    /// `route` is whatever the [`crate::AuthValidator`] returned. This crate
    /// treats it as an opaque routing key and never interprets it.
    async fn open_tcp(&self, route: &str, target: &str) -> anyhow::Result<BoxTcpTunnel>;

    fn increment_listener_rejects(&self) {}
}
