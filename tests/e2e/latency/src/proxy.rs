//! Thin SOCKS5 / direct transport wrappers shared across Phase 2 tests.
//!
//! The Phase 1 crate's no-auth SOCKS5 helpers and `socks5_udp::*` codec do all
//! the heavy lifting — this module's job is to bundle the per-test
//! "open a UDP-ASSOCIATE channel and hand back a wrapped sender" pattern
//! once instead of replicating it in every Phase 2 test body.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::{TcpStream, UdpSocket};

use tp_e2e_p1_connectivity::socks5::write_greeting_no_auth;
use tp_e2e_p1_connectivity::socks5_udp::{
    decode_udp_reply, encode_udp_request, parse_ipv4, read_associate_reply,
    write_udp_associate_request, ReplyHost,
};

/// Default round-trip timeout for individual UDP samples. Loopback should
/// reply in well under 50 ms; 5 s lets us flag a stuck gateway without
/// being so tight that a brief scheduler hiccup looks like a loss.
pub const UDP_RTT_TIMEOUT: Duration = Duration::from_secs(5);

/// A SOCKS5 UDP-ASSOCIATE session: TCP control channel kept alive on
/// `_tcp`, paired with a bound `udp` socket pre-resolved to the
/// gateway's BND.ADDR/BND.PORT. The control channel is held in the
/// struct so tests don't need to keep a separate handle alive — when
/// the session drops, the gateway tears down the UDP mapping per
/// RFC 1928 §6.
pub struct Socks5UdpSession {
    _tcp: TcpStream,
    udp: UdpSocket,
    bnd_sockaddr: String,
    target_v4: [u8; 4],
    target_port: u16,
}

impl Socks5UdpSession {
    /// Open a SOCKS5 UDP-ASSOCIATE session for `target` (must be an
    /// IPv4 literal — the echo target is loopback so this is fine).
    pub async fn open(proxy_addr: &str, target: &str) -> Result<Self> {
        let (target_host, target_port) = tp_e2e_p1_connectivity::parse_host_port(target)?;
        let target_v4 = parse_ipv4(&target_host)
            .with_context(|| format!("UDP target must be IPv4 literal, got {target_host:?}"))?;

        let mut tcp = TcpStream::connect(proxy_addr)
            .await
            .with_context(|| format!("dial SOCKS5 proxy {proxy_addr}"))?;
        write_greeting_no_auth(&mut tcp).await?;
        write_udp_associate_request(&mut tcp).await?;
        let bnd = read_associate_reply(&mut tcp)
            .await
            .context("read UDP ASSOCIATE reply")?;
        let bnd_v4 = match bnd.host {
            ReplyHost::V4([0, 0, 0, 0]) => {
                // Per RFC 1928, BND.ADDR=0.0.0.0 means "use the address you
                // connected to over TCP". Fall back to the proxy IP.
                let (proxy_host, _) = tp_e2e_p1_connectivity::parse_host_port(proxy_addr)?;
                parse_ipv4(&proxy_host).with_context(|| {
                    format!("BND=0.0.0.0 fallback needs IPv4 proxy, got {proxy_host:?}")
                })?
            }
            ReplyHost::V4(v) => v,
            ReplyHost::Other(kind) => {
                bail!("UDP ASSOCIATE reply non-IPv4 BND.ADDR (ATYP={kind})")
            }
        };
        let bnd_sockaddr = format!(
            "{}.{}.{}.{}:{}",
            bnd_v4[0], bnd_v4[1], bnd_v4[2], bnd_v4[3], bnd.port
        );
        let udp = UdpSocket::bind("127.0.0.1:0")
            .await
            .context("bind local UDP socket for SOCKS5 UDP-ASSOC")?;
        Ok(Self {
            _tcp: tcp,
            udp,
            bnd_sockaddr,
            target_v4,
            target_port,
        })
    }

    /// Send a SOCKS5-wrapped UDP datagram to the configured target.
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        let pkt = encode_udp_request(self.target_v4, self.target_port, data);
        self.udp
            .send_to(&pkt, &self.bnd_sockaddr)
            .await
            .with_context(|| format!("send wrapped UDP via gateway BND {}", self.bnd_sockaddr))?;
        Ok(())
    }

    /// Receive a single wrapped reply, unwrap, and return the inner DATA.
    /// `timeout` bounds how long we wait — caller controls; pass
    /// `UDP_RTT_TIMEOUT` for the default loopback-friendly value.
    pub async fn recv(&self, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 64 * 1024];
        let recv = tokio::time::timeout(timeout, self.udp.recv_from(&mut buf))
            .await
            .map_err(|_| anyhow!("UDP recv timeout after {:?}", timeout))?
            .context("recv wrapped UDP reply")?;
        let n = recv.0;
        decode_udp_reply(&buf[..n])
    }
}

/// Direct UDP socket pre-connected to `target` (IPv4 literal). Used by
/// the baseline test as the "no proxy" oracle for proxy-vs-direct
/// percentile deltas.
pub struct DirectUdp {
    socket: UdpSocket,
}

impl DirectUdp {
    pub async fn connect(target: &str) -> Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .context("bind direct UDP socket")?;
        socket
            .connect(target)
            .await
            .with_context(|| format!("connect direct UDP to {target}"))?;
        Ok(Self { socket })
    }

    pub async fn send(&self, data: &[u8]) -> Result<()> {
        self.socket.send(data).await.context("send direct UDP")?;
        Ok(())
    }

    pub async fn recv(&self, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 64 * 1024];
        let n = tokio::time::timeout(timeout, self.socket.recv(&mut buf))
            .await
            .map_err(|_| anyhow!("direct UDP recv timeout after {:?}", timeout))?
            .context("recv direct UDP")?;
        Ok(buf[..n].to_vec())
    }
}

/// Build a deterministic byte payload of the given length. Used by every
/// shape (TCP-128B, UDP-400B, UDP-1400B) so the inner bytes are stable
/// run-over-run for any future per-byte digest correlation.
pub fn fill_payload(len: usize, seed: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len);
    for i in 0..len {
        // A simple LCG-flavored byte pattern keyed on the seed so different
        // shapes produce visibly different bytes in any captured pcap.
        buf.push(seed.wrapping_add((i as u8).wrapping_mul(31)));
    }
    buf
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn fill_payload_is_deterministic() {
        let a = fill_payload(128, 7);
        let b = fill_payload(128, 7);
        assert_eq!(a, b);
        assert_eq!(a.len(), 128);
    }

    #[test]
    fn fill_payload_varies_by_seed() {
        let a = fill_payload(64, 1);
        let b = fill_payload(64, 2);
        assert_ne!(a, b);
    }
}
