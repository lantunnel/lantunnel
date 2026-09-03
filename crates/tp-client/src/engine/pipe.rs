//! TCP and UDP pipe loops — the per-connection data-plane paths spun
//! up by [`crate::engine::Engine::handle_msg`] when it accepts a
//! `Connect` frame from the gateway.
//!
//! Split out of `engine.rs`.
//! Both functions are framework-free async I/O helpers: they hold no
//! reference to `Engine` and only interact with the session sender
//! and the per-conn inbound/UDP maps. Keeping them in a separate
//! submodule makes the deep I/O commentary easier to navigate without
//! scrolling through the engine control flow.

use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Instant as TokioInstant};
use tp_core::protocol::BinaryMessage;

use crate::p2p::multi_sender::MultiSenderRouter;

const RELAY_TAG_SIZE_V2: usize =
    crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2 - crate::relay_crypto::RELAY_NONCE_SIZE_V2;

fn prepared_record_read_limit(arena: &BytesMut) -> usize {
    arena
        .capacity()
        .saturating_sub(arena.len())
        .saturating_sub(RELAY_TAG_SIZE_V2)
        .min(crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2)
}

fn take_prepared_record(arena: &mut BytesMut, plaintext_len: usize) -> BytesMut {
    let plaintext_end = crate::relay_crypto::RELAY_NONCE_SIZE_V2 + plaintext_len;
    debug_assert_eq!(arena.len(), plaintext_end);
    let record_end = plaintext_end + RELAY_TAG_SIZE_V2;
    debug_assert!(arena.capacity() >= record_end);
    arena.resize(record_end, 0);
    let mut record = arena.split_to(record_end);
    record.truncate(plaintext_end);
    record
}

fn udp_io_error_is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
    )
}

