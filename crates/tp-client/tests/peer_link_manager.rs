use std::time::{Duration, Instant};
use tp_client::peer_link_manager::{
    MembershipSnapshot, MembershipState, PeerConnectivity, PeerDescriptor, PeerId, PeerLinkCommand,
    PeerLinkManager, RelationRole,
};

fn peer(name: &str, replica_count: usize) -> PeerDescriptor {
    PeerDescriptor::from_replica_ids(
        (0..replica_count)
            .map(|index| format!("{name}-{index}"))
            .collect(),
    )
    .expect("valid peer replicas")
}

fn peer_with_indexes(name: &str, indexes: &[usize]) -> PeerDescriptor {
    PeerDescriptor::from_replica_ids(
        indexes
            .iter()
            .map(|index| format!("{name}-{index}"))
            .collect(),
    )
    .expect("valid peer replicas")
}

fn manager(local_peer: PeerDescriptor, replica_count: usize) -> PeerLinkManager {
    PeerLinkManager::new(local_peer, replica_count).expect("non-zero configured replica count")
}

#[test]
fn stable_peer_membership_builds_one_logical_link_and_targets_the_peer_identity() {
    let local = PeerDescriptor::from_stable_peer_and_replica_ids(
        "stable-peer-a".into(),
        vec!["runtime-a-AbCd0001-0".into(), "runtime-a-AbCd0001-1".into()],
    )
    .expect("local stable Peer");
    let remote =
        PeerDescriptor::from_stable_peer_id("stable-peer-b".into()).expect("remote stable Peer");
    let mut manager = manager(local.clone(), 1);

    let work = manager.apply_snapshot(&MembershipSnapshot::new(vec![local, remote]));

    assert_eq!(
        work.iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) => (
                    lane.index(),
                    lane.local_replica_id(),
                    lane.remote_replica_id(),
                ),
            })
            .collect::<Vec<_>>(),
        vec![(0, "runtime-a-AbCd0001-0", "stable-peer-b")]
    );
}

fn unavailable(_: &PeerId) -> PeerConnectivity {
    PeerConnectivity::unavailable()
}

#[test]
fn unordered_peer_pair_has_one_shared_bidirectional_link() {
    let peer_a = peer("peer-a-AbCd0001", 1);
    let peer_b = peer("peer-b-AbCd0002", 1);
    let membership = MembershipSnapshot::new(vec![peer_a.clone(), peer_b.clone()]);

    let mut manager_a = manager(peer_a, 1);
    let mut manager_b = manager(peer_b, 1);
    let work_from_a = manager_a.apply_snapshot(&membership);
    let work_from_b = manager_b.apply_snapshot(&membership);

    let links_from_a = manager_a.links();
    let links_from_b = manager_b.links();
    assert_eq!(links_from_a.len(), 1);
    assert_eq!(links_from_b.len(), 1);
    assert_eq!(links_from_a[0].key(), links_from_b[0].key());
    assert_eq!(
        work_from_a
            .iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) => (lane.key(), lane.local_role()),
            })
            .collect::<Vec<_>>(),
        vec![(links_from_a[0].lanes()[0].key(), RelationRole::Initiator)]
    );
    assert_eq!(
        work_from_b
            .iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) => (lane.key(), lane.local_role()),
            })
            .collect::<Vec<_>>(),
        vec![(links_from_b[0].lanes()[0].key(), RelationRole::Acceptor)]
    );
}

