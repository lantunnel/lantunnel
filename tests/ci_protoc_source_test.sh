#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="$ROOT_DIR/.github/workflows/ci.yml"
CARGO_COMPILE_RE='^[[:space:]]+(run:[[:space:]]+)?cargo (build|check|clippy|test|bench|llvm-cov)([[:space:]]|$)'
# A job can also compile indirectly, by running a script that calls cargo.
INDIRECT_COMPILE_RE='scripts/build-ios-mobile-libs\.sh'

extract_job() {
  local wanted="$1"
  awk -v wanted="$wanted" '
    /^jobs:$/ { in_jobs = 1; next }
    in_jobs && /^  [[:alnum:]_-]+:$/ {
      name = $1
      sub(/:$/, "", name)
      if (capture && name != wanted) exit
      capture = (name == wanted)
    }
    capture { print }
  ' "$WORKFLOW"
}

job_names="$({
  awk '
    /^jobs:$/ { in_jobs = 1; next }
    in_jobs && /^  [[:alnum:]_-]+:$/ {
      name = $1
      sub(/:$/, "", name)
      print name
    }
  ' "$WORKFLOW"
})"

linux_compile_jobs=0
windows_compile_jobs=0
macos_compile_jobs=0
while IFS= read -r job_name; do
  [ -n "$job_name" ] || continue
  job_block="$(extract_job "$job_name")"

  # Windows runners have no protoc and no apt, so they carry their own
  # install contract instead of the pinned /usr/bin/protoc path.
  if grep -Eq '^[[:space:]]+runs-on: windows-' <<<"$job_block"; then
    grep -Eq "$CARGO_COMPILE_RE" <<<"$job_block" || continue

    windows_compile_jobs=$((windows_compile_jobs + 1))

    grep -Fq 'choco install protoc' <<<"$job_block" || {
      echo "$job_name must install protoc" >&2
      exit 1
    }
    grep -Fq 'protoc --version' <<<"$job_block" || {
      echo "$job_name must report the installed protoc version" >&2
      exit 1
    }

    install_line="$(grep -nF 'choco install protoc' <<<"$job_block" | head -n 1 | cut -d: -f1)"
    version_probe_line="$(grep -nF 'protoc --version' <<<"$job_block" | head -n 1 | cut -d: -f1)"
    first_compile_line="$(grep -nE "$CARGO_COMPILE_RE" <<<"$job_block" \
      | head -n 1 | cut -d: -f1)"

    if [ "$install_line" -ge "$first_compile_line" ] \
      || [ "$version_probe_line" -ge "$first_compile_line" ]; then
      echo "$job_name must install and probe protoc before its first Cargo compilation" >&2
      exit 1
    fi

    continue
  fi

  # macOS runners have neither apt nor a preinstalled protoc, so they carry a
  # brew install contract and use PATH rather than a pinned path.
  if grep -Eq '^[[:space:]]+runs-on: macos-' <<<"$job_block"; then
    if ! grep -Eq "$CARGO_COMPILE_RE" <<<"$job_block" \
      && ! grep -Eq "$INDIRECT_COMPILE_RE" <<<"$job_block"; then
      continue
    fi

    macos_compile_jobs=$((macos_compile_jobs + 1))

    grep -Eq 'brew install .*protobuf' <<<"$job_block" || {
      echo "$job_name must install protobuf" >&2
      exit 1
    }
    grep -Fq 'protoc --version' <<<"$job_block" || {
      echo "$job_name must report the installed protoc version" >&2
      exit 1
    }

    install_line="$(grep -nE 'brew install .*protobuf' <<<"$job_block" | head -n 1 | cut -d: -f1)"
    version_probe_line="$(grep -nF 'protoc --version' <<<"$job_block" | head -n 1 | cut -d: -f1)"
    first_compile_line="$(grep -nE "$CARGO_COMPILE_RE|$INDIRECT_COMPILE_RE" <<<"$job_block" \
      | head -n 1 | cut -d: -f1)"

    if [ "$install_line" -ge "$first_compile_line" ] \
      || [ "$version_probe_line" -ge "$first_compile_line" ]; then
      echo "$job_name must install and probe protoc before its first Cargo compilation" >&2
      exit 1
    fi

    continue
  fi

  grep -Eq '^[[:space:]]+runs-on: ubuntu-' <<<"$job_block" || continue
  grep -Eq "$CARGO_COMPILE_RE" <<<"$job_block" || continue

  linux_compile_jobs=$((linux_compile_jobs + 1))

  grep -Fq 'PROTOC: /usr/bin/protoc' <<<"$job_block" || {
    echo "$job_name must pin PROTOC to /usr/bin/protoc" >&2
    exit 1
  }
  grep -Fq 'protobuf-compiler' <<<"$job_block" || {
    echo "$job_name must install protobuf-compiler" >&2
    exit 1
  }
  grep -Fq 'test -x "$PROTOC"' <<<"$job_block" || {
    echo "$job_name must verify the pinned protoc path is executable" >&2
    exit 1
  }
  grep -Fq '"$PROTOC" --version' <<<"$job_block" || {
    echo "$job_name must report the pinned protoc version" >&2
    exit 1
  }

  install_line="$(grep -nF 'protobuf-compiler' <<<"$job_block" | head -n 1 | cut -d: -f1)"
  executable_probe_line="$(grep -nF 'test -x "$PROTOC"' <<<"$job_block" | head -n 1 | cut -d: -f1)"
  version_probe_line="$(grep -nF '"$PROTOC" --version' <<<"$job_block" | head -n 1 | cut -d: -f1)"
  first_compile_line="$(grep -nE "$CARGO_COMPILE_RE" <<<"$job_block" \
    | head -n 1 | cut -d: -f1)"

  if [ "$install_line" -ge "$first_compile_line" ] \
    || [ "$executable_probe_line" -ge "$first_compile_line" ] \
    || [ "$version_probe_line" -ge "$first_compile_line" ]; then
    echo "$job_name must install and probe protoc before its first Cargo compilation" >&2
    exit 1
  fi
done <<<"$job_names"

if [ "$linux_compile_jobs" -eq 0 ]; then
  echo 'CI workflow has no detected Linux Cargo compilation jobs' >&2
  exit 1
fi

echo "CI protoc source contract: PASS linux_jobs=$linux_compile_jobs windows_jobs=$windows_compile_jobs macos_jobs=$macos_compile_jobs"
