package dev.fcvm.robotpark

import android.content.Context
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLSurface
import android.opengl.GLES30
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import dev.fcvm.robotpark.scene.Command
import dev.fcvm.robotpark.scene.FrameState
import dev.fcvm.robotpark.scene.Tile
import java.io.File
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

/**
 * The render thread: owns EGL and every GL object. The context lives on a 1x1 pbuffer, so it
 * outlives the window: losing the surface (onSurfaceDestroyed) only drops the window surface,
 * and a new one (onSurfaceCreated) is drawn on at once. EGL_CONTEXT_LOST from any EGL call
 * tears down the display, the context and every GL name, and builds them again, as phones mode
 * needs when the emulator's renderer lets go of the GPU and takes it back.
 */
class GlThread(private val context: Context) : Thread("robotpark-gl") {
    private val lock = Object()
    // guarded by lock: what the UI thread asks for
    private var window: Surface? = null
    private var windowW = 0
    private var windowH = 0
    private var windowGen = 0
    private var bound: Surface? = null
    private var paused = false
    private var quit = false
    private val tasks = ArrayDeque<() -> Unit>()
    /** Called on the UI thread's behalf when the frame's chrome flag changes. */
    @Volatile var onChrome: ((Boolean) -> Unit)? = null

    // the GL thread's own
    private var dpy: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var cfg: EGLConfig? = null
    private var ctx: EGLContext = EGL14.EGL_NO_CONTEXT
    private var pbuffer: EGLSurface = EGL14.EGL_NO_SURFACE
    private var win: EGLSurface = EGL14.EGL_NO_SURFACE
    private var boundGen = -1
    private var renderer: SceneRenderer? = null
    private var lastChrome: Boolean? = null
    private var fpsFrames = 0
    private var fpsSince = 0L

    /** Hands the thread a window, or takes it away (null): returns only once the thread has let go of the old one. */
    fun setWindow(s: Surface?, w: Int, h: Int) {
        synchronized(lock) {
            window = s; windowW = w; windowH = h; windowGen++
            lock.notifyAll()
            if (s == null) {
                val deadline = SystemClock.uptimeMillis() + 3000
                while (bound != null && isAlive && SystemClock.uptimeMillis() < deadline) lock.wait(100)
            }
        }
    }

    fun setPaused(p: Boolean) = synchronized(lock) { paused = p; lock.notifyAll() }

    fun quit() {
        synchronized(lock) { quit = true; lock.notifyAll() }
        join(3000)
    }

    private fun <T> run(timeoutS: Long, f: () -> T): T {
        val fut = CompletableFuture<T>()
        synchronized(lock) {
            tasks.addLast { try { fut.complete(f()) } catch (e: Throwable) { fut.completeExceptionally(e) } }
            lock.notifyAll()
        }
        return fut.get(timeoutS, TimeUnit.SECONDS)
    }

    /** Draws one frame offscreen at the capture's t, rect and size, and writes its RGBA rows top first. */
    fun capture(c: Command.Capture): Map<String, Any?> = run(30) {
        val r = renderer ?: return@run mapOf("error" to "no GL context")
        val scene = Store.effectiveScene().copy(rect = c.rect, imageAspect = c.imageAspect, previousRect = null, rectFromFrame = null)
        val tile = Tile.of(c.rect, c.imageAspect, c.w, c.h)
        val fs = FrameState.compute(scene, c.t, c.w.toDouble() / c.h, Store.systemNight)
        val pixels = r.drawOffscreen(fs, tile, Store.robots)
        val dir = context.getExternalFilesDir(null) ?: context.filesDir
        val f = File(dir, "${c.name}.rgba")
        f.writeBytes(pixels)
        var h = -0x340d631b7bdddcdbL // FNV-1a 64 offset basis
        for (b in pixels) { h = h xor (b.toLong() and 0xff); h *= 0x100000001b3L }
        PhoneApp.ok("path" to f.path, "w" to c.w, "h" to c.h, "t" to c.t, "x0" to tile.x0, "y0" to tile.y0,
            "full_w" to tile.fullW, "full_h" to tile.fullH, "fnv64" to java.lang.Long.toUnsignedString(h, 16))
    }

    /** Tears the context down and builds it again, the path EGL_CONTEXT_LOST takes. */
    fun dropContext(): Map<String, Any?> = run(30) {
        renderer?.release()
        teardown("asked to drop the context")
        ensureContext()
        PhoneApp.ok("context_generation" to Store.contextGeneration)
    }

    /** Draws the scene offscreen at rising robot counts and reports the time per frame. */
    fun benchmark() {
        if (Store.benchmarkRunning) return
        Store.benchmarkRunning = true
        Store.benchmark = null
        synchronized(lock) {
            tasks.addLast {
                try {
                    val scene = Store.effectiveScene()
                    Store.benchmark = renderer?.benchmark(scene, Store.frameTime(scene),
                        if (Store.surfaceW > 0) Store.surfaceW else 720, if (Store.surfaceH > 0) Store.surfaceH else 1280)
                } finally {
                    Store.benchmarkRunning = false
                }
            }
            lock.notifyAll()
        }
    }

