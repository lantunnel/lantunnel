package com.buhuipao.tunnelproxy

import android.webkit.JavascriptInterface
import org.json.JSONArray
import org.json.JSONObject

/**
 * The Android end of the one bridge every Client speaks.
 *
 * The UI posts `{id, command, args}` and gets an answer back by id. Nothing
 * here decides what a screen says — the wording, the ordering and the state
 * vocabulary all come from the shared bundle and from `client_ui` in Rust.
 * This file only answers questions.
 *
 * `postMessage` is called on a WebView-owned thread, never the main thread, so
 * anything touching an Activity hops back explicitly.
 */
class WebBridge(private val host: Host) {

    /** What the Activity has to be able to do for the UI. */
    interface Host {
        fun runOnUi(action: () -> Unit)
        fun replyOk(id: Int, json: String)
        fun replyErr(id: Int, message: String)

        fun statusJson(): String
        fun proxyStatusJson(): String
        fun settingsJson(): String
        fun saveSettings(settings: JSONObject)
        fun productInfoJson(): String
        fun peerProfilesJson(): String
        fun forgetPeerProfile(tunnelId: String): String
        fun pickPeerProfile(id: Int)
        fun scanPeerProfile(id: Int)
        fun connect(tunnelId: String, id: Int)
        fun disconnect(id: Int)
        fun logsJson(limit: Int): String
        fun clearLogs()
        fun setLogLevel(level: String)
        fun copyToClipboard(text: String)
    }

    @JavascriptInterface
    fun postMessage(payload: String) {
        val call = runCatching { JSONObject(payload) }.getOrNull()
        val id = call?.optInt("id", -1) ?: -1
        if (call == null || id < 0) {
            // Without an id there is nobody to answer, so this can only be
            // logged. A malformed call is a bug in the bundle, not user input.
            ProxyServiceState.appendLog(HOST_LOG_CONTEXT ?: return, "bridge: unreadable call")
            return
        }
        val command = call.optString("command")
        val args = call.optJSONObject("args") ?: JSONObject()
        // Every handler runs on the main thread: they read Activity state, and
        // several of them start an Activity for a result.
        host.runOnUi { dispatch(id, command, args) }
    }

    private fun dispatch(id: Int, command: String, args: JSONObject) {
        try {
            when (command) {
                "get_capabilities" -> host.replyOk(id, CAPABILITIES)
                "get_status" -> host.replyOk(id, host.statusJson())
                "get_proxy_status" -> host.replyOk(id, host.proxyStatusJson())
                "get_settings" -> host.replyOk(id, host.settingsJson())
                "get_product_info" -> host.replyOk(id, host.productInfoJson())
                "list_peer_profiles" -> host.replyOk(id, host.peerProfilesJson())
                "get_logs" -> host.replyOk(id, host.logsJson(args.optInt("limit", 500)))

                "save_settings" -> {
                    val settings = args.optJSONObject("settings")
                    if (settings == null) {
                        host.replyErr(id, "save_settings needs a settings object")
                    } else {
                        host.saveSettings(settings)
                        host.replyOk(id, "null")
                    }
                }
                "forget_peer_profile" ->
                    host.replyOk(id, host.forgetPeerProfile(args.optString("tunnelId")))
                "clear_logs" -> {
                    host.clearLogs()
                    host.replyOk(id, "null")
                }
                "set_log_level" -> {
                    host.setLogLevel(args.optString("level", "info"))
                    host.replyOk(id, "null")
                }
                "write_clipboard_text" -> {
                    host.copyToClipboard(args.optString("text"))
                    host.replyOk(id, "null")
                }

                // These answer later: a picker, a camera and a VPN consent
                // dialog all return through onActivityResult.
                "pick_peer_profile" -> host.pickPeerProfile(id)
                "scan_peer_profile" -> host.scanPeerProfile(id)
                "connect_peer_profile" -> host.connect(args.optString("tunnelId"), id)
                "disconnect" -> host.disconnect(id)

                // A phone has no loopback proxy to configure and no privileged
                // helper to install, so the UI never asks. Answering plainly
                // beats a silent hang if a future bundle does.
                "get_clash_config", "install_tun_helper" ->
                    host.replyErr(id, "$command is not available on this device")

                else -> host.replyErr(id, "unknown command: $command")
            }
        } catch (e: Throwable) {
            // A throw here would otherwise cross back into the WebView and
            // leave the call pending forever, which reads on screen as a
            // button that does nothing.
            host.replyErr(id, e.message ?: e::class.java.simpleName)
        }
    }

    companion object {
        /** Set once by the Activity so a malformed call can still be logged. */
        var HOST_LOG_CONTEXT: android.content.Context? = null

        /**
         * What this Client genuinely cannot do, not what looks different.
         *
         * A phone routes every app through its VPN service, so a loopback
         * SOCKS5 port would serve nobody; there is no login item; and it
         * reports no interface facts, so per-prefix export readiness is not
         * something it can answer.
         */
        val CAPABILITIES: String = JSONObject()
            .put("qrScanner", true)
            .put("startAtLogin", false)
            .put("localProxy", false)
            .put("exportReadiness", false)
            .toString()

        fun jsonArrayOfStrings(values: List<String>): String =
            JSONArray().also { array -> values.forEach(array::put) }.toString()
    }
}
