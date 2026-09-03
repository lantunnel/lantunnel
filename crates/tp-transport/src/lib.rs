//! QUIC transport for Lantunnel.
//!
//! A `Session` is a bidirectional channel of `BinaryMessage`s carried over a
//! single QUIC bi-directional stream. Frames are length-prefixed with a u32
//! big-endian length, followed by the raw bytes produced by `tp_core::protocol::pack`.

pub mod datagram_scheduler;
pub mod drop_oldest;
pub mod grpc;
pub mod quic;
pub mod session;
pub mod tls;
pub mod ws;

pub use datagram_scheduler::{
    datagram_scheduler_channel, DatagramFrame, DatagramSchedulerConfig, DatagramSchedulerReceiver,
    DatagramSchedulerSender,
};
pub use drop_oldest::{drop_oldest_channel, DropOldestReceiver, DropOldestSender};
pub use grpc::{GrpcClient, GrpcServer};
pub use quic::{
    bind_tuned_udp, AuthHandler, AuthParams, QuicClient, QuicServer, QuicTuning,
    QUIC_DATAGRAM_BUFFER_BYTES, UDP_SOCKET_RECV_BUF_BYTES, UDP_SOCKET_SEND_BUF_BYTES,
};
pub use session::{
    DatagramBufSpaceFn, DatagramMtuFn, DatagramReceiver, Session, SessionReceiver, SessionSender,
    TcpFlowIncoming, TcpFlowIncomingReceiver, TcpFlowStream, TrySendKind, UdpDataMode,
    UdpRouteStats,
};
pub use ws::{WsClient, WsServer};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("tls error: {0}")]
    Tls(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("quic connect error: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("quic connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("quic write error: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("quic read error: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("quic read-to-end error: {0}")]
    ReadToEnd(#[from] quinn::ReadToEndError),
    #[error("protocol error: {0}")]
    Protocol(#[from] tp_core::protocol::ProtoError),
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("datagram transport unavailable")]
    DatagramUnavailable,
    #[error("tcp flow stream unavailable")]
    FlowStreamUnavailable,
    #[error("closed")]
    Closed,
    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Upper bound on one packed tunnel message frame (8 MiB).
///
/// This is a protocol-level per-frame guard, not a QUIC reliable-stream
/// capacity limit. QUIC streams can carry larger byte sequences, but this
/// transport frames them as `[len:u32][packed BinaryMessage]`; bounding each
/// packed frame prevents bad peers or local bugs from forcing huge buffer
/// allocations or filling mpsc queues with jumbo messages. Large TCP flows
/// should be split into normal-sized `Data` frames by the pipe layer.
pub const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024;
