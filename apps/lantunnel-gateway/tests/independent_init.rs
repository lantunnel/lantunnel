use std::fs;
use std::process::{Command, Output};

fn owner_only_temporary_directory() -> tempfile::TempDir {
    let temporary_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
    let directory = tempfile::Builder::new()
        .prefix("lantunnel-independent-gateway-init-")
        .tempdir_in(temporary_root)
        .expect("temporary Gateway directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("owner-only Gateway directory");
    }
    directory
}

fn run_gateway(directory: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lantunnel-gateway"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run lantunnel-gateway")
}

fn path_text(path: &std::path::Path) -> &str {
    path.to_str().expect("test path is UTF-8")
}

#[test]
fn init_writes_a_ready_independent_gateway_without_platform_state() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = directory.path().join("configs/gateway.yaml");
    let certificate = directory.path().join("certs/server.crt");
    let private_key = directory.path().join("certs/server.key");
    let scopes = directory.path().join("state/scopes.d");
    for path in [&config, &certificate, &private_key] {
        assert!(path.is_file(), "missing generated file: {}", path.display());
    }
    assert!(scopes.is_dir(), "missing Scope directory");
    assert!(!directory
        .path()
        .join("configs/gateway.yaml.managed-identity.json")
        .exists());

    let yaml = fs::read_to_string(&config).expect("read generated config");
    let parsed = tp_core::config::load_from_str(&yaml).expect("parse generated config");
    let gateway = parsed.gateway.expect("generated Gateway config");
    assert_eq!(gateway.listen_addr, "0.0.0.0:8443");
    assert_eq!(gateway.transport_type, "quic");
    assert_eq!(gateway.tls_cert.as_deref(), Some(path_text(&certificate)));
    assert_eq!(gateway.tls_key.as_deref(), Some(path_text(&private_key)));
    assert_eq!(gateway.scopes_dir.as_deref(), Some(path_text(&scopes)));
    assert_eq!(gateway.mapping_probe_port, 8444);
    assert!(gateway.p2p.enabled);
    assert_eq!(
        gateway.usage_ledger.wal_path,
        path_text(&directory.path().join("state/relay-usage.wal"))
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [
            directory.path().join("configs"),
            directory.path().join("certs"),
            directory.path().join("state"),
            scopes.clone(),
        ] {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} must be owner-only",
                path.display()
            );
        }
        for path in [&config, &certificate, &private_key] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600,
                "{} must be owner-only",
                path.display()
            );
        }
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Independent Gateway: initialized"));
    assert!(stdout.contains("Open inbound: UDP 8443 (QUIC data) and UDP 8444 (mapping)"));
    assert!(stdout.contains("lantunnel-admin init-tunnel"));
    assert!(stdout.contains("--gateway-ip 1.1.1.1"));
    assert!(stdout.contains("--gateway-mapping-port 8444"));
    assert!(stdout.contains("--gateway-cert ./server.crt"));
    assert!(stdout.contains("Start the Gateway: lantunnel-gateway"));

    let check = run_gateway(directory.path(), &["--check-config"]);
    assert!(
        check.status.success(),
        "generated config failed validation: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let other_directory = owner_only_temporary_directory();
    let check_elsewhere = run_gateway(
        other_directory.path(),
        &["--config", path_text(&config), "--check-config"],
    );
    assert!(
        check_elsewhere.status.success(),
        "generated config depended on its startup cwd: {}",
        String::from_utf8_lossy(&check_elsewhere.stderr)
    );
    assert!(
        fs::read_dir(other_directory.path())
            .unwrap()
            .next()
            .is_none(),
        "config validation wrote runtime state into the startup cwd"
    );
}