#[test]
fn replica_count_r_produces_r_shared_indexed_lanes() {
    let peer_a = peer("peer-a-AbCd0001", 3);
    let peer_b = peer("peer-b-AbCd0002", 3);
    let membership = MembershipSnapshot::new(vec![peer_a.clone(), peer_b.clone()]);

    let mut manager_a = manager(peer_a, 3);
    let mut manager_b = manager(peer_b, 3);
    manager_a.apply_snapshot(&membership);
    manager_b.apply_snapshot(&membership);

    let lanes_from_a = manager_a.links()[0].lanes().to_vec();
    let lanes_from_b = manager_b.links()[0].lanes().to_vec();
    assert_eq!(lanes_from_a.len(), 3);
    assert_eq!(lanes_from_b.len(), 3);
    assert_eq!(
        lanes_from_a
            .iter()
            .map(|lane| lane.key())
            .collect::<Vec<_>>(),
        lanes_from_b
            .iter()
            .map(|lane| lane.key())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        lanes_from_a
            .iter()
            .map(|lane| lane.index())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn canonical_link_key_selects_one_deterministic_initiator() {
    let peer_a = peer("peer-a-AbCd0001", 1);
    let peer_b = peer("peer-b-AbCd0002", 1);
    let expected_initiator = peer_a.peer_id().clone();
    let membership = MembershipSnapshot::new(vec![peer_b.clone(), peer_a.clone()]);

    let mut manager_a = manager(peer_a, 1);
    let mut manager_b = manager(peer_b, 1);
    manager_a.apply_snapshot(&membership);
    manager_b.apply_snapshot(&membership);

    let link_from_a = &manager_a.links()[0];
    let link_from_b = &manager_b.links()[0];
    assert_eq!(link_from_a.key().initiator(), &expected_initiator);
    assert_eq!(link_from_b.key().initiator(), &expected_initiator);
    assert!(link_from_a.local_is_initiator());
    assert!(!link_from_b.local_is_initiator());
}

#[test]
fn replaying_membership_snapshot_emits_no_duplicate_lane_work() {
    let peer_a = peer("peer-a-AbCd0001", 3);
    let peer_b = peer("peer-b-AbCd0002", 3);
    let membership = MembershipSnapshot::new(vec![peer_a.clone(), peer_b]);
    let mut manager = manager(peer_a, 3);

    let first_reconcile = manager.apply_snapshot(&membership);
    let repeated_reconcile = manager.apply_snapshot(&membership);

    assert_eq!(first_reconcile.len(), 3);
    assert!(repeated_reconcile.is_empty());
}

#[test]
fn one_peer_retry_and_absence_do_not_change_other_peer() {
    let peer_a = peer("peer-a-AbCd0001", 2);
    let peer_b = peer("peer-b-AbCd0002", 2);
    let peer_c = peer("peer-c-AbCd0003", 2);
    let peer_b_id = peer_b.peer_id().clone();
    let peer_c_id = peer_c.peer_id().clone();
    let mut manager = manager(peer_a.clone(), 2);
    manager.apply_snapshot(&MembershipSnapshot::new(vec![
        peer_a.clone(),
        peer_b,
        peer_c.clone(),
    ]));

    assert!(manager.record_retry_failure(&peer_b_id));
    let work = manager.apply_snapshot(&MembershipSnapshot::new(vec![peer_a, peer_c]));

    assert!(
        work.is_empty(),
        "soft absence must not tear down healthy lanes"
    );
    assert_eq!(manager.links().len(), 2);
    let link_b = manager.link(&peer_b_id).expect("B link is retained");
    assert_eq!(link_b.membership(), MembershipState::SuspectMissing);
    assert_eq!(link_b.consecutive_retry_failures(), 1);
    let link_c = manager.link(&peer_c_id).expect("C link remains present");
    assert_eq!(link_c.membership(), MembershipState::Present);
    assert_eq!(link_c.consecutive_retry_failures(), 0);
    assert_eq!(link_c.lanes().len(), 2);
}

#[test]
fn ensure_lane_work_carries_the_exact_replica_relation() {
    let peer_a = peer("peer-a-AbCd0001", 3);
    let peer_b = peer("peer-b-AbCd0002", 2);
    let mut manager = manager(peer_a.clone(), 3);

    let work = manager.apply_snapshot(&MembershipSnapshot::new(vec![peer_a, peer_b]));
    let relations = work
        .iter()
        .map(|command| match command {
            PeerLinkCommand::EnsureLane(lane) => (
                lane.index(),
                lane.local_replica_id(),
                lane.remote_replica_id(),
                lane.local_role(),
            ),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        relations,
        vec![
            (
                0,
                "peer-a-AbCd0001-0",
                "peer-b-AbCd0002-0",
                RelationRole::Initiator,
            ),
            (
                1,
                "peer-a-AbCd0001-1",
                "peer-b-AbCd0002-1",
                RelationRole::Initiator,
            ),
            (
                2,
                "peer-a-AbCd0001-2",
                "peer-b-AbCd0002-0",
                RelationRole::Initiator,
            ),
        ]
    );
}

#[test]
fn configured_replica_count_defines_lane_keys_under_asymmetric_live_views() {
    let peer_a_local = peer_with_indexes("peer-a-AbCd0001", &[0, 2]);
    let peer_b_seen_by_a = peer_with_indexes("peer-b-AbCd0002", &[1]);
    let peer_b_local = peer_with_indexes("peer-b-AbCd0002", &[0, 1]);
    let peer_a_seen_by_b = peer_with_indexes("peer-a-AbCd0001", &[1]);
    let mut manager_a = manager(peer_a_local.clone(), 4);
    let mut manager_b = manager(peer_b_local.clone(), 4);

    manager_a.apply_snapshot(&MembershipSnapshot::new(vec![
        peer_a_local,
        peer_b_seen_by_a,
    ]));
    manager_b.apply_snapshot(&MembershipSnapshot::new(vec![
        peer_a_seen_by_b,
        peer_b_local,
    ]));

    let lane_keys_from_a = manager_a.links()[0]
        .lanes()
        .iter()
        .map(|lane| lane.key().clone())
        .collect::<Vec<_>>();
    let lane_keys_from_b = manager_b.links()[0]
        .lanes()
        .iter()
        .map(|lane| lane.key().clone())
        .collect::<Vec<_>>();
    assert_eq!(lane_keys_from_a, lane_keys_from_b);
    assert_eq!(
        manager_a.links()[0]
            .lanes()
            .iter()
            .map(|lane| lane.index())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn later_complete_replica_view_updates_only_changed_lane_targets() {
    let peer_a = peer("peer-a-AbCd0001", 3);
    let partial_peer_b = peer_with_indexes("peer-b-AbCd0002", &[0]);
    let complete_peer_b = peer("peer-b-AbCd0002", 3);
    let mut manager = manager(peer_a.clone(), 3);

    let initial = manager.apply_snapshot(&MembershipSnapshot::new(vec![
        peer_a.clone(),
        partial_peer_b,
    ]));
    assert_eq!(
        initial
            .iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) =>
                    (lane.index(), lane.remote_replica_id().to_string(),),
            })
            .collect::<Vec<_>>(),
        vec![
            (0, "peer-b-AbCd0002-0".into()),
            (1, "peer-b-AbCd0002-0".into()),
            (2, "peer-b-AbCd0002-0".into()),
        ]
    );

    let upgraded = manager.apply_snapshot(&MembershipSnapshot::new(vec![peer_a, complete_peer_b]));
    assert_eq!(
        upgraded
            .iter()
            .map(|command| match command {
                PeerLinkCommand::EnsureLane(lane) =>
                    (lane.index(), lane.remote_replica_id().to_string(),),
            })
            .collect::<Vec<_>>(),
        vec![
            (1, "peer-b-AbCd0002-1".into()),
            (2, "peer-b-AbCd0002-2".into()),
        ],
        "a later complete snapshot should repair desired lane pairing without rebuilding lane 0"
    );
    assert_eq!(
        manager.links()[0]
            .lanes()
            .iter()
            .map(|lane| (lane.index(), lane.remote_replica_id()))
            .collect::<Vec<_>>(),
        vec![
            (0, "peer-b-AbCd0002-0"),
            (1, "peer-b-AbCd0002-1"),
            (2, "peer-b-AbCd0002-2"),
        ]
    );
}

#[test]
fn absent_peer_retires_only_after_grace_without_direct_or_exact_relay() {
    let peer_a = peer("peer-a-AbCd0001", 1);
    let peer_b = peer("peer-b-AbCd0002", 1);
    let peer_b_id = peer_b.peer_id().clone();
    let present = MembershipSnapshot::new(vec![peer_a.clone(), peer_b]);
    let missing = MembershipSnapshot::new(vec![peer_a.clone()]);
    let started = Instant::now();
    let mut manager = manager(peer_a, 1);

    manager.apply_snapshot_at(&present, started, unavailable);
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(1), unavailable);

    let before_grace =
        manager.apply_snapshot_at(&missing, started + Duration::from_secs(120), unavailable);
    assert!(before_grace.is_empty());
    assert_eq!(
        manager.link(&peer_b_id).map(|link| link.membership()),
        Some(MembershipState::SuspectMissing)
    );

    let at_grace =
        manager.apply_snapshot_at(&missing, started + Duration::from_secs(121), unavailable);
    assert!(at_grace.is_empty());
    assert_eq!(manager.take_retired_peers(), vec![peer_b_id.clone()]);
    assert_eq!(
        manager.link(&peer_b_id).map(|link| link.membership()),
        Some(MembershipState::Retired),
        "retirement remains provisional until the route authority accepts it"
    );
    assert!(manager.confirm_retired_peer(&peer_b_id));
    assert!(manager.link(&peer_b_id).is_none());
}

#[test]
fn healthy_direct_keeps_suspect_peer_routable_beyond_absence_grace() {
    let peer_a = peer("peer-a-AbCd0001", 1);
    let peer_b = peer("peer-b-AbCd0002", 1);
    let peer_b_id = peer_b.peer_id().clone();
    let present = MembershipSnapshot::new(vec![peer_a.clone(), peer_b]);
    let missing = MembershipSnapshot::new(vec![peer_a.clone()]);
    let started = Instant::now();
    let mut manager = manager(peer_a, 1);

    manager.apply_snapshot_at(&present, started, unavailable);
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(1), unavailable);
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(600), |_| {
        PeerConnectivity {
            healthy_direct: true,
            usable_exact_relay: false,
        }
    });

    assert!(manager.take_retired_peers().is_empty());
    assert_eq!(
        manager.link(&peer_b_id).map(|link| link.membership()),
        Some(MembershipState::SuspectMissing)
    );
}

