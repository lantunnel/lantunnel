#!/usr/bin/env bash
# Validate, upload, or download the complete public Lantunnel 2.0 R2 release.
# Usage: ./scripts/upload.sh <version> [remote|local|all|changelog|check|download]

set -euo pipefail

VERSION="${1:-}"
TARGET="${2:-remote}"

if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version> [remote|local|all|changelog|check|download]"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

# A maintainer keeps credentials in an untracked upload.env next to this
# script. Tests and CI instead pass an explicit environment, and sourcing the
# local file on top of that would silently supply values the caller left unset
# on purpose — which is exactly how a "missing bucket must fail" check passes
# on CI and fails on the maintainer's machine. UPLOAD_SKIP_ENV_FILE=1 opts out.
if [ -f upload.env ] && [ "${UPLOAD_SKIP_ENV_FILE:-0}" != "1" ]; then
    set -a
    # shellcheck disable=SC1091
    source upload.env
    set +a
fi

DIST_DIR="${DIST_DIR:-dist}"
RELEASE_DIR="${RELEASE_DIR:-dist/release}"
DOWNLOAD_DIR="${DOWNLOAD_DIR:-${RELEASE_DIR}}"
PLATFORM_DIR="${PLATFORM_DIR:-}"
CHANGELOG_FILE="${CHANGELOG_FILE:-CHANGELOG.md}"
RELEASE_PATH="releases/${VERSION}"
# The public production target must be explicit. A fallback bucket can turn a
# release typo or missing CI variable into a successful upload to the wrong
# account namespace.
R2_BUCKET_NAME="${R2_BUCKET_NAME:-}"

UPLOAD_PRODUCTS_VALUE="${UPLOAD_PRODUCTS:-client gateway admin}"
read -r -a UPLOAD_PRODUCTS_LIST <<< "$UPLOAD_PRODUCTS_VALUE"

CLIENT_SUFFIXES=(
    "windows-amd64.exe"
    "macos-amd64.dmg"
    "macos-arm64.dmg"
    "linux-amd64.AppImage"
    "linux-arm64.AppImage"
)

CLI_SUFFIXES=(
    "aarch64-apple-darwin"
    "x86_64-unknown-linux-musl"
)

REMOTE_MISSING_NAMES=()
REMOTE_UPLOAD_SNAPSHOT_DIR=""

check_var() {
    local name="$1"
    if [ -z "${!name:-}" ]; then
        echo "Error: ${name} is not set"
        exit 1
    fi
}

validate_products() {
    if [ "${#UPLOAD_PRODUCTS_LIST[@]}" -eq 0 ]; then
        echo "Error: UPLOAD_PRODUCTS is empty"
        exit 1
    fi

    local product
    for product in "${UPLOAD_PRODUCTS_LIST[@]}"; do
        case "$product" in
            client|gateway|admin) ;;
            *)
                echo "Error: unsupported upload product '${product}'"
                echo "Lantunnel 2.0 public upload supports client, gateway, and admin."
                exit 1
                ;;
        esac
    done

    local required count
    for required in client gateway admin; do
        count=0
        for product in "${UPLOAD_PRODUCTS_LIST[@]}"; do
            if [ "$product" = "$required" ]; then
                count=$((count + 1))
            fi
        done
        if [ "$count" -ne 1 ]; then
            echo "Error: UPLOAD_PRODUCTS must contain client, gateway, and admin exactly once"
            exit 1
        fi
    done
}

local_name() {
    local product="$1"
    local suffix="$2"
    echo "lantunnel-${product}-${VERSION}-${suffix}"
}

suffixes_for_product() {
    local product="$1"
    if [ "$product" = "client" ]; then
        printf '%s\n' "${CLIENT_SUFFIXES[@]}"
    else
        printf '%s\n' "${CLI_SUFFIXES[@]}"
    fi
}

find_upload_file() {
    local product="$1"
    local suffix="$2"
    local file
    file="$(local_name "$product" "$suffix")"
    if [ -f "${DOWNLOAD_DIR}/${file}" ]; then
        echo "${DOWNLOAD_DIR}/${file}"
    fi
}

remote_manifest_names() {
    local product suffix
    for product in "${UPLOAD_PRODUCTS_LIST[@]}"; do
        while IFS= read -r suffix; do
            local_name "$product" "$suffix"
        done < <(suffixes_for_product "$product")
    done
    printf '%s\n' 'checksums.txt' 'CHANGELOG.md'
}

