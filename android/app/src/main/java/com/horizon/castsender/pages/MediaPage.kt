package com.horizon.castsender.pages

import android.app.Activity
import android.content.Intent
import android.content.res.ColorStateList
import android.graphics.Color
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.SeekBar
import android.widget.TextView
import com.horizon.castsender.Dlna
import com.horizon.castsender.MainActivity
import com.horizon.castsender.MediaServer
import com.horizon.castsender.ui.Brand
import com.horizon.castsender.ui.LineIcon
import com.horizon.castsender.ui.Ui
import com.horizon.castsender.ui.dp
import com.horizon.castsender.ui.ripple
import com.horizon.castsender.ui.rounded
import com.horizon.castsender.ui.tap
import java.io.FileInputStream
import java.io.FilterInputStream
import java.io.InputStream

/**
 * DLNA control: the phone is the controller and the media server; the desktop
 * renderer fetches the file over HTTP and paints it in its own video window.
 */
class MediaPage(activity: MainActivity) : Page(activity) {
    private val column = Ui.column(activity)
    override val root: View = Ui.page(activity, column)

    private val devicePill = Ui.pill(activity, "未搜索", Brand.MUTED, Brand.SOFT)
    private val deviceBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val deviceHint = Ui.body(
        activity,
        "没有发现 DLNA 设备。请在大屏「投屏」面板打开「DLNA 视频接收」后重新搜索。",
    )
    private val scanButton = Ui.smallButton(activity, "搜索设备")
    private val pickButton = Ui.softButton(activity, "选择媒体")
    private val mediaName = Ui.text(activity, "还没有选择媒体", 13.5f, Brand.NAVY, medium = true)
    private val mediaMeta = Ui.text(activity, "支持视频、音乐和图片", 11.5f, Brand.FAINT)
    private val castButton = Ui.button(activity, "投到大屏")
    private val castHint = Ui.text(activity, "", 11.5f, Brand.WARN)
    private val nowCard = Ui.card(activity).apply { visibility = View.GONE }
    private val nowName = Ui.text(activity, "", 14.5f, Brand.NAVY, medium = true)
    private val nowState = Ui.pill(activity, "—", Brand.MUTED, Brand.SOFT)
    private val seek = seekBar(1000)
    private val timeLabel = Ui.text(activity, "00:00 / 00:00", 11f, Brand.FAINT, mono = true)
    private val playPause = Ui.softButton(activity, "暂停")
    private val stopButton = Ui.text(activity, "停止", 12.5f, Brand.MUTED, medium = true).apply {
        gravity = Gravity.CENTER
        setPadding(dp(16), dp(11), dp(16), dp(11))
        background = activity.ripple(activity.rounded(Color.TRANSPARENT, 11))
        tap { stopCasting() }
    }
    private val volumeRow = Ui.row(activity)
    private val volumeIcon = LineIcon(activity, LineIcon.Kind.SOUND, Brand.MUTED, 18)
    private val volumeBar = seekBar(100)

    private var renderers = emptyList<Dlna.Dmr>()
    private var selected: Dlna.Dmr? = null
    private var scanning = false
    private var scannedOnce = false
    private var mediaUri: Uri? = null
    private var mediaNameValue = ""
    private var mediaMime = "application/octet-stream"
    private var mediaSize = 0L
    private val server = MediaServer()
    private val hub = MediaHub(activity)
    private var active = false
    private var lastState = ""
    private var duration = 0.0
    private var seeking = false
    private var syncingVolume = false
    @Volatile private var polling = false
    private var pollThread: Thread? = null

    init {
        build()
        renderDevices()
        updateCastState()
    }

    // ---------------------------------------------------------------- build

