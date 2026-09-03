#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# V2 has no mutable V1 deployment, certificate, restart-helper, or
# tunnel-key smoke authority. Frozen plans/reports may still name these paths
# as historical provenance, but no executable artifact may remain.
for retired_path in \
  scripts/deploy-gateway-release.sh \
  scripts/configure-gateway-platform-auth.sh \
  scripts/renew-al-gateway-cert.sh \
  scripts/setup_letsencrypt.sh \
  scripts/check_gateway_connect.sh \
  scripts/p2p_three_host_smoke.sh \
  scripts/p2p_three_host_perf.sh \
  scripts/run-go-test.sh \
  scripts/peer_mesh_three_peer_acceptance.sh \
  scripts/tests/peer_mesh_three_peer_acceptance_static.sh \
  scripts/remote/peer-mesh-node.sh \
  scripts/remote/peer-mesh-probe.py \
  scripts/remote/peer-mesh-relations.py \
  scripts/remote/peer-mesh-lan-ip.py \
  scripts/remote/peer-mesh-tun-route.py \
  scripts/remote/peer-mesh-underlay.py \
  scripts/pc_multi_endpoint_container_smoke.sh \
  tests/e2e/v2_al/migrate-gateway.sh \
  tests/e2e/v2_al/al-migrate-ops.sh \
  tests/e2e/v2_al/al-ops.sh \
  tests/e2e/v2_al/al-pairing.sh \
  tests/e2e/v2_al/al-update-ops.sh \
  tests/e2e/v2_al/cleanup.sh \
  tests/e2e/v2_al/common.sh \
  tests/e2e/v2_al/cross-scope.sh \
  tests/e2e/v2_al/dry-run.sh \
  tests/e2e/v2_al/fleet-live-operator.sh \
  tests/e2e/v2_al/four-gateway-ingress.sh \
  tests/e2e/v2_al/four-gateway-sni-passthrough.py \
  tests/e2e/v2_al/four-gateway.sh \
  tests/e2e/v2_al/platform.sh \
  tests/e2e/v2_al/preflight.sh \
  tests/e2e/v2_al/provision.sh \
  tests/e2e/v2_al/run.sh \
  tests/e2e/v2_al/traffic.sh \
  tests/e2e/v2_al/update-gateway.sh \
  scripts/remote/lantunnel-gateway-fleet.yaml \
  tests/v2_al_migration_source_test.sh \
  tests/v2_al_migration_behavior_test.sh \
  .github/workflows/e2e.yml \
  docs/SAAS_ARCHITECTURE.md \
  docs/ARCHITECTURE.md \
  docs/e2e-pr-body.md \
  docs/rfcs/protocol-v2-rust-go-mixed-plan.md \
  docs/rfcs/transport-mux-v2.md \
  docs/design \
  docs/runbooks \
  docs/p2p-public-validation \
  docs/e2e-test-matrix.md \
  docs/link-heartbeat-watchdog.md \
  docs/p2p-lan-candidate-policy.md \
  docs/p2p-relay-fallback.md \
  docs/runtime-timeout-ttl-policy.md \
  docs/RUST_QUIC_DATAGRAM_ROUTING.md \
  tests/e2e/v2_al \
  tests/e2e/v2_native_lan \
  tests/e2e/v2_multi_host \
  scripts/remote/peer-mesh-firewall-linux.sh \
  scripts/remote/peer-mesh-firewall-macos.sh \
  scripts/install-peer-mesh-firewall-helpers.sh \
  scripts/asc-release.py \
  scripts/ios-app-store.sh \
  tests/e2e/run_all.sh \
  docs/p2p-public-validation/lan-p2p-performance-baseline.md \
  docs/p2p-public-validation/2026-05-13-wsl-pc-lan-p2p-performance.md \
  docs/p2p-public-validation/commands.md \
  docs/p2p-public-validation/next-session-prompt.md \
  docs/p2p-public-validation/p2p-srflx-fix-plan.md \
  docs/p2p-public-validation/sunshine-lan-loss-investigation.md \
  docs/p2p-public-validation/scripts/udp_candidate_port_probe.py \
  docs/p2p-public-validation/scripts/udp_cross_nat_probe.py \
  docs/p2p-public-validation/scripts/udp_nat_hairpin_probe.py \
  docs/p2p-public-validation/scripts/udp_reflector.py \
  scripts/remote/anyproxy-gateway.service \
  scripts/remote/anyproxy-gateway-restart.sh \
  scripts/remote/tunnel-proxy-app-restart.sh \
  scripts/remote/tunnel-proxy-client-restart.sh \
  ops/gateways.csv \
  tests/gateway_cert_renewal_source_test.sh
