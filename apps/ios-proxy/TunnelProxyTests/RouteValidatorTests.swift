import XCTest
@testable import TunnelProxy

final class RouteValidatorTests: XCTestCase {
    func testValidatesTheDefaultRoute() throws {
        // Peer addresses live in the overlay, and that is the one network a
        // Client always needs — so it is carried whatever the manual list says
        // rather than sitting in it as a row someone can delete.
        let validated = try RouteValidator.validate(
            MobileConfig.withOverlay(MobileConfig.defaultLanRoutes)
        )

        XCTAssertEqual(validated.routes, ["198.18.0.0/16"])
    }

    /// Routes follow what Peers publish, so a ceiling would let a large Tunnel
    /// refuse a connection over a list nobody typed.
    func testAcceptsMoreRoutesThanAnyoneWouldType() throws {
        let routes = (0...32).map { "10.0.\($0).0/24" }

        XCTAssertEqual(try RouteValidator.validate(routes).routes.count, routes.count)
    }

    func testNormalizesWhitespaceAndHostBits() throws {
        let validated = try RouteValidator.validate([" 192.168.1.42/24 "])

        XCTAssertEqual(validated.routes, ["192.168.1.0/24"])
    }

    func testRejectsPublicRoutes() {
        XCTAssertThrowsError(try RouteValidator.validate(["8.8.8.0/24"]))
    }

    func testRejectsInvalidPrefixes() {
        XCTAssertThrowsError(try RouteValidator.validate(["10.0.0.0/0"]))
        XCTAssertThrowsError(try RouteValidator.validate(["10.0.0.0/33"]))
    }

    func testRejectsRoutesThatEscapePrivateRange() {
        XCTAssertThrowsError(try RouteValidator.validate(["172.16.0.0/11"]))
    }

    func testAcceptsLinkLocalRoutes() throws {
        let validated = try RouteValidator.validate(["169.254.7.8/16"])

        XCTAssertEqual(validated.routes, ["169.254.0.0/16"])
    }
}
