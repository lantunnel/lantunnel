use std::net::Ipv4Addr;

use base64::Engine as _;
use tp_core::provisioning::{
    encode_managed_resolve_proof_v2, encode_platform_heartbeat_proof_v2, GatewayBootstrapV2,
    PeerBootstrapV2, PeerProfileV2, PlatformHeartbeatPathModeV2, PlatformHeartbeatProofV2,
    ProvisioningError, PublicPeerMembershipV2, TunnelOwnerFileV2,
};

fn gateway() -> GatewayBootstrapV2 {
    GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example.com".into(),
        port: 443,
        mapping_port: None,
        tls_server_name: None,
        trusted_certificate_pem: None,
    }
}

#[test]
fn owner_creates_scope_and_importable_peer() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");

    let scope = owner.scope().expect("derive public scope");
    let peer = owner
        .add_peer(Some(Ipv4Addr::new(198, 18, 23, 7)), 1, Some("pc".into()))
        .expect("add peer");

    assert_eq!(scope.tunnel_id, owner.tunnel_id);
    assert_eq!(peer.tunnel_id, owner.tunnel_id);
    assert_eq!(peer.peer.overlay_ip, Ipv4Addr::new(198, 18, 23, 7));
    assert_eq!(owner.allocated_peers.len(), 1);
    peer.verify().expect("peer profile must verify");
}

#[test]
fn static_gateway_mapping_port_must_be_nonzero_and_not_collide_with_quic_data() {
    let mut zero = gateway();
    zero.mapping_port = Some(0);
    assert_eq!(
        zero.validate(),
        Err(ProvisioningError::InvalidGatewayAddress)
    );

    let mut collision = gateway();
    collision.mapping_port = Some(collision.port);
    assert_eq!(
        collision.validate(),
        Err(ProvisioningError::InvalidGatewayAddress)
    );

    collision.transport = "websocket".into();
    collision
        .validate()
        .expect("TCP data may share the UDP port");

    let mut default_collision = gateway();
    default_collision.port = tp_core::config::DEFAULT_GATEWAY_MAPPING_PROBE_PORT;
    assert_eq!(
        default_collision.validate(),
        Err(ProvisioningError::InvalidGatewayAddress)
    );
}

#[test]
fn mapping_port_changes_preserve_the_tunnel_scope_and_peer_membership() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let scope_before = owner.scope().expect("derive public scope");
    let mut peer = owner.add_peer(None, 1, None).expect("add peer");
    let membership_signature_before = peer.peer.membership_signature.clone();

    owner.static_gateway.mapping_port = Some(10_444);
    owner.verify().expect("updated owner must remain valid");
    let scope_after = owner.scope().expect("derive unchanged public scope");
    assert_eq!(scope_after.tunnel_id, scope_before.tunnel_id);
    assert_eq!(
        scope_after.tunnel_signing_public_key,
        scope_before.tunnel_signing_public_key
    );

    match &mut peer.bootstrap {
        PeerBootstrapV2::StaticGateway(gateway) => gateway.mapping_port = Some(10_444),
        PeerBootstrapV2::ManagedPlatform { .. } => panic!("expected static Gateway bootstrap"),
    }
    peer.verify()
        .expect("mapping port is not part of the Peer membership signature");
    assert_eq!(peer.peer.membership_signature, membership_signature_before);
}

#[test]
fn artifacts_round_trip_without_leaking_owner_key_to_scope() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let scope = owner.scope().expect("scope");
    let peer = owner.add_peer(None, 1, None).expect("peer");

    let owner_yaml = serde_yaml::to_string(&owner).expect("owner yaml");
    let scope_yaml = serde_yaml::to_string(&scope).expect("scope yaml");
    let peer_yaml = serde_yaml::to_string(&peer).expect("peer yaml");

    assert!(owner_yaml.contains("tunnel_signing_private_key"));
    assert!(!scope_yaml.contains("private_key"));
    assert!(!scope_yaml.contains("static_gateway"));
    assert!(peer_yaml.contains("type: static_gateway"));
    assert!(!peer_yaml.contains("!static_gateway"));

    serde_yaml::from_str::<TunnelOwnerFileV2>(&owner_yaml)
        .expect("decode owner")
        .verify()
        .expect("verify owner");
    serde_yaml::from_str::<PeerProfileV2>(&peer_yaml)
        .expect("decode peer")
        .verify()
        .expect("verify peer");
}

