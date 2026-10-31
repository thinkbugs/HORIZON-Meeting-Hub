package com.horizon.castsender

import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket

/**
 * A raw-socket stand-in for the hub's `/api/media/upload` and queue routes. It speaks the same
 * JSON that `media_stage::invoke` writes and parses the same multipart body the
 * extension's `FormData` produces, so the phone's client is exercised against
 * the wire shape rather than against its own parser.
 */
class HubMediaClientTest {
    private var hub: FakeHub? = null

    @After
    fun tearDown() {
        hub?.close()
    }

    private fun start(handler: (FakeHub.Request) -> FakeHub.Reply): FakeHub {
        val fake = FakeHub(handler)
        hub = fake
        return fake
    }

    private fun client(fake: FakeHub) = HubMediaClient("127.0.0.1", fake.port)

    // ------------------------------------------------------------- library

    @Test
    fun libraryKeepsChineseTitlesAndByteCounts() {
        val fake = start { request ->
            assertEquals("GET", request.method)
            assertEquals("/api/media/library", request.path)
            FakeHub.Reply(
                200,
                """{"items":[{"id":"a1","title":"会议记录.mp4","kind":"video","file":"a1.mp4",
                   "url":"http://h/media/a1/会议记录.mp4","bytes":5242880,"addedAt":1,"source":"upload"}],
                   "root":"C:/lib","base":"http://192.168.1.5:7878"}""",
            )
        }
        val items = client(fake).library()
        assertEquals(1, items.size)
        assertEquals("会议记录.mp4", items[0].title)
        assertEquals("video", items[0].kind)
        assertEquals(5242880L, items[0].bytes)
        assertEquals("a1", items[0].id)
    }

    @Test
    fun queueReadsItemsAndThePlayingState() {
        val fake = start {
            FakeHub.Reply(200, queueJson(current = 1, playing = true, phase = "playing"))
        }
        val queue = client(fake).queue()
        assertEquals(2, queue.items.size)
        assertEquals("第一张.jpg", queue.items[0].title)
        assertEquals("image", queue.items[0].kind)
        assertTrue(queue.items[0].local)
        assertEquals("第二段.mp4", queue.currentItem?.title)
        assertEquals(1, queue.current)
        assertTrue(queue.playing)
        assertEquals("playing", queue.phase)
        assertEquals("http://h/media/b2/第二段.mp4", queue.uri)
        assertEquals(42.5, queue.position, 0.001)
        assertEquals(120.0, queue.duration, 0.001)
        assertNull(queue.halted)
    }

    @Test
    fun haltedQueueKeepsTheReason() {
        val fake = start {
            FakeHub.Reply(
                200,
                """{"items":[],"current":null,"order":"sequential","repeat":"off",
                   "imageDwell":5.0,"playing":false,"halted":"无法解码：a1","state":{"phase":"error"}}""",
            )
        }
        val queue = client(fake).queue()
        assertEquals(-1, queue.current)
        assertEquals("无法解码：a1", queue.halted)
        assertNull(queue.currentItem)
        assertEquals(5.0, queue.imageDwell, 0.001)
    }

    // ------------------------------------------------------------- actions

    @Test
    fun setQueuePostsEverySelectedIdInOneRequest() {
        var body: JSONObject? = null
        val fake = start { request ->
            assertEquals("POST", request.method)
            assertEquals("/api/media/setQueue", request.path)
            body = JSONObject(String(request.body, Charsets.UTF_8))
            FakeHub.Reply(200, queueJson(current = 0, playing = true, phase = "loading"))
        }
        val queue = client(fake).setQueue(listOf("a1", "b2", "c3"), autoplay = true)
        val sent = requireNotNull(body)
        val ids = sent.getJSONArray("ids")
        assertEquals(3, ids.length())
        assertEquals("a1", ids.getString(0))
        assertEquals("c3", ids.getString(2))
        assertTrue(sent.getBoolean("autoplay"))
        assertEquals(2, queue.items.size)
        assertTrue(queue.playing)
    }

    @Test
    fun enqueueAppendsWithoutTakingTheScreen() {
        var body: JSONObject? = null
        val fake = start { request ->
            body = JSONObject(String(request.body, Charsets.UTF_8))
            FakeHub.Reply(200, queueJson(current = 1, playing = true, phase = "playing"))
        }
        client(fake).enqueue(listOf("b2"))
        val sent = requireNotNull(body)
        assertEquals(1, sent.getJSONArray("ids").length())
        assertTrue("enqueue must not start playback", !sent.getBoolean("autoplay"))
    }

