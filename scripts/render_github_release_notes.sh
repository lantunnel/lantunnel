#!/usr/bin/env bash
# Render the deterministic, user-facing body for one stable GitHub Release.

set -euo pipefail

if [ "$#" -ne 5 ]; then
    echo "Usage: $0 <vX.Y.Z-tag> <source-commit> <owner/repository> <release-dir> <output-file>" >&2
    exit 1
fi

tag="$1"
source_commit="$2"
repository="$3"
release_dir="$4"
output_file="$5"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "Error: GitHub release tag must be stable SemVer with a v prefix: ${tag}" >&2
    exit 1
fi
version="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"
if [[ ! "$source_commit" =~ ^[0-9a-f]{40}$ ]]; then
    echo "Error: source commit must be an exact lowercase 40-hex commit" >&2
    exit 1
fi
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
    echo "Error: repository must use the owner/name form: ${repository}" >&2
    exit 1
fi
if [ ! -d "$(dirname "$output_file")" ]; then
    echo "Error: release-note output directory not found: $(dirname "$output_file")" >&2
    exit 1
fi

"$root_dir/scripts/verify_release_bundle.sh" "$version" "$release_dir" >/dev/null

temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/lantunnel-release-notes.XXXXXX")"
trap 'rm -rf "$temporary_dir"' EXIT
changes_file="$temporary_dir/current-changes.md"
rendered_file="$temporary_dir/release-notes.md"
final_file="$temporary_dir/final-release-notes.md"
heading="## [${version}]"
awk -v heading="$heading" '
    index($0, heading) == 1 &&
      (length($0) == length(heading) || substr($0, length(heading) + 1, 1) == " ") {
        in_section = 1
        next
      }
    in_section && /^## / { exit }
    in_section { print }
' "$release_dir/CHANGELOG.md" > "$changes_file"
if ! grep -q '[^[:space:]]' "$changes_file"; then
    echo "Error: changelog section for ${version} is empty" >&2
    exit 1
fi

release_base="https://github.com/${repository}/releases/download/${tag}"

