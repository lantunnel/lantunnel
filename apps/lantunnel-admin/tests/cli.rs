use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use tp_core::provisioning::{
    GatewayScopeFileV2, PeerBootstrapV2, PeerProfileV2, TunnelOwnerFileV2,
};

const FIXTURE_CERTIFICATE_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBgDCCASegAwIBAgIUPHDUu9WL36yvTmFeNFZVe/qhClcwCgYIKoZIzj0EAwIw\n\
HTEbMBkGA1UEAwwSUnVzdGxzIFJvYnVzdCBSb290MCAXDTc1MDEwMTAwMDAwMFoY\n\
DzQwOTYwMTAxMDAwMDAwWjAdMRswGQYDVQQDDBJSdXN0bHMgUm9idXN0IFJvb3Qw\n\
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASW/VkDFs5iGDQvH8jaXYT4jMx66jo+\n\
5CWKyMt4OlTDdBfKfnmQ9LYeK/PsYfJ8wVizuSlPzXi9je8SnyYejGP3o0MwQTAP\n\
BgNVHQ8BAf8EBQMDB4QAMB0GA1UdDgQWBBRqY/oMENJbNo7y39iL6GW3tDs0rzAP\n\
BgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIEUbrmSUjANju9nNpFop\n\
PAl9Wh8tBxI5IY+BPh466+aUAiA1/9+prypt6s3Doo0GDsnoFGJi1UBivUg1qdik\n\
cy4eNw==\n\
-----END CERTIFICATE-----\n";

#[cfg(windows)]
fn assert_owner_only_acl(path: &Path, directory: bool) {
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, INHERITED_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PSID, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: buffers are initialized for the Win32 APIs and the returned
    // security descriptor owns the owner/DACL pointers until LocalFree.
    unsafe {
        let mut owner: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let status = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        );
        assert_eq!(status, ERROR_SUCCESS, "read ACL for {}", path.display());
        assert!(!owner.is_null());
        assert!(!dacl.is_null());

        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0, "DACL must not inherit");

        let mut size: ACL_SIZE_INFORMATION = zeroed();
        assert_ne!(
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ),
            0
        );
        assert_eq!(size.AceCount, 1, "only the owner may appear in the DACL");
        let mut raw_ace = ptr::null_mut();
        assert_ne!(GetAce(dacl, 0, &mut raw_ace), 0);
        let ace = &*(raw_ace as *const ACCESS_ALLOWED_ACE);
        assert_eq!(ace.Header.AceType, 0, "ACE must allow access");
        assert_eq!(u32::from(ace.Header.AceFlags) & INHERITED_ACE, 0);
        assert_eq!(ace.Mask & FILE_ALL_ACCESS, FILE_ALL_ACCESS);
        let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast();
        assert_ne!(EqualSid(owner, ace_sid), 0, "sole ACE must be the owner");
        let expected_inheritance = if directory {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            0
        };
        assert_eq!(
            u32::from(ace.Header.AceFlags) & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE),
            expected_inheritance
        );
        LocalFree(descriptor);
    }
}

fn admin(args: &[&OsStr], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lantunnel-admin"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("run lantunnel-admin")
}

