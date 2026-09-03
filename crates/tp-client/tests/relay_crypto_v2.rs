use std::collections::HashSet;
use std::net::Ipv4Addr;

use bytes::{BufMut, BytesMut};
use tp_client::relay_crypto::{
    RelayAadV2, RelayCipherV2, RelayControlPayloadV2, RelayCryptoErrorV2, RelayFlowKindV2,
    RelayFramedKindV2, RelayRecordContextV2, MAX_RELAY_PLAINTEXT_V2, RELAY_NONCE_SIZE_V2,
    RELAY_SEALED_OVERHEAD_V2,
};
use tp_core::protocol::{pack, unpack_bytes, BinaryMessage};

#[test]
fn encrypted_control_plaintext_variants_have_one_strict_codec() {
    let variants = [
        RelayControlPayloadV2::Open {
            network: "tcp".into(),
            address: "198.18.0.2:27015".into(),
        },
        RelayControlPayloadV2::OpenResponse {
            success: true,
            error: String::new(),
        },
        RelayControlPayloadV2::RuntimeRecord(vec![0x01, 0x02, 0x03]),
        RelayControlPayloadV2::Digest([0x77; 32]),
        RelayControlPayloadV2::Need,
    ];

    for expected in variants {
        let encoded = expected.encode().expect("encode control payload");
        assert_eq!(
            RelayControlPayloadV2::decode(&encoded).expect("decode control payload"),
            expected
        );
    }
}

#[test]
fn encrypted_control_plaintext_rejects_unknown_truncated_and_trailing_data() {
    let valid = RelayControlPayloadV2::Open {
        network: "udp".into(),
        address: "198.18.0.3:27016".into(),
    }
    .encode()
    .expect("encode open");

    let invalid = [
        Vec::new(),
        vec![0x7f],
        valid[..valid.len() - 1].to_vec(),
        [valid.as_slice(), &[0]].concat(),
        vec![0x02, 0x02, 0, 0, 0, 0],
        vec![0x05, 0],
    ];
    for encoded in invalid {
        assert_eq!(
            RelayControlPayloadV2::decode(&encoded),
            Err(RelayCryptoErrorV2::InvalidControlPayload)
        );
    }
}

#[test]
fn open_control_accepts_only_tcp_or_udp_and_bounded_strings() {
    for network in ["", "quic", "TCP"] {
        assert_eq!(
            RelayControlPayloadV2::Open {
                network: network.into(),
                address: "198.18.0.2:1".into(),
            }
            .encode(),
            Err(RelayCryptoErrorV2::InvalidControlPayload)
        );
    }
    assert_eq!(
        RelayControlPayloadV2::Open {
            network: "tcp".into(),
            address: "x".repeat(4097),
        }
        .encode(),
        Err(RelayCryptoErrorV2::InvalidControlPayload)
    );
}
use tp_core::p2p_types::{CertFingerprint, SessionId};
use tp_core::peer_link_crypto::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};
use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};

struct CipherPair {
    source: RelayCipherV2,
    target: RelayCipherV2,
    tunnel_id: String,
    session_id: SessionId,
    source_peer_id: String,
    target_peer_id: String,
}

impl CipherPair {
    fn context(&self) -> RelayRecordContextV2<'_> {
        RelayRecordContextV2 {
            tunnel_id: &self.tunnel_id,
            peerlink_session_id: &self.session_id,
            source_peer_id: &self.source_peer_id,
            target_peer_id: &self.target_peer_id,
            conn_id: b"relay-flow-1",
        }
    }
}

fn cipher_pair() -> CipherPair {
    let mut tunnel = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example".into(),
        port: 8443,
        mapping_port: None,
        tls_server_name: Some("gateway.example".into()),
        trusted_certificate_pem: None,
    })
    .expect("generate tunnel");
    let issuer = tunnel.scope().expect("scope").tunnel_signing_public_key;
    let source_profile = tunnel
        .add_peer(Some(Ipv4Addr::new(198, 18, 0, 1)), 1, None)
        .expect("source profile");
    let target_profile = tunnel
        .add_peer(Some(Ipv4Addr::new(198, 18, 0, 2)), 1, None)
        .expect("target profile");
    let source_secret = PeerLinkEphemeralSecretV2::generate();
    let target_secret = PeerLinkEphemeralSecretV2::generate();
    let session_id = SessionId::from_bytes([0x77; 16]);
    let offer = P2pOfferV2::sign(
        &source_profile,
        session_id,
        target_profile.peer.peer_id.clone(),
        Vec::new(),
        CertFingerprint::from_bytes([0x11; 32]),
        &source_secret,
    )
    .expect("sign offer");
    let answer = P2pAnswerV2::sign(
        &target_profile,
        &offer,
        true,
        0,
        Vec::new(),
        CertFingerprint::from_bytes([0x22; 32]),
        &target_secret,
    )
    .expect("sign answer");
    let source_keys = source_secret
        .derive_session_keys(&offer, &answer, &issuer)
        .expect("derive source keys");
    let target_keys = target_secret
        .derive_session_keys(&offer, &answer, &issuer)
        .expect("derive target keys");

    CipherPair {
        source: RelayCipherV2::new(&source_keys),
        target: RelayCipherV2::new(&target_keys),
        tunnel_id: source_profile.tunnel_id,
        session_id,
        source_peer_id: source_profile.peer.peer_id,
        target_peer_id: target_profile.peer.peer_id,
    }
}

