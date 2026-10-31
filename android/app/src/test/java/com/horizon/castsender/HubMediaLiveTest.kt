package com.horizon.castsender

import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.util.Base64

/**
 * Drives a real, running hub: `./gradlew testDebugUnitTest -PhorizonHub=192.168.1.5:7878`.
 * Without that property it skips itself, because a hub is an external service,
 * not a fixture.
 *
 * Everything this test adds is removed again, including the queue entries the
 * library deletion cascades into; the last assertion compares the library to the
 * count it had before the test so a silent leftover cannot pass.
 */
class HubMediaLiveTest {
    private val address: String? = System.getProperty("horizonHub")
    private val prefix = "安卓端验证-${System.currentTimeMillis()}-"

    @After
    fun tearDown() {
        // Cleanup lives in the test body's finally block: it needs the ids the
        // upload returned.
    }

    @Test
    fun theWholePhoneFlowRunsAgainstARealHub() {
        val where = address
        assumeTrue("horizonHub 未设置，跳过真机/真实主机联调", where != null)
        val host = where!!.substringBefore(':')
        val port = where.substringAfter(':', LanDiscovery.HUB_PORT.toString()).toIntOrNull()
            ?: LanDiscovery.HUB_PORT
        val client = HubMediaClient(host, port)

        val baseline = client.library().size
        println("[live] hub $address library baseline=$baseline")

        val wav = wavBytes(12)
        val png = PNG
        var cleaned = false
        try {
            // 1. Multi-file upload: two media files in one request.
            var lastPercent = -1L
            val items = client.upload(
                listOf(
                    UploadFile("${prefix}音频.wav", "audio/wav", wav.size.toLong()) { ByteArrayInputStream(wav) },
                    UploadFile("${prefix}图片.png", "image/png", png.size.toLong()) { ByteArrayInputStream(png) },
                ),
            ) { sent, total ->
                val percent = if (total > 0) sent * 100 / total else 0
                lastPercent = percent
            }
            assertEquals("一次请求应上传两项", 2, items.size)
            assertEquals(wav.size.toLong(), items[0].bytes)
            assertEquals(png.size.toLong(), items[1].bytes)
            assertEquals(100L, lastPercent)
            val wavItem = items[0]
            val pngItem = items[1]
            assertTrue("片库标题即文件名：${wavItem.title}", wavItem.title.startsWith(prefix))
            assertTrue("片库条目保留文件名：${pngItem.file}", pngItem.file.startsWith(prefix))
            assertEquals("上传条目的 source 就是它的 MIME", "image/png", pngItem.source)
            println("[live] upload items=${items.map { "${it.id}:${it.kind}:${it.bytes}" }}")

            // 2. The whole selection becomes the queue, first item playing.
            var queue = client.setQueue(items.map { it.id }, autoplay = true)
            assertEquals(2, queue.items.size)
            assertEquals(0, queue.current)
            assertEquals(wavItem.id, queue.currentItem?.id)
            assertTrue("setQueue 应开始播放", queue.playing)
            // The play address the hub hands a player must end in the file name,
            // or MediaEngine feeds a still photo to the video decoder.
            assertTrue(
                "队列地址应带文件名：${queue.items[0].uri}",
                queue.items[0].uri.contains("/media/${wavItem.id}/"),
            )
            val started = awaitPlaying(client, 15000, wavItem.id)
            assertNotNull("队列应真的进入播放态，而不是只停在请求回执上", started)
            println("[live] setQueue phase=${started!!.phase} position=${started.position}")

            // 3. Transport verbs on a timed item: pause and resume are reported
            //    by phase, so this is the real pause check. The native player
            //    applies commands on its own thread, so the snapshot a verb
            //    returns can still show the previous phase — the phone UI reads
            //    the queue through a 1 s poll, and so does this check.
            queue = client.transport("pause")
            assertTrue("暂停后 playing 必须立即为 false", !queue.playing)
            val paused = awaitPhase(client, "paused", 5000)
            assertNotNull("暂停后应读到 paused，实际停在 ${client.queue().phase}", paused)
            queue = client.transport("play")
            val resumed = awaitPhase(client, "playing", 15000)
            assertNotNull("恢复播放应回到 playing", resumed)
            println("[live] transport pause→play ok")

            // 4. A still image on its own — the address shape that once made the
            //    native player answer SRC_NOT_SUPPORTED.
            queue = client.setQueue(listOf(pngItem.id), autoplay = true)
            assertEquals(1, queue.items.size)
            val imagePlaying = awaitPlaying(client, 15000, pngItem.id)
            assertNotNull("图片应进入播放态（静帧靠停留时间前进）", imagePlaying)
            println("[live] image phase=${imagePlaying!!.phase} duration=${imagePlaying.duration}")

            // 5. Append, reorder, jump — the phone's management actions.
            queue = client.enqueue(listOf(wavItem.id), autoplay = false)
            assertEquals(2, queue.items.size)
            assertEquals("追加不应打断当前播放", pngItem.id, queue.currentItem?.id)

            queue = client.queueMove(1, 0)
            assertEquals(wavItem.id, queue.items[0].id)
            println("[live] queueMove order=${queue.items.map { it.kind }}")

            queue = client.jump(1)
            assertEquals(1, queue.current)
            assertEquals(pngItem.id, queue.currentItem?.id)

            // 6. Removing the playing image hands the queue the audio item; stop
            //    is asserted once that load has settled, so the verb is not
            //    racing a load the removal started.
            queue = client.queueRemove(pngItem.id)
            assertEquals(1, queue.items.size)
            assertEquals(wavItem.id, queue.items[0].id)
            val afterRemove = awaitPlaying(client, 15000, wavItem.id)
            assertNotNull("移除当前条目后队列应自动播放下一条", afterRemove)
            client.transport("stop")
            val stopped = awaitPhase(client, "stopped", 5000)
            assertNotNull("停止后应读到 stopped", stopped)
            assertTrue("停止后 playing 必须为 false", !stopped!!.playing)
            println("[live] queueRemove then stop ok")

            // 6. Delete the uploads; the queue cascades, the library returns to
            //    the size it had before this test.
            client.remove(wavItem.id)
            client.remove(pngItem.id)
            cleaned = true
            val remaining = client.library()
            assertEquals("片库应回到测试前的条数", baseline, remaining.size)
            assertTrue(
                "测试素材不能留在片库里：${remaining.map { it.title }}",
                remaining.none { it.title.startsWith(prefix) },
            )
            assertEquals("删除应连带清空队列", 0, client.queue().items.size)
            println("[live] cleanup library=$baseline queue=0")
        } finally {
            if (!cleaned) {
                // Best effort: a failed assertion must not leave fixtures behind.
                client.library()
                    .filter { it.title.startsWith(prefix) }
                    .forEach { runCatching { client.remove(it.id) } }
            }
            runCatching { client.transport("stop") }
        }
    }

