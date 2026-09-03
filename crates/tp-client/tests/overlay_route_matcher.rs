use std::net::{IpAddr, Ipv4Addr};

use tp_client::peer_runtime::{LanExportPrefixV2, LanExportV2, PeerRuntimeRecordV2};
use tp_client::route_matcher::{
    LanAliasInstallError, OverlayRouteMatch, OverlayRouteMatcher, MAX_LAN_ALIASES_PER_PEER,
    MAX_UNIQUE_LAN_ALIAS_DESTINATIONS,
};

fn v2_prefix(cidr: &str) -> LanExportPrefixV2 {
    let (network, bits) = cidr.split_once('/').expect("CIDR");
    LanExportPrefixV2::new(
        network.parse().expect("IPv4"),
        bits.parse().expect("prefix"),
    )
    .expect("valid LAN Export")
}

fn v2_record(exports: &[(&str, bool)]) -> PeerRuntimeRecordV2 {
    PeerRuntimeRecordV2::new(
        exports
            .iter()
            .map(|(cidr, ready)| LanExportV2 {
                prefix: v2_prefix(cidr),
                ready: *ready,
            })
            .collect(),
    )
    .expect("valid runtime record")
}

#[test]
fn v2_lan_exports_use_longest_ready_prefix() {
    let mut matcher = OverlayRouteMatcher::default();
    matcher
        .replace_v2_lan_export_origin("peer-wide", v2_record(&[("10.0.0.0/8", true)]))
        .unwrap();
    matcher
        .replace_v2_lan_export_origin("peer-narrow", v2_record(&[("10.20.0.0/16", true)]))
        .unwrap();

    assert_eq!(
        matcher.match_v2_destination(IpAddr::V4("10.20.30.40".parse().unwrap())),
        OverlayRouteMatch::Peer {
            peer_id: "peer-narrow".into(),
        }
    );
    assert_eq!(
        matcher.match_v2_destination(IpAddr::V4("10.30.40.50".parse().unwrap())),
        OverlayRouteMatch::Peer {
            peer_id: "peer-wide".into(),
        }
    );
}

#[test]
fn v2_exact_overlay_precedes_lan_lpm_and_duplicate_overlay_fails_closed() {
    let mut matcher = OverlayRouteMatcher::default();
    let overlay: Ipv4Addr = "198.18.22.9".parse().unwrap();
    matcher.upsert_peer_overlay("peer-overlay-a", overlay);
    matcher
        .replace_v2_lan_export_origin("peer-lan", v2_record(&[("10.0.0.0/8", true)]))
        .unwrap();

    assert_eq!(
        matcher.match_v2_destination(IpAddr::V4(overlay)),
        OverlayRouteMatch::Peer {
            peer_id: "peer-overlay-a".into(),
        }
    );

    matcher.upsert_peer_overlay("peer-overlay-b", overlay);
    assert_eq!(
        matcher.match_v2_destination(IpAddr::V4(overlay)),
        OverlayRouteMatch::Ambiguous,
        "duplicate exact signed Overlay ownership must fail closed"
    );
}

#[test]
fn v2_peer_ids_are_opaque_while_legacy_replicas_still_share_a_family() {
    let mut matcher = OverlayRouteMatcher::default();
    let v2_peer_id = "peer-AbCd1234-1";
    let v2_overlay: Ipv4Addr = "198.18.22.10".parse().unwrap();
    matcher.upsert_peer_overlay(v2_peer_id, v2_overlay);

    assert!(matcher.has_peer_overlay(v2_peer_id));
    assert!(!matcher.has_peer_overlay("peer-AbCd1234-0"));
    assert_eq!(
        matcher.match_v2_destination(IpAddr::V4(v2_overlay)),
        OverlayRouteMatch::Peer {
            peer_id: v2_peer_id.into(),
        }
    );
    assert!(!matcher.remove_peer("peer-AbCd1234-0"));
    assert!(matcher.remove_peer(v2_peer_id));

    let legacy_overlay = matcher
        .upsert_replica("tunnel-a", "tunnel-a-AbC123z9-1")
        .expect("valid legacy replica");
    assert_eq!(
        matcher.match_destination(IpAddr::V4(legacy_overlay)),
        OverlayRouteMatch::Peer {
            peer_id: "tunnel-a-AbC123z9-0".into(),
        }
    );
}

