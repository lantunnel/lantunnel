//! Lantunnel 2.0 PeerLink identity binding and Relay key agreement.

use std::net::IpAddr;

use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{EphemeralSecret, PublicKey};

use crate::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};
use crate::provisioning::{
    append_field, encode_public_membership_v2, PeerProfileV2, ProvisioningError,
    PublicPeerMembershipV2,
};

const OFFER_DOMAIN_V2: &[u8] = b"lantunnel.offer.v2";
const ANSWER_DOMAIN_V2: &[u8] = b"lantunnel.answer.v2";
const RELAY_KEY_DOMAIN_V2: &[u8] = b"lantunnel.relay.key.v2";
const X25519_KEY_SIZE: usize = 32;
const ED25519_SIGNATURE_SIZE: usize = 64;
const OFFER_WIRE_TAG_V2: u8 = 1;
const ANSWER_WIRE_TAG_V2: u8 = 2;
const MAX_WIRE_STRING_SIZE_V2: usize = 4096;
const MAX_PEER_LINK_WIRE_SIZE_V2: usize = 64 * 1024;
pub const MAX_P2P_CANDIDATES_V2: usize = u8::MAX as usize;

/// Signed, end-to-end PeerLink offer. Gateway delivery metadata is not part
/// of this value and cannot alter any of its security fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pOfferV2 {
    pub tunnel_id: String,
    pub session_id: SessionId,
    pub source_peer_id: String,
    pub target_peer_id: String,
    pub candidates: Vec<Candidate>,
    pub direct_certificate_fingerprint: CertFingerprint,
    pub public_peer_membership: PublicPeerMembershipV2,
    pub ephemeral_x25519_public_key: [u8; X25519_KEY_SIZE],
    pub peer_signature: [u8; ED25519_SIGNATURE_SIZE],
}

impl P2pOfferV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        source_profile: &PeerProfileV2,
        session_id: SessionId,
        target_peer_id: String,
        candidates: Vec<Candidate>,
        direct_certificate_fingerprint: CertFingerprint,
        ephemeral_secret: &PeerLinkEphemeralSecretV2,
    ) -> Result<Self, PeerLinkCryptoErrorV2> {
        source_profile.verify()?;
        if target_peer_id.trim().is_empty() || target_peer_id == source_profile.peer.peer_id {
            return Err(PeerLinkCryptoErrorV2::InvalidPeerIdentity);
        }
        validate_candidates(&candidates)?;
        let mut offer = Self {
            tunnel_id: source_profile.tunnel_id.clone(),
            session_id,
            source_peer_id: source_profile.peer.peer_id.clone(),
            target_peer_id,
            candidates,
            direct_certificate_fingerprint,
            public_peer_membership: source_profile.public_membership(),
            ephemeral_x25519_public_key: ephemeral_secret.public_key(),
            peer_signature: [0; ED25519_SIGNATURE_SIZE],
        };
        offer.peer_signature = source_profile.sign_peer_message_v2(&offer.canonical_unsigned()?)?;
        Ok(offer)
    }

    pub fn verify(&self, tunnel_signing_public_key: &str) -> Result<(), PeerLinkCryptoErrorV2> {
        if self.tunnel_id.trim().is_empty()
            || self.source_peer_id.trim().is_empty()
            || self.target_peer_id.trim().is_empty()
            || self.source_peer_id == self.target_peer_id
            || self.public_peer_membership.tunnel_id != self.tunnel_id
            || self.public_peer_membership.peer_id != self.source_peer_id
        {
            return Err(PeerLinkCryptoErrorV2::InvalidPeerIdentity);
        }
        validate_candidates(&self.candidates)?;
        self.public_peer_membership
            .verify(tunnel_signing_public_key)?;
        self.public_peer_membership
            .verify_peer_message_v2(&self.canonical_unsigned()?, &self.peer_signature)
            .map_err(|_| PeerLinkCryptoErrorV2::InvalidPeerSignature)
    }

    pub fn canonical_unsigned(&self) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
        let candidates = canonical_candidates(&self.candidates)?;
        let membership = encode_public_membership_v2(&self.public_peer_membership)?;
        canonical_context(
            OFFER_DOMAIN_V2,
            &[
                self.tunnel_id.as_bytes(),
                self.session_id.as_bytes(),
                self.source_peer_id.as_bytes(),
                self.target_peer_id.as_bytes(),
                &candidates,
                self.direct_certificate_fingerprint.as_bytes(),
                &membership,
                &self.ephemeral_x25519_public_key,
            ],
        )
    }

    pub fn canonical_signed(&self) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
        let mut encoded = self.canonical_unsigned()?;
        append_field(&mut encoded, &self.peer_signature)?;
        Ok(encoded)
    }

    pub fn signed_hash(&self) -> Result<[u8; 32], PeerLinkCryptoErrorV2> {
        Ok(Sha256::digest(self.canonical_signed()?).into())
    }

    /// Encode the complete signed Offer carried by `P2pOfferV2.signed_offer`.
    /// The outer message selects the destination; this body contains every
    /// end-to-end authenticated field required by the receiving Peer.
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
        validate_candidates(&self.candidates)?;
        let mut encoded = vec![OFFER_WIRE_TAG_V2];
        append_wire_string(&mut encoded, &self.tunnel_id)?;
        encoded.extend_from_slice(self.session_id.as_bytes());
        append_wire_string(&mut encoded, &self.source_peer_id)?;
        append_wire_string(&mut encoded, &self.target_peer_id)?;
        append_wire_candidates(&mut encoded, &self.candidates)?;
        encoded.extend_from_slice(self.direct_certificate_fingerprint.as_bytes());
        append_wire_membership(&mut encoded, &self.public_peer_membership)?;
        encoded.extend_from_slice(&self.ephemeral_x25519_public_key);
        encoded.extend_from_slice(&self.peer_signature);
        validate_wire_size(&encoded)?;
        Ok(encoded)
    }

    pub fn from_wire_bytes(encoded: &[u8]) -> Result<Self, PeerLinkCryptoErrorV2> {
        validate_wire_size(encoded)?;
        let mut reader = WireReaderV2::new(encoded);
        if reader.read_u8()? != OFFER_WIRE_TAG_V2 {
            return Err(PeerLinkCryptoErrorV2::InvalidWireEncoding);
        }
        let offer = Self {
            tunnel_id: reader.read_string()?,
            session_id: SessionId::from_bytes(reader.read_array()?),
            source_peer_id: reader.read_string()?,
            target_peer_id: reader.read_string()?,
            candidates: reader.read_candidates()?,
            direct_certificate_fingerprint: CertFingerprint::from_bytes(reader.read_array()?),
            public_peer_membership: reader.read_membership()?,
            ephemeral_x25519_public_key: reader.read_array()?,
            peer_signature: reader.read_array()?,
        };
        reader.finish()?;
        validate_candidates(&offer.candidates)?;
        Ok(offer)
    }
}

