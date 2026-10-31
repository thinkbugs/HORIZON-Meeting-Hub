package com.horizon.castsender.pages

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.provider.OpenableColumns
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView
import com.horizon.castsender.HubClient
import com.horizon.castsender.LanDiscovery
import com.horizon.castsender.MainActivity
import com.horizon.castsender.Prefs
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

class TransferPage(activity: MainActivity) : Page(activity) {
    private val column = Ui.column(activity)
    override val root: View = Ui.page(activity, column)

    private val hubStatus = Ui.pill(activity, "未连接", Brand.MUTED, Brand.SOFT)
    private val hubPicker = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val hubBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val hubHint = Ui.body(activity, "正在搜索会议主机…")
    private val connectedBox = Ui.card(activity).apply { visibility = View.GONE }
    private val manualHub = Ui.field(activity, "192.168.0.27:7878")
    private val manualConnect = Ui.smallButton(activity, "连接")
    private val sendFile = Ui.button(activity, "选择文件发送")
    private val textField = Ui.field(activity, "发送一段文字到电脑")
    private val sendText = Ui.smallButton(activity, "发送")
    private val transferCard = Ui.card(activity).apply { visibility = View.GONE }
    private val transferName = Ui.text(activity, "", 14f, Brand.NAVY, medium = true)
    private val transferMeta = Ui.text(activity, "", 11.5f, Brand.MUTED)
    private val transferBar = Ui.progress(activity)
    private val transferResult = LinearLayout(activity).apply { orientation = LinearLayout.HORIZONTAL }
    private val transferResultText = Ui.text(activity, "", 13f, Brand.GOOD, medium = true)
    private val historyBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val historyCard = Ui.card(activity).apply { visibility = View.GONE }

    private var client: HubClient? = null
    private var connecting = false
    private var busy = false
    private var lastBytes = 0L
    private var lastAt = 0L
    private var speedBps = 0.0
    // The signalling socket dies while another app (or the system file picker)
    // pauses us; keep the intent so a return can reopen it instead of stranding
    // the user on "未连接".
    private var wantConnection = false
    private var afterConnect: (() -> Unit)? = null

    private val onNetwork: () -> Unit = {
        activity.runOnUiThread { renderHub() }
    }

    init {
        if (Prefs.lastHub.isNotEmpty()) manualHub.setText(Prefs.lastHub)
        column.addView(
            Ui.heading(
                activity,
                "传输",
                "照片、视频、文档直接走局域网发到会议电脑，不经过云端；文件落在电脑上设置的接收文件夹。",
            ),
        )
        buildHubCard()
        buildSendCard()
        buildTransferCard()
        buildHistory()
        renderHub()
        renderHistory()
    }

    // ---------------------------------------------------------------- build

