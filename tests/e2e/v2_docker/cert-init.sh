#!/bin/sh
set -eu

CERT=/certs/server.crt
KEY=/certs/server.key

if [ -f "$CERT" ] && [ -f "$KEY" ]; then
    chmod 0644 "$CERT"
    chmod 0600 "$KEY"
    chown 10001:10001 "$CERT" "$KEY"
    exit 0
fi

if [ -e "$CERT" ] || [ -e "$KEY" ]; then
    echo "refusing partial persistent Gateway certificate state" >&2
    exit 1
fi

umask 077
openssl req -x509 -newkey rsa:2048 \
    -keyout "$KEY" \
    -out "$CERT" \
    -days 3650 -nodes \
    -subj "/CN=gateway" \
    -addext "subjectAltName = DNS:gateway" \
    -addext "basicConstraints = critical, CA:FALSE" \
    -addext "keyUsage = critical, digitalSignature, keyEncipherment" \
    -addext "extendedKeyUsage = serverAuth"
chmod 0644 "$CERT"
chmod 0600 "$KEY"
chown 10001:10001 "$CERT" "$KEY"

