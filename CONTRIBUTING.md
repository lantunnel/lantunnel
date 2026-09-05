# Contributing to Lantunnel

Thanks for your interest. This document covers what you need to build, test,
and land a change.

If you are here to *use* Lantunnel rather than change it, [`docs/USAGE.md`](./docs/USAGE.md)
is the guide you want. [`CONTEXT.md`](./CONTEXT.md) explains the architecture,
and [`docs/PROTOCOL.md`](./docs/PROTOCOL.md) is the wire format.

By participating you agree to abide by our
[Code of Conduct](./CODE_OF_CONDUCT.md).

## License

Lantunnel is licensed under the Apache License, Version 2.0. By submitting a
pull request you agree that your contribution is licensed under the same terms,
per section 5 of the license. There is no separate CLA.

New files do not require a license header. The `LICENSE` and `NOTICE` files at
the repository root cover the whole tree. If you vendor third-party source,
add it to `NOTICE` with its own license text in place.

## What ships

The repository builds exactly three public binaries plus two mobile apps:

| Component | Path |
|---|---|
| `lantunnel-gateway` | `apps/lantunnel-gateway` |
| `lantunnel-client` | `apps/lantunnel-client` |
| `lantunnel-admin` | `apps/lantunnel-admin` |
| Android app | `apps/android-proxy` |
| iOS app | `apps/ios-proxy` |

Shared code lives in `crates/`. `lantunnel-tun-helper` is an internal helper
binary, not a fourth product.

## Build

```bash
# Gateway and local provisioning tool
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin

# Client: build the frontend before checking the Tauri crate
npm --prefix apps/lantunnel-client/frontend ci
npm --prefix apps/lantunnel-client/frontend run build
cargo check -p lantunnel-client
```

The workspace MSRV is pinned in `Cargo.toml` (`rust-version`) and CI builds
both stable and that exact version. `protoc` is required for the gRPC
transport.

On Linux the Tauri Client links against webkit2gtk, appindicator, and rsvg, so
`cargo check`, `clippy`, and `test` all need those `-dev` packages installed —
see `.github/workflows/ci.yml` for the exact list.

## Test

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Some `crates/tp-client` P2P tests bind real sockets and can take a minute; run
a narrower `-p` target while iterating.

Beyond the Rust tests, `tests/*.sh` holds source-contract tests. These grep the
tree to enforce invariants that a unit test cannot express — cross-file log
policy, hot-path allocation guardrails, Windows-only code that will not compile
on a Linux runner, and V1-retirement checks. If you move or rename a file that
one of them names, update the test in the same commit. CI runs the wired
subset; `.github/workflows/ci.yml` and `release.yml` are the source of truth
for which.

End-to-end suites live under `tests/e2e/`. The Docker three-Peer acceptance is
the one that runs without special hardware:

```bash
tests/e2e/v2_docker/run.sh
```

## Release candidates and releases

The Release workflow has two deliberately separate entry points:

- A manual run on `main` takes the exact lowercase 40-character commit from
  `source_commit`, rebuilds the complete native matrix, and stores only a
  `manual-release-candidate-X.Y.Z` Actions artifact. It never creates or
  changes a GitHub Release.
- Creating and pushing a new tag whose name is exactly `vX.Y.Z` rebuilds the
  same matrix, verifies that the tag points into `main` and carries the current
  `.github/workflows` tree, and publishes those bytes as a GitHub Release. Tag
  updates and deletions do not publish.

GitHub Releases are the sole authoritative destination for new public releases.
Do not upload new release bundles to R2 or another parallel download store.
Every candidate is validated locally with
`scripts/verify_release_bundle.sh X.Y.Z dist/release`; the exact bundle is nine
native packages, `checksums.txt`, and `CHANGELOG.md`.

Before creating a release tag, set `[workspace.package].version` in
`Cargo.toml` to `X.Y.Z` and add the matching `## [X.Y.Z]` entry to
`CHANGELOG.md`. The tag, Cargo version, and changelog entry must agree. Do not
move or reuse a release tag after pushing it. A tag ruleset enforces this:
`v*` creation is restricted to release maintainers, and tag updates and
deletions are blocked outright. Lightweight and annotated tags are both
supported; annotated tags are recommended for an explicit release message.

