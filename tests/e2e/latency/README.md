# V2 Latency Probe

`tp-e2e-p2` is a workload generator for an already-running Lantunnel 2.0
Client. It always uses the Client's loopback-only SOCKS5 NO AUTH listener.

```bash
cargo build --release -p tp-e2e-p2-latency
./target/release/tp-e2e-p2 --test latency_stress_curve \
  --proxy 127.0.0.1:1080 \
  --udp-target 198.18.0.2:18997 --packet-bytes 1385
```

The removed cold-start case depended on the retired mixed-client E2E
orchestrator.
