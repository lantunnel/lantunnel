#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUBLISH_RELEASE="$ROOT_DIR/scripts/publish_github_release.sh"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/publish-github-release.XXXXXX")"
trap 'rm -rf "$TEST_DIR"' EXIT

release_dir="$TEST_DIR/release"
remote_asset_dir="$TEST_DIR/remote-assets"
mock_bin="$TEST_DIR/bin"
state_json="$TEST_DIR/release-state.json"
tag_state="$TEST_DIR/tag-state"
race_marker="$TEST_DIR/race-marker"
release_get_count="$TEST_DIR/release-get-count"
fetched_commit="$TEST_DIR/fetched-commit"
mkdir -p "$release_dir" "$remote_asset_dir" "$mock_bin"

expected_commit=0123456789abcdef0123456789abcdef01234567
moved_commit=89abcdef0123456789abcdef0123456789abcdef

reset_race_state() {
  printf 'annotated:%s\n' "$expected_commit" > "$tag_state"
  rm -f "$race_marker" "$release_get_count" "$fetched_commit"
}

use_lightweight_tag() {
  printf 'lightweight:%s\n' "$expected_commit" > "$tag_state"
}

reset_race_state

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
expected_body="$TEST_DIR/expected-release-body.md"
printf '%s\n' \
  '# Lantunnel v2.0.0' \
  '' \
  'Rendered release guidance, deliberately different from CHANGELOG.md.' \
  > "$expected_body"

write_release_state() {
  local draft="$1"
  local asset_count="$2"
  local assets_json='[]'
  local index
  find "$remote_asset_dir" -mindepth 1 -maxdepth 1 -type f -delete
  for ((index = 0; index < asset_count; index++)); do
    cp "$release_dir/${assets[$index]}" "$remote_asset_dir/${assets[$index]}"
    assets_json="$(
      jq -c \
        --arg name "${assets[$index]}" \
        --argjson id "$((100 + index))" \
        '. + [{id: $id, name: $name}]' <<<"$assets_json"
    )"
  done
  jq -n \
    --argjson draft "$draft" \
    --argjson assets "$assets_json" \
    --rawfile body "$expected_body" \
    '{id: 42, tag_name: "v2.0.0", name: "v2.0.0", draft: $draft,
      prerelease: false, body: $body, assets: $assets}' \
    > "$state_json"
}

cat > "$mock_bin/gh" <<'MOCK_GH'
#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$1" != api ]; then
  echo 'mock gh accepts only API calls' >&2
  exit 64
fi
shift

method=GET
input=
endpoint=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --method)
      method="$2"
      shift 2
      ;;
    --input)
      input="$2"
      shift 2
      ;;
    -H)
      shift 2
      ;;
    --paginate | --slurp)
      shift
      ;;
    *)
      endpoint="$1"
      shift
      ;;
  esac
done
printf '%s %s\n' "$method" "$endpoint" >> "$MOCK_GH_LOG"

repo="repos/${GITHUB_REPOSITORY}"
if [ "$method" = GET ] && [ "$endpoint" = "$repo/releases?per_page=100" ]; then
  if [ -f "$MOCK_RELEASE_STATE" ]; then
    jq -c '[[.]]' "$MOCK_RELEASE_STATE"
  else
    printf '[[]]\n'
  fi
  exit 0
fi

if [ "$method" = POST ] && [ "$endpoint" = "$repo/releases" ]; then
  test ! -e "$MOCK_RELEASE_STATE"
  temporary_state="${MOCK_RELEASE_STATE}.tmp"
  jq '. + {id: 42, assets: []}' "$input" > "$temporary_state"
  mv "$temporary_state" "$MOCK_RELEASE_STATE"
  cat "$MOCK_RELEASE_STATE"
  exit 0
fi

if [ "$method" = GET ] && [ "$endpoint" = "$repo/releases/42/assets?per_page=100" ]; then
  jq -c '[.assets]' "$MOCK_RELEASE_STATE"
  exit 0
fi

