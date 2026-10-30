package com.horizon.castsender

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.BufferedInputStream
import java.io.InputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.util.concurrent.atomic.AtomicReference

/**
 * `com.sun.net.httpserver` is not on the Android unit-test classpath, so the
 * receiver is stubbed with a raw socket that answers one request and closes.
 */
class DiscoveryTest {
    private val failure = AtomicReference<Throwable?>(null)
    private var server: ServerSocket? = null
    private var port = 0

    @After
    fun tearDown() {
        server?.close()
        failure.get()?.let { throw AssertionError("stub receiver failed", it) }
    }

    private fun serve(status: String, body: String) {
        val socket = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
        server = socket
        port = socket.localPort
        Thread({
            try {
                socket.use { listener ->
                    listener.accept().use { client ->
                        val input = BufferedInputStream(client.getInputStream())
                        readHeader(input)
                        val payload = body.toByteArray()
                        val head = "HTTP/1.1 $status\r\n" +
                            "Content-Type: application/json\r\n" +
                            "Content-Length: ${payload.size}\r\n" +
                            "Connection: close\r\n\r\n"
                        client.getOutputStream().apply {
                            write(head.toByteArray())
                            write(payload)
                            flush()
                        }
                    }
                }
            } catch (error: Throwable) {
                failure.set(error)
            }
        }, "stub-info").start()
    }

    private fun readHeader(input: InputStream) {
        var matched = 0
        while (matched < 4) {
            val byte = input.read()
            if (byte < 0) return
            matched = when {
                byte == '\r'.code && (matched == 0 || matched == 2) -> matched + 1
                byte == '\n'.code && (matched == 1 || matched == 3) -> matched + 1
                else -> 0
            }
        }
    }

    @Test
    fun identifiedReceiverKeepsItsNameAndPort() {
        serve("200 OK", """{"service":"horizon-browser","name":"会议室大屏","port":9407,"paired":false}""")
        val receiver = Discovery.probe("127.0.0.1", port)
        assertEquals("会议室大屏", receiver?.name)
        assertEquals(9407, receiver?.port)
        assertEquals("127.0.0.1:9407", receiver?.address)
        assertFalse(receiver!!.legacy)
    }

    @Test
    fun missingInfoEndpointIsTreatedAsLegacyReceiver() {
        serve("404 Not Found", "not found")
        val receiver = Discovery.probe("127.0.0.1", port)
        assertEquals("HORIZON 接收端", receiver?.name)
        assertTrue(receiver!!.legacy)
    }

    @Test
    fun foreignServiceIsNotListed() {
        serve("200 OK", """{"service":"something-else","name":"web server","port":80}""")
        assertNull(Discovery.probe("127.0.0.1", port))
    }

    @Test
    fun nonHorizonErrorIsNotListed() {
        serve("500 Internal Server Error", "boom")
        assertNull(Discovery.probe("127.0.0.1", port))
    }

    @Test
    fun closedPortIsNotListed() {
        val freePort = ServerSocket(0).use { it.localPort }
        assertNull(Discovery.probe("127.0.0.1", freePort))
    }

    @Test
    fun rankingPrefersIdentifiedPreferredLanOverLoopback() {
        val list = listOf(
            Receiver("127.0.0.1", 9407, "A 本机", false),
            Receiver("192.168.1.9", 9407, "B 会议室", false),
            Receiver("192.168.1.5", 9407, "C 大屏", true),
            Receiver("192.168.1.2", 9407, "D 首选", false),
        )
        val ranked = Discovery.rank(list, "192.168.1.2:9407")
        assertEquals("D 首选", ranked[0].name)
        assertEquals("B 会议室", ranked[1].name)
        assertEquals("A 本机", ranked[2].name)
        // A legacy receiver never outranks one that identified itself.
        assertEquals("C 大屏", ranked[3].name)
    }

    @Test
    fun candidatesAlwaysIncludeLoopbackAndExtraHost() {
        val hosts = Discovery.candidates("10.0.0.77")
        assertTrue(hosts.contains("127.0.0.1"))
        assertTrue(hosts.contains("10.0.0.77"))
    }
}
