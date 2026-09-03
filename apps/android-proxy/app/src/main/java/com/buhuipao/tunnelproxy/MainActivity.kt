package com.buhuipao.tunnelproxy

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.WindowInsets
import android.widget.FrameLayout
import android.webkit.ConsoleMessage
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import com.google.zxing.integration.android.IntentIntegrator
import org.json.JSONArray
import org.json.JSONObject
import java.io.InputStream

/**
 * The Android Client is the shared UI in a WebView.
 *
 * It used to hand-build every screen in Kotlin — two thousand lines that had
 * to agree with a React file and a SwiftUI file by nothing stronger than
 * discipline, and did not. The screens come from one bundle now; what is left
 * here is the part a phone genuinely owns: VPN consent, a document picker, a
 * camera, and the runtime.
 */
class MainActivity : Activity(), WebBridge.Host {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var webView: WebView

    /**
     * The imported profile, held in a field and never rendered. It carries
     * `peer_private_key`, so replacing it means importing another file.
     */
    private var activePeerProfileJson: String = ""

    /** Calls that can only be answered once an Activity result comes back. */
    private var pendingPickCallId: Int? = null
    private var pendingScanCallId: Int? = null
    private var pendingConnectCallId: Int? = null
    private var pendingVpnConfig: MobileConfig? = null

    private var routedAtConnect: Set<String> = emptySet()
    private var rememberedMeshRoutes: Set<String> = emptySet()
    private var uiReady = false

    private val pollRunnable = object : Runnable {
        override fun run() {
            refreshDerivedRoutes()
            emit("status", statusJson())
            handler.postDelayed(this, STATUS_POLL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        routedAtConnect = MobileConfig.readRoutedAtConnect(MobileConfig.preferences(this))
        rememberedMeshRoutes = MobileConfig.readRememberedMeshRoutes(MobileConfig.preferences(this))
        WebBridge.HOST_LOG_CONTEXT = applicationContext
        window.statusBarColor = COLOR_CANVAS
        window.navigationBarColor = COLOR_CANVAS
        activePeerProfileJson = MobileConfig.fromPreferences(this).peerProfileJson
        buildWebView()
        handleSetupIntent(intent)
        maybeAutoConnect()
    }

    override fun onResume() {
        super.onResume()
        handler.removeCallbacks(pollRunnable)
        handler.post(pollRunnable)
    }

    override fun onPause() {
        handler.removeCallbacks(pollRunnable)
        super.onPause()
    }

    override fun onDestroy() {
        handler.removeCallbacks(pollRunnable)
        WebBridge.HOST_LOG_CONTEXT = null
        super.onDestroy()
    }

    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleSetupIntent(intent)
    }

    private fun buildWebView() {
        webView = WebView(this).apply {
            setBackgroundColor(COLOR_CANVAS)
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            // The bundle is packaged in the APK. Nothing is fetched, so no
            // remote origin is ever handed the bridge.
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            addJavascriptInterface(WebBridge(this@MainActivity), "__lantunnelAndroid")
            webViewClient = object : WebViewClient() {
                override fun onPageFinished(view: WebView?, url: String?) {
                    uiReady = true
                }

                override fun shouldInterceptRequest(
                    view: WebView,
                    request: WebResourceRequest,
                ): WebResourceResponse? = serveBundle(request.url)
            }
            // A blank screen that explains nothing is the worst failure this
            // host can have, and it is the one it had first.
            webChromeClient = object : WebChromeClient() {
                override fun onConsoleMessage(message: ConsoleMessage): Boolean {
                    if (message.messageLevel() == ConsoleMessage.MessageLevel.ERROR) {
                        ProxyServiceState.appendLog(
                            this@MainActivity,
                            "ui: " + message.message() + " @" + message.lineNumber(),
                        )
                    }
                    return true
                }
            }
            loadUrl(UI_URL)
        }
        // The page is laid out to the edges, so without an inset the header
        // sits under the clock and the primary action sits under the gesture
        // bar. The padding goes on a container rather than on the WebView:
        // a WebView does not lay its page out inside its own padding, so
        // setting it there changed nothing on screen.
        val frame = FrameLayout(this).apply {
            setBackgroundColor(COLOR_CANVAS)
            setPadding(0, systemBarHeight(STATUS_BAR_HEIGHT), 0, systemBarHeight(NAV_BAR_HEIGHT))
            addView(
                webView,
                FrameLayout.LayoutParams(
                    FrameLayout.LayoutParams.MATCH_PARENT,
                    FrameLayout.LayoutParams.MATCH_PARENT,
                ),
            )
            setOnApplyWindowInsetsListener { view, insets ->
                // The dispatched insets are the exact answer where they arrive;
                // the measured heights above are what keeps the first frame
                // right when they do not.
                val top: Int
                val bottom: Int
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    val bars = insets.getInsets(WindowInsets.Type.systemBars())
                    top = bars.top
                    bottom = bars.bottom
                } else {
                    @Suppress("DEPRECATION")
                    top = insets.systemWindowInsetTop
                    @Suppress("DEPRECATION")
                    bottom = insets.systemWindowInsetBottom
                }
                if (top > 0 || bottom > 0) view.setPadding(0, top, 0, bottom)
                insets
            }
        }
        setContentView(frame)
    }

