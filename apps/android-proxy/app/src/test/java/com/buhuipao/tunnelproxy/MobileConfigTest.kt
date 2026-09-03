package com.buhuipao.tunnelproxy

import android.content.SharedPreferences
import android.content.ContextWrapper
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class MobileConfigTest {
    @Test
    fun saveConfigRoundTripsFreshBlankPeerProfile() {
        val preferences = InMemorySharedPreferences()
        val context = object : ContextWrapper(null) {
            override fun getSharedPreferences(name: String, mode: Int): SharedPreferences = preferences
        }

        MobileConfig(deviceId = "device-1").saveConfig(preferences)

        assertEquals("", MobileConfig.fromPreferences(context).peerProfileJson)
    }

    @Test
    fun saveConfigRejectsNonEmptyInvalidPeerProfile() {
        val preferences = InMemorySharedPreferences()

        assertThrows(IllegalArgumentException::class.java) {
            MobileConfig(peerProfileJson = "{}", deviceId = "device-1").saveConfig(preferences)
        }
    }

    @Test
    fun freshConfigAcceptsASecondPrivateLanRoute() {
        val result = MobileConfig.validateLanRoutes(
            listOf("192.168.0.0/16", "10.0.0.0/8"),
        )

        assertTrue(result.second, result.first)
    }

    @Test
    fun buildStartJsonStillRejectsBlankPeerProfile() {
        assertThrows(RuntimeException::class.java) {
            MobileConfig(deviceId = "device-1").buildStartJson()
        }
    }

    @Test
    fun buildStartJsonUsesOnlyPeerProfileForTunnelIdentity() {
        val config = MobileConfig(
            peerProfileJson = """{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}""",
            deviceId = "device-1",
            localSocks5Listen = "127.0.0.1:1080",
        )

        val json = JSONObject(config.buildStartJson())

        assertEquals(
            setOf(
                "client_access",
                "device_id",
                "exported_lans",
                "local_socks5_listen",
                "p2p_allow_lan_candidates",
                "peer_profile",
                "tunnel_first",
            ),
            json.keys().asSequence().toSet(),
        )
        assertEquals("tid-1", json.getJSONObject("peer_profile").getString("tunnel_id"))
        assertFalse(json.has("tunnel_id"))
        assertFalse(json.has("tunnel_key"))
        assertFalse(json.has("platform_url"))
        assertFalse(json.has("local_proxy_auth_enabled"))
        assertFalse(json.has("p2p_enabled"))
        assertFalse(json.has("peer_client_id"))
    }

    internal class InMemorySharedPreferences : SharedPreferences {
        private val values = mutableMapOf<String, Any?>()

        override fun getAll(): MutableMap<String, *> = values.toMutableMap()

        override fun getString(key: String?, defaultValue: String?): String? =
            values[key] as? String ?: defaultValue

        override fun getStringSet(key: String?, defaultValues: MutableSet<String>?): MutableSet<String>? =
            @Suppress("UNCHECKED_CAST")
            ((values[key] as? Set<String>)?.toMutableSet() ?: defaultValues)

        override fun getInt(key: String?, defaultValue: Int): Int = values[key] as? Int ?: defaultValue

        override fun getLong(key: String?, defaultValue: Long): Long = values[key] as? Long ?: defaultValue

        override fun getFloat(key: String?, defaultValue: Float): Float = values[key] as? Float ?: defaultValue

        override fun getBoolean(key: String?, defaultValue: Boolean): Boolean =
            values[key] as? Boolean ?: defaultValue

        override fun contains(key: String?): Boolean = values.containsKey(key)

        override fun edit(): SharedPreferences.Editor = Editor(values)

        override fun registerOnSharedPreferenceChangeListener(listener: SharedPreferences.OnSharedPreferenceChangeListener?) = Unit

        override fun unregisterOnSharedPreferenceChangeListener(listener: SharedPreferences.OnSharedPreferenceChangeListener?) = Unit

        private class Editor(private val values: MutableMap<String, Any?>) : SharedPreferences.Editor {
            private val pending = mutableMapOf<String, Any?>()
            private val removals = mutableSetOf<String>()
            private var clear = false

            override fun putString(key: String?, value: String?): SharedPreferences.Editor = put(key, value)

            override fun putStringSet(key: String?, values: MutableSet<String>?): SharedPreferences.Editor =
                put(key, values?.toSet())

            override fun putInt(key: String?, value: Int): SharedPreferences.Editor = put(key, value)

            override fun putLong(key: String?, value: Long): SharedPreferences.Editor = put(key, value)

            override fun putFloat(key: String?, value: Float): SharedPreferences.Editor = put(key, value)

            override fun putBoolean(key: String?, value: Boolean): SharedPreferences.Editor = put(key, value)

            override fun remove(key: String?): SharedPreferences.Editor = apply {
                key?.let(removals::add)
            }

            override fun clear(): SharedPreferences.Editor = apply { clear = true }

            override fun commit(): Boolean {
                apply()
                return true
            }

            override fun apply() {
                if (clear) values.clear()
                removals.forEach(values::remove)
                values.putAll(pending)
            }

            private fun put(key: String?, value: Any?): SharedPreferences.Editor = apply {
                requireNotNull(key)
                pending[key] = value
            }
        }
    }

    /**
     * A phone is a Peer like any other: it can publish a network.
     *
     * The start JSON carried no exports and no tunnel-first flag, so the
     * runtime installed no local record and the phone shared nothing whatever
     * its owner configured.
     */
    @Test
    fun startJsonCarriesExportsAndTunnelFirst() {
        val config = MobileConfig(
            peerProfileJson = """{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}""",
            deviceId = "device-1",
            exportedLans = listOf("192.168.7.0/24"),
            tunnelFirst = true,
        )

        val json = JSONObject(config.buildStartJson())

        assertEquals("192.168.7.0/24", json.getJSONArray("exported_lans").getString(0))
        assertTrue(json.getBoolean("tunnel_first"))
    }

    @Test
    fun startJsonPublishesNothingByDefault() {
        val config = MobileConfig(peerProfileJson = """{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}""", deviceId = "device-1")

        val json = JSONObject(config.buildStartJson())

        assertEquals(0, json.getJSONArray("exported_lans").length())
        assertFalse(json.getBoolean("tunnel_first"))
    }

    /**
     * The desktop reconnects the last Peer profile on launch; a phone did not,
     * so the same Tunnel behaved differently depending on which device you
     * picked it up from.
     */
    @Test
    fun autoConnectSurvivesARoundTrip() {
        val prefs = InMemorySharedPreferences()
        MobileConfig(
            peerProfileJson = """{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}""",
            deviceId = "device-1",
            autoConnect = true,
        ).saveConfig(prefs)

        assertTrue(MobileConfig.readAutoConnect(prefs))
    }

    @Test
    fun autoConnectIsOffUntilItIsAskedFor() {
        assertFalse(MobileConfig().autoConnect)
    }

    /**
     * A fresh install publishes nothing and restricts nothing.
     *
     * routesFromJson falls back to DEFAULT_LAN_ROUTES when the key is absent —
     * correct for the route list it was written for, wrong for every other
     * list read through it. Exports came back as the overlay prefix, and the
     * access rules came back as "198.18.0.0/16", which is not a rule: parsing
     * it threw on the launch path and the app died before drawing anything.
     */
    @Test
    fun anAbsentListIsEmptyRatherThanTheDefaultRoutes() {
        // The route list has a meaningful default; nothing else read through
        // this helper does. Exports came back as the overlay prefix, and the
        // access rules as "198.18.0.0/16" — not a rule, so parsing it threw on
        // the launch path and the app died before drawing anything.
        assertTrue(MobileConfig.listFromJson(null).isEmpty())
        assertTrue(MobileConfig.listFromJson("").isEmpty())
        assertEquals(listOf("a", "b"), MobileConfig.listFromJson("""["a","b"]"""))
    }

    @Test
    fun aFreshInstallStartsWithoutThrowing() {
        val config = MobileConfig(
            peerProfileJson = """{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}""",
            deviceId = "device-1",
        )

        val json = JSONObject(config.buildStartJson())

        assertEquals(0, json.getJSONArray("exported_lans").length())
        assertEquals(0, json.getJSONObject("client_access").getJSONArray("deny").length())
    }
}
