#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID_DIR="$ROOT/apps/android-proxy/app/src/main/java/com/buhuipao/tunnelproxy"

CONFIG="$ANDROID_DIR/MobileConfig.kt"
ACTIVITY="$ANDROID_DIR/MainActivity.kt"
VPN="$ANDROID_DIR/TunnelVpnService.kt"
TEST="$ROOT/apps/android-proxy/app/src/test/java/com/buhuipao/tunnelproxy/Tun2SocksConfigBuilderTest.kt"
MOBILE_TEST="$ROOT/apps/android-proxy/app/src/test/java/com/buhuipao/tunnelproxy/MobileConfigTest.kt"
GRADLE="$ROOT/apps/android-proxy/app/build.gradle.kts"
SMOKE="$ROOT/apps/android-proxy/run-device-smoke.sh"
README="$ROOT/apps/android-proxy/README.md"

grep -q 'peerProfileJson: String' "$CONFIG"
grep -q 'put("peer_profile", JSONObject(normalizePeerProfileJson(peerProfileJson)))' "$CONFIG"
grep -q 'profile.optInt("version") == 2' "$CONFIG"
# The profile is summarised, never rendered: it carries peer_private_key.
! grep -q 'Peer profile JSON' "$ACTIVITY"
# The identity is summarised for the shared UI, never rendered here.
grep -q 'fun peerSummary(' "$ACTIVITY"
# The Activity is exported, so any installed app can send it an Intent. It must
# not act on a Peer profile carried in one: VpnService consent is granted once
# and remembered, so that would let another app raise the tunnel against a
# Tunnel of its choosing.
! grep -q 'EXTRA_START_JSON' "$ACTIVITY"
# The Peers tab is drawn by the shared bundle; what this side owes it is the
# route derivation off the same directory.
grep -q 'peer_directory' "$CONFIG"
grep -q 'routesFromExports' "$ACTIVITY"
grep -q 'buildStartJsonUsesOnlyPeerProfileForTunnelIdentity' "$MOBILE_TEST"
grep -q 'freshConfigAcceptsASecondPrivateLanRoute' "$MOBILE_TEST"
grep -q 'saveConfigRoundTripsFreshBlankPeerProfile' "$MOBILE_TEST"
if grep -Eq 'lanRouteLimit|lan_route_limit|Plan allows' "$CONFIG" "$ACTIVITY" "$VPN"; then
  echo "Android mobile production still contains a dynamic or plan-derived LAN route limit" >&2
  exit 1
fi
# A bound on what a person types, not a ceiling on what the mesh publishes.
grep -q 'const val MAX_MANUAL_LAN_ROUTES' "$CONFIG"
# The start request is built in MobileConfig; no V1 identity may reach it.
if grep -Eq 'fromSeedJson|parseSeedJson|localProxyAuthEnabled|put\("tunnel_(id|key)"' "$CONFIG"; then
  echo "Android production request surface still contains removed V1 setup/auth fields" >&2
  exit 1
fi
# The Activity reports a Tunnel ID to the shared UI, which is what names an
# imported profile. A Tunnel *key* is V1 and belongs nowhere.
if grep -Eq 'fromSeedJson|parseSeedJson|localProxyAuthEnabled|tunnel_key' "$ACTIVITY"; then
  echo "Android Activity still contains removed V1 setup/auth fields" >&2
  exit 1
fi
if grep -Eq 'put\("(p2p_enabled|peer_client_id)"' "$CONFIG" "$ACTIVITY"; then
  echo "Android production request still contains a removed Mesh role field" >&2
  exit 1
fi
if grep -Eq 'P2P_ENABLED|PEER_CLIENT_ID|p2p_enabled|peer_client_id' "$SMOKE"; then
  echo "Android device smoke still sends a removed Mesh role/target field" >&2
  exit 1
fi
grep -q 'PEER_PROFILE_FILE=/absolute/path/to/device.peer' "$README"
if grep -Eq 'TUNNEL_KEY|PLATFORM_URL|P2P_ENABLED' "$README"; then
  echo "Android README still advertises removed V1 identity or Mesh controls" >&2
  exit 1
fi

