#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
package_name="com.buhuipao.tunnelproxy"
start_extra="com.buhuipao.tunnelproxy.extra.START_JSON"
apk_path="$script_dir/app/build/outputs/apk/debug/app-debug.apk"

usage() {
  cat <<'USAGE'
Build, install, and smoke-test the Android proxy app on a connected device.

Required environment:
  PEER_PROFILE_FILE  Path to one verified V2 .peer JSON profile.
  SMOKE_URL       URL reachable through the tunnel, usually an echo service
                  on the LAN PC side.

Optional environment:
  ANDROID_SERIAL             adb serial when multiple devices are connected.
  ABIS                       Passed to build-rust-jni-libs.sh. Default: arm64-v8a
  INSECURE_TLS               true or false. Default: false
  DEVICE_ID                  Stable test device id. Default: android-smoke
  DEVICE_SOCKS_LISTEN        Device listen socket. Default: 127.0.0.1:1080
  HOST_SOCKS_PORT            Host port forwarded to device SOCKS. Default: 61080
  STARTUP_WAIT_SECONDS       Wait after service start. Default: 5
  CURL_TIMEOUT_SECONDS       curl max time. Default: 20
  CURL_PROXY                 Override curl proxy URL. When unset the script
                             uses --socks5-hostname without credentials.
  EXPECT_LOG_REGEX           Optional regex that must appear in adb logcat.
  KEEP_RUNNING=1             Leave app and adb forward running after the smoke.
  SKIP_NATIVE_BUILD=1        Skip Rust .so build.
  SKIP_ASSEMBLE=1            Skip Gradle assemble.
  GRADLE=/path/to/gradle     Use a specific Gradle executable.
  ADB=/path/to/adb           Use a specific adb executable.

The service receives JSON with local_socks5_listen and starts:
  Clash/curl -> adb forward -> 127.0.0.1:1080 on device -> Rust tunnel engine
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    echo "error: missing required environment variable: $name" >&2
    usage >&2
    exit 1
  fi
}

require_env PEER_PROFILE_FILE
require_env SMOKE_URL
[[ -f "$PEER_PROFILE_FILE" ]] || { echo "error: missing Peer profile: $PEER_PROFILE_FILE" >&2; exit 1; }

adb_bin="${ADB:-adb}"
insecure_tls="${INSECURE_TLS:-false}"
device_socks_listen="${DEVICE_SOCKS_LISTEN:-127.0.0.1:1080}"
host_socks_port="${HOST_SOCKS_PORT:-61080}"
device_id="${DEVICE_ID:-android-smoke}"
startup_wait="${STARTUP_WAIT_SECONDS:-5}"
curl_timeout="${CURL_TIMEOUT_SECONDS:-20}"

case "$insecure_tls" in
  true|false) ;;
  *)
    echo "error: INSECURE_TLS must be true or false" >&2
    exit 1
    ;;
esac

device_socks_port="${device_socks_listen##*:}"
if [[ "$device_socks_port" == "$device_socks_listen" || -z "$device_socks_port" ]]; then
  echo "error: DEVICE_SOCKS_LISTEN must include a TCP port" >&2
  exit 1
fi

if [[ -n "${ANDROID_SERIAL:-}" ]]; then
  adb_args=(-s "$ANDROID_SERIAL")
else
  adb_args=()
fi

cleanup() {
  "$adb_bin" "${adb_args[@]}" forward --remove "tcp:$host_socks_port" >/dev/null 2>&1 || true
  if [[ "${KEEP_RUNNING:-0}" != "1" ]]; then
    "$adb_bin" "${adb_args[@]}" shell am force-stop "$package_name" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ "${SKIP_NATIVE_BUILD:-0}" != "1" ]]; then
  "$script_dir/build-rust-jni-libs.sh"
fi

if [[ "${SKIP_ASSEMBLE:-0}" != "1" ]]; then
  if [[ -n "${GRADLE:-}" ]]; then
    gradle_cmd=("$GRADLE")
  elif [[ -x "$script_dir/gradlew" ]]; then
    gradle_cmd=("$script_dir/gradlew")
  elif command -v gradle >/dev/null 2>&1; then
    gradle_cmd=(gradle)
  else
    echo "error: Gradle is required; install gradle or add a Gradle wrapper under apps/android-proxy" >&2
    exit 1
  fi
  "${gradle_cmd[@]}" -p "$script_dir" :app:assembleDebug
fi

if [[ ! -f "$apk_path" ]]; then
  echo "error: missing debug APK: $apk_path" >&2
  exit 1
fi

start_json=$(jq -cn \
  --argjson peer_profile "$(jq -c . "$PEER_PROFILE_FILE")" \
  --arg device_id "$device_id" \
  --arg local_socks5_listen "$device_socks_listen" \
  --argjson insecure_tls "$insecure_tls" \
  '{peer_profile: $peer_profile, device_id: $device_id, local_socks5_listen: $local_socks5_listen, insecure_tls: $insecure_tls}')

"$adb_bin" "${adb_args[@]}" wait-for-device
"$adb_bin" "${adb_args[@]}" install -r "$apk_path"
"$adb_bin" "${adb_args[@]}" logcat -c || true
"$adb_bin" "${adb_args[@]}" shell am start \
  -n "$package_name/.MainActivity" \
  --es "$start_extra" "$start_json" >/dev/null

sleep "$startup_wait"

"$adb_bin" "${adb_args[@]}" forward "tcp:$host_socks_port" "tcp:$device_socks_port"

curl_args=(
  --fail
  --show-error
  --silent
  --max-time "$curl_timeout"
)
if [[ -n "${CURL_PROXY:-}" ]]; then
  curl_args+=(--proxy "$CURL_PROXY")
else
  curl_args+=(--socks5-hostname "127.0.0.1:$host_socks_port")
fi

curl "${curl_args[@]}" "$SMOKE_URL" >/tmp/lantunnel-android-smoke.out

echo "ok: SOCKS5 data-plane smoke reached $SMOKE_URL through device port $device_socks_listen"

if [[ -n "${EXPECT_LOG_REGEX:-}" ]]; then
  log_file="$(mktemp "${TMPDIR:-/tmp}/lantunnel-android-log.XXXXXX")"
  "$adb_bin" "${adb_args[@]}" logcat -d >"$log_file"
  if ! grep -E -q "$EXPECT_LOG_REGEX" "$log_file"; then
    echo "error: EXPECT_LOG_REGEX did not match adb logcat: $EXPECT_LOG_REGEX" >&2
    echo "log saved at: $log_file" >&2
    exit 1
  fi
  echo "ok: log matched EXPECT_LOG_REGEX"
fi
