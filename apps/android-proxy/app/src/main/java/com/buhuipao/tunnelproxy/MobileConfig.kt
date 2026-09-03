package com.buhuipao.tunnelproxy

import android.content.Context
import android.content.SharedPreferences
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

data class MobileConfig(
    val peerProfileJson: String = "",
    val deviceId: String = "",
    val localSocks5Listen: String = DEFAULT_LOCAL_SOCKS5_LISTEN,
    val lanP2pEnabled: Boolean = DEFAULT_LAN_P2P_ENABLED,
    val lanRoutes: List<String> = DEFAULT_LAN_ROUTES,
    /** Route what Peers publish, rather than a list typed by hand. */
    /** Networks this phone publishes to the Tunnel. */
    val exportedLans: List<String> = emptyList(),
    /** Keep a locally reachable network on the local path. */
    val tunnelFirst: Boolean = false,
    /** Refuse every destination, including ones this device publishes. */
    val blockAllIncoming: Boolean = false,
    /** Reconnect the imported Peer profile when the app opens. */
    val autoConnect: Boolean = false,
    /**
     * Named access rules, one per line: `allow|deny <target> <tcp|udp> [port]`.
     *
     * Typed rather than assembled from pickers: on a phone a row of dropdowns
     * per rule is worse than a line someone can read back.
     */
    val accessRules: List<String> = emptyList(),
    /**
     * The Access policy exactly as the shared UI writes it.
     *
     * `{"allow":[...],"deny":[...]}` with the same rule objects the engine
     * takes and the desktop edits. The line list above was only ever an
     * Android-UI storage format; it is still read so an existing install keeps
     * its rules, but nothing writes it any more.
     */
    val clientAccessJson: String = "",
) {
    fun buildStartJson(): String {
        return JSONObject()
            .put("peer_profile", JSONObject(normalizePeerProfileJson(peerProfileJson)))
            .put("device_id", deviceId.trim())
            .put("local_socks5_listen", localSocks5Listen.trim().ifEmpty { DEFAULT_LOCAL_SOCKS5_LISTEN })
            .put("p2p_allow_lan_candidates", lanP2pEnabled)
            .put("client_access", clientAccessJson())
            .put("exported_lans", JSONArray().also { array -> exportedLans.forEach(array::put) })
            .put("tunnel_first", tunnelFirst)
            .toString()
    }

    /**
     * Who may reach what this device publishes.
     *
     * Empty means every Peer in the Tunnel may. That was the only possible
     * answer while a phone could publish nothing; now that it can, its owner
     * needs a way to say no.
     *
     * Closed names both address families on both protocols. Naming one left
     * every IPv6 destination open while the screen said nothing was reachable.
     */
    /**
     * One rule line into the wire shape the engine compiles.
     *
     * A line that cannot be read fails the start. Dropping it silently would
     * leave the owner looking at a rule that is not in force.
     */
    private fun accessRuleJson(line: String): Pair<String, JSONObject> {
        val parts = line.trim().split(Regex("\\s+"))
        require(parts.size in 3..4) { "not a rule: $line" }
        val list = parts[0].lowercase()
        require(list == "allow" || list == "deny") { "rule must start with allow or deny: $line" }
        val target = parts[1]
        val protocol = parts[2].lowercase()
        require(protocol == "tcp" || protocol == "udp") { "protocol must be tcp or udp: $line" }

        val targetJson = when {
            target.equals("this_peer", ignoreCase = true) ->
                JSONObject().put("type", "this_peer")
            target.contains('/') ->
                JSONObject().put("type", "cidr").put("value", target)
            target.count { it == '.' } == 3 && target.all { it.isDigit() || it == '.' } ->
                JSONObject().put("type", "ip").put("value", target)
            else -> JSONObject().put("type", "host").put("value", target)
        }
        val portJson = if (parts.size == 4) {
            val port = parts[3].toIntOrNull()
            require(port != null && port in 1..65535) { "port must be 1-65535: $line" }
            JSONObject().put("type", "exact").put("value", port)
        } else {
            JSONObject().put("type", "any")
        }
        return list to JSONObject()
            .put("target", targetJson)
            .put("protocol", protocol)
            .put("port", portJson)
    }

    /**
     * The first stored line that is not a rule, or null when every line reads.
     *
     * buildStartJson throws on an unreadable line on purpose: dropping one
     * would leave the owner looking at a restriction that is not in force. But
     * Connect calls it from a click handler, where the throw took the app down
     * instead of naming the line. The caller asks first now.
     */
    fun unreadableAccessRule(): String? = accessRules
        .map { it.trim() }
        .filter { it.isNotBlank() }
        .firstOrNull { line -> runCatching { accessRuleJson(line) }.isFailure }

    private fun clientAccessJson(): JSONObject {
        clientAccessJson.trim().takeIf { it.isNotEmpty() }?.let { raw ->
            runCatching { JSONObject(raw) }
                .map { policy ->
                    JSONObject()
                        .put("allow", policy.optJSONArray("allow") ?: JSONArray())
                        .put("deny", policy.optJSONArray("deny") ?: JSONArray())
                }
                .getOrNull()
                ?.let { return it }
        }
        return legacyClientAccessPolicy()
    }

    /**
     * The policy an install from before the shared UI is still holding.
     *
     * Rules were text lines and "Block all" was a separate flag. Both are read
     * once, on the way into the shared editor, so upgrading does not silently
     * drop the rules someone wrote.
     */
    fun legacyClientAccessPolicy(): JSONObject {
        val allow = JSONArray()
        val deny = JSONArray()
        for (line in accessRules.map { it.trim() }.filter { it.isNotBlank() }) {
            val (list, rule) = accessRuleJson(line)
            if (list == "allow") allow.put(rule) else deny.put(rule)
        }
        if (blockAllIncoming) {
            for (cidr in listOf("0.0.0.0/0", "::/0")) {
                for (protocol in listOf("tcp", "udp")) {
                    deny.put(
                        JSONObject()
                            .put("target", JSONObject().put("type", "cidr").put("value", cidr))
                            .put("protocol", protocol)
                            .put("port", JSONObject().put("type", "any")),
                    )
                }
            }
        }
        return JSONObject().put("allow", allow).put("deny", deny)
    }

    fun saveConfig(prefs: SharedPreferences) {
        val persistedPeerProfile = peerProfileJson.trim().let { raw ->
            if (raw.isEmpty()) "" else normalizePeerProfileJson(raw)
        }
        prefs.edit()
            .putString(KEY_PEER_PROFILE_JSON, persistedPeerProfile)
            .putString(KEY_DEVICE_ID, deviceId.trim().ifEmpty { ensureDeviceId(prefs) })
            .putString(KEY_LOCAL_SOCKS5_LISTEN, localSocks5Listen)
            .putBoolean(KEY_LAN_P2P_ENABLED, lanP2pEnabled)
            .putString(KEY_LAN_ROUTES, JSONArray(lanRoutes.map { normalizeRoute(it) }).toString())
            .putString(KEY_EXPORTED_LANS, JSONArray().also { a -> exportedLans.forEach(a::put) }.toString())
            .putBoolean(KEY_TUNNEL_FIRST, tunnelFirst)
            .putBoolean(KEY_BLOCK_ALL_INCOMING, blockAllIncoming)
            .putBoolean(KEY_AUTO_CONNECT, autoConnect)
            .putString(KEY_ACCESS_RULES, JSONArray().also { a -> accessRules.forEach(a::put) }.toString())
            .putString(KEY_CLIENT_ACCESS_JSON, clientAccessJson)
            .apply()
    }

    companion object {
        const val DEFAULT_LOCAL_SOCKS5_LISTEN = "127.0.0.1:1080"
        const val DEFAULT_LAN_P2P_ENABLED = false

        /**
         * How many rows the manual editor offers.
         *
         * A guard against a slip in a text field, not a limit on the mesh: it
         * is deliberately not applied to derived routes or to what is read back
         * from storage, where it silently kept an arbitrary eight of a sorted
         * set and made a large Tunnel unreachable.
         */
        const val MAX_MANUAL_LAN_ROUTES = 8
        val DEFAULT_LAN_ROUTES = listOf(OVERLAY_ROUTE)

        private const val PREFS_NAME = "mobile_config"
        private const val KEY_PEER_PROFILE_JSON = "peer_profile_json"
        private const val KEY_DEVICE_ID = "device_id"
        private const val KEY_LOCAL_SOCKS5_LISTEN = "local_socks5_listen"
        private const val KEY_LAN_P2P_ENABLED = "lan_p2p_enabled"
        private const val KEY_LAN_ROUTES = "lan_routes"
        private const val KEY_EXPORTED_LANS = "exported_lans"
        private const val KEY_TUNNEL_FIRST = "tunnel_first"
        private const val KEY_BLOCK_ALL_INCOMING = "block_all_incoming"
        private const val KEY_AUTO_CONNECT = "auto_connect"
        private const val KEY_ACCESS_RULES = "access_rules"
        private const val KEY_CLIENT_ACCESS_JSON = "client_access_json"

        fun readAutoConnect(prefs: SharedPreferences): Boolean =
            prefs.getBoolean(KEY_AUTO_CONNECT, false)

        fun preferences(context: Context): SharedPreferences {
            return context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
        }

        private fun ensureDeviceId(prefs: SharedPreferences): String {
            val existing = prefs.getString(KEY_DEVICE_ID, "")?.trim().orEmpty()
            if (existing.isNotEmpty()) {
                return existing
            }
            val generated = UUID.randomUUID().toString()
            prefs.edit().putString(KEY_DEVICE_ID, generated).apply()
            return generated
        }

        fun fromPreferences(context: Context): MobileConfig {
            val prefs = preferences(context)
            return MobileConfig(
                peerProfileJson = prefs.getString(KEY_PEER_PROFILE_JSON, "") ?: "",
                deviceId = ensureDeviceId(prefs),
                localSocks5Listen = prefs.getString(KEY_LOCAL_SOCKS5_LISTEN, DEFAULT_LOCAL_SOCKS5_LISTEN)
                    ?: DEFAULT_LOCAL_SOCKS5_LISTEN,
                lanP2pEnabled = prefs.getBoolean(KEY_LAN_P2P_ENABLED, DEFAULT_LAN_P2P_ENABLED),
                lanRoutes = routesFromJson(prefs.getString(KEY_LAN_ROUTES, null)),
                exportedLans = listFromJson(prefs.getString(KEY_EXPORTED_LANS, null)),
                tunnelFirst = prefs.getBoolean(KEY_TUNNEL_FIRST, false),
                blockAllIncoming = prefs.getBoolean(KEY_BLOCK_ALL_INCOMING, false),
                autoConnect = readAutoConnect(prefs),
                accessRules = withoutCorruptedAccessRules(listFromJson(prefs.getString(KEY_ACCESS_RULES, null))),
                clientAccessJson = prefs.getString(KEY_CLIENT_ACCESS_JSON, "").orEmpty(),
            )
        }

        /**
         * Drop the route prefixes an older build wrote into the rules field.
         *
         * routesFromJson handed back DEFAULT_LAN_ROUTES for an absent key, and
         * the access rules were read through it, so phones carry the overlay
         * prefix as a rule. It is not a line anyone could have typed — the
         * field's grammar has always required allow or deny first — so it is
         * ours to clean up rather than theirs. A line that is merely typed
         * wrong is left alone and reported.
         */
        fun withoutCorruptedAccessRules(lines: List<String>): List<String> =
            lines.filterNot { it.trim() in DEFAULT_LAN_ROUTES }

        fun installDeviceId(context: Context): String {
            return ensureDeviceId(preferences(context))
        }

        fun fromPeerProfileJson(rawJson: String, previous: MobileConfig): MobileConfig {
            return previous.copy(peerProfileJson = normalizePeerProfileJson(rawJson))
        }

        /**
         * Largest `.peer` file worth reading into memory.
         *
         * A V2 Peer profile is a small JSON document — identity, tunnel id and
         * bootstrap. Anything past this is the wrong file, and refusing early
         * keeps a mistapped video out of the heap.
         */
        const val MAX_PEER_PROFILE_BYTES = 64 * 1024

        private val UTF8_BOM = byteArrayOf(0xEF.toByte(), 0xBB.toByte(), 0xBF.toByte())

        /**
         * Turns the bytes of a `.peer` file into a validated profile.
         *
         * Whatever wrote the file is out of our hands: an export from Windows
         * carries a byte order mark and CRLF endings, and a share sheet will
         * hand over whatever the user tapped. Decoding is checked here so the
         * caller can show one clear message instead of a JSON parser's.
         */
        fun decodePeerProfileFile(bytes: ByteArray): String {
            require(bytes.isNotEmpty()) { "The selected file is empty" }
            require(bytes.size <= MAX_PEER_PROFILE_BYTES) {
                "The selected file is too large to be a Peer profile"
            }

            val body = if (bytes.size >= UTF8_BOM.size &&
                bytes.copyOfRange(0, UTF8_BOM.size).contentEquals(UTF8_BOM)
            ) {
                bytes.copyOfRange(UTF8_BOM.size, bytes.size)
            } else {
                bytes
            }

            val text = String(body, Charsets.UTF_8).trim()
            require(text.isNotEmpty()) { "The selected file is empty" }
            return normalizePeerProfileJson(text)
        }

        /**
         * The public identity of an imported profile.
         *
         * Everything shown about a Peer profile comes from here, so the parts
         * that must not be shown — the private key above all — have nowhere to
         * appear even by accident.
         */
        data class PeerIdentity(
            val tunnelId: String,
            val peerId: String,
            val overlayIp: String,
        ) {
            /** Enough of the Peer ID to tell two devices apart, without a UUID wall. */
            val shortPeerId: String
                get() = if (peerId.length <= 8) peerId else "\u2026" + peerId.takeLast(8)
        }

        fun peerIdentity(rawJson: String): PeerIdentity? {
            if (rawJson.isBlank()) return null
            return runCatching {
                val profile = JSONObject(rawJson.trim())
                val peer = profile.optJSONObject("peer") ?: return null
                PeerIdentity(
                    tunnelId = profile.optString("tunnel_id"),
                    peerId = peer.optString("peer_id"),
                    overlayIp = peer.optString("overlay_ip"),
                )
            }.getOrNull()
        }

        fun normalizePeerProfileJson(rawJson: String): String {
            val profile = JSONObject(rawJson.trim())
            require(profile.optInt("version") == 2) { "Peer profile version must be 2" }
            require(profile.optString("tunnel_id").isNotBlank()) { "Peer profile tunnel_id is required" }
            require(profile.optJSONObject("peer") != null) { "Peer profile peer identity is required" }
            return profile.toString()
        }

        /**
         * Every Peer address lives here, so this is routed whatever else is.
         *
         * Without it a first connection is impossible: the phone cannot learn
         * what Peers publish until it has joined them, and a tunnel with no
         * routes captures nothing.
         */
        const val OVERLAY_ROUTE = "198.18.0.0/16"

        /**
         * The overlay, plus whatever the other Peers publish.
         *
         * These are the networks worth sending through the tunnel, and the mesh
         * is the side that knows them. Asking the owner to type CIDRs asked a
         * question the mesh answers, and the old default — 192.168.0.0/16 —
         * also covered the Wi-Fi the phone was standing on.
         */
        /**
         * The overlay, then whatever else was asked for.
         *
         * Peer addresses live on the overlay, so it is not a preference. It
         * was an ordinary row in the manual list: deleting it and connecting
         * left the tunnel carrying no route to any Peer address — every Peer
         * unreachable, with nothing on screen saying why.
         */
        fun withOverlay(routes: List<String>): List<String> {
            val out = mutableListOf(OVERLAY_ROUTE)
            routes.filter { it.isNotBlank() && it != OVERLAY_ROUTE }.forEach { out.add(it) }
            return out
        }

        fun routesFromExports(rawStatus: String): List<String> {
            val prefixes = sortedSetOf<String>()
            runCatching {
                val peers = JSONObject(rawStatus.trim())
                    .optJSONObject("peer_directory")
                    ?.optJSONArray("peers")
                for (index in 0 until (peers?.length() ?: 0)) {
                    val exports = peers?.optJSONObject(index)?.optJSONArray("exports") ?: continue
                    for (exportIndex in 0 until exports.length()) {
                        val prefix = exports.optJSONObject(exportIndex)?.optString("prefix").orEmpty()
                        if (prefix.isNotBlank()) prefixes.add(prefix)
                    }
                }
            }
            prefixes.add(OVERLAY_ROUTE)
            return prefixes.toList()
        }

        /**
         * What the mesh publishes, and whether the tunnel is carrying it yet.
         *
         * The VPN's route set is fixed when the tunnel starts, so a prefix a
         * Peer published afterwards is listed here and is not carried until
         * the next reconnect. An empty [routedAtConnect] means the Activity
         * was recreated while the VPN kept running, so what it started with is
         * not known here — claiming a mismatch against nothing made the hint
         * permanent.
         */
        private const val KEY_REMEMBERED_MESH = "remembered_mesh_routes"

        /**
         * The mesh as last seen, so Connect can route it.
         *
         * The FFI reports an empty peer directory once the proxy stops, so
         * deriving at the moment Connect is pressed yields only the overlay.
         * Following the mesh would route nothing any Peer publishes unless
         * what was last seen survives the disconnect.
         */
        fun writeRememberedMeshRoutes(prefs: SharedPreferences, routes: Set<String>) {
            prefs.edit().putStringSet(KEY_REMEMBERED_MESH, routes).apply()
        }

        fun readRememberedMeshRoutes(prefs: SharedPreferences): Set<String> =
            prefs.getStringSet(KEY_REMEMBERED_MESH, emptySet()) ?: emptySet()

        /** A Peer that has appeared since is added, not swapped in. */
        fun mergedMeshRoutes(remembered: Set<String>, derived: List<String>): List<String> =
            (remembered + derived).sorted()

        private const val KEY_ROUTED_AT_CONNECT = "routed_at_connect"

        /** Survives the Activity, which the running VPN does too. */
        fun writeRoutedAtConnect(prefs: SharedPreferences, routes: Set<String>) {
            prefs.edit().putStringSet(KEY_ROUTED_AT_CONNECT, routes).apply()
        }

        fun readRoutedAtConnect(prefs: SharedPreferences): Set<String> =
            prefs.getStringSet(KEY_ROUTED_AT_CONNECT, emptySet()) ?: emptySet()

        fun derivedRoutesSummary(
            following: Boolean,
            derived: List<String>,
            running: Boolean,
            routedAtConnect: Set<String>,
        ): String = when {
            !following -> "Turned off \u2014 only the networks below are used."
            derived.isEmpty() -> "No device is publishing a network yet."
            running && routedAtConnect.isNotEmpty() && derived.toSet() != routedAtConnect ->
                derived.joinToString(", ") + " \u2014 reconnect to route the newest"
            else -> derived.joinToString(", ")
        }

        fun validateLanRoutes(routes: List<String>): Pair<Boolean, String> {
            val normalizedRoutes = routes.map { normalizeRoute(it) }.filter { it.isNotBlank() }
            if (normalizedRoutes.isEmpty()) {
                return false to "At least one LAN route is required"
            }
            // No cap. The list is derived from what Peers publish, so a cap
            // would let a large Tunnel refuse a connection over a list its
            // owner never wrote.
            val seen = mutableSetOf<String>()
            for (route in normalizedRoutes) {
                val normalized = normalizeRoute(route)
                if (!isPrivateOrLinkLocalIpv4Cidr(normalized)) {
                    return false to "Route must be a private IPv4 LAN or link-local CIDR: $route"
                }
                if (!seen.add(normalized)) {
                    return false to "Duplicate route: $normalized"
                }
            }
            return true to ""
        }

        fun isPrivateOrLinkLocalIpv4Cidr(route: String): Boolean {
            val parsed = parseCidr(route) ?: return false
            if (parsed.prefix !in 1..32) {
                return false
            }
            val mask = if (parsed.prefix == 0) 0L else (-1L shl (32 - parsed.prefix)) and 0xffffffffL
            val network = parsed.address and mask
            val broadcast = network or (mask.inv() and 0xffffffffL)
            return isRangeInCidr(network, broadcast, ipv4ToLong(10, 0, 0, 0), 8) ||
                isRangeInCidr(network, broadcast, ipv4ToLong(172, 16, 0, 0), 12) ||
                isRangeInCidr(network, broadcast, ipv4ToLong(192, 168, 0, 0), 16) ||
                isRangeInCidr(network, broadcast, ipv4ToLong(169, 254, 0, 0), 16) ||
                // Peer addresses live here. Refusing it made every Connect fail
                // on a route the owner never typed.
                isRangeInCidr(network, broadcast, ipv4ToLong(198, 18, 0, 0), 15)
        }

        fun normalizeRoute(route: String): String {
            return route.trim()
        }

        /**
         * A stored list, with no default.
         *
         * routesFromJson falls back to DEFAULT_LAN_ROUTES, which is right for
         * the route list it was written for and wrong for every other list.
         * Reading the exports through it published the overlay prefix, and
         * reading the access rules through it produced "198.18.0.0/16" — not a
         * rule, so parsing threw on the launch path and the app died before
         * drawing anything.
         */
        fun listFromJson(raw: String?): List<String> {
            if (raw.isNullOrBlank()) return emptyList()
            return runCatching {
                val json = JSONArray(raw)
                List(json.length()) { index -> json.getString(index).trim() }
                    .filter { it.isNotBlank() }
            }.getOrDefault(emptyList())
        }

        private fun routesFromJson(raw: String?): List<String> {
            if (raw.isNullOrBlank()) {
                return DEFAULT_LAN_ROUTES
            }
            return runCatching {
                val json = JSONArray(raw)
                List(json.length()) { index -> json.getString(index).trim() }
                    .filter { it.isNotBlank() }
                    
                    .ifEmpty { DEFAULT_LAN_ROUTES }
            }.getOrDefault(DEFAULT_LAN_ROUTES)
        }

        private fun parseCidr(route: String): Cidr? {
            val value = route.trim()
            val slash = value.indexOf('/')
            if (slash <= 0 || slash == value.lastIndex || slash != value.lastIndexOf('/')) {
                return null
            }
            val prefix = value.substring(slash + 1).toIntOrNull() ?: return null
            val octets = value.substring(0, slash).split('.')
            if (octets.size != 4) {
                return null
            }
            val parsed = octets.map { part ->
                if (part.isEmpty()) return null
                part.toIntOrNull() ?: return null
            }
            if (parsed.any { it !in 0..255 }) {
                return null
            }
            return Cidr(ipv4ToLong(parsed[0], parsed[1], parsed[2], parsed[3]), prefix)
        }

        private fun isInCidr(address: Long, network: Long, prefix: Int): Boolean {
            val mask = if (prefix == 0) 0L else (-1L shl (32 - prefix)) and 0xffffffffL
            return (address and mask) == (network and mask)
        }

        private fun isRangeInCidr(start: Long, end: Long, network: Long, prefix: Int): Boolean {
            return isInCidr(start, network, prefix) && isInCidr(end, network, prefix)
        }

        private fun ipv4ToLong(a: Int, b: Int, c: Int, d: Int): Long {
            return ((a.toLong() shl 24) or (b.toLong() shl 16) or (c.toLong() shl 8) or d.toLong()) and
                0xffffffffL
        }

        private data class Cidr(val address: Long, val prefix: Int)
    }
}
