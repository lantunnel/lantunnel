import Foundation

public enum TunnelStatus: Equatable, Sendable {
    case disconnected
    case connecting
    case connected(TunnelConnectionDetails)
    case disconnecting
    case failed(String)
    case unsupported(String)

    public var displayName: String {
        switch self {
        case .disconnected:
            return "Disconnected"
        case .connecting:
            return "Connecting"
        case .connected:
            return "Connected"
        case .disconnecting:
            return "Disconnecting"
        case .failed:
            return "Connection Failed"
        case .unsupported:
            return "Unsupported"
        }
    }

    public var isConnected: Bool {
        if case .connected = self {
            return true
        }
        return false
    }
}

public struct TunnelConnectionDetails: Equatable, Sendable {
    public var pathMode: TunnelPathMode
    public var transport: String
    public var peerClientId: String?
    public var bytesSent: UInt64
    public var bytesReceived: UInt64
    public var lastHeartbeatAt: Date?

    public init(
        pathMode: TunnelPathMode = .unknown,
        transport: String = "unknown",
        peerClientId: String? = nil,
        bytesSent: UInt64 = 0,
        bytesReceived: UInt64 = 0,
        lastHeartbeatAt: Date? = nil
    ) {
        self.pathMode = pathMode
        self.transport = transport
        self.peerClientId = peerClientId
        self.bytesSent = bytesSent
        self.bytesReceived = bytesReceived
        self.lastHeartbeatAt = lastHeartbeatAt
    }
}

public enum TunnelPathMode: String, CaseIterable, Equatable, Sendable {
    case unknown
    case p2p
    case relay

    public var displayName: String {
        switch self {
        case .unknown:
            return "Unknown"
        case .p2p:
            return "Direct"
        case .relay:
            return "Relay"
        }
    }
}
