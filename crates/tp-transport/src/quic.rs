//! QUIC server and client. A `Session` wraps a single bidirectional QUIC stream
//! and exposes message-oriented send/recv over `BinaryMessage`.
//!
//! Frame format on the QUIC stream: `[len:u32 BE][packed BinaryMessage bytes]`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use quinn::congestion::{BbrConfig, CubicConfig, NewRenoConfig};
use quinn::{
    ClientConfig, Endpoint, EndpointConfig, IdleTimeout, MtuDiscoveryConfig, RecvStream,
    SendStream, ServerConfig, TransportConfig, VarInt,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tp_core::config::ClientRoleConfig;
use tp_core::protocol::{
    pack, pack_tcp_flow_stream_preface, tcp_flow_open_route, unpack, unpack_bytes,
    unpack_tcp_flow_stream_preface, BinaryMessage, PackedMessage, TcpFlowStreamPreface,
    TransportCapabilities, AUTH_STATUS_FAILED, AUTH_STATUS_SUCCESS, TCP_FLOW_OPEN_V2_VERSION,
};

use crate::datagram_scheduler::{
    datagram_scheduler_channel, DatagramSchedulerConfig, DEFAULT_DATAGRAM_DRR_QUANTUM,
};
use crate::drop_oldest::drop_oldest_channel;
use crate::session::{
    read_tcp_flow_frame, write_tcp_flow_frame, DatagramBufSpaceFn, DatagramMtuFn, Session,
    SessionStats, SessionStatsFn, TcpFlowConnector, TcpFlowIncoming, TcpFlowOpen,
    TcpFlowOpenRequest, TcpFlowStream, UdpDataMode,
};
use crate::{Result, TransportError, MAX_FRAME_LEN};

/// Parameters surfaced to the server's auth handler when a client connects.
#[derive(Debug, Clone)]
pub struct AuthParams {
    pub tunnel_id: String,
    pub client_id: String,
    pub group_id: String,
    pub username: String,
    pub password: String,
    pub group_password: String,
    pub role: ClientRoleConfig,
    pub capabilities: TransportCapabilities,
    pub peer_addr: SocketAddr,
}

/// Server-side auth hook. Return Ok(()) to accept; Err(reason) to reject.
#[async_trait]
pub trait AuthHandler: Send + Sync + 'static {
    async fn authenticate(&self, params: &AuthParams) -> std::result::Result<(), String>;
}

/// Runtime-tunable QUIC transport knobs, passed into `QuicServer::bind` and
/// `QuicClient::new`. Mirrors the `gateway.transport.*` config section.
#[derive(Debug, Clone)]
pub struct QuicTuning {
    pub congestion: String,
    /// Explicit congestion-controller initial window. `None` preserves the
    /// selected controller's own default.
    pub initial_congestion_window_bytes: Option<u64>,
    pub keep_alive_secs: u32,
    pub max_idle_secs: u32,
    pub initial_mtu: u16,
    pub min_mtu: u16,
    pub mtu_upper_bound: u16,
    pub black_hole_cooldown_secs: u32,
}

impl Default for QuicTuning {
    fn default() -> Self {
        // 60 s idle + 10 s keepalive: on aliyun→home-PC paths, brief bursty
        // 4K video can coincide with a scheduler hiccup on either side and
        // stretch the keepalive RTT beyond 15 s. File-management traffic can
        // also spend long periods in one high-throughput stream where control
        // frames are delayed behind bulk data. 60 s is still short enough to
        // reap genuinely dead sessions while absorbing transient stalls.
        Self {
            congestion: "bbr".into(),
            initial_congestion_window_bytes: Some(QUIC_INITIAL_CONGESTION_WINDOW_BYTES),
            keep_alive_secs: 10,
            max_idle_secs: 60,
            initial_mtu: QUIC_SAFE_INITIAL_MTU_BYTES,
            min_mtu: QUIC_SAFE_INITIAL_MTU_BYTES,
            mtu_upper_bound: QUIC_UDP_PAYLOAD_CEILING_BYTES,
            black_hole_cooldown_secs: 60,
        }
    }
}

impl QuicTuning {
    /// Profile for tunnel/game-stream traffic. It keeps Quinn's safe 1200-byte
    /// recovery floor, but starts at an IPv6-safe Ethernet datagram budget so
    /// common 1385-byte Moonlight/Sunshine UDP payloads fit in one QUIC
    /// datagram after the tunnel header. If the path really cannot carry that
    /// size, Quinn can still black-hole recover to the safe floor and retry
    /// MTUD shortly after.
    pub fn game_streaming() -> Self {
        Self {
            initial_mtu: QUIC_UDP_PAYLOAD_CEILING_BYTES,
            black_hole_cooldown_secs: 5,
            ..Self::default()
        }
    }
}

/// UDP kernel receive-buffer target for the endpoint socket. Moonlight/Sunshine
/// 4K streams push ~500 Mbps / ~50 kpps; Linux default `SO_RCVBUF` is a couple
/// hundred KB, which evaporates in a single scheduler hiccup. Match Go's
/// gateway 64 MB SOCKS5 UDP listener target on RX (kernel may clip via
/// `net.core.rmem_max`; we log + continue on clip).
pub const UDP_SOCKET_RECV_BUF_BYTES: usize = 64 * 1024 * 1024;

/// Keep the endpoint send buffer around one high-bitrate WAN BDP, not tens of
/// megabytes. A giant UDP send buffer hides congestion in the kernel and turns
/// overload into Moonlight latency instead of near-source drop-oldest loss.
pub const UDP_SOCKET_SEND_BUF_BYTES: usize = 4 * 1024 * 1024;

/// QUIC datagrams are real-time traffic here. Keep enough packet budget in
/// Quinn to absorb Moonlight/Sunshine burst pacing and short scheduler stalls,
/// while still bounding stale-frame latency.
pub const QUIC_DATAGRAM_BUFFER_BYTES: usize = 48 * 1024 * 1024;
const QUIC_INITIAL_CONGESTION_WINDOW_BYTES: u64 = 4 * 1024 * 1024;
const QUIC_DATAGRAM_CHANNEL_CAP: usize = 4096;
const QUIC_DATAGRAM_SEND_SPACE_RECHECKS: usize = 8;
const QUIC_CONTROL_LANE_V1: &str = "quic-control-lane-v1";
const QUIC_CONTROL_LANE_ACCEPT_TIMEOUT: Duration = Duration::from_millis(100);
const TCP_FLOW_OPEN_CHANNEL_CAP: usize = 256;
const TCP_FLOW_INCOMING_CHANNEL_CAP: usize = 1024;
const TCP_FLOW_PREFACE_TIMEOUT: Duration = Duration::from_secs(3);
const QUIC_SAFE_INITIAL_MTU_BYTES: u16 = 1200;

/// QUIC UDP payload ceiling used by the tunnel endpoint.
///
/// Quinn's `max_udp_payload_size` excludes IP/UDP headers. Use the IPv6-safe
/// Ethernet payload ceiling (1500 - 40 IP - 8 UDP = 1452) as both the normal
/// ceiling and game-streaming profile target. Moonlight/Sunshine commonly emits
/// ~1385-byte UDP payloads; after the tunnel header they need a QUIC datagram
/// budget just under this value. Letting congestion-triggered black-hole
/// detection linger at Quinn's 1200-byte floor forces every video packet
/// through tunnel fragmentation, doubling packet rate and causing latency/loss.
const QUIC_UDP_PAYLOAD_CEILING_BYTES: u16 = 1452;
const UDP_FRAGMENT_REASSEMBLY_TTL: Duration = Duration::from_secs(2);
const UDP_FRAGMENT_REASSEMBLY_MAX_ENTRIES: usize = 4096;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct UdpFragmentKey {
    conn_id: String,
    frag_id: u32,
}

struct UdpFragmentEntry {
    created: Instant,
    total: u8,
    received: u8,
    received_bytes: usize,
    parts: Vec<Option<Bytes>>,
}

