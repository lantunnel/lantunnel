import Foundation

enum PacketTunnelConfigurationKey {
    static let packetTunnelConfigJSON = "packet_tunnel_config_json"
    static let packetTunnelConfigJSONCamel = "packetTunnelConfigJSON"
    static let configJSON = "config_json"
    static let configJSONCamel = "configJSON"

    static let includedRoutes = "included_routes"
    static let includedRoutesCamel = "includedRoutes"
    static let routes = "routes"
    static let lanRoutes = "lan_routes"

    static let tunnelAddress = "tunnel_address"
    static let tunnelAddressCamel = "tunnelAddress"
    static let mtu = "mtu"
    static let dnsServers = "dns_servers"
    static let dnsServersCamel = "dnsServers"

    static let startRequestJSON = "start_request_json"
    static let startRequestJSONCamel = "startRequestJSON"
    static let startJSON = "start_json"
}

struct PacketTunnelLaunchConfiguration: Equatable {
    static let defaultIncludedRoutes = [
        // Peer addresses live here. The previous default carried no Peer
        // address at all and swallowed the Wi-Fi the phone was standing on.
        "198.18.0.0/16",
    ]
    static let defaultTunnelAddress = "10.255.0.2"
    static let defaultMTU = 8500

    let includedRoutes: [String]
    let tunnelAddress: String
    let mtu: Int
    let dnsServers: [String]
    let startRequestJSON: String

    init(
        includedRoutes: [String] = Self.defaultIncludedRoutes,
        tunnelAddress: String = Self.defaultTunnelAddress,
        mtu: Int = Self.defaultMTU,
        dnsServers: [String] = [],
        startRequestJSON: String = "{}"
    ) {
        self.includedRoutes = includedRoutes
        self.tunnelAddress = tunnelAddress
        self.mtu = mtu
        self.dnsServers = dnsServers
        self.startRequestJSON = startRequestJSON
    }

    func makeProviderConfiguration(extra: [String: Any] = [:]) -> [String: Any] {
        var providerConfiguration = extra
        providerConfiguration[PacketTunnelConfigurationKey.packetTunnelConfigJSON] = encodedJSONString()
        providerConfiguration[PacketTunnelConfigurationKey.includedRoutes] = includedRoutes
        providerConfiguration[PacketTunnelConfigurationKey.tunnelAddress] = tunnelAddress
        providerConfiguration[PacketTunnelConfigurationKey.mtu] = mtu
        providerConfiguration[PacketTunnelConfigurationKey.dnsServers] = dnsServers
        providerConfiguration[PacketTunnelConfigurationKey.startRequestJSON] = startRequestJSON

        // Keep the first iOS build tolerant of the early scaffold key names.
        providerConfiguration[PacketTunnelConfigurationKey.lanRoutes] = includedRoutes
        providerConfiguration[PacketTunnelConfigurationKey.startJSON] = startRequestJSON
        return providerConfiguration
    }

    func makeStartOptions() -> [String: NSObject] {
        [
            PacketTunnelConfigurationKey.packetTunnelConfigJSON: encodedJSONString() as NSString,
            PacketTunnelConfigurationKey.includedRoutes: includedRoutes as NSArray,
            PacketTunnelConfigurationKey.tunnelAddress: tunnelAddress as NSString,
            PacketTunnelConfigurationKey.mtu: NSNumber(value: mtu),
            PacketTunnelConfigurationKey.dnsServers: dnsServers as NSArray,
            PacketTunnelConfigurationKey.startRequestJSON: startRequestJSON as NSString,
        ]
    }

    static func decode(
        from providerConfiguration: [String: Any]?,
        options: [String: NSObject]? = nil
    ) throws -> Self {
        var merged = providerConfiguration ?? [:]
        if let options {
            for (key, value) in options {
                merged[key] = value
            }
        }

        let base: Self
        if let jsonData = embeddedJSONData(in: merged) {
            base = try decode(fromJSONData: jsonData)
        } else {
            base = Self()
        }

        return Self(
            includedRoutes: stringArray(
                in: merged,
                keys: [
                    PacketTunnelConfigurationKey.includedRoutes,
                    PacketTunnelConfigurationKey.includedRoutesCamel,
                    PacketTunnelConfigurationKey.routes,
                    PacketTunnelConfigurationKey.lanRoutes,
                ]
            ) ?? base.includedRoutes,
            tunnelAddress: string(
                in: merged,
                keys: [
                    PacketTunnelConfigurationKey.tunnelAddress,
                    PacketTunnelConfigurationKey.tunnelAddressCamel,
                ]
            ) ?? base.tunnelAddress,
            mtu: integer(
                in: merged,
                keys: [PacketTunnelConfigurationKey.mtu]
            ) ?? base.mtu,
            dnsServers: stringArray(
                in: merged,
                keys: [
                    PacketTunnelConfigurationKey.dnsServers,
                    PacketTunnelConfigurationKey.dnsServersCamel,
                ]
            ) ?? base.dnsServers,
            startRequestJSON: string(
                in: merged,
                keys: [
                    PacketTunnelConfigurationKey.startRequestJSON,
                    PacketTunnelConfigurationKey.startRequestJSONCamel,
                    PacketTunnelConfigurationKey.startJSON,
                ]
            ) ?? base.startRequestJSON
        )
    }

