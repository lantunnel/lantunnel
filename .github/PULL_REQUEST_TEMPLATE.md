<!--
Thanks for contributing. CONTRIBUTING.md has the build, test, and style
guidance; this template is just the summary a reviewer needs.
-->

## What changed and why

<!-- What problem does this solve? Prefer the smallest change that solves it. -->

## How it was tested

<!--
Commands you ran and what they showed. At minimum:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
-->

## Compatibility impact

- [ ] No protocol, profile, or settings-format change
- [ ] Changes the wire format — `docs/PROTOCOL.md` updated in this PR
- [ ] Changes a profile (`.tunnel` / `.scope` / `.peer`) format
- [ ] Changes saved Client settings, and existing files still load

## Checklist

- [ ] Tests cover the new behavior, and I saw them fail before the fix
- [ ] Docs updated where behavior changed (`README.md`, `docs/USAGE.md`, `CONTEXT.md`)
- [ ] `CHANGELOG.md` updated if the change is user-visible
- [ ] No secret material — keys, profile contents, or session keys — reaches a log line
- [ ] If a source-contract test in `tests/*.sh` names a file I moved, I updated it here