/// Signed answer bound to the complete signed Offer hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pAnswerV2 {
    pub offer_hash: [u8; 32],
    pub accepted_peer_id: String,
    pub accepted: bool,
    pub reason_code: u8,
    pub candidates: Vec<Candidate>,
    pub direct_certificate_fingerprint: CertFingerprint,
    pub public_peer_membership: PublicPeerMembershipV2,
    pub ephemeral_x25519_public_key: [u8; X25519_KEY_SIZE],
    pub peer_signature: [u8; ED25519_SIGNATURE_SIZE],
}

impl P2pAnswerV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        target_profile: &PeerProfileV2,
        offer: &P2pOfferV2,
        accepted: bool,
        reason_code: u8,
        candidates: Vec<Candidate>,
        direct_certificate_fingerprint: CertFingerprint,
        ephemeral_secret: &PeerLinkEphemeralSecretV2,
    ) -> Result<Self, PeerLinkCryptoErrorV2> {
        target_profile.verify()?;
        offer.verify(&target_profile.tunnel_signing_public_key)?;
        if target_profile.tunnel_id != offer.tunnel_id
            || target_profile.peer.peer_id != offer.target_peer_id
        {
            return Err(PeerLinkCryptoErrorV2::InvalidPeerIdentity);
        }
        validate_answer_result(accepted, reason_code)?;
        validate_candidates(&candidates)?;
        let mut answer = Self {
            offer_hash: offer.signed_hash()?,
            accepted_peer_id: target_profile.peer.peer_id.clone(),
            accepted,
            reason_code,
            candidates,
            direct_certificate_fingerprint,
            public_peer_membership: target_profile.public_membership(),
            ephemeral_x25519_public_key: ephemeral_secret.public_key(),
            peer_signature: [0; ED25519_SIGNATURE_SIZE],
        };
        answer.peer_signature =
            target_profile.sign_peer_message_v2(&answer.canonical_unsigned()?)?;
        Ok(answer)
    }

    pub fn verify_for_offer(
        &self,
        offer: &P2pOfferV2,
        tunnel_signing_public_key: &str,
    ) -> Result<(), PeerLinkCryptoErrorV2> {
        offer.verify(tunnel_signing_public_key)?;
        if self.offer_hash != offer.signed_hash()? {
            return Err(PeerLinkCryptoErrorV2::OfferSubstitution);
        }
        validate_answer_result(self.accepted, self.reason_code)?;
        validate_candidates(&self.candidates)?;
        if self.accepted_peer_id != offer.target_peer_id
            || self.public_peer_membership.tunnel_id != offer.tunnel_id
            || self.public_peer_membership.peer_id != self.accepted_peer_id
        {
            return Err(PeerLinkCryptoErrorV2::InvalidPeerIdentity);
        }
        self.public_peer_membership
            .verify(tunnel_signing_public_key)?;
        self.public_peer_membership
            .verify_peer_message_v2(&self.canonical_unsigned()?, &self.peer_signature)
            .map_err(|_| PeerLinkCryptoErrorV2::InvalidPeerSignature)
    }

    pub fn canonical_unsigned(&self) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
        let accepted = [u8::from(self.accepted)];
        let reason_code = [self.reason_code];
        let candidates = canonical_candidates(&self.candidates)?;
        let membership = encode_public_membership_v2(&self.public_peer_membership)?;
        canonical_context(
            ANSWER_DOMAIN_V2,
            &[
                &self.offer_hash,
                self.accepted_peer_id.as_bytes(),
                &accepted,
                &reason_code,
                &candidates,
                self.direct_certificate_fingerprint.as_bytes(),
                &membership,
                &self.ephemeral_x25519_public_key,
            ],
        )
    }

    pub fn canonical_signed(&self) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
        let mut encoded = self.canonical_unsigned()?;
        append_field(&mut encoded, &self.peer_signature)?;
        Ok(encoded)
    }

    /// Encode the complete signed Answer carried by `P2pAnswerV2.signed_answer`.
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
        validate_answer_result(self.accepted, self.reason_code)?;
        validate_candidates(&self.candidates)?;
        let mut encoded = vec![ANSWER_WIRE_TAG_V2];
        encoded.extend_from_slice(&self.offer_hash);
        append_wire_string(&mut encoded, &self.accepted_peer_id)?;
        encoded.push(u8::from(self.accepted));
        encoded.push(self.reason_code);
        append_wire_candidates(&mut encoded, &self.candidates)?;
        encoded.extend_from_slice(self.direct_certificate_fingerprint.as_bytes());
        append_wire_membership(&mut encoded, &self.public_peer_membership)?;
        encoded.extend_from_slice(&self.ephemeral_x25519_public_key);
        encoded.extend_from_slice(&self.peer_signature);
        validate_wire_size(&encoded)?;
        Ok(encoded)
    }

    pub fn from_wire_bytes(encoded: &[u8]) -> Result<Self, PeerLinkCryptoErrorV2> {
        validate_wire_size(encoded)?;
        let mut reader = WireReaderV2::new(encoded);
        if reader.read_u8()? != ANSWER_WIRE_TAG_V2 {
            return Err(PeerLinkCryptoErrorV2::InvalidWireEncoding);
        }
        let offer_hash = reader.read_array()?;
        let accepted_peer_id = reader.read_string()?;
        let accepted = match reader.read_u8()? {
            0 => false,
            1 => true,
            _ => return Err(PeerLinkCryptoErrorV2::InvalidWireEncoding),
        };
        let reason_code = reader.read_u8()?;
        let answer = Self {
            offer_hash,
            accepted_peer_id,
            accepted,
            reason_code,
            candidates: reader.read_candidates()?,
            direct_certificate_fingerprint: CertFingerprint::from_bytes(reader.read_array()?),
            public_peer_membership: reader.read_membership()?,
            ephemeral_x25519_public_key: reader.read_array()?,
            peer_signature: reader.read_array()?,
        };
        reader.finish()?;
        validate_answer_result(answer.accepted, answer.reason_code)?;
        validate_candidates(&answer.candidates)?;
        Ok(answer)
    }
}

