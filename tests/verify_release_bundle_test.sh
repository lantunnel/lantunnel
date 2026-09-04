#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY_BUNDLE="$ROOT_DIR/scripts/verify_release_bundle.sh"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/verify-release-bundle.XXXXXX")"
trap 'rm -rf "$TEST_DIR"' EXIT

version=2.0.0
artifacts=(
  lantunnel-client-2.0.0-windows-amd64.exe
  lantunnel-client-2.0.0-macos-amd64.dmg
  lantunnel-client-2.0.0-macos-arm64.dmg
  lantunnel-client-2.0.0-linux-amd64.AppImage
  lantunnel-client-2.0.0-linux-arm64.AppImage
  lantunnel-gateway-2.0.0-aarch64-apple-darwin
  lantunnel-gateway-2.0.0-x86_64-unknown-linux-musl
  lantunnel-admin-2.0.0-aarch64-apple-darwin
  lantunnel-admin-2.0.0-x86_64-unknown-linux-musl
)

create_bundle() {
  local directory="$1"
  mkdir -p "$directory"
  local artifact
  for artifact in "${artifacts[@]}"; do
    printf 'accepted bytes for %s\n' "$artifact" > "$directory/$artifact"
  done
  (
    cd "$directory"
    shasum -a 256 "${artifacts[@]}" > checksums.txt
  )
  printf '%s\n' \
    '# Changelog' \
    '' \
    '## [Unreleased]' \
    '' \
    '## [2.0.0] - 2026-09-04' \
    '' \
    '### Added' \
    '' \
    '- Accepted current change.' \
    '' \
    '## [1.9.9] - 2026-09-03' \
    '' \
    '- Older change.' > "$directory/CHANGELOG.md"
}

expect_failure() {
  local description="$1"
  shift
  if "$@" > "$TEST_DIR/expected-failure.log" 2>&1; then
    echo "expected bundle verifier failure: ${description}" >&2
    exit 1
  fi
}

release_dir="$TEST_DIR/release"
create_bundle "$release_dir"
"$VERIFY_BUNDLE" "$version" "$release_dir"

expect_failure 'version includes a v prefix' \
  "$VERIFY_BUNDLE" v2.0.0 "$release_dir"

missing_dir="$TEST_DIR/missing"
cp -R "$release_dir" "$missing_dir"
rm "$missing_dir/${artifacts[0]}"
expect_failure 'one public package is missing' \
  "$VERIFY_BUNDLE" "$version" "$missing_dir"

extra_dir="$TEST_DIR/extra"
cp -R "$release_dir" "$extra_dir"
printf 'unexpected\n' > "$extra_dir/lantunnel-client-2.0.0-android-arm64.apk"
expect_failure 'bundle contains an extra package' \
  "$VERIFY_BUNDLE" "$version" "$extra_dir"

tampered_dir="$TEST_DIR/tampered"
cp -R "$release_dir" "$tampered_dir"
printf 'tampered\n' >> "$tampered_dir/${artifacts[1]}"
expect_failure 'checksum does not match package bytes' \
  "$VERIFY_BUNDLE" "$version" "$tampered_dir"

duplicate_changelog_dir="$TEST_DIR/duplicate-changelog"
cp -R "$release_dir" "$duplicate_changelog_dir"
printf '%s\n' '' '## [2.0.0] - duplicate' >> "$duplicate_changelog_dir/CHANGELOG.md"
expect_failure 'changelog version section is not unique' \
  "$VERIFY_BUNDLE" "$version" "$duplicate_changelog_dir"

missing_changelog_dir="$TEST_DIR/missing-changelog"
cp -R "$release_dir" "$missing_changelog_dir"
sed 's/^## \[2\.0\.0\]/## [2.0.1]/' \
  "$missing_changelog_dir/CHANGELOG.md" > "$missing_changelog_dir/CHANGELOG.md.next"
mv "$missing_changelog_dir/CHANGELOG.md.next" "$missing_changelog_dir/CHANGELOG.md"
expect_failure 'matching changelog version section is missing' \
  "$VERIFY_BUNDLE" "$version" "$missing_changelog_dir"

echo 'local public release bundle verifier: PASS'
