# Lantunnel — Makefile
#
# Three public Lantunnel 2.0 binaries:
#   - lantunnel-gateway
#   - lantunnel-client       (UI by default, --headless/--no-ui without UI)
#   - lantunnel-admin        (offline Tunnel and Peer provisioning)
#
# Public targets are release-oriented and include platform/arch in the
# target name, e.g. `release-lantunnel-client-macos-arm64` for lantunnel-client.
# The lower-level
# `_build-*` dispatchers stay internal so release automation has one stable
# surface.
#
# Override the docker image coordinates for the gateway container with
# DOCKER_REGISTRY / DOCKER_IMAGE / DOCKER_TAG.

.DEFAULT_GOAL := help

# ------------------------------ variables ----------------------------------

CARGO           ?= cargo
BUILD_PROFILE   ?= release-perf
WORKSPACE_VERSION := $(shell sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
ifeq ($(strip $(WORKSPACE_VERSION)),)
WORKSPACE_VERSION := dev
endif
VERSION         ?= $(WORKSPACE_VERSION)
UI_VERSION_DEFAULT := $(shell sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' apps/lantunnel-client/src-tauri/tauri.conf.json | head -n 1)
ifeq ($(strip $(UI_VERSION_DEFAULT)),)
UI_VERSION_DEFAULT := $(VERSION)
endif
ifeq ($(origin VERSION),command line)
UI_VERSION      ?= $(VERSION)
else
UI_VERSION      ?= $(UI_VERSION_DEFAULT)
endif
COMMIT          := $(shell git rev-parse --short HEAD 2>/dev/null || echo "unknown")
HOST_OS         := $(shell uname -s | tr A-Z a-z)
HOST_ARCH       := $(shell uname -m)
# Docker platform arch for the host (arm64|amd64). Used to pick the
# matching rustup host-cache subdir so we don't mix arm64 and amd64
# rustc/cargo binaries in one bind mount.
HOST_ARCH_DOCKER := $(if $(filter arm64 aarch64,$(HOST_ARCH)),arm64,amd64)

DIST_DIR        := dist
RELEASE_DIR     := $(DIST_DIR)/release
DOWNLOAD_DIR    := $(RELEASE_DIR)
CONFIG_DIR      := configs
CERT_DIR        := certs
CERT_FILE       := $(CERT_DIR)/server.crt
CERT_KEY        := $(CERT_DIR)/server.key
UPLOAD_ENV      ?= upload.env
PLATFORM_DIR    ?=
UPLOAD_DIRECT   ?= 1

# Docker builder image used for linux/windows cross-compiles.
#
# For *-unknown-linux-musl and *-pc-windows-msvc we rely on cargo-zigbuild /
# cargo-xwin, which are arch-neutral inside any Linux container — so we can
# use the host-native image (BUILDER_IMAGE) regardless of the target.
#
# For *-unknown-linux-gnu (Tauri UI) we cannot cross-compile cleanly: ring's
# cc build-script + webkit2gtk/appindicator/rsvg -dev packages only exist in
# one arch per container. Instead we run a native-arch container per target
# (via `--platform=linux/<arch>`) so gcc, pkg-config, and all -dev headers
# match the target triple. On a mismatched host Docker uses QEMU (slower,
# but reliable). One image per platform; both share the cargo cache volume.
BUILDER_IMAGE         ?= tp-builder:latest
BUILDER_IMAGE_AMD64   ?= tp-builder:amd64
BUILDER_IMAGE_ARM64   ?= tp-builder:arm64
BUILDER_IMAGE_NSIS    ?= tp-builder-nsis:amd64

# ---- macOS release signing -------------------------------------------------
# Apple signing identities. Not secrets, but account-specific: they name which
# certificate and which App Store Connect key to use. The private material is
# the .p8 and the keychain, and neither is in this repository.
#
# These are deliberately empty here so a fork never inherits another
# maintainer's Apple identity. Set them per release machine in release.local.mk
# (gitignored) or in the environment:
#
#   MACOS_CODESIGN_IDENTITY  Developer ID cert. Prefer the SHA-1 hash over the
#                            subject name when one keychain carries two certs
#                            with the same name — codesign refuses an ambiguous
#                            match.
#   ASC_KEY_ID               App Store Connect API key ID.
#   ASC_ISSUER_ID            App Store Connect issuer ID.
#
# The signing script defaults ASC_KEY_PATH to
# ~/.appstoreconnect/private_keys/AuthKey_$(ASC_KEY_ID).p8 — the one file that
# must never be committed.
-include release.local.mk
MACOS_CODESIGN_IDENTITY ?=
ASC_KEY_ID              ?=
ASC_ISSUER_ID           ?=
export MACOS_CODESIGN_IDENTITY ASC_KEY_ID ASC_ISSUER_ID
BUILDER_DOCKERFILE    := Dockerfile.builder
BUILDER_DOCKERFILE_NSIS := Dockerfile.builder-nsis

# Build caches live on the host under $(CACHE_DIR) so they survive across
# docker runs, can be inspected / `rm -rf`ed, and are .gitignore'd.
#
# Layout (all bind-mounted into the builder container):
#   $(CACHE_DIR)/cargo-registry → /usr/local/cargo/registry  (crate tarballs + index)
#   $(CACHE_DIR)/cargo-git      → /usr/local/cargo/git       (git dependencies)
#   $(CACHE_DIR)/npm            → /root/.npm                 (npm ci tarball cache)
#   $(CACHE_DIR)/xwin           → /root/.cache/cargo-xwin    (cargo-xwin MSVC CRT/SDK)
#   $(CACHE_DIR)/zig            → /root/.cache/zig           (cargo-zigbuild zig compiler cache)
#   $(CACHE_DIR)/rustup-<arch>  → /usr/local/rustup          (rustc/cargo/std + targets)
#
# The rustup cache is split by container arch (arm64|amd64) because rustup
# stores arch-specific host binaries. The linux-gnu cross-builds run with
# --platform=linux/<arch>; they use the matching rustup-<arch> cache.
#
# First-run bootstrap: .cache/rustup-<arch>/ is empty, so _ensure-builder*
# below runs a one-shot container that copies /opt/rustup-seed (snapshot
# baked into the builder image) into the host cache. This preserves the
# pre-installed "stable" toolchain + musl/msvc targets and avoids a ~500MB
# rustup download on first build against every fresh cache.
#
# The `target/` directory is already bind-mounted via $(PWD) (whole repo) so
# compiled artefacts persist per-triple; switching triple still triggers a
# dep rebuild.
CACHE_DIR              ?= $(PWD)/.cache
CARGO_REGISTRY_CACHE   := $(CACHE_DIR)/cargo-registry
CARGO_GIT_CACHE        := $(CACHE_DIR)/cargo-git
NPM_CACHE              := $(CACHE_DIR)/npm
XWIN_CACHE             := $(CACHE_DIR)/xwin
ZIG_CACHE              := $(CACHE_DIR)/zig
XWIN_HTTP_RETRIES      ?= 10
RUSTUP_CACHE_NATIVE    := $(CACHE_DIR)/rustup-$(HOST_ARCH_DOCKER)
RUSTUP_CACHE_AMD64     := $(CACHE_DIR)/rustup-amd64
RUSTUP_CACHE_ARM64     := $(CACHE_DIR)/rustup-arm64
DOCKER_CACHE_COMMON     = -v $(CARGO_REGISTRY_CACHE):/usr/local/cargo/registry \
                         -v $(CARGO_GIT_CACHE):/usr/local/cargo/git \
                         -v $(NPM_CACHE):/root/.npm \
                         -v $(XWIN_CACHE):/root/.cache/cargo-xwin \
                         -v $(ZIG_CACHE):/root/.cache/zig

# Forward the host's proxy into the builder container so cargo / cargo-xwin /
# npm can reach external CDNs (aka.ms, visualstudio.com, crates.io) on
# networks where direct egress is blocked. Docker Desktop exposes the host
# at `host.docker.internal`; if the proxy is bound to 127.0.0.1 on the host
# we rewrite it since that address loops back to the container, not the host.
# When HTTPS_PROXY is unset, PROXY_ENV expands to empty and behavior is
# identical to the previous recipe.
# Local `,` helper — `comma` is also redefined below for `_triples`; both
# assignments produce the same value, so the later one is harmless.
comma := ,
PROXY_FOR_DOCKER := $(subst 127.0.0.1,host.docker.internal,$(HTTPS_PROXY))
PROXY_ENV := $(if $(PROXY_FOR_DOCKER),-e HTTPS_PROXY=$(PROXY_FOR_DOCKER) -e HTTP_PROXY=$(PROXY_FOR_DOCKER) -e NO_PROXY=localhost$(comma)host.docker.internal,)
XWIN_ENV := -e XWIN_HTTP_RETRIES=$(XWIN_HTTP_RETRIES)

DOCKER_RUN_BUILDER      = docker run --rm \
    $(PROXY_ENV) \
    $(XWIN_ENV) \
    -v $(PWD):/src \
    $(DOCKER_CACHE_COMMON) \
    -v $(RUSTUP_CACHE_NATIVE):/usr/local/rustup \
    -w /src \
    $(BUILDER_IMAGE)

.PHONY: _ensure-cache-dirs
_ensure-cache-dirs:
	@mkdir -p $(CARGO_REGISTRY_CACHE) $(CARGO_GIT_CACHE) $(NPM_CACHE) \
	          $(XWIN_CACHE) $(ZIG_CACHE) \
	          $(RUSTUP_CACHE_NATIVE) $(RUSTUP_CACHE_AMD64) $(RUSTUP_CACHE_ARM64)

# Gateway docker image (x86_64 linux only, gateway binary only).
DOCKER_REGISTRY ?=
DOCKER_IMAGE    ?= lantunnel-gateway
DOCKER_TAG      ?= $(VERSION)
DOCKER_FULL     := $(if $(DOCKER_REGISTRY),$(DOCKER_REGISTRY)/,)$(DOCKER_IMAGE)

# Public release target names use platform/arch words; these constants keep
# the Rust triples in one place.
TRIPLE_MACOS_ARM64       := aarch64-apple-darwin
TRIPLE_MACOS_AMD64       := x86_64-apple-darwin
TRIPLE_LINUX_AMD64_MUSL  := x86_64-unknown-linux-musl
TRIPLE_LINUX_ARM64_MUSL  := aarch64-unknown-linux-musl
TRIPLE_LINUX_AMD64_GNU   := x86_64-unknown-linux-gnu
TRIPLE_LINUX_ARM64_GNU   := aarch64-unknown-linux-gnu
TRIPLE_WINDOWS_AMD64     := x86_64-pc-windows-msvc

R2_RELEASE_FILES := \
	lantunnel-client-$(UI_VERSION)-windows-amd64.exe \
	lantunnel-client-$(UI_VERSION)-macos-amd64.dmg \
	lantunnel-client-$(UI_VERSION)-macos-arm64.dmg \
	lantunnel-client-$(UI_VERSION)-linux-amd64.AppImage \
	lantunnel-client-$(UI_VERSION)-linux-arm64.AppImage \
	lantunnel-gateway-$(VERSION)-aarch64-apple-darwin \
	lantunnel-gateway-$(VERSION)-x86_64-unknown-linux-musl \
	lantunnel-admin-$(VERSION)-aarch64-apple-darwin \
	lantunnel-admin-$(VERSION)-x86_64-unknown-linux-musl

CHECKSUM_FILES ?= $(R2_RELEASE_FILES)

REAL_TEST_GATEWAY_ARTIFACT ?= $(RELEASE_DIR)/lantunnel-gateway-$(VERSION)-$(TRIPLE_LINUX_AMD64_MUSL)
REAL_TEST_CLIENT_ARTIFACT ?= target/$(TRIPLE_LINUX_AMD64_GNU)/release/lantunnel-client

ANDROID_DIR       := apps/android-proxy
ANDROID_ABIS      ?= arm64-v8a
ANDROID_APK_NAME  := lantunnel-client-$(VERSION)-android-arm64.apk
ANDROID_AAB_NAME  := lantunnel-client-$(VERSION)-android-arm64.aab
ANDROID_SDK_ROOT  ?= $(shell if [ -d "$$HOME/Library/Android/sdk" ]; then echo "$$HOME/Library/Android/sdk"; elif [ -d "/opt/homebrew/share/android-commandlinetools" ]; then echo "/opt/homebrew/share/android-commandlinetools"; fi)
ANDROID_NDK_VERSION ?= 27.2.12479018
ANDROID_NDK_HOME  ?= $(ANDROID_SDK_ROOT)/ndk/$(ANDROID_NDK_VERSION)
IOS_DIR           := apps/ios-proxy
IOS_PROJECT       := $(IOS_DIR)/TunnelProxyIOS.xcodeproj
IOS_SCHEME        ?= TunnelProxy
IOS_CONFIGURATION ?= Release
IOS_DERIVED_DATA_DIR ?= $(DIST_DIR)/ios-derived-data
IOS_APP_BUNDLE_NAME ?= TunnelProxy.app
IOS_APP_ZIP_NAME  := lantunnel-client-$(VERSION)-ios-arm64.app.zip
IOS_IPA_NAME      := lantunnel-client-$(VERSION)-ios-arm64.ipa
IOS_CODE_SIGNING_ALLOWED ?= NO
IOS_XCODEBUILD_EXTRA ?=
HEV_TUN_DIR       := apps/android-proxy/app/src/main/jni/hev-socks5-tunnel
TUN_SIDECAR_GEN_DIR := apps/lantunnel-client/src-tauri/gen/tun-sidecar
TUN_SIDECAR_TARGET_DIR = $(TUN_SIDECAR_GEN_DIR)/$(TRIPLE)
TUN_SIDECAR_BUILD_LOCK := $(TUN_SIDECAR_GEN_DIR)/.hev-build.lock
TUN_SIDECAR_PREBUILT_DIR ?= dist/tun-sidecars
WINTUN_DLL        := $(HEV_TUN_DIR)/third-part/wintun/bin/wintun.dll
WINDOWS_CERTIFICATE_THUMBPRINT ?=
WINDOWS_TIMESTAMP_URL ?= http://timestamp.digicert.com
# There is no Windows code-signing certificate yet, so the published Windows
# build is the unsigned preview and the download page carries the Unknown
# publisher warning for it. Pinned here rather than left to an environment
# variable nobody remembers: forgetting it failed the Windows target and took
# every target queued after it down with it, while the wrapper still reported
# success. Set WINDOWS_CERTIFICATE_THUMBPRINT to sign instead, or pass
# ALLOW_UNSIGNED_WINDOWS_INSTALLER=0 to refuse an unsigned build outright.
ALLOW_UNSIGNED_WINDOWS_INSTALLER ?= 1

# ------------------------------ help ---------------------------------------

.PHONY: help
help:  ## Show this help message
	@awk 'BEGIN { FS = ":.*## " } \
	     /^## ==/ { sub("## == ", ""); printf "\n\033[1m%s\033[0m\n", $$0; next } \
	     /^[a-zA-Z0-9_.-]+:.*## / { printf "  \033[36m%-32s\033[0m %s\n", $$1, $$2 }' \
	     $(MAKEFILE_LIST)
	@echo
	@echo "  version: $(VERSION)  ui_version: $(UI_VERSION)  commit: $(COMMIT)  host: $(HOST_OS)/$(HOST_ARCH)"
	@echo

# Answers "can this machine produce a signed release?" before anything is
# built. A macOS DMG takes twenty minutes to reach the packaging step, which is
# where a missing certificate or notarization key used to surface.
.PHONY: check-release-signing
check-release-signing:  ## Verify macOS signing and notarization inputs are present
	@set -e; \
	if ! security find-identity -v -p codesigning | grep -q "$(MACOS_CODESIGN_IDENTITY)"; then \
	    echo "missing signing certificate $(MACOS_CODESIGN_IDENTITY) in the login keychain" >&2; \
	    echo "import the Developer ID .p12, or override MACOS_CODESIGN_IDENTITY" >&2; \
	    exit 1; \
	fi; \
	key="$${ASC_KEY_PATH:-$$HOME/.appstoreconnect/private_keys/AuthKey_$(ASC_KEY_ID).p8}"; \
	if [ ! -f "$$key" ]; then \
	    echo "missing App Store Connect key for notarization: $$key" >&2; \
	    exit 1; \
	fi; \
	echo "  ✓ signing certificate present"; \
	echo "  ✓ notarization key present"; \
	echo "  ✓ this machine can build a publishable macOS release"

.PHONY: check-release-surface
check-release-surface:  ## Verify release help exposes only product/platform release targets
	@tmp="$${TMPDIR:-/tmp}/tp-release-help.$$RANDOM"; \
	$(MAKE) --no-print-directory help > "$$tmp"; \
	grep -q 'release-lantunnel-gateway-linux-amd64' "$$tmp"; \
	grep -q 'release-lantunnel-client-linux-amd64' "$$tmp"; \
	grep -q 'release-lantunnel-client-macos-arm64' "$$tmp"; \
	grep -q 'release-lantunnel-admin-linux-amd64' "$$tmp"; \
	grep -q 'release-lantunnel-admin-macos-arm64' "$$tmp"; \
	grep -q 'release-lantunnel-client-windows-amd64' "$$tmp"; \
	grep -q 'release-real-test' "$$tmp"; \
	for forbidden in build-anyproxy build-tunnel-proxy release-anyproxy-client release-fast TARGET= 'lantunnel-client-u[i]'; do \
	  if grep -q "$$forbidden" "$$tmp"; then \
	    echo "public release help contains forbidden surface: $$forbidden" >&2; \
	    rm -f "$$tmp"; \
	    exit 1; \
	  fi; \
	done; \
	rm -f "$$tmp"
	@! rg -n 'apps/(client-app|client/|gateway)|lantunnel-client-cli|TARGET=<|TARGET=' README.md scripts/fast_build.sh Dockerfile.builder apps/lantunnel-client/src-tauri/src/main.rs
	@! awk '/^release([-a-zA-Z0-9]*):/ && /clean-release/ { found=1 } END { exit found ? 0 : 1 }' Makefile
	@! awk '/^_package-ui:/{in_ui=1} /^\.PHONY: _package-ui-download/{in_ui=0} in_ui && /tar -C/ {found=1} END { exit found ? 0 : 1 }' Makefile
	@! awk '/^_package-cli:/{in_cli=1} /^# ---- internal: UI/{in_cli=0} in_cli && /(tar -C|cp -R \$\(CONFIG_DIR\)|cp README.md CHANGELOG.md)/ {found=1} END { exit found ? 0 : 1 }' Makefile
	@rg -n '\$\(XWIN_CACHE\):/root/\.cache/cargo-xwin' Makefile >/dev/null
	@! rg -n '\$\(XWIN_CACHE\):/root/\.cache/xwin' Makefile
	@awk '/^release-fast:/{in_fast=1; next} /^$$/{in_fast=0} in_fast && /release-lantunnel-gateway-linux-amd64/ {gw=1} in_fast && /release-lantunnel-admin-linux-amd64/ {admin=1} in_fast && /release-lantunnel-client-windows-amd64/ {win=1} in_fast && /release-android-proxy-apk|release-ios-proxy-app/ {mobile=1} in_fast && /&/ {bg=1} in_fast && /wait/ {wait=1} END{exit (gw && admin && win && !mobile && bg && wait) ? 0 : 1}' Makefile
	@awk '/^release-mobile:/{in_mobile=1; next} /^$$/{in_mobile=0} in_mobile && /_release-android-proxy-apk/ {android=1} in_mobile && /_release-ios-proxy-app/ {ios=1} in_mobile && /checksums/ {checksums=1} END{exit (android && ios && checksums) ? 0 : 1}' Makefile
	@awk '/^release-real-test:/{in_rt=1; next} /^$$/{in_rt=0} in_rt && /_release-lantunnel-gateway-linux-amd64/ {gw=1} in_rt && /_release-lantunnel-client-raw-linux-amd64/ {client=1} in_rt && /&/ {bg=1} in_rt && /wait/ {wait=1} END{exit (gw && client && bg && wait) ? 0 : 1}' Makefile
	@awk '/^release-all:/{in_all=1} /^$$/{in_all=0} in_all && /pre-aggregated/ {aggregated=1} in_all && /_release-/ {build=1} in_all && /checksums/ {checksums=1} in_all && /upload\.sh.*check/ {check=1} END{exit (aggregated && !build && checksums && check) ? 0 : 1}' Makefile

# ------------------------------ release ------------------------------------
## == release ==

.PHONY: clean-release
clean-release:  ## Remove dist/ release artifacts without touching target/
	@rm -rf $(DIST_DIR)
	@echo "  ✓ cleaned $(DIST_DIR)/"

.PHONY: checksums
checksums:  ## Generate one checksum manifest for the requested release artifacts
	@mkdir -p "$(DOWNLOAD_DIR)"
	@set -e; \
	  files=""; \
	  for file in $(CHECKSUM_FILES); do \
	    if [ -f "$(DOWNLOAD_DIR)/$$file" ]; then files="$$files $$file"; fi; \
	  done; \
	  if [ -z "$$files" ]; then \
	    echo "Error: no public V2 release artifacts found in $(DOWNLOAD_DIR)" >&2; \
	    exit 1; \
	  fi; \
	  (cd "$(DOWNLOAD_DIR)" && shasum -a 256 $$files > checksums.txt); \
	  echo "  ✓ $(DOWNLOAD_DIR)/checksums.txt"

.PHONY: release
release: release-lantunnel-client-macos-arm64  ## Build default macOS arm64 lantunnel-client release
	@echo "lantunnel-client release $(UI_VERSION) ready under $(RELEASE_DIR)/"
	@ls -la $(RELEASE_DIR)/

.PHONY: _release-lantunnel-gateway-macos-arm64
_release-lantunnel-gateway-macos-arm64:
	@$(MAKE) --no-print-directory _build-cli \
	    PKG=lantunnel-gateway BIN=lantunnel-gateway \
	    TRIPLES="$(TRIPLE_MACOS_ARM64)"

.PHONY: release-lantunnel-gateway-macos-arm64
release-lantunnel-gateway-macos-arm64:  ## Release lantunnel-gateway for macOS arm64
	@$(MAKE) --no-print-directory _release-lantunnel-gateway-macos-arm64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-gateway-linux-amd64
_release-lantunnel-gateway-linux-amd64:
	@$(MAKE) --no-print-directory _build-cli \
	    PKG=lantunnel-gateway BIN=lantunnel-gateway \
	    TRIPLES="$(TRIPLE_LINUX_AMD64_MUSL)"

.PHONY: release-lantunnel-gateway-linux-amd64
release-lantunnel-gateway-linux-amd64:  ## Release lantunnel-gateway for Linux amd64
	@$(MAKE) --no-print-directory _release-lantunnel-gateway-linux-amd64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-admin-macos-arm64
_release-lantunnel-admin-macos-arm64:
	@$(MAKE) --no-print-directory _build-cli \
	    PKG=lantunnel-admin BIN=lantunnel-admin \
	    TRIPLES="$(TRIPLE_MACOS_ARM64)"

.PHONY: release-lantunnel-admin-macos-arm64
release-lantunnel-admin-macos-arm64:  ## Release lantunnel-admin for macOS arm64
	@$(MAKE) --no-print-directory _release-lantunnel-admin-macos-arm64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-admin-linux-amd64
_release-lantunnel-admin-linux-amd64:
	@$(MAKE) --no-print-directory _build-cli \
	    PKG=lantunnel-admin BIN=lantunnel-admin \
	    TRIPLES="$(TRIPLE_LINUX_AMD64_MUSL)"

.PHONY: release-lantunnel-admin-linux-amd64
release-lantunnel-admin-linux-amd64:  ## Release lantunnel-admin for Linux amd64
	@$(MAKE) --no-print-directory _release-lantunnel-admin-linux-amd64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-client-macos-arm64
_release-lantunnel-client-macos-arm64:
	@$(MAKE) --no-print-directory _build-ui \
	    TRIPLES="$(TRIPLE_MACOS_ARM64)"

.PHONY: release-lantunnel-client-macos-arm64
release-lantunnel-client-macos-arm64:  ## Release lantunnel-client for macOS arm64
	@$(MAKE) --no-print-directory _release-lantunnel-client-macos-arm64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-client-macos-amd64
_release-lantunnel-client-macos-amd64:
	@$(MAKE) --no-print-directory _build-ui \
	    TRIPLES="$(TRIPLE_MACOS_AMD64)"

.PHONY: release-lantunnel-client-macos-amd64
release-lantunnel-client-macos-amd64:  ## Release lantunnel-client for macOS amd64
	@$(MAKE) --no-print-directory _release-lantunnel-client-macos-amd64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-client-windows-amd64
_release-lantunnel-client-windows-amd64:
	@$(MAKE) --no-print-directory _build-ui \
	    TRIPLES="$(TRIPLE_WINDOWS_AMD64)"

.PHONY: release-lantunnel-client-windows-amd64
release-lantunnel-client-windows-amd64:  ## Release lantunnel-client for Windows amd64 (.exe)
	@$(MAKE) --no-print-directory _release-lantunnel-client-windows-amd64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-client-linux-amd64
_release-lantunnel-client-linux-amd64:
	@$(MAKE) --no-print-directory _build-ui \
	    TRIPLES="$(TRIPLE_LINUX_AMD64_GNU)"

.PHONY: release-lantunnel-client-linux-amd64
release-lantunnel-client-linux-amd64:  ## Release lantunnel-client for Linux amd64
	@$(MAKE) --no-print-directory _release-lantunnel-client-linux-amd64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-client-linux-arm64
_release-lantunnel-client-linux-arm64:
	@$(MAKE) --no-print-directory _build-ui \
	    TRIPLES="$(TRIPLE_LINUX_ARM64_GNU)"

.PHONY: release-lantunnel-client-linux-arm64
release-lantunnel-client-linux-arm64:  ## Release lantunnel-client for Linux arm64
	@$(MAKE) --no-print-directory _release-lantunnel-client-linux-arm64
	@$(MAKE) --no-print-directory checksums

.PHONY: _release-lantunnel-client-raw-linux-amd64
_release-lantunnel-client-raw-linux-amd64:
	@if [ "$(SKIP_UI_FRONTEND)" != "1" ]; then \
	    $(MAKE) --no-print-directory _build-ui-frontend; \
	fi
	@set -e; \
	 override_file="apps/lantunnel-client/src-tauri/.tauri-build-override-raw-$$$$.json"; \
	 override_rel="$$(basename "$$override_file")"; \
	 printf '%s' '$(TAURI_CONFIG)' > "$$override_file"; \
	 trap 'rm -f "$$override_file"' EXIT; \
	 $(MAKE) --no-print-directory _ensure-builder-amd64; \
	 docker run --platform=linux/amd64 --rm \
	    -v $(PWD):/src \
	    $(DOCKER_CACHE_COMMON) \
	    -v $(RUSTUP_CACHE_AMD64):/usr/local/rustup \
	    -w /src \
	    $(BUILDER_IMAGE_AMD64) bash -c "set -e; \
	        rustup target add $(TRIPLE_LINUX_AMD64_GNU); \
	        cd apps/lantunnel-client/src-tauri && \
	        CARGO_PROFILE_RELEASE_OPT_LEVEL=3 cargo tauri build --no-bundle \
	            --config $$override_rel \
	            --target $(TRIPLE_LINUX_AMD64_GNU)"

.PHONY: release-fast
# Internal convenience target for a native Windows shell with the configured
# Linux builders. Cross-OS public releases are assembled by CI.
release-fast:
	@$(MAKE) --no-print-directory _ensure-builder
	@set -e; \
	  echo "→ fast release: Linux gateway/admin, Windows lantunnel-client"; \
	  $(MAKE) --no-print-directory _release-lantunnel-gateway-linux-amd64 & gw_pid=$$!; \
	  $(MAKE) --no-print-directory _release-lantunnel-admin-linux-amd64 & admin_pid=$$!; \
	  $(MAKE) --no-print-directory _release-lantunnel-client-windows-amd64 & win_pid=$$!; \
	  status=0; \
	  wait $$gw_pid || status=$$?; \
	  wait $$admin_pid || status=$$?; \
	  wait $$win_pid || status=$$?; \
	  if [ "$$status" -ne 0 ]; then exit "$$status"; fi
	@$(MAKE) --no-print-directory checksums
	@echo "Fast release ready: lantunnel-gateway/admin linux-amd64, lantunnel-client windows-amd64"
	@ls -la $(RELEASE_DIR)/

.PHONY: release-real-test
release-real-test:  ## Release a Linux gateway + a headless Linux client for a two-host acceptance run
	@$(MAKE) --no-print-directory _ensure-builder
	@$(MAKE) --no-print-directory _ensure-builder-amd64
	@$(DOCKER_RUN_BUILDER) cargo fetch --locked
	@set -e; \
	  echo "→ real-test release: Linux gateway and unified Linux lantunnel-client"; \
	  $(MAKE) --no-print-directory _release-lantunnel-gateway-linux-amd64 & gw_pid=$$!; \
	  SKIP_UI_FRONTEND=1 $(MAKE) --no-print-directory _release-lantunnel-client-raw-linux-amd64 & client_pid=$$!; \
	  status=0; \
	  wait $$gw_pid || status=$$?; \
	  wait $$client_pid || status=$$?; \
	  if [ "$$status" -ne 0 ]; then exit "$$status"; fi
	@$(MAKE) --no-print-directory checksums
	@test -f "$(REAL_TEST_GATEWAY_ARTIFACT)" || { echo "missing gateway artifact: $(REAL_TEST_GATEWAY_ARTIFACT)" >&2; exit 1; }
	@test -f "$(REAL_TEST_CLIENT_ARTIFACT)" || { echo "missing client artifact: $(REAL_TEST_CLIENT_ARTIFACT)" >&2; exit 1; }
	@echo "Real-test release ready:"
	@ls -lh "$(REAL_TEST_GATEWAY_ARTIFACT)" "$(REAL_TEST_CLIENT_ARTIFACT)"

# The Android Client is the shared UI in a WebView, so the bundle is part of
# the APK. Staging it here is what stops a release shipping yesterday's screens.
.PHONY: _stage-android-ui
_stage-android-ui:
	@$(MAKE) --no-print-directory _build-ui-frontend
	@rm -rf "$(ANDROID_DIR)/app/src/main/assets/ui"
	@mkdir -p "$(ANDROID_DIR)/app/src/main/assets/ui"
	@cp -R apps/lantunnel-client/frontend/dist/. "$(ANDROID_DIR)/app/src/main/assets/ui/"
	@echo "  ✓ staged the shared UI into the Android assets"

.PHONY: _release-android-proxy-apk
_release-android-proxy-apk:
	@mkdir -p $(RELEASE_DIR)
	@$(MAKE) --no-print-directory _stage-android-ui
	@ANDROID_SDK_ROOT="$(ANDROID_SDK_ROOT)" ANDROID_HOME="$(ANDROID_SDK_ROOT)" ANDROID_NDK_HOME="$(ANDROID_NDK_HOME)" \
	    ABIS="$(ANDROID_ABIS)" PROFILE=release "$(ANDROID_DIR)/build-rust-jni-libs.sh"
	@set -e; \
	  if [ -x "$(ANDROID_DIR)/gradlew" ]; then gradle_cmd="./gradlew"; \
	  elif command -v gradle >/dev/null 2>&1; then gradle_cmd="gradle"; \
	  else echo "Error: Gradle is required. Install Gradle or add a Gradle wrapper under $(ANDROID_DIR)."; exit 1; fi; \
	  cd "$(ANDROID_DIR)" && ANDROID_SDK_ROOT="$(ANDROID_SDK_ROOT)" ANDROID_HOME="$(ANDROID_SDK_ROOT)" env -u JAVA_TOOL_OPTIONS $$gradle_cmd :app:assembleRelease
	@set -e; \
	  src=""; \
	  for candidate in \
	    "$(ANDROID_DIR)/app/build/outputs/apk/release/app-release.apk" \
	    "$(ANDROID_DIR)/app/build/outputs/apk/release/app-release-unsigned.apk"; do \
	    if [ -f "$$candidate" ]; then src="$$candidate"; break; fi; \
	  done; \
	  if [ -z "$$src" ]; then echo "Error: release APK not found"; exit 1; fi; \
	  cp "$$src" "$(RELEASE_DIR)/$(ANDROID_APK_NAME)"; \
	  echo "  ✓ $(RELEASE_DIR)/$(ANDROID_APK_NAME)"

.PHONY: release-android-proxy-apk
# Non-public pre-2.0 mobile experiment. Not part of the V2 release surface.
release-android-proxy-apk:
	@$(MAKE) --no-print-directory _release-android-proxy-apk
	@$(MAKE) --no-print-directory checksums CHECKSUM_FILES="$(ANDROID_APK_NAME)"

.PHONY: release-android-proxy-aab
# Non-public pre-2.0 mobile experiment. Not part of the V2 release surface.
release-android-proxy-aab:
	@mkdir -p $(RELEASE_DIR)
	@$(MAKE) --no-print-directory _stage-android-ui
	@ANDROID_SDK_ROOT="$(ANDROID_SDK_ROOT)" ANDROID_HOME="$(ANDROID_SDK_ROOT)" ANDROID_NDK_HOME="$(ANDROID_NDK_HOME)" \
	    ABIS="$(ANDROID_ABIS)" PROFILE=release "$(ANDROID_DIR)/build-rust-jni-libs.sh"
	@set -e; \
	  if [ -x "$(ANDROID_DIR)/gradlew" ]; then gradle_cmd="./gradlew"; \
	  elif command -v gradle >/dev/null 2>&1; then gradle_cmd="gradle"; \
	  else echo "Error: Gradle is required. Install Gradle or add a Gradle wrapper under $(ANDROID_DIR)."; exit 1; fi; \
	  cd "$(ANDROID_DIR)" && ANDROID_SDK_ROOT="$(ANDROID_SDK_ROOT)" ANDROID_HOME="$(ANDROID_SDK_ROOT)" env -u JAVA_TOOL_OPTIONS $$gradle_cmd :app:bundleRelease
	@src="$(ANDROID_DIR)/app/build/outputs/bundle/release/app-release.aab"; \
	  if [ ! -f "$$src" ]; then echo "Error: release AAB not found: $$src"; exit 1; fi; \
	  cp "$$src" "$(RELEASE_DIR)/$(ANDROID_AAB_NAME)"; \
	  echo "  ✓ $(RELEASE_DIR)/$(ANDROID_AAB_NAME)"
	@$(MAKE) --no-print-directory checksums CHECKSUM_FILES="$(ANDROID_AAB_NAME)"

# The iOS Client is the shared UI in a WKWebView, so the bundle is part of the
# app. Staging it here is what stops a release shipping yesterday's screens.
.PHONY: _stage-ios-ui
_stage-ios-ui:
	@$(MAKE) --no-print-directory _build-ui-frontend
	@rm -rf "$(IOS_DIR)/TunnelProxy/Resources/ui"
	@mkdir -p "$(IOS_DIR)/TunnelProxy/Resources/ui"
	@cp -R apps/lantunnel-client/frontend/dist/. "$(IOS_DIR)/TunnelProxy/Resources/ui/"
	@echo "  ✓ staged the shared UI into the iOS resources"

.PHONY: _release-ios-proxy-app
_release-ios-proxy-app:
	@mkdir -p $(RELEASE_DIR)
	@$(MAKE) --no-print-directory _stage-ios-ui
	@PROFILE=release scripts/build-ios-mobile-libs.sh
	@rm -rf "$(IOS_DERIVED_DATA_DIR)"
	@xcodebuild build \
	    -project "$(IOS_PROJECT)" \
	    -scheme "$(IOS_SCHEME)" \
	    -configuration "$(IOS_CONFIGURATION)" \
	    -destination "generic/platform=iOS" \
	    -derivedDataPath "$(IOS_DERIVED_DATA_DIR)" \
	    CODE_SIGNING_ALLOWED="$(IOS_CODE_SIGNING_ALLOWED)" \
	    CODE_SIGNING_REQUIRED=NO \
	    CODE_SIGN_IDENTITY="" \
	    $(IOS_XCODEBUILD_EXTRA)
	@set -e; \
	  app="$(IOS_DERIVED_DATA_DIR)/Build/Products/$(IOS_CONFIGURATION)-iphoneos/$(IOS_APP_BUNDLE_NAME)"; \
	  if [ ! -d "$$app" ]; then echo "Error: iOS app bundle not found: $$app"; exit 1; fi; \
	  ditto -c -k --keepParent "$$app" "$(RELEASE_DIR)/$(IOS_APP_ZIP_NAME)"; \
	  tmp="$(RELEASE_DIR)/.ios-payload-$$$$"; \
	  rm -rf "$$tmp"; \
	  mkdir -p "$$tmp/Payload"; \
	  trap 'rm -rf "$$tmp"' EXIT; \
	  ditto "$$app" "$$tmp/Payload/$(IOS_APP_BUNDLE_NAME)"; \
	  (cd "$$tmp" && ditto -c -k --sequesterRsrc --keepParent Payload "$(abspath $(RELEASE_DIR))/$(IOS_IPA_NAME)"); \
	  echo "  ✓ $(RELEASE_DIR)/$(IOS_APP_ZIP_NAME)"; \
	  echo "  ✓ $(RELEASE_DIR)/$(IOS_IPA_NAME)"

.PHONY: release-ios-proxy-app
# Non-public pre-2.0 mobile experiment. Not part of the V2 release surface.
release-ios-proxy-app:
	@$(MAKE) --no-print-directory _release-ios-proxy-app
	@$(MAKE) --no-print-directory checksums CHECKSUM_FILES="$(IOS_APP_ZIP_NAME) $(IOS_IPA_NAME)"

.PHONY: release-mobile
# Non-public pre-2.0 mobile experiment. Not part of the V2 release surface.
release-mobile:
	@$(MAKE) --no-print-directory _release-android-proxy-apk
	@$(MAKE) --no-print-directory _release-ios-proxy-app
	@$(MAKE) --no-print-directory checksums CHECKSUM_FILES="$(ANDROID_APK_NAME) $(IOS_APP_ZIP_NAME) $(IOS_IPA_NAME)"
	@echo "Mobile release $(VERSION) ready under $(RELEASE_DIR)/"
	@ls -la $(RELEASE_DIR)/

DESKTOP_MACOS_RELEASE_TARGETS := \
	_release-lantunnel-client-macos-arm64 \
	_release-lantunnel-client-macos-amd64

DESKTOP_LINUX_RELEASE_TARGETS := \
	_release-lantunnel-client-linux-amd64 \
	_release-lantunnel-client-linux-arm64

DESKTOP_WINDOWS_RELEASE_TARGETS := \
	_release-lantunnel-client-windows-amd64

DESKTOP_DOWNLOAD_FILES := \
	lantunnel-client-$(UI_VERSION)-windows-amd64.exe \
	lantunnel-client-$(UI_VERSION)-macos-amd64.dmg \
	lantunnel-client-$(UI_VERSION)-macos-arm64.dmg \
	lantunnel-client-$(UI_VERSION)-linux-amd64.AppImage \
	lantunnel-client-$(UI_VERSION)-linux-arm64.AppImage

.PHONY: release-desktop
release-desktop:  ## Release all lantunnel-client desktop artifacts shown on the download page
	@$(MAKE) --no-print-directory _ensure-builder
	@$(MAKE) --no-print-directory _ensure-builder-amd64
	@$(MAKE) --no-print-directory _build-ui-frontend
	@set -e; \
	  echo "→ release-desktop: lantunnel-client download-page artifacts"; \
	  status=0; \
	  (set -e; \
	    pids=""; group_status=0; \
	    for target in $(DESKTOP_MACOS_RELEASE_TARGETS); do \
	      $(MAKE) --no-print-directory $$target SKIP_UI_FRONTEND=1 & \
	      pids="$$pids $$!"; \
	    done; \
	    for pid in $$pids; do \
	      wait $$pid || group_status=$$?; \
	    done; \
	    exit $$group_status) & macos_pid=$$!; \
	  (set -e; \
	    for target in $(DESKTOP_LINUX_RELEASE_TARGETS); do \
	      $(MAKE) --no-print-directory $$target SKIP_UI_FRONTEND=1; \
	    done) & linux_pid=$$!; \
	  (set -e; \
	    for target in $(DESKTOP_WINDOWS_RELEASE_TARGETS); do \
	      $(MAKE) --no-print-directory $$target SKIP_UI_FRONTEND=1; \
	    done) & windows_pid=$$!; \
	  wait $$macos_pid || status=$$?; \
	  wait $$linux_pid || status=$$?; \
	  wait $$windows_pid || status=$$?; \
	  if [ "$$status" -ne 0 ]; then exit "$$status"; fi
	@$(MAKE) --no-print-directory checksums
	@set -e; \
	  missing=0; \
	  echo "→ verifying download-page artifacts for $(UI_VERSION)"; \
	  for file in $(DESKTOP_DOWNLOAD_FILES); do \
	    if [ -f "$(DOWNLOAD_DIR)/$$file" ]; then \
	      echo "  ✓ $(DOWNLOAD_DIR)/$$file"; \
	    else \
	      echo "  ! missing $(DOWNLOAD_DIR)/$$file"; \
	      missing=1; \
	    fi; \
	  done; \
	  exit $$missing
	@echo "Desktop release $(UI_VERSION) ready under $(DOWNLOAD_DIR)/"
	@ls -la $(DOWNLOAD_DIR)/

.PHONY: release-desktop-remote
release-desktop-remote: release-all  ## Validate and upload a pre-aggregated public V2 release to remote R2
	@if [ "$(UPLOAD_DIRECT)" = "1" ]; then \
	    echo "→ uploading the public V2 release to R2 without local proxy env"; \
	    HTTP_PROXY= HTTPS_PROXY= ALL_PROXY= NO_PROXY=* $(MAKE) --no-print-directory upload-remote; \
	else \
	    $(MAKE) --no-print-directory upload-remote; \
	fi
	@echo "Public V2 remote release $(UI_VERSION) complete"

.PHONY: release-all
release-all:  ## Validate a complete pre-aggregated cross-OS public V2 release; does not build
	@echo "→ validating pre-aggregated public V2 release $(UI_VERSION) in $(DOWNLOAD_DIR)/"
	@$(MAKE) --no-print-directory checksums
	@DIST_DIR="$(DIST_DIR)" RELEASE_DIR="$(RELEASE_DIR)" DOWNLOAD_DIR="$(DOWNLOAD_DIR)" \
	    ./scripts/upload.sh "$(UI_VERSION)" check
	@echo "Pre-aggregated release $(VERSION) verified under $(RELEASE_DIR)/"
	@ls -la $(RELEASE_DIR)/

.PHONY: upload
upload: upload-remote  ## Upload the complete public V2 release to remote R2

.PHONY: upload-local
upload-local:  ## Upload the complete public V2 release to local wrangler R2
	@echo "Uploading the public V2 release $(UI_VERSION) to local R2..."
	@DIST_DIR="$(DIST_DIR)" RELEASE_DIR="$(RELEASE_DIR)" DOWNLOAD_DIR="$(DOWNLOAD_DIR)" PLATFORM_DIR="$(PLATFORM_DIR)" \
	    ./scripts/upload.sh "$(UI_VERSION)" local

.PHONY: upload-remote
upload-remote:  ## Upload the complete public V2 release to remote Cloudflare R2
	@if [ ! -f "$(UPLOAD_ENV)" ]; then \
	    echo "Error: $(UPLOAD_ENV) not found. Create it with R2_ACCOUNT_ID, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY and R2_BUCKET_NAME."; \
	    exit 1; \
	fi
	@echo "Loading $(UPLOAD_ENV) and uploading the public V2 release $(UI_VERSION) to remote R2..."
	@bash -c 'set -a; source "$(UPLOAD_ENV)"; set +a; \
	    DIST_DIR="$(DIST_DIR)" RELEASE_DIR="$(RELEASE_DIR)" DOWNLOAD_DIR="$(DOWNLOAD_DIR)" PLATFORM_DIR="$(PLATFORM_DIR)" \
	    ./scripts/upload.sh "$(UI_VERSION)" remote'

.PHONY: upload-all
upload-all:  ## Upload the complete public V2 release to both local and remote R2
	@if [ ! -f "$(UPLOAD_ENV)" ]; then \
	    echo "Error: $(UPLOAD_ENV) not found. Create it with R2_ACCOUNT_ID, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY and R2_BUCKET_NAME."; \
	    exit 1; \
	fi
	@echo "Loading $(UPLOAD_ENV) and uploading the public V2 release $(UI_VERSION) to local + remote R2..."
	@bash -c 'set -a; source "$(UPLOAD_ENV)"; set +a; \
	    DIST_DIR="$(DIST_DIR)" RELEASE_DIR="$(RELEASE_DIR)" DOWNLOAD_DIR="$(DOWNLOAD_DIR)" PLATFORM_DIR="$(PLATFORM_DIR)" \
	    ./scripts/upload.sh "$(UI_VERSION)" all'

.PHONY: upload-changelog
upload-changelog:  ## Verify CHANGELOG.md matches the immutable remote R2 release
	@if [ ! -f "$(UPLOAD_ENV)" ]; then \
	    echo "Error: $(UPLOAD_ENV) not found. Create it with R2_ACCOUNT_ID, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY and R2_BUCKET_NAME."; \
	    exit 1; \
	fi
	@echo "Loading $(UPLOAD_ENV) and verifying CHANGELOG.md for $(UI_VERSION)..."
	@bash -c 'set -a; source "$(UPLOAD_ENV)"; set +a; ./scripts/upload.sh "$(UI_VERSION)" changelog'

.PHONY: full-release
full-release: release-all upload-remote  ## Validate and upload a pre-aggregated public V2 release to remote R2
	@echo "Lantunnel public release $(UI_VERSION) complete"

.PHONY: local-release
local-release: release-all upload-local  ## Validate and upload a pre-aggregated public V2 release to local R2
	@echo "Lantunnel public local release $(UI_VERSION) complete"

# ---- internal: CLI build dispatcher ----------------------------------------
# Args: PKG (cargo -p), BIN (binary name), TRIPLES (space-separated)

.PHONY: _build-cli
_build-cli:
	@set -e; for t in $(TRIPLES); do \
	    echo "→ $(BIN) @ $$t"; \
	    case "$$t" in \
	      *-apple-darwin) \
	        if [ "$(HOST_OS)" != "darwin" ]; then \
	            echo "  ! skip $$t — requires macOS host"; continue; \
	        fi; \
	        rustup target add $$t >/dev/null 2>&1 || true; \
	        $(CARGO) build --profile $(BUILD_PROFILE) --target $$t -p $(PKG) ;; \
	      *-unknown-linux-musl) \
	        $(MAKE) --no-print-directory _ensure-builder; \
	        $(DOCKER_RUN_BUILDER) bash -c "set -e; \
	            rustup target add $$t; \
	            cargo zigbuild --profile $(BUILD_PROFILE) --target $$t -p $(PKG)" ;; \
	      *-pc-windows-msvc) \
	        $(MAKE) --no-print-directory _ensure-builder; \
	        $(DOCKER_RUN_BUILDER) bash -c "set -e; \
	            rustup target add $$t; \
	            cargo xwin build --profile $(BUILD_PROFILE) --target $$t -p $(PKG)" ;; \
	      *) echo "  ! unsupported triple: $$t"; exit 2 ;; \
	    esac; \
	    $(MAKE) --no-print-directory _package-cli BIN=$(BIN) TRIPLE=$$t; \
	done