    private fun build() {
        column.addView(
            Ui.heading(
                activity,
                "媒体",
                "照片、视频和音乐投到大屏：片库主机上可以排成播放队列，也可以不走片库直接 DLNA 投一条。",
            ),
        )
        column.addView(hub.root)
        column.addView(
            Ui.sectionTitle(activity, "DLNA 直投").apply {
                setPadding(dp(2), dp(22), dp(2), dp(8))
            },
        )
        val deviceCard = Ui.card(activity)
        val top = Ui.row(activity)
        top.addView(
            Ui.text(activity, "播放设备", 15f, Brand.NAVY, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        top.addView(devicePill)
        deviceCard.addView(top)
        deviceCard.addView(
            Ui.body(
                activity,
                "大屏需要先打开「投屏 → DLNA 视频接收」；开启后这里会列出它。",
            ).apply { setPadding(0, dp(5), 0, dp(10)) },
        )
        deviceCard.addView(scanButton)
        deviceCard.addView(deviceBox.apply { setPadding(0, dp(4), 0, 0) })
        deviceCard.addView(deviceHint.apply { setPadding(dp(2), dp(10), dp(2), 0) })
        column.addView(deviceCard)

        val mediaCard = Ui.card(activity)
        mediaCard.addView(pickButton)
        val mediaInfo = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(2), dp(12), dp(2), dp(4))
        }
        mediaInfo.addView(mediaName)
        mediaInfo.addView(mediaMeta.apply { setPadding(0, dp(3), 0, 0) })
        mediaCard.addView(mediaInfo)
        castButton.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        ).apply { topMargin = dp(10) }
        mediaCard.addView(castButton)
        mediaCard.addView(castHint.apply { setPadding(dp(2), dp(8), dp(2), 0) })
        column.addView(mediaCard)

        buildNowCard()
        column.addView(Ui.cardSpacing(activity, nowCard, 12))

