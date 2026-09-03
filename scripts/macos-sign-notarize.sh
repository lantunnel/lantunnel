#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/macos-sign-notarize.sh <app-bundle> <output-dmg>

Environment:
  MACOS_CODESIGN_IDENTITY   Developer ID signing identity.
                            Default: Developer ID Application
  MACOS_KEYCHAIN_PATH       Optional keychain path for codesign.
                            Example: ~/Library/Keychains/login.keychain-db
  ASC_KEY_ID                App Store Connect API key id.
  ASC_ISSUER_ID             App Store Connect issuer id.
  ASC_KEY_PATH              App Store Connect .p8 key path.
                            Default: ~/.appstoreconnect/private_keys/AuthKey_${ASC_KEY_ID}.p8
  SKIP_NOTARIZE=1           Only sign and package; do not notarize/staple.
                            Not allowed for app bundles that contain the
                            privileged macOS TUN helper unless
                            ALLOW_UNNOTARIZED_MACOS_HELPER=1 is set for local
                            testing.

Example:
  MACOS_CODESIGN_IDENTITY='Developer ID Application: Your Name (YOURTEAMID)' \
  ASC_KEY_ID='YOURKEYID' \
  ASC_ISSUER_ID='00000000-0000-0000-0000-000000000000' \
  scripts/macos-sign-notarize.sh \
    'target/aarch64-apple-darwin/release/bundle/macos/Lantunnel Client.app' \
    'dist/release/lantunnel-client-2.0.8-macos-arm64.dmg'
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -ne 2 ]]; then
  usage >&2
  exit 2
fi

app_src=$1
dmg_out=$2
identity=${MACOS_CODESIGN_IDENTITY:-Developer ID Application}
keychain_path=${MACOS_KEYCHAIN_PATH:-}
skip_notarize=${SKIP_NOTARIZE:-0}

if [[ ! -d "$app_src" ]]; then
  echo "missing app bundle: $app_src" >&2
  exit 1
fi

helper_path="$app_src/Contents/Library/LaunchServices/app.lantunnel.tun-helper"
has_tun_helper=0
if [[ -x "$helper_path" ]]; then
  has_tun_helper=1
fi
if [[ "$has_tun_helper" == "1" && "$skip_notarize" == "1" && "${ALLOW_UNNOTARIZED_MACOS_HELPER:-0}" != "1" ]]; then
  echo "app bundles with the privileged macOS TUN helper must be notarized; SKIP_NOTARIZE=1 is local-only and requires ALLOW_UNNOTARIZED_MACOS_HELPER=1" >&2
  exit 1
fi

if [[ "$skip_notarize" != "1" ]]; then
  : "${ASC_KEY_ID:?ASC_KEY_ID is required unless SKIP_NOTARIZE=1}"
  : "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required unless SKIP_NOTARIZE=1}"
  ASC_KEY_PATH=${ASC_KEY_PATH:-"$HOME/.appstoreconnect/private_keys/AuthKey_${ASC_KEY_ID}.p8"}
  if [[ ! -f "$ASC_KEY_PATH" ]]; then
    echo "missing App Store Connect key: $ASC_KEY_PATH" >&2
    exit 1
  fi
fi

workdir=$(mktemp -d "${TMPDIR:-/tmp}/lantunnel-macos-sign.XXXXXX")
cleanup() {
  rm -rf "$workdir"
}
trap cleanup EXIT

app_name=$(basename "$app_src")
signed_app="$workdir/$app_name"
stage="$workdir/dmg-stage"
codesign_keychain_args=()
if [[ -n "$keychain_path" ]]; then
  codesign_keychain_args=(--keychain "$keychain_path")
fi

codesign_app_executable() {
  local path=$1
  local args=(codesign --force --options runtime --timestamp)
  if [[ ${#codesign_keychain_args[@]} -gt 0 ]]; then
    args+=("${codesign_keychain_args[@]}")
  fi
  args+=(--sign "$identity" "$path")
  "${args[@]}"
}

codesign_dmg() {
  local path=$1
  local args=(codesign --force --timestamp)
  if [[ ${#codesign_keychain_args[@]} -gt 0 ]]; then
    args+=("${codesign_keychain_args[@]}")
  fi
  args+=(--sign "$identity" "$path")
  "${args[@]}"
}

mkdir -p "$(dirname "$dmg_out")"
ditto "$app_src" "$signed_app"
main_executable=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$signed_app/Contents/Info.plist")
main_executable_path="$signed_app/Contents/MacOS/$main_executable"

echo "Signing nested executables with: $identity"
while IFS= read -r -d '' file; do
  if [[ "$file" == "$main_executable_path" ]]; then
    continue
  fi
  codesign_app_executable "$file"
done < <(
  find "$signed_app/Contents/MacOS" "$signed_app/Contents/Resources" "$signed_app/Contents/Library/LaunchServices" \
    -type f -perm -111 -print0 2>/dev/null
)

echo "Signing app bundle"
codesign_app_executable "$signed_app"
if [[ "$has_tun_helper" == "1" ]]; then
  codesign --verify --strict --verbose=2 "$signed_app/Contents/Library/LaunchServices/app.lantunnel.tun-helper"
fi
codesign --verify --deep --strict --verbose=2 "$signed_app"

echo "Creating DMG"
rm -rf "$stage"
mkdir -p "$stage"
ditto "$signed_app" "$stage/$app_name"
# The disk image used to hold nothing but the app, so opening it offered no way
# to install: the only thing to do was drag the icon somewhere and hope. The
# link is what every macOS app ships, and what makes the drag obvious.
ln -s /Applications "$stage/Applications"
rm -f "$dmg_out"
hdiutil create -volname "${app_name%.app}" -srcfolder "$stage" -ov -format UDZO "$dmg_out" >/dev/null

echo "Signing DMG"
codesign_dmg "$dmg_out"
codesign --verify --verbose=2 "$dmg_out"

if [[ "$skip_notarize" != "1" ]]; then
  echo "Notarizing DMG"
  xcrun notarytool submit "$dmg_out" \
    --key-id "$ASC_KEY_ID" \
    --issuer "$ASC_ISSUER_ID" \
    --key "$ASC_KEY_PATH" \
    --wait
  xcrun stapler staple "$dmg_out"
fi

if [[ "$skip_notarize" != "1" ]]; then
  echo "Gatekeeper verification"
  spctl -a -vv --type open --context context:primary-signature "$dmg_out"
fi

echo "Done: $dmg_out"
