import SwiftUI
import UIKit
import WebKit
import UniformTypeIdentifiers

/// The iOS Client is the shared UI in a WKWebView.
///
/// It used to be its own SwiftUI screens — a third copy of a vocabulary the
/// desktop and Android also each held, which is how a Peer's state came to be
/// worded three ways and a Settings tab grew sections nobody else had. The
/// screens come from one bundle now. What is left native is what a phone
/// genuinely owns: VPN consent, a document picker, a camera, and the runtime.
struct WebHostView: UIViewRepresentable {
    @ObservedObject var model: TunnelAppModel

    func makeCoordinator() -> WebHostCoordinator {
        WebHostCoordinator(model: model)
    }

    func makeUIView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.userContentController.add(context.coordinator, name: "lantunnel")
        // A blank screen that explains nothing is the worst failure this host
        // can have, and it is the one it had first: a script that will not load
        // leaves no trace anywhere a native log can see.
        configuration.userContentController.add(context.coordinator, name: "lantunnelDiag")
        configuration.userContentController.addUserScript(WKUserScript(
            source: Self.diagnosticsScript,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        ))
        // The bundle is served over a scheme of our own rather than read off
        // disk. A file:// page has an opaque origin, and the bundle's entry is
        // a module script, which is fetched with CORS — so from file:// both
        // the script and the stylesheet are refused and the screen stays blank
        // with nothing in any native log to say why.
        configuration.setURLSchemeHandler(BundleUISchemeHandler(), forURLScheme: Self.uiScheme)
        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.isOpaque = false
        webView.backgroundColor = UIColor(TunnelProxyTheme.background)
        webView.scrollView.backgroundColor = UIColor(TunnelProxyTheme.background)
        context.coordinator.attach(webView)

        guard Bundle.main.url(forResource: "index", withExtension: "html", subdirectory: "ui") != nil,
              let entry = URL(string: "\(Self.uiScheme)://ui/index.html")
        else {
            // A bundle that shipped without its UI cannot be recovered at
            // runtime, and a blank white screen says nothing about why.
            webView.loadHTMLString(Self.missingBundleNotice, baseURL: nil)
            return webView
        }
        webView.load(URLRequest(url: entry))
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static let uiScheme = "lantunnel-ui"

    static let diagnosticsScript = """
        window.addEventListener('error', function (event) {
          var detail = event.message
            ? event.message + ' @ ' + (event.filename || '?') + ':' + (event.lineno || 0)
            : 'failed to load ' + ((event.target && (event.target.src || event.target.href)) || '?');
          window.webkit.messageHandlers.lantunnelDiag.postMessage(detail);
        }, true);
        window.addEventListener('unhandledrejection', function (event) {
          window.webkit.messageHandlers.lantunnelDiag.postMessage('unhandled: ' + event.reason);
        });
        """

    private static let missingBundleNotice = """
        <html><body style="font:-apple-system-body;padding:24px;color:#0f172a">
        <h3>The interface did not ship with this build.</h3>
        <p>Reinstall Lantunnel.</p></body></html>
        """
}

/// The iOS end of the one bridge every Client speaks.
///
/// The UI posts `{id, command, args}` and gets an answer back by id. Nothing
/// here decides what a screen says — the wording, the ordering and the state
/// vocabulary all come from the shared bundle and from `client_ui` in Rust.
@MainActor
final class WebHostCoordinator: NSObject, WKScriptMessageHandler {
    private let model: TunnelAppModel
    private weak var webView: WKWebView?
    private var pendingPickCallID: Int?
    private var pendingScanCallID: Int?
    private var statusPump: Task<Void, Never>?

    init(model: TunnelAppModel) {
        self.model = model
        super.init()
    }

