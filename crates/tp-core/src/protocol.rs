//! Binary wire protocol.
//!
//! All lengths are big-endian. Connection IDs are fixed-size 12 bytes, zero-padded.

use bytes::{BufMut, Bytes, BytesMut};
use std::net::Ipv4Addr;
use thiserror::Error;

use crate::config::ClientRoleConfig;
use crate::p2p_codec::{read_fixed_bytes_at, read_i8_at, read_u8_at, write_fixed_bytes};
use crate::p2p_types::{
    Candidate, CandidateKind, CertFingerprint, NatHint, P2pRole, SessionId, TeardownReason,
    CERT_FP_SIZE, SESSION_ID_SIZE,
};
use crate::provisioning::PublicPeerMembershipV2;
use crate::types::CONN_ID_SIZE;

pub const PROTOCOL_VERSION: u8 = 4;
pub const HEADER_SIZE: usize = 2;
pub const AUTH_STATUS_SUCCESS: &str = "success";
pub const AUTH_STATUS_FAILED: &str = "failed";
pub const MAX_P2P_SIGNED_BODY_V2: usize = 64 * 1024;
pub const MAX_ENCRYPTED_PEER_CONTROL_V2_SEALED: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    Connect = 0x01,
    ConnectResponse = 0x02,
    Close = 0x03,
    // 0x04 / 0x05 are permanently reserved: they were the retired
    // PortForward / PortForwardResponse pair. Never reuse these codes.
    Auth = 0x06,
    AuthResponse = 0x07,
    Error = 0x08,
    Heartbeat = 0x09,
    HeartbeatAck = 0x0A,
    Data = 0x10,
    UdpData = 0x11,
    UdpFragment = 0x12,
    P2pAnnounce = 0x20,
    P2pAnnounceAck = 0x21,
    P2pOffer = 0x22,
    P2pAnswer = 0x23,
    P2pPunchSync = 0x24,
    P2pProbe = 0x25,
    P2pProbeAck = 0x26,
    P2pSessionReady = 0x27,
    P2pTeardown = 0x28,
    P2pPeerHint = 0x29,
    RelayRouteBind = 0x2A,
    RelayRouteBindAck = 0x2B,
    AuthV2Challenge = 0x2C,
    AuthV2Proof = 0x2D,
    P2pOfferV2 = 0x2E,
    P2pAnswerV2 = 0x2F,
    EncryptedPeerControlV2 = 0x30,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Connect,
            0x02 => Self::ConnectResponse,
            0x03 => Self::Close,
            0x06 => Self::Auth,
            0x07 => Self::AuthResponse,
            0x08 => Self::Error,
            0x09 => Self::Heartbeat,
            0x0A => Self::HeartbeatAck,
            0x10 => Self::Data,
            0x11 => Self::UdpData,
            0x12 => Self::UdpFragment,
            0x20 => Self::P2pAnnounce,
            0x21 => Self::P2pAnnounceAck,
            0x22 => Self::P2pOffer,
            0x23 => Self::P2pAnswer,
            0x24 => Self::P2pPunchSync,
            0x25 => Self::P2pProbe,
            0x26 => Self::P2pProbeAck,
            0x27 => Self::P2pSessionReady,
            0x28 => Self::P2pTeardown,
            0x29 => Self::P2pPeerHint,
            0x2A => Self::RelayRouteBind,
            0x2B => Self::RelayRouteBindAck,
            0x2C => Self::AuthV2Challenge,
            0x2D => Self::AuthV2Proof,
            0x2E => Self::P2pOfferV2,
            0x2F => Self::P2pAnswerV2,
            0x30 => Self::EncryptedPeerControlV2,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportCapabilities {
    pub route_bind_control_v1: bool,
    pub tcp_flow_stream_v1: bool,
    pub relay_source_attestation_v1: bool,
    pub peer_mesh_v2: bool,
}

impl TransportCapabilities {
    const ROUTE_BIND_CONTROL_V1: u8 = 0x01;
    const TCP_FLOW_STREAM_V1: u8 = 0x02;
    const RELAY_SOURCE_ATTESTATION_V1: u8 = 0x04;
    const PEER_MESH_V2: u8 = 0x08;

    pub fn flags(self) -> u8 {
        let mut flags = 0;
        if self.route_bind_control_v1 {
            flags |= Self::ROUTE_BIND_CONTROL_V1;
        }
        if self.tcp_flow_stream_v1 {
            flags |= Self::TCP_FLOW_STREAM_V1;
        }
        if self.relay_source_attestation_v1 {
            flags |= Self::RELAY_SOURCE_ATTESTATION_V1;
        }
        if self.peer_mesh_v2 {
            flags |= Self::PEER_MESH_V2;
        }
        flags
    }

    pub fn from_flags(flags: u8) -> Self {
        Self {
            route_bind_control_v1: flags & Self::ROUTE_BIND_CONTROL_V1 != 0,
            tcp_flow_stream_v1: flags & Self::TCP_FLOW_STREAM_V1 != 0,
            relay_source_attestation_v1: flags & Self::RELAY_SOURCE_ATTESTATION_V1 != 0,
            peer_mesh_v2: flags & Self::PEER_MESH_V2 != 0,
        }
    }
}

const TCP_FLOW_STREAM_PREFACE_VERSION: u8 = 1;
pub const TCP_FLOW_OPEN_V2_VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpFlowStreamPreface {
    pub conn_id: String,
    pub network: String,
    pub address: String,
}

/// Opaque end-to-end TCP Flow OPEN carried on the existing QUIC
/// one-bidirectional-stream-per-flow path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpFlowOpenV2 {
    pub conn_id: String,
    pub peerlink_session_id: [u8; 16],
    pub sealed_open: Bytes,
}

pub fn pack_tcp_flow_stream_preface(preface: &TcpFlowStreamPreface) -> Bytes {
    let mut buf = BytesMut::with_capacity(32 + preface.address.len());
    buf.put_u8(TCP_FLOW_STREAM_PREFACE_VERSION);
    write_conn_id(&mut buf, &preface.conn_id);
    write_u16_bytes(&mut buf, preface.network.as_bytes());
    write_u16_bytes(&mut buf, preface.address.as_bytes());
    buf.freeze()
}

pub fn unpack_tcp_flow_stream_preface(frame: &[u8]) -> Result<TcpFlowStreamPreface, ProtoError> {
    let frame = Bytes::copy_from_slice(frame);
    if frame.is_empty() {
        return Err(ProtoError::TooShort(0));
    }
    if frame[0] != TCP_FLOW_STREAM_PREFACE_VERSION {
        return Err(ProtoError::BadVersion(frame[0]));
    }
    let mut pos = 1;
    let conn_id = read_conn_id_at(&frame, &mut pos)?;
    let network = read_u16_string_at(&frame, &mut pos)?;
    let address = read_u16_string_at(&frame, &mut pos)?;
    if pos != frame.len() {
        return Err(ProtoError::BadLength);
    }
    Ok(TcpFlowStreamPreface {
        conn_id,
        network,
        address,
    })
}

pub fn pack_tcp_flow_open_v2(open: &TcpFlowOpenV2) -> Bytes {
    let mut buf = BytesMut::with_capacity(
        1 + CONN_ID_SIZE + open.peerlink_session_id.len() + open.sealed_open.len(),
    );
    buf.put_u8(TCP_FLOW_OPEN_V2_VERSION);
    write_conn_id(&mut buf, &open.conn_id);
    buf.extend_from_slice(&open.peerlink_session_id);
    buf.extend_from_slice(&open.sealed_open);
    buf.freeze()
}

/// Read only the version and fixed-width connection id shared by TCP Flow
/// OPEN versions. Gateways use this narrow parser for routing without
/// inspecting the V2 PeerLink id or sealed OPEN body.
pub fn tcp_flow_open_route(frame: &[u8]) -> Result<(u8, String), ProtoError> {
    let Some(version) = frame.first().copied() else {
        return Err(ProtoError::TooShort(0));
    };
    if version != TCP_FLOW_STREAM_PREFACE_VERSION && version != TCP_FLOW_OPEN_V2_VERSION {
        return Err(ProtoError::BadVersion(version));
    }
    if frame.len().saturating_sub(1) < CONN_ID_SIZE {
        return Err(ProtoError::TooShort(frame.len().saturating_sub(1)));
    }
    let raw_conn_id = &frame[1..1 + CONN_ID_SIZE];
    let end = raw_conn_id
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(CONN_ID_SIZE);
    let conn_id = String::from_utf8(raw_conn_id[..end].to_vec())?;
    Ok((version, conn_id))
}

pub fn unpack_tcp_flow_open_v2(frame: &[u8]) -> Result<TcpFlowOpenV2, ProtoError> {
    let frame = Bytes::copy_from_slice(frame);
    let (version, conn_id) = tcp_flow_open_route(&frame)?;
    if version != TCP_FLOW_OPEN_V2_VERSION {
        return Err(ProtoError::BadVersion(version));
    }
    let mut pos = 1 + CONN_ID_SIZE;
    let peerlink_session_id = read_fixed_bytes_at(&frame, &mut pos)?;
    Ok(TcpFlowOpenV2 {
        conn_id,
        peerlink_session_id,
        sealed_open: frame.slice(pos..),
    })
}

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("message too short: {0} bytes")]
    TooShort(usize),
    #[error("unsupported protocol version: {0}")]
    BadVersion(u8),
    #[error("unknown message type: {0:#x}")]
    BadType(u8),
    #[error("invalid length field")]
    BadLength,
    #[error("utf-8 decode error: {0}")]
    BadUtf8(#[from] std::string::FromUtf8Error),
}

