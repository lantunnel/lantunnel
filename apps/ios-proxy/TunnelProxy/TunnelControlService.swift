import Foundation
@preconcurrency import NetworkExtension

#if canImport(TpMobileFfi)
import TpMobileFfi
#endif

struct TunnelControlSnapshot {
    var rawStatus: TunnelStatus
    var presentation: TunnelStatusSnapshot
    /// The runtime runs in the PacketTunnel extension, so this is the only
    /// way the app sees what the Tunnel holds. A bridge built in the app
    /// process answers for a runtime that was never started there.
    var providerStatusJSON: String?
}

enum TunnelLogLevelTarget: Equatable {
    case provider
    case app
}

@MainActor
protocol TunnelControlServicing: AnyObject {
    func start(config: MobileConfig, logLevel: String) async throws -> TunnelControlSnapshot
    func stop(config: MobileConfig) async throws -> TunnelControlSnapshot
    func status(config: MobileConfig) async -> TunnelControlSnapshot
    func logs(limit: Int) async -> [MobileLogEntry]
    func clearLogs() async
    func setLogLevel(_ level: String) async -> TunnelLogLevelTarget
}

enum TunnelControlError: LocalizedError {
    case missingManager
    case startFailed(String)

    var errorDescription: String? {
        switch self {
        case .missingManager:
            return "VPN profile is not available"
        case .startFailed(let message):
            return message
        }
    }
}

@MainActor
final class TunnelControlService: TunnelControlServicing {
    private let providerBundleIdentifier: String
    private let localizedDescription: String
    private let nativeBridge: TunnelProxyNativeBridge
    private let lastStartErrorStore: PacketTunnelLastStartErrorStore

    init(
        providerBundleIdentifier: String = "com.buhuipao.tunnelproxy.ios.PacketTunnel",
        localizedDescription: String = "Lantunnel",
        nativeBridge: TunnelProxyNativeBridge = TunnelProxyNativeBridge(),
        lastStartErrorStore: PacketTunnelLastStartErrorStore = PacketTunnelLastStartErrorStore()
    ) {
        self.providerBundleIdentifier = providerBundleIdentifier
        self.localizedDescription = localizedDescription
        self.nativeBridge = nativeBridge
        self.lastStartErrorStore = lastStartErrorStore
    }

    func start(config: MobileConfig, logLevel: String) async throws -> TunnelControlSnapshot {
        lastStartErrorStore.clear()
        let manager = try await loadOrCreateManager()
        let startJSON = try config.startRequestJSON(logLevel: logLevel)
        let launchConfiguration = PacketTunnelLaunchConfiguration(
            includedRoutes: config.lanRoutes,
            startRequestJSON: startJSON
        )
        let providerProtocol = NETunnelProviderProtocol()
        providerProtocol.providerBundleIdentifier = providerBundleIdentifier
        providerProtocol.serverAddress = localizedDescription
        providerProtocol.providerConfiguration = launchConfiguration.makeProviderConfiguration(extra: [
            "local_socks5_listen": config.localSocks5Listen,
            "lan_p2p_enabled": config.lanP2pEnabled,
        ])

        manager.localizedDescription = localizedDescription
        manager.protocolConfiguration = providerProtocol
        manager.isEnabled = true
        try await manager.saveToPreferencesAsync()
        try await manager.loadFromPreferencesAsync()

        do {
            try manager.connection.startVPNTunnel(options: launchConfiguration.makeStartOptions())
        } catch {
            throw TunnelControlError.startFailed(error.localizedDescription)
        }

        return snapshot(for: manager.connection.status, config: config, message: "Connecting")
    }

    func stop(config: MobileConfig) async throws -> TunnelControlSnapshot {
        guard let manager = try await loadExistingManager() else {
            return TunnelControlSnapshot(
                rawStatus: .disconnected,
                presentation: .disconnected(routeCount: config.lanRoutes.count)
            )
        }
        manager.connection.stopVPNTunnel()
        return TunnelControlSnapshot(
            rawStatus: .disconnected,
            presentation: .disconnected(routeCount: config.lanRoutes.count, message: "Disconnected")
        )
    }