    /**
     * Waits for playback that belongs to [expectId]: `state.uri` lags a new
     * queue load, so a bare `phase == "playing"` can still describe the track
     * that was playing before.
     */
    private fun awaitPlaying(client: HubMediaClient, timeoutMs: Long, expectId: String? = null): MediaQueue? {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            val queue = client.queue()
            val loaded = expectId == null || queue.uri.contains("/media/$expectId/")
            if (queue.phase == "playing" && loaded) return queue
            if (queue.phase == "error" || queue.halted != null) return null
            Thread.sleep(250)
        }
        return null
    }

    private fun awaitPhase(
        client: HubMediaClient,
        phase: String,
        timeoutMs: Long,
    ): MediaQueue? {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            val queue = client.queue()
            if (queue.phase == phase) return queue
            if (queue.phase == "error" && phase != "error") return null
            Thread.sleep(250)
        }
        return null
    }

    private fun wavBytes(seconds: Int, rate: Int = 8000): ByteArray {
        val samples = seconds * rate
        val out = ByteArrayOutputStream()
        fun le32(value: Int) {
            out.write(value and 0xFF)
            out.write((value shr 8) and 0xFF)
            out.write((value shr 16) and 0xFF)
            out.write((value shr 24) and 0xFF)
        }
        fun le16(value: Int) {
            out.write(value and 0xFF)
            out.write((value shr 8) and 0xFF)
        }
        out.write("RIFF".toByteArray())
        le32(36 + samples * 2)
        out.write("WAVE".toByteArray())
        out.write("fmt ".toByteArray())
        le32(16)
        le16(1)
        le16(1)
        le32(rate)
        le32(rate * 2)
        le16(2)
        le16(16)
        out.write("data".toByteArray())
        le32(samples * 2)
        for (index in 0 until samples) {
            val value = Math.round(6000 * Math.sin(index.toDouble() / rate * 2 * Math.PI * 440)).toInt()
            le16(value)
        }
        return out.toByteArray()
    }

    private companion object {
        val PNG: ByteArray = Base64.getDecoder().decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
        )
    }
}
