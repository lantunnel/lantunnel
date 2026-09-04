#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAKEFILE="$ROOT_DIR/Makefile"
DOCKERFILE="$ROOT_DIR/Dockerfile"
RELEASE_WORKFLOW="$ROOT_DIR/.github/workflows/release.yml"

# A manual candidate is identified by an immutable source commit, so it can be
# built and accepted before the formal release tag exists.
workflow_header="$(sed -n '1,/^jobs:/p' "$RELEASE_WORKFLOW")"
grep -Fq '      source_commit:' <<<"$workflow_header"
grep -Fq '        description: "Exact 40-hex source commit at dispatched main HEAD"' <<<"$workflow_header"
grep -Fq '        required: true' <<<"$(sed -n '/^      source_commit:/,/^permissions:/p' <<<"$workflow_header")"
if grep -Fq '      tag:' <<<"$workflow_header"; then
  echo 'manual candidate workflow still requires a pre-existing release tag' >&2
  exit 1
fi

# Local release/test generators must never become candidate source files.
for generated_path in \
  .superpowers/contract.json \
  apps/lantunnel-client/frontend/dist/index.html \
  apps/lantunnel-client/src-tauri/.tauri-build-override-contract.json \
  apps/lantunnel-client/src-tauri/gen/contract.json
do
  git -C "$ROOT_DIR" check-ignore -q -- "$generated_path"
done

# Manual candidate builds keep the stable workspace gates, while a tag only
# republishes the already accepted and checksummed R2 bytes.
prepare_job="$(sed -n '/^  manual-build-prepare:/,/^  build-cli:/p' "$RELEASE_WORKFLOW")"
grep -Fq "if: github.event_name == 'workflow_dispatch'" <<<"$prepare_job"
grep -Eq 'uses: dtolnay/rust-toolchain@[0-9a-f]{40} # stable' <<<"$prepare_job"
grep -Fq 'components: rustfmt, clippy' <<<"$prepare_job"

# The Ubuntu workspace gates compile protobuf consumers through an explicitly
# installed and pinned protoc, and fail before compilation if it is unavailable.
prepare_job_header="$(sed -n '1,/^    steps:/p' <<<"$prepare_job")"
grep -Fq '      PROTOC: /usr/bin/protoc' <<<"$prepare_job_header"
grep -Fq 'protobuf-compiler' <<<"$prepare_job"
grep -Fq 'test -x "$PROTOC"' <<<"$prepare_job"
grep -Fq '"$PROTOC" --version' <<<"$prepare_job"
protobuf_install_line="$(grep -nF 'protobuf-compiler' <<<"$prepare_job" | head -n 1 | cut -d: -f1)"
protoc_executable_probe_line="$(grep -nF 'test -x "$PROTOC"' <<<"$prepare_job" | head -n 1 | cut -d: -f1)"
protoc_version_probe_line="$(grep -nF '"$PROTOC" --version' <<<"$prepare_job" | head -n 1 | cut -d: -f1)"
first_cargo_compile_line="$(grep -nE 'run: cargo (clippy|test|check|build)' <<<"$prepare_job" | head -n 1 | cut -d: -f1)"
if [ -z "$protobuf_install_line" ] \
  || [ -z "$protoc_executable_probe_line" ] \
  || [ -z "$protoc_version_probe_line" ] \
  || [ -z "$first_cargo_compile_line" ] \
  || [ "$protobuf_install_line" -ge "$protoc_executable_probe_line" ] \
  || [ "$protoc_executable_probe_line" -ge "$protoc_version_probe_line" ] \
  || [ "$protoc_version_probe_line" -ge "$first_cargo_compile_line" ]; then
  echo 'manual Ubuntu workspace gates must install and probe pinned protoc before compiling' >&2
  exit 1
fi

grep -Fq 'cargo fmt --all -- --check' <<<"$prepare_job"
grep -Fq 'cargo clippy --workspace --all-targets -- -D warnings' <<<"$prepare_job"
grep -Fq 'cargo test --workspace' <<<"$prepare_job"
grep -Fq 'name: cargo-deny (advisories + licenses + bans + sources)' <<<"$prepare_job"
grep -Eq 'uses: EmbarkStudios/cargo-deny-action@[0-9a-f]{40} # v2' <<<"$prepare_job"
grep -Fq 'command: check all' <<<"$prepare_job"

