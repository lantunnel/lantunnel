#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="$ROOT_DIR/tests/e2e/v2_docker/compose.yaml"
PROVISION_SCRIPT="$ROOT_DIR/tests/e2e/v2_docker/provision.sh"
PEER_INVARIANTS="$ROOT_DIR/tests/e2e/v2_docker/peer-invariants.sh"
PEER_UNIQUENESS_BEHAVIOR="$ROOT_DIR/tests/v2_peer_profile_uniqueness_behavior_test.sh"
CERT_SCRIPT="$ROOT_DIR/tests/e2e/v2_docker/cert-init.sh"
PROBE_MAIN="$ROOT_DIR/tests/e2e/connectivity/src/main.rs"
RUN_SCRIPT="$ROOT_DIR/tests/e2e/v2_docker/run.sh"
PROBE_SCRIPT="$ROOT_DIR/tests/e2e/v2_docker/probe.sh"
WAIT_PATH_SCRIPT="$ROOT_DIR/tests/e2e/v2_docker/wait-path.sh"
PRODUCTION_DOCKERFILE="$ROOT_DIR/Dockerfile"

test -f "$COMPOSE_FILE"
grep -q -- 'image: lantunnel:2.0-dev' "$COMPOSE_FILE"
grep -Fq -- 'command: ["lantunnel-gateway", "mapping", "serve"]' "$COMPOSE_FILE"
grep -Fq -- 'network_mode: "service:mapping"' "$COMPOSE_FILE"
test "$(grep -Fc -- 'aliases: [gateway]' "$COMPOSE_FILE")" -eq 4
if grep -R -Eq -- 'p2p_enabled|disable-p2p|no-p2p|enable-p2p' \
  "$ROOT_DIR/tests/e2e/v2_docker"; then
  echo 'V2 Docker acceptance must use the fixed Mesh and contain no public Mesh toggle' >&2
  exit 1
fi

for peer in 1 2 3; do
    grep -q -- "client${peer}:" "$COMPOSE_FILE"
    grep -q -- "TUNNEL_PROXY_APP_CONFIG_DIR: /state/client${peer}" "$COMPOSE_FILE"
done

# V2 Local SOCKS is intentionally unauthenticated and must never leave the
# Client namespace. Probes use network_mode: service:<client> instead.
grep -q -- 'LANTUNNEL_LOCAL_SOCKS5_LISTEN: 127.0.0.1:1080' "$COMPOSE_FILE"
if grep -q -- ':1080:1080' "$COMPOSE_FILE"; then
  echo 'acceptance must not publish the loopback-only Client SOCKS listener' >&2
  exit 1
fi

test -f "$PROVISION_SCRIPT"
test -f "$PEER_INVARIANTS"
bash "$PEER_UNIQUENESS_BEHAVIOR"
grep -q -- 'lantunnel-admin init-tunnel' "$PROVISION_SCRIPT"
test "$(grep -c -- 'lantunnel-admin add-peer' "$PROVISION_SCRIPT")" -eq 3
grep -q -- '/state/scopes.d' "$PROVISION_SCRIPT"
grep -Fq -- '. /accept/peer-invariants.sh' "$PROVISION_SCRIPT"
grep -Fq -- 'assert_distinct_peer_profiles "$TUNNEL_ID"' "$PROVISION_SCRIPT"
gate_line="$(grep -n -m1 -- 'assert_distinct_peer_profiles' "$PROVISION_SCRIPT" | cut -d: -f1)"
import_line="$(grep -n -m1 -- 'lantunnel-client tunnel import' "$PROVISION_SCRIPT" | cut -d: -f1)"
[[ "$gate_line" -lt "$import_line" ]]

test -f "$CERT_SCRIPT"
grep -q -- '/certs/server.crt' "$CERT_SCRIPT"
grep -q -- 'DNS:gateway' "$CERT_SCRIPT"
grep -q -- 'certs:/certs' "$COMPOSE_FILE"

# The acceptance probe has one V2 Client loopback SOCKS mode: NO AUTH.
if grep -Eq -- 'no_auth: bool|cell_positional|user: String|pass: String' "$PROBE_MAIN"; then
  echo 'V2 connectivity probe still exposes a Legacy auth or matrix selector' >&2
  exit 1
fi
grep -q -- 'connect_no_auth' "$ROOT_DIR/tests/e2e/connectivity/src/socks5.rs"

test -f "$PROBE_SCRIPT"
grep -q -- 'socks5_tcp_connect' "$PROBE_SCRIPT"
grep -q -- 'socks5_udp_associate' "$PROBE_SCRIPT"
if grep -Eq -- '--(cell|no-auth|user|pass)' "$PROBE_SCRIPT"; then
  echo 'V2 Docker probe still passes a removed auth or matrix selector' >&2
  exit 1
fi

test -f "$WAIT_PATH_SCRIPT"
grep -q -- 'selected replica lane for TCP open' "$WAIT_PATH_SCRIPT"
grep -q -- 'selected replica lane for UDP open' "$WAIT_PATH_SCRIPT"

test -f "$RUN_SCRIPT"
# Every acceptance run rebuilds the product image from the production
# Dockerfile. Only the probe/helper image may use a test-specific Dockerfile.
grep -Fq -- 'docker build --file "$REPO_ROOT/Dockerfile"' "$RUN_SCRIPT"
grep -Fq -- 'docker buildx build' "$RUN_SCRIPT"
grep -Fq -- '--builder "$V2_DOCKER_BUILDER"' "$RUN_SCRIPT"
grep -Fq -- '--load' "$RUN_SCRIPT"
grep -Fq -- '--build-arg BUILD_PROFILE=release-perf' "$RUN_SCRIPT"
grep -Fq -- '--tag lantunnel:2.0-dev' "$RUN_SCRIPT"
grep -q -- '^USER lantunnel$' "$PRODUCTION_DOCKERFILE"
if grep -R -q -- 'Dockerfile.local-product' "$ROOT_DIR/tests/e2e/v2_docker"; then
  echo 'V2 acceptance still permits a product-image simulation' >&2
  exit 1
fi
grep -q -- 'wait-path.sh 1 2 P2p' "$RUN_SCRIPT"
grep -q -- 'SKIP_V2_HELPER_BUILD' "$RUN_SCRIPT"
grep -q -- 'compose up --no-build' "$RUN_SCRIPT"
grep -q -- 'docker network connect' "$RUN_SCRIPT"
grep -q -- 'docker network disconnect' "$RUN_SCRIPT"
grep -q -- 'wait-path.sh 1 2 Relay' "$RUN_SCRIPT"
grep -q -- 'V2_GATEWAY_TRANSPORT' "$RUN_SCRIPT"
grep -q -- 'websocket)' "$RUN_SCRIPT"
grep -q -- 'grpc)' "$RUN_SCRIPT"
grep -q -- '--gateway-transport "$V2_GATEWAY_TRANSPORT"' "$PROVISION_SCRIPT"
grep -q -- 'gateway-websocket.yaml' "$RUN_SCRIPT"
grep -q -- 'gateway-grpc.yaml' "$RUN_SCRIPT"
test -f "$ROOT_DIR/tests/e2e/v2_docker/gateway-websocket.yaml"
test -f "$ROOT_DIR/tests/e2e/v2_docker/gateway-grpc.yaml"

echo 'v2 Docker three-Peer loopback topology: PASS'
