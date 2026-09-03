#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAKEFILE="$ROOT_DIR/Makefile"
RELEASE_WORKFLOW="$ROOT_DIR/.github/workflows/release.yml"

while IFS= read -r workflow; do
  if grep -q -- 'anyproxy-client' "$workflow"; then
    echo "GitHub workflow still builds the removed Legacy Client: ${workflow#"$ROOT_DIR"/}" >&2
    exit 1
  fi
done < <(
  find "$ROOT_DIR/.github/workflows" -type f \
    \( -name '*.yml' -o -name '*.yaml' \) -print
)
if test -e "$ROOT_DIR/.github/workflows/e2e.yml"; then
  echo 'Legacy Rust/Go compatibility E2E workflow still exists' >&2
  exit 1
fi

# Versions are checked by crates/tp-core/tests/version_consistency.rs, which
# owns the "one version, written once" rule for every product and both lock
# files. The block that used to sit here duplicated it and had rotted three
# ways: it pinned the workspace at a literal 2.0.1, and it still read a version
# out of tauri.conf.json and out of the desktop crate's own manifest, neither
# of which declares one any more.

grep -q 'release-lantunnel-gateway-linux-amd64' "$MAKEFILE"
grep -q 'release-lantunnel-client-macos-arm64' "$MAKEFILE"
grep -q 'release-lantunnel-admin-linux-amd64' "$MAKEFILE"
grep -q '_release-lantunnel-admin-macos-arm64' "$MAKEFILE"
if grep -q '^release-anyproxy-client-' "$MAKEFILE"; then
  echo 'Makefile still publishes anyproxy-client' >&2
  exit 1
fi

release_all="$({ sed -n '/^release-all:/,/^$/p' "$MAKEFILE"; } || true)"
grep -q 'pre-aggregated' <<<"$release_all"
grep -q 'checksums' <<<"$release_all"
grep -q './scripts/upload.sh "$(UI_VERSION)" check' <<<"$release_all"
for forbidden in _release- _ensure-builder _build-ui anyproxy-client android ios; do
  if grep -q "$forbidden" <<<"$release_all"; then
    echo "release-all must only validate a pre-aggregated cross-OS bundle: $forbidden" >&2
    exit 1
  fi
done

r2_manifest="$({ sed -n '/^R2_RELEASE_FILES :=/,/^$/p' "$MAKEFILE"; } || true)"
for artifact in \
  'lantunnel-client-$(UI_VERSION)-windows-amd64.exe' \
  'lantunnel-client-$(UI_VERSION)-macos-amd64.dmg' \
  'lantunnel-client-$(UI_VERSION)-macos-arm64.dmg' \
  'lantunnel-client-$(UI_VERSION)-linux-amd64.AppImage' \
  'lantunnel-client-$(UI_VERSION)-linux-arm64.AppImage' \
  'lantunnel-client-$(VERSION)-android-arm64.apk' \
  'lantunnel-gateway-$(VERSION)-aarch64-apple-darwin' \
  'lantunnel-gateway-$(VERSION)-x86_64-unknown-linux-musl' \
  'lantunnel-admin-$(VERSION)-aarch64-apple-darwin' \
  'lantunnel-admin-$(VERSION)-x86_64-unknown-linux-musl'
do
  grep -Fq "$artifact" <<<"$r2_manifest"
done
# The Android Client ships with the rest of the release. iOS goes to the App
# Store, and the Legacy Client is gone, so neither belongs in this manifest.
for forbidden in ios anyproxy; do
  if grep -qi "$forbidden" <<<"$r2_manifest"; then
    echo "R2 manifest contains a non-V2 public artifact: $forbidden" >&2
    exit 1
  fi
done

checksums_recipe="$({ sed -n '/^checksums:/,/^$/p' "$MAKEFILE"; } || true)"
grep -q 'R2_RELEASE_FILES' <<<"$checksums_recipe"
grep -q 'checksums.txt' <<<"$checksums_recipe"
for forbidden in SHA256SUMS android ios anyproxy; do
  if grep -qi "$forbidden" <<<"$checksums_recipe"; then
    echo "V2 checksum recipe contains a non-public manifest: $forbidden" >&2
    exit 1
  fi
done

grep -q '^release-desktop-remote: release-all.*pre-aggregated' "$MAKEFILE"
grep -q '^full-release: release-all upload-remote.*pre-aggregated' "$MAKEFILE"
grep -q '^local-release: release-all upload-local.*pre-aggregated' "$MAKEFILE"

grep -q 'product: lantunnel-gateway' "$RELEASE_WORKFLOW"
grep -q 'build-client-linux:' "$RELEASE_WORKFLOW"
grep -q 'build-client-macos:' "$RELEASE_WORKFLOW"
grep -q 'build-client-windows:' "$RELEASE_WORKFLOW"
grep -q 'product: lantunnel-admin' "$RELEASE_WORKFLOW"
for forbidden in 'product: anyproxy-client' 'continue-on-error: true'; do
  if grep -q "$forbidden" "$RELEASE_WORKFLOW"; then
    echo "release workflow contains forbidden surface: $forbidden" >&2
    exit 1
  fi
done
grep -q "needs.build-client-linux.result == 'success'" "$RELEASE_WORKFLOW"
grep -q "needs.build-client-macos.result == 'success'" "$RELEASE_WORKFLOW"
grep -q "needs.build-client-windows.result == 'success'" "$RELEASE_WORKFLOW"
grep -q 'Verify and package the exact manual build set' "$RELEASE_WORKFLOW"

release_help="$(make -s -C "$ROOT_DIR" help)"
for forbidden in release-android-proxy release-ios-proxy release-mobile; do
  if grep -q "$forbidden" <<<"$release_help"; then
    echo "public make help exposes a non-V2 product: $forbidden" >&2
    exit 1
  fi
done

echo 'v2 public release surface: PASS'