    /**
     * Serves the packaged bundle from a host that only ever exists in process.
     *
     * The alternative was `file://`, whose origin is opaque: the bundle's entry
     * is a module script and modules are fetched with CORS, so the script and
     * the stylesheet were both refused and the app opened on a blank canvas.
     * `appassets.androidplatform.net` is the reserved name for exactly this and
     * resolves nowhere, so a missed interception fails rather than reaching the
     * network.
     */
    private fun serveBundle(url: Uri): WebResourceResponse? {
        if (!url.scheme.equals("https", ignoreCase = true) || url.host != UI_HOST) return null
        val path = url.path.orEmpty().trimStart('/').ifEmpty { "index.html" }
        // A request is only ever served from inside the packaged directory;
        // "../" in a path must not reach the rest of the assets.
        if (path.contains("..")) return null
        val asset = "ui/" + path
        return runCatching {
            WebResourceResponse(
                contentTypeFor(asset),
                "utf-8",
                assets.open(asset),
            )
        }.getOrNull()
    }

    /**
     * A module script served as anything but JavaScript is refused outright,
     * so this is not cosmetic.
     */
    private fun contentTypeFor(asset: String): String = when (asset.substringAfterLast('.', "")) {
        "html" -> "text/html"
        "js", "mjs" -> "text/javascript"
        "css" -> "text/css"
        "json" -> "application/json"
        "svg" -> "image/svg+xml"
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "woff2" -> "font/woff2"
        else -> "application/octet-stream"
    }

    /** The height the framework reports for a system bar, or a sane default. */
    private fun systemBarHeight(name: String): Int {
        val id = resources.getIdentifier(name, "dimen", "android")
        if (id > 0) return resources.getDimensionPixelSize(id)
        return if (name == STATUS_BAR_HEIGHT) (24 * resources.displayMetrics.density).toInt() else 0
    }

    // --- WebBridge.Host ------------------------------------------------------

    override fun runOnUi(action: () -> Unit) {
        handler.post { action() }
    }

    override fun replyOk(id: Int, json: String) = resolve(id, true, json)

    override fun replyErr(id: Int, message: String) =
        resolve(id, false, JSONObject.quote(message))

    private fun resolve(id: Int, ok: Boolean, payloadJson: String) {
        if (id < 0) return
        // The payload crosses as a JSON string literal, so a quote or a newline
        // inside a message cannot end the script early.
        val script = "window.__lantunnelResolve && window.__lantunnelResolve(" +
            id + ", " + ok + ", " + JSONObject.quote(payloadJson) + ")"
        handler.post { webView.evaluateJavascript(script, null) }
    }