.PHONY: _package-cli
_package-cli:
	@mkdir -p $(RELEASE_DIR); \
	 ext=""; case "$(TRIPLE)" in *windows*) ext=".exe" ;; esac; \
	 name="$(BIN)-$(VERSION)-$(TRIPLE)$$ext"; out="$(RELEASE_DIR)/$$name"; \
	 cp "target/$(TRIPLE)/$(BUILD_PROFILE)/$(BIN)$$ext" "$$out"; \
	 chmod 0755 "$$out"; \
	 echo "  ✓ $$out"

# ---- internal: UI (Tauri) build dispatcher ---------------------------------
# Args: TRIPLES (space-separated)
# macOS → native `tauri build --bundles app`
# windows-msvc → native Windows Tauri NSIS installer
# linux-gnu → docker builder + `tauri build --bundles appimage`
#
# The tauri --config override is written to a JSON file instead of passed
# inline. Inline '{"…":""}' collides with the outer bash -c "…" quoting
# used for the docker cross-builds, which strips the embedded double quotes
# before tauri sees them.

TAURI_BIN           := lantunnel-client
TAURI_PRODUCT_NAME  := Lantunnel Client
# Frozen legacy identifier. It carries the project's former name, but it is the
# macOS .app bundle id, the Windows installer registry key and the Linux
# .desktop id of every install already out there. Changing it does not rename
# anything — it creates a second, unrelated application, and no existing user
# upgrades in place. tauri.conf.json holds the same value and cannot carry a
# comment, so this is the note.
TAURI_IDENTIFIER    := com.buhuipao.tunnel-proxy-app
TAURI_OVERRIDE_FILE := apps/lantunnel-client/src-tauri/.tauri-build-override.json
TAURI_OVERRIDE_REL  := .tauri-build-override.json
# The window title is the product; TAURI_PRODUCT_NAME still names the bundle,
# the DMG volume, the AppImage and the exe, so the two are kept apart.
TAURI_WINDOW_TITLE  := Lantunnel
TAURI_CONFIG        := {"productName":"$(TAURI_PRODUCT_NAME)","mainBinaryName":"$(TAURI_BIN)","identifier":"$(TAURI_IDENTIFIER)","build":{"beforeBuildCommand":""},"app":{"windows":[{"title":"$(TAURI_WINDOW_TITLE)","width":480,"height":720,"resizable":true,"fullscreen":false,"transparent":false,"visible":false}]},"bundle":{"category":"Utility"}}
TAURI_CONFIG_TEMPLATE := {"productName":"$(TAURI_PRODUCT_NAME)","mainBinaryName":"$(TAURI_BIN)","identifier":"$(TAURI_IDENTIFIER)","build":{"beforeBuildCommand":""},"app":{"windows":[{"title":"$(TAURI_WINDOW_TITLE)","width":480,"height":720,"resizable":true,"fullscreen":false,"transparent":false,"visible":false}]},"bundle":{"category":"Utility"__TAURI_RESOURCES____TAURI_WINDOWS____TAURI_LINUX__}}

