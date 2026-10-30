package com.horizon.castsender

import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.security.SecureRandom
import java.util.Base64

/** A minimal RFC 6455 client: one text/binary writer, one reader, ping replies. */
class WsClient(
    private val host: String,
    private val port: Int,
    private val path: String,
    private val listener: Listener,
) {
    interface Listener {
        fun onOpen()
        fun onText(text: String)
        fun onClosed(reason: String)
    }

    @Volatile private var socket: Socket? = null
    @Volatile private var open = false
    private val writeLock = Any()
    private val random = SecureRandom()

    fun connect() {
        val socket = Socket()
        socket.tcpNoDelay = true
        socket.connect(InetSocketAddress(host, port), 6000)

        val key = ByteArray(16).also(random::nextBytes)
        val request = buildString {
            append("GET ").append(path).append(" HTTP/1.1\r\n")
            append("Host: ").append(host).append(':').append(port).append("\r\n")
            append("Upgrade: websocket\r\n")
            append("Connection: Upgrade\r\n")
            append("Sec-WebSocket-Key: ").append(Base64.getEncoder().encodeToString(key)).append("\r\n")
            append("Sec-WebSocket-Version: 13\r\n\r\n")
        }
        socket.getOutputStream().apply {
            write(request.toByteArray(Charsets.UTF_8))
            flush()
        }

        val input = BufferedInputStream(socket.getInputStream())
        val header = readHeader(input)
        val statusLine = header.lineSequence().firstOrNull().orEmpty()
        if (!statusLine.contains(" 101 ")) {
            socket.close()
            throw IOException(
                when {
                    statusLine.contains("403") -> "投屏码不正确"
                    statusLine.contains("404") -> "接收端没有开启投屏"
                    else -> "连接被拒绝（$statusLine）"
                },
            )
        }

        this.socket = socket
        open = true
        listener.onOpen()
        Thread({ readLoop(socket, input) }, "ws-read").start()
    }

    fun sendText(text: String) = sendFrame(OP_TEXT, text.toByteArray(Charsets.UTF_8))

    fun sendBinary(payload: ByteArray) = sendFrame(OP_BINARY, payload)

    fun close() {
        open = false
        try {
            socket?.close()
        } catch (_: IOException) {
        }
        socket = null
    }

    private fun readLoop(socket: Socket, input: InputStream) {
        var reason = "接收端已断开"
        try {
            while (true) {
                val first = input.read()
                if (first < 0) break
                val opcode = first and 0x0F
                val second = input.read()
                if (second < 0) break
                val masked = second and 0x80 != 0
                var length = (second and 0x7F).toLong()
                if (length == 126L) {
                    length = ((input.read() shl 8) or input.read()).toLong()
                } else if (length == 127L) {
                    var value = 0L
                    repeat(8) { value = (value shl 8) or input.read().toLong() }
                    length = value
                }
                if (length < 0 || length > MAX_FRAME) throw IOException("帧过大")
                val mask = if (masked) ByteArray(4).also { readFully(input, it) } else null
                val payload = ByteArray(length.toInt())
                readFully(input, payload)
                if (mask != null) {
                    for (i in payload.indices) {
                        payload[i] = (payload[i].toInt() xor mask[i % 4].toInt()).toByte()
                    }
                }
                when (opcode) {
                    OP_TEXT -> listener.onText(payload.toString(Charsets.UTF_8))
                    OP_CLOSE -> {
                        reason = "接收端已断开"
                        break
                    }
                    OP_PING -> sendFrame(OP_PONG, payload)
                    else -> Unit
                }
            }
        } catch (error: IOException) {
            reason = if (open) "连接中断：${error.message}" else "连接已关闭"
        } finally {
            open = false
            try {
                socket.close()
            } catch (_: IOException) {
            }
            this.socket = null
            listener.onClosed(reason)
        }
    }

    private fun readHeader(input: InputStream): String {
        val out = ByteArrayOutputStream()
        val tail = ByteArray(4)
        while (true) {
            val byte = input.read()
            if (byte < 0) throw IOException("连接已关闭")
            out.write(byte)
            System.arraycopy(tail, 1, tail, 0, 3)
            tail[3] = byte.toByte()
            if (tail.contentEquals(HEADER_END)) return out.toString("ISO-8859-1")
            if (out.size() > MAX_HEADER) throw IOException("握手响应过大")
        }
    }

    private fun readFully(input: InputStream, target: ByteArray) {
        var offset = 0
        while (offset < target.size) {
            val read = input.read(target, offset, target.size - offset)
            if (read < 0) throw IOException("连接已关闭")
            offset += read
        }
    }

    private fun sendFrame(opcode: Int, payload: ByteArray) {
        val socket = socket ?: return
        val mask = ByteArray(4).also(random::nextBytes)
        val header = ByteArrayOutputStream()
        header.write(0x80 or opcode)
        when {
            payload.size < 126 -> header.write(0x80 or payload.size)
            payload.size <= 0xFFFF -> {
                header.write(0x80 or 126)
                header.write(payload.size shr 8)
                header.write(payload.size and 0xFF)
            }
            else -> {
                header.write(0x80 or 127)
                val remaining = payload.size.toLong()
                for (shift in 7 downTo 0) header.write(((remaining shr (shift * 8)) and 0xFF).toInt())
            }
        }
        header.write(mask)
        val masked = ByteArray(payload.size) { index ->
            (payload[index].toInt() xor mask[index % 4].toInt()).toByte()
        }
        synchronized(writeLock) {
            try {
                val out = socket.getOutputStream()
                out.write(header.toByteArray())
                out.write(masked)
                out.flush()
            } catch (_: IOException) {
                close()
            }
        }
    }

    private companion object {
        const val OP_TEXT = 0x1
        const val OP_BINARY = 0x2
        const val OP_CLOSE = 0x8
        const val OP_PING = 0x9
        const val OP_PONG = 0xA
        const val MAX_HEADER = 16 * 1024
        const val MAX_FRAME = 4L * 1024 * 1024
        val HEADER_END = byteArrayOf(13, 10, 13, 10)
    }
}
