#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
multi_sender="$repo_root/crates/tp-client/src/p2p/multi_sender.rs"
engine="$repo_root/crates/tp-client/src/engine.rs"
proxy_tunnel="$repo_root/crates/tp-client/src/proxy_tunnel.rs"
pipe="$repo_root/crates/tp-client/src/engine/pipe.rs"
transport_session="$repo_root/crates/tp-transport/src/session.rs"
relay_crypto="$repo_root/crates/tp-client/src/relay_crypto.rs"

for cached_aad in control_aad data_aad udp_aad; do
  rg -q "${cached_aad}: RelayAadV2" "$multi_sender" || {
    echo "missing per-Flow cached Relay AAD: ${cached_aad}" >&2
    exit 1
  }
done

if rg -q 'payload\.to_vec\(\)' "$multi_sender" "$engine"; then
  echo "Relay framed hot path must not clone payload into Vec" >&2
  exit 1
fi

rg -q 'seal_precomputed\(&self\.data_aad' "$multi_sender"
rg -q 'seal_precomputed\(&self\.udp_aad' "$multi_sender"
rg -q 'open_bytes_precomputed' "$engine"
rg -q 'read_tcp_flow_frame_into_bytes' "$engine" "$proxy_tunnel"
rg -q 'let mut limited = \(&mut \*buf\)\.limit\(remaining\)' "$transport_session"
rg -q 'send_prepared_data' "$pipe" "$proxy_tunnel"
rg -q 'try_send_prepared_data' "$pipe" "$proxy_tunnel"
rg -q 'put_bytes\(0, .*RELAY_NONCE_SIZE_V2' "$pipe" "$proxy_tunnel"
rg -q 'take_prepared_record\(&mut arena, n\)' "$pipe"

if rg -q 'split_to\(.*RELAY_NONCE_SIZE_V2 \+ n\)' "$pipe"; then
  echo "producer split discarded the in-place AEAD tag capacity" >&2
  exit 1
fi

if rg -q 'sealed\.extend_from_slice\(&payload\)|opened = Vec::with_capacity' "$proxy_tunnel"; then
  echo "sealed TCP pump reintroduced a second record copy" >&2
  exit 1
fi

if sed -n '/pub async fn read_tcp_flow_frame_into_bytes/,/^}/p' "$transport_session" \
  | rg -q 'resize\(len as usize, 0\)'; then
  echo "sealed TCP reader reintroduced a full-record zero fill" >&2
  exit 1
fi

if sed -n \
  -e '/fn seal_prepared_with_nonce/,/^}/p' \
  -e '/fn open_in_place/,/^}/p' \
  "$relay_crypto" | rg -q 'copy_within'; then
  echo "prepared Relay codec reintroduced a plaintext move" >&2
  exit 1
fi

rg -q 'prepared_elapsed <= legacy_elapsed' "$relay_crypto"
rg -q 'prepared_open_elapsed <= legacy_open_elapsed' "$relay_crypto"

echo "V2 Relay hot-path source contract passed"
