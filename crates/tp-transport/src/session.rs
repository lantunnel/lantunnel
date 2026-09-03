//! Transport-agnostic `Session` — a pair of async channels carrying
//! `BinaryMessage` plus a handle to close the underlying connection.
//!
//! Both QUIC (`quic.rs`) and WebSocket (`ws.rs`) transports spawn their own
//! reader/writer tasks and construct a `Session` via [`Session::new_channeled`].
//!
//! ## Splitting
//!
//! Callers typically drive outbound and inbound on separate tasks.
//! [`Session::split`] consumes a `Session` and returns
//! `(SessionSender, SessionReceiver, Option<DatagramReceiver>)` — the sender
//! is cheaply cloneable and `Send + Sync`, so multiple producers can push
//! concurrently; the receivers are single-consumer. Dropping any half keeps
//! the background transport tasks alive until all halves are dropped.
//!
//! ## Decoupled stream / datagram inbound channels (Go parity)
//!
//! Datagrams are handled on a separate task, entirely bypassing the stream
//! read path. That keeps UDP datagrams from
//! queueing behind a slow TCP consumer — critical for game-streaming
//! latency where each packet has a sub-4ms deadline.
//!
//! We mirror that here: stream-received `BinaryMessage`s go to a dedicated
//! `stream_in_tx` mpsc; datagram-received `BinaryMessage`s (always
//! `UdpData` in the current protocol) go to a separate `datagram_in_tx`.
//! `Session::split` returns both receivers so the caller spawns two
//! independent consumer tasks.
//!
//! ## Outbound channel type: `PackedMessage` (header + optional payload)
//!
//! The outbound queues carry `PackedMessage` values — the output of
//! [`tp_core::protocol::pack`]. Each carries a required `header: Bytes`
//! plus an optional `payload: Bytes` (only present for `Data` / `UdpData`).
//! Transports that can vectored-write (QUIC stream via `write_chunks`)
//! dispatch the two chunks separately, avoiding the per-frame memcpy of
//! the old single-`Bytes` channel; transports that can't (WS Binary
//! frame, gRPC `data` field, QUIC datagram) call `PackedMessage::to_bytes`
//! to merge before sending. Packing happens once per message inside
//! [`SessionSender::send`] / [`SessionSender::try_send`] — no double-pack.
//!
//! ## UDP fast path with dynamic MTU
//!
//! When a datagram sender is attached via [`Session::with_datagram_channel`],
//! outbound `BinaryMessage::UdpData` that fits the connection's current
//! `max_datagram_size` is written straight to QUIC datagrams instead of
//! serialized onto the single bidi stream. The MTU is queried **fresh** on
//! every send (via a caller-supplied closure) so Path MTU Discovery expansion
//! is honored immediately. UDP packets that fit use QUIC datagrams. Packets
//! that exceed the live MTU are counted and dropped so realtime video/audio
//! never falls back onto the reliable stream and head-of-line blocks.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tp_core::protocol::{
    pack, unpack, BinaryMessage, PackedMessage, TcpFlowStreamPreface, TransportCapabilities,
};

use crate::datagram_scheduler::{DatagramEnqueueOutcome, DatagramFrame, DatagramSchedulerSender};
use crate::drop_oldest::DropOldestReceiver;
use crate::quic::QUIC_DATAGRAM_BUFFER_BYTES;
use crate::{Result, TransportError, MAX_FRAME_LEN};

pub(crate) const HEADER_AUTH_TRANSPORT_CAPABILITIES: TransportCapabilities =
    TransportCapabilities {
        route_bind_control_v1: true,
        tcp_flow_stream_v1: false,
        relay_source_attestation_v1: true,
        peer_mesh_v2: true,
    };

pub(crate) fn header_auth_capability_mask(capabilities: TransportCapabilities) -> u8 {
    capabilities.flags()
}

pub(crate) fn header_auth_offered_capabilities(
    requested: TransportCapabilities,
) -> TransportCapabilities {
    TransportCapabilities {
        peer_mesh_v2: requested.peer_mesh_v2,
        ..HEADER_AUTH_TRANSPORT_CAPABILITIES
    }
}

pub(crate) fn header_auth_capabilities_from_mask(mask: u8) -> TransportCapabilities {
    TransportCapabilities::from_flags(mask)
}

pub(crate) fn negotiate_header_auth_capabilities(
    offered: TransportCapabilities,
) -> TransportCapabilities {
    TransportCapabilities {
        route_bind_control_v1: offered.route_bind_control_v1
            && HEADER_AUTH_TRANSPORT_CAPABILITIES.route_bind_control_v1,
        tcp_flow_stream_v1: false,
        relay_source_attestation_v1: offered.relay_source_attestation_v1
            && HEADER_AUTH_TRANSPORT_CAPABILITIES.relay_source_attestation_v1,
        peer_mesh_v2: offered.peer_mesh_v2 && HEADER_AUTH_TRANSPORT_CAPABILITIES.peer_mesh_v2,
    }
}

/// Closure that returns the live `max_datagram_size` for the underlying
/// transport, or `None` if datagrams aren't negotiated yet / at all.
pub type DatagramMtuFn = Arc<dyn Fn() -> Option<usize> + Send + Sync + 'static>;

/// Closure that returns the live remaining bytes available in the outgoing
/// QUIC datagram buffer (via `quinn::Connection::datagram_send_buffer_space`).
/// When this approaches zero on a sender, quinn 0.11.9 silently evicts older
/// queued datagrams to make room for new ones — the hidden drop path that
/// makes our own scheduler-accepted counter overstate the real wire throughput.
///
/// We sample this periodically (tunnel replica summary / gateway client
/// conn summary) precisely to detect that condition and distinguish it
/// from network-level loss downstream.
pub type DatagramBufSpaceFn = Arc<dyn Fn() -> usize + Send + Sync + 'static>;

/// Snapshot of transport-level health for the underlying connection. Surfaced
/// to the P2P scheduler so it can decide which `Session` (e.g. relay vs.
/// peer-to-peer hole-punched path) is currently the better carrier for a
/// given flow.
///
/// All fields are best-effort. `pto_count` may be `0` when the underlying
/// transport doesn't expose it; quinn 0.11's public `PathStats` doesn't
/// surface the QUIC PTO counter directly, so the QUIC probe approximates
/// it from `black_holes_detected` (see `quic.rs`). The scheduler's health
/// predicate is designed to degrade gracefully (i.e. `pto_count < 3` stays
/// healthy).
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionStats {
    /// Smoothed RTT estimate.
    pub rtt: std::time::Duration,
    /// Recent loss ratio, in `[0.0, 1.0]`. Computed as `lost_packets / sent_packets`
    /// over the connection's lifetime.
    pub loss_rate: f64,
    /// Probe-Timeout (PTO) backoff count. `0` when the transport doesn't
    /// expose it; see struct-level note.
    pub pto_count: u32,
}

/// Closure that returns a fresh [`SessionStats`] snapshot. The QUIC layer
/// installs one of these at `wrap()` time so `Session::stats()` can fan out
/// to live `quinn::Connection` metrics without holding a connection ref
/// directly (keeping `Session` transport-agnostic).
pub type SessionStatsFn = Arc<dyn Fn() -> SessionStats + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UdpDataMode {
    #[default]
    ReliableUdpEmulationAllowed,
    QuicDatagramRequired,
}

/// Non-destructive snapshot of sender-side queue and UDP routing state.
/// Existing `take_*` UDP counters remain reset-on-read for interval logs;
/// this type is for placement scoring and diagnostics that need a cheap
/// point-in-time view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionQueueSnapshot {
    pub stream_queue_used: usize,
    pub stream_queue_capacity: usize,
    pub datagram_send_buffer_space: Option<usize>,
    pub datagram_send_buffer_capacity: Option<usize>,
    pub udp_route_stats: UdpRouteStatsSnapshot,
}

pub trait TcpFlowIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> TcpFlowIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

pub struct TcpFlowStream {
    conn_id: String,
    network: String,
    address: String,
    raw_preface: Option<Bytes>,
    io: Pin<Box<dyn TcpFlowIo>>,
}

impl TcpFlowStream {
    pub fn new(preface: TcpFlowStreamPreface, io: Pin<Box<dyn TcpFlowIo>>) -> Self {
        Self {
            conn_id: preface.conn_id,
            network: preface.network,
            address: preface.address,
            raw_preface: None,
            io,
        }
    }

    #[doc(hidden)]
    pub fn new_raw(conn_id: String, raw_preface: Bytes, io: Pin<Box<dyn TcpFlowIo>>) -> Self {
        Self {
            conn_id,
            network: "tcp".into(),
            address: String::new(),
            raw_preface: Some(raw_preface),
            io,
        }
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Complete opaque V2 OPEN frame, when this stream was accepted through
    /// the raw QUIC flow hook. Callers must forward it unchanged.
    pub fn raw_preface(&self) -> Option<&Bytes> {
        self.raw_preface.as_ref()
    }

    pub async fn send_connect_response(&mut self, success: bool, error: String) -> Result<()> {
        let bytes = pack(&BinaryMessage::ConnectResponse {
            conn_id: self.conn_id.clone(),
            success,
            error,
        })
        .to_bytes();
        write_tcp_flow_frame(self, &bytes).await
    }

    pub async fn read_connect_response(&mut self) -> Result<std::result::Result<(), String>> {
        match unpack(&read_tcp_flow_frame(self).await?)? {
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } if conn_id == self.conn_id => {
                if success {
                    Ok(Ok(()))
                } else {
                    Ok(Err(error))
                }
            }
            BinaryMessage::ConnectResponse { .. } => Err(TransportError::Unexpected(
                "ConnectResponse for different TCP flow",
            )),
            _ => Err(TransportError::Unexpected("ConnectResponse")),
        }
    }
}

impl AsyncRead for TcpFlowStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.io.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpFlowStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.io.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.io.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.io.as_mut().poll_shutdown(cx)
    }
}

pub struct TcpFlowIncoming {
    pub preface: TcpFlowStreamPreface,
    pub stream: TcpFlowStream,
}

pub struct TcpFlowIncomingReceiver {
    rx: mpsc::Receiver<TcpFlowIncoming>,
    _keepalive: Arc<Keepalive>,
}

impl TcpFlowIncomingReceiver {
    pub async fn recv(&mut self) -> Option<TcpFlowIncoming> {
        self.rx.recv().await
    }
}

#[derive(Clone)]
pub(crate) struct TcpFlowConnector {
    tx: mpsc::Sender<TcpFlowOpenRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TcpFlowOpen {
    Legacy(TcpFlowStreamPreface),
    Raw(Bytes),
}

pub(crate) struct TcpFlowOpenRequest {
    pub open: TcpFlowOpen,
    pub timeout: Duration,
    pub response: oneshot::Sender<Result<TcpFlowStream>>,
}

impl TcpFlowConnector {
    pub(crate) fn new(tx: mpsc::Sender<TcpFlowOpenRequest>) -> Self {
        Self { tx }
    }

    async fn open(
        &self,
        preface: TcpFlowStreamPreface,
        timeout: Duration,
    ) -> Result<TcpFlowStream> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(TcpFlowOpenRequest {
                open: TcpFlowOpen::Legacy(preface),
                timeout,
                response,
            })
            .await
            .map_err(|_| TransportError::FlowStreamUnavailable)?;
        rx.await
            .map_err(|_| TransportError::FlowStreamUnavailable)?
    }

    async fn open_raw(&self, preface: Bytes, timeout: Duration) -> Result<TcpFlowStream> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(TcpFlowOpenRequest {
                open: TcpFlowOpen::Raw(preface),
                timeout,
                response,
            })
            .await
            .map_err(|_| TransportError::FlowStreamUnavailable)?;
        rx.await
            .map_err(|_| TransportError::FlowStreamUnavailable)?
    }
}

