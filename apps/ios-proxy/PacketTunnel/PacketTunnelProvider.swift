import Foundation
@preconcurrency import NetworkExtension

final class PacketTunnelProvider: NEPacketTunnelProvider, @unchecked Sendable {
    private let runtime = NativeTunnelRuntime()
    private let lastStartErrorStore = PacketTunnelLastStartErrorStore()

    override func startTunnel(
        options: [String: NSObject]?,
        completionHandler: @escaping (Error?) -> Void
    ) {
        lastStartErrorStore.clear()
        let completion = PacketTunnelStartCompletion(completionHandler)

        do {
            let providerConfig = try PacketTunnelLaunchConfiguration.decode(
                from: (protocolConfiguration as? NETunnelProviderProtocol)?.providerConfiguration,
                options: options
            )
            _ = try RouteValidator.validate(providerConfig.includedRoutes)
            let settings = try PacketTunnelSettingsFactory.makeValidatedSettings(
                routes: providerConfig.includedRoutes,
                tunnelAddress: providerConfig.tunnelAddress,
                mtu: providerConfig.mtu,
                dnsServers: providerConfig.dnsServers
            )

            setTunnelNetworkSettings(settings) { [weak self, completion] error in
                guard let self else {
                    completion(PacketTunnelProviderError.providerReleased)
                    return
                }

                if let error {
                    NSLog("Lantunnel PacketTunnel network settings failed: %@", error.localizedDescription)
                    self.recordStartFailure("Network settings failed: \(error.localizedDescription)")
                    completion(error)
                    return
                }

                do {
                    try self.runtime.start(
                        requestJSON: providerConfig.startRequestJSON,
                        packetFlow: self.packetFlow,
                        tunnelAddress: providerConfig.tunnelAddress,
                        mtu: providerConfig.mtu
                    )
                    completion(nil)
                } catch {
                    NSLog("Lantunnel PacketTunnel native bridge unavailable: %@", error.localizedDescription)
                    self.recordStartFailure("Native runtime failed: \(error.localizedDescription)")
                    completion(error)
                }
            }
        } catch {
            NSLog("Lantunnel PacketTunnel startup configuration failed: %@", error.localizedDescription)
            recordStartFailure("Startup configuration failed: \(error.localizedDescription)")
            completion(error)
        }
    }

    override func stopTunnel(
        with reason: NEProviderStopReason,
        completionHandler: @escaping () -> Void
    ) {
        runtime.stop()
        completionHandler()
    }

    override func handleAppMessage(
        _ messageData: Data,
        completionHandler: ((Data?) -> Void)?
    ) {
        let command = PacketTunnelProviderCommand(data: messageData)
        let response: String

        switch command.name {
        case "status":
            response = runtime.statusJSON()
        case "logs":
            response = runtime.logsJSON(limit: command.limit ?? 200)
        case "clear_logs":
            response = Self.codeResponse(runtime.clearLogs())
        case "set_log_level":
            if let level = command.level, !level.isEmpty {
                response = Self.codeResponse(runtime.setLogLevel(level))
            } else {
                response = Self.errorResponse("set_log_level requires level")
            }
        case "log_config":
            response = runtime.logConfigJSON()
        default:
            response = Self.errorResponse("unknown provider command: \(command.name)")
        }

        completionHandler?(response.data(using: .utf8))
    }

    private static func codeResponse(_ code: Int32) -> String {
        let payload: [String: Any] = [
            "ok": code == TunnelProxyNativeBridge.ok,
            "code": Int(code),
        ]
        return jsonObject(payload, fallback: #"{"ok":false,"code":-5}"#)
    }

    private static func errorResponse(_ message: String) -> String {
        jsonObject(
            [
                "ok": false,
                "code": Int(TunnelProxyNativeBridge.invalidArgument),
                "error": message,
            ],
            fallback: #"{"ok":false,"code":-1,"error":"provider message failed"}"#
        )
    }

    private static func jsonObject(_ payload: [String: Any], fallback: String) -> String {
        guard JSONSerialization.isValidJSONObject(payload),
              let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]),
              let json = String(data: data, encoding: .utf8)
        else {
            return fallback
        }
        return json
    }

    private func recordStartFailure(_ message: String) {
        lastStartErrorStore.record(message)
    }
}

private struct PacketTunnelProviderCommand {
    let name: String
    let limit: Int?
    let level: String?

    init(data: Data) {
        if let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            name = (object["command"] as? String)
                ?? (object["cmd"] as? String)
                ?? "status"
            limit = Self.integer(object["limit"])
            level = object["level"] as? String
            return
        }

        let raw = String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        name = raw?.isEmpty == false ? raw! : "status"
        limit = nil
        level = nil
    }

    private static func integer(_ value: Any?) -> Int? {
        if let value = value as? Int {
            return value
        }
        if let value = value as? NSNumber {
            return value.intValue
        }
        return nil
    }
}

private final class PacketTunnelStartCompletion: @unchecked Sendable {
    private let completion: (Error?) -> Void

    init(_ completion: @escaping (Error?) -> Void) {
        self.completion = completion
    }

    func callAsFunction(_ error: Error?) {
        completion(error)
    }
}

private enum PacketTunnelProviderError: Error, LocalizedError {
    case providerReleased

    var errorDescription: String? {
        switch self {
        case .providerReleased:
            return "Packet Tunnel provider was released during startup"
        }
    }
}
