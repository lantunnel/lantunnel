package com.buhuipao.tunnelproxy

import android.content.Context
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

data class ProxyServiceState(
    val state: String = STATE_STOPPED,
    val message: String = "",
    val statusJson: String = "",
    val updatedAtMs: Long = 0L,
) {
    val isConnecting: Boolean get() = state == STATE_CONNECTING
    val isStopping: Boolean get() = state == STATE_STOPPING
    val isRunning: Boolean get() = state == STATE_RUNNING
    val isFailed: Boolean get() = state == STATE_FAILED

    fun isStaleTransient(
        nowMs: Long = System.currentTimeMillis(),
        timeoutMs: Long = STALE_TRANSIENT_STATE_TIMEOUT_MS,
    ): Boolean {
        if (!isConnecting && !isStopping) {
            return false
        }
        if (updatedAtMs <= 0L) {
            return true
        }
        return nowMs - updatedAtMs >= timeoutMs
    }

    fun lastErrorMessage(): String {
        if (message.isNotBlank()) {
            return message
        }
        return runCatching {
            val lastError = JSONObject(statusJson).optJSONObject("last_error")
            lastError?.optString("error").orEmpty()
        }.getOrDefault("")
    }

    companion object {
        const val STATE_STOPPED = "Stopped"
        const val STATE_CONNECTING = "Connecting"
        const val STATE_STOPPING = "Stopping"
        const val STATE_RUNNING = "Running"
        const val STATE_FAILED = "Failed"
        const val STALE_TRANSIENT_STATE_TIMEOUT_MS = 30_000L

        private const val PREFS_NAME = "proxy_service_state"
        private const val KEY_STATE = "state"
        private const val KEY_MESSAGE = "message"
        private const val KEY_STATUS_JSON = "status_json"
        private const val KEY_UPDATED_AT_MS = "updated_at_ms"
        private const val KEY_LOGS = "logs"

        fun fromPreferences(context: Context): ProxyServiceState {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            return ProxyServiceState(
                state = prefs.getString(KEY_STATE, STATE_STOPPED) ?: STATE_STOPPED,
                message = prefs.getString(KEY_MESSAGE, "") ?: "",
                statusJson = prefs.getString(KEY_STATUS_JSON, "") ?: "",
                updatedAtMs = prefs.getLong(KEY_UPDATED_AT_MS, 0L),
            )
        }

        fun save(
            context: Context,
            state: String,
            message: String = "",
            statusJson: String = "",
        ) {
            appendLog(context, "$state: $message")
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putString(KEY_STATE, state)
                .putString(KEY_MESSAGE, message)
                .putString(KEY_STATUS_JSON, statusJson)
                .putLong(KEY_UPDATED_AT_MS, System.currentTimeMillis())
                .apply()
        }

        fun appendLog(context: Context, line: String) {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            val now = SimpleDateFormat("HH:mm:ss", Locale.US).format(Date())
            val previous = prefs.getString(KEY_LOGS, "") ?: ""
            val next = (previous.lines() + "$now $line")
                .filter { it.isNotBlank() }
                .takeLast(160)
                .joinToString("\n")
            prefs.edit().putString(KEY_LOGS, next).apply()
        }

        fun logs(context: Context): String {
            return context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .getString(KEY_LOGS, "") ?: ""
        }

        fun clearLogs(context: Context) {
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .remove(KEY_LOGS)
                .apply()
        }
    }
}
