//! WebSocket transport for the Rust stack.
//!
//! Framing: each `BinaryMessage` is `pack()`-encoded and sent as a single
//! WebSocket **Binary** frame after the HTTP upgrade succeeds.
//!
//! Authentication: the client sends
//! `X-Client-ID`, `X-Group-ID`, `X-Group-Password`, and HTTP Basic auth in the
//! upgrade request. WebSocket does not use an in-band `Auth` / `AuthResponse`
//! frame; that handshake is QUIC-specific.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http::header;
use http::{HeaderMap, HeaderValue, Request};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{
    client_async_tls_with_config, connect_async_tls_with_config, Connector, MaybeTlsStream,
    WebSocketStream,
};
use tp_core::config::ClientRoleConfig;
use tp_core::protocol::{unpack, BinaryMessage, PackedMessage, TransportCapabilities};

use crate::quic::{AuthHandler, AuthParams};
use crate::session::{
    header_auth_capabilities_from_mask, header_auth_capability_mask,
    header_auth_offered_capabilities, negotiate_header_auth_capabilities, Session,
};
use crate::{Result, TransportError};

const HEADER_CLIENT_ID: &str = "X-Client-ID";
const HEADER_TUNNEL_ID: &str = "X-TP-Tunnel-ID";
const HEADER_GROUP_ID: &str = "X-Group-ID";
const HEADER_GROUP_PASSWORD: &str = "X-Group-Password";
const HEADER_CLIENT_ROLE: &str = "X-Client-Role";
const HEADER_TRANSPORT_CAPABILITIES: &str = "X-TP-Transport-Capabilities";

/// WebSocket server. Accepts ws:// (plain) or wss:// (via `bind_tls`).
pub struct WsServer {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
}

impl WsServer {
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            tls: None,
        })
    }

    pub async fn bind_tls(addr: SocketAddr, tls_cfg: Arc<rustls::ServerConfig>) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            tls: Some(TlsAcceptor::from(tls_cfg)),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept one connection, run the auth handshake, and return a ready Session.
    pub async fn accept<H: AuthHandler>(&self, auth: &H) -> Result<Option<(AuthParams, Session)>> {
        let (tcp, peer) = match self.listener.accept().await {
            Ok(v) => v,
            Err(e) => return Err(TransportError::Io(e)),
        };
        let params_and_session = if let Some(acceptor) = &self.tls {
            let tls = acceptor
                .accept(tcp)
                .await
                .map_err(|e| TransportError::Tls(e.to_string()))?;
            let (mut ws, params, capabilities) = accept_with_header_auth(tls, peer).await?;
            if let Err(err) = authenticate_header_params(&params, auth).await {
                let _ = ws.close(None).await;
                return Err(err);
            }
            let (sink, stream) = ws.split();
            (params, wrap_server_tls(sink, stream, peer, capabilities))
        } else {
            let (mut ws, params, capabilities) = accept_with_header_auth(tcp, peer).await?;
            if let Err(err) = authenticate_header_params(&params, auth).await {
                let _ = ws.close(None).await;
                return Err(err);
            }
            let (sink, stream) = ws.split();
            (params, wrap_server_plain(sink, stream, peer, capabilities))
        };
        Ok(Some(params_and_session))
    }
}

/// WebSocket client. `connect("wss://host:port/…", auth)` returns a Session.
pub struct WsClient;

impl WsClient {
    pub async fn connect(url: &str, auth: AuthParams) -> Result<Session> {
        Self::connect_with_tls_config(url, auth, None).await
    }

