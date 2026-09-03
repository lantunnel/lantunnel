#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAIN="$ROOT_DIR/apps/lantunnel-client/src-tauri/src/main.rs"
APP_TS="$ROOT_DIR/apps/lantunnel-client/frontend/src/App.tsx"
API_TS="$ROOT_DIR/apps/lantunnel-client/frontend/src/client-api.ts"
README="$ROOT_DIR/README.md"
MAKEFILE="$ROOT_DIR/Makefile"
RUNTIME_MAIN="$(mktemp)"
trap 'rm -f "$RUNTIME_MAIN"' EXIT
awk '/^mod tests/ { exit } { print }' "$MAIN" > "$RUNTIME_MAIN"

# The HTTP and TUIC proxy frontends are back under crates/ as unwired future
# work, so their absence is no longer the contract. What must hold is that no
# shipped binary links them: they exist to be compiled and kept honest, not to
# reach a release artifact.
for proxy_crate in tp-proxy-http tp-proxy-tuic; do
  test -f "$ROOT_DIR/crates/$proxy_crate/Cargo.toml" \
    || { echo "$proxy_crate should be retained under crates/" >&2; exit 1; }
  for product in lantunnel-gateway lantunnel-client lantunnel-admin; do
    if (cd "$ROOT_DIR" && cargo tree -p "$product" -e normal 2>/dev/null) | grep -q "$proxy_crate"; then
      echo "$proxy_crate must not be linked into $product" >&2
      exit 1
    fi
  done
done

for removed_path in \
  apps/anyproxy-client/Cargo.toml \
  apps/e2e-orchestrator/Cargo.toml \
  tests/e2e/local_proxy_auto_switch/Cargo.toml \
  tests/e2e/_fixtures/client-rust-loopback.yaml \
  tests/e2e/_fixtures/run_matrix.sh \
  tests/e2e/_fixtures/run_mixed_compat.sh \
  tests/e2e/run_all.sh \
  scripts/three_host_http_upload_bench.sh \
  tests/e2e/tuic/Cargo.toml \
  tests/e2e/observability/Cargo.toml \
  tests/e2e/security/Cargo.toml \
  tests/e2e/web-admin/Cargo.toml \
  tests/e2e/chaos/Cargo.toml \
  tests/e2e/stress/Cargo.toml \
  tests/e2e/p2p_handshake/Cargo.toml
do
  if test -e "$ROOT_DIR/$removed_path"; then
    echo "removed Legacy build/run entry still exists: $removed_path" >&2
    exit 1
  fi
done

# Retained P1/P2/P3 binaries are stateless V2 traffic probes. They have one
# loopback NO AUTH path and no removed implementation-matrix or credential
# selection contract.
if rg -n -- \
  'run_matrix\.sh|matrix::|mod matrix|pub mod matrix|enum Cell|cell_positional|--(cell|no-auth|user|pass)([^[:alnum:]-]|$)|user_pass_subnegotiation|write_greeting_userpass|METHOD_USER_PASS|no_auth_requested|test-group|test-password' \
  "$ROOT_DIR/tests/e2e/connectivity" \
  "$ROOT_DIR/tests/e2e/latency" \
  "$ROOT_DIR/tests/e2e/throughput"; then
  echo 'retained V2 probe still exposes a Legacy matrix or shared-secret path' >&2
  exit 1
fi
grep -q -- 'connect_no_auth' "$ROOT_DIR/tests/e2e/connectivity/src/socks5.rs"

# tp-proxy-http and tp-proxy-tuic are deliberate workspace members again — see
# the non-linkage check above — so only the genuinely retired packages count.
if grep -Eq -- \
  'anyproxy-client|apps/e2e-orchestrator|local_proxy_auto_switch' \
  "$ROOT_DIR/Cargo.toml"; then
  echo 'workspace still contains a removed Legacy Client or E2E package' >&2
  exit 1
fi
if rg -n --glob '*.rs' \
  '\.(open_tunnel|open_udp_tunnel|open_udp_tunnel_to|pick_client)\(' \
  "$ROOT_DIR/apps" "$ROOT_DIR/crates"; then
  echo 'compiled product source still calls a removed Gateway routing API' >&2
  exit 1
fi

# The public 2.0 Client accepts only imported .peer identities. Legacy shared
# secrets never belong in this runtime.
for forbidden in \
  '"--seed"' \
  '"--seed-file"' \
  'struct SavedCreds' \
  'LegacySeed' \
  'ConnectParams' \
  'async fn connect(' \
  'fn load_credentials' \
  'fn save_credentials'
do
  if grep -q -- "$forbidden" "$MAIN"; then
    echo "public Client still contains forbidden V1 surface: $forbidden" >&2
    exit 1
  fi
