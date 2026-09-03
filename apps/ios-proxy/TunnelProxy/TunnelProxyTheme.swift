import SwiftUI

/// The few tokens a native surface still needs.
///
/// The type ramp and every component moved into the shared UI bundle, which is
/// what stopped this file, `MainActivity.kt` and `index.css` from each holding
/// their own answer. What is left is the canvas the WebView sits on and the
/// colours the camera sheet draws with.
enum TunnelProxyTheme {
    static let background = Color(red: 248 / 255, green: 250 / 255, blue: 252 / 255)  // canvas
    static let surface = Color(red: 255 / 255, green: 255 / 255, blue: 255 / 255)     // surface
    static let surfaceAlt = Color(red: 241 / 255, green: 245 / 255, blue: 249 / 255)  // surface-subdued
    static let button = Color(red: 241 / 255, green: 245 / 255, blue: 249 / 255)
    static let buttonBorder = Color(red: 226 / 255, green: 232 / 255, blue: 240 / 255)
    static let input = Color(red: 255 / 255, green: 255 / 255, blue: 255 / 255)
    static let border = Color(red: 226 / 255, green: 232 / 255, blue: 240 / 255)      // border
    static let text = Color(red: 15 / 255, green: 23 / 255, blue: 42 / 255)           // text-primary
    static let secondaryText = Color(red: 71 / 255, green: 85 / 255, blue: 105 / 255) // text-secondary
    static let muted = Color(red: 100 / 255, green: 116 / 255, blue: 139 / 255)       // text-muted
    static let onAccent = Color(red: 255 / 255, green: 255 / 255, blue: 255 / 255)    // text-inverse
    static let primary = Color(red: 109 / 255, green: 40 / 255, blue: 217 / 255)      // accent
    static let accentSoft = Color(red: 245 / 255, green: 243 / 255, blue: 255 / 255)  // accent-soft
    static let tabActive = Color(red: 109 / 255, green: 40 / 255, blue: 217 / 255)
    static let secondary = Color(red: 124 / 255, green: 58 / 255, blue: 237 / 255)    // focus
    static let success = Color(red: 4 / 255, green: 120 / 255, blue: 87 / 255)        // status.success
    static let warning = Color(red: 180 / 255, green: 83 / 255, blue: 9 / 255)        // status.warning
    static let danger = Color(red: 185 / 255, green: 28 / 255, blue: 28 / 255)        // status.danger

    static let horizontalPadding: CGFloat = 18
    static let cardRadius: CGFloat = 14
    static let controlRadius: CGFloat = 10
}
