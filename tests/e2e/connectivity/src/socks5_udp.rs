//! SOCKS5 UDP-ASSOCIATE codec helpers (RFC 1928 §7).
//!
//! Used by `tests::socks5_udp_associate`. Lives in its own module so the
//! main `socks5.rs` (CONNECT-only) stays small.

use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::socks5::{ATYP_DOMAIN, ATYP_IPV4, ATYP_IPV6, REP_SUCCEEDED, VER_SOCKS5};

/// Encode RFC 1928 §7 UDP request: RSV[2] + FRAG[1] + ATYP[1] + DST.ADDR +
/// DST.PORT + DATA. We always use ATYP=IPv4 because the echo target is a
/// loopback IP literal.
pub fn encode_udp_request(target: [u8; 4], port: u16, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10 + data.len());
    buf.extend_from_slice(&[0x00, 0x00]); // RSV
    buf.push(0x00); // FRAG (no fragmentation)
    buf.push(ATYP_IPV4);
    buf.extend_from_slice(&target);
    buf.extend_from_slice(&port.to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Decode SOCKS5 UDP reply, returning just the inner DATA (gateway → us).
pub fn decode_udp_reply(pkt: &[u8]) -> Result<Vec<u8>> {
    if pkt.len() < 10 {
        bail!("SOCKS5 UDP reply too short: {} bytes", pkt.len());
    }
    if pkt[0] != 0x00 || pkt[1] != 0x00 {
        bail!(
            "SOCKS5 UDP reply RSV != 0x0000: {:#x}{:02x}",
            pkt[0],
            pkt[1]
        );
    }
    if pkt[2] != 0x00 {
        bail!("SOCKS5 UDP reply FRAG != 0: {:#x}", pkt[2]);
    }
    let header_len = match pkt[3] {
        ATYP_IPV4 => 4 + 6,  // header + 4 (addr) + 2 (port)
        ATYP_IPV6 => 4 + 18, // header + 16 + 2
        ATYP_DOMAIN => {
            let len = *pkt.get(4).ok_or_else(|| anyhow!("truncated DOMAIN len"))? as usize;
            4 + 1 + len + 2
        }
        atyp => bail!("unknown ATYP {:#x} in SOCKS5 UDP reply", atyp),
    };
    if pkt.len() < header_len {
        bail!(
            "SOCKS5 UDP reply truncated: header_len={header_len} got={}",
            pkt.len()
        );
    }
    Ok(pkt[header_len..].to_vec())
}

/// What we extracted from the SOCKS5 UDP-ASSOCIATE reply. We only ever talk
/// to a loopback IPv4 echo target, so the test bails on non-V4. The parser
/// still drains those bytes off the wire so the TCP control channel stays
/// in sync.
#[derive(Debug)]
pub enum ReplyHost {
    V4([u8; 4]),
    /// IPv6/DOMAIN are wire-legal but unsupported by the test — we record
    /// the variant tag so error reporting can name what the gateway returned.
    Other(&'static str),
}

#[derive(Debug)]
pub struct AssociateReply {
    pub host: ReplyHost,
    pub port: u16,
}

/// Read a SOCKS5 control-channel reply (after CONNECT or UDP ASSOCIATE).
pub async fn read_associate_reply(tcp: &mut TcpStream) -> Result<AssociateReply> {
    let mut head = [0u8; 4];
    tcp.read_exact(&mut head).await.context("read reply head")?;
    if head[0] != VER_SOCKS5 {
        bail!("non-SOCKS5 reply (VER={:#x})", head[0]);
    }
    if head[1] != REP_SUCCEEDED {
        bail!("SOCKS5 reply REP={:#x} (RFC 1928 §6)", head[1]);
    }
    let host = match head[3] {
        ATYP_IPV4 => {
            let mut v4 = [0u8; 4];
            tcp.read_exact(&mut v4).await.context("read BND.ADDR v4")?;
            ReplyHost::V4(v4)
        }
        ATYP_IPV6 => {
            let mut v6 = [0u8; 16];
            tcp.read_exact(&mut v6).await.context("read BND.ADDR v6")?;
            ReplyHost::Other("IPv6")
        }
        ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            tcp.read_exact(&mut len_buf)
                .await
                .context("read BND.ADDR DOMAIN len")?;
            let mut name = vec![0u8; len_buf[0] as usize];
            tcp.read_exact(&mut name)
                .await
                .context("read BND.ADDR DOMAIN")?;
            ReplyHost::Other("DOMAIN")
        }
        atyp => bail!("unknown ATYP {:#x}", atyp),
    };
    let mut port_buf = [0u8; 2];
    tcp.read_exact(&mut port_buf)
        .await
        .context("read BND.PORT")?;
    Ok(AssociateReply {
        host,
        port: u16::from_be_bytes(port_buf),
    })
}

/// Send a UDP ASSOCIATE request (CMD=0x03, ATYP=IPv4, BND.ADDR=0.0.0.0,
/// BND.PORT=0). Caller is responsible for reading the reply.
pub async fn write_udp_associate_request(tcp: &mut TcpStream) -> Result<()> {
    write_udp_associate_request_with_source(tcp, SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await
}

/// Send a UDP ASSOCIATE request declaring the UDP source endpoint that the
/// client already bound. This lets a shared-port SOCKS server bind the control
/// channel to the exact UDP source before it returns success.
pub async fn write_udp_associate_request_with_source(
    tcp: &mut TcpStream,
    source: SocketAddrV4,
) -> Result<()> {
    use crate::socks5::{CMD_UDP_ASSOCIATE, RSV};
    let mut request = [
        VER_SOCKS5,
        CMD_UDP_ASSOCIATE,
        RSV,
        ATYP_IPV4,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    request[4..8].copy_from_slice(&source.ip().octets());
    request[8..10].copy_from_slice(&source.port().to_be_bytes());
    tcp.write_all(&request)
        .await
        .context("send UDP ASSOCIATE request")?;
    Ok(())
}

/// XOR-fold a byte slice into a u32. Mirrors
/// `tests/e2e/_fixtures/echo-services/src/udp.rs::xor_fold` so the echo
/// service's checksum-validation counters can be sanity-checked from the
/// test side.
pub fn xor_fold(payload: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for &b in payload {
        acc = acc.rotate_left(8) ^ (b as u32);
    }
    acc
}

/// Parse a dotted-quad IPv4 literal into 4 octets.
pub fn parse_ipv4(s: &str) -> Result<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut iter = s.split('.');
    for slot in &mut octets {
        let part = iter
            .next()
            .ok_or_else(|| anyhow!("not an IPv4 dotted quad: {s:?}"))?;
        *slot = part
            .parse::<u8>()
            .with_context(|| format!("invalid octet {part:?} in {s:?}"))?;
    }
    if iter.next().is_some() {
        bail!("too many octets in {s:?}");
    }
    Ok(octets)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_codec() {
        let payload = b"hello-udp";
        let wrapped = encode_udp_request([127, 0, 0, 1], 18997, payload);
        let inner = decode_udp_reply(&wrapped).unwrap();
        assert_eq!(inner, payload);
    }

    #[test]
    fn xor_fold_matches_echo_service() {
        // Same vectors as echo-services::udp::tests.
        assert_eq!(xor_fold(&[]), 0);
        assert_eq!(xor_fold(&[0xab]), 0x0000_00ab);
        assert_eq!(xor_fold(&[0xab, 0xcd, 0xef, 0x01]), 0xabcd_ef01);
    }

    #[test]
    fn parse_ipv4_accepts_dotted_quad() {
        assert_eq!(parse_ipv4("127.0.0.1").unwrap(), [127, 0, 0, 1]);
    }

    #[test]
    fn parse_ipv4_rejects_garbage() {
        assert!(parse_ipv4("not-an-ip").is_err());
        assert!(parse_ipv4("1.2.3").is_err());
        assert!(parse_ipv4("1.2.3.4.5").is_err());
    }

    #[test]
    fn decode_rejects_short_packet() {
        assert!(decode_udp_reply(&[0; 5]).is_err());
    }

    #[test]
    fn decode_rejects_nonzero_rsv() {
        let mut pkt = vec![0x01, 0x00, 0x00, ATYP_IPV4, 1, 2, 3, 4, 0, 80, b'x'];
        assert!(decode_udp_reply(&pkt).is_err());
        pkt[0] = 0x00;
        pkt[1] = 0x01;
        assert!(decode_udp_reply(&pkt).is_err());
    }
}
