package com.horizon.castsender.pages

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Color
import android.os.Build
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView
import com.horizon.castsender.Discovery
import com.horizon.castsender.HistoryEntry
import com.horizon.castsender.MainActivity
import com.horizon.castsender.MirrorState
import com.horizon.castsender.Prefs
import com.horizon.castsender.ui.Brand
import com.horizon.castsender.ui.LineIcon
import com.horizon.castsender.ui.Ui
import com.horizon.castsender.ui.dp
import com.horizon.castsender.ui.rounded
import com.horizon.castsender.ui.tap

class MinePage(activity: MainActivity) : Page(activity) {
    private class Choice(val value: Int, val label: String)

    private val column = Ui.column(activity)
    override val root: View = Ui.page(activity, column)

    private val quality = listOf(Choice(1920, "1080p"), Choice(1280, "720p"))
    private val rates = listOf(Choice(30, "30 fps"), Choice(60, "60 fps"))
    private val repaints = mutableListOf<() -> Unit>()

    private val selfPill = Ui.pill(activity, "待连接", Brand.MUTED, Brand.SOFT)
    private val settingsNote = Ui.body(activity, "", Brand.FAINT)
    private val deviceIp = Ui.text(activity, "—", 12.5f, Brand.NAVY, mono = true)
    private val historyBox = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private val clearAction = Ui.text(activity, "清空", 12.5f, Brand.BLUE, medium = true)
    private val logView = Ui.text(activity, "", 10.5f, Brand.MUTED, mono = true).apply {
        setLineSpacing(dp(3).toFloat(), 1f)
    }

    private var wasStreaming = false

    init {
        column.addView(
            Ui.heading(
                activity,
                "我的",
                "投屏画质、历史记录与本机信息；诊断日志用于排查链路问题。",
            ),
        )
        column.addView(profileCard())
        column.addView(Ui.cardSpacing(activity, settingsCard(), 12))
        column.addView(Ui.cardSpacing(activity, deviceCard(), 12))
        column.addView(Ui.cardSpacing(activity, historyCard(), 12))
        column.addView(Ui.cardSpacing(activity, logCard(), 12))
        column.addView(Ui.cardSpacing(activity, aboutCard(), 12))
        column.addView(
            Ui.body(
                activity,
                "录屏授权由系统逐次确认，HORIZON 不会保存该授权；所有内容只在手机与大屏之间直连传输。",
                Brand.FAINT,
            ).apply { setPadding(dp(4), dp(14), dp(4), 0) },
        )
        renderSettings()
        renderHistory()
        refreshDynamic()
    }

    // ---------------------------------------------------------------- build

    private fun profileCard(): View {
        val card = Ui.card(activity, 16)
        val row = Ui.row(activity)
        val badge = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            background = activity.rounded(Brand.SOFT, 16)
            addView(LineIcon(activity, LineIcon.Kind.CAST, Brand.BLUE, 24))
        }
        badge.layoutParams = LinearLayout.LayoutParams(dp(50), dp(50)).apply { rightMargin = dp(14) }
        row.addView(badge)

