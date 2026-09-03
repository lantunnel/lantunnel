#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_TSX="$ROOT_DIR/apps/lantunnel-client/frontend/src/App.tsx"
CLIENT_API="$ROOT_DIR/apps/lantunnel-client/frontend/src/client-api.ts"
BRIDGE="$ROOT_DIR/apps/lantunnel-client/frontend/src/bridge.ts"
CAPS="$ROOT_DIR/apps/lantunnel-client/frontend/src/capabilities.ts"

assert_absent() {
  local pattern="$1"
  shift
  if grep -q "$pattern" "$@"; then
    echo "unexpected frontend source match: $pattern" >&2
    return 1
  fi
}

assert_absent_ci() {
  local pattern="$1"
  shift
  if grep -qi "$pattern" "$@"; then
    echo "unexpected frontend source match: $pattern" >&2
    return 1
  fi
}

# The Client keeps the reviewed Connection / Settings / Logs shell and renders
# the app-owned V2 status snapshot instead of rebuilding frontend state.
grep -q "type AppTab = 'connection' | 'peers' | 'settings' | 'logs'" "$APP_TSX"
grep -q 'TabButton label="Connection"' "$APP_TSX"
grep -q 'TabButton label="Settings"' "$APP_TSX"
grep -q 'TabButton label="Logs"' "$APP_TSX"
grep -q "status.client_ui" "$APP_TSX"
grep -q "Gateway attachment" "$APP_TSX"
grep -q "Mesh" "$APP_TSX"
grep -q "Native routing" "$APP_TSX"
grep -q "Peers in this tunnel" "$APP_TSX"
grep -q "Encrypted Relay" "$APP_TSX"
grep -q "Active here" "$APP_TSX"
grep -q "Standby #" "$APP_TSX"
grep -q "Direct" "$APP_TSX"
grep -q "Relay" "$APP_TSX"

# How long the tunnel has been up. Every client answers it, in the same place:
# after the badges and before the traffic rows.
grep -q 'label="Uptime"' "$APP_TSX"
grep -q 'function formatUptime' "$APP_TSX"

# --- no Details disclosure on Connection ------------------------------------
# The same ban the two phones carry. Platform, Transport and Routes are not
# acted on, so folding them away would hide rather than remove them; the Logs
# tab is where diagnosis belongs. A row every client refuses is still a row all
# three agree on.
for gone in 'showDetails' '<details' '>Details<' 'label="Platform"' 'label="Transport"' 'label="Routes"'; do
  if grep -qF "$gone" "$APP_TSX"; then
    echo "Connection still carries $gone" >&2
    exit 1
  fi
done

# Settings has exactly these four V2 sections, in this order.
connection_line="$(grep -n '>Connection<' "$APP_TSX" | head -1 | cut -d: -f1)"
network_line="$(grep -n '>Network<' "$APP_TSX" | head -1 | cut -d: -f1)"
access_line="$(grep -n '>Access<InfoHint' "$APP_TSX" | head -1 | cut -d: -f1)"
test "$connection_line" -lt "$network_line"
test "$network_line" -lt "$access_line"
# The local proxy lives inside the network section: it opens a port on this
# machine. As its own "Diagnostics" section it named nothing an owner would
# recognise, and it carried a second copy of the log level the Logs tab owns.
# It no longer carries a heading of its own either — the section said
# "Local proxy" three times over, so the toggle label is the one name left.
proxy_line="$(grep -n 'label="Local proxy"' "$APP_TSX" | head -1 | cut -d: -f1)"
test -n "$proxy_line"
test "$network_line" -lt "$proxy_line"
test "$proxy_line" -lt "$access_line"
if grep -q '>Diagnostics<' "$APP_TSX"; then
  echo "the local proxy is a section called Diagnostics again" >&2
  exit 1
fi
if [ "$(grep -c 'saveSettingsPatch({ log_level' "$APP_TSX")" -ne 1 ]; then
  echo "the log level is settable from more than one place again" >&2
  exit 1
