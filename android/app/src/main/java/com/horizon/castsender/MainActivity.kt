package com.horizon.castsender

import android.Manifest
import android.app.Activity
import android.app.NotificationManager
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.GradientDrawable
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputFilter
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.AbsListView
import android.widget.BaseAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ListView
import android.widget.TextView
import android.widget.Toast

/**
 * Sender UI. Deliberately AndroidX-free: one activity, one service, no
 * dependencies beyond the platform, so the APK stays a couple of hundred KB
 * and the protocol code is the only thing worth reviewing.
 */
class MainActivity : Activity() {
    private lateinit var statusText: TextView
    private lateinit var statsText: TextView
    private lateinit var logText: TextView
    private lateinit var scanHint: TextView
    private lateinit var hostInput: EditText
    private lateinit var tokenInput: EditText
    private lateinit var listView: ListView
    private lateinit var startButton: Button
    private lateinit var adapter: ReceiverAdapter
    private lateinit var emptyText: TextView

    private val receivers = mutableListOf<Receiver>()
    private var selected: Receiver? = null
    private val scanning = java.util.concurrent.atomic.AtomicBoolean(false)
    private val cancelScan = java.util.concurrent.atomic.AtomicBoolean(false)
    private val handler = Handler(Looper.getMainLooper())

    private var pendingHost = ""
    private var pendingPort = Discovery.PORT
    private var pendingToken = ""

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildUi())
        askNotifications()
        startScan()
    }

    override fun onResume() {
        super.onResume()
        handler.post(ticker)
    }

    override fun onPause() {
        handler.removeCallbacks(ticker)
        super.onPause()
    }

    // ---------------------------------------------------------------- UI

    private fun buildUi(): View {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(24), dp(20), dp(16))
            setBackgroundColor(BACKGROUND)
        }

        root.addView(
            TextView(this).apply {
                text = "HORIZON 投屏"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 24f)
                setTextColor(TEXT)
                setTypeface(typeface, Typeface.BOLD)
            },
        )
        root.addView(
            TextView(this).apply {
                text = "把这块屏幕投到会议室大屏 · 与浏览器扩展同协议"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(MUTED)
                setPadding(0, dp(4), 0, dp(16))
            },
        )

        val card = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = rounded(SURFACE, 16)
            setPadding(dp(16), dp(14), dp(16), dp(14))
        }
        statusText = TextView(this).apply {
            text = MirrorState.status
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(TEXT)
        }
        statsText = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextColor(MUTED)
            setPadding(0, dp(6), 0, 0)
            typeface = Typeface.MONOSPACE
        }
        card.addView(statusText)
        card.addView(statsText)
        root.addView(card)

        val header = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(0, dp(18), 0, dp(8))
        }
        header.addView(
            TextView(this).apply {
                text = "接收端"
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(MUTED)
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
            },
        )
        scanHint = TextView(this).apply {
            text = "扫描中…"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(ACCENT)
            setOnClickListener { startScan() }
        }
        header.addView(scanHint)
        root.addView(header)

        listView = ListView(this).apply {
            divider = ColorDrawable(Color.TRANSPARENT)
            dividerHeight = dp(8)
            setSelector(ColorDrawable(Color.TRANSPARENT))
            isScrollbarFadingEnabled = true
        }
        adapter = ReceiverAdapter()
        listView.adapter = adapter
        listView.setOnItemClickListener { _, _, position, _ ->
            selected = receivers.getOrNull(position)
            hostInput.setText("")
            adapter.notifyDataSetChanged()
        }
        emptyText = TextView(this).apply {
            text = "没有发现接收端。请确认大屏已开启「浏览器投屏」，或在下面填写地址。"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(MUTED)
            gravity = Gravity.CENTER
            setPadding(dp(16), 0, dp(16), 0)
            visibility = View.GONE
        }
        val listFrame = FrameLayout(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                0,
                1f,
            )
        }
        listFrame.addView(
            listView,
            FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
        )
        listFrame.addView(
            emptyText,
            FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER),
        )
        root.addView(listFrame)

        hostInput = field("接收端地址（留空则使用上面选中的）", InputType.TYPE_CLASS_TEXT)
        root.addView(sectionLabel("手动地址"))
        root.addView(hostInput)

        tokenInput = field("6 位投屏码", InputType.TYPE_CLASS_NUMBER).apply {
            filters = arrayOf(InputFilter.LengthFilter(6))
            letterSpacing = 0.3f
        }
        root.addView(sectionLabel("投屏码（大屏「浏览器投屏」面板上显示）"))
        root.addView(tokenInput)

        val buttons = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(0, dp(16), 0, 0)
        }
        startButton = actionButton("开始投屏", ACCENT, TEXT) {
            beginCasting()
        }
        startButton.layoutParams = LinearLayout.LayoutParams(0, dp(48), 1f).apply {
            rightMargin = dp(10)
        }
        val stopButton = actionButton("停止", SURFACE_ALT, TEXT) {
            MirrorService.stop(this@MainActivity)
            MirrorState.idle("正在停止…")
        }
        stopButton.layoutParams = LinearLayout.LayoutParams(dp(96), dp(48))
        buttons.addView(startButton)
        buttons.addView(stopButton)
        root.addView(buttons)

        logText = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 10f)
            setTextColor(MUTED)
            typeface = Typeface.MONOSPACE
            setPadding(0, dp(14), 0, 0)
            maxLines = 5
        }
        root.addView(logText)

        return root
    }

    private fun sectionLabel(label: String) = TextView(this).apply {
        text = label
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
        setTextColor(MUTED)
        setPadding(0, dp(14), 0, dp(6))
    }

    private fun field(placeholder: String, keyboard: Int) = EditText(this).apply {
        hint = placeholder
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setTextColor(TEXT)
        setHintTextColor(FAINT)
        inputType = keyboard
        background = rounded(SURFACE, 12)
        setPadding(dp(14), dp(12), dp(14), dp(12))
        layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        )
    }

    private fun actionButton(label: String, fill: Int, tint: Int, onClick: () -> Unit) =
        Button(this).apply {
            text = label
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(tint)
            background = rounded(fill, 14)
            stateListAnimator = null
            isAllCaps = false
            minHeight = 0
            minimumHeight = 0
            setPadding(0, 0, 0, 0)
            setOnClickListener { onClick() }
        }

    private fun rounded(color: Int, radiusDp: Int): GradientDrawable =
        GradientDrawable().apply {
            setColor(color)
            cornerRadius = dp(radiusDp).toFloat()
        }

    private fun dp(value: Int): Int =
        (value * resources.displayMetrics.density + 0.5f).toInt()

    // -------------------------------------------------------------- scan

    private fun startScan() {
        if (scanning.getAndSet(true)) return
        receivers.clear()
        adapter.notifyDataSetChanged()
        selected = null
        scanHint.text = "扫描中…"
        emptyText.visibility = View.GONE
        cancelScan.set(false)
        val extra = hostInput.text.toString().trim().substringBefore(':')
        Thread({
            Discovery.sweep(extra, cancelScan, onFound = { receiver ->
                runOnUiThread {
                    val duplicate = receivers.any { it.address == receiver.address }
                    if (!duplicate) {
                        receivers.add(receiver)
                        if (selected == null) selected = receiver
                        adapter.notifyDataSetChanged()
                    }
                }
            }, onDone = {
                runOnUiThread {
                    scanning.set(false)
                    scanHint.text = if (receivers.isEmpty()) "重新扫描" else "刷新（${receivers.size}）"
                    emptyText.visibility = if (receivers.isEmpty()) View.VISIBLE else View.GONE
                }
            })
        }, "sweep").start()
    }

    // -------------------------------------------------------------- cast

    private fun beginCasting() {
        val token = tokenInput.text.toString().trim()
        if (token.length != 6) {
            toast("请输入 6 位投屏码")
            return
        }
        val typed = hostInput.text.toString().trim()
        val target = selected
        val host: String
        val port: Int
        if (typed.isNotEmpty()) {
            host = typed.substringBefore(':')
            port = typed.substringAfter(':', Discovery.PORT.toString()).toIntOrNull() ?: Discovery.PORT
        } else if (target != null) {
            host = target.host
            port = target.port
        } else {
            toast("请选择接收端或填写地址")
            return
        }
        pendingHost = host
        pendingPort = port
        pendingToken = token
        val manager = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        startActivityForResult(manager.createScreenCaptureIntent(), REQUEST_CAPTURE)
    }

    @Deprecated("Plain Activity, no AndroidX result API")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_CAPTURE) return
        if (resultCode != RESULT_OK || data == null) {
            MirrorState.fail("已取消录屏授权")
            return
        }
        MirrorService.start(this, pendingHost, pendingPort, pendingToken, resultCode, data)
    }

    private fun askNotifications() {
        if (Build.VERSION.SDK_INT < 33) return
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return
        requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
    }

    private fun toast(message: String) = Toast.makeText(this, message, Toast.LENGTH_SHORT).show()

    // ------------------------------------------------------------ render

    private val ticker = object : Runnable {
        override fun run() {
            render()
            handler.postDelayed(this, 500)
        }
    }

    private fun render() {
        statusText.text = MirrorState.status
        statusText.setTextColor(
            when {
                MirrorState.failed -> WARN
                MirrorState.streaming -> GOOD
                else -> TEXT
            },
        )
        statsText.text = if (MirrorState.streaming) {
            "已发送 ${MirrorState.fps} fps · ${MirrorState.kbps} kbps · 丢弃 ${MirrorState.drops}"
        } else {
            "等待开始 · 接收端需开启浏览器投屏"
        }
        startButton.isEnabled = !MirrorState.streaming
        startButton.alpha = if (MirrorState.streaming) 0.5f else 1f
        logText.text = MirrorState.recent()
    }

    private inner class ReceiverAdapter : BaseAdapter() {
        override fun getCount(): Int = receivers.size

        override fun getItem(position: Int): Receiver = receivers[position]

        override fun getItemId(position: Int): Long = position.toLong()

        override fun getView(position: Int, convertView: View?, parent: ViewGroup): View {
            val row = (convertView as? LinearLayout) ?: LinearLayout(this@MainActivity).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(14), dp(11), dp(14), dp(11))
                layoutParams = AbsListView.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                )
                addView(
                    TextView(this@MainActivity).apply {
                        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                        setTextColor(TEXT)
                    },
                )
                addView(
                    TextView(this@MainActivity).apply {
                        setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
                        typeface = Typeface.MONOSPACE
                        setPadding(0, dp(3), 0, 0)
                    },
                )
            }
            val receiver = receivers[position]
            val chosen = selected?.address == receiver.address
            row.background = rounded(if (chosen) SURFACE_SELECTED else SURFACE, 12)
            (row.getChildAt(0) as TextView).apply {
                text = receiver.name
                setTextColor(if (chosen) ACCENT else TEXT)
            }
            (row.getChildAt(1) as TextView).apply {
                text = receiver.address + if (receiver.legacy) "  ·  旧版接收端" else ""
                setTextColor(FAINT)
            }
            return row
        }
    }

    private companion object {
        const val REQUEST_CAPTURE = 1001
        const val REQUEST_NOTIFICATIONS = 1002

        val BACKGROUND = Color.parseColor("#0B0E12")
        val SURFACE = Color.parseColor("#151A21")
        val SURFACE_ALT = Color.parseColor("#232A34")
        val SURFACE_SELECTED = Color.parseColor("#16243B")
        val ACCENT = Color.parseColor("#4C8DFF")
        val TEXT = Color.parseColor("#E8ECF2")
        val MUTED = Color.parseColor("#8A94A6")
        val FAINT = Color.parseColor("#5C6577")
        val GOOD = Color.parseColor("#4ADE80")
        val WARN = Color.parseColor("#F87171")
    }
}
