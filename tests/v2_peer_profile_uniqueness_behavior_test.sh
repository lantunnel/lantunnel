#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INVARIANTS="$ROOT_DIR/tests/e2e/v2_docker/peer-invariants.sh"

# shellcheck source=tests/e2e/v2_docker/peer-invariants.sh
source "$INVARIANTS"

work="$(mktemp -d)"
trap 'rm -R -- "$work"' EXIT

write_peer() {
  local path="$1"
  local tunnel_id="$2"
  local peer_id="$3"
  local overlay_ip="$4"
  cat >"$path" <<EOF
version: 2
tunnel_id: $tunnel_id
peer:
  peer_id: $peer_id
  overlay_ip: $overlay_ip
EOF
}

tunnel_id=11111111-1111-4111-8111-111111111111
write_peer "$work/peer1.peer" "$tunnel_id" aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1 198.18.0.1
write_peer "$work/peer2.peer" "$tunnel_id" aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa2 198.18.0.2
write_peer "$work/peer3.peer" "$tunnel_id" aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa3 198.18.0.3

assert_distinct_peer_profiles "$tunnel_id" \
  "$work/peer1.peer" "$work/peer2.peer" "$work/peer3.peer"

if assert_distinct_peer_profiles "$tunnel_id" \
  "$work/peer1.peer" "$work/peer1.peer" "$work/peer3.peer" \
  >"$work/path-collision.stdout" 2>"$work/path-collision.stderr"; then
  echo 'duplicate Peer profile path was accepted' >&2
  exit 1
fi

ln "$work/peer1.peer" "$work/peer1-alias.peer"
if assert_distinct_peer_profiles "$tunnel_id" \
  "$work/peer1.peer" "$work/peer1-alias.peer" "$work/peer3.peer" \
  >"$work/identity-collision.stdout" 2>"$work/identity-collision.stderr"; then
  echo 'hard-linked Peer profile files were accepted' >&2
  exit 1
fi

write_peer "$work/duplicate-peer-id.peer" "$tunnel_id" \
  aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1 198.18.0.9
if assert_distinct_peer_profiles "$tunnel_id" \
  "$work/peer1.peer" "$work/duplicate-peer-id.peer" "$work/peer3.peer" \
  >"$work/peer-id-collision.stdout" 2>"$work/peer-id-collision.stderr"; then
  echo 'duplicate stable Peer IDs were accepted' >&2
  exit 1
fi

write_peer "$work/duplicate-overlay.peer" "$tunnel_id" \
  aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa9 198.18.0.1
if assert_distinct_peer_profiles "$tunnel_id" \
  "$work/peer1.peer" "$work/duplicate-overlay.peer" "$work/peer3.peer" \
  >"$work/overlay-collision.stdout" 2>"$work/overlay-collision.stderr"; then
  echo 'duplicate Overlay IPs were accepted' >&2
  exit 1
fi

echo 'v2 Peer profile uniqueness behavior: PASS'