fi
# Rules are edited as rules. The default-action selector is gone: an empty
# Allow list means open, so the list answers the question by itself.
assert_absent 'label="Default"' "$APP_TSX"
grep -q 'title="Allowed destinations"' "$APP_TSX"
grep -q 'title="Blocked destinations"' "$APP_TSX"
grep -q 'client_access: ClientAccessPolicyV2' "$CLIENT_API"
grep -q 'exported_lans: string\[\]' "$CLIENT_API"
grep -q 'tunnel_first: boolean' "$CLIENT_API"
# The JSON escape hatch is gone from every client: the builder cannot produce
# a rule the parser refuses, so a second, rawer way in only offered a way to
# get it wrong.
assert_absent 'Allow rules (JSON array)' "$APP_TSX"
assert_absent 'Edit as JSON' "$APP_TSX"
assert_absent 'accessJsonDraft' "$APP_TSX"
# Peers is its own tab now, so the first screenful holds ten.
grep -q 'filteredPeers.slice(0, 10)' "$APP_TSX"
grep -q 'function RuleForm' "$APP_TSX"
grep -q 'LAN Export<InfoHint' "$APP_TSX"
grep -q 'Interface unavailable' "$APP_TSX"
# An idle Client has reported no interface facts yet, which is not the same as
# an interface that is missing.
grep -q 'Checked once connected' "$APP_TSX"
grep -q 'Published' "$APP_TSX"
grep -q 'exported_lan_statuses' "$APP_TSX"
grep -q 'exportedLanReadiness.get(prefix) === true' "$APP_TSX"
grep -q 'exported_lan_statuses: LocalExportStatusV2\[\]' "$CLIENT_API"
assert_absent 'Saved, not applied' "$APP_TSX"
assert_absent '<span className="shrink-0 text-yellow-300">Interface unavailable</span>' "$APP_TSX"
grep -q 'saveClientAccess' "$APP_TSX"
grep -q 'saveExportedLans' "$APP_TSX"
# The mesh is a tab, not a paragraph inside Connection.
grep -q "activeTab === 'peers'" "$APP_TSX"
grep -q 'peerSearch' "$APP_TSX"
# The profile summary must never render key material.
assert_absent 'peer_private_key' "$APP_TSX"
grep -q 'Loopback only' "$APP_TSX"
assert_absent 'group_id/group_password' "$APP_TSX"
assert_absent 'Require local proxy authentication' "$APP_TSX"

# Runtime truth has an explicit DTO; legacy counts never become Peer rows in React.
grep -q "interface ClientUiStatusV2" "$CLIENT_API"
grep -q "gateway_attachment" "$CLIENT_API"
grep -q "peer_directory" "$CLIENT_API"
grep -q "current_path" "$CLIENT_API"
grep -q "native_routing" "$CLIENT_API"
grep -q "direct_tx_bytes" "$CLIENT_API"

# The desktop Client joins through imported V2 Peer profiles. React only sees
# public summaries and delegates validation/storage to Rust.
grep -q "interface ImportedPeerSummaryV2" "$CLIENT_API"
grep -q "listPeerProfiles" "$CLIENT_API"
# Importing is one call for every Client: the host runs whatever picker it has.
# The desktop's file dialog lives in the bridge, not in the shared screen.
grep -q "pickPeerProfile" "$CLIENT_API"
grep -q "import_peer_profile" "$BRIDGE"
grep -q "@tauri-apps/plugin-dialog" "$BRIDGE"
# The shared screen must not reach for a host API directly; that is the seam.
if grep -q "@tauri-apps" "$APP_TSX"; then
  echo "the shared screen imports a host API directly" >&2
  exit 1
fi
grep -q "export function hostKind" "$BRIDGE"
grep -q "export interface Capabilities" "$CAPS"

# --- one product name, on every Client --------------------------------------
# The header renders whatever the host reports. Three hosts reporting three
# names is the same class of divergence the shared UI was built to end.
MAIN_RS="$ROOT_DIR/apps/lantunnel-client/src-tauri/src/main.rs"
ANDROID_ACTIVITY="$ROOT_DIR/apps/android-proxy/app/src/main/java/com/buhuipao/tunnelproxy/MainActivity.kt"
IOS_HOST="$ROOT_DIR/apps/ios-proxy/TunnelProxy/WebHostView.swift"

