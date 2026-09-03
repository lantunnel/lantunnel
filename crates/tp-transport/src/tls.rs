//! TLS helpers: load server cert/key from PEM, build rustls configs, and
//! (for dev) mint self-signed certs in-memory.

use std::fs;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tp_core::config::{TRANSPORT_TYPE_GRPC, TRANSPORT_TYPE_QUIC, TRANSPORT_TYPE_WEBSOCKET};

use crate::TransportError;

// The wire ALPN value is frozen for compatibility with deployed peers; the
// name predates the Lantunnel rename and cannot change without breaking them.
// `ALPN_TP_QUIC` is the alias new code should use.
pub const ALPN_ANYPROXY_QUIC: &[u8] = b"anyproxy-quic";
pub const ALPN_TP_QUIC_LEGACY: &[u8] = b"tp-quic/1";
pub const ALPN_TP_QUIC: &[u8] = ALPN_ANYPROXY_QUIC;

fn tunnel_quic_alpns() -> Vec<Vec<u8>> {
    vec![ALPN_ANYPROXY_QUIC.to_vec(), ALPN_TP_QUIC_LEGACY.to_vec()]
}

pub fn load_certs(path: impl AsRef<Path>) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    let data = fs::read(path.as_ref())?;
    parse_certs(&data)
}

pub fn parse_certs(data: &[u8]) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    let mut rd = BufReader::new(data);
    let certs: Vec<_> = rustls_pemfile::certs(&mut rd).collect::<std::io::Result<Vec<_>>>()?;
    if certs.is_empty() {
        return Err(TransportError::Tls("no certificates found in PEM".into()));
    }
    Ok(certs)
}

pub fn load_private_key(path: impl AsRef<Path>) -> Result<PrivateKeyDer<'static>, TransportError> {
    let data = fs::read(path.as_ref())?;
    parse_private_key(&data)
}

/// Parse exactly one private key in a format supported by the transport TLS
/// stack. Keeping this byte-oriented lets callers securely open a key before
/// parsing it instead of reopening an already-validated filesystem path.
pub fn parse_private_key(data: &[u8]) -> Result<PrivateKeyDer<'static>, TransportError> {
    let mut reader = BufReader::new(data);
    let mut private_key = None;
    for item in rustls_pemfile::read_all(&mut reader) {
        let parsed = match item? {
            rustls_pemfile::Item::Pkcs8Key(key) => Some(PrivateKeyDer::Pkcs8(key)),
            rustls_pemfile::Item::Pkcs1Key(key) => Some(PrivateKeyDer::Pkcs1(key)),
            rustls_pemfile::Item::Sec1Key(_) => {
                return Err(TransportError::Tls(
                    "unsupported SEC1 private key found in PEM".into(),
                ));
            }
            _ => None,
        };
        if let Some(parsed) = parsed {
            if private_key.is_some() {
                return Err(TransportError::Tls(
                    "multiple private keys found in PEM".into(),
                ));
            }
            private_key = Some(parsed);
        }
    }
    private_key.ok_or_else(|| TransportError::Tls("no supported private key found in PEM".into()))
}