    pub async fn connect_with_tls_config(
        url: &str,
        auth: AuthParams,
        tls_config: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Session> {
        let req = ws_request(url, &auth)?;
        let connector = tls_config.map(Connector::Rustls);
        let (ws, resp) = connect_async_tls_with_config(req, None, false, connector)
            .await
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let capabilities =
            negotiate_header_auth_capabilities(capabilities_from_headers(resp.headers()));
        let (sink, stream) = ws.split();
        Ok(wrap_client(sink, stream, auth.peer_addr, capabilities))
    }

    /// Connect TCP to `dial_addr` while keeping the URL host as the WebSocket
    /// HTTP Host and, for `wss`, the TLS server name.
    pub async fn connect_to_addr_with_tls_config(
        url: &str,
        dial_addr: SocketAddr,
        auth: AuthParams,
        tls_config: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Session> {
        let req = ws_request(url, &auth)?;
        let connector = tls_config.map(Connector::Rustls);
        let tcp = TcpStream::connect(dial_addr).await?;
        let (ws, resp) = client_async_tls_with_config(req, tcp, None, connector)
            .await
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let capabilities =
            negotiate_header_auth_capabilities(capabilities_from_headers(resp.headers()));
        let (sink, stream) = ws.split();
        Ok(wrap_client(sink, stream, dial_addr, capabilities))
    }
}

// ---- handshake helpers ----

fn ws_request(url: &str, auth: &AuthParams) -> Result<Request<()>> {
    let mut req = url
        .into_client_request()
        .map_err(|e| TransportError::Other(format!("websocket request: {e}")))?;
    insert_header(req.headers_mut(), HEADER_CLIENT_ID, &auth.client_id)?;
    insert_header(req.headers_mut(), HEADER_TUNNEL_ID, &auth.tunnel_id)?;
    insert_header(req.headers_mut(), HEADER_GROUP_ID, &auth.group_id)?;
    insert_header(
        req.headers_mut(),
        HEADER_GROUP_PASSWORD,
        &auth.group_password,
    )?;
    insert_header(
        req.headers_mut(),
        HEADER_CLIENT_ROLE,
        encode_role(auth.role),
    )?;
    insert_header(
        req.headers_mut(),
        HEADER_TRANSPORT_CAPABILITIES,
        &header_auth_capability_mask(header_auth_offered_capabilities(auth.capabilities))
            .to_string(),
    )?;

    let raw = format!("{}:{}", auth.username, auth.password);
    let value = format!("Basic {}", BASE64.encode(raw.as_bytes()));
    insert_header(req.headers_mut(), header::AUTHORIZATION.as_str(), &value)?;
    Ok(req)
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    let value =
        HeaderValue::from_str(value).map_err(|e| TransportError::Other(format!("{name}: {e}")))?;
    headers.insert(name, value);
    Ok(())
}

// `accept_hdr_async` fixes the callback error type to tungstenite's large
// `ErrorResponse`; this adapter only returns `Ok`, so we cannot shrink it.
#[allow(clippy::result_large_err)]
async fn accept_with_header_auth<S>(
    stream: S,
    peer: SocketAddr,
) -> Result<(WebSocketStream<S>, AuthParams, TransportCapabilities)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let captured = Arc::new(Mutex::new(None));
    let captured_for_cb = Arc::clone(&captured);
    let ws = accept_hdr_async(
        stream,
        move |req: &Request<()>, mut resp: http::Response<()>| {
            let capabilities =
                negotiate_header_auth_capabilities(capabilities_from_headers(req.headers()));
            if let Ok(value) =
                HeaderValue::from_str(&header_auth_capability_mask(capabilities).to_string())
            {
                resp.headers_mut()
                    .insert(HEADER_TRANSPORT_CAPABILITIES, value);
            }
            if let Ok(mut guard) = captured_for_cb.lock() {
                let mut params = auth_params_from_headers(req.headers(), peer);
                params.capabilities = capabilities;
                *guard = Some((params, capabilities));
            }
            Ok(resp)
        },
    )
    .await
    .map_err(|e| TransportError::Other(e.to_string()))?;

    let (params, capabilities) = captured
        .lock()
        .map_err(|_| TransportError::Other("websocket header capture poisoned".into()))?
        .take()
        .ok_or_else(|| TransportError::Other("websocket request headers not captured".into()))?;
    Ok((ws, params, capabilities))
}

fn capabilities_from_headers(headers: &HeaderMap) -> TransportCapabilities {
    let mask = headers
        .get(HEADER_TRANSPORT_CAPABILITIES)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or_default();
    header_auth_capabilities_from_mask(mask)
}

fn auth_params_from_headers(headers: &HeaderMap, peer: SocketAddr) -> AuthParams {
    let (username, password) = basic_auth(headers).unwrap_or_default();
    AuthParams {
        tunnel_id: header_string(headers, HEADER_TUNNEL_ID),
        client_id: header_string(headers, HEADER_CLIENT_ID),
        group_id: header_string(headers, HEADER_GROUP_ID),
        username,
        password,
        group_password: header_string(headers, HEADER_GROUP_PASSWORD),
        role: decode_role(&header_string(headers, HEADER_CLIENT_ROLE)),
        capabilities: TransportCapabilities::default(),
        peer_addr: peer,
    }
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

fn header_string(headers: &HeaderMap, name: &'static str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = BASE64.decode(encoded.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

async fn authenticate_header_params<H: AuthHandler>(params: &AuthParams, auth: &H) -> Result<()> {
    if params.client_id.is_empty() {
        return Err(TransportError::AuthFailed("missing client id".into()));
    }
    auth.authenticate(params)
        .await
        .map_err(TransportError::AuthFailed)
}

// ---- Session wrappers ----

type PlainSink = futures_util::stream::SplitSink<WebSocketStream<TcpStream>, WsMessage>;
type PlainStream = futures_util::stream::SplitStream<WebSocketStream<TcpStream>>;
type TlsSink = futures_util::stream::SplitSink<
    WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>,
    WsMessage,
>;
type TlsRecv =
    futures_util::stream::SplitStream<WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>>;
type ClientSink =
    futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
type ClientRecv = futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

fn wrap_server_plain(
    sink: PlainSink,
    stream: PlainStream,
    peer: SocketAddr,
    capabilities: TransportCapabilities,
) -> Session {
    let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(256);
    let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(64);
    let (close_tx, close_rx) = mpsc::channel::<()>(1);
    let writer = spawn_writer_plain(sink, out_rx, close_rx);
    let reader = spawn_reader_plain(stream, in_tx);
    let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
        let _ = close_tx.try_send(());
    });
    Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
        .with_capabilities(capabilities)
}

fn wrap_server_tls(
    sink: TlsSink,
    stream: TlsRecv,
    peer: SocketAddr,
    capabilities: TransportCapabilities,
) -> Session {
    let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(256);
    let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(64);
    let (close_tx, close_rx) = mpsc::channel::<()>(1);
    let writer = spawn_writer_tls(sink, out_rx, close_rx);
    let reader = spawn_reader_tls(stream, in_tx);
    let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
        let _ = close_tx.try_send(());
    });
    Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
        .with_capabilities(capabilities)
}