    override fun run() {
        try {
            loop()
        } catch (e: Throwable) {
            Log.e(TAG, "render thread failed", e)
            Store.lastError = "render thread: ${e.message}"
        } finally {
            renderer?.release()
            teardown("render thread exiting")
            synchronized(lock) { bound = null; lock.notifyAll() }
        }
    }

    private fun loop() {
        while (true) {
            var hasTask: Boolean
            var want: Surface?
            var w: Int
            var h: Int
            var gen: Int
            var draw: Boolean
            synchronized(lock) {
                while (true) {
                    if (quit) return
                    hasTask = tasks.isNotEmpty()
                    draw = window != null && !paused
                    if (hasTask || draw || windowGen != boundGen) break
                    lock.wait()
                }
                want = window; w = windowW; h = windowH; gen = windowGen
            }
            if (!ensureContext()) { SystemClock.sleep(500); continue }
            if (gen != boundGen) rebind(want, gen)
            if (hasTask) {
                // a task leaves the queue only with a current context: a lost one delays it
                if (makeCurrent(if (win != EGL14.EGL_NO_SURFACE) win else pbuffer)) {
                    val task = synchronized(lock) { tasks.removeFirstOrNull() }
                    survive("task") { task?.invoke() }
                } else SystemClock.sleep(50)
                continue
            }
            if (draw && win != EGL14.EGL_NO_SURFACE) survive("frame") { frame(w, h) }
            else if (draw) SystemClock.sleep(50) // a window we could not bind: wait for the next one
        }
    }

    /**
     * Runs one frame or task; a failure (a scene too large for the GPU's framebuffers, a
     * framebuffer the driver refused) is reported and the thread goes on, so the next scene
     * draws again. It backs off 200 ms so a scene that fails every frame does not flood the log.
     */
    private fun survive(what: String, f: () -> Unit) {
        try {
            f()
        } catch (e: RuntimeException) {
            Log.e(TAG, "$what failed", e)
            Store.lastError = "$what: ${e.message}"
            SystemClock.sleep(200)
        }
    }

    private fun frame(w: Int, h: Int) {
        if (!makeCurrent(win)) return
        val r = renderer ?: return
        val t0 = SystemClock.elapsedRealtimeNanos()
        val scene = Store.effectiveScene()
        val t = Store.frameTime(scene)
        val (rect, aspect) = scene.viewAt(t, w, h)
        val tile = Tile.of(rect, aspect, w, h)
        val fs = FrameState.compute(scene.copy(imageAspect = aspect), t, w.toDouble() / h, Store.systemNight)
        r.drawToWindow(fs, tile, Store.robots, if (scene.stamp) fs.frameIndex else -1)
        if (!EGL14.eglSwapBuffers(dpy, win)) {
            eglFailed("eglSwapBuffers")
            return
        }
        Store.frameMs = (SystemClock.elapsedRealtimeNanos() - t0) / 1e6
        Store.lastT = t
        Store.frames++
        Store.surfaceW = w; Store.surfaceH = h
        fpsFrames++
        val now = SystemClock.elapsedRealtime()
        if (fpsSince == 0L) fpsSince = now
        if (now - fpsSince >= 1000) {
            Store.fps = fpsFrames * 1000.0 / (now - fpsSince)
            Log.i(TAG, String.format("fps=%.1f t=%.3f frame_ms=%.2f surface=%dx%d robots=%d", Store.fps, t, Store.frameMs, w, h, Store.robots))
            fpsFrames = 0; fpsSince = now
        }
        if (fs.chrome != lastChrome) { lastChrome = fs.chrome; onChrome?.invoke(fs.chrome) }
    }

