import Foundation

#if canImport(TpMobileFfi)
import TpMobileFfi
#endif

public struct TunnelProxyNativeBridge {
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
