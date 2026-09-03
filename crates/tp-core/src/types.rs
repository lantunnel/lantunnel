//! Lightweight shared types.

/// Fixed-size connection identifier on the wire (12 bytes, zero-padded).
pub const CONN_ID_SIZE: usize = 12;

/// Connection identifier. String in Rust; truncated/padded to [`CONN_ID_SIZE`] on the wire.
pub type ConnId = String;

/// Tunnel group identifier (UUID v4 in practice).
pub type GroupId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            _ => None,
        }
    }
}
