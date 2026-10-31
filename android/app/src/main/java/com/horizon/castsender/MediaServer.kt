package com.horizon.castsender

import java.io.BufferedOutputStream
import java.io.InputStream
import java.net.ServerSocket
import java.net.Socket
import java.net.URLEncoder

/**
 * Serves the one file the renderer is being asked to play. The DMR fetches the
 * URL itself over the LAN, so this has to understand Range requests — a player
 * that cannot seek will not play; Windows Media Foundation asks for ranges
 * immediately after the first bytes.
 */
class MediaServer {
    class Item(
        val name: String,
        val mime: String,
        val size: Long,
        /** Opens the bytes at an absolute offset; null when the URI is gone. */
        val open: (Long) -> InputStream?,
    )

    @Volatile private var server: ServerSocket? = null
    private var thread: Thread? = null
    @Volatile private var item: Item? = null

    var url: String? = null
        private set

    fun start(item: Item): String? {
        stop()
        val socket = try {
            ServerSocket(0)
        } catch (_: Exception) {
            return null
        }
        this.item = item
        server = socket
        val host = Discovery.localIp() ?: "127.0.0.1"
        val path = "/media/" + URLEncoder.encode(item.name, "UTF-8").replace("+", "%20")
        url = "http://$host:${socket.localPort}$path"
        val worker = Thread({ acceptLoop(socket) }, "media-http")
        worker.isDaemon = true
        thread = worker
        worker.start()
        return url
    }

    fun stop() {
        val socket = server
        server = null
        item = null
        try {
            socket?.close()
        } catch (_: Exception) {
        }
        thread = null
        url = null
    }

    private fun acceptLoop(socket: ServerSocket) {
        while (!socket.isClosed) {
            val connection = try {
                socket.accept()
            } catch (_: Exception) {
                break
            }
            Thread({ serve(connection) }, "media-http-conn").apply {
                isDaemon = true
                start()
            }
        }
    }

    /**
     * A player that seeks or stops resets the socket mid-body; an uncaught
     * IOException here used to kill the process, because an uncaught throw on
     * any thread takes the whole app down.
     */
    private fun serve(socket: Socket) {
        try {
            serveOne(socket)
        } catch (_: Exception) {
        }
    }

    private fun serveOne(socket: Socket) {
        socket.use { client ->
            val media = item ?: return
            client.soTimeout = 5000
            val input = client.getInputStream().bufferedReader(Charsets.ISO_8859_1)
            val requestLine = input.readLine() ?: return
            val parts = requestLine.split(' ')
            if (parts.size < 2) return
            val method = parts[0]
            var range: String? = null
            while (true) {
                val line = input.readLine() ?: break
                if (line.isEmpty()) break
                val colon = line.indexOf(':')
                if (colon <= 0) continue
                val name = line.substring(0, colon).trim()
                if (name.equals("Range", ignoreCase = true)) range = line.substring(colon + 1).trim()
            }

            val known = media.size > 0
            val requested = parseRange(range, media.size)
            val head = method.equals("HEAD", ignoreCase = true)
            val output = BufferedOutputStream(client.getOutputStream())
            val headers = StringBuilder()
            var start = 0L
            var end = if (known) media.size - 1 else -1L
            when {
                !known -> {
                    headers.append("HTTP/1.1 200 OK\r\n")
                    headers.append("Content-Type: ${media.mime}\r\n")
                }
                requested == null -> {
                    headers.append("HTTP/1.1 200 OK\r\n")
                    headers.append("Content-Type: ${media.mime}\r\n")
                    headers.append("Content-Length: ${media.size}\r\n")
                    headers.append("Accept-Ranges: bytes\r\n")
                }
                requested == Range.Unsatisfiable -> {
                    headers.append("HTTP/1.1 416 Range Not Satisfiable\r\n")
                    headers.append("Content-Range: bytes */${media.size}\r\n")
                    headers.append("Content-Length: 0\r\n")
                    output.write(headers.toString().toByteArray())
                    output.flush()
                    return
                }
                else -> {
                    start = requested.start
                    end = requested.end
                    val length = end - start + 1
                    headers.append("HTTP/1.1 206 Partial Content\r\n")
                    headers.append("Content-Type: ${media.mime}\r\n")
                    headers.append("Content-Length: $length\r\n")
                    headers.append("Content-Range: bytes $start-$end/${media.size}\r\n")
                    headers.append("Accept-Ranges: bytes\r\n")
                }
            }
            headers.append("Connection: close\r\n")
            headers.append("Cache-Control: no-cache\r\n\r\n")
            output.write(headers.toString().toByteArray(Charsets.ISO_8859_1))
            if (head) {
                output.flush()
                return
            }
            val stream = media.open(start) ?: return
            stream.use {
                val buffer = ByteArray(64 * 1024)
                var left = if (known) end - start + 1 else -1L
                while (left != 0L) {
                    val want = if (left < 0) buffer.size else minOf(buffer.size.toLong(), left).toInt()
                    val read = it.read(buffer, 0, want)
                    if (read < 0) break
                    output.write(buffer, 0, read)
                    if (left > 0) left -= read
                }
            }
            output.flush()
        }
    }

    private data class Range(val start: Long, val end: Long) {
        companion object {
            val Unsatisfiable = Range(-1, -1)
        }
    }

    private fun parseRange(value: String?, size: Long): Range? {
        if (value == null || size <= 0) return null
        if (!value.startsWith("bytes=", ignoreCase = true)) return null
        val spec = value.substring(6).substringBefore(',').trim()
        val dash = spec.indexOf('-')
        if (dash < 0) return Range.Unsatisfiable
        val left = spec.substring(0, dash).trim()
        val right = spec.substring(dash + 1).trim()
        val start: Long
        val end: Long
        if (left.isEmpty()) {
            // bytes=-500: the last 500 bytes.
            val suffix = right.toLongOrNull() ?: return Range.Unsatisfiable
            if (suffix <= 0) return Range.Unsatisfiable
            start = (size - suffix).coerceAtLeast(0)
            end = size - 1
        } else {
            start = left.toLongOrNull() ?: return Range.Unsatisfiable
            end = if (right.isEmpty()) size - 1 else (right.toLongOrNull() ?: return Range.Unsatisfiable)
        }
        if (start >= size || start > end) return Range.Unsatisfiable
        return Range(start, minOf(end, size - 1))
    }

    companion object {
        fun guessMime(name: String): String =
            when (name.substringAfterLast('.', "").lowercase()) {
                "mp4", "m4v" -> "video/mp4"
                "mkv" -> "video/x-matroska"
                "avi" -> "video/x-msvideo"
                "mpeg", "mpg" -> "video/mpeg"
                "ts" -> "video/mp2t"
                "mp3" -> "audio/mpeg"
                "m4a", "aac" -> "audio/mp4"
                "wav" -> "audio/wav"
                "flac" -> "audio/flac"
                "wma" -> "audio/x-ms-wma"
                "jpg", "jpeg" -> "image/jpeg"
                "png" -> "image/png"
                else -> "application/octet-stream"
            }
    }
}
