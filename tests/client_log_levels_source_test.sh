#!/usr/bin/env bash
# Levels have to mean something.
#
# warn was the dumping ground: 188 statements against 3 errors, and most of
# them named outcomes the protocol is designed to produce — a P2P probe
# failing, a flow falling back to the relay, an Answer arriving for a session
# that already closed. They fire per attempt and per flow, so on a busy
# connection they were the log, and a real warning had nowhere to stand out.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC=("$ROOT/crates/tp-client/src" "$ROOT/apps/lantunnel-client/src-tauri/src")

# An outcome the design produces is not a warning.
if grep -rnE 'warn!\(' "${SRC[@]}" -A4 \
     | grep -iE 'falling back to relay|retrying proxy placement|rerunning replica placement' \
     | grep -v '^\s*//' | head -1 | grep -q .; then
  echo "a designed fallback is logged as a warning again" >&2
  exit 1
fi

# The level has to carry signal: warnings must not outnumber everything else
# put together by the margin they once did.
warns=$(grep -rho 'warn!(' "${SRC[@]}" | wc -l | tr -d ' ')
debugs=$(grep -rho 'debug!(' "${SRC[@]}" | wc -l | tr -d ' ')
if [ "$warns" -gt $((debugs * 2)) ]; then
  echo "warn is being used as the default level again ($warns warn vs $debugs debug)" >&2
  exit 1
fi
echo "log levels: $warns warn, $debugs debug"
