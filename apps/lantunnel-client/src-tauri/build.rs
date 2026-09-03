use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const WINDOWS_ADMIN_MANIFEST: &str = include_str!("windows/require-administrator.manifest.xml");

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let sidecar_dir = env::var("LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR").ok();

    println!("cargo:rerun-if-env-changed=LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR");
    println!("cargo:rerun-if-changed=windows/require-administrator.manifest.xml");

    generate_sidecar_assets(&target_os, sidecar_dir.as_deref());
    run_tauri_build(&target_os);
}

fn run_tauri_build(target_os: &str) {
    if target_os == "windows" {
        let windows = tauri_build::WindowsAttributes::new().app_manifest(WINDOWS_ADMIN_MANIFEST);
        let attributes = tauri_build::Attributes::new().windows_attributes(windows);
        tauri_build::try_build(attributes).expect("failed to run Tauri build script");
    } else {
        tauri_build::build();
    }
}

fn generate_sidecar_assets(target_os: &str, sidecar_dir: Option<&str>) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let out_path = out_dir.join("sidecar_assets.rs");

    let mut assets = Vec::new();
    if target_os == "windows" {
        if let Some(dir) = sidecar_dir.filter(|value| !value.trim().is_empty()) {
            let dir = Path::new(dir);
            for name in ["hev-socks5-tunnel.exe", "wintun.dll", "msys-2.0.dll"] {
                let path = dir.join(name);
                println!("cargo:rerun-if-changed={}", path.display());
                if !path.is_file() {
                    panic!(
                        "missing required Windows TUN sidecar asset: {}",
                        path.display()
                    );
                }
                let absolute_path = fs::canonicalize(&path).unwrap_or_else(|error| {
                    panic!(
                        "failed to resolve Windows TUN sidecar asset {}: {error}",
                        path.display()
                    )
                });
                assets.push((name, absolute_path));
            }
        }
    }

    let mut body = String::from(
        "pub struct BundledSidecarAsset {\n    pub name: &'static str,\n    pub bytes: &'static [u8],\n}\n\npub const BUNDLED_SIDECAR_ASSETS: &[BundledSidecarAsset] = &[\n",
    );
    for (name, path) in assets {
        body.push_str(&format!(
            "    BundledSidecarAsset {{ name: {:?}, bytes: include_bytes!({:?}) }},\n",
            name,
            path.display().to_string()
        ));
    }
    body.push_str("];\n");

    fs::write(out_path, body).expect("write generated sidecar assets");
}