#[test]
fn v2_duplicate_lan_prefix_uses_local_first_seen_then_ready_origin_joins_tail() {
    let mut matcher = OverlayRouteMatcher::default();
    let ready = v2_record(&[("192.168.70.0/24", true)]);
    matcher
        .replace_v2_lan_export_origin("peer-a", ready.clone())
        .unwrap();
    matcher
        .replace_v2_lan_export_origin("peer-b", ready.clone())
        .unwrap();
    let destination = IpAddr::V4("192.168.70.44".parse().unwrap());

    assert_eq!(
        matcher.match_v2_destination(destination),
        OverlayRouteMatch::Peer {
            peer_id: "peer-a".into(),
        },
        "local first-seen exporter is ActiveHere"
    );

    matcher
        .replace_v2_lan_export_origin("peer-a", v2_record(&[("192.168.70.0/24", false)]))
        .unwrap();
    assert_eq!(
        matcher.match_v2_destination(destination),
        OverlayRouteMatch::Peer {
            peer_id: "peer-b".into(),
        },
        "Standby becomes active only when the prior origin is not ready"
    );

    matcher
        .replace_v2_lan_export_origin("peer-a", ready)
        .unwrap();
    assert_eq!(
        matcher.match_v2_destination(destination),
        OverlayRouteMatch::Peer {
            peer_id: "peer-b".into(),
        },
        "returning ready origin joins the local order tail"
    );
    assert!(matcher.remove_v2_lan_export_origin("peer-b"));
    assert_eq!(
        matcher.match_v2_destination(destination),
        OverlayRouteMatch::Peer {
            peer_id: "peer-a".into(),
        }
    );
}

#[test]
fn v2_active_lan_export_snapshot_projects_prefix_and_stable_owner_only() {
    let mut matcher = OverlayRouteMatcher::default();
    matcher
        .replace_v2_lan_export_origin(
            "stable-peer-b",
            v2_record(&[("192.168.70.0/24", true), ("172.20.0.0/16", false)]),
        )
        .unwrap();
    matcher
        .replace_v2_lan_export_origin(
            "stable-peer-a",
            v2_record(&[("10.20.0.0/16", true), ("192.168.70.0/24", true)]),
        )
        .unwrap();

    assert_eq!(
        matcher.v2_active_lan_export_snapshot(),
        vec![
            (v2_prefix("10.20.0.0/16"), "stable-peer-a".into()),
            (v2_prefix("192.168.70.0/24"), "stable-peer-b".into()),
        ]
    );
}

#[test]
fn v2_missing_non_private_and_ipv6_destinations_fail_closed() {
    let mut matcher = OverlayRouteMatcher::default();
    matcher
        .replace_v2_lan_export_origin("peer-a", v2_record(&[("172.20.0.0/16", true)]))
        .unwrap();

    for destination in [
        "172.21.1.1".parse().unwrap(),
        "8.8.8.8".parse().unwrap(),
        "198.18.200.1".parse().unwrap(),
        "fd00::1".parse().unwrap(),
    ] {
        assert_eq!(
            matcher.match_v2_destination(destination),
            OverlayRouteMatch::Unmatched,
            "missing, public, unowned Overlay-pool, and IPv6 destinations never fall through"
        );
    }
}

#[test]
fn exact_overlay_32_selects_one_peer_and_neighbour_is_unmatched() {
    let mut matcher = OverlayRouteMatcher::default();
    let overlay = matcher
        .upsert_replica("tunnel-a", "tunnel-a-AbC123z9-0")
        .expect("valid stable replica");

    assert_eq!(overlay, Ipv4Addr::new(198, 18, 172, 249));
    assert_eq!(
        matcher.match_destination(IpAddr::V4(overlay)),
        OverlayRouteMatch::Peer {
            peer_id: "tunnel-a-AbC123z9-0".into(),
        }
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(Ipv4Addr::new(198, 18, 172, 248))),
        OverlayRouteMatch::Unmatched,
        "an address merely inside 198.18/16 is not a route"
    );
}

