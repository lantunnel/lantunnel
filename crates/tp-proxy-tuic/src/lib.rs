//! TUIC v5 proxy frontend (runs its own QUIC endpoint on a separate port).
//!
//! NOT WIRED INTO ANY SHIPPED BINARY. It is retained for a possible future
//! opt-in Gateway proxy listener.
//!
//! Implements Authenticate (0x00), Connect (0x01), and Packet (0x02, UDP native mode
//! via QUIC datagrams). QUIC-mode packets (sent on uni-streams) are not implemented;
//! only native mode. Dissociate (0x03) cleans up a UDP association.
//!
//! Authentication: the UUID sent in Authenticate names an identity; its secret
//! is the token (hex-encoded 32 bytes).
//!
//! Submodule layout:
//!
//! * [`addr`]   — `Addr` enum + ATYP constants + encode/read helpers +
//!   `build_packet_datagram` (single-packet builder).
//! * [`frag`]   — multi-fragment `Packet` builder (`build_packet_fragments`)
//!   and the per-connection `FragAssembler` reassembler.
//! * [`stats`]  — `TuicOutboundStats` rolling counters + log emitter.
//! * [`stream`] — bi-stream `CONNECT` handler and its half-close-safe pipe.

mod addr;
mod frag;
mod stats;
mod stream;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::{Buf, Bytes};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use quinn::congestion::{BbrConfig, CubicConfig, NewRenoConfig};
use quinn::{Endpoint, EndpointConfig, MtuDiscoveryConfig, ServerConfig, TransportConfig, VarInt};
use tokio::sync::{mpsc, Semaphore};
pub mod backend;

use backend::{TuicAuthenticator, TuicBackend};
use tp_transport::bind_tuned_udp;

use addr::{build_packet_datagram, format_addr, read_addr_sync};
use frag::{build_packet_fragments, FragAssembler};
use stats::TuicOutboundStats;
use stream::handle_bi_stream;

/// TUIC listener options.
#[derive(Debug, Clone)]
pub struct TuicServeOptions {
    /// "bbr" (default) | "cubic" | "new_reno".
    pub congestion_control: String,
    /// "native" (QUIC Datagram) | "quic" (QUIC uni-stream). Affects the
    /// transport chosen for server → client Packet responses; inbound Packets
    /// are accepted on either transport regardless of this setting.
    pub udp_relay_mode: String,
    /// Enable 0-RTT handshake (vulnerable to replay — opt-in).
    pub zero_rtt: bool,
    /// ALPN tokens. Go default is `["h3"]` to blend with HTTP/3.
    pub alpn: Vec<String>,
    /// QUIC max idle timeout, seconds. Go default 30.
    pub max_idle_timeout_secs: u32,
    /// Authenticate command timeout, seconds. Go default 3.
    pub auth_timeout_secs: u32,
    /// Heartbeat interval, seconds. Go default 10. Maps to QUIC keepalive.
    pub heartbeat_secs: u32,
    /// Native-mode bare UDP payloads up to this size are sent as one TUIC
    /// datagram even if Quinn's current PMTUD value is still smaller.
    /// 0 uses the code default.
    pub native_no_fragment_max_payload: usize,
}

impl Default for TuicServeOptions {
    fn default() -> Self {
        Self {
            congestion_control: "bbr".into(),
            udp_relay_mode: "native".into(),
            zero_rtt: false,
            alpn: vec!["h3".into()],
            max_idle_timeout_secs: 30,
            // Bumped from 3 to 15
            // (clash-verge on mobile networks regularly exceeds 3s for
            // the QUIC handshake + Authenticate uni-stream round-trip).
            auth_timeout_secs: 15,
            heartbeat_secs: 10,
            native_no_fragment_max_payload: DEFAULT_NATIVE_NO_FRAGMENT_MAX_PAYLOAD,
        }
    }
}

pub(crate) const TUIC_VER: u8 = 0x05;
const CMD_AUTHENTICATE: u8 = 0x00;
pub(crate) const CMD_CONNECT: u8 = 0x01;
pub(crate) const CMD_PACKET: u8 = 0x02;
pub(crate) const CMD_DISSOCIATE: u8 = 0x03;
const CMD_HEARTBEAT: u8 = 0x04;
const MAX_TUIC_CONNECTIONS: usize = 4096;
const MAX_PREAUTH_BI_STREAMS: usize = 16;
const TUIC_UDP_PAYLOAD_CEILING_BYTES: u16 = 1472;
const DEFAULT_NATIVE_NO_FRAGMENT_MAX_PAYLOAD: usize = 1392;

type UdpAssocKey = (u16, String);
type UdpAssocMap = Arc<DashMap<UdpAssocKey, mpsc::Sender<Bytes>>>;