The publisher creates a draft, uploads only missing assets, verifies every
remote byte, publishes the draft, and verifies it again. Immediately before
each asset upload and the final publication, it confirms that the remote tag
still resolves to the accepted source commit and that the same numeric release
record remains the expected draft. A rerun accepts an identical partial draft
or already-published release; unexpected metadata, assets, or bytes fail
without overwriting or deleting anything. Before publishing, the workflow
renders a deterministic Release body from the accepted tag, source commit,
bundle, and only that version's `CHANGELOG.md` section. It includes direct
package links, signature status, install and system requirements, checksum
verification, and links to all four installation modes. GitHub uses the
job-scoped `GITHUB_TOKEN`; no personal release token or storage credentials are
required.

The macOS matrix requires these repository secrets:
`MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_PASSWORD`,
`MACOS_CODESIGN_IDENTITY`, `ASC_KEY_ID`, `ASC_ISSUER_ID`, and
`ASC_PRIVATE_KEY_P8_BASE64`. The Windows installer remains the canonical
unsigned preview described by the existing packaging contract.

## Supply chain

`cargo deny check` gates advisories, licenses, sources, and bans, and runs on
every PR. A new transitive dependency with a license outside the allowlist in
`deny.toml` fails the build. Either add the license to the allowlist with a
short justification in the same commit, or pin/replace the dependency.

## Style

- Prefer the smallest change that solves the problem. Large refactors should be
  proposed in an issue first.
- Match the surrounding code. The tree is `rustfmt`-clean and
  `clippy -D warnings`-clean; keep it that way.
- Immutable data by default. Return new values rather than mutating in place.
- Handle errors explicitly. Do not silently swallow them, and do not log secret
  material — private keys, profile contents, or session keys must never reach a
  log line.
- Validate at boundaries. Anything arriving from the network, a profile file,
  or a CLI flag is untrusted until parsed and checked.

## Sending a pull request

`main` is protected. It takes no direct pushes, no force pushes, and cannot be
deleted; every change lands through a pull request whose required checks pass.
Work from a fork:

```bash
# 1. Fork on GitHub, then clone your fork
git clone git@github.com:<you>/lantunnel.git
cd lantunnel
git remote add upstream git@github.com:lantunnel/lantunnel.git

# 2. Branch from an up-to-date main
git fetch upstream
git switch -c fix/short-description upstream/main

# 3. Commit, then push to your fork
git push -u origin fix/short-description
```

Open the pull request against `lantunnel/lantunnel:main`. One topic per pull
request; unrelated fixes belong in separate ones. Update a branch under review
by pushing follow-up commits rather than force-pushing over what a reviewer is
reading — if you do need to rebase onto `upstream/main`, say so in a comment.

Commit messages follow Conventional Commits:

```
<type>: <description>

<optional body>
```

Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `ci`.
Scope the type when it helps, for example `fix(gateway):`.

A pull request should explain what changed and why, note any protocol or
profile-compatibility impact, and list how you tested it. Protocol changes need
a matching update to `docs/PROTOCOL.md`.

### What happens after you open it

CI runs on every pull request. Six checks are required before `main` will accept
the merge:

| Required check | What it gates |
|---|---|
| `fmt + clippy + test (stable)` | `rustfmt`, `clippy -D warnings`, workspace tests, the wired source contracts |
| `fmt + clippy + test (1.89)` | The same suite at the pinned MSRV |
| `Android unit tests` | `apps/android-proxy` |
| `iOS unit tests` | `apps/ios-proxy` |
| `Windows secret ACL behavior` | Real DACL and reparse-point behavior on a Windows runner |
| `cargo-deny` | Advisories, licenses, sources, bans |

`coverage warning (crates)` and `coverage warning (apps)` also run. They are
advisory and never block a merge.

A first pull request from a new contributor waits on a maintainer to approve the
workflow run, so the checks may take a while to appear. Every review
conversation must be resolved before merging, and a maintainer does the merge.

## The desktop bundle identifier

`apps/lantunnel-client/src-tauri/tauri.conf.json` sets `identifier` to
`com.buhuipao.tunnel-proxy-app`, which carries the project's former name. It is
deliberately frozen: it is the macOS bundle id, the Windows installer registry
key, and the Linux `.desktop` id of every existing install. Changing it would
not rename the app, it would ship a second unrelated one, and nobody would
upgrade in place. Leave it alone.

## Reporting security issues

Do not open a public issue. See [SECURITY.md](./SECURITY.md).