remote_manifest_keys() {
    local name
    while IFS= read -r name; do
        printf '%s/%s\n' "$RELEASE_PATH" "$name"
    done < <(remote_manifest_names)
}

list_remote_keys() {
    local endpoint="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"
    AWS_ACCESS_KEY_ID="${R2_ACCESS_KEY_ID}" \
    AWS_SECRET_ACCESS_KEY="${R2_SECRET_ACCESS_KEY}" \
    aws s3api list-objects-v2 \
        --bucket "${R2_BUCKET_NAME}" \
        --prefix "${RELEASE_PATH}/" \
        --endpoint-url "$endpoint" \
        --region auto \
        --query 'Contents[].Key || `[]`' \
        --output text \
      | tr '\t' '\n' \
      | awk 'NF' \
      | sort -u
}

list_remote_names() {
    list_remote_keys | sed "s#^${RELEASE_PATH}/##"
}

verify_remote_manifest() {
    local phase="$1"
    local expected actual unexpected missing
    expected="$(remote_manifest_keys | sort)"
    actual="$(list_remote_keys)"
    unexpected="$(comm -13 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual"))"
    if [ -n "$unexpected" ]; then
        echo "Error: R2 ${RELEASE_PATH} contains objects outside the exact release manifest:" >&2
        printf '%s\n' "$unexpected" >&2
        return 1
    fi
    if [ "$phase" = subset ]; then
        return 0
    fi
    missing="$(comm -23 <(printf '%s\n' "$expected") <(printf '%s\n' "$actual"))"
    if [ -n "$missing" ]; then
        echo "Error: R2 ${RELEASE_PATH} is missing expected release objects:" >&2
        printf '%s\n' "$missing" >&2
        return 1
    fi
}

validate_checksum_manifest() {
    local checksum_file="${DOWNLOAD_DIR}/checksums.txt"
    local expected_count=0
    local product suffix expected matches

    for product in "${UPLOAD_PRODUCTS_LIST[@]}"; do
        while IFS= read -r suffix; do
            expected="$(local_name "$product" "$suffix")"
            matches="$(awk -v file="$expected" '$2 == file { count++ } END { print count + 0 }' "$checksum_file")"
            if [ "$matches" -ne 1 ]; then
                echo "Error: checksums.txt must contain exactly one entry for ${expected}"
                return 1
            fi
            expected_count=$((expected_count + 1))
        done < <(suffixes_for_product "$product")
    done

    matches="$(awk 'NF { count++ } END { print count + 0 }' "$checksum_file")"
    if [ "$matches" -ne "$expected_count" ]; then
        echo "Error: checksums.txt contains files outside the public artifact manifest"
        return 1
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        if ! (cd "$DOWNLOAD_DIR" && sha256sum --check --strict checksums.txt); then
            echo "Error: checksums.txt does not match the public release artifacts"
            return 1
        fi
    elif command -v shasum >/dev/null 2>&1; then
        if ! (cd "$DOWNLOAD_DIR" && shasum -a 256 --check checksums.txt); then
            echo "Error: checksums.txt does not match the public release artifacts"
            return 1
        fi
    else
        echo "Error: sha256sum or shasum is required to verify release artifacts"
        return 1
    fi
}

print_manifest() {
    local missing=0

    validate_products

    echo "Checking Lantunnel release artifacts in ${DOWNLOAD_DIR}/..."
    local product
    for product in "${UPLOAD_PRODUCTS_LIST[@]}"; do
        echo "  ${product}:"
        while IFS= read -r suffix; do
            local file
            file="$(find_upload_file "$product" "$suffix")"
            if [ -n "$file" ]; then
                echo "    [FOUND] ${file}"
            else
                echo "    [MISSING] $(local_name "$product" "$suffix")"
                missing=1
            fi
        done < <(suffixes_for_product "$product")
    done

    if [ -f "${DOWNLOAD_DIR}/checksums.txt" ]; then
        echo "  [FOUND] ${DOWNLOAD_DIR}/checksums.txt"
        if ! validate_checksum_manifest; then
            missing=1
        fi
    else
        echo "  [MISSING] ${DOWNLOAD_DIR}/checksums.txt"
        missing=1
    fi
    if [ -f "$CHANGELOG_FILE" ] && grep -Fq "## [${VERSION}]" "$CHANGELOG_FILE"; then
        echo "  [FOUND] ${CHANGELOG_FILE} (${VERSION})"
    else
        echo "  [MISSING] ${CHANGELOG_FILE} entry for ${VERSION}"
        missing=1
    fi

    if [ "$missing" -ne 0 ]; then
        echo "Error: incomplete Lantunnel 2.0 public artifact manifest."
        echo "Aggregate the nine native-OS artifacts, then run make release-all to validate them."
        exit 1
    fi
}