# Build the frontend bundle once on the host. Output (apps/lantunnel-client/frontend/dist/)
# is pure JS/CSS/HTML, target-independent — Tauri reads it via the repo bind-mount
# for every triple, so per-triple `npm ci && vite build` is pure waste.
# Host must have node/npm (already required by the macOS path).
.PHONY: _build-ui-frontend
_build-ui-frontend:
	@echo "→ $(TAURI_BIN) frontend bundle (host, once)"
	@cd apps/lantunnel-client/frontend && npm ci --no-audit --no-fund && npm run build

.PHONY: _clear-tun-sidecar-resource
_clear-tun-sidecar-resource:
	@rm -rf "$(TUN_SIDECAR_TARGET_DIR)"
	@mkdir -p "$(TUN_SIDECAR_TARGET_DIR)"

.PHONY: _prepare-tun-sidecar
_prepare-tun-sidecar: _clear-tun-sidecar-resource
	@set -e; \
	  prebuilt_dir="$(TUN_SIDECAR_PREBUILT_DIR)/$(TRIPLE)"; \
	  case "$(TRIPLE)" in \
	    aarch64-apple-darwin|x86_64-apple-darwin) \
	      if [ -x "$$prebuilt_dir/hev-socks5-tunnel" ]; then \
	        cp "$$prebuilt_dir/hev-socks5-tunnel" "$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel"; \
	      else \
	        lock="$(TUN_SIDECAR_BUILD_LOCK)"; \
	        while ! mkdir "$$lock" 2>/dev/null; do sleep 1; done; \
	        trap 'rmdir "$$lock"' EXIT; \
	        case "$(TRIPLE)" in aarch64-*) arch=arm64 ;; x86_64-*) arch=x86_64 ;; esac; \
	        $(MAKE) --no-print-directory -C "$(HEV_TUN_DIR)" clean >/dev/null; \
	        $(MAKE) --no-print-directory -C "$(HEV_TUN_DIR)" exec \
	          PP="xcrun --sdk macosx --toolchain macosx clang" \
	          CC="xcrun --sdk macosx --toolchain macosx clang" \
	          CFLAGS="-arch $$arch -mmacosx-version-min=10.14" \
	          LFLAGS="-arch $$arch -mmacosx-version-min=10.14"; \
	        cp "$(HEV_TUN_DIR)/bin/hev-socks5-tunnel" "$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel"; \
	        $(MAKE) --no-print-directory -C "$(HEV_TUN_DIR)" clean >/dev/null; \
	        trap - EXIT; \
	        rmdir "$$lock"; \
	      fi; \
	      chmod 0755 "$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel" ;; \
	    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) \
	      if [ -x "$$prebuilt_dir/hev-socks5-tunnel" ]; then \
	        cp "$$prebuilt_dir/hev-socks5-tunnel" "$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel"; \
	        chmod 0755 "$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel"; \
	      else \
	        $(MAKE) --no-print-directory _prepare-tun-sidecar-linux TRIPLE="$(TRIPLE)"; \
	      fi ;; \
	    x86_64-pc-windows-msvc) \
	      sidecar=""; \
	      for candidate in "$$prebuilt_dir/hev-socks5-tunnel.exe" "$(HEV_TUN_DIR)/bin/hev-socks5-tunnel.exe" "target/$(TRIPLE)/release/hev-socks5-tunnel.exe"; do \
	        if [ -f "$$candidate" ]; then sidecar="$$candidate"; break; fi; \
	      done; \
	      if [ -z "$$sidecar" ]; then \
	        echo "Error: missing Windows hev-socks5-tunnel.exe for $(TRIPLE)." >&2; \
	        echo "Place it at $$prebuilt_dir/hev-socks5-tunnel.exe or set TUN_SIDECAR_PREBUILT_DIR." >&2; \
	        exit 1; \
	      fi; \
	      msys_dll=""; \
	      sidecar_dir="$$(dirname "$$sidecar")"; \
	      for candidate in "$$prebuilt_dir/msys-2.0.dll" "$$sidecar_dir/msys-2.0.dll" "$(HEV_TUN_DIR)/bin/msys-2.0.dll" "target/$(TRIPLE)/release/msys-2.0.dll"; do \
	        if [ -f "$$candidate" ]; then msys_dll="$$candidate"; break; fi; \
	      done; \
	      if [ -z "$$msys_dll" ]; then \
	        echo "Error: missing Windows msys-2.0.dll for $(TRIPLE)." >&2; \
	        echo "Place it next to hev-socks5-tunnel.exe or set TUN_SIDECAR_PREBUILT_DIR." >&2; \
	        exit 1; \
	      fi; \
	      cp "$$sidecar" "$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel.exe"; \
	      cp "$$msys_dll" "$(TUN_SIDECAR_TARGET_DIR)/msys-2.0.dll"; \
	      wintun_dll=""; \
	      for candidate in "$$prebuilt_dir/wintun.dll" "$(WINTUN_DLL)"; do \
	        if [ -f "$$candidate" ]; then wintun_dll="$$candidate"; break; fi; \
	      done; \
	      if [ -z "$$wintun_dll" ]; then \
	        echo "Error: missing Windows wintun.dll for $(TRIPLE)." >&2; \
	        echo "Place the pinned upstream DLL at $$prebuilt_dir/wintun.dll or set TUN_SIDECAR_PREBUILT_DIR." >&2; \
	        exit 1; \
	      fi; \
	      cp "$$wintun_dll" "$(TUN_SIDECAR_TARGET_DIR)/wintun.dll" ;; \
	    *) echo "Error: unsupported TUN sidecar target $(TRIPLE)" >&2; exit 2 ;; \
	  esac

