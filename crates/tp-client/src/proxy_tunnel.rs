use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio_util::sync::PollSender;
use tp_core::protocol::BinaryMessage;
use tp_transport::{DropOldestReceiver, DropOldestSender, TcpFlowStream, TrySendKind};

use crate::p2p::multi_sender::MultiSenderRouter;
use crate::p2p::session::TcpFlowStreamGuard;
use crate::relay_crypto::{RelayCipherV2, RelayFlowKindV2, RelayRecordContextV2};

const MAX_TCP_WRITE_CHUNK: usize = 64 * 1024;

type FlowCloseHook = Arc<dyn Fn(&str) + Send + Sync + 'static>;
type FlowTrafficHook = Arc<dyn Fn(&str, u64) + Send + Sync + 'static>;

enum ProxyTunnelWrite {
    Data(BytesMut),
    Close,
}

#[derive(Clone, Default)]
pub(crate) struct ProxyTunnelConnHooks {
    pub(crate) on_close: Option<FlowCloseHook>,
    pub(crate) on_data_sent: Option<FlowTrafficHook>,
    pub(crate) on_data_received: Option<FlowTrafficHook>,
}

#[derive(Clone, Default)]
pub(crate) struct ProxyTunnelDatagramHooks {
    pub(crate) on_close: Option<FlowCloseHook>,
    pub(crate) on_data_sent: Option<FlowTrafficHook>,
    pub(crate) on_data_received: Option<FlowTrafficHook>,
}

pub struct ProxyTunnelConn {
    conn_id: String,
    rx: mpsc::Receiver<Bytes>,
    write_tx: mpsc::Sender<ProxyTunnelWrite>,
    write_sender: PollSender<ProxyTunnelWrite>,
    inbound_maps: Vec<Arc<DashMap<String, mpsc::Sender<Bytes>>>>,
    pending_writes: Arc<AtomicUsize>,
    flush_waker: Arc<AtomicWaker>,
    leftover: Option<Bytes>,
    closed: bool,
    write_closed: bool,
    hooks: ProxyTunnelConnHooks,
    tcp_flow_stream: Option<TcpFlowStream>,
    sealed_tcp_flow: Option<SealedTcpFlowEndpoint>,
    _tcp_flow_guard: Option<TcpFlowStreamGuard>,
}

struct SealedTcpFlowEndpoint {
    app_to_flow_sender: PollSender<BytesMut>,
}

impl ProxyTunnelConn {
    pub fn new(
        conn_id: String,
        rx: mpsc::Receiver<Bytes>,
        router: MultiSenderRouter,
        inbound_map: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
    ) -> Self {
        Self::new_with_inbound_maps(conn_id, rx, router, vec![inbound_map])
    }

    pub(crate) fn new_with_inbound_maps(
        conn_id: String,
        rx: mpsc::Receiver<Bytes>,
        router: MultiSenderRouter,
        inbound_maps: Vec<Arc<DashMap<String, mpsc::Sender<Bytes>>>>,
    ) -> Self {
        Self::new_with_inbound_maps_and_hooks(
            conn_id,
            rx,
            router,
            inbound_maps,
            ProxyTunnelConnHooks::default(),
        )
    }

    pub(crate) fn new_with_inbound_maps_and_hooks(
        conn_id: String,
        rx: mpsc::Receiver<Bytes>,
        router: MultiSenderRouter,
        inbound_maps: Vec<Arc<DashMap<String, mpsc::Sender<Bytes>>>>,
        hooks: ProxyTunnelConnHooks,
    ) -> Self {
        let (write_tx, mut write_rx) = mpsc::channel::<ProxyTunnelWrite>(64);
        let writer_router = router.clone();
        let writer_conn_id = conn_id.clone();
        let writer_hooks = hooks.clone();
        let pending_writes = Arc::new(AtomicUsize::new(0));
        let writer_pending_writes = pending_writes.clone();
        let flush_waker = Arc::new(AtomicWaker::new());
        let writer_flush_waker = flush_waker.clone();
        tokio::spawn(async move {
            while let Some(cmd) = write_rx.recv().await {
                match cmd {
                    ProxyTunnelWrite::Data(payload) => {
                        let payload_bytes = payload
                            .len()
                            .saturating_sub(crate::relay_crypto::RELAY_NONCE_SIZE_V2)
                            as u64;
                        let send_result = writer_router
                            .send_prepared_data(
                                writer_conn_id.clone(),
                                crate::relay_crypto::RelayFramedKindV2::Data,
                                payload,
                            )
                            .await;
                        if send_result.is_ok() {
                            if let Some(on_data_sent) = &writer_hooks.on_data_sent {
                                on_data_sent(&writer_conn_id, payload_bytes);
                            }
                        }
                        if writer_pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
                            writer_flush_waker.wake();
                        }
                        if send_result.is_err() {
                            break;
                        }
                    }
                    ProxyTunnelWrite::Close => {
                        let _ = writer_router
                            .send(BinaryMessage::Close {
                                conn_id: writer_conn_id.clone(),
                            })
                            .await;
                        break;
                    }
                }
            }
            while let Ok(cmd) = write_rx.try_recv() {
                if matches!(cmd, ProxyTunnelWrite::Data(_))
                    && writer_pending_writes.fetch_sub(1, Ordering::AcqRel) == 1
                {
                    writer_flush_waker.wake();
                }
            }
            writer_flush_waker.wake();
        });

