import XCTest
@preconcurrency import NetworkExtension

@testable import TunnelProxy

final class TunnelServiceStateTests: XCTestCase {
    func testTransientStateExpiresAfterThirtySeconds() {
        let startedAt = Date(timeIntervalSince1970: 1_000)
        let state = TunnelServiceState(
            phase: .connecting,
            message: "Connecting",
            updatedAt: startedAt
        )

        let recovered = state.recovered(now: startedAt.addingTimeInterval(31))

        XCTAssertEqual(recovered.phase, .stopped)
        XCTAssertEqual(recovered.message, "Previous connecting state expired")
    }

    func testPersistedRunningStateRestoresStatus() {
        let state = TunnelServiceState(
            phase: .running,
            message: "Connected",
            updatedAt: Date(timeIntervalSince1970: 2_000)
        )

        guard case .connected(let details) = state.tunnelStatus() else {
            XCTFail("Expected connected tunnel status")
            return
        }

        XCTAssertEqual(details.transport, "NetworkExtension")
        XCTAssertEqual(details.lastHeartbeatAt, state.updatedAt)
    }

    func testServiceStateRoundTripsThroughUserDefaults() {
        let suiteName = "TunnelServiceStateTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }

        let state = TunnelServiceState(
            phase: .failed,
            message: "boom",
            updatedAt: Date(timeIntervalSince1970: 3_000)
        )
        TunnelAppModel.persistServiceState(state, defaults: defaults)

        let loaded = TunnelAppModel.loadPersistedServiceState(
            defaults: defaults,
            now: Date(timeIntervalSince1970: 3_005)
        )

        XCTAssertEqual(loaded, state)
    }

    @MainActor
    func testLanP2pSettingIsLockedWhileConnected() {
        let model = TunnelAppModel(config: MobileConfig(), status: .connected(TunnelConnectionDetails()))

        XCTAssertFalse(model.canEditLanP2pSetting)
    }

    @MainActor
    func testLanP2pSettingIsEditableWhenDisconnectedAndIdle() {
        let model = TunnelAppModel(config: MobileConfig(), status: .disconnected)

        XCTAssertTrue(model.canEditLanP2pSetting)
    }

    @MainActor
    func testLanP2pSettingIsLockedWhileBusy() {
        let model = TunnelAppModel(config: MobileConfig(), status: .disconnected)
        model.isBusy = true

        XCTAssertFalse(model.canEditLanP2pSetting)
    }

    @MainActor
    func testRefreshStatusKeepsMissingCredentialsDisconnectedWhenProviderIPCFails() async {
        let config = MobileConfig()
        let service = StubTunnelControlService(statusSnapshot: TunnelControlSnapshot(
            rawStatus: .failed("IPC failed"),
            presentation: .failed(routeCount: config.lanRoutes.count, message: "IPC failed")
        ))
        let model = TunnelAppModel(config: config, status: .disconnected, controlService: service)

        await model.refreshStatus()

        XCTAssertEqual(model.statusPresentation.phase, .disconnected)
        XCTAssertEqual(model.statusPresentation.title, "Disconnected")
        XCTAssertNil(model.statusPresentation.message)
    }

    @MainActor
    func testConnectKeepsConnectingWhenAcceptedStartReportsInitialDisconnectedStatus() async {
        clearStandardModelPersistence()
        defer {
            clearStandardModelPersistence()
        }

        let config = validConfig()
        let service = StubTunnelControlService(startSnapshot: TunnelControlSnapshot(
            rawStatus: .disconnected,
            presentation: .disconnected(routeCount: config.lanRoutes.count)
        ))
        let model = TunnelAppModel(
            config: config,
            status: .disconnected,
            controlService: service,
            startStatusPollAttempts: 1,
            startStatusPollDelayNanoseconds: 0
        )

        await model.connect()

        XCTAssertEqual(model.statusPresentation.phase, .connecting)
        XCTAssertEqual(model.statusPresentation.message, "Connecting")
        XCTAssertFalse(model.canConnect)
        XCTAssertTrue(model.canDisconnect)
        XCTAssertTrue(model.logs.contains { entry in
            entry.message == "VPN start accepted; waiting for iOS status to update"
        })
    }