#[test]
fn exact_init_replay_preserves_files_and_changed_ip_is_refused() {
    let directory = owner_only_temporary_directory();
    let first = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(first.status.success());
    let paths = [
        directory.path().join("configs/gateway.yaml"),
        directory.path().join("certs/server.crt"),
        directory.path().join("certs/server.key"),
    ];
    let before: Vec<Vec<u8>> = paths
        .iter()
        .map(|path| fs::read(path).expect("read initialized file"))
        .collect();

    let replay = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(
        replay.status.success(),
        "exact replay failed: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert!(String::from_utf8_lossy(&replay.stdout)
        .contains("Independent Gateway: already initialized"));
    for (path, expected) in paths.iter().zip(&before) {
        assert_eq!(
            &fs::read(path).unwrap(),
            expected,
            "{} changed",
            path.display()
        );
    }

    let other_directory = owner_only_temporary_directory();
    let config_argument = path_text(&paths[0]);
    let replay_elsewhere = run_gateway(
        other_directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--config",
            config_argument,
        ],
    );
    assert!(
        replay_elsewhere.status.success(),
        "exact replay depended on its cwd: {}",
        String::from_utf8_lossy(&replay_elsewhere.stderr)
    );
    assert!(
        fs::read_dir(other_directory.path())
            .unwrap()
            .next()
            .is_none(),
        "exact replay created a second identity in its invocation cwd"
    );

    let changed = run_gateway(directory.path(), &["init", "--public-ip", "8.8.8.8"]);
    assert!(!changed.status.success(), "changed identity was accepted");
    let changed_config = directory.path().join("configs/changed-ip.yaml");
    let changed_at_new_config = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "8.8.8.8",
            "--config",
            path_text(&changed_config),
        ],
    );
    assert!(
        !changed_at_new_config.status.success(),
        "changed identity was accepted at a new config path"
    );
    assert!(
        !changed_config.exists(),
        "identity mismatch left a partial runtime config"
    );
    let changed_listener = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--transport",
            "websocket",
            "--data-port",
            "9443",
        ],
    );
    assert!(
        !changed_listener.status.success(),
        "changed listener facts were accepted at the same config path"
    );
    let changed_mapping = run_gateway(
        directory.path(),
        &["init", "--public-ip", "1.1.1.1", "--mapping-port", "10444"],
    );
    assert!(
        !changed_mapping.status.success(),
        "changed mapping port was accepted at the same config path"
    );
    for (path, expected) in paths.iter().zip(&before) {
        assert_eq!(
            &fs::read(path).unwrap(),
            expected,
            "{} changed",
            path.display()
        );
    }
}

#[test]
fn another_config_in_the_same_deployment_root_reuses_the_matching_identity() {
    let directory = owner_only_temporary_directory();
    let first = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(first.status.success());
    let certificate = directory.path().join("certs/server.crt");
    let private_key = directory.path().join("certs/server.key");
    let certificate_before = fs::read(&certificate).unwrap();
    let key_before = fs::read(&private_key).unwrap();

    let second = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--transport",
            "websocket",
            "--data-port",
            "9443",
            "--config",
            "configs/secondary.yaml",
        ],
    );
    assert!(
        second.status.success(),
        "second config failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(directory.path().join("configs/secondary.yaml").is_file());
    assert_eq!(fs::read(&certificate).unwrap(), certificate_before);
    assert_eq!(fs::read(&private_key).unwrap(), key_before);
}

#[test]
fn init_validates_ports_before_writing_anything() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(
        directory.path(),
        &["init", "--public-ip", "1.1.1.1", "--data-port", "8444"],
    );
    assert!(
        !output.status.success(),
        "colliding QUIC ports were accepted"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("collides with mapping port 8444"));
    assert!(
        fs::read_dir(directory.path()).unwrap().next().is_none(),
        "failed validation mutated the working directory"
    );

    let custom_directory = owner_only_temporary_directory();
    let custom_collision = run_gateway(
        custom_directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--data-port",
            "10444",
            "--mapping-port",
            "10444",
        ],
    );
    assert!(!custom_collision.status.success());
    assert!(String::from_utf8_lossy(&custom_collision.stderr)
        .contains("collides with mapping port 10444"));
    assert!(
        fs::read_dir(custom_directory.path())
            .unwrap()
            .next()
            .is_none(),
        "custom port collision mutated the working directory"
    );

    let zero_directory = owner_only_temporary_directory();
    let zero = run_gateway(
        zero_directory.path(),
        &["init", "--public-ip", "1.1.1.1", "--mapping-port", "0"],
    );
    assert!(!zero.status.success());
    assert!(String::from_utf8_lossy(&zero.stderr).contains("mapping port must be non-zero"));
    assert!(
        fs::read_dir(zero_directory.path())
            .unwrap()
            .next()
            .is_none(),
        "zero mapping port mutated the working directory"
    );
}

#[test]
fn init_requires_a_public_ip_before_writing_anything() {
    for address in [
        "192.168.1.10",
        "2620:4f:8000::1",
        "3fff::1",
        "::ffff:8.8.8.8",
    ] {
        let directory = owner_only_temporary_directory();
        let output = run_gateway(directory.path(), &["init", "--public-ip", address]);
        assert!(
            !output.status.success(),
            "non-public IP {address} was accepted"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("not a public IP"));
        assert!(
            fs::read_dir(directory.path()).unwrap().next().is_none(),
            "failed validation for {address} mutated the working directory"
        );
    }
}