/// One X25519 private value for one PeerLink negotiation. It intentionally
/// implements neither `Clone`, `Debug`, nor serialization.
pub struct PeerLinkEphemeralSecretV2(EphemeralSecret);

impl PeerLinkEphemeralSecretV2 {
    pub fn generate() -> Self {
        Self(EphemeralSecret::random_from_rng(OsRng))
    }

    pub fn public_key(&self) -> [u8; X25519_KEY_SIZE] {
        PublicKey::from(&self.0).to_bytes()
    }

    pub fn derive_session_keys(
        self,
        offer: &P2pOfferV2,
        answer: &P2pAnswerV2,
        tunnel_signing_public_key: &str,
    ) -> Result<PeerLinkSessionKeysV2, PeerLinkCryptoErrorV2> {
        answer.verify_for_offer(offer, tunnel_signing_public_key)?;
        if !answer.accepted {
            return Err(PeerLinkCryptoErrorV2::PeerLinkRejected(answer.reason_code));
        }

        let local_public_key = self.public_key();
        let (remote_public_key, local_is_source) =
            if local_public_key == offer.ephemeral_x25519_public_key {
                (answer.ephemeral_x25519_public_key, true)
            } else if local_public_key == answer.ephemeral_x25519_public_key {
                (offer.ephemeral_x25519_public_key, false)
            } else {
                return Err(PeerLinkCryptoErrorV2::EphemeralKeyMismatch);
            };
        let shared_secret = self.0.diffie_hellman(&PublicKey::from(remote_public_key));
        if !shared_secret.was_contributory() {
            return Err(PeerLinkCryptoErrorV2::NonContributorySharedSecret);
        }

        let mut transcript = offer.canonical_signed()?;
        transcript.extend_from_slice(&answer.canonical_signed()?);
        let transcript_hash = Sha256::digest(transcript);
        let hkdf = Hkdf::<Sha256>::new(Some(transcript_hash.as_ref()), shared_secret.as_bytes());
        let source_to_target_info = canonical_context(
            RELAY_KEY_DOMAIN_V2,
            &[
                offer.tunnel_id.as_bytes(),
                offer.session_id.as_bytes(),
                offer.source_peer_id.as_bytes(),
                offer.target_peer_id.as_bytes(),
            ],
        )?;
        let target_to_source_info = canonical_context(
            RELAY_KEY_DOMAIN_V2,
            &[
                offer.tunnel_id.as_bytes(),
                offer.session_id.as_bytes(),
                offer.target_peer_id.as_bytes(),
                offer.source_peer_id.as_bytes(),
            ],
        )?;
        let mut source_to_target = [0; 32];
        let mut target_to_source = [0; 32];
        hkdf.expand(&source_to_target_info, &mut source_to_target)
            .expect("32 bytes is within the HKDF-SHA256 output limit");
        hkdf.expand(&target_to_source_info, &mut target_to_source)
            .expect("32 bytes is within the HKDF-SHA256 output limit");

        let (send, receive) = if local_is_source {
            (source_to_target, target_to_source)
        } else {
            (target_to_source, source_to_target)
        };
        Ok(PeerLinkSessionKeysV2 { send, receive })
    }
}

/// Locally oriented keys. A source's send key equals the target's receive key
/// and vice versa. Key bytes are deliberately omitted from `Debug` output by
/// not implementing `Debug` for this type.
pub struct PeerLinkSessionKeysV2 {
    send: [u8; 32],
    receive: [u8; 32],
}

