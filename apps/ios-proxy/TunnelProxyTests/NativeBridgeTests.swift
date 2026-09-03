import XCTest

@testable import TunnelProxy

final class NativeBridgeTests: XCTestCase {
    func testStatusJSONReportsBridgeState() throws {
        let bridge = TunnelProxyNativeBridge()
        let status = try Self.object(from: bridge.statusJSON())

        XCTAssertEqual(status["running"] as? Bool, false)
        if bridge.isAvailable {
            XCTAssertNotNil(status["native_version"] as? String)
        } else {
            XCTAssertEqual(status["bridge_available"] as? Bool, false)
            XCTAssertEqual(status["message"] as? String, TunnelProxyNativeBridge.unavailableMessage)

            let lastError = try XCTUnwrap(status["last_error"] as? [String: Any])
            XCTAssertEqual(lastError["code"] as? Int, Int(TunnelProxyNativeBridge.startFailed))
            XCTAssertEqual(lastError["error"] as? String, TunnelProxyNativeBridge.unavailableMessage)
        }
    }

    func testStartProxyRejectsInvalidRequest() {
        let bridge = TunnelProxyNativeBridge()

        let code = bridge.startProxy(requestJSON: "{not json")
        XCTAssertEqual(
            code,
            bridge.isAvailable ? TunnelProxyNativeBridge.invalidJSON : TunnelProxyNativeBridge.startFailed
        )
    }

    func testRuntimeConfigJSONReturnsRustStyleError() throws {
        let bridge = TunnelProxyNativeBridge()
        let error = try Self.object(from: bridge.runtimeConfigJSON())

        XCTAssertEqual(error["ok"] as? Bool, false)
        XCTAssertEqual(error["code"] as? Int, Int(TunnelProxyNativeBridge.startFailed))
        if bridge.isAvailable {
            XCTAssertTrue((error["error"] as? String)?.contains("mobile proxy is not running") == true)
        } else {
            XCTAssertEqual(error["error"] as? String, TunnelProxyNativeBridge.unavailableMessage)
        }
    }

    func testLogsJSONIsArray() throws {
        let bridge = TunnelProxyNativeBridge()
        let data = try XCTUnwrap(bridge.logsJSON(limit: 10).data(using: .utf8))
        let object = try JSONSerialization.jsonObject(with: data)
        let logs = try XCTUnwrap(object as? [String])

        if bridge.isAvailable {
            XCTAssertGreaterThanOrEqual(logs.count, 0)
        } else {
            XCTAssertEqual(logs.count, 1)
            XCTAssertTrue(logs[0].contains(TunnelProxyNativeBridge.unavailableMessage))
        }
    }

    private static func object(from json: String) throws -> [String: Any] {
        let data = try XCTUnwrap(json.data(using: .utf8))
        let object = try JSONSerialization.jsonObject(with: data)
        return try XCTUnwrap(object as? [String: Any])
    }
}
