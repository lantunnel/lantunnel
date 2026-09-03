import Foundation
import Security

public final class KeychainStore {
    public let service: String
    public let accessGroup: String?

    public init(
        service: String = Bundle.main.bundleIdentifier ?? "com.buhuipao.tunnelproxy.ios",
        accessGroup: String? = nil
    ) {
        self.service = service
        self.accessGroup = accessGroup
    }

    public func save(_ value: String, forKey key: String) throws {
        guard let data = value.data(using: .utf8) else {
            throw KeychainStoreError.invalidStringEncoding
        }

        try delete(key, ignoreMissing: true)

        var item = baseQuery(forKey: key)
        item[kSecValueData as String] = data
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly

        let status = SecItemAdd(item as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw KeychainStoreError.unexpectedStatus(status)
        }
    }

    public func loadString(forKey key: String) throws -> String? {
        var query = baseQuery(forKey: key)
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        query[kSecReturnData as String] = true

        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess else {
            throw KeychainStoreError.unexpectedStatus(status)
        }
        guard let data = result as? Data, let value = String(data: data, encoding: .utf8) else {
            throw KeychainStoreError.invalidStringEncoding
        }
        return value
    }

    public func delete(_ key: String) throws {
        try delete(key, ignoreMissing: false)
    }

    private func delete(_ key: String, ignoreMissing: Bool) throws {
        let status = SecItemDelete(baseQuery(forKey: key) as CFDictionary)
        guard status == errSecSuccess || (ignoreMissing && status == errSecItemNotFound) else {
            throw KeychainStoreError.unexpectedStatus(status)
        }
    }

    private func baseQuery(forKey key: String) -> [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        if let accessGroup {
            query[kSecAttrAccessGroup as String] = accessGroup
        }
        return query
    }
}

public enum KeychainStoreError: Error, Equatable, LocalizedError {
    case invalidStringEncoding
    case unexpectedStatus(OSStatus)

    public var errorDescription: String? {
        switch self {
        case .invalidStringEncoding:
            return "Keychain value is not valid UTF-8."
        case let .unexpectedStatus(status):
            return "Keychain operation failed with status \(status)."
        }
    }
}
