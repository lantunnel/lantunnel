#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RENDER_NOTES="$ROOT_DIR/scripts/render_github_release_notes.sh"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/render-github-release-notes.XXXXXX")"
trap 'rm -rf "$TEST_DIR"' EXIT

tag=v2.0.0
version=2.0.0
source_commit=0123456789abcdef0123456789abcdef01234567
repository=example/lantunnel
release_dir="$TEST_DIR/release"
mkdir -p "$release_dir"

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
for artifact in "${artifacts[@]}"; do
  printf 'accepted bytes for %s\n' "$artifact" > "$release_dir/$artifact"
done
(
  cd "$release_dir"
  shasum -a 256 "${artifacts[@]}" > checksums.txt
)
printf '%s\n' \
  '# Changelog' \
  '' \
  '## [Unreleased]' \
  '' \
  '- FUTURE-ONLY-MARKER' \
  '' \
  '## [2.0.0] - 2026-09-04' \
  '' \
  'A CURRENT-ONLY-MARKER introduction.' \
  '' \
  '### Added' \
  '' \
  '- The current release feature.' \
  '' \
  '## [1.9.9] - 2026-09-03' \
  '' \
  '- OLD-ONLY-MARKER' > "$release_dir/CHANGELOG.md"

notes_one="$TEST_DIR/release-notes-one.md"
notes_two="$TEST_DIR/release-notes-two.md"
"$RENDER_NOTES" "$tag" "$source_commit" "$repository" "$release_dir" "$notes_one"
"$RENDER_NOTES" "$tag" "$source_commit" "$repository" "$release_dir" "$notes_two"
cmp "$notes_one" "$notes_two"

grep -Fq '# Lantunnel v2.0.0' "$notes_one"
grep -Fq "https://github.com/${repository}/commit/${source_commit}" "$notes_one"
grep -Fq '## Choose a download' "$notes_one"
grep -Fq '### Client — connect this device' "$notes_one"
grep -Fq '### Gateway — relay and coordinate a Tunnel' "$notes_one"
grep -Fq '### Admin — create independent Tunnel files' "$notes_one"
grep -Fq 'Client is the right download for most people.' "$notes_one"
grep -Fq 'Use the same Client program on every Peer, with the default desktop UI or `--headless`.' "$notes_one"
grep -Fq 'Install Gateway only when you operate an independent or Platform-connected Gateway host.' "$notes_one"
grep -Fq 'Admin is only for offline provisioning with an independent Gateway' "$notes_one"
grep -Fq 'Connected Gateway and Lantunnel Gateway modes do not use it.' "$notes_one"
for artifact in "${artifacts[@]}"; do
  grep -Fq "https://github.com/${repository}/releases/download/${tag}/${artifact}" "$notes_one"
done
grep -Fq "https://github.com/${repository}/releases/download/${tag}/checksums.txt" "$notes_one"
grep -Fq "https://github.com/${repository}/releases/download/${tag}/CHANGELOG.md" "$notes_one"

grep -Fq 'macOS Client DMGs are Developer ID signed, notarized, and stapled.' "$notes_one"
grep -Fq 'Windows Client executable is an intentionally unsigned preview' "$notes_one"
grep -Fq 'Gateway and Admin command-line binaries and Linux AppImages are not code-signed.' "$notes_one"
grep -Fq 'macOS Gateway and Admin CLI binaries are unsigned and not notarized.' "$notes_one"
grep -Fq "https://github.com/${repository}#building-from-source" "$notes_one"
grep -Fq 'Do not bypass Gatekeeper or an organization policy.' "$notes_one"
grep -Fq '## Install' "$notes_one"
grep -Fq '## System requirements' "$notes_one"
grep -Fq 'Windows 10 or later' "$notes_one"
grep -Fq 'macOS 10.15 Catalina or later' "$notes_one"
grep -Fq 'macOS 11 Big Sur or later' "$notes_one"
grep -Fq 'GTK 3 and WebKitGTK 4.1' "$notes_one"
grep -Fq '## Verify SHA-256' "$notes_one"
grep -Fq 'sha256sum --check --strict' "$notes_one"
grep -Fq 'shasum -a 256 --check' "$notes_one"
grep -Fq 'Get-FileHash' "$notes_one"
grep -Fq '## Choose an installation mode' "$notes_one"
for mode_link in \
  '[My Gateway](https://lantunnel.app/docs/installation#own-independent)' \
  "[Friend's Gateway](https://lantunnel.app/docs/installation#friend-independent)" \
  '[Connected Gateway](https://lantunnel.app/docs/installation#platform-connected)' \
  '[Lantunnel Gateway](https://lantunnel.app/docs/installation#lantunnel-provided)'
do
  grep -Fq "$mode_link" "$notes_one"
done
grep -Fq 'https://lantunnel.app/docs/quickstart' "$notes_one"
grep -Fq '## Lantunnel Gateway quick start' "$notes_one"
grep -Fq 'https://lantunnel.app/register' "$notes_one"
grep -Fq 'Create a separate Peer for every device' "$notes_one"
grep -Fq 'private `.peer` profile' "$notes_one"
grep -Fq 'connect with its Tunnel ID' "$notes_one"
grep -Fq 'prefers a Direct path and falls back to Encrypted Relay' "$notes_one"
grep -Fq '## What changed in 2.0.0' "$notes_one"
grep -Fq 'CURRENT-ONLY-MARKER' "$notes_one"
if grep -Eq 'FUTURE-ONLY-MARKER|OLD-ONLY-MARKER|downloads\.lantunnel\.app|Cloudflare R2' "$notes_one"; then
  echo 'release notes contain another version or the retired download store' >&2
  exit 1
fi

expect_failure() {
  local description="$1"
  shift
  if "$@" > "$TEST_DIR/expected-failure.log" 2>&1; then
    echo "expected release-note renderer failure: ${description}" >&2
    exit 1
  fi
}

expect_failure 'tag is not stable SemVer' \
  "$RENDER_NOTES" latest "$source_commit" "$repository" "$release_dir" "$TEST_DIR/invalid-tag.md"
expect_failure 'source commit is not exact lowercase 40-hex' \
  "$RENDER_NOTES" "$tag" deadbeef "$repository" "$release_dir" "$TEST_DIR/invalid-commit.md"
expect_failure 'repository is not owner/name' \
  "$RENDER_NOTES" "$tag" "$source_commit" invalid "$release_dir" "$TEST_DIR/invalid-repo.md"

duplicate_dir="$TEST_DIR/duplicate"
cp -R "$release_dir" "$duplicate_dir"
printf '%s\n' '' '## [2.0.0] - duplicate' >> "$duplicate_dir/CHANGELOG.md"
expect_failure 'matching changelog section is duplicated' \
  "$RENDER_NOTES" "$tag" "$source_commit" "$repository" "$duplicate_dir" "$TEST_DIR/duplicate.md"

echo 'deterministic GitHub release-note renderer: PASS'
