package com.horizon.castsender

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

class WireTest {
    @Test
    fun headerLayoutMatchesReceiverParser() {
        val payload = byteArrayOf(0x67, 0x42, 0x00, 0x1F)
        val packet = Wire.packet(
            kind = Wire.KIND_VIDEO,
            flags = 0b11,
            ptsUs = 0x0102030405060708L,
            a = 1920,
            b = 1080,
            payload = payload,
        )

        assertEquals(24 + payload.size, packet.size)
        assertEquals(1, packet[0].toInt())            // kind video
        assertEquals(3, packet[1].toInt())            // keyframe + wall clock
        assertEquals(0, packet[2].toInt())            // reserved u16
        assertEquals(0, packet[3].toInt())
        // pts_us little endian
        assertEquals(0x08, packet[4].toInt())
        assertEquals(0x07, packet[5].toInt())
        assertEquals(0x01, packet[11].toInt())
        // 1920 = 0x0780 -> 80 07
        assertEquals(0x80, packet[12].toInt() and 0xFF)
        assertEquals(0x07, packet[13].toInt())
        // 1080 = 0x0438 -> 38 04
        assertEquals(0x38, packet[14].toInt())
        assertEquals(0x04, packet[15].toInt())
        // payload length u32 little endian
        assertEquals(4, packet[16].toInt())
        assertEquals(0, packet[19].toInt())
        assertArrayEquals(payload, packet.copyOfRange(24, packet.size))
    }

    @Test
    fun audioPacketUsesSampleRateAndChannels() {
        val packet = Wire.packet(Wire.KIND_AUDIO, 0, 0, 480, 2, ByteArray(960))
        assertEquals(2, packet[0].toInt())
        assertEquals(480, (packet[12].toInt() and 0xFF) or (packet[13].toInt() shl 8))
        assertEquals(2, (packet[14].toInt() and 0xFF) or (packet[15].toInt() shl 8))
        assertEquals(960, (packet[16].toInt() and 0xFF) or ((packet[17].toInt() and 0xFF) shl 8))
    }
}