        Self {
            conn_id,
            rx,
            write_tx: write_tx.clone(),
            write_sender: PollSender::new(write_tx),
            inbound_maps,
            pending_writes,
            flush_waker,
            leftover: None,
            closed: false,
            write_closed: false,
            hooks,
            tcp_flow_stream: None,
            sealed_tcp_flow: None,
            _tcp_flow_guard: None,
        }
    }

    pub(crate) fn new_with_tcp_flow_stream(
        conn_id: String,
        stream: TcpFlowStream,
        hooks: ProxyTunnelConnHooks,
        tcp_flow_guard: Option<TcpFlowStreamGuard>,
    ) -> Self {
        let (_in_tx, rx) = mpsc::channel::<Bytes>(1);
        let (write_tx, _write_rx) = mpsc::channel::<ProxyTunnelWrite>(1);
        Self {
            conn_id,
            rx,
            write_tx: write_tx.clone(),
            write_sender: PollSender::new(write_tx),
            inbound_maps: Vec::new(),
            pending_writes: Arc::new(AtomicUsize::new(0)),
            flush_waker: Arc::new(AtomicWaker::new()),
            leftover: None,
            closed: false,
            write_closed: false,
            hooks,
            tcp_flow_stream: Some(stream),
            sealed_tcp_flow: None,
            _tcp_flow_guard: tcp_flow_guard,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_sealed_tcp_flow_stream(
        conn_id: String,
        stream: TcpFlowStream,
        tunnel_id: String,
        session_id: tp_core::p2p_types::SessionId,
        local_peer_id: String,
        remote_peer_id: String,
        cipher: Arc<RelayCipherV2>,
        hooks: ProxyTunnelConnHooks,
        tcp_flow_guard: Option<TcpFlowStreamGuard>,
    ) -> Self {
        let conn_id_wire = conn_id_wire(&conn_id).expect("validated V2 Relay conn_id");
        let (app_to_flow_tx, mut app_to_flow_rx) = mpsc::channel::<BytesMut>(64);
        let (flow_to_app_tx, rx) = mpsc::channel::<Bytes>(64);
        let pending_writes = Arc::new(AtomicUsize::new(0));
        let flush_waker = Arc::new(AtomicWaker::new());
        let pump_tunnel_id = tunnel_id.clone();
        let pump_local = local_peer_id.clone();
        let pump_remote = remote_peer_id.clone();
        let pump_cipher = cipher.clone();
        let pump_conn_id = conn_id.clone();
        let writer_hooks = hooks.clone();
        let writer_conn_id = conn_id.clone();
        let (mut flow_read, mut flow_write) = tokio::io::split(stream);
        let writer_tunnel_id = pump_tunnel_id.clone();
        let writer_local = pump_local.clone();
        let writer_remote = pump_remote.clone();
        let writer_cipher = pump_cipher.clone();
        let writer_pending_writes = pending_writes.clone();
        let writer_flush_waker = flush_waker.clone();
        tokio::spawn(async move {
            let aad = crate::relay_crypto::RelayAadV2::flow(
                RelayRecordContextV2 {
                    tunnel_id: &writer_tunnel_id,
                    peerlink_session_id: &session_id,
                    source_peer_id: &writer_local,
                    target_peer_id: &writer_remote,
                    conn_id: &conn_id_wire,
                },
                RelayFlowKindV2::Data,
            );
            if let Ok(aad) = aad {
                while let Some(mut sealed) = app_to_flow_rx.recv().await {
                    let payload_len = sealed
                        .len()
                        .saturating_sub(crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                    let failed = writer_cipher.seal_precomputed(&aad, &mut sealed).is_err()
                        || tp_transport::session::write_tcp_flow_frame(&mut flow_write, &sealed)
                            .await
                            .is_err();
                    if !failed {
                        if let Some(hook) = &writer_hooks.on_data_sent {
                            hook(&writer_conn_id, payload_len as u64);
                        }
                    }
                    if writer_pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
                        writer_flush_waker.wake();
                    }
                    if failed {
                        break;
                    }
                }
            }
            while app_to_flow_rx.try_recv().is_ok() {
                if writer_pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
                    writer_flush_waker.wake();
                }
            }
            writer_flush_waker.wake();
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut flow_write).await;
        });
        tokio::spawn(async move {
            let aad = crate::relay_crypto::RelayAadV2::flow(
                RelayRecordContextV2 {
                    tunnel_id: &pump_tunnel_id,
                    peerlink_session_id: &session_id,
                    source_peer_id: &pump_remote,
                    target_peer_id: &pump_local,
                    conn_id: &conn_id_wire,
                },
                RelayFlowKindV2::Data,
            );
            let Ok(aad) = aad else {
                return;
            };
            let mut arena = BytesMut::with_capacity(
                crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2 * 4
                    + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
            );
            loop {
                if arena.capacity()
                    < crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2
                        + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2
                {
                    arena.reserve(
                        crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2 * 4
                            + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
                    );
                }
                if let Err(error) = tp_transport::session::read_tcp_flow_frame_into_bytes(
                    &mut flow_read,
                    &mut arena,
                )
                .await
                {
                    tracing::debug!(
                        conn_id = %pump_conn_id,
                        %error,
                        "sealed Relay TCP flow reader ended"
                    );
                    break;
                }
                if let Err(error) = pump_cipher.open_precomputed(&aad, &mut arena) {
                    tracing::debug!(
                        conn_id = %pump_conn_id,
                        %error,
                        "sealed Relay TCP flow record authentication failed"
                    );
                    break;
                }
                if flow_to_app_tx.send(arena.split().freeze()).await.is_err() {
                    break;
                }
            }
        });
        let (write_tx, _write_rx) = mpsc::channel::<ProxyTunnelWrite>(1);
        Self {
            conn_id,
            rx,
            write_tx: write_tx.clone(),
            write_sender: PollSender::new(write_tx),
            inbound_maps: Vec::new(),
            pending_writes,
            flush_waker,
            leftover: None,
            closed: false,
            write_closed: false,
            hooks,
            tcp_flow_stream: None,
            sealed_tcp_flow: Some(SealedTcpFlowEndpoint {
                app_to_flow_sender: PollSender::new(app_to_flow_tx.clone()),
            }),
            _tcp_flow_guard: tcp_flow_guard,
        }
    }

    fn close_once(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        remove_tcp_inbound_maps(&self.inbound_maps, &self.conn_id);
        if let Some(on_close) = &self.hooks.on_close {
            on_close(&self.conn_id);
        }
        if let Some(sealed) = self.sealed_tcp_flow.as_mut() {
            sealed.app_to_flow_sender.close();
            return;
        }
        if self.tcp_flow_stream.is_some() {
            return;
        }
        if self.write_closed {
            return;
        }
        self.write_closed = true;
        match self.write_tx.try_send(ProxyTunnelWrite::Close) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let tx = self.write_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(cmd).await;
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn poll_ordered_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.closed || self.write_closed {
            return Poll::Ready(Ok(()));
        }
        match self.write_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => {
                self.write_closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(())) => match self.write_sender.send_item(ProxyTunnelWrite::Close) {
                Ok(()) => {
                    self.write_closed = true;
                    self.write_sender.close();
                    Poll::Ready(Ok(()))
                }
                Err(_) => {
                    self.write_closed = true;
                    Poll::Ready(Ok(()))
                }
            },
        }
    }
}

