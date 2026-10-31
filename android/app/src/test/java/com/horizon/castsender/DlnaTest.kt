package com.horizon.castsender

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.ServerSocket
import java.net.Socket

/**
 * Exercises the controller against a fake DMR built from the desktop's own
 * `/dd.xml` shape: relative control URLs, entities in friendlyName, SOAP bodies
 * with the instance and transport fields the Windows renderer returns.
 */
class DlnaTest {
    private val server = FakeDmr()
    private lateinit var dmr: Dlna.Dmr

    @After
    fun tearDown() {
        server.close()
    }

    private fun described(): Dlna.Dmr {
        if (::dmr.isInitialized) return dmr
        val found = Dlna.describe("http://127.0.0.1:${server.port}/dd.xml")
        assertNotNull("description should parse", found)
        dmr = found!!
        return dmr
    }

    @Test
    fun describeResolvesRelativeControlUrlsAndDecodesName() {
        val found = described()
        assertEquals("客厅大屏 & 投影", found.name)
        assertEquals("127.0.0.1", found.host)
        assertEquals("http://127.0.0.1:${server.port}/ctl/AVTransport", found.avTransport)
        assertEquals("http://127.0.0.1:${server.port}/ctl/RenderingControl", found.renderingControl)
    }

    @Test
    fun describeRejectsDeviceWithoutAvTransport() {
        val bare = FakeDmr(withAvTransport = false)
        try {
            assertNull(Dlna.describe("http://127.0.0.1:${bare.port}/dd.xml"))
        } finally {
            bare.close()
        }
    }

    @Test
    fun describeReturnsNullWhenDescriptionIsUnreachable() {
        val url = "http://127.0.0.1:${server.port}/dd.xml"
        server.close()
        assertNull(Dlna.describe(url))
    }

    @Test
    fun setUriSendsEscapedUriInsideDidlMetadata() {
        val target = described()
        assertTrue(Dlna.setUri(target, "http://127.0.0.1:9/media/a&b \"x\".mp4", "video/mp4", "标题 & 1"))
        val request = server.lastRequest("SetAVTransportURI")
        assertNotNull(request)
        assertEquals(
            "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"",
            request!!.soapAction,
        )
        assertTrue(request.body.contains("<InstanceID>0</InstanceID>"))
        assertTrue(request.body.contains("http://127.0.0.1:9/media/a&amp;b &quot;x&quot;.mp4"))
        // The DIDL is escaped once as a whole, so its own attributes survive a
        // second decode on the renderer.
        assertTrue(request.body.contains("&lt;DIDL-Lite xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;"))
        assertTrue(request.body.contains("&lt;upnp:class&gt;object.item.videoItem&lt;/upnp:class&gt;"))
    }

    @Test
    fun positionReadsTransportStateTimesAndDuration() {
        val position = Dlna.position(described())
        assertNotNull(position)
        assertEquals("PLAYING", position!!.state)
        assertTrue(position.playing)
        assertEquals(83.0, position.seconds, 0.001)
        assertEquals(600.0, position.duration, 0.001)
    }

    @Test
    fun volumeParsesRenderingControl() {
        val volume = Dlna.volume(described())
        assertNotNull(Dlna.lastError, volume)
        assertEquals(Dlna.lastError, 42, volume!!.toInt())
    }

    @Test
    fun seekSendsRelativeTimeTarget() {
        val target = described()
        assertTrue(Dlna.seek(target, 65.4))
        val request = server.lastRequest("Seek")
        assertNotNull(request)
        assertTrue(request!!.body.contains("<Unit>REL_TIME</Unit>"))
        assertTrue(request.body.contains("<Target>00:01:05</Target>"))
    }

    @Test
    fun actionOnMissingServiceReportsError() {
        val bare = FakeDmr(withAvTransport = false)
        try {
            // No AVTransport URL means describe fails, so hand the controller a
            // device that only knows RenderingControl.
            val onlyRendering = Dlna.Dmr(
                name = "only rendering",
                location = "http://127.0.0.1:${bare.port}/dd.xml",
                avTransport = "",
                renderingControl = "http://127.0.0.1:${bare.port}/ctl/RenderingControl",
                host = "127.0.0.1",
            )
            assertNull(Dlna.action(onlyRendering, "AVTransport", "Play", "<InstanceID>0</InstanceID>"))
            assertEquals("接收端不支持 AVTransport", Dlna.lastError)
        } finally {
            bare.close()
        }
    }

