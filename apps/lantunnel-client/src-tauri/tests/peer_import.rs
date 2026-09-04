use std::fs;

use lantunnel_client::peer_store::{
    import_peer_profile, list_peer_profiles, load_peer_profile, PeerBootstrapKindV2,
    PeerImportError,
};
use tp_core::provisioning::{
    GatewayBootstrapV2, PeerBootstrapV2, PeerProfileV2, TunnelOwnerFileV2,
};
use uuid::Uuid;

fn valid_peer_profile() -> PeerProfileV2 {
    let gateway = GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example.com".into(),
        port: 443,
        mapping_port: None,
        tls_server_name: Some("gateway.example.com".into()),
        trusted_certificate_pem: None,
    };
    let mut tunnel = TunnelOwnerFileV2::generate(gateway).expect("generate Tunnel owner file");
    tunnel
        .add_peer(None, 1, Some("gaming-pc".into()))
        .expect("generate Peer profile")
}

#[test]
fn imported_peer_profiles_are_listed_as_sorted_public_summaries() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-list-{}", Uuid::new_v4()));
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profiles = [valid_peer_profile(), valid_peer_profile()];
    for (index, profile) in profiles.iter().enumerate().rev() {
        let source = root.join(format!("incoming-{index}.peer"));
        fs::write(
            &source,
            serde_yaml::to_string(profile).expect("serialize Peer profile"),
        )
        .expect("write Peer profile");
        import_peer_profile(&source, &config_root).expect("import Peer profile");
    }

    let summaries = list_peer_profiles(&config_root).expect("list imported Peer profiles");
    let mut expected_tunnel_ids: Vec<_> = profiles
        .iter()
        .map(|profile| profile.tunnel_id.clone())
        .collect();
    expected_tunnel_ids.sort();

    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.tunnel_id.clone())
            .collect::<Vec<_>>(),
        expected_tunnel_ids
    );
    let public_json = serde_json::to_string(&summaries).expect("serialize public summaries");
    for profile in &profiles {
        assert!(!public_json.contains(profile.peer.peer_private_key.as_str()));
        assert!(!public_json.contains(&profile.peer.membership_signature));
    }

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn a_gateway_override_file_cannot_redirect_a_profile() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-loader-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write Peer profile");
    import_peer_profile(&source, &config_root).expect("import static Peer profile");

    let replacement = GatewayBootstrapV2 {
        transport: "websocket".into(),
        dial_address: "203.0.113.9".into(),
        port: 8443,
        mapping_port: None,
        tls_server_name: Some("friend-gateway.example.com".into()),
        trusted_certificate_pem: None,
    };
    let overrides = config_root.join("gateway-overrides");
    fs::create_dir_all(&overrides).expect("stale override dir");
    fs::write(
        overrides.join(format!("{}.yaml", profile.tunnel_id)),
        serde_yaml::to_string(&replacement).expect("serialize replacement Gateway facts"),
    )
    .expect("replace Gateway facts fixture");

    let loaded = load_peer_profile(&config_root, &profile.tunnel_id)
        .expect("load Peer with effective Gateway bootstrap");

    assert_eq!(loaded.profile().peer.peer_id, profile.peer.peer_id);
    // The Gateway is whatever the .peer file says. The membership signature
    // covers tunnel_id, peer_id, overlay_ip and peer_public_key — not the
    // Gateway facts — so honouring a file beside the profile pointed the
    // Client at a host of someone's choosing and, with the certificate field,
    // told it to trust that host. Importing a different .peer file is how the
    // Gateway changes.
    let _unused = replacement;
    assert!(
        loaded.effective_bootstrap() == &profile.bootstrap,
        "the Gateway must come from the .peer file"
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[cfg(unix)]
#[test]
fn valid_peer_profile_imports_as_private_canonical_copy() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-import-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write Peer profile");

    let summary = import_peer_profile(&source, &config_root).expect("import valid Peer profile");

    assert_eq!(summary.tunnel_id, profile.tunnel_id);
    assert_eq!(summary.peer_id, profile.peer.peer_id);
    assert_eq!(summary.overlay_ip, profile.peer.overlay_ip);
    assert_eq!(summary.bootstrap_kind, PeerBootstrapKindV2::StaticGateway);

    let stored_path = config_root
        .join("peers")
        .join(format!("{}.peer", profile.tunnel_id));
    let stored: PeerProfileV2 = serde_yaml::from_str(
        &fs::read_to_string(&stored_path).expect("read imported Peer profile"),
    )
    .expect("parse imported Peer profile");
    assert!(stored == profile);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(config_root.join("peers"))
                .expect("peers directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(stored_path)
                .expect("Peer profile metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn tampered_peer_profile_is_rejected_before_storage() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-tamper-{}", Uuid::new_v4()));
    let source = root.join("tampered.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let mut profile = valid_peer_profile();
    profile.peer.overlay_ip = "198.18.200.10".parse().expect("valid Overlay IP");
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize tampered Peer profile"),
    )
    .expect("write tampered Peer profile");

    let error = import_peer_profile(&source, &config_root)
        .expect_err("tampered Tunnel membership must be rejected");

    assert!(matches!(error, PeerImportError::InvalidProvisioning(_)));
    assert!(!config_root.join("peers").exists());

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn mismatched_peer_private_key_is_rejected_before_storage() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-key-mismatch-{}", Uuid::new_v4()));
    let source = root.join("mismatched.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let mut profile = valid_peer_profile();
    profile.peer.peer_private_key = valid_peer_profile().peer.peer_private_key;
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize mismatched Peer profile"),
    )
    .expect("write mismatched Peer profile");

    let error = import_peer_profile(&source, &config_root)
        .expect_err("mismatched Peer keys must be rejected");

    assert!(matches!(error, PeerImportError::InvalidProvisioning(_)));
    assert!(!config_root.join("peers").exists());

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn importing_a_tunnel_again_replaces_what_was_stored() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-overwrite-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write Peer profile");
    import_peer_profile(&source, &config_root).expect("first import should succeed");
    let stored_path = config_root
        .join("peers")
        .join(format!("{}.peer", profile.tunnel_id));
    let before = fs::read(&stored_path).expect("read first imported profile");

    // Replacing is the point: the file names the Tunnel, so importing it again
    // is the owner saying this is the profile for that Tunnel now. Refusing
    // meant a Tunnel could be joined once and never re-joined.
    import_peer_profile(&source, &config_root).expect("a second import replaces the first");

    assert_eq!(
        fs::read(&stored_path).expect("read replaced imported profile"),
        before,
        "the same file in gives the same file stored",
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[cfg(unix)]
#[test]
fn symlink_peer_source_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("lantunnel-peer-symlink-{}", Uuid::new_v4()));
    let real_source = root.join("real.peer");
    let source_link = root.join("linked.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(
        &real_source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write real Peer profile");
    symlink(&real_source, &source_link).expect("create source symlink");

    let error = import_peer_profile(&source_link, &config_root)
        .expect_err("a symlink source must be rejected");

    assert!(matches!(error, PeerImportError::UnsafeSource(_)));
    assert!(!config_root.exists());

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn tunnel_import_cli_returns_only_public_peer_summary() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-cli-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write Peer profile");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lantunnel-client"))
        .env("TUNNEL_PROXY_APP_CONFIG_DIR", &config_root)
        .args(["tunnel", "import"])
        .arg(&source)
        .output()
        .expect("run Peer import CLI");

    assert!(
        output.status.success(),
        "Peer import CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output must be UTF-8");
    let summary: serde_json::Value = serde_json::from_str(&stdout).expect("public summary JSON");
    assert_eq!(summary["tunnel_id"], profile.tunnel_id);
    assert_eq!(summary["peer_id"], profile.peer.peer_id);
    assert_eq!(summary["bootstrap_kind"], "static_gateway");
    assert!(!stdout.contains(profile.peer.peer_private_key.as_str()));
    assert!(!stdout.contains(&profile.peer.peer_public_key));
    assert!(!stdout.contains(&profile.peer.membership_signature));

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn tunnel_list_cli_returns_only_public_peer_summaries() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-list-cli-{}", Uuid::new_v4()));
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profiles = [valid_peer_profile(), valid_peer_profile()];
    for (index, profile) in profiles.iter().enumerate() {
        let source = root.join(format!("incoming-{index}.peer"));
        fs::write(
            &source,
            serde_yaml::to_string(profile).expect("serialize Peer profile"),
        )
        .expect("write Peer profile");
        import_peer_profile(&source, &config_root).expect("import Peer profile");
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lantunnel-client"))
        .env("TUNNEL_PROXY_APP_CONFIG_DIR", &config_root)
        .args(["tunnel", "list"])
        .output()
        .expect("run Peer list CLI");

    assert!(
        output.status.success(),
        "Peer list CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output must be UTF-8");
    let summaries: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).expect("public summary list JSON");
    assert_eq!(summaries.len(), 2);
    for profile in &profiles {
        assert!(!stdout.contains(profile.peer.peer_private_key.as_str()));
        assert!(!stdout.contains(&profile.peer.membership_signature));
    }

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn headless_v2_connect_rejects_non_loopback_local_socks_before_gateway_dial() {
    let root = std::env::temp_dir().join(format!("lantunnel-peer-connect-cli-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");
    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write Peer profile");
    import_peer_profile(&source, &config_root).expect("import Peer profile");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lantunnel-client"))
        .env("TUNNEL_PROXY_APP_CONFIG_DIR", &config_root)
        .env("LANTUNNEL_LOCAL_SOCKS5_LISTEN", "0.0.0.0:1080")
        .args(["connect", &profile.tunnel_id])
        .output()
        .expect("run V2 headless connect");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("loopback"), "unexpected error: {stderr}");

    fs::remove_dir_all(root).expect("remove test root");
}

