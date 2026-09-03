use async_trait::async_trait;
use bytes::Bytes;
use std::pin::Pin;
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

#[async_trait]
pub trait Socks5Backend: Send + Sync + 'static {
    async fn open_tcp(&self, group_id: &str, target: &str) -> anyhow::Result<BoxTcpTunnel>;
    async fn open_udp(&self, group_id: &str, target: &str) -> anyhow::Result<BoxUdpTunnel>;

    fn increment_listener_rejects(&self) {}
    fn increment_udp_drops(&self) {}
}
