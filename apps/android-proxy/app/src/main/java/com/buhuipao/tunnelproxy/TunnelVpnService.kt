package com.buhuipao.tunnelproxy

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.net.VpnService
import android.os.Build
import android.os.IBinder
import android.os.ParcelFileDescriptor
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicInteger

class TunnelVpnService : VpnService() {
    @Volatile
    private var proxyStarted = false
    @Volatile
    private var startThread: Thread? = null
    @Volatile
    private var stopThread: Thread? = null
    @Volatile
    private var stopRequested = false
    @Volatile
    private var tunFd: ParcelFileDescriptor? = null
    @Volatile
    private var tun2SocksThread: Thread? = null
    @Volatile
    private var tun2SocksRunning = false

    override fun onBind(intent: Intent?): IBinder? {
        return super.onBind(intent)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopRequested = true
            createNotificationChannel()
            startForeground(NOTIFICATION_ID, notification("Disconnecting"))
            ProxyServiceState.save(this, ProxyServiceState.STATE_STOPPING, "Disconnecting VPN")
            stopVpnInBackground("Disconnect requested")
            return START_STICKY
        }

        createNotificationChannel()
        startForeground(NOTIFICATION_ID, notification("Starting VPN"))

        val requestJson = intent?.getStringExtra(EXTRA_START_JSON)
        if (requestJson.isNullOrBlank()) {
            failAndStop("Missing VPN config")
            return START_NOT_STICKY
        }

        val routes = routesFromJson(intent.getStringExtra(EXTRA_ROUTES_JSON))
        val validation = MobileConfig.validateLanRoutes(routes)
        if (!validation.first) {
            failAndStop(validation.second)
            return START_NOT_STICKY
        }

        val currentThread = startThread
        if (currentThread?.isAlive == true) {
            return START_STICKY
        }

