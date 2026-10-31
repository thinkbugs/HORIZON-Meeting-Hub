package com.horizon.castsender

import org.json.JSONObject
import java.io.InputStream
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit

/**
 * Client for the desktop file hub (`/server/webrtc` on 7878). The hub is a
 * headless SnapDrop peer: a file is a `header`, a stream of masked binary
 * frames, and a `partition` marker every megabyte. The hub answers each marker
 * and reports progress; the transfer is over when `transfer-complete` arrives.
 */
class HubClient(
    private val host: String,
    private val port: Int,
    private val listener: Listener,
) {
    interface Listener {
        fun onPeers(hubId: String, hubName: String, peerCount: Int)
        fun onProgress(progress: Double)
        fun onComplete()
        fun onClosed(reason: String)
    }

    @Volatile var hubId: String? = null
        private set
    @Volatile var hubName: String = "HORIZON 会议主机"
        private set
    @Volatile private var cancelled = false

    // Lazy so the listener (declared below) is ready before the socket is made.
    private val ws: WsClient by lazy { WsClient(host, port, "/server/webrtc", socketListener) }
    private val partitionAck = Semaphore(0)
    private val complete = Semaphore(0)
    private val connected = Semaphore(0)

    val address: String get() = "$host:$port"

    fun connect() {
        Thread({
            try {
                ws.connect()
                connected.release()
            } catch (error: Exception) {
                listener.onClosed(error.message ?: "无法连接会议主机")
            }
        }, "hub-connect").start()
    }

    fun awaitConnected(timeoutMs: Long): Boolean =
        connected.tryAcquire(timeoutMs, TimeUnit.MILLISECONDS)

    fun isConnected(): Boolean = ws.isOpen()

    fun close() {
        cancelled = true
        ws.close()
    }

    /** Text becomes a line in 桌面\MeetingHub\收到的文字.txt on the desktop. */
    fun sendText(text: String): Boolean {
        val hub = hubId ?: return false
        return ws.sendText(
            JSONObject().apply {
                put("type", "text")
                put("text", text)
                put("to", hub)
            }.toString(),
        )
    }

    /**
     * Blocks until the hub acknowledges the end of the file. [onBytes] reports
     * cumulative bytes so the page can show speed, not just a percentage.
     */
    fun sendFile(
        name: String,
        mime: String,
        size: Long,
        input: InputStream,
        onBytes: (Long) -> Unit,
    ): Boolean {
        val hub = hubId ?: return false
        drain(partitionAck)
        drain(complete)
        val header = JSONObject().apply {
            put("type", "header")
            put("name", name)
            put("mime", mime)
            put("size", size)
            put("to", hub)
        }
        ws.sendText(header.toString()) || return false

        val buffer = ByteArray(CHUNK)
        var offset = 0L
        var sincePartition = 0
        input.use { stream ->
            while (!cancelled) {
                val read = stream.read(buffer)
                if (read < 0) break
                if (read == 0) continue
                val chunk = if (read == buffer.size) buffer else buffer.copyOf(read)
                if (!ws.sendBinary(chunk)) return false
                offset += read
                sincePartition += read
                onBytes(offset)
                if (sincePartition >= PARTITION && offset < size) {
                    sincePartition = 0
                    if (!mark(offset, hub)) return false
                }
            }
        }
        if (offset < size) return false
        mark(offset, hub)
        return complete.tryAcquire(COMPLETE_TIMEOUT_MS, TimeUnit.MILLISECONDS)
    }

    private fun mark(offset: Long, hub: String): Boolean {
        drain(partitionAck)
        val sent = ws.sendText(
            JSONObject().apply {
                put("type", "partition")
                put("offset", offset)
                put("to", hub)
            }.toString(),
        )
        if (!sent) return false
        return partitionAck.tryAcquire(ACK_TIMEOUT_MS, TimeUnit.MILLISECONDS)
    }

    private fun drain(semaphore: Semaphore) {
        while (semaphore.tryAcquire()) Unit
    }

    private val socketListener = object : WsClient.Listener {
        override fun onOpen() {
            ws.sendText(JSONObject().put("type", "ping").toString())
        }

        override fun onText(text: String) {
            val value = try {
                JSONObject(text)
            } catch (_: Exception) {
                return
            }
            when (value.optString("type")) {
                "peers" -> {
                    val peers = value.optJSONArray("peers") ?: return
                    var id: String? = null
                    var name = ""
                    var count = 0
                    for (index in 0 until peers.length()) {
                        val peer = peers.optJSONObject(index) ?: continue
                        count += 1
                        val peerId = peer.optString("id")
                        if (peerId == HUB_ID) {
                            id = peerId
                            name = displayName(peer)
                        }
                    }
                    if (id == null) {
                        // Older builds may not use the fixed hub id; the hub is
                        // the one peer that cannot take a WebRTC offer.
                        for (index in 0 until peers.length()) {
                            val peer = peers.optJSONObject(index) ?: continue
                            if (!peer.optBoolean("rtcSupported", true)) {
                                id = peer.optString("id")
                                name = displayName(peer)
                                break
                            }
                        }
                    }
                    val resolved = id ?: return
                    hubId = resolved
                    if (name.isNotEmpty()) hubName = name
                    listener.onPeers(resolved, hubName, count)
                }
                "progress" -> listener.onProgress(value.optDouble("progress", 0.0))
                "partition-received" -> partitionAck.release()
                "transfer-complete" -> {
                    complete.release()
                    listener.onComplete()
                }
            }
        }

        override fun onClosed(reason: String) {
            cancelled = true
            listener.onClosed(reason)
        }
    }

    private fun displayName(peer: JSONObject): String {
        val name = peer.opt("name")
        if (name is JSONObject) {
            return name.optString("displayName").ifEmpty { name.optString("deviceName") }
        }
        return name?.toString().orEmpty()
    }

    private companion object {
        const val HUB_ID = "horizon-hub"
        const val CHUNK = 64_000
        const val PARTITION = 1_000_000
        const val ACK_TIMEOUT_MS = 8_000L
        const val COMPLETE_TIMEOUT_MS = 25_000L
    }
}
