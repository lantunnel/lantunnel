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
release_dir="$tmp_dir/release"
bin_dir="$tmp_dir/bin"
log_file="$tmp_dir/aws.log"

mkdir -p "$release_dir" "$bin_dir"

touch "$release_dir/lantunnel-client-${version}-windows-amd64.exe"
touch "$release_dir/lantunnel-client-${version}-macos-amd64.dmg"
touch "$release_dir/lantunnel-client-${version}-macos-arm64.dmg"
touch "$release_dir/lantunnel-client-${version}-linux-amd64.AppImage"
touch "$release_dir/lantunnel-client-${version}-linux-arm64.AppImage"
touch "$release_dir/lantunnel-gateway-${version}-aarch64-apple-darwin"
touch "$release_dir/lantunnel-gateway-${version}-x86_64-unknown-linux-musl"
touch "$release_dir/lantunnel-admin-${version}-aarch64-apple-darwin"
touch "$release_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl"
touch "$release_dir/lantunnel-client-${version}-android-arm64.apk"
touch "$release_dir/SHA256SUMS"
make -s checksums \
  VERSION="$version" \
  UI_VERSION="$version" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" >/dev/null
# Six client artifacts and two each for the Gateway and the Admin tool. The
# APK became a published artifact when the download page started serving it,
# and upload.sh requires exactly one checksum line per artifact it uploads.
test "$(wc -l < "$release_dir/checksums.txt" | tr -d ' ')" -eq 10
if grep -Eq 'ios|anyproxy' "$release_dir/checksums.txt"; then
  echo 'V2 checksums include a non-public artifact' >&2
  exit 1
fi

cat > "$bin_dir/aws" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$AWS_STUB_LOG"
if [ "$1 $2" = 's3 ls' ]; then
  echo 'legacy s3 ls must not be used for an empty release prefix' >&2
  exit 66
elif [ "$1 $2" = 's3api list-objects-v2' ]; then
  if [ "${AWS_STUB_FAIL_LIST:-0}" = 1 ]; then
    exit 75
  fi
  prefix=''
  query=''
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --prefix)
        prefix="$2"
        shift 2
        ;;
      --query)
        query="$2"
        shift 2
        ;;
      *)
        shift
        ;;
    esac
  done
  found=0
  while IFS= read -r object; do
    [ -n "$object" ] || continue
    found=1
    printf '%s%s\t' "$prefix" "$object"
  done < "${AWS_STUB_REMOTE_OBJECTS:?AWS_STUB_REMOTE_OBJECTS is required}"
  if [ "$found" -eq 0 ] && [ "$query" != 'Contents[].Key || `[]`' ]; then
    printf 'None\n'
  else
    printf '\n'
  fi
elif [ "$1 $2" = 's3api put-object' ]; then
  shift 2
  key=''
  body=''
  if_none_match=''
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --key)
        key="$2"
        shift 2
        ;;
      --body)
        body="$2"
        shift 2
        ;;
      --if-none-match)
        if_none_match="$2"
        shift 2
        ;;
      *)
        shift
        ;;
    esac
  done
  object_name="$(basename "$key")"
  if [ -n "${AWS_STUB_MUTATE_REMOTE_ON_FIRST_PUT:-}" ] &&
     [ ! -e "${AWS_STUB_MUTATION_MARKER:?AWS_STUB_MUTATION_MARKER is required}" ]
  then
    printf 'mutated existing remote bytes\n' \
      > "${AWS_STUB_REMOTE_CONTENT_DIR:?AWS_STUB_REMOTE_CONTENT_DIR is required}/${AWS_STUB_MUTATE_REMOTE_ON_FIRST_PUT}"
    : > "$AWS_STUB_MUTATION_MARKER"
  fi
  if [ "${AWS_STUB_RACE_OBJECT:-}" = "$object_name" ] &&
     ! grep -Fxq "$object_name" "${AWS_STUB_REMOTE_OBJECTS:?AWS_STUB_REMOTE_OBJECTS is required}"
  then
    printf '%s\n' "$object_name" >> "$AWS_STUB_REMOTE_OBJECTS"
    printf 'concurrent writer bytes\n' \
      > "${AWS_STUB_REMOTE_CONTENT_DIR:?AWS_STUB_REMOTE_CONTENT_DIR is required}/${object_name}"
    exit 76
  fi
  if [ "$if_none_match" = '*' ] && grep -Fxq "$object_name" "${AWS_STUB_REMOTE_OBJECTS:?AWS_STUB_REMOTE_OBJECTS is required}"; then
    exit 76
  fi
  printf '%s\n' "$object_name" >> "$AWS_STUB_REMOTE_OBJECTS"
  cp "$body" "${AWS_STUB_REMOTE_CONTENT_DIR:?AWS_STUB_REMOTE_CONTENT_DIR is required}/${object_name}"
  if [ -n "${AWS_STUB_MUTATE_LOCAL_ON_FIRST_PUT:-}" ] &&
     [ ! -e "${AWS_STUB_LOCAL_MUTATION_MARKER:?AWS_STUB_LOCAL_MUTATION_MARKER is required}" ]
  then
    printf 'mutated local artifact bytes\n' > "$AWS_STUB_MUTATE_LOCAL_ON_FIRST_PUT"
    : > "$AWS_STUB_LOCAL_MUTATION_MARKER"
  fi
