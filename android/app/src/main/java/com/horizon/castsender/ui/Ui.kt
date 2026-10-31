package com.horizon.castsender.ui

import android.app.Activity
import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.text.InputFilter
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast

/** HORIZON brand: quiet, light, one blue that earns its place. */
object Brand {
    val BG = Color.parseColor("#F7F9FC")
    val CARD = Color.WHITE
    val NAVY = Color.parseColor("#172B46")
    val BLUE = Color.parseColor("#397DE8")
    val SKY = Color.parseColor("#B9D9FF")
    val SOFT = Color.parseColor("#EAF2FE")
    val LINE = Color.parseColor("#E4EBF4")
    val MUTED = Color.parseColor("#6F7F94")
    val FAINT = Color.parseColor("#9AA7B8")
    val GOOD = Color.parseColor("#1E9E67")
    val GOOD_SOFT = Color.parseColor("#E6F6EE")
    val WARN = Color.parseColor("#D64545")
    val WARN_SOFT = Color.parseColor("#FDECEC")
    val NAVY_CARD = Color.parseColor("#172B46")
}

fun Context.dp(value: Int): Int =
    (value * resources.displayMetrics.density + 0.5f).toInt()

fun Context.dp(value: Float): Float = value * resources.displayMetrics.density

fun Context.sp(value: Float): Float =
    TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, value, resources.displayMetrics)

fun View.tap(block: () -> Unit) {
    isClickable = true
    setOnClickListener { block() }
}

fun Context.rounded(color: Int, radiusDp: Int, strokeColor: Int? = null, strokeDp: Int = 1): GradientDrawable =
    GradientDrawable().apply {
        setColor(color)
        cornerRadius = dp(radiusDp).toFloat()
        if (strokeColor != null) setStroke(dp(strokeDp), strokeColor)
    }

fun Context.ripple(background: GradientDrawable, color: Int = Color.parseColor("#1F172B46")): RippleDrawable =
    RippleDrawable(ColorStateList.valueOf(color), background, null)

object Ui {
    fun text(
        context: Context,
        value: CharSequence,
        size: Float,
        color: Int = Brand.NAVY,
        medium: Boolean = false,
        mono: Boolean = false,
    ): TextView = TextView(context).apply {
        text = value
        setTextSize(TypedValue.COMPLEX_UNIT_SP, size)
        setTextColor(color)
        typeface = when {
            mono -> Typeface.MONOSPACE
            medium -> Typeface.create("sans-serif-medium", Typeface.NORMAL)
            else -> Typeface.create("sans-serif", Typeface.NORMAL)
        }
    }

