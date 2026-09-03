#!/bin/sh
set -eu

. /accept/peer-invariants.sh

if [ -f /state/provisioning-complete ]; then
    for peer in 1 2 3; do
        test -f "/state/peers/peer${peer}.peer"
        test -f "/state/client${peer}/settings.json"
    done
    test -f /state/tunnel-id
    test -f /state/scopes.d/tunnel.scope
    TUNNEL_ID=$(cat /state/tunnel-id)
    assert_distinct_peer_profiles "$TUNNEL_ID" \
        /state/peers/peer1.peer \
        /state/peers/peer2.peer \
        /state/peers/peer3.peer
    exit 0
fi

if [ -e /state/tunnel-id ] || [ -e /state/provision ]; then
    echo "refusing partial V2 provisioning state; run the acceptance cleanup" >&2
    exit 1
fi

umask 077
mkdir -p /state/provision /state/peers /state/scopes.d /state/gateway

V2_GATEWAY_TRANSPORT=${V2_GATEWAY_TRANSPORT:-quic}
case "$V2_GATEWAY_TRANSPORT" in
    quic|websocket|grpc) ;;
    *) echo "invalid V2 Gateway transport: $V2_GATEWAY_TRANSPORT" >&2; exit 2 ;;
esac

lantunnel-admin init-tunnel \
    --gateway-transport "$V2_GATEWAY_TRANSPORT" \
    --gateway-host gateway \
    --gateway-port 8443 \
    --gateway-cert /certs/server.crt \
    --output-dir /state/provision

set -- /state/provision/*.tunnel
if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
    echo "expected exactly one generated .tunnel file" >&2
    exit 1
fi
TUNNEL_FILE=$1
TUNNEL_ID=$(basename "$TUNNEL_FILE" .tunnel)
SCOPE_FILE="/state/provision/${TUNNEL_ID}.scope"
test -f "$SCOPE_FILE"

lantunnel-admin add-peer --tunnel "$TUNNEL_FILE" --name peer1 --output /state/peers/peer1.peer
lantunnel-admin add-peer --tunnel "$TUNNEL_FILE" --name peer2 --output /state/peers/peer2.peer
lantunnel-admin add-peer --tunnel "$TUNNEL_FILE" --name peer3 --output /state/peers/peer3.peer

# A `.peer` is one logical Client identity. Refuse duplicate paths, file
# identities, contents, stable Peer IDs, or Overlay IPs before importing any
# profile into a Client config directory.
assert_distinct_peer_profiles "$TUNNEL_ID" \
    /state/peers/peer1.peer \
    /state/peers/peer2.peer \
    /state/peers/peer3.peer

cp "$SCOPE_FILE" /state/scopes.d/tunnel.scope
printf '%s\n' "$TUNNEL_ID" > /state/tunnel-id

for peer in 1 2 3; do
    CONFIG_DIR="/state/client${peer}"
    TUNNEL_PROXY_APP_CONFIG_DIR="$CONFIG_DIR" \
        lantunnel-client tunnel import "/state/peers/peer${peer}.peer"
    cp /accept/settings.json "$CONFIG_DIR/settings.json"
done

touch /state/provisioning-complete
chown -R 10001:10001 /state
chmod 0644 /state/tunnel-id /state/scopes.d/tunnel.scope /state/provisioning-complete
