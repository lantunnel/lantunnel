#!/bin/sh
set -eu

PEER=${1:?peer number is required}
case "$PEER" in
    1|2|3) ;;
    *) echo "invalid peer number: $PEER" >&2; exit 2 ;;
esac

test -f "/state/client${PEER}/settings.json"
TUNNEL_ID=$(sed -n '1p' /state/tunnel-id)
test -n "$TUNNEL_ID"

exec lantunnel-client connect "$TUNNEL_ID"