#[test]
fn init_refuses_a_root_config_flag_instead_of_silently_ignoring_it() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(
        directory.path(),
        &["--config", "custom.yaml", "init", "--public-ip", "1.1.1.1"],
    );
    assert!(
        !output.status.success(),
        "ambiguous config flag was ignored"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("place it after init or onboard"));
    assert!(
        fs::read_dir(directory.path()).unwrap().next().is_none(),
        "ambiguous invocation mutated the working directory"
    );
}

#[test]
fn init_refuses_check_config_instead_of_silently_writing_state() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(
        directory.path(),
        &["--check-config", "init", "--public-ip", "1.1.1.1"],
    );
    assert!(!output.status.success(), "--check-config init was accepted");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--check-config cannot be combined with a subcommand"));
    assert!(
        fs::read_dir(directory.path()).unwrap().next().is_none(),
        "--check-config init mutated the working directory"
    );
}

#[cfg(unix)]
#[test]
fn init_rejects_a_symlink_ancestor_before_creating_through_it() {
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    let directory = owner_only_temporary_directory();
    let real = directory.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&real, directory.path().join("linked")).unwrap();

    let output = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--config",
            "linked/nested/gateway.yaml",
        ],
    );
    assert!(!output.status.success(), "symlink ancestor was accepted");
    assert!(
        !real.join("nested").exists(),
        "init wrote through a symlink"
    );
}

#[cfg(unix)]
#[test]
fn init_never_tightens_an_existing_config_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = owner_only_temporary_directory();
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();

    let output = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--config",
            "shared/gateway.yaml",
        ],
    );
    assert!(
        !output.status.success(),
        "existing shared directory was accepted"
    );
    assert_eq!(
        fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
        0o755,
        "init changed permissions on an existing directory"
    );
    assert!(!shared.join("gateway.yaml").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn init_rejects_an_allow_acl_on_the_deployment_root_before_writing() {
    let directory = owner_only_temporary_directory();
    let acl_status = Command::new("chmod")
        .args([
            "+a",
            "everyone allow read,execute,file_inherit,directory_inherit",
        ])
        .arg(directory.path())
        .status()
        .expect("add an inheritable macOS ACL");
    assert!(acl_status.success());

    let output = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(
        !output.status.success(),
        "an ACL-accessible deployment root was accepted"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("extended ACL allow entry"));
    for path in ["configs", "certs", "state"] {
        assert!(
            !directory.path().join(path).exists(),
            "failed ACL validation created {path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn conflicting_config_is_refused_before_any_runtime_side_effect() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = owner_only_temporary_directory();
    let configs = directory.path().join("configs");
    fs::create_dir(&configs).unwrap();
    fs::set_permissions(&configs, fs::Permissions::from_mode(0o700)).unwrap();
    let config = configs.join("gateway.yaml");
    fs::write(&config, b"gateway:\n  listen_addr: 127.0.0.1:1\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    let output = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(!output.status.success(), "conflicting config was accepted");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("refusing to replace existing Gateway runtime config"));
    assert_eq!(
        fs::metadata(&configs).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::read(&config).unwrap(),
        b"gateway:\n  listen_addr: 127.0.0.1:1\n"
    );
    assert!(!directory.path().join("certs").exists());
    assert!(!directory.path().join("state").exists());
}

#[cfg(unix)]
#[test]
fn init_does_not_mint_a_certificate_from_a_hard_linked_key() {
    let directory = owner_only_temporary_directory();
    let first = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(first.status.success());
    let certificate = directory.path().join("certs/server.crt");
    let key = directory.path().join("certs/server.key");
    fs::remove_file(&certificate).unwrap();
    fs::hard_link(&key, directory.path().join("certs/key-copy")).unwrap();

    let replay = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(!replay.status.success(), "hard-linked key was accepted");
    assert!(String::from_utf8_lossy(&replay.stderr).contains("must not be hard-linked"));
    assert!(
        !certificate.exists(),
        "init minted a certificate before rejecting the hard-linked key"
    );
}

#[cfg(unix)]
#[test]
fn init_does_not_mint_a_certificate_from_a_key_with_an_invalid_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    for mode in [0o400, 0o4600] {
        let directory = owner_only_temporary_directory();
        let first = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
        assert!(first.status.success());
        let certificate = directory.path().join("certs/server.crt");
        let key = directory.path().join("certs/server.key");
        fs::remove_file(&certificate).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(mode)).unwrap();

        let secondary_config = directory.path().join("configs/secondary.yaml");
        let replay = run_gateway(
            directory.path(),
            &[
                "init",
                "--public-ip",
                "1.1.1.1",
                "--config",
                path_text(&secondary_config),
            ],
        );
        assert!(
            !replay.status.success(),
            "private-key mode {mode:o} was accepted"
        );
        assert!(String::from_utf8_lossy(&replay.stderr).contains("exactly 0600"));
        assert!(
            !certificate.exists(),
            "init minted a certificate before rejecting private-key mode {mode:o}"
        );
        assert!(
            !secondary_config.exists(),
            "invalid private-key mode left a partial runtime config"
        );
    }
}

#[test]
fn init_refuses_managed_identity_state_before_creating_static_files() {
    let directory = owner_only_temporary_directory();
    let configs = directory.path().join("configs");
    fs::create_dir(&configs).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&configs, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let managed_identity = configs.join("gateway.yaml.managed-identity.json");
    fs::write(&managed_identity, b"sentinel\n").unwrap();

    let output = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(!output.status.success(), "Managed state was accepted");
    assert!(String::from_utf8_lossy(&output.stderr).contains("refuses Managed identity state"));
    assert_eq!(fs::read(&managed_identity).unwrap(), b"sentinel\n");
    assert!(!directory.path().join("configs/gateway.yaml").exists());
    assert!(!directory.path().join("certs/server.key").exists());
}

#[test]
fn init_writes_the_selected_tcp_transport_and_data_port() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "1.1.1.1",
            "--transport",
            "websocket",
            "--data-port",
            "9443",
            "--mapping-port",
            "9443",
        ],
    );
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = fs::read_to_string(directory.path().join("configs/gateway.yaml")).unwrap();
    let gateway = tp_core::config::load_from_str(&config)
        .unwrap()
        .gateway
        .unwrap();
    assert_eq!(gateway.listen_addr, "0.0.0.0:9443");
    assert_eq!(gateway.transport_type, "websocket");
    assert_eq!(gateway.mapping_probe_port, 9443);
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("Open inbound: TCP 9443 (WebSocket data) and UDP 9443 (mapping)"));
}

