//! `TunneledConn` — an `AsyncRead + AsyncWrite` handle over the tunneled link.
//!
//! The Exact-Peer relay path uses this handle to pipe bytes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::time::Sleep;
use tokio_util::sync::PollSender;
use tp_core::bandwidth::BandwidthLimiter;
use tp_core::protocol::{pack, BinaryMessage, PackedMessage};
use tp_metrics::MetricsManager;
use tp_transport::{SessionSender, TcpFlowStream};

use crate::RelayQuotaLimiter;

/// Matches `BandwidthLimiter::try_acquire`'s internal MAX_CHUNK. Keeping this
/// local constant lets `poll_write` cap the per-call charge before consulting
/// the limiter, so `try_acquire` never returns `Err` via the "n > burst"
/// branch and the rate-limit budget is spent linearly with bytes written.
const BANDWIDTH_CHUNK: usize = 64 * 1024;
const TCP_FLOW_READ_CHUNK: usize = 16 * 1024;

pub struct TunneledConn {
    conn_id: String,
    rx: mpsc::Receiver<Bytes>,
    /// Reliable-stream writer wrapped for `AsyncWrite::poll_write`. `PollSender`
    /// reserves a permit via the underlying mpsc's async capacity-wait, which
    /// registers the current task's waker. This is the critical fix vs. a
    /// naive `SessionSender::try_send` + `Poll::Pending` (which never wakes
    /// the `tokio::io::copy_bidirectional` future once stream_tx saturates —
    /// the "SOCKS5 stream dies a few seconds into moonlight" symptom).
    poll_sender: PollSender<PackedMessage>,
    /// Retained SessionSender clone — used ONLY for the best-effort `Close`
    /// emit on Drop/poll_shutdown. Any backpressured Data writes must go
    /// through `poll_sender` above so a full queue parks the future instead
    /// of silently deadlocking.
    tx: SessionSender,
    inbound_map: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
    limiter: Arc<BandwidthLimiter>,
    quota: Option<Arc<RelayQuotaLimiter>>,
    metrics: Arc<MetricsManager>,
    client_id: String,
    tcp_flow_stream: Option<TcpFlowStream>,
    leftover: Option<Bytes>,
    closed: bool,
    read_closed: bool,
    write_closed: bool,
    /// Bandwidth-limit deferral. When `poll_write` finds the token bucket
    /// empty, it parks on `tokio::time::sleep(wait)` to wake the task
    /// exactly when budget is available again, and stashes the Sleep here
    /// so subsequent polls observe completion instead of creating a new
    /// timer on every call. Pattern mirrors the async bandwidth integration
    /// in `ClientConn::handle_inbound` (which can just `.await` its limiter)
    /// into the Poll world required by `AsyncWrite`.
    pending_sleep: Option<Pin<Box<Sleep>>>,
    reserved_write_quota: usize,
}

