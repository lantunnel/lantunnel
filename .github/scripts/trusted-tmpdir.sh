#!/usr/bin/env bash
# The Gateway refuses key and Scope paths whose ancestors are group- or
# world-writable, and Linux /tmp is world-writable, so every Gateway test that
# builds its fixture under the default temporary directory fails there. macOS
# already hands each user a private temporary directory; give Linux the same
# guarantee instead of relaxing the check.
set -euo pipefail

trusted="${RUNNER_TEMP:?RUNNER_TEMP is required}/lantunnel-trusted-tmp"
mkdir -p "$trusted"
chmod 700 "$trusted"

# Fail before the tests do if any ancestor is still writable by someone else.
probe="$trusted"
while :; do
  mode="$(stat -c '%a' "$probe")"
  if [ $((0"$mode" & 0022)) -ne 0 ]; then
    echo "untrusted ancestor for TMPDIR: $probe has mode $mode" >&2
    exit 1
  fi
  [ "$probe" != "/" ] || break
  probe="$(dirname "$probe")"
done

echo "TMPDIR=$trusted" >>"$GITHUB_ENV"
echo "trusted TMPDIR: $trusted"
