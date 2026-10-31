package com.horizon.castsender

import android.graphics.SurfaceTexture
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLExt
import android.opengl.EGLSurface
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/**
 * Paces composition to [fps] before the encoder.
 *
 * A VirtualDisplay mirrors at the panel refresh rate (~60 Hz); for surface input
 * MediaFormat.KEY_FRAME_RATE is only a rate-control hint, so the encoder emits
 * every composition. This relay samples the newest composed frame at most once
 * per frame interval and blits it onto the codec's input surface, so the
 * bitstream carries the intended frame rate instead of double.
 */
class GlRelay(
    private val output: Surface,
    private val width: Int,
    private val height: Int,
    private val fps: Int,
) {
    private val lock = Object()
    private val ready = CountDownLatch(1)
    private val swaps = AtomicLong()
    private val swapNanos = AtomicLong()

    @Volatile private var running = false
    @Volatile private var failure: Exception? = null
    @Volatile private var producerSurface: Surface? = null

    private var thread: Thread? = null

    private var eglDisplay: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var eglContext: EGLContext = EGL14.EGL_NO_CONTEXT
    private var eglSurface: EGLSurface = EGL14.EGL_NO_SURFACE
    private var textureId = 0
    private var program = 0
    private var aPosition = 0
    private var aTexCoord = 0
    private var uMatrix = 0
    private var uTexture = 0
    private var surfaceTexture: SurfaceTexture? = null
    private var callbackThread: HandlerThread? = null
    private var frameAvailable = false

    private val vertices = floatBuffer(
        floatArrayOf(-1f, -1f, 1f, -1f, -1f, 1f, 1f, 1f),
    )
    private val texCoords = floatBuffer(
        floatArrayOf(0f, 0f, 1f, 0f, 0f, 1f, 1f, 1f),
    )

    /** Surface for the VirtualDisplay; blocks until GL is initialised. */
    fun start(): Surface {
        running = true
        thread = Thread({ runRelay() }, "gl-relay").apply { start() }
        if (!ready.await(4, TimeUnit.SECONDS)) {
            stop()
            throw IllegalStateException("GL 中转初始化超时")
        }
        failure?.let { error ->
            stop()
            throw error
        }
        return producerSurface ?: throw IllegalStateException("GL 中转没有输出面")
    }

    fun stop() {
        running = false
        val worker = thread
        thread = null
        if (worker != null) {
            try {
                worker.join(1500)
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
            }
        }
        try {
            producerSurface?.release()
        } catch (_: Exception) {
        }
        producerSurface = null
        val st = surfaceTexture
        surfaceTexture = null
        if (st != null) {
            try {
                st.setOnFrameAvailableListener(null)
                st.release()
            } catch (_: Exception) {
            }
        }
        callbackThread?.quitSafely()
        callbackThread = null
    }

    private fun runRelay() {
        try {
            initGl()
            ready.countDown()
            renderLoop()
        } catch (error: Exception) {
            failure = error
            ready.countDown()
        } finally {
            releaseGl()
        }
    }

    private fun initGl() {
        eglDisplay = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        check(eglDisplay != EGL14.EGL_NO_DISPLAY) { "没有 EGL display" }
        val version = IntArray(2)
        check(EGL14.eglInitialize(eglDisplay, version, 0, version, 1)) { "eglInitialize 失败" }

        val configs = arrayOfNulls<EGLConfig>(1)
        val count = IntArray(1)
        val attribs = intArrayOf(
            EGL14.EGL_RED_SIZE, 8,
            EGL14.EGL_GREEN_SIZE, 8,
            EGL14.EGL_BLUE_SIZE, 8,
            EGL14.EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
            EGL_RECORDABLE_ANDROID, 1,
            EGL14.EGL_NONE,
        )
        check(
            EGL14.eglChooseConfig(eglDisplay, attribs, 0, configs, 0, 1, count, 0) &&
                count[0] > 0 &&
                configs[0] != null,
        ) { "没有可用的 EGL 配置" }

        eglContext = EGL14.eglCreateContext(
            eglDisplay,
            configs[0],
            EGL14.EGL_NO_CONTEXT,
            intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 2, EGL14.EGL_NONE),
            0,
        )
        check(eglContext != EGL14.EGL_NO_CONTEXT) { "eglCreateContext 失败" }

        eglSurface = EGL14.eglCreateWindowSurface(
            eglDisplay,
            configs[0],
            output,
            intArrayOf(EGL14.EGL_NONE),
            0,
        )
        check(eglSurface != EGL14.EGL_NO_SURFACE) { "编码器不接受 EGL 输出面" }
        check(EGL14.eglMakeCurrent(eglDisplay, eglSurface, eglSurface, eglContext)) {
            "eglMakeCurrent 失败"
        }

        program = buildProgram()
        aPosition = GLES20.glGetAttribLocation(program, "aPosition")
        aTexCoord = GLES20.glGetAttribLocation(program, "aTexCoord")
        uMatrix = GLES20.glGetUniformLocation(program, "uMatrix")
        uTexture = GLES20.glGetUniformLocation(program, "sTexture")

        val ids = IntArray(1)
        GLES20.glGenTextures(1, ids, 0)
        textureId = ids[0]
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, textureId)
        GLES20.glTexParameteri(
            GLES11Ext.GL_TEXTURE_EXTERNAL_OES,
            GLES20.GL_TEXTURE_MIN_FILTER,
            GLES20.GL_LINEAR,
        )
        GLES20.glTexParameteri(
            GLES11Ext.GL_TEXTURE_EXTERNAL_OES,
            GLES20.GL_TEXTURE_MAG_FILTER,
            GLES20.GL_LINEAR,
        )
        GLES20.glTexParameteri(
            GLES11Ext.GL_TEXTURE_EXTERNAL_OES,
            GLES20.GL_TEXTURE_WRAP_S,
            GLES20.GL_CLAMP_TO_EDGE,
        )
        GLES20.glTexParameteri(
            GLES11Ext.GL_TEXTURE_EXTERNAL_OES,
            GLES20.GL_TEXTURE_WRAP_T,
            GLES20.GL_CLAMP_TO_EDGE,
        )

        val st = SurfaceTexture(textureId)
        st.setDefaultBufferSize(width, height)
        // Frame callbacks must not share the app's main looper: a UI frame there
        // delays the wake-up, and every late wake-up is a missed sample.
        val frames = HandlerThread("gl-frames").apply { start() }
        callbackThread = frames
        st.setOnFrameAvailableListener(
            {
                synchronized(lock) {
                    frameAvailable = true
                    lock.notifyAll()
                }
            },
            Handler(frames.looper),
        )
        surfaceTexture = st
        producerSurface = Surface(st)
    }

    private fun renderLoop() {
        val intervalNs = 1_000_000_000L / fps
        val matrix = FloatArray(16)
        // Absolute schedule: a slow swap must not push every later frame out by
        // the same amount, so wake-ups sit on a fixed grid and a missed slot is
        // skipped instead of caught up in a burst.
        var nextTickNs = System.nanoTime() + intervalNs
        while (running) {
            val waitNs = nextTickNs - System.nanoTime()
            if (waitNs > 0) {
                try {
                    Thread.sleep(waitNs / 1_000_000, (waitNs % 1_000_000).toInt())
                } catch (_: InterruptedException) {
                    break
                }
                continue
            }
            nextTickNs += intervalNs
            if (nextTickNs < System.nanoTime()) nextTickNs = System.nanoTime() + intervalNs

            val fresh = synchronized(lock) {
                val had = frameAvailable
                frameAvailable = false
                had
            }
            if (!fresh) continue

            val st = surfaceTexture ?: break
            st.updateTexImage()
            st.getTransformMatrix(matrix)
            val ptsNs = st.timestamp

            GLES20.glViewport(0, 0, width, height)
            GLES20.glClearColor(0f, 0f, 0f, 1f)
            GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
            GLES20.glUseProgram(program)
            GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
            GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, textureId)
            GLES20.glUniform1i(uTexture, 0)
            GLES20.glUniformMatrix4fv(uMatrix, 1, false, matrix, 0)
            drawQuad()

            EGLExt.eglPresentationTimeANDROID(eglDisplay, eglSurface, ptsNs)
            val swapStart = System.nanoTime()
            if (!EGL14.eglSwapBuffers(eglDisplay, eglSurface)) {
                throw IllegalStateException("eglSwapBuffers 失败：${EGL14.eglGetError()}")
            }
            swapNanos.addAndGet(System.nanoTime() - swapStart)
            swaps.incrementAndGet()
        }
    }

    /** Frames blitted since the previous call, plus the average blit cost. */
    fun takeStats(): Pair<Int, Double> {
        val frames = swaps.getAndSet(0)
        val nanos = swapNanos.getAndSet(0)
        return frames.toInt() to if (frames > 0) nanos.toDouble() / frames / 1_000_000 else 0.0
    }

    private fun drawQuad() {
        vertices.position(0)
        GLES20.glEnableVertexAttribArray(aPosition)
        GLES20.glVertexAttribPointer(aPosition, 2, GLES20.GL_FLOAT, false, 0, vertices)
        texCoords.position(0)
        GLES20.glEnableVertexAttribArray(aTexCoord)
        GLES20.glVertexAttribPointer(aTexCoord, 2, GLES20.GL_FLOAT, false, 0, texCoords)
        GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)
    }

    private fun releaseGl() {
        running = false
        val display = eglDisplay
        if (display != EGL14.EGL_NO_DISPLAY) {
            EGL14.eglMakeCurrent(
                display,
                EGL14.EGL_NO_SURFACE,
                EGL14.EGL_NO_SURFACE,
                EGL14.EGL_NO_CONTEXT,
            )
            if (eglSurface != EGL14.EGL_NO_SURFACE) {
                EGL14.eglDestroySurface(display, eglSurface)
                eglSurface = EGL14.EGL_NO_SURFACE
            }
            if (eglContext != EGL14.EGL_NO_CONTEXT) {
                EGL14.eglDestroyContext(display, eglContext)
                eglContext = EGL14.EGL_NO_CONTEXT
            }
            EGL14.eglTerminate(display)
        }
        eglDisplay = EGL14.EGL_NO_DISPLAY
    }

    private fun buildProgram(): Int {
        val vertex = compile(GLES20.GL_VERTEX_SHADER, VERTEX_SHADER)
        val fragment = compile(GLES20.GL_FRAGMENT_SHADER, FRAGMENT_SHADER)
        val program = GLES20.glCreateProgram()
        GLES20.glAttachShader(program, vertex)
        GLES20.glAttachShader(program, fragment)
        GLES20.glLinkProgram(program)
        val status = IntArray(1)
        GLES20.glGetProgramiv(program, GLES20.GL_LINK_STATUS, status, 0)
        check(status[0] == GLES20.GL_TRUE) {
            "着色器链接失败：${GLES20.glGetProgramInfoLog(program)}"
        }
        GLES20.glDeleteShader(vertex)
        GLES20.glDeleteShader(fragment)
        return program
    }

    private fun compile(type: Int, source: String): Int {
        val shader = GLES20.glCreateShader(type)
        GLES20.glShaderSource(shader, source)
        GLES20.glCompileShader(shader)
        val status = IntArray(1)
        GLES20.glGetShaderiv(shader, GLES20.GL_COMPILE_STATUS, status, 0)
        check(status[0] == GLES20.GL_TRUE) {
            "着色器编译失败：${GLES20.glGetShaderInfoLog(shader)}"
        }
        return shader
    }

    private companion object {
        const val EGL_OPENGL_ES2_BIT = 4
        const val EGL_RECORDABLE_ANDROID = 0x3142

        const val VERTEX_SHADER = """
            uniform mat4 uMatrix;
            attribute vec4 aPosition;
            attribute vec4 aTexCoord;
            varying vec2 vTexCoord;
            void main() {
                gl_Position = aPosition;
                vTexCoord = (uMatrix * aTexCoord).xy;
            }
        """

        const val FRAGMENT_SHADER = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;
            varying vec2 vTexCoord;
            uniform samplerExternalOES sTexture;
            void main() {
                gl_FragColor = texture2D(sTexture, vTexCoord);
            }
        """

        fun floatBuffer(values: FloatArray): java.nio.FloatBuffer =
            ByteBuffer.allocateDirect(values.size * 4)
                .order(ByteOrder.nativeOrder())
                .asFloatBuffer()
                .apply {
                    put(values)
                    position(0)
                }
    }
}
