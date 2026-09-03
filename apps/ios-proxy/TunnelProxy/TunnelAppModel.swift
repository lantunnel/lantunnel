import Foundation
import SwiftUI

enum AppTab: Hashable {
    case status
    case peers
    case config
    case logs
}

enum TunnelConnectionPhase: String, Equatable {
    case disconnected
    case connecting
    case connected
    case failed
}

enum TunnelServiceStatePhase: String, Equatable {
    case stopped
    case connecting
    case stopping
    case running
    case failed

    var isTransient: Bool {
        self == .connecting || self == .stopping
    }
}

struct TunnelServiceState: Equatable {
    static let staleTransientInterval: TimeInterval = 30

    var phase: TunnelServiceStatePhase
    var message: String?
    var updatedAt: Date

    static func stopped(updatedAt: Date, message: String? = nil) -> TunnelServiceState {
        TunnelServiceState(phase: .stopped, message: message, updatedAt: updatedAt)
    }

    func recovered(now: Date) -> TunnelServiceState {
        guard phase.isTransient,
              now.timeIntervalSince(updatedAt) > Self.staleTransientInterval
        else {
            return self
        }

        return .stopped(
            updatedAt: now,
            message: "Previous \(phase.rawValue) state expired"
        )
    }

    func tunnelStatus() -> TunnelStatus {
        switch phase {
        case .stopped:
            return .disconnected
        case .connecting:
            return .connecting
        case .stopping:
            return .disconnecting
        case .running:
            return .connected(TunnelConnectionDetails(
                pathMode: .unknown,
                transport: "NetworkExtension",
                lastHeartbeatAt: updatedAt
            ))
        case .failed:
            return .failed(message ?? "Connection failed")
        }
    }
}

struct TunnelStatusSnapshot: Equatable {
    var phase: TunnelConnectionPhase
    var title: String
    var subtitle: String
    var pathMode: String
    var peer: String
    var p2pTraffic: String
    var relayTraffic: String
    /// What the Platform last reported about the Relay allowance, if anything.
    var relayAllowance: String?
    var uptime: String
    var platformHeartbeat: String
    var transportHeartbeat: String
    var routeCount: Int
    var message: String?

    var canConnect: Bool {
        phase == .disconnected || phase == .failed
    }

    var canDisconnect: Bool {
        phase == .connecting || phase == .connected
    }

    init(
        phase: TunnelConnectionPhase,
        title: String,
        subtitle: String,
        pathMode: String,
        peer: String,
        p2pTraffic: String,
        relayTraffic: String,
        relayAllowance: String? = nil,
        uptime: String,
        platformHeartbeat: String,
        transportHeartbeat: String,
        routeCount: Int,
        message: String? = nil
    ) {
        self.phase = phase
        self.title = title
        self.subtitle = subtitle
        self.pathMode = pathMode
        self.peer = peer
        self.p2pTraffic = p2pTraffic
        self.relayTraffic = relayTraffic
        self.relayAllowance = relayAllowance
        self.uptime = uptime
        self.platformHeartbeat = platformHeartbeat
        self.transportHeartbeat = transportHeartbeat
        self.routeCount = routeCount
        self.message = message
    }

    static func disconnected(routeCount: Int, message: String? = nil) -> TunnelStatusSnapshot {
        TunnelStatusSnapshot(
            phase: .disconnected,
            title: "Disconnected",
            subtitle: message ?? "Import a Peer profile to connect",
            pathMode: "Disconnected",
            peer: "None",
            p2pTraffic: "0 B ↑ / 0 B ↓",
            relayTraffic: "0 B ↑ / 0 B ↓",
            uptime: "0s",
            platformHeartbeat: "Inactive",
            transportHeartbeat: "Inactive",
            routeCount: routeCount,
            message: message
        )
    }

    static func connecting(routeCount: Int, message: String = "Connecting") -> TunnelStatusSnapshot {
        TunnelStatusSnapshot(
            phase: .connecting,
            title: "Starting",
            subtitle: message,
            pathMode: "Connecting",
            peer: "None",
            p2pTraffic: "0 B ↑ / 0 B ↓",
            relayTraffic: "0 B ↑ / 0 B ↓",
            uptime: "0s",
            platformHeartbeat: "Connecting",
            transportHeartbeat: "Connecting",
            routeCount: routeCount,
            message: message
        )
    }

    static func connected(
        routeCount: Int,
        pathMode: String = "\u{2014}",
        peer: String = "None",
        p2pTraffic: String = "0 B ↑ / 0 B ↓",
        relayTraffic: String = "0 B ↑ / 0 B ↓",
        relayAllowance: String? = nil,
        uptime: String = "0s",
        platformHeartbeat: String = "Active",
        transportHeartbeat: String = "Active",
        subtitle: String? = nil,
        message: String? = nil
    ) -> TunnelStatusSnapshot {
        TunnelStatusSnapshot(
            phase: .connected,
            title: "Connected",
            subtitle: subtitle ?? message ?? "Split tunnel VPN is active",
            pathMode: pathMode,
            peer: peer,
            p2pTraffic: p2pTraffic,
            relayTraffic: relayTraffic,
            relayAllowance: relayAllowance,
            uptime: uptime,
            platformHeartbeat: platformHeartbeat,
            transportHeartbeat: transportHeartbeat,
            routeCount: routeCount,
            message: message
        )
    }

