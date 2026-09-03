//! The macOS disk image has to offer a way to install.
//!
//! It used to contain the app and nothing else, so opening it presented one
//! icon and no destination: the only move was to drag it somewhere and hope.
//! Every macOS app ships a link to /Applications beside the bundle, and that
//! link is what makes the drag mean something.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root")
}

#[test]
fn the_disk_image_stages_a_link_to_applications() {
    for (script, staging) in [
        (
            "scripts/macos-sign-notarize.sh",
            "ditto \"$signed_app\" \"$stage/$app_name\"",
        ),
        (
            "Makefile",
            "cp -R \"$$app_dir\" \"$$stage/$(TAURI_PRODUCT_NAME).app\"",
        ),
    ] {
        let source = fs::read_to_string(repo_root().join(script))
            .unwrap_or_else(|error| panic!("read {script}: {error}"));
        assert!(
            source.contains(staging),
            "{script} no longer stages the app the way this test reads it",
        );
        assert!(
            source.contains("ln -s /Applications"),
            "{script} builds a disk image with no way to install from it",
        );
    }
}