        stopRequested = false
        ProxyServiceState.save(this, ProxyServiceState.STATE_CONNECTING, "Starting VPN")
        updateNotification("Starting VPN")
        startThread = Thread({
            runVpn(requestJson, routes)
        }, "tp-android-vpn-start").apply { start() }
        return START_STICKY
    }

    override fun onDestroy() {
        if (!stopRequested) {
            stopRequested = true
            stopVpnInBackground("VPN service destroyed")
        }
        super.onDestroy()
    }

    private fun runVpn(requestJson: String, routes: List<String>) {
        ProxyServiceState.appendLog(this, "VPN startProxy entered")
        val tunMtu = tunMtuFromStartJson(requestJson)
        ProxyServiceState.appendLog(this, "VPN TUN MTU=$tunMtu")
        val code = TunnelProxyNative.startProxy(requestJson)
        ProxyServiceState.appendLog(this, "VPN startProxy returned code=$code")
        if (code != TunnelProxyNative.OK) {
            if (stopRequested) {
                ProxyServiceState.save(this, ProxyServiceState.STATE_STOPPED, "Disconnected")
                updateNotification("Stopped")
                stopSelf()
                return
            }
            val status = TunnelProxyNative.statusJson()
            val message = parseStartFailure(status, code)
            ProxyServiceState.save(this, ProxyServiceState.STATE_FAILED, message, status)
            updateNotification("VPN start failed")
            stopSelf()
            return
        }

        proxyStarted = true
        if (stopRequested) {
            stopVpnInBackground("Disconnect requested during startup")
            return
        }

        val pfd = establishVpn(routes, tunMtu)
        if (pfd == null) {
            stopVpnInBackground("VPN establish failed")
            return
        }
        tunFd = pfd

        val runtimeConfig = TunnelProxyNative.runtimeConfigJson()
        if (runtimeConfig.contains("\"ok\":false")) {
            ProxyServiceState.appendLog(this, "runtimeConfigJson failed: $runtimeConfig")
            stopVpnInBackground(parseRuntimeConfigFailure(runtimeConfig))
            return
        }

        val tun2SocksConfig = buildTun2SocksConfig(runtimeConfig, tunMtu)
        val tun2SocksStartError = startTun2SocksInBackground(tun2SocksConfig, pfd.fd)
        if (tun2SocksStartError != null) {
            stopVpnInBackground(tun2SocksStartError)
            return
        }

        val status = TunnelProxyNative.statusJson()
        ProxyServiceState.save(this, ProxyServiceState.STATE_RUNNING, "VPN running", status)
        updateNotification("VPN running")
    }

    private fun establishVpn(routes: List<String>, tunMtu: Int): ParcelFileDescriptor? {
        val builder = Builder()
            .setSession("Lantunnel")
            .addAddress(TUN_ADDRESS, 32)
            .setMtu(tunMtu)

        runCatching {
            builder.addDisallowedApplication(packageName)
        }.onFailure {
            ProxyServiceState.appendLog(this, "addDisallowedApplication failed: ${it.message}")
        }

        for (route in routes) {
            val parsed = parseRoute(route)
            if (parsed == null) {
                ProxyServiceState.save(this, ProxyServiceState.STATE_FAILED, "Invalid route: $route")
                return null
            }
            builder.addRoute(parsed.first, parsed.second)
        }

        val pfd = builder.establish()
        if (pfd == null) {
            ProxyServiceState.save(this, ProxyServiceState.STATE_FAILED, "Android VPN establish returned null")
            ProxyServiceState.appendLog(this, "VPN establish returned null")
        }
        return pfd
    }

    private fun stopVpnInBackground(reason: String) {
        val currentThread = stopThread
        if (currentThread?.isAlive == true) {
            ProxyServiceState.appendLog(this, "VPN stop already running: $reason")
            return
        }

        stopThread = Thread({
            ProxyServiceState.appendLog(this, "VPN stop entered: $reason")
            val nativeTunRunning = runCatching { Tun2SocksNative.isRunning() }.getOrDefault(false)
            val tunThreadAlive = tun2SocksThread?.isAlive == true
            if (tun2SocksRunning || nativeTunRunning || tunThreadAlive) {
                runCatching { Tun2SocksNative.stop() }
                    .onFailure {
                        ProxyServiceState.appendLog(
                            this,
                            "Tun2SocksNative stop failed: ${it.message}",
                        )
                    }
            } else {
                ProxyServiceState.appendLog(this, "Tun2SocksNative stop skipped: idle")
            }
            runCatching { tun2SocksThread?.join(2_000) }
            tun2SocksThread = null
            runCatching { tunFd?.close() }
            tunFd = null
            val startInProgress = startThread?.isAlive == true
            if (proxyStarted || startInProgress) {
                val code = TunnelProxyNative.stopProxy()
                ProxyServiceState.appendLog(this, "VPN stopProxy returned code=$code")
                proxyStarted = false
            }
            ProxyServiceState.save(this, ProxyServiceState.STATE_STOPPED, "Disconnected")
            updateNotification("Stopped")
            stopSelf()
        }, "tp-android-stop").apply { start() }
    }

    private fun startTun2SocksInBackground(config: String, tunFd: Int): String? {
        val currentThread = tun2SocksThread
        if (currentThread?.isAlive == true) {
            return null
        }

        val result = AtomicInteger(TUN2SOCKS_RUNNING)
        val thread = Thread({
            ProxyServiceState.appendLog(this, "Tun2SocksNative start entered")
            if (stopRequested) {
                val code = -3
                result.set(code)
                ProxyServiceState.appendLog(this, "Tun2SocksNative start cancelled before native entry")
                return@Thread
            }
            tun2SocksRunning = true
            var code = -1
            try {
                code = Tun2SocksNative.start(config, tunFd)
            } catch (e: Throwable) {
                ProxyServiceState.appendLog(this, "Tun2SocksNative start failed: ${e.message}")
            } finally {
                tun2SocksRunning = false
            }
            result.set(code)
            ProxyServiceState.appendLog(this, "Tun2SocksNative start returned code=$code")
            if (!stopRequested) {
                val message = "tun2socks stopped unexpectedly: $code"
                ProxyServiceState.save(this, ProxyServiceState.STATE_FAILED, message)
                updateNotification("VPN failed")
                stopVpnInBackground(message)
            }
        }, "tp-android-tun2socks")
        tun2SocksThread = thread
        thread.start()

        Thread.sleep(TUN2SOCKS_START_GRACE_MS)
        if (!thread.isAlive) {
            val code = result.get()
            return if (code == 0) {
                "tun2socks exited during startup"
            } else {
                "tun2socks start failed: $code"
            }
        }
        if (!runCatching { Tun2SocksNative.isRunning() }.getOrDefault(false)) {
            return "tun2socks native runtime did not report running"
        }
        ProxyServiceState.appendLog(this, "Tun2SocksNative running")
        return null
    }

    private fun buildTun2SocksConfig(runtimeConfigJson: String, tunMtu: Int): String {
        return Tun2SocksConfigBuilder.build(runtimeConfigJson, tunMtu, TUN_ADDRESS)
    }

    private fun parseRuntimeConfigFailure(raw: String): String {
        return runCatching {
            JSONObject(raw).optString("error")
        }.getOrDefault("").ifBlank {
            "mobile runtime config unavailable"
        }
    }

    private fun failAndStop(message: String) {
        ProxyServiceState.save(this, ProxyServiceState.STATE_FAILED, message)
        updateNotification("VPN failed")
        stopSelf()
    }

    private fun parseStartFailure(statusJson: String, code: Int): String {
        return runCatching {
            val lastError = JSONObject(statusJson).optJSONObject("last_error")
            lastError?.optString("error").orEmpty()
        }.getOrDefault("").ifBlank {
            "Start failed: $code"
        }
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Lantunnel VPN",
            NotificationManager.IMPORTANCE_LOW,
        )
        notificationManager().createNotificationChannel(channel)
    }

    private fun updateNotification(status: String) {
        notificationManager().notify(NOTIFICATION_ID, notification(status))
    }

    private fun notification(status: String): Notification {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Lantunnel")
                .setContentText(status)
                .setSmallIcon(android.R.drawable.stat_sys_upload_done)
                .setOngoing(true)
                .build()
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
                .setContentTitle("Lantunnel")
                .setContentText(status)
                .setSmallIcon(android.R.drawable.stat_sys_upload_done)
                .setOngoing(true)
                .build()
        }
    }

    private fun notificationManager(): NotificationManager {
        return getSystemService(NotificationManager::class.java)
    }

    companion object {
        private const val ACTION_START = "com.buhuipao.tunnelproxy.action.VPN_START"
        private const val ACTION_STOP = "com.buhuipao.tunnelproxy.action.VPN_STOP"
        private const val EXTRA_START_JSON = "com.buhuipao.tunnelproxy.extra.START_JSON"
        private const val EXTRA_ROUTES_JSON = "com.buhuipao.tunnelproxy.extra.ROUTES_JSON"
        private const val CHANNEL_ID = "vpn"
        private const val NOTIFICATION_ID = 1002
        private const val TUN_ADDRESS = "10.255.0.2"
        // Keep the Android app-facing TUN MTU large. The Rust transport has
        // its own datagram fragmentation, while a 1400-byte TUN forces common
        // Moonlight/Sunshine UDP packets through Android IP fragmentation
        // before tun2socks can proxy them.
        private const val DEFAULT_TUN_MTU = 8500
        private const val MIN_TUN_MTU = 1280
        private const val MAX_TUN_MTU = 8500
        private const val TUN2SOCKS_RUNNING = Int.MIN_VALUE
        private const val TUN2SOCKS_START_GRACE_MS = 500L

        fun start(context: Context, requestJson: String, routes: List<String>) {
            val intent = Intent(context, TunnelVpnService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_START_JSON, requestJson)
                putExtra(EXTRA_ROUTES_JSON, JSONArray(routes).toString())
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            val intent = Intent(context, TunnelVpnService::class.java).apply {
                action = ACTION_STOP
            }
            context.startService(intent)
        }

        private fun routesFromJson(raw: String?): List<String> {
            if (raw.isNullOrBlank()) {
                return MobileConfig.DEFAULT_LAN_ROUTES
            }
            return runCatching {
                val json = JSONArray(raw)
                List(json.length()) { index -> json.getString(index).trim() }
                    .filter { it.isNotBlank() }
                    .ifEmpty { MobileConfig.DEFAULT_LAN_ROUTES }
            }.getOrDefault(MobileConfig.DEFAULT_LAN_ROUTES)
        }

        private fun tunMtuFromStartJson(raw: String): Int {
            return runCatching {
                JSONObject(raw)
                    .optInt("tun_mtu", DEFAULT_TUN_MTU)
                    .coerceIn(MIN_TUN_MTU, MAX_TUN_MTU)
            }.getOrDefault(DEFAULT_TUN_MTU)
        }

        private fun parseRoute(route: String): Pair<String, Int>? {
            val parts = route.trim().split('/')
            if (parts.size != 2) {
                return null
            }
            val prefix = parts[1].toIntOrNull() ?: return null
            return parts[0] to prefix
        }
    }
}

