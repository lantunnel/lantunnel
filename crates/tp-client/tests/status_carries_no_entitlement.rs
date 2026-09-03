//! What the Client reports about itself carries no plan and no quota.
//!
//! The product does not advertise or enforce a subscription tier, a LAN-route
//! ceiling or a relay allowance on the Client; do not reintroduce one. These
//! fields were
//! removed from the callers that acted on them and left on the wire, so every
//! desktop status payload — and every "Native status:" blob a phone user copies
//! into a support thread — still carried them.
use tp_client::status::ConnectionStatus;
use tp_client::TunnelConfig;

const FORBIDDEN: [&str; 3] = ["subscription_tier", "lan_route_limit", "relay_quota"];

#[test]
fn a_status_payload_names_no_plan_and_no_quota() {
    let json = serde_json::to_value(ConnectionStatus::default()).expect("serialize status");
    let object = json.as_object().expect("status is an object");

    for field in FORBIDDEN {
        assert!(
            !object.contains_key(field),
            "status still carries `{field}`, which the product does not have"
        );
    }
}

/// The config the Platform hands a Client is where these actually lived.
///
/// The first version of this test checked `ConnectionStatus`, which had already
/// been cleaned — so it passed while `TunnelConfig` carried all three, and the
/// round that added it reported the table gone for the fourth time.
#[test]
fn the_tunnel_config_names_no_plan_and_no_quota() {
    let json = serde_json::to_value(TunnelConfig::default()).expect("serialize tunnel config");
    let object = json.as_object().expect("tunnel config is an object");

    for field in FORBIDDEN {
        assert!(
            !object.contains_key(field),
            "the Tunnel config still carries `{field}`, which the product does not have"
        );
    }
}
