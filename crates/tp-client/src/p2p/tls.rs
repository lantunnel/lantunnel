//! Cert-fingerprint-pinned TLS client config for P2P. Verifies the peer's
//! end-entity cert SHA-256 matches the fingerprint exchanged via relay
//! signaling (P2pAnnounce.cert_fp / P2pAnswer.dst_cert_fp).

use std::sync::Arc;

use crate::p2p::cert::CertBundle;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{
    crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tp_core::p2p_types::{CertFingerprint, CERT_FP_SIZE};

#[derive(Debug)]
struct PinnedFp {
    fingerprint: CertFingerprint,
    supported: WebPkiSupportedAlgorithms,
}

impl PinnedFp {
    fn new(fingerprint: CertFingerprint) -> Self {
        Self {
            fingerprint,
            supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinnedFp {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let digest = hasher.finalize();
        // Constant-time: the pinned fingerprint travels through relay
        // signaling and is not a long-term secret, but a verifier that leaks
        // how many leading bytes matched is still a verifier that can be
        // probed. `subtle` keeps the comparison branch-free.
        if digest[..CERT_FP_SIZE]
            .ct_eq(self.fingerprint.as_bytes())
            .into()
        {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("cert fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(msg, cert, dss, &self.supported)
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(msg, cert, dss, &self.supported)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

pub fn make_pinned_client_config(fp: CertFingerprint) -> quinn::ClientConfig {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedFp::new(fp)))
        .with_no_client_auth();
    let cfg = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .expect("rustls client config to QuicClientConfig conversion");
    quinn::ClientConfig::new(Arc::new(cfg))
}

pub fn make_pinned_client_config_with_identity(
    fp: CertFingerprint,
    identity: &CertBundle,
) -> quinn::ClientConfig {
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivatePkcs8KeyDer::from(identity.key_der.clone());
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedFp::new(fp)))
        .with_client_auth_cert(vec![cert], key.into())
        .expect("valid p2p client identity");
    let cfg = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .expect("rustls client config to QuicClientConfig conversion");
    quinn::ClientConfig::new(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::cert::generate_self_signed_cert;

    #[test]
    fn matching_fp_verifies() {
        let bundle = generate_self_signed_cert("test-peer").expect("cert");
        let verifier = PinnedFp::new(bundle.fingerprint);
        let cert = CertificateDer::from(bundle.cert_der.clone());
        let server = ServerName::try_from("test-peer").expect("server name");
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(0));
        let r = verifier.verify_server_cert(&cert, &[], &server, &[], now);
        assert!(r.is_ok(), "expected match, got {r:?}");
    }

    #[test]
    fn mismatched_fp_rejects() {
        let bundle = generate_self_signed_cert("test-peer").expect("cert");
        let wrong_fp = tp_core::p2p_types::CertFingerprint::from_bytes([0x42u8; 32]);
        let verifier = PinnedFp::new(wrong_fp);
        let cert = CertificateDer::from(bundle.cert_der.clone());
        let server = ServerName::try_from("test-peer").expect("server name");
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(0));
        let r = verifier.verify_server_cert(&cert, &[], &server, &[], now);
        assert!(r.is_err(), "expected mismatch error, got {r:?}");
    }
}
