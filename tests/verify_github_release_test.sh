#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY_RELEASE="$ROOT_DIR/scripts/verify_github_release.sh"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/verify-github-release.XXXXXX")"
trap 'rm -rf "$TEST_DIR"' EXIT

release_dir="$TEST_DIR/release"
asset_dir="$TEST_DIR/github-assets"
mock_bin="$TEST_DIR/bin"
mkdir -p "$release_dir" "$asset_dir" "$mock_bin"

assets=(
  lantunnel-client-2.0.0-windows-amd64.exe
  lantunnel-client-2.0.0-macos-amd64.dmg
  lantunnel-client-2.0.0-macos-arm64.dmg
  lantunnel-client-2.0.0-linux-amd64.AppImage
  lantunnel-client-2.0.0-linux-arm64.AppImage
  lantunnel-gateway-2.0.0-aarch64-apple-darwin
  lantunnel-gateway-2.0.0-x86_64-unknown-linux-musl
  lantunnel-admin-2.0.0-aarch64-apple-darwin
  lantunnel-admin-2.0.0-x86_64-unknown-linux-musl
  checksums.txt
  CHANGELOG.md
)

for asset in "${assets[@]}"; do
  printf 'accepted bytes for %s\n' "$asset" > "$release_dir/$asset"
done
(
  cd "$release_dir"
  shasum -a 256 "${assets[@]:0:9}" > checksums.txt
)
printf '%s\n' '# Changelog' '' '## [2.0.0] - 2026-09-04' '' '- Accepted.' \
  > "$release_dir/CHANGELOG.md"
for asset in "${assets[@]}"; do
  cp "$release_dir/$asset" "$asset_dir/$asset"
done
expected_body="$TEST_DIR/expected-release-body.md"
printf '%s\n' \
  '# Lantunnel v2.0.0' \
  '' \
  'Rendered release guidance, deliberately different from CHANGELOG.md.' \
  > "$expected_body"

assets_json='[]'
asset_id=100
for asset in "${assets[@]}"; do
  assets_json="$(
    jq -c \
      --arg name "$asset" \
      --argjson id "$asset_id" \
      '. + [{id: $id, name: $name}]' <<<"$assets_json"
  )"
  asset_id=$((asset_id + 1))
done

jq -n \
  --argjson assets "$assets_json" \
  --rawfile body "$expected_body" \
  '{id: 42, tag_name: "v2.0.0", name: "v2.0.0", draft: true,
    prerelease: false, body: $body, assets: $assets}' \
  > "$TEST_DIR/draft.json"

apply_patch_json() {
  local source_json="$1"
  local filter="$2"
  local destination_json="$3"
  jq "$filter" "$source_json" > "$destination_json"
}

apply_patch_json "$TEST_DIR/draft.json" '.draft = false' "$TEST_DIR/published.json"
apply_patch_json "$TEST_DIR/draft.json" '.name = "wrong title"' "$TEST_DIR/wrong-title.json"
apply_patch_json "$TEST_DIR/draft.json" '.prerelease = true' "$TEST_DIR/prerelease.json"
apply_patch_json "$TEST_DIR/draft.json" '.body = "wrong body"' "$TEST_DIR/wrong-body.json"
apply_patch_json "$TEST_DIR/draft.json" \
  '.assets += [{id: 999, name: "unexpected.bin"}]' \
  "$TEST_DIR/extra-asset.json"

cat > "$mock_bin/gh" <<'MOCK_GH'
#!/usr/bin/env bash
set -euo pipefail

endpoint="${!#}"
printf '%s\n' "$endpoint" >> "$MOCK_GH_LOG"
case "$endpoint" in
  repos/*/releases/assets/*)
    requested_id="${endpoint##*/}"
    asset_name="$(
      jq -er --argjson id "$requested_id" \
        '.assets[] | select(.id == $id) | .name' \
        "$MOCK_RELEASE_JSON"
    )"
    cat "$MOCK_ASSET_DIR/$asset_name"
    ;;
  repos/*/releases/*/assets\?per_page=100)
    jq -c '[.assets]' "$MOCK_RELEASE_JSON"
    ;;
  repos/*/releases/*)
    cat "$MOCK_RELEASE_JSON"
    ;;
  *)
    echo "unexpected gh endpoint: ${endpoint}" >&2
    exit 64
    ;;
esac
MOCK_GH
chmod +x "$mock_bin/gh"

verify() {
  local release_json="$1"
  local github_assets="$2"
  local release_id="$3"
  local expected_draft="$4"
  local verify_dir="$5"
  mkdir "$verify_dir"
  MOCK_RELEASE_JSON="$release_json" \
    MOCK_ASSET_DIR="$github_assets" \
    MOCK_GH_LOG="$TEST_DIR/gh.log" \
    GITHUB_REPOSITORY=example/lantunnel \
    PATH="$mock_bin:$PATH" \
    "$VERIFY_RELEASE" \
      "$release_id" v2.0.0 "$expected_draft" "$release_dir" \
      "$expected_body" "$verify_dir"
}

expect_failure() {
  local description="$1"
  shift
  if "$@" > "$TEST_DIR/expected-failure.log" 2>&1; then
    echo "expected verifier failure: ${description}" >&2
    exit 1
  fi
}

: > "$TEST_DIR/gh.log"
verify "$TEST_DIR/draft.json" "$asset_dir" 42 true "$TEST_DIR/verify-draft"
test "$(wc -l < "$TEST_DIR/gh.log" | tr -d ' ')" -eq 15

: > "$TEST_DIR/gh.log"
verify "$TEST_DIR/published.json" "$asset_dir" 42 false "$TEST_DIR/verify-published"
test "$(wc -l < "$TEST_DIR/gh.log" | tr -d ' ')" -eq 15

expect_failure 'release database ID mismatch' \
  verify "$TEST_DIR/draft.json" "$asset_dir" 43 true "$TEST_DIR/verify-wrong-id"
expect_failure 'published release accepted as draft' \
  verify "$TEST_DIR/published.json" "$asset_dir" 42 true "$TEST_DIR/verify-wrong-state"
expect_failure 'title mismatch' \
  verify "$TEST_DIR/wrong-title.json" "$asset_dir" 42 true "$TEST_DIR/verify-wrong-title"
expect_failure 'prerelease metadata' \
  verify "$TEST_DIR/prerelease.json" "$asset_dir" 42 true "$TEST_DIR/verify-prerelease"
expect_failure 'release body mismatch' \
  verify "$TEST_DIR/wrong-body.json" "$asset_dir" 42 true "$TEST_DIR/verify-wrong-body"
expect_failure 'extra GitHub release asset' \
  verify "$TEST_DIR/extra-asset.json" "$asset_dir" 42 true "$TEST_DIR/verify-extra-asset"

tampered_assets="$TEST_DIR/tampered-assets"
mkdir "$tampered_assets"
cp "$asset_dir"/* "$tampered_assets/"
printf 'tampered bytes\n' > "$tampered_assets/${assets[0]}"
expect_failure 'asset byte mismatch' \
  verify "$TEST_DIR/draft.json" "$tampered_assets" 42 true "$TEST_DIR/verify-tampered"

echo 'GitHub release ID-bound verifier behavior: PASS'
