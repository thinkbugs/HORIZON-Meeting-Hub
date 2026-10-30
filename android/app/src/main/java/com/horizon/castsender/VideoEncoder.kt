package com.horizon.castsender

import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Bundle
import java.nio.ByteBuffer

/**
 * MediaProjection → VirtualDisplay → MediaCodec AVC (surface input) → Annex B.
 *
 * Timestamps follow `extension/offscreen.js`: the receiver stores a frame with
 * the sender's wall clock, so pts is remapped from the monotonic clock the
 * compositor stamps frames with onto wall-clock microseconds.
 */
class VideoEncoder(private val sink: Sink) {
    interface Sink {
        fun onVideo(data: ByteArray, keyframe: Boolean, width: Int, height: Int, ptsUs: Long)
    }

    @Volatile private var width = 0
    @Volatile private var height = 0
    private var codec: MediaCodec? = null
    private var display: VirtualDisplay? = null
    private var drain: Thread? = null
    @Volatile private var running = false
    @Volatile private var params: ByteArray? = null
    private var wallBaseUs = 0L
    private var monoBaseUs = 0L

    fun isRunning() = running

    @Synchronized
    fun start(projection: MediaProjection, width: Int, height: Int, densityDpi: Int) {
        stopInternal()
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(
                MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface,
            )
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate(width, height, FPS))
            setInteger(MediaFormat.KEY_FRAME_RATE, FPS)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, KEYFRAME_SECONDS)
            setInteger(
                MediaFormat.KEY_BITRATE_MODE,
                MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR,
            )
            // Ask the encoder to inline SPS/PPS ahead of every IDR; devices that
            // ignore the key still get them from [prependParams].
            setInteger("prepend-sps-pps-to-idr", 1)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        val surface = codec.createInputSurface()
        codec.start()

        this.codec = codec
        this.width = width
        this.height = height
        wallBaseUs = System.currentTimeMillis() * 1000
        monoBaseUs = System.nanoTime() / 1000
        running = true

        display = projection.createVirtualDisplay(
            "horizon-cast",
            width,
            height,
            densityDpi,
            android.hardware.display.DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            surface,
            null,
            null,
        )

        drain = Thread({ drainLoop(codec) }, "video-drain").apply { start() }
        MirrorState.log("编码 ${width}×$height @${FPS}fps ${bitrate(width, height, FPS) / 1000} kbps")
    }

    /** Rebuilds the encoder at a new size; the projection itself survives. */
    @Synchronized
    fun resize(projection: MediaProjection, width: Int, height: Int, densityDpi: Int) {
        if (!running || (width == this.width && height == this.height)) return
        start(projection, width, height, densityDpi)
    }

    @Synchronized
    fun stop() {
        stopInternal()
    }

    private fun stopInternal() {
        running = false
        display?.release()
        display = null
        val codec = codec
        this.codec = null
        if (codec != null) {
            try {
                codec.stop()
            } catch (_: Exception) {
            }
            try {
                codec.release()
            } catch (_: Exception) {
            }
        }
        drain?.let { thread ->
            try {
                thread.join(500)
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
            }
        }
        drain = null
    }

    fun requestKeyFrame() {
        val codec = codec ?: return
        try {
            codec.setParameters(Bundle().apply {
                putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
            })
        } catch (_: IllegalStateException) {
        }
    }

    private fun drainLoop(codec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val index = try {
                codec.dequeueOutputBuffer(info, 10_000)
            } catch (_: IllegalStateException) {
                break
            } catch (_: Exception) {
                break
            }
            when {
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> captureParams(codec.outputFormat)
                index >= 0 -> {
                    if (info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                        val buffer = codec.getOutputBuffer(index)
                        if (buffer != null) {
                            buffer.position(info.offset)
                            buffer.limit(info.offset + info.size)
                            val raw = ByteArray(info.size)
                            buffer.get(raw)
                            val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                            val annex = AnnexB.toAnnexB(raw)
                            val payload = if (key) prependParams(annex) else annex
                            val pts = remapPts(info.presentationTimeUs)
                            sink.onVideo(payload, key, width, height, pts)
                        }
                    }
                    try {
                        codec.releaseOutputBuffer(index, false)
                    } catch (_: IllegalStateException) {
                        break
                    }
                }
            }
        }
    }

    private fun captureParams(format: MediaFormat?) {
        val sps = format?.getByteBuffer("csd-0")?.let(::readAll) ?: return
        val pps = format.getByteBuffer("csd-1")?.let(::readAll) ?: return
        params = AnnexB.withStartCode(sps) + AnnexB.withStartCode(pps)
    }

    private fun prependParams(annex: ByteArray): ByteArray {
        val sets = params ?: return annex
        val out = ByteArray(sets.size + annex.size)
        System.arraycopy(sets, 0, out, 0, sets.size)
        System.arraycopy(annex, 0, out, sets.size, annex.size)
        return out
    }

    /** Compositor frames are stamped in elapsed-realtime µs; map them to wall clock. */
    private fun remapPts(presentationTimeUs: Long): Long {
        val nowUs = System.currentTimeMillis() * 1000
        if (presentationTimeUs <= 0) return nowUs
        val mapped = wallBaseUs + (presentationTimeUs - monoBaseUs)
        return if (mapped in (wallBaseUs - 5_000_000)..(nowUs + 100_000)) mapped else nowUs
    }

    companion object {
        const val FPS = 30
        const val KEYFRAME_SECONDS = 5

        fun bitrate(width: Int, height: Int, fps: Int): Int =
            (width.toLong() * height * fps * 0.12).toInt().coerceIn(1_500_000, 12_000_000)

        private fun readAll(buffer: ByteBuffer): ByteArray {
            val copy = buffer.duplicate()
            val out = ByteArray(copy.remaining())
            copy.get(out)
            return out
        }
    }
}