pub async fn write_tcp_flow_frame<W>(writer: &mut W, bytes: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let len = bytes.len();
    if len as u64 > MAX_FRAME_LEN as u64 {
        return Err(TransportError::FrameTooLarge(len as u32));
    }
    writer.write_all(&(len as u32).to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_tcp_flow_frame<R>(reader: &mut R) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut buf = Vec::new();
    read_tcp_flow_frame_into(reader, &mut buf).await?;
    Ok(buf)
}

/// Read one bounded TCP flow record into a caller-owned buffer so sealed V2
/// flow pumps can reuse allocation for every record.
pub async fn read_tcp_flow_frame_into<R>(reader: &mut R, buf: &mut Vec<u8>) -> Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::Other(e.to_string()))?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(TransportError::FrameTooLarge(len));
    }
    buf.clear();
    buf.resize(len as usize, 0);
    reader
        .read_exact(buf)
        .await
        .map_err(|e| TransportError::Other(e.to_string()))?;
    Ok(())
}

/// Read one bounded TCP flow record into reusable `BytesMut` storage.
pub async fn read_tcp_flow_frame_into_bytes<R>(reader: &mut R, buf: &mut BytesMut) -> Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::Other(e.to_string()))?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(TransportError::FrameTooLarge(len));
    }
    buf.clear();
    let len = len as usize;
    buf.reserve(len);
    while buf.len() < len {
        let remaining = len - buf.len();
        let mut limited = (&mut *buf).limit(remaining);
        let read = reader
            .read_buf(&mut limited)
            .await
            .map_err(|e| TransportError::Other(e.to_string()))?;
        if read == 0 {
            return Err(TransportError::Other(
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string(),
            ));
        }
    }
    Ok(())
}

impl SessionQueueSnapshot {
    pub fn stream_queue_used_ratio(&self) -> Option<f64> {
        ratio(self.stream_queue_used, self.stream_queue_capacity)
    }

    pub fn datagram_send_buffer_space_ratio(&self) -> Option<f64> {
        ratio(
            self.datagram_send_buffer_space?,
            self.datagram_send_buffer_capacity?,
        )
    }
}

fn datagram_send_buffer_capacity(datagram_buf_space: &Option<DatagramBufSpaceFn>) -> Option<usize> {
    datagram_buf_space
        .as_ref()
        .map(|_| QUIC_DATAGRAM_BUFFER_BYTES)
}

const UDP_FRAGMENT_HEADER_LEN: usize = 20;
const MAX_UDP_FRAGMENTS: usize = u8::MAX as usize;

fn record_oversized_udp_drop_on_stats(stats: &UdpRouteStats, packed_len: usize, max_dg: usize) {
    stats
        .last_fallback_packed_len
        .store(packed_len, Ordering::Relaxed);
    stats.last_fallback_max_dg.store(max_dg, Ordering::Relaxed);
    stats.dropped_full.fetch_add(1, Ordering::Relaxed);
}

fn record_datagram_scheduler_outcome(stats: &UdpRouteStats, outcome: &DatagramEnqueueOutcome) {
    let evicted = outcome.per_association_evicted + outcome.global_budget_evicted;
    if outcome.accepted_packets > 0 {
        stats
            .datagram_accepted_to_scheduler
            .fetch_add(outcome.accepted_packets as u64, Ordering::Relaxed);
    }
    if outcome.per_association_evicted > 0 {
        stats
            .datagram_per_association_evicted
            .fetch_add(outcome.per_association_evicted as u64, Ordering::Relaxed);
    }
    if outcome.global_budget_evicted > 0 {
        stats
            .datagram_global_budget_evicted
            .fetch_add(outcome.global_budget_evicted as u64, Ordering::Relaxed);
    }
    if evicted > 0 {
        stats
            .dropped_full
            .fetch_add(evicted as u64, Ordering::Relaxed);
    }
}

fn enqueue_datagram_frame(
    dg_tx: &DatagramSchedulerSender,
    stats: &UdpRouteStats,
    conn_id: String,
    packed: PackedMessage,
    fragment_group: Option<u64>,
) -> std::result::Result<(), TrySendKind> {
    if dg_tx.is_closed() {
        return Err(TrySendKind::Closed);
    }
    let frame = DatagramFrame {
        conn_id,
        bytes: packed.total_len(),
        packed,
        fragment_group,
    };
    let outcome = dg_tx.enqueue(frame);
    record_datagram_scheduler_outcome(stats, &outcome);
    if outcome.accepted {
        Ok(())
    } else if dg_tx.is_closed() {
        Err(TrySendKind::Closed)
    } else {
        stats.dropped_full.fetch_add(1, Ordering::Relaxed);
        Err(TrySendKind::Full)
    }
}

fn try_fragment_udp_to_datagrams(
    dg_tx: &DatagramSchedulerSender,
    stats: &UdpRouteStats,
    frag_seq: &AtomicU32,
    conn_id: &str,
    payload: Bytes,
    original_packed_len: usize,
    max_dg: usize,
) -> std::result::Result<(), TrySendKind> {
    let Some(max_fragment_payload) = max_dg.checked_sub(UDP_FRAGMENT_HEADER_LEN) else {
        record_oversized_udp_drop_on_stats(stats, original_packed_len, max_dg);
        return Err(TrySendKind::Full);
    };
    if max_fragment_payload == 0 {
        record_oversized_udp_drop_on_stats(stats, original_packed_len, max_dg);
        return Err(TrySendKind::Full);
    }
    // `max_dg` is Quinn's live per-connection PMTUD result. First derive the
    // minimum number of fragments that can traverse this exact QUIC path, then
    // spread the payload evenly across that fixed count. This preserves the
    // minimum packet/header/PPS cost while avoiding a near-MTU first fragment
    // followed by a tiny tail. The same transport rule applies to Gateway and
    // Direct P2P sessions; a UdpData that already fits never enters this path.
    let fragment_count = payload.len().div_ceil(max_fragment_payload);
    if fragment_count == 0 || fragment_count > MAX_UDP_FRAGMENTS {
        record_oversized_udp_drop_on_stats(stats, original_packed_len, max_dg);
        return Err(TrySendKind::Full);
    }
    let fragment_payload_base = payload.len() / fragment_count;
    let larger_fragment_count = payload.len() % fragment_count;
    debug_assert!(
        fragment_payload_base + usize::from(larger_fragment_count > 0) <= max_fragment_payload
    );
    if dg_tx.is_closed() {
        return Err(TrySendKind::Closed);
    }

    let frag_id = frag_seq.fetch_add(1, Ordering::Relaxed);
    let mut frames = Vec::with_capacity(fragment_count);
    for fragment_index in 0..fragment_count {
        let start =
            fragment_index * fragment_payload_base + fragment_index.min(larger_fragment_count);
        let fragment_len =
            fragment_payload_base + usize::from(fragment_index < larger_fragment_count);
        let end = start + fragment_len;
        let packed = pack(&BinaryMessage::UdpFragment {
            conn_id: conn_id.to_string(),
            frag_id,
            frag_index: fragment_index as u8,
            frag_total: fragment_count as u8,
            payload: payload.slice(start..end),
        });
        debug_assert_eq!(packed.header.len(), UDP_FRAGMENT_HEADER_LEN);
        if packed.total_len() > max_dg {
            record_oversized_udp_drop_on_stats(stats, original_packed_len, max_dg);
            return Err(TrySendKind::Full);
        }
        frames.push(DatagramFrame {
            conn_id: conn_id.to_string(),
            bytes: packed.total_len(),
            packed,
            fragment_group: Some(frag_id as u64),
        });
    }
    let outcome = dg_tx.enqueue_group(frames);
    record_datagram_scheduler_outcome(stats, &outcome);
    if outcome.accepted {
        Ok(())
    } else if dg_tx.is_closed() {
        Err(TrySendKind::Closed)
    } else {
        record_oversized_udp_drop_on_stats(stats, original_packed_len, max_dg);
        Err(TrySendKind::Full)
    }
}

/// Keepalive for the background transport tasks. Holding one (via any split
/// half) keeps the transport reader+writer+datagram tasks running; dropping
/// the last handle lets them tear down.
struct Keepalive {
    _writer: JoinHandle<()>,
    _reader: JoinHandle<()>,
    _control_writer: Option<JoinHandle<()>>,
    _control_reader: Option<JoinHandle<()>>,
    _datagram_writer: Option<JoinHandle<()>>,
    _datagram_reader: Option<JoinHandle<()>>,
    _tcp_flow_manager: Option<JoinHandle<()>>,
}

/// Bidirectional, message-oriented session decoupled from the transport.
pub struct Session {
    stream_tx: mpsc::Sender<PackedMessage>,
    control_tx: Option<mpsc::Sender<PackedMessage>>,
    stream_rx: mpsc::Receiver<BinaryMessage>,
    control_rx: Option<mpsc::Receiver<BinaryMessage>>,
    peer: SocketAddr,
    closer: Arc<dyn Fn() + Send + Sync + 'static>,
    /// Outbound datagram queue; `None` when the transport has no datagram path.
    datagram_tx: Option<DatagramSchedulerSender>,
    /// Inbound datagram queue; `None` when the transport has no datagram path.
    datagram_rx: Option<DropOldestReceiver<BinaryMessage>>,
    /// Live max-datagram-size probe. Called on every `send()` so PMTUD growth
    /// is honored immediately. `None` disables datagram routing.
    datagram_mtu: Option<DatagramMtuFn>,
    /// Live QUIC outgoing datagram-buffer-space probe. Sampled in the
    /// per-replica / per-clientconn summary logs; the ground-truth signal
    /// for "is quinn silently evicting my datagrams?".
    datagram_buf_space: Option<DatagramBufSpaceFn>,
    /// Live transport-health probe (rtt / loss / pto). Installed by the
    /// transport's `wrap()` factory; `None` when the transport can't supply
    /// metrics (in which case all accessors return zero / default).
    stats_probe: Option<SessionStatsFn>,
    /// Session-wide UDP route counters, shared with every `SessionSender` clone.
    stats: Arc<UdpRouteStats>,
    /// Session-wide UDP fragment id generator. Shared by every sender clone
    /// so fragmented datagrams do not collide across concurrent producers.
    udp_frag_seq: Arc<AtomicU32>,
    udp_data_mode: UdpDataMode,
    capabilities: TransportCapabilities,
    tcp_flow_connector: Option<TcpFlowConnector>,
    tcp_flow_rx: Option<mpsc::Receiver<TcpFlowIncoming>>,
    keepalive: Arc<Keepalive>,
}

impl Session {
    /// Build a Session from a pre-wired channel pair + background tasks + closer.
    /// Stream-only; call [`Session::with_datagram_channel`] to attach UDP fast path.
    pub fn new_channeled(
        out_tx: mpsc::Sender<PackedMessage>,
        in_rx: mpsc::Receiver<BinaryMessage>,
        peer: SocketAddr,
        closer: Arc<dyn Fn() + Send + Sync + 'static>,
        writer: JoinHandle<()>,
        reader: JoinHandle<()>,
    ) -> Self {
        Self {
            stream_tx: out_tx,
            control_tx: None,
            stream_rx: in_rx,
            control_rx: None,
            peer,
            closer,
            datagram_tx: None,
            datagram_rx: None,
            datagram_mtu: None,
            datagram_buf_space: None,
            stats_probe: None,
            stats: Arc::new(UdpRouteStats::default()),
            udp_frag_seq: Arc::new(AtomicU32::new(1)),
            udp_data_mode: UdpDataMode::default(),
            capabilities: TransportCapabilities::default(),
            tcp_flow_connector: None,
            tcp_flow_rx: None,
            keepalive: Arc::new(Keepalive {
                _writer: writer,
                _reader: reader,
                _control_writer: None,
                _control_reader: None,
                _datagram_writer: None,
                _datagram_reader: None,
                _tcp_flow_manager: None,
            }),
        }
    }