    private class Captured(val soapAction: String?, val body: String)

    private class FakeDmr(private val withAvTransport: Boolean = true) {
        private val socket = ServerSocket(0)
        private val running = java.util.concurrent.atomic.AtomicBoolean(true)
        private val captures = java.util.concurrent.ConcurrentHashMap<String, Captured>()
        val port: Int get() = socket.localPort

        init {
            val xml by lazy { description() }
            Thread({
                while (running.get()) {
                    val client = try {
                        socket.accept()
                    } catch (_: Exception) {
                        break
                    }
                    Thread({ serve(client, xml) }, "fake-dmr").apply { isDaemon = true }.start()
                }
            }, "fake-dmr-accept").apply { isDaemon = true }.start()
        }

        fun lastRequest(action: String): Captured? = captures[action]

        fun close() {
            running.set(false)
            try {
                socket.close()
            } catch (_: Exception) {
            }
        }

        private fun serve(client: Socket, description: String) {
            client.use { connection ->
                val input = connection.getInputStream()
                val head = readHead(input) ?: return
                val lines = head.split("\r\n")
                val requestLine = lines.first().split(' ')
                val method = requestLine.getOrElse(0) { "" }
                val path = requestLine.getOrElse(1) { "/" }
                
                val headers = lines.drop(1).mapNotNull { line ->
                    val colon = line.indexOf(':')
                    if (colon <= 0) null else line.substring(0, colon).trim().lowercase() to line.substring(colon + 1).trim()
                }.toMap()
                val length = headers["content-length"]?.toIntOrNull() ?: 0
                val body = if (length > 0) {
                    val bytes = ByteArray(length)
                    var read = 0
                    while (read < length) {
                        val n = input.read(bytes, read, length - read)
                        if (n < 0) break
                        read += n
                    }
                    String(bytes, 0, read, Charsets.UTF_8)
                } else {
                    ""
                }

                val action = Regex("#([A-Za-z]+)\"?$").find(headers["soapaction"].orEmpty())?.groupValues?.get(1)
                if (action != null) captures[action] = Captured(headers["soapaction"], body)

                val payload = when {
                    method == "GET" && path == "/dd.xml" -> description
                    path == "/ctl/AVTransport" -> soap(action, soapActionBody(action))
                    path == "/ctl/RenderingControl" -> soap(action, renderingBody(action))
                    else -> "not found"
                }
                val response = ByteArrayOutputStream().apply {
                    write("HTTP/1.1 200 OK\r\n".toByteArray())
                    write("Content-Type: text/xml; charset=\"utf-8\"\r\n".toByteArray())
                    write("Content-Length: ${payload.toByteArray(Charsets.UTF_8).size}\r\n".toByteArray())
                    write("Connection: close\r\n\r\n".toByteArray())
                    write(payload.toByteArray(Charsets.UTF_8))
                }.toByteArray()
                connection.getOutputStream().write(response)
                connection.getOutputStream().flush()
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

        private fun description(): String = buildString {
            append("<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device>")
            append("<friendlyName>客厅大屏 &amp; 投影</friendlyName>")
            append("<serviceList>")
            if (withAvTransport) {
                append("<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>")
                append("<serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>")
                append("<controlURL>/ctl/AVTransport</controlURL></service>")
            }
            append("<service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>")
            append("<serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>")
            append("<controlURL>http://127.0.0.1:$port/ctl/RenderingControl</controlURL></service>")
            append("</serviceList></device></root>")
        }

        private fun soap(action: String?, inner: String): String =
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">" +
                "<s:Body><u:${action}Response xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">" +
                inner +
                "</u:${action}Response></s:Body></s:Envelope>"

        private fun soapActionBody(action: String?): String = when (action) {
            "GetPositionInfo" ->
                "<RelTime>00:01:23</RelTime><TrackDuration>00:10:00</TrackDuration>" +
                    "<TrackURI>http://127.0.0.1:9/media/clip.mp4</TrackURI>"
            "GetTransportInfo" -> "<CurrentTransportState>PLAYING</CurrentTransportState>"
            else -> ""
        }

        private fun renderingBody(action: String?): String = when (action) {
            "GetVolume" -> "<CurrentVolume>42</CurrentVolume>"
            else -> ""
        }
    }
}
