#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../../.." && pwd)
COMPOSE_FILE="$SCRIPT_DIR/compose.yaml"
PROJECT=lantunnel-v2-local

V2_GATEWAY_TRANSPORT=${V2_GATEWAY_TRANSPORT:-quic}
case "$V2_GATEWAY_TRANSPORT" in
    quic) V2_GATEWAY_CONFIG=/accept/gateway.yaml ;;
    websocket) V2_GATEWAY_CONFIG=/accept/gateway-websocket.yaml ;;
    grpc) V2_GATEWAY_CONFIG=/accept/gateway-grpc.yaml ;;
    *)
        echo "V2_GATEWAY_TRANSPORT must be quic, websocket, or grpc" >&2
        exit 2
        ;;
esac
export V2_GATEWAY_TRANSPORT V2_GATEWAY_CONFIG

compose() {
    docker compose -f "$COMPOSE_FILE" "$@"
}

cleanup() {
    STATUS=$?
    trap - EXIT
    if [ "$STATUS" -ne 0 ]; then
        compose ps >&2 || true
        compose logs --no-color mapping gateway client1 client2 client3 >&2 || true
    fi
    if [ "${KEEP_V2_DOCKER:-0}" != "1" ]; then
        compose down --volumes --remove-orphans >/dev/null 2>&1 || true
    fi
    exit "$STATUS"
}
trap cleanup EXIT

compose down --volumes --remove-orphans >/dev/null 2>&1 || true
if [ -n "${V2_DOCKER_BUILDER:-}" ]; then
    docker buildx build \
        --builder "$V2_DOCKER_BUILDER" \
        --platform "${V2_DOCKER_PLATFORM:-linux/arm64}" \
        --progress=plain \
        --load \
        --file "$REPO_ROOT/Dockerfile" \
        --build-arg BUILD_PROFILE=release-perf \
        --tag lantunnel:2.0-dev \
        "$REPO_ROOT"
else
    docker build --file "$REPO_ROOT/Dockerfile" \
        --build-arg BUILD_PROFILE=release-perf \
        --tag lantunnel:2.0-dev \
        "$REPO_ROOT"
fi
if [ "${SKIP_V2_HELPER_BUILD:-0}" = "1" ]; then
    compose up --no-build --detach --wait --wait-timeout 240 gateway
else
    compose up --build --detach --wait --wait-timeout 240 gateway
fi

# Verify the persistent certificate before any Client starts. Restarting the
# Gateway while all three PeerLinks are first negotiating would add unrelated
# attachment churn to the Direct-path acceptance.
CERT_BEFORE=$(compose run --rm --no-deps cert-init sha256sum /certs/server.crt)
CERT_BEFORE=${CERT_BEFORE%% *}
compose restart gateway
compose up --no-build --detach --wait --wait-timeout 120 gateway
CERT_AFTER=$(compose run --rm --no-deps cert-init sha256sum /certs/server.crt)
CERT_AFTER=${CERT_AFTER%% *}
if [ -z "$CERT_BEFORE" ] || [ "$CERT_BEFORE" != "$CERT_AFTER" ]; then
    echo "persistent Gateway certificate changed across restart" >&2
    exit 1
fi

if [ "${SKIP_V2_HELPER_BUILD:-0}" = "1" ]; then
    compose up --no-build --detach --wait --wait-timeout 240
else
    compose up --build --detach --wait --wait-timeout 240
fi

# The first generation shares one underlay with the Gateway and can therefore
# establish real LAN Direct paths.
compose exec -T echo1 /bin/sh /accept/wait-path.sh 1 2 P2p
compose exec -T echo2 /bin/sh /accept/wait-path.sh 2 3 P2p
compose exec -T echo3 /bin/sh /accept/wait-path.sh 3 1 P2p

MESH_NETWORK=$(docker network ls \
    --filter "label=com.docker.compose.project=$PROJECT" \
    --filter "label=com.docker.compose.network=mesh" \
    --format '{{.Name}}')
test -n "$MESH_NETWORK"

# Move each live Client onto its own Gateway-only underlay, then remove the
# common Direct network. The processes, DNS name, QUIC port, local proxy,
# Scope, certificate, and PeerLink identity all stay unchanged.
for PEER in 1 2 3; do
    CLIENT_ID=$(compose ps -q "client${PEER}")
    PEER_NETWORK=$(docker network ls \
        --filter "label=com.docker.compose.project=$PROJECT" \
        --filter "label=com.docker.compose.network=peer${PEER}_gateway" \
        --format '{{.Name}}')
    test -n "$CLIENT_ID"
    test -n "$PEER_NETWORK"
    docker network connect "$PEER_NETWORK" "$CLIENT_ID"
done
for PEER in 1 2 3; do
    CLIENT_ID=$(compose ps -q "client${PEER}")
    docker network disconnect "$MESH_NETWORK" "$CLIENT_ID"
done

compose exec -T echo1 /bin/sh /accept/wait-path.sh 1 2 Relay
compose exec -T echo2 /bin/sh /accept/wait-path.sh 2 3 Relay
compose exec -T echo3 /bin/sh /accept/wait-path.sh 3 1 Relay

if compose logs --no-color client1 client2 client3 \
    | grep -E 'encrypted Relay control authentication failed|could not authenticate V2 Relay' \
    >/dev/null; then
    echo "V2 encrypted Relay authentication error found in Client logs" >&2
    exit 1
fi

echo "Lantunnel 2.0 Docker acceptance: transport=$V2_GATEWAY_TRANSPORT, 3 Peers, persistent TLS, Direct TCP/UDP, encrypted Relay TCP/UDP PASS"
