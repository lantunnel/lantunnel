//! Outbound sender wrapper that routes each `send` through
//! [`MultiSession::pick`]. Replaces a single `SessionSender` in the data
//! plane (`pipe_tcp` / `pipe_udp`) so frames migrate to relay when P2P
//! goes away (best-effort migration).
//!
//! `closed()` only fires when **relay** is dropped — losing P2P alone
//! must not tear down per-conn pipes. The per-frame `pick()` already
//! routes around a missing P2P session.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tp_core::p2p_types::SessionId;
use tp_core::protocol::BinaryMessage;
use tp_transport::TrySendKind;

use crate::p2p::scheduler::PathKind;
use crate::p2p::session::MultiSession;
use crate::relay_crypto::{
    RelayAadV2, RelayControlPayloadV2, RelayFramedKindV2, RelayRecordContextV2,
};

/// Endpoint-only sealing facts for one exact V2 Relay Flow.
///
/// The router applies this adapter only after its existing scheduler has
/// selected Relay. Direct keeps the original message and wire path.
#[derive(Clone)]
pub(crate) struct V2RelaySealContext {
    pub(crate) tunnel_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) local_peer_id: String,
    pub(crate) remote_peer_id: String,
    pub(crate) cipher: Arc<crate::relay_crypto::RelayCipherV2>,
    conn_id: [u8; 12],
    control_aad: RelayAadV2,
    data_aad: RelayAadV2,
    udp_aad: RelayAadV2,
}

impl V2RelaySealContext {
    pub(crate) fn new(
        tunnel_id: String,
        session_id: SessionId,
        local_peer_id: String,
        remote_peer_id: String,
        conn_id: [u8; 12],
        cipher: Arc<crate::relay_crypto::RelayCipherV2>,
    ) -> Result<Self, crate::relay_crypto::RelayCryptoErrorV2> {
        let context = RelayRecordContextV2 {
            tunnel_id: &tunnel_id,
            peerlink_session_id: &session_id,
            source_peer_id: &local_peer_id,
            target_peer_id: &remote_peer_id,
            conn_id: &conn_id,
        };
        let control_aad = RelayAadV2::control(context, false)?;
        let data_aad = RelayAadV2::framed(context, RelayFramedKindV2::Data)?;
        let udp_aad = RelayAadV2::framed(context, RelayFramedKindV2::UdpData)?;
        Ok(Self {
            tunnel_id,
            session_id,
            local_peer_id,
            remote_peer_id,
            conn_id,
            cipher,
            control_aad,
            data_aad,
            udp_aad,
        })
    }

    fn seal_for_relay(&self, msg: &BinaryMessage) -> tp_transport::Result<BinaryMessage> {
        let conn_id = message_conn_id_wire(msg)?;
        if conn_id != self.conn_id {
            return Err(tp_transport::TransportError::Other(
                "V2 Relay seal context used for a different Flow".into(),
            ));
        }
        match msg {
            BinaryMessage::Connect {
                network, address, ..
            } => {
                let encoded = RelayControlPayloadV2::Open {
                    network: network.clone(),
                    address: address.clone(),
                }
                .encode()
                .map_err(relay_crypto_transport_error)?;
                let mut sealed = BytesMut::with_capacity(
                    encoded.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
                );
                sealed.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                sealed.extend_from_slice(&encoded);
                self.cipher
                    .seal_precomputed(&self.control_aad, &mut sealed)
                    .map_err(relay_crypto_transport_error)?;
                Ok(BinaryMessage::EncryptedPeerControlV2 {
                    target_peer_id: self.remote_peer_id.clone(),
                    peerlink_session_id: *self.session_id.as_bytes(),
                    conn_id,
                    route_abort: false,
                    sealed: sealed.freeze(),
                })
            }
            BinaryMessage::ConnectResponse { success, error, .. } => {
                let encoded = RelayControlPayloadV2::OpenResponse {
                    success: *success,
                    error: error.clone(),
                }
                .encode()
                .map_err(relay_crypto_transport_error)?;
                let mut sealed = BytesMut::with_capacity(
                    encoded.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
                );
                sealed.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                sealed.extend_from_slice(&encoded);
                self.cipher
                    .seal_precomputed(&self.control_aad, &mut sealed)
                    .map_err(relay_crypto_transport_error)?;
                Ok(BinaryMessage::EncryptedPeerControlV2 {
                    target_peer_id: self.remote_peer_id.clone(),
                    peerlink_session_id: *self.session_id.as_bytes(),
                    conn_id,
                    route_abort: false,
                    sealed: sealed.freeze(),
                })
            }
            BinaryMessage::Data { conn_id, payload } => {
                let mut sealed = BytesMut::with_capacity(
                    payload.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
                );
                sealed.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                sealed.extend_from_slice(payload);
                self.cipher
                    .seal_precomputed(&self.data_aad, &mut sealed)
                    .map_err(relay_crypto_transport_error)?;
                Ok(BinaryMessage::Data {
                    conn_id: conn_id.clone(),
                    payload: sealed.freeze(),
                })
            }
            BinaryMessage::UdpData { conn_id, payload } => {
                let mut sealed = BytesMut::with_capacity(
                    payload.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
                );
                sealed.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
                sealed.extend_from_slice(payload);
                self.cipher
                    .seal_precomputed(&self.udp_aad, &mut sealed)
                    .map_err(relay_crypto_transport_error)?;
                Ok(BinaryMessage::UdpData {
                    conn_id: conn_id.clone(),
                    payload: sealed.freeze(),
                })
            }
            _ => Ok(msg.clone()),
        }
    }

    fn seal_prepared(
        &self,
        conn_id: &[u8; 12],
        kind: RelayFramedKindV2,
        record: &mut BytesMut,
    ) -> tp_transport::Result<()> {
        if conn_id != &self.conn_id {
            return Err(tp_transport::TransportError::Other(
                "V2 Relay seal context used for a different Flow".into(),
            ));
        }
        let aad = match kind {
            RelayFramedKindV2::Data => &self.data_aad,
            RelayFramedKindV2::UdpData => &self.udp_aad,
        };
        self.cipher
            .seal_precomputed(aad, record)
            .map_err(relay_crypto_transport_error)
    }
}

fn message_conn_id_wire(msg: &BinaryMessage) -> tp_transport::Result<[u8; 12]> {
    let conn_id = match msg {
        BinaryMessage::Connect { conn_id, .. }
        | BinaryMessage::ConnectResponse { conn_id, .. }
        | BinaryMessage::Data { conn_id, .. }
        | BinaryMessage::UdpData { conn_id, .. }
        | BinaryMessage::Close { conn_id } => conn_id,
        _ => return Ok([0; 12]),
    };
    conn_id_wire(conn_id)
}

fn conn_id_wire(conn_id: &str) -> tp_transport::Result<[u8; 12]> {
    let bytes = conn_id.as_bytes();
    if bytes.is_empty() || bytes.len() > 12 || !bytes.is_ascii() || bytes.contains(&0) {
        return Err(tp_transport::TransportError::Other(
            "invalid V2 Relay connection id".into(),
        ));
    }
    let mut wire = [0_u8; 12];
    wire[..bytes.len()].copy_from_slice(bytes);
    Ok(wire)
}

fn relay_crypto_transport_error(error: impl std::fmt::Display) -> tp_transport::TransportError {
    tp_transport::TransportError::Other(format!("V2 Relay sealing failed: {error}"))
}

fn prepared_plaintext_len(record: &BytesMut) -> tp_transport::Result<i64> {
    let len = record
        .len()
        .checked_sub(crate::relay_crypto::RELAY_NONCE_SIZE_V2)
        .ok_or_else(|| {
            tp_transport::TransportError::Other(
                "prepared Relay payload has no reserved nonce prefix".into(),
            )
        })?;
    if len > crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2 {
        return Err(tp_transport::TransportError::FrameTooLarge(
            u32::try_from(len).unwrap_or(u32::MAX),
        ));
    }
    Ok(i64::try_from(len).unwrap_or(i64::MAX))
}

