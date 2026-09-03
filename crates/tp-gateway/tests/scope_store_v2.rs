use std::fs;

use tempfile::TempDir;
use tp_core::provisioning::{GatewayBootstrapV2, TunnelOwnerFileV2};
use tp_gateway::scope::{ScopeStore, ScopeStoreError};

fn scope() -> tp_core::provisioning::GatewayScopeFileV2 {
    TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: "quic".into(),
        dial_address: "gateway.example.com".into(),
        port: 443,
        mapping_port: None,
        tls_server_name: None,
        trusted_certificate_pem: None,
    })
    .expect("owner")
    .scope()
    .expect("scope")
}

#[test]
fn static_reload_and_managed_snapshot_share_one_lookup_without_replacing_each_other() {
    let static_dir = TempDir::new().expect("static dir");
    let static_scope = scope();
    let managed_scope = scope();
    fs::write(
        static_dir.path().join("home.scope"),
        serde_yaml::to_string(&static_scope).expect("yaml"),
    )
    .expect("write scope");

    let store = ScopeStore::new();
    store
        .replace_managed_snapshot(vec![managed_scope.clone()])
        .expect("managed snapshot");
    assert_eq!(
        store
            .reload_static(static_dir.path())
            .expect("reload")
            .count,
        1
    );

    assert!(store.contains(&static_scope.tunnel_id));
    assert!(store.contains(&managed_scope.tunnel_id));
    store
        .replace_managed_snapshot(Vec::new())
        .expect("empty managed snapshot");
    assert!(store.contains(&static_scope.tunnel_id));
    assert!(!store.contains(&managed_scope.tunnel_id));
}

#[test]
fn full_managed_snapshot_replaces_only_the_managed_partition_atomically() {
    let static_dir = TempDir::new().expect("static dir");
    let static_scope = scope();
    let removed = scope();
    let retained = scope();
    let added = scope();
    fs::write(
        static_dir.path().join("static.scope"),
        serde_yaml::to_string(&static_scope).expect("yaml"),
    )
    .expect("write static scope");

    let store = ScopeStore::new();
    store
        .reload_static(static_dir.path())
        .expect("static reload");
    store
        .replace_managed_snapshot(vec![removed.clone(), retained.clone()])
        .expect("first full snapshot");

    let outcome = store
        .replace_managed_snapshot(vec![retained.clone(), added.clone()])
        .expect("replacement full snapshot");

    assert_eq!(outcome.count, 2);
    assert_eq!(outcome.removed_ids, vec![removed.tunnel_id.clone()]);
    assert!(store.contains(&static_scope.tunnel_id));
    assert!(!store.contains(&removed.tunnel_id));
    assert!(store.contains(&retained.tunnel_id));
    assert!(store.contains(&added.tunnel_id));
    assert_eq!(store.static_len(), 1);
    assert_eq!(store.managed_len(), 2);
}

#[test]
fn failed_static_reload_preserves_last_known_good_snapshot() {
    let static_dir = TempDir::new().expect("static dir");
    let original = scope();
    fs::write(
        static_dir.path().join("home.scope"),
        serde_yaml::to_string(&original).expect("yaml"),
    )
    .expect("write scope");

    let store = ScopeStore::new();
    store.reload_static(static_dir.path()).expect("reload");
    fs::write(static_dir.path().join("broken.scope"), "not: [valid").expect("write invalid");

    assert!(matches!(
        store.reload_static(static_dir.path()),
        Err(ScopeStoreError::InvalidScope { .. })
    ));
    assert!(store.contains(&original.tunnel_id));
    assert_eq!(store.static_len(), 1);
}

#[test]
fn successful_static_reload_reports_only_removed_tunnels() {
    let static_dir = TempDir::new().expect("static dir");
    let retained = scope();
    let removed = scope();
    fs::write(
        static_dir.path().join("retained.scope"),
        serde_yaml::to_string(&retained).expect("yaml"),
    )
    .expect("write retained scope");
    fs::write(
        static_dir.path().join("removed.scope"),
        serde_yaml::to_string(&removed).expect("yaml"),
    )
    .expect("write removed scope");

    let store = ScopeStore::new();
    store
        .reload_static(static_dir.path())
        .expect("first reload");
    fs::remove_file(static_dir.path().join("removed.scope")).expect("remove scope");

    let outcome = store
        .reload_static(static_dir.path())
        .expect("second reload");
    assert_eq!(outcome.count, 1);
    assert_eq!(outcome.removed_ids, vec![removed.tunnel_id.clone()]);
    assert!(store.contains(&retained.tunnel_id));
    assert!(!store.contains(&removed.tunnel_id));
}

