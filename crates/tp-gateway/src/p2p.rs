//! Gateway P2P state: runtime endpoints bound to authenticated V2 Peer IDs.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;

#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub public: SocketAddr,
    pub locals: Vec<SocketAddr>,
    pub nat_hint: u8,
    pub cert_fp: [u8; 32],
    pub last_seen: Instant,
}

type PeerKey = (String, String);

#[derive(Default)]
pub struct PeerRegistry {
    inner: DashMap<PeerKey, PeerEndpoint>,
    /// Authenticated V2 identity for each runtime attachment handle.
    /// Endpoint updates remain keyed by the existing Replica/client ID.
    stable_peer_by_replica: DashMap<PeerKey, String>,
}

impl PeerRegistry {
    pub fn upsert(&self, tunnel_id: &str, client_id: &str, ep: PeerEndpoint) {
        self.inner
            .insert((tunnel_id.to_string(), client_id.to_string()), ep);
    }
    pub fn get(&self, tunnel_id: &str, client_id: &str) -> Option<PeerEndpoint> {
        self.inner
            .get(&(tunnel_id.to_string(), client_id.to_string()))
            .map(|r| r.clone())
    }
    pub fn remove(&self, tunnel_id: &str, client_id: &str) {
        self.inner
            .remove(&(tunnel_id.to_string(), client_id.to_string()));
        self.stable_peer_by_replica
            .remove(&(tunnel_id.to_string(), client_id.to_string()));
    }
    pub fn bind_v2_identity(&self, tunnel_id: &str, peer_id: &str, replica_id: &str) {
        self.stable_peer_by_replica.insert(
            (tunnel_id.to_string(), replica_id.to_string()),
            peer_id.to_string(),
        );
    }
    pub fn stable_peer_id(&self, tunnel_id: &str, replica_id: &str) -> Option<String> {
        self.stable_peer_by_replica
            .get(&(tunnel_id.to_string(), replica_id.to_string()))
            .map(|peer_id| peer_id.clone())
    }
    pub fn evict_older_than(&self, ttl: Duration) {
        let cutoff = Instant::now() - ttl;
        self.inner.retain(|_, ep| ep.last_seen > cutoff);
        self.stable_peer_by_replica
            .retain(|key, _| self.inner.contains_key(key));
    }
    pub fn touch(&self, tunnel_id: &str, client_id: &str) {
        if let Some(mut ep) = self
            .inner
            .get_mut(&(tunnel_id.to_string(), client_id.to_string()))
        {
            ep.last_seen = Instant::now();
        }
    }
}

/// Server's reply to a client `P2pAnnounce`: tells the client what public
/// `(ip, port)` the gateway observed (server-reflexive candidate) and the
/// gateway's wall-clock at receive time so the client can estimate clock
/// skew before scheduling synchronized hole-punch sends.
pub struct AnnounceAck {
    pub public_ip: String,
    pub public_port: u16,
    pub server_time_ms: i64,
}

/// Apply an inbound `P2pAnnounce`: refresh the registry entry for this
/// client with its observed public address, locals, NAT hint, and cert
/// fingerprint, and produce the `AnnounceAck` payload the caller will send
/// back. The registry key is bound to the authenticated tunnel and client
/// identity; message-supplied group/client fields are intentionally ignored
/// by callers.
#[allow(
    clippy::too_many_arguments,
    reason = "the signaling boundary mirrors the authenticated announce fields"
)]
pub fn handle_announce(
    registry: &PeerRegistry,
    observed: SocketAddr,
    tunnel_id: &str,
    client_id: &str,
    locals: Vec<(String, u16)>,
    nat_hint: u8,
    cert_fp: [u8; 32],
    server_time_ms: i64,
) -> AnnounceAck {
    let locals_sa: Vec<SocketAddr> = locals
        .iter()
        .filter_map(|(ip, port)| ip.parse().ok().map(|ip| SocketAddr::new(ip, *port)))
        .collect();
    registry.upsert(
        tunnel_id,
        client_id,
        PeerEndpoint {
            public: observed,
            locals: locals_sa,
            nat_hint,
            cert_fp,
            last_seen: Instant::now(),
        },
    );
    AnnounceAck {
        public_ip: observed.ip().to_string(),
        public_port: observed.port(),
        server_time_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Instant;

    #[test]
    fn registry_insert_and_lookup() {
        let r = PeerRegistry::default();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 9);
        r.upsert(
            "tun-1",
            "c1",
            PeerEndpoint {
                public: addr,
                locals: vec![],
                nat_hint: 0,
                cert_fp: [0u8; 32],
                last_seen: Instant::now(),
            },
        );
        assert!(r.get("tun-1", "c1").is_some());
        assert!(r.get("tun-1", "c2").is_none());
        assert!(r.get("other-tun", "c1").is_none());
    }

    #[test]
    fn registry_scopes_same_client_id_by_tunnel_id() {
        let r = PeerRegistry::default();
        let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 1001);
        let b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)), 2002);
        r.upsert(
            "tun-a",
            "client-1",
            PeerEndpoint {
                public: a,
                locals: vec![],
                nat_hint: 0,
                cert_fp: [1u8; 32],
                last_seen: Instant::now(),
            },
        );
        r.upsert(
            "tun-b",
            "client-1",
            PeerEndpoint {
                public: b,
                locals: vec![],
                nat_hint: 0,
                cert_fp: [2u8; 32],
                last_seen: Instant::now(),
            },
        );

        assert_eq!(r.get("tun-a", "client-1").unwrap().public, a);
        assert_eq!(r.get("tun-b", "client-1").unwrap().public, b);
        assert!(r.get("tun-c", "client-1").is_none());
    }

    #[test]
    fn handle_announce_updates_registry_and_returns_ack() {
        let r = PeerRegistry::default();
        let public = "1.2.3.4:9".parse().unwrap();
        let now_ms = 1_700_000_000_000;
        let ack = handle_announce(
            &r,
            public,
            "tun-1",
            "c1",
            vec![("10.0.0.1".into(), 4433)],
            2u8, // Restricted
            [9u8; 32],
            now_ms,
        );
        assert_eq!(ack.public_ip, "1.2.3.4");
        assert_eq!(ack.public_port, 9);
        assert_eq!(ack.server_time_ms, now_ms);
        let ep = r.get("tun-1", "c1").unwrap();
        assert_eq!(ep.public, public);
        assert_eq!(ep.nat_hint, 2);
        assert_eq!(ep.cert_fp, [9u8; 32]);
    }

    #[test]
    fn registry_evicts_stale() {
        use std::time::Duration;
        let r = PeerRegistry::default();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 9);
        let stale = Instant::now() - Duration::from_secs(120);
        r.upsert(
            "tun-1",
            "c1",
            PeerEndpoint {
                public: addr,
                locals: vec![],
                nat_hint: 0,
                cert_fp: [0u8; 32],
                last_seen: stale,
            },
        );
        r.bind_v2_identity("tun-1", "stable-peer", "c1");
        r.evict_older_than(Duration::from_secs(60));
        assert!(r.get("tun-1", "c1").is_none());
        assert!(r.stable_peer_id("tun-1", "c1").is_none());
    }
}
