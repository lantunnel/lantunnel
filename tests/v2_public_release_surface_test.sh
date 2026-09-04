#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAKEFILE="$ROOT_DIR/Makefile"
RELEASE_WORKFLOW="$ROOT_DIR/.github/workflows/release.yml"
VERIFY_BUNDLE="$ROOT_DIR/scripts/verify_release_bundle.sh"

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
grep -q './scripts/verify_release_bundle.sh "$(UI_VERSION)" "$(RELEASE_DIR)"' <<<"$release_all"
for forbidden in _release- _ensure-builder _build-ui anyproxy-client android ios; do
  if grep -q "$forbidden" <<<"$release_all"; then
    echo "release-all must only validate a pre-aggregated cross-OS bundle: $forbidden" >&2
    exit 1
  fi
done

public_manifest="$({ sed -n '/^PUBLIC_RELEASE_FILES :=/,/^$/p' "$MAKEFILE"; } || true)"
for artifact in \
  'lantunnel-client-$(UI_VERSION)-windows-amd64.exe' \
  'lantunnel-client-$(UI_VERSION)-macos-amd64.dmg' \
  'lantunnel-client-$(UI_VERSION)-macos-arm64.dmg' \
  'lantunnel-client-$(UI_VERSION)-linux-amd64.AppImage' \
  'lantunnel-client-$(UI_VERSION)-linux-arm64.AppImage' \
  'lantunnel-gateway-$(VERSION)-aarch64-apple-darwin' \
  'lantunnel-gateway-$(VERSION)-x86_64-unknown-linux-musl' \
  'lantunnel-admin-$(VERSION)-aarch64-apple-darwin' \
  'lantunnel-admin-$(VERSION)-x86_64-unknown-linux-musl'
do
  grep -Fq "$artifact" <<<"$public_manifest"
done
# Mobile Clients ship on their own cadence, and the Legacy Client is gone, so
# none belongs in this native desktop/CLI manifest.
for forbidden in android ios anyproxy; do
  if grep -qi "$forbidden" <<<"$public_manifest"; then
    echo "public manifest contains a non-V2 public artifact: $forbidden" >&2
    exit 1
  fi
done

checksums_recipe="$({ sed -n '/^checksums:/,/^$/p' "$MAKEFILE"; } || true)"
grep -q 'CHECKSUM_FILES' <<<"$checksums_recipe"
grep -q 'checksums.txt' <<<"$checksums_recipe"
grep -Fq 'CHECKSUM_FILES ?= $(PUBLIC_RELEASE_FILES)' "$MAKEFILE"
for forbidden in SHA256SUMS android ios anyproxy; do
  if grep -qi "$forbidden" <<<"$checksums_recipe"; then
    echo "V2 checksum recipe contains a non-public manifest: $forbidden" >&2
    exit 1
  fi
done

test -x "$VERIFY_BUNDLE"
test ! -e "$ROOT_DIR/scripts/upload.sh"
test ! -e "$ROOT_DIR/tests/upload_script_test.sh"
test ! -e "$ROOT_DIR/tests/download_existing_release_test.sh"
for retired_target in \
  release-desktop-remote upload upload-local upload-remote upload-all \
  upload-changelog full-release local-release
do
  if grep -Eq "^${retired_target}:" "$MAKEFILE"; then
    echo "Makefile still exposes retired R2 target: ${retired_target}" >&2
    exit 1
  fi
done
if grep -Eq 'UPLOAD_ENV|R2_RELEASE_FILES|scripts/upload\.sh|R2_|Cloudflare R2' \
    "$MAKEFILE" "$RELEASE_WORKFLOW"; then
  echo 'active release configuration still contains an R2 publishing path' >&2
  exit 1
fi

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
