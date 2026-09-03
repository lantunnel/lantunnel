use std::net::Ipv4Addr;

use tp_client::overlay::{derive_peer_overlay_ipv4, overlay_ipv4_for_replica_id};

#[test]
fn overlay_v1_matches_platform_fixed_vectors() {
    assert_eq!(
        derive_peer_overlay_ipv4("tunnel-a", "AbC123z9"),
        Ipv4Addr::new(198, 18, 172, 249),
    );
    assert_eq!(
        derive_peer_overlay_ipv4("tunnel-reuse-a", "SameSeed"),
        Ipv4Addr::new(198, 18, 152, 98),
    );
}

#[test]
fn every_replica_in_one_stable_family_reconstructs_one_overlay() {
    let expected = Ipv4Addr::new(198, 18, 172, 249);
    assert_eq!(
        overlay_ipv4_for_replica_id("tunnel-a", "tunnel-a-AbC123z9-0").expect("primary replica id"),
        expected,
    );
    assert_eq!(
        overlay_ipv4_for_replica_id("tunnel-a", "tunnel-a-AbC123z9-7").expect("sidecar replica id"),
        expected,
    );
}

#[test]
fn replica_from_another_tunnel_or_invalid_family_is_rejected() {
    assert!(overlay_ipv4_for_replica_id("tunnel-a", "tunnel-b-AbC123z9-0").is_err());
    assert!(overlay_ipv4_for_replica_id("tunnel-a", "legacy-client").is_err());
}
