package com.buhuipao.tunnelproxy

import android.content.Context
import android.content.SharedPreferences
import org.json.JSONObject

/**
 * The signed-in Platform account, and the local names a `.peer` cannot carry.
 *
 * Storage is the same app-private `SharedPreferences` file that already holds
 * the Peer profile — which contains that Peer's private key. A session token is
 * a strictly smaller secret than the key sitting beside it, so this adds no new
 * class of secret to the file and no new place to wipe on sign-out.
 *
 * The in-flight device code is deliberately *not* stored. It lives for ten
 * minutes and a Client closed mid-approval should start over rather than resume
 * a grant whose code the owner can no longer see.
 */
object PlatformAccountStore {
    private const val KEY_PLATFORM_URL = "platform_account_url"
    private const val KEY_ACCESS_TOKEN = "platform_account_token"
    private const val KEY_EXPIRES_AT = "platform_account_expires_at"
    private const val KEY_EMAIL = "platform_account_email"
    private const val KEY_PEER_LABELS = "peer_labels_json"

    /** A token that dies mid-request is worse than one refreshed early. */
    private const val EXPIRY_GRACE_SECONDS = 60L

    const val DEFAULT_PLATFORM_URL = "https://lantunnel.app"

    /** In memory only, for as long as one approval is outstanding. */
    @Volatile
    private var pendingDeviceCode: String? = null

    data class Session(
        val platformUrl: String,
        val accessToken: String,
        val expiresAtUnix: Long,
        val email: String?,
    )

    private fun prefs(context: Context): SharedPreferences = MobileConfig.preferences(context)

    private fun nowUnix(): Long = System.currentTimeMillis() / 1000

    /** The stored session, or null when there is none or it has expired. */
    fun session(context: Context): Session? {
        val p = prefs(context)
        val token = p.getString(KEY_ACCESS_TOKEN, "")?.trim().orEmpty()
        if (token.isEmpty()) return null
        val expiresAt = p.getLong(KEY_EXPIRES_AT, 0)
        if (expiresAt <= nowUnix() + EXPIRY_GRACE_SECONDS) {
            // Expired is signed out. Clearing here means no caller has to
            // reason about a half-signed-in state.
            signOut(context)
            return null
        }
        return Session(
            platformUrl = p.getString(KEY_PLATFORM_URL, DEFAULT_PLATFORM_URL)
                ?.trim()
                ?.ifEmpty { DEFAULT_PLATFORM_URL }
                ?: DEFAULT_PLATFORM_URL,
            accessToken = token,
            expiresAtUnix = expiresAt,
            email = p.getString(KEY_EMAIL, "")?.trim()?.ifEmpty { null },
        )
    }

    fun store(context: Context, session: Session) {
        prefs(context).edit()
            .putString(KEY_PLATFORM_URL, session.platformUrl)
            .putString(KEY_ACCESS_TOKEN, session.accessToken)
            .putLong(KEY_EXPIRES_AT, session.expiresAtUnix)
            .putString(KEY_EMAIL, session.email.orEmpty())
            .apply()
    }

    /** Forgets the token and any grant still in flight. Repeatable. */
    fun signOut(context: Context) {
        pendingDeviceCode = null
        prefs(context).edit()
            .remove(KEY_PLATFORM_URL)
            .remove(KEY_ACCESS_TOKEN)
            .remove(KEY_EXPIRES_AT)
            .remove(KEY_EMAIL)
            .apply()
    }

    fun rememberPendingDeviceCode(code: String?) {
        pendingDeviceCode = code?.takeIf { it.isNotBlank() }
    }

    fun pendingDeviceCode(): String? = pendingDeviceCode

    /**
     * Gives up the in-flight grant, but only if it is still the one that
     * started this poll.
     *
     * The owner can sign out or start over while a request is in the air.
     * Storing the token that lands afterwards would sign them back in behind
     * their own back, with the panel still showing signed out.
     */
    @Synchronized
    fun releasePendingDeviceCode(matching: String): Boolean {
        if (pendingDeviceCode != matching) return false
        pendingDeviceCode = null
        return true
    }

    fun statusJson(context: Context): String {
        val current = session(context)
        return JSONObject()
            .put("signed_in", current != null)
            .put("email", current?.email ?: JSONObject.NULL)
            .put("expires_at_unix", current?.expiresAtUnix ?: JSONObject.NULL)
            .put("platform_url", current?.platformUrl ?: DEFAULT_PLATFORM_URL)
            .toString()
    }

    // ------------------------------------------------------------- labels

    /** The longest name kept for one Tunnel or Peer, matching the desktop. */
    const val MAX_LABEL_CHARS = 64

    private fun labels(context: Context): JSONObject =
        runCatching { JSONObject(prefs(context).getString(KEY_PEER_LABELS, "{}").orEmpty()) }
            .getOrDefault(JSONObject())

    private fun normalize(value: String?): String? =
        value?.trim()?.takeIf { it.isNotEmpty() }?.take(MAX_LABEL_CHARS)

    /** Records the local names for one Tunnel. Empty names remove the entry. */
    fun setLabels(context: Context, tunnelId: String, tunnelName: String?, peerName: String?) {
        if (tunnelId.isBlank()) return
        val stored = labels(context)
        val tunnel = normalize(tunnelName)
        val peer = normalize(peerName)
        if (tunnel == null && peer == null) {
            stored.remove(tunnelId)
        } else {
            // Absent names are omitted, never written as JSONObject.NULL:
            // org.json renders that sentinel back through optString as the
            // literal string "null", which would label a row `null - laptop`.
            val entry = JSONObject()
            if (tunnel != null) entry.put("tunnel_name", tunnel)
            if (peer != null) entry.put("peer_name", peer)
            stored.put(tunnelId, entry)
        }
        prefs(context).edit().putString(KEY_PEER_LABELS, stored.toString()).apply()
    }

    fun forgetLabels(context: Context, tunnelId: String) =
        setLabels(context, tunnelId, null, null)

    /** Writes whatever names are known for this Tunnel onto a summary object. */
    fun decorate(context: Context, summary: JSONObject, tunnelId: String): JSONObject {
        val entry = labels(context).optJSONObject(tunnelId) ?: return summary
        for (key in listOf("tunnel_name", "peer_name")) {
            // `isNull` is the only way to tell an absent name from one stored
            // as JSONObject.NULL by an older build of this file.
            if (entry.isNull(key)) continue
            entry.optString(key).takeIf { it.isNotEmpty() }?.let { summary.put(key, it) }
        }
        return summary
    }
}
