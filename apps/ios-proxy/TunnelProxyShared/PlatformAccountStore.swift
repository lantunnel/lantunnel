import Foundation

/// The signed-in Platform account, and the local names a `.peer` cannot carry.
///
/// The token goes in the Keychain, beside the Peer profile that already lives
/// there for the same reason: both are secrets, and a session token is the
/// smaller of the two. The names are not secrets and go in `UserDefaults`,
/// which is where every other non-secret preference already is.
///
/// The in-flight device code is deliberately not persisted. It lives for ten
/// minutes, and an app closed mid-approval should start over rather than resume
/// a grant whose code the owner can no longer see.
public final class PlatformAccountStore {
    public static let defaultPlatformURL = "https://lantunnel.app"

    /// A token that dies mid-request is worse than one refreshed early.
    private static let expiryGraceSeconds: UInt64 = 60

    private enum Key {
        static let accessToken = "platform_account_token"
        static let platformURL = "platform_account_url"
        static let expiresAt = "platform_account_expires_at"
        static let email = "platform_account_email"
        static let labels = "peer_labels_json"
    }

    public struct Session: Sendable {
        public let platformURL: String
        public let accessToken: String
        public let expiresAtUnix: UInt64
        public let email: String?

        public init(platformURL: String, accessToken: String, expiresAtUnix: UInt64, email: String?) {
            self.platformURL = platformURL
            self.accessToken = accessToken
            self.expiresAtUnix = expiresAtUnix
            self.email = email
        }
    }

    private let keychain: KeychainStore
    private let defaults: UserDefaults

    /// In memory only, for as long as one approval is outstanding.
    private var pendingDeviceCode: String?

    public init(keychain: KeychainStore = KeychainStore(), defaults: UserDefaults = .standard) {
        self.keychain = keychain
        self.defaults = defaults
    }

    private var nowUnix: UInt64 { UInt64(max(0, Date().timeIntervalSince1970)) }

    /// The stored session, or nil when there is none or it has expired.
    public func session() -> Session? {
        guard let token = (try? keychain.loadString(forKey: Key.accessToken))??
            .trimmingCharacters(in: .whitespacesAndNewlines),
            !token.isEmpty
        else { return nil }

        let expiresAt = UInt64(max(0, defaults.double(forKey: Key.expiresAt)))
        if expiresAt <= nowUnix + Self.expiryGraceSeconds {
            // Expired is signed out. Clearing here means no caller has to
            // reason about a half-signed-in state.
            signOut()
            return nil
        }
        let url = defaults.string(forKey: Key.platformURL)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let email = defaults.string(forKey: Key.email)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return Session(
            platformURL: (url?.isEmpty == false ? url! : Self.defaultPlatformURL),
            accessToken: token,
            expiresAtUnix: expiresAt,
            email: (email?.isEmpty == false) ? email : nil
        )
    }

    /// Returns false when the Keychain refused the token.
    ///
    /// Swallowing that would leave the expiry and the address recorded with no
    /// token behind them: the panel would flash "signed in" and then read back
    /// as signed out, with the device grant already spent and nothing said.
    @discardableResult
    public func store(_ session: Session) -> Bool {
        do {
            try keychain.save(session.accessToken, forKey: Key.accessToken)
        } catch {
            signOut()
            return false
        }
        defaults.set(session.platformURL, forKey: Key.platformURL)
        defaults.set(Double(session.expiresAtUnix), forKey: Key.expiresAt)
        defaults.set(session.email ?? "", forKey: Key.email)
        return true
    }

    /// Forgets the token and any grant still in flight. Repeatable.
    public func signOut() {
        pendingDeviceCode = nil
        try? keychain.delete(Key.accessToken)
        defaults.removeObject(forKey: Key.platformURL)
        defaults.removeObject(forKey: Key.expiresAt)
        defaults.removeObject(forKey: Key.email)
    }

    public func rememberPendingDeviceCode(_ code: String?) {
        let trimmed = code?.trimmingCharacters(in: .whitespacesAndNewlines)
        pendingDeviceCode = (trimmed?.isEmpty == false) ? trimmed : nil
    }

    public func currentPendingDeviceCode() -> String? { pendingDeviceCode }

    /// Gives up the in-flight grant, but only if it is still the one that
    /// started this poll.
    ///
    /// The owner can sign out or start over while a request is in the air.
    /// Storing the token that lands afterwards would sign them back in behind
    /// their own back, with the panel still showing signed out.
    public func releasePendingDeviceCode(matching code: String) -> Bool {
        guard pendingDeviceCode == code else { return false }
        pendingDeviceCode = nil
        return true
    }

    public func statusPayload() -> [String: Any] {
        guard let current = session() else {
            return [
                "signed_in": false,
                "platform_url": Self.defaultPlatformURL,
            ]
        }
        var payload: [String: Any] = [
            "signed_in": true,
            "expires_at_unix": current.expiresAtUnix,
            "platform_url": current.platformURL,
        ]
        if let email = current.email { payload["email"] = email }
        return payload
    }

    // MARK: - labels

    /// The longest name kept for one Tunnel or Peer, matching the desktop.
    public static let maxLabelCharacters = 64

    private func labels() -> [String: [String: String]] {
        guard let raw = defaults.string(forKey: Key.labels),
              let data = raw.data(using: .utf8),
              let decoded = try? JSONSerialization.jsonObject(with: data) as? [String: [String: String]]
        else { return [:] }
        return decoded
    }

    private func normalize(_ value: String?) -> String? {
        guard let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines),
              !trimmed.isEmpty
        else { return nil }
        return String(trimmed.prefix(Self.maxLabelCharacters))
    }

    /// Records the local names for one Tunnel. Empty names remove the entry.
    public func setLabels(tunnelID: String, tunnelName: String?, peerName: String?) {
        guard !tunnelID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        var stored = labels()
        var entry: [String: String] = [:]
        if let tunnel = normalize(tunnelName) { entry["tunnel_name"] = tunnel }
        if let peer = normalize(peerName) { entry["peer_name"] = peer }
        if entry.isEmpty { stored.removeValue(forKey: tunnelID) } else { stored[tunnelID] = entry }
        guard let data = try? JSONSerialization.data(withJSONObject: stored),
              let raw = String(data: data, encoding: .utf8)
        else { return }
        defaults.set(raw, forKey: Key.labels)
    }

    public func forgetLabels(tunnelID: String) {
        setLabels(tunnelID: tunnelID, tunnelName: nil, peerName: nil)
    }

    /// Writes whatever names are known for this Tunnel onto a summary object.
    public func decorate(_ summary: [String: Any], tunnelID: String) -> [String: Any] {
        guard let entry = labels()[tunnelID] else { return summary }
        var decorated = summary
        if let tunnel = entry["tunnel_name"] { decorated["tunnel_name"] = tunnel }
        if let peer = entry["peer_name"] { decorated["peer_name"] = peer }
        return decorated
    }
}
