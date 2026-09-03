//! End-to-end AEAD for the three Lantunnel 2.0 Relay codec seams.
//!
//! This module authenticates one record and its routing context. Random nonces
//! deliberately do not add replay detection, sequencing, rekeying, or freshness.

use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use chacha20poly1305::aead::{AeadCore, AeadInPlace, OsRng};
use chacha20poly1305::{KeyInit, Tag, XChaCha20Poly1305, XNonce};
use thiserror::Error;
use tp_core::p2p_types::SessionId;
use tp_core::peer_link_crypto::PeerLinkSessionKeysV2;

const CONTROL_DOMAIN_V2: &[u8] = b"lantunnel.relay.control.v2";
const FRAMED_DOMAIN_V2: &[u8] = b"lantunnel.relay.framed.v2";
const TCP_OPEN_DOMAIN_V2: &[u8] = b"lantunnel.relay.tcp-open.v2";
const TCP_OPEN_RESPONSE_DOMAIN_V2: &[u8] = b"lantunnel.relay.tcp-open-response.v2";
const TCP_DATA_DOMAIN_V2: &[u8] = b"lantunnel.relay.tcp-data.v2";
pub const RELAY_NONCE_SIZE_V2: usize = 24;
const NONCE_SIZE: usize = RELAY_NONCE_SIZE_V2;
const TAG_SIZE: usize = 16;
pub const MAX_RELAY_PLAINTEXT_V2: usize = 64 * 1024;
pub const RELAY_SEALED_OVERHEAD_V2: usize = NONCE_SIZE + TAG_SIZE;
const MAX_RELAY_CONTROL_STRING_V2: usize = 4 * 1024;

const CONTROL_OPEN_TAG_V2: u8 = 0x01;
const CONTROL_OPEN_RESPONSE_TAG_V2: u8 = 0x02;
const CONTROL_RUNTIME_RECORD_TAG_V2: u8 = 0x03;
const CONTROL_DIGEST_TAG_V2: u8 = 0x04;
const CONTROL_NEED_TAG_V2: u8 = 0x05;

/// Plaintext variants allowed inside `EncryptedPeerControlV2`.
///
/// Tunnel, source, target, PeerLink session and flow identity stay in the
/// authenticated outer context and are intentionally not repeated here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayControlPayloadV2 {
    Open { network: String, address: String },
    OpenResponse { success: bool, error: String },
    RuntimeRecord(Vec<u8>),
    Digest([u8; 32]),
    Need,
}