    @Test
    fun queueMoveSendsTheSlotItCameFromAndTheSlotItGoesTo() {
        var body: JSONObject? = null
        val fake = start { request ->
            assertEquals("/api/media/queueMove", request.path)
            body = JSONObject(String(request.body, Charsets.UTF_8))
            FakeHub.Reply(200, queueJson(current = 1, playing = true, phase = "playing"))
        }
        client(fake).queueMove(2, 0)
        val sent = requireNotNull(body)
        assertEquals(2, sent.getInt("index"))
        assertEquals(0, sent.getInt("to"))
    }

    @Test
    fun queueRemoveAndSetNextWorkById() {
        val seen = mutableListOf<String>()
        val bodies = mutableListOf<JSONObject>()
        val fake = start { request ->
            seen.add(request.path)
            bodies.add(JSONObject(String(request.body, Charsets.UTF_8)))
            FakeHub.Reply(200, queueJson(current = 0, playing = true, phase = "playing"))
        }
        val real = client(fake)
        real.queueRemove("b2")
        real.setNext("a1")
        assertEquals(listOf("/api/media/queueRemove", "/api/media/setNext"), seen)
        assertEquals("b2", bodies[0].getString("id"))
        assertEquals("a1", bodies[1].getString("id"))
    }

    @Test
    fun transportReadsTheQueueNestedInsideTheStatus() {
        var body: JSONObject? = null
        val fake = start { request ->
            assertEquals("/api/media/transport", request.path)
            body = JSONObject(String(request.body, Charsets.UTF_8))
            FakeHub.Reply(
                200,
                """{"state":{"phase":"paused","position":12.0,"duration":120.0},
                   "playing":false,"current":{"id":"b2","title":"第二段.mp4"},
                   "queue":${queueJson(current = 1, playing = false, phase = "paused", position = 12.0)}}""",
            )
        }
        val queue = client(fake).transport("pause")
        assertEquals("pause", requireNotNull(body).getString("action"))
        assertEquals("paused", queue.phase)
        assertTrue(!queue.playing)
        assertEquals("第二段.mp4", queue.currentItem?.title)
        assertEquals(12.0, queue.position, 0.001)
    }

    @Test
    fun seekCarriesThePositionItWasGiven() {
        var body: JSONObject? = null
        val fake = start { request ->
            body = JSONObject(String(request.body, Charsets.UTF_8))
            FakeHub.Reply(200, queueJson(current = 0, playing = true, phase = "playing"))
        }
        client(fake).transport("seek", at = 30.0)
        val sent = requireNotNull(body)
        assertEquals("seek", sent.getString("action"))
        assertEquals(30.0, sent.getDouble("at"), 0.001)
    }

    @Test
    fun aRefusedActionCarriesTheHubsOwnMessage() {
        val fake = start {
            FakeHub.Reply(400, """{"error":"队列里没有这一项"}""")
        }
        try {
            client(fake).jump(9)
            fail("a refused jump must throw")
        } catch (error: HubException) {
            assertEquals("队列里没有这一项", error.message)
        }
    }

    @Test
    fun aClosedPortIsReportedInsteadOfHanging() {
        val freePort = ServerSocket(0).use { it.localPort }
        try {
            HubMediaClient("127.0.0.1", freePort).queue()
            fail("a closed port must throw")
        } catch (error: HubException) {
            assertTrue("message names the address: ${error.message}", error.message!!.contains("127.0.0.1:$freePort"))
        }
    }

    // -------------------------------------------------------------- upload