#[test]
fn duplicate_overlay_owners_fail_closed_until_conflict_is_removed() {
    let mut matcher = OverlayRouteMatcher::default();
    let overlay = Ipv4Addr::new(198, 18, 7, 9);
    matcher.upsert_peer_overlay("peer-a-AbCd0001-0", overlay);
    matcher.upsert_peer_overlay("peer-b-AbCd0002-0", overlay);

    assert_eq!(
        matcher.match_destination(IpAddr::V4(overlay)),
        OverlayRouteMatch::Ambiguous,
    );
    assert!(matcher.remove_peer("peer-b-AbCd0002-0"));
    assert_eq!(
        matcher.match_destination(IpAddr::V4(overlay)),
        OverlayRouteMatch::Peer {
            peer_id: "peer-a-AbCd0001-0".into(),
        }
    );
}

#[test]
fn peer_route_update_is_idempotent_and_does_not_leave_old_alias() {
    let mut matcher = OverlayRouteMatcher::default();
    let old_overlay = Ipv4Addr::new(198, 18, 1, 1);
    let new_overlay = Ipv4Addr::new(198, 18, 1, 2);

    matcher.upsert_peer_overlay("peer-a-AbCd0001-0", old_overlay);
    matcher.upsert_peer_overlay("peer-a-AbCd0001-0", old_overlay);
    matcher.upsert_peer_overlay("peer-a-AbCd0001-0", new_overlay);

    assert_eq!(
        matcher.match_destination(IpAddr::V4(old_overlay)),
        OverlayRouteMatch::Unmatched,
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(new_overlay)),
        OverlayRouteMatch::Peer {
            peer_id: "peer-a-AbCd0001-0".into(),
        }
    );
    assert_eq!(matcher.peer_count(), 1);
}

#[test]
fn wrong_tunnel_replica_and_ipv6_never_create_an_overlay_route() {
    let mut matcher = OverlayRouteMatcher::default();
    let error = matcher
        .upsert_replica("tunnel-a", "tunnel-b-AbC123z9-0")
        .expect_err("cross-tunnel replica must fail");
    assert!(error.to_string().contains("tunnel-a"), "{error}");
    assert_eq!(matcher.peer_count(), 0);
    assert_eq!(
        matcher.match_destination("fd00::1".parse().expect("IPv6")),
        OverlayRouteMatch::Unmatched,
    );
}

#[test]
fn route_snapshot_is_sorted_by_overlay_then_peer() {
    let mut matcher = OverlayRouteMatcher::default();
    matcher.upsert_peer_overlay("mesh-peer-b-0", "198.18.2.9".parse().unwrap());
    matcher.upsert_peer_overlay("mesh-peer-a-0", "198.18.1.9".parse().unwrap());

    assert_eq!(
        matcher.route_snapshot(),
        vec![
            ("198.18.1.9".parse().unwrap(), "mesh-peer-a-0".to_string()),
            ("198.18.2.9".parse().unwrap(), "mesh-peer-b-0".to_string()),
        ]
    );
}

#[test]
fn private_lan_host_aliases_select_one_peer_and_replace_atomically() {
    let mut matcher = OverlayRouteMatcher::default();
    let first: Ipv4Addr = "192.168.240.44".parse().unwrap();
    let second: Ipv4Addr = "10.20.30.40".parse().unwrap();
    let replacement: Ipv4Addr = "172.30.240.1".parse().unwrap();

    matcher
        .replace_peer_lan_aliases("mesh-peer-a-0", [first, second])
        .expect("private host aliases");
    assert_eq!(
        matcher.match_destination(IpAddr::V4(first)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-0".into(),
        }
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(second)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-0".into(),
        }
    );

    matcher
        .replace_peer_lan_aliases("mesh-peer-a-0", [replacement])
        .expect("replacement alias set");
    assert_eq!(
        matcher.match_destination(IpAddr::V4(first)),
        OverlayRouteMatch::Unmatched,
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(replacement)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-0".into(),
        }
    );
}

#[test]
fn duplicate_lan_host_aliases_fail_closed_until_one_peer_retires() {
    let mut matcher = OverlayRouteMatcher::default();
    let alias: Ipv4Addr = "192.168.50.9".parse().unwrap();
    matcher
        .replace_peer_lan_aliases("mesh-peer-a-0", [alias])
        .unwrap();
    matcher
        .replace_peer_lan_aliases("mesh-peer-b-0", [alias])
        .unwrap();

    assert_eq!(
        matcher.match_destination(IpAddr::V4(alias)),
        OverlayRouteMatch::Ambiguous,
    );
    assert!(matcher.remove_peer("mesh-peer-b-0"));
    assert_eq!(
        matcher.match_destination(IpAddr::V4(alias)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-0".into(),
        }
    );
}