elif [ "$1 $2" = 's3 cp' ]; then
  if [[ "$3" = s3://* ]]; then
    object_name="$(basename "$3")"
    remote_file="${AWS_STUB_REMOTE_CONTENT_DIR:?AWS_STUB_REMOTE_CONTENT_DIR is required}/${object_name}"
    if [ -f "$remote_file" ]; then
      cp "$remote_file" "$4"
    else
      : > "$4"
    fi
  else
    object_name="$(basename "$4")"
    printf '%s\n' "$object_name" >> "${AWS_STUB_REMOTE_OBJECTS:?AWS_STUB_REMOTE_OBJECTS is required}"
    cp "$3" "${AWS_STUB_REMOTE_CONTENT_DIR:?AWS_STUB_REMOTE_CONTENT_DIR is required}/${object_name}"
  fi
fi
SH
chmod +x "$bin_dir/aws"

remote_objects="$tmp_dir/remote-objects"
remote_content_dir="$tmp_dir/remote-content"
mkdir -p "$remote_content_dir"
: > "$remote_objects"
export AWS_STUB_REMOTE_OBJECTS="$remote_objects"
export AWS_STUB_REMOTE_CONTENT_DIR="$remote_content_dir"

# A remote release target is production configuration, never a silent fallback.
if env -u R2_BUCKET_NAME -u R2_BUCKET \
  PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  AWS_STUB_REMOTE_OBJECTS="$remote_objects" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'remote upload unexpectedly accepted a missing explicit R2 bucket' >&2
  exit 1
fi
if [ -f "$log_file" ] && grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'remote upload wrote before requiring an explicit R2 bucket' >&2
  exit 1
fi

export R2_BUCKET_NAME='release-bucket'

# A same-version retry must compare existing bytes before writing. A single
# mismatched expected object makes the entire attempt fail closed, so none of
# the other missing objects may be created first.
printf '%s\n' "lantunnel-client-${version}-windows-amd64.exe" > "$remote_objects"
printf 'different remote bytes\n' \
  > "$remote_content_dir/lantunnel-client-${version}-windows-amd64.exe"
: > "$log_file"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'same-version upload unexpectedly overwrote a mismatched R2 object' >&2
  exit 1
fi
if grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'same-version upload wrote before rejecting a mismatched R2 object' >&2
  exit 1
fi
rm "$remote_content_dir/lantunnel-client-${version}-windows-amd64.exe"
: > "$remote_objects"
: > "$log_file"

PATH="$bin_dir:$PATH" \
AWS_STUB_LOG="$log_file" \
AWS_STUB_REMOTE_OBJECTS="$remote_objects" \
R2_ACCOUNT_ID="account" \
R2_ACCESS_KEY_ID="access" \
R2_SECRET_ACCESS_KEY="secret" \
DIST_DIR="$tmp_dir/dist" \
RELEASE_DIR="$release_dir" \
DOWNLOAD_DIR="$release_dir" \
scripts/upload.sh "$version" remote >/dev/null

if grep -q '^s3 cp [^ ]* s3://' "$log_file"; then
  echo 'missing R2 objects were created with an unconditional upload' >&2
  exit 1
fi
# Ten release artifacts plus checksums.txt and CHANGELOG.md.
if [ "$(grep -Ec '^s3api put-object .*--if-none-match \*([[:space:]]|$)' "$log_file" || true)" -ne 12 ]; then
  echo 'missing R2 objects were not all created with an atomic create-only condition' >&2
  exit 1
fi

grep -q -- "--key releases/${version}/lantunnel-client-${version}-windows-amd64.exe" "$log_file"
grep -q -- "--key releases/${version}/lantunnel-client-${version}-linux-amd64.AppImage" "$log_file"
grep -q -- "--key releases/${version}/lantunnel-gateway-${version}-aarch64-apple-darwin" "$log_file"
grep -q -- "--key releases/${version}/lantunnel-gateway-${version}-x86_64-unknown-linux-musl" "$log_file"
grep -q -- "--key releases/${version}/lantunnel-admin-${version}-aarch64-apple-darwin" "$log_file"
grep -q -- "--key releases/${version}/lantunnel-admin-${version}-x86_64-unknown-linux-musl" "$log_file"
# The Android build ships with the rest of the release. It is published again
# under the mobile/<version>/ prefix the download page reads, which this stub
# reports as already present rather than writing a second time.
grep -q -- "--key releases/${version}/lantunnel-client-${version}-android-arm64.apk" "$log_file"
grep -q -- "--key releases/${version}/checksums.txt" "$log_file"
if grep -q -- "--key releases/${version}/SHA256SUMS" "$log_file"; then
  echo 'desktop upload must not publish the CLI archive checksum manifest' >&2
  exit 1
fi

# Metadata retries are immutable too. When the complete remote release is
# already byte-identical, the changelog target is an idempotent no-op.
: > "$log_file"
PATH="$bin_dir:$PATH" \
AWS_STUB_LOG="$log_file" \
R2_ACCOUNT_ID="account" \
R2_ACCESS_KEY_ID="access" \
R2_SECRET_ACCESS_KEY="secret" \
DIST_DIR="$tmp_dir/dist" \
RELEASE_DIR="$release_dir" \
DOWNLOAD_DIR="$release_dir" \
scripts/upload.sh "$version" changelog >/dev/null
if grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'byte-identical changelog retry attempted to overwrite immutable metadata' >&2
  exit 1
fi

printf 'different published changelog\n' > "$remote_content_dir/CHANGELOG.md"
: > "$log_file"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" changelog >/dev/null 2>&1
then
  echo 'changelog retry unexpectedly accepted mismatched immutable metadata' >&2
  exit 1
fi
if grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'changelog retry wrote before rejecting mismatched immutable metadata' >&2
  exit 1
fi
cp CHANGELOG.md "$remote_content_dir/CHANGELOG.md"

# If another publisher creates a previously missing key after the read-only
# preflight, the server-side create condition must reject this publisher and
# preserve the concurrent writer's bytes.
race_remote_objects="$tmp_dir/race-remote-objects"
race_remote_content_dir="$tmp_dir/race-remote-content"
race_name="lantunnel-client-${version}-windows-amd64.exe"
mkdir -p "$race_remote_content_dir"
: > "$race_remote_objects"
: > "$log_file"
if AWS_STUB_RACE_OBJECT="$race_name" \
  AWS_STUB_REMOTE_OBJECTS="$race_remote_objects" \
  AWS_STUB_REMOTE_CONTENT_DIR="$race_remote_content_dir" \
  PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'concurrent same-key creation unexpectedly allowed this publisher to continue' >&2
  exit 1
fi
if [ "$(cat "$race_remote_objects")" != "$race_name" ] ||
   [ "$(cat "$race_remote_content_dir/$race_name")" != 'concurrent writer bytes' ]
then
  echo 'conditional R2 create did not preserve the concurrent writer object' >&2
  exit 1
fi

# An existing object can be replaced by another writer after the read-only
# preflight while this publisher creates the missing suffix of the manifest.
# A successful command must therefore re-read every published byte, not only
# re-list the final keys.
postcheck_remote_objects="$tmp_dir/postcheck-remote-objects"
postcheck_remote_content_dir="$tmp_dir/postcheck-remote-content"
postcheck_mutation_marker="$tmp_dir/postcheck-mutated"
postcheck_output="$tmp_dir/postcheck-output"
mkdir -p "$postcheck_remote_content_dir"
printf '%s\n' "$race_name" > "$postcheck_remote_objects"
cp "$release_dir/$race_name" "$postcheck_remote_content_dir/$race_name"
: > "$log_file"
if AWS_STUB_MUTATE_REMOTE_ON_FIRST_PUT="$race_name" \
  AWS_STUB_MUTATION_MARKER="$postcheck_mutation_marker" \
  AWS_STUB_REMOTE_OBJECTS="$postcheck_remote_objects" \
  AWS_STUB_REMOTE_CONTENT_DIR="$postcheck_remote_content_dir" \
  PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >"$postcheck_output" 2>&1
then
  echo 'upload reported success after a preflighted remote object changed during publish' >&2
  exit 1
fi
if [ "$(cat "$postcheck_remote_content_dir/$race_name")" != 'mutated existing remote bytes' ]; then
  echo 'remote mutation fixture did not preserve the competing writer bytes' >&2
  exit 1
fi
if grep -Fq "Version ${version} uploaded" "$postcheck_output"; then
  echo 'failed remote post-write verification still printed a success claim' >&2
  exit 1
fi

# Local inputs are mutable files. Changing a later artifact after the first
# object is created must not let a mixed artifact/checksum bundle report
# success. The publisher must bind the whole attempt to one stable snapshot.
local_race_release_dir="$tmp_dir/local-race-release"
local_race_remote_objects="$tmp_dir/local-race-remote-objects"
local_race_remote_content_dir="$tmp_dir/local-race-remote-content"
local_race_mutation_marker="$tmp_dir/local-race-mutated"
local_race_target="$local_race_release_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl"
local_race_output="$tmp_dir/local-race-output"
cp -R "$release_dir" "$local_race_release_dir"
mkdir -p "$local_race_remote_content_dir"
: > "$local_race_remote_objects"
: > "$log_file"
if AWS_STUB_MUTATE_LOCAL_ON_FIRST_PUT="$local_race_target" \
  AWS_STUB_LOCAL_MUTATION_MARKER="$local_race_mutation_marker" \
  AWS_STUB_REMOTE_OBJECTS="$local_race_remote_objects" \
  AWS_STUB_REMOTE_CONTENT_DIR="$local_race_remote_content_dir" \
  PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$local_race_release_dir" \
  DOWNLOAD_DIR="$local_race_release_dir" \
  scripts/upload.sh "$version" remote >"$local_race_output" 2>&1
then
  echo 'upload reported success after a local release input changed during publish' >&2
  exit 1
fi
if [ "$(cat "$local_race_target")" != 'mutated local artifact bytes' ]; then
  echo 'local mutation fixture did not change the release input' >&2
  exit 1
fi
if [ -s "$local_race_remote_content_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl" ]; then
  echo 'local mutation leaked into the remote bundle instead of using the stable snapshot' >&2
  exit 1
fi
if grep -Fq "Version ${version} uploaded" "$local_race_output"; then
  echo 'failed local stability verification still printed a success claim' >&2
  exit 1
fi

# A retry may find a subset from a prior interrupted run, but an unexpected
# object must fail before this invocation writes anything.
printf '%s\n' 'unrelated-old-artifact' > "$remote_objects"
: > "$log_file"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'remote upload unexpectedly accepted an extra object under its release prefix' >&2
  exit 1
fi
if grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'remote upload wrote before rejecting an extra prefix object' >&2
  exit 1
fi

# Prefix exactness is recursive: a nested object is not allowed to hide below
# an otherwise valid version prefix.
printf '%s\n' 'nested/checksums.txt' > "$remote_objects"
: > "$log_file"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'remote upload unexpectedly accepted a nested extra object' >&2
  exit 1
fi
if grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'remote upload wrote before rejecting a nested extra object' >&2
  exit 1
fi

# A failed object listing is an indeterminate remote state, so fail closed
# before a retry can write any object.
: > "$remote_objects"
: > "$log_file"
if AWS_STUB_FAIL_LIST=1 \
  PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'remote upload unexpectedly ignored an R2 prefix listing failure' >&2
  exit 1
fi
if grep -Eq '^s3 cp [^ ]+ s3://|^s3api put-object ' "$log_file"; then
  echo 'remote upload wrote after an R2 prefix listing failure' >&2
  exit 1
fi

# An interrupted same-version upload is recoverable: an expected subset is
# completed, then the post-write read must observe the exact fixed manifest.
# The byte-identical object already present is verified and skipped rather
# than overwritten during the retry.
printf '%s\n' "lantunnel-client-${version}-windows-amd64.exe" > "$remote_objects"
cp "$release_dir/lantunnel-client-${version}-windows-amd64.exe" \
  "$remote_content_dir/lantunnel-client-${version}-windows-amd64.exe"
: > "$log_file"
PATH="$bin_dir:$PATH" \
AWS_STUB_LOG="$log_file" \
R2_ACCOUNT_ID="account" \
R2_ACCESS_KEY_ID="access" \
R2_SECRET_ACCESS_KEY="secret" \
DIST_DIR="$tmp_dir/dist" \
RELEASE_DIR="$release_dir" \
DOWNLOAD_DIR="$release_dir" \
scripts/upload.sh "$version" remote >/dev/null
if grep -Fq "s3 cp $release_dir/lantunnel-client-${version}-windows-amd64.exe s3://release-bucket/releases/${version}/lantunnel-client-${version}-windows-amd64.exe" \
  "$log_file" || \
   grep -Eq "^s3api put-object .*--key releases/${version}/lantunnel-client-${version}-windows-amd64.exe([[:space:]]|$)" \
  "$log_file"
then
  echo 'same-version retry overwrote a byte-identical R2 object instead of skipping it' >&2
  exit 1
fi
cat > "$tmp_dir/expected-remote-objects" <<EOF
lantunnel-client-${version}-windows-amd64.exe
lantunnel-client-${version}-macos-amd64.dmg
lantunnel-client-${version}-macos-arm64.dmg
lantunnel-client-${version}-linux-amd64.AppImage
lantunnel-client-${version}-linux-arm64.AppImage
lantunnel-client-${version}-android-arm64.apk
lantunnel-gateway-${version}-aarch64-apple-darwin
lantunnel-gateway-${version}-x86_64-unknown-linux-musl
lantunnel-admin-${version}-aarch64-apple-darwin
lantunnel-admin-${version}-x86_64-unknown-linux-musl
checksums.txt
CHANGELOG.md
EOF
if ! diff -u <(sort -u "$tmp_dir/expected-remote-objects") <(sort -u "$remote_objects"); then
  echo 'remote retry did not finish with the exact product-plus-metadata manifest' >&2
  exit 1
fi

: > "$log_file"
rm "$release_dir/lantunnel-gateway-${version}-x86_64-unknown-linux-musl"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted an incomplete public artifact manifest' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 upload started before validating the complete artifact manifest' >&2
  exit 1
fi

: > "$log_file"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  UPLOAD_PRODUCTS="client" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted a partial public product set' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 upload started before validating the complete public product set' >&2
  exit 1
fi

: > "$log_file"
touch "$release_dir/lantunnel-gateway-${version}-x86_64-unknown-linux-musl"
mv "$release_dir/checksums.txt" "$release_dir/checksums.saved"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted a release without checksums.txt' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 upload started before validating checksums.txt' >&2
  exit 1
fi
mv "$release_dir/checksums.saved" "$release_dir/checksums.txt"

: > "$log_file"
mv "$release_dir/checksums.txt" "$release_dir/checksums.saved"
grep -v "lantunnel-admin-${version}-aarch64-apple-darwin" \
  "$release_dir/checksums.saved" > "$release_dir/checksums.txt"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted an incomplete checksum manifest' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 upload started before validating the checksum manifest' >&2
  exit 1
fi
mv "$release_dir/checksums.saved" "$release_dir/checksums.txt"

: > "$log_file"
printf 'changed after checksums were generated\n' \
  > "$release_dir/lantunnel-admin-${version}-aarch64-apple-darwin"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted a stale checksum manifest' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 upload started before verifying artifact checksums' >&2
  exit 1
fi
: > "$release_dir/lantunnel-admin-${version}-aarch64-apple-darwin"

: > "$log_file"
printf '%s\n' '# Changelog' '## [1.9.9]' > "$tmp_dir/CHANGELOG.md"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  CHANGELOG_FILE="$tmp_dir/CHANGELOG.md" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted a changelog without this release version' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 upload started before validating the release changelog' >&2
  exit 1
fi

: > "$log_file"
mv "$release_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl" \
  "$release_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl.saved"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  scripts/upload.sh "$version" changelog >/dev/null 2>&1
then
  echo 'V2 changelog upload unexpectedly bypassed the complete release manifest' >&2
  exit 1
fi
if [ -s "$log_file" ]; then
  echo 'V2 changelog upload started before validating the complete release' >&2
  exit 1
fi
mv "$release_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl.saved" \
  "$release_dir/lantunnel-admin-${version}-x86_64-unknown-linux-musl"

: > "$log_file"
if PATH="$bin_dir:$PATH" \
  AWS_STUB_LOG="$log_file" \
  R2_ACCOUNT_ID="account" \
  R2_ACCESS_KEY_ID="access" \
  R2_SECRET_ACCESS_KEY="secret" \
  DIST_DIR="$tmp_dir/dist" \
  RELEASE_DIR="$release_dir" \
  DOWNLOAD_DIR="$release_dir" \
  UPLOAD_PRODUCTS="mobile" \
  scripts/upload.sh "$version" remote >/dev/null 2>&1
then
  echo 'V2 upload unexpectedly accepted the mobile product' >&2
  exit 1
fi