    private fun buildHubCard() {
        val card = Ui.card(activity)
        val top = Ui.row(activity)
        top.addView(
            Ui.text(activity, "会议主机", 15f, Brand.NAVY, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        top.addView(hubStatus)
        card.addView(top)
        card.addView(
            Ui.body(activity, "连接电脑上的 HORIZON 文件服务（端口 7878）。").apply {
                setPadding(0, dp(5), 0, dp(12))
            },
        )
        hubPicker.addView(hubHint)
        hubPicker.addView(hubBox.apply { setPadding(0, dp(4), 0, 0) })
        val manual = Ui.row(activity).apply { setPadding(0, dp(12), 0, 0) }
        manual.addView(
            manualHub,
            LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                rightMargin = dp(8)
            },
        )
        manual.addView(manualConnect)
        hubPicker.addView(manual)
        card.addView(hubPicker)

        connectedBox.background = activity.rounded(Brand.SOFT, 14)
        connectedBox.setPadding(dp(14), dp(14), dp(14), dp(14))
        val info = Ui.row(activity)
        info.addView(Ui.dot(activity, Brand.GOOD, 8).apply {
            (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(8)
        })
        val copy = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        copy.addView(Ui.text(activity, "已连接", 13.5f, Brand.NAVY, medium = true))
        copy.addView(
            Ui.text(activity, "${Prefs.lastHub} · 电脑接收文件夹", 11f, Brand.MUTED, mono = true).apply {
                setPadding(0, dp(3), 0, 0)
            },
        )
        info.addView(copy)
        info.addView(
            Ui.text(activity, "断开", 12f, Brand.BLUE, medium = true).apply {
                setPadding(dp(6), dp(6), 0, dp(6))
                tap { disconnect(notify = true) }
            },
        )
        connectedBox.addView(info)
        card.addView(connectedBox)

        column.addView(card)

        manualConnect.tap { connectTo(manualHub.text.toString()) }
        sendFile.tap { pickFiles() }
        sendText.tap { sendTextMessage() }
        renderSendState()
    }

    private fun buildSendCard() {
        column.addView(
            Ui.sectionTitle(activity, "发送").apply {
                setPadding(dp(2), dp(20), dp(2), dp(8))
            },
        )
        val card = Ui.card(activity)
        sendFile.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        )
        card.addView(sendFile)
        card.addView(
            Ui.body(activity, "支持任意类型文件，可一次选择多个；每个文件都会保留原文件名。").apply {
                setPadding(dp(2), dp(9), dp(2), dp(14))
            },
        )
        val row = Ui.row(activity)
        row.addView(
            textField,
            LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                rightMargin = dp(8)
            },
        )
        row.addView(sendText)
        card.addView(row)
        column.addView(card)
    }

    private fun buildTransferCard() {
        transferCard.addView(transferResult)
        transferResult.addView(LineIcon(activity, LineIcon.Kind.FILE, Brand.BLUE, 20).apply {
            (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(8)
        })
        transferResult.addView(transferResultText)
        transferCard.addView(transferName.apply { setPadding(0, dp(6), 0, dp(3)) })
        transferCard.addView(transferMeta)
        transferCard.addView(transferBar.apply {
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                dp(6),
            ).apply { topMargin = dp(12) }
        })
        column.addView(Ui.cardSpacing(activity, transferCard, 12))
    }

    private fun buildHistory() {
        historyCard.addView(Ui.sectionTitle(activity, "传输记录"))
        historyCard.addView(historyBox)
        column.addView(Ui.cardSpacing(activity, historyCard, 12))
    }

    // --------------------------------------------------------------- render