    fun page(context: Context, content: LinearLayout): ScrollView = ScrollView(context).apply {
        isFillViewport = true
        clipToPadding = false
        setBackgroundColor(Brand.BG)
        addView(
            content,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
            ),
        )
    }

    fun column(context: Context, paddingDp: Int = 20): LinearLayout = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(context.dp(paddingDp), context.dp(4), context.dp(paddingDp), context.dp(28))
    }

    fun heading(context: Context, title: String, subtitle: String): LinearLayout =
        LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, context.dp(10), 0, context.dp(14))
            addView(text(context, title, 26f, Brand.NAVY, medium = true))
            addView(
                text(context, subtitle, 12.5f, Brand.MUTED).apply {
                    setPadding(0, context.dp(5), 0, 0)
                    setLineSpacing(context.sp(3f), 1f)
                },
            )
        }

    fun card(context: Context, paddingDp: Int = 18): LinearLayout = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        background = context.rounded(Brand.CARD, 20, Brand.LINE)
        setPadding(context.dp(paddingDp), context.dp(paddingDp), context.dp(paddingDp), context.dp(paddingDp))
        elevation = context.dp(1).toFloat()
    }

    fun cardSpacing(context: Context, view: View, topDp: Int = 12): View {
        val params = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        )
        params.topMargin = context.dp(topDp)
        view.layoutParams = params
        return view
    }

    fun sectionTitle(context: Context, title: String, action: String? = null, onAction: (() -> Unit)? = null): LinearLayout =
        LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(context.dp(2), 0, context.dp(2), context.dp(10))
            addView(
                text(context, title, 13f, Brand.MUTED, medium = true).apply {
                    layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
                },
            )
            if (action != null) {
                addView(
                    text(context, action, 12.5f, Brand.BLUE, medium = true).apply {
                        setPadding(context.dp(8), context.dp(4), 0, context.dp(4))
                        if (onAction != null) tap { onAction() }
                    },
                )
            }
        }

    fun button(context: Context, label: String, primary: Boolean = true): TextView =
        text(context, label, 14f, if (primary) Color.WHITE else Brand.NAVY, medium = true).apply {
            gravity = Gravity.CENTER
            setPadding(context.dp(16), context.dp(14), context.dp(16), context.dp(14))
            background = context.ripple(
                if (primary) context.rounded(Brand.BLUE, 14)
                else context.rounded(Color.WHITE, 14, Brand.LINE),
            )
        }

    fun softButton(context: Context, label: String): TextView =
        text(context, label, 14f, Brand.BLUE, medium = true).apply {
            gravity = Gravity.CENTER
            setPadding(context.dp(16), context.dp(13), context.dp(16), context.dp(13))
            background = context.ripple(context.rounded(Brand.SOFT, 14))
        }

    fun smallButton(context: Context, label: String): TextView =
        text(context, label, 12.5f, Brand.BLUE, medium = true).apply {
            gravity = Gravity.CENTER
            setPadding(context.dp(14), context.dp(9), context.dp(14), context.dp(9))
            background = context.ripple(context.rounded(Brand.SOFT, 11))
        }

    fun field(
        context: Context,
        hint: String,
        inputType: Int = InputType.TYPE_CLASS_TEXT,
    ): EditText = EditText(context).apply {
        this.hint = hint
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        setTextColor(Brand.NAVY)
        setHintTextColor(Brand.FAINT)
        this.inputType = inputType
        background = context.rounded(Color.parseColor("#F2F6FC"), 12, Brand.LINE)
        setPadding(context.dp(14), context.dp(12), context.dp(14), context.dp(12))
        setSingleLine()
        layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        )
    }

    fun codeField(context: Context): EditText = field(context, "000000", InputType.TYPE_CLASS_NUMBER).apply {
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 24f)
        letterSpacing = 0.32f
        filters = arrayOf(InputFilter.LengthFilter(6))
        gravity = Gravity.CENTER
        typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
    }

    fun divider(context: Context, topDp: Int = 10, bottomDp: Int = 10): View =
        View(context).apply {
            setBackgroundColor(Brand.LINE)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, context.dp(1)).apply {
                topMargin = context.dp(topDp)
                bottomMargin = context.dp(bottomDp)
            }
        }

    fun pill(context: Context, label: String, fg: Int, bg: Int): TextView =
        text(context, label, 11.5f, fg, medium = true).apply {
            setPadding(context.dp(10), context.dp(5), context.dp(10), context.dp(5))
            background = context.rounded(bg, 99)
        }

    fun dot(context: Context, color: Int, sizeDp: Int = 7): View = View(context).apply {
        background = context.rounded(color, 99)
        layoutParams = LinearLayout.LayoutParams(context.dp(sizeDp), context.dp(sizeDp))
    }

    fun progress(context: Context): ProgressBar =
        ProgressBar(context, null, android.R.attr.progressBarStyleHorizontal).apply {
            max = 100
            progress = 0
            progressTintList = ColorStateList.valueOf(Brand.BLUE)
            progressBackgroundTintList = ColorStateList.valueOf(Brand.SOFT)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, context.dp(6))
        }

    fun space(context: Context, heightDp: Int): View = View(context).apply {
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, context.dp(heightDp))
    }

    fun row(context: Context): LinearLayout = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
    }

    fun body(context: Context, value: String, color: Int = Brand.MUTED): TextView =
        text(context, value, 12.5f, color).apply {
            setLineSpacing(context.sp(4f), 1f)
        }

    fun stat(context: Context, label: String, value: String, dark: Boolean = false): LinearLayout =
        LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            addView(text(context, value, 18f, if (dark) Color.WHITE else Brand.NAVY, medium = true))
            addView(
                text(context, label, 11f, if (dark) Color.parseColor("#A9BDD6") else Brand.MUTED).apply {
                    setPadding(0, context.dp(3), 0, 0)
                },
            )
        }

    fun toast(context: Context, message: String) {
        Toast.makeText(context, message, Toast.LENGTH_SHORT).show()
    }

    fun compactTime(millis: Long): String {
        val minutes = (millis / 60_000).toInt()
        return when {
            minutes < 1 -> "刚刚"
            minutes < 60 -> "${minutes} 分钟前"
            minutes < 60 * 24 -> "${minutes / 60} 小时前"
            else -> "${minutes / (60 * 24)} 天前"
        }
    }

    fun bytes(value: Long): String = when {
        value >= 1_000_000_000 -> String.format("%.2f GB", value / 1_000_000_000.0)
        value >= 1_000_000 -> String.format("%.1f MB", value / 1_000_000.0)
        value >= 1_000 -> String.format("%.0f KB", value / 1_000.0)
        else -> "$value B"
    }

    fun hms(seconds: Double): String {
        val total = seconds.toInt().coerceAtLeast(0)
        return String.format("%02d:%02d", total / 60, total % 60)
    }

    fun statusBar(activity: Activity) {
        activity.window.statusBarColor = Brand.BG
        activity.window.navigationBarColor = Color.WHITE
        val apply = {
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                val appearance = android.view.WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS or
                    android.view.WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
                activity.window.decorView.windowInsetsController
                    ?.setSystemBarsAppearance(appearance, appearance)
            } else {
                @Suppress("DEPRECATION")
                activity.window.decorView.systemUiVisibility =
                    View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR or View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR
            }
        }
        try {
            apply()
        } catch (_: Exception) {
            // The decor view is not attached yet on some OEM builds.
            activity.window.decorView.post { try { apply() } catch (_: Exception) {} }
        }
    }
}
