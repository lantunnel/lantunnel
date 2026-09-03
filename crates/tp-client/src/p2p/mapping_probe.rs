use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

use tokio::net::UdpSocket;

const MAPPING_PROBE_ENV: &str = "TUNNEL_PROXY_P2P_MAPPING_PROBE_ADDR";
const PROBE_RESEND_INTERVAL: Duration = Duration::from_millis(100);
const SYNC_RECV_IDLE_SLEEP: Duration = Duration::from_millis(5);

pub(crate) const DEFAULT_MAPPING_PROBE_TIMEOUT: Duration = Duration::from_millis(5000);

pub(crate) fn mapping_probe_addr_from_env() -> Option<SocketAddr> {
    std::env::var(MAPPING_PROBE_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
}

/// Where to probe for this Gateway's public mapping.
///
/// `mapping_port` is what the Gateway reported through managed resolve. A host
/// whose firewall cannot expose the shared default reflects somewhere else, so
/// only fall back to the default when nothing was reported.
pub(crate) fn mapping_probe_addr_for_gateway(
    gateway_addr: SocketAddr,
    mapping_port: Option<u16>,
) -> SocketAddr {
    mapping_probe_addr_from_env().unwrap_or_else(|| {
        SocketAddr::new(
            gateway_addr.ip(),
            mapping_port.unwrap_or(tp_core::config::DEFAULT_GATEWAY_MAPPING_PROBE_PORT),
        )
    })
}

#[allow(dead_code)]
pub(crate) fn parse_observed_endpoint(buf: &[u8]) -> Option<SocketAddr> {
    let text = std::str::from_utf8(buf).ok()?;
    parse_observed_response(text).map(|(_, addr)| addr)
}

#[allow(dead_code)]
pub(crate) async fn probe_socket_public_endpoint(
    sock: &UdpSocket,
    reflector: SocketAddr,
    label: &str,
    timeout: Duration,
) -> io::Result<Option<SocketAddr>> {
    let label = sanitize_label(label);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut next_send = tokio::time::Instant::now();
    let mut seq = probe_seq_seed();
    let mut buf = [0u8; 1500];

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }

        if now >= next_send {
            let msg = format!("REG label={label} seq={seq}");
            sock.send_to(msg.as_bytes(), reflector).await?;
            seq = seq.wrapping_add(1);
            next_send = now + PROBE_RESEND_INTERVAL;
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let wait_until = next_send.min(deadline);
        let wait = wait_until.saturating_duration_since(now);
        match tokio::time::timeout(wait, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _src))) => {
                if let Some((observed_label, addr)) = std::str::from_utf8(&buf[..n])
                    .ok()
                    .and_then(parse_observed_response)
                {
                    if observed_label == label {
                        return Ok(Some(addr));
                    }
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {}
        }
    }
}

pub(crate) fn probe_std_socket_public_endpoint(
    sock: &std::net::UdpSocket,
    reflector: SocketAddr,
    label: &str,
    timeout: Duration,
) -> io::Result<Option<SocketAddr>> {
    sock.set_nonblocking(true)?;

    let label = sanitize_label(label);
    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    let mut seq = probe_seq_seed();
    let mut buf = [0u8; 1500];

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }

        if now >= next_send {
            let msg = format!("REG label={label} seq={seq}");
            sock.send_to(msg.as_bytes(), reflector)?;
            seq = seq.wrapping_add(1);
            next_send = now + PROBE_RESEND_INTERVAL;
        }

        match sock.recv_from(&mut buf) {
            Ok((n, _src)) => {
                if let Some((observed_label, addr)) = std::str::from_utf8(&buf[..n])
                    .ok()
                    .and_then(parse_observed_response)
                {
                    if observed_label == label {
                        return Ok(Some(addr));
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                let sleep_until = next_send.min(deadline);
                std::thread::sleep(
                    sleep_until
                        .saturating_duration_since(now)
                        .min(SYNC_RECV_IDLE_SLEEP),
                );
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn parse_observed_response(text: &str) -> Option<(String, SocketAddr)> {
    let mut parts = text.split_whitespace();
    if parts.next()? != "OBS" {
        return None;
    }

    let mut label = None;
    let mut ip = None;
    let mut port = None;
    for token in parts {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "label" => label = Some(value.to_string()),
            "ip" => ip = value.parse::<IpAddr>().ok(),
            "port" => port = value.parse::<u16>().ok(),
            _ => {}
        }
    }

    let ip = ip?;
    if !is_public_probe_ip(ip) {
        return None;
    }
    Some((label.unwrap_or_default(), SocketAddr::new(ip, port?)))
}

fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
        .collect();
    if cleaned.is_empty() {
        "-".to_string()
    } else {
        cleaned
    }
}

fn probe_seq_seed() -> u32 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}

fn is_public_probe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_probe_ipv4(ip),
        IpAddr::V6(ip) => is_public_probe_ipv6(ip),
    }
}