fn prepared_binary_message(
    conn_id: String,
    kind: RelayFramedKindV2,
    payload: Bytes,
) -> BinaryMessage {
    match kind {
        RelayFramedKindV2::Data => BinaryMessage::Data { conn_id, payload },
        RelayFramedKindV2::UdpData => BinaryMessage::UdpData { conn_id, payload },
    }
}

fn prepared_route_message(conn_id: &str, kind: RelayFramedKindV2) -> BinaryMessage {
    prepared_binary_message(conn_id.to_string(), kind, Bytes::new())
}

/// Byte count attributed to one frame for `p2p_bytes_total`. Only
/// data-plane variants (`Data` / `UdpData`) carry payload bytes worth
/// measuring; control frames are small and would skew the per-path
/// ratio if mixed in. Returns `0` for control frames so the metric
/// reflects actual bandwidth, not signaling chatter.
fn data_plane_byte_count(msg: &BinaryMessage) -> i64 {
    match msg {
        BinaryMessage::Data { payload, .. } | BinaryMessage::UdpData { payload, .. } => {
            payload_len(payload)
        }
        // Control frames + the `PackedMessage` wire-format container: not
        // counted. The client never constructs `PackedMessage` for outbound
        // sends today (it's a decoded inbound container), so this match
        // arm stays at 0. If a future task introduces outbound batching of
        // `Data` frames into a `PackedMessage`, sum the contained data-plane
        // bytes here so the per-path metric stays honest.
        _ => 0,
    }
}

#[inline]
fn payload_len(b: &Bytes) -> i64 {
    // `Bytes::len()` returns `usize`; saturate to `i64::MAX` for safety
    // although a single payload >8 EB is, generously, not a real
    // scenario. The atomic in `MetricsManager` is `i64`, so we cast
    // here once instead of at every call site.
    i64::try_from(b.len()).unwrap_or(i64::MAX)
}

/// Cloneable outbound router. One per replica session; cloned freely into
/// per-conn pipe tasks (`pipe_tcp`, `pipe_udp`) by Tasks 4.9 / 4.10.
#[derive(Clone)]
pub struct MultiSenderRouter {
    multi: Arc<MultiSession>,
    mode: RouteMode,
    last_logged_kind: Arc<AtomicU8>,
    local_client_id: Option<String>,
    selected_p2p_session_id: Option<SessionId>,
    v2_relay_seal: Option<V2RelaySealContext>,
}

#[derive(Clone)]
enum RouteMode {
    Scheduled,
    P2pPreferred,
    PinnedP2p(Arc<tp_transport::session::Session>),
    PinnedP2pNoRelayFallback(Arc<tp_transport::session::Session>),
    RelayOnly,
    RelayWithP2pFallback,
}

const LAST_PATH_UNSET: u8 = u8::MAX;
const LAST_PATH_RELAY: u8 = 0;
const LAST_PATH_P2P: u8 = 1;

impl MultiSenderRouter {
    pub fn new(multi: Arc<MultiSession>) -> Self {
        Self {
            multi,
            mode: RouteMode::Scheduled,
            last_logged_kind: Arc::new(AtomicU8::new(LAST_PATH_UNSET)),
            local_client_id: None,
            selected_p2p_session_id: None,
            v2_relay_seal: None,
        }
    }

    pub fn new_p2p_preferred(multi: Arc<MultiSession>) -> Self {
        Self {
            multi,
            mode: RouteMode::P2pPreferred,
            last_logged_kind: Arc::new(AtomicU8::new(LAST_PATH_UNSET)),
            local_client_id: None,
            selected_p2p_session_id: None,
            v2_relay_seal: None,
        }
    }

    pub(crate) fn new_pinned_p2p(
        multi: Arc<MultiSession>,
        p2p: Arc<tp_transport::session::Session>,
    ) -> Self {
        let selected_p2p_session_id = multi.p2p_session_id_for_handle(&p2p);
        Self {
            multi,
            mode: RouteMode::PinnedP2p(p2p),
            last_logged_kind: Arc::new(AtomicU8::new(LAST_PATH_UNSET)),
            local_client_id: None,
            selected_p2p_session_id,
            v2_relay_seal: None,
        }
    }

    pub(crate) fn new_pinned_p2p_no_relay_fallback(
        multi: Arc<MultiSession>,
        p2p: Arc<tp_transport::session::Session>,
    ) -> Self {
        let selected_p2p_session_id = multi.p2p_session_id_for_handle(&p2p);
        Self {
            multi,
            mode: RouteMode::PinnedP2pNoRelayFallback(p2p),
            last_logged_kind: Arc::new(AtomicU8::new(LAST_PATH_UNSET)),
            local_client_id: None,
            selected_p2p_session_id,
            v2_relay_seal: None,
        }
    }

    pub fn new_relay_only(multi: Arc<MultiSession>) -> Self {
        Self {
            multi,
            mode: RouteMode::RelayOnly,
            last_logged_kind: Arc::new(AtomicU8::new(LAST_PATH_UNSET)),
            local_client_id: None,
            selected_p2p_session_id: None,
            v2_relay_seal: None,
        }
    }

    pub fn new_relay_with_p2p_fallback(multi: Arc<MultiSession>) -> Self {
        Self {
            multi,
            mode: RouteMode::RelayWithP2pFallback,
            last_logged_kind: Arc::new(AtomicU8::new(LAST_PATH_UNSET)),
            local_client_id: None,
            selected_p2p_session_id: None,
            v2_relay_seal: None,
        }
    }

    pub(crate) fn with_local_client_id(mut self, local_client_id: impl Into<String>) -> Self {
        self.local_client_id = Some(local_client_id.into());
        self
    }

    pub(crate) fn with_v2_relay_seal(mut self, context: V2RelaySealContext) -> Self {
        self.v2_relay_seal = Some(context);
        self
    }

    fn message_for_path(
        &self,
        msg: &BinaryMessage,
        kind: PathKind,
    ) -> tp_transport::Result<BinaryMessage> {
        match (kind, self.v2_relay_seal.as_ref()) {
            (PathKind::Relay, Some(context)) => context.seal_for_relay(msg),
            _ => Ok(msg.clone()),
        }
    }

    /// Per-frame path selection. Picks `multi.pick_with_kind()` (relay or
    /// P2P) then forwards the existing `Session::send`. Equivalent to
    /// `SessionSender::send` from the caller's POV. On success,
    /// attributes the frame's payload bytes to the chosen path's
    /// `p2p_bytes_total{path=...}` counter.
    pub async fn send(&self, msg: BinaryMessage) -> tp_transport::Result<()> {
        let (_kind, result) = self.send_with_path(msg).await;
        result
    }

    /// Send producer-owned V2 data storage. `record` starts with reserved
    /// nonce bytes followed by the exact plaintext frame/datagram. Relay
    /// seals it in place; Direct advances the view to the plaintext.
    pub(crate) async fn send_prepared_data(
        &self,
        conn_id: String,
        kind: RelayFramedKindV2,
        record: BytesMut,
    ) -> tp_transport::Result<()> {
        let bytes = prepared_plaintext_len(&record)?;
        let (session, path) = self.pick_with_kind();
        let (outbound, fallback_plaintext) =
            self.prepared_message_for_initial_path(&conn_id, kind, record, path)?;
        let result = session.send(outbound).await;
        if result.is_ok() {
            self.record_bytes(path, bytes);
            self.record_prepared_route_decision(&conn_id, kind, bytes, path, Some(&session), None);
            return result;
        }
        if path == PathKind::P2p {
            if !matches!(result, Err(tp_transport::TransportError::Closed)) {
                return result;
            }
            if self.relay_fallback_disabled() {
                self.close_failed_p2p_session(
                    &session,
                    false,
                    &prepared_route_message(&conn_id, kind),
                );
                return result;
            }
            let route_message = prepared_route_message(&conn_id, kind);
            let fallback_p2p_session_id =
                self.close_failed_p2p_session(&session, true, &route_message);
            let relay = self.multi.relay().clone();
            let fallback = match fallback_plaintext {
                Some(plaintext) => match self.prepared_message_from_plaintext(
                    &conn_id,
                    kind,
                    plaintext,
                    PathKind::Relay,
                ) {
                    Ok(message) => relay.send(message).await,
                    Err(error) => Err(error),
                },
                None => Err(tp_transport::TransportError::Other(
                    "prepared Relay fallback lost plaintext".into(),
                )),
            };
            if fallback.is_ok() {
                self.record_bytes(PathKind::Relay, bytes);
                self.record_prepared_route_decision(
                    &conn_id,
                    kind,
                    bytes,
                    PathKind::Relay,
                    Some(&relay),
                    Some("closed"),
                );
                if let Some(session_id) = fallback_p2p_session_id {
                    tracing::trace!(?session_id, "prepared data migrated to Relay");
                }
            }
            return fallback;
        }
        if matches!(result, Err(tp_transport::TransportError::Closed))
            && self.p2p_fallback_enabled()
        {
            let Some(p2p) = self.multi.p2p_for_new_flow() else {
                return result;
            };
            let Some(plaintext) = fallback_plaintext else {
                return result;
            };
            let message = prepared_binary_message(conn_id.clone(), kind, plaintext);
            let fallback = p2p.send(message).await;
            if fallback.is_ok() {
                self.record_bytes(PathKind::P2p, bytes);
                self.record_prepared_route_decision(
                    &conn_id,
                    kind,
                    bytes,
                    PathKind::P2p,
                    Some(&p2p),
                    Some("relay_closed"),
                );
            }
            return fallback;
        }
        result
    }

