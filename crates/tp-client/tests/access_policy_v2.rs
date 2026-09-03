use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

fn other<'a>(host: &'a str) -> ClientAccessTargetClassV2<'a> {
    ClientAccessTargetClassV2::Other {
        requested_host: host,
    }
}

fn addr(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::from((ip, port))
}

#[test]
fn missing_or_invalid_first_policy_can_use_explicit_deny_all() {
    let policy = CompiledClientAccessPolicyV2::deny_all();
    assert_eq!(
        policy.decide(other("10.0.0.2"), Protocol::Tcp, addr([10, 0, 0, 2], 22)),
        ClientAccessDecisionV2::Deny
    );
}

#[test]
fn default_deny_requires_allow_and_deny_always_wins() {
    let policy = CompiledClientAccessPolicyV2::compile(&ClientAccessPolicyV2 {
        allow: vec![rule(
            ClientAccessTargetV2::Cidr("10.0.0.0/8".into()),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Exact(22),
        )],
        deny: vec![rule(
            ClientAccessTargetV2::Ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7))),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Exact(22),
        )],
    })
    .expect("compile");

    assert_eq!(
        policy.decide(other("10.0.0.6"), Protocol::Tcp, addr([10, 0, 0, 6], 22)),
        ClientAccessDecisionV2::AllowDirect
    );
    assert_eq!(
        policy.decide(other("10.0.0.7"), Protocol::Tcp, addr([10, 0, 0, 7], 22)),
        ClientAccessDecisionV2::Deny
    );
    assert_eq!(
        policy.decide(other("10.0.0.6"), Protocol::Udp, addr([10, 0, 0, 6], 22)),
        ClientAccessDecisionV2::Deny
    );
}

#[test]
fn default_allow_is_blacklist_mode_but_does_not_create_this_peer_mapping() {
    let policy = CompiledClientAccessPolicyV2::compile(&ClientAccessPolicyV2 {
        allow: vec![],
        deny: vec![rule(
            ClientAccessTargetV2::Host("*.blocked.example".into()),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Any,
        )],
    })
    .expect("compile");

    assert_eq!(
        policy.decide(
            other("game.example"),
            Protocol::Udp,
            addr([10, 1, 2, 3], 27015)
        ),
        ClientAccessDecisionV2::AllowDirect
    );
    assert_eq!(
        policy.decide(
            other("api.blocked.example"),
            Protocol::Tcp,
            addr([10, 1, 2, 3], 443)
        ),
        ClientAccessDecisionV2::Deny
    );
    let overlay = Ipv4Addr::new(198, 18, 0, 9);
    assert_eq!(
        policy.decide(
            ClientAccessTargetClassV2::ThisPeer {
                own_overlay: overlay,
            },
            Protocol::Tcp,
            SocketAddr::new(overlay.into(), 22),
        ),
        ClientAccessDecisionV2::Deny
    );
}

#[test]
fn this_peer_allow_binds_the_requested_port_on_loopback() {
    let overlay = Ipv4Addr::new(198, 18, 0, 9);
    let explicit = rule(
        ClientAccessTargetV2::ThisPeer,
        ClientAccessProtocolV2::Tcp,
        ClientAccessPortV2::Exact(2222),
    );
    let policy = CompiledClientAccessPolicyV2::compile(&ClientAccessPolicyV2 {
        allow: vec![
            rule(
                ClientAccessTargetV2::ThisPeer,
                ClientAccessProtocolV2::Udp,
                ClientAccessPortV2::Exact(27015),
            ),
            explicit,
        ],
        deny: vec![],
    })
    .expect("compile");
    let class = ClientAccessTargetClassV2::ThisPeer {
        own_overlay: overlay,
    };

    assert_eq!(
        policy.decide(class, Protocol::Udp, SocketAddr::new(overlay.into(), 27015)),
        ClientAccessDecisionV2::AllowThisPeer {
            final_target: "127.0.0.1:27015".into(),
        }
    );
    assert_eq!(
        policy.decide(class, Protocol::Tcp, SocketAddr::new(overlay.into(), 2222)),
        ClientAccessDecisionV2::AllowThisPeer {
            final_target: "127.0.0.1:2222".into(),
        }
    );
    assert!(policy.mapped_final_allowed(Protocol::Tcp, "127.0.0.1", addr([127, 0, 0, 1], 2222),));
    // Reaching a mapping's final target directly is refused before the policy
    // is consulted: the engine requires the requested address to sit inside a
    // ready LAN Export, and a loopback prefix can never be exported
    // (peer_runtime rejects it). The policy layer is therefore not where this
    // is enforced, and with an open Allow list it does not pretend to be.
    assert_eq!(
        policy.decide(other("127.0.0.1"), Protocol::Tcp, addr([127, 0, 0, 1], 22)),
        ClientAccessDecisionV2::AllowDirect
    );
}