#[derive(Default)]
struct UdpFragmentReassembler {
    entries: HashMap<UdpFragmentKey, UdpFragmentEntry>,
    pushes_since_cleanup: u16,
}

impl UdpFragmentReassembler {
    fn push(
        &mut self,
        conn_id: String,
        frag_id: u32,
        frag_index: u8,
        frag_total: u8,
        payload: Bytes,
    ) -> Option<BinaryMessage> {
        if frag_total == 0 || frag_index >= frag_total {
            return None;
        }
        let now = Instant::now();
        self.pushes_since_cleanup = self.pushes_since_cleanup.wrapping_add(1);
        if self.pushes_since_cleanup == 0
            || self.entries.len() >= UDP_FRAGMENT_REASSEMBLY_MAX_ENTRIES
        {
            self.cleanup(now);
        }
        if self.entries.len() >= UDP_FRAGMENT_REASSEMBLY_MAX_ENTRIES {
            self.remove_oldest();
        }

        let key = UdpFragmentKey { conn_id, frag_id };
        let entry = self
            .entries
            .entry(key.clone())
            .or_insert_with(|| UdpFragmentEntry {
                created: now,
                total: frag_total,
                received: 0,
                received_bytes: 0,
                parts: vec![None; frag_total as usize],
            });
        if entry.total != frag_total {
            self.entries.remove(&key);
            return None;
        }
        let slot = frag_index as usize;
        if entry.parts[slot].is_some() {
            return None;
        }
        entry.received = entry.received.saturating_add(1);
        entry.received_bytes = entry.received_bytes.saturating_add(payload.len());
        if entry.received_bytes > MAX_FRAME_LEN as usize {
            self.entries.remove(&key);
            return None;
        }
        entry.parts[slot] = Some(payload);
        if entry.received != entry.total {
            return None;
        }

        let entry = self.entries.remove(&key)?;
        let mut out = BytesMut::with_capacity(entry.received_bytes);
        for part in entry.parts {
            out.extend_from_slice(&part?);
        }
        Some(BinaryMessage::UdpData {
            conn_id: key.conn_id,
            payload: out.freeze(),
        })
    }

    fn cleanup(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.created) <= UDP_FRAGMENT_REASSEMBLY_TTL);
    }

    fn remove_oldest(&mut self) {
        if let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.created)
            .map(|(key, _)| key.clone())
        {
            self.entries.remove(&key);
        }
    }
}

/// Bind a UDP socket at `addr`, tune kernel receive + send buffers, and hand
/// back a `std::net::UdpSocket` ready to be handed to `quinn::Endpoint::new`.
///
/// Public so the TUIC server endpoint (separate crate) can reuse the same
/// tuning path without duplicating the socket2 boilerplate.
pub fn bind_tuned_udp(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    bind_tuned_udp_on_interface(addr, None)
}

/// Bind and tune a UDP socket, optionally pinning every outbound packet to
/// one OS interface before the socket is bound. P2P callers use this to keep
/// mapping probes, punch packets, and QUIC packets out of the overlay TUN
/// when a learned peer LAN `/32` would otherwise win normal route lookup.
///
/// A requested interface is fail-closed: unsupported platforms and rejected
/// interface indexes return an error instead of silently using the default
/// route. Passing `None` preserves the relay/server socket behavior of
/// [`bind_tuned_udp`].
pub fn bind_tuned_udp_on_interface(
    addr: SocketAddr,
    interface_index: Option<NonZeroU32>,
) -> std::io::Result<std::net::UdpSocket> {
    let domain = match addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if matches!(addr, SocketAddr::V6(_)) {
        // A family-pinned socket has only an IPV6_UNICAST_IF index. Keep it
        // IPv6-only so IPv4-mapped replies cannot escape through the default
        // route. Unpinned relay/server sockets retain their dual-stack policy.
        let family_strict = interface_index.is_some();
        if let Err(error) = socket.set_only_v6(family_strict) {
            if family_strict {
                return Err(error);
            }
            tracing::warn!(error = %error, "IPV6_V6ONLY disable failed; using OS default");
        }
    }
    if let Some(interface_index) = interface_index {
        bind_udp_egress_interface(&socket, addr, interface_index)?;
    }
    // Reuse addr so a quick restart doesn't hit TIME_WAIT on Linux. Not
    // SO_REUSEPORT — we want one socket per endpoint so quinn controls the
    // full UDP flow.
    socket.set_reuse_address(true)?;
    if let Err(e) = socket.set_recv_buffer_size(UDP_SOCKET_RECV_BUF_BYTES) {
        tracing::warn!(error = %e, target = UDP_SOCKET_RECV_BUF_BYTES,
            "SO_RCVBUF setsockopt failed; using OS default (check net.core.rmem_max)");
    }
    if let Err(e) = socket.set_send_buffer_size(UDP_SOCKET_SEND_BUF_BYTES) {
        tracing::warn!(error = %e, target = UDP_SOCKET_SEND_BUF_BYTES,
            "SO_SNDBUF setsockopt failed; using OS default (check net.core.wmem_max)");
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_udp_egress_interface(
    socket: &Socket,
    addr: SocketAddr,
    interface_index: NonZeroU32,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let (level, option) = match addr {
        SocketAddr::V4(_) => (libc::IPPROTO_IP, libc::IP_UNICAST_IF),
        SocketAddr::V6(_) => (libc::IPPROTO_IPV6, libc::IPV6_UNICAST_IF),
    };
    // Linux defines both options as an interface index in network byte order.
    let interface_index = interface_index.get().to_be();
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            option,
            (&interface_index as *const u32).cast(),
            std::mem::size_of_val(&interface_index) as libc::socklen_t,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "visionos",
    target_os = "tvos",
    target_os = "watchos"
))]
fn bind_udp_egress_interface(
    socket: &Socket,
    addr: SocketAddr,
    interface_index: NonZeroU32,
) -> std::io::Result<()> {
    match addr {
        SocketAddr::V4(_) => socket.bind_device_by_index_v4(Some(interface_index)),
        SocketAddr::V6(_) => socket.bind_device_by_index_v6(Some(interface_index)),
    }
}

#[cfg(target_os = "windows")]
fn bind_udp_egress_interface(
    socket: &Socket,
    addr: SocketAddr,
    interface_index: NonZeroU32,
) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, IP_UNICAST_IF,
        SOCKET_ERROR,
    };

    let (level, option) = match addr {
        SocketAddr::V4(_) => (IPPROTO_IP, IP_UNICAST_IF),
        SocketAddr::V6(_) => (IPPROTO_IPV6, IPV6_UNICAST_IF),
    };
    let interface_index = winsock_unicast_interface_option_value(addr, interface_index);
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            level,
            option,
            (&interface_index as *const u32).cast(),
            std::mem::size_of_val(&interface_index) as i32,
        )
    };
    if result == SOCKET_ERROR {
        Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }))
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "windows", test))]
fn winsock_unicast_interface_option_value(addr: SocketAddr, interface_index: NonZeroU32) -> u32 {
    match addr {
        // Microsoft specifies network byte order for IP_UNICAST_IF.
        SocketAddr::V4(_) => interface_index.get().to_be(),
        // IPV6_UNICAST_IF instead consumes a native-endian IF_INDEX.
        SocketAddr::V6(_) => interface_index.get(),
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "visionos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "windows"
)))]
fn bind_udp_egress_interface(
    _socket: &Socket,
    _addr: SocketAddr,
    _interface_index: NonZeroU32,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "UDP egress interface pinning is unsupported on this platform",
    ))
}

fn tuned_endpoint_config() -> Result<EndpointConfig> {
    let mut cfg = EndpointConfig::default();
    cfg.max_udp_payload_size(QUIC_UDP_PAYLOAD_CEILING_BYTES)
        .map_err(|e| TransportError::Other(format!("quic endpoint max_udp_payload_size: {e}")))?;
    Ok(cfg)
}

