use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tp_core::p2p_types::{Candidate, CertFingerprint, SessionId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedPeer {
    pub peer_client_id: String,
    pub cert_fp: CertFingerprint,
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedPeerMatch {
    pub session_id: SessionId,
    pub peer: ExpectedPeer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpectedPeerMatchError {
    Ambiguous { count: usize },
}

#[derive(Clone, Debug, Default)]
pub struct ExpectedPeerMap {
    inner: Arc<Mutex<HashMap<SessionId, ExpectedPeer>>>,
}

impl ExpectedPeerMap {
    pub fn insert(&self, session_id: SessionId, peer: ExpectedPeer) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id, peer);
    }

    pub fn update(&self, session_id: SessionId, peer: ExpectedPeer) {
        self.insert(session_id, peer);
    }

    pub fn get(&self, session_id: SessionId) -> Option<ExpectedPeer> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&session_id)
            .cloned()
    }

    pub fn remove(&self, session_id: SessionId) -> Option<ExpectedPeer> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&session_id)
    }

    pub fn match_unique_by_cert_fp(
        &self,
        cert_fp: CertFingerprint,
    ) -> Result<Option<ExpectedPeerMatch>, ExpectedPeerMatchError> {
        let matches: Vec<ExpectedPeerMatch> = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .filter(|(_, peer)| peer.cert_fp == cert_fp)
            .map(|(session_id, peer)| ExpectedPeerMatch {
                session_id: *session_id,
                peer: peer.clone(),
            })
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            count => Err(ExpectedPeerMatchError::Ambiguous { count }),
        }
    }

    pub fn take_unique_by_cert_fp(
        &self,
        cert_fp: CertFingerprint,
    ) -> Result<Option<ExpectedPeerMatch>, ExpectedPeerMatchError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let matches: Vec<SessionId> = guard
            .iter()
            .filter_map(|(session_id, peer)| (peer.cert_fp == cert_fp).then_some(*session_id))
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => {
                let session_id = matches[0];
                Ok(guard
                    .remove(&session_id)
                    .map(|peer| ExpectedPeerMatch { session_id, peer }))
            }
            count => Err(ExpectedPeerMatchError::Ambiguous { count }),
        }
    }

    pub fn take_unique_by_cert_fp_for_session(
        &self,
        cert_fp: CertFingerprint,
        expected_session_id: SessionId,
    ) -> Result<Option<ExpectedPeerMatch>, ExpectedPeerMatchError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let matches: Vec<SessionId> = guard
            .iter()
            .filter_map(|(session_id, peer)| (peer.cert_fp == cert_fp).then_some(*session_id))
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 if matches[0] == expected_session_id => {
                Ok(guard
                    .remove(&expected_session_id)
                    .map(|peer| ExpectedPeerMatch {
                        session_id: expected_session_id,
                        peer,
                    }))
            }
            1 => Ok(None),
            count => Err(ExpectedPeerMatchError::Ambiguous { count }),
        }
    }

    pub fn take_by_session_and_cert_fp(
        &self,
        session_id: SessionId,
        cert_fp: CertFingerprint,
    ) -> Option<ExpectedPeerMatch> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let matches = guard
            .get(&session_id)
            .map(|peer| peer.cert_fp == cert_fp)
            .unwrap_or(false);
        if !matches {
            return None;
        }
        guard
            .remove(&session_id)
            .map(|peer| ExpectedPeerMatch { session_id, peer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_core::p2p_types::CandidateKind;

    fn candidate(port: u16) -> Candidate {
        Candidate {
            ip: "127.0.0.1".into(),
            port,
            kind: CandidateKind::Host,
        }
    }

    #[test]
    fn match_by_cert_fp_returns_unique_session_even_when_inserted_later() {
        let map = ExpectedPeerMap::default();
        let sid_a = SessionId::from_bytes([1u8; 16]);
        let sid_b = SessionId::from_bytes([2u8; 16]);
        let fp_a = CertFingerprint::from_bytes([10u8; 32]);
        let fp_b = CertFingerprint::from_bytes([11u8; 32]);

        map.insert(
            sid_b,
            ExpectedPeer {
                peer_client_id: "peer-b".into(),
                cert_fp: fp_b,
                candidates: vec![candidate(2000)],
            },
        );
        map.insert(
            sid_a,
            ExpectedPeer {
                peer_client_id: "peer-a".into(),
                cert_fp: fp_a,
                candidates: vec![candidate(1000)],
            },
        );

        let matched = map
            .match_unique_by_cert_fp(fp_a)
            .expect("unique lookup should not be ambiguous")
            .expect("fp_a should match");
        assert_eq!(matched.session_id, sid_a);
        assert_eq!(matched.peer.peer_client_id, "peer-a");
    }

    #[test]
    fn match_by_cert_fp_reports_ambiguous_same_fingerprint() {
        let map = ExpectedPeerMap::default();
        let fp = CertFingerprint::from_bytes([7u8; 32]);
        for (sid_byte, peer_client_id) in [(1u8, "peer-a"), (2u8, "peer-b")] {
            map.insert(
                SessionId::from_bytes([sid_byte; 16]),
                ExpectedPeer {
                    peer_client_id: peer_client_id.into(),
                    cert_fp: fp,
                    candidates: vec![candidate(sid_byte as u16)],
                },
            );
        }

        assert_eq!(
            map.match_unique_by_cert_fp(fp),
            Err(ExpectedPeerMatchError::Ambiguous { count: 2 })
        );
    }

    #[test]
    fn take_by_cert_fp_consumes_unique_session() {
        let map = ExpectedPeerMap::default();
        let sid = SessionId::from_bytes([3u8; 16]);
        let fp = CertFingerprint::from_bytes([9u8; 32]);
        map.insert(
            sid,
            ExpectedPeer {
                peer_client_id: "peer-a".into(),
                cert_fp: fp,
                candidates: vec![candidate(3000)],
            },
        );

        let matched = map
            .take_unique_by_cert_fp(fp)
            .expect("unique lookup should not be ambiguous")
            .expect("fp should match");

        assert_eq!(matched.session_id, sid);
        assert!(map.get(sid).is_none(), "matched session must be consumed");
        assert_eq!(
            map.match_unique_by_cert_fp(fp),
            Ok(None),
            "consumed cert fp must not match again"
        );
    }

    #[test]
    fn take_by_cert_fp_keeps_entries_on_ambiguous_match() {
        let map = ExpectedPeerMap::default();
        let fp = CertFingerprint::from_bytes([8u8; 32]);
        let sid_a = SessionId::from_bytes([4u8; 16]);
        let sid_b = SessionId::from_bytes([5u8; 16]);
        for (sid, peer_client_id) in [(sid_a, "peer-a"), (sid_b, "peer-b")] {
            map.insert(
                sid,
                ExpectedPeer {
                    peer_client_id: peer_client_id.into(),
                    cert_fp: fp,
                    candidates: vec![candidate(4000)],
                },
            );
        }

        assert_eq!(
            map.take_unique_by_cert_fp(fp),
            Err(ExpectedPeerMatchError::Ambiguous { count: 2 })
        );
        assert!(map.get(sid_a).is_some());
        assert!(map.get(sid_b).is_some());
    }

    #[test]
    fn take_by_cert_fp_for_session_only_consumes_matching_session() {
        let map = ExpectedPeerMap::default();
        let fp = CertFingerprint::from_bytes([6u8; 32]);
        let sid_a = SessionId::from_bytes([6u8; 16]);
        let sid_b = SessionId::from_bytes([7u8; 16]);
        map.insert(
            sid_a,
            ExpectedPeer {
                peer_client_id: "peer-a".into(),
                cert_fp: fp,
                candidates: vec![candidate(6000)],
            },
        );

        assert_eq!(
            map.take_unique_by_cert_fp_for_session(fp, sid_b),
            Ok(None),
            "a stale post-accept lookup must not consume another session"
        );
        assert!(map.get(sid_a).is_some());

        let matched = map
            .take_unique_by_cert_fp_for_session(fp, sid_a)
            .expect("unique lookup should not be ambiguous")
            .expect("fp should match sid_a");
        assert_eq!(matched.session_id, sid_a);
        assert!(map.get(sid_a).is_none());
    }

    #[test]
    fn take_by_session_and_cert_fp_allows_same_fingerprint_concurrency() {
        let map = ExpectedPeerMap::default();
        let fp = CertFingerprint::from_bytes([12u8; 32]);
        let sid_a = SessionId::from_bytes([12u8; 16]);
        let sid_b = SessionId::from_bytes([13u8; 16]);
        for (sid, peer_client_id, port) in [(sid_a, "peer-a", 1200), (sid_b, "peer-b", 1300)] {
            map.insert(
                sid,
                ExpectedPeer {
                    peer_client_id: peer_client_id.into(),
                    cert_fp: fp,
                    candidates: vec![candidate(port)],
                },
            );
        }

        let matched = map
            .take_by_session_and_cert_fp(sid_b, fp)
            .expect("sid_b with matching fp should be consumed");

        assert_eq!(matched.session_id, sid_b);
        assert!(map.get(sid_a).is_some());
        assert!(map.get(sid_b).is_none());
    }
}
