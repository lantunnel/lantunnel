ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

ENV CARGO_HTTP_TIMEOUT=300 \
    CARGO_NET_RETRY=10 \
    CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked \
        -p tp-e2e-p1-connectivity \
        -p tp-e2e-p2-latency \
        -p tp-e2e-p3-throughput \
        -p tp-e2e-echo-services \
    && mkdir -p /out \
    && cp target/release/tp-e2e-p1 /out/ \
    && cp target/release/tp-e2e-p2 /out/ \
    && cp target/release/tp-e2e-p3 /out/ \
    && cp target/release/tp-e2e-echo-services /out/

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 lantunnel \
    && useradd --system --uid 10001 --gid lantunnel --home-dir /nonexistent --shell /sbin/nologin lantunnel

COPY --from=builder /out/tp-e2e-p1 /out/tp-e2e-p2 /out/tp-e2e-p3 /out/tp-e2e-echo-services /usr/local/bin/

USER lantunnel
