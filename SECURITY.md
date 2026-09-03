# Security Policy

## Supported versions

Lantunnel 2.x is a V2-only line. Only the latest `2.0.x` release receives
security fixes. The 1.x line and any V1 profile, binary, or mixed-version
deployment are out of support and will not be patched.

| Version | Supported |
|---|---|
| 2.0.x (latest) | Yes |
| 2.0.x (older patch) | Upgrade first |
| 1.x | No |

## Reporting a vulnerability

Report privately. Do not open a public issue, pull request, or discussion for a
suspected vulnerability.

Use GitHub private vulnerability reporting:
<https://github.com/lantunnel/lantunnel/security/advisories/new>

Please include:

- affected component (`lantunnel-client`, `lantunnel-gateway`,
  `lantunnel-admin`, the mobile apps, or a `tp-*` crate) and version
- the deployment mode involved (Static profile, Managed profile, self-hosted
  Gateway, Direct PeerLink, Encrypted Relay)
- reproduction steps, a proof of concept, or a failing test
- the impact you believe it has

You should get an acknowledgement within 7 days. This is a small project; there
is no paid bug bounty.

Please give a reasonable disclosure window — 90 days is the default — before
publishing. Credit is given in the release notes unless you ask otherwise.

## Scope

In scope:

- the three published binaries and the crates in `crates/`
- the Gateway admission, Scope, and Relay data path
- PeerLink establishment, certificate pinning, and the relay AEAD
- profile handling (`.tunnel`, `.scope`, `.peer`) and secret material at rest
- the Android and iOS apps in `apps/`
- the release and signing pipeline in `.github/workflows/`

Out of scope:

- the hosted Lantunnel Platform service, which is closed source and not part of
  this repository — report those to the same address, but note that this
  repository cannot carry the fix
- findings that require an attacker who already has root/Administrator on the
  Peer machine, or possession of a `.tunnel` owner file or Peer private key
- denial of service through raw resource exhaustion of a self-hosted Gateway
  that the operator has not rate-limited
- vulnerabilities in third-party dependencies that already have a published
  advisory and no Lantunnel-specific exploitation path — open a normal issue so
  the dependency can be bumped

## Security model in brief

- A Peer's private key never leaves the machine that generated it, and never
  reaches a Gateway Operator or the Platform.
- Gateway Operators receive only public `.scope` material. They can see relay
  traffic volume and routing metadata, not Peer-to-Peer plaintext.
- Encrypted Relay payloads are sealed end-to-end with XChaCha20-Poly1305 under
  keys derived from an X25519 PeerLink exchange, so a Gateway relays ciphertext
  it cannot read.
- Direct PeerLinks verify the exact SHA-256 of the peer's end-entity
  certificate, pinned from signaling. There is no CA path for PeerLinks.
- Profiles are signed with Ed25519 and verified against the Tunnel's signing
  public key before use.
- The local SOCKS5 surface has no shared secret and is therefore restricted to
  a loopback listener.

See `docs/PROTOCOL.md` and `CONTEXT.md` for the full contract.