upload_local_file_remote() {
    local file="$1"
    local key="$2"
    local endpoint="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"

    echo "Creating immutable object: ${file} -> R2://${R2_BUCKET_NAME}/${key}"
    # Cloudflare R2 implements conditional PutObject. The wildcard condition
    # closes the list-to-create race: a concurrent publisher wins without
    # allowing this process to replace its object.
    AWS_ACCESS_KEY_ID="${R2_ACCESS_KEY_ID}" \
    AWS_SECRET_ACCESS_KEY="${R2_SECRET_ACCESS_KEY}" \
    aws s3api put-object \
        --bucket "${R2_BUCKET_NAME}" \
        --key "$key" \
        --body "$file" \
        --if-none-match '*' \
        --endpoint-url "$endpoint" \
        --region auto
}

release_source_file_for_remote_name() {
    local name="$1"
    case "$name" in
        CHANGELOG.md)
            printf '%s\n' "$CHANGELOG_FILE"
            ;;
        checksums.txt)
            printf '%s\n' "${DOWNLOAD_DIR}/checksums.txt"
            ;;
        *)
            printf '%s\n' "${DOWNLOAD_DIR}/${name}"
            ;;
    esac
}

authoritative_file_for_remote_name() {
    local name="$1"
    if [ -n "$REMOTE_UPLOAD_SNAPSHOT_DIR" ]; then
        printf '%s/%s\n' "$REMOTE_UPLOAD_SNAPSHOT_DIR" "$name"
    else
        release_source_file_for_remote_name "$name"
    fi
}

cleanup_remote_upload_snapshot() {
    if [ -n "$REMOTE_UPLOAD_SNAPSHOT_DIR" ] &&
       [ -d "$REMOTE_UPLOAD_SNAPSHOT_DIR" ]; then
        rm -rf -- "$REMOTE_UPLOAD_SNAPSHOT_DIR"
    fi
    REMOTE_UPLOAD_SNAPSHOT_DIR=""
}

create_remote_upload_snapshot() {
    local name source snapshot_download_dir snapshot_changelog_file valid
    if [ -n "$REMOTE_UPLOAD_SNAPSHOT_DIR" ]; then
        echo "Error: remote upload snapshot already exists" >&2
        return 1
    fi

    REMOTE_UPLOAD_SNAPSHOT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/lantunnel-r2-upload.XXXXXX")"
    trap cleanup_remote_upload_snapshot EXIT

    while IFS= read -r name; do
        source="$(release_source_file_for_remote_name "$name")"
        if ! cp -- "$source" "${REMOTE_UPLOAD_SNAPSHOT_DIR}/${name}"; then
            echo "Error: unable to snapshot release input: ${source}" >&2
            return 1
        fi
    done < <(remote_manifest_names)

    # Revalidate the copied artifact/checksum set so a source mutation during
    # the copy cannot produce a torn authority snapshot.
    snapshot_download_dir="$DOWNLOAD_DIR"
    snapshot_changelog_file="$CHANGELOG_FILE"
    DOWNLOAD_DIR="$REMOTE_UPLOAD_SNAPSHOT_DIR"
    CHANGELOG_FILE="${REMOTE_UPLOAD_SNAPSHOT_DIR}/CHANGELOG.md"
    valid=0
    if validate_checksum_manifest >/dev/null &&
       grep -Fq "## [${VERSION}]" "$CHANGELOG_FILE"; then
        valid=1
    fi
    DOWNLOAD_DIR="$snapshot_download_dir"
    CHANGELOG_FILE="$snapshot_changelog_file"

    if [ "$valid" -ne 1 ]; then
        echo "Error: release inputs changed while creating the immutable upload snapshot" >&2
        return 1
    fi
}

verify_release_sources_unchanged() {
    local name source snapshot
    while IFS= read -r name; do
        source="$(release_source_file_for_remote_name "$name")"
        snapshot="${REMOTE_UPLOAD_SNAPSHOT_DIR}/${name}"
        if ! cmp -s "$source" "$snapshot"; then
            echo "Error: local release input changed during remote publish: ${source}" >&2
            return 1
        fi
    done < <(remote_manifest_names)
}