    static func failed(routeCount: Int, message: String) -> TunnelStatusSnapshot {
        TunnelStatusSnapshot(
            phase: .failed,
            title: "Connection Failed",
            subtitle: "Check the Peer profile and the network",
            pathMode: "Disconnected",
            peer: "None",
            p2pTraffic: "0 B ↑ / 0 B ↓",
            relayTraffic: "0 B ↑ / 0 B ↓",
            uptime: "0s",
            platformHeartbeat: "Failed",
            transportHeartbeat: "Stopped",
            routeCount: routeCount,
            message: message
        )
    }

    init(status: TunnelStatus?, routeCount: Int) {
        guard let status else {
            self = .disconnected(routeCount: routeCount)
            return
        }
        self.init(status: status, routeCount: routeCount)
    }

    init(status: TunnelStatus, routeCount: Int) {
        switch status {
        case .connected(let details):
            let traffic = Self.trafficLabel(tx: Int64(clamping: details.bytesSent), rx: Int64(clamping: details.bytesReceived))
            let p2pTraffic = details.pathMode == .p2p ? traffic : "0 B ↑ / 0 B ↓"
            let relayTraffic = details.pathMode == .relay ? traffic : "0 B ↑ / 0 B ↓"
            self = .connected(
                routeCount: routeCount,
                pathMode: details.pathMode.displayName,
                peer: Self.peerLabel(details.peerClientId),
                p2pTraffic: p2pTraffic,
                relayTraffic: relayTraffic,
                uptime: "0s",
                platformHeartbeat: details.lastHeartbeatAt == nil ? "\u{2014}" : "Active",
                transportHeartbeat: details.transport,
                message: nil
            )
        case .connecting:
            self = .connecting(routeCount: routeCount)
        case .disconnecting:
            self = .connecting(routeCount: routeCount, message: "Disconnecting")
        case .failed(let message):
            self = .failed(routeCount: routeCount, message: message)
        case .unsupported(let message):
            self = .failed(routeCount: routeCount, message: message)
        case .disconnected:
            self = .disconnected(routeCount: routeCount)
        }
    }

    private static func trafficLabel(tx: Int64, rx: Int64) -> String {
        "\(byteLabel(tx)) \u{2191} / \(byteLabel(rx)) \u{2193}"
    }

    static func byteLabel(_ bytes: Int64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB"]
        var value = Double(max(bytes, 0))
        var unit = 0
        while value >= 1024 && unit < units.count - 1 {
            value /= 1024
            unit += 1
        }
        return unit == 0 ? "\(Int(value)) B" : String(format: "%.1f %@", value, units[unit])
    }

    private static func peerLabel(_ raw: String?) -> String {
        guard let raw, !raw.isEmpty else {
            return "None"
        }
        return raw
    }

}

struct LanRouteValidationResult: Equatable {
    var isValid: Bool
    var message: String
    var routes: [String] = []
}

enum LanRouteUIValidator {
    static func validate(_ routes: [String]) -> LanRouteValidationResult {
        let normalized = routes.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        guard !normalized.isEmpty else {
            return LanRouteValidationResult(isValid: false, message: "At least one LAN route is required")
        }

        do {
            let validated = try RouteValidator.validate(normalized)
            let duplicates = Dictionary(grouping: validated.routes, by: { $0 })
                .first { $0.value.count > 1 }
            if let duplicate = duplicates?.key {
                return LanRouteValidationResult(isValid: false, message: "Duplicate route: \(duplicate)")
            }
            return LanRouteValidationResult(isValid: true, message: "", routes: validated.routes)
        } catch {
            return LanRouteValidationResult(isValid: false, message: error.localizedDescription)
        }
    }
}

@MainActor
final class TunnelAppModel: ObservableObject {
    @Published var config: MobileConfig

    /// One other device in this Tunnel, as the Peers tab renders it.
    struct KnownPeer: Equatable {
        let peerId: String
        let overlayIP: String
        let phase: String
        let path: String
        let exports: [String]

        /// `V2RemotePeerPhase` also has Syncing and Stale; calling a
        /// mid-handshake device "Unreachable" describes it as broken.
        var reachability: String {
            switch phase.lowercased() {
            case "ready": return "Ready"
            // The desktop reads these off the same enum: Syncing, not
            // Connecting; Unavailable, not Unreachable.
            case "syncing": return "Syncing"
            case "stale": return "Stale"
            default: return "Unavailable"
            }
        }

