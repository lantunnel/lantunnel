//! Core types shared by P2P signaling and rendezvous code.

use rand::RngCore;

pub const SESSION_ID_SIZE: usize = 16;
pub const CERT_FP_SIZE: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId([u8; SESSION_ID_SIZE]);

impl SessionId {
    pub fn new_random() -> Self {
        let mut buf = [0u8; SESSION_ID_SIZE];
        rand::thread_rng().fill_bytes(&mut buf);
        Self(buf)
    }
    pub fn from_bytes(bytes: [u8; SESSION_ID_SIZE]) -> Self {
        Self(bytes)
    }
    pub fn as_bytes(&self) -> &[u8; SESSION_ID_SIZE] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NatHint {
    Unknown = 0,
    FullCone = 1,
    Restricted = 2,
    PortRestricted = 3,
    Symmetric = 4,
}

impl NatHint {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Unknown,
            1 => Self::FullCone,
            2 => Self::Restricted,
            3 => Self::PortRestricted,
            4 => Self::Symmetric,
            _ => return None,
        })
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CandidateKind {
    Host = 1,
    ServerReflexive = 2,
}

impl CandidateKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Host,
            2 => Self::ServerReflexive,
            _ => return None,
        })
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub ip: String,
    pub port: u16,
    pub kind: CandidateKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum P2pRole {
    Initiator = 1,
    Acceptor = 2,
}

impl P2pRole {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Initiator,
            2 => Self::Acceptor,
            _ => return None,
        })
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TeardownReason {
    Idle = 1,
    HealthFail = 2,
    User = 3,
    FatalError = 4,
}

impl TeardownReason {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Idle,
            2 => Self::HealthFail,
            3 => Self::User,
            4 => Self::FatalError,
            _ => return None,
        })
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CertFingerprint([u8; CERT_FP_SIZE]);

impl CertFingerprint {
    pub fn from_bytes(bytes: [u8; CERT_FP_SIZE]) -> Self {
        Self(bytes)
    }
    pub fn zero() -> Self {
        Self([0u8; CERT_FP_SIZE])
    }
    pub fn as_bytes(&self) -> &[u8; CERT_FP_SIZE] {
        &self.0
    }
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_random_unique() {
        let a = SessionId::new_random();
        let b = SessionId::new_random();
        assert_ne!(a.as_bytes(), b.as_bytes());
        assert_eq!(a.as_bytes().len(), 16);
    }

    #[test]
    fn nat_hint_from_u8_round_trip() {
        for v in [0u8, 1, 2, 3, 4] {
            let h = NatHint::from_u8(v).expect("known");
            assert_eq!(h.as_u8(), v);
        }
        assert!(NatHint::from_u8(99).is_none());
    }

    #[test]
    fn cert_fingerprint_zero_default() {
        let fp = CertFingerprint::zero();
        assert_eq!(fp.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn p2p_role_round_trip() {
        for v in [1u8, 2] {
            let r = P2pRole::from_u8(v).expect("known");
            assert_eq!(r.as_u8(), v);
        }
        assert!(P2pRole::from_u8(0).is_none());
        assert!(P2pRole::from_u8(3).is_none());
    }

    #[test]
    fn teardown_reason_round_trip() {
        for v in [1u8, 2, 3, 4] {
            let r = TeardownReason::from_u8(v).expect("known");
            assert_eq!(r.as_u8(), v);
        }
        assert!(TeardownReason::from_u8(0).is_none());
        assert!(TeardownReason::from_u8(5).is_none());
    }
}
