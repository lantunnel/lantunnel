# Lantunnel Android

The Android Peer: a split-tunnel VPN client that reaches the LAN services other
Peers publish.

Runtime path:

`Android VpnService LAN routes -> tun2socks -> 127.0.0.1:1080 internal SOCKS5 -> Rust tunnel engine -> Direct PeerLink preferred / Encrypted Relay fallback -> remote Peer`

The app owns routing through Android `VpnService`. It installs only configured
private LAN CIDRs, so normal internet traffic keeps using the phone network.
Lantunnel excludes its own package from the VPN with
`addDisallowedApplication(packageName)` to avoid routing control traffic back
through the tunnel.

The routes installed are the ones the other Peers publish; there is no default
CIDR list to ask for.

Import a Lantunnel V2 Peer profile by scanning its QR code. The app passes that
profile to the native runtime as the single source of tunnel and peer identity.

Native library packaging is expected at:

`app/src/main/jniLibs/<abi>/libtp_mobile_ffi.so`

Build the Rust library for the Android ABI before assembling this app:

```sh
ABIS="arm64-v8a" ./build-rust-jni-libs.sh
```

Then assemble with the checked-in Gradle wrapper from this directory:

```sh
./gradlew :app:assembleDebug
```

The machine running the build must provide Android SDK, Android NDK,
`cargo-ndk`, and the matching Rust Android target.

Release APK packaging from the repository root:

```sh
make release-android-proxy-apk VERSION=2.0.8
```

The APK is copied to:

`dist/release/lantunnel-client-2.0.8-android-arm64.apk`

## Device Smoke

`run-device-smoke.sh` builds native libs, assembles the APK, installs it on
the connected adb device, starts `MainActivity` with the same JSON that the UI
uses, forwards a host TCP port to the device internal SOCKS5 port, and fetches
a private LAN URL through the VPN route.

```sh
PEER_PROFILE_FILE=/absolute/path/to/device.peer \
SMOKE_URL=http://<lan-host>:18080/health \
./run-device-smoke.sh
```

For an Encrypted Relay fallback run, use a topology where Direct cannot
establish, then confirm the smoke URL still loads. Mesh is always enabled and
has no runtime toggle. Set `EXPECT_LOG_REGEX` when the deployed build emits a
known Direct or Encrypted Relay marker in logcat.