fn is_public_probe_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    let shared_carrier_nat = a == 100 && (64..=127).contains(&b);
    let benchmark = a == 198 && (b == 18 || b == 19);
    let reserved = a >= 240;
    let this_network = a == 0;

    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || shared_carrier_nat
        || benchmark
        || reserved
        || this_network)
}

fn is_public_probe_ipv6(ip: std::net::Ipv6Addr) -> bool {
    let segments = ip.segments();
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;

    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.segments()[0] & 0xfe00 == 0xfc00
        || ip.segments()[0] & 0xffc0 == 0xfe80
        || documentation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_obs_response_accepts_public_endpoint() {
        let parsed =
            parse_observed_endpoint(b"OBS label=A via=40100 ip=1.1.1.1 port=3078").unwrap();
        assert_eq!(parsed.to_string(), "1.1.1.1:3078");
    }

    #[test]
    fn parse_obs_response_rejects_private_endpoint() {
        assert!(
            parse_observed_endpoint(b"OBS label=A via=40100 ip=192.168.0.1 port=3078").is_none()
        );
    }

    #[test]
    fn mapping_probe_addr_follows_the_gateway_reported_port() {
        let gateway_addr: SocketAddr = "203.0.113.88:8443".parse().unwrap();
        // Nothing reported: the host never moved off the shared default.
        assert_eq!(
            mapping_probe_addr_for_gateway(gateway_addr, None).to_string(),
            "203.0.113.88:8444"
        );
        // Reported: probe where the Gateway says its host reflects.
        assert_eq!(
            mapping_probe_addr_for_gateway(gateway_addr, Some(10_444)).to_string(),
            "203.0.113.88:10444"
        );
    }

    #[tokio::test]
    async fn probe_socket_drains_noise_until_matching_obs() {
        let reflector = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("reflector bind");
        let reflector_addr = reflector.local_addr().expect("reflector addr");
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, src)) = reflector.recv_from(&mut buf).await {
                let text = String::from_utf8_lossy(&buf[..n]);
                let label = text
                    .split_whitespace()
                    .find_map(|token| token.strip_prefix("label="))
                    .unwrap_or("-");
                for seq in 0..16 {
                    let noise = format!("EXT label={label} seq={seq}");
                    let _ = reflector.send_to(noise.as_bytes(), src).await;
                }
                let obs = format!(
                    "OBS label={label} via={} ip=8.8.8.8 port=3078",
                    reflector_addr.port()
                );
                let _ = reflector.send_to(obs.as_bytes(), src).await;
            }
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client bind");
        let observed = probe_socket_public_endpoint(
            &client,
            reflector_addr,
            "offer:client:session",
            Duration::from_millis(500),
        )
        .await
        .expect("probe should not error")
        .expect("probe should observe public endpoint");

        assert_eq!(observed.to_string(), "8.8.8.8:3078");
    }
}