grep -q 'optBoolean("auth_enabled", false)' "$VPN"
grep -q 'if (authEnabled)' "$VPN"
# optString, not getString: a missing key must not throw on the VPN thread.
grep -q 'optString("username")' "$VPN"
grep -q 'optString("password")' "$VPN"
grep -q 'internal object Tun2SocksConfigBuilder' "$VPN"

grep -q 'testImplementation("junit:junit:4.13.2")' "$GRADLE"
grep -q 'testImplementation("org.json:json:' "$GRADLE"
grep -q 'buildOmitsCredentialsWhenLocalProxyAuthIsDisabled' "$TEST"
grep -q 'buildOmitsCredentialsWhenAuthEnabledIsAbsent' "$TEST"
grep -q 'buildDoesNotThrowWhenAuthEnabledButCredentialsMissing' "$TEST"

# A property initializer runs inside the Activity constructor, before the
# framework attaches a base Context. Touching SharedPreferences from one is an
# NPE on cold start, before any UI is drawn — and the JVM suite never
# constructs the Activity, so it cannot see it.
if perl -0777 -ne 'exit 1 if /^\s+private va[lr] [^\n]*=(?:[^\n]*\n\s+)?[^\n]*MobileConfig\.\w+\([^\n]*\bthis\b/m' "$ACTIVITY"; then
  :
else
  echo "MainActivity uses a Context in a property initializer; move it into onCreate" >&2
  exit 1
fi

# --- the screens are not built here any more ---------------------------------
# Two thousand lines of Kotlin used to draw them, and had to agree with a React
# file and a SwiftUI file by discipline alone. It did not: four tab labels at
# three different sizes, a Peer's state worded three ways, and a Settings tab
# whose sections were invented locally. The Activity hosts the shared bundle
# now and owns only what a phone genuinely owns.
BRIDGE="$ANDROID_DIR/WebBridge.kt"
test -f "$BRIDGE"

grep -qF 'addJavascriptInterface(WebBridge(this@MainActivity), "__lantunnelAndroid")' "$ACTIVITY"
grep -qF 'setContentView(frame)' "$ACTIVITY"

# The bundle is served from the APK over an intercepted host, not off disk. A
# file:// page has an opaque origin, and the bundle's entry is a module script,
# which is fetched with CORS — so from file:// the script and the stylesheet are
# both refused and the app opens on a blank canvas with nothing in any native
# log to say why.
grep -qF 'shouldInterceptRequest' "$ACTIVITY"
grep -qF 'appassets.androidplatform.net' "$ACTIVITY"
if grep -qF 'file:///android_asset' "$ACTIVITY"; then
  echo "the Android WebView loads the bundle over file://; module scripts are refused there" >&2
  exit 1
fi
# The reserved host resolves nowhere, so a missed interception fails rather
# than reaching the network. It is the only URL the WebView may load.
if [ "$(grep -c 'loadUrl(' "$ACTIVITY")" -ne 1 ] ||
   ! grep -qF 'loadUrl(UI_URL)' "$ACTIVITY" ||
   ! grep -qF 'private const val UI_URL = "https://appassets.androidplatform.net/index.html"' "$ACTIVITY"; then
  echo "the Android WebView loads something other than the packaged UI origin" >&2
  exit 1
fi
grep -qF 'settings.allowFileAccess = false' "$ACTIVITY"
grep -qF 'settings.allowContentAccess = false' "$ACTIVITY"
# A path is only ever served from inside the packaged directory.
grep -qF "path.contains(\"..\")" "$ACTIVITY"

# The strongest form of "one UI": the Activity cannot import a widget.
# FrameLayout is the one exception: it carries the system-bar padding around
# the WebView, because a WebView does not lay its page out inside its own
# padding. It draws nothing.
if grep -E '^import android\.widget\.' "$ACTIVITY" | grep -qv '^import android\.widget\.FrameLayout$'; then
  echo "MainActivity imports a widget again — screens belong to the shared bundle" >&2
  exit 1
fi
# The container holds exactly one child, and it is the WebView.
if [ "$(grep -c 'addView(' "$ACTIVITY")" -ne 1 ]; then
  echo "the padding container holds more than the WebView" >&2
  exit 1
fi
if ! grep -A1 'addView(' "$ACTIVITY" | grep -qF 'webView'; then
  echo "the padding container holds something other than the WebView" >&2
  exit 1