    pub fn with_capabilities(mut self, capabilities: TransportCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_udp_data_mode(mut self, mode: UdpDataMode) -> Self {
        self.udp_data_mode = mode;
        self
    }

    pub fn udp_data_mode(&self) -> UdpDataMode {
        self.udp_data_mode
    }

    pub fn udp_datagram_available(&self) -> bool {
        self.current_max_datagram_size().is_some()
    }

    pub fn capabilities(&self) -> TransportCapabilities {
        self.capabilities
    }

    pub async fn open_tcp_flow_stream(
        &self,
        conn_id: String,
        address: String,
        timeout: Duration,
    ) -> Result<TcpFlowStream> {
        if !self.capabilities.tcp_flow_stream_v1 {
            return Err(TransportError::FlowStreamUnavailable);
        }
        let Some(connector) = &self.tcp_flow_connector else {
            return Err(TransportError::FlowStreamUnavailable);
        };
        connector
            .open(
                TcpFlowStreamPreface {
                    conn_id,
                    network: "tcp".into(),
                    address,
                },
                timeout,
            )
            .await
    }

    pub async fn open_raw_tcp_flow_stream(
        &self,
        preface: Bytes,
        timeout: Duration,
    ) -> Result<TcpFlowStream> {
        if !self.capabilities.tcp_flow_stream_v1 {
            return Err(TransportError::FlowStreamUnavailable);
        }
        let Some(connector) = &self.tcp_flow_connector else {
            return Err(TransportError::FlowStreamUnavailable);
        };
        connector.open_raw(preface, timeout).await
    }

    /// Attach an independent reliable control lane. Transports use this for
    /// QUIC's second bidirectional stream after feature negotiation; legacy
    /// transports leave it unset and all messages keep using `stream_tx`.
    pub fn with_control_channel(
        mut self,
        control_tx: mpsc::Sender<PackedMessage>,
        control_rx: mpsc::Receiver<BinaryMessage>,
        writer: JoinHandle<()>,
        reader: JoinHandle<()>,
    ) -> Self {
        self.control_tx = Some(control_tx);
        self.control_rx = Some(control_rx);
        let inner = Arc::try_unwrap(self.keepalive)
            .unwrap_or_else(|_| panic!("with_control_channel called after split()"));
        self.keepalive = Arc::new(Keepalive {
            _writer: inner._writer,
            _reader: inner._reader,
            _control_writer: Some(writer),
            _control_reader: Some(reader),
            _datagram_writer: inner._datagram_writer,
            _datagram_reader: inner._datagram_reader,
            _tcp_flow_manager: inner._tcp_flow_manager,
        });
        self
    }

    /// Install the QUIC datagram inbound/outbound channels + background
    /// writer/reader tasks + a live MTU-size probe. The channel-backed writer
    /// serializes `conn.send_datagram` calls so multiple concurrent producers
    /// don't contend on quinn's internal connection lock.
    ///
    /// `datagram_mtu` is queried on every outbound `send()` — never cache
    /// the result here.
    pub fn with_datagram_channel(
        mut self,
        datagram_tx: DatagramSchedulerSender,
        datagram_rx: DropOldestReceiver<BinaryMessage>,
        datagram_mtu: DatagramMtuFn,
        datagram_buf_space: DatagramBufSpaceFn,
        writer: JoinHandle<()>,
        reader: JoinHandle<()>,
    ) -> Self {
        self.datagram_tx = Some(datagram_tx);
        self.datagram_rx = Some(datagram_rx);
        self.datagram_mtu = Some(datagram_mtu);
        self.datagram_buf_space = Some(datagram_buf_space);
        let inner = Arc::try_unwrap(self.keepalive)
            .unwrap_or_else(|_| panic!("with_datagram_channel called after split()"));
        self.keepalive = Arc::new(Keepalive {
            _writer: inner._writer,
            _reader: inner._reader,
            _control_writer: inner._control_writer,
            _control_reader: inner._control_reader,
            _datagram_writer: Some(writer),
            _datagram_reader: Some(reader),
            _tcp_flow_manager: inner._tcp_flow_manager,
        });
        self
    }

    pub(crate) fn with_tcp_flow_streams(
        mut self,
        connector: TcpFlowConnector,
        incoming_rx: mpsc::Receiver<TcpFlowIncoming>,
        manager: JoinHandle<()>,
    ) -> Self {
        self.tcp_flow_connector = Some(connector);
        self.tcp_flow_rx = Some(incoming_rx);
        let inner = Arc::try_unwrap(self.keepalive)
            .unwrap_or_else(|_| panic!("with_tcp_flow_streams called after split()"));
        self.keepalive = Arc::new(Keepalive {
            _writer: inner._writer,
            _reader: inner._reader,
            _control_writer: inner._control_writer,
            _control_reader: inner._control_reader,
            _datagram_writer: inner._datagram_writer,
            _datagram_reader: inner._datagram_reader,
            _tcp_flow_manager: Some(manager),
        });
        self
    }

    /// Shared `Arc<UdpRouteStats>` handle. The QUIC transport passes this
    /// into the dg_writer / dg_reader tasks so they can bump `datagram_write_*`
    /// and `datagram_recv_*` counters; the same handle is also cloned into
    /// every `SessionSender` at `split()` time for the producer-side
    /// accepted-to-scheduler / `stream_fallback` counters. A single read
    /// from any clone reflects all producers / writer / reader activity.
    pub fn stats_handle(&self) -> Arc<UdpRouteStats> {
        self.stats.clone()
    }

    pub fn current_max_datagram_size(&self) -> Option<usize> {
        self.datagram_mtu.as_ref().and_then(|f| f())
    }

    pub fn current_datagram_send_buffer_space(&self) -> Option<usize> {
        self.datagram_buf_space.as_ref().map(|f| f())
    }

    pub fn remove_datagram_association(&self, conn_id: &str) -> usize {
        self.datagram_tx
            .as_ref()
            .map(|dg_tx| dg_tx.remove_association(conn_id))
            .unwrap_or(0)
    }

    /// Install a transport-health probe. Mirrors the pattern of
    /// [`Session::with_datagram_channel`]'s `datagram_mtu` arg: the QUIC
    /// `wrap()` factory hands in a closure that reads live
    /// `quinn::Connection::stats()`, keeping `Session` itself
    /// transport-agnostic. Calling twice replaces the previous probe.
    pub fn install_stats_probe(&mut self, f: SessionStatsFn) {
        self.stats_probe = Some(f);
    }

    /// Current smoothed RTT, or `Duration::ZERO` when no probe is installed.
    pub fn rtt(&self) -> std::time::Duration {
        self.stats_probe
            .as_ref()
            .map(|f| f().rtt)
            .unwrap_or_default()
    }

    /// Current loss ratio in `[0.0, 1.0]`, or `0.0` when no probe is installed.
    pub fn loss_rate(&self) -> f64 {
        self.stats_probe
            .as_ref()
            .map(|f| f().loss_rate)
            .unwrap_or(0.0)
    }

    /// Current PTO backoff count, or `0` when no probe is installed (or the
    /// transport doesn't expose it — see [`SessionStats::pto_count`]).
    pub fn pto_count(&self) -> u32 {
        self.stats_probe
            .as_ref()
            .map(|f| f().pto_count)
            .unwrap_or(0)
    }

    /// Snapshot of all transport-health metrics in one call. Cheaper than
    /// hitting `rtt()` / `loss_rate()` / `pto_count()` separately because the
    /// underlying `quinn::Connection::stats()` is computed once.
    pub fn stats(&self) -> SessionStats {
        self.stats_probe.as_ref().map(|f| f()).unwrap_or_default()
    }

    pub fn queue_snapshot(&self) -> SessionQueueSnapshot {
        SessionQueueSnapshot {
            stream_queue_used: self
                .stream_tx
                .max_capacity()
                .saturating_sub(self.stream_tx.capacity()),
            stream_queue_capacity: self.stream_tx.max_capacity(),
            datagram_send_buffer_space: self.current_datagram_send_buffer_space(),
            datagram_send_buffer_capacity: datagram_send_buffer_capacity(&self.datagram_buf_space),
            udp_route_stats: self.stats.snapshot(),
        }
    }

    /// Send a message. Routes `UdpData` over QUIC datagrams when possible
    /// (fast path). If a datagram channel is negotiated and the UDP frame is
    /// larger than the live datagram MTU, it is counted and dropped instead of
    /// falling back to the reliable bidi stream. Non-UDP traffic still uses
    /// the reliable stream.
    pub async fn send(&self, msg: BinaryMessage) -> Result<()> {
        use std::sync::atomic::Ordering;
        if let BinaryMessage::Close { conn_id } = &msg {
            if let Some(dg_tx) = &self.datagram_tx {
                dg_tx.remove_association(conn_id);
            }
        }
        let is_udp = matches!(msg, BinaryMessage::UdpData { .. });
        let packed = pack(&msg);
        ensure_frame_len(packed.total_len())?;
        if is_udp {
            if let (Some(dg_tx), Some(mtu_fn)) = (&self.datagram_tx, &self.datagram_mtu) {
                if let Some(max_dg) = mtu_fn() {
                    if packed.total_len() <= max_dg {
                        let BinaryMessage::UdpData { conn_id, .. } = &msg else {
                            unreachable!();
                        };
                        match enqueue_datagram_frame(
                            dg_tx,
                            &self.stats,
                            conn_id.clone(),
                            packed,
                            None,
                        ) {
                            Ok(()) | Err(TrySendKind::Full) => return Ok(()),
                            Err(TrySendKind::Closed) => return Err(TransportError::Closed),
                            Err(TrySendKind::TooLarge(len)) => {
                                return Err(TransportError::FrameTooLarge(len));
                            }
                            Err(TrySendKind::DatagramUnavailable) => {
                                return Err(TransportError::DatagramUnavailable);
                            }
                        }
                    }
                    if let BinaryMessage::UdpData { conn_id, payload } = &msg {
                        match try_fragment_udp_to_datagrams(
                            dg_tx,
                            &self.stats,
                            &self.udp_frag_seq,
                            conn_id,
                            payload.clone(),
                            packed.total_len(),
                            max_dg,
                        ) {
                            Ok(()) | Err(TrySendKind::Full) => return Ok(()),
                            Err(TrySendKind::Closed) => return Err(TransportError::Closed),
                            Err(TrySendKind::DatagramUnavailable) => {
                                return Err(TransportError::DatagramUnavailable);
                            }
                            Err(TrySendKind::TooLarge(len)) => {
                                return Err(TransportError::FrameTooLarge(len));
                            }
                        }
                    }
                    self.record_oversized_udp_drop(packed.total_len(), max_dg);
                    return Ok(());
                }
            }
            if self.udp_data_mode == UdpDataMode::QuicDatagramRequired {
                return Err(TransportError::DatagramUnavailable);
            }
            self.stats.stream_fallback.fetch_add(1, Ordering::Relaxed);
        }
        let tx = if is_control_lane_message(&msg)
            || (self.capabilities.route_bind_control_v1 && is_route_bind_control_message(&msg))
        {
            self.control_tx.as_ref().unwrap_or(&self.stream_tx)
        } else {
            &self.stream_tx
        };
        tx.send(packed).await.map_err(|_| TransportError::Closed)
    }

    /// Non-blocking send variant. Mirrors [`SessionSender::try_send`] but on
    /// the un-split [`Session`] handle so the P2P [`crate::session::Session`]
    /// router (`MultiSenderRouter`) can keep an `Arc<Session>` per path
    /// instead of pre-splitting and losing access to [`Session::stats`].
    /// Routes `UdpData` to the datagram fast-path when the packed body fits
    /// the live `max_datagram_size`; UDP that is too large for the realtime
    /// datagram path is dropped with `TrySendKind::Full` instead of falling
    /// back to the reliable stream. Other traffic still uses the reliable
    /// stream.
    pub fn try_send(&self, msg: BinaryMessage) -> std::result::Result<(), TrySendKind> {
        use std::sync::atomic::Ordering;
        if let BinaryMessage::Close { conn_id } = &msg {
            if let Some(dg_tx) = &self.datagram_tx {
                dg_tx.remove_association(conn_id);
            }
        }
        let is_udp = matches!(msg, BinaryMessage::UdpData { .. });
        let packed = pack(&msg);
        let total_len = packed.total_len();
        if total_len as u64 > MAX_FRAME_LEN as u64 {
            return Err(TrySendKind::TooLarge(frame_len_for_error(total_len)));
        }
        if is_udp {
            if let (Some(dg_tx), Some(mtu_fn)) = (&self.datagram_tx, &self.datagram_mtu) {
                if let Some(max_dg) = mtu_fn() {
                    if packed.total_len() <= max_dg {
                        if dg_tx.is_closed() {
                            return Err(TrySendKind::Closed);
                        }
                        let BinaryMessage::UdpData { conn_id, .. } = &msg else {
                            unreachable!();
                        };
                        enqueue_datagram_frame(dg_tx, &self.stats, conn_id.clone(), packed, None)?;
                        return Ok(());
                    }
                    if let BinaryMessage::UdpData { conn_id, payload } = &msg {
                        return try_fragment_udp_to_datagrams(
                            dg_tx,
                            &self.stats,
                            &self.udp_frag_seq,
                            conn_id,
                            payload.clone(),
                            packed.total_len(),
                            max_dg,
                        );
                    }
                    self.record_oversized_udp_drop(packed.total_len(), max_dg);
                    return Err(TrySendKind::Full);
                }
            }
            if self.udp_data_mode == UdpDataMode::QuicDatagramRequired {
                return Err(TrySendKind::DatagramUnavailable);
            }
            self.stats.stream_fallback.fetch_add(1, Ordering::Relaxed);
        }
        let tx = if is_control_lane_message(&msg)
            || (self.capabilities.route_bind_control_v1 && is_route_bind_control_message(&msg))
        {
            self.control_tx.as_ref().unwrap_or(&self.stream_tx)
        } else {
            &self.stream_tx
        };
        match tx.try_send(packed) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                if is_udp {
                    self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendKind::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TrySendKind::Closed),
        }
    }