    private func encodedJSONString() -> String {
        let payload: [String: Any] = [
            PacketTunnelConfigurationKey.includedRoutes: includedRoutes,
            PacketTunnelConfigurationKey.tunnelAddress: tunnelAddress,
            PacketTunnelConfigurationKey.mtu: mtu,
            PacketTunnelConfigurationKey.dnsServers: dnsServers,
            PacketTunnelConfigurationKey.startRequestJSON: startRequestJSON,
        ]

        guard JSONSerialization.isValidJSONObject(payload),
              let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]),
              let json = String(data: data, encoding: .utf8)
        else {
            return "{}"
        }
        return json
    }

    private static func decode(fromJSONData data: Data) throws -> Self {
        let decoder = JSONDecoder()
        let decoded = try decoder.decode(PacketTunnelJSONConfiguration.self, from: data)
        return Self(
            includedRoutes: decoded.includedRoutes
                ?? decoded.included_routes
                ?? decoded.routes
                ?? decoded.lan_routes
                ?? Self.defaultIncludedRoutes,
            tunnelAddress: decoded.tunnelAddress
                ?? decoded.tunnel_address
                ?? Self.defaultTunnelAddress,
            mtu: decoded.mtu ?? Self.defaultMTU,
            dnsServers: decoded.dnsServers
                ?? decoded.dns_servers
                ?? [],
            startRequestJSON: decoded.startRequestJSON
                ?? decoded.start_request_json
                ?? decoded.start_json
                ?? "{}"
        )
    }

    private static func embeddedJSONData(in providerConfiguration: [String: Any]) -> Data? {
        for key in [
            PacketTunnelConfigurationKey.packetTunnelConfigJSON,
            PacketTunnelConfigurationKey.packetTunnelConfigJSONCamel,
            PacketTunnelConfigurationKey.configJSON,
            PacketTunnelConfigurationKey.configJSONCamel,
        ] {
            if let raw = providerConfiguration[key] as? String {
                return raw.data(using: .utf8)
            }
            if let raw = providerConfiguration[key] as? NSString {
                return String(raw).data(using: .utf8)
            }
            if let data = providerConfiguration[key] as? Data {
                return data
            }
        }
        return nil
    }

    private static func string(in providerConfiguration: [String: Any], keys: [String]) -> String? {
        for key in keys {
            if let value = providerConfiguration[key] as? String, !value.isEmpty {
                return value
            }
            if let value = providerConfiguration[key] as? NSString, value.length > 0 {
                return String(value)
            }
        }
        return nil
    }

    private static func stringArray(in providerConfiguration: [String: Any], keys: [String]) -> [String]? {
        for key in keys {
            if let value = providerConfiguration[key] as? [String] {
                return value
            }
            if let value = providerConfiguration[key] as? NSArray {
                let strings = value.compactMap { $0 as? String }
                if strings.count == value.count {
                    return strings
                }
            }
            if let value = providerConfiguration[key] as? String,
               let data = value.data(using: .utf8),
               let decoded = try? JSONDecoder().decode([String].self, from: data) {
                return decoded
            }
            if let value = providerConfiguration[key] as? NSString,
               let data = String(value).data(using: .utf8),
               let decoded = try? JSONDecoder().decode([String].self, from: data) {
                return decoded
            }
        }
        return nil
    }

    private static func integer(in providerConfiguration: [String: Any], keys: [String]) -> Int? {
        for key in keys {
            if let value = providerConfiguration[key] as? Int {
                return value
            }
            if let value = providerConfiguration[key] as? NSNumber {
                return value.intValue
            }
        }
        return nil
    }
}

