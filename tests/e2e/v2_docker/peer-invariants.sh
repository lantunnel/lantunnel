#!/bin/sh

# Public-field reader shared by the production-Docker and AL acceptance
# harnesses. JSON is used by Managed Platform downloads; lantunnel-admin writes
# YAML. The function never reads or prints private-key fields.
peer_profile_public_value() {
    peer_file=$1
    field=$2
    if command -v jq >/dev/null 2>&1 && jq -e . "$peer_file" >/dev/null 2>&1; then
        case "$field" in
            tunnel_id) jq -er '.tunnel_id' "$peer_file" ;;
            peer_id|overlay_ip) jq -er ".peer.$field" "$peer_file" ;;
            *) return 2 ;;
        esac
    else
        awk -v field="$field" '$1 == field ":" {print $2; exit}' "$peer_file" | tr -d '"'
    fi
}

peer_profile_file_identity() {
    peer_file=$1
    if identity=$(stat -Lc '%d:%i' "$peer_file" 2>/dev/null); then
        printf '%s\n' "$identity"
    else
        stat -f '%d:%i' "$peer_file"
    fi
}

# Usage: assert_distinct_peer_profiles <expected-tunnel-id> <absolute.peer>...
assert_distinct_peer_profiles() {
    expected_tunnel_id=$1
    shift
    [ "$#" -ge 1 ] || {
        echo 'Peer profile invariant requires at least one file' >&2
        return 1
    }
    seen_peer_paths='
'
    seen_peer_file_identities='
'
    seen_peer_ids='
'
    seen_overlay_ips='
'
    for peer_file in "$@"; do
        [ "${peer_file#/}" != "$peer_file" ] && [ -f "$peer_file" ] && [ ! -L "$peer_file" ] || {
            echo 'Peer profile invariant requires absolute, non-symlink regular file paths' >&2
            return 1
        }
        case "$seen_peer_paths" in
            *"
$peer_file
"*)
                echo 'Peer profile path collision detected' >&2
                return 1
                ;;
        esac
        seen_peer_paths="${seen_peer_paths}${peer_file}
"
        peer_file_identity=$(peer_profile_file_identity "$peer_file") || {
            echo 'Could not inspect Peer profile file identity' >&2
            return 1
        }
        case "$seen_peer_file_identities" in
            *"
$peer_file_identity
"*)
                echo 'Peer profile file identity collision detected' >&2
                return 1
                ;;
        esac
        seen_peer_file_identities="${seen_peer_file_identities}${peer_file_identity}
"
        [ "$(peer_profile_public_value "$peer_file" tunnel_id)" = "$expected_tunnel_id" ] || {
            echo 'Peer profile Tunnel ID does not match the acceptance Tunnel' >&2
            return 1
        }
        peer_id=$(peer_profile_public_value "$peer_file" peer_id)
        overlay_ip=$(peer_profile_public_value "$peer_file" overlay_ip)
        [ -n "$peer_id" ] && [ -n "$overlay_ip" ] || {
            echo 'Peer profile is missing a public Peer identity field' >&2
            return 1
        }
        case "$seen_peer_ids" in
            *"
$peer_id
"*)
                echo 'Stable Peer ID collision detected' >&2
                return 1
                ;;
        esac
        seen_peer_ids="${seen_peer_ids}${peer_id}
"
        case "$seen_overlay_ips" in
            *"
$overlay_ip
"*)
                echo 'Overlay IP collision detected' >&2
                return 1
                ;;
        esac
        seen_overlay_ips="${seen_overlay_ips}${overlay_ip}
"
    done
}
