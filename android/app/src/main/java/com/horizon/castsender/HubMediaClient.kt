package com.horizon.castsender

import org.json.JSONArray
import org.json.JSONObject
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.util.UUID

/** A call the hub refused or never answered; the message is the hub's own text. */
class HubException(message: String) : Exception(message)

data class LibraryItem(
    val id: String,
    val title: String,
    val kind: String,
    val bytes: Long,
    val file: String,
    val url: String,
    val source: String,
)

data class QueueItem(
    val id: String,
    val title: String,
    val kind: String,
    val uri: String,
    val local: Boolean,
)

/** One queue snapshot, as `media_stage::queue_json` writes it. */
data class MediaQueue(
    val items: List<QueueItem>,
    val current: Int,
    val playing: Boolean,
    val halted: String?,
    val phase: String,
    /** The address the engine is actually feeding a player; lags `items` by one load. */
    val uri: String,
    val position: Double,
    val duration: Double,
    val order: String,
    val repeat: String,
    val imageDwell: Double,
) {
    val currentItem: QueueItem? get() = items.getOrNull(current)
}

data class UploadFile(
    val name: String,
    val mime: String,
    val size: Long,
    val open: () -> InputStream?,
)

/**
 * The projection library and its queue over the hub's HTTP API — the same
 * `/api/media/<action>` routes `extension/media.js` posts to, so a phone and the
 * extension cannot drift apart. Plain `HttpURLConnection`: the hub speaks CORS
 * for browsers, not a protocol of its own.
 */
class HubMediaClient(val host: String, val port: Int = LanDiscovery.HUB_PORT) {
    val address: String get() = "$host:$port"
    private val base: String get() = "http://$host:$port"

    fun library(): List<LibraryItem> = parseLibrary(read("library"))

    fun queue(): MediaQueue = parseQueue(read("queue"))

    fun setQueue(ids: List<String>, autoplay: Boolean = true): MediaQueue =
        act("setQueue", ids(ids).put("autoplay", autoplay))

    fun enqueue(ids: List<String>, autoplay: Boolean = false): MediaQueue =
        act("enqueue", ids(ids).put("autoplay", autoplay))

    fun jump(index: Int): MediaQueue = act("jump", JSONObject().put("index", index))

    fun queueMove(from: Int, to: Int): MediaQueue =
        act("queueMove", JSONObject().put("index", from).put("to", to))

    fun queueRemove(id: String): MediaQueue = act("queueRemove", JSONObject().put("id", id))

    fun setNext(id: String): MediaQueue = act("setNext", JSONObject().put("id", id))

    fun remove(id: String): MediaQueue = act("remove", JSONObject().put("id", id))

    fun transport(action: String, at: Double? = null, value: Double? = null): MediaQueue {
        val body = JSONObject().put("action", action)
        if (at != null) body.put("at", at)
        if (value != null) body.put("value", value)
        return act("transport", body)
    }

    /**
     * One multipart request for the whole selection, the same body a browser
     * builds from a `FormData` with repeated `file` fields. Sizes must be exact:
     * the body is streamed against a fixed Content-Length, so a provider that
     * cannot report a size has to be copied to a cache file first.
     */
    fun upload(files: List<UploadFile>, onProgress: (Long, Long) -> Unit = { _, _ -> }): List<LibraryItem> {
        require(files.isNotEmpty()) { "没有要上传的文件" }
        files.forEach { require(it.size >= 0) { "文件大小未知：${it.name}" } }
        val boundary = "----HorizonPhone${UUID.randomUUID().toString().replace("-", "")}"
        val tail = "--$boundary--\r\n".toByteArray()
        val heads = files.map { partHead(boundary, it).toByteArray(Charsets.UTF_8) }
        val total = heads.sumOf { it.size.toLong() } + files.sumOf { it.size + 2 } + tail.size
        val payloadTotal = files.sumOf { it.size }
        var connection: HttpURLConnection? = null
        try {
            connection = (URL("$base/api/media/upload").openConnection() as HttpURLConnection).apply {
                requestMethod = "POST"
                doOutput = true
                useCaches = false
                connectTimeout = CONNECT_TIMEOUT_MS
                // A multi-gigabyte recording is not a client-side error.
                readTimeout = 0
                setRequestProperty("Content-Type", "multipart/form-data; boundary=$boundary")
                setFixedLengthStreamingMode(total)
            }
            var sent = 0L
            connection.outputStream.use { out ->
                for ((index, file) in files.withIndex()) {
                    out.write(heads[index])
                    val stream = file.open() ?: throw HubException("无法读取 ${file.name}")
                    stream.use {
                        val buffer = ByteArray(64 * 1024)
                        while (true) {
                            val read = it.read(buffer)
                            if (read < 0) break
                            out.write(buffer, 0, read)
                            sent += read
                            onProgress(sent, payloadTotal)
                        }
                    }
                    out.write(CRLF)
                }
                out.write(tail)
                out.flush()
            }
            val code = connection.responseCode
            val body = stream(connection, code).text()
            if (code !in 200..299) throw failure(code, body)
            val array = try {
                JSONObject(body).optJSONArray("items")
            } catch (_: Exception) {
                null
            } ?: JSONArray()
            val items = ArrayList<LibraryItem>(array.length())
            for (index in 0 until array.length()) {
                array.optJSONObject(index)?.let { items.add(parseItem(it)) }
            }
            if (items.isEmpty()) throw HubException("片库没有接受这些文件")
            return items
        } catch (error: HubException) {
            throw error
        } catch (_: Exception) {
            throw HubException("无法连接会议主机 $address")
        } finally {
            connection?.disconnect()
        }
    }