    @Test
    fun uploadSendsTheWholeSelectionInOneMultipartRequest() {
        val first = ByteArray(64) { (it + 1).toByte() }
        val second = ByteArray(200_000) { (it % 251).toByte() }
        var parts: List<FakeHub.Part> = emptyList()
        val fake = start { request ->
            assertEquals("/api/media/upload", request.path)
            assertTrue(request.method == "POST")
            assertTrue(
                "multipart body: ${request.headers["content-type"]}",
                request.headers["content-type"]!!.startsWith("multipart/form-data; boundary="),
            )
            parts = FakeHub.parts(request)
            FakeHub.Reply(
                200,
                """{"items":[{"id":"u1","title":"现场 照片.jpg","kind":"image","bytes":${first.size}},
                   {"id":"u2","title":"录音.wav","kind":"audio","bytes":${second.size}}]}""",
            )
        }
        val progress = mutableListOf<Pair<Long, Long>>()
        val items = client(fake).upload(
            listOf(
                UploadFile("现场 照片.jpg", "image/jpeg", first.size.toLong()) { ByteArrayInputStream(first) },
                UploadFile("录音.wav", "audio/wav", second.size.toLong()) { ByteArrayInputStream(second) },
            ),
        ) { sent, total -> progress.add(sent to total) }

        assertEquals(2, parts.size)
        assertEquals("现场 照片.jpg", parts[0].filename)
        assertEquals("image/jpeg", parts[0].mime)
        assertArrayEquals(first, parts[0].data)
        assertEquals("录音.wav", parts[1].filename)
        assertArrayEquals(second, parts[1].data)
        assertEquals(2, items.size)
        assertEquals("u1", items[0].id)
        assertEquals(64L, items[0].bytes)
        // Progress counts payload bytes across the selection, so the last tick
        // is the sum of both files and never the boundary or header overhead.
        assertEquals((first.size + second.size).toLong(), progress.last().first)
        assertEquals((first.size + second.size).toLong(), progress.last().second)
        assertTrue("progress must grow", progress.size > 1)
    }

    @Test
    fun aRejectedUploadExplainsWhy() {
        val fake = start {
            FakeHub.Reply(400, """{"error":"文件太大"}""")
        }
        try {
            client(fake).upload(listOf(UploadFile("big.mp4", "video/mp4", 3) { ByteArrayInputStream(ByteArray(3)) }))
            fail("a rejected upload must throw")
        } catch (error: HubException) {
            assertEquals("文件太大", error.message)
        }
    }

    @Test
    fun anUploadTheLibraryDroppedIsNotReportedAsSuccess() {
        val fake = start { FakeHub.Reply(200, """{"items":[]}""") }
        try {
            client(fake).upload(listOf(UploadFile("empty.bin", "application/octet-stream", 0) { ByteArrayInputStream(ByteArray(0)) }))
            fail("zero stored items must throw")
        } catch (error: HubException) {
            assertTrue(error.message!!.contains("片库没有接受"))
        }
    }

    // -------------------------------------------------------------- fixtures

    private fun queueJson(
        current: Int,
        playing: Boolean,
        phase: String,
        position: Double = 42.5,
        duration: Double = 120.0,
    ): String {
        val items = JSONArray()
        items.put(
            JSONObject().apply {
                put("id", "a1")
                put("uri", "http://h/media/a1/第一张.jpg")
                put("title", "第一张.jpg")
                put("kind", "image")
                put("local", true)
            },
        )
        items.put(
            JSONObject().apply {
                put("id", "b2")
                put("uri", "http://h/media/b2/第二段.mp4")
                put("title", "第二段.mp4")
                put("kind", "video")
                put("local", true)
            },
        )
        return JSONObject().apply {
            put("items", items)
            put("current", current)
            put("order", "sequential")
            put("repeat", "off")
            put("imageDwell", 5.0)
            put("playing", playing)
            put("halted", JSONObject.NULL)
            put(
                "state",
                JSONObject().apply {
                    put("phase", phase)
                    put("uri", "http://h/media/b2/第二段.mp4")
                    put("position", position)
                    put("duration", duration)
                    put("volume", 1.0)
                    put("muted", false)
                    put("error", JSONObject.NULL)
                },
            )
        }.toString()
    }
}

/**
 * One request at a time, `Connection: close`, exactly the shape a client with a
 * plain `HttpURLConnection` sees from the hub.
 */
class FakeHub(private val handler: (Request) -> Reply) {
    data class Request(
        val method: String,
        val path: String,
        val headers: Map<String, String>,
        val body: ByteArray,
    )

    data class Reply(val status: Int, val body: String, val contentType: String = "application/json")

    data class Part(val filename: String, val mime: String, val data: ByteArray)

    private val socket = ServerSocket(0, 20, InetAddress.getByName("127.0.0.1"))
    private val thread: Thread
    val port: Int = socket.localPort

    @Volatile private var running = true

