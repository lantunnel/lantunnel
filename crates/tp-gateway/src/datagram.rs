//! `TunneledDatagram` — a datagram-oriented handle for UDP tunneling.
//!
//! SOCKS5 UDP ASSOCIATE and TUIC Packet use this to send/receive UDP datagrams
//! through a client's tunneled link. Each target-bound association gets its own
//! `conn_id` so the client side can demultiplex back to the right `UdpSocket`.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use tp_core::bandwidth::BandwidthLimiter;
use tp_core::protocol::BinaryMessage;
use tp_metrics::MetricsManager;
use tp_transport::{DropOldestReceiver, DropOldestSender, SessionSender, TrySendKind};

use crate::RelayQuotaLimiter;

fn refund_quota(quota: &Option<Arc<RelayQuotaLimiter>>, payload_len: usize) {
    if let Some(quota) = quota {
        quota.refund(payload_len);
    }
}

fn try_send_udp_after_bandwidth(
    tx: &SessionSender,
    conn_id: &str,
    quota: &Option<Arc<RelayQuotaLimiter>>,
    metrics: &Arc<MetricsManager>,
    payload: Bytes,
) -> std::result::Result<(), TrySendKind> {
    let payload_len = payload.len();
    if quota
        .as_ref()
        .is_some_and(|quota| !quota.try_consume(payload_len))
    {
        metrics.increment_udp_drops();
        return Err(TrySendKind::Full);
    }
    match tx.try_send(BinaryMessage::UdpData {
        conn_id: conn_id.to_string(),
        payload,
    }) {
        Ok(()) => {
            if let Some(quota) = quota {
                quota.commit_usage(payload_len);
            }
            Ok(())
        }
        Err(TrySendKind::Full) => {
            refund_quota(quota, payload_len);
            metrics.increment_udp_drops();
            Err(TrySendKind::Full)
        }
        Err(TrySendKind::TooLarge(len)) => {
            refund_quota(quota, payload_len);
            Err(TrySendKind::TooLarge(len))
        }
        Err(TrySendKind::DatagramUnavailable) => {
            refund_quota(quota, payload_len);
            Err(TrySendKind::DatagramUnavailable)
        }
        Err(TrySendKind::Closed) => {
            refund_quota(quota, payload_len);
            Err(TrySendKind::Closed)
        }
    }
}

fn udp_try_send_terminal_error(kind: TrySendKind) -> anyhow::Error {
    match kind {
        TrySendKind::Full => anyhow::anyhow!("udp tunnel send queue full"),
        TrySendKind::TooLarge(len) => anyhow::anyhow!("udp tunnel frame too large: {len}"),
        TrySendKind::DatagramUnavailable => anyhow::anyhow!("udp datagram transport unavailable"),
        TrySendKind::Closed => anyhow::anyhow!("tunnel closed"),
    }
}

pub struct TunneledDatagram {
    conn_id: String,
    tx: SessionSender,
    rx: DropOldestReceiver<Bytes>,
    inbound_map: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    /// Per-group bandwidth budget shared across every DatagramSender /
    /// DatagramReceiver produced from this association. Enforced on both
    /// send halves (async `send` blocks, non-blocking `try_send`
    /// drops on over-budget) so the symmetric bandwidth cap covers UDP
    /// as well as TCP. Inbound (client → gateway) is still charged by
    /// `ClientConn::handle_inbound`, so this half only needs to police
    /// the gateway-originated direction.
    limiter: Arc<BandwidthLimiter>,
    quota: Option<Arc<RelayQuotaLimiter>>,
    /// Metrics handle so Drop (and the split-produced DatagramReceiver's
    /// Drop) can release the per-conn `MetricsManager.connections` slot.
    /// Without this, every UDP association leaks a metrics entry on
    /// teardown and `Global.active_connections` climbs without bound.
    metrics: Arc<MetricsManager>,
    closed: bool,
}