#[test]
fn init_writes_the_selected_mapping_port_and_owner_command() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(
        directory.path(),
        &["init", "--public-ip", "1.1.1.1", "--mapping-port", "10444"],
    );
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = fs::read_to_string(directory.path().join("configs/gateway.yaml")).unwrap();
    let gateway = tp_core::config::load_from_str(&config)
        .unwrap()
        .gateway
        .unwrap();
    assert_eq!(gateway.mapping_probe_port, 10_444);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("UDP 10444 (mapping)"));
    assert!(stdout.contains("--gateway-mapping-port 10444"));
}

#[test]
fn a_manually_edited_mapping_port_remains_a_valid_runtime_config() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(directory.path(), &["init", "--public-ip", "1.1.1.1"]);
    assert!(output.status.success());
    let config_path = directory.path().join("configs/gateway.yaml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        config.replace("mapping_probe_port: 8444", "mapping_probe_port: 10444"),
    )
    .unwrap();

    let check = run_gateway(
        directory.path(),
        &["--config", path_text(&config_path), "--check-config"],
    );
    assert!(
        check.status.success(),
        "edited mapping port failed validation: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let replay = run_gateway(
        directory.path(),
        &["init", "--public-ip", "1.1.1.1", "--mapping-port", "10444"],
    );
    assert!(
        replay.status.success(),
        "edited config did not match an explicit init replay: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert!(String::from_utf8_lossy(&replay.stdout)
        .contains("Independent Gateway: already initialized"));
}

#[test]
fn init_uses_an_ipv6_listener_for_a_public_ipv6_identity() {
    let directory = owner_only_temporary_directory();
    let output = run_gateway(
        directory.path(),
        &[
            "init",
            "--public-ip",
            "2606:4700:4700::1111",
            "--transport",
            "grpc",
        ],
    );
    assert!(
        output.status.success(),
        "IPv6 init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = fs::read_to_string(directory.path().join("configs/gateway.yaml")).unwrap();
    let gateway = tp_core::config::load_from_str(&config)
        .unwrap()
        .gateway
        .unwrap();
    assert_eq!(gateway.listen_addr, "[::]:8443");
    assert_eq!(gateway.transport_type, "grpc");
}
