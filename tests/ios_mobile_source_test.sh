#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IOS_DIR="$ROOT/apps/ios-proxy"

CONFIG="$IOS_DIR/TunnelProxyShared/MobileConfig.swift"
HOST="$IOS_DIR/TunnelProxy/WebHostView.swift"
APP="$IOS_DIR/TunnelProxy/TunnelProxyApp.swift"
PROJECT="$IOS_DIR/project.yml"
MODEL="$IOS_DIR/TunnelProxy/TunnelAppModel.swift"
CONTROL="$IOS_DIR/TunnelProxy/TunnelControlService.swift"
BRIDGE="$IOS_DIR/TunnelProxyShared/TunnelProxyNativeBridge.swift"
HEADER="$IOS_DIR/NativeLibs/TpMobileFfi/include/tp_mobile_ffi.h"
SHARED="$IOS_DIR/TunnelProxyShared/PacketTunnelLaunchConfiguration.swift"
RUNTIME="$IOS_DIR/PacketTunnel/NativeTunnelRuntime.swift"
PROVIDER="$IOS_DIR/PacketTunnel/PacketTunnelProvider.swift"
TEST="$IOS_DIR/TunnelProxyTests/PacketTunnelLaunchConfigurationTests.swift"
START_TEST="$IOS_DIR/TunnelProxyTests/MobileConfigTests.swift"

grep -q 'peerProfileJSON: String' "$CONFIG"
grep -q '"peer_profile": profile' "$CONFIG"
grep -q 'normalizedPeerProfileJSON' "$CONFIG"
# The profile is summarised, never rendered: it carries peer_private_key.
grep -q 'importPeerProfile' "$MODEL"
! grep -q 'peer_private_key' "$HOST"
if grep -Eq 'MobileSeed|parseSeed|tp_mobile_parse_seed_json|localProxyAuthEnabled|"tunnel_key"[[:space:]]*:' "$CONFIG" "$HOST" "$MODEL" "$CONTROL" "$BRIDGE" "$HEADER"; then
  echo "iOS production request/FFI surface still contains removed V1 setup/auth fields" >&2
  exit 1
fi
if grep -Eq '"(p2p_enabled|peer_client_id)"[[:space:]]*:' "$CONFIG"; then
  echo "iOS production request still contains a removed Mesh role field" >&2
  exit 1
fi
if grep -Fq '"peer_client_id"' "$CONTROL"; then
  echo "iOS status adapter still accepts the removed role-target status field" >&2
  exit 1
fi
if grep -Eq 'lanRouteLimit|lan_route_limit|Plan allows' "$CONFIG" "$HOST" "$MODEL" "$CONTROL" "$SHARED" "$PROVIDER" "$IOS_DIR/TunnelProxyShared/RouteValidator.swift"; then
  echo "iOS mobile production still contains a plan-derived LAN route limit" >&2
  exit 1
fi
if grep -Fq "TunnelProxyNativeBridge().statusJSON()" "$MODEL"; then
  echo "the app queries the FFI directly instead of the extension" >&2
  exit 1
fi

# --- the screens are not built here any more ---------------------------------
# iOS held a third copy of a vocabulary the desktop and Android also each had,
# which is how a Peer's state came to be worded three ways and Settings grew
# sections nobody else had. The screens come from one bundle now.
test -f "$HOST"
grep -qF 'WKWebView' "$HOST"
grep -qF 'name: "lantunnel"' "$HOST"
grep -qF 'WebHostView(model: model)' "$APP"
grep -qF 'forResource: "index", withExtension: "html", subdirectory: "ui"' "$HOST"

# The bundle is served over a scheme of our own, not read off disk. A file://
# page has an opaque origin, and the bundle's entry is a module script, which is
# fetched with CORS — so from file:// the script and the stylesheet are both
# refused and the app opens on a blank canvas.
grep -qF 'setURLSchemeHandler' "$HOST"
grep -qF 'WKURLSchemeHandler' "$HOST"
if grep -qF 'loadFileURL' "$HOST"; then
  echo "the iOS WebView loads the bundle over file://; module scripts are refused there" >&2
  exit 1
