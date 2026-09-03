#![cfg(windows)]

use std::fs;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;

use lantunnel_client::peer_store::{import_peer_profile, replace_private_json_file};
use tp_core::provisioning::{GatewayBootstrapV2, PeerProfileV2, TunnelOwnerFileV2};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, INHERITED_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSID,
    SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

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
        .add_peer(None, 1, Some("windows-pc".into()))
        .expect("generate Peer profile")
}

fn assert_owner_only_acl(path: &Path, directory: bool) {
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

#[test]
fn imported_profiles_overrides_and_settings_are_owner_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("incoming.peer");
    let config_root = temp.path().join("client-config");
    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write source Peer profile");

    import_peer_profile(&source, &config_root).expect("import Peer profile");
    let peer_path = config_root
        .join("peers")
        .join(format!("{}.peer", profile.tunnel_id));
    let peers_dir = config_root.join("peers");
    let settings_path = config_root.join("settings.json");
    replace_private_json_file(&settings_path, &serde_json::json!({ "pin": "secret" }))
        .expect("write settings");

    // `gateway-overrides` is deliberately absent here: the Client only ever
    // reads that directory, and an override is placed by the owner. Asserting
    // an ACL on a path the product never creates asserts nothing.
    for directory in [&config_root, &peers_dir] {
        assert_owner_only_acl(directory, true);
    }
    for file in [&peer_path, &settings_path] {
        assert_owner_only_acl(file, false);
    }
}

#[test]
fn reparse_directories_and_file_targets_are_rejected() {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("incoming.peer");
    let profile = valid_peer_profile();
    fs::write(
        &source,
        serde_yaml::to_string(&profile).expect("serialize Peer profile"),
    )
    .expect("write source Peer profile");

    let real_config = temp.path().join("real-config");
    fs::create_dir(&real_config).expect("create real config directory");
    let config_link = temp.path().join("config-link");
    symlink_dir(&real_config, &config_link).expect("create directory reparse point");
    assert!(import_peer_profile(&source, &config_link).is_err());
    assert_eq!(
        fs::read_dir(&real_config)
            .expect("read real config")
            .count(),
        0
    );

    let settings_dir = temp.path().join("settings");
    fs::create_dir(&settings_dir).expect("create settings directory");
    let victim = temp.path().join("victim.json");
    fs::write(&victim, b"do not replace").expect("write victim");
    let settings_link = settings_dir.join("settings.json");
    symlink_file(&victim, &settings_link).expect("create file reparse point");
    assert!(
        replace_private_json_file(&settings_link, &serde_json::json!({ "pin": "new" })).is_err()
    );
    assert_eq!(fs::read(victim).expect("read victim"), b"do not replace");
}
