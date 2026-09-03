#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Drive upload.sh from an explicit environment: a maintainer's untracked
# upload.env must not decide whether these assertions hold.
export UPLOAD_SKIP_ENV_FILE=1
cd "$ROOT_DIR"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

version="2.0.0"
r2_root="$tmp_dir/r2"
r2_release="$r2_root/releases/$version"
download_dir="$tmp_dir/download"
bin_dir="$tmp_dir/bin"
aws_log="$tmp_dir/aws.log"
provenance_checksums="$tmp_dir/accepted-checksums.txt"
provenance_changelog="$tmp_dir/accepted-CHANGELOG.md"

mkdir -p "$r2_release" "$download_dir" "$bin_dir"

artifacts=(
  "lantunnel-client-${version}-windows-amd64.exe"
  "lantunnel-client-${version}-macos-amd64.dmg"
  "lantunnel-client-${version}-macos-arm64.dmg"
  "lantunnel-client-${version}-linux-amd64.AppImage"
  "lantunnel-client-${version}-linux-arm64.AppImage"
  "lantunnel-client-${version}-android-arm64.apk"
  "lantunnel-gateway-${version}-aarch64-apple-darwin"
  "lantunnel-gateway-${version}-x86_64-unknown-linux-musl"
  "lantunnel-admin-${version}-aarch64-apple-darwin"
  "lantunnel-admin-${version}-x86_64-unknown-linux-musl"
)

for artifact in "${artifacts[@]}"; do
  printf 'accepted bytes for %s\n' "$artifact" > "$r2_release/$artifact"
done
(
  cd "$r2_release"
  sha256sum "${artifacts[@]}" > checksums.txt
)
cp "$r2_release/checksums.txt" "$provenance_checksums"
printf '%s\n' '# Changelog' "## [$version]" 'Accepted release.' > "$r2_release/CHANGELOG.md"
cp "$r2_release/CHANGELOG.md" "$provenance_changelog"

cat > "$bin_dir/aws" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$AWS_STUB_LOG"

if [ "$1 $2" = 's3api list-objects-v2' ]; then
  prefix=''
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --prefix)
        prefix="$2"
        shift 2
        ;;
      *)
        shift
        ;;
    esac
  done
  find "$AWS_STUB_R2_ROOT/$prefix" -type f -print \
    | sed "s#^$AWS_STUB_R2_ROOT/##" \
    | sort \
    | paste -sd '\t' -
elif [ "$1 $2" = 's3 cp' ]; then
  source_uri="$3"
  destination="$4"
  key="${source_uri#s3://release-bucket/}"
  cp "$AWS_STUB_R2_ROOT/$key" "$destination"
else
  echo "unexpected aws invocation: $*" >&2
  exit 64
fi
SH
chmod +x "$bin_dir/aws"

download_release() {
  local destination="$1"
  PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$aws_log" \
  AWS_STUB_R2_ROOT="$r2_root" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  R2_BUCKET_NAME="release-bucket" \
  DOWNLOAD_DIR="$destination" \
  CHANGELOG_FILE="$destination/CHANGELOG.md" \
  PROVENANCE_CHECKSUM_FILE="$provenance_checksums" \
  PROVENANCE_CHANGELOG_FILE="$provenance_changelog" \
  scripts/upload.sh "$version" download
}

download_release "$download_dir" >/dev/null

find "$download_dir" -maxdepth 1 -type f -exec basename {} \; | sort > "$tmp_dir/downloaded"
printf '%s\n' "${artifacts[@]}" checksums.txt CHANGELOG.md | sort > "$tmp_dir/expected"
diff -u "$tmp_dir/expected" "$tmp_dir/downloaded"

while IFS= read -r name; do
  cmp "$r2_release/$name" "$download_dir/$name"
done < "$tmp_dir/expected"

if grep -Eq '^s3 cp [^s].* s3://' "$aws_log"; then
  echo 'download-existing path attempted an R2 write' >&2
  exit 1
fi

# An object nested below the version prefix is still an extra authenticated
# R2 object, even when its basename duplicates one expected release asset.
mkdir -p "$r2_release/nested"
cp "$r2_release/checksums.txt" "$r2_release/nested/checksums.txt"
second_download="$tmp_dir/second-download"
mkdir -p "$second_download"
: > "$aws_log"
if download_release "$second_download" >/dev/null 2>&1
then
  echo 'download-existing path accepted a nested extra R2 object' >&2
  exit 1
fi
if grep -q '^s3 cp ' "$aws_log"; then
  echo 'download-existing path downloaded before rejecting an extra R2 object' >&2
  exit 1
fi
rm "$r2_release/nested/checksums.txt"
rmdir "$r2_release/nested"

# R2's checksum metadata itself must be the exact accepted provenance, not a
# newly generated manifest that happens to match different bytes.
printf '\n' >> "$r2_release/checksums.txt"
third_download="$tmp_dir/third-download"
mkdir -p "$third_download"
if download_release "$third_download" >/dev/null 2>&1; then
  echo 'download-existing path accepted unrecorded checksum provenance' >&2
  exit 1
fi
cp "$provenance_checksums" "$r2_release/checksums.txt"

# Changelog metadata is also pinned byte-for-byte, not merely checked for a
# matching version heading.
printf 'unaccepted release note\n' >> "$r2_release/CHANGELOG.md"
changelog_download="$tmp_dir/changelog-download"
mkdir -p "$changelog_download"
if download_release "$changelog_download" >/dev/null 2>&1; then
  echo 'download-existing path accepted unrecorded changelog bytes' >&2
  exit 1
fi
cp "$provenance_changelog" "$r2_release/CHANGELOG.md"

# Every artifact is verified against that accepted checksum manifest.
changed_artifact="$r2_release/lantunnel-client-${version}-windows-amd64.exe"
cp "$changed_artifact" "$tmp_dir/original-artifact"
printf 'mutated bytes\n' >> "$changed_artifact"
fourth_download="$tmp_dir/fourth-download"
mkdir -p "$fourth_download"
if download_release "$fourth_download" >/dev/null 2>&1; then
  echo 'download-existing path accepted artifact bytes with the wrong SHA-256' >&2
  exit 1
fi
cp "$tmp_dir/original-artifact" "$changed_artifact"

# Missing metadata is rejected from the authenticated listing before any
# object download starts.
mv "$r2_release/CHANGELOG.md" "$tmp_dir/CHANGELOG.md"
fifth_download="$tmp_dir/fifth-download"
mkdir -p "$fifth_download"
: > "$aws_log"
if download_release "$fifth_download" >/dev/null 2>&1; then
  echo 'download-existing path accepted a missing R2 release object' >&2
  exit 1
fi
if grep -q '^s3 cp ' "$aws_log"; then
  echo 'download-existing path downloaded before rejecting a missing R2 object' >&2
  exit 1
fi
mv "$tmp_dir/CHANGELOG.md" "$r2_release/CHANGELOG.md"

echo 'download existing release: PASS'