    pub(crate) fn try_send_prepared_data(
        &self,
        conn_id: String,
        kind: RelayFramedKindV2,
        record: BytesMut,
    ) -> Result<(), TrySendKind> {
        let bytes = match prepared_plaintext_len(&record) {
            Ok(bytes) => bytes,
            Err(tp_transport::TransportError::FrameTooLarge(len)) => {
                return Err(TrySendKind::TooLarge(len));
            }
            Err(_) => return Err(TrySendKind::Closed),
        };
        let (session, path) = self.pick_with_kind();
        let (outbound, fallback_plaintext) = self
            .prepared_message_for_initial_path(&conn_id, kind, record, path)
            .map_err(|_| TrySendKind::Closed)?;
        match session.try_send(outbound) {
            Ok(()) => {
                self.record_bytes(path, bytes);
                self.record_prepared_route_decision(
                    &conn_id,
                    kind,
                    bytes,
                    path,
                    Some(&session),
                    None,
                );
                Ok(())
            }
            Err(TrySendKind::Full) if path == PathKind::P2p => Err(TrySendKind::Full),
            Err(TrySendKind::Closed) if path == PathKind::P2p => {
                if self.relay_fallback_disabled() {
                    self.close_failed_p2p_session(
                        &session,
                        false,
                        &prepared_route_message(&conn_id, kind),
                    );
                    return Err(TrySendKind::Closed);
                }
                let route_message = prepared_route_message(&conn_id, kind);
                self.close_failed_p2p_session(&session, true, &route_message);
                let relay = self.multi.relay().clone();
                let plaintext = fallback_plaintext.ok_or(TrySendKind::Closed)?;
                let outbound = self
                    .prepared_message_from_plaintext(&conn_id, kind, plaintext, PathKind::Relay)
                    .map_err(|_| TrySendKind::Closed)?;
                relay.try_send(outbound)?;
                self.record_bytes(PathKind::Relay, bytes);
                self.record_prepared_route_decision(
                    &conn_id,
                    kind,
                    bytes,
                    PathKind::Relay,
                    Some(&relay),
                    Some("closed"),
                );
                Ok(())
            }
            Err(TrySendKind::Closed) if path == PathKind::Relay && self.p2p_fallback_enabled() => {
                let p2p = self.multi.p2p_for_new_flow().ok_or(TrySendKind::Closed)?;
                let plaintext = fallback_plaintext.ok_or(TrySendKind::Closed)?;
                p2p.try_send(prepared_binary_message(conn_id.clone(), kind, plaintext))?;
                self.record_bytes(PathKind::P2p, bytes);
                self.record_prepared_route_decision(
                    &conn_id,
                    kind,
                    bytes,
                    PathKind::P2p,
                    Some(&p2p),
                    Some("relay_closed"),
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn prepared_message_for_initial_path(
        &self,
        conn_id: &str,
        kind: RelayFramedKindV2,
        mut record: BytesMut,
        path: PathKind,
    ) -> tp_transport::Result<(BinaryMessage, Option<Bytes>)> {
        let fallback_needed = match path {
            PathKind::P2p => !self.relay_fallback_disabled(),
            PathKind::Relay => self.p2p_fallback_enabled(),
        };
        if path == PathKind::Relay {
            let conn_id_wire = conn_id_wire(conn_id)?;
            if let Some(context) = &self.v2_relay_seal {
                // Encryption consumes this allocation. Preserve plaintext only
                // for the existing Relay-closed-to-P2P fallback mode; exact V2
                // Relay lanes do not pay this copy.
                let fallback = fallback_needed.then(|| {
                    Bytes::copy_from_slice(&record[crate::relay_crypto::RELAY_NONCE_SIZE_V2..])
                });
                context.seal_prepared(&conn_id_wire, kind, &mut record)?;
                return Ok((
                    prepared_binary_message(conn_id.to_string(), kind, record.freeze()),
                    fallback,
                ));
            }
            record.advance(crate::relay_crypto::RELAY_NONCE_SIZE_V2);
            let plaintext = record.freeze();
            let fallback = fallback_needed.then(|| plaintext.clone());
            return Ok((
                prepared_binary_message(conn_id.to_string(), kind, plaintext),
                fallback,
            ));
        }

        record.advance(crate::relay_crypto::RELAY_NONCE_SIZE_V2);
        let plaintext = record.freeze();
        let fallback = fallback_needed.then(|| plaintext.clone());
        Ok((
            prepared_binary_message(conn_id.to_string(), kind, plaintext),
            fallback,
        ))
    }

    fn prepared_message_from_plaintext(
        &self,
        conn_id: &str,
        kind: RelayFramedKindV2,
        plaintext: Bytes,
        path: PathKind,
    ) -> tp_transport::Result<BinaryMessage> {
        if path != PathKind::Relay || self.v2_relay_seal.is_none() {
            return Ok(prepared_binary_message(
                conn_id.to_string(),
                kind,
                plaintext,
            ));
        }
        let mut record = BytesMut::with_capacity(
            plaintext.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2,
        );
        record.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
        record.extend_from_slice(&plaintext);
        let conn_id_wire = conn_id_wire(conn_id)?;
        self.v2_relay_seal
            .as_ref()
            .expect("checked V2 Relay context")
            .seal_prepared(&conn_id_wire, kind, &mut record)?;
        Ok(prepared_binary_message(
            conn_id.to_string(),
            kind,
            record.freeze(),
        ))
    }

    pub(crate) async fn send_with_path(
        &self,
        msg: BinaryMessage,
    ) -> (PathKind, tp_transport::Result<()>) {
        let (kind, _selected_p2p, result) = self.send_with_path_and_session(msg).await;
        (kind, result)
    }

    pub(crate) async fn send_with_path_and_session(
        &self,
        msg: BinaryMessage,
    ) -> (
        PathKind,
        Option<Arc<tp_transport::session::Session>>,
        tp_transport::Result<()>,
    ) {
        self.remove_datagram_association_for_close(&msg);
        let bytes = data_plane_byte_count(&msg);
        let (sess, kind) = self.pick_with_kind();
        let selected_p2p = if kind == PathKind::P2p {
            Some(sess.clone())
        } else {
            None
        };
        if udp_connect_requires_unavailable_datagram(&msg, &sess) {
            return (
                kind,
                selected_p2p,
                Err(tp_transport::TransportError::DatagramUnavailable),
            );
        }
        let outbound = match self.message_for_path(&msg, kind) {
            Ok(outbound) => outbound,
            Err(error) => return (kind, selected_p2p, Err(error)),
        };
        let result = sess.send(outbound).await;
        if result.is_ok() {
            self.record_bytes(kind, bytes);
            self.record_route_decision(&msg, kind, Some(&sess), None, None);
            return (kind, selected_p2p, result);
        }
        if kind == PathKind::P2p {
            if !matches!(result, Err(tp_transport::TransportError::Closed)) {
                return (kind, selected_p2p, result);
            }
            if self.relay_fallback_disabled() {
                self.close_failed_p2p_session(&sess, false, &msg);
                return (kind, selected_p2p, result);
            }
            let fallback_p2p_session_id = self.close_failed_p2p_session(&sess, true, &msg);
            let relay = self.multi.relay().clone();
            let fallback = match self.message_for_path(&msg, PathKind::Relay) {
                Ok(outbound) => relay.send(outbound).await,
                Err(error) => Err(error),
            };
            if fallback.is_ok() {
                self.record_bytes(PathKind::Relay, bytes);
                self.record_route_decision(
                    &msg,
                    PathKind::Relay,
                    Some(&relay),
                    Some("closed"),
                    fallback_p2p_session_id,
                );
            }
            return (PathKind::Relay, None, fallback);
        }
        if matches!(result, Err(tp_transport::TransportError::Closed))
            && self.p2p_fallback_enabled()
        {
            return self.send_via_p2p_after_relay_closed(msg, bytes).await;
        }
        (kind, selected_p2p, result)
    }

    /// Non-blocking variant for `pipe_udp`'s `try_send` path. Equivalent
    /// to `SessionSender::try_send`. On success, attributes payload
    /// bytes to the chosen path (see `send`).
    pub fn try_send(&self, msg: BinaryMessage) -> Result<(), TrySendKind> {
        self.remove_datagram_association_for_close(&msg);
        let bytes = data_plane_byte_count(&msg);
        let (sess, kind) = self.pick_with_kind();
        let outbound = self
            .message_for_path(&msg, kind)
            .map_err(|_| TrySendKind::Closed)?;
        match sess.try_send(outbound) {
            Ok(()) => {
                self.record_bytes(kind, bytes);
                self.record_route_decision(&msg, kind, Some(&sess), None, None);
                Ok(())
            }
            Err(TrySendKind::Full) if kind == PathKind::P2p => Err(TrySendKind::Full),
            Err(TrySendKind::Closed) if kind == PathKind::P2p => {
                if self.relay_fallback_disabled() {
                    self.close_failed_p2p_session(&sess, false, &msg);
                    return Err(TrySendKind::Closed);
                }
                let fallback_p2p_session_id = self.close_failed_p2p_session(&sess, true, &msg);
                let relay = self.multi.relay().clone();
                let outbound = self
                    .message_for_path(&msg, PathKind::Relay)
                    .map_err(|_| TrySendKind::Closed)?;
                relay.try_send(outbound)?;
                self.record_bytes(PathKind::Relay, bytes);
                self.record_route_decision(
                    &msg,
                    PathKind::Relay,
                    Some(&relay),
                    Some("closed"),
                    fallback_p2p_session_id,
                );
                Ok(())
            }
            Err(TrySendKind::Closed) if kind == PathKind::Relay && self.p2p_fallback_enabled() => {
                self.try_send_via_p2p_after_relay_closed(msg, bytes)
            }
            Err(e) => Err(e),
        }
    }

    async fn send_via_p2p_after_relay_closed(
        &self,
        msg: BinaryMessage,
        bytes: i64,
    ) -> (
        PathKind,
        Option<Arc<tp_transport::session::Session>>,
        tp_transport::Result<()>,
    ) {
        let Some(p2p) = self.multi.p2p_for_new_flow() else {
            return (
                PathKind::Relay,
                None,
                Err(tp_transport::TransportError::Closed),
            );
        };
        if udp_connect_requires_unavailable_datagram(&msg, &p2p) {
            return (
                PathKind::P2p,
                Some(p2p),
                Err(tp_transport::TransportError::DatagramUnavailable),
            );
        }
        let p2p_session_id = self.multi.p2p_session_id_for_handle(&p2p);
        let result = p2p.send(msg.clone()).await;
        if result.is_ok() {
            self.record_bytes(PathKind::P2p, bytes);
            self.record_route_decision(
                &msg,
                PathKind::P2p,
                Some(&p2p),
                Some("relay_closed"),
                p2p_session_id,
            );
        } else if matches!(result, Err(tp_transport::TransportError::Closed)) {
            self.close_failed_p2p_session(&p2p, false, &msg);
        }
        (PathKind::P2p, Some(p2p), result)
    }

    fn try_send_via_p2p_after_relay_closed(
        &self,
        msg: BinaryMessage,
        bytes: i64,
    ) -> Result<(), TrySendKind> {
        let Some(p2p) = self.multi.p2p_for_new_flow() else {
            return Err(TrySendKind::Closed);
        };
        let p2p_session_id = self.multi.p2p_session_id_for_handle(&p2p);
        match p2p.try_send(msg.clone()) {
            Ok(()) => {
                self.record_bytes(PathKind::P2p, bytes);
                self.record_route_decision(
                    &msg,
                    PathKind::P2p,
                    Some(&p2p),
                    Some("relay_closed"),
                    p2p_session_id,
                );
                Ok(())
            }
            Err(TrySendKind::Closed) => {
                self.close_failed_p2p_session(&p2p, false, &msg);
                Err(TrySendKind::Closed)
            }
            Err(e) => Err(e),
        }
    }

    fn close_failed_p2p_session(
        &self,
        session: &Arc<tp_transport::session::Session>,
        report_relay_migration: bool,
        msg: &BinaryMessage,
    ) -> Option<SessionId> {
        let p2p_session_id = self
            .selected_p2p_session_id
            .or_else(|| self.multi.p2p_session_id_for_handle(session));
        let conn_id = route_conn_id(msg);
        if self.multi.close_p2p_session_for_handle(session) && report_relay_migration {
            self.multi.report_p2p_to_relay_migration_with_context(
                "closed",
                conn_id,
                self.local_client_id.as_deref(),
                p2p_session_id,
            );
        }
        p2p_session_id
    }

    fn relay_fallback_disabled(&self) -> bool {
        matches!(self.mode, RouteMode::PinnedP2pNoRelayFallback(_))
    }

    fn p2p_fallback_enabled(&self) -> bool {
        matches!(self.mode, RouteMode::RelayWithP2pFallback)
    }

    fn remove_datagram_association_for_close(&self, msg: &BinaryMessage) {
        if let BinaryMessage::Close { conn_id } = msg {
            self.multi
                .remove_datagram_association_from_all_paths(conn_id);
        }
    }

    fn pick_with_kind(&self) -> (Arc<tp_transport::session::Session>, PathKind) {
        match &self.mode {
            RouteMode::Scheduled => self.multi.pick_with_kind(),
            RouteMode::P2pPreferred => self.multi.pick_p2p_first_with_kind(),
            RouteMode::PinnedP2p(p2p) | RouteMode::PinnedP2pNoRelayFallback(p2p) => {
                self.multi.record_path_pick(PathKind::P2p);
                (p2p.clone(), PathKind::P2p)
            }
            RouteMode::RelayOnly | RouteMode::RelayWithP2pFallback => {
                (self.multi.relay().clone(), PathKind::Relay)
            }
        }
    }

    /// Route payload bytes to metrics/status counters. Zero-byte
    /// frames still advance the watchdog progress epoch after a successful
    /// transport send, without faking byte increments.
    fn record_bytes(&self, kind: PathKind, bytes: i64) {
        if bytes <= 0 {
            if bytes == 0 {
                self.multi.mark_progress();
            }
            return;
        }
        if let Some(m) = self.multi.metrics() {
            let label = match kind {
                PathKind::Relay => tp_metrics::P2pPathKind::Relay,
                PathKind::P2p => tp_metrics::P2pPathKind::P2p,
            };
            m.incr_p2p_bytes(label, bytes);
        }
        self.multi.record_traffic_tx(kind, bytes);
    }

    fn record_route_decision(
        &self,
        msg: &BinaryMessage,
        kind: PathKind,
        session: Option<&Arc<tp_transport::session::Session>>,
        fallback_reason: Option<&'static str>,
        fallback_p2p_session_id: Option<SessionId>,
    ) {
        let encoded = encode_path_kind(kind);
        let previous = self.last_logged_kind.swap(encoded, Ordering::Relaxed);
        let is_connect = matches!(msg, BinaryMessage::Connect { .. });
        if previous == encoded && !is_connect {
            tracing::trace!(
                path = path_kind_label(kind),
                msg = route_msg_label(msg),
                conn_id = route_conn_id(msg).unwrap_or(""),
                payload_len = route_payload_len(msg).unwrap_or(0),
                fallback_reason = fallback_reason.unwrap_or(""),
                local_client_id = self.local_client_id.as_deref().unwrap_or(""),
                selected_p2p_session_id = ?fallback_p2p_session_id,
                peer = %session.map(|s| s.peer_addr().to_string()).unwrap_or_default(),
                "tunnel data path"
            );
            return;
        }

        tracing::info!(
            previous_path = decode_path_kind_label(previous),
            path = path_kind_label(kind),
            msg = route_msg_label(msg),
            conn_id = route_conn_id(msg).unwrap_or(""),
            payload_len = route_payload_len(msg).unwrap_or(0),
            fallback_reason = fallback_reason.unwrap_or(""),
            local_client_id = self.local_client_id.as_deref().unwrap_or(""),
            selected_p2p_session_id = ?fallback_p2p_session_id,
            peer = %session.map(|s| s.peer_addr().to_string()).unwrap_or_default(),
            route_mode = route_mode_label(&self.mode),
            "tunnel data path selected"
        );
    }

    fn record_prepared_route_decision(
        &self,
        conn_id: &str,
        framed_kind: RelayFramedKindV2,
        payload_bytes: i64,
        path: PathKind,
        session: Option<&Arc<tp_transport::session::Session>>,
        fallback_reason: Option<&'static str>,
    ) {
        let encoded = encode_path_kind(path);
        let previous = self.last_logged_kind.swap(encoded, Ordering::Relaxed);
        if previous == encoded {
            tracing::trace!(
                path = path_kind_label(path),
                msg = prepared_kind_label(framed_kind),
                conn_id,
                payload_len = payload_bytes,
                fallback_reason = fallback_reason.unwrap_or(""),
                local_client_id = self.local_client_id.as_deref().unwrap_or(""),
                peer = %session.map(|s| s.peer_addr().to_string()).unwrap_or_default(),
                "tunnel data path"
            );
            return;
        }
        tracing::info!(
            previous_path = decode_path_kind_label(previous),
            path = path_kind_label(path),
            msg = prepared_kind_label(framed_kind),
            conn_id,
            payload_len = payload_bytes,
            fallback_reason = fallback_reason.unwrap_or(""),
            local_client_id = self.local_client_id.as_deref().unwrap_or(""),
            peer = %session.map(|s| s.peer_addr().to_string()).unwrap_or_default(),
            route_mode = route_mode_label(&self.mode),
            "tunnel data path selected"
        );
    }

    #[cfg(test)]
    fn last_logged_kind_for_test(&self) -> Option<PathKind> {
        match self.last_logged_kind.load(Ordering::Relaxed) {
            LAST_PATH_RELAY => Some(PathKind::Relay),
            LAST_PATH_P2P => Some(PathKind::P2p),
            _ => None,
        }
    }

    /// Resolves only when **relay** is gone. Does NOT fire when only P2P
    /// drops — pipes keep running on relay. Tasks 4.9 / 4.10 use this in
    /// `tokio::select!` arms in place of `SessionSender::closed()`.
    pub async fn closed(&self) {
        match self.mode {
            RouteMode::RelayWithP2pFallback => {
                self.multi.relay().closed().await;
                while self.multi.p2p_session_count() > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            _ => self.multi.relay().closed().await,
        }
    }
}

fn udp_connect_requires_unavailable_datagram(
    msg: &BinaryMessage,
    session: &tp_transport::session::Session,
) -> bool {
    matches!(msg, BinaryMessage::Connect { network, .. } if network == "udp")
        && session.udp_data_mode() == tp_transport::UdpDataMode::QuicDatagramRequired
        && !session.udp_datagram_available()
}

fn encode_path_kind(kind: PathKind) -> u8 {
    match kind {
        PathKind::Relay => LAST_PATH_RELAY,
        PathKind::P2p => LAST_PATH_P2P,
    }
}

fn decode_path_kind_label(kind: u8) -> &'static str {
    match kind {
        LAST_PATH_RELAY => "relay",
        LAST_PATH_P2P => "p2p",
        _ => "unset",
    }
}

fn path_kind_label(kind: PathKind) -> &'static str {
    match kind {
        PathKind::Relay => "relay",
        PathKind::P2p => "p2p",
    }
}

fn route_mode_label(mode: &RouteMode) -> &'static str {
    match mode {
        RouteMode::Scheduled => "scheduled",
        RouteMode::P2pPreferred => "p2p_preferred",
        RouteMode::PinnedP2p(_) => "p2p_pinned",
        RouteMode::PinnedP2pNoRelayFallback(_) => "p2p_pinned_no_relay_fallback",
        RouteMode::RelayOnly => "relay_only",
        RouteMode::RelayWithP2pFallback => "relay_with_p2p_fallback",
    }
}

fn route_msg_label(msg: &BinaryMessage) -> &'static str {
    match msg {
        BinaryMessage::Connect { network, .. } if network == "udp" => "connect_udp",
        BinaryMessage::Connect { network, .. } if network == "tcp" => "connect_tcp",
        BinaryMessage::Connect { .. } => "connect",
        BinaryMessage::Data { .. } => "tcp_data",
        BinaryMessage::UdpData { .. } => "udp_data",
        BinaryMessage::Close { .. } => "close",
        BinaryMessage::RelayRouteBind { .. } => "relay_route_bind",
        BinaryMessage::RelayRouteBindAck { .. } => "relay_route_bind_ack",
        _ => "control",
    }
}

fn prepared_kind_label(kind: RelayFramedKindV2) -> &'static str {
    match kind {
        RelayFramedKindV2::Data => "tcp_data",
        RelayFramedKindV2::UdpData => "udp_data",
    }
}

