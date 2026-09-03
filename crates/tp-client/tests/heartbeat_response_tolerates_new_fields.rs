//! The Platform may add to the heartbeat answer without breaking every Client
//! already installed.
//!
//! The response was `deny_unknown_fields`, so the first field the Platform
//! added would have made every heartbeat fail to parse — on Clients already in
//! the field, which cannot be updated in step with the Platform.
use tp_client::peer_heartbeat::PeerHeartbeatResponse;

#[test]
fn a_field_this_client_does_not_know_is_ignored() {
    let json = r#"{
        "accepted_timestamp_ms": 1,
        "server_time": "2026-08-25T00:00:00.000Z",
        "something_added_later": {"nested": true}
    }"#;

    let parsed: PeerHeartbeatResponse = serde_json::from_str(json).expect("parses");

    assert_eq!(parsed.accepted_timestamp_ms, 1);
}

#[test]
fn the_relay_allowance_is_read_when_it_is_there() {
    let json = r#"{
        "accepted_timestamp_ms": 1,
        "server_time": "2026-08-25T00:00:00.000Z",
        "relay_usage": {"used_bytes": 1024, "allowance_bytes": 5368709120}
    }"#;

    let parsed: PeerHeartbeatResponse = serde_json::from_str(json).expect("parses");
    let usage = parsed.relay_usage.expect("usage is present");

    assert_eq!(usage.used_bytes, 1024);
    assert_eq!(usage.allowance_bytes, 5_368_709_120);
}

/// An older Platform does not send it, and that is not an error.
#[test]
fn a_platform_that_does_not_report_usage_still_parses() {
    let json = r#"{"accepted_timestamp_ms": 1, "server_time": "2026-08-25T00:00:00.000Z"}"#;

    let parsed: PeerHeartbeatResponse = serde_json::from_str(json).expect("parses");

    assert!(parsed.relay_usage.is_none());
}