#[test]
fn control_round_trip_uses_peerlink_directional_keys() {
    let pair = cipher_pair();
    let context = pair.context();
    let plaintext = b"\x01tcp\0game-server.internal:27015";
    let mut buffer = plaintext.to_vec();

    pair.source
        .seal_control(context, false, &mut buffer)
        .expect("seal control");
    pair.target
        .open_control(context, false, &mut buffer)
        .expect("open control");

    assert_eq!(buffer, plaintext);
}

#[test]
fn framed_data_round_trip_keeps_the_existing_payload_boundary() {
    let pair = cipher_pair();
    let context = pair.context();
    let payload = b"existing framed TCP payload";
    let mut buffer = payload.to_vec();

    pair.source
        .seal_framed(context, RelayFramedKindV2::Data, &mut buffer)
        .expect("seal framed payload");
    pair.target
        .open_framed(context, RelayFramedKindV2::Data, &mut buffer)
        .expect("open framed payload");

    assert_eq!(buffer, payload);
}

#[test]
fn tcp_flow_data_round_trip_uses_the_dedicated_codec_context() {
    let pair = cipher_pair();
    let context = pair.context();
    let record = b"one existing QUIC flow-stream record";
    let mut buffer = record.to_vec();

    pair.source
        .seal_flow(context, RelayFlowKindV2::Data, &mut buffer)
        .expect("seal flow record");
    pair.target
        .open_flow(context, RelayFlowKindV2::Data, &mut buffer)
        .expect("open flow record");

    assert_eq!(buffer, record);
}

#[test]
fn prepared_flow_record_seals_and_opens_without_moving_plaintext() {
    let pair = cipher_pair();
    let context = pair.context();
    let aad = RelayAadV2::flow(context, RelayFlowKindV2::Data).expect("precompute AAD");
    let plaintext = b"caller-owned flow record";
    let mut record = BytesMut::with_capacity(plaintext.len() + RELAY_SEALED_OVERHEAD_V2);
    record.put_bytes(0, RELAY_NONCE_SIZE_V2);
    record.extend_from_slice(plaintext);
    let allocation = record.as_ptr();

    pair.source
        .seal_precomputed(&aad, &mut record)
        .expect("seal prepared flow record");
    assert_eq!(
        record.as_ptr(),
        allocation,
        "seal must reuse caller storage"
    );

    pair.target
        .open_precomputed(&aad, &mut record)
        .expect("open flow record in place");
    assert_eq!(record, plaintext.as_slice());
    assert_eq!(
        record.as_ptr(),
        unsafe { allocation.add(RELAY_NONCE_SIZE_V2) },
        "open must expose the plaintext by advancing the view, not copying it"
    );
}

#[test]
fn precomputed_aad_and_record_allocation_are_reused_across_flow_records() {
    let pair = cipher_pair();
    let aad = RelayAadV2::flow(pair.context(), RelayFlowKindV2::Data).expect("precompute AAD");
    let plaintext = b"steady-state flow record";
    let record_capacity = plaintext.len() + RELAY_SEALED_OVERHEAD_V2;
    let mut record = BytesMut::with_capacity(record_capacity);
    let allocation = record.as_ptr();

    for _ in 0..64 {
        record.clear();
        record.reserve(record_capacity);
        record.put_bytes(0, RELAY_NONCE_SIZE_V2);
        record.extend_from_slice(plaintext);
        assert_eq!(
            record.as_ptr(),
            allocation,
            "record allocation must be reused"
        );

        pair.source
            .seal_precomputed(&aad, &mut record)
            .expect("seal with precomputed AAD");
        pair.target
            .open_precomputed(&aad, &mut record)
            .expect("open with precomputed AAD");
        assert_eq!(record, plaintext.as_slice());
    }
}