/// QUIC transport tuning shared by both sides of the client<->gateway tunnel.
///
pub fn tuned_transport_config(tuning: &QuicTuning) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_concurrent_bidi_streams(256_u32.into());
    t.max_concurrent_uni_streams(0_u32.into());
    t.receive_window(VarInt::from_u64(200 * 1024 * 1024).unwrap());
    t.stream_receive_window(VarInt::from_u64(64 * 1024 * 1024).unwrap());
    t.send_window(64 * 1024 * 1024);
    t.datagram_receive_buffer_size(Some(QUIC_DATAGRAM_BUFFER_BYTES));
    t.datagram_send_buffer_size(QUIC_DATAGRAM_BUFFER_BYTES);
    let initial_mtu = tuning
        .initial_mtu
        .clamp(QUIC_SAFE_INITIAL_MTU_BYTES, QUIC_UDP_PAYLOAD_CEILING_BYTES);
    let min_mtu = tuning
        .min_mtu
        .clamp(QUIC_SAFE_INITIAL_MTU_BYTES, initial_mtu);
    let mtu_upper_bound = tuning
        .mtu_upper_bound
        .clamp(initial_mtu, QUIC_UDP_PAYLOAD_CEILING_BYTES);
    t.initial_mtu(initial_mtu);
    t.min_mtu(min_mtu);
    let mut mtud = MtuDiscoveryConfig::default();
    mtud.upper_bound(mtu_upper_bound);
    mtud.black_hole_cooldown(Duration::from_secs(
        tuning.black_hole_cooldown_secs.max(1) as u64
    ));
    t.mtu_discovery_config(Some(mtud));
    t.keep_alive_interval(Some(Duration::from_secs(tuning.keep_alive_secs as u64)));
    let idle_ms = tuning.max_idle_secs.saturating_mul(1000).max(1);
    t.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_millis(idle_ms as u64))
            .unwrap_or_else(|_| IdleTimeout::from(VarInt::from_u32(15_000))),
    ));
    match tuning.congestion.to_ascii_lowercase().as_str() {
        "bbr" => {
            let mut c = BbrConfig::default();
            if let Some(initial_window) = tuning.initial_congestion_window_bytes {
                c.initial_window(initial_window);
            }
            t.congestion_controller_factory(Arc::new(c));
        }
        "cubic" => {
            let mut c = CubicConfig::default();
            if let Some(initial_window) = tuning.initial_congestion_window_bytes {
                c.initial_window(initial_window);
            }
            t.congestion_controller_factory(Arc::new(c));
        }
        "new_reno" | "newreno" | "reno" => {
            let mut c = NewRenoConfig::default();
            if let Some(initial_window) = tuning.initial_congestion_window_bytes {
                c.initial_window(initial_window);
            }
            t.congestion_controller_factory(Arc::new(c));
        }
        _ => {
            tracing::warn!(
                requested = %tuning.congestion,
                "unknown congestion control, falling back to bbr"
            );
            let mut c = BbrConfig::default();
            if let Some(initial_window) = tuning.initial_congestion_window_bytes {
                c.initial_window(initial_window);
            }
            t.congestion_controller_factory(Arc::new(c));
        }
    }
    t
}

/// Public wrapper around the internal `wrap` so external callers
/// (e.g. P2P client) can build a `Session` from a quinn `Connection`
/// + bi-stream pair using the same Session pump used for relay traffic.
///
/// `wrap` already installs the `stats_probe` (see body), so the P2P
/// scheduler sees real RTT/loss numbers from sessions built via this
/// alias just as it does for relay sessions.
pub fn wrap_for_p2p(conn: quinn::Connection, send: SendStream, recv: RecvStream) -> Session {
    wrap(conn, send, recv)
}

pub fn wrap_for_p2p_with_control(
    conn: quinn::Connection,
    send: SendStream,
    recv: RecvStream,
    control: Option<(SendStream, RecvStream)>,
) -> Session {
    let capabilities = TransportCapabilities {
        route_bind_control_v1: control.is_some(),
        tcp_flow_stream_v1: true,
        // Direct P2P sessions have an authenticated peer certificate and do
        // not traverse a Gateway that can attest the relay source Peer.
        relay_source_attestation_v1: false,
        peer_mesh_v2: false,
    };
    wrap_with_optional_control(conn, send, recv, control, capabilities)
}

pub async fn open_p2p_control_lane(conn: &quinn::Connection) -> Option<(SendStream, RecvStream)> {
    let (mut send, mut recv) =
        match tokio::time::timeout(QUIC_CONTROL_LANE_ACCEPT_TIMEOUT, conn.open_bi()).await {
            Ok(Ok(streams)) => streams,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "P2P control lane open failed; using single stream");
                return None;
            }
            Err(_) => {
                tracing::debug!("P2P control lane open timed out; using single stream");
                return None;
            }
        };
    let hello = pack(&BinaryMessage::P2pPeerHint {
        peer_client_id: QUIC_CONTROL_LANE_V1.into(),
    })
    .to_bytes();
    if let Err(e) = write_frame(&mut send, &hello).await {
        tracing::debug!(error = %e, "P2P control lane hello write failed; using single stream");
        return None;
    }
    let ack = match tokio::time::timeout(QUIC_CONTROL_LANE_ACCEPT_TIMEOUT, read_frame(&mut recv))
        .await
    {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "P2P control lane ack read failed; using single stream");
            return None;
        }
        Err(_) => {
            tracing::debug!("P2P control lane ack timed out; using single stream");
            return None;
        }
    };
    match unpack(&ack) {
        Ok(BinaryMessage::P2pPeerHint { peer_client_id })
            if peer_client_id == QUIC_CONTROL_LANE_V1 =>
        {
            Some((send, recv))
        }
        Ok(other) => {
            tracing::debug!(
                ?other,
                "unexpected P2P control lane ack; using single stream"
            );
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "invalid P2P control lane ack; using single stream");
            None
        }
    }
}

pub async fn accept_p2p_control_lane(conn: &quinn::Connection) -> Option<(SendStream, RecvStream)> {
    let (mut send, mut recv) =
        match tokio::time::timeout(QUIC_CONTROL_LANE_ACCEPT_TIMEOUT, conn.accept_bi()).await {
            Ok(Ok(streams)) => streams,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "P2P control lane accept failed; using single stream");
                return None;
            }
            Err(_) => {
                tracing::debug!("P2P control lane not opened; using single stream");
                return None;
            }
        };
    let hello = match tokio::time::timeout(QUIC_CONTROL_LANE_ACCEPT_TIMEOUT, read_frame(&mut recv))
        .await
    {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "P2P control lane hello read failed; using single stream");
            return None;
        }
        Err(_) => {
            tracing::debug!("P2P control lane hello timed out; using single stream");
            return None;
        }
    };
    match unpack(&hello) {
        Ok(BinaryMessage::P2pPeerHint { peer_client_id })
            if peer_client_id == QUIC_CONTROL_LANE_V1 =>
        {
            let ack = pack(&BinaryMessage::P2pPeerHint {
                peer_client_id: QUIC_CONTROL_LANE_V1.into(),
            })
            .to_bytes();
            if let Err(e) = write_frame(&mut send, &ack).await {
                tracing::debug!(error = %e, "P2P control lane ack write failed; using single stream");
                None
            } else {
                Some((send, recv))
            }
        }
        Ok(other) => {
            tracing::debug!(
                ?other,
                "unexpected P2P control lane hello; using single stream"
            );
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "invalid P2P control lane hello; using single stream");
            None
        }
    }
}

struct QuicTcpFlowIo {
    send: SendStream,
    recv: RecvStream,
}

impl AsyncRead for QuicTcpFlowIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for QuicTcpFlowIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

impl Drop for QuicTcpFlowIo {
    fn drop(&mut self) {
        let _ = self.send.finish();
    }
}