fn tuned_tuic_transport_config(options: &TuicServeOptions) -> anyhow::Result<TransportConfig> {
    // Transport tuning: the same shape as the gateway-side QUIC transport,
    // with enlarged datagram buffers so the
    // moonlight/sunshine video stream (~500 Mbps / ~50 kpps) doesn't drop
    // frames when a GC pause briefly stalls the reader.
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(
        VarInt::from_u64(options.max_idle_timeout_secs as u64 * 1000)
            .context("max_idle_timeout fits in u62 millis")?
            .into(),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(
        options.heartbeat_secs.max(1) as u64
    )));
    // Flow-control windows aligned with Go TUIC: large enough to sustain
    // 1 Gbps+ over typical mobile RTTs without starving.
    transport.receive_window(VarInt::from_u64(100 * 1024 * 1024).unwrap());
    transport.stream_receive_window(VarInt::from_u64(16 * 1024 * 1024).unwrap());
    transport.send_window(64 * 1024 * 1024);
    // Datagram buffers 8 MiB each — eight times the previous 1 MiB; mirrors
    // the tp-transport tuning so clash-side bursts don't drop frames in
    // quinn's internal queue before the reader drains them.
    transport.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    transport.datagram_send_buffer_size(8 * 1024 * 1024);
    // Keep Quinn's safe 1200-byte initial MTU. Starting at the Ethernet
    // ceiling can black-hole fresh near-1500B UDP packets on PPPoE/VPN/mobile
    // paths before MTUD has proven the size, which shows up as severe
    // Moonlight/Sunshine frame loss. MTUD still probes up to the ceiling
    // below and native TUIC fragmentation keeps oversized packets safe until
    // the path expands.
    let mut mtud = MtuDiscoveryConfig::default();
    mtud.upper_bound(TUIC_UDP_PAYLOAD_CEILING_BYTES);
    transport.mtu_discovery_config(Some(mtud));
    // Congestion control — BBR initial_window 16 MiB matches the tunnel
    // transport and lets STARTUP ramp in one RTT on fast links.
    const INITIAL_WINDOW: u64 = 16 * 1024 * 1024;
    match options.congestion_control.to_ascii_lowercase().as_str() {
        "cubic" => {
            let mut c = CubicConfig::default();
            c.initial_window(INITIAL_WINDOW);
            transport.congestion_controller_factory(Arc::new(c));
        }
        "new_reno" | "newreno" => {
            let mut c = NewRenoConfig::default();
            c.initial_window(INITIAL_WINDOW);
            transport.congestion_controller_factory(Arc::new(c));
        }
        _ => {
            let mut bbr = BbrConfig::default();
            bbr.initial_window(INITIAL_WINDOW);
            transport.congestion_controller_factory(Arc::new(bbr));
        }
    };
    Ok(transport)
}

pub async fn serve(
    listen_addr: SocketAddr,
    tls_cfg: Arc<rustls::ServerConfig>,
    backend: Arc<dyn TuicBackend>,
    auth: Arc<dyn TuicAuthenticator>,
    options: TuicServeOptions,
) -> anyhow::Result<()> {
    // Always override ALPN for the TUIC listener. The shared gateway rustls
    // config carries the client<->gateway ALPN used by tunnel transport.
    // TUIC clients (sing-quic, etc.) advertise `h3` and require
    // their own ALPN on this listener.
    let mut tls_owned = tls_cfg.as_ref().clone();
    tls_owned.alpn_protocols = options.alpn.iter().map(|s| s.as_bytes().to_vec()).collect();
    if tls_owned.alpn_protocols.is_empty() {
        tls_owned.alpn_protocols = vec![b"h3".to_vec()];
    }
    if options.zero_rtt {
        tls_owned.max_early_data_size = u32::MAX;
    }

    let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(tls_owned)
        .with_context(|| "tuic: quic server crypto")?;
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(qsc));

    let transport = tuned_tuic_transport_config(&options)?;
    server_cfg.transport_config(Arc::new(transport));

    // Bind a tuned UDP socket (SO_RCVBUF/SO_SNDBUF = 8 MiB) before handing
    // to quinn. Without this the kernel receive queue is only a few hundred
    // KB and a single scheduler hiccup drops tens of ms of video/audio.
    let std_sock = bind_tuned_udp(listen_addr)
        .with_context(|| format!("tuic: bind tuned udp {listen_addr}"))?;
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("tuic: no quinn runtime (tokio)"))?;
    let mut endpoint_cfg = EndpointConfig::default();
    endpoint_cfg
        .max_udp_payload_size(TUIC_UDP_PAYLOAD_CEILING_BYTES)
        .with_context(|| "tuic: endpoint max_udp_payload_size")?;
    let endpoint = Endpoint::new(endpoint_cfg, Some(server_cfg), std_sock, runtime)?;
    let native_no_fragment_max_payload =
        effective_native_no_fragment_max_payload(options.native_no_fragment_max_payload);
    tracing::info!(
        addr = %listen_addr,
        cc = %options.congestion_control,
        udp_relay = %options.udp_relay_mode,
        zero_rtt = options.zero_rtt,
        heartbeat = options.heartbeat_secs,
        native_no_fragment_max_payload_configured = options.native_no_fragment_max_payload,
        native_no_fragment_max_payload,
        "tuic listening"
    );

    let relay_mode = options.udp_relay_mode.clone();
    // Pre-clamp the Authenticate deadline: `auth_timeout_secs = 0` in YAML
    // would otherwise expire immediately and reject every connection.
    let auth_timeout_secs = options.auth_timeout_secs.max(1) as u64;
    let permits = Arc::new(Semaphore::new(MAX_TUIC_CONNECTIONS));
    while let Some(incoming) = endpoint.accept().await {
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            backend.increment_listener_rejects();
            tracing::warn!(
                max_connections = MAX_TUIC_CONNECTIONS,
                "tuic connection limit reached; rejecting"
            );
            drop(incoming);
            continue;
        };
        let gw = backend.clone();
        let auth = auth.clone();
        let relay_mode = relay_mode.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_connection(
                        conn,
                        gw,
                        auth,
                        relay_mode,
                        auth_timeout_secs,
                        native_no_fragment_max_payload,
                    )
                    .await
                    {
                        tracing::debug!(error = %e, "tuic conn ended");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "tuic handshake failed"),
            }
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UdpRelayMode {
    Native,
    QuicStream,
}

