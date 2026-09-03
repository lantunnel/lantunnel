import Foundation

enum MobileTrafficStatusNormalizer {
    static func normalizedProviderStatus(_ providerStatus: [String: Any]) -> [String: Any] {
        var root = providerStatus
        var nativeStatus = dictionaryValue(root["native_status"]) ?? root
        var connection = dictionaryValue(nativeStatus["connection"])
            ?? dictionaryValue(root["connection"])
            ?? [:]

        let localTraffic = localTraffic(root: root, nativeStatus: nativeStatus)
        guard hasPositiveTraffic(tx: localTraffic.tx, rx: localTraffic.rx) else {
            return providerStatus
        }

        let pathMode = stringValue(connection["path_mode"])
            ?? stringValue(connection["pathMode"])
            ?? stringValue(nativeStatus["path_mode"])
            ?? stringValue(nativeStatus["pathMode"])
            ?? stringValue(root["path_mode"])
            ?? stringValue(root["pathMode"])
        guard let normalizedPath = pathMode?.lowercased() else {
            return providerStatus
        }

        var traffic = dictionaryValue(connection["traffic"])
            ?? dictionaryValue(nativeStatus["traffic"])
            ?? dictionaryValue(root["traffic"])
            ?? [:]

        switch normalizedPath {
        case "p2p":
            mergeCounter("p2p_tx_bytes", localTraffic.tx, into: &traffic)
            mergeCounter("p2p_rx_bytes", localTraffic.rx, into: &traffic)
        case "relay":
            mergeCounter("relay_tx_bytes", localTraffic.tx, into: &traffic)
            mergeCounter("relay_rx_bytes", localTraffic.rx, into: &traffic)
        default:
            return providerStatus
        }

        connection["traffic"] = traffic
        nativeStatus["connection"] = connection
        root["native_status"] = nativeStatus
        return root
    }

    private static func localTraffic(root: [String: Any], nativeStatus: [String: Any]) -> (tx: Int64?, rx: Int64?) {
        var tx: Int64?
        var rx: Int64?

        if let packetBridge = dictionaryValue(root["packet_bridge"])
            ?? dictionaryValue(nativeStatus["packet_bridge"]) {
            tx = maxCounter(tx, intValue(packetBridge["bytes_to_tun2socks"]))
            rx = maxCounter(rx, intValue(packetBridge["bytes_from_tun2socks"]))
        }

        if let tun2socksStats = dictionaryValue(root["tun2socks_stats"])
            ?? dictionaryValue(nativeStatus["tun2socks_stats"]) {
            tx = maxCounter(tx, intValue(tun2socksStats["tx_bytes"]))
            rx = maxCounter(
                rx,
                intValue(tun2socksStats["rx_payload_bytes"])
                    ?? intValue(tun2socksStats["rx_bytes"])
            )
        }

        return (tx, rx)
    }

    private static func mergeCounter(
        _ key: String,
        _ localValue: Int64?,
        into traffic: inout [String: Any]
    ) {
        let existing = intValue(traffic[key]) ?? 0
        traffic[key] = max(existing, localValue ?? 0)
    }

    private static func maxCounter(_ lhs: Int64?, _ rhs: Int64?) -> Int64? {
        guard let lhs else {
            return rhs
        }
        guard let rhs else {
            return lhs
        }
        return max(lhs, rhs)
    }

    private static func hasPositiveTraffic(tx: Int64?, rx: Int64?) -> Bool {
        (tx ?? 0) > 0 || (rx ?? 0) > 0
    }

    private static func dictionaryValue(_ raw: Any?) -> [String: Any]? {
        raw as? [String: Any]
    }

    private static func stringValue(_ raw: Any?) -> String? {
        if let value = raw as? String {
            let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed
        }
        if let value = raw as? NSNumber {
            return value.stringValue
        }
        return nil
    }

    private static func intValue(_ raw: Any?) -> Int64? {
        if let value = raw as? Int64 {
            return value
        }
        if let value = raw as? Int {
            return Int64(value)
        }
        if let value = raw as? UInt64 {
            return Int64(clamping: value)
        }
        if let value = raw as? NSNumber {
            return value.int64Value
        }
        if let value = raw as? String {
            return Int64(value)
        }
        return nil
    }
}
