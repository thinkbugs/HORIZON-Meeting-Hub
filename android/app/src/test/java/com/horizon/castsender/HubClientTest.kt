package com.horizon.castsender

import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.ServerSocket
import java.net.Socket
import java.security.MessageDigest
import java.util.Base64
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * Speaks the desktop hub's side of `/server/webrtc` (same messages as
 * `snapdrop_server.rs`): a fixed `horizon-hub` peer, `partition` acks every
 * megabyte, progress, and `transfer-complete`. Verifies the Android client's
 * framing and byte accounting without needing the Windows app running.
 */
class HubClientTest {
    private var hub: FakeHub? = null
    private var client: HubClient? = null

    @After
    fun tearDown() {
        client?.close()
        hub?.close()
    }

    private fun connect(): Triple<FakeHub, HubClient, Events> {
        val fake = FakeHub()
        hub = fake
        val events = Events()
        val real = HubClient("127.0.0.1", fake.port, events)
        client = real
        real.connect()
        assertTrue("ws handshake", real.awaitConnected(3000))
        val deadline = System.currentTimeMillis() + 3000
        while (real.hubId == null && System.currentTimeMillis() < deadline) Thread.sleep(20)
        assertEquals("horizon-hub", real.hubId)
        assertEquals("测试主机", real.hubName)
        assertTrue("peer list must include the desktop", events.peers.get() >= 1)
        return Triple(fake, real, events)
    }

    @Test
    fun hubPeerIsResolvedAndFileArrivesByteForByte() {
        val (fake, real, events) = connect()
        val payload = ByteArray(2_400_000) { (it % 251).toByte() }

        val ok = real.sendFile(
            "probe.bin",
            "application/octet-stream",
            payload.size.toLong(),
            ByteArrayInputStream(payload),
        ) { events.progress.add(it) }

        assertTrue("sendFile should complete", ok)
        assertEquals("probe.bin", fake.header!!.optString("name"))
        assertEquals(payload.size.toLong(), fake.header!!.optLong("size"))
        assertArrayEquals(payload, fake.received.toByteArray())
        // 1 MB partitions at 1_000_000, 2_000_000 and the closing marker.
        assertTrue("partition markers: ${fake.partitions}", fake.partitions >= 3)
        assertTrue("progress should be reported", events.progress.isNotEmpty())
        assertEquals(payload.size.toLong(), events.progress.last())
        assertTrue("no transfer error", events.closedReason == null)
    }

    @Test
    fun textIsAddressedToTheHubPeer() {
        val (fake, real, _) = connect()
        assertTrue(real.sendText("会议纪要：15:30"))
        val deadline = System.currentTimeMillis() + 2000
        while (fake.texts.isEmpty() && System.currentTimeMillis() < deadline) Thread.sleep(20)
        assertEquals(listOf("会议纪要：15:30"), fake.texts.toList())
        assertEquals("horizon-hub", fake.lastTo)
    }

    @Test
    fun sendsAreRejectedBeforeThePeerListArrives() {
        val real = HubClient("127.0.0.1", 9, Events())
        client = real
        assertFalse(real.sendText("hello"))
        assertFalse(
            real.sendFile("x.bin", "application/octet-stream", 1, ByteArrayInputStream(byteArrayOf(1))) {},
        )
    }

    private class Events : HubClient.Listener {
        val peers = AtomicInteger(0)
        val progress = CopyOnWriteArrayList<Long>()
        @Volatile var closedReason: String? = null
        override fun onPeers(hubId: String, hubName: String, peerCount: Int) {
            peers.incrementAndGet()
        }

        override fun onProgress(progress: Double) = Unit

        override fun onComplete() = Unit

        override fun onClosed(reason: String) {
            closedReason = reason
        }
    }

    private class FakeHub : AutoCloseable {
        private val server = ServerSocket(0)
        private val running = AtomicBoolean(true)
        private var expected = 0L
        private var receivedCount = 0L
        private var socket: Socket? = null
        private var out: java.io.OutputStream? = null

        val received = ByteArrayOutputStream()
        var header: JSONObject? = null
        @Volatile var partitions = 0
        @Volatile var lastTo: String? = null
        val texts = CopyOnWriteArrayList<String>()

        val port: Int get() = server.localPort

        init {
            Thread({
                while (running.get()) {
                    val accepted = try {
                        server.accept()
                    } catch (_: Exception) {
                        break
                    }
                    socket = accepted
                    Thread({ serve(accepted) }, "fake-hub").apply { isDaemon = true }.start()
                }
            }, "fake-hub-accept").apply { isDaemon = true }.start()
        }