    func status(config: MobileConfig) async -> TunnelControlSnapshot {
        do {
            guard let manager = try await loadExistingManager() else {
                return TunnelControlSnapshot(
                    rawStatus: .disconnected,
                    presentation: .disconnected(routeCount: config.lanRoutes.count)
                )
            }
            let status = manager.connection.status
            if Self.shouldQueryProvider(for: status),
               let providerStatusJSON = try? await sendProviderMessage(command: "status", manager: manager) {
                return snapshot(for: status, config: config, providerStatusJSON: providerStatusJSON)
            }
            return snapshot(for: status, config: config)
        } catch {
            return TunnelControlSnapshot(
                rawStatus: .failed(error.localizedDescription),
                presentation: .failed(routeCount: config.lanRoutes.count, message: error.localizedDescription)
            )
        }
    }

    func logs(limit: Int) async -> [MobileLogEntry] {
        if let manager = try? await loadExistingManager(),
           Self.shouldQueryProvider(for: manager.connection.status) {
            async let statusFetch = sendProviderMessage(command: "status", manager: manager)
            async let logConfigFetch = sendProviderMessage(command: "log_config", manager: manager)
            async let logsFetch = sendProviderMessage(command: "logs", limit: limit, manager: manager)
            let providerStatus = try? await statusFetch
            let logConfig = try? await logConfigFetch
            let raw = try? await logsFetch

            var entries: [MobileLogEntry] = []
            if let providerStatus {
                entries.append(MobileLogEntry(
                    level: .info,
                    message: "Native status: \(providerStatus)",
                    subsystem: "provider"
                ))
            }
            if let logConfig {
                entries.append(MobileLogEntry(
                    level: .info,
                    message: "Native log config: \(logConfig)",
                    subsystem: "provider"
                ))
            }
            if let raw {
                entries.append(contentsOf: Self.logEntries(from: raw, subsystem: "provider"))
                return entries
            }
            if !entries.isEmpty {
                return entries
            }
        }

        let raw = nativeBridge.logsJSON(limit: limit)
        var entries: [MobileLogEntry] = []
        entries.append(MobileLogEntry(
            level: .info,
            message: "Native status: \(nativeBridge.statusJSON())",
            subsystem: "native"
        ))
        entries.append(MobileLogEntry(
            level: .info,
            message: "Native log config: \(nativeBridge.logConfigJSON())",
            subsystem: "native"
        ))
        if let lastStartError = lastStartErrorStore.load() {
            entries.append(MobileLogEntry(
                timestamp: lastStartError.recordedAt,
                level: .error,
                message: "Packet Tunnel start failed: \(lastStartError.message)",
                subsystem: "provider"
            ))
        }
        entries.append(contentsOf: Self.logEntries(from: raw, subsystem: "native"))
        return entries
    }

    func clearLogs() async {
        lastStartErrorStore.clear()
        if let manager = try? await loadExistingManager(),
           Self.shouldQueryProvider(for: manager.connection.status),
           let response = try? await sendProviderMessage(command: "clear_logs", manager: manager),
           Self.providerResponseSucceeded(response) {
            return
        }

        _ = NativeBridgeFFICommands.clearLogs()
    }

    func setLogLevel(_ level: String) async -> TunnelLogLevelTarget {
        if let manager = try? await loadExistingManager(),
           Self.shouldQueryProvider(for: manager.connection.status),
           let response = try? await sendProviderMessage(
            command: "set_log_level",
            level: level,
            manager: manager
           ),
           Self.providerResponseSucceeded(response) {
            return .provider
        }

        _ = nativeBridge.setLogLevel(level)
        return .app
    }

    private func loadOrCreateManager() async throws -> NETunnelProviderManager {
        if let existing = try await loadExistingManager() {
            return existing
        }
        return NETunnelProviderManager()
    }

    private func loadExistingManager() async throws -> NETunnelProviderManager? {
        let managers = try await NETunnelProviderManager.loadAllFromPreferencesAsync()
        return managers.first { manager in
            guard let tunnelProtocol = manager.protocolConfiguration as? NETunnelProviderProtocol else {
                return false
            }
            return tunnelProtocol.providerBundleIdentifier == providerBundleIdentifier
                || manager.localizedDescription == localizedDescription
        }
    }

    private func snapshot(
        for status: NEVPNStatus,
        config: MobileConfig,
        message: String? = nil,
        providerStatusJSON: String? = nil
    ) -> TunnelControlSnapshot {
        Self.makeSnapshot(
            for: status,
            routeCount: config.lanRoutes.count,
            message: message,
            providerStatusJSON: providerStatusJSON
        )
    }