fn one_file_with_extension(dir: &Path, extension: &str) -> PathBuf {
    let mut matches = fs::read_dir(dir)
        .expect("read output directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension() == Some(OsStr::new(extension)));
    let path = matches.next().expect("matching artifact");
    assert!(
        matches.next().is_none(),
        "expected exactly one .{extension}"
    );
    path
}

fn write_fixture_certificate(dir: &Path) -> PathBuf {
    let path = dir.join("gateway.crt");
    fs::write(&path, FIXTURE_CERTIFICATE_PEM).expect("write fixture certificate");
    path
}

fn init_static_tunnel(dir: &Path) -> PathBuf {
    let output = admin(
        &[
            OsStr::new("init-tunnel"),
            OsStr::new("--gateway-transport"),
            OsStr::new("quic"),
            OsStr::new("--gateway-host"),
            OsStr::new("gateway.example.com"),
            OsStr::new("--gateway-port"),
            OsStr::new("443"),
            OsStr::new("--output-dir"),
            dir.as_os_str(),
        ],
        dir,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    one_file_with_extension(dir, "tunnel")
}

#[test]
fn init_tunnel_creates_verifiable_yaml_artifacts() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let scope_path = one_file_with_extension(temp.path(), "scope");
    let owner: TunnelOwnerFileV2 =
        serde_yaml::from_str(&fs::read_to_string(tunnel_path).expect("read tunnel"))
            .expect("parse tunnel YAML");
    let scope: GatewayScopeFileV2 =
        serde_yaml::from_str(&fs::read_to_string(scope_path).expect("read scope"))
            .expect("parse scope YAML");

    owner.verify().expect("valid owner artifact");
    scope.verify().expect("valid scope artifact");
    assert_eq!(scope.tunnel_id, owner.tunnel_id);
    assert_eq!(owner.static_gateway.dial_address, "gateway.example.com");
    assert_eq!(owner.static_gateway.tls_server_name, None);
    assert_eq!(owner.static_gateway.mapping_port, None);
}

#[test]
fn init_tunnel_records_a_nondefault_mapping_port_in_owner_and_peer_artifacts() {
    let temp = TempDir::new().expect("temp dir");
    let output = admin(
        &[
            OsStr::new("init-tunnel"),
            OsStr::new("--gateway-transport"),
            OsStr::new("quic"),
            OsStr::new("--gateway-host"),
            OsStr::new("gateway.example.com"),
            OsStr::new("--gateway-port"),
            OsStr::new("443"),
            OsStr::new("--gateway-mapping-port"),
            OsStr::new("10444"),
            OsStr::new("--output-dir"),
            temp.path().as_os_str(),
        ],
        temp.path(),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tunnel_path = one_file_with_extension(temp.path(), "tunnel");
    let owner: TunnelOwnerFileV2 =
        serde_yaml::from_str(&fs::read_to_string(&tunnel_path).unwrap()).unwrap();
    assert_eq!(owner.static_gateway.mapping_port, Some(10_444));

    let peer_path = temp.path().join("custom-mapping.peer");
    let add = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--output"),
            peer_path.as_os_str(),
        ],
        temp.path(),
    );
    assert!(add.status.success());
    let peer: PeerProfileV2 =
        serde_yaml::from_str(&fs::read_to_string(peer_path).unwrap()).unwrap();
    match peer.bootstrap {
        PeerBootstrapV2::StaticGateway(gateway) => {
            assert_eq!(gateway.mapping_port, Some(10_444));
        }
        PeerBootstrapV2::ManagedPlatform { .. } => panic!("expected static Gateway bootstrap"),
    }
}

#[test]
fn add_peer_updates_owner_and_creates_an_importable_profile() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let peer_path = temp.path().join("desktop.peer");

    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--overlay-ip"),
            OsStr::new("198.18.23.7"),
            OsStr::new("--replicas"),
            OsStr::new("3"),
            OsStr::new("--name"),
            OsStr::new("desktop"),
            OsStr::new("--output"),
            peer_path.as_os_str(),
        ],
        temp.path(),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let owner: TunnelOwnerFileV2 =
        serde_yaml::from_str(&fs::read_to_string(&tunnel_path).expect("read tunnel"))
            .expect("parse tunnel");
    let peer: PeerProfileV2 =
        serde_yaml::from_str(&fs::read_to_string(&peer_path).expect("read peer"))
            .expect("parse peer");
    owner.verify().expect("updated owner verifies");
    peer.verify().expect("Peer profile verifies");
    assert_eq!(owner.allocated_peers.len(), 1);
    assert_eq!(owner.allocated_peers[0].label.as_deref(), Some("desktop"));
    assert_eq!(peer.peer.overlay_ip.to_string(), "198.18.23.7");
    assert_eq!(peer.replicas, 3);
    match peer.bootstrap {
        PeerBootstrapV2::StaticGateway(gateway) => {
            assert_eq!(gateway.dial_address, "gateway.example.com")
        }
        PeerBootstrapV2::ManagedPlatform { .. } => panic!("expected static Gateway bootstrap"),
    }
}

#[cfg(unix)]
#[test]
fn secret_artifacts_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let peer_path = temp.path().join("owner-only.peer");
    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--output"),
            peer_path.as_os_str(),
        ],
        temp.path(),
    );
    assert!(output.status.success());

    assert_eq!(
        fs::metadata(tunnel_path)
            .expect("Tunnel metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(peer_path)
            .expect("Peer metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(windows)]
#[test]
fn windows_secret_artifacts_and_directory_are_owner_only() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let peer_path = temp.path().join("owner-only.peer");
    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--output"),
            peer_path.as_os_str(),
        ],
        temp.path(),
    );
    assert!(output.status.success());

    assert_owner_only_acl(temp.path(), true);
    assert_owner_only_acl(&tunnel_path, false);
    assert_owner_only_acl(&peer_path, false);
}

