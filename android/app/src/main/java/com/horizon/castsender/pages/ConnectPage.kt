package com.horizon.castsender.pages

import android.graphics.Color
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView
import com.horizon.castsender.LanDiscovery
import com.horizon.castsender.MainActivity
import com.horizon.castsender.MirrorState
import com.horizon.castsender.Prefs
import com.horizon.castsender.Receiver
import com.horizon.castsender.ui.Brand
import com.horizon.castsender.ui.LineIcon
import com.horizon.castsender.ui.RadarView
import com.horizon.castsender.ui.Ui
import com.horizon.castsender.ui.dp
import com.horizon.castsender.ui.ripple
import com.horizon.castsender.ui.rounded
import com.horizon.castsender.ui.tap

class ConnectPage(activity: MainActivity) : Page(activity) {
    private val column = Ui.column(activity)
    override val root: View = Ui.page(activity, column)

    private val liveCard = Ui.card(activity, 20)
    private val setup = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val radar = RadarView(activity)
    private val scanState = Ui.text(activity, "正在搜索局域网…", 13f, Brand.NAVY, medium = true)
    private val scanDetail = Ui.text(activity, "", 11.5f, Brand.FAINT)
    private val listBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val emptyHint = Ui.body(
        activity,
        "没有发现大屏。请确认电脑已打开「浏览器投屏」，或手动填写地址。",
    )
    private val hostField = Ui.field(activity, "192.168.0.27:9407")
    private val codeField = Ui.codeField(activity)
    private val connectButton = Ui.button(activity, "开始投屏")
    private val recentBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val recentCard = Ui.card(activity)
    private val liveName = Ui.text(activity, "", 13f, Color.parseColor("#A9BDD6"))
    private val liveTitle = Ui.text(activity, "正在投屏", 20f, Color.WHITE, medium = true)
    private val liveResolution = Ui.text(activity, "", 11.5f, Color.parseColor("#A9BDD6"))
    private val statFps = Ui.stat(activity, "发送帧率", "—", dark = true)
    private val statRate = Ui.stat(activity, "码率", "—", dark = true)
    private val statCapture = Ui.stat(activity, "采集帧率", "—", dark = true)
    private val statDrop = Ui.stat(activity, "丢帧", "—", dark = true)
    private val liveNote = Ui.text(activity, "", 11f, Color.parseColor("#A9BDD6"))
    private val notice = Ui.text(activity, "", 12f, Brand.WARN, medium = true).apply {
        setPadding(dp(14), dp(11), dp(14), dp(11))
        visibility = View.GONE
    }

    private var selected: Receiver? = null
    private var wasStreaming = false
    private var started = false

    private val onNetwork: () -> Unit = {
        activity.runOnUiThread { renderNetwork() }
    }

