use lantunnel_client::peer_store::replace_private_json_file;

#[cfg(unix)]
#[test]
fn private_json_replacement_is_complete_and_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("client").join("settings.json");

    replace_private_json_file(&path, &serde_json::json!({ "value": "old" }))
        .expect("write old settings");
    replace_private_json_file(
        &path,
        &serde_json::json!({ "value": "new", "items": [1, 2, 3] }),
    )
    .expect("replace settings");

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read settings"))
            .expect("settings remain complete JSON");
    assert_eq!(saved["value"], "new");
    assert_eq!(saved["items"], serde_json::json!([1, 2, 3]));
    assert_eq!(
        std::fs::metadata(&path)
            .expect("settings metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().expect("settings parent"))
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[cfg(unix)]
#[test]
fn private_json_replacement_refuses_a_symlink_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let parent = temp.path().join("client");
    std::fs::create_dir(&parent).expect("create parent");
    let victim = temp.path().join("victim.json");
    std::fs::write(&victim, b"do not replace").expect("write victim");
    let path = parent.join("settings.json");
    symlink(&victim, &path).expect("create symlink");

    assert!(replace_private_json_file(&path, &serde_json::json!({ "value": "new" })).is_err());
    assert_eq!(
        std::fs::read(&victim).expect("read victim"),
        b"do not replace"
    );
    assert!(std::fs::symlink_metadata(path)
        .expect("symlink metadata")
        .file_type()
        .is_symlink());
}