private struct PacketTunnelJSONConfiguration: Decodable {
    let includedRoutes: [String]?
    let included_routes: [String]?
    let routes: [String]?
    let lan_routes: [String]?
    let tunnelAddress: String?
    let tunnel_address: String?
    let mtu: Int?
    let dnsServers: [String]?
    let dns_servers: [String]?
    let startRequestJSON: String?
    let start_request_json: String?
    let start_json: String?
}

enum LocalSocks5RuntimeConfigError: Error, Equatable, LocalizedError {
    case runtimeConfigUnavailable(String)
    case invalidRuntimeConfig(String)

    var errorDescription: String? {
        switch self {
        case .runtimeConfigUnavailable(let message):
            return message
        case .invalidRuntimeConfig(let message):
            return message
        }
    }
}

struct LocalSocks5RuntimeConfig {
    let host: String
    let port: Int
    let authEnabled: Bool
    let username: String?
    let password: String?

    init(jsonString: String) throws {
        guard let data = jsonString.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            throw LocalSocks5RuntimeConfigError.invalidRuntimeConfig("runtimeConfigJSON was not valid JSON")
        }

        if object["ok"] as? Bool == false {
            let message = object["error"] as? String ?? "mobile runtime config unavailable"
            throw LocalSocks5RuntimeConfigError.runtimeConfigUnavailable(message)
        }

        guard let socks5 = object["local_socks5"] as? [String: Any] else {
            throw LocalSocks5RuntimeConfigError.invalidRuntimeConfig("runtimeConfigJSON missing local_socks5")
        }
        guard let host = socks5["host"] as? String, !host.isEmpty else {
            throw LocalSocks5RuntimeConfigError.invalidRuntimeConfig("runtimeConfigJSON missing local_socks5.host")
        }
        guard let port = Self.integer(in: socks5, key: "port"), (1...65_535).contains(port) else {
            throw LocalSocks5RuntimeConfigError.invalidRuntimeConfig("runtimeConfigJSON missing local_socks5.port")
        }
        // The runtime emits auth_enabled: false and never a credential pair;
        // defaulting to true made a missing key demand credentials that do not
        // exist.
        let authEnabled = Self.boolean(in: socks5, key: "auth_enabled") ?? false
        let username = socks5["username"] as? String
        let password = socks5["password"] as? String
        if authEnabled {
            guard let username, !username.isEmpty else {
                throw LocalSocks5RuntimeConfigError.invalidRuntimeConfig("runtimeConfigJSON missing local_socks5.username")
            }
            guard let password, !password.isEmpty else {
                throw LocalSocks5RuntimeConfigError.invalidRuntimeConfig("runtimeConfigJSON missing local_socks5.password")
            }
        }

        self.host = host
        self.port = port
        self.authEnabled = authEnabled
        self.username = username
        self.password = password
    }

    private static func integer(in object: [String: Any], key: String) -> Int? {
        if let value = object[key] as? Int {
            return value
        }
        if let value = object[key] as? NSNumber {
            return value.intValue
        }
        return nil
    }

    private static func boolean(in object: [String: Any], key: String) -> Bool? {
        if let value = object[key] as? Bool {
            return value
        }
        if let value = object[key] as? NSNumber {
            return value.boolValue
        }
        return nil
    }
}

enum Tun2SocksConfigBuilder {
    static func makeConfig(
        socks5: LocalSocks5RuntimeConfig,
        tunnelAddress: String,
        mtu: Int
    ) -> String {
        var lines = [
            "tunnel:",
            "  mtu: \(mtu)",
            "  multi-queue: false",
            "  ipv4: \(yamlQuoted(tunnelAddress))",
            "socks5:",
            "  port: \(socks5.port)",
            "  address: \(yamlQuoted(socks5.host))",
            "  udp: udp",
        ]
        if socks5.authEnabled {
            lines.append("  username: \(yamlQuoted(socks5.username ?? ""))")
            lines.append("  password: \(yamlQuoted(socks5.password ?? ""))")
        }
        lines.append(contentsOf: [
            "misc:",
            "  task-stack-size: 131072",
            "  tcp-buffer-size: 65536",
            "  udp-recv-buffer-size: 4194304",
            "  udp-copy-buffer-nums: 64",
            "  connect-timeout: 5000",
            "  tcp-read-write-timeout: 300000",
            "  udp-read-write-timeout: 60000",
            "  log-level: warn",
        ])
        return lines.joined(separator: "\n")
    }

    private static func yamlQuoted(_ value: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: [value], options: []),
              let json = String(data: data, encoding: .utf8),
              json.count >= 2
        else {
            return "\"\""
        }

        return String(json.dropFirst().dropLast())
    }
}