{
    printf '# Lantunnel %s\n\n' "$tag"
    printf 'Built from [`%s`](https://github.com/%s/commit/%s). ' \
        "$source_commit" "$repository" "$source_commit"
    printf 'The attached packages and checksum manifest are the exact files accepted by the release workflow.\n\n'

    printf '## Choose a download\n\n'
    printf '### Client — connect this device\n\n'
    printf 'Client is the right download for most people. Use the same Client program on every Peer, with the default desktop UI or `--headless`.\n\n'
    printf '| Download | Platform | Trust note |\n'
    printf '| --- | --- | --- |\n'
    printf '| [Windows x64](%s/lantunnel-client-%s-windows-amd64.exe) | Windows 10 or later | Intentionally unsigned preview |\n' "$release_base" "$version"
    printf '| [macOS Intel](%s/lantunnel-client-%s-macos-amd64.dmg) | Intel Mac | Signed, notarized, and stapled |\n' "$release_base" "$version"
    printf '| [macOS Apple Silicon](%s/lantunnel-client-%s-macos-arm64.dmg) | Apple Silicon Mac | Signed, notarized, and stapled |\n' "$release_base" "$version"
    printf '| [Linux x64](%s/lantunnel-client-%s-linux-amd64.AppImage) | 64-bit Intel/AMD Linux | Verify SHA-256 |\n' "$release_base" "$version"
    printf '| [Linux ARM64](%s/lantunnel-client-%s-linux-arm64.AppImage) | 64-bit ARM Linux | Verify SHA-256 |\n\n' "$release_base" "$version"

    printf '### Gateway — relay and coordinate a Tunnel\n\n'
    printf 'Install Gateway only when you operate an independent or Platform-connected Gateway host.\n\n'
    printf '| Download | Platform |\n'
    printf '| --- | --- |\n'
    printf '| [macOS Apple Silicon](%s/lantunnel-gateway-%s-aarch64-apple-darwin) | Apple Silicon Mac |\n' "$release_base" "$version"
    printf '| [Linux x64](%s/lantunnel-gateway-%s-x86_64-unknown-linux-musl) | 64-bit Intel/AMD Linux |\n\n' "$release_base" "$version"

    printf '### Admin — create independent Tunnel files\n\n'
    printf 'Admin is only for offline provisioning with an independent Gateway; Connected Gateway and Lantunnel Gateway modes do not use it.\n\n'
    printf '| Download | Platform |\n'
    printf '| --- | --- |\n'
    printf '| [macOS Apple Silicon](%s/lantunnel-admin-%s-aarch64-apple-darwin) | Apple Silicon Mac |\n' "$release_base" "$version"
    printf '| [Linux x64](%s/lantunnel-admin-%s-x86_64-unknown-linux-musl) | 64-bit Intel/AMD Linux |\n\n' "$release_base" "$version"

    cat <<'MARKDOWN'
## Trust and signatures

- macOS Client DMGs are Developer ID signed, notarized, and stapled.
- The Windows Client executable is an intentionally unsigned preview; Windows may show **Unknown publisher**. Verify its SHA-256 before running it.
- Gateway and Admin command-line binaries and Linux AppImages are not code-signed. Verify their SHA-256 before running them.
- macOS Gateway and Admin CLI binaries are unsigned and not notarized. If Gatekeeper or an organization policy blocks one, [build it from source](SOURCE_BUILD_URL). Do not bypass Gatekeeper or an organization policy.

## Install

### Client

- **Windows:** download the `.exe`, verify it, and run it. Review the Windows security prompt before continuing.
- **macOS:** open the `.dmg`, drag Lantunnel Client to Applications, then open it from Applications.
- **Linux:** make the AppImage executable with `chmod +x <downloaded.AppImage>`, then run it.

### Gateway and Admin

Verify the downloaded CLI, make it executable, rename it to `lantunnel-gateway` or `lantunnel-admin`, and place it on the appropriate machine's `PATH`.

## System requirements

- Windows 10 or later on x86-64.
- macOS 10.15 Catalina or later on Intel.
- macOS 11 Big Sur or later on Apple Silicon.
- 64-bit Linux on x86-64 or ARM64; the desktop Client requires GTK 3 and WebKitGTK 4.1.
- Gateway and Admin packages are available for Apple Silicon macOS and x86-64 Linux.

## Verify SHA-256

Download your chosen file and [`checksums.txt`](CHECKSUM_URL). Set `FILE` to the downloaded filename, then verify only its matching entry.

Linux:

```sh
FILE=lantunnel-client-VERSION-linux-amd64.AppImage
grep "  ${FILE}$" checksums.txt | sha256sum --check --strict -
```

macOS:

```sh
FILE=lantunnel-client-VERSION-macos-arm64.dmg
grep "  ${FILE}$" checksums.txt | shasum -a 256 --check -
```

Windows PowerShell:

```powershell
Get-FileHash .\lantunnel-client-VERSION-windows-amd64.exe -Algorithm SHA256
Select-String -Path .\checksums.txt -Pattern 'lantunnel-client-VERSION-windows-amd64.exe$'
```

The two displayed SHA-256 values must match. The complete accepted changelog is also attached as [`CHANGELOG.md`](CHANGELOG_URL).

## Choose an installation mode

1. [My Gateway](https://lantunnel.app/docs/installation#own-independent) — deploy your own independent Gateway; no lantunnel.app account is involved.
2. [Friend's Gateway](https://lantunnel.app/docs/installation#friend-independent) — own your Tunnel while a friend operates the independent Gateway.
3. [Connected Gateway](https://lantunnel.app/docs/installation#platform-connected) — connect your own or a friend's Gateway to the lantunnel.app Platform.
4. [Lantunnel Gateway](https://lantunnel.app/docs/installation#lantunnel-provided) — install only Client while Lantunnel operates the Gateway.

## Lantunnel Gateway quick start

1. [Create your account](https://lantunnel.app/register).
2. Create a Tunnel in the Dashboard and keep its Tunnel ID.
3. Create a separate Peer for every device, then download that device's private `.peer` profile. Never share or reuse a Peer profile.
4. Install Client on each device, import its own profile, and connect with its Tunnel ID.
5. Keep Client running. It prefers a Direct path and falls back to Encrypted Relay when Direct connectivity is unavailable.

See the [full Lantunnel Gateway quick start](https://lantunnel.app/docs/quickstart) for the complete walkthrough.
MARKDOWN
} > "$rendered_file"

# Replace only renderer-owned placeholders. Changelog content is appended
# afterwards and is never rewritten.
sed \
    -e "s#CHECKSUM_URL#${release_base}/checksums.txt#g" \
    -e "s#CHANGELOG_URL#${release_base}/CHANGELOG.md#g" \
    -e "s|SOURCE_BUILD_URL|https://github.com/${repository}#building-from-source|g" \
    -e "s#VERSION#${version}#g" \
    "$rendered_file" > "$final_file"
{
    printf '\n## What changed in %s\n' "$version"
    cat "$changes_file"
    printf '\n'
} >> "$final_file"
mv "$final_file" "$output_file"

echo "Rendered GitHub release notes for ${tag}: ${output_file}"