#[test]
fn a_tunnel_id_cannot_exist_in_static_and_managed_sources() {
    let static_dir = TempDir::new().expect("static dir");
    let shared = scope();
    fs::write(
        static_dir.path().join("shared.scope"),
        serde_yaml::to_string(&shared).expect("yaml"),
    )
    .expect("write scope");

    let store = ScopeStore::new();
    store
        .replace_managed_snapshot(vec![shared.clone()])
        .expect("managed scope");
    assert!(matches!(
        store.reload_static(static_dir.path()),
        Err(ScopeStoreError::SourceConflict { .. })
    ));
    assert_eq!(store.static_len(), 0);
    assert!(store.contains(&shared.tunnel_id));
}

#[test]
fn managed_issuer_key_invariant_survives_removal() {
    let store = ScopeStore::new();
    let original = scope();
    store
        .replace_managed_snapshot(vec![original.clone()])
        .expect("first managed snapshot");
    store
        .replace_managed_snapshot(vec![original.clone()])
        .expect("same Tunnel and issuer key is accepted");
    assert_eq!(store.managed_len(), 1);
    store
        .replace_managed_snapshot(Vec::new())
        .expect("remove original scope");
    store
        .replace_managed_snapshot(vec![original.clone()])
        .expect("same issuer key can be re-added");
    store
        .replace_managed_snapshot(Vec::new())
        .expect("remove original scope again");

    let mut replacement = scope();
    replacement.tunnel_id = original.tunnel_id.clone();
    assert!(matches!(
        store.replace_managed_snapshot(vec![replacement]),
        Err(ScopeStoreError::IssuerReplacement { tunnel_id }) if tunnel_id == original.tunnel_id
    ));
    assert!(!store.contains(&original.tunnel_id));
}

#[test]
fn static_issuer_key_invariant_survives_removal() {
    let static_dir = TempDir::new().expect("static dir");
    let original = scope();
    fs::write(
        static_dir.path().join("home.scope"),
        serde_yaml::to_string(&original).expect("yaml"),
    )
    .expect("write original scope");

    let store = ScopeStore::new();
    store
        .reload_static(static_dir.path())
        .expect("first reload");
    fs::remove_file(static_dir.path().join("home.scope")).expect("remove original scope");
    store
        .reload_static(static_dir.path())
        .expect("remove original Scope");
    fs::write(
        static_dir.path().join("home.scope"),
        serde_yaml::to_string(&original).expect("yaml"),
    )
    .expect("re-add original scope");
    store
        .reload_static(static_dir.path())
        .expect("same issuer key can be re-added");
    fs::remove_file(static_dir.path().join("home.scope")).expect("remove original scope again");
    store
        .reload_static(static_dir.path())
        .expect("remove original Scope again");

    let mut replacement = scope();
    replacement.tunnel_id = original.tunnel_id.clone();
    fs::write(
        static_dir.path().join("home.scope"),
        serde_yaml::to_string(&replacement).expect("yaml"),
    )
    .expect("write replacement scope");

    assert!(matches!(
        store.reload_static(static_dir.path()),
        Err(ScopeStoreError::IssuerReplacement { tunnel_id }) if tunnel_id == original.tunnel_id
    ));
    assert!(!store.contains(&original.tunnel_id));
}

#[cfg(unix)]
#[test]
fn static_loader_rejects_symlinked_scope_files() {
    use std::os::unix::fs::symlink;

    let static_dir = TempDir::new().expect("static dir");
    let outside = TempDir::new().expect("outside dir");
    let target = outside.path().join("outside.scope");
    fs::write(&target, serde_yaml::to_string(&scope()).expect("yaml")).expect("write scope");
    symlink(target, static_dir.path().join("linked.scope")).expect("symlink");

    let store = ScopeStore::new();
    assert!(matches!(
        store.reload_static(static_dir.path()),
        Err(ScopeStoreError::UnsafePath { .. })
    ));
}
