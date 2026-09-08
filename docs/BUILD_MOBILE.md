# Building the mobile Clients

The Android Client ships as a signed `.apk` on every
[release](https://github.com/lantunnel/lantunnel/releases). Most people should just
download that one.

The iOS Client has no download and never will. Apple ties the entitlements a packet
tunnel needs to a specific developer account, so the only iOS Client that can exist is
one you build under your own account. That is what the second half of this page is for.

Both apps are the same shared UI in a system webview over the same Rust tunnel engine as
the desktop Client. Neither is a different product.

- [Android](#android)
- [iOS](#ios)

## Android

### Install the released APK

Download `lantunnel-client-<version>-android-arm64.apk` and `checksums.txt` from the
[latest release](https://github.com/lantunnel/lantunnel/releases/latest), verify it, then
open the APK. Android will ask you to allow installs from your browser or file manager.

```sh
FILE=lantunnel-client-2.0.9-android-arm64.apk
grep "  ${FILE}$" checksums.txt | sha256sum --check --strict -
```

The APK is signed with the Lantunnel release key, not distributed through Google Play.
Its signing certificate SHA-256 is:

```
9E:A6:2E:C4:06:25:D7:D6:85:03:6C:E3:E9:1A:DC:DA:C7:06:1C:2E:CB:F5:41:F3:24:AA:B9:39:2A:FE:83:B6
```

Only ARM64 is published. Every Android phone sold since roughly 2016 is ARM64; build
your own if you need `armeabi-v7a` or `x86_64`.

### Build your own APK

Requirements: Android SDK with NDK `27.2.12479018`, JDK 17, Node, `protoc`, Rust with
the `aarch64-linux-android` target, and `cargo-ndk`.

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

An unsigned APK installs on nothing, so create a keystore first. Keep it outside the
repository — it is the only key that will ever be able to upgrade what it installs.

```sh
keytool -genkeypair -keystore ~/lantunnel-android.jks -storetype PKCS12 \
  -alias lantunnel -keyalg RSA -keysize 4096 -validity 10000

cat > apps/android-proxy/keystore.properties <<'EOF'
storeFile=/absolute/path/to/lantunnel-android.jks
storePassword=...
keyAlias=lantunnel
keyPassword=...
EOF
```

`keystore.properties` and `*.jks` are gitignored. Then build from the repository root:

```sh
make release-android-proxy-apk
```

The APK lands in `dist/release/lantunnel-client-<version>-android-arm64.apk`. To build
for more ABIs, set `ANDROID_ABIS` and match it in `app/build.gradle.kts`:

```sh
make release-android-proxy-apk ANDROID_ABIS="arm64-v8a armeabi-v7a"
```

A debug build skips signing setup entirely and is enough to check a code change:

```sh
cd apps/android-proxy
ABIS="arm64-v8a" ./build-rust-jni-libs.sh
./gradlew :app:assembleDebug
```

### First run

Import the device's own `.peer` profile by scanning its QR code, then approve the VPN
permission dialog. The app installs only the private LAN routes the other Peers publish,
so the rest of the phone's traffic keeps using the normal network.

## iOS

### Why there is no download

The Packet Tunnel extension needs two entitlements:

- `com.apple.developer.networking.networkextension` with `packet-tunnel-provider`
- `com.apple.security.application-groups`

Apple grants neither to a free Personal Team, and both are bound to the developer
account that signed the build. A redistributable `.ipa` signed by this project would not
install for you, and a sideloaded unsigned build cannot get the entitlements at all.
So the iOS Client is built once, by you, under your own account.

**You need a paid Apple Developer Program membership.** There is no way around this for a
VPN app on iOS. Free provisioning is enough to run the tests and the SwiftUI screens in
the Simulator, but not to move a single packet on a real device.

### Requirements

- macOS with Xcode 15 or later, and the Command Line Tools
- [XcodeGen](https://github.com/yonaskolb/XcodeGen) — `brew install xcodegen`
- Node, `protoc`, and Rust with the iOS targets:

  ```sh
  rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
  ```

- An Apple Developer Program membership and its Team ID

### Claim your own identifiers

The checked-in identifiers belong to this project's account. Replace them with your own
before you build, in three places:

1. `apps/ios-proxy/project.yml` — `bundleIdPrefix`, and both `PRODUCT_BUNDLE_IDENTIFIER`
   values.
2. `apps/ios-proxy/TunnelProxy/TunnelProxy.entitlements` — the App Group string.
3. `apps/ios-proxy/PacketTunnel/PacketTunnel.entitlements` — the same App Group string.

The container app and the extension must share one App Group; that group is how the
extension hands tunnel status back to the UI. Getting it wrong produces an app that
launches and then reports nothing.

In the [Apple Developer portal](https://developer.apple.com/account/resources), register:

- App ID `<your-prefix>.tunnelproxy.ios`, with **Network Extensions** and **App Groups**
  enabled
- App ID `<your-prefix>.tunnelproxy.ios.PacketTunnel`, with the same two capabilities
- App Group `group.<your-prefix>.tunnelproxy.ios`

### Build and install

Stage the shared UI, build the Rust and hev-socks5-tunnel XCFrameworks, and generate the
Xcode project:

```sh
npm --prefix apps/lantunnel-client/frontend ci
make _stage-ios-ui
PROFILE=release scripts/build-ios-mobile-libs.sh
cd apps/ios-proxy && xcodegen generate
```

Then open `apps/ios-proxy/TunnelProxyIOS.xcodeproj`, select your team on both the
**TunnelProxy** and **PacketTunnel** targets, plug in your device, and run. Xcode creates
the provisioning profiles. On the device, trust the developer certificate under
**Settings → General → VPN & Device Management** the first time.

The signed build expires when its provisioning profile does — a year for a paid account.
Rebuild before then.

Use Xcode for the device install. `make release-ios-proxy-app` exists, but it pins
`CODE_SIGNING_REQUIRED=NO` and an empty `CODE_SIGN_IDENTITY` to produce an unsigned
bundle for inspection and CI. That bundle will not run on a phone.

### Simulator and tests

No account and no entitlements needed. Packet forwarding does not work in the Simulator,
but the UI and the shared models do:

```sh
xcodebuild build \
  -project apps/ios-proxy/TunnelProxyIOS.xcodeproj \
  -scheme TunnelProxy \
  -destination 'generic/platform=iOS Simulator'

xcodebuild test \
  -project apps/ios-proxy/TunnelProxyIOS.xcodeproj \
  -scheme TunnelProxyTests \
  -destination 'platform=iOS Simulator,name=iPhone 16'
```

### First run

Same as Android: scan the device's own `.peer` profile QR code, approve the VPN
configuration prompt, and the app installs only the LAN routes the other Peers publish.