done

# Tests may name rejected Legacy flags and stale JSON fields, but the compiled
# public runtime before `mod tests` must have no Mesh enable/disable seam or V1
# local-proxy credential branch.
if grep -Eq -- \
  '"--(p2p-enabled|disable-p2p|no-p2p|enable-p2p)"|p2p_enabled:|local_proxy_auth_enabled|AuthMode::UserPass|if p2p_cfg\.enabled' \
  "$RUNTIME_MAIN"; then
  echo 'public Client runtime still exposes a Mesh toggle or Legacy proxy-auth branch' >&2
  exit 1
fi
if grep -Eq -- 'p2p_enabled|peer_client_id|local_proxy_auth_enabled' "$APP_TS" "$API_TS"; then
  echo 'public Client UI settings still expose a Mesh role/target or auth compatibility field' >&2
  exit 1
fi

if grep -Eq -- \
  'APP_MODE_|legacy_desired_role|^[[:space:]]*mode: String|settings\.mode|previous\.mode' \
  "$RUNTIME_MAIN"; then
  echo 'public Client runtime still exposes a historical app/client product mode' >&2
  exit 1
fi
if grep -Eq -- \
  'migrate_legacy_product_config|legacy_config_root_dir|\.tunnel-proxy-client' \
  "$RUNTIME_MAIN"; then
  echo 'public Client runtime still migrates the removed V1 config root' >&2
  exit 1
fi
if grep -Eq -- "mode\??: 'client' \| 'app'|mode: 'client'" "$APP_TS" "$API_TS"; then
  echo 'public Client UI/IPC settings still expose a historical app/client product mode' >&2
  exit 1
fi

test "$(grep -c 'connect_with_peer_profile(' "$RUNTIME_MAIN")" -eq 2
test "$(grep -c 'p2p::bootstrap::run(' "$RUNTIME_MAIN")" -eq 2

grep -q -- 'connect <Tunnel ID>' "$MAIN"
grep -q -- 'connect_peer_profile' "$MAIN"
grep -q -- 'LastPeerSelection' "$MAIN"

main_attach_line="$(awk '/^fn main\(\)/ { in_main=1 } in_main && /attach_parent_console_for_cli\(\)/ { print NR; exit }' "$MAIN")"
main_early_exit_line="$(awk '/^fn main\(\)/ { in_main=1 } in_main && /early_exit_output\(ProductKind\)/ { print NR; exit }' "$MAIN")"
test -n "$main_attach_line"
test -n "$main_early_exit_line"
test "$main_attach_line" -lt "$main_early_exit_line"

if grep -Eq -- '--seed|--seed-file|<tunnel_id>:<tunnel_key>|calls tunnel-proxy-platform with.*tunnel_key' "$README"; then
  echo 'README still advertises a Legacy V1 Client startup path' >&2
  exit 1
fi
grep -q -- 'lantunnel-client tunnel import' "$README"
grep -q -- "lantunnel-client connect '<tunnel_id>'" "$README"
if grep -q -- "lantunnel-client --headless connect" "$README"; then
  echo 'README places the headless flag before the connect subcommand, which the public parser rejects' >&2
  exit 1
fi
# The README must state the single-line requirement, and must not offer a
# cross-version story. It used to be checked by the phrase "V2-only release",
# which said nothing to a reader who has never seen V1 -- the word appears
# nowhere else in that file and is never defined.
grep -q -- 'same 2.0.x line' "$README"
if grep -Eq -- 'backward[- ]compatible|mixed-version (support|compatibility)|compatible with 1\.x' "$README"; then
  echo 'README advertises cross-version compatibility' >&2
  exit 1
fi
if grep -q -- 'make release-fast' "$README"; then
  echo 'README still advertises a host combination that release-fast cannot build' >&2
  exit 1
fi

if grep -Eq -- 'REAL_TEST_APP_ARTIFACT|APP_ARTIFACT' "$MAKEFILE"; then
  echo 'public real-test Make targets still require a separate App artifact' >&2
  exit 1
fi
if grep -Eq -- '^(deploy-real-test|release-deploy-real-test):' "$MAKEFILE"; then
  echo 'Make still publishes the Legacy role-split real-test deploy target' >&2
  exit 1
fi
if grep -Eq -- '^release-fast:.*##' "$MAKEFILE"; then
  echo 'make help still advertises the host-incompatible release-fast target' >&2
  exit 1
fi
# The recovery runbook that used to carry this assertion was an internal
# operator document and is no longer published. The invariant it guarded --
# that no Client-side P2P toggle or protocol downgrade exists to advertise --
# is enforced against the source above, which is where it actually lives.

echo 'public lantunnel-client is V2 .peer-only: PASS'