#[test]
fn public_or_loopback_alias_update_is_rejected_without_losing_previous_routes() {
    let mut matcher = OverlayRouteMatcher::default();
    let private: Ipv4Addr = "192.168.240.44".parse().unwrap();
    matcher
        .replace_peer_lan_aliases("mesh-peer-a-0", [private])
        .unwrap();

    for invalid in ["8.8.8.8", "127.0.0.1", "0.0.0.0"] {
        let error = matcher
            .replace_peer_lan_aliases("mesh-peer-a-0", [invalid.parse().unwrap()])
            .expect_err("non-RFC1918 aliases must fail closed");
        assert!(error.to_string().contains(invalid), "{error}");
    }
    assert_eq!(
        matcher.match_destination(IpAddr::V4(private)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-0".into(),
        }
    );
}

fn private_alias(index: usize) -> Ipv4Addr {
    Ipv4Addr::new(10, 23, (index / 256) as u8, (index % 256) as u8)
}

#[test]
fn logical_peer_alias_limit_is_exact_and_rejected_replacement_is_atomic() {
    let mut matcher = OverlayRouteMatcher::default();
    let at_limit = (0..255).map(private_alias).collect::<Vec<_>>();
    let mut at_limit_with_duplicates = at_limit.clone();
    at_limit_with_duplicates.extend_from_slice(&at_limit[..8]);
    matcher
        .replace_peer_lan_aliases("mesh-peer-a-AbCd0001-1", at_limit_with_duplicates)
        .expect("255 deduplicated exact aliases are allowed for one logical Peer");
    let before = matcher.lan_alias_destinations();
    assert_eq!(before.len(), 255);

    let over_limit = (256..512).map(private_alias).collect::<Vec<_>>();
    let error = matcher
        .replace_peer_lan_aliases("mesh-peer-a-AbCd0001-2", over_limit)
        .expect_err("Replica siblings share one 255-alias logical-Peer cap");

    assert_eq!(
        error,
        LanAliasInstallError::PeerAliasLimitExceeded {
            count: 256,
            max: MAX_LAN_ALIASES_PER_PEER,
        }
    );
    assert_eq!(
        matcher.lan_alias_destinations(),
        before,
        "a rejected replacement must preserve every previous owner and route"
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(at_limit[0])),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-AbCd0001-0".into(),
        }
    );
}

#[test]
fn global_unique_alias_limit_counts_shared_destinations_once_and_rejects_atomically() {
    let mut matcher = OverlayRouteMatcher::default();
    let all_aliases = (0..MAX_UNIQUE_LAN_ALIAS_DESTINATIONS)
        .map(private_alias)
        .collect::<Vec<_>>();
    for (peer_index, aliases) in all_aliases.chunks(MAX_LAN_ALIASES_PER_PEER).enumerate() {
        matcher
            .replace_peer_lan_aliases(
                &format!("capacity-peer-{peer_index}"),
                aliases.iter().copied(),
            )
            .expect("exactly 4096 unique aliases fit across logical Peers");
    }
    assert_eq!(
        matcher.lan_alias_destinations().len(),
        MAX_UNIQUE_LAN_ALIAS_DESTINATIONS
    );

    let replaced_at_limit = *all_aliases.last().expect("non-empty capacity fixture");
    let replacement_at_limit = private_alias(MAX_UNIQUE_LAN_ALIAS_DESTINATIONS);
    let last_peer_aliases = all_aliases[(MAX_UNIQUE_LAN_ALIAS_DESTINATIONS
        / MAX_LAN_ALIASES_PER_PEER)
        * MAX_LAN_ALIASES_PER_PEER..]
        .iter()
        .copied()
        .filter(|alias| *alias != replaced_at_limit)
        .chain([replacement_at_limit])
        .collect::<Vec<_>>();
    matcher
        .replace_peer_lan_aliases("capacity-peer-16", last_peer_aliases)
        .expect("a replacement may exchange one unique slot at the global limit");
    assert_eq!(
        matcher.match_destination(IpAddr::V4(replaced_at_limit)),
        OverlayRouteMatch::Unmatched
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(replacement_at_limit)),
        OverlayRouteMatch::Peer {
            peer_id: "capacity-peer-16".into(),
        }
    );
    assert_eq!(
        matcher.lan_alias_destinations().len(),
        MAX_UNIQUE_LAN_ALIAS_DESTINATIONS
    );

    let shared = all_aliases[0];
    matcher
        .replace_peer_lan_aliases("sharing-peer", [shared])
        .expect("an existing destination consumes no additional global slot");
    assert_eq!(
        matcher.lan_alias_destinations().len(),
        MAX_UNIQUE_LAN_ALIAS_DESTINATIONS,
        "an ambiguous destination is one captured OS /32 route"
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(shared)),
        OverlayRouteMatch::Ambiguous
    );

    let new_alias = private_alias(MAX_UNIQUE_LAN_ALIAS_DESTINATIONS + 1);
    let destinations_before_failure = matcher.lan_alias_destinations();
    let error = matcher
        .replace_peer_lan_aliases("sharing-peer", [shared, new_alias])
        .expect_err("a genuinely new destination would exceed the global limit");
    assert_eq!(
        error,
        LanAliasInstallError::UniqueDestinationLimitExceeded {
            count: MAX_UNIQUE_LAN_ALIAS_DESTINATIONS + 1,
            max: MAX_UNIQUE_LAN_ALIAS_DESTINATIONS,
        }
    );
    assert_eq!(
        matcher.lan_alias_destinations(),
        destinations_before_failure
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(shared)),
        OverlayRouteMatch::Ambiguous,
        "the rejected replacement must preserve the old shared owner"
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(new_alias)),
        OverlayRouteMatch::Unmatched,
        "the rejected replacement must not partially install its new owner"
    );
}

