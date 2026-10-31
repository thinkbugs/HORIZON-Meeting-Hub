package com.horizon.castsender.ui

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.SweepGradient
import android.view.View
import android.widget.LinearLayout

/**
 * The scanning affordance: faint rings always, one sweep while a LAN sweep is
 * running. Drawn rather than animated in XML so it costs nothing when idle.
 */
class RadarView(context: Context) : View(context) {
    var active: Boolean = false
        set(value) {
            if (field == value) return
            field = value
            invalidate()
        }

    private val ring = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = dp(1).toFloat()
        color = 0x33_397DE8
    }
    private val core = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = Brand.BLUE
    }
    private val halo = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        color = 0x14_397DE8
    }
    private val sweep = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
    }

    init {
        layoutParams = LinearLayout.LayoutParams(dp(132).toInt(), dp(132).toInt())
    }

    private fun dp(value: Int): Float = value * resources.displayMetrics.density + 0.5f

    override fun onDraw(canvas: Canvas) {
        val cx = width / 2f
        val cy = height / 2f
        val radius = minOf(width, height) / 2f - dp(8)
        if (radius <= 0) return
        canvas.drawCircle(cx, cy, radius, halo)
        canvas.drawCircle(cx, cy, radius, ring)
        canvas.drawCircle(cx, cy, radius * 0.68f, ring)
        canvas.drawCircle(cx, cy, radius * 0.36f, ring)

        if (active) {
            val angle = (System.currentTimeMillis() % 2400L) / 2400f * 360f
            sweep.shader = SweepGradient(
                cx,
                cy,
                intArrayOf(0x00397DE8, 0x05397DE8, 0x40397DE8),
                floatArrayOf(0f, 0.72f, 1f),
            )
            canvas.save()
            canvas.rotate(angle, cx, cy)
            canvas.drawCircle(cx, cy, radius, sweep)
            canvas.restore()
            postInvalidateOnAnimation()
        }
        canvas.drawCircle(cx, cy, dp(4).toFloat(), core)
    }
}