download_remote_name_to_file() {
    local name="$1"
    local destination="$2"
    local endpoint="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"

    AWS_ACCESS_KEY_ID="${R2_ACCESS_KEY_ID}" \
    AWS_SECRET_ACCESS_KEY="${R2_SECRET_ACCESS_KEY}" \
    aws s3 cp "s3://${R2_BUCKET_NAME}/${RELEASE_PATH}/${name}" "$destination" \
        --endpoint-url "$endpoint" \
        --region auto
}

verify_existing_remote_content() {
    local actual compare_dir name local_file remote_file
    actual="$(list_remote_keys)"
    compare_dir="$(mktemp -d)"
    REMOTE_MISSING_NAMES=()

    # Complete this read-only pass before creating anything. This guarantees a
    # mismatched existing object cannot be discovered after a partial write.
    while IFS= read -r name; do
        if ! grep -Fxq -- "${RELEASE_PATH}/${name}" <<< "$actual"; then
            REMOTE_MISSING_NAMES+=("$name")
            continue
        fi

        local_file="$(authoritative_file_for_remote_name "$name")"
        remote_file="${compare_dir}/${name}"
        if ! download_remote_name_to_file "$name" "$remote_file" >/dev/null; then
            rm -rf -- "$compare_dir"
            echo "Error: unable to verify existing immutable R2 object: ${RELEASE_PATH}/${name}" >&2
            return 1
        fi
        if ! cmp -s "$local_file" "$remote_file"; then
            rm -rf -- "$compare_dir"
            echo "Error: immutable R2 object differs from the upload snapshot: ${RELEASE_PATH}/${name}" >&2
            return 1
        fi
    done < <(remote_manifest_names)

    rm -rf -- "$compare_dir"
}

download_remote_file() {
    local name="$1"
    download_remote_name_to_file "$name" "${DOWNLOAD_DIR}/${name}"
}

download_payload_remote() {
    check_var R2_ACCOUNT_ID
    check_var R2_ACCESS_KEY_ID
    check_var R2_SECRET_ACCESS_KEY
    check_var R2_BUCKET_NAME
    check_var PROVENANCE_CHECKSUM_FILE
    check_var PROVENANCE_CHANGELOG_FILE
    if ! command -v aws >/dev/null 2>&1; then
        echo "Error: aws cli not found. Install with: pip install awscli"
        exit 1
    fi
    if [ ! -f "$PROVENANCE_CHECKSUM_FILE" ]; then
        echo "Error: accepted checksum provenance not found: ${PROVENANCE_CHECKSUM_FILE}"
        exit 1
    fi
    if [ ! -f "$PROVENANCE_CHANGELOG_FILE" ]; then
        echo "Error: accepted changelog provenance not found: ${PROVENANCE_CHANGELOG_FILE}"
        exit 1
    fi

    verify_remote_manifest exact
    mkdir -p "$DOWNLOAD_DIR"
    if find "$DOWNLOAD_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
        echo "Error: download destination must be empty: ${DOWNLOAD_DIR}" >&2
        exit 1
    fi

    local name
    while IFS= read -r name; do
        download_remote_file "$name"
    done < <(remote_manifest_names)

    if ! cmp -s "$PROVENANCE_CHECKSUM_FILE" "${DOWNLOAD_DIR}/checksums.txt"; then
        echo "Error: R2 checksums.txt does not match the accepted release provenance" >&2
        exit 1
    fi
    if ! cmp -s "$PROVENANCE_CHANGELOG_FILE" "${DOWNLOAD_DIR}/CHANGELOG.md"; then
        echo "Error: R2 CHANGELOG.md does not match the accepted release provenance" >&2
        exit 1
    fi

    print_manifest
    verify_remote_manifest exact
}

upload_local_file_local() {
    local file="$1"
    local key="$2"

    if [ ! -d "$PLATFORM_DIR" ]; then
        echo "Error: platform directory not found: ${PLATFORM_DIR}"
        exit 1
    fi
    if ! command -v wrangler >/dev/null 2>&1; then
        echo "Error: wrangler not found. Install with: npm install -g wrangler"
        exit 1
    fi

    echo "Uploading: ${file} -> local R2://${R2_BUCKET_NAME}/${key}"
    (cd "$PLATFORM_DIR" && wrangler r2 object put "${R2_BUCKET_NAME}/${key}" --file "${REPO_ROOT}/${file}" --local)
}