fn wrap_client(
    sink: ClientSink,
    stream: ClientRecv,
    peer: SocketAddr,
    capabilities: TransportCapabilities,
) -> Session {
    let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(256);
    let (in_tx, in_rx) = mpsc::channel::<BinaryMessage>(64);
    let (close_tx, close_rx) = mpsc::channel::<()>(1);
    let writer = spawn_writer_client(sink, out_rx, close_rx);
    let reader = spawn_reader_client(stream, in_tx);
    let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
        let _ = close_tx.try_send(());
    });
    Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader)
        .with_capabilities(capabilities)
}

macro_rules! define_writer {
    ($name:ident, $sink:ty) => {
        fn $name(
            mut sink: $sink,
            mut out_rx: mpsc::Receiver<PackedMessage>,
            mut close_rx: mpsc::Receiver<()>,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = close_rx.recv() => {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            break;
                        }
                        packed = out_rx.recv() => {
                            let Some(packed) = packed else { break };
                            // WebSocket Binary frame takes one contiguous
                            // buffer — merge header + payload into a single
                            // Bytes via PackedMessage::to_bytes. One memcpy
                            // of `payload.len()` bytes here (on the WS path
                            // only; the QUIC stream writer vectors the
                            // chunks without merging).
                            let bytes = packed.to_bytes();
                            if sink.send(WsMessage::Binary(bytes.to_vec().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            })
        }
    };
}

macro_rules! define_reader {
    ($name:ident, $stream:ty) => {
        fn $name(
            mut stream: $stream,
            in_tx: mpsc::Sender<BinaryMessage>,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                while let Some(frame) = stream.next().await {
                    match frame {
                        Ok(WsMessage::Binary(b)) => match unpack(&b) {
                            Ok(m) => {
                                if in_tx.send(m).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "ws protocol decode error");
                                break;
                            }
                        },
                        Ok(WsMessage::Close(_)) | Err(_) => break,
                        _ => continue,
                    }
                }
            })
        }
    };
}