/// Re-importing the same Tunnel replaces what is stored.
///
/// A profile could be imported exactly once per Tunnel: the second attempt
/// failed with "already exists and was not overwritten", and nothing in the
/// Client could remove one. A reinstall, a second device, or a profile the
/// Platform issued again all hit the same wall, with no way past it.
#[test]
fn importing_the_same_tunnel_again_replaces_the_stored_profile() {
    let root = std::env::temp_dir().join(format!("lantunnel-reimport-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(&source, serde_yaml::to_string(&profile).expect("serialize")).expect("write profile");
    import_peer_profile(&source, &config_root).expect("first import");

    let again = import_peer_profile(&source, &config_root);

    assert!(
        again.is_ok(),
        "re-importing the same Tunnel must replace it: {again:?}"
    );
    assert_eq!(
        list_peer_profiles(&config_root).expect("list").len(),
        1,
        "and must not leave two copies behind",
    );

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn reimporting_a_peer_updates_its_mapping_port_without_changing_its_identity() {
    let root = std::env::temp_dir().join(format!("lantunnel-reimport-port-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let mut profile = valid_peer_profile();
    let peer_id = profile.peer.peer_id.clone();
    let membership_signature = profile.peer.membership_signature.clone();
    fs::write(&source, serde_yaml::to_string(&profile).expect("serialize")).expect("write profile");
    import_peer_profile(&source, &config_root).expect("first import");

    match &mut profile.bootstrap {
        PeerBootstrapV2::StaticGateway(gateway) => gateway.mapping_port = Some(10_444),
        PeerBootstrapV2::ManagedPlatform { .. } => panic!("expected static Gateway bootstrap"),
    }
    fs::write(&source, serde_yaml::to_string(&profile).expect("serialize")).expect("write update");
    import_peer_profile(&source, &config_root).expect("reimport updated profile");

    let loaded = load_peer_profile(&config_root, &profile.tunnel_id).expect("load updated profile");
    assert_eq!(loaded.profile().peer.peer_id, peer_id);
    assert_eq!(
        loaded.profile().peer.membership_signature,
        membership_signature
    );
    match loaded.effective_bootstrap() {
        PeerBootstrapV2::StaticGateway(gateway) => {
            assert_eq!(gateway.mapping_port, Some(10_444));
        }
        PeerBootstrapV2::ManagedPlatform { .. } => panic!("expected static Gateway bootstrap"),
    }

    fs::remove_dir_all(root).expect("remove test root");
}

/// An imported profile can be removed.
#[test]
fn a_stored_profile_can_be_forgotten() {
    let root = std::env::temp_dir().join(format!("lantunnel-forget-{}", Uuid::new_v4()));
    let source = root.join("incoming.peer");
    let config_root = root.join("client-config");
    fs::create_dir_all(&root).expect("create test root");

    let profile = valid_peer_profile();
    fs::write(&source, serde_yaml::to_string(&profile).expect("serialize")).expect("write profile");
    import_peer_profile(&source, &config_root).expect("import");

    lantunnel_client::peer_store::forget_peer_profile(&config_root, &profile.tunnel_id)
        .expect("forget the profile");

    assert!(list_peer_profiles(&config_root).expect("list").is_empty());

    fs::remove_dir_all(root).expect("remove test root");
}