        /// Greyed out only when there is genuinely no way through; a Peer
        /// mid-handshake is not that.
        var reachable: Bool {
            ["ready", "syncing"].contains(phase.lowercased())
        }

        /// The overlay address names a device. A Peer ID, whole or truncated,
        /// is not something anyone can act on and does not belong on screen.
        var title: String { overlayIP.isEmpty ? "\u{2014}" : overlayIP }

        var subtitle: String {
            var parts = [reachability]
            if !path.isEmpty { parts.append(path) }
            if !exports.isEmpty { parts.append(exports.joined(separator: " ")) }
            return parts.joined(separator: " \u{00B7} ")
        }

        func matches(_ query: String) -> Bool {
            let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
            if needle.isEmpty { return true }
            return peerId.lowercased().contains(needle)
                || overlayIP.lowercased().contains(needle)
                || exports.contains { $0.lowercased().contains(needle) }
        }
    }

    /// Refreshed on the status poll and on tab entry, not read from a view body.
    ///
    /// A computed property calling the FFI ran once per render and never told
    /// SwiftUI anything had changed, so the list was whatever it was the first
    /// time the tab was drawn.
    @Published var knownPeers: [KnownPeer] = []

    /// Last status the extension reported, and the app's only view of the mesh.
    var latestProviderStatusJSON: String?

    /// What the tunnel was actually started with.
    ///
    /// The VPN's route set is fixed at start, so this is what a freshly
    /// derived list has to be compared against — comparing a derived list to
    /// another derived list gave the same value on both sides.
    var routedAtConnect: Set<String> = Set(
        UserDefaults.standard.stringArray(forKey: ServiceStatePersistence.routedAtConnect) ?? []
    )

    /// The mesh as last seen, so Connect can route it.
    ///
    /// The provider is not queried while disconnected, so deriving from the
    /// live status at the moment Connect is pressed yields only the overlay.
    /// Following the mesh would route nothing any Peer publishes — the whole
    /// feature — unless what was last seen survives the disconnect.
    var rememberedMeshRoutes: [String] = UserDefaults.standard
        .stringArray(forKey: ServiceStatePersistence.rememberedMeshRoutes) ?? []

    /// Takes a status the app just fetched, if it fetched one.
    ///
    /// A status the app could not fetch is not news that the mesh is empty.
    func noteProviderStatus(_ json: String?) {
        guard let json, !json.isEmpty else { return }
        latestProviderStatusJSON = json
        guard statusPresentation.phase == .connected else { return }
        let seen = MobileConfig.routesFromExports(json)
        guard seen != rememberedMeshRoutes else { return }
        rememberedMeshRoutes = seen
        UserDefaults.standard.set(seen, forKey: ServiceStatePersistence.rememberedMeshRoutes)
    }

    /// A Peer that has appeared since is added, not swapped in.
    nonisolated static func mergedMeshRoutes(remembered: [String], derived: [String]) -> [String] {
        Set(remembered).union(derived).sorted()
    }

    /// What the Settings tab says about the mesh-derived list.
    var meshRoutesSummary: String {
        Self.derivedRoutesSummary(
            following: true,
            derived: effectiveLanRoutes(),
            running: statusPresentation.phase == .connected,
            routedAtConnect: routedAtConnect
        )
    }

    /// The routes this device carries: whatever the mesh publishes.
    ///
    /// There is no opting out. Which of two overlapping prefixes wins is
    /// decided by Tunnel First, in the engine, not by refusing to learn the
    /// remote one.
    func effectiveLanRoutes() -> [String] {
        Self.mergedMeshRoutes(
            remembered: rememberedMeshRoutes,
            derived: MobileConfig.routesFromExports(latestProviderStatusJSON ?? "")
        )
    }

    func refreshKnownPeers() {
        // The last status is kept for the route derivation, but rendering it
        // while the tunnel is off showed every Peer with its old phase and a
        // Direct/Relay badge — a live-looking mesh for a tunnel that is not up.
        guard statusPresentation.phase == .connected else {
            knownPeers = []
            return
        }
        knownPeers = Self.knownPeers(from: latestProviderStatusJSON ?? "")
    }

    /// Drops everything learned from the Tunnel that was just left.
    private func forgetTheMesh() {
        rememberedMeshRoutes = []
        routedAtConnect = []
        UserDefaults.standard.set([String](), forKey: ServiceStatePersistence.rememberedMeshRoutes)
        UserDefaults.standard.set([String](), forKey: ServiceStatePersistence.routedAtConnect)
    }

    static func knownPeers(from rawStatus: String) -> [KnownPeer] {
        guard let root = MobileConfig.runtimeStatusObject(rawStatus),
              let directory = root["peer_directory"] as? [String: Any],
              let peers = directory["peers"] as? [[String: Any]]
        else { return [] }
        return peers.map { peer in
            let exports = (peer["exports"] as? [[String: Any]] ?? [])
                .compactMap { $0["prefix"] as? String }
            return KnownPeer(
                peerId: peer["peer_id"] as? String ?? "",
                overlayIP: peer["overlay_ip"] as? String ?? "",
                phase: peer["phase"] as? String ?? "unknown",
                path: {
                    switch peer["current_path"] as? String {
                    case "direct": return "Direct"
                    case "encrypted_relay": return "Relay"
                    default: return ""
                    }
                }(),
                exports: exports
            )
        }
    }