#[test]
fn modified_membership_is_rejected() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let mut peer = owner.add_peer(None, 1, None).expect("peer");
    peer.peer.overlay_ip = Ipv4Addr::new(198, 18, 99, 8);

    assert_eq!(
        peer.verify(),
        Err(ProvisioningError::InvalidMembershipSignature)
    );
}

#[test]
fn duplicate_or_out_of_pool_overlay_is_rejected() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    owner
        .add_peer(Some(Ipv4Addr::new(198, 18, 23, 7)), 1, None)
        .expect("first peer");

    assert!(matches!(
        owner.add_peer(Some(Ipv4Addr::new(198, 18, 23, 7)), 1, None),
        Err(ProvisioningError::OverlayUnavailable)
    ));
    assert!(matches!(
        owner.add_peer(Some(Ipv4Addr::new(10, 0, 0, 1)), 1, None),
        Err(ProvisioningError::OverlayUnavailable)
    ));
}

#[test]
fn replica_hint_cannot_exceed_the_gateway_machine_safety_limit() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");

    assert!(matches!(
        owner.add_peer(None, 9, None),
        Err(ProvisioningError::InvalidReplicas)
    ));
}

#[test]
fn unknown_fields_and_private_key_pem_are_rejected() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let peer = owner.add_peer(None, 1, None).expect("peer");
    let mut peer_yaml = serde_yaml::to_string(&peer).expect("peer yaml");
    peer_yaml.push_str("unexpected: true\n");
    assert!(serde_yaml::from_str::<PeerProfileV2>(&peer_yaml).is_err());

    let gateway = GatewayBootstrapV2 {
        trusted_certificate_pem: Some(
            "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n".into(),
        ),
        ..gateway()
    };
    assert!(matches!(
        TunnelOwnerFileV2::generate(gateway),
        Err(ProvisioningError::InvalidCertificatePem) | Err(ProvisioningError::CertificateRead)
    ));
}

#[test]
fn managed_bootstrap_uses_plain_tagged_yaml_fields() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let mut peer = owner.add_peer(None, 1, None).expect("peer");
    peer.bootstrap = PeerBootstrapV2::ManagedPlatform {
        platform_url: "https://lantunnel.example".into(),
    };

    let yaml = serde_yaml::to_string(&peer).expect("peer yaml");
    assert!(yaml.contains("type: managed_platform"));
    assert!(yaml.contains("platform_url: https://lantunnel.example"));
    assert!(!yaml.contains("!managed_platform"));
    serde_yaml::from_str::<PeerProfileV2>(&yaml)
        .expect("decode managed Peer")
        .verify()
        .expect("verify managed Peer");
}

#[test]
fn peer_signs_one_gateway_attachment_challenge() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let peer = owner.add_peer(None, 1, None).expect("peer");
    let membership = peer.public_membership();
    let challenge = [0x5a; 32];
    let signature = peer
        .sign_attachment_proof(&challenge, "runtime-replica-0")
        .expect("sign attachment proof");

    membership
        .verify_attachment_proof(&challenge, "runtime-replica-0", &signature)
        .expect("Peer proof must bind the challenge and runtime Replica");
    assert_eq!(
        membership.verify_attachment_proof(&challenge, "runtime-replica-1", &signature),
        Err(ProvisioningError::InvalidAttachmentProof)
    );
}

#[test]
fn managed_resolve_proof_uses_the_cross_language_canonical_bytes() {
    let membership = PublicPeerMembershipV2 {
        tunnel_id: "4d50ee50-3739-4e68-b726-2be6984a955d".into(),
        peer_id: "36bebb3f-6fda-431a-910b-757584cf5769".into(),
        overlay_ip: Ipv4Addr::new(198, 18, 23, 7),
        peer_public_key: "HA+9KgWIfVG7Awtl1AJGeeNUe4mMfpVCxeDnChS36qo=".into(),
        membership_signature:
            "S8sH6R40FnQcMqB/DiMhB7Gk1TIoEbl0Uh62XL2LsV9HdW1J6nAbla9snBYBx4L+uxDLjE9dB4y03obLTlgYCg=="
                .into(),
    };
    let canonical = encode_managed_resolve_proof_v2(
        &membership,
        1_786_426_560,
        "018f6e84-e11b-7f3a-8cad-9f68f4482001",
    )
    .expect("resolve canonical");
    let actual = canonical
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    assert_eq!(
        actual,
        "000000146c616e74756e6e656c2e7265736f6c76652e7632\
         00000020382a6f6837156483361dcafea6c8d1d2281f414490f07daf4152b58267f1cea3\
         00000008000000006a7ab4c0\
         0000002430313866366538342d653131622d376633612d386361642d396636386634343832303031"
            .replace(' ', "")
    );
}