#[test]
fn full_lan_alias_snapshot_replaces_every_peer_and_empty_snapshot_clears_only_aliases() {
    let mut matcher = OverlayRouteMatcher::default();
    let overlay: Ipv4Addr = "198.18.7.9".parse().unwrap();
    let retired: Ipv4Addr = "192.168.1.10".parse().unwrap();
    let remote_b: Ipv4Addr = "192.168.2.20".parse().unwrap();
    let remote_c: Ipv4Addr = "10.3.0.30".parse().unwrap();
    matcher.upsert_peer_overlay("mesh-peer-b-0", overlay);
    matcher
        .replace_peer_lan_aliases("mesh-retired-0", [retired])
        .unwrap();

    matcher
        .replace_lan_alias_snapshot(&[
            ("mesh-peer-b-0".into(), vec![remote_b]),
            ("mesh-peer-c-0".into(), vec![remote_c]),
        ])
        .expect("valid full snapshot");

    assert_eq!(
        matcher.match_destination(IpAddr::V4(retired)),
        OverlayRouteMatch::Unmatched
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(remote_b)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-b-0".into()
        }
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(remote_c)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-c-0".into()
        }
    );

    matcher
        .replace_lan_alias_snapshot(&[])
        .expect("empty full snapshot clears aliases");
    assert!(matcher.lan_alias_destinations().is_empty());
    assert_eq!(
        matcher.match_destination(IpAddr::V4(overlay)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-b-0".into()
        },
        "LAN replacement must not mutate Overlay ownership"
    );
}

#[test]
fn invalid_full_lan_alias_snapshot_preserves_the_entire_previous_snapshot() {
    let mut matcher = OverlayRouteMatcher::default();
    let previous: Ipv4Addr = "192.168.1.10".parse().unwrap();
    let would_be_partial: Ipv4Addr = "192.168.2.20".parse().unwrap();
    let public: Ipv4Addr = "8.8.8.8".parse().unwrap();
    matcher
        .replace_lan_alias_snapshot(&[("mesh-peer-a-0".into(), vec![previous])])
        .expect("initial snapshot");

    matcher
        .replace_lan_alias_snapshot(&[
            ("mesh-peer-b-0".into(), vec![would_be_partial]),
            ("mesh-peer-c-0".into(), vec![public]),
        ])
        .expect_err("one invalid route rejects the whole snapshot");

    assert_eq!(matcher.lan_alias_destinations(), vec![previous]);
    assert_eq!(
        matcher.match_destination(IpAddr::V4(previous)),
        OverlayRouteMatch::Peer {
            peer_id: "mesh-peer-a-0".into()
        }
    );
    assert_eq!(
        matcher.match_destination(IpAddr::V4(would_be_partial)),
        OverlayRouteMatch::Unmatched,
        "the valid prefix of a rejected snapshot must not be installed"
    );
}
