use std::net::Ipv4Addr;

use tp_client::peer_runtime::{
    LanExportPrefixV2, LanExportV2, LocalLanExportConfigV2, PeerGossipDirectoryV2,
    PeerRuntimeErrorV2, PeerRuntimeRecordV2, RuntimeRecordRepairV2, MAX_LAN_EXPORTS_PER_PEER_V2,
};

fn prefix(address: [u8; 4], bits: u8) -> LanExportPrefixV2 {
    LanExportPrefixV2::new(address.into(), bits).expect("valid private prefix")
}

fn record(exports: &[(LanExportPrefixV2, bool)]) -> PeerRuntimeRecordV2 {
    PeerRuntimeRecordV2::new(
        exports
            .iter()
            .map(|(prefix, ready)| LanExportV2 {
                prefix: *prefix,
                ready: *ready,
            })
            .collect(),
    )
    .expect("valid record")
}

#[test]
fn record_codec_is_canonical_and_hashes_full_replacement() {
    let a = prefix([192, 168, 50, 0], 24);
    let b = prefix([10, 2, 0, 0], 16);
    let expected = record(&[(b, true), (a, false)]);
    let reversed = record(&[(a, false), (b, true)]);
    assert_eq!(expected, reversed);
    assert_eq!(expected.hash(), reversed.hash());
    assert_eq!(
        PeerRuntimeRecordV2::decode(&expected.encode()).expect("decode"),
        expected
    );
}

#[test]
fn export_validation_accepts_only_canonical_rfc1918_ipv4() {
    for (address, bits) in [
        ([0, 0, 0, 0], 0),
        ([198, 18, 0, 0], 16),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([224, 0, 0, 0], 4),
        ([8, 8, 8, 0], 24),
        ([192, 168, 1, 7], 24),
    ] {
        assert_eq!(
            LanExportPrefixV2::new(Ipv4Addr::from(address), bits),
            Err(PeerRuntimeErrorV2::InvalidPrefix)
        );
    }
    assert!(LanExportPrefixV2::new("10.0.0.0".parse().unwrap(), 8).is_ok());
    assert!(LanExportPrefixV2::new("172.16.0.0".parse().unwrap(), 12).is_ok());
    assert!(LanExportPrefixV2::new("192.168.1.0".parse().unwrap(), 24).is_ok());
}

#[test]
fn export_validation_rejects_a_prefix_that_extends_beyond_rfc1918() {
    assert!(LanExportPrefixV2::new("192.168.0.0".parse().unwrap(), 15).is_err());
}

#[test]
fn duplicate_and_unbounded_exports_fail_closed() {
    let p = prefix([10, 0, 0, 0], 8);
    assert_eq!(
        PeerRuntimeRecordV2::new(vec![
            LanExportV2 {
                prefix: p,
                ready: true,
            },
            LanExportV2 {
                prefix: p,
                ready: false,
            },
        ]),
        Err(PeerRuntimeErrorV2::DuplicatePrefix)
    );
    let too_many = (0..=MAX_LAN_EXPORTS_PER_PEER_V2)
        .map(|index| LanExportV2 {
            prefix: prefix([10, index as u8, 0, 0], 16),
            ready: true,
        })
        .collect();
    assert_eq!(
        PeerRuntimeRecordV2::new(too_many),
        Err(PeerRuntimeErrorV2::TooManyExports)
    );
}

#[test]
fn local_first_seen_order_is_not_global_and_repair_does_not_reorder() {
    let p = prefix([192, 168, 80, 0], 24);
    let ready = record(&[(p, true)]);
    let mut left = PeerGossipDirectoryV2::default();
    left.replace_origin("peer-a", ready.clone()).unwrap();
    left.replace_origin("peer-b", ready.clone()).unwrap();
    let mut right = PeerGossipDirectoryV2::default();
    right.replace_origin("peer-b", ready.clone()).unwrap();
    right.replace_origin("peer-a", ready.clone()).unwrap();

    assert_eq!(left.exporters(p), vec!["peer-a", "peer-b"]);
    assert_eq!(right.exporters(p), vec!["peer-b", "peer-a"]);
    assert_eq!(
        left.compare_digest("peer-a", ready.hash()),
        RuntimeRecordRepairV2::InSync
    );
    left.replace_origin("peer-a", ready.clone()).unwrap();
    assert_eq!(left.exporters(p), vec!["peer-a", "peer-b"]);
}

#[test]
fn ready_false_or_link_close_removes_and_returning_origin_joins_tail() {
    let p = prefix([10, 77, 0, 0], 16);
    let ready = record(&[(p, true)]);
    let not_ready = record(&[(p, false)]);
    let mut directory = PeerGossipDirectoryV2::default();
    directory.replace_origin("peer-a", ready.clone()).unwrap();
    directory.replace_origin("peer-b", ready.clone()).unwrap();

    directory.replace_origin("peer-a", not_ready).unwrap();
    assert_eq!(directory.active_exporter(p), Some("peer-b"));
    directory.replace_origin("peer-a", ready.clone()).unwrap();
    assert_eq!(directory.exporters(p), vec!["peer-b", "peer-a"]);

    assert!(directory.remove_origin("peer-b"));
    assert_eq!(directory.exporters(p), vec!["peer-a"]);
    directory.replace_origin("peer-b", ready).unwrap();
    assert_eq!(directory.exporters(p), vec!["peer-a", "peer-b"]);
}

