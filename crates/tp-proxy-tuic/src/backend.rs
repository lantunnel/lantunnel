//! What the TUIC frontend needs from whatever routes its traffic and from
//! whatever holds its identities.
//!
//! The frontend knows nothing about Gateways, Tunnels, or credential storage.
//! It resolves an identity to a route key plus a shared secret, then opens
//! tunnels with that key. Mirrors `tp_proxy_socks5::backend`.

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::error::TryRecvError;
use tp_transport::TrySendKind;

pub trait TcpTunnel: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> TcpTunnel for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub type BoxTcpTunnel = Pin<Box<dyn TcpTunnel>>;
pub type BoxUdpTunnel = Box<dyn UdpTunnel>;
pub type BoxUdpTunnelSender = Box<dyn UdpTunnelSender>;
pub type BoxUdpTunnelReceiver = Box<dyn UdpTunnelReceiver>;

pub trait UdpTunnelSender: Send + Sync {
    fn try_send(&self, payload: Bytes) -> Result<(), TrySendKind>;
}

#[async_trait]
pub trait UdpTunnelReceiver: Send {
    async fn recv(&mut self) -> Option<Bytes>;
    fn try_recv(&mut self) -> Result<Bytes, TryRecvError>;
    fn conn_id(&self) -> &str;
    async fn close(&mut self);
}

pub trait UdpTunnel: Send {
    fn split(self: Box<Self>) -> (BoxUdpTunnelSender, BoxUdpTunnelReceiver);
}

/// One TUIC identity: the shared secret its token is derived from, and the
/// opaque key used to route its traffic.
#[derive(Clone)]
pub struct TuicIdentity {
    /// Secret mixed into the TLS keying-material export. TUIC v5 authenticates
    /// by proving possession of this, so the protocol requires a shared secret;
    /// that is inherent to TUIC and not a Lantunnel design choice.
    pub secret: Vec<u8>,
    /// Passed to [`TuicBackend::open_tcp`] / [`TuicBackend::open_udp`] and never
    /// interpreted by this crate.
    pub route_key: String,
}

/// Resolves the identity a TUIC client claims in its Authenticate frame.
///
/// There is no shared credential store and no shared proxy secret; supplying
/// identities is entirely the embedder's problem.
pub trait TuicAuthenticator: Send + Sync + 'static {
    fn identity(&self, claimed: &str) -> Option<TuicIdentity>;
}

#[async_trait]
pub trait TuicBackend: Send + Sync + 'static {
    async fn open_tcp(&self, route: &str, target: &str) -> anyhow::Result<BoxTcpTunnel>;
    async fn open_udp(&self, route: &str, target: &str) -> anyhow::Result<BoxUdpTunnel>;

    fn increment_listener_rejects(&self) {}
    fn increment_udp_drops(&self) {}
}
