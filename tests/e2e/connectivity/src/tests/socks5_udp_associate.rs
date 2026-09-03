//! V2 SOCKS5 NO AUTH + UDP ASSOCIATE → echo UDP.
//!
//! Flow: TCP control channel does NO AUTH greeting + UDP ASSOCIATE (CMD=0x03),
//! the Client's reply carries (BND.ADDR, BND.PORT) for its
//! UDP listener. We then send a SOCKS5-wrapped echo datagram (codec in
//! `crate::socks5_udp`) to BND, read the reply, and assert payload survives
//! round-trip. Closing the TCP control channel tears down the UDP mapping
//! per RFC 1928 §6.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::{TcpStream, UdpSocket};

use crate::socks5::write_greeting_no_auth;
use crate::socks5_udp::{
    decode_udp_reply, encode_udp_request, parse_ipv4, read_associate_reply,
    write_udp_associate_request, xor_fold, ReplyHost,
};

/// Round-trip timeout for the UDP echo. The shared listener should reply in
/// well under 500 ms on loopback; 5 s gives generous slack for CI.
const UDP_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(proxy_addr: &str, udp_target: &str) -> Result<()> {
    let (target_host, target_port) = crate::parse_host_port(udp_target)?;
    let target_ipv4 = parse_ipv4(&target_host)
        .with_context(|| format!("UDP echo target must be an IPv4 literal, got {target_host:?}"))?;

    tracing::info!(
        proxy = %proxy_addr,
        udp_target = %udp_target,
        "SOCKS5 UDP ASSOCIATE begin"
    );

    // 1. NO AUTH greeting + UDP ASSOCIATE on the TCP control channel.
    let mut tcp = TcpStream::connect(proxy_addr)
        .await
        .with_context(|| format!("dial SOCKS5 proxy {proxy_addr}"))?;
    write_greeting_no_auth(&mut tcp).await?;
    write_udp_associate_request(&mut tcp).await?;
    let bnd = read_associate_reply(&mut tcp)
        .await
        .context("read UDP ASSOCIATE reply")?;
    let bnd_ipv4 = match bnd.host {
        ReplyHost::V4(v4) => {
            // Per RFC 1928, BND.ADDR may be 0.0.0.0 meaning "use the same
            // address you connected to over TCP". Fall back to the proxy IP.
            if v4 == [0, 0, 0, 0] {
                let (proxy_host, _) = crate::parse_host_port(proxy_addr)?;
                parse_ipv4(&proxy_host).with_context(|| {
                    format!("BND.ADDR=0.0.0.0 fallback needs IPv4 proxy, got {proxy_host:?}")
                })?
            } else {
                v4
            }
        }
        ReplyHost::Other(kind) => {
            bail!("UDP ASSOCIATE reply has non-IPv4 BND.ADDR (ATYP={kind})")
        }
    };
    let bnd_port = bnd.port;
    tracing::info!(
        bnd_addr = format!(
            "{}.{}.{}.{}",
            bnd_ipv4[0], bnd_ipv4[1], bnd_ipv4[2], bnd_ipv4[3]
        ),
        bnd_port,
        "UDP ASSOCIATE reply OK"
    );

    // 2. Bind a UDP socket and send a SOCKS5-wrapped echo datagram.
    let udp = UdpSocket::bind("127.0.0.1:0")
        .await
        .context("bind local UDP socket")?;
    let bnd_sockaddr = format!(
        "{}.{}.{}.{}:{}",
        bnd_ipv4[0], bnd_ipv4[1], bnd_ipv4[2], bnd_ipv4[3], bnd_port
    );

    // Echo payload: a V2 marker + 4-byte XOR-fold
    // checksum trailer. The echo service validates the checksum and returns
    // the full bytes unchanged.
    let payload_inner = build_payload();
    let request = encode_udp_request(target_ipv4, target_port, &payload_inner);
    udp.send_to(&request, &bnd_sockaddr)
        .await
        .with_context(|| format!("send wrapped UDP to gateway BND {bnd_sockaddr}"))?;

    // 3. Read the echoed reply and verify the unwrapped DATA matches.
    let mut buf = vec![0u8; 64 * 1024];
    let (n, _from) = tokio::time::timeout(UDP_TIMEOUT, udp.recv_from(&mut buf))
        .await
        .map_err(|_| anyhow!("timeout waiting for echoed UDP reply from gateway"))?
        .context("recv echoed UDP reply")?;
    let inner = decode_udp_reply(&buf[..n])?;
    if inner != payload_inner {
        bail!(
            "echoed payload mismatch: sent {} bytes, got {} bytes",
            payload_inner.len(),
            inner.len(),
        );
    }

    // 4. Close the TCP control channel — RFC 1928 §6.
    drop(tcp);

    tracing::info!(
        bytes = inner.len(),
        "PASS: SOCKS5 UDP ASSOCIATE echoed payload"
    );
    Ok(())
}

/// Build the echo-service payload: a small "magic" prefix + 4-byte XOR-fold
/// trailer matching `_fixtures/echo-services/src/udp.rs::xor_fold`.
fn build_payload() -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(b"udp-v2:hi");
    let cksum = xor_fold(&body);
    body.extend_from_slice(&cksum.to_be_bytes());
    body
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn payload_checksum_trailer_matches_echo_validator() {
        // Codec-level round-trip is tested in `crate::socks5_udp::tests`;
        // here we verify the test-specific build_payload helper produces a
        // 4-byte trailer that the echo service's xor_fold will validate.
        let p = build_payload();
        let body = &p[..p.len() - 4];
        let trailer = &p[p.len() - 4..];
        assert_eq!(trailer, xor_fold(body).to_be_bytes());
    }
}