impl TunneledConn {
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor preserves explicit ownership of existing relay dependencies"
    )]
    pub(crate) fn new(
        conn_id: String,
        rx: mpsc::Receiver<Bytes>,
        tx: SessionSender,
        inbound_map: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        limiter: Arc<BandwidthLimiter>,
        quota: Option<Arc<RelayQuotaLimiter>>,
        metrics: Arc<MetricsManager>,
        client_id: String,
    ) -> Self {
        let poll_sender = PollSender::new(tx.stream_mpsc());
        Self {
            conn_id,
            rx,
            poll_sender,
            tx,
            inbound_map,
            limiter,
            quota,
            metrics,
            client_id,
            tcp_flow_stream: None,
            leftover: None,
            closed: false,
            read_closed: false,
            write_closed: false,
            pending_sleep: None,
            reserved_write_quota: 0,
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "constructor preserves explicit ownership of existing relay dependencies"
    )]
    #[cfg(test)]
    pub(crate) fn new_with_tcp_flow_stream(
        conn_id: String,
        stream: TcpFlowStream,
        tx: SessionSender,
        inbound_map: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
        limiter: Arc<BandwidthLimiter>,
        quota: Option<Arc<RelayQuotaLimiter>>,
        metrics: Arc<MetricsManager>,
        client_id: String,
    ) -> Self {
        let (_rx_tx, rx) = mpsc::channel::<Bytes>(1);
        let poll_sender = PollSender::new(tx.stream_mpsc());
        Self {
            conn_id,
            rx,
            poll_sender,
            tx,
            inbound_map,
            limiter,
            quota,
            metrics,
            client_id,
            tcp_flow_stream: Some(stream),
            leftover: None,
            closed: false,
            read_closed: false,
            write_closed: false,
            pending_sleep: None,
            reserved_write_quota: 0,
        }
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    fn poll_bandwidth_permit(
        &mut self,
        cx: &mut Context<'_>,
        want: usize,
    ) -> Poll<std::io::Result<()>> {
        if want == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if let Some(mut sleep) = self.pending_sleep.take() {
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => {}
                    Poll::Pending => {
                        self.pending_sleep = Some(sleep);
                        return Poll::Pending;
                    }
                }
            }
            if let Err(wait) = self.limiter.try_acquire(want) {
                let mut sleep: Pin<Box<Sleep>> = Box::pin(tokio::time::sleep(wait));
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => continue,
                    Poll::Pending => {
                        self.pending_sleep = Some(sleep);
                        return Poll::Pending;
                    }
                }
            }
            return Poll::Ready(Ok(()));
        }
    }

    fn relay_quota_exhausted() -> std::io::Error {
        std::io::Error::other("relay quota exhausted")
    }

    fn consume_read_quota(&self, bytes: usize) -> std::io::Result<()> {
        if let Some(quota) = &self.quota {
            if !quota.try_consume(bytes) {
                return Err(Self::relay_quota_exhausted());
            }
            quota.commit_usage(bytes);
        }
        Ok(())
    }

    fn reserve_write_quota(&mut self, want: usize) -> std::io::Result<()> {
        if self.reserved_write_quota > want {
            self.refund_quota(self.reserved_write_quota - want);
            self.reserved_write_quota = want;
        }
        if self.reserved_write_quota >= want {
            return Ok(());
        }
        let needed = want - self.reserved_write_quota;
        if self
            .quota
            .as_ref()
            .is_some_and(|quota| !quota.try_consume(needed))
        {
            return Err(Self::relay_quota_exhausted());
        }
        self.reserved_write_quota += needed;
        Ok(())
    }

    fn settle_write_quota(&mut self, written: usize) {
        let reserved = std::mem::take(&mut self.reserved_write_quota);
        if written > 0 {
            self.commit_quota(written);
        }
        if reserved > written {
            self.refund_quota(reserved - written);
        }
    }

    fn refund_pending_write_quota(&mut self) {
        let reserved = std::mem::take(&mut self.reserved_write_quota);
        self.refund_quota(reserved);
    }

    fn refund_quota(&self, bytes: usize) {
        if let Some(quota) = &self.quota {
            quota.refund(bytes);
        }
    }

    fn commit_quota(&self, bytes: usize) {
        if let Some(quota) = &self.quota {
            quota.commit_usage(bytes);
        }
    }
}

impl AsyncRead for TunneledConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.tcp_flow_stream.is_some() {
            loop {
                if let Some(mut chunk) = self.leftover.take() {
                    let n = chunk.len().min(buf.remaining());
                    match self.poll_bandwidth_permit(cx, n) {
                        Poll::Pending => {
                            self.leftover = Some(chunk);
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => {}
                    }
                    if let Err(e) = self.consume_read_quota(n) {
                        return Poll::Ready(Err(e));
                    }
                    let front = chunk.split_to(n);
                    buf.put_slice(&front);
                    if !chunk.is_empty() {
                        self.leftover = Some(chunk);
                    }
                    self.metrics.update_connection_bytes(
                        &self.conn_id,
                        &self.client_id,
                        0,
                        n as i64,
                    );
                    return Poll::Ready(Ok(()));
                }
                if self.closed || self.read_closed {
                    return Poll::Ready(Ok(()));
                }
                if buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
                let mut tmp = [0u8; TCP_FLOW_READ_CHUNK];
                let want = tmp.len().min(buf.remaining()).min(BANDWIDTH_CHUNK);
                let mut read_buf = ReadBuf::new(&mut tmp[..want]);
                let Some(stream) = self.tcp_flow_stream.as_mut() else {
                    unreachable!();
                };
                match Pin::new(stream).poll_read(cx, &mut read_buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {}
                }
                let n = read_buf.filled().len();
                if n > 0 {
                    self.leftover = Some(Bytes::copy_from_slice(read_buf.filled()));
                    continue;
                }
                self.read_closed = true;
                return Poll::Ready(Ok(()));
            }
        }

