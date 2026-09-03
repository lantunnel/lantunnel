import Foundation
import Darwin
@preconcurrency import NetworkExtension

#if canImport(HevSocks5Tunnel)
import HevSocks5Tunnel
#endif

#if canImport(TpMobileFfi)
import TpMobileFfi
#endif

enum NativeTunnelRuntimeError: Error, Equatable, LocalizedError {
    case bridgeUnavailable(String)
    case nativeStartFailed(code: Int32, message: String)
    case runtimeConfigUnavailable(String)
    case invalidRuntimeConfig(String)
    case packetBridgeUnavailable(String)
    case tun2SocksUnavailable(String)
    case tun2SocksExitedDuringStartup
    case tun2SocksStartFailed(code: Int32)

    var errorDescription: String? {
        switch self {
        case .bridgeUnavailable(let message):
            return message
        case .nativeStartFailed(let code, let message):
            return "\(message) (\(code))"
        case .runtimeConfigUnavailable(let message):
            return message
        case .invalidRuntimeConfig(let message):
            return message
        case .packetBridgeUnavailable(let message):
            return message
        case .tun2SocksUnavailable(let message):
            return message
        case .tun2SocksExitedDuringStartup:
            return "tun2socks exited during startup"
        case .tun2SocksStartFailed(let code):
            return "tun2socks start failed: \(code)"
        }
    }
}

final class NativeTunnelRuntime {
    static let bridgeUnavailableMessage = "Rust/tun2socks packet bridge is not linked in this build"

    private(set) var isRunning = false
    private(set) var lastError: NativeTunnelRuntimeError?

    private let nativeBridge = TunnelProxyNativeBridge()
    private var packetBridge: PacketTunnelFlowBridge?
    private var tun2Socks: Tun2SocksEngine?
    private var runtimeConfigJSON: String?
    private var tun2SocksConfigYAML: String?
    private var mtu = PacketTunnelLaunchConfiguration.defaultMTU
    private var tunnelAddress = PacketTunnelLaunchConfiguration.defaultTunnelAddress

    func start(
        requestJSON: String,
        packetFlow: NEPacketTunnelFlow,
        tunnelAddress: String,
        mtu: Int
    ) throws {
        if isRunning {
            stop()
        }

        lastError = nil
        isRunning = false
        self.mtu = mtu
        self.tunnelAddress = tunnelAddress

        let startCode = nativeBridge.startProxy(requestJSON: requestJSON)
        guard startCode == TunnelProxyNativeBridge.ok else {
            let error = NativeTunnelRuntimeError.nativeStartFailed(
                code: startCode,
                message: Self.nativeErrorMessage(
                    statusJSON: nativeBridge.statusJSON(),
                    fallback: "Rust proxy start failed"
                )
            )
            lastError = error
            throw error
        }

        var startingPacketBridge: PacketTunnelFlowBridge?
        var startingTun2Socks: Tun2SocksEngine?

        do {
            let runtimeConfigJSON = nativeBridge.runtimeConfigJSON()
            let socks5 = try LocalSocks5RuntimeConfig(jsonString: runtimeConfigJSON)
            let tun2SocksConfigYAML = Tun2SocksConfigBuilder.makeConfig(
                socks5: socks5,
                tunnelAddress: tunnelAddress,
                mtu: mtu
            )
            let packetBridge = try PacketTunnelFlowBridge(packetFlow: packetFlow, mtu: mtu)
            startingPacketBridge = packetBridge
            let tunnelFileDescriptor = try packetBridge.duplicateTunnelFileDescriptor()
            let tun2Socks = Tun2SocksEngine()
            startingTun2Socks = tun2Socks

            try tun2Socks.start(
                configYAML: tun2SocksConfigYAML,
                tunnelFileDescriptor: tunnelFileDescriptor
            )
            packetBridge.startPacketPump()

            self.runtimeConfigJSON = runtimeConfigJSON
            self.tun2SocksConfigYAML = tun2SocksConfigYAML
            self.packetBridge = packetBridge
            self.tun2Socks = tun2Socks
            startingPacketBridge = nil
            startingTun2Socks = nil
            isRunning = true
        } catch {
            startingPacketBridge?.stopPacketPump()
            startingTun2Socks?.stop()
            startingPacketBridge?.closeFileDescriptors()
            self.packetBridge?.stopPacketPump()
            self.tun2Socks?.stop()
            self.packetBridge?.closeFileDescriptors()
            packetBridge = nil
            tun2Socks = nil
            _ = nativeBridge.stopProxy()

            let runtimeError = (error as? NativeTunnelRuntimeError)
                ?? (error as? LocalSocks5RuntimeConfigError)?.nativeTunnelRuntimeError
                ?? NativeTunnelRuntimeError.invalidRuntimeConfig(error.localizedDescription)
            lastError = runtimeError
            isRunning = false
            throw runtimeError
        }
    }