define_writer!(spawn_writer_plain, PlainSink);
define_writer!(spawn_writer_tls, TlsSink);
define_writer!(spawn_writer_client, ClientSink);

define_reader!(spawn_reader_plain, PlainStream);
define_reader!(spawn_reader_tls, TlsRecv);
define_reader!(spawn_reader_client, ClientRecv);

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::time::Duration;
    use tokio::time::timeout;

    fn params() -> AuthParams {
        AuthParams {
            capabilities: Default::default(),
            tunnel_id: "tun-1".into(),
            client_id: "client-1".into(),
            group_id: "group-1".into(),
            username: "user".into(),
            password: "pass".into(),
            group_password: "group-pass".into(),
            role: ClientRoleConfig::App,
            peer_addr: "127.0.0.1:0".parse().unwrap(),
        }
    }

    #[test]
    fn ws_request_uses_go_upgrade_headers() {
        let req = ws_request("ws://127.0.0.1:8080/ws", &params()).expect("request");
        assert_eq!(req.headers()[HEADER_CLIENT_ID], "client-1");
        assert_eq!(req.headers()[HEADER_TUNNEL_ID], "tun-1");
        assert_eq!(req.headers()[HEADER_GROUP_ID], "group-1");
        assert_eq!(req.headers()[HEADER_GROUP_PASSWORD], "group-pass");
        assert_eq!(req.headers()[HEADER_TRANSPORT_CAPABILITIES], "5");
        assert_eq!(
            basic_auth(req.headers()).expect("basic auth"),
            ("user".into(), "pass".into())
        );
    }

    #[test]
    fn ws_missing_capability_header_is_old_endpoint_and_fails_closed() {
        let old_client_request = HeaderMap::new();
        let server_capabilities =
            negotiate_header_auth_capabilities(capabilities_from_headers(&old_client_request));
        assert!(!server_capabilities.route_bind_control_v1);
        assert!(!server_capabilities.relay_source_attestation_v1);

        let old_server_response = HeaderMap::new();
        let client_capabilities =
            negotiate_header_auth_capabilities(capabilities_from_headers(&old_server_response));
        assert!(!client_capabilities.route_bind_control_v1);
        assert!(!client_capabilities.relay_source_attestation_v1);
    }

    #[test]
    fn auth_params_parse_go_upgrade_headers() {
        let req = ws_request("ws://127.0.0.1:8080/ws", &params()).expect("request");
        let peer = "127.0.0.1:9000".parse().unwrap();
        let got = auth_params_from_headers(req.headers(), peer);
        assert_eq!(got.tunnel_id, "tun-1");
        assert_eq!(got.client_id, "client-1");
        assert_eq!(got.group_id, "group-1");
        assert_eq!(got.username, "user");
        assert_eq!(got.password, "pass");
        assert_eq!(got.group_password, "group-pass");
        assert_eq!(got.peer_addr, peer);
    }

    struct ExactAuth;

    #[async_trait]
    impl AuthHandler for ExactAuth {
        async fn authenticate(&self, p: &AuthParams) -> std::result::Result<(), String> {
            if p.client_id == "client-1"
                && p.tunnel_id == "tun-1"
                && p.group_id == "group-1"
                && p.username == "user"
                && p.password == "pass"
                && p.group_password == "group-pass"
            {
                Ok(())
            } else {
                Err(format!("unexpected auth params: {p:?}"))
            }
        }
    }

    struct RejectAuth;

    #[async_trait]
    impl AuthHandler for RejectAuth {
        async fn authenticate(&self, _p: &AuthParams) -> std::result::Result<(), String> {
            Err("rejected fixture credentials".into())
        }
    }

    #[tokio::test]
    async fn ws_rejected_credentials_never_expose_a_server_session() {
        let server = WsServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let addr = server.local_addr().expect("local addr");

        let server_task = tokio::spawn(async move { server.accept(&RejectAuth).await });

        let mut auth = params();
        auth.peer_addr = addr;
        let provisional_client = WsClient::connect(&format!("ws://{addr}/ws"), auth)
            .await
            .expect("HTTP 101 establishes only a provisional client session");

        let server_result = timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server authentication timeout")
            .expect("server task");
        assert!(matches!(server_result, Err(TransportError::AuthFailed(_))));

        let (_sender, mut receiver, _datagram) = provisional_client.split();
        assert!(timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("rejected client session did not close")
            .is_none());
    }

    #[tokio::test]
    async fn ws_client_and_server_authenticate_and_negotiate_exact_relay_via_upgrade_headers() {
        let server = WsServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let addr = server.local_addr().expect("local addr");

        let server_task = tokio::spawn(async move {
            let auth = ExactAuth;
            server.accept(&auth).await.expect("accept").expect("some")
        });

        let mut auth = params();
        auth.peer_addr = addr;
        let client = WsClient::connect(&format!("ws://{addr}/ws"), auth)
            .await
            .expect("connect");

        let (got, server_session) = timeout(Duration::from_secs(2), server_task)
            .await
            .expect("accept timeout")
            .expect("server task");
        assert_eq!(got.client_id, "client-1");
        assert_eq!(got.group_id, "group-1");
        assert_eq!(got.username, "user");
        assert_eq!(got.password, "pass");
        assert_eq!(got.group_password, "group-pass");
        assert!(client.capabilities().route_bind_control_v1);
        assert!(client.capabilities().relay_source_attestation_v1);
        assert!(server_session.capabilities().route_bind_control_v1);
        assert!(server_session.capabilities().relay_source_attestation_v1);
    }

    #[tokio::test]
    async fn wss_can_dial_an_ip_while_verifying_a_separate_server_name() {
        let certified = rcgen::generate_simple_self_signed(vec!["gateway.example".into()])
            .expect("test certificate");
        let certificate_pem = certified.cert.pem();
        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certified.cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
            )
            .expect("WebSocket TLS");
        let server = WsServer::bind_tls(
            "127.0.0.1:0".parse().expect("bind address"),
            Arc::new(server_tls),
        )
        .await
        .expect("bind WebSocket server");
        let dial_addr = server.local_addr().expect("server address");
        let server_task = tokio::spawn(async move {
            server
                .accept(&ExactAuth)
                .await
                .expect("accept")
                .expect("session")
        });
        let client_tls = crate::tls::client_config_for_https(Some(&certificate_pem), None, false)
            .expect("client TLS");
        let mut auth = params();
        auth.peer_addr = dial_addr;

        let client = WsClient::connect_to_addr_with_tls_config(
            &format!("wss://gateway.example:{}/ws", dial_addr.port()),
            dial_addr,
            auth,
            Some(client_tls),
        )
        .await
        .expect("connect by IP using gateway.example for TLS");
        let (accepted, server_session) = timeout(Duration::from_secs(2), server_task)
            .await
            .expect("accept timeout")
            .expect("server task");

        assert_eq!(accepted.client_id, "client-1");
        assert!(client.capabilities().route_bind_control_v1);
        assert!(server_session.capabilities().route_bind_control_v1);
    }
}