case "$method $endpoint" in
  "GET $repo/releases/assets/"*)
    requested_id="${endpoint##*/}"
    asset_name="$(
      jq -er --argjson id "$requested_id" \
        '.assets[] | select(.id == $id) | .name' \
        "$MOCK_RELEASE_STATE"
    )"
    cat "$MOCK_REMOTE_ASSET_DIR/$asset_name"
    exit 0
    ;;
  "GET $repo/releases/42")
    cat "$MOCK_RELEASE_STATE"
    if [ "${MOCK_AFTER_DRAFT_VERIFY_RACE:-}" = publish ]; then
      count=0
      if [ -f "$MOCK_RELEASE_GET_COUNT" ]; then
        read -r count < "$MOCK_RELEASE_GET_COUNT"
      fi
      count=$((count + 1))
      printf '%s\n' "$count" > "$MOCK_RELEASE_GET_COUNT"
      if [ "$count" -eq 2 ]; then
        temporary_state="${MOCK_RELEASE_STATE}.tmp"
        jq '.draft = false' "$MOCK_RELEASE_STATE" > "$temporary_state"
        mv "$temporary_state" "$MOCK_RELEASE_STATE"
        printf 'RACE draft-publish\n' >> "$MOCK_GH_LOG"
      fi
    fi
    exit 0
    ;;
  "PATCH $repo/releases/42")
    jq -e '.draft == false' "$input" >/dev/null
    jq -e '.draft == true' "$MOCK_RELEASE_STATE" >/dev/null
    temporary_state="${MOCK_RELEASE_STATE}.tmp"
    jq '.draft = false' "$MOCK_RELEASE_STATE" > "$temporary_state"
    mv "$temporary_state" "$MOCK_RELEASE_STATE"
    cat "$MOCK_RELEASE_STATE"
    exit 0
    ;;
esac

case "$endpoint" in
  https://uploads.github.com/*/releases/42/assets\?name=*)
    test "$method" = POST
    jq -e '.draft == true' "$MOCK_RELEASE_STATE" >/dev/null
    asset_name="${endpoint##*=}"
    test -f "$input"
    if jq -e --arg name "$asset_name" '.assets[] | select(.name == $name)' \
        "$MOCK_RELEASE_STATE" >/dev/null; then
      echo "duplicate mock asset: ${asset_name}" >&2
      exit 65
    fi
    asset_id="$(jq -r '([.assets[].id] | max // 99) + 1' "$MOCK_RELEASE_STATE")"
    cp "$input" "$MOCK_REMOTE_ASSET_DIR/$asset_name"
    temporary_state="${MOCK_RELEASE_STATE}.tmp"
    jq --arg name "$asset_name" --argjson id "$asset_id" \
      '.assets += [{id: $id, name: $name}]' \
      "$MOCK_RELEASE_STATE" > "$temporary_state"
    mv "$temporary_state" "$MOCK_RELEASE_STATE"
    if [ -n "${MOCK_AFTER_UPLOAD_RACE:-}" ] && [ ! -e "$MOCK_RACE_MARKER" ]; then
      case "$MOCK_AFTER_UPLOAD_RACE" in
        tag-move | tag-delete)
          if [ "$MOCK_AFTER_UPLOAD_RACE" = tag-move ]; then
            printf 'annotated:%s\n' "$MOCK_MOVED_COMMIT" > "$MOCK_TAG_STATE"
          else
            printf 'deleted\n' > "$MOCK_TAG_STATE"
          fi
          printf 'triggered\n' > "$MOCK_RACE_MARKER"
          printf 'RACE %s\n' "$MOCK_AFTER_UPLOAD_RACE" >> "$MOCK_GH_LOG"
          ;;
        draft-publish)
          temporary_state="${MOCK_RELEASE_STATE}.tmp"
          jq '.draft = false' "$MOCK_RELEASE_STATE" > "$temporary_state"
          mv "$temporary_state" "$MOCK_RELEASE_STATE"
          printf 'triggered\n' > "$MOCK_RACE_MARKER"
          printf 'RACE draft-publish\n' >> "$MOCK_GH_LOG"
          ;;
        *)
          echo "unsupported mock race: ${MOCK_AFTER_UPLOAD_RACE}" >&2
          exit 64
          ;;
      esac
    fi
    jq -n --arg name "$asset_name" --argjson id "$asset_id" \
      '{id: $id, name: $name}'
    exit 0
    ;;
esac

echo "unexpected gh request: ${method} ${endpoint}" >&2
exit 64
MOCK_GH
chmod +x "$mock_bin/gh"

cat > "$mock_bin/git" <<'MOCK_GIT'
#!/usr/bin/env bash
set -euo pipefail