    func stop() {
        packetBridge?.stopPacketPump()
        tun2Socks?.stop()
        packetBridge?.closeFileDescriptors()
        packetBridge = nil
        tun2Socks = nil
        runtimeConfigJSON = nil
        tun2SocksConfigYAML = nil
        _ = nativeBridge.stopProxy()
        isRunning = false
    }

    func statusJSON() -> String {
        let nativeStatus = Self.jsonValue(from: nativeBridge.statusJSON())
        let nativeStatusObject = nativeStatus as? [String: Any]
        let nativeBridgeAvailable = nativeStatusObject?["bridge_available"] as? Bool ?? false
        let lastErrorPayload: Any = lastError.map(Self.errorPayload) ?? NSNull()

        let payload: [String: Any] = [
            "running": isRunning,
            "bridge_available": nativeBridgeAvailable && Tun2SocksEngine.isAvailable,
            "bridge": "tp-mobile-ffi+HevSocks5Tunnel",
            "tun2socks_available": Tun2SocksEngine.isAvailable,
            "tun2socks_running": tun2Socks?.isRunning ?? false,
            "tun2socks_stats": tun2Socks?.statsPayload() ?? NSNull(),
            "packet_pump_running": packetBridge?.isPacketPumpRunning ?? false,
            "tunnel_address": tunnelAddress,
            "mtu": mtu,
            "packet_bridge": packetBridge?.statsPayload() ?? NSNull(),
            "native_status": nativeStatus,
            "last_error": lastErrorPayload,
        ]
        let normalizedPayload = MobileTrafficStatusNormalizer.normalizedProviderStatus(payload)

        return Self.jsonObject(
            normalizedPayload,
            fallback: #"{"running":false,"bridge_available":false,"bridge":"tp-mobile-ffi+HevSocks5Tunnel"}"#
        )
    }

    func logsJSON(limit: Int) -> String {
        nativeBridge.logsJSON(limit: limit)
    }

    func clearLogs() -> Int32 {
        NativeBridgeFFICommands.clearLogs()
    }

    func setLogLevel(_ level: String) -> Int32 {
        nativeBridge.setLogLevel(level)
    }

    func logConfigJSON() -> String {
        NativeBridgeFFICommands.logConfigJSON()
    }

    private static func nativeErrorMessage(statusJSON: String, fallback: String) -> String {
        guard let data = statusJSON.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            return fallback
        }

        if let lastError = object["last_error"] as? [String: Any],
           let message = lastError["error"] as? String,
           !message.isEmpty {
            return message
        }
        if let message = object["message"] as? String, !message.isEmpty {
            return message
        }
        return fallback
    }

    private static func errorPayload(_ error: NativeTunnelRuntimeError) -> [String: Any] {
        [
            "code": errorCode(error),
            "error": error.localizedDescription,
        ]
    }

    private static func errorCode(_ error: NativeTunnelRuntimeError) -> Int {
        switch error {
        case .nativeStartFailed(let code, _):
            return Int(code)
        case .runtimeConfigUnavailable:
            return Int(TunnelProxyNativeBridge.startFailed)
        case .invalidRuntimeConfig:
            return Int(TunnelProxyNativeBridge.invalidConfig)
        case .bridgeUnavailable, .packetBridgeUnavailable, .tun2SocksUnavailable,
             .tun2SocksExitedDuringStartup, .tun2SocksStartFailed:
            return Int(TunnelProxyNativeBridge.startFailed)
        }
    }

    private static func jsonValue(from rawJSON: String) -> Any {
        guard let data = rawJSON.data(using: .utf8),
              let value = try? JSONSerialization.jsonObject(with: data)
        else {
            return rawJSON
        }
        return value
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
}

private extension LocalSocks5RuntimeConfigError {
    var nativeTunnelRuntimeError: NativeTunnelRuntimeError {
        switch self {
        case .runtimeConfigUnavailable(let message):
            return .runtimeConfigUnavailable(message)
        case .invalidRuntimeConfig(let message):
            return .invalidRuntimeConfig(message)
        }
    }
}

private final class Tun2SocksEngine: @unchecked Sendable {
    static var isAvailable: Bool {
        #if canImport(HevSocks5Tunnel)
        true
        #else
        false
        #endif
    }

