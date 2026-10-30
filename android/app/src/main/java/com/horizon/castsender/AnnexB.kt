package com.horizon.castsender

import java.io.ByteArrayOutputStream

/** NAL helpers shared by the encoder; kept free of Android types so JVM tests can run. */
object AnnexB {
    fun hasStartCode(bytes: ByteArray, offset: Int): Boolean =
        offset + 3 < bytes.size &&
            bytes[offset] == 0.toByte() &&
            bytes[offset + 1] == 0.toByte() &&
            (bytes[offset + 2] == 1.toByte() ||
                (bytes[offset + 2] == 0.toByte() && bytes[offset + 3] == 1.toByte()))

    fun withStartCode(nal: ByteArray): ByteArray =
        if (hasStartCode(nal, 0)) nal else byteArrayOf(0, 0, 0, 1) + nal

    /**
     * MediaCodec AVC output is normally Annex B already; some OMX encoders emit
     * 4-byte length-prefixed NAL units instead. Mirrors `toAnnexB` in
     * `extension/offscreen.js`.
     */
    fun toAnnexB(raw: ByteArray): ByteArray {
        if (hasStartCode(raw, 0)) return raw
        val out = ByteArrayOutputStream(raw.size + 16)
        var offset = 0
        while (offset + 4 <= raw.size) {
            val length = ((raw[offset].toInt() and 0xFF) shl 24) or
                ((raw[offset + 1].toInt() and 0xFF) shl 16) or
                ((raw[offset + 2].toInt() and 0xFF) shl 8) or
                (raw[offset + 3].toInt() and 0xFF)
            if (length <= 0 || offset + 4 + length > raw.size) return raw
            out.write(0)
            out.write(0)
            out.write(0)
            out.write(1)
            out.write(raw, offset + 4, length)
            offset += 4 + length
        }
        return if (out.size() == 0) raw else out.toByteArray()
    }
}