    /// Resolves when the underlying transport writer drops its receiver — i.e.,
    /// the underlying connection is torn down. Mirrors [`SessionSender::closed`]
    /// for callers that hold an `Arc<Session>` (e.g. P2P `MultiSenderRouter`)
    /// rather than a split [`SessionSender`].
    pub async fn closed(&self) {
        self.stream_tx.closed().await;
    }

    fn record_oversized_udp_drop(&self, packed_len: usize, max_dg: usize) {
        use std::sync::atomic::Ordering;
        self.stats
            .last_fallback_packed_len
            .store(packed_len, Ordering::Relaxed);
        self.stats
            .last_fallback_max_dg
            .store(max_dg, Ordering::Relaxed);
        self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
    }

    /// Receive the next stream (reliable) message. `None` means the
    /// transport reader has stopped.
    pub async fn recv(&mut self) -> Option<BinaryMessage> {
        self.stream_rx.recv().await
    }

    /// Receive the next datagram-delivered message (always `UdpData` in the
    /// current protocol). Returns `None` if datagrams aren't negotiated or
    /// the datagram reader has stopped.
    pub async fn recv_datagram(&mut self) -> Option<BinaryMessage> {
        let rx = self.datagram_rx.as_mut()?;
        rx.recv().await
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    pub fn close(&self) {
        (self.closer)();
    }

    /// Build a send-only `Session` shell from an existing
    /// [`SessionSender`]. The returned `Session` shares the underlying
    /// outbound channels (and `closer`/`stats`/`keepalive`) with the
    /// sender, so `send`/`try_send`/`closed` behave identically. The
    /// inbound side (`stream_rx`/`datagram_rx`) is replaced with a stub
    /// channel that yields no messages — calling `recv` / `recv_datagram`
    /// on the returned shell will block forever or return `None`.
    ///
    /// Intended for callers (e.g. `tp_client::p2p::session::MultiSession`)
    /// that want an `Arc<Session>` to drive outbound through after the
    /// real session has already been `split()` for separate reader tasks.
    pub fn send_only_from_sender(sender: SessionSender) -> Self {
        // Stub inbound channels — not driven by any transport task; their
        // receivers will only fire when their senders are dropped, which
        // happens here at construction (the senders are never stored).
        let (_stub_stream_tx, stub_stream_rx) = mpsc::channel::<BinaryMessage>(1);
        // Carry the original Session's stats probe through the
        // shell so `stats()`, `rtt()`, `loss_rate()`, `pto_count()` on the
        // shell reflect actual transport health. Pre-fix the shell
        // returned default zeros and the path scheduler picked paths
        // without real RTT/loss data.
        let stats_probe = sender.stats_probe.clone();
        Self {
            stream_tx: sender.stream_tx,
            control_tx: sender.control_tx,
            stream_rx: stub_stream_rx,
            control_rx: None,
            peer: sender.peer,
            closer: sender.closer,
            datagram_tx: sender.datagram_tx,
            datagram_rx: None,
            datagram_mtu: sender.datagram_mtu,
            datagram_buf_space: sender.datagram_buf_space,
            tcp_flow_connector: sender.tcp_flow_connector,
            tcp_flow_rx: None,
            stats_probe,
            stats: sender.stats,
            udp_frag_seq: sender.udp_frag_seq,
            udp_data_mode: sender.udp_data_mode,
            capabilities: sender.capabilities,
            keepalive: sender._keepalive,
        }
    }

    /// Split into independently-owned sender, stream-receiver, and optional
    /// datagram-receiver halves so the three paths can run on different
    /// tasks without a shared `tokio::select!` serializing them.
    ///
    /// The returned `DatagramReceiver` is `Some` iff the transport
    /// negotiated a datagram channel via
    /// [`Session::with_datagram_channel`].
    pub fn split(self) -> (SessionSender, SessionReceiver, Option<DatagramReceiver>) {
        let sender = SessionSender {
            stream_tx: self.stream_tx,
            control_tx: self.control_tx,
            datagram_tx: self.datagram_tx,
            datagram_mtu: self.datagram_mtu,
            datagram_buf_space: self.datagram_buf_space,
            tcp_flow_connector: self.tcp_flow_connector,
            closer: self.closer.clone(),
            peer: self.peer,
            _keepalive: self.keepalive.clone(),
            stats: self.stats,
            udp_frag_seq: self.udp_frag_seq,
            stats_probe: self.stats_probe.clone(),
            udp_data_mode: self.udp_data_mode,
            capabilities: self.capabilities,
        };
        let receiver = SessionReceiver {
            rx: self.stream_rx,
            control_rx: self.control_rx,
            tcp_flow_rx: self.tcp_flow_rx,
            closer: self.closer.clone(),
            peer: self.peer,
            _keepalive: self.keepalive.clone(),
        };
        let dg_receiver = self.datagram_rx.map(|rx| DatagramReceiver {
            rx,
            _keepalive: self.keepalive,
        });
        (sender, receiver, dg_receiver)
    }
}

/// Shared UDP routing counters. One instance per Session, cloned by all
/// SessionSender clones so a single process-wide snapshot reflects all
/// producers for that session. Used by the client-side engine's summary
/// log to answer "is video UDP actually riding QUIC datagrams, or is it
/// falling back onto the reliable stream where it head-of-line-blocks TCP?"
#[derive(Debug, Default)]
pub struct UdpRouteStats {
    /// UdpData packets accepted into scheduler queues before DRR/QUIC.
    pub datagram_accepted_to_scheduler: std::sync::atomic::AtomicU64,
    /// UdpData packets that fell back to the reliable stream because there
    /// was no datagram channel, or because an awaiting send path explicitly
    /// accepted reliable fallback.
    pub stream_fallback: std::sync::atomic::AtomicU64,
    /// UdpData packets dropped before the wire because the real-time path
    /// refused to queue them, including packets larger than the live
    /// datagram MTU.
    pub dropped_full: std::sync::atomic::AtomicU64,
    /// Packets evicted from the association whose bounded queue filled.
    pub datagram_per_association_evicted: std::sync::atomic::AtomicU64,
    /// Packets evicted by the global scheduler/QUIC-buffer budget guard.
    pub datagram_global_budget_evicted: std::sync::atomic::AtomicU64,
    /// Sampled `bytes.len()` (the packed-UdpData size) from the last UDP
    /// call that exceeded the current datagram MTU — gives operators a
    /// one-shot "look what size packets are overflowing" value without
    /// emitting a log per packet.
    pub last_fallback_packed_len: std::sync::atomic::AtomicUsize,
    /// Sampled `max_datagram_size()` value from that same call. Together
    /// with `last_fallback_packed_len`, answers "by how much does the
    /// UdpData exceed the datagram window right now?"
    pub last_fallback_max_dg: std::sync::atomic::AtomicUsize,

    // --- OUTBOUND datagram writer task counters (see `quic::wrap`) ---
    /// Times the dedicated datagram-writer task handed a packet to Quinn
    /// via `conn.send_datagram(bytes)`.
    pub datagram_write_ok: std::sync::atomic::AtomicU64,
    /// Times `conn.send_datagram` returned an error variant
    /// (`UnsupportedByPeer` / `Disabled` / `TooLarge` / `ConnectionLost`).
    /// quinn 0.11.9 does NOT surface buffer-full via an error — it silently
    /// evicts older datagrams instead — so this counter stays ~0 under
    /// buffer pressure. Use `datagram_send_buffer_space` (via
    /// `SessionSender::current_datagram_send_buffer_space`) to see that.
    pub datagram_write_err: std::sync::atomic::AtomicU64,
    /// Minimum `datagram_send_buffer_space()` observed by the writer since
    /// the last summary log. Stored as `space + 1`; 0 means "no sample yet".
    pub datagram_send_buffer_space_min_plus_one: std::sync::atomic::AtomicUsize,
    /// Count of writer samples where `datagram_send_buffer_space()` was
    /// exactly zero. Quinn may silently evict older datagrams in this state,
    /// so this exposes the hidden drop pressure as a rate, not only a min.
    pub datagram_send_buffer_space_zero_count: std::sync::atomic::AtomicU64,

    // --- INBOUND datagram reader task counters (see `quic::wrap`) ---
    /// Successful inbound datagrams: decoded + handed off to `dg_in_tx`.
    /// Compare with peer-side `udp_handed_to_quinn` and
    /// `udp_scheduler_accepted` to separate local eviction from tunnel loss.
    pub datagram_recv_ok: std::sync::atomic::AtomicU64,
    /// Inbound datagrams replaced by newer datagrams before the session-level
    /// consumer observed them.
    pub datagram_recv_dropped: std::sync::atomic::AtomicU64,
    /// dg_reader received bytes from quinn but `unpack()` failed. Should
    /// be 0 in normal operation — non-zero = protocol desync / a peer is
    /// emitting malformed datagrams.
    pub datagram_recv_decode_err: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UdpRouteStatsSnapshot {
    pub datagram_accepted_to_scheduler: u64,
    pub stream_fallback: u64,
    pub dropped_full: u64,
    pub datagram_per_association_evicted: u64,
    pub datagram_global_budget_evicted: u64,
    pub last_fallback_packed_len: usize,
    pub last_fallback_max_dg: usize,
    pub datagram_write_ok: u64,
    pub datagram_write_err: u64,
    pub datagram_send_buffer_space_min: Option<usize>,
    pub datagram_send_buffer_space_zero_count: u64,
    pub datagram_recv_ok: u64,
    pub datagram_recv_dropped: u64,
    pub datagram_recv_decode_err: u64,
}

impl UdpRouteStats {
    pub fn snapshot(&self) -> UdpRouteStatsSnapshot {
        use std::sync::atomic::Ordering;

        UdpRouteStatsSnapshot {
            datagram_accepted_to_scheduler: self
                .datagram_accepted_to_scheduler
                .load(Ordering::Relaxed),
            stream_fallback: self.stream_fallback.load(Ordering::Relaxed),
            dropped_full: self.dropped_full.load(Ordering::Relaxed),
            datagram_per_association_evicted: self
                .datagram_per_association_evicted
                .load(Ordering::Relaxed),
            datagram_global_budget_evicted: self
                .datagram_global_budget_evicted
                .load(Ordering::Relaxed),
            last_fallback_packed_len: self.last_fallback_packed_len.load(Ordering::Relaxed),
            last_fallback_max_dg: self.last_fallback_max_dg.load(Ordering::Relaxed),
            datagram_write_ok: self.datagram_write_ok.load(Ordering::Relaxed),
            datagram_write_err: self.datagram_write_err.load(Ordering::Relaxed),
            datagram_send_buffer_space_min: self
                .datagram_send_buffer_space_min_plus_one
                .load(Ordering::Relaxed)
                .checked_sub(1),
            datagram_send_buffer_space_zero_count: self
                .datagram_send_buffer_space_zero_count
                .load(Ordering::Relaxed),
            datagram_recv_ok: self.datagram_recv_ok.load(Ordering::Relaxed),
            datagram_recv_dropped: self.datagram_recv_dropped.load(Ordering::Relaxed),
            datagram_recv_decode_err: self.datagram_recv_decode_err.load(Ordering::Relaxed),
        }
    }

