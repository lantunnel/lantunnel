#!/usr/bin/env bash
# Linking the workspace test binaries exhausts the stock runner disk, so drop
# the preinstalled toolchains the Rust job does not need. The Android SDK is
# among them: the Android suite runs in its own job, on a runner where this
# script is deliberately not used.
set -euo pipefail

sudo rm -rf \
  /usr/share/dotnet \
  /usr/local/lib/android \
  /opt/ghc \
  /usr/local/share/boost \
  /usr/local/share/powershell \
  /usr/local/lib/node_modules \
  /opt/hostedtoolcache/CodeQL

df -h /
