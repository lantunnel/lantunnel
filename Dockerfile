#
# Multi-stage build for the three public Lantunnel 2.0 binaries.
#
# Stage 1 (builder) — compile release binaries with cargo on debian-slim. We
# deliberately avoid musl/alpine for the builder because quinn/rustls pull in
# ring, which is faster to build against glibc. The runtime stage is
# debian:stable-slim (ca-certificates + small).
#
# Build args:
#   VERSION     — workspace version (baked into image labels)
#   COMMIT      — short git sha
#   BUILD_TIME  — ISO-8601 UTC
#
# Typical build:
#   docker build --build-arg VERSION=0.1.0 -t lantunnel:dev .

ARG RUST_VERSION=1.89
ARG FRONTEND_IMAGE=node:22-bookworm-slim
ARG BUILDER_IMAGE=rust:${RUST_VERSION}-slim-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

# ---------- frontend ------------------------------------------------------

FROM ${FRONTEND_IMAGE} AS frontend

WORKDIR /frontend
ARG SKIP_FRONTEND_BUILD=0
COPY apps/lantunnel-client/frontend/package.json \
     apps/lantunnel-client/frontend/package-lock.json ./
COPY apps/lantunnel-client/frontend/ ./
RUN if [ "$SKIP_FRONTEND_BUILD" = "1" ]; then \
        test -f dist/index.html; \
    else \
        npm ci && npm run build; \
    fi

# ---------- builder -------------------------------------------------------

FROM ${BUILDER_IMAGE} AS builder

# Toolchain dependencies for the three public binaries.
ARG SKIP_SYSTEM_DEPS=0
RUN if [ "$SKIP_SYSTEM_DEPS" != "1" ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends \
        ca-certificates \
        libayatana-appindicator3-dev \
        libgtk-3-dev \
        librsvg2-dev \
        pkg-config \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        protobuf-compiler \
      && rm -rf /var/lib/apt/lists/*; \
    fi

WORKDIR /build

# Crates.io fetch hardening — the default 30 s / 3-retry budget is easy to
# bust on docker-bridge networks with flaky DNS or slow upstream mirrors.
# These knobs are honoured by `cargo fetch` and `cargo build` alike.
ENV CARGO_HTTP_TIMEOUT=300 \
    CARGO_HTTP_LOW_SPEED_LIMIT=10 \
    CARGO_HTTP_MULTIPLEXING=false \
    CARGO_NET_RETRY=10 \
    CARGO_NET_GIT_FETCH_WITH_CLI=true \
    CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

# --- dependency layer: copy manifests + sources.
# We copy everything in one layer because cargo needs the full workspace to
# resolve path dependencies; the BuildKit cargo-registry cache mount below
# still avoids re-downloading crates when only source files change.
#
# Note: we deliberately DO NOT copy `rust-toolchain.toml`. That file pins the
# workspace to the latest `stable` channel, which makes cargo invoke rustup
# and download the newest Rust (every time). The base image already ships
# a pinned stable Rust compatible with our code — using it directly saves
# 10+ minutes of toolchain downloads per build.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY apps/ ./apps/
COPY tests/ ./tests/
COPY --from=frontend /frontend/dist/ ./apps/lantunnel-client/frontend/dist/

ARG VERSION=dev
ARG COMMIT=unknown
ARG BUILD_TIME=unknown
ARG BUILD_PROFILE=release-perf

# Fetch deps as a separate, cache-friendly step so flaky crates.io network
# failures only cost a `cargo fetch` retry — not a full rebuild. `cargo fetch`
# resolves at the workspace level (not per-package); the actual build step
# restricts to the three public V2 packages.
RUN if [ "$SKIP_SYSTEM_DEPS" != "1" ]; then cargo fetch; fi

# Build release binaries. `lantunnel-client` is one binary with UI by default
# and a headless mode; we do not ship a second Client implementation.
# `--offline` forces cargo to use the
# registry cache populated by the previous step so the build stage never
# hits the network (and so `CARGO_NET_OFFLINE=true` would be an equivalent
# expression of the intent).
RUN --mount=type=cache,target=/build/target \
    cargo build --profile "${BUILD_PROFILE}" --offline \
        -p lantunnel-gateway \
        -p lantunnel-client \
        -p lantunnel-admin \
    && mkdir -p /out \
    && cp "target/${BUILD_PROFILE}/lantunnel-gateway" /out/ \
    && cp "target/${BUILD_PROFILE}/lantunnel-client"  /out/ \
    && cp "target/${BUILD_PROFILE}/lantunnel-admin"   /out/

# ---------- runtime -------------------------------------------------------

FROM ${RUNTIME_IMAGE} AS runtime

# ca-certificates so outbound TLS to real CAs works; tini for proper PID-1
# signal forwarding so `docker stop` triggers our graceful-drain path.
ARG SKIP_SYSTEM_DEPS=0
RUN if [ "$SKIP_SYSTEM_DEPS" != "1" ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends \
        ca-certificates \
        libayatana-appindicator3-1 \
        libgtk-3-0 \
        librsvg2-2 \
        libwebkit2gtk-4.1-0 \
        tini \
        procps \
      && rm -rf /var/lib/apt/lists/*; \
    fi

# Non-root runtime user. UID/GID 10001 avoids collisions with host-side users
# when volumes are bind-mounted.
RUN groupadd --system --gid 10001 lantunnel \
    && useradd  --system --uid 10001 --gid lantunnel --home-dir /app --shell /sbin/nologin lantunnel

WORKDIR /app

# The production image contains binaries only. Gateway identity, Scope state,
# and configuration must be mounted explicitly by the operator.
COPY --from=builder /out/lantunnel-gateway \
                    /out/lantunnel-client \
                    /out/lantunnel-admin \
                    /usr/local/bin/
COPY scripts/container-entrypoint.sh /usr/local/bin/lantunnel-container-entrypoint
RUN mkdir -p /app/logs \
    && chown -R lantunnel:lantunnel /app \
    && chmod 0755 /usr/local/bin/lantunnel-container-entrypoint

USER lantunnel

# Exposed ports (all optional depending on which binary runs):
#   8443/udp   QUIC tunnel listener (gateway<->peer)
#   8444/udp   shared UDP mapping probe service (gateway)
#   8090       metrics (gateway)
# Client SOCKS and UI listeners are loopback-only by default and are not
# published from this production image. The HTTP, SOCKS5 and TUIC proxy
# frontends are not wired into the Gateway, so their ports are not published.
EXPOSE 8443/udp 8444/udp 8090

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD pgrep -f '(lantunnel-gateway|lantunnel-client)' >/dev/null || exit 1

# Labels let `docker inspect` show provenance at runtime.
ARG VERSION=dev
ARG COMMIT=unknown
ARG BUILD_TIME=unknown
LABEL org.opencontainers.image.title="Lantunnel" \
      org.opencontainers.image.description="Lantunnel 2.0 Gateway, Client, and administration CLI." \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${COMMIT}" \
      org.opencontainers.image.created="${BUILD_TIME}" \
      org.opencontainers.image.source="https://github.com/lantunnel/lantunnel" \
      org.opencontainers.image.licenses="Apache-2.0"

# tini handles SIGINT/SIGTERM correctly so our drain path runs.
ENTRYPOINT ["/usr/local/bin/lantunnel-container-entrypoint"]
# Fail closed unless the operator mounts the persistent Gateway configuration
# and identity at the documented path (or overrides the command explicitly).
CMD ["lantunnel-gateway", "--config", "/run/lantunnel/gateway.yaml"]
