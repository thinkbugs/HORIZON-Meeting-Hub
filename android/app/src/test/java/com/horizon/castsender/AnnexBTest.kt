package com.horizon.castsender

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

class AnnexBTest {
    @Test
    fun annexBPayloadPassesThrough() {
        val raw = byteArrayOf(0, 0, 0, 1, 0x65, 0x01, 0x02)
        assertArrayEquals(raw, AnnexB.toAnnexB(raw))
    }

    @Test
    fun threeByteStartCodeIsRecognised() {
        val raw = byteArrayOf(0, 0, 1, 0x67, 0x42)
        assertArrayEquals(raw, AnnexB.toAnnexB(raw))
    }

    @Test
    fun lengthPrefixedNalsBecomeAnnexB() {
        // Two NAL units: [8 65 AA], [2 41]
        val raw = byteArrayOf(
            0, 0, 0, 2, 0x65, 0xAA.toByte(),
            0, 0, 0, 1, 0x41,
        )
        val expected = byteArrayOf(
            0, 0, 0, 1, 0x65, 0xAA.toByte(),
            0, 0, 0, 1, 0x41,
        )
        assertArrayEquals(expected, AnnexB.toAnnexB(raw))
    }

    @Test
    fun truncatedLengthPrefixIsLeftAlone() {
        val raw = byteArrayOf(0, 0, 0, 9, 0x65)
        assertArrayEquals(raw, AnnexB.toAnnexB(raw))
    }

    @Test
    fun parameterSetGetsStartCode() {
        val sps = byteArrayOf(0x67, 0x42, 0x00)
        val wrapped = AnnexB.withStartCode(sps)
        assertEquals(7, wrapped.size)
        assertArrayEquals(byteArrayOf(0, 0, 0, 1, 0x67, 0x42, 0x00), wrapped)
        assertArrayEquals(wrapped, AnnexB.withStartCode(wrapped))
    }
}
