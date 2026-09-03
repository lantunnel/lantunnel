# syntax=docker/dockerfile:1.7
#
# Cross-compile builder image — produces Linux musl binaries AND
# Windows x86_64 binaries from a single Linux container, so macOS users
# don't need to install zig / cargo-xwin / llvm / nsis on the host.
#
# Used by:
#   make release-lantunnel-gateway-linux-amd64      — gateway musl binary
#   make release-lantunnel-client-linux-amd64       — Tauri Client AppImage
#
# Build once:
#   make docker-builder
#
# Runtime pattern (set by the Makefile targets):
#   docker run --rm -v $PWD:/src -v tp-cargo-cache:/usr/local/cargo/registry \
#              -v tp-target-cache:/src/target tp-builder:latest <cmd>
#
# Notes:
#   - We pin zig 0.14 to match cargo-zigbuild 0.22.x ABI (same rationale as
#     the Makefile deps-cross target).
#   - Final image size ~2.5 GB; mostly rust stdlib + node_modules for
#     Tauri. Acceptable for a dev tool.

# cargo-xwin 0.21.x needs rustc >= 1.89; cargo-zigbuild 0.22.x needs >= 1.88.
ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-bookworm

# ---- system deps ----------------------------------------------------------
# clang/lld:   cargo-xwin link step
# nodejs:      Tauri frontend build (vite)
# xz-utils:    unpack zig tarball
# libssl-dev:  host-side openssl (not linked into cross artifacts, used by
#              build.rs for some crates on host target)
# Drop bookworm-updates suite — Debian's fastly mirrors have been flaky on
# arm64 (persistent 502s on the -updates Packages index) and we don't need
# freshness from it for a toolchain image. `bookworm` + `bookworm-security`
# are enough for every package below.
# Switch deb.debian.org → TUNA (Tsinghua) mirror; drop bookworm-updates
# suite; retry apt-get update + apt-get install up to 5× / 3× to ride out
# mirror blips. Override APT_MIRROR at build time for different regions:
#   --build-arg APT_MIRROR=https://mirrors.ustc.edu.cn/debian
#   --build-arg APT_MIRROR=http://ftp.us.debian.org/debian
#
# NSIS intentionally NOT installed — it pulls ~300 tiny node-* packages
# that amplify any mirror flakiness, and Tauri can produce raw .exe
# artefacts from cargo-xwin without an installer. If you need a Windows
# .exe installer, build on a Windows host or CI.
ARG APT_MIRROR=https://mirrors.tuna.tsinghua.edu.cn/debian
ARG APT_SECURITY_MIRROR=https://mirrors.tuna.tsinghua.edu.cn/debian-security
RUN set -eux; \
    if [ -f /etc/apt/sources.list.d/debian.sources ]; then \
        sed -i -E "s|http://deb.debian.org/debian-security|${APT_SECURITY_MIRROR}|g; \
                   s|http://deb.debian.org/debian|${APT_MIRROR}|g; \
                   s/(^Suites:.*) bookworm-updates/\1/" \
            /etc/apt/sources.list.d/debian.sources; \
    fi; \
    if [ -f /etc/apt/sources.list ]; then \
        sed -i "/bookworm-updates/d; \
                s|http://deb.debian.org/debian-security|${APT_SECURITY_MIRROR}|g; \
                s|http://deb.debian.org/debian|${APT_MIRROR}|g" \
            /etc/apt/sources.list; \
    fi; \
    for i in 1 2 3 4 5; do apt-get update && break || { echo "apt update retry $i"; sleep $((i*5)); }; done; \
    for i in 1 2 3; do \
        apt-get install -y --no-install-recommends \
            build-essential pkg-config curl git ca-certificates xz-utils \
            clang lld llvm \
            libssl-dev \
            protobuf-compiler \
            nodejs npm \
            libayatana-appindicator3-dev \
            libwebkit2gtk-4.1-dev \
            librsvg2-dev \
        && break \
        || { echo "apt install retry $i"; sleep $((i*10)); }; \
    done; \
    rm -rf /var/lib/apt/lists/*

# ---- clang-cl symlink -----------------------------------------------------
# cargo-xwin passes MSVC-style flags (/imsvc …) that clang only parses in
# CL-driver mode. Debian's `clang` package doesn't ship a clang-cl symlink,
# so create one manually.
RUN ln -sf "$(which clang)" /usr/local/bin/clang-cl

# ---- zig (for cargo-zigbuild musl cross) ----------------------------------
ARG ZIG_VERSION=0.14.0
RUN arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) zarch="x86_64" ;; \
        arm64) zarch="aarch64" ;; \
        *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${zarch}-${ZIG_VERSION}.tar.xz" \
        | tar -xJ -C /opt \
    && ln -s "/opt/zig-linux-${zarch}-${ZIG_VERSION}/zig" /usr/local/bin/zig \
    && zig version

# ---- cargo cross-compile front-ends ---------------------------------------
RUN cargo install --locked cargo-zigbuild cargo-xwin

# ---- rust: install "stable" channel + workspace components + cross targets.
#
# The workspace pins `channel = "stable"` in rust-toolchain.toml. If that
# toolchain isn't present inside the container at build time, rustup will
# fetch it on first cargo invocation (~500MB). Installing it here ensures
# a fresh host-side /usr/local/rustup cache (seeded from /opt/rustup-seed
# below) starts pre-provisioned — every subsequent container run reuses
# the host cache and skips the download entirely.
#
# `rustup default stable` is required so that /usr/local/rustup/settings.toml
# names "stable" as the default toolchain; without this, cargo tools
# invoked outside rust-toolchain.toml scope would fall back to the base
# image's rustc.
RUN rustup toolchain install stable --profile minimal \
        --component rustfmt --component clippy \
    && rustup default stable \
    && rustup target add --toolchain stable \
        x86_64-unknown-linux-musl \
        aarch64-unknown-linux-musl \
        x86_64-pc-windows-msvc

# ---- tauri cli (used by the windows UI build) -----------------------------
RUN cargo install --locked tauri-cli --version "^2"

# ---- rustup seed snapshot -------------------------------------------------
# Freeze the fully-provisioned rustup dir so the Makefile's host-cache
# bootstrap (`make build-*` → `_ensure-builder*`) can `cp -a` from here
# on first run. Without this, bind-mounting an empty host dir over
# /usr/local/rustup would wipe all the work above and trigger a fresh
# rustup sync every time the cache is cleared.
RUN cp -a /usr/local/rustup /opt/rustup-seed

WORKDIR /src

# Self-documenting default: prints installed toolchain versions.
CMD ["bash", "-c", "echo '# tp-builder ready'; rustc -V; cargo zigbuild --version; cargo xwin --version; node -v; cargo tauri --version"]
