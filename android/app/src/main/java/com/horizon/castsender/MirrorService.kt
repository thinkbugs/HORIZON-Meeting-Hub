package com.horizon.castsender

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.DisplayMetrics
import android.view.Display
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * Owns the whole sender pipeline for one session:
 * MediaProjection → VirtualDisplay → MediaCodec (VideoEncoder) → 24-byte wire
 * header → WebSocket → receiver on 9407. Same contract as the browser extension.
 */
class MirrorService : Service() {
    private lateinit var projectionManager: MediaProjectionManager
    private var projection: MediaProjection? = null
    private var ws: WsClient? = null
    private var encoder: VideoEncoder? = null
    private var writer: Thread? = null
    private var statsThread: Thread? = null
    private var displayListener: DisplayManager.DisplayListener? = null

    private val queue = ArrayBlockingQueue<ByteArray>(16)
    private val stopping = AtomicBoolean(false)
    private val frames = AtomicLong()
    private val bytes = AtomicLong()
    private val dropped = AtomicLong()

    @Volatile private var token = ""
    @Volatile private var targetLabel = ""

    override fun onCreate() {
        super.onCreate()
        projectionManager = getSystemService(MediaProjectionManager::class.java)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> stopMirror("已停止投屏")
            ACTION_START -> startMirror(intent)
            else -> stopSelf()
        }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        stopMirror("投屏已结束")
        super.onDestroy()
    }

    // -------------------------------------------------------------- start

    private fun startMirror(intent: Intent) {
        if (projection != null) return
        val host = intent.getStringExtra(EXTRA_HOST).orEmpty()
        val port = intent.getIntExtra(EXTRA_PORT, Discovery.PORT)
        token = intent.getStringExtra(EXTRA_TOKEN).orEmpty()
        val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, Activity.RESULT_CANCELED)
        val resultData = if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableExtra(EXTRA_RESULT_DATA, Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(EXTRA_RESULT_DATA)
        }
        if (host.isEmpty() || resultData == null) {
            MirrorState.fail("缺少投屏参数，请重新选择接收端")
            stopMirror("参数缺失")
            return
        }
        targetLabel = "$host:$port"

        // Android 14 refuses getMediaProjection() until the FGS is running with
        // type mediaProjection, so the notification always comes first.
        startForegroundCompat("正在连接 $targetLabel")
        MirrorState.idle("正在连接 $targetLabel")
        frames.set(0)
        bytes.set(0)
        dropped.set(0)

        val projection = projectionManager.getMediaProjection(resultCode, resultData)
        if (projection == null) {
            MirrorState.fail("系统没有授予录屏权限")
            stopMirror("未获得录屏权限")
            return
        }
        projection.registerCallback(
            object : MediaProjection.Callback() {
                override fun onStop() {
                    stopMirror("系统已停止投屏")
                }
            },
            Handler(Looper.getMainLooper()),
        )
        this.projection = projection
        watchRotation(projection)

        Thread({
            val client = WsClient(host, port, "/stream?token=$token", socketListener)
            ws = client
            try {
                client.connect()
            } catch (error: Exception) {
                MirrorState.fail(error.message ?: "连接失败")
                stopMirror("连接失败")
            }
        }, "ws-connect").start()
    }

    private fun startEncoder() {
        val projection = projection ?: return
        val (width, height, dpi) = captureSpec()
        val encoder = VideoEncoder(encoderSink)
        this.encoder = encoder
        try {
            encoder.start(projection, width, height, dpi)
        } catch (error: Exception) {
            MirrorState.fail("编码器启动失败：${error.message}")
            stopMirror("编码器启动失败")
        }
    }

    private val socketListener = object : WsClient.Listener {
        override fun onOpen() {
            ws?.sendText("""{"type":"hello"}""")
            val client = ws ?: return
            startWriter(client)
            startEncoder()
            startStats()
            MirrorState.live("正在投屏 · $targetLabel")
        }

        override fun onText(text: String) {
            when {
                text.contains("\"keyframe\"") -> encoder?.requestKeyFrame()
                text.contains("\"welcome\"") -> MirrorState.live("正在投屏 · $targetLabel")
            }
        }

        override fun onClosed(reason: String) {
            if (stopping.get()) return
            MirrorState.fail(reason)
            stopMirror(reason)
        }
    }

    // ------------------------------------------------------------- frames

    private val encoderSink = object : VideoEncoder.Sink {
        override fun onVideo(data: ByteArray, keyframe: Boolean, width: Int, height: Int, ptsUs: Long) {
            frames.incrementAndGet()
            bytes.addAndGet(data.size.toLong())
            // Bit 1 marks pts as sender wall clock, exactly like offscreen.js.
            val flags = (if (keyframe) 1 else 0) or 2
            enqueue(Wire.packet(Wire.KIND_VIDEO, flags, ptsUs, width, height, data), keyframe)
        }
    }

    private fun enqueue(packet: ByteArray, keyframe: Boolean) {
        if (queue.offer(packet)) return
        dropped.incrementAndGet()
        if (keyframe) {
            queue.clear()
            queue.offer(packet)
        }
    }

    private fun startWriter(client: WsClient) {
        writer = Thread({
            try {
                while (!stopping.get()) {
                    client.sendBinary(queue.take())
                }
            } catch (_: InterruptedException) {
            }
        }, "video-writer").apply { start() }
    }

    private fun startStats() {
        statsThread = Thread({
            var lastFrames = 0L
            var lastBytes = 0L
            while (!stopping.get()) {
                try {
                    Thread.sleep(1000)
                } catch (_: InterruptedException) {
                    break
                }
                val nowFrames = frames.get()
                val nowBytes = bytes.get()
                MirrorState.fps = (nowFrames - lastFrames).toInt()
                MirrorState.kbps = ((nowBytes - lastBytes) * 8 / 1000).toInt()
                MirrorState.drops = dropped.get()
                lastFrames = nowFrames
                lastBytes = nowBytes
            }
        }, "stats").apply { start() }
    }

    // ------------------------------------------------------------- display

    private fun captureSpec(): Triple<Int, Int, Int> {
        val manager = getSystemService(DisplayManager::class.java)
        val display = manager.getDisplay(Display.DEFAULT_DISPLAY)
        val metrics = DisplayMetrics()
        @Suppress("DEPRECATION")
        display.getRealMetrics(metrics)
        val longest = maxOf(metrics.widthPixels, metrics.heightPixels).coerceAtLeast(1)
        val scale = min(1f, 1920f / longest)
        val width = (metrics.widthPixels * scale).roundToInt() and 0x7FFFFFFE
        val height = (metrics.heightPixels * scale).roundToInt() and 0x7FFFFFFE
        val dpi = (metrics.densityDpi * scale).roundToInt().coerceAtLeast(120)
        return Triple(width, height, dpi)
    }

    private fun watchRotation(projection: MediaProjection) {
        val manager = getSystemService(DisplayManager::class.java)
        val listener = object : DisplayManager.DisplayListener {
            override fun onDisplayAdded(displayId: Int) = Unit

            override fun onDisplayRemoved(displayId: Int) = Unit

            override fun onDisplayChanged(displayId: Int) {
                if (displayId != Display.DEFAULT_DISPLAY) return
                Handler(Looper.getMainLooper()).postDelayed({
                    if (stopping.get()) return@postDelayed
                    val (width, height, dpi) = captureSpec()
                    try {
                        encoder?.resize(projection, width, height, dpi)
                    } catch (_: Exception) {
                    }
                }, 350)
            }
        }
        displayListener = listener
        manager.registerDisplayListener(listener, Handler(Looper.getMainLooper()))
    }

    // --------------------------------------------------------------- stop

    private fun stopMirror(reason: String) {
        if (!stopping.compareAndSet(false, true)) return
        if (!MirrorState.failed) MirrorState.idle(reason)
        MirrorState.streaming = false
        MirrorState.fps = 0
        MirrorState.kbps = 0

        encoder?.stop()
        encoder = null
        statsThread?.interrupt()
        statsThread = null
        writer?.interrupt()
        writer = null
        queue.clear()
        ws?.close()
        ws = null
        displayListener?.let { listener ->
            try {
                getSystemService(DisplayManager::class.java).unregisterDisplayListener(listener)
            } catch (_: Exception) {
            }
        }
        displayListener = null
        projection?.let { active ->
            try {
                active.stop()
            } catch (_: Exception) {
            }
        }
        projection = null
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    // -------------------------------------------------------- notification

    private fun startForegroundCompat(text: String) {
        val manager = getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "投屏", NotificationManager.IMPORTANCE_LOW).apply {
                    description = "投屏进行中的常驻通知"
                    setShowBadge(false)
                },
            )
        }
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, MirrorService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setContentTitle("HORIZON 投屏")
            .setContentText(text)
            .setOngoing(true)
            .setContentIntent(open)
            .addAction(
                Notification.Action.Builder(
                    android.R.drawable.ic_menu_close_clear_cancel,
                    "停止投屏",
                    stop,
                ).build(),
            )
            .build()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    companion object {
        const val ACTION_START = "com.horizon.castsender.action.START"
        const val ACTION_STOP = "com.horizon.castsender.action.STOP"
        private const val EXTRA_HOST = "host"
        private const val EXTRA_PORT = "port"
        private const val EXTRA_TOKEN = "token"
        private const val EXTRA_RESULT_CODE = "resultCode"
        private const val EXTRA_RESULT_DATA = "resultData"
        private const val CHANNEL_ID = "mirror"
        private const val NOTIFICATION_ID = 41

        fun start(
            context: Context,
            host: String,
            port: Int,
            token: String,
            resultCode: Int,
            data: Intent,
        ) {
            val intent = Intent(context, MirrorService::class.java)
                .setAction(ACTION_START)
                .putExtra(EXTRA_HOST, host)
                .putExtra(EXTRA_PORT, port)
                .putExtra(EXTRA_TOKEN, token)
                .putExtra(EXTRA_RESULT_CODE, resultCode)
                .putExtra(EXTRA_RESULT_DATA, data)
            if (Build.VERSION.SDK_INT >= 26) context.startForegroundService(intent)
            else context.startService(intent)
        }

        fun stop(context: Context) {
            context.startService(Intent(context, MirrorService::class.java).setAction(ACTION_STOP))
        }
    }
}
