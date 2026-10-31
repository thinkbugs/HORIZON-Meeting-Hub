package com.horizon.castsender.ui

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RectF
import android.view.View
import android.widget.LinearLayout

/** One stroke weight for every glyph, so icons read as a set. */
class LineIcon(
    context: Context,
    private val kind: Kind,
    color: Int,
    sizeDp: Int = 22,
) : View(context) {
    enum class Kind { CAST, TRANSFER, MEDIA, PROFILE, PLAY, PAUSE, STOP, SEND, FILE, CHECK, SCREEN, SOUND, BACK }

    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = context.dp(1.7f)
        strokeCap = Paint.Cap.ROUND
        strokeJoin = Paint.Join.ROUND
        this.color = color
    }
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.FILL
        this.color = color
    }
    private val path = Path()

    init {
        layoutParams = LinearLayout.LayoutParams(context.dp(sizeDp), context.dp(sizeDp))
    }

    fun tint(color: Int) {
        stroke.color = color
        fill.color = color
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        val size = minOf(width, height).toFloat()
        if (size <= 0) return
        val scale = size / 24f
        canvas.save()
        canvas.translate((width - size) / 2f, (height - size) / 2f)
        canvas.scale(scale, scale)
        path.reset()
        when (kind) {
            Kind.CAST -> {
                path.addRoundRect(RectF(3f, 4f, 21f, 17.5f), 2.4f, 2.4f, Path.Direction.CW)
                canvas.drawPath(path, stroke)
                canvas.drawArc(RectF(3f, 9f, 12.5f, 18.5f), 180f, 90f, false, stroke)
                canvas.drawArc(RectF(3f, 13f, 8.5f, 18.5f), 180f, 90f, false, stroke)
                canvas.drawCircle(4.8f, 17.4f, 1.1f, fill)
            }
            Kind.TRANSFER -> {
                path.moveTo(8.5f, 19f); path.lineTo(8.5f, 6.2f)
                path.moveTo(5.2f, 9.4f); path.lineTo(8.5f, 6f); path.lineTo(11.8f, 9.4f)
                path.moveTo(15.5f, 5f); path.lineTo(15.5f, 17.8f)
                path.moveTo(12.2f, 14.6f); path.lineTo(15.5f, 18f); path.lineTo(18.8f, 14.6f)
                canvas.drawPath(path, stroke)
            }
            Kind.MEDIA -> {
                canvas.drawCircle(12f, 12f, 8.8f, stroke)
                path.moveTo(10.2f, 8.6f); path.lineTo(16.2f, 12f); path.lineTo(10.2f, 15.4f); path.close()
                canvas.drawPath(path, fill)
            }
            Kind.PROFILE -> {
                canvas.drawCircle(12f, 8.4f, 3.6f, stroke)
                path.moveTo(4.8f, 19.6f)
                path.cubicTo(6.2f, 15.2f, 9f, 13.2f, 12f, 13.2f)
                path.cubicTo(15f, 13.2f, 17.8f, 15.2f, 19.2f, 19.6f)
                canvas.drawPath(path, stroke)
            }
            Kind.PLAY -> {
                path.moveTo(9.2f, 6.3f); path.lineTo(18.4f, 12f); path.lineTo(9.2f, 17.7f); path.close()
                canvas.drawPath(path, fill)
            }
            Kind.PAUSE -> {
                path.addRoundRect(RectF(7f, 5.8f, 10.2f, 18.2f), 1.2f, 1.2f, Path.Direction.CW)
                path.addRoundRect(RectF(13.8f, 5.8f, 17f, 18.2f), 1.2f, 1.2f, Path.Direction.CW)
                canvas.drawPath(path, fill)
            }
            Kind.STOP -> {
                path.addRoundRect(RectF(6.4f, 6.4f, 17.6f, 17.6f), 2f, 2f, Path.Direction.CW)
                canvas.drawPath(path, fill)
            }
            Kind.SEND -> {
                path.moveTo(4f, 11.7f); path.lineTo(20f, 4.4f); path.lineTo(15.6f, 20f); path.lineTo(11.5f, 13.6f); path.close()
                path.moveTo(11.5f, 13.6f); path.lineTo(20f, 4.4f)
                canvas.drawPath(path, stroke)
            }
            Kind.FILE -> {
                path.moveTo(6.5f, 3.6f); path.lineTo(14f, 3.6f); path.lineTo(18f, 7.6f); path.lineTo(18f, 20.4f)
                path.lineTo(6.5f, 20.4f); path.close()
                path.moveTo(14f, 3.6f); path.lineTo(14f, 7.6f); path.lineTo(18f, 7.6f)
                canvas.drawPath(path, stroke)
            }
            Kind.CHECK -> {
                path.moveTo(5.4f, 12.6f); path.lineTo(10f, 17f); path.lineTo(18.8f, 7.2f)
                canvas.drawPath(path, stroke)
            }
            Kind.SCREEN -> {
                path.addRoundRect(RectF(3.2f, 4.6f, 20.8f, 16.4f), 2f, 2f, Path.Direction.CW)
                path.moveTo(9f, 20f); path.lineTo(15f, 20f)
                path.moveTo(12f, 16.4f); path.lineTo(12f, 20f)
                canvas.drawPath(path, stroke)
            }
            Kind.SOUND -> {
                path.moveTo(4.5f, 9.5f); path.lineTo(8f, 9.5f); path.lineTo(12f, 5.5f); path.lineTo(12f, 18.5f)
                path.lineTo(8f, 14.5f); path.lineTo(4.5f, 14.5f); path.close()
                canvas.drawPath(path, stroke)
                canvas.drawArc(RectF(12f, 8.4f, 16.4f, 15.6f), -60f, 120f, false, stroke)
                canvas.drawArc(RectF(12f, 5.6f, 20.2f, 18.4f), -55f, 110f, false, stroke)
            }
            Kind.BACK -> {
                path.moveTo(14.5f, 5.5f); path.lineTo(8f, 12f); path.lineTo(14.5f, 18.5f)
                canvas.drawPath(path, stroke)
            }
        }
        canvas.restore()
    }
}