    func attach(_ webView: WKWebView) {
        self.webView = webView
        statusPump?.cancel()
        statusPump = Task { [weak self] in
            while !Task.isCancelled {
                await self?.model.refreshStatus()
                self?.emit("status", json: self?.statusJSON() ?? "{}")
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
    }

    deinit {
        statusPump?.cancel()
    }

    func userContentController(
        _ controller: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        guard let body = message.body as? String else { return }
        if message.name == "lantunnelDiag" {
            NSLog("[lantunnel-ui] %@", body)
            return
        }
        dispatch(body)
    }

    private func dispatch(_ raw: String) {
        guard let data = raw.data(using: .utf8),
              let call = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = call["id"] as? Int
        else { return }
        let command = call["command"] as? String ?? ""
        let args = call["args"] as? [String: Any] ?? [:]

        switch command {
        case "get_capabilities": replyOk(id, Self.capabilitiesJSON)
        case "get_status": replyOk(id, statusJSON())
        case "get_proxy_status": replyOk(id, proxyStatusJSON())
        case "get_settings": replyOk(id, settingsJSON())
        case "get_product_info": replyOk(id, productInfoJSON())
        case "list_peer_profiles": replyOk(id, peerProfilesJSON())
        case "get_logs": Task { await self.answerLogs(id, limit: args["limit"] as? Int ?? 500) }
        case "clear_logs": Task { await self.model.clearLogs(); self.replyOk(id, "null") }
        case "write_clipboard_text":
            UIPasteboard.general.string = args["text"] as? String ?? ""
            replyOk(id, "null")
        case "save_settings":
            saveSettings(args["settings"] as? [String: Any] ?? [:])
            replyOk(id, "null")
        case "forget_peer_profile":
            forgetProfile(tunnelID: args["tunnelId"] as? String ?? "")
            replyOk(id, peerProfilesJSON())
        case "connect_peer_profile":
            Task {
                await self.model.connect()
                if let message = self.model.bannerMessage, self.model.statusPresentation.phase == .failed {
                    self.replyErr(id, message)
                } else {
                    self.replyOk(id, "null")
                }
            }
        case "disconnect":
            Task { await self.model.disconnect(); self.replyOk(id, "null") }
        case "pick_peer_profile": presentDocumentPicker(id)
        case "scan_peer_profile": presentScanner(id)
        // A phone has no loopback proxy to configure and no privileged helper
        // to install, so the UI never asks. Answering plainly beats a silent
        // hang if a future bundle does.
        case "get_clash_config", "install_tun_helper":
            replyErr(id, "\(command) is not available on this device")
        default: replyErr(id, "unknown command: \(command)")
        }
    }

    // MARK: - answering

    private func replyOk(_ id: Int, _ json: String) { resolve(id, ok: true, payload: json) }

    private func replyErr(_ id: Int, _ message: String) {
        resolve(id, ok: false, payload: Self.jsonString(message))
    }

    private func resolve(_ id: Int, ok: Bool, payload: String) {
        // The payload crosses as a JSON string literal, so a quote or a newline
        // inside a message cannot end the script early.
        let script = "window.__lantunnelResolve && window.__lantunnelResolve("
            + "\(id), \(ok), \(Self.jsonString(payload)))"
        webView?.evaluateJavaScript(script)
    }

    private func emit(_ event: String, json: String) {
        let script = "window.__lantunnelEmit && window.__lantunnelEmit("
            + "\(Self.jsonString(event)), \(Self.jsonString(json)))"
        webView?.evaluateJavaScript(script)
    }

    private func answerLogs(_ id: Int, limit: Int) async {
        await model.refreshLogs(limit: limit)
        let lines = model.logText
            .split(separator: "\n", omittingEmptySubsequences: true)
            .suffix(limit)
            .map(String.init)
        replyOk(id, Self.encode(lines))
    }

    // MARK: - the shapes the shared UI reads

    /// `client_ui` is computed in Rust and passed through untouched — that is
    /// the whole point of the exercise. Only the flattening happens here.
    private func statusJSON() -> String {
        let raw = model.latestProviderStatusJSON ?? "{}"
        let root = (try? JSONSerialization.jsonObject(with: Data(raw.utf8))) as? [String: Any] ?? [:]
        var out = root["connection"] as? [String: Any] ?? [:]
        let phase = model.statusPresentation.phase
        out["connected"] = phase == .connected
        out["connecting"] = phase == .connecting
        if out["uptime_secs"] == nil { out["uptime_secs"] = 0 }
        out["message"] = model.statusPresentation.message ?? model.statusPresentation.subtitle
        if let clientUI = root["client_ui"] { out["client_ui"] = clientUI }
        return Self.encode(out)
    }

    private func proxyStatusJSON() -> String {
        let running = model.statusPresentation.phase == .connected
        return Self.encode([
            "running": running,
            "listen_addr": model.config.localSocks5Listen,
            "tun_running": running,
            "tun_routes": Array(model.routedAtConnect),
        ])
    }

    private func settingsJSON() -> String {
        Self.encode([
            "auto_start": false,
            "auto_connect": model.config.autoConnect,
            "local_socks5_listen": model.config.localSocks5Listen,
            "local_proxy_enabled": false,
            "p2p_allow_lan_candidates": model.config.lanP2pEnabled,
            "log_level": model.logLevel,
            "client_access": storedClientAccess(),
            "exported_lans": model.config.exportedLans,
            "tunnel_first": model.config.tunnelFirst,
            "exported_lan_statuses": [],
        ])
    }

    /// An install from before the shared UI holds its rules as text lines and a
    /// Block-all flag; both are converted on the way out, so nobody opens the
    /// Access tab after upgrading and finds it empty.
    private func storedClientAccess() -> [String: Any] {
        let trimmed = model.config.clientAccessJSON.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty,
           let policy = (try? JSONSerialization.jsonObject(with: Data(trimmed.utf8))) as? [String: Any] {
            return policy
        }
        return model.config.legacyClientAccessPolicy()
    }

    private func saveSettings(_ settings: [String: Any]) {
        var next = model.config
        next.autoConnect = settings["auto_connect"] as? Bool ?? next.autoConnect
        next.lanP2pEnabled = settings["p2p_allow_lan_candidates"] as? Bool ?? next.lanP2pEnabled
        next.tunnelFirst = settings["tunnel_first"] as? Bool ?? next.tunnelFirst
        next.exportedLans = settings["exported_lans"] as? [String] ?? next.exportedLans
        if let policy = settings["client_access"] {
            next.clientAccessJSON = Self.encode(policy)
        }
        // The line list and the Block-all flag are never written again; clearing
        // them keeps a stale copy from outliving the rules the owner can see.
        next.accessRules = []
        next.blockAllIncoming = false
        model.config = next
        model.applyConfig()
        if let level = settings["log_level"] as? String, level != model.logLevel {
            Task { await self.model.setLogLevel(level) }
        }
    }

    private func productInfoJSON() -> String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return Self.encode([
            "binary_name": "lantunnel-client",
            "display_name": "Lantunnel",
            "role": "peer",
            "version": version ?? "dev",
        ])
    }

    private func peerProfilesJSON() -> String {
        guard let identity = model.peerIdentity else { return "[]" }
        return Self.encode([peerSummary(identity)])
    }

    private func peerSummary(_ identity: MobileConfig.PeerIdentity) -> [String: Any] {
        [
            "tunnel_id": identity.tunnelId,
            "peer_id": identity.peerId,
            "overlay_ip": identity.overlayIP,
            "bootstrap_kind": "static_gateway",
        ]
    }

    private func forgetProfile(tunnelID: String) {
        guard let identity = model.peerIdentity, identity.tunnelId == tunnelID else { return }
        var next = model.config
        next.peerProfileJSON = ""
        model.config = next
        model.applyConfig()
    }

    // MARK: - what a phone owns

    private func topViewController() -> UIViewController? {
        let scene = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .first { $0.activationState == .foregroundActive }
        var top = scene?.keyWindow?.rootViewController
        while let presented = top?.presentedViewController { top = presented }
        return top
    }

    private func presentDocumentPicker(_ id: Int) {
        guard let host = topViewController() else {
            replyErr(id, "No window is available to present the picker")
            return
        }
        pendingPickCallID = id
        // A `.peer` file has no registered type, so the picker stays open.
        let picker = UIDocumentPickerViewController(
            forOpeningContentTypes: [.data, .json, .plainText]
        )
        picker.allowsMultipleSelection = false
        picker.delegate = self
        host.present(picker, animated: true)
    }

    private func presentScanner(_ id: Int) {
        guard let host = topViewController() else {
            replyErr(id, "No window is available to present the camera")
            return
        }
        pendingScanCallID = id
        var hosting: UIHostingController<QRCodeScannerView>?
        let scanner = QRCodeScannerView(
            onCode: { [weak self] code in
                hosting?.dismiss(animated: true)
                self?.finishScan(with: code)
            },
            onCancel: { [weak self] in
                hosting?.dismiss(animated: true)
                self?.finishScan(with: nil)
            },
            onManualImport: { [weak self] in
                hosting?.dismiss(animated: true)
                // Pasting stays for a profile that arrived as text.
                self?.finishScan(with: UIPasteboard.general.string)
            }
        )
        let controller = UIHostingController(rootView: scanner)
        hosting = controller
        host.present(controller, animated: true)
    }

    private func finishScan(with code: String?) {
        guard let id = pendingScanCallID else { return }
        pendingScanCallID = nil
        guard let code, !code.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            // A cancelled scan is not a failure; the UI keeps what it had.
            replyOk(id, "null")
            return
        }
        adopt(code, id: id)
    }