printf 'TAG-CHECK %s\n' "$*" >> "$MOCK_GH_LOG"
case "${1:-}" in
  fetch)
    test "$2" = --no-tags
    test "$3" = --no-write-fetch-head
    test "$4" = origin
    [[ "$5" = refs/tags/v2.0.0:refs/lantunnel-release-verification/v2.0.0/* ]]
    read -r tag_target < "$MOCK_TAG_STATE"
    if [ "$tag_target" = deleted ]; then
      echo 'mock remote tag does not exist' >&2
      exit 1
    fi
    case "$tag_target" in
      annotated:* | lightweight:*)
        printf '%s\n' "${tag_target#*:}" > "$MOCK_FETCHED_COMMIT"
        ;;
      *)
        echo "invalid mock tag state: ${tag_target}" >&2
        exit 64
        ;;
    esac
    ;;
  rev-parse)
    test "$2" = --verify
    [[ "$3" = refs/lantunnel-release-verification/v2.0.0/*\^\{commit\} ]]
    cat "$MOCK_FETCHED_COMMIT"
    ;;
  *)
    echo "unexpected git request: $*" >&2
    exit 64
    ;;
esac
MOCK_GIT
chmod +x "$mock_bin/git"

publish() {
  local work_dir="$1"
  MOCK_GH_LOG="$TEST_DIR/gh.log" \
    MOCK_RELEASE_STATE="$state_json" \
    MOCK_REMOTE_ASSET_DIR="$remote_asset_dir" \
    MOCK_TAG_STATE="$tag_state" \
    MOCK_RACE_MARKER="$race_marker" \
    MOCK_RELEASE_GET_COUNT="$release_get_count" \
    MOCK_FETCHED_COMMIT="$fetched_commit" \
    MOCK_MOVED_COMMIT="$moved_commit" \
    MOCK_AFTER_UPLOAD_RACE="${MOCK_AFTER_UPLOAD_RACE:-}" \
    MOCK_AFTER_DRAFT_VERIFY_RACE="${MOCK_AFTER_DRAFT_VERIFY_RACE:-}" \
    GITHUB_REPOSITORY=example/lantunnel \
    PATH="$mock_bin:$PATH" \
    "$PUBLISH_RELEASE" \
      v2.0.0 "$expected_commit" "$release_dir" "$expected_body" "$work_dir"
}

expect_failure() {
  local description="$1"
  shift
  if "$@" > "$TEST_DIR/expected-failure.log" 2>&1; then
    echo "expected publisher failure: ${description}" >&2
    exit 1
  fi
}

assert_no_mutation_after_race() {
  if sed -n '/^RACE /,$p' "$TEST_DIR/gh.log" | grep -Eq '^(POST|PATCH|DELETE) '; then
    echo 'publisher mutated GitHub after observing an injected race' >&2
    exit 1
  fi
}

# A fresh tag creates a draft, uploads the exact set, verifies it, publishes it,
# and verifies the published bytes without any destructive API operation.
: > "$TEST_DIR/gh.log"
publish "$TEST_DIR/work-new"
jq -e '.draft == false and (.assets | length) == 11' "$state_json" >/dev/null
test "$(grep -Ec '^POST repos/.*/releases$' "$TEST_DIR/gh.log")" -eq 1
test "$(grep -Ec '^POST https://uploads.github.com/' "$TEST_DIR/gh.log")" -eq 11
test "$(grep -Ec '^PATCH repos/.*/releases/42$' "$TEST_DIR/gh.log")" -eq 1
test "$(grep -E '^(POST|PATCH|DELETE) ' "$TEST_DIR/gh.log" | tail -n 1)" = \
  'PATCH repos/example/lantunnel/releases/42'
if grep -Eq 'DELETE|clobber' "$TEST_DIR/gh.log"; then
  echo 'publisher used a destructive GitHub operation' >&2
  exit 1
fi

# Rerunning an already published, byte-identical release performs only reads.
: > "$TEST_DIR/gh.log"
publish "$TEST_DIR/work-published"
if grep -Eq '^(POST|PATCH|DELETE) ' "$TEST_DIR/gh.log"; then
  echo 'published release rerun attempted to mutate GitHub' >&2
  exit 1
fi

# A lightweight release tag resolves directly to the same expected commit.
use_lightweight_tag
: > "$TEST_DIR/gh.log"
publish "$TEST_DIR/work-lightweight"
if grep -Eq '^(POST|PATCH|DELETE) ' "$TEST_DIR/gh.log"; then
  echo 'lightweight tag validation mutated an accepted published release' >&2
  exit 1
fi
reset_race_state

# An interrupted draft is resumed by adding only missing accepted assets.
write_release_state true 4
: > "$TEST_DIR/gh.log"
publish "$TEST_DIR/work-partial"
jq -e '.draft == false and (.assets | length) == 11' "$state_json" >/dev/null
test "$(grep -Ec '^POST https://uploads.github.com/' "$TEST_DIR/gh.log")" -eq 7
test "$(grep -Ec '^POST repos/.*/releases$' "$TEST_DIR/gh.log" || true)" -eq 0

# A conflicting existing byte fails before any upload or publish operation.
write_release_state true 4
printf 'tampered remote bytes\n' > "$remote_asset_dir/${assets[1]}"
: > "$TEST_DIR/gh.log"
expect_failure 'draft asset byte mismatch' publish "$TEST_DIR/work-conflict"
jq -e '.draft == true and (.assets | length) == 4' "$state_json" >/dev/null
if grep -Eq '^(POST|PATCH|DELETE) ' "$TEST_DIR/gh.log"; then
  echo 'conflicting draft was mutated' >&2
  exit 1
fi

# An unexpected draft asset also fails before any mutation.
write_release_state true 4
temporary_state="$TEST_DIR/release-state-extra.json"
jq '.assets += [{id: 999, name: "unexpected.bin"}]' \
  "$state_json" > "$temporary_state"
mv "$temporary_state" "$state_json"
: > "$TEST_DIR/gh.log"
expect_failure 'unexpected draft asset' publish "$TEST_DIR/work-extra"
jq -e '.draft == true and (.assets | length) == 5' "$state_json" >/dev/null
if grep -Eq '^(POST|PATCH|DELETE) ' "$TEST_DIR/gh.log"; then
  echo 'draft with an unexpected asset was mutated' >&2
  exit 1
fi

# A moved annotated tag is detected before the next missing asset is written.
write_release_state true 4
reset_race_state
: > "$TEST_DIR/gh.log"
MOCK_AFTER_UPLOAD_RACE=tag-move
expect_failure 'annotated tag moved during uploads' publish "$TEST_DIR/work-tag-move"
unset MOCK_AFTER_UPLOAD_RACE
grep -Fq 'no longer resolves to' "$TEST_DIR/expected-failure.log"
test "$(grep -Ec '^POST https://uploads.github.com/' "$TEST_DIR/gh.log")" -eq 1
test "$(grep -Ec '^PATCH repos/.*/releases/42$' "$TEST_DIR/gh.log" || true)" -eq 0
assert_no_mutation_after_race

# A deleted tag after the final missing upload is detected before publication.
write_release_state true 10
reset_race_state
: > "$TEST_DIR/gh.log"
MOCK_AFTER_UPLOAD_RACE=tag-delete
expect_failure 'annotated tag deleted before publish' publish "$TEST_DIR/work-tag-delete"
unset MOCK_AFTER_UPLOAD_RACE
grep -Fq 'no longer exists' "$TEST_DIR/expected-failure.log"
test "$(grep -Ec '^POST https://uploads.github.com/' "$TEST_DIR/gh.log")" -eq 1
test "$(grep -Ec '^PATCH repos/.*/releases/42$' "$TEST_DIR/gh.log" || true)" -eq 0
assert_no_mutation_after_race

# A draft published by another actor stops the remaining asset uploads.
write_release_state true 4
reset_race_state
: > "$TEST_DIR/gh.log"
MOCK_AFTER_UPLOAD_RACE=draft-publish
expect_failure 'draft published during uploads' publish "$TEST_DIR/work-draft-upload-published"
unset MOCK_AFTER_UPLOAD_RACE
grep -Fq 'changed before write' "$TEST_DIR/expected-failure.log"
jq -e '.draft == false and (.assets | length) == 5' "$state_json" >/dev/null
test "$(grep -Ec '^POST https://uploads.github.com/' "$TEST_DIR/gh.log")" -eq 1
test "$(grep -Ec '^PATCH repos/.*/releases/42$' "$TEST_DIR/gh.log" || true)" -eq 0
assert_no_mutation_after_race

# Publication by another actor after draft verification is observed through
# the numeric release ID immediately before our PATCH, so we perform no write.
write_release_state true 11
reset_race_state
: > "$TEST_DIR/gh.log"
MOCK_AFTER_DRAFT_VERIFY_RACE=publish
expect_failure 'draft published concurrently' publish "$TEST_DIR/work-draft-published"
unset MOCK_AFTER_DRAFT_VERIFY_RACE
grep -Fq 'changed before write' "$TEST_DIR/expected-failure.log"
if ! jq -e '.draft == false and (.assets | length) == 11' "$state_json" >/dev/null; then
  echo 'concurrent publisher did not leave the complete release published' >&2
  jq '{draft, asset_count: (.assets | length)}' "$state_json" >&2
  cat "$TEST_DIR/expected-failure.log" >&2
  exit 1
fi
if grep -Eq '^(POST|PATCH|DELETE) ' "$TEST_DIR/gh.log"; then
  echo 'concurrently published release was mutated' >&2
  exit 1
fi
assert_no_mutation_after_race

echo 'resumable GitHub release publisher behavior: PASS'