    pub fn record_datagram_send_buffer_space(&self, space: usize) {
        use std::sync::atomic::Ordering;

        if space == 0 {
            self.datagram_send_buffer_space_zero_count
                .fetch_add(1, Ordering::Relaxed);
        }
        let sample = space.saturating_add(1);
        let mut current = self
            .datagram_send_buffer_space_min_plus_one
            .load(Ordering::Relaxed);
        while current == 0 || sample < current {
            match self
                .datagram_send_buffer_space_min_plus_one
                .compare_exchange_weak(current, sample, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    pub fn take_datagram_send_buffer_space_min(&self) -> Option<usize> {
        use std::sync::atomic::Ordering;

        self.datagram_send_buffer_space_min_plus_one
            .swap(0, Ordering::Relaxed)
            .checked_sub(1)
    }

    pub fn take_datagram_send_buffer_space_zero_count(&self) -> u64 {
        use std::sync::atomic::Ordering;

        self.datagram_send_buffer_space_zero_count
            .swap(0, Ordering::Relaxed)
    }
}

/// Cloneable sender half of a split [`Session`]. Multiple producers may hold
/// clones and push concurrently; the underlying transport serializes writes
/// onto the QUIC stream / datagram queue.
#[derive(Clone)]
pub struct SessionSender {
    stream_tx: mpsc::Sender<PackedMessage>,
    control_tx: Option<mpsc::Sender<PackedMessage>>,
    datagram_tx: Option<DatagramSchedulerSender>,
    datagram_mtu: Option<DatagramMtuFn>,
    datagram_buf_space: Option<DatagramBufSpaceFn>,
    tcp_flow_connector: Option<TcpFlowConnector>,
    closer: Arc<dyn Fn() + Send + Sync + 'static>,
    peer: SocketAddr,
    _keepalive: Arc<Keepalive>,
    stats: Arc<UdpRouteStats>,
    udp_frag_seq: Arc<AtomicU32>,
    /// Live transport-stats probe cloned from the original
    /// `Session`. Pre-fix `split()` dropped the probe and
    /// `send_only_from_sender` reconstructed a probe-less `Session`,
    /// so `Session::stats()` on the shell returned defaults — the
    /// path scheduler then couldn't see real RTT/loss for path
    /// selection. Carrying the probe here lets shells expose true
    /// transport health.
    stats_probe: Option<SessionStatsFn>,
    udp_data_mode: UdpDataMode,
    capabilities: TransportCapabilities,
}

impl SessionSender {
    /// Send a message, routing `UdpData` to the datagram fast-path when the
    /// packed body fits the live `max_datagram_size`. Oversized UDP is counted
    /// and dropped instead of falling back to the reliable stream; stream
    /// writes still await capacity and back-pressure via the underlying mpsc.
    pub async fn send(&self, msg: BinaryMessage) -> Result<()> {
        use std::sync::atomic::Ordering;
        if let BinaryMessage::Close { conn_id } = &msg {
            if let Some(dg_tx) = &self.datagram_tx {
                dg_tx.remove_association(conn_id);
            }
        }
        let is_udp = matches!(msg, BinaryMessage::UdpData { .. });
        let packed = pack(&msg);
        ensure_frame_len(packed.total_len())?;
        if is_udp {
            if let (Some(dg_tx), Some(mtu_fn)) = (&self.datagram_tx, &self.datagram_mtu) {
                if let Some(max_dg) = mtu_fn() {
                    if packed.total_len() <= max_dg {
                        let BinaryMessage::UdpData { conn_id, .. } = &msg else {
                            unreachable!();
                        };
                        match enqueue_datagram_frame(
                            dg_tx,
                            &self.stats,
                            conn_id.clone(),
                            packed,
                            None,
                        ) {
                            Ok(()) | Err(TrySendKind::Full) => return Ok(()),
                            Err(TrySendKind::Closed) => return Err(TransportError::Closed),
                            Err(TrySendKind::TooLarge(len)) => {
                                return Err(TransportError::FrameTooLarge(len));
                            }
                            Err(TrySendKind::DatagramUnavailable) => {
                                return Err(TransportError::DatagramUnavailable);
                            }
                        }
                    }
                    if let BinaryMessage::UdpData { conn_id, payload } = &msg {
                        match try_fragment_udp_to_datagrams(
                            dg_tx,
                            &self.stats,
                            &self.udp_frag_seq,
                            conn_id,
                            payload.clone(),
                            packed.total_len(),
                            max_dg,
                        ) {
                            Ok(()) | Err(TrySendKind::Full) => return Ok(()),
                            Err(TrySendKind::Closed) => return Err(TransportError::Closed),
                            Err(TrySendKind::DatagramUnavailable) => {
                                return Err(TransportError::DatagramUnavailable);
                            }
                            Err(TrySendKind::TooLarge(len)) => {
                                return Err(TransportError::FrameTooLarge(len));
                            }
                        }
                    }
                    self.record_oversized_udp_drop(packed.total_len(), max_dg);
                    return Ok(());
                }
            }
            if self.udp_data_mode == UdpDataMode::QuicDatagramRequired {
                return Err(TransportError::DatagramUnavailable);
            }
            self.stats.stream_fallback.fetch_add(1, Ordering::Relaxed);
        }
        let tx = if is_control_lane_message(&msg)
            || (self.capabilities.route_bind_control_v1 && is_route_bind_control_message(&msg))
        {
            self.control_tx.as_ref().unwrap_or(&self.stream_tx)
        } else {
            &self.stream_tx
        };
        tx.send(packed).await.map_err(|_| TransportError::Closed)
    }

    /// Non-blocking variant used from `poll_*` contexts (e.g. `AsyncWrite`).
    /// Returns `TrySendKind::Full` when the destination queue is saturated
    /// or a realtime UDP packet is larger than the live datagram MTU,
    /// `TrySendKind::TooLarge` when the packed tunnel message exceeds the
    /// per-frame protocol limit, and `TrySendKind::Closed` after teardown.
    /// This is independent of QUIC reliable-stream capacity: oversized TCP
    /// flows should arrive as multiple smaller `Data` frames, while one jumbo
    /// message is rejected instead of queued. The caller may retry or drop
    /// `Full` according to message semantics (TCP: retry; UDP game-streaming:
    /// drop).
    pub fn try_send(&self, msg: BinaryMessage) -> std::result::Result<(), TrySendKind> {
        use std::sync::atomic::Ordering;
        if let BinaryMessage::Close { conn_id } = &msg {
            if let Some(dg_tx) = &self.datagram_tx {
                dg_tx.remove_association(conn_id);
            }
        }
        let is_udp = matches!(msg, BinaryMessage::UdpData { .. });
        let packed = pack(&msg);
        let total_len = packed.total_len();
        if total_len as u64 > MAX_FRAME_LEN as u64 {
            return Err(TrySendKind::TooLarge(frame_len_for_error(total_len)));
        }
        if is_udp {
            if let (Some(dg_tx), Some(mtu_fn)) = (&self.datagram_tx, &self.datagram_mtu) {
                if let Some(max_dg) = mtu_fn() {
                    if packed.total_len() <= max_dg {
                        if dg_tx.is_closed() {
                            return Err(TrySendKind::Closed);
                        }
                        let BinaryMessage::UdpData { conn_id, .. } = &msg else {
                            unreachable!();
                        };
                        enqueue_datagram_frame(dg_tx, &self.stats, conn_id.clone(), packed, None)?;
                        return Ok(());
                    }
                    if let BinaryMessage::UdpData { conn_id, payload } = &msg {
                        return try_fragment_udp_to_datagrams(
                            dg_tx,
                            &self.stats,
                            &self.udp_frag_seq,
                            conn_id,
                            payload.clone(),
                            packed.total_len(),
                            max_dg,
                        );
                    }
                    self.record_oversized_udp_drop(packed.total_len(), max_dg);
                    return Err(TrySendKind::Full);
                }
            }
            if self.udp_data_mode == UdpDataMode::QuicDatagramRequired {
                return Err(TrySendKind::DatagramUnavailable);
            }
            self.stats.stream_fallback.fetch_add(1, Ordering::Relaxed);
        }
        let tx = if is_control_lane_message(&msg)
            || (self.capabilities.route_bind_control_v1 && is_route_bind_control_message(&msg))
        {
            self.control_tx.as_ref().unwrap_or(&self.stream_tx)
        } else {
            &self.stream_tx
        };
        match tx.try_send(packed) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                if is_udp {
                    self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendKind::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TrySendKind::Closed),
        }
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    pub fn capabilities(&self) -> TransportCapabilities {
        self.capabilities
    }

    pub async fn open_tcp_flow_stream(
        &self,
        conn_id: String,
        address: String,
        timeout: Duration,
    ) -> Result<TcpFlowStream> {
        if !self.capabilities.tcp_flow_stream_v1 {
            return Err(TransportError::FlowStreamUnavailable);
        }
        let Some(connector) = &self.tcp_flow_connector else {
            return Err(TransportError::FlowStreamUnavailable);
        };
        connector
            .open(
                TcpFlowStreamPreface {
                    conn_id,
                    network: "tcp".into(),
                    address,
                },
                timeout,
            )
            .await
    }

    pub async fn open_raw_tcp_flow_stream(
        &self,
        preface: Bytes,
        timeout: Duration,
    ) -> Result<TcpFlowStream> {
        if !self.capabilities.tcp_flow_stream_v1 {
            return Err(TransportError::FlowStreamUnavailable);
        }
        let Some(connector) = &self.tcp_flow_connector else {
            return Err(TransportError::FlowStreamUnavailable);
        };
        connector.open_raw(preface, timeout).await
    }

    pub fn close(&self) {
        (self.closer)();
    }

    /// Clone of the underlying reliable-stream mpsc sender. Exposed so callers
    /// that need `AsyncWrite`-style backpressure (e.g. `TunneledConn::poll_write`
    /// in tp-gateway) can wrap it in a `tokio_util::sync::PollSender` and
    /// register a waker when the queue is full — `try_send` alone returns
    /// `TrySendKind::Full` without any wakeup path, which deadlocks
    /// `tokio::io::copy_bidirectional` once the outbound queue saturates.
    ///
    /// Channel carries `PackedMessage` (post-pack split form) now that the
    /// QUIC writer vectors header + payload separately — callers that pipe
    /// `BinaryMessage::Data` through this sender must `pack(&msg)` first
    /// and send the resulting `PackedMessage`.
    pub fn stream_mpsc(&self) -> mpsc::Sender<PackedMessage> {
        self.stream_tx.clone()
    }

    /// Current quinn-reported `max_datagram_size()` on this session's
    /// underlying QUIC connection. `None` if the transport has no datagram
    /// channel (e.g. websocket, or the handshake disabled datagrams).
    ///
    /// Changes over time as quinn's PMTUD grows (or shrinks) the path
    /// MTU estimate; callers should re-read rather than caching.
    pub fn current_max_datagram_size(&self) -> Option<usize> {
        self.datagram_mtu.as_ref().and_then(|f| f())
    }

    /// Current quinn-reported `datagram_send_buffer_space()` on this
    /// session's underlying QUIC connection. `None` if the transport has
    /// no datagram channel. When this value approaches zero on the sender
    /// side, quinn 0.11.9 silently drops older queued datagrams on each
    /// new `send_datagram` to free room — the hidden tunnel loss we're
    /// chasing. Callers that log this should compare it to the configured
    /// `datagram_send_buffer_size`.
    pub fn current_datagram_send_buffer_space(&self) -> Option<usize> {
        self.datagram_buf_space.as_ref().map(|f| f())
    }

    pub fn udp_data_mode(&self) -> UdpDataMode {
        self.udp_data_mode
    }

    pub fn udp_datagram_available(&self) -> bool {
        self.current_max_datagram_size().is_some()
    }

    /// Snapshot of transport health for the underlying session.
    pub fn stats(&self) -> SessionStats {
        self.stats_probe.as_ref().map(|f| f()).unwrap_or_default()
    }

    pub fn queue_snapshot(&self) -> SessionQueueSnapshot {
        SessionQueueSnapshot {
            stream_queue_used: self
                .stream_tx
                .max_capacity()
                .saturating_sub(self.stream_tx.capacity()),
            stream_queue_capacity: self.stream_tx.max_capacity(),
            datagram_send_buffer_space: self.current_datagram_send_buffer_space(),
            datagram_send_buffer_capacity: datagram_send_buffer_capacity(&self.datagram_buf_space),
            udp_route_stats: self.stats.snapshot(),
        }
    }

    /// Shared route-stats handle. Cloning is cheap (Arc). The same handle
    /// is populated by every `SessionSender` clone from this split, so a
    /// single read reflects session-wide UDP routing.
    pub fn udp_route_stats(&self) -> Arc<UdpRouteStats> {
        self.stats.clone()
    }

    /// Resolves when the transport writer drops its receiver — i.e., the
    /// underlying QUIC connection is torn down. Use as a `tokio::select!`
    /// branch in long-lived per-connection pipe tasks (pipe_tcp, pipe_udp)
    /// so they exit promptly when the session dies instead of hanging on
    /// idle reads/receives. Without this, a target-idle TCP or UDP flow
    /// would block forever because neither the local socket nor the
    /// tunnel-side mpsc would unblock on session death → orphaned tasks
    /// accumulate (TCP fd + 1024-slot mpsc + `Arc<Keepalive>`) across
    /// reconnect cycles, producing a slow memory growth.
    pub async fn closed(&self) {
        self.stream_tx.closed().await;
    }

    fn record_oversized_udp_drop(&self, packed_len: usize, max_dg: usize) {
        use std::sync::atomic::Ordering;
        self.stats
            .last_fallback_packed_len
            .store(packed_len, Ordering::Relaxed);
        self.stats
            .last_fallback_max_dg
            .store(max_dg, Ordering::Relaxed);
        self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
    }
}

/// Outcome of [`SessionSender::try_send`] when it cannot enqueue synchronously.
#[derive(Debug)]
pub enum TrySendKind {
    /// Destination queue is full — caller may retry.
    Full,
    /// Message exceeds the packed tunnel frame limit and was not queued.
    TooLarge(u32),
    /// UDP datagrams are required for this transport path but unavailable.
    DatagramUnavailable,
    /// Destination queue is closed (session torn down).
    Closed,
}

fn is_control_lane_message(msg: &BinaryMessage) -> bool {
    matches!(
        msg,
        BinaryMessage::Heartbeat { .. }
            | BinaryMessage::HeartbeatAck { .. }
            | BinaryMessage::P2pAnnounce { .. }
            | BinaryMessage::P2pAnnounceAck { .. }
            | BinaryMessage::P2pOffer { .. }
            | BinaryMessage::P2pAnswer { .. }
            | BinaryMessage::P2pOfferV2 { .. }
            | BinaryMessage::P2pAnswerV2 { .. }
            | BinaryMessage::EncryptedPeerControlV2 { .. }
            | BinaryMessage::P2pPunchSync { .. }
            | BinaryMessage::P2pProbe { .. }
            | BinaryMessage::P2pProbeAck { .. }
            | BinaryMessage::P2pSessionReady { .. }
            | BinaryMessage::P2pTeardown { .. }
            | BinaryMessage::P2pPeerHint { .. }
    )
}

fn is_route_bind_control_message(msg: &BinaryMessage) -> bool {
    matches!(
        msg,
        BinaryMessage::RelayRouteBind { .. } | BinaryMessage::RelayRouteBindAck { .. }
    )
}

fn ratio(used: usize, capacity: usize) -> Option<f64> {
    if capacity == 0 {
        None
    } else {
        Some(used as f64 / capacity as f64)
    }
}

fn ensure_frame_len(total_len: usize) -> Result<()> {
    if total_len as u64 > MAX_FRAME_LEN as u64 {
        return Err(TransportError::FrameTooLarge(frame_len_for_error(
            total_len,
        )));
    }
    Ok(())
}

fn frame_len_for_error(total_len: usize) -> u32 {
    u32::try_from(total_len).unwrap_or(u32::MAX)
}

/// Single-consumer receiver half for reliable (stream-delivered) messages.
pub struct SessionReceiver {
    rx: mpsc::Receiver<BinaryMessage>,
    control_rx: Option<mpsc::Receiver<BinaryMessage>>,
    tcp_flow_rx: Option<mpsc::Receiver<TcpFlowIncoming>>,
    closer: Arc<dyn Fn() + Send + Sync + 'static>,
    peer: SocketAddr,
    _keepalive: Arc<Keepalive>,
}

impl SessionReceiver {
    pub async fn recv(&mut self) -> Option<BinaryMessage> {
        if let Some(control_rx) = &mut self.control_rx {
            match control_rx.try_recv() {
                Ok(msg) => return Some(msg),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.control_rx = None;
                }
            }
        }
        if let Some(control_rx) = &mut self.control_rx {
            tokio::select! {
                biased;
                msg = control_rx.recv() => {
                    if msg.is_some() {
                        msg
                    } else {
                        self.control_rx = None;
                        self.rx.recv().await
                    }
                }
                msg = self.rx.recv() => msg,
            }
        } else {
            self.rx.recv().await
        }
    }

    pub async fn recv_data(&mut self) -> Option<BinaryMessage> {
        self.rx.recv().await
    }

    pub fn take_control_receiver(&mut self) -> Option<SessionControlReceiver> {
        self.control_rx.take().map(|rx| SessionControlReceiver {
            rx,
            _keepalive: self._keepalive.clone(),
        })
    }

    pub fn take_tcp_flow_receiver(&mut self) -> Option<TcpFlowIncomingReceiver> {
        self.tcp_flow_rx.take().map(|rx| TcpFlowIncomingReceiver {
            rx,
            _keepalive: self._keepalive.clone(),
        })
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    pub fn close(&self) {
        (self.closer)();
    }
}

/// Single-consumer receiver for reliable control-lane messages. When a
/// transport exposes one, callers should drain it from an independent task so
/// heartbeat acks and P2P signaling cannot sit behind bulk data handlers.
pub struct SessionControlReceiver {
    rx: mpsc::Receiver<BinaryMessage>,
    _keepalive: Arc<Keepalive>,
}

impl SessionControlReceiver {
    pub async fn recv(&mut self) -> Option<BinaryMessage> {
        self.rx.recv().await
    }
}

/// Single-consumer receiver half for QUIC datagram-delivered messages
/// (typically `UdpData`). Kept entirely separate from [`SessionReceiver`] so
/// a slow TCP consumer cannot backpressure UDP game-stream delivery.
pub struct DatagramReceiver {
    rx: DropOldestReceiver<BinaryMessage>,
    _keepalive: Arc<Keepalive>,
}

impl DatagramReceiver {
    pub async fn recv(&mut self) -> Option<BinaryMessage> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagram_scheduler::{
        datagram_scheduler_channel, DatagramSchedulerConfig, DatagramSchedulerReceiver,
        DatagramSchedulerSender,
    };
    use crate::drop_oldest::drop_oldest_channel;
    use bytes::Bytes;
    use tp_core::protocol::{
        pack_tcp_flow_stream_preface, unpack, unpack_tcp_flow_stream_preface, TcpFlowStreamPreface,
        TransportCapabilities,
    };

    #[test]
    fn header_auth_negotiates_route_bind_and_attestation_independently() {
        let attestation_only = negotiate_header_auth_capabilities(TransportCapabilities {
            route_bind_control_v1: false,
            tcp_flow_stream_v1: false,
            relay_source_attestation_v1: true,
            peer_mesh_v2: false,
        });
        assert!(!attestation_only.route_bind_control_v1);
        assert!(attestation_only.relay_source_attestation_v1);

        let exact_relay = negotiate_header_auth_capabilities(TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: false,
            relay_source_attestation_v1: true,
            peer_mesh_v2: false,
        });
        assert!(exact_relay.route_bind_control_v1);
        assert!(exact_relay.relay_source_attestation_v1);
        assert!(!exact_relay.tcp_flow_stream_v1);
        assert_eq!(header_auth_capability_mask(exact_relay), 0x05);
    }

    fn test_datagram_scheduler(cap: usize) -> (DatagramSchedulerSender, DatagramSchedulerReceiver) {
        datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: cap,
            global_packet_limit: cap,
            ..DatagramSchedulerConfig::for_test()
        })
    }