.PHONY: _prepare-tun-sidecar-linux
_prepare-tun-sidecar-linux:
	@mkdir -p "$(TUN_SIDECAR_GEN_DIR)"
	@case "$(TRIPLE)" in \
	    x86_64-*) plat=linux/amd64; img=$(BUILDER_IMAGE_AMD64); rustup_cache=$(RUSTUP_CACHE_AMD64); \
	              $(MAKE) --no-print-directory _ensure-builder-amd64 ;; \
	    aarch64-*) plat=linux/arm64; img=$(BUILDER_IMAGE_ARM64); rustup_cache=$(RUSTUP_CACHE_ARM64); \
	              $(MAKE) --no-print-directory _ensure-builder-arm64 ;; \
	    *) echo "Error: unsupported linux sidecar target $(TRIPLE)" >&2; exit 2 ;; \
	  esac; \
	  lock="$(TUN_SIDECAR_BUILD_LOCK)"; \
	  while ! mkdir "$$lock" 2>/dev/null; do sleep 1; done; \
	  trap 'rmdir "$$lock"' EXIT; \
	  docker run --platform=$$plat --rm \
	    $(PROXY_ENV) \
	    -v $(PWD):/src \
	    $(DOCKER_CACHE_COMMON) \
	    -v $$rustup_cache:/usr/local/rustup \
	    -w /src \
	    $$img bash -c "set -e; \
	      cd $(HEV_TUN_DIR); \
	      make clean >/dev/null; \
	      make exec; \
	      install -m 0755 bin/hev-socks5-tunnel /src/$(TUN_SIDECAR_TARGET_DIR)/hev-socks5-tunnel; \
	      make clean >/dev/null"; \
	  trap - EXIT; \
	  rmdir "$$lock"

