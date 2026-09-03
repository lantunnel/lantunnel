import XCTest

@testable import TunnelProxy

final class PacketTunnelLaunchConfigurationTests: XCTestCase {
    func testProviderConfigurationRoundTripsCanonicalKeys() throws {
        let launchConfiguration = PacketTunnelLaunchConfiguration(
            includedRoutes: ["10.1.2.0/24"],
            tunnelAddress: "10.255.0.9",
            mtu: 1280,
            dnsServers: ["1.1.1.1"],
            startRequestJSON: #"{"tunnel_id":"tid-1"}"#
        )

        let providerConfiguration = launchConfiguration.makeProviderConfiguration()
        let decoded = try PacketTunnelLaunchConfiguration.decode(from: providerConfiguration)

        XCTAssertEqual(decoded, launchConfiguration)
    }

    func testDefaultMTUMatchesAndroidMobileVPN() {
        XCTAssertEqual(PacketTunnelLaunchConfiguration.defaultMTU, 8500)
        XCTAssertEqual(PacketTunnelLaunchConfiguration().mtu, 8500)
    }

    func testDecodeAcceptsEarlyScaffoldKeys() throws {
        let providerConfiguration: [String: Any] = [
            "lan_routes": ["192.168.50.0/24"],
            "start_json": #"{"tunnel_id":"legacy"}"#,
        ]

        let decoded = try PacketTunnelLaunchConfiguration.decode(from: providerConfiguration)

        XCTAssertEqual(decoded.includedRoutes, ["192.168.50.0/24"])
        XCTAssertEqual(decoded.startRequestJSON, #"{"tunnel_id":"legacy"}"#)
    }

    func testStartOptionsOverrideSavedProviderConfiguration() throws {
        let saved = PacketTunnelLaunchConfiguration(
            includedRoutes: ["10.0.0.0/8"],
            startRequestJSON: #"{"tunnel_id":"saved"}"#
        )
        let live = PacketTunnelLaunchConfiguration(
            includedRoutes: ["172.16.0.0/12"],
            startRequestJSON: #"{"tunnel_id":"live"}"#
        )

        let decoded = try PacketTunnelLaunchConfiguration.decode(
            from: saved.makeProviderConfiguration(),
            options: live.makeStartOptions()
        )

        XCTAssertEqual(decoded.includedRoutes, live.includedRoutes)
        XCTAssertEqual(decoded.startRequestJSON, live.startRequestJSON)
    }

    func testTun2SocksConfigOmitsCredentialsWhenLocalProxyAuthIsDisabled() throws {
        let socks5 = try LocalSocks5RuntimeConfig(jsonString: #"{"local_socks5":{"host":"127.0.0.1","port":1080,"auth_enabled":false}}"#)

        let yaml = Tun2SocksConfigBuilder.makeConfig(
            socks5: socks5,
            tunnelAddress: "10.255.0.2",
            mtu: 8500
        )

        XCTAssertTrue(yaml.contains("  port: 1080"))
        XCTAssertTrue(yaml.contains("  address: \"127.0.0.1\""))
        XCTAssertFalse(yaml.contains("username:"))
        XCTAssertFalse(yaml.contains("password:"))
    }

    /// The flag decides, not the presence of the strings.
    ///
    /// A config carrying a credential pair but never turning auth on is not
    /// asking for auth; writing the pair anyway would offer credentials the
    /// local SOCKS listener never asked for.
    func testTun2SocksConfigOmitsCredentialsWhenAuthEnabledIsAbsent() throws {
        let socks5 = try LocalSocks5RuntimeConfig(jsonString: #"{"local_socks5":{"host":"127.0.0.1","port":1080,"username":"group-1","password":"secret-1"}}"#)

        let yaml = Tun2SocksConfigBuilder.makeConfig(
            socks5: socks5,
            tunnelAddress: "10.255.0.2",
            mtu: 8500
        )

        XCTAssertFalse(yaml.contains("username:"))
        XCTAssertFalse(yaml.contains("password:"))
    }

    /// Auth on with a real pair still carries it.
    func testTun2SocksConfigIncludesCredentialsWhenAuthEnabled() throws {
        let socks5 = try LocalSocks5RuntimeConfig(jsonString: #"{"local_socks5":{"host":"127.0.0.1","port":1080,"auth_enabled":true,"username":"group-1","password":"secret-1"}}"#)

        let yaml = Tun2SocksConfigBuilder.makeConfig(
            socks5: socks5,
            tunnelAddress: "10.255.0.2",
            mtu: 8500
        )

        XCTAssertTrue(yaml.contains("  username: \"group-1\""))
        XCTAssertTrue(yaml.contains("  password: \"secret-1\""))
    }

    /// A runtime config with no credentials is the normal case, not an error.
    ///
    /// This parse runs inside the PacketTunnel extension, where a throw is not
    /// shown to anyone — it just leaves the tunnel unestablished.
    func testRuntimeConfigParsesWithoutCredentials() throws {
        let socks5 = try LocalSocks5RuntimeConfig(jsonString: #"{"local_socks5":{"host":"127.0.0.1","port":1080}}"#)

        XCTAssertEqual(socks5.port, 1080)
        XCTAssertFalse(socks5.authEnabled)
    }
}
