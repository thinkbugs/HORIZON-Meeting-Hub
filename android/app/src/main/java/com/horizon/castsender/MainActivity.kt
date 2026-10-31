package com.horizon.castsender

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import com.horizon.castsender.pages.ConnectPage
import com.horizon.castsender.pages.MediaPage
import com.horizon.castsender.pages.MinePage
import com.horizon.castsender.pages.Page
import com.horizon.castsender.pages.TransferPage
import com.horizon.castsender.ui.Brand
import com.horizon.castsender.ui.LineIcon
import com.horizon.castsender.ui.Ui
import com.horizon.castsender.ui.dp
import com.horizon.castsender.ui.rounded
import com.horizon.castsender.ui.tap

class MainActivity : Activity() {
    private lateinit var content: FrameLayout
    private lateinit var pages: List<Page>
    private lateinit var tabIcons: List<LineIcon>
    private lateinit var tabLabels: List<TextView>
    private lateinit var statusPill: LinearLayout
    private lateinit var statusDot: View
    private lateinit var statusLabel: TextView
    private val handler = Handler(Looper.getMainLooper())
    private var current = 0

    private var pendingHost = ""
    private var pendingPort = Discovery.PORT
    private var pendingToken = ""
    private var pendingName = ""

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Prefs.init(this)
        setContentView(buildShell())
        Ui.statusBar(this)
        pages = listOf(ConnectPage(this), TransferPage(this), MediaPage(this), MinePage(this))
        showPage(0)
        askNotifications()
    }

    override fun onResume() {
        super.onResume()
        handler.post(ticker)
        pages.forEach { it.onActivityResume() }
    }

    override fun onPause() {
        handler.removeCallbacks(ticker)
        super.onPause()
    }

    override fun onDestroy() {
        pages.forEach { it.onDestroy() }
        LanDiscovery.stop()
        super.onDestroy()
    }

    @Deprecated("Plain Activity, no AndroidX result API")
    override fun onBackPressed() {
        if (current != 0) {
            showPage(0)
            return
        }
        super.onBackPressed()
    }

    // --------------------------------------------------------------- shell

    private fun buildShell(): View {
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Brand.BG)
        }
        root.setOnApplyWindowInsetsListener { view, insets ->
            // Edge-to-edge is forced from targetSdk 35, so pad the shell instead.
            val bars = if (android.os.Build.VERSION.SDK_INT >= 30) {
                insets.getInsets(
                    android.view.WindowInsets.Type.systemBars() or
                        android.view.WindowInsets.Type.displayCutout(),
                )
            } else {
                @Suppress("DEPRECATION")
                android.graphics.Insets.of(
                    insets.systemWindowInsetLeft,
                    insets.systemWindowInsetTop,
                    insets.systemWindowInsetRight,
                    insets.systemWindowInsetBottom,
                )
            }
            view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            insets
        }

        val header = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(20), dp(14), dp(20), dp(2))
        }
        val brand = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        brand.addView(
            TextView(this).apply {
                text = "HORIZON"
                setTextSize(android.util.TypedValue.COMPLEX_UNIT_SP, 19f)
                letterSpacing = 0.18f
                setTextColor(Brand.NAVY)
                typeface = android.graphics.Typeface.create("sans-serif-medium", android.graphics.Typeface.NORMAL)
            },
        )
        brand.addView(
            TextView(this).apply {
                text = "跨越屏幕，连接无限可能"
                setTextSize(android.util.TypedValue.COMPLEX_UNIT_SP, 10.5f)
                setTextColor(Brand.FAINT)
                setPadding(0, dp(3), 0, 0)
            },
        )
        header.addView(brand)

        statusPill = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(10), dp(6), dp(12), dp(6))
            background = rounded(Brand.SOFT, 99)
        }
        statusDot = Ui.dot(this, Brand.FAINT, 7).apply {
            (layoutParams as LinearLayout.LayoutParams).rightMargin = dp(6)
        }
        statusLabel = Ui.text(this, "待连接", 11f, Brand.MUTED, medium = true)
        statusPill.addView(statusDot)
        statusPill.addView(statusLabel)
        header.addView(statusPill)
        root.addView(header)

        content = FrameLayout(this).apply {
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                0,
                1f,
            )
        }
        root.addView(content)
        root.addView(buildNav())
        return root
    }

    private fun buildNav(): View {
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(Brand.CARD)
            setPadding(dp(8), dp(7), dp(8), dp(6))
        }
        val hairline = View(this).apply {
            setBackgroundColor(Brand.LINE)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(1))
        }
        val wrapper = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(hairline)
            addView(bar)
        }

        val icons = listOf(
            LineIcon.Kind.CAST,
            LineIcon.Kind.TRANSFER,
            LineIcon.Kind.MEDIA,
            LineIcon.Kind.PROFILE,
        )
        val labels = listOf("连接", "传输", "媒体", "我的")
        tabIcons = icons.mapIndexed { index, kind ->
            LineIcon(this, kind, Brand.FAINT, 23).also { icon ->
                icon.layoutParams = LinearLayout.LayoutParams(dp(23), dp(23)).apply {
                    gravity = Gravity.CENTER_HORIZONTAL
                }
            }
        }
        tabLabels = labels.map { label ->
            Ui.text(this, label, 10.5f, Brand.FAINT, medium = true).apply {
                gravity = Gravity.CENTER
                setPadding(0, dp(3), 0, 0)
            }
        }
        labels.indices.forEach { index ->
            val item = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER
                setPadding(0, dp(4), 0, dp(2))
                layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
                addView(tabIcons[index])
                addView(tabLabels[index])
                tap { showPage(index) }
            }
            bar.addView(item)
        }
        return wrapper
    }

    private fun paintTabs() {
        tabIcons.forEachIndexed { index, icon ->
            val color = if (index == current) Brand.BLUE else Brand.FAINT
            icon.tint(color)
            tabLabels[index].setTextColor(color)
        }
    }

    private fun showPage(index: Int) {
        if (index !in pages.indices) return
        pages[current].onHide()
        content.removeAllViews()
        current = index
        val view = pages[index].root
        (view.parent as? ViewGroup)?.removeView(view)
        content.addView(
            view,
            FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
        )
        pages[index].onShow()
        paintTabs()
        refreshPill()
    }

    // -------------------------------------------------------------- status

    private val ticker = object : Runnable {
        override fun run() {
            pages[current].onTick()
            refreshPill()
            handler.postDelayed(this, 500)
        }
    }

    private fun refreshPill() {
        when {
            MirrorState.streaming -> {
                statusLabel.text = "投屏中"
                statusLabel.setTextColor(Brand.GOOD)
                statusDot.background = rounded(Brand.GOOD, 99)
                statusPill.background = rounded(Brand.GOOD_SOFT, 99)
            }
            MirrorState.reconnecting -> {
                statusLabel.text = "重连中"
                statusLabel.setTextColor(Brand.WARN)
                statusDot.background = rounded(Brand.WARN, 99)
                statusPill.background = rounded(Brand.WARN_SOFT, 99)
            }
            MirrorState.failed -> {
                statusLabel.text = "需要处理"
                statusLabel.setTextColor(Brand.WARN)
                statusDot.background = rounded(Brand.WARN, 99)
                statusPill.background = rounded(Brand.WARN_SOFT, 99)
            }
            else -> {
                statusLabel.text = "待连接"
                statusLabel.setTextColor(Brand.MUTED)
                statusDot.background = rounded(Brand.FAINT, 99)
                statusPill.background = rounded(Brand.SOFT, 99)
            }
        }
    }

    // ------------------------------------------------------------- capture

    fun requestCapture(host: String, port: Int, token: String, name: String) {
        pendingHost = host
        pendingPort = port
        pendingToken = token
        pendingName = name
        val manager = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        startActivityForResult(manager.createScreenCaptureIntent(), REQUEST_CAPTURE)
    }

    @Deprecated("Plain Activity, no AndroidX result API")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQUEST_CAPTURE) {
            if (resultCode != RESULT_OK || data == null) {
                MirrorState.idle("已取消录屏授权")
                return
            }
            Prefs.record(
                "mirror",
                pendingName.ifEmpty { "HORIZON 接收端" },
                "$pendingHost:$pendingPort",
            )
            MirrorService.start(this, pendingHost, pendingPort, pendingToken, resultCode, data)
            return
        }
        pages.forEach { it.onActivityResult(requestCode, resultCode, data) }
    }

    private fun askNotifications() {
        if (android.os.Build.VERSION.SDK_INT < 33) return
        if (checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
    }

    private companion object {
        const val REQUEST_CAPTURE = 1001
        const val REQUEST_NOTIFICATIONS = 1002
    }
}