impl AsyncRead for ProxyTunnelConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(stream) = self.tcp_flow_stream.as_mut() {
            let before = buf.filled().len();
            let poll = Pin::new(stream).poll_read(cx, buf);
            if matches!(poll, Poll::Ready(Ok(()))) {
                let n = buf.filled().len().saturating_sub(before);
                if n > 0 {
                    if let Some(on_data_received) = &self.hooks.on_data_received {
                        on_data_received(&self.conn_id, n as u64);
                    }
                }
            }
            return poll;
        }

        loop {
            if let Some(mut chunk) = self.leftover.take() {
                let n = chunk.len().min(buf.remaining());
                let front = chunk.split_to(n);
                buf.put_slice(&front);
                if !chunk.is_empty() {
                    self.leftover = Some(chunk);
                }
                if n > 0 {
                    if let Some(on_data_received) = &self.hooks.on_data_received {
                        on_data_received(&self.conn_id, n as u64);
                    }
                }
                return Poll::Ready(Ok(()));
            }

            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(data)) => {
                    if !data.is_empty() {
                        self.leftover = Some(data);
                    }
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for ProxyTunnelConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed || self.write_closed {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Some(stream) = self.tcp_flow_stream.as_mut() {
            return match Pin::new(stream).poll_write(cx, buf) {
                Poll::Ready(Ok(n)) => {
                    if n > 0 {
                        if let Some(on_data_sent) = &self.hooks.on_data_sent {
                            on_data_sent(&self.conn_id, n as u64);
                        }
                    }
                    Poll::Ready(Ok(n))
                }
                other => other,
            };
        }
        let sealed_pending_writes = self.pending_writes.clone();
        let sealed_flush_waker = self.flush_waker.clone();
        if let Some(sealed) = self.sealed_tcp_flow.as_mut() {
            let n = buf.len().min(MAX_TCP_WRITE_CHUNK);
            return match sealed.app_to_flow_sender.poll_reserve(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "sealed tunnel closed",
                ))),
                Poll::Ready(Ok(())) => {
                    let mut record =
                        BytesMut::with_capacity(n + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2);
                    record.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                    record.extend_from_slice(&buf[..n]);
                    sealed_pending_writes.fetch_add(1, Ordering::AcqRel);
                    match sealed.app_to_flow_sender.send_item(record) {
                        Ok(()) => Poll::Ready(Ok(n)),
                        Err(_) => {
                            if sealed_pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
                                sealed_flush_waker.wake();
                            }
                            Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "sealed tunnel closed",
                            )))
                        }
                    }
                }
            };
        }

        let n = buf.len().min(MAX_TCP_WRITE_CHUNK);
        match self.write_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel closed",
            ))),
            Poll::Ready(Ok(())) => {
                let mut record =
                    BytesMut::with_capacity(n + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2);
                record.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                record.extend_from_slice(&buf[..n]);
                self.pending_writes.fetch_add(1, Ordering::AcqRel);
                match self.write_sender.send_item(ProxyTunnelWrite::Data(record)) {
                    Ok(()) => Poll::Ready(Ok(n)),
                    Err(_) => {
                        if self.pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
                            self.flush_waker.wake();
                        }
                        Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "tunnel closed",
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
        if self.pending_writes.load(Ordering::Acquire) == 0 {
            return Poll::Ready(Ok(()));
        }
        self.flush_waker.register(cx.waker());
        if self.pending_writes.load(Ordering::Acquire) == 0 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if let Some(stream) = self.tcp_flow_stream.as_mut() {
            let poll = Pin::new(stream).poll_shutdown(cx);
            if poll.is_ready() {
                self.write_closed = true;
            }
            return poll;
        }
        if let Some(sealed) = self.sealed_tcp_flow.as_mut() {
            sealed.app_to_flow_sender.close();
            self.write_closed = true;
            return Poll::Ready(Ok(()));
        }
        self.poll_ordered_shutdown(cx)
    }
}

