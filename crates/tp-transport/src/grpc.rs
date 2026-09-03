//! gRPC transport.
//!
//! Wire format: one bidirectional gRPC stream per connection. Each
//! `BinaryMessage` is `pack()`-encoded and placed in `StreamMessage.data`.
//! Auth metadata travels in gRPC headers on stream initiation, matching the
//! Go implementation's `metadata.NewOutgoingContext` call.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Server};
use tonic::{Request, Response, Status, Streaming};
use tower_service::Service;
use tp_core::config::ClientRoleConfig;
use tp_core::protocol::{unpack, BinaryMessage, PackedMessage, TransportCapabilities};

use crate::quic::{AuthHandler, AuthParams};
use crate::session::{
    header_auth_capabilities_from_mask, header_auth_capability_mask,
    header_auth_offered_capabilities, negotiate_header_auth_capabilities, Session,
};
use crate::tls;
use crate::{Result, TransportError};

const GRPC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const GRPC_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);
const METADATA_TRANSPORT_CAPABILITIES: &str = "transport-capabilities";

// Generated code from proto/transport.proto
// tonic returns its own large error enum by value, which we cannot box here.
#[allow(clippy::result_large_err)]
pub mod pb {
    tonic::include_proto!("transport");
}

use pb::transport_service_client::TransportServiceClient;
use pb::transport_service_server::{TransportService, TransportServiceServer};
use pb::StreamMessage;

/// gRPC server.
pub struct GrpcServer {
    bind: SocketAddr,
    tls: Option<tonic::transport::ServerTlsConfig>,
}

impl GrpcServer {
    pub fn new(bind: SocketAddr) -> Self {
        Self { bind, tls: None }
    }

    pub fn with_tls(mut self, certs_pem: Vec<u8>, key_pem: Vec<u8>) -> Self {
        let identity = tonic::transport::Identity::from_pem(certs_pem, key_pem);
        self.tls = Some(tonic::transport::ServerTlsConfig::new().identity(identity));
        self
    }

    /// Run until cancelled. Each accepted stream produces one `Session` handed to
    /// `on_session` alongside its `AuthParams`.
    pub async fn serve<H, F>(self, auth: Arc<H>, on_session: F) -> Result<()>
    where
        H: AuthHandler,
        F: Fn(AuthParams, Session) + Send + Sync + 'static,
    {
        let svc = GrpcService {
            auth,
            on_session: Arc::new(on_session),
        };
        let mut builder = Server::builder()
            .http2_keepalive_interval(Some(GRPC_KEEPALIVE_INTERVAL))
            .http2_keepalive_timeout(Some(GRPC_KEEPALIVE_TIMEOUT));
        if let Some(tls) = self.tls {
            builder = builder
                .tls_config(tls)
                .map_err(|e| TransportError::Tls(e.to_string()))?;
        }
        builder
            .add_service(TransportServiceServer::new(svc))
            .serve(self.bind)
            .await
            .map_err(|e| TransportError::Other(e.to_string()))?;
        Ok(())
    }
}

struct GrpcService<H: AuthHandler> {
    auth: Arc<H>,
    on_session: Arc<dyn Fn(AuthParams, Session) + Send + Sync + 'static>,
}

fn encode_role(role: ClientRoleConfig) -> &'static str {
    match role {
        ClientRoleConfig::Client => "client",
        ClientRoleConfig::App => "app",
    }
}

fn decode_role(raw: &str) -> ClientRoleConfig {
    match raw.trim() {
        "app" => ClientRoleConfig::App,
        _ => ClientRoleConfig::Client,
    }
}

#[tonic::async_trait]
impl<H: AuthHandler> TransportService for GrpcService<H> {
    type BiStreamStream =
        Pin<Box<dyn Stream<Item = std::result::Result<StreamMessage, Status>> + Send + 'static>>;

