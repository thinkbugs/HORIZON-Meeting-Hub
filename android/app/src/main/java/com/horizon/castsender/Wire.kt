package com.horizon.castsender

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The 24-byte frame header the receiver parses. Mirrors
 * `crates/horizon-cast/src/browser.rs`:
 *
 * ```
 * 0  u8  kind (1 H.264 Annex B, 2 PCM s16le)
 * 1  u8  flags (bit 0: keyframe, bit 1: pts is sender wall clock)
 * 2  u16 reserved
 * 4  u64 pts_us
 * 12 u16 width / sample rate / 100
 * 14 u16 height / channels
 * 16 u32 payload length
 * 20 4 bytes reserved
 * ```
 */
object Wire {
    const val KIND_VIDEO = 1
    const val KIND_AUDIO = 2

    fun packet(
        kind: Int,
        flags: Int,
        ptsUs: Long,
        a: Int,
        b: Int,
        payload: ByteArray,
    ): ByteArray {
        val out = ByteArray(24 + payload.size)
        val buffer = ByteBuffer.wrap(out).order(ByteOrder.LITTLE_ENDIAN)
        buffer.put(0, kind.toByte())
        buffer.put(1, flags.toByte())
        buffer.putShort(2, 0)
        buffer.putLong(4, ptsUs)
        buffer.putShort(12, a.toShort())
        buffer.putShort(14, b.toShort())
        buffer.putInt(16, payload.size)
        buffer.putInt(20, 0)
        System.arraycopy(payload, 0, out, 24, payload.size)
        return out
    }
}