    /// What the Peer profile card is allowed to render.
    /// Same vocabulary as the desktop's meshStateLabel.
    var meshStateLabel: String {
        let phase = (MobileConfig.runtimeStatusObject(latestProviderStatusJSON ?? "")?["peer_directory"]
            as? [String: Any])?["phase"] as? String ?? ""
        switch phase.lowercased() {
        case "ready", "healthy": return "Healthy"
        case "syncing": return "Syncing"
        case "degraded": return "Degraded"
        case "unavailable": return "Unavailable"
        default: return "Unknown"
        }
    }

    var peerIdentity: MobileConfig.PeerIdentity? {
        MobileConfig.peerIdentity(config.peerProfileJSON)
    }
    @Published var status: TunnelStatus
    @Published var logs: [MobileLogEntry]
    @Published var logText: String
    @Published var selectedTab: AppTab
    @Published var statusPresentation: TunnelStatusSnapshot
    @Published var bannerMessage: String?
    @Published var isBusy: Bool
    @Published var logLevel: String

    private let controlService: TunnelControlServicing
    private let logStore: MobileLogStore
    private let keychainStore: KeychainStore
    private let clock: () -> Date
    private let startStatusPollAttempts: Int
    private let startStatusPollDelayNanoseconds: UInt64
    private let startStatusProtectionInterval: TimeInterval
    private var connectAttemptStartedAt: Date?
    private var lastLogsRefreshAt: Date?

    private static let activeLogsRefreshInterval: TimeInterval = 5

    var canConnect: Bool {
        !isBusy
            && statusPresentation.canConnect
    }

    var canDisconnect: Bool {
        !isBusy && statusPresentation.canDisconnect
    }

    var canEditLanP2pSetting: Bool {
        !isBusy && statusPresentation.canConnect
    }

    init(
        config: MobileConfig? = nil,
        status: TunnelStatus = .disconnected,
        logs: [MobileLogEntry] = [],
        selectedTab: AppTab = .status,
        controlService: TunnelControlServicing? = nil,
        logStore: MobileLogStore? = nil,
        keychainStore: KeychainStore = KeychainStore(),
        clock: @escaping () -> Date = Date.init,
        startStatusPollAttempts: Int = 12,
        startStatusPollDelayNanoseconds: UInt64 = 250_000_000,
        startStatusProtectionInterval: TimeInterval = 15
    ) {
        let resolvedConfig = config ?? Self.loadPersistedConfig()
        let now = clock()
        let loadedServiceState = Self.loadPersistedServiceState(now: now)
        let serviceState = Self.hasPeerProfile(resolvedConfig)
            ? loadedServiceState
            : .stopped(updatedAt: now)
        let resolvedStatus = status == .disconnected ? serviceState.tunnelStatus() : status
        self.config = resolvedConfig
        self.status = resolvedStatus
        self.logs = logs
        self.logText = Self.logText(from: logs)
        self.selectedTab = selectedTab
        self.controlService = controlService ?? TunnelControlService()
        self.logStore = logStore ?? MobileLogStore()
        self.keychainStore = keychainStore
        self.clock = clock
        self.startStatusPollAttempts = max(0, startStatusPollAttempts)
        self.startStatusPollDelayNanoseconds = startStatusPollDelayNanoseconds
        self.startStatusProtectionInterval = startStatusProtectionInterval
        self.statusPresentation = TunnelStatusSnapshot(status: resolvedStatus, routeCount: resolvedConfig.lanRoutes.count)
        self.bannerMessage = serviceState.message
        self.isBusy = false
        self.logLevel = "info"
    }

    @discardableResult
    func importPeerProfile(_ raw: String) -> Bool {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            bannerMessage = "Paste a Lantunnel Peer profile"
            return false
        }

