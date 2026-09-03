import Combine
import Foundation

public enum MobileLogLevel: String, CaseIterable, Codable, Equatable, Sendable {
    case debug
    case info
    case warning
    case error
}

public struct MobileLogEntry: Identifiable, Equatable, Sendable {
    public let id: UUID
    public let timestamp: Date
    public let level: MobileLogLevel
    public let message: String
    public let subsystem: String?

    public init(
        id: UUID = UUID(),
        timestamp: Date = Date(),
        level: MobileLogLevel,
        message: String,
        subsystem: String? = nil
    ) {
        self.id = id
        self.timestamp = timestamp
        self.level = level
        self.message = message
        self.subsystem = subsystem
    }
}

@MainActor
public final class MobileLogStore: ObservableObject {
    @Published public private(set) var entries: [MobileLogEntry]

    public init(entries: [MobileLogEntry] = []) {
        self.entries = entries
    }

    public func append(_ message: String, level: MobileLogLevel = .info, subsystem: String? = nil) {
        entries.append(MobileLogEntry(level: level, message: message, subsystem: subsystem))
    }

    public func append(_ entry: MobileLogEntry) {
        entries.append(entry)
    }

    public func clear() {
        entries.removeAll()
    }
}