#[test]
fn usable_exact_relay_defers_retirement_after_absence_grace() {
    let peer_a = peer("peer-a-AbCd0001", 1);
    let peer_b = peer("peer-b-AbCd0002", 1);
    let peer_b_id = peer_b.peer_id().clone();
    let present = MembershipSnapshot::new(vec![peer_a.clone(), peer_b]);
    let missing = MembershipSnapshot::new(vec![peer_a.clone()]);
    let started = Instant::now();
    let mut manager = manager(peer_a, 1);

    manager.apply_snapshot_at(&present, started, unavailable);
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(1), unavailable);
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(121), |_| {
        PeerConnectivity {
            healthy_direct: false,
            usable_exact_relay: true,
        }
    });

    assert!(manager.take_retired_peers().is_empty());
    assert_eq!(
        manager.link(&peer_b_id).map(|link| link.membership()),
        Some(MembershipState::SuspectMissing)
    );
}

#[test]
fn reappearing_peer_becomes_present_and_restarts_a_later_absence_grace() {
    let peer_a = peer("peer-a-AbCd0001", 1);
    let peer_b = peer("peer-b-AbCd0002", 1);
    let peer_b_id = peer_b.peer_id().clone();
    let present = MembershipSnapshot::new(vec![peer_a.clone(), peer_b]);
    let missing = MembershipSnapshot::new(vec![peer_a.clone()]);
    let started = Instant::now();
    let mut manager = manager(peer_a, 1);

    assert_eq!(
        manager
            .apply_snapshot_at(&present, started, unavailable)
            .len(),
        1
    );
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(1), unavailable);
    assert!(manager
        .apply_snapshot_at(&present, started + Duration::from_secs(100), unavailable)
        .is_empty());
    assert_eq!(
        manager.link(&peer_b_id).map(|link| link.membership()),
        Some(MembershipState::Present)
    );

    manager.apply_snapshot_at(&missing, started + Duration::from_secs(110), unavailable);
    manager.apply_snapshot_at(&missing, started + Duration::from_secs(121), unavailable);

    assert!(manager.take_retired_peers().is_empty());
    assert!(manager.link(&peer_b_id).is_some());
}
