package com.horizon.castsender

import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Process-wide status the activity polls, plus a short rolling log. */
object MirrorState {
    @Volatile var status: String = "未连接"
    @Volatile var streaming: Boolean = false
    @Volatile var failed: Boolean = false
    @Volatile var fps: Int = 0
    @Volatile var kbps: Int = 0
    @Volatile var drops: Long = 0

    private val lines = ArrayDeque<String>()
    private val stamp = SimpleDateFormat("HH:mm:ss", Locale.CHINA)

    fun live(text: String) {
        status = text
        streaming = true
        failed = false
        log(text)
    }

    fun fail(text: String) {
        status = text
        streaming = false
        failed = true
        log(text)
    }

    fun idle(text: String) {
        status = text
        streaming = false
        failed = false
        log(text)
    }

    fun log(text: String) {
        synchronized(lines) {
            lines.addLast("${stamp.format(Date())}  $text")
            while (lines.size > 10) lines.removeFirst()
        }
    }

    fun recent(): String = synchronized(lines) { lines.joinToString("\n") }
}
