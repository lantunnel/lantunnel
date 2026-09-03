package com.buhuipao.tunnelproxy

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Whether other devices can reach this phone.
 *
 * The mobile runtime never sent a policy, so it ran on the engine's
 * refuse-everything placeholder: a phone could reach a laptop and never the
 * other way round, with nothing in the app saying so. The start request now
 * carries the answer, and the default is the same as the desktop's — an empty
 * Allow list, which means every Peer in the Tunnel.
 */
class ClientAccessTest {
    private fun profile(): String = JSONObject()
        .put("version", 2)
        .put("tunnel_id", "t")
        .put("peer", JSONObject().put("peer_id", "p"))
        .toString()

    private fun startJson(config: MobileConfig): JSONObject =
        JSONObject(config.copy(peerProfileJson = profile(), deviceId = "d").buildStartJson())

    @Test
    fun anUnconfiguredPhoneIsReachableByItsTunnel() {
        val access = startJson(MobileConfig()).getJSONObject("client_access")

        assertEquals(0, access.getJSONArray("allow").length())
        assertEquals(0, access.getJSONArray("deny").length())
    }


    /**
     * A phone that publishes a network needs a way to say who may reach it.
     *
     * The policy was a hard-coded empty object, written when a phone could not
     * publish anything. Now that it can, an empty policy means every Peer in
     * the Tunnel reaches whatever it shares, with nothing on screen to change.
     */
    @Test
    fun blockingEverythingClosesBothFamiliesOnBothProtocols() {
        val config = MobileConfig(
            peerProfileJson = PROFILE,
            deviceId = "device-1",
            blockAllIncoming = true,
        )

        val deny = JSONObject(config.buildStartJson())
            .getJSONObject("client_access")
            .getJSONArray("deny")

        val targets = (0 until deny.length()).map {
            val rule = deny.getJSONObject(it)
            rule.getJSONObject("target").getString("value") to rule.getString("protocol")
        }.toSet()
        assertEquals(
            setOf(
                "0.0.0.0/0" to "tcp", "0.0.0.0/0" to "udp",
                "::/0" to "tcp", "::/0" to "udp",
            ),
            targets,
        )
    }

    @Test
    fun anOpenPhoneStillSendsAnEmptyPolicy() {
        val config = MobileConfig(peerProfileJson = PROFILE, deviceId = "device-1")

        val access = JSONObject(config.buildStartJson()).getJSONObject("client_access")

        assertEquals(0, access.getJSONArray("allow").length())
        assertEquals(0, access.getJSONArray("deny").length())
    }

    /**
     * Named rules, the way the desktop has them.
     *
     * A phone had one switch: everything or nothing. Now that it can publish a
     * network, "who may reach it" needs the same answer the desktop gives —
     * a target, a protocol, and a port.
     */
    @Test
    fun aNamedAllowRuleReachesThePolicy() {
        val config = MobileConfig(
            peerProfileJson = PROFILE,
            deviceId = "device-1",
            accessRules = listOf("allow 192.168.7.0/24 tcp 22"),
        )

        val allow = JSONObject(config.buildStartJson())
            .getJSONObject("client_access")
            .getJSONArray("allow")

        assertEquals(1, allow.length())
        val rule = allow.getJSONObject(0)
        assertEquals("cidr", rule.getJSONObject("target").getString("type"))
        assertEquals("192.168.7.0/24", rule.getJSONObject("target").getString("value"))
        assertEquals("tcp", rule.getString("protocol"))
        assertEquals(22, rule.getJSONObject("port").getInt("value"))
    }

    @Test
    fun aDenyRuleWithoutAPortMeansEveryPort() {
        val config = MobileConfig(
            peerProfileJson = PROFILE,
            deviceId = "device-1",
            accessRules = listOf("deny 10.0.0.5 udp"),
        )

        val deny = JSONObject(config.buildStartJson())
            .getJSONObject("client_access")
            .getJSONArray("deny")
        val rule = deny.getJSONObject(0)

        assertEquals("ip", rule.getJSONObject("target").getString("type"))
        assertEquals("any", rule.getJSONObject("port").getString("type"))
    }

    @Test
    fun anUnreadableLineIsNotSilentlyDropped() {
        val bad = MobileConfig(
            peerProfileJson = PROFILE,
            deviceId = "device-1",
            accessRules = listOf("this is not a rule"),
        )

        assertThrows(IllegalArgumentException::class.java) { bad.buildStartJson() }
    }

    /**
     * Connect ran buildStartJson from a click handler, so an unreadable line
     * took the app down rather than saying which line was wrong. The caller
     * needs to ask before it starts.
     */
    @Test
    fun anUnreadableLineCanBeFoundBeforeConnecting() {
        val bad = MobileConfig(
            peerProfileJson = PROFILE,
            deviceId = "device-1",
            accessRules = listOf("allow this_peer tcp", "198.18.0.0/16"),
        )

        assertEquals("198.18.0.0/16", bad.unreadableAccessRule())
    }

    @Test
    fun everyLineReadingMeansNothingToReport() {
        val good = MobileConfig(
            peerProfileJson = PROFILE,
            deviceId = "device-1",
            accessRules = listOf("allow this_peer tcp", "  ", "deny 10.0.0.0/8 udp 53"),
        )

        assertNull(good.unreadableAccessRule())
    }

    /**
     * The overlay prefix in the rules field is our own corruption, not a line
     * anyone typed: the field's grammar has always required allow or deny.
     * An upgrader must not be left hand-cleaning it out of a crashed app.
     */
    @Test
    fun theOverlayPrefixLeftByTheOldParserIsNotCarriedForward() {
        assertEquals(
            listOf("allow this_peer tcp"),
            MobileConfig.withoutCorruptedAccessRules(
                listOf("allow this_peer tcp", MobileConfig.OVERLAY_ROUTE),
            ),
        )
    }

    @Test
    fun aLineSomeoneTypedWrongIsStillTheirs() {
        assertEquals(
            listOf("allow this_peer tcp", "allwo this_peer tcp"),
            MobileConfig.withoutCorruptedAccessRules(
                listOf("allow this_peer tcp", "allwo this_peer tcp"),
            ),
        )
    }

    private companion object {
        const val PROFILE = """{"version":2,"tunnel_id":"tid-1","peer":{"peer_id":"peer-1"}}"""
    }
}