import XCTest

@testable import TunnelProxy

final class PacketTunnelLastStartErrorStoreTests: XCTestCase {
    func testLastStartErrorRoundTripsThroughInjectedDefaults() {
        let suiteName = "PacketTunnelLastStartErrorStoreTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }
        let store = PacketTunnelLastStartErrorStore(defaults: defaults)
        let date = Date(timeIntervalSince1970: 1_000)

        store.record("native bridge failed", at: date)

        let entry = store.load()
        XCTAssertEqual(entry?.message, "native bridge failed")
        XCTAssertEqual(entry?.recordedAt, date)
    }

    @MainActor
    func testTunnelControlLogsIncludeLastPacketTunnelStartErrorWhenProviderLogsAreUnavailable() async {
        let suiteName = "PacketTunnelLastStartErrorStoreTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }
        let store = PacketTunnelLastStartErrorStore(defaults: defaults)
        store.record("setTunnelNetworkSettings failed", at: Date(timeIntervalSince1970: 2_000))
        let service = TunnelControlService(lastStartErrorStore: store)

        let logs = await service.logs(limit: 20)

        XCTAssertTrue(logs.contains { entry in
            entry.subsystem == "provider"
                && entry.level == .error
                && entry.message.contains("setTunnelNetworkSettings failed")
        })
    }

    @MainActor
    func testTunnelControlClearLogsClearsLastPacketTunnelStartError() async {
        let suiteName = "PacketTunnelLastStartErrorStoreTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }
        let store = PacketTunnelLastStartErrorStore(defaults: defaults)
        store.record("native startup failed", at: Date(timeIntervalSince1970: 3_000))
        let service = TunnelControlService(lastStartErrorStore: store)

        await service.clearLogs()

        XCTAssertNil(store.load())
    }
}
