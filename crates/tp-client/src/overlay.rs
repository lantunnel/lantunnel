//! Deterministic Tunnel-scoped Overlay IPv4 addresses.
//!
//! The Platform persists the authoritative allocation. This module reproduces
//! the public Overlay v1 mapping from an existing stable Replica family so v4
//! membership hints do not need another transport field.

use std::net::Ipv4Addr;

use sha2::{Digest, Sha256};
use thiserror::Error;

const OVERLAY_DERIVATION_DOMAIN: &[u8] = b"lantunnel-overlay-v1\0";
const OVERLAY_HOST_COUNT: u16 = 65_534;

pub fn derive_peer_overlay_ipv4(tunnel_id: &str, replica_seed: &str) -> Ipv4Addr {
    let mut hasher = Sha256::new();
    hasher.update(OVERLAY_DERIVATION_DOMAIN);
    hasher.update(tunnel_id.as_bytes());
    hasher.update([0]);
    hasher.update(replica_seed.as_bytes());
    let digest = hasher.finalize();
    let hash_prefix = u16::from_be_bytes([digest[0], digest[1]]);
    let host = hash_prefix % OVERLAY_HOST_COUNT + 1;
    Ipv4Addr::new(198, 18, (host >> 8) as u8, host as u8)
}

pub fn overlay_ipv4_for_replica_id(
    tunnel_id: &str,
    replica_id: &str,
) -> Result<Ipv4Addr, OverlayAddressError> {
    let seed = crate::p2p::replica::replica_seed_for_tunnel(tunnel_id, replica_id)
        .ok_or_else(|| OverlayAddressError::InvalidReplicaId(replica_id.to_string()))?;
    Ok(derive_peer_overlay_ipv4(tunnel_id, seed))
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OverlayAddressError {
    #[error("Replica ID is not a stable family member of this Tunnel: {0}")]
    InvalidReplicaId(String),
}
