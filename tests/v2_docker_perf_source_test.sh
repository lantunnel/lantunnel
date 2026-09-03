#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PERF="$ROOT_DIR/tests/e2e/v2_docker/perf.sh"
HELPER="$ROOT_DIR/tests/e2e/v2_docker/Dockerfile"
SETTINGS="$ROOT_DIR/tests/e2e/v2_docker/settings.json"

bash -n "$PERF"
grep -q -- 'client1 client2 client3' "$PERF"
grep -q -- 'wait-path.sh 1 2 Relay' "$PERF"
grep -q -- 'tcp_large_download' "$PERF"
grep -q -- 'V2_PERF_SINGLE_ITERATIONS' "$PERF"
grep -q -- 'udp_stress_multi_stream' "$PERF"
grep -q -- '--streams 30' "$PERF"
grep -q -- 'latency_stress_curve' "$PERF"
grep -q -- '--packet-bytes 1385' "$PERF"
grep -q -- 'docker stats --no-stream' "$PERF"
grep -q -- 'formal_al_wsl_pc_gate: "deferred_pc_unavailable"' "$PERF"
grep -q -- 'tp-e2e-p2' "$HELPER"
grep -q -- 'tp-e2e-p3' "$HELPER"
jq -e '
  any(.local_service_exports[]; .protocol == "tcp" and .ingress_port == 18998)
  and any(.client_access.allow[];
    .protocol == "tcp" and .port.type == "exact" and .port.value == 18998)
' "$SETTINGS" >/dev/null

echo 'v2 Docker performance harness source contract: PASS'