.PHONY: _build-ui
_build-ui:
	@if [ "$(SKIP_UI_FRONTEND)" != "1" ]; then \
	    $(MAKE) --no-print-directory _build-ui-frontend; \
	fi
	@set -e; \
	 override_files=""; \
	 trap 'for f in $$override_files; do rm -f "$$f"; done' EXIT; \
	 tauri_bin="../frontend/node_modules/.bin/tauri"; \
	 for t in $(TRIPLES); do \
	    $(MAKE) --no-print-directory _prepare-tun-sidecar TRIPLE=$$t; \
	    windows_config=""; \
	    linux_config=""; \
	    case "$$t" in \
	      *windows*) \
	        resources=',"resources":{"gen/tun-sidecar/'"$$t"'/hev-socks5-tunnel.exe":"hev-socks5-tunnel.exe","gen/tun-sidecar/'"$$t"'/wintun.dll":"wintun.dll","gen/tun-sidecar/'"$$t"'/msys-2.0.dll":"msys-2.0.dll"}'; \
	        thumbprint="$(WINDOWS_CERTIFICATE_THUMBPRINT)"; \
	        if [ -z "$$thumbprint" ]; then \
	          if [ "$(ALLOW_UNSIGNED_WINDOWS_INSTALLER)" != "1" ]; then \
	            echo "Error: Windows release requires WINDOWS_CERTIFICATE_THUMBPRINT or explicit ALLOW_UNSIGNED_WINDOWS_INSTALLER=1." >&2; \
	            echo "Use the thumbprint for a signed installer or the explicit switch for the current unsigned preview." >&2; \
	            exit 1; \
	          fi; \
	          echo "  ! building the canonical unsigned Windows preview; Windows may show Unknown publisher"; \
	        elif ! printf '%s' "$$thumbprint" | grep -Eq '^[0-9A-Fa-f]{40}$$'; then \
	          echo "Error: WINDOWS_CERTIFICATE_THUMBPRINT must be a 40-character SHA-1 certificate thumbprint." >&2; \
	          exit 1; \
	        else \
	          windows_config=',"windows":{"certificateThumbprint":"'"$$thumbprint"'","digestAlgorithm":"sha256","timestampUrl":"$(WINDOWS_TIMESTAMP_URL)"}'; \
	        fi ;; \
	      *-unknown-linux-gnu) \
	        resources=',"resources":{"gen/tun-sidecar/'"$$t"'/hev-socks5-tunnel":"hev-socks5-tunnel"}'; \
	        case "$$t" in \
	          x86_64-*) libdir=x86_64-linux-gnu ;; \
	          aarch64-*) libdir=aarch64-linux-gnu ;; \
	        esac; \
	        linux_config=',"linux":{"appimage":{"files":{"usr/lib/libharfbuzz.so.0":"/usr/lib/'"$$libdir"'/libharfbuzz.so.0"}}}' ;; \
	      *) resources=',"resources":{"gen/tun-sidecar/'"$$t"'/hev-socks5-tunnel":"hev-socks5-tunnel"}' ;; \
	    esac; \
	    override_file="apps/lantunnel-client/src-tauri/.tauri-build-override-$$t-$$$$.json"; \
	    override_files="$$override_files $$override_file"; \
	    override_rel="$$(basename "$$override_file")"; \
	    printf '%s' '$(TAURI_CONFIG_TEMPLATE)' | \
	      sed -e "s|__TAURI_RESOURCES__|$$resources|" -e "s|__TAURI_WINDOWS__|$$windows_config|" \
	          -e "s|__TAURI_LINUX__|$$linux_config|" > "$$override_file"; \
	    echo "→ $(TAURI_BIN) @ $$t"; \
	    case "$$t" in \
	      *-apple-darwin) \
	        if [ "$(HOST_OS)" != "darwin" ]; then \
	            echo "  ! skip $$t — requires macOS host"; continue; \
	        fi; \
	        rustup target add $$t >/dev/null 2>&1 || true; \
	        (cd apps/lantunnel-client/src-tauri && \
	            CARGO_PROFILE_RELEASE_OPT_LEVEL=3 "$$tauri_bin" build \
	                --bundles app \
	                --config "$$override_rel" \
	                --target $$t); \
	        (cd apps/lantunnel-client/src-tauri && \
	            cargo build --profile $(BUILD_PROFILE) --bin lantunnel-tun-helper --target $$t); \
	        app_dir="target/$$t/release/bundle/macos/$(TAURI_PRODUCT_NAME).app"; \
	        launch_daemons="$$app_dir/Contents/Library/LaunchDaemons"; \
	        launch_services="$$app_dir/Contents/Library/LaunchServices"; \
	        mkdir -p "$$launch_daemons" "$$launch_services"; \
	        cp "apps/lantunnel-client/src-tauri/macos/app.lantunnel.tun-helper.plist" \
	            "$$launch_daemons/app.lantunnel.tun-helper.plist"; \
	        cp "target/$$t/$(BUILD_PROFILE)/lantunnel-tun-helper" \
	            "$$launch_services/app.lantunnel.tun-helper"; \
	        chmod 755 "$$launch_services/app.lantunnel.tun-helper" ;; \
	      *-pc-windows-msvc) \
	        case "$(HOST_OS)" in \
	          mingw*|msys*|cygwin*) \
	            rustup target add $$t >/dev/null 2>&1 || true; \
	            (cd apps/lantunnel-client/src-tauri && \
	                CARGO_PROFILE_RELEASE_OPT_LEVEL=3 \
	                LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR="gen/tun-sidecar/$$t" \
	                "$$tauri_bin" build --no-bundle \
	                    --config "$$override_rel" \
	                    --target $$t) ;; \
	          *) \
	            $(MAKE) --no-print-directory _ensure-builder-nsis; \
	            docker run --platform=linux/amd64 --rm \
	                $(PROXY_ENV) \
	                -v $(CURDIR):/src \
	                -v $(CARGO_REGISTRY_CACHE):/usr/local/cargo/registry \
	                -v $(CARGO_GIT_CACHE):/usr/local/cargo/git \
	                -v $(XWIN_CACHE):/root/.cache/cargo-xwin \
	                -w /src $(BUILDER_IMAGE_NSIS) bash -c "set -e; \
	                    rustup target add $$t; \
	                    cd apps/lantunnel-client/src-tauri && \
	                    CARGO_PROFILE_RELEASE_OPT_LEVEL=3 \
	                    LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR=gen/tun-sidecar/$$t \
	                    cargo tauri build --runner cargo-xwin --no-bundle \
	                        --config $$override_rel --target $$t" ;; \
	        esac ;; \
	      *-unknown-linux-gnu) \
	        case "$$t" in \
	            aarch64-*) plat=linux/arm64; img=$(BUILDER_IMAGE_ARM64); \
	                       rustup_cache=$(RUSTUP_CACHE_ARM64); \
	                       $(MAKE) --no-print-directory _ensure-builder-arm64 ;; \
	            x86_64-*)  plat=linux/amd64; img=$(BUILDER_IMAGE_AMD64); \
	                       rustup_cache=$(RUSTUP_CACHE_AMD64); \
	                       $(MAKE) --no-print-directory _ensure-builder-amd64 ;; \
	            *) echo "  ! unsupported linux-gnu arch: $$t"; exit 2 ;; \
	        esac; \
	        docker run --platform=$$plat --rm \
	            $(PROXY_ENV) \
	            -v $(PWD):/src \
	            $(DOCKER_CACHE_COMMON) \
	            -v $$rustup_cache:/usr/local/rustup \
	            -w /src \
	            $$img bash -c "set -e; \
	                rustup target add $$t; \
	                cd apps/lantunnel-client/src-tauri && \
	                CARGO_PROFILE_RELEASE_OPT_LEVEL=3 cargo tauri build --bundles appimage \
	                    --config $$override_rel \
	                    --target $$t" ;; \
	      *) echo "  ! unsupported triple: $$t"; exit 2 ;; \
	    esac; \
	    rm -f "$$override_file"; \
	    $(MAKE) --no-print-directory _package-ui TRIPLE=$$t; \
	done