#[test]
fn unique_framed_payload_opens_as_a_zero_copy_bytes_view() {
    let pair = cipher_pair();
    let aad = RelayAadV2::framed(pair.context(), RelayFramedKindV2::Data).expect("precompute AAD");
    let plaintext = b"framed ingress payload";
    let mut sealed = BytesMut::with_capacity(plaintext.len() + RELAY_SEALED_OVERHEAD_V2);
    sealed.put_bytes(0, RELAY_NONCE_SIZE_V2);
    sealed.extend_from_slice(plaintext);
    pair.source
        .seal_precomputed(&aad, &mut sealed)
        .expect("seal framed payload");
    let sealed = sealed.freeze();
    let allocation = sealed.as_ptr();

    let opened = pair
        .target
        .open_bytes_precomputed(&aad, sealed)
        .expect("open unique framed payload");

    assert_eq!(opened, plaintext.as_slice());
    assert_eq!(
        opened.as_ptr(),
        unsafe { allocation.add(RELAY_NONCE_SIZE_V2) },
        "unique inbound Bytes must be decrypted and sliced without copying"
    );
}

#[test]
fn decoded_frame_payload_slice_opens_without_copying_the_owner_allocation() {
    let pair = cipher_pair();
    let aad = RelayAadV2::framed(pair.context(), RelayFramedKindV2::Data).expect("precompute AAD");
    let plaintext = b"payload sliced from a decoded transport frame";
    let mut sealed = BytesMut::with_capacity(plaintext.len() + RELAY_SEALED_OVERHEAD_V2);
    sealed.put_bytes(0, RELAY_NONCE_SIZE_V2);
    sealed.extend_from_slice(plaintext);
    pair.source
        .seal_precomputed(&aad, &mut sealed)
        .expect("seal framed payload");

    let frame = pack(&BinaryMessage::Data {
        conn_id: "relay-flow-1".into(),
        payload: sealed.freeze(),
    })
    .to_bytes();
    let payload = match unpack_bytes(frame).expect("decode transport frame") {
        BinaryMessage::Data { conn_id, payload } => {
            assert_eq!(conn_id, "relay-flow-1");
            payload
        }
        other => panic!("expected decoded Data payload, got {other:?}"),
    };
    assert!(
        payload.is_unique(),
        "protocol decode must leave the payload as the sole owner"
    );
    let payload_allocation = payload.as_ptr();

    let opened = pair
        .target
        .open_bytes_precomputed(&aad, payload)
        .expect("open decoded frame payload slice");

    assert_eq!(opened, plaintext.as_slice());
    assert_eq!(
        opened.as_ptr(),
        unsafe { payload_allocation.add(RELAY_NONCE_SIZE_V2) },
        "the sole remaining Bytes slice must retain and decrypt its owner allocation"
    );
}

#[test]
fn tampered_ciphertext_fails_closed() {
    let pair = cipher_pair();
    let context = pair.context();
    let mut buffer = b"private game payload".to_vec();
    pair.source
        .seal_framed(context, RelayFramedKindV2::UdpData, &mut buffer)
        .expect("seal datagram");
    buffer[24] ^= 0x80;

    assert_eq!(
        pair.target
            .open_framed(context, RelayFramedKindV2::UdpData, &mut buffer),
        Err(RelayCryptoErrorV2::AuthenticationFailed)
    );
}

fn assert_authentication_failed(result: Result<(), RelayCryptoErrorV2>) {
    assert_eq!(result, Err(RelayCryptoErrorV2::AuthenticationFailed));
}

#[test]
fn aad_rejects_cross_context_and_cross_carrier_open() {
    let pair = cipher_pair();
    let context = pair.context();
    let mut sealed = b"bound payload".to_vec();
    pair.source
        .seal_framed(context, RelayFramedKindV2::Data, &mut sealed)
        .expect("seal framed payload");

    let other_session = SessionId::from_bytes([0x78; 16]);
    let other_conn = *b"relay-flow-2";
    let contexts = [
        RelayRecordContextV2 {
            tunnel_id: "other-tunnel",
            ..context
        },
        RelayRecordContextV2 {
            peerlink_session_id: &other_session,
            ..context
        },
        RelayRecordContextV2 {
            conn_id: &other_conn,
            ..context
        },
    ];
    for changed in contexts {
        let mut candidate = sealed.clone();
        assert_authentication_failed(pair.target.open_framed(
            changed,
            RelayFramedKindV2::Data,
            &mut candidate,
        ));
    }

    let mut wrong_message_kind = sealed.clone();
    assert_authentication_failed(pair.target.open_framed(
        context,
        RelayFramedKindV2::UdpData,
        &mut wrong_message_kind,
    ));
    let mut wrong_carrier = sealed;
    assert_authentication_failed(pair.target.open_flow(
        context,
        RelayFlowKindV2::Data,
        &mut wrong_carrier,
    ));
}

#[test]
fn wrong_direction_key_and_peer_order_fail_authentication() {
    let pair = cipher_pair();
    let context = pair.context();
    let mut sealed = b"source to target".to_vec();
    pair.source
        .seal_control(context, false, &mut sealed)
        .expect("seal source-to-target control");

    let mut wrong_local_key = sealed.clone();
    assert_authentication_failed(
        pair.source
            .open_control(context, false, &mut wrong_local_key),
    );

    let reversed = RelayRecordContextV2 {
        source_peer_id: &pair.target_peer_id,
        target_peer_id: &pair.source_peer_id,
        ..context
    };
    assert_authentication_failed(pair.target.open_control(reversed, false, &mut sealed));
}