#[test]
fn this_peer_allow_still_applies_deny_to_the_bound_final_target() {
    let overlay = Ipv4Addr::new(198, 18, 0, 9);
    let mapping = rule(
        ClientAccessTargetV2::ThisPeer,
        ClientAccessProtocolV2::Tcp,
        ClientAccessPortV2::Exact(2222),
    );
    let policy = CompiledClientAccessPolicyV2::compile(&ClientAccessPolicyV2 {
        allow: vec![mapping],
        deny: vec![rule(
            ClientAccessTargetV2::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Exact(2222),
        )],
    })
    .expect("compile");

    assert_eq!(
        policy.decide(
            ClientAccessTargetClassV2::ThisPeer {
                own_overlay: overlay,
            },
            Protocol::Tcp,
            SocketAddr::new(overlay.into(), 2222),
        ),
        ClientAccessDecisionV2::AllowThisPeer {
            final_target: "127.0.0.1:2222".into(),
        }
    );
    assert!(!policy.mapped_final_allowed(Protocol::Tcp, "127.0.0.1", addr([127, 0, 0, 1], 2222),));
}

#[test]
fn hostname_allow_and_resolved_ip_deny_are_one_decision() {
    let policy = CompiledClientAccessPolicyV2::compile(&ClientAccessPolicyV2 {
        allow: vec![rule(
            ClientAccessTargetV2::Host("*.games.lan".into()),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Exact(27015),
        )],
        deny: vec![rule(
            ClientAccessTargetV2::Cidr("192.168.50.128/25".into()),
            ClientAccessProtocolV2::Tcp,
            ClientAccessPortV2::Exact(27015),
        )],
    })
    .expect("compile");

    assert_eq!(
        policy.decide(
            other("server.games.lan"),
            Protocol::Tcp,
            addr([192, 168, 50, 20], 27015),
        ),
        ClientAccessDecisionV2::AllowDirect,
    );
    assert_eq!(
        policy.decide(
            other("server.games.lan"),
            Protocol::Tcp,
            addr([192, 168, 50, 200], 27015),
        ),
        ClientAccessDecisionV2::Deny,
    );
}

#[test]
fn regex_like_hosts_are_rejected() {
    for target in [
        ClientAccessTargetV2::Host("^example\\.com$".into()),
        ClientAccessTargetV2::Host("foo.*.example".into()),
        ClientAccessTargetV2::Cidr("10.0.0.0/33".into()),
    ] {
        assert!(
            CompiledClientAccessPolicyV2::compile(&ClientAccessPolicyV2 {
                allow: vec![rule(
                    target,
                    ClientAccessProtocolV2::Tcp,
                    ClientAccessPortV2::Any,
                )],
                deny: vec![],
            })
            .is_err()
        );
    }
}

#[test]
fn yaml_schema_has_no_source_peer_or_rule_order_semantics() {
    let yaml = r#"
allow:
  - target: { type: cidr, value: 192.168.1.0/24 }
    protocol: tcp
    port: { type: exact, value: 27015 }
deny: []
"#;
    let policy: ClientAccessPolicyV2 = serde_yaml::from_str(yaml).expect("parse minimal policy");
    CompiledClientAccessPolicyV2::compile(&policy).expect("compile minimal policy");

    let source_peer = yaml.replace("protocol: tcp", "source_peer: peer-a\n    protocol: tcp");
    assert!(serde_yaml::from_str::<ClientAccessPolicyV2>(&source_peer).is_err());
}
