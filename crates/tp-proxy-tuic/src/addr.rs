//! TUIC address codec and the `Addr` enum.
//!
//! Split out of `lib.rs`. Owns
//! everything that reads or writes the `[ATYP][ADDR_BYTES][PORT:u16_be]`
//! field plus the tiny `build_packet_datagram` encoder that uses the
//! same address layout inline.

use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::bail;
use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{CMD_PACKET, TUIC_VER};

/// TUIC v5 address-type tags.
pub(crate) const ADDR_NONE: u8 = 0xFF;
pub(crate) const ADDR_DOMAIN: u8 = 0x00;
pub(crate) const ADDR_V4: u8 = 0x01;
pub(crate) const ADDR_V6: u8 = 0x02;

/// Parsed TUIC address.
pub(crate) enum Addr {
    Domain(String, u16),
    V4(Ipv4Addr, u16),
    V6(Ipv6Addr, u16),
    None,
}

/// Serialize the TUIC address encoding for a `"host:port"` string.
/// Shape: `[ATYP][ADDR_BYTES][PORT:u16_be]`.
pub(crate) fn encode_addr_bytes(from: &str) -> Vec<u8> {
    let (host, port) = match from.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(0)),
        None => (from, 0u16),
    };
    let mut out = Vec::with_capacity(1 + 16 + 2);
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        out.push(ADDR_V4);
        out.extend_from_slice(&v4.octets());
        out.extend_from_slice(&port.to_be_bytes());
    } else if let Ok(v6) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<Ipv6Addr>()
    {
        out.push(ADDR_V6);
        out.extend_from_slice(&v6.octets());
        out.extend_from_slice(&port.to_be_bytes());
    } else {
        let hb = host.as_bytes();
        out.push(ADDR_DOMAIN);
        out.push(hb.len() as u8);
        out.extend_from_slice(hb);
        out.extend_from_slice(&port.to_be_bytes());
    }
    out
}

pub(crate) fn read_addr_sync(b: &mut &[u8]) -> anyhow::Result<Addr> {
    if b.is_empty() {
        bail!("empty address");
    }
    let ty = b.get_u8();
    match ty {
        ADDR_NONE => Ok(Addr::None),
        ADDR_DOMAIN => {
            if b.is_empty() {
                bail!("short domain");
            }
            let len = b.get_u8() as usize;
            if b.len() < len + 2 {
                bail!("short domain body");
            }
            let name = String::from_utf8(b[..len].to_vec())?;
            b.advance(len);
            let port = b.get_u16();
            Ok(Addr::Domain(name, port))
        }
        ADDR_V4 => {
            if b.len() < 6 {
                bail!("short v4");
            }
            let mut a = [0u8; 4];
            a.copy_from_slice(&b[..4]);
            b.advance(4);
            let port = b.get_u16();
            Ok(Addr::V4(Ipv4Addr::from(a), port))
        }
        ADDR_V6 => {
            if b.len() < 18 {
                bail!("short v6");
            }
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[..16]);
            b.advance(16);
            let port = b.get_u16();
            Ok(Addr::V6(Ipv6Addr::from(a), port))
        }
        other => bail!("bad address type: {other:#x}"),
    }
}

pub(crate) async fn read_addr(recv: &mut quinn::RecvStream) -> anyhow::Result<Addr> {
    let mut ty = [0u8; 1];
    recv.read_exact(&mut ty).await?;
    match ty[0] {
        ADDR_NONE => Ok(Addr::None),
        ADDR_DOMAIN => {
            let mut len_b = [0u8; 1];
            recv.read_exact(&mut len_b).await?;
            let mut name = vec![0u8; len_b[0] as usize];
            recv.read_exact(&mut name).await?;
            let mut port_b = [0u8; 2];
            recv.read_exact(&mut port_b).await?;
            Ok(Addr::Domain(
                String::from_utf8(name)?,
                u16::from_be_bytes(port_b),
            ))
        }
        ADDR_V4 => {
            let mut b = [0u8; 4];
            recv.read_exact(&mut b).await?;
            let mut port_b = [0u8; 2];
            recv.read_exact(&mut port_b).await?;
            Ok(Addr::V4(Ipv4Addr::from(b), u16::from_be_bytes(port_b)))
        }
        ADDR_V6 => {
            let mut b = [0u8; 16];
            recv.read_exact(&mut b).await?;
            let mut port_b = [0u8; 2];
            recv.read_exact(&mut port_b).await?;
            Ok(Addr::V6(Ipv6Addr::from(b), u16::from_be_bytes(port_b)))
        }
        other => bail!("bad TUIC address type: {other:#x}"),
    }
}

pub(crate) fn format_addr(a: &Addr) -> String {
    use std::net::IpAddr;
    match a {
        Addr::Domain(h, p) => format!("{h}:{p}"),
        Addr::V4(ip, p) => format!("{}:{}", IpAddr::V4(*ip), p),
        Addr::V6(ip, p) => {
            // sing-quic's AddressSerializer emits IPv4-mapped IPv6 addresses
            // (e.g. `::ffff:127.0.0.1`) for IPv4 targets. Unwrap to plain V4
            // so downstream UDP sockets bound to `0.0.0.0` can actually
            // `send_to` the target; otherwise Tokio returns EINVAL because
            // an IPv4 socket can't reach an IPv6 address.
            if let Some(v4) = ip.to_ipv4_mapped() {
                format!("{}:{}", IpAddr::V4(v4), p)
            } else {
                format!("[{}]:{}", IpAddr::V6(*ip), p)
            }
        }
        Addr::None => "0.0.0.0:0".into(),
    }
}

/// Build an outbound TUIC Packet datagram with a single-fragment payload
/// and source address. Used by `UdpRelayMode::QuicStream` which wraps
/// every packet in a fresh uni-stream.
pub(crate) fn build_packet_datagram(
    assoc_id: u16,
    pkt_id: u16,
    from: &str,
    payload: &[u8],
) -> Bytes {
    let mut out = BytesMut::with_capacity(payload.len() + 64);
    out.put_u8(TUIC_VER);
    out.put_u8(CMD_PACKET);
    out.put_u16(assoc_id);
    out.put_u16(pkt_id);
    out.put_u8(1); // FRAG_TOTAL
    out.put_u8(0); // FRAG_ID
    out.put_u16(payload.len() as u16);
    let (host, port) = match from.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(0)),
        None => (from, 0),
    };
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        out.put_u8(ADDR_V4);
        out.put_slice(&v4.octets());
        out.put_u16(port);
    } else if let Ok(v6) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<Ipv6Addr>()
    {
        out.put_u8(ADDR_V6);
        out.put_slice(&v6.octets());
        out.put_u16(port);
    } else {
        let hb = host.as_bytes();
        out.put_u8(ADDR_DOMAIN);
        out.put_u8(hb.len() as u8);
        out.put_slice(hb);
        out.put_u16(port);
    }
    out.put_slice(payload);
    out.freeze()
}
