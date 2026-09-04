#!/usr/bin/env bash
# Publish one exact release directory through a resumable GitHub Release draft.

set -euo pipefail

if [ "$#" -ne 5 ]; then
    echo "Usage: $0 <vX.Y.Z-tag> <expected-source-commit> <release-dir> <expected-body-file> <empty-work-dir>" >&2
    exit 1
fi

tag="$1"
expected_commit="$2"
release_dir="$3"
expected_body_file="$4"
work_dir="$5"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
verify_release="$root_dir/scripts/verify_github_release.sh"
verify_bundle="$root_dir/scripts/verify_release_bundle.sh"

if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "Error: GitHub release tag must be stable SemVer with a v prefix: ${tag}" >&2
    exit 1
fi
version="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"
if [[ ! "$expected_commit" =~ ^[0-9a-f]{40}$ ]]; then
    echo "Error: expected source commit must be an exact lowercase 40-hex commit" >&2
    exit 1
fi
: "${GITHUB_REPOSITORY:?missing GITHUB_REPOSITORY}"
api_repo="repos/${GITHUB_REPOSITORY}"
test -d "$release_dir"
test -s "$expected_body_file"
"$verify_bundle" "$version" "$release_dir" >/dev/null

if [ -e "$work_dir" ]; then
    test -d "$work_dir"
    if find "$work_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
        echo "Error: GitHub release work directory must be empty: ${work_dir}" >&2
        exit 1
    fi
else
    mkdir -p "$work_dir"
fi

desired=()
while IFS= read -r asset_path; do
    desired[${#desired[@]}]="${asset_path##*/}"
done < <(find "$release_dir" -maxdepth 1 -type f | sort)
if [ "${#desired[@]}" -ne 11 ]; then
    echo "Error: expected exactly 11 local release assets, got ${#desired[@]}" >&2
    exit 1
fi
test -f "$release_dir/CHANGELOG.md"
jq -n --args '$ARGS.positional | sort' "${desired[@]}" > "$work_dir/desired-assets.json"

tag_check_count=0
assert_remote_tag_commit() {
    local verify_ref actual_commit

    tag_check_count=$((tag_check_count + 1))
    verify_ref="refs/lantunnel-release-verification/${tag}/${tag_check_count}"
    if ! git fetch --no-tags --no-write-fetch-head origin \
        "refs/tags/${tag}:${verify_ref}"; then
        echo "Error: release tag ${tag} no longer exists" >&2
        return 1
    fi
    if ! actual_commit="$(git rev-parse --verify "${verify_ref}^{commit}")"; then
        echo "Error: release tag ${tag} does not resolve to a commit" >&2
        return 1
    fi
    if [ "$actual_commit" != "$expected_commit" ]; then
        echo "Error: release tag ${tag} no longer resolves to ${expected_commit}" >&2
        return 1
    fi
}

assert_expected_draft() {
    local current_release_json

    if ! current_release_json="$(gh api "${api_repo}/releases/${release_id}")"; then
        echo "Error: draft GitHub release ID ${release_id} is unavailable" >&2
        return 1
    fi
    if ! jq -e \
        --arg id "$release_id" \
        --arg tag "$tag" \
        --rawfile body "$expected_body_file" \
        '((.id | type) == "number")
          and ((.id | tostring) == $id)
          and (.tag_name == $tag)
          and (.name == $tag)
          and (.draft == true)
          and (.prerelease == false)
          and (.body == $body)' \
        <<<"$current_release_json" >/dev/null; then
        echo "Error: draft GitHub release ID ${release_id} changed before write" >&2
        return 1
    fi
}

assert_write_preconditions() {
    assert_remote_tag_commit
    # Keep the draft-state read closest to the write. GitHub offers no atomic
    # conditional asset upload spanning both the tag ref and release record.
    assert_expected_draft
}

# Own the source/tag check inside the publisher instead of relying only on a
# previous workflow step whose result can become stale before the first write.
assert_remote_tag_commit

releases_pages="$work_dir/releases-pages.json"
matching_releases="$work_dir/matching-releases.json"
gh api --paginate --slurp \
    "${api_repo}/releases?per_page=100" \
    > "$releases_pages"
jq --arg tag "$tag" '[.[][] | select(.tag_name == $tag)]' \
    "$releases_pages" > "$matching_releases"

release_count="$(jq -r 'length' "$matching_releases")"
release_json="$work_dir/release.json"
case "$release_count" in
    0)
        jq -n \
            --arg tag "$tag" \
            --rawfile body "$expected_body_file" \
            '{tag_name: $tag, name: $tag, body: $body, draft: true, prerelease: false}' \
            > "$work_dir/create-request.json"
        assert_remote_tag_commit
        gh api --method POST \
            --input "$work_dir/create-request.json" \
            "${api_repo}/releases" \
            > "$release_json"
        ;;
    1)
        jq '.[0]' "$matching_releases" > "$release_json"
        ;;
    *)
        echo "Error: multiple GitHub releases use tag ${tag}" >&2
        exit 1
        ;;
