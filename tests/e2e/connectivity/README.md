# V2 Connectivity Probe

`tp-e2e-p1` is the traffic probe used by the current the `v2_docker` harness. It connects to an already
running, loopback-only Lantunnel 2.0 Client SOCKS5 listener; it does not start a
Gateway or Client.

```bash
cargo build --release -p tp-e2e-p1-connectivity -p tp-e2e-echo-services
./target/release/tp-e2e-p1 --test socks5_tcp_connect
./target/release/tp-e2e-p1 --test socks5_udp_associate
```

The retained test names are exactly the TCP and UDP probes used by those V2
harnesses. The probe has no role, implementation-matrix, or credential flags;
it always uses the Client's loopback-only SOCKS5 NO AUTH listener.