        private fun serve(client: Socket) {
            try {
                val input = client.getInputStream()
                val head = readHead(input) ?: return
                val key = Regex("(?i)sec-websocket-key:\\s*(.+)").find(head)?.groupValues?.get(1)?.trim()
                val digest = MessageDigest.getInstance("SHA-1")
                    .digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").toByteArray())
                val accept = Base64.getEncoder().encodeToString(digest)
                client.getOutputStream().apply {
                    write(
                        ("HTTP/1.1 101 Switching Protocols\r\n" +
                            "Upgrade: websocket\r\nConnection: Upgrade\r\n" +
                            "Sec-WebSocket-Accept: $accept\r\n\r\n").toByteArray(),
                    )
                    flush()
                }
                out = client.getOutputStream()
                sendText(
                    JSONObject().put("type", "display-name").put(
                        "message",
                        JSONObject().put("displayName", "测试主机 · hub").put("deviceName", "测试主机"),
                    ).toString(),
                )
                sendText(
                    JSONObject().put("type", "peers").put(
                        "peers",
                        org.json.JSONArray().apply {
                            put(
                                JSONObject().put("id", "horizon-hub").put(
                                    "name",
                                    JSONObject().put("displayName", "测试主机").put("deviceName", "测试主机"),
                                ).put("rtcSupported", false),
                            )
                            put(
                                JSONObject().put("id", "tab1").put(
                                    "name",
                                    JSONObject().put("displayName", "浏览器 · tab1").put("deviceName", "浏览器"),
                                ).put("rtcSupported", true),
                            )
                        },
                    ).toString(),
                )

                while (running.get()) {
                    val frame = readFrame(input) ?: break
                    when (frame.opcode) {
                        0x1 -> handleText(String(frame.payload, Charsets.UTF_8))
                        0x2 -> handleBinary(frame.payload)
                        0x8 -> break
                        0x9 -> sendFrame(0xA, frame.payload)
                    }
                }
            } catch (_: Exception) {
            } finally {
                try {
                    client.close()
                } catch (_: Exception) {
                }
            }
        }

        private fun handleText(text: String) {
            val value = try {
                JSONObject(text)
            } catch (_: Exception) {
                return
            }
            when (value.optString("type")) {
                "header" -> {
                    synchronized(received) {
                        received.reset()
                        receivedCount = 0
                    }
                    expected = value.optLong("size")
                    header = value
                    lastTo = value.optString("to")
                }
                "partition" -> {
                    lastTo = value.optString("to")
                    partitions += 1
                    val offset = value.optLong("offset")
                    sendText(
                        JSONObject().put("type", "partition-received").put("offset", offset).toString(),
                    )
                    if (receivedCount >= expected) {
                        sendText(JSONObject().put("type", "transfer-complete").toString())
                    }
                }
                "text" -> {
                    lastTo = value.optString("to")
                    texts.add(value.optString("text"))
                    sendText(JSONObject().put("type", "transfer-complete").toString())
                }
            }
        }

        private fun handleBinary(payload: ByteArray) {
            synchronized(received) {
                received.write(payload)
                receivedCount += payload.size
            }
            sendText(
                JSONObject().put("type", "progress")
                    .put("progress", if (expected > 0) (receivedCount.toDouble() / expected).coerceAtMost(1.0) else 1.0)
                    .toString(),
            )
        }

        private fun sendText(text: String) = sendFrame(0x1, text.toByteArray(Charsets.UTF_8))

        private fun sendFrame(opcode: Int, payload: ByteArray) {
            val target = out ?: return
            val header = ByteArrayOutputStream()
            header.write(0x80 or opcode)
            when {
                payload.size < 126 -> header.write(payload.size)
                payload.size <= 0xFFFF -> {
                    header.write(126)
                    header.write(payload.size shr 8)
                    header.write(payload.size and 0xFF)
                }
                else -> {
                    header.write(127)
                    for (shift in 7 downTo 0) header.write(((payload.size.toLong() shr (shift * 8)) and 0xFF).toInt())
                }
            }
            synchronized(this) {
                try {
                    target.write(header.toByteArray())
                    target.write(payload)
                    target.flush()
                } catch (_: Exception) {
                }
            }
        }

        private fun readHead(input: InputStream): String? {
            val out = ByteArrayOutputStream()
            var state = 0
            while (true) {
                val byte = input.read()
                if (byte < 0) return null
                out.write(byte)
                state = when {
                    (state == 0 || state == 2) && byte == '\r'.code -> state + 1
                    (state == 1 || state == 3) && byte == '\n'.code -> state + 1
                    else -> 0
                }
                if (state == 4) return out.toString("ISO-8859-1")
            }
        }

        private class Frame(val opcode: Int, val payload: ByteArray)

        private fun readFrame(input: InputStream): Frame? {
            val first = input.read()
            if (first < 0) return null
            val opcode = first and 0x0F
            val second = input.read()
            if (second < 0) return null
            val masked = second and 0x80 != 0
            var length = (second and 0x7F).toLong()
            if (length == 126L) {
                length = ((input.read() shl 8) or input.read()).toLong()
            } else if (length == 127L) {
                var value = 0L
                repeat(8) { value = (value shl 8) or input.read().toLong() }
                length = value
            }
            val mask = if (masked) ByteArray(4).also { readFully(input, it) } else null
            val payload = ByteArray(length.toInt())
            readFully(input, payload)
            if (mask != null) {
                for (index in payload.indices) {
                    payload[index] = (payload[index].toInt() xor mask[index % 4].toInt()).toByte()
                }
            }
            return Frame(opcode, payload)
        }

        private fun readFully(input: InputStream, target: ByteArray) {
            var offset = 0
            while (offset < target.size) {
                val read = input.read(target, offset, target.size - offset)
                if (read < 0) throw java.io.EOFException()
                offset += read
            }
        }

        override fun close() {
            running.set(false)
            try {
                socket?.close()
            } catch (_: Exception) {
            }
            try {
                server.close()
            } catch (_: Exception) {
            }
        }
    }
}
