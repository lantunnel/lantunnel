#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
repo_dir="$(CDPATH= cd -- "$script_dir/../.." && pwd)"
jni_root="$script_dir/app/src/main/jniLibs"

abis="${ABIS:-arm64-v8a}"
profile="${PROFILE:-release}"

usage() {
  cat <<'USAGE'
Build tp-mobile-ffi for Android and place libtp_mobile_ffi.so under:
  apps/android-proxy/app/src/main/jniLibs/<abi>/libtp_mobile_ffi.so

Environment:
  ABIS     Space-separated Android ABIs. Default: arm64-v8a
           Supported: arm64-v8a armeabi-v7a x86_64 x86
  PROFILE  Cargo profile. Default: release

Prerequisites:
  cargo install cargo-ndk
  rustup target add aarch64-linux-android
  Android NDK available via ANDROID_NDK_HOME or Android SDK ndk install
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required" >&2
  exit 1
fi

if ! cargo ndk --version >/dev/null 2>&1; then
  echo "error: cargo-ndk is required; install it with: cargo install cargo-ndk" >&2
  exit 1
fi

target_args=()
for abi in $abis; do
  case "$abi" in
    arm64-v8a|armeabi-v7a|x86_64|x86)
      target_args+=("-t" "$abi")
      ;;
    *)
      echo "error: unsupported Android ABI: $abi" >&2
      exit 1
      ;;
  esac
done

cargo_args=(
  build
  --manifest-path "$repo_dir/Cargo.toml"
  -p tp-mobile-ffi
)

if [[ "$profile" == "release" ]]; then
  cargo_args+=(--release)
else
  cargo_args+=(--profile "$profile")
fi

mkdir -p "$jni_root"

# cargo ndk writes each ABI to app/src/main/jniLibs/<abi>/libtp_mobile_ffi.so.
cargo ndk "${target_args[@]}" -o "$jni_root" "${cargo_args[@]}"

for abi in $abis; do
  lib="$jni_root/$abi/libtp_mobile_ffi.so"
  if [[ ! -f "$lib" ]]; then
    echo "error: expected native library was not produced: $lib" >&2
    exit 1
  fi
  echo "ok: $lib"
done