        do {
            // Carrying the previous Tunnel's prefixes over means installing VPN
            // routes nothing in the new Tunnel exports — and since home LANs are
            // nearly always 192.168.x.0/24, the usual casualty is the phone's
            // own Wi-Fi.
            let previousTunnel = MobileConfig.peerIdentity(config.peerProfileJSON)?.tunnelId
            config = try config.applyingPeerProfile(trimmed)
            if previousTunnel != MobileConfig.peerIdentity(config.peerProfileJSON)?.tunnelId {
                forgetTheMesh()
            }
            persistConfig(config)
            bannerMessage = "Imported Peer profile"
            appendAppLog("Imported Peer profile")
            selectedTab = .config
            return true
        } catch {
            bannerMessage = "Import failed: \(error.localizedDescription)"
            appendAppLog("Import failed: \(error.localizedDescription)")
            return false
        }
    }

    func applyConfig() {
        persistConfig(config)
        bannerMessage = "Config applied"
        appendAppLog("Config applied: lan_p2p=\(config.lanP2pEnabled) routes=follow-mesh")
    }

    func connect() async {
        guard Self.hasPeerProfile(config) else {
            bannerMessage = "A valid Lantunnel Peer profile is required"
            return
        }

        let validation = LanRouteUIValidator.validate(effectiveLanRoutes())
        guard validation.isValid else {
            bannerMessage = validation.message
            return
        }

        // A rule that cannot be read used to be skipped on the way to the
        // extension, so the screen showed a restriction the tunnel never had.
        if let unreadable = config.unreadableAccessRule() {
            bannerMessage = "Not a rule: \(unreadable)"
            return
        }

        isBusy = true
        connectAttemptStartedAt = clock()
        persistConfig(config)
        routedAtConnect = Set(validation.routes)
        UserDefaults.standard.set(validation.routes, forKey: ServiceStatePersistence.routedAtConnect)
        // A copy, so the derived list reaches the extension without the
        // manual list being overwritten by it.
        var launchConfig = config
        launchConfig.lanRoutes = validation.routes
        persistServiceState(TunnelServiceStatePhase.connecting, message: "Connecting")
        statusPresentation = .connecting(routeCount: validation.routes.count, message: "Connecting")
        bannerMessage = "Connecting"
        appendAppLog("VPN connect requested")

        do {
            let snapshot = try await controlService.start(config: launchConfig, logLevel: logLevel)
            let acceptedSnapshot = acceptedStartSnapshot(from: snapshot, routeCount: validation.routes.count)
            apply(snapshot: acceptedSnapshot)
            if snapshot.presentation.phase == .disconnected {
                appendAppLog("VPN start accepted; waiting for iOS status to update")
            }
            let resolvedSnapshot = await waitForStartProgress(after: acceptedSnapshot)
            apply(snapshot: resolvedSnapshot)
            bannerMessage = resolvedSnapshot.presentation.message
        } catch {
            connectAttemptStartedAt = nil
            let failed = TunnelStatusSnapshot.failed(routeCount: config.lanRoutes.count, message: error.localizedDescription)
            statusPresentation = failed
            bannerMessage = failed.message
            persistServiceState(TunnelServiceStatePhase.failed, message: error.localizedDescription)
            appendAppLog("VPN connect failed: \(error.localizedDescription)")
        }

        isBusy = false
    }

    func disconnect() async {
        isBusy = true
        connectAttemptStartedAt = nil
        bannerMessage = "Disconnecting"
        persistServiceState(TunnelServiceStatePhase.stopping, message: "Disconnecting")
        appendAppLog("VPN disconnect requested")

        do {
            let snapshot = try await controlService.stop(config: config)
            apply(snapshot: snapshot)
            bannerMessage = "Disconnected"
        } catch {
            bannerMessage = "Disconnect failed: \(error.localizedDescription)"
            persistServiceState(TunnelServiceStatePhase.failed, message: error.localizedDescription)
            appendAppLog("VPN disconnect failed: \(error.localizedDescription)")
        }

        isBusy = false
    }

    /// Reconnects the imported profile when the app opens, if asked to.
    ///
    /// Once per launch, and never over a tunnel that is already up. A setting
    /// that is stored but changes nothing is the same as no setting.
    func autoConnectIfAsked() async {
        guard config.autoConnect, Self.hasPeerProfile(config) else { return }
        await refreshStatus()
        guard statusPresentation.phase == .disconnected else { return }
        await connect()
    }

    func refreshStatus() async {
        guard Self.hasPeerProfile(config) else {
            apply(snapshot: TunnelControlSnapshot(
                rawStatus: .disconnected,
                presentation: .disconnected(routeCount: config.lanRoutes.count)
            ))
            bannerMessage = nil
            return
        }

        let snapshot = await controlService.status(config: config)
        if let guardedSnapshot = guardedStartSnapshot(for: snapshot) {
            apply(snapshot: guardedSnapshot)
            return
        }
        apply(snapshot: snapshot)
    }

    func refreshLogs(limit: Int = 500) async {
        let nativeLogs = await controlService.logs(limit: limit)
        let refreshedLogs = logStore.entries + nativeLogs
        replaceLogs(refreshedLogs)
        lastLogsRefreshAt = clock()
        if let nativeLogLevel = Self.nativeLogLevel(in: refreshedLogs) {
            logLevel = nativeLogLevel
        }
    }

    func refreshActiveView() async {
        await refreshStatus()
        // The Peers tab was frozen for as long as it stayed open — only
        // .onAppear refreshed it, though its own comment said the poll did.
        if selectedTab == .peers {
            refreshKnownPeers()
        }
        if selectedTab == .logs {
            await refreshLogsIfStale()
        }
    }

    func runForegroundRefreshLoop(intervalNanoseconds: UInt64 = 1_000_000_000) async {
        while !Task.isCancelled {
            await refreshActiveView()
            do {
                try await Task.sleep(nanoseconds: intervalNanoseconds)
            } catch {
                return
            }
        }
    }

    func clearLogs() async {
        await controlService.clearLogs()
        logStore.clear()
        replaceLogs([])
        appendAppLog("Logs cleared")
    }

    func setLogLevel(_ level: String) async {
        logLevel = level
        let target = await controlService.setLogLevel(level)
        switch target {
        case .provider:
            appendAppLog("Provider log level set to \(level)")
        case .app:
            appendAppLog("App log level set to \(level); VPN provider logs are available while the VPN is starting or connected")
        }
        await refreshLogs()
    }

    private func apply(snapshot: TunnelControlSnapshot) {
        status = snapshot.rawStatus
        noteProviderStatus(snapshot.providerStatusJSON)
        if selectedTab == .peers {
            refreshKnownPeers()
        }
        statusPresentation = snapshot.presentation
        if snapshot.presentation.phase != .connecting {
            connectAttemptStartedAt = nil
        }
        if snapshot.presentation.phase == .connecting,
           snapshot.presentation.message == "Disconnecting" {
            persistServiceState(TunnelServiceStatePhase.stopping, message: snapshot.presentation.message)
        } else {
            persistServiceState(snapshot.presentation.phase, message: snapshot.presentation.message)
        }
        if let message = snapshot.presentation.message, !message.isEmpty {
            bannerMessage = message
        } else if snapshot.presentation.phase == .connected,
                  bannerMessage == "Connecting" {
            bannerMessage = nil
        }
    }

    private func acceptedStartSnapshot(
        from snapshot: TunnelControlSnapshot,
        routeCount: Int
    ) -> TunnelControlSnapshot {
        guard snapshot.presentation.phase == .disconnected else {
            return snapshot
        }

        return TunnelControlSnapshot(
            rawStatus: .connecting,
            presentation: .connecting(routeCount: routeCount, message: "Connecting")
        )
    }

    private func waitForStartProgress(after snapshot: TunnelControlSnapshot) async -> TunnelControlSnapshot {
        guard snapshot.presentation.phase == .connecting,
              startStatusPollAttempts > 0
        else {
            return snapshot
        }

        var latest = snapshot
        for attempt in 0..<startStatusPollAttempts {
            let polled = await controlService.status(config: config)
            switch polled.presentation.phase {
            case .connected, .failed:
                return polled
            case .connecting:
                latest = polled
            case .disconnected:
                break
            }

            if attempt < startStatusPollAttempts - 1,
               startStatusPollDelayNanoseconds > 0 {
                try? await Task.sleep(nanoseconds: startStatusPollDelayNanoseconds)
            }
        }

        return latest
    }

    private func guardedStartSnapshot(for snapshot: TunnelControlSnapshot) -> TunnelControlSnapshot? {
        guard snapshot.presentation.phase == .disconnected,
              statusPresentation.phase == .connecting,
              let connectAttemptStartedAt
        else {
            return nil
        }

        let elapsed = clock().timeIntervalSince(connectAttemptStartedAt)
        guard elapsed < startStatusProtectionInterval else {
            return TunnelControlSnapshot(
                rawStatus: .failed("VPN did not start"),
                presentation: .failed(
                    routeCount: config.lanRoutes.count,
                    message: "VPN did not start. Check Logs for details."
                )
            )
        }

        return TunnelControlSnapshot(
            rawStatus: .connecting,
            presentation: .connecting(
                routeCount: config.lanRoutes.count,
                message: statusPresentation.message ?? "Connecting"
            )
        )
    }

    private func appendAppLog(_ message: String) {
        logStore.append(
            message,
            level: .info,
            subsystem: "app"
        )
        replaceLogs(logStore.entries + logs.filter { $0.subsystem != "app" })
    }

    func refreshLogsIfStale(limit: Int = 500) async {
        if let lastLogsRefreshAt,
           clock().timeIntervalSince(lastLogsRefreshAt) < Self.activeLogsRefreshInterval {
            return
        }
        await refreshLogs(limit: limit)
    }

    private func replaceLogs(_ entries: [MobileLogEntry]) {
        logs = entries
        logText = Self.logText(from: entries)
    }

    private static func logText(from entries: [MobileLogEntry]) -> String {
        var nativeStatus = "Unavailable"
        var nativeLogConfig = "Unavailable"
        var appLines: [String] = []
        var nativeLines: [String] = []

        for entry in entries {
            if let status = sectionPayload(entry.message, prefix: "Native status:") {
                nativeStatus = status
                continue
            }
            if let logConfig = sectionPayload(entry.message, prefix: "Native log config:") {
                nativeLogConfig = logConfig
                continue
            }
            if entry.subsystem == "provider" || entry.subsystem == "native" {
                nativeLines.append(entry.message)
            } else {
                appLines.append(entry.message)
            }
        }

        let appText = appLines.isEmpty ? "No logs yet." : appLines.joined(separator: "\n")
        let nativeText = nativeLines.isEmpty ? "No native logs yet." : nativeLines.joined(separator: "\n")
        return """
        Native status:
        \(nativeStatus)

        \(appText)

        Native log config:
        \(nativeLogConfig)

        Native logs:
        \(nativeText)
        """
    }

    private static func nativeLogLevel(in entries: [MobileLogEntry]) -> String? {
        for entry in entries {
            guard let logConfig = sectionPayload(entry.message, prefix: "Native log config:"),
                  let data = logConfig.data(using: .utf8),
                  let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let rawLevel = object["level"] as? String
            else {
                continue
            }
            if let normalized = normalizedLogLevel(rawLevel) {
                return normalized
            }
        }
        return nil
    }

    private static func normalizedLogLevel(_ raw: String) -> String? {
        switch raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "error":
            return "error"
        case "warn", "warning":
            return "warn"
        case "info":
            return "info"
        case "debug":
            return "debug"
        case "trace":
            return "trace"
        default:
            return nil
        }
    }

    private static func sectionPayload(_ message: String, prefix: String) -> String? {
        guard message.hasPrefix(prefix) else {
            return nil
        }
        return String(message.dropFirst(prefix.count))
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func normalizedRoutes(_ routes: [String]) -> [String] {
        routes.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
    }

    nonisolated private static func hasPeerProfile(_ config: MobileConfig) -> Bool {
        (try? MobileConfig.normalizedPeerProfileJSON(config.peerProfileJSON)) != nil
    }
}