#[test]
fn managed_resolve_proof_binds_timestamp_and_request_id() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let peer = owner.add_peer(None, 1, None).expect("peer");
    let membership = peer.public_membership();
    let signature = peer
        .sign_managed_resolve_proof(1_786_426_560, "request-a")
        .expect("sign resolve proof");

    membership
        .verify_managed_resolve_proof(1_786_426_560, "request-a", &signature)
        .expect("resolve proof verifies");
    assert_eq!(
        membership.verify_managed_resolve_proof(1_786_426_561, "request-a", &signature),
        Err(ProvisioningError::InvalidManagedResolveProof)
    );
    assert_eq!(
        membership.verify_managed_resolve_proof(1_786_426_560, "request-b", &signature),
        Err(ProvisioningError::InvalidManagedResolveProof)
    );
}

#[test]
fn platform_heartbeat_proof_uses_the_cross_language_canonical_bytes() {
    let input = PlatformHeartbeatProofV2 {
        tunnel_id: "018f6e84-e11b-7f3a-8cad-9f68f4480100",
        peer_id: "018f6e84-e11b-7f3a-8cad-9f68f4480200",
        request_id: "018f6e84-e11b-7f3a-8cad-9f68f4480300",
        timestamp_ms: 1_786_426_560_123,
        client_version: "2.0.0-test",
        final_heartbeat: false,
        transport_active: true,
        path_mode: PlatformHeartbeatPathModeV2::Relay,
    };

    let canonical = encode_platform_heartbeat_proof_v2(&input).expect("heartbeat canonical");
    let actual = canonical
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    assert_eq!(
        actual,
        "0000001f6c616e74756e6e656c2e706c6174666f726d2d6865617274626561742e7632\
         0000002430313866366538342d653131622d376633612d386361642d396636386634343830313030\
         0000002430313866366538342d653131622d376633612d386361642d396636386634343830323030\
         0000002430313866366538342d653131622d376633612d386361642d396636386634343830333030\
         000000080000019fef520e7b\
         0000000a322e302e302d74657374\
         0000000100\
         0000000101\
         0000000572656c6179"
            .replace(' ', "")
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode(canonical),
        "AAAAH2xhbnR1bm5lbC5wbGF0Zm9ybS1oZWFydGJlYXQudjIAAAAkMDE4ZjZlODQtZTExYi03ZjNhLThjYWQtOWY2OGY0NDgwMTAwAAAAJDAxOGY2ZTg0LWUxMWItN2YzYS04Y2FkLTlmNjhmNDQ4MDIwMAAAACQwMThmNmU4NC1lMTFiLTdmM2EtOGNhZC05ZjY4ZjQ0ODAzMDAAAAAIAAABn+9SDnsAAAAKMi4wLjAtdGVzdAAAAAEAAAAAAQEAAAAFcmVsYXk="
    );
}

#[test]
fn platform_heartbeat_proof_binds_the_logical_peer_and_runtime_state() {
    let mut owner = TunnelOwnerFileV2::generate(gateway()).expect("generate tunnel");
    let peer = owner.add_peer(None, 1, None).expect("peer");
    let membership = peer.public_membership();
    let input = PlatformHeartbeatProofV2 {
        tunnel_id: &peer.tunnel_id,
        peer_id: &peer.peer.peer_id,
        request_id: "018f6e84-e11b-7f3a-8cad-9f68f4480300",
        timestamp_ms: 1_786_426_560_123,
        client_version: "2.0.0-test",
        final_heartbeat: false,
        transport_active: true,
        path_mode: PlatformHeartbeatPathModeV2::Direct,
    };
    let signature = peer
        .sign_platform_heartbeat_proof(&input)
        .expect("sign heartbeat proof");

    membership
        .verify_platform_heartbeat_proof(&input, &signature)
        .expect("heartbeat proof verifies");

    let changed = PlatformHeartbeatProofV2 {
        path_mode: PlatformHeartbeatPathModeV2::Relay,
        ..input
    };
    assert_eq!(
        membership.verify_platform_heartbeat_proof(&changed, &signature),
        Err(ProvisioningError::InvalidPlatformHeartbeatProof)
    );
}