    async fn bi_stream(
        &self,
        req: Request<Streaming<StreamMessage>>,
    ) -> std::result::Result<Response<Self::BiStreamStream>, Status> {
        let md = req.metadata();
        let client_id = md
            .get("client-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let tunnel_id = md
            .get("tunnel-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let group_id = md
            .get("group-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let group_password = md
            .get("group-password")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let username = md
            .get("username")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let password = md
            .get("password")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let role = md
            .get("client-role")
            .and_then(|v| v.to_str().ok())
            .map(decode_role)
            .unwrap_or_default();
        let capabilities = negotiate_header_auth_capabilities(capabilities_from_metadata(md));
        let peer = req
            .remote_addr()
            .ok_or_else(|| Status::internal("grpc transport missing TCP peer address"))?;

        let params = AuthParams {
            tunnel_id,
            client_id,
            group_id,
            username,
            password,
            group_password,
            role,
            capabilities,
            peer_addr: peer,
        };

        if let Err(reason) = self.auth.authenticate(&params).await {
            return Err(Status::unauthenticated(reason));
        }

        let mut inbound = req.into_inner();
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(256);
        let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(64);
        let (stream_tx, stream_rx) =
            mpsc::channel::<std::result::Result<StreamMessage, Status>>(64);
        let (close_tx, mut close_rx) = mpsc::channel::<()>(1);

        let cid_for_writer = params.client_id.clone();
        let gid_for_writer = params.group_id.clone();
        let mut out_rx2 = out_rx;
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = close_rx.recv() => break,
                    packed = out_rx2.recv() => {
                        let Some(packed) = packed else { break };
                        // gRPC StreamMessage.data is a single Vec<u8>
                        // (proto `bytes` field) so we merge via to_bytes.
                        // One memcpy of `payload.len()` bytes here, same
                        // as the pre-split code did inside `pack()`.
                        let msg = StreamMessage {
                            r#type: pb::stream_message::MessageType::Data as i32,
                            data: packed.to_bytes().to_vec(),
                            client_id: cid_for_writer.clone(),
                            group_id: gid_for_writer.clone(),
                            metadata: Default::default(),
                        };
                        if stream_tx.send(Ok(msg)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let reader = tokio::spawn(async move {
            while let Some(item) = inbound.next().await {
                match item {
                    Ok(sm) => match unpack(&sm.data) {
                        Ok(m) => {
                            if in_tx.send(m).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "grpc protocol decode error");
                            break;
                        }
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, "grpc inbound stream ended");
                        break;
                    }
                }
            }
        });

        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = close_tx.try_send(());
        });
        let session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
            .with_capabilities(capabilities);
        (self.on_session)(params, session);

        let out: Self::BiStreamStream = Box::pin(ReceiverStream::new(stream_rx));
        let mut response = Response::new(out);
        response.metadata_mut().insert(
            METADATA_TRANSPORT_CAPABILITIES,
            header_auth_capability_mask(capabilities)
                .to_string()
                .parse()
                .expect("capability mask is valid gRPC metadata"),
        );
        Ok(response)
    }
}

/// gRPC client.
pub struct GrpcClient {
    url: String,
    endpoint: tonic::transport::Endpoint,
    pinned_tls: Option<GrpcPinnedTlsConnector>,
}

impl GrpcClient {
    /// Accepts `http://host:port` or `https://host:port`.
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let url = url.into();
        let endpoint = grpc_endpoint(&url)?;
        Ok(Self {
            url,
            endpoint,
            pinned_tls: None,
        })
    }

    pub fn with_insecure_tls(mut self, domain: impl Into<String>) -> Result<Self> {
        let original_uri: http::Uri = self
            .url
            .parse()
            .map_err(|e| TransportError::Other(format!("grpc uri: {e}")))?;
        let connect_url = tls_connect_url(&self.url)?;
        self.endpoint = grpc_endpoint(&connect_url)?.origin(original_uri);
        let tls = tls::client_config_for_grpc(None, None, true)?;
        self.pinned_tls = Some(GrpcPinnedTlsConnector::new(domain.into(), tls)?);
        Ok(self)
    }