private enum ConfigPersistence {
    static let peerProfile = "TunnelProxy.config.peerProfile"
    static let deviceID = "TunnelProxy.config.deviceID"
    static let localSocks5Listen = "TunnelProxy.config.localSocks5Listen"
    static let lanP2pEnabled = "TunnelProxy.config.lanP2pEnabled"
    static let lanRoutes = "TunnelProxy.config.lanRoutes"
    static let exportedLans = "TunnelProxy.config.exportedLans"
    static let tunnelFirst = "TunnelProxy.config.tunnelFirst"
    static let blockAllIncoming = "TunnelProxy.config.blockAllIncoming"
    static let autoConnect = "TunnelProxy.config.autoConnect"
    static let accessRules = "TunnelProxy.config.accessRules"
}

private enum ServiceStatePersistence {
    static let phase = "TunnelProxy.serviceState.phase"
    static let message = "TunnelProxy.serviceState.message"
    static let updatedAt = "TunnelProxy.serviceState.updatedAt"
    static let routedAtConnect = "TunnelProxy.serviceState.routedAtConnect"
    static let rememberedMeshRoutes = "TunnelProxy.serviceState.rememberedMeshRoutes"
}

extension TunnelAppModel {
    nonisolated static func loadPersistedConfig(defaults: UserDefaults = .standard) -> MobileConfig {
        let fallback = MobileConfig()
        let routes = defaults.stringArray(forKey: ConfigPersistence.lanRoutes) ?? fallback.lanRoutes
        let peerProfile = (try? KeychainStore().loadString(forKey: ConfigPersistence.peerProfile))
            ?? fallback.peerProfileJSON
        let deviceID = loadOrCreateDeviceID(defaults: defaults)
        return MobileConfig(
            peerProfileJSON: peerProfile,
            deviceID: deviceID,
            localSocks5Listen: Self.resolvedLocalSocks5Listen(
                defaults.string(forKey: ConfigPersistence.localSocks5Listen),
                fallback: fallback.localSocks5Listen
            ),
            lanP2pEnabled: defaults.object(forKey: ConfigPersistence.lanP2pEnabled) == nil
                ? fallback.lanP2pEnabled
                : defaults.bool(forKey: ConfigPersistence.lanP2pEnabled),
            lanRoutes: routes.isEmpty ? MobileConfig.defaultLanRoutes : routes,
            exportedLans: defaults.stringArray(forKey: ConfigPersistence.exportedLans) ?? [],
            tunnelFirst: defaults.bool(forKey: ConfigPersistence.tunnelFirst),
            blockAllIncoming: defaults.bool(forKey: ConfigPersistence.blockAllIncoming),
            autoConnect: defaults.bool(forKey: ConfigPersistence.autoConnect),
            accessRules: defaults.stringArray(forKey: ConfigPersistence.accessRules) ?? []
        )
    }