fn conn_id_wire(conn_id: &str) -> Option<[u8; 12]> {
    let bytes = conn_id.as_bytes();
    if bytes.is_empty() || bytes.len() > 12 || !bytes.is_ascii() || bytes.contains(&0) {
        return None;
    }
    let mut wire = [0_u8; 12];
    wire[..bytes.len()].copy_from_slice(bytes);
    Some(wire)
}

impl Drop for ProxyTunnelConn {
    fn drop(&mut self) {
        self.close_once();
    }
}

pub struct ProxyTunnelDatagram {
    conn_id: String,
    rx: DropOldestReceiver<Bytes>,
    router: MultiSenderRouter,
    inbound_maps: Vec<Arc<DashMap<String, DropOldestSender<Bytes>>>>,
    closed: bool,
    hooks: ProxyTunnelDatagramHooks,
}

impl ProxyTunnelDatagram {
    pub fn new(
        conn_id: String,
        rx: DropOldestReceiver<Bytes>,
        router: MultiSenderRouter,
        inbound_map: Arc<DashMap<String, DropOldestSender<Bytes>>>,
    ) -> Self {
        Self::new_with_inbound_maps(conn_id, rx, router, vec![inbound_map])
    }

    pub(crate) fn new_with_inbound_maps(
        conn_id: String,
        rx: DropOldestReceiver<Bytes>,
        router: MultiSenderRouter,
        inbound_maps: Vec<Arc<DashMap<String, DropOldestSender<Bytes>>>>,
    ) -> Self {
        Self::new_with_inbound_maps_and_hooks(
            conn_id,
            rx,
            router,
            inbound_maps,
            ProxyTunnelDatagramHooks::default(),
        )
    }

    pub(crate) fn new_with_inbound_maps_and_hooks(
        conn_id: String,
        rx: DropOldestReceiver<Bytes>,
        router: MultiSenderRouter,
        inbound_maps: Vec<Arc<DashMap<String, DropOldestSender<Bytes>>>>,
        hooks: ProxyTunnelDatagramHooks,
    ) -> Self {
        Self {
            conn_id,
            rx,
            router,
            inbound_maps,
            closed: false,
            hooks,
        }
    }

    pub fn try_send(&self, payload: Bytes) -> Result<(), TrySendKind> {
        if self.closed {
            return Err(TrySendKind::Closed);
        }
        let payload_bytes = payload.len() as u64;
        let result = self.router.try_send_prepared_data(
            self.conn_id.clone(),
            crate::relay_crypto::RelayFramedKindV2::UdpData,
            prepare_relay_record(&payload),
        );
        if result.is_ok() {
            if let Some(on_data_sent) = &self.hooks.on_data_sent {
                on_data_sent(&self.conn_id, payload_bytes);
            }
        }
        result
    }

    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub async fn recv(&mut self) -> Option<Bytes> {
        let payload = self.rx.recv().await;
        if let Some(payload) = payload.as_ref() {
            self.run_data_received_hook(payload.len());
        }
        payload
    }

    pub fn try_recv(&mut self) -> Result<Bytes, TryRecvError> {
        let payload = self.rx.try_recv()?;
        self.run_data_received_hook(payload.len());
        Ok(payload)
    }

    pub fn split(mut self) -> (ProxyTunnelDatagramSender, ProxyTunnelDatagramReceiver) {
        self.closed = true;
        let sender = ProxyTunnelDatagramSender {
            conn_id: self.conn_id.clone(),
            router: self.router.clone(),
            hooks: self.hooks.clone(),
        };
        let (_, sentinel_rx) = tp_transport::drop_oldest_channel::<Bytes>(1);
        let rx = std::mem::replace(&mut self.rx, sentinel_rx);
        let receiver = ProxyTunnelDatagramReceiver {
            conn_id: self.conn_id.clone(),
            rx,
            router: self.router.clone(),
            inbound_maps: self.inbound_maps.clone(),
            closed: false,
            hooks: self.hooks.clone(),
        };
        (sender, receiver)
    }

    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.send_close();
        remove_udp_inbound_maps(&self.inbound_maps, &self.conn_id);
        self.run_close_hook();
    }

    fn close_once(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.send_close();
        remove_udp_inbound_maps(&self.inbound_maps, &self.conn_id);
        self.run_close_hook();
    }

    fn send_close(&self) {
        let _ = self.router.try_send(BinaryMessage::Close {
            conn_id: self.conn_id.clone(),
        });
    }

    fn run_close_hook(&self) {
        if let Some(on_close) = &self.hooks.on_close {
            on_close(&self.conn_id);
        }
    }

    fn run_data_received_hook(&self, payload_bytes: usize) {
        if payload_bytes == 0 {
            return;
        }
        if let Some(on_data_received) = &self.hooks.on_data_received {
            on_data_received(
                &self.conn_id,
                u64::try_from(payload_bytes).unwrap_or(u64::MAX),
            );
        }
    }
}

