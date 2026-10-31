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
    @Volatile private var fps = FPS
    private var codec: MediaCodec? = null
    private var display: VirtualDisplay? = null
    @Volatile private var relay: GlRelay? = null
    private var drain: Thread? = null
    @Volatile private var running = false
    @Volatile private var params: ByteArray? = null
    private var wallBaseUs = 0L
    private var monoBaseUs = 0L

    fun isRunning() = running

    @Synchronized
    fun start(projection: MediaProjection, width: Int, height: Int, densityDpi: Int, fps: Int = FPS) {
        // Only the codec is rebuilt. Releasing the VirtualDisplay would end the
        // projection for good: its token dies with the last display, and Android
        // then rejects any further createVirtualDisplay on that instance.
        stopCodec()
        this.fps = fps.coerceIn(15, 60)
        val codec = createCodec(width, height)
        val codecInput = codec.createInputSurface()
        codec.start()

        this.codec = codec
        this.width = width
        this.height = height
        params = null
        wallBaseUs = System.currentTimeMillis() * 1000
        monoBaseUs = System.nanoTime() / 1000
        running = true

        // The display composes at the panel rate; the relay is what actually
        // holds the stream at FPS. If GL is unavailable we still encode, just
        // at whatever rate the compositor delivers.
        val relay = GlRelay(codecInput, width, height, this.fps)
        val relaySurface = try {
            relay.start()
        } catch (error: Exception) {
            relay.stop()
            MirrorState.log("GL 限帧不可用（${error.message}），按屏幕刷新率编码")
            null
        }
        this.relay = if (relaySurface != null) relay else null

        val surface = relaySurface ?: codecInput
        val live = display
        if (live == null) {
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
        } else {
            live.setSurface(surface)
            live.resize(width, height, densityDpi)
        }

        drain = Thread({ drainLoop(codec) }, "video-drain").apply { start() }
        MirrorState.resolution = "${width}×$height · ${this.fps} fps"
        MirrorState.log(
            "编码 ${width}×$height @${this.fps}fps ${bitrate(width, height, this.fps) / 1000} kbps" +
                if (relaySurface != null) " · GL 限帧" else "",
        )
    }

    private fun createCodec(width: Int, height: Int): MediaCodec {
        val type = MediaFormat.MIMETYPE_VIDEO_AVC
        val codec = MediaCodec.createEncoderByType(type)
        try {
            codec.configure(buildFormat(width, height, baseline = true), null, null, FLAG_ENCODE)
            MirrorState.log("编码器 ${codec.name}")
            return codec
        } catch (error: Exception) {
            MirrorState.log("编码器不接受 Baseline（${error.message}），退回默认 profile")
        }
        try {
            codec.release()
        } catch (_: Exception) {
        }
        val retry = MediaCodec.createEncoderByType(type)
        retry.configure(buildFormat(width, height, baseline = false), null, null, FLAG_ENCODE)
        MirrorState.log("编码器 ${retry.name}")
        return retry
    }

    private fun buildFormat(width: Int, height: Int, baseline: Boolean): MediaFormat =
        MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(
                MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface,
            )
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate(width, height, fps))
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, KEYFRAME_SECONDS)
            setInteger(
                MediaFormat.KEY_BITRATE_MODE,
                MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR,
            )
            // Baseline has no B-frames and no CABAC, which is what the receiver's
            // software decoder wants; the other keys ask for real-time behaviour
            // instead of the codec's default buffering.
            if (baseline) {
                setInteger(
                    MediaFormat.KEY_PROFILE,
                    MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline,
                )
                setInteger(
                    MediaFormat.KEY_LEVEL,
                    if (fps > 30) MediaCodecInfo.CodecProfileLevel.AVCLevel42
                    else MediaCodecInfo.CodecProfileLevel.AVCLevel41,
                )
                setInteger("max-bframes", 0)
            }
            setInteger(MediaFormat.KEY_MAX_FPS_TO_ENCODER, fps)
            setInteger(MediaFormat.KEY_OPERATING_RATE, fps)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            setInteger(MediaFormat.KEY_LATENCY, 1)
            // Ask the encoder to inline SPS/PPS ahead of every IDR; devices that
            // ignore the key still get them from [prependParams].
            setInteger("prepend-sps-pps-to-idr", 1)
        }

    /** Rebuilds the encoder at a new size; the projection itself survives. */
    @Synchronized
    fun resize(projection: MediaProjection, width: Int, height: Int, densityDpi: Int, fps: Int = this.fps) {
        if (!running || (width == this.width && height == this.height)) return
        start(projection, width, height, densityDpi, fps)
    }

    @Synchronized
    fun stop() {
        stopCodec()
        display?.release()
        display = null
    }

    private fun stopCodec() {
        running = false
        relay?.stop()
        relay = null
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

    /** Frames the relay blitted since the previous call, plus the average cost. */
    fun relayStats(): Pair<Int, Double>? = relay?.takeStats()

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

        private const val FLAG_ENCODE = MediaCodec.CONFIGURE_FLAG_ENCODE

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