impl RelayControlPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, RelayCryptoErrorV2> {
        let mut encoded = Vec::new();
        match self {
            Self::Open { network, address } => {
                if !matches!(network.as_str(), "tcp" | "udp")
                    || address.is_empty()
                    || network.len() > MAX_RELAY_CONTROL_STRING_V2
                    || address.len() > MAX_RELAY_CONTROL_STRING_V2
                {
                    return Err(RelayCryptoErrorV2::InvalidControlPayload);
                }
                encoded.push(CONTROL_OPEN_TAG_V2);
                append_lp(&mut encoded, network.as_bytes())?;
                append_lp(&mut encoded, address.as_bytes())?;
            }
            Self::OpenResponse { success, error } => {
                if error.len() > MAX_RELAY_CONTROL_STRING_V2 {
                    return Err(RelayCryptoErrorV2::InvalidControlPayload);
                }
                encoded.push(CONTROL_OPEN_RESPONSE_TAG_V2);
                encoded.push(u8::from(*success));
                append_lp(&mut encoded, error.as_bytes())?;
            }
            Self::RuntimeRecord(record) => {
                if record.is_empty() || record.len() > MAX_RELAY_PLAINTEXT_V2 - 5 {
                    return Err(RelayCryptoErrorV2::InvalidControlPayload);
                }
                encoded.push(CONTROL_RUNTIME_RECORD_TAG_V2);
                append_lp(&mut encoded, record)?;
            }
            Self::Digest(hash) => {
                encoded.push(CONTROL_DIGEST_TAG_V2);
                encoded.extend_from_slice(hash);
            }
            Self::Need => encoded.push(CONTROL_NEED_TAG_V2),
        }
        if encoded.len() > MAX_RELAY_PLAINTEXT_V2 {
            return Err(RelayCryptoErrorV2::PlaintextTooLarge);
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, RelayCryptoErrorV2> {
        if encoded.is_empty() || encoded.len() > MAX_RELAY_PLAINTEXT_V2 {
            return Err(RelayCryptoErrorV2::InvalidControlPayload);
        }
        let tag = encoded[0];
        let mut cursor = 1;
        let payload = match tag {
            CONTROL_OPEN_TAG_V2 => {
                let network = read_control_string(encoded, &mut cursor)?;
                let address = read_control_string(encoded, &mut cursor)?;
                if !matches!(network.as_str(), "tcp" | "udp") || address.is_empty() {
                    return Err(RelayCryptoErrorV2::InvalidControlPayload);
                }
                Self::Open { network, address }
            }
            CONTROL_OPEN_RESPONSE_TAG_V2 => {
                let success = match encoded.get(cursor).copied() {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err(RelayCryptoErrorV2::InvalidControlPayload),
                };
                cursor += 1;
                let error = read_control_string(encoded, &mut cursor)?;
                Self::OpenResponse { success, error }
            }
            CONTROL_RUNTIME_RECORD_TAG_V2 => {
                let record = read_control_field(encoded, &mut cursor)?.to_vec();
                if record.is_empty() {
                    return Err(RelayCryptoErrorV2::InvalidControlPayload);
                }
                Self::RuntimeRecord(record)
            }
            CONTROL_DIGEST_TAG_V2 => {
                let hash: [u8; 32] = encoded
                    .get(cursor..cursor + 32)
                    .and_then(|value| value.try_into().ok())
                    .ok_or(RelayCryptoErrorV2::InvalidControlPayload)?;
                cursor += 32;
                Self::Digest(hash)
            }
            CONTROL_NEED_TAG_V2 => Self::Need,
            _ => return Err(RelayCryptoErrorV2::InvalidControlPayload),
        };
        if cursor != encoded.len() {
            return Err(RelayCryptoErrorV2::InvalidControlPayload);
        }
        Ok(payload)
    }
}

/// Public routing values bound into AEAD. The source and target are ordered
/// for the record's wire direction, not for the local endpoint's role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayRecordContextV2<'a> {
    pub tunnel_id: &'a str,
    pub peerlink_session_id: &'a SessionId,
    pub source_peer_id: &'a str,
    pub target_peer_id: &'a str,
    pub conn_id: &'a [u8; 12],
}

/// The only existing framed payload kinds that V2 Relay seals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayFramedKindV2 {
    Data = 0x10,
    UdpData = 0x11,
}

/// The three separately bound records on the existing QUIC TCP flow stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayFlowKindV2 {
    Open,
    OpenResponse,
    Data,
}

/// Canonical Relay AAD prepared once for a fixed record context.
///
/// It is deliberately opaque: callers can cache it per Flow but cannot
/// assemble a different authenticated wire context accidentally.
#[derive(Clone)]
pub struct RelayAadV2(Arc<[u8]>);

impl RelayAadV2 {
    pub fn control(
        context: RelayRecordContextV2<'_>,
        route_abort: bool,
    ) -> Result<Self, RelayCryptoErrorV2> {
        control_aad(context, route_abort).map(|aad| Self(aad.into()))
    }

    pub fn framed(
        context: RelayRecordContextV2<'_>,
        kind: RelayFramedKindV2,
    ) -> Result<Self, RelayCryptoErrorV2> {
        framed_aad(context, kind).map(|aad| Self(aad.into()))
    }

    pub fn flow(
        context: RelayRecordContextV2<'_>,
        kind: RelayFlowKindV2,
    ) -> Result<Self, RelayCryptoErrorV2> {
        flow_aad(context, kind).map(|aad| Self(aad.into()))
    }
}

impl RelayFlowKindV2 {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::Open => TCP_OPEN_DOMAIN_V2,
            Self::OpenResponse => TCP_OPEN_RESPONSE_DOMAIN_V2,
            Self::Data => TCP_DATA_DOMAIN_V2,
        }
    }
}

/// Locally oriented Relay cipher: `seal_*` uses the PeerLink send key and
/// `open_*` uses its receive key. It intentionally exposes neither keys nor
/// `Debug`/serialization implementations.
pub struct RelayCipherV2 {
    send: XChaCha20Poly1305,
    receive: XChaCha20Poly1305,
}