impl Drop for ProxyTunnelDatagram {
    fn drop(&mut self) {
        self.close_once();
    }
}

#[derive(Clone)]
pub struct ProxyTunnelDatagramSender {
    conn_id: String,
    router: MultiSenderRouter,
    hooks: ProxyTunnelDatagramHooks,
}

impl ProxyTunnelDatagramSender {
    /// Non-blocking UDP send. `DatagramUnavailable` means the selected QUIC
    /// path cannot carry UDP datagrams, so callers should fail the association
    /// instead of relying on reliable-stream fallback.
    pub fn try_send(&self, payload: Bytes) -> Result<(), TrySendKind> {
        let payload_bytes = payload.len() as u64;
        let result = self.router.try_send_prepared_data(
            self.conn_id.clone(),
            crate::relay_crypto::RelayFramedKindV2::UdpData,
            prepare_relay_record(&payload),
        );
        if result.is_ok() {
            if let Some(on_data_sent) = &self.hooks.on_data_sent {
                on_data_sent(&self.conn_id, payload_bytes);
            }
        }
        result
    }
}

fn prepare_relay_record(payload: &[u8]) -> BytesMut {
    let mut record =
        BytesMut::with_capacity(payload.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2);
    record.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
    record.extend_from_slice(payload);
    record
}

pub struct ProxyTunnelDatagramReceiver {
    conn_id: String,
    rx: DropOldestReceiver<Bytes>,
    router: MultiSenderRouter,
    inbound_maps: Vec<Arc<DashMap<String, DropOldestSender<Bytes>>>>,
    closed: bool,
    hooks: ProxyTunnelDatagramHooks,
}

impl ProxyTunnelDatagramReceiver {
    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    pub async fn recv(&mut self) -> Option<Bytes> {
        let payload = self.rx.recv().await;
        if let Some(payload) = payload.as_ref() {
            self.run_data_received_hook(payload.len());
        }
        payload
    }

    pub fn try_recv(&mut self) -> Result<Bytes, TryRecvError> {
        let payload = self.rx.try_recv()?;
        self.run_data_received_hook(payload.len());
        Ok(payload)
    }

    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.send_close();
        remove_udp_inbound_maps(&self.inbound_maps, &self.conn_id);
        self.run_close_hook();
    }

    fn close_once(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.send_close();
        remove_udp_inbound_maps(&self.inbound_maps, &self.conn_id);
        self.run_close_hook();
    }

    fn send_close(&self) {
        let _ = self.router.try_send(BinaryMessage::Close {
            conn_id: self.conn_id.clone(),
        });
    }

    fn run_close_hook(&self) {
        if let Some(on_close) = &self.hooks.on_close {
            on_close(&self.conn_id);
        }
    }

    fn run_data_received_hook(&self, payload_bytes: usize) {
        if payload_bytes == 0 {
            return;
        }
        if let Some(on_data_received) = &self.hooks.on_data_received {
            on_data_received(
                &self.conn_id,
                u64::try_from(payload_bytes).unwrap_or(u64::MAX),
            );
        }
    }
}

impl Drop for ProxyTunnelDatagramReceiver {
    fn drop(&mut self) {
        self.close_once();
    }
}

fn remove_tcp_inbound_maps(maps: &[Arc<DashMap<String, mpsc::Sender<Bytes>>>], conn_id: &str) {
    for map in maps {
        map.remove(conn_id);
    }
}

