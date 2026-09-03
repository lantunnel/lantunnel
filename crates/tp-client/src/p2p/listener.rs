//! PC-side P2P QUIC listener. As a simplification, PC binds
//! a separate quinn-bound socket from the probe socket used by the
//! responder loop in `manager.rs::spawn_punch_responder`. Initiator dials
//! the quinn-bound port; cert-fingerprint validation pins the peer
//! identity to the keyed expected-peer map stamped by the acceptor's
//! `P2pOffer` arm.
//!
//! ALPN: PeerLinks deliberately skip ALPN on both client (`tls.rs`) and server
//! (this module). Cert-fingerprint pinning already prevents cross-talk
//! with non-P2P QUIC services on the same host (any other service has a
//! different cert and so a different fingerprint), and skipping ALPN
//! avoids a second axis of upgrade coordination across PC/Mobile builds.
//! Revisit if a real conflict emerges (e.g. another QUIC service binds a
//! port we'd realistically share).

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;

use quinn::{Endpoint, EndpointConfig, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms},
    DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme,
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tp_core::p2p_types::SessionId;
use tp_metrics::MetricsManager;
use tp_transport::session::Session;

use crate::p2p::cert::CertBundle;
use crate::p2p::expected::{ExpectedPeerMap, ExpectedPeerMatchError};

/// PC-side P2P QUIC listener. Owns a quinn `Endpoint` bound to a UDP
/// socket distinct from the probe socket used by the punch responder.
pub struct P2pListener {
    endpoint: Endpoint,
    listen_addr: SocketAddr,
    probe_socket: std::net::UdpSocket,
    mapping_probe_observed: Option<SocketAddr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct P2pUnderlayInterfaceIndexes {
    pub(crate) ipv4: Option<NonZeroU32>,
    pub(crate) ipv6: Option<NonZeroU32>,
    pub(crate) ipv4_source_ip: Option<std::net::Ipv4Addr>,
}

impl P2pUnderlayInterfaceIndexes {
    fn required_for_addr(self, addr: SocketAddr) -> std::io::Result<NonZeroU32> {
        let index = match addr {
            SocketAddr::V4(_) => self.ipv4,
            SocketAddr::V6(_) => self.ipv6,
        };
        index.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "selected P2P underlay adapter has no index for the requested address family",
            )
        })
    }

    pub(crate) fn bind_addr_for(self, addr: SocketAddr) -> std::io::Result<SocketAddr> {
        #[cfg(target_os = "macos")]
        if let SocketAddr::V4(addr) = addr {
            let source_ip = self.ipv4_source_ip.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "selected P2P underlay adapter has no exact IPv4 source address",
                )
            })?;
            return Ok(SocketAddr::new(source_ip.into(), addr.port()));
        }
        Ok(addr)
    }
}

impl P2pListener {
    /// Bind a quinn-server endpoint with the given P2P cert bundle.
    /// Returns the listener and its public-facing UDP local address;
    /// the caller (Task 4.11) uses `listen_addr.port()` as the
    /// `p2p_local_port` shared with `P2pManager` so it can be
    /// announced in `P2pAnnounce`.
    pub fn bind(bundle: &CertBundle) -> std::io::Result<Self> {
        Self::bind_with_mapping_probe(
            bundle,
            crate::p2p::mapping_probe::mapping_probe_addr_from_env(),
        )
    }

    pub(crate) fn bind_with_mapping_probe(
        bundle: &CertBundle,
        mapping_probe_reflector: Option<SocketAddr>,
    ) -> std::io::Result<Self> {
        Self::bind_with_mapping_probe_on_interfaces(bundle, mapping_probe_reflector, None)
    }

    #[cfg(test)]
    pub(crate) fn bind_with_mapping_probe_on_interface(
        bundle: &CertBundle,
        mapping_probe_reflector: Option<SocketAddr>,
        underlay_interface_index: Option<NonZeroU32>,
    ) -> std::io::Result<Self> {
        Self::bind_with_mapping_probe_on_interfaces(
            bundle,
            mapping_probe_reflector,
            underlay_interface_index.map(|index| P2pUnderlayInterfaceIndexes {
                ipv4: Some(index),
                ipv6: Some(index),
                ipv4_source_ip: Some(std::net::Ipv4Addr::LOCALHOST),
            }),
        )
    }