#[test]
fn active_export_snapshot_is_sorted_and_tracks_local_promotion() {
    let wide = prefix([10, 0, 0, 0], 8);
    let narrow = prefix([192, 168, 70, 0], 24);
    let mut directory = PeerGossipDirectoryV2::default();
    directory
        .replace_origin("peer-z", record(&[(narrow, true), (wide, false)]))
        .unwrap();
    directory
        .replace_origin("peer-a", record(&[(wide, true), (narrow, true)]))
        .unwrap();

    assert_eq!(
        directory.active_export_snapshot(),
        vec![(wide, "peer-a".to_string()), (narrow, "peer-z".to_string())],
        "only ready ActiveHere owners appear in canonical prefix order"
    );

    directory
        .replace_origin("peer-z", record(&[(narrow, false)]))
        .unwrap();
    assert_eq!(
        directory.active_export_snapshot(),
        vec![(wide, "peer-a".to_string()), (narrow, "peer-a".to_string())],
        "the local Standby is projected immediately after ActiveHere withdraws"
    );

    assert!(directory.remove_origin("peer-a"));
    assert!(directory.active_export_snapshot().is_empty());
}

#[test]
fn missing_or_different_digest_requests_full_record_only() {
    let p = prefix([172, 20, 0, 0], 16);
    let mut directory = PeerGossipDirectoryV2::default();
    directory
        .replace_origin("peer-a", record(&[(p, true)]))
        .unwrap();
    assert_eq!(
        directory.compare_digest("peer-a", [0x55; 32]),
        RuntimeRecordRepairV2::NeedFullRecord
    );
    assert_eq!(
        directory.compare_digest("peer-missing", [0; 32]),
        RuntimeRecordRepairV2::NeedFullRecord
    );
}

#[test]
fn malformed_record_bytes_and_blank_origin_are_rejected() {
    for encoded in [vec![], vec![1], vec![1, 10, 0, 0, 0, 8, 2], vec![0, 0]] {
        assert!(PeerRuntimeRecordV2::decode(&encoded).is_err());
    }
    assert_eq!(
        PeerGossipDirectoryV2::default().replace_origin(" ", PeerRuntimeRecordV2::default()),
        Err(PeerRuntimeErrorV2::InvalidOrigin)
    );
}

#[test]
fn automatic_current_lan_adds_connected_prefixes_without_touching_the_typed_list() {
    let typed = prefix([10, 20, 0, 0], 16);
    let attached = prefix([192, 168, 44, 0], 24);
    let config = LocalLanExportConfigV2 {
        configured: vec![typed],
        auto_current_lan: true,
    };

    assert_eq!(
        config.resolve(Some(&[attached])),
        record(&[(typed, false), (attached, true)])
    );

    // The same machine with the switch off publishes only what its owner typed.
    assert_eq!(
        LocalLanExportConfigV2 {
            auto_current_lan: false,
            ..config.clone()
        }
        .resolve(Some(&[attached])),
        record(&[(typed, false)])
    );
}

#[test]
fn automatic_current_lan_follows_the_machine_onto_a_new_network() {
    let first = prefix([192, 168, 1, 0], 24);
    let second = prefix([10, 0, 5, 0], 24);
    let config = LocalLanExportConfigV2 {
        configured: Vec::new(),
        auto_current_lan: true,
    };

    assert_eq!(config.resolve(Some(&[first])), record(&[(first, true)]));
    assert_eq!(config.resolve(Some(&[second])), record(&[(second, true)]));

    // An unreadable interface list is not evidence of a LAN, so nothing is
    // published rather than the network this machine used to be on.
    assert_eq!(config.resolve(None), PeerRuntimeRecordV2::default());
    assert_eq!(config.resolve(Some(&[])), PeerRuntimeRecordV2::default());
}

#[test]
fn a_typed_prefix_that_is_also_attached_stays_one_ready_export() {
    let shared = prefix([192, 168, 8, 0], 24);
    let config = LocalLanExportConfigV2 {
        configured: vec![shared],
        auto_current_lan: true,
    };

    assert_eq!(config.resolve(Some(&[shared])), record(&[(shared, true)]));
}

#[test]
fn automatic_prefixes_never_push_the_typed_list_past_the_record_limit() {
    let configured = (0..MAX_LAN_EXPORTS_PER_PEER_V2)
        .map(|index| prefix([10, index as u8, 0, 0], 16))
        .collect::<Vec<_>>();
    let attached = prefix([192, 168, 99, 0], 24);
    let resolved = LocalLanExportConfigV2 {
        configured: configured.clone(),
        auto_current_lan: true,
    }
    .resolve(Some(&[attached]));

    assert_eq!(resolved.lan_exports.len(), MAX_LAN_EXPORTS_PER_PEER_V2);
    assert!(resolved
        .lan_exports
        .iter()
        .all(|export| configured.contains(&export.prefix)));
}