/// Parsed message — owns its payload bytes.
#[derive(Debug, Clone)]
pub enum BinaryMessage {
    Connect {
        conn_id: String,
        network: String,
        address: String,
    },
    ConnectResponse {
        conn_id: String,
        success: bool,
        error: String,
    },
    Close {
        conn_id: String,
    },
    Auth {
        tunnel_id: String,
        client_id: String,
        group_id: String,
        username: String,
        password: String,
        group_password: String,
        role: ClientRoleConfig,
        capabilities: TransportCapabilities,
    },
    AuthResponse {
        status: String,
        reason: String,
        capabilities: TransportCapabilities,
    },
    Error(String),
    Heartbeat {
        client_id: String,
        timestamp: i64,
    },
    HeartbeatAck {
        timestamp: i64,
    },
    Data {
        conn_id: String,
        payload: Bytes,
    },
    UdpData {
        conn_id: String,
        payload: Bytes,
    },
    UdpFragment {
        conn_id: String,
        frag_id: u32,
        frag_index: u8,
        frag_total: u8,
        payload: Bytes,
    },
    P2pAnnounce {
        client_id: String,
        group_id: String,
        locals: Vec<(String, u16)>,
        nat_hint: NatHint,
        cert_fp: CertFingerprint,
    },
    P2pAnnounceAck {
        public_ip: String,
        public_port: u16,
        server_time_ms: i64,
    },
    P2pOffer {
        session_id: SessionId,
        src_client_id: String,
        dst_client_id: String,
        candidates: Vec<Candidate>,
        src_cert_fp: CertFingerprint,
        role: P2pRole,
    },
    P2pAnswer {
        session_id: SessionId,
        accepted_client_id: String,
        ok: bool,
        reason: String,
        candidates: Vec<Candidate>,
        dst_cert_fp: CertFingerprint,
    },
    P2pPunchSync {
        session_id: SessionId,
        t_start_ms: i64,
        burst_count: u8,
        port_offsets: Vec<i8>,
    },
    P2pProbe {
        session_id: SessionId,
        seq: u32,
        sent_ms: i64,
    },
    P2pProbeAck {
        session_id: SessionId,
        seq: u32,
        recv_ms: i64,
    },
    P2pSessionReady {
        session_id: SessionId,
        rtt_us: u32,
        chosen_remote_ip: String,
        chosen_remote_port: u16,
    },
    P2pTeardown {
        session_id: SessionId,
        reason: TeardownReason,
    },
    P2pPeerHint {
        peer_client_id: String,
    },
    RelayRouteBind {
        conn_id: String,
        peer_client_id: String,
    },
    RelayRouteBindAck {
        conn_id: String,
        success: bool,
        error: String,
    },
    AuthV2Challenge {
        challenge: [u8; 32],
    },
    AuthV2Proof {
        membership: PublicPeerMembershipV2,
        signature: String,
    },
    P2pOfferV2 {
        source_peer_id: String,
        target_peer_id: String,
        signed_offer: Bytes,
    },
    P2pAnswerV2 {
        source_peer_id: String,
        target_peer_id: String,
        signed_answer: Bytes,
    },
    EncryptedPeerControlV2 {
        target_peer_id: String,
        peerlink_session_id: [u8; 16],
        conn_id: [u8; 12],
        route_abort: bool,
        sealed: Bytes,
    },
}

/// A packed `BinaryMessage` carried as up to two zero-copy `Bytes` chunks.
///
/// `header` holds `[version][type][…non-payload fields]` — always present.
/// `payload` holds the trailing variable-length payload when the message
/// variant carries one (`Data` / `UdpData`); `None` for every other
/// variant. Keeping the payload unmerged on the stream-outbound path lets
/// the QUIC writer `write_chunks(&[len, header, payload])` via vectored I/O
/// instead
/// of the old `extend_from_slice(payload)` which paid a per-frame memcpy at
/// ~42 MB/s on a 4K / 30 kpps game-streaming workload.
///
/// Transports that cannot express two chunks in a single wire-level frame
/// (WebSocket Binary, gRPC message body, QUIC datagram) call
/// [`PackedMessage::to_bytes`] to get a contiguous `Bytes`. That path
/// still has to copy `payload` into a merge buffer once, but it matches
/// the pre-split behavior byte-for-byte.
#[derive(Debug, Clone)]
pub struct PackedMessage {
    pub header: Bytes,
    pub payload: Option<Bytes>,
}

impl PackedMessage {
    /// Total on-the-wire length: header + optional payload.
    pub fn total_len(&self) -> usize {
        self.header.len() + self.payload.as_ref().map_or(0, |p| p.len())
    }

    /// Merge header and payload into a single contiguous `Bytes`. Required
    /// by transports that can't writev (WebSocket Binary, gRPC body, QUIC
    /// datagram). Zero-copy when `payload` is `None` (just clones the
    /// header Arc); otherwise one memcpy of `payload.len()` bytes.
    pub fn to_bytes(&self) -> Bytes {
        match &self.payload {
            None => self.header.clone(),
            Some(payload) => {
                let mut buf = BytesMut::with_capacity(self.header.len() + payload.len());
                buf.extend_from_slice(&self.header);
                buf.extend_from_slice(payload);
                buf.freeze()
            }
        }
    }
}

