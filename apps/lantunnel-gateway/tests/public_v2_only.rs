use std::fs;
use std::net::UdpSocket;
use std::process::{Command, Output};

use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};

fn run_with_config(config: &str) -> Output {
    let temporary_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
    let dir = tempfile::Builder::new()
        .prefix("lantunnel-gateway-public-v2-")
        .tempdir_in(temporary_root)
        .expect("temporary config directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
            .expect("owner-only config directory");
    }
    let path = dir.path().join("gateway.yaml");
    std::fs::write(&path, config).expect("write test config");
    Command::new(env!("CARGO_BIN_EXE_lantunnel-gateway"))
        .args(["--config", path.to_str().expect("UTF-8 config path")])
        .output()
        .expect("run public Gateway")
}

#[test]
fn binary_rejects_every_legacy_gateway_authority_key_before_startup() {
    for legacy_field in [
        "auth_username: ''",
        "auth_password: ''",
        "credential: null",
        "proxy: {}",
        "tunnel_key: ''",
        "group: ''",
        "group_id: ''",
        "group_password: ''",
        "username: ''",
        "password: ''",
    ] {
        let output = run_with_config(&format!(
            "gateway:\n  listen_addr: 127.0.0.1:8443\n  {legacy_field}\n"
        ));
        assert!(!output.status.success(), "accepted {legacy_field}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("forbidden Legacy field"),
            "unexpected error for {legacy_field}: {stderr}"
        );
    }
}

#[test]
fn binary_rejects_yaml_merge_and_tagged_key_bypasses() {
    for config in [
        "gateway:\n  <<: {auth_username: legacy, auth_password: key}\n",
        "gateway:\n  !!str auth_username: legacy\n",
    ] {
        let output = run_with_config(config);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("forbidden") || stderr.contains("plain YAML strings"),
            "unexpected error: {stderr}"
        );
    }
}

#[test]
fn binary_requires_a_v2_scope_source() {
    let output = run_with_config("gateway:\n  listen_addr: 127.0.0.1:8443\n");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("V2 Scope source"),
        "unexpected error: {stderr}"
    );
}

#[test]
fn binary_allows_disabled_p2p_signaling_before_opening_tls_files() {
    let output = run_with_config(
        "gateway:\n  listen_addr: 127.0.0.1:8443\n  tls_cert: missing.crt\n  tls_key: missing.key\n  scopes_dir: scopes.d\n  p2p:\n    enabled: false\n",
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing.key"),
        "startup did not reach persistent TLS validation: {stderr}"
    );
    assert!(
        !stderr.contains("requires gateway.p2p.enabled=true"),
        "startup still rejected the Relay-only Gateway clone: {stderr}"
    );
}

#[test]
fn binary_fails_closed_when_another_process_holds_its_mapping_port() {
    let temporary_root = fs::canonicalize(std::env::temp_dir()).unwrap();
    let temporary = tempfile::Builder::new()
        .prefix("lantunnel-gateway-mapping-port-taken-")
        .tempdir_in(temporary_root)
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = temporary.path().join("server.crt");
    let private_key = temporary.path().join("server.key");
    fs::write(&certificate, identity.cert.pem()).unwrap();
    fs::write(&private_key, identity.key_pair.serialize_pem()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&certificate, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let scopes = temporary.path().join("scopes.d");
    fs::create_dir(&scopes).unwrap();
    let scope = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "localhost".into(),
        port: 443,
        mapping_port: None,
        tls_server_name: None,
        trusted_certificate_pem: None,
    })
    .unwrap()
    .scope()
    .unwrap();
    fs::write(
        scopes.join("test.scope"),
        serde_yaml::to_string(&scope).unwrap(),
    )
    .unwrap();

    // Someone else already owns the port this Gateway would reflect on.
    let occupied = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mapping_port = occupied.local_addr().unwrap().port();
    let data_port = if mapping_port == 18_443 {
        18_442
    } else {
        18_443
    };
    let config = temporary.path().join("gateway.yaml");
    fs::write(
        &config,
        format!(
            "gateway:\n  listen_addr: 127.0.0.1:{data_port}\n  tls_cert: {}\n  tls_key: {}\n  scopes_dir: {}\n  mapping_probe_port: {mapping_port}\n",
            certificate.display(),
            private_key.display(),
            scopes.display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lantunnel-gateway"))
        .args(["--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bind this Gateway's UDP mapping service")
            && stderr.contains("already holds that port"),
        "unexpected startup error: {stderr}"
    );
}
