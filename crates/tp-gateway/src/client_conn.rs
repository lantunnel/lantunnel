//! A single authenticated V2 Peer attachment. Owns the transport session and
//! demultiplexes traffic for exact-Peer relay routes.
//!
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use nonzero_ext::nonzero;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Instant};
use tp_core::bandwidth::BandwidthLimiter;
use tp_core::protocol::BinaryMessage;
use tp_metrics::MetricsManager;
use tp_transport::{
    AuthParams, DatagramReceiver, DropOldestSender, Session, SessionReceiver, SessionSender,
    TcpFlowIncoming,
};

#[cfg(not(test))]
const SCOPE_REVALIDATE_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const SCOPE_REVALIDATE_INTERVAL: Duration = Duration::from_millis(25);

/// Per-`ClientConn` inbound message counters. Every tunnel message that
/// arrives from the remote client goes through `handle_inbound`, which
/// bumps the corresponding field. A periodic summary task (spawned in
/// `ClientConn::run`) reads these every 10 s and logs a `gateway client
/// conn summary` line so we can answer exactly two questions:
///
/// 1. **Did gateway see each UdpData the client handed to Quinn?** Compare
///    `msg_udp_data` here with the client-side `udp_handed_to_quinn` from
///    `tunnel replica summary`. Compare that with `udp_scheduler_accepted`
///    to separate local sender eviction from tunnel-level loss.
/// 2. **Where inside the gateway do UDP frames go missing?** Between
///    `msg_udp_data` and the proxy-outbound counter (TUIC's `in_count`
///    or SOCKS5's reply-pump `recv_count`), the diff is:
///    `msg_udp_data_dropped_no_conn` (target conn_id gone — tunnel teardown
///    race), `msg_udp_data_dropped_full` (newer real-time UDP replaced an older
///    unconsumed packet).
#[derive(Debug, Default)]
pub struct ClientConnStats {
    pub msg_udp_data: AtomicU64,
    pub msg_udp_data_dropped_no_conn: AtomicU64,
    pub msg_udp_data_dropped_full: AtomicU64,
    pub relay_udp_forward_ok: AtomicU64,
    pub relay_udp_forward_dropped_full: AtomicU64,
    pub relay_udp_forward_dropped_too_large: AtomicU64,
    pub relay_udp_forward_closed: AtomicU64,
    pub relay_udp_forward_bytes: AtomicU64,
    pub msg_data: AtomicU64,
    pub msg_data_dropped_no_conn: AtomicU64,
    pub msg_connect_response: AtomicU64,
    pub msg_heartbeat: AtomicU64,
    pub msg_close: AtomicU64,
    pub msg_other: AtomicU64,
    pub bytes_udp_in: AtomicU64,
    pub bytes_data_in: AtomicU64,
}

/// Per-`ClientConn` rate limiter for P2P signaling. Two independent buckets
/// at 1 token/sec each: one gates inbound `P2pAnnounce`, the other gates
/// inbound `P2pOffer`. An authed client could otherwise multicast
/// `P2pOffer` to every known `client_id` and force the cluster into cooldown
/// loops, or spam `P2pAnnounce` to thrash the peer registry. Reuses the
/// `governor` pattern from `tp_core::bandwidth::BandwidthLimiter` (different
/// unit — messages instead of bytes — so the struct itself isn't shared).
struct P2pSignalingRateLimiter {
    announce: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    offer: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
}

impl P2pSignalingRateLimiter {
    fn new() -> Self {
        let announce_quota = Quota::per_second(nonzero!(1u32));
        let offer_quota = Quota::per_second(nonzero!(1u32)).allow_burst(nonzero!(P2P_OFFER_BURST));
        Self {
            announce: RateLimiter::direct(announce_quota),
            offer: RateLimiter::direct(offer_quota),
        }
    }

    fn try_announce(&self) -> bool {
        self.announce.check().is_ok()
    }

    fn try_offer(&self) -> bool {
        self.offer.check().is_ok()
    }
}

use crate::tunneled::TunneledConn;

const P2P_OFFER_BURST: u32 = 16;
const P2P_V2_PEER_OFFLINE: &str = "P2P V2 peer offline";