    private let lock = NSLock()
    private var thread: Thread?
    private var lastExitCode: Int32?

    var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return thread?.isFinished == false
    }

    func start(configYAML: String, tunnelFileDescriptor: Int32) throws {
        #if canImport(HevSocks5Tunnel)
        lock.lock()
        if thread?.isFinished == false {
            lock.unlock()
            return
        }
        lastExitCode = nil
        lock.unlock()

        let worker = Thread { [weak self] in
            defer {
                Darwin.close(tunnelFileDescriptor)
            }
            let bytes = Array(configYAML.utf8)
            let code: Int32 = bytes.withUnsafeBufferPointer { buffer in
                guard let baseAddress = buffer.baseAddress else {
                    return -1
                }
                return hev_socks5_tunnel_main_from_str(
                    baseAddress,
                    UInt32(buffer.count),
                    tunnelFileDescriptor
                )
            }

            self?.lock.lock()
            self?.lastExitCode = code
            self?.lock.unlock()
        }
        worker.name = "tp-ios-tun2socks"
        worker.start()

        lock.lock()
        thread = worker
        lock.unlock()

        Thread.sleep(forTimeInterval: 0.5)
        if worker.isFinished {
            let code = exitCode()
            if code == 0 {
                throw NativeTunnelRuntimeError.tun2SocksExitedDuringStartup
            }
            throw NativeTunnelRuntimeError.tun2SocksStartFailed(code: code ?? -1)
        }
        #else
        Darwin.close(tunnelFileDescriptor)
        throw NativeTunnelRuntimeError.tun2SocksUnavailable(Self.unavailableMessage)
        #endif
    }

    func stop() {
        lock.lock()
        let worker = thread
        lock.unlock()

        #if canImport(HevSocks5Tunnel)
        hev_socks5_tunnel_quit()
        #endif

        guard let worker else {
            return
        }

        for _ in 0..<20 where !worker.isFinished {
            Thread.sleep(forTimeInterval: 0.1)
        }

        lock.lock()
        if thread === worker {
            thread = nil
        }
        lock.unlock()
    }

    func statsPayload() -> [String: Any] {
        #if canImport(HevSocks5Tunnel)
        var txPackets = size_t(0)
        var txBytes = size_t(0)
        var rxPackets = size_t(0)
        var rxBytes = size_t(0)
        hev_socks5_tunnel_stats(&txPackets, &txBytes, &rxPackets, &rxBytes)

        let rxHeaderBytes = rxPackets * size_t(MemoryLayout<UInt32>.size)
        let rxPayloadBytes = rxBytes > rxHeaderBytes ? rxBytes - rxHeaderBytes : rxBytes
        return [
            "tx_packets": numericValue(txPackets),
            "tx_bytes": numericValue(txBytes),
            "rx_packets": numericValue(rxPackets),
            "rx_bytes": numericValue(rxBytes),
            "rx_payload_bytes": numericValue(rxPayloadBytes),
        ]
        #else
        return [
            "tx_packets": 0,
            "tx_bytes": 0,
            "rx_packets": 0,
            "rx_bytes": 0,
            "rx_payload_bytes": 0,
        ]
        #endif
    }

    private func exitCode() -> Int32? {
        lock.lock()
        defer { lock.unlock() }
        return lastExitCode
    }

    private func numericValue(_ value: size_t) -> NSNumber {
        NSNumber(value: UInt64(value))
    }

    private static let unavailableMessage = "HevSocks5Tunnel is not linked in this build"
}

private final class PacketTunnelFlowBridge: @unchecked Sendable {
    private let packetFlow: NEPacketTunnelFlow
    private let mtu: Int
    private let packetReadQueue = DispatchQueue(label: "com.buhuipao.tunnelproxy.packet-read")
    private let socketReadQueue = DispatchQueue(label: "com.buhuipao.tunnelproxy.socket-read")
    private let lock = NSLock()

    private var appFileDescriptor: Int32 = -1
    private var tunnelFileDescriptor: Int32 = -1
    private var running = false
    private var packetsToTun2Socks = 0
    private var packetsFromTun2Socks = 0
    private var bytesToTun2Socks = 0
    private var bytesFromTun2Socks = 0
    private var droppedPackets = 0