fn tcp_flow_stream_manager(
    conn: quinn::Connection,
) -> (
    TcpFlowConnector,
    mpsc::Receiver<TcpFlowIncoming>,
    tokio::task::JoinHandle<()>,
) {
    let (open_tx, mut open_rx) = mpsc::channel::<TcpFlowOpenRequest>(TCP_FLOW_OPEN_CHANNEL_CAP);
    let (incoming_tx, incoming_rx) =
        mpsc::channel::<TcpFlowIncoming>(TCP_FLOW_INCOMING_CHANNEL_CAP);
    let connector = TcpFlowConnector::new(open_tx);
    let manager = tokio::spawn(async move {
        loop {
            tokio::select! {
                request = open_rx.recv() => {
                    let Some(request) = request else {
                        break;
                    };
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let result = open_tcp_flow_stream(conn, request.open, request.timeout).await;
                        let _ = request.response.send(result);
                    });
                }
                accepted = conn.accept_bi() => {
                    let (send, recv) = match accepted {
                        Ok(streams) => streams,
                        Err(e) => {
                            tracing::debug!(error = %e, "tcp flow stream accept loop ended");
                            break;
                        }
                    };
                    let incoming_tx = incoming_tx.clone();
                    tokio::spawn(async move {
                        accept_tcp_flow_stream(send, recv, incoming_tx).await;
                    });
                }
            }
        }
    });
    (connector, incoming_rx, manager)
}

async fn open_tcp_flow_stream(
    conn: quinn::Connection,
    open: TcpFlowOpen,
    timeout: Duration,
) -> Result<TcpFlowStream> {
    tokio::time::timeout(timeout, async move {
        let (send, recv) = conn.open_bi().await?;
        match open {
            TcpFlowOpen::Legacy(preface) => {
                let mut stream =
                    TcpFlowStream::new(preface.clone(), Box::pin(QuicTcpFlowIo { send, recv }));
                let preface_bytes = pack_tcp_flow_stream_preface(&preface);
                write_tcp_flow_frame(&mut stream, &preface_bytes).await?;
                match stream.read_connect_response().await? {
                    Ok(()) => Ok(stream),
                    Err(error) => Err(TransportError::Other(error)),
                }
            }
            TcpFlowOpen::Raw(preface_bytes) => {
                let (version, conn_id) = tcp_flow_open_route(&preface_bytes)?;
                if version != TCP_FLOW_OPEN_V2_VERSION {
                    return Err(tp_core::protocol::ProtoError::BadVersion(version).into());
                }
                let preface = TcpFlowStreamPreface {
                    conn_id,
                    network: "tcp".into(),
                    address: String::new(),
                };
                let mut stream =
                    TcpFlowStream::new(preface, Box::pin(QuicTcpFlowIo { send, recv }));
                write_tcp_flow_frame(&mut stream, &preface_bytes).await?;
                Ok(stream)
            }
        }
    })
    .await
    .map_err(|_| TransportError::FlowStreamUnavailable)?
}

async fn accept_tcp_flow_stream(
    send: SendStream,
    recv: RecvStream,
    incoming_tx: mpsc::Sender<TcpFlowIncoming>,
) {
    let mut io = QuicTcpFlowIo { send, recv };
    let frame =
        match tokio::time::timeout(TCP_FLOW_PREFACE_TIMEOUT, read_tcp_flow_frame(&mut io)).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "tcp flow stream preface read failed");
                return;
            }
            Err(_) => {
                tracing::debug!("tcp flow stream preface timed out");
                return;
            }
        };
    let (version, conn_id) = match tcp_flow_open_route(&frame) {
        Ok(route) => route,
        Err(e) => {
            tracing::debug!(error = %e, "tcp flow stream route header decode failed");
            return;
        }
    };
    let (preface, stream) = if version == TCP_FLOW_OPEN_V2_VERSION {
        let raw_preface = Bytes::from(frame);
        let preface = TcpFlowStreamPreface {
            conn_id: conn_id.clone(),
            network: "tcp".into(),
            address: String::new(),
        };
        let stream = TcpFlowStream::new_raw(conn_id, raw_preface, Box::pin(io));
        (preface, stream)
    } else {
        let preface = match unpack_tcp_flow_stream_preface(&frame) {
            Ok(preface) if preface.network == "tcp" => preface,
            Ok(preface) => {
                tracing::debug!(network = %preface.network, "tcp flow stream rejected non-tcp preface");
                return;
            }
            Err(e) => {
                tracing::debug!(error = %e, "tcp flow stream preface decode failed");
                return;
            }
        };
        let stream = TcpFlowStream::new(preface.clone(), Box::pin(io));
        (preface, stream)
    };
    let _ = incoming_tx.send(TcpFlowIncoming { preface, stream }).await;
}

fn wrap(conn: quinn::Connection, send: SendStream, recv: RecvStream) -> Session {
    wrap_with_optional_control(conn, send, recv, None, TransportCapabilities::default())
}

