//! Shared SOCKS5 UDP-ASSOCIATE session helper for Phase 3 tests.
//!
//! Same shape as the latency crate's `proxy::Socks5UdpSession` — we
//! deliberately copy-localize it rather than path-dep on
//! `tp-e2e-p2-latency`, so the throughput crate stands on Phase 1 only
//! and changes to the latency crate don't accidentally invalidate
//! Phase 3 builds.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use tp_e2e_p1_connectivity::socks5::{connect_no_auth, write_greeting_no_auth, Socks5Connect};
use tp_e2e_p1_connectivity::socks5_udp::{
    decode_udp_reply, encode_udp_request, parse_ipv4, read_associate_reply,
    write_udp_associate_request_with_source, ReplyHost,
};

/// SOCKS5 UDP-ASSOCIATE session: TCP control held in `_tcp` so the
/// gateway tears down the UDP mapping when this struct drops, paired
/// with a bound `udp` socket pre-resolved to the gateway's BND.
pub struct Socks5UdpSession {
    _tcp: TcpStream,
    udp: UdpSocket,
    bnd_sockaddr: String,
    target_v4: [u8; 4],
    target_port: u16,
}

impl Socks5UdpSession {
    pub async fn open(proxy_addr: &str, target: &str) -> Result<Self> {
        let (target_host, target_port) = tp_e2e_p1_connectivity::parse_host_port(target)?;
        let target_v4 = parse_ipv4(&target_host)
            .with_context(|| format!("UDP target must be IPv4 literal, got {target_host:?}"))?;

        let udp = UdpSocket::bind("127.0.0.1:0")
            .await
            .context("bind local UDP socket")?;
        let udp_source = match udp.local_addr().context("read local UDP source")? {
            SocketAddr::V4(source) => source,
            SocketAddr::V6(_) => bail!("throughput UDP source must be IPv4"),
        };

        let mut tcp = TcpStream::connect(proxy_addr)
            .await
            .with_context(|| format!("dial SOCKS5 proxy {proxy_addr}"))?;
        write_greeting_no_auth(&mut tcp).await?;
        write_udp_associate_request_with_source(&mut tcp, udp_source).await?;
        let bnd = read_associate_reply(&mut tcp)
            .await
            .context("read UDP ASSOCIATE reply")?;
        let bnd_v4 = match bnd.host {
            ReplyHost::V4([0, 0, 0, 0]) => {
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
        Ok(Self {
            _tcp: tcp,
            udp,
            bnd_sockaddr,
            target_v4,
            target_port,
        })
    }

    pub async fn send(&self, data: &[u8]) -> Result<()> {
        let pkt = encode_udp_request(self.target_v4, self.target_port, data);
        self.udp
            .send_to(&pkt, &self.bnd_sockaddr)
            .await
            .with_context(|| format!("send wrapped UDP via gateway BND {}", self.bnd_sockaddr))?;
        Ok(())
    }

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

/// Connect through the mandatory loopback NO AUTH listener exposed by a
/// Lantunnel 2.0 Client.
pub async fn socks5_connect(
    proxy_addr: &str,
    target_host: &str,
    target_port: u16,
) -> Result<Socks5Connect> {
    connect_no_auth(proxy_addr, target_host, target_port).await
}

/// Length-prefix size for the in-process TCP echo channel. 4 BE bytes
/// → max 4 GiB body; we cap at 64 KiB in the echo loop to bound the
/// allocation footprint.
pub const FRAMED_LEN_PREFIX: usize = 4;

/// In-process TCP echo loop for length-prefixed frames. Reads
/// `FRAMED_LEN_PREFIX` BE-encoded body length, then exactly that many
/// body bytes, then echoes both back. Returns when the peer closes.
///
/// Used by `udp_streaming_game` for the TCP control channel: the test
/// binds a local listener, hands it to this helper, and routes a
/// SOCKS5 CONNECT through the proxy back to the listener's port.
pub async fn echo_one_framed_connection(listener: TcpListener) {
    let Ok((mut sock, _)) = listener.accept().await else {
        return;
    };
    loop {
        let mut len_buf = [0u8; FRAMED_LEN_PREFIX];
        if sock.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > 64 * 1024 {
            break;
        }
        let mut body = vec![0u8; len];
        if sock.read_exact(&mut body).await.is_err() {
            break;
        }
        if sock.write_all(&len_buf).await.is_err() {
            break;
        }
        if sock.write_all(&body).await.is_err() {
            break;
        }
    }
}