pub fn pack(msg: &BinaryMessage) -> PackedMessage {
    // Initial capacity 64 B covers every message header with
    // many entries; the variants that carry a large trailing payload
    // (`Data`, `UdpData`) pull the payload OUT of the header buffer below
    // so it's never memcpy'd in the first place.
    let mut buf = BytesMut::with_capacity(64);
    buf.put_u8(PROTOCOL_VERSION);
    let type_pos = buf.len();
    buf.put_u8(0);

    // For `Data` / `UdpData` we stash the payload out-of-band so the
    // caller can writev or merge at its discretion. Every other variant
    // leaves `payload_out = None` and writes everything into `buf`.
    let mut payload_out: Option<Bytes> = None;

    let ty = match msg {
        BinaryMessage::Connect {
            conn_id,
            network,
            address,
        } => {
            write_conn_id(&mut buf, conn_id);
            write_u16_bytes(&mut buf, network.as_bytes());
            write_u16_bytes(&mut buf, address.as_bytes());
            MsgType::Connect
        }
        BinaryMessage::ConnectResponse {
            conn_id,
            success,
            error,
        } => {
            write_conn_id(&mut buf, conn_id);
            buf.put_u8(if *success { 1 } else { 0 });
            write_u16_bytes(&mut buf, error.as_bytes());
            MsgType::ConnectResponse
        }
        BinaryMessage::Close { conn_id } => {
            write_conn_id(&mut buf, conn_id);
            MsgType::Close
        }
        BinaryMessage::Auth {
            tunnel_id,
            client_id,
            group_id,
            username,
            password,
            group_password,
            role,
            capabilities,
        } => {
            write_u16_bytes(&mut buf, tunnel_id.as_bytes());
            write_u16_bytes(&mut buf, client_id.as_bytes());
            write_u16_bytes(&mut buf, group_id.as_bytes());
            write_u16_bytes(&mut buf, username.as_bytes());
            write_u16_bytes(&mut buf, password.as_bytes());
            write_u16_bytes(&mut buf, group_password.as_bytes());
            write_u16_bytes(&mut buf, encode_client_role(*role).as_bytes());
            buf.put_u8(capabilities.flags());
            MsgType::Auth
        }
        BinaryMessage::AuthResponse {
            status,
            reason,
            capabilities,
        } => {
            write_u16_bytes(&mut buf, status.as_bytes());
            write_u16_bytes(&mut buf, reason.as_bytes());
            buf.put_u8(capabilities.flags());
            MsgType::AuthResponse
        }
        BinaryMessage::Error(msg) => {
            write_u16_bytes(&mut buf, msg.as_bytes());
            MsgType::Error
        }
        BinaryMessage::Heartbeat {
            client_id,
            timestamp,
        } => {
            write_u16_bytes(&mut buf, client_id.as_bytes());
            buf.put_i64(*timestamp);
            MsgType::Heartbeat
        }
        BinaryMessage::HeartbeatAck { timestamp } => {
            buf.put_i64(*timestamp);
            MsgType::HeartbeatAck
        }
        BinaryMessage::Data { conn_id, payload } => {
            write_conn_id(&mut buf, conn_id);
            // Pull the payload out-of-band. QUIC stream writer will
            // writev it next to the header; other transports merge via
            // `to_bytes`. Either way the payload never rides through
            // a per-frame `extend_from_slice` here.
            payload_out = Some(payload.clone());
            MsgType::Data
        }
        BinaryMessage::UdpData { conn_id, payload } => {
            write_conn_id(&mut buf, conn_id);
            payload_out = Some(payload.clone());
            MsgType::UdpData
        }
        BinaryMessage::UdpFragment {
            conn_id,
            frag_id,
            frag_index,
            frag_total,
            payload,
        } => {
            write_conn_id(&mut buf, conn_id);
            buf.put_u32(*frag_id);
            buf.put_u8(*frag_index);
            buf.put_u8(*frag_total);
            payload_out = Some(payload.clone());
            MsgType::UdpFragment
        }
        BinaryMessage::P2pAnnounce {
            client_id,
            group_id,
            locals,
            nat_hint,
            cert_fp,
        } => {
            write_u16_bytes(&mut buf, client_id.as_bytes());
            write_u16_bytes(&mut buf, group_id.as_bytes());
            buf.put_u8(locals.len().min(u8::MAX as usize) as u8);
            for (ip, port) in locals.iter().take(u8::MAX as usize) {
                write_u16_bytes(&mut buf, ip.as_bytes());
                buf.put_u16(*port);
            }
            buf.put_u8(nat_hint.as_u8());
            write_fixed_bytes(&mut buf, cert_fp.as_bytes());
            MsgType::P2pAnnounce
        }
        BinaryMessage::P2pAnnounceAck {
            public_ip,
            public_port,
            server_time_ms,
        } => {
            write_u16_bytes(&mut buf, public_ip.as_bytes());
            buf.put_u16(*public_port);
            buf.put_i64(*server_time_ms);
            MsgType::P2pAnnounceAck
        }
        BinaryMessage::P2pOffer {
            session_id,
            src_client_id,
            dst_client_id,
            candidates,
            src_cert_fp,
            role,
        } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            write_u16_bytes(&mut buf, src_client_id.as_bytes());
            write_u16_bytes(&mut buf, dst_client_id.as_bytes());
            buf.put_u8(candidates.len().min(u8::MAX as usize) as u8);
            for c in candidates.iter().take(u8::MAX as usize) {
                write_u16_bytes(&mut buf, c.ip.as_bytes());
                buf.put_u16(c.port);
                buf.put_u8(c.kind.as_u8());
            }
            write_fixed_bytes(&mut buf, src_cert_fp.as_bytes());
            buf.put_u8(role.as_u8());
            MsgType::P2pOffer
        }
        BinaryMessage::P2pAnswer {
            session_id,
            accepted_client_id,
            ok,
            reason,
            candidates,
            dst_cert_fp,
        } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            write_u16_bytes(&mut buf, accepted_client_id.as_bytes());
            buf.put_u8(if *ok { 1 } else { 0 });
            write_u16_bytes(&mut buf, reason.as_bytes());
            buf.put_u8(candidates.len().min(u8::MAX as usize) as u8);
            for c in candidates.iter().take(u8::MAX as usize) {
                write_u16_bytes(&mut buf, c.ip.as_bytes());
                buf.put_u16(c.port);
                buf.put_u8(c.kind.as_u8());
            }
            write_fixed_bytes(&mut buf, dst_cert_fp.as_bytes());
            MsgType::P2pAnswer
        }
        BinaryMessage::P2pPunchSync {
            session_id,
            t_start_ms,
            burst_count,
            port_offsets,
        } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            buf.put_i64(*t_start_ms);
            buf.put_u8(*burst_count);
            buf.put_u8(port_offsets.len().min(u8::MAX as usize) as u8);
            for off in port_offsets.iter().take(u8::MAX as usize) {
                buf.put_i8(*off);
            }
            MsgType::P2pPunchSync
        }
        BinaryMessage::P2pProbe {
            session_id,
            seq,
            sent_ms,
        } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            buf.put_u32(*seq);
            buf.put_i64(*sent_ms);
            MsgType::P2pProbe
        }
        BinaryMessage::P2pProbeAck {
            session_id,
            seq,
            recv_ms,
        } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            buf.put_u32(*seq);
            buf.put_i64(*recv_ms);
            MsgType::P2pProbeAck
        }
        BinaryMessage::P2pSessionReady {
            session_id,
            rtt_us,
            chosen_remote_ip,
            chosen_remote_port,
        } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            buf.put_u32(*rtt_us);
            write_u16_bytes(&mut buf, chosen_remote_ip.as_bytes());
            buf.put_u16(*chosen_remote_port);
            MsgType::P2pSessionReady
        }
        BinaryMessage::P2pTeardown { session_id, reason } => {
            write_fixed_bytes(&mut buf, session_id.as_bytes());
            buf.put_u8(reason.as_u8());
            MsgType::P2pTeardown
        }
        BinaryMessage::P2pPeerHint { peer_client_id } => {
            write_u16_bytes(&mut buf, peer_client_id.as_bytes());
            MsgType::P2pPeerHint
        }
        BinaryMessage::RelayRouteBind {
            conn_id,
            peer_client_id,
        } => {
            write_conn_id(&mut buf, conn_id);
            write_u16_bytes(&mut buf, peer_client_id.as_bytes());
            MsgType::RelayRouteBind
        }
        BinaryMessage::RelayRouteBindAck {
            conn_id,
            success,
            error,
        } => {
            write_conn_id(&mut buf, conn_id);
            buf.put_u8(if *success { 1 } else { 0 });
            write_u16_bytes(&mut buf, error.as_bytes());
            MsgType::RelayRouteBindAck
        }
        BinaryMessage::AuthV2Challenge { challenge } => {
            buf.extend_from_slice(challenge);
            MsgType::AuthV2Challenge
        }
        BinaryMessage::AuthV2Proof {
            membership,
            signature,
        } => {
            write_u16_bytes(&mut buf, membership.tunnel_id.as_bytes());
            write_u16_bytes(&mut buf, membership.peer_id.as_bytes());
            buf.extend_from_slice(&membership.overlay_ip.octets());
            write_u16_bytes(&mut buf, membership.peer_public_key.as_bytes());
            write_u16_bytes(&mut buf, membership.membership_signature.as_bytes());
            write_u16_bytes(&mut buf, signature.as_bytes());
            MsgType::AuthV2Proof
        }
        BinaryMessage::P2pOfferV2 {
            source_peer_id,
            target_peer_id,
            signed_offer,
        } => {
            write_u16_bytes(&mut buf, source_peer_id.as_bytes());
            write_u16_bytes(&mut buf, target_peer_id.as_bytes());
            payload_out = Some(signed_offer.clone());
            MsgType::P2pOfferV2
        }
        BinaryMessage::P2pAnswerV2 {
            source_peer_id,
            target_peer_id,
            signed_answer,
        } => {
            write_u16_bytes(&mut buf, source_peer_id.as_bytes());
            write_u16_bytes(&mut buf, target_peer_id.as_bytes());
            payload_out = Some(signed_answer.clone());
            MsgType::P2pAnswerV2
        }
        BinaryMessage::EncryptedPeerControlV2 {
            target_peer_id,
            peerlink_session_id,
            conn_id,
            route_abort,
            sealed,
        } => {
            write_u16_bytes(&mut buf, target_peer_id.as_bytes());
            buf.extend_from_slice(peerlink_session_id);
            buf.extend_from_slice(conn_id);
            buf.put_u8(u8::from(*route_abort));
            payload_out = Some(sealed.clone());
            MsgType::EncryptedPeerControlV2
        }
    };
    buf[type_pos] = ty as u8;
    PackedMessage {
        header: buf.freeze(),
        payload: payload_out,
    }
}

pub fn unpack(frame: &[u8]) -> Result<BinaryMessage, ProtoError> {
    unpack_bytes(Bytes::copy_from_slice(frame))
}

fn encode_client_role(role: ClientRoleConfig) -> &'static str {
    match role {
        ClientRoleConfig::Client => "client",
        ClientRoleConfig::App => "app",
    }
}

fn decode_client_role(raw: &str) -> ClientRoleConfig {
    match raw.trim() {
        "app" => ClientRoleConfig::App,
        _ => ClientRoleConfig::Client,
    }
}

