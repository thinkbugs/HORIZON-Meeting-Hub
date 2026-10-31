package com.horizon.castsender

import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.io.FileInputStream
import java.io.InputStream
import java.net.Socket
import java.util.concurrent.CopyOnWriteArrayList

/**
 * Windows Media Foundation fetches the media URL itself and asks for byte
 * ranges before it plays. These cases pin the HTTP contract: full GET, closed
 * range, suffix range, range past EOF, and HEAD.
 */
class MediaServerTest {
    private val bytes = ByteArray(1000) { (it % 251).toByte() }
    private val file = File.createTempFile("horizon-media", ".bin").apply { writeBytes(bytes) }
    private val server = MediaServer()
    private var port = -1

    private fun started(): String {
        if (port > 0) return "http://127.0.0.1:$port/media/test.bin"
        val url = server.start(
            MediaServer.Item("test.bin", "application/octet-stream", file.length()) { offset ->
                FileInputStream(file).apply { channel.position(offset) }
            },
        )
        requireNotNull(url)
        port = Regex(":(\\d+)/").find(url)!!.groupValues[1].toInt()
        return "http://127.0.0.1:$port/media/test.bin"
    }

    @After
    fun tearDown() {
        server.stop()
        file.delete()
    }

    @Test
    fun fullGetReturnsTheWholeFile() {
        started()
        val (head, body) = request("GET /media/test.bin HTTP/1.1\r\nHost: x\r\n\r\n")
        assertTrue(head.startsWith("HTTP/1.1 200 OK"))
        assertTrue(head.contains("Content-Length: 1000"))
        assertTrue(head.contains("Accept-Ranges: bytes"))
        assertArrayEquals(bytes, body)
    }

    @Test
    fun closedRangeReturns206WithContentRange() {
        started()
        val (head, body) = request(
            "GET /media/test.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=100-199\r\n\r\n",
        )
        assertTrue(head.startsWith("HTTP/1.1 206 Partial Content"))
        assertTrue(head.contains("Content-Range: bytes 100-199/1000"))
        assertTrue(head.contains("Content-Length: 100"))
        assertArrayEquals(bytes.copyOfRange(100, 200), body)
    }

    @Test
    fun openEndedRangeStopsAtEof() {
        started()
        val (head, body) = request(
            "GET /media/test.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=900-\r\n\r\n",
        )
        assertTrue(head.startsWith("HTTP/1.1 206 Partial Content"))
        assertTrue(head.contains("Content-Range: bytes 900-999/1000"))
        assertArrayEquals(bytes.copyOfRange(900, 1000), body)
    }

    @Test
    fun suffixRangeServesTheTail() {
        started()
        val (head, body) = request(
            "GET /media/test.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=-50\r\n\r\n",
        )
        assertTrue(head.startsWith("HTTP/1.1 206 Partial Content"))
        assertTrue(head.contains("Content-Range: bytes 950-999/1000"))
        assertArrayEquals(bytes.copyOfRange(950, 1000), body)
    }

    @Test
    fun rangePastEofIs416() {
        started()
        val (head, _) = request(
            "GET /media/test.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=5000-5009\r\n\r\n",
        )
        assertTrue(head.startsWith("HTTP/1.1 416"))
        assertTrue(head.contains("Content-Range: bytes */1000"))
    }

    @Test
    fun headSendsHeadersWithoutBody() {
        started()
        val (head, body) = request("HEAD /media/test.bin HTTP/1.1\r\nHost: x\r\n\r\n")
        assertTrue(head.startsWith("HTTP/1.1 200 OK"))
        assertTrue(head.contains("Content-Length: 1000"))
        assertEquals(0, body.size)
    }

    @Test
    fun clientResetMidStreamDoesNotKillTheServer() {
        // A renderer that seeks or stops resets the socket while bytes are
        // still in flight; Android kills the app for an uncaught throw on any
        // thread, so the serve loop must swallow exactly this.
        val big = File.createTempFile("horizon-reset", ".bin").apply {
            writeBytes(ByteArray(2 * 1024 * 1024) { (it % 251).toByte() })
        }
        val failures = CopyOnWriteArrayList<Throwable>()
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        try {
            val url = server.start(
                MediaServer.Item("big.bin", "video/mp4", big.length()) { offset ->
                    FileInputStream(big).apply { channel.position(offset) }
                },
            )
            requireNotNull(url)
            val bigPort = Regex(":(\\d+)/").find(url)!!.groupValues[1].toInt()
            Thread.setDefaultUncaughtExceptionHandler { _, error -> failures.add(error) }
            Socket("127.0.0.1", bigPort).use { socket ->
                socket.getOutputStream().write("GET /media/big.bin HTTP/1.1\r\nHost: x\r\n\r\n".toByteArray())
                socket.getOutputStream().flush()
                socket.getInputStream().read(ByteArray(64))
                socket.setSoLinger(true, 0)
            }
            Thread.sleep(400)
            assertTrue("serve thread threw ${failures.firstOrNull()}", failures.isEmpty())
            Socket("127.0.0.1", bigPort).use { socket ->
                socket.soTimeout = 4000
                socket.getOutputStream().write("HEAD /media/big.bin HTTP/1.1\r\nHost: x\r\n\r\n".toByteArray())
                socket.getOutputStream().flush()
                val head = socket.getInputStream().bufferedReader(Charsets.ISO_8859_1).readLine()
                assertEquals("HTTP/1.1 200 OK", head)
            }
        } finally {
            Thread.setDefaultUncaughtExceptionHandler(previous)
            big.delete()
        }
    }

    @Test
    fun urlEncodesNonAsciiNames() {
        server.stop()
        val name = "会议 记录.mp4"
        val url = server.start(MediaServer.Item(name, "video/mp4", file.length()) { offset ->
            FileInputStream(file).apply { channel.position(offset) }
        })
        requireNotNull(url)
        assertTrue(url!!.endsWith("/media/%E4%BC%9A%E8%AE%AE%20%E8%AE%B0%E5%BD%95.mp4"))
    }

    @Test
    fun mimeGuessingCoversDesktopSinkFormats() {
        assertEquals("video/mp4", MediaServer.guessMime("a.MP4"))
        assertEquals("video/x-matroska", MediaServer.guessMime("b.mkv"))
        assertEquals("video/mp2t", MediaServer.guessMime("c.ts"))
        assertEquals("audio/flac", MediaServer.guessMime("d.flac"))
        assertEquals("image/png", MediaServer.guessMime("e.png"))
        assertEquals("application/octet-stream", MediaServer.guessMime("f.bin"))
    }

    private fun request(raw: String): Pair<String, ByteArray> {
        Socket("127.0.0.1", port).use { socket ->
            socket.soTimeout = 4000
            socket.getOutputStream().write(raw.toByteArray(Charsets.ISO_8859_1))
            socket.getOutputStream().flush()
            val all = socket.getInputStream().readBytes()
            val split = indexOf(all, byteArrayOf(13, 10, 13, 10))
            if (split < 0) return String(all, Charsets.ISO_8859_1) to ByteArray(0)
            val head = String(all, 0, split, Charsets.ISO_8859_1)
            return head to all.copyOfRange(split + 4, all.size)
        }
    }

    private fun indexOf(haystack: ByteArray, needle: ByteArray): Int {
        outer@ for (index in 0..haystack.size - needle.size) {
            for (offset in needle.indices) {
                if (haystack[index + offset] != needle[offset]) continue@outer
            }
            return index
        }
        return -1
    }
}
