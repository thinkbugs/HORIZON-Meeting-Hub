package com.horizon.castsender

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.IOException
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * Live check against a real HORIZON receiver, skipped unless a token (and a
 * canned Annex B clip) are supplied:
 *
 * ```
 * gradle testDebugUnitTest -PhorizonToken=504282 \
 *   -PhorizonH264=/tmp/horizon-test.h264
 * ```
 *
 * It exercises the same Wire/WsClient code the APK ships, so the only Android
 * pieces left untested here are MediaProjection and MediaCodec themselves.
 */
class ReceiverLiveTest {
    private fun token() = System.getProperty("horizonToken").orEmpty()
    private fun clip() = System.getProperty("horizonH264").orEmpty()
    private fun host() = System.getProperty("horizonHost") ?: "127.0.0.1"
    private fun port() = (System.getProperty("horizonPort") ?: "9407").toInt()

    @Test
    fun streamsAnnexBClipIntoTheRealReceiver() {
        assumeTrue("set -PhorizonToken and -PhorizonH264 to run", token().length == 6 && clip().isNotEmpty())
        val units = splitAccessUnits(File(clip()).readBytes())
        assumeTrue("clip has no access units", units.isNotEmpty())
        val (width, height) = (System.getProperty("horizonSize") ?: "1280x720")
            .split("x").map(String::toInt)

        val welcome = CountDownLatch(1)
        val closed = AtomicReference<String?>(null)
        lateinit var client: WsClient
        client = WsClient(host(), port(), "/stream?token=${token()}", object : WsClient.Listener {
            override fun onOpen() {
                client.sendText("""{"type":"hello"}""")
            }

            override fun onText(text: String) {
                if (text.contains("welcome")) welcome.countDown()
            }

            override fun onClosed(reason: String) {
                closed.set(reason)
            }
        })

        client.connect()
        assertTrue("receiver never said welcome", welcome.await(5, TimeUnit.SECONDS))

        var sent = 0
        var keyframes = 0
        val start = System.currentTimeMillis()
        for ((index, unit) in units.withIndex()) {
            val target = start + index * FRAME_MS
            val slack = target - System.currentTimeMillis()
            if (slack > 0) Thread.sleep(slack)
            val flags = (if (unit.key) 1 else 0) or 2
            client.sendBinary(Wire.packet(Wire.KIND_VIDEO, flags, System.currentTimeMillis() * 1000, width, height, unit.payload))
            sent += 1
            if (unit.key) keyframes += 1
            assertNull("receiver closed after $sent frames: ${closed.get()}", closed.get())
        }
        Thread.sleep(500)
        client.close()

        assertEquals(units.size, sent)
        assertTrue("clip should carry at least one IDR", keyframes >= 1)
        System.out.println("live stream sent: $sent frames ($keyframes keyframes) to ${host()}:${port()}")
    }

    @Test
    fun wrongTokenIsRefusedByTheRealReceiver() {
        assumeTrue("set -PhorizonToken to run", token().length == 6)
        val client = WsClient(host(), port(), "/stream?token=000000", object : WsClient.Listener {
            override fun onOpen() = Unit
            override fun onText(text: String) = Unit
            override fun onClosed(reason: String) = Unit
        })
        try {
            client.connect()
            throw AssertionError("a wrong token must not open a session")
        } catch (error: IOException) {
            assertEquals("投屏码不正确", error.message)
        }
    }

    private data class Unit(val payload: ByteArray, val key: Boolean)

    private fun splitAccessUnits(data: ByteArray): List<Unit> {
        val nals = mutableListOf<ByteArray>()
        var start = -1
        var index = 0
        while (index + 3 <= data.size) {
            val length = when {
                data[index] == 0.toByte() && data[index + 1] == 0.toByte() && data[index + 2] == 1.toByte() -> 3
                index + 4 <= data.size && data[index] == 0.toByte() && data[index + 1] == 0.toByte() &&
                    data[index + 2] == 0.toByte() && data[index + 3] == 1.toByte() -> 4
                else -> 0
            }
            if (length == 0) {
                index += 1
                continue
            }
            if (start >= 0) nals.add(data.copyOfRange(start, index))
            start = index
            index += length
        }
        if (start >= 0) nals.add(data.copyOfRange(start, data.size))

        val units = mutableListOf<Unit>()
        var current = ByteArrayOutputStream()
        var key = false
        for (nal in nals) {
            val type = nal[(if (nal[2] == 1.toByte()) 3 else 4)].toInt() and 0x1F
            if (type == 9 && current.size() > 0) {
                units.add(Unit(current.toByteArray(), key))
                current = ByteArrayOutputStream()
                key = false
            }
            if (type == 5) key = true
            current.write(nal)
        }
        if (current.size() > 0) units.add(Unit(current.toByteArray(), key))
        return units
    }

    private companion object {
        const val FRAME_MS = 33L
    }
}
