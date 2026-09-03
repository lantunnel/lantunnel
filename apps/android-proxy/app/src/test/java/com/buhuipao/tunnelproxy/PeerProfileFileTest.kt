package com.buhuipao.tunnelproxy

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * A `.peer` file arrives as bytes from a document picker or a share sheet, and
 * whatever produced it is out of our hands: a Windows export carries a UTF-8
 * BOM and CRLF, a mistapped file could be a gigabyte of anything. Decoding is
 * therefore its own step with its own rules, separate from profile validation.
 */
class PeerProfileFileTest {
    private fun profileJson(): String = JSONObject()
        .put("version", 2)
        .put("tunnel_id", "831cf706-7d9b-4576-9d2b-2f86213e38f0")
        .put("peer", JSONObject().put("peer_id", "peer-1"))
        .toString()

    @Test
    fun decodesAPlainUtf8Profile() {
        val decoded = MobileConfig.decodePeerProfileFile(profileJson().toByteArray(Charsets.UTF_8))

        assertEquals(2, JSONObject(decoded).getInt("version"))
    }

    @Test
    fun stripsAUtf8ByteOrderMark() {
        val bom = byteArrayOf(0xEF.toByte(), 0xBB.toByte(), 0xBF.toByte())
        val decoded = MobileConfig.decodePeerProfileFile(bom + profileJson().toByteArray(Charsets.UTF_8))

        assertEquals(2, JSONObject(decoded).getInt("version"))
    }

    @Test
    fun toleratesSurroundingWhitespaceAndCrlf() {
        val padded = "\r\n  " + profileJson() + "  \r\n"
        val decoded = MobileConfig.decodePeerProfileFile(padded.toByteArray(Charsets.UTF_8))

        assertEquals(2, JSONObject(decoded).getInt("version"))
    }

    @Test
    fun refusesAnEmptyFile() {
        val error = assertThrows(IllegalArgumentException::class.java) {
            MobileConfig.decodePeerProfileFile(ByteArray(0))
        }
        assertTrue(error.message!!.contains("empty"))
    }

    @Test
    fun refusesAFileTooLargeToBeAProfile() {
        val oversized = ByteArray(MobileConfig.MAX_PEER_PROFILE_BYTES + 1) { '{'.code.toByte() }

        val error = assertThrows(IllegalArgumentException::class.java) {
            MobileConfig.decodePeerProfileFile(oversized)
        }
        assertTrue(error.message!!.contains("too large"))
    }

    @Test
    fun refusesSomethingThatIsNotAV2Profile() {
        val notAProfile = JSONObject().put("version", 1).put("tunnel_id", "t").toString()

        assertThrows(IllegalArgumentException::class.java) {
            MobileConfig.decodePeerProfileFile(notAProfile.toByteArray(Charsets.UTF_8))
        }
    }
}
