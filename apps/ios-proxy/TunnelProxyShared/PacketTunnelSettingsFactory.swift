import Foundation
import NetworkExtension

enum PacketTunnelSettingsFactory {
    static let defaultIncludedRoutes = PacketTunnelLaunchConfiguration.defaultIncludedRoutes

    static func makeSettings(
        routes: [String],
        tunnelAddress: String,
        mtu: Int = PacketTunnelLaunchConfiguration.defaultMTU,
        dnsServers: [String] = []
    ) -> NEPacketTunnelNetworkSettings {
        let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: tunnelAddress)
        settings.ipv4Settings = NEIPv4Settings(
            addresses: [tunnelAddress],
            subnetMasks: ["255.255.255.255"]
        )
        settings.ipv4Settings?.includedRoutes = (try? includedRoutes(from: routes)) ?? []
        settings.mtu = NSNumber(value: mtu)

        if !dnsServers.isEmpty {
            settings.dnsSettings = NEDNSSettings(servers: dnsServers)
        }

        return settings
    }

    static func makeValidatedSettings(
        routes: [String],
        tunnelAddress: String,
        mtu: Int = PacketTunnelLaunchConfiguration.defaultMTU,
        dnsServers: [String] = []
    ) throws -> NEPacketTunnelNetworkSettings {
        let includedRoutes = try includedRoutes(from: routes)
        let settings = makeSettings(
            routes: [],
            tunnelAddress: tunnelAddress,
            mtu: mtu,
            dnsServers: dnsServers
        )
        settings.ipv4Settings?.includedRoutes = includedRoutes
        return settings
    }

    static func includedRoutes(from routes: [String]) throws -> [NEIPv4Route] {
        try routes.map { route in
            let cidr = try IPv4CIDR(route)
            return NEIPv4Route(
                destinationAddress: cidr.networkAddress,
                subnetMask: cidr.subnetMask
            )
        }
    }
}

enum PacketTunnelRouteError: Error, Equatable, LocalizedError {
    case invalidCIDR(String)
    case invalidPrefix(String)
    case nonPrivateRoute(String)

    var errorDescription: String? {
        switch self {
        case .invalidCIDR(let route):
            return "Invalid IPv4 CIDR route: \(route)"
        case .invalidPrefix(let route):
            return "Invalid IPv4 CIDR prefix: \(route)"
        case .nonPrivateRoute(let route):
            return "IPv4 route must stay inside private LAN or link-local ranges: \(route)"
        }
    }
}

private struct IPv4CIDR {
    let networkAddress: String
    let subnetMask: String

    init(_ raw: String) throws {
        let parts = raw.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, let address = Self.parseIPv4(String(parts[0])) else {
            throw PacketTunnelRouteError.invalidCIDR(raw)
        }

        guard let prefix = Int(parts[1]), (1...32).contains(prefix) else {
            throw PacketTunnelRouteError.invalidPrefix(raw)
        }

        let mask = Self.mask(prefix: prefix)
        let network = address & mask
        let broadcast = network | ~mask

        guard Self.rangeIsPrivate(start: network, end: broadcast) else {
            throw PacketTunnelRouteError.nonPrivateRoute(raw)
        }

        networkAddress = Self.formatIPv4(network)
        subnetMask = Self.formatIPv4(mask)
    }

    private static func parseIPv4(_ raw: String) -> UInt32? {
        let octets = raw.split(separator: ".", omittingEmptySubsequences: false)
        guard octets.count == 4 else {
            return nil
        }

        var value: UInt32 = 0
        for octet in octets {
            guard let number = UInt8(String(octet)) else {
                return nil
            }
            value = (value << 8) | UInt32(number)
        }
        return value
    }

    private static func mask(prefix: Int) -> UInt32 {
        UInt32.max << UInt32(32 - prefix)
    }

    private static func formatIPv4(_ value: UInt32) -> String {
        [
            (value >> 24) & 0xff,
            (value >> 16) & 0xff,
            (value >> 8) & 0xff,
            value & 0xff,
        ]
        .map(String.init)
        .joined(separator: ".")
    }

    private static func rangeIsPrivate(start: UInt32, end: UInt32) -> Bool {
        privateRanges.contains { privateRange in
            start >= privateRange.start && end <= privateRange.end
        }
    }

    private static let privateRanges: [(start: UInt32, end: UInt32)] = [
        (0x0a00_0000, 0x0aff_ffff),
        (0xac10_0000, 0xac1f_ffff),
        (0xc0a8_0000, 0xc0a8_ffff),
        (0xa9fe_0000, 0xa9fe_ffff),
        // 198.18.0.0/15 — the overlay every Peer address sits in.
        (0xc612_0000, 0xc613_ffff),
    ]
}
