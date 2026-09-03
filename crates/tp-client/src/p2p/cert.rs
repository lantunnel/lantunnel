//! Self-signed X.509 cert generation + SHA-256 fingerprint helpers used by
//! the P2P TLS handshake. The fingerprint is exchanged via the relay
//! channel and pinned by the peer on incoming P2P connections.
//!
//! rcgen 0.13's [`generate_simple_self_signed`] is used (matching the
//! existing usage in `tp-transport::tls::self_signed`). The DER bytes and
//! private-key DER are exposed directly so the P2P TLS layer can install
//! them without reaching for rustls types here.

use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
use tp_core::p2p_types::{CertFingerprint, CERT_FP_SIZE};

/// Self-signed certificate material for one P2P endpoint.
#[derive(Clone)]
pub struct CertBundle {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: CertFingerprint,
}

/// Generate a fresh self-signed cert with `common_name` as the only SAN/CN.
pub fn generate_self_signed_cert(common_name: &str) -> Result<CertBundle, rcgen::Error> {
    let certified = generate_simple_self_signed(vec![common_name.to_string()])?;
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let fingerprint = sha256_fingerprint(&cert_der);
    Ok(CertBundle {
        cert_der,
        key_der,
        fingerprint,
    })
}

/// SHA-256 over the full DER-encoded cert; first 32 bytes go into a [`CertFingerprint`].
pub fn sha256_fingerprint(cert_der: &[u8]) -> CertFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    let digest = hasher.finalize();
    let mut out = [0u8; CERT_FP_SIZE];
    out.copy_from_slice(&digest[..CERT_FP_SIZE]);
    CertFingerprint::from_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_cert_returns_consistent_fingerprint() {
        let bundle = generate_self_signed_cert("p2p").unwrap();
        let again = sha256_fingerprint(&bundle.cert_der);
        assert_eq!(bundle.fingerprint.as_bytes(), again.as_bytes());
        assert!(!bundle.cert_der.is_empty());
        assert!(!bundle.key_der.is_empty());
    }
}
