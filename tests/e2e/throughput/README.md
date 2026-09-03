# V2 Throughput Probe

`tp-e2e-p3` supplies the TCP and UDP workloads used by current Lantunnel 2.0
Docker and real-machine harnesses. It connects to an already-running Client
and does not launch compatibility binaries.

```bash
cargo build --release -p tp-e2e-p3-throughput
./target/release/tp-e2e-p3 --test tcp_large_download \
  --proxy 127.0.0.1:1080 \
  --tcp-target 198.18.0.2:18998
```

The probe exposes no credential or implementation-matrix flags.
