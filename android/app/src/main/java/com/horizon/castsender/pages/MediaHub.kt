package com.horizon.castsender.pages

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.provider.OpenableColumns
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import com.horizon.castsender.HubMediaClient
import com.horizon.castsender.HubException
import com.horizon.castsender.LanDiscovery
import com.horizon.castsender.LibraryItem
import com.horizon.castsender.MainActivity
import com.horizon.castsender.MediaQueue
import com.horizon.castsender.MediaServer
import com.horizon.castsender.Prefs
import com.horizon.castsender.UploadFile
import com.horizon.castsender.ui.Brand
import com.horizon.castsender.ui.LineIcon
import com.horizon.castsender.ui.Ui
import com.horizon.castsender.ui.dp
import com.horizon.castsender.ui.ripple
import com.horizon.castsender.ui.rounded
import com.horizon.castsender.ui.tap
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream

/**
 * The phone's side of the projection library: multi-file upload, the hub's
 * queue with reordering, and the hub library itself. Same HTTP actions the
 * browser extension posts, so a phone, the extension and the big screen all
 * drive one queue instead of three ideas of what is playing.
 */
internal class MediaHub(private val activity: MainActivity) {
    val root: LinearLayout = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }

    private val hubPill = Ui.pill(activity, "未连接", Brand.MUTED, Brand.SOFT)
    private val hubHint = Ui.body(activity, "正在查找会议主机…")
    private val hubBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val manualHub = Ui.field(activity, "192.168.1.5:7878")
    private val connectButton = Ui.smallButton(activity, "连接")
    private val connectedBox = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        background = activity.rounded(Brand.SOFT, 14)
        setPadding(activity.dp(14), activity.dp(12), activity.dp(14), activity.dp(12))
        visibility = View.GONE
    }
    private val connectedText = Ui.text(activity, "", 11.5f, Brand.MUTED, mono = true)
    private val changeHub = Ui.text(activity, "更换主机", 12f, Brand.BLUE, medium = true).apply {
        setPadding(activity.dp(6), activity.dp(6), 0, activity.dp(6))
        tap { disconnect() }
    }

    private val pickButton = Ui.softButton(activity, "选择媒体（可多选）")
    private val pendingBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val pendingEmpty = Ui.body(activity, "还没有选择媒体。一次可以选择多个视频、音乐或图片，它们的顺序就是队列顺序。")
    private val playPicked = Ui.button(activity, "播放所选")
    private val appendPicked = Ui.button(activity, "追加到队列", primary = false)
    private val uploadBar = Ui.progress(activity).apply { visibility = View.GONE }
    private val uploadNote = Ui.text(activity, "", 11.5f, Brand.MUTED).apply { visibility = View.GONE }

    private val libraryPill = Ui.pill(activity, "—", Brand.MUTED, Brand.SOFT)
    private val libraryBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val libraryEmpty = Ui.body(activity, "片库还没有文件。投一条手机媒体进来，或在电脑上导入。")

    private val queuePill = Ui.pill(activity, "—", Brand.MUTED, Brand.SOFT)
    private val nowDot = Ui.dot(activity, Brand.FAINT, 8)
    private val nowTitle = Ui.text(activity, "没有正在播放的条目", 13.5f, Brand.NAVY, medium = true)
    private val nowState = Ui.text(activity, "未连接", 11f, Brand.FAINT)
    private val playPause = Ui.smallButton(activity, "播放")
    private val stopButton = Ui.smallButton(activity, "停止")
    private val queueNote = Ui.text(activity, "", 11.5f, Brand.MUTED).apply { visibility = View.GONE }
    private val queueBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val queueEmpty = Ui.body(activity, "队列是空的。投一条媒体，或在片库里点 ▶。")

    private var client: HubMediaClient? = null
    private var connecting = false
    private var connectToken = 0
    private var busy = false
    private var acting = false
    private var visible = false
    private var queue: MediaQueue? = null
    private var library: List<LibraryItem>? = null
    private var pending = listOf<Picked>()
    private var lastPoll = 0L
    private val tempFiles = mutableListOf<File>()

    private val onNetwork: () -> Unit = {
        activity.runOnUiThread { if (client == null) renderHub() }
    }

    private data class Picked(val uri: Uri, val name: String, val mime: String, val size: Long)

    init {
        build()
    }

    // ---------------------------------------------------------------- build

    private fun build() {
        root.addView(
            Ui.sectionTitle(activity, "大屏片库 · 播放队列").apply {
                setPadding(activity.dp(2), activity.dp(22), activity.dp(2), activity.dp(8))
            },
        )
        buildHubCard()
        buildUploadCard()
        buildLibraryCard()
        buildQueueCard()
        renderHub()
        renderPending()
        renderLibrary()
        renderQueue()
    }

    private fun buildHubCard() {
        val card = Ui.card(activity)
        val top = Ui.row(activity)
        top.addView(
            Ui.text(activity, "片库主机", 15f, Brand.NAVY, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        top.addView(hubPill)
        card.addView(top)
        card.addView(
            Ui.body(activity, "电影、音乐和照片在电脑上排队播放；端口取自接收端 /info，不必自己填。").apply {
                setPadding(0, activity.dp(5), 0, activity.dp(10))
            },
        )
        card.addView(hubHint)
        card.addView(hubBox.apply { setPadding(0, activity.dp(4), 0, 0) })
        val manual = Ui.row(activity).apply { setPadding(0, activity.dp(12), 0, 0) }
        manual.addView(
            manualHub,
            LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                rightMargin = activity.dp(8)
            },
        )
        manual.addView(connectButton)
        card.addView(manual)

        connectedBox.addView(Ui.text(activity, "已连接", 13.5f, Brand.GOOD, medium = true))
        connectedBox.addView(connectedText.apply { setPadding(0, activity.dp(3), 0, 0) })
        connectedBox.addView(changeHub)
        card.addView(connectedBox)
        root.addView(Ui.cardSpacing(activity, card, 0))

        connectButton.tap { connect(manualHub.text.toString()) }
        if (Prefs.lastHub.isNotEmpty()) manualHub.setText(Prefs.lastHub)
    }

    private fun buildUploadCard() {
        val card = Ui.card(activity)
        card.addView(Ui.text(activity, "投放手机媒体", 15f, Brand.NAVY, medium = true))
        card.addView(
            Ui.body(activity, "文件先进入大屏片库，再从片库地址播放；一次可以选多个，顺序即投放顺序。").apply {
                setPadding(0, activity.dp(5), 0, activity.dp(12))
            },
        )
        card.addView(pickButton)
        card.addView(pendingEmpty.apply { setPadding(activity.dp(2), activity.dp(10), activity.dp(2), 0) })
        card.addView(pendingBox.apply { setPadding(0, activity.dp(4), 0, 0) })
        val actions = Ui.row(activity).apply { setPadding(0, activity.dp(12), 0, 0) }
        actions.addView(
            playPicked,
            LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                rightMargin = activity.dp(8)
            },
        )
        actions.addView(appendPicked, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        card.addView(actions)
        card.addView(uploadBar.apply { setPadding(0, activity.dp(10), 0, 0) })
        card.addView(uploadNote.apply { setPadding(activity.dp(2), activity.dp(6), activity.dp(2), 0) })
        root.addView(Ui.cardSpacing(activity, card, 12))

        pickButton.tap { pick() }
        playPicked.tap { upload(append = false) }
        appendPicked.tap { upload(append = true) }
    }

    private fun buildLibraryCard() {
        val card = Ui.card(activity)
        val row = Ui.row(activity)
        row.addView(
            Ui.text(activity, "片库", 15f, Brand.NAVY, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        row.addView(libraryPill)
        row.addView(
            Ui.text(activity, "刷新", 12f, Brand.BLUE, medium = true).apply {
                setPadding(activity.dp(12), activity.dp(4), 0, activity.dp(4))
                tap { refreshLibrary() }
            },
        )
        card.addView(row)
        card.addView(
            Ui.body(activity, "▶ 只投这一条，＋ 追加到队尾，✕ 从片库删除。").apply {
                setPadding(0, activity.dp(5), 0, activity.dp(8))
            },
        )
        card.addView(libraryBox)
        card.addView(libraryEmpty.apply { setPadding(activity.dp(2), activity.dp(8), activity.dp(2), 0) })
        root.addView(Ui.cardSpacing(activity, card, 12))
    }

    private fun buildQueueCard() {
        val card = Ui.card(activity)
        val row = Ui.row(activity)
        row.addView(
            Ui.text(activity, "播放队列", 15f, Brand.NAVY, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        row.addView(queuePill)
        card.addView(row)

        val now = Ui.row(activity).apply { setPadding(0, activity.dp(10), 0, 0) }
        now.addView(nowDot.apply { (layoutParams as LinearLayout.LayoutParams).rightMargin = activity.dp(8) })
        val copy = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        copy.addView(nowTitle)
        copy.addView(nowState.apply { setPadding(0, activity.dp(2), 0, 0) })
        now.addView(copy)
        card.addView(now)

        val transport = Ui.row(activity).apply { setPadding(0, activity.dp(8), 0, 0) }
        transport.addView(
            playPause.apply {
                layoutParams = LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                ).apply { rightMargin = activity.dp(8) }
            },
        )
        transport.addView(stopButton)
        card.addView(transport)
        card.addView(queueNote.apply { setPadding(activity.dp(2), activity.dp(6), activity.dp(2), 0) })
        card.addView(queueBox.apply { setPadding(0, activity.dp(4), 0, 0) })
        card.addView(queueEmpty.apply { setPadding(activity.dp(2), activity.dp(8), activity.dp(2), 0) })
        root.addView(Ui.cardSpacing(activity, card, 12))

        playPause.tap { transport(if (queue?.playing == true) "pause" else "play", if (queue?.playing == true) "已暂停" else "开始播放") }
        stopButton.tap { transport("stop", "已停止") }
    }

    // -------------------------------------------------------------- connect

    private fun renderHub() {
        val ready = client != null
        hubPill.text = when {
            connecting -> "连接中"
            ready -> "已就绪"
            else -> "未连接"
        }
        hubPill.setTextColor(
            when {
                ready -> Brand.GOOD
                connecting -> Brand.BLUE
                else -> Brand.MUTED
            },
        )
        hubPill.background = activity.rounded(if (ready) Brand.GOOD_SOFT else Brand.SOFT, 99)
        connectButton.isEnabled = !connecting
        connectButton.alpha = if (connecting) 0.45f else 1f
        connectButton.text = if (connecting) "连接中…" else "连接"
        connectedBox.visibility = if (ready) View.VISIBLE else View.GONE
        client?.let { connectedText.text = "${it.address} · 片库与队列" }
        manualHub.visibility = if (ready) View.GONE else View.VISIBLE
        connectButton.visibility = if (ready) View.GONE else View.VISIBLE
        if (ready || connecting) {
            hubBox.removeAllViews()
            hubHint.visibility = View.GONE
            return
        }
        val hubs = LanDiscovery.hubs
        hubHint.visibility = if (hubs.isEmpty()) View.VISIBLE else View.GONE
        if (hubs.isEmpty()) {
            hubHint.text = when {
                LanDiscovery.scanning -> "正在查找会议主机…"
                else -> "没有发现会议主机。确认电脑上的 HORIZON 已启动，或在下面填写地址。"
            }
        }
        hubBox.removeAllViews()
        hubs.forEachIndexed { index, hub ->
            if (index > 0) hubBox.addView(Ui.divider(activity, 2, 2))
            val row = LinearLayout(activity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(activity.dp(2), activity.dp(10), activity.dp(2), activity.dp(10))
                background = activity.ripple(activity.rounded(Color.TRANSPARENT, 10))
                tap { if (!connecting) connect("${hub.host}:${hub.port}") }
            }
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(Ui.text(activity, "HORIZON 会议主机", 14f, Brand.NAVY, medium = true))
            copy.addView(
                Ui.text(activity, "${hub.host}:${hub.port}  ·  片库与队列", 11f, Brand.FAINT, mono = true).apply {
                    setPadding(0, activity.dp(3), 0, 0)
                },
            )
            row.addView(copy)
            row.addView(LineIcon(activity, LineIcon.Kind.MEDIA, Brand.BLUE, 18))
            hubBox.addView(row)
        }
    }

    private fun connect(address: String, quiet: Boolean = false) {
        val trimmed = address.trim()
        if (trimmed.isEmpty()) {
            if (!quiet) Ui.toast(activity, "请输入会议主机地址")
            return
        }
        val host = trimmed.substringBefore(':').trim()
        val port = trimmed.substringAfter(':', LanDiscovery.HUB_PORT.toString()).trim().toIntOrNull()
            ?: LanDiscovery.HUB_PORT
        if (host.isEmpty()) {
            if (!quiet) Ui.toast(activity, "地址不完整")
            return
        }
        val token = ++connectToken
        connecting = true
        renderHub()
        Thread({
            val candidate = HubMediaClient(host, port)
            val outcome = try {
                // A hub that answers the library is the one that can also queue;
                // an open port that speaks something else is not this service.
                Result.success(candidate.library() to candidate.queue())
            } catch (error: Exception) {
                Result.failure(error)
            }
            activity.runOnUiThread {
                if (token != connectToken) return@runOnUiThread
                connecting = false
                val value = outcome.getOrNull()
                if (value == null) {
                    renderHub()
                    val message = (outcome.exceptionOrNull() as? HubException)?.message
                        ?: "无法连接会议主机"
                    if (quiet) {
                        hubHint.visibility = View.VISIBLE
                        hubHint.text = "上次的会议主机没有响应：$message"
                    } else {
                        Ui.toast(activity, message)
                    }
                    return@runOnUiThread
                }
                client = candidate
                Prefs.lastHub = "${candidate.host}:${candidate.port}"
                library = value.first
                queue = value.second
                manualHub.setText(Prefs.lastHub)
                renderHub()
                renderPending()
                renderLibrary()
                renderQueue()
                if (!quiet) Ui.toast(activity, "已连接 ${candidate.address}")
            }
        }, "hub-media-open").start()
    }

    private fun disconnect() {
        client = null
        connecting = false
        connectToken++
        queue = null
        library = null
        renderHub()
        renderPending()
        renderLibrary()
        renderQueue()
    }

    // --------------------------------------------------------------- upload

    private fun pick() {
        if (client == null) {
            Ui.toast(activity, "先连接会议主机")
            return
        }
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
            putExtra(Intent.EXTRA_MIME_TYPES, arrayOf("video/*", "audio/*", "image/*"))
            putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        }
        @Suppress("DEPRECATION")
        activity.startActivityForResult(Intent.createChooser(intent, "选择要投放的媒体"), REQUEST_LIBRARY)
    }

    fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (requestCode != REQUEST_LIBRARY || resultCode != Activity.RESULT_OK) return
        val uris = mutableListOf<Uri>()
        data?.clipData?.let { clip ->
            for (index in 0 until clip.itemCount) uris.add(clip.getItemAt(index).uri)
        }
        data?.data?.let { if (uris.isEmpty()) uris.add(it) }
        if (uris.isNotEmpty()) stage(uris)
    }

    private fun stage(uris: List<Uri>) {
        val added = mutableListOf<Picked>()
        var skipped = 0
        for (uri in uris) {
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
                skipped++
                continue
            }
            added.add(Picked(uri, name, mime, size))
        }
        if (added.isEmpty()) {
            Ui.toast(activity, "请选择视频、音乐或图片")
            return
        }
        if (skipped > 0) Ui.toast(activity, "已跳过 $skipped 个非媒体文件")
        pending = pending + added
        renderPending()
    }

    private fun renderPending() {
        pendingEmpty.visibility = if (pending.isEmpty()) View.VISIBLE else View.GONE
        pendingBox.removeAllViews()
        val ready = client != null && !busy
        listOf(playPicked, appendPicked).forEach {
            it.isEnabled = ready && pending.isNotEmpty()
            it.alpha = if (it.isEnabled) 1f else 0.45f
        }
        pickButton.isEnabled = client != null && !busy
        pickButton.alpha = if (pickButton.isEnabled) 1f else 0.45f
        pending.forEachIndexed { index, file ->
            if (index > 0) pendingBox.addView(Ui.divider(activity, 2, 2))
            val row = Ui.row(activity).apply { setPadding(0, activity.dp(8), 0, activity.dp(8)) }
            row.addView(LineIcon(activity, LineIcon.Kind.FILE, Brand.FAINT, 18).apply {
                (layoutParams as LinearLayout.LayoutParams).rightMargin = activity.dp(9)
            })
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(Ui.text(activity, file.name, 13f, Brand.NAVY, medium = true))
            copy.addView(
                Ui.text(
                    activity,
                    listOf(if (file.size > 0) Ui.bytes(file.size) else "大小未知", kindLabel(file.mime)).joinToString(" · "),
                    10.5f,
                    Brand.FAINT,
                ).apply { setPadding(0, activity.dp(2), 0, 0) },
            )
            row.addView(copy)
            row.addView(op("✕", Brand.WARN) {
                pending = pending.filterIndexed { at, _ -> at != index }
                renderPending()
            })
            pendingBox.addView(row)
        }
    }

    private fun upload(append: Boolean) {
        val target = client ?: run {
            Ui.toast(activity, "先连接会议主机")
            return
        }
        if (pending.isEmpty() || busy) return
        val selection = pending
        busy = true
        uploadBar.visibility = View.VISIBLE
        uploadBar.progress = 0
        uploadNote.visibility = View.VISIBLE
        uploadNote.setTextColor(Brand.BLUE)
        uploadNote.text = "正在上传 ${selection.size} 个文件…"
        renderPending()
        Thread({
            try {
                val uploads = selection.map { prepare(it) }
                var shownAt = 0L
                val items = target.upload(uploads) { sent, total ->
                    val now = System.currentTimeMillis()
                    if (now - shownAt < 200) return@upload
                    shownAt = now
                    val percent = if (total > 0) (sent * 100 / total).toInt() else 0
                    activity.runOnUiThread {
                        uploadBar.progress = percent
                        uploadNote.text = "正在上传 ${selection.size} 个文件 · ${Ui.bytes(sent)} / ${Ui.bytes(total)}"
                    }
                }
                val ids = items.map { it.id }
                val fresh = if (append) target.enqueue(ids, autoplay = false) else target.setQueue(ids, autoplay = true)
                val items2 = target.library()
                activity.runOnUiThread {
                    busy = false
                    pending = emptyList()
                    queue = fresh
                    library = items2
                    uploadBar.visibility = View.GONE
                    uploadNote.visibility = View.GONE
                    renderPending()
                    renderLibrary()
                    renderQueue()
                    val label = if (append) "已追加 ${items.size} 条到队列" else "已开始播放 ${items.size} 条"
                    note(label, Brand.GOOD)
                    Ui.toast(activity, label)
                }
            } catch (error: Exception) {
                val message = (error as? HubException)?.message ?: "上传失败"
                activity.runOnUiThread {
                    busy = false
                    uploadBar.visibility = View.GONE
                    uploadNote.visibility = View.VISIBLE
                    uploadNote.setTextColor(Brand.WARN)
                    uploadNote.text = "上传失败：$message"
                    renderPending()
                    note("没有投送成功：$message", Brand.WARN)
                }
            } finally {
                synchronized(tempFiles) {
                    tempFiles.forEach { it.delete() }
                    tempFiles.clear()
                }
            }
        }, "hub-media-upload").start()
    }

    private fun prepare(file: Picked): UploadFile {
        var size = file.size
        var path: File? = null
        if (size <= 0) {
            // The body is streamed against a fixed Content-Length, so a provider
            // that will not report a size has to be copied first.
            val cached = File(activity.cacheDir, "hub-${System.currentTimeMillis()}-${file.name.takeLast(36)}")
            try {
                activity.contentResolver.openInputStream(file.uri)?.use { input ->
                    FileOutputStream(cached).use { output -> input.copyTo(output) }
                }
            } catch (_: Exception) {
            }
            if (!cached.exists() || cached.length() <= 0) throw HubException("无法读取 ${file.name}")
            synchronized(tempFiles) { tempFiles.add(cached) }
            size = cached.length()
            path = cached
        }
        val source = path
        return UploadFile(file.name, file.mime, size) {
            if (source != null) FileInputStream(source) else openSource(file.uri)
        }
    }

    private fun openSource(uri: Uri): InputStream? =
        try {
            activity.contentResolver.openInputStream(uri)
        } catch (_: Exception) {
            null
        }

    // -------------------------------------------------------------- library

    private fun refreshLibrary() {
        val target = client ?: return
        Thread({
            try {
                val items = target.library()
                activity.runOnUiThread {
                    if (client === target) {
                        library = items
                        renderLibrary()
                    }
                }
            } catch (_: Exception) {
            }
        }, "hub-library").start()
    }

    private fun renderLibrary() {
        val items = library
        libraryPill.text = items?.let { "${it.size} 项" } ?: "—"
        libraryEmpty.visibility = if (items == null || items.isEmpty()) View.VISIBLE else View.GONE
        libraryEmpty.text = when {
            client == null -> "连接会议主机后显示片库。"
            items == null -> "正在读取片库…"
            else -> "片库还没有文件。投一条手机媒体进来，或在电脑上导入。"
        }
        libraryBox.removeAllViews()
        items?.forEachIndexed { index, item ->
            if (index > 0) libraryBox.addView(Ui.divider(activity, 2, 2))
            val row = Ui.row(activity).apply { setPadding(0, activity.dp(8), 0, activity.dp(8)) }
            row.addView(kindChip(item.kind))
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(activity.dp(8), 0, activity.dp(4), 0)
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            val title = Ui.text(activity, item.title, 13f, Brand.NAVY, medium = true)
            title.maxLines = 1
            copy.addView(title)
            copy.addView(
                Ui.text(
                    activity,
                    "${Ui.bytes(item.bytes)} · ${kindLabel(item.kind)}",
                    10.5f,
                    Brand.FAINT,
                ).apply { setPadding(0, activity.dp(2), 0, 0) },
            )
            row.addView(copy)
            row.addView(op("▶", Brand.BLUE) {
                act({ it.setQueue(listOf(item.id), autoplay = true) }, "已开始播放：${item.title}")
            })
            row.addView(op("＋", Brand.BLUE) {
                act({ it.enqueue(listOf(item.id), autoplay = false) }, "已追加：${item.title}")
            })
            row.addView(op("✕", Brand.WARN) { confirmRemove(item) })
            libraryBox.addView(row)
        }
    }

    private fun confirmRemove(item: LibraryItem) {
        AlertDialog.Builder(activity)
            .setTitle("从片库删除")
            .setMessage("「${item.title}」会从电脑片库中删除，队列里的这一条也一并移除。")
            .setNegativeButton("取消", null)
            .setPositiveButton("删除") { _, _ ->
                if (acting) return@setPositiveButton
                acting = true
                val target = client
                if (target == null) {
                    acting = false
                    return@setPositiveButton
                }
                Thread({
                    try {
                        val fresh = target.remove(item.id)
                        val items = target.library()
                        activity.runOnUiThread {
                            acting = false
                            queue = fresh
                            library = items
                            renderLibrary()
                            renderQueue()
                            note("已从片库删除：${item.title}", Brand.MUTED)
                        }
                    } catch (error: Exception) {
                        val message = (error as? HubException)?.message ?: "删除失败"
                        activity.runOnUiThread {
                            acting = false
                            renderLibrary()
                            note(message, Brand.WARN)
                        }
                    }
                }, "hub-library-remove").start()
            }
            .show()
    }

    // ---------------------------------------------------------------- queue

    private fun refreshQueue() {
        val target = client ?: return
        lastPoll = System.currentTimeMillis()
        Thread({
            try {
                val fresh = target.queue()
                activity.runOnUiThread {
                    if (client === target) {
                        queue = fresh
                        renderQueue()
                    }
                }
            } catch (error: Exception) {
                activity.runOnUiThread {
                    if (client === target) {
                        client = null
                        queue = null
                        library = null
                        renderHub()
                        renderQueue()
                        note("与会议主机的连接已断开", Brand.WARN)
                    }
                }
            }
        }, "hub-queue").start()
    }

    private fun act(block: (HubMediaClient) -> MediaQueue, done: String? = null) {
        val target = client ?: run {
            Ui.toast(activity, "先连接会议主机")
            return
        }
        if (acting) return
        acting = true
        renderQueue()
        renderLibrary()
        Thread({
            try {
                val fresh = block(target)
                activity.runOnUiThread {
                    acting = false
                    queue = fresh
                    if (done != null) note(done, Brand.GOOD)
                    renderQueue()
                    renderLibrary()
                }
            } catch (error: Exception) {
                val message = (error as? HubException)?.message ?: "操作失败"
                activity.runOnUiThread {
                    acting = false
                    note(message, Brand.WARN)
                    renderQueue()
                    renderLibrary()
                }
            }
        }, "hub-act").start()
    }

    private fun transport(action: String, done: String) {
        if (queue?.currentItem == null) {
            Ui.toast(activity, "队列里没有正在播放的条目")
            return
        }
        act({ it.transport(action) }, done)
    }

    private fun renderQueue() {
        val current = queue
        queuePill.text = current?.let { "${it.items.size} 项" } ?: "—"
        val item = current?.currentItem
        nowDot.background = activity.rounded(if (current?.playing == true) Brand.GOOD else Brand.FAINT, 99)
        nowTitle.text = item?.title ?: if (current == null) "没有正在播放的条目" else "队列已暂停或结束"
        nowState.text = when {
            current == null -> "未连接"
            current.halted != null -> "队列自行停止：${current.halted}"
            else -> {
                val phase = phaseLabel(current.phase)
                if (current.duration > 0) {
                    "$phase · ${Ui.hms(current.position)} / ${Ui.hms(current.duration)}"
                } else {
                    phase
                }
            }
        }
        playPause.text = if (current?.playing == true) "暂停" else "播放"
        val ready = current != null && !acting
        playPause.isEnabled = ready
        stopButton.isEnabled = ready
        playPause.alpha = if (ready) 1f else 0.45f
        stopButton.alpha = if (ready) 1f else 0.45f
        queueEmpty.visibility = if (current == null || current.items.isEmpty()) View.VISIBLE else View.GONE
        queueEmpty.text = when {
            current == null -> "连接会议主机后显示队列。"
            else -> "队列是空的。投一条媒体，或在片库里点 ▶。"
        }
        queueBox.removeAllViews()
        current?.items?.forEachIndexed { index, track ->
            if (index > 0) queueBox.addView(Ui.divider(activity, 2, 2))
            val playingRow = current.current == index
            val row = Ui.row(activity).apply {
                setPadding(activity.dp(2), activity.dp(8), activity.dp(2), activity.dp(8))
                background = activity.ripple(
                    if (playingRow) activity.rounded(Brand.SOFT, 10) else activity.rounded(Color.TRANSPARENT, 10),
                )
                tap { act({ it.jump(index) }, "已切到：${track.title}") }
            }
            row.addView(kindChip(track.kind))
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(activity.dp(8), 0, activity.dp(4), 0)
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            val title = Ui.text(activity, track.title, 13f, if (playingRow) Brand.BLUE else Brand.NAVY, medium = true)
            title.maxLines = 1
            copy.addView(title)
            copy.addView(
                Ui.text(
                    activity,
                    (if (playingRow) "正在播放" else "第 ${index + 1} 项") + (if (track.local) "" else " · 外链"),
                    10.5f,
                    Brand.FAINT,
                ).apply { setPadding(0, activity.dp(2), 0, 0) },
            )
            row.addView(copy)
            row.addView(op("↑", Brand.MUTED, enabled = index > 0) {
                act({ it.queueMove(index, index - 1) }, "队列顺序已更新")
            })
            row.addView(op("↓", Brand.MUTED, enabled = index < current.items.size - 1) {
                act({ it.queueMove(index, index + 1) }, "队列顺序已更新")
            })
            row.addView(op("⇥", Brand.MUTED, enabled = index != current.current) {
                act({ it.setNext(track.id) }, "下一条：${track.title}")
            })
            row.addView(op("✕", Brand.WARN) {
                act({ it.queueRemove(track.id) }, "已从队列移除：${track.title}")
            })
            queueBox.addView(row)
        }
    }

    // ---------------------------------------------------------------- parts

    private fun op(label: String, color: Int, enabled: Boolean = true, action: () -> Unit): View {
        val size = activity.dp(30)
        val view = Ui.text(activity, label, 13f, color, medium = true).apply {
            gravity = Gravity.CENTER
            layoutParams = LinearLayout.LayoutParams(size, size)
            background = activity.ripple(activity.rounded(Brand.SOFT, 9))
            isEnabled = enabled
            alpha = if (enabled) 1f else 0.35f
            tap { if (isEnabled) action() }
        }
        return view
    }

    private fun kindChip(kind: String): View =
        Ui.text(activity, kindLabel(kind), 10.5f, Brand.BLUE, medium = true).apply {
            gravity = Gravity.CENTER
            setPadding(activity.dp(7), activity.dp(3), activity.dp(7), activity.dp(3))
            background = activity.rounded(Brand.SOFT, 99)
        }

    private fun kindLabel(kind: String): String = when {
        kind.startsWith("video") -> "视频"
        kind.startsWith("audio") -> "音乐"
        kind.startsWith("image") -> "图片"
        else -> kind
    }

    private fun phaseLabel(phase: String): String = when (phase) {
        "idle" -> "空闲"
        "loading" -> "加载中"
        "playing" -> "正在播放"
        "paused" -> "已暂停"
        "stopped" -> "已停止"
        "ended" -> "播完"
        "error" -> "播放出错"
        else -> phase
    }

    private fun note(text: String, color: Int) {
        queueNote.visibility = View.VISIBLE
        queueNote.setTextColor(color)
        queueNote.text = text
    }

    // ------------------------------------------------------------ lifecycle

    fun onShow() {
        visible = true
        LanDiscovery.addListener(onNetwork)
        if (LanDiscovery.hubs.isEmpty() && !LanDiscovery.scanning) LanDiscovery.start()
        if (client == null && !connecting && Prefs.lastHub.isNotEmpty()) connect(Prefs.lastHub, quiet = true)
        renderHub()
    }

    fun onHide() {
        visible = false
        LanDiscovery.removeListener(onNetwork)
    }

    fun onTick() {
        if (!visible || client == null || busy || acting) return
        if (System.currentTimeMillis() - lastPoll < 1000) return
        refreshQueue()
    }

    fun onDestroy() {
        visible = false
        LanDiscovery.removeListener(onNetwork)
    }

    private companion object {
        const val REQUEST_LIBRARY = 3002
    }
}