impl PeerLinkSessionKeysV2 {
    pub fn send_key(&self) -> &[u8; 32] {
        &self.send
    }

    pub fn receive_key(&self) -> &[u8; 32] {
        &self.receive
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PeerLinkCryptoErrorV2 {
    #[error(transparent)]
    Provisioning(#[from] ProvisioningError),
    #[error("invalid PeerLink Peer identity")]
    InvalidPeerIdentity,
    #[error("invalid Peer signature")]
    InvalidPeerSignature,
    #[error("answer does not bind this signed Offer")]
    OfferSubstitution,
    #[error("answer acceptance and reason code disagree")]
    InvalidAnswerResult,
    #[error("invalid P2P candidate list")]
    InvalidCandidate,
    #[error("PeerLink was rejected with reason code {0}")]
    PeerLinkRejected(u8),
    #[error("local ephemeral key is not part of this PeerLink transcript")]
    EphemeralKeyMismatch,
    #[error("X25519 shared secret is all zero")]
    NonContributorySharedSecret,
    #[error("invalid PeerLink wire encoding")]
    InvalidWireEncoding,
}

fn validate_wire_size(encoded: &[u8]) -> Result<(), PeerLinkCryptoErrorV2> {
    if encoded.is_empty() || encoded.len() > MAX_PEER_LINK_WIRE_SIZE_V2 {
        Err(PeerLinkCryptoErrorV2::InvalidWireEncoding)
    } else {
        Ok(())
    }
}

fn append_wire_string(encoded: &mut Vec<u8>, value: &str) -> Result<(), PeerLinkCryptoErrorV2> {
    if value.len() > MAX_WIRE_STRING_SIZE_V2 {
        return Err(PeerLinkCryptoErrorV2::InvalidWireEncoding);
    }
    append_field(encoded, value.as_bytes())?;
    Ok(())
}

fn append_wire_candidates(
    encoded: &mut Vec<u8>,
    candidates: &[Candidate],
) -> Result<(), PeerLinkCryptoErrorV2> {
    validate_candidates(candidates)?;
    encoded
        .push(u8::try_from(candidates.len()).map_err(|_| PeerLinkCryptoErrorV2::InvalidCandidate)?);
    for candidate in candidates {
        append_wire_string(encoded, &candidate.ip)?;
        encoded.extend_from_slice(&candidate.port.to_be_bytes());
        encoded.push(candidate.kind.as_u8());
    }
    Ok(())
}

fn append_wire_membership(
    encoded: &mut Vec<u8>,
    membership: &PublicPeerMembershipV2,
) -> Result<(), PeerLinkCryptoErrorV2> {
    append_wire_string(encoded, &membership.tunnel_id)?;
    append_wire_string(encoded, &membership.peer_id)?;
    encoded.extend_from_slice(&membership.overlay_ip.octets());
    append_wire_string(encoded, &membership.peer_public_key)?;
    append_wire_string(encoded, &membership.membership_signature)?;
    Ok(())
}

struct WireReaderV2<'a> {
    encoded: &'a [u8],
    cursor: usize,
}

impl<'a> WireReaderV2<'a> {
    fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, cursor: 0 }
    }

    fn read_exact(&mut self, size: usize) -> Result<&'a [u8], PeerLinkCryptoErrorV2> {
        let end = self
            .cursor
            .checked_add(size)
            .ok_or(PeerLinkCryptoErrorV2::InvalidWireEncoding)?;
        let value = self
            .encoded
            .get(self.cursor..end)
            .ok_or(PeerLinkCryptoErrorV2::InvalidWireEncoding)?;
        self.cursor = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, PeerLinkCryptoErrorV2> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, PeerLinkCryptoErrorV2> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], PeerLinkCryptoErrorV2> {
        self.read_exact(N)?
            .try_into()
            .map_err(|_| PeerLinkCryptoErrorV2::InvalidWireEncoding)
    }

    fn read_string(&mut self) -> Result<String, PeerLinkCryptoErrorV2> {
        let size = u32::from_be_bytes(self.read_array()?) as usize;
        if size > MAX_WIRE_STRING_SIZE_V2 {
            return Err(PeerLinkCryptoErrorV2::InvalidWireEncoding);
        }
        std::str::from_utf8(self.read_exact(size)?)
            .map(str::to_owned)
            .map_err(|_| PeerLinkCryptoErrorV2::InvalidWireEncoding)
    }

    fn read_candidates(&mut self) -> Result<Vec<Candidate>, PeerLinkCryptoErrorV2> {
        let count = usize::from(self.read_u8()?);
        let mut candidates = Vec::with_capacity(count);
        for _ in 0..count {
            let ip = self.read_string()?;
            let port = self.read_u16()?;
            let kind = CandidateKind::from_u8(self.read_u8()?)
                .ok_or(PeerLinkCryptoErrorV2::InvalidWireEncoding)?;
            candidates.push(Candidate { ip, port, kind });
        }
        validate_candidates(&candidates)?;
        Ok(candidates)
    }

    fn read_membership(&mut self) -> Result<PublicPeerMembershipV2, PeerLinkCryptoErrorV2> {
        Ok(PublicPeerMembershipV2 {
            tunnel_id: self.read_string()?,
            peer_id: self.read_string()?,
            overlay_ip: std::net::Ipv4Addr::from(self.read_array()?),
            peer_public_key: self.read_string()?,
            membership_signature: self.read_string()?,
        })
    }

    fn finish(self) -> Result<(), PeerLinkCryptoErrorV2> {
        if self.cursor == self.encoded.len() {
            Ok(())
        } else {
            Err(PeerLinkCryptoErrorV2::InvalidWireEncoding)
        }
    }
}

