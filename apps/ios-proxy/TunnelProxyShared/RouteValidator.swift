import Foundation

public struct ValidatedRoutes: Equatable, Sendable {
    public let routes: [String]

    public init(routes: [String]) {
        self.routes = routes
    }
}

public enum RouteValidator {
    public static func validate(_ routes: [String]) throws -> ValidatedRoutes {
        // No cap. Routes follow what Peers publish, so a ceiling would let a
        // large Tunnel refuse a connection over a list nobody typed.
        let normalizedRoutes = try routes.enumerated().map { index, route in
            try IPv4CIDR.parse(route, index: index).normalizedDescription
        }
        return ValidatedRoutes(routes: normalizedRoutes)
    }
}

public enum RouteValidationError: Error, Equatable, LocalizedError {
    case emptyRoute(index: Int)
    case invalidCIDR(String)
    case invalidIPv4(String)
    case invalidPrefix(String)
    case publicRoute(String)

    public var errorDescription: String? {
        switch self {
        case let .emptyRoute(index):
            return "Route at index \(index) is empty."
        case let .invalidCIDR(route):
            return "\(route) is not an IPv4 CIDR."
        case let .invalidIPv4(route):
            return "\(route) is not a valid IPv4 address."
        case let .invalidPrefix(route):
            return "\(route) must use a prefix from 1 through 32."
        case let .publicRoute(route):
            return "\(route) must be fully contained in a private IPv4 LAN or link-local range."
        }
    }
}

private struct IPv4CIDR {
    let network: UInt32
    let prefix: Int

    var normalizedDescription: String {
        "\(Self.format(network))/\(prefix)"
    }

    static func parse(_ rawRoute: String, index: Int) throws -> IPv4CIDR {
        let route = rawRoute.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !route.isEmpty else {
            throw RouteValidationError.emptyRoute(index: index)
        }

        let parts = route.split(separator: "/", maxSplits: 1, omittingEmptySubsequences: false)
        guard parts.count == 2 else {
            throw RouteValidationError.invalidCIDR(route)
        }

        let address = try parseIPv4(String(parts[0]), route: route)
        guard let prefix = Int(parts[1]), (1...32).contains(prefix) else {
            throw RouteValidationError.invalidPrefix(route)
        }

        let mask = mask(prefix: prefix)
        let network = address & mask
        let broadcast = network | ~mask
        guard isPrivateRange(network: network, broadcast: broadcast) else {
            throw RouteValidationError.publicRoute(route)
        }

        return IPv4CIDR(network: network, prefix: prefix)
    }

    private static func parseIPv4(_ rawAddress: String, route: String) throws -> UInt32 {
        let octets = rawAddress.split(separator: ".", omittingEmptySubsequences: false)
        guard octets.count == 4 else {
            throw RouteValidationError.invalidIPv4(route)
        }

        var address: UInt32 = 0
        for octet in octets {
            guard !octet.isEmpty, octet.allSatisfy(\.isNumber), let value = UInt8(octet) else {
                throw RouteValidationError.invalidIPv4(route)
            }
            address = (address << 8) | UInt32(value)
        }
        return address
    }

    private static func mask(prefix: Int) -> UInt32 {
        if prefix == 32 {
            return UInt32.max
        }
        return UInt32.max << (32 - prefix)
    }

    private static func isPrivateRange(network: UInt32, broadcast: UInt32) -> Bool {
        privateRanges.contains { range in
            range.contains(network) && range.contains(broadcast)
        }
    }

    private static var privateRanges: [ClosedRange<UInt32>] {
        [
            makeRange("10.0.0.0", prefix: 8),
            makeRange("172.16.0.0", prefix: 12),
            makeRange("192.168.0.0", prefix: 16),
            makeRange("169.254.0.0", prefix: 16),
            // Peer addresses live here. Without it the app lists Peers it can
            // never send a packet to.
            makeRange("198.18.0.0", prefix: 15),
        ]
    }

    private static func makeRange(_ address: String, prefix: Int) -> ClosedRange<UInt32> {
        let start = try! parseIPv4(address, route: address) & mask(prefix: prefix)
        let end = start | ~mask(prefix: prefix)
        return start...end
    }

    private static func format(_ address: UInt32) -> String {
        [
            (address >> 24) & 0xff,
            (address >> 16) & 0xff,
            (address >> 8) & 0xff,
            address & 0xff,
        ]
        .map(String.init)
        .joined(separator: ".")
    }
}