pub fn unpack_bytes(frame: Bytes) -> Result<BinaryMessage, ProtoError> {
    if frame.len() < HEADER_SIZE {
        return Err(ProtoError::TooShort(frame.len()));
    }
    let version = frame[0];
    if version != PROTOCOL_VERSION {
        return Err(ProtoError::BadVersion(version));
    }
    let raw_ty = frame[1];
    let ty = MsgType::from_u8(raw_ty).ok_or(ProtoError::BadType(raw_ty))?;
    let mut pos = HEADER_SIZE;

    Ok(match ty {
        MsgType::Connect => BinaryMessage::Connect {
            conn_id: read_conn_id_at(&frame, &mut pos)?,
            network: read_u16_string_at(&frame, &mut pos)?,
            address: read_u16_string_at(&frame, &mut pos)?,
        },
        MsgType::ConnectResponse => {
            let conn_id = read_conn_id_at(&frame, &mut pos)?;
            if pos >= frame.len() {
                return Err(ProtoError::TooShort(0));
            }
            let success = frame[pos] == 1;
            pos += 1;
            let error = read_u16_string_at(&frame, &mut pos)?;
            BinaryMessage::ConnectResponse {
                conn_id,
                success,
                error,
            }
        }
        MsgType::Close => BinaryMessage::Close {
            conn_id: read_conn_id_at(&frame, &mut pos)?,
        },
        MsgType::Auth => {
            let tunnel_id = read_u16_string_at(&frame, &mut pos)?;
            let client_id = read_u16_string_at(&frame, &mut pos)?;
            let group_id = read_u16_string_at(&frame, &mut pos)?;
            let username = read_u16_string_at(&frame, &mut pos)?;
            let password = read_u16_string_at(&frame, &mut pos)?;
            let group_password = read_u16_string_at(&frame, &mut pos)?;
            let role = if pos < frame.len() {
                decode_client_role(&read_u16_string_at(&frame, &mut pos)?)
            } else {
                ClientRoleConfig::Client
            };
            let capabilities = if pos < frame.len() {
                TransportCapabilities::from_flags(read_u8_at(&frame, &mut pos)?)
            } else {
                TransportCapabilities::default()
            };
            BinaryMessage::Auth {
                tunnel_id,
                client_id,
                group_id,
                username,
                password,
                group_password,
                role,
                capabilities,
            }
        }
        MsgType::AuthResponse => {
            let status = read_u16_string_at(&frame, &mut pos)?;
            let reason = read_u16_string_at(&frame, &mut pos)?;
            let capabilities = if pos < frame.len() {
                TransportCapabilities::from_flags(read_u8_at(&frame, &mut pos)?)
            } else {
                TransportCapabilities::default()
            };
            BinaryMessage::AuthResponse {
                status,
                reason,
                capabilities,
            }
        }
        MsgType::Error => BinaryMessage::Error(read_u16_string_at(&frame, &mut pos)?),
        MsgType::Heartbeat => {
            let client_id = read_u16_string_at(&frame, &mut pos)?;
            if frame.len().saturating_sub(pos) < 8 {
                return Err(ProtoError::TooShort(frame.len().saturating_sub(pos)));
            }
            let timestamp = i64::from_be_bytes([
                frame[pos],
                frame[pos + 1],
                frame[pos + 2],
                frame[pos + 3],
                frame[pos + 4],
                frame[pos + 5],
                frame[pos + 6],
                frame[pos + 7],
            ]);
            BinaryMessage::Heartbeat {
                client_id,
                timestamp,
            }
        }
        MsgType::HeartbeatAck => {
            if frame.len().saturating_sub(pos) < 8 {
                return Err(ProtoError::TooShort(frame.len().saturating_sub(pos)));
            }
            let timestamp = i64::from_be_bytes([
                frame[pos],
                frame[pos + 1],
                frame[pos + 2],
                frame[pos + 3],
                frame[pos + 4],
                frame[pos + 5],
                frame[pos + 6],
                frame[pos + 7],
            ]);
            BinaryMessage::HeartbeatAck { timestamp }
        }
        MsgType::Data => {
            let conn_id = read_conn_id_at(&frame, &mut pos)?;
            BinaryMessage::Data {
                conn_id,
                payload: frame.slice(pos..),
            }
        }
        MsgType::UdpData => {
            let conn_id = read_conn_id_at(&frame, &mut pos)?;
            BinaryMessage::UdpData {
                conn_id,
                payload: frame.slice(pos..),
            }
        }
        MsgType::UdpFragment => {
            let conn_id = read_conn_id_at(&frame, &mut pos)?;
            let frag_id = read_u32_at(&frame, &mut pos)?;
            let frag_index = read_u8_at(&frame, &mut pos)?;
            let frag_total = read_u8_at(&frame, &mut pos)?;
            BinaryMessage::UdpFragment {
                conn_id,
                frag_id,
                frag_index,
                frag_total,
                payload: frame.slice(pos..),
            }
        }
        MsgType::P2pAnnounce => {
            let client_id = read_u16_string_at(&frame, &mut pos)?;
            let group_id = read_u16_string_at(&frame, &mut pos)?;
            let count = read_u8_at(&frame, &mut pos)? as usize;
            let mut locals = Vec::with_capacity(count);
            for _ in 0..count {
                let ip = read_u16_string_at(&frame, &mut pos)?;
                let port = read_u16_at(&frame, &mut pos)?;
                locals.push((ip, port));
            }
            let nat_hint =
                NatHint::from_u8(read_u8_at(&frame, &mut pos)?).ok_or(ProtoError::BadLength)?;
            let cert_fp_raw: [u8; CERT_FP_SIZE] = read_fixed_bytes_at(&frame, &mut pos)?;
            let cert_fp = CertFingerprint::from_bytes(cert_fp_raw);
            BinaryMessage::P2pAnnounce {
                client_id,
                group_id,
                locals,
                nat_hint,
                cert_fp,
            }
        }
        MsgType::P2pAnnounceAck => {
            let public_ip = read_u16_string_at(&frame, &mut pos)?;
            let public_port = read_u16_at(&frame, &mut pos)?;
            if frame.len().saturating_sub(pos) < 8 {
                return Err(ProtoError::TooShort(frame.len().saturating_sub(pos)));
            }
            let server_time_ms = i64::from_be_bytes([
                frame[pos],
                frame[pos + 1],
                frame[pos + 2],
                frame[pos + 3],
                frame[pos + 4],
                frame[pos + 5],
                frame[pos + 6],
                frame[pos + 7],
            ]);
            pos += 8;
            let _ = pos;
            BinaryMessage::P2pAnnounceAck {
                public_ip,
                public_port,
                server_time_ms,
            }
        }
        MsgType::P2pOffer => {
            let session_id_raw: [u8; SESSION_ID_SIZE] = read_fixed_bytes_at(&frame, &mut pos)?;
            let session_id = SessionId::from_bytes(session_id_raw);
            let src_client_id = read_u16_string_at(&frame, &mut pos)?;
            let dst_client_id = read_u16_string_at(&frame, &mut pos)?;
            let candidates = read_candidates(&frame, &mut pos)?;
            let src_cert_fp_raw: [u8; CERT_FP_SIZE] = read_fixed_bytes_at(&frame, &mut pos)?;
            let src_cert_fp = CertFingerprint::from_bytes(src_cert_fp_raw);
            let role =
                P2pRole::from_u8(read_u8_at(&frame, &mut pos)?).ok_or(ProtoError::BadLength)?;
            BinaryMessage::P2pOffer {
                session_id,
                src_client_id,
                dst_client_id,
                candidates,
                src_cert_fp,
                role,
            }
        }
        MsgType::P2pAnswer => {
            let session_id_raw: [u8; SESSION_ID_SIZE] = read_fixed_bytes_at(&frame, &mut pos)?;
            let session_id = SessionId::from_bytes(session_id_raw);
            let accepted_client_id = read_u16_string_at(&frame, &mut pos)?;
            let ok = read_u8_at(&frame, &mut pos)? != 0;
            let reason = read_u16_string_at(&frame, &mut pos)?;
            let candidates = read_candidates(&frame, &mut pos)?;
            let dst_cert_fp_raw: [u8; CERT_FP_SIZE] = read_fixed_bytes_at(&frame, &mut pos)?;
            let dst_cert_fp = CertFingerprint::from_bytes(dst_cert_fp_raw);
            BinaryMessage::P2pAnswer {
                session_id,
                accepted_client_id,
                ok,
                reason,
                candidates,
                dst_cert_fp,
            }
        }
        MsgType::P2pPunchSync => {
            let sid_raw: [u8; SESSION_ID_SIZE] = read_fixed_bytes_at(&frame, &mut pos)?;
            let session_id = SessionId::from_bytes(sid_raw);
            if frame.len().saturating_sub(pos) < 8 {
                return Err(ProtoError::TooShort(frame.len().saturating_sub(pos)));
            }
            let t_start_ms = i64::from_be_bytes([
                frame[pos],
                frame[pos + 1],
                frame[pos + 2],
                frame[pos + 3],
                frame[pos + 4],
                frame[pos + 5],
                frame[pos + 6],
                frame[pos + 7],
            ]);
            pos += 8;
            let burst_count = read_u8_at(&frame, &mut pos)?;
            let off_count = read_u8_at(&frame, &mut pos)? as usize;
            let mut port_offsets = Vec::with_capacity(off_count);
            for _ in 0..off_count {
                port_offsets.push(read_i8_at(&frame, &mut pos)?);
            }
            BinaryMessage::P2pPunchSync {
                session_id,
                t_start_ms,
                burst_count,
                port_offsets,
            }
        }
        MsgType::P2pProbe => BinaryMessage::P2pProbe {
            session_id: read_session_id(&frame, &mut pos)?,
            seq: read_u32_at(&frame, &mut pos)?,
            sent_ms: read_i64_at(&frame, &mut pos)?,
        },
        MsgType::P2pProbeAck => BinaryMessage::P2pProbeAck {
            session_id: read_session_id(&frame, &mut pos)?,
            seq: read_u32_at(&frame, &mut pos)?,
            recv_ms: read_i64_at(&frame, &mut pos)?,
        },
        MsgType::P2pSessionReady => BinaryMessage::P2pSessionReady {
            session_id: read_session_id(&frame, &mut pos)?,
            rtt_us: read_u32_at(&frame, &mut pos)?,
            chosen_remote_ip: read_u16_string_at(&frame, &mut pos)?,
            chosen_remote_port: read_u16_at(&frame, &mut pos)?,
        },
        MsgType::P2pTeardown => BinaryMessage::P2pTeardown {
            session_id: read_session_id(&frame, &mut pos)?,
            reason: TeardownReason::from_u8(read_u8_at(&frame, &mut pos)?)
                .ok_or(ProtoError::BadLength)?,
        },
        MsgType::P2pPeerHint => BinaryMessage::P2pPeerHint {
            peer_client_id: read_u16_string_at(&frame, &mut pos)?,
        },
        MsgType::RelayRouteBind => BinaryMessage::RelayRouteBind {
            conn_id: read_conn_id_at(&frame, &mut pos)?,
            peer_client_id: read_u16_string_at(&frame, &mut pos)?,
        },
        MsgType::RelayRouteBindAck => {
            let conn_id = read_conn_id_at(&frame, &mut pos)?;
            let success = read_u8_at(&frame, &mut pos)? != 0;
            let error = read_u16_string_at(&frame, &mut pos)?;
            BinaryMessage::RelayRouteBindAck {
                conn_id,
                success,
                error,
            }
        }
        MsgType::AuthV2Challenge => BinaryMessage::AuthV2Challenge {
            challenge: read_fixed_bytes_at(&frame, &mut pos)?,
        },
        MsgType::AuthV2Proof => {
            let tunnel_id = read_u16_string_at(&frame, &mut pos)?;
            let peer_id = read_u16_string_at(&frame, &mut pos)?;
            let overlay_ip = Ipv4Addr::from(read_fixed_bytes_at::<4>(&frame, &mut pos)?);
            let peer_public_key = read_u16_string_at(&frame, &mut pos)?;
            let membership_signature = read_u16_string_at(&frame, &mut pos)?;
            let signature = read_u16_string_at(&frame, &mut pos)?;
            BinaryMessage::AuthV2Proof {
                membership: PublicPeerMembershipV2 {
                    tunnel_id,
                    peer_id,
                    overlay_ip,
                    peer_public_key,
                    membership_signature,
                },
                signature,
            }
        }
        MsgType::P2pOfferV2 => BinaryMessage::P2pOfferV2 {
            source_peer_id: read_u16_string_at(&frame, &mut pos)?,
            target_peer_id: read_u16_string_at(&frame, &mut pos)?,
            signed_offer: read_signed_body_v2(&frame, pos)?,
        },
        MsgType::P2pAnswerV2 => BinaryMessage::P2pAnswerV2 {
            source_peer_id: read_u16_string_at(&frame, &mut pos)?,
            target_peer_id: read_u16_string_at(&frame, &mut pos)?,
            signed_answer: read_signed_body_v2(&frame, pos)?,
        },
        MsgType::EncryptedPeerControlV2 => {
            let target_peer_id = read_u16_string_at(&frame, &mut pos)?;
            if target_peer_id.trim().is_empty() {
                return Err(ProtoError::BadLength);
            }
            let peerlink_session_id = read_fixed_bytes_at(&frame, &mut pos)?;
            let conn_id = read_fixed_bytes_at(&frame, &mut pos)?;
            let flags = read_u8_at(&frame, &mut pos)?;
            if flags & !0x01 != 0 {
                return Err(ProtoError::BadLength);
            }
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id,
                peerlink_session_id,
                conn_id,
                route_abort: flags & 0x01 != 0,
                sealed: read_encrypted_peer_control_v2_sealed(&frame, pos)?,
            }
        }
    })
}