    pub(crate) fn bind_with_mapping_probe_on_interfaces(
        bundle: &CertBundle,
        mapping_probe_reflector: Option<SocketAddr>,
        underlay_interface_indexes: Option<P2pUnderlayInterfaceIndexes>,
    ) -> std::io::Result<Self> {
        let bind_on_underlay = |addr: SocketAddr| {
            let index = underlay_interface_indexes
                .map(|indexes| indexes.required_for_addr(addr))
                .transpose()?;
            let addr = underlay_interface_indexes
                .map(|indexes| indexes.bind_addr_for(addr))
                .transpose()?
                .unwrap_or(addr);
            tp_transport::quic::bind_tuned_udp_on_interface(addr, index)
        };
        let std_sock = match (mapping_probe_reflector, underlay_interface_indexes) {
            (Some(addr), Some(_)) if addr.is_ipv4() => {
                bind_on_underlay("0.0.0.0:0".parse().unwrap())?
            }
            (Some(_), Some(_)) => bind_on_underlay("[::]:0".parse().unwrap())?,
            (Some(addr), None) if addr.is_ipv4() => {
                tp_transport::quic::bind_tuned_udp_on_interface("0.0.0.0:0".parse().unwrap(), None)
                    .or_else(|_| {
                        tp_transport::quic::bind_tuned_udp_on_interface(
                            "[::]:0".parse().unwrap(),
                            None,
                        )
                    })?
            }
            (Some(_), None) => {
                tp_transport::quic::bind_tuned_udp_on_interface("[::]:0".parse().unwrap(), None)
                    .or_else(|_| {
                        tp_transport::quic::bind_tuned_udp_on_interface(
                            "0.0.0.0:0".parse().unwrap(),
                            None,
                        )
                    })?
            }
            (None, Some(indexes)) => {
                let addr = if indexes.ipv6.is_some() {
                    "[::]:0".parse().unwrap()
                } else if indexes.ipv4.is_some() {
                    "0.0.0.0:0".parse().unwrap()
                } else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "selected P2P underlay adapter has no usable interface index",
                    ));
                };
                bind_on_underlay(addr)?
            }
            (None, None) => {
                tp_transport::quic::bind_tuned_udp_on_interface("[::]:0".parse().unwrap(), None)
                    .or_else(|_| {
                        tp_transport::quic::bind_tuned_udp_on_interface(
                            "0.0.0.0:0".parse().unwrap(),
                            None,
                        )
                    })?
            }
        };
        let listen_addr = std_sock.local_addr()?;
        let probe_socket = std_sock.try_clone()?;
        let mapping_probe_observed = mapping_probe_reflector.and_then(|reflector| {
            let label = format!("listener:{}", listen_addr.port());
            match crate::p2p::mapping_probe::probe_std_socket_public_endpoint(
                &probe_socket,
                reflector,
                &label,
                crate::p2p::mapping_probe::DEFAULT_MAPPING_PROBE_TIMEOUT,
            ) {
                Ok(Some(observed)) => {
                    tracing::info!(
                        local_port = listen_addr.port(),
                        observed = %observed,
                        reflector = %reflector,
                        "p2p listener mapping probe ok"
                    );
                    Some(observed)
                }
                Ok(None) => {
                    tracing::debug!(
                        local_port = listen_addr.port(),
                        reflector = %reflector,
                        "p2p listener mapping probe unavailable before endpoint start"
                    );
                    None
                }
                Err(e) => {
                    tracing::debug!(
                        local_port = listen_addr.port(),
                        reflector = %reflector,
                        error = %e,
                        "p2p listener mapping probe failed before endpoint start"
                    );
                    None
                }
            }
        });
        let endpoint = endpoint_from_socket(bundle, std_sock)?;
        Ok(Self {
            endpoint,
            listen_addr,
            probe_socket,
            mapping_probe_observed,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn probe_socket(&self) -> std::io::Result<std::net::UdpSocket> {
        self.probe_socket.try_clone()
    }

    pub fn mapping_probe_observed(&self) -> Option<SocketAddr> {
        self.mapping_probe_observed
    }

    pub fn into_parts(self) -> (Endpoint, SocketAddr) {
        (self.endpoint, self.listen_addr)
    }
}

pub(crate) fn endpoint_from_socket(
    bundle: &CertBundle,
    std_sock: std::net::UdpSocket,
) -> std::io::Result<Endpoint> {
    let mut server_cfg = make_p2p_server_config(&bundle.cert_der, &bundle.key_der)
        .map_err(|e| std::io::Error::other(format!("server config: {e}")))?;
    server_cfg.transport_config(Arc::new(tp_transport::quic::tuned_transport_config(
        &crate::p2p::p2p_quic_tuning(),
    )));
    let runtime = quinn::default_runtime().ok_or_else(|| std::io::Error::other("no runtime"))?;
    Endpoint::new(
        EndpointConfig::default(),
        Some(server_cfg),
        std_sock,
        runtime,
    )
}

pub(crate) fn make_p2p_server_config(
    cert_der: &[u8],
    key_der: &[u8],
) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cert = CertificateDer::from(cert_der.to_vec());
    let key = PrivatePkcs8KeyDer::from(key_der.to_vec());
    let crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(RequireAnyClientCert::new()))
        .with_single_cert(vec![cert], key.into())?;
    let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?;
    Ok(ServerConfig::with_crypto(Arc::new(qsc)))
}

