//! One version, written once.
//!
//! The workspace already declared it and every crate but one inherited it. The
//! desktop client pinned its own, Android and iOS each declared theirs, and the
//! Tauri manifest carried a third copy that the release Makefile read for the
//! artifact names. So 2.0.5 shipped desktop and Android builds that called
//! themselves 2.0.4, with the file names saying otherwise.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn workspace_version() -> String {
    let manifest = read("Cargo.toml");
    let after = manifest
        .split_once("[workspace.package]")
        .expect("[workspace.package]")
        .1;
    let line = after
        .lines()
        .find(|line| line.trim_start().starts_with("version"))
        .expect("workspace version line");
    line.split('"').nth(1).expect("quoted version").to_string()
}

#[test]
fn every_product_takes_its_version_from_the_workspace() {
    let version = workspace_version();

    // Rust: the desktop client is the one that used to pin its own.
    let desktop = read("apps/lantunnel-client/src-tauri/Cargo.toml");
    assert!(
        desktop.contains("version.workspace = true"),
        "the desktop client must inherit the workspace version",
    );

    // Tauri falls back to the crate version when the manifest omits one, and
    // the release Makefile falls back to the workspace version in turn.
    let tauri = read("apps/lantunnel-client/src-tauri/tauri.conf.json");
    assert!(
        !tauri
            .lines()
            .any(|line| line.trim_start().starts_with("\"version\"")),
        "tauri.conf.json must not declare a version of its own",
    );

    // Android reads the workspace manifest at configure time.
    let gradle = read("apps/android-proxy/app/build.gradle.kts");
    assert!(
        gradle.contains("versionName = workspaceVersion")
            && gradle.contains("versionCode = workspaceVersionCode"),
        "the Android build must derive its version from the workspace",
    );

    // The rest are declarations a build system cannot compute for itself, so
    // they are checked rather than derived.
    for (file, needle) in [
        (
            "apps/lantunnel-client/frontend/package.json",
            format!("\"version\": \"{version}\""),
        ),
        (
            "apps/ios-proxy/project.yml",
            format!("MARKETING_VERSION: \"{version}\""),
        ),
        // The Xcode project is generated from project.yml by `xcodegen
        // generate` and is not tracked, so project.yml above is the only iOS
        // declaration there is to check.
    ] {
        assert!(
            read(file).contains(&needle),
            "{file} disagrees with the workspace version {version}",
        );
    }

    // The lock files are generated, but only when someone runs the tool. The
    // frontend lock went two releases saying 2.0.4 because nothing looked at
    // it: the one check that did sat behind a hardcoded version assertion that
    // had already failed.
    let frontend_lock = read("apps/lantunnel-client/frontend/package-lock.json");
    let lock_versions: Vec<&str> = frontend_lock
        .lines()
        .take(12)
        .filter_map(|line| line.trim_start().strip_prefix("\"version\": \""))
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert_eq!(
        lock_versions,
        vec![version.as_str(), version.as_str()],
        "package-lock.json disagrees with the workspace version {version}; run `npm version {version}`",
    );

    for package in workspace_member_names() {
        assert_eq!(
            locked_version(&package),
            version,
            "Cargo.lock has {package} at a version other than the workspace's",
        );
    }
}

/// Every workspace member, read from the manifest rather than listed here, so
/// a new crate is covered without anyone remembering to add it. The package
/// name comes from the member's own manifest, because it is not always the
/// directory name — `apps/lantunnel-client/src-tauri` is `lantunnel-client`.
fn workspace_member_names() -> Vec<String> {
    let manifest = read("Cargo.toml");
    let members = manifest
        .split_once("members = [")
        .expect("[workspace] members")
        .1
        .split_once(']')
        .expect("members list end")
        .0;
    members
        .split('"')
        .filter(|part| part.contains('/'))
        .map(|path| {
            let member = read(&format!("{path}/Cargo.toml"));
            member
                .lines()
                .find_map(|line| line.trim().strip_prefix("name = \""))
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_else(|| panic!("{path}/Cargo.toml has no package name"))
                .to_string()
        })
        .collect()
}

fn locked_version(package: &str) -> String {
    let lock = read("Cargo.lock");
    let entry = lock
        .split("[[package]]")
        .find(|entry| entry.contains(&format!("name = \"{package}\"")))
        .unwrap_or_else(|| panic!("Cargo.lock has no entry for {package}"));
    entry
        .lines()
        .find_map(|line| line.trim().strip_prefix("version = "))
        .and_then(|rest| rest.trim().strip_prefix('"'))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("Cargo.lock entry for {package} has no version"))
        .to_string()
}
