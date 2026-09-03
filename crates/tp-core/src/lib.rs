//! Shared core for Lantunnel.
//!
//! Contains the binary wire protocol, per-Tunnel bandwidth limiting, and the
//! YAML configuration schema shared by the gateway, client CLI, and client GUI.

pub mod atomic_file;
pub mod bandwidth;
pub mod config;
pub mod log;
pub mod p2p_codec;
pub mod p2p_types;
pub mod peer_link_crypto;
pub mod protocol;
pub mod provisioning;
pub mod types;

pub use protocol::{BinaryMessage, MsgType, ProtoError, PROTOCOL_VERSION};
pub use types::{ConnId, GroupId, Protocol, CONN_ID_SIZE};
