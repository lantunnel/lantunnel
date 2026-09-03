# Lantunnel iOS

This directory contains the generated Xcode project source for the Lantunnel iOS container app and Packet Tunnel extension.

## Generate

```sh
cd apps/ios-proxy
xcodegen generate
```

## Build

```sh
xcodebuild build \
  -project apps/ios-proxy/TunnelProxyIOS.xcodeproj \
  -scheme TunnelProxy \
  -destination 'generic/platform=iOS Simulator'
```

A named simulator was pinned here and stopped existing when Xcode moved on, so
the command failed for a reason that had nothing to do with the code. The
generic destination does not rot.

## Test

```sh
xcodebuild test \
  -project apps/ios-proxy/TunnelProxyIOS.xcodeproj \
  -scheme TunnelProxyTests \
  -destination 'platform=iOS Simulator,name=iPhone 16'
```

## Device Notes

The Packet Tunnel extension requires Apple's Network Extension entitlement for real-device use. Simulator builds are useful for SwiftUI and shared model validation, but packet forwarding must be verified on a signed device profile with the `packet-tunnel-provider` capability.
