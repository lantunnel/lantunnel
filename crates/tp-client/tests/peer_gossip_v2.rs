use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use tp_client::peer_gossip::{
    PeerGossipControllerV2, PeerGossipErrorV2, GOSSIP_DIGEST_MAX_INTERVAL_V2,
    GOSSIP_DIGEST_MIN_INTERVAL_V2, MAX_GOSSIP_PEER_ID_BYTES_V2,
};
use tp_client::peer_runtime::{
    LanExportPrefixV2, LanExportV2, PeerRuntimeErrorV2, PeerRuntimeRecordV2,
};
use tp_client::relay_crypto::RelayControlPayloadV2;

fn record(network: [u8; 4], prefix_len: u8, ready: bool) -> PeerRuntimeRecordV2 {
    PeerRuntimeRecordV2::new(vec![LanExportV2 {
        prefix: LanExportPrefixV2::new(Ipv4Addr::from(network), prefix_len)
            .expect("valid private prefix"),
        ready,
    }])
    .expect("valid record")
}

#[test]
fn authenticated_link_ready_sends_the_current_full_record() {
    let local = record([10, 20, 0, 0], 16, true);
    let mut gossip = PeerGossipControllerV2::new(local.clone());

    let outbound = gossip
        .link_ready("peer-b", Instant::now())
        .expect("authenticated link");

    assert_eq!(outbound.target_peer_id, "peer-b");
    assert_eq!(
        outbound.payload,
        RelayControlPayloadV2::RuntimeRecord(local.encode())
    );
}

#[test]
fn local_record_change_pushes_one_full_replacement_to_every_ready_link() {
    let mut gossip = PeerGossipControllerV2::new(record([10, 20, 0, 0], 16, true));
    let now = Instant::now();
    gossip.link_ready("peer-b", now).unwrap();
    gossip.link_ready("peer-c", now).unwrap();
    let changed = record([192, 168, 40, 0], 24, true);

    let outbound = gossip.set_local_record(changed.clone());

    assert_eq!(outbound.len(), 2);
    assert!(outbound.iter().all(|message| {
        message.payload == RelayControlPayloadV2::RuntimeRecord(changed.encode())
    }));
    assert_eq!(outbound[0].target_peer_id, "peer-b");
    assert_eq!(outbound[1].target_peer_id, "peer-c");
}

#[test]
fn setting_the_same_local_record_is_a_no_op() {
    let local = record([10, 20, 0, 0], 16, true);
    let mut gossip = PeerGossipControllerV2::new(local.clone());
    gossip.link_ready("peer-b", Instant::now()).unwrap();

    assert!(gossip.set_local_record(local).is_empty());
}

#[test]
fn ready_link_gets_one_current_digest_about_every_thirty_seconds() {
    let local = record([172, 20, 0, 0], 16, true);
    let mut gossip = PeerGossipControllerV2::new(local.clone());
    let started = Instant::now();
    gossip.link_ready("peer-b", started).unwrap();

    assert!(gossip
        .poll_digests(started + GOSSIP_DIGEST_MIN_INTERVAL_V2 - Duration::from_millis(1))
        .is_empty());
    let outbound = gossip.poll_digests(started + GOSSIP_DIGEST_MAX_INTERVAL_V2);

    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].target_peer_id, "peer-b");
    assert_eq!(
        outbound[0].payload,
        RelayControlPayloadV2::Digest(local.hash())
    );
    assert!(gossip
        .poll_digests(started + GOSSIP_DIGEST_MAX_INTERVAL_V2)
        .is_empty());
}

#[test]
fn first_full_record_is_reciprocated_once_and_updates_only_the_authenticated_origin() {
    let local = record([10, 20, 0, 0], 16, true);
    let mut gossip = PeerGossipControllerV2::new(local.clone());
    let now = Instant::now();
    gossip.link_ready("peer-b", now).unwrap();
    gossip.link_ready("peer-c", now).unwrap();
    let remote = record([192, 168, 80, 0], 24, true);

    let response = gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        )
        .expect("valid full record");

    let response = response.expect("first full record is reciprocated");
    assert_eq!(response.target_peer_id, "peer-b");
    assert_eq!(
        response.payload,
        RelayControlPayloadV2::RuntimeRecord(local.encode())
    );
    assert_eq!(gossip.directory().record("peer-b"), Some(&remote));
    assert_eq!(gossip.directory().record("peer-c"), None);

    let changed_remote = record([192, 168, 81, 0], 24, true);
    assert_eq!(
        gossip
            .receive(
                "peer-b",
                RelayControlPayloadV2::RuntimeRecord(changed_remote.encode()),
            )
            .expect("later full record in the same generation"),
        None
    );
    assert_eq!(gossip.directory().record("peer-b"), Some(&changed_remote));
}