#[cfg(windows)]
#[test]
fn windows_reparse_output_directory_is_rejected() {
    use std::os::windows::fs::symlink_dir;

    let temp = TempDir::new().expect("temp dir");
    let real_output = temp.path().join("real");
    fs::create_dir(&real_output).expect("create real output directory");
    let output_link = temp.path().join("linked");
    symlink_dir(&real_output, &output_link).expect("create directory reparse point");

    let output = admin(
        &[
            OsStr::new("init-tunnel"),
            OsStr::new("--gateway-transport"),
            OsStr::new("quic"),
            OsStr::new("--gateway-host"),
            OsStr::new("gateway.example.com"),
            OsStr::new("--gateway-port"),
            OsStr::new("443"),
            OsStr::new("--output-dir"),
            output_link.as_os_str(),
        ],
        temp.path(),
    );
    assert!(!output.status.success());
    assert_eq!(
        fs::read_dir(real_output).expect("read real output").count(),
        0
    );
}

#[test]
fn add_peer_refuses_to_overwrite_an_existing_profile() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let peer_path = temp.path().join("existing.peer");
    fs::write(&peer_path, b"keep me").expect("write sentinel");

    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--output"),
            peer_path.as_os_str(),
        ],
        temp.path(),
    );

    assert!(!output.status.success());
    assert_eq!(fs::read(&peer_path).expect("read sentinel"), b"keep me");
    let owner: TunnelOwnerFileV2 =
        serde_yaml::from_str(&fs::read_to_string(tunnel_path).expect("read tunnel"))
            .expect("parse tunnel");
    assert!(owner.allocated_peers.is_empty());
}

#[test]
fn init_tunnel_rejects_pem_containing_a_private_key() {
    let temp = TempDir::new().expect("temp dir");
    let pem_path = temp.path().join("combined.pem");
    fs::write(
        &pem_path,
        "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
    )
    .expect("write invalid PEM");

    let output = admin(
        &[
            OsStr::new("init-tunnel"),
            OsStr::new("--gateway-transport"),
            OsStr::new("quic"),
            OsStr::new("--gateway-host"),
            OsStr::new("gateway.example.com"),
            OsStr::new("--gateway-port"),
            OsStr::new("443"),
            OsStr::new("--gateway-cert"),
            pem_path.as_os_str(),
            OsStr::new("--output-dir"),
            temp.path().as_os_str(),
        ],
        temp.path(),
    );

    assert!(!output.status.success());
    assert!(
        fs::read_dir(temp.path())
            .expect("read output directory")
            .all(|entry| {
                let path = entry.expect("entry").path();
                path.extension() != Some(OsStr::new("tunnel"))
                    && path.extension() != Some(OsStr::new("scope"))
            }),
        "invalid PEM must not create provisioning artifacts"
    );
}

#[test]
fn init_tunnel_uses_ip_for_dial_and_host_for_tls_name() {
    let temp = TempDir::new().expect("temp dir");
    let certificate = write_fixture_certificate(temp.path());
    let output = admin(
        &[
            OsStr::new("init-tunnel"),
            OsStr::new("--gateway-transport"),
            OsStr::new("quic"),
            OsStr::new("--gateway-host"),
            OsStr::new("gateway.example.com"),
            OsStr::new("--gateway-ip"),
            OsStr::new("203.0.113.10"),
            OsStr::new("--gateway-port"),
            OsStr::new("443"),
            OsStr::new("--gateway-cert"),
            certificate.as_os_str(),
            OsStr::new("--output-dir"),
            temp.path().as_os_str(),
        ],
        temp.path(),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let owner: TunnelOwnerFileV2 = serde_yaml::from_str(
        &fs::read_to_string(one_file_with_extension(temp.path(), "tunnel")).expect("read tunnel"),
    )
    .expect("parse tunnel");
    assert_eq!(owner.static_gateway.dial_address, "203.0.113.10");
    assert_eq!(
        owner.static_gateway.tls_server_name.as_deref(),
        Some("gateway.example.com")
    );
    assert!(owner
        .static_gateway
        .trusted_certificate_pem
        .as_deref()
        .is_some_and(|pem| pem.contains("BEGIN CERTIFICATE")));
}

#[test]
fn add_peer_rejects_an_already_allocated_overlay_ip() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let first_path = temp.path().join("first.peer");
    let second_path = temp.path().join("second.peer");
    let common = [
        OsStr::new("add-peer"),
        OsStr::new("--tunnel"),
        tunnel_path.as_os_str(),
        OsStr::new("--overlay-ip"),
        OsStr::new("198.18.23.7"),
    ];
    let first = admin(
        &[
            common[0],
            common[1],
            common[2],
            common[3],
            common[4],
            OsStr::new("--output"),
            first_path.as_os_str(),
        ],
        temp.path(),
    );
    assert!(first.status.success());

    let second = admin(
        &[
            common[0],
            common[1],
            common[2],
            common[3],
            common[4],
            OsStr::new("--output"),
            second_path.as_os_str(),
        ],
        temp.path(),
    );
    assert!(!second.status.success());
    assert!(!second_path.exists());
    let owner: TunnelOwnerFileV2 =
        serde_yaml::from_str(&fs::read_to_string(tunnel_path).expect("read tunnel"))
            .expect("parse tunnel");
    assert_eq!(owner.allocated_peers.len(), 1);
}

