package com.horizon.castsender

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import java.io.BufferedInputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread

/** Drives the real client against a hand-rolled RFC 6455 server. */
class WsClientTest {
    private lateinit var server: ServerSocket
    private var port = 0

    @Before
    fun setUp() {
        server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
        port = server.localPort
    }

    @After
    fun tearDown() {
        server.close()
    }

    @Test
    fun handshakeCarriesPathAndFramesRoundTrip() {
        val failure = AtomicReference<Throwable?>(null)
        val clientTexts = CopyOnWriteArrayList<String>()
        val serverText = LinkedBlockingQueue<String>()
        val serverBinary = LinkedBlockingQueue<ByteArray>()
        val closed = CountDownLatch(1)
        val keyframeAsked = CountDownLatch(1)

        val serverThread = thread(name = "stub-receiver") {
            try {
                server.accept().use { socket ->
                    val input = BufferedInputStream(socket.getInputStream())
                    val request = readHeader(input)
                    val out = socket.getOutputStream()
                    out.write(
                        (
                            "HTTP/1.1 101 Switching Protocols\r\n" +
                                "Upgrade: websocket\r\n" +
                                "Connection: Upgrade\r\n" +
                                "Sec-WebSocket-Accept: dGVzdA==\r\n\r\n"
                            ).toByteArray(),
                    )
                    out.flush()
                    assertTrue(request.startsWith("GET /stream?token=123456 HTTP/1.1"))
                    assertTrue(request.contains("Upgrade: websocket"))
                    assertTrue(request.contains("Sec-WebSocket-Version: 13"))

                    writeText(out, """{"type":"welcome","receiver":"测试大屏","version":1}""")

                    var text = ""
                    var binary = ByteArray(0)
                    while (text.isEmpty() || binary.isEmpty()) {
                        val (opcode, payload) = readFrame(input)
                        if (opcode == 0x1) text = payload.toString(Charsets.UTF_8) else binary = payload
                    }
                    serverText.add(text)
                    serverBinary.add(binary)
                    writeText(out, """{"type":"keyframe"}""")
                    out.flush()
                    Thread.sleep(100)
                    out.write(byteArrayOf(0x88.toByte(), 0x02, 0x03, 0xE8.toByte()))
                    out.flush()
                }
            } catch (error: Throwable) {
                failure.set(error)
            }
        }

        lateinit var client: WsClient
        val binarySent = Wire.packet(Wire.KIND_VIDEO, 0b11, 1_700_000_000_000_000L, 1920, 1080, byteArrayOf(0x65, 1, 2, 3))
        client = WsClient("127.0.0.1", port, "/stream?token=123456", object : WsClient.Listener {
            override fun onOpen() {
                client.sendText("""{"type":"hello"}""")
                client.sendBinary(binarySent)
            }

            override fun onText(text: String) {
                clientTexts.add(text)
                if (text.contains("\"keyframe\"")) keyframeAsked.countDown()
            }

            override fun onClosed(reason: String) {
                closed.countDown()
            }
        })

        client.connect()
        assertTrue("keyframe request not received", keyframeAsked.await(5, TimeUnit.SECONDS))
        assertTrue("stream did not close", closed.await(5, TimeUnit.SECONDS))
        serverThread.join(5000)

        assertNull(failure.get())
        assertEquals(2, clientTexts.size)
        assertTrue(clientTexts[0].contains("welcome"))
        assertTrue(clientTexts[1].contains("keyframe"))
        assertEquals("""{"type":"hello"}""", serverText.poll(1, TimeUnit.SECONDS))
        val received = serverBinary.poll(1, TimeUnit.SECONDS)
        // The server side verifies the mask bit is set, which readFrame already
        // enforces by unmasking; the payload must still be our packet.
        assertEquals(Wire.KIND_VIDEO, received[0].toInt())
        assertEquals(binarySent.size, received.size)
        assertEquals(binarySent.last(), received.last())
        client.close()
    }

    @Test
    fun wrongTokenSurfacesReceiverMessage() {
        val serverThread = thread {
            server.accept().use { socket ->
                readHeader(BufferedInputStream(socket.getInputStream()))
                socket.getOutputStream().apply {
                    write("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".toByteArray())
                    flush()
                }
            }
        }
        val client = WsClient("127.0.0.1", port, "/stream?token=000000", object : WsClient.Listener {
            override fun onOpen() = Unit
            override fun onText(text: String) = Unit
            override fun onClosed(reason: String) = Unit
        })
        try {
            client.connect()
            fail("403 must not be treated as a session")
        } catch (error: IOException) {
            assertEquals("投屏码不正确", error.message)
        }
        serverThread.join(3000)
    }

    @Test
    fun missingStreamSurfacesReceiverMessage() {
        val serverThread = thread {
            server.accept().use { socket ->
                readHeader(BufferedInputStream(socket.getInputStream()))
                socket.getOutputStream().apply {
                    write("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".toByteArray())
                    flush()
                }
            }
        }
        val client = WsClient("127.0.0.1", port, "/stream?token=111111", object : WsClient.Listener {
            override fun onOpen() = Unit
            override fun onText(text: String) = Unit
            override fun onClosed(reason: String) = Unit
        })
        try {
            client.connect()
            fail("404 must not be treated as a session")
        } catch (error: IOException) {
            assertEquals("接收端没有开启投屏", error.message)
        }
        serverThread.join(3000)
    }

    private fun writeText(out: OutputStream, text: String) {
        val payload = text.toByteArray(Charsets.UTF_8)
        // Text frames here stay under 126 bytes so one length byte is enough.
        out.write(byteArrayOf(0x81.toByte(), payload.size.toByte()))
        out.write(payload)
    }

    private fun readHeader(input: InputStream): String {
        val buffer = StringBuilder()
        while (true) {
            val byte = input.read()
            if (byte < 0) break
            buffer.append(byte.toChar())
            if (buffer.length >= 4 && buffer.endsWith("\r\n\r\n")) break
        }
        return buffer.toString()
    }

    private fun readFrame(input: InputStream): Pair<Int, ByteArray> {
        val first = input.read()
        val second = input.read()
        val opcode = first and 0x0F
        assertTrue("client frames must be masked", second and 0x80 != 0)
        var length = (second and 0x7F).toLong()
        if (length == 126L) {
            length = ((input.read() shl 8) or input.read()).toLong()
        } else if (length == 127L) {
            var value = 0L
            repeat(8) { value = (value shl 8) or input.read().toLong() }
            length = value
        }
        val mask = ByteArray(4)
        readFully(input, mask)
        val payload = ByteArray(length.toInt())
        readFully(input, payload)
        for (index in payload.indices) {
            payload[index] = (payload[index].toInt() xor mask[index % 4].toInt()).toByte()
        }
        return opcode to payload
    }

    private fun readFully(input: InputStream, target: ByteArray) {
        var offset = 0
        while (offset < target.size) {
            val read = input.read(target, offset, target.size - offset)
            if (read < 0) throw IOException("closed")
            offset += read
        }
    }
}
