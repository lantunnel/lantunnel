import Foundation
import XCTest
@testable import TunnelProxy

final class MobileConfigTests: XCTestCase {
    func testDefaultsMatchMobileContract() {
        let config = MobileConfig.defaults

        XCTAssertTrue(config.peerProfileJSON.isEmpty)
        XCTAssertFalse(config.deviceID.isEmpty)
        XCTAssertEqual(config.localSocks5Listen, "127.0.0.1:0")
        XCTAssertFalse(config.lanP2pEnabled)
        // The manual list starts empty; the overlay is not something someone
        // adds or removes, so it is not a row in it.
        XCTAssertTrue(config.lanRoutes.isEmpty)
    }

    func testStartRequestJSONUsesOnlyPeerProfileForTunnelIdentity() throws {
        let config = MobileConfig(
            peerProfileJSON: #"{"version":2,"tunnel_id":"tid-5","peer":{"peer_id":"peer-5"}}"#,
            deviceID: "4BFC8C69-4307-4F8F-9593-39F54121B702",
            localSocks5Listen: "127.0.0.1:1081",
            lanP2pEnabled: false,
            lanRoutes: ["10.1.2.0/24"]
        )

        let data = try XCTUnwrap(config.startRequestJSON(logLevel: "debug").data(using: .utf8))
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])

        let profile = try XCTUnwrap(json["peer_profile"] as? [String: Any])
        XCTAssertEqual(profile["tunnel_id"] as? String, "tid-5")
        XCTAssertNil(json["tunnel_id"])
        XCTAssertNil(json["tunnel_key"])
        XCTAssertEqual(json["device_id"] as? String, "4BFC8C69-4307-4F8F-9593-39F54121B702")
        XCTAssertNil(json["platform_url"])
        XCTAssertEqual(json["local_socks5_listen"] as? String, "127.0.0.1:1081")
        XCTAssertNil(json["local_proxy_auth_enabled"])
        XCTAssertNil(json["p2p_enabled"])
        XCTAssertNil(json["peer_client_id"])
        XCTAssertEqual(json["p2p_allow_lan_candidates"] as? Bool, false)
        XCTAssertEqual(json["log_level"] as? String, "debug")
        XCTAssertEqual(Set(json.keys), Set([
            "client_access",
            "device_id",
            "exported_lans",
            "tunnel_first",
            "local_socks5_listen",
            "log_level",
            "p2p_allow_lan_candidates",
            "peer_profile",
        ]))
    }

    func testStartRequestJSONValidatesRoutes() {
        let config = MobileConfig.defaults.with(lanRoutes: ["1.1.1.0/24"])

        XCTAssertThrowsError(try config.startRequestJSON(logLevel: "info"))
    }

    func testStartRequestJSONAcceptsASecondPrivateLANRoute() {
        let config = MobileConfig(
            peerProfileJSON: #"{"version":2,"tunnel_id":"tid-5","peer":{"peer_id":"peer-5"}}"#,
            localSocks5Listen: "127.0.0.1:1081",
            lanP2pEnabled: false,
            lanRoutes: [MobileConfig.overlayRoute, "10.0.0.0/8"]
        )

        XCTAssertNoThrow(try config.startRequestJSON(logLevel: "info"))
    }

    func testLoadPersistedConfigMigratesLegacyFixedSocks5PortToEphemeral() {
        let suiteName = "MobileConfigTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer {
            defaults.removePersistentDomain(forName: suiteName)
        }
        defaults.set("127.0.0.1:1080", forKey: "TunnelProxy.config.localSocks5Listen")

        let config = TunnelAppModel.loadPersistedConfig(defaults: defaults)

        XCTAssertEqual(config.localSocks5Listen, "127.0.0.1:0")
    }

    /// The switch has to reach the policy, not just the struct.
    ///
    /// A removed switch once kept writing its last value into client_access,
    /// so a device that had ever turned it on stayed permanently unreachable.
    /// The inverse is just as bad: a switch on screen that changes nothing.
    func testBlockingEverythingClosesBothFamiliesOnBothProtocols() throws {
        var config = MobileConfig.defaults
        config.peerProfileJSON = Self.profileJSON
        config.blockAllIncoming = true

        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(config.startRequestJSON(logLevel: "info").utf8))
                as? [String: Any]
        )
        let access = try XCTUnwrap(json["client_access"] as? [String: Any])
        let deny = try XCTUnwrap(access["deny"] as? [[String: Any]])

        XCTAssertEqual(deny.count, 4)
        let families = Set(deny.compactMap { ($0["target"] as? [String: Any])?["value"] as? String })
        XCTAssertEqual(families, ["0.0.0.0/0", "::/0"])
    }

    func testAnOpenPhoneStillSendsAnEmptyPolicy() throws {
        var config = MobileConfig.defaults
        config.peerProfileJSON = Self.profileJSON

        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(config.startRequestJSON(logLevel: "info").utf8))
                as? [String: Any]
        )
        let access = try XCTUnwrap(json["client_access"] as? [String: Any])

        XCTAssertEqual((access["deny"] as? [[String: Any]])?.count, 0)
    }

    private static let profileJSON = #"{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}"#

    /// Named rules, the way the desktop has them.
    func testANamedAllowRuleReachesThePolicy() throws {
        var config = MobileConfig.defaults
        config.peerProfileJSON = Self.profileJSON
        config.accessRules = ["allow 192.168.7.0/24 tcp 22"]

        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(config.startRequestJSON(logLevel: "info").utf8))
                as? [String: Any]
        )
        let access = try XCTUnwrap(json["client_access"] as? [String: Any])
        let allow = try XCTUnwrap(access["allow"] as? [[String: Any]])

        XCTAssertEqual(allow.count, 1)
        let target = try XCTUnwrap(allow[0]["target"] as? [String: Any])
        XCTAssertEqual(target["type"] as? String, "cidr")
        XCTAssertEqual(target["value"] as? String, "192.168.7.0/24")
    }

    func testARuleWithoutAPortMeansEveryPort() throws {
        let rule = try XCTUnwrap(MobileConfig.accessRule(from: "deny 10.0.0.5 udp"))
        let port = try XCTUnwrap(rule.1["port"] as? [String: Any])

        XCTAssertEqual(port["type"] as? String, "any")
        XCTAssertEqual((rule.1["target"] as? [String: Any])?["type"] as? String, "ip")
    }

    func testALineThatIsNotARuleIsRefused() {
        XCTAssertNil(MobileConfig.accessRule(from: "this is not a rule"))
    }

    /// Refused, and then dropped: clientAccessJSON skipped the line, so the
    /// owner was left looking at a restriction that was not in force. The
    /// caller has to be able to ask before it starts, the way Android does.
    func testAnUnreadableLineCanBeFoundBeforeConnecting() {
        var config = MobileConfig.defaults
        config.accessRules = ["allow this_peer tcp", "198.18.0.0/16"]

        XCTAssertEqual(config.unreadableAccessRule(), "198.18.0.0/16")
    }

    func testEveryLineReadingMeansNothingToReport() {
        var config = MobileConfig.defaults
        config.accessRules = ["allow this_peer tcp", "   ", "deny 10.0.0.0/8 udp 53"]

        XCTAssertNil(config.unreadableAccessRule())
    }
}