    private fun emit(event: String, payloadJson: String) {
        if (!uiReady) return
        val script = "window.__lantunnelEmit && window.__lantunnelEmit(" +
            JSONObject.quote(event) + ", " + JSONObject.quote(payloadJson) + ")"
        webView.evaluateJavascript(script, null)
    }

    /**
     * The connection status, in the shape every Client renders.
     *
     * `client_ui` is computed in Rust and passed through untouched — that is
     * the whole point of the exercise. Only the flattening happens here,
     * because the native blob nests the connection one level deeper than the
     * shared UI reads it.
     */
    override fun statusJson(): String {
        val raw = runCatching { TunnelProxyNative.statusJson() }.getOrDefault("{}")
        val root = runCatching { JSONObject(raw) }.getOrDefault(JSONObject())
        val connection = root.optJSONObject("connection") ?: JSONObject()
        val running = root.optBoolean("running", false)
        val nativeStarting = root.optJSONObject("startup")?.optBoolean("active", false) == true
        val serviceState = recoverStaleServiceState(
            ProxyServiceState.fromPreferences(this),
            nativeActive = running || nativeStarting,
        )

        val out = JSONObject()
        for (key in connection.keys()) {
            out.put(key, connection.get(key))
        }
        val connected = running && connection.optBoolean("connected", false)
        out.put("connected", connected)
        // Before the runtime reports anything, the service state is the only
        // thing that knows a start was asked for. Without it the button falls
        // back to Connect the moment it is pressed.
        out.put(
            "connecting",
            !connected &&
                (running || nativeStarting || serviceState.isConnecting || serviceState.isStopping),
        )
        if (!out.has("uptime_secs")) out.put("uptime_secs", 0)
        if (!out.has("message")) out.put("message", serviceState.message)
        root.optJSONObject("client_ui")?.let { out.put("client_ui", it) }
        root.optJSONObject("last_error")?.optString("error")?.takeIf { it.isNotBlank() }
            ?.let { out.put("error", it) }
        return out.toString()
    }

    /**
     * A transient service state the runtime has stopped backing.
     *
     * The service writes Connecting before the engine starts and Stopping
     * before it stops. If the engine then fails and goes away, nothing rewrites
     * that record — and every control the UI disables while connecting stays
     * disabled, under a headline that reads Disconnected. The runtime is the
     * authority; when it says nothing is running, a transient record that has
     * stopped being refreshed is spent.
     */
    private fun recoverStaleServiceState(
        serviceState: ProxyServiceState,
        nativeActive: Boolean,
    ): ProxyServiceState {
        val nativeInactiveStopping = serviceState.isStopping && !nativeActive
        if (!nativeInactiveStopping && (nativeActive || !serviceState.isStaleTransient())) {
            return serviceState
        }
        val message = if (nativeInactiveStopping) {
            "Recovered inactive native " + serviceState.state + " state"
        } else {
            "Recovered stale " + serviceState.state + " state"
        }
        ProxyServiceState.save(this, ProxyServiceState.STATE_STOPPED, message)
        return serviceState.copy(
            state = ProxyServiceState.STATE_STOPPED,
            message = message,
            statusJson = "",
            updatedAtMs = System.currentTimeMillis(),
        )
    }

    override fun proxyStatusJson(): String {
        val running = runCatching {
            JSONObject(TunnelProxyNative.statusJson()).optBoolean("running", false)
        }.getOrDefault(false)
        return JSONObject()
            .put("running", running)
            .put("listen_addr", MobileConfig.DEFAULT_LOCAL_SOCKS5_LISTEN)
            .put("tun_running", running)
            .put("tun_routes", JSONArray(routedAtConnect.toList()))
            .toString()
    }

