//! Minimal Lantunnel 2.0 provisioning artifacts.
//!
//! This module deliberately owns only the offline trust chain:
//! `.tunnel` -> `.scope` and `.peer`. Runtime admission, transport, proxying,
//! and Platform management remain outside this module.

use std::collections::HashSet;
use std::io::Cursor;
use std::net::Ipv4Addr;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::config::DEFAULT_GATEWAY_MAPPING_PROBE_PORT;

pub const PROVISIONING_VERSION_V2: u16 = 2;
pub const MAX_REPLICA_HINT_V2: u16 = 8;
pub const OVERLAY_NETWORK_V2: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 0);
pub const OVERLAY_BROADCAST_V2: Ipv4Addr = Ipv4Addr::new(198, 18, 255, 255);

const MEMBERSHIP_DOMAIN_V2: &[u8] = b"lantunnel.peer.v2";
const PUBLIC_MEMBERSHIP_DOMAIN_V2: &[u8] = b"lantunnel.public-membership.v2";
const ATTACHMENT_PROOF_DOMAIN_V2: &[u8] = b"lantunnel.attach.v2";
const MANAGED_RESOLVE_PROOF_DOMAIN_V2: &[u8] = b"lantunnel.resolve.v2";
const PLATFORM_HEARTBEAT_PROOF_DOMAIN_V2: &[u8] = b"lantunnel.platform-heartbeat.v2";
const ED25519_PUBLIC_KEY_LEN: usize = 32;
const JAVASCRIPT_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayBootstrapV2 {
    pub transport: String,
    pub dial_address: String,
    pub port: u16,
    /// UDP port this Gateway's host reflects P2P mapping probes on. `None`
    /// means the host never moved off the shared default, which is what every
    /// Gateway registered before the port became a fact reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_certificate_pem: Option<String>,
}

