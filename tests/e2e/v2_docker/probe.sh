#!/bin/sh
set -eu

TARGET_PEER=${1:?target Peer number is required}
case "$TARGET_PEER" in
    1|2|3) ;;
    *) echo "invalid target Peer number: $TARGET_PEER" >&2; exit 2 ;;
esac

PEER_FILE="/state/peers/peer${TARGET_PEER}.peer"
test -f "$PEER_FILE"
OVERLAY_IP=$(sed -n 's/^[[:space:]]*overlay_ip:[[:space:]]*//p' "$PEER_FILE" | sed -n '1p')
case "$OVERLAY_IP" in
    198.18.*.*) ;;
    *) echo "invalid generated Overlay IP: $OVERLAY_IP" >&2; exit 1 ;;
esac

tp-e2e-p1 \
    --test socks5_tcp_connect \
    --proxy 127.0.0.1:1080 \
    --target "${OVERLAY_IP}:18999"

tp-e2e-p1 \
    --test socks5_udp_associate \
    --proxy 127.0.0.1:1080 \
    --udp-target "${OVERLAY_IP}:18997"