impl RelayCipherV2 {
    pub fn new(keys: &PeerLinkSessionKeysV2) -> Self {
        Self {
            send: XChaCha20Poly1305::new(keys.send_key().into()),
            receive: XChaCha20Poly1305::new(keys.receive_key().into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_directional_keys_for_test(send: [u8; 32], receive: [u8; 32]) -> Self {
        Self {
            send: XChaCha20Poly1305::new((&send).into()),
            receive: XChaCha20Poly1305::new((&receive).into()),
        }
    }

    pub fn seal_control(
        &self,
        context: RelayRecordContextV2<'_>,
        route_abort: bool,
        buffer: &mut Vec<u8>,
    ) -> Result<(), RelayCryptoErrorV2> {
        let aad = control_aad(context, route_abort)?;
        seal(&self.send, &aad, buffer)
    }

    pub fn open_control(
        &self,
        context: RelayRecordContextV2<'_>,
        route_abort: bool,
        buffer: &mut Vec<u8>,
    ) -> Result<(), RelayCryptoErrorV2> {
        let aad = control_aad(context, route_abort)?;
        open(&self.receive, &aad, buffer)
    }

    pub fn seal_framed(
        &self,
        context: RelayRecordContextV2<'_>,
        kind: RelayFramedKindV2,
        buffer: &mut Vec<u8>,
    ) -> Result<(), RelayCryptoErrorV2> {
        let aad = framed_aad(context, kind)?;
        seal(&self.send, &aad, buffer)
    }

    pub fn open_framed(
        &self,
        context: RelayRecordContextV2<'_>,
        kind: RelayFramedKindV2,
        buffer: &mut Vec<u8>,
    ) -> Result<(), RelayCryptoErrorV2> {
        let aad = framed_aad(context, kind)?;
        open(&self.receive, &aad, buffer)
    }

    pub fn seal_flow(
        &self,
        context: RelayRecordContextV2<'_>,
        kind: RelayFlowKindV2,
        buffer: &mut Vec<u8>,
    ) -> Result<(), RelayCryptoErrorV2> {
        let aad = flow_aad(context, kind)?;
        seal(&self.send, &aad, buffer)
    }

    pub fn open_flow(
        &self,
        context: RelayRecordContextV2<'_>,
        kind: RelayFlowKindV2,
        buffer: &mut Vec<u8>,
    ) -> Result<(), RelayCryptoErrorV2> {
        let aad = flow_aad(context, kind)?;
        open(&self.receive, &aad, buffer)
    }

    /// Seals `[reserved nonce][plaintext]` in caller-owned storage.
    pub fn seal_precomputed(
        &self,
        aad: &RelayAadV2,
        buffer: &mut BytesMut,
    ) -> Result<(), RelayCryptoErrorV2> {
        seal_prepared(&self.send, &aad.0, buffer)
    }

    /// Authenticates in place, then exposes plaintext by advancing the view.
    pub fn open_precomputed(
        &self,
        aad: &RelayAadV2,
        buffer: &mut BytesMut,
    ) -> Result<(), RelayCryptoErrorV2> {
        open_in_place(&self.receive, &aad.0, buffer)
    }

    /// Consumes an inbound wire payload so the common unique-`Bytes` case is
    /// decrypted in its transport allocation. Shared buffers retain the same
    /// behavior through a single defensive copy.
    pub fn open_bytes_precomputed(
        &self,
        aad: &RelayAadV2,
        buffer: Bytes,
    ) -> Result<Bytes, RelayCryptoErrorV2> {
        let mut buffer = match buffer.try_into_mut() {
            Ok(unique) => unique,
            Err(shared) => BytesMut::from(shared.as_ref()),
        };
        self.open_precomputed(aad, &mut buffer)?;
        Ok(buffer.freeze())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RelayCryptoErrorV2 {
    #[error("invalid Relay record context")]
    InvalidContext,
    #[error("Relay plaintext exceeds {MAX_RELAY_PLAINTEXT_V2} bytes")]
    PlaintextTooLarge,
    #[error("invalid Relay sealed blob length")]
    InvalidSealedLength,
    #[error("Relay record authentication failed")]
    AuthenticationFailed,
    #[error("Relay canonical field exceeds u32 length")]
    CanonicalFieldTooLarge,
    #[error("invalid encrypted Relay control payload")]
    InvalidControlPayload,
}

fn read_control_field<'a>(
    encoded: &'a [u8],
    cursor: &mut usize,
) -> Result<&'a [u8], RelayCryptoErrorV2> {
    let len_bytes: [u8; 4] = encoded
        .get(*cursor..cursor.saturating_add(4))
        .and_then(|value| value.try_into().ok())
        .ok_or(RelayCryptoErrorV2::InvalidControlPayload)?;
    *cursor += 4;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_RELAY_PLAINTEXT_V2 {
        return Err(RelayCryptoErrorV2::InvalidControlPayload);
    }
    let end = cursor
        .checked_add(len)
        .ok_or(RelayCryptoErrorV2::InvalidControlPayload)?;
    let field = encoded
        .get(*cursor..end)
        .ok_or(RelayCryptoErrorV2::InvalidControlPayload)?;
    *cursor = end;
    Ok(field)
}

fn read_control_string(encoded: &[u8], cursor: &mut usize) -> Result<String, RelayCryptoErrorV2> {
    let field = read_control_field(encoded, cursor)?;
    if field.len() > MAX_RELAY_CONTROL_STRING_V2 {
        return Err(RelayCryptoErrorV2::InvalidControlPayload);
    }
    std::str::from_utf8(field)
        .map(str::to_owned)
        .map_err(|_| RelayCryptoErrorV2::InvalidControlPayload)
}

fn control_aad(
    context: RelayRecordContextV2<'_>,
    route_abort: bool,
) -> Result<Vec<u8>, RelayCryptoErrorV2> {
    let mut aad = canonical_aad_prefix(CONTROL_DOMAIN_V2, context)?;
    append_lp(&mut aad, context.conn_id)?;
    append_lp(&mut aad, &[u8::from(route_abort)])?;
    Ok(aad)
}

fn framed_aad(
    context: RelayRecordContextV2<'_>,
    kind: RelayFramedKindV2,
) -> Result<Vec<u8>, RelayCryptoErrorV2> {
    let mut aad = canonical_aad_prefix(FRAMED_DOMAIN_V2, context)?;
    append_lp(&mut aad, &[kind as u8])?;
    append_lp(&mut aad, context.conn_id)?;
    Ok(aad)
}

fn flow_aad(
    context: RelayRecordContextV2<'_>,
    kind: RelayFlowKindV2,
) -> Result<Vec<u8>, RelayCryptoErrorV2> {
    let mut aad = canonical_aad_prefix(kind.domain(), context)?;
    append_lp(&mut aad, context.conn_id)?;
    Ok(aad)
}

fn canonical_aad_prefix(
    domain: &[u8],
    context: RelayRecordContextV2<'_>,
) -> Result<Vec<u8>, RelayCryptoErrorV2> {
    if context.tunnel_id.is_empty()
        || context.source_peer_id.is_empty()
        || context.target_peer_id.is_empty()
        || context.source_peer_id == context.target_peer_id
    {
        return Err(RelayCryptoErrorV2::InvalidContext);
    }

    let mut aad = Vec::new();
    append_lp(&mut aad, domain)?;
    append_lp(&mut aad, context.tunnel_id.as_bytes())?;
    append_lp(&mut aad, context.peerlink_session_id.as_bytes())?;
    append_lp(&mut aad, context.source_peer_id.as_bytes())?;
    append_lp(&mut aad, context.target_peer_id.as_bytes())?;
    Ok(aad)
}

fn append_lp(output: &mut Vec<u8>, field: &[u8]) -> Result<(), RelayCryptoErrorV2> {
    let len = u32::try_from(field.len()).map_err(|_| RelayCryptoErrorV2::CanonicalFieldTooLarge)?;
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(field);
    Ok(())
}

fn seal(
    cipher: &XChaCha20Poly1305,
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), RelayCryptoErrorV2> {
    if buffer.len() > MAX_RELAY_PLAINTEXT_V2 {
        return Err(RelayCryptoErrorV2::PlaintextTooLarge);
    }
    let generated = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let mut nonce = [0_u8; NONCE_SIZE];
    nonce.copy_from_slice(&generated);
    seal_with_nonce(cipher, aad, buffer, nonce)
}

fn seal_with_nonce(
    cipher: &XChaCha20Poly1305,
    aad: &[u8],
    buffer: &mut Vec<u8>,
    nonce: [u8; NONCE_SIZE],
) -> Result<(), RelayCryptoErrorV2> {
    let plaintext_len = buffer.len();
    buffer.reserve(RELAY_SEALED_OVERHEAD_V2);
    buffer.resize(plaintext_len + RELAY_SEALED_OVERHEAD_V2, 0);
    buffer.copy_within(0..plaintext_len, NONCE_SIZE);
    let tag = cipher
        .encrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            aad,
            &mut buffer[NONCE_SIZE..NONCE_SIZE + plaintext_len],
        )
        .map_err(|_| RelayCryptoErrorV2::AuthenticationFailed)?;
    buffer[..NONCE_SIZE].copy_from_slice(&nonce);
    buffer[NONCE_SIZE + plaintext_len..].copy_from_slice(&tag);
    Ok(())
}

fn open(
    cipher: &XChaCha20Poly1305,
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), RelayCryptoErrorV2> {
    if !(RELAY_SEALED_OVERHEAD_V2..=MAX_RELAY_PLAINTEXT_V2 + RELAY_SEALED_OVERHEAD_V2)
        .contains(&buffer.len())
    {
        return Err(RelayCryptoErrorV2::InvalidSealedLength);
    }
    let plaintext_len = buffer.len() - RELAY_SEALED_OVERHEAD_V2;
    let tag_start = NONCE_SIZE + plaintext_len;
    let mut nonce = [0_u8; NONCE_SIZE];
    nonce.copy_from_slice(&buffer[..NONCE_SIZE]);
    let mut tag = [0_u8; TAG_SIZE];
    tag.copy_from_slice(&buffer[tag_start..]);
    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            aad,
            &mut buffer[NONCE_SIZE..tag_start],
            Tag::from_slice(&tag),
        )
        .map_err(|_| RelayCryptoErrorV2::AuthenticationFailed)?;
    buffer.copy_within(NONCE_SIZE..tag_start, 0);
    buffer.truncate(plaintext_len);
    Ok(())
}

fn seal_prepared(
    cipher: &XChaCha20Poly1305,
    aad: &[u8],
    buffer: &mut BytesMut,
) -> Result<(), RelayCryptoErrorV2> {
    let generated = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let mut nonce = [0_u8; NONCE_SIZE];
    nonce.copy_from_slice(&generated);
    seal_prepared_with_nonce(cipher, aad, buffer, nonce)
}

fn seal_prepared_with_nonce(
    cipher: &XChaCha20Poly1305,
    aad: &[u8],
    buffer: &mut BytesMut,
    nonce: [u8; NONCE_SIZE],
) -> Result<(), RelayCryptoErrorV2> {
    let plaintext_len = buffer
        .len()
        .checked_sub(NONCE_SIZE)
        .ok_or(RelayCryptoErrorV2::InvalidSealedLength)?;
    if plaintext_len > MAX_RELAY_PLAINTEXT_V2 {
        return Err(RelayCryptoErrorV2::PlaintextTooLarge);
    }
    buffer.reserve(TAG_SIZE);
    let tag_start = NONCE_SIZE + plaintext_len;
    buffer.resize(tag_start + TAG_SIZE, 0);
    let tag = cipher
        .encrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            aad,
            &mut buffer[NONCE_SIZE..tag_start],
        )
        .map_err(|_| RelayCryptoErrorV2::AuthenticationFailed)?;
    buffer[..NONCE_SIZE].copy_from_slice(&nonce);
    buffer[tag_start..].copy_from_slice(&tag);
    Ok(())
}

