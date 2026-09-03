use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context};
use tp_core::protocol::BinaryMessage;
use tp_core::provisioning::PeerProfileV2;
use tp_transport::{AuthParams, Session};
use uuid::Uuid;

const V2_ATTACHMENT_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn runtime_replica_ids(profile: &PeerProfileV2) -> Vec<String> {
    let random = Uuid::new_v4().simple().to_string();
    let family = format!("{}-{}", profile.tunnel_id, &random[..8]);
    (0..profile.replicas)
        .map(|index| format!("{family}-{index}"))
        .collect()
}

pub(crate) fn v2_auth_params(
    profile: &PeerProfileV2,
    replica_id: String,
    gateway_port: u16,
) -> AuthParams {
    AuthParams {
        tunnel_id: profile.tunnel_id.clone(),
        client_id: replica_id,
        capabilities: tp_core::protocol::TransportCapabilities {
            route_bind_control_v1: true,
            tcp_flow_stream_v1: true,
            relay_source_attestation_v1: true,
            peer_mesh_v2: true,
        },
        group_id: String::new(),
        username: String::new(),
        password: String::new(),
        group_password: String::new(),
        role: Default::default(),
        peer_addr: SocketAddr::from(([0, 0, 0, 0], gateway_port)),
    }
}

pub(crate) async fn complete_v2_gateway_attachment(
    session: &mut Session,
    profile: &PeerProfileV2,
    replica_id: &str,
) -> anyhow::Result<()> {
    let result = async {
        let first = tokio::time::timeout(V2_ATTACHMENT_CHALLENGE_TIMEOUT, session.recv())
            .await
            .context("Gateway V2 attachment challenge timed out")?;
        let Some(first) = first else {
            bail!("Gateway closed before V2 attachment challenge");
        };
        let BinaryMessage::AuthV2Challenge { challenge } = first else {
            bail!("Gateway did not send a V2 attachment challenge");
        };
        let signature = profile
            .sign_attachment_proof(&challenge, replica_id)
            .context("could not sign Gateway V2 attachment proof")?;
        session
            .send(BinaryMessage::AuthV2Proof {
                membership: profile.public_membership(),
                signature,
            })
            .await
            .context("could not send Gateway V2 attachment proof")
    }
    .await;
    if result.is_err() {
        session.close();
    }
    result
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use tp_core::protocol::{unpack, BinaryMessage};
    use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};
    use tp_transport::Session;

    fn profile() -> tp_core::provisioning::PeerProfileV2 {
        TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: None,
            trusted_certificate_pem: None,
        })
        .expect("Tunnel")
        .add_peer(None, 1, None)
        .expect("Peer")
    }

    fn session() -> (
        Session,
        mpsc::Sender<BinaryMessage>,
        mpsc::Receiver<tp_core::protocol::PackedMessage>,
        Arc<AtomicBool>,
    ) {
        let (out_tx, out_rx) = mpsc::channel(4);
        let (in_tx, in_rx) = mpsc::channel(4);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_for_session = closed.clone();
        let session = Session::new_channeled(
            out_tx,
            in_rx,
            SocketAddr::from(([127, 0, 0, 1], 8443)),
            Arc::new(move || closed_for_session.store(true, Ordering::SeqCst)),
            tokio::spawn(async {}),
            tokio::spawn(async {}),
        );
        (session, in_tx, out_rx, closed)
    }

    #[tokio::test]
    async fn answers_gateway_challenge_with_a_verifiable_peer_proof() {
        let profile = profile();
        let replica_id = format!("{}-AbC12345-0", profile.tunnel_id);
        let challenge = [0x42; 32];
        let (mut session, inbound, mut outbound, _closed) = session();
        inbound
            .send(BinaryMessage::AuthV2Challenge { challenge })
            .await
            .expect("challenge");

        super::complete_v2_gateway_attachment(&mut session, &profile, &replica_id)
            .await
            .expect("attachment proof");

        let proof =
            unpack(&outbound.recv().await.expect("proof frame").to_bytes()).expect("decode proof");
        let BinaryMessage::AuthV2Proof {
            membership,
            signature,
        } = proof
        else {
            panic!("unexpected proof message: {proof:?}");
        };
        assert_eq!(membership, profile.public_membership());
        membership
            .verify_attachment_proof(&challenge, &replica_id, &signature)
            .expect("proof verifies");
    }

    #[tokio::test]
    async fn rejects_and_closes_a_session_when_the_first_message_is_not_a_challenge() {
        let profile = profile();
        let private_key = profile.peer.peer_private_key.clone();
        let replica_id = format!("{}-AbC12345-0", profile.tunnel_id);
        let (mut session, inbound, _outbound, closed) = session();
        inbound
            .send(BinaryMessage::Heartbeat {
                client_id: "not-a-challenge".into(),
                timestamp: 42,
            })
            .await
            .expect("unexpected first message");

        let error = super::complete_v2_gateway_attachment(&mut session, &profile, &replica_id)
            .await
            .expect_err("non-challenge must fail")
            .to_string();

        assert!(closed.load(Ordering::SeqCst));
        assert!(!error.contains(private_key.as_str()));
    }

    #[tokio::test]
    async fn reports_a_gateway_close_before_the_challenge() {
        let profile = profile();
        let replica_id = format!("{}-AbC12345-0", profile.tunnel_id);
        let (mut session, inbound, _outbound, closed) = session();
        drop(inbound);

        let error = super::complete_v2_gateway_attachment(&mut session, &profile, &replica_id)
            .await
            .expect_err("closed challenge stream must fail")
            .to_string();

        assert_eq!(error, "Gateway closed before V2 attachment challenge");
        assert!(closed.load(Ordering::SeqCst));
    }

    #[test]
    fn initial_auth_uses_v2_identity_and_existing_transport_capabilities() {
        let profile = profile();
        let replica_id = format!("{}-AbC12345-0", profile.tunnel_id);

        let auth = super::v2_auth_params(&profile, replica_id.clone(), 8443);

        assert_eq!(auth.tunnel_id, profile.tunnel_id);
        assert_eq!(auth.client_id, replica_id);
        assert!(auth.capabilities.peer_mesh_v2);
        assert!(auth.capabilities.route_bind_control_v1);
        assert!(auth.capabilities.tcp_flow_stream_v1);
        assert!(auth.capabilities.relay_source_attestation_v1);
        assert_eq!(auth.group_id, "");
        assert_eq!(auth.username, "");
        assert_eq!(auth.password, "");
        assert_eq!(auth.group_password, "");
    }

    #[test]
    fn runtime_replica_handles_use_the_existing_family_format() {
        let mut profile = profile();
        profile.replicas = 3;

        let ids = super::runtime_replica_ids(&profile);

        assert_eq!(ids.len(), 3);
        for (index, id) in ids.iter().enumerate() {
            assert_ne!(id, &profile.peer.peer_id);
            assert!(crate::p2p::replica::replica_seed_for_tunnel(&profile.tunnel_id, id).is_some());
            assert_eq!(crate::p2p::replica::replica_index(id), Some(index));
        }
    }
}