esac

release_id="$(jq -er '.id | select(type == "number")' "$release_json")"
case "$release_id" in
    '' | *[!0-9]*)
        echo "Error: GitHub release returned an invalid numeric database ID" >&2
        exit 1
        ;;
esac
if ! jq -e \
    --arg tag "$tag" \
    '(.tag_name == $tag)
      and (.name == $tag)
      and ((.draft | type) == "boolean")
      and (.prerelease == false)
      and ((.body | type) == "string")' \
    "$release_json" >/dev/null; then
    echo "Error: existing GitHub release metadata does not match ${tag}" >&2
    exit 1
fi
jq -jr '.body' "$release_json" > "$work_dir/release-notes.md"
cmp "$expected_body_file" "$work_dir/release-notes.md"

draft="$(jq -r '.draft' "$release_json")"
if [ "$draft" = false ]; then
    mkdir "$work_dir/verify-published"
    "$verify_release" \
        "$release_id" "$tag" false "$release_dir" "$expected_body_file" \
        "$work_dir/verify-published"
    assert_remote_tag_commit
    echo "GitHub release ${tag} is already published with the accepted bytes."
    exit 0
fi

assets_pages="$work_dir/assets-pages.json"
assets_json="$work_dir/assets.json"
gh api --paginate --slurp \
    "${api_repo}/releases/${release_id}/assets?per_page=100" \
    > "$assets_pages"
jq '[.[][]]' "$assets_pages" > "$assets_json"
if ! jq -e --slurpfile desired "$work_dir/desired-assets.json" '
    ([.[].name] | length) == ([.[].name] | unique | length)
      and all(.[]; .name as $name | ($desired[0] | index($name)) != null)
  ' "$assets_json" >/dev/null; then
    echo "Error: draft release contains duplicate or unexpected assets" >&2
    exit 1
fi

# Verify every existing byte before adding anything, so a conflicting draft is
# never made harder to inspect or recover.
mkdir "$work_dir/existing-assets"
missing=()
for asset in "${desired[@]}"; do
    asset_id="$(jq -r --arg name "$asset" '
      [.[] | select(.name == $name)]
      | if length == 0 then ""
        elif length == 1 and ((.[0].id | type) == "number") then .[0].id
        else error("asset name does not map to at most one numeric asset ID")
        end
    ' "$assets_json")"
    if [ -z "$asset_id" ]; then
        missing[${#missing[@]}]="$asset"
        continue
    fi
    gh api \
        -H 'Accept: application/octet-stream' \
        "repos/${GITHUB_REPOSITORY}/releases/assets/${asset_id}" \
        > "$work_dir/existing-assets/$asset"
    cmp "$release_dir/$asset" "$work_dir/existing-assets/$asset"
done

if [ "${#missing[@]}" -gt 0 ]; then
    for asset in "${missing[@]}"; do
        encoded_asset="$(jq -rn --arg name "$asset" '$name | @uri')"
        upload_endpoint="https://uploads.github.com/repos/${GITHUB_REPOSITORY}/releases/${release_id}/assets?name=${encoded_asset}"
        assert_write_preconditions
        gh api --method POST \
            -H 'Content-Type: application/octet-stream' \
            --input "$release_dir/$asset" \
            "$upload_endpoint" \
            > "$work_dir/upload-${asset}.json"
        if ! jq -e --arg name "$asset" \
            '((.id | type) == "number") and (.name == $name)' \
            "$work_dir/upload-${asset}.json" >/dev/null; then
            echo "Error: GitHub returned invalid upload metadata for ${asset}" >&2
            exit 1
        fi
    done
fi

mkdir "$work_dir/verify-draft"
"$verify_release" \
    "$release_id" "$tag" true "$release_dir" "$expected_body_file" \
    "$work_dir/verify-draft"

jq -n '{draft: false}' > "$work_dir/publish-request.json"
assert_write_preconditions
gh api --method PATCH \
    --input "$work_dir/publish-request.json" \
    "${api_repo}/releases/${release_id}" \
    > "$work_dir/publish-response.json"

mkdir "$work_dir/verify-published"
"$verify_release" \
    "$release_id" "$tag" false "$release_dir" "$expected_body_file" \
    "$work_dir/verify-published"
assert_remote_tag_commit

echo "Published verified GitHub release ${tag}."