# Mobile source contracts inspect the generated bundles, so a clean release
# checkout must stage them before running those contracts.
mobile_ui_stage_line="$(grep -nF 'run: make _stage-android-ui _stage-ios-ui' <<<"$prepare_job" | head -n 1 | cut -d: -f1)"
mobile_contract_line="$(grep -nF 'bash tests/android_mobile_source_test.sh' <<<"$prepare_job" | head -n 1 | cut -d: -f1)"
if [ -z "$mobile_ui_stage_line" ] || [ -z "$mobile_contract_line" ] || [ "$mobile_ui_stage_line" -ge "$mobile_contract_line" ]; then
  echo 'manual release gates must stage mobile UI bundles before source contracts' >&2
  exit 1
fi

for source_contract in \
  v2_public_release_surface_test.sh \
  v2_client_only_source_test.sh \
  v2_platform_api_only_source_test.sh \
  v2_operator_authority_source_test.sh \
  android_mobile_source_test.sh \
  ios_mobile_source_test.sh \
  tun_sidecar_release_source_test.sh \
  v2_windows_secret_acl_source_test.sh \
  v2_relay_hotpath_source_test.sh \
  v2_release_packaging_source_test.sh
do
  grep -Fq "bash tests/$source_contract" <<<"$prepare_job"
done
for artifact_job in \
  build-cli \
  build-client-linux \
  build-client-macos \
  build-client-windows
do
  artifact_job_block="$(sed -n "/^  ${artifact_job}:/,/^  [[:alnum:]_-]\\+:/p" "$RELEASE_WORKFLOW")"
  grep -Fq '    needs: manual-build-prepare' <<<"$artifact_job_block"
  grep -Fq "if: github.event_name == 'workflow_dispatch'" <<<"$artifact_job_block"
done
manual_bundle_dependency_block="$(sed -n '/^  manual-build-bundle:/,/^    if:/p' "$RELEASE_WORKFLOW")"
grep -Fq '      - manual-build-prepare' <<<"$manual_bundle_dependency_block"

# Every public production binary uses the performance release profile. Tauri
# owns the `--release` flag internally, so its public build path must override
# that profile to the equivalent opt-level=3 instead of silently using opt-z.
grep -q '^BUILD_PROFILE[[:space:]]*?=[[:space:]]*release-perf' "$MAKEFILE"
grep -Fq -- '--profile $(BUILD_PROFILE)' "$MAKEFILE"
if grep -Eq 'cargo (build|zigbuild|xwin build) --release' "$MAKEFILE"; then
  echo 'public Makefile build path still uses the size-oriented release profile' >&2
  exit 1
fi
grep -q 'CARGO_PROFILE_RELEASE_OPT_LEVEL=3' "$MAKEFILE"

grep -q '^ARG BUILD_PROFILE=release-perf' "$DOCKERFILE"
grep -q 'cargo build --profile "\${BUILD_PROFILE}"' "$DOCKERFILE"
if grep -q 'cargo build --release' "$DOCKERFILE"; then
  echo 'production Dockerfile still uses the size-oriented release profile' >&2
  exit 1
fi

grep -q 'BUILD_PROFILE: release-perf' "$RELEASE_WORKFLOW"

# macOS and Linux ship real Tauri bundles. Windows ships the executable
# itself: the NSIS installer was dropped deliberately, so the assertions that
# demanded it are gone rather than left failing. What has to hold for Windows
# is that the executable carries its TUN sidecar, since a raw Tauri binary
# without it installs and then routes nothing.
build_ui="$(sed -n '/^_build-ui:/,/^\.PHONY: _package-ui/p' "$MAKEFILE")"
grep -q -- '--bundles app' <<<"$build_ui"
grep -q -- '--bundles appimage' <<<"$build_ui"
grep -q 'LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR' <<<"$build_ui"
package_ui="$(sed -n '/^_package-ui-download:/,/^\.PHONY: _ensure-builder/p' "$MAKEFILE")"
grep -q 'bundle/appimage' <<<"$package_ui"
grep -q 'bundle/macos' <<<"$package_ui"
if ! grep -q 'carries no embedded TUN sidecar' <<<"$package_ui"; then
  echo 'Windows packaging no longer refuses an executable without its TUN sidecar' >&2
  exit 1
fi
if grep -q 'appimage="target/$(TRIPLE)/release/$(TAURI_BIN)"' <<<"$package_ui"; then
  echo 'Linux release package still falls back to a raw Tauri executable' >&2
  exit 1
fi

