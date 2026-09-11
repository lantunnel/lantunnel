import Foundation

#if canImport(TpMobileFfi)
import TpMobileFfi
#endif

/// Stateless: every method is one call into Rust, so this crosses actors freely.
public struct TunnelProxyNativeBridge: Sendable {
    public static let ok: Int32 = 0
    public static let invalidArgument: Int32 = -1
    public static let invalidJSON: Int32 = -2
    public static let invalidConfig: Int32 = -3
    public static let alreadyRunning: Int32 = -4
    public static let startFailed: Int32 = -5

    public static let unavailableMessage = "Rust/tp-mobile-ffi native bridge is not linked in this build"

    public init() {}

    public var isAvailable: Bool {
        #if canImport(TpMobileFfi)
        return true
        #else
        return false
        #endif
    }

    public func startProxy(requestJSON: String) -> Int32 {
        #if canImport(TpMobileFfi)
        return requestJSON.withCString { pointer in
            tp_mobile_start_proxy(pointer)
        }
        #else
        _ = requestJSON
        return Self.startFailed
        #endif
    }

    public func stopProxy() -> Int32 {
        #if canImport(TpMobileFfi)
        return tp_mobile_stop_proxy()
        #else
        Self.startFailed
        #endif
    }

    public func statusJSON() -> String {
        #if canImport(TpMobileFfi)
        return Self.nativeString(tp_mobile_status_json)
        #else
        Self.jsonObject([
            "running": false,
            "bridge_available": false,
            "bridge": "tp-mobile-ffi",
            "native_version": NSNull(),
            "listen_addr": NSNull(),
            "connection": NSNull(),
            "p2p": NSNull(),
            "clash_overlay_available": false,
            "startup": NSNull(),
            "last_error": [
                "code": Int(Self.startFailed),
                "error": Self.unavailableMessage,
            ],
            "message": Self.unavailableMessage,
        ])
        #endif
    }

    public func logsJSON(limit: Int) -> String {
        #if canImport(TpMobileFfi)
        return Self.nativeString {
            tp_mobile_logs_json(max(limit, 0))
        }
        #else
        guard limit != 0 else {
            return "[]"
        }

        return Self.jsonArray([
            "native bridge unavailable: \(Self.unavailableMessage)",
        ])
        #endif
    }

    public func setLogLevel(_ level: String) -> Int32 {
        #if canImport(TpMobileFfi)
        return level.withCString { pointer in
            tp_mobile_set_log_level(pointer)
        }
        #else
        _ = level
        return Self.startFailed
        #endif
    }

    public func clearLogs() -> Int32 {
        #if canImport(TpMobileFfi)
        return tp_mobile_clear_logs()
        #else
        return Self.startFailed
        #endif
    }

    public func logConfigJSON() -> String {
        #if canImport(TpMobileFfi)
        return Self.nativeString(tp_mobile_log_config_json)
        #else
        return Self.errorJSON()
        #endif
    }

    public func clashOverlayYAML() -> String {
        #if canImport(TpMobileFfi)
        return Self.nativeString(tp_mobile_clash_overlay_yaml)
        #else
        return ""
        #endif
    }

    public func runtimeConfigJSON() -> String {
        #if canImport(TpMobileFfi)
        return Self.nativeString(tp_mobile_runtime_config_json)
        #else
        Self.errorJSON()
        #endif
    }

    // MARK: - Platform account

    /// Each of these blocks on one HTTP round trip, so callers must be off the
    /// main thread. Nothing here retains the token: the Keychain holds it and
    /// it is passed back in, so signing out is one delete rather than two.
    public func platformStartSignIn(platformURL: String) -> String {
        #if canImport(TpMobileFfi)
        return platformURL.withCString { url in
            Self.nativeString { tp_mobile_platform_start_sign_in(url) }
        }
        #else
        _ = platformURL
        return Self.errorJSON()
        #endif
    }

    public func platformPollSignIn(platformURL: String, deviceCode: String) -> String {
        #if canImport(TpMobileFfi)
        return platformURL.withCString { url in
            deviceCode.withCString { code in
                Self.nativeString { tp_mobile_platform_poll_sign_in(url, code) }
            }
        }
        #else
        _ = (platformURL, deviceCode)
        return Self.errorJSON()
        #endif
    }

    public func platformListTunnels(
        platformURL: String,
        accessToken: String,
        expiresAtUnix: UInt64
    ) -> String {
        #if canImport(TpMobileFfi)
        return platformURL.withCString { url in
            accessToken.withCString { token in
                Self.nativeString { tp_mobile_platform_list_tunnels(url, token, expiresAtUnix) }
            }
        }
        #else
        _ = (platformURL, accessToken, expiresAtUnix)
        return Self.errorJSON()
        #endif
    }

    public func platformListPeers(
        platformURL: String,
        accessToken: String,
        expiresAtUnix: UInt64,
        tunnelID: String
    ) -> String {
        #if canImport(TpMobileFfi)
        return platformURL.withCString { url in
            accessToken.withCString { token in
                tunnelID.withCString { tunnel in
                    Self.nativeString {
                        tp_mobile_platform_list_peers(url, token, expiresAtUnix, tunnel)
                    }
                }
            }
        }
        #else
        _ = (platformURL, accessToken, expiresAtUnix, tunnelID)
        return Self.errorJSON()
        #endif
    }

    public func platformImportPeer(
        platformURL: String,
        accessToken: String,
        expiresAtUnix: UInt64,
        tunnelID: String,
        peerID: String
    ) -> String {
        #if canImport(TpMobileFfi)
        return platformURL.withCString { url in
            accessToken.withCString { token in
                tunnelID.withCString { tunnel in
                    peerID.withCString { peer in
                        Self.nativeString {
                            tp_mobile_platform_import_peer(
                                url, token, expiresAtUnix, tunnel, peer
                            )
                        }
                    }
                }
            }
        }
        #else
        _ = (platformURL, accessToken, expiresAtUnix, tunnelID, peerID)
        return Self.errorJSON()
        #endif
    }

    public func platformCreatePeer(
        platformURL: String,
        accessToken: String,
        expiresAtUnix: UInt64,
        tunnelID: String,
        name: String
    ) -> String {
        #if canImport(TpMobileFfi)
        return platformURL.withCString { url in
            accessToken.withCString { token in
                tunnelID.withCString { tunnel in
                    name.withCString { peerName in
                        Self.nativeString {
                            tp_mobile_platform_create_peer(
                                url, token, expiresAtUnix, tunnel, peerName
                            )
                        }
                    }
                }
            }
        }
        #else
        _ = (platformURL, accessToken, expiresAtUnix, tunnelID, name)
        return Self.errorJSON()
        #endif
    }

    #if canImport(TpMobileFfi)
    private static func nativeString(_ producer: () -> UnsafeMutablePointer<CChar>?) -> String {
        guard let pointer = producer() else {
            return errorJSON()
        }
        defer {
            tp_mobile_free_string(pointer)
        }
        return String(cString: pointer)
    }
    #endif

    private static func errorJSON() -> String {
        jsonObject([
            "ok": false,
            "code": Int(startFailed),
            "error": unavailableMessage,
        ])
    }

    private static func jsonObject(_ object: [String: Any]) -> String {
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              let json = String(data: data, encoding: .utf8)
        else {
            return #"{"ok":false,"code":-5,"error":"Rust/tp-mobile-ffi native bridge is not linked in this build"}"#
        }

        return json
    }

    private static func jsonArray(_ array: [String]) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: array, options: []),
              let json = String(data: data, encoding: .utf8)
        else {
            return "[]"
        }

        return json
    }
}
