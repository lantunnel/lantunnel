import Foundation

public struct MobileConfig: Equatable, Sendable {
    public static let defaultLocalSocks5Listen = "127.0.0.1:0"
    /// Peer addresses live here, so this is routed whatever else is.
    public static let overlayRoute = "198.18.0.0/16"
    /// The manual list starts empty: it holds the networks someone adds, and
    /// the overlay is carried whatever it says.
    public static let defaultLanRoutes: [String] = []

    /// The runtime's own status object, wherever it arrives from.
    ///
    /// The PacketTunnel provider answers `status` with its own envelope and
    /// nests the runtime under `native_status`; the FFI hands the same object
    /// back bare. Reading one shape found nothing in the other.
    public static func runtimeStatusObject(_ rawStatus: String) -> [String: Any]? {
        guard let parsed = try? JSONSerialization.jsonObject(with: Data(rawStatus.utf8)),
              let root = parsed as? [String: Any]
        else { return nil }
        return (root["native_status"] as? [String: Any]) ?? root
    }

    /// The overlay, then whatever else was asked for.
    ///
    /// Peer addresses live on the overlay, so it is not a preference. It was
    /// an ordinary row in the manual list: deleting it and connecting left the
    /// tunnel carrying no route to any Peer address — every Peer unreachable,
    /// with nothing on screen saying why.
    public static func withOverlay(_ routes: [String]) -> [String] {
        [overlayRoute] + routes.filter { !$0.isEmpty && $0 != overlayRoute }
    }

    /// Every prefix the Tunnel currently publishes, plus the overlay.
    ///
    /// The list follows what Peers publish rather than what anyone typed, so
    /// it carries no ceiling. The overlay is unconditional — Peer addresses
    /// live there, so it is routed even when nothing else is.
    public static func routesFromExports(_ rawStatus: String) -> [String] {
        var prefixes = Set<String>()
        if let peers = (runtimeStatusObject(rawStatus)?["peer_directory"] as? [String: Any])?["peers"] as? [[String: Any]] {
            for peer in peers {
                for export in peer["exports"] as? [[String: Any]] ?? [] {
                    if let prefix = export["prefix"] as? String, !prefix.isEmpty {
                        prefixes.insert(prefix)
                    }
                }
            }
        }
        prefixes.insert(overlayRoute)
        return prefixes.sorted()
    }

    public static var defaults: MobileConfig {
        return MobileConfig(
            peerProfileJSON: "",
            deviceID: UUID().uuidString,
            localSocks5Listen: defaultLocalSocks5Listen,
            lanP2pEnabled: false,
            lanRoutes: defaultLanRoutes,
            exportedLans: [],
            tunnelFirst: false,
            blockAllIncoming: false,
            autoConnect: false,
            accessRules: []
        )
    }

    public var peerProfileJSON: String
    public var deviceID: String
    public var localSocks5Listen: String
    public var lanP2pEnabled: Bool
    /// Route what the Tunnel publishes rather than what someone typed.
    ///
    /// On by default: a phone that has to be told what its own Tunnel
    /// publishes is asking the owner to keep a list by hand.
    public var lanRoutes: [String]
    /// Networks this phone publishes to the Tunnel.
    public var exportedLans: [String]
    /// Keep a locally reachable network on the local path.
    public var tunnelFirst: Bool
    /// Refuse every destination, including ones this device publishes.
    public var blockAllIncoming: Bool
    /// Reconnect the imported Peer profile when the app opens.
    public var autoConnect: Bool
    /// Named access rules, one per line: `allow|deny <target> <tcp|udp> [port]`.
    public var accessRules: [String]
    /// The Access policy exactly as the shared UI writes it.
    ///
    /// `{"allow":[...],"deny":[...]}` with the same rule objects the engine
    /// takes and the desktop edits. The line list above was only ever a
    /// mobile-UI storage format; it is still read so an existing install keeps
    /// its rules, and never written again.
    public var clientAccessJSON: String

    public init() {
        self = Self.defaults
    }

    public init(
        peerProfileJSON: String,
        deviceID: String = UUID().uuidString,
        localSocks5Listen: String,
        lanP2pEnabled: Bool,
        lanRoutes: [String],
        exportedLans: [String] = [],
        tunnelFirst: Bool = false,
        blockAllIncoming: Bool = false,
        autoConnect: Bool = false,
        accessRules: [String] = [],
        clientAccessJSON: String = ""
    ) {
        self.peerProfileJSON = peerProfileJSON
        self.deviceID = deviceID
        self.localSocks5Listen = localSocks5Listen
        self.lanP2pEnabled = lanP2pEnabled
        self.lanRoutes = lanRoutes
        self.exportedLans = exportedLans
        self.tunnelFirst = tunnelFirst
        self.blockAllIncoming = blockAllIncoming
        self.autoConnect = autoConnect
        self.accessRules = accessRules
        self.clientAccessJSON = clientAccessJSON
    }

    public func applyingPeerProfile(_ rawJSON: String) throws -> MobileConfig {
        var config = self
        config.peerProfileJSON = try Self.normalizedPeerProfileJSON(rawJSON)
        return config
    }

    /// The public identity of an imported profile.
    ///
    /// Everything the app shows about a profile comes from here, so the parts
    /// that must not be shown — the private key above all — have nowhere to
    /// appear even by accident.
    public struct PeerIdentity: Equatable, Sendable {
        public let tunnelId: String
        public let peerId: String
        public let overlayIP: String

    }

