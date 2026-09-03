#!/usr/bin/env bash
# Regenerate the self-signed cert pair under <repo-root>/certs/ used by
# the Rust gateway in E2E matrix cells B and C.
#
# Usage:
#   ./tests/e2e/_fixtures/regen-certs.sh           # regenerate, prompt if exists
#   ./tests/e2e/_fixtures/regen-certs.sh --force   # overwrite without asking
#
# The Go gateway has its own cert pair at ~/github/tunnel-proxy/certs/ that
# is regenerated via that repo's `make certs` target.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
CERT_DIR="${REPO_ROOT}/certs"
CRT="${CERT_DIR}/server.crt"
KEY="${CERT_DIR}/server.key"
CN="${TP_E2E_CERT_CN:-gateway-01.example.net}"
DAYS="${TP_E2E_CERT_DAYS:-3650}"

force=0
if [[ "${1:-}" == "--force" ]]; then
  force=1
fi

if [[ -f "${CRT}" || -f "${KEY}" ]] && [[ "${force}" -eq 0 ]]; then
  echo "Existing cert found at ${CERT_DIR}. Re-run with --force to overwrite." >&2
  echo "  ${CRT}" >&2
  echo "  ${KEY}" >&2
  exit 1
fi

mkdir -p "${CERT_DIR}"

openssl req \
  -x509 \
  -newkey rsa:4096 \
  -keyout "${KEY}" \
  -out "${CRT}" \
  -days "${DAYS}" \
  -nodes \
  -subj "/CN=${CN}" \
  -addext "subjectAltName=DNS:${CN},DNS:localhost,IP:127.0.0.1,IP:::1"

chmod 600 "${KEY}"
chmod 644 "${CRT}"

echo "Wrote self-signed cert (CN=${CN}, ${DAYS}d validity):"
echo "  ${CRT}"
echo "  ${KEY}"
