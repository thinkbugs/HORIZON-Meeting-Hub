package com.horizon.castsender

import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * One sweep of the phone's own /24s for both services HORIZON exposes: the
 * browser-mirror receiver on 9407 and the file hub on 7878. They usually run in
 * the same desktop app but are independent — transfer must still work after the
 * cast receiver is switched off, so both are probed.
 */
object LanDiscovery {
    data class Hub(val host: String, val port: Int = HUB_PORT)

    const val HUB_PORT = 7878
    private const val CONCURRENCY = 24

    @Volatile var receivers: List<Receiver> = emptyList()
        private set
    @Volatile var hubs: List<Hub> = emptyList()
        private set
    @Volatile var scanning: Boolean = false
        private set
    val scanned: Int get() = scannedCount.get()
    @Volatile var total: Int = 0
        private set

    private val scannedCount = AtomicInteger(0)

    private val listeners = CopyOnWriteArrayList<() -> Unit>()
    private val generation = AtomicInteger()
    private var cancel = AtomicBoolean(true)
    private var thread: Thread? = null

    fun addListener(listener: () -> Unit) {
        if (!listeners.contains(listener)) listeners.add(listener)
        listener()
    }

    fun removeListener(listener: () -> Unit) {
        listeners.remove(listener)
    }

    private fun publish() {
        for (listener in listeners) listener()
    }

    @Synchronized
    fun start(extraHost: String = "", force: Boolean = false) {
        if (scanning && !force) return
        val id = generation.incrementAndGet()
        cancel.set(true)
        val cancelled = AtomicBoolean(false)
        cancel = cancelled
        receivers = emptyList()
        hubs = emptyList()
        scannedCount.set(0)
        val hosts = Discovery.candidates(extraHost)
        total = hosts.size
        scanning = true
        publish()

        val work = Thread({
            val queue = ArrayDeque(hosts)
            val pool = Executors.newFixedThreadPool(minOf(CONCURRENCY, queue.size).coerceAtLeast(1))
            repeat(minOf(CONCURRENCY, queue.size).coerceAtLeast(1)) {
                pool.execute {
                    while (!cancelled.get() && generation.get() == id) {
                        val host = synchronized(queue) { queue.removeFirstOrNull() } ?: break
                        val receiver = Discovery.probe(host)
                        if (receiver != null && !cancelled.get() && generation.get() == id) {
                            synchronized(this) {
                                if (receivers.none { it.address == receiver.address }) {
                                    receivers = Discovery.rank(receivers + receiver, null)
                                }
                            }
                            publish()
                        }
                        if (probeHub(host, receiver?.hub) && !cancelled.get() && generation.get() == id) {
                            synchronized(this) {
                                val port = receiver?.hub ?: HUB_PORT
                                if (hubs.none { it.host == host && it.port == port }) {
                                    hubs = hubs + Hub(host, port)
                                }
                            }
                            publish()
                        }
                        if (generation.get() == id) {
                            scannedCount.incrementAndGet()
                            publish()
                        }
                    }
                }
            }
            pool.shutdown()
            pool.awaitTermination(120, TimeUnit.SECONDS)
            if (generation.get() == id) {
                scanning = false
                publish()
            }
        }, "lan-sweep")
        work.isDaemon = true
        thread = work
        work.start()
    }

    fun stop() {
        cancel.set(true)
        generation.incrementAndGet()
        scanning = false
    }

    /** Refresh a single host after a manual address is typed. */
    fun probeOne(host: String) {
        Thread({
            val receiver = Discovery.probe(host)
            if (receiver != null) {
                synchronized(this) {
                    if (receivers.none { it.address == receiver.address }) {
                        receivers = Discovery.rank(receivers + receiver, null)
                    }
                }
            }
            if (probeHub(host, receiver?.hub)) {
                val port = receiver?.hub ?: HUB_PORT
                synchronized(this) {
                    if (hubs.none { it.host == host && it.port == port }) hubs = hubs + Hub(host, port)
                }
            }
            publish()
        }, "lan-probe").start()
    }

    /**
     * The hub the receiver's `/info` named, or the default port when the
     * receiver did not identify itself. A named port is trusted on its own:
     * falling back to 7878 after it failed would just wait on a guess.
     */
    private fun probeHub(host: String, reported: Int? = null): Boolean {
        var connection: HttpURLConnection? = null
        val port = reported ?: HUB_PORT
        return try {
            connection = (URL("http://$host:$port/").openConnection() as HttpURLConnection).apply {
                connectTimeout = 450
                readTimeout = 450
                requestMethod = "GET"
                useCaches = false
            }
            if (connection.responseCode != HttpURLConnection.HTTP_OK) return false
            val buffer = CharArray(4096)
            val read = connection.inputStream.reader().use { it.read(buffer) }
            val head = if (read > 0) String(buffer, 0, read) else ""
            head.contains("snapdrop", ignoreCase = true)
        } catch (_: Exception) {
            false
        } finally {
            connection?.disconnect()
        }
    }
}