        scanButton.tap { scan() }
        pickButton.tap { pickMedia() }
        castButton.tap { cast() }
    }

    private fun buildNowCard() {
        val top = Ui.row(activity)
        top.addView(nowName.apply {
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        })
        top.addView(nowState)
        nowCard.addView(top)
        nowCard.addView(seek.apply {
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
            ).apply { topMargin = dp(10) }
        })
        nowCard.addView(timeLabel.apply {
            width = ViewGroup.LayoutParams.MATCH_PARENT
            gravity = Gravity.END
        })
        val controls = Ui.row(activity).apply { setPadding(0, dp(8), 0, 0) }
        playPause.tap { togglePlay() }
        controls.addView(playPause, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
            rightMargin = dp(8)
        })
        controls.addView(stopButton)
        nowCard.addView(controls)

        volumeRow.apply { setPadding(0, dp(12), 0, 0) }
        volumeRow.addView(volumeIcon.apply {
            (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(6)
        })
        volumeRow.addView(volumeBar, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        nowCard.addView(volumeRow)
        nowCard.addView(
            Ui.body(activity, "音量由大屏播放器执行；设备不支持时会自动隐藏。", Brand.FAINT).apply {
                setPadding(0, dp(4), 0, 0)
            },
        )

        seek.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(bar: SeekBar?, progress: Int, fromUser: Boolean) = Unit

            override fun onStartTrackingTouch(bar: SeekBar?) {
                seeking = true
            }

            override fun onStopTrackingTouch(bar: SeekBar?) {
                val dmr = selected ?: return
                val target = (bar?.progress ?: 0) / 1000.0 * duration
                seeking = false
                Thread({ Dlna.seek(dmr, target) }, "dlna-seek").start()
            }
        })
        volumeBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(bar: SeekBar?, progress: Int, fromUser: Boolean) = Unit

            override fun onStartTrackingTouch(bar: SeekBar?) = Unit

            override fun onStopTrackingTouch(bar: SeekBar?) {
                val dmr = selected ?: return
                val value = bar?.progress ?: return
                syncingVolume = true
                Thread({
                    Dlna.setVolume(dmr, value)
                    val actual = Dlna.volume(dmr)
                    activity.runOnUiThread {
                        syncingVolume = false
                        if (actual != null) volumeBar.progress = actual
                    }
                }, "dlna-volume").start()
            }
        })
    }

    private fun seekBar(max: Int): SeekBar = SeekBar(activity).apply {
        this.max = max
        progress = 0
        progressTintList = ColorStateList.valueOf(Brand.BLUE)
        progressBackgroundTintList = ColorStateList.valueOf(Brand.SOFT)
        thumbTintList = ColorStateList.valueOf(Brand.BLUE)
        splitTrack = false
    }

    // -------------------------------------------------------------- devices

    private fun scan() {
        if (scanning) return
        scanning = true
        renderDevices()
        Thread({
            val found = Dlna.discover(3500)
            activity.runOnUiThread {
                scanning = false
                scannedOnce = true
                renderers = found
                if (selected == null || found.none { it.location == selected?.location }) {
                    selected = found.firstOrNull()
                }
                renderDevices()
                if (found.isEmpty()) {
                    castHint.text = Dlna.lastError ?: "没有发现 DLNA 设备"
                } else {
                    castHint.text = ""
                }
            }
        }, "dlna-scan").start()
    }

    private fun renderDevices() {
        devicePill.text = when {
            scanning -> "搜索中"
            renderers.isEmpty() -> "未搜索"
            else -> "${renderers.size} 台"
        }
        devicePill.setTextColor(if (renderers.isEmpty()) Brand.MUTED else Brand.GOOD)
        devicePill.background = activity.rounded(
            if (renderers.isEmpty()) Brand.SOFT else Brand.GOOD_SOFT,
            99,
        )
        scanButton.isEnabled = !scanning
        scanButton.alpha = if (scanning) 0.5f else 1f
        scanButton.text = if (scanning) "搜索中…" else "搜索设备"
        deviceHint.visibility =
            if (renderers.isEmpty() && !scanning && scannedOnce) View.VISIBLE else View.GONE
        deviceBox.removeAllViews()
        renderers.forEachIndexed { index, dmr ->
            if (index > 0) deviceBox.addView(Ui.divider(activity, 2, 2))
            val chosen = selected?.location == dmr.location
            val row = LinearLayout(activity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(2), dp(10), dp(2), dp(10))
                background = activity.ripple(
                    if (chosen) activity.rounded(Brand.SOFT, 10, Brand.SKY)
                    else activity.rounded(Color.TRANSPARENT, 10),
                )
                tap {
                    selected = dmr
                    renderDevices()
                    updateCastState()
                }
            }
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(
                TextView(activity).apply {
                    text = dmr.name
                    setTextSize(android.util.TypedValue.COMPLEX_UNIT_SP, 14f)
                    setTextColor(if (chosen) Brand.BLUE else Brand.NAVY)
                    typeface = android.graphics.Typeface.create("sans-serif-medium", android.graphics.Typeface.NORMAL)
                },
            )
            copy.addView(
                Ui.text(activity, dmr.host, 11f, Brand.FAINT, mono = true).apply {
                    setPadding(0, dp(2), 0, 0)
                },
            )
            row.addView(copy)
            row.addView(LineIcon(activity, LineIcon.Kind.SCREEN, if (chosen) Brand.BLUE else Brand.FAINT, 18))
            deviceBox.addView(row)
        }
        updateCastState()
    }

    // ---------------------------------------------------------------- media

    private fun pickMedia() {
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
            putExtra(Intent.EXTRA_MIME_TYPES, arrayOf("video/*", "audio/*", "image/*"))
        }
        @Suppress("DEPRECATION")
        activity.startActivityForResult(Intent.createChooser(intent, "选择要投放的媒体"), REQUEST_MEDIA)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        hub.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_MEDIA || resultCode != Activity.RESULT_OK) return
        val uri = data?.data ?: return
        var name = "媒体"
        var size = -1L
        try {
            activity.contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val nameIndex = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (nameIndex >= 0) name = cursor.getString(nameIndex) ?: name
                    val sizeIndex = cursor.getColumnIndex(OpenableColumns.SIZE)
                    if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) size = cursor.getLong(sizeIndex)
                }
            }
        } catch (_: Exception) {
        }
        val mime = activity.contentResolver.getType(uri) ?: MediaServer.guessMime(name)
        if (!mime.startsWith("video/") && !mime.startsWith("audio/") && !mime.startsWith("image/")) {
            Ui.toast(activity, "请选择视频、音乐或图片")
            return
        }
        mediaUri = uri
        mediaNameValue = name
        mediaMime = mime
        mediaSize = size
        mediaName.text = name
        mediaMeta.text = listOf(
            if (size > 0) Ui.bytes(size) else "大小未知",
            mime,
        ).joinToString(" · ")
        castHint.text = ""
        updateCastState()
    }

    private fun cast() {
        val dmr = selected
        val uri = mediaUri
        if (dmr == null) {
            Ui.toast(activity, "先选择播放设备")
            return
        }
        if (uri == null) {
            Ui.toast(activity, "先选择要投放的媒体")
            return
        }
        castButton.isEnabled = false
        castButton.text = "正在准备…"
        castHint.text = ""
        stopPolling()
        Thread({
            val item = MediaServer.Item(
                name = mediaNameValue,
                mime = mediaMime,
                size = mediaSize,
                open = { offset -> openMedia(uri, offset) },
            )
            val url = server.start(item)
            if (url == null) {
                activity.runOnUiThread {
                    castButton.text = "投到大屏"
                    castHint.text = "无法启动手机端媒体服务"
                    updateCastState()
                }
                return@Thread
            }
            val prepared = Dlna.setUri(dmr, url, mediaMime, mediaNameValue)
            val started = prepared && Dlna.play(dmr)
            val volume = if (started) Dlna.volume(dmr) else null
            activity.runOnUiThread {
                castButton.text = "投到大屏"
                if (!started) {
                    castHint.text = Dlna.lastError ?: "大屏拒绝了这次投放"
                    updateCastState()
                    server.stop()
                    return@runOnUiThread
                }
                active = true
                lastState = "PLAYING"
                duration = 0.0
                nowName.text = mediaNameValue
                nowCard.visibility = View.VISIBLE
                nowState.text = "加载中"
                nowState.setTextColor(Brand.BLUE)
                nowState.background = activity.rounded(Brand.SOFT, 99)
                playPause.text = "暂停"
                volumeRow.visibility = if (dmr.renderingControl == null) View.GONE else View.VISIBLE
                if (volume != null) {
                    syncingVolume = true
                    volumeBar.progress = volume
                    syncingVolume = false
                }
                updateCastState()
                startPolling()
                Ui.toast(activity, "已投到 ${dmr.name}")
            }
        }, "dlna-cast").start()
    }

    private fun openMedia(uri: Uri, offset: Long): InputStream? {
        return try {
            val descriptor = activity.contentResolver.openFileDescriptor(uri, "r")
            if (descriptor == null) {
                activity.contentResolver.openInputStream(uri)?.skipFully(offset)
            } else {
                val stream = FileInputStream(descriptor.fileDescriptor)
                try {
                    if (offset > 0) stream.channel.position(offset)
                    wrap(stream, descriptor)
                } catch (_: Exception) {
                    stream.close()
                    descriptor.close()
                    activity.contentResolver.openInputStream(uri)?.skipFully(offset)
                }
            }
        } catch (_: Exception) {
            null
        }
    }

    private fun wrap(stream: FileInputStream, descriptor: ParcelFileDescriptor): InputStream =
        object : FilterInputStream(stream) {
            override fun close() {
                try {
                    super.close()
                } finally {
                    try {
                        descriptor.close()
                    } catch (_: Exception) {
                    }
                }
            }
        }

    private fun InputStream.skipFully(offset: Long): InputStream {
        var left = offset
        while (left > 0) {
            val skipped = skip(left)
            if (skipped <= 0) break
            left -= skipped
        }
        return this
    }

    private fun updateCastState() {
        if (active) {
            castButton.isEnabled = true
            castButton.alpha = 1f
            return
        }
        val ready = selected != null && mediaUri != null
        castButton.isEnabled = ready
        castButton.alpha = if (ready) 1f else 0.45f
    }

    // ------------------------------------------------------------- playback

    private fun togglePlay() {
        val dmr = selected ?: return
        val playing = lastState == "PLAYING"
        Thread({
            val ok = if (playing) Dlna.pause(dmr) else Dlna.play(dmr)
            if (!ok) activity.runOnUiThread { Ui.toast(activity, Dlna.lastError ?: "操作失败") }
        }, "dlna-toggle").start()
    }

    private fun stopCasting() {
        val dmr = selected
        stopPolling()
        active = false
        nowCard.visibility = View.GONE
        server.stop()
        updateCastState()
        if (dmr != null) Thread({ Dlna.stop(dmr) }, "dlna-stop").start()
    }

    private fun startPolling() {
        stopPolling()
        polling = true
        pollThread = Thread({
            while (polling) {
                val dmr = selected
                if (dmr == null) break
                val position = Dlna.position(dmr)
                if (position != null) {
                    activity.runOnUiThread {
                        if (polling) renderPosition(position)
                    }
                }
                try {
                    Thread.sleep(1000)
                } catch (_: InterruptedException) {
                    break
                }
            }
        }, "dlna-poll").apply {
            isDaemon = true
            start()
        }
    }

    private fun stopPolling() {
        polling = false
        pollThread?.interrupt()
        pollThread = null
    }

    private fun renderPosition(position: Dlna.Position) {
        if (position.state.isNotEmpty()) lastState = position.state
        val playing = position.playing
        playPause.text = if (playing) "暂停" else "播放"
        nowState.text = when (position.state) {
            "PLAYING" -> "播放中"
            "PAUSED_PLAYBACK" -> "已暂停"
            "STOPPED", "NO_MEDIA_PRESENT" -> "已停止"
            else -> "加载中"
        }
        nowState.setTextColor(
            when (position.state) {
                "PLAYING" -> Brand.GOOD
                "PAUSED_PLAYBACK" -> Brand.BLUE
                else -> Brand.MUTED
            },
        )
        nowState.background = activity.rounded(
            when (position.state) {
                "PLAYING" -> Brand.GOOD_SOFT
                "PAUSED_PLAYBACK" -> Brand.SOFT
                else -> Brand.SOFT
            },
            99,
        )
        if (position.duration > 0) duration = position.duration
        if (!seeking) {
            val fraction = if (duration > 0) (position.seconds / duration).coerceIn(0.0, 1.0) else 0.0
            seek.progress = (fraction * 1000).toInt()
            timeLabel.text = "${Ui.hms(position.seconds)} / ${Ui.hms(duration)}"
        }
        if (position.state == "STOPPED") {
            playPause.text = "播放"
        }
    }

    // ------------------------------------------------------------ lifecycle

    override fun onShow() {
        hub.onShow()
        if (!scannedOnce && renderers.isEmpty() && !scanning) scan()
        if (active) startPolling()
    }

    override fun onHide() {
        hub.onHide()
        stopPolling()
    }

    override fun onTick() {
        hub.onTick()
    }

    override fun onDestroy() {
        hub.onDestroy()
        stopPolling()
        server.stop()
        val dmr = selected
        if (active && dmr != null && lastState != "STOPPED") Dlna.stop(dmr)
        active = false
    }

    private companion object {
        const val REQUEST_MEDIA = 3001
    }
}
