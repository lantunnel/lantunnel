//! "Nothing is reachable" has to be true of IPv6 too.
//!
//! `is_closed` accepted either family per protocol, so a policy that denies
//! only the IPv4 catch-all reported closed — the Client UI switched "Block
//! everything" on and printed "Nothing is reachable through this device" —
//! while an empty Allow list left every IPv6 destination open.
use tp_client::access_policy::{
    ClientAccessPolicyV2, ClientAccessPortV2, ClientAccessProtocolV2, ClientAccessRuleV2,
    ClientAccessTargetV2,
};

fn catch_all(cidr: &str, protocol: ClientAccessProtocolV2) -> ClientAccessRuleV2 {
    ClientAccessRuleV2 {
        target: ClientAccessTargetV2::Cidr(cidr.into()),
        protocol,
        port: ClientAccessPortV2::Any,
    }
}

#[test]
fn denying_only_ipv4_is_not_closed() {
    let policy = ClientAccessPolicyV2 {
        allow: Vec::new(),
        deny: vec![
            catch_all("0.0.0.0/0", ClientAccessProtocolV2::Tcp),
            catch_all("0.0.0.0/0", ClientAccessProtocolV2::Udp),
        ],
    };

    assert!(
        !policy.is_closed(),
        "IPv6 destinations are still open, so this is not closed"
    );
}

#[test]
fn denying_both_families_is_closed() {
    assert!(ClientAccessPolicyV2::closed().is_closed());
}