    init {
        thread = Thread({
            while (running) {
                val client = try {
                    socket.accept()
                } catch (_: Exception) {
                    break
                }
                try {
                    serve(client)
                } catch (_: Exception) {
                    // A client that hangs up mid-request is the test's business.
                } finally {
                    try {
                        client.close()
                    } catch (_: Exception) {
                    }
                }
            }
        }, "fake-hub")
        thread.isDaemon = true
        thread.start()
    }

    fun close() {
        running = false
        try {
            socket.close()
        } catch (_: Exception) {
        }
    }

    private fun serve(client: Socket) {
        val input = client.getInputStream()
        val requestLine = readLine(input) ?: return
        val parts = requestLine.split(" ")
        if (parts.size < 2) return
        val method = parts[0]
        val path = parts[1]
        val headers = mutableMapOf<String, String>()
        while (true) {
            val line = readLine(input) ?: break
            if (line.isEmpty()) break
            val colon = line.indexOf(':')
            if (colon <= 0) continue
            headers[line.substring(0, colon).trim().lowercase()] = line.substring(colon + 1).trim()
        }
        val length = headers["content-length"]?.toIntOrNull() ?: 0
        val body = readFully(input, length)
        val reply = handler(Request(method, path, headers, body))
        val payload = reply.body.toByteArray(Charsets.UTF_8)
        val head = "HTTP/1.1 ${reply.status} ${reason(reply.status)}\r\n" +
            "Content-Type: ${reply.contentType}\r\n" +
            "Content-Length: ${payload.size}\r\n" +
            "Connection: close\r\n\r\n"
        val output: OutputStream = client.getOutputStream()
        output.write(head.toByteArray(Charsets.ISO_8859_1))
        output.write(payload)
        output.flush()
    }

    private fun reason(status: Int): String = when (status) {
        200 -> "OK"
        400 -> "Bad Request"
        404 -> "Not Found"
        else -> "Error"
    }

    private fun readLine(input: InputStream): String? {
        val out = ByteArrayOutputStream()
        while (true) {
            val byte = input.read()
            if (byte < 0) return if (out.size() == 0) null else out.toString("ISO-8859-1")
            if (byte == '\n'.code) {
                val text = out.toString("ISO-8859-1")
                return text.removeSuffix("\r")
            }
            out.write(byte)
        }
    }

    private fun readFully(input: InputStream, length: Int): ByteArray {
        val buffer = ByteArray(length)
        var read = 0
        while (read < length) {
            val step = input.read(buffer, read, length - read)
            if (step < 0) break
            read += step
        }
        return if (read == length) buffer else buffer.copyOf(read)
    }

    companion object {
        fun parts(request: Request): List<Part> {
            val type = request.headers["content-type"].orEmpty()
            val boundary = type.substringAfter("boundary=").trim()
            val delimiter = "--$boundary".toByteArray(Charsets.ISO_8859_1)
            val body = request.body
            val result = mutableListOf<Part>()
            var cursor = indexOf(body, delimiter, 0)
            while (cursor >= 0) {
                var start = cursor + delimiter.size
                if (start + 2 <= body.size && body[start] == '\r'.code.toByte() && body[start + 1] == '\n'.code.toByte()) {
                    start += 2
                } else {
                    break
                }
                val next = indexOf(body, delimiter, start)
                if (next < 0) break
                var end = next
                if (end >= 2 && body[end - 2] == '\r'.code.toByte() && body[end - 1] == '\n'.code.toByte()) end -= 2
                val chunk = body.copyOfRange(start, end)
                val split = indexOf(chunk, "\r\n\r\n".toByteArray(Charsets.ISO_8859_1), 0)
                if (split >= 0) {
                    val head = String(chunk, 0, split, Charsets.UTF_8)
                    val data = chunk.copyOfRange(split + 4, chunk.size)
                    val filename = Regex("filename=\"([^\"]*)\"").find(head)?.groupValues?.get(1).orEmpty()
                    val mime = Regex("Content-Type: ([^\r\n]+)").find(head)?.groupValues?.get(1)?.trim().orEmpty()
                    result.add(Part(filename, mime, data))
                }
                cursor = next
            }
            return result
        }

        private fun indexOf(haystack: ByteArray, needle: ByteArray, from: Int): Int {
            if (needle.isEmpty() || haystack.size < needle.size) return -1
            outer@ for (index in from..haystack.size - needle.size) {
                for (offset in needle.indices) {
                    if (haystack[index + offset] != needle[offset]) continue@outer
                }
                return index
            }
            return -1
        }
    }
}