if ! grep -A6 'fn display_name' "$MAIN_RS" | grep -qF '"Lantunnel"'; then
  echo "the desktop reports a product name other than Lantunnel" >&2
  exit 1
fi
grep -qF '.put("display_name", "Lantunnel")' "$ANDROID_ACTIVITY"
grep -qF '"display_name": "Lantunnel",' "$IOS_HOST"
grep -qF "display_name: 'Lantunnel'," "$APP_TSX"
for f in "$ANDROID_ACTIVITY" "$IOS_HOST" "$APP_TSX"; do
  if grep -qF 'Lantunnel Client' "$f"; then
    echo "$(basename "$f") still shows the bundle name where the product name goes" >&2
    exit 1
  fi
done
# The bundle name does not move with the product name: the installed .app, the
# DMG volume and the exe keep their paths, and the macOS helper authorises
# exactly one client path.
grep -q '"productName": "Lantunnel Client"' "$ROOT_DIR/apps/lantunnel-client/src-tauri/tauri.conf.json"
grep -q "connectPeerProfile" "$CLIENT_API"
grep -q "interface GatewayBootstrapV2" "$CLIENT_API"
# The Gateway comes from the .peer file. It was overridable locally — address,
# TLS server name and trusted certificate together — and the Peer membership
# signature does not cover the Gateway facts, so that pointed the Client at a
# host of someone's choosing and told it to trust that host.
if grep -qE "getStaticGatewayFacts|setStaticGatewayFacts" "$CLIENT_API"; then
  echo "the Gateway can be overridden from the Client again" >&2
  exit 1
fi
grep -q "Import .peer" "$APP_TSX"
grep -q "api.connectPeerProfile" "$APP_TSX"
assert_absent "peer_private_key" "$APP_TSX" "$CLIENT_API"
assert_absent "membership_signature" "$APP_TSX" "$CLIENT_API"
assert_absent 'Field label="Tunnel ID"' "$APP_TSX"
assert_absent 'Field label="Tunnel Key"' "$APP_TSX"
assert_absent "api.connect(" "$APP_TSX"
assert_absent "loadCredentials" "$APP_TSX" "$CLIENT_API"
assert_absent "saveCredentials" "$APP_TSX" "$CLIENT_API"

# V2 Settings does not expose old topology switches or forbidden control-plane UI.
assert_absent 'label="LAN device addressing"' "$APP_TSX"
assert_absent 'Legacy LAN Routes' "$APP_TSX"
assert_absent 'label="Tunnel routes via TUN"' "$APP_TSX"
assert_absent_ci 'expected_generation' "$APP_TSX" "$CLIENT_API"
assert_absent_ci 'security tab' "$APP_TSX"
assert_absent_ci 'rollout' "$APP_TSX"
assert_absent_ci 'profile editor' "$APP_TSX"

# The rule editor and the JSON editor are two views of one policy. Showing both
# at once let a rule added in the form sit behind a stale draft, and Save JSON
# put the pre-edit policy back without saying so.

# A setting is named, not explained. Every toggle carried a sentence under it,
# so the Settings tab was a wall of small grey text and the names had grown
# into sentences themselves to compensate. The explanation moves behind an info
# control that shows it on demand.
grep -q 'function InfoHint' "$APP_TSX"
grep -q 'aria-label="What this does"' "$APP_TSX"
if grep -qE 'label="(Direct connections over the local network|Let apps on this computer use the Tunnel|Prefer the local network)"' "$APP_TSX"; then
  echo "a setting is still named with a sentence" >&2
  exit 1
fi
# V2 reconnects the last selected Peer profile. There are no saved credentials.
if grep -q "Connect using saved credentials" "$APP_TSX"; then
  echo "the auto-connect description still names credentials that do not exist" >&2
  exit 1
fi


