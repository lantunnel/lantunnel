# Lantunnel 2.0 local Docker acceptance

This topology builds `lantunnel:2.0-dev` from the repository's production
`Dockerfile` with the `release-perf` profile, then consumes that exact runtime
image. Run:

```sh
./tests/e2e/v2_docker/run.sh
```

If the daemon's default BuildKit session is unhealthy, select another local
builder without changing the image contract, for example
`V2_DOCKER_BUILDER=multiplatform V2_DOCKER_PLATFORM=linux/arm64 ./tests/e2e/v2_docker/run.sh`.
Both paths rebuild the same root `Dockerfile` with `release-perf`.

The runner creates one static Tunnel, one `scopes.d` entry, and three distinct
Peer profiles. Each unified `lantunnel-client` runs `connect <Tunnel ID>` with
its own config directory and a loopback-only SOCKS listener. It verifies TCP
and UDP Overlay access in two network generations:

The `mapping` service is the sole UDP/8444 owner. The `gateway` service shares
that container's network namespace, probes the reflector before binding its
data plane, and never opens a second mapping socket.

Before the first import, provisioning rejects duplicate absolute paths, file
identities, public Peer IDs, or Overlay IPs. One `.peer` is never reused by two
Client containers.

1. all Clients share the Gateway mesh and must select Direct (`P2p` internally);
2. each live Client is moved onto an isolated Gateway network and must select
   end-to-end encrypted Relay.

QUIC remains the default. The same three-Peer acceptance can exercise V2 Auth
and encrypted framed Relay over either stream carrier without changing product
code:

```sh
V2_GATEWAY_TRANSPORT=websocket ./tests/e2e/v2_docker/run.sh
V2_GATEWAY_TRANSPORT=grpc ./tests/e2e/v2_docker/run.sh
```

Direct remains end-to-end QUIC in all three runs; the selected value controls
the Gateway attachment and therefore the fallback Relay carrier.

The Gateway certificate, Scope, Peer files, QUIC port, mapping port, and local
proxy configuration do not change between generations. Set `KEEP_V2_DOCKER=1`
to retain failed containers and volumes for inspection; otherwise the runner
prints diagnostics and cleans up its named Compose project.

After a successful retained Relay generation, `perf.sh <absolute-output-dir>`
runs five sequential one-Flow samples, one 30-Flow sample, and a 1385-byte UDP
rate sweep while sampling Docker CPU/memory. It is a Mac-local regression trend,
not the required AL+wsl+PC hardware sign-off.