# Windows always receives the elevation manifest. Supplying a sidecar bundle
# directory is an all-or-nothing contract: all three assets must be present.
grep -q 'if target_os == "windows" {' "$ROOT_DIR/apps/lantunnel-client/src-tauri/build.rs"
if grep -q 'target_os == "windows" && has_tun_sidecar_dir' "$ROOT_DIR/apps/lantunnel-client/src-tauri/build.rs"; then
  echo 'Windows elevation manifest is conditional on sidecar discovery' >&2
  exit 1
fi
grep -q 'missing required Windows TUN sidecar asset' "$ROOT_DIR/apps/lantunnel-client/src-tauri/build.rs"

# Windows signing remains supported when a certificate is available. The
# current canonical Windows preview is intentionally unsigned and the release
# workflow must opt into that state explicitly instead of depending on a PFX.
grep -q 'WINDOWS_CERTIFICATE_THUMBPRINT' "$MAKEFILE"
grep -q 'WINDOWS_TIMESTAMP_URL' "$MAKEFILE"
grep -q 'ALLOW_UNSIGNED_WINDOWS_INSTALLER' "$MAKEFILE"
grep -q 'Windows release requires WINDOWS_CERTIFICATE_THUMBPRINT or explicit ALLOW_UNSIGNED_WINDOWS_INSTALLER=1' "$MAKEFILE"
grep -q 'building the canonical unsigned Windows preview' "$MAKEFILE"

# The unsigned preview is the shipped Windows product, so the Makefile has to
# say so. Leaving it to an environment variable meant forgetting it failed the
# Windows target and took every target queued after it down with it, silently.
grep -q '^ALLOW_UNSIGNED_WINDOWS_INSTALLER ?= 1' "$MAKEFILE"
if grep -q 'ALLOW_UNSIGNED_WINDOWS_INSTALLER:-0' "$MAKEFILE"; then
  echo 'Windows signing gate still reads the environment instead of the pinned default' >&2
  exit 1
fi
grep -q 'if \[ "$(ALLOW_UNSIGNED_WINDOWS_INSTALLER)" != "1" \]' "$MAKEFILE"
if grep -q 'local, non-publishable fixture\|unsigned local Windows fixture; do not publish it' "$MAKEFILE"; then
  echo 'Windows unsigned-preview wording still claims the canonical artifact cannot be published' >&2
  exit 1
fi
grep -q 'certificateThumbprint' "$MAKEFILE"
grep -q 'ALLOW_UNSIGNED_MACOS_DMG' "$MAKEFILE"

windows_job="$(sed -n '/^  build-client-windows:/,/^  manual-build-bundle:/p' "$RELEASE_WORKFLOW")"
grep -q 'name: lantunnel-client windows-amd64 unsigned preview NSIS' <<<"$windows_job"
if grep -q 'WINDOWS_CERTIFICATE_PFX_BASE64\|WINDOWS_CERTIFICATE_PASSWORD\|Import-PfxCertificate' <<<"$windows_job"; then
  echo 'unsigned Windows preview still depends on a production signing PFX' >&2
  exit 1
fi
grep -q 'Verify unsigned Windows TUN sidecar assets and signed Wintun' <<<"$windows_job"
grep -Fq '$signature.Status -ne '\''NotSigned'\''' <<<"$windows_job"
grep -Fq '$wintunSignature.Status -ne '\''Valid'\''' <<<"$windows_job"
if grep -q 'Set-AuthenticodeSignature' <<<"$windows_job"; then
  echo 'unsigned Windows preview still signs a Lantunnel-owned sidecar asset' >&2
  exit 1
fi
grep -q 'Build unsigned preview NSIS installer through the public Make release path' <<<"$windows_job"
installer_verification="$(sed -n '/Verify unsigned preview installer is NotSigned/,/uses: actions\/upload-artifact@v4/p' <<<"$windows_job")"
grep -Fq 'Get-AuthenticodeSignature -LiteralPath $artifact' <<<"$installer_verification"
grep -Fq '$signature.Status -ne '\''NotSigned'\''' <<<"$installer_verification"
grep -q 'expected unsigned Windows preview installer' <<<"$installer_verification"

frontend_build="$(sed -n '/Build lantunnel-client frontend once for Windows/,/uses: Swatinem\/rust-cache@/p' <<<"$windows_job")"
grep -q 'shell: powershell' <<<"$frontend_build"
grep -Fq 'npm --prefix apps/lantunnel-client/frontend ci --no-audit --no-fund' <<<"$frontend_build"
grep -Fq 'npm --prefix apps/lantunnel-client/frontend run build' <<<"$frontend_build"
grep -Fq '$nodeDirectory = Split-Path (Get-Command node).Source' <<<"$frontend_build"
grep -Fq 'WINDOWS_NODE_DIRECTORY=$nodeDirectory' <<<"$frontend_build"