fn wrap_with_optional_control(
    conn: quinn::Connection,
    send: SendStream,
    recv: RecvStream,
    control: Option<(SendStream, RecvStream)>,
    capabilities: TransportCapabilities,
) -> Session {
    // Outbound stream carries `PackedMessage` values. Each is two Bytes at
    // most (header + optional payload) — the writer batches several and
    // hands every chunk straight to `send.write_chunks` (vectored writev
    // into QUIC), so the payload (16 KiB TCP block or ~1350 B UdpData)
    // never rides through an `extend_from_slice` memcpy. Capacity 2048
    // matches the pre-split sizing: 2048 slots × worst-case 64 KiB ≈
    // 128 MiB queue ceiling, roughly 2000 queued writes at typical frame size.
    let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(2048);

    // Inbound stream queue (reliable messages only). Datagram arrivals have
    // their own channel below so a slow TCP consumer cannot queue ahead of
    // UDP game-stream frames — this decoupling is the single biggest
    // game-streaming latency win (Go parity with `handleDatagrams()` goroutine).
    let (stream_in_tx, stream_in_rx) = mpsc::channel::<BinaryMessage>(4096);

    // Writer task: drain outbound queue onto the QUIC send stream,
    // coalescing every queued message's chunks into one `write_chunks`
    // call — writev batching. Per queued `PackedMessage` we push up to TWO chunks:
    //   * `[len u32 BE][header bytes]` — concatenated into a shared
    //     `BytesMut` arena so the per-frame len prefix + small header
    //     share one allocation;
    //   * optional `payload` Bytes — zero-copy, taken straight from the
    //     producer (e.g. `pipe_tcp`'s BytesMut arena or `pipe_udp`'s
    //     `recv_buf_from` arena).
    const MAX_BATCH: usize = 64;
    let writer = tokio::spawn(async move {
        let mut send = send;
        let mut framed = BytesMut::with_capacity(64 * 1024);
        // Worst case: 2 chunks per message × MAX_BATCH.
        let mut chunks: Vec<Bytes> = Vec::with_capacity(MAX_BATCH * 2);
        loop {
            chunks.clear();
            let Some(first) = out_rx.recv().await else {
                break;
            };
            push_frame_chunks(&mut framed, first, &mut chunks);
            while chunks.len() < MAX_BATCH * 2 {
                match out_rx.try_recv() {
                    Ok(p) => push_frame_chunks(&mut framed, p, &mut chunks),
                    Err(_) => break,
                }
            }
            if chunks.is_empty() {
                continue;
            }
            let mut idx = 0;
            let ok = loop {
                if idx >= chunks.len() {
                    break true;
                }
                match send.write_chunks(&mut chunks[idx..]).await {
                    Ok(w) => idx += w.chunks,
                    Err(_) => break false,
                }
            };
            if !ok {
                break;
            }
        }
        let _ = send.finish();
    });

    // Stream reader task: parse inbound frames on the reliable stream and
    // hand them off to `stream_in_tx` only (datagram arrivals go to their
    // own channel below — the critical decoupling for game streaming).
    let stream_reader = {
        let stream_in_tx = stream_in_tx.clone();
        tokio::spawn(async move {
            let mut recv = recv;
            let mut len_buf = [0u8; 4];
            loop {
                if recv.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let len = u32::from_be_bytes(len_buf);
                if len > MAX_FRAME_LEN {
                    tracing::warn!(len, "inbound frame too large; closing");
                    break;
                }
                // Keep the buffer initialized: `read_exact` may return after
                // writing only a prefix, so exposing spare capacity as `u8`
                // would make that error path rely on uninitialized memory.
                let mut body = vec![0u8; len as usize];
                if recv.read_exact(&mut body).await.is_err() {
                    break;
                }
                match unpack_bytes(Bytes::from(body)) {
                    Ok(msg) => {
                        if stream_in_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "protocol decode error");
                        break;
                    }
                }
            }
        })
    };

    let peer = conn.remote_address();
    let closer_conn = conn.clone();
    let closer: Arc<dyn Fn() + Send + Sync + 'static> =
        Arc::new(move || closer_conn.close(0u32.into(), b"bye"));

    let mut session =
        Session::new_channeled(out_tx, stream_in_rx, peer, closer, writer, stream_reader)
            .with_udp_data_mode(UdpDataMode::QuicDatagramRequired)
            .with_capabilities(capabilities);
    if let Some((control_send, control_recv)) = control {
        let (control_tx, control_rx) = mpsc::channel::<PackedMessage>(256);
        let (control_in_tx, control_in_rx) = mpsc::channel::<BinaryMessage>(1024);
        let control_writer = spawn_control_lane_writer(control_send, control_rx);
        let control_reader = spawn_control_lane_reader(control_recv, control_in_tx);
        session =
            session.with_control_channel(control_tx, control_in_rx, control_writer, control_reader);
    }

    if capabilities.tcp_flow_stream_v1 {
        let (connector, incoming_rx, manager) = tcp_flow_stream_manager(conn.clone());
        session = session.with_tcp_flow_streams(connector, incoming_rx, manager);
    }

    // Install the transport-health probe used by the P2P scheduler. Reads
    // live `quinn::Connection::stats()` on every call so callers always see
    // a fresh snapshot.
    //
    // Quinn 0.11's public `PathStats` doesn't expose the QUIC PTO
    // counter directly, so we approximate `pto_count` from
    // `path.black_holes_detected` — the count of times quinn declared the
    // path a "black hole" (no acks despite continued sends). That signal is
    // semantically close to "path is in trouble": rare on healthy
    // connections (no false positives on long-lived tunnels) and grows when
    // the path is genuinely stuck. The scheduler's `pto_count < 3`
    // threshold therefore degrades to "after 3 black-hole events the P2P
    // path is unhealthy → relay fallback", which matches the original
    // intent.
    {
        let stats_conn = conn.clone();
        let probe: SessionStatsFn = Arc::new(move || {
            let s = stats_conn.stats();
            let path = &s.path;
            let sent = path.sent_packets as f64;
            let lost = path.lost_packets as f64;
            let loss_rate = if sent > 0.0 { lost / sent } else { 0.0 };
            SessionStats {
                rtt: stats_conn.rtt(),
                loss_rate,
                pto_count: u32::try_from(path.black_holes_detected).unwrap_or(u32::MAX),
            }
        });
        session.install_stats_probe(probe);
    }

    // --- UDP fast path via QUIC datagrams --------------------------------
    //
    // Outbound UdpData bypasses the reliable bidi stream and rides QUIC
    // datagrams. Inbound datagrams land on a DEDICATED `datagram_in_tx`
    // (separate from `stream_in_tx`) — this is the key latency fix vs. the
    // pre-refactor design where a slow TCP handler on the session receiver
    // could queue UDP game-stream frames behind a 4096-deep mpsc.
    if conn.max_datagram_size().map(|v| v >= 64).unwrap_or(false) {
        let (dg_out_tx, mut dg_out_rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: QUIC_DATAGRAM_CHANNEL_CAP,
            per_association_byte_limit: QUIC_DATAGRAM_BUFFER_BYTES,
            global_packet_limit: QUIC_DATAGRAM_CHANNEL_CAP,
            global_byte_limit: QUIC_DATAGRAM_BUFFER_BYTES,
        });
        let (dg_in_tx, dg_in_rx) = drop_oldest_channel::<BinaryMessage>(QUIC_DATAGRAM_CHANNEL_CAP);

        // Pull the Arc<UdpRouteStats> from the session so the dg_writer /
        // dg_reader tasks can bump counters that are visible to callers via
        // `SessionSender::udp_route_stats()` / periodic summary logs.
        let stats = session.stats_handle();

        let dg_conn_send = conn.clone();
        let stats_writer = stats.clone();
        let dg_writer = tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            // Shared merge arena for the (header, payload) → single Bytes
            // collapse that `quinn::Connection::send_datagram` requires
            // (quinn takes one contiguous Bytes per datagram — no
            // vectored variant). 64 KiB / 8 KiB low-water mirrors the
            // pipe_udp arena sizing: enough to hold a handful of
            // 1350 B datagrams in flight before the writer side
            // round-trips through quinn. The payload memcpy itself
            // is unavoidable on this API surface; what this arena
            // removes is the per-frame `BytesMut::with_capacity` +
            // Arc allocation that the old `packed.to_bytes()` call
            // performed inside the hot loop.
            const MERGE_ARENA_BYTES: usize = 64 * 1024;
            const MERGE_ARENA_LOW_WATER: usize = 8 * 1024;
            let mut merge: BytesMut = BytesMut::with_capacity(MERGE_ARENA_BYTES);
            loop {
                let scheduler_quantum = dg_conn_send
                    .max_datagram_size()
                    .unwrap_or(DEFAULT_DATAGRAM_DRR_QUANTUM)
                    .max(1);
                let Some(batch) = dg_out_rx.recv_batch_with_quantum(scheduler_quantum).await else {
                    break;
                };
                let max_datagram_size = dg_conn_send
                    .max_datagram_size()
                    .unwrap_or(DEFAULT_DATAGRAM_DRR_QUANTUM)
                    .max(1);
                stats_writer
                    .record_datagram_send_buffer_space(dg_conn_send.datagram_send_buffer_space());
                let batch_bytes = batch.iter().map(|frame| frame.bytes).sum::<usize>();
                if batch.iter().any(|frame| frame.bytes > max_datagram_size) {
                    let dropped = batch.len() as u64;
                    stats_writer
                        .dropped_full
                        .fetch_add(dropped, Ordering::Relaxed);
                    tracing::debug!(
                        packets = batch.len(),
                        bytes = batch_bytes,
                        max_datagram_size,
                        "datagram batch exceeds current mtu; dropping before partial send"
                    );
                    continue;
                }
                let mut send_space = dg_conn_send.datagram_send_buffer_space();
                for _ in 0..QUIC_DATAGRAM_SEND_SPACE_RECHECKS {
                    if send_space >= batch_bytes {
                        break;
                    }
                    tokio::task::yield_now().await;
                    send_space = dg_conn_send.datagram_send_buffer_space();
                    stats_writer.record_datagram_send_buffer_space(send_space);
                }
                if send_space < batch_bytes {
                    let evicted = batch.len() as u64;
                    stats_writer
                        .datagram_global_budget_evicted
                        .fetch_add(evicted, Ordering::Relaxed);
                    stats_writer
                        .dropped_full
                        .fetch_add(evicted, Ordering::Relaxed);
                    tracing::debug!(
                        packets = batch.len(),
                        bytes = batch_bytes,
                        send_space,
                        "datagram send buffer full; locally dropped before quinn eviction"
                    );
                    continue;
                }
                for frame in batch {
                    let packed = frame.packed;
                    if merge.capacity() - merge.len() < MERGE_ARENA_LOW_WATER {
                        merge.reserve(MERGE_ARENA_BYTES);
                    }
                    let bytes = match &packed.payload {
                        // Header-only datagram (none currently produced — UdpData
                        // is the only datagram variant, and it always has a
                        // payload — but keep the branch cheap for future types).
                        None => packed.header.clone(),
                        Some(payload) => {
                            let total = packed.header.len() + payload.len();
                            merge.extend_from_slice(&packed.header);
                            merge.extend_from_slice(payload);
                            merge.split_to(total).freeze()
                        }
                    };
                    match dg_conn_send.send_datagram(bytes) {
                        Ok(()) => {
                            stats_writer
                                .datagram_write_ok
                                .fetch_add(1, Ordering::Relaxed);
                            stats_writer.record_datagram_send_buffer_space(
                                dg_conn_send.datagram_send_buffer_space(),
                            );
                        }
                        Err(e) => {
                            // `send_datagram` is deliberately non-blocking for
                            // realtime UDP. This branch is only for terminal/setup
                            // conditions (ConnectionLost / UnsupportedByPeer /
                            // Disabled / TooLarge).
                            stats_writer
                                .datagram_write_err
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(error = %e, "datagram send failed; dropping");
                        }
                    }
                }
            }
        });

        let dg_conn_recv = conn.clone();
        let stats_reader = stats.clone();
        let dg_reader = tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            let mut udp_fragments = UdpFragmentReassembler::default();
            while let Ok(data) = dg_conn_recv.read_datagram().await {
                let len = data.len();
                match unpack_bytes(data) {
                    Ok(BinaryMessage::UdpFragment {
                        conn_id,
                        frag_id,
                        frag_index,
                        frag_total,
                        payload,
                    }) => {
                        let Some(msg) =
                            udp_fragments.push(conn_id, frag_id, frag_index, frag_total, payload)
                        else {
                            continue;
                        };
                        if dg_in_tx.is_closed() {
                            break;
                        }
                        let dropped = dg_in_tx.send_drop_oldest(msg).unwrap_or(false);
                        if dropped {
                            stats_reader
                                .datagram_recv_dropped
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        stats_reader
                            .datagram_recv_ok
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(msg) => {
                        if dg_in_tx.is_closed() {
                            break;
                        }
                        let dropped = dg_in_tx.send_drop_oldest(msg).unwrap_or(false);
                        if dropped {
                            stats_reader
                                .datagram_recv_dropped
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        stats_reader
                            .datagram_recv_ok
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        stats_reader
                            .datagram_recv_decode_err
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(error = %e, len, "datagram decode failed");
                    }
                }
            }
        });

        // Live MTU probe: quinn's `max_datagram_size()` reflects PMTUD state,
        // so a session-start cached value (which starts around 1200 B) would
        // permanently strand 1200–1410 B packets on the stream after the
        // path expands. Query fresh on every send.
        let mtu_conn = conn.clone();
        let mtu_fn: DatagramMtuFn = Arc::new(move || mtu_conn.max_datagram_size());

        // Live buffer-space probe: quinn 0.11.9 silently evicts older queued
        // datagrams when this hits 0 on send_datagram, so sampling this in
        // the summary log is the single best signal for "is my tunnel the
        // actual bottleneck?" as distinct from network-downstream loss.
        let bufspace_conn = conn.clone();
        let buf_space_fn: DatagramBufSpaceFn =
            Arc::new(move || bufspace_conn.datagram_send_buffer_space());

        session.with_datagram_channel(
            dg_out_tx,
            dg_in_rx,
            mtu_fn,
            buf_space_fn,
            dg_writer,
            dg_reader,
        )
    } else {
        tracing::debug!("QUIC datagrams unavailable; UdpData requires datagram transport");
        session
    }
}

