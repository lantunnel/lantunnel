#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ADMIN="$ROOT/apps/lantunnel-admin/src/main.rs"
CLIENT="$ROOT/apps/lantunnel-client/src-tauri/src/peer_store.rs"
RELEASE="$ROOT/.github/workflows/release.yml"

grep -q 'FILE_ATTRIBUTE_REPARSE_POINT' "$ADMIN"
grep -q 'PROTECTED_DACL_SECURITY_INFORMATION' "$ADMIN"
grep -q 'FILE_ATTRIBUTE_REPARSE_POINT' "$CLIENT"
grep -q 'PROTECTED_DACL_SECURITY_INFORMATION' "$CLIENT"
grep -q 'Windows secret ACL tests' "$RELEASE"
grep -q 'cargo test -p lantunnel-admin' "$RELEASE"
grep -q 'cargo test -p lantunnel-client' "$RELEASE"

if grep -q 'fn set_permissions(_file: &fs::File, _secret: SecretFile)' "$ADMIN"; then
  echo 'lantunnel-admin still contains a non-Unix permission no-op' >&2
  exit 1
fi
if grep -q 'fn set_owner_only_open_file_permissions(_file: &fs::File)' "$CLIENT" \
  || grep -q 'fn set_owner_only_dir_permissions(_path: &Path)' "$CLIENT"; then
  echo 'lantunnel-client still contains a non-Unix permission no-op' >&2
  exit 1
fi

echo 'Windows secret ACL source contract passed.'