fn quic_udp_datagram_unavailable(sender: &SessionSender) -> bool {
    sender.udp_data_mode() == tp_transport::UdpDataMode::QuicDatagramRequired
        && !sender.udp_datagram_available()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthenticatedPeerV2 {
    pub tunnel_id: String,
    pub peer_id: String,
    pub replica_id: String,
    pub overlay_ip: Ipv4Addr,
}

pub struct ClientConn {
    pub params: AuthParams,
    /// Minimal identity established by the V2 Scope + challenge proof chain.
    /// It contains no Peer private key or transport secret.
    authenticated_peer_v2: AuthenticatedPeerV2,
    /// Cloneable handle producers use to push onto the session.
    sender: SessionSender,
    /// Stream (reliable) receiver half, taken by `run()`.
    receiver: Mutex<Option<SessionReceiver>>,
    /// Datagram (UDP fast path) receiver half, taken by `run()`. `None` if
    /// the underlying transport doesn't negotiate a datagram channel
    /// (WebSocket / gRPC).
    datagram_receiver: Mutex<Option<DatagramReceiver>>,
    /// TCP inbound channels keyed by conn_id.
    inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
    /// UDP inbound channels keyed by conn_id.
    udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    /// Pending connect acks keyed by conn_id.
    pending: Arc<DashMap<String, oneshot::Sender<std::result::Result<(), String>>>>,
    /// Shared gateway-wide P2P peer registry. Inbound `P2pAnnounce` upserts
    /// the client's endpoint here so peer-signaling (P2pOffer/P2pAnswer in
    /// later tasks) can look it up by `client_id`.
    peers: Arc<crate::p2p::PeerRegistry>,
    /// Weak back-reference to the owning `Gateway`. `Weak` avoids the
    /// `Arc<Gateway>` ↔ `ClientConn` cycle.
    gateway: Weak<crate::Gateway>,
    pub limiter: Arc<BandwidthLimiter>,
    pub metrics: Arc<MetricsManager>,
    /// Per-client inbound-message counters + drop counters. Read every
    /// 10 s by the summary task spawned inside `run()`.
    stats: Arc<ClientConnStats>,
    /// Leaky-bucket on inbound P2P signaling. 1 announce/sec, 1 offer/sec.
    p2p_signaling_limiter: P2pSignalingRateLimiter,
}

impl ClientConn {
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor keeps the existing gateway dependency wiring explicit"
    )]
    pub(crate) fn new(
        params: AuthParams,
        session: Session,
        limiter: Arc<BandwidthLimiter>,
        metrics: Arc<MetricsManager>,
        peers: Arc<crate::p2p::PeerRegistry>,
        gateway: Weak<crate::Gateway>,
        identity: AuthenticatedPeerV2,
    ) -> Arc<Self> {
        // Split the session eagerly so producers (TunneledConn, TunneledDatagram,
        // heartbeat responder) can hand out `SessionSender` clones. `run()`
        // drives the two receiver halves (stream + datagram) on independent
        // tasks — this keeps UDP game-stream frames from queueing behind a
        // slow TCP consumer.
        let (sender, stream_rx, datagram_rx) = session.split();
        Arc::new(Self {
            params,
            authenticated_peer_v2: identity,
            sender,
            receiver: Mutex::new(Some(stream_rx)),
            datagram_receiver: Mutex::new(datagram_rx),
            inbound: Arc::new(DashMap::new()),
            udp_inbound: Arc::new(DashMap::new()),
            pending: Arc::new(DashMap::new()),
            peers,
            gateway,
            limiter,
            metrics,
            stats: Arc::new(ClientConnStats::default()),
            p2p_signaling_limiter: P2pSignalingRateLimiter::new(),
        })
    }

    /// Session sender clone — the handle producers use to push messages onto
    /// the underlying QUIC stream / datagram queue.
    pub fn sender(&self) -> SessionSender {
        self.sender.clone()
    }

    pub(crate) fn authenticated_peer_v2(&self) -> &AuthenticatedPeerV2 {
        &self.authenticated_peer_v2
    }

    async fn wait_until_scope_is_invalid(&self) {
        let mut ticker = tokio::time::interval(SCOPE_REVALIDATE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval`'s first tick is immediate. Delay the first validation so
        // registration's synchronous post-check remains the single race
        // fence, then monitor quiet sessions at a bounded cadence.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(gateway) = self.gateway.upgrade() else {
                // Unit fixtures may intentionally construct an unattached
                // ClientConn. A production session keeps Gateway alive for
                // the duration of run_client_session.
                continue;
            };
            if !gateway.client_scope_identity_is_current(self) {
                tracing::warn!(
                    tunnel_id = %self.authenticated_peer_v2.tunnel_id,
                    client_id = %self.authenticated_peer_v2.replica_id,
                    peer_id = %self.authenticated_peer_v2.peer_id,
                    "V2 Scope identity fence changed; closing live session"
                );
                self.sender.close();
                return;
            }
        }
    }

    pub(crate) fn record_relay_udp_forward_ok(&self, payload_len: usize) {
        self.stats
            .relay_udp_forward_ok
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .relay_udp_forward_bytes
            .fetch_add(payload_len as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_relay_udp_forward_full(&self) {
        self.stats
            .relay_udp_forward_dropped_full
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_relay_udp_forward_too_large(&self) {
        self.stats
            .relay_udp_forward_dropped_too_large
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_relay_udp_forward_closed(&self) {
        self.stats
            .relay_udp_forward_closed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Drive the session — returns when the peer disconnects. Runs three
    /// independent reader loops:
    ///   • `control_loop` drains the reliable control receiver (Heartbeat,
    ///     P2P signaling, …) when the transport negotiated one.
    ///   • `stream_loop` drains the reliable bidi-stream receiver (Connect,
    ///     Data, Close, …).
    ///   • `datagram_loop` drains the QUIC-datagram receiver (UdpData).
    ///
    /// All feed the same `handle_inbound` match, but sit on independent
    /// mpsc queues so slow TCP-side consumers cannot stall heartbeat or UDP
    /// delivery.
    pub async fn run(self: &Arc<Self>) {
        let mut stream_rx = self.receiver.lock().await.take();
        let control_rx = stream_rx.as_mut().and_then(|rx| rx.take_control_receiver());
        let tcp_flow_rx = stream_rx
            .as_mut()
            .and_then(|rx| rx.take_tcp_flow_receiver());
        let dg_rx = self.datagram_receiver.lock().await.take();

        let control_task = {
            let me = self.clone();
            async move {
                if let Some(mut rx) = control_rx {
                    while let Some(m) = rx.recv().await {
                        me.handle_inbound(m).await;
                    }
                }
            }
        };

        let stream_task = {
            let me = self.clone();
            async move {
                if let Some(mut rx) = stream_rx {
                    while let Some(m) = rx.recv_data().await {
                        me.handle_inbound(m).await;
                    }
                }
            }
        };

        let datagram_task = {
            let me = self.clone();
            async move {
                if let Some(mut rx) = dg_rx {
                    while let Some(m) = rx.recv().await {
                        me.handle_inbound(m).await;
                    }
                }
            }
        };

        let tcp_flow_task = {
            let me = self.clone();
            async move {
                if let Some(mut rx) = tcp_flow_rx {
                    while let Some(incoming) = rx.recv().await {
                        let me = me.clone();
                        tokio::spawn(async move {
                            me.handle_tcp_flow_stream(incoming).await;
                        });
                    }
                }
            }
        };

        // Periodic gateway-side summary — mirror of the client-side
        // `tunnel replica summary` but from the server's point of view.
        // Every 10 s, emit per-client counters + tunnel-QUIC health so we
        // can correlate with the matching client log and find the exact
        // hop where UDP frames are lost.
        let summary_task = {
            let stats = self.stats.clone();
            let route_stats = self.sender.udp_route_stats();
            let sender_clone = self.sender.clone();
            let inbound_map = self.inbound.clone();
            let udp_inbound_map = self.udp_inbound.clone();
            let client_id = self.params.client_id.clone();
            let group_id = self.params.group_id.clone();
            let peer = self.params.peer_addr;
            tokio::spawn(async move {
                let mut t = tokio::time::interval(Duration::from_secs(10));
                t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                t.tick().await;
                loop {
                    t.tick().await;
                    let max_dg = sender_clone.current_max_datagram_size();
                    let buf_space = sender_clone.current_datagram_send_buffer_space();
                    let tunnel_health = sender_clone.stats();
                    let buf_space_min = route_stats.take_datagram_send_buffer_space_min();
                    let buf_space_zero_count =
                        route_stats.take_datagram_send_buffer_space_zero_count();
                    tracing::info!(
                        %client_id,
                        %group_id,
                        %peer,
                        active_tcp = inbound_map.len(),
                        active_udp = udp_inbound_map.len(),
                        // Tunnel-QUIC health (gateway side — gateway is
                        // READER for client→gateway UdpData, WRITER for
                        // gateway→client Connect/Data/Close/HeartbeatAck).
                        max_datagram_size = ?max_dg,
                        tunnel_rtt_ms = tunnel_health.rtt.as_millis(),
                        tunnel_loss_rate = tunnel_health.loss_rate,
                        tunnel_pto_count = tunnel_health.pto_count,
                        tunnel_send_buf_space = ?buf_space,
                        tunnel_send_buf_space_min = ?buf_space_min,
                        tunnel_send_buf_space_zero_count = buf_space_zero_count,
                        udp_scheduler_accepted = route_stats
                            .datagram_accepted_to_scheduler
                            .load(Ordering::Relaxed),
                        udp_stream_fallback =
                            route_stats.stream_fallback.load(Ordering::Relaxed),
                        udp_dropped_full =
                            route_stats.dropped_full.load(Ordering::Relaxed),
                        udp_assoc_evicted = route_stats
                            .datagram_per_association_evicted
                            .load(Ordering::Relaxed),
                        udp_global_evicted = route_stats
                            .datagram_global_budget_evicted
                            .load(Ordering::Relaxed),
                        last_fallback_packed_len =
                            route_stats.last_fallback_packed_len.load(Ordering::Relaxed),
                        last_fallback_max_dg =
                            route_stats.last_fallback_max_dg.load(Ordering::Relaxed),
                        udp_handed_to_quinn =
                            route_stats.datagram_write_ok.load(Ordering::Relaxed),
                        udp_quinn_error =
                            route_stats.datagram_write_err.load(Ordering::Relaxed),
                        dg_recv_ok = route_stats.datagram_recv_ok.load(Ordering::Relaxed),
                        udp_inbound_dropped =
                            route_stats.datagram_recv_dropped.load(Ordering::Relaxed),
                        dg_recv_decode_err =
                            route_stats.datagram_recv_decode_err.load(Ordering::Relaxed),
                        // Per-message dispatch counters.
                        in_udp = stats.msg_udp_data.load(Ordering::Relaxed),
                        in_udp_no_conn =
                            stats.msg_udp_data_dropped_no_conn.load(Ordering::Relaxed),
                        in_udp_full = stats.msg_udp_data_dropped_full.load(Ordering::Relaxed),
                        in_udp_bytes = stats.bytes_udp_in.load(Ordering::Relaxed),
                        relay_udp_fwd_ok =
                            stats.relay_udp_forward_ok.load(Ordering::Relaxed),
                        relay_udp_fwd_full =
                            stats.relay_udp_forward_dropped_full.load(Ordering::Relaxed),
                        relay_udp_fwd_too_large =
                            stats.relay_udp_forward_dropped_too_large.load(Ordering::Relaxed),
                        relay_udp_fwd_closed =
                            stats.relay_udp_forward_closed.load(Ordering::Relaxed),
                        relay_udp_fwd_bytes =
                            stats.relay_udp_forward_bytes.load(Ordering::Relaxed),
                        in_tcp = stats.msg_data.load(Ordering::Relaxed),
                        in_tcp_no_conn = stats.msg_data_dropped_no_conn.load(Ordering::Relaxed),
                        in_tcp_bytes = stats.bytes_data_in.load(Ordering::Relaxed),
                        in_hb = stats.msg_heartbeat.load(Ordering::Relaxed),
                        in_connect_resp = stats.msg_connect_response.load(Ordering::Relaxed),
                        in_close = stats.msg_close.load(Ordering::Relaxed),
                        in_other = stats.msg_other.load(Ordering::Relaxed),
                        "gateway client conn summary"
                    );
                }
            })
        };

        // Run all readers until transport teardown drains them, while also
        // enforcing the V2 Scope identity fence for idle sessions. Dropping
        // the reader future is intentional when the guard
        // wins; closing the transport wakes remote I/O and run_client_session
        // performs registry cleanup immediately after this returns.
        let readers = async {
            tokio::join!(control_task, stream_task, datagram_task, tcp_flow_task);
        };
        tokio::select! {
            _ = readers => {}
            _ = self.wait_until_scope_is_invalid() => {}
        }
        summary_task.abort();

        let route_stats = self.sender.udp_route_stats();
        let tunnel_health = self.sender.stats();
        tracing::info!(
            client_id = %self.params.client_id,
            group_id = %self.params.group_id,
            peer = %self.params.peer_addr,
            active_tcp = self.inbound.len(),
            active_udp = self.udp_inbound.len(),
            tunnel_rtt_ms = tunnel_health.rtt.as_millis(),
            tunnel_loss_rate = tunnel_health.loss_rate,
            tunnel_pto_count = tunnel_health.pto_count,
            udp_scheduler_accepted = route_stats
                .datagram_accepted_to_scheduler
                .load(Ordering::Relaxed),
            udp_stream_fallback = route_stats.stream_fallback.load(Ordering::Relaxed),
            udp_dropped_full = route_stats.dropped_full.load(Ordering::Relaxed),
            udp_assoc_evicted = route_stats
                .datagram_per_association_evicted
                .load(Ordering::Relaxed),
            udp_global_evicted = route_stats
                .datagram_global_budget_evicted
                .load(Ordering::Relaxed),
            udp_handed_to_quinn = route_stats.datagram_write_ok.load(Ordering::Relaxed),
            udp_quinn_error = route_stats.datagram_write_err.load(Ordering::Relaxed),
            dg_recv_ok = route_stats.datagram_recv_ok.load(Ordering::Relaxed),
            udp_inbound_dropped = route_stats.datagram_recv_dropped.load(Ordering::Relaxed),
            dg_recv_decode_err = route_stats.datagram_recv_decode_err.load(Ordering::Relaxed),
            in_udp = self.stats.msg_udp_data.load(Ordering::Relaxed),
            in_udp_no_conn = self
                .stats
                .msg_udp_data_dropped_no_conn
                .load(Ordering::Relaxed),
            in_udp_full = self
                .stats
                .msg_udp_data_dropped_full
                .load(Ordering::Relaxed),
            in_udp_bytes = self.stats.bytes_udp_in.load(Ordering::Relaxed),
            relay_udp_fwd_ok = self.stats.relay_udp_forward_ok.load(Ordering::Relaxed),
            relay_udp_fwd_full = self
                .stats
                .relay_udp_forward_dropped_full
                .load(Ordering::Relaxed),
            relay_udp_fwd_too_large = self
                .stats
                .relay_udp_forward_dropped_too_large
                .load(Ordering::Relaxed),
            relay_udp_fwd_closed = self.stats.relay_udp_forward_closed.load(Ordering::Relaxed),
            relay_udp_fwd_bytes = self.stats.relay_udp_forward_bytes.load(Ordering::Relaxed),
            in_tcp = self.stats.msg_data.load(Ordering::Relaxed),
            in_tcp_no_conn = self.stats.msg_data_dropped_no_conn.load(Ordering::Relaxed),
            in_tcp_bytes = self.stats.bytes_data_in.load(Ordering::Relaxed),
            in_hb = self.stats.msg_heartbeat.load(Ordering::Relaxed),
            in_connect_resp = self.stats.msg_connect_response.load(Ordering::Relaxed),
            in_close = self.stats.msg_close.load(Ordering::Relaxed),
            in_other = self.stats.msg_other.load(Ordering::Relaxed),
            "gateway client conn final summary"
        );

        self.sender.close();

        // Release metrics slots for every in-flight conn BEFORE clearing the
        // inbound maps. Without this, `MetricsManager.connections` entries
        // orphaned at tunnel teardown stay forever and `active_connections`
        // is permanently inflated by the count of conns open at disconnect
        // — one of the slow-OOM paths in the gateway.
        //
        // Collecting keys first (rather than calling close_connection from
        // inside iter()) keeps DashMap shard lock hold-time O(keys) of a
        // cheap String clone, not O(keys) of a DashMap write on another
        // shard.
        let tcp_keys: Vec<String> = self.inbound.iter().map(|e| e.key().clone()).collect();
        let udp_keys: Vec<String> = self.udp_inbound.iter().map(|e| e.key().clone()).collect();
        self.inbound.clear();
        self.udp_inbound.clear();
        for k in tcp_keys.iter().chain(udp_keys.iter()) {
            self.metrics.close_connection(k);
        }
        self.pending.clear();
    }

    #[tracing::instrument(
        level = "debug",
        skip(self, msg),
        fields(client_id = %self.params.client_id, group_id = %self.params.group_id)
    )]
    async fn handle_inbound(self: &Arc<Self>, msg: BinaryMessage) {
        // The isolated Static Relay acceptance clone keeps V2 membership and
        // PeerLink key agreement alive while disabling every Direct lane. A
        // zero-candidate signed V2 Offer/Answer derives the encrypted Relay
        // keys without starting UDP punching. Legacy and candidate-bearing
        // signaling remains fail-closed when Direct is disabled.
        let p2p_direct_enabled = self
            .gateway
            .upgrade()
            .map(|gw| gw.p2p_config.enabled)
            .unwrap_or(false);
        if !p2p_direct_enabled {
            let relay_only_allowed = match &msg {
                BinaryMessage::P2pAnnounce { .. } => true,
                BinaryMessage::P2pOfferV2 { signed_offer, .. } => {
                    tp_core::peer_link_crypto::P2pOfferV2::from_wire_bytes(signed_offer)
                        .is_ok_and(|offer| offer.candidates.is_empty())
                }
                BinaryMessage::P2pAnswerV2 { signed_answer, .. } => {
                    tp_core::peer_link_crypto::P2pAnswerV2::from_wire_bytes(signed_answer)
                        .is_ok_and(|answer| answer.candidates.is_empty())
                }
                _ => false,
            };
            if matches!(
                msg,
                BinaryMessage::P2pAnnounce { .. }
                    | BinaryMessage::P2pOffer { .. }
                    | BinaryMessage::P2pAnswer { .. }
                    | BinaryMessage::P2pOfferV2 { .. }
                    | BinaryMessage::P2pAnswerV2 { .. }
                    | BinaryMessage::P2pPunchSync { .. }
                    | BinaryMessage::P2pSessionReady { .. }
                    | BinaryMessage::P2pTeardown { .. }
                    | BinaryMessage::P2pProbe { .. }
                    | BinaryMessage::P2pProbeAck { .. }
                    | BinaryMessage::P2pAnnounceAck { .. }
                    | BinaryMessage::P2pPeerHint { .. }
            ) && !relay_only_allowed
            {
                return;
            }
        }
        match msg {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
                if network == "udp" && quic_udp_datagram_unavailable(&self.sender) {
                    let _ = self
                        .sender
                        .send(BinaryMessage::ConnectResponse {
                            conn_id,
                            success: false,
                            error: "datagram transport unavailable".into(),
                        })
                        .await;
                    return;
                }
                let result = match self.gateway.upgrade() {
                    Some(gw) => {
                        gw.relay_client_connect(self, conn_id.clone(), network, address)
                            .await
                    }
                    None => Err(anyhow::anyhow!("gateway shutting down")),
                };
                if let Err(e) = result {
                    tracing::debug!(
                        %conn_id,
                        client_id = %self.params.client_id,
                        error = %e,
                        "client relay connect failed details"
                    );
                    let _ = self
                        .sender
                        .send(BinaryMessage::ConnectResponse {
                            conn_id,
                            success: false,
                            error: "relay connect failed".into(),
                        })
                        .await;
                }
            }
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            } => {
                self.stats
                    .msg_connect_response
                    .fetch_add(1, Ordering::Relaxed);
                if let Some((_, ch)) = self.pending.remove(&conn_id) {
                    let _ = ch.send(if success { Ok(()) } else { Err(error) });
                } else if let Some(gw) = self.gateway.upgrade() {
                    let _ = gw
                        .forward_client_relay(
                            self,
                            BinaryMessage::ConnectResponse {
                                conn_id,
                                success,
                                error,
                            },
                        )
                        .await;
                }
            }
            BinaryMessage::Data { conn_id, payload } => {
                self.stats.msg_data.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .bytes_data_in
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                // Clone the Sender out of the DashMap Ref and drop the Ref
                // BEFORE any await. Holding a DashMap Ref across await keeps
                // a parking_lot read lock on the shard for as long as the
                // future is suspended; a concurrent `inbound.insert(...)`
                // from `open()` would then block the tokio worker thread
                // (parking_lot write() is sync), starving the accept loops.
                let tx = self.inbound.get(&conn_id).map(|r| r.clone());
                if let Some(tx) = tx {
                    let payload_len = payload.len();
                    let gateway = self.gateway.upgrade();
                    self.limiter.acquire(payload_len).await;
                    let permit = match tx.reserve().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            self.metrics.close_connection(&conn_id);
                            self.stats
                                .msg_data_dropped_no_conn
                                .fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    };
                    if gateway.as_ref().is_some_and(|gw| {
                        !gw.try_consume_relay_quota(&self.params.tunnel_id, payload_len)
                    }) {
                        self.inbound.remove(&conn_id);
                        self.metrics.close_connection(&conn_id);
                        tracing::debug!(
                            conn_id = %conn_id,
                            client_id = %self.params.client_id,
                            payload_len,
                            "relay TCP data dropped: relay quota exhausted"
                        );
                        return;
                    }
                    permit.send(payload);
                    if let Some(gw) = &gateway {
                        gw.commit_relay_usage(&self.params.tunnel_id, payload_len);
                    }
                    self.metrics.update_connection_bytes(
                        &conn_id,
                        &self.params.client_id,
                        0,
                        payload_len as i64,
                    );
                } else if let Some(gw) = self.gateway.upgrade() {
                    if !gw
                        .forward_client_relay(self, BinaryMessage::Data { conn_id, payload })
                        .await
                    {
                        self.stats
                            .msg_data_dropped_no_conn
                            .fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    self.stats
                        .msg_data_dropped_no_conn
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            BinaryMessage::UdpData { conn_id, payload } => {
                self.stats.msg_udp_data.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .bytes_udp_in
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                // Same Ref-across-await hazard as the Data arm above:
                // `limiter.acquire` awaits, so clone the Sender and release
                // the shard lock first.
                let tx = self.udp_inbound.get(&conn_id).map(|r| r.clone());
                if let Some(tx) = tx {
                    let payload_len = payload.len();
                    let gateway = self.gateway.upgrade();
                    self.limiter.acquire(payload_len).await;
                    if tx.is_closed() {
                        self.stats
                            .msg_udp_data_dropped_no_conn
                            .fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    if gateway.as_ref().is_some_and(|gw| {
                        !gw.try_consume_relay_quota(&self.params.tunnel_id, payload_len)
                    }) {
                        self.metrics.increment_udp_drops();
                        self.stats
                            .msg_udp_data_dropped_full
                            .fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    match tx.send_drop_oldest(payload) {
                        Ok(dropped) => {
                            if let Some(gw) = &gateway {
                                gw.commit_relay_usage(&self.params.tunnel_id, payload_len);
                            }
                            self.metrics.update_connection_bytes(
                                &conn_id,
                                &self.params.client_id,
                                0,
                                payload_len as i64,
                            );
                            if dropped {
                                self.metrics.increment_udp_drops();
                                self.stats
                                    .msg_udp_data_dropped_full
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => {
                            if let Some(gw) = gateway {
                                gw.refund_relay_quota(&self.params.tunnel_id, payload_len);
                            }
                            self.stats
                                .msg_udp_data_dropped_no_conn
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                } else if let Some(gw) = self.gateway.upgrade() {
                    if !gw
                        .forward_client_relay(self, BinaryMessage::UdpData { conn_id, payload })
                        .await
                    {
                        self.stats
                            .msg_udp_data_dropped_no_conn
                            .fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    self.stats
                        .msg_udp_data_dropped_no_conn
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            BinaryMessage::Close { conn_id } => {
                self.stats.msg_close.fetch_add(1, Ordering::Relaxed);
                self.inbound.remove(&conn_id);
                self.udp_inbound.remove(&conn_id);
                self.metrics.close_connection(&conn_id);
                if let Some(gw) = self.gateway.upgrade() {
                    let _ = gw
                        .forward_client_relay(self, BinaryMessage::Close { conn_id })
                        .await;
                }
            }
            BinaryMessage::Heartbeat {
                client_id,
                timestamp,
            } => {
                self.stats.msg_heartbeat.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    %client_id,
                    group_id = %self.params.group_id,
                    ts = timestamp,
                    "heartbeat received; replying with ack"
                );
                self.metrics
                    .update_client_heartbeat(&client_id, &self.params.group_id);
                let send_started = Instant::now();
                let send_result = self
                    .sender
                    .send(BinaryMessage::HeartbeatAck { timestamp })
                    .await;
                let send_elapsed = send_started.elapsed();
                match send_result {
                    Ok(()) if send_elapsed >= Duration::from_millis(500) => {
                        tracing::warn!(
                            %client_id,
                            group_id = %self.params.group_id,
                            ts = timestamp,
                            heartbeat_ack_send_elapsed_ms = send_elapsed.as_millis(),
                            "slow heartbeat ack send"
                        );
                    }
                    Ok(()) => {}
                    Err(err) => {
                        tracing::debug!(
                            %client_id,
                            group_id = %self.params.group_id,
                            ts = timestamp,
                            error = ?err,
                            "heartbeat ack send failed"
                        );
                    }
                }
            }
            BinaryMessage::RelayRouteBindAck { .. } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
            }
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id,
                peerlink_session_id,
                conn_id,
                route_abort,
                sealed,
            } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
                if let Some(gateway) = self.gateway.upgrade() {
                    let _ = gateway
                        .forward_encrypted_peer_control_v2(
                            self,
                            target_peer_id,
                            peerlink_session_id,
                            conn_id,
                            route_abort,
                            sealed,
                        )
                        .await;
                }
            }
            BinaryMessage::P2pAnnounce {
                client_id: _,
                group_id: _,
                locals,
                nat_hint,
                cert_fp,
            } => {
                // P2P task 2.3: refresh the peer registry with the public
                // address we observed (`params.peer_addr`, populated at
                // Auth — Task 2.2) plus client-reported locals / NAT hint /
                // cert fingerprint. The reply lets the client learn its
                // server-reflexive candidate and gateway clock for
                // synchronized hole-punch scheduling.
                //
                // Bind `client_id` and `group_id` to the authenticated
                // identity (`self.params.{client_id,group_id}`). Message-
                // supplied values are ignored so an authed client cannot
                // spoof another client's slot in the peer registry. Silent
                // rebind — no warn — so the spoof is not observable.
                //
                // Leaky-bucket at 1 announce/sec. Over-rate drops
                // silently (no Ack). Protects the registry from thrash
                // when an authed client floods Announces.
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
                if !self.p2p_signaling_limiter.try_announce() {
                    return;
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let ack = crate::p2p::handle_announce(
                    &self.peers,
                    self.params.peer_addr,
                    &self.params.tunnel_id,
                    &self.params.client_id,
                    locals,
                    nat_hint.as_u8(),
                    *cert_fp.as_bytes(),
                    now_ms,
                );
                let peer_ids = self
                    .gateway
                    .upgrade()
                    .map(|gw| gw.p2p_membership_peer_ids_in_tunnel(&self.params.tunnel_id, self))
                    .unwrap_or_default();
                tracing::info!(
                    client_id = %self.params.client_id,
                    p2p_hint_count = peer_ids.len(),
                    "p2p announce accepted"
                );
                for peer_client_id in peer_ids {
                    let _ = self
                        .sender
                        .send(BinaryMessage::P2pPeerHint { peer_client_id })
                        .await;
                }
                let _ = self
                    .sender
                    .send(BinaryMessage::P2pAnnounceAck {
                        public_ip: ack.public_ip,
                        // Port zero is the existing-wire sentinel that tells
                        // a V2 Client to derive a zero-candidate Relay-only
                        // PeerLink. A real observed UDP source port is never
                        // zero.
                        public_port: if p2p_direct_enabled {
                            ack.public_port
                        } else {
                            0
                        },
                        server_time_ms: ack.server_time_ms,
                    })
                    .await;
            }
            BinaryMessage::P2pOfferV2 {
                source_peer_id: _,
                target_peer_id,
                signed_offer,
            } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
                let source_identity = self.authenticated_peer_v2();
                if !self.p2p_signaling_limiter.try_offer() {
                    let _ = self
                        .sender
                        .send(BinaryMessage::Error(P2P_V2_PEER_OFFLINE.into()))
                        .await;
                    return;
                }
                let target = self
                    .gateway
                    .upgrade()
                    .and_then(|gateway| gateway.p2p_v2_target_in_tunnel(self, &target_peer_id));
                let Some(target) = target else {
                    let _ = self
                        .sender
                        .send(BinaryMessage::Error(P2P_V2_PEER_OFFLINE.into()))
                        .await;
                    return;
                };
                let target_identity = target.authenticated_peer_v2();
                let _ = target
                    .sender
                    .send(BinaryMessage::P2pOfferV2 {
                        source_peer_id: source_identity.peer_id.clone(),
                        target_peer_id: target_identity.peer_id.clone(),
                        signed_offer,
                    })
                    .await;
            }
            BinaryMessage::P2pAnswerV2 {
                source_peer_id: _,
                target_peer_id,
                signed_answer,
            } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
                let source_identity = self.authenticated_peer_v2();
                let target = self
                    .gateway
                    .upgrade()
                    .and_then(|gateway| gateway.p2p_v2_target_in_tunnel(self, &target_peer_id));
                let Some(target) = target else {
                    let _ = self
                        .sender
                        .send(BinaryMessage::Error(P2P_V2_PEER_OFFLINE.into()))
                        .await;
                    return;
                };
                let target_identity = target.authenticated_peer_v2();
                let _ = target
                    .sender
                    .send(BinaryMessage::P2pAnswerV2 {
                        source_peer_id: source_identity.peer_id.clone(),
                        target_peer_id: target_identity.peer_id.clone(),
                        signed_answer,
                    })
                    .await;
            }
            BinaryMessage::P2pOffer { .. }
            | BinaryMessage::P2pAnswer { .. }
            | BinaryMessage::P2pTeardown { .. }
            | BinaryMessage::P2pSessionReady { .. }
            | BinaryMessage::P2pPeerHint { .. } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
            }
            BinaryMessage::RelayRouteBind {
                conn_id,
                peer_client_id,
            } => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
                let Some(gw) = self.gateway.upgrade() else {
                    if self.sender.capabilities().route_bind_control_v1 {
                        let _ = self
                            .sender
                            .send(BinaryMessage::RelayRouteBindAck {
                                conn_id,
                                success: false,
                                error: "gateway unavailable".into(),
                            })
                            .await;
                    }
                    return;
                };
                match gw.bind_client_relay_route(self, conn_id.clone(), &peer_client_id) {
                    Ok(()) => {
                        tracing::debug!(
                            %conn_id,
                            peer_client_id = %peer_client_id,
                            "bound relay route for P2P fallback"
                        );
                        if self.sender.capabilities().route_bind_control_v1 {
                            let _ = self
                                .sender
                                .send(BinaryMessage::RelayRouteBindAck {
                                    conn_id,
                                    success: true,
                                    error: String::new(),
                                })
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            %conn_id,
                            peer_client_id = %peer_client_id,
                            error = %e,
                            "failed to bind relay route"
                        );
                        if self.sender.capabilities().route_bind_control_v1 {
                            let _ = self
                                .sender
                                .send(BinaryMessage::RelayRouteBindAck {
                                    conn_id,
                                    success: false,
                                    error: "relay route bind failed".into(),
                                })
                                .await;
                        }
                    }
                }
            }
            _ => {
                self.stats.msg_other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    async fn handle_tcp_flow_stream(self: Arc<Self>, mut incoming: TcpFlowIncoming) {
        let conn_id = incoming.preface.conn_id.clone();
        let address = incoming.preface.address.clone();
        if incoming.preface.network != "tcp" {
            let _ = incoming
                .stream
                .send_connect_response(false, "unsupported tcp flow network".into())
                .await;
            return;
        }
        let Some(gw) = self.gateway.upgrade() else {
            let _ = incoming
                .stream
                .send_connect_response(false, "gateway shutting down".into())
                .await;
            return;
        };
        gw.relay_client_tcp_flow(&self, incoming).await;
        tracing::debug!(%conn_id, %address, "tcp flow stream handler exited");
    }

    pub(crate) async fn open_framed_with_conn_id(
        self: &Arc<Self>,
        conn_id: String,
        network: &str,
        address: &str,
    ) -> anyhow::Result<TunneledConn> {
        let (done_tx, done_rx) = oneshot::channel();
        self.pending.insert(conn_id.clone(), done_tx);
        let address_present = !address.is_empty();
        let address_count = usize::from(address_present);

        tracing::debug!(
            %conn_id,
            client_id = %self.params.client_id,
            group_id = %self.params.group_id,
            network,
            %address,
            "sending framed Connect to exact Peer attachment"
        );

        let connect_msg = BinaryMessage::Connect {
            conn_id: conn_id.clone(),
            network: network.into(),
            address: address.into(),
        };
        // Bound the outbound enqueue: if the tunnel's outbound stream mpsc
        // is saturated (QUIC writer stalled by a degraded network path)
        // we must NOT park every proxy handler indefinitely waiting for a
        // slot. Fail fast so the caller returns REP_GENERAL_FAIL / 502 and
        // the accepted TCP socket is released, keeping proxy accept loops
        // drainable. 3 s is generous — a healthy tunnel flushes 2048 slots
        // in milliseconds.
        match timeout(Duration::from_secs(3), self.sender.send(connect_msg)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                self.pending.remove(&conn_id);
                tracing::warn!(
                    %conn_id,
                    client_id = %self.params.client_id,
                    reason = "client_session_closed",
                    failure_class = "session",
                    address_present,
                    address_count,
                    "exact Peer attachment already closed"
                );
                anyhow::bail!("client session closed");
            }
            Err(_) => {
                self.pending.remove(&conn_id);
                self.metrics.increment_errors(Some(&self.params.client_id));
                tracing::warn!(
                    %conn_id,
                    client_id = %self.params.client_id,
                    reason = "outbound_queue_saturated",
                    failure_class = "backpressure",
                    address_present,
                    address_count,
                    "exact Peer outbound queue saturated (>3s)"
                );
                anyhow::bail!("tunnel outbound queue saturated");
            }
        }

        match timeout(Duration::from_secs(15), done_rx).await {
            Ok(Ok(Ok(()))) => {
                tracing::debug!(
                    %conn_id,
                    client_id = %self.params.client_id,
                    %address,
                    "exact Peer framed Connect acknowledged"
                );
                let (rx_tx, rx_rx) = mpsc::channel::<Bytes>(64);
                self.inbound.insert(conn_id.clone(), rx_tx);
                self.metrics
                    .create_connection(&conn_id, &self.params.client_id, address);
                let quota = self
                    .gateway
                    .upgrade()
                    .and_then(|gw| gw.relay_quota_limiter(&self.params.tunnel_id));
                Ok(TunneledConn::new(
                    conn_id,
                    rx_rx,
                    self.sender.clone(),
                    self.inbound.clone(),
                    self.limiter.clone(),
                    quota,
                    self.metrics.clone(),
                    self.params.client_id.clone(),
                ))
            }
            Ok(Ok(Err(reason))) => {
                tracing::debug!(
                    %conn_id,
                    client_id = %self.params.client_id,
                    %address,
                    %reason,
                    "exact Peer refused framed Connect"
                );
                self.inbound.remove(&conn_id);
                self.metrics.increment_errors(Some(&self.params.client_id));
                anyhow::bail!("remote refused connect")
            }
            Ok(Err(_)) | Err(_) => {
                tracing::warn!(
                    %conn_id,
                    client_id = %self.params.client_id,
                    reason = "connect_ack_timeout_or_cancelled",
                    failure_class = "ack_wait",
                    address_present,
                    address_count,
                    timeout_secs = 15,
                    "exact Peer framed Connect acknowledgement timed out"
                );
                self.pending.remove(&conn_id);
                self.inbound.remove(&conn_id);
                self.metrics.increment_errors(Some(&self.params.client_id));
                anyhow::bail!("connect ack timeout or cancelled")
            }
        }
    }
}

#[cfg(test)]
mod p2p_signaling_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::sync::mpsc;
    use tp_core::protocol::{unpack, BinaryMessage, PackedMessage, TransportCapabilities};

    fn gateway() -> Arc<crate::Gateway> {
        crate::Gateway::new(Default::default(), None)
    }

    fn v2_conn(
        gateway: &Arc<crate::Gateway>,
        tunnel_id: &str,
        peer_id: &str,
        replica_id: &str,
    ) -> (Arc<ClientConn>, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel(16);
        let (_in_tx, in_rx) = mpsc::channel(1);
        let peer_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345);
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer_addr,
            Arc::new(|| {}),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        let conn = ClientConn::new(
            AuthParams {
                tunnel_id: tunnel_id.into(),
                capabilities: TransportCapabilities {
                    peer_mesh_v2: true,
                    ..Default::default()
                },
                client_id: replica_id.into(),
                group_id: String::new(),
                username: String::new(),
                password: String::new(),
                group_password: String::new(),
                role: tp_core::config::ClientRoleConfig::Client,
                peer_addr,
            },
            session,
            Arc::new(BandwidthLimiter::new(0)),
            MetricsManager::new(),
            gateway.peers.clone(),
            Arc::downgrade(gateway),
            AuthenticatedPeerV2 {
                tunnel_id: tunnel_id.into(),
                peer_id: peer_id.into(),
                replica_id: replica_id.into(),
                overlay_ip: Ipv4Addr::new(198, 18, 0, 9),
            },
        );
        gateway
            .peers
            .bind_v2_identity(tunnel_id, peer_id, replica_id);
        gateway.register(tunnel_id, conn.clone());
        (conn, out_rx)
    }

    async fn drain(rx: &mut mpsc::Receiver<PackedMessage>) -> Vec<BinaryMessage> {
        tokio::task::yield_now().await;
        let mut messages = Vec::new();
        while let Ok(packed) = rx.try_recv() {
            messages.push(unpack(&packed.to_bytes()).expect("valid frame"));
        }
        messages
    }

    #[tokio::test]
    async fn v2_offer_stamps_authenticated_source_and_exact_forwards_opaque_body() {
        let gateway = gateway();
        let (source, mut source_rx) = v2_conn(&gateway, "tun-v2", "peer-a", "tun-v2-Aaaaaaaa-0");
        let (_target, mut target_rx) = v2_conn(&gateway, "tun-v2", "peer-b", "tun-v2-Bbbbbbbb-0");
        let opaque = Bytes::from_static(b"opaque signed offer\0\xff");

        source
            .handle_inbound(BinaryMessage::P2pOfferV2 {
                source_peer_id: "spoofed".into(),
                target_peer_id: "peer-b".into(),
                signed_offer: opaque.clone(),
            })
            .await;

        assert!(drain(&mut source_rx).await.is_empty());
        assert!(matches!(
            drain(&mut target_rx).await.as_slice(),
            [BinaryMessage::P2pOfferV2 { source_peer_id, target_peer_id, signed_offer }]
                if source_peer_id == "peer-a" && target_peer_id == "peer-b" && signed_offer == &opaque
        ));
    }

    #[tokio::test]
    async fn v2_answer_routes_by_stable_peer_without_legacy_session_state() {
        let gateway = gateway();
        let (_source, mut source_rx) = v2_conn(&gateway, "tun-v2", "peer-a", "tun-v2-Aaaaaaaa-0");
        let (target, mut target_rx) = v2_conn(&gateway, "tun-v2", "peer-b", "tun-v2-Bbbbbbbb-0");
        let opaque = Bytes::from_static(&[9, 8, 7, 0, 6]);

        target
            .handle_inbound(BinaryMessage::P2pAnswerV2 {
                source_peer_id: "ignored".into(),
                target_peer_id: "peer-a".into(),
                signed_answer: opaque.clone(),
            })
            .await;

        assert!(drain(&mut target_rx).await.is_empty());
        assert!(matches!(
            drain(&mut source_rx).await.as_slice(),
            [BinaryMessage::P2pAnswerV2 { source_peer_id, target_peer_id, signed_answer }]
                if source_peer_id == "peer-b" && target_peer_id == "peer-a" && signed_answer == &opaque
        ));
    }

    #[tokio::test]
    async fn v1_offer_is_not_routed() {
        let gateway = gateway();
        let (source, mut source_rx) = v2_conn(&gateway, "tun-v2", "peer-a", "tun-v2-Aaaaaaaa-0");
        let (_target, mut target_rx) = v2_conn(&gateway, "tun-v2", "peer-b", "tun-v2-Bbbbbbbb-0");

        source
            .handle_inbound(BinaryMessage::P2pOffer {
                session_id: tp_core::p2p_types::SessionId::from_bytes([1; 16]),
                src_client_id: source.params.client_id.clone(),
                dst_client_id: "tun-v2-Bbbbbbbb-0".into(),
                candidates: vec![],
                src_cert_fp: tp_core::p2p_types::CertFingerprint::zero(),
                role: tp_core::p2p_types::P2pRole::Initiator,
            })
            .await;

        assert!(drain(&mut source_rx).await.is_empty());
        assert!(drain(&mut target_rx).await.is_empty());
    }
}