.PHONY: _package-ui
_package-ui:
	@$(MAKE) --no-print-directory _package-ui-download TRIPLE=$(TRIPLE)

.PHONY: _package-ui-download
_package-ui-download:
	@set -e; \
	mkdir -p "$(DOWNLOAD_DIR)" "$(RELEASE_DIR)"; \
	case "$(TRIPLE)" in \
	  aarch64-apple-darwin|x86_64-apple-darwin) \
	    case "$(TRIPLE)" in \
	      aarch64-apple-darwin) suffix="macos-arm64.dmg" ;; \
	      x86_64-apple-darwin) suffix="macos-amd64.dmg" ;; \
	    esac; \
	    app_dir="target/$(TRIPLE)/release/bundle/macos/$(TAURI_PRODUCT_NAME).app"; \
	    dmg="$(DOWNLOAD_DIR)/$(TAURI_BIN)-$(UI_VERSION)-$$suffix"; \
	    stage="$(RELEASE_DIR)/.dmg-$(TAURI_BIN)-$(TRIPLE)"; \
	    if [ ! -d "$$app_dir" ]; then echo "  ! missing $$app_dir"; exit 1; fi; \
	    test -x "$$app_dir/Contents/Library/LaunchServices/app.lantunnel.tun-helper" || { echo "  ! missing macOS TUN helper in $$app_dir"; exit 1; }; \
	    test -f "$$app_dir/Contents/Library/LaunchDaemons/app.lantunnel.tun-helper.plist" || { echo "  ! missing macOS TUN helper plist in $$app_dir"; exit 1; }; \
	    if [ -n "$${MACOS_CODESIGN_IDENTITY:-}" ] || [ "$${MACOS_SIGN_NOTARIZE:-0}" = "1" ]; then \
	      if [ "$${SKIP_NOTARIZE:-0}" = "1" ]; then \
	        echo "  ! macOS TUN helper releases require notarization; SKIP_NOTARIZE=1 is not allowed for $(TAURI_PRODUCT_NAME)"; \
	        exit 1; \
	      fi; \
	      scripts/macos-sign-notarize.sh "$$app_dir" "$$dmg"; \
	    elif [ "$${ALLOW_UNSIGNED_MACOS_DMG:-0}" = "1" ]; then \
	      echo "  ! creating unsigned local macOS DMG; do not publish this artifact"; \
	      rm -rf "$$stage"; mkdir -p "$$stage"; \
	      cp -R "$$app_dir" "$$stage/$(TAURI_PRODUCT_NAME).app"; \
	      ln -s /Applications "$$stage/Applications"; \
	      hdiutil create -volname "$(TAURI_PRODUCT_NAME)" -srcfolder "$$stage" -ov -format UDZO "$$dmg" >/dev/null; \
	      rm -rf "$$stage"; \
	    else \
	      echo "  ! macOS helper releases require Developer ID signing/notarization"; \
	      echo "    Set MACOS_CODESIGN_IDENTITY and ASC_* for release, or ALLOW_UNSIGNED_MACOS_DMG=1 for local-only testing."; \
	      exit 1; \
	    fi; \
	    echo "  ✓ $$dmg" ;; \
	  x86_64-pc-windows-msvc) \
	    : "The executable itself, not the NSIS installer. build.rs embeds"; \
	    : "hev-socks5-tunnel.exe, wintun.dll and msys-2.0.dll with"; \
	    : "include_bytes! and ensure_bundled_sidecars unpacks them at run"; \
	    : "time, so nothing has to sit beside it — the installer existed to"; \
	    : "place files the binary already carries, and it cost the owner a"; \
	    : "version they could not tell apart from the one they had."; \
	    src="target/$(TRIPLE)/release/$(TAURI_BIN).exe"; \
	    if [ ! -f "$$src" ]; then echo "  ! missing Windows executable for $(TRIPLE)"; exit 1; fi; \
	    if ! strings "$$src" 2>/dev/null | grep -qiF wintun; then \
	      echo "  ! $$src carries no embedded TUN sidecar; it will not route once installed"; \
	      echo "    build with LANTUNNEL_BUNDLE_TUN_SIDECAR_DIR set"; \
	      exit 1; \
	    fi; \
	    dst="$(DOWNLOAD_DIR)/$(TAURI_BIN)-$(UI_VERSION)-windows-amd64.exe"; \
	    cp "$$src" "$$dst"; \
	    chmod +x "$$dst"; \
	    echo "  ✓ $$dst" ;; \
	  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) \
	    case "$(TRIPLE)" in \
	      x86_64-unknown-linux-gnu) suffix="linux-amd64.AppImage" ;; \
	      aarch64-unknown-linux-gnu) suffix="linux-arm64.AppImage" ;; \
	    esac; \
	    appimage=$$(find "target/$(TRIPLE)/release/bundle/appimage" -maxdepth 1 -type f -name '*$(TAURI_PRODUCT_NAME)*$(UI_VERSION)*.AppImage' | head -n 1); \
	    if [ -z "$$appimage" ]; then appimage=$$(find "target/$(TRIPLE)/release/bundle/appimage" -maxdepth 1 -type f -name '*$(TAURI_PRODUCT_NAME)*.AppImage' | head -n 1); fi; \
	    if [ -z "$$appimage" ] || [ ! -f "$$appimage" ]; then echo "  ! missing AppImage for $(TRIPLE)"; exit 1; fi; \
	    dst="$(DOWNLOAD_DIR)/$(TAURI_BIN)-$(UI_VERSION)-$$suffix"; \
	    cp "$$appimage" "$$dst"; \
	    chmod +x "$$dst"; \
	    echo "  ✓ $$dst" ;; \
	  *) \
	    echo "  · no UI download artifact mapping for $(TRIPLE)" ;; \
	esac

