//! UDP hole-punching primitives. Initiator side sends a burst of P2pProbe
//! frames; responder (Task 3.4) replies with P2pProbeAck. The first
//! successful round-trip pins the chosen remote endpoint for QUIC handshake.

use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;
use tokio::net::UdpSocket;
use tp_core::p2p_types::SessionId;
use tp_core::protocol::{pack, unpack, BinaryMessage};

#[derive(Debug, Clone)]
pub struct BurstParams {
    pub candidates: Vec<SocketAddr>,
    pub port_offsets: Vec<i8>,
    pub burst_count: u8,
    pub gap: Duration,
    pub session_id: SessionId,
}

impl BurstParams {
    pub fn expected_total(&self) -> u32 {
        (self.candidates.len() as u32)
            * (self.port_offsets.len() as u32)
            * (self.burst_count as u32)
    }
}

/// Send `burst_count` rounds of P2pProbe frames to every (candidate × port_offset)
/// destination, with `gap` between rounds. A single unreachable candidate must
/// not abort the whole burst: mobile networks commonly expose IPv6 candidates
/// that are not reachable from the current route, while later IPv4/LAN
/// candidates may still work.
pub async fn send_burst(sock: &UdpSocket, params: &BurstParams) -> std::io::Result<()> {
    debug_assert!(
        params.candidates.is_empty() || burst_candidate_family(&params.candidates).is_some(),
        "P2P probe burst must not mix IPv4 and IPv6 candidates"
    );
    let mut seq: u32 = 0;
    let mut sent = 0usize;
    let mut attempted = 0usize;
    let mut last_err = None;
    let local_addr = sock.local_addr().ok();
    for _round in 0..params.burst_count {
        for cand in &params.candidates {
            for off in &params.port_offsets {
                attempted += 1;
                let dst = adjust_port(*cand, *off);
                let probe = BinaryMessage::P2pProbe {
                    session_id: params.session_id,
                    seq,
                    sent_ms: now_ms(),
                };
                let bytes = pack(&probe).to_bytes();
                match sock.send_to(&bytes, dst).await {
                    Ok(_) => {
                        sent += 1;
                        tracing::info!(
                            session_id = ?params.session_id,
                            seq,
                            family = if dst.is_ipv6() { "ipv6" } else { "ipv4" },
                            target = %dst,
                            local_addr = ?local_addr,
                            "p2p probe sent"
                        );
                    }
                    Err(e) => {
                        tracing::debug!(session_id = ?params.session_id, %dst, error = %e, "p2p probe send failed; continuing");
                        last_err = Some(e);
                    }
                }
                seq = seq.wrapping_add(1);
            }
        }
        tokio::time::sleep(params.gap).await;
    }
    if sent > 0 || attempted == 0 {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| std::io::Error::other("p2p burst had no sends")))
    }
}

fn adjust_port(addr: SocketAddr, off: i8) -> SocketAddr {
    let new_port = (addr.port() as i32).saturating_add(off as i32);
    let port = new_port.clamp(1, u16::MAX as i32) as u16;
    let mut a = addr;
    a.set_port(port);
    a
}

fn burst_candidate_family(candidates: &[SocketAddr]) -> Option<bool> {
    let mut ipv6 = None;
    for candidate in candidates {
        match ipv6 {
            Some(existing) if existing != candidate.is_ipv6() => return None,
            Some(_) => {}
            None => ipv6 = Some(candidate.is_ipv6()),
        }
    }
    ipv6
}

fn now_ms() -> i64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Error)]
pub enum PunchError {
    #[error("timeout waiting for ProbeAck")]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Receive loop: returns the source address of the first `P2pProbeAck` whose
/// `session_id` matches `expected`. Non-matching frames (different session,
/// malformed, other message types) are ignored. Returns `PunchError::Timeout`
/// if no matching ack arrives within `timeout`.
pub async fn wait_first_ack(
    sock: &UdpSocket,
    expected: SessionId,
    timeout: Duration,
) -> Result<SocketAddr, PunchError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    let local_addr = sock.local_addr().ok();
    let mut received_probe_while_waiting = false;
    let mut received_probe_count: u32 = 0;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::debug!(
                session_id = ?expected,
                local_addr = ?local_addr,
                received_probe_while_waiting,
                received_probe_count,
                "p2p probe ack wait timed out"
            );
            return Err(PunchError::Timeout);
        }
        let remain = deadline - now;
        match tokio::time::timeout(remain, sock.recv_from(&mut buf)).await {
            Err(_) => {
                tracing::debug!(
                    session_id = ?expected,
                    local_addr = ?local_addr,
                    received_probe_while_waiting,
                    received_probe_count,
                    "p2p probe ack wait timed out"
                );
                return Err(PunchError::Timeout);
            }
            Ok(Ok((n, src))) => {
                if let Ok(parsed) = unpack(&buf[..n]) {
                    match parsed {
                        BinaryMessage::P2pProbeAck {
                            session_id, seq, ..
                        } if session_id == expected => {
                            tracing::info!(
                                ?session_id,
                                seq,
                                family = if src.is_ipv6() { "ipv6" } else { "ipv4" },
                                source = %src,
                                local_addr = ?local_addr,
                                "p2p probe ack received"
                            );
                            return Ok(src);
                        }
                        BinaryMessage::P2pProbe {
                            session_id, seq, ..
                        } if session_id == expected => {
                            received_probe_while_waiting = true;
                            received_probe_count = received_probe_count.saturating_add(1);
                            tracing::info!(
                                ?session_id,
                                seq,
                                family = if src.is_ipv6() { "ipv6" } else { "ipv4" },
                                source = %src,
                                local_addr = ?local_addr,
                                "p2p probe received while waiting for ack"
                            );
                        }
                        _ => {}
                    }
                }
                // Ignore non-matching frames (different session, malformed, etc.)
            }
            Ok(Err(e)) => return Err(PunchError::Io(e)),
        }
    }
}