internal object Tun2SocksConfigBuilder {
    fun build(runtimeConfigJson: String, tunMtu: Int, tunAddress: String): String {
        val socks5 = JSONObject(runtimeConfigJson).getJSONObject("local_socks5")
        val host = socks5.getString("host")
        val port = socks5.getInt("port")
        // The runtime emits auth_enabled: false and never a credential pair.
        // Defaulting to true meant a missing key sent this into getString(
        // "username"), which throws on the VPN start thread and leaves the
        // tunnel silently unestablished.
        val authEnabled = socks5.optBoolean("auth_enabled", false)
        return buildString {
            appendLine("tunnel:")
            appendLine("  mtu: $tunMtu")
            appendLine("  multi-queue: false")
            appendLine("  ipv4: $tunAddress")
            appendLine("socks5:")
            appendLine("  port: $port")
            appendLine("  address: $host")
            appendLine("  udp: udp")
            if (authEnabled) {
                appendLine("  username: \"${socks5.optString("username")}\"")
                appendLine("  password: \"${socks5.optString("password")}\"")
            }
            appendLine("misc:")
            appendLine("  task-stack-size: 131072")
            appendLine("  tcp-buffer-size: 65536")
            appendLine("  udp-recv-buffer-size: 4194304")
            appendLine("  udp-copy-buffer-nums: 64")
            appendLine("  connect-timeout: 5000")
            appendLine("  tcp-read-write-timeout: 300000")
            appendLine("  udp-read-write-timeout: 60000")
            appendLine("  log-level: warn")
        }.trimEnd()
    }
}