    private func adopt(_ raw: String, id: Int) {
        guard model.importPeerProfile(raw), let identity = model.peerIdentity else {
            replyErr(id, model.bannerMessage ?? "That is not a Peer profile")
            return
        }
        replyOk(id, Self.encode(peerSummary(identity)))
    }

    // MARK: - encoding

    private static let capabilitiesJSON = encode([
        "qrScanner": true,
        "startAtLogin": false,
        "localProxy": false,
        "exportReadiness": false,
    ])

    static func encode(_ value: Any) -> String {
        guard JSONSerialization.isValidJSONObject(value),
              let data = try? JSONSerialization.data(withJSONObject: value),
              let text = String(data: data, encoding: .utf8)
        else { return "null" }
        return text
    }

    /// A JSON string literal, so quotes and newlines cannot end a script early.
    static func jsonString(_ value: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: [value]),
              let wrapped = String(data: data, encoding: .utf8)
        else { return "\"\"" }
        return String(wrapped.dropFirst().dropLast())
    }
}

extension WebHostCoordinator: UIDocumentPickerDelegate {
    nonisolated func documentPicker(
        _ controller: UIDocumentPickerViewController,
        didPickDocumentsAt urls: [URL]
    ) {
        Task { @MainActor [weak self] in
            guard let self, let id = self.pendingPickCallID else { return }
            self.pendingPickCallID = nil
            guard let url = urls.first else {
                self.replyOk(id, "null")
                return
            }
            guard let contents = Self.readProfile(at: url) else {
                self.replyErr(id, "That file could not be read as a Peer profile")
                return
            }
            self.adopt(contents, id: id)
        }
    }

