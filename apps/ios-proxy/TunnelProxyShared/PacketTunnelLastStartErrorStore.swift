import Foundation

struct PacketTunnelLastStartError: Equatable, Sendable {
    let message: String
    let recordedAt: Date
}

final class PacketTunnelLastStartErrorStore {
    static let appGroupIdentifier = "group.com.buhuipao.tunnelproxy.ios"

    private enum Key {
        static let message = "PacketTunnel.lastStartError.message"
        static let recordedAt = "PacketTunnel.lastStartError.recordedAt"
    }

    private let defaults: UserDefaults

    init(defaults: UserDefaults? = UserDefaults(suiteName: appGroupIdentifier)) {
        self.defaults = defaults ?? .standard
    }

    func record(_ message: String, at date: Date = Date()) {
        let trimmed = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return
        }
        defaults.set(trimmed, forKey: Key.message)
        defaults.set(date.timeIntervalSince1970, forKey: Key.recordedAt)
    }

    func load() -> PacketTunnelLastStartError? {
        guard let message = defaults.string(forKey: Key.message),
              !message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            return nil
        }
        let recordedAt = defaults.double(forKey: Key.recordedAt)
        return PacketTunnelLastStartError(
            message: message,
            recordedAt: Date(timeIntervalSince1970: recordedAt)
        )
    }

    func clear() {
        defaults.removeObject(forKey: Key.message)
        defaults.removeObject(forKey: Key.recordedAt)
    }
}
