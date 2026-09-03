use lantunnel_client::client_settings_v2::{
    compile_client_settings_v2, compile_client_settings_v2_with_connected_lans, ClientSettingsV2,
};
use tp_client::access_policy::{
    ClientAccessPortV2, ClientAccessProtocolV2, ClientAccessRuleV2, ClientAccessTargetV2,
};

#[test]
fn default_v2_settings_open_to_the_tunnel_with_no_exports() {
    let settings = ClientSettingsV2::default();
    let compiled = compile_client_settings_v2(&settings).expect("compile safe defaults");

    // Membership in the Tunnel is the boundary. A Client nobody configured is
    // reachable by the Peers that were issued a profile for the same Tunnel,
    // and closes with an explicit rule. LAN Exports stay empty either way —
    // reaching this Client is not the same as reaching the network behind it.
    assert!(
        settings.client_access.allow.is_empty(),
        "an empty Allow list is what leaves a fresh Client open to its Tunnel"
    );
    assert!(!settings.client_access.is_closed());
    assert!(settings.client_access.allow.is_empty());
    assert!(settings.client_access.deny.is_empty());
    assert!(settings.exported_lans.is_empty());
    assert!(!settings.tunnel_first);
    assert!(compiled.local_runtime_record.lan_exports.is_empty());
}

#[test]
fn canonical_private_exports_compile_as_interface_unavailable() {
    let settings = ClientSettingsV2 {
        exported_lans: vec!["192.168.44.0/24".into(), "10.0.0.0/8".into()],
        ..ClientSettingsV2::default()
    };

    let compiled = compile_client_settings_v2(&settings).expect("compile canonical exports");
    let exports = compiled.local_runtime_record.lan_exports;

    assert_eq!(exports.len(), 2);
    assert!(exports.iter().all(|export| !export.ready));
    assert_eq!(exports[0].prefix.network.to_string(), "10.0.0.0");
    assert_eq!(exports[0].prefix.prefix_len, 8);
    assert_eq!(exports[1].prefix.network.to_string(), "192.168.44.0");
    assert_eq!(exports[1].prefix.prefix_len, 24);
}

#[test]
fn only_an_exact_connected_prefix_is_published() {
    let settings = ClientSettingsV2 {
        exported_lans: vec![
            "10.20.0.0/16".into(),
            "192.168.44.0/24".into(),
            "192.168.0.0/16".into(),
        ],
        auto_export_current_lan: false,
        ..ClientSettingsV2::default()
    };
    let connected = [
        tp_client::peer_runtime::LanExportPrefixV2::new("10.20.0.0".parse().unwrap(), 16).unwrap(),
        tp_client::peer_runtime::LanExportPrefixV2::new("192.168.44.0".parse().unwrap(), 25)
            .unwrap(),
    ];

    let compiled = compile_client_settings_v2_with_connected_lans(&settings, Some(&connected))
        .expect("compile canonical exports");

    assert_eq!(
        compiled
            .local_runtime_record
            .lan_exports
            .iter()
            .map(|export| (export.prefix, export.ready))
            .collect::<Vec<_>>(),
        vec![
            (connected[0], true),
            (
                tp_client::peer_runtime::LanExportPrefixV2::new(
                    "192.168.0.0".parse().unwrap(),
                    16,
                )
                .unwrap(),
                false,
            ),
            (
                tp_client::peer_runtime::LanExportPrefixV2::new(
                    "192.168.44.0".parse().unwrap(),
                    24,
                )
                .unwrap(),
                false,
            ),
        ]
    );

    let unavailable = compile_client_settings_v2_with_connected_lans(&settings, None)
        .expect("scan failure keeps last-known-good settings");
    assert!(unavailable
        .local_runtime_record
        .lan_exports
        .iter()
        .all(|export| !export.ready));
}

#[test]
fn invalid_or_duplicate_exports_reject_the_whole_v2_block() {
    for exported_lans in [
        vec!["192.168.44.1/24".into()],
        vec!["8.8.8.0/24".into()],
        vec!["192.168.0.0/15".into()],
        vec!["10.0.0.0/8".into(), "10.0.0.0/8".into()],
    ] {
        let settings = ClientSettingsV2 {
            exported_lans,
            ..ClientSettingsV2::default()
        };
        assert!(compile_client_settings_v2(&settings).is_err());
    }
}

#[test]
fn one_invalid_access_rule_rejects_the_whole_v2_block() {
    let mut settings = ClientSettingsV2::default();
    settings.client_access.allow.push(ClientAccessRuleV2 {
        target: ClientAccessTargetV2::ThisPeer,
        protocol: ClientAccessProtocolV2::Tcp,
        port: ClientAccessPortV2::Exact(0),
    });

    assert!(compile_client_settings_v2(&settings).is_err());
}

#[test]
fn exporting_the_current_lan_is_on_by_default_and_leaves_the_typed_list_alone() {
    let settings = ClientSettingsV2::default();
    assert!(
        settings.auto_export_current_lan,
        "a fresh Client shares the network it is on without anyone having to find this setting"
    );

    let attached =
        tp_client::peer_runtime::LanExportPrefixV2::new("192.168.44.0".parse().unwrap(), 24)
            .unwrap();
    let typed =
        tp_client::peer_runtime::LanExportPrefixV2::new("10.20.0.0".parse().unwrap(), 16).unwrap();
    let settings = ClientSettingsV2 {
        exported_lans: vec!["10.20.0.0/16".into()],
        ..settings
    };

    let compiled = compile_client_settings_v2_with_connected_lans(&settings, Some(&[attached]))
        .expect("compile with an automatic Export");
    assert_eq!(
        compiled
            .local_runtime_record
            .lan_exports
            .iter()
            .map(|export| (export.prefix, export.ready))
            .collect::<Vec<_>>(),
        vec![(typed, false), (attached, true)]
    );
    assert_eq!(
        settings.exported_lans,
        vec!["10.20.0.0/16".to_string()],
        "the automatic Export must never be written into the owner's own list"
    );
    assert!(compiled.local_export_config.auto_current_lan);
    assert_eq!(compiled.local_export_config.configured, vec![typed]);
}

#[test]
fn turning_the_current_lan_switch_off_publishes_only_the_typed_list() {
    let attached =
        tp_client::peer_runtime::LanExportPrefixV2::new("192.168.44.0".parse().unwrap(), 24)
            .unwrap();
    let settings = ClientSettingsV2 {
        exported_lans: vec!["10.20.0.0/16".into()],
        auto_export_current_lan: false,
        ..ClientSettingsV2::default()
    };

    let compiled = compile_client_settings_v2_with_connected_lans(&settings, Some(&[attached]))
        .expect("compile with the switch off");
    assert_eq!(
        compiled
            .local_runtime_record
            .lan_exports
            .iter()
            .map(|export| format!("{}/{}", export.prefix.network, export.prefix.prefix_len))
            .collect::<Vec<_>>(),
        vec!["10.20.0.0/16".to_string()]
    );
}

#[test]
fn an_unreadable_interface_list_exports_nothing_automatically() {
    let settings = ClientSettingsV2::default();

    let compiled = compile_client_settings_v2_with_connected_lans(&settings, None)
        .expect("scan failure is not a settings error");

    assert!(compiled.local_runtime_record.lan_exports.is_empty());
}