#[test]
fn new_authenticated_link_generation_reciprocates_with_a_retained_record() {
    let local = record([10, 20, 0, 0], 16, true);
    let remote = record([192, 168, 80, 0], 24, true);
    let mut gossip = PeerGossipControllerV2::new(local.clone());
    let now = Instant::now();
    gossip.link_ready("peer-b", now).unwrap();
    gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        )
        .expect("first generation record")
        .expect("first generation reciprocal");

    gossip
        .link_ready("peer-b", now + Duration::from_secs(1))
        .unwrap();
    let response = gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        )
        .expect("replacement generation record")
        .expect("replacement generation reciprocal");

    assert_eq!(response.target_peer_id, "peer-b");
    assert_eq!(
        response.payload,
        RelayControlPayloadV2::RuntimeRecord(local.encode())
    );
}

#[test]
fn overtaken_initial_full_sync_converges_both_peers_without_an_echo_loop() {
    let record_a = record([10, 20, 0, 0], 16, true);
    let record_b = record([192, 168, 80, 0], 24, true);
    let mut gossip_a = PeerGossipControllerV2::new(record_a.clone());
    let mut gossip_b = PeerGossipControllerV2::new(record_b.clone());
    let now = Instant::now();
    let full_a = gossip_a.link_ready("peer-b", now).unwrap();
    let _overtaken_full_b = gossip_b.link_ready("peer-a", now).unwrap();

    // B's initial push overtook the Answer and was dropped before A had the
    // authenticated key. A's post-Answer push is the first deliverable record.
    let reciprocal_b = gossip_b
        .receive("peer-a", full_a.payload)
        .expect("A's authenticated full record")
        .expect("B reciprocates its current record");
    let reciprocal_a = gossip_a
        .receive("peer-b", reciprocal_b.payload)
        .expect("B's reciprocal full record")
        .expect("A finishes its own first-record reciprocity");
    let terminal = gossip_b
        .receive("peer-a", reciprocal_a.payload)
        .expect("bounded reciprocal replay");

    assert_eq!(terminal, None);
    assert_eq!(gossip_a.directory().record("peer-b"), Some(&record_b));
    assert_eq!(gossip_b.directory().record("peer-a"), Some(&record_a));
}

#[test]
fn malformed_first_record_does_not_consume_generation_reciprocity() {
    let local = record([10, 20, 0, 0], 16, true);
    let remote = record([192, 168, 80, 0], 24, true);
    let mut gossip = PeerGossipControllerV2::new(local.clone());
    gossip.link_ready("peer-b", Instant::now()).unwrap();

    assert_eq!(
        gossip.receive("peer-b", RelayControlPayloadV2::RuntimeRecord(vec![0; 386]),),
        Err(PeerGossipErrorV2::PeerRuntime(
            PeerRuntimeErrorV2::InvalidEncoding
        ))
    );
    let response = gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        )
        .expect("valid first record")
        .expect("malformed record did not consume reciprocity");

    assert_eq!(
        response.payload,
        RelayControlPayloadV2::RuntimeRecord(local.encode())
    );
}

#[test]
fn matching_digest_is_a_no_op_and_different_digest_requests_full_repair() {
    let mut gossip = PeerGossipControllerV2::new(PeerRuntimeRecordV2::default());
    gossip.link_ready("peer-b", Instant::now()).unwrap();
    let remote = record([10, 77, 0, 0], 16, true);
    gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        )
        .unwrap();

    assert_eq!(
        gossip
            .receive("peer-b", RelayControlPayloadV2::Digest(remote.hash()))
            .expect("matching digest"),
        None
    );
    let response = gossip
        .receive("peer-b", RelayControlPayloadV2::Digest([0x55; 32]))
        .expect("different digest")
        .expect("repair request");

    assert_eq!(response.target_peer_id, "peer-b");
    assert_eq!(response.payload, RelayControlPayloadV2::Need);
}