    pub fn with_exact_leaf_tls(
        mut self,
        domain: impl Into<String>,
        certificate_pem: Vec<u8>,
    ) -> Result<Self> {
        let original_uri: http::Uri = self
            .url
            .parse()
            .map_err(|e| TransportError::Other(format!("grpc uri: {e}")))?;
        let connect_url = tls_connect_url(&self.url)?;
        self.endpoint = grpc_endpoint(&connect_url)?.origin(original_uri);
        let certificate_pem = std::str::from_utf8(&certificate_pem)
            .map_err(|_| TransportError::Tls("exact leaf PEM is not UTF-8".into()))?;
        let tls = tls::client_config_for_grpc_with_exact_leaf(certificate_pem)?;
        self.pinned_tls = Some(GrpcPinnedTlsConnector::new(domain.into(), tls)?);
        Ok(self)
    }

    pub fn with_tls(mut self, domain: impl Into<String>) -> Result<Self> {
        self = self.with_tls_roots(domain, None)?;
        Ok(self)
    }

    pub fn with_tls_roots(
        mut self,
        domain: impl Into<String>,
        ca_pem: Option<Vec<u8>>,
    ) -> Result<Self> {
        let tls = grpc_client_tls_config(domain.into(), ca_pem);
        self.endpoint = self
            .endpoint
            .tls_config(tls)
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        Ok(self)
    }

    /// Connect, open BiStream with auth metadata, return the Session.
    pub async fn connect(self, auth: AuthParams) -> Result<Session> {
        let dial_addr = auth.peer_addr;
        let channel: Channel = if let Some(connector) = self.pinned_tls {
            self.endpoint
                .connect_with_connector(connector.pin(dial_addr))
                .await
                .map_err(|e| TransportError::Other(e.to_string()))?
        } else {
            self.endpoint
                .connect_with_connector(GrpcPinnedTcpConnector { dial_addr })
                .await
                .map_err(|e| TransportError::Other(e.to_string()))?
        };
        let mut client = TransportServiceClient::new(channel);

        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(256);
        let (req_tx, req_rx) = mpsc::channel::<StreamMessage>(64);
        let (close_tx, mut close_rx) = mpsc::channel::<()>(1);

        let cid_for_writer = auth.client_id.clone();
        let gid_for_writer = auth.group_id.clone();
        let mut out_rx2 = out_rx;
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = close_rx.recv() => break,
                    packed = out_rx2.recv() => {
                        let Some(packed) = packed else { break };
                        // Same merge rationale as the server-side writer
                        // above: proto `bytes` is a single contiguous
                        // buffer, so we collapse header + payload here.
                        let msg = StreamMessage {
                            r#type: pb::stream_message::MessageType::Data as i32,
                            data: packed.to_bytes().to_vec(),
                            client_id: cid_for_writer.clone(),
                            group_id: gid_for_writer.clone(),
                            metadata: Default::default(),
                        };
                        if req_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut req = Request::new(ReceiverStream::new(req_rx));
        let md = req.metadata_mut();
        md.insert("client-id", auth.client_id.parse().unwrap());
        md.insert("tunnel-id", auth.tunnel_id.parse().unwrap());
        md.insert("group-id", auth.group_id.parse().unwrap());
        md.insert("group-password", auth.group_password.parse().unwrap());
        md.insert("username", auth.username.parse().unwrap());
        md.insert("password", auth.password.parse().unwrap());
        md.insert("client-role", encode_role(auth.role).parse().unwrap());
        md.insert(
            METADATA_TRANSPORT_CAPABILITIES,
            header_auth_capability_mask(header_auth_offered_capabilities(auth.capabilities))
                .to_string()
                .parse()
                .expect("capability mask is valid gRPC metadata"),
        );

        let response = client
            .bi_stream(req)
            .await
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let capabilities =
            negotiate_header_auth_capabilities(capabilities_from_metadata(response.metadata()));
        let mut inbound = response.into_inner();
        let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(64);
        let reader = tokio::spawn(async move {
            while let Some(item) = inbound.next().await {
                match item {
                    Ok(sm) => match unpack(&sm.data) {
                        Ok(m) => {
                            if in_tx.send(m).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "grpc protocol decode error");
                            break;
                        }
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, "grpc inbound stream ended");
                        break;
                    }
                }
            }
        });

        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            let _ = close_tx.try_send(());
        });
        let peer = auth.peer_addr;
        Ok(
            Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
                .with_capabilities(capabilities),
        )
    }
}