#[derive(Debug)]
struct RequireAnyClientCert {
    supported: WebPkiSupportedAlgorithms,
    subjects: Vec<DistinguishedName>,
}

impl RequireAnyClientCert {
    fn new() -> Self {
        Self {
            supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
            subjects: Vec::new(),
        }
    }
}

impl ClientCertVerifier for RequireAnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.subjects
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        if end_entity.as_ref().is_empty() {
            return Err(TlsError::General("empty p2p client cert".into()));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// Drive `endpoint.accept()` indefinitely. For each incoming, validate
/// the peer cert fingerprint against a unique expected map entry
/// (stamped by `P2pManager` when it received the P2pOffer). On match,
/// accept one bi-stream and call `on_session(session_id, Session)`; on
/// mismatch or ambiguity close the connection.
pub async fn run_listener_loop(
    endpoint: Endpoint,
    expected_peers: ExpectedPeerMap,
    on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync>,
    cancel: CancellationToken,
    metrics: Option<Arc<MetricsManager>>,
) {
    let accept_tasks = TaskTracker::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                endpoint.close(0u32.into(), b"listener-cancelled");
                break;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let expected_peers = expected_peers.clone();
                let on_session = on_session.clone();
                let metrics = metrics.clone();
                let task_cancel = cancel.clone();
                accept_tasks.spawn(async move {
                    tokio::select! {
                        _ = task_cancel.cancelled() => {}
                        accepted = accept_incoming_session(incoming, expected_peers, metrics) => {
                            if let Some((session_id, session)) = accepted {
                                if !task_cancel.is_cancelled() {
                                    on_session(session_id, session);
                                }
                            }
                        }
                    }
                });
            }
        }
    }
    accept_tasks.close();
    accept_tasks.wait().await;
}

pub(crate) async fn accept_one_session(
    endpoint: Endpoint,
    expected_peers: ExpectedPeerMap,
    metrics: Option<Arc<MetricsManager>>,
    timeout: std::time::Duration,
) -> Option<(SessionId, Session)> {
    let incoming = match tokio::time::timeout(timeout, endpoint.accept()).await {
        Ok(Some(incoming)) => incoming,
        Ok(None) => return None,
        Err(_) => {
            tracing::debug!("p2p one-shot listener timed out waiting for QUIC");
            return None;
        }
    };
    accept_incoming_session(incoming, expected_peers, metrics).await
}