#[test]
fn need_is_repaired_with_the_current_full_record() {
    let mut gossip = PeerGossipControllerV2::new(record([10, 10, 0, 0], 16, true));
    gossip.link_ready("peer-b", Instant::now()).unwrap();
    let current = record([192, 168, 90, 0], 24, true);
    gossip.set_local_record(current.clone());

    let response = gossip
        .receive("peer-b", RelayControlPayloadV2::Need)
        .expect("valid need")
        .expect("full repair");

    assert_eq!(response.target_peer_id, "peer-b");
    assert_eq!(
        response.payload,
        RelayControlPayloadV2::RuntimeRecord(current.encode())
    );
}

#[test]
fn link_close_removes_origin_and_returning_exporter_joins_the_tail() {
    let mut gossip = PeerGossipControllerV2::new(PeerRuntimeRecordV2::default());
    let now = Instant::now();
    gossip.link_ready("peer-b", now).unwrap();
    gossip.link_ready("peer-c", now).unwrap();
    let shared = record([10, 42, 0, 0], 16, true);
    let prefix = shared.lan_exports[0].prefix;
    for peer_id in ["peer-b", "peer-c"] {
        gossip
            .receive(
                peer_id,
                RelayControlPayloadV2::RuntimeRecord(shared.encode()),
            )
            .unwrap();
    }
    assert_eq!(
        gossip.directory().exporters(prefix),
        vec!["peer-b", "peer-c"]
    );

    assert!(gossip.link_closed("peer-b").expect("known link"));
    assert_eq!(gossip.directory().exporters(prefix), vec!["peer-c"]);

    gossip.link_ready("peer-b", now).unwrap();
    gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(shared.encode()),
        )
        .unwrap();
    assert_eq!(
        gossip.directory().exporters(prefix),
        vec!["peer-c", "peer-b"]
    );
}

#[test]
fn closed_link_reopens_with_latest_full_instead_of_queued_history() {
    let mut gossip = PeerGossipControllerV2::new(record([10, 10, 0, 0], 16, true));
    let now = Instant::now();
    gossip.link_ready("peer-b", now).unwrap();
    gossip.link_closed("peer-b").unwrap();

    assert!(gossip
        .set_local_record(record([172, 20, 0, 0], 16, true))
        .is_empty());
    let latest = record([192, 168, 120, 0], 24, true);
    assert!(gossip.set_local_record(latest.clone()).is_empty());

    let outbound = gossip.link_ready("peer-b", now).unwrap();
    assert_eq!(
        outbound.payload,
        RelayControlPayloadV2::RuntimeRecord(latest.encode())
    );
}

#[test]
fn invalid_identity_unready_link_and_malformed_payload_fail_closed() {
    let mut gossip = PeerGossipControllerV2::new(PeerRuntimeRecordV2::default());
    let now = Instant::now();
    for invalid_peer_id in [
        String::new(),
        " peer-b".to_owned(),
        "x".repeat(MAX_GOSSIP_PEER_ID_BYTES_V2 + 1),
    ] {
        assert_eq!(
            gossip.link_ready(&invalid_peer_id, now),
            Err(PeerGossipErrorV2::InvalidPeerId)
        );
    }
    assert_eq!(
        gossip.receive("peer-b", RelayControlPayloadV2::Digest([0; 32])),
        Err(PeerGossipErrorV2::LinkNotReady)
    );

    gossip.link_ready("peer-b", now).unwrap();
    let remote = record([192, 168, 100, 0], 24, true);
    gossip
        .receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        )
        .unwrap();
    assert_eq!(
        gossip.receive("peer-b", RelayControlPayloadV2::RuntimeRecord(vec![0; 386]),),
        Err(PeerGossipErrorV2::PeerRuntime(
            PeerRuntimeErrorV2::InvalidEncoding
        ))
    );
    assert_eq!(gossip.directory().record("peer-b"), Some(&remote));
    assert_eq!(
        gossip.receive(
            "peer-b",
            RelayControlPayloadV2::Open {
                network: "tcp".into(),
                address: "198.18.0.2:80".into(),
            },
        ),
        Err(PeerGossipErrorV2::UnexpectedPayload)
    );

    gossip.link_closed("peer-b").unwrap();
    assert_eq!(
        gossip.receive(
            "peer-b",
            RelayControlPayloadV2::RuntimeRecord(remote.encode()),
        ),
        Err(PeerGossipErrorV2::LinkNotReady)
    );
    assert_eq!(gossip.directory().record("peer-b"), None);
}