    init(packetFlow: NEPacketTunnelFlow, mtu: Int) throws {
        self.packetFlow = packetFlow
        self.mtu = mtu

        var descriptors = [Int32](repeating: -1, count: 2)
        guard Darwin.socketpair(AF_UNIX, SOCK_DGRAM, 0, &descriptors) == 0 else {
            throw NativeTunnelRuntimeError.packetBridgeUnavailable(Self.posixError("socketpair"))
        }

        do {
            try Self.setNonBlocking(descriptors[0])
            try Self.setNonBlocking(descriptors[1])
            Self.setSocketBuffer(descriptors[0], option: SO_SNDBUF, size: 4 * 1024 * 1024)
            Self.setSocketBuffer(descriptors[0], option: SO_RCVBUF, size: 4 * 1024 * 1024)
            Self.setSocketBuffer(descriptors[1], option: SO_SNDBUF, size: 4 * 1024 * 1024)
            Self.setSocketBuffer(descriptors[1], option: SO_RCVBUF, size: 4 * 1024 * 1024)
        } catch {
            Darwin.close(descriptors[0])
            Darwin.close(descriptors[1])
            throw error
        }

        appFileDescriptor = descriptors[0]
        tunnelFileDescriptor = descriptors[1]
    }

    var isPacketPumpRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return running
    }

    func duplicateTunnelFileDescriptor() throws -> Int32 {
        lock.lock()
        let fd = tunnelFileDescriptor
        lock.unlock()

        guard fd >= 0 else {
            throw NativeTunnelRuntimeError.packetBridgeUnavailable("packet bridge tunnel fd is closed")
        }
        let duplicate = Darwin.dup(fd)
        guard duplicate >= 0 else {
            throw NativeTunnelRuntimeError.packetBridgeUnavailable(Self.posixError("dup"))
        }
        return duplicate
    }

    func startPacketPump() {
        lock.lock()
        if running {
            lock.unlock()
            return
        }
        running = true
        lock.unlock()

        schedulePacketFlowRead()
        socketReadQueue.async { [weak self] in
            self?.readPacketsFromTun2Socks()
        }
    }

    func stopPacketPump() {
        lock.lock()
        running = false
        lock.unlock()
    }

    func closeFileDescriptors() {
        lock.lock()
        let appFD = appFileDescriptor
        let tunnelFD = tunnelFileDescriptor
        appFileDescriptor = -1
        tunnelFileDescriptor = -1
        lock.unlock()

        if appFD >= 0 {
            Darwin.close(appFD)
        }
        if tunnelFD >= 0 {
            Darwin.close(tunnelFD)
        }
    }

    func statsPayload() -> [String: Any] {
        lock.lock()
        defer { lock.unlock() }
        return [
            "packets_to_tun2socks": packetsToTun2Socks,
            "packets_from_tun2socks": packetsFromTun2Socks,
            "bytes_to_tun2socks": bytesToTun2Socks,
            "bytes_from_tun2socks": bytesFromTun2Socks,
            "dropped_packets": droppedPackets,
        ]
    }

    private func schedulePacketFlowRead() {
        guard isActive else {
            return
        }

        packetFlow.readPackets { [weak self] packets, protocols in
            guard let self else {
                return
            }
            packetReadQueue.async { [weak self] in
                guard let self, self.isActive else {
                    return
                }
                for (index, packet) in packets.enumerated() {
                    let family = protocols.indices.contains(index)
                        ? protocols[index].int32Value
                        : Self.inferAddressFamily(packet)
                    self.writePacketToTun2Socks(packet, addressFamily: family)
                }
                self.schedulePacketFlowRead()
            }
        }
    }

    private func writePacketToTun2Socks(_ packet: Data, addressFamily: Int32) {
        let fd = currentAppFileDescriptor()
        guard fd >= 0 else {
            incrementDroppedPackets()
            return
        }

        var frame = Data()
        frame.reserveCapacity(packet.count + 4)
        appendNetworkOrderAddressFamily(addressFamily, to: &frame)
        frame.append(packet)

        let written = frame.withUnsafeBytes { rawBuffer in
            guard let baseAddress = rawBuffer.baseAddress else {
                return -1
            }
            return Darwin.write(fd, baseAddress, rawBuffer.count)
        }

        guard written == frame.count else {
            incrementDroppedPackets()
            return
        }

        lock.lock()
        packetsToTun2Socks += 1
        bytesToTun2Socks += packet.count
        lock.unlock()
    }

    private func readPacketsFromTun2Socks() {
        var buffer = [UInt8](repeating: 0, count: max(mtu + 4, 2048))

        while isActive {
            let fd = currentAppFileDescriptor()
            if fd < 0 {
                break
            }

            let readCount = buffer.withUnsafeMutableBytes { rawBuffer in
                Darwin.read(fd, rawBuffer.baseAddress, rawBuffer.count)
            }

            if readCount > 4 {
                let family = Self.addressFamily(fromNetworkOrderHeader: buffer)
                let packet = Data(buffer[4..<readCount])
                if packetFlow.writePackets([packet], withProtocols: [NSNumber(value: family)]) {
                    lock.lock()
                    packetsFromTun2Socks += 1
                    bytesFromTun2Socks += packet.count
                    lock.unlock()
                } else {
                    incrementDroppedPackets()
                }
                continue
            }

            if readCount == 0 {
                break
            }

            if readCount > 0 {
                // 1-4 bytes: too short to contain address-family header + IP packet; skip
                incrementDroppedPackets()
                continue
            }

            // readCount < 0: check errno
            let err = errno
            if err == EAGAIN || err == EWOULDBLOCK {
                usleep(1_000)
                continue
            }
            if err == EINTR {
                continue
            }

            incrementDroppedPackets()
            break
        }
    }

    private var isActive: Bool {
        lock.lock()
        defer { lock.unlock() }
        return running
    }

    private func currentAppFileDescriptor() -> Int32 {
        lock.lock()
        defer { lock.unlock() }
        return appFileDescriptor
    }

    private func incrementDroppedPackets() {
        lock.lock()
        droppedPackets += 1
        lock.unlock()
    }

    private static func inferAddressFamily(_ packet: Data) -> Int32 {
        guard let first = packet.first else {
            return Int32(AF_INET)
        }
        return (first >> 4) == 6 ? Int32(AF_INET6) : Int32(AF_INET)
    }

    private static func addressFamily(fromNetworkOrderHeader buffer: [UInt8]) -> Int32 {
        guard buffer.count >= 4 else {
            return Int32(AF_INET)
        }
        let raw = (UInt32(buffer[0]) << 24)
            | (UInt32(buffer[1]) << 16)
            | (UInt32(buffer[2]) << 8)
            | UInt32(buffer[3])
        if raw == UInt32(AF_INET6) {
            return Int32(AF_INET6)
        }
        return Int32(AF_INET)
    }

    private func appendNetworkOrderAddressFamily(_ family: Int32, to frame: inout Data) {
        let value = UInt32(bitPattern: family)
        frame.append(UInt8((value >> 24) & 0xff))
        frame.append(UInt8((value >> 16) & 0xff))
        frame.append(UInt8((value >> 8) & 0xff))
        frame.append(UInt8(value & 0xff))
    }

    private static func setNonBlocking(_ fd: Int32) throws {
        let flags = Darwin.fcntl(fd, F_GETFL, 0)
        guard flags >= 0 else {
            throw NativeTunnelRuntimeError.packetBridgeUnavailable(posixError("fcntl(F_GETFL)"))
        }
        guard Darwin.fcntl(fd, F_SETFL, flags | O_NONBLOCK) >= 0 else {
            throw NativeTunnelRuntimeError.packetBridgeUnavailable(posixError("fcntl(F_SETFL)"))
        }
    }

    private static func setSocketBuffer(_ fd: Int32, option: Int32, size: Int32) {
        var value = size
        _ = withUnsafePointer(to: &value) { pointer in
            Darwin.setsockopt(
                fd,
                SOL_SOCKET,
                option,
                pointer,
                socklen_t(MemoryLayout<Int32>.size)
            )
        }
    }

    private static func posixError(_ operation: String) -> String {
        "\(operation) failed: \(String(cString: strerror(errno)))"
    }
}

private enum NativeBridgeFFICommands {
    static func clearLogs() -> Int32 {
        #if canImport(TpMobileFfi)
        return tp_mobile_clear_logs()
        #else
        return TunnelProxyNativeBridge.startFailed
        #endif
    }

    static func logConfigJSON() -> String {
        #if canImport(TpMobileFfi)
        guard let pointer = tp_mobile_log_config_json() else {
            return errorJSON("native log config unavailable")
        }
        defer {
            tp_mobile_free_string(pointer)
        }
        return String(cString: pointer)
        #else
        return errorJSON(TunnelProxyNativeBridge.unavailableMessage)
        #endif
    }

    private static func errorJSON(_ message: String) -> String {
        let payload: [String: Any] = [
            "ok": false,
            "code": Int(TunnelProxyNativeBridge.startFailed),
            "error": message,
        ]

        guard JSONSerialization.isValidJSONObject(payload),
              let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]),
              let json = String(data: data, encoding: .utf8)
        else {
            return #"{"ok":false,"code":-5,"error":"native log config unavailable"}"#
        }

        return json
    }
}
