//! Local-IP candidate detection for P2pAnnounce.

use tp_core::p2p_types::{Candidate, CandidateKind};

pub fn detect_local_candidates(port: u16) -> Vec<Candidate> {
    let Ok(addrs) = if_addrs::get_if_addrs() else {
        return vec![];
    };
    let mut out = Vec::new();
    for ifa in addrs {
        if ifa.is_loopback() {
            continue;
        }
        let ip = ifa.ip();
        if ip.is_unspecified() {
            continue;
        }
        out.push(Candidate {
            ip: ip.to_string(),
            port,
            kind: CandidateKind::Host,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_local_candidates_returns_only_host_kind() {
        let cands = detect_local_candidates(0);
        assert!(cands.iter().all(|c| matches!(c.kind, CandidateKind::Host)));
    }

    #[test]
    fn detect_local_candidates_skips_loopback_and_unspecified() {
        let cands = detect_local_candidates(0);
        for c in &cands {
            assert!(!c.ip.starts_with("127."), "loopback {}", c.ip);
            assert!(c.ip != "0.0.0.0", "unspecified {}", c.ip);
            assert!(c.ip != "::", "unspecified v6 {}", c.ip);
        }
    }
}