async fn accept_incoming_session(
    incoming: quinn::Incoming,
    expected_peers: ExpectedPeerMap,
    metrics: Option<Arc<MetricsManager>>,
) -> Option<(SessionId, Session)> {
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, "p2p incoming failed");
            return None;
        }
    };
    // Pull peer's leaf cert and verify SHA-256 first 32 bytes match.
    let Some(peer_id) = conn.peer_identity() else {
        tracing::warn!("p2p peer has no identity; closing");
        conn.close(0u32.into(), b"no-peer-cert");
        return None;
    };
    let leaf_der = match peer_id.downcast::<Vec<CertificateDer<'static>>>() {
        Ok(boxed) => match boxed.first().cloned() {
            Some(c) => c,
            None => {
                tracing::warn!("p2p peer cert chain empty; closing");
                conn.close(0u32.into(), b"empty-cert-chain");
                return None;
            }
        },
        Err(_) => {
            tracing::warn!("p2p peer identity is not Vec<CertificateDer>; closing");
            conn.close(0u32.into(), b"unknown-peer-id");
            return None;
        }
    };
    let actual_fp = crate::p2p::cert::sha256_fingerprint(leaf_der.as_ref());
    match expected_peers.match_unique_by_cert_fp(actual_fp) {
        Ok(Some(_)) | Err(ExpectedPeerMatchError::Ambiguous { .. }) => {}
        Ok(None) => {
            tracing::warn!(
                actual = ?actual_fp.as_bytes(),
                "p2p peer cert did not match any expected peer; closing"
            );
            if let Some(m) = metrics.as_ref() {
                m.incr_p2p_cert_mismatch();
            }
            conn.close(0u32.into(), b"fp-mismatch");
            return None;
        }
    }
    // Accept one bi-stream and wrap as Session.
    let (send, mut recv) = match conn.accept_bi().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, "p2p accept_bi failed");
            return None;
        }
    };
    let stream_session_id = match read_stream_session_preface(&mut recv).await {
        Ok(session_id) => session_id,
        Err(e) => {
            tracing::warn!(error = %e, "p2p stream session preface invalid; closing");
            if let Some(m) = metrics.as_ref() {
                m.incr_p2p_cert_mismatch();
            }
            conn.close(0u32.into(), b"bad-session-preface");
            return None;
        }
    };
    let session_id = match expected_peers.take_by_session_and_cert_fp(stream_session_id, actual_fp)
    {
        Some(matched) => matched.session_id,
        None => {
            tracing::warn!(
                actual = ?actual_fp.as_bytes(),
                ?stream_session_id,
                "p2p stream session preface did not match expected cert; closing"
            );
            if let Some(m) = metrics.as_ref() {
                m.incr_p2p_cert_mismatch();
            }
            conn.close(0u32.into(), b"fp-mismatch");
            return None;
        }
    };
    let control = tp_transport::quic::accept_p2p_control_lane(&conn).await;
    let session = tp_transport::quic::wrap_for_p2p_with_control(conn, send, recv, control);
    Some((session_id, session))
}

