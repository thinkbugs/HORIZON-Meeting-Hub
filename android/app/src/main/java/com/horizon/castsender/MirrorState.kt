package com.horizon.castsender

import android.util.Log
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Process-wide status the activity polls, plus a short rolling log. */
object MirrorState {
    @Volatile var status: String = "未连接"
    @Volatile var streaming: Boolean = false
    /** The session is alive but the socket dropped; the service is retrying. */
    @Volatile var reconnecting: Boolean = false
    @Volatile var failed: Boolean = false
    @Volatile var fps: Int = 0
    @Volatile var kbps: Int = 0
    @Volatile var drops: Long = 0
    /** Frames the GL relay handed to the encoder, -1 when the relay is not in use. */
    @Volatile var captureFps: Int = -1
    /** Average time one frame spends in eglSwapBuffers. */
    @Volatile var swapMs: Double = 0.0
    /** What the encoder is configured for, e.g. "1920×1080 · 30 fps". */
    @Volatile var resolution: String = ""

    private val lines = ArrayDeque<String>()
    private val stamp = SimpleDateFormat("HH:mm:ss", Locale.CHINA)

    fun live(text: String) {
        status = text
        streaming = true
        reconnecting = false
        failed = false
        log(text)
    }

    fun fail(text: String) {
        status = text
        streaming = false
        reconnecting = false
        failed = true
        log(text)
    }

    fun idle(text: String) {
        status = text
        streaming = false
        reconnecting = false
        failed = false
        log(text)
    }

    /** Session survives, socket does not: the service is dialling again. */
    fun retrying(text: String) {
        status = text
        streaming = false
        reconnecting = true
        failed = false
        log(text)
    }

    fun log(text: String) {
        // Release builds carry no debugger, so the same rolling log also goes to
        // logcat where adb can read it back.
        Log.i(TAG, text)
        synchronized(lines) {
            lines.addLast("${stamp.format(Date())}  $text")
            while (lines.size > 10) lines.removeFirst()
        }
    }

    fun recent(): String = synchronized(lines) { lines.joinToString("\n") }

    private const val TAG = "HorizonCast"
}