    static func makeSnapshot(
        for status: NEVPNStatus,
        routeCount: Int,
        message: String? = nil,
        providerStatusJSON: String? = nil,
        now: Date = Date()
    ) -> TunnelControlSnapshot {
        let providerStatus = ProviderRuntimeStatus(rawJSON: providerStatusJSON)
        let resolvedMessage = message ?? providerStatus.message
        let rawStatus: TunnelStatus
        let presentation: TunnelStatusSnapshot
        switch status {
        case .connected:
            rawStatus = .connected(TunnelConnectionDetails(
                pathMode: providerStatus.pathModeValue,
                transport: "NetworkExtension",
                peerClientId: providerStatus.peer,
                lastHeartbeatAt: now
            ))
            presentation = .connected(
                routeCount: routeCount,
                pathMode: providerStatus.pathModeLabel,
                peer: providerStatus.peerLabel,
                p2pTraffic: providerStatus.p2pTraffic,
                relayTraffic: providerStatus.relayTraffic,
                relayAllowance: providerStatus.relayAllowance,
                uptime: providerStatus.uptime,
                platformHeartbeat: providerStatus.platformHeartbeat,
                transportHeartbeat: providerStatus.running == false ? "Stopped" : providerStatus.transportHeartbeat,
                subtitle: providerStatus.gatewaySubtitle,
                message: resolvedMessage
            )
        case .connecting where providerStatus.reportsConnected,
             .reasserting where providerStatus.reportsConnected:
            rawStatus = .connected(TunnelConnectionDetails(
                pathMode: providerStatus.pathModeValue,
                transport: "NetworkExtension",
                peerClientId: providerStatus.peer,
                lastHeartbeatAt: now
            ))
            presentation = .connected(
                routeCount: routeCount,
                pathMode: providerStatus.pathModeLabel,
                peer: providerStatus.peerLabel,
                p2pTraffic: providerStatus.p2pTraffic,
                relayTraffic: providerStatus.relayTraffic,
                relayAllowance: providerStatus.relayAllowance,
                uptime: providerStatus.uptime,
                platformHeartbeat: providerStatus.platformHeartbeat,
                transportHeartbeat: providerStatus.running == false ? "Stopped" : providerStatus.transportHeartbeat,
                subtitle: providerStatus.gatewaySubtitle,
                message: resolvedMessage
            )
        case .connecting, .reasserting:
            rawStatus = .connecting
            presentation = .connecting(routeCount: routeCount, message: resolvedMessage ?? "Connecting")
        case .disconnecting:
            rawStatus = .disconnecting
            presentation = .connecting(routeCount: routeCount, message: "Disconnecting")
        case .disconnected, .invalid:
            rawStatus = .disconnected
            presentation = .disconnected(routeCount: routeCount, message: resolvedMessage)
        @unknown default:
            rawStatus = .disconnected
            presentation = .disconnected(routeCount: routeCount, message: resolvedMessage)
        }
        return TunnelControlSnapshot(
            rawStatus: rawStatus,
            presentation: presentation,
            providerStatusJSON: providerStatusJSON
        )
    }

    private func sendProviderMessage(
        command: String,
        limit: Int? = nil,
        level: String? = nil,
        manager: NETunnelProviderManager? = nil
    ) async throws -> String? {
        let resolvedManager: NETunnelProviderManager
        if let manager {
            resolvedManager = manager
        } else if let manager = try await loadExistingManager() {
            resolvedManager = manager
        } else {
            return nil
        }

        guard let session = resolvedManager.connection as? NETunnelProviderSession else {
            return nil
        }

        let responseData = try await session.sendProviderMessageAsync(
            Self.providerMessageData(command: command, limit: limit, level: level)
        )
        guard let responseData else {
            return nil
        }
        return String(data: responseData, encoding: .utf8)
    }