    @MainActor
    func testConnectPollsStatusAfterAcceptedStartReportsInitialDisconnectedStatus() async {
        clearStandardModelPersistence()
        defer {
            clearStandardModelPersistence()
        }

        let config = validConfig()
        let connected = TunnelControlSnapshot(
            rawStatus: .connected(TunnelConnectionDetails(transport: "NetworkExtension")),
            presentation: .connected(routeCount: config.lanRoutes.count, message: "Connected")
        )
        let service = StubTunnelControlService(
            startSnapshot: TunnelControlSnapshot(
                rawStatus: .disconnected,
                presentation: .disconnected(routeCount: config.lanRoutes.count)
            ),
            statusSnapshots: [connected]
        )
        let model = TunnelAppModel(
            config: config,
            status: .disconnected,
            controlService: service,
            startStatusPollAttempts: 1,
            startStatusPollDelayNanoseconds: 0
        )

        await model.connect()

        XCTAssertEqual(service.statusCallCount, 1)
        XCTAssertEqual(model.statusPresentation.phase, .connected)
        XCTAssertEqual(model.statusPresentation.message, "Connected")
    }

    @MainActor
    func testRefreshStatusDoesNotOverwriteFreshConnectingStateWithInitialDisconnectedStatus() async {
        clearStandardModelPersistence()
        defer {
            clearStandardModelPersistence()
        }

        let config = validConfig()
        let service = StubTunnelControlService(startSnapshot: TunnelControlSnapshot(
            rawStatus: .disconnected,
            presentation: .disconnected(routeCount: config.lanRoutes.count)
        ))
        let model = TunnelAppModel(
            config: config,
            status: .disconnected,
            controlService: service,
            startStatusPollAttempts: 0,
            startStatusPollDelayNanoseconds: 0
        )

        await model.connect()
        await model.refreshStatus()

        XCTAssertEqual(model.statusPresentation.phase, .connecting)
        XCTAssertEqual(model.statusPresentation.message, "Connecting")
    }

    @MainActor
    func testSetLogLevelExplainsWhenProviderLogsAreUnavailable() async {
        let service = StubTunnelControlService(logLevelTarget: .app)
        let model = TunnelAppModel(
            config: validConfig(),
            status: .disconnected,
            controlService: service
        )

        await model.setLogLevel("trace")

        XCTAssertEqual(model.logLevel, "trace")
        XCTAssertTrue(model.logs.contains { entry in
            entry.message == "App log level set to trace; VPN provider logs are available while the VPN is starting or connected"
        })
    }

    @MainActor
    func testRefreshLogsUsesAndroidDefaultLimit() async {
        let service = StubTunnelControlService()
        let model = TunnelAppModel(
            config: validConfig(),
            status: .disconnected,
            controlService: service
        )

        await model.refreshLogs()

        XCTAssertEqual(service.lastLogsLimit, 500)
    }