upload_payload_remote() {
    check_var R2_ACCOUNT_ID
    check_var R2_ACCESS_KEY_ID
    check_var R2_SECRET_ACCESS_KEY
    check_var R2_BUCKET_NAME
    if ! command -v aws >/dev/null 2>&1; then
        echo "Error: aws cli not found. Install with: pip install awscli"
        exit 1
    fi
    create_remote_upload_snapshot

    # Existing objects may be a subset from an interrupted prior publish, but
    # anything outside this immutable product-plus-metadata manifest is unsafe
    # to overwrite or publish alongside.
    verify_remote_manifest subset
    verify_existing_remote_content

    # bash 3.2 — which is what macOS ships — treats "${arr[@]}" on an empty
    # array as an unbound variable under `set -u`. Nothing is missing whenever
    # a publish is re-run after it already uploaded everything, so that is the
    # resume path this guard keeps working.
    local name file
    if [ "${#REMOTE_MISSING_NAMES[@]}" -ne 0 ]; then
        for name in "${REMOTE_MISSING_NAMES[@]}"; do
            file="$(authoritative_file_for_remote_name "$name")"
            upload_local_file_remote "$file" "${RELEASE_PATH}/${name}"
        done
    fi
    verify_remote_manifest exact
    verify_existing_remote_content
    if [ "${#REMOTE_MISSING_NAMES[@]}" -ne 0 ]; then
        echo "Error: exact remote release changed during post-upload content verification" >&2
        return 1
    fi
    verify_remote_manifest exact
    verify_release_sources_unchanged
}

upload_payload_local() {
    check_var R2_BUCKET_NAME
    local product
    for product in "${UPLOAD_PRODUCTS_LIST[@]}"; do
        while IFS= read -r suffix; do
            local file
            file="$(find_upload_file "$product" "$suffix")"
            if [ -n "$file" ]; then
                upload_local_file_local "$file" "${RELEASE_PATH}/$(local_name "$product" "$suffix")"
            fi
        done < <(suffixes_for_product "$product")
    done

    upload_metadata_local
}

upload_metadata_local() {
    upload_local_file_local "$CHANGELOG_FILE" "${RELEASE_PATH}/CHANGELOG.md"
    if [ -f "${DOWNLOAD_DIR}/checksums.txt" ]; then
        upload_local_file_local "${DOWNLOAD_DIR}/checksums.txt" "${RELEASE_PATH}/checksums.txt"
    fi
}

list_remote() {
    echo "--- Remote Cloudflare R2 files ---"
    list_remote_names
}

case "$TARGET" in
    remote)
        print_manifest
        upload_payload_remote
        list_remote
        ;;
    local)
        print_manifest
        upload_payload_local
        ;;
    all)
        print_manifest
        upload_payload_local
        upload_payload_remote
        list_remote
        ;;
    changelog)
        print_manifest
        check_var R2_ACCOUNT_ID
        check_var R2_ACCESS_KEY_ID
        check_var R2_SECRET_ACCESS_KEY
        check_var R2_BUCKET_NAME
        if ! command -v aws >/dev/null 2>&1; then
            echo "Error: aws cli not found. Install with: pip install awscli"
            exit 1
        fi
        create_remote_upload_snapshot
        # Metadata-only repair is permitted only for an already complete exact
        # release. Published version prefixes are immutable, so a retry only
        # verifies byte identity and never overwrites metadata in place.
        verify_remote_manifest exact
        verify_existing_remote_content
        if [ "${#REMOTE_MISSING_NAMES[@]}" -ne 0 ]; then
            echo "Error: exact remote release changed during immutable metadata verification" >&2
            exit 1
        fi
        verify_remote_manifest exact
        verify_release_sources_unchanged
        list_remote
        ;;
    check)
        print_manifest
        ;;
    download)
        download_payload_remote
        ;;
    *)
        echo "Error: unknown target '${TARGET}'"
        echo "Valid targets: remote, local, all, changelog, check, download"
        exit 1
        ;;
esac

if [ "$TARGET" = "check" ]; then
    echo "Version ${VERSION} public release manifest validated."
elif [ "$TARGET" = "download" ]; then
    echo "Version ${VERSION} downloaded and verified from remote R2."
elif [ "$TARGET" = "changelog" ]; then
    echo "Version ${VERSION} changelog matches the immutable remote R2 release."
else
    echo "Version ${VERSION} uploaded to ${TARGET} R2."
fi