fn spawn_control_lane_writer(
    mut send: SendStream,
    mut rx: mpsc::Receiver<PackedMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(packed) = rx.recv().await {
            let bytes = packed.to_bytes();
            if write_frame(&mut send, &bytes).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    })
}

fn spawn_control_lane_reader(
    mut recv: RecvStream,
    stream_in_tx: mpsc::Sender<BinaryMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let frame = match read_frame(&mut recv).await {
                Ok(frame) => frame,
                Err(_) => break,
            };
            match unpack(&frame) {
                Ok(msg) => {
                    if stream_in_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "control lane protocol decode error");
                    break;
                }
            }
        }
    })
}

/// Append up to two write-ready `Bytes` chunks for one outbound frame to
/// `out`:
///   * `[len:u32 BE][header bytes]` — concatenated into `framed` and split
///     off as one zero-copy `Bytes` so the small header + 4-byte len
///     prefix share a single allocation (no per-frame Bytes::from([u8;4])).
///   * optional `payload` — pushed as-is (zero-copy clone of the producer's
///     arena slice), so the writer's subsequent `write_chunks` sends
///     header and payload via vectored writev without any memcpy.
///
/// Oversized frames (total > MAX_FRAME_LEN) are dropped with a warning and
/// contribute no chunks — matches the pre-refactor `frame_into`'s semantics.
fn push_frame_chunks(framed: &mut BytesMut, packed: PackedMessage, out: &mut Vec<Bytes>) {
    let total_len = packed.total_len();
    if total_len as u64 > MAX_FRAME_LEN as u64 {
        tracing::warn!(len = total_len, "dropping oversized outbound frame");
        return;
    }
    framed.reserve(4 + packed.header.len());
    framed.extend_from_slice(&(total_len as u32).to_be_bytes());
    framed.extend_from_slice(&packed.header);
    out.push(framed.split_to(4 + packed.header.len()).freeze());
    if let Some(payload) = packed.payload {
        out.push(payload);
    }
}

// ---------- server ----------

pub struct QuicServer {
    endpoint: Endpoint,
}

impl QuicServer {
    pub fn bind(
        addr: SocketAddr,
        tls_cfg: Arc<rustls::ServerConfig>,
        tuning: QuicTuning,
    ) -> Result<Self> {
        let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(tls_cfg.as_ref().clone())
            .map_err(|e| TransportError::Tls(format!("quic server crypto: {e}")))?;
        let mut server_cfg = ServerConfig::with_crypto(Arc::new(qsc));
        server_cfg.transport_config(Arc::new(tuned_transport_config(&tuning)));

        // Build the underlying UDP socket ourselves so we can apply
        // SO_RCVBUF / SO_SNDBUF tuning before handing to quinn.
        let std_sock = bind_tuned_udp(addr)?;
        let runtime = quinn::default_runtime()
            .ok_or_else(|| TransportError::Other("no quinn runtime (tokio)".into()))?;
        let endpoint = Endpoint::new(
            tuned_endpoint_config()?,
            Some(server_cfg),
            std_sock,
            runtime,
        )?;
        Ok(Self { endpoint })
    }

    /// Pull the next inbound QUIC connection attempt off the endpoint.
    /// Returns `None` only when the endpoint is closed (terminal — the
    /// accept loop should exit). Does **not** drive the QUIC/TLS handshake:
    /// hand the returned `Incoming` to `complete_handshake` on a spawned
    /// task so that a failing or slow peer cannot block other incoming
    /// clients, and per-peer handshake failures (ALPN mismatch, TLS error,
    /// bad Auth frame, auth rejection) never take the gateway offline.
    pub async fn accept_incoming(&self) -> Option<quinn::Incoming> {
        self.endpoint.accept().await
    }