do
  if [[ -e "$ROOT_DIR/$retired_path" || -L "$ROOT_DIR/$retired_path" ]]; then
    echo "retired V1 operator artifact still exists: $retired_path" >&2
    exit 1
  fi
done

# Current repository/operator authority must not direct a future operator back
# to a deleted helper or removed Platform/Gateway endpoint. Frozen plans and
# historical reports are deliberately outside this set.
current_authority=(
  "$ROOT_DIR/AGENTS.md"
  "$ROOT_DIR/CLAUDE.md"
  "$ROOT_DIR/CONTEXT.md"
  "$ROOT_DIR/docs/PROTOCOL.md"
  "$ROOT_DIR/Makefile"
  "$ROOT_DIR/README.md"
  "$ROOT_DIR/Dockerfile"
  "$ROOT_DIR/tests/v2_release_packaging_source_test.sh"
)

while IFS= read -r current_workflow; do
  current_authority+=("$current_workflow")
done < <(find "$ROOT_DIR/.github/workflows" -maxdepth 1 -type f \
  \( -name '*.yml' -o -name '*.yaml' \) -print | sort)

while IFS= read -r current_script; do
  current_authority+=("$current_script")
done < <(find "$ROOT_DIR/scripts" -maxdepth 1 -type f -name '*.sh' -print | sort)

for retired_reference in \
  deploy-gateway-release.sh \
  configure-gateway-platform-auth.sh \
  renew-al-gateway-cert.sh \
  setup_letsencrypt.sh \
  check_gateway_connect.sh \
  p2p_three_host_smoke.sh \
  p2p_three_host_perf.sh \
  run-go-test.sh \
  peer_mesh_three_peer_acceptance.sh \
  peer_mesh_three_peer_acceptance_static.sh \
  peer-mesh-node.sh \
  peer-mesh-probe.py \
  peer-mesh-relations.py \
  peer-mesh-lan-ip.py \
  peer-mesh-tun-route.py \
  peer-mesh-underlay.py \
  pc_multi_endpoint_container_smoke.sh \
  migrate-gateway.sh \
  al-migrate-ops.sh \
  al-ops.sh \
  al-pairing.sh \
  al-update-ops.sh \
  fleet-live-operator.sh \
  four-gateway.sh \
  four-gateway-ingress.sh \
  four-gateway-sni-passthrough.py \
  update-gateway.sh \
  v2_al_migration_source_test.sh \
  v2_al_migration_behavior_test.sh \
  gateway_cert_renewal_source_test.sh \
  anyproxy-gateway-restart.sh \
  tunnel-proxy-app-restart.sh \
  tunnel-proxy-client-restart.sh \
  lan-p2p-performance-baseline.md \
  ops/gateways.csv \
  scripts/remote/anyproxy-gateway.service \
  /api/tunnel/config \
  /api/session/heartbeat \
  /api/session/disconnect \
  /api/credentials/register \
  /api/credentials/delete
do
  if grep -Fq -- "$retired_reference" "${current_authority[@]}"; then
    echo "current operator authority still references V1 surface: $retired_reference" >&2
    exit 1
  fi
done

for retired_make_target in test-integration bench-stress; do
  if grep -Eq "^${retired_make_target}:" "$ROOT_DIR/Makefile"; then
    echo "current Makefile still exposes V1 compatibility target: $retired_make_target" >&2
    exit 1
  fi
done

# A workflow step that invokes a deleted script fails only when CI runs, which
# is exactly how a `bash <deleted-contract>` line survived a cleanup pass.
while IFS= read -r invoked; do
  if [[ ! -f "$ROOT_DIR/$invoked" ]]; then
    echo "a workflow invokes a script that does not exist: $invoked" >&2
    exit 1
  fi
done < <(grep -rhoE '(bash |\./)(tests|scripts|\.github)/[A-Za-z0-9_./-]+\.(sh|py)' \
  "$ROOT_DIR/.github/workflows" | sed -e 's/^bash //' -e 's|^\./||' | sort -u)

echo 'V2-only operator authority source contract: PASS'