fi
if grep -qE 'https?://' "$HOST"; then
  echo "the iOS WebView names a remote origin" >&2
  exit 1
fi
# A request is only ever served from inside the packaged directory.
grep -qF 'file.path.hasPrefix(root.path)' "$HOST"
# A module script served as anything but JavaScript is refused outright.
grep -qF '"text/javascript; charset=utf-8"' "$HOST"

for gone in AppShellView StatusView ConfigView PeersView LogsView PeerProfileImportSheet; do
  if [ -f "$IOS_DIR/TunnelProxy/$gone.swift" ]; then
    echo "iOS draws a screen again: $gone" >&2
    exit 1
  fi
done

# Every command the shared UI can call is answered here. A missing one is a
# button that never resolves, which reads on screen as a dead control.
for command in $(grep -o "invoke<[^>]*>('[a-z_]*'" \
                   "$ROOT/apps/lantunnel-client/frontend/src/client-api.ts" \
                 | sed "s/.*('//; s/'//"); do
  if ! grep -qF "\"$command\"" "$HOST"; then
    echo "the iOS bridge does not answer $command" >&2
    exit 1
  fi
done
for command in get_capabilities pick_peer_profile; do
  if ! grep -qF "\"$command\"" "$HOST"; then
    echo "the iOS bridge does not answer $command" >&2
    exit 1
  fi
done

# A capability is what the platform genuinely cannot do, not what looks
# different. A phone routes every app through its VPN service, so a loopback
# SOCKS5 port would serve nobody and there is no login item to set.
grep -qF '"qrScanner": true' "$HOST"
grep -qF '"startAtLogin": false' "$HOST"
grep -qF '"localProxy": false' "$HOST"

# The projection comes from Rust and is passed through untouched. Re-deriving a
# label here is how the three Clients drifted apart in the first place.
grep -qF 'root["client_ui"]' "$HOST"
for gone in 'meshStateLabel' 'formatBytes' 'trafficLabel' 'reachability'; do
  if grep -qF "$gone" "$HOST"; then
    echo "the iOS host re-derives $gone; it belongs to client_ui in Rust" >&2
    exit 1
  fi
done

# The type ramp belongs to the bundle. A size literal here means a native
# surface started competing with it again.
if grep -qE '\.font\(\.system\(size: [0-9]+' "$HOST"; then
  echo "the iOS host sets a type size; the ramp lives in the shared bundle" >&2
  exit 1
fi

# --- following the mesh is not a choice -------------------------------------
# tunnel_first already decides overlap in the engine, and mergedMeshRoutes never
# consulted the hand-typed list, so the editor it revealed could not apply.
for gone in 'followMesh' 'Follow mesh' 'manualRoutes' 'manualRouteLimit'; do
  for f in "$CONFIG" "$HOST" "$MODEL"; do
    if grep -qF "$gone" "$f"; then
      echo "$(basename "$f") still carries $gone" >&2
      exit 1
    fi
  done
done
if grep -q 'Local network first' "$CONFIG" "$HOST" "$MODEL"; then
  echo 'the tunnel_first switch still carries the inverted label' >&2
  exit 1
fi

# --- the Access policy is stored in the shape the shared editor writes -------
grep -qF 'public var clientAccessJSON: String' "$CONFIG"
grep -qF 'public func legacyClientAccessPolicy()' "$CONFIG"
grep -qF 'next.accessRules = []' "$HOST"

# --- the UI bundle ships with the app ---------------------------------------
grep -qF 'path: TunnelProxy/Resources/ui' "$PROJECT"
grep -qF 'type: folder' "$PROJECT"
UI_INDEX="$IOS_DIR/TunnelProxy/Resources/ui/index.html"
test -f "$UI_INDEX"
# An absolute /assets/... path resolves to the filesystem root off a file URL
# and the page comes up blank.
if grep -qE 'src="/assets/|href="/assets/' "$UI_INDEX"; then
  echo "the packaged bundle uses absolute asset paths" >&2
  exit 1
fi