fn canonical_context(domain: &[u8], fields: &[&[u8]]) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
    let mut encoded = Vec::new();
    append_field(&mut encoded, domain)?;
    for field in fields {
        append_field(&mut encoded, field)?;
    }
    Ok(encoded)
}

fn canonical_candidates(candidates: &[Candidate]) -> Result<Vec<u8>, PeerLinkCryptoErrorV2> {
    let count =
        u32::try_from(candidates.len()).map_err(|_| ProvisioningError::CanonicalFieldTooLarge)?;
    let mut encoded = Vec::new();
    append_field(&mut encoded, &count.to_be_bytes())?;
    for candidate in candidates {
        append_field(&mut encoded, candidate.ip.as_bytes())?;
        append_field(&mut encoded, &candidate.port.to_be_bytes())?;
        append_field(&mut encoded, &[candidate.kind.as_u8()])?;
    }
    Ok(encoded)
}

fn validate_answer_result(accepted: bool, reason_code: u8) -> Result<(), PeerLinkCryptoErrorV2> {
    if accepted == (reason_code == 0) {
        Ok(())
    } else {
        Err(PeerLinkCryptoErrorV2::InvalidAnswerResult)
    }
}

fn validate_candidates(candidates: &[Candidate]) -> Result<(), PeerLinkCryptoErrorV2> {
    if candidates.len() > MAX_P2P_CANDIDATES_V2
        || candidates
            .iter()
            .any(|candidate| candidate.port == 0 || candidate.ip.parse::<IpAddr>().is_err())
    {
        Err(PeerLinkCryptoErrorV2::InvalidCandidate)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use std::fmt::Write as _;
    use std::net::Ipv4Addr;

    use crate::p2p_types::{Candidate, CandidateKind, CertFingerprint, SessionId};
    use crate::provisioning::{
        GatewayBootstrapV2, PeerProfileV2, PublicPeerMembershipV2, TunnelOwnerFileV2,
    };

    use super::{P2pAnswerV2, P2pOfferV2, PeerLinkEphemeralSecretV2};

    fn peer_profiles() -> (PeerProfileV2, PeerProfileV2, String) {
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
        let source = tunnel
            .add_peer(Some(Ipv4Addr::new(198, 18, 0, 1)), 1, None)
            .expect("source profile");
        let target = tunnel
            .add_peer(Some(Ipv4Addr::new(198, 18, 0, 2)), 1, None)
            .expect("target profile");
        (source, target, issuer)
    }

    fn candidate(ip: &str, port: u16) -> Candidate {
        Candidate {
            ip: ip.into(),
            port,
            kind: CandidateKind::ServerReflexive,
        }
    }

    fn to_hex(bytes: &[u8]) -> String {
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut encoded, "{byte:02x}").expect("write hex");
        }
        encoded
    }

    fn signed_offer(
        source: &PeerProfileV2,
        target: &PeerProfileV2,
        secret: &PeerLinkEphemeralSecretV2,
    ) -> P2pOfferV2 {
        P2pOfferV2::sign(
            source,
            SessionId::from_bytes([0x11; 16]),
            target.peer.peer_id.clone(),
            vec![
                candidate("203.0.113.10", 41000),
                candidate("203.0.113.11", 41001),
            ],
            CertFingerprint::from_bytes([0x22; 32]),
            secret,
        )
        .expect("sign offer")
    }

    fn signed_answer(
        target: &PeerProfileV2,
        offer: &P2pOfferV2,
        secret: &PeerLinkEphemeralSecretV2,
    ) -> P2pAnswerV2 {
        P2pAnswerV2::sign(
            target,
            offer,
            true,
            0,
            vec![
                candidate("198.51.100.20", 42000),
                candidate("198.51.100.21", 42001),
            ],
            CertFingerprint::from_bytes([0x33; 32]),
            secret,
        )
        .expect("sign answer")
    }

    #[test]
    fn signed_round_trip_derives_opposite_direction_keys() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = P2pOfferV2::sign(
            &source,
            SessionId::from_bytes([0x11; 16]),
            target.peer.peer_id.clone(),
            vec![candidate("203.0.113.10", 41000)],
            CertFingerprint::from_bytes([0x22; 32]),
            &source_secret,
        )
        .expect("sign offer");
        offer.verify(&issuer).expect("verify offer");

        let answer = P2pAnswerV2::sign(
            &target,
            &offer,
            true,
            0,
            vec![candidate("198.51.100.20", 42000)],
            CertFingerprint::from_bytes([0x33; 32]),
            &target_secret,
        )
        .expect("sign answer");
        answer
            .verify_for_offer(&offer, &issuer)
            .expect("verify answer");

        let source_keys = source_secret
            .derive_session_keys(&offer, &answer, &issuer)
            .expect("derive source keys");
        let target_keys = target_secret
            .derive_session_keys(&offer, &answer, &issuer)
            .expect("derive target keys");

        assert_eq!(source_keys.send_key(), target_keys.receive_key());
        assert_eq!(source_keys.receive_key(), target_keys.send_key());
        assert_ne!(source_keys.send_key(), source_keys.receive_key());
    }

    #[test]
    fn signed_offer_and_answer_wire_round_trip_preserves_verified_identity() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let answer = signed_answer(&target, &offer, &target_secret);

        let decoded_offer =
            P2pOfferV2::from_wire_bytes(&offer.to_wire_bytes().expect("encode Offer"))
                .expect("decode Offer");
        let decoded_answer =
            P2pAnswerV2::from_wire_bytes(&answer.to_wire_bytes().expect("encode Answer"))
                .expect("decode Answer");

        assert_eq!(decoded_offer, offer);
        assert_eq!(decoded_answer, answer);
        decoded_offer.verify(&issuer).expect("verify decoded Offer");
        decoded_answer
            .verify_for_offer(&decoded_offer, &issuer)
            .expect("verify decoded Answer");
    }

    #[test]
    fn signed_peerlink_wire_rejects_truncation_trailing_bytes_and_unknown_tags() {
        let (source, target, _) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let answer = signed_answer(&target, &offer, &target_secret);

        for mut encoded in [
            offer.to_wire_bytes().expect("encode Offer"),
            answer.to_wire_bytes().expect("encode Answer"),
        ] {
            let truncated = &encoded[..encoded.len() - 1];
            assert!(P2pOfferV2::from_wire_bytes(truncated).is_err());
            assert!(P2pAnswerV2::from_wire_bytes(truncated).is_err());

            encoded.push(0xff);
            assert!(P2pOfferV2::from_wire_bytes(&encoded).is_err());
            assert!(P2pAnswerV2::from_wire_bytes(&encoded).is_err());
        }

        let mut encoded = offer.to_wire_bytes().expect("encode Offer");
        encoded[0] = 0xff;
        assert!(P2pOfferV2::from_wire_bytes(&encoded).is_err());

        let mut encoded = answer.to_wire_bytes().expect("encode Answer");
        encoded[0] = 0xff;
        assert!(P2pAnswerV2::from_wire_bytes(&encoded).is_err());
    }

    #[test]
    fn tampering_any_offer_security_field_is_rejected() {
        let (source, target, issuer) = peer_profiles();
        let secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &secret);
        let mut cases = Vec::new();

        let mut changed = offer.clone();
        changed.tunnel_id.push('x');
        cases.push(("tunnel", changed));
        let mut changed = offer.clone();
        changed.session_id = SessionId::from_bytes([0x12; 16]);
        cases.push(("session", changed));
        let mut changed = offer.clone();
        changed.source_peer_id.push('x');
        cases.push(("source peer", changed));
        let mut changed = offer.clone();
        changed.target_peer_id.push('x');
        cases.push(("target peer", changed));
        let mut changed = offer.clone();
        changed.candidates[0].ip.push('1');
        cases.push(("candidate ip", changed));
        let mut changed = offer.clone();
        changed.candidates[0].port += 1;
        cases.push(("candidate port", changed));
        let mut changed = offer.clone();
        changed.candidates[0].kind = CandidateKind::Host;
        cases.push(("candidate kind", changed));
        let mut changed = offer.clone();
        changed.candidates.swap(0, 1);
        cases.push(("candidate order", changed));
        let mut changed = offer.clone();
        changed.direct_certificate_fingerprint = CertFingerprint::from_bytes([0x23; 32]);
        cases.push(("certificate fingerprint", changed));
        let mut changed = offer.clone();
        changed.ephemeral_x25519_public_key[0] ^= 1;
        cases.push(("ephemeral public key", changed));
        let mut changed = offer.clone();
        changed.peer_signature[0] ^= 1;
        cases.push(("Peer signature", changed));
        let mut changed = offer.clone();
        changed.public_peer_membership.tunnel_id.push('x');
        cases.push(("membership Tunnel", changed));
        let mut changed = offer.clone();
        changed.public_peer_membership.peer_id.push('x');
        cases.push(("membership Peer", changed));
        let mut changed = offer.clone();
        changed.public_peer_membership.overlay_ip = Ipv4Addr::new(198, 18, 0, 9);
        cases.push(("membership overlay", changed));
        let mut changed = offer.clone();
        changed.public_peer_membership.peer_public_key = target.peer.peer_public_key.clone();
        cases.push(("membership Peer key", changed));
        let mut changed = offer.clone();
        changed
            .public_peer_membership
            .membership_signature
            .push('A');
        cases.push(("membership signature", changed));

        for (field, changed) in cases {
            assert!(
                changed.verify(&issuer).is_err(),
                "tampered {field} unexpectedly verified"
            );
        }
    }

    #[test]
    fn tampering_any_answer_security_field_is_rejected() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let answer = signed_answer(&target, &offer, &target_secret);
        let mut cases = Vec::new();

        let mut changed = answer.clone();
        changed.offer_hash[0] ^= 1;
        cases.push(("signed Offer hash", changed));
        let mut changed = answer.clone();
        changed.accepted_peer_id.push('x');
        cases.push(("accepted peer", changed));
        let mut changed = answer.clone();
        changed.accepted = false;
        cases.push(("accepted flag", changed));
        let mut changed = answer.clone();
        changed.reason_code = 1;
        cases.push(("reason code", changed));
        let mut changed = answer.clone();
        changed.candidates[0].ip.push('1');
        cases.push(("candidate ip", changed));
        let mut changed = answer.clone();
        changed.candidates[0].port += 1;
        cases.push(("candidate port", changed));
        let mut changed = answer.clone();
        changed.candidates[0].kind = CandidateKind::Host;
        cases.push(("candidate kind", changed));
        let mut changed = answer.clone();
        changed.candidates.swap(0, 1);
        cases.push(("candidate order", changed));
        let mut changed = answer.clone();
        changed.direct_certificate_fingerprint = CertFingerprint::from_bytes([0x34; 32]);
        cases.push(("certificate fingerprint", changed));
        let mut changed = answer.clone();
        changed.ephemeral_x25519_public_key[0] ^= 1;
        cases.push(("ephemeral public key", changed));
        let mut changed = answer.clone();
        changed.peer_signature[0] ^= 1;
        cases.push(("Peer signature", changed));
        let mut changed = answer.clone();
        changed.public_peer_membership.tunnel_id.push('x');
        cases.push(("membership Tunnel", changed));
        let mut changed = answer.clone();
        changed.public_peer_membership.peer_id.push('x');
        cases.push(("membership Peer", changed));
        let mut changed = answer.clone();
        changed.public_peer_membership.overlay_ip = Ipv4Addr::new(198, 18, 0, 9);
        cases.push(("membership overlay", changed));
        let mut changed = answer.clone();
        changed.public_peer_membership.peer_public_key = source.peer.peer_public_key.clone();
        cases.push(("membership Peer key", changed));
        let mut changed = answer.clone();
        changed
            .public_peer_membership
            .membership_signature
            .push('A');
        cases.push(("membership signature", changed));

        for (field, changed) in cases {
            assert!(
                changed.verify_for_offer(&offer, &issuer).is_err(),
                "tampered {field} unexpectedly verified"
            );
        }
    }

    #[test]
    fn wrong_issuer_and_wrong_peer_signing_key_are_rejected() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let answer = signed_answer(&target, &offer, &target_secret);

        let other_tunnel = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "other-gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("other-gateway.example".into()),
            trusted_certificate_pem: None,
        })
        .expect("generate other tunnel");
        let wrong_issuer = other_tunnel
            .scope()
            .expect("other scope")
            .tunnel_signing_public_key;
        assert!(offer.verify(&wrong_issuer).is_err());
        assert!(answer.verify_for_offer(&offer, &wrong_issuer).is_err());

        let mut offer_with_wrong_peer_key = offer.clone();
        offer_with_wrong_peer_key.peer_signature = target
            .sign_peer_message_v2(
                &offer_with_wrong_peer_key
                    .canonical_unsigned()
                    .expect("canonical offer"),
            )
            .expect("sign with wrong Peer key");
        assert_eq!(
            offer_with_wrong_peer_key.verify(&issuer),
            Err(super::PeerLinkCryptoErrorV2::InvalidPeerSignature)
        );

        let mut answer_with_wrong_peer_key = answer.clone();
        answer_with_wrong_peer_key.peer_signature = source
            .sign_peer_message_v2(
                &answer_with_wrong_peer_key
                    .canonical_unsigned()
                    .expect("canonical answer"),
            )
            .expect("sign with wrong Peer key");
        assert_eq!(
            answer_with_wrong_peer_key.verify_for_offer(&offer, &issuer),
            Err(super::PeerLinkCryptoErrorV2::InvalidPeerSignature)
        );
    }

    #[test]
    fn answer_cannot_be_substituted_between_offers() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let first_offer = signed_offer(&source, &target, &source_secret);
        let answer = signed_answer(&target, &first_offer, &target_secret);

        let other_source_secret = PeerLinkEphemeralSecretV2::generate();
        let mut second_offer = signed_offer(&source, &target, &other_source_secret);
        second_offer.session_id = SessionId::from_bytes([0x44; 16]);
        second_offer.peer_signature = source
            .sign_peer_message_v2(
                &second_offer
                    .canonical_unsigned()
                    .expect("canonical second offer"),
            )
            .expect("sign second offer");

        assert_eq!(
            answer.verify_for_offer(&second_offer, &issuer),
            Err(super::PeerLinkCryptoErrorV2::OfferSubstitution)
        );
    }

    #[test]
    fn all_zero_x25519_shared_secret_is_rejected() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let mut answer = signed_answer(&target, &offer, &target_secret);
        answer.ephemeral_x25519_public_key = [0; 32];
        answer.peer_signature = target
            .sign_peer_message_v2(
                &answer
                    .canonical_unsigned()
                    .expect("canonical zero-key answer"),
            )
            .expect("sign zero-key answer");
        answer
            .verify_for_offer(&offer, &issuer)
            .expect("zero key is signed but not contributory");

        let result = source_secret.derive_session_keys(&offer, &answer, &issuer);
        assert!(matches!(
            result,
            Err(super::PeerLinkCryptoErrorV2::NonContributorySharedSecret)
        ));
    }

    #[test]
    fn answer_acceptance_and_reason_code_must_agree() {
        let (source, target, _) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);

        let secret = PeerLinkEphemeralSecretV2::generate();
        assert!(P2pAnswerV2::sign(
            &target,
            &offer,
            true,
            1,
            Vec::new(),
            CertFingerprint::zero(),
            &secret,
        )
        .is_err());
        let secret = PeerLinkEphemeralSecretV2::generate();
        assert!(P2pAnswerV2::sign(
            &target,
            &offer,
            false,
            0,
            Vec::new(),
            CertFingerprint::zero(),
            &secret,
        )
        .is_err());
    }

    #[test]
    fn rejected_answer_verifies_but_does_not_derive_session_keys() {
        let (source, target, issuer) = peer_profiles();
        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let answer = P2pAnswerV2::sign(
            &target,
            &offer,
            false,
            1,
            Vec::new(),
            CertFingerprint::zero(),
            &target_secret,
        )
        .expect("sign rejection");
        answer
            .verify_for_offer(&offer, &issuer)
            .expect("verify signed rejection");

        assert!(matches!(
            source_secret.derive_session_keys(&offer, &answer, &issuer),
            Err(super::PeerLinkCryptoErrorV2::PeerLinkRejected(1))
        ));
    }

    #[test]
    fn candidate_lists_are_bounded_and_contain_valid_socket_parts() {
        let (source, target, _) = peer_profiles();
        for candidates in [
            vec![candidate("not-an-ip", 41000)],
            vec![candidate("203.0.113.10", 0)],
            vec![candidate("203.0.113.10", 41000); 256],
        ] {
            let secret = PeerLinkEphemeralSecretV2::generate();
            assert!(P2pOfferV2::sign(
                &source,
                SessionId::from_bytes([0x11; 16]),
                target.peer.peer_id.clone(),
                candidates,
                CertFingerprint::from_bytes([0x22; 32]),
                &secret,
            )
            .is_err());
        }

        let source_secret = PeerLinkEphemeralSecretV2::generate();
        let offer = signed_offer(&source, &target, &source_secret);
        let target_secret = PeerLinkEphemeralSecretV2::generate();
        assert!(P2pAnswerV2::sign(
            &target,
            &offer,
            true,
            0,
            vec![candidate("not-an-ip", 42000)],
            CertFingerprint::from_bytes([0x33; 32]),
            &target_secret,
        )
        .is_err());
    }

    #[test]
    fn signed_offer_and_answer_canonical_encoding_matches_golden_fixture() {
        let membership = |overlay_ip| PublicPeerMembershipV2 {
            tunnel_id: "t".into(),
            peer_id: if overlay_ip == Ipv4Addr::new(198, 18, 0, 1) {
                "a".into()
            } else {
                "bb".into()
            },
            overlay_ip,
            peer_public_key: STANDARD.encode([0x77; 32]),
            membership_signature: STANDARD.encode([0x88; 64]),
        };
        let offer = P2pOfferV2 {
            tunnel_id: "t".into(),
            session_id: SessionId::from_bytes([
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f,
            ]),
            source_peer_id: "a".into(),
            target_peer_id: "bb".into(),
            candidates: vec![Candidate {
                ip: "127.0.0.1".into(),
                port: 443,
                kind: CandidateKind::Host,
            }],
            direct_certificate_fingerprint: CertFingerprint::from_bytes([0x11; 32]),
            public_peer_membership: membership(Ipv4Addr::new(198, 18, 0, 1)),
            ephemeral_x25519_public_key: [0x22; 32],
            peer_signature: [0x33; 64],
        };
        let actual = to_hex(&offer.canonical_unsigned().expect("canonical offer"));
        let expected_unsigned = concat!(
            "000000126c616e74756e6e656c2e6f666665722e7632",
            "0000000174",
            "00000010000102030405060708090a0b0c0d0e0f",
            "0000000161",
            "000000026262",
            "00000020",
            "0000000400000001",
            "000000093132372e302e302e31",
            "0000000201bb",
            "0000000101",
            "000000201111111111111111111111111111111111111111111111111111111111111111",
            "0000009c",
            "0000001e6c616e74756e6e656c2e7075626c69632d6d656d626572736869702e7632",
            "0000000174",
            "0000000161",
            "00000004c6120001",
            "000000207777777777777777777777777777777777777777777777777777777777777777",
            "0000004088888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888",
            "000000202222222222222222222222222222222222222222222222222222222222222222",
        );
        assert_eq!(actual, expected_unsigned);
        let expected_signed = format!("{expected_unsigned}00000040{}", "33".repeat(64));
        assert_eq!(
            to_hex(&offer.canonical_signed().expect("canonical signed offer")),
            expected_signed
        );

        let answer = P2pAnswerV2 {
            offer_hash: offer.signed_hash().expect("signed Offer hash"),
            accepted_peer_id: "bb".into(),
            accepted: true,
            reason_code: 0,
            candidates: Vec::new(),
            direct_certificate_fingerprint: CertFingerprint::from_bytes([0x44; 32]),
            public_peer_membership: membership(Ipv4Addr::new(198, 18, 0, 2)),
            ephemeral_x25519_public_key: [0x55; 32],
            peer_signature: [0x66; 64],
        };
        let expected_answer_unsigned = concat!(
            "000000136c616e74756e6e656c2e616e737765722e7632",
            "00000020",
            // This hash changes whenever the complete signed Offer fixture changes.
            "PLACEHOLDER_OFFER_HASH",
            "000000026262",
            "0000000101",
            "0000000100",
            "000000080000000400000000",
            "000000204444444444444444444444444444444444444444444444444444444444444444",
            "0000009d",
            "0000001e6c616e74756e6e656c2e7075626c69632d6d656d626572736869702e7632",
            "0000000174",
            "000000026262",
            "00000004c6120002",
            "000000207777777777777777777777777777777777777777777777777777777777777777",
            "0000004088888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888",
            "000000205555555555555555555555555555555555555555555555555555555555555555",
        );
        let expected_answer_unsigned = expected_answer_unsigned.replace(
            "PLACEHOLDER_OFFER_HASH",
            &to_hex(&offer.signed_hash().expect("signed Offer hash")),
        );
        let expected_answer_signed =
            format!("{expected_answer_unsigned}00000040{}", "66".repeat(64));
        assert_eq!(
            to_hex(&answer.canonical_signed().expect("canonical signed answer")),
            expected_answer_signed
        );
    }
}