pub(crate) async fn read_stream_session_preface(
    recv: &mut quinn::RecvStream,
) -> Result<SessionId, String> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read len: {e}"))?;
    let len = u32::from_be_bytes(len_buf);
    if len > tp_transport::MAX_FRAME_LEN {
        return Err(format!("frame too large: {len}"));
    }
    let mut body = vec![0u8; len as usize];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    match tp_core::protocol::unpack(&body).map_err(|e| e.to_string())? {
        tp_core::protocol::BinaryMessage::P2pSessionReady { session_id, .. } => Ok(session_id),
        other => Err(format!("unexpected preface message: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_fails_closed_when_requested_underlay_interface_is_invalid() {
        let bundle = crate::p2p::cert::generate_self_signed_cert("listener-underlay-test")
            .expect("test certificate");
        let invalid = std::num::NonZeroU32::new(u32::MAX).unwrap();

        assert!(
            P2pListener::bind_with_mapping_probe_on_interface(&bundle, None, Some(invalid),)
                .is_err()
        );
    }

    #[test]
    fn listener_does_not_borrow_the_other_family_underlay_index() {
        let bundle = crate::p2p::cert::generate_self_signed_cert("listener-family-index-test")
            .expect("test certificate");
        let indexes = P2pUnderlayInterfaceIndexes {
            ipv4: NonZeroU32::new(7),
            ipv6: None,
            ipv4_source_ip: Some(std::net::Ipv4Addr::LOCALHOST),
        };

        let error = match P2pListener::bind_with_mapping_probe_on_interfaces(
            &bundle,
            Some("[::1]:3479".parse().unwrap()),
            Some(indexes),
        ) {
            Ok(_) => panic!("an IPv6 listener cannot borrow the selected adapter's IPv4 index"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
    use crate::p2p::cert::generate_self_signed_cert;
    use crate::p2p::tls::make_pinned_client_config_with_identity;
    use std::time::Duration;
    use tokio::sync::mpsc;

    // P2pListener requires a real peer with a matching cert to drive accept;
    // cert-fp validation paths are exercised by Phase 5 e2e (Task 5.1, 5.3).
    // This smoke test only verifies bind + local_addr shape.
    #[test]
    fn bind_then_local_addr_is_dual_stack_wildcard() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let bundle = generate_self_signed_cert("p2p").expect("cert");
            let listener = P2pListener::bind(&bundle).expect("bind");
            let addr = listener.local_addr();
            assert!(
                addr.is_ipv6(),
                "expected ipv6 wildcard listen addr, got {addr}"
            );
            assert_ne!(addr.port(), 0, "kernel should have assigned a port");
        });
    }

    #[test]
    fn p2p_listener_endpoint_uses_tuned_transport_keepalive() {
        let source = include_str!("listener.rs");
        let needle = [
            "server_cfg",
            ".transport_config(Arc::new(tp_transport::quic::tuned_transport_config(",
        ]
        .concat();

        assert!(
            source.contains(&needle),
            "P2P listener QUIC config must use the shared keepalive/idle tuning"
        );
    }

    #[tokio::test]
    async fn listener_accepts_peer_with_matching_cert_identity() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::SessionId;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-server").expect("server cert");
        let initiator_bundle =
            generate_self_signed_cert("p2p-listener-initiator").expect("initiator cert");
        let listener = P2pListener::bind(&server_bundle).expect("listener bind");
        let listen_port = listener.local_addr().port();
        let (endpoint, _) = listener.into_parts();

        let expected = ExpectedPeerMap::default();
        let session_id = SessionId::from_bytes([9u8; 16]);
        expected.insert(
            session_id,
            ExpectedPeer {
                peer_client_id: "initiator".into(),
                cert_fp: initiator_bundle.fingerprint,
                candidates: vec![],
            },
        );
        let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> =
            Arc::new(move |_session_id, _session| {
                let _ = accepted_tx.try_send(());
            });
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            expected.clone(),
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let runtime = quinn::default_runtime().expect("quinn runtime");
        let mut client =
            Endpoint::new(EndpointConfig::default(), None, sock, runtime).expect("client endpoint");
        client.set_default_client_config(make_pinned_client_config_with_identity(
            server_bundle.fingerprint,
            &initiator_bundle,
        ));
        let peer = SocketAddr::from(([127, 0, 0, 1], listen_port));
        let conn = client
            .connect(peer, "p2p")
            .expect("connect start")
            .await
            .expect("connect");
        let (mut send, _recv) = conn.open_bi().await.expect("open bi");
        write_test_stream_preface(&mut send, session_id).await;

        tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
            .await
            .expect("listener should accept before timeout")
            .expect("listener accepted channel closed");
        assert!(
            expected.get(session_id).is_none(),
            "listener must consume the matched expected entry after accept"
        );

        client.close(0u32.into(), b"done");
        listener_task.abort();
    }

    #[tokio::test]
    async fn listener_loop_exits_promptly_when_cancelled() {
        use crate::p2p::expected::ExpectedPeerMap;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-cancel").expect("server cert");
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("listener udp bind");
        let endpoint = endpoint_from_socket(&server_bundle, sock).expect("listener endpoint");
        let cancel = tokio_util::sync::CancellationToken::new();
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> = Arc::new(|_, _| {});
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            ExpectedPeerMap::default(),
            on_session,
            cancel.clone(),
            None,
        ));

        tokio::time::sleep(Duration::from_millis(25)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_millis(500), listener_task)
            .await
            .expect("listener loop should exit promptly after cancellation")
            .expect("listener task should not panic");
    }

    #[tokio::test]
    async fn listener_matches_unique_session_by_cert_fingerprint() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::{CandidateKind, SessionId};

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-server").expect("server cert");
        let peer_a = generate_self_signed_cert("p2p-listener-peer-a").expect("peer a cert");
        let peer_b = generate_self_signed_cert("p2p-listener-peer-b").expect("peer b cert");
        let listener = P2pListener::bind(&server_bundle).expect("listener bind");
        let listen_port = listener.local_addr().port();
        let (endpoint, _) = listener.into_parts();

        let sid_a = SessionId::from_bytes([1u8; 16]);
        let sid_b = SessionId::from_bytes([2u8; 16]);
        let expected = ExpectedPeerMap::default();
        expected.insert(
            sid_b,
            ExpectedPeer {
                peer_client_id: "peer-b".into(),
                cert_fp: peer_b.fingerprint,
                candidates: vec![],
            },
        );
        expected.insert(
            sid_a,
            ExpectedPeer {
                peer_client_id: "peer-a".into(),
                cert_fp: peer_a.fingerprint,
                candidates: vec![tp_core::p2p_types::Candidate {
                    ip: "127.0.0.1".into(),
                    port: 1234,
                    kind: CandidateKind::Host,
                }],
            },
        );

        let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> =
            Arc::new(move |session_id, _session| {
                let _ = accepted_tx.try_send(session_id);
            });
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            expected,
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let runtime = quinn::default_runtime().expect("quinn runtime");
        let mut client =
            Endpoint::new(EndpointConfig::default(), None, sock, runtime).expect("client endpoint");
        client.set_default_client_config(make_pinned_client_config_with_identity(
            server_bundle.fingerprint,
            &peer_a,
        ));
        let peer = SocketAddr::from(([127, 0, 0, 1], listen_port));
        let conn = client
            .connect(peer, "p2p")
            .expect("connect start")
            .await
            .expect("connect");
        let (mut send, _recv) = conn.open_bi().await.expect("open bi");
        write_test_stream_preface(&mut send, sid_a).await;

        let accepted_sid = tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
            .await
            .expect("listener should accept before timeout")
            .expect("listener accepted channel closed");
        assert_eq!(accepted_sid, sid_a);

        client.close(0u32.into(), b"done");
        listener_task.abort();
    }

    #[tokio::test]
    async fn listener_uses_stream_preface_to_accept_same_fingerprint_concurrent_sessions() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::SessionId;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-server").expect("server cert");
        let peer_bundle = generate_self_signed_cert("p2p-listener-peer").expect("peer cert");
        let listener = P2pListener::bind(&server_bundle).expect("listener bind");
        let listen_port = listener.local_addr().port();
        let (endpoint, _) = listener.into_parts();

        let sid_a = SessionId::from_bytes([22u8; 16]);
        let sid_b = SessionId::from_bytes([23u8; 16]);
        let expected = ExpectedPeerMap::default();
        for (sid, peer_client_id) in [(sid_a, "peer-a"), (sid_b, "peer-b")] {
            expected.insert(
                sid,
                ExpectedPeer {
                    peer_client_id: peer_client_id.into(),
                    cert_fp: peer_bundle.fingerprint,
                    candidates: vec![],
                },
            );
        }

        let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> =
            Arc::new(move |session_id, _session| {
                let _ = accepted_tx.try_send(session_id);
            });
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            expected.clone(),
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let runtime = quinn::default_runtime().expect("quinn runtime");
        let mut client =
            Endpoint::new(EndpointConfig::default(), None, sock, runtime).expect("client endpoint");
        client.set_default_client_config(make_pinned_client_config_with_identity(
            server_bundle.fingerprint,
            &peer_bundle,
        ));
        let peer = SocketAddr::from(([127, 0, 0, 1], listen_port));
        let conn = client
            .connect(peer, "p2p")
            .expect("connect start")
            .await
            .expect("connect");
        let (mut send, _recv) = conn.open_bi().await.expect("open bi");
        write_test_stream_preface(&mut send, sid_b).await;

        let accepted_sid = tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
            .await
            .expect("listener should accept before timeout")
            .expect("listener accepted channel closed");
        assert_eq!(accepted_sid, sid_b);
        assert!(
            expected.get(sid_a).is_some(),
            "accepting sid_b must leave concurrent same-fingerprint sid_a intact"
        );
        assert!(
            expected.get(sid_b).is_none(),
            "accepted sid_b must be consumed"
        );

        client.close(0u32.into(), b"done");
        listener_task.abort();
    }

    #[tokio::test]
    async fn listener_keeps_expected_entry_when_bi_stream_accept_fails() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::SessionId;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-server").expect("server cert");
        let initiator_bundle =
            generate_self_signed_cert("p2p-listener-initiator").expect("initiator cert");
        let listener = P2pListener::bind(&server_bundle).expect("listener bind");
        let listen_port = listener.local_addr().port();
        let (endpoint, _) = listener.into_parts();

        let expected = ExpectedPeerMap::default();
        let session_id = SessionId::from_bytes([19u8; 16]);
        expected.insert(
            session_id,
            ExpectedPeer {
                peer_client_id: "initiator".into(),
                cert_fp: initiator_bundle.fingerprint,
                candidates: vec![],
            },
        );

        let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> =
            Arc::new(move |_session_id, _session| {
                let _ = accepted_tx.try_send(());
            });
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            expected.clone(),
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let runtime = quinn::default_runtime().expect("quinn runtime");
        let mut client =
            Endpoint::new(EndpointConfig::default(), None, sock, runtime).expect("client endpoint");
        client.set_default_client_config(make_pinned_client_config_with_identity(
            server_bundle.fingerprint,
            &initiator_bundle,
        ));
        let peer = SocketAddr::from(([127, 0, 0, 1], listen_port));
        let conn = client
            .connect(peer, "p2p")
            .expect("connect start")
            .await
            .expect("connect");

        tokio::time::sleep(Duration::from_millis(100)).await;
        conn.close(0u32.into(), b"client-closed-before-bi");
        client.close(0u32.into(), b"done");
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            expected.get(session_id).is_some(),
            "listener must not consume expected entry until accept_bi succeeds"
        );
        assert!(
            accepted_rx.try_recv().is_err(),
            "failed accept_bi must not call on_session"
        );

        listener_task.abort();
    }

    #[tokio::test]
    async fn listener_rejects_unexpected_cert_before_waiting_for_stream() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::SessionId;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-server").expect("server cert");
        let expected_bundle =
            generate_self_signed_cert("p2p-listener-expected").expect("expected cert");
        let unexpected_bundle =
            generate_self_signed_cert("p2p-listener-unexpected").expect("unexpected cert");
        let listener = P2pListener::bind(&server_bundle).expect("listener bind");
        let listen_port = listener.local_addr().port();
        let (endpoint, _) = listener.into_parts();

        let expected = ExpectedPeerMap::default();
        let session_id = SessionId::from_bytes([20u8; 16]);
        expected.insert(
            session_id,
            ExpectedPeer {
                peer_client_id: "expected".into(),
                cert_fp: expected_bundle.fingerprint,
                candidates: vec![],
            },
        );

        let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> =
            Arc::new(move |_session_id, _session| {
                let _ = accepted_tx.try_send(());
            });
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            expected.clone(),
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let runtime = quinn::default_runtime().expect("quinn runtime");
        let mut client =
            Endpoint::new(EndpointConfig::default(), None, sock, runtime).expect("client endpoint");
        client.set_default_client_config(make_pinned_client_config_with_identity(
            server_bundle.fingerprint,
            &unexpected_bundle,
        ));
        let peer = SocketAddr::from(([127, 0, 0, 1], listen_port));
        let conn = client
            .connect(peer, "p2p")
            .expect("connect start")
            .await
            .expect("connect");

        tokio::time::timeout(Duration::from_millis(750), conn.closed())
            .await
            .expect("unexpected cert must be rejected before listener waits for a stream");
        assert!(
            expected.get(session_id).is_some(),
            "wrong-cert connection must not consume expected entry"
        );
        assert!(
            accepted_rx.try_recv().is_err(),
            "wrong-cert connection must not call on_session"
        );

        client.close(0u32.into(), b"done");
        listener_task.abort();
    }

    async fn write_test_stream_preface(send: &mut quinn::SendStream, session_id: SessionId) {
        let packed = tp_core::protocol::pack(&tp_core::protocol::BinaryMessage::P2pSessionReady {
            session_id,
            rtt_us: 0,
            chosen_remote_ip: "127.0.0.1".into(),
            chosen_remote_port: 0,
        })
        .to_bytes();
        send.write_all(&(packed.len() as u32).to_be_bytes())
            .await
            .expect("write preface len");
        send.write_all(&packed).await.expect("write preface body");
    }

    #[tokio::test]
    async fn listener_requires_stream_preface_for_same_fingerprint_sessions() {
        use crate::p2p::expected::{ExpectedPeer, ExpectedPeerMap};
        use tp_core::p2p_types::SessionId;

        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_bundle = generate_self_signed_cert("p2p-listener-server").expect("server cert");
        let peer_bundle = generate_self_signed_cert("p2p-listener-peer").expect("peer cert");
        let listener = P2pListener::bind(&server_bundle).expect("listener bind");
        let listen_port = listener.local_addr().port();
        let (endpoint, _) = listener.into_parts();

        let expected = ExpectedPeerMap::default();
        for (sid, peer_client_id) in [
            (SessionId::from_bytes([70u8; 16]), "peer-a"),
            (SessionId::from_bytes([71u8; 16]), "peer-b"),
        ] {
            expected.insert(
                sid,
                ExpectedPeer {
                    peer_client_id: peer_client_id.into(),
                    cert_fp: peer_bundle.fingerprint,
                    candidates: vec![],
                },
            );
        }

        let (accepted_tx, mut accepted_rx) = mpsc::channel(1);
        let on_session: Arc<dyn Fn(SessionId, Session) + Send + Sync> =
            Arc::new(move |session_id, _session| {
                let _ = accepted_tx.try_send(session_id);
            });
        let listener_task = tokio::spawn(run_listener_loop(
            endpoint,
            expected,
            on_session,
            CancellationToken::new(),
            None,
        ));

        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
        let runtime = quinn::default_runtime().expect("quinn runtime");
        let mut client =
            Endpoint::new(EndpointConfig::default(), None, sock, runtime).expect("client endpoint");
        client.set_default_client_config(make_pinned_client_config_with_identity(
            server_bundle.fingerprint,
            &peer_bundle,
        ));
        let peer = SocketAddr::from(([127, 0, 0, 1], listen_port));
        let conn = client
            .connect(peer, "p2p")
            .expect("connect start")
            .await
            .expect("connect");

        tokio::time::timeout(Duration::from_millis(200), accepted_rx.recv())
            .await
            .expect_err(
                "same-fingerprint sessions without stream preface must not call on_session",
            );

        conn.close(0u32.into(), b"done");
        client.close(0u32.into(), b"done");
        listener_task.abort();
    }
}