    @MainActor
    func testConnectingNetworkExtensionUsesProviderConnectedRuntimeStatus() {
        let providerStatusJSON = """
        {
          "running": true,
          "native_status": {
            "running": true,
            "connection": {
              "connected": true,
              "connecting": false,
              "path_mode": "relay",
              "p2p_primary_peer_id": "peer-client-1",
              "platform_heartbeat": { "active": true },
              "transport_heartbeat": { "active": true },
              "uptime_secs": 12,
              "traffic": {
                "relay_tx_bytes": 1024,
                "relay_rx_bytes": 2048
              }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connecting,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(snapshot.presentation.phase, .connected)
        XCTAssertEqual(snapshot.presentation.pathMode, "Relay")
        XCTAssertEqual(snapshot.presentation.p2pTraffic, "0 B ↑ / 0 B ↓")
        XCTAssertEqual(snapshot.presentation.relayTraffic, "1.0 KB ↑ / 2.0 KB ↓")
        XCTAssertEqual(snapshot.presentation.platformHeartbeat, "Active")
        XCTAssertEqual(snapshot.presentation.transportHeartbeat, "Active")
        guard case .connected(let details) = snapshot.rawStatus else {
            XCTFail("Expected raw status to be connected")
            return
        }
        XCTAssertEqual(details.pathMode, .relay)
        XCTAssertEqual(details.peerClientId, "peer-client-1")
    }

    @MainActor
    func testConnectedProviderStatusUsesTheSharedPathAndPeerLabels() {
        let providerStatusJSON = """
        {
          "running": true,
          "native_status": {
            "running": true,
            "connection": {
              "connected": true,
              "connecting": false,
              "path_mode": "p2p",
              "p2p_state": "running",
              "p2p_peer_count": 3,
              "p2p_primary_peer_id": "client-abcd1234-0",
              "traffic": {
                "p2p_tx_bytes": 4096,
                "p2p_rx_bytes": 8192,
                "relay_tx_bytes": 1024,
                "relay_rx_bytes": 2048
              },
              "platform_heartbeat": { "active": true },
              "transport_heartbeat": { "active": true }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connected,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        // "Direct" is the V2 word for this path, on every screen and every
        // Client. "3 devices" replaced a Peer ID run through a formatter that
        // expects `name-xxxxxxxx-0`, which rendered every UUID as "Unknown" —
        // this assertion was pinning that bug in place.
        XCTAssertEqual(snapshot.presentation.pathMode, "Direct")
        XCTAssertEqual(snapshot.presentation.peer, "3 devices")
        XCTAssertEqual(snapshot.presentation.p2pTraffic, "4.0 KB ↑ / 8.0 KB ↓")
        XCTAssertEqual(snapshot.presentation.relayTraffic, "1.0 KB ↑ / 2.0 KB ↓")
    }

    @MainActor
    func testConnectedProviderStatusReadsFlatTrafficCountersFromConnection() {
        let providerStatusJSON = """
        {
          "running": true,
          "native_status": {
            "running": true,
            "connection": {
              "connected": true,
              "connecting": false,
              "path_mode": "p2p",
              "p2p_state": "running",
              "p2p_tx_bytes": 4096,
              "p2p_rx_bytes": 8192,
              "relay_tx_bytes": 1024,
              "relay_rx_bytes": 2048,
              "platform_heartbeat": { "active": true },
              "transport_heartbeat": { "active": true }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connected,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(snapshot.presentation.p2pTraffic, "4.0 KB ↑ / 8.0 KB ↓")
        XCTAssertEqual(snapshot.presentation.relayTraffic, "1.0 KB ↑ / 2.0 KB ↓")
    }

    @MainActor
    func testConnectedProviderStatusFallsBackToPacketBridgeTrafficForActivePath() {
        let providerStatusJSON = """
        {
          "running": true,
          "packet_bridge": {
            "bytes_to_tun2socks": 4096,
            "bytes_from_tun2socks": 8192
          },
          "native_status": {
            "running": true,
            "connection": {
              "connected": true,
              "connecting": false,
              "path_mode": "p2p",
              "traffic": {
                "p2p_tx_bytes": 0,
                "p2p_rx_bytes": 0,
                "relay_tx_bytes": 0,
                "relay_rx_bytes": 0
              },
              "platform_heartbeat": { "active": true },
              "transport_heartbeat": { "active": true }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connected,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(snapshot.presentation.p2pTraffic, "4.0 KB ↑ / 8.0 KB ↓")
        XCTAssertEqual(snapshot.presentation.relayTraffic, "0 B ↑ / 0 B ↓")
    }

    @MainActor
    func testConnectedProviderStatusFallsBackToPacketBridgeTrafficForRelayPath() {
        let providerStatusJSON = """
        {
          "running": true,
          "packet_bridge": {
            "bytes_to_tun2socks": 1024,
            "bytes_from_tun2socks": 2048
          },
          "native_status": {
            "running": true,
            "connection": {
              "connected": true,
              "connecting": false,
              "path_mode": "relay",
              "traffic": {
                "p2p_tx_bytes": 0,
                "p2p_rx_bytes": 0,
                "relay_tx_bytes": 0,
                "relay_rx_bytes": 0
              },
              "platform_heartbeat": { "active": true },
              "transport_heartbeat": { "active": true }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connected,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(snapshot.presentation.p2pTraffic, "0 B ↑ / 0 B ↓")
        XCTAssertEqual(snapshot.presentation.relayTraffic, "1.0 KB ↑ / 2.0 KB ↓")
    }

    @MainActor
    func testConnectedProviderStatusFallsBackToPacketBridgeTrafficWhenOtherPathHasCounters() {
        let providerStatusJSON = """
        {
          "running": true,
          "packet_bridge": {
            "bytes_to_tun2socks": 16384,
            "bytes_from_tun2socks": 32768
          },
          "native_status": {
            "running": true,
            "connection": {
              "connected": true,
              "connecting": false,
              "path_mode": "p2p",
              "traffic": {
                "p2p_tx_bytes": 0,
                "p2p_rx_bytes": 0,
                "relay_tx_bytes": 1024,
                "relay_rx_bytes": 2048
              },
              "platform_heartbeat": { "active": true },
              "transport_heartbeat": { "active": true }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connected,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(snapshot.presentation.p2pTraffic, "16.0 KB ↑ / 32.0 KB ↓")
        XCTAssertEqual(snapshot.presentation.relayTraffic, "1.0 KB ↑ / 2.0 KB ↓")
    }

    func testProviderStatusNormalizationMergesPacketBridgeTrafficIntoAndroidConnectionTraffic() throws {
        let providerStatus = try Self.object(from: """
        {
          "running": true,
          "packet_bridge": {
            "bytes_to_tun2socks": 16384,
            "bytes_from_tun2socks": 32768
          },
          "native_status": {
            "connection": {
              "connected": true,
              "path_mode": "p2p",
              "traffic": {
                "p2p_tx_bytes": 0,
                "p2p_rx_bytes": 0,
                "relay_tx_bytes": 1024,
                "relay_rx_bytes": 2048
              }
            }
          }
        }
        """)

        let normalized = MobileTrafficStatusNormalizer.normalizedProviderStatus(providerStatus)
        let nativeStatus = try XCTUnwrap(normalized["native_status"] as? [String: Any])
        let connection = try XCTUnwrap(nativeStatus["connection"] as? [String: Any])
        let traffic = try XCTUnwrap(connection["traffic"] as? [String: Any])

        XCTAssertEqual(traffic["p2p_tx_bytes"] as? Int64, 16384)
        XCTAssertEqual(traffic["p2p_rx_bytes"] as? Int64, 32768)
        XCTAssertEqual(traffic["relay_tx_bytes"] as? Int64, 1024)
        XCTAssertEqual(traffic["relay_rx_bytes"] as? Int64, 2048)
    }

    func testProviderStatusNormalizationMergesTun2SocksTrafficIntoAndroidConnectionTraffic() throws {
        let providerStatus = try Self.object(from: """
        {
          "running": true,
          "packet_bridge": {
            "bytes_to_tun2socks": 0,
            "bytes_from_tun2socks": 0
          },
          "tun2socks_stats": {
            "tx_packets": 4,
            "tx_bytes": 16384,
            "rx_packets": 8,
            "rx_bytes": 32800,
            "rx_payload_bytes": 32768
          },
          "native_status": {
            "connection": {
              "connected": true,
              "path_mode": "p2p",
              "traffic": {
                "p2p_tx_bytes": 0,
                "p2p_rx_bytes": 0,
                "relay_tx_bytes": 1024,
                "relay_rx_bytes": 2048
              }
            }
          }
        }
        """)

        let normalized = MobileTrafficStatusNormalizer.normalizedProviderStatus(providerStatus)
        let nativeStatus = try XCTUnwrap(normalized["native_status"] as? [String: Any])
        let connection = try XCTUnwrap(nativeStatus["connection"] as? [String: Any])
        let traffic = try XCTUnwrap(connection["traffic"] as? [String: Any])

        XCTAssertEqual(traffic["p2p_tx_bytes"] as? Int64, 16384)
        XCTAssertEqual(traffic["p2p_rx_bytes"] as? Int64, 32768)
        XCTAssertEqual(traffic["relay_tx_bytes"] as? Int64, 1024)
        XCTAssertEqual(traffic["relay_rx_bytes"] as? Int64, 2048)
    }

    @MainActor
    func testConnectingNetworkExtensionStaysConnectingWhenProviderRuntimeIsStillStarting() {
        let providerStatusJSON = """
        {
          "running": true,
          "native_status": {
            "running": true,
            "connection": {
              "connected": false,
              "connecting": true,
              "path_mode": "connecting",
              "platform_heartbeat": { "active": false },
              "transport_heartbeat": { "active": false }
            }
          }
        }
        """

        let snapshot = TunnelControlService.makeSnapshot(
            for: .connecting,
            routeCount: 1,
            providerStatusJSON: providerStatusJSON,
            now: Date(timeIntervalSince1970: 1_000)
        )

        XCTAssertEqual(snapshot.presentation.phase, .connecting)
        // The desktop calls this phase Starting; the phase enum keeps its name.
        XCTAssertEqual(snapshot.presentation.title, "Starting")
    }

    @MainActor
    func testActiveRefreshPullsLogsWhileLogsTabIsVisible() async {
        let config = validConfig()
        let logEntry = MobileLogEntry(level: .info, message: "provider tick", subsystem: "provider")
        let service = StubTunnelControlService(
            statusSnapshot: TunnelControlSnapshot(
                rawStatus: .connected(TunnelConnectionDetails(transport: "NetworkExtension")),
                presentation: .connected(routeCount: config.lanRoutes.count)
            ),
            logResponses: [[logEntry]]
        )
        let model = TunnelAppModel(
            config: config,
            status: .connected(TunnelConnectionDetails()),
            selectedTab: .logs,
            controlService: service
        )

        await model.refreshActiveView()

        XCTAssertEqual(service.statusCallCount, 1)
        XCTAssertEqual(service.logsCallCount, 1)
        XCTAssertEqual(model.logs.map(\.message), ["provider tick"])
    }

    @MainActor
    func testActiveRefreshThrottlesLogsWhileLogsTabIsVisible() async {
        let config = validConfig()
        var now = Date(timeIntervalSince1970: 1_000)
        let service = StubTunnelControlService(
            statusSnapshot: TunnelControlSnapshot(
                rawStatus: .connected(TunnelConnectionDetails(transport: "NetworkExtension")),
                presentation: .connected(routeCount: config.lanRoutes.count)
            ),
            logResponses: [
                [MobileLogEntry(level: .info, message: "first", subsystem: "provider")],
                [MobileLogEntry(level: .info, message: "second", subsystem: "provider")],
            ]
        )
        let model = TunnelAppModel(
            config: config,
            status: .connected(TunnelConnectionDetails()),
            selectedTab: .logs,
            controlService: service,
            clock: { now }
        )

        await model.refreshActiveView()
        await model.refreshActiveView()
        now = now.addingTimeInterval(6)
        await model.refreshActiveView()

        XCTAssertEqual(service.statusCallCount, 3)
        XCTAssertEqual(service.logsCallCount, 2)
        XCTAssertEqual(model.logs.map(\.message), ["second"])
    }

    @MainActor
    func testRefreshLogsIfStaleSkipsFreshLogs() async {
        var now = Date(timeIntervalSince1970: 1_000)
        let service = StubTunnelControlService(
            logResponses: [
                [MobileLogEntry(level: .info, message: "first", subsystem: "provider")],
                [MobileLogEntry(level: .info, message: "second", subsystem: "provider")],
            ]
        )
        let model = TunnelAppModel(
            config: validConfig(),
            status: .connected(TunnelConnectionDetails()),
            selectedTab: .logs,
            controlService: service,
            clock: { now }
        )

        await model.refreshLogs()
        await model.refreshLogsIfStale()
        now = now.addingTimeInterval(6)
        await model.refreshLogsIfStale()

        XCTAssertEqual(service.logsCallCount, 2)
        XCTAssertEqual(model.logs.map(\.message), ["second"])
    }

    @MainActor
    func testRefreshLogsSyncsNativeLogLevel() async {
        let service = StubTunnelControlService(
            logResponses: [[
                MobileLogEntry(
                    level: .info,
                    message: #"Native log config: {"level":"trace"}"#,
                    subsystem: "provider"
                ),
            ]]
        )
        let model = TunnelAppModel(
            config: validConfig(),
            status: .connected(TunnelConnectionDetails()),
            controlService: service
        )

        await model.refreshLogs()

        XCTAssertEqual(model.logLevel, "trace")
    }

    @MainActor
    func testLogTextUsesAndroidSections() async {
        let service = StubTunnelControlService(
            logResponses: [[
                MobileLogEntry(level: .info, message: "Native status: {\"running\":true}", subsystem: "provider"),
                MobileLogEntry(level: .info, message: #"Native log config: {"level":"debug"}"#, subsystem: "provider"),
                MobileLogEntry(level: .info, message: "[10:00:00] INFO provider ready", subsystem: "provider"),
            ]]
        )
        let logStore = MobileLogStore(entries: [
            MobileLogEntry(level: .info, message: "VPN connect requested", subsystem: "app"),
        ])
        let model = TunnelAppModel(
            config: validConfig(),
            status: .connected(TunnelConnectionDetails()),
            controlService: service,
            logStore: logStore
        )

        await model.refreshLogs()

        XCTAssertEqual(
            model.logText,
            """
            Native status:
            {\"running\":true}

            VPN connect requested

            Native log config:
            {\"level\":\"debug\"}

            Native logs:
            [10:00:00] INFO provider ready
            """
        )
    }

    @MainActor
    func testRefreshStatusClearsStaleConnectingBannerAfterConnected() async {
        let config = validConfig()
        let service = StubTunnelControlService(statusSnapshot: TunnelControlSnapshot(
            rawStatus: .connected(TunnelConnectionDetails(transport: "NetworkExtension")),
            presentation: .connected(routeCount: config.lanRoutes.count)
        ))
        let model = TunnelAppModel(
            config: config,
            status: .connecting,
            controlService: service
        )
        model.bannerMessage = "Connecting"

        await model.refreshStatus()

        XCTAssertEqual(model.statusPresentation.phase, .connected)
        XCTAssertNil(model.bannerMessage)
    }

    private func validConfig() -> MobileConfig {
        var config = MobileConfig()
        config.peerProfileJSON = #"{"version":2,"tunnel_id":"test-tunnel","peer":{"peer_id":"test-peer"}}"#
        config.lanRoutes = ["192.168.0.0/16"]
        return config
    }

    private func clearStandardModelPersistence() {
        [
            "TunnelProxy.config.peerProfile",
            "TunnelProxy.config.deviceID",
            "TunnelProxy.config.localSocks5Listen",
            "TunnelProxy.config.lanP2pEnabled",
            "TunnelProxy.config.lanRoutes",
            "TunnelProxy.serviceState.phase",
            "TunnelProxy.serviceState.message",
            "TunnelProxy.serviceState.updatedAt",
        ].forEach {
            UserDefaults.standard.removeObject(forKey: $0)
        }
        try? KeychainStore().delete("TunnelProxy.config.peerProfile")
    }

    private static func object(from json: String) throws -> [String: Any] {
        let data = try XCTUnwrap(json.data(using: .utf8))
        let object = try JSONSerialization.jsonObject(with: data)
        return try XCTUnwrap(object as? [String: Any])
    }
}

@MainActor
private final class StubTunnelControlService: TunnelControlServicing {
    let statusSnapshot: TunnelControlSnapshot
    let startSnapshot: TunnelControlSnapshot
    private var statusSnapshots: [TunnelControlSnapshot]
    let logLevelTarget: TunnelLogLevelTarget
    private var logResponses: [[MobileLogEntry]]
    private(set) var statusCallCount = 0
    private(set) var logsCallCount = 0
    private(set) var lastLogsLimit: Int?

    init(
        statusSnapshot: TunnelControlSnapshot = TunnelControlSnapshot(
            rawStatus: .disconnected,
            presentation: .disconnected(routeCount: 0)
        ),
        startSnapshot: TunnelControlSnapshot? = nil,
        statusSnapshots: [TunnelControlSnapshot] = [],
        logLevelTarget: TunnelLogLevelTarget = .provider,
        logResponses: [[MobileLogEntry]] = []
    ) {
        self.statusSnapshot = statusSnapshot
        self.startSnapshot = startSnapshot ?? statusSnapshot
        self.statusSnapshots = statusSnapshots
        self.logLevelTarget = logLevelTarget
        self.logResponses = logResponses
    }

    func start(config: MobileConfig, logLevel: String) async throws -> TunnelControlSnapshot {
        startSnapshot
    }

    func stop(config: MobileConfig) async throws -> TunnelControlSnapshot {
        statusSnapshot
    }

    func status(config: MobileConfig) async -> TunnelControlSnapshot {
        statusCallCount += 1
        if !statusSnapshots.isEmpty {
            return statusSnapshots.removeFirst()
        }
        return statusSnapshot
    }

    func logs(limit: Int) async -> [MobileLogEntry] {
        logsCallCount += 1
        lastLogsLimit = limit
        if !logResponses.isEmpty {
            return logResponses.removeFirst()
        }
        return []
    }

    func clearLogs() async {}

    func setLogLevel(_ level: String) async -> TunnelLogLevelTarget {
        logLevelTarget
    }
}
