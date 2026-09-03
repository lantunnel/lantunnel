#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
repo_dir="$(CDPATH= cd -- "$script_dir/.." && pwd)"

required_targets=(
  aarch64-apple-ios
  aarch64-apple-ios-sim
  x86_64-apple-ios
)

required_tools=(
  cargo
  ditto
  libtool
  lipo
  make
  rustup
  xcodebuild
  xcrun
)

profile="${PROFILE:-release}"
ios_deployment_target="${IOS_DEPLOYMENT_TARGET:-15.0}"
native_libs_dir="$repo_dir/apps/ios-proxy/NativeLibs"
out_dir="${OUT_DIR:-$native_libs_dir}"
check_only=0

tp_module_dir="$native_libs_dir/TpMobileFfi"
tp_header_dir="$tp_module_dir/include"
tp_header="$tp_header_dir/tp_mobile_ffi.h"
tp_modulemap="$tp_module_dir/module.modulemap"
tp_xcframework="$out_dir/TpMobileFfi.xcframework"

hev_dir="$repo_dir/apps/android-proxy/app/src/main/jni/hev-socks5-tunnel"
hev_script="$hev_dir/build-apple.sh"
hev_source_xcframework="$hev_dir/HevSocks5Tunnel.xcframework"
hev_xcframework="$out_dir/HevSocks5Tunnel.xcframework"

usage() {
  cat <<'USAGE'
Build iOS native libraries for the Tunnel Proxy mobile app.

Usage:
  scripts/build-ios-mobile-libs.sh [--check]

Options:
  --check   Validate Rust targets, Xcode tools, and native-library inputs, then exit.

Environment:
  PROFILE                 Cargo profile to build. Default: release
  IOS_DEPLOYMENT_TARGET   Minimum iOS version for Rust C/asm deps. Default: 15.0
  OUT_DIR                 Output directory. Default: apps/ios-proxy/NativeLibs

Required Rust targets:
  aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check)
      check_only=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

missing_tools=()
for tool in "${required_tools[@]}"; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    missing_tools+=("$tool")
  fi
done

if [[ ${#missing_tools[@]} -gt 0 ]]; then
  echo "error: missing required build tool(s):" >&2
  for tool in "${missing_tools[@]}"; do
    echo "  $tool" >&2
  done
  echo >&2
  echo "Install Rust and Xcode Command Line Tools, then retry:" >&2
  echo "  rustup target add ${required_targets[*]}" >&2
  echo "  xcode-select --install" >&2
  exit 1
fi

installed_targets="$(rustup target list --installed)"
missing_targets=()
for target in "${required_targets[@]}"; do
  if ! grep -qx "$target" <<<"$installed_targets"; then
    missing_targets+=("$target")
  fi
done

if [[ ${#missing_targets[@]} -gt 0 ]]; then
  echo "error: missing required iOS Rust target(s):" >&2
  for target in "${missing_targets[@]}"; do
    echo "  $target" >&2
  done
  echo >&2
  echo "Install with:" >&2
  echo "  rustup target add ${missing_targets[*]}" >&2
  exit 1
fi

missing_inputs=()
for input in "$tp_header" "$tp_modulemap"; do
  if [[ ! -f "$input" ]]; then
    missing_inputs+=("$input")
  fi
done

if [[ ! -d "$hev_dir" ]]; then
  missing_inputs+=("$hev_dir")
elif [[ ! -f "$hev_script" && ! -d "$hev_source_xcframework" ]]; then
  missing_inputs+=("$hev_script or $hev_source_xcframework")
fi

if [[ ${#missing_inputs[@]} -gt 0 ]]; then
  echo "error: missing required iOS native-library input(s):" >&2
  for input in "${missing_inputs[@]}"; do
    echo "  $input" >&2
  done
  exit 1
fi

if [[ "$check_only" -eq 1 ]]; then
  echo "ok: iOS Rust targets, Xcode tools, and native-library inputs are available"
  exit 0
fi

cargo_profile_args=()
artifact_profile="$profile"
case "$profile" in
  release)
    cargo_profile_args+=(--release)
    artifact_profile="release"
    ;;
  dev|debug)
    cargo_profile_args+=(--profile dev)
    artifact_profile="debug"
    ;;
  *)
    cargo_profile_args+=(--profile "$profile")
    artifact_profile="$profile"
    ;;
esac

mkdir -p "$out_dir"

tp_device_lib=""
tp_sim_arm64_lib=""
tp_sim_x86_64_lib=""
for target in "${required_targets[@]}"; do
  echo "building tp-mobile-ffi for $target"
  IPHONEOS_DEPLOYMENT_TARGET="$ios_deployment_target" \
  IPHONESIMULATOR_DEPLOYMENT_TARGET="$ios_deployment_target" \
  cargo build \
    --manifest-path "$repo_dir/Cargo.toml" \
    -p tp-mobile-ffi \
    --target "$target" \
    "${cargo_profile_args[@]}"

  lib="$repo_dir/target/$target/$artifact_profile/libtp_mobile_ffi.a"
  if [[ ! -f "$lib" ]]; then
    echo "error: expected static library was not produced: $lib" >&2
    exit 1
  fi

  case "$target" in
    aarch64-apple-ios)
      tp_device_lib="$lib"
      ;;
    aarch64-apple-ios-sim)
      tp_sim_arm64_lib="$lib"
      ;;
    x86_64-apple-ios)
      tp_sim_x86_64_lib="$lib"
      ;;
  esac
done

tp_sim_universal_dir="$out_dir/TpMobileFfiBuild/ios-arm64_x86_64-simulator"
tp_sim_universal_lib="$tp_sim_universal_dir/libtp_mobile_ffi.a"
rm -rf "$out_dir/TpMobileFfiBuild"
mkdir -p "$tp_sim_universal_dir"

echo "creating universal simulator tp-mobile-ffi static library"
lipo -create \
  "$tp_sim_arm64_lib" \
  "$tp_sim_x86_64_lib" \
  -output "$tp_sim_universal_lib"

echo "packaging $tp_xcframework"
rm -rf "$tp_xcframework"
xcodebuild -create-xcframework \
  -library "$tp_device_lib" -headers "$tp_module_dir" \
  -library "$tp_sim_universal_lib" -headers "$tp_module_dir" \
  -output "$tp_xcframework"
rm -rf "$out_dir/TpMobileFfiBuild"
echo "ok: $tp_xcframework"

copy_hev_xcframework() {
  local source="$1"

  if [[ ! -d "$source" ]]; then
    echo "error: expected HevSocks5Tunnel.xcframework was not produced: $source" >&2
    exit 1
  fi

  echo "copying HevSocks5Tunnel.xcframework to $hev_xcframework"
  ditto "$source" "$hev_xcframework"
  echo "ok: $hev_xcframework"
}

if [[ -f "$hev_script" ]]; then
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/tp-ios-libs.XXXXXX")"
  trap 'rm -rf "$tmp_dir"' EXIT

  hev_work_dir="$tmp_dir/hev-socks5-tunnel"
  cp -R "$hev_dir" "$hev_work_dir"

  echo "building HevSocks5Tunnel.xcframework from vendored source"
  (cd "$hev_work_dir" && bash ./build-apple.sh)
  copy_hev_xcframework "$hev_work_dir/HevSocks5Tunnel.xcframework"
else
  copy_hev_xcframework "$hev_source_xcframework"
fi