        loop {
            if let Some(mut chunk) = self.leftover.take() {
                let n = chunk.len().min(buf.remaining());
                let front = chunk.split_to(n);
                buf.put_slice(&front);
                if !chunk.is_empty() {
                    self.leftover = Some(chunk);
                }
                return Poll::Ready(Ok(()));
            }
            if self.closed || self.read_closed {
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(data)) => {
                    self.leftover = Some(data);
                    continue;
                }
                Poll::Ready(None) => {
                    self.read_closed = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for TunneledConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed || self.write_closed {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Bandwidth gate — the gateway's outbound direction (gateway → client
        // → internet) used to bypass the per-group BandwidthLimiter entirely,
        // leaving only the download leg enforced. Now we charge the same
        // token bucket on both halves so per-group Mbps caps are actually
        // symmetric. Cap the per-call charge to BANDWIDTH_CHUNK so
        // `try_acquire` stays on its fast "<= burst" branch.
        let want = buf.len().min(BANDWIDTH_CHUNK);
        // First, resolve any pending sleep left over from a previous
        // over-budget call. If it's still pending, park again — the
        // runtime re-polls us once the timer fires.
        match self.poll_bandwidth_permit(cx, want) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }

        if self.tcp_flow_stream.is_some() {
            if let Err(e) = self.reserve_write_quota(want) {
                return Poll::Ready(Err(e));
            }
            let stream = self.tcp_flow_stream.as_mut().expect("checked above");
            return match Pin::new(stream).poll_write(cx, &buf[..want]) {
                Poll::Ready(Ok(n)) => {
                    self.settle_write_quota(n);
                    if n > 0 {
                        self.metrics.update_connection_bytes(
                            &self.conn_id,
                            &self.client_id,
                            n as i64,
                            0,
                        );
                    }
                    Poll::Ready(Ok(n))
                }
                Poll::Ready(Err(e)) => {
                    self.refund_pending_write_quota();
                    Poll::Ready(Err(e))
                }
                Poll::Pending => Poll::Pending,
            };
        }

        // `poll_reserve` internally calls `Sender::reserve` which registers
        // the current task's waker with the mpsc's semaphore; when the
        // writer task drains a permit, our waker fires and we get re-polled.
        // This is the critical difference vs. the previous `try_send` + bare
        // `Poll::Pending` path that never woke — once the reliable-stream
        // queue saturated (e.g. during moonlight's pre-PMTUD burst when
        // large UdpData frames spill onto the stream), the `copy_bidirectional`
        // future parked forever and moonlight's control TCP channel died
        // after a few seconds.
        match self.poll_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session closed",
            ))),
            Poll::Ready(Ok(())) => {
                if let Err(e) = self.reserve_write_quota(want) {
                    return Poll::Ready(Err(e));
                }
                let msg = BinaryMessage::Data {
                    conn_id: self.conn_id.clone(),
                    payload: Bytes::copy_from_slice(&buf[..want]),
                };
                let packed = pack(&msg);
                match self.poll_sender.send_item(packed) {
                    Ok(()) => {
                        self.metrics.update_connection_bytes(
                            &self.conn_id,
                            &self.client_id,
                            want as i64,
                            0,
                        );
                        self.settle_write_quota(want);
                        Poll::Ready(Ok(want))
                    }
                    Err(_) => {
                        self.refund_pending_write_quota();
                        Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "session closed",
                        )))
                    }
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if let Some(stream) = self.tcp_flow_stream.as_mut() {
            return Pin::new(stream).poll_flush(cx);
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.closed || self.write_closed {
            return Poll::Ready(Ok(()));
        }
        if let Some(stream) = self.tcp_flow_stream.as_mut() {
            let poll = Pin::new(stream).poll_shutdown(cx);
            if poll.is_ready() {
                self.write_closed = true;
            }
            return poll;
        }
        match self.poll_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => {
                self.write_closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(())) => {
                let msg = BinaryMessage::Close {
                    conn_id: self.conn_id.clone(),
                };
                match self.poll_sender.send_item(pack(&msg)) {
                    Ok(()) => {
                        self.write_closed = true;
                        self.poll_sender.close();
                        Poll::Ready(Ok(()))
                    }
                    Err(_) => {
                        self.write_closed = true;
                        Poll::Ready(Ok(()))
                    }
                }
            }
        }
    }
}