fn capabilities_from_metadata(metadata: &tonic::metadata::MetadataMap) -> TransportCapabilities {
    let mask = metadata
        .get(METADATA_TRANSPORT_CAPABILITIES)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or_default();
    header_auth_capabilities_from_mask(mask)
}

fn grpc_endpoint(url: &str) -> Result<tonic::transport::Endpoint> {
    let endpoint = tonic::transport::Endpoint::from_shared(url.to_string())
        .map_err(|e| TransportError::Other(e.to_string()))?
        .http2_keep_alive_interval(GRPC_KEEPALIVE_INTERVAL)
        .keep_alive_timeout(GRPC_KEEPALIVE_TIMEOUT)
        .keep_alive_while_idle(true);
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rcgen::generate_simple_self_signed;
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    struct AllowAuth;

    #[async_trait]
    impl AuthHandler for AllowAuth {
        async fn authenticate(&self, _params: &AuthParams) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    fn params(peer_addr: SocketAddr) -> AuthParams {
        AuthParams {
            capabilities: Default::default(),
            tunnel_id: "tun-1".into(),
            client_id: "client-1".into(),
            group_id: "group-1".into(),
            username: "user".into(),
            password: "pass".into(),
            group_password: "group-pass".into(),
            role: ClientRoleConfig::App,
            peer_addr,
        }
    }

    #[test]
    fn grpc_keepalive_policy_is_not_more_aggressive_than_relay_quic() {
        assert_eq!(GRPC_KEEPALIVE_INTERVAL, Duration::from_secs(10));
        assert_eq!(GRPC_KEEPALIVE_TIMEOUT, Duration::from_secs(20));
    }

    #[test]
    fn grpc_exact_pem_tls_config_excludes_webpki_roots() {
        let exact = grpc_client_tls_config(
            "gateway.example".into(),
            Some(b"exact Gateway leaf PEM".to_vec()),
        );
        let public = grpc_client_tls_config("gateway.example".into(), None);

        assert!(format!("{exact:?}").contains("with_webpki_roots: false"));
        assert!(format!("{public:?}").contains("with_webpki_roots: true"));
    }

    #[test]
    fn grpc_missing_capability_metadata_is_old_endpoint_and_fails_closed() {
        let old_client_request = tonic::metadata::MetadataMap::new();
        let server_capabilities =
            negotiate_header_auth_capabilities(capabilities_from_metadata(&old_client_request));
        assert!(!server_capabilities.route_bind_control_v1);
        assert!(!server_capabilities.relay_source_attestation_v1);

        let old_server_response = tonic::metadata::MetadataMap::new();
        let client_capabilities =
            negotiate_header_auth_capabilities(capabilities_from_metadata(&old_server_response));
        assert!(!client_capabilities.route_bind_control_v1);
        assert!(!client_capabilities.relay_source_attestation_v1);
    }

    #[tokio::test]
    async fn grpc_pinned_tcp_dial_disables_nagle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener address");
        let (client, accepted) = tokio::join!(connect_grpc_tcp(addr), listener.accept());
        let client = client.expect("connect client");
        accepted.expect("accept client");

        assert!(client.nodelay().expect("read TCP_NODELAY"));
    }

    #[tokio::test]
    async fn grpc_client_pins_tcp_dial_to_recorded_peer_and_negotiates_capabilities() {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = reservation.local_addr().expect("reserved addr");
        drop(reservation);

        let (session_tx, session_rx) = oneshot::channel();
        let session_tx = Arc::new(Mutex::new(Some(session_tx)));
        let server_task = tokio::spawn({
            let session_tx = Arc::clone(&session_tx);
            async move {
                GrpcServer::new(addr)
                    .serve(Arc::new(AllowAuth), move |_params, session| {
                        if let Some(tx) = session_tx.lock().expect("session tx").take() {
                            let _ = tx.send(session);
                        }
                    })
                    .await
            }
        });

        // The URI authority is intentionally not the listening address. The
        // explicit peer address must control the TCP dial while the URI keeps
        // its independent HTTP authority/TLS-name semantics.
        let endpoint = "http://127.0.0.1:1".to_string();
        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match GrpcClient::new(endpoint.clone())
                    .expect("client")
                    .connect(params(addr))
                    .await
                {
                    Ok(client) => break client,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("gRPC server readiness timeout");
        let server = tokio::time::timeout(Duration::from_secs(2), session_rx)
            .await
            .expect("server session timeout")
            .expect("server session");

        assert_eq!(client.peer_addr(), addr);
        assert!(client.capabilities().route_bind_control_v1);
        assert!(client.capabilities().relay_source_attestation_v1);
        assert!(server.capabilities().route_bind_control_v1);
        assert!(server.capabilities().relay_source_attestation_v1);
        server_task.abort();
    }

    #[tokio::test]
    async fn grpc_tls_pins_tcp_dial_while_verifying_the_uri_server_name() {
        let certified =
            generate_simple_self_signed(vec!["gateway.example".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem().into_bytes();
        let key_pem = certified.key_pair.serialize_pem().into_bytes();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = reservation.local_addr().expect("reserved addr");
        drop(reservation);

        let (session_tx, session_rx) = oneshot::channel();
        let session_tx = Arc::new(Mutex::new(Some(session_tx)));
        let server_task = tokio::spawn({
            let session_tx = Arc::clone(&session_tx);
            let certificate_pem = certificate_pem.clone();
            async move {
                GrpcServer::new(addr)
                    .with_tls(certificate_pem, key_pem)
                    .serve(Arc::new(AllowAuth), move |_params, session| {
                        if let Some(tx) = session_tx.lock().expect("session tx").take() {
                            let _ = tx.send(session);
                        }
                    })
                    .await
            }
        });

        let endpoint = "https://gateway.example:1";
        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match GrpcClient::new(endpoint)
                    .expect("client")
                    .with_tls_roots("gateway.example", Some(certificate_pem.clone()))
                    .expect("client TLS")
                    .connect(params(addr))
                    .await
                {
                    Ok(client) => break client,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("gRPC TLS server readiness timeout");
        let server = tokio::time::timeout(Duration::from_secs(2), session_rx)
            .await
            .expect("server session timeout")
            .expect("server session");

        assert_eq!(client.peer_addr(), addr);
        assert!(client.capabilities().route_bind_control_v1);
        assert!(server.capabilities().route_bind_control_v1);
        server_task.abort();
    }

    #[tokio::test]
    async fn grpc_exact_leaf_pin_rejects_a_leaf_signed_by_the_pin() {
        let mut issuer_params =
            rcgen::CertificateParams::new(Vec::<String>::new()).expect("issuer params");
        issuer_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let issuer_key = rcgen::KeyPair::generate().expect("issuer key");
        let issuer = issuer_params
            .self_signed(&issuer_key)
            .expect("issuer certificate");
        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
            .expect("leaf params")
            .signed_by(&leaf_key, &issuer, &issuer_key)
            .expect("issuer-signed leaf");
        let chain_pem = format!("{}{}", leaf.pem(), issuer.pem()).into_bytes();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = reservation.local_addr().expect("reserved addr");
        drop(reservation);
        let server_task = tokio::spawn(async move {
            GrpcServer::new(addr)
                .with_tls(chain_pem, leaf_key.serialize_pem().into_bytes())
                .serve(Arc::new(AllowAuth), |_params, _session| {})
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if TcpStream::connect(addr).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("gRPC server readiness timeout");

        let result = GrpcClient::new(format!("https://127.0.0.1:{}", addr.port()))
            .expect("client")
            .with_exact_leaf_tls("127.0.0.1", issuer.pem().into_bytes())
            .expect("exact leaf TLS")
            .connect(params(addr))
            .await;

        assert!(
            result.is_err(),
            "an issuer relationship must not satisfy a leaf pin"
        );
        server_task.abort();
    }

    #[tokio::test]
    async fn grpc_exact_leaf_pin_accepts_only_the_matching_ip_leaf() {
        let certified =
            generate_simple_self_signed(vec!["127.0.0.1".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem().into_bytes();
        let key_pem = certified.key_pair.serialize_pem().into_bytes();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = reservation.local_addr().expect("reserved addr");
        drop(reservation);
        let (session_tx, session_rx) = oneshot::channel();
        let session_tx = Arc::new(Mutex::new(Some(session_tx)));
        let server_task = tokio::spawn({
            let session_tx = Arc::clone(&session_tx);
            let certificate_pem = certificate_pem.clone();
            async move {
                GrpcServer::new(addr)
                    .with_tls(certificate_pem, key_pem)
                    .serve(Arc::new(AllowAuth), move |_params, session| {
                        if let Some(tx) = session_tx.lock().expect("session tx").take() {
                            let _ = tx.send(session);
                        }
                    })
                    .await
            }
        });

        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match GrpcClient::new(format!("https://127.0.0.1:{}", addr.port()))
                    .expect("client")
                    .with_exact_leaf_tls("127.0.0.1", certificate_pem.clone())
                    .expect("exact leaf TLS")
                    .connect(params(addr))
                    .await
                {
                    Ok(client) => break client,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("gRPC exact-leaf TLS server readiness timeout");
        let server = tokio::time::timeout(Duration::from_secs(2), session_rx)
            .await
            .expect("server session timeout")
            .expect("server session");

        assert_eq!(client.peer_addr(), addr);
        assert!(client.capabilities().route_bind_control_v1);
        assert!(server.capabilities().route_bind_control_v1);
        server_task.abort();
    }
}

fn grpc_client_tls_config(domain: String, ca_pem: Option<Vec<u8>>) -> ClientTlsConfig {
    let tls = ClientTlsConfig::new().domain_name(domain);
    match ca_pem {
        Some(ca_pem) => tls.ca_certificate(Certificate::from_pem(ca_pem)),
        None => tls.with_webpki_roots(),
    }
}

fn tls_connect_url(url: &str) -> Result<String> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e| TransportError::Other(format!("grpc uri: {e}")))?;
    if uri.scheme_str() != Some("https") {
        return Ok(url.to_string());
    }
    let authority = uri
        .authority()
        .ok_or_else(|| TransportError::Other("grpc uri missing authority".into()))?;
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("");
    Ok(format!("http://{authority}{path}"))
}

#[derive(Clone)]
struct GrpcPinnedTlsConnector {
    tls: TlsConnector,
    server_name: ServerName<'static>,
}

impl GrpcPinnedTlsConnector {
    fn new(domain: String, cfg: Arc<rustls::ClientConfig>) -> Result<Self> {
        let server_name = ServerName::try_from(domain.clone())
            .map_err(|_| TransportError::Tls(format!("invalid grpc TLS server name {domain:?}")))?;
        Ok(Self {
            tls: TlsConnector::from(cfg),
            server_name,
        })
    }

    fn pin(self, dial_addr: SocketAddr) -> GrpcPinnedTlsIoConnector {
        GrpcPinnedTlsIoConnector {
            tls: self.tls,
            server_name: self.server_name,
            dial_addr,
        }
    }
}

#[derive(Clone, Copy)]
struct GrpcPinnedTcpConnector {
    dial_addr: SocketAddr,
}

impl Service<http::Uri> for GrpcPinnedTcpConnector {
    type Response = TokioIo<TcpStream>;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = std::io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        let dial_addr = self.dial_addr;
        Box::pin(async move { connect_grpc_tcp(dial_addr).await.map(TokioIo::new) })
    }
}

async fn connect_grpc_tcp(dial_addr: SocketAddr) -> std::io::Result<TcpStream> {
    let tcp = TcpStream::connect(dial_addr).await?;
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

#[derive(Clone)]
struct GrpcPinnedTlsIoConnector {
    tls: TlsConnector,
    server_name: ServerName<'static>,
    dial_addr: SocketAddr,
}

impl Service<http::Uri> for GrpcPinnedTlsIoConnector {
    type Response = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = std::io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        let tls = self.tls.clone();
        let server_name = self.server_name.clone();
        let dial_addr = self.dial_addr;
        Box::pin(async move {
            let tcp = connect_grpc_tcp(dial_addr).await?;
            let stream = tls.connect(server_name, tcp).await?;
            Ok(TokioIo::new(stream))
        })
    }
}
