//! The Allow list is the gate, and the gate is only closed once something is
//! standing in it.
//!
//! The old model asked the owner two questions — a default action and two rule
//! lists — to answer one: who may reach this Client. Now the Allow list answers
//! it alone: empty means anyone in the Tunnel, non-empty means only what it
//! names. Deny is checked first either way, so a refusal is never overridden.
use std::net::SocketAddr;

use tp_client::access_policy::{
    ClientAccessDecisionV2, ClientAccessPolicyV2, ClientAccessPortV2, ClientAccessProtocolV2,
    ClientAccessRuleV2, ClientAccessTargetClassV2, ClientAccessTargetV2,
    CompiledClientAccessPolicyV2,
};
use tp_core::Protocol;

fn rule(
    target: ClientAccessTargetV2,
    protocol: ClientAccessProtocolV2,
    port: ClientAccessPortV2,
) -> ClientAccessRuleV2 {
    ClientAccessRuleV2 {
        target,
        protocol,
        port,
    }
}

fn other(host: &str) -> ClientAccessTargetClassV2<'_> {
    ClientAccessTargetClassV2::Other {
        requested_host: host,
    }
}

fn addr(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::from((ip, port))
}

fn policy(allow: Vec<ClientAccessRuleV2>, deny: Vec<ClientAccessRuleV2>) -> ClientAccessPolicyV2 {
    ClientAccessPolicyV2 { allow, deny }
}

#[test]
fn an_empty_allow_list_lets_the_tunnel_through() {
    let compiled = CompiledClientAccessPolicyV2::compile(&policy(vec![], vec![])).expect("compile");

    assert_eq!(
        compiled.decide(other("10.0.0.2"), Protocol::Tcp, addr([10, 0, 0, 2], 22)),
        ClientAccessDecisionV2::AllowDirect
    );
}

#[test]
fn one_allow_rule_turns_the_list_into_the_only_way_in() {
    let compiled = CompiledClientAccessPolicyV2::compile(&policy(
        vec![rule(
            ClientAccessTargetV2::Cidr("10.0.0.0/8".into()),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Exact(22),
        )],
        vec![],
    ))
    .expect("compile");

    assert_eq!(
        compiled.decide(other("10.0.0.2"), Protocol::Tcp, addr([10, 0, 0, 2], 22)),
        ClientAccessDecisionV2::AllowDirect
    );
    assert_eq!(
        compiled.decide(other("10.0.0.2"), Protocol::Tcp, addr([10, 0, 0, 2], 80)),
        ClientAccessDecisionV2::Deny,
        "a port the Allow list does not name is refused once the list exists"
    );
}

#[test]
fn deny_is_checked_before_allow() {
    let compiled = CompiledClientAccessPolicyV2::compile(&policy(
        vec![rule(
            ClientAccessTargetV2::Cidr("10.0.0.0/8".into()),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Any,
        )],
        vec![rule(
            ClientAccessTargetV2::Ip(std::net::IpAddr::from([10, 0, 0, 9])),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Any,
        )],
    ))
    .expect("compile");

    assert_eq!(
        compiled.decide(other("10.0.0.9"), Protocol::Tcp, addr([10, 0, 0, 9], 22)),
        ClientAccessDecisionV2::Deny
    );
}

#[test]
fn deny_alone_closes_only_what_it_names() {
    let compiled = CompiledClientAccessPolicyV2::compile(&policy(
        vec![],
        vec![rule(
            ClientAccessTargetV2::Ip(std::net::IpAddr::from([10, 0, 0, 9])),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Any,
        )],
    ))
    .expect("compile");

    assert_eq!(
        compiled.decide(other("10.0.0.9"), Protocol::Tcp, addr([10, 0, 0, 9], 22)),
        ClientAccessDecisionV2::Deny
    );
    assert_eq!(
        compiled.decide(other("10.0.0.8"), Protocol::Tcp, addr([10, 0, 0, 8], 22)),
        ClientAccessDecisionV2::AllowDirect,
        "everything the Deny list does not name stays open"
    );
}

#[test]
fn a_local_service_mapping_does_not_close_the_door_behind_it() {
    // A ThisPeer rule is a capability, not a gate: it is the only way to expose
    // a local port, and it must stay possible to expose one without turning the
    // Allow list into a whitelist for every other destination.
    let compiled = CompiledClientAccessPolicyV2::compile(&policy(
        vec![ClientAccessRuleV2 {
            target: ClientAccessTargetV2::ThisPeer,
            protocol: ClientAccessProtocolV2::Tcp,
            port: ClientAccessPortV2::Exact(8080),
        }],
        vec![],
    ))
    .expect("compile");

    assert_eq!(
        compiled.decide(other("10.0.0.2"), Protocol::Tcp, addr([10, 0, 0, 2], 22)),
        ClientAccessDecisionV2::AllowDirect,
        "adding a local mapping must not silently restrict everything else"
    );
}

#[test]
fn a_local_port_is_reachable_only_when_a_mapping_names_it() {
    let own = [198, 18, 0, 5];
    let this_peer = ClientAccessTargetClassV2::ThisPeer {
        own_overlay: own.into(),
    };
    let compiled = CompiledClientAccessPolicyV2::compile(&policy(
        vec![ClientAccessRuleV2 {
            target: ClientAccessTargetV2::ThisPeer,
            protocol: ClientAccessProtocolV2::Tcp,
            port: ClientAccessPortV2::Exact(8080),
        }],
        vec![],
    ))
    .expect("compile");

    assert_eq!(
        compiled.decide(this_peer, Protocol::Tcp, addr(own, 8080)),
        ClientAccessDecisionV2::AllowThisPeer {
            final_target: "127.0.0.1:8080".into()
        }
    );
    assert_eq!(
        compiled.decide(this_peer, Protocol::Tcp, addr(own, 9090)),
        ClientAccessDecisionV2::Deny,
        "an open Allow list still never exposes a local port on its own"
    );
}

#[test]
fn a_closed_policy_refuses_every_address_and_protocol() {
    let closed = ClientAccessPolicyV2::closed();
    assert!(closed.is_closed());

    let compiled = CompiledClientAccessPolicyV2::compile(&closed).expect("compile");
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        assert_eq!(
            compiled.decide(other("10.0.0.2"), protocol, addr([10, 0, 0, 2], 22)),
            ClientAccessDecisionV2::Deny
        );
    }
}

#[test]
fn an_open_policy_does_not_claim_to_be_closed() {
    assert!(!policy(vec![], vec![]).is_closed());
}