# --- review asks 2,3,4,5,7 ------------------------------------------------
# No Peer ID reaches any client surface but the Logs tab, not even a
# truncated one, and no row prints the same address twice.
if grep -q 'shortPeerId' "$APP_TSX"; then
  echo 'a Peer ID still reaches the UI through shortPeerId' >&2
  exit 1
fi
# The peer row rendered overlay_cidr as its title and again beneath it, so the
# same address appeared twice in one row. Count only the row's own renders;
# this_peer and the search haystack are different uses.
if [ "$(grep -c '{peer\.overlay_cidr}' "$APP_TSX")" -gt 1 ]; then
  echo 'the peer row still prints its overlay address more than once' >&2
  exit 1
fi
# This Peer and its address share one line.
grep -q 'This Peer</span>' "$APP_TSX"
# A profile can still be removed, as an icon on the row.
grep -q 'aria-label="Remove this profile"' "$APP_TSX"
# Local proxy says its name once, and the address sits on the toggle's row.
if [ "$(grep -c '>Local proxy<' "$APP_TSX")" -gt 1 ]; then
  echo 'Local proxy is named more than once in the same section' >&2
  exit 1
fi

# --- Tunnel First -----------------------------------------------------------
# The switch binds straight to tunnel_first, and tunnel_first = true means the
# tunnel wins over an overlapping connected LAN. "Local network first" said the
# opposite of what it did. Polarity is unchanged; only the name.
if grep -q 'Local network first' "$APP_TSX"; then
  echo 'the tunnel_first switch still carries the inverted label' >&2
  exit 1
fi
grep -q 'Tunnel First' "$APP_TSX"

# --- Native routing is its own switch ---------------------------------------
# Tunnel First used to be the only control on this window that could start the
# TUN, so turning it off reported Native routing: Disabled. Native routing has
# its own switch now, and Tunnel First is disabled while it is off — with the
# owner's answer kept rather than cleared.
grep -q "desktop_network_mode: enabled ? 'lan_routes_tun' : 'socks5_only'" "$APP_TSX"
grep -q 'disabled={!nativeRoutingEnabled || status.connecting || loading}' "$APP_TSX"
# A phone has nothing to switch: its VPN service is the only way to reach other
# apps' traffic. The control is absent there, and Tunnel First stays available.
grep -q 'nativeRoutingSwitch: boolean' "$CAPS"
if ! grep -A6 'const PHONE' "$CAPS" | grep -q 'nativeRoutingSwitch: false'; then
  echo 'a phone is offered a native routing switch it cannot answer' >&2
  exit 1
fi
grep -q 'caps.nativeRoutingSwitch$' "$APP_TSX"

# --- forward_to is gone -----------------------------------------------------
# It was in the 2.0 spec as an Advanced option on a ThisPeer Allow rule, and
# the engine validated it, but it never reached a changelog and the UI gave it
# only a placeholder. Nobody could have known what the box was for.
ROOT_DIR_ABS="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for f in "$APP_TSX" \
         "$ROOT_DIR_ABS/apps/lantunnel-client/frontend/src/client-api.ts" \
         "$ROOT_DIR_ABS/crates/tp-client/src/access_policy.rs"; do
  if grep -qE 'forward_to|Send it to' "$f"; then
    echo "$(basename "$f") still carries forward_to" >&2
    exit 1
  fi
done

# --- a toggle must toggle ---------------------------------------------------
# A <label> binds to the first labelable element inside it, and <button> is
# labelable. With InfoHint's button sitting above the checkbox, every row
# bound to the info button instead, so no switch on the Settings tab could be
# clicked at all.
toggle_body="$(sed -n '/^function Toggle(/,/^}/p' "$APP_TSX")"
if grep -q '<label' <<<"$toggle_body" && grep -q 'InfoHint' <<<"$toggle_body"; then
  grep -q 'htmlFor' <<<"$toggle_body" || {
    echo 'Toggle wraps InfoHint in a bare <label>; the button steals the binding' >&2
    exit 1
  }
fi
grep -q 'useId' "$APP_TSX"