/// Send a `P2pProbeAck` echoing back `session_id` and `seq` from the given
/// `P2pProbe`. Silently no-ops if `parsed` is not a `P2pProbe`.
pub async fn answer_probe(
    sock: &UdpSocket,
    src: SocketAddr,
    parsed: &BinaryMessage,
) -> std::io::Result<()> {
    if let BinaryMessage::P2pProbe {
        session_id, seq, ..
    } = parsed
    {
        let ack = BinaryMessage::P2pProbeAck {
            session_id: *session_id,
            seq: *seq,
            recv_ms: now_ms(),
        };
        let local_addr = sock.local_addr().ok();
        sock.send_to(&pack(&ack).to_bytes(), src).await?;
        tracing::info!(
            ?session_id,
            seq,
            family = if src.is_ipv6() { "ipv6" } else { "ipv4" },
            destination = %src,
            local_addr = ?local_addr,
            "p2p probe ack sent"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;
    use tp_core::p2p_types::SessionId;

    #[tokio::test]
    async fn burst_sends_expected_packet_count() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let session_id = SessionId::from_bytes([7u8; 16]);

        let burst = BurstParams {
            candidates: vec![listen_addr],
            port_offsets: vec![0],
            burst_count: 5,
            gap: std::time::Duration::from_millis(5),
            session_id,
        };
        assert_eq!(burst.expected_total(), 5);

        tokio::spawn(async move {
            send_burst(&sender, &burst).await.unwrap();
        });

        let mut buf = [0u8; 1500];
        let mut received = 0;
        while received < 5 {
            tokio::select! {
                Ok((_n, _src)) = listener.recv_from(&mut buf) => received += 1,
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => break,
            }
        }
        assert_eq!(received, 5);
    }

    #[tokio::test]
    async fn burst_continues_after_unreachable_candidate() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let session_id = SessionId::from_bytes([8u8; 16]);

        let burst = BurstParams {
            candidates: vec!["198.51.100.1:9".parse().unwrap(), listen_addr],
            port_offsets: vec![0],
            burst_count: 1,
            gap: std::time::Duration::from_millis(1),
            session_id,
        };

        send_burst(&sender, &burst)
            .await
            .expect("one unreachable candidate must not abort the whole burst");

        let mut buf = [0u8; 1500];
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            listener.recv_from(&mut buf),
        )
        .await
        .expect("reachable candidate should still receive a probe")
        .expect("listener recv should succeed");
    }

    #[tokio::test]
    async fn wait_first_ack_returns_remote_on_first_match() {
        let recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv.local_addr().unwrap();
        let session_id = SessionId::from_bytes([3u8; 16]);

        tokio::spawn(async move {
            let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let ack = BinaryMessage::P2pProbeAck {
                session_id,
                seq: 1,
                recv_ms: 0,
            };
            let bytes = pack(&ack).to_bytes();
            s.send_to(&bytes, recv_addr).await.unwrap();
        });

        let r = wait_first_ack(&recv, session_id, std::time::Duration::from_secs(2)).await;
        assert!(r.is_ok(), "got {:?}", r);
    }

    #[tokio::test]
    async fn wait_first_ack_times_out_with_no_traffic() {
        let recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let session_id = SessionId::from_bytes([4u8; 16]);
        let r = wait_first_ack(&recv, session_id, std::time::Duration::from_millis(150)).await;
        assert!(matches!(r, Err(PunchError::Timeout)));
    }

    #[tokio::test]
    async fn answer_probe_emits_ack_with_same_session_seq() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let session_id = SessionId::from_bytes([5u8; 16]);

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let probe = BinaryMessage::P2pProbe {
            session_id,
            seq: 99,
            sent_ms: 0,
        };
        sender
            .send_to(&pack(&probe).to_bytes(), listener_addr)
            .await
            .unwrap();

        let mut buf = [0u8; 1500];
        let (n, src) = listener.recv_from(&mut buf).await.unwrap();
        let parsed = unpack(&buf[..n]).unwrap();
        answer_probe(&listener, src, &parsed).await.unwrap();

        let mut ack_buf = [0u8; 1500];
        let (m, _) = sender.recv_from(&mut ack_buf).await.unwrap();
        let parsed_ack = unpack(&ack_buf[..m]).unwrap();
        match parsed_ack {
            BinaryMessage::P2pProbeAck {
                session_id: sid,
                seq,
                ..
            } => {
                assert_eq!(sid, session_id);
                assert_eq!(seq, 99);
            }
            _ => panic!("expected ProbeAck"),
        }
    }

    #[test]
    fn burst_candidate_family_rejects_mixed_ipv4_ipv6() {
        let candidates = vec![
            "8.8.8.8:50000".parse().unwrap(),
            "[2606:4700:4700::1111]:50001".parse().unwrap(),
        ];

        assert_eq!(burst_candidate_family(&candidates), None);
    }
}