#[test]
fn route_abort_bit_is_authenticated() {
    let pair = cipher_pair();
    let context = pair.context();
    let mut sealed = b"\x02open rejected".to_vec();
    pair.source
        .seal_control(context, false, &mut sealed)
        .expect("seal control");

    assert_authentication_failed(pair.target.open_control(context, true, &mut sealed));
}

#[test]
fn udp_data_round_trip_uses_the_existing_datagram_payload_boundary() {
    let pair = cipher_pair();
    let context = pair.context();
    let payload = vec![0x5a; 1385];
    let mut buffer = payload.clone();

    pair.source
        .seal_framed(context, RelayFramedKindV2::UdpData, &mut buffer)
        .expect("seal UDP payload");
    pair.target
        .open_framed(context, RelayFramedKindV2::UdpData, &mut buffer)
        .expect("open UDP payload");

    assert_eq!(buffer, payload);
}

#[test]
fn flow_open_and_response_use_distinct_directional_domains() {
    let pair = cipher_pair();
    let source_to_target = pair.context();
    let mut open = b"game-server.internal:27015".to_vec();
    pair.source
        .seal_flow(source_to_target, RelayFlowKindV2::Open, &mut open)
        .expect("seal flow OPEN");
    let mut wrong_domain = open.clone();
    assert_authentication_failed(pair.target.open_flow(
        source_to_target,
        RelayFlowKindV2::OpenResponse,
        &mut wrong_domain,
    ));
    pair.target
        .open_flow(source_to_target, RelayFlowKindV2::Open, &mut open)
        .expect("open flow OPEN");

    let target_to_source = RelayRecordContextV2 {
        source_peer_id: &pair.target_peer_id,
        target_peer_id: &pair.source_peer_id,
        ..source_to_target
    };
    let mut response = b"success".to_vec();
    pair.target
        .seal_flow(
            target_to_source,
            RelayFlowKindV2::OpenResponse,
            &mut response,
        )
        .expect("seal flow OPEN response");
    pair.source
        .open_flow(
            target_to_source,
            RelayFlowKindV2::OpenResponse,
            &mut response,
        )
        .expect("open flow OPEN response");

    assert_eq!(open, b"game-server.internal:27015");
    assert_eq!(response, b"success");
}

#[test]
fn empty_and_maximum_records_round_trip_and_oversize_is_rejected() {
    let pair = cipher_pair();
    let context = pair.context();

    let mut empty = Vec::new();
    pair.source
        .seal_flow(context, RelayFlowKindV2::Data, &mut empty)
        .expect("seal empty record");
    assert_eq!(empty.len(), RELAY_SEALED_OVERHEAD_V2);
    pair.target
        .open_flow(context, RelayFlowKindV2::Data, &mut empty)
        .expect("open empty record");
    assert!(empty.is_empty());

    let mut maximum = vec![0xa5; MAX_RELAY_PLAINTEXT_V2];
    pair.source
        .seal_flow(context, RelayFlowKindV2::Data, &mut maximum)
        .expect("seal maximum record");
    assert_eq!(
        maximum.len(),
        MAX_RELAY_PLAINTEXT_V2 + RELAY_SEALED_OVERHEAD_V2
    );
    pair.target
        .open_flow(context, RelayFlowKindV2::Data, &mut maximum)
        .expect("open maximum record");
    assert_eq!(maximum, vec![0xa5; MAX_RELAY_PLAINTEXT_V2]);

    let mut oversized = vec![0; MAX_RELAY_PLAINTEXT_V2 + 1];
    assert_eq!(
        pair.source
            .seal_flow(context, RelayFlowKindV2::Data, &mut oversized),
        Err(RelayCryptoErrorV2::PlaintextTooLarge)
    );
    let mut invalid_blob = vec![0; RELAY_SEALED_OVERHEAD_V2 - 1];
    assert_eq!(
        pair.target
            .open_flow(context, RelayFlowKindV2::Data, &mut invalid_blob),
        Err(RelayCryptoErrorV2::InvalidSealedLength)
    );
}

#[test]
fn random_nonce_smoke_test_does_not_repeat_across_records() {
    let pair = cipher_pair();
    let context = pair.context();
    let mut nonces = HashSet::new();

    for _ in 0..128 {
        let mut sealed = b"same plaintext".to_vec();
        pair.source
            .seal_framed(context, RelayFramedKindV2::Data, &mut sealed)
            .expect("seal payload");
        assert!(nonces.insert(sealed[..24].to_vec()));
    }
}