    private fun renderHub() {
        val active = client != null && !connecting
        hubStatus.text = when {
            connecting -> "连接中"
            active -> "已连接"
            else -> "未连接"
        }
        hubStatus.setTextColor(
            when {
                active -> Brand.GOOD
                connecting -> Brand.BLUE
                else -> Brand.MUTED
            },
        )
        hubStatus.background = activity.rounded(
            when {
                active -> Brand.GOOD_SOFT
                else -> Brand.SOFT
            },
            99,
        )
        hubPicker.visibility = if (active) View.GONE else View.VISIBLE
        connectedBox.visibility = if (active) View.VISIBLE else View.GONE
        if (active) {
            renderSendState()
            return
        }
        val hubs = LanDiscovery.hubs
        hubHint.visibility = if (hubs.isEmpty()) View.VISIBLE else View.GONE
        hubHint.text = when {
            LanDiscovery.scanning -> "正在搜索会议主机…"
            hubs.isEmpty() -> "没有发现会议主机。确认电脑上的 HORIZON 已启动，或在下面填写地址。"
            else -> ""
        }
        hubBox.removeAllViews()
        hubs.forEachIndexed { index, hub ->
            if (index > 0) hubBox.addView(Ui.divider(activity, 2, 2))
            val row = LinearLayout(activity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(2), dp(11), dp(2), dp(11))
                background = activity.ripple(activity.rounded(Color.TRANSPARENT, 10))
                tap {
                    if (!connecting) connectTo("${hub.host}:${hub.port}")
                }
            }
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(Ui.text(activity, "HORIZON 会议主机", 14f, Brand.NAVY, medium = true))
            copy.addView(
                Ui.text(activity, "${hub.host}:${hub.port}  ·  文件服务", 11f, Brand.FAINT, mono = true).apply {
                    setPadding(0, dp(3), 0, 0)
                },
            )
            row.addView(copy)
            row.addView(LineIcon(activity, LineIcon.Kind.TRANSFER, Brand.BLUE, 18))
            hubBox.addView(row)
        }
        renderSendState()
    }

    private fun renderSendState() {
        val ready = client != null && !connecting
        listOf(sendFile, sendText).forEach { view ->
            view.isEnabled = ready && !busy
            view.alpha = if (view.isEnabled) 1f else 0.45f
        }
        textField.isEnabled = ready && !busy
        manualConnect.isEnabled = !connecting
        manualConnect.alpha = if (connecting) 0.45f else 1f
    }

    private fun renderHistory() {
        val history = Prefs.history("transfer").take(6)
        historyCard.visibility = if (history.isEmpty()) View.GONE else View.VISIBLE
        historyBox.removeAllViews()
        history.forEachIndexed { index, entry ->
            if (index > 0) historyBox.addView(Ui.divider(activity, 2, 2))
            val row = Ui.row(activity).apply { setPadding(0, dp(9), 0, dp(9)) }
            row.addView(LineIcon(activity, LineIcon.Kind.FILE, Brand.FAINT, 18).apply {
                (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(9)
            })
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(Ui.text(activity, entry.name, 13f, Brand.NAVY, medium = true))
            copy.addView(
                Ui.text(activity, entry.address, 10.5f, Brand.FAINT, mono = true).apply {
                    setPadding(0, dp(2), 0, 0)
                },
            )
            row.addView(copy)
            row.addView(Ui.text(activity, Ui.compactTime(System.currentTimeMillis() - entry.at), 10.5f, Brand.FAINT))
            historyBox.addView(row)
        }
    }

    // ------------------------------------------------------------ connection

    private fun connectTo(address: String, quiet: Boolean = false) {
        val trimmed = address.trim()
        if (trimmed.isEmpty()) {
            if (!quiet) Ui.toast(activity, "请输入会议主机地址")
            return
        }
        val host = trimmed.substringBefore(':').trim()
        val port = trimmed.substringAfter(':', "7878").trim().toIntOrNull() ?: 7878
        if (host.isEmpty()) {
            if (!quiet) Ui.toast(activity, "地址不完整")
            return
        }
        disconnect(notify = false)
        connecting = true
        wantConnection = true
        renderHub()
        val listener = object : HubClient.Listener {
            override fun onPeers(hubId: String, hubName: String, peerCount: Int) {
                activity.runOnUiThread {
                    connecting = false
                    renderHub()
                    renderSendState()
                    runAfterConnect()
                }
            }

            override fun onProgress(progress: Double) = Unit

            override fun onComplete() = Unit

            override fun onClosed(reason: String) {
                activity.runOnUiThread {
                    if (client?.address == "$host:$port") {
                        client = null
                        connecting = false
                        connectedBox.visibility = View.GONE
                        renderHub()
                        renderSendState()
                        if (!busy && !quiet) Ui.toast(activity, reason)
                    }
                }
            }
        }
        val target = HubClient(host, port, listener)
        client = target
        Thread({
            target.connect()
            val opened = target.awaitConnected(7000)
            var waited = 0
            while (opened && target.hubId == null && waited < 4000) {
                Thread.sleep(50)
                waited += 50
            }
            activity.runOnUiThread {
                if (client !== target) return@runOnUiThread
                connecting = false
                if (!opened || target.hubId == null) {
                    target.close()
                    client = null
                    renderHub()
                    if (!quiet) Ui.toast(activity, "无法连接会议主机，请检查地址与电脑端 HORIZON")
                } else {
                    Prefs.lastHub = "$host:$port"
                    renderHub()
                    if (!quiet) Ui.toast(activity, "已连接会议主机")
                    runAfterConnect()
                }
            }
        }, "hub-open").start()
    }

    private fun runAfterConnect() {
        val next = afterConnect ?: return
        afterConnect = null
        next()
    }

    /** The picker pauses us, so the socket is gone when the files come back. */
    private fun ensureConnected(then: () -> Unit) {
        val target = client
        if (target != null && target.isConnected()) {
            then()
            return
        }
        target?.close()
        client = null
        val address = Prefs.lastHub.ifEmpty { LanDiscovery.hubs.firstOrNull()?.let { "${it.host}:${it.port}" } ?: "" }
        if (address.isEmpty()) {
            Ui.toast(activity, "先连接会议主机")
            return
        }
        afterConnect = then
        connectTo(address, quiet = true)
    }

    override fun onActivityResume() {
        val target = client
        if (target != null && !target.isConnected()) {
            target.close()
            client = null
            connecting = false
            renderHub()
            renderSendState()
        }
        if (wantConnection && client == null && !connecting) {
            val address = Prefs.lastHub.ifEmpty { return }
            connectTo(address, quiet = true)
        }
    }

    private fun disconnect(notify: Boolean) {
        wantConnection = false
        client?.close()
        client = null
        connecting = false
        if (notify) {
            renderHub()
            renderSendState()
            Ui.toast(activity, "已断开连接")
        }
    }

    // ----------------------------------------------------------------- send

    private fun pickFiles() {
        if (client == null) {
            Ui.toast(activity, "先连接会议主机")
            return
        }
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
            putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        }
        @Suppress("DEPRECATION")
        activity.startActivityForResult(Intent.createChooser(intent, "选择要发送的文件"), REQUEST_FILES)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (requestCode != REQUEST_FILES || resultCode != Activity.RESULT_OK) return
        val uris = mutableListOf<Uri>()
        data?.clipData?.let { clip ->
            for (index in 0 until clip.itemCount) uris.add(clip.getItemAt(index).uri)
        }
        data?.data?.let { if (uris.isEmpty()) uris.add(it) }
        if (uris.isNotEmpty()) ensureConnected { sendFiles(uris) }
    }

    private fun sendFiles(uris: List<Uri>) {
        val target = client ?: return
        if (busy) {
            Ui.toast(activity, "正在发送，请稍候")
            return
        }
        busy = true
        renderSendState()
        transferCard.visibility = View.VISIBLE
        transferResultText.text = "正在发送"
        transferResultText.setTextColor(Brand.BLUE)
        transferBar.progress = 0
        transferMeta.text = "0%"
        Thread({
            var completed = 0
            var failed: String? = null
            var lastName = ""
            for ((index, uri) in uris.withIndex()) {
                val meta = queryMeta(uri)
                lastName = meta.name
                activity.runOnUiThread {
                    transferName.text = meta.name
                    transferMeta.text = "第 ${index + 1} / ${uris.size} 个 · ${Ui.bytes(meta.size)}"
                }
                lastBytes = 0
                lastAt = System.currentTimeMillis()
                speedBps = 0.0
                val result = try {
                    val cached = meta.file
                    val payload: String = cached?.name ?: meta.name
                    val opened = if (cached != null) FileInputStream(cached) else openStream(uri) ?: continue
                    val size = if (cached != null) cached.length() else meta.size
                    target.sendFile(payload, meta.mime, size, opened) { bytes ->
                        val now = System.currentTimeMillis()
                        val elapsed = (now - lastAt).coerceAtLeast(1)
                        if (elapsed >= 250) {
                            speedBps = (bytes - lastBytes) * 1000.0 / elapsed
                            lastBytes = bytes
                            lastAt = now
                            val fraction = if (size > 0) (bytes.toDouble() / size).coerceIn(0.0, 1.0) else 0.0
                            val overall = (index + fraction) / uris.size
                            activity.runOnUiThread {
                                transferBar.progress = (overall * 100).toInt()
                                transferMeta.text = "第 ${index + 1} / ${uris.size} 个 · " +
                                    "${Ui.bytes(bytes)} / ${Ui.bytes(size)} · ${Ui.bytes(speedBps.toLong())}/s"
                            }
                        }
                    }
                } catch (error: Exception) {
                    false
                }
                meta.file?.delete()
                if (result) {
                    completed += 1
                    Prefs.record("transfer", meta.name, target.address)
                } else {
                    failed = meta.name
                    break
                }
            }
            busy = false
            activity.runOnUiThread {
                renderSendState()
                renderHistory()
                transferBar.progress = if (failed == null) 100 else transferBar.progress
                if (failed == null) {
                    transferResultText.text = "发送完成 · $completed 个文件"
                    transferResultText.setTextColor(Brand.GOOD)
                    transferName.text = lastName
                    transferMeta.text = "已保存到电脑的接收文件夹"
                } else {
                    transferResultText.text = "发送失败 · $failed"
                    transferResultText.setTextColor(Brand.WARN)
                    transferMeta.text = "检查与会议主机的连接后重试"
                }
            }
        }, "hub-send").start()
    }

    private fun sendTextMessage() {
        val target = client
        if (target == null) {
            Ui.toast(activity, "先连接会议主机")
            return
        }
        val text = textField.text.toString().trim()
        if (text.isEmpty()) return
        textField.setText("")
        // sendText writes to the socket, which StrictMode forbids on the UI thread.
        Thread({
            val sent = target.sendText(text)
            activity.runOnUiThread {
                if (!sent) {
                    textField.setText(text)
                    Ui.toast(activity, "发送失败")
                } else {
                    Prefs.record("transfer", "文字：${text.take(16)}", target.address)
                    renderHistory()
                    Ui.toast(activity, "文字已发送到电脑")
                }
            }
        }, "hub-text").start()
    }

    // --------------------------------------------------------------- files

    private data class Meta(
        val name: String,
        val mime: String,
        val size: Long,
        val file: File? = null,
    )

    private fun queryMeta(uri: Uri): Meta {
        var name = "文件"
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
        val mime = activity.contentResolver.getType(uri)
            ?: com.horizon.castsender.MediaServer.guessMime(name)
        if (size > 0) return Meta(name, mime, size)
        // Providers that do not report a size force a local copy: the hub must
        // know the byte count up front to decide when the file is complete.
        val cached = File(activity.cacheDir, "outgoing-${System.currentTimeMillis()}-${name.takeLast(40)}")
        return try {
            activity.contentResolver.openInputStream(uri)?.use { input ->
                FileOutputStream(cached).use { output -> input.copyTo(output) }
            }
            Meta(name, mime, cached.length(), cached)
        } catch (_: Exception) {
            Meta(name, mime, 0)
        }
    }

    private fun openStream(uri: Uri) =
        try {
            activity.contentResolver.openInputStream(uri)
        } catch (_: Exception) {
            null
        }

    // ----------------------------------------------------------- lifecycle

    override fun onShow() {
        LanDiscovery.addListener(onNetwork)
        if (LanDiscovery.hubs.isEmpty() && !LanDiscovery.scanning) LanDiscovery.start()
        renderHub()
        renderHistory()
    }

    override fun onHide() {
        LanDiscovery.removeListener(onNetwork)
    }

    override fun onTick() {
        renderSendState()
    }

    override fun onDestroy() {
        LanDiscovery.removeListener(onNetwork)
        disconnect(notify = false)
    }

    private companion object {
        const val REQUEST_FILES = 2001
    }
}