msys_setup_line="$(grep -n 'uses: msys2/setup-msys2@' <<<"$windows_job" | cut -d: -f1)"
windows_acl_line="$(grep -n 'name: Windows secret ACL tests' <<<"$windows_job" | cut -d: -f1)"
if [ -z "$msys_setup_line" ] || [ -z "$windows_acl_line" ] || [ "$msys_setup_line" -ge "$windows_acl_line" ]; then
  echo 'MSYS protobuf must be installed before Windows cargo tests compile tp-transport' >&2
  exit 1
fi
grep -q 'install: make gcc curl unzip mingw-w64-x86_64-protobuf' <<<"$windows_job"
windows_acl_tests="$(sed -n '/name: Windows secret ACL tests/,/^      - /p' <<<"$windows_job")"
grep -q 'shell: msys2 {0}' <<<"$windows_acl_tests"
grep -Fq 'export PATH="$(cygpath -u "$CARGO_HOME")/bin:$PATH"' <<<"$windows_acl_tests"
grep -Fq 'export PROTOC="$(cygpath -w /mingw64/bin/protoc.exe)"' <<<"$windows_acl_tests"
grep -Fq '/mingw64/bin/protoc.exe --version' <<<"$windows_acl_tests"

windows_nsis_build="$(sed -n '/Build unsigned preview NSIS installer through the public Make release path/,/Verify unsigned preview installer is NotSigned/p' <<<"$windows_job")"
grep -Fq 'export PATH="$(cygpath -u "$WINDOWS_NODE_DIRECTORY"):$(cygpath -u "$CARGO_HOME")/bin:$PATH"' <<<"$windows_nsis_build"
grep -Fq 'export PROTOC="$(cygpath -w /mingw64/bin/protoc.exe)"' <<<"$windows_nsis_build"
grep -Fq 'node --version' <<<"$windows_nsis_build"
grep -Fq 'cargo --version' <<<"$windows_nsis_build"
grep -Fq '/mingw64/bin/protoc.exe --version' <<<"$windows_nsis_build"
grep -Fq 'ALLOW_UNSIGNED_WINDOWS_INSTALLER=1 SKIP_UI_FRONTEND=1 make' <<<"$windows_nsis_build"

# The separate manual candidate build invokes the existing Make targets for
# the supported product/platform matrix: two Gateway, two Admin, five Client.
for target in \
  _release-lantunnel-gateway-macos-arm64 \
  _release-lantunnel-gateway-linux-amd64 \
  _release-lantunnel-admin-macos-arm64 \
  _release-lantunnel-admin-linux-amd64 \
  _release-lantunnel-client-macos-arm64 \
  _release-lantunnel-client-macos-amd64 \
  _release-lantunnel-client-windows-amd64 \
  _release-lantunnel-client-linux-amd64 \
  _release-lantunnel-client-linux-arm64
do
  grep -q "$target" "$RELEASE_WORKFLOW"
done
if grep -Eq 'cargo (build|zigbuild|xwin build)' "$RELEASE_WORKFLOW"; then
  echo 'GitHub Release duplicates the Makefile production build path' >&2
  exit 1
fi
if grep -q '\.tar\.gz' "$RELEASE_WORKFLOW"; then
  echo 'GitHub Release wraps the established product artifacts in generic tarballs' >&2
  exit 1
fi

for artifact in \
  'lantunnel-gateway-${version}-aarch64-apple-darwin' \
  'lantunnel-gateway-${version}-x86_64-unknown-linux-musl' \
  'lantunnel-admin-${version}-aarch64-apple-darwin' \
  'lantunnel-admin-${version}-x86_64-unknown-linux-musl' \
  'lantunnel-client-${version}-macos-arm64.dmg' \
  'lantunnel-client-${version}-macos-amd64.dmg' \
  'lantunnel-client-${version}-windows-amd64.exe' \
  'lantunnel-client-${version}-linux-amd64.AppImage' \
  'lantunnel-client-${version}-linux-arm64.AppImage'
do
  grep -Fq "$artifact" "$RELEASE_WORKFLOW"
done
grep -q 'refusing incomplete or extra public release artifacts' "$RELEASE_WORKFLOW"