    /// Drive one `quinn::Incoming` through QUIC/TLS handshake and the
    /// tunnel-protocol Auth frame, producing a ready `(AuthParams, Session)`
    /// on success. All failure modes map to `Err` that the caller is
    /// expected to log and drop; they are per-connection and must not
    /// affect the accept loop.
    pub async fn complete_handshake<H: AuthHandler>(
        incoming: quinn::Incoming,
        auth: &H,
    ) -> Result<(AuthParams, Session)> {
        let conn = incoming.await?;
        let peer_addr = conn.remote_address();
        let (mut send, mut recv) = conn.accept_bi().await?;

        let first = read_frame(&mut recv).await?;
        let msg = unpack(&first)?;
        let BinaryMessage::Auth {
            tunnel_id,
            client_id,
            group_id,
            username,
            password,
            group_password,
            role,
            capabilities: offered_capabilities,
        } = msg
        else {
            let err_frame = pack(&BinaryMessage::AuthResponse {
                status: AUTH_STATUS_FAILED.into(),
                reason: "expected Auth as first frame".into(),
                capabilities: TransportCapabilities::default(),
            })
            .to_bytes();
            let _ = write_frame(&mut send, &err_frame).await;
            return Err(TransportError::Unexpected("Auth"));
        };

        let params = AuthParams {
            tunnel_id,
            client_id,
            group_id,
            username,
            password,
            group_password,
            role,
            capabilities: offered_capabilities,
            peer_addr,
        };

        match auth.authenticate(&params).await {
            Ok(()) => {
                tracing::info!(
                    client_id = %params.client_id,
                    group_id = %params.group_id,
                    peer = %peer_addr,
                    "transport auth accepted"
                );
                let control = match tokio::time::timeout(
                    QUIC_CONTROL_LANE_ACCEPT_TIMEOUT,
                    conn.accept_bi(),
                )
                .await
                {
                    Ok(Ok((control_send, mut control_recv))) => {
                        let hello = match tokio::time::timeout(
                            QUIC_CONTROL_LANE_ACCEPT_TIMEOUT,
                            read_frame(&mut control_recv),
                        )
                        .await
                        {
                            Ok(Ok(frame)) => frame,
                            Ok(Err(e)) => {
                                tracing::debug!(client_id = %params.client_id, error = %e, "QUIC control lane hello read failed; using single stream");
                                Vec::new()
                            }
                            Err(_) => {
                                tracing::debug!(client_id = %params.client_id, "QUIC control lane hello timed out; using single stream");
                                Vec::new()
                            }
                        };
                        match unpack(&hello) {
                            Ok(BinaryMessage::P2pPeerHint { peer_client_id })
                                if peer_client_id == QUIC_CONTROL_LANE_V1 =>
                            {
                                tracing::info!(client_id = %params.client_id, control_lane = true, "QUIC control lane accepted");
                                Some((control_send, control_recv))
                            }
                            Ok(other) => {
                                tracing::debug!(client_id = %params.client_id, ?other, "unexpected QUIC control lane hello; using single stream");
                                None
                            }
                            Err(e) => {
                                tracing::debug!(client_id = %params.client_id, error = %e, "invalid QUIC control lane hello; using single stream");
                                None
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(client_id = %params.client_id, error = %e, "QUIC control lane accept failed; using single stream");
                        None
                    }
                    Err(_) => {
                        tracing::debug!(client_id = %params.client_id, "QUIC control lane not opened; using single stream");
                        None
                    }
                };
                let reason = if control.is_some() {
                    QUIC_CONTROL_LANE_V1
                } else {
                    ""
                };
                let negotiated_capabilities = TransportCapabilities {
                    route_bind_control_v1: offered_capabilities.route_bind_control_v1
                        && control.is_some(),
                    tcp_flow_stream_v1: offered_capabilities.tcp_flow_stream_v1,
                    relay_source_attestation_v1: offered_capabilities.relay_source_attestation_v1,
                    peer_mesh_v2: offered_capabilities.peer_mesh_v2,
                };
                let ack = pack(&BinaryMessage::AuthResponse {
                    status: AUTH_STATUS_SUCCESS.into(),
                    reason: reason.into(),
                    capabilities: negotiated_capabilities,
                })
                .to_bytes();
                write_frame(&mut send, &ack).await?;
                Ok((
                    params,
                    wrap_with_optional_control(conn, send, recv, control, negotiated_capabilities),
                ))
            }
            Err(reason) => {
                tracing::warn!(
                    client_id = %params.client_id,
                    group_id = %params.group_id,
                    peer = %peer_addr,
                    reason = %reason,
                    "transport auth rejected"
                );
                let nak = pack(&BinaryMessage::AuthResponse {
                    status: AUTH_STATUS_FAILED.into(),
                    reason: reason.clone(),
                    capabilities: TransportCapabilities::default(),
                })
                .to_bytes();
                let _ = write_frame(&mut send, &nak).await;
                Err(TransportError::AuthFailed(reason))
            }
        }
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Cheap clone of the underlying `quinn::Endpoint` (internally an Arc),
    /// so the binary's shutdown path can issue `close()` + `wait_idle()`
    /// while `serve()` still owns the `QuicServer` by move. Without this
    /// the select!-break on SIGINT just drops the Endpoint which emits a
    /// hard reset — peers see an abrupt disconnect instead of a clean
    /// CONNECTION_CLOSE frame.
    pub fn endpoint_handle(&self) -> quinn::Endpoint {
        self.endpoint.clone()
    }
}

// ---------- client ----------

pub struct QuicClient {
    endpoint: Endpoint,
}

impl QuicClient {
    pub fn new(tls_cfg: Arc<rustls::ClientConfig>, tuning: QuicTuning) -> Result<Self> {
        let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(tls_cfg.as_ref().clone())
            .map_err(|e| TransportError::Tls(format!("quic client crypto: {e}")))?;
        let mut client_cfg = ClientConfig::new(Arc::new(qcc));
        client_cfg.transport_config(Arc::new(tuned_transport_config(&tuning)));

        // Same tuned UDP socket treatment on the client side — the return
        // path carries moonlight UDP frames back to clash, so the kernel
        // receive buffer must be fat enough to absorb bursts.
        let std_sock = bind_tuned_udp("0.0.0.0:0".parse().unwrap())?;
        let runtime = quinn::default_runtime()
            .ok_or_else(|| TransportError::Other("no quinn runtime (tokio)".into()))?;
        let mut endpoint = Endpoint::new(tuned_endpoint_config()?, None, std_sock, runtime)?;
        endpoint.set_default_client_config(client_cfg);
        Ok(Self { endpoint })
    }

    /// Complete only the QUIC/TLS handshake, without opening the tunnel Auth
    /// stream. Used by the Gateway's local BYOG readiness Test so a TCP probe
    /// is never mistaken for QUIC readiness.
    pub async fn probe_tls(&self, addr: SocketAddr, server_name: &str) -> Result<()> {
        let connection = self.endpoint.connect(addr, server_name)?.await?;
        connection.close(0u32.into(), b"readiness probe complete");
        Ok(())
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        server_name: &str,
        auth: AuthParams,
    ) -> Result<Session> {
        let client_id = auth.client_id.clone();
        let group_id = auth.group_id.clone();
        tracing::debug!(%client_id, %group_id, gateway = %addr, "dialing gateway");
        let conn = self.endpoint.connect(addr, server_name)?.await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let pkt = pack(&BinaryMessage::Auth {
            tunnel_id: auth.tunnel_id,
            client_id: auth.client_id,
            group_id: auth.group_id,
            username: auth.username,
            password: auth.password,
            group_password: auth.group_password,
            role: auth.role,
            capabilities: TransportCapabilities {
                route_bind_control_v1: true,
                tcp_flow_stream_v1: true,
                relay_source_attestation_v1: true,
                peer_mesh_v2: auth.capabilities.peer_mesh_v2,
            },
        })
        .to_bytes();
        write_frame(&mut send, &pkt).await?;
        let tentative_control = match tokio::time::timeout(
            QUIC_CONTROL_LANE_ACCEPT_TIMEOUT,
            conn.open_bi(),
        )
        .await
        {
            Ok(Ok((mut control_send, control_recv))) => {
                let hello = pack(&BinaryMessage::P2pPeerHint {
                    peer_client_id: QUIC_CONTROL_LANE_V1.into(),
                })
                .to_bytes();
                match write_frame(&mut control_send, &hello).await {
                    Ok(()) => Some((control_send, control_recv)),
                    Err(e) => {
                        tracing::debug!(%client_id, error = %e, "QUIC control lane hello write failed; using single stream");
                        None
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::debug!(%client_id, error = %e, "QUIC control lane open failed; using single stream");
                None
            }
            Err(_) => {
                tracing::debug!(%client_id, "QUIC control lane open timed out; using single stream");
                None
            }
        };

        let reply = read_frame(&mut recv).await?;
        match unpack(&reply)? {
            BinaryMessage::AuthResponse {
                status,
                reason,
                capabilities,
            } if status == AUTH_STATUS_SUCCESS => {
                tracing::info!(%client_id, %group_id, "transport auth accepted by gateway");
                let control = if reason == QUIC_CONTROL_LANE_V1 {
                    tracing::info!(%client_id, control_lane = true, "QUIC control lane negotiated");
                    tentative_control
                } else {
                    None
                };
                let negotiated_capabilities = TransportCapabilities {
                    route_bind_control_v1: capabilities.route_bind_control_v1 && control.is_some(),
                    tcp_flow_stream_v1: capabilities.tcp_flow_stream_v1,
                    relay_source_attestation_v1: capabilities.relay_source_attestation_v1,
                    peer_mesh_v2: capabilities.peer_mesh_v2,
                };
                Ok(wrap_with_optional_control(
                    conn,
                    send,
                    recv,
                    control,
                    negotiated_capabilities,
                ))
            }
            BinaryMessage::AuthResponse { reason, .. } => {
                tracing::warn!(%client_id, %group_id, reason = %reason, "transport auth rejected by gateway");
                Err(TransportError::AuthFailed(reason))
            }
            _ => Err(TransportError::Unexpected("AuthResponse")),
        }
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }
}

// ---------- framed helpers (auth handshake only) ----------

async fn write_frame(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    let len = bytes.len();
    if len as u64 > MAX_FRAME_LEN as u64 {
        return Err(TransportError::FrameTooLarge(len as u32));
    }
    let hdr = (len as u32).to_be_bytes();
    send.write_all(&hdr).await?;
    send.write_all(bytes).await?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::Other(e.to_string()))?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(TransportError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| TransportError::Other(e.to_string()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winsock_unicast_interface_options_use_family_specific_byte_order() {
        let index = NonZeroU32::new(0x0102_0304).unwrap();

        assert_eq!(
            winsock_unicast_interface_option_value("0.0.0.0:0".parse().unwrap(), index),
            index.get().to_be(),
            "Windows IPv4 IP_UNICAST_IF consumes network byte order"
        );
        assert_eq!(
            winsock_unicast_interface_option_value("[::]:0".parse().unwrap(), index),
            index.get(),
            "Windows IPv6 IPV6_UNICAST_IF consumes host byte order"
        );
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    ))]
    #[test]
    fn requested_invalid_udp_egress_interface_fails_closed() {
        let invalid = std::num::NonZeroU32::new(u32::MAX).unwrap();

        let error = bind_tuned_udp_on_interface("0.0.0.0:0".parse().unwrap(), Some(invalid))
            .expect_err("an invalid requested interface must not fall back to the default route");

        assert_ne!(error.kind(), std::io::ErrorKind::Unsupported);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_udp_socket_records_the_requested_egress_interface() {
        use std::ffi::{c_char, CString};

        extern "C" {
            fn if_nametoindex(ifname: *const c_char) -> u32;
        }

        let name = CString::new("lo0").unwrap();
        let index = NonZeroU32::new(unsafe { if_nametoindex(name.as_ptr()) })
            .expect("macOS loopback interface index");
        let socket = bind_tuned_udp_on_interface("0.0.0.0:0".parse().unwrap(), Some(index))
            .expect("bind pinned UDP socket");
        let socket = socket2::SockRef::from(&socket);

        assert_eq!(socket.device_index_v4().unwrap(), Some(index));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pinned_ipv6_udp_socket_is_family_strict() {
        use std::ffi::{c_char, CString};

        extern "C" {
            fn if_nametoindex(ifname: *const c_char) -> u32;
        }

        let name = CString::new("lo0").unwrap();
        let index = NonZeroU32::new(unsafe { if_nametoindex(name.as_ptr()) })
            .expect("macOS loopback interface index");
        let socket = bind_tuned_udp_on_interface("[::]:0".parse().unwrap(), Some(index))
            .expect("bind pinned IPv6 UDP socket");
        let socket = socket2::SockRef::from(&socket);

        assert!(
            socket.only_v6().unwrap(),
            "a family-pinned IPv6 socket must not emit IPv4-mapped traffic without IP_UNICAST_IF"
        );
        assert_eq!(socket.device_index_v6().unwrap(), Some(index));
    }

    #[test]
    fn quic_tuning_keeps_safe_initial_mtu_and_probes_to_ceiling() {
        let cfg = tuned_transport_config(&QuicTuning::default());
        let debug = format!("{cfg:?}");
        assert!(
            debug.contains("initial_mtu: 1200"),
            "transport must not assume Ethernet MTU before PMTUD: {debug}"
        );
        assert!(
            debug.contains("upper_bound: 1452"),
            "MTUD should still be allowed to probe up to the IPv6-safe Ethernet payload ceiling: {debug}"
        );
    }

    #[test]
    fn game_streaming_profile_starts_high_but_keeps_safe_recovery_floor() {
        let cfg = tuned_transport_config(&QuicTuning::game_streaming());
        let debug = format!("{cfg:?}");
        assert!(
            debug.contains("initial_mtu: 1452"),
            "game-streaming profile should avoid fragmenting common 1385B UDP payloads: {debug}"
        );
        assert!(
            debug.contains("min_mtu: 1200"),
            "game-streaming profile must preserve Quinn's safe black-hole recovery floor: {debug}"
        );
        assert!(
            debug.contains("black_hole_cooldown: 5s"),
            "game-streaming profile should retry MTUD quickly after congestion false positives: {debug}"
        );
    }

    #[test]
    fn quic_realtime_defaults_match_moonlight_tuic_envelope() {
        assert_eq!(QuicTuning::default().congestion, "bbr");
        assert_eq!(
            QuicTuning::default().initial_congestion_window_bytes,
            Some(QUIC_INITIAL_CONGESTION_WINDOW_BYTES)
        );
        assert_eq!(QuicTuning::default().keep_alive_secs, 10);
        assert_eq!(QuicTuning::default().max_idle_secs, 60);
        assert_eq!(QUIC_DATAGRAM_BUFFER_BYTES, 48 * 1024 * 1024);
        assert_eq!(QUIC_INITIAL_CONGESTION_WINDOW_BYTES, 4 * 1024 * 1024);
        assert_eq!(QUIC_DATAGRAM_CHANNEL_CAP, 4096);
        assert_eq!(QUIC_DATAGRAM_SEND_SPACE_RECHECKS, 8);
    }

    #[test]
    fn quic_datagram_writer_keeps_realtime_drop_oldest_semantics() {
        let source = include_str!("quic.rs");
        let nonblocking_call = ["dg_conn_send", ".send_datagram(bytes)"].concat();
        let blocking_call = ["send_datagram_wait", "(bytes).await"].concat();

        assert!(
            source.contains(&nonblocking_call),
            "datagram writer must use non-blocking send_datagram for realtime UDP"
        );
        assert!(
            !source.contains(&blocking_call),
            "datagram writer must not prioritize stale datagrams with send_datagram_wait"
        );
    }

    #[test]
    fn udp_fragment_reassembler_rebuilds_original_udp_data() {
        let mut reassembler = UdpFragmentReassembler::default();
        assert!(reassembler
            .push("abcdefghijkl".into(), 7, 1, 2, Bytes::from_static(b"light"),)
            .is_none());

        match reassembler
            .push("abcdefghijkl".into(), 7, 0, 2, Bytes::from_static(b"moon"))
            .expect("second fragment should complete the datagram")
        {
            BinaryMessage::UdpData { conn_id, payload } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert_eq!(&payload[..], b"moonlight");
            }
            other => panic!("expected reassembled UdpData, got {other:?}"),
        }
    }
}
