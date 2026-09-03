package com.buhuipao.tunnelproxy

import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The routes the app derives must be routes the app accepts.
 *
 * These two halves were tested separately and never together, so a derived list
 * that the validator refuses shipped green: every Connect failed with a message
 * about a route the owner never typed. This is the composition that was missing.
 */
class ConnectPathTest {
    @Test
    fun aFreshInstallProducesRoutesItsOwnValidatorAccepts() {
        // Nothing has connected, so nothing is published yet.
        val routes = MobileConfig.routesFromExports("{}")

        val (valid, reason) = MobileConfig.validateLanRoutes(routes)
        assertTrue("a first connection must be possible: $reason", valid)
    }

    @Test
    fun routesLearnedFromTheMeshAreAcceptedToo() {
        val status = """
            {"peer_directory":{"peers":[
              {"peer_id":"a","exports":[{"prefix":"192.168.7.0/24"}]},
              {"peer_id":"b","exports":[{"prefix":"10.20.0.0/16"}]}
            ]}}
        """.trimIndent()

        val (valid, reason) = MobileConfig.validateLanRoutes(MobileConfig.routesFromExports(status))
        assertTrue("published prefixes must be routable: $reason", valid)
    }

    @Test
    fun aMeshLargerThanTheRouteCapStillConnects() {
        // The owner did not type this list, so it must not be able to refuse
        // their connection by being too long.
        val peers = (1..12).joinToString(",") { """{"peer_id":"p$it","exports":[{"prefix":"10.$it.0.0/16"}]}""" }
        val status = """{"peer_directory":{"peers":[$peers]}}"""

        val routes = MobileConfig.routesFromExports(status)
        val (valid, reason) = MobileConfig.validateLanRoutes(routes)
        assertTrue("a large mesh must not block Connect: $reason", valid)
    }

    @Test
    fun theDefaultRoutesAreAcceptedAsWell() {
        val (valid, reason) = MobileConfig.validateLanRoutes(MobileConfig.DEFAULT_LAN_ROUTES)
        assertTrue("the built-in default must be usable: $reason", valid)
    }
}
