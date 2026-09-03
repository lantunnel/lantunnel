package com.buhuipao.tunnelproxy

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Which networks the phone sends through the tunnel.
 *
 * This used to be a list of CIDRs typed by hand, defaulting to 192.168.0.0/16 —
 * a range that also covers the Wi-Fi the phone is standing on, so the default
 * captured the local router and printer. The mesh already reports which
 * prefixes each Peer exports, which is the same question answered by the side
 * that knows.
 */
class DerivedRoutesTest {
    private fun status(vararg prefixes: String): String {
        val peers = prefixes.map { prefix ->
            JSONObject()
                .put("peer_id", "peer-$prefix")
                .put("exports", org.json.JSONArray(listOf(JSONObject().put("prefix", prefix))))
        }
        return JSONObject()
            .put("peer_directory", JSONObject().put("peers", org.json.JSONArray(peers)))
            .toString()
    }

    @Test
    fun alwaysRoutesTheOverlaySoPeersAreReachable() {
        // Peer addresses live in 198.18.0.0/16. Without it the tunnel is up and
        // no Peer can be reached, which is what a first connection looks like:
        // nothing is published yet because nothing has connected yet.
        assertEquals(
            listOf(MobileConfig.OVERLAY_ROUTE),
            MobileConfig.routesFromExports("{}"),
        )
    }

    @Test
    fun followsWhatPeersExport() {
        assertEquals(
            listOf("10.20.0.0/16", "192.168.7.0/24", MobileConfig.OVERLAY_ROUTE),
            MobileConfig.routesFromExports(status("192.168.7.0/24", "10.20.0.0/16")),
        )
    }

    @Test
    fun ignoresAnExportWithNoPrefix() {
        val blank = JSONObject()
            .put(
                "peer_directory",
                JSONObject().put(
                    "peers",
                    org.json.JSONArray(
                        listOf(
                            JSONObject().put("peer_id", "p").put(
                                "exports",
                                org.json.JSONArray(listOf(JSONObject().put("prefix", ""))),
                            ),
                        ),
                    ),
                ),
            )
            .toString()

        assertEquals(listOf(MobileConfig.OVERLAY_ROUTE), MobileConfig.routesFromExports(blank))
    }

    @Test
    fun doesNotRepeatAPrefixTwoPeersBothExport() {
        assertEquals(
            listOf("192.168.7.0/24", MobileConfig.OVERLAY_ROUTE),
            MobileConfig.routesFromExports(status("192.168.7.0/24", "192.168.7.0/24")),
        )
    }

    @Test
    fun anUnreadableStatusStillRoutesTheOverlay() {
        // A first connection has to be possible: the phone cannot learn what
        // Peers publish until it has joined them.
        assertEquals(listOf(MobileConfig.OVERLAY_ROUTE), MobileConfig.routesFromExports("not json"))
    }

    /// An unknown start set is not a mismatch.
    ///
    /// The Activity is recreated on rotation and after process death while the
    /// VPN keeps running, so what the tunnel started with is gone from memory.
    /// Comparing against an empty set made every derived list look stale.
    @Test
    fun anUnknownStartSetIsNotClaimedStale() {
        val summary = MobileConfig.derivedRoutesSummary(
            following = true,
            derived = listOf("198.18.0.0/16"),
            running = true,
            routedAtConnect = emptySet(),
        )

        assertFalse(summary.contains("reconnect"))
    }

    @Test
    fun aKnownStartSetThatDisagreesIsNamed() {
        val summary = MobileConfig.derivedRoutesSummary(
            following = true,
            derived = listOf("10.0.0.0/8", "198.18.0.0/16"),
            running = true,
            routedAtConnect = setOf("198.18.0.0/16"),
        )

        assertTrue(summary.contains("reconnect"))
    }

    @Test
    fun followingOffSaysSo() {
        val summary = MobileConfig.derivedRoutesSummary(
            following = false,
            derived = listOf("198.18.0.0/16"),
            running = false,
            routedAtConnect = emptySet(),
        )

        assertTrue(summary.lowercase().contains("off"))
    }

    /**
     * Connect happens while the runtime is stopped.
     *
     * The FFI reports an empty peer directory once the proxy stops, so
     * deriving at the moment Connect is pressed yields only the overlay —
     * following the mesh routed nothing any Peer publishes, which is the whole
     * feature. What was last seen has to survive the disconnect.
     */
    @Test
    fun connectMergesWhatIsRememberedWithWhatIsLive() {
        val merged = MobileConfig.mergedMeshRoutes(
            remembered = setOf("192.168.7.0/24", "198.18.0.0/16"),
            derived = listOf("10.2.0.0/16", "198.18.0.0/16"),
        )

        assertEquals(listOf("10.2.0.0/16", "192.168.7.0/24", "198.18.0.0/16"), merged)
    }

    @Test
    fun aStoppedRuntimeStillRoutesTheMeshItLastSaw() {
        val merged = MobileConfig.mergedMeshRoutes(
            remembered = setOf("192.168.7.0/24"),
            derived = MobileConfig.routesFromExports("{}"),
        )

        assertEquals(listOf("192.168.7.0/24", "198.18.0.0/16"), merged)
    }

    /**
     * Peer addresses live on the overlay, so it is not a preference.
     *
     * With following off it was an ordinary row in the manual list. Deleting
     * it and connecting left the tunnel carrying no route to any Peer address
     * — every Peer unreachable, with nothing on screen saying why.
     */
    @Test
    fun theOverlayIsCarriedEvenWhenTheManualListOmitsIt() {
        val routes = MobileConfig.withOverlay(listOf("192.168.1.0/24"))

        assertTrue(routes.contains(MobileConfig.OVERLAY_ROUTE))
        assertTrue(routes.contains("192.168.1.0/24"))
    }

    @Test
    fun theOverlayIsNotDuplicatedWhenAlreadyListed() {
        val routes = MobileConfig.withOverlay(listOf(MobileConfig.OVERLAY_ROUTE, "10.0.0.0/8"))

        assertEquals(1, routes.count { it == MobileConfig.OVERLAY_ROUTE })
    }

    @Test
    fun anEmptyManualListStillReachesPeers() {
        assertEquals(listOf(MobileConfig.OVERLAY_ROUTE), MobileConfig.withOverlay(emptyList()))
    }
}
