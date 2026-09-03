#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/compose.yaml"
PROJECT=lantunnel-v2-local

out_dir="${1:?usage: $0 <absolute-output-directory>}"
[[ "$out_dir" == /* ]] || { echo "output directory must be absolute" >&2; exit 2; }
[[ ! -L "$out_dir" ]] || { echo "output directory must not be a symlink" >&2; exit 2; }
mkdir -p "$out_dir"

transport="${V2_GATEWAY_TRANSPORT:-quic}"
case "$transport" in
  quic) V2_GATEWAY_CONFIG=/accept/gateway.yaml ;;
  websocket) V2_GATEWAY_CONFIG=/accept/gateway-websocket.yaml ;;
  grpc) V2_GATEWAY_CONFIG=/accept/gateway-grpc.yaml ;;
  *) echo "V2_GATEWAY_TRANSPORT must be quic, websocket, or grpc" >&2; exit 2 ;;
esac
export V2_GATEWAY_TRANSPORT="$transport" V2_GATEWAY_CONFIG

compose() {
  docker compose -f "$COMPOSE_FILE" "$@"
}

for service in mapping gateway client1 client2 client3 echo1 echo2 echo3; do
  [[ -n "$(compose ps -q "$service")" ]] \
    || { echo "retained V2 Docker service is not running: $service" >&2; exit 1; }
done

gateway_id="$(compose ps -q gateway)"
gateway_cmd="$(docker inspect --format '{{json .Config.Cmd}}' "$gateway_id")"
[[ "$gateway_cmd" == *"$V2_GATEWAY_CONFIG"* ]] \
  || { echo "retained Gateway transport does not match V2_GATEWAY_TRANSPORT=$transport" >&2; exit 1; }

# A successful KEEP_V2_DOCKER=1 acceptance ends in the isolated generation.
# Re-probe it instead of assuming that container presence implies Relay.
compose exec -T echo1 /bin/sh /accept/wait-path.sh 1 2 Relay
compose exec -T echo2 /bin/sh /accept/wait-path.sh 2 3 Relay
compose exec -T echo3 /bin/sh /accept/wait-path.sh 3 1 Relay

target_ip="$(compose exec -T echo1 sh -c \
  "sed -n 's/^[[:space:]]*overlay_ip:[[:space:]]*//p' /state/peers/peer2.peer | sed -n '1p'" \
  | tr -d '\r')"
[[ "$target_ip" == 198.18.* ]] || { echo "invalid target Overlay IP" >&2; exit 1; }

run_id="$(date -u +%Y%m%dT%H%M%SZ)-${transport}"
single_bytes="${V2_PERF_SINGLE_BYTES:-268435456}"
single_iterations="${V2_PERF_SINGLE_ITERATIONS:-5}"
flow_bytes="${V2_PERF_FLOW_BYTES:-33554432}"
moonlight_steps="${V2_PERF_MOONLIGHT_STEPS:-20,40,100}"
moonlight_seconds="${V2_PERF_MOONLIGHT_SECONDS:-10}"
for value in "$single_bytes" "$single_iterations" "$flow_bytes" "$moonlight_seconds"; do
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || { echo "performance sizes/duration must be positive integers" >&2; exit 2; }
done

service_ids=()
for service in mapping gateway client1 client2 client3; do
  service_ids+=("$(compose ps -q "$service")")
done

sample_stats() {
  local workload_pid="$1"
  local output="$2"
  while kill -0 "$workload_pid" 2>/dev/null; do
    local sampled_at
    sampled_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    local id
    for id in "${service_ids[@]}"; do
      docker stats --no-stream --format '{{json .}}' "$id" \
        | jq -c --arg sampled_at "$sampled_at" --arg transport "$transport" \
          '. + {sampled_at: $sampled_at, gateway_transport: $transport}' >>"$output"
    done
    sleep 1
  done
}

run_case() {
  local label="$1"
  shift
  local remote_report="/tmp/lantunnel-v2-${run_id}-${label}.json"
  local log="$out_dir/${label}.log"
  local stats="$out_dir/${label}-docker-stats.jsonl"
  : >"$stats"
  compose exec -T echo1 "$@" --out "$remote_report" >"$log" 2>&1 &
  local workload_pid=$!
  sample_stats "$workload_pid" "$stats" &
  local stats_pid=$!
  local status=0
  wait "$workload_pid" || status=$?
  wait "$stats_pid" 2>/dev/null || true
  if ((status != 0)); then
    echo "$label failed; see $log" >&2
    return "$status"
  fi
  docker cp "$(compose ps -q echo1):$remote_report" "$out_dir/${label}.json" >/dev/null
}

for iteration in $(seq 1 "$single_iterations"); do
  printf -v label 'tcp-single-flow-%02d' "$iteration"
  run_case "$label" \
    tp-e2e-p3 --test tcp_large_download \
    --proxy 127.0.0.1:1080 \
    --tcp-target "$target_ip:18998" --bytes "$single_bytes" --min-mbps 0
done

run_case tcp-30-flows \
  tp-e2e-p3 --test udp_stress_multi_stream \
  --proxy 127.0.0.1:1080 \
  --tcp-target "$target_ip:18998" --streams 30 --bytes-per-stream "$flow_bytes"

run_case moonlight-udp-1385 \
  tp-e2e-p2 --test latency_stress_curve \
  --proxy 127.0.0.1:1080 \
  --udp-target "$target_ip:18997" --packet-bytes 1385 \
  --steps "$moonlight_steps" --step-duration "$moonlight_seconds"

for service in gateway client1 client2 client3; do
  compose logs --no-color "$service" >"$out_dir/${service}.log" 2>&1
done

jq -n \
  --arg run_id "$run_id" \
  --arg transport "$transport" \
  --arg target_ip "$target_ip" \
  --arg product_image "$(docker image inspect lantunnel:2.0-dev --format '{{.Id}}')" \
  --arg helper_image "$(docker image inspect lantunnel-v2-acceptance-helper:local --format '{{.Id}}')" \
  --argjson single_bytes "$single_bytes" \
  --argjson single_iterations "$single_iterations" \
  --argjson flow_count 30 \
  --argjson bytes_per_flow "$flow_bytes" \
  --arg moonlight_steps "$moonlight_steps" \
  --argjson moonlight_seconds "$moonlight_seconds" \
  '{
    run_id: $run_id,
    evidence_class: "local_mac_docker_trend_only",
    formal_al_wsl_pc_gate: "deferred_pc_unavailable",
    gateway_transport: $transport,
    peers_running: 3,
    expected_path: "Encrypted Relay",
    target_overlay_ip: $target_ip,
    product_image: $product_image,
    helper_image: $helper_image,
    workloads: {
      tcp_single_flow: {bytes: $single_bytes, sequential_iterations: $single_iterations},
      tcp_30_flows: {flows: $flow_count, bytes_per_flow: $bytes_per_flow},
      moonlight_udp: {payload_bytes: 1385, steps_mbps: $moonlight_steps, seconds_per_step: $moonlight_seconds}
    }
  }' >"$out_dir/manifest.json"

echo "Local Mac Docker V2 performance evidence (not PC hardware sign-off): $out_dir"
