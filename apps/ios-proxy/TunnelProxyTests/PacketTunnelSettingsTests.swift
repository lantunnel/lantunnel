import NetworkExtension
import XCTest

@testable import TunnelProxy

final class PacketTunnelSettingsTests: XCTestCase {
    func testMakeSettingsIncludesConfiguredIPv4Routes() {
        let settings = PacketTunnelSettingsFactory.makeSettings(
            routes: ["192.168.0.0/16", "10.0.0.0/8"],
            tunnelAddress: "10.255.0.2"
        )

        XCTAssertEqual(settings.ipv4Settings?.includedRoutes?.count, 2)
        XCTAssertEqual(settings.ipv4Settings?.addresses, ["10.255.0.2"])
    }

    func testIncludedRoutesRejectPublicIPv4CIDR() {
        XCTAssertThrowsError(
            try PacketTunnelSettingsFactory.includedRoutes(from: ["8.8.8.0/24"])
        ) { error in
            XCTAssertEqual(
                error as? PacketTunnelRouteError,
                PacketTunnelRouteError.nonPrivateRoute("8.8.8.0/24")
            )
        }
    }

    func testIncludedRoutesNormalizesHostAddressToNetworkAddress() throws {
        let routes = try PacketTunnelSettingsFactory.includedRoutes(from: ["192.168.1.25/24"])

        XCTAssertEqual(routes.first?.destinationAddress, "192.168.1.0")
        XCTAssertEqual(routes.first?.destinationSubnetMask, "255.255.255.0")
    }

    func testIncludedRoutesAcceptsLinkLocalIPv4CIDR() throws {
        let routes = try PacketTunnelSettingsFactory.includedRoutes(from: ["169.254.7.8/16"])

        XCTAssertEqual(routes.first?.destinationAddress, "169.254.0.0")
        XCTAssertEqual(routes.first?.destinationSubnetMask, "255.255.0.0")
    }
}