    async fn recv_scheduled_datagram(
        rx: &mut DatagramSchedulerReceiver,
        quantum: usize,
    ) -> PackedMessage {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.recv_with_quantum(quantum),
        )
        .await
        .expect("timed out waiting for datagram")
        .expect("datagram scheduler closed")
        .packed
    }

    /// `SessionSender::closed()` must resolve once the transport writer drops
    /// its receiver — that signal is what lets `pipe_tcp`/`pipe_udp` exit
    /// when the QUIC connection dies, instead of blocking on an idle socket
    /// forever. Regression guard for the slow RSS growth on the Tauri client
    /// (orphaned `pipe_tcp` tasks accumulating across reconnect cycles).
    #[tokio::test(flavor = "current_thread")]
    async fn closed_resolves_when_writer_drops_receiver() {
        // `out_rx` lives inside the writer task; when the writer task ends,
        // `out_rx` drops and `closed()` must fire.
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(16);

        let writer = tokio::spawn(async move {
            let _owned_receiver = out_rx;
            // Exit immediately → simulates quinn writer erroring on dead conn.
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        let (sender, _receiver, _dg) = session.split();

        tokio::time::timeout(std::time::Duration::from_secs(1), sender.closed())
            .await
            .expect("closed() must resolve promptly after writer drops its receiver");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_lane_routes_heartbeat_around_full_data_queue() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(1);
        out_tx
            .try_send(pack(&BinaryMessage::Data {
                conn_id: "bulk".into(),
                payload: Bytes::from_static(b"queued"),
            }))
            .expect("fill data queue");
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<PackedMessage>(1);
        let (_control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(1);

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_control_channel(
            control_tx,
            control_in_rx,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _receiver, _datagram) = session.split();

        sender
            .send(BinaryMessage::Heartbeat {
                client_id: "client-1".into(),
                timestamp: 7,
            })
            .await
            .expect("heartbeat should use independent reliable control lane");

        let control = control_rx.try_recv().expect("heartbeat on control lane");
        assert!(matches!(
            unpack(&control.to_bytes()).expect("decode heartbeat"),
            BinaryMessage::Heartbeat { timestamp: 7, .. }
        ));
        sender
            .send(BinaryMessage::P2pOfferV2 {
                source_peer_id: "peer-a".into(),
                target_peer_id: "peer-b".into(),
                signed_offer: Bytes::from_static(b"opaque"),
            })
            .await
            .expect("V2 signaling should use the reliable control lane");
        let control = control_rx.try_recv().expect("V2 offer on control lane");
        assert!(matches!(
            unpack(&control.to_bytes()).expect("decode V2 offer"),
            BinaryMessage::P2pOfferV2 { signed_offer, .. } if signed_offer.as_ref() == b"opaque"
        ));
        sender
            .send(BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id: "peer-b".into(),
                peerlink_session_id: [0x22; 16],
                conn_id: [0; 12],
                route_abort: false,
                sealed: Bytes::from_static(b"opaque-control"),
            })
            .await
            .expect("encrypted Peer control should use the reliable control lane");
        let control = control_rx
            .try_recv()
            .expect("encrypted Peer control on control lane");
        assert!(matches!(
            unpack(&control.to_bytes()).expect("decode encrypted Peer control"),
            BinaryMessage::EncryptedPeerControlV2 { sealed, .. }
                if sealed.as_ref() == b"opaque-control"
        ));
        let data = out_rx.try_recv().expect("original data frame remains");
        assert!(matches!(
            unpack(&data.to_bytes()).expect("decode data"),
            BinaryMessage::Data { .. }
        ));
        assert!(
            out_rx.try_recv().is_err(),
            "heartbeat must not be queued behind bulk data on the data lane"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn per_flow_tcp_stream_does_not_block_control_lane() {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(1);
        out_tx
            .try_send(pack(&BinaryMessage::Data {
                conn_id: "bulk".into(),
                payload: Bytes::from_static(b"queued"),
            }))
            .expect("fill data queue");
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<PackedMessage>(1);
        let (_control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(1);

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_control_channel(
            control_tx,
            control_in_rx,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_capabilities(TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: true,
            relay_source_attestation_v1: false,
            peer_mesh_v2: false,
        });
        let (sender, _receiver, _datagram) = session.split();

        sender
            .send(BinaryMessage::Heartbeat {
                client_id: "client-1".into(),
                timestamp: 7,
            })
            .await
            .expect("control lane must not wait behind data lane");

        assert!(matches!(
            unpack(
                &control_rx
                    .try_recv()
                    .expect("heartbeat on control")
                    .to_bytes()
            )
            .unwrap(),
            BinaryMessage::Heartbeat { timestamp: 7, .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tcp_flow_stream_preface_roundtrip() {
        let preface = TcpFlowStreamPreface {
            conn_id: "flow-1".into(),
            network: "tcp".into(),
            address: "127.0.0.1:443".into(),
        };
        let (mut client, mut server) = tokio::io::duplex(1024);

        write_tcp_flow_frame(&mut client, &pack_tcp_flow_stream_preface(&preface))
            .await
            .expect("write preface");
        let decoded = unpack_tcp_flow_stream_preface(
            &read_tcp_flow_frame(&mut server)
                .await
                .expect("read preface"),
        )
        .expect("decode preface");

        assert_eq!(decoded, preface);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_tcp_flow_open_forwards_opaque_v2_preface_without_connect_response() {
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let connector = TcpFlowConnector::new(request_tx);
        let opaque = Bytes::from_static(b"\x02flow-v2-0001opaque-peerlink-and-open");

        let opener = tokio::spawn({
            let opaque = opaque.clone();
            async move { connector.open_raw(opaque, Duration::from_secs(1)).await }
        });
        let request = request_rx.recv().await.expect("raw open request");
        assert_eq!(request.open, TcpFlowOpen::Raw(opaque));
        let preface = TcpFlowStreamPreface {
            conn_id: "flow-v2-0001".into(),
            network: "tcp".into(),
            address: String::new(),
        };
        let (io, _peer) = tokio::io::duplex(64);
        request
            .response
            .send(Ok(TcpFlowStream::new(preface, Box::pin(io))))
            .unwrap_or_else(|_| panic!("return raw stream"));

        assert_eq!(
            opener
                .await
                .expect("opener task")
                .expect("raw stream")
                .conn_id(),
            "flow-v2-0001"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connect_response_data_and_close_stay_on_data_lane_with_control() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<PackedMessage>(8);
        let (_control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(8);

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_control_channel(
            control_tx,
            control_in_rx,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _receiver, _datagram) = session.split();

        for msg in [
            BinaryMessage::Connect {
                conn_id: "conn-data-lane".into(),
                network: "tcp".into(),
                address: "127.0.0.1:80".into(),
            },
            BinaryMessage::ConnectResponse {
                conn_id: "conn-data-lane".into(),
                success: true,
                error: String::new(),
            },
            BinaryMessage::Close {
                conn_id: "conn-data-lane".into(),
            },
        ] {
            sender
                .send(msg)
                .await
                .expect("send data-lane control frame");
        }

        assert!(
            control_rx.try_recv().is_err(),
            "flow-open and close frames must not use the control lane before per-flow streams"
        );
        assert!(matches!(
            unpack(&out_rx.try_recv().expect("connect").to_bytes()).expect("decode"),
            BinaryMessage::Connect { .. }
        ));
        assert!(matches!(
            unpack(&out_rx.try_recv().expect("connect response").to_bytes()).expect("decode"),
            BinaryMessage::ConnectResponse { .. }
        ));
        assert!(matches!(
            unpack(&out_rx.try_recv().expect("close").to_bytes()).expect("decode"),
            BinaryMessage::Close { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn relay_route_bind_uses_control_lane_when_available() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<PackedMessage>(8);
        let (_control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(1);

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_control_channel(
            control_tx,
            control_in_rx,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_capabilities(TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: false,
            relay_source_attestation_v1: false,
            peer_mesh_v2: false,
        });
        let (sender, _receiver, _datagram) = session.split();

        sender
            .send(BinaryMessage::RelayRouteBind {
                conn_id: "route-bind-1".into(),
                peer_client_id: "pc-main".into(),
            })
            .await
            .expect("send route bind");
        sender
            .send(BinaryMessage::RelayRouteBindAck {
                conn_id: "route-bind-1".into(),
                success: true,
                error: String::new(),
            })
            .await
            .expect("send route bind ack");

        assert!(out_rx.try_recv().is_err(), "route bind frames use control");
        assert!(matches!(
            unpack(&control_rx.try_recv().expect("bind on control").to_bytes()).unwrap(),
            BinaryMessage::RelayRouteBind { .. }
        ));
        assert!(matches!(
            unpack(&control_rx.try_recv().expect("ack on control").to_bytes()).unwrap(),
            BinaryMessage::RelayRouteBindAck { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn relay_route_bind_uses_main_stream_without_a_separate_control_lane() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_capabilities(TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: false,
            relay_source_attestation_v1: true,
            peer_mesh_v2: false,
        });
        let (sender, _receiver, _datagram) = session.split();

        sender
            .send(BinaryMessage::RelayRouteBind {
                conn_id: "route-bind-main".into(),
                peer_client_id: "pc-main".into(),
            })
            .await
            .expect("send route bind");
        sender
            .send(BinaryMessage::RelayRouteBindAck {
                conn_id: "route-bind-main".into(),
                success: true,
                error: String::new(),
            })
            .await
            .expect("send route bind ack");

        assert!(matches!(
            unpack(&out_rx.try_recv().expect("bind on main stream").to_bytes()).unwrap(),
            BinaryMessage::RelayRouteBind { .. }
        ));
        assert!(matches!(
            unpack(&out_rx.try_recv().expect("ack on main stream").to_bytes()).unwrap(),
            BinaryMessage::RelayRouteBindAck { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inbound_control_receiver_can_bypass_queued_data_frames() {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(1);
        let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let (control_tx, _control_out_rx) = mpsc::channel::<PackedMessage>(1);
        let (control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(8);

        in_tx
            .send(BinaryMessage::Data {
                conn_id: "bulk-1".into(),
                payload: Bytes::from_static(b"queued"),
            })
            .await
            .expect("queue bulk data");
        control_in_tx
            .send(BinaryMessage::HeartbeatAck { timestamp: 99 })
            .await
            .expect("queue control ack");

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_control_channel(
            control_tx,
            control_in_rx,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (_sender, mut receiver, _datagram) = session.split();
        let mut control_receiver = receiver
            .take_control_receiver()
            .expect("control receiver must be available");

        assert!(matches!(
            control_receiver.recv().await.expect("control msg"),
            BinaryMessage::HeartbeatAck { timestamp: 99 }
        ));
        assert!(matches!(
            receiver.recv_data().await.expect("data msg"),
            BinaryMessage::Data { .. }
        ));
    }

    /// Complement: while the writer is alive, `closed()` must NOT resolve —
    /// otherwise pipe tasks would exit spuriously on every poll.
    #[tokio::test(flavor = "current_thread")]
    async fn closed_pending_while_writer_alive() {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(16);

        let writer = tokio::spawn(async move {
            let _owned_receiver = out_rx;
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        let (sender, _receiver, _dg) = session.split();

        let res = tokio::time::timeout(std::time::Duration::from_millis(50), sender.closed()).await;
        assert!(
            res.is_err(),
            "closed() must stay pending while writer still holds its receiver"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_udp_try_send_drops_instead_of_stream_fallback() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (captured_tx, mut captured_rx) = mpsc::channel::<PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            if let Some(msg) = out_rx.recv().await {
                let _ = captured_tx.send(msg).await;
            }
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let (dg_tx, mut dg_out_rx) = test_datagram_scheduler(4);
        let (_dg_in_tx, dg_in_rx) = drop_oldest_channel::<BinaryMessage>(4);
        let dg_writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let dg_reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
            .with_datagram_channel(
                dg_tx,
                dg_in_rx,
                Arc::new(|| Some(16)),
                Arc::new(|| 1024),
                dg_writer,
                dg_reader,
            );
        let msg = BinaryMessage::UdpData {
            conn_id: "moonlight-udp".into(),
            payload: Bytes::from_static(&[7; 64]),
        };
        let packed_len = pack(&msg).total_len();
        assert!(packed_len > 16);

        let err = session
            .try_send(msg)
            .expect_err("oversized realtime UDP should be dropped, not streamed");
        assert!(matches!(err, TrySendKind::Full));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), captured_rx.recv())
                .await
                .is_err(),
            "oversized UDP must not enter the reliable stream"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                dg_out_rx.recv_with_quantum(16)
            )
            .await
            .is_err(),
            "oversized UDP must not enter the datagram scheduler"
        );

        let stats = session.stats_handle();
        assert_eq!(
            stats
                .stream_fallback
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            stats
                .dropped_full
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            stats
                .datagram_accepted_to_scheduler
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            stats
                .last_fallback_packed_len
                .load(std::sync::atomic::Ordering::Relaxed),
            packed_len
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_data_without_quic_datagram_does_not_use_reliable_stream() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_udp_data_mode(UdpDataMode::QuicDatagramRequired);

        let err = session
            .try_send(BinaryMessage::UdpData {
                conn_id: "udp-no-dg".into(),
                payload: Bytes::from_static(b"packet"),
            })
            .expect_err("QUIC UdpData without datagrams must fail explicitly");

        assert!(matches!(err, TrySendKind::DatagramUnavailable));
        assert!(matches!(
            out_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(
            session
                .stats_handle()
                .stream_fallback
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "QUIC UdpData must not fall back to the reliable stream"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_data_datagram_unavailable_returns_explicit_error() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_udp_data_mode(UdpDataMode::QuicDatagramRequired);
        let (sender, _receiver, _datagram) = session.split();

        let err = sender
            .send(BinaryMessage::UdpData {
                conn_id: "udp-no-dg".into(),
                payload: Bytes::from_static(b"packet"),
            })
            .await
            .expect_err("QUIC UdpData without datagrams must fail explicitly");

        assert!(matches!(err, TransportError::DatagramUnavailable));
        assert!(matches!(
            out_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_udp_async_send_drops_instead_of_stream_fallback() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (captured_tx, mut captured_rx) = mpsc::channel::<PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            if let Some(msg) = out_rx.recv().await {
                let _ = captured_tx.send(msg).await;
            }
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let (dg_tx, mut dg_out_rx) = test_datagram_scheduler(4);
        let (_dg_in_tx, dg_in_rx) = drop_oldest_channel::<BinaryMessage>(4);
        let dg_writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let dg_reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
            .with_datagram_channel(
                dg_tx,
                dg_in_rx,
                Arc::new(|| Some(16)),
                Arc::new(|| 1024),
                dg_writer,
                dg_reader,
            );
        let msg = BinaryMessage::UdpData {
            conn_id: "moonlight-udp".into(),
            payload: Bytes::from_static(&[9; 64]),
        };
        let packed_len = pack(&msg).total_len();
        assert!(packed_len > 16);

        session
            .send(msg)
            .await
            .expect("async realtime UDP oversize drop is not a tunnel-close error");

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), captured_rx.recv())
                .await
                .is_err(),
            "oversized async UDP must not enter the reliable stream"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                dg_out_rx.recv_with_quantum(16)
            )
            .await
            .is_err(),
            "oversized async UDP must not enter the datagram scheduler"
        );

        let stats = session.stats_handle();
        assert_eq!(
            stats
                .stream_fallback
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            stats
                .dropped_full
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            stats
                .last_fallback_packed_len
                .load(std::sync::atomic::Ordering::Relaxed),
            packed_len
        );
    }

    #[test]
    fn datagram_buffer_zero_count_is_interval_counter() {
        let stats = UdpRouteStats::default();

        stats.record_datagram_send_buffer_space(1024);
        stats.record_datagram_send_buffer_space(0);
        stats.record_datagram_send_buffer_space(512);
        stats.record_datagram_send_buffer_space(0);

        assert_eq!(stats.take_datagram_send_buffer_space_min(), Some(0));
        assert_eq!(stats.take_datagram_send_buffer_space_zero_count(), 2);
        assert_eq!(stats.take_datagram_send_buffer_space_min(), None);
        assert_eq!(stats.take_datagram_send_buffer_space_zero_count(), 0);

        let datagram_probe: Option<DatagramBufSpaceFn> = Some(Arc::new(|| 1024));
        assert_eq!(
            datagram_send_buffer_capacity(&datagram_probe),
            Some(QUIC_DATAGRAM_BUFFER_BYTES)
        );
        assert_eq!(datagram_send_buffer_capacity(&None), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_send_rejects_oversized_frame_before_queueing() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        let (sender, _receiver, _dg) = session.split();

        let err = sender
            .send(BinaryMessage::Data {
                conn_id: "abcdefghijkl".into(),
                payload: Bytes::from(vec![0x33; crate::MAX_FRAME_LEN as usize]),
            })
            .await
            .expect_err("oversized messages must be rejected before the transport queue");

        assert!(matches!(err, TransportError::FrameTooLarge(_)));
        assert!(matches!(
            out_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_send_rejects_oversized_frame_before_queueing() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        let (sender, _receiver, _dg) = session.split();

        let result = sender.try_send(BinaryMessage::Data {
            conn_id: "abcdefghijkl".into(),
            payload: Bytes::from(vec![0x33; crate::MAX_FRAME_LEN as usize]),
        });

        assert!(matches!(result, Err(TrySendKind::TooLarge(_))));
        assert!(matches!(
            out_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn session_stats_default_is_all_zero() {
        let s = SessionStats::default();
        assert_eq!(s.rtt, std::time::Duration::ZERO);
        assert_eq!(s.loss_rate, 0.0);
        assert_eq!(s.pto_count, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_health_accessors_default_when_no_probe_installed() {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);

        assert_eq!(session.rtt(), std::time::Duration::ZERO);
        assert_eq!(session.loss_rate(), 0.0);
        assert_eq!(session.pto_count(), 0);
        let snap = session.stats();
        assert_eq!(snap.rtt, std::time::Duration::ZERO);
        assert_eq!(snap.loss_rate, 0.0);
        assert_eq!(snap.pto_count, 0);
        assert_eq!(session.peer_addr(), peer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_health_accessors_dispatch_to_installed_probe() {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let mut session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);

        session.install_stats_probe(Arc::new(|| SessionStats {
            rtt: std::time::Duration::from_millis(42),
            loss_rate: 0.125,
            pto_count: 2,
        }));

        assert_eq!(session.rtt(), std::time::Duration::from_millis(42));
        assert!((session.loss_rate() - 0.125).abs() < f64::EPSILON);
        assert_eq!(session.pto_count(), 2);
        let snap = session.stats();
        assert_eq!(snap.rtt, std::time::Duration::from_millis(42));
        assert!((snap.loss_rate - 0.125).abs() < f64::EPSILON);
        assert_eq!(snap.pto_count, 2);
    }

    /// A `Session` shell built via `send_only_from_sender` must
    /// expose the original session's stats via the probe — pre-fix the
    /// shell hardcoded `stats_probe: None`, so the path scheduler picked
    /// paths against zeroed `SessionStats` for any shell.
    #[tokio::test(flavor = "current_thread")]
    async fn send_only_from_sender_carries_stats_probe() {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let mut original = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);

        original.install_stats_probe(Arc::new(|| SessionStats {
            rtt: std::time::Duration::from_millis(33),
            loss_rate: 0.05,
            pto_count: 1,
        }));

        let (sender, _rx, _dg) = original.split();
        let shell = Session::send_only_from_sender(sender);

        assert_eq!(shell.rtt(), std::time::Duration::from_millis(33));
        assert!((shell.loss_rate() - 0.05).abs() < f64::EPSILON);
        assert_eq!(shell.pto_count(), 1);
        let snap = shell.stats();
        assert_eq!(snap.rtt, std::time::Duration::from_millis(33));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_data_uses_datagram_up_to_runtime_max_and_fragments_after() {
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(4);

        let writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let (dg_tx, mut dg_out_rx) = test_datagram_scheduler(4);
        let (_dg_in_tx, dg_in_rx) = drop_oldest_channel::<BinaryMessage>(4);
        let dg_writer = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        let dg_reader = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });

        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
            .with_datagram_channel(
                dg_tx,
                dg_in_rx,
                Arc::new(|| Some(1414)),
                Arc::new(|| 1024),
                dg_writer,
                dg_reader,
            );
        let (sender, _receiver, _dg) = session.split();

        for payload_len in [1375, 1385, 1400] {
            sender
                .try_send(BinaryMessage::UdpData {
                    conn_id: "abcdefghijkl".into(),
                    payload: Bytes::from(vec![0x42; payload_len]),
                })
                .expect("payload should route to datagram queue");
            let routed = recv_scheduled_datagram(&mut dg_out_rx, 1414).await;
            assert_eq!(routed.total_len(), payload_len + 14);
        }

        let fragmented_payload = vec![0x42; 1425];
        sender
            .try_send(BinaryMessage::UdpData {
                conn_id: "abcdefghijkl".into(),
                payload: Bytes::from(fragmented_payload.clone()),
            })
            .expect("oversized realtime UDP should fragment onto datagrams");

        let first = recv_scheduled_datagram(&mut dg_out_rx, 1414).await;
        let second = recv_scheduled_datagram(&mut dg_out_rx, 1414).await;
        assert_eq!(first.total_len(), 733);
        assert_eq!(second.total_len(), 732);
        let first_payload = match tp_core::protocol::unpack(&first.to_bytes()).unwrap() {
            BinaryMessage::UdpFragment {
                conn_id,
                frag_index,
                frag_total,
                payload,
                ..
            } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert_eq!(frag_index, 0);
                assert_eq!(frag_total, 2);
                payload
            }
            other => panic!("expected first UdpFragment, got {other:?}"),
        };
        let second_payload = match tp_core::protocol::unpack(&second.to_bytes()).unwrap() {
            BinaryMessage::UdpFragment {
                conn_id,
                frag_index,
                frag_total,
                payload,
                ..
            } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert_eq!(frag_index, 1);
                assert_eq!(frag_total, 2);
                payload
            }
            other => panic!("expected second UdpFragment, got {other:?}"),
        };
        let mut reassembled = Vec::with_capacity(fragmented_payload.len());
        reassembled.extend_from_slice(&first_payload);
        reassembled.extend_from_slice(&second_payload);
        assert_eq!(reassembled, fragmented_payload);

        sender
            .try_send(BinaryMessage::UdpData {
                conn_id: "abcdefghijkl".into(),
                payload: Bytes::from(vec![0x42; 2791]),
            })
            .expect("three-fragment payload should stay on the datagram path");
        let balanced_sizes = [
            recv_scheduled_datagram(&mut dg_out_rx, 1414)
                .await
                .total_len(),
            recv_scheduled_datagram(&mut dg_out_rx, 1414)
                .await
                .total_len(),
            recv_scheduled_datagram(&mut dg_out_rx, 1414)
                .await
                .total_len(),
        ];
        assert_eq!(balanced_sizes, [951, 950, 950]);

        assert!(matches!(
            out_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                dg_out_rx.recv_with_quantum(1414)
            )
            .await
            .is_err(),
            "only two fragments should be queued"
        );
    }

    #[tokio::test]
    async fn bytes_mut_flow_frame_reader_reuses_caller_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        tokio::spawn(async move {
            write_tcp_flow_frame(&mut writer, b"first")
                .await
                .expect("write first frame");
            write_tcp_flow_frame(&mut writer, b"second")
                .await
                .expect("write second frame");
        });
        let mut record = bytes::BytesMut::with_capacity(64);
        let allocation = record.as_ptr();

        read_tcp_flow_frame_into_bytes(&mut reader, &mut record)
            .await
            .expect("read first frame");
        assert_eq!(record, b"first".as_slice());
        assert_eq!(record.as_ptr(), allocation);

        read_tcp_flow_frame_into_bytes(&mut reader, &mut record)
            .await
            .expect("read second frame");
        assert_eq!(record, b"second".as_slice());
        assert_eq!(record.as_ptr(), allocation);
    }

    #[tokio::test]
    async fn bytes_mut_flow_frame_reader_does_not_zero_fill_before_socket_read() {
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::task::{Context, Poll};
        use tokio::io::{AsyncRead, ReadBuf};

        struct InitializedProbe {
            wire: &'static [u8],
            pos: usize,
            max_chunk: usize,
            payload_initialized: Arc<AtomicUsize>,
        }

        impl AsyncRead for InitializedProbe {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if self.pos >= 4 && self.pos < self.wire.len() {
                    self.payload_initialized
                        .store(buf.initialized().len(), Ordering::Relaxed);
                }
                let available = &self.wire[self.pos..];
                let n = available.len().min(buf.remaining()).min(self.max_chunk);
                buf.put_slice(&available[..n]);
                self.pos += n;
                Poll::Ready(Ok(()))
            }
        }

        static WIRE: &[u8] = b"\0\0\0\x05hello";
        let payload_initialized = Arc::new(AtomicUsize::new(usize::MAX));
        let mut reader = InitializedProbe {
            wire: WIRE,
            pos: 0,
            max_chunk: 2,
            payload_initialized: payload_initialized.clone(),
        };
        let mut record = BytesMut::with_capacity(64);

        read_tcp_flow_frame_into_bytes(&mut reader, &mut record)
            .await
            .expect("read probed frame");

        assert_eq!(record, b"hello".as_slice());
        assert_eq!(
            payload_initialized.load(Ordering::Relaxed),
            0,
            "payload storage must not be zero-filled before the transport overwrites it"
        );
    }
}
