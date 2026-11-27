package dev.tinyrtc

import java.nio.ByteBuffer
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * What a decoder is configured from and what a keyframe must carry. The parameter sets are x264's
 * (emulation prevention bytes inside); access units are framed as the JNI shim frames them.
 */
class H264Test {
    private fun hex(s: String) = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }

    private val sps720 = hex("6742c01fd9005005bb0110000003001000000303c0f1832480") // Baseline 1280x720
    private val sps360 = hex("6742c01ed900a02ff970110000030001000003003c0f162e48") // Baseline 640x368, cropped to 360
    private val sps1080 = hex("67640028acd940780227e5c044000003000400000300f03c60c658") // High 1920x1088, cropped to 1080
    private val pps = hex("68cb83cb20")
    private val idr = byteArrayOf(0x65, 0x88.toByte(), 0x84.toByte(), 0x21)

    private fun au(vararg units: ByteArray) = units.fold(ByteArray(0)) { a, u -> a + H264.START_CODE + u }

    private fun direct(bytes: ByteArray, at: Int): ByteBuffer =
        ByteBuffer.allocateDirect(at + bytes.size).apply {
            position(at)
            put(bytes)
        }

    @Test
    fun theSizeAnSpsDeclares() {
        assertEquals(1280 to 720, H264.spsSize(sps720))
        assertEquals(640 to 360, H264.spsSize(sps360))
        assertEquals(1920 to 1080, H264.spsSize(sps1080))
        assertNull(H264.spsSize(sps720.copyOf(5)))
    }

    @Test
    fun theParameterSetsOfAnAccessUnit() {
        val (sps, ppsFound) = H264.parameterSets(au(sps720, pps, idr))
        assertArrayEquals(sps720, sps)
        assertArrayEquals(pps, ppsFound)
        val (none, alsoNone) = H264.parameterSets(au(idr))
        assertNull(none)
        assertNull(alsoNone)
    }

    @Test
    fun aKeyframeCarriesItsParameterSets() {
        val key = au(sps720, pps, idr)
        assertTrue(H264.hasParameterSets(direct(key, 7), 7, key.size))
        val bare = au(idr)
        assertFalse(H264.hasParameterSets(direct(bare, 3), 3, bare.size))
        val spsOnly = au(sps720, idr)
        assertFalse(H264.hasParameterSets(direct(spsOnly, 0), 0, spsOnly.size))
    }
}
