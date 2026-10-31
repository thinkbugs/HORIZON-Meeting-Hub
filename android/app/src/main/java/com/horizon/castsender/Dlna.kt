package com.horizon.castsender

import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.HttpURLConnection
import java.net.InetAddress
import java.net.SocketTimeoutException
import java.net.URL
import java.util.Locale
import kotlin.math.roundToInt

/**
 * The controller half of the DLNA link the desktop already exposes: find a
 * MediaRenderer over SSDP, read its description, then drive AVTransport and
 * RenderingControl with SOAP. The renderer fetches the media URL itself, so the
 * phone also has to serve the file over HTTP (see [MediaServer]).
 */
object Dlna {
    const val AVT = "AVTransport"
    const val RCS = "RenderingControl"
    private const val SSDP_ADDRESS = "239.255.255.250"
    private const val SSDP_PORT = 1900
    private const val SEARCH_TARGET = "urn:schemas-upnp-org:device:MediaRenderer:1"

    data class Dmr(
        val name: String,
        val location: String,
        val avTransport: String,
        val renderingControl: String?,
        val host: String,
    )

    data class Position(val state: String, val seconds: Double, val duration: Double) {
        val playing: Boolean get() = state == "PLAYING"
        val paused: Boolean get() = state == "PAUSED_PLAYBACK"
    }

    @Volatile var lastError: String? = null
        private set

    fun discover(timeoutMs: Int = 3500): List<Dmr> {
        val locations = LinkedHashSet<String>()
        try {
            DatagramSocket().use { socket ->
                socket.soTimeout = 500
                val request =
                    "M-SEARCH * HTTP/1.1\r\n" +
                        "HOST: $SSDP_ADDRESS:$SSDP_PORT\r\n" +
                        "MAN: \"ssdp:discover\"\r\n" +
                        "MX: 2\r\n" +
                        "ST: $SEARCH_TARGET\r\n\r\n"
                val bytes = request.toByteArray(Charsets.US_ASCII)
                val group = InetAddress.getByName(SSDP_ADDRESS)
                repeat(2) {
                    socket.send(DatagramPacket(bytes, bytes.size, group, SSDP_PORT))
                }
                val deadline = System.currentTimeMillis() + timeoutMs
                while (System.currentTimeMillis() < deadline) {
                    val buffer = ByteArray(4096)
                    val packet = DatagramPacket(buffer, buffer.size)
                    try {
                        socket.receive(packet)
                    } catch (_: SocketTimeoutException) {
                        continue
                    }
                    val text = String(buffer, 0, packet.length, Charsets.ISO_8859_1)
                    header(text, "location")?.let(locations::add)
                }
            }
        } catch (error: Exception) {
            lastError = "SSDP 不可用：${error.message}"
        }
        val found = locations.mapNotNull { describe(it) }
        if (found.isEmpty() && lastError == null) {
            lastError = "没有发现 MediaRenderer"
        }
        return found.distinctBy { it.location }.sortedBy { it.name }
    }

    fun describe(location: String): Dmr? {
        val xml = httpGet(location) ?: return null
        val name = tag(xml, "friendlyName").ifEmpty { "DLNA 设备" }
        val base = try {
            URL(location)
        } catch (_: Exception) {
            return null
        }
        var avTransport: String? = null
        var rendering: String? = null
        for (service in SERVICE.findAll(xml)) {
            val body = service.groupValues[1]
            val type = tag(body, "serviceType")
            val control = tag(body, "controlURL").trim()
            if (control.isEmpty()) continue
            val resolved = try {
                URL(base, control).toString()
            } catch (_: Exception) {
                continue
            }
            when {
                type.contains("AVTransport") -> avTransport = resolved
                type.contains("RenderingControl") -> rendering = resolved
            }
        }
        val avt = avTransport ?: return null
        return Dmr(
            name = name,
            location = location,
            avTransport = avt,
            renderingControl = rendering,
            host = base.host,
        )
    }

    // ------------------------------------------------------------- control

    fun setUri(dmr: Dmr, uri: String, mime: String, title: String): Boolean {
        val metadata = didl(title, uri, mime)
        val args = "<InstanceID>0</InstanceID>" +
            "<CurrentURI>${escape(uri)}</CurrentURI>" +
            "<CurrentURIMetaData>${escape(metadata)}</CurrentURIMetaData>"
        return action(dmr, AVT, "SetAVTransportURI", args) != null
    }

    fun play(dmr: Dmr): Boolean =
        action(dmr, AVT, "Play", "<InstanceID>0</InstanceID><Speed>1</Speed>") != null

    fun pause(dmr: Dmr): Boolean =
        action(dmr, AVT, "Pause", "<InstanceID>0</InstanceID>") != null

    fun stop(dmr: Dmr): Boolean =
        action(dmr, AVT, "Stop", "<InstanceID>0</InstanceID>") != null

    fun seek(dmr: Dmr, seconds: Double): Boolean {
        val target = hms(seconds)
        val args = "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>$target</Target>"
        return action(dmr, AVT, "Seek", args) != null
    }

    fun setVolume(dmr: Dmr, percent: Int): Boolean {
        val args = "<InstanceID>0</InstanceID><Channel>Master</Channel>" +
            "<DesiredVolume>${percent.coerceIn(0, 100)}</DesiredVolume>"
        return action(dmr, RCS, "SetVolume", args) != null
    }

