#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAKEFILE="$ROOT_DIR/Makefile"
BUILD_RS="$ROOT_DIR/apps/lantunnel-client/src-tauri/build.rs"
WINDOWS_ADMIN_MANIFEST="$ROOT_DIR/apps/lantunnel-client/src-tauri/windows/require-administrator.manifest.xml"
DESKTOP_TUN="$ROOT_DIR/apps/lantunnel-client/src-tauri/src/desktop_tun.rs"
MAIN_RS="$ROOT_DIR/apps/lantunnel-client/src-tauri/src/main.rs"

grep -q "_prepare-tun-sidecar:" "$MAKEFILE"
grep -q "_prepare-tun-sidecar-linux:" "$MAKEFILE"
grep -q "TUN_SIDECAR_PREBUILT_DIR" "$MAKEFILE"
grep -q "TUN_SIDECAR_BUILD_LOCK" "$MAKEFILE"
grep -q "x86_64-pc-windows-msvc" "$MAKEFILE"
grep -q "hev-socks5-tunnel.exe" "$MAKEFILE"
grep -q "wintun.dll" "$MAKEFILE"
grep -q "msys-2.0.dll" "$MAKEFILE"
grep -q 'Place it next to hev-socks5-tunnel.exe or set TUN_SIDECAR_PREBUILT_DIR' "$MAKEFILE"
grep -q 'TUN_SIDECAR_TARGET_DIR = $(TUN_SIDECAR_GEN_DIR)/$(TRIPLE)' "$MAKEFILE"
grep -q 'TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel' "$MAKEFILE"
grep -q 'gen/tun-sidecar/' "$MAKEFILE"
if grep -q 'rm -f apps/lantunnel-client/src-tauri/.tauri-build-override-\*.json' "$MAKEFILE"; then
  echo 'release cleanup must not delete another concurrent Tauri override' >&2
  exit 1
fi
grep -q 'lantunnel-client-$(UI_VERSION)-windows-amd64.exe' "$MAKEFILE"
grep -q "LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR" "$MAKEFILE"
if grep -q "zip -q" "$MAKEFILE"; then
  echo 'release packaging must not silently overwrite zip entries' >&2
  exit 1
fi
grep -q 'msys-2.0.dll":"msys-2.0.dll' "$MAKEFILE"
grep -q "__TAURI_RESOURCES__" "$MAKEFILE"
# tauri.conf.json deliberately declares no version: Tauri falls back to the
# crate's, which inherits the workspace's. crates/tp-core/tests/
# version_consistency.rs owns that rule and asserts the field is absent, which
# is the opposite of what this line used to require.

test -f "$WINDOWS_ADMIN_MANIFEST"
grep -q 'requestedExecutionLevel level="requireAdministrator" uiAccess="false"' "$WINDOWS_ADMIN_MANIFEST"
grep -q 'Microsoft.Windows.Common-Controls' "$WINDOWS_ADMIN_MANIFEST"
grep -q 'require-administrator.manifest.xml' "$BUILD_RS"
grep -q 'WindowsAttributes::new().app_manifest' "$BUILD_RS"
grep -q 'LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR' "$BUILD_RS"
grep -Fq 'fs::canonicalize(&path)' "$BUILD_RS"
grep -Fq 'assets.push((name, absolute_path));' "$BUILD_RS"
if grep -Fq 'assets.push((name, path));' "$BUILD_RS"; then
  echo 'generated sidecar assets still embed a path relative to OUT_DIR' >&2
  exit 1
fi

grep -q "resource_dir: Option<PathBuf>" "$DESKTOP_TUN"
grep -q "ensure_bundled_sidecars(&config.config_dir)" "$DESKTOP_TUN"
grep -q "resolve_sidecar_binary(" "$DESKTOP_TUN"
grep -q "resource_dir.join(name)" "$DESKTOP_TUN"
grep -q "app.path().resource_dir().ok()" "$MAIN_RS"
grep -q "desktop LAN routes via TUN failed; keeping SOCKS5 proxy connected" "$MAIN_RS"
if grep -A12 "if should_run_desktop_tun(&settings)" "$MAIN_RS" | grep -q "stop_local_proxy_task(&local_proxy_slot).await;"; then
  echo 'TUN apply failure must not stop the local SOCKS proxy' >&2
  exit 1
fi