    override fun settingsJson(): String {
        val cfg = MobileConfig.fromPreferences(this)
        return JSONObject()
            .put("auto_start", false)
            .put("auto_connect", cfg.autoConnect)
            .put("local_socks5_listen", MobileConfig.DEFAULT_LOCAL_SOCKS5_LISTEN)
            .put("local_proxy_enabled", false)
            .put("p2p_allow_lan_candidates", cfg.lanP2pEnabled)
            .put("log_level", currentLogLevel())
            .put("client_access", storedClientAccess(cfg))
            .put("exported_lans", JSONArray(cfg.exportedLans))
            .put("tunnel_first", cfg.tunnelFirst)
            .put("exported_lan_statuses", JSONArray())
            .toString()
    }

    /**
     * The saved policy, in the shape the shared editor reads.
     *
     * An install from before the shared UI holds its rules as text lines and
     * a Block-all switch; both are converted on the way out, so nobody opens
     * the Access tab after upgrading and finds it empty.
     */
    private fun storedClientAccess(cfg: MobileConfig): JSONObject {
        cfg.clientAccessJson.trim().takeIf { it.isNotEmpty() }
            ?.let { raw -> runCatching { JSONObject(raw) }.getOrNull() }
            ?.let { return it }
        return runCatching { cfg.legacyClientAccessPolicy() }
            .getOrDefault(JSONObject().put("allow", JSONArray()).put("deny", JSONArray()))
    }

    override fun saveSettings(settings: JSONObject) {
        val current = MobileConfig.fromPreferences(this)
        val next = current.copy(
            peerProfileJson = activePeerProfileJson,
            autoConnect = settings.optBoolean("auto_connect", current.autoConnect),
            lanP2pEnabled = settings.optBoolean("p2p_allow_lan_candidates", current.lanP2pEnabled),
            tunnelFirst = settings.optBoolean("tunnel_first", current.tunnelFirst),
            exportedLans = stringList(settings.optJSONArray("exported_lans")),
            clientAccessJson = settings.optJSONObject("client_access")?.toString().orEmpty(),
            // The line list and the Block-all flag are never written again;
            // clearing them keeps a stale copy from outliving the rules the
            // owner can actually see and edit.
            accessRules = emptyList(),
            blockAllIncoming = false,
            lanRoutes = effectiveLanRoutes(),
        )
        next.saveConfig(MobileConfig.preferences(this))
        settings.optString("log_level").takeIf { it.isNotBlank() }?.let { level ->
            if (level != currentLogLevel()) setLogLevel(level)
        }
    }

    private fun stringList(array: JSONArray?): List<String> {
        if (array == null) return emptyList()
        return (0 until array.length())
            .map { array.optString(it).trim() }
            .filter { it.isNotBlank() }
    }

    override fun productInfoJson(): String = JSONObject()
        .put("binary_name", "lantunnel-client")
        .put("display_name", "Lantunnel")
        .put("role", "peer")
        .put("version", BuildConfig.VERSION_NAME)
        .toString()

    override fun peerProfilesJson(): String {
        val identity = MobileConfig.peerIdentity(activePeerProfileJson)
            ?: return JSONArray().toString()
        return JSONArray().put(peerSummary(identity)).toString()
    }

    private fun peerSummary(identity: MobileConfig.Companion.PeerIdentity): JSONObject =
        JSONObject()
            .put("tunnel_id", identity.tunnelId)
            .put("peer_id", identity.peerId)
            .put("overlay_ip", identity.overlayIp)
            .put("bootstrap_kind", bootstrapKind())

    private fun bootstrapKind(): String =
        runCatching {
            val profile = JSONObject(activePeerProfileJson)
            if (profile.optJSONObject("gateway") != null) "static_gateway" else "managed_platform"
        }.getOrDefault("static_gateway")

    override fun forgetPeerProfile(tunnelId: String): String {
        val identity = MobileConfig.peerIdentity(activePeerProfileJson)
        if (identity != null && identity.tunnelId == tunnelId) {
            activePeerProfileJson = ""
            forgetTheMesh()
            MobileConfig.fromPreferences(this)
                .copy(peerProfileJson = "")
                .saveConfig(MobileConfig.preferences(this))
            ProxyServiceState.appendLog(this, "Removed Peer profile")
        }
        return peerProfilesJson()
    }