.PHONY: _ensure-builder
_ensure-builder: _ensure-cache-dirs
	@if ! docker image inspect $(BUILDER_IMAGE) >/dev/null 2>&1; then \
	    echo "→ building $(BUILDER_IMAGE) (one-shot, takes a few minutes)"; \
	    docker build -f $(BUILDER_DOCKERFILE) -t $(BUILDER_IMAGE) .; \
	fi
	@if [ ! -f $(RUSTUP_CACHE_NATIVE)/settings.toml ]; then \
	    echo "→ seeding $(RUSTUP_CACHE_NATIVE) from $(BUILDER_IMAGE) (one-shot)"; \
	    docker run --rm \
	        -v $(RUSTUP_CACHE_NATIVE):/host-rustup \
	        $(BUILDER_IMAGE) \
	        bash -c 'if [ -d /opt/rustup-seed ]; then cp -a /opt/rustup-seed/. /host-rustup/; else cp -a /usr/local/rustup/. /host-rustup/; fi'; \
	fi

# Per-platform builder images, used for *-unknown-linux-gnu (Tauri UI) so
# each build runs inside a native-arch container with matching gcc + system
# -dev libs. Cross-compiling glibc+webkit2gtk across Debian multiarch is
# notoriously brittle; QEMU emulation via --platform is slower but reliable.
.PHONY: _ensure-builder-amd64
_ensure-builder-amd64: _ensure-cache-dirs
	@if ! docker image inspect $(BUILDER_IMAGE_AMD64) >/dev/null 2>&1; then \
	    echo "→ building $(BUILDER_IMAGE_AMD64) (one-shot, takes a few minutes)"; \
	    docker build --platform=linux/amd64 -f $(BUILDER_DOCKERFILE) -t $(BUILDER_IMAGE_AMD64) .; \
	fi
	@if [ ! -f $(RUSTUP_CACHE_AMD64)/settings.toml ]; then \
	    echo "→ seeding $(RUSTUP_CACHE_AMD64) from $(BUILDER_IMAGE_AMD64) (one-shot)"; \
	    docker run --rm --platform=linux/amd64 \
	        -v $(RUSTUP_CACHE_AMD64):/host-rustup \
	        $(BUILDER_IMAGE_AMD64) \
	        bash -c 'if [ -d /opt/rustup-seed ]; then cp -a /opt/rustup-seed/. /host-rustup/; else cp -a /usr/local/rustup/. /host-rustup/; fi'; \
	fi