    init {
        column.addView(
            Ui.heading(
                activity,
                "连接",
                "同一 Wi-Fi 下自动发现大屏，输入大屏上的 6 位投屏码即可开始。",
            ),
        )
        buildLiveCard()
        column.addView(liveCard)
        column.addView(notice)
        buildSetup()
        column.addView(setup)
        buildRecent()
        column.addView(recentCard)
        column.addView(
            Ui.body(
                activity,
                "投屏基于 HORIZON 浏览器投屏协议：屏幕在手机上编码为 H.264，局域网直连大屏解码，不上传云端。",
                Brand.FAINT,
            ).apply { setPadding(dp(4), dp(6), dp(4), 0) },
        )
        codeField.addTextChangedListener(object : android.text.TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit

            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit

            override fun afterTextChanged(s: android.text.Editable?) {
                connectButton.alpha = if ((s?.length ?: 0) == 6) 1f else 0.55f
            }
        })
        connectButton.alpha = 0.55f
        connectButton.tap { beginCasting() }
        renderLive()
        renderRecent()
    }

    // ---------------------------------------------------------------- build

    private fun buildLiveCard() {
        liveCard.background = activity.rounded(Brand.NAVY_CARD, 22)
        val top = Ui.row(activity)
        top.addView(Ui.dot(activity, Brand.GOOD, 8).apply {
            (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(8)
        })
        top.addView(liveTitle)
        top.addView(View(activity).apply {
            layoutParams = LinearLayout.LayoutParams(0, 1, 1f)
        })
        top.addView(Ui.pill(activity, "实时", Color.WHITE, Color.parseColor("#2E4C74")))
        liveCard.addView(top)
        liveCard.addView(liveName.apply { setPadding(0, dp(8), 0, 0) })
        liveCard.addView(liveResolution.apply { setPadding(0, dp(3), 0, dp(16)) })

        val grid = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            weightSum = 4f
        }
        listOf(statFps, statRate, statCapture, statDrop).forEach { cell ->
            cell.layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            grid.addView(cell)
        }
        liveCard.addView(grid)
        liveCard.addView(liveNote.apply { setPadding(0, dp(12), 0, dp(14)) })

        val stop = Ui.text(activity, "停止投屏", 14f, Brand.NAVY, medium = true).apply {
            gravity = Gravity.CENTER
            setPadding(dp(16), dp(13), dp(16), dp(13))
            background = activity.ripple(activity.rounded(Color.WHITE, 13))
            tap {
                com.horizon.castsender.MirrorService.stop(activity)
                MirrorState.idle("正在停止投屏…")
            }
        }
        liveCard.addView(stop)
        liveCard.visibility = View.GONE
    }

    private fun buildSetup() {
        setup.addView(Ui.sectionTitle(activity, "附近的大屏", "刷新") {
            LanDiscovery.start(force = true)
        })

        val card = Ui.card(activity)
        radar.layoutParams = LinearLayout.LayoutParams(dp(132), dp(132)).apply {
            gravity = Gravity.CENTER_HORIZONTAL
            topMargin = dp(6)
        }
        card.addView(radar)
        scanState.gravity = Gravity.CENTER
        scanState.setPadding(0, dp(10), 0, dp(2))
        card.addView(scanState)
        scanDetail.gravity = Gravity.CENTER
        card.addView(scanDetail)
        card.addView(listBox.apply { setPadding(0, dp(10), 0, 0) })
        emptyHint.gravity = Gravity.CENTER
        emptyHint.setPadding(dp(8), dp(12), dp(8), dp(4))
        card.addView(emptyHint)
        setup.addView(card)

        setup.addView(
            Ui.sectionTitle(activity, "投屏码", null).apply {
                setPadding(dp(2), dp(20), dp(2), dp(8))
            },
        )
        val connectCard = Ui.card(activity)
        connectCard.addView(
            Ui.body(activity, "大屏「浏览器投屏」面板上显示的 6 位数字。").apply {
                setPadding(0, 0, 0, dp(10))
            },
        )
        connectCard.addView(codeField)
        connectCard.addView(
            Ui.body(activity, "手动地址（可选，填了就不再使用上面选中的设备）").apply {
                setPadding(0, dp(14), 0, dp(8))
            },
        )
        connectCard.addView(hostField)
        connectButton.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        ).apply { topMargin = dp(14) }
        connectCard.addView(connectButton)
        setup.addView(connectCard)
    }

    private fun buildRecent() {
        recentCard.addView(Ui.sectionTitle(activity, "最近连接"))
        recentCard.addView(recentBox)
        recentCard.visibility = View.GONE
    }

    // --------------------------------------------------------------- render

    private fun renderNetwork() {
        val receivers = LanDiscovery.receivers
        if (selected == null && receivers.isNotEmpty()) selected = receivers.first()
        if (selected != null && receivers.none { it.address == selected!!.address }) {
            selected = receivers.firstOrNull()
        }
        radar.active = LanDiscovery.scanning && receivers.isEmpty()
        scanState.text = when {
            LanDiscovery.scanning -> "正在搜索局域网…"
            receivers.isEmpty() -> "没有发现接收端"
            else -> "发现 ${receivers.size} 台接收端"
        }
        scanDetail.text = if (LanDiscovery.scanning) {
            "已扫描 ${LanDiscovery.scanned} / ${LanDiscovery.total}"
        } else if (receivers.isNotEmpty()) {
            "点击设备选中，再输入投屏码"
        } else {
            "确认手机与电脑在同一 Wi-Fi"
        }
        emptyHint.visibility = if (receivers.isEmpty() && !LanDiscovery.scanning) View.VISIBLE else View.GONE

        listBox.removeAllViews()
        receivers.forEachIndexed { index, receiver ->
            if (index > 0) listBox.addView(Ui.divider(activity, 4, 4))
            listBox.addView(receiverRow(receiver))
        }
        renderRecent()
    }

    private fun receiverRow(receiver: Receiver): View {
        val chosen = selected?.address == receiver.address
        val row = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(12), dp(11), dp(12), dp(11))
            background = activity.ripple(
                if (chosen) activity.rounded(Brand.SOFT, 12, Brand.SKY)
                else activity.rounded(Color.TRANSPARENT, 12),
            )
            tap {
                selected = receiver
                hostField.setText("")
                renderNetwork()
            }
        }
        val copy = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        copy.addView(
            TextView(activity).apply {
                text = receiver.name
                setTextSize(android.util.TypedValue.COMPLEX_UNIT_SP, 14.5f)
                setTextColor(if (chosen) Brand.BLUE else Brand.NAVY)
                typeface = android.graphics.Typeface.create("sans-serif-medium", android.graphics.Typeface.NORMAL)
            },
        )
        copy.addView(
            Ui.text(
                activity,
                receiver.address + if (receiver.legacy) "  ·  旧版接收端" else "",
                11.5f,
                Brand.FAINT,
                mono = true,
            ).apply { setPadding(0, dp(3), 0, 0) },
        )
        row.addView(copy)
        if (chosen) {
            row.addView(
                Ui.text(activity, "已选", 11f, Brand.BLUE, medium = true).apply {
                    setPadding(dp(8), dp(4), dp(8), dp(4))
                    background = activity.rounded(Color.WHITE, 99, Brand.SKY)
                },
            )
        } else {
            row.addView(LineIcon(activity, LineIcon.Kind.CAST, Brand.FAINT, 20))
        }
        return row
    }

    private fun renderRecent() {
        val history = Prefs.history("mirror").take(4)
        recentCard.visibility = if (history.isEmpty()) View.GONE else View.VISIBLE
        recentBox.removeAllViews()
        history.forEachIndexed { index, entry ->
            if (index > 0) recentBox.addView(Ui.divider(activity, 2, 2))
            val row = LinearLayout(activity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, dp(10), 0, dp(10))
                background = activity.ripple(activity.rounded(Color.TRANSPARENT, 10))
                tap {
                    hostField.setText(entry.address)
                    val match = LanDiscovery.receivers.firstOrNull { it.address == entry.address }
                    if (match != null) selected = match
                    renderNetwork()
                    Ui.toast(activity, "已填入 ${entry.address}")
                }
            }
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(Ui.text(activity, entry.name, 13.5f, Brand.NAVY, medium = true))
            copy.addView(
                Ui.text(activity, entry.address, 11f, Brand.FAINT, mono = true).apply {
                    setPadding(0, dp(2), 0, 0)
                },
            )
            row.addView(copy)
            row.addView(Ui.text(activity, Ui.compactTime(System.currentTimeMillis() - entry.at), 11f, Brand.FAINT))
            recentBox.addView(row)
        }
    }

    private fun renderLive() {
        val active = MirrorState.streaming || MirrorState.reconnecting
        liveCard.visibility = if (active) View.VISIBLE else View.GONE
        setup.visibility = if (active) View.GONE else View.VISIBLE
        when {
            MirrorState.failed -> {
                notice.text = MirrorState.status
                notice.setTextColor(Brand.WARN)
                notice.background = activity.rounded(Brand.WARN_SOFT, 12)
                notice.visibility = View.VISIBLE
            }
            !MirrorState.streaming && MirrorState.status.contains("正在") -> {
                notice.text = MirrorState.status
                notice.setTextColor(Brand.BLUE)
                notice.background = activity.rounded(Brand.SOFT, 12)
                notice.visibility = View.VISIBLE
            }
            else -> notice.visibility = View.GONE
        }
        if (!MirrorState.streaming) {
            if (MirrorState.reconnecting) {
                liveResolution.text = MirrorState.status
                liveNote.text = "正在恢复连接，画面会自动回来"
                notice.visibility = View.GONE
            }
            return
        }
        val target = MirrorState.status.substringAfter(" · ", "")
        liveName.text = target.ifEmpty { "HORIZON 接收端" }
        liveResolution.text = MirrorState.resolution.ifEmpty { "正在建立编码器…" }
        (statFps.getChildAt(0) as TextView).text = "${MirrorState.fps}"
        (statRate.getChildAt(0) as TextView).text = "${MirrorState.kbps}"
        (statCapture.getChildAt(0) as TextView).text =
            if (MirrorState.captureFps >= 0) "${MirrorState.captureFps}" else "—"
        (statDrop.getChildAt(0) as TextView).text = "${MirrorState.drops}"
        liveNote.text = if (MirrorState.captureFps >= 0) {
            "采集 fps · 每帧换帧 ${"%.1f".format(MirrorState.swapMs)} ms · 编码 → 局域网直连"
        } else {
            "编码 → 局域网直连 · 大屏解码"
        }
    }

    // ---------------------------------------------------------------- cast

    private fun beginCasting() {
        if (MirrorState.streaming || MirrorState.reconnecting) {
            Ui.toast(activity, "已经在投屏了")
            return
        }
        val token = codeField.text.toString().trim()
        if (token.length != 6) {
            Ui.toast(activity, "请输入 6 位投屏码")
            codeField.requestFocus()
            return
        }
        val typed = hostField.text.toString().trim()
        val target = selected
        val host: String
        val port: Int
        val name: String
        if (typed.isNotEmpty()) {
            host = typed.substringBefore(':').trim()
            port = typed.substringAfter(':', "9407").trim().toIntOrNull() ?: 9407
            name = "手动地址"
        } else if (target != null) {
            host = target.host
            port = target.port
            name = target.name
        } else {
            Ui.toast(activity, "请选择接收端或填写地址")
            return
        }
        if (host.isEmpty()) {
            Ui.toast(activity, "地址不完整")
            return
        }
        activity.requestCapture(host, port, token, name)
    }

    // -------------------------------------------------------------- lifecycle

    override fun onShow() {
        LanDiscovery.addListener(onNetwork)
        if (!started) {
            started = true
            if (LanDiscovery.receivers.isEmpty() && !LanDiscovery.scanning) {
                LanDiscovery.start()
            }
        }
        renderNetwork()
        renderLive()
    }

    override fun onHide() {
        LanDiscovery.removeListener(onNetwork)
    }

    override fun onTick() {
        if (MirrorState.streaming != wasStreaming) {
            wasStreaming = MirrorState.streaming
            renderLive()
        }
        if (MirrorState.streaming || MirrorState.reconnecting) renderLive()
    }

    override fun onDestroy() {
        LanDiscovery.removeListener(onNetwork)
    }
}