fn write_conn_id(buf: &mut BytesMut, id: &str) {
    let mut padded = [0u8; CONN_ID_SIZE];
    let src = id.as_bytes();
    let n = src.len().min(CONN_ID_SIZE);
    padded[..n].copy_from_slice(&src[..n]);
    buf.extend_from_slice(&padded);
}

fn write_u16_bytes(buf: &mut BytesMut, data: &[u8]) {
    buf.put_u16(data.len() as u16);
    buf.extend_from_slice(data);
}

fn read_conn_id_at(frame: &Bytes, pos: &mut usize) -> Result<String, ProtoError> {
    if frame.len().saturating_sub(*pos) < CONN_ID_SIZE {
        return Err(ProtoError::TooShort(frame.len().saturating_sub(*pos)));
    }
    let raw = &frame[*pos..*pos + CONN_ID_SIZE];
    *pos += CONN_ID_SIZE;
    let end = raw.iter().position(|b| *b == 0).unwrap_or(CONN_ID_SIZE);
    Ok(String::from_utf8(raw[..end].to_vec())?)
}

fn read_u16_at(frame: &Bytes, pos: &mut usize) -> Result<u16, ProtoError> {
    if frame.len().saturating_sub(*pos) < 2 {
        return Err(ProtoError::TooShort(frame.len().saturating_sub(*pos)));
    }
    let v = u16::from_be_bytes([frame[*pos], frame[*pos + 1]]);
    *pos += 2;
    Ok(v)
}

fn read_u16_string_at(frame: &Bytes, pos: &mut usize) -> Result<String, ProtoError> {
    let len = read_u16_at(frame, pos)? as usize;
    if frame.len().saturating_sub(*pos) < len {
        return Err(ProtoError::BadLength);
    }
    let s = String::from_utf8(frame[*pos..*pos + len].to_vec())?;
    *pos += len;
    Ok(s)
}

fn read_signed_body_v2(frame: &Bytes, pos: usize) -> Result<Bytes, ProtoError> {
    let len = frame.len().saturating_sub(pos);
    if len == 0 || len > MAX_P2P_SIGNED_BODY_V2 {
        return Err(ProtoError::BadLength);
    }
    Ok(frame.slice(pos..))
}

fn read_encrypted_peer_control_v2_sealed(frame: &Bytes, pos: usize) -> Result<Bytes, ProtoError> {
    let len = frame.len().saturating_sub(pos);
    if len == 0 || len > MAX_ENCRYPTED_PEER_CONTROL_V2_SEALED {
        return Err(ProtoError::BadLength);
    }
    Ok(frame.slice(pos..))
}

fn read_session_id(frame: &Bytes, pos: &mut usize) -> Result<SessionId, ProtoError> {
    let raw: [u8; SESSION_ID_SIZE] = read_fixed_bytes_at(frame, pos)?;
    Ok(SessionId::from_bytes(raw))
}

fn read_u32_at(frame: &Bytes, pos: &mut usize) -> Result<u32, ProtoError> {
    if frame.len().saturating_sub(*pos) < 4 {
        return Err(ProtoError::TooShort(frame.len().saturating_sub(*pos)));
    }
    let v = u32::from_be_bytes([
        frame[*pos],
        frame[*pos + 1],
        frame[*pos + 2],
        frame[*pos + 3],
    ]);
    *pos += 4;
    Ok(v)
}

fn read_i64_at(frame: &Bytes, pos: &mut usize) -> Result<i64, ProtoError> {
    if frame.len().saturating_sub(*pos) < 8 {
        return Err(ProtoError::TooShort(frame.len().saturating_sub(*pos)));
    }
    let v = i64::from_be_bytes([
        frame[*pos],
        frame[*pos + 1],
        frame[*pos + 2],
        frame[*pos + 3],
        frame[*pos + 4],
        frame[*pos + 5],
        frame[*pos + 6],
        frame[*pos + 7],
    ]);
    *pos += 8;
    Ok(v)
}