impl Drop for TunneledConn {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            if self.tcp_flow_stream.is_none() && !self.write_closed {
                let _ = self.tx.try_send(BinaryMessage::Close {
                    conn_id: self.conn_id.clone(),
                });
            }
            self.refund_pending_write_quota();
            self.inbound_map.remove(&self.conn_id);
            self.metrics.close_connection(&self.conn_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::task::noop_waker;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::task::Context;
    use tokio::io::ReadBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{timeout, Duration};
    use tp_core::protocol::{unpack, TcpFlowStreamPreface};
    use tp_transport::{session::Session, TcpFlowStream};

    fn channel_sender() -> (SessionSender, mpsc::Receiver<PackedMessage>) {
        channel_sender_with_capacity(8)
    }

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
    async fn poll_write_waiting_for_channel_capacity_does_not_charge_quota() {
        let conn_id = "gwquota01".to_string();
        let (tx, mut outbound_rx) = channel_sender_with_capacity(1);
        tx.try_send(BinaryMessage::Data {
            conn_id: "filled".into(),
            payload: Bytes::from_static(b"x"),
        })
        .expect("fill stream channel");
        let (_in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        let inbound = Arc::new(DashMap::new());
        let quota = Arc::new(RelayQuotaLimiter::default());
        quota.update("tun-tcp", "202605", 10, 10);

        let mut conn = TunneledConn::new(
            conn_id.clone(),
            in_rx,
            tx,
            inbound,
            Arc::new(BandwidthLimiter::new(0)),
            Some(quota.clone()),
            MetricsManager::new(),
            "client-1".into(),
        );

        let write = conn.write_all(b"hello");
        tokio::pin!(write);
        assert!(
            timeout(Duration::from_millis(20), &mut write)
                .await
                .is_err(),
            "full stream channel should park poll_write"
        );
        assert_eq!(quota.remaining_bytes(), Some(10));

        outbound_rx.recv().await.expect("drain filler frame");
        timeout(Duration::from_secs(1), &mut write)
            .await
            .expect("write wakes after channel capacity returns")
            .expect("write succeeds");
        assert_eq!(quota.remaining_bytes(), Some(5));

        let frame = outbound_rx.recv().await.expect("queued data frame");
        match unpack(&frame.to_bytes()).expect("decode data") {
            BinaryMessage::Data {
                conn_id: got,
                payload,
            } => {
                assert_eq!(got, conn_id);
                assert_eq!(&payload[..], b"hello");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_keeps_read_half_open_for_remote_response() {
        let conn_id = "gateway-half".to_string();
        let (tx, mut outbound_rx) = channel_sender();
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        let inbound = Arc::new(DashMap::new());
        inbound.insert(conn_id.clone(), in_tx.clone());

        let mut conn = TunneledConn::new(
            conn_id.clone(),
            in_rx,
            tx,
            inbound.clone(),
            Arc::new(BandwidthLimiter::new(0)),
            None,
            MetricsManager::new(),
            "client-1".into(),
        );

        conn.shutdown().await.expect("shutdown write half");
        let close = outbound_rx
            .recv()
            .await
            .expect("shutdown should send ordered close");
        match unpack(&close.to_bytes()).expect("decode close") {
            BinaryMessage::Close { conn_id: got } => assert_eq!(got, conn_id),
            other => panic!("expected Close, got {other:?}"),
        }
        assert!(
            inbound.contains_key(&conn_id),
            "write shutdown must keep inbound route for the remote response"
        );

        in_tx
            .send(Bytes::from_static(b"HTTP/1.1 200 OK\r\n\r\n"))
            .await
            .expect("send remote response");
        let mut buf = [0u8; 19];
        conn.read_exact(&mut buf)
            .await
            .expect("read response after local write shutdown");
        assert_eq!(&buf, b"HTTP/1.1 200 OK\r\n\r\n");

        drop(conn);
        assert!(
            !inbound.contains_key(&conn_id),
            "dropping the connection must remove the inbound route"
        );
    }

    #[tokio::test]
    async fn tcp_flow_read_eof_still_shutdowns_and_drops_metrics() {
        let conn_id = "flow-half".to_string();
        let (tx, _outbound_rx) = channel_sender();
        let inbound = Arc::new(DashMap::new());
        let limiter = Arc::new(BandwidthLimiter::new(0));
        let metrics = MetricsManager::new();
        metrics.create_connection(&conn_id, "client-1", "127.0.0.1:9000");
        assert_eq!(metrics.global().active_connections, 1);

        let (mut peer, gateway_side) = tokio::io::duplex(1024);
        let preface = TcpFlowStreamPreface {
            conn_id: conn_id.clone(),
            network: "tcp".into(),
            address: "127.0.0.1:9000".into(),
        };
        let stream = TcpFlowStream::new(preface, Box::pin(gateway_side));
        let mut conn = TunneledConn::new_with_tcp_flow_stream(
            conn_id.clone(),
            stream,
            tx,
            inbound,
            limiter,
            None,
            metrics.clone(),
            "client-1".into(),
        );

        peer.write_all(b"hello").await.expect("peer write");
        peer.shutdown().await.expect("peer write eof");
        let mut buf = [0u8; 5];
        conn.read_exact(&mut buf).await.expect("read flow bytes");
        assert_eq!(&buf, b"hello");
        let mut eof = [0u8; 1];
        assert_eq!(conn.read(&mut eof).await.expect("read eof"), 0);

        conn.shutdown()
            .await
            .expect("local write FIN after remote read EOF");
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail)
            .await
            .expect("peer observes local FIN");
        drop(conn);
        assert_eq!(metrics.global().active_connections, 0);
    }

    #[tokio::test]
    async fn tcp_flow_read_limiter_retries_and_charges_after_sleep() {
        let conn_id = "flow-limit".to_string();
        let (tx, _outbound_rx) = channel_sender();
        let inbound = Arc::new(DashMap::new());
        let limiter = Arc::new(BandwidthLimiter::new(1));
        for chunk in 0..6 {
            limiter
                .try_acquire(BANDWIDTH_CHUNK)
                .unwrap_or_else(|_| panic!("drain 3s burst chunk {chunk}"));
        }
        let quota = Arc::new(RelayQuotaLimiter::default());
        quota.update(
            "tun-flow",
            "202605",
            (BANDWIDTH_CHUNK * 2) as u64,
            (BANDWIDTH_CHUNK * 2) as u64,
        );
        let metrics = MetricsManager::new();
        metrics.create_connection(&conn_id, "client-1", "127.0.0.1:9000");

        let (mut peer, gateway_side) = tokio::io::duplex(BANDWIDTH_CHUNK * 2);
        let preface = TcpFlowStreamPreface {
            conn_id: conn_id.clone(),
            network: "tcp".into(),
            address: "127.0.0.1:9000".into(),
        };
        let stream = TcpFlowStream::new(preface, Box::pin(gateway_side));
        let mut conn = TunneledConn::new_with_tcp_flow_stream(
            conn_id,
            stream,
            tx,
            inbound,
            limiter.clone(),
            Some(quota.clone()),
            metrics,
            "client-1".into(),
        );

        let payload = vec![7u8; BANDWIDTH_CHUNK];
        peer.write_all(&payload).await.expect("peer write payload");
        while limiter.try_acquire(TCP_FLOW_READ_CHUNK).is_ok() {}

        let mut out = vec![0u8; BANDWIDTH_CHUNK];
        {
            let mut immediate = ReadBuf::new(&mut out);
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            match Pin::new(&mut conn).poll_read(&mut cx, &mut immediate) {
                Poll::Pending => {}
                Poll::Ready(result) => {
                    panic!("read should wait for bandwidth tokens: {result:?}")
                }
            }
            assert!(immediate.filled().is_empty());
            assert_eq!(quota.remaining_bytes(), Some((BANDWIDTH_CHUNK * 2) as u64));
        }

        let writer = tokio::spawn(async move {
            peer.shutdown().await.expect("peer shutdown");
        });
        let read = conn.read_exact(&mut out);
        tokio::pin!(read);
        timeout(Duration::from_secs(2), &mut read)
            .await
            .expect("read wakes after limiter sleep")
            .expect("read rate-limited chunk");
        assert_eq!(out[0], 7);
        assert_eq!(quota.remaining_bytes(), Some(BANDWIDTH_CHUNK as u64));
        assert!(
            limiter.try_acquire(BANDWIDTH_CHUNK).is_err(),
            "read-side limiter must consume tokens after the wait, not deliver an uncharged chunk"
        );
        writer.await.expect("writer task");
    }
}