    // ------------------------------------------------------------ transport

    private fun read(what: String): JSONObject = request("GET", "/api/media/$what", null)

    private fun act(action: String, args: JSONObject): MediaQueue =
        parseQueue(request("POST", "/api/media/$action", args.toString().toByteArray(Charsets.UTF_8)))

    private fun request(method: String, path: String, body: ByteArray?): JSONObject {
        var connection: HttpURLConnection? = null
        try {
            connection = (URL("$base$path").openConnection() as HttpURLConnection).apply {
                requestMethod = method
                useCaches = false
                connectTimeout = CONNECT_TIMEOUT_MS
                readTimeout = READ_TIMEOUT_MS
                if (body != null) {
                    doOutput = true
                    setRequestProperty("Content-Type", "application/json; charset=utf-8")
                    setFixedLengthStreamingMode(body.size)
                }
            }
            if (body != null) connection.outputStream.use { it.write(body) }
            val code = connection.responseCode
            val text = stream(connection, code).text()
            if (code !in 200..299) throw failure(code, text)
            return try {
                JSONObject(text)
            } catch (_: Exception) {
                throw HubException("接收端返回的不是 JSON")
            }
        } catch (error: HubException) {
            throw error
        } catch (_: Exception) {
            throw HubException("无法连接会议主机 $address")
        } finally {
            connection?.disconnect()
        }
    }

    private fun stream(connection: HttpURLConnection, code: Int): InputStream =
        (if (code in 200..299) connection.inputStream else connection.errorStream)
            ?: throw HubException("接收端返回 $code")

    private fun InputStream.text(): String = bufferedReader().use { it.readText() }

    private fun failure(code: Int, body: String): HubException {
        val detail = try {
            JSONObject(body).optString("error")
        } catch (_: Exception) {
            ""
        }
        return HubException(detail.ifEmpty { "接收端返回 $code" })
    }

    // -------------------------------------------------------------- parsing

    private fun ids(values: List<String>): JSONObject {
        val array = JSONArray()
        values.forEach { array.put(it) }
        return JSONObject().put("ids", array)
    }

    private fun partHead(boundary: String, file: UploadFile): String {
        // Quotes and line breaks would end the header early; the name is only a
        // caption, so dropping them changes the label and never the bytes.
        val name = file.name.replace("\"", "'").replace("\r", "").replace("\n", "").ifEmpty { "media" }
        val mime = file.mime.ifEmpty { "application/octet-stream" }
        return "--$boundary\r\n" +
            "Content-Disposition: form-data; name=\"file\"; filename=\"$name\"\r\n" +
            "Content-Type: $mime\r\n\r\n"
    }

    companion object {
        private const val CONNECT_TIMEOUT_MS = 2000
        private const val READ_TIMEOUT_MS = 5000
        private val CRLF = "\r\n".toByteArray()

        fun parseLibrary(body: JSONObject): List<LibraryItem> {
            val array = body.optJSONArray("items") ?: return emptyList()
            return (0 until array.length()).mapNotNull { array.optJSONObject(it)?.let(::parseItem) }
        }

        fun parseQueue(body: JSONObject): MediaQueue {
            // `transport` answers a status object with the queue nested under
            // `queue`; every queue action answers the queue itself.
            val queue = body.optJSONObject("queue") ?: body
            val items = queue.optJSONArray("items") ?: JSONArray()
            val list = ArrayList<QueueItem>(items.length())
            for (index in 0 until items.length()) {
                val item = items.optJSONObject(index) ?: continue
                list.add(
                    QueueItem(
                        id = item.optString("id"),
                        title = item.optString("title"),
                        kind = item.optString("kind"),
                        uri = item.optString("uri"),
                        local = item.optBoolean("local"),
                    ),
                )
            }
            val state = queue.optJSONObject("state")
            return MediaQueue(
                items = list,
                current = if (queue.isNull("current")) -1 else queue.optInt("current", -1),
                playing = queue.optBoolean("playing"),
                halted = queue.optString("halted").takeIf { it.isNotEmpty() && it != "null" },
                phase = state?.optString("phase").orEmpty().ifEmpty { "idle" },
                uri = state?.optString("uri").orEmpty(),
                position = state?.optDouble("position", 0.0) ?: 0.0,
                duration = state?.optDouble("duration", 0.0) ?: 0.0,
                order = queue.optString("order", "sequential"),
                repeat = queue.optString("repeat", "off"),
                imageDwell = queue.optDouble("imageDwell", 0.0),
            )
        }

        private fun parseItem(item: JSONObject): LibraryItem = LibraryItem(
            id = item.optString("id"),
            title = item.optString("title"),
            kind = item.optString("kind"),
            bytes = item.optLong("bytes"),
            file = item.optString("file"),
            url = item.optString("url"),
            source = item.optString("source"),
        )
    }
}