impl GatewayBootstrapV2 {
    pub fn validate(&self) -> Result<(), ProvisioningError> {
        match self.transport.as_str() {
            "quic" | "websocket" | "grpc" => {}
            _ => return Err(ProvisioningError::UnsupportedTransport),
        }
        if self.dial_address.trim().is_empty() || self.port == 0 {
            return Err(ProvisioningError::InvalidGatewayAddress);
        }
        let mapping_port = self
            .mapping_port
            .unwrap_or(DEFAULT_GATEWAY_MAPPING_PROBE_PORT);
        if mapping_port == 0 || (self.transport == "quic" && mapping_port == self.port) {
            return Err(ProvisioningError::InvalidGatewayAddress);
        }
        if self
            .tls_server_name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(ProvisioningError::InvalidGatewayAddress);
        }
        if let Some(pem) = &self.trusted_certificate_pem {
            normalize_certificate_pem(pem)?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelOwnerFileV2 {
    pub version: u16,
    pub tunnel_id: String,
    pub static_gateway: GatewayBootstrapV2,
    /// Wiped on drop; the file it comes from is mode 0600.
    pub tunnel_signing_private_key: Zeroizing<String>,
    #[serde(default)]
    pub allocated_peers: Vec<AllocatedPeerV2>,
}

impl TunnelOwnerFileV2 {
    pub fn generate(static_gateway: GatewayBootstrapV2) -> Result<Self, ProvisioningError> {
        static_gateway.validate()?;
        let key_pair = generate_key_pair()?;
        Ok(Self {
            version: PROVISIONING_VERSION_V2,
            tunnel_id: Uuid::new_v4().to_string(),
            static_gateway,
            tunnel_signing_private_key: Zeroizing::new(STANDARD.encode(key_pair.pkcs8)),
            allocated_peers: Vec::new(),
        })
    }

    pub fn verify(&self) -> Result<(), ProvisioningError> {
        validate_version(self.version)?;
        if self.tunnel_id.trim().is_empty() {
            return Err(ProvisioningError::InvalidIdentifier);
        }
        self.static_gateway.validate()?;
        let _ = decode_key_pair(&self.tunnel_signing_private_key)?;

        let mut peer_ids = HashSet::with_capacity(self.allocated_peers.len());
        let mut overlay_ips = HashSet::with_capacity(self.allocated_peers.len());
        for peer in &self.allocated_peers {
            if peer.peer_id.trim().is_empty()
                || !is_usable_overlay(peer.overlay_ip)
                || !peer_ids.insert(peer.peer_id.as_str())
                || !overlay_ips.insert(peer.overlay_ip)
                || decode_public_key(&peer.peer_public_key).is_err()
            {
                return Err(ProvisioningError::InvalidAllocation);
            }
        }
        Ok(())
    }

    pub fn scope(&self) -> Result<GatewayScopeFileV2, ProvisioningError> {
        self.verify()?;
        let key_pair = decode_key_pair(&self.tunnel_signing_private_key)?;
        Ok(GatewayScopeFileV2 {
            version: PROVISIONING_VERSION_V2,
            tunnel_id: self.tunnel_id.clone(),
            tunnel_signing_public_key: STANDARD.encode(key_pair.key_pair.public_key().as_ref()),
        })
    }

    pub fn add_peer(
        &mut self,
        requested_overlay_ip: Option<Ipv4Addr>,
        replicas: u16,
        label: Option<String>,
    ) -> Result<PeerProfileV2, ProvisioningError> {
        self.verify()?;
        if replicas == 0 || replicas > MAX_REPLICA_HINT_V2 {
            return Err(ProvisioningError::InvalidReplicas);
        }

        let overlay_ip = match requested_overlay_ip {
            Some(ip) => {
                if !is_usable_overlay(ip)
                    || self
                        .allocated_peers
                        .iter()
                        .any(|allocation| allocation.overlay_ip == ip)
                {
                    return Err(ProvisioningError::OverlayUnavailable);
                }
                ip
            }
            None => self.next_overlay_ip()?,
        };

        let tunnel_key_pair = decode_key_pair(&self.tunnel_signing_private_key)?;
        let peer_key_pair = generate_key_pair()?;
        let peer_id = Uuid::new_v4().to_string();
        let peer_public_key = STANDARD.encode(&peer_key_pair.public_key);
        let membership = encode_peer_membership_v2(
            &self.tunnel_id,
            &peer_id,
            overlay_ip,
            &peer_key_pair.public_key,
        )?;
        let membership_signature = STANDARD.encode(tunnel_key_pair.key_pair.sign(&membership));
        let tunnel_signing_public_key =
            STANDARD.encode(tunnel_key_pair.key_pair.public_key().as_ref());
        let peer_private_key = Zeroizing::new(STANDARD.encode(&peer_key_pair.pkcs8));

        let profile = PeerProfileV2 {
            version: PROVISIONING_VERSION_V2,
            tunnel_id: self.tunnel_id.clone(),
            tunnel_signing_public_key,
            replicas,
            peer: PeerIdentityV2 {
                peer_id: peer_id.clone(),
                overlay_ip,
                peer_private_key,
                peer_public_key: peer_public_key.clone(),
                membership_signature,
            },
            bootstrap: PeerBootstrapV2::StaticGateway(self.static_gateway.clone()),
        };
        profile.verify()?;

        self.allocated_peers.push(AllocatedPeerV2 {
            peer_id,
            overlay_ip,
            peer_public_key,
            label: label.filter(|value| !value.trim().is_empty()),
        });
        Ok(profile)
    }

    fn next_overlay_ip(&self) -> Result<Ipv4Addr, ProvisioningError> {
        let allocated: HashSet<Ipv4Addr> = self
            .allocated_peers
            .iter()
            .map(|peer| peer.overlay_ip)
            .collect();
        let first = u32::from(OVERLAY_NETWORK_V2) + 1;
        let last = u32::from(OVERLAY_BROADCAST_V2);
        (first..last)
            .map(Ipv4Addr::from)
            .find(|ip| !allocated.contains(ip))
            .ok_or(ProvisioningError::OverlayPoolExhausted)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocatedPeerV2 {
    pub peer_id: String,
    pub overlay_ip: Ipv4Addr,
    pub peer_public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayScopeFileV2 {
    pub version: u16,
    pub tunnel_id: String,
    pub tunnel_signing_public_key: String,
}

impl GatewayScopeFileV2 {
    pub fn verify(&self) -> Result<(), ProvisioningError> {
        validate_version(self.version)?;
        if self.tunnel_id.trim().is_empty() {
            return Err(ProvisioningError::InvalidIdentifier);
        }
        decode_public_key(&self.tunnel_signing_public_key)?;
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerProfileV2 {
    pub version: u16,
    pub tunnel_id: String,
    pub tunnel_signing_public_key: String,
    pub replicas: u16,
    pub peer: PeerIdentityV2,
    pub bootstrap: PeerBootstrapV2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlatformHeartbeatPathModeV2 {
    Direct,
    Relay,
    Connecting,
    Disconnected,
}

impl PlatformHeartbeatPathModeV2 {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::Connecting => "connecting",
            Self::Disconnected => "disconnected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformHeartbeatProofV2<'a> {
    pub tunnel_id: &'a str,
    pub peer_id: &'a str,
    pub request_id: &'a str,
    pub timestamp_ms: u64,
    pub client_version: &'a str,
    pub final_heartbeat: bool,
    pub transport_active: bool,
    pub path_mode: PlatformHeartbeatPathModeV2,
}

impl PeerProfileV2 {
    pub fn public_membership(&self) -> PublicPeerMembershipV2 {
        PublicPeerMembershipV2 {
            tunnel_id: self.tunnel_id.clone(),
            peer_id: self.peer.peer_id.clone(),
            overlay_ip: self.peer.overlay_ip,
            peer_public_key: self.peer.peer_public_key.clone(),
            membership_signature: self.peer.membership_signature.clone(),
        }
    }

    pub fn verify(&self) -> Result<(), ProvisioningError> {
        validate_version(self.version)?;
        if self.tunnel_id.trim().is_empty()
            || self.peer.peer_id.trim().is_empty()
            || self.replicas == 0
            || self.replicas > MAX_REPLICA_HINT_V2
            || !is_usable_overlay(self.peer.overlay_ip)
        {
            return Err(ProvisioningError::InvalidPeerProfile);
        }
        match &self.bootstrap {
            PeerBootstrapV2::StaticGateway(gateway) => gateway.validate()?,
            PeerBootstrapV2::ManagedPlatform { platform_url } => {
                if !(platform_url.starts_with("https://") && platform_url.len() > 8) {
                    return Err(ProvisioningError::InvalidPlatformUrl);
                }
            }
        }

        let peer_public_key = decode_public_key(&self.peer.peer_public_key)?;
        let peer_key_pair = decode_key_pair(&self.peer.peer_private_key)?;
        if peer_key_pair.key_pair.public_key().as_ref() != peer_public_key {
            return Err(ProvisioningError::PeerKeyMismatch);
        }
        self.public_membership()
            .verify(&self.tunnel_signing_public_key)
    }

    /// Proves possession of this Peer key for one Gateway challenge and one
    /// runtime Replica handle. The public membership stays independently
    /// verifiable by the Tunnel issuer key in `.scope`.
    pub fn sign_attachment_proof(
        &self,
        challenge: &[u8; 32],
        replica_id: &str,
    ) -> Result<String, ProvisioningError> {
        self.verify()?;
        let peer_key_pair = decode_key_pair(&self.peer.peer_private_key)?;
        let message = encode_attachment_proof_v2(&self.public_membership(), challenge, replica_id)?;
        Ok(STANDARD.encode(peer_key_pair.key_pair.sign(&message)))
    }

    /// Proves possession of this Peer key to the Managed Platform while
    /// resolving the Tunnel's current Gateway. `request_id` is correlation
    /// data, not a persisted replay authority.
    pub fn sign_managed_resolve_proof(
        &self,
        timestamp: u64,
        request_id: &str,
    ) -> Result<String, ProvisioningError> {
        self.verify()?;
        let peer_key_pair = decode_key_pair(&self.peer.peer_private_key)?;
        let message =
            encode_managed_resolve_proof_v2(&self.public_membership(), timestamp, request_id)?;
        Ok(STANDARD.encode(peer_key_pair.key_pair.sign(&message)))
    }

    pub fn sign_platform_heartbeat_proof(
        &self,
        input: &PlatformHeartbeatProofV2<'_>,
    ) -> Result<String, ProvisioningError> {
        self.verify()?;
        if input.tunnel_id != self.tunnel_id || input.peer_id != self.peer.peer_id {
            return Err(ProvisioningError::InvalidPlatformHeartbeatProof);
        }
        let peer_key_pair = decode_key_pair(&self.peer.peer_private_key)?;
        let message = encode_platform_heartbeat_proof_v2(input)?;
        Ok(STANDARD.encode(peer_key_pair.key_pair.sign(&message)))
    }

    pub(crate) fn sign_peer_message_v2(
        &self,
        message: &[u8],
    ) -> Result<[u8; 64], ProvisioningError> {
        self.verify()?;
        let peer_key_pair = decode_key_pair(&self.peer.peer_private_key)?;
        peer_key_pair
            .key_pair
            .sign(message)
            .as_ref()
            .try_into()
            .map_err(|_| ProvisioningError::InvalidKey)
    }
}

/// The only Peer identity material sent to Platform, Gateway, or another Peer.
/// It deliberately excludes the Peer private key and Gateway bootstrap facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicPeerMembershipV2 {
    pub tunnel_id: String,
    pub peer_id: String,
    pub overlay_ip: Ipv4Addr,
    pub peer_public_key: String,
    pub membership_signature: String,
}

impl PublicPeerMembershipV2 {
    pub fn verify(&self, tunnel_signing_public_key: &str) -> Result<(), ProvisioningError> {
        if self.tunnel_id.trim().is_empty()
            || self.peer_id.trim().is_empty()
            || !is_usable_overlay(self.overlay_ip)
        {
            return Err(ProvisioningError::InvalidPeerProfile);
        }
        let tunnel_public_key = decode_public_key(tunnel_signing_public_key)?;
        let peer_public_key = decode_public_key(&self.peer_public_key)?;
        let membership = encode_peer_membership_v2(
            &self.tunnel_id,
            &self.peer_id,
            self.overlay_ip,
            &peer_public_key,
        )?;
        let signature = STANDARD
            .decode(&self.membership_signature)
            .map_err(|_| ProvisioningError::InvalidMembershipSignature)?;
        UnparsedPublicKey::new(&ED25519, tunnel_public_key)
            .verify(&membership, &signature)
            .map_err(|_| ProvisioningError::InvalidMembershipSignature)
    }

    pub fn verify_attachment_proof(
        &self,
        challenge: &[u8; 32],
        replica_id: &str,
        signature: &str,
    ) -> Result<(), ProvisioningError> {
        let peer_public_key = decode_public_key(&self.peer_public_key)?;
        let signature = STANDARD
            .decode(signature)
            .map_err(|_| ProvisioningError::InvalidAttachmentProof)?;
        let message = encode_attachment_proof_v2(self, challenge, replica_id)?;
        UnparsedPublicKey::new(&ED25519, peer_public_key)
            .verify(&message, &signature)
            .map_err(|_| ProvisioningError::InvalidAttachmentProof)
    }

    pub fn verify_managed_resolve_proof(
        &self,
        timestamp: u64,
        request_id: &str,
        signature: &str,
    ) -> Result<(), ProvisioningError> {
        let peer_public_key = decode_public_key(&self.peer_public_key)?;
        let signature = STANDARD
            .decode(signature)
            .map_err(|_| ProvisioningError::InvalidManagedResolveProof)?;
        let message = encode_managed_resolve_proof_v2(self, timestamp, request_id)?;
        UnparsedPublicKey::new(&ED25519, peer_public_key)
            .verify(&message, &signature)
            .map_err(|_| ProvisioningError::InvalidManagedResolveProof)
    }

    pub fn verify_platform_heartbeat_proof(
        &self,
        input: &PlatformHeartbeatProofV2<'_>,
        signature: &str,
    ) -> Result<(), ProvisioningError> {
        if input.tunnel_id != self.tunnel_id || input.peer_id != self.peer_id {
            return Err(ProvisioningError::InvalidPlatformHeartbeatProof);
        }
        let peer_public_key = decode_public_key(&self.peer_public_key)?;
        let signature = STANDARD
            .decode(signature)
            .map_err(|_| ProvisioningError::InvalidPlatformHeartbeatProof)?;
        let message = encode_platform_heartbeat_proof_v2(input)?;
        UnparsedPublicKey::new(&ED25519, peer_public_key)
            .verify(&message, &signature)
            .map_err(|_| ProvisioningError::InvalidPlatformHeartbeatProof)
    }

    pub(crate) fn verify_peer_message_v2(
        &self,
        message: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), ProvisioningError> {
        let peer_public_key = decode_public_key(&self.peer_public_key)?;
        UnparsedPublicKey::new(&ED25519, peer_public_key)
            .verify(message, signature)
            .map_err(|_| ProvisioningError::InvalidAttachmentProof)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerIdentityV2 {
    pub peer_id: String,
    pub overlay_ip: Ipv4Addr,
    /// Wiped on drop; the file it comes from is mode 0600.
    pub peer_private_key: Zeroizing<String>,
    pub peer_public_key: String,
    pub membership_signature: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PeerBootstrapV2 {
    StaticGateway(GatewayBootstrapV2),
    ManagedPlatform { platform_url: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProvisioningError {
    #[error("unsupported Gateway transport")]
    UnsupportedTransport,
    #[error("invalid Gateway address")]
    InvalidGatewayAddress,
    #[error("trusted Gateway PEM must contain certificates only")]
    InvalidCertificatePem,
    #[error("unsupported provisioning version")]
    UnsupportedVersion,
    #[error("invalid identifier")]
    InvalidIdentifier,
    #[error("invalid Ed25519 key")]
    InvalidKey,
    #[error("invalid Peer allocation")]
    InvalidAllocation,
    #[error("replicas must be between 1 and 8")]
    InvalidReplicas,
    #[error("requested Overlay address is unavailable")]
    OverlayUnavailable,
    #[error("Overlay pool is exhausted")]
    OverlayPoolExhausted,
    #[error("invalid Peer profile")]
    InvalidPeerProfile,
    #[error("Peer private and public keys do not match")]
    PeerKeyMismatch,
    #[error("invalid Tunnel membership signature")]
    InvalidMembershipSignature,
    #[error("invalid Peer attachment proof")]
    InvalidAttachmentProof,
    #[error("invalid Managed Gateway resolve proof")]
    InvalidManagedResolveProof,
    #[error("invalid Platform heartbeat proof")]
    InvalidPlatformHeartbeatProof,
    #[error("Managed Platform URL must use HTTPS")]
    InvalidPlatformUrl,
    #[error("random key generation failed")]
    RandomGeneration,
    #[error("canonical membership field is too large")]
    CanonicalFieldTooLarge,
    #[error("certificate PEM could not be read")]
    CertificateRead,
}

struct GeneratedKeyPair {
    pkcs8: Vec<u8>,
    public_key: Vec<u8>,
}

struct DecodedKeyPair {
    key_pair: Ed25519KeyPair,
}

fn generate_key_pair() -> Result<GeneratedKeyPair, ProvisioningError> {
    let rng = SystemRandom::new();
    let pkcs8 =
        Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| ProvisioningError::RandomGeneration)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| ProvisioningError::RandomGeneration)?;
    Ok(GeneratedKeyPair {
        pkcs8: pkcs8.as_ref().to_vec(),
        public_key: key_pair.public_key().as_ref().to_vec(),
    })
}

fn decode_key_pair(encoded: &str) -> Result<DecodedKeyPair, ProvisioningError> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ProvisioningError::InvalidKey)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| ProvisioningError::InvalidKey)?;
    Ok(DecodedKeyPair { key_pair })
}

fn decode_public_key(encoded: &str) -> Result<Vec<u8>, ProvisioningError> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ProvisioningError::InvalidKey)?;
    if bytes.len() != ED25519_PUBLIC_KEY_LEN {
        return Err(ProvisioningError::InvalidKey);
    }
    Ok(bytes)
}

pub fn encode_peer_membership_v2(
    tunnel_id: &str,
    peer_id: &str,
    overlay_ip: Ipv4Addr,
    peer_public_key: &[u8],
) -> Result<Vec<u8>, ProvisioningError> {
    let mut encoded = Vec::with_capacity(
        MEMBERSHIP_DOMAIN_V2.len() + tunnel_id.len() + peer_id.len() + peer_public_key.len() + 20,
    );
    append_field(&mut encoded, MEMBERSHIP_DOMAIN_V2)?;
    append_field(&mut encoded, tunnel_id.as_bytes())?;
    append_field(&mut encoded, peer_id.as_bytes())?;
    append_field(&mut encoded, &overlay_ip.octets())?;
    append_field(&mut encoded, peer_public_key)?;
    Ok(encoded)
}

fn encode_attachment_proof_v2(
    membership: &PublicPeerMembershipV2,
    challenge: &[u8; 32],
    replica_id: &str,
) -> Result<Vec<u8>, ProvisioningError> {
    if replica_id.trim().is_empty() {
        return Err(ProvisioningError::InvalidIdentifier);
    }
    let membership_hash = Sha256::digest(encode_public_membership_v2(membership)?);
    let mut encoded = Vec::with_capacity(
        ATTACHMENT_PROOF_DOMAIN_V2.len()
            + challenge.len()
            + membership.tunnel_id.len()
            + membership.peer_id.len()
            + replica_id.len()
            + membership_hash.len()
            + 24,
    );
    append_field(&mut encoded, ATTACHMENT_PROOF_DOMAIN_V2)?;
    append_field(&mut encoded, challenge)?;
    append_field(&mut encoded, membership.tunnel_id.as_bytes())?;
    append_field(&mut encoded, membership.peer_id.as_bytes())?;
    append_field(&mut encoded, replica_id.as_bytes())?;
    append_field(&mut encoded, membership_hash.as_ref())?;
    Ok(encoded)
}

pub fn encode_managed_resolve_proof_v2(
    membership: &PublicPeerMembershipV2,
    timestamp: u64,
    request_id: &str,
) -> Result<Vec<u8>, ProvisioningError> {
    if request_id.trim().is_empty() {
        return Err(ProvisioningError::InvalidIdentifier);
    }
    let membership_hash = Sha256::digest(encode_public_membership_v2(membership)?);
    let mut encoded = Vec::with_capacity(
        MANAGED_RESOLVE_PROOF_DOMAIN_V2.len()
            + membership_hash.len()
            + std::mem::size_of::<u64>()
            + request_id.len()
            + 16,
    );
    append_field(&mut encoded, MANAGED_RESOLVE_PROOF_DOMAIN_V2)?;
    append_field(&mut encoded, membership_hash.as_ref())?;
    append_field(&mut encoded, &timestamp.to_be_bytes())?;
    append_field(&mut encoded, request_id.as_bytes())?;
    Ok(encoded)
}

pub fn encode_platform_heartbeat_proof_v2(
    input: &PlatformHeartbeatProofV2<'_>,
) -> Result<Vec<u8>, ProvisioningError> {
    if Uuid::parse_str(input.tunnel_id).is_err()
        || Uuid::parse_str(input.peer_id).is_err()
        || Uuid::parse_str(input.request_id).is_err()
        || input.timestamp_ms > JAVASCRIPT_MAX_SAFE_INTEGER
        || input.client_version.is_empty()
        || input.client_version.len() > 64
        || input.client_version.trim() != input.client_version
        || (input.final_heartbeat
            && (input.transport_active
                || input.path_mode != PlatformHeartbeatPathModeV2::Disconnected))
    {
        return Err(ProvisioningError::InvalidPlatformHeartbeatProof);
    }

    let mut encoded = Vec::with_capacity(
        PLATFORM_HEARTBEAT_PROOF_DOMAIN_V2.len()
            + input.tunnel_id.len()
            + input.peer_id.len()
            + input.request_id.len()
            + input.client_version.len()
            + input.path_mode.as_str().len()
            + 48,
    );
    append_field(&mut encoded, PLATFORM_HEARTBEAT_PROOF_DOMAIN_V2)?;
    append_field(&mut encoded, input.tunnel_id.as_bytes())?;
    append_field(&mut encoded, input.peer_id.as_bytes())?;
    append_field(&mut encoded, input.request_id.as_bytes())?;
    append_field(&mut encoded, &input.timestamp_ms.to_be_bytes())?;
    append_field(&mut encoded, input.client_version.as_bytes())?;
    append_field(&mut encoded, &[u8::from(input.final_heartbeat)])?;
    append_field(&mut encoded, &[u8::from(input.transport_active)])?;
    append_field(&mut encoded, input.path_mode.as_str().as_bytes())?;
    Ok(encoded)
}

pub(crate) fn encode_public_membership_v2(
    membership: &PublicPeerMembershipV2,
) -> Result<Vec<u8>, ProvisioningError> {
    let peer_public_key = decode_public_key(&membership.peer_public_key)?;
    let membership_signature = STANDARD
        .decode(&membership.membership_signature)
        .map_err(|_| ProvisioningError::InvalidMembershipSignature)?;
    let mut encoded = Vec::with_capacity(
        PUBLIC_MEMBERSHIP_DOMAIN_V2.len()
            + membership.tunnel_id.len()
            + membership.peer_id.len()
            + peer_public_key.len()
            + membership_signature.len()
            + 24,
    );
    append_field(&mut encoded, PUBLIC_MEMBERSHIP_DOMAIN_V2)?;
    append_field(&mut encoded, membership.tunnel_id.as_bytes())?;
    append_field(&mut encoded, membership.peer_id.as_bytes())?;
    append_field(&mut encoded, &membership.overlay_ip.octets())?;
    append_field(&mut encoded, &peer_public_key)?;
    append_field(&mut encoded, &membership_signature)?;
    Ok(encoded)
}

/// Parses a certificate-only PEM bundle and renders one stable PEM block per
/// certificate. Private keys and every other PEM item are rejected.
pub fn normalize_certificate_pem(pem: &str) -> Result<String, ProvisioningError> {
    let mut certificates = Vec::new();
    for item in rustls_pemfile::read_all(&mut Cursor::new(pem.as_bytes())) {
        match item.map_err(|_| ProvisioningError::CertificateRead)? {
            rustls_pemfile::Item::X509Certificate(certificate) => {
                certificates.push(certificate.as_ref().to_vec());
            }
            _ => return Err(ProvisioningError::InvalidCertificatePem),
        }
    }
    if certificates.is_empty() {
        return Err(ProvisioningError::InvalidCertificatePem);
    }

    let mut normalized = String::new();
    for certificate in certificates {
        normalized.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = STANDARD.encode(certificate);
        for line in encoded.as_bytes().chunks(64) {
            normalized.push_str(
                std::str::from_utf8(line).map_err(|_| ProvisioningError::InvalidCertificatePem)?,
            );
            normalized.push('\n');
        }
        normalized.push_str("-----END CERTIFICATE-----\n");
    }
    Ok(normalized)
}

pub(crate) fn append_field(encoded: &mut Vec<u8>, value: &[u8]) -> Result<(), ProvisioningError> {
    let len = u32::try_from(value.len()).map_err(|_| ProvisioningError::CanonicalFieldTooLarge)?;
    encoded.extend_from_slice(&len.to_be_bytes());
    encoded.extend_from_slice(value);
    Ok(())
}

fn validate_version(version: u16) -> Result<(), ProvisioningError> {
    if version == PROVISIONING_VERSION_V2 {
        Ok(())
    } else {
        Err(ProvisioningError::UnsupportedVersion)
    }
}

fn is_usable_overlay(ip: Ipv4Addr) -> bool {
    let raw = u32::from(ip);
    raw > u32::from(OVERLAY_NETWORK_V2) && raw < u32::from(OVERLAY_BROADCAST_V2)
}