# The tag-triggered publish job and its pinned 2.0.0 provenance were removed:
# both were bound to a commit and a CHANGELOG digest that no longer exist. The
# manual multi-OS build is the surviving path, and it may not publish.
manual_bundle="$(sed -n '/^  manual-build-bundle:/,$p' "$RELEASE_WORKFLOW")"
grep -Fq 'sha256sum "${expected[@]}" > checksums.txt' <<<"$manual_bundle"
grep -Fq './scripts/upload.sh "$VERSION" check' <<<"$manual_bundle"
grep -Eq 'uses: actions/upload-artifact@[0-9a-f]{40} # v4' <<<"$manual_bundle"
if grep -Eq 'upload\.sh.*remote|gh release (create|upload)|R2_SECRET_ACCESS_KEY' <<<"$manual_bundle"; then
  echo 'manual candidate build can overwrite the accepted formal release' >&2
  exit 1
fi

# R2 permits only an interrupted-release subset before write, and demands the
# full product-plus-fixed-metadata set afterward. It never silently falls back
# to a legacy bucket or hides a failed remote listing.
grep -Fq 'R2_BUCKET_NAME="${R2_BUCKET_NAME:-}"' "$ROOT_DIR/scripts/upload.sh"
grep -Fq 'verify_remote_manifest subset' "$ROOT_DIR/scripts/upload.sh"
grep -Fq 'verify_remote_manifest exact' "$ROOT_DIR/scripts/upload.sh"
grep -Fq 'remote_manifest_keys' "$ROOT_DIR/scripts/upload.sh"
grep -Fq 'download_payload_remote' "$ROOT_DIR/scripts/upload.sh"
if grep -Fq '|| true' <(sed -n '/^list_remote()/,/^}/p' "$ROOT_DIR/scripts/upload.sh"); then
  echo 'R2 listing must fail closed' >&2
  exit 1
fi

# macOS publish still requires its signing identity. Windows TUN inputs are
# reproducible: HEV is built from this checkout and Wintun is pinned to the
# upstream signed archive and digest.
grep -q 'MACOS_CERTIFICATE_P12_BASE64' "$RELEASE_WORKFLOW"
grep -q 'ASC_PRIVATE_KEY_P8_BASE64' "$RELEASE_WORKFLOW"
grep -q 'Get-AuthenticodeSignature' "$RELEASE_WORKFLOW"
grep -q 'xcrun stapler validate' "$RELEASE_WORKFLOW"
grep -q 'https://www.wintun.net/builds/wintun-0.14.1.zip' "$RELEASE_WORKFLOW"
grep -q '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51' "$RELEASE_WORKFLOW"
grep -q 'prebuilt_dir/wintun.dll' "$MAKEFILE"

ruby -e 'require "yaml"; YAML.load_file(ARGV.fetch(0))' "$RELEASE_WORKFLOW"

# --- release container surface ----------------------------------------------
# Moved here from v2_container_release_source_test.sh, which had shrunk to
# these Dockerfile assertions plus two `test -e` checks for V1 files that were
# deleted with V1 and cannot come back.
while IFS= read -r container_source; do
  if grep -Eq -- 'anyproxy-client|configs/client\.yaml' "$container_source"; then
    echo "container source still exposes the removed Legacy Client: ${container_source#"$ROOT_DIR"/}" >&2
    exit 1
  fi
done < <(
  rg --files --hidden \
    -g 'Dockerfile*' -g '*compose*.yml' -g '*compose*.yaml' \
    -g '!.git/**' -g '!target/**' -g '!dist/**' \
    "$ROOT_DIR"
)

# The image builds the three public V2 products and nothing else.
grep -q -- '-p lantunnel-gateway' "$DOCKERFILE"
grep -q -- '-p lantunnel-client' "$DOCKERFILE"
grep -q -- '-p lantunnel-admin' "$DOCKERFILE"
grep -q -- '/out/lantunnel-gateway' "$DOCKERFILE"
grep -q -- '/out/lantunnel-client' "$DOCKERFILE"
grep -q -- '/out/lantunnel-admin' "$DOCKERFILE"

# A release image bakes no seed, no key, and no config.
for forbidden in \
  '-p anyproxy-client' \
  '/out/anyproxy-client' \
  'CMD .*--seed' \
  'generate_certs' \
  'server.key' \
  'COPY configs/'
do
  if grep -q -- "$forbidden" "$DOCKERFILE"; then
    echo "release container contains forbidden baked/test surface: $forbidden" >&2
    exit 1
  fi
done
grep -q -- 'CMD \["lantunnel-gateway", "--config", "/run/lantunnel/gateway.yaml"\]' "$DOCKERFILE"

echo 'v2 release packaging source: PASS'