fn read_candidates(frame: &Bytes, pos: &mut usize) -> Result<Vec<Candidate>, ProtoError> {
    let count = read_u8_at(frame, pos)? as usize;
    let mut cands = Vec::with_capacity(count);
    for _ in 0..count {
        let ip = read_u16_string_at(frame, pos)?;
        let port = read_u16_at(frame, pos)?;
        let kind = CandidateKind::from_u8(read_u8_at(frame, pos)?).ok_or(ProtoError::BadLength)?;
        cands.push(Candidate { ip, port, kind });
    }
    Ok(cands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p_types::{
        Candidate, CandidateKind, CertFingerprint, NatHint, P2pRole, SessionId, TeardownReason,
    };

    #[test]
    fn p2p_announce_round_trip() {
        let msg = BinaryMessage::P2pAnnounce {
            client_id: "pc-123".into(),
            group_id: "g1".into(),
            locals: vec![
                ("192.168.1.10".into(), 4433u16),
                ("10.0.0.5".into(), 5544u16),
            ],
            nat_hint: NatHint::PortRestricted,
            cert_fp: CertFingerprint::from_bytes([7u8; 32]),
        };
        let packed = pack(&msg).to_bytes();
        let parsed = unpack(&packed).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_offer_round_trip() {
        let msg = BinaryMessage::P2pOffer {
            session_id: SessionId::from_bytes([9u8; 16]),
            src_client_id: "mobile-1".into(),
            dst_client_id: "pc-1".into(),
            candidates: vec![
                Candidate {
                    ip: "1.2.3.4".into(),
                    port: 4433,
                    kind: CandidateKind::ServerReflexive,
                },
                Candidate {
                    ip: "10.0.0.1".into(),
                    port: 4433,
                    kind: CandidateKind::Host,
                },
            ],
            src_cert_fp: CertFingerprint::from_bytes([3u8; 32]),
            role: P2pRole::Initiator,
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_offer_v2_wire_is_frozen_and_keeps_signed_body_opaque() {
        let message = BinaryMessage::P2pOfferV2 {
            source_peer_id: "peer-a".into(),
            target_peer_id: "peer-b".into(),
            signed_offer: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        };

        let packed = pack(&message);
        assert_eq!(
            packed.to_bytes().as_ref(),
            &[
                PROTOCOL_VERSION,
                MsgType::P2pOfferV2 as u8,
                0,
                6,
                b'p',
                b'e',
                b'e',
                b'r',
                b'-',
                b'a',
                0,
                6,
                b'p',
                b'e',
                b'e',
                b'r',
                b'-',
                b'b',
                0xde,
                0xad,
                0xbe,
                0xef,
            ]
        );
        assert_eq!(
            packed.payload.as_deref(),
            Some(&[0xde, 0xad, 0xbe, 0xef][..])
        );
        match unpack(&packed.to_bytes()).expect("decode V2 offer") {
            BinaryMessage::P2pOfferV2 {
                source_peer_id,
                target_peer_id,
                signed_offer,
            } => {
                assert_eq!(source_peer_id, "peer-a");
                assert_eq!(target_peer_id, "peer-b");
                assert_eq!(signed_offer.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn p2p_answer_v2_wire_is_frozen_and_keeps_signed_body_opaque() {
        let message = BinaryMessage::P2pAnswerV2 {
            source_peer_id: "peer-b".into(),
            target_peer_id: "peer-a".into(),
            signed_answer: Bytes::from_static(&[1, 2, 3]),
        };

        assert_eq!(
            pack(&message).to_bytes().as_ref(),
            &[
                PROTOCOL_VERSION,
                MsgType::P2pAnswerV2 as u8,
                0,
                6,
                b'p',
                b'e',
                b'e',
                b'r',
                b'-',
                b'b',
                0,
                6,
                b'p',
                b'e',
                b'e',
                b'r',
                b'-',
                b'a',
                1,
                2,
                3,
            ]
        );
        match unpack(&pack(&message).to_bytes()).expect("decode V2 answer") {
            BinaryMessage::P2pAnswerV2 {
                source_peer_id,
                target_peer_id,
                signed_answer,
            } => {
                assert_eq!(source_peer_id, "peer-b");
                assert_eq!(target_peer_id, "peer-a");
                assert_eq!(signed_answer.as_ref(), &[1, 2, 3]);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn p2p_v2_codec_rejects_empty_truncated_and_oversized_signed_bodies() {
        let truncated = [PROTOCOL_VERSION, 0x2e, 0, 6, b'p'];
        assert!(matches!(unpack(&truncated), Err(ProtoError::BadLength)));

        let mut empty = BytesMut::new();
        empty.put_u8(PROTOCOL_VERSION);
        empty.put_u8(0x2e);
        write_u16_bytes(&mut empty, b"peer-a");
        write_u16_bytes(&mut empty, b"peer-b");
        assert!(matches!(unpack(&empty), Err(ProtoError::BadLength)));

        let mut maximum = empty.clone();
        maximum.extend_from_slice(&vec![0x7a; MAX_P2P_SIGNED_BODY_V2]);
        match unpack(&maximum).expect("maximum signed body is accepted") {
            BinaryMessage::P2pOfferV2 { signed_offer, .. } => {
                assert_eq!(signed_offer.len(), MAX_P2P_SIGNED_BODY_V2);
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let mut oversized = empty;
        oversized.extend_from_slice(&vec![0u8; MAX_P2P_SIGNED_BODY_V2 + 1]);
        assert!(matches!(unpack(&oversized), Err(ProtoError::BadLength)));
    }

    #[test]
    fn p2p_v2_unknown_message_type_is_rejected() {
        assert!(matches!(
            unpack(&[PROTOCOL_VERSION, 0x31]),
            Err(ProtoError::BadType(0x31))
        ));
    }

    #[test]
    fn p2p_answer_round_trip_ok() {
        let msg = BinaryMessage::P2pAnswer {
            session_id: SessionId::from_bytes([1u8; 16]),
            accepted_client_id: "pc-1-AbC12345-1".into(),
            ok: true,
            reason: String::new(),
            candidates: vec![Candidate {
                ip: "5.6.7.8".into(),
                port: 5544,
                kind: CandidateKind::ServerReflexive,
            }],
            dst_cert_fp: CertFingerprint::from_bytes([5u8; 32]),
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_answer_round_trip_reject() {
        let msg = BinaryMessage::P2pAnswer {
            session_id: SessionId::from_bytes([2u8; 16]),
            accepted_client_id: String::new(),
            ok: false,
            reason: "peer offline".into(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::zero(),
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_punch_sync_round_trip() {
        let msg = BinaryMessage::P2pPunchSync {
            session_id: SessionId::from_bytes([0xAA; 16]),
            t_start_ms: 1_700_000_000_500,
            burst_count: 30,
            port_offsets: vec![0i8, 1, 2, 5, -1],
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_announce_ack_round_trip() {
        let msg = BinaryMessage::P2pAnnounceAck {
            public_ip: "1.2.3.4".into(),
            public_port: 9999,
            server_time_ms: 1_700_000_000_000,
        };
        let packed = pack(&msg).to_bytes();
        let parsed = unpack(&packed).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_peer_hint_v4_bytes_are_frozen() {
        let message = BinaryMessage::P2pPeerHint {
            peer_client_id: "pc-1".into(),
        };

        assert_eq!(
            pack(&message).to_bytes().as_ref(),
            &[
                PROTOCOL_VERSION,
                MsgType::P2pPeerHint as u8,
                0,
                4,
                b'p',
                b'c',
                b'-',
                b'1',
            ],
            "transport protocol v4 forbids adding a P2pPeerHint tail",
        );
    }

    #[test]
    fn p2p_peer_hint_round_trips() {
        let message = BinaryMessage::P2pPeerHint {
            peer_client_id: "pc-1".into(),
        };

        let decoded = unpack(&pack(&message).to_bytes()).expect("P2pPeerHint");

        assert_eq!(format!("{decoded:?}"), format!("{message:?}"));
    }

    #[test]
    fn relay_route_bind_round_trip() {
        let msg = BinaryMessage::RelayRouteBind {
            conn_id: "conn-route".into(),
            peer_client_id: "pc-1".into(),
        };
        let packed = pack(&msg).to_bytes();
        let parsed = unpack(&packed).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn encrypted_peer_control_v2_round_trip_keeps_sealed_body_opaque() {
        let msg = BinaryMessage::EncryptedPeerControlV2 {
            target_peer_id: "peer-b".into(),
            peerlink_session_id: [0x22; 16],
            conn_id: *b"relayflow001",
            route_abort: false,
            sealed: Bytes::from_static(&[0x00, 0xff, 0x7e, 0x81]),
        };

        let packed = pack(&msg);
        assert_eq!(
            packed.payload.as_deref(),
            Some(&[0x00, 0xff, 0x7e, 0x81][..])
        );
        match unpack(&packed.to_bytes()).expect("encrypted control round trip") {
            BinaryMessage::EncryptedPeerControlV2 {
                target_peer_id,
                peerlink_session_id,
                conn_id,
                route_abort,
                sealed,
            } => {
                assert_eq!(target_peer_id, "peer-b");
                assert_eq!(peerlink_session_id, [0x22; 16]);
                assert_eq!(conn_id, *b"relayflow001");
                assert!(!route_abort);
                assert_eq!(sealed.as_ref(), &[0x00, 0xff, 0x7e, 0x81]);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn encrypted_peer_control_v2_rejects_unknown_flags_and_body_bounds() {
        let message = BinaryMessage::EncryptedPeerControlV2 {
            target_peer_id: "peer-b".into(),
            peerlink_session_id: [0x22; 16],
            conn_id: [0; 12],
            route_abort: false,
            sealed: Bytes::from_static(b"x"),
        };
        let packed = pack(&message).to_bytes();
        let flags_at = packed.len() - 2;

        let mut unknown_flags = packed.to_vec();
        unknown_flags[flags_at] = 0x02;
        assert!(matches!(unpack(&unknown_flags), Err(ProtoError::BadLength)));

        assert!(matches!(
            unpack(&packed[..packed.len() - 1]),
            Err(ProtoError::BadLength)
        ));

        let oversized = BinaryMessage::EncryptedPeerControlV2 {
            target_peer_id: "peer-b".into(),
            peerlink_session_id: [0x22; 16],
            conn_id: [0; 12],
            route_abort: false,
            sealed: Bytes::from(vec![0x5a; MAX_ENCRYPTED_PEER_CONTROL_V2_SEALED + 1]),
        };
        assert!(matches!(
            unpack(&pack(&oversized).to_bytes()),
            Err(ProtoError::BadLength)
        ));
    }

    #[test]
    fn p2p_probe_round_trip() {
        let msg = BinaryMessage::P2pProbe {
            session_id: SessionId::from_bytes([0x55; 16]),
            seq: 42,
            sent_ms: 1_700_000_000_123,
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_probe_ack_round_trip() {
        let msg = BinaryMessage::P2pProbeAck {
            session_id: SessionId::from_bytes([0x77; 16]),
            seq: 42,
            recv_ms: 1_700_000_000_456,
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_session_ready_round_trip() {
        let msg = BinaryMessage::P2pSessionReady {
            session_id: SessionId::from_bytes([0x11; 16]),
            rtt_us: 12_345,
            chosen_remote_ip: "1.2.3.4".into(),
            chosen_remote_port: 4433,
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn p2p_teardown_round_trip() {
        let msg = BinaryMessage::P2pTeardown {
            session_id: SessionId::from_bytes([0x22; 16]),
            reason: TeardownReason::HealthFail,
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        assert_eq!(format!("{:?}", parsed), format!("{:?}", msg));
    }

    #[test]
    fn conn_id_size_is_v2_wire_size() {
        assert_eq!(CONN_ID_SIZE, 12);
    }

    #[test]
    fn protocol_version_is_4() {
        assert_eq!(PROTOCOL_VERSION, 4);
    }

    #[test]
    fn p2p_msg_types_decode() {
        for (raw, expected) in [
            (0x20, MsgType::P2pAnnounce),
            (0x21, MsgType::P2pAnnounceAck),
            (0x22, MsgType::P2pOffer),
            (0x23, MsgType::P2pAnswer),
            (0x24, MsgType::P2pPunchSync),
            (0x25, MsgType::P2pProbe),
            (0x26, MsgType::P2pProbeAck),
            (0x27, MsgType::P2pSessionReady),
            (0x28, MsgType::P2pTeardown),
            (0x29, MsgType::P2pPeerHint),
            (0x2A, MsgType::RelayRouteBind),
            (0x2B, MsgType::RelayRouteBindAck),
            (0x2C, MsgType::AuthV2Challenge),
            (0x2D, MsgType::AuthV2Proof),
            (0x2E, MsgType::P2pOfferV2),
            (0x2F, MsgType::P2pAnswerV2),
            (0x30, MsgType::EncryptedPeerControlV2),
        ] {
            assert_eq!(MsgType::from_u8(raw), Some(expected));
        }
    }

    #[test]
    fn roundtrip_data() {
        let m = BinaryMessage::Data {
            conn_id: "abc123".into(),
            payload: Bytes::from_static(b"hello world"),
        };
        let bytes = pack(&m).to_bytes();
        match unpack(&bytes).unwrap() {
            BinaryMessage::Data { conn_id, payload } => {
                assert_eq!(conn_id, "abc123");
                assert_eq!(&payload[..], b"hello world");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_connect() {
        let m = BinaryMessage::Connect {
            conn_id: "id".into(),
            network: "tcp".into(),
            address: "example.com:443".into(),
        };
        let bytes = pack(&m).to_bytes();
        match unpack(&bytes).unwrap() {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(conn_id, "id");
                assert_eq!(network, "tcp");
                assert_eq!(address, "example.com:443");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_auth() {
        let m = BinaryMessage::Auth {
            tunnel_id: "tun-1".into(),
            client_id: "c1".into(),
            group_id: "g1".into(),
            username: "u".into(),
            password: "p".into(),
            group_password: "gp".into(),
            role: ClientRoleConfig::App,
            capabilities: TransportCapabilities {
                route_bind_control_v1: true,
                tcp_flow_stream_v1: false,
                relay_source_attestation_v1: false,
                peer_mesh_v2: true,
            },
        };
        let bytes = pack(&m).to_bytes();
        match unpack(&bytes).unwrap() {
            BinaryMessage::Auth {
                tunnel_id,
                client_id,
                group_id,
                username,
                password,
                group_password,
                role,
                capabilities,
            } => {
                assert_eq!(tunnel_id, "tun-1");
                assert_eq!(client_id, "c1");
                assert_eq!(group_id, "g1");
                assert_eq!(username, "u");
                assert_eq!(password, "p");
                assert_eq!(group_password, "gp");
                assert_eq!(role, ClientRoleConfig::App);
                assert!(capabilities.route_bind_control_v1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn auth_capabilities_default_absent_tails_to_false() {
        let mut auth = BytesMut::new();
        auth.put_u8(PROTOCOL_VERSION);
        auth.put_u8(MsgType::Auth as u8);
        write_u16_bytes(&mut auth, b"tun-1");
        write_u16_bytes(&mut auth, b"c1");
        write_u16_bytes(&mut auth, b"g1");
        write_u16_bytes(&mut auth, b"u");
        write_u16_bytes(&mut auth, b"p");
        write_u16_bytes(&mut auth, b"gp");
        write_u16_bytes(&mut auth, b"app");

        match unpack(&auth).unwrap() {
            BinaryMessage::Auth {
                role, capabilities, ..
            } => {
                assert_eq!(role, ClientRoleConfig::App);
                assert!(!capabilities.route_bind_control_v1);
                assert!(!capabilities.relay_source_attestation_v1);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let mut auth_response = BytesMut::new();
        auth_response.put_u8(PROTOCOL_VERSION);
        auth_response.put_u8(MsgType::AuthResponse as u8);
        write_u16_bytes(&mut auth_response, AUTH_STATUS_SUCCESS.as_bytes());
        write_u16_bytes(&mut auth_response, b"");

        match unpack(&auth_response).unwrap() {
            BinaryMessage::AuthResponse { capabilities, .. } => {
                assert!(!capabilities.route_bind_control_v1);
                assert!(!capabilities.relay_source_attestation_v1);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn auth_capabilities_tail_remains_decodable() {
        let auth = BinaryMessage::Auth {
            tunnel_id: "tun-1".into(),
            client_id: "c1".into(),
            group_id: "g1".into(),
            username: "u".into(),
            password: "p".into(),
            group_password: "gp".into(),
            role: ClientRoleConfig::App,
            capabilities: TransportCapabilities {
                route_bind_control_v1: true,
                tcp_flow_stream_v1: false,
                relay_source_attestation_v1: false,
                peer_mesh_v2: false,
            },
        };
        match unpack(&pack(&auth).to_bytes()).unwrap() {
            BinaryMessage::Auth { capabilities, .. } => {
                assert!(capabilities.route_bind_control_v1);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let auth_response = BinaryMessage::AuthResponse {
            status: AUTH_STATUS_SUCCESS.into(),
            reason: String::new(),
            capabilities: TransportCapabilities {
                route_bind_control_v1: true,
                tcp_flow_stream_v1: false,
                relay_source_attestation_v1: false,
                peer_mesh_v2: false,
            },
        };
        match unpack(&pack(&auth_response).to_bytes()).unwrap() {
            BinaryMessage::AuthResponse { capabilities, .. } => {
                assert!(capabilities.route_bind_control_v1);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn relay_route_bind_ack_roundtrip() {
        let msg = BinaryMessage::RelayRouteBindAck {
            conn_id: "abcdefghijkl".into(),
            success: false,
            error: "route install failed".into(),
        };
        let parsed = unpack(&pack(&msg).to_bytes()).unwrap();
        match parsed {
            BinaryMessage::RelayRouteBindAck {
                conn_id,
                success,
                error,
            } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert!(!success);
                assert_eq!(error, "route install failed");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn conn_id_wire_size_remains_12_bytes() {
        assert_eq!(CONN_ID_SIZE, 12);
    }

    #[test]
    fn roundtrip_heartbeat() {
        let m = BinaryMessage::Heartbeat {
            client_id: "c".into(),
            timestamp: 1_700_000_000,
        };
        let bytes = pack(&m).to_bytes();
        match unpack(&bytes).unwrap() {
            BinaryMessage::Heartbeat {
                client_id,
                timestamp,
            } => {
                assert_eq!(client_id, "c");
                assert_eq!(timestamp, 1_700_000_000);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// Data / UdpData MUST populate `payload` out-of-band so the QUIC
    /// stream writer can writev it. Pinning this invariant here because
    /// losing it silently re-introduces the per-frame memcpy this whole
    /// refactor was meant to delete.
    #[test]
    fn pack_splits_payload_for_data_variant() {
        let m = BinaryMessage::Data {
            conn_id: "conn".into(),
            payload: Bytes::from_static(b"payload"),
        };
        let packed = pack(&m);
        assert!(
            packed.payload.is_some(),
            "Data must expose payload out-of-band"
        );
        let payload = packed.payload.as_ref().unwrap();
        assert_eq!(&payload[..], b"payload");
        // Header must not contain the payload bytes.
        assert!(
            !packed.header.windows(7).any(|w| w == b"payload"),
            "header leaked payload bytes: {:?}",
            &packed.header[..]
        );
        assert_eq!(packed.total_len(), packed.header.len() + 7);
    }

    #[test]
    fn data_header_is_14_bytes() {
        let packed = pack(&BinaryMessage::Data {
            conn_id: "abcdefghijkl".into(),
            payload: Bytes::from_static(b"x"),
        });
        assert_eq!(packed.header.len(), 14);
        assert_eq!(packed.total_len(), 15);
    }

    /// UdpData must split payload without embedding the target address.
    /// The target is carried only by the initial Connect(udp, address).
    #[test]
    fn pack_splits_payload_for_udp_data_variant_without_addr_in_header() {
        let m = BinaryMessage::UdpData {
            conn_id: "abcdefghijkl".into(),
            payload: Bytes::from_static(b"video-frame"),
        };
        let packed = pack(&m);
        assert_eq!(packed.payload.as_deref().unwrap(), b"video-frame");
        assert_eq!(packed.header.len(), 14);
        assert!(!packed.header.windows(9).any(|w| w == b"127.0.0.1"));
    }

    #[test]
    fn udp_data_payload_sizes_pack_with_14_byte_header() {
        for (payload_len, packed_len) in [(1375, 1389), (1392, 1406), (1400, 1414), (1411, 1425)] {
            let msg = BinaryMessage::UdpData {
                conn_id: "abcdefghijkl".into(),
                payload: Bytes::from(vec![0x55; payload_len]),
            };
            assert_eq!(pack(&msg).total_len(), packed_len);
        }
    }

    #[test]
    fn udp_fragment_roundtrip_has_compact_header() {
        let msg = BinaryMessage::UdpFragment {
            conn_id: "abcdefghijkl".into(),
            frag_id: 42,
            frag_index: 1,
            frag_total: 3,
            payload: Bytes::from_static(b"fragment"),
        };
        let packed = pack(&msg);
        assert_eq!(packed.header.len(), 20);
        assert_eq!(packed.total_len(), 28);

        match unpack(&packed.to_bytes()).unwrap() {
            BinaryMessage::UdpFragment {
                conn_id,
                frag_id,
                frag_index,
                frag_total,
                payload,
            } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert_eq!(frag_id, 42);
                assert_eq!(frag_index, 1);
                assert_eq!(frag_total, 3);
                assert_eq!(&payload[..], b"fragment");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn udp_data_roundtrip_does_not_decode_target_address() {
        let msg = BinaryMessage::UdpData {
            conn_id: "abcdefghijkl".into(),
            payload: Bytes::from_static(b"payload"),
        };
        match unpack(&pack(&msg).to_bytes()).unwrap() {
            BinaryMessage::UdpData { conn_id, payload } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert_eq!(&payload[..], b"payload");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn connect_udp_still_carries_target_address() {
        let msg = BinaryMessage::Connect {
            conn_id: "abcdefghijkl".into(),
            network: "udp".into(),
            address: "10.0.0.1:47998".into(),
        };
        match unpack(&pack(&msg).to_bytes()).unwrap() {
            BinaryMessage::Connect {
                conn_id,
                network,
                address,
            } => {
                assert_eq!(conn_id, "abcdefghijkl");
                assert_eq!(network, "udp");
                assert_eq!(address, "10.0.0.1:47998");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn v1_messages_are_rejected_by_v2_parser() {
        let mut frame = BytesMut::new();
        frame.put_u8(1);
        frame.put_u8(MsgType::Data as u8);
        frame.extend_from_slice(&[0u8; 12]);
        assert!(matches!(unpack(&frame), Err(ProtoError::BadVersion(1))));
    }

    /// Non-Data variants leave `payload` empty — the whole frame is in
    /// `header`. Regression guard against accidentally pulling a control
    /// message's bytes out as "payload".
    #[test]
    fn pack_keeps_other_variants_header_only() {
        for m in [
            BinaryMessage::Heartbeat {
                client_id: "c".into(),
                timestamp: 1,
            },
            BinaryMessage::Close {
                conn_id: "cid".into(),
            },
            BinaryMessage::AuthResponse {
                status: AUTH_STATUS_SUCCESS.into(),
                reason: String::new(),
                capabilities: TransportCapabilities::default(),
            },
        ] {
            let packed = pack(&m);
            assert!(
                packed.payload.is_none(),
                "non-Data variant produced payload: {m:?}"
            );
        }
    }

    #[test]
    fn auth_v2_challenge_and_proof_round_trip() {
        use crate::provisioning::PublicPeerMembershipV2;
        use std::net::Ipv4Addr;

        let challenge = BinaryMessage::AuthV2Challenge {
            challenge: [0x4d; 32],
        };
        let proof = BinaryMessage::AuthV2Proof {
            membership: PublicPeerMembershipV2 {
                tunnel_id: "tunnel-v2".into(),
                peer_id: "peer-v2".into(),
                overlay_ip: Ipv4Addr::new(198, 18, 0, 9),
                peer_public_key: "peer-public".into(),
                membership_signature: "issuer-signature".into(),
            },
            signature: "peer-proof".into(),
        };

        match unpack(&pack(&challenge).to_bytes()).expect("challenge round trip") {
            BinaryMessage::AuthV2Challenge { challenge } => assert_eq!(challenge, [0x4d; 32]),
            other => panic!("unexpected message: {other:?}"),
        }
        match unpack(&pack(&proof).to_bytes()).expect("proof round trip") {
            BinaryMessage::AuthV2Proof {
                membership,
                signature,
            } => {
                assert_eq!(membership.tunnel_id, "tunnel-v2");
                assert_eq!(membership.peer_id, "peer-v2");
                assert_eq!(membership.overlay_ip, Ipv4Addr::new(198, 18, 0, 9));
                assert_eq!(signature, "peer-proof");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// `to_bytes()` must produce byte-identical output to the pre-split
    /// wire format — this guards Go↔Rust wire compat.
    #[test]
    fn pack_to_bytes_matches_direct_extend_equivalent() {
        let m = BinaryMessage::Data {
            conn_id: "abc".into(),
            payload: Bytes::from_static(b"abcdefghij"),
        };
        let packed = pack(&m);
        let merged = packed.to_bytes();

        // Manually construct the equivalent wire bytes to catch any
        // header/payload boundary drift (e.g. if a future refactor
        // accidentally skipped the conn_id padding).
        let mut expected = BytesMut::new();
        expected.put_u8(PROTOCOL_VERSION);
        expected.put_u8(MsgType::Data as u8);
        let mut padded = [0u8; CONN_ID_SIZE];
        padded[..3].copy_from_slice(b"abc");
        expected.extend_from_slice(&padded);
        expected.extend_from_slice(b"abcdefghij");
        assert_eq!(merged, expected.freeze());
    }

    #[test]
    fn p2p_answer_wire_places_accepted_client_id_after_session_id() {
        let sid_bytes = [0xA5; SESSION_ID_SIZE];
        let accepted_client_id = "pc-1-AbC12345-1";
        let msg = BinaryMessage::P2pAnswer {
            session_id: SessionId::from_bytes(sid_bytes),
            accepted_client_id: accepted_client_id.into(),
            ok: true,
            reason: String::new(),
            candidates: vec![],
            dst_cert_fp: CertFingerprint::zero(),
        };

        let bytes = pack(&msg).to_bytes();
        let accepted_offset = HEADER_SIZE + SESSION_ID_SIZE;
        assert_eq!(&bytes[HEADER_SIZE..accepted_offset], sid_bytes.as_slice());

        let len = u16::from_be_bytes([bytes[accepted_offset], bytes[accepted_offset + 1]]);
        assert_eq!(len as usize, accepted_client_id.len());
        let value_start = accepted_offset + 2;
        let value_end = value_start + len as usize;
        assert_eq!(
            &bytes[value_start..value_end],
            accepted_client_id.as_bytes()
        );
        assert_eq!(
            bytes[value_end], 1,
            "ok byte must follow accepted_client_id"
        );
    }

    #[test]
    fn tcp_flow_stream_preface_roundtrip() {
        let preface = TcpFlowStreamPreface {
            conn_id: "abc123".into(),
            network: "tcp".into(),
            address: "example.com:443".into(),
        };

        let encoded = pack_tcp_flow_stream_preface(&preface);
        let decoded = unpack_tcp_flow_stream_preface(&encoded).expect("decode preface");

        assert_eq!(decoded, preface);
    }

    #[test]
    fn sealed_tcp_flow_open_v2_roundtrips_while_route_parse_reads_only_outer_header() {
        let open = TcpFlowOpenV2 {
            conn_id: "flow-v2-0001".into(),
            peerlink_session_id: [0x42; 16],
            sealed_open: Bytes::from_static(b"opaque sealed destination"),
        };

        let encoded = pack_tcp_flow_open_v2(&open);
        assert_eq!(encoded[0], 2);
        assert_eq!(
            tcp_flow_open_route(&encoded).expect("route header"),
            (2, open.conn_id.clone())
        );
        assert_eq!(
            unpack_tcp_flow_open_v2(&encoded).expect("decode v2 open"),
            open
        );

        let outer_header_only = &encoded[..1 + CONN_ID_SIZE];
        assert_eq!(
            tcp_flow_open_route(outer_header_only)
                .expect("route header does not inspect sealed body"),
            (2, "flow-v2-0001".into())
        );
        assert!(unpack_tcp_flow_open_v2(outer_header_only).is_err());
    }

    #[test]
    fn auth_capabilities_roundtrip_tcp_flow_stream_v1() {
        let msg = BinaryMessage::Auth {
            tunnel_id: "t".into(),
            client_id: "c".into(),
            group_id: "g".into(),
            username: "u".into(),
            password: "p".into(),
            group_password: "gp".into(),
            role: ClientRoleConfig::Client,
            capabilities: TransportCapabilities {
                route_bind_control_v1: true,
                tcp_flow_stream_v1: true,
                relay_source_attestation_v1: false,
                peer_mesh_v2: false,
            },
        };

        match unpack(&pack(&msg).to_bytes()).expect("decode auth") {
            BinaryMessage::Auth { capabilities, .. } => {
                assert!(capabilities.route_bind_control_v1);
                assert!(capabilities.tcp_flow_stream_v1);
            }
            other => panic!("expected auth, got {other:?}"),
        }
    }

    #[test]
    fn relay_source_attestation_uses_the_existing_auth_capability_byte() {
        let message = BinaryMessage::Auth {
            tunnel_id: "t".into(),
            client_id: "c".into(),
            group_id: "g".into(),
            username: "u".into(),
            password: "p".into(),
            group_password: "gp".into(),
            role: ClientRoleConfig::Client,
            capabilities: TransportCapabilities {
                route_bind_control_v1: false,
                tcp_flow_stream_v1: false,
                relay_source_attestation_v1: true,
                peer_mesh_v2: false,
            },
        };
        let baseline = BinaryMessage::Auth {
            tunnel_id: "t".into(),
            client_id: "c".into(),
            group_id: "g".into(),
            username: "u".into(),
            password: "p".into(),
            group_password: "gp".into(),
            role: ClientRoleConfig::Client,
            capabilities: TransportCapabilities::default(),
        };

        let encoded = pack(&message).to_bytes();
        let baseline = pack(&baseline).to_bytes();
        assert_eq!(
            encoded.len(),
            baseline.len(),
            "v4 Auth layout must not grow"
        );
        assert_eq!(
            &encoded[..encoded.len() - 1],
            &baseline[..baseline.len() - 1]
        );
        assert_eq!(encoded[encoded.len() - 1], 0x04);
        match unpack(&encoded).expect("decode auth") {
            BinaryMessage::Auth { capabilities, .. } => {
                assert!(capabilities.relay_source_attestation_v1);
                assert!(!capabilities.route_bind_control_v1);
                assert!(!capabilities.tcp_flow_stream_v1);
            }
            other => panic!("expected auth, got {other:?}"),
        }
    }

    #[test]
    fn peer_mesh_v2_uses_the_existing_auth_capability_byte() {
        let capabilities = TransportCapabilities {
            peer_mesh_v2: true,
            ..Default::default()
        };
        assert_eq!(capabilities.flags(), 0x08);
        assert_eq!(
            TransportCapabilities::from_flags(0x08),
            capabilities,
            "V2 admission must use the existing capability byte"
        );
    }

    #[test]
    fn unknown_transport_capability_bits_are_ignored() {
        assert_eq!(
            TransportCapabilities::from_flags(0xf0),
            TransportCapabilities::default()
        );
    }
}