    private static func providerMessageData(command: String, limit: Int?, level: String?) -> Data {
        var payload: [String: Any] = ["command": command]
        if let limit {
            payload["limit"] = limit
        }
        if let level {
            payload["level"] = level
        }
        return (try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]))
            ?? Data(command.utf8)
    }

    private static func shouldQueryProvider(for status: NEVPNStatus) -> Bool {
        switch status {
        case .connected, .connecting, .reasserting:
            return true
        case .disconnecting, .disconnected, .invalid:
            return false
        @unknown default:
            return false
        }
    }

    private static func providerResponseSucceeded(_ raw: String) -> Bool {
        guard let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            return false
        }
        return object["ok"] as? Bool == true
            || (object["code"] as? Int) == Int(TunnelProxyNativeBridge.ok)
    }

    private static func logEntries(from raw: String, subsystem: String) -> [MobileLogEntry] {
        guard let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data)
        else {
            return [MobileLogEntry(level: .info, message: raw, subsystem: subsystem)]
        }

        if let lines = object as? [String] {
            return lines.map { line in
                MobileLogEntry(level: .info, message: line, subsystem: subsystem)
            }
        }

        if let payload = object as? [String: Any] {
            if let lines = payload["logs"] as? [String] {
                return lines.map { line in
                    MobileLogEntry(level: .info, message: line, subsystem: subsystem)
                }
            }
            if payload["ok"] as? Bool == false {
                let message = (payload["error"] as? String) ?? raw
                return [MobileLogEntry(level: .error, message: message, subsystem: subsystem)]
            }
        }

        return [MobileLogEntry(level: .info, message: raw, subsystem: subsystem)]
    }
}

@MainActor
private extension NETunnelProviderManager {
    static func loadAllFromPreferencesAsync() async throws -> [NETunnelProviderManager] {
        let box: TunnelProviderManagersBox = try await withCheckedThrowingContinuation { continuation in
            NETunnelProviderManager.loadAllFromPreferences { managers, error in
                if let error {
                    continuation.resume(throwing: error)
                    return
                }
                continuation.resume(returning: TunnelProviderManagersBox(managers: managers ?? []))
            }
        }
        return box.managers
    }

    func loadFromPreferencesAsync() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            loadFromPreferences { error in
                if let error {
                    continuation.resume(throwing: error)
                    return
                }
                continuation.resume()
            }
        }
    }

    func saveToPreferencesAsync() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            saveToPreferences { error in
                if let error {
                    continuation.resume(throwing: error)
                    return
                }
                continuation.resume()
            }
        }
    }
}

private struct TunnelProviderManagersBox: @unchecked Sendable {
    let managers: [NETunnelProviderManager]
}

@MainActor
private extension NETunnelProviderSession {
    func sendProviderMessageAsync(_ data: Data) async throws -> Data? {
        try await withCheckedThrowingContinuation { continuation in
            do {
                try sendProviderMessage(data) { responseData in
                    continuation.resume(returning: responseData)
                }
            } catch {
                continuation.resume(throwing: error)
            }
        }
    }
}

private struct ProviderRuntimeStatus {
    let running: Bool?
    let connected: Bool?
    let connecting: Bool?
    let message: String?
    let gatewaySubtitle: String?
    let pathMode: String?
    let p2pState: String?
    let peer: String?
    let peerCount: Int
    let p2pTraffic: String
    let relayTraffic: String
    var relayAllowance: String?
    let uptime: String
    let platformHeartbeat: String
    let transportHeartbeat: String

    init(rawJSON: String?) {
        guard let rawJSON,
              let data = rawJSON.data(using: .utf8),
              let parsedRoot = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            running = nil
            connected = nil
            connecting = nil
            message = nil
            gatewaySubtitle = nil
            pathMode = nil
            p2pState = nil
            peer = nil
            peerCount = 0
            p2pTraffic = "0 B ↑ / 0 B ↓"
            relayTraffic = "0 B ↑ / 0 B ↓"
            uptime = "0s"
            platformHeartbeat = "Unknown"
            transportHeartbeat = "Unknown"
            return
        }
        let root = MobileTrafficStatusNormalizer.normalizedProviderStatus(parsedRoot)

        running = root["running"] as? Bool
        let lastError = root["last_error"] as? [String: Any]
        message = (lastError?["error"] as? String)
            ?? (root["message"] as? String)