# The Windows installer builder. Cross-building the NSIS package needs
# makensis, which the plain builder does not carry; everything else comes from
# the amd64 image this is layered on.
.PHONY: _ensure-builder-nsis
_ensure-builder-nsis: _ensure-builder-amd64
	@if ! docker image inspect $(BUILDER_IMAGE_NSIS) >/dev/null 2>&1; then \
	    echo "→ building $(BUILDER_IMAGE_NSIS) (one-shot, takes a few minutes)"; \
	    docker build --platform=linux/amd64 -f $(BUILDER_DOCKERFILE_NSIS) \
	        --build-arg BUILDER_IMAGE=$(BUILDER_IMAGE_AMD64) \
	        -t $(BUILDER_IMAGE_NSIS) .; \
	fi

.PHONY: _ensure-builder-arm64
_ensure-builder-arm64: _ensure-cache-dirs
	@if ! docker image inspect $(BUILDER_IMAGE_ARM64) >/dev/null 2>&1; then \
	    echo "→ building $(BUILDER_IMAGE_ARM64) (one-shot, takes a few minutes)"; \
	    docker build --platform=linux/arm64 -f $(BUILDER_DOCKERFILE) -t $(BUILDER_IMAGE_ARM64) .; \
	fi
	@if [ ! -f $(RUSTUP_CACHE_ARM64)/settings.toml ]; then \
	    echo "→ seeding $(RUSTUP_CACHE_ARM64) from $(BUILDER_IMAGE_ARM64) (one-shot)"; \
	    docker run --rm --platform=linux/arm64 \
	        -v $(RUSTUP_CACHE_ARM64):/host-rustup \
	        $(BUILDER_IMAGE_ARM64) \
	        bash -c 'if [ -d /opt/rustup-seed ]; then cp -a /opt/rustup-seed/. /host-rustup/; else cp -a /usr/local/rustup/. /host-rustup/; fi'; \
	fi

# ------------------------------ test ---------------------------------------
## == test ==

.PHONY: test
test:  ## cargo test --workspace
	@$(CARGO) test --workspace

.PHONY: fmt
fmt:  ## cargo fmt --all
	@$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check:  ## cargo fmt --all -- --check (CI gate)
	@$(CARGO) fmt --all -- --check

.PHONY: clippy
clippy:  ## cargo clippy (warnings as errors)
	@$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: check
check: fmt-check clippy test  ## Pre-commit gate (fmt + clippy + test)

# ------------------------------ docker -------------------------------------
## == docker ==

.PHONY: docker-builder
docker-builder:  ## Build the cross-compile builder image (tp-builder:latest) — one-shot
	@docker build -f $(BUILDER_DOCKERFILE) -t $(BUILDER_IMAGE) .
	@echo "  ✓ $(BUILDER_IMAGE)"

.PHONY: docker-image
docker-image:  ## Build $(DOCKER_FULL):$(DOCKER_TAG) — x86_64 linux gateway image
	@$(MAKE) --no-print-directory _ensure-builder
	@$(DOCKER_RUN_BUILDER) bash -c "set -e; \
	    rustup target add x86_64-unknown-linux-musl; \
	    cargo zigbuild --profile $(BUILD_PROFILE) --target x86_64-unknown-linux-musl -p lantunnel-gateway"
	@docker build --platform=linux/amd64 \
	    --build-arg GATEWAY_BIN=target/x86_64-unknown-linux-musl/$(BUILD_PROFILE)/lantunnel-gateway \
	    -t $(DOCKER_FULL):$(DOCKER_TAG) \
	    -t $(DOCKER_FULL):latest \
	    .
	@echo "  ✓ $(DOCKER_FULL):$(DOCKER_TAG)"

# ------------------------------ deploy -------------------------------------
## == deploy ==

.PHONY: deploy
deploy:  ## Print the minimal static V2 provisioning and startup workflow
	@echo ""
	@echo "  1) Install Gateway on the public host and Admin on a trusted owner machine:"
	@echo ""
	@echo "     install -m 0755 lantunnel-gateway-$(VERSION)-<triple> ./lantunnel-gateway"
	@echo ""
	@echo "  2) On the Gateway host, initialize the fixed public-IP deployment locally:"
	@echo ""
	@echo "     ./lantunnel-gateway init --public-ip <PUBLIC_IP>"
	@echo "     # defaults: QUIC/UDP 8443, mapping UDP 8444, configs/gateway.yaml"
	@echo "     # for another mapping port, append --mapping-port <PORT> and pass"
	@echo "     # the same value to --gateway-mapping-port in the Admin command below"
	@echo ""
	@echo "  3) Copy only certs/server.crt to the owner machine, then generate the Tunnel:"
	@echo ""
	@echo "     ./lantunnel-admin init-tunnel --gateway-transport quic --gateway-ip <PUBLIC_IP> --gateway-port 8443 --gateway-mapping-port 8444 --gateway-cert ./server.crt"
	@echo "     ./lantunnel-admin add-peer --tunnel <Tunnel>.tunnel"
	@echo ""
	@echo "  4) Copy only the public Scope back to the Gateway, validate, and start:"
	@echo ""
	@echo "     cp <Tunnel>.scope state/scopes.d/"
	@echo "     ./lantunnel-gateway --config configs/gateway.yaml --check-config"
	@echo "     ./lantunnel-gateway --config configs/gateway.yaml"
	@echo ""
	@echo "  5) Import each private .peer on its intended Client, then connect:"
	@echo ""
	@echo "     ./lantunnel-client tunnel import <Peer>.peer"
	@echo "     ./lantunnel-client connect <Tunnel ID>"
	@echo ""

# ------------------------------ certs / housekeeping -----------------------
## == housekeeping ==

.PHONY: certs
certs:  ## Generate a self-signed localhost cert under certs/
	@scripts/generate_certs.sh

$(CERT_FILE) $(CERT_KEY):
	@scripts/generate_certs.sh

.PHONY: clean
clean:  ## Remove target/ and dist/
	@$(CARGO) clean
	@rm -rf $(DIST_DIR)
	@echo "  ✓ cleaned"

.PHONY: version
version:  ## Print version / commit / host
	@echo "version: $(VERSION)"
	@echo "ui_version: $(UI_VERSION)"
	@echo "commit:  $(COMMIT)"
	@echo "host:    $(HOST_OS)/$(HOST_ARCH)"