    nonisolated func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        Task { @MainActor [weak self] in
            guard let self, let id = self.pendingPickCallID else { return }
            self.pendingPickCallID = nil
            self.replyOk(id, "null")
        }
    }

    /// A picked file lives outside the app's sandbox until it is opened.
    private static func readProfile(at url: URL) -> String? {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        guard let data = try? Data(contentsOf: url), data.count <= 64 * 1024 else { return nil }
        // A profile exported on Windows carries a byte order mark and CRLF;
        // Android strips both, and a file that imports there must import here.
        let bom = Data([0xEF, 0xBB, 0xBF])
        let body = data.starts(with: bom) ? data.dropFirst(bom.count) : data
        return String(decoding: body, as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

/// Serves the packaged UI over a scheme of our own.
///
/// The alternative was `file://`, whose origin is opaque: the bundle's entry is
/// a module script and modules are fetched with CORS, so the script and the
/// stylesheet were both refused and the app opened on a blank canvas. A scheme
/// with a host gives the page a real, same-origin home.
final class BundleUISchemeHandler: NSObject, WKURLSchemeHandler {
    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        guard let url = task.request.url,
              let root = Bundle.main.url(forResource: "ui", withExtension: nil)?.standardizedFileURL
        else {
            task.didFailWithError(URLError(.badURL))
            return
        }
        let relative = url.path.isEmpty || url.path == "/" ? "index.html" : String(url.path.dropFirst())
        let file = root.appendingPathComponent(relative).standardizedFileURL
        // A request is only ever served from inside the packaged directory;
        // "../" in a path must not reach the rest of the bundle.
        guard file.path.hasPrefix(root.path), let data = try? Data(contentsOf: file) else {
            task.didFailWithError(URLError(.fileDoesNotExist))
            return
        }
        let response = HTTPURLResponse(
            url: url,
            statusCode: 200,
            httpVersion: "HTTP/1.1",
            headerFields: [
                "Content-Type": Self.contentType(for: file.pathExtension),
                "Content-Length": String(data.count),
                // The bundle ships with the app, so a cache would only ever
                // serve the previous release's screens after an update.
                "Cache-Control": "no-store",
            ]
        )
        guard let response else {
            task.didFailWithError(URLError(.badServerResponse))
            return
        }
        task.didReceive(response)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {}

    /// A module script served as anything but JavaScript is refused outright,
    /// so this is not cosmetic.
    static func contentType(for pathExtension: String) -> String {
        switch pathExtension.lowercased() {
        case "html": return "text/html; charset=utf-8"
        case "js", "mjs": return "text/javascript; charset=utf-8"
        case "css": return "text/css; charset=utf-8"
        case "json": return "application/json; charset=utf-8"
        case "svg": return "image/svg+xml"
        case "png": return "image/png"
        case "jpg", "jpeg": return "image/jpeg"
        case "woff2": return "font/woff2"
        default: return "application/octet-stream"
        }
    }
}