/// Pipe bytes between a local `TcpStream` and the tunnel in both
/// directions. Exits on EOF, target-side write failure, or gateway
/// session death (`out.closed()`).
///
/// `out` is a `MultiSenderRouter` so each frame's path is picked
/// per-send by [`MultiSession::pick`]. With P2P up the router's
/// `pick()` returns the P2P session and the data plane bypasses the
/// gateway. With P2P down it returns the relay session — same code
/// path, no migration teardown. `closed()` only fires on relay loss
/// so a P2P drop doesn't kill the per-conn pipe.
pub(crate) async fn pipe_tcp(
    conn_id: String,
    stream: TcpStream,
    mut rx_in: mpsc::Receiver<Bytes>,
    out: MultiSenderRouter,
    inbound_map: Arc<DashMap<String, mpsc::Sender<Bytes>>>,
) {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let send_out = out.clone();
    let conn_up = conn_id.clone();
    let up = async move {
        // Read after the reserved nonce directly into arena storage. Each
        // split also carries hidden tag capacity so Relay can seal without
        // reallocating or moving the plaintext.
        //
        // 64 KiB arena / 16 KiB low-water is deliberately 4x the old
        // stack buffer: it lets a slow consumer hold ~3 chunks in flight
        // without forcing the next `reserve` to re-allocate, but keeps
        // the 1024-conn worst-case memory footprint bounded (~64 MiB).
        const ARENA_BYTES: usize = 64 * 1024 + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2;
        const ARENA_LOW_WATER: usize = 16 * 1024;
        let mut arena: BytesMut = BytesMut::with_capacity(ARENA_BYTES);
        loop {
            if arena.capacity() - arena.len() < ARENA_LOW_WATER {
                arena.reserve(ARENA_BYTES);
            }
            arena.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
            let read_limit = prepared_record_read_limit(&arena);
            let mut read_buf = (&mut arena).limit(read_limit);
            match rd.read_buf(&mut read_buf).await {
                Ok(0) => {
                    arena.clear();
                    let _ = send_out
                        .send(BinaryMessage::Close {
                            conn_id: conn_up.clone(),
                        })
                        .await;
                    break;
                }
                Ok(n) => {
                    // Consume the prefix and all n new bytes, while lending
                    // the record its tag spare from the same allocation.
                    debug_assert_eq!(
                        arena.len(),
                        crate::relay_crypto::RELAY_NONCE_SIZE_V2 + n,
                        "pipe_tcp arena invariant: each iter consumes all"
                    );
                    let record = take_prepared_record(&mut arena, n);
                    if send_out
                        .send_prepared_data(
                            conn_up.clone(),
                            crate::relay_crypto::RelayFramedKindV2::Data,
                            record,
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => {
                    arena.clear();
                    let _ = send_out
                        .send(BinaryMessage::Close {
                            conn_id: conn_up.clone(),
                        })
                        .await;
                    break;
                }
            }
        }
    };
    let down = async move {
        while let Some(payload) = rx_in.recv().await {
            if wr.write_all(&payload).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    // Third select branch: transport-level session death. Without it, a
    // target-idle TCP flow blocks `up` in `rd.read()` AND `down` in
    // `rx_in.recv()` forever when the QUIC connection dies (gateway crash,
    // network flap), because the gateway never delivers the `Close` that
    // would drop `tx_in` from the `inbound` DashMap. Orphaned `pipe_tcp`
    // tasks accumulating across reconnect cycles were the root cause of
    // the slow RSS growth reported on the Tauri client.
    tokio::pin!(up);
    tokio::pin!(down);
    let mut up_done = false;
    let mut down_done = false;
    loop {
        if up_done && down_done {
            break;
        }
        tokio::select! {
            _ = &mut up, if !up_done => {
                up_done = true;
            }
            _ = &mut down, if !down_done => {
                down_done = true;
            }
            _ = out.closed() => {
                break;
            }
        }
    }
    inbound_map.remove(&conn_id);
}

/// Pipe UDP datagrams between a local `UdpSocket` and the tunnel in
/// both directions. Uses a [`BytesMut`] arena for zero-copy receive and
/// `try_send` drop-on-full on the upstream leg — UDP is unreliable by
/// design, and blocking here would propagate back-pressure into the
/// kernel UDP receive queue.
///
/// `out` is a `MultiSenderRouter` so each datagram is routed via
/// `MultiSession::pick`. P2P-up flows go directly peer-to-peer; P2P-down
/// flows fall back to relay — same per-frame `try_send` path.
pub(crate) async fn pipe_udp(
    conn_id: String,
    socket: UdpSocket,
    mut rx_in: tp_transport::DropOldestReceiver<Bytes>,
    out: MultiSenderRouter,
    udp_map: Arc<DashMap<String, tp_transport::DropOldestSender<Bytes>>>,
) {
    let socket = Arc::new(socket);
    let read_sock = socket.clone();
    let conn_up = conn_id.clone();
    let out_up = out.clone();

    let up = async move {
        // Non-blocking sends on the sunshine→gateway leg. Previous code
        // used awaited tunnel sends, which back-pressures the UdpSocket
        // reader whenever the session's dg_out_tx / stream_tx fills. That
        // stall propagates into the kernel UDP receive queue and the kernel
        // silently drops — the ~10% intermittent frame-loss symptom seen
        // on bursty 4K120 video. For game streaming a late packet is a
        // dead packet; drop at the app layer instead and keep reading so
        // newer packets still arrive on time.
        //
        // Receive after the reserved nonce directly into a shared arena.
        // Each record retains hidden AEAD tag capacity for in-place Relay
        // sealing; Direct simply advances past the nonce.
        // Arena grows to 1 MiB upfront so typical moonlight video rates
        // (~2.5k pps × 1.3 KiB = ~3 MiB/s = ~300 ms of buffering at 1 MiB)
        // only reallocate when downstream holds Bytes longer than the
        // arena can churn. Once all derived Bytes drop, BytesMut::reserve
        // can reuse the freed prefix in-place without syscall.
        const ARENA_BYTES: usize = 1024 * 1024;
        const ARENA_LOW_WATER: usize = 64 * 1024;
        let mut arena: BytesMut = BytesMut::with_capacity(ARENA_BYTES);
        let mut dropped: u64 = 0;
        let mut recv_errors: u64 = 0;
        loop {
            // If we don't have room for one jumbo UDP frame, grow (or
            // re-allocate — BytesMut::reserve reuses in place when it can).
            if arena.capacity() - arena.len() < ARENA_LOW_WATER {
                arena.reserve(ARENA_BYTES);
            }
            arena.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
            let read_limit = prepared_record_read_limit(&arena);
            let mut read_buf = (&mut arena).limit(read_limit);
            match read_sock.recv_buf(&mut read_buf).await {
                Ok(n) => {
                    // The record owns the prefix, datagram, and hidden tag
                    // spare; the arena advances to the next free region.
                    let record = take_prepared_record(&mut arena, n);
                    match out_up.try_send_prepared_data(
                        conn_up.clone(),
                        crate::relay_crypto::RelayFramedKindV2::UdpData,
                        record,
                    ) {
                        Ok(()) => {}
                        Err(tp_transport::TrySendKind::Full) => {
                            dropped = dropped.wrapping_add(1);
                            if dropped.is_power_of_two() {
                                tracing::debug!(
                                    conn_id = %conn_up,
                                    dropped,
                                    "client pipe_udp: tunnel queue full; dropped UDP upstream"
                                );
                            }
                        }
                        Err(tp_transport::TrySendKind::TooLarge(len)) => {
                            tracing::warn!(
                                conn_id = %conn_up,
                                len,
                                "client pipe_udp: oversized UDP frame rejected"
                            );
                        }
                        Err(tp_transport::TrySendKind::DatagramUnavailable) => {
                            tracing::debug!(
                                conn_id = %conn_up,
                                "client pipe_udp: datagram transport unavailable"
                            );
                            break;
                        }
                        Err(tp_transport::TrySendKind::Closed) => break,
                    }
                }
                Err(e) if udp_io_error_is_transient(&e) => {
                    arena.clear();
                    recv_errors = recv_errors.wrapping_add(1);
                    if recv_errors <= 4 || recv_errors.is_power_of_two() {
                        tracing::debug!(
                            conn_id = %conn_up,
                            error = %e,
                            recv_errors,
                            "client pipe_udp: transient local UDP recv error; keeping flow open"
                        );
                    }
                    sleep(Duration::from_millis(1)).await;
                }
                Err(e) => {
                    arena.clear();
                    tracing::warn!(
                        conn_id = %conn_up,
                        error = %e,
                        "client pipe_udp: local UDP recv failed; closing flow"
                    );
                    break;
                }
            }
        }
    };

    let conn_down = conn_id.clone();
    let down = async move {
        // Gateway->local UDP is a bounded drop-oldest real-time path. If the
        // local socket cannot keep up, newer frames replace older ones upstream.
        let local_addr = socket.local_addr().ok();
        let peer_addr = socket.peer_addr().ok();
        let mut sent: u64 = 0;
        let mut bytes_sent: u64 = 0;
        let mut send_errors: u64 = 0;
        let mut would_block: u64 = 0;
        while let Some(payload) = rx_in.recv().await {
            let payload_len = payload.len();
            match socket.try_send(&payload) {
                Ok(_) => {
                    sent = sent.wrapping_add(1);
                    bytes_sent = bytes_sent.wrapping_add(payload_len as u64);
                    if sent <= 4 || sent.is_power_of_two() {
                        tracing::debug!(
                            conn_id = %conn_down,
                            sent,
                            bytes_sent,
                            payload_len,
                            ?local_addr,
                            ?peer_addr,
                            "client pipe_udp: delivered UDP downstream to local socket"
                        );
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    would_block = would_block.wrapping_add(1);
                    match socket.send(&payload).await {
                        Ok(_) => {
                            sent = sent.wrapping_add(1);
                            bytes_sent = bytes_sent.wrapping_add(payload_len as u64);
                        }
                        Err(e) if udp_io_error_is_transient(&e) => {
                            send_errors = send_errors.wrapping_add(1);
                            if send_errors <= 4 || send_errors.is_power_of_two() {
                                tracing::debug!(
                                    conn_id = %conn_down,
                                    error = %e,
                                    send_errors,
                                    ?local_addr,
                                    ?peer_addr,
                                    "client pipe_udp: transient local UDP send error; keeping flow open"
                                );
                            }
                        }
                        Err(e) => {
                            send_errors = send_errors.wrapping_add(1);
                            tracing::warn!(
                                conn_id = %conn_down,
                                error = %e,
                                send_errors,
                                ?local_addr,
                                ?peer_addr,
                                "client pipe_udp: local UDP send failed after WouldBlock"
                            );
                            break;
                        }
                    }
                }
                Err(e) if udp_io_error_is_transient(&e) => {
                    send_errors = send_errors.wrapping_add(1);
                    if send_errors <= 4 || send_errors.is_power_of_two() {
                        tracing::debug!(
                            conn_id = %conn_down,
                            error = %e,
                            send_errors,
                            ?local_addr,
                            ?peer_addr,
                            "client pipe_udp: transient local UDP send error; keeping flow open"
                        );
                    }
                }
                Err(e) => {
                    send_errors = send_errors.wrapping_add(1);
                    tracing::warn!(
                        conn_id = %conn_down,
                        error = %e,
                        send_errors,
                        ?local_addr,
                        ?peer_addr,
                        "client pipe_udp: local UDP send failed"
                    );
                    break;
                }
            }
        }
        if sent > 0 || send_errors > 0 || would_block > 0 {
            tracing::info!(
                conn_id = %conn_down,
                sent,
                bytes_sent,
                send_errors,
                would_block,
                ?local_addr,
                ?peer_addr,
                "client pipe_udp downstream summary"
            );
        }
    };

    // Same session-death signal as `pipe_tcp`: if the UDP socket is idle
    // AND the gateway never delivers `Close`/`UdpData` for this conn_id,
    // both `up` (blocked in `recv`) and `down` (blocked in `rx_in.recv()`)
    // would hang forever after QUIC teardown. `out.closed()` fires as soon
    // as the transport writer drops its receiver, so the task exits and
    // `udp_map` entry + socket are released.
    //
    // UDP data rides QUIC datagrams while Close/control rides the reliable
    // lane. They are not ordered relative to each other, so one side ending
    // must not tear the conn_id out of `udp_map` immediately; otherwise
    // late-but-valid datagrams are misclassified as "pending conn_id" and
    // dropped by the tiny pre-Connect buffer. Keep the slot alive for a
    // short drain window, then close.
    tokio::pin!(up);
    tokio::pin!(down);
    let drain = sleep(Duration::from_secs(86_400));
    tokio::pin!(drain);
    let mut up_done = false;
    let mut down_done = false;
    let mut draining = false;
    loop {
        if up_done && down_done {
            break;
        }
        tokio::select! {
            _ = &mut up, if !up_done => {
                up_done = true;
                if !draining {
                    drain
                        .as_mut()
                        .reset(TokioInstant::now() + super::UDP_CLOSE_DRAIN_GRACE);
                    draining = true;
                }
            }
            _ = &mut down, if !down_done => {
                down_done = true;
                if !draining {
                    drain
                        .as_mut()
                        .reset(TokioInstant::now() + super::UDP_CLOSE_DRAIN_GRACE);
                    draining = true;
                }
            }
            _ = &mut drain, if draining => {
                break;
            }
            _ = out.closed() => {
                break;
            }
        }
    }
    let _ = out.try_send(BinaryMessage::Close {
        conn_id: conn_id.clone(),
    });
    udp_map.remove(&conn_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::time::Duration;

    use tp_core::protocol::{unpack, PackedMessage};
    use tp_transport::session::Session;

    #[test]
    fn udp_refused_recv_error_is_transient() {
        let err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(udp_io_error_is_transient(&err));
    }

    #[test]
    fn producer_record_retains_tag_capacity_for_in_place_seal() {
        let mut maximum = BytesMut::with_capacity(
            crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2
                + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
        );
        maximum.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
        assert_eq!(
            prepared_record_read_limit(&maximum),
            crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2,
            "the reserved tag must not reduce the existing plaintext ceiling"
        );

        let payload = b"producer-owned-payload";
        let mut arena = BytesMut::with_capacity(128);
        arena.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
        arena.extend_from_slice(payload);
        let allocation = arena.as_ptr();

        let mut record = take_prepared_record(&mut arena, payload.len());
        assert_eq!(
            record.capacity() - record.len(),
            RELAY_TAG_SIZE_V2,
            "the split record must own exactly the AEAD tag spare"
        );
        record.reserve(RELAY_TAG_SIZE_V2);

        assert_eq!(
            record.as_ptr(),
            allocation,
            "reserving the AEAD tag must not replace the producer allocation"
        );
    }

    type TestRouterFixture = (
        MultiSenderRouter,
        mpsc::Receiver<PackedMessage>,
        Arc<DashMap<String, mpsc::Sender<Bytes>>>,
    );

    fn test_router() -> TestRouterFixture {
        let (relay_out_tx, relay_out_rx) = mpsc::channel::<PackedMessage>(8);
        let (_relay_in_tx, relay_in_rx) = mpsc::channel::<BinaryMessage>(1);
        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let relay = Arc::new(Session::new_channeled(
            relay_out_tx,
            relay_in_rx,
            peer,
            closer,
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        ));
        let multi = crate::p2p::session::MultiSession::new_with_relay_only(relay);
        let inbound = multi.inbound();
        (MultiSenderRouter::new(multi), relay_out_rx, inbound)
    }

    #[tokio::test]
    async fn pipe_tcp_keeps_remote_to_local_open_after_local_write_half_fin() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let client_stream = TcpStream::connect(addr).await.expect("connect pair");
        let (mut target_stream, _) = listener.accept().await.expect("accept pair");

        let (router, mut relay_rx, inbound_map) = test_router();
        let conn_id = "tcp-half-fin".to_string();
        let (tx_in, rx_in) = mpsc::channel::<Bytes>(8);
        inbound_map.insert(conn_id.clone(), tx_in.clone());

        let pipe = tokio::spawn(pipe_tcp(
            conn_id.clone(),
            client_stream,
            rx_in,
            router,
            inbound_map.clone(),
        ));

        target_stream
            .shutdown()
            .await
            .expect("target write half shutdown");

        let close = tokio::time::timeout(Duration::from_secs(1), relay_rx.recv())
            .await
            .expect("timed out waiting for close")
            .expect("relay channel closed");
        match unpack(&close.to_bytes()).expect("decode close") {
            BinaryMessage::Close { conn_id: got } => assert_eq!(got, conn_id),
            other => panic!("expected Close after local write-half FIN, got {other:?}"),
        }

        tx_in
            .send(Bytes::from_static(b"after-fin"))
            .await
            .expect("remote-to-local half must remain open after local FIN");

        let mut buf = [0u8; 9];
        tokio::time::timeout(Duration::from_secs(1), target_stream.read_exact(&mut buf))
            .await
            .expect("timed out reading post-FIN payload")
            .expect("read post-FIN payload");
        assert_eq!(&buf, b"after-fin");

        drop(tx_in);
        inbound_map.remove(&conn_id);
        tokio::time::timeout(Duration::from_secs(1), pipe)
            .await
            .expect("pipe did not exit")
            .expect("pipe task panicked");
    }

    #[tokio::test]
    async fn pipe_udp_delivers_remote_payload_to_connected_local_socket() {
        let listener = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind UDP listener");
        let target_addr = listener.local_addr().expect("listener addr");

        let pipe_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind pipe socket");
        pipe_socket.connect(target_addr).await.expect("connect UDP");

        let (router, _relay_rx, _inbound_map) = test_router();
        let udp_map = Arc::new(DashMap::new());
        let conn_id = "udp-local-delivery".to_string();
        let (tx_in, rx_in) = tp_transport::drop_oldest_channel::<Bytes>(8);
        udp_map.insert(conn_id.clone(), tx_in.clone());
        let pipe = tokio::spawn(pipe_udp(
            conn_id.clone(),
            pipe_socket,
            rx_in,
            router,
            udp_map.clone(),
        ));

        tx_in
            .send_drop_oldest(Bytes::from_static(b"hello-udp"))
            .expect("send into UDP pipe");

        let mut buf = [0u8; 32];
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), listener.recv_from(&mut buf))
            .await
            .expect("timed out waiting for UDP payload")
            .expect("recv UDP payload");
        assert_eq!(&buf[..n], b"hello-udp");

        udp_map.remove(&conn_id);
        drop(tx_in);
        tokio::time::timeout(
            crate::engine::UDP_CLOSE_DRAIN_GRACE + Duration::from_secs(1),
            pipe,
        )
        .await
        .expect("pipe did not exit")
        .expect("pipe task panicked");
    }

    #[tokio::test]
    async fn pipe_udp_keeps_flow_open_after_transient_local_recv_error() {
        let unused_target = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind unused UDP target");
        let target_addr = unused_target.local_addr().expect("unused target addr");
        drop(unused_target);

        let pipe_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind pipe socket");
        pipe_socket.connect(target_addr).await.expect("connect UDP");

        let (router, _relay_rx, _inbound_map) = test_router();
        let udp_map = Arc::new(DashMap::new());
        let conn_id = "udp-transient-refused".to_string();
        let (tx_in, rx_in) = tp_transport::drop_oldest_channel::<Bytes>(8);
        udp_map.insert(conn_id.clone(), tx_in.clone());
        let pipe = tokio::spawn(pipe_udp(
            conn_id.clone(),
            pipe_socket,
            rx_in,
            router,
            udp_map.clone(),
        ));

        tx_in
            .send_drop_oldest(Bytes::from_static(b"probe"))
            .expect("send probe into UDP pipe");
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx_in
            .send_drop_oldest(Bytes::from_static(b"probe-again"))
            .expect("send second probe into UDP pipe after ICMP error");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            udp_map.contains_key(&conn_id),
            "transient connected UDP recv errors must not remove the active tunnel slot"
        );

        udp_map.remove(&conn_id);
        drop(tx_in);
        tokio::time::timeout(
            crate::engine::UDP_CLOSE_DRAIN_GRACE + Duration::from_secs(1),
            pipe,
        )
        .await
        .expect("pipe did not exit")
        .expect("pipe task panicked");
    }
}