    func persistConfig(_ config: MobileConfig, defaults: UserDefaults = .standard) {
        defaults.set(config.deviceID, forKey: ConfigPersistence.deviceID)
        do {
            try keychainStore.save(config.peerProfileJSON, forKey: ConfigPersistence.peerProfile)
        } catch {
            bannerMessage = "Could not securely save the Peer profile"
            appendAppLog("Peer profile persistence failed: \(error.localizedDescription)")
        }
        Self.persistConfigDefaults(config, defaults: defaults)
    }

    /// Everything about a config that is not the Keychain-held Peer profile.
    nonisolated static func persistConfigDefaults(
        _ config: MobileConfig,
        defaults: UserDefaults = .standard
    ) {
        defaults.set(config.deviceID, forKey: ConfigPersistence.deviceID)
        defaults.set(config.localSocks5Listen, forKey: ConfigPersistence.localSocks5Listen)
        defaults.set(config.lanP2pEnabled, forKey: ConfigPersistence.lanP2pEnabled)
        defaults.set(config.lanRoutes, forKey: ConfigPersistence.lanRoutes)
        defaults.set(config.exportedLans, forKey: ConfigPersistence.exportedLans)
        defaults.set(config.tunnelFirst, forKey: ConfigPersistence.tunnelFirst)
        defaults.set(config.blockAllIncoming, forKey: ConfigPersistence.blockAllIncoming)
        defaults.set(config.autoConnect, forKey: ConfigPersistence.autoConnect)
        defaults.set(config.accessRules, forKey: ConfigPersistence.accessRules)
    }