    fun position(dmr: Dmr): Position? {
        val body = action(dmr, AVT, "GetPositionInfo", "<InstanceID>0</InstanceID>") ?: return null
        val transport = action(dmr, AVT, "GetTransportInfo", "<InstanceID>0</InstanceID>")
        val state = transport?.let { tag(it, "CurrentTransportState") }.orEmpty()
        return Position(
            state = state,
            seconds = parseHms(tag(body, "RelTime")),
            duration = parseHms(tag(body, "TrackDuration")),
        )
    }

    fun volume(dmr: Dmr): Int? {
        val body = action(dmr, RCS, "GetVolume", "<InstanceID>0</InstanceID><Channel>Master</Channel>")
            ?: return null
        return tag(body, "CurrentVolume").trim().toDoubleOrNull()?.roundToInt()
    }

    /** Returns the response body, or null after recording why it failed. */
    fun action(dmr: Dmr, service: String, action: String, arguments: String): String? {
        val control: String? = when (service) {
            AVT -> dmr.avTransport
            RCS -> dmr.renderingControl
            else -> null
        }
        if (control.isNullOrBlank()) {
            lastError = "接收端不支持 $service"
            return null
        }
        val envelope =
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>" +
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" " +
                "s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">" +
                "<s:Body><u:$action xmlns:u=\"urn:schemas-upnp-org:service:$service:1\">" +
                arguments +
                "</u:$action></s:Body></s:Envelope>"
        var connection: HttpURLConnection? = null
        return try {
            connection = (URL(control).openConnection() as HttpURLConnection).apply {
                requestMethod = "POST"
                connectTimeout = 4000
                readTimeout = 4000
                doOutput = true
                useCaches = false
                setRequestProperty(
                    "SOAPACTION",
                    "\"urn:schemas-upnp-org:service:$service:1#$action\"",
                )
                setRequestProperty("Content-Type", "text/xml; charset=\"utf-8\"")
            }
            connection.outputStream.use { it.write(envelope.toByteArray(Charsets.UTF_8)) }
            val code = connection.responseCode
            if (code !in 200..299) {
                lastError = "$action 失败（HTTP $code）"
                return null
            }
            connection.inputStream.bufferedReader().use { it.readText() }
        } catch (error: Exception) {
            lastError = "$action 失败：${error.message}"
            null
        } finally {
            connection?.disconnect()
        }
    }

    // --------------------------------------------------------------- utils

    private fun httpGet(location: String): String? {
        var connection: HttpURLConnection? = null
        return try {
            connection = (URL(location).openConnection() as HttpURLConnection).apply {
                connectTimeout = 2000
                readTimeout = 2000
                requestMethod = "GET"
                useCaches = false
            }
            if (connection.responseCode != HttpURLConnection.HTTP_OK) return null
            connection.inputStream.bufferedReader().use { it.readText() }
        } catch (_: Exception) {
            null
        } finally {
            connection?.disconnect()
        }
    }

    private fun header(message: String, name: String): String? {
        for (line in message.lineSequence()) {
            val colon = line.indexOf(':')
            if (colon <= 0) continue
            if (line.substring(0, colon).trim().equals(name, ignoreCase = true)) {
                return line.substring(colon + 1).trim()
            }
        }
        return null
    }

    private fun tag(xml: String, name: String): String {
        val found = Regex("<(?:[\\w-]+:)?$name[^>]*>(.*?)</(?:[\\w-]+:)?$name>", RegexOption.DOT_MATCHES_ALL)
            .find(xml) ?: return ""
        return unescape(found.groupValues[1].trim())
    }

    private fun didl(title: String, uri: String, mime: String): String {
        val klass = when {
            mime.startsWith("video/") -> "object.item.videoItem"
            mime.startsWith("audio/") -> "object.item.audioItem.musicTrack"
            else -> "object.item.imageItem.photo"
        }
        return "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" " +
            "xmlns:dc=\"http://purl.org/dc/elements/1.1/\" " +
            "xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">" +
            "<item id=\"1\" parentID=\"0\" restricted=\"1\">" +
            "<dc:title>${escape(title)}</dc:title>" +
            "<upnp:class>$klass</upnp:class>" +
            "<res protocolInfo=\"http-get:*:$mime:*\">${escape(uri)}</res>" +
            "</item></DIDL-Lite>"
    }

    private fun escape(value: String): String = value
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\"", "&quot;")
        .replace("'", "&apos;")

    private fun unescape(value: String): String = value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")

    private fun hms(seconds: Double): String {
        val total = seconds.coerceAtLeast(0.0).roundToInt()
        return String.format(Locale.US, "%02d:%02d:%02d", total / 3600, (total % 3600) / 60, total % 60)
    }

    private fun parseHms(value: String): Double {
        val parts = value.trim().split(':')
        if (parts.size != 3) return 0.0
        val hours = parts[0].toDoubleOrNull() ?: return 0.0
        val minutes = parts[1].toDoubleOrNull() ?: return 0.0
        val seconds = parts[2].toDoubleOrNull() ?: return 0.0
        return hours * 3600 + minutes * 60 + seconds
    }

    private val SERVICE = Regex("<service>(.*?)</service>", RegexOption.DOT_MATCHES_ALL)
}
