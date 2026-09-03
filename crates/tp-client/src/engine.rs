//! Tunnel client engine.
//!
//! Owns one or more transport `Session`s to the gateway (one per replica) and a
//! dispatch loop that handles inbound `Connect`/`Data`/`Close` by dialing
//! locally and piping. Also runs a heartbeat with the platform.
//!
//! Submodule layout:
//!
//! * [`pipe`] — per-connection TCP / UDP pipe loops.
//! * [`util`] — time / DNS / cancellation helpers + the internal
//!   `SessionOutcome` enum shared across reconnect loops.

mod pipe;
mod util;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use parking_lot::RwLock;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tokio_util::task::{AbortOnDropHandle, TaskTracker};
use tp_core::protocol::{unpack_tcp_flow_open_v2, BinaryMessage};
use tp_core::provisioning::{
    GatewayBootstrapV2, PeerBootstrapV2, PeerProfileV2, PlatformHeartbeatPathModeV2,
};
use tp_core::Protocol;
use tp_transport::{
    drop_oldest_channel, tls, AuthParams, DropOldestSender, GrpcClient, QuicClient, QuicTuning,
    Session, WsClient,
};

use crate::host_filter::HostFilter as CompiledHostFilter;
use crate::link::watchdog::{
    evaluate_link_watchdog, LinkKind, LinkWatchdogConfig, LinkWatchdogDecision,
    LinkWatchdogSnapshot,
};
use crate::local_target::{
    LocalRouteClaims, LocalRouteKind, LocalServiceExport, LocalTargetResolver,
};
use crate::p2p::flow_scheduler::{
    CandidateKey, CandidatePath, FlowKind, FlowPlacementRegistry, LaneLoadSnapshot,
    PlacementCandidate, PlacementExcludedReason, ReplicaFlowScheduler,
};
use crate::p2p::multi_sender::MultiSenderRouter;
use crate::p2p::scheduler::PathKind;
use crate::p2p::session::PendingUdpInboundBufferResult;
use crate::peer_link_manager::PeerRelationKey;
use crate::platform::TunnelConfig;
#[cfg(test)]
use crate::status::NullListener;
use crate::status::{
    derive_path_mode, ConnectionStatus, HeartbeatStatus, StatusListener, TrafficCounters,
    TrafficPath,
};
use pipe::{pipe_tcp, pipe_udp};
use util::{
    gateway_endpoint, grpc_url, has_tls_scheme, resolve_gateway_addr, resolve_target_addr_once,
    tls_domain, unix_now, websocket_url, SessionOutcome, TransportKind,
};

#[derive(Clone, Debug, Default)]
pub struct EngineConfig {
    pub platform_url: String,
    pub gateway_ca_path: Option<std::path::PathBuf>,
    pub insecure_tls: bool,
    pub client_version: String,
    pub device_id: Option<String>,
    pub device_name: Option<String>,
}

fn next_peer_heartbeat_timestamp_ms(last: &mut u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let timestamp = now.max(last.saturating_add(1));
    *last = timestamp;
    timestamp
}

fn peer_heartbeat_path_mode(
    path_mode: crate::status::ConnectionPathMode,
) -> PlatformHeartbeatPathModeV2 {
    match path_mode {
        crate::status::ConnectionPathMode::P2p => PlatformHeartbeatPathModeV2::Direct,
        crate::status::ConnectionPathMode::Relay => PlatformHeartbeatPathModeV2::Relay,
        crate::status::ConnectionPathMode::Connecting => PlatformHeartbeatPathModeV2::Connecting,
        crate::status::ConnectionPathMode::Disconnected => {
            PlatformHeartbeatPathModeV2::Disconnected
        }
    }
}

fn bind_target_udp_socket() -> std::io::Result<std::net::UdpSocket> {
    // Target sockets are one-flow-per-conn_id. Do not use the QUIC listener
    // helper here: it enables SO_REUSEADDR, which lets Linux assign the same
    // ephemeral 4-tuple to concurrent sockets targeting the same UDP service.
    // Replies can then be delivered to the wrong flow.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    let socket_ref = socket2::SockRef::from(&socket);
    if let Err(error) = socket_ref.set_recv_buffer_size(tp_transport::UDP_SOCKET_RECV_BUF_BYTES) {
        tracing::warn!(
            error = %error,
            target = tp_transport::UDP_SOCKET_RECV_BUF_BYTES,
            "target UDP SO_RCVBUF setsockopt failed; using OS default"
        );
    }
    if let Err(error) = socket_ref.set_send_buffer_size(tp_transport::UDP_SOCKET_SEND_BUF_BYTES) {
        tracing::warn!(
            error = %error,
            target = tp_transport::UDP_SOCKET_SEND_BUF_BYTES,
            "target UDP SO_SNDBUF setsockopt failed; using OS default"
        );
    }
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn bind_tokio_target_udp_socket() -> std::io::Result<UdpSocket> {
    UdpSocket::from_std(bind_target_udp_socket()?)
}

fn relay_conn_id_to_wire_v2(conn_id: &str) -> Option<[u8; 12]> {
    let bytes = conn_id.as_bytes();
    if bytes.is_empty() || bytes.len() > 12 || !bytes.is_ascii() || bytes.contains(&0) {
        return None;
    }
    let mut wire = [0_u8; 12];
    wire[..bytes.len()].copy_from_slice(bytes);
    Some(wire)
}

fn relay_conn_id_from_wire_v2(conn_id: &[u8; 12]) -> Option<String> {
    let end = conn_id
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(conn_id.len());
    if end == 0 || conn_id[end..].iter().any(|byte| *byte != 0) {
        return None;
    }
    std::str::from_utf8(&conn_id[..end])
        .ok()
        .map(ToOwned::to_owned)
}

const TCP_FLOW_COPY_BUFFER_BYTES: usize = 64 * 1024;
const V2_LOCAL_LAN_EXPORT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const UDP_FLOW_INBOUND_CHANNEL_CAP: usize = 2048;
pub(crate) const UDP_CLOSE_DRAIN_GRACE: Duration = Duration::from_secs(2);
const P2P_SIGNALING_INGRESS_BROKER_CAPACITY: usize = 64;
const P2P_MEMBERSHIP_BATCH_MAX_HINTS: usize = 4096;

enum P2pSignalingIngressItem {
    Single {
        message: BinaryMessage,
        relay: Arc<crate::p2p::session::MultiSession>,
    },
    MembershipBatch {
        messages: Vec<BinaryMessage>,
        authority: P2pMembershipBatchAuthority,
        v2_authority_required: bool,
    },
}

#[derive(Clone)]
struct P2pMembershipBatchAuthority {
    source_multi: Arc<crate::p2p::session::MultiSession>,
    client_id: String,
    transport_generation: u64,
}

#[derive(Clone)]
struct DeliveredP2pMembershipAuthority {
    delivery_sequence: u64,
    source: P2pMembershipBatchAuthority,
}

#[derive(Default)]
struct PendingP2pMembershipBatch {
    hints: Vec<BinaryMessage>,
    overflowed: bool,
}

#[derive(Default)]
struct NativeLanRouteGeneration {
    epoch: u64,
    bypass_ready: bool,
    inventory_ready: bool,
    exclusions: BTreeSet<std::net::Ipv4Addr>,
    connected_lans: Vec<crate::peer_runtime::LanExportPrefixV2>,
}

fn v2_lan_prefixes_overlap(
    left: crate::peer_runtime::LanExportPrefixV2,
    right: crate::peer_runtime::LanExportPrefixV2,
) -> bool {
    let bounds = |prefix: crate::peer_runtime::LanExportPrefixV2| {
        let first = u32::from(prefix.network);
        let mask = u32::MAX << (32 - prefix.prefix_len);
        (first, first | !mask)
    };
    let (left_first, left_last) = bounds(left);
    let (right_first, right_last) = bounds(right);
    left_first <= right_last && right_first <= left_last
}

fn v2_lan_prefix_children(
    prefix: crate::peer_runtime::LanExportPrefixV2,
) -> Option<[crate::peer_runtime::LanExportPrefixV2; 2]> {
    if prefix.prefix_len == 32 {
        return None;
    }
    let child_prefix_len = prefix.prefix_len + 1;
    let child_size = 1u32 << (32 - child_prefix_len);
    Some([
        crate::peer_runtime::LanExportPrefixV2 {
            network: prefix.network,
            prefix_len: child_prefix_len,
        },
        crate::peer_runtime::LanExportPrefixV2 {
            network: std::net::Ipv4Addr::from(u32::from(prefix.network) + child_size),
            prefix_len: child_prefix_len,
        },
    ])
}

fn v2_lan_prefix_without_hosts(
    prefix: crate::peer_runtime::LanExportPrefixV2,
    excluded: &BTreeSet<std::net::Ipv4Addr>,
) -> Vec<crate::peer_runtime::LanExportPrefixV2> {
    fn subtract_one(
        prefix: crate::peer_runtime::LanExportPrefixV2,
        address: std::net::Ipv4Addr,
        output: &mut Vec<crate::peer_runtime::LanExportPrefixV2>,
    ) {
        if !prefix.contains(address) {
            output.push(prefix);
            return;
        }
        let Some(children) = v2_lan_prefix_children(prefix) else {
            return;
        };
        for child in children {
            subtract_one(child, address, output);
        }
    }

    let mut remaining = vec![prefix];
    for address in excluded {
        let mut next = Vec::new();
        for candidate in remaining {
            subtract_one(candidate, *address, &mut next);
        }
        remaining = next;
    }
    remaining
}

fn v2_lan_prefix_without_prefixes(
    prefix: crate::peer_runtime::LanExportPrefixV2,
    excluded: &[crate::peer_runtime::LanExportPrefixV2],
) -> Vec<crate::peer_runtime::LanExportPrefixV2> {
    fn subtract_one(
        prefix: crate::peer_runtime::LanExportPrefixV2,
        excluded: crate::peer_runtime::LanExportPrefixV2,
        output: &mut Vec<crate::peer_runtime::LanExportPrefixV2>,
    ) {
        if !v2_lan_prefixes_overlap(prefix, excluded) {
            output.push(prefix);
            return;
        }
        if excluded.prefix_len <= prefix.prefix_len {
            return;
        }

        let Some(children) = v2_lan_prefix_children(prefix) else {
            return;
        };
        for child in children {
            subtract_one(child, excluded, output);
        }
    }

    let mut remaining = vec![prefix];
    for excluded in excluded {
        let mut next = Vec::new();
        for candidate in remaining {
            subtract_one(candidate, *excluded, &mut next);
        }
        remaining = next;
    }
    remaining
}

fn v2_lan_prefixes_prefer_over_connected(
    prefix: crate::peer_runtime::LanExportPrefixV2,
    connected_lans: &[crate::peer_runtime::LanExportPrefixV2],
) -> Vec<crate::peer_runtime::LanExportPrefixV2> {
    fn fragment_one(
        prefix: crate::peer_runtime::LanExportPrefixV2,
        connected: crate::peer_runtime::LanExportPrefixV2,
        output: &mut Vec<crate::peer_runtime::LanExportPrefixV2>,
    ) {
        if !v2_lan_prefixes_overlap(prefix, connected) || prefix.prefix_len > connected.prefix_len {
            output.push(prefix);
            return;
        }
        let Some(children) = v2_lan_prefix_children(prefix) else {
            return;
        };
        if prefix.prefix_len == connected.prefix_len {
            output.extend(children);
            return;
        }
        for child in children {
            fragment_one(child, connected, output);
        }
    }

    let mut routes = vec![prefix];
    for connected in connected_lans {
        let mut next = Vec::new();
        for route in routes {
            fragment_one(route, *connected, &mut next);
        }
        routes = next;
    }
    routes
}

#[derive(Default)]
struct LocalLanPublicationState {
    generation: u64,
    hosts: BTreeSet<std::net::Ipv4Addr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct P2pUnderlayGeneration(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct InboundLogicalTuple {
    route_kind: LocalRouteKind,
    protocol: Protocol,
    requested: SocketAddr,
}

struct RelayInboundAttestation {
    relay_generation: Weak<crate::p2p::session::MultiSession>,
    source_peer_id: String,
    logical_tuple: Option<InboundLogicalTuple>,
}

#[derive(Debug)]
struct InboundDialTarget {
    address: String,
    relay_local_authorized: bool,
    v2_access_authorized: bool,
}

#[derive(Debug)]
pub(crate) struct ResolvedProxyTarget {
    pub(crate) peer_id: Option<String>,
    pub(crate) logical_destination: Option<SocketAddr>,
    /// The destination was resolved through the V2 exact-Peer route table.
    /// Keep this decision stable through placement and OPEN even if a
    /// concurrent disconnect clears the active profile.
    pub(crate) v2_exact_target: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelayRouteBindKey {
    pub(crate) source_peer_id: String,
    pub(crate) target_peer_id: String,
    pub(crate) protocol: Protocol,
    pub(crate) logical_destination: SocketAddr,
}

pub(crate) struct RelayRouteBindPending {
    pub(crate) key: RelayRouteBindKey,
    pub(crate) relay_generation: Weak<crate::p2p::session::MultiSession>,
    pub(crate) response: oneshot::Sender<Result<(), String>>,
}

struct RelayInboundAttestationGuard {
    engine: Arc<Engine>,
    multi: Arc<crate::p2p::session::MultiSession>,
    conn_id: String,
    armed: bool,
}

impl RelayInboundAttestationGuard {
    fn new(
        engine: Arc<Engine>,
        multi: Arc<crate::p2p::session::MultiSession>,
        conn_id: String,
        armed: bool,
    ) -> Self {
        Self {
            engine,
            multi,
            conn_id,
            armed,
        }
    }
}

impl Drop for RelayInboundAttestationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.engine
                .remove_relay_inbound_attestation_for_generation(&self.conn_id, &self.multi);
            self.engine.v2_relay_flows.remove(&self.conn_id);
        }
    }
}

#[derive(Clone)]
struct LinkLivenessState {
    last_ack_ms: Arc<AtomicU64>,
    last_link_progress_ms: Arc<AtomicU64>,
    active_flows: Option<Arc<LinkActiveFlows>>,
}

impl LinkLivenessState {
    fn relay(
        last_ack_ms: Arc<AtomicU64>,
        last_link_progress_ms: Arc<AtomicU64>,
        active_flows: Arc<LinkActiveFlows>,
    ) -> Self {
        Self {
            last_ack_ms,
            last_link_progress_ms,
            active_flows: Some(active_flows),
        }
    }

    fn p2p(
        last_ack_ms: Arc<AtomicU64>,
        last_link_progress_ms: Arc<AtomicU64>,
        active_flows: Arc<LinkActiveFlows>,
    ) -> Self {
        Self {
            last_ack_ms,
            last_link_progress_ms,
            active_flows: Some(active_flows),
        }
    }

    fn record_link_progress(&self, now_ms: u64) {
        self.last_link_progress_ms.store(now_ms, Ordering::Relaxed);
    }

    fn record_ack(&self, now_ms: u64) {
        self.last_ack_ms.store(now_ms, Ordering::Relaxed);
    }

    fn begin_flow(&self, network: &str, conn_id: &str) -> Option<LinkActiveFlowGuard> {
        self.active_flows
            .as_ref()
            .and_then(|flows| flows.begin(network, conn_id))
    }

    fn end_flow(&self, conn_id: &str) {
        if let Some(flows) = self.active_flows.as_ref() {
            flows.end(conn_id);
        }
    }
}

#[derive(Clone, Copy)]
enum LinkActiveFlowKind {
    Tcp,
    Udp,
}

#[derive(Default)]
struct LinkActiveFlows {
    tcp: DashMap<String, ()>,
    udp: DashMap<String, ()>,
}

impl LinkActiveFlows {
    fn begin(self: &Arc<Self>, network: &str, conn_id: &str) -> Option<LinkActiveFlowGuard> {
        let kind = match network {
            "tcp" => LinkActiveFlowKind::Tcp,
            "udp" => LinkActiveFlowKind::Udp,
            _ => return None,
        };
        match kind {
            LinkActiveFlowKind::Tcp => {
                self.tcp.insert(conn_id.to_string(), ());
            }
            LinkActiveFlowKind::Udp => {
                self.udp.insert(conn_id.to_string(), ());
            }
        }
        Some(LinkActiveFlowGuard {
            flows: self.clone(),
            conn_id: conn_id.to_string(),
            kind,
        })
    }

    fn end(&self, conn_id: &str) {
        self.tcp.remove(conn_id);
        self.udp.remove(conn_id);
    }

    fn counts(&self) -> (usize, usize) {
        (self.tcp.len(), self.udp.len())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LinkActiveFlowSnapshot {
    active_tcp_flows: usize,
    active_udp_flows: usize,
    last_link_io_progress_ms: u64,
}

#[derive(Clone)]
struct LinkActiveFlowCounters {
    remote: Arc<LinkActiveFlows>,
    source: Option<Arc<dyn Fn() -> LinkActiveFlowSnapshot + Send + Sync>>,
}

impl LinkActiveFlowCounters {
    #[cfg(test)]
    fn new(remote: Arc<LinkActiveFlows>) -> Self {
        Self {
            remote,
            source: None,
        }
    }

    fn with_source(
        remote: Arc<LinkActiveFlows>,
        source: Arc<dyn Fn() -> LinkActiveFlowSnapshot + Send + Sync>,
    ) -> Self {
        Self {
            remote,
            source: Some(source),
        }
    }

    fn snapshot(&self) -> LinkActiveFlowSnapshot {
        let (tcp, udp) = self.remote.counts();
        let mut snapshot = LinkActiveFlowSnapshot {
            active_tcp_flows: tcp,
            active_udp_flows: udp,
            last_link_io_progress_ms: 0,
        };
        if let Some(source) = self.source.as_ref() {
            let source = source();
            snapshot.active_tcp_flows = snapshot
                .active_tcp_flows
                .saturating_add(source.active_tcp_flows);
            snapshot.active_udp_flows = snapshot
                .active_udp_flows
                .saturating_add(source.active_udp_flows);
            snapshot.last_link_io_progress_ms = snapshot
                .last_link_io_progress_ms
                .max(source.last_link_io_progress_ms);
        }
        snapshot
    }
}

struct LinkActiveFlowGuard {
    flows: Arc<LinkActiveFlows>,
    conn_id: String,
    kind: LinkActiveFlowKind,
}

#[derive(Default)]
struct TcpFlowLinkContext {
    p2p_source_session: Option<Arc<tp_transport::session::Session>>,
    link_progress_ms: Option<Arc<AtomicU64>>,
    link_active_flow: Option<LinkActiveFlowGuard>,
}

impl Drop for LinkActiveFlowGuard {
    fn drop(&mut self) {
        match self.kind {
            LinkActiveFlowKind::Tcp => {
                self.flows.tcp.remove(&self.conn_id);
            }
            LinkActiveFlowKind::Udp => {
                self.flows.udp.remove(&self.conn_id);
            }
        }
    }
}

fn schedule_udp_inbound_close_grace(
    conn_id: String,
    udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    multi: Arc<crate::p2p::session::MultiSession>,
    tasks: &TaskTracker,
) {
    tasks.spawn(async move {
        sleep(UDP_CLOSE_DRAIN_GRACE).await;
        udp_inbound.remove(&conn_id);
        multi.clear_pending_udp_inbound(&conn_id);
    });
}

async fn copy_one_direction_with_progress<R, W, FR, FW>(
    reader: &mut R,
    writer: &mut W,
    on_read: &mut FR,
    on_write: &mut FW,
) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    FR: FnMut(usize),
    FW: FnMut(usize),
{
    let mut buf = [0u8; TCP_FLOW_COPY_BUFFER_BYTES];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            writer.shutdown().await?;
            return Ok(total);
        }
        on_read(n);
        writer.write_all(&buf[..n]).await?;
        total = total.saturating_add(n as u64);
        on_write(n);
    }
}

async fn copy_bidirectional_with_progress<A, B, FLR, FLW, FRR, FRW>(
    left: &mut A,
    right: &mut B,
    mut left_to_right_read: FLR,
    mut left_to_right_written: FLW,
    mut right_to_left_read: FRR,
    mut right_to_left_written: FRW,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
    FLR: FnMut(usize),
    FLW: FnMut(usize),
    FRR: FnMut(usize),
    FRW: FnMut(usize),
{
    let (mut left_reader, mut left_writer) = tokio::io::split(left);
    let (mut right_reader, mut right_writer) = tokio::io::split(right);
    tokio::try_join!(
        copy_one_direction_with_progress(
            &mut left_reader,
            &mut right_writer,
            &mut left_to_right_read,
            &mut left_to_right_written,
        ),
        copy_one_direction_with_progress(
            &mut right_reader,
            &mut left_writer,
            &mut right_to_left_read,
            &mut right_to_left_written,
        ),
    )
}

async fn copy_v2_relay_tcp_flow(
    stream: tp_transport::TcpFlowStream,
    target: TcpStream,
    flow: V2RelayFlowCryptoContext,
    conn_id: [u8; 12],
    multi: Arc<crate::p2p::session::MultiSession>,
    link_progress_ms: Option<Arc<AtomicU64>>,
) -> Result<(), String> {
    let inbound_aad = crate::relay_crypto::RelayAadV2::flow(
        flow.record_context(&conn_id, false),
        crate::relay_crypto::RelayFlowKindV2::Data,
    )
    .map_err(|error| error.to_string())?;
    let outbound_aad = crate::relay_crypto::RelayAadV2::flow(
        flow.record_context(&conn_id, true),
        crate::relay_crypto::RelayFlowKindV2::Data,
    )
    .map_err(|error| error.to_string())?;
    let (mut flow_read, mut flow_write) = tokio::io::split(stream);
    let (mut target_read, mut target_write) = target.into_split();
    let inbound_flow = flow.clone();
    let inbound_progress = link_progress_ms.clone();
    let inbound_multi = multi.clone();
    let peer_to_target = async move {
        let mut record = BytesMut::with_capacity(
            crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2
                + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
        );
        loop {
            match tp_transport::session::read_tcp_flow_frame_into_bytes(&mut flow_read, &mut record)
                .await
            {
                Ok(()) => {}
                Err(tp_transport::TransportError::Other(_)) => break,
                Err(error) => return Err(error.to_string()),
            }
            inbound_flow
                .cipher
                .open_precomputed(&inbound_aad, &mut record)
                .map_err(|error| error.to_string())?;
            target_write
                .write_all(&record)
                .await
                .map_err(|error| error.to_string())?;
            inbound_multi.record_traffic_rx(TrafficPath::Relay, record.len());
            if let Some(progress) = &inbound_progress {
                progress.store(monotonic_millis(), Ordering::Relaxed);
            }
        }
        target_write
            .shutdown()
            .await
            .map_err(|error| error.to_string())
    };

    let outbound_flow = flow;
    let outbound_progress = link_progress_ms;
    let target_to_peer = async move {
        let mut sealed = BytesMut::with_capacity(
            crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2
                + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
        );
        loop {
            sealed.clear();
            sealed.reserve(
                crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2
                    + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
            );
            sealed.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
            let read = (&mut target_read)
                .take(crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2 as u64)
                .read_buf(&mut sealed)
                .await
                .map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            outbound_flow
                .cipher
                .seal_precomputed(&outbound_aad, &mut sealed)
                .map_err(|error| error.to_string())?;
            tp_transport::session::write_tcp_flow_frame(&mut flow_write, &sealed)
                .await
                .map_err(|error| error.to_string())?;
            multi.record_traffic_tx(PathKind::Relay, i64::try_from(read).unwrap_or(i64::MAX));
            if let Some(progress) = &outbound_progress {
                progress.store(monotonic_millis(), Ordering::Relaxed);
            }
        }
        flow_write
            .shutdown()
            .await
            .map_err(|error| error.to_string())
    };

    tokio::try_join!(peer_to_target, target_to_peer)?;
    Ok(())
}

fn transport_dial_timeout() -> Duration {
    Duration::from_secs(30)
}

fn replica_dial_stagger() -> Duration {
    if let Ok(raw) = std::env::var("TUNNEL_PROXY_REPLICA_DIAL_STAGGER_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return Duration::from_millis(ms);
        }
    }

    #[cfg(target_os = "macos")]
    {
        Duration::from_millis(500)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Duration::ZERO
    }
}

fn status_refresh_interval() -> Duration {
    Duration::from_secs(1)
}

#[allow(dead_code)] // consumed by the Relay AEAD wiring slice
#[derive(Clone)]
pub(crate) struct V2PeerLinkCryptoContext {
    pub(crate) session_id: tp_core::p2p_types::SessionId,
    pub(crate) remote_peer_id: String,
    pub(crate) cipher: Arc<crate::relay_crypto::RelayCipherV2>,
}

#[derive(Clone)]
struct V2RelayFlowCryptoContext {
    tunnel_id: String,
    session_id: tp_core::p2p_types::SessionId,
    local_peer_id: String,
    remote_peer_id: String,
    cipher: Arc<crate::relay_crypto::RelayCipherV2>,
    inbound_framed_aad: Option<(
        crate::relay_crypto::RelayAadV2,
        crate::relay_crypto::RelayAadV2,
    )>,
}

impl V2RelayFlowCryptoContext {
    fn seal_context(&self, conn_id: &str) -> Option<crate::p2p::multi_sender::V2RelaySealContext> {
        crate::p2p::multi_sender::V2RelaySealContext::new(
            self.tunnel_id.clone(),
            self.session_id,
            self.local_peer_id.clone(),
            self.remote_peer_id.clone(),
            relay_conn_id_to_wire_v2(conn_id)?,
            self.cipher.clone(),
        )
        .ok()
    }

    fn record_context<'a>(
        &'a self,
        conn_id: &'a [u8; 12],
        outbound: bool,
    ) -> crate::relay_crypto::RelayRecordContextV2<'a> {
        let (source_peer_id, target_peer_id) = if outbound {
            (&self.local_peer_id, &self.remote_peer_id)
        } else {
            (&self.remote_peer_id, &self.local_peer_id)
        };
        crate::relay_crypto::RelayRecordContextV2 {
            tunnel_id: &self.tunnel_id,
            peerlink_session_id: &self.session_id,
            source_peer_id,
            target_peer_id,
            conn_id,
        }
    }

    fn with_inbound_framed_aad(mut self, conn_id: &[u8; 12]) -> Option<Self> {
        let data = crate::relay_crypto::RelayAadV2::framed(
            self.record_context(conn_id, false),
            crate::relay_crypto::RelayFramedKindV2::Data,
        )
        .ok()?;
        let udp = crate::relay_crypto::RelayAadV2::framed(
            self.record_context(conn_id, false),
            crate::relay_crypto::RelayFramedKindV2::UdpData,
        )
        .ok()?;
        self.inbound_framed_aad = Some((data, udp));
        Some(self)
    }

    fn inbound_framed_aad(
        &self,
        kind: crate::relay_crypto::RelayFramedKindV2,
    ) -> Option<&crate::relay_crypto::RelayAadV2> {
        let (data, udp) = self.inbound_framed_aad.as_ref()?;
        Some(match kind {
            crate::relay_crypto::RelayFramedKindV2::Data => data,
            crate::relay_crypto::RelayFramedKindV2::UdpData => udp,
        })
    }
}

pub struct Engine {
    cfg: EngineConfig,
    listener: Arc<dyn StatusListener>,
    managed_gateway_resolver: Arc<dyn ManagedGatewayResolver>,
    managed_peer_heartbeat_sender: Arc<dyn ManagedPeerHeartbeatSender>,
    state: RwLock<ConnectionStatus>,
    connected_since: parking_lot::Mutex<Option<Instant>>,
    stop_tx: RwLock<Option<mpsc::Sender<()>>>,
    latest_tunnel_config: RwLock<Option<TunnelConfig>>,
    active_v2_profile: RwLock<Option<Arc<PeerProfileV2>>>,
    /// One secret-free V2 runtime read model for UI/headless status. Runtime
    /// events update this narrow lock; payload forwarding never takes it.
    v2_runtime: RwLock<crate::runtime_snapshot::V2RuntimeSnapshot>,
    /// Lane, membership, and Gossip events arrive on independent tasks. Keep
    /// their derived route/read-model commit ordered so an older observation
    /// cannot overwrite a newer registry mutation.
    v2_runtime_reconcile_lock: Arc<parking_lot::Mutex<()>>,
    /// UDP mapping port reported by the Gateway currently being attached. Zero
    /// until a Gateway says otherwise; P2P bootstrap probes that port instead
    /// of assuming every host reflects on the same one.
    managed_mapping_port: AtomicU16,
    /// P2P anchor `MultiSession`. In a multi-replica relay group this is the
    /// primary replica, not a round-robin data-plane slot.
    multi: parking_lot::Mutex<Option<Arc<crate::p2p::session::MultiSession>>>,
    /// Live relay replicas available to source-side local proxy traffic.
    /// `ProxyTunnelOpener` picks from this list round-robin so app/mobile
    /// SOCKS5 can use the same capacity model as gateway-side SOCKS5.
    replica_sessions: parking_lot::Mutex<Vec<ReplicaMultiSession>>,
    /// Serializes P2P install versus replica unregister so a direct session
    /// cannot be installed onto a replica while it is being unpublished.
    p2p_session_registry_lock: parking_lot::Mutex<()>,
    proxy_replica_rr: AtomicUsize,
    proxy_flow_placement_lock: parking_lot::Mutex<()>,
    proxy_flow_scheduler: ReplicaFlowScheduler,
    proxy_flow_registry: FlowPlacementRegistry,
    /// Sparse tunnel Overlay `/32` ownership. This resolves a destination to
    /// one logical Peer before any P2P/relay lane is considered.
    overlay_routes: RwLock<crate::route_matcher::OverlayRouteMatcher>,
    /// Current V2 PeerLink cipher for each stable remote Peer. Re-handshake
    /// replaces this entry; already-open Flows keep their cloned cipher `Arc`.
    v2_peer_links: DashMap<String, V2PeerLinkCryptoContext>,
    /// Exact Relay Flow crypto, installed only after a bound route consumes a
    /// sealed OPEN. It is endpoint-local and removed with the existing Flow.
    v2_relay_flows: DashMap<String, V2RelayFlowCryptoContext>,
    /// Pairwise origin-only Runtime Gossip for authenticated V2 PeerLinks.
    v2_peer_gossip: parking_lot::Mutex<Option<crate::peer_gossip::PeerGossipControllerV2>>,
    /// Stable Peers named by the latest complete authenticated Gateway
    /// membership cycle. This is local Relay reachability input, not a
    /// persistent membership database or a wire-level state machine.
    v2_current_membership: RwLock<BTreeSet<String>>,
    /// Distinguishes a valid empty Tunnel view from startup before the first
    /// complete Hint...Ack cycle.
    v2_membership_cycle_complete: AtomicBool,
    /// The local record is retained independently so settings saved before a
    /// PeerLink exists become the first full sync instead of being lost.
    v2_local_runtime_record: RwLock<crate::peer_runtime::PeerRuntimeRecordV2>,
    /// The owner's LAN Export answer. The published record above is derived
    /// from this and the live interface list on every scan, so a machine that
    /// moves to another network publishes the one it is on now instead of the
    /// one it was on when its settings were saved.
    v2_local_lan_export_config: RwLock<crate::peer_runtime::LocalLanExportConfigV2>,
    /// Bumped on every install, so a watchdog pass can tell that the answer it
    /// scanned for is no longer the current one.
    v2_local_lan_export_generation: AtomicU64,
    /// One target-side V2 policy. Missing or invalid persisted policy is
    /// represented by the deny-all value, never by an implicit allow.
    v2_access_policy: RwLock<crate::access_policy::CompiledClientAccessPolicyV2>,
    p2p_install_rr: AtomicUsize,
    p2p_pending_installs:
        parking_lot::Mutex<HashMap<tp_core::p2p_types::SessionId, PendingP2pInstall>>,
    link_refill_limiter: Arc<LinkRefillLimiter>,
    relay_transport_generations: DashMap<String, AtomicU64>,
    p2p_refill_handle: parking_lot::Mutex<Option<crate::p2p::manager::P2pRefillHandle>>,
    #[cfg(test)]
    p2p_refill_requests: DashMap<String, AtomicUsize>,
    /// Platform primary client_id used as the process-wide P2P manager anchor.
    /// Sidecar direct sessions still bind to their own replica `MultiSession`.
    p2p_anchor_client_id: parking_lot::Mutex<Option<String>>,
    /// Non-blocking relay-reader ingress for the single broker that serializes
    /// all P2P signaling into the bounded `P2pManager` channel. A broker item
    /// is either one ordinary signaling message or one complete membership
    /// Hint...Ack transaction.
    p2p_signaling_ingress_tx: parking_lot::Mutex<Option<mpsc::Sender<P2pSignalingIngressItem>>>,
    /// Membership hints form one relay-scoped transaction terminated by
    /// `P2pAnnounceAck`. Keeping incomplete transactions here prevents a
    /// disconnected replica's hints from leaking into a later relay cycle.
    /// Access is serialized by `p2p_session_registry_lock` so unregister and
    /// inbound delivery share one publication boundary.
    p2p_pending_membership_batches: parking_lot::Mutex<HashMap<usize, PendingP2pMembershipBatch>>,
    /// Relay-generation authorities delivered by the signaling broker, in
    /// the same FIFO order as the membership Ack messages consumed by the
    /// process-wide P2P manager. The manager consumes one token per complete
    /// V2 Hint...Ack transaction before it may commit that cycle.
    p2p_delivered_membership_authorities:
        parking_lot::Mutex<VecDeque<DeliveredP2pMembershipAuthority>>,
    /// Relay replica that delivered each in-flight P2P signaling session.
    /// The `P2pManager` is process-wide, but gateway `P2pAnswer` validation is
    /// per authenticated relay connection. Sidecar offers must therefore answer
    /// on the same replica relay that received the offer.
    p2p_signaling_routes:
        Arc<DashMap<tp_core::p2p_types::SessionId, Arc<crate::p2p::session::MultiSession>>>,
    /// Shared expected-peer-fingerprint slot for the P2P listener
    /// (Task 4.8). Forwarded into `P2pManager::set_expected_fp_handle`
    /// at startup by Task 4.11; held here so the engine can hand it to
    /// the manager construction site.
    p2p_expected_fp: parking_lot::Mutex<
        Option<Arc<std::sync::Mutex<Option<tp_core::p2p_types::CertFingerprint>>>>,
    >,
    /// Resolved `(client_id, group_id)` for the current replica, populated
    /// after `run_replica` connects. Cleared on teardown. Task 4.11 reads
    /// this to construct the P2P announce/offer payloads from the same
    /// values negotiated with the platform.
    tunnel_identity: parking_lot::Mutex<Option<(String, String)>>,
    /// Optional metrics sink (Task 4.12). `None` = no-op. Currently only
    /// used by the `Connect`-dedup arm in `handle_msg`. Wired by
    /// `apps/lantunnel-client/src-tauri/src/main.rs` when P2P is enabled.
    metrics: parking_lot::Mutex<Option<Arc<tp_metrics::MetricsManager>>>,
    /// Per-engine data-plane counters surfaced through [`ConnectionStatus`].
    /// Reset on explicit connect/disconnect, retained across gateway retry
    /// loops inside one app/client run.
    traffic: Arc<TrafficCounters>,
    /// Replica fanout for the active V2 Gateway Attachment.
    replicas: parking_lot::Mutex<Option<usize>>,
    /// P2P tuning for the active Peer. `None` uses
    /// `ClientP2pConfig::default()`.
    p2p_config: parking_lot::Mutex<Option<Arc<tp_core::config::ClientP2pConfig>>>,
    /// Epoch-scoped connected-LAN inventory and exact infrastructure
    /// exclusions for the current P2P underlay generation. Keeping these in
    /// one lock prevents native capture from mixing facts across reconnects.
    native_lan_route_generation: RwLock<NativeLanRouteGeneration>,
    /// Current authoritative local RFC1918 host inventory used both for the
    /// Platform publication and target-side service ownership checks. This is
    /// deliberately independent of P2P underlay readiness: losing a Direct
    /// Path withdraws native TUN capture, not the Peer's stable route alias
    /// publication or explicit SOCKS semantics.
    local_lan_publication: RwLock<LocalLanPublicationState>,
    /// Explicit local delivery policy. Empty is intentionally meaningful:
    /// owned Overlay/LAN destinations are then denied instead of being
    /// rewritten to loopback.
    local_service_exports: RwLock<Vec<LocalServiceExport>>,
    /// Tunnel/group access-policy context compiled from the live config.
    /// P2P receive pumps clone this because installed direct-session readers
    /// can receive `Connect` and must enforce the same access policy as relay.
    group_context: parking_lot::Mutex<Option<Arc<TunnelGroupContext>>>,
    /// Source-side local proxy requests waiting for a `ConnectResponse`.
    /// A local proxy inserts a oneshot before emitting `Connect` through the
    /// live `MultiSession`; target-only runtimes leave it empty.
    proxy_pending: Arc<DashMap<String, oneshot::Sender<Result<(), String>>>>,
    /// Relay route binds waiting for a gateway-side route-ready ack.
    relay_route_bind_pending: Arc<DashMap<String, RelayRouteBindPending>>,
    /// Target-side gateway attestations. Each entry is scoped to the exact
    /// relay `MultiSession` generation that delivered it; later Connect/flow
    /// handling consumes the source principal from here instead of trusting
    /// any client-authored data-plane payload.
    relay_inbound_attestations: DashMap<String, RelayInboundAttestation>,
    /// Engine-lifetime [`TaskTracker`]. Spawns whose lifetime
    /// should match `Engine` shutdown — the Gateway Attachment driver, the long-lived
    /// P2P signaling forwarder installed by `attach_p2p_signaling`, plus
    /// any external bootstrap tasks routed through [`Engine::tasks`] —
    /// register here so [`Engine::disconnect`] can drain them before
    /// returning. Wrapped in `RwLock` because `disconnect` swaps in a
    /// fresh tracker after each drain (closed trackers reject the
    /// next `connect` cycle's spawn semantics, see `disconnect`).
    tasks: RwLock<TaskTracker>,
    /// Engine-lifetime cancellation token paired with [`Engine::tasks`].
    /// Long-lived tasks should capture a clone when they are spawned and
    /// cooperatively exit when `disconnect` cancels it. `disconnect` replaces
    /// the token before draining the tracker so the next connect cycle gets a
    /// fresh, uncancelled token.
    task_cancel: RwLock<CancellationToken>,
    /// Abort handles for engine-owned tracked tasks. `TaskTracker` gives us
    /// the drain point; these handles give the timeout path a way to stop the
    /// old generation instead of detaching it forever.
    task_abort_handles: parking_lot::Mutex<Vec<AbortHandle>>,
}

#[async_trait]
trait ManagedGatewayResolver: Send + Sync {
    async fn resolve(&self, profile: &PeerProfileV2) -> anyhow::Result<GatewayBootstrapV2>;
}

struct PlatformManagedGatewayResolver;

#[async_trait]
impl ManagedGatewayResolver for PlatformManagedGatewayResolver {
    async fn resolve(&self, profile: &PeerProfileV2) -> anyhow::Result<GatewayBootstrapV2> {
        crate::managed_resolve::resolve_managed_gateway(profile).await
    }
}

#[async_trait]
trait ManagedPeerHeartbeatSender: Send + Sync {
    async fn send(
        &self,
        platform_url: &str,
        request: &crate::peer_heartbeat::PeerHeartbeatRequest,
    ) -> Result<
        Option<crate::peer_heartbeat::PeerRelayUsage>,
        crate::peer_heartbeat::PeerHeartbeatSendError,
    >;
}

struct PlatformManagedPeerHeartbeatSender {
    client: crate::peer_heartbeat::PeerHeartbeatClient,
}

#[async_trait]
impl ManagedPeerHeartbeatSender for PlatformManagedPeerHeartbeatSender {
    async fn send(
        &self,
        platform_url: &str,
        request: &crate::peer_heartbeat::PeerHeartbeatRequest,
    ) -> Result<
        Option<crate::peer_heartbeat::PeerRelayUsage>,
        crate::peer_heartbeat::PeerHeartbeatSendError,
    > {
        // The answer carries how much of the Tunnel's Relay allowance is gone.
        // It was dropped here, so nothing downstream could show it.
        self.client
            .post(platform_url, request)
            .await
            .map(|response| response.relay_usage)
    }
}

#[derive(Clone)]
struct ReplicaMultiSession {
    client_id: String,
    multi: Arc<crate::p2p::session::MultiSession>,
    relay_active: bool,
    relay_accepts_new_flows: bool,
    transport_generation: u64,
}

#[derive(Clone)]
#[cfg(test)]
pub(crate) struct ProxyLane {
    pub(crate) local_client_id: String,
    pub(crate) multi: Arc<crate::p2p::session::MultiSession>,
}

#[derive(Clone)]
pub(crate) struct ProxyFlowLane {
    pub(crate) local_client_id: String,
    pub(crate) multi: Arc<crate::p2p::session::MultiSession>,
    pub(crate) path: crate::p2p::scheduler::PathKind,
    pub(crate) p2p_session_id: Option<tp_core::p2p_types::SessionId>,
    pub(crate) p2p_session: Option<Arc<tp_transport::session::Session>>,
    /// Exact remote Replica for this local lane when an Overlay destination
    /// selected a logical Peer. RelayRouteBind consumes this value before a
    /// gateway-routed Connect is sent.
    pub(crate) target_peer_client_id: Option<String>,
    /// The exact target was selected while a V2 Peer profile was active.
    /// This pins fail-closed routing semantics across concurrent disconnects.
    pub(crate) v2_exact_target: bool,
    pub(crate) candidate_key: CandidateKey,
}

#[derive(Clone)]
pub(crate) enum ProxyFlowAttemptExclude {
    Lane {
        local_client_id: String,
        path: crate::p2p::scheduler::PathKind,
        p2p_session_id: Option<tp_core::p2p_types::SessionId>,
    },
    Path {
        path: crate::p2p::scheduler::PathKind,
    },
}

impl ProxyFlowLane {
    pub(crate) fn attempt_exclude(&self) -> ProxyFlowAttemptExclude {
        ProxyFlowAttemptExclude::Lane {
            local_client_id: self.local_client_id.clone(),
            path: self.path,
            p2p_session_id: self.p2p_session_id,
        }
    }
}

impl ProxyFlowAttemptExclude {
    pub(crate) fn path(path: crate::p2p::scheduler::PathKind) -> Self {
        Self::Path { path }
    }

    fn matches(&self, lane: &ProxyFlowLane) -> bool {
        match self {
            Self::Lane {
                local_client_id,
                path,
                p2p_session_id,
            } => {
                local_client_id == &lane.local_client_id
                    && *path == lane.path
                    && *p2p_session_id == lane.p2p_session_id
            }
            Self::Path { path } => *path == lane.path,
        }
    }
}

struct PendingP2pInstall {
    multi: Arc<crate::p2p::session::MultiSession>,
    peer_client_id: String,
    relation_key: Option<PeerRelationKey>,
    refill_permit: Option<LinkRefillPermit>,
}

pub(crate) struct HostFilter {
    inner: RwLock<CompiledHostFilter>,
}

impl HostFilter {
    fn new(forbidden: &[String], allowed: &[String]) -> crate::Result<Self> {
        Ok(Self {
            inner: RwLock::new(CompiledHostFilter::new(forbidden, allowed)?),
        })
    }

    fn is_allowed(&self, address: &str) -> bool {
        self.inner.read().is_allowed(address)
    }
}

#[derive(Clone)]
#[allow(dead_code)]
struct TunnelGroupContext {
    tunnel_id: String,
    group_id: String,
    anchor_client_id: Option<String>,
    host_filter: Arc<HostFilter>,
}

impl TunnelGroupContext {
    fn from_config(
        tc: &TunnelConfig,
        anchor_client_id: Option<String>,
        host_filter: Arc<HostFilter>,
    ) -> Self {
        Self {
            tunnel_id: tc.tunnel_id.clone(),
            group_id: tc.group_id.clone(),
            anchor_client_id,
            host_filter,
        }
    }
}

fn close_multi_p2p_session_if_active(
    multi: &Arc<crate::p2p::session::MultiSession>,
    session_id: tp_core::p2p_types::SessionId,
) -> bool {
    if !multi.close_p2p_session(session_id) {
        return false;
    }
    if multi.p2p_session_count() == 0 && multi_p2p_state_matches_active_session(multi, session_id) {
        multi.set_state(crate::p2p::session::P2pState::Idle);
    }
    true
}

fn multi_p2p_session_is_active(
    multi: &Arc<crate::p2p::session::MultiSession>,
    session_id: tp_core::p2p_types::SessionId,
) -> bool {
    multi.has_p2p_session(session_id)
}

fn multi_p2p_state_matches_active_session(
    multi: &Arc<crate::p2p::session::MultiSession>,
    session_id: tp_core::p2p_types::SessionId,
) -> bool {
    matches!(
        multi.p2p_state(),
        crate::p2p::session::P2pState::Active {
            session_id: active,
            ..
        } if active == session_id
    )
}

fn p2p_state_status_label(state: &crate::p2p::session::P2pState) -> &'static str {
    match state {
        crate::p2p::session::P2pState::Disabled => "disabled",
        crate::p2p::session::P2pState::Idle => "idle",
        crate::p2p::session::P2pState::Announcing => "announcing",
        crate::p2p::session::P2pState::Negotiating { .. } => "negotiating",
        crate::p2p::session::P2pState::Punching { .. } => "punching",
        crate::p2p::session::P2pState::HandshakingQuic { .. } => "handshaking_quic",
        crate::p2p::session::P2pState::Active { .. } => "active",
        crate::p2p::session::P2pState::Cooldown { .. } => "cooldown",
    }
}

fn p2p_signaling_session_id(msg: &BinaryMessage) -> Option<tp_core::p2p_types::SessionId> {
    match msg {
        BinaryMessage::P2pOffer { session_id, .. }
        | BinaryMessage::P2pAnswer { session_id, .. }
        | BinaryMessage::P2pPunchSync { session_id, .. }
        | BinaryMessage::P2pSessionReady { session_id, .. }
        | BinaryMessage::P2pTeardown { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

fn p2p_relay_instance_key(multi: &Arc<crate::p2p::session::MultiSession>) -> usize {
    Arc::as_ptr(multi) as usize
}

fn unique_p2p_multis_from(
    replica_sessions: &[ReplicaMultiSession],
    anchor: Option<Arc<crate::p2p::session::MultiSession>>,
) -> Vec<Arc<crate::p2p::session::MultiSession>> {
    let mut p2p_multis: Vec<Arc<crate::p2p::session::MultiSession>> = Vec::new();
    for entry in replica_sessions {
        if !p2p_multis
            .iter()
            .any(|existing| Arc::ptr_eq(existing, &entry.multi))
        {
            p2p_multis.push(entry.multi.clone());
        }
    }
    if let Some(anchor) = anchor {
        if !p2p_multis
            .iter()
            .any(|existing| Arc::ptr_eq(existing, &anchor))
        {
            p2p_multis.push(anchor);
        }
    }
    p2p_multis
}

fn p2p_installed_session_count_in(multis: &[Arc<crate::p2p::session::MultiSession>]) -> usize {
    multis.iter().map(|multi| multi.p2p_session_count()).sum()
}

fn p2p_eligible_session_count_in(multis: &[Arc<crate::p2p::session::MultiSession>]) -> usize {
    multis
        .iter()
        .map(|multi| multi.p2p_eligible_session_count())
        .sum()
}

fn p2p_eligible_peer_ids_in(multis: &[Arc<crate::p2p::session::MultiSession>]) -> Vec<String> {
    let mut peers = Vec::new();
    for multi in multis {
        for peer in multi.p2p_eligible_peer_ids() {
            if !peers.iter().any(|existing| existing == &peer) {
                peers.push(peer);
            }
        }
    }
    peers
}

#[derive(Clone)]
enum ReplicaTransport {
    Quic {
        client: Arc<QuicClient>,
        candidates: Arc<Vec<QuicTransportCandidate>>,
    },
    WebSocket {
        candidates: Arc<Vec<WebSocketTransportCandidate>>,
        tls_config: Option<Arc<rustls::ClientConfig>>,
    },
    Grpc {
        candidates: Arc<Vec<GrpcTransportCandidate>>,
        insecure_tls: bool,
    },
}

#[derive(Clone)]
struct GatewayDialCandidate {
    gateway_addr: String,
    gateway_port: u16,
    tls_server_name: Option<String>,
    force_tls: bool,
}

#[derive(Clone)]
struct QuicTransportCandidate {
    gateway_addr: String,
    gateway_port: u16,
    server_name: String,
}

#[derive(Clone)]
struct WebSocketTransportCandidate {
    gateway_addr: String,
    gateway_port: u16,
    url: String,
    tls_server_name: Option<String>,
}

#[derive(Clone)]
struct GrpcTransportCandidate {
    gateway_addr: String,
    gateway_port: u16,
    url: String,
    tls_domain: Option<String>,
    ca_pem: Option<Vec<u8>>,
    exact_leaf_pem: Option<Vec<u8>>,
}

struct ConnectedTransport {
    session: Session,
    gateway_addr: SocketAddr,
}

#[derive(Clone)]
struct V2GatewayAttachment {
    profile: Arc<PeerProfileV2>,
    gateway: GatewayBootstrapV2,
}

#[derive(Clone)]
struct GatewayAttachmentSource {
    profile: Arc<PeerProfileV2>,
    static_gateway_override: Option<GatewayBootstrapV2>,
    runtime_replica_ids: Vec<String>,
}

impl GatewayAttachmentSource {
    fn new(
        profile: Arc<PeerProfileV2>,
        static_gateway_override: Option<GatewayBootstrapV2>,
    ) -> Self {
        let runtime_replica_ids = crate::v2_attachment::runtime_replica_ids(&profile);
        Self {
            profile,
            static_gateway_override,
            runtime_replica_ids,
        }
    }
}

struct GatewayAttachmentGeneration {
    tunnel_config: TunnelConfig,
    attachment: V2GatewayAttachment,
}

#[derive(Clone)]
struct ReplicaActivity {
    total: usize,
    active: Arc<AtomicUsize>,
    gateway_name: Option<String>,
}

impl ReplicaActivity {
    fn new(total: usize, gateway_name: Option<String>) -> Self {
        Self {
            total: total.max(1),
            active: Arc::new(AtomicUsize::new(0)),
            gateway_name,
        }
    }

    fn mark_connected(&self, engine: &Engine, gateway_addr: SocketAddr) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let mut s = engine.state.read().clone();
        s.connected = true;
        s.connecting = false;
        s.gateway_name = self.gateway_name.clone();
        s.gateway_addr = Some(gateway_addr.to_string());
        s.message = replica_connected_message(active, self.total);
        s.transport_heartbeat = HeartbeatStatus {
            active: true,
            last_time: Some(unix_now()),
            last_error: None,
        };
        s.error = None;
        engine.set_status(s);
        engine.mark_v2_gateway_attached(gateway_addr);
    }

    fn mark_disconnected(&self, engine: &Engine, parent_cancelled: bool) {
        let active = self
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .map(|previous| previous.saturating_sub(1))
            .unwrap_or(0);
        let mut s = engine.state.read().clone();
        if active == 0 {
            s.connected = false;
            s.connecting = false;
            if !parent_cancelled {
                s.message = "Gateway disconnected".into();
            }
            s.transport_heartbeat.active = false;
        } else {
            s.connected = true;
            s.connecting = false;
            s.message = replica_connected_message(active, self.total);
        }
        engine.set_status(s);
        if active == 0 && !parent_cancelled {
            engine.mark_v2_gateway_disconnected();
        }
    }

    fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

fn replica_connected_message(active: usize, total: usize) -> String {
    let total = total.max(1);
    if total == 1 {
        "Connected".into()
    } else if active >= total {
        format!("Connected ({total} replicas)")
    } else {
        format!("Connected ({active}/{total} replicas)")
    }
}

fn effective_gateway_name(tc: &TunnelConfig) -> Option<String> {
    tc.gateway_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn v2_tunnel_config(
    profile: &PeerProfileV2,
    gateway: &GatewayBootstrapV2,
    client_ids: Vec<String>,
) -> TunnelConfig {
    TunnelConfig {
        tunnel_id: profile.tunnel_id.clone(),
        gateway_addr: gateway.dial_address.clone(),
        gateway_port: gateway.port,
        transport_type: gateway.transport.clone(),
        tls_cert: gateway.trusted_certificate_pem.clone().unwrap_or_default(),
        peer_id: profile.peer.peer_id.clone(),
        overlay_ipv4: profile.peer.overlay_ip.to_string(),
        client_id: client_ids.first().cloned().unwrap_or_default(),
        replicas: client_ids.len() as u32,
        client_ids,
        ..Default::default()
    }
}

fn monotonic_millis() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn unix_timestamp_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Copy, Debug)]
struct ReplicaReconnectPolicy {
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl ReplicaReconnectPolicy {
    fn new(initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            initial_backoff,
            max_backoff,
        }
    }

    fn production() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30))
    }
}

#[derive(Clone, Debug)]
struct ReplicaReconnectGroup {
    total: usize,
    reported_down: Arc<AtomicUsize>,
    all_down: CancellationToken,
}

impl ReplicaReconnectGroup {
    fn new(total: usize) -> Self {
        Self {
            total: total.max(1),
            reported_down: Arc::new(AtomicUsize::new(0)),
            all_down: CancellationToken::new(),
        }
    }

    fn report_failure(&self, already_reported_down: &mut bool, active_count: usize) -> bool {
        let down = if *already_reported_down {
            self.reported_down.load(Ordering::SeqCst)
        } else {
            *already_reported_down = true;
            self.reported_down.fetch_add(1, Ordering::SeqCst) + 1
        };

        if active_count == 0 && down >= self.total {
            self.all_down.cancel();
            return true;
        }

        self.all_down.is_cancelled()
    }

    async fn all_down_cancelled(&self) {
        self.all_down.cancelled().await;
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum LinkRefillKey {
    P2p {
        endpoint_a_family: String,
        endpoint_b_family: String,
    },
}

impl LinkRefillKey {
    fn p2p(local_client_id: &str, peer_client_id: &str) -> Self {
        let local_family = crate::p2p::replica::replica_family_id(local_client_id);
        let peer_family = crate::p2p::replica::replica_family_id(peer_client_id);
        if local_family <= peer_family {
            Self::P2p {
                endpoint_a_family: local_family,
                endpoint_b_family: peer_family,
            }
        } else {
            Self::P2p {
                endpoint_a_family: peer_family,
                endpoint_b_family: local_family,
            }
        }
    }
}

#[derive(Default)]
struct LinkRefillLimiter {
    in_flight: parking_lot::Mutex<HashMap<LinkRefillKey, usize>>,
}

struct LinkRefillPermit {
    key: LinkRefillKey,
    limiter: Arc<LinkRefillLimiter>,
    released: AtomicBool,
}

impl LinkRefillLimiter {
    fn try_acquire(
        self: &Arc<Self>,
        key: LinkRefillKey,
        max_links: usize,
        current_links: usize,
    ) -> Option<LinkRefillPermit> {
        let mut in_flight = self.in_flight.lock();
        let pending = *in_flight.get(&key).unwrap_or(&0);
        if current_links.saturating_add(pending) >= max_links {
            return None;
        }
        *in_flight.entry(key.clone()).or_insert(0) += 1;
        Some(LinkRefillPermit {
            key,
            limiter: self.clone(),
            released: AtomicBool::new(false),
        })
    }

    fn release(&self, key: &LinkRefillKey) {
        let mut in_flight = self.in_flight.lock();
        if let Some(count) = in_flight.get_mut(key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(key);
            }
        }
    }
}

impl Drop for LinkRefillPermit {
    fn drop(&mut self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.limiter.release(&self.key);
        }
    }
}

struct ReplicaShutdownGuard {
    cancel: CancellationToken,
    sender: tp_transport::SessionSender,
}

impl ReplicaShutdownGuard {
    fn new(cancel: CancellationToken, sender: tp_transport::SessionSender) -> Self {
        Self { cancel, sender }
    }
}

impl Drop for ReplicaShutdownGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.sender.close();
    }
}

async fn run_reconnecting_replica<F, Fut>(
    client_id: String,
    policy: ReplicaReconnectPolicy,
    group: ReplicaReconnectGroup,
    activity: ReplicaActivity,
    cancel: CancellationToken,
    mut run_once: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut backoff = policy.initial_backoff;
    let mut reported_down = false;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        match run_once().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                let active_count = activity.active_count();
                if group.report_failure(&mut reported_down, active_count) {
                    tracing::warn!(
                        %client_id,
                        error = %e,
                        active_replicas = active_count,
                        "all tunnel replicas are down; returning failure to reconnect group"
                    );
                    return Err(e);
                }
                tracing::warn!(
                    %client_id,
                    error = %e,
                    active_replicas = active_count,
                    backoff_ms = backoff.as_millis(),
                    "tunnel replica failed, reconnecting"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = group.all_down_cancelled() => return Err(e),
                    _ = sleep(backoff) => {}
                }
                backoff = backoff.saturating_mul(2).min(policy.max_backoff);
            }
        }
    }
}

type WatchdogPreCloseHook = Arc<dyn Fn() + Send + Sync + 'static>;

#[cfg(test)]
// The watchdog test adapter mirrors the production watchdog inputs explicitly.
#[allow(clippy::too_many_arguments)]
async fn run_transport_heartbeat_watchdog(
    sender: tp_transport::SessionSender,
    last_ack: Arc<AtomicU64>,
    relay_last_link_progress_ms: Arc<AtomicU64>,
    client_id: String,
    active_flows: LinkActiveFlowCounters,
    traffic: Arc<TrafficCounters>,
    config: LinkWatchdogConfig,
    cancel: CancellationToken,
) {
    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    let multi = crate::p2p::session::MultiSession::new_with_relay_only(Arc::new(
        tp_transport::session::Session::send_only_from_sender(sender.clone()),
    ));
    run_relay_link_watchdog_inner(
        engine,
        multi,
        sender,
        last_ack,
        relay_last_link_progress_ms,
        client_id,
        0,
        active_flows,
        traffic,
        config,
        cancel,
        None,
    )
    .await;
}

// Keep link-liveness state explicit at this established transport boundary.
#[allow(clippy::too_many_arguments)]
async fn run_relay_link_watchdog_with_tcp_streams(
    engine: Arc<Engine>,
    multi: Arc<crate::p2p::session::MultiSession>,
    sender: tp_transport::SessionSender,
    last_ack: Arc<AtomicU64>,
    relay_last_link_progress_ms: Arc<AtomicU64>,
    client_id: String,
    transport_generation: u64,
    active_flows: LinkActiveFlowCounters,
    traffic: Arc<TrafficCounters>,
    config: LinkWatchdogConfig,
    cancel: CancellationToken,
) {
    run_relay_link_watchdog_inner(
        engine,
        multi,
        sender,
        last_ack,
        relay_last_link_progress_ms,
        client_id,
        transport_generation,
        active_flows,
        traffic,
        config,
        cancel,
        None,
    )
    .await;
}

#[cfg(test)]
// The test hook is one additional input on the production watchdog adapter.
#[allow(clippy::too_many_arguments)]
async fn run_transport_heartbeat_watchdog_with_pre_close_hook(
    sender: tp_transport::SessionSender,
    last_ack: Arc<AtomicU64>,
    relay_last_link_progress_ms: Arc<AtomicU64>,
    client_id: String,
    active_flows: LinkActiveFlowCounters,
    traffic: Arc<TrafficCounters>,
    config: LinkWatchdogConfig,
    cancel: CancellationToken,
    pre_close_hook: WatchdogPreCloseHook,
) {
    let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
    let multi = crate::p2p::session::MultiSession::new_with_relay_only(Arc::new(
        tp_transport::session::Session::send_only_from_sender(sender.clone()),
    ));
    run_relay_link_watchdog_inner(
        engine,
        multi,
        sender,
        last_ack,
        relay_last_link_progress_ms,
        client_id,
        0,
        active_flows,
        traffic,
        config,
        cancel,
        Some(pre_close_hook),
    )
    .await;
}

// Grouping these mature watchdog inputs would add churn without clarifying ownership.
#[allow(clippy::too_many_arguments)]
async fn run_relay_link_watchdog_inner(
    engine: Arc<Engine>,
    multi: Arc<crate::p2p::session::MultiSession>,
    sender: tp_transport::SessionSender,
    last_ack: Arc<AtomicU64>,
    relay_last_link_progress_ms: Arc<AtomicU64>,
    client_id: String,
    transport_generation: u64,
    active_flows: LinkActiveFlowCounters,
    traffic: Arc<TrafficCounters>,
    config: LinkWatchdogConfig,
    cancel: CancellationToken,
    pre_close_hook: Option<WatchdogPreCloseHook>,
) {
    let mut tick = interval(config.check_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_active_flow_stale_log: Option<u64> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let now = monotonic_millis();
        let active_snapshot = active_flows.snapshot();
        let active_tcp_count = active_snapshot.active_tcp_flows;
        let active_udp_count = active_snapshot.active_udp_flows;
        let last = last_ack.load(Ordering::Relaxed);
        let last_link_progress = relay_last_link_progress_ms
            .load(Ordering::Relaxed)
            .max(active_snapshot.last_link_io_progress_ms);
        let snapshot = LinkWatchdogSnapshot {
            now_ms: now,
            last_ack_ms: last,
            last_link_progress_ms: last_link_progress,
            active_tcp_flows: active_tcp_count,
            active_udp_flows: active_udp_count,
        };
        match evaluate_link_watchdog(LinkKind::Relay, config, snapshot) {
            decision @ (LinkWatchdogDecision::Keep
            | LinkWatchdogDecision::KeepIdleStale
            | LinkWatchdogDecision::KeepActiveTcpPinned) => {
                let restored = engine.mark_relay_session_usable_for_new_flows(
                    &client_id,
                    transport_generation,
                    &multi,
                );
                if restored {
                    tracing::debug!(
                        %client_id,
                        transport_generation,
                        link_kind = "relay",
                        peer = "gateway",
                        "relay watchdog is keeping the session open; allowing new flows"
                    );
                }
                let ack_age_ms = now.saturating_sub(last);
                if ack_age_ms < config.ack_stale_after.as_millis() as u64 {
                    continue;
                }
                let no_link_progress_ms = now.saturating_sub(last_link_progress);
                let should_log = match last_active_flow_stale_log {
                    Some(previous) => {
                        now.saturating_sub(previous) >= config.stale_log_interval.as_millis() as u64
                    }
                    None => true,
                };
                if should_log {
                    let traffic_snapshot = traffic.snapshot();
                    last_active_flow_stale_log = Some(now);
                    let watchdog_state = match decision {
                        LinkWatchdogDecision::KeepActiveTcpPinned => "active_tcp_pinned",
                        LinkWatchdogDecision::KeepIdleStale => "idle_suspect",
                        LinkWatchdogDecision::Keep => "progress_grace",
                        LinkWatchdogDecision::Close(_) => unreachable!(),
                    };
                    tracing::warn!(
                        %client_id,
                        transport_generation,
                        link_kind = "relay",
                        peer = "gateway",
                        close_reason = "none",
                        watchdog_state,
                        ack_age_ms,
                        ack_stale_after_ms = config.ack_stale_after.as_millis(),
                        no_link_progress_ms,
                        active_no_link_progress_grace_ms =
                            config.active_no_link_progress_grace.as_millis(),
                        active_tcp_flows = active_tcp_count,
                        active_udp_flows = active_udp_count,
                        relay_rx_bytes = traffic_snapshot.relay_rx_bytes,
                        relay_tx_bytes = traffic_snapshot.relay_tx_bytes,
                        p2p_rx_bytes = traffic_snapshot.p2p_rx_bytes,
                        p2p_tx_bytes = traffic_snapshot.p2p_tx_bytes,
                        "relay heartbeat ACK is stale; watchdog is keeping the session open"
                    );
                }
            }
            LinkWatchdogDecision::Close(reason) => {
                if let Some(pre_close_hook) = &pre_close_hook {
                    pre_close_hook();
                }
                let recheck_now = monotonic_millis();
                let fresh_last = last_ack.load(Ordering::Relaxed);
                let fresh_active_snapshot = active_flows.snapshot();
                let fresh_link_progress = relay_last_link_progress_ms
                    .load(Ordering::Relaxed)
                    .max(fresh_active_snapshot.last_link_io_progress_ms);
                let fresh_active_tcp_count = fresh_active_snapshot.active_tcp_flows;
                let fresh_active_udp_count = fresh_active_snapshot.active_udp_flows;
                let recheck_snapshot = LinkWatchdogSnapshot {
                    now_ms: recheck_now,
                    last_ack_ms: fresh_last,
                    last_link_progress_ms: fresh_link_progress,
                    active_tcp_flows: fresh_active_tcp_count,
                    active_udp_flows: fresh_active_udp_count,
                };
                let LinkWatchdogDecision::Close(recheck_reason) =
                    evaluate_link_watchdog(LinkKind::Relay, config, recheck_snapshot)
                else {
                    continue;
                };
                let traffic_snapshot = traffic.snapshot();
                engine.mark_relay_session_unusable_for_new_flows(
                    &client_id,
                    transport_generation,
                    &multi,
                );
                tracing::warn!(
                    %client_id,
                    link_kind = "relay",
                    peer = "gateway",
                    close_reason = recheck_reason.as_str(),
                    initial_close_reason = reason.as_str(),
                    ack_age_ms = recheck_now.saturating_sub(fresh_last),
                    ack_stale_after_ms = config.ack_stale_after.as_millis(),
                    no_link_progress_ms = recheck_now.saturating_sub(fresh_link_progress),
                    active_no_link_progress_grace_ms =
                        config.active_no_link_progress_grace.as_millis(),
                    active_tcp_flows = fresh_active_tcp_count,
                    active_udp_flows = fresh_active_udp_count,
                    relay_rx_bytes = traffic_snapshot.relay_rx_bytes,
                    relay_tx_bytes = traffic_snapshot.relay_tx_bytes,
                    p2p_rx_bytes = traffic_snapshot.p2p_rx_bytes,
                    p2p_tx_bytes = traffic_snapshot.p2p_tx_bytes,
                    "relay heartbeat ACK and same-link progress are stale; closing relay session"
                );
                sender.close();
                return;
            }
        }
    }
}

// Keep relay and P2P watchdog signatures parallel and their state explicit.
#[allow(clippy::too_many_arguments)]
async fn run_p2p_link_watchdog(
    engine: Arc<Engine>,
    multi: Arc<crate::p2p::session::MultiSession>,
    send_shell: Arc<tp_transport::session::Session>,
    last_ack: Arc<AtomicU64>,
    p2p_last_link_progress_ms: Arc<AtomicU64>,
    peer_client_id: String,
    active_flows: LinkActiveFlowCounters,
    traffic: Arc<TrafficCounters>,
    config: LinkWatchdogConfig,
    cancel: CancellationToken,
) {
    let mut tick = interval(config.check_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_active_flow_stale_log: Option<u64> = None;
    let mut last_refill_request_ms: Option<u64> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        if multi.p2p_peer_client_id_for_handle(&send_shell).is_none() {
            return;
        }

        let now = monotonic_millis();
        let active_snapshot = active_flows.snapshot();
        let active_tcp_count = active_snapshot.active_tcp_flows;
        let active_udp_count = active_snapshot.active_udp_flows;
        let last = last_ack.load(Ordering::Relaxed);
        let last_link_progress = p2p_last_link_progress_ms
            .load(Ordering::Relaxed)
            .max(active_snapshot.last_link_io_progress_ms);
        let snapshot = LinkWatchdogSnapshot {
            now_ms: now,
            last_ack_ms: last,
            last_link_progress_ms: last_link_progress,
            active_tcp_flows: active_tcp_count,
            active_udp_flows: active_udp_count,
        };

        match evaluate_link_watchdog(LinkKind::P2p, config, snapshot) {
            LinkWatchdogDecision::Keep => {
                let restored = multi.mark_p2p_session_usable_for_new_flows_for_handle(&send_shell);
                if restored {
                    tracing::info!(
                        link_kind = "p2p",
                        peer = %peer_client_id,
                        "P2P link recovered; allowing new flows on direct path"
                    );
                }
            }
            decision @ (LinkWatchdogDecision::KeepActiveTcpPinned
            | LinkWatchdogDecision::KeepIdleStale) => {
                let marked_unusable =
                    multi.mark_p2p_session_unusable_for_new_flows_for_handle(&send_shell);
                let ack_age_ms = now.saturating_sub(last);
                let no_link_progress_ms = now.saturating_sub(last_link_progress);
                let should_log = match last_active_flow_stale_log {
                    Some(previous) => {
                        now.saturating_sub(previous) >= config.stale_log_interval.as_millis() as u64
                    }
                    None => true,
                };
                if should_log {
                    let traffic_snapshot = traffic.snapshot();
                    last_active_flow_stale_log = Some(now);
                    tracing::warn!(
                        link_kind = "p2p",
                        peer = %peer_client_id,
                        close_reason = "none",
                        watchdog_state = match decision {
                            LinkWatchdogDecision::KeepActiveTcpPinned => "active_tcp_pinned",
                            LinkWatchdogDecision::KeepIdleStale => "idle_suspect",
                            _ => unreachable!(),
                        },
                        ack_age_ms,
                        ack_stale_after_ms = config.ack_stale_after.as_millis(),
                        no_link_progress_ms,
                        active_no_link_progress_grace_ms =
                            config.active_no_link_progress_grace.as_millis(),
                        active_tcp_flows = active_tcp_count,
                        active_udp_flows = active_udp_count,
                        p2p_new_flows_disabled = marked_unusable,
                        relay_rx_bytes = traffic_snapshot.relay_rx_bytes,
                        relay_tx_bytes = traffic_snapshot.relay_tx_bytes,
                        p2p_rx_bytes = traffic_snapshot.p2p_rx_bytes,
                        p2p_tx_bytes = traffic_snapshot.p2p_tx_bytes,
                        "P2P heartbeat ACK and same-link progress are stale; quarantining from new flow placement"
                    );
                }
                let should_request_refill = last_refill_request_ms
                    .map(|previous| {
                        now.saturating_sub(previous) >= config.ack_stale_after.as_millis() as u64
                    })
                    .unwrap_or(true);
                if should_request_refill {
                    engine.request_p2p_refill(&peer_client_id);
                    last_refill_request_ms = Some(now);
                }
            }
            LinkWatchdogDecision::Close(reason) => {
                let recheck_now = monotonic_millis();
                let fresh_last = last_ack.load(Ordering::Relaxed);
                let fresh_active_snapshot = active_flows.snapshot();
                let fresh_link_progress = p2p_last_link_progress_ms
                    .load(Ordering::Relaxed)
                    .max(fresh_active_snapshot.last_link_io_progress_ms);
                let fresh_active_tcp_count = fresh_active_snapshot.active_tcp_flows;
                let fresh_active_udp_count = fresh_active_snapshot.active_udp_flows;
                let recheck_snapshot = LinkWatchdogSnapshot {
                    now_ms: recheck_now,
                    last_ack_ms: fresh_last,
                    last_link_progress_ms: fresh_link_progress,
                    active_tcp_flows: fresh_active_tcp_count,
                    active_udp_flows: fresh_active_udp_count,
                };
                let LinkWatchdogDecision::Close(recheck_reason) =
                    evaluate_link_watchdog(LinkKind::P2p, config, recheck_snapshot)
                else {
                    continue;
                };
                let traffic_snapshot = traffic.snapshot();
                tracing::warn!(
                    link_kind = "p2p",
                    peer = %peer_client_id,
                    close_reason = recheck_reason.as_str(),
                    initial_close_reason = reason.as_str(),
                    ack_age_ms = recheck_now.saturating_sub(fresh_last),
                    ack_stale_after_ms = config.ack_stale_after.as_millis(),
                    no_link_progress_ms = recheck_now.saturating_sub(fresh_link_progress),
                    active_no_link_progress_grace_ms =
                        config.active_no_link_progress_grace.as_millis(),
                    active_tcp_flows = fresh_active_tcp_count,
                    active_udp_flows = fresh_active_udp_count,
                    relay_rx_bytes = traffic_snapshot.relay_rx_bytes,
                    relay_tx_bytes = traffic_snapshot.relay_tx_bytes,
                    p2p_rx_bytes = traffic_snapshot.p2p_rx_bytes,
                    p2p_tx_bytes = traffic_snapshot.p2p_tx_bytes,
                    "P2P heartbeat ACK and same-link progress are stale; closing P2P session"
                );
                let p2p_session_id = multi.p2p_session_id_for_handle(&send_shell);
                engine.request_p2p_refill(&peer_client_id);
                if multi.close_p2p_session_for_handle(&send_shell) {
                    multi.report_p2p_to_relay_migration_with_context(
                        recheck_reason.as_str(),
                        None,
                        None,
                        p2p_session_id,
                    );
                }
                if let Some(session_id) = p2p_session_id {
                    engine.notify_p2p_relation_closed(session_id);
                }
                return;
            }
        }
    }
}

fn replica_dial_delay(idx: usize, stagger: Duration) -> Duration {
    if idx == 0 || stagger.is_zero() {
        Duration::ZERO
    } else {
        stagger.saturating_mul(idx as u32)
    }
}

async fn sleep_replica_dial_stagger(idx: usize, stagger: Duration) {
    let delay = replica_dial_delay(idx, stagger);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

fn gateway_candidate_endpoints(candidates: &[GatewayDialCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| gateway_endpoint(&candidate.gateway_addr, candidate.gateway_port))
        .collect()
}

impl ReplicaTransport {
    fn kind(&self) -> &'static str {
        match self {
            Self::Quic { .. } => "quic",
            Self::WebSocket { .. } => "websocket",
            Self::Grpc { .. } => "grpc",
        }
    }
}

async fn first_successful_gateway_attempt<T, Fut>(
    attempts: Vec<Fut>,
    candidate_count: usize,
) -> tp_transport::Result<T>
where
    Fut: Future<Output = (usize, tp_transport::Result<T>)>,
{
    let mut errors: Vec<Option<tp_transport::TransportError>> =
        (0..candidate_count).map(|_| None).collect();
    for attempt in attempts {
        let (index, result) = attempt.await;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                if let Some(slot) = errors.get_mut(index) {
                    *slot = Some(error);
                }
            }
        }
    }

    Err(errors.into_iter().flatten().next().unwrap_or_else(|| {
        tp_transport::TransportError::Other("no gateway candidates configured".into())
    }))
}

async fn connect_transport(
    transport: &ReplicaTransport,
    auth: AuthParams,
    dial_timeout: Duration,
) -> tp_transport::Result<ConnectedTransport> {
    match transport {
        ReplicaTransport::Quic { client, candidates } => {
            let mut attempts = Vec::with_capacity(candidates.len());
            for (index, candidate) in candidates.iter().cloned().enumerate() {
                let client = client.clone();
                let mut auth = auth.clone();
                let candidate_count = candidates.len();
                attempts.push(async move {
                    tracing::debug!(
                        attempt = index + 1,
                        total_candidates = candidate_count,
                        gateway = %candidate.gateway_addr,
                        gateway_port = candidate.gateway_port,
                        server_name = %candidate.server_name,
                        "quic gateway candidate attempt starting"
                    );
                    let resolved_addr =
                        match resolve_gateway_addr(&candidate.gateway_addr, candidate.gateway_port)
                            .await
                        {
                            Ok(addr) => addr,
                            Err(e) => {
                                let e = tp_transport::TransportError::Other(format!(
                                    "resolve gateway {}:{} failed: {e}",
                                    candidate.gateway_addr, candidate.gateway_port
                                ));
                                tracing::warn!(
                                    gateway = %candidate.gateway_addr,
                                    gateway_port = candidate.gateway_port,
                                    error = %e,
                                    "quic gateway candidate DNS resolution failed"
                                );
                                return (index, Err(e));
                            }
                        };
                    tracing::debug!(
                        attempt = index + 1,
                        total_candidates = candidate_count,
                        gateway = %candidate.gateway_addr,
                        resolved_addr = %resolved_addr,
                        server_name = %candidate.server_name,
                        "quic gateway candidate resolved"
                    );
                    auth.peer_addr = resolved_addr;
                    let result = match tokio::time::timeout(
                        dial_timeout,
                        client.connect(resolved_addr, &candidate.server_name, auth),
                    )
                    .await
                    {
                        Ok(Ok(session)) => Ok(ConnectedTransport {
                            session,
                            gateway_addr: resolved_addr,
                        }),
                        Ok(Err(e)) => {
                            tracing::warn!(
                                gateway = %candidate.gateway_addr,
                                resolved_addr = %resolved_addr,
                                error = %e,
                                "quic gateway candidate failed"
                            );
                            Err(e)
                        }
                        Err(_) => {
                            let e = tp_transport::TransportError::Other(format!(
                                "quic dial to {} ({}) timed out (>{}s)",
                                resolved_addr,
                                candidate.gateway_addr,
                                dial_timeout.as_secs()
                            ));
                            tracing::debug!(
                                gateway = %candidate.gateway_addr,
                                resolved_addr = %resolved_addr,
                                error = %e,
                                "quic gateway candidate timed out"
                            );
                            Err(e)
                        }
                    };
                    (index, result)
                });
            }
            first_successful_gateway_attempt(attempts, candidates.len()).await
        }
        ReplicaTransport::WebSocket {
            candidates,
            tls_config,
        } => {
            let mut attempts = Vec::with_capacity(candidates.len());
            for (index, candidate) in candidates.iter().cloned().enumerate() {
                let mut auth = auth.clone();
                let tls_config = tls_config.clone();
                let candidate_count = candidates.len();
                attempts.push(async move {
                    tracing::debug!(
                        attempt = index + 1,
                        total_candidates = candidate_count,
                        gateway = %candidate.gateway_addr,
                        gateway_port = candidate.gateway_port,
                        url = %candidate.url,
                        "websocket gateway candidate attempt starting"
                    );
                    let resolved_addr =
                        match resolve_gateway_addr(&candidate.gateway_addr, candidate.gateway_port)
                            .await
                        {
                            Ok(addr) => addr,
                            Err(e) => {
                                let e = tp_transport::TransportError::Other(format!(
                                    "resolve gateway {}:{} failed: {e}",
                                    candidate.gateway_addr, candidate.gateway_port
                                ));
                                tracing::warn!(
                                    gateway = %candidate.gateway_addr,
                                    gateway_port = candidate.gateway_port,
                                    url = %candidate.url,
                                    error = %e,
                                    "websocket gateway candidate DNS resolution failed"
                                );
                                return (index, Err(e));
                            }
                        };
                    tracing::debug!(
                        attempt = index + 1,
                        total_candidates = candidate_count,
                        gateway = %candidate.gateway_addr,
                        resolved_addr = %resolved_addr,
                        url = %candidate.url,
                        "websocket gateway candidate resolved"
                    );
                    auth.peer_addr = resolved_addr;
                    let result = match tokio::time::timeout(dial_timeout, async {
                        if candidate.tls_server_name.is_some() {
                            WsClient::connect_to_addr_with_tls_config(
                                &candidate.url,
                                resolved_addr,
                                auth,
                                tls_config,
                            )
                            .await
                        } else {
                            WsClient::connect_with_tls_config(&candidate.url, auth, tls_config)
                                .await
                        }
                    })
                    .await
                    {
                        Ok(Ok(session)) => Ok(ConnectedTransport {
                            session,
                            gateway_addr: resolved_addr,
                        }),
                        Ok(Err(e)) => {
                            tracing::warn!(
                                gateway = %candidate.gateway_addr,
                                resolved_addr = %resolved_addr,
                                url = %candidate.url,
                                error = %e,
                                "websocket gateway candidate failed"
                            );
                            Err(e)
                        }
                        Err(_) => {
                            let e = tp_transport::TransportError::Other(format!(
                                "websocket dial to {} ({}) timed out (>{}s)",
                                candidate.url,
                                candidate.gateway_addr,
                                dial_timeout.as_secs()
                            ));
                            tracing::debug!(
                                gateway = %candidate.gateway_addr,
                                resolved_addr = %resolved_addr,
                                url = %candidate.url,
                                error = %e,
                                "websocket gateway candidate timed out"
                            );
                            Err(e)
                        }
                    };
                    (index, result)
                });
            }
            first_successful_gateway_attempt(attempts, candidates.len()).await
        }
        ReplicaTransport::Grpc {
            candidates,
            insecure_tls,
        } => {
            let mut attempts = Vec::with_capacity(candidates.len());
            for (index, candidate) in candidates.iter().cloned().enumerate() {
                let mut auth = auth.clone();
                let insecure_tls = *insecure_tls;
                let candidate_count = candidates.len();
                attempts.push(async move {
                    tracing::debug!(
                        attempt = index + 1,
                        total_candidates = candidate_count,
                        gateway = %candidate.gateway_addr,
                        gateway_port = candidate.gateway_port,
                        url = %candidate.url,
                        tls_domain = ?candidate.tls_domain,
                        "grpc gateway candidate attempt starting"
                    );
                    let resolved_addr =
                        match resolve_gateway_addr(&candidate.gateway_addr, candidate.gateway_port)
                            .await
                        {
                            Ok(addr) => addr,
                            Err(e) => {
                                let e = tp_transport::TransportError::Other(format!(
                                    "resolve gateway {}:{} failed: {e}",
                                    candidate.gateway_addr, candidate.gateway_port
                                ));
                                tracing::warn!(
                                    gateway = %candidate.gateway_addr,
                                    gateway_port = candidate.gateway_port,
                                    url = %candidate.url,
                                    error = %e,
                                    "grpc gateway candidate DNS resolution failed"
                                );
                                return (index, Err(e));
                            }
                        };
                    tracing::debug!(
                        attempt = index + 1,
                        total_candidates = candidate_count,
                        gateway = %candidate.gateway_addr,
                        resolved_addr = %resolved_addr,
                        url = %candidate.url,
                        tls_domain = ?candidate.tls_domain,
                        "grpc gateway candidate resolved"
                    );
                    auth.peer_addr = resolved_addr;
                    let connect = async {
                        let client = GrpcClient::new(candidate.url.clone())?;
                        let client = if let Some(domain) = &candidate.tls_domain {
                            if let Some(exact_leaf_pem) = &candidate.exact_leaf_pem {
                                client
                                    .with_exact_leaf_tls(domain.clone(), exact_leaf_pem.clone())?
                            } else if insecure_tls {
                                client.with_insecure_tls(domain.clone())?
                            } else {
                                client.with_tls_roots(domain.clone(), candidate.ca_pem.clone())?
                            }
                        } else {
                            client
                        };
                        client.connect(auth).await
                    };
                    let result = match tokio::time::timeout(dial_timeout, connect).await {
                        Ok(Ok(session)) => Ok(ConnectedTransport {
                            session,
                            gateway_addr: resolved_addr,
                        }),
                        Ok(Err(e)) => {
                            tracing::warn!(
                                gateway = %candidate.gateway_addr,
                                resolved_addr = %resolved_addr,
                                url = %candidate.url,
                                error = %e,
                                "grpc gateway candidate failed"
                            );
                            Err(e)
                        }
                        Err(_) => {
                            let e = tp_transport::TransportError::Other(format!(
                                "grpc dial to {} ({}) timed out (>{}s)",
                                candidate.url,
                                candidate.gateway_addr,
                                dial_timeout.as_secs()
                            ));
                            tracing::debug!(
                                gateway = %candidate.gateway_addr,
                                resolved_addr = %resolved_addr,
                                url = %candidate.url,
                                error = %e,
                                "grpc gateway candidate timed out"
                            );
                            Err(e)
                        }
                    };
                    (index, result)
                });
            }
            first_successful_gateway_attempt(attempts, candidates.len()).await
        }
    }
}

async fn wait_for_replica_group_outcome(
    total: usize,
    stop_rx: &mut mpsc::Receiver<()>,
    err_rx: &mut mpsc::Receiver<anyhow::Error>,
    cancel: &CancellationToken,
) -> SessionOutcome {
    let mut remaining = total.max(1);
    let mut last_error = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return SessionOutcome::UserCancel,
            _ = stop_rx.recv() => return SessionOutcome::UserCancel,
            maybe_error = err_rx.recv() => {
                match maybe_error {
                    Some(e) => {
                        remaining = remaining.saturating_sub(1);
                        last_error = Some(e);
                        if remaining == 0 {
                            return SessionOutcome::Failed(
                                last_error.expect("last replica failure must be present"),
                            );
                        }
                    }
                    None => {
                        return SessionOutcome::Failed(
                            last_error.unwrap_or_else(|| anyhow::anyhow!("all tunnel replicas exited")),
                        );
                    }
                }
            }
        }
    }
}

async fn wait_for_abort_on_drop_handles(
    task_kind: &'static str,
    handles: Vec<AbortOnDropHandle<()>>,
    timeout_duration: Duration,
) {
    if handles.is_empty() {
        return;
    }
    if tokio::time::timeout(timeout_duration, futures_util::future::join_all(handles))
        .await
        .is_err()
    {
        tracing::debug!(
            task_kind,
            timeout_ms = timeout_duration.as_millis() as u64,
            "task cohort drain timed out; aborting remaining tasks"
        );
    }
}

async fn drain_replica_intake_readers(
    reader: Option<AbortOnDropHandle<()>>,
    tcp_flow_reader: Option<AbortOnDropHandle<()>>,
    datagram_reader: Option<AbortOnDropHandle<()>>,
    timeout_duration: Duration,
) {
    let mut readers = Vec::with_capacity(3);
    readers.extend(reader);
    readers.extend(tcp_flow_reader);
    readers.extend(datagram_reader);
    wait_for_abort_on_drop_handles("replica intake reader", readers, timeout_duration).await;
}

impl Engine {
    pub fn new(cfg: EngineConfig, listener: Arc<dyn StatusListener>) -> Arc<Self> {
        Self::new_with_services(
            cfg,
            listener,
            Arc::new(PlatformManagedGatewayResolver),
            Arc::new(PlatformManagedPeerHeartbeatSender {
                client: crate::peer_heartbeat::PeerHeartbeatClient::new(),
            }),
        )
    }

    #[cfg(test)]
    fn new_with_managed_services(
        cfg: EngineConfig,
        listener: Arc<dyn StatusListener>,
        resolver: Arc<dyn ManagedGatewayResolver>,
        heartbeat_sender: Arc<dyn ManagedPeerHeartbeatSender>,
    ) -> Arc<Self> {
        Self::new_with_services(cfg, listener, resolver, heartbeat_sender)
    }

    fn new_with_services(
        cfg: EngineConfig,
        listener: Arc<dyn StatusListener>,
        managed_gateway_resolver: Arc<dyn ManagedGatewayResolver>,
        managed_peer_heartbeat_sender: Arc<dyn ManagedPeerHeartbeatSender>,
    ) -> Arc<Self> {
        let initial = ConnectionStatus {
            message: "Disconnected".into(),
            ..Default::default()
        };
        Arc::new(Self {
            cfg,
            listener,
            managed_gateway_resolver,
            managed_peer_heartbeat_sender,
            state: RwLock::new(initial),
            connected_since: parking_lot::Mutex::new(None),
            stop_tx: RwLock::new(None),
            latest_tunnel_config: RwLock::new(None),
            active_v2_profile: RwLock::new(None),
            v2_runtime: RwLock::new(crate::runtime_snapshot::V2RuntimeSnapshot::default()),
            v2_runtime_reconcile_lock: Arc::new(parking_lot::Mutex::new(())),
            managed_mapping_port: AtomicU16::new(0),
            multi: parking_lot::Mutex::new(None),
            replica_sessions: parking_lot::Mutex::new(Vec::new()),
            p2p_session_registry_lock: parking_lot::Mutex::new(()),
            proxy_replica_rr: AtomicUsize::new(0),
            proxy_flow_placement_lock: parking_lot::Mutex::new(()),
            proxy_flow_scheduler: ReplicaFlowScheduler::default(),
            proxy_flow_registry: FlowPlacementRegistry::default(),
            overlay_routes: RwLock::new(crate::route_matcher::OverlayRouteMatcher::default()),
            v2_peer_links: DashMap::new(),
            v2_relay_flows: DashMap::new(),
            v2_peer_gossip: parking_lot::Mutex::new(None),
            v2_current_membership: RwLock::new(BTreeSet::new()),
            v2_membership_cycle_complete: AtomicBool::new(false),
            v2_local_runtime_record: RwLock::new(
                crate::peer_runtime::PeerRuntimeRecordV2::default(),
            ),
            v2_local_lan_export_config: RwLock::new(
                crate::peer_runtime::LocalLanExportConfigV2::default(),
            ),
            v2_local_lan_export_generation: AtomicU64::new(0),
            v2_access_policy: RwLock::new(
                crate::access_policy::CompiledClientAccessPolicyV2::deny_all(),
            ),
            p2p_install_rr: AtomicUsize::new(0),
            p2p_pending_installs: parking_lot::Mutex::new(HashMap::new()),
            link_refill_limiter: Arc::new(LinkRefillLimiter::default()),
            relay_transport_generations: DashMap::new(),
            p2p_refill_handle: parking_lot::Mutex::new(None),
            #[cfg(test)]
            p2p_refill_requests: DashMap::new(),
            p2p_anchor_client_id: parking_lot::Mutex::new(None),
            p2p_signaling_ingress_tx: parking_lot::Mutex::new(None),
            p2p_pending_membership_batches: parking_lot::Mutex::new(HashMap::new()),
            p2p_delivered_membership_authorities: parking_lot::Mutex::new(VecDeque::new()),
            p2p_signaling_routes: Arc::new(DashMap::new()),
            replicas: parking_lot::Mutex::new(None),
            p2p_expected_fp: parking_lot::Mutex::new(None),
            tunnel_identity: parking_lot::Mutex::new(None),
            metrics: parking_lot::Mutex::new(None),
            traffic: Arc::new(TrafficCounters::default()),
            p2p_config: parking_lot::Mutex::new(None),
            native_lan_route_generation: RwLock::new(NativeLanRouteGeneration::default()),
            local_lan_publication: RwLock::new(LocalLanPublicationState::default()),
            local_service_exports: RwLock::new(Vec::new()),
            group_context: parking_lot::Mutex::new(None),
            proxy_pending: Arc::new(DashMap::new()),
            relay_route_bind_pending: Arc::new(DashMap::new()),
            relay_inbound_attestations: DashMap::new(),
            tasks: RwLock::new(TaskTracker::new()),
            task_cancel: RwLock::new(CancellationToken::new()),
            task_abort_handles: parking_lot::Mutex::new(Vec::new()),
        })
    }

    pub fn status(&self) -> ConnectionStatus {
        self.decorate_status(self.state.read().clone())
    }

    /// Atomically clone the public Lantunnel 2.0 runtime truth. Existing
    /// lock-free payload counters are sampled while this one read guard is
    /// held, so callers never assemble identity, attachment, Mesh, Gossip,
    /// and Peer rows through separate Engine reads.
    pub fn v2_runtime_snapshot(&self) -> crate::runtime_snapshot::V2RuntimeSnapshot {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let runtime = self.v2_runtime.read();
        let traffic = self.traffic.snapshot();
        let mut snapshot = runtime.clone();
        snapshot.traffic = crate::runtime_snapshot::V2TrafficSnapshot {
            direct_tx_bytes: traffic.p2p_tx_bytes,
            direct_rx_bytes: traffic.p2p_rx_bytes,
            relay_tx_bytes: traffic.relay_tx_bytes,
            relay_rx_bytes: traffic.relay_rx_bytes,
        };
        snapshot
    }

    fn begin_v2_runtime(&self, profile: &PeerProfileV2) {
        use crate::runtime_snapshot::{
            V2GatewayAttachmentPhase, V2GatewayAttachmentSnapshot, V2GossipPhase, V2MeshPhase,
            V2OverallPhase, V2PeerDirectoryPhase, V2PeerDirectorySnapshot, V2RuntimePhase,
            V2RuntimeReasonCode, V2RuntimeSnapshot, V2ThisPeerSnapshot,
        };

        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        self.v2_current_membership.write().clear();
        self.v2_membership_cycle_complete
            .store(false, Ordering::Release);
        let (gateway_phase, endpoint, reason_code) = match &profile.bootstrap {
            PeerBootstrapV2::StaticGateway(gateway) => (
                V2GatewayAttachmentPhase::Connecting,
                Some(gateway_endpoint(&gateway.dial_address, gateway.port)),
                V2RuntimeReasonCode::ConnectingToGateway,
            ),
            PeerBootstrapV2::ManagedPlatform { .. } => (
                V2GatewayAttachmentPhase::ResolvingThroughPlatform,
                None,
                V2RuntimeReasonCode::ResolvingThroughPlatform,
            ),
        };
        let local_exports = Self::v2_local_export_snapshots(&self.v2_local_runtime_record.read());
        let carried_relay_usage = self.v2_runtime.read().relay_usage;
        *self.v2_runtime.write() = V2RuntimeSnapshot {
            overall: V2RuntimePhase {
                phase: V2OverallPhase::WaitingForGateway,
                reason_code: Some(reason_code),
            },
            gateway_attachment: V2GatewayAttachmentSnapshot {
                phase: gateway_phase,
                endpoint,
                reason_code: Some(reason_code),
            },
            this_peer: Some(V2ThisPeerSnapshot {
                peer_id: profile.peer.peer_id.clone(),
                overlay_ip: profile.peer.overlay_ip,
            }),
            mesh: V2RuntimePhase {
                phase: V2MeshPhase::Syncing,
                reason_code: Some(V2RuntimeReasonCode::MembershipCyclePending),
            },
            gossip: V2RuntimePhase {
                phase: V2GossipPhase::Syncing,
                reason_code: Some(V2RuntimeReasonCode::MembershipCyclePending),
            },
            local_exports,
            peer_directory: V2PeerDirectorySnapshot {
                phase: V2PeerDirectoryPhase::Syncing,
                reason_code: Some(V2RuntimeReasonCode::MembershipCyclePending),
                peers: Vec::new(),
            },
            traffic: Default::default(),
            relay_usage: carried_relay_usage,
        };
    }

    fn v2_local_export_snapshots(
        record: &crate::peer_runtime::PeerRuntimeRecordV2,
    ) -> Vec<crate::runtime_snapshot::V2LocalExportSnapshot> {
        record
            .lan_exports
            .iter()
            .map(|export| crate::runtime_snapshot::V2LocalExportSnapshot {
                prefix: format!("{}/{}", export.prefix.network, export.prefix.prefix_len),
                ready: export.ready,
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn commit_v2_membership_cycle(&self, peer_ids: &[String]) -> bool {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        self.commit_v2_membership_cycle_locked(peer_ids)
    }

    pub(crate) fn commit_delivered_v2_membership_cycle(&self, peer_ids: &[String]) -> bool {
        // The broker and manager share FIFO delivery order, but this queue
        // must never be held while waiting for the session registry. Relay
        // teardown owns the opposite publication boundary.
        let authority = self.p2p_delivered_membership_authorities.lock().pop_front();
        let Some(authority) = authority else {
            tracing::warn!("V2 membership Ack had no delivered relay authority; dropping cycle");
            return false;
        };

        let _registry_guard = self.p2p_session_registry_lock.lock();
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        if !self.p2p_membership_authority_is_active_locked(&authority.source) {
            tracing::debug!(
                client_id = %authority.source.client_id,
                transport_generation = authority.source.transport_generation,
                delivery_sequence = authority.delivery_sequence,
                "V2 membership cycle came from an inactive relay generation; dropping"
            );
            return false;
        }
        self.commit_v2_membership_cycle_locked(peer_ids)
    }

    fn commit_v2_membership_cycle_locked(&self, peer_ids: &[String]) -> bool {
        use crate::runtime_snapshot::{
            V2RemotePeerPhase, V2RemotePeerSnapshot, V2RoutingPhase, V2RuntimeReasonCode,
        };

        let Some(active_profile) = self.active_v2_peer_profile() else {
            return false;
        };
        if self
            .v2_runtime
            .read()
            .this_peer
            .as_ref()
            .is_none_or(|peer| peer.peer_id != active_profile.peer.peer_id)
        {
            return false;
        }
        let local_peer_id = active_profile.peer.peer_id.clone();
        let peers = peer_ids
            .iter()
            .filter(|peer_id| {
                !peer_id.trim().is_empty() && peer_id.as_str() != local_peer_id.as_str()
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        *self.v2_current_membership.write() = peers.clone();
        self.v2_membership_cycle_complete
            .store(true, Ordering::Release);

        let mut runtime = self.v2_runtime.write();
        for peer_id in peers {
            if runtime
                .peer_directory
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id)
            {
                continue;
            }
            runtime.peer_directory.peers.push(V2RemotePeerSnapshot {
                peer_id,
                overlay_ip: None,
                phase: V2RemotePeerPhase::Syncing,
                reason_code: Some(V2RuntimeReasonCode::PeerLinkUnavailable),
                current_path: None,
                usable_lanes: None,
                routing: V2RoutingPhase::Syncing,
                exports: Vec::new(),
            });
        }
        runtime
            .peer_directory
            .peers
            .sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
        drop(runtime);
        self.reconcile_v2_routes_and_runtime_locked();
        true
    }

    pub(crate) fn has_usable_exact_relay_for_peer(&self, peer_id: &str) -> bool {
        self.v2_runtime.read().gateway_attachment.phase
            == crate::runtime_snapshot::V2GatewayAttachmentPhase::Attached
            && self.has_usable_v2_gateway_relay_lane()
            && self.v2_current_membership.read().contains(peer_id)
            && self.v2_peer_links.contains_key(peer_id)
    }

    pub(crate) fn is_v2_current_member(&self, peer_id: &str) -> bool {
        self.v2_membership_cycle_complete.load(Ordering::Acquire)
            && self.v2_current_membership.read().contains(peer_id)
    }

    fn has_usable_v2_gateway_relay_lane(&self) -> bool {
        self.replica_sessions
            .lock()
            .iter()
            .any(|entry| entry.relay_active && entry.relay_accepts_new_flows)
    }

    fn ensure_v2_runtime_peer(&self, peer_id: &str) {
        use crate::runtime_snapshot::{
            V2RemotePeerPhase, V2RemotePeerSnapshot, V2RoutingPhase, V2RuntimeReasonCode,
        };

        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none()
            || runtime
                .peer_directory
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id)
        {
            return;
        }
        runtime.peer_directory.peers.push(V2RemotePeerSnapshot {
            peer_id: peer_id.to_owned(),
            overlay_ip: None,
            phase: V2RemotePeerPhase::Syncing,
            reason_code: Some(V2RuntimeReasonCode::PeerLinkUnavailable),
            current_path: None,
            usable_lanes: None,
            routing: V2RoutingPhase::Syncing,
            exports: Vec::new(),
        });
        runtime
            .peer_directory
            .peers
            .sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
    }

    /// Reconcile the public read model and the routable LAN Export table from
    /// existing product authorities. Losing the final Lane closes that
    /// PeerLink's Gossip view and withdraws its learned record. Recovery must
    /// complete a fresh full sync before the origin re-enters at the tail.
    fn reconcile_v2_routes_and_runtime_locked(&self) {
        use crate::runtime_snapshot::{
            V2ExportPlacement, V2GatewayAttachmentPhase, V2GossipPhase, V2MeshPhase,
            V2OverallPhase, V2PeerDirectoryPhase, V2PeerPath, V2RemoteExportSnapshot,
            V2RemotePeerPhase, V2RoutingPhase, V2RuntimeReasonCode,
        };

        let (peer_ids, previous_lanes, gateway_phase) = {
            let runtime = self.v2_runtime.read();
            if runtime.this_peer.is_none() {
                return;
            }
            (
                runtime
                    .peer_directory
                    .peers
                    .iter()
                    .map(|peer| peer.peer_id.clone())
                    .collect::<Vec<_>>(),
                runtime
                    .peer_directory
                    .peers
                    .iter()
                    .map(|peer| (peer.peer_id.clone(), peer.usable_lanes.unwrap_or(0)))
                    .collect::<HashMap<_, _>>(),
                runtime.gateway_attachment.phase,
            )
        };
        let current_membership = self.v2_current_membership.read().clone();
        let relay_available = gateway_phase == V2GatewayAttachmentPhase::Attached
            && self.has_usable_v2_gateway_relay_lane();
        let lane_truth = peer_ids
            .iter()
            .map(|peer_id| {
                let direct_lanes = self.v2_direct_lane_count_for_peer(peer_id);
                let relay = relay_available
                    && current_membership.contains(peer_id)
                    && self.v2_peer_links.contains_key(peer_id);
                (peer_id.clone(), (direct_lanes, relay))
            })
            .collect::<HashMap<_, _>>();
        let (records, outbound_full_syncs) = {
            let mut gossip = self.v2_peer_gossip.lock();
            let Some(gossip) = gossip.as_mut() else {
                return;
            };
            let mut outbound = Vec::new();
            for peer_id in &peer_ids {
                let old_lanes = previous_lanes.get(peer_id).copied().unwrap_or(0);
                let (direct_lanes, relay) = lane_truth.get(peer_id).copied().unwrap_or_default();
                let new_lanes = (direct_lanes as u32).saturating_add(u32::from(relay));
                if old_lanes > 0 && new_lanes == 0 {
                    let _ = gossip.link_closed(peer_id);
                } else if old_lanes == 0 && new_lanes > 0 {
                    if let Ok(full_sync) = gossip.link_ready(peer_id, Instant::now()) {
                        outbound.push(full_sync);
                    }
                }
            }
            let records = peer_ids
                .iter()
                .map(|peer_id| (peer_id.clone(), gossip.directory().record(peer_id).cloned()))
                .collect::<HashMap<_, _>>();
            (records, outbound)
        };
        for outbound in outbound_full_syncs {
            self.send_v2_gossip_outbound(outbound);
        }

        let mut export_snapshots = {
            let mut routes = self.overlay_routes.write();
            for peer_id in &peer_ids {
                let (direct_lanes, relay) = lane_truth.get(peer_id).copied().unwrap_or_default();
                if direct_lanes == 0 && !relay {
                    routes.remove_v2_lan_export_origin(peer_id);
                }
            }
            for peer_id in &peer_ids {
                let (direct_lanes, relay) = lane_truth.get(peer_id).copied().unwrap_or_default();
                if direct_lanes > 0 || relay {
                    if let Some(record) = records.get(peer_id).and_then(Option::as_ref) {
                        let _ = routes.replace_v2_lan_export_origin(peer_id, record.clone());
                    }
                }
            }
            peer_ids
                .iter()
                .map(|peer_id| {
                    let exports = records
                        .get(peer_id)
                        .and_then(Option::as_ref)
                        .map(|record| {
                            record
                                .lan_exports
                                .iter()
                                .map(|export| {
                                    let placement = routes
                                        .v2_lan_export_position(export.prefix, peer_id)
                                        .map(|position| {
                                            if position == 0 {
                                                V2ExportPlacement::ActiveHere
                                            } else {
                                                V2ExportPlacement::StandbyHere {
                                                    position: position as u32,
                                                }
                                            }
                                        });
                                    V2RemoteExportSnapshot {
                                        prefix: format!(
                                            "{}/{}",
                                            export.prefix.network, export.prefix.prefix_len
                                        ),
                                        placement,
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    (peer_id.clone(), exports)
                })
                .collect::<HashMap<_, _>>()
        };

        let cycle_complete = self.v2_membership_cycle_complete.load(Ordering::Acquire);
        let mut runtime = self.v2_runtime.write();
        for peer in &mut runtime.peer_directory.peers {
            let (direct_lanes, relay) = lane_truth.get(&peer.peer_id).copied().unwrap_or_default();
            let lane_count = (direct_lanes as u32).saturating_add(u32::from(relay));
            peer.current_path = if direct_lanes > 0 {
                Some(V2PeerPath::Direct)
            } else if relay {
                Some(V2PeerPath::EncryptedRelay)
            } else {
                None
            };
            peer.usable_lanes = (lane_count > 0).then_some(lane_count);
            peer.exports = export_snapshots.remove(&peer.peer_id).unwrap_or_default();
            if lane_count == 0 {
                peer.phase = V2RemotePeerPhase::Unavailable;
                peer.reason_code = Some(V2RuntimeReasonCode::NoUsablePeerPath);
                peer.routing = V2RoutingPhase::Unavailable;
            } else if records.get(&peer.peer_id).is_some_and(Option::is_some) {
                peer.phase = V2RemotePeerPhase::Ready;
                peer.reason_code = None;
                peer.routing = V2RoutingPhase::Ready;
            } else {
                peer.phase = V2RemotePeerPhase::Syncing;
                peer.reason_code = Some(V2RuntimeReasonCode::InitialFullSyncPending);
                peer.routing = V2RoutingPhase::Syncing;
            }
        }

        let expected_missing = current_membership.iter().any(|peer_id| {
            !runtime
                .peer_directory
                .peers
                .iter()
                .any(|peer| &peer.peer_id == peer_id)
        });
        let any_syncing = expected_missing
            || runtime
                .peer_directory
                .peers
                .iter()
                .any(|peer| peer.phase == V2RemotePeerPhase::Syncing);
        let any_unavailable = runtime.peer_directory.peers.iter().any(|peer| {
            matches!(
                peer.phase,
                V2RemotePeerPhase::Stale | V2RemotePeerPhase::Unavailable
            )
        });
        let any_usable = runtime
            .peer_directory
            .peers
            .iter()
            .any(|peer| peer.usable_lanes.is_some_and(|lanes| lanes > 0));
        let any_direct = runtime
            .peer_directory
            .peers
            .iter()
            .any(|peer| peer.current_path == Some(V2PeerPath::Direct));
        let has_peers = !runtime.peer_directory.peers.is_empty();

        if !cycle_complete || any_syncing {
            runtime.peer_directory.phase = V2PeerDirectoryPhase::Syncing;
            runtime.peer_directory.reason_code = Some(if cycle_complete {
                V2RuntimeReasonCode::InitialFullSyncPending
            } else {
                V2RuntimeReasonCode::MembershipCyclePending
            });
            runtime.mesh.phase = V2MeshPhase::Syncing;
            runtime.mesh.reason_code = runtime.peer_directory.reason_code;
            runtime.gossip.phase = V2GossipPhase::Syncing;
            runtime.gossip.reason_code = runtime.peer_directory.reason_code;
        } else {
            runtime.peer_directory.phase = V2PeerDirectoryPhase::Ready;
            runtime.peer_directory.reason_code = None;
            if any_unavailable {
                runtime.mesh.phase = V2MeshPhase::Degraded;
                runtime.mesh.reason_code = Some(V2RuntimeReasonCode::NoUsablePeerPath);
                runtime.gossip.phase = V2GossipPhase::Repairing;
                runtime.gossip.reason_code = Some(V2RuntimeReasonCode::NoUsablePeerPath);
            } else {
                runtime.mesh.phase = V2MeshPhase::Healthy;
                runtime.mesh.reason_code = None;
                runtime.gossip.phase = V2GossipPhase::Ready;
                runtime.gossip.reason_code = None;
            }
        }

        if !cycle_complete || any_syncing {
            let gateway_failed = matches!(
                gateway_phase,
                V2GatewayAttachmentPhase::Unavailable
                    | V2GatewayAttachmentPhase::Rejected
                    | V2GatewayAttachmentPhase::TlsFailed
            );
            runtime.overall.phase = if gateway_failed && !any_usable {
                V2OverallPhase::Blocked
            } else if gateway_phase == V2GatewayAttachmentPhase::Attached {
                V2OverallPhase::Starting
            } else if any_usable {
                V2OverallPhase::Degraded
            } else {
                V2OverallPhase::WaitingForGateway
            };
            runtime.overall.reason_code = Some(if gateway_failed && !any_usable {
                runtime
                    .gateway_attachment
                    .reason_code
                    .unwrap_or(V2RuntimeReasonCode::GatewayUnavailable)
            } else if cycle_complete {
                V2RuntimeReasonCode::InitialFullSyncPending
            } else {
                V2RuntimeReasonCode::MembershipCyclePending
            });
        } else {
            let (phase, reason) = settled_overall_phase(
                gateway_phase,
                any_direct,
                any_unavailable,
                any_usable,
                has_peers,
                runtime.gateway_attachment.reason_code,
            );
            runtime.overall.phase = phase;
            runtime.overall.reason_code = reason;
        }
    }

    /// Records what the Platform last reported about the Relay allowance.
    ///
    /// An observation the owner is shown, not a limit this Client applies.
    pub(crate) fn set_v2_relay_usage(&self, usage: crate::peer_heartbeat::PeerRelayUsage) {
        self.v2_runtime.write().relay_usage = Some(crate::runtime_snapshot::V2RelayUsageSnapshot {
            used_bytes: usage.used_bytes,
            allowance_bytes: usage.allowance_bytes,
        });
    }

    fn bind_v2_lane_change_observer(
        self: &Arc<Self>,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        let engine = Arc::downgrade(self);
        multi.set_p2p_lane_change_observer(
            self.v2_runtime_reconcile_lock.clone(),
            Arc::new(move || {
                if let Some(engine) = engine.upgrade() {
                    engine.reconcile_v2_routes_and_runtime_locked();
                }
            }),
        );
    }

    fn mark_v2_gateway_attached(&self, gateway_addr: SocketAddr) {
        use crate::runtime_snapshot::V2GatewayAttachmentPhase;

        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none() {
            return;
        }
        runtime.gateway_attachment.phase = V2GatewayAttachmentPhase::Attached;
        runtime.gateway_attachment.endpoint = Some(gateway_addr.to_string());
        runtime.gateway_attachment.reason_code = None;
        drop(runtime);
        self.reconcile_v2_routes_and_runtime_locked();
    }

    fn mark_v2_gateway_disconnected(&self) {
        use crate::runtime_snapshot::{V2GatewayAttachmentPhase, V2RuntimeReasonCode};

        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none() {
            return;
        }
        runtime.gateway_attachment.phase = V2GatewayAttachmentPhase::Unavailable;
        runtime.gateway_attachment.reason_code = Some(V2RuntimeReasonCode::GatewayUnavailable);
        drop(runtime);
        // A reattached Gateway must supply a fresh full cycle before old
        // PeerLink keys become exact Relay authority again. Direct remains
        // independently usable throughout the outage.
        self.v2_current_membership.write().clear();
        self.reconcile_v2_routes_and_runtime_locked();
    }

    #[cfg(test)]
    fn mark_v2_peer_direct_ready(&self, remote_peer_id: &str) {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        self.ensure_v2_runtime_peer(remote_peer_id);
        self.reconcile_v2_routes_and_runtime_locked();
    }

    #[cfg(test)]
    fn mark_v2_peer_direct_closed(&self, remote_peer_id: &str) {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        self.ensure_v2_runtime_peer(remote_peer_id);
        self.reconcile_v2_routes_and_runtime_locked();
    }

    fn mark_v2_gateway_resolving(&self) {
        use crate::runtime_snapshot::{V2GatewayAttachmentPhase, V2RuntimeReasonCode};

        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none() {
            return;
        }
        runtime.gateway_attachment.phase = V2GatewayAttachmentPhase::ResolvingThroughPlatform;
        runtime.gateway_attachment.endpoint = None;
        runtime.gateway_attachment.reason_code =
            Some(V2RuntimeReasonCode::ResolvingThroughPlatform);
        drop(runtime);
        self.reconcile_v2_routes_and_runtime_locked();
    }

    /// The UDP mapping port the attached Gateway reflects on, when it named one.
    pub(crate) fn managed_mapping_port(&self) -> Option<u16> {
        match self.managed_mapping_port.load(Ordering::Relaxed) {
            0 => None,
            port => Some(port),
        }
    }

    fn mark_v2_gateway_connecting(&self, gateway: &GatewayBootstrapV2) {
        use crate::runtime_snapshot::{V2GatewayAttachmentPhase, V2RuntimeReasonCode};

        self.managed_mapping_port
            .store(gateway.mapping_port.unwrap_or(0), Ordering::Relaxed);
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none() {
            return;
        }
        runtime.gateway_attachment.phase = V2GatewayAttachmentPhase::Connecting;
        runtime.gateway_attachment.endpoint =
            Some(gateway_endpoint(&gateway.dial_address, gateway.port));
        runtime.gateway_attachment.reason_code = Some(V2RuntimeReasonCode::ConnectingToGateway);
        drop(runtime);
        self.reconcile_v2_routes_and_runtime_locked();
    }

    fn mark_v2_runtime_failure(&self, error: &anyhow::Error) {
        use crate::runtime_snapshot::{V2GatewayAttachmentPhase, V2RuntimeReasonCode};

        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none() {
            return;
        }
        let resolving =
            runtime.gateway_attachment.phase == V2GatewayAttachmentPhase::ResolvingThroughPlatform;
        let error_text = error
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let (phase, reason_code) = if resolving {
            if error_text.contains("http 503") || error_text.contains("no eligible gateway") {
                (
                    V2GatewayAttachmentPhase::Unavailable,
                    V2RuntimeReasonCode::NoEligibleGateway,
                )
            } else if error_text.contains("http 409") {
                (
                    V2GatewayAttachmentPhase::ProvisioningScope,
                    V2RuntimeReasonCode::ResolvingThroughPlatform,
                )
            } else if error_text.contains("http 422") || error_text.contains("scope rejected") {
                (
                    V2GatewayAttachmentPhase::Rejected,
                    V2RuntimeReasonCode::ScopeRejected,
                )
            } else {
                (
                    V2GatewayAttachmentPhase::Unavailable,
                    V2RuntimeReasonCode::PlatformUnavailable,
                )
            }
        } else if error_text.contains("certificate")
            || error_text.contains("tls")
            || error_text.contains("unknown issuer")
        {
            (
                V2GatewayAttachmentPhase::TlsFailed,
                V2RuntimeReasonCode::GatewayTlsFailed,
            )
        } else if error_text.contains("auth")
            || error_text.contains("proof")
            || error_text.contains("unknown scope")
            || error_text.contains("rejected")
        {
            (
                V2GatewayAttachmentPhase::Rejected,
                V2RuntimeReasonCode::GatewayAuthenticationRejected,
            )
        } else {
            (
                V2GatewayAttachmentPhase::Unavailable,
                V2RuntimeReasonCode::GatewayConnectFailed,
            )
        };
        runtime.gateway_attachment.phase = phase;
        runtime.gateway_attachment.reason_code = Some(reason_code);
        drop(runtime);
        self.v2_current_membership.write().clear();
        self.reconcile_v2_routes_and_runtime_locked();
    }

    fn observe_v2_peer_membership_locked(
        &self,
        membership: &tp_core::provisioning::PublicPeerMembershipV2,
    ) {
        use crate::runtime_snapshot::{
            V2RemotePeerPhase, V2RemotePeerSnapshot, V2RoutingPhase, V2RuntimeReasonCode,
        };

        let mut runtime = self.v2_runtime.write();
        if runtime.this_peer.is_none() {
            return;
        }
        match runtime
            .peer_directory
            .peers
            .iter_mut()
            .find(|peer| peer.peer_id == membership.peer_id)
        {
            Some(peer) => {
                peer.overlay_ip = Some(membership.overlay_ip);
                if peer.phase == V2RemotePeerPhase::Unavailable {
                    peer.phase = V2RemotePeerPhase::Syncing;
                    peer.reason_code = Some(V2RuntimeReasonCode::PeerLinkUnavailable);
                }
            }
            None => runtime.peer_directory.peers.push(V2RemotePeerSnapshot {
                peer_id: membership.peer_id.clone(),
                overlay_ip: Some(membership.overlay_ip),
                phase: V2RemotePeerPhase::Syncing,
                reason_code: Some(V2RuntimeReasonCode::PeerLinkUnavailable),
                current_path: None,
                usable_lanes: None,
                routing: V2RoutingPhase::Syncing,
                exports: Vec::new(),
            }),
        }
        runtime
            .peer_directory
            .peers
            .sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
        drop(runtime);
        self.reconcile_v2_routes_and_runtime_locked();
    }

    pub fn latest_tunnel_config(&self) -> Option<TunnelConfig> {
        self.latest_tunnel_config.read().clone()
    }

    pub(crate) fn active_v2_peer_profile(&self) -> Option<Arc<PeerProfileV2>> {
        self.active_v2_profile.read().clone()
    }

    #[cfg(test)]
    pub(crate) fn set_active_v2_peer_profile_for_test(&self, profile: Arc<PeerProfileV2>) {
        *self.active_v2_profile.write() = Some(profile.clone());
        self.begin_v2_runtime(&profile);
    }

    #[cfg(test)]
    pub(crate) fn mark_v2_gateway_attached_for_test(&self, gateway_addr: SocketAddr) {
        self.mark_v2_gateway_attached(gateway_addr);
    }

    #[cfg(test)]
    pub(crate) fn clear_active_v2_peer_profile_for_test(&self) {
        *self.active_v2_profile.write() = None;
    }

    pub fn uses_v2_peer_profile(&self) -> bool {
        self.active_v2_profile.read().is_some()
    }

    pub(crate) fn install_v2_peer_membership(
        &self,
        membership: &tp_core::provisioning::PublicPeerMembershipV2,
    ) -> anyhow::Result<()> {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let profile = self
            .active_v2_peer_profile()
            .ok_or_else(|| anyhow::anyhow!("V2 Peer profile is not active"))?;
        membership.verify(&profile.tunnel_signing_public_key)?;
        if membership.tunnel_id != profile.tunnel_id || membership.peer_id == profile.peer.peer_id {
            anyhow::bail!("remote V2 membership is outside the active PeerLink");
        }
        self.overlay_routes
            .write()
            .upsert_peer_overlay(&membership.peer_id, membership.overlay_ip);
        self.observe_v2_peer_membership_locked(membership);
        Ok(())
    }

    pub(crate) fn install_v2_peer_link(
        &self,
        remote_peer_id: String,
        session_id: tp_core::p2p_types::SessionId,
        keys: tp_core::peer_link_crypto::PeerLinkSessionKeysV2,
    ) -> anyhow::Result<()> {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let profile = self
            .active_v2_peer_profile()
            .ok_or_else(|| anyhow::anyhow!("V2 Peer profile is not active"))?;
        if remote_peer_id.trim().is_empty() || remote_peer_id == profile.peer.peer_id {
            anyhow::bail!("invalid remote V2 Peer identity");
        }
        let replacing_current_key = self.v2_peer_links.contains_key(&remote_peer_id);
        self.v2_peer_links.insert(
            remote_peer_id.clone(),
            V2PeerLinkCryptoContext {
                session_id,
                remote_peer_id: remote_peer_id.clone(),
                cipher: Arc::new(crate::relay_crypto::RelayCipherV2::new(&keys)),
            },
        );
        self.ensure_v2_runtime_peer(&remote_peer_id);
        self.reconcile_v2_routes_and_runtime_locked();
        // A key replacement on an already-usable PeerLink is also a fresh
        // authenticated link generation and must push the full current record.
        // The 0→1 case is handled once by the central reconciler above.
        let currently_usable = self
            .v2_runtime
            .read()
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == remote_peer_id)
            .and_then(|peer| peer.usable_lanes)
            .is_some_and(|lanes| lanes > 0);
        if replacing_current_key && currently_usable {
            let outbound = self
                .v2_peer_gossip
                .lock()
                .as_mut()
                .and_then(|gossip| gossip.link_ready(&remote_peer_id, Instant::now()).ok());
            if let Some(outbound) = outbound {
                self.send_v2_gossip_outbound(outbound);
            }
        }
        Ok(())
    }

    fn initialize_v2_peer_gossip(&self) {
        *self.v2_peer_gossip.lock() = Some(crate::peer_gossip::PeerGossipControllerV2::new(
            self.v2_local_runtime_record.read().clone(),
        ));
    }

    fn poll_v2_gossip_digests(&self) {
        let outbound = self
            .v2_peer_gossip
            .lock()
            .as_mut()
            .map(|gossip| gossip.poll_digests(Instant::now()))
            .unwrap_or_default();
        for message in outbound {
            self.send_v2_gossip_outbound(message);
        }
    }

    fn send_v2_gossip_outbound(&self, outbound: crate::peer_gossip::PeerGossipOutboundV2) {
        let Some(link) = self
            .v2_peer_links_for_peer(&outbound.target_peer_id)
            .into_iter()
            .next()
        else {
            return;
        };
        let Some(profile) = self.active_v2_peer_profile() else {
            return;
        };
        let Ok(mut sealed) = outbound.payload.encode() else {
            return;
        };
        let conn_id = [0_u8; 12];
        let context = crate::relay_crypto::RelayRecordContextV2 {
            tunnel_id: &profile.tunnel_id,
            peerlink_session_id: &link.session_id,
            source_peer_id: &profile.peer.peer_id,
            target_peer_id: &outbound.target_peer_id,
            conn_id: &conn_id,
        };
        if link
            .cipher
            .seal_control(context, false, &mut sealed)
            .is_err()
        {
            return;
        }
        let sealed = Bytes::from(sealed);
        let direct = self
            .replica_sessions
            .lock()
            .iter()
            .flat_map(|entry| entry.multi.candidate_paths_for_peer(&link.remote_peer_id))
            .next()
            .map(|candidate| candidate.session)
            .or_else(|| {
                self.multi.lock().as_ref().and_then(|multi| {
                    multi
                        .candidate_paths_for_peer(&link.remote_peer_id)
                        .into_iter()
                        .next()
                        .map(|candidate| candidate.session)
                })
            });
        if let Some(direct) = direct {
            // The Gateway rewrites the Relay outer target into the authenticated
            // source before delivery. Direct has no such hop, so put the source
            // identity in the same field explicitly and verify it at ingress.
            let message = BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id: profile.peer.peer_id.clone(),
                peerlink_session_id: *link.session_id.as_bytes(),
                conn_id,
                route_abort: false,
                sealed,
            };
            self.spawn_engine_task(async move {
                let _ = direct.send(message).await;
            });
            return;
        }
        let relay = {
            let sessions = self.replica_sessions.lock();
            let eligible = sessions
                .iter()
                .find(|entry| entry.relay_active && entry.relay_accepts_new_flows)
                .map(|entry| entry.multi.relay().clone());
            if eligible.is_some() || !sessions.is_empty() {
                eligible
            } else {
                self.multi
                    .lock()
                    .as_ref()
                    .map(|multi| multi.relay().clone())
            }
        };
        if let Some(relay) = relay {
            let message = BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id: outbound.target_peer_id,
                peerlink_session_id: *link.session_id.as_bytes(),
                conn_id,
                route_abort: false,
                sealed,
            };
            self.spawn_engine_task(async move {
                let _ = relay.send(message).await;
            });
        }
    }

    fn receive_v2_gossip(
        &self,
        remote_peer_id: &str,
        payload: crate::relay_crypto::RelayControlPayloadV2,
    ) {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let (response, record) = {
            let mut gossip = self.v2_peer_gossip.lock();
            let Some(gossip) = gossip.as_mut() else {
                return;
            };
            let response = gossip.receive(remote_peer_id, payload).ok().flatten();
            let record = gossip.directory().record(remote_peer_id).cloned();
            (response, record)
        };
        if record.is_some() {
            self.ensure_v2_runtime_peer(remote_peer_id);
            self.reconcile_v2_routes_and_runtime_locked();
        }
        drop(_reconcile_guard);
        if let Some(response) = response {
            self.send_v2_gossip_outbound(response);
        }
    }

    #[allow(dead_code)] // consumed by the Relay AEAD wiring slice
    pub(crate) fn v2_peer_links_for_peer(
        &self,
        remote_peer_id: &str,
    ) -> Vec<V2PeerLinkCryptoContext> {
        self.v2_peer_links
            .get(remote_peer_id)
            .map(|link| vec![link.clone()])
            .unwrap_or_default()
    }

    pub(crate) fn prepare_v2_relay_flow(
        &self,
        conn_id: &str,
        remote_peer_id: &str,
        preferred_session_id: Option<tp_core::p2p_types::SessionId>,
    ) -> Option<crate::p2p::multi_sender::V2RelaySealContext> {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        if !self.has_usable_exact_relay_for_peer(remote_peer_id) {
            return None;
        }
        let profile = self.active_v2_peer_profile()?;
        let link = preferred_session_id
            .and_then(|session_id| {
                self.v2_peer_links
                    .get(remote_peer_id)
                    .filter(|link| link.session_id == session_id)
                    .map(|link| link.clone())
            })
            .or_else(|| {
                self.v2_peer_links_for_peer(remote_peer_id)
                    .into_iter()
                    .next()
            })?;
        let flow = V2RelayFlowCryptoContext {
            tunnel_id: profile.tunnel_id.clone(),
            session_id: link.session_id,
            local_peer_id: profile.peer.peer_id.clone(),
            remote_peer_id: link.remote_peer_id,
            cipher: link.cipher,
            inbound_framed_aad: None,
        }
        .with_inbound_framed_aad(&relay_conn_id_to_wire_v2(conn_id)?)?;
        self.v2_relay_flows
            .insert(conn_id.to_string(), flow.clone());
        flow.seal_context(conn_id)
    }

    #[cfg(test)]
    pub(crate) fn v2_relay_seal_for_flow(
        &self,
        conn_id: &str,
    ) -> Option<crate::p2p::multi_sender::V2RelaySealContext> {
        self.v2_relay_flows
            .get(conn_id)
            .and_then(|flow| flow.seal_context(conn_id))
    }

    fn v2_relay_flow_from_remote(
        &self,
        remote_peer_id: &str,
        session_id: tp_core::p2p_types::SessionId,
    ) -> Option<V2RelayFlowCryptoContext> {
        let profile = self.active_v2_peer_profile()?;
        let link = self.v2_peer_links.get(remote_peer_id)?;
        if link.session_id != session_id {
            return None;
        }
        Some(V2RelayFlowCryptoContext {
            tunnel_id: profile.tunnel_id.clone(),
            session_id,
            local_peer_id: profile.peer.peer_id.clone(),
            remote_peer_id: remote_peer_id.to_string(),
            cipher: link.cipher.clone(),
            inbound_framed_aad: None,
        })
    }

    fn v2_relay_flow_from_session(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> Option<V2RelayFlowCryptoContext> {
        let profile = self.active_v2_peer_profile()?;
        let mut matches = self
            .v2_peer_links
            .iter()
            .filter(|entry| entry.value().session_id == session_id);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(V2RelayFlowCryptoContext {
            tunnel_id: profile.tunnel_id.clone(),
            session_id,
            local_peer_id: profile.peer.peer_id.clone(),
            remote_peer_id: first.key().clone(),
            cipher: first.value().cipher.clone(),
            inbound_framed_aad: None,
        })
    }

    pub(crate) fn install_overlay_replica(
        &self,
        tunnel_id: &str,
        replica_id: &str,
    ) -> Result<std::net::Ipv4Addr, crate::route_matcher::OverlayRouteInstallError> {
        self.overlay_routes
            .write()
            .upsert_replica(tunnel_id, replica_id)
    }

    #[cfg(test)]
    pub(crate) fn replace_peer_lan_aliases(
        &self,
        peer_id: &str,
        lan_ips: &[String],
    ) -> anyhow::Result<()> {
        let tunnel_config = self
            .latest_tunnel_config
            .read()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Tunnel config is unavailable"))?;
        if crate::p2p::replica::replica_seed_for_tunnel(&tunnel_config.tunnel_id, peer_id).is_none()
        {
            anyhow::bail!("LAN alias Peer is outside the active Tunnel");
        }
        if crate::p2p::replica::same_replica_family(&tunnel_config.peer_id, peer_id) {
            anyhow::bail!("LAN aliases cannot select the local Peer");
        }
        let aliases = lan_ips
            .iter()
            .map(|address| {
                address
                    .parse::<std::net::Ipv4Addr>()
                    .map_err(|_| anyhow::anyhow!("invalid LAN host alias {address:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.overlay_routes
            .write()
            .replace_peer_lan_aliases(peer_id, aliases)
            .map_err(anyhow::Error::new)
    }

    pub(crate) fn retire_overlay_peer(&self, peer_id: &str) -> bool {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        // Membership retirement is decided from a sampled connectivity view.
        // A Direct Lane can recover between that sample and this destructive
        // commit, so re-check the live authorities while holding the same
        // fence used by lane installation and runtime reconciliation.
        if self.v2_direct_lane_count_for_peer(peer_id) > 0
            || self.has_usable_exact_relay_for_peer(peer_id)
        {
            self.ensure_v2_runtime_peer(peer_id);
            self.reconcile_v2_routes_and_runtime_locked();
            return false;
        }
        // If retirement wins the registry race, revoke any not-yet-installed
        // Direct generation for this Peer. A concurrent installer that
        // already consumed its reservation holds the registry lock and will
        // finish first; the live-lane check above then preserves the Peer.
        let peer_family = crate::p2p::replica::replica_family_id(peer_id);
        let stale_sessions = self
            .p2p_pending_installs
            .lock()
            .iter()
            .filter_map(|(session_id, pending)| {
                let pending_family =
                    crate::p2p::replica::replica_family_id(&pending.peer_client_id);
                let relation_matches = pending.relation_key.as_ref().is_some_and(|relation| {
                    relation.first_peer_family == peer_family
                        || relation.second_peer_family == peer_family
                });
                (pending_family == peer_family || relation_matches).then_some(*session_id)
            })
            .collect::<Vec<_>>();
        for session_id in stale_sessions {
            self.p2p_pending_installs.lock().remove(&session_id);
            self.p2p_signaling_routes.remove(&session_id);
        }
        let mut multis = self.multi.lock().iter().cloned().collect::<Vec<_>>();
        multis.extend(
            self.replica_sessions
                .lock()
                .iter()
                .map(|entry| entry.multi.clone()),
        );
        for multi in multis {
            multi.close_p2p_sessions_for_peer_without_lane_change_notification(peer_id);
        }
        self.v2_peer_links.remove(peer_id);
        if let Some(gossip) = self.v2_peer_gossip.lock().as_mut() {
            let _ = gossip.link_closed(peer_id);
        }
        {
            let mut routes = self.overlay_routes.write();
            routes.remove_v2_lan_export_origin(peer_id);
            routes.remove_peer(peer_id);
        }
        self.v2_runtime
            .write()
            .peer_directory
            .peers
            .retain(|peer| peer.peer_id != peer_id);
        self.v2_current_membership.write().remove(peer_id);
        self.reconcile_v2_routes_and_runtime_locked();
        // The authority commit succeeded even if an earlier lane-loss path
        // had already withdrawn every route artifact for this Peer.
        true
    }

    pub fn overlay_route_cidrs(&self) -> Vec<String> {
        self.overlay_routes
            .read()
            .route_snapshot()
            .into_iter()
            .map(|(overlay, _peer_id)| format!("{overlay}/32"))
            .collect()
    }

    pub fn v2_active_lan_export_snapshot(
        &self,
    ) -> Vec<(crate::peer_runtime::LanExportPrefixV2, String)> {
        self.overlay_routes.read().v2_active_lan_export_snapshot()
    }

    /// Compile the current process-local V2 LAN Export selection into native
    /// capture routes. Without Tunnel First, an overlapping connected LAN
    /// remains native. With Tunnel First, exact protected addresses are cut
    /// out, so the remaining more-specific routes win without replacing the
    /// connected route. Missing underlay inventory always fails closed.
    pub fn v2_native_lan_route_cidrs(&self, tunnel_first: bool) -> Vec<String> {
        let generation = self.native_lan_route_generation.read();
        if !generation.inventory_ready || !generation.bypass_ready {
            return Vec::new();
        }
        let connected_lans = generation.connected_lans.clone();
        let exclusions = generation.exclusions.clone();
        drop(generation);
        let local_exports = self
            .v2_local_runtime_record
            .read()
            .lan_exports
            .iter()
            .filter(|export| export.ready)
            .map(|export| export.prefix)
            .collect::<Vec<_>>();

        let mut routes = self
            .v2_active_lan_export_snapshot()
            .into_iter()
            .map(|(prefix, _origin_peer_id)| prefix)
            .filter(|prefix| {
                tunnel_first
                    || !connected_lans
                        .iter()
                        .any(|connected| v2_lan_prefixes_overlap(*prefix, *connected))
            })
            .flat_map(|prefix| v2_lan_prefix_without_prefixes(prefix, &local_exports))
            .flat_map(|prefix| v2_lan_prefix_without_hosts(prefix, &exclusions))
            .flat_map(|prefix| {
                if tunnel_first {
                    v2_lan_prefixes_prefer_over_connected(prefix, &connected_lans)
                } else {
                    vec![prefix]
                }
            })
            .collect::<Vec<_>>();
        routes.sort_by_key(|prefix| (u32::from(prefix.network), prefix.prefix_len));
        routes.dedup();
        routes
            .into_iter()
            .map(|prefix| format!("{}/{}", prefix.network, prefix.prefix_len))
            .collect()
    }

    pub fn lan_alias_route_cidrs(&self) -> Vec<String> {
        let Some(exclusions) = self.native_lan_capture_exclusions() else {
            return Vec::new();
        };
        self.overlay_routes
            .read()
            .lan_alias_destinations()
            .into_iter()
            .filter(|address| !exclusions.contains(address))
            .map(|address| format!("{address}/32"))
            .collect()
    }

    pub(crate) fn has_healthy_direct_path_for_peer(&self, peer_id: &str) -> bool {
        self.v2_direct_lane_count_for_peer(peer_id) > 0
    }

    fn v2_direct_lane_count_for_peer(&self, peer_id: &str) -> usize {
        let sessions = self.replica_sessions.lock().clone();
        let anchor = self.multi.lock().clone();
        unique_p2p_multis_from(&sessions, anchor)
            .into_iter()
            .map(|multi| multi.candidate_paths_for_peer(peer_id).len())
            .sum()
    }

    /// Whether the active verified V2 profile matches the attached runtime.
    pub fn uses_exact_peer_routing(&self) -> bool {
        let latest = self.latest_tunnel_config.read();
        if let (Some(profile), Some(config)) =
            (self.active_v2_profile.read().as_ref(), latest.as_ref())
        {
            return config.tunnel_id == profile.tunnel_id
                && config.peer_id == profile.peer.peer_id
                && config.overlay_ipv4 == profile.peer.overlay_ip.to_string();
        }
        false
    }

    /// Resolve only literal IP destinations. Duplicate Overlay or private-LAN
    /// host ownership is a configuration error and must never pick an
    /// arbitrary Peer.
    #[cfg(test)]
    pub(crate) fn resolve_overlay_peer(&self, address: &str) -> anyhow::Result<Option<String>> {
        self.resolve_overlay_peer_with_mode(address, self.active_v2_peer_profile().is_some())
    }

    fn resolve_overlay_peer_with_mode(
        &self,
        address: &str,
        v2_exact_routing: bool,
    ) -> anyhow::Result<Option<String>> {
        let destination = match address.parse::<SocketAddr>() {
            Ok(address) => address.ip(),
            Err(_) => return Ok(None),
        };
        let routes = self.overlay_routes.read();
        let route = if v2_exact_routing {
            routes.match_v2_destination(destination)
        } else {
            routes.match_destination(destination)
        };
        match route {
            crate::route_matcher::OverlayRouteMatch::Peer { peer_id } => Ok(Some(peer_id)),
            crate::route_matcher::OverlayRouteMatch::Unmatched => Ok(None),
            crate::route_matcher::OverlayRouteMatch::Ambiguous => {
                anyhow::bail!("ambiguous exact Peer route for destination {destination}")
            }
        }
    }

    /// Resolve the destination owner before lane placement. In V2, a hostname
    /// is resolved once to a deterministic IPv4 address for exact Overlay or
    /// LAN Export routing; the original hostname remains the Peer request.
    /// Unmatched destinations cannot fall through to an arbitrary Peer.
    pub(crate) async fn resolve_proxy_target_peer(
        &self,
        address: &str,
    ) -> anyhow::Result<ResolvedProxyTarget> {
        let active_v2_profile = self.active_v2_peer_profile();
        let latest = self.latest_tunnel_config();
        let exact_routing = match (active_v2_profile.as_ref(), latest.as_ref()) {
            (Some(profile), Some(config)) => {
                config.tunnel_id == profile.tunnel_id
                    && config.peer_id == profile.peer.peer_id
                    && config.overlay_ipv4 == profile.peer.overlay_ip.to_string()
            }
            _ => false,
        };
        let v2_exact_routing = active_v2_profile.is_some();
        let logical_destination = if exact_routing {
            Some(resolve_target_addr_once(address, true).await?.socket)
        } else {
            address.parse::<SocketAddr>().ok()
        };
        let target = match logical_destination {
            Some(destination) => {
                self.resolve_overlay_peer_with_mode(&destination.to_string(), v2_exact_routing)?
            }
            None => None,
        };
        if exact_routing && target.is_none() {
            anyhow::bail!("no exact Peer route for destination {address}");
        }
        Ok(ResolvedProxyTarget {
            v2_exact_target: v2_exact_routing && target.is_some(),
            peer_id: target,
            logical_destination,
        })
    }

    #[cfg(test)]
    pub(crate) fn install_overlay_peer_for_test(&self, peer_id: &str, overlay: std::net::Ipv4Addr) {
        self.overlay_routes
            .write()
            .upsert_peer_overlay(peer_id, overlay);
    }

    #[cfg(test)]
    pub(crate) fn set_latest_tunnel_config_for_test(&self, config: TunnelConfig) {
        *self.latest_tunnel_config.write() = Some(config);
    }

    fn set_status(&self, new: ConnectionStatus) {
        let new = self.decorate_status(new);
        let mut current = self.state.write();
        if *current == new {
            return;
        }
        *current = new.clone();
        drop(current);
        self.listener.on_status(&new);
    }

    fn emit_status_snapshot(&self) {
        let current = self.state.read().clone();
        self.set_status(current);
    }

    fn start_status_refresh_loop(
        self: &Arc<Self>,
        cancel: CancellationToken,
    ) -> AbortOnDropHandle<()> {
        let engine = self.clone();
        AbortOnDropHandle::new(tokio::spawn(async move {
            let mut ticker = interval(status_refresh_interval());
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = ticker.tick() => engine.emit_status_snapshot(),
                }
            }
        }))
    }

    fn decorate_status(&self, mut status: ConnectionStatus) -> ConnectionStatus {
        {
            let mut connected_since = self.connected_since.lock();
            if status.connected {
                connected_since.get_or_insert_with(Instant::now);
            } else if !status.connecting {
                *connected_since = None;
            }
            status.uptime_secs = if status.connected {
                connected_since
                    .as_ref()
                    .map(|started| started.elapsed().as_secs())
                    .unwrap_or(0)
            } else {
                0
            };
        }
        let replica_sessions = self.replica_sessions.lock().clone();
        let anchor = self.multi.lock().clone();
        let p2p_multis = unique_p2p_multis_from(&replica_sessions, anchor);
        let p2p_installed_sessions = p2p_installed_session_count_in(&p2p_multis);
        let p2p_active_sessions = p2p_eligible_session_count_in(&p2p_multis);
        let p2p_peer_ids = p2p_eligible_peer_ids_in(&p2p_multis);
        let p2p_desired_sessions = if status.connected {
            self.p2p_desired_session_count()
        } else {
            0
        };
        let p2p_degraded = (p2p_installed_sessions > 0
            && p2p_active_sessions < p2p_installed_sessions)
            || (p2p_active_sessions > 0 && p2p_active_sessions < p2p_desired_sessions);
        let p2p_state = if p2p_degraded {
            Some("degraded".to_string())
        } else {
            p2p_multis
                .iter()
                .find(|multi| !multi.p2p_candidate_paths().is_empty())
                .or_else(|| p2p_multis.first())
                .map(|multi| p2p_state_status_label(&multi.p2p_state()).to_string())
        };
        let p2p_installed = p2p_active_sessions > 0;
        status.path_mode = derive_path_mode(status.connected, status.connecting, p2p_installed);
        status.p2p_active_sessions = p2p_active_sessions;
        status.p2p_primary_peer_id = p2p_peer_ids.first().cloned();
        status.p2p_peer_count = p2p_peer_ids.len();
        status.p2p_state = p2p_state;
        status.traffic = self.traffic.snapshot();
        status
    }

    fn reset_replica_sessions_for_connect(&self, anchor_client_id: Option<String>) {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        // A Gateway Attachment generation owns Relay, signaling, and pending
        // installs. Installed Direct sessions are endpoint-to-endpoint and may
        // outlive every Gateway replica, so retain their MultiSession as a
        // P2P-only lane. Explicit disconnect remains the owner that closes it.
        self.replica_sessions.lock().retain_mut(|entry| {
            let keep_direct = entry.multi.p2p_session_count() > 0;
            if keep_direct {
                entry.relay_active = false;
                entry.relay_accepts_new_flows = false;
            }
            keep_direct
        });
        self.proxy_replica_rr.store(0, Ordering::Relaxed);
        self.proxy_flow_registry.clear_relay();
        self.p2p_install_rr.store(0, Ordering::Relaxed);
        self.p2p_pending_installs.lock().clear();
        self.relay_transport_generations.clear();
        self.relay_inbound_attestations.clear();
        self.v2_relay_flows.clear();
        *self.p2p_anchor_client_id.lock() = anchor_client_id;
        *self.multi.lock() = None;
        *self.tunnel_identity.lock() = None;
        *self.group_context.lock() = None;
        self.p2p_signaling_routes.clear();
    }

    #[cfg(test)]
    fn register_replica_multi_session(
        &self,
        client_id: &str,
        group_id: &str,
        multi: Arc<crate::p2p::session::MultiSession>,
        transport_generation: u64,
    ) {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        self.register_replica_multi_session_locked(
            client_id,
            group_id,
            multi,
            transport_generation,
        );
    }

    fn register_replica_multi_session_locked(
        &self,
        client_id: &str,
        group_id: &str,
        multi: Arc<crate::p2p::session::MultiSession>,
        transport_generation: u64,
    ) {
        if let Some(metrics) = self.metrics() {
            multi.set_metrics(Some(metrics));
        }
        multi.set_traffic(Some(self.traffic.clone()));

        {
            let mut sessions = self.replica_sessions.lock();
            sessions.retain(|entry| {
                !(entry.client_id == client_id
                    && entry.transport_generation == transport_generation
                    && Arc::ptr_eq(&entry.multi, &multi))
            });
            sessions.push(ReplicaMultiSession {
                client_id: client_id.to_string(),
                multi: multi.clone(),
                relay_active: true,
                relay_accepts_new_flows: true,
                transport_generation,
            });
        }

        let anchor_matches = {
            let mut anchor = self.p2p_anchor_client_id.lock();
            if anchor.is_none() {
                *anchor = Some(client_id.to_string());
            }
            anchor.as_deref() == Some(client_id)
        };
        if anchor_matches {
            *self.multi.lock() = Some(multi);
            *self.tunnel_identity.lock() = Some((client_id.to_string(), group_id.to_string()));
        }
    }

    // This is the atomic publication boundary for all replica identity and state.
    #[allow(clippy::too_many_arguments)]
    fn publish_connected_replica_if_active(
        &self,
        cancel: &CancellationToken,
        client_id: &str,
        group_id: &str,
        multi: Arc<crate::p2p::session::MultiSession>,
        transport_generation: u64,
        replica_activity: &ReplicaActivity,
        gateway_addr: SocketAddr,
    ) -> bool {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        if cancel.is_cancelled() {
            return false;
        }
        self.register_replica_multi_session_locked(
            client_id,
            group_id,
            multi,
            transport_generation,
        );
        replica_activity.mark_connected(self, gateway_addr);
        true
    }

    #[cfg(test)]
    fn unregister_replica_multi_session(
        &self,
        client_id: &str,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        self.unregister_replica_multi_session_inner(client_id, multi, true);
    }

    pub(crate) fn unregister_relay_closed_multi_session(
        &self,
        client_id: &str,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        self.unregister_replica_multi_session_inner(client_id, multi, false);
    }

    fn mark_relay_session_unusable_for_new_flows(
        &self,
        client_id: &str,
        transport_generation: u64,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> bool {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut sessions = self.replica_sessions.lock();
        let mut changed = false;
        for entry in sessions.iter_mut() {
            if entry.client_id == client_id
                && entry.transport_generation == transport_generation
                && Arc::ptr_eq(&entry.multi, multi)
                && entry.relay_accepts_new_flows
            {
                entry.relay_accepts_new_flows = false;
                changed = true;
            }
        }
        drop(sessions);
        if changed {
            self.reconcile_v2_routes_and_runtime_locked();
        }
        changed
    }

    fn mark_relay_session_usable_for_new_flows(
        &self,
        client_id: &str,
        transport_generation: u64,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> bool {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let mut sessions = self.replica_sessions.lock();
        let mut changed = false;
        for entry in sessions.iter_mut() {
            if entry.client_id == client_id
                && entry.transport_generation == transport_generation
                && Arc::ptr_eq(&entry.multi, multi)
                && entry.relay_active
                && !entry.relay_accepts_new_flows
            {
                entry.relay_accepts_new_flows = true;
                changed = true;
            }
        }
        drop(sessions);
        if changed {
            self.reconcile_v2_routes_and_runtime_locked();
        }
        changed
    }

    fn unregister_replica_multi_session_inner(
        &self,
        client_id: &str,
        multi: &Arc<crate::p2p::session::MultiSession>,
        close_p2p: bool,
    ) {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        self.clear_relay_inbound_attestations_for_generation(multi);
        let pending_bind_ids: Vec<_> = self
            .relay_route_bind_pending
            .iter()
            .filter(|pending| {
                pending
                    .relay_generation
                    .upgrade()
                    .is_none_or(|bound| Arc::ptr_eq(&bound, multi))
            })
            .map(|pending| pending.key().clone())
            .collect();
        for conn_id in pending_bind_ids {
            if let Some((_, pending)) = self.relay_route_bind_pending.remove(&conn_id) {
                let _ = pending
                    .response
                    .send(Err("relay session generation disconnected".into()));
            }
        }
        {
            let mut sessions = self.replica_sessions.lock();
            if close_p2p || multi.p2p_session_count() == 0 {
                sessions.retain(|entry| {
                    !(entry.client_id == client_id && Arc::ptr_eq(&entry.multi, multi))
                });
            } else {
                for entry in sessions.iter_mut() {
                    if entry.client_id == client_id && Arc::ptr_eq(&entry.multi, multi) {
                        entry.relay_active = false;
                        entry.relay_accepts_new_flows = false;
                    }
                }
            }
        }
        self.p2p_signaling_routes
            .retain(|_, route| !Arc::ptr_eq(route, multi));
        self.p2p_pending_membership_batches
            .lock()
            .remove(&p2p_relay_instance_key(multi));
        let has_remaining_replicas = !self.replica_sessions.lock().is_empty();

        let mut slot = self.multi.lock();
        if slot
            .as_ref()
            .map(|live| Arc::ptr_eq(live, multi))
            .unwrap_or(false)
        {
            *slot = None;
            *self.tunnel_identity.lock() = None;
        }
        drop(slot);
        if !has_remaining_replicas {
            *self.group_context.lock() = None;
        }
        if close_p2p {
            multi.close_all_p2p();
        }
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        self.reconcile_v2_routes_and_runtime_locked();
    }

    fn p2p_relay_multi_is_active_locked(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> bool {
        if self
            .replica_sessions
            .lock()
            .iter()
            .any(|entry| entry.relay_active && Arc::ptr_eq(&entry.multi, multi))
        {
            return true;
        }
        self.multi
            .lock()
            .as_ref()
            .map(|live| Arc::ptr_eq(live, multi))
            .unwrap_or(false)
    }

    fn p2p_membership_authority_for_multi_locked(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> Option<P2pMembershipBatchAuthority> {
        self.replica_sessions
            .lock()
            .iter()
            .find(|entry| entry.relay_active && Arc::ptr_eq(&entry.multi, multi))
            .map(|entry| P2pMembershipBatchAuthority {
                source_multi: multi.clone(),
                client_id: entry.client_id.clone(),
                transport_generation: entry.transport_generation,
            })
    }

    fn p2p_membership_authority_is_active_locked(
        &self,
        authority: &P2pMembershipBatchAuthority,
    ) -> bool {
        self.replica_sessions.lock().iter().any(|entry| {
            entry.relay_active
                && entry.client_id == authority.client_id
                && entry.transport_generation == authority.transport_generation
                && Arc::ptr_eq(&entry.multi, &authority.source_multi)
        })
    }

    fn insert_p2p_signaling_route_from_relay(
        &self,
        session_id: tp_core::p2p_types::SessionId,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> bool {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        if !self.p2p_relay_multi_is_active_locked(multi) {
            return false;
        }
        self.p2p_signaling_routes.insert(session_id, multi.clone());
        true
    }

    fn p2p_relay_multi_is_active(&self, multi: &Arc<crate::p2p::session::MultiSession>) -> bool {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        self.p2p_relay_multi_is_active_locked(multi)
    }

    fn remove_p2p_signaling_route_for_multi(
        &self,
        session_id: tp_core::p2p_types::SessionId,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        self.p2p_signaling_routes
            .remove_if(&session_id, |_, route| Arc::ptr_eq(route, multi));
    }

    fn local_client_id_for_multi(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> Option<String> {
        if let Some(entry) = self
            .replica_sessions
            .lock()
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.multi, multi))
            .cloned()
        {
            return Some(entry.client_id);
        }
        self.multi
            .lock()
            .as_ref()
            .filter(|anchor| Arc::ptr_eq(anchor, multi))
            .and_then(|_| {
                self.tunnel_identity
                    .lock()
                    .as_ref()
                    .map(|(client_id, _)| client_id.clone())
            })
    }

    #[cfg(test)]
    pub(crate) fn pick_proxy_relay_lane(&self) -> Option<ProxyLane> {
        let sessions = self.replica_sessions.lock();
        let relay_sessions: Vec<_> = sessions
            .iter()
            .filter(|entry| entry.relay_active && entry.relay_accepts_new_flows)
            .cloned()
            .collect();
        if !relay_sessions.is_empty() {
            let idx = self.proxy_replica_rr.fetch_add(1, Ordering::Relaxed) % relay_sessions.len();
            let entry = relay_sessions[idx].clone();
            return Some(ProxyLane {
                local_client_id: entry.client_id,
                multi: entry.multi,
            });
        }
        drop(sessions);
        self.multi_session().map(|multi| ProxyLane {
            local_client_id: self
                .tunnel_identity
                .lock()
                .as_ref()
                .map(|(client_id, _)| client_id.clone())
                .unwrap_or_else(|| "anchor".into()),
            multi,
        })
    }

    #[cfg(test)]
    pub(crate) fn pick_proxy_flow_lane(
        &self,
        flow_kind: FlowKind,
        excludes: &[ProxyFlowAttemptExclude],
    ) -> Option<ProxyFlowLane> {
        let _guard = self.proxy_flow_placement_lock.lock();
        self.pick_proxy_flow_lane_locked(flow_kind, excludes, None, false)
    }

    #[cfg(test)]
    pub(crate) fn pick_proxy_flow_lane_for_peer(
        &self,
        flow_kind: FlowKind,
        excludes: &[ProxyFlowAttemptExclude],
        target_peer_family: Option<&str>,
        v2_exact_target: bool,
    ) -> Option<ProxyFlowLane> {
        let _guard = self.proxy_flow_placement_lock.lock();
        self.pick_proxy_flow_lane_locked(flow_kind, excludes, target_peer_family, v2_exact_target)
    }

    #[cfg(test)]
    pub(crate) fn pick_and_record_proxy_flow_lane(
        &self,
        conn_id: &str,
        flow_kind: FlowKind,
        excludes: &[ProxyFlowAttemptExclude],
    ) -> Option<ProxyFlowLane> {
        self.pick_and_record_proxy_flow_lane_for_peer(conn_id, flow_kind, excludes, None, false)
    }

    pub(crate) fn pick_and_record_proxy_flow_lane_for_peer(
        &self,
        conn_id: &str,
        flow_kind: FlowKind,
        excludes: &[ProxyFlowAttemptExclude],
        target_peer_family: Option<&str>,
        v2_exact_target: bool,
    ) -> Option<ProxyFlowLane> {
        let _guard = self.proxy_flow_placement_lock.lock();
        let lane = self.pick_proxy_flow_lane_locked(
            flow_kind,
            excludes,
            target_peer_family,
            v2_exact_target,
        )?;
        self.proxy_flow_registry.record_pending(
            conn_id.to_string(),
            flow_kind,
            lane.candidate_key.clone(),
        );
        Some(lane)
    }

    fn pick_proxy_flow_lane_locked(
        &self,
        flow_kind: FlowKind,
        excludes: &[ProxyFlowAttemptExclude],
        target_peer_family: Option<&str>,
        v2_exact_target: bool,
    ) -> Option<ProxyFlowLane> {
        // A destination resolved before V2 activation cannot be reinterpreted
        // as an exact Peer after V2 becomes authoritative. The caller retries
        // from resolution and obtains a fresh V2 route instead.
        if !v2_exact_target && self.uses_v2_peer_profile() {
            return None;
        }
        let v2_exact_target = v2_exact_target && target_peer_family.is_some();
        let target_peer_family = target_peer_family.map(|peer_id| {
            if v2_exact_target {
                peer_id.to_string()
            } else {
                crate::p2p::replica::replica_family_id(peer_id)
            }
        });
        let sessions = self.replica_sessions.lock().clone();
        let anchor = if sessions.is_empty() {
            self.multi_session().map(|multi| {
                let local_client_id = self
                    .tunnel_identity
                    .lock()
                    .as_ref()
                    .map(|(client_id, _)| client_id.clone())
                    .or_else(|| {
                        self.group_context
                            .lock()
                            .as_ref()
                            .and_then(|ctx| ctx.anchor_client_id.clone())
                    })
                    .unwrap_or_else(|| "anchor".into());
                ReplicaMultiSession {
                    client_id: local_client_id,
                    multi,
                    relay_active: true,
                    relay_accepts_new_flows: true,
                    transport_generation: 0,
                }
            })
        } else {
            None
        };
        let lanes: Vec<ReplicaMultiSession> = if sessions.is_empty() {
            anchor.into_iter().collect()
        } else {
            sessions
        };
        if lanes.is_empty() {
            return None;
        }

        let exact_v2_relay_authorized = match target_peer_family.as_deref() {
            Some(peer_id) if v2_exact_target => self.has_usable_exact_relay_for_peer(peer_id),
            _ => true,
        };

        let mut flow_lanes = Vec::new();
        let mut candidates = Vec::new();
        for entry in &lanes {
            let p2p_candidates = match target_peer_family.as_deref() {
                Some(peer_id) if v2_exact_target => entry
                    .multi
                    .p2p_candidate_paths()
                    .into_iter()
                    .filter(|candidate| candidate.peer_client_id == peer_id)
                    .collect(),
                Some(peer_family) => entry.multi.candidate_paths_for_peer(peer_family),
                None => entry.multi.p2p_candidate_paths(),
            };
            for p2p in p2p_candidates {
                let key = CandidateKey {
                    local_client_id: entry.client_id.clone(),
                    path: CandidatePath::P2p,
                    p2p_session_id: Some(p2p.session_id),
                    peer_client_id: Some(p2p.peer_client_id.clone()),
                    peer_family: Some(if v2_exact_target {
                        target_peer_family
                            .as_ref()
                            .expect("V2 exact target checked")
                            .clone()
                    } else {
                        p2p.peer_family.clone()
                    }),
                    transport_generation: 0,
                };
                let lane = ProxyFlowLane {
                    local_client_id: entry.client_id.clone(),
                    multi: entry.multi.clone(),
                    path: crate::p2p::scheduler::PathKind::P2p,
                    p2p_session_id: Some(p2p.session_id),
                    p2p_session: Some(p2p.session),
                    target_peer_client_id: target_peer_family.as_ref().map(|_| p2p.peer_client_id),
                    v2_exact_target,
                    candidate_key: key.clone(),
                };
                candidates.push(self.proxy_flow_candidate_for_lane(flow_kind, &lane));
                flow_lanes.push(lane);
            }
        }
        for entry in lanes {
            if !entry.relay_active || !entry.relay_accepts_new_flows || !exact_v2_relay_authorized {
                continue;
            }
            let target_peer_client_id = match target_peer_family.as_deref() {
                Some(peer_family) => {
                    if v2_exact_target {
                        // V2 exact routing addresses the issuer-signed stable
                        // Peer. The Gateway selects one attached runtime
                        // Replica; its ephemeral spelling is not identity.
                        Some(peer_family.to_string())
                    } else {
                        let Some(index) = crate::p2p::replica::replica_index(&entry.client_id)
                        else {
                            continue;
                        };
                        let Some(replica_id) =
                            crate::p2p::replica::replica_id_for_index(peer_family, index)
                        else {
                            continue;
                        };
                        Some(replica_id)
                    }
                }
                None => None,
            };
            let key = match target_peer_client_id.as_deref() {
                Some(peer_client_id) => CandidateKey::relay_to_peer(
                    entry.client_id.clone(),
                    entry.transport_generation,
                    peer_client_id,
                ),
                None => CandidateKey::relay(entry.client_id.clone(), entry.transport_generation),
            };
            let lane = ProxyFlowLane {
                local_client_id: entry.client_id.clone(),
                multi: entry.multi.clone(),
                path: crate::p2p::scheduler::PathKind::Relay,
                p2p_session_id: None,
                p2p_session: None,
                target_peer_client_id,
                v2_exact_target,
                candidate_key: key,
            };
            candidates.push(self.proxy_flow_candidate_for_lane(flow_kind, &lane));
            flow_lanes.push(lane);
        }

        for (candidate, lane) in candidates.iter_mut().zip(flow_lanes.iter()) {
            if excludes.iter().any(|exclude| exclude.matches(lane)) {
                candidate.excluded_reason = PlacementExcludedReason::AttemptTimeout;
            }
        }

        let decision = self.proxy_flow_scheduler.place_proxy_flow(
            flow_kind,
            &self.proxy_flow_registry,
            candidates,
        );
        let selected = decision.selected?;
        flow_lanes
            .into_iter()
            .find(|lane| lane.candidate_key == selected)
    }

    fn proxy_flow_candidate_for_lane(
        &self,
        _flow_kind: FlowKind,
        lane: &ProxyFlowLane,
    ) -> PlacementCandidate {
        let mut load = LaneLoadSnapshot::default();
        if let Some(snapshot) = lane.multi.queue_snapshot_for_candidate(&lane.candidate_key) {
            load = load.with_queue_snapshot(&snapshot, snapshot.udp_route_stats.dropped_full);
        }
        PlacementCandidate {
            key: lane.candidate_key.clone(),
            load,
            excluded_reason: PlacementExcludedReason::None,
        }
    }

    fn relay_source_active_flow_counter(
        self: &Arc<Self>,
        local_client_id: String,
        transport_generation: u64,
    ) -> Arc<dyn Fn() -> LinkActiveFlowSnapshot + Send + Sync> {
        let engine = self.clone();
        Arc::new(move || {
            let relay = engine
                .proxy_flow_registry
                .relay_attachment_snapshot(&local_client_id, transport_generation);
            LinkActiveFlowSnapshot {
                active_tcp_flows: relay.active_tcp,
                active_udp_flows: relay.active_udp,
                last_link_io_progress_ms: relay.last_link_io_progress_ms,
            }
        })
    }

    fn p2p_source_active_flow_counter(
        self: &Arc<Self>,
        local_client_id: String,
        session_id: tp_core::p2p_types::SessionId,
        peer_client_id: String,
    ) -> Arc<dyn Fn() -> LinkActiveFlowSnapshot + Send + Sync> {
        let engine = self.clone();
        let peer_family = crate::p2p::replica::replica_family_id(&peer_client_id);
        Arc::new(move || {
            let key = CandidateKey {
                local_client_id: local_client_id.clone(),
                path: CandidatePath::P2p,
                p2p_session_id: Some(session_id),
                peer_client_id: Some(peer_client_id.clone()),
                peer_family: Some(peer_family.clone()),
                transport_generation: 0,
            };
            let (active_tcp_flows, active_udp_flows) =
                engine.proxy_flow_registry.active_counts_for_candidate(&key);
            LinkActiveFlowSnapshot {
                active_tcp_flows,
                active_udp_flows,
                last_link_io_progress_ms: engine
                    .proxy_flow_registry
                    .last_link_io_progress_ms_for_candidate(&key),
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn reserve_p2p_session_install(
        &self,
        session_id: tp_core::p2p_types::SessionId,
        preferred_client_id: Option<&str>,
        peer_client_id: Option<&str>,
    ) -> bool {
        self.reserve_p2p_session_install_for_relation(
            session_id,
            preferred_client_id,
            peer_client_id,
            None,
        )
    }

    pub(crate) fn reserve_p2p_session_install_for_relation(
        &self,
        session_id: tp_core::p2p_types::SessionId,
        preferred_client_id: Option<&str>,
        peer_client_id: Option<&str>,
        relation_key: Option<PeerRelationKey>,
    ) -> bool {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        if relation_key.as_ref().is_some_and(|relation_key| {
            self.p2p_pending_installs
                .lock()
                .values()
                .any(|pending| pending.relation_key.as_ref() == Some(relation_key))
                || self.has_eligible_p2p_relation(relation_key)
        }) {
            return false;
        }
        let Some(multi) = self.pick_p2p_install_multi_session(preferred_client_id) else {
            return false;
        };
        let peer_client_id = peer_client_id
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{session_id:?}"));

        let local_client_id = self
            .local_client_id_for_multi(&multi)
            .or_else(|| {
                self.group_context
                    .lock()
                    .as_ref()
                    .and_then(|ctx| ctx.anchor_client_id.clone())
            })
            .or_else(|| {
                self.tunnel_identity
                    .lock()
                    .as_ref()
                    .map(|(client_id, _)| client_id.clone())
            })
            .unwrap_or_else(|| "anchor".into());
        let refill_permit = if peer_client_id != format!("{session_id:?}") {
            let key = LinkRefillKey::p2p(&local_client_id, &peer_client_id);
            let max_links = self.max_links_per_relation();
            let current_links = self.current_p2p_link_count_for_relation(&key);
            let Some(permit) =
                self.link_refill_limiter
                    .try_acquire(key.clone(), max_links, current_links)
            else {
                tracing::warn!(
                    %local_client_id,
                    peer_client_id = %peer_client_id,
                    current_links,
                    max_links,
                    "P2P link refill capped; skipping P2P session reservation"
                );
                return false;
            };
            Some(permit)
        } else {
            None
        };
        self.p2p_pending_installs.lock().insert(
            session_id,
            PendingP2pInstall {
                multi: multi.clone(),
                peer_client_id,
                relation_key,
                refill_permit,
            },
        );
        self.p2p_signaling_routes.insert(session_id, multi);
        true
    }

    fn has_eligible_p2p_relation(&self, relation_key: &PeerRelationKey) -> bool {
        let replica_sessions = self.replica_sessions.lock().clone();
        let anchor = self.multi.lock().clone();
        unique_p2p_multis_from(&replica_sessions, anchor)
            .iter()
            .any(|multi| multi.has_eligible_p2p_relation(relation_key))
    }

    pub(crate) fn has_live_or_pending_p2p_relation(&self, relation_key: &PeerRelationKey) -> bool {
        self.p2p_pending_installs
            .lock()
            .values()
            .any(|pending| pending.relation_key.as_ref() == Some(relation_key))
            || self.has_eligible_p2p_relation(relation_key)
    }

    pub(crate) fn unreserve_p2p_session_install(&self, session_id: tp_core::p2p_types::SessionId) {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        self.p2p_pending_installs.lock().remove(&session_id);
    }

    pub(crate) fn expire_p2p_session_install(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> crate::p2p::installer::P2pInstallExpiration {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        if self
            .p2p_pending_installs
            .lock()
            .remove(&session_id)
            .is_some()
        {
            return crate::p2p::installer::P2pInstallExpiration::Expired;
        }
        if self.has_p2p_session_by_id(session_id) {
            crate::p2p::installer::P2pInstallExpiration::Installed
        } else {
            crate::p2p::installer::P2pInstallExpiration::Missing
        }
    }

    pub(crate) fn update_p2p_pending_peer_client_id(
        &self,
        session_id: tp_core::p2p_types::SessionId,
        peer_client_id: &str,
    ) {
        let peer_client_id = peer_client_id.trim();
        if peer_client_id.is_empty() {
            return;
        }
        let _registry_guard = self.p2p_session_registry_lock.lock();
        if let Some(pending) = self.p2p_pending_installs.lock().get_mut(&session_id) {
            pending.peer_client_id = peer_client_id.to_string();
        }
    }

    pub(crate) fn has_pending_p2p_session_install(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> bool {
        self.p2p_pending_installs.lock().contains_key(&session_id)
    }

    #[allow(dead_code)]
    pub(crate) fn p2p_active_session_count(&self) -> usize {
        let replica_sessions = self.replica_sessions.lock().clone();
        let anchor = self.multi.lock().clone();
        let p2p_multis = unique_p2p_multis_from(&replica_sessions, anchor);
        p2p_installed_session_count_in(&p2p_multis)
    }

    pub(crate) fn p2p_eligible_session_count(&self) -> usize {
        let replica_sessions = self.replica_sessions.lock().clone();
        let anchor = self.multi.lock().clone();
        let p2p_multis = unique_p2p_multis_from(&replica_sessions, anchor);
        p2p_eligible_session_count_in(&p2p_multis)
    }

    pub(crate) fn p2p_pending_session_count(&self) -> usize {
        self.p2p_pending_installs.lock().len()
    }

    pub(crate) fn p2p_available_install_client_ids(&self) -> Vec<String> {
        let pending: Vec<Arc<crate::p2p::session::MultiSession>> = self
            .p2p_pending_installs
            .lock()
            .values()
            .map(|pending| pending.multi.clone())
            .collect();
        let mut out = Vec::new();
        for entry in self.replica_sessions.lock().iter() {
            if entry.relay_active
                && entry.multi.p2p_eligible_session_count() == 0
                && !pending
                    .iter()
                    .any(|pending_multi| Arc::ptr_eq(pending_multi, &entry.multi))
            {
                out.push(entry.client_id.clone());
            }
        }
        if out.is_empty() {
            if let Some(anchor) = self.multi.lock().clone() {
                let anchor_pending = pending
                    .iter()
                    .any(|pending_multi| Arc::ptr_eq(pending_multi, &anchor));
                if anchor.p2p_eligible_session_count() == 0 && !anchor_pending {
                    if let Some((client_id, _)) = self.tunnel_identity.lock().clone() {
                        out.push(client_id);
                    }
                }
            }
        }
        out
    }

    pub(crate) fn p2p_desired_session_count(&self) -> usize {
        self.replicas
            .lock()
            .unwrap_or_else(|| {
                let replica_count = self.replica_sessions.lock().len();
                if replica_count == 0 {
                    1
                } else {
                    replica_count
                }
            })
            .max(1)
    }

    fn max_links_per_relation(&self) -> usize {
        self.p2p_desired_session_count().saturating_mul(3).max(3)
    }

    fn current_p2p_link_count_for_relation(&self, key: &LinkRefillKey) -> usize {
        let LinkRefillKey::P2p {
            endpoint_a_family,
            endpoint_b_family,
        } = key;
        let replica_sessions = self.replica_sessions.lock().clone();
        let anchor = self.multi.lock().clone();
        let p2p_multis = unique_p2p_multis_from(&replica_sessions, anchor);
        p2p_multis
            .iter()
            .flat_map(|multi| multi.p2p_installed_paths())
            .filter(|p2p| {
                let peer_family = &p2p.peer_family;
                peer_family == endpoint_a_family || peer_family == endpoint_b_family
            })
            .count()
    }

    pub(crate) fn set_p2p_refill_handle(&self, handle: crate::p2p::manager::P2pRefillHandle) {
        *self.p2p_refill_handle.lock() = Some(handle);
    }

    fn request_p2p_refill(&self, peer_client_id: &str) {
        #[cfg(test)]
        self.p2p_refill_requests
            .entry(peer_client_id.to_string())
            .or_insert_with(|| AtomicUsize::new(0))
            .fetch_add(1, Ordering::Relaxed);

        if let Some(handle) = self.p2p_refill_handle.lock().clone() {
            handle.request_refill(peer_client_id);
        }
    }

    fn notify_p2p_relation_closed(&self, session_id: tp_core::p2p_types::SessionId) {
        let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
        let remote_peer_id = self
            .v2_peer_links
            .iter()
            .find(|link| link.session_id == session_id)
            .map(|link| link.remote_peer_id.clone());
        if let Some(remote_peer_id) = remote_peer_id {
            self.ensure_v2_runtime_peer(&remote_peer_id);
        }
        // The closing Direct handle may belong to an older PeerLink key or one
        // of several lanes. Recompute all known Peers from the actual eligible
        // session registry instead of guessing identity from the newest key.
        self.reconcile_v2_routes_and_runtime_locked();
        drop(_reconcile_guard);
        if let Some(handle) = self.p2p_refill_handle.lock().clone() {
            handle.relation_closed(session_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn p2p_refill_requested_for_test(&self, peer_client_id: &str) -> usize {
        self.p2p_refill_requests
            .get(peer_client_id)
            .map(|count| count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn next_relay_transport_generation(&self, client_id: &str) -> u64 {
        self.relay_transport_generations
            .entry(client_id.to_string())
            .or_insert_with(|| AtomicU64::new(1))
            .fetch_add(1, Ordering::SeqCst)
    }

    pub(crate) fn close_p2p_session_by_id(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> bool {
        let _registry_guard = self.p2p_session_registry_lock.lock();
        let anchor = self.multi.lock().clone();
        if let Some(multi) = anchor.as_ref() {
            if close_multi_p2p_session_if_active(multi, session_id) {
                return true;
            }
        }
        let replicas: Vec<Arc<crate::p2p::session::MultiSession>> = self
            .replica_sessions
            .lock()
            .iter()
            .map(|entry| entry.multi.clone())
            .collect();
        for multi in replicas {
            if close_multi_p2p_session_if_active(&multi, session_id) {
                return true;
            }
        }
        false
    }

    pub(crate) fn has_p2p_session_by_id(&self, session_id: tp_core::p2p_types::SessionId) -> bool {
        let anchor = self.multi.lock().clone();
        if let Some(multi) = anchor.as_ref() {
            if multi_p2p_session_is_active(multi, session_id) {
                return true;
            }
        }
        let replicas: Vec<Arc<crate::p2p::session::MultiSession>> = self
            .replica_sessions
            .lock()
            .iter()
            .map(|entry| entry.multi.clone())
            .collect();
        replicas
            .iter()
            .any(|multi| multi_p2p_session_is_active(multi, session_id))
    }

    #[cfg(test)]
    pub(crate) fn has_pending_p2p_session_install_for_test(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> bool {
        self.has_pending_p2p_session_install(session_id)
    }

    #[cfg(test)]
    pub(crate) fn pending_p2p_peer_client_id_for_test(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> Option<String> {
        self.p2p_pending_installs
            .lock()
            .get(&session_id)
            .map(|pending| pending.peer_client_id.clone())
    }

    #[cfg(test)]
    pub(crate) fn pending_p2p_local_client_id_for_test(
        &self,
        session_id: tp_core::p2p_types::SessionId,
    ) -> Option<String> {
        let pending_multi = self
            .p2p_pending_installs
            .lock()
            .get(&session_id)
            .map(|pending| pending.multi.clone())?;
        self.replica_sessions
            .lock()
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.multi, &pending_multi))
            .map(|entry| entry.client_id.clone())
    }

    fn pick_p2p_install_multi_session(
        &self,
        preferred_client_id: Option<&str>,
    ) -> Option<Arc<crate::p2p::session::MultiSession>> {
        let sessions = self.replica_sessions.lock();
        if let Some(client_id) = preferred_client_id {
            return sessions
                .iter()
                .find(|entry| {
                    entry.relay_active
                        && entry.relay_accepts_new_flows
                        && entry.client_id == client_id
                })
                .map(|entry| entry.multi.clone());
        }
        let relay_sessions: Vec<_> = sessions
            .iter()
            .filter(|entry| entry.relay_active && entry.relay_accepts_new_flows)
            .cloned()
            .collect();
        if !relay_sessions.is_empty() {
            let idx = self.p2p_install_rr.fetch_add(1, Ordering::Relaxed) % relay_sessions.len();
            return Some(relay_sessions[idx].multi.clone());
        }
        drop(sessions);
        self.multi_session().filter(|multi| {
            self.multi
                .lock()
                .as_ref()
                .map(|live| Arc::ptr_eq(live, multi))
                .unwrap_or(false)
        })
    }

    pub fn has_proxy_sessions(&self) -> bool {
        !self.replica_sessions.lock().is_empty() || self.multi.lock().is_some()
    }

    pub fn p2p_relay_context(
        &self,
    ) -> Option<(String, String, Arc<crate::p2p::session::MultiSession>)> {
        let ctx = self.group_context.lock().clone();
        let identity = self.tunnel_identity.lock().clone();
        let group_id = ctx
            .as_ref()
            .map(|ctx| ctx.group_id.clone())
            .or_else(|| identity.as_ref().map(|(_, group_id)| group_id.clone()))?;
        let sessions = self.replica_sessions.lock();
        if let Some(anchor_id) = self.p2p_anchor_client_id.lock().clone() {
            if let Some(entry) = sessions.iter().find(|entry| {
                entry.relay_active && entry.relay_accepts_new_flows && entry.client_id == anchor_id
            }) {
                return Some((entry.client_id.clone(), group_id, entry.multi.clone()));
            }
        }
        if let Some(entry) = sessions
            .iter()
            .find(|entry| entry.relay_active && entry.relay_accepts_new_flows)
        {
            return Some((entry.client_id.clone(), group_id, entry.multi.clone()));
        }
        drop(sessions);
        let multi = self.multi.lock().clone()?;
        let client_id = self
            .group_context
            .lock()
            .as_ref()
            .and_then(|ctx| ctx.anchor_client_id.clone())
            .or_else(|| identity.as_ref().map(|(client_id, _)| client_id.clone()))?;
        Some((client_id, group_id, multi))
    }

    fn exact_live_p2p_relay_context(
        &self,
        client_id: &str,
    ) -> Option<(String, Arc<crate::p2p::session::MultiSession>)> {
        let sessions = self.replica_sessions.lock();
        if let Some(entry) = sessions
            .iter()
            .find(|entry| entry.relay_active && entry.client_id == client_id)
        {
            return Some((entry.client_id.clone(), entry.multi.clone()));
        }
        drop(sessions);
        self.p2p_relay_context()
            .filter(|(live_client_id, _, _)| live_client_id == client_id)
            .map(|(live_client_id, _, multi)| (live_client_id, multi))
    }

    fn build_replica_transport(
        &self,
        tc: &TunnelConfig,
        candidates: &[GatewayDialCandidate],
        managed_exact_leaf: bool,
    ) -> anyhow::Result<ReplicaTransport> {
        let kind = TransportKind::parse(&tc.transport_type)?;
        match kind {
            TransportKind::Quic => {
                let tls_cfg = if managed_exact_leaf {
                    tls::client_config_with_exact_leaf(self.platform_tls_pem(tc).ok_or_else(
                        || anyhow::anyhow!("Managed Gateway facts are missing the exact leaf PEM"),
                    )?)?
                } else {
                    tls::client_config_with_pem(
                        self.platform_tls_pem(tc),
                        self.cfg.gateway_ca_path.as_deref(),
                        self.cfg.insecure_tls,
                    )?
                };
                let candidates = candidates
                    .iter()
                    .map(|candidate| {
                        let endpoint =
                            gateway_endpoint(&candidate.gateway_addr, candidate.gateway_port);
                        QuicTransportCandidate {
                            gateway_addr: candidate.gateway_addr.clone(),
                            gateway_port: candidate.gateway_port,
                            server_name: candidate
                                .tls_server_name
                                .clone()
                                .unwrap_or_else(|| tls_domain(&endpoint)),
                        }
                    })
                    .collect();
                Ok(ReplicaTransport::Quic {
                    client: Arc::new(QuicClient::new(tls_cfg, QuicTuning::game_streaming())?),
                    candidates: Arc::new(candidates),
                })
            }
            TransportKind::WebSocket => {
                let transport_candidates = candidates
                    .iter()
                    .map(|candidate| {
                        let dial_endpoint =
                            gateway_endpoint(&candidate.gateway_addr, candidate.gateway_port);
                        let request_endpoint = candidate
                            .tls_server_name
                            .as_deref()
                            .map(|server_name| {
                                gateway_endpoint(server_name, candidate.gateway_port)
                            })
                            .unwrap_or_else(|| dial_endpoint.clone());
                        Ok(WebSocketTransportCandidate {
                            gateway_addr: candidate.gateway_addr.clone(),
                            gateway_port: candidate.gateway_port,
                            url: websocket_url(
                                &request_endpoint,
                                candidate.force_tls || self.tunnel_uses_tls(tc, &dial_endpoint),
                            )?,
                            tls_server_name: candidate.tls_server_name.clone(),
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok(ReplicaTransport::WebSocket {
                    candidates: Arc::new(transport_candidates),
                    tls_config: if managed_exact_leaf {
                        Some(tls::client_config_for_https_with_exact_leaf(
                            self.platform_tls_pem(tc).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Managed Gateway facts are missing the exact leaf PEM"
                                )
                            })?,
                        )?)
                    } else {
                        self.https_tls_config(tc)?
                    },
                })
            }
            TransportKind::Grpc => {
                let ca_pem = if managed_exact_leaf {
                    None
                } else {
                    self.tls_ca_pem_bytes(tc)?
                };
                let exact_leaf_pem = if managed_exact_leaf {
                    Some(
                        self.platform_tls_pem(tc)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Managed Gateway facts are missing the exact leaf PEM"
                                )
                            })?
                            .as_bytes()
                            .to_vec(),
                    )
                } else {
                    None
                };
                let transport_candidates = candidates
                    .iter()
                    .map(|candidate| {
                        let endpoint =
                            gateway_endpoint(&candidate.gateway_addr, candidate.gateway_port);
                        let url = grpc_url(
                            &endpoint,
                            candidate.force_tls || self.tunnel_uses_tls(tc, &endpoint),
                        )?;
                        let tls_domain = if has_tls_scheme(&url) {
                            Some(
                                candidate
                                    .tls_server_name
                                    .clone()
                                    .unwrap_or_else(|| tls_domain(&url)),
                            )
                        } else {
                            None
                        };
                        let candidate_ca_pem = tls_domain.as_ref().and_then(|_| ca_pem.clone());
                        Ok(GrpcTransportCandidate {
                            gateway_addr: candidate.gateway_addr.clone(),
                            gateway_port: candidate.gateway_port,
                            url,
                            tls_domain,
                            ca_pem: candidate_ca_pem,
                            exact_leaf_pem: exact_leaf_pem.clone(),
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok(ReplicaTransport::Grpc {
                    candidates: Arc::new(transport_candidates),
                    insecure_tls: !managed_exact_leaf && self.cfg.insecure_tls,
                })
            }
        }
    }

    fn v2_gateway_dial_candidates(
        &self,
        gateway: &GatewayBootstrapV2,
    ) -> Vec<GatewayDialCandidate> {
        vec![GatewayDialCandidate {
            gateway_addr: gateway.dial_address.clone(),
            gateway_port: gateway.port,
            tls_server_name: gateway.tls_server_name.clone(),
            force_tls: true,
        }]
    }

    fn tunnel_uses_tls(&self, tc: &TunnelConfig, endpoint: &str) -> bool {
        self.cfg.insecure_tls
            || self.cfg.gateway_ca_path.is_some()
            || !tc.tls_cert.trim().is_empty()
            || has_tls_scheme(endpoint)
    }

    fn platform_tls_pem<'a>(&self, tc: &'a TunnelConfig) -> Option<&'a str> {
        let cert = tc.tls_cert.trim();
        cert.contains("-----BEGIN CERTIFICATE-----").then_some(cert)
    }

    fn https_tls_config(
        &self,
        tc: &TunnelConfig,
    ) -> anyhow::Result<Option<Arc<rustls::ClientConfig>>> {
        let pem = self.platform_tls_pem(tc);
        if pem.is_none() && self.cfg.gateway_ca_path.is_none() && !self.cfg.insecure_tls {
            return Ok(None);
        }
        Ok(Some(tls::client_config_for_https(
            pem,
            self.cfg.gateway_ca_path.as_deref(),
            self.cfg.insecure_tls,
        )?))
    }

    fn tls_ca_pem_bytes(&self, tc: &TunnelConfig) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(pem) = self.platform_tls_pem(tc) {
            return Ok(Some(pem.as_bytes().to_vec()));
        }
        if let Some(path) = &self.cfg.gateway_ca_path {
            return Ok(Some(std::fs::read(path)?));
        }
        Ok(None)
    }

    fn start_managed_peer_heartbeat(
        self: &Arc<Self>,
        profile: Arc<PeerProfileV2>,
        platform_url: String,
        generation_cancel: CancellationToken,
    ) {
        let engine = self.clone();
        let sender = self.managed_peer_heartbeat_sender.clone();
        let client_version = if self.cfg.client_version.trim().is_empty() {
            env!("CARGO_PKG_VERSION").to_string()
        } else {
            self.cfg.client_version.clone()
        };
        self.spawn_engine_task(async move {
            let mut last_timestamp_ms = 0_u64;
            let mut tick = interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = generation_cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }

                let status = engine.status();
                let request = match crate::peer_heartbeat::build_peer_heartbeat_request(
                    &profile,
                    &uuid::Uuid::new_v4().to_string(),
                    next_peer_heartbeat_timestamp_ms(&mut last_timestamp_ms),
                    &client_version,
                    false,
                    status.transport_heartbeat.active,
                    peer_heartbeat_path_mode(status.path_mode),
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        tracing::warn!(%error, "could not sign Platform Peer heartbeat");
                        let mut current = engine.state.read().clone();
                        current.platform_heartbeat.active = false;
                        current.platform_heartbeat.last_error = Some(error.to_string());
                        engine.set_status(current);
                        continue;
                    }
                };
                let result = tokio::select! {
                    _ = generation_cancel.cancelled() => break,
                    result = sender.send(&platform_url, &request) => result,
                };
                match result {
                    Ok(relay_usage) => {
                        if let Some(usage) = relay_usage {
                            engine.set_v2_relay_usage(usage);
                        }
                        let mut current = engine.state.read().clone();
                        current.platform_heartbeat = HeartbeatStatus {
                            active: true,
                            last_time: Some(unix_now()),
                            last_error: None,
                        };
                        engine.set_status(current);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "Platform Peer heartbeat failed; retrying");
                        let mut current = engine.state.read().clone();
                        current.platform_heartbeat.active = false;
                        current.platform_heartbeat.last_error = Some(error.to_string());
                        engine.set_status(current);
                    }
                }
            }

            let request = crate::peer_heartbeat::build_peer_heartbeat_request(
                &profile,
                &uuid::Uuid::new_v4().to_string(),
                next_peer_heartbeat_timestamp_ms(&mut last_timestamp_ms),
                &client_version,
                true,
                false,
                PlatformHeartbeatPathModeV2::Disconnected,
            );
            if let Ok(request) = request {
                let _ = timeout(Duration::from_secs(2), sender.send(&platform_url, &request)).await;
            }
        });
    }

    /// Connect a verified Lantunnel 2.0 Peer to one effective Gateway.
    ///
    /// Static profiles may omit `static_gateway_override` and use the Gateway
    /// facts embedded in the profile. Managed profiles are resolved through
    /// the Platform for every full Gateway Attachment generation.
    pub async fn connect_with_peer_profile(
        self: &Arc<Self>,
        profile: PeerProfileV2,
        static_gateway_override: Option<GatewayBootstrapV2>,
    ) -> crate::Result<()> {
        profile
            .verify()
            .map_err(|error| crate::EngineError::Other(error.to_string()))?;
        match &profile.bootstrap {
            PeerBootstrapV2::StaticGateway(_) => {
                if let Some(gateway) = &static_gateway_override {
                    gateway
                        .validate()
                        .map_err(|error| crate::EngineError::Other(error.to_string()))?;
                }
            }
            PeerBootstrapV2::ManagedPlatform { .. } if static_gateway_override.is_some() => {
                return Err(crate::EngineError::Platform(
                    "Managed Peer profiles do not allow static Gateway overrides".into(),
                ));
            }
            PeerBootstrapV2::ManagedPlatform { .. } => {}
        }
        let profile = Arc::new(profile);
        let source = GatewayAttachmentSource::new(profile.clone(), static_gateway_override);

        self.disconnect().await;
        *self.active_v2_profile.write() = Some(profile.clone());
        self.begin_v2_runtime(
            self.active_v2_profile
                .read()
                .as_deref()
                .expect("V2 profile was just installed"),
        );
        self.initialize_v2_peer_gossip();
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        *self.stop_tx.write() = Some(stop_tx);
        let me = self.clone();
        let cancel = self.task_cancel_token();
        if let PeerBootstrapV2::ManagedPlatform { platform_url } = &profile.bootstrap {
            self.start_managed_peer_heartbeat(
                profile.clone(),
                platform_url.clone(),
                cancel.clone(),
            );
        }
        self.start_v2_local_lan_export_watchdog(cancel.clone());
        let digest_engine = self.clone();
        let digest_cancel = cancel.clone();
        self.spawn_engine_task(async move {
            let mut tick = interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = digest_cancel.cancelled() => break,
                    _ = tick.tick() => digest_engine.poll_v2_gossip_digests(),
                }
            }
        });
        self.spawn_engine_task(async move {
            if let Err(error) = me
                .clone()
                .run_gateway_attachments(source, stop_rx, cancel)
                .await
            {
                tracing::warn!(error = %error, "V2 Client engine exited");
                let mut status = me.state.read().clone();
                status.connected = false;
                status.connecting = false;
                status.error = Some(error.to_string());
                me.set_status(status);
            }
        });
        Ok(())
    }

    async fn run_gateway_attachments(
        self: Arc<Self>,
        source: GatewayAttachmentSource,
        mut stop_rx: mpsc::Receiver<()>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let outcome = match self.prepare_gateway_attachment(&source).await {
                Ok(generation) => match self
                    .clone()
                    .run_gateway_attachment_once(
                        generation.tunnel_config,
                        generation.attachment,
                        &mut stop_rx,
                        &cancel,
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => SessionOutcome::Failed(error),
                },
                Err(error) => SessionOutcome::Failed(error),
            };
            match outcome {
                SessionOutcome::UserCancel => return Ok(()),
                SessionOutcome::Failed(e) => {
                    self.mark_v2_runtime_failure(&e);
                    tracing::warn!(
                        error = %e,
                        retry_in_ms = backoff.as_millis() as u64,
                        "Gateway Attachment session failed, reconnecting"
                    );
                    self.listener.on_log(&format!(
                        "[{}] session failed: {}; reconnecting in {}s",
                        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S"),
                        e,
                        backoff.as_secs()
                    ));
                    let mut s = self.state.read().clone();
                    s.connected = false;
                    s.connecting = true;
                    s.message = format!("Reconnecting in {}s…", backoff.as_secs());
                    s.transport_heartbeat.active = false;
                    s.error = Some(e.to_string());
                    self.set_status(s);
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(()),
                        _ = stop_rx.recv() => return Ok(()),
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn prepare_gateway_attachment(
        &self,
        source: &GatewayAttachmentSource,
    ) -> anyhow::Result<GatewayAttachmentGeneration> {
        let gateway = match &source.profile.bootstrap {
            PeerBootstrapV2::StaticGateway(embedded) => source
                .static_gateway_override
                .clone()
                .unwrap_or_else(|| embedded.clone()),
            PeerBootstrapV2::ManagedPlatform { .. } => {
                self.mark_v2_gateway_resolving();
                self.managed_gateway_resolver
                    .resolve(&source.profile)
                    .await?
            }
        };
        gateway
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid Gateway facts for V2 attachment: {error}"))?;
        self.mark_v2_gateway_connecting(&gateway);
        let tunnel_config = v2_tunnel_config(
            &source.profile,
            &gateway,
            source.runtime_replica_ids.clone(),
        );
        Ok(GatewayAttachmentGeneration {
            tunnel_config,
            attachment: V2GatewayAttachment {
                profile: source.profile.clone(),
                gateway,
            },
        })
    }

    async fn run_gateway_attachment_once(
        self: Arc<Self>,
        tc: TunnelConfig,
        attachment: V2GatewayAttachment,
        stop_rx: &mut mpsc::Receiver<()>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<SessionOutcome> {
        if cancel.is_cancelled() {
            return Ok(SessionOutcome::UserCancel);
        }
        let client_ids: Vec<String> = if !tc.client_ids.is_empty() {
            tc.client_ids.clone()
        } else {
            vec![tc.client_id.clone()]
        };
        self.reset_replica_sessions_for_connect(client_ids.first().cloned());
        let replicas: usize = client_ids.len().max(1);
        *self.replicas.lock() = Some(replicas);
        *self.latest_tunnel_config.write() = Some(tc.clone());

        if cancel.is_cancelled() {
            return Ok(SessionOutcome::UserCancel);
        }
        let gateway_candidates = self.v2_gateway_dial_candidates(&attachment.gateway);
        let gateway_candidate_endpoints = gateway_candidate_endpoints(&gateway_candidates);
        let gateway_display_addr = gateway_candidate_endpoints
            .first()
            .cloned()
            .unwrap_or_else(|| gateway_endpoint(&tc.gateway_addr, tc.gateway_port));
        let managed_exact_leaf = matches!(
            &attachment.profile.bootstrap,
            PeerBootstrapV2::ManagedPlatform { .. }
        );
        let transport =
            self.build_replica_transport(&tc, &gateway_candidates, managed_exact_leaf)?;
        tracing::info!(
            platform_base = %tc.platform_base_url.as_deref().unwrap_or("<unset>"),
            canonical_gateway = %gateway_endpoint(&tc.gateway_addr, tc.gateway_port),
            preferred_gateway = %gateway_display_addr,
            candidate_gateways = ?gateway_candidate_endpoints,
            candidates = gateway_candidate_endpoints.len(),
            "gateway address candidates prepared"
        );

        // HostFilter::new returns `EngineError` at the library boundary;
        // `?` here lets anyhow wrap it while preserving the display prefix.
        let host_filter = Arc::new(HostFilter::new(&tc.forbidden_hosts, &tc.allowed_hosts)?);
        self.set_group_context(Arc::new(TunnelGroupContext::from_config(
            &tc,
            client_ids.first().cloned(),
            host_filter.clone(),
        )));

        let gateway_name = effective_gateway_name(&tc);
        let s1 = ConnectionStatus {
            connected: false,
            connecting: true,
            message: if replicas > 1 {
                format!("Connecting {replicas} replicas…")
            } else {
                "Connecting…".into()
            },
            gateway_name: gateway_name.clone(),
            gateway_addr: Some(gateway_display_addr.clone()),
            ..Default::default()
        };
        self.set_status(s1);
        let status_refresh_token = CancellationToken::new();
        let status_refresh_handle = self.start_status_refresh_loop(status_refresh_token.clone());

        let (err_tx, mut err_rx) = mpsc::channel::<anyhow::Error>(replicas);
        let mut handles = Vec::with_capacity(replicas);
        let replica_activity = ReplicaActivity::new(replicas, gateway_name);
        let dial_stagger = replica_dial_stagger();
        let reconnect_policy = ReplicaReconnectPolicy::production();
        let reconnect_group = ReplicaReconnectGroup::new(replicas);
        let v2_profile = attachment.profile;
        for (idx, cid) in client_ids.iter().enumerate() {
            let me = self.clone();
            let transport = transport.clone();
            let tc2 = tc.clone();
            let cid = cid.clone();
            let profile = v2_profile.clone();
            let hf = host_filter.clone();
            let etx = err_tx.clone();
            let activity = replica_activity.clone();
            let reconnect_group = reconnect_group.clone();
            let cancel = cancel.clone();
            let gateway_display_addr = gateway_display_addr.clone();
            self.relay_transport_generations
                .entry(cid.clone())
                .or_insert_with(|| AtomicU64::new(1));
            let h = AbortOnDropHandle::new(tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = sleep_replica_dial_stagger(idx, dial_stagger) => {}
                }
                let replica_client_id = cid.clone();
                let retry_activity = activity.clone();
                let result = run_reconnecting_replica(
                    replica_client_id,
                    reconnect_policy,
                    reconnect_group,
                    retry_activity,
                    cancel.clone(),
                    || {
                        let me = me.clone();
                        let transport = transport.clone();
                        let tc2 = tc2.clone();
                        let cid = cid.clone();
                        let profile = profile.clone();
                        let hf = hf.clone();
                        let activity = activity.clone();
                        let cancel = cancel.clone();
                        let gateway_display_addr = gateway_display_addr.clone();
                        async move {
                            let transport_generation = me.next_relay_transport_generation(&cid);
                            me.run_replica(
                                transport,
                                tc2,
                                cid,
                                profile,
                                gateway_display_addr,
                                hf,
                                activity,
                                transport_generation,
                                cancel.clone(),
                            )
                            .await
                        }
                    },
                )
                .await;
                if let Err(e) = result {
                    let _ = etx.send(e).await;
                }
            }));
            handles.push(h);
        }
        drop(err_tx);

        let outcome = wait_for_replica_group_outcome(replicas, stop_rx, &mut err_rx, cancel).await;
        if matches!(outcome, SessionOutcome::UserCancel) {
            cancel.cancel();
        }
        status_refresh_token.cancel();
        drop(status_refresh_handle);
        wait_for_abort_on_drop_handles("direct replica", handles, Duration::from_secs(5)).await;
        Ok(outcome)
    }

    pub async fn disconnect(&self) {
        let disconnect_started = Instant::now();
        // Bind to a local so the RwLockWriteGuard drops at the `;` — otherwise
        // the guard's lifetime spans the `.await` below and the future becomes
        // `!Send` (parking_lot guards are not Send by default).
        let task_cancel = self.task_cancel.read().clone();
        let tx = self.stop_tx.write().take();
        {
            let _registry_guard = self.p2p_session_registry_lock.lock();
            let _reconcile_guard = self.v2_runtime_reconcile_lock.lock();
            task_cancel.cancel();
            self.traffic.reset();
            self.set_status(ConnectionStatus {
                message: "Disconnected".into(),
                ..Default::default()
            });
            self.close_live_p2p_session_and_clear_group_context();
            self.invalidate_p2p_underlay_generation();
            self.invalidate_local_lan_publication_generation();
            self.p2p_pending_membership_batches.lock().clear();
            self.p2p_delivered_membership_authorities.lock().clear();
            self.relay_inbound_attestations.clear();
            self.v2_peer_links.clear();
            self.v2_relay_flows.clear();
            self.v2_current_membership.write().clear();
            self.v2_membership_cycle_complete
                .store(false, Ordering::Release);
            *self.v2_peer_gossip.lock() = None;
            *self.active_v2_profile.write() = None;
            *self.v2_runtime.write() = crate::runtime_snapshot::V2RuntimeSnapshot::default();
            *self.latest_tunnel_config.write() = None;
            *self.overlay_routes.write() = crate::route_matcher::OverlayRouteMatcher::default();
        }
        if let Some(tx) = tx {
            let _ = tx.send(()).await;
        }

        // Drain the engine-lifetime task cohort. Take + replace so a
        // subsequent `connect()` registers on a fresh tracker — `close()` is
        // sticky on `TaskTracker` (it permanently signals "no more tasks
        // expected") so we cannot reuse the same instance across reconnects.
        // Same shape as the per-`run_replica` tracker.
        //
        // Drop the parked signaling-broker ingress before the
        // drain. Cancellation wakes a broker blocked on the bounded manager
        // channel; dropping its final `in_tx` then lets `P2pManager::run` exit.
        // The listener accept loop and other detached futures (where we don't
        // have a handle to signal) remain guarded by the 5 s wait deadline.
        *self.p2p_signaling_ingress_tx.lock() = None;
        let replica_multis: Vec<_> = self
            .replica_sessions
            .lock()
            .iter()
            .map(|entry| entry.multi.clone())
            .collect();
        for multi in replica_multis {
            multi.relay().close();
            multi.close_all_p2p();
        }
        self.relay_transport_generations.clear();
        *self.p2p_refill_handle.lock() = None;
        let (tracker, abort_handles) = self.replace_task_tracker_for_disconnect();
        tracker.close();
        match tokio::time::timeout(Duration::from_secs(5), tracker.wait()).await {
            Ok(()) => {
                tracing::info!("engine task drain complete; shutdown")
            }
            Err(_) => {
                if abort_handles.is_empty() {
                    tracing::warn!(
                        "engine task drain timeout (5s); external unabortable tasks may still be alive"
                    );
                } else {
                    for handle in abort_handles {
                        handle.abort();
                    }
                    tracing::warn!(
                        "engine task drain timeout (5s); aborted owned engine-lifetime tasks"
                    );
                    if tokio::time::timeout(Duration::from_secs(1), tracker.wait())
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "engine task abort drain timeout (1s); external unabortable tasks may still be alive"
                        );
                    }
                }
            }
        }
        *self.task_cancel.write() = CancellationToken::new();
        // The broker is now drained, so no canceled delivery can append a
        // stale authority after the initial disconnect clear above.
        self.p2p_delivered_membership_authorities.lock().clear();
        self.replica_sessions.lock().clear();
        self.proxy_replica_rr.store(0, Ordering::Relaxed);
        self.p2p_install_rr.store(0, Ordering::Relaxed);
        self.p2p_pending_installs.lock().clear();
        self.relay_transport_generations.clear();
        *self.p2p_refill_handle.lock() = None;
        *self.p2p_anchor_client_id.lock() = None;
        *self.multi.lock() = None;
        *self.tunnel_identity.lock() = None;
        tracing::info!(
            elapsed_ms = disconnect_started.elapsed().as_millis() as u64,
            "engine disconnect complete"
        );
    }

    /// One replica's session: dial transport, run heartbeat, handle msgs.
    #[tracing::instrument(
        level = "debug",
        skip(self, transport, tc, v2_profile, host_filter, replica_activity),
        fields(client_id = %client_id, group_id = %tc.group_id, gateway = %gateway_display_addr, transport = %transport.kind())
    )]
    // Replica startup intentionally receives the complete immutable connection context.
    #[allow(clippy::too_many_arguments)]
    async fn run_replica(
        self: Arc<Self>,
        transport: ReplicaTransport,
        tc: TunnelConfig,
        client_id: String,
        v2_profile: Arc<PeerProfileV2>,
        gateway_display_addr: String,
        host_filter: Arc<HostFilter>,
        replica_activity: ReplicaActivity,
        transport_generation: u64,
        parent_cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let replica_cancel = parent_cancel.child_token();
        let auth =
            crate::v2_attachment::v2_auth_params(&v2_profile, client_id.clone(), tc.gateway_port);
        let session_started = std::time::Instant::now();
        // Cap the transport dial + Auth round-trip. QUIC's own
        // initial RTT + retries can otherwise consume tens of seconds when
        // the path is congested, and WS/gRPC handshakes can stall behind
        // captive DNS/proxies. Field evidence from Mac/mobile -> AL showed
        // valid QUIC packets arriving just after the old 15 s cap, so keep a
        // bounded timeout while allowing slow WAN handshakes to complete.
        let dial_timeout = transport_dial_timeout();
        let connected = tokio::select! {
            _ = replica_cancel.cancelled() => anyhow::bail!("replica cancelled"),
            result = connect_transport(&transport, auth, dial_timeout) => result?,
        };
        let mut session = connected.session;
        let gw_addr = connected.gateway_addr;
        if !session.capabilities().peer_mesh_v2 {
            session.close();
            anyhow::bail!("Gateway does not support Lantunnel 2.0 Peer authentication");
        }
        crate::v2_attachment::complete_v2_gateway_attachment(&mut session, &v2_profile, &client_id)
            .await?;
        if replica_cancel.is_cancelled() {
            session.close();
            anyhow::bail!("replica cancelled");
        }
        tracing::debug!(
            %client_id,
            group_id = %tc.group_id,
            gateway = %gw_addr,
            transport = %transport.kind(),
            elapsed_ms = session_started.elapsed().as_millis() as u64,
            "tunnel replica connected"
        );
        // Split session eagerly so producers (heartbeat, pipe_udp, pipe_tcp,
        // handle_msg) can hold cheap SessionSender clones instead of funneling
        // through a per-engine mpsc. The split now yields THREE halves — the
        // third is an optional datagram receiver driven on its own task so
        // UDP game-stream frames don't queue behind a slow TCP consumer.
        let (sender, mut receiver, datagram_receiver) = session.split();
        let _shutdown_guard = ReplicaShutdownGuard::new(replica_cancel.clone(), sender.clone());
        let control_receiver = receiver.take_control_receiver();
        let tcp_flow_receiver = receiver.take_tcp_flow_receiver();
        let established_ms = monotonic_millis();
        let last_transport_ack = Arc::new(AtomicU64::new(established_ms));
        let relay_last_link_progress_ms = Arc::new(AtomicU64::new(established_ms));
        let relay_active_flows = Arc::new(LinkActiveFlows::default());

        let inbound = Arc::new(DashMap::<String, mpsc::Sender<Bytes>>::new());
        let udp_inbound = Arc::new(DashMap::<String, DropOldestSender<Bytes>>::new());

        // Build the `MultiSession` bridge: shares the conn-id maps with the
        // local `inbound` / `udp_inbound` `Arc`s above so external callers
        // (Task 4.11 in `apps/lantunnel-client/src-tauri/src/main.rs`) can route P2P data through
        // `engine.multi_session()` without dropping per-conn state on a
        // path-flip. The relay slot holds an `Arc<Session>` "send-only
        // shell" built from a clone of `sender`; `Session::send`/`closed`
        // on it share the underlying outbound channels so its lifecycle
        // tracks the real session.
        //
        // Stored under `self.multi` so `Engine::multi_session()` /
        // `Engine::relay_session()` accessors (consumed by Task 4.11) can
        // observe it. Per-replica scope: replaced on each replica
        // start; on teardown we clear it just before logging the
        // disconnect line.
        let relay_arc = Arc::new(tp_transport::session::Session::send_only_from_sender(
            sender.clone(),
        ));
        // Build the path scheduler from the configured P2P knobs so
        // YAML overrides for `min_advantage` / `stable_cycles` reach
        // runtime. `p2p_config()` returns defaults when nothing is
        // installed (preserves the original hardcoded behavior for callers that
        // never call `set_p2p_config`).
        let scheduler = Arc::new(crate::p2p::scheduler::PathScheduler::from_config(
            &self.p2p_config(),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps_and_scheduler(
            relay_arc,
            inbound.clone(),
            udp_inbound.clone(),
            scheduler,
        );
        self.bind_v2_lane_change_observer(&multi);
        if !self.publish_connected_replica_if_active(
            &parent_cancel,
            &client_id,
            &tc.group_id,
            multi.clone(),
            transport_generation,
            &replica_activity,
            gw_addr,
        ) {
            sender.close();
            anyhow::bail!("replica cancelled");
        }
        // Structured-cancellation plumbing for every task spawned below.
        // Replaces the previous mix of `tokio::spawn + .abort()` / detached
        // `tokio::spawn` with a single TaskTracker that cancellation wakes and
        // that we await before returning from `run_replica`. Two deliberate
        // exceptions stay on bare `tokio::spawn` + `JoinHandle`:
        //   * the stream reader below, whose natural "EOF" exit is the signal
        //     we use to drive the teardown; waiting on it separately from the
        //     tracker is how `run_replica` distinguishes "session closed" from
        //     "user cancelled";
        //   * the datagram reader below, for the same reason.
        // Everything else — the periodic summary, transport heartbeat, and
        // every per-`Connect` pipe_tcp/pipe_udp spawn inside `handle_msg` —
        // now registers with `tasks` so that on teardown we cancel first,
        // close the sender (so `out.closed()` fires in pipe_*), then await
        // every tracked task instead of leaking detached futures.
        let tasks = TaskTracker::new();

        // Periodic session summary — one `info!` line every 10 s while the
        // replica is live. 10s (was 60s) because the UDP-route telemetry
        // below is the single most important signal for diagnosing the
        // "moonlight FPS drops after a few seconds" class of bugs; we want
        // enough samples during a short reproduction to see the transition
        // from early PMTUD probing to steady state.
        //
        // Critical fields:
        //   - `max_datagram_size`: quinn's current PMTUD result. Tunnel
        //     traffic uses the tp-transport game-streaming profile: an
        //     IPv6-safe Ethernet initial/upper MTU with Quinn's 1200-byte
        //     black-hole recovery floor. If this stays small, oversized UDP
        //     fragments and can saturate the datagram buffer.
        //   - `udp_scheduler_accepted`, `udp_handed_to_quinn`, and
        //     `udp_stream_fallback`: separate producer enqueue, local
        //     datagram-buffer eviction, and missing QUIC datagram support.
        //   - `udp_dropped_full`: per-session count of `try_send` drops
        //     (client pipe_udp drops UDP packets when stream_tx fills).
        //   - `last_fallback_packed_len` / `last_fallback_max_dg`: sampled
        //     from the most recent MTU overshoot, tells us by how much the
        //     packed UdpData exceeded the datagram window.
        {
            let cid = client_id.clone();
            let gid = tc.group_id.clone();
            let inbound = inbound.clone();
            let udp_inbound = udp_inbound.clone();
            let multi_for_stats = multi.clone();
            let started = session_started;
            let sender_for_stats = sender.clone();
            let stats_handle = sender.udp_route_stats();
            let cancel = replica_cancel.clone();
            tasks.spawn(async move {
                use std::sync::atomic::Ordering;
                let mut t = interval(Duration::from_secs(10));
                t.set_missed_tick_behavior(MissedTickBehavior::Delay);
                // Skip the instant first tick so we don't log zeros on startup.
                t.tick().await;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = t.tick() => {}
                    }
                    let max_dg = sender_for_stats.current_max_datagram_size();
                    let buf_space = sender_for_stats.current_datagram_send_buffer_space();
                    let tunnel_health = sender_for_stats.stats();
                    let buf_space_min = stats_handle.take_datagram_send_buffer_space_min();
                    let buf_space_zero_count =
                        stats_handle.take_datagram_send_buffer_space_zero_count();
                    let p2p_session = multi_for_stats.p2p();
                    let p2p_peer = p2p_session
                        .as_ref()
                        .map(|session| session.peer_addr().to_string());
                    let p2p_health = p2p_session.as_ref().map(|session| session.stats());
                    let p2p_max_dg = p2p_session
                        .as_ref()
                        .and_then(|session| session.current_max_datagram_size());
                    let p2p_buf_space = p2p_session
                        .as_ref()
                        .and_then(|session| session.current_datagram_send_buffer_space());
                    let p2p_stats_handle =
                        p2p_session.as_ref().map(|session| session.stats_handle());
                    let p2p_buf_space_min = p2p_stats_handle
                        .as_ref()
                        .and_then(|stats| stats.take_datagram_send_buffer_space_min());
                    let p2p_buf_space_zero_count = p2p_stats_handle
                        .as_ref()
                        .map(|stats| stats.take_datagram_send_buffer_space_zero_count())
                        .unwrap_or(0);
                    tracing::info!(
                        client_id = %cid,
                        group_id = %gid,
                        uptime_secs = started.elapsed().as_secs(),
                        active_tcp = inbound.len(),
                        active_udp = udp_inbound.len(),
                        p2p_installed = p2p_session.is_some(),
                        p2p_state = ?multi_for_stats.p2p_state(),
                        p2p_peer = ?p2p_peer,
                        p2p_rtt_ms = ?p2p_health.map(|stats| stats.rtt.as_millis()),
                        p2p_loss_rate = ?p2p_health.map(|stats| stats.loss_rate),
                        p2p_pto_count = ?p2p_health.map(|stats| stats.pto_count),
                        // Tunnel-QUIC path MTU (datagram cap).
                        max_datagram_size = ?max_dg,
                        tunnel_rtt_ms = tunnel_health.rtt.as_millis(),
                        tunnel_loss_rate = tunnel_health.loss_rate,
                        tunnel_pto_count = tunnel_health.pto_count,
                        // Outgoing datagram buffer remaining. Close to 0 =
                        // quinn is silently evicting older datagrams on each
                        // new send_datagram to free room. This is the ground
                        // truth for "is the tunnel the real bottleneck?"
                        tunnel_send_buf_space = ?buf_space,
                        tunnel_send_buf_space_min = ?buf_space_min,
                        tunnel_send_buf_space_zero_count = buf_space_zero_count,
                        // Producer-side counters (pipe_udp/pipe_tcp).
                        udp_scheduler_accepted = stats_handle
                            .datagram_accepted_to_scheduler
                            .load(Ordering::Relaxed),
                        udp_stream_fallback =
                            stats_handle.stream_fallback.load(Ordering::Relaxed),
                        udp_dropped_full =
                            stats_handle.dropped_full.load(Ordering::Relaxed),
                        udp_assoc_evicted = stats_handle
                            .datagram_per_association_evicted
                            .load(Ordering::Relaxed),
                        udp_global_evicted = stats_handle
                            .datagram_global_budget_evicted
                            .load(Ordering::Relaxed),
                        last_fallback_packed_len =
                            stats_handle.last_fallback_packed_len.load(Ordering::Relaxed),
                        last_fallback_max_dg =
                            stats_handle.last_fallback_max_dg.load(Ordering::Relaxed),
                        udp_handed_to_quinn =
                            stats_handle.datagram_write_ok.load(Ordering::Relaxed),
                        udp_quinn_error =
                            stats_handle.datagram_write_err.load(Ordering::Relaxed),
                        // Datagram reader task counters (quinn → mpsc).
                        dg_recv_ok =
                            stats_handle.datagram_recv_ok.load(Ordering::Relaxed),
                        udp_inbound_dropped =
                            stats_handle.datagram_recv_dropped.load(Ordering::Relaxed),
                        dg_recv_decode_err =
                            stats_handle.datagram_recv_decode_err.load(Ordering::Relaxed),
                        // Direct P2P QUIC counters for the same replica slot.
                        p2p_max_datagram_size = ?p2p_max_dg,
                        p2p_send_buf_space = ?p2p_buf_space,
                        p2p_send_buf_space_min = ?p2p_buf_space_min,
                        p2p_send_buf_space_zero_count = p2p_buf_space_zero_count,
                        p2p_udp_scheduler_accepted = p2p_stats_handle
                            .as_ref()
                            .map(|stats| {
                                stats
                                    .datagram_accepted_to_scheduler
                                    .load(Ordering::Relaxed)
                            })
                            .unwrap_or(0),
                        p2p_udp_stream_fallback = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.stream_fallback.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        p2p_udp_dropped_full = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.dropped_full.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        p2p_udp_assoc_evicted = p2p_stats_handle
                            .as_ref()
                            .map(|stats| {
                                stats
                                    .datagram_per_association_evicted
                                    .load(Ordering::Relaxed)
                            })
                            .unwrap_or(0),
                        p2p_udp_global_evicted = p2p_stats_handle
                            .as_ref()
                            .map(|stats| {
                                stats.datagram_global_budget_evicted.load(Ordering::Relaxed)
                            })
                            .unwrap_or(0),
                        p2p_udp_handed_to_quinn = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.datagram_write_ok.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        p2p_udp_quinn_error = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.datagram_write_err.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        p2p_dg_recv_ok = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.datagram_recv_ok.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        p2p_udp_inbound_dropped = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.datagram_recv_dropped.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        p2p_dg_recv_decode_err = p2p_stats_handle
                            .as_ref()
                            .map(|stats| stats.datagram_recv_decode_err.load(Ordering::Relaxed))
                            .unwrap_or(0),
                        "tunnel replica summary"
                    );
                }
            });
        }

        // Transport heartbeat — 1 s. This is the fine-grained liveness
        // signal: the gateway's MetricsManager flips `is_online` once
        // we've been silent for 120 s, and a 1 s cadence gives us a
        // sub-second failure-detection window with trivial overhead
        // (~30 B Heartbeat + ~12 B HeartbeatAck per second per replica;
        // the QUIC stream is already carrying orders of magnitude more
        // traffic). Unlike the Platform heartbeat, which is persisted,
        // this one only touches in-memory counters on the gateway —
        // there is no write-amplification cost. Fast detection matters
        // because `Engine::disconnect` / reconnect only triggers when
        // the transport path is declared dead, and until then proxy
        // requests routed to a dead replica time out at 15 s in
        // `ClientConn::open`.
        {
            let hb_sender = sender.clone();
            let cid = client_id.clone();
            let cancel = replica_cancel.clone();
            tasks.spawn(async move {
                let mut t = interval(Duration::from_secs(1));
                t.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = t.tick() => {}
                    }
                    let ts = unix_now();
                    tracing::debug!(client_id = %cid, ts, "transport heartbeat tick");
                    let send_started_ms = monotonic_millis();
                    let heartbeat = BinaryMessage::Heartbeat {
                            client_id: cid.clone(),
                            timestamp: ts,
                    };
                    let send_result = tokio::select! {
                        _ = cancel.cancelled() => return,
                        result = hb_sender.send(heartbeat) => result,
                    };
                    let heartbeat_send_elapsed_ms =
                        monotonic_millis().saturating_sub(send_started_ms);
                    if heartbeat_send_elapsed_ms >= 500 {
                        tracing::warn!(
                            client_id = %cid,
                            link_kind = "relay",
                            peer = "gateway",
                            ts,
                            heartbeat_send_elapsed_ms,
                            "relay link heartbeat send was delayed"
                        );
                    }
                    if send_result.is_err() {
                        tracing::debug!(client_id = %cid, "transport heartbeat loop exiting (session closed)");
                        return;
                    }
                }
            });
        }

        {
            let watchdog_sender = sender.clone();
            let last_ack = last_transport_ack.clone();
            let relay_last_link_progress_ms = relay_last_link_progress_ms.clone();
            let cid = client_id.clone();
            let active_flows = LinkActiveFlowCounters::with_source(
                relay_active_flows.clone(),
                self.relay_source_active_flow_counter(client_id.clone(), transport_generation),
            );
            let traffic = multi.local_traffic();
            let cancel = replica_cancel.clone();
            tasks.spawn(run_relay_link_watchdog_with_tcp_streams(
                self.clone(),
                multi.clone(),
                watchdog_sender,
                last_ack,
                relay_last_link_progress_ms,
                cid,
                transport_generation,
                active_flows,
                traffic,
                LinkWatchdogConfig::production(),
                cancel,
            ));
        }

        // Control reader task — drives heartbeat ACKs and P2P signaling from
        // the negotiated control stream. It is separate from the stream data
        // reader so bulk Data frames cannot sit ahead of liveness messages.
        if let Some(mut control_rx) = control_receiver {
            let me = self.clone();
            let multi = multi.clone();
            let inbound = inbound.clone();
            let udp_inbound = udp_inbound.clone();
            let host_filter = host_filter.clone();
            let tracker = tasks.clone();
            let ack = last_transport_ack.clone();
            let relay_progress = relay_last_link_progress_ms.clone();
            let relay_liveness =
                LinkLivenessState::relay(ack, relay_progress.clone(), relay_active_flows.clone());
            let control_cancel = replica_cancel.clone();
            tasks.spawn(async move {
                loop {
                    let m = tokio::select! {
                        _ = control_cancel.cancelled() => break,
                        maybe = control_rx.recv() => match maybe {
                            Some(m) => m,
                            None => break,
                        },
                    };
                    match m {
                        BinaryMessage::P2pAnnounceAck { .. }
                        | BinaryMessage::P2pOffer { .. }
                        | BinaryMessage::P2pAnswer { .. }
                        | BinaryMessage::P2pOfferV2 { .. }
                        | BinaryMessage::P2pAnswerV2 { .. }
                        | BinaryMessage::P2pPunchSync { .. }
                        | BinaryMessage::P2pTeardown { .. }
                        | BinaryMessage::P2pSessionReady { .. }
                        | BinaryMessage::P2pPeerHint { .. } => {
                            relay_progress.store(monotonic_millis(), Ordering::Relaxed);
                            me.forward_p2p_signaling_from_relay(m, &multi).await;
                        }
                        other => {
                            tokio::select! {
                                _ = control_cancel.cancelled() => break,
                                _ = me.handle_msg(
                                    other,
                                    &multi,
                                    &inbound,
                                    &udp_inbound,
                                    &host_filter,
                                    &tracker,
                                    None,
                                    Some(relay_liveness.clone()),
                                ) => {}
                            }
                        }
                    }
                }
            });
        }

        // Stream reader task — drives inbound reliable data messages. Outbound
        // goes directly from producers (pipe_tcp/pipe_udp/ConnectResponse
        // emitters) through `sender.send()`; no intermediate writer task.
        //
        // P2P signaling messages are split off to a dedicated `mpsc::Sender`
        // (set by `Engine::attach_p2p_signaling`) so `P2pManager` consumes
        // them on its own task. When no P2P signaling channel is attached
        // they're silently dropped — the existing `_ => {}` arm in
        // `handle_msg` would have ignored them anyway.
        let reader = {
            let me = self.clone();
            let multi = multi.clone();
            let inbound = inbound.clone();
            let udp_inbound = udp_inbound.clone();
            let host_filter = host_filter.clone();
            let tracker = tasks.clone();
            let ack = last_transport_ack.clone();
            let relay_progress = relay_last_link_progress_ms.clone();
            let relay_liveness =
                LinkLivenessState::relay(ack, relay_progress.clone(), relay_active_flows.clone());
            AbortOnDropHandle::new(tokio::spawn(async move {
                while let Some(m) = receiver.recv_data().await {
                    match m {
                        BinaryMessage::P2pAnnounceAck { .. }
                        | BinaryMessage::P2pOffer { .. }
                        | BinaryMessage::P2pAnswer { .. }
                        | BinaryMessage::P2pOfferV2 { .. }
                        | BinaryMessage::P2pAnswerV2 { .. }
                        | BinaryMessage::P2pPunchSync { .. }
                        | BinaryMessage::P2pTeardown { .. }
                        | BinaryMessage::P2pSessionReady { .. }
                        | BinaryMessage::P2pPeerHint { .. } => {
                            relay_progress.store(monotonic_millis(), Ordering::Relaxed);
                            me.forward_p2p_signaling_from_relay(m, &multi).await;
                        }
                        other => {
                            me.handle_msg(
                                other,
                                &multi,
                                &inbound,
                                &udp_inbound,
                                &host_filter,
                                &tracker,
                                None,
                                Some(relay_liveness.clone()),
                            )
                            .await;
                        }
                    }
                }
            }))
        };

        // Datagram reader task — drives inbound UDP (moonlight/sunshine game
        // stream on the return path, if it ever fires here). Runs entirely
        // independent of the stream reader so a slow TCP consumer cannot
        // backpressure UDP delivery.
        let datagram_reader = datagram_receiver.map(|mut dg_rx| {
            let me = self.clone();
            let multi = multi.clone();
            let inbound = inbound.clone();
            let udp_inbound = udp_inbound.clone();
            let host_filter = host_filter.clone();
            let tracker = tasks.clone();
            let ack = last_transport_ack.clone();
            let relay_progress = relay_last_link_progress_ms.clone();
            let relay_liveness =
                LinkLivenessState::relay(ack, relay_progress.clone(), relay_active_flows.clone());
            AbortOnDropHandle::new(tokio::spawn(async move {
                while let Some(m) = dg_rx.recv().await {
                    me.handle_msg(
                        m,
                        &multi,
                        &inbound,
                        &udp_inbound,
                        &host_filter,
                        &tracker,
                        None,
                        Some(relay_liveness.clone()),
                    )
                    .await;
                }
            }))
        });

        let tcp_flow_reader = tcp_flow_receiver.map(|mut flow_rx| {
            let me = self.clone();
            let multi = multi.clone();
            let host_filter = host_filter.clone();
            let tracker = tasks.clone();
            let relay_progress = relay_last_link_progress_ms.clone();
            let relay_active_flows = relay_active_flows.clone();
            AbortOnDropHandle::new(tokio::spawn(async move {
                while let Some(incoming) = flow_rx.recv().await {
                    relay_progress.store(monotonic_millis(), Ordering::Relaxed);
                    let active_flow = relay_active_flows.begin("tcp", &incoming.preface.conn_id);
                    let me = me.clone();
                    let multi = multi.clone();
                    let host_filter = host_filter.clone();
                    let relay_progress = relay_progress.clone();
                    tracker.spawn(async move {
                        me.handle_tcp_flow_stream(
                            incoming,
                            multi,
                            host_filter,
                            TrafficPath::Relay,
                            TcpFlowLinkContext {
                                p2p_source_session: None,
                                link_progress_ms: Some(relay_progress),
                                link_active_flow: active_flow,
                            },
                        )
                        .await;
                    });
                }
            }))
        });

        // Wait for the main reader to finish naturally, or cut the session
        // closed when the parent engine generation is cancelled. The replica
        // future itself still runs the cleanup below; only the low-level
        // receiver tasks are aborted if they do not wake after close.
        let mut reader = reader;
        let main_reader_finished = tokio::select! {
            result = &mut reader => {
                let _ = result;
                true
            }
            _ = replica_cancel.cancelled() => false,
        };

        // The main reader exiting ends this Relay generation. Cancel and close
        // before joining auxiliary intake readers: transport keepalive owners
        // may otherwise keep their channels open after the main reader EOF.
        let parent_cancelled = parent_cancel.is_cancelled();
        replica_cancel.cancel();
        sender.close();
        self.unregister_relay_closed_multi_session(&client_id, &multi);
        let pending_main_reader = if main_reader_finished {
            drop(reader);
            None
        } else {
            Some(reader)
        };
        drain_replica_intake_readers(
            pending_main_reader,
            tcp_flow_reader,
            datagram_reader,
            Duration::from_secs(1),
        )
        .await;

        // Intake has stopped spawning work. Existing business tasks observe
        // cancellation or the closed transport and drain before reconnect.
        tasks.close();
        tasks.wait().await;

        tracing::debug!(
            %client_id,
            group_id = %tc.group_id,
            gateway = %gw_addr,
            session_secs = session_started.elapsed().as_secs(),
            "tunnel replica disconnected"
        );

        replica_activity.mark_disconnected(&self, parent_cancelled);
        anyhow::bail!("gateway closed the connection");
    }

    // Per-conn pipe spawns now construct a `MultiSenderRouter` from
    // `self.multi` (the live MultiSession) and pass it into `pipe_tcp` /
    // `pipe_udp`. The Connect arm uses a P2P-preferred router so replies
    // to P2P-opened flows stay on the same replica's direct path when it is
    // healthy, falling back only to that replica's relay path if P2P fails.
    // The `out: &SessionSender` parameter remains for non-Connect arms that
    // don't need migration semantics; the Connect arm itself routes
    // ConnectResponse, Close, Data, and UdpData through the router.
    // Message handling needs the independently-owned flow maps and reply-path context.
    fn install_relay_inbound_attestation(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
        conn_id: &str,
        source_peer_id: &str,
    ) -> Result<(), String> {
        if !multi.relay().capabilities().relay_source_attestation_v1 {
            return Err("relay source attestation capability was not negotiated".into());
        }
        let source_peer_id = source_peer_id.trim();
        let config = self
            .latest_tunnel_config()
            .ok_or_else(|| "active Tunnel identity is unavailable".to_string())?;
        if source_peer_id.is_empty()
            || crate::p2p::replica::replica_index(source_peer_id) != Some(0)
            || crate::p2p::replica::replica_seed_for_tunnel(&config.tunnel_id, source_peer_id)
                .is_none()
            || crate::p2p::replica::same_replica_family(&config.peer_id, source_peer_id)
        {
            return Err("relay source Peer is not a canonical remote Tunnel Peer".into());
        }
        match self.relay_inbound_attestations.entry(conn_id.to_string()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(RelayInboundAttestation {
                    relay_generation: Arc::downgrade(multi),
                    source_peer_id: source_peer_id.to_string(),
                    logical_tuple: None,
                });
                Ok(())
            }
            dashmap::mapref::entry::Entry::Occupied(_) => {
                Err("relay source attestation conn_id is already bound".into())
            }
        }
    }

    fn remove_relay_inbound_attestation_for_generation(
        &self,
        conn_id: &str,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        self.relay_inbound_attestations
            .remove_if(conn_id, |_, attestation| {
                attestation
                    .relay_generation
                    .upgrade()
                    .is_some_and(|bound| Arc::ptr_eq(&bound, multi))
            });
    }

    fn remove_pending_relay_inbound_attestation_for_generation(
        &self,
        conn_id: &str,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        self.relay_inbound_attestations
            .remove_if(conn_id, |_, attestation| {
                attestation.logical_tuple.is_none()
                    && attestation
                        .relay_generation
                        .upgrade()
                        .is_some_and(|bound| Arc::ptr_eq(&bound, multi))
            });
    }

    fn clear_relay_inbound_attestations_for_generation(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        self.relay_inbound_attestations.retain(|_, attestation| {
            !attestation
                .relay_generation
                .upgrade()
                .is_none_or(|bound| Arc::ptr_eq(&bound, multi))
        });
    }

    fn has_relay_inbound_attestation_for_generation(
        &self,
        conn_id: &str,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) -> bool {
        self.relay_inbound_attestations
            .get(conn_id)
            .is_some_and(|attestation| {
                attestation
                    .relay_generation
                    .upgrade()
                    .is_some_and(|bound| Arc::ptr_eq(&bound, multi))
            })
    }

    fn published_local_service_lan_hosts(&self) -> Vec<std::net::IpAddr> {
        self.local_lan_publication
            .read()
            .hosts
            .iter()
            .copied()
            .map(std::net::IpAddr::V4)
            .collect()
    }

    fn local_route_claims(&self) -> LocalRouteClaims {
        let overlay = self
            .latest_tunnel_config()
            .and_then(|config| config.overlay_ipv4.parse().ok());
        LocalRouteClaims {
            overlay,
            peer_lan_hosts: self.published_local_service_lan_hosts(),
        }
    }

    fn relay_attested_source(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
        conn_id: &str,
    ) -> Result<Option<String>, String> {
        let Some(attestation) = self.relay_inbound_attestations.get(conn_id) else {
            return Ok(None);
        };
        let exact_generation = attestation
            .relay_generation
            .upgrade()
            .is_some_and(|bound| Arc::ptr_eq(&bound, multi));
        if !exact_generation {
            drop(attestation);
            self.relay_inbound_attestations.remove(conn_id);
            return Err(
                "relay source attestation belongs to a different session generation".into(),
            );
        }
        if attestation.logical_tuple.is_some() {
            return Err("relay local conn_id has already consumed its attestation".into());
        }
        Ok(Some(attestation.source_peer_id.clone()))
    }

    fn lock_relay_inbound_tuple(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
        conn_id: &str,
        source_peer_id: &str,
        logical_tuple: InboundLogicalTuple,
    ) -> Result<(), String> {
        let Some(mut attestation) = self.relay_inbound_attestations.get_mut(conn_id) else {
            return Err("relay local destination lacks a source attestation".into());
        };
        if !attestation
            .relay_generation
            .upgrade()
            .is_some_and(|bound| Arc::ptr_eq(&bound, multi))
            || attestation.source_peer_id != source_peer_id
        {
            return Err("relay source attestation session or Peer changed".into());
        }
        if attestation.logical_tuple.is_some() {
            return Err("relay local conn_id has already consumed its attestation".into());
        }
        attestation.logical_tuple = Some(logical_tuple);
        Ok(())
    }

    async fn resolve_inbound_dial_target(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
        p2p_session: Option<&Arc<tp_transport::session::Session>>,
        conn_id: &str,
        protocol: Protocol,
        address: &str,
    ) -> Result<InboundDialTarget, String> {
        let v2_relay_source = if p2p_session.is_none() {
            self.v2_relay_flows
                .get(conn_id)
                .map(|flow| flow.remote_peer_id.clone())
        } else {
            None
        };
        let relay_source = if p2p_session.is_none() {
            match v2_relay_source.clone() {
                Some(source) => Some(source),
                None => self.relay_attested_source(multi, conn_id)?,
            }
        } else {
            None
        };
        let source_peer_id = if let Some(p2p_session) = p2p_session {
            let local_peer_id = self
                .latest_tunnel_config()
                .map(|config| config.peer_id)
                .filter(|peer| !peer.trim().is_empty())
                .ok_or_else(|| "direct session lacks the local stable Peer identity".to_string())?;
            Some(
                multi
                    .authenticated_remote_peer_for_handle(p2p_session, &local_peer_id)
                    .ok_or_else(|| {
                        "direct session is not installed for a canonical Peer relation".to_string()
                    })?,
            )
        } else {
            relay_source.clone()
        };

        if let Some(profile) = self.active_v2_peer_profile() {
            source_peer_id
                .as_deref()
                .ok_or_else(|| "V2 delivery lacks an authenticated source Peer".to_string())?;
            let policy = self.v2_access_policy.read().clone();
            let local_runtime = self.v2_local_runtime_record.read().clone();
            let requested_target = resolve_target_addr_once(address, true)
                .await
                .map_err(|_| "NotAuthorized".to_string())?;
            let requested = requested_target.socket;
            let class = if requested.ip() == std::net::IpAddr::V4(profile.peer.overlay_ip) {
                crate::access_policy::ClientAccessTargetClassV2::ThisPeer {
                    own_overlay: profile.peer.overlay_ip,
                }
            } else {
                let std::net::IpAddr::V4(ip) = requested.ip() else {
                    return Err("NotAuthorized".into());
                };
                if !local_runtime
                    .lan_exports
                    .iter()
                    .any(|export| export.ready && export.prefix.contains(ip))
                {
                    return Err("NotAuthorized".into());
                }
                crate::access_policy::ClientAccessTargetClassV2::Other {
                    requested_host: &requested_target.original_host,
                }
            };
            let target = match policy.decide(class, protocol, requested) {
                crate::access_policy::ClientAccessDecisionV2::AllowDirect => requested,
                crate::access_policy::ClientAccessDecisionV2::AllowThisPeer { final_target } => {
                    let final_target = resolve_target_addr_once(&final_target, false)
                        .await
                        .map_err(|_| "NotAuthorized".to_string())?;
                    if !policy.mapped_final_allowed(
                        protocol,
                        &final_target.original_host,
                        final_target.socket,
                    ) {
                        return Err("NotAuthorized".into());
                    }
                    final_target.socket
                }
                crate::access_policy::ClientAccessDecisionV2::Deny => {
                    return Err("NotAuthorized".into())
                }
            };
            return Ok(InboundDialTarget {
                address: target.to_string(),
                relay_local_authorized: p2p_session.is_none(),
                v2_access_authorized: true,
            });
        }

        let requested = match address.parse::<SocketAddr>() {
            Ok(requested) => requested,
            Err(_) => {
                if relay_source.is_some()
                    || (p2p_session.is_some() && self.uses_exact_peer_routing())
                {
                    return Err("Peer-local delivery requires a literal socket address".into());
                }
                return Ok(InboundDialTarget {
                    address: address.to_string(),
                    relay_local_authorized: false,
                    v2_access_authorized: false,
                });
            }
        };
        let resolver = LocalTargetResolver::new(
            self.local_route_claims(),
            self.local_service_exports.read().clone(),
        )
        .map_err(|error| format!("invalid local service export policy: {error:?}"))?;
        match resolver.resolve(source_peer_id.as_deref(), protocol, requested) {
            Ok(Some(resolved)) => {
                if p2p_session.is_none() {
                    let source_peer_id = relay_source.as_deref().ok_or_else(|| {
                        "Peer-local relay delivery lacks source attestation".to_string()
                    })?;
                    if v2_relay_source.is_none() {
                        self.lock_relay_inbound_tuple(
                            multi,
                            conn_id,
                            source_peer_id,
                            InboundLogicalTuple {
                                route_kind: resolved.route_kind,
                                protocol,
                                requested,
                            },
                        )?;
                    }
                }
                Ok(InboundDialTarget {
                    address: resolved.target.to_string(),
                    relay_local_authorized: p2p_session.is_none(),
                    v2_access_authorized: false,
                })
            }
            Ok(None) => {
                if relay_source.is_some()
                    || (p2p_session.is_some() && self.uses_exact_peer_routing())
                {
                    return Err("destination is not claimed by the selected local Peer".into());
                }
                Ok(InboundDialTarget {
                    address: address.to_string(),
                    relay_local_authorized: false,
                    v2_access_authorized: false,
                })
            }
            Err(error) => Err(format!("local service export denied delivery: {error:?}")),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::collapsible_match)]
    async fn handle_msg(
        self: &Arc<Self>,
        msg: BinaryMessage,
        multi: &Arc<crate::p2p::session::MultiSession>,
        inbound: &Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        udp_inbound: &Arc<DashMap<String, DropOldestSender<Bytes>>>,
        host_filter: &Arc<HostFilter>,
        tracker: &TaskTracker,
        p2p_reply_session: Option<Arc<tp_transport::session::Session>>,
        liveness: Option<LinkLivenessState>,
    ) {
        let now_ms = monotonic_millis();
        if let Some(liveness) = liveness.as_ref() {
            liveness.record_link_progress(now_ms);
        }

        match msg {
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id: remote_peer_id,
                peerlink_session_id,
                conn_id,
                route_abort,
                sealed,
            } => {
                if let Some(direct) = p2p_reply_session.as_ref() {
                    if conn_id != [0; 12] || route_abort {
                        tracing::warn!("non-Gossip encrypted control arrived on Direct");
                        return;
                    }
                    let Some(local_peer_id) = self
                        .active_v2_peer_profile()
                        .map(|profile| profile.peer.peer_id.clone())
                    else {
                        return;
                    };
                    if multi
                        .authenticated_remote_peer_for_handle(direct, &local_peer_id)
                        .as_deref()
                        != Some(remote_peer_id.as_str())
                    {
                        tracing::warn!(%remote_peer_id, "Direct Gossip source identity mismatch");
                        return;
                    }
                }
                let session_id = tp_core::p2p_types::SessionId::from_bytes(peerlink_session_id);
                let Some(flow) = self.v2_relay_flow_from_remote(&remote_peer_id, session_id) else {
                    tracing::warn!(
                        %remote_peer_id,
                        "encrypted Relay control has no authenticated PeerLink"
                    );
                    return;
                };
                let aad = match crate::relay_crypto::RelayAadV2::control(
                    flow.record_context(&conn_id, false),
                    route_abort,
                ) {
                    Ok(aad) => aad,
                    Err(_) => return,
                };
                let plaintext = match flow.cipher.open_bytes_precomputed(&aad, sealed) {
                    Ok(plaintext) => plaintext,
                    Err(_) => {
                        tracing::warn!(%remote_peer_id, "encrypted Relay control authentication failed");
                        return;
                    }
                };
                let Ok(payload) = crate::relay_crypto::RelayControlPayloadV2::decode(&plaintext)
                else {
                    tracing::warn!(%remote_peer_id, "encrypted Relay control payload is invalid");
                    return;
                };
                if p2p_reply_session.is_some()
                    && !matches!(
                        &payload,
                        crate::relay_crypto::RelayControlPayloadV2::RuntimeRecord(_)
                            | crate::relay_crypto::RelayControlPayloadV2::Digest(_)
                            | crate::relay_crypto::RelayControlPayloadV2::Need
                    )
                {
                    tracing::warn!(%remote_peer_id, "non-Gossip payload arrived on Direct control");
                    return;
                }
                if route_abort {
                    if conn_id != [0; 12] {
                        if let Some(conn_id) = relay_conn_id_from_wire_v2(&conn_id) {
                            self.v2_relay_flows.remove(&conn_id);
                            inbound.remove(&conn_id);
                            udp_inbound.remove(&conn_id);
                            self.proxy_flow_registry.remove(&conn_id);
                        }
                    }
                    return;
                }
                match payload {
                    crate::relay_crypto::RelayControlPayloadV2::Open { network, address } => {
                        let Some(conn_id_string) = relay_conn_id_from_wire_v2(&conn_id) else {
                            return;
                        };
                        let Some(flow) = flow.clone().with_inbound_framed_aad(&conn_id) else {
                            return;
                        };
                        if self
                            .v2_relay_flows
                            .insert(conn_id_string.clone(), flow)
                            .is_some()
                        {
                            self.v2_relay_flows.remove(&conn_id_string);
                            return;
                        }
                        let synthetic = BinaryMessage::Connect {
                            conn_id: conn_id_string,
                            network,
                            address,
                        };
                        Box::pin(self.handle_msg(
                            synthetic,
                            multi,
                            inbound,
                            udp_inbound,
                            host_filter,
                            tracker,
                            None,
                            liveness,
                        ))
                        .await;
                    }
                    crate::relay_crypto::RelayControlPayloadV2::OpenResponse { success, error } => {
                        let Some(conn_id) = relay_conn_id_from_wire_v2(&conn_id) else {
                            return;
                        };
                        if success {
                            let Some(conn_id_wire) = relay_conn_id_to_wire_v2(&conn_id) else {
                                return;
                            };
                            let Some(flow) = flow.with_inbound_framed_aad(&conn_id_wire) else {
                                return;
                            };
                            self.v2_relay_flows.insert(conn_id.clone(), flow);
                        } else {
                            self.v2_relay_flows.remove(&conn_id);
                        }
                        if self.handle_proxy_connect_response(conn_id, success, error) {
                            multi.mark_progress();
                        }
                    }
                    gossip_payload
                    @ (crate::relay_crypto::RelayControlPayloadV2::RuntimeRecord(
                        _,
                    )
                    | crate::relay_crypto::RelayControlPayloadV2::Digest(_)
                    | crate::relay_crypto::RelayControlPayloadV2::Need) => {
                        if conn_id == [0; 12] {
                            self.receive_v2_gossip(&remote_peer_id, gossip_payload);
                        }
                    }
                }
            }
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                let result = if p2p_reply_session.is_some() {
                    Err("relay source attestation arrived on a direct P2P session".into())
                } else {
                    self.install_relay_inbound_attestation(multi, &conn_id, &peer_client_id)
                };
                let (success, error) = match result {
                    Ok(()) => (true, String::new()),
                    Err(error) => {
                        self.remove_relay_inbound_attestation_for_generation(&conn_id, multi);
                        (false, error)
                    }
                };
                let sender = p2p_reply_session.as_ref().unwrap_or_else(|| multi.relay());
                if sender
                    .send(BinaryMessage::RelayRouteBindAck {
                        conn_id: conn_id.clone(),
                        success,
                        error,
                    })
                    .await
                    .is_err()
                    && success
                {
                    self.remove_relay_inbound_attestation_for_generation(&conn_id, multi);
                }
            }
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                // Dedup: if a prior `Connect` for this conn_id already
                // dialed, reply success without re-dialing. This handles
                // the path-flip retry case where the gateway re-issues
                // `Connect` after a P2P→relay flip — the local target
                // socket is still live, so a fresh dial would create a
                // second fd and orphan the first. The maps live on
                // `MultiSession` per Task 4.9 precisely so they survive
                // the flip.
                // Build the router from the same MultiSession generation
                // that owns these inbound maps, then pin the TCP flow to
                // its ingress path. A relay fallback must not send the
                // response back over a congested P2P path.
                let existing_conn =
                    inbound.contains_key(&conn_id) || udp_inbound.contains_key(&conn_id);
                let local_client_id = self.local_client_id_for_multi(multi);
                let relay_source_attested = p2p_reply_session.is_none()
                    && (self.has_relay_inbound_attestation_for_generation(&conn_id, multi)
                        || self.v2_relay_flows.contains_key(&conn_id));
                let v2_relay_flow =
                    p2p_reply_session.is_none() && self.v2_relay_flows.contains_key(&conn_id);
                let router = match p2p_reply_session.as_ref() {
                    Some(p2p) if self.uses_v2_peer_profile() => {
                        MultiSenderRouter::new_pinned_p2p_no_relay_fallback(
                            multi.clone(),
                            p2p.clone(),
                        )
                    }
                    Some(p2p) => MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p.clone()),
                    None => {
                        if relay_source_attested {
                            MultiSenderRouter::new_relay_only(multi.clone())
                        } else {
                            MultiSenderRouter::new_relay_with_p2p_fallback(multi.clone())
                        }
                    }
                };
                let router = match local_client_id {
                    Some(local_client_id) => router.with_local_client_id(local_client_id),
                    None => router,
                };
                let router = if p2p_reply_session.is_none() {
                    self.v2_relay_flows
                        .get(&conn_id)
                        .and_then(|flow| flow.seal_context(&conn_id))
                        .map(|context| router.clone().with_v2_relay_seal(context))
                        .unwrap_or(router)
                } else {
                    router
                };
                let protocol = match network.as_str() {
                    "tcp" => Protocol::Tcp,
                    "udp" => Protocol::Udp,
                    other => {
                        let _ = router
                            .send(BinaryMessage::ConnectResponse {
                                conn_id: conn_id.clone(),
                                success: false,
                                error: format!("unsupported network: {other}"),
                            })
                            .await;
                        self.remove_pending_relay_inbound_attestation_for_generation(
                            &conn_id, multi,
                        );
                        return;
                    }
                };
                let dial_target = match self
                    .resolve_inbound_dial_target(
                        multi,
                        p2p_reply_session.as_ref(),
                        &conn_id,
                        protocol,
                        &address,
                    )
                    .await
                {
                    Ok(target) => target,
                    Err(error) => {
                        self.remove_pending_relay_inbound_attestation_for_generation(
                            &conn_id, multi,
                        );
                        tracing::warn!(
                            %conn_id,
                            protocol = protocol.as_str(),
                            reason = "local_service_export_rejected",
                            "inbound target authorization rejected connect"
                        );
                        let _ = router
                            .send(BinaryMessage::ConnectResponse {
                                conn_id: conn_id.clone(),
                                success: false,
                                error,
                            })
                            .await;
                        return;
                    }
                };
                if existing_conn {
                    if v2_relay_flow {
                        let _ = router
                            .send(BinaryMessage::ConnectResponse {
                                conn_id,
                                success: true,
                                error: String::new(),
                            })
                            .await;
                        return;
                    }
                    if dial_target.relay_local_authorized {
                        self.remove_relay_inbound_attestation_for_generation(&conn_id, multi);
                        let _ = router
                            .send(BinaryMessage::ConnectResponse {
                                conn_id,
                                success: false,
                                error: "relay local conn_id is already active".into(),
                            })
                            .await;
                        return;
                    }
                    if let Some(m) = self.metrics.lock().clone() {
                        m.incr_p2p_conn_id_dedup();
                    }
                    let _ = router
                        .send(BinaryMessage::ConnectResponse {
                            conn_id,
                            success: true,
                            error: String::new(),
                        })
                        .await;
                    return;
                }
                let v2_access_authorized = dial_target.v2_access_authorized;
                let dial_address = dial_target.address;
                // Enforce the host filter BEFORE any dial happens.
                if !v2_access_authorized
                    && (!host_filter.is_allowed(&address)
                        || (dial_address != address && !host_filter.is_allowed(&dial_address)))
                {
                    tracing::warn!(%address, %dial_address, %network, "host filter rejected connect");
                    let _ = router
                        .send(BinaryMessage::ConnectResponse {
                            conn_id: conn_id.clone(),
                            success: false,
                            error: format!("forbidden host: {address}"),
                        })
                        .await;
                    return;
                }
                let active_flow = liveness
                    .as_ref()
                    .and_then(|liveness| liveness.begin_flow(&network, &conn_id));
                let out2 = router;
                let inbound2 = inbound.clone();
                let udp2 = udp_inbound.clone();
                let multi2 = multi.clone();
                let relay_attestation_guard = RelayInboundAttestationGuard::new(
                    self.clone(),
                    multi.clone(),
                    conn_id.clone(),
                    dial_target.relay_local_authorized,
                );
                // Register with the replica TaskTracker so `run_replica`'s
                // teardown waits for every per-connect pipe task instead of
                // leaking them as detached futures (the old `tokio::spawn`
                // path would orphan one pipe_tcp/pipe_udp per in-flight
                // `Connect` on replica exit; the tasks did exit via
                // `out.closed()` but the window was uncontrolled).
                tracker.spawn(async move {
                    let _relay_attestation_guard = relay_attestation_guard;
                    let _active_flow = active_flow;
                    match network.as_str() {
                        // 10 s upper bound on the dial. OS default TCP
                        // connect can wait 60–180 s when the destination
                        // is blackholed (firewalled Google/Firebase hosts
                        // from a China-egress path are the common case).
                        // The gateway already gave up at 15 s
                        // (`ClientConn::open` timeout), so anything past
                        // ~13 s is a guaranteed orphan: the `conn_id` has
                        // been cleaned up server-side, so even a
                        // late-success ConnectResponse would find no
                        // pending entry. Each such orphan holds a TCP fd
                        // + task slot; clash retries aggressively, so the
                        // client accumulates thousands of them over hours
                        // — the exact "after a long running" pathology.
                        "tcp" => match tokio::time::timeout(
                            Duration::from_secs(10),
                            TcpStream::connect(&dial_address),
                        )
                        .await
                        {
                            Ok(Ok(stream)) => {
                                let (tx_in, rx_in) = mpsc::channel::<Bytes>(1024);
                                inbound2.insert(conn_id.clone(), tx_in);
                                if out2
                                    .send(BinaryMessage::ConnectResponse {
                                        conn_id: conn_id.clone(),
                                        success: true,
                                        error: String::new(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    inbound2.remove(&conn_id);
                                    return;
                                }
                                pipe_tcp(conn_id.clone(), stream, rx_in, out2, inbound2).await;
                            }
                            Ok(Err(e)) => {
                                let _ = out2
                                    .send(BinaryMessage::ConnectResponse {
                                        conn_id: conn_id.clone(),
                                        success: false,
                                        error: e.to_string(),
                                    })
                                    .await;
                            }
                            Err(_) => {
                                tracing::debug!(
                                    %address,
                                    "tcp connect timed out after 10s; giving up"
                                );
                                let _ = out2
                                    .send(BinaryMessage::ConnectResponse {
                                        conn_id: conn_id.clone(),
                                        success: false,
                                        error: "tcp connect timed out".into(),
                                    })
                                    .await;
                            }
                        },
                        "udp" => match bind_tokio_target_udp_socket() {
                            Ok(socket) => match socket.connect(&dial_address).await {
                                Ok(()) => {
                                    let (tx_in, rx_in) =
                                        drop_oldest_channel::<Bytes>(UDP_FLOW_INBOUND_CHANNEL_CAP);
                                    udp2.insert(conn_id.clone(), tx_in.clone());
                                    for pending in multi2.drain_pending_udp_inbound(&conn_id) {
                                        let len = pending.payload.len();
                                        if tx_in.send_drop_oldest(pending.payload).is_ok() {
                                            multi2.record_traffic_rx(pending.path, len);
                                        }
                                    }
                                    if out2
                                        .send(BinaryMessage::ConnectResponse {
                                            conn_id: conn_id.clone(),
                                            success: true,
                                            error: String::new(),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        udp2.remove(&conn_id);
                                        multi2.clear_pending_udp_inbound(&conn_id);
                                        return;
                                    }
                                    pipe_udp(conn_id.clone(), socket, rx_in, out2, udp2).await;
                                }
                                Err(e) => {
                                    multi2.clear_pending_udp_inbound(&conn_id);
                                    let _ = out2
                                        .send(BinaryMessage::ConnectResponse {
                                            conn_id: conn_id.clone(),
                                            success: false,
                                            error: e.to_string(),
                                        })
                                        .await;
                                }
                            },
                            Err(e) => {
                                multi2.clear_pending_udp_inbound(&conn_id);
                                let _ = out2
                                    .send(BinaryMessage::ConnectResponse {
                                        conn_id: conn_id.clone(),
                                        success: false,
                                        error: e.to_string(),
                                    })
                                    .await;
                            }
                        },
                        other => {
                            let _ = out2
                                .send(BinaryMessage::ConnectResponse {
                                    conn_id: conn_id.clone(),
                                    success: false,
                                    error: format!("unsupported network: {other}"),
                                })
                                .await;
                        }
                    }
                });
            }
            BinaryMessage::Data {
                conn_id,
                mut payload,
            } => {
                let path = if p2p_reply_session.is_some() {
                    TrafficPath::P2p
                } else {
                    TrafficPath::Relay
                };
                // Clone the Sender out of the Ref and drop the Ref BEFORE
                // awaiting — same Ref-across-await hazard as the gateway's
                // `ClientConn::handle_inbound`. A parking_lot read lock
                // held across a suspended future blocks any concurrent
                // `inbound.insert(...)` from the Connect spawn path, which
                // would pin a tokio worker thread synchronously.
                if p2p_reply_session.is_none() && self.active_v2_peer_profile().is_some() {
                    let Some(flow) = self.v2_relay_flows.get(&conn_id) else {
                        return;
                    };
                    let Some(aad) =
                        flow.inbound_framed_aad(crate::relay_crypto::RelayFramedKindV2::Data)
                    else {
                        return;
                    };
                    let opened = match flow.cipher.open_bytes_precomputed(aad, payload) {
                        Ok(opened) => opened,
                        Err(_) => {
                            drop(flow);
                            self.v2_relay_flows.remove(&conn_id);
                            return;
                        }
                    };
                    payload = opened;
                }
                let chan = inbound.get(&conn_id).map(|r| r.clone());
                if let Some(chan) = chan {
                    let len = payload.len();
                    if chan.send(payload).await.is_ok() {
                        multi.record_traffic_rx(path, len);
                    }
                }
            }
            BinaryMessage::UdpData {
                conn_id,
                mut payload,
            } => {
                let path = if p2p_reply_session.is_some() {
                    TrafficPath::P2p
                } else {
                    TrafficPath::Relay
                };
                if p2p_reply_session.is_none() && self.active_v2_peer_profile().is_some() {
                    let Some(flow) = self.v2_relay_flows.get(&conn_id) else {
                        return;
                    };
                    let Some(aad) =
                        flow.inbound_framed_aad(crate::relay_crypto::RelayFramedKindV2::UdpData)
                    else {
                        return;
                    };
                    payload = match flow.cipher.open_bytes_precomputed(aad, payload) {
                        Ok(opened) => opened,
                        Err(_) => return,
                    };
                }
                if let Some(chan) = udp_inbound.get(&conn_id) {
                    if !chan.is_closed() {
                        let len = payload.len();
                        if chan.send_drop_oldest(payload).is_ok() {
                            multi.record_traffic_rx(path, len);
                        }
                    }
                } else {
                    match multi.buffer_pending_udp_inbound(&conn_id, path, payload) {
                        PendingUdpInboundBufferResult::Buffered { dropped_oldest } => {
                            tracing::debug!(
                                conn_id = %conn_id,
                                ?path,
                                dropped_oldest,
                                "client buffered UDP data for pending conn_id"
                            );
                        }
                        PendingUdpInboundBufferResult::DroppedConnCap => {
                            tracing::debug!(
                                conn_id = %conn_id,
                                ?path,
                                "client dropped UDP data for unknown conn_id because pending buffer is full"
                            );
                        }
                    }
                }
            }
            BinaryMessage::Close { conn_id } => {
                self.v2_relay_flows.remove(&conn_id);
                self.remove_relay_inbound_attestation_for_generation(&conn_id, multi);
                if let Some(liveness) = liveness.as_ref() {
                    liveness.end_flow(&conn_id);
                }
                let removed_tcp = inbound.remove(&conn_id).is_some();
                let removed_udp = udp_inbound.get(&conn_id).is_some();
                if removed_udp {
                    schedule_udp_inbound_close_grace(
                        conn_id.clone(),
                        udp_inbound.clone(),
                        multi.clone(),
                        tracker,
                    );
                } else {
                    multi.clear_pending_udp_inbound(&conn_id);
                }
                self.proxy_flow_registry.remove(&conn_id);
                if removed_tcp || removed_udp {
                    multi.mark_progress();
                }
            }
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } => {
                if self.handle_proxy_connect_response(conn_id, success, error) {
                    multi.mark_progress();
                }
            }
            BinaryMessage::RelayRouteBindAck {
                conn_id,
                success,
                error,
            } => {
                if p2p_reply_session.is_some() {
                    tracing::debug!(
                        %conn_id,
                        success,
                        "ignored relay route bind ack received from P2P path"
                    );
                } else {
                    self.handle_relay_route_bind_ack(multi, conn_id, success, error);
                }
            }
            BinaryMessage::HeartbeatAck { timestamp } => {
                if let Some(liveness) = liveness.as_ref() {
                    liveness.record_ack(now_ms);
                }
                let mut s = self.state.write();
                s.transport_heartbeat = HeartbeatStatus {
                    active: true,
                    last_time: Some(timestamp),
                    last_error: None,
                };
            }
            BinaryMessage::Heartbeat { timestamp, .. } => {
                if let Some(reply) = p2p_reply_session {
                    let send_started_ms = monotonic_millis();
                    let send_result = reply.send(BinaryMessage::HeartbeatAck { timestamp }).await;
                    let heartbeat_ack_send_elapsed_ms =
                        monotonic_millis().saturating_sub(send_started_ms);
                    if heartbeat_ack_send_elapsed_ms >= 500 {
                        tracing::warn!(
                            link_kind = "p2p",
                            peer = %reply.peer_addr(),
                            ts = timestamp,
                            heartbeat_ack_send_elapsed_ms,
                            "P2P heartbeat ACK send was delayed"
                        );
                    }
                    if send_result.is_err() {
                        let peer_client_id = multi.p2p_peer_client_id_for_handle(&reply);
                        let p2p_session_id = multi.p2p_session_id_for_handle(&reply);
                        let closed = multi.close_p2p_session_for_handle(&reply);
                        if closed {
                            multi.report_p2p_to_relay_migration_with_context(
                                "heartbeat_ack_send_failed",
                                None,
                                None,
                                p2p_session_id,
                            );
                        }
                        if let Some(peer_client_id) = peer_client_id.as_deref() {
                            self.request_p2p_refill(peer_client_id);
                        }
                        if let Some(session_id) = p2p_session_id {
                            self.notify_p2p_relation_closed(session_id);
                        }
                        tracing::warn!(
                            link_kind = "p2p",
                            peer = %reply.peer_addr(),
                            ts = timestamp,
                            p2p_link_closed = closed,
                            peer_client_id = peer_client_id.as_deref().unwrap_or(""),
                            "P2P heartbeat ACK send failed; closing direct link"
                        );
                    }
                }
            }
            _ => {}
        }
    }

    async fn handle_tcp_flow_stream(
        self: Arc<Self>,
        mut incoming: tp_transport::TcpFlowIncoming,
        multi: Arc<crate::p2p::session::MultiSession>,
        host_filter: Arc<HostFilter>,
        path: TrafficPath,
        link_context: TcpFlowLinkContext,
    ) {
        let TcpFlowLinkContext {
            p2p_source_session,
            link_progress_ms,
            link_active_flow,
        } = link_context;
        let _link_active_flow = link_active_flow;
        if let Some(raw_preface) = incoming.stream.raw_preface().cloned() {
            self.handle_v2_relay_tcp_flow_stream(
                incoming,
                raw_preface,
                multi,
                host_filter,
                link_progress_ms,
            )
            .await;
            return;
        }
        let conn_id = incoming.preface.conn_id.clone();
        let address = incoming.preface.address.clone();
        let dial_target = match self
            .resolve_inbound_dial_target(
                &multi,
                p2p_source_session.as_ref(),
                &conn_id,
                Protocol::Tcp,
                &address,
            )
            .await
        {
            Ok(target) => target,
            Err(error) => {
                self.remove_pending_relay_inbound_attestation_for_generation(&conn_id, &multi);
                tracing::warn!(
                    %conn_id,
                    protocol = "tcp",
                    reason = "local_service_export_rejected",
                    "inbound target authorization rejected tcp flow stream"
                );
                let _ = incoming.stream.send_connect_response(false, error).await;
                return;
            }
        };
        let v2_access_authorized = dial_target.v2_access_authorized;
        let dial_address = dial_target.address;
        let _relay_attestation_guard = RelayInboundAttestationGuard::new(
            self.clone(),
            multi.clone(),
            conn_id.clone(),
            dial_target.relay_local_authorized,
        );
        let flow_started = Instant::now();
        if !v2_access_authorized
            && (!host_filter.is_allowed(&address)
                || (dial_address != address && !host_filter.is_allowed(&dial_address)))
        {
            tracing::warn!(%conn_id, "host filter rejected tcp flow stream");
            let _ = incoming
                .stream
                .send_connect_response(false, format!("forbidden host: {address}"))
                .await;
            return;
        }
        let active_tcp_flow = multi.begin_tcp_flow_stream();
        multi.mark_progress();
        let target_connect_started = Instant::now();
        let mut target =
            match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&dial_address))
                .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    let _ = incoming
                        .stream
                        .send_connect_response(false, e.to_string())
                        .await;
                    return;
                }
                Err(_) => {
                    let _ = incoming
                        .stream
                        .send_connect_response(false, "tcp connect timed out".into())
                        .await;
                    return;
                }
            };
        if incoming
            .stream
            .send_connect_response(true, String::new())
            .await
            .is_err()
        {
            return;
        }
        tracing::debug!(
            %conn_id,
            %address,
            path = ?path,
            target_connect_elapsed_ms = target_connect_started.elapsed().as_millis(),
            "tcp flow stream accepted"
        );
        let path_kind = match path {
            TrafficPath::Relay => PathKind::Relay,
            TrafficPath::P2p => PathKind::P2p,
        };
        let rx_multi = multi.clone();
        let tx_multi = multi.clone();
        let rx_read_link_progress_ms = link_progress_ms.clone();
        let rx_written_link_progress_ms = link_progress_ms.clone();
        let tx_link_progress_ms = link_progress_ms.clone();
        match copy_bidirectional_with_progress(
            &mut incoming.stream,
            &mut target,
            move |n| {
                let _ = n;
                if let Some(progress) = &rx_read_link_progress_ms {
                    progress.store(monotonic_millis(), Ordering::Relaxed);
                }
            },
            move |n| {
                rx_multi.record_traffic_rx(path, n);
                if let Some(progress) = &rx_written_link_progress_ms {
                    progress.store(monotonic_millis(), Ordering::Relaxed);
                }
            },
            |_| {},
            move |n| {
                tx_multi.record_traffic_tx(path_kind, i64::try_from(n).unwrap_or(i64::MAX));
                if let Some(progress) = &tx_link_progress_ms {
                    progress.store(monotonic_millis(), Ordering::Relaxed);
                }
            },
        )
        .await
        {
            Ok((from_peer, to_peer)) => {
                tracing::debug!(
                    %conn_id,
                    %address,
                    path = ?path,
                    from_peer_bytes = from_peer,
                    to_peer_bytes = to_peer,
                    elapsed_ms = flow_started.elapsed().as_millis(),
                    "tcp flow stream bridge completed"
                );
            }
            Err(e) => {
                tracing::debug!(%conn_id, error = %e, "tcp flow stream bridge ended with error");
            }
        }
        drop(active_tcp_flow);
        multi.mark_progress();
    }

    async fn handle_v2_relay_tcp_flow_stream(
        self: Arc<Self>,
        mut incoming: tp_transport::TcpFlowIncoming,
        raw_preface: Bytes,
        multi: Arc<crate::p2p::session::MultiSession>,
        host_filter: Arc<HostFilter>,
        link_progress_ms: Option<Arc<AtomicU64>>,
    ) {
        let Ok(open) = unpack_tcp_flow_open_v2(&raw_preface) else {
            return;
        };
        let conn_id = open.conn_id;
        let Some(conn_id_wire) = relay_conn_id_to_wire_v2(&conn_id) else {
            return;
        };
        let session_id = tp_core::p2p_types::SessionId::from_bytes(open.peerlink_session_id);
        let Some(flow) = self.v2_relay_flow_from_session(session_id) else {
            tracing::warn!(%conn_id, "sealed Relay TCP OPEN has no unique authenticated PeerLink");
            return;
        };
        let mut address = open.sealed_open.to_vec();
        if flow
            .cipher
            .open_flow(
                flow.record_context(&conn_id_wire, false),
                crate::relay_crypto::RelayFlowKindV2::Open,
                &mut address,
            )
            .is_err()
        {
            tracing::warn!(%conn_id, "sealed Relay TCP OPEN authentication failed");
            return;
        }
        let Ok(address) = String::from_utf8(address) else {
            return;
        };
        if self
            .v2_relay_flows
            .insert(conn_id.clone(), flow.clone())
            .is_some()
        {
            self.v2_relay_flows.remove(&conn_id);
            return;
        }
        let response = match self
            .resolve_inbound_dial_target(&multi, None, &conn_id, Protocol::Tcp, &address)
            .await
        {
            Ok(target)
                if target.v2_access_authorized
                    || host_filter.is_allowed(&address)
                        && (target.address == address
                            || host_filter.is_allowed(&target.address)) =>
            {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    TcpStream::connect(&target.address),
                )
                .await
                {
                    Ok(Ok(target)) => Ok(target),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("tcp connect timed out".into()),
                }
            }
            Ok(_) => Err(format!("forbidden host: {address}")),
            Err(error) => Err(error),
        };
        let (success, error) = match &response {
            Ok(_) => (true, String::new()),
            Err(error) => (false, error.clone()),
        };
        let Ok(mut sealed_response) =
            (crate::relay_crypto::RelayControlPayloadV2::OpenResponse { success, error }).encode()
        else {
            self.v2_relay_flows.remove(&conn_id);
            return;
        };
        if flow
            .cipher
            .seal_flow(
                flow.record_context(&conn_id_wire, true),
                crate::relay_crypto::RelayFlowKindV2::OpenResponse,
                &mut sealed_response,
            )
            .is_err()
            || tp_transport::session::write_tcp_flow_frame(&mut incoming.stream, &sealed_response)
                .await
                .is_err()
        {
            self.v2_relay_flows.remove(&conn_id);
            return;
        }
        let Ok(target) = response else {
            self.v2_relay_flows.remove(&conn_id);
            return;
        };
        let active_tcp_flow = multi.begin_tcp_flow_stream();
        multi.mark_progress();
        let bridge = copy_v2_relay_tcp_flow(
            incoming.stream,
            target,
            flow,
            conn_id_wire,
            multi.clone(),
            link_progress_ms,
        )
        .await;
        if let Err(error) = bridge {
            tracing::debug!(%conn_id, %error, "sealed Relay TCP flow ended");
        }
        self.v2_relay_flows.remove(&conn_id);
        drop(active_tcp_flow);
        multi.mark_progress();
    }

    // ---------------------------------------------------------------
    // Public surface for Task 4.11 (`apps/lantunnel-client/src-tauri/src/main.rs`) wiring.
    //
    // These accessors expose the live `MultiSession` and the P2P
    // signaling plumbing without changing `handle_msg`'s signature.
    // They return `None` until the first `run_replica` finishes
    // dialing — callers should poll or wire up after the Engine's
    // status flips to `connected`.
    // ---------------------------------------------------------------

    /// Snapshot the live `MultiSession` for the current replica, if any.
    /// Cleared on replica teardown.
    pub fn multi_session(&self) -> Option<Arc<crate::p2p::session::MultiSession>> {
        self.multi.lock().clone()
    }

    pub(crate) fn proxy_pending(
        &self,
    ) -> Arc<DashMap<String, oneshot::Sender<Result<(), String>>>> {
        self.proxy_pending.clone()
    }

    pub(crate) fn relay_route_bind_pending(&self) -> Arc<DashMap<String, RelayRouteBindPending>> {
        self.relay_route_bind_pending.clone()
    }

    pub(crate) fn mark_proxy_flow_established(&self, conn_id: &str) {
        self.proxy_flow_registry.mark_established(conn_id);
    }

    pub(crate) fn replace_proxy_flow(&self, conn_id: &str, flow_kind: FlowKind, key: CandidateKey) {
        self.proxy_flow_registry.replace(conn_id, flow_kind, key);
    }

    pub(crate) fn remove_proxy_flow(&self, conn_id: &str) {
        self.proxy_flow_registry.remove(conn_id);
        self.v2_relay_flows.remove(conn_id);
    }

    pub(crate) fn record_proxy_flow_outbound_payload_bytes(
        &self,
        conn_id: &str,
        flow_kind: FlowKind,
        payload_bytes: u64,
    ) {
        if let Some(key) = self.proxy_flow_registry.candidate_key(conn_id) {
            self.proxy_flow_registry
                .record_outbound_payload_bytes(&key, flow_kind, payload_bytes);
        }
    }

    pub(crate) fn record_proxy_flow_link_io_progress(&self, conn_id: &str) {
        if let Some(key) = self.proxy_flow_registry.candidate_key(conn_id) {
            self.proxy_flow_registry
                .record_link_io_progress_ms(&key, monotonic_millis());
        }
    }

    #[cfg(test)]
    pub(crate) fn proxy_flow_candidate_key_for_test(&self, conn_id: &str) -> Option<CandidateKey> {
        self.proxy_flow_registry.candidate_key(conn_id)
    }

    #[cfg(test)]
    pub(crate) fn record_proxy_flow_pending_for_test(
        &self,
        conn_id: &str,
        flow_kind: FlowKind,
        key: CandidateKey,
    ) {
        self.proxy_flow_registry
            .record_pending(conn_id.to_string(), flow_kind, key);
    }

    #[cfg(test)]
    pub(crate) fn proxy_flow_last_link_io_progress_for_test(&self, key: &CandidateKey) -> u64 {
        self.proxy_flow_registry
            .last_link_io_progress_ms_for_candidate(key)
    }

    fn handle_proxy_connect_response(&self, conn_id: String, success: bool, error: String) -> bool {
        if let Some((_, tx)) = self.proxy_pending.remove(&conn_id) {
            let _ = tx.send(if success { Ok(()) } else { Err(error) });
            true
        } else {
            false
        }
    }

    fn handle_relay_route_bind_ack(
        &self,
        multi: &Arc<crate::p2p::session::MultiSession>,
        conn_id: String,
        success: bool,
        error: String,
    ) {
        let exact_generation = self
            .relay_route_bind_pending
            .get(&conn_id)
            .is_some_and(|pending| {
                pending
                    .relay_generation
                    .upgrade()
                    .is_some_and(|bound| Arc::ptr_eq(&bound, multi))
            });
        if !exact_generation {
            tracing::warn!(%conn_id, "ignored relay route bind ack from the wrong session generation");
            return;
        }
        if let Some((_, pending)) =
            self.relay_route_bind_pending
                .remove_if(&conn_id, |_, pending| {
                    pending
                        .relay_generation
                        .upgrade()
                        .is_some_and(|bound| Arc::ptr_eq(&bound, multi))
                })
        {
            tracing::debug!(
                %conn_id,
                source_peer_id = %pending.key.source_peer_id,
                target_peer_id = %pending.key.target_peer_id,
                protocol = pending.key.protocol.as_str(),
                "matched relay route bind ack to exact source tuple"
            );
            let _ = pending
                .response
                .send(if success { Ok(()) } else { Err(error) });
        }
    }

    #[cfg(test)]
    pub(crate) fn install_multi_session_for_test(
        self: &Arc<Self>,
        multi: Arc<crate::p2p::session::MultiSession>,
    ) {
        self.bind_v2_lane_change_observer(&multi);
        *self.multi.lock() = Some(multi);
    }

    #[cfg(test)]
    pub(crate) fn install_proxy_replica_session_for_test(
        self: &Arc<Self>,
        client_id: &str,
        multi: Arc<crate::p2p::session::MultiSession>,
    ) {
        self.bind_v2_lane_change_observer(&multi);
        if self.p2p_anchor_client_id.lock().is_none() {
            *self.p2p_anchor_client_id.lock() = Some(client_id.to_string());
        }
        self.register_replica_multi_session(client_id, "group-test", multi, 0);
    }

    #[cfg(test)]
    pub(crate) fn set_p2p_anchor_client_id_for_test(&self, client_id: &str) {
        *self.p2p_anchor_client_id.lock() = Some(client_id.to_string());
    }

    #[cfg(test)]
    pub(crate) fn set_replicas_for_test(&self, replicas: usize) {
        *self.replicas.lock() = Some(replicas);
    }

    #[cfg(test)]
    pub(crate) fn set_group_context_for_test(
        &self,
        tunnel_id: &str,
        group_id: &str,
        host_filter: Arc<HostFilter>,
    ) {
        let context = TunnelGroupContext {
            tunnel_id: tunnel_id.to_string(),
            group_id: group_id.to_string(),
            anchor_client_id: self.p2p_anchor_client_id.lock().clone(),
            host_filter,
        };
        self.set_group_context(Arc::new(context));
    }

    #[cfg(test)]
    pub(crate) fn proxy_pending_contains_for_test(&self, conn_id: &str) -> bool {
        self.proxy_pending.contains_key(conn_id)
    }

    #[cfg(test)]
    pub(crate) async fn handle_msg_from_p2p_for_test(self: &Arc<Self>, msg: BinaryMessage) {
        let Some(multi) = self.multi_session() else {
            panic!("expected live MultiSession");
        };
        let p2p = multi.p2p();
        self.handle_msg_from_p2p_session_for_test(msg, p2p).await;
    }

    #[cfg(test)]
    pub(crate) async fn handle_msg_from_p2p_session_for_test(
        self: &Arc<Self>,
        msg: BinaryMessage,
        p2p: Option<Arc<tp_transport::session::Session>>,
    ) {
        let Some(multi) = self.multi_session() else {
            panic!("expected live MultiSession");
        };
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();
        self.handle_msg(
            msg,
            &multi,
            &multi.inbound(),
            &multi.udp_inbound(),
            &host_filter,
            &tracker,
            p2p,
            None,
        )
        .await;
    }

    #[cfg(test)]
    pub(crate) async fn handle_proxy_connect_response_for_test(
        self: &Arc<Self>,
        msg: BinaryMessage,
    ) {
        let Some(multi) = self.multi_session() else {
            panic!("expected live MultiSession");
        };
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();
        self.handle_msg(
            msg,
            &multi,
            &multi.inbound(),
            &multi.udp_inbound(),
            &host_filter,
            &tracker,
            None,
            None,
        )
        .await;
    }

    fn set_group_context(&self, context: Arc<TunnelGroupContext>) {
        *self.group_context.lock() = Some(context);
    }

    fn close_live_p2p_session_and_clear_group_context(&self) {
        let anchor = self.multi.lock().clone();
        if let Some(multi) = anchor.as_ref() {
            multi.close_all_p2p_without_lane_change_notification();
        }
        let replicas: Vec<Arc<crate::p2p::session::MultiSession>> = self
            .replica_sessions
            .lock()
            .iter()
            .map(|entry| entry.multi.clone())
            .collect();
        for multi in replicas {
            multi.close_all_p2p_without_lane_change_notification();
        }
        *self.group_context.lock() = None;
    }

    /// Build an installer handle that can attach direct P2P sessions to this
    /// engine's live replica.
    pub fn attach_p2p_session_installer(
        self: &Arc<Self>,
    ) -> crate::p2p::installer::P2pSessionInstaller {
        self.attach_p2p_session_installer_with_cancel(self.task_cancel_token())
    }

    pub fn attach_p2p_session_installer_with_cancel(
        self: &Arc<Self>,
        cancel: CancellationToken,
    ) -> crate::p2p::installer::P2pSessionInstaller {
        crate::p2p::installer::P2pSessionInstaller::new(self.clone(), cancel)
    }

    pub(crate) async fn install_p2p_session(
        self: &Arc<Self>,
        session_id: tp_core::p2p_types::SessionId,
        session: tp_transport::session::Session,
        task_cancel: CancellationToken,
    ) -> anyhow::Result<crate::p2p::installer::P2pInstalledSession> {
        self.install_p2p_session_inner(session_id, session, task_cancel, false)
            .await
    }

    pub(crate) async fn install_reserved_p2p_session(
        self: &Arc<Self>,
        session_id: tp_core::p2p_types::SessionId,
        session: tp_transport::session::Session,
        task_cancel: CancellationToken,
    ) -> anyhow::Result<crate::p2p::installer::P2pInstalledSession> {
        self.install_p2p_session_inner(session_id, session, task_cancel, true)
            .await
    }

    async fn install_p2p_session_inner(
        self: &Arc<Self>,
        session_id: tp_core::p2p_types::SessionId,
        session: tp_transport::session::Session,
        task_cancel: CancellationToken,
        require_reservation: bool,
    ) -> anyhow::Result<crate::p2p::installer::P2pInstalledSession> {
        if task_cancel.is_cancelled() {
            session.close();
            self.unreserve_p2p_session_install(session_id);
            anyhow::bail!("P2P install cancelled");
        }
        let registry_guard = self.p2p_session_registry_lock.lock();
        let pending = self
            .p2p_pending_installs
            .lock()
            .remove(&session_id)
            .or_else(|| {
                (!require_reservation)
                    .then(|| self.p2p_relay_context())
                    .flatten()
                    .map(|(_, _, multi)| PendingP2pInstall {
                        multi,
                        peer_client_id: format!("{session_id:?}"),
                        relation_key: None,
                        refill_permit: None,
                    })
            });
        let Some(pending) = pending else {
            session.close();
            if require_reservation {
                anyhow::bail!("no live reservation for P2P install");
            }
            anyhow::bail!("no live MultiSession for P2P install");
        };
        let multi = pending.multi;
        let peer_client_id = pending.peer_client_id;
        let relation_key = pending.relation_key;
        let _refill_permit = pending.refill_permit;
        let (sender, mut receiver, datagram_receiver) = session.split();
        let control_receiver = receiver.take_control_receiver();
        let tcp_flow_receiver = receiver.take_tcp_flow_receiver();
        let send_shell = Arc::new(tp_transport::session::Session::send_only_from_sender(
            sender,
        ));
        let inbound = multi.inbound();
        let udp_inbound = multi.udp_inbound();

        {
            let target_is_anchor = self
                .multi
                .lock()
                .as_ref()
                .map(|live_multi| Arc::ptr_eq(live_multi, &multi))
                .unwrap_or(false);
            let target_is_replica = self
                .replica_sessions
                .lock()
                .iter()
                .any(|entry| Arc::ptr_eq(&entry.multi, &multi));
            let target_is_live = target_is_anchor || target_is_replica;
            if !target_is_live {
                send_shell.close();
                anyhow::bail!("stale MultiSession for P2P install");
            }
            if task_cancel.is_cancelled() {
                send_shell.close();
                anyhow::bail!("P2P install cancelled");
            }
            let Some(group_context) = self.group_context.lock().clone() else {
                send_shell.close();
                anyhow::bail!("no live group context/access policy for proxy session");
            };

            let tracker = self.tasks.read().clone();
            let local_client_id = self
                .local_client_id_for_multi(&multi)
                .or_else(|| group_context.anchor_client_id.clone())
                .unwrap_or_else(|| group_context.group_id.clone());
            // Direct eligibility and the derived V2 Mesh/Export view form one
            // commit. The registry lock is already held, so this preserves the
            // global registry -> V2 reconciliation lock order used by retire
            // and disconnect paths.
            let reconcile_guard = self.v2_runtime_reconcile_lock.lock();
            multi.install_p2p_session_for_relation(
                session_id,
                peer_client_id.clone(),
                send_shell.clone(),
                relation_key,
            )?;
            if task_cancel.is_cancelled() {
                multi.close_p2p_session_for_handle_without_lane_change_notification(&send_shell);
                send_shell.close();
                anyhow::bail!("P2P install cancelled");
            }
            multi.set_state(crate::p2p::session::P2pState::Active {
                session_id,
                since: std::time::Instant::now(),
            });
            if task_cancel.is_cancelled() {
                multi.close_p2p_session_for_handle_without_lane_change_notification(&send_shell);
                send_shell.close();
                anyhow::bail!("P2P install cancelled");
            }
            if self.active_v2_profile.read().is_some() {
                self.ensure_v2_runtime_peer(&peer_client_id);
                self.reconcile_v2_routes_and_runtime_locked();
            }
            drop(reconcile_guard);
            tracing::debug!(
                ?session_id,
                peer_client_id = %peer_client_id,
                peer = %send_shell.peer_addr(),
                "P2P direct QUIC session installed"
            );
            drop(registry_guard);

            let stream_host_filter = group_context.host_filter.clone();
            let stream_engine = self.clone();
            let stream_multi = multi.clone();
            let stream_shell = send_shell.clone();
            let stream_tracker = tracker.clone();
            let heartbeat_client_id = group_context
                .anchor_client_id
                .clone()
                .unwrap_or_else(|| group_context.group_id.clone());
            let heartbeat_shell = send_shell.clone();
            let heartbeat_multi = multi.clone();
            let heartbeat_engine = self.clone();
            let heartbeat_peer_client_id = peer_client_id.clone();
            let established_ms = monotonic_millis();
            let p2p_last_ack_ms = Arc::new(AtomicU64::new(established_ms));
            let p2p_last_link_progress_ms = Arc::new(AtomicU64::new(established_ms));
            let p2p_active_flows = Arc::new(LinkActiveFlows::default());
            let p2p_active_counters = LinkActiveFlowCounters::with_source(
                p2p_active_flows.clone(),
                self.p2p_source_active_flow_counter(
                    local_client_id,
                    session_id,
                    peer_client_id.clone(),
                ),
            );
            let p2p_watchdog_config = LinkWatchdogConfig::production();
            let heartbeat_cancel = task_cancel.clone();
            tracker.spawn(async move {
                let mut tick = interval(p2p_watchdog_config.heartbeat_interval);
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = heartbeat_cancel.cancelled() => break,
                        _ = tick.tick() => {}
                    }
                    let ts = unix_timestamp_secs();
                    let send_started_ms = monotonic_millis();
                    let heartbeat = BinaryMessage::Heartbeat {
                        client_id: heartbeat_client_id.clone(),
                        timestamp: ts,
                    };
                    let send_result = tokio::select! {
                        _ = heartbeat_cancel.cancelled() => break,
                        result = heartbeat_shell.send(heartbeat) => result,
                    };
                    let heartbeat_send_elapsed_ms =
                        monotonic_millis().saturating_sub(send_started_ms);
                    if heartbeat_send_elapsed_ms >= 500 {
                        tracing::warn!(
                            ?session_id,
                            link_kind = "p2p",
                            peer = %heartbeat_peer_client_id,
                            ts,
                            heartbeat_send_elapsed_ms,
                            "P2P link heartbeat send was delayed"
                        );
                    }
                    if send_result.is_err() {
                        let p2p_session_id =
                            heartbeat_multi.p2p_session_id_for_handle(&heartbeat_shell);
                        if heartbeat_multi.close_p2p_session_for_handle(&heartbeat_shell) {
                            heartbeat_multi.report_p2p_to_relay_migration_with_context(
                                "heartbeat_send_failed",
                                None,
                                None,
                                p2p_session_id,
                            );
                            tracing::warn!(
                                ?session_id,
                                p2p_state = ?heartbeat_multi.p2p_state(),
                                "P2P heartbeat send failed; clearing installed session"
                            );
                        }
                        if let Some(session_id) = p2p_session_id {
                            heartbeat_engine.notify_p2p_relation_closed(session_id);
                        }
                        break;
                    }
                }
            });
            tracker.spawn(run_p2p_link_watchdog(
                self.clone(),
                multi.clone(),
                send_shell.clone(),
                p2p_last_ack_ms.clone(),
                p2p_last_link_progress_ms.clone(),
                peer_client_id.clone(),
                p2p_active_counters,
                multi.local_traffic(),
                LinkWatchdogConfig::production(),
                task_cancel.clone(),
            ));
            if let Some(mut flow_rx) = tcp_flow_receiver {
                let flow_host_filter = group_context.host_filter.clone();
                let flow_engine = self.clone();
                let flow_multi = multi.clone();
                let flow_shell = send_shell.clone();
                let flow_tracker = tracker.clone();
                let per_flow_tracker = tracker.clone();
                let flow_link_progress = p2p_last_link_progress_ms.clone();
                let flow_active_flows = p2p_active_flows.clone();
                let flow_cancel = task_cancel.clone();
                let per_flow_cancel = task_cancel.clone();
                flow_tracker.spawn(async move {
                    loop {
                        let incoming = tokio::select! {
                            _ = flow_cancel.cancelled() => break,
                            maybe = flow_rx.recv() => match maybe {
                                Some(incoming) => incoming,
                                None => break,
                            },
                        };
                        flow_link_progress.store(monotonic_millis(), Ordering::Relaxed);
                        let active_flow = flow_active_flows.begin("tcp", &incoming.preface.conn_id);
                        let engine = flow_engine.clone();
                        let multi = flow_multi.clone();
                        let p2p_source_session = flow_shell.clone();
                        let host_filter = flow_host_filter.clone();
                        let link_progress = flow_link_progress.clone();
                        let cancel = per_flow_cancel.clone();
                        per_flow_tracker.spawn(async move {
                            tokio::select! {
                                _ = cancel.cancelled() => {}
                                _ = engine.handle_tcp_flow_stream(
                                    incoming,
                                    multi,
                                    host_filter,
                                    TrafficPath::P2p,
                                    TcpFlowLinkContext {
                                        p2p_source_session: Some(p2p_source_session),
                                        link_progress_ms: Some(link_progress),
                                        link_active_flow: active_flow,
                                    },
                                ) => {}
                            }
                        });
                    }
                });
            }
            if let Some(mut control_rx) = control_receiver {
                let control_host_filter = group_context.host_filter.clone();
                let control_engine = self.clone();
                let control_multi = multi.clone();
                let control_inbound = multi.inbound();
                let control_udp_inbound = multi.udp_inbound();
                let control_tracker = tracker.clone();
                let control_shell = send_shell.clone();
                let control_liveness = LinkLivenessState::p2p(
                    p2p_last_ack_ms.clone(),
                    p2p_last_link_progress_ms.clone(),
                    p2p_active_flows.clone(),
                );
                let control_cancel = task_cancel.clone();
                tracker.spawn(async move {
                    loop {
                        let msg = tokio::select! {
                            _ = control_cancel.cancelled() => break,
                            maybe = control_rx.recv() => match maybe {
                                Some(msg) => msg,
                                None => break,
                            },
                        };
                        tokio::select! {
                            _ = control_cancel.cancelled() => break,
                            _ = control_engine.handle_msg(
                                msg,
                                &control_multi,
                                &control_inbound,
                                &control_udp_inbound,
                                &control_host_filter,
                                &control_tracker,
                                Some(control_shell.clone()),
                                Some(control_liveness.clone()),
                            ) => {}
                        }
                    }
                });
            }
            let stream_liveness = LinkLivenessState::p2p(
                p2p_last_ack_ms.clone(),
                p2p_last_link_progress_ms.clone(),
                p2p_active_flows.clone(),
            );
            let stream_cancel = task_cancel.clone();
            tracker.spawn(async move {
                loop {
                    let msg = tokio::select! {
                        _ = stream_cancel.cancelled() => break,
                        maybe = receiver.recv_data() => match maybe {
                            Some(msg) => msg,
                            None => break,
                        },
                    };
                    tokio::select! {
                        _ = stream_cancel.cancelled() => break,
                        _ = stream_engine.handle_msg(
                            msg,
                            &stream_multi,
                            &inbound,
                            &udp_inbound,
                            &stream_host_filter,
                            &stream_tracker,
                            Some(stream_shell.clone()),
                            Some(stream_liveness.clone()),
                        ) => {}
                    }
                }
                let p2p_session_id = stream_multi.p2p_session_id_for_handle(&stream_shell);
                if stream_multi.close_p2p_session_for_handle(&stream_shell) {
                    stream_multi.report_p2p_to_relay_migration_with_context(
                        "stream_reader_ended",
                        None,
                        None,
                        p2p_session_id,
                    );
                    tracing::warn!(
                        ?session_id,
                        p2p_state = ?stream_multi.p2p_state(),
                        "P2P stream reader ended; clearing installed session"
                    );
                    stream_shell.close();
                } else {
                    tracing::debug!(
                        ?session_id,
                        p2p_state = ?stream_multi.p2p_state(),
                        "P2P stream reader ended after session was replaced or cleared"
                    );
                }
                // Notify by exact generation even when another path won the
                // conditional close first (for example a send error). The
                // manager treats duplicate/stale session ids idempotently.
                stream_engine.notify_p2p_relation_closed(session_id);
            });

            if let Some(mut dg_rx) = datagram_receiver {
                let datagram_host_filter = group_context.host_filter.clone();
                let dg_engine = self.clone();
                let dg_multi = multi.clone();
                let dg_inbound = multi.inbound();
                let dg_udp_inbound = multi.udp_inbound();
                let dg_tracker = tracker.clone();
                let dg_shell = send_shell.clone();
                let dg_liveness = LinkLivenessState::p2p(
                    p2p_last_ack_ms,
                    p2p_last_link_progress_ms,
                    p2p_active_flows,
                );
                let dg_cancel = task_cancel.clone();
                tracker.spawn(async move {
                    loop {
                        let msg = tokio::select! {
                            _ = dg_cancel.cancelled() => break,
                            maybe = dg_rx.recv() => match maybe {
                                Some(msg) => msg,
                                None => break,
                            },
                        };
                        tokio::select! {
                            _ = dg_cancel.cancelled() => break,
                            _ = dg_engine.handle_msg(
                                msg,
                                &dg_multi,
                                &dg_inbound,
                                &dg_udp_inbound,
                                &datagram_host_filter,
                                &dg_tracker,
                                Some(dg_shell.clone()),
                                Some(dg_liveness.clone()),
                            ) => {}
                        }
                    }
                });
            }
        }

        Ok(crate::p2p::installer::P2pInstalledSession::new(
            multi, send_shell,
        ))
    }

    /// Snapshot the live relay [`Session`] handle (the send-only shell
    /// installed on `MultiSession`). `None` until a replica is up.
    pub fn relay_session(&self) -> Option<Arc<tp_transport::session::Session>> {
        self.multi.lock().as_ref().map(|m| m.relay().clone())
    }

    /// Wire a P2P signaling channel pair into the engine.
    ///
    /// `in_tx` receives P2P-typed `BinaryMessage`s parsed from the relay
    /// stream — `P2pManager` consumes via its `inbound` rx half. `out_rx`
    /// is the manager's outbound; this method spawns a long-lived
    /// forwarder that drains it onto whatever relay is currently live.
    ///
    /// The forwarder is single, long-lived, and re-resolves
    /// `self.multi` on every message so it survives replica reconnects.
    /// While `multi` is `None` (between replicas) outbound messages are
    /// dropped — the manager's spec retries (cooldown / timeout) cover
    /// the gap. Pre-fix the forwarder was bound to one replica's relay
    /// and silently stopped working on reconnect.
    ///
    /// The forwarder exits only when the manager drops `out_tx` (its
    /// half closes), i.e. when `P2pManager` itself is dropped.
    pub fn attach_p2p_signaling(
        self: &Arc<Self>,
        in_tx: mpsc::Sender<tp_core::protocol::BinaryMessage>,
        mut out_rx: mpsc::Receiver<tp_core::protocol::BinaryMessage>,
    ) {
        let (ingress_tx, mut ingress_rx) =
            mpsc::channel::<P2pSignalingIngressItem>(P2P_SIGNALING_INGRESS_BROKER_CAPACITY);
        {
            let _registry_guard = self.p2p_session_registry_lock.lock();
            self.p2p_pending_membership_batches.lock().clear();
            self.p2p_delivered_membership_authorities.lock().clear();
            *self.p2p_signaling_ingress_tx.lock() = Some(ingress_tx);
        }

        let broker_engine = self.clone();
        let broker_cancel = self.task_cancel_token();
        self.spawn_engine_task(async move {
            let mut next_membership_delivery_sequence = 0_u64;
            loop {
                let item = tokio::select! {
                    _ = broker_cancel.cancelled() => break,
                    item = ingress_rx.recv() => match item {
                        Some(item) => item,
                        None => break,
                    },
                };
                match item {
                    P2pSignalingIngressItem::Single { message, relay } => {
                        let session_id = p2p_signaling_session_id(&message);
                        let delivered = tokio::select! {
                            _ = broker_cancel.cancelled() => false,
                            result = in_tx.send(message) => result.is_ok(),
                        };
                        if !delivered {
                            if let Some(session_id) = session_id {
                                broker_engine
                                    .remove_p2p_signaling_route_for_multi(session_id, &relay);
                            }
                            break;
                        }
                    }
                    P2pSignalingIngressItem::MembershipBatch {
                        mut messages,
                        authority,
                        v2_authority_required,
                    } => {
                        let Some(ack) = messages.pop() else {
                            continue;
                        };
                        for message in messages {
                            let delivered = tokio::select! {
                                _ = broker_cancel.cancelled() => false,
                                result = in_tx.send(message) => result.is_ok(),
                            };
                            if !delivered {
                                return;
                            }
                        }
                        let delivered_authority = v2_authority_required.then(|| {
                            next_membership_delivery_sequence =
                                next_membership_delivery_sequence.saturating_add(1);
                            let delivered = DeliveredP2pMembershipAuthority {
                                delivery_sequence: next_membership_delivery_sequence,
                                source: authority,
                            };
                            broker_engine
                                .p2p_delivered_membership_authorities
                                .lock()
                                .push_back(delivered.clone());
                            delivered
                        });
                        let delivered = tokio::select! {
                            _ = broker_cancel.cancelled() => false,
                            result = in_tx.send(ack) => result.is_ok(),
                        };
                        if !delivered {
                            if let Some(authority) = delivered_authority {
                                let mut pending =
                                    broker_engine.p2p_delivered_membership_authorities.lock();
                                if pending.back().is_some_and(|tail| {
                                    tail.delivery_sequence == authority.delivery_sequence
                                }) {
                                    pending.pop_back();
                                }
                            }
                            return;
                        }
                    }
                }
            }
            tracing::debug!("P2P signaling ingress broker exited");
        });

        let me = self.clone();
        // Forwarder lifetime matches the engine (it survives replica
        // reconnects), so register under the engine-lifetime tracker
        // — `disconnect()` will drop the manager's `out_tx`, the loop exits,
        // and the drain unblocks.
        self.spawn_engine_task(async move {
            while let Some(msg) = out_rx.recv().await {
                let session_id = p2p_signaling_session_id(&msg);
                let exact_local_client_id = match &msg {
                    BinaryMessage::P2pOffer { src_client_id, .. } => Some(src_client_id.as_str()),
                    BinaryMessage::P2pAnswer {
                        accepted_client_id, ..
                    } => Some(accepted_client_id.as_str()),
                    _ => None,
                };
                let route_multi = session_id.and_then(|session_id| {
                    me.p2p_signaling_routes
                        .get(&session_id)
                        .map(|entry| entry.value().clone())
                });
                let relay_context = if let Some(exact_local_client_id) = exact_local_client_id {
                    // Offer/Answer carry the local Replica identity that the
                    // Gateway authenticates against this relay connection.
                    // If that exact relay vanished after enqueue, fail closed:
                    // sending through another Replica recreates a different
                    // canonical relation on the Gateway side.
                    me.exact_live_p2p_relay_context(exact_local_client_id)
                        .map(|(client_id, multi)| (client_id, multi.clone(), multi.relay().clone()))
                } else {
                    route_multi
                        .as_ref()
                        .and_then(|multi| {
                            me.local_client_id_for_multi(multi)
                                .map(|client_id| (client_id, multi.clone(), multi.relay().clone()))
                        })
                        .or_else(|| {
                            me.p2p_relay_context().map(|(client_id, _, multi)| {
                                (client_id, multi.clone(), multi.relay().clone())
                            })
                        })
                };
                let Some((relay_client_id, relay_multi, relay)) = relay_context else {
                    tracing::debug!("P2P signaling forwarder: no live multi; dropping outbound");
                    continue;
                };
                let remove_route = matches!(msg, BinaryMessage::P2pTeardown { .. });
                if let Err(e) = relay.send(msg).await {
                    // Relay closed mid-replica; drop and loop. The next
                    // iteration re-resolves `self.multi` and will pick up
                    // the new replica's relay once `run_replica` installs
                    // it.
                    //
                    // Yield between the writer-task drop and the
                    // run_replica `*self.multi = None` so the bounded
                    // out_rx queue (cap 64) doesn't tight-spin through
                    // its backlog burning a CPU during the brief
                    // closed-but-not-yet-cleared window.
                    tracing::warn!(
                        error = %e,
                        "relay send (P2P signaling) failed; will retry next replica"
                    );
                    if matches!(e, tp_transport::TransportError::Closed) {
                        me.unregister_relay_closed_multi_session(&relay_client_id, &relay_multi);
                    }
                    if let (Some(session_id), Some(route_multi)) =
                        (session_id, route_multi.as_ref())
                    {
                        me.remove_p2p_signaling_route_for_multi(session_id, route_multi);
                    }
                    tokio::task::yield_now().await;
                    continue;
                }
                if remove_route {
                    if let Some(session_id) = session_id {
                        if let Some(route_multi) = route_multi.as_ref() {
                            me.remove_p2p_signaling_route_for_multi(session_id, route_multi);
                        } else {
                            me.p2p_signaling_routes.remove(&session_id);
                        }
                    }
                }
            }
            tracing::debug!("P2P signaling forwarder exited (manager out_tx closed)");
        });
    }

    async fn forward_p2p_signaling_from_relay(
        &self,
        msg: BinaryMessage,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        let (item, session_id, remove_route) = if matches!(&msg, BinaryMessage::P2pPeerHint { .. })
        {
            let _registry_guard = self.p2p_session_registry_lock.lock();
            if !self.p2p_relay_multi_is_active_locked(multi) {
                tracing::debug!("P2P membership hint from inactive relay; dropping");
                return;
            }
            let mut pending_batches = self.p2p_pending_membership_batches.lock();
            let pending = pending_batches
                .entry(p2p_relay_instance_key(multi))
                .or_default();
            if !pending.overflowed {
                if pending.hints.len() < P2P_MEMBERSHIP_BATCH_MAX_HINTS {
                    pending.hints.push(msg);
                } else {
                    pending.hints = Vec::new();
                    pending.overflowed = true;
                }
            }
            return;
        } else if matches!(&msg, BinaryMessage::P2pAnnounceAck { .. }) {
            let _registry_guard = self.p2p_session_registry_lock.lock();
            let Some(authority) = self.p2p_membership_authority_for_multi_locked(multi) else {
                tracing::debug!("P2P membership Ack from inactive relay; dropping");
                return;
            };
            let pending = self
                .p2p_pending_membership_batches
                .lock()
                .remove(&p2p_relay_instance_key(multi))
                .unwrap_or_default();
            if pending.overflowed {
                tracing::warn!(
                    max_hints = P2P_MEMBERSHIP_BATCH_MAX_HINTS,
                    "P2P membership cycle exceeded Hint limit; dropping whole cycle"
                );
                return;
            }
            let mut messages = pending.hints;
            messages.push(msg);
            (
                P2pSignalingIngressItem::MembershipBatch {
                    messages,
                    authority,
                    v2_authority_required: self.active_v2_peer_profile().is_some(),
                },
                None,
                false,
            )
        } else {
            let remove_route = matches!(msg, BinaryMessage::P2pTeardown { .. });
            let session_id = p2p_signaling_session_id(&msg);
            if let Some(session_id) = session_id {
                if !self.insert_p2p_signaling_route_from_relay(session_id, multi) {
                    tracing::debug!(
                        ?session_id,
                        "P2P signaling inbound from inactive relay; dropping"
                    );
                    return;
                }
            } else if !self.p2p_relay_multi_is_active(multi) {
                tracing::debug!("P2P signaling inbound from inactive relay; dropping");
                return;
            }
            (
                P2pSignalingIngressItem::Single {
                    message: msg,
                    relay: multi.clone(),
                },
                session_id,
                remove_route,
            )
        };

        let tx = self.p2p_signaling_ingress_tx.lock().clone();
        let delivered = if let Some(tx) = tx {
            match tx.try_send(item) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(item)) => {
                    tracing::warn!(
                        ?session_id,
                        membership_batch =
                            matches!(item, P2pSignalingIngressItem::MembershipBatch { .. }),
                        "P2P signaling ingress broker full; dropping whole item"
                    );
                    false
                }
                Err(mpsc::error::TrySendError::Closed(item)) => {
                    tracing::debug!(
                        ?session_id,
                        membership_batch =
                            matches!(item, P2pSignalingIngressItem::MembershipBatch { .. }),
                        "P2P signaling ingress broker closed; dropping whole item"
                    );
                    false
                }
            }
        } else {
            false
        };
        if remove_route || !delivered {
            if let Some(session_id) = session_id {
                self.remove_p2p_signaling_route_for_multi(session_id, multi);
            }
        }
    }

    #[cfg(test)]
    async fn forward_p2p_signaling_from_relay_for_test(
        &self,
        msg: BinaryMessage,
        multi: &Arc<crate::p2p::session::MultiSession>,
    ) {
        self.forward_p2p_signaling_from_relay(msg, multi).await;
    }

    /// Install the shared expected-peer-fingerprint slot for the QUIC
    /// listener (Task 4.8). Stored here so `apps/lantunnel-client/src-tauri/src/main.rs`
    /// (Task 4.11) can hand the same handle to `P2pManager` and the
    /// listener.
    pub fn set_p2p_expected_fp_handle(
        &self,
        handle: Arc<std::sync::Mutex<Option<tp_core::p2p_types::CertFingerprint>>>,
    ) {
        *self.p2p_expected_fp.lock() = Some(handle);
    }

    /// Read-back accessor for the slot installed via
    /// [`Engine::set_p2p_expected_fp_handle`].
    pub fn p2p_expected_fp_handle(
        &self,
    ) -> Option<Arc<std::sync::Mutex<Option<tp_core::p2p_types::CertFingerprint>>>> {
        self.p2p_expected_fp.lock().clone()
    }

    /// Replica fanout resolved for the active V2 Gateway Attachment.
    /// `None` until a connect is in progress.
    pub fn replicas(&self) -> Option<usize> {
        *self.replicas.lock()
    }

    /// Snapshot the current `(client_id, group_id)` resolved from the
    /// platform after the live replica connected. `None` until the first
    /// replica's `run_replica` finishes the dial; cleared on teardown.
    /// Consumed by `apps/lantunnel-client/src-tauri/src/main.rs` (Task 4.11) to construct the
    /// P2P manager's announce/offer payloads from the same identity the
    /// gateway sees on the relay.
    pub fn tunnel_identity(&self) -> Option<(String, String)> {
        self.tunnel_identity.lock().clone()
    }

    /// Install or clear the P2P metrics sink (Task 4.12). Wired by
    /// `apps/lantunnel-client/src-tauri/src/main.rs` when P2P is enabled. The handle is also
    /// installed onto the live `MultiSession` so the `pick`/migration
    /// emitters can find it; future replicas pick it up too.
    pub fn set_metrics(&self, metrics: Option<Arc<tp_metrics::MetricsManager>>) {
        *self.metrics.lock() = metrics.clone();
        if let Some(m) = self.multi.lock().clone() {
            m.set_metrics(metrics.clone());
        }
        for replica in self.replica_sessions.lock().iter() {
            replica.multi.set_metrics(metrics.clone());
        }
    }

    /// Read-back accessor for the metrics sink installed via
    /// [`Engine::set_metrics`]. Cheap (mutex-guarded `Arc` clone).
    pub fn metrics(&self) -> Option<Arc<tp_metrics::MetricsManager>> {
        self.metrics.lock().clone()
    }

    /// Install the P2P tuning config so the next `run_replica` can
    /// build the `MultiSession`'s scheduler from `min_advantage` /
    /// `stable_cycles`. Stamping after the replica is already up has no
    /// retroactive effect (the scheduler is constructed once per replica
    /// and never swapped — concurrent swap with `pick_kind` would race);
    /// callers are expected to install before `connect`.
    pub fn set_p2p_config(&self, cfg: Arc<tp_core::config::ClientP2pConfig>) {
        *self.p2p_config.lock() = Some(cfg);
        if !self.p2p_config().allow_lan_route_aliases {
            let _ = self.overlay_routes.write().replace_lan_alias_snapshot(&[]);
        }
    }

    /// Compile and install the explicit target-side local delivery policy.
    /// Call before `connect`; changing it during a live generation is left to
    /// product settings code to reject so Bind/Connect authorization remains
    /// generation-stable.
    pub fn set_local_service_exports(
        &self,
        configs: &[tp_core::config::LocalServiceExportConfig],
    ) -> Result<(), crate::local_target::LocalServiceExportError> {
        let exports = crate::local_target::compile_local_service_exports(configs)?;
        *self.local_service_exports.write() = exports;
        Ok(())
    }

    pub fn set_v2_access_policy(
        &self,
        policy: &crate::access_policy::ClientAccessPolicyV2,
    ) -> Result<(), crate::access_policy::ClientAccessPolicyErrorV2> {
        let compiled = crate::access_policy::CompiledClientAccessPolicyV2::compile(policy)?;
        *self.v2_access_policy.write() = compiled;
        Ok(())
    }

    /// Replace the one locally originated Runtime record and immediately push
    /// the full current value to every Ready PeerLink. Validation is performed
    /// by `PeerRuntimeRecordV2::new` before callers reach this method.
    pub fn set_v2_local_runtime_record(
        &self,
        record: crate::peer_runtime::PeerRuntimeRecordV2,
    ) -> Result<(), crate::peer_runtime::PeerRuntimeErrorV2> {
        let record = crate::peer_runtime::PeerRuntimeRecordV2::new(record.lan_exports)?;
        // The watchdog re-derives the published record from the Export config
        // on every scan, so a caller that installed a record without one would
        // have these Exports withdrawn on the next tick.
        *self.v2_local_lan_export_config.write() = crate::peer_runtime::LocalLanExportConfigV2 {
            configured: record
                .lan_exports
                .iter()
                .map(|export| export.prefix)
                .collect(),
            auto_current_lan: false,
        };
        self.v2_local_lan_export_generation
            .fetch_add(1, Ordering::AcqRel);
        self.publish_v2_local_runtime_record(record);
        Ok(())
    }

    /// Install the owner's LAN Export answer — the prefixes they typed, plus
    /// whether the networks this machine is attached to are exported without
    /// being named — and publish it against the snapshot the caller took.
    pub fn set_v2_local_lan_export_config(
        &self,
        config: crate::peer_runtime::LocalLanExportConfigV2,
        connected_lans: Option<&[crate::peer_runtime::LanExportPrefixV2]>,
    ) {
        let record = {
            let mut current = self.v2_local_lan_export_config.write();
            *current = config;
            current.resolve(connected_lans)
        };
        self.v2_local_lan_export_generation
            .fetch_add(1, Ordering::AcqRel);
        self.publish_v2_local_runtime_record(record);
    }

    /// The current LAN Export answer's generation, taken before a blocking
    /// interface scan and handed back to the refresh that follows it.
    fn v2_local_lan_export_generation(&self) -> u64 {
        self.v2_local_lan_export_generation.load(Ordering::Acquire)
    }

    /// Replace the published record and push the full current value to every
    /// Ready PeerLink.
    fn publish_v2_local_runtime_record(&self, record: crate::peer_runtime::PeerRuntimeRecordV2) {
        let mut current = self.v2_local_runtime_record.write();
        *current = record.clone();
        drop(current);
        self.v2_runtime.write().local_exports = Self::v2_local_export_snapshots(&record);
        let outbound = self
            .v2_peer_gossip
            .lock()
            .as_mut()
            .map(|gossip| gossip.set_local_record(record))
            .unwrap_or_default();
        for message in outbound {
            self.send_v2_gossip_outbound(message);
        }
    }

    /// Re-resolve the local Exports against a fresh interface snapshot.
    ///
    /// This is what makes an automatic Export follow the machine: the set of
    /// prefixes is derived here, not carried over from the last save.
    fn refresh_v2_local_lan_exports_from_snapshot(
        &self,
        connected_lans: Option<&[crate::peer_runtime::LanExportPrefixV2]>,
        scanned_generation: u64,
    ) -> bool {
        // A settings save may race the blocking interface scan. Publishing the
        // new answer against the pre-save snapshot would flap readiness for a
        // whole watchdog interval; that save already published its own
        // resolution, and the next pass scans again.
        if self.v2_local_lan_export_generation() != scanned_generation {
            return false;
        }
        let next = self
            .v2_local_lan_export_config
            .read()
            .resolve(connected_lans);
        if *self.v2_local_runtime_record.read() == next {
            return false;
        }
        self.publish_v2_local_runtime_record(next);
        // Reuse the existing product status event to make Settings refresh
        // this backend-owned fact without a second event/controller.
        self.listener.on_status(&self.status());
        true
    }

    fn start_v2_local_lan_export_watchdog(self: &Arc<Self>, cancel: CancellationToken) {
        let engine = Arc::clone(self);
        self.spawn_engine_task(async move {
            let mut ticker = interval(V2_LOCAL_LAN_EXPORT_WATCHDOG_INTERVAL);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = ticker.tick() => {}
                }
                let scanned_generation = engine.v2_local_lan_export_generation();
                let inventory = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    result = tokio::task::spawn_blocking(crate::native_route_guard::discover_connected_lan_prefixes) => {
                        result.ok().and_then(Result::ok)
                    }
                };
                if engine
                    .refresh_v2_local_lan_exports_from_snapshot(inventory.as_deref(), scanned_generation)
                {
                    tracing::debug!(
                        scanned = inventory.is_some(),
                        "local LAN Exports changed; pushed full Runtime record"
                    );
                }
            }
        });
    }

    /// Snapshot the configured P2P tuning, falling back to defaults when
    /// no config has been installed (preserves the original hardcoded values).
    pub fn p2p_config(&self) -> Arc<tp_core::config::ClientP2pConfig> {
        self.p2p_config
            .lock()
            .clone()
            .unwrap_or_else(|| Arc::new(tp_core::config::ClientP2pConfig::default()))
    }

    /// Expose learned LAN aliases to the native TUN only after the current
    /// generation has inventoried local infrastructure routes. When LAN Link
    /// Candidates are enabled, their sockets must additionally be guaranteed
    /// to bypass the TUN. Explicit SOCKS matching remains available while
    /// either native-capture prerequisite is absent.
    fn native_lan_capture_exclusions(&self) -> Option<BTreeSet<std::net::Ipv4Addr>> {
        let config = self.p2p_config();
        let generation = self.native_lan_route_generation.read();
        (config.allow_lan_route_aliases
            && generation.inventory_ready
            && (!config.allow_lan_candidates || generation.bypass_ready))
            .then(|| generation.exclusions.clone())
    }

    /// Start a reconnect-scoped underlay generation and invalidate every
    /// earlier guard. Only this token may subsequently commit inventory,
    /// publish bypass readiness, or clear the generation.
    pub(crate) fn begin_p2p_underlay_generation(&self) -> P2pUnderlayGeneration {
        let mut generation = self.native_lan_route_generation.write();
        let epoch = generation.epoch.wrapping_add(1).max(1);
        *generation = NativeLanRouteGeneration {
            epoch,
            ..NativeLanRouteGeneration::default()
        };
        P2pUnderlayGeneration(epoch)
    }

    pub(crate) fn p2p_underlay_generation_is_ready(&self, token: P2pUnderlayGeneration) -> bool {
        let generation = self.native_lan_route_generation.read();
        generation.epoch == token.0 && generation.bypass_ready
    }

    fn invalidate_p2p_underlay_generation(&self) {
        let _ = self.begin_p2p_underlay_generation();
    }

    pub(crate) fn set_p2p_underlay_generation_ready(
        &self,
        token: P2pUnderlayGeneration,
        ready: bool,
    ) -> bool {
        let mut generation = self.native_lan_route_generation.write();
        if generation.epoch != token.0 {
            return false;
        }
        generation.bypass_ready = ready;
        if !ready {
            let epoch = generation.epoch;
            *generation = NativeLanRouteGeneration {
                epoch,
                ..NativeLanRouteGeneration::default()
            };
        }
        true
    }

    /// Atomically prepare the local-infrastructure exclusions for one P2P
    /// underlay generation. Callers must complete this before publishing the
    /// generation as bypass-ready.
    pub(crate) fn configure_native_lan_route_inventory(
        &self,
        token: P2pUnderlayGeneration,
        exclusions: BTreeSet<std::net::Ipv4Addr>,
        connected_lans: Vec<crate::peer_runtime::LanExportPrefixV2>,
    ) -> anyhow::Result<()> {
        if !self.commit_native_lan_route_generation(token, exclusions, connected_lans) {
            anyhow::bail!("P2P underlay generation was superseded");
        }
        Ok(())
    }

    fn commit_native_lan_route_generation(
        &self,
        token: P2pUnderlayGeneration,
        exclusions: BTreeSet<std::net::Ipv4Addr>,
        connected_lans: Vec<crate::peer_runtime::LanExportPrefixV2>,
    ) -> bool {
        let mut generation = self.native_lan_route_generation.write();
        if generation.epoch != token.0 {
            return false;
        }
        generation.exclusions = exclusions;
        generation.connected_lans = connected_lans;
        generation.inventory_ready = true;
        true
    }

    fn begin_local_lan_publication_generation(&self) -> u64 {
        let mut state = self.local_lan_publication.write();
        state.generation = state.generation.wrapping_add(1).max(1);
        state.hosts.clear();
        state.generation
    }

    fn invalidate_local_lan_publication_generation(&self) {
        let _ = self.begin_local_lan_publication_generation();
    }

    #[cfg(test)]
    fn apply_local_lan_route_publication_for_generation(
        &self,
        generation: u64,
        enabled: bool,
        discovered: std::io::Result<Vec<String>>,
    ) -> Option<Vec<String>> {
        let mut state = self.local_lan_publication.write();
        if state.generation != generation {
            return None;
        }
        if !enabled {
            state.hosts.clear();
            return Some(Vec::new());
        }

        let Ok(discovered) = discovered else {
            // Unknown current interface ownership must fail closed locally.
            state.hosts.clear();
            return None;
        };
        let parsed = discovered
            .into_iter()
            .map(|address| address.parse::<std::net::Ipv4Addr>())
            .collect::<Result<Vec<_>, _>>();
        let Ok(parsed) = parsed else {
            state.hosts.clear();
            return None;
        };
        let publication = crate::platform::normalize_local_lan_ipv4s(parsed);
        state.hosts = publication
            .iter()
            .filter_map(|address| address.parse().ok())
            .collect();
        Some(publication)
    }

    #[cfg(test)]
    fn refresh_local_lan_route_publication_from_discovery(
        &self,
        discovered: std::io::Result<Vec<String>>,
    ) -> Option<Vec<String>> {
        let generation = {
            let current = self.local_lan_publication.read().generation;
            if current == 0 {
                self.begin_local_lan_publication_generation()
            } else {
                current
            }
        };
        self.apply_local_lan_route_publication_for_generation(
            generation,
            self.publish_local_lan_routes(),
            discovered,
        )
    }

    #[cfg(test)]
    pub(crate) fn set_native_lan_route_exclusions_for_test(
        &self,
        exclusions: &[std::net::Ipv4Addr],
    ) {
        let mut generation = self.native_lan_route_generation.write();
        if generation.epoch == 0 {
            generation.epoch = 1;
        }
        generation.exclusions = exclusions.iter().copied().collect();
        generation.connected_lans.clear();
        generation.inventory_ready = true;
    }

    #[cfg(test)]
    fn set_native_v2_route_inventory_for_test(
        &self,
        connected_lans: &[crate::peer_runtime::LanExportPrefixV2],
        exclusions: &[std::net::Ipv4Addr],
    ) {
        let mut generation = self.native_lan_route_generation.write();
        generation.epoch = generation.epoch.max(1);
        generation.bypass_ready = true;
        generation.inventory_ready = true;
        generation.exclusions = exclusions.iter().copied().collect();
        generation.connected_lans = connected_lans.to_vec();
    }

    #[cfg(test)]
    fn commit_native_lan_route_inventory_for_test(&self, exclusions: &[std::net::Ipv4Addr]) {
        let token = {
            let mut generation = self.native_lan_route_generation.write();
            if generation.epoch == 0 {
                generation.epoch = 1;
            }
            P2pUnderlayGeneration(generation.epoch)
        };
        assert!(self.commit_native_lan_route_generation(
            token,
            exclusions.iter().copied().collect(),
            Vec::new(),
        ));
    }

    #[cfg(test)]
    pub(crate) fn set_p2p_underlay_bypass_ready(&self, ready: bool) {
        let token = {
            let mut generation = self.native_lan_route_generation.write();
            if generation.epoch == 0 {
                generation.epoch = 1;
            }
            P2pUnderlayGeneration(generation.epoch)
        };
        assert!(self.set_p2p_underlay_generation_ready(token, ready));
    }

    #[cfg(test)]
    pub(crate) fn p2p_underlay_bypass_ready_for_test(&self) -> bool {
        self.native_lan_route_generation.read().bypass_ready
    }

    #[cfg(test)]
    fn publish_local_lan_routes(&self) -> bool {
        self.p2p_config().allow_lan_route_aliases
    }

    /// Hand out a clone of the engine-lifetime [`TaskTracker`].
    ///
    /// Callers (apps + the P2P bootstrap helper) should register spawns
    /// whose lifetime should match the engine — i.e., outlive any single
    /// `connect()` cycle but should be drained on
    /// [`Engine::disconnect`] — through this tracker rather than bare
    /// `tokio::spawn`. The clone is cheap (`TaskTracker` is internally
    /// reference-counted). The tracker is replaced on every `disconnect`,
    /// so cache the clone only for the duration of one connect cycle.
    pub fn tasks(&self) -> TaskTracker {
        self.tasks.read().clone()
    }

    /// Clone the cancellation token for tasks registered with [`Engine::tasks`].
    ///
    /// Capture this immediately before spawning engine-lifetime work. The token
    /// is cancelled and replaced by [`Engine::disconnect`], so clones kept
    /// across connect cycles intentionally keep the old cancelled generation.
    pub fn task_cancel_token(&self) -> CancellationToken {
        self.task_cancel.read().clone()
    }

    fn spawn_engine_task<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let tasks = self.tasks.read();
        let handle = tasks.spawn(task);
        self.task_abort_handles.lock().push(handle.abort_handle());
    }

    fn replace_task_tracker_for_disconnect(&self) -> (TaskTracker, Vec<AbortHandle>) {
        let mut tasks_guard = self.tasks.write();
        let tracker = std::mem::replace(&mut *tasks_guard, TaskTracker::new());
        let abort_handles = std::mem::take(&mut *self.task_abort_handles.lock());
        (tracker, abort_handles)
    }
}

/// The overall state once the directory has settled.
///
/// Whether this device has a path is asked first. The old order asked "is any
/// Peer unavailable" before anything else, so a Tunnel whose other devices
/// happened to be switched off left this one reading "Blocked — no way to
/// reach this device yet" directly under a healthy Gateway attachment. Nothing
/// was wrong with this device; there was simply nobody else on, and that is
/// each Peer's own state in the directory, not this one's.
pub(crate) fn settled_overall_phase(
    gateway_phase: crate::runtime_snapshot::V2GatewayAttachmentPhase,
    any_direct: bool,
    any_unavailable: bool,
    any_usable: bool,
    has_peers: bool,
    gateway_reason: Option<crate::runtime_snapshot::V2RuntimeReasonCode>,
) -> (
    crate::runtime_snapshot::V2OverallPhase,
    Option<crate::runtime_snapshot::V2RuntimeReasonCode>,
) {
    use crate::runtime_snapshot::{V2GatewayAttachmentPhase, V2OverallPhase, V2RuntimeReasonCode};

    let reachable = gateway_phase == V2GatewayAttachmentPhase::Attached || any_direct;

    if reachable {
        // Attached and carrying traffic. A Peer that is off is reported on that
        // Peer's row; it does not make this device unreachable. Degraded is for
        // a Tunnel that is partly working: some Peers reachable, some not.
        return if any_unavailable && any_usable {
            (
                V2OverallPhase::Degraded,
                Some(V2RuntimeReasonCode::NoUsablePeerPath),
            )
        } else {
            (V2OverallPhase::Connected, None)
        };
    }

    if has_peers || any_unavailable {
        return (
            V2OverallPhase::Blocked,
            gateway_reason.or(Some(V2RuntimeReasonCode::GatewayUnavailable)),
        );
    }

    let phase = match gateway_phase {
        V2GatewayAttachmentPhase::ResolvingThroughPlatform
        | V2GatewayAttachmentPhase::ProvisioningScope
        | V2GatewayAttachmentPhase::Connecting => V2OverallPhase::WaitingForGateway,
        _ => V2OverallPhase::Blocked,
    };
    let reason = gateway_reason.unwrap_or(V2RuntimeReasonCode::GatewayUnavailable);
    (phase, Some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{ConnectionPathMode, NullListener};
    use async_trait::async_trait;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};
    use tp_core::provisioning::{
        GatewayBootstrapV2, PeerBootstrapV2, PeerProfileV2, TunnelOwnerFileV2,
    };
    use tp_gateway::{Gateway, GatewayServer};
    use tp_transport::Session;
    use tp_transport::{QuicServer, QuicTuning};

    #[tokio::test]
    async fn managed_all_carriers_require_an_exact_leaf_even_when_global_tls_is_insecure() {
        let engine = Engine::new(
            EngineConfig {
                insecure_tls: true,
                ..EngineConfig::default()
            },
            Arc::new(NullListener),
        );
        let candidates = vec![GatewayDialCandidate {
            gateway_addr: "203.0.113.88".into(),
            gateway_port: 8443,
            tls_server_name: Some("203.0.113.88".into()),
            force_tls: true,
        }];

        for transport in ["quic", "websocket", "grpc"] {
            let config = TunnelConfig {
                transport_type: transport.into(),
                gateway_addr: "203.0.113.88".into(),
                gateway_port: 8443,
                ..TunnelConfig::default()
            };
            let managed_error = match engine.build_replica_transport(&config, &candidates, true) {
                Ok(_) => panic!("Managed {transport} must not use insecure or generic CA TLS"),
                Err(error) => error.to_string(),
            };
            assert!(managed_error.contains("missing the exact leaf PEM"));
            assert!(
                engine
                    .build_replica_transport(&config, &candidates, false)
                    .is_ok(),
                "Static {transport} must retain its existing TLS configuration semantics"
            );
        }
    }

    #[test]
    fn active_v2_profile_enables_exact_routes_without_v1_peer_join_fields() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let remote = owner.add_peer(None, 1, None).expect("remote Peer");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(local.clone());
        *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
            &local,
            &gateway,
            vec![format!("{}-AbCd0001-0", local.tunnel_id)],
        ));

        assert!(engine.uses_exact_peer_routing());
        engine
            .install_v2_peer_membership(&remote.public_membership())
            .expect("install signed remote membership");
        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{}:27015", remote.peer.overlay_ip))
                .expect("resolve Overlay"),
            Some(remote.peer.peer_id)
        );
    }

    #[tokio::test]
    async fn source_hostname_route_keeps_the_original_name_and_one_literal_route_target() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let remote = owner.add_peer(None, 1, None).expect("remote Peer");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(local.clone());
        *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
            &local,
            &gateway,
            vec![format!("{}-local-0", local.tunnel_id)],
        ));
        engine.install_overlay_peer_for_test(&remote.peer.peer_id, std::net::Ipv4Addr::LOCALHOST);

        let route = engine
            .resolve_proxy_target_peer("localhost:27015")
            .await
            .expect("resolve hostname route");
        assert_eq!(route.peer_id, Some(remote.peer.peer_id));
        assert_eq!(
            route.logical_destination,
            Some(SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 27015)))
        );
    }

    #[tokio::test]
    async fn v2_peerlink_rehandshake_replaces_current_key_while_open_flow_keeps_old_key() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::peer_link_crypto::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};

        fn session_keys(
            source: &PeerProfileV2,
            target: &PeerProfileV2,
            session_id: SessionId,
        ) -> tp_core::peer_link_crypto::PeerLinkSessionKeysV2 {
            let source_secret = PeerLinkEphemeralSecretV2::generate();
            let target_secret = PeerLinkEphemeralSecretV2::generate();
            let offer = P2pOfferV2::sign(
                source,
                session_id,
                target.peer.peer_id.clone(),
                Vec::new(),
                CertFingerprint::from_bytes([0x41; 32]),
                &source_secret,
            )
            .expect("Offer");
            let answer = P2pAnswerV2::sign(
                target,
                &offer,
                true,
                0,
                Vec::new(),
                CertFingerprint::from_bytes([0x42; 32]),
                &target_secret,
            )
            .expect("Answer");
            source_secret
                .derive_session_keys(&offer, &answer, &source.tunnel_signing_public_key)
                .expect("source keys")
        }

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let remote = owner.add_peer(None, 1, None).expect("remote Peer");
        let old_session = SessionId::from_bytes([0x43; 16]);
        let new_session = SessionId::from_bytes([0x44; 16]);
        let old_keys = session_keys(&local, &remote, old_session);
        let new_keys = session_keys(&local, &remote, new_session);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_active_v2_peer_profile_for_test(local.clone());
        engine
            .install_v2_peer_membership(&remote.public_membership())
            .expect("install current remote membership");
        assert!(engine.commit_v2_membership_cycle(std::slice::from_ref(&remote.peer.peer_id)));
        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_proxy_replica_session_for_test(&local.peer.peer_id, multi);
        engine.mark_v2_gateway_attached_for_test("127.0.0.1:8443".parse().unwrap());

        engine
            .install_v2_peer_link(remote.peer.peer_id.clone(), old_session, old_keys)
            .expect("install old PeerLink");
        let old_flow = engine
            .prepare_v2_relay_flow("oldflow00001", &remote.peer.peer_id, None)
            .expect("prepare old Relay Flow");
        assert_eq!(old_flow.session_id, old_session);

        engine
            .install_v2_peer_link(remote.peer.peer_id.clone(), new_session, new_keys)
            .expect("replace PeerLink");
        let current = engine.v2_peer_links_for_peer(&remote.peer.peer_id);
        assert_eq!(
            current.len(),
            1,
            "only the current PeerLink key is retained"
        );
        assert_eq!(current[0].session_id, new_session);
        assert_eq!(
            engine
                .v2_relay_seal_for_flow("oldflow00001")
                .expect("old Flow keeps its Arc")
                .session_id,
            old_session
        );
        assert_eq!(
            engine
                .prepare_v2_relay_flow("newflow00001", &remote.peer.peer_id, None)
                .expect("new Flow uses current PeerLink")
                .session_id,
            new_session
        );
    }

    #[tokio::test]
    async fn v2_runtime_gossip_uses_authenticated_direct_peerlink_and_installs_lan_route() {
        use std::net::Ipv4Addr;
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::peer_link_crypto::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};
        use tp_core::protocol::{unpack, PackedMessage};

        fn channel_session() -> (Arc<Session>, mpsc::Receiver<PackedMessage>) {
            let (out_tx, out_rx) = mpsc::channel(8);
            let (_in_tx, in_rx) = mpsc::channel(1);
            (
                Arc::new(Session::new_channeled(
                    out_tx,
                    in_rx,
                    "127.0.0.1:8443".parse().expect("peer"),
                    Arc::new(|| {}),
                    tokio::spawn(async {}),
                    tokio::spawn(async {}),
                )),
                out_rx,
            )
        }

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let peer_a = Arc::new(owner.add_peer(None, 1, None).expect("Peer A"));
        let peer_b = Arc::new(owner.add_peer(None, 1, None).expect("Peer B"));
        let issuer = owner.scope().expect("Scope").tunnel_signing_public_key;
        let secret_a = PeerLinkEphemeralSecretV2::generate();
        let secret_b = PeerLinkEphemeralSecretV2::generate();
        let session_id = SessionId::from_bytes([0x73; 16]);
        let offer = P2pOfferV2::sign(
            &peer_a,
            session_id,
            peer_b.peer.peer_id.clone(),
            Vec::new(),
            CertFingerprint::from_bytes([0x31; 32]),
            &secret_a,
        )
        .expect("Offer");
        let answer = P2pAnswerV2::sign(
            &peer_b,
            &offer,
            true,
            0,
            Vec::new(),
            CertFingerprint::from_bytes([0x32; 32]),
            &secret_b,
        )
        .expect("Answer");
        let keys_a = secret_a
            .derive_session_keys(&offer, &answer, &issuer)
            .expect("A keys");
        let keys_b = secret_b
            .derive_session_keys(&offer, &answer, &issuer)
            .expect("B keys");

        let engine_a = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let engine_b = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        for (engine, profile) in [(&engine_a, &peer_a), (&engine_b, &peer_b)] {
            *engine.active_v2_profile.write() = Some((*profile).clone());
            engine.begin_v2_runtime(profile);
            *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
                profile,
                &gateway,
                vec![format!("{}-AbCd0001-0", profile.tunnel_id)],
            ));
            engine.initialize_v2_peer_gossip();
        }
        let export_prefix =
            crate::peer_runtime::LanExportPrefixV2::new(Ipv4Addr::new(10, 77, 0, 0), 16)
                .expect("prefix");
        let exported =
            crate::peer_runtime::PeerRuntimeRecordV2::new(vec![crate::peer_runtime::LanExportV2 {
                prefix: export_prefix,
                ready: true,
            }])
            .expect("runtime record");
        engine_a
            .set_v2_local_runtime_record(exported)
            .expect("publish local record");

        let (relay_a, _relay_a_rx) = channel_session();
        let (surviving_direct_a, mut surviving_direct_a_rx) = channel_session();
        let (current_key_direct_a, _current_key_direct_a_rx) = channel_session();
        let multi_a = crate::p2p::session::MultiSession::new_with_relay_only(relay_a);
        let (relay_b, _relay_b_rx) = channel_session();
        let (direct_b, _direct_b_rx) = channel_session();
        let multi_b = crate::p2p::session::MultiSession::new_with_relay_only(relay_b);
        let surviving_relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
            &peer_a.peer.peer_id,
            &peer_b.peer.peer_id,
            0,
        )
        .expect("stable relation");
        let current_key_relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
            &peer_a.peer.peer_id,
            &peer_b.peer.peer_id,
            1,
        )
        .expect("second stable relation");
        let surviving_session_id = SessionId::from_bytes([0x72; 16]);
        multi_a
            .install_p2p_session_for_relation(
                surviving_session_id,
                peer_b.peer.peer_id.clone(),
                surviving_direct_a,
                Some(surviving_relation),
            )
            .expect("A surviving Direct Lane");
        multi_a
            .install_p2p_session_for_relation(
                session_id,
                peer_b.peer.peer_id.clone(),
                current_key_direct_a.clone(),
                Some(current_key_relation.clone()),
            )
            .expect("A current-key Direct Lane");
        assert!(multi_a.mark_p2p_session_unusable_for_new_flows_for_handle(&current_key_direct_a));
        multi_b
            .install_p2p_session_for_relation(
                session_id,
                peer_a.peer.peer_id.clone(),
                direct_b.clone(),
                Some(current_key_relation),
            )
            .expect("B direct");
        engine_a.install_multi_session_for_test(multi_a);
        engine_b.install_multi_session_for_test(multi_b);
        engine_b
            .install_v2_peer_link(peer_a.peer.peer_id.clone(), session_id, keys_b)
            .expect("B PeerLink");
        engine_a
            .install_v2_peer_link(peer_b.peer.peer_id.clone(), session_id, keys_a)
            .expect("A PeerLink");

        let packed = timeout(Duration::from_secs(1), surviving_direct_a_rx.recv())
            .await
            .expect("surviving Direct Gossip timeout")
            .expect("surviving Direct Gossip message");
        engine_b
            .handle_msg_from_p2p_session_for_test(
                unpack(&packed.to_bytes()).expect("decode Direct Gossip"),
                Some(direct_b.clone()),
            )
            .await;

        assert_eq!(
            engine_b
                .resolve_overlay_peer("10.77.9.8:27015")
                .expect("route learned Export"),
            Some(peer_a.peer.peer_id.clone())
        );
        assert_eq!(
            engine_b.v2_active_lan_export_snapshot(),
            vec![(
                crate::peer_runtime::LanExportPrefixV2::new(Ipv4Addr::new(10, 77, 0, 0), 16,)
                    .unwrap(),
                peer_a.peer.peer_id.clone(),
            )],
            "native consumers see the same process-local ActiveHere selection"
        );

        engine_a.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[]),
            engine_a.v2_local_lan_export_generation(),
        );
        let packed = timeout(Duration::from_secs(1), surviving_direct_a_rx.recv())
            .await
            .expect("withdraw Gossip timeout")
            .expect("withdraw Gossip message");
        engine_b
            .handle_msg_from_p2p_session_for_test(
                unpack(&packed.to_bytes()).expect("decode withdraw Gossip"),
                Some(direct_b.clone()),
            )
            .await;
        assert_eq!(
            engine_b
                .resolve_overlay_peer("10.77.9.8:27015")
                .expect("withdrawn Export route"),
            None
        );

        engine_a.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[export_prefix]),
            engine_a.v2_local_lan_export_generation(),
        );
        let packed = timeout(Duration::from_secs(1), surviving_direct_a_rx.recv())
            .await
            .expect("republish Gossip timeout")
            .expect("republish Gossip message");
        engine_b
            .handle_msg_from_p2p_session_for_test(
                unpack(&packed.to_bytes()).expect("decode republish Gossip"),
                Some(direct_b),
            )
            .await;
        assert_eq!(
            engine_b
                .resolve_overlay_peer("10.77.9.8:27015")
                .expect("republished Export route"),
            Some(peer_a.peer.peer_id.clone())
        );
    }

    #[tokio::test]
    async fn v2_lan_export_fails_over_on_final_lane_loss_and_rejoins_after_fresh_sync() {
        use crate::runtime_snapshot::{V2ExportPlacement, V2RemotePeerPhase, V2RoutingPhase};
        use tp_core::p2p_types::SessionId;

        fn install_direct_lane(
            engine: &Arc<Engine>,
            local_peer_id: &str,
            remote_peer_id: &str,
            runtime_id: &str,
            session_byte: u8,
            relation_index: usize,
        ) -> (
            Arc<crate::p2p::session::MultiSession>,
            Arc<tp_transport::session::Session>,
        ) {
            let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
            let (direct, _direct_rx, _direct_closed) = watchdog_channel_session();
            let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
            let relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
                local_peer_id,
                remote_peer_id,
                relation_index,
            )
            .expect("stable Peer relation");
            multi
                .install_p2p_session_for_relation(
                    SessionId::from_bytes([session_byte; 16]),
                    remote_peer_id.to_owned(),
                    direct.clone(),
                    Some(relation),
                )
                .expect("Direct Lane");
            engine.install_proxy_replica_session_for_test(runtime_id, multi.clone());
            engine.mark_v2_peer_direct_ready(remote_peer_id);
            (multi, direct)
        }

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let peer_a = owner.add_peer(None, 1, None).expect("Peer A");
        let peer_b = owner.add_peer(None, 1, None).expect("Peer B");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(local.clone());
        engine.begin_v2_runtime(&local);
        engine.initialize_v2_peer_gossip();
        engine.commit_v2_membership_cycle(&[
            peer_a.peer.peer_id.clone(),
            peer_b.peer.peer_id.clone(),
        ]);

        let (multi_a, direct_a) = install_direct_lane(
            &engine,
            &local.peer.peer_id,
            &peer_a.peer.peer_id,
            "local-runtime-0",
            0x61,
            0,
        );
        let (multi_a_second, direct_a_second) = install_direct_lane(
            &engine,
            &local.peer.peer_id,
            &peer_a.peer.peer_id,
            "local-runtime-1",
            0x62,
            1,
        );
        let (_multi_b, _direct_b) = install_direct_lane(
            &engine,
            &local.peer.peer_id,
            &peer_b.peer.peer_id,
            "local-runtime-2",
            0x63,
            0,
        );
        let prefix = crate::peer_runtime::LanExportPrefixV2::new(
            "10.88.0.0".parse().expect("prefix address"),
            16,
        )
        .expect("prefix");
        let record =
            crate::peer_runtime::PeerRuntimeRecordV2::new(vec![crate::peer_runtime::LanExportV2 {
                prefix,
                ready: true,
            }])
            .expect("runtime record");
        engine.receive_v2_gossip(
            &peer_a.peer.peer_id,
            crate::relay_crypto::RelayControlPayloadV2::RuntimeRecord(record.encode()),
        );
        engine.receive_v2_gossip(
            &peer_b.peer.peer_id,
            crate::relay_crypto::RelayControlPayloadV2::RuntimeRecord(record.encode()),
        );
        assert_eq!(
            engine
                .resolve_overlay_peer("10.88.7.9:445")
                .expect("initial Export route"),
            Some(peer_a.peer.peer_id.clone())
        );

        assert!(multi_a.mark_p2p_session_unusable_for_new_flows_for_handle(&direct_a));
        assert_eq!(
            engine
                .resolve_overlay_peer("10.88.7.9:445")
                .expect("route with one remaining Direct Lane"),
            Some(peer_a.peer.peer_id.clone())
        );
        let one_lane_left = engine.v2_runtime_snapshot();
        let peer_a_one_lane_left = one_lane_left
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == peer_a.peer.peer_id)
            .expect("Peer A row");
        assert_eq!(peer_a_one_lane_left.phase, V2RemotePeerPhase::Ready);
        assert_eq!(
            peer_a_one_lane_left.exports[0].placement,
            Some(V2ExportPlacement::ActiveHere)
        );

        assert!(multi_a_second.close_p2p_session_for_handle(&direct_a_second));
        assert_eq!(
            engine
                .resolve_overlay_peer("10.88.7.9:445")
                .expect("failed-over Export route"),
            Some(peer_b.peer.peer_id.clone())
        );
        let after_loss = engine.v2_runtime_snapshot();
        let peer_a_after_loss = after_loss
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == peer_a.peer.peer_id)
            .expect("Peer A row");
        assert_eq!(peer_a_after_loss.phase, V2RemotePeerPhase::Unavailable);
        assert_eq!(peer_a_after_loss.routing, V2RoutingPhase::Unavailable);
        assert!(peer_a_after_loss.exports.is_empty());

        assert!(multi_a.mark_p2p_session_usable_for_new_flows_for_handle(&direct_a));
        let awaiting_fresh_sync = engine.v2_runtime_snapshot();
        let peer_a_syncing = awaiting_fresh_sync
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == peer_a.peer.peer_id)
            .expect("Peer A row");
        assert_eq!(peer_a_syncing.phase, V2RemotePeerPhase::Syncing);
        assert_eq!(peer_a_syncing.routing, V2RoutingPhase::Syncing);
        assert!(peer_a_syncing.exports.is_empty());
        assert_eq!(
            engine
                .resolve_overlay_peer("10.88.7.9:445")
                .expect("route while fresh sync is pending"),
            Some(peer_b.peer.peer_id.clone())
        );

        engine.receive_v2_gossip(
            &peer_a.peer.peer_id,
            crate::relay_crypto::RelayControlPayloadV2::RuntimeRecord(record.encode()),
        );
        let after_fresh_sync = engine.v2_runtime_snapshot();
        let peer_a_rejoined = after_fresh_sync
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == peer_a.peer.peer_id)
            .expect("Peer A row");
        assert_eq!(
            peer_a_rejoined.exports[0].placement,
            Some(V2ExportPlacement::StandbyHere { position: 1 })
        );
        assert_eq!(
            engine
                .resolve_overlay_peer("10.88.7.9:445")
                .expect("route after Peer A fresh sync"),
            Some(peer_b.peer.peer_id)
        );
    }

    #[tokio::test]
    async fn v2_retirement_closes_a_quarantined_direct_generation() {
        use tp_core::p2p_types::SessionId;

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let remote = owner.add_peer(None, 1, None).expect("remote Peer");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(local.clone());
        engine.begin_v2_runtime(&local);
        engine.initialize_v2_peer_gossip();
        engine
            .install_v2_peer_membership(&remote.public_membership())
            .expect("signed membership");

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let (direct, _direct_rx, mut direct_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let session_id = SessionId::from_bytes([0x6a; 16]);
        let relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
            &local.peer.peer_id,
            &remote.peer.peer_id,
            0,
        )
        .expect("stable Peer relation");
        multi
            .install_p2p_session_for_relation(
                session_id,
                remote.peer.peer_id.clone(),
                direct.clone(),
                Some(relation),
            )
            .expect("Direct Lane");
        engine.install_proxy_replica_session_for_test("local-runtime-0", multi.clone());

        assert!(multi.mark_p2p_session_unusable_for_new_flows_for_handle(&direct));
        assert!(multi.has_p2p_session(session_id));
        assert!(engine.retire_overlay_peer(&remote.peer.peer_id));
        assert!(!multi.has_p2p_session(session_id));
        assert!(
            !multi.mark_p2p_session_usable_for_new_flows_for_handle(&direct),
            "a retired quarantined generation must not be resurrected"
        );
        timeout(Duration::from_secs(1), direct_closed.recv())
            .await
            .expect("retirement closes the stale Direct handle")
            .expect("Direct close signal");
    }

    #[test]
    fn v2_native_lan_routes_respect_connected_lan_tunnel_first_policy() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let remote =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 24)
                .unwrap();
        engine
            .overlay_routes
            .write()
            .replace_v2_lan_export_origin(
                "stable-peer-remote",
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix: remote,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        engine.set_native_v2_route_inventory_for_test(&[remote], &[]);

        assert!(engine.v2_native_lan_route_cidrs(false).is_empty());
        assert_eq!(
            engine.v2_native_lan_route_cidrs(true),
            vec!["192.168.70.0/25", "192.168.70.128/25"],
            "Tunnel First uses more-specific fragments instead of replacing a connected route"
        );

        engine
            .set_v2_local_runtime_record(
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix: remote,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        assert!(
            engine.v2_native_lan_route_cidrs(true).is_empty(),
            "Tunnel First never captures this Peer's own ready LAN Export"
        );
    }

    #[test]
    fn local_lan_export_readiness_withdraws_and_republishes_from_inventory() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let prefix =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 24)
                .unwrap();
        engine
            .set_v2_local_runtime_record(
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();

        assert_eq!(
            engine.v2_runtime_snapshot().local_exports,
            vec![crate::runtime_snapshot::V2LocalExportSnapshot {
                prefix: "192.168.70.0/24".into(),
                ready: true,
            }]
        );

        assert!(engine.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[]),
            engine.v2_local_lan_export_generation()
        ));
        assert!(!engine.v2_runtime_snapshot().local_exports[0].ready);

        assert!(engine.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[prefix]),
            engine.v2_local_lan_export_generation()
        ));
        assert!(engine.v2_runtime_snapshot().local_exports[0].ready);

        assert!(engine.refresh_v2_local_lan_exports_from_snapshot(
            None,
            engine.v2_local_lan_export_generation()
        ));
        assert!(!engine.v2_runtime_snapshot().local_exports[0].ready);
    }

    #[test]
    fn automatic_local_lan_export_follows_the_machine_between_networks() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let typed =
            crate::peer_runtime::LanExportPrefixV2::new("10.20.0.0".parse().unwrap(), 16).unwrap();
        let office =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 24)
                .unwrap();
        let home = crate::peer_runtime::LanExportPrefixV2::new("192.168.1.0".parse().unwrap(), 24)
            .unwrap();
        engine.set_v2_local_lan_export_config(
            crate::peer_runtime::LocalLanExportConfigV2 {
                configured: vec![typed],
                auto_current_lan: true,
            },
            Some(&[office]),
        );

        let published = |engine: &Engine| {
            engine
                .v2_runtime_snapshot()
                .local_exports
                .into_iter()
                .map(|export| (export.prefix, export.ready))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            published(&engine),
            vec![
                ("10.20.0.0/16".to_string(), false),
                ("192.168.70.0/24".to_string(), true),
            ]
        );

        // Moving to another network replaces the automatic prefix rather than
        // leaving the last one behind as unavailable.
        assert!(engine.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[home]),
            engine.v2_local_lan_export_generation()
        ));
        assert_eq!(
            published(&engine),
            vec![
                ("10.20.0.0/16".to_string(), false),
                ("192.168.1.0/24".to_string(), true),
            ]
        );

        // Turning the switch off withdraws only what it added.
        engine.set_v2_local_lan_export_config(
            crate::peer_runtime::LocalLanExportConfigV2 {
                configured: vec![typed],
                auto_current_lan: false,
            },
            Some(&[home]),
        );
        assert_eq!(
            published(&engine),
            vec![("10.20.0.0/16".to_string(), false)]
        );
        assert!(!engine.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[home]),
            engine.v2_local_lan_export_generation()
        ));
    }

    #[test]
    fn a_scan_that_predates_a_settings_save_does_not_publish() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let attached =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 24)
                .unwrap();

        let scanned_generation = engine.v2_local_lan_export_generation();
        engine.set_v2_local_lan_export_config(
            crate::peer_runtime::LocalLanExportConfigV2 {
                configured: Vec::new(),
                auto_current_lan: true,
            },
            Some(&[attached]),
        );

        // The blocking scan started before the save, so its snapshot says
        // nothing about the answer the save installed and its own resolution.
        assert!(!engine.refresh_v2_local_lan_exports_from_snapshot(Some(&[]), scanned_generation));
        assert!(engine.v2_runtime_snapshot().local_exports[0].ready);

        // The pass after it scans for the current answer and is accepted.
        assert!(engine.refresh_v2_local_lan_exports_from_snapshot(
            Some(&[]),
            engine.v2_local_lan_export_generation()
        ));
        assert!(engine.v2_runtime_snapshot().local_exports.is_empty());
    }

    #[test]
    fn v2_tunnel_first_fragments_a_broad_remote_prefix_over_a_narrow_connected_lan() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let remote =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.0.0".parse().unwrap(), 16)
                .unwrap();
        let connected =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 24)
                .unwrap();
        engine
            .overlay_routes
            .write()
            .replace_v2_lan_export_origin(
                "stable-peer-remote",
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix: remote,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        engine.set_native_v2_route_inventory_for_test(&[connected], &[]);

        assert_eq!(
            engine.v2_native_lan_route_cidrs(true),
            vec![
                "192.168.0.0/18",
                "192.168.64.0/22",
                "192.168.68.0/23",
                "192.168.70.0/25",
                "192.168.70.128/25",
                "192.168.71.0/24",
                "192.168.72.0/21",
                "192.168.80.0/20",
                "192.168.96.0/19",
                "192.168.128.0/17",
            ]
        );
    }

    #[test]
    fn v2_native_lan_routes_leave_protected_exact_destinations_native() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let remote =
            crate::peer_runtime::LanExportPrefixV2::new("192.168.70.0".parse().unwrap(), 30)
                .unwrap();
        engine
            .overlay_routes
            .write()
            .replace_v2_lan_export_origin(
                "stable-peer-remote",
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix: remote,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        engine
            .set_native_v2_route_inventory_for_test(&[remote], &["192.168.70.1".parse().unwrap()]);

        assert_eq!(
            engine.v2_native_lan_route_cidrs(true),
            vec!["192.168.70.0/32", "192.168.70.2/31"],
            "a protected control/native host is never captured by the TUN"
        );
    }

    #[test]
    fn v2_native_lan_routes_cut_out_only_the_local_self_export() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let remote =
            crate::peer_runtime::LanExportPrefixV2::new("10.0.0.0".parse().unwrap(), 8).unwrap();
        let local =
            crate::peer_runtime::LanExportPrefixV2::new("10.20.0.0".parse().unwrap(), 16).unwrap();
        engine
            .overlay_routes
            .write()
            .replace_v2_lan_export_origin(
                "stable-peer-remote",
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix: remote,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        engine
            .set_v2_local_runtime_record(
                crate::peer_runtime::PeerRuntimeRecordV2::new(vec![
                    crate::peer_runtime::LanExportV2 {
                        prefix: local,
                        ready: true,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        engine.set_native_v2_route_inventory_for_test(&[], &[]);

        assert_eq!(
            engine.v2_native_lan_route_cidrs(true),
            vec![
                "10.0.0.0/12",
                "10.16.0.0/14",
                "10.21.0.0/16",
                "10.22.0.0/15",
                "10.24.0.0/13",
                "10.32.0.0/11",
                "10.64.0.0/10",
                "10.128.0.0/9",
            ],
            "other remote destinations remain capturable without capturing this Peer's LAN"
        );
    }

    #[tokio::test]
    async fn sealed_v2_relay_tcp_flow_opens_bridges_and_preserves_half_close() {
        use crate::access_policy::{
            ClientAccessPolicyV2, ClientAccessPortV2, ClientAccessProtocolV2, ClientAccessRuleV2,
            ClientAccessTargetV2,
        };
        use std::net::Ipv4Addr;
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::peer_link_crypto::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};
        use tp_core::protocol::{pack_tcp_flow_open_v2, TcpFlowOpenV2};

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let source = owner
            .add_peer(Some(Ipv4Addr::new(198, 18, 0, 1)), 1, None)
            .expect("source Peer");
        let target = Arc::new(
            owner
                .add_peer(Some(Ipv4Addr::new(198, 18, 0, 2)), 1, None)
                .expect("target Peer"),
        );
        let issuer = owner.scope().expect("Scope").tunnel_signing_public_key;
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let session_id = SessionId::from_bytes([0x71; 16]);
        let offer = P2pOfferV2::sign(
            &source,
            session_id,
            target.peer.peer_id.clone(),
            Vec::new(),
            CertFingerprint::from_bytes([0x11; 32]),
            &source_secret,
        )
        .expect("Offer");
        let answer = P2pAnswerV2::sign(
            &target,
            &offer,
            true,
            0,
            Vec::new(),
            CertFingerprint::from_bytes([0x22; 32]),
            &target_secret,
        )
        .expect("Answer");
        let source_keys = source_secret
            .derive_session_keys(&offer, &answer, &issuer)
            .expect("source keys");
        let target_keys = target_secret
            .derive_session_keys(&offer, &answer, &issuer)
            .expect("target keys");
        let source_cipher = crate::relay_crypto::RelayCipherV2::new(&source_keys);

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(target.clone());
        *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
            &target,
            &gateway,
            vec![format!("{}-target-0", target.tunnel_id)],
        ));
        engine
            .install_v2_peer_link(source.peer.peer_id.clone(), session_id, target_keys)
            .expect("install target PeerLink");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("target listener");
        let target_addr = listener.local_addr().expect("target address");
        engine
            .set_v2_access_policy(&ClientAccessPolicyV2 {
                allow: vec![ClientAccessRuleV2 {
                    target: ClientAccessTargetV2::ThisPeer,
                    protocol: ClientAccessProtocolV2::Tcp,
                    port: ClientAccessPortV2::Exact(target_addr.port()),
                }],
                deny: vec![],
            })
            .expect("install V2 This Peer mapping");
        let requested_address = format!("{}:{}", target.peer.overlay_ip, target_addr.port());
        let target_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("target accept");
            let mut request = [0_u8; 4];
            socket.read_exact(&mut request).await.expect("target read");
            assert_eq!(&request, b"ping");
            socket.write_all(b"pong").await.expect("target write");
            socket.shutdown().await.expect("target shutdown");
        });

        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            "127.0.0.1:1".parse().expect("relay address"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let conn_id = "seal-flow-1".to_string();
        let conn_id_wire = relay_conn_id_to_wire_v2(&conn_id).expect("wire id");
        let source_context = crate::relay_crypto::RelayRecordContextV2 {
            tunnel_id: &source.tunnel_id,
            peerlink_session_id: &session_id,
            source_peer_id: &source.peer.peer_id,
            target_peer_id: &target.peer.peer_id,
            conn_id: &conn_id_wire,
        };
        let mut sealed_open = requested_address.into_bytes();
        source_cipher
            .seal_flow(
                source_context,
                crate::relay_crypto::RelayFlowKindV2::Open,
                &mut sealed_open,
            )
            .expect("seal OPEN");
        let raw_preface = pack_tcp_flow_open_v2(&TcpFlowOpenV2 {
            conn_id: conn_id.clone(),
            peerlink_session_id: *session_id.as_bytes(),
            sealed_open: Bytes::from(sealed_open),
        });
        let (flow_io, mut source_io) = tokio::io::duplex(256 * 1024);
        let preface = tp_core::protocol::TcpFlowStreamPreface {
            conn_id: conn_id.clone(),
            network: "tcp".into(),
            address: String::new(),
        };
        let incoming = tp_transport::TcpFlowIncoming {
            preface,
            stream: tp_transport::TcpFlowStream::new_raw(
                conn_id.clone(),
                raw_preface,
                Box::pin(flow_io),
            ),
        };
        let flow_task = tokio::spawn(Engine::handle_tcp_flow_stream(
            engine,
            incoming,
            multi,
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
            TrafficPath::Relay,
            TcpFlowLinkContext::default(),
        ));

        let mut response = tp_transport::session::read_tcp_flow_frame(&mut source_io)
            .await
            .expect("OPEN response");
        let response_context = crate::relay_crypto::RelayRecordContextV2 {
            source_peer_id: &target.peer.peer_id,
            target_peer_id: &source.peer.peer_id,
            ..source_context
        };
        source_cipher
            .open_flow(
                response_context,
                crate::relay_crypto::RelayFlowKindV2::OpenResponse,
                &mut response,
            )
            .expect("open response");
        let decoded_response = crate::relay_crypto::RelayControlPayloadV2::decode(&response)
            .expect("decode OPEN response");
        assert!(
            matches!(
                decoded_response,
                crate::relay_crypto::RelayControlPayloadV2::OpenResponse { success: true, .. }
            ),
            "OPEN failed: {decoded_response:?}"
        );

        let mut request = b"ping".to_vec();
        source_cipher
            .seal_flow(
                source_context,
                crate::relay_crypto::RelayFlowKindV2::Data,
                &mut request,
            )
            .expect("seal request");
        tp_transport::session::write_tcp_flow_frame(&mut source_io, &request)
            .await
            .expect("write request");
        source_io.shutdown().await.expect("source half-close");
        let mut reply = tp_transport::session::read_tcp_flow_frame(&mut source_io)
            .await
            .expect("read reply");
        source_cipher
            .open_flow(
                response_context,
                crate::relay_crypto::RelayFlowKindV2::Data,
                &mut reply,
            )
            .expect("open reply");
        assert_eq!(reply, b"pong");
        target_task.await.expect("target task");
        timeout(Duration::from_secs(1), flow_task)
            .await
            .expect("flow timeout")
            .expect("flow join");
    }

    struct SequenceManagedGatewayResolver {
        gateways: Vec<GatewayBootstrapV2>,
        calls: AtomicUsize,
    }

    struct RecordingPeerHeartbeatSender {
        requests: parking_lot::Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait]
    impl ManagedPeerHeartbeatSender for RecordingPeerHeartbeatSender {
        async fn send(
            &self,
            _platform_url: &str,
            request: &crate::peer_heartbeat::PeerHeartbeatRequest,
        ) -> Result<
            Option<crate::peer_heartbeat::PeerRelayUsage>,
            crate::peer_heartbeat::PeerHeartbeatSendError,
        > {
            self.requests
                .lock()
                .push(serde_json::to_value(request).expect("heartbeat JSON"));
            Ok(None)
        }
    }

    struct FailingManagedGatewayResolver;

    #[async_trait]
    impl ManagedGatewayResolver for FailingManagedGatewayResolver {
        async fn resolve(&self, _profile: &PeerProfileV2) -> anyhow::Result<GatewayBootstrapV2> {
            Err(anyhow::anyhow!("Gateway unavailable for test"))
        }
    }

    struct RejectingPeerHeartbeatSender {
        status: reqwest::StatusCode,
        calls: AtomicUsize,
    }

    struct RetryOncePeerHeartbeatSender {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ManagedPeerHeartbeatSender for RetryOncePeerHeartbeatSender {
        async fn send(
            &self,
            _platform_url: &str,
            _request: &crate::peer_heartbeat::PeerHeartbeatRequest,
        ) -> Result<
            Option<crate::peer_heartbeat::PeerRelayUsage>,
            crate::peer_heartbeat::PeerHeartbeatSendError,
        > {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(crate::peer_heartbeat::PeerHeartbeatSendError::Retryable(
                    "Platform heartbeat returned HTTP 503 Service Unavailable".into(),
                ))
            } else {
                Ok(None)
            }
        }
    }

    #[async_trait]
    impl ManagedPeerHeartbeatSender for RejectingPeerHeartbeatSender {
        async fn send(
            &self,
            _platform_url: &str,
            _request: &crate::peer_heartbeat::PeerHeartbeatRequest,
        ) -> Result<
            Option<crate::peer_heartbeat::PeerRelayUsage>,
            crate::peer_heartbeat::PeerHeartbeatSendError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(crate::peer_heartbeat::PeerHeartbeatSendError::Retryable(
                format!("Platform heartbeat returned HTTP {}", self.status),
            ))
        }
    }

    #[test]
    fn managed_scope_pending_stays_in_provisioning_while_resolve_retries() {
        use crate::runtime_snapshot::{
            V2GatewayAttachmentPhase, V2OverallPhase, V2RuntimeReasonCode,
        };

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let mut profile = owner.add_peer(None, 1, None).expect("Peer");
        profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
            platform_url: "https://platform.example".into(),
        };
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.begin_v2_runtime(&profile);

        engine.mark_v2_runtime_failure(&anyhow::anyhow!(
            "Managed Gateway resolve returned HTTP 409 Conflict"
        ));

        let runtime = engine.v2_runtime_snapshot();
        assert_eq!(
            runtime.gateway_attachment.phase,
            V2GatewayAttachmentPhase::ProvisioningScope
        );
        assert_eq!(
            runtime.gateway_attachment.reason_code,
            Some(V2RuntimeReasonCode::ResolvingThroughPlatform)
        );
        assert_eq!(runtime.overall.phase, V2OverallPhase::WaitingForGateway);
    }

    #[tokio::test]
    async fn managed_peer_lifecycle_heartbeats_one_logical_peer_and_finalizes_on_disconnect() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let mut profile = owner.add_peer(None, 1, None).expect("Peer");
        profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
            platform_url: "https://platform.example".into(),
        };
        let sender = Arc::new(RecordingPeerHeartbeatSender {
            requests: parking_lot::Mutex::new(Vec::new()),
        });
        let engine = Engine::new_with_managed_services(
            EngineConfig::default(),
            Arc::new(NullListener),
            Arc::new(FailingManagedGatewayResolver),
            sender.clone(),
        );

        engine
            .connect_with_peer_profile(profile.clone(), None)
            .await
            .expect("start Managed Peer");
        timeout(Duration::from_secs(1), async {
            while sender.requests.lock().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial heartbeat");

        engine.disconnect().await;
        let requests = sender.requests.lock().clone();
        assert!(requests.iter().any(|request| request["final"] == false));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["final"] == true)
                .count(),
            1,
        );
        assert!(requests.iter().all(|request| {
            request["tunnel_id"] == profile.tunnel_id && request["peer_id"] == profile.peer.peer_id
        }));
        assert!(requests
            .iter()
            .all(|request| request["client_version"] == env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn rejected_managed_peer_heartbeat_does_not_cancel_the_connection_generation() {
        for status in [
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::CONFLICT,
        ] {
            let gateway = GatewayBootstrapV2 {
                transport: "quic".into(),
                dial_address: "gateway.example".into(),
                port: 8443,
                mapping_port: None,
                tls_server_name: Some("gateway.example".into()),
                trusted_certificate_pem: None,
            };
            let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
            let mut profile = owner.add_peer(None, 1, None).expect("Peer");
            profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
                platform_url: "https://platform.example".into(),
            };
            let sender = Arc::new(RejectingPeerHeartbeatSender {
                status,
                calls: AtomicUsize::new(0),
            });
            let engine = Engine::new_with_managed_services(
                EngineConfig {
                    client_version: "2.0.0-test".into(),
                    ..EngineConfig::default()
                },
                Arc::new(NullListener),
                Arc::new(FailingManagedGatewayResolver),
                sender.clone(),
            );

            engine
                .connect_with_peer_profile(profile, None)
                .await
                .expect("start Managed Peer");
            let generation = engine.task_cancel_token();
            timeout(Duration::from_secs(1), async {
                while sender.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{status} heartbeat was not attempted"));
            assert!(
                !generation.is_cancelled(),
                "{status} cancelled the Mesh generation"
            );
            assert!(!engine.status().platform_heartbeat.active);
            engine.disconnect().await;
        }
    }

    #[tokio::test]
    async fn retryable_managed_peer_heartbeat_failure_retries_without_cancelling_generation() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let mut profile = owner.add_peer(None, 1, None).expect("Peer");
        profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
            platform_url: "https://platform.example".into(),
        };
        let sender = Arc::new(RetryOncePeerHeartbeatSender {
            calls: AtomicUsize::new(0),
        });
        let engine = Engine::new_with_managed_services(
            EngineConfig {
                client_version: "2.0.0-test".into(),
                ..EngineConfig::default()
            },
            Arc::new(NullListener),
            Arc::new(FailingManagedGatewayResolver),
            sender.clone(),
        );

        engine
            .connect_with_peer_profile(profile, None)
            .await
            .expect("start Managed Peer");
        let generation = engine.task_cancel_token();
        timeout(Duration::from_secs(12), async {
            while sender.calls.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("heartbeat was not retried at the 10 second cadence");
        assert!(!generation.is_cancelled());
        assert!(engine.status().platform_heartbeat.active);
        engine.disconnect().await;
    }

    #[tokio::test]
    async fn one_v2_connect_source_reuses_runtime_family_across_gateway_generations() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let profile = Arc::new(owner.add_peer(None, 3, None).expect("Peer"));
        let source = GatewayAttachmentSource::new(profile, None);
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));

        let first = engine
            .prepare_gateway_attachment(&source)
            .await
            .expect("first Gateway generation");
        let second = engine
            .prepare_gateway_attachment(&source)
            .await
            .expect("second Gateway generation");

        assert_eq!(
            first.tunnel_config.client_ids, second.tunnel_config.client_ids,
            "one connect lifecycle must keep one runtime Replica family"
        );
    }

    #[test]
    fn separate_v2_connect_sources_generate_different_runtime_families() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let profile = Arc::new(owner.add_peer(None, 3, None).expect("Peer"));
        let first = GatewayAttachmentSource::new(profile.clone(), None);
        let second = GatewayAttachmentSource::new(profile, None);

        assert_ne!(
            first.runtime_replica_ids, second.runtime_replica_ids,
            "a new user connect lifecycle must receive a fresh runtime family"
        );
    }

    #[async_trait]
    impl ManagedGatewayResolver for SequenceManagedGatewayResolver {
        async fn resolve(&self, _profile: &PeerProfileV2) -> anyhow::Result<GatewayBootstrapV2> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.gateways
                .get(call)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected Managed resolve call {call}"))
        }
    }

    #[tokio::test]
    async fn managed_peer_gateway_reconnect_does_not_restart_the_heartbeat_task() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).expect("certificate");
        let certificate_pem = certified.cert.pem();
        let server_tls = tp_transport::tls::server_config(
            vec![CertificateDer::from(certified.cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
        )
        .expect("server TLS");
        let unknown_scope_server = QuicServer::bind(
            "127.0.0.1:0".parse().expect("bind address"),
            server_tls.clone(),
            QuicTuning::game_streaming(),
        )
        .expect("bind unknown-Scope Gateway");
        let loaded_scope_server = QuicServer::bind(
            "127.0.0.1:0".parse().expect("bind address"),
            server_tls,
            QuicTuning::game_streaming(),
        )
        .expect("bind loaded-Scope Gateway");
        let unknown_scope_addr = unknown_scope_server.local_addr().expect("Gateway address");
        let loaded_scope_addr = loaded_scope_server.local_addr().expect("Gateway address");

        let initial_gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "127.0.0.1".into(),
            port: unknown_scope_addr.port(),
            mapping_port: None,
            tls_server_name: Some("127.0.0.1".into()),
            trusted_certificate_pem: Some(certificate_pem.clone()),
        };
        let recovered_gateway = GatewayBootstrapV2 {
            port: loaded_scope_addr.port(),
            ..initial_gateway.clone()
        };
        let mut owner = TunnelOwnerFileV2::generate(initial_gateway.clone()).expect("Tunnel");
        let scope = owner.scope().expect("Scope");
        let mut profile = owner.add_peer(None, 1, None).expect("Peer");
        profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
            platform_url: "https://platform.example".into(),
        };

        let unknown_scope_runtime =
            Gateway::new(tp_core::config::GatewayP2pConfig::default(), None);
        let loaded_scope_runtime = Gateway::new(tp_core::config::GatewayP2pConfig::default(), None);
        loaded_scope_runtime
            .scopes()
            .replace_managed_snapshot(vec![scope])
            .expect("load Scope");
        let unknown_scope_task = tokio::spawn({
            let gateway = unknown_scope_runtime.clone();
            async move {
                let _ = gateway
                    .serve(GatewayServer::Quic(unknown_scope_server))
                    .await;
            }
        });
        let loaded_scope_task = tokio::spawn({
            let gateway = loaded_scope_runtime.clone();
            async move {
                let _ = gateway
                    .serve(GatewayServer::Quic(loaded_scope_server))
                    .await;
            }
        });

        let resolver = Arc::new(SequenceManagedGatewayResolver {
            gateways: vec![initial_gateway, recovered_gateway],
            calls: AtomicUsize::new(0),
        });
        let heartbeat_sender = Arc::new(RecordingPeerHeartbeatSender {
            requests: parking_lot::Mutex::new(Vec::new()),
        });
        let engine = Engine::new_with_managed_services(
            EngineConfig::default(),
            Arc::new(NullListener),
            resolver.clone(),
            heartbeat_sender.clone(),
        );
        engine
            .connect_with_peer_profile(profile.clone(), None)
            .await
            .expect("start Managed Peer");

        timeout(Duration::from_secs(8), async {
            loop {
                if engine.latest_tunnel_config().is_some_and(|config| {
                    config.gateway_port == loaded_scope_addr.port()
                        && engine.v2_runtime_snapshot().gateway_attachment.phase
                            == crate::runtime_snapshot::V2GatewayAttachmentPhase::Attached
                        && config.client_ids.first().is_some_and(|replica_id| {
                            loaded_scope_runtime
                                .peers
                                .stable_peer_id(&profile.tunnel_id, replica_id)
                                .as_deref()
                                == Some(profile.peer.peer_id.as_str())
                        })
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Managed Peer did not recover on the newly resolved Gateway");

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            engine
                .latest_tunnel_config()
                .expect("runtime config")
                .gateway_port,
            loaded_scope_addr.port()
        );
        assert!(matches!(
            profile.bootstrap,
            PeerBootstrapV2::ManagedPlatform { .. }
        ));
        assert_eq!(
            heartbeat_sender
                .requests
                .lock()
                .iter()
                .filter(|request| request["final"] == false)
                .count(),
            1,
            "a Gateway reconnect must not start a second immediate heartbeat loop"
        );

        engine.disconnect().await;
        unknown_scope_task.abort();
        loaded_scope_task.abort();
    }

    struct WatchdogFixture {
        sender: tp_transport::SessionSender,
        last_ack: Arc<AtomicU64>,
        relay_last_link_progress_ms: Arc<AtomicU64>,
        closed_rx: mpsc::Receiver<()>,
        active_tcp: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        active_udp: Arc<DashMap<String, DropOldestSender<Bytes>>>,
        traffic: Arc<TrafficCounters>,
        cancel: CancellationToken,
    }

    fn active_tcp_map() -> Arc<DashMap<String, mpsc::Sender<Bytes>>> {
        Arc::new(DashMap::new())
    }

    fn active_udp_map() -> Arc<DashMap<String, DropOldestSender<Bytes>>> {
        Arc::new(DashMap::new())
    }

    #[tokio::test]
    async fn engine_default_denies_unattested_owned_overlay_destination() {
        use tp_core::protocol::PackedMessage;

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            overlay_ipv4: "198.18.7.9".into(),
            ..TunnelConfig::default()
        });
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            "127.0.0.1:8443".parse().expect("peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);

        let error = engine
            .resolve_inbound_dial_target(
                &multi,
                None,
                "deny-local",
                Protocol::Tcp,
                "198.18.7.9:27015",
            )
            .await
            .expect_err("missing LocalServiceExport must deny owned Overlay ingress");

        assert!(error.contains("local service export denied"));
    }

    fn v2_hostname_acl_fixture(
        policy: crate::access_policy::ClientAccessPolicyV2,
        export_ready: bool,
    ) -> (
        Arc<Engine>,
        Arc<crate::p2p::session::MultiSession>,
        std::net::Ipv4Addr,
    ) {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let target = Arc::new(owner.add_peer(None, 1, None).expect("target Peer"));
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(target.clone());
        *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
            &target,
            &gateway,
            vec![format!("{}-target-0", target.tunnel_id)],
        ));
        engine
            .set_v2_access_policy(&policy)
            .expect("install V2 access policy");
        *engine.v2_local_runtime_record.write() = crate::peer_runtime::PeerRuntimeRecordV2 {
            lan_exports: vec![crate::peer_runtime::LanExportV2 {
                // System resolvers portably map `localhost` to loopback.
                // The production setter rejects loopback; this direct
                // record keeps the test focused on the resolver/ACL
                // transaction while exercising a real DNS lookup.
                prefix: crate::peer_runtime::LanExportPrefixV2 {
                    network: std::net::Ipv4Addr::new(127, 0, 0, 0),
                    prefix_len: 8,
                },
                ready: export_ready,
            }],
        };

        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            "127.0.0.1:8443".parse().expect("relay peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        (engine, multi, target.peer.overlay_ip)
    }

    fn attest_hostname_relay(
        engine: &Arc<Engine>,
        multi: &Arc<crate::p2p::session::MultiSession>,
        conn_id: &str,
    ) {
        engine.relay_inbound_attestations.insert(
            conn_id.into(),
            RelayInboundAttestation {
                relay_generation: Arc::downgrade(multi),
                source_peer_id: "source-peer".into(),
                logical_tuple: None,
            },
        );
    }

    fn hostname_access_rule(
        action_target: crate::access_policy::ClientAccessTargetV2,
        port: u16,
    ) -> crate::access_policy::ClientAccessRuleV2 {
        crate::access_policy::ClientAccessRuleV2 {
            target: action_target,
            protocol: crate::access_policy::ClientAccessProtocolV2::Tcp,
            port: crate::access_policy::ClientAccessPortV2::Exact(port),
        }
    }

    #[tokio::test]
    async fn v2_hostname_allow_cannot_override_a_deny_on_the_resolved_ip() {
        let (engine, multi, _) = v2_hostname_acl_fixture(
            crate::access_policy::ClientAccessPolicyV2 {
                allow: vec![hostname_access_rule(
                    crate::access_policy::ClientAccessTargetV2::Host("localhost".into()),
                    27015,
                )],
                deny: vec![hostname_access_rule(
                    crate::access_policy::ClientAccessTargetV2::Ip(
                        std::net::Ipv4Addr::LOCALHOST.into(),
                    ),
                    27015,
                )],
            },
            true,
        );
        attest_hostname_relay(&engine, &multi, "hostname-ip-deny");

        let error = engine
            .resolve_inbound_dial_target(
                &multi,
                None,
                "hostname-ip-deny",
                Protocol::Tcp,
                "localhost:27015",
            )
            .await
            .expect_err("resolved IP Deny must win over Host Allow");
        assert_eq!(error, "NotAuthorized");
    }

    #[tokio::test]
    async fn v2_hostname_target_outside_a_ready_local_export_is_not_authorized() {
        let (engine, multi, _) = v2_hostname_acl_fixture(
            crate::access_policy::ClientAccessPolicyV2 {
                allow: vec![hostname_access_rule(
                    crate::access_policy::ClientAccessTargetV2::Host("localhost".into()),
                    27015,
                )],
                deny: vec![],
            },
            false,
        );
        attest_hostname_relay(&engine, &multi, "hostname-not-ready");

        let error = engine
            .resolve_inbound_dial_target(
                &multi,
                None,
                "hostname-not-ready",
                Protocol::Tcp,
                "localhost:27015",
            )
            .await
            .expect_err("a non-ready Export must fail closed");
        assert_eq!(error, "NotAuthorized");
    }

    #[tokio::test]
    async fn v2_relay_hostname_allow_resolves_to_one_authorized_literal_lan_target() {
        use crate::access_policy::{
            ClientAccessPolicyV2, ClientAccessPortV2, ClientAccessProtocolV2, ClientAccessRuleV2,
            ClientAccessTargetV2,
        };
        use crate::peer_runtime::{LanExportPrefixV2, LanExportV2, PeerRuntimeRecordV2};
        use std::net::Ipv4Addr;
        use tp_core::protocol::PackedMessage;

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let target = Arc::new(owner.add_peer(None, 1, None).expect("target Peer"));
        let source = owner.add_peer(None, 1, None).expect("source Peer");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(target.clone());
        *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
            &target,
            &gateway,
            vec![format!("{}-target-0", target.tunnel_id)],
        ));
        engine
            .set_v2_access_policy(&ClientAccessPolicyV2 {
                allow: vec![ClientAccessRuleV2 {
                    target: ClientAccessTargetV2::Host("localhost".into()),
                    protocol: ClientAccessProtocolV2::Tcp,
                    port: ClientAccessPortV2::Exact(27015),
                }],
                deny: vec![],
            })
            .expect("install Host allow");
        // `localhost` is used only to exercise the system resolver in a
        // portable test. Production validation permits only RFC1918 LAN
        // Exports; constructing the record directly keeps this test focused
        // on the target resolver/ACL/dial-address transaction.
        *engine.v2_local_runtime_record.write() = PeerRuntimeRecordV2 {
            lan_exports: vec![LanExportV2 {
                prefix: LanExportPrefixV2 {
                    network: Ipv4Addr::new(127, 0, 0, 0),
                    prefix_len: 8,
                },
                ready: true,
            }],
        };

        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            "127.0.0.1:8443".parse().expect("relay peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let (direct_out_tx, _direct_out_rx) = mpsc::channel::<PackedMessage>(1);
        let (_direct_in_tx, direct_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let direct = Arc::new(Session::new_channeled(
            direct_out_tx,
            direct_in_rx,
            "127.0.0.1:9443".parse().expect("direct peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
            &source.peer.peer_id,
            &target.peer.peer_id,
            0,
        )
        .expect("canonical relation");
        multi
            .install_p2p_session_for_relation(
                tp_core::p2p_types::SessionId::from_bytes([0x98; 16]),
                source.peer.peer_id,
                direct.clone(),
                Some(relation),
            )
            .expect("install Direct source");
        engine.relay_inbound_attestations.insert(
            "hostname-relay-1".into(),
            RelayInboundAttestation {
                relay_generation: Arc::downgrade(&multi),
                source_peer_id: "source-peer".into(),
                logical_tuple: None,
            },
        );

        let target = engine
            .resolve_inbound_dial_target(
                &multi,
                None,
                "hostname-relay-1",
                Protocol::Tcp,
                "localhost:27015",
            )
            .await
            .expect("authorized hostname must resolve");
        assert_eq!(target.address, "127.0.0.1:27015");
        assert!(target.v2_access_authorized);

        let direct_target = engine
            .resolve_inbound_dial_target(
                &multi,
                Some(&direct),
                "hostname-direct-1",
                Protocol::Tcp,
                "localhost:27015",
            )
            .await
            .expect("Direct must use the same hostname authorization path");
        assert_eq!(direct_target.address, target.address);
        assert!(direct_target.v2_access_authorized);
    }

    #[tokio::test]
    async fn relay_target_acknowledges_source_attestation_on_exact_capable_generation() {
        use std::net::{IpAddr, Ipv4Addr};
        use tp_core::protocol::{unpack, PackedMessage, TransportCapabilities};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            overlay_ipv4: "198.18.7.9".into(),
            ..TunnelConfig::default()
        });
        let (relay_out_tx, mut relay_out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8443);
        let relay = Arc::new(
            Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                Arc::new(|| {}),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            )
            .with_capabilities(TransportCapabilities {
                relay_source_attestation_v1: true,
                ..TransportCapabilities::default()
            }),
        );
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_multi_session_for_test(multi);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBind {
                conn_id: "relay-loc-1".into(),
                peer_client_id: "mesh-RemoteB1-0".into(),
            })
            .await;

        let ack = timeout(Duration::from_millis(100), relay_out_rx.recv())
            .await
            .expect("target must acknowledge a valid source attestation")
            .expect("relay outbound remains open");
        let ack = unpack(&ack.to_bytes()).expect("decode relay target ack");
        assert!(
            matches!(
                ack,
                BinaryMessage::RelayRouteBindAck {
                    ref conn_id,
                    success: true,
                    ref error,
                } if conn_id == "relay-loc-1" && error.is_empty()
            ),
            "unexpected target ack: {ack:?}"
        );
    }

    #[tokio::test]
    async fn duplicate_relay_source_bind_fails_closed_and_clears_the_conn_id() {
        use tp_core::protocol::{unpack, PackedMessage, TransportCapabilities};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            ..TunnelConfig::default()
        });
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(4);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(
            Session::new_channeled(
                out_tx,
                in_rx,
                "127.0.0.1:8443".parse().expect("peer"),
                Arc::new(|| {}),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            )
            .with_capabilities(TransportCapabilities {
                relay_source_attestation_v1: true,
                ..TransportCapabilities::default()
            }),
        );
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_multi_session_for_test(multi);

        for expected_success in [true, false] {
            engine
                .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBind {
                    conn_id: "dup-bind-1".into(),
                    peer_client_id: "mesh-RemoteB1-0".into(),
                })
                .await;
            let ack = timeout(Duration::from_millis(200), out_rx.recv())
                .await
                .expect("bind ack timeout")
                .expect("bind ack");
            assert!(matches!(
                unpack(&ack.to_bytes()).expect("decode bind ack"),
                BinaryMessage::RelayRouteBindAck { success, .. } if success == expected_success
            ));
        }
        assert!(
            !engine.relay_inbound_attestations.contains_key("dup-bind-1"),
            "a duplicate Bind failure must not leave the first attestation reusable"
        );
    }

    #[tokio::test]
    async fn relay_source_bind_without_negotiated_capability_is_rejected_without_state() {
        use tp_core::protocol::{unpack, PackedMessage};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            ..TunnelConfig::default()
        });
        let (out_tx, mut out_rx) = mpsc::channel::<PackedMessage>(2);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            "127.0.0.1:8443".parse().expect("peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_multi_session_for_test(multi);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBind {
                conn_id: "legacy-bind-1".into(),
                peer_client_id: "mesh-RemoteB1-0".into(),
            })
            .await;
        assert!(matches!(
            unpack(
                &timeout(Duration::from_millis(200), out_rx.recv())
                    .await
                    .expect("bind ack timeout")
                    .expect("bind ack")
                    .to_bytes()
            )
            .expect("decode bind ack"),
            BinaryMessage::RelayRouteBindAck { success: false, .. }
        ));
        assert!(!engine
            .relay_inbound_attestations
            .contains_key("legacy-bind-1"));
    }

    #[tokio::test]
    async fn relay_source_bind_on_a_direct_session_is_rejected_without_state() {
        use tp_core::protocol::{unpack, PackedMessage};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            ..TunnelConfig::default()
        });
        let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(1);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            relay_out_tx,
            relay_in_rx,
            "127.0.0.1:8443".parse().expect("relay peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let (p2p_out_tx, mut p2p_out_rx) = mpsc::channel::<PackedMessage>(2);
        let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let direct = Arc::new(Session::new_channeled(
            p2p_out_tx,
            p2p_in_rx,
            "127.0.0.1:9443".parse().expect("direct peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_multi_session_for_test(multi);

        engine
            .handle_msg_from_p2p_session_for_test(
                BinaryMessage::RelayRouteBind {
                    conn_id: "direct-bind-1".into(),
                    peer_client_id: "mesh-RemoteB1-0".into(),
                },
                Some(direct),
            )
            .await;
        assert!(matches!(
            unpack(
                &timeout(Duration::from_millis(200), p2p_out_rx.recv())
                    .await
                    .expect("direct bind ack timeout")
                    .expect("direct bind ack")
                    .to_bytes()
            )
            .expect("decode direct bind ack"),
            BinaryMessage::RelayRouteBindAck { success: false, .. }
        ));
        assert!(!engine
            .relay_inbound_attestations
            .contains_key("direct-bind-1"));
    }

    #[tokio::test]
    async fn relay_attestation_is_atomically_consumed_by_the_first_logical_tuple() {
        use tp_core::config::{
            LocalServiceExportConfig, LocalServiceProtocolConfig, LocalServiceRouteKindConfig,
            LocalServiceSourcePolicyConfig,
        };
        use tp_core::protocol::PackedMessage;

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            overlay_ipv4: "198.18.7.9".into(),
            ..TunnelConfig::default()
        });
        engine
            .set_local_service_exports(&[
                LocalServiceExportConfig {
                    route_kind: LocalServiceRouteKindConfig::Overlay,
                    protocol: LocalServiceProtocolConfig::Tcp,
                    ingress_port: 27015,
                    source_policy: LocalServiceSourcePolicyConfig::AnyTunnelPeer,
                    local_host: "127.0.0.1".into(),
                    local_port: 31015,
                },
                LocalServiceExportConfig {
                    route_kind: LocalServiceRouteKindConfig::Overlay,
                    protocol: LocalServiceProtocolConfig::Tcp,
                    ingress_port: 27016,
                    source_policy: LocalServiceSourcePolicyConfig::AnyTunnelPeer,
                    local_host: "127.0.0.1".into(),
                    local_port: 31016,
                },
            ])
            .expect("install tuple exports");
        let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(1);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            relay_out_tx,
            relay_in_rx,
            "127.0.0.1:8443".parse().expect("relay peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.relay_inbound_attestations.insert(
            "tuple-lock-1".into(),
            RelayInboundAttestation {
                relay_generation: Arc::downgrade(&multi),
                source_peer_id: "mesh-RemoteB1-0".into(),
                logical_tuple: None,
            },
        );

        assert_eq!(
            engine
                .resolve_inbound_dial_target(
                    &multi,
                    None,
                    "tuple-lock-1",
                    Protocol::Tcp,
                    "198.18.7.9:27015",
                )
                .await
                .expect("first tuple consumes attestation")
                .address,
            "127.0.0.1:31015"
        );
        let error = engine
            .resolve_inbound_dial_target(
                &multi,
                None,
                "tuple-lock-1",
                Protocol::Tcp,
                "198.18.7.9:27016",
            )
            .await
            .expect_err("a second tuple must not reuse the same attestation");
        assert!(error.contains("already consumed"));
    }

    #[tokio::test]
    async fn explicit_relay_overlay_export_authorizes_framed_connect_and_tcp_flow() {
        use std::net::{IpAddr, Ipv4Addr};
        use tokio::net::TcpListener;
        use tp_core::config::{
            LocalServiceExportConfig, LocalServiceProtocolConfig, LocalServiceRouteKindConfig,
            LocalServiceSourcePolicyConfig,
        };
        use tp_core::protocol::{unpack, PackedMessage, TransportCapabilities};

        let target_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local exported service");
        let target_addr = target_listener.local_addr().expect("target address");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            overlay_ipv4: "198.18.7.9".into(),
            ..TunnelConfig::default()
        });
        engine
            .set_local_service_exports(&[LocalServiceExportConfig {
                route_kind: LocalServiceRouteKindConfig::Overlay,
                protocol: LocalServiceProtocolConfig::Tcp,
                ingress_port: 27015,
                source_policy: LocalServiceSourcePolicyConfig::AnyTunnelPeer,
                local_host: target_addr.ip().to_string(),
                local_port: target_addr.port(),
            }])
            .expect("install export");
        let (relay_out_tx, mut relay_out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(
            Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8443),
                Arc::new(|| {}),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            )
            .with_capabilities(TransportCapabilities {
                relay_source_attestation_v1: true,
                ..TransportCapabilities::default()
            }),
        );
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_multi_session_for_test(multi.clone());

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBind {
                conn_id: "map-frame-1".into(),
                peer_client_id: "mesh-RemoteB1-0".into(),
            })
            .await;
        assert!(matches!(
            unpack(
                &timeout(Duration::from_millis(200), relay_out_rx.recv())
                    .await
                    .expect("bind ack timeout")
                    .expect("relay output")
                    .to_bytes()
            )
            .expect("decode bind ack"),
            BinaryMessage::RelayRouteBindAck { success: true, .. }
        ));
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Connect {
                conn_id: "map-frame-1".into(),
                network: "tcp".into(),
                address: "198.18.7.9:27015".into(),
            })
            .await;
        let (framed_target, _) = timeout(Duration::from_secs(1), target_listener.accept())
            .await
            .expect("framed export dial timeout")
            .expect("accept framed export dial");
        assert!(matches!(
            unpack(
                &timeout(Duration::from_secs(1), relay_out_rx.recv())
                    .await
                    .expect("connect response timeout")
                    .expect("relay output")
                    .to_bytes()
            )
            .expect("decode connect response"),
            BinaryMessage::ConnectResponse { success: true, .. }
        ));
        drop(framed_target);
        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Close {
                conn_id: "map-frame-1".into(),
            })
            .await;

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBind {
                conn_id: "map-flow-1".into(),
                peer_client_id: "mesh-RemoteB1-0".into(),
            })
            .await;
        assert!(matches!(
            unpack(
                &timeout(Duration::from_millis(200), relay_out_rx.recv())
                    .await
                    .expect("flow bind ack timeout")
                    .expect("relay output")
                    .to_bytes()
            )
            .expect("decode flow bind ack"),
            BinaryMessage::RelayRouteBindAck { success: true, .. }
        ));
        let preface = tp_core::protocol::TcpFlowStreamPreface {
            conn_id: "map-flow-1".into(),
            network: "tcp".into(),
            address: "198.18.7.9:27015".into(),
        };
        let (flow_io, mut peer_io) = tokio::io::duplex(4096);
        let incoming = tp_transport::TcpFlowIncoming {
            preface: preface.clone(),
            stream: tp_transport::TcpFlowStream::new(preface, Box::pin(flow_io)),
        };
        let flow = tokio::spawn(Engine::handle_tcp_flow_stream(
            engine.clone(),
            incoming,
            multi,
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
            TrafficPath::Relay,
            TcpFlowLinkContext::default(),
        ));
        let (flow_target, _) = timeout(Duration::from_secs(1), target_listener.accept())
            .await
            .expect("flow export dial timeout")
            .expect("accept flow export dial");
        let response = tp_transport::session::read_tcp_flow_frame(&mut peer_io)
            .await
            .expect("flow response frame");
        assert!(matches!(
            unpack(&response).expect("decode flow response"),
            BinaryMessage::ConnectResponse { success: true, .. }
        ));
        drop(flow_target);
        drop(peer_io);
        timeout(Duration::from_secs(1), flow)
            .await
            .expect("flow handler finish")
            .expect("flow task join");
    }

    #[tokio::test]
    async fn attested_relay_local_response_never_falls_back_to_an_unrelated_p2p_peer() {
        use tokio::net::TcpListener;
        use tp_core::config::{
            LocalServiceExportConfig, LocalServiceProtocolConfig, LocalServiceRouteKindConfig,
            LocalServiceSourcePolicyConfig,
        };
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{unpack, PackedMessage, TransportCapabilities};

        let target_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind exported target");
        let target_addr = target_listener.local_addr().expect("target address");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            overlay_ipv4: "198.18.7.9".into(),
            ..TunnelConfig::default()
        });
        engine
            .set_local_service_exports(&[LocalServiceExportConfig {
                route_kind: LocalServiceRouteKindConfig::Overlay,
                protocol: LocalServiceProtocolConfig::Tcp,
                ingress_port: 27015,
                source_policy: LocalServiceSourcePolicyConfig::AnyTunnelPeer,
                local_host: target_addr.ip().to_string(),
                local_port: target_addr.port(),
            }])
            .expect("install export");

        let (relay_out_tx, mut relay_out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(
            Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                "127.0.0.1:8443".parse().expect("relay peer"),
                Arc::new(|| {}),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            )
            .with_capabilities(TransportCapabilities {
                relay_source_attestation_v1: true,
                ..TransportCapabilities::default()
            }),
        );
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let (p2p_out_tx, mut p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let unrelated_p2p = Arc::new(Session::new_channeled(
            p2p_out_tx,
            p2p_in_rx,
            "127.0.0.1:9443".parse().expect("p2p peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        multi
            .install_p2p_session(
                SessionId::from_bytes([0x73; 16]),
                "mesh-UnrelatedC1-0".into(),
                unrelated_p2p,
            )
            .expect("install unrelated direct Peer");
        engine.install_multi_session_for_test(multi);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::RelayRouteBind {
                conn_id: "relay-pin-1".into(),
                peer_client_id: "mesh-RemoteB1-0".into(),
            })
            .await;
        assert!(matches!(
            unpack(
                &timeout(Duration::from_millis(200), relay_out_rx.recv())
                    .await
                    .expect("bind ack timeout")
                    .expect("relay ack")
                    .to_bytes()
            )
            .expect("decode bind ack"),
            BinaryMessage::RelayRouteBindAck { success: true, .. }
        ));
        drop(relay_out_rx);

        engine
            .handle_proxy_connect_response_for_test(BinaryMessage::Connect {
                conn_id: "relay-pin-1".into(),
                network: "tcp".into(),
                address: "198.18.7.9:27015".into(),
            })
            .await;
        let (target, _) = timeout(Duration::from_secs(1), target_listener.accept())
            .await
            .expect("target dial timeout")
            .expect("target accept");
        drop(target);

        assert!(
            timeout(Duration::from_millis(100), p2p_out_rx.recv())
                .await
                .is_err(),
            "an attested relay-local response must not be sent to an unrelated direct Peer"
        );
    }

    #[tokio::test]
    async fn v2_direct_inbound_send_failure_does_not_migrate_the_flow_to_relay() {
        use crate::access_policy::{
            ClientAccessPolicyV2, ClientAccessPortV2, ClientAccessProtocolV2, ClientAccessRuleV2,
            ClientAccessTargetV2,
        };
        use std::net::Ipv4Addr;
        use tokio::net::TcpListener;
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::PackedMessage;

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let source = owner
            .add_peer(Some(Ipv4Addr::new(198, 18, 0, 1)), 1, None)
            .expect("source Peer");
        let target = Arc::new(
            owner
                .add_peer(Some(Ipv4Addr::new(198, 18, 0, 2)), 1, None)
                .expect("target Peer"),
        );
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(target.clone());
        *engine.latest_tunnel_config.write() = Some(v2_tunnel_config(
            &target,
            &gateway,
            vec![format!("{}-target-0", target.tunnel_id)],
        ));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("target listener");
        let target_addr = listener.local_addr().expect("target address");
        engine
            .set_v2_access_policy(&ClientAccessPolicyV2 {
                allow: vec![ClientAccessRuleV2 {
                    target: ClientAccessTargetV2::ThisPeer,
                    protocol: ClientAccessProtocolV2::Tcp,
                    port: ClientAccessPortV2::Exact(target_addr.port()),
                }],
                deny: vec![],
            })
            .expect("install V2 This Peer mapping");

        let (relay_out_tx, mut relay_out_rx) = mpsc::channel::<PackedMessage>(2);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let relay = Arc::new(Session::new_channeled(
            relay_out_tx,
            relay_in_rx,
            "127.0.0.1:8443".parse().expect("relay peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let (direct_out_tx, direct_out_rx) = mpsc::channel::<PackedMessage>(1);
        drop(direct_out_rx);
        let (_direct_in_tx, direct_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let direct = Arc::new(Session::new_channeled(
            direct_out_tx,
            direct_in_rx,
            "127.0.0.1:9443".parse().expect("direct peer"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
            &source.peer.peer_id,
            &target.peer.peer_id,
            0,
        )
        .expect("canonical relation");
        multi
            .install_p2p_session_for_relation(
                SessionId::from_bytes([0x92; 16]),
                source.peer.peer_id.clone(),
                direct.clone(),
                Some(relation),
            )
            .expect("install Direct session");
        engine.install_multi_session_for_test(multi);

        engine
            .handle_msg_from_p2p_session_for_test(
                BinaryMessage::Connect {
                    conn_id: "v2pinclose1".into(),
                    network: "tcp".into(),
                    address: format!("{}:{}", target.peer.overlay_ip, target_addr.port()),
                },
                Some(direct),
            )
            .await;
        let (target_socket, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("target dial timeout")
            .expect("target accept");
        drop(target_socket);

        assert!(
            timeout(Duration::from_millis(150), relay_out_rx.recv())
                .await
                .is_err(),
            "a V2 Direct Flow must close instead of moving its response or data to Relay"
        );
    }

    #[tokio::test]
    async fn direct_session_without_canonical_relation_cannot_use_local_export() {
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::PackedMessage;

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            overlay_ipv4: "198.18.7.9".into(),
            ..TunnelConfig::default()
        });
        let session = || {
            let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(1);
            let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
            Arc::new(Session::new_channeled(
                out_tx,
                in_rx,
                "127.0.0.1:8443".parse().expect("peer"),
                Arc::new(|| {}),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ))
        };
        let direct = session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(session());
        multi
            .install_p2p_session(
                SessionId::from_bytes([0x91; 16]),
                "mesh-RemoteB1-0".into(),
                direct.clone(),
            )
            .expect("install legacy relationless session");

        let error = engine
            .resolve_inbound_dial_target(
                &multi,
                Some(&direct),
                "direct-deny",
                Protocol::Tcp,
                "198.18.7.9:27015",
            )
            .await
            .expect_err("relationless direct session must fail closed");

        assert!(error.contains("canonical Peer relation"));
    }

    #[tokio::test]
    async fn disconnect_cancels_cooperative_engine_tasks() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let cancel = engine.task_cancel_token();
        let (cancelled_tx, cancelled_rx) = oneshot::channel();

        engine.tasks().spawn(async move {
            cancel.cancelled().await;
            let _ = cancelled_tx.send(());
        });

        timeout(Duration::from_secs(1), engine.disconnect())
            .await
            .expect("disconnect should not wait for the 5s fallback for cooperative tasks");
        timeout(Duration::from_millis(100), cancelled_rx)
            .await
            .expect("task should observe engine cancellation")
            .expect("task should report cancellation");
    }

    #[tokio::test]
    async fn disconnect_replaces_task_cancel_token() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let old_token = engine.task_cancel_token();

        engine.disconnect().await;

        let new_token = engine.task_cancel_token();
        assert!(
            old_token.is_cancelled(),
            "disconnect must cancel the token captured by active tasks"
        );
        assert!(
            !new_token.is_cancelled(),
            "disconnect must replace the cancelled token for the next connect cycle"
        );
    }

    #[tokio::test]
    async fn disconnect_falls_back_after_5s_for_uncooperative_engine_tasks() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.tasks().spawn(futures_util::future::pending::<()>());

        let started = Instant::now();
        timeout(Duration::from_secs(6), engine.disconnect())
            .await
            .expect("disconnect should return via the 5s fallback for uncooperative tasks");
        assert!(
            started.elapsed() >= Duration::from_secs(5),
            "uncooperative tasks should still exercise the bounded drain fallback"
        );
    }

    #[tokio::test]
    async fn disconnect_aborts_owned_engine_tasks_after_drain_timeout() {
        struct DropNotify(Option<oneshot::Sender<()>>);

        impl Drop for DropNotify {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (dropped_tx, dropped_rx) = oneshot::channel();

        engine.spawn_engine_task(async move {
            let _notify = DropNotify(Some(dropped_tx));
            futures_util::future::pending::<()>().await;
        });

        timeout(Duration::from_secs(6), engine.disconnect())
            .await
            .expect("disconnect should return via the 5s fallback");
        timeout(Duration::from_millis(200), dropped_rx)
            .await
            .expect("owned engine task should be aborted after fallback")
            .expect("drop notifier should send");
    }

    #[tokio::test]
    async fn cooperative_engine_task_disconnect_does_not_wait_for_5s_fallback() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let cancel = engine.task_cancel_token();
        engine.tasks().spawn(async move {
            cancel.cancelled().await;
        });

        let started = Instant::now();
        timeout(Duration::from_secs(1), engine.disconnect())
            .await
            .expect("cooperative task should let disconnect finish promptly");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cooperative task should not wait for the bounded drain fallback"
        );
    }

    #[test]
    fn local_lan_route_publish_policy_reads_the_latest_runtime_config() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        assert!(!engine.publish_local_lan_routes());

        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_candidates: true,
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        assert!(engine.publish_local_lan_routes());

        engine.set_p2p_underlay_bypass_ready(true);
        assert!(engine.publish_local_lan_routes());

        engine.set_p2p_underlay_bypass_ready(false);
        assert!(engine.publish_local_lan_routes());

        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_route_aliases: false,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        assert!(!engine.publish_local_lan_routes());
    }

    #[test]
    fn native_tun_lan_routes_exclude_local_infrastructure_but_socks_keeps_matching() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            ..TunnelConfig::default()
        });
        engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("active remote Peer");
        engine
            .replace_peer_lan_aliases(
                "mesh-RemoteB1-0",
                &[
                    "192.168.2.20".into(),
                    "192.168.240.44".into(),
                    "192.168.240.1".into(),
                    "192.168.240.53".into(),
                    "192.168.240.88".into(),
                ],
            )
            .expect("trusted-Tunnel aliases");

        engine.set_native_lan_route_exclusions_for_test(&[
            "192.168.240.44".parse().unwrap(), // local interface
            "192.168.240.1".parse().unwrap(),  // default gateway
            "192.168.240.53".parse().unwrap(), // DNS resolver
            "192.168.240.88".parse().unwrap(), // current Gateway endpoint
        ]);

        assert_eq!(engine.lan_alias_route_cidrs(), vec!["192.168.2.20/32"]);
        for destination in [
            "192.168.240.44:39001",
            "192.168.240.1:39001",
            "192.168.240.53:39001",
            "192.168.240.88:39001",
        ] {
            assert_eq!(
                engine
                    .resolve_overlay_peer(destination)
                    .expect("SOCKS matching remains independent of native capture"),
                Some("mesh-RemoteB1-0".into())
            );
        }
    }

    #[test]
    fn local_lan_publication_uses_all_discovered_interfaces_not_only_the_underlay() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_candidates: true,
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        engine.commit_native_lan_route_inventory_for_test(&["192.168.240.1".parse().unwrap()]);

        assert_eq!(
            engine.refresh_local_lan_route_publication_from_discovery(Ok(vec![
                "192.168.240.44".into(),
                "10.20.0.3".into(),
                "172.30.240.1".into(),
            ])),
            Some(vec![
                "10.20.0.3".into(),
                "172.30.240.1".into(),
                "192.168.240.44".into(),
            ])
        );
        assert_eq!(
            engine.published_local_service_lan_hosts(),
            vec![
                "10.20.0.3".parse::<std::net::IpAddr>().unwrap(),
                "172.30.240.1".parse::<std::net::IpAddr>().unwrap(),
                "192.168.240.44".parse::<std::net::IpAddr>().unwrap(),
            ]
        );

        engine.set_p2p_underlay_bypass_ready(false);
        assert_eq!(
            engine.published_local_service_lan_hosts(),
            vec![
                "10.20.0.3".parse::<std::net::IpAddr>().unwrap(),
                "172.30.240.1".parse::<std::net::IpAddr>().unwrap(),
                "192.168.240.44".parse::<std::net::IpAddr>().unwrap(),
            ],
            "Manager teardown may withdraw native capture but must not erase the Peer's publication"
        );
    }

    #[test]
    fn local_lan_discovery_failure_denies_local_delivery() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        assert_eq!(
            engine.refresh_local_lan_route_publication_from_discovery(Ok(vec![
                "192.168.240.44".into(),
            ])),
            Some(vec!["192.168.240.44".into()])
        );

        let publication = engine.refresh_local_lan_route_publication_from_discovery(Err(
            std::io::Error::other("interface inventory unavailable"),
        ));

        assert_eq!(
            publication, None,
            "failed discovery has no authoritative publication"
        );
        assert!(
            engine.published_local_service_lan_hosts().is_empty(),
            "the target must fail closed locally while its current interface ownership is unknown"
        );
    }

    #[test]
    fn stale_connection_generation_cannot_restore_lan_hosts_after_reconnect() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_route_aliases: true,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        let old_generation = engine.begin_local_lan_publication_generation();
        assert_eq!(
            engine.apply_local_lan_route_publication_for_generation(
                old_generation,
                true,
                Ok(vec!["192.168.240.44".into()]),
            ),
            Some(vec!["192.168.240.44".into()])
        );

        let new_generation = engine.begin_local_lan_publication_generation();
        assert_eq!(
            engine.apply_local_lan_route_publication_for_generation(
                old_generation,
                true,
                Ok(vec!["10.20.0.3".into()]),
            ),
            None,
            "a completed discovery from the old connection must be discarded"
        );
        assert!(engine.published_local_service_lan_hosts().is_empty());

        assert_eq!(
            engine.apply_local_lan_route_publication_for_generation(
                new_generation,
                true,
                Ok(vec!["172.30.240.1".into()]),
            ),
            Some(vec!["172.30.240.1".into()])
        );
    }

    #[test]
    fn alias_only_mode_still_requires_local_route_inventory_before_tun_capture() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_p2p_config(Arc::new(tp_core::config::ClientP2pConfig {
            allow_lan_route_aliases: true,
            allow_lan_candidates: false,
            ..tp_core::config::ClientP2pConfig::default()
        }));
        engine.set_latest_tunnel_config_for_test(TunnelConfig {
            tunnel_id: "mesh".into(),
            peer_id: "mesh-Local001-0".into(),
            ..TunnelConfig::default()
        });
        engine
            .replace_peer_lan_aliases("mesh-RemoteB1-0", &["192.168.2.20".into()])
            .expect("trusted-Tunnel alias");

        assert!(
            engine.lan_alias_route_cidrs().is_empty(),
            "no LAN Link Candidate removes recursion risk, but an unknown local gateway/DNS inventory still forbids native capture"
        );
        assert_eq!(
            engine.resolve_overlay_peer("192.168.2.20:39001").unwrap(),
            Some("mesh-RemoteB1-0".into()),
            "explicit SOCKS matching remains available"
        );

        engine.commit_native_lan_route_inventory_for_test(&[
            "192.168.240.44".parse().unwrap(),
            "192.168.240.1".parse().unwrap(),
        ]);
        assert_eq!(engine.lan_alias_route_cidrs(), vec!["192.168.2.20/32"]);
    }

    fn insert_active_udp(
        active_udp: &Arc<DashMap<String, DropOldestSender<Bytes>>>,
        conn_id: &str,
    ) {
        let (udp_tx, _udp_rx) = tp_transport::drop_oldest_channel::<Bytes>(1);
        active_udp.insert(conn_id.to_string(), udp_tx);
    }

    fn test_active_counters(
        active_tcp: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        active_udp: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    ) -> LinkActiveFlowCounters {
        LinkActiveFlowCounters::with_source(
            Arc::new(LinkActiveFlows::default()),
            Arc::new(move || LinkActiveFlowSnapshot {
                active_tcp_flows: active_tcp.len(),
                active_udp_flows: active_udp.len(),
                last_link_io_progress_ms: 0,
            }),
        )
    }

    fn handle_msg_test_multi() -> Arc<crate::p2p::session::MultiSession> {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        crate::p2p::session::MultiSession::new_with_existing_maps(
            Arc::new(session),
            active_tcp_map(),
            active_udp_map(),
        )
    }

    fn test_link_watchdog_config(
        stale_after: Duration,
        check_interval: Duration,
    ) -> LinkWatchdogConfig {
        LinkWatchdogConfig {
            heartbeat_interval: Duration::from_secs(1),
            ack_stale_after: stale_after,
            active_no_link_progress_grace: stale_after
                .saturating_mul(2)
                .max(stale_after + check_interval),
            check_interval,
            stale_log_interval: Duration::from_secs(30),
        }
    }

    fn with_active_link_progress_grace(
        mut config: LinkWatchdogConfig,
        grace: Duration,
    ) -> LinkWatchdogConfig {
        config.active_no_link_progress_grace = config.active_no_link_progress_grace.max(grace);
        config
    }

    fn test_link_watchdog_config_with_dead_after(
        stale_after: Duration,
        dead_after: Duration,
        check_interval: Duration,
    ) -> LinkWatchdogConfig {
        LinkWatchdogConfig {
            heartbeat_interval: Duration::from_secs(1),
            ack_stale_after: stale_after,
            active_no_link_progress_grace: dead_after,
            check_interval,
            stale_log_interval: Duration::from_secs(30),
        }
    }

    fn watchdog_fixture() -> WatchdogFixture {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();

        let now = monotonic_millis();
        WatchdogFixture {
            sender,
            last_ack: Arc::new(AtomicU64::new(now)),
            relay_last_link_progress_ms: Arc::new(AtomicU64::new(now)),
            closed_rx,
            active_tcp: active_tcp_map(),
            active_udp: active_udp_map(),
            traffic: Arc::new(TrafficCounters::default()),
            cancel: CancellationToken::new(),
        }
    }

    fn p2p_watchdog_test_config(active_grace: Duration) -> LinkWatchdogConfig {
        LinkWatchdogConfig {
            heartbeat_interval: Duration::from_millis(5),
            ack_stale_after: Duration::from_millis(20),
            active_no_link_progress_grace: active_grace.max(Duration::from_millis(50)),
            check_interval: Duration::from_millis(5),
            stale_log_interval: Duration::from_millis(10),
        }
    }

    fn p2p_watchdog_test_config_with_ack_stale_after(
        stale_after: Duration,
        dead_after: Duration,
    ) -> LinkWatchdogConfig {
        LinkWatchdogConfig {
            heartbeat_interval: Duration::from_millis(5),
            ack_stale_after: stale_after,
            active_no_link_progress_grace: dead_after,
            check_interval: Duration::from_millis(5),
            stale_log_interval: Duration::from_millis(10),
        }
    }

    fn watchdog_channel_session() -> (
        Arc<Session>,
        mpsc::Receiver<tp_core::protocol::PackedMessage>,
        mpsc::Receiver<()>,
    ) {
        let (out_tx, out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        (session, out_rx, closed_rx)
    }

    async fn wait_for_p2p_ingress_broker_to_take_one(engine: &Engine) {
        timeout(Duration::from_millis(100), async {
            loop {
                let capacity = engine
                    .p2p_signaling_ingress_tx
                    .lock()
                    .as_ref()
                    .expect("P2P signaling broker attached")
                    .capacity();
                if capacity == P2P_SIGNALING_INGRESS_BROKER_CAPACITY {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("P2P signaling broker should take the queued item");
    }

    type P2pWatchdogFixture = (
        Arc<crate::p2p::session::MultiSession>,
        Arc<Session>,
        mpsc::Receiver<tp_core::protocol::PackedMessage>,
        mpsc::Receiver<()>,
        mpsc::Receiver<()>,
    );

    fn p2p_watchdog_multi() -> P2pWatchdogFixture {
        let (relay, relay_out_rx, relay_closed_rx) = watchdog_channel_session();
        let (p2p, _p2p_out_rx, p2p_closed_rx) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        multi
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0x3b; 16]),
                "peer-watchdog".into(),
                p2p.clone(),
            )
            .expect("install p2p");
        (multi, p2p, relay_out_rx, relay_closed_rx, p2p_closed_rx)
    }

    #[test]
    fn target_udp_socket_uses_tuned_bind_path() {
        const SOCKET_COUNT: usize = 1_000;

        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind local UDP target");
        let target_addr = target.local_addr().expect("local UDP target addr");
        let mut sockets = Vec::with_capacity(SOCKET_COUNT);
        let mut local_tuples = BTreeSet::new();

        for _ in 0..SOCKET_COUNT {
            let socket = bind_target_udp_socket().expect("bind tuned target UDP socket");
            socket
                .connect(target_addr)
                .expect("connect target UDP socket");
            let local_tuple = socket.local_addr().expect("target UDP local tuple");
            assert_ne!(local_tuple.port(), 0);
            assert!(
                local_tuples.insert(local_tuple),
                "duplicate target UDP local tuple: {local_tuple}"
            );
            sockets.push(socket);
        }

        assert_eq!(sockets.len(), SOCKET_COUNT);
        assert_eq!(local_tuples.len(), SOCKET_COUNT);
    }

    #[test]
    fn transport_heartbeat_idle_relay_stays_open_after_stale_threshold() {
        let config = test_link_watchdog_config(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(
            evaluate_link_watchdog(
                LinkKind::Relay,
                config,
                LinkWatchdogSnapshot {
                    now_ms: 3999,
                    last_ack_ms: 1000,
                    last_link_progress_ms: 1000,
                    active_tcp_flows: 0,
                    active_udp_flows: 0,
                },
            ),
            LinkWatchdogDecision::Keep
        );
        assert!(matches!(
            evaluate_link_watchdog(
                LinkKind::Relay,
                config,
                LinkWatchdogSnapshot {
                    now_ms: 4000,
                    last_ack_ms: 1000,
                    last_link_progress_ms: 1000,
                    active_tcp_flows: 0,
                    active_udp_flows: 0,
                },
            ),
            LinkWatchdogDecision::KeepIdleStale
        ));
        assert!(matches!(
            evaluate_link_watchdog(
                LinkKind::Relay,
                config,
                LinkWatchdogSnapshot {
                    now_ms: 10_000,
                    last_ack_ms: 1000,
                    last_link_progress_ms: 1000,
                    active_tcp_flows: 0,
                    active_udp_flows: 0,
                },
            ),
            LinkWatchdogDecision::KeepIdleStale
        ));
    }

    #[tokio::test]
    async fn p2p_watchdog_idle_stale_closes_only_the_p2p_session() {
        use tp_core::protocol::unpack;

        let (multi, p2p, mut relay_out_rx, mut relay_closed_rx, mut p2p_closed_rx) =
            p2p_watchdog_multi();
        let cancel = CancellationToken::new();
        let active_flows = LinkActiveFlowCounters::new(Arc::new(LinkActiveFlows::default()));
        let established_ms = monotonic_millis();

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        tokio::spawn(run_p2p_link_watchdog(
            engine.clone(),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(established_ms)),
            Arc::new(AtomicU64::new(established_ms)),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config_with_ack_stale_after(
                Duration::from_millis(20),
                Duration::from_secs(5),
            ),
            cancel.clone(),
        ));

        timeout(Duration::from_millis(100), p2p_closed_rx.recv())
            .await
            .expect("idle stale P2P watchdog should promptly clear the direct path")
            .expect("P2P close signal");
        assert!(
            multi.p2p().is_none(),
            "idle stale P2P session must not remain installed"
        );
        assert!(
            timeout(Duration::from_millis(30), relay_closed_rx.recv())
                .await
                .is_err(),
            "P2P watchdog must not close relay handle"
        );
        assert!(
            multi.p2p_for_new_flow().is_none(),
            "stale P2P must not accept future traffic"
        );
        assert!(engine.p2p_refill_requested_for_test("peer-watchdog") > 0);

        multi
            .relay()
            .send(BinaryMessage::Heartbeat {
                client_id: "relay-still-open".into(),
                timestamp: 42,
            })
            .await
            .expect("relay sender should remain usable after P2P watchdog close");
        let relay_msg = timeout(Duration::from_millis(100), relay_out_rx.recv())
            .await
            .expect("relay outbound message")
            .expect("relay outbound channel open");
        assert!(matches!(
            unpack(&relay_msg.to_bytes()).expect("decode relay outbound"),
            BinaryMessage::Heartbeat { timestamp: 42, .. }
        ));
        cancel.cancel();
    }

    #[tokio::test]
    async fn p2p_watchdog_keeps_active_tcp_flow_pinned_when_ack_and_progress_are_stale() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        let active_flow_state = Arc::new(LinkActiveFlows::default());
        let _active_flow = active_flow_state
            .begin("tcp", "active-p2p-flow")
            .expect("track active P2P TCP flow");
        let active_flows = LinkActiveFlowCounters::new(active_flow_state);
        let cancel = CancellationToken::new();

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(monotonic_millis())),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config_with_ack_stale_after(
                Duration::from_millis(20),
                Duration::from_millis(160),
            ),
            cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(80), p2p_closed_rx.recv())
                .await
                .is_err(),
            "active P2P TCP flow should stay pinned instead of being closed by watchdog stale ACK"
        );
        assert!(multi.p2p().is_some(), "P2P session should remain installed");
        assert!(
            multi.p2p_for_new_flow().is_none(),
            "stale P2P with pinned TCP must stop accepting new flows"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn p2p_watchdog_counts_source_side_p2p_flows_as_active() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0x3b; 16]);
        let key = CandidateKey::p2p("app-1", session_id, "peer-watchdog", 0);
        engine
            .proxy_flow_registry
            .record_pending("source-p2p-flow", FlowKind::Tcp, key);
        engine
            .proxy_flow_registry
            .mark_established("source-p2p-flow");
        let active_flows = LinkActiveFlowCounters::with_source(
            Arc::new(LinkActiveFlows::default()),
            engine.p2p_source_active_flow_counter(
                "app-1".into(),
                session_id,
                "peer-watchdog".into(),
            ),
        );
        let cancel = CancellationToken::new();

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(monotonic_millis())),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config_with_ack_stale_after(
                Duration::from_millis(100),
                Duration::from_millis(120),
            ),
            cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(40), p2p_closed_rx.recv())
                .await
                .is_err(),
            "source-side P2P flows should use the watched P2P link's active grace"
        );
        assert!(multi.p2p().is_some(), "P2P session should remain installed");
        cancel.cancel();
    }

    #[tokio::test]
    async fn p2p_watchdog_source_tcp_flow_stream_link_progress_keeps_stale_ack_link_open() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0x3b; 16]);
        let key = CandidateKey::p2p("app-1", session_id, "peer-watchdog", 0);
        engine
            .proxy_flow_registry
            .record_pending("source-p2p-stream", FlowKind::Tcp, key.clone());
        engine
            .proxy_flow_registry
            .mark_established("source-p2p-stream");
        engine
            .proxy_flow_registry
            .record_link_io_progress_ms(&key, monotonic_millis());
        let active_flows = LinkActiveFlowCounters::with_source(
            Arc::new(LinkActiveFlows::default()),
            engine.p2p_source_active_flow_counter(
                "app-1".into(),
                session_id,
                "peer-watchdog".into(),
            ),
        );
        let cancel = CancellationToken::new();

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config_with_ack_stale_after(
                Duration::from_millis(100),
                Duration::from_millis(120),
            ),
            cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(40), p2p_closed_rx.recv())
                .await
                .is_err(),
            "recent source-side P2P tcp flow stream I/O should keep a stale-ACK P2P link open"
        );
        assert!(multi.p2p().is_some(), "P2P session should remain installed");
        cancel.cancel();
    }

    #[tokio::test]
    async fn p2p_watchdog_closes_active_udp_even_with_unrelated_multi_session_flow() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        let (active_tx, _active_rx) = mpsc::channel::<Bytes>(1);
        multi
            .inbound()
            .insert("unrelated-relay-or-other-p2p-flow".into(), active_tx);
        let active_flow_state = Arc::new(LinkActiveFlows::default());
        let _active_udp = active_flow_state
            .begin("udp", "watched-p2p-udp")
            .expect("track watched P2P UDP flow");
        let active_flows = LinkActiveFlowCounters::new(active_flow_state);
        let cancel = CancellationToken::new();

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config(Duration::from_millis(120)),
            cancel,
        ));

        timeout(Duration::from_millis(200), p2p_closed_rx.recv())
            .await
            .expect("active UDP stale P2P link should close after active grace")
            .expect("P2P close signal");
        assert!(multi.p2p().is_none(), "stale P2P session should be removed");
    }

    #[tokio::test]
    async fn p2p_watchdog_closes_active_udp_without_waiting_for_relay_grace() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        let active_flow_state = Arc::new(LinkActiveFlows::default());
        let _active_udp = active_flow_state
            .begin("udp", "watched-p2p-udp")
            .expect("track watched P2P UDP flow");
        let active_flows = LinkActiveFlowCounters::new(active_flow_state);
        let cancel = CancellationToken::new();

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config_with_ack_stale_after(
                Duration::from_millis(20),
                Duration::from_millis(60),
            ),
            cancel,
        ));

        timeout(Duration::from_millis(120), p2p_closed_rx.recv())
            .await
            .expect("active UDP stale P2P should close after the dead threshold")
            .expect("P2P close signal");
        assert!(multi.p2p().is_none(), "stale P2P session should be removed");
    }

    #[tokio::test]
    async fn p2p_watchdog_link_progress_keeps_stale_ack_link_open() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        let cancel = CancellationToken::new();
        let active_flows = LinkActiveFlowCounters::new(Arc::new(LinkActiveFlows::default()));

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(monotonic_millis())),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config_with_ack_stale_after(
                Duration::from_millis(100),
                Duration::from_millis(120),
            ),
            cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(40), p2p_closed_rx.recv())
                .await
                .is_err(),
            "recent P2P link progress should keep stale-ACK P2P link open"
        );
        assert!(multi.p2p().is_some(), "P2P session should remain installed");
        cancel.cancel();
    }

    #[tokio::test]
    async fn p2p_watchdog_ignores_relay_progress_for_p2p_liveness() {
        let (multi, p2p, _relay_out_rx, _relay_closed_rx, mut p2p_closed_rx) = p2p_watchdog_multi();
        multi.record_traffic_rx(TrafficPath::Relay, 64);
        let cancel = CancellationToken::new();
        let active_flow_state = Arc::new(LinkActiveFlows::default());
        let _active_udp = active_flow_state
            .begin("udp", "watched-p2p-udp")
            .expect("track watched P2P UDP flow");
        let active_flows = LinkActiveFlowCounters::new(active_flow_state);

        tokio::spawn(run_p2p_link_watchdog(
            Engine::new(EngineConfig::default(), Arc::new(NullListener)),
            multi.clone(),
            p2p,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            "peer-watchdog".into(),
            active_flows,
            multi.local_traffic(),
            p2p_watchdog_test_config(Duration::ZERO),
            cancel,
        ));

        timeout(Duration::from_millis(200), p2p_closed_rx.recv())
            .await
            .expect("relay progress must not keep stale P2P watchdog open")
            .expect("P2P close signal");
        assert!(multi.p2p().is_none(), "stale P2P session should be removed");
    }

    #[test]
    fn gateway_name_comes_only_from_platform_config() {
        let from_platform = TunnelConfig {
            gateway_name: Some(" gw-01 ".into()),
            gateway_addr: "203.0.113.88".into(),
            ..Default::default()
        };
        assert_eq!(
            effective_gateway_name(&from_platform).as_deref(),
            Some("gw-01")
        );

        let without_name = TunnelConfig {
            gateway_addr: "sz-01.gt.example.net".into(),
            ..Default::default()
        };
        assert_eq!(effective_gateway_name(&without_name), None);
    }

    #[tokio::test]
    async fn replica_reconnect_retries_after_failure_and_returns_ok() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let activity = ReplicaActivity::new(2, None);
        let group = ReplicaReconnectGroup::new(2);
        let cancel = CancellationToken::new();
        let result = timeout(
            Duration::from_millis(100),
            run_reconnecting_replica(
                "client-1".into(),
                ReplicaReconnectPolicy::new(Duration::from_millis(1), Duration::from_millis(1)),
                group,
                activity,
                cancel,
                || {
                    let attempts = attempts.clone();
                    async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            Err(anyhow::anyhow!("first failure"))
                        } else {
                            Ok(())
                        }
                    }
                },
            ),
        )
        .await
        .expect("replica reconnect helper should finish after retry");

        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn canonical_initial_replica_attempt_consumes_transport_generation() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let gateway = GatewayBootstrapV2 {
            transport: tp_core::config::TRANSPORT_TYPE_WEBSOCKET.into(),
            dial_address: "127.0.0.1".into(),
            port: 1,
            mapping_port: None,
            tls_server_name: None,
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let profile = Arc::new(owner.add_peer(None, 1, None).expect("Peer"));
        let source = GatewayAttachmentSource::new(profile, None);
        let generation = engine
            .prepare_gateway_attachment(&source)
            .await
            .expect("V2 Gateway Attachment");
        let client_id = generation.tunnel_config.client_ids[0].clone();
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let cancel = CancellationToken::new();

        let outcome = timeout(
            Duration::from_secs(2),
            engine.clone().run_gateway_attachment_once(
                generation.tunnel_config,
                generation.attachment,
                &mut stop_rx,
                &cancel,
            ),
        )
        .await
        .expect("local refused dial should fail promptly")
        .expect("failed Replica dial returns a session outcome");
        assert!(matches!(outcome, SessionOutcome::Failed(_)));
        assert_eq!(
            engine.next_relay_transport_generation(&client_id),
            2,
            "canonical initial attempt must consume generation 1 even when dialing fails"
        );
    }

    #[tokio::test]
    async fn canonical_reconnect_attempts_receive_distinct_transport_generations() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let generations = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let activity = ReplicaActivity::new(2, None);
        let group = ReplicaReconnectGroup::new(2);
        let cancel = CancellationToken::new();

        let result = run_reconnecting_replica(
            "client-generation".into(),
            ReplicaReconnectPolicy::new(Duration::from_millis(1), Duration::from_millis(1)),
            group,
            activity,
            cancel,
            {
                let engine = engine.clone();
                let generations = generations.clone();
                move || {
                    let generation = engine.next_relay_transport_generation("client-generation");
                    let generations = generations.clone();
                    async move {
                        let mut seen = generations.lock();
                        seen.push(generation);
                        if seen.len() == 1 {
                            Err(anyhow::anyhow!("first attempt failed"))
                        } else {
                            Ok(())
                        }
                    }
                }
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(*generations.lock(), vec![1, 2]);
    }

    #[tokio::test]
    async fn replica_reconnect_returns_failure_when_all_replicas_are_down() {
        let activity = ReplicaActivity::new(2, None);
        let group = ReplicaReconnectGroup::new(2);
        let policy =
            ReplicaReconnectPolicy::new(Duration::from_millis(100), Duration::from_millis(100));
        let cancel = CancellationToken::new();
        let (first_attempt_tx, mut first_attempt_rx) = mpsc::channel::<()>(1);
        let mut first_attempt_tx = Some(first_attempt_tx);

        let first = tokio::spawn(run_reconnecting_replica(
            "client-1".into(),
            policy,
            group.clone(),
            activity.clone(),
            cancel.clone(),
            move || {
                let first_attempt_tx = first_attempt_tx.take();
                async move {
                    if let Some(tx) = first_attempt_tx {
                        let _ = tx.send(()).await;
                    }
                    Err(anyhow::anyhow!("first replica down"))
                }
            },
        ));

        first_attempt_rx
            .recv()
            .await
            .expect("first replica should enter retry before group failure");

        let second = run_reconnecting_replica(
            "client-2".into(),
            policy,
            group,
            activity,
            cancel,
            || async { Err(anyhow::anyhow!("second replica down")) },
        );
        let second_err = timeout(Duration::from_millis(100), second)
            .await
            .expect("second replica should trip all-down group failure")
            .expect_err("second replica should return failure");
        assert_eq!(second_err.to_string(), "second replica down");

        let first_err = timeout(Duration::from_millis(100), first)
            .await
            .expect("all-down cancellation should wake first replica retry")
            .expect("first task should join")
            .expect_err("first replica should return failure");
        assert_eq!(first_err.to_string(), "first replica down");
    }

    #[tokio::test]
    async fn replica_reconnect_exits_promptly_when_cancelled_during_backoff() {
        let activity = ReplicaActivity::new(2, None);
        let group = ReplicaReconnectGroup::new(2);
        let cancel = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_task = attempts.clone();
        let cancel_for_task = cancel.clone();

        let task = tokio::spawn(run_reconnecting_replica(
            "client-1".into(),
            ReplicaReconnectPolicy::new(Duration::from_secs(30), Duration::from_secs(30)),
            group,
            activity,
            cancel_for_task,
            move || {
                let attempts = attempts_for_task.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("still down"))
                }
            },
        ));

        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        let result = timeout(Duration::from_secs(1), task)
            .await
            .expect("cancel should wake reconnect backoff")
            .expect("reconnect helper should not panic");
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn replica_intake_reader_drain_is_bounded_when_aux_channel_stays_open() {
        struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropNotify {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let reader = AbortOnDropHandle::new(tokio::spawn(async {}));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let tcp_flow_reader = AbortOnDropHandle::new(tokio::spawn(async move {
            let _drop_notify = DropNotify(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        }));
        started_rx.await.expect("aux reader should start");

        timeout(
            Duration::from_millis(50),
            drain_replica_intake_readers(
                Some(reader),
                Some(tcp_flow_reader),
                None,
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("replica intake drain must be bounded");
        timeout(Duration::from_millis(100), dropped_rx)
            .await
            .expect("timed-out aux reader should be aborted")
            .expect("aux reader drop notification");
    }

    #[tokio::test]
    async fn replica_intake_reader_drain_does_not_repoll_completed_main_reader() {
        let mut reader = AbortOnDropHandle::new(tokio::spawn(async {}));
        let cancel = CancellationToken::new();
        let main_reader_finished = tokio::select! {
            result = &mut reader => {
                result.expect("main reader should complete normally");
                true
            }
            _ = cancel.cancelled() => false,
        };
        let pending_main_reader = if main_reader_finished {
            drop(reader);
            None
        } else {
            Some(reader)
        };

        drain_replica_intake_readers(pending_main_reader, None, None, Duration::from_millis(10))
            .await;
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_exits_on_cancel_without_closing_stale_session() {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis()));
        let cancel = CancellationToken::new();
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            sender,
            last_ack,
            Arc::new(AtomicU64::new(monotonic_millis())),
            "client-1".into(),
            test_active_counters(active_tcp_map(), active_udp_map()),
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config(Duration::from_secs(3), Duration::from_millis(10)),
            cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(50), closed_rx.recv())
                .await
                .is_err(),
            "missing transport heartbeat ack alone must not tear down an idle session"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[test]
    fn production_transport_heartbeat_watchdog_recovers_stale_paths_quickly() {
        let config = LinkWatchdogConfig::production();
        assert_eq!(config.ack_stale_after, Duration::from_secs(3));
        assert_eq!(
            config.active_no_link_progress_grace,
            Duration::from_secs(30)
        );
        assert_eq!(config.check_interval, Duration::from_secs(1));
        assert_eq!(config.stale_log_interval, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_stale_session_with_business_progress() {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis()));
        sleep(Duration::from_millis(5)).await;
        let active_tcp = Arc::new(DashMap::<String, mpsc::Sender<Bytes>>::new());
        let (tcp_tx, _tcp_rx) = mpsc::channel::<Bytes>(1);
        active_tcp.insert("active-tcp".into(), tcp_tx);
        let active_udp = Arc::new(DashMap::<String, DropOldestSender<Bytes>>::new());
        let traffic = Arc::new(TrafficCounters::default());
        let cancel = CancellationToken::new();
        let progress_cancel = cancel.clone();
        let relay_progress = Arc::new(AtomicU64::new(monotonic_millis()));
        let progress_timestamp = relay_progress.clone();
        let progress = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = progress_cancel.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {
                        progress_timestamp.store(monotonic_millis(), Ordering::Relaxed);
                    }
                }
            }
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            sender,
            last_ack,
            relay_progress,
            "client-1".into(),
            test_active_counters(active_tcp, active_udp),
            traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(10)),
                Duration::from_millis(20),
            ),
            cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(50), closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "stale heartbeat ACK must not close a relay session while inbound data still progresses"
        );
        cancel.cancel();
        progress
            .await
            .expect("progress task should exit after cancel");
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_active_tcp_flow_pinned_after_stale_ack_and_no_progress_grace(
    ) {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis()));
        sleep(Duration::from_millis(5)).await;
        let active_tcp = Arc::new(DashMap::<String, mpsc::Sender<Bytes>>::new());
        let (tcp_tx, _tcp_rx) = mpsc::channel::<Bytes>(1);
        active_tcp.insert("stalled-tcp".into(), tcp_tx);
        let active_udp = Arc::new(DashMap::<String, DropOldestSender<Bytes>>::new());
        let cancel = CancellationToken::new();
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            sender,
            last_ack,
            Arc::new(AtomicU64::new(monotonic_millis())),
            "client-1".into(),
            test_active_counters(active_tcp, active_udp),
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config_with_dead_after(
                Duration::from_millis(1),
                Duration::from_millis(120),
                Duration::from_millis(10),
            ),
            cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(80), closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "active TCP flow handles must stay pinned; watchdog cannot prove link death from stale ACK plus app-level no-progress alone"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn relay_watchdog_source_tcp_flow_stream_link_progress_keeps_stale_ack_link_open() {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let key = CandidateKey::relay("client-1", 0);
        engine.proxy_flow_registry.record_pending(
            "source-relay-stream",
            FlowKind::Tcp,
            key.clone(),
        );
        engine
            .proxy_flow_registry
            .mark_established("source-relay-stream");
        engine
            .proxy_flow_registry
            .record_link_io_progress_ms(&key, monotonic_millis());
        let active_flows = LinkActiveFlowCounters::with_source(
            Arc::new(LinkActiveFlows::default()),
            engine.relay_source_active_flow_counter("client-1".into(), 0),
        );
        let cancel = CancellationToken::new();

        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            "client-1".into(),
            active_flows,
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config_with_dead_after(
                Duration::ZERO,
                Duration::from_millis(120),
                Duration::from_millis(10),
            ),
            cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(40), closed_rx.recv())
                .await
                .is_err(),
            "recent source-side relay tcp flow stream I/O should keep a stale-ACK relay link open"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[test]
    fn relay_source_counter_aggregates_peer_scoped_counts_and_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let peer_a = CandidateKey::relay_to_peer("client-1", 7, "peer-a");
        let peer_b = CandidateKey::relay_to_peer("client-1", 7, "peer-b");
        let other_generation = CandidateKey::relay_to_peer("client-1", 8, "peer-c");

        engine
            .proxy_flow_registry
            .record_pending("peer-a-tcp", FlowKind::Tcp, peer_a.clone());
        engine
            .proxy_flow_registry
            .record_pending("peer-b-udp", FlowKind::Udp, peer_b.clone());
        engine.proxy_flow_registry.record_pending(
            "other-generation-tcp",
            FlowKind::Tcp,
            other_generation.clone(),
        );
        engine
            .proxy_flow_registry
            .record_link_io_progress_ms(&peer_a, 100);
        engine
            .proxy_flow_registry
            .record_link_io_progress_ms(&peer_b, 200);
        engine
            .proxy_flow_registry
            .record_link_io_progress_ms(&other_generation, 300);

        let snapshot = engine.relay_source_active_flow_counter("client-1".into(), 7)();
        assert_eq!(snapshot.active_tcp_flows, 1);
        assert_eq!(snapshot.active_udp_flows, 1);
        assert_eq!(
            snapshot.last_link_io_progress_ms, 200,
            "Relay attachment source progress must use the same local client and generation"
        );
    }

    #[tokio::test]
    async fn relay_source_counter_reports_explicit_tcp_flow_stream_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let key = CandidateKey::relay_to_peer("client-1", 7, "peer-a");
        engine
            .proxy_flow_registry
            .record_pending("relay-flow-stream", FlowKind::Tcp, key.clone());
        engine
            .proxy_flow_registry
            .mark_established("relay-flow-stream");

        assert_eq!(
            engine.relay_source_active_flow_counter("client-1".into(), 7)()
                .last_link_io_progress_ms,
            0
        );
        engine.record_proxy_flow_link_io_progress("relay-flow-stream");
        sleep(Duration::from_millis(2)).await;
        engine.record_proxy_flow_link_io_progress("relay-flow-stream");

        let exact_progress = engine.proxy_flow_last_link_io_progress_for_test(&key);
        assert_ne!(
            exact_progress, 0,
            "an actual TCP flow-stream write must record same-link progress"
        );
        assert_eq!(
            engine.relay_source_active_flow_counter("client-1".into(), 7)()
                .last_link_io_progress_ms,
            exact_progress,
            "the Relay attachment snapshot must expose the exact flow-stream progress"
        );
    }

    #[test]
    fn old_generation_source_flow_does_not_pin_new_relay_attachment() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.proxy_flow_registry.record_pending(
            "old-generation-tcp",
            FlowKind::Tcp,
            CandidateKey::relay_to_peer("client-1", 1, "peer-a"),
        );

        let old_snapshot = engine.relay_source_active_flow_counter("client-1".into(), 1)();
        let new_snapshot = engine.relay_source_active_flow_counter("client-1".into(), 2)();
        assert_eq!(old_snapshot.active_tcp_flows, 1);
        assert_eq!(new_snapshot.active_tcp_flows, 0);
        assert_eq!(
            evaluate_link_watchdog(
                LinkKind::Relay,
                LinkWatchdogConfig::production(),
                LinkWatchdogSnapshot {
                    now_ms: 40_000,
                    last_ack_ms: 0,
                    last_link_progress_ms: 0,
                    active_tcp_flows: new_snapshot.active_tcp_flows,
                    active_udp_flows: new_snapshot.active_udp_flows,
                },
            ),
            LinkWatchdogDecision::KeepIdleStale,
            "old-generation TCP must not pin the new Relay attachment"
        );
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_tcp_flow_stream_pinned_after_stale_ack_and_no_progress_grace(
    ) {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis()));
        sleep(Duration::from_millis(5)).await;
        let active_flow_state = Arc::new(LinkActiveFlows::default());
        let _active_flow = active_flow_state
            .begin("tcp", "stalled-tcp-flow-stream")
            .expect("track active TCP flow stream");
        let cancel = CancellationToken::new();
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(Arc::new(
            tp_transport::session::Session::send_only_from_sender(sender.clone()),
        ));
        let watchdog = tokio::spawn(run_relay_link_watchdog_with_tcp_streams(
            engine,
            multi,
            sender,
            last_ack,
            Arc::new(AtomicU64::new(monotonic_millis())),
            "client-1".into(),
            0,
            LinkActiveFlowCounters::new(active_flow_state),
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config_with_dead_after(
                Duration::from_millis(1),
                Duration::from_millis(120),
                Duration::from_millis(10),
            ),
            cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(80), closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "per-flow TCP streams are closed by transport or stream errors, not by heartbeat stale ACK alone"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_pins_active_tcp_streams_beyond_active_grace() {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis()));
        sleep(Duration::from_millis(5)).await;
        let active_flow_state = Arc::new(LinkActiveFlows::default());
        let _active_flow = active_flow_state
            .begin("tcp", "active-tcp-flow-stream")
            .expect("track active TCP flow stream");
        let cancel = CancellationToken::new();
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(Arc::new(
            tp_transport::session::Session::send_only_from_sender(sender.clone()),
        ));
        let watchdog = tokio::spawn(run_relay_link_watchdog_with_tcp_streams(
            engine,
            multi,
            sender,
            last_ack,
            Arc::new(AtomicU64::new(monotonic_millis())),
            "client-1".into(),
            0,
            LinkActiveFlowCounters::new(active_flow_state),
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config_with_dead_after(
                Duration::from_millis(1),
                Duration::from_millis(200),
                Duration::from_millis(10),
            ),
            cancel.clone(),
        ));

        let early_close = timeout(Duration::from_millis(40), closed_rx.recv()).await;
        assert!(
            early_close.is_err(),
            "active TCP streams should stay pinned instead of being treated as idle stale"
        );
        let eventual_close = timeout(Duration::from_millis(120), closed_rx.recv()).await;
        assert!(
            eventual_close.is_err(),
            "watchdog must not kill active TCP streams after active-flow grace without a transport or stream error"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_session_before_stale_threshold() {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis().saturating_sub(1000)));
        let cancel = CancellationToken::new();
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            sender,
            last_ack,
            Arc::new(AtomicU64::new(monotonic_millis())),
            "client-1".into(),
            test_active_counters(active_tcp_map(), active_udp_map()),
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config(Duration::from_secs(3), Duration::from_millis(10)),
            cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(50), closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "heartbeat ACK age below stale threshold must not close the relay session"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_idle_session_when_heartbeat_is_stale() {
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = closed_tx.try_send(());
        });
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let (sender, _rx, _dg) = session.split();
        let last_ack = Arc::new(AtomicU64::new(monotonic_millis()));
        sleep(Duration::from_millis(5)).await;
        let cancel = CancellationToken::new();
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            sender,
            last_ack,
            Arc::new(AtomicU64::new(monotonic_millis())),
            "client-1".into(),
            test_active_counters(active_tcp_map(), active_udp_map()),
            Arc::new(TrafficCounters::default()),
            test_link_watchdog_config_with_dead_after(
                Duration::from_millis(1),
                Duration::from_millis(200),
                Duration::from_millis(10),
            ),
            cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(100), closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "idle stale heartbeat ACK must not close the relay session"
        );
        cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_rechecks_progress_before_close() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let relay_progress = fixture.relay_last_link_progress_ms.clone();
        let hook: WatchdogPreCloseHook = Arc::new(move || {
            relay_progress.store(monotonic_millis(), Ordering::Relaxed);
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog_with_pre_close_hook(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(1),
            ),
            fixture.cancel.clone(),
            hook,
        ));

        let close = timeout(Duration::from_millis(40), fixture.closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "fresh progress observed by the final recheck must cancel the close"
        );
        fixture.cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_rechecks_active_flows_before_close() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let active_tcp = fixture.active_tcp.clone();
        let hook: WatchdogPreCloseHook = Arc::new(move || {
            let (tcp_tx, _tcp_rx) = mpsc::channel::<Bytes>(1);
            active_tcp.insert("fresh-active-flow".into(), tcp_tx);
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog_with_pre_close_hook(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(1),
            ),
            fixture.cancel.clone(),
            hook,
        ));

        let close = timeout(Duration::from_millis(40), fixture.closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "a TCP flow found by the final recheck pins the relay session until normal stream or transport closure"
        );
        fixture.cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_rechecks_tcp_flow_preface_progress_before_close() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let relay_progress = fixture.relay_last_link_progress_ms.clone();
        let hook: WatchdogPreCloseHook = Arc::new(move || {
            relay_progress.store(monotonic_millis(), Ordering::Relaxed);
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog_with_pre_close_hook(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(1),
            ),
            fixture.cancel.clone(),
            hook,
        ));

        let close = timeout(Duration::from_millis(40), fixture.closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "fresh TCP flow stream preface progress observed by the final recheck must cancel the close"
        );
        fixture.cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_session_when_business_progress_is_recent() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let progress_cancel = fixture.cancel.clone();
        let progress_timestamp = fixture.relay_last_link_progress_ms.clone();
        let progress = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = progress_cancel.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {
                        progress_timestamp.store(monotonic_millis(), Ordering::Relaxed);
                    }
                }
            }
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(20),
            ),
            fixture.cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(50), fixture.closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "stale heartbeat ACK must not close while inbound traffic keeps progressing"
        );
        fixture.cancel.cancel();
        progress
            .await
            .expect("progress task should exit after cancel");
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_closes_when_only_outbound_enqueue_progresses() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let progress_cancel = fixture.cancel.clone();
        let progress_traffic = fixture.traffic.clone();
        let progress = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = progress_cancel.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {
                        progress_traffic.mark_progress();
                    }
                }
            }
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(20),
            ),
            fixture.cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(80), fixture.closed_rx.recv()).await;
        assert!(
            close.is_ok(),
            "local outbound enqueue progress must not mask a broken relay path without heartbeat ACKs or inbound data"
        );
        fixture.cancel.cancel();
        progress
            .await
            .expect("progress task should exit after cancel");
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_closes_when_only_p2p_rx_progresses() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let progress_cancel = fixture.cancel.clone();
        let progress_traffic = fixture.traffic.clone();
        let progress = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = progress_cancel.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {
                        progress_traffic.record_rx(TrafficPath::P2p, 1);
                    }
                }
            }
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(20),
            ),
            fixture.cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(80), fixture.closed_rx.recv()).await;
        assert!(
            close.is_ok(),
            "P2P RX must not keep the relay watchdog open"
        );
        fixture.cancel.cancel();
        progress
            .await
            .expect("progress task should exit after cancel");
        watchdog
            .await
            .expect("watchdog task should exit after close");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_keeps_business_idle_session_when_control_ack_is_fresh() {
        let mut fixture = watchdog_fixture();
        let ack_cancel = fixture.cancel.clone();
        let last_ack = fixture.last_ack.clone();
        let ack_refresh = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = ack_cancel.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {
                        last_ack.store(monotonic_millis(), Ordering::Relaxed);
                    }
                }
            }
        });
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(20), Duration::from_millis(5)),
                Duration::from_millis(1),
            ),
            fixture.cancel.clone(),
        ));

        let close = timeout(Duration::from_millis(70), fixture.closed_rx.recv()).await;
        assert!(
            close.is_err(),
            "business-idle sessions must stay open while heartbeat ACKs remain fresh"
        );
        fixture.cancel.cancel();
        ack_refresh
            .await
            .expect("ack refresh task should exit after cancel");
        watchdog
            .await
            .expect("watchdog task should exit after cancel");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_closes_only_after_no_ack_and_no_progress_grace() {
        let mut fixture = watchdog_fixture();
        insert_active_udp(&fixture.active_udp, "active-udp");
        sleep(Duration::from_millis(5)).await;
        let watchdog = tokio::spawn(run_transport_heartbeat_watchdog(
            fixture.sender,
            fixture.last_ack,
            fixture.relay_last_link_progress_ms,
            "client-1".into(),
            test_active_counters(fixture.active_tcp, fixture.active_udp),
            fixture.traffic,
            with_active_link_progress_grace(
                test_link_watchdog_config(Duration::from_millis(1), Duration::from_millis(5)),
                Duration::from_millis(40),
            ),
            fixture.cancel.clone(),
        ));

        assert!(
            timeout(Duration::from_millis(25), fixture.closed_rx.recv())
                .await
                .is_err(),
            "stale ACK alone must not close before the no-progress grace expires"
        );
        assert!(
            timeout(Duration::from_millis(80), fixture.closed_rx.recv())
                .await
                .is_ok(),
            "watchdog should close after both ACK and progress are stale"
        );
        fixture.cancel.cancel();
        watchdog
            .await
            .expect("watchdog task should exit after close");
    }

    #[tokio::test]
    async fn relay_udp_blackhole_closes_after_grace_despite_continuous_local_outbound_enqueue() {
        let mut fixture = watchdog_fixture();
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let key = CandidateKey::relay_to_peer("client-1", 7, "peer-a");
        engine
            .proxy_flow_registry
            .record_pending("blackholed-udp", FlowKind::Udp, key);
        engine.mark_proxy_flow_established("blackholed-udp");
        let active_flows = LinkActiveFlowCounters::with_source(
            Arc::new(LinkActiveFlows::default()),
            engine.relay_source_active_flow_counter("client-1".into(), 7),
        );
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(Arc::new(
            Session::send_only_from_sender(fixture.sender.clone()),
        ));
        let established_progress = Arc::new(AtomicU64::new(monotonic_millis()));
        let enqueue_cancel = fixture.cancel.clone();
        let enqueue_engine = engine.clone();
        let enqueue = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = enqueue_cancel.cancelled() => return,
                    _ = sleep(Duration::from_millis(2)) => {
                        enqueue_engine.record_proxy_flow_outbound_payload_bytes(
                            "blackholed-udp",
                            FlowKind::Udp,
                            1200,
                        );
                    }
                }
            }
        });
        let watchdog = tokio::spawn(run_relay_link_watchdog_inner(
            engine,
            multi,
            fixture.sender,
            Arc::new(AtomicU64::new(0)),
            established_progress,
            "client-1".into(),
            7,
            active_flows,
            fixture.traffic,
            test_link_watchdog_config_with_dead_after(
                Duration::from_millis(1),
                Duration::from_millis(40),
                Duration::from_millis(5),
            ),
            fixture.cancel.clone(),
            None,
        ));

        assert!(
            timeout(Duration::from_millis(25), fixture.closed_rx.recv())
                .await
                .is_err(),
            "Relay UDP must retain the full no-progress grace"
        );
        timeout(Duration::from_millis(80), fixture.closed_rx.recv())
            .await
            .expect("continuous local UDP enqueue must not keep a blackholed Relay open")
            .expect("Relay close signal");

        fixture.cancel.cancel();
        enqueue.await.expect("enqueue task exits after cancel");
        watchdog.await.expect("watchdog exits after close");
    }

    #[tokio::test]
    async fn transport_heartbeat_watchdog_counts_connect_response_and_close_as_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let inbound = active_tcp_map();
        let udp_inbound = active_udp_map();
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            inbound.clone(),
            udp_inbound.clone(),
        );
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();

        let before = multi.local_traffic().progress_snapshot();
        let (pending_tx, pending_rx) = oneshot::channel();
        engine
            .proxy_pending()
            .insert("connect-response".into(), pending_tx);
        engine
            .handle_msg(
                BinaryMessage::ConnectResponse {
                    conn_id: "connect-response".into(),
                    success: true,
                    error: String::new(),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;
        pending_rx
            .await
            .expect("pending connect response should resolve")
            .expect("connect response should be successful");
        let after_connect = multi.local_traffic().progress_snapshot();
        assert_ne!(
            before, after_connect,
            "resolving a pending ConnectResponse must advance the watchdog progress epoch"
        );
        engine
            .handle_msg(
                BinaryMessage::ConnectResponse {
                    conn_id: "connect-response".into(),
                    success: true,
                    error: String::new(),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;
        assert_eq!(
            after_connect,
            multi.local_traffic().progress_snapshot(),
            "late ConnectResponse without a pending waiter must not count as watchdog progress"
        );

        let (tcp_tx, _tcp_rx) = mpsc::channel::<Bytes>(1);
        inbound.insert("known-close".into(), tcp_tx);
        engine
            .handle_msg(
                BinaryMessage::Close {
                    conn_id: "known-close".into(),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;
        let after_close = multi.local_traffic().progress_snapshot();
        assert_ne!(
            after_connect, after_close,
            "removing a known Close slot must advance the watchdog progress epoch"
        );

        engine
            .handle_msg(
                BinaryMessage::Close {
                    conn_id: "unknown-close".into(),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;
        assert_eq!(
            after_close,
            multi.local_traffic().progress_snapshot(),
            "Close for an unknown conn_id must not count as watchdog progress"
        );
    }

    #[tokio::test]
    async fn tcp_flow_stream_relay_rx_updates_link_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target_addr = listener.local_addr().expect("target listener addr");
        let target = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("target accept");
            let mut buf = [0u8; 4];
            socket.read_exact(&mut buf).await.expect("target read");
            assert_eq!(&buf, b"ping");
        });

        let inbound = active_tcp_map();
        let udp_inbound = active_udp_map();
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi =
            crate::p2p::session::MultiSession::new_with_existing_maps(relay, inbound, udp_inbound);
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let initial_progress = u64::MAX;
        let link_progress = Arc::new(AtomicU64::new(initial_progress));
        let preface = tp_core::protocol::TcpFlowStreamPreface {
            conn_id: "tcp-flow-progress".into(),
            network: "tcp".into(),
            address: target_addr.to_string(),
        };
        let (flow_io, mut peer_io) = tokio::io::duplex(4096);
        let incoming = tp_transport::TcpFlowIncoming {
            preface: preface.clone(),
            stream: tp_transport::TcpFlowStream::new(preface, Box::pin(flow_io)),
        };

        let flow_engine = engine.clone();
        let flow_progress = link_progress.clone();
        let flow = tokio::spawn(async move {
            Engine::handle_tcp_flow_stream(
                flow_engine,
                incoming,
                multi,
                host_filter,
                TrafficPath::Relay,
                TcpFlowLinkContext {
                    p2p_source_session: None,
                    link_progress_ms: Some(flow_progress),
                    link_active_flow: None,
                },
            )
            .await;
        });

        let frame = tp_transport::session::read_tcp_flow_frame(&mut peer_io)
            .await
            .expect("connect response frame");
        match tp_core::protocol::unpack(&frame).expect("connect response") {
            BinaryMessage::ConnectResponse { success, error, .. } => {
                assert!(success, "connect response failed: {error}");
            }
            other => panic!("unexpected tcp flow response: {other:?}"),
        }

        peer_io.write_all(b"ping").await.expect("write peer bytes");
        peer_io.shutdown().await.expect("shutdown peer");

        timeout(Duration::from_millis(200), async {
            loop {
                if link_progress.load(Ordering::Relaxed) != initial_progress {
                    return;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("relay TCP flow bytes should refresh link progress");

        flow.await.expect("tcp flow task should finish");
        target.await.expect("target task should finish");
    }

    #[tokio::test]
    async fn tcp_flow_stream_counts_active_while_target_connect_is_pending() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let inbound = active_tcp_map();
        let udp_inbound = active_udp_map();
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi =
            crate::p2p::session::MultiSession::new_with_existing_maps(relay, inbound, udp_inbound);
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let link_progress = Arc::new(AtomicU64::new(u64::MAX));
        let preface = tp_core::protocol::TcpFlowStreamPreface {
            conn_id: "tcp-flow-connect-pending".into(),
            network: "tcp".into(),
            address: "203.0.113.1:9".into(),
        };
        let (flow_io, _peer_io) = tokio::io::duplex(4096);
        let incoming = tp_transport::TcpFlowIncoming {
            preface: preface.clone(),
            stream: tp_transport::TcpFlowStream::new(preface, Box::pin(flow_io)),
        };

        let flow_engine = engine.clone();
        let flow_multi = multi.clone();
        let flow = tokio::spawn(async move {
            Engine::handle_tcp_flow_stream(
                flow_engine,
                incoming,
                flow_multi,
                host_filter,
                TrafficPath::Relay,
                TcpFlowLinkContext {
                    p2p_source_session: None,
                    link_progress_ms: Some(link_progress),
                    link_active_flow: None,
                },
            )
            .await;
        });

        timeout(Duration::from_millis(200), async {
            loop {
                if multi.active_tcp_flow_streams().load(Ordering::Relaxed) > 0 {
                    return;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("TCP flow stream should count active before target connect completes");

        flow.abort();
        let _ = flow.await;
    }

    #[tokio::test]
    async fn tcp_flow_copy_reports_progress_before_peer_eof() {
        let (mut left_app, mut left_stream) = tokio::io::duplex(1024);
        let (mut right_stream, mut right_app) = tokio::io::duplex(1024);
        let left_to_right = Arc::new(AtomicUsize::new(0));
        let right_to_left = Arc::new(AtomicUsize::new(0));
        let left_progress = left_to_right.clone();
        let right_progress = right_to_left.clone();

        let bridge = tokio::spawn(async move {
            copy_bidirectional_with_progress(
                &mut left_stream,
                &mut right_stream,
                |_| {},
                |n| {
                    left_progress.fetch_add(n, Ordering::SeqCst);
                },
                |_| {},
                |n| {
                    right_progress.fetch_add(n, Ordering::SeqCst);
                },
            )
            .await
        });

        const LEFT_PAYLOAD: &[u8] = b"client-to-target";
        left_app.write_all(LEFT_PAYLOAD).await.expect("write left");
        let mut received = vec![0u8; LEFT_PAYLOAD.len()];
        right_app
            .read_exact(&mut received)
            .await
            .expect("read right");
        assert_eq!(received, LEFT_PAYLOAD);
        assert_eq!(
            left_to_right.load(Ordering::SeqCst),
            LEFT_PAYLOAD.len(),
            "active TCP flow bytes must advance watchdog-visible progress before EOF"
        );

        const RIGHT_PAYLOAD: &[u8] = b"target-to-client";
        right_app
            .write_all(RIGHT_PAYLOAD)
            .await
            .expect("write right");
        let mut received = vec![0u8; RIGHT_PAYLOAD.len()];
        left_app.read_exact(&mut received).await.expect("read left");
        assert_eq!(received, RIGHT_PAYLOAD);
        assert_eq!(
            right_to_left.load(Ordering::SeqCst),
            RIGHT_PAYLOAD.len(),
            "reverse TCP flow bytes must also advance progress before EOF"
        );

        left_app.shutdown().await.expect("shutdown left");
        right_app.shutdown().await.expect("shutdown right");
        let (from_left, from_right) = timeout(Duration::from_secs(1), bridge)
            .await
            .expect("bridge should finish after both peers close")
            .expect("bridge task should not panic")
            .expect("bridge should not error");
        assert_eq!(from_left, LEFT_PAYLOAD.len() as u64);
        assert_eq!(from_right, RIGHT_PAYLOAD.len() as u64);
    }

    #[tokio::test]
    async fn udp_datagram_before_connect_is_buffered_for_pending_udp_channel() {
        let engine = Arc::new(Engine::new(EngineConfig::default(), Arc::new(NullListener)));
        let inbound = active_tcp_map();
        let udp_inbound = active_udp_map();
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            inbound.clone(),
            udp_inbound.clone(),
        );
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();

        engine
            .handle_msg(
                BinaryMessage::UdpData {
                    conn_id: "udp-race".into(),
                    payload: Bytes::from_static(b"start"),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;

        assert_eq!(
            multi.drain_pending_udp_inbound("udp-race"),
            vec![crate::p2p::session::PendingUdpInbound {
                path: TrafficPath::Relay,
                payload: Bytes::from_static(b"start"),
            }],
            "QUIC DATAGRAM payloads that beat the UDP Connect stream must not be dropped"
        );
    }

    #[tokio::test]
    async fn udp_close_keeps_inbound_slot_briefly_for_late_datagrams() {
        let engine = Arc::new(Engine::new(EngineConfig::default(), Arc::new(NullListener)));
        let inbound = active_tcp_map();
        let udp_inbound = active_udp_map();
        let (out_tx, _out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(8);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(8);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("peer addr");
        let relay = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            inbound.clone(),
            udp_inbound.clone(),
        );
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();
        let (udp_tx, mut udp_rx) = tp_transport::drop_oldest_channel::<Bytes>(8);
        udp_inbound.insert("udp-close-race".into(), udp_tx);

        engine
            .handle_msg(
                BinaryMessage::Close {
                    conn_id: "udp-close-race".into(),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;

        assert!(
            udp_inbound.contains_key("udp-close-race"),
            "UDP Close must not remove the inbound slot before late datagrams drain"
        );

        engine
            .handle_msg(
                BinaryMessage::UdpData {
                    conn_id: "udp-close-race".into(),
                    payload: Bytes::from_static(b"late"),
                },
                &multi,
                &inbound,
                &udp_inbound,
                &host_filter,
                &tracker,
                None,
                None,
            )
            .await;

        assert_eq!(
            timeout(Duration::from_secs(1), udp_rx.recv())
                .await
                .expect("late datagram should be routed before close drain expires"),
            Some(Bytes::from_static(b"late"))
        );

        sleep(UDP_CLOSE_DRAIN_GRACE + Duration::from_millis(100)).await;
        assert!(
            !udp_inbound.contains_key("udp-close-race"),
            "UDP close drain must still release the inbound slot"
        );
    }

    /// `Engine::disconnect()` must be safe to call on an Engine that was
    /// constructed but never `connect`ed — the Tauri reconnect path relies
    /// on this to drain a previous engine before replacing it, without
    /// having to track whether that engine was ever started.
    #[tokio::test]
    async fn disconnect_on_unconnected_engine_is_noop() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.disconnect().await;
        engine.disconnect().await;
        let s = engine.status();
        assert!(!s.connected);
        assert!(!s.connecting);
        assert_eq!(s.message, "Disconnected");
    }

    #[tokio::test]
    async fn disconnect_resets_transient_status_fields() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_status(ConnectionStatus {
            connected: true,
            gateway_addr: Some("203.0.113.88:8443".into()),
            message: "Connected".into(),
            platform_heartbeat: HeartbeatStatus {
                active: true,
                last_time: Some(123),
                last_error: None,
            },
            transport_heartbeat: HeartbeatStatus {
                active: true,
                last_time: Some(124),
                last_error: None,
            },
            ..Default::default()
        });

        engine.disconnect().await;

        let s = engine.status();
        assert!(!s.connected);
        assert!(!s.connecting);
        assert_eq!(s.message, "Disconnected");
        assert_eq!(s.gateway_addr, None);
        assert_eq!(s.platform_heartbeat, HeartbeatStatus::default());
        assert_eq!(s.transport_heartbeat, HeartbeatStatus::default());
        assert_eq!(s.path_mode, ConnectionPathMode::Disconnected);
    }

    #[test]
    fn parent_cancelled_replica_teardown_preserves_user_disconnect_status() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let activity = ReplicaActivity::new(1, None);

        activity.mark_connected(&engine, "127.0.0.1:1".parse().unwrap());
        engine.set_status(ConnectionStatus {
            connected: false,
            connecting: false,
            message: "Disconnected".into(),
            ..Default::default()
        });

        activity.mark_disconnected(&engine, true);

        let status = engine.status();
        assert!(!status.connected);
        assert!(!status.connecting);
        assert_eq!(status.message, "Disconnected");
        assert!(!status.transport_heartbeat.active);
    }

    #[test]
    fn gateway_closed_replica_teardown_reports_gateway_disconnect() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let activity = ReplicaActivity::new(1, None);

        activity.mark_connected(&engine, "127.0.0.1:1".parse().unwrap());
        activity.mark_disconnected(&engine, false);

        let status = engine.status();
        assert!(!status.connected);
        assert!(!status.connecting);
        assert_eq!(status.message, "Gateway disconnected");
        assert!(!status.transport_heartbeat.active);
    }

    #[tokio::test]
    async fn v2_runtime_path_and_gateway_loss_follow_live_lanes_not_legacy_heartbeat() {
        use crate::runtime_snapshot::{
            V2GatewayAttachmentPhase, V2OverallPhase, V2PeerPath, V2RemotePeerPhase,
            V2RuntimeReasonCode,
        };
        use tp_core::p2p_types::{CertFingerprint, SessionId};
        use tp_core::peer_link_crypto::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};

        fn session_keys(
            source: &PeerProfileV2,
            target: &PeerProfileV2,
            session_id: SessionId,
        ) -> tp_core::peer_link_crypto::PeerLinkSessionKeysV2 {
            let source_secret = PeerLinkEphemeralSecretV2::generate();
            let target_secret = PeerLinkEphemeralSecretV2::generate();
            let offer = P2pOfferV2::sign(
                source,
                session_id,
                target.peer.peer_id.clone(),
                Vec::new(),
                CertFingerprint::from_bytes([0x51; 32]),
                &source_secret,
            )
            .expect("Offer");
            let answer = P2pAnswerV2::sign(
                target,
                &offer,
                true,
                0,
                Vec::new(),
                CertFingerprint::from_bytes([0x52; 32]),
                &target_secret,
            )
            .expect("Answer");
            source_secret
                .derive_session_keys(&offer, &answer, &source.tunnel_signing_public_key)
                .expect("source keys")
        }

        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let remote = owner.add_peer(None, 1, None).expect("remote Peer");
        let relay_only = owner.add_peer(None, 1, None).expect("Relay-only Peer");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(local.clone());
        engine.begin_v2_runtime(&local);
        engine.initialize_v2_peer_gossip();
        engine
            .install_v2_peer_membership(&remote.public_membership())
            .expect("signed membership");
        engine
            .install_v2_peer_membership(&relay_only.public_membership())
            .expect("signed Relay-only membership");
        engine.commit_v2_membership_cycle(&[
            remote.peer.peer_id.clone(),
            relay_only.peer.peer_id.clone(),
        ]);

        let direct_session_id = SessionId::from_bytes([0x53; 16]);
        let relay_only_session_id = SessionId::from_bytes([0x54; 16]);
        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let (direct, _direct_rx, _direct_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let relation = crate::peer_link_manager::PeerRelationKey::from_stable_peers(
            &local.peer.peer_id,
            &remote.peer.peer_id,
            0,
        )
        .expect("stable Peer relation");
        multi
            .install_p2p_session_for_relation(
                direct_session_id,
                remote.peer.peer_id.clone(),
                direct.clone(),
                Some(relation),
            )
            .expect("Direct Lane");
        engine.install_proxy_replica_session_for_test("local-runtime-0", multi.clone());
        engine
            .install_v2_peer_link(
                remote.peer.peer_id.clone(),
                direct_session_id,
                session_keys(&local, &remote, direct_session_id),
            )
            .expect("Direct PeerLink");
        engine
            .install_v2_peer_link(
                relay_only.peer.peer_id.clone(),
                relay_only_session_id,
                session_keys(&local, &relay_only, relay_only_session_id),
            )
            .expect("Relay-only PeerLink");

        let activity = ReplicaActivity::new(1, None);
        activity.mark_connected(&engine, "127.0.0.1:8443".parse().unwrap());
        let empty_record = crate::peer_runtime::PeerRuntimeRecordV2::new(Vec::new())
            .expect("empty Runtime record");
        for peer_id in [&remote.peer.peer_id, &relay_only.peer.peer_id] {
            engine.receive_v2_gossip(
                peer_id,
                crate::relay_crypto::RelayControlPayloadV2::RuntimeRecord(empty_record.encode()),
            );
        }
        assert_eq!(
            engine
                .v2_runtime_snapshot()
                .peer_directory
                .peers
                .iter()
                .find(|peer| peer.peer_id == remote.peer.peer_id)
                .and_then(|peer| peer.current_path),
            Some(V2PeerPath::Direct)
        );
        assert_eq!(
            engine
                .v2_runtime_snapshot()
                .peer_directory
                .peers
                .iter()
                .find(|peer| peer.peer_id == relay_only.peer.peer_id)
                .and_then(|peer| peer.current_path),
            Some(V2PeerPath::EncryptedRelay)
        );

        engine.unregister_relay_closed_multi_session("local-runtime-0", &multi);
        let relay_closed = engine.v2_runtime_snapshot();
        assert_eq!(relay_closed.overall.phase, V2OverallPhase::Degraded);
        assert_eq!(
            relay_closed
                .peer_directory
                .peers
                .iter()
                .find(|peer| peer.peer_id == remote.peer.peer_id)
                .and_then(|peer| peer.current_path),
            Some(V2PeerPath::Direct),
            "Relay unregister must preserve the independent Direct Lane"
        );
        let relay_only_after_unregister = relay_closed
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == relay_only.peer.peer_id)
            .expect("Relay-only Peer row");
        assert_eq!(
            relay_only_after_unregister.phase,
            V2RemotePeerPhase::Unavailable
        );
        assert_eq!(relay_only_after_unregister.current_path, None);

        activity.mark_disconnected(&engine, false);
        let preserved = engine.v2_runtime_snapshot();
        assert_eq!(
            preserved.gateway_attachment.phase,
            V2GatewayAttachmentPhase::Unavailable
        );
        assert_eq!(
            preserved.gateway_attachment.reason_code,
            Some(V2RuntimeReasonCode::GatewayUnavailable)
        );
        assert_eq!(preserved.overall.phase, V2OverallPhase::Degraded);
        let preserved_direct = preserved
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == remote.peer.peer_id)
            .expect("Direct Peer remains known");
        assert_eq!(preserved_direct.current_path, Some(V2PeerPath::Direct));
        let unavailable_relay = preserved
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == relay_only.peer.peer_id)
            .expect("Relay-only Peer remains visible");
        assert_eq!(unavailable_relay.phase, V2RemotePeerPhase::Unavailable);
        assert_eq!(unavailable_relay.current_path, None);

        engine.mark_v2_gateway_resolving();
        assert_eq!(
            engine.v2_runtime_snapshot().overall.phase,
            V2OverallPhase::Degraded
        );
        engine.mark_v2_gateway_connecting(&GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "replacement.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("replacement.example".into()),
            trusted_certificate_pem: None,
        });
        assert_eq!(
            engine.v2_runtime_snapshot().overall.phase,
            V2OverallPhase::Degraded
        );
        engine.mark_v2_runtime_failure(&anyhow::anyhow!("replacement Gateway unavailable"));
        assert_eq!(
            engine.v2_runtime_snapshot().overall.phase,
            V2OverallPhase::Degraded
        );
        assert_eq!(
            engine.v2_runtime_snapshot().gateway_attachment.reason_code,
            Some(V2RuntimeReasonCode::GatewayConnectFailed),
            "a surviving Direct Lane must not hide the actionable Gateway failure"
        );

        assert!(multi.mark_p2p_session_unusable_for_new_flows_for_handle(&direct));
        engine.mark_v2_peer_direct_closed(&remote.peer.peer_id);
        let unavailable = engine.v2_runtime_snapshot();
        assert_eq!(unavailable.overall.phase, V2OverallPhase::Blocked);
        let unavailable_direct = unavailable
            .peer_directory
            .peers
            .iter()
            .find(|peer| peer.peer_id == remote.peer.peer_id)
            .expect("Direct Peer remains visible after lane closes");
        assert_eq!(unavailable_direct.phase, V2RemotePeerPhase::Unavailable);
        assert_eq!(unavailable_direct.current_path, None);
        assert_eq!(
            unavailable_direct.reason_code,
            Some(V2RuntimeReasonCode::NoUsablePeerPath)
        );
    }

    #[tokio::test]
    async fn cancelled_replica_publish_does_not_register_or_mark_connected() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let activity = ReplicaActivity::new(1, None);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (relay, _out_rx, _closed_rx) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );

        let published = engine.publish_connected_replica_if_active(
            &cancel,
            "client-1",
            "group-1",
            multi,
            0,
            &activity,
            "127.0.0.1:1".parse().unwrap(),
        );

        assert!(!published);
        assert!(engine.replica_sessions.lock().is_empty());
        let status = engine.status();
        assert!(!status.connected);
        assert_eq!(status.message, "Disconnected");
    }

    #[test]
    fn engine_status_reports_connected_uptime_and_relay_mode() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.connected_since.lock() = Some(std::time::Instant::now() - Duration::from_secs(65));

        engine.set_status(ConnectionStatus {
            connected: true,
            message: "Connected".into(),
            ..Default::default()
        });

        let status = engine.status();
        assert_eq!(status.path_mode, ConnectionPathMode::Relay);
        assert!(status.uptime_secs >= 65, "got {}", status.uptime_secs);
    }

    #[tokio::test]
    async fn engine_status_reports_p2p_degraded_when_only_sidecar_has_direct_session() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        fn make_multi(
            relay: Arc<tp_transport::session::Session>,
        ) -> Arc<crate::p2p::session::MultiSession> {
            crate::p2p::session::MultiSession::new_with_relay_only(relay)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let anchor = make_multi(channel_session());
        let sidecar = make_multi(channel_session());
        let session_id = tp_core::p2p_types::SessionId::from_bytes([7u8; 16]);
        sidecar
            .install_p2p_session(session_id, "pc-main".into(), channel_session())
            .expect("install p2p");
        sidecar.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });

        engine.install_proxy_replica_session_for_test("app-1", anchor);
        engine.install_proxy_replica_session_for_test("app-1-1", sidecar);
        engine.set_status(ConnectionStatus {
            connected: true,
            message: "Connected".into(),
            ..Default::default()
        });

        let status = engine.status();
        assert_eq!(status.path_mode, ConnectionPathMode::P2p);
        assert_eq!(status.p2p_active_sessions, 1);
        assert_eq!(status.p2p_state.as_deref(), Some("degraded"));
        assert_eq!(status.p2p_peer_count, 1);
        assert_eq!(status.p2p_primary_peer_id.as_deref(), Some("pc-main"));
    }

    #[tokio::test]
    async fn engine_status_reports_relay_when_installed_p2p_is_not_usable_for_new_flows() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(channel_session());
        let session_id = tp_core::p2p_types::SessionId::from_bytes([8u8; 16]);
        let p2p = channel_session();
        multi
            .install_p2p_session(session_id, "pc-stale".into(), p2p.clone())
            .expect("install p2p");
        multi.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });
        assert!(multi.mark_p2p_session_unusable_for_new_flows_for_handle(&p2p));

        engine.install_proxy_replica_session_for_test("app-1", multi);
        engine.set_status(ConnectionStatus {
            connected: true,
            message: "Connected".into(),
            ..Default::default()
        });

        let status = engine.status();
        assert_eq!(status.path_mode, ConnectionPathMode::Relay);
        assert_eq!(status.p2p_active_sessions, 0);
        assert_eq!(status.p2p_state.as_deref(), Some("degraded"));
        assert_eq!(status.p2p_peer_count, 0);
        assert_eq!(status.p2p_primary_peer_id, None);
    }

    #[tokio::test]
    async fn peer_direct_health_uses_only_exact_eligible_paths_across_replicas() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (relay_b, _, _) = watchdog_channel_session();
        let (direct_b, _, _) = watchdog_channel_session();
        let multi_b = crate::p2p::session::MultiSession::new_with_relay_only(relay_b);
        multi_b
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0xb1; 16]),
                "peer-b-AbCd0002-0".into(),
                direct_b.clone(),
            )
            .expect("install Peer B direct path");
        let (relay_c, _, _) = watchdog_channel_session();
        let (direct_c, _, _) = watchdog_channel_session();
        let multi_c = crate::p2p::session::MultiSession::new_with_relay_only(relay_c);
        multi_c
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0xc1; 16]),
                "peer-c-AbCd0003-0".into(),
                direct_c,
            )
            .expect("install Peer C direct path");
        engine.install_proxy_replica_session_for_test("peer-a-AbCd0001-0", multi_b.clone());
        engine.install_proxy_replica_session_for_test("peer-a-AbCd0001-1", multi_c);

        assert!(engine.has_healthy_direct_path_for_peer("peer-b-AbCd0002-0"));
        assert!(engine.has_healthy_direct_path_for_peer("peer-c-AbCd0003-0"));
        assert!(!engine.has_healthy_direct_path_for_peer("peer-d-AbCd0004-0"));

        assert!(multi_b.mark_p2p_session_unusable_for_new_flows_for_handle(&direct_b));
        assert!(!engine.has_healthy_direct_path_for_peer("peer-b-AbCd0002-0"));
        assert!(engine.has_healthy_direct_path_for_peer("peer-c-AbCd0003-0"));
    }

    #[tokio::test]
    async fn proxy_relay_lane_round_robins_new_flows_across_replicas() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        fn make_multi() -> Arc<crate::p2p::session::MultiSession> {
            crate::p2p::session::MultiSession::new_with_relay_only(channel_session())
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-1", make_multi());
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-0", make_multi());
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-2", make_multi());

        let expected = [
            "tunA-AppRnd01-1",
            "tunA-AppRnd01-0",
            "tunA-AppRnd01-2",
            "tunA-AppRnd01-1",
        ];
        for expected_client_id in expected {
            let lane = engine.pick_proxy_relay_lane().expect("proxy lane");
            assert_eq!(
                lane.local_client_id, expected_client_id,
                "new proxy flows should rotate across connected relay lanes"
            );
            assert_eq!(lane.multi.p2p_session_count(), 0);
        }
    }

    #[tokio::test]
    async fn public_p2p_tcp_load_does_not_make_healthy_p2p_use_relay() {
        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi =
            crate::p2p::session::MultiSession::new_with_relay_only(channel_session("127.0.0.1:0"));
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xC1; 16]);
        multi
            .install_p2p_session(
                session_id,
                "pc-public-0".into(),
                channel_session("8.8.8.8:443"),
            )
            .expect("install public p2p");
        multi.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });
        engine.install_proxy_replica_session_for_test("app-public-0", multi);

        let p2p_key = CandidateKey::p2p("app-public-0", session_id, "pc-public-0", 0);
        for idx in 0..30 {
            let conn_id = format!("existing-public-tcp-{idx}");
            engine.proxy_flow_registry.record_pending(
                conn_id.clone(),
                FlowKind::Tcp,
                p2p_key.clone(),
            );
            engine.proxy_flow_registry.mark_established(&conn_id);
        }

        let lane = engine
            .pick_proxy_flow_lane(FlowKind::Tcp, &[])
            .expect("flow lane");
        assert_eq!(
            lane.path,
            PathKind::P2p,
            "healthy public P2P must stay preferred even under TCP load; relay is only for unavailable or failed P2P"
        );
    }

    #[tokio::test]
    async fn stale_p2p_is_not_a_candidate_for_new_proxy_flows() {
        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi =
            crate::p2p::session::MultiSession::new_with_relay_only(channel_session("127.0.0.1:0"));
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xC2; 16]);
        let p2p = channel_session("8.8.8.8:443");
        multi
            .install_p2p_session(session_id, "pc-public-0".into(), p2p.clone())
            .expect("install public p2p");
        assert!(multi.mark_p2p_session_unusable_for_new_flows_for_handle(&p2p));
        engine.install_proxy_replica_session_for_test("app-public-0", multi);

        let lane = engine
            .pick_proxy_flow_lane(FlowKind::Tcp, &[])
            .expect("flow lane");
        assert_eq!(
            lane.path,
            PathKind::Relay,
            "stale P2P must not block new traffic from using relay"
        );
        assert_eq!(lane.p2p_session_id, None);
    }

    #[tokio::test]
    async fn target_peer_scopes_p2p_candidates_and_pins_the_matching_relay_replica() {
        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi =
            crate::p2p::session::MultiSession::new_with_relay_only(channel_session("127.0.0.1:0"));
        let peer_b_session = tp_core::p2p_types::SessionId::from_bytes([0xB1; 16]);
        let peer_c_session = tp_core::p2p_types::SessionId::from_bytes([0xC1; 16]);
        multi
            .install_p2p_session(
                peer_b_session,
                "mesh-RemoteB1-1".into(),
                channel_session("192.0.2.10:443"),
            )
            .expect("install Peer B");
        multi
            .install_p2p_session(
                peer_c_session,
                "mesh-RemoteC1-1".into(),
                channel_session("192.0.2.11:443"),
            )
            .expect("install Peer C");
        engine.install_proxy_replica_session_for_test("mesh-Local001-1", multi);

        let direct = engine
            .pick_proxy_flow_lane_for_peer(FlowKind::Tcp, &[], Some("mesh-RemoteB1-0"), false)
            .expect("Peer B direct lane");
        assert_eq!(direct.path, PathKind::P2p);
        assert_eq!(
            direct.candidate_key.peer_family.as_deref(),
            Some("mesh-RemoteB1-0")
        );
        assert_eq!(
            direct.target_peer_client_id.as_deref(),
            Some("mesh-RemoteB1-1")
        );

        let relay = engine
            .pick_proxy_flow_lane_for_peer(
                FlowKind::Tcp,
                &[ProxyFlowAttemptExclude::path(PathKind::P2p)],
                Some("mesh-RemoteB1-0"),
                false,
            )
            .expect("Peer B relay lane");
        assert_eq!(relay.path, PathKind::Relay);
        assert_eq!(
            relay.candidate_key.peer_family.as_deref(),
            Some("mesh-RemoteB1-0")
        );
        assert_eq!(
            relay.target_peer_client_id.as_deref(),
            Some("mesh-RemoteB1-1"),
            "relay target must use the same replica index as its local lane"
        );
    }

    #[test]
    fn overlay_destination_resolution_is_exact_and_ambiguous_routes_fail_closed() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install stable Peer B overlay");
        let overlay = crate::overlay::overlay_ipv4_for_replica_id("mesh", "mesh-RemoteB1-0")
            .expect("derive Peer B overlay");

        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{overlay}:27015"))
                .expect("exact route lookup"),
            Some("mesh-RemoteB1-0".to_string())
        );
        assert_eq!(
            engine
                .resolve_overlay_peer("203.0.113.8:27015")
                .expect("ordinary Internet target remains unmatched"),
            None
        );

        engine.install_overlay_peer_for_test("mesh-Collision1-0", overlay);
        let error = engine
            .resolve_overlay_peer(&format!("{overlay}:27015"))
            .expect_err("duplicate ownership must not choose an arbitrary Peer");
        assert!(error.to_string().contains("ambiguous exact Peer route"));
    }

    #[tokio::test]
    async fn v2_private_lan_export_selects_exact_peer_and_unmatched_target_fails_closed() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway.clone()).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let peer_b = owner.add_peer(None, 1, None).expect("Peer B");
        let peer_c = owner.add_peer(None, 1, None).expect("Peer C");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_active_v2_peer_profile_for_test(local.clone());
        engine.set_latest_tunnel_config_for_test(v2_tunnel_config(
            &local,
            &gateway,
            vec![format!("{}-Local001-0", local.tunnel_id)],
        ));
        engine
            .install_v2_peer_membership(&peer_b.public_membership())
            .expect("signed Peer B membership");
        engine
            .install_v2_peer_membership(&peer_c.public_membership())
            .expect("signed Peer C membership");

        let alias = "192.168.50.9".parse().expect("private host");
        let prefix =
            crate::peer_runtime::LanExportPrefixV2::new(alias, 32).expect("private host Export");
        let ready_record =
            crate::peer_runtime::PeerRuntimeRecordV2::new(vec![crate::peer_runtime::LanExportV2 {
                prefix,
                ready: true,
            }])
            .expect("ready Runtime record");
        engine
            .overlay_routes
            .write()
            .replace_v2_lan_export_origin(&peer_b.peer.peer_id, ready_record.clone())
            .expect("Peer B private host Export");

        assert_eq!(
            engine
                .resolve_proxy_target_peer("192.168.50.9:39002")
                .await
                .expect("exact private host route")
                .peer_id,
            Some(peer_b.peer.peer_id.clone())
        );
        assert!(engine
            .resolve_proxy_target_peer("192.168.50.10:39002")
            .await
            .expect_err("an unadvertised neighbour must fail closed")
            .to_string()
            .contains("no exact Peer route"));

        engine
            .overlay_routes
            .write()
            .replace_v2_lan_export_origin(&peer_c.peer.peer_id, ready_record)
            .expect("Peer C standby host Export");
        assert_eq!(
            engine
                .resolve_proxy_target_peer("192.168.50.9:39002")
                .await
                .expect("first-seen Export route")
                .peer_id,
            Some(peer_b.peer.peer_id.clone()),
            "the first ready origin is ActiveHere"
        );
        assert_eq!(
            engine.v2_active_lan_export_snapshot(),
            vec![(prefix, peer_b.peer.peer_id.clone())]
        );

        assert!(engine.retire_overlay_peer(&peer_b.peer.peer_id));
        assert_eq!(
            engine
                .resolve_proxy_target_peer("192.168.50.9:39002")
                .await
                .unwrap()
                .peer_id,
            Some(peer_c.peer.peer_id.clone()),
            "the standby origin becomes ActiveHere after retirement"
        );

        assert!(engine.retire_overlay_peer(&peer_c.peer.peer_id));
        assert!(engine
            .resolve_proxy_target_peer("192.168.50.9:39002")
            .await
            .expect_err("retired final Export must fail closed")
            .to_string()
            .contains("no exact Peer route"));
    }

    #[test]
    fn retiring_peer_removes_overlay_lookup_and_os_route_snapshot() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let overlay = engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install route");
        assert_eq!(engine.overlay_route_cidrs(), vec![format!("{overlay}/32")]);

        assert!(engine.retire_overlay_peer("mesh-RemoteB1-0"));

        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{overlay}:27015"))
                .expect("lookup after retirement"),
            None
        );
        assert!(engine.overlay_route_cidrs().is_empty());
    }

    #[tokio::test]
    async fn disconnect_removes_runtime_overlay_routes() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let overlay = engine
            .install_overlay_replica("mesh", "mesh-RemoteB1-0")
            .expect("install route");
        assert!(engine
            .resolve_overlay_peer(&format!("{overlay}:9"))
            .expect("lookup before disconnect")
            .is_some());

        engine.disconnect().await;

        assert_eq!(
            engine
                .resolve_overlay_peer(&format!("{overlay}:9"))
                .expect("lookup after disconnect"),
            None
        );
    }

    #[tokio::test]
    async fn p2p_path_attempt_exclude_places_tcp_on_relay() {
        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi =
            crate::p2p::session::MultiSession::new_with_relay_only(channel_session("127.0.0.1:0"));
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xC3; 16]);
        multi
            .install_p2p_session(
                session_id,
                "pc-public-0".into(),
                channel_session("8.8.4.4:443"),
            )
            .expect("install public p2p");
        multi.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });
        engine.install_proxy_replica_session_for_test("app-public-0", multi);

        let lane = engine
            .pick_proxy_flow_lane(
                FlowKind::Tcp,
                &[ProxyFlowAttemptExclude::path(PathKind::P2p)],
            )
            .expect("flow lane");
        assert_eq!(
            lane.path,
            PathKind::Relay,
            "after a P2P open timeout this CONNECT must fall back to relay instead of probing more P2P lanes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_public_p2p_tcp_placement_reserves_before_next_pick() {
        use std::collections::HashMap;

        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        for replica in 0..6 {
            let multi = crate::p2p::session::MultiSession::new_with_relay_only(channel_session(
                "127.0.0.1:0",
            ));
            let mut sid_bytes = [0u8; 16];
            sid_bytes[0] = 0xD0;
            sid_bytes[15] = replica;
            let session_id = tp_core::p2p_types::SessionId::from_bytes(sid_bytes);
            multi
                .install_p2p_session(
                    session_id,
                    format!("pc-public-{replica}"),
                    channel_session(&format!("8.8.8.{}:443", replica + 1)),
                )
                .expect("install public p2p");
            multi.set_state(crate::p2p::session::P2pState::Active {
                session_id,
                since: std::time::Instant::now(),
            });
            let local_client_id = format!("app-public-{replica}");
            engine.install_proxy_replica_session_for_test(&local_client_id, multi);
        }

        let start = Arc::new(tokio::sync::Barrier::new(31));
        let mut tasks = Vec::new();
        for idx in 0..30 {
            let engine = engine.clone();
            let start = start.clone();
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                engine
                    .pick_and_record_proxy_flow_lane(
                        &format!("burst-tcp-{idx}"),
                        FlowKind::Tcp,
                        &[],
                    )
                    .expect("flow lane")
                    .candidate_key
            }));
        }
        start.wait().await;

        let mut p2p_per_replica: HashMap<String, usize> = HashMap::new();
        let mut p2p_total = 0;
        let mut relay_total = 0;
        for task in tasks {
            let key = task.await.expect("placement task");
            match key.path {
                CandidatePath::P2p => {
                    p2p_total += 1;
                    *p2p_per_replica.entry(key.local_client_id).or_default() += 1;
                }
                CandidatePath::Relay => relay_total += 1,
            }
        }

        assert_eq!(p2p_total, 30);
        assert_eq!(relay_total, 0);
        assert_eq!(p2p_per_replica.len(), 6);
        for (replica, count) in p2p_per_replica {
            assert!(
                count > 0,
                "{replica} should receive some public P2P TCP flows"
            );
        }
    }

    #[tokio::test]
    async fn lan_p2p_tcp_load_keeps_using_p2p() {
        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi =
            crate::p2p::session::MultiSession::new_with_relay_only(channel_session("127.0.0.1:0"));
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xC2; 16]);
        multi
            .install_p2p_session(
                session_id,
                "pc-lan-0".into(),
                channel_session("192.168.1.20:443"),
            )
            .expect("install lan p2p");
        multi.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });
        engine.install_proxy_replica_session_for_test("app-lan-0", multi);

        let p2p_key = CandidateKey::p2p("app-lan-0", session_id, "pc-lan-0", 0);
        for idx in 0..30 {
            let conn_id = format!("existing-lan-tcp-{idx}");
            engine.proxy_flow_registry.record_pending(
                conn_id.clone(),
                FlowKind::Tcp,
                p2p_key.clone(),
            );
            engine.proxy_flow_registry.mark_established(&conn_id);
        }

        let lane = engine
            .pick_proxy_flow_lane(FlowKind::Tcp, &[])
            .expect("flow lane");
        assert_eq!(
            lane.path,
            PathKind::P2p,
            "LAN P2P must not inherit the public P2P TCP cap"
        );
    }

    #[tokio::test]
    async fn p2p_udp_placement_avoids_lane_with_prior_datagram_drops() {
        fn channel_session(peer: &str) -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(8);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = peer.parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        fn p2p_multi(
            session_id: tp_core::p2p_types::SessionId,
            peer_client_id: &str,
            p2p_session: Arc<tp_transport::session::Session>,
        ) -> Arc<crate::p2p::session::MultiSession> {
            let multi = crate::p2p::session::MultiSession::new_with_relay_only(channel_session(
                "127.0.0.1:0",
            ));
            multi
                .install_p2p_session(session_id, peer_client_id.into(), p2p_session)
                .expect("install p2p");
            multi.set_state(crate::p2p::session::P2pState::Active {
                session_id,
                since: std::time::Instant::now(),
            });
            multi
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let bad_session = channel_session("192.168.0.20:443");
        bad_session
            .stats_handle()
            .dropped_full
            .store(100, Ordering::Relaxed);

        let bad_id = tp_core::p2p_types::SessionId::from_bytes([0xD1; 16]);
        let good_id = tp_core::p2p_types::SessionId::from_bytes([0xD2; 16]);
        engine.install_proxy_replica_session_for_test(
            "app-0",
            p2p_multi(bad_id, "pc-0", bad_session),
        );
        engine.install_proxy_replica_session_for_test(
            "app-1",
            p2p_multi(good_id, "pc-1", channel_session("192.168.0.21:443")),
        );

        let lane = engine
            .pick_proxy_flow_lane(FlowKind::Udp, &[])
            .expect("flow lane");
        assert_eq!(
            lane.local_client_id, "app-1",
            "new UDP flows should avoid a P2P lane that already reported datagram drops"
        );
    }

    #[tokio::test]
    async fn engine_resolves_local_client_id_for_replica_multi_session() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let multi = crate::p2p::session::MultiSession::new_with_relay_only(channel_session());
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("tunA-AppRnd01-7", multi.clone());

        assert_eq!(
            engine.local_client_id_for_multi(&multi).as_deref(),
            Some("tunA-AppRnd01-7")
        );
    }

    #[tokio::test]
    async fn engine_status_reports_degraded_when_some_replicas_lack_p2p() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        fn make_multi(
            relay: Arc<tp_transport::session::Session>,
        ) -> Arc<crate::p2p::session::MultiSession> {
            crate::p2p::session::MultiSession::new_with_relay_only(relay)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let active = make_multi(channel_session());
        let relay_only = make_multi(channel_session());
        let session_id = tp_core::p2p_types::SessionId::from_bytes([8u8; 16]);
        active
            .install_p2p_session(session_id, "pc-main".into(), channel_session())
            .expect("install p2p");
        active.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });

        engine.install_proxy_replica_session_for_test("app-1", active);
        engine.install_proxy_replica_session_for_test("app-1-1", relay_only);
        engine.set_replicas_for_test(2);
        engine.set_status(ConnectionStatus {
            connected: true,
            message: "Connected".into(),
            ..Default::default()
        });

        let status = engine.status();
        assert_eq!(status.path_mode, ConnectionPathMode::P2p);
        assert_eq!(status.p2p_active_sessions, 1);
        assert_eq!(status.p2p_state.as_deref(), Some("degraded"));
    }

    #[tokio::test]
    async fn engine_status_counts_p2p_inbound_payload_bytes() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let relay = channel_session();
        let p2p = channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0u8; 16]);
        multi
            .install_p2p_session(session_id, "peer-main".into(), p2p.clone())
            .expect("install p2p");
        let (tx, mut rx) = mpsc::channel::<Bytes>(1);
        multi.inbound().insert("conn-rx".into(), tx);
        engine.install_proxy_replica_session_for_test("app-1", multi);

        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::Data {
                conn_id: "conn-rx".into(),
                payload: Bytes::from_static(b"hello"),
            })
            .await;

        assert_eq!(rx.recv().await.as_deref(), Some(&b"hello"[..]));
        assert_eq!(engine.status().traffic.p2p_rx_bytes, 5);
    }

    #[tokio::test]
    async fn p2p_payload_for_existing_relay_tcp_conn_is_accepted_within_same_replica() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let relay = channel_session();
        let p2p = channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        multi
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0u8; 16]),
                "peer-main".into(),
                p2p,
            )
            .expect("install p2p");
        let (tx, mut rx) = mpsc::channel::<Bytes>(1);
        multi.inbound().insert("relay-born".into(), tx);
        engine.install_proxy_replica_session_for_test("app-1", multi.clone());

        engine
            .handle_msg_from_p2p_for_test(BinaryMessage::Data {
                conn_id: "relay-born".into(),
                payload: Bytes::from_static(b"resume"),
            })
            .await;

        let delivered = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("P2P recovery payload should be delivered within 500ms");
        assert_eq!(delivered.as_deref(), Some(&b"resume"[..]));
    }

    #[tokio::test]
    async fn engine_status_uses_first_replica_peer_as_primary() {
        fn channel_session() -> Arc<tp_transport::session::Session> {
            let (out_tx, _out_rx) =
                tokio::sync::mpsc::channel::<tp_core::protocol::PackedMessage>(1);
            let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(tp_transport::session::Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let primary = crate::p2p::session::MultiSession::new_with_relay_only(channel_session());
        let secondary = crate::p2p::session::MultiSession::new_with_relay_only(channel_session());
        primary
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0x01; 16]),
                "peer-primary".into(),
                channel_session(),
            )
            .expect("install primary");
        secondary
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0x02; 16]),
                "aaa-secondary-would-sort-first".into(),
                channel_session(),
            )
            .expect("install secondary");

        engine.install_proxy_replica_session_for_test("app-1", primary);
        engine.install_proxy_replica_session_for_test("app-2", secondary);
        engine.set_status(ConnectionStatus {
            connected: true,
            message: "Connected".into(),
            ..Default::default()
        });

        let status = engine.status();
        assert_eq!(status.p2p_peer_count, 2);
        assert_eq!(status.p2p_primary_peer_id.as_deref(), Some("peer-primary"));
    }

    #[test]
    fn engine_status_clears_uptime_when_disconnected() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.connected_since.lock() = Some(std::time::Instant::now() - Duration::from_secs(65));

        engine.set_status(ConnectionStatus {
            connected: false,
            connecting: false,
            message: "Disconnected".into(),
            ..Default::default()
        });

        let status = engine.status();
        assert_eq!(status.path_mode, ConnectionPathMode::Disconnected);
        assert_eq!(status.uptime_secs, 0);
    }

    #[tokio::test]
    async fn multi_replica_group_waits_until_every_replica_fails() {
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (err_tx, mut err_rx) = mpsc::channel::<anyhow::Error>(3);
        let cancel = CancellationToken::new();

        err_tx.send(anyhow::anyhow!("replica-1")).await.unwrap();

        let first_failure = timeout(
            Duration::from_millis(25),
            wait_for_replica_group_outcome(3, &mut stop_rx, &mut err_rx, &cancel),
        )
        .await;
        assert!(
            first_failure.is_err(),
            "one failed replica must not end the group"
        );

        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (err_tx, mut err_rx) = mpsc::channel::<anyhow::Error>(3);
        let cancel = CancellationToken::new();
        err_tx.send(anyhow::anyhow!("replica-2")).await.unwrap();
        err_tx.send(anyhow::anyhow!("replica-3")).await.unwrap();
        err_tx.send(anyhow::anyhow!("replica-4")).await.unwrap();

        let outcome = wait_for_replica_group_outcome(3, &mut stop_rx, &mut err_rx, &cancel).await;
        match outcome {
            SessionOutcome::Failed(e) => assert_eq!(e.to_string(), "replica-4"),
            SessionOutcome::UserCancel => panic!("expected replica group failure"),
        }
    }

    #[tokio::test]
    async fn multi_replica_group_returns_user_cancel_on_engine_cancel() {
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (_err_tx, mut err_rx) = mpsc::channel::<anyhow::Error>(3);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = timeout(
            Duration::from_secs(1),
            wait_for_replica_group_outcome(3, &mut stop_rx, &mut err_rx, &cancel),
        )
        .await
        .expect("engine cancel should wake replica group wait");
        match outcome {
            SessionOutcome::UserCancel => {}
            SessionOutcome::Failed(e) => panic!("expected user cancel, got {e}"),
        }
    }

    #[tokio::test]
    async fn first_successful_gateway_attempt_waits_for_preferred_candidate_before_fallback() {
        async fn delayed_attempt(
            index: usize,
            delay: Duration,
            result: tp_transport::Result<&'static str>,
        ) -> (usize, tp_transport::Result<&'static str>) {
            tokio::time::sleep(delay).await;
            (index, result)
        }

        let attempts = vec![
            delayed_attempt(
                0,
                Duration::from_millis(120),
                Err(tp_transport::TransportError::Other(
                    "preferred failure".into(),
                )),
            ),
            delayed_attempt(1, Duration::ZERO, Ok("fast")),
        ];

        let started_at = Instant::now();
        let result = timeout(
            Duration::from_secs(1),
            first_successful_gateway_attempt(attempts, 2),
        )
        .await
        .expect("gateway attempts should complete")
        .expect("fallback gateway candidate");

        assert_eq!(result, "fast");
        assert!(
            started_at.elapsed() >= Duration::from_millis(100),
            "fallback candidate returned before the preferred candidate failed"
        );
    }

    /// A single long-lived forwarder must (a) drop messages while
    /// `multi == None`, (b) start delivering once a replica is installed,
    /// (c) keep delivering after the replica is replaced (reconnect).
    /// Pre-fix the forwarder spawn was guarded by `if let Some(multi)`
    /// at attach time, so a fresh-engine attach left the outbound dead
    /// even after subsequent replicas came up.
    #[tokio::test]
    async fn p2p_signaling_forwarder_survives_replica_reconnect() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};
        use tp_core::protocol::{unpack, BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn make_test_multi() -> (Arc<MultiSession>, mpsc::Receiver<PackedMessage>) {
            let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
            let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            let multi =
                MultiSession::new_with_existing_maps(Arc::new(session), inbound, udp_inbound);
            (multi, out_rx)
        }

        fn ack(seq: u8) -> BinaryMessage {
            BinaryMessage::P2pAnnounceAck {
                public_ip: format!("1.1.1.{seq}"),
                public_port: 1000 + seq as u16,
                server_time_ms: 0,
            }
        }

        fn offer(seq: u8) -> BinaryMessage {
            BinaryMessage::P2pOffer {
                session_id: SessionId::from_bytes([seq; 16]),
                src_client_id: "client-1".into(),
                dst_client_id: "bob".into(),
                candidates: vec![],
                src_cert_fp: CertFingerprint::zero(),
                role: P2pRole::Initiator,
            }
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, _in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(16);
        engine.attach_p2p_signaling(in_tx, out_rx);

        // Phase 1: no multi yet — message gets dropped, forwarder stays alive.
        out_tx.send(offer(1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Phase 2: install replica 1.
        let (multi1, mut relay_rx_1) = make_test_multi();
        engine.install_proxy_replica_session_for_test("client-1", multi1.clone());

        out_tx.send(offer(2)).await.unwrap();
        let pkt = timeout(Duration::from_millis(500), relay_rx_1.recv())
            .await
            .expect("forwarder must deliver to replica 1 within 500ms")
            .expect("relay channel still open");
        match unpack(&pkt.to_bytes()).expect("decode") {
            BinaryMessage::P2pOffer { session_id, .. } => {
                assert_eq!(session_id.as_bytes(), &[2u8; 16]);
            }
            other => panic!("expected P2pOffer; got {other:?}"),
        }

        // Phase 3: replica torn down. Subsequent send must drop and NOT panic
        // the forwarder.
        engine.unregister_replica_multi_session("client-1", &multi1);
        out_tx.send(offer(3)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            relay_rx_1.try_recv().is_err(),
            "no live multi: nothing must arrive on the dead replica"
        );

        // Phase 4: install replica 2 (reconnect). Forwarder must pick up
        // the new relay automatically.
        let (multi2, mut relay_rx_2) = make_test_multi();
        engine.install_proxy_replica_session_for_test("client-1", multi2);

        out_tx.send(ack(4)).await.unwrap();
        let pkt = timeout(Duration::from_millis(500), relay_rx_2.recv())
            .await
            .expect("forwarder must deliver to replica 2 within 500ms")
            .expect("relay channel still open");
        match unpack(&pkt.to_bytes()).expect("decode") {
            BinaryMessage::P2pAnnounceAck { public_port, .. } => {
                assert_eq!(public_port, 1004);
            }
            other => panic!("expected P2pAnnounceAck; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queued_p2p_offer_does_not_fallback_after_exact_replica_unregisters() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, _in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(16);

        let (anchor_relay, mut anchor_relay_rx, _anchor_closed_rx) = watchdog_channel_session();
        let anchor_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            anchor_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let (exact_relay, _exact_relay_rx, _exact_closed_rx) = watchdog_channel_session();
        let exact_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            exact_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let anchor_id = "peer-a-AbCd0001-0";
        let exact_id = "peer-a-AbCd0001-2";
        engine.install_proxy_replica_session_for_test(anchor_id, anchor_multi);
        engine.install_proxy_replica_session_for_test(exact_id, exact_multi.clone());

        let session_id = SessionId::from_bytes([0xD3; 16]);
        assert!(engine.reserve_p2p_session_install(
            session_id,
            Some(exact_id),
            Some("peer-b-AbCd0002-2"),
        ));
        out_tx
            .send(BinaryMessage::P2pOffer {
                session_id,
                src_client_id: exact_id.into(),
                dst_client_id: "peer-b-AbCd0002-2".into(),
                candidates: vec![],
                src_cert_fp: CertFingerprint::zero(),
                role: P2pRole::Initiator,
            })
            .await
            .expect("queue exact Offer before starting the forwarder");

        engine.unregister_replica_multi_session(exact_id, &exact_multi);
        engine.attach_p2p_signaling(in_tx, out_rx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            anchor_relay_rx.try_recv().is_err(),
            "an Offer claiming Replica 2 must fail closed instead of using Replica 0's authenticated relay"
        );
        engine.unreserve_p2p_session_install(session_id);
    }

    #[tokio::test]
    async fn queued_p2p_answer_does_not_fallback_after_exact_replica_unregisters() {
        use tp_core::p2p_types::{CertFingerprint, SessionId};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, _in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(16);

        let (anchor_relay, mut anchor_relay_rx, _anchor_closed_rx) = watchdog_channel_session();
        let anchor_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            anchor_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let (exact_relay, _exact_relay_rx, _exact_closed_rx) = watchdog_channel_session();
        let exact_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            exact_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let anchor_id = "peer-b-AbCd0002-0";
        let exact_id = "peer-b-AbCd0002-2";
        engine.install_proxy_replica_session_for_test(anchor_id, anchor_multi);
        engine.install_proxy_replica_session_for_test(exact_id, exact_multi.clone());

        let session_id = SessionId::from_bytes([0xD4; 16]);
        assert!(engine.reserve_p2p_session_install(
            session_id,
            Some(exact_id),
            Some("peer-a-AbCd0001-2"),
        ));
        out_tx
            .send(BinaryMessage::P2pAnswer {
                session_id,
                accepted_client_id: exact_id.into(),
                ok: true,
                reason: String::new(),
                candidates: vec![],
                dst_cert_fp: CertFingerprint::zero(),
            })
            .await
            .expect("queue exact Answer before starting the forwarder");

        engine.unregister_replica_multi_session(exact_id, &exact_multi);
        engine.attach_p2p_signaling(in_tx, out_rx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            anchor_relay_rx.try_recv().is_err(),
            "an Answer claiming Replica 2 must fail closed instead of using Replica 0's authenticated relay"
        );
        engine.unreserve_p2p_session_install(session_id);
    }

    #[tokio::test]
    async fn p2p_signaling_forwarder_unregisters_closed_announce_relay_and_retries_next_replica() {
        use tp_core::p2p_types::{CertFingerprint, NatHint};
        use tp_core::protocol::unpack;

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, _in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(16);
        engine.attach_p2p_signaling(in_tx, out_rx);
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );

        let (closed_relay, closed_relay_rx, _closed_rx) = watchdog_channel_session();
        drop(closed_relay_rx);
        let closed_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            closed_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let (live_relay, mut live_relay_rx, _live_rx) = watchdog_channel_session();
        let live_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            live_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("client-closed", closed_multi.clone());
        engine.install_proxy_replica_session_for_test("client-live", live_multi);

        let announce = || BinaryMessage::P2pAnnounce {
            client_id: "client-closed".into(),
            group_id: "group-test".into(),
            locals: vec![],
            nat_hint: NatHint::Unknown,
            cert_fp: CertFingerprint::zero(),
        };

        out_tx.send(announce()).await.expect("first announce");
        out_tx.send(announce()).await.expect("second announce");

        let pkt = timeout(Duration::from_millis(500), live_relay_rx.recv())
            .await
            .expect("second announce should retry on live relay")
            .expect("live relay channel open");
        match unpack(&pkt.to_bytes()).expect("decode announce") {
            BinaryMessage::P2pAnnounce { client_id, .. } => {
                assert_eq!(client_id, "client-closed");
            }
            other => panic!("expected P2pAnnounce on live relay, got {other:?}"),
        }

        assert!(
            !engine
                .replica_sessions
                .lock()
                .iter()
                .any(|entry| entry.relay_active && Arc::ptr_eq(&entry.multi, &closed_multi)),
            "closed relay must not remain selectable for P2P signaling"
        );
    }

    #[tokio::test]
    async fn unacked_membership_hints_from_disconnected_relay_do_not_pollute_next_cycle() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay_a, _relay_a_rx, _relay_a_closed) = watchdog_channel_session();
        let multi_a = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay_a,
            active_tcp_map(),
            active_udp_map(),
        );
        let (relay_b, _relay_b_rx, _relay_b_closed) = watchdog_channel_session();
        let multi_b = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay_b,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi_a.clone());
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-1", multi_b.clone());

        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pPeerHint {
                    peer_client_id: "mesh-StaleB1-0".into(),
                },
                &multi_a,
            )
            .await;
        engine.unregister_replica_multi_session("mesh-LocalA1-0", &multi_a);

        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pPeerHint {
                    peer_client_id: "mesh-LiveC01-0".into(),
                },
                &multi_b,
            )
            .await;
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 1,
                },
                &multi_b,
            )
            .await;

        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pPeerHint { peer_client_id }))
                if peer_client_id == "mesh-LiveC01-0"
        ));
        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 1,
                ..
            }))
        ));
        assert!(in_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn queued_membership_batch_keeps_its_source_relay_generation_authority() {
        let gateway = GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        };
        let mut owner = TunnelOwnerFileV2::generate(gateway).expect("Tunnel");
        let local = Arc::new(owner.add_peer(None, 1, None).expect("local Peer"));
        let stale_peer = owner.add_peer(None, 1, None).expect("stale Peer");
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        *engine.active_v2_profile.write() = Some(local.clone());
        engine.begin_v2_runtime(&local);

        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(1);
        in_tx
            .try_send(BinaryMessage::HeartbeatAck { timestamp: 0 })
            .expect("block broker delivery");
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (old_relay, _old_rx, _old_closed) = watchdog_channel_session();
        let old_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            old_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.register_replica_multi_session("mesh-LocalA1-0", "group-test", old_multi.clone(), 1);
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pPeerHint {
                    peer_client_id: stale_peer.peer.peer_id.clone(),
                },
                &old_multi,
            )
            .await;
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 1,
                },
                &old_multi,
            )
            .await;

        engine.unregister_replica_multi_session("mesh-LocalA1-0", &old_multi);
        let (new_relay, _new_rx, _new_closed) = watchdog_channel_session();
        let new_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            new_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.register_replica_multi_session("mesh-LocalA1-0", "group-test", new_multi.clone(), 2);

        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::HeartbeatAck { timestamp: 0 })
        ));
        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::P2pPeerHint { .. })
        ));
        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 1,
                ..
            })
        ));
        assert!(
            !engine.commit_delivered_v2_membership_cycle(&[stale_peer.peer.peer_id]),
            "old queued Ack must not gain the new relay generation's authority"
        );

        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 2,
                },
                &new_multi,
            )
            .await;
        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 2,
                ..
            })
        ));
        assert!(
            engine.commit_delivered_v2_membership_cycle(&[]),
            "current relay generation must still commit its zero-hint cycle"
        );
    }

    #[tokio::test]
    async fn zero_hint_membership_ack_reaches_manager_as_an_empty_cycle() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());

        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 7,
                },
                &multi,
            )
            .await;

        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 7,
                ..
            }))
        ));
        assert!(in_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn concurrent_relay_membership_batches_commit_without_interleaving() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(8);
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay_a, _relay_a_rx, _relay_a_closed) = watchdog_channel_session();
        let multi_a = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay_a,
            active_tcp_map(),
            active_udp_map(),
        );
        let (relay_b, _relay_b_rx, _relay_b_closed) = watchdog_channel_session();
        let multi_b = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay_b,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi_a.clone());
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-1", multi_b.clone());

        for (multi, peer_client_id) in
            [(&multi_a, "mesh-RemoteB1-0"), (&multi_b, "mesh-RemoteC1-0")]
        {
            engine
                .forward_p2p_signaling_from_relay_for_test(
                    BinaryMessage::P2pPeerHint {
                        peer_client_id: peer_client_id.into(),
                    },
                    multi,
                )
                .await;
        }

        for (multi, server_time_ms, expected_peer) in [
            (&multi_b, 2, "mesh-RemoteC1-0"),
            (&multi_a, 1, "mesh-RemoteB1-0"),
        ] {
            engine
                .forward_p2p_signaling_from_relay_for_test(
                    BinaryMessage::P2pAnnounceAck {
                        public_ip: "203.0.113.10".into(),
                        public_port: 4433,
                        server_time_ms,
                    },
                    multi,
                )
                .await;

            assert!(matches!(
                timeout(Duration::from_millis(100), in_rx.recv()).await,
                Ok(Some(BinaryMessage::P2pPeerHint { peer_client_id, .. }))
                    if peer_client_id == expected_peer
            ));
            assert!(matches!(
                timeout(Duration::from_millis(100), in_rx.recv()).await,
                Ok(Some(BinaryMessage::P2pAnnounceAck { server_time_ms: actual, .. }))
                    if actual == server_time_ms
            ));
        }
        assert!(in_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn membership_batch_survives_manager_backpressure_without_blocking_relay_heartbeat() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(1);
        in_tx
            .try_send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 1,
                server_time_ms: -1,
            })
            .expect("test setup fills the manager ingress");
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());

        for index in 0..3 {
            engine
                .forward_p2p_signaling_from_relay_for_test(
                    BinaryMessage::P2pPeerHint {
                        peer_client_id: format!("mesh-Remote{index:02}-0"),
                    },
                    &multi,
                )
                .await;
        }
        timeout(
            Duration::from_millis(50),
            engine.forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 9,
                },
                &multi,
            ),
        )
        .await
        .expect("a full manager channel must not block the relay reader");
        let trailing_session_id = tp_core::p2p_types::SessionId::from_bytes([0x91; 16]);
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pSessionReady {
                    session_id: trailing_session_id,
                    rtt_us: 100,
                    chosen_remote_ip: "203.0.113.20".into(),
                    chosen_remote_port: 5000,
                },
                &multi,
            )
            .await;

        let previous_ack = u64::MAX;
        let last_ack = Arc::new(AtomicU64::new(previous_ack));
        let last_progress = Arc::new(AtomicU64::new(0));
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();
        engine
            .handle_msg(
                BinaryMessage::HeartbeatAck { timestamp: 456 },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                None,
                Some(LinkLivenessState::relay(
                    last_ack.clone(),
                    last_progress,
                    Arc::new(LinkActiveFlows::default()),
                )),
            )
            .await;
        assert_ne!(last_ack.load(Ordering::Relaxed), previous_ack);

        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: -1,
                ..
            })
        ));
        for index in 0..3 {
            assert!(matches!(
                timeout(Duration::from_millis(100), in_rx.recv()).await,
                Ok(Some(BinaryMessage::P2pPeerHint { peer_client_id, .. }))
                    if peer_client_id == format!("mesh-Remote{index:02}-0")
            ));
        }
        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 9,
                ..
            }))
        ));
        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pSessionReady { session_id, .. }))
                if session_id == trailing_session_id
        ));
    }

    #[tokio::test]
    async fn oversized_membership_cycle_is_dropped_whole_and_does_not_poison_next_cycle() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());

        for index in 0..=P2P_MEMBERSHIP_BATCH_MAX_HINTS {
            engine
                .forward_p2p_signaling_from_relay_for_test(
                    BinaryMessage::P2pPeerHint {
                        peer_client_id: format!("mesh-Remote{index:08}-0"),
                    },
                    &multi,
                )
                .await;
        }
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 10,
                },
                &multi,
            )
            .await;
        assert!(
            timeout(Duration::from_millis(50), in_rx.recv())
                .await
                .is_err(),
            "an oversized membership transaction must not publish a prefix"
        );

        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 11,
                },
                &multi,
            )
            .await;
        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 11,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn full_ingress_broker_drops_membership_batch_whole() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(1);
        in_tx
            .try_send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 1,
                server_time_ms: -1,
            })
            .expect("test setup fills manager ingress");
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());

        let ack = |server_time_ms| BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms,
        };
        engine
            .forward_p2p_signaling_from_relay_for_test(ack(0), &multi)
            .await;
        wait_for_p2p_ingress_broker_to_take_one(&engine).await;
        for server_time_ms in 1..=P2P_SIGNALING_INGRESS_BROKER_CAPACITY as i64 {
            engine
                .forward_p2p_signaling_from_relay_for_test(ack(server_time_ms), &multi)
                .await;
        }

        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pPeerHint {
                    peer_client_id: "mesh-Dropped1-0".into(),
                },
                &multi,
            )
            .await;
        timeout(
            Duration::from_millis(50),
            engine.forward_p2p_signaling_from_relay_for_test(ack(999), &multi),
        )
        .await
        .expect("a full broker must not block the relay reader");

        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: -1,
                ..
            })
        ));
        for expected in 0..=P2P_SIGNALING_INGRESS_BROKER_CAPACITY as i64 {
            assert!(matches!(
                timeout(Duration::from_millis(100), in_rx.recv()).await,
                Ok(Some(BinaryMessage::P2pAnnounceAck { server_time_ms, .. }))
                    if server_time_ms == expected
            ));
        }

        engine
            .forward_p2p_signaling_from_relay_for_test(ack(1000), &multi)
            .await;
        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: 1000,
                ..
            }))
        ));
        assert!(in_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn disconnect_cancels_backpressured_ingress_broker_and_clears_pending_batch() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(1);
        in_tx
            .try_send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 1,
                server_time_ms: -1,
            })
            .expect("test setup fills manager ingress");
        let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pAnnounceAck {
                    public_ip: "203.0.113.10".into(),
                    public_port: 4433,
                    server_time_ms: 1,
                },
                &multi,
            )
            .await;
        wait_for_p2p_ingress_broker_to_take_one(&engine).await;
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pPeerHint {
                    peer_client_id: "mesh-Uncommitted1-0".into(),
                },
                &multi,
            )
            .await;
        drop(out_tx);

        timeout(Duration::from_millis(500), engine.disconnect())
            .await
            .expect("disconnect must cancel a broker blocked on manager backpressure");
        assert!(engine.p2p_pending_membership_batches.lock().is_empty());
        assert!(engine.p2p_signaling_ingress_tx.lock().is_none());
        assert!(matches!(
            in_rx.recv().await,
            Some(BinaryMessage::P2pAnnounceAck {
                server_time_ms: -1,
                ..
            })
        ));
        assert!(matches!(
            timeout(Duration::from_millis(100), in_rx.recv()).await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn full_p2p_signaling_broker_does_not_block_relay_heartbeat_ack() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, _in_rx) = mpsc::channel::<BinaryMessage>(1);
        in_tx
            .try_send(BinaryMessage::P2pAnnounceAck {
                public_ip: "127.0.0.1".into(),
                public_port: 12345,
                server_time_ms: 0,
            })
            .expect("test setup should fill inbound signaling channel");
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(1);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (relay, _relay_rx, _relay_closed) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());
        let ack = |server_time_ms| BinaryMessage::P2pAnnounceAck {
            public_ip: "203.0.113.10".into(),
            public_port: 4433,
            server_time_ms,
        };
        engine
            .forward_p2p_signaling_from_relay_for_test(ack(0), &multi)
            .await;
        wait_for_p2p_ingress_broker_to_take_one(&engine).await;
        for server_time_ms in 1..=P2P_SIGNALING_INGRESS_BROKER_CAPACITY as i64 {
            engine
                .forward_p2p_signaling_from_relay_for_test(ack(server_time_ms), &multi)
                .await;
        }

        let session_id = SessionId::from_bytes([0xAA; 16]);
        timeout(
            Duration::from_millis(50),
            engine.forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pOffer {
                    session_id,
                    src_client_id: "mobile-1".into(),
                    dst_client_id: "pc-1".into(),
                    candidates: vec![],
                    src_cert_fp: CertFingerprint::zero(),
                    role: P2pRole::Initiator,
                },
                &multi,
            ),
        )
        .await
        .expect("full P2P signaling broker must not block relay reader");
        assert!(
            !engine.p2p_signaling_routes.contains_key(&session_id),
            "dropped inbound signaling must not leave a stale session route"
        );

        let previous_ack = u64::MAX;
        let last_ack = Arc::new(AtomicU64::new(previous_ack));
        let last_progress = Arc::new(AtomicU64::new(0));
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();
        engine
            .handle_msg(
                BinaryMessage::HeartbeatAck { timestamp: 456 },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                None,
                Some(LinkLivenessState::relay(
                    last_ack.clone(),
                    last_progress.clone(),
                    Arc::new(LinkActiveFlows::default()),
                )),
            )
            .await;

        assert!(
            last_ack.load(Ordering::Relaxed) != previous_ack,
            "HeartbeatAck must still update the transport last-ack path"
        );
        let status = engine.status();
        assert_eq!(status.transport_heartbeat.last_time, Some(456));
        assert!(status.transport_heartbeat.active);
    }

    #[tokio::test]
    async fn relay_heartbeat_ack_updates_relay_liveness() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = handle_msg_test_multi();
        let previous_ack = u64::MAX;
        let previous_progress = u64::MAX - 1;
        let relay_ack = Arc::new(AtomicU64::new(previous_ack));
        let relay_progress = Arc::new(AtomicU64::new(previous_progress));
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();

        engine
            .handle_msg(
                BinaryMessage::HeartbeatAck { timestamp: 456 },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                None,
                Some(LinkLivenessState::relay(
                    relay_ack.clone(),
                    relay_progress.clone(),
                    Arc::new(LinkActiveFlows::default()),
                )),
            )
            .await;

        assert_ne!(
            relay_ack.load(Ordering::Relaxed),
            previous_ack,
            "relay HeartbeatAck must update relay ACK liveness"
        );
        assert_ne!(
            relay_progress.load(Ordering::Relaxed),
            previous_progress,
            "relay HeartbeatAck must also count as inbound relay progress"
        );
    }

    #[tokio::test]
    async fn p2p_heartbeat_ack_updates_p2p_liveness_not_relay_liveness() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = handle_msg_test_multi();
        let relay_ack = Arc::new(AtomicU64::new(111));
        let previous_p2p_ack = u64::MAX;
        let p2p_ack = Arc::new(AtomicU64::new(previous_p2p_ack));
        let p2p_progress = Arc::new(AtomicU64::new(u64::MAX - 1));
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();

        engine
            .handle_msg(
                BinaryMessage::HeartbeatAck { timestamp: 789 },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                None,
                Some(LinkLivenessState::p2p(
                    p2p_ack.clone(),
                    p2p_progress.clone(),
                    Arc::new(LinkActiveFlows::default()),
                )),
            )
            .await;

        assert_ne!(
            p2p_ack.load(Ordering::Relaxed),
            previous_p2p_ack,
            "P2P HeartbeatAck must update P2P ACK liveness"
        );
        assert_eq!(
            relay_ack.load(Ordering::Relaxed),
            111,
            "P2P HeartbeatAck must not update relay ACK liveness"
        );
    }

    #[tokio::test]
    async fn p2p_heartbeat_ack_send_failure_closes_link_and_requests_refill() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = handle_msg_test_multi();
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0x9A; 16]);
        let (out_tx, out_rx) = mpsc::channel::<tp_core::protocol::PackedMessage>(1);
        drop(out_rx);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let p2p = Arc::new(Session::new_channeled(
            out_tx,
            in_rx,
            "127.0.0.1:1".parse().expect("peer addr"),
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        multi
            .install_p2p_session(session_id, "peer-watchdog".into(), p2p.clone())
            .expect("install p2p");
        multi.set_state(crate::p2p::session::P2pState::Active {
            session_id,
            since: std::time::Instant::now(),
        });

        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();
        engine
            .handle_msg(
                BinaryMessage::Heartbeat {
                    client_id: "peer-watchdog".into(),
                    timestamp: 456,
                },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                Some(p2p),
                None,
            )
            .await;

        assert!(
            multi.p2p().is_none(),
            "failed P2P heartbeat ACK send is a broken link and must close the direct session"
        );
        assert!(
            engine.p2p_refill_requested_for_test("peer-watchdog") > 0,
            "closing the broken P2P link must request a replacement"
        );
    }

    #[tokio::test]
    async fn inbound_non_heartbeat_updates_same_link_link_progress() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let multi = handle_msg_test_multi();
        let relay_ack = Arc::new(AtomicU64::new(777));
        let previous_progress = u64::MAX;
        let relay_progress = Arc::new(AtomicU64::new(previous_progress));
        let p2p_progress = Arc::new(AtomicU64::new(333));
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();

        engine
            .handle_msg(
                BinaryMessage::UdpData {
                    conn_id: "progress-only".into(),
                    payload: Bytes::from_static(b"payload"),
                },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                None,
                Some(LinkLivenessState::relay(
                    relay_ack.clone(),
                    relay_progress.clone(),
                    Arc::new(LinkActiveFlows::default()),
                )),
            )
            .await;

        assert_eq!(
            relay_ack.load(Ordering::Relaxed),
            777,
            "non-heartbeat inbound messages must not update ACK liveness"
        );
        assert_ne!(
            relay_progress.load(Ordering::Relaxed),
            previous_progress,
            "non-heartbeat inbound messages must update same-link link progress"
        );
        assert_eq!(
            p2p_progress.load(Ordering::Relaxed),
            333,
            "relay inbound progress must not update P2P progress state"
        );
    }

    #[tokio::test]
    async fn relay_heartbeat_ack_does_not_wait_for_status_listener() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::protocol::PackedMessage;
        use tp_transport::DropOldestSender;

        struct BlockingListener {
            calls: Arc<AtomicUsize>,
        }

        impl StatusListener for BlockingListener {
            fn on_status(&self, _status: &ConnectionStatus) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(200));
            }
        }

        fn make_test_multi() -> Arc<MultiSession> {
            let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(16);
            let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(Arc::new(session), inbound, udp_inbound)
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let engine = Engine::new(
            EngineConfig::default(),
            Arc::new(BlockingListener {
                calls: calls.clone(),
            }),
        );
        let multi = make_test_multi();
        let previous_ack = u64::MAX;
        let last_ack = Arc::new(AtomicU64::new(previous_ack));
        let last_progress = Arc::new(AtomicU64::new(0));
        let host_filter = Arc::new(HostFilter::new(&[], &[]).expect("empty host filter"));
        let tracker = TaskTracker::new();

        timeout(
            Duration::from_millis(50),
            engine.handle_msg(
                BinaryMessage::HeartbeatAck { timestamp: 789 },
                &multi,
                &multi.inbound(),
                &multi.udp_inbound(),
                &host_filter,
                &tracker,
                None,
                Some(LinkLivenessState::relay(
                    last_ack.clone(),
                    last_progress.clone(),
                    Arc::new(LinkActiveFlows::default()),
                )),
            ),
        )
        .await
        .expect("HeartbeatAck must not synchronously call the status listener");

        assert!(
            last_ack.load(Ordering::Relaxed) != previous_ack,
            "HeartbeatAck must still update liveness"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "transport heartbeat ACK should not emit a per-second status event"
        );
        assert_eq!(engine.status().transport_heartbeat.last_time, Some(789));
    }

    #[test]
    fn status_refresh_interval_is_fast_enough_for_ui_traffic() {
        assert_eq!(status_refresh_interval(), Duration::from_secs(1));
    }

    #[test]
    fn status_snapshot_emits_when_only_traffic_changes() {
        use std::sync::mpsc;

        struct CapturingListener {
            tx: mpsc::Sender<ConnectionStatus>,
        }

        impl StatusListener for CapturingListener {
            fn on_status(&self, status: &ConnectionStatus) {
                let _ = self.tx.send(status.clone());
            }
        }

        let (tx, rx) = mpsc::channel();
        let engine = Engine::new(EngineConfig::default(), Arc::new(CapturingListener { tx }));
        engine.set_status(ConnectionStatus {
            connected: true,
            message: "Connected".into(),
            ..Default::default()
        });
        let _ = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("initial connected status should emit");

        engine.traffic.record_tx(TrafficPath::P2p, 42);
        engine.emit_status_snapshot();

        let refreshed = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("traffic-only status refresh should emit");
        assert_eq!(refreshed.traffic.p2p_tx_bytes, 42);
    }

    #[tokio::test]
    async fn p2p_signaling_answer_returns_on_offer_replica_relay() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};
        use tp_core::protocol::{unpack, BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn make_test_multi() -> (Arc<MultiSession>, mpsc::Receiver<PackedMessage>) {
            let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
            let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
            let writer = tokio::spawn(async {});
            let reader = tokio::spawn(async {});
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            let multi =
                MultiSession::new_with_existing_maps(Arc::new(session), inbound, udp_inbound);
            (multi, out_rx)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (out_tx, out_rx) = mpsc::channel::<BinaryMessage>(16);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (anchor, mut anchor_rx) = make_test_multi();
        let (replica, mut replica_rx) = make_test_multi();
        engine.install_proxy_replica_session_for_test("pc-AbC12345-0", anchor);
        engine.install_proxy_replica_session_for_test("pc-AbC12345-1", replica.clone());

        let outbound_session_id = SessionId::from_bytes([0x5B; 16]);
        engine.reserve_p2p_session_install(
            outbound_session_id,
            Some("pc-AbC12345-1"),
            Some("mobile-Zz987654-0"),
        );
        out_tx
            .send(BinaryMessage::P2pOffer {
                session_id: outbound_session_id,
                src_client_id: "pc-AbC12345-1".into(),
                dst_client_id: "mobile-Zz987654-0".into(),
                candidates: vec![],
                src_cert_fp: CertFingerprint::from_bytes([1u8; 32]),
                role: P2pRole::Initiator,
            })
            .await
            .expect("manager outbound offer");
        let routed_offer = timeout(Duration::from_millis(500), replica_rx.recv())
            .await
            .expect("offer should route to reserved replica relay")
            .expect("replica relay channel open");
        match unpack(&routed_offer.to_bytes()).expect("decode offer") {
            BinaryMessage::P2pOffer {
                session_id: got, ..
            } => assert_eq!(got, outbound_session_id),
            other => panic!("expected P2pOffer on replica relay, got {other:?}"),
        }
        engine.unreserve_p2p_session_install(outbound_session_id);
        out_tx
            .send(BinaryMessage::P2pTeardown {
                session_id: outbound_session_id,
                reason: tp_core::p2p_types::TeardownReason::FatalError,
            })
            .await
            .expect("manager outbound teardown");
        let routed_teardown = timeout(Duration::from_millis(500), replica_rx.recv())
            .await
            .expect("teardown should preserve reserved replica relay after unreserve")
            .expect("replica relay channel open");
        match unpack(&routed_teardown.to_bytes()).expect("decode teardown") {
            BinaryMessage::P2pTeardown {
                session_id: got, ..
            } => assert_eq!(got, outbound_session_id),
            other => panic!("expected P2pTeardown on replica relay, got {other:?}"),
        }

        let session_id = SessionId::from_bytes([0x5A; 16]);
        engine
            .forward_p2p_signaling_from_relay_for_test(
                BinaryMessage::P2pOffer {
                    session_id,
                    src_client_id: "mobile-1".into(),
                    dst_client_id: "pc-AbC12345-1".into(),
                    candidates: vec![],
                    src_cert_fp: CertFingerprint::from_bytes([1u8; 32]),
                    role: P2pRole::Initiator,
                },
                &replica,
            )
            .await;
        assert!(matches!(
            timeout(Duration::from_millis(500), in_rx.recv()).await,
            Ok(Some(BinaryMessage::P2pOffer { session_id: got, .. })) if got == session_id
        ));

        out_tx
            .send(BinaryMessage::P2pAnswer {
                session_id,
                accepted_client_id: "pc-AbC12345-1".into(),
                ok: true,
                reason: String::new(),
                candidates: vec![],
                dst_cert_fp: CertFingerprint::from_bytes([2u8; 32]),
            })
            .await
            .expect("manager outbound answer");

        let routed = timeout(Duration::from_millis(500), replica_rx.recv())
            .await
            .expect("answer should route to replica relay")
            .expect("replica relay channel open");
        match unpack(&routed.to_bytes()).expect("decode answer") {
            BinaryMessage::P2pAnswer {
                session_id: got, ..
            } => assert_eq!(got, session_id),
            other => panic!("expected P2pAnswer on replica relay, got {other:?}"),
        }
        assert!(
            anchor_rx.try_recv().is_err(),
            "sidecar answer must not be sent on the anchor relay"
        );
    }

    #[tokio::test]
    async fn p2p_session_install_uses_reserved_replica_slots() {
        use crate::p2p::session::{MultiSession, P2pState};
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (Arc<Session>, Session) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, _p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer.clone(),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p)
        }

        fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (relay_a, p2p_a) = session_pair();
        let (relay_b, p2p_b) = session_pair();
        let multi_a = make_multi(relay_a);
        let multi_b = make_multi(relay_b);
        engine.install_proxy_replica_session_for_test("mobile-1", multi_a.clone());
        engine.install_proxy_replica_session_for_test("mobile-1-1", multi_b.clone());

        let sid_a = SessionId::from_bytes([0xA1; 16]);
        let sid_b = SessionId::from_bytes([0xA2; 16]);
        engine.reserve_p2p_session_install(sid_a, None, Some("peer-a"));
        engine.reserve_p2p_session_install(sid_b, None, Some("peer-b"));

        let _installed_a = engine
            .install_p2p_session(sid_a, p2p_a, engine.task_cancel_token())
            .await
            .expect("install sid_a");
        let _installed_b = engine
            .install_p2p_session(sid_b, p2p_b, engine.task_cancel_token())
            .await
            .expect("install sid_b");

        assert!(multi_a.p2p().is_some(), "sid_a should install on replica A");
        assert!(multi_b.p2p().is_some(), "sid_b should install on replica B");
        assert!(
            matches!(multi_a.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid_a),
            "replica A state should track sid_a"
        );
        assert!(
            matches!(multi_b.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid_b),
            "replica B state should track sid_b"
        );
    }

    #[tokio::test]
    async fn anchor_replica_disconnect_keeps_group_context_for_live_sidecar_p2p_install() {
        use crate::p2p::session::{MultiSession, P2pState};
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (Arc<Session>, Session) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, _p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer.clone(),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p)
        }

        fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (anchor_relay, _anchor_p2p) = session_pair();
        let (sidecar_relay, sidecar_p2p) = session_pair();
        let anchor = make_multi(anchor_relay);
        let sidecar = make_multi(sidecar_relay);
        engine.install_proxy_replica_session_for_test("mobile-1", anchor.clone());
        engine.install_proxy_replica_session_for_test("mobile-1-1", sidecar.clone());

        engine.unregister_replica_multi_session("mobile-1", &anchor);

        assert!(
            engine.group_context.lock().is_some(),
            "group context must stay live while any replica remains connected"
        );
        let context = engine.group_context.lock().clone().expect("group context");
        assert_eq!(context.tunnel_id, "tunnel-test");
        assert_eq!(context.group_id, "group-test");

        let sid = SessionId::from_bytes([0xA3; 16]);
        engine.reserve_p2p_session_install(sid, None, Some("peer-sidecar"));
        let _installed = engine
            .install_p2p_session(sid, sidecar_p2p, engine.task_cancel_token())
            .await
            .expect("sidecar P2P install should still have group access policy");

        assert!(sidecar.p2p().is_some(), "sid should install on sidecar");
        assert!(
            matches!(sidecar.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid),
            "sidecar state should track sid"
        );
    }

    #[tokio::test]
    async fn sidecar_unregister_closes_sidecar_p2p_session() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (anchor_relay, _anchor_out_rx, _anchor_closed_rx) = watchdog_channel_session();
        let (sidecar_relay, _sidecar_out_rx, _sidecar_closed_rx) = watchdog_channel_session();
        let (sidecar_p2p, _sidecar_p2p_out_rx, mut sidecar_p2p_closed_rx) =
            watchdog_channel_session();
        let anchor = crate::p2p::session::MultiSession::new_with_existing_maps(
            anchor_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let sidecar = crate::p2p::session::MultiSession::new_with_existing_maps(
            sidecar_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.install_proxy_replica_session_for_test("mobile-1", anchor);
        engine.install_proxy_replica_session_for_test("mobile-1-1", sidecar.clone());

        let sid = tp_core::p2p_types::SessionId::from_bytes([0xC5; 16]);
        sidecar
            .install_p2p_session(sid, "peer-sidecar".into(), sidecar_p2p)
            .expect("install sidecar P2P");
        sidecar.set_state(crate::p2p::session::P2pState::Active {
            session_id: sid,
            since: Instant::now(),
        });

        engine.unregister_replica_multi_session("mobile-1-1", &sidecar);

        timeout(Duration::from_millis(100), sidecar_p2p_closed_rx.recv())
            .await
            .expect("sidecar unregister must close its P2P handle")
            .expect("sidecar P2P close signal");
        assert!(
            sidecar.p2p().is_none(),
            "removed sidecar must not retain an installed P2P session"
        );
        assert!(
            matches!(sidecar.p2p_state(), crate::p2p::session::P2pState::Idle),
            "removed sidecar P2P state should return to idle"
        );
        assert!(
            engine.group_context.lock().is_some(),
            "anchor still connected, so group context should remain live"
        );
    }

    #[tokio::test]
    async fn relay_closed_unregister_preserves_p2p_only_lane() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (multi, _p2p, _relay_out_rx, mut relay_closed_rx, mut p2p_closed_rx) =
            p2p_watchdog_multi();
        engine.install_proxy_replica_session_for_test("app-1", multi.clone());

        multi.relay().close();
        timeout(Duration::from_millis(100), relay_closed_rx.recv())
            .await
            .expect("test relay close signal")
            .expect("relay close signal");
        engine.unregister_relay_closed_multi_session("app-1", &multi);

        assert!(
            timeout(Duration::from_millis(30), p2p_closed_rx.recv())
                .await
                .is_err(),
            "relay teardown must not close a still-installed P2P link"
        );
        assert!(multi.p2p().is_some(), "P2P link should remain installed");
        assert!(
            engine.pick_proxy_relay_lane().is_none(),
            "closed relay must not stay available as a relay lane"
        );
        let lane = engine
            .pick_proxy_flow_lane(FlowKind::Tcp, &[])
            .expect("p2p-only lane should remain usable for new source flows");
        assert_eq!(lane.path, crate::p2p::scheduler::PathKind::P2p);
    }

    #[tokio::test]
    async fn gateway_attachment_generation_reset_preserves_direct_lane_and_flow() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (old_relay, _old_relay_out_rx, mut old_relay_closed_rx) = watchdog_channel_session();
        let (direct, _direct_out_rx, mut direct_closed_rx) = watchdog_channel_session();
        let old_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            old_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        let direct_session_id = tp_core::p2p_types::SessionId::from_bytes([0x4d; 16]);
        let remote_peer = "mesh-RemoteB1-0";
        old_multi
            .install_p2p_session(direct_session_id, remote_peer.into(), direct.clone())
            .expect("install healthy Direct session");
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", old_multi.clone());

        let direct_flow = engine
            .pick_and_record_proxy_flow_lane_for_peer(
                "direct-flow-across-gateway-generation",
                FlowKind::Tcp,
                &[],
                Some(remote_peer),
                false,
            )
            .expect("healthy Direct lane");
        assert_eq!(direct_flow.path, crate::p2p::scheduler::PathKind::P2p);
        engine.mark_proxy_flow_established("direct-flow-across-gateway-generation");

        old_multi.relay().close();
        timeout(Duration::from_millis(100), old_relay_closed_rx.recv())
            .await
            .expect("old Gateway relay close signal")
            .expect("old Gateway relay closed");
        engine.unregister_relay_closed_multi_session("mesh-LocalA1-0", &old_multi);

        engine.reset_replica_sessions_for_connect(Some("mesh-LocalA1-0".into()));

        assert!(
            timeout(Duration::from_millis(30), direct_closed_rx.recv())
                .await
                .is_err(),
            "a full Gateway Attachment reset must not close healthy Direct"
        );
        assert_eq!(
            engine
                .proxy_flow_registry
                .candidate_key("direct-flow-across-gateway-generation")
                .expect("established Direct flow placement must survive")
                .path,
            CandidatePath::P2p
        );
        let retained_direct = engine
            .pick_proxy_flow_lane_for_peer(FlowKind::Tcp, &[], Some(remote_peer), false)
            .expect("retained Direct lane remains selectable");
        assert_eq!(retained_direct.path, crate::p2p::scheduler::PathKind::P2p);
        assert!(Arc::ptr_eq(
            retained_direct
                .p2p_session
                .as_ref()
                .expect("retained Direct session"),
            &direct
        ));

        let (new_relay, _new_relay_out_rx, _new_relay_closed_rx) = watchdog_channel_session();
        let new_multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            new_relay,
            active_tcp_map(),
            active_udp_map(),
        );
        engine.register_replica_multi_session("mesh-LocalA1-0", "mesh", new_multi.clone(), 1);

        let restored_relay = engine
            .pick_proxy_relay_lane()
            .expect("new Gateway generation restores Relay");
        assert!(Arc::ptr_eq(&restored_relay.multi, &new_multi));
        let preferred = engine
            .pick_proxy_flow_lane_for_peer(FlowKind::Tcp, &[], Some(remote_peer), false)
            .expect("Direct remains preferred after Relay recovery");
        assert_eq!(preferred.path, crate::p2p::scheduler::PathKind::P2p);
    }

    #[tokio::test]
    async fn explicit_disconnect_closes_direct_retained_across_gateway_generation() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (relay, _relay_out_rx, _relay_closed_rx) = watchdog_channel_session();
        let (direct, _direct_out_rx, mut direct_closed_rx) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_existing_maps(
            relay,
            active_tcp_map(),
            active_udp_map(),
        );
        multi
            .install_p2p_session(
                tp_core::p2p_types::SessionId::from_bytes([0x5d; 16]),
                "mesh-RemoteB1-0".into(),
                direct,
            )
            .expect("install healthy Direct session");
        engine.install_proxy_replica_session_for_test("mesh-LocalA1-0", multi.clone());
        engine.unregister_relay_closed_multi_session("mesh-LocalA1-0", &multi);
        engine.reset_replica_sessions_for_connect(Some("mesh-LocalA1-0".into()));

        engine.disconnect().await;

        timeout(Duration::from_millis(100), direct_closed_rx.recv())
            .await
            .expect("explicit disconnect must close retained Direct")
            .expect("retained Direct close signal");
        assert!(engine.replica_sessions.lock().is_empty());
    }

    #[tokio::test]
    async fn relay_closed_reader_cannot_reinsert_p2p_signaling_route() {
        use tp_core::p2p_types::{CertFingerprint, P2pRole, SessionId};

        fn offer(session_id: SessionId) -> BinaryMessage {
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id: "app-1".into(),
                dst_client_id: "client-1".into(),
                candidates: vec![],
                src_cert_fp: CertFingerprint::zero(),
                role: P2pRole::Initiator,
            }
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (in_tx, mut in_rx) = mpsc::channel::<BinaryMessage>(16);
        let (_out_tx, out_rx) = mpsc::channel::<BinaryMessage>(16);
        engine.attach_p2p_signaling(in_tx, out_rx);

        let (multi, _p2p, _relay_out_rx, mut relay_closed_rx, _p2p_closed_rx) =
            p2p_watchdog_multi();
        engine.install_proxy_replica_session_for_test("app-1", multi.clone());

        let live_sid = SessionId::from_bytes([0xD1; 16]);
        engine
            .forward_p2p_signaling_from_relay_for_test(offer(live_sid), &multi)
            .await;
        assert!(
            engine.p2p_signaling_routes.contains_key(&live_sid),
            "active relay should publish the inbound signaling route"
        );
        match timeout(Duration::from_millis(100), in_rx.recv())
            .await
            .expect("active relay signaling should reach manager")
            .expect("manager inbound channel open")
        {
            BinaryMessage::P2pOffer { session_id, .. } => assert_eq!(session_id, live_sid),
            other => panic!("expected live P2pOffer, got {other:?}"),
        }

        multi.relay().close();
        timeout(Duration::from_millis(100), relay_closed_rx.recv())
            .await
            .expect("test relay close signal")
            .expect("relay close signal");
        engine.unregister_relay_closed_multi_session("app-1", &multi);
        assert!(
            !engine.p2p_signaling_routes.contains_key(&live_sid),
            "relay unregister must remove routes for the closed relay"
        );

        let stale_sid = SessionId::from_bytes([0xD2; 16]);
        engine
            .forward_p2p_signaling_from_relay_for_test(offer(stale_sid), &multi)
            .await;
        assert!(
            !engine.p2p_signaling_routes.contains_key(&stale_sid),
            "tail messages from a closed relay must not recreate a stale route"
        );
        assert!(
            in_rx.try_recv().is_err(),
            "tail messages from a closed relay must not reach the P2P manager"
        );
    }

    #[tokio::test]
    async fn anchor_disconnect_promotes_sidecar_for_p2p_relay_context() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn channel_session() -> Arc<Session> {
            let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(Session::new_channeled(
                out_tx,
                in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ))
        }

        fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let anchor = make_multi(channel_session());
        let sidecar = make_multi(channel_session());
        engine.install_proxy_replica_session_for_test("mobile-1", anchor.clone());
        engine.install_proxy_replica_session_for_test("mobile-1-1", sidecar.clone());

        engine.unregister_replica_multi_session("mobile-1", &anchor);

        let (client_id, group_id, promoted) = engine
            .p2p_relay_context()
            .expect("sidecar should remain usable as P2P relay context");
        assert_eq!(client_id, "mobile-1-1");
        assert_eq!(group_id, "group-test");
        assert!(Arc::ptr_eq(&promoted, &sidecar));
    }

    #[tokio::test]
    async fn last_replica_disconnect_clears_group_context() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn channel_session() -> Arc<Session> {
            let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            Arc::new(Session::new_channeled(
                out_tx,
                in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ))
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        let multi = MultiSession::new_with_existing_maps(channel_session(), inbound, udp_inbound);
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());

        engine.unregister_replica_multi_session("mobile-1", &multi);

        assert!(
            engine.group_context.lock().is_none(),
            "group context must be cleared after the last live replica disconnects"
        );
    }

    #[tokio::test]
    async fn p2p_install_without_group_context_reports_access_policy_error() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (Arc<Session>, Session) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, _p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer.clone(),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (relay, p2p) = session_pair();
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        let multi = MultiSession::new_with_existing_maps(relay, inbound, udp_inbound);
        engine.install_proxy_replica_session_for_test("mobile-1", multi);

        let sid = SessionId::from_bytes([0xE1; 16]);
        engine.reserve_p2p_session_install(sid, None, Some("peer"));
        let err = match engine
            .install_p2p_session(sid, p2p, engine.task_cancel_token())
            .await
        {
            Ok(_) => panic!("install must fail without group context/access policy"),
            Err(err) => err,
        };
        let text = err.to_string();
        assert!(
            text.contains("group context/access policy"),
            "error should describe missing group access policy, got {text:?}"
        );
        assert!(
            !text.contains("HostFilter"),
            "error should not expose HostFilter as a P2P install dependency: {text:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_p2p_session_installer_rejects_late_install() {
        use crate::p2p::session::{MultiSession, P2pState};
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (Arc<Session>, Session, mpsc::Receiver<()>) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, _p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (closed_tx, closed_rx) = mpsc::channel::<()>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let relay_closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let p2p_closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
                let _ = closed_tx.try_send(());
            });
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                relay_closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                p2p_closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p, closed_rx)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (relay, p2p, mut closed_rx) = session_pair();
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        let multi = MultiSession::new_with_existing_maps(relay, inbound, udp_inbound);
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());

        let cancel = CancellationToken::new();
        let installer = engine.attach_p2p_session_installer_with_cancel(cancel.clone());
        let sid = SessionId::from_bytes([0xE2; 16]);
        engine.reserve_p2p_session_install(sid, None, Some("peer"));
        cancel.cancel();

        let err = match installer.install(sid, p2p).await {
            Ok(_) => panic!("cancelled installer must reject late P2P installs"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("cancelled"),
            "error should mention cancellation, got {err}"
        );
        timeout(Duration::from_secs(1), closed_rx.recv())
            .await
            .expect("cancelled installer should close the incoming session")
            .expect("incoming session closer should run");
        assert!(
            !matches!(multi.p2p_state(), P2pState::Active { .. }),
            "cancelled installer must not publish an active P2P session"
        );
    }

    #[tokio::test]
    async fn p2p_session_reader_end_marks_active_state_idle_for_refill() {
        use crate::p2p::session::{MultiSession, P2pState};
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (Arc<Session>, Session, mpsc::Sender<BinaryMessage>) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, _p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer.clone(),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p, p2p_in_tx)
        }

        fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (relay, p2p, p2p_in_tx) = session_pair();
        let multi = make_multi(relay);
        engine.install_proxy_replica_session_for_test("mobile-1", multi.clone());

        let sid = SessionId::from_bytes([0xB1; 16]);
        engine.reserve_p2p_session_install(sid, None, Some("peer"));
        let _installed = engine
            .install_p2p_session(sid, p2p, engine.task_cancel_token())
            .await
            .expect("install p2p session");
        assert!(
            matches!(multi.p2p_state(), P2pState::Active { session_id, .. } if session_id == sid),
            "test setup should install active P2P state"
        );

        drop(p2p_in_tx);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if multi.p2p().is_none() && matches!(multi.p2p_state(), P2pState::Idle) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("closed direct session should clear active state promptly");
    }

    #[tokio::test]
    async fn p2p_direct_heartbeat_gets_ack_on_same_session() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{unpack, BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (
            Arc<Session>,
            Session,
            mpsc::Sender<BinaryMessage>,
            mpsc::Receiver<PackedMessage>,
        ) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(8);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer.clone(),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p, p2p_in_tx, p2p_out_rx)
        }

        fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (relay, p2p, p2p_in_tx, mut p2p_out_rx) = session_pair();
        let multi = make_multi(relay);
        engine.install_proxy_replica_session_for_test("mobile-1", multi);

        let sid = SessionId::from_bytes([0xB4; 16]);
        engine.reserve_p2p_session_install(sid, None, Some("peer"));
        let _installed = engine
            .install_p2p_session(sid, p2p, engine.task_cancel_token())
            .await
            .expect("install p2p session");

        p2p_in_tx
            .send(BinaryMessage::Heartbeat {
                client_id: "peer".into(),
                timestamp: 12345,
            })
            .await
            .expect("inject p2p heartbeat");

        let ack = timeout(Duration::from_millis(500), async {
            loop {
                let Some(msg) = p2p_out_rx.recv().await else {
                    panic!("p2p outbound channel closed");
                };
                if matches!(
                    unpack(&msg.to_bytes()).expect("decode p2p outbound"),
                    BinaryMessage::HeartbeatAck { timestamp: 12345 }
                ) {
                    break;
                }
            }
        })
        .await;
        assert!(ack.is_ok(), "P2P heartbeat ack should be emitted promptly");
    }

    #[tokio::test]
    async fn p2p_installed_session_sends_periodic_heartbeat_when_business_idle() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{unpack, BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn session_pair() -> (Arc<Session>, Session, mpsc::Receiver<PackedMessage>) {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let (p2p_out_tx, p2p_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_p2p_in_tx, p2p_in_rx) = mpsc::channel::<BinaryMessage>(8);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer.clone(),
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let p2p = Session::new_channeled(
                p2p_out_tx,
                p2p_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            );
            (relay, p2p, p2p_out_rx)
        }

        fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );
        let (relay, p2p, mut p2p_out_rx) = session_pair();
        let multi = make_multi(relay);
        engine.install_proxy_replica_session_for_test("mobile-1", multi);

        let sid = SessionId::from_bytes([0xB5; 16]);
        engine.reserve_p2p_session_install(sid, None, Some("peer"));
        let _installed = engine
            .install_p2p_session(sid, p2p, engine.task_cancel_token())
            .await
            .expect("install p2p session");

        let heartbeat = timeout(Duration::from_millis(1500), p2p_out_rx.recv())
            .await
            .expect("idle P2P session should send a heartbeat before QUIC idle timeout")
            .expect("p2p outbound channel open");
        assert!(matches!(
            unpack(&heartbeat.to_bytes()).expect("decode heartbeat"),
            BinaryMessage::Heartbeat { .. }
        ));
    }

    #[tokio::test]
    async fn p2p_session_install_reservation_can_be_released_without_install() {
        use crate::p2p::session::MultiSession;
        use bytes::Bytes;
        use dashmap::DashMap;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tp_core::p2p_types::SessionId;
        use tp_core::protocol::{BinaryMessage, PackedMessage};
        use tp_transport::session::Session;
        use tp_transport::DropOldestSender;

        fn make_multi() -> Arc<MultiSession> {
            let (relay_out_tx, _relay_out_rx) = mpsc::channel::<PackedMessage>(8);
            let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
            let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
            let relay = Arc::new(Session::new_channeled(
                relay_out_tx,
                relay_in_rx,
                peer,
                closer,
                tokio::spawn(async {}),
                tokio::spawn(async {}),
            ));
            let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
            let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> =
                Arc::new(DashMap::new());
            MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
        }

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        engine.install_proxy_replica_session_for_test("mobile-1", make_multi());
        let sid = SessionId::from_bytes([0xB3; 16]);

        engine.reserve_p2p_session_install(sid, None, Some("peer"));
        assert!(
            engine.has_pending_p2p_session_install_for_test(sid),
            "reservation should be visible before cleanup"
        );

        engine.unreserve_p2p_session_install(sid);
        assert!(
            !engine.has_pending_p2p_session_install_for_test(sid),
            "cleanup must release pending P2P install reservation"
        );

        let first_generation = SessionId::from_bytes([0xB4; 16]);
        let duplicate_generation = SessionId::from_bytes([0xB5; 16]);
        let relation = crate::peer_link_manager::PeerRelationKey::from_canonical_initiator(
            "peer-a-AbCd0001-0",
            "peer-b-AbCd0002-0",
        )
        .expect("canonical relation");
        assert!(engine.reserve_p2p_session_install_for_relation(
            first_generation,
            Some("mobile-1"),
            Some("peer-b-AbCd0002-0"),
            Some(relation.clone()),
        ));
        assert!(
            !engine.reserve_p2p_session_install_for_relation(
                duplicate_generation,
                Some("mobile-1"),
                Some("peer-b-AbCd0002-0"),
                Some(relation.clone()),
            ),
            "one relation must have at most one pending generation"
        );
        engine.unreserve_p2p_session_install(first_generation);
        assert!(engine.reserve_p2p_session_install_for_relation(
            duplicate_generation,
            Some("mobile-1"),
            Some("peer-b-AbCd0002-0"),
            Some(relation),
        ));
        engine.unreserve_p2p_session_install(duplicate_generation);
    }

    #[tokio::test]
    async fn reserved_p2p_installer_rejects_late_install_after_reservation_expires() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (relay, _relay_rx, _relay_closed_rx) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_proxy_replica_session_for_test("peer-b-AbCd0002-0", multi.clone());
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );

        let installer = engine.attach_p2p_session_installer();
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xB6; 16]);
        let relation = crate::peer_link_manager::PeerRelationKey::from_canonical_initiator(
            "peer-a-AbCd0001-0",
            "peer-b-AbCd0002-0",
        )
        .expect("canonical relation");
        assert!(installer.reserve_for_relation(
            session_id,
            Some("peer-b-AbCd0002-0"),
            Some("peer-a-AbCd0001-0"),
            Some(relation),
        ));
        assert_eq!(
            installer.expire_for_session(session_id),
            crate::p2p::installer::P2pInstallExpiration::Expired,
        );

        let (late_session, _late_rx, mut late_closed_rx) = watchdog_channel_session();
        let late_session = match Arc::try_unwrap(late_session) {
            Ok(session) => session,
            Err(_) => panic!("late test session should have one owner"),
        };
        let error = match installer.install_reserved(session_id, late_session).await {
            Ok(_) => panic!("expired reservation must reject a late P2P install"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("reservation"),
            "error should identify the expired reservation: {error}"
        );
        assert!(!multi.has_p2p_session(session_id));
        tokio::time::timeout(Duration::from_secs(1), late_closed_rx.recv())
            .await
            .expect("rejected late P2P session should be closed");
    }

    #[tokio::test]
    async fn p2p_install_expiration_preserves_installed_registry_winner() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (relay, _relay_rx, _relay_closed_rx) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_proxy_replica_session_for_test("peer-b-AbCd0002-0", multi.clone());
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );

        let installer = engine.attach_p2p_session_installer();
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xB7; 16]);
        assert!(installer.reserve_for_session(
            session_id,
            Some("peer-b-AbCd0002-0"),
            Some("peer-a-AbCd0001-0"),
        ));
        let (session, _session_rx, _session_closed_rx) = watchdog_channel_session();
        let session = match Arc::try_unwrap(session) {
            Ok(session) => session,
            Err(_) => panic!("test session should have one owner"),
        };
        let _installed = installer
            .install_reserved(session_id, session)
            .await
            .expect("reserved install wins before timeout expiration");

        assert_eq!(
            installer.expire_for_session(session_id),
            crate::p2p::installer::P2pInstallExpiration::Installed,
        );
        assert!(multi.has_p2p_session(session_id));
    }

    #[tokio::test]
    async fn legacy_unreserved_p2p_installer_path_remains_available() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let (relay, _relay_rx, _relay_closed_rx) = watchdog_channel_session();
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        engine.install_proxy_replica_session_for_test("legacy-client-0", multi.clone());
        engine.set_group_context_for_test(
            "tunnel-test",
            "group-test",
            Arc::new(HostFilter::new(&[], &[]).expect("host filter")),
        );

        let installer = engine.attach_p2p_session_installer();
        let session_id = tp_core::p2p_types::SessionId::from_bytes([0xB9; 16]);
        let (session, _session_rx, _session_closed_rx) = watchdog_channel_session();
        let session = match Arc::try_unwrap(session) {
            Ok(session) => session,
            Err(_) => panic!("legacy test session should have one owner"),
        };

        let _installed = installer
            .install(session_id, session)
            .await
            .expect("explicit legacy installer path may install without a reservation");

        assert!(multi.has_p2p_session(session_id));
    }

    /// `Engine::replicas()` round-trips the resolved fanout for status and
    /// bootstrap diagnostics.
    #[test]
    fn engine_replicas_reads_back_after_install() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        assert_eq!(
            engine.replicas(),
            None,
            "replicas() must be None before any connect resolves fanout"
        );
        *engine.replicas.lock() = Some(3);
        assert_eq!(engine.replicas(), Some(3));
        *engine.replicas.lock() = Some(1);
        assert_eq!(engine.replicas(), Some(1));
    }

    #[test]
    fn transport_dial_timeout_tolerates_slow_gateway_handshake() {
        assert!(
            transport_dial_timeout() >= Duration::from_secs(30),
            "gateway QUIC handshake timeout must allow slow mobile/Mac WAN handshakes"
        );
    }

    #[test]
    fn replica_dial_delay_staggers_only_sidecar_replicas() {
        let stagger = Duration::from_millis(500);

        assert_eq!(replica_dial_delay(0, stagger), Duration::ZERO);
        assert_eq!(replica_dial_delay(1, stagger), Duration::from_millis(500));
        assert_eq!(replica_dial_delay(2, stagger), Duration::from_millis(1000));
        assert_eq!(replica_dial_delay(2, Duration::ZERO), Duration::ZERO);
    }

    /// Tasks registered through `Engine::tasks()` must be drained
    /// by `disconnect()` before it returns. Pre-fix the engine had no
    /// engine-lifetime tracker; bare `tokio::spawn`s for the run-loop
    /// driver and the P2P signaling forwarder leaked past `disconnect`.
    #[tokio::test]
    async fn engine_disconnect_drains_tracked_tasks() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));
        let done = Arc::new(AtomicBool::new(false));

        let done_for_task = done.clone();
        engine.tasks().spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            done_for_task.store(true, Ordering::SeqCst);
        });

        engine.disconnect().await;
        assert!(
            done.load(Ordering::SeqCst),
            "disconnect must wait for tracker tasks to finish"
        );

        // Tracker is replaced on disconnect, so the next spawn lands on a
        // fresh tracker and the next disconnect must drain *that* one.
        let done2 = Arc::new(AtomicBool::new(false));
        let done2_for_task = done2.clone();
        engine.tasks().spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            done2_for_task.store(true, Ordering::SeqCst);
        });
        engine.disconnect().await;
        assert!(
            done2.load(Ordering::SeqCst),
            "second disconnect must drain the post-replace tracker"
        );
    }

    /// `disconnect()` must NOT
    /// deadlock when an engine-lifetime tracked task has no shutdown
    /// signal (e.g. the P2P bootstrap's listener accept loop, whose
    /// `endpoint.accept()` future runs forever absent a `close()`).
    /// The 5 s drain deadline degrades to detached-shutdown semantics
    /// (the historical behaviour) instead of permanently freezing
    /// `disconnect`. The original `engine_disconnect_drains_tracked_tasks`
    /// test only covered the happy path with a 50 ms sleep task.
    #[tokio::test]
    async fn engine_disconnect_does_not_deadlock_on_unstoppable_task() {
        let engine = Engine::new(EngineConfig::default(), Arc::new(NullListener));

        // Simulate the P2P listener accept loop: a future that never resolves.
        engine.tasks().spawn(async move {
            std::future::pending::<()>().await;
        });

        // Drain must complete via the 5 s timeout (test pauses tokio time
        // so it doesn't actually wait wall-clock 5 s).
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(8), engine.disconnect())
            .await
            .expect("disconnect must finish — this deadline guards against deadlock");
        // Sanity: didn't return instantly (the 5 s timeout did fire).
        assert!(
            start.elapsed() >= Duration::from_secs(4),
            "disconnect should have waited at least most of the 5s deadline; \
             got {:?} — did the deadline get bypassed?",
            start.elapsed()
        );
    }
}