    /// What the mesh publishes, and whether the tunnel is carrying it yet.
    ///
    /// The VPN's route set is fixed when the tunnel starts, so a prefix a Peer
    /// published afterwards is listed here and is not carried until the next
    /// reconnect. Comparing against what the tunnel actually started with —
    /// not against a freshly derived list — is what makes that detectable.
    nonisolated static func derivedRoutesSummary(
        following: Bool,
        derived: [String],
        running: Bool,
        routedAtConnect: Set<String>
    ) -> String {
        guard following else {
            return "Turned off \u{2014} only the networks below are used."
        }
        if derived.isEmpty {
            return "No device is publishing a network yet."
        }
        let joined = derived.joined(separator: ", ")
        // An empty start set means the app was relaunched while the tunnel
        // kept running, so what it started with is not known here. Claiming a
        // mismatch against nothing made the hint permanent.
        if running && !routedAtConnect.isEmpty && Set(derived) != routedAtConnect {
            return joined + " \u{2014} reconnect to route the newest"
        }
        return joined
    }

    nonisolated private static func loadOrCreateDeviceID(defaults: UserDefaults) -> String {
        if let stored = defaults.string(forKey: ConfigPersistence.deviceID),
           !stored.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return stored
        }
        let generated = UUID().uuidString
        defaults.set(generated, forKey: ConfigPersistence.deviceID)
        return generated
    }

    nonisolated private static func resolvedLocalSocks5Listen(_ stored: String?, fallback: String) -> String {
        let value = stored?.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let value, !value.isEmpty else {
            return fallback
        }
        if value == "127.0.0.1:1080" {
            return fallback
        }
        return value
    }

    nonisolated static func loadPersistedServiceState(
        defaults: UserDefaults = .standard,
        now: Date = Date()
    ) -> TunnelServiceState {
        let phase = defaults.string(forKey: ServiceStatePersistence.phase)
            .flatMap(TunnelServiceStatePhase.init(rawValue:))
            ?? .stopped
        let updatedAt = defaults.object(forKey: ServiceStatePersistence.updatedAt) as? Date ?? now
        let message = defaults.string(forKey: ServiceStatePersistence.message)
        let state = TunnelServiceState(
            phase: phase,
            message: message,
            updatedAt: updatedAt
        )
        return state.recovered(now: now)
    }

    nonisolated static func persistServiceState(
        _ state: TunnelServiceState,
        defaults: UserDefaults = .standard
    ) {
        defaults.set(state.phase.rawValue, forKey: ServiceStatePersistence.phase)
        defaults.set(state.updatedAt, forKey: ServiceStatePersistence.updatedAt)
        if let message = state.message, !message.isEmpty {
            defaults.set(message, forKey: ServiceStatePersistence.message)
        } else {
            defaults.removeObject(forKey: ServiceStatePersistence.message)
        }
    }

    func persistServiceState(
        _ phase: TunnelConnectionPhase,
        message: String? = nil,
        defaults: UserDefaults = .standard
    ) {
        let servicePhase: TunnelServiceStatePhase
        switch phase {
        case .disconnected:
            servicePhase = .stopped
        case .connecting:
            servicePhase = .connecting
        case .connected:
            servicePhase = .running
        case .failed:
            servicePhase = .failed
        }
        Self.persistServiceState(
            TunnelServiceState(phase: servicePhase, message: message, updatedAt: clock()),
            defaults: defaults
        )
    }

    func persistServiceState(
        _ phase: TunnelServiceStatePhase,
        message: String? = nil,
        defaults: UserDefaults = .standard
    ) {
        Self.persistServiceState(
            TunnelServiceState(phase: phase, message: message, updatedAt: clock()),
            defaults: defaults
        )
    }
}
