//! Minimal SOCKS5 NO AUTH client used by the stateless V2 probes.
//!
//! References:
//!   * RFC 1928 (SOCKS Protocol Version 5)

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const VER_SOCKS5: u8 = 0x05;
pub const METHOD_NO_AUTH: u8 = 0x00;
pub const CMD_CONNECT: u8 = 0x01;
pub const CMD_UDP_ASSOCIATE: u8 = 0x03;
pub const RSV: u8 = 0x00;
pub const ATYP_IPV4: u8 = 0x01;
pub const ATYP_DOMAIN: u8 = 0x03;
pub const ATYP_IPV6: u8 = 0x04;
pub const REP_SUCCEEDED: u8 = 0x00;

/// Send a SOCKS5 greeting offering only NO AUTHENTICATION. Lantunnel 2.0
/// uses this mode exclusively on its mandatory loopback Client listener.
pub async fn write_greeting_no_auth(s: &mut TcpStream) -> Result<()> {
    s.write_all(&[VER_SOCKS5, 1, METHOD_NO_AUTH])
        .await
        .context("send SOCKS5 no-auth greeting")?;
    let mut greet = [0u8; 2];
    s.read_exact(&mut greet)
        .await
        .context("read SOCKS5 no-auth greeting reply")?;
    if greet != [VER_SOCKS5, METHOD_NO_AUTH] {
        bail!(
            "server did not select SOCKS5 NO AUTH (version={:#x}, method={:#x})",
            greet[0],
            greet[1]
        );
    }
    Ok(())
}

/// Result of a successful SOCKS5 CONNECT handshake — the underlying TCP stream
/// is positioned at the start of the proxied tunnel and ready for app-layer I/O.
pub struct Socks5Connect {
    pub stream: TcpStream,
}

/// Open a SOCKS5 CONNECT tunnel through a loopback-only NO AUTH listener.
///
/// Limits enforced (per RFC):
///   * `target_host` ≤ 255 bytes (DOMAIN ATYP only — we don't try IP literals
///     because the Client resolves DOMAIN itself, which is what we want to
///     exercise).
pub async fn connect_no_auth(
    proxy_addr: &str,
    target_host: &str,
    target_port: u16,
) -> Result<Socks5Connect> {
    let mut s = TcpStream::connect(proxy_addr)
        .await
        .with_context(|| format!("dial SOCKS5 proxy {proxy_addr}"))?;

    s.write_all(&[VER_SOCKS5, 1, METHOD_NO_AUTH])
        .await
        .context("send SOCKS5 greeting")?;
    let mut greet_resp = [0u8; 2];
    s.read_exact(&mut greet_resp)
        .await
        .context("read SOCKS5 greeting reply")?;
    if greet_resp[0] != VER_SOCKS5 {
        bail!(
            "SOCKS5 server replied with non-5 version: got {:#x}",
            greet_resp[0]
        );
    }
    if greet_resp[1] != METHOD_NO_AUTH {
        bail!(
            "SOCKS5 server did not select NO AUTH (got method {:#x})",
            greet_resp[1]
        );
    }

    // ---- CONNECT request: VER=5, CMD=1, RSV=0, ATYP=DOMAIN, DLEN, DOMAIN, PORT ----
    if target_host.len() > 255 {
        return Err(anyhow!(
            "target host too long: {} bytes (max 255)",
            target_host.len()
        ));
    }
    let mut req = Vec::with_capacity(7 + target_host.len());
    req.push(VER_SOCKS5);
    req.push(CMD_CONNECT);
    req.push(RSV);
    req.push(ATYP_DOMAIN);
    req.push(target_host.len() as u8);
    req.extend_from_slice(target_host.as_bytes());
    req.extend_from_slice(&target_port.to_be_bytes());
    s.write_all(&req).await.context("send SOCKS5 CONNECT")?;

    // ---- Reply: VER, REP, RSV, ATYP, BND.ADDR..., BND.PORT ----
    let mut reply_head = [0u8; 4];
    s.read_exact(&mut reply_head)
        .await
        .context("read SOCKS5 CONNECT reply header")?;
    if reply_head[0] != VER_SOCKS5 {
        bail!("non-SOCKS5 reply (VER={:#x})", reply_head[0]);
    }
    if reply_head[1] != REP_SUCCEEDED {
        bail!(
            "SOCKS5 CONNECT failed (REP={:#x} — see RFC 1928 §6)",
            reply_head[1]
        );
    }

    // Drain BND.ADDR by ATYP. We don't use it — the gateway's bound address
    // is irrelevant to the app-layer test — but per RFC the bytes are on the
    // wire and must be consumed before reading BND.PORT.
    match reply_head[3] {
        ATYP_IPV4 => {
            let mut x = [0u8; 4];
            s.read_exact(&mut x)
                .await
                .context("drain BND.ADDR (IPv4)")?;
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len)
                .await
                .context("read BND.ADDR domain length")?;
            let mut x = vec![0u8; len[0] as usize];
            s.read_exact(&mut x)
                .await
                .context("drain BND.ADDR (DOMAIN)")?;
        }
        ATYP_IPV6 => {
            let mut x = [0u8; 16];
            s.read_exact(&mut x)
                .await
                .context("drain BND.ADDR (IPv6)")?;
        }
        atyp => bail!("unknown ATYP {atyp:#x} in SOCKS5 reply"),
    }
    let mut bnd_port = [0u8; 2];
    s.read_exact(&mut bnd_port)
        .await
        .context("drain BND.PORT")?;

    Ok(Socks5Connect { stream: s })
}
