package com.horizon.castsender

import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.Inet4Address
import java.net.NetworkInterface
import java.net.URL
import java.util.Collections
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

data class Receiver(val host: String, val port: Int, val name: String, val legacy: Boolean) {
    val address: String get() = "$host:$port"
}

/**
 * Same contract as `extension/discovery.js`: sweep the phone's own /24s for
 * `GET http://<ip>:9407/info`. A 404 means a receiver older than the discovery
 * endpoint — it still takes the stream, so it is listed, flagged legacy.
 */
object Discovery {
    const val PORT = 9407
    private const val TIMEOUT_MS = 700
    private const val CONCURRENCY = 32

    fun localPrefixes(): List<String> {
        val prefixes = LinkedHashSet<String>()
        val interfaces = try {
            Collections.list(NetworkInterface.getNetworkInterfaces())
        } catch (_: Exception) {
            emptyList<NetworkInterface>()
        }
        for (iface in interfaces) {
            if (!iface.isUp || iface.isLoopback) continue
            for (address in Collections.list(iface.inetAddresses)) {
                if (address !is Inet4Address || address.isLoopbackAddress) continue
                val octets = address.hostAddress?.split(".") ?: continue
                if (octets.size != 4) continue
                prefixes.add(octets.take(3).joinToString("."))
            }
        }
        return prefixes.toList()
    }

    fun candidates(extraHost: String): List<String> {
        val hosts = LinkedHashSet<String>()
        hosts.add("127.0.0.1")
        for (prefix in localPrefixes()) {
            for (index in 1..254) hosts.add("$prefix.$index")
        }
        val trimmed = extraHost.trim()
        if (trimmed.isNotEmpty()) hosts.add(trimmed)
        return hosts.toList()
    }

    fun probe(host: String, port: Int = PORT): Receiver? {
        var connection: HttpURLConnection? = null
        return try {
            connection = (URL("http://$host:$port/info").openConnection() as HttpURLConnection).apply {
                connectTimeout = TIMEOUT_MS
                readTimeout = TIMEOUT_MS
                requestMethod = "GET"
                useCaches = false
                setRequestProperty("Accept", "application/json")
            }
            when (connection.responseCode) {
                HttpURLConnection.HTTP_OK -> {
                    val body = connection.inputStream.bufferedReader().use { it.readText() }
                    val info = JSONObject(body)
                    if (info.optString("service") != "horizon-browser") {
                        null
                    } else {
                        Receiver(
                            host = host,
                            port = info.optInt("port", port),
                            name = info.optString("name").ifEmpty { "HORIZON 接收端" },
                            legacy = false,
                        )
                    }
                }
                HttpURLConnection.HTTP_NOT_FOUND -> Receiver(host, port, "HORIZON 接收端", true)
                else -> null
            }
        } catch (_: Exception) {
            null
        } finally {
            connection?.disconnect()
        }
    }

    /** `onFound` fires from a worker thread; `onDone` once the sweep is empty. */
    fun sweep(extraHost: String, cancelled: AtomicBoolean, onFound: (Receiver) -> Unit, onDone: () -> Unit) {
        val queue = ArrayDeque(candidates(extraHost))
        if (queue.isEmpty()) {
            onDone()
            return
        }
        val pool = Executors.newFixedThreadPool(minOf(CONCURRENCY, queue.size))
        repeat(minOf(CONCURRENCY, queue.size)) {
            pool.execute {
                while (!cancelled.get()) {
                    val host = synchronized(queue) { queue.removeFirstOrNull() } ?: break
                    val receiver = probe(host) ?: continue
                    onFound(receiver)
                }
            }
        }
        pool.shutdown()
        pool.awaitTermination(60, TimeUnit.SECONDS)
        onDone()
    }

    fun rank(receivers: List<Receiver>, preferred: String?): List<Receiver> =
        receivers.sortedWith(
            compareBy<Receiver> { if (it.legacy) 1 else 0 }
                .thenBy {
                    when {
                        it.address == preferred -> 0
                        it.host.startsWith("127.") -> 2
                        else -> 1
                    }
                }
                .thenBy { it.name },
        )
}