/// Development-only: generate a self-signed cert covering the given SANs.
pub fn self_signed(
    sans: &[&str],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TransportError> {
    let subject_alt_names = sans.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let cert = rcgen::generate_simple_self_signed(subject_alt_names)
        .map_err(|e| TransportError::Tls(format!("rcgen: {e}")))?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    Ok((vec![cert_der], key_der))
}

pub fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<rustls::ServerConfig>, TransportError> {
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    cfg.alpn_protocols = tunnel_quic_alpns();
    Ok(Arc::new(cfg))
}

pub fn client_config(
    ca_pem_path: Option<&Path>,
    insecure: bool,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    client_config_with_pem(None, ca_pem_path, insecure)
}

pub fn client_config_with_pem(
    ca_pem: Option<&str>,
    ca_pem_path: Option<&Path>,
    insecure: bool,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    build_client_config(ca_pem, ca_pem_path, insecure, tunnel_quic_alpns())
}

pub fn client_config_for_https(
    ca_pem: Option<&str>,
    ca_pem_path: Option<&Path>,
    insecure: bool,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    build_client_config(ca_pem, ca_pem_path, insecure, Vec::new())
}

pub fn client_config_for_grpc(
    ca_pem: Option<&str>,
    ca_pem_path: Option<&Path>,
    insecure: bool,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    build_client_config(ca_pem, ca_pem_path, insecure, vec![b"h2".to_vec()])
}

pub fn client_config_with_exact_leaf(
    certificate_pem: &str,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    build_exact_leaf_client_config(certificate_pem, tunnel_quic_alpns())
}

pub fn client_config_for_https_with_exact_leaf(
    certificate_pem: &str,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    build_exact_leaf_client_config(certificate_pem, Vec::new())
}

pub fn client_config_for_grpc_with_exact_leaf(
    certificate_pem: &str,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    build_exact_leaf_client_config(certificate_pem, vec![b"h2".to_vec()])
}

#[derive(Debug)]
struct ExactLeafVerifier {
    pinned_leaf: CertificateDer<'static>,
    webpki: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for ExactLeafVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() != self.pinned_leaf.as_ref() || !intermediates.is_empty() {
            return Err(rustls::Error::General(
                "Managed Gateway exact leaf mismatch".into(),
            ));
        }
        self.webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.webpki
            .verify_tls12_signature(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.webpki
            .verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

fn exact_leaf_verifier(
    pinned_leaf: CertificateDer<'static>,
) -> Result<Arc<dyn rustls::client::danger::ServerCertVerifier>, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(pinned_leaf.clone())
        .map_err(|error| TransportError::Tls(format!("add exact leaf: {error}")))?;
    let webpki = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|error| TransportError::Tls(format!("build exact leaf verifier: {error}")))?;
    Ok(Arc::new(ExactLeafVerifier {
        pinned_leaf,
        webpki,
    }))
}

fn build_exact_leaf_client_config(
    certificate_pem: &str,
    alpns: Vec<Vec<u8>>,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    let certificates = parse_certs(certificate_pem.as_bytes())?;
    if certificates.len() != 1 {
        return Err(TransportError::Tls(
            "exact leaf pin must contain one certificate".into(),
        ));
    }
    let verifier = exact_leaf_verifier(
        certificates
            .into_iter()
            .next()
            .expect("the exact leaf certificate count was checked above"),
    )?;
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = alpns;
    Ok(Arc::new(config))
}

/// Verify the configured local data-plane listener with its real TLS stack.
/// QUIC is probed over UDP/QUIC; WebSocket and gRPC use their TCP/TLS ALPN.
/// No transport authentication or application data is sent.
pub async fn probe_data_plane_tls(
    transport: &str,
    address: std::net::SocketAddr,
    tls_server_name: &str,
    certificate_pem: &str,
) -> Result<(), TransportError> {
    let probe = async {
        match transport {
            TRANSPORT_TYPE_QUIC => {
                let tls = client_config_with_pem(Some(certificate_pem), None, false)?;
                let client = crate::QuicClient::new(tls, crate::QuicTuning::default())?;
                client.probe_tls(address, tls_server_name).await
            }
            TRANSPORT_TYPE_WEBSOCKET => {
                let tls = client_config_for_https(Some(certificate_pem), None, false)?;
                probe_tcp_tls(address, tls_server_name, tls).await
            }
            TRANSPORT_TYPE_GRPC => {
                let tls = client_config_for_grpc(Some(certificate_pem), None, false)?;
                probe_tcp_tls(address, tls_server_name, tls).await
            }
            other => Err(TransportError::Other(format!(
                "unsupported data-plane transport {other:?}"
            ))),
        }
    };
    tokio::time::timeout(Duration::from_secs(3), probe)
        .await
        .map_err(|_| TransportError::Other("data-plane TLS readiness probe timed out".into()))?
}

async fn probe_tcp_tls(
    address: std::net::SocketAddr,
    tls_server_name: &str,
    tls: Arc<rustls::ClientConfig>,
) -> Result<(), TransportError> {
    let tcp = TcpStream::connect(address).await?;
    let server_name = rustls::pki_types::ServerName::try_from(tls_server_name.to_owned())
        .map_err(|_| TransportError::Tls("invalid TLS server name".into()))?;
    TlsConnector::from(tls)
        .connect(server_name, tcp)
        .await
        .map_err(|error| TransportError::Tls(error.to_string()))?;
    Ok(())
}

fn build_client_config(
    ca_pem: Option<&str>,
    ca_pem_path: Option<&Path>,
    insecure: bool,
    alpns: Vec<Vec<u8>>,
) -> Result<Arc<rustls::ClientConfig>, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(ca) = ca_pem {
        for cert in parse_certs(ca.as_bytes())? {
            roots
                .add(cert)
                .map_err(|e| TransportError::Tls(format!("add root: {e}")))?;
        }
    } else if let Some(ca) = ca_pem_path {
        for cert in load_certs(ca)? {
            roots
                .add(cert)
                .map_err(|e| TransportError::Tls(format!("add root: {e}")))?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let mut cfg = if insecure {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(danger::NoVerify))
            .with_no_client_auth()
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    cfg.alpn_protocols = alpns;
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_key_parser_rejects_multiple_supported_keys() {
        let pem = b"-----BEGIN PRIVATE KEY-----\nAQ==\n-----END PRIVATE KEY-----\n\
                    -----BEGIN RSA PRIVATE KEY-----\nAQ==\n-----END RSA PRIVATE KEY-----\n";

        let error = parse_private_key(pem).expect_err("multiple keys must be rejected");

        assert!(error.to_string().contains("multiple private keys"));
    }

    #[test]
    fn private_key_parser_rejects_an_unsupported_sec1_key() {
        let pem = b"-----BEGIN EC PRIVATE KEY-----\nAQ==\n-----END EC PRIVATE KEY-----\n";

        let error = parse_private_key(pem).expect_err("SEC1 is not a supported key format");

        assert!(error.to_string().contains("unsupported SEC1"));
    }

    #[test]
    fn private_key_parser_rejects_malformed_pem() {
        let pem = b"-----BEGIN PRIVATE KEY-----\nnot-base64!\n-----END PRIVATE KEY-----\n";

        parse_private_key(pem).expect_err("malformed PEM must be rejected");
    }

    #[tokio::test]
    async fn data_plane_probe_performs_a_real_quic_tls_handshake() {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("test certificate");
        let certificate_pem = certified.cert.pem();
        let certs = parse_certs(certificate_pem.as_bytes()).expect("parse cert");
        let key = PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let server = crate::QuicServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_config(certs, key).expect("server config"),
            crate::QuicTuning::default(),
        )
        .expect("bind QUIC");
        let address = server.endpoint_handle().local_addr().expect("local addr");
        let accept = tokio::spawn(async move {
            server
                .accept_incoming()
                .await
                .expect("incoming probe")
                .await
                .expect("QUIC TLS handshake")
        });

        probe_data_plane_tls("quic", address, "localhost", &certificate_pem)
            .await
            .expect("probe succeeds");
        let _connection = accept.await.expect("accept task");
    }

    #[test]
    fn tunnel_quic_alpn_prefers_go_token_and_accepts_legacy_rust_token() {
        let alpns = tunnel_quic_alpns();
        assert_eq!(alpns[0], b"anyproxy-quic");
        assert!(alpns.iter().any(|p| p == b"tp-quic/1"));
    }

    #[test]
    fn client_and_server_configs_use_compatible_alpn_list() {
        let (certs, key) = self_signed(&["localhost"]).unwrap();
        let server = server_config(certs, key).unwrap();
        let client = client_config(None, true).unwrap();

        assert_eq!(server.alpn_protocols[0], b"anyproxy-quic");
        assert!(server.alpn_protocols.iter().any(|p| p == b"tp-quic/1"));
        assert_eq!(client.alpn_protocols[0], b"anyproxy-quic");
        assert!(client.alpn_protocols.iter().any(|p| p == b"tp-quic/1"));
    }

    #[test]
    fn exact_leaf_verifier_rejects_another_leaf_signed_by_the_pinned_certificate() {
        let mut issuer_params =
            rcgen::CertificateParams::new(Vec::<String>::new()).expect("issuer params");
        issuer_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let issuer_key = rcgen::KeyPair::generate().expect("issuer key");
        let issuer = issuer_params
            .self_signed(&issuer_key)
            .expect("issuer certificate");
        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let leaf = rcgen::CertificateParams::new(vec!["203.0.113.88".into()])
            .expect("leaf params")
            .signed_by(&leaf_key, &issuer, &issuer_key)
            .expect("issuer-signed leaf");
        let pinned = CertificateDer::from(issuer.der().to_vec());
        let presented = CertificateDer::from(leaf.der().to_vec());
        let verifier = exact_leaf_verifier(pinned).expect("exact-leaf verifier");
        let server_name = rustls::pki_types::ServerName::try_from("203.0.113.88".to_owned())
            .expect("IP server name");
        let now = rustls::pki_types::UnixTime::now();

        let error = verifier
            .verify_server_cert(&presented, &[], &server_name, &[], now)
            .expect_err("a certificate signed by the pin is not the pinned leaf");

        assert!(error.to_string().contains("exact leaf mismatch"));
    }

    #[test]
    fn exact_leaf_verifier_still_enforces_the_ip_server_identity() {
        let certified = rcgen::generate_simple_self_signed(vec!["203.0.113.88".into()])
            .expect("test certificate");
        let certificate = CertificateDer::from(certified.cert.der().to_vec());
        let verifier = exact_leaf_verifier(certificate.clone()).expect("exact-leaf verifier");
        let correct_server_name =
            rustls::pki_types::ServerName::try_from("203.0.113.88".to_owned())
                .expect("IP server name");
        let wrong_server_name = rustls::pki_types::ServerName::try_from("192.0.2.89".to_owned())
            .expect("IP server name");
        let now = rustls::pki_types::UnixTime::now();

        verifier
            .verify_server_cert(&certificate, &[], &correct_server_name, &[], now)
            .expect("the exact pinned leaf with the exact IP SAN must verify");

        verifier
            .verify_server_cert(&certificate, &[], &wrong_server_name, &[], now)
            .expect_err("an exact leaf pin must not bypass IP-SAN verification");
    }
}

mod danger {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    pub struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
            ]
        }
    }
}