impl UdpRelayMode {
    fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "quic" | "quic_stream" | "stream" => UdpRelayMode::QuicStream,
            _ => UdpRelayMode::Native,
        }
    }
}

struct PacketHandlerContext {
    conn: quinn::Connection,
    backend: Arc<dyn TuicBackend>,
    route_key: String,
    assocs: UdpAssocMap,
    relay_mode: UdpRelayMode,
    frag_asm: Arc<FragAssembler>,
    native_no_fragment_max_payload: usize,
}

async fn handle_connection(
    conn: quinn::Connection,
    backend: Arc<dyn TuicBackend>,
    auth: Arc<dyn TuicAuthenticator>,
    relay_mode_str: String,
    auth_timeout_secs: u64,
    native_no_fragment_max_payload: usize,
) -> anyhow::Result<()> {
    let relay_mode = UdpRelayMode::parse(&relay_mode_str);
    let mut authenticated: Option<String> = None;
    // Bound the window a peer has to complete the Authenticate uni-stream.
    // Before today the TuicServeOptions.auth_timeout_secs field existed
    // but was dead config (comment at top of file acknowledged it). A
    // QUIC-authenticated peer that opened a uni stream and then stalled
    // kept `handle_connection` alive until QUIC's max_idle_timeout (30 s)
    // with a datagram_drain potentially never spawned.
    let auth_deadline = tokio::time::Instant::now() + Duration::from_secs(auth_timeout_secs);
    // Per-target UDP tunnels keyed by (ASSOC_ID, target). A TUIC association
    // can emit packets to multiple destinations; protocol-v2 UDPData carries
    // only conn_id + payload, so each target needs its own inner tunnel.
    let udp_assocs: UdpAssocMap = Arc::new(DashMap::new());
    // Per-connection fragment reassembler: fragmented Packet messages are
    // reassembled before forwarding.
    let frag_asm: Arc<FragAssembler> = Arc::new(FragAssembler::new());
    let mut pending_bi = Vec::new();

    let conn_for_datagrams = conn.clone();
    // Handle to the spawned datagram drain task — started lazily once Auth
    // completes (before auth we can't dispatch because we don't know
    // route_key). Keeping it here so the outer loop can abort it on close.
    let mut datagram_drain: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(auth_deadline), if authenticated.is_none() => {
                anyhow::bail!(
                    "tuic authenticate timed out (>{}s)",
                    auth_timeout_secs
                );
            }
            bi = conn.accept_bi() => {
                let (send, recv) = bi?;
                if let Some(route_key) = authenticated.clone() {
                    spawn_bi_stream(send, recv, backend.clone(), route_key);
                } else if pending_bi.len() < MAX_PREAUTH_BI_STREAMS {
                    pending_bi.push((send, recv));
                } else {
                    tracing::warn!(
                        max_pending = MAX_PREAUTH_BI_STREAMS,
                        "tuic pre-auth bi-stream limit reached; dropped stream"
                    );
                }
            }
            uni = conn.accept_uni() => {
                let mut recv = uni?;
                if authenticated.is_none() {
                    // First uni stream must be Authenticate. Bound the
                    // wall-clock cost of reading it via `auth_deadline`
                    // so a peer that opens the stream and then stalls
                    // doesn't pin the connection until quinn's idle
                    // timeout. Using `timeout_at` (absolute) rather than
                    // `timeout` (relative) so a staggered-read attacker
                    // can't extend the window by hitting each read
                    // just-under the deadline.
                    let auth_reads = async {
                        let mut hdr = [0u8; 2];
                        recv.read_exact(&mut hdr).await?;
                        if hdr[0] != TUIC_VER || hdr[1] != CMD_AUTHENTICATE {
                            anyhow::bail!("expected Authenticate on first uni stream");
                        }
                        let mut uuid = [0u8; 16];
                        recv.read_exact(&mut uuid).await?;
                        let mut token = [0u8; 32];
                        recv.read_exact(&mut token).await?;
                        Ok::<_, anyhow::Error>((uuid, token))
                    };
                    let (uuid, token) =
                        match tokio::time::timeout_at(auth_deadline, auth_reads).await {
                            Ok(Ok(v)) => v,
                            Ok(Err(e)) => return Err(e),
                            Err(_) => anyhow::bail!(
                                "tuic authenticate timed out (>{}s)",
                                auth_timeout_secs
                            ),
                        };
                    let route_key = resolve_tuic_tunnel(
                        &conn,
                        &uuid,
                        &token,
                        auth.as_ref(),
                    )
                    .map_err(|e| anyhow::anyhow!("tuic auth failed: {e}"))?;
                    tracing::debug!(route_key = %route_key, "tuic auth accepted");
                    authenticated = Some(route_key.clone());
                    for (send, recv) in pending_bi.drain(..) {
                        spawn_bi_stream(send, recv, backend.clone(), route_key.clone());
                    }

                    // Spawn the dedicated QUIC-datagram drain task. CRITICAL:
                    // this must NOT be awaited inline in the main select,
                    // because `handle_datagram` may briefly await (tunnel
                    // open, per-target mpsc try_send) and any stall there
                    // would back up quinn's 8 MiB datagram receive buffer
                    // until the kernel drops packets — the exact 4K240 Hz
                    // game-stream pathology we're fixing.
                    let dg_ctx = PacketHandlerContext {
                        conn: conn_for_datagrams.clone(),
                        backend: backend.clone(),
                        route_key: route_key.clone(),
                        assocs: udp_assocs.clone(),
                        relay_mode,
                        frag_asm: frag_asm.clone(),
                        native_no_fragment_max_payload,
                    };
                    datagram_drain = Some(tokio::spawn(async move {
                        while let Ok(data) = dg_ctx.conn.read_datagram().await {
                            if let Err(e) = handle_datagram(data, &dg_ctx).await {
                                tracing::debug!(error = %e, "tuic datagram dispatch error (dropped)");
                            }
                        }
                    }));
                } else {
                    // Post-auth uni streams carry Packet (quic-stream mode) or
                    // Dissociate. Spawn a task to drain it.
                    let packet_ctx = PacketHandlerContext {
                        conn: conn_for_datagrams.clone(),
                        backend: backend.clone(),
                        route_key: authenticated.clone().unwrap(),
                        assocs: udp_assocs.clone(),
                        relay_mode,
                        frag_asm: frag_asm.clone(),
                        native_no_fragment_max_payload,
                    };
                    tokio::spawn(async move {
                        if let Err(e) = handle_uni_packet_stream(recv, packet_ctx).await
                        {
                            tracing::debug!(error = %e, "tuic uni packet stream");
                        }
                    });
                }
            }
            _ = conn.closed() => {
                if let Some(h) = datagram_drain.take() { h.abort(); }
                return Ok(());
            }
        }
    }
}

