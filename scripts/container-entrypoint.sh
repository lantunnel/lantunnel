#!/bin/sh
set -eu

if command -v tini >/dev/null 2>&1; then
    exec tini -- "$@"
fi

exec "$@"
