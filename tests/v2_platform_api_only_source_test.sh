#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TP_CLIENT_SRC="$ROOT_DIR/crates/tp-client/src"
TP_CLIENT_LIB="$TP_CLIENT_SRC/lib.rs"
TP_CLIENT_ENGINE="$TP_CLIENT_SRC/engine.rs"
TP_CLIENT_BOOTSTRAP="$TP_CLIENT_SRC/p2p/bootstrap.rs"
TP_CLIENT_PROXY_MODE="$TP_CLIENT_SRC/proxy_mode.rs"
MOBILE_FFI="$ROOT_DIR/crates/tp-mobile-ffi/src/lib.rs"
ANDROID_CONFIG="$ROOT_DIR/apps/android-proxy/app/src/main/java/com/buhuipao/tunnelproxy/MobileConfig.kt"
ANDROID_NATIVE="$ROOT_DIR/apps/android-proxy/app/src/main/java/com/buhuipao/tunnelproxy/TunnelProxyNative.kt"
ANDROID_ACTIVITY="$ROOT_DIR/apps/android-proxy/app/src/main/java/com/buhuipao/tunnelproxy/MainActivity.kt"
IOS_CONFIG="$ROOT_DIR/apps/ios-proxy/TunnelProxyShared/MobileConfig.swift"
IOS_BRIDGE="$ROOT_DIR/apps/ios-proxy/TunnelProxyShared/TunnelProxyNativeBridge.swift"
IOS_HEADER="$ROOT_DIR/apps/ios-proxy/NativeLibs/TpMobileFfi/include/tp_mobile_ffi.h"
PRODUCTION_RUST_ROOTS=("$ROOT_DIR/crates" "$ROOT_DIR/apps")

start_request_allowlist="$({
  sed -n '/pub struct StartProxyRequest {/,/^}/p' "$MOBILE_FFI" \
    | sed -n 's/^[[:space:]]*pub \([a-z0-9_]*\):.*/\1/p'
} | sort -u)"

# Only the top level of the start request. Scanning the whole file also picked
# up the keys of the objects nested inside it — `client_access` is built here
# too, and its own `allow`/`deny` are not StartProxyRequest fields.
android_start_keys="$({
  sed -n '/fun buildStartJson()/,/^    }/p' "$ANDROID_CONFIG" \
    | sed -n 's/^[[:space:]]*\.put("\([a-z0-9_]*\)".*/\1/p'
  sed -n 's/.*JSONObject(raw)\.put("\([a-z0-9_]*\)".*/\1/p' "$ANDROID_ACTIVITY"
} | sort -u)"

ios_start_keys="$(
  sed -n '/let request: \[String: Any\] = \[/,/^[[:space:]]*\]/p' "$IOS_CONFIG" \
    | sed -n 's/^[[:space:]]*"\([a-z0-9_]*\)":.*/\1/p' \
    | sort -u
)"

for client_keys in "$android_start_keys" "$ios_start_keys"; do
  while IFS= read -r key; do
    if ! grep -Fxq -- "$key" <<<"$start_request_allowlist"; then
      echo "mobile client sends a key rejected by Rust StartProxyRequest: $key" >&2
      exit 1
    fi
  done <<<"$client_keys"
done

for endpoint in \
  '/api/tunnel/config' \
  '/api/session/heartbeat' \
  '/api/session/disconnect'
do
  if grep -R -Fq --include='*.rs' -- "$endpoint" "${PRODUCTION_RUST_ROOTS[@]}"; then
    echo "production Rust source still contains removed Platform endpoint: $endpoint" >&2
    exit 1
  fi
done

for legacy_surface in \
  'PlatformClient' \
  'ConnectParams' \
  'pub async fn connect(' \
  'connect_with_config' \
  'DirectGenerationSource::Fixed'
do
  if grep -Fq -- "$legacy_surface" "$TP_CLIENT_LIB" "$TP_CLIENT_ENGINE"; then
    echo "tp-client still exposes removed V1 connection surface: $legacy_surface" >&2
    exit 1
  fi
done

if grep -Eq -- 'desired_role|ClientRoleConfig|v2_attachment:[[:space:]]*Option|auth_(user|pass)word' "$TP_CLIENT_ENGINE"; then
  echo 'tp-client Engine still contains a selectable or unauthenticated V1 generation' >&2
  exit 1
fi
if grep -Eq -- 'ClientRoleConfig|peer_client_id|tunnel_key|group_password' "$TP_CLIENT_SRC/platform.rs"; then
  echo 'tp-client internal TunnelConfig still contains a V1 role/shared-secret routing field' >&2
  exit 1
fi
if sed '/^#\[cfg(test)\]/,$d' "$TP_CLIENT_BOOTSTRAP" \
  | grep -Eq -- 'BootstrapRole|run_acceptor|run_with_role'; then
  echo 'tp-client bootstrap still exposes a product-level initiator/acceptor role' >&2
  exit 1
fi
if sed '/^#\[cfg(test)\]/,$d' "$TP_CLIENT_PROXY_MODE" \
  | grep -Eq -- 'ClientRoleConfig|platform_relay_peer_hint'; then
  echo 'tp-client proxy mode still selects a V1 product role or manual relay peer hint' >&2
  exit 1
fi

if grep -R -Fq --include='*.rs' -- 'ConnectParams' "${PRODUCTION_RUST_ROOTS[@]}"; then
  echo 'production Rust source still contains the removed ConnectParams surface' >&2
  exit 1
fi

grep -Fq -- '/api/tunnels/{}/resolve' "$TP_CLIENT_SRC/managed_resolve.rs"
grep -Fq -- '/api/peers/heartbeat' "$TP_CLIENT_SRC/peer_heartbeat.rs"
grep -Fq -- 'connect_with_peer_profile(' "$MOBILE_FFI"
grep -Fq -- 'pub peer_profile: PeerProfileV2' "$MOBILE_FFI"
grep -Fq -- 'mobile_contract_rejects_the_removed_tunnel_key_start_request' "$MOBILE_FFI"

grep -Fq -- 'put("peer_profile"' "$ANDROID_CONFIG"
grep -Fq -- '"peer_profile": profile' "$IOS_CONFIG"
for removed_mobile_surface in \
  'MobileSeed' \
  'parseSeedJson' \
  'tp_mobile_parse_seed_json'
do
  if grep -Fq -- "$removed_mobile_surface" \
    "$MOBILE_FFI" "$ANDROID_CONFIG" "$ANDROID_NATIVE" "$ANDROID_ACTIVITY" \
    "$IOS_CONFIG" "$IOS_BRIDGE" "$IOS_HEADER"; then
    echo "production mobile source still contains removed V1 surface: $removed_mobile_surface" >&2
    exit 1
  fi
done

for mobile_source in \
  "$ANDROID_CONFIG" \
  "$ANDROID_NATIVE" \
  "$ANDROID_ACTIVITY" \
  "$IOS_CONFIG" \
  "$IOS_BRIDGE" \
  "$IOS_HEADER"
do
  if grep -Fq -- 'tunnel_key' "$mobile_source"; then
    echo "production mobile source still contains a tunnel_id+tunnel_key seed/start schema: $mobile_source" >&2
    exit 1
  fi
done

if sed '/^#\[cfg(test)\]/,$d' "$MOBILE_FFI" | grep -Eq 'tunnel_key|local_proxy_auth_enabled|group_password'; then
  echo 'production tp-mobile-ffi source still contains a removed V1 start/auth field' >&2
  exit 1
fi

echo 'production Rust Platform surface is V2-only: PASS'