fn open_in_place(
    cipher: &XChaCha20Poly1305,
    aad: &[u8],
    buffer: &mut BytesMut,
) -> Result<(), RelayCryptoErrorV2> {
    if !(RELAY_SEALED_OVERHEAD_V2..=MAX_RELAY_PLAINTEXT_V2 + RELAY_SEALED_OVERHEAD_V2)
        .contains(&buffer.len())
    {
        return Err(RelayCryptoErrorV2::InvalidSealedLength);
    }
    let plaintext_len = buffer.len() - RELAY_SEALED_OVERHEAD_V2;
    let tag_start = NONCE_SIZE + plaintext_len;
    let mut nonce = [0_u8; NONCE_SIZE];
    nonce.copy_from_slice(&buffer[..NONCE_SIZE]);
    let mut tag = [0_u8; TAG_SIZE];
    tag.copy_from_slice(&buffer[tag_start..]);
    cipher
        .decrypt_in_place_detached(
            XNonce::from_slice(&nonce),
            aad,
            &mut buffer[NONCE_SIZE..tag_start],
            Tag::from_slice(&tag),
        )
        .map_err(|_| RelayCryptoErrorV2::AuthenticationFailed)?;
    buffer.truncate(tag_start);
    buffer.advance(NONCE_SIZE);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn control_canonical_aad_and_sealed_blob_golden() {
        let session_id = SessionId::from_bytes([0x22; 16]);
        let context = RelayRecordContextV2 {
            tunnel_id: "tunnel-golden",
            peerlink_session_id: &session_id,
            source_peer_id: "peer-source",
            target_peer_id: "peer-target",
            conn_id: b"golden-flow1",
        };
        let aad = control_aad(context, true).expect("canonical AAD");
        assert_eq!(
            hex(&aad),
            "0000001a6c616e74756e6e656c2e72656c61792e636f6e74726f6c2e7632\
             0000000d74756e6e656c2d676f6c64656e\
             0000001022222222222222222222222222222222\
             0000000b706565722d736f75726365\
             0000000b706565722d746172676574\
             0000000c676f6c64656e2d666c6f7731\
             0000000101"
        );

        let cipher = XChaCha20Poly1305::new((&[0x11; 32]).into());
        let mut buffer = b"\x01tcp\0example.internal:27015".to_vec();
        seal_with_nonce(&cipher, &aad, &mut buffer, [0x33; 24]).expect("seal fixture");
        assert_eq!(
            hex(&buffer),
            "333333333333333333333333333333333333333333333333\
             bbd014f9780e7b56efe444c1e08490a2ee35129f208946bbdb55a34cf353\
             cd772569a75efd54c541ca1283"
        );
    }

    #[test]
    fn prepared_record_is_wire_identical_to_the_existing_codec() {
        let cipher = XChaCha20Poly1305::new((&[0x44; 32]).into());
        let aad = b"wire-compatible-aad";
        let plaintext = b"wire-compatible-payload";
        let nonce = [0x55; NONCE_SIZE];
        let mut existing = plaintext.to_vec();
        seal_with_nonce(&cipher, aad, &mut existing, nonce).expect("seal existing record");

        let mut prepared = BytesMut::with_capacity(plaintext.len() + RELAY_SEALED_OVERHEAD_V2);
        prepared.resize(NONCE_SIZE, 0);
        prepared.extend_from_slice(plaintext);
        seal_prepared_with_nonce(&cipher, aad, &mut prepared, nonce).expect("seal prepared record");

        assert_eq!(prepared.as_ref(), existing.as_slice());
    }

    #[test]
    #[ignore = "run with --profile release-perf --ignored --nocapture for evidence"]
    fn relay_record_hotpath_benchmark() {
        const RECORD_BYTES: usize = 4 * 1024;
        const RECORDS: usize = 100_000;
        const BATCH_RECORDS: usize = 512;
        let cipher = XChaCha20Poly1305::new((&[0x66; 32]).into());
        let aad = b"relay-hotpath-benchmark-aad";
        let plaintext = vec![0x77; RECORD_BYTES];
        let nonce = [0x88; NONCE_SIZE];

        let mut legacy_elapsed = Duration::ZERO;
        let mut prepared_elapsed = Duration::ZERO;
        let mut completed = 0;
        let mut batch_index = 0;
        while completed < RECORDS {
            let count = (RECORDS - completed).min(BATCH_RECORDS);
            // Socket/transport input production is common to both paths, so
            // populate it outside the timers. The old transform allocated and
            // copied `Bytes` once per record, then shifted it for the nonce.
            // The prepared transform encrypts producer-owned storage in place.
            let legacy_inputs = (0..count)
                .map(|_| Bytes::copy_from_slice(&plaintext))
                .collect::<Vec<_>>();
            let mut prepared_inputs = Vec::with_capacity(count);
            for _ in 0..count {
                let mut prepared = BytesMut::with_capacity(RECORD_BYTES + RELAY_SEALED_OVERHEAD_V2);
                prepared.resize(NONCE_SIZE, 0);
                prepared.extend_from_slice(&plaintext);
                prepared_inputs.push(prepared);
            }

            let mut legacy_outputs = Vec::with_capacity(count);
            let mut run_legacy = || {
                let started = Instant::now();
                for payload in &legacy_inputs {
                    let mut legacy = payload.to_vec();
                    seal_with_nonce(&cipher, aad, &mut legacy, nonce).expect("legacy seal");
                    black_box(&legacy);
                    legacy_outputs.push(legacy);
                }
                started.elapsed()
            };
            let mut prepared_outputs = Vec::with_capacity(count);
            let mut run_prepared = || {
                let started = Instant::now();
                for mut prepared in prepared_inputs.drain(..) {
                    seal_prepared_with_nonce(&cipher, aad, &mut prepared, nonce)
                        .expect("prepared seal");
                    black_box(&prepared);
                    prepared_outputs.push(prepared);
                }
                started.elapsed()
            };

            // Alternate order so neither implementation consistently receives
            // the warmer CPU/cache state.
            if batch_index % 2 == 0 {
                legacy_elapsed += run_legacy();
                prepared_elapsed += run_prepared();
            } else {
                prepared_elapsed += run_prepared();
                legacy_elapsed += run_legacy();
            }
            completed += count;
            batch_index += 1;
        }

        let mib = (RECORD_BYTES * RECORDS) as f64 / (1024.0 * 1024.0);
        eprintln!(
            "relay seal 4KiB: legacy={:.0} MiB/s prepared={:.0} MiB/s speedup={:.2}x",
            mib / legacy_elapsed.as_secs_f64(),
            mib / prepared_elapsed.as_secs_f64(),
            legacy_elapsed.as_secs_f64() / prepared_elapsed.as_secs_f64(),
        );
        assert!(
            prepared_elapsed <= legacy_elapsed,
            "producer-owned sealing regressed: legacy={legacy_elapsed:?} prepared={prepared_elapsed:?}"
        );

        let mut legacy_open_elapsed = Duration::ZERO;
        let mut prepared_open_elapsed = Duration::ZERO;
        let mut completed = 0;
        let mut batch_index = 0;
        while completed < RECORDS {
            let count = (RECORDS - completed).min(BATCH_RECORDS);
            let mut legacy_inputs = Vec::with_capacity(count);
            let mut prepared_inputs = Vec::with_capacity(count);
            for _ in 0..count {
                let mut legacy = plaintext.to_vec();
                seal_with_nonce(&cipher, aad, &mut legacy, nonce).expect("prepare legacy open");
                legacy_inputs.push(legacy);

                let mut prepared = BytesMut::with_capacity(RECORD_BYTES + RELAY_SEALED_OVERHEAD_V2);
                prepared.resize(NONCE_SIZE, 0);
                prepared.extend_from_slice(&plaintext);
                seal_prepared_with_nonce(&cipher, aad, &mut prepared, nonce)
                    .expect("prepare in-place open");
                prepared_inputs.push(prepared);
            }

            let mut run_legacy = || {
                let started = Instant::now();
                for record in &mut legacy_inputs {
                    open(&cipher, aad, record).expect("legacy open");
                    black_box(record);
                }
                started.elapsed()
            };
            let mut run_prepared = || {
                let started = Instant::now();
                for record in &mut prepared_inputs {
                    open_in_place(&cipher, aad, record).expect("in-place open");
                    black_box(record);
                }
                started.elapsed()
            };

            if batch_index % 2 == 0 {
                legacy_open_elapsed += run_legacy();
                prepared_open_elapsed += run_prepared();
            } else {
                prepared_open_elapsed += run_prepared();
                legacy_open_elapsed += run_legacy();
            }
            completed += count;
            batch_index += 1;
        }

        eprintln!(
            "relay open 4KiB: legacy={:.0} MiB/s prepared={:.0} MiB/s speedup={:.2}x",
            mib / legacy_open_elapsed.as_secs_f64(),
            mib / prepared_open_elapsed.as_secs_f64(),
            legacy_open_elapsed.as_secs_f64() / prepared_open_elapsed.as_secs_f64(),
        );
        assert!(
            prepared_open_elapsed <= legacy_open_elapsed,
            "in-place open regressed: legacy={legacy_open_elapsed:?} prepared={prepared_open_elapsed:?}"
        );
    }
}