        let nativeStatus = (root["native_status"] as? [String: Any]) ?? root
        if let usage = nativeStatus["relay_usage"] as? [String: Any],
           let used = usage["used_bytes"] as? NSNumber,
           let allowance = usage["allowance_bytes"] as? NSNumber {
            relayAllowance = "\(TunnelStatusSnapshot.byteLabel(used.int64Value)) of \(TunnelStatusSnapshot.byteLabel(allowance.int64Value))"
        } else {
            relayAllowance = nil
        }
        let connection = nativeStatus["connection"] as? [String: Any]
        connected = Self.boolValue(connection?["connected"])
            ?? Self.boolValue(nativeStatus["connected"])
        connecting = Self.boolValue(connection?["connecting"])
            ?? Self.boolValue(nativeStatus["connecting"])
        let gatewayName = Self.stringValue(connection?["gateway_name"])
            ?? Self.stringValue(connection?["gatewayName"])
            ?? Self.stringValue(nativeStatus["gateway_name"])
            ?? Self.stringValue(nativeStatus["gatewayName"])
        gatewaySubtitle = Self.gatewaySubtitle(name: gatewayName)
        let resolvedPathMode = Self.stringValue(connection?["path_mode"])
            ?? Self.stringValue(nativeStatus["path_mode"])
        pathMode = resolvedPathMode
        p2pState = Self.stringValue(connection?["p2p_state"])
            ?? Self.stringValue(nativeStatus["p2p_state"])
        peer = Self.stringValue(connection?["p2p_primary_peer_id"])
        peerCount = Int(Self.intValue(connection?["p2p_peer_count"]) ?? 0)
        let traffic = Self.dictionaryValue(connection?["traffic"])
            ?? Self.dictionaryValue(nativeStatus["traffic"])
            ?? Self.dictionaryValue(root["traffic"])
        var p2pTx = Self.intValue(traffic?["p2p_tx_bytes"])
            ?? Self.intValue(connection?["p2p_tx_bytes"])
            ?? Self.intValue(nativeStatus["p2p_tx_bytes"])
            ?? Self.intValue(root["p2p_tx_bytes"])
        var p2pRx = Self.intValue(traffic?["p2p_rx_bytes"])
            ?? Self.intValue(connection?["p2p_rx_bytes"])
            ?? Self.intValue(nativeStatus["p2p_rx_bytes"])
            ?? Self.intValue(root["p2p_rx_bytes"])
        var relayTx = Self.intValue(traffic?["relay_tx_bytes"])
            ?? Self.intValue(connection?["relay_tx_bytes"])
            ?? Self.intValue(nativeStatus["relay_tx_bytes"])
            ?? Self.intValue(root["relay_tx_bytes"])
        var relayRx = Self.intValue(traffic?["relay_rx_bytes"])
            ?? Self.intValue(connection?["relay_rx_bytes"])
            ?? Self.intValue(nativeStatus["relay_rx_bytes"])
            ?? Self.intValue(root["relay_rx_bytes"])
        if let packetBridge = Self.dictionaryValue(root["packet_bridge"])
                ?? Self.dictionaryValue(nativeStatus["packet_bridge"]) {
            let bridgeTx = Self.intValue(packetBridge["bytes_to_tun2socks"])
            let bridgeRx = Self.intValue(packetBridge["bytes_from_tun2socks"])
            if Self.hasPositiveTraffic(tx: bridgeTx, rx: bridgeRx) {
                switch resolvedPathMode?.lowercased() {
                case "p2p":
                    (p2pTx, p2pRx) = Self.mergedLocalTraffic(
                        tx: p2pTx,
                        rx: p2pRx,
                        bridgeTx: bridgeTx,
                        bridgeRx: bridgeRx
                    )
                case "relay":
                    (relayTx, relayRx) = Self.mergedLocalTraffic(
                        tx: relayTx,
                        rx: relayRx,
                        bridgeTx: bridgeTx,
                        bridgeRx: bridgeRx
                    )
                default:
                    break
                }
            }
        }
        p2pTraffic = Self.trafficLabel(tx: p2pTx, rx: p2pRx)
        relayTraffic = Self.trafficLabel(tx: relayTx, rx: relayRx)
        uptime = Self.uptimeLabel(Self.intValue(connection?["uptime_secs"]))
        platformHeartbeat = Self.heartbeatLabel(connection?["platform_heartbeat"] as? [String: Any])
        transportHeartbeat = Self.heartbeatLabel(connection?["transport_heartbeat"] as? [String: Any])
    }

    var pathModeValue: TunnelPathMode {
        guard let pathMode else {
            return .unknown
        }
        return TunnelPathMode(rawValue: pathMode.lowercased()) ?? .unknown
    }

    var pathModeLabel: String {
        guard let normalized = pathMode?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased(),
              !normalized.isEmpty
        else {
            return "\u{2014}"
        }
        switch normalized {
        case "p2p":
            return p2pState?.lowercased() == "degraded" ? "Direct degraded" : "Direct"
        case "relay":
            return "Relay"
        case "connecting":
            return "Connecting"
        default:
            return "Disconnected"
        }
    }

    var peerLabel: String {
        Self.peerLabel(primaryPeer: peer, peerCount: peerCount)
    }

    var reportsConnected: Bool {
        if connected == true {
            return true
        }
        if connected == false || connecting == true {
            return false
        }
        return pathModeValue == .relay || pathModeValue == .p2p
    }

    private static func gatewaySubtitle(name: String?) -> String? {
        guard let display = name, !display.isEmpty else {
            return nil
        }
        return "Gateway: \(display)"
    }

    /// How many other devices are connected, not which one is primary.
    ///
    /// The previous label ran the Peer ID through a formatter expecting
    /// `name-xxxxxxxx-0`, so every UUID Peer rendered as "Unknown (+1)" — a
    /// count dressed up as an identity, and a wrong one. Identity lives in the
    /// Peers tab, where there is room for it.
    private static func peerLabel(primaryPeer: String?, peerCount: Int) -> String {
        switch peerCount {
        case ..<1: return "None"
        case 1: return "1 device"
        default: return "\(peerCount) devices"
        }
    }

    private static func stringValue(_ raw: Any?) -> String? {
        if let value = raw as? String {
            let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed
        }
        if let value = raw as? NSNumber {
            return value.stringValue
        }
        return nil
    }

    private static func dictionaryValue(_ raw: Any?) -> [String: Any]? {
        raw as? [String: Any]
    }

    private static func boolValue(_ raw: Any?) -> Bool? {
        if let value = raw as? Bool {
            return value
        }
        if let value = raw as? NSNumber {
            return value.boolValue
        }
        if let value = raw as? String {
            switch value.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
            case "true", "yes", "1":
                return true
            case "false", "no", "0":
                return false
            default:
                return nil
            }
        }
        return nil
    }

    private static func heartbeatLabel(_ raw: [String: Any]?) -> String {
        raw?["active"] as? Bool == true ? "Active" : "Inactive"
    }

    private static func trafficLabel(tx: Int64?, rx: Int64?) -> String {
        "\(formatBytes(tx ?? 0)) ↑ / \(formatBytes(rx ?? 0)) ↓"
    }

    private static func hasPositiveTraffic(tx: Int64?, rx: Int64?) -> Bool {
        (tx ?? 0) > 0 || (rx ?? 0) > 0
    }

    private static func mergedLocalTraffic(
        tx: Int64?,
        rx: Int64?,
        bridgeTx: Int64?,
        bridgeRx: Int64?
    ) -> (Int64?, Int64?) {
        (
            max(tx ?? 0, bridgeTx ?? 0),
            max(rx ?? 0, bridgeRx ?? 0)
        )
    }

    private static func uptimeLabel(_ totalSeconds: Int64?) -> String {
        let total = max(0, totalSeconds ?? 0)
        let hours = total / 3600
        let minutes = (total % 3600) / 60
        let seconds = total % 60
        if hours > 0 {
            return "\(hours)h \(minutes)m \(seconds)s"
        }
        if minutes > 0 {
            return "\(minutes)m \(seconds)s"
        }
        return "\(seconds)s"
    }

    private static func intValue(_ raw: Any?) -> Int64? {
        if let value = raw as? Int64 {
            return value
        }
        if let value = raw as? Int {
            return Int64(value)
        }
        if let value = raw as? UInt64 {
            return Int64(clamping: value)
        }
        if let value = raw as? NSNumber {
            return value.int64Value
        }
        if let value = raw as? String {
            return Int64(value)
        }
        return nil
    }

    private static func formatBytes(_ bytes: Int64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB"]
        var value = Double(max(0, bytes))
        var unit = 0
        while value >= 1024 && unit < units.count - 1 {
            value /= 1024
            unit += 1
        }
        if unit == 0 {
            return "\(Int(value)) \(units[unit])"
        }
        return String(format: "%.1f %@", value, units[unit])
    }
}

private enum NativeBridgeFFICommands {
    static func clearLogs() -> Int32 {
        #if canImport(TpMobileFfi)
        return tp_mobile_clear_logs()
        #else
        return TunnelProxyNativeBridge.startFailed
        #endif
    }
}