fn route_conn_id(msg: &BinaryMessage) -> Option<&str> {
    match msg {
        BinaryMessage::Connect { conn_id, .. }
        | BinaryMessage::ConnectResponse { conn_id, .. }
        | BinaryMessage::Close { conn_id }
        | BinaryMessage::Data { conn_id, .. }
        | BinaryMessage::UdpData { conn_id, .. }
        | BinaryMessage::UdpFragment { conn_id, .. }
        | BinaryMessage::RelayRouteBind { conn_id, .. }
        | BinaryMessage::RelayRouteBindAck { conn_id, .. } => Some(conn_id.as_str()),
        _ => None,
    }
}

fn route_payload_len(msg: &BinaryMessage) -> Option<usize> {
    match msg {
        BinaryMessage::Data { payload, .. } | BinaryMessage::UdpData { payload, .. } => {
            Some(payload.len())
        }
        BinaryMessage::UdpFragment { payload, .. } => Some(payload.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Migration coverage. The router's per-frame `pick()` is the
    //! whole point of the router; without these tests the scheduler could
    //! silently mis-route Data frames and nothing would catch it until rollout.

    use super::*;
    use bytes::{BufMut, Bytes, BytesMut};
    use dashmap::DashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tp_core::p2p_types::SessionId;
    use tp_core::protocol::{unpack, BinaryMessage, PackedMessage};
    use tp_metrics::MetricsManager;
    use tp_transport::session::{Session, SessionStats};
    use tp_transport::DropOldestSender;

    use crate::p2p::session::MultiSession;

    /// Build a `Session` whose stream-tx feeds the returned receiver. The
    /// test drains the receiver to observe what frames the router actually
    /// sent on this path.
    fn channel_session() -> (Arc<Session>, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        (
            Arc::new(Session::new_channeled(
                out_tx, in_rx, peer, closer, writer, reader,
            )),
            out_rx,
        )
    }

    fn channel_session_with_stats(
        stats: SessionStats,
    ) -> (Arc<Session>, mpsc::Receiver<PackedMessage>) {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let mut session = Session::new_channeled(out_tx, in_rx, peer, closer, writer, reader);
        session.install_stats_probe(Arc::new(move || stats));
        (Arc::new(session), out_rx)
    }

    fn stats(rtt_ms: u64, loss_rate: f64, pto_count: u32) -> SessionStats {
        SessionStats {
            rtt: Duration::from_millis(rtt_ms),
            loss_rate,
            pto_count,
        }
    }

    fn make_multi_with_relay(relay: Arc<Session>) -> Arc<MultiSession> {
        let inbound: Arc<DashMap<String, mpsc::Sender<Bytes>>> = Arc::new(DashMap::new());
        let udp_inbound: Arc<DashMap<String, DropOldestSender<Bytes>>> = Arc::new(DashMap::new());
        MultiSession::new_with_existing_maps(relay, inbound, udp_inbound)
    }

    fn data_frame(seq: u8) -> BinaryMessage {
        BinaryMessage::Data {
            conn_id: format!("conn-{seq}"),
            payload: Bytes::from_static(b"x"),
        }
    }

    fn prepared_record(payload: &[u8]) -> BytesMut {
        let mut record =
            BytesMut::with_capacity(payload.len() + crate::relay_crypto::RELAY_SEALED_OVERHEAD_V2);
        record.put_bytes(0, crate::relay_crypto::RELAY_NONCE_SIZE_V2);
        record.extend_from_slice(payload);
        record
    }

    /// Drain whatever PackedMessages have queued on `rx` without awaiting.
    fn drain(rx: &mut mpsc::Receiver<PackedMessage>) -> Vec<BinaryMessage> {
        let mut out = Vec::new();
        while let Ok(packed) = rx.try_recv() {
            out.push(unpack(&packed.to_bytes()).expect("decode"));
        }
        out
    }

    /// Phase H pre-flight: with both paths installed and healthy, the
    /// scheduler warms over 3 cycles, then steady-state picks P2P. Frames
    /// must land on the P2P sink and the `p2p_path_picks_total{kind=p2p}`
    /// counter must increment.
    #[tokio::test]
    async fn router_routes_to_p2p_after_scheduler_warmup() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));

        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));

        let router = MultiSenderRouter::new(multi.clone());

        // Send 4 frames. Scheduler defaults need 3 healthy cycles before
        // it returns P2p; frames 1 + 2 go on relay, frames 3 + 4 on P2P.
        for i in 0..4u8 {
            router.send(data_frame(i)).await.expect("send");
        }

        let relay_msgs = drain(&mut relay_rx);
        let p2p_msgs = drain(&mut p2p_rx);
        assert_eq!(
            relay_msgs.len(),
            2,
            "first two frames must warm scheduler on relay"
        );
        assert_eq!(
            p2p_msgs.len(),
            2,
            "frames 3 + 4 must land on P2P after scheduler warmup; got {} on P2P",
            p2p_msgs.len()
        );

        let promtext = metrics.prometheus_text();
        assert!(
            promtext.contains("p2p_path_picks_total{kind=\"p2p\"} 2"),
            "metrics must record 2 P2P picks; got:\n{promtext}"
        );
        assert!(
            promtext.contains("p2p_path_picks_total{kind=\"relay\"} 2"),
            "metrics must record 2 relay picks; got:\n{promtext}"
        );
    }

    /// `p2p_bytes_total{path}` accumulates payload bytes per
    /// chosen path. Frames sent during the relay warm-up land in
    /// `path="relay"`; frames sent after the scheduler flips land in
    /// `path="p2p"`. The exact split is dictated by the default
    /// `stable_cycles=3` warm-up that the existing router tests rely
    /// on.
    #[tokio::test]
    async fn multi_sender_increments_bytes_for_chosen_path() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));

        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));

        let router = MultiSenderRouter::new(multi.clone());

        // Frames 1-2: warm-up → relay (1-byte payload each).
        // Frame 3: P2p (1-byte payload).
        for i in 0..3u8 {
            router.send(data_frame(i)).await.expect("send");
        }
        // Drain to confirm routing actually happened (matches the
        // pre-existing router tests).
        let _ = drain(&mut relay_rx);
        let _ = drain(&mut p2p_rx);

        // Send a fatter frame on the now-steady P2p path: 32 bytes.
        let fat = BinaryMessage::Data {
            conn_id: "conn-fat".into(),
            payload: Bytes::from_static(&[0xAB; 32]),
        };
        router.send(fat).await.expect("send fat");
        let _ = drain(&mut p2p_rx);

        let text = metrics.prometheus_text();
        // Relay total: 2 bytes (frames 1-2).
        assert!(
            text.contains("p2p_bytes_total{path=\"relay\"} 2"),
            "relay path bytes must equal warm-up payloads (2):\n{text}"
        );
        // P2p total: 1 + 32 = 33 bytes (frame 3, fat).
        assert!(
            text.contains("p2p_bytes_total{path=\"p2p\"} 33"),
            "p2p path bytes must total 33 (1+32):\n{text}"
        );
    }

    #[tokio::test]
    async fn multi_sender_updates_status_traffic_tx_for_chosen_path() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));

        let traffic = Arc::new(crate::status::TrafficCounters::default());
        multi.set_traffic(Some(traffic.clone()));

        let router = MultiSenderRouter::new(multi.clone());

        for i in 0..3u8 {
            router.send(data_frame(i)).await.expect("send");
        }
        let _ = drain(&mut relay_rx);
        let _ = drain(&mut p2p_rx);

        router
            .send(BinaryMessage::Data {
                conn_id: "conn-fat".into(),
                payload: Bytes::from_static(&[0xAB; 32]),
            })
            .await
            .expect("send fat");

        assert_eq!(
            traffic.snapshot(),
            crate::status::TrafficStats {
                relay_tx_bytes: 2,
                relay_rx_bytes: 0,
                p2p_tx_bytes: 33,
                p2p_rx_bytes: 0,
            }
        );
    }

    #[tokio::test]
    async fn prepared_relay_payload_keeps_producer_allocation_and_exact_boundary() {
        let (relay, mut relay_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        let router = MultiSenderRouter::new_relay_only(multi);
        let record = prepared_record(b"producer-owned");
        let allocation = record.as_ptr();

        router
            .send_prepared_data("prepared-1".into(), RelayFramedKindV2::Data, record)
            .await
            .expect("send prepared payload");

        let packed = relay_rx.recv().await.expect("packed payload");
        let payload = packed.payload.expect("out-of-band payload");
        assert_eq!(payload, b"producer-owned".as_slice());
        assert_eq!(
            payload.as_ptr(),
            unsafe { allocation.add(crate::relay_crypto::RELAY_NONCE_SIZE_V2) },
            "unencrypted path must advance the producer buffer view without copying"
        );
    }

    #[tokio::test]
    async fn prepared_udp_keeps_drop_on_full_without_relay_switch() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, _p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_p2p_preferred(multi);

        for _ in 0..16 {
            router
                .try_send_prepared_data(
                    "prepared-dg".into(),
                    RelayFramedKindV2::UdpData,
                    prepared_record(b"datagram"),
                )
                .expect("fill P2P queue");
        }
        assert!(matches!(
            router.try_send_prepared_data(
                "prepared-dg".into(),
                RelayFramedKindV2::UdpData,
                prepared_record(b"overflow"),
            ),
            Err(TrySendKind::Full)
        ));
        assert!(drain(&mut relay_rx).is_empty());
    }

    #[tokio::test]
    async fn prepared_try_send_preserves_oversized_error_classification() {
        let (relay, _relay_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        let router = MultiSenderRouter::new_relay_only(multi);
        let oversized = vec![0_u8; crate::relay_crypto::MAX_RELAY_PLAINTEXT_V2 + 1];

        assert!(matches!(
            router.try_send_prepared_data(
                "prepared-dg".into(),
                RelayFramedKindV2::UdpData,
                prepared_record(&oversized),
            ),
            Err(TrySendKind::TooLarge(_))
        ));
    }

    /// A failed underlying `Session::send` MUST NOT bump
    /// `p2p_bytes_total`. The metric counts delivered bytes, not attempted.
    /// Today the `?` operator short-circuits before `record_bytes` —
    /// regression guard so a future refactor can't move the bump above the
    /// fallible await.
    #[tokio::test]
    async fn failed_send_does_not_bump_bytes_counter() {
        let (out_tx, out_rx) = mpsc::channel::<PackedMessage>(16);
        let (_in_tx, in_rx) = mpsc::channel::<BinaryMessage>(1);
        let writer = tokio::spawn(async {});
        let reader = tokio::spawn(async {});
        let peer: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let closer: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(|| {});
        let relay = Arc::new(Session::new_channeled(
            out_tx, in_rx, peer, closer, writer, reader,
        ));
        // Drop the receiver so any send returns SendError.
        drop(out_rx);

        let multi = make_multi_with_relay(relay);
        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));
        let router = MultiSenderRouter::new(multi);

        let result = router
            .send(BinaryMessage::Data {
                conn_id: "conn-x".into(),
                payload: Bytes::from_static(&[0xAB; 64]),
            })
            .await;
        assert!(result.is_err(), "send must fail when receiver dropped");

        let text = metrics.prometheus_text();
        assert!(
            text.contains("p2p_bytes_total{path=\"relay\"} 0"),
            "failed send must not bump relay bytes counter; got:\n{text}"
        );
        assert!(
            text.contains("p2p_bytes_total{path=\"p2p\"} 0"),
            "p2p bytes counter unchanged when send fails; got:\n{text}"
        );
    }

    /// Phase H pre-flight: when the P2P session drops mid-flow, subsequent
    /// frames must fall back to relay seamlessly — no panic, no dropped
    /// frame, no migration teardown of the per-conn pipe.
    #[tokio::test]
    async fn router_falls_back_to_relay_after_p2p_drop() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));

        let metrics = MetricsManager::new();
        multi.set_metrics(Some(metrics.clone()));

        let router = MultiSenderRouter::new(multi.clone());

        // Warm to P2P.
        for i in 0..3u8 {
            router.send(data_frame(i)).await.expect("send");
        }
        // Drain so subsequent assertions are local to the post-drop phase.
        let _ = drain(&mut relay_rx);
        let _ = drain(&mut p2p_rx);

        // P2P drops.
        multi.set_p2p(None);

        // Subsequent frames must arrive on relay.
        for i in 10..13u8 {
            router
                .send(data_frame(i))
                .await
                .expect("send must not fail on P2P drop");
        }

        let relay_msgs = drain(&mut relay_rx);
        let p2p_msgs = drain(&mut p2p_rx);
        assert_eq!(
            relay_msgs.len(),
            3,
            "all 3 post-drop frames must land on relay; got {} on relay, {} on P2P",
            relay_msgs.len(),
            p2p_msgs.len()
        );
        assert!(
            p2p_msgs.is_empty(),
            "no frames must reach the dropped P2P sink"
        );

        let promtext = metrics.prometheus_text();
        assert!(
            promtext.contains("p2p_path_picks_total{kind=\"relay\"} 5"),
            "metrics must show 5 relay picks (2 warmup + 3 post-drop); got:\n{promtext}"
        );
    }

    #[tokio::test]
    async fn router_removes_closed_p2p_when_falling_back_to_relay() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        drop(p2p_rx);

        let router = MultiSenderRouter::new_p2p_preferred(multi.clone());
        router
            .send(BinaryMessage::Data {
                conn_id: "conn-closed-p2p".into(),
                payload: Bytes::from_static(b"x"),
            })
            .await
            .expect("same-replica relay fallback should send");

        let relay_msgs = drain(&mut relay_rx);
        assert_eq!(relay_msgs.len(), 1);
        assert_eq!(
            multi.p2p_session_count(),
            0,
            "closed P2P session must be removed after relay fallback"
        );
    }

    #[tokio::test]
    async fn router_closed_p2p_fallback_ignores_relay_rtt_and_loss_score() {
        let (relay, mut relay_rx) = channel_session_with_stats(stats(2500, 0.35, 9));
        let (p2p, p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        drop(p2p_rx);

        let router = MultiSenderRouter::new_p2p_preferred(multi.clone());
        router
            .send(BinaryMessage::Data {
                conn_id: "conn-closed-high-rtt".into(),
                payload: Bytes::from_static(b"x"),
            })
            .await
            .expect("same-replica relay fallback should not use RTT/loss as liveness");

        let relay_msgs = drain(&mut relay_rx);
        assert_eq!(relay_msgs.len(), 1);
        assert_eq!(
            multi.p2p_session_count(),
            0,
            "Closed is the signal that clears P2P, not relay health scoring"
        );
    }

    #[tokio::test]
    async fn router_returns_full_on_p2p_full_without_switching_to_relay() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_p2p_preferred(multi.clone());

        for i in 0..16u8 {
            router
                .try_send(BinaryMessage::UdpData {
                    conn_id: "p2pfull0001".into(),
                    payload: Bytes::from(vec![i]),
                })
                .expect("p2p queue accepts until full");
        }

        match router.try_send(BinaryMessage::UdpData {
            conn_id: "p2pfull0001".into(),
            payload: Bytes::from_static(b"relay"),
        }) {
            Err(TrySendKind::Full) => {}
            other => panic!("expected Full without switching to relay, got {other:?}"),
        }

        let relay_msgs = drain(&mut relay_rx);
        assert!(relay_msgs.is_empty(), "P2P Full must not switch to relay");
        assert_eq!(
            multi.p2p_session_count(),
            1,
            "Full is queue pressure, not a closed P2P session"
        );
        assert_eq!(
            drain(&mut p2p_rx).len(),
            16,
            "fallback must not enqueue the overflow packet on P2P"
        );
    }

    #[tokio::test]
    async fn router_returns_full_when_p2p_and_same_replica_relay_are_full() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, p2p_rx) = channel_session();
        let relay_handle = relay.clone();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_p2p_preferred(multi.clone());

        for i in 0..16u8 {
            relay_handle
                .try_send(BinaryMessage::UdpData {
                    conn_id: "relayfull001".into(),
                    payload: Bytes::from(vec![i]),
                })
                .expect("relay queue accepts until full");
        }
        for i in 0..16u8 {
            router
                .try_send(BinaryMessage::UdpData {
                    conn_id: "p2pfull0002".into(),
                    payload: Bytes::from(vec![i]),
                })
                .expect("p2p queue accepts until full");
        }

        match router.try_send(BinaryMessage::UdpData {
            conn_id: "p2pfull0002".into(),
            payload: Bytes::from_static(b"full"),
        }) {
            Err(TrySendKind::Full) => {}
            other => panic!("expected Full when both same-replica paths are full, got {other:?}"),
        }

        assert_eq!(
            multi.p2p_session_count(),
            1,
            "queue pressure must not close the P2P session"
        );
        drop(p2p_rx);
        assert_eq!(
            drain(&mut relay_rx).len(),
            16,
            "overflow packet must not cross to another replica"
        );
    }

    #[tokio::test]
    async fn router_does_not_fallback_to_unhealthy_relay_on_p2p_full() {
        let (relay, mut relay_rx) = channel_session_with_stats(stats(30, 0.20, 0));
        let (p2p, mut p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_p2p_preferred(multi.clone());

        for i in 0..16u8 {
            router
                .try_send(BinaryMessage::UdpData {
                    conn_id: "p2pfull0003".into(),
                    payload: Bytes::from(vec![i]),
                })
                .expect("p2p queue accepts until full");
        }

        match router.try_send(BinaryMessage::UdpData {
            conn_id: "p2pfull0003".into(),
            payload: Bytes::from_static(b"full"),
        }) {
            Err(TrySendKind::Full) => {}
            other => panic!("expected Full instead of switching to unhealthy relay, got {other:?}"),
        }

        assert!(drain(&mut relay_rx).is_empty());
        assert_eq!(drain(&mut p2p_rx).len(), 16);
        assert_eq!(multi.p2p_session_count(), 1);
    }

    #[tokio::test]
    async fn pinned_router_does_not_flap_to_relay_on_transient_p2p_loss_stats() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session_with_stats(stats(30, 0.30, 5));
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p.clone()));
        let router = MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p);

        router
            .try_send(BinaryMessage::UdpData {
                conn_id: "pinned-stable".into(),
                payload: Bytes::from_static(b"video"),
            })
            .expect("pinned P2P should keep using the ingress session");

        assert!(drain(&mut relay_rx).is_empty());
        let p2p_msgs = drain(&mut p2p_rx);
        assert_eq!(p2p_msgs.len(), 1);
        assert_eq!(router.last_logged_kind_for_test(), Some(PathKind::P2p));
    }

    #[tokio::test]
    async fn same_lane_fallback_ignores_high_rtt_loss_queue_pressure() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session_with_stats(stats(2500, 0.50, 9));
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p.clone()));
        let router =
            MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p).with_local_client_id("local-a");

        router
            .send(BinaryMessage::Data {
                conn_id: "conn-high-pressure".into(),
                payload: Bytes::from_static(b"x"),
            })
            .await
            .expect("high RTT/loss/PTO must not make pinned P2P fallback");

        assert!(drain(&mut relay_rx).is_empty());
        assert_eq!(drain(&mut p2p_rx).len(), 1);
        assert_eq!(multi.p2p_session_count(), 1);

        for i in 0..16u8 {
            router
                .try_send(BinaryMessage::UdpData {
                    conn_id: "udp-high-pressure".into(),
                    payload: Bytes::from(vec![i]),
                })
                .expect("pinned P2P queue accepts until full");
        }

        match router.try_send(BinaryMessage::UdpData {
            conn_id: "udp-high-pressure".into(),
            payload: Bytes::from_static(b"overflow"),
        }) {
            Err(TrySendKind::Full) => {}
            other => panic!("expected Full without same-lane relay fallback, got {other:?}"),
        }

        assert!(
            drain(&mut relay_rx).is_empty(),
            "queue pressure must not send overflow on relay"
        );
        assert_eq!(
            multi.p2p_session_count(),
            1,
            "RTT/loss/PTO and queue pressure are not P2P close signals"
        );
    }

    #[tokio::test]
    async fn same_lane_fallback_never_uses_scheduler_score() {
        let (relay, mut relay_rx) = channel_session();
        let (p2p_a, p2p_a_rx) = channel_session();
        let (p2p_b, mut p2p_b_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        let p2p_a_id = SessionId::from_bytes([0xA1; 16]);
        let p2p_b_id = SessionId::from_bytes([0xB2; 16]);
        multi
            .install_p2p_session(p2p_a_id, "peer-a".into(), p2p_a.clone())
            .expect("install selected P2P session");
        multi
            .install_p2p_session(p2p_b_id, "peer-b".into(), p2p_b)
            .expect("install alternate P2P session");
        drop(p2p_a_rx);

        let router =
            MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p_a).with_local_client_id("local-a");

        router
            .send(BinaryMessage::Data {
                conn_id: "conn-selected-closed".into(),
                payload: Bytes::from_static(b"x"),
            })
            .await
            .expect("selected closed P2P should fall back only to same-lane relay");

        assert_eq!(
            drain(&mut relay_rx).len(),
            1,
            "closed selected P2P must fall back to relay"
        );
        assert!(
            drain(&mut p2p_b_rx).is_empty(),
            "established pinned flow must not try an alternate P2P candidate"
        );
        assert!(!multi.has_p2p_session(p2p_a_id));
        assert!(multi.has_p2p_session(p2p_b_id));
    }

    #[tokio::test]
    async fn pinned_router_keeps_selected_p2p_session_id_after_registry_removal() {
        let (relay, _relay_rx) = channel_session();
        let (p2p, _p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        let p2p_id = SessionId::from_bytes([0xC3; 16]);
        multi
            .install_p2p_session(p2p_id, "peer-c".into(), p2p.clone())
            .expect("install selected P2P session");

        let router = MultiSenderRouter::new_pinned_p2p(multi.clone(), p2p.clone())
            .with_local_client_id("local-c");
        assert!(multi.close_p2p_session(p2p_id));

        assert_eq!(
            router.close_failed_p2p_session(
                &p2p,
                true,
                &BinaryMessage::Data {
                    conn_id: "conn-removed-p2p".into(),
                    payload: Bytes::from_static(b"x"),
                },
            ),
            Some(p2p_id),
            "fallback logs need the originally selected P2P session id even if another path removed it first"
        );
    }

    #[tokio::test]
    async fn router_tracks_effective_path_for_first_payload_and_fallback() {
        let (relay, _relay_rx) = channel_session();
        let (p2p, _p2p_rx) = channel_session();
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_p2p_preferred(multi.clone());

        assert_eq!(router.last_logged_kind_for_test(), None);

        router
            .send(BinaryMessage::UdpData {
                conn_id: "conn-route".into(),
                payload: Bytes::from_static(b"video"),
            })
            .await
            .expect("p2p payload send");
        assert_eq!(router.last_logged_kind_for_test(), Some(PathKind::P2p));

        multi.set_p2p(None);
        router
            .try_send(BinaryMessage::UdpData {
                conn_id: "conn-route".into(),
                payload: Bytes::from_static(b"input"),
            })
            .expect("relay fallback payload send");
        assert_eq!(router.last_logged_kind_for_test(), Some(PathKind::Relay));
    }

    #[tokio::test]
    async fn relay_sticky_router_falls_back_to_p2p_only_after_relay_closed() {
        let (relay, relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        drop(relay_rx);
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_relay_with_p2p_fallback(multi.clone());

        router
            .try_send(BinaryMessage::UdpData {
                conn_id: "relay2p2p".into(),
                payload: Bytes::from_static(b"recover"),
            })
            .expect("closed relay should recover through installed P2P");

        let p2p_msgs = drain(&mut p2p_rx);
        assert_eq!(p2p_msgs.len(), 1);
        match &p2p_msgs[0] {
            BinaryMessage::UdpData { conn_id, payload } => {
                assert_eq!(conn_id, "relay2p2p");
                assert_eq!(payload, &Bytes::from_static(b"recover"));
            }
            other => panic!("expected UDP data on P2P fallback, got {other:?}"),
        }
        assert_eq!(router.last_logged_kind_for_test(), Some(PathKind::P2p));
    }

    #[tokio::test]
    async fn prepared_encrypted_relay_fallback_restores_plaintext_for_p2p() {
        let (relay, relay_rx) = channel_session();
        let (p2p, mut p2p_rx) = channel_session();
        drop(relay_rx);
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let conn_id = "seal-fall-1";
        let seal = V2RelaySealContext::new(
            "tunnel-1".into(),
            SessionId::from_bytes([0x61; 16]),
            "source-peer".into(),
            "target-peer".into(),
            conn_id_wire(conn_id).expect("valid conn id"),
            Arc::new(
                crate::relay_crypto::RelayCipherV2::from_directional_keys_for_test(
                    [0x71; 32], [0x72; 32],
                ),
            ),
        )
        .expect("seal context");
        let router = MultiSenderRouter::new_relay_with_p2p_fallback(multi).with_v2_relay_seal(seal);

        router
            .try_send_prepared_data(
                conn_id.into(),
                RelayFramedKindV2::UdpData,
                prepared_record(b"fallback-plaintext"),
            )
            .expect("closed encrypted Relay should recover through P2P");

        let mut messages = drain(&mut p2p_rx);
        assert_eq!(messages.len(), 1);
        match messages.remove(0) {
            BinaryMessage::UdpData {
                conn_id: received_conn_id,
                payload,
            } => {
                assert_eq!(received_conn_id, conn_id);
                assert_eq!(payload, b"fallback-plaintext".as_slice());
            }
            other => panic!("expected plaintext P2P fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn relay_sticky_router_closed_waits_while_p2p_fallback_is_available() {
        let (relay, relay_rx) = channel_session();
        let (p2p, _p2p_rx) = channel_session();
        drop(relay_rx);
        let multi = make_multi_with_relay(relay);
        multi.set_p2p(Some(p2p));
        let router = MultiSenderRouter::new_relay_with_p2p_fallback(multi);

        let closed = tokio::time::timeout(Duration::from_millis(20), router.closed()).await;
        assert!(
            closed.is_err(),
            "relay-sticky pipes must stay open after relay closes when P2P fallback is installed"
        );
    }
}