impl TunneledDatagram {
    #[cfg(test)]
    pub(crate) fn new(
        conn_id: String,
        rx: DropOldestReceiver<Bytes>,
        tx: SessionSender,
        inbound_map: Arc<DashMap<String, DropOldestSender<Bytes>>>,
        limiter: Arc<BandwidthLimiter>,
        quota: Option<Arc<RelayQuotaLimiter>>,
        metrics: Arc<MetricsManager>,
    ) -> Self {
        Self {
            conn_id,
            tx,
            rx,
            inbound_map,
            limiter,
            quota,
            metrics,
            closed: false,
        }
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    /// Emit one UDP datagram via this target-bound tunnel. Blocks on the
    /// per-group BandwidthLimiter, then charges relay quota only when the
    /// transport accepts the datagram for relay. Queue-full drops are normal
    /// UDP loss and do not close the association.
    pub async fn send(&self, payload: Bytes) -> anyhow::Result<()> {
        let payload_len = payload.len();
        self.limiter.acquire(payload_len).await;
        match try_send_udp_after_bandwidth(
            &self.tx,
            &self.conn_id,
            &self.quota,
            &self.metrics,
            payload,
        ) {
            Ok(()) | Err(TrySendKind::Full) => Ok(()),
            Err(kind) => Err(udp_try_send_terminal_error(kind)),
        }
    }

    /// Receive one incoming datagram from this target-bound tunnel.
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }

    pub async fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self
                .tx
                .send(BinaryMessage::Close {
                    conn_id: self.conn_id.clone(),
                })
                .await;
            self.inbound_map.remove(&self.conn_id);
        }
    }

    /// Split into (sender, receiver) so upstream and downstream relay pumps
    /// can run on independent tasks. The sender is cheaply cloneable. When
    /// the receiver is dropped a `Close` is emitted and the conn_id is
    /// removed from the shared inbound map (same semantics as dropping a
    /// whole `TunneledDatagram`).
    pub fn split(mut self) -> (DatagramSender, DatagramReceiver) {
        self.closed = true;
        let sender = DatagramSender {
            conn_id: self.conn_id.clone(),
            tx: self.tx.clone(),
            limiter: self.limiter.clone(),
            quota: self.quota.clone(),
            metrics: self.metrics.clone(),
        };
        let (_, sentinel_rx) = tp_transport::drop_oldest_channel::<Bytes>(1);
        let rx = std::mem::replace(&mut self.rx, sentinel_rx);
        let receiver = DatagramReceiver {
            conn_id: self.conn_id.clone(),
            rx,
            tx: self.tx.clone(),
            inbound_map: self.inbound_map.clone(),
            metrics: self.metrics.clone(),
            closed: false,
        };
        (sender, receiver)
    }
}

impl Drop for TunneledDatagram {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self
                .tx
                .try_send(BinaryMessage::Close {
                    conn_id: self.conn_id.clone(),
                })
                .map_err(|_| ());
            self.inbound_map.remove(&self.conn_id);
            // Release the metrics slot so Global.active_connections doesn't
            // stay permanently inflated for this dropped UDP association.
            self.metrics.close_connection(&self.conn_id);
        }
    }
}

/// Cheaply-cloneable sender half produced by [`TunneledDatagram::split`].
#[derive(Clone)]
pub struct DatagramSender {
    conn_id: String,
    tx: SessionSender,
    limiter: Arc<BandwidthLimiter>,
    quota: Option<Arc<RelayQuotaLimiter>>,
    metrics: Arc<MetricsManager>,
}

impl DatagramSender {
    /// Awaiting variant — backpressures if the underlying transport's
    /// outbound datagram queue is full, and blocks on the per-group
    /// BandwidthLimiter when the send would push the bucket over quota.
    /// Appropriate for reliable TCP paths. **For game-streaming UDP, prefer
    /// [`try_send`] which drops instead of queueing.**
    ///
    /// [`try_send`]: Self::try_send
    pub async fn send(&self, payload: Bytes) -> anyhow::Result<()> {
        let payload_len = payload.len();
        self.limiter.acquire(payload_len).await;
        match try_send_udp_after_bandwidth(
            &self.tx,
            &self.conn_id,
            &self.quota,
            &self.metrics,
            payload,
        ) {
            Ok(()) | Err(TrySendKind::Full) => Ok(()),
            Err(kind) => Err(udp_try_send_terminal_error(kind)),
        }
    }

