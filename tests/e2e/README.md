# Lantunnel 2.0 E2E

Current acceptance entry points are V2-only:

- `v2_docker/` — isolated local acceptance and bounded performance trends.
  It is the one suite that runs anywhere Docker does.

The `connectivity`, `latency`, `throughput`, and `_fixtures/echo-services`
Cargo packages are stateless traffic generators it drives. They do not start a
Gateway or Client themselves.

Start with the mode-specific README and use its dry-run/preflight command before
the real-machine run.