    public static func peerIdentity(_ rawJSON: String) -> PeerIdentity? {
        let trimmed = rawJSON.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let object = try? JSONSerialization.jsonObject(with: Data(trimmed.utf8)),
              let profile = object as? [String: Any],
              let peer = profile["peer"] as? [String: Any]
        else { return nil }
        return PeerIdentity(
            tunnelId: profile["tunnel_id"] as? String ?? "",
            peerId: peer["peer_id"] as? String ?? "",
            overlayIP: peer["overlay_ip"] as? String ?? ""
        )
    }

    /// An empty Allow list means open, so refusing everything is spelled out:
    /// every address, both families, both protocols.
    private func clientAccessPolicy() -> [String: Any] {
        let trimmed = clientAccessJSON.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty,
           let data = trimmed.data(using: .utf8),
           let policy = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            return [
                "allow": policy["allow"] as? [[String: Any]] ?? [],
                "deny": policy["deny"] as? [[String: Any]] ?? [],
            ]
        }
        return legacyClientAccessPolicy()
    }

    /// The policy an install from before the shared UI is still holding.
    public func legacyClientAccessPolicy() -> [String: Any] {
        // Empty means every Peer in the Tunnel may reach what this device
        // publishes. That was the only possible answer while a phone could
        // publish nothing; now that it can, its owner needs a way to say no.
        var allow: [[String: Any]] = []
        var deny: [[String: Any]] = []
        for line in accessRules.map({ $0.trimmingCharacters(in: .whitespaces) }) where !line.isEmpty {
            guard let (list, rule) = Self.accessRule(from: line) else { continue }
            if list == "allow" { allow.append(rule) } else { deny.append(rule) }
        }

        guard blockAllIncoming else { return ["allow": allow, "deny": deny] }

        // Both families, both protocols. Naming one family left every IPv6
        // destination open while the screen said nothing was reachable.
        let closedDeny: [[String: Any]] = ["0.0.0.0/0", "::/0"].flatMap { cidr in
            ["tcp", "udp"].map { proto in
                [
                    "target": ["type": "cidr", "value": cidr],
                    "protocol": proto,
                    "port": ["type": "any"],
                ] as [String: Any]
            }
        }
        return ["allow": allow, "deny": deny + closedDeny]
    }

    /// The first stored line that is not a rule, or nil when every line reads.
    ///
    /// clientAccessJSON skipped a line it could not read, which left the owner
    /// looking at a restriction that was not in force. Connect asks first now,
    /// and refuses, the way Android does.
    func unreadableAccessRule() -> String? {
        accessRules
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .first { Self.accessRule(from: $0) == nil }
    }

    /// One rule line into the wire shape the engine compiles.
    static func accessRule(from line: String) -> (String, [String: Any])? {
        let parts = line.split(separator: " ", omittingEmptySubsequences: true).map(String.init)
        guard parts.count == 3 || parts.count == 4 else { return nil }
        let list = parts[0].lowercased()
        guard list == "allow" || list == "deny" else { return nil }
        let proto = parts[2].lowercased()
        guard proto == "tcp" || proto == "udp" else { return nil }

        let target = parts[1]
        let targetJSON: [String: Any]
        if target.lowercased() == "this_peer" {
            targetJSON = ["type": "this_peer"]
        } else if target.contains("/") {
            targetJSON = ["type": "cidr", "value": target]
        } else if target.allSatisfy({ $0.isNumber || $0 == "." }) {
            targetJSON = ["type": "ip", "value": target]
        } else {
            targetJSON = ["type": "host", "value": target]
        }

        var portJSON: [String: Any] = ["type": "any"]
        if parts.count == 4 {
            guard let port = Int(parts[3]), (1...65535).contains(port) else { return nil }
            portJSON = ["type": "exact", "value": port]
        }
        return (list, ["target": targetJSON, "protocol": proto, "port": portJSON])
    }

    public func with(lanRoutes: [String]) -> MobileConfig {
        var config = self
        config.lanRoutes = lanRoutes
        return config
    }

    public func startRequestJSON(logLevel: String) throws -> String {
        _ = try RouteValidator.validate(lanRoutes)
        let profileJSON = try Self.normalizedPeerProfileJSON(peerProfileJSON)
        let profileData = Data(profileJSON.utf8)
        let profile = try JSONSerialization.jsonObject(with: profileData)
        let request: [String: Any] = [
            "peer_profile": profile,
            "device_id": deviceID,
            "local_socks5_listen": localSocks5Listen,
            "p2p_allow_lan_candidates": lanP2pEnabled,
            "log_level": logLevel,
            "client_access": clientAccessPolicy(),
            "exported_lans": exportedLans,
            "tunnel_first": tunnelFirst,
        ]
        let data = try JSONSerialization.data(withJSONObject: request, options: [.sortedKeys])
        return String(decoding: data, as: UTF8.self)
    }

    public static func normalizedPeerProfileJSON(_ rawJSON: String) throws -> String {
        let data = Data(rawJSON.trimmingCharacters(in: .whitespacesAndNewlines).utf8)
        guard
            let profile = try JSONSerialization.jsonObject(with: data) as? [String: Any],
            profile["version"] as? Int == 2,
            let tunnelID = profile["tunnel_id"] as? String,
            !tunnelID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            profile["peer"] is [String: Any]
        else {
            throw PeerProfileJSONError.invalidV2Profile
        }
        let normalized = try JSONSerialization.data(withJSONObject: profile, options: [.sortedKeys])
        return String(decoding: normalized, as: UTF8.self)
    }
}

public enum PeerProfileJSONError: Error, Equatable, LocalizedError {
    case invalidV2Profile

    public var errorDescription: String? {
        "That is not a Lantunnel Peer profile."
    }
}
