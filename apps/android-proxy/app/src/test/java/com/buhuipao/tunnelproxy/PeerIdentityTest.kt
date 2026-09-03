package com.buhuipao.tunnelproxy

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * What the app is allowed to show about an imported profile.
 *
 * The file carries `peer_private_key`, so putting it on screen — which is what
 * the app used to do, in an editable box — is a plaintext private key one
 * screenshot away from leaving the device. Identity is three public facts, and
 * that is all this returns.
 */
class PeerIdentityTest {
    private fun profile(): String = JSONObject()
        .put("version", 2)
        .put("tunnel_id", "831cf706-7d9b-4576-9d2b-2f86213e38f0")
        .put(
            "peer",
            JSONObject()
                .put("peer_id", "b7c1e5d2-0000-4000-8000-000000000001")
                .put("overlay_ip", "198.18.0.5")
                .put("peer_private_key", "SECRET-MUST-NOT-LEAK"),
        )
        .toString()

    @Test
    fun readsTheThreePublicFacts() {
        val identity = MobileConfig.peerIdentity(profile())!!

        assertEquals("831cf706-7d9b-4576-9d2b-2f86213e38f0", identity.tunnelId)
        assertEquals("b7c1e5d2-0000-4000-8000-000000000001", identity.peerId)
        assertEquals("198.18.0.5", identity.overlayIp)
    }

    @Test
    fun carriesNothingSecret() {
        val identity = MobileConfig.peerIdentity(profile())!!

        val rendered = listOf(identity.tunnelId, identity.peerId, identity.overlayIp, identity.shortPeerId)
        assertEquals(
            "no field may carry key material",
            emptyList<String>(),
            rendered.filter { it.contains("SECRET") },
        )
    }

    @Test
    fun shortensThePeerIdForDisplay() {
        assertEquals("…00000001", MobileConfig.peerIdentity(profile())!!.shortPeerId)
    }

    @Test
    fun anAbsentOrUnreadableProfileHasNoIdentity() {
        assertNull(MobileConfig.peerIdentity(""))
        assertNull(MobileConfig.peerIdentity("not json"))
    }

    @Test
    fun anOverlayIpIsOptional() {
        val withoutOverlay = JSONObject()
            .put("version", 2)
            .put("tunnel_id", "t")
            .put("peer", JSONObject().put("peer_id", "p"))
            .toString()

        assertEquals("", MobileConfig.peerIdentity(withoutOverlay)!!.overlayIp)
    }
}