fn spawn_bi_stream(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    backend: Arc<dyn TuicBackend>,
    route_key: String,
) {
    tokio::spawn(async move {
        if let Err(e) = handle_bi_stream(send, recv, backend, Some(route_key)).await {
            tracing::debug!(error = %e, "tuic stream ended");
        }
    });
}

fn remove_assoc_targets(assocs: &UdpAssocMap, assoc_id: u16) {
    let keys: Vec<UdpAssocKey> = assocs
        .iter()
        .filter(|entry| entry.key().0 == assoc_id)
        .map(|entry| entry.key().clone())
        .collect();
    for key in keys {
        assocs.remove(&key);
    }
}

/// Read a whole Packet message from a uni stream (up to 64KiB) and forward it
/// into the same per-target pipeline used for datagrams.
async fn handle_uni_packet_stream(
    mut recv: quinn::RecvStream,
    context: PacketHandlerContext,
) -> anyhow::Result<()> {
    // A TUIC Packet payload on a uni stream is at most the QUIC stream's
    // flow-control limit; cap at 64 KiB which covers max UDP + header.
    let buf = recv.read_to_end(64 * 1024 + 256).await?;
    if buf.is_empty() {
        return Ok(());
    }
    // Peek command byte — Dissociate also comes on uni streams.
    if buf.len() < 2 {
        anyhow::bail!("uni stream too short");
    }
    if buf[0] != TUIC_VER {
        anyhow::bail!("bad TUIC version on uni");
    }
    match buf[1] {
        CMD_PACKET => handle_packet_bytes(Bytes::from(buf), &context).await,
        CMD_DISSOCIATE => {
            if buf.len() >= 4 {
                let assoc_id = u16::from_be_bytes([buf[2], buf[3]]);
                remove_assoc_targets(&context.assocs, assoc_id);
            }
            Ok(())
        }
        other => anyhow::bail!("unexpected uni command: {other:#x}"),
    }
}

/// Entry point for a TUIC Packet command arriving on either a QUIC datagram
/// (native mode) or a uni-stream (quic mode). Wire:
/// [VER][CMD=0x02][ASSOC_ID:u16][PKT_ID:u16][FRAG_TOTAL:u8][FRAG_ID:u8][SIZE:u16][ADDR][PAYLOAD]
///
/// The per-(ASSOC_ID, target) relay task spun up on first sight handles both
/// directions for that target. Outbound (server → client) packets are sent via
/// the transport selected by `relay_mode`.
async fn handle_datagram(data: Bytes, context: &PacketHandlerContext) -> anyhow::Result<()> {
    handle_packet_bytes(data, context).await
}

