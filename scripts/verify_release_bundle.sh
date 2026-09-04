#!/usr/bin/env bash
# Verify the exact local file set accepted by the public GitHub Release flow.

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <X.Y.Z-version> <release-dir>" >&2
    exit 1
fi

version="$1"
release_dir="$2"

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "Error: release version must be stable SemVer without a v prefix: ${version}" >&2
    exit 1
fi
if [ ! -d "$release_dir" ]; then
    echo "Error: release directory not found: ${release_dir}" >&2
    exit 1
fi

artifacts=(
    "lantunnel-client-${version}-windows-amd64.exe"
    "lantunnel-client-${version}-macos-amd64.dmg"
    "lantunnel-client-${version}-macos-arm64.dmg"
    "lantunnel-client-${version}-linux-amd64.AppImage"
    "lantunnel-client-${version}-linux-arm64.AppImage"
    "lantunnel-gateway-${version}-aarch64-apple-darwin"
    "lantunnel-gateway-${version}-x86_64-unknown-linux-musl"
    "lantunnel-admin-${version}-aarch64-apple-darwin"
    "lantunnel-admin-${version}-x86_64-unknown-linux-musl"
)
expected=("${artifacts[@]}" checksums.txt CHANGELOG.md)

actual=()
while IFS= read -r file; do
    actual[${#actual[@]}]="${file##*/}"
done < <(find "$release_dir" -maxdepth 1 -type f | sort)

if [ "${#actual[@]}" -ne 11 ]; then
    echo "Error: expected exactly 11 local release files, got ${#actual[@]}" >&2
    exit 1
fi

for name in "${expected[@]}"; do
    if [ ! -f "$release_dir/$name" ]; then
        echo "Error: missing public release file: ${name}" >&2
        exit 1
    fi
done
for name in "${actual[@]}"; do
    matched=0
    for wanted in "${expected[@]}"; do
        if [ "$name" = "$wanted" ]; then
            matched=1
            break
        fi
    done
    if [ "$matched" -ne 1 ]; then
        echo "Error: unexpected public release file: ${name}" >&2
        exit 1
    fi
done

checksum_file="$release_dir/checksums.txt"
checksum_count="$(awk 'NF { count++ } END { print count + 0 }' "$checksum_file")"
if [ "$checksum_count" -ne 9 ]; then
    echo "Error: checksums.txt must contain exactly 9 non-empty entries" >&2
    exit 1
fi
if ! awk '
    NF && (NF != 2 || length($1) != 64 || $1 !~ /^[0-9a-f]+$/) { exit 1 }
' "$checksum_file"; then
    echo "Error: checksums.txt must use lowercase SHA-256 and exact filenames" >&2
    exit 1
fi
for artifact in "${artifacts[@]}"; do
    matches="$(awk -v file="$artifact" '$2 == file { count++ } END { print count + 0 }' "$checksum_file")"
    if [ "$matches" -ne 1 ]; then
        echo "Error: checksums.txt must contain exactly one entry for ${artifact}" >&2
        exit 1
    fi
done

if command -v sha256sum >/dev/null 2>&1; then
    if ! (cd "$release_dir" && sha256sum --check --strict checksums.txt >/dev/null); then
        echo "Error: checksums.txt does not match the public release files" >&2
        exit 1
    fi
elif command -v shasum >/dev/null 2>&1; then
    if ! (cd "$release_dir" && shasum -a 256 --check checksums.txt >/dev/null); then
        echo "Error: checksums.txt does not match the public release files" >&2
        exit 1
    fi
else
    echo "Error: sha256sum or shasum is required to verify release files" >&2
    exit 1
fi

changelog_heading="## [${version}]"
changelog_count="$(awk -v heading="$changelog_heading" '
    index($0, heading) == 1 &&
      (length($0) == length(heading) || substr($0, length(heading) + 1, 1) == " ") {
        count++
      }
    END { print count + 0 }
' "$release_dir/CHANGELOG.md")"
if [ "$changelog_count" -ne 1 ]; then
    echo "Error: changelog must contain exactly one version section for ${version}" >&2
    exit 1
fi

echo "Verified exact public release bundle ${version}."
