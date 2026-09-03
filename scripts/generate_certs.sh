#!/usr/bin/env bash
#
# Generate a self-signed TLS cert for the QUIC transport + HTTPS proxy.
#
# Usage:
#   scripts/generate_certs.sh [primary_cn] [extra_san ...]
#
# Any bare argument is added to the Subject Alternative Names. Arguments that
# look like an IPv4 address become "IP:...", everything else becomes "DNS:...".
# `localhost` and `127.0.0.1` are always added if missing. The cert is valid
# for 10 years (3650 days) — fine for dev/test; replace with a real CA-signed
# cert for production.
#
# Emits the same YAML-compatible material the Go implementation did, so configs
# and same test suites run against the Rust binaries.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CERT_DIR="${CERT_DIR:-$REPO_ROOT/certs}"

PRIMARY="${1:-localhost}"
shift 2>/dev/null || true

mkdir -p "$CERT_DIR"

SAN=""
append_san() {
    local v="$1"
    [[ -z "$v" ]] && return
    if [[ -n "$SAN" ]]; then
        SAN="$SAN,"
    fi
    if [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        SAN="${SAN}IP:$v"
    else
        SAN="${SAN}DNS:$v"
    fi
}

# Primary CN always appears in SAN too (some clients require it).
append_san "$PRIMARY"
for extra in "$@"; do
    append_san "$extra"
done
# Safety defaults for loopback testing.
if [[ ! "$SAN" =~ "DNS:localhost" ]]; then append_san "localhost"; fi
if [[ ! "$SAN" =~ "IP:127.0.0.1" ]]; then append_san "127.0.0.1"; fi
# Default gateway fallback domains. Production gateway hostnames normally use
# <node>.gt.<suffix>, so include both one-label and gt-scoped wildcards.
for fallback_domain in \
    "*.lt.example.net" \
    "*.example.net" \
    "*.gt.example.net" \
    "*.gt.example.net"; do
    if [[ ! "$SAN" =~ "DNS:${fallback_domain//\*/\\*}" ]]; then
        append_san "$fallback_domain"
    fi
done

echo "→ generating cert"
echo "  CN:  $PRIMARY"
echo "  SAN: $SAN"
echo "  out: $CERT_DIR/server.{crt,key}"

openssl req -x509 -newkey rsa:4096 \
    -keyout "$CERT_DIR/server.key" \
    -out "$CERT_DIR/server.crt" \
    -days 3650 -nodes \
    -subj "/CN=$PRIMARY" \
    -addext "subjectAltName = $SAN" \
    -addext "basicConstraints = critical, CA:FALSE" \
    -addext "keyUsage = critical, digitalSignature, keyEncipherment" \
    -addext "extendedKeyUsage = serverAuth" \
    2>/dev/null

chmod 600 "$CERT_DIR/server.key"
chmod 644 "$CERT_DIR/server.crt"

echo "✓ certificate valid for 10 years written to $CERT_DIR"
