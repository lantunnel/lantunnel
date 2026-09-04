#!/usr/bin/env bash
# Verify one immutable GitHub release database ID against accepted local bytes.

set -euo pipefail

if [ "$#" -ne 6 ]; then
    echo "Usage: $0 <release-id> <tag> <expected-draft:true|false> <release-dir> <expected-body-file> <fresh-verify-dir>" >&2
    exit 1
fi

release_id="$1"
tag="$2"
expected_draft="$3"
release_dir="$4"
expected_body_file="$5"
verify_dir="$6"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "$release_id" in
    '' | *[!0-9]*)
        echo "Error: invalid GitHub release database ID: ${release_id}" >&2
        exit 1
        ;;
esac
case "$expected_draft" in
    true | false) ;;
    *)
        echo "Error: expected draft state must be true or false" >&2
        exit 1
        ;;
esac
if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "Error: GitHub release tag must be stable SemVer with a v prefix: ${tag}" >&2
    exit 1
fi
version="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"

: "${GITHUB_REPOSITORY:?missing GITHUB_REPOSITORY}"
test -d "$release_dir"
test -s "$expected_body_file"
test -d "$verify_dir"
if find "$verify_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo "Error: GitHub release verification directory must be empty: ${verify_dir}" >&2
    exit 1
fi
"$root_dir/scripts/verify_release_bundle.sh" "$version" "$release_dir" >/dev/null

release_json="$verify_dir/release.json"
gh api "repos/${GITHUB_REPOSITORY}/releases/${release_id}" > "$release_json"
if ! jq -e --arg id "$release_id" '((.id | tostring) == $id)' "$release_json" >/dev/null; then
    echo "Error: GitHub release database ID response mismatch" >&2
    exit 1
fi

assets_pages_json="$verify_dir/assets-pages.json"
assets_json="$verify_dir/github-assets.json"
gh api --paginate --slurp \
    "repos/${GITHUB_REPOSITORY}/releases/${release_id}/assets?per_page=100" \
    > "$assets_pages_json"
jq '[.[][]]' "$assets_pages_json" > "$assets_json"

desired=()
while IFS= read -r asset_path; do
    desired[${#desired[@]}]="${asset_path##*/}"
done < <(find "$release_dir" -maxdepth 1 -type f | sort)
if [ "${#desired[@]}" -ne 11 ]; then
    echo "Error: expected exactly 11 local release assets, got ${#desired[@]}" >&2
    exit 1
fi

jq -n --args '$ARGS.positional | sort' "${desired[@]}" > "$verify_dir/desired-assets.json"
jq -S '[.[].name] | sort' "$assets_json" > "$verify_dir/github-asset-names.json"
diff -u "$verify_dir/desired-assets.json" "$verify_dir/github-asset-names.json"

jq -jr '.body' "$release_json" > "$verify_dir/release-notes.md"
cmp "$expected_body_file" "$verify_dir/release-notes.md"

mkdir "$verify_dir/assets"
for asset in "${desired[@]}"; do
    asset_id="$(jq -er --arg name "$asset" '
      [.[] | select(.name == $name)]
      | if length == 1 and ((.[0].id | type) == "number")
        then .[0].id
        else error("asset name does not map to exactly one numeric asset ID")
        end
    ' "$assets_json")"
    gh api \
        -H 'Accept: application/octet-stream' \
        "repos/${GITHUB_REPOSITORY}/releases/assets/${asset_id}" \
        > "$verify_dir/assets/$asset"
    cmp "$release_dir/$asset" "$verify_dir/assets/$asset"
done

current_release_json="$verify_dir/current-release.json"
current_assets_pages_json="$verify_dir/current-assets-pages.json"
current_assets_json="$verify_dir/current-assets.json"
gh api "repos/${GITHUB_REPOSITORY}/releases/${release_id}" > "$current_release_json"
gh api --paginate --slurp \
    "repos/${GITHUB_REPOSITORY}/releases/${release_id}/assets?per_page=100" \
    > "$current_assets_pages_json"
jq '[.[][]]' "$current_assets_pages_json" > "$current_assets_json"
jq -S '[.[].name] | sort' "$current_assets_json" > "$verify_dir/current-asset-names.json"
diff -u "$verify_dir/desired-assets.json" "$verify_dir/current-asset-names.json"
jq -S '[.[] | {id, name}] | sort_by(.name)' "$assets_json" > "$verify_dir/asset-identities.json"
jq -S '[.[] | {id, name}] | sort_by(.name)' "$current_assets_json" > "$verify_dir/current-asset-identities.json"
cmp "$verify_dir/asset-identities.json" "$verify_dir/current-asset-identities.json"
jq -jr '.body' "$current_release_json" > "$verify_dir/current-release-notes.md"
cmp "$expected_body_file" "$verify_dir/current-release-notes.md"

if ! jq -e \
    --arg id "$release_id" \
    --arg tag "$tag" \
    --argjson draft "$expected_draft" \
    '((.id | tostring) == $id)
      and (.tag_name == $tag)
      and (.name == $tag)
      and (.draft == $draft)
      and (.prerelease == false)
      and ((.body | type) == "string")
    ' "$current_release_json" >/dev/null; then
    echo "Error: GitHub release ID/tag/title/draft/prerelease metadata mismatch" >&2
    exit 1
fi