fi
for gone in 'titleText(' 'mutedText(' 'valueText(' 'badgeText(' 'settingSwitch(' \
            'settingRow(' 'tabBar()' 'accessBuilder(' 'peerRowView(' 'statusCard()' \
            'settingsCard(' 'buildPeersPage(' 'logsToolbar('; do
  if grep -qF "$gone" "$ACTIVITY"; then
    echo "MainActivity draws a screen again: $gone" >&2
    exit 1
  fi
done

# Every command the shared UI can call is answered here. A missing one is a
# button that never resolves, which reads on screen as a dead control.
for command in $(grep -o "invoke<[^>]*>('[a-z_]*'" \
                   "$ROOT/apps/lantunnel-client/frontend/src/client-api.ts" \
                 | sed "s/.*('//; s/'//"); do
  if ! grep -qF "\"$command\"" "$BRIDGE"; then
    echo "the Android bridge does not answer $command" >&2
    exit 1
  fi
done
for command in get_capabilities pick_peer_profile; do
  if ! grep -qF "\"$command\"" "$BRIDGE"; then
    echo "the Android bridge does not answer $command" >&2
    exit 1
  fi
done

# A capability is what the platform genuinely cannot do, not what looks
# different. A phone routes every app through its VPN service, so a loopback
# SOCKS5 port would serve nobody and there is no login item to set.
grep -qF '.put("qrScanner", true)' "$BRIDGE"
grep -qF '.put("startAtLogin", false)' "$BRIDGE"
grep -qF '.put("localProxy", false)' "$BRIDGE"

# A transient service record the runtime has stopped backing must not freeze
# the UI. The service writes Connecting before the engine starts; if the engine
# then fails and goes away, nothing rewrites that record, and every control the
# shared UI disables while connecting stays disabled under a Disconnected
# headline. This guard exists because the rewrite dropped it once.
grep -qF 'fun recoverStaleServiceState(' "$ACTIVITY"
grep -qF 'isStaleTransient()' "$ACTIVITY"
grep -qF 'recoverStaleServiceState(' "$ACTIVITY"

# The projection comes from Rust and is passed through untouched. Re-deriving
# a label here is how the three Clients drifted apart in the first place.
grep -qF 'root.optJSONObject("client_ui")' "$ACTIVITY"
for gone in 'meshStateLabel' 'reachability' 'formatBytes' 'formatUptime' 'trafficLabel'; do
  if grep -qF "$gone" "$ACTIVITY"; then
    echo "MainActivity re-derives $gone; it belongs to client_ui in Rust" >&2
    exit 1
  fi
done

# --- Tunnel First -----------------------------------------------------------
# The label lives in the shared bundle now, but the inverted name must not come
# back anywhere on this side either.
if grep -q 'Local network first' "$ACTIVITY" "$CONFIG"; then
  echo 'the tunnel_first switch still carries the inverted label' >&2
  exit 1
fi

# --- following the mesh is not a choice -------------------------------------
# tunnel_first already decides overlap in the engine, and mergedMeshRoutes never
# consulted the hand-typed list, so the editor it revealed could not apply.
for gone in 'followMesh' 'Follow mesh' 'manualRoutesBlock' 'routesFromInput' 'routeInputs'; do
  for f in "$ACTIVITY" "$CONFIG"; do
    if grep -qF "$gone" "$f"; then
      echo "$(basename "$f") still carries $gone" >&2
      exit 1
    fi
  done
done

# --- the Access policy is stored in the shape the shared editor writes -------
# The text-line format was an Android-UI artifact. It is still read so an
# upgrade keeps its rules, and never written again.
grep -qF 'val clientAccessJson: String' "$CONFIG"
grep -qF 'fun legacyClientAccessPolicy()' "$CONFIG"
grep -qF 'accessRules = emptyList()' "$ACTIVITY"

# --- the UI bundle ships with the app ---------------------------------------
UI_INDEX="$ROOT/apps/android-proxy/app/src/main/assets/ui/index.html"
test -f "$UI_INDEX"
# An absolute /assets/... path resolves to the filesystem root under
# file:///android_asset/ and the page comes up blank.
if grep -qE 'src="/assets/|href="/assets/' "$UI_INDEX"; then
  echo "the packaged bundle uses absolute asset paths" >&2
  exit 1
fi