    /// Non-blocking send. Returns `TrySendKind::Full` when the transport's
    /// outbound datagram queue has no capacity OR the per-group
    /// BandwidthLimiter would exceed its quota — callers on the game-stream
    /// hot path should drop + count rather than await, because a late UDP
    /// packet is a dead packet (each frame in a 4K240Hz stream has a <4ms
    /// deadline, so buffering past that is pure latency with no benefit).
    /// Conflating both over-budget conditions under `Full` is intentional:
    /// the `udp_dropped_full` counter already tracks these drops and the
    /// caller's reaction (drop-and-continue) is identical. Oversized frames
    /// are surfaced separately and are not queued.
    pub fn try_send(&self, payload: Bytes) -> std::result::Result<(), TrySendKind> {
        let payload_len = payload.len();
        if self.limiter.try_acquire(payload_len).is_err() {
            self.metrics.increment_udp_drops();
            return Err(TrySendKind::Full);
        }
        try_send_udp_after_bandwidth(&self.tx, &self.conn_id, &self.quota, &self.metrics, payload)
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub fn metrics(&self) -> &Arc<MetricsManager> {
        &self.metrics
    }
}

/// Receiver half produced by [`TunneledDatagram::split`]. Owns the lifecycle
/// of the conn_id — dropping it releases the inbound-map slot and emits a
/// `Close` to the remote side.
pub struct DatagramReceiver {
    conn_id: String,
    rx: DropOldestReceiver<Bytes>,
    tx: SessionSender,
    inbound_map: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    metrics: Arc<MetricsManager>,
    closed: bool,
}

impl DatagramReceiver {
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }

    /// Non-blocking drain variant used by batching reply pumps (e.g.
    /// `tp_proxy_socks5::target_reply_pump`). After `recv().await`
    /// wakes us with one packet, we call this in a loop to slurp all
    /// currently-queued packets and process them before the next yield.
    /// This cuts the number of scheduler round-trips from "one per
    /// packet" to "one per burst" — important for moonlight/sunshine
    /// video at ~2500 pps where each Rust async wakeup adds tens of µs
    /// of extra latency vs. the Go implementation.
    pub fn try_recv(
        &mut self,
    ) -> std::result::Result<Bytes, tokio::sync::mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub async fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self
                .tx
                .send(BinaryMessage::Close {
                    conn_id: self.conn_id.clone(),
                })
                .await;
            self.inbound_map.remove(&self.conn_id);
        }
    }
}

impl Drop for DatagramReceiver {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self
                .tx
                .try_send(BinaryMessage::Close {
                    conn_id: self.conn_id.clone(),
                })
                .map_err(|_| ());
            self.inbound_map.remove(&self.conn_id);
            // Release the metrics slot (DatagramReceiver owns the conn_id
            // lifecycle after `split()`, mirroring TunneledDatagram::drop).
            self.metrics.close_connection(&self.conn_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::sync::mpsc;
    use tp_core::protocol::PackedMessage;
    use tp_transport::session::Session;

    fn channel_sender_with_capacity(
        capacity: usize,
    ) -> (SessionSender, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(capacity);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        let (sender, _receiver, _datagram) = session.split();
        (sender, out_rx)
    }

    #[tokio::test]
    async fn async_datagram_send_does_not_charge_quota_when_transport_queue_is_full() {
        let (tx, _outbound_rx) = channel_sender_with_capacity(1);
        tx.try_send(BinaryMessage::Data {
            conn_id: "filled".into(),
            payload: Bytes::from_static(b"x"),
        })
        .expect("fill stream channel");
        let quota = Arc::new(RelayQuotaLimiter::default());
        quota.update("tun-datagram", "202605", 10, 10);
        let (rx_tx, rx_rx) = tp_transport::drop_oldest_channel::<Bytes>(8);
        let inbound = Arc::new(DashMap::new());
        let datagram = TunneledDatagram::new(
            "udpquota1".into(),
            rx_rx,
            tx,
            inbound,
            Arc::new(BandwidthLimiter::new(0)),
            Some(quota.clone()),
            MetricsManager::new(),
        );

        datagram
            .send(Bytes::from_static(b"hello"))
            .await
            .expect("full transport queue is lossy, not terminal");
        assert_eq!(quota.remaining_bytes(), Some(10));
        drop(rx_tx);
    }

    #[tokio::test]
    async fn async_datagram_sender_does_not_charge_quota_when_transport_queue_is_full() {
        let (tx, _outbound_rx) = channel_sender_with_capacity(1);
        tx.try_send(BinaryMessage::Data {
            conn_id: "filled".into(),
            payload: Bytes::from_static(b"x"),
        })
        .expect("fill stream channel");
        let quota = Arc::new(RelayQuotaLimiter::default());
        quota.update("tun-datagram", "202605", 10, 10);
        let sender = DatagramSender {
            conn_id: "udpquota2".into(),
            tx,
            limiter: Arc::new(BandwidthLimiter::new(0)),
            quota: Some(quota.clone()),
            metrics: MetricsManager::new(),
        };

        sender
            .send(Bytes::from_static(b"hello"))
            .await
            .expect("full transport queue is lossy, not terminal");
        assert_eq!(quota.remaining_bytes(), Some(10));
    }
}