    override fun pickPeerProfile(id: Int) {
        pendingPickCallId = id
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            // A `.peer` file has no registered type; providers report it as
            // octet-stream, text/plain, or nothing at all.
            type = "*/*"
        }
        runCatching { startActivityForResult(intent, REQUEST_IMPORT_PROFILE) }
            .onFailure {
                pendingPickCallId = null
                replyErr(id, "No file picker available on this device")
            }
    }

    override fun scanPeerProfile(id: Int) {
        pendingScanCallId = id
        if (!hasCameraPermission()) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                requestPermissions(arrayOf(Manifest.permission.CAMERA), REQUEST_CAMERA_PERMISSION)
            } else {
                pendingScanCallId = null
                replyErr(id, "Camera permission is required for QR scan")
            }
            return
        }
        startQrScanner()
    }

    override fun connect(tunnelId: String, id: Int) {
        val config = currentConfig()
        if (runCatching { MobileConfig.normalizePeerProfileJson(config.peerProfileJson) }.isFailure) {
            replyErr(id, "Import a Peer profile first")
            return
        }
        val routeValidation = MobileConfig.validateLanRoutes(config.lanRoutes)
        if (!routeValidation.first) {
            replyErr(id, routeValidation.second)
            return
        }
        config.saveConfig(MobileConfig.preferences(this))
        pendingConnectCallId = id
        val permissionIntent = android.net.VpnService.prepare(this)
        if (permissionIntent != null) {
            pendingVpnConfig = config
            @Suppress("DEPRECATION")
            startActivityForResult(permissionIntent, REQUEST_VPN_PERMISSION)
            return
        }
        startVpn(config)
    }

    override fun disconnect(id: Int) {
        TunnelVpnService.stop(this)
        ProxyServiceState.appendLog(this, "Disconnect requested")
        replyOk(id, "null")
        emit("status", statusJson())
    }

    override fun logsJson(limit: Int): String {
        val appLogs = ProxyServiceState.logs(applicationContext).split('\n')
        val nativeLogs = runCatching {
            val array = JSONArray(TunnelProxyNative.logsJson(limit))
            (0 until array.length()).map { array.optString(it) }
        }.getOrDefault(emptyList())
        val lines = (appLogs + nativeLogs).filter { it.isNotBlank() }
        return JSONArray(lines.takeLast(limit)).toString()
    }

    override fun clearLogs() {
        ProxyServiceState.clearLogs(this)
        runCatching { TunnelProxyNative.clearNativeLogs() }
        ProxyServiceState.appendLog(this, "Logs cleared")
    }

    override fun setLogLevel(level: String) {
        val code = runCatching { TunnelProxyNative.setLogLevel(level) }.getOrElse { -1 }
        ProxyServiceState.appendLog(this, "Native log level set to " + level + " code=" + code)
    }

    override fun copyToClipboard(text: String) {
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("Lantunnel", text))
    }

    private fun currentLogLevel(): String =
        runCatching { JSONObject(TunnelProxyNative.logConfigJson()).optString("level", "info") }
            .getOrDefault("info")
            .ifBlank { "info" }

    // --- runtime -------------------------------------------------------------

    private fun currentConfig(): MobileConfig = MobileConfig.fromPreferences(this).copy(
        peerProfileJson = activePeerProfileJson,
        deviceId = MobileConfig.installDeviceId(this),
        localSocks5Listen = MobileConfig.DEFAULT_LOCAL_SOCKS5_LISTEN,
        lanRoutes = effectiveLanRoutes(),
    )

    /**
     * The routes this device carries: whatever the mesh publishes.
     *
     * Connect happens while the runtime is stopped, so the live derivation is
     * only the overlay. What was last seen is what makes following the mesh
     * route anything at all.
     */
    private fun effectiveLanRoutes(): List<String> = MobileConfig.mergedMeshRoutes(
        remembered = rememberedMeshRoutes,
        derived = MobileConfig.routesFromExports(
            runCatching { TunnelProxyNative.statusJson() }.getOrDefault("{}"),
        ),
    )

    private fun refreshDerivedRoutes() {
        val raw = runCatching { TunnelProxyNative.statusJson() }.getOrDefault("{}")
        val running = runCatching { JSONObject(raw).optBoolean("running", false) }
            .getOrDefault(false)
        if (!running) return
        // Only when it changed: this runs on the status poll, so writing
        // unconditionally would commit to SharedPreferences every tick.
        val seen = MobileConfig.routesFromExports(raw).toSet()
        if (seen == rememberedMeshRoutes) return
        rememberedMeshRoutes = seen
        MobileConfig.writeRememberedMeshRoutes(MobileConfig.preferences(this), seen)
    }

    private fun startVpn(config: MobileConfig) {
        val id = pendingConnectCallId
        pendingConnectCallId = null
        ProxyServiceState.appendLog(this, "VPN connect requested")
        val startJson = runCatching {
            JSONObject(config.buildStartJson()).put("log_level", currentLogLevel()).toString()
        }.getOrElse { failure ->
            ProxyServiceState.appendLog(this, "VPN connect refused: " + failure.message)
            id?.let { replyErr(it, failure.message ?: "These settings cannot start a tunnel") }
            return
        }
        routedAtConnect = config.lanRoutes.toSet()
        MobileConfig.writeRoutedAtConnect(MobileConfig.preferences(this), routedAtConnect)
        TunnelVpnService.start(this, startJson, config.lanRoutes)
        id?.let { replyOk(it, "null") }
        emit("status", statusJson())
    }

    private fun maybeAutoConnect() {
        if (!MobileConfig.readAutoConnect(MobileConfig.preferences(this))) return
        val identity = MobileConfig.peerIdentity(activePeerProfileJson) ?: return
        val running = runCatching {
            JSONObject(TunnelProxyNative.statusJson()).optBoolean("running", false)
        }.getOrDefault(false)
        if (running) return
        connect(identity.tunnelId, -1)
    }

    // --- importing -----------------------------------------------------------

    private fun handleSetupIntent(intent: Intent?) {
        // A start payload is deliberately not accepted from an Intent. This
        // Activity is exported, so any installed app can send it one, and
        // VpnService consent is granted once and remembered — acting on a Peer
        // profile carried in an Intent would let another app raise the tunnel
        // against a Tunnel of its choosing.
        val shared = when (intent?.action) {
            Intent.ACTION_VIEW -> intent.data
            Intent.ACTION_SEND -> intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM)
            else -> null
        } ?: return
        if (importFromUri(shared) != null) emit("status", statusJson())
    }

    private fun importFromUri(uri: Uri): String? {
        val bytes = runCatching {
            contentResolver.openInputStream(uri).use { stream ->
                requireNotNull(stream) { "The selected file could not be opened" }
                stream.readAtMostBytes(MobileConfig.MAX_PEER_PROFILE_BYTES + 1)
            }
        }.getOrElse { return null }
        val profile = runCatching { MobileConfig.decodePeerProfileFile(bytes) }
            .getOrElse { return null }
        return adoptProfile(profile, "file")
    }

    /** The one place an imported profile becomes the active one. */
    private fun adoptProfile(profileJson: String, source: String): String? {
        val normalized = runCatching { MobileConfig.normalizePeerProfileJson(profileJson) }
            .getOrElse { return null }
        // A different Tunnel publishes different networks. Carrying the old
        // one's prefixes over installs VPN routes nothing in the new Tunnel
        // exports — and since home LANs are nearly always 192.168.x.0/24, the
        // usual casualty is the phone's own Wi-Fi.
        val previousTunnel = MobileConfig.peerIdentity(activePeerProfileJson)?.tunnelId
        val nextTunnel = MobileConfig.peerIdentity(normalized)?.tunnelId
        if (previousTunnel != nextTunnel) forgetTheMesh()
        activePeerProfileJson = normalized
        MobileConfig.fromPreferences(this)
            .copy(peerProfileJson = normalized)
            .saveConfig(MobileConfig.preferences(this))
        ProxyServiceState.appendLog(this, "Imported Peer profile from " + source)
        val identity = MobileConfig.peerIdentity(normalized) ?: return null
        return peerSummary(identity).toString()
    }

    private fun forgetTheMesh() {
        rememberedMeshRoutes = emptySet()
        routedAtConnect = emptySet()
        MobileConfig.writeRememberedMeshRoutes(MobileConfig.preferences(this), emptySet())
        MobileConfig.writeRoutedAtConnect(MobileConfig.preferences(this), emptySet())
    }

    private fun hasCameraPermission(): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.M ||
            checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED

    private fun startQrScanner() {
        IntentIntegrator(this).apply {
            setCaptureActivity(QrCaptureActivity::class.java)
            setDesiredBarcodeFormats(listOf(IntentIntegrator.QR_CODE))
            setPrompt("Scan Lantunnel Peer profile QR")
            setBeepEnabled(false)
            setOrientationLocked(false)
            initiateScan()
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQUEST_CAMERA_PERMISSION) return
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
            startQrScanner()
        } else {
            pendingScanCallId?.let { replyErr(it, "Camera permission is required for QR scan") }
            pendingScanCallId = null
        }
    }

    @Deprecated("Activity result API is unavailable without AndroidX in this minimal app")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val scan = IntentIntegrator.parseActivityResult(requestCode, resultCode, data)
        if (scan != null) {
            val id = pendingScanCallId
            pendingScanCallId = null
            if (id == null) return
            val contents = scan.contents
            if (contents.isNullOrBlank()) {
                // A cancelled scan is not a failure; the UI keeps what it had.
                replyOk(id, "null")
                return
            }
            val summary = adoptProfile(contents, "QR")
            if (summary == null) replyErr(id, "That is not a Peer profile") else replyOk(id, summary)
            return
        }
        when (requestCode) {
            REQUEST_IMPORT_PROFILE -> {
                val id = pendingPickCallId
                pendingPickCallId = null
                if (id == null) return
                val uri = data?.data
                if (resultCode != RESULT_OK || uri == null) {
                    replyOk(id, "null")
                    return
                }
                val summary = importFromUri(uri)
                if (summary == null) {
                    replyErr(id, "That is not a Peer profile")
                } else {
                    replyOk(id, summary)
                }
            }
            REQUEST_VPN_PERMISSION -> {
                val config = pendingVpnConfig
                pendingVpnConfig = null
                if (resultCode == RESULT_OK && config != null) {
                    startVpn(config)
                } else {
                    val id = pendingConnectCallId
                    pendingConnectCallId = null
                    id?.let { replyErr(it, "VPN permission is required to connect") }
                }
            }
        }
    }

    companion object {
        private const val REQUEST_CAMERA_PERMISSION = 2401
        private const val REQUEST_VPN_PERMISSION = 2402
        private const val REQUEST_IMPORT_PROFILE = 2403
        private const val STATUS_POLL_MS = 1000L
        private const val STATUS_BAR_HEIGHT = "status_bar_height"
        private const val NAV_BAR_HEIGHT = "navigation_bar_height"

        /** Reserved for exactly this; it resolves nowhere. */
        private const val UI_HOST = "appassets.androidplatform.net"
        private const val UI_URL = "https://appassets.androidplatform.net/index.html"

        /** The one canvas colour, so the window matches the bundle's background. */
        private val COLOR_CANVAS = Color.rgb(248, 250, 252)
    }
}

/**
 * Reads at most [limit] bytes.
 *
 * A content provider can report any length, or none, so the cap is enforced
 * while reading rather than trusted up front.
 */
private fun InputStream.readAtMostBytes(limit: Int): ByteArray {
    val buffer = ByteArray(limit)
    var filled = 0
    while (filled < limit) {
        val read = read(buffer, filled, limit - filled)
        if (read <= 0) break
        filled += read
    }
    return buffer.copyOf(filled)
}