#[test]
fn add_peer_rejects_a_replica_hint_above_the_gateway_safety_limit() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let peer_path = temp.path().join("too-many-replicas.peer");
    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--replicas"),
            OsStr::new("9"),
            OsStr::new("--output"),
            peer_path.as_os_str(),
        ],
        temp.path(),
    );

    assert!(!output.status.success());
    assert!(!peer_path.exists());
}

#[test]
fn concurrent_add_peer_commands_allocate_distinct_profiles() {
    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let mut children = Vec::new();
    let mut peer_paths = Vec::new();
    for index in 0..8 {
        let peer_path = temp.path().join(format!("peer-{index}.peer"));
        let child = Command::new(env!("CARGO_BIN_EXE_lantunnel-admin"))
            .args([
                OsStr::new("add-peer"),
                OsStr::new("--tunnel"),
                tunnel_path.as_os_str(),
                OsStr::new("--output"),
                peer_path.as_os_str(),
            ])
            .current_dir(temp.path())
            .spawn()
            .expect("spawn lantunnel-admin");
        children.push(child);
        peer_paths.push(peer_path);
    }

    for child in children {
        assert!(child
            .wait_with_output()
            .expect("wait for add-peer")
            .status
            .success());
    }
    let mut overlay_ips = std::collections::HashSet::new();
    for peer_path in peer_paths {
        let peer: PeerProfileV2 =
            serde_yaml::from_str(&fs::read_to_string(peer_path).expect("read peer"))
                .expect("parse peer");
        peer.verify().expect("verify peer");
        assert!(overlay_ips.insert(peer.peer.overlay_ip));
    }
    let owner: TunnelOwnerFileV2 =
        serde_yaml::from_str(&fs::read_to_string(tunnel_path).expect("read tunnel"))
            .expect("parse tunnel");
    owner.verify().expect("verify owner");
    assert_eq!(owner.allocated_peers.len(), 8);
}

#[cfg(unix)]
#[test]
fn add_peer_refuses_symlink_paths() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp dir");
    let tunnel_path = init_static_tunnel(temp.path());
    let tunnel_link = temp.path().join("owner-link.tunnel");
    symlink(&tunnel_path, &tunnel_link).expect("create Tunnel symlink");
    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_link.as_os_str(),
            OsStr::new("--output"),
            temp.path().join("blocked.peer").as_os_str(),
        ],
        temp.path(),
    );
    assert!(!output.status.success());

    let sentinel = temp.path().join("sentinel");
    fs::write(&sentinel, b"keep me").expect("write sentinel");
    let peer_link = temp.path().join("peer-link.peer");
    symlink(&sentinel, &peer_link).expect("create Peer symlink");
    let output = admin(
        &[
            OsStr::new("add-peer"),
            OsStr::new("--tunnel"),
            tunnel_path.as_os_str(),
            OsStr::new("--output"),
            peer_link.as_os_str(),
        ],
        temp.path(),
    );
    assert!(!output.status.success());
    assert_eq!(fs::read(sentinel).expect("read sentinel"), b"keep me");
}

#[cfg(unix)]
#[test]
fn init_tunnel_refuses_a_symlink_output_directory() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp dir");
    let real_output = temp.path().join("real");
    fs::create_dir(&real_output).expect("create real output directory");
    let output_link = temp.path().join("linked");
    symlink(&real_output, &output_link).expect("create output directory symlink");

    let output = admin(
        &[
            OsStr::new("init-tunnel"),
            OsStr::new("--gateway-transport"),
            OsStr::new("quic"),
            OsStr::new("--gateway-host"),
            OsStr::new("gateway.example.com"),
            OsStr::new("--gateway-port"),
            OsStr::new("443"),
            OsStr::new("--output-dir"),
            output_link.as_os_str(),
        ],
        temp.path(),
    );
    assert!(!output.status.success());
    assert_eq!(
        fs::read_dir(real_output).expect("read real output").count(),
        0
    );
}