        val copy = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        copy.addView(Ui.text(activity, "HORIZON 投屏", 17f, Brand.NAVY, medium = true))
        copy.addView(
            Ui.text(activity, "Android 发送端 · v${version()}", 11.5f, Brand.FAINT).apply {
                setPadding(0, dp(4), 0, 0)
            },
        )
        row.addView(copy)
        row.addView(selfPill)
        card.addView(row)
        return card
    }

    private fun settingsCard(): View {
        val card = Ui.card(activity)
        card.addView(Ui.sectionTitle(activity, "投屏设置"))
        card.addView(Ui.text(activity, "画质", 13.5f, Brand.NAVY, medium = true))
        val qualityRow = segment(quality, { Prefs.maxEdge }) { Prefs.maxEdge = it }
        qualityRow.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        ).apply { topMargin = dp(8) }
        card.addView(qualityRow)

        card.addView(Ui.divider(activity, 14, 12))
        card.addView(Ui.text(activity, "帧率", 13.5f, Brand.NAVY, medium = true))
        val rateRow = segment(rates, { Prefs.fps }) { Prefs.fps = it }
        rateRow.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        ).apply { topMargin = dp(8) }
        card.addView(rateRow)

        settingsNote.setPadding(0, dp(12), 0, 0)
        card.addView(settingsNote)
        return card
    }

    private fun segment(choices: List<Choice>, current: () -> Int, onPick: (Int) -> Unit): LinearLayout {
        val box = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(dp(4), dp(4), dp(4), dp(4))
            background = activity.rounded(Color.parseColor("#F2F6FC"), 14, Brand.LINE)
        }
        val cells = mutableListOf<TextView>()
        choices.forEach { choice ->
            val cell = Ui.text(activity, choice.label, 13f, Brand.MUTED, medium = true).apply {
                gravity = Gravity.CENTER
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
                tap {
                    if (current() != choice.value) {
                        onPick(choice.value)
                        repaintAll()
                        refreshDynamic()
                    }
                }
            }
            cells.add(cell)
            box.addView(cell)
        }
        repaints += {
            cells.forEachIndexed { index, cell ->
                val on = choices[index].value == current()
                cell.setPadding(0, dp(9), 0, dp(9))
                cell.setTextColor(if (on) Brand.BLUE else Brand.MUTED)
                cell.background = if (on) activity.rounded(Color.WHITE, 11, Brand.LINE) else null
                cell.elevation = if (on) dp(1).toFloat() else 0f
            }
        }
        return box
    }

    private fun deviceCard(): View {
        val card = Ui.card(activity)
        card.addView(Ui.sectionTitle(activity, "本机"))
        card.addView(infoRow("型号", Ui.text(activity, "${Build.MANUFACTURER} ${Build.MODEL}", 12.5f, Brand.NAVY)))
        card.addView(Ui.divider(activity, 8, 8))
        card.addView(infoRow("局域网地址", deviceIp))
        card.addView(Ui.divider(activity, 8, 8))
        card.addView(
            infoRow(
                "系统",
                Ui.text(activity, "Android ${Build.VERSION.RELEASE} · API ${Build.VERSION.SDK_INT}", 12.5f, Brand.NAVY),
            ),
        )
        card.addView(Ui.divider(activity, 8, 8))
        card.addView(infoRow("应用版本", Ui.text(activity, "v${version()}", 12.5f, Brand.NAVY, mono = true)))
        return card
    }

    private fun infoRow(label: String, value: TextView): View {
        val row = Ui.row(activity)
        row.addView(
            Ui.text(activity, label, 12.5f, Brand.MUTED).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        value.maxLines = 1
        value.ellipsize = android.text.TextUtils.TruncateAt.END
        value.gravity = Gravity.END
        row.addView(value)
        return row
    }

    private fun historyCard(): View {
        val card = Ui.card(activity)
        val top = Ui.row(activity)
        top.addView(
            Ui.text(activity, "记录", 13f, Brand.MUTED, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        clearAction.tap {
            Prefs.clearHistory()
            renderHistory()
            Ui.toast(activity, "已清空记录")
        }
        top.addView(clearAction)
        top.setPadding(activity.dp(2), 0, activity.dp(2), activity.dp(8))
        card.addView(top)
        card.addView(historyBox)
        return card
    }

    private fun logCard(): View {
        val card = Ui.card(activity)
        val top = Ui.row(activity)
        top.addView(
            Ui.text(activity, "诊断日志", 13f, Brand.MUTED, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        top.addView(Ui.text(activity, "复制", 12.5f, Brand.BLUE, medium = true).apply {
            setPadding(activity.dp(8), activity.dp(4), 0, activity.dp(4))
            tap {
                val clip = activity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                clip.setPrimaryClip(ClipData.newPlainText("HORIZON 诊断日志", logView.text))
                Ui.toast(activity, "日志已复制")
            }
        })
        top.setPadding(activity.dp(2), 0, activity.dp(2), activity.dp(10))
        card.addView(top)
        card.addView(logView)
        card.addView(
            Ui.body(activity, "每次开始投屏会重新记录链路状态。", Brand.FAINT).apply {
                setPadding(0, dp(10), 0, 0)
            },
        )
        return card
    }

    private fun aboutCard(): View {
        val card = Ui.card(activity)
        card.addView(Ui.sectionTitle(activity, "关于协议"))
        card.addView(protocolRow("浏览器投屏", "WS · 9407 · H.264 直连大屏"))
        card.addView(Ui.divider(activity, 8, 8))
        card.addView(protocolRow("文件互传", "HTTP/WS · 7878 · 电脑「发到本机」"))
        card.addView(Ui.divider(activity, 8, 8))
        card.addView(protocolRow("DLNA 媒体", "SSDP + HTTP · 大屏需开启视频接收"))
        card.addView(
            Ui.body(
                activity,
                "HORIZON 把手机、电脑与大屏放在同一个局域网里：投屏走 9407，文件走 7878，媒体走 DLNA。",
                Brand.FAINT,
            ).apply { setPadding(0, dp(12), 0, 0) },
        )
        return card
    }

    private fun protocolRow(name: String, detail: String): View {
        val row = Ui.row(activity)
        row.addView(
            Ui.text(activity, name, 12.5f, Brand.NAVY, medium = true).apply {
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        row.addView(Ui.text(activity, detail, 10.5f, Brand.FAINT, mono = true))
        return row
    }

    // --------------------------------------------------------------- render

    private fun repaintAll() {
        repaints.forEach { it() }
    }

    private fun renderSettings() {
        repaintAll()
        settingsNote.text = if (MirrorState.streaming) {
            "正在投屏，修改将在下次开始投屏时生效。"
        } else {
            "1080p 适合大多数场景；60 fps 更耗带宽与电量，接收端解码跟不上时会反复重同步。"
        }
    }

    private fun renderHistory() {
        historyBox.removeAllViews()
        val all = Prefs.history()
        val mirror = all.filter { it.kind == "mirror" }.take(3)
        val transfer = all.filter { it.kind == "transfer" }.take(3)
        clearAction.visibility = if (all.isEmpty()) View.GONE else View.VISIBLE
        if (mirror.isEmpty() && transfer.isEmpty()) {
            historyBox.addView(Ui.body(activity, "还没有记录。投屏成功或发送文件后会出现在这里。"))
            return
        }
        if (mirror.isNotEmpty()) {
            addHistoryGroup("投屏", mirror)
        }
        if (transfer.isNotEmpty()) {
            if (mirror.isNotEmpty()) historyBox.addView(Ui.divider(activity, 8, 8))
            addHistoryGroup("文件", transfer)
        }
    }

    private fun addHistoryGroup(title: String, entries: List<HistoryEntry>) {
        historyBox.addView(
            Ui.text(activity, title, 11f, Brand.FAINT, medium = true).apply {
                setPadding(dp(2), 0, 0, dp(4))
            },
        )
        entries.forEach { entry ->
            val row = LinearLayout(activity).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, dp(7), 0, dp(7))
            }
            row.addView(
                Ui.dot(activity, if (entry.kind == "mirror") Brand.BLUE else Brand.SKY, 6).apply {
                    (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(8)
                },
            )
            val copy = LinearLayout(activity).apply {
                orientation = LinearLayout.VERTICAL
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            }
            copy.addView(Ui.text(activity, entry.name, 12.5f, Brand.NAVY, medium = true))
            copy.addView(
                Ui.text(activity, entry.address, 10.5f, Brand.FAINT, mono = true).apply {
                    setPadding(0, dp(2), 0, 0)
                },
            )
            row.addView(copy)
            row.addView(
                Ui.text(activity, Ui.compactTime(System.currentTimeMillis() - entry.at), 10.5f, Brand.FAINT),
            )
            historyBox.addView(row)
        }
    }

    private fun refreshDynamic() {
        val ip = Discovery.localIp()
        deviceIp.text = ip ?: "未连接局域网"
        deviceIp.setTextColor(if (ip == null) Brand.FAINT else Brand.NAVY)
        logView.text = MirrorState.recent().ifEmpty { "暂无日志" }
        when {
            MirrorState.streaming -> {
                selfPill.text = "投屏中"
                selfPill.setTextColor(Brand.GOOD)
                selfPill.background = activity.rounded(Brand.GOOD_SOFT, 99)
            }
            MirrorState.reconnecting -> {
                selfPill.text = "重连中"
                selfPill.setTextColor(Brand.WARN)
                selfPill.background = activity.rounded(Brand.WARN_SOFT, 99)
            }
            MirrorState.failed -> {
                selfPill.text = "需要处理"
                selfPill.setTextColor(Brand.WARN)
                selfPill.background = activity.rounded(Brand.WARN_SOFT, 99)
            }
            else -> {
                selfPill.text = "待连接"
                selfPill.setTextColor(Brand.MUTED)
                selfPill.background = activity.rounded(Brand.SOFT, 99)
            }
        }
        if (MirrorState.streaming != wasStreaming) {
            wasStreaming = MirrorState.streaming
            renderSettings()
        }
    }

    private fun version(): String = try {
        activity.packageManager.getPackageInfo(activity.packageName, 0).versionName ?: "0.1.0"
    } catch (_: Exception) {
        "0.1.0"
    }

    override fun onShow() {
        renderSettings()
        renderHistory()
        refreshDynamic()
    }

    override fun onTick() {
        refreshDynamic()
    }
}