    private fun ensureContext(): Boolean {
        if (ctx != EGL14.EGL_NO_CONTEXT) return true
        dpy = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        val v = IntArray(2)
        if (dpy == EGL14.EGL_NO_DISPLAY || !EGL14.eglInitialize(dpy, v, 0, v, 1)) { Store.lastError = "eglInitialize failed"; return false }
        val attrs = intArrayOf(
            EGL14.EGL_RED_SIZE, 8, EGL14.EGL_GREEN_SIZE, 8, EGL14.EGL_BLUE_SIZE, 8, EGL14.EGL_ALPHA_SIZE, 8,
            EGL14.EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
            EGL14.EGL_SURFACE_TYPE, EGL14.EGL_WINDOW_BIT or EGL14.EGL_PBUFFER_BIT,
            EGL14.EGL_NONE,
        )
        val configs = arrayOfNulls<EGLConfig>(1)
        val n = IntArray(1)
        if (!EGL14.eglChooseConfig(dpy, attrs, 0, configs, 0, 1, n, 0) || n[0] == 0) { Store.lastError = "no RGBA8 ES3 EGL config"; return false }
        cfg = configs[0]
        ctx = EGL14.eglCreateContext(dpy, cfg, EGL14.EGL_NO_CONTEXT, intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 3, EGL14.EGL_NONE), 0)
        if (ctx == EGL14.EGL_NO_CONTEXT) { Store.lastError = "eglCreateContext failed: 0x${Integer.toHexString(EGL14.eglGetError())}"; return false }
        pbuffer = EGL14.eglCreatePbufferSurface(dpy, cfg, intArrayOf(EGL14.EGL_WIDTH, 1, EGL14.EGL_HEIGHT, 1, EGL14.EGL_NONE), 0)
        if (!EGL14.eglMakeCurrent(dpy, pbuffer, pbuffer, ctx)) { eglFailed("eglMakeCurrent(pbuffer)"); return false }
        val r = SceneRenderer(context.assets)
        r.init()
        renderer = r
        Store.renderer = GLES30.glGetString(GLES30.GL_RENDERER) ?: ""
        Store.vendor = GLES30.glGetString(GLES30.GL_VENDOR) ?: ""
        Store.glVersion = GLES30.glGetString(GLES30.GL_VERSION) ?: ""
        Store.glsl = GLES30.glGetString(GLES30.GL_SHADING_LANGUAGE_VERSION) ?: ""
        Store.samples = r.samples
        Store.contextGeneration++
        boundGen = -1
        Log.i(TAG, "renderer=${Store.renderer} vendor=${Store.vendor} version=${Store.glVersion} samples=${r.samples} context_generation=${Store.contextGeneration}")
        return true
    }

    private fun rebind(want: Surface?, gen: Int) {
        if (win != EGL14.EGL_NO_SURFACE) {
            EGL14.eglMakeCurrent(dpy, pbuffer, pbuffer, ctx)
            EGL14.eglDestroySurface(dpy, win)
            win = EGL14.EGL_NO_SURFACE
        }
        synchronized(lock) { bound = null; lock.notifyAll() }
        boundGen = gen
        if (want == null || !want.isValid) return
        win = EGL14.eglCreateWindowSurface(dpy, cfg, want, intArrayOf(EGL14.EGL_NONE), 0)
        if (win == EGL14.EGL_NO_SURFACE) {
            Log.e(TAG, "eglCreateWindowSurface failed: 0x${Integer.toHexString(EGL14.eglGetError())}")
            return
        }
        synchronized(lock) { bound = want }
        Log.i(TAG, "window surface bound (generation $gen)")
    }

    private fun makeCurrent(s: EGLSurface): Boolean {
        if (EGL14.eglMakeCurrent(dpy, s, s, ctx)) return true
        eglFailed("eglMakeCurrent")
        return false
    }

    /** An EGL call failed: a lost context is rebuilt, a dead window is dropped until the next one. */
    private fun eglFailed(what: String) {
        val err = EGL14.eglGetError()
        Log.w(TAG, "$what failed: 0x${Integer.toHexString(err)}")
        when (err) {
            EGL14.EGL_CONTEXT_LOST -> {
                renderer?.forget()
                teardown("EGL_CONTEXT_LOST")
            }
            EGL14.EGL_BAD_SURFACE, EGL14.EGL_BAD_NATIVE_WINDOW, EGL14.EGL_BAD_CURRENT_SURFACE -> {
                if (win != EGL14.EGL_NO_SURFACE) {
                    EGL14.eglMakeCurrent(dpy, pbuffer, pbuffer, ctx)
                    EGL14.eglDestroySurface(dpy, win)
                    win = EGL14.EGL_NO_SURFACE
                }
                synchronized(lock) { bound = null; lock.notifyAll() }
            }
            else -> Store.lastError = "$what: EGL error 0x${Integer.toHexString(err)}"
        }
    }

    /** Lets go of everything EGL: the next loop builds a new display and context. */
    private fun teardown(why: String) {
        renderer = null
        if (dpy != EGL14.EGL_NO_DISPLAY) {
            EGL14.eglMakeCurrent(dpy, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT)
            if (win != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(dpy, win)
            if (pbuffer != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(dpy, pbuffer)
            if (ctx != EGL14.EGL_NO_CONTEXT) EGL14.eglDestroyContext(dpy, ctx)
            EGL14.eglTerminate(dpy)
            EGL14.eglReleaseThread()
        }
        dpy = EGL14.EGL_NO_DISPLAY; ctx = EGL14.EGL_NO_CONTEXT; pbuffer = EGL14.EGL_NO_SURFACE; win = EGL14.EGL_NO_SURFACE
        boundGen = -1
        lastChrome = null
        synchronized(lock) { bound = null; lock.notifyAll() }
        Log.i(TAG, "EGL torn down: $why")
    }

    companion object {
        const val TAG = "ROBOTPARK"
        const val EGL_OPENGL_ES3_BIT = 0x40
        @Volatile var current: GlThread? = null
    }
}
