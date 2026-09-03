//! Machine-wide UDP mapping service used by P2P clients to discover the public
//! endpoint of the exact UDP socket they will later reuse for hole punching.
//! Exactly one `lantunnel-gateway mapping serve` process owns this socket in
//! each OS network namespace; ordinary Gateway instances only probe it.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;

const MAX_PACKET_SIZE: usize = 1500;

pub struct MappingProbeServer {
    socket: UdpSocket,
}

impl MappingProbeServer {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(addr).await?,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn run(self) -> io::Result<()> {
        let local_addr = self.socket.local_addr()?;
        let via = local_addr.port();
        let mut buf = [0u8; MAX_PACKET_SIZE];
        loop {
            let (n, remote) = self.socket.recv_from(&mut buf).await?;
            let label = mapping_probe_label(&buf[..n]);
            let reply = format!(
                "OBS label={label} via={via} ip={} port={}",
                remote.ip(),
                remote.port()
            );
            if let Err(e) = self.socket.send_to(reply.as_bytes(), remote).await {
                tracing::warn!(error = %e, %remote, label, "UDP mapping probe reply failed");
            } else {
                tracing::debug!(%remote, label, "UDP mapping probe reply sent");
            }
        }
    }
}

/// Proves that the already-bound reflector can receive and echo one datagram
/// through the local network stack. This intentionally says nothing about
/// firewall, NAT, or public-Internet reachability.
pub async fn probe_local_readiness(addr: SocketAddr, deadline: Duration) -> io::Result<()> {
    let target_ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let target = SocketAddr::new(target_ip, addr.port());
    let bind_addr = match target {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let socket = UdpSocket::bind(bind_addr).await?;
    socket.connect(target).await?;
    let local = socket.local_addr()?;
    let request = b"REG label=readiness seq=0";
    tokio::time::timeout(deadline, async {
        socket.send(request).await?;
        let mut reply = [0u8; MAX_PACKET_SIZE];
        let length = socket.recv(&mut reply).await?;
        let expected = format!(
            "OBS label=readiness via={} ip={} port={}",
            target.port(),
            local.ip(),
            local.port()
        );
        if reply[..length] != *expected.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mapping reflector returned an invalid readiness echo",
            ));
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "mapping reflector readiness timed out",
        )
    })?
}

fn mapping_probe_label(packet: &[u8]) -> &str {
    let Ok(text) = std::str::from_utf8(packet) else {
        return "-";
    };
    for token in text.split_whitespace() {
        if let Some(label) = token.strip_prefix("label=") {
            if !label.is_empty() {
                return label;
            }
        }
    }
    "-"
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn mapping_probe_replies_with_observed_udp_endpoint() {
        let server = MappingProbeServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("server bind");
        let server_addr = server.local_addr().expect("server local addr");
        let handle = tokio::spawn(server.run());

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client bind");
        let client_addr = client.local_addr().expect("client local addr");
        client
            .send_to(b"REG label=answer:test seq=7", server_addr)
            .await
            .expect("client send");

        let mut buf = [0u8; MAX_PACKET_SIZE];
        let (n, src) = timeout(Duration::from_secs(1), client.recv_from(&mut buf))
            .await
            .expect("reply timeout")
            .expect("reply recv");
        handle.abort();

        assert_eq!(src, server_addr);
        assert_eq!(
            std::str::from_utf8(&buf[..n]).unwrap(),
            format!(
                "OBS label=answer:test via={} ip=127.0.0.1 port={}",
                server_addr.port(),
                client_addr.port()
            )
        );
    }

    #[tokio::test]
    async fn local_mapping_probe_readiness_round_trips_the_running_reflector() {
        let server = MappingProbeServer::bind("0.0.0.0:0".parse().unwrap())
            .await
            .expect("server bind");
        let server_addr = server.local_addr().expect("server local addr");
        let handle = tokio::spawn(server.run());

        probe_local_readiness(server_addr, Duration::from_secs(1))
            .await
            .expect("local mapping reflector readiness");

        handle.abort();
    }

    #[tokio::test]
    async fn only_one_mapping_service_can_own_an_endpoint() {
        let owner = MappingProbeServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("first machine service owns the socket");
        let address = owner.local_addr().unwrap();

        let error = MappingProbeServer::bind(address)
            .await
            .err()
            .expect("a second owner must not share the UDP socket");

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn mapping_probe_label_extracts_label_field_only() {
        assert_eq!(
            mapping_probe_label(b"REG label=offer:abc seq=1"),
            "offer:abc"
        );
        assert_eq!(mapping_probe_label(b"REG seq=1"), "-");
        assert_eq!(mapping_probe_label(b"\xff"), "-");
    }
}