fn remove_udp_inbound_maps(maps: &[Arc<DashMap<String, DropOldestSender<Bytes>>>], conn_id: &str) {
    for map in maps {
        map.remove(conn_id);
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::{BufMut, Bytes, BytesMut};
    use dashmap::DashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tp_core::p2p_types::SessionId;
    use tp_core::protocol::{unpack, BinaryMessage, PackedMessage, TcpFlowStreamPreface};
    use tp_transport::session::Session;
    use tp_transport::{
        datagram_scheduler_channel, drop_oldest_channel, DatagramSchedulerConfig,
        DatagramSchedulerReceiver, DropOldestSender,
    };

    use crate::p2p::multi_sender::MultiSenderRouter;
    use crate::p2p::session::MultiSession;
    use crate::proxy_tunnel::{
        ProxyTunnelConn, ProxyTunnelConnHooks, ProxyTunnelDatagram, ProxyTunnelDatagramHooks,
        MAX_TCP_WRITE_CHUNK,
    };

    fn channel_session() -> (Arc<Session>, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        (
            Arc::new(Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            )),
            out_rx,
        )
    }

    fn datagram_session(
        per_association_packet_limit: usize,
        global_packet_limit: usize,
    ) -> (Arc<Session>, DatagramSchedulerReceiver) {
        let (out_tx, _out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let (dg_tx, dg_out_rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit,
            global_packet_limit,
            ..DatagramSchedulerConfig::for_test()
        });
        let (_dg_in_tx, dg_in_rx) = drop_oldest_channel::<BinaryMessage>(8);
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        )
        .with_datagram_channel(
            dg_tx,
            dg_in_rx,
            Arc::new(|| Some(1452)),
            Arc::new(|| 1024 * 1024),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        (Arc::new(session), dg_out_rx)
    }

    fn make_multi(relay: Arc<Session>) -> Arc<MultiSession> {
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
    }

    async fn recv_msg(rx: &mut mpsc::Receiver<PackedMessage>) -> BinaryMessage {
        let packed = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for routed message")
            .expect("routed message channel closed");
        unpack(&packed.to_bytes()).expect("decode routed message")
    }

    #[tokio::test]
    async fn tcp_write_routes_data_and_shutdown_routes_close() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        multi.inbound().insert("tcp-1".into(), in_tx);

        let mut conn = ProxyTunnelConn::new("tcp-1".into(), in_rx, router, multi.inbound());

        conn.write_all(b"hello").await.expect("write");
        conn.shutdown().await.expect("shutdown");

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Data { conn_id, payload } => {
                assert_eq!(conn_id, "tcp-1");
                assert_eq!(payload, Bytes::from_static(b"hello"));
            }
            other => panic!("expected Data, got {other:?}"),
        }

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Close { conn_id } => assert_eq!(conn_id, "tcp-1"),
            other => panic!("expected Close, got {other:?}"),
        }
        assert!(
            multi.inbound().get("tcp-1").is_some(),
            "write shutdown is a TCP FIN; the read half must stay open for the response"
        );
        drop(conn);
        assert!(multi.inbound().get("tcp-1").is_none());
    }

    #[tokio::test]
    async fn tcp_shutdown_keeps_read_half_open_for_remote_response() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        multi.inbound().insert("tcp-half".into(), in_tx.clone());
        let mut conn = ProxyTunnelConn::new("tcp-half".into(), in_rx, router, multi.inbound());

        conn.shutdown().await.expect("shutdown write half");
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Close { conn_id } => assert_eq!(conn_id, "tcp-half"),
            other => panic!("expected Close, got {other:?}"),
        }
        assert!(
            multi.inbound().get("tcp-half").is_some(),
            "remote response path must remain installed after local write shutdown"
        );

        in_tx
            .send(Bytes::from_static(b"HTTP/1.1 200 OK\r\n\r\n"))
            .await
            .expect("deliver remote response");
        let mut response = vec![0; 19];
        conn.read_exact(&mut response)
            .await
            .expect("read response after write shutdown");
        assert_eq!(&response, b"HTTP/1.1 200 OK\r\n\r\n");
    }

    #[tokio::test]
    async fn sealed_tcp_shutdown_drains_write_and_keeps_read_half_open() {
        let tunnel_id = "sealed-tunnel".to_string();
        let session_id = SessionId::from_bytes([0x31; 16]);
        let source_peer_id = "source-peer".to_string();
        let target_peer_id = "target-peer".to_string();
        let conn_id = "seal-proxy1".to_string();
        let conn_id_wire = super::conn_id_wire(&conn_id).expect("valid conn id");
        let source_cipher = Arc::new(
            crate::relay_crypto::RelayCipherV2::from_directional_keys_for_test(
                [0x41; 32], [0x42; 32],
            ),
        );
        let target_cipher = crate::relay_crypto::RelayCipherV2::from_directional_keys_for_test(
            [0x42; 32], [0x41; 32],
        );
        let source_to_target = crate::relay_crypto::RelayAadV2::flow(
            crate::relay_crypto::RelayRecordContextV2 {
                tunnel_id: &tunnel_id,
                peerlink_session_id: &session_id,
                source_peer_id: &source_peer_id,
                target_peer_id: &target_peer_id,
                conn_id: &conn_id_wire,
            },
            crate::relay_crypto::RelayFlowKindV2::Data,
        )
        .expect("source-to-target AAD");
        let target_to_source = crate::relay_crypto::RelayAadV2::flow(
            crate::relay_crypto::RelayRecordContextV2 {
                tunnel_id: &tunnel_id,
                peerlink_session_id: &session_id,
                source_peer_id: &target_peer_id,
                target_peer_id: &source_peer_id,
                conn_id: &conn_id_wire,
            },
            crate::relay_crypto::RelayFlowKindV2::Data,
        )
        .expect("target-to-source AAD");

        let (local_io, mut remote_io) = tokio::io::duplex(64 * 1024);
        let stream = tp_transport::TcpFlowStream::new(
            TcpFlowStreamPreface {
                conn_id: conn_id.clone(),
                network: "tcp".into(),
                address: String::new(),
            },
            Box::pin(local_io),
        );
        let mut conn = ProxyTunnelConn::new_with_sealed_tcp_flow_stream(
            conn_id,
            stream,
            tunnel_id,
            session_id,
            source_peer_id,
            target_peer_id,
            source_cipher,
            ProxyTunnelConnHooks::default(),
            None,
        );

        let remote = tokio::spawn(async move {
            let mut request = BytesMut::with_capacity(64);
            tp_transport::session::read_tcp_flow_frame_into_bytes(&mut remote_io, &mut request)
                .await
                .expect("read sealed request");
            target_cipher
                .open_precomputed(&source_to_target, &mut request)
                .expect("open sealed request");
            assert_eq!(request, b"ping".as_slice());

            assert!(
                tp_transport::session::read_tcp_flow_frame(&mut remote_io)
                    .await
                    .is_err(),
                "local shutdown must close only the stream write half after queued data"
            );

            let mut response =
                BytesMut::with_capacity(4 + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2);
            response.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
            response.extend_from_slice(b"pong");
            target_cipher
                .seal_precomputed(&target_to_source, &mut response)
                .expect("seal response");
            tp_transport::session::write_tcp_flow_frame(&mut remote_io, &response)
                .await
                .expect("write sealed response after remote FIN");
            remote_io
                .shutdown()
                .await
                .expect("shutdown remote write half");
        });

        conn.write_all(b"ping").await.expect("write request");
        conn.shutdown().await.expect("shutdown local write half");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), conn.read_to_end(&mut response))
            .await
            .expect("timed out reading after local shutdown")
            .expect("read response after local shutdown");
        assert_eq!(response, b"pong");
        tokio::time::timeout(Duration::from_secs(1), remote)
            .await
            .expect("timed out waiting for remote half-close")
            .expect("remote task");
    }

    #[tokio::test]
    async fn tcp_read_invokes_data_received_hook() {
        let (relay, _relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        let received = Arc::new(AtomicU64::new(0));
        let hook_received = received.clone();
        let mut conn = ProxyTunnelConn::new_with_inbound_maps_and_hooks(
            "tcp-hook".into(),
            in_rx,
            router,
            vec![multi.inbound()],
            ProxyTunnelConnHooks {
                on_data_received: Some(Arc::new(move |_conn_id, bytes| {
                    hook_received.fetch_add(bytes, Ordering::SeqCst);
                })),
                ..ProxyTunnelConnHooks::default()
            },
        );

        in_tx
            .send(Bytes::from_static(b"remote-data"))
            .await
            .expect("deliver remote data");
        let mut buf = vec![0; 11];
        conn.read_exact(&mut buf).await.expect("read remote data");

        assert_eq!(&buf, b"remote-data");
        assert_eq!(received.load(Ordering::SeqCst), 11);
    }

    #[tokio::test]
    async fn tcp_flush_then_drop_drains_queued_data_before_close() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        multi.inbound().insert("tcp-drop".into(), in_tx);

        let mut conn = ProxyTunnelConn::new("tcp-drop".into(), in_rx, router, multi.inbound());
        let body = vec![0x42; MAX_TCP_WRITE_CHUNK * 20];
        conn.write_all(&body).await.expect("write body");

        let mut received = 0usize;
        let flush = conn.flush();
        tokio::pin!(flush);
        loop {
            tokio::select! {
                result = &mut flush => {
                    result.expect("flush queued body");
                    break;
                }
                msg = relay_rx.recv() => {
                    let packed = msg.expect("routed message channel closed");
                    match unpack(&packed.to_bytes()).expect("decode routed message") {
                        BinaryMessage::Data { conn_id, payload } => {
                            assert_eq!(conn_id, "tcp-drop");
                            received += payload.len();
                        }
                        other => panic!("expected Data while flushing, got {other:?}"),
                    }
                }
            }
        }
        drop(conn);

        loop {
            match recv_msg(&mut relay_rx).await {
                BinaryMessage::Data { conn_id, payload } => {
                    assert_eq!(conn_id, "tcp-drop");
                    received += payload.len();
                }
                BinaryMessage::Close { conn_id } => {
                    assert_eq!(conn_id, "tcp-drop");
                    break;
                }
                other => panic!("expected Data or Close, got {other:?}"),
            }
        }

        assert_eq!(received, body.len());
        assert!(multi.inbound().get("tcp-drop").is_none());
    }

    #[tokio::test]
    async fn udp_recv_invokes_data_received_hook() {
        let (relay, _relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (udp_tx, udp_rx) = drop_oldest_channel::<Bytes>(8);
        let received = Arc::new(AtomicU64::new(0));
        let hook_received = received.clone();
        let mut datagram = ProxyTunnelDatagram::new_with_inbound_maps_and_hooks(
            "udp-hook".into(),
            udp_rx,
            router,
            vec![multi.udp_inbound()],
            ProxyTunnelDatagramHooks {
                on_data_received: Some(Arc::new(move |_conn_id, bytes| {
                    hook_received.fetch_add(bytes, Ordering::SeqCst);
                })),
                ..ProxyTunnelDatagramHooks::default()
            },
        );

        udp_tx
            .send_drop_oldest(Bytes::from_static(b"udp-data"))
            .expect("deliver udp data");
        let payload = datagram.recv().await.expect("receive udp data");

        assert_eq!(payload, Bytes::from_static(b"udp-data"));
        assert_eq!(received.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn tcp_read_preserves_leftover_bytes() {
        let (relay, _relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
        let mut conn = ProxyTunnelConn::new("tcp-read".into(), in_rx, router, multi.inbound());

        in_tx
            .send(Bytes::from_static(b"abcde"))
            .await
            .expect("send inbound payload");

        let mut first = [0u8; 3];
        conn.read_exact(&mut first).await.expect("first read");
        assert_eq!(&first, b"abc");

        let mut second = [0u8; 2];
        conn.read_exact(&mut second).await.expect("second read");
        assert_eq!(&second, b"de");
    }

    #[tokio::test]
    async fn udp_send_recv_and_close_use_router_and_map() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (udp_tx, udp_rx) = drop_oldest_channel::<Bytes>(8);
        multi.udp_inbound().insert("udp-1".into(), udp_tx.clone());
        let mut datagram =
            ProxyTunnelDatagram::new("udp-1".into(), udp_rx, router, multi.udp_inbound());

        datagram
            .try_send(Bytes::from_static(b"ping"))
            .expect("udp send");
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::UdpData { conn_id, payload } => {
                assert_eq!(conn_id, "udp-1");
                assert_eq!(payload, Bytes::from_static(b"ping"));
            }
            other => panic!("expected UdpData, got {other:?}"),
        }

        udp_tx
            .send_drop_oldest(Bytes::from_static(b"pong"))
            .expect("recv side open");
        assert_eq!(datagram.recv().await, Some(Bytes::from_static(b"pong")));

        datagram.close().await;
        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Close { conn_id } => assert_eq!(conn_id, "udp-1"),
            other => panic!("expected Close, got {other:?}"),
        }
        assert!(multi.udp_inbound().get("udp-1").is_none());
    }

    #[tokio::test]
    async fn udp_split_receiver_drop_routes_close_and_cleans_map() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());
        let (udp_tx, udp_rx) = drop_oldest_channel::<Bytes>(8);
        multi.udp_inbound().insert("udp-split".into(), udp_tx);
        let datagram =
            ProxyTunnelDatagram::new("udp-split".into(), udp_rx, router, multi.udp_inbound());

        let (_sender, receiver) = datagram.split();
        drop(receiver);

        match recv_msg(&mut relay_rx).await {
            BinaryMessage::Close { conn_id } => assert_eq!(conn_id, "udp-split"),
            other => panic!("expected Close, got {other:?}"),
        }
        assert!(multi.udp_inbound().get("udp-split").is_none());
    }

    #[tokio::test]
    async fn high_rate_udp_association_does_not_evict_unrelated_association() {
        let (relay, mut dg_out_rx) = datagram_session(2, 8);
        let multi = make_multi(relay);
        let router = MultiSenderRouter::new(multi.clone());

        let (_noisy_tx, noisy_rx) = drop_oldest_channel::<Bytes>(8);
        let (_quiet_tx, quiet_rx) = drop_oldest_channel::<Bytes>(8);
        let noisy = ProxyTunnelDatagram::new(
            "udp-noisy".into(),
            noisy_rx,
            router.clone(),
            multi.udp_inbound(),
        );
        let quiet =
            ProxyTunnelDatagram::new("udp-quiet".into(), quiet_rx, router, multi.udp_inbound());

        noisy
            .try_send(Bytes::from_static(b"n1"))
            .expect("send noisy 1");
        quiet
            .try_send(Bytes::from_static(b"q1"))
            .expect("send quiet");
        noisy
            .try_send(Bytes::from_static(b"n2"))
            .expect("send noisy 2");
        noisy
            .try_send(Bytes::from_static(b"n3"))
            .expect("send noisy 3");

        let mut seen = Vec::new();
        for _ in 0..3 {
            let frame =
                tokio::time::timeout(Duration::from_secs(1), dg_out_rx.recv_with_quantum(1452))
                    .await
                    .expect("timed out waiting for scheduled datagram")
                    .expect("scheduler closed");
            let BinaryMessage::UdpData { conn_id, payload } =
                unpack(&frame.packed.to_bytes()).expect("decode datagram")
            else {
                panic!("expected UdpData");
            };
            seen.push((conn_id, payload));
        }

        assert!(seen.contains(&("udp-quiet".to_string(), Bytes::from_static(b"q1"))));
        assert!(!seen.contains(&("udp-noisy".to_string(), Bytes::from_static(b"n1"))));
        assert!(seen.contains(&("udp-noisy".to_string(), Bytes::from_static(b"n2"))));
        assert!(seen.contains(&("udp-noisy".to_string(), Bytes::from_static(b"n3"))));
    }

    #[tokio::test]
    async fn close_clears_udp_association_queues_on_all_paths() {
        let (relay, mut relay_rx) = datagram_session(8, 16);
        let (p2p, mut p2p_rx) = datagram_session(8, 16);
        let multi = make_multi(relay);
        multi.set_p2p(Some(p2p.clone()));
        let relay_router = MultiSenderRouter::new_relay_only(multi.clone());
        let p2p_router = MultiSenderRouter::new_pinned_p2p(multi, p2p);
        let conn_id = "udp-stale".to_string();

        relay_router
            .try_send(BinaryMessage::UdpData {
                conn_id: conn_id.clone(),
                payload: Bytes::from_static(b"relay"),
            })
            .expect("relay enqueue");
        p2p_router
            .try_send(BinaryMessage::UdpData {
                conn_id: conn_id.clone(),
                payload: Bytes::from_static(b"p2p"),
            })
            .expect("p2p enqueue");

        let _ = relay_router.try_send(BinaryMessage::Close { conn_id });

        assert!(
            tokio::time::timeout(Duration::from_millis(50), relay_rx.recv_with_quantum(1452))
                .await
                .is_err(),
            "relay queued UDP for closed association must be purged"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), p2p_rx.recv_with_quantum(1452))
                .await
                .is_err(),
            "p2p queued UDP for closed association must be purged"
        );
    }
}