async fn handle_packet_bytes(data: Bytes, context: &PacketHandlerContext) -> anyhow::Result<()> {
    let mut b = data.as_ref();
    if b.len() < 2 {
        anyhow::bail!("tuic packet too short (<2 header bytes)");
    }
    let ver = b.get_u8();
    let cmd = b.get_u8();
    if ver != TUIC_VER {
        anyhow::bail!("expected TUIC v{TUIC_VER}, got {ver}");
    }
    // Heartbeat is a 2-byte datagram (`[VER, CMD_HEARTBEAT]`). Accept silently
    // so sing-quic's periodic liveness pings don't tear down the connection.
    if cmd == CMD_HEARTBEAT {
        return Ok(());
    }
    if cmd != CMD_PACKET {
        anyhow::bail!("expected Packet (0x02), got cmd={cmd:#x}");
    }
    if b.len() < 8 {
        anyhow::bail!("tuic packet too short (<10 header bytes)");
    }
    let assoc_id = b.get_u16();
    let pkt_id = b.get_u16();
    let frag_total = b.get_u8();
    let frag_id = b.get_u8();
    let size = b.get_u16() as usize;
    // Validate fragment indices. TUIC v5 allows 1..=N_FRAGS; frag_id is 0-based
    // up to frag_total-1.
    if frag_total == 0 || frag_id >= frag_total {
        anyhow::bail!("invalid fragment header: total={frag_total} id={frag_id}");
    }
    // Address is only present on the FIRST fragment in TUIC v5; successors
    // may carry ADDR_NONE. We treat a leading ADDR_NONE as "use the cached
    // one for this pkt_id".
    let maybe_target = read_addr_sync(&mut b)?;
    if b.len() < size {
        anyhow::bail!("tuic packet payload truncated");
    }
    let payload_start = data.len() - b.len();
    let frag_payload = data.slice(payload_start..payload_start + size);

    // Fast path: single-fragment packet.
    let (target_str, payload) = if frag_total == 1 {
        let tgt = format_addr(&maybe_target);
        (tgt, frag_payload)
    } else {
        // Multi-fragment reassembly.
        match context.frag_asm.accept(
            assoc_id,
            pkt_id,
            frag_id,
            frag_total,
            maybe_target,
            frag_payload,
        ) {
            Some((tgt, full)) => (tgt, full),
            None => return Ok(()), // more fragments pending
        }
    };

    // Find or open a tunnel for this (ASSOC_ID, target).
    //
    // Use DashMap's `entry` API instead of a `get` + `insert` pair: two
    // packets racing for the same fresh key would both see `None` from `get`,
    // both create a channel, and the second `insert` would clobber the first.
    // `entry` is atomic over the Vacant → Occupied transition so only one
    // caller spawns the relay; the loser cheaply clones the winner's sender.
    let assoc_key = (assoc_id, target_str.clone());
    let tx_to_tunnel = match context.assocs.entry(assoc_key.clone()) {
        Entry::Occupied(o) => o.get().clone(),
        Entry::Vacant(v) => {
            // Per-target mpsc sized at 4096 (was 256). At 500 Mbps / ~50 kpps
            // moonlight video, 256 slots is roughly 5 ms of packets and
            // scheduler hiccups spill; 4096 gives roughly 80 ms headroom while
            // still bounding memory.
            let (tx_fwd, mut rx_fwd) = mpsc::channel::<Bytes>(4096);
            // The RefMut returned by `insert` is dropped at the end of this
            // statement, which releases the shard write lock BEFORE we spawn
            // — critical because `tokio::spawn` must not run under a
            // parking_lot write guard (parking_lot is !Send-across-await).
            v.insert(tx_fwd.clone());
            // Outbound and inbound run as independent tasks so a slow
            // direction can never stall the other.
            let gw = context.backend.clone();
            let route_key = context.route_key.clone();
            let target = target_str.clone();
            let conn2 = context.conn.clone();
            let assocs2 = context.assocs.clone();
            let assoc_key_for_task = assoc_key.clone();
            let relay_mode = context.relay_mode;
            let native_no_fragment_max_payload = context.native_no_fragment_max_payload;
            tokio::spawn(async move {
                let tunnel = match gw.open_udp(&route_key, &target).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, assoc_id, "tuic open_udp failed");
                        assocs2.remove(&assoc_key_for_task);
                        return;
                    }
                };
                tracing::debug!(assoc_id, route_key, %target, "tuic udp tunnel opened");
                let (tunnel_sender, mut tunnel_recv) = tunnel.split();

                // Outbound (clash → sunshine direction): drain per-target mpsc
                // and push to the tunnel. Use try_send so the 8192-slot
                // tunnel datagram queue filling never stalls rx_fwd draining
                // — for game streaming, a dropped packet is strictly better
                // than a late one.
                let outbound_task = tokio::spawn(async move {
                    let mut dropped: u64 = 0;
                    while let Some(payload) = rx_fwd.recv().await {
                        match tunnel_sender.try_send(payload) {
                            Ok(()) => {}
                            Err(tp_transport::TrySendKind::Full) => {
                                dropped = dropped.wrapping_add(1);
                                if dropped.is_power_of_two() {
                                    tracing::debug!(
                                        assoc_id,
                                        dropped,
                                        "tuic outbound: tunnel datagram queue full; dropped"
                                    );
                                }
                            }
                            Err(tp_transport::TrySendKind::TooLarge(len)) => {
                                tracing::warn!(
                                    assoc_id,
                                    len,
                                    "tuic outbound: oversized tunnel frame rejected"
                                );
                            }
                            Err(tp_transport::TrySendKind::DatagramUnavailable) => {
                                tracing::warn!(
                                    assoc_id,
                                    "tuic outbound: datagram transport unavailable"
                                );
                                break;
                            }
                            Err(tp_transport::TrySendKind::Closed) => {
                                tracing::debug!(assoc_id, "tuic outbound: tunnel closed");
                                break;
                            }
                        }
                    }
                });

                // Inbound (sunshine → clash direction): read from tunnel,
                // fragment for QUIC datagrams, emit. Isolated drop-and-
                // continue on `send_datagram` failure — a single full
                // quinn internal queue must NOT tear down the whole stream
                // for the association. The previous `break` semantics
                // produced cliffs where a brief kernel stall killed the
                // assoc permanently.
                //
                // Diagnostics: every ~1000 payloads we emit an INFO line
                // with the rolling counts so operators can see at a glance
                // whether the loss is:
                //   - app-layer fragmentation pressure (frag_count >> in_count),
                //   - forced no-fragment sends (forced_single_datagram),
                //   - quinn internal queue exhaustion (Blocked drops),
                //   - payloads too big to fit even as fragments
                //     (drop_unfragmentable),
                //   - a specific send_datagram error variant.
                // `max_dg` min/max values are also reported because PMTUD
                // shifts it during the session.
                let inbound_task = {
                    let conn2 = conn2.clone();
                    let metrics = gw.clone();
                    let target = target.clone();
                    tokio::spawn(async move {
                        let mut pkt_id: u16 = 0;
                        let mut stats = TuicOutboundStats::default();
                        while let Some(payload) = tunnel_recv.recv().await {
                            pkt_id = pkt_id.wrapping_add(1);
                            stats.in_count = stats.in_count.wrapping_add(1);
                            match relay_mode {
                                UdpRelayMode::Native => {
                                    // Live MTU query: quinn's value reflects
                                    // PMTUD state, so caching would strand
                                    // mid-sized packets on reliable uni
                                    // streams after the link expanded.
                                    let max_dg = conn2.max_datagram_size().unwrap_or(1200).max(64);
                                    if max_dg < stats.min_max_dg {
                                        stats.min_max_dg = max_dg;
                                    }
                                    if max_dg > stats.max_max_dg {
                                        stats.max_max_dg = max_dg;
                                    }
                                    if should_force_single_native_datagram(
                                        payload.len(),
                                        native_no_fragment_max_payload,
                                    ) {
                                        stats.forced_single_datagram =
                                            stats.forced_single_datagram.wrapping_add(1);
                                        stats.frag_count = stats.frag_count.wrapping_add(1);
                                        if stats.max_frags_seen == 0 {
                                            stats.max_frags_seen = 1;
                                        }
                                        let pkt = build_packet_datagram(
                                            assoc_id, pkt_id, &target, &payload,
                                        );
                                        if let Err(e) = conn2.send_datagram_wait(pkt).await {
                                            metrics.increment_udp_drops();
                                            match &e {
                                                quinn::SendDatagramError::TooLarge => {
                                                    stats.drop_toolarge =
                                                        stats.drop_toolarge.wrapping_add(1);
                                                }
                                                quinn::SendDatagramError::ConnectionLost(_) => {
                                                    stats.drop_closed =
                                                        stats.drop_closed.wrapping_add(1);
                                                }
                                                _ => {
                                                    stats.drop_other =
                                                        stats.drop_other.wrapping_add(1);
                                                }
                                            }
                                            tracing::trace!(
                                                assoc_id, error = %e,
                                                payload_len = payload.len(),
                                                native_no_fragment_max_payload,
                                                max_dg,
                                                "tuic outbound: forced single native datagram failed"
                                            );
                                        }
                                        let free = conn2.datagram_send_buffer_space();
                                        if free < stats.min_buffer_space {
                                            stats.min_buffer_space = free;
                                        }
                                        stats.maybe_log(assoc_id);
                                        continue;
                                    }
                                    let fragments = build_packet_fragments(
                                        assoc_id, pkt_id, &target, &payload, max_dg,
                                    );
                                    stats.frag_count =
                                        stats.frag_count.wrapping_add(fragments.len() as u64);
                                    if fragments.len() > stats.max_frags_seen {
                                        stats.max_frags_seen = fragments.len();
                                    }
                                    if fragments.is_empty() {
                                        metrics.increment_udp_drops();
                                        stats.drop_unfragmentable =
                                            stats.drop_unfragmentable.wrapping_add(1);
                                        tracing::debug!(
                                            payload_len = payload.len(),
                                            max_dg,
                                            "tuic outbound: payload unfragmentable, dropped"
                                        );
                                        stats.maybe_log(assoc_id);
                                        continue;
                                    }
                                    // `send_datagram` silently evicts older buffered
                                    // datagrams when Quinn's internal queue is full.
                                    // Waiting preserves packet integrity under sustained
                                    // 200 Mbps TUIC native tests and turns queue pressure
                                    // into backpressure instead of hidden packet loss.
                                    for frag in fragments {
                                        if let Err(e) = conn2.send_datagram_wait(frag).await {
                                            metrics.increment_udp_drops();
                                            match &e {
                                                quinn::SendDatagramError::TooLarge => {
                                                    stats.drop_toolarge =
                                                        stats.drop_toolarge.wrapping_add(1);
                                                }
                                                quinn::SendDatagramError::ConnectionLost(_) => {
                                                    stats.drop_closed =
                                                        stats.drop_closed.wrapping_add(1);
                                                }
                                                _ => {
                                                    stats.drop_other =
                                                        stats.drop_other.wrapping_add(1);
                                                }
                                            }
                                            tracing::trace!(
                                                assoc_id, error = %e,
                                                "tuic outbound: send_datagram failed"
                                            );
                                            // Continue with next payload;
                                            // do NOT break the assoc.
                                            break;
                                        }
                                    }
                                    // Sample quinn's remaining buffer space once per payload
                                    // so the rolling log exposes the true queue pressure.
                                    let free = conn2.datagram_send_buffer_space();
                                    if free < stats.min_buffer_space {
                                        stats.min_buffer_space = free;
                                    }
                                    stats.maybe_log(assoc_id);
                                }
                                UdpRelayMode::QuicStream => {
                                    let pkt =
                                        build_packet_datagram(assoc_id, pkt_id, &target, &payload);
                                    if send_packet_uni_stream(&conn2, pkt).await.is_err() {
                                        metrics.increment_udp_drops();
                                        stats.drop_other = stats.drop_other.wrapping_add(1);
                                        tracing::trace!(
                                            assoc_id,
                                            "tuic outbound: uni-stream send failed; dropped"
                                        );
                                    }
                                    stats.maybe_log(assoc_id);
                                }
                            }
                        }
                    })
                };

                tokio::select! {
                    _ = outbound_task => {}
                    _ = inbound_task => {}
                }
                assocs2.remove(&assoc_key_for_task);
            });
            tx_fwd
        }
    };
    // Enqueue on per-target mpsc — try_send so the main datagram drain loop
    // NEVER awaits on a full queue (that would back up quinn's receive
    // buffer and kill packet flow for this AND every other assoc on the
    // connection). A full queue means the outbound tunnel writer is behind
    // schedule; dropping is correct for game streaming.
    match tx_to_tunnel.try_send(payload) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            context.backend.increment_udp_drops();
            tracing::debug!(
                assoc_id,
                "tuic per-target queue full; dropped inbound packet"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Receiver died (tunnel torn down); drop the assoc entry so
            // the next packet re-opens cleanly.
            context.assocs.remove(&assoc_key);
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn parse_packet_for_bench(data: Bytes) -> anyhow::Result<(u16, String, Bytes)> {
    let mut b = data.as_ref();
    if b.len() < 2 {
        anyhow::bail!("tuic packet too short (<2 header bytes)");
    }
    let ver = b.get_u8();
    let cmd = b.get_u8();
    if ver != TUIC_VER {
        anyhow::bail!("expected TUIC v{TUIC_VER}, got {ver}");
    }
    if cmd != CMD_PACKET {
        anyhow::bail!("expected Packet (0x02), got cmd={cmd:#x}");
    }
    if b.len() < 8 {
        anyhow::bail!("tuic packet too short (<10 header bytes)");
    }
    let assoc_id = b.get_u16();
    let _pkt_id = b.get_u16();
    let frag_total = b.get_u8();
    let frag_id = b.get_u8();
    let size = b.get_u16() as usize;
    if frag_total != 1 || frag_id != 0 {
        anyhow::bail!("bench parser expects a single-fragment Packet");
    }
    let target = read_addr_sync(&mut b)?;
    if b.len() < size {
        anyhow::bail!("tuic packet payload truncated");
    }
    let payload_start = data.len() - b.len();
    Ok((
        assoc_id,
        format_addr(&target),
        data.slice(payload_start..payload_start + size),
    ))
}

/// Send a complete TUIC Packet on a fresh uni stream. Used when
/// `udp_relay_mode = "quic"`. The client reassembles Packet messages by
/// reading each stream to EOF, so we must finish the stream after the write.
async fn send_packet_uni_stream(conn: &quinn::Connection, pkt: Bytes) -> anyhow::Result<()> {
    let mut send = conn.open_uni().await?;
    send.write_all(&pkt).await?;
    send.finish()
        .map_err(|e| anyhow::anyhow!("finish uni: {e}"))?;
    Ok(())
}

/// Resolve a TUIC auth request to a tunnel route key. Tries two route_key
/// encodings from the TUIC UUID field that
/// match production Go clients:
///   1. ASCII-trimmed `uuid_bytes` (sing-quic `copy(uuid[:], route_key)`).
///   2. Canonical RFC-4122 UUID string from the 16 raw bytes.
///
/// For the matched proxy route_key, re-derives the TLS exported keying material
/// with `label=uuid_bytes`, `context=password_bytes`, `len=32`, compares to
/// the token sent by the client (constant-time), then returns that credential's
/// route key for the backend.
fn resolve_tuic_tunnel(
    conn: &quinn::Connection,
    uuid_bytes: &[u8; 16],
    expected_token: &[u8; 32],
    auth: &dyn TuicAuthenticator,
) -> Result<String, String> {
    let mut candidates: Vec<String> = Vec::with_capacity(2);
    let end = uuid_bytes
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    if end > 0 {
        if let Ok(s) = std::str::from_utf8(&uuid_bytes[..end]) {
            if s.chars()
                .all(|c| c.is_ascii_graphic() || c == '-' || c == '_')
            {
                candidates.push(s.to_string());
            }
        }
    }
    candidates.push(uuid::Uuid::from_bytes(*uuid_bytes).to_string());

    for claimed in candidates {
        let Some(identity) = auth.identity(&claimed) else {
            continue;
        };
        let mut derived = [0u8; 32];
        conn.export_keying_material(&mut derived, uuid_bytes, &identity.secret)
            .map_err(|e| format!("export_keying_material: {e:?}"))?;
        if ct_eq(&derived, expected_token) {
            return Ok(identity.route_key);
        }
    }
    Err("unknown identity or bad token".into())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    d == 0
}

fn should_force_single_native_datagram(
    payload_len: usize,
    native_no_fragment_max_payload: usize,
) -> bool {
    payload_len <= native_no_fragment_max_payload
}

fn effective_native_no_fragment_max_payload(configured: usize) -> usize {
    if configured == 0 {
        DEFAULT_NATIVE_NO_FRAGMENT_MAX_PAYLOAD
    } else {
        configured
    }
}

/// Regression guards for the `assocs` DashMap race fix. The old code did a
/// `get` + `insert` pair for a fresh key, which admitted a TOCTOU where two
/// concurrent first-packets both passed the `None` check and the second
/// `insert` clobbered the first, orphaning its relay task. The fix uses
/// `entry(...)` — these tests exercise the contract we now rely on.
#[cfg(test)]
mod assoc_entry_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// The single-threaded happy path: Vacant inserts; the subsequent Occupied
    /// branch must hand back a CLONE of the same sender, not a new one. If
    /// the fix regressed into "always insert", both senders would feed
    /// different receivers and packets would fan out incorrectly.
    #[tokio::test]
    async fn entry_vacant_then_occupied_returns_same_sender() {
        let assocs: UdpAssocMap = Arc::new(DashMap::new());
        let key = (7u16, "1.2.3.4:53".to_string());
        let (tx, mut rx) = mpsc::channel::<Bytes>(4);
        match assocs.entry(key.clone()) {
            Entry::Occupied(_) => panic!("first call must be Vacant"),
            Entry::Vacant(v) => {
                v.insert(tx.clone());
            }
        }
        let second = match assocs.entry(key) {
            Entry::Occupied(o) => o.get().clone(),
            Entry::Vacant(_) => panic!("second call must be Occupied"),
        };
        second.send(Bytes::from_static(b"hi")).await.unwrap();
        let got = rx.recv().await.expect("receiver must see the packet");
        assert_eq!(got, Bytes::from_static(b"hi"));
    }

    /// Stress race: many tasks pile on the same fresh target key. Exactly one
    /// must win the Vacant branch; everyone else must converge on the same
    /// sender. We verify "exactly one spawn" by counting `winner` increments
    /// and by asserting the final `assocs` size is 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn entry_is_race_free_under_contention() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let assocs: UdpAssocMap = Arc::new(DashMap::new());
        let winner = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let assocs = assocs.clone();
            let winner = winner.clone();
            handles.push(tokio::spawn(async move {
                match assocs.entry((42u16, "1.2.3.4:53".to_string())) {
                    Entry::Occupied(o) => o.get().clone(),
                    Entry::Vacant(v) => {
                        let (tx, _rx) = mpsc::channel::<Bytes>(4);
                        v.insert(tx.clone());
                        winner.fetch_add(1, Ordering::SeqCst);
                        tx
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            winner.load(Ordering::SeqCst),
            1,
            "exactly one task must take the Vacant branch"
        );
        assert_eq!(assocs.len(), 1, "only one assoc entry should exist");
    }

    #[test]
    fn same_assoc_distinct_targets_keep_distinct_tunnels() {
        let assocs: UdpAssocMap = Arc::new(DashMap::new());
        let (tx_a, _rx_a) = mpsc::channel::<Bytes>(1);
        let (tx_b, _rx_b) = mpsc::channel::<Bytes>(1);
        assocs.insert((7, "1.1.1.1:53".into()), tx_a);
        assocs.insert((7, "8.8.8.8:53".into()), tx_b);
        assert_eq!(assocs.len(), 2);
    }

    #[test]
    fn dissociate_removes_all_targets_for_assoc_only() {
        let assocs: UdpAssocMap = Arc::new(DashMap::new());
        let (tx_a, _rx_a) = mpsc::channel::<Bytes>(1);
        let (tx_b, _rx_b) = mpsc::channel::<Bytes>(1);
        let (tx_c, _rx_c) = mpsc::channel::<Bytes>(1);
        assocs.insert((7, "1.1.1.1:53".into()), tx_a);
        assocs.insert((7, "8.8.8.8:53".into()), tx_b);
        assocs.insert((8, "9.9.9.9:53".into()), tx_c);

        remove_assoc_targets(&assocs, 7);

        assert!(!assocs.contains_key(&(7, "1.1.1.1:53".into())));
        assert!(!assocs.contains_key(&(7, "8.8.8.8:53".into())));
        assert!(assocs.contains_key(&(8, "9.9.9.9:53".into())));
    }
}

#[cfg(test)]
mod native_no_fragment_policy_tests {
    use super::*;

    #[test]
    fn zero_uses_default_native_no_fragment_limit() {
        let limit = effective_native_no_fragment_max_payload(0);
        assert_eq!(limit, DEFAULT_NATIVE_NO_FRAGMENT_MAX_PAYLOAD);
        assert!(should_force_single_native_datagram(1392, limit));
        assert!(!should_force_single_native_datagram(1393, limit));
    }

    #[test]
    fn native_no_fragment_forces_datagram_up_to_payload_limit() {
        assert!(should_force_single_native_datagram(1200, 1200));
        assert!(should_force_single_native_datagram(1400, 1400));
        assert!(!should_force_single_native_datagram(1401, 1400));
    }

    #[test]
    fn tuic_transport_keeps_safe_initial_mtu_and_probes_to_ceiling() {
        let cfg = tuned_tuic_transport_config(&TuicServeOptions::default()).unwrap();
        let debug = format!("{cfg:?}");
        assert!(
            debug.contains("initial_mtu: 1200"),
            "tuic transport must not assume Ethernet MTU before PMTUD: {debug}"
        );
        assert!(
            debug.contains("upper_bound: 1472"),
            "TUIC MTUD should still be allowed to probe up to the IPv4 UDP payload ceiling: {debug}"
        );
    }
}
