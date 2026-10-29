package dev.fcvm.robotpark

import android.content.res.AssetManager
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.Typeface
import android.opengl.GLES30
import android.opengl.GLUtils
import android.os.SystemClock
import android.util.Log
import dev.fcvm.robotpark.scene.Crowd
import dev.fcvm.robotpark.scene.FrameState
import dev.fcvm.robotpark.scene.Mesh
import dev.fcvm.robotpark.scene.Meshes
import dev.fcvm.robotpark.scene.Rect
import dev.fcvm.robotpark.scene.SceneInput
import dev.fcvm.robotpark.scene.Shaders
import dev.fcvm.robotpark.scene.Tile
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.FloatBuffer
import java.nio.ShortBuffer

/**
 * Every GL object and every draw. Created on a current context ([init]); [forget] drops the
 * names without GL calls after EGL_CONTEXT_LOST, [release] deletes them while the context lives.
 *
 * A frame: the sun's shadow map, then the whole image into a multisampled framebuffer the size
 * of the whole image, scissored to this phone's tile ([Tile]), resolved, and the tile copied to
 * the window (or read back, for a capture). Every phone rasterizes in the same window
 * coordinates, so a pixel is the same on every phone that draws it: with a framebuffer the size
 * of the surface and the viewport moved to the tile instead, the GPU's attribute interpolation
 * rounded differently and about 1 pixel in 10,000 came out 1 level apart across a seam
 * (measured on the T4 through the emulator). The whole-image framebuffers cost
 * full_w x full_h x (4 samples x 8 bytes + 4) of GPU memory: 36 MB per phone of a 720 x 1280 wall.
 */
class SceneRenderer(private val assets: AssetManager) {
    private class Program(val id: Int) {
        private val locs = HashMap<String, Int>()
        fun u(name: String): Int = locs.getOrPut(name) { GLES30.glGetUniformLocation(id, name) }
    }

    private class Batch(val vao: Int, val indexCount: Int, var instances: Int, val instanceVbo: Int, val strip: Boolean = false, val vertexCount: Int = 0)

    var samples = 0
        private set
    private val progs = HashMap<String, Program>()
    private val buffers = ArrayList<Int>()
    private val vaos = ArrayList<Int>()
    private lateinit var robots: Batch
    private lateinit var carriers: Batch
    private lateinit var buddies: Batch
    private lateinit var roundTrees: Batch
    private lateinit var pineTrees: Batch
    private lateinit var flowers: Batch
    private lateinit var hills: Batch
    private lateinit var clouds: Batch
    private lateinit var orbs: Batch
    private lateinit var ground: Batch
    private lateinit var sign: Batch
    private lateinit var particles: Batch
    private var emptyVao = 0
    private var shadowFbo = 0
    private var shadowTex = 0
    private var msaaFbo = 0
    private var msaaColor = 0
    private var msaaDepth = 0
    private var msaaW = 0
    private var msaaH = 0
    private var resolveFbo = 0
    private var resolveColor = 0
    private var capFbo = 0
    private var capColor = 0
    private var capW = 0
    private var capH = 0
    private var maxViewport = IntArray(2)
    private var maxRenderbuffer = 0
    private val text = LinkedHashMap<String, Int>()
    private val palette = Crowd.paletteLinear()

    fun init() {
        val t0 = SystemClock.elapsedRealtime()
        for (p in Shaders.PROGRAMS) {
            val read = { f: String -> assets.open("shaders/$f").bufferedReader().use { it.readText() } }
            progs[p.name] = link(p.name, Shaders.source(p.vertex, p.defines, read), Shaders.source(p.fragment, p.defines, read))
        }
        for (name in listOf("robot", "prop", "ground", "sign")) {
            val p = progs.getValue(name)
            GLES30.glUseProgram(p.id)
            GLES30.glUniform1i(p.u("u_shadow"), 1)
            GLES30.glUniform1i(p.u("u_text"), 0)
        }
        val mx = IntArray(1)
        GLES30.glGetIntegerv(GLES30.GL_MAX_SAMPLES, mx, 0)
        samples = minOf(4, mx[0])
        GLES30.glGetIntegerv(GLES30.GL_MAX_VIEWPORT_DIMS, maxViewport, 0)
        GLES30.glGetIntegerv(GLES30.GL_MAX_RENDERBUFFER_SIZE, mx, 0)
        maxRenderbuffer = mx[0]

        val robotMesh = Meshes.robot()
        robots = batch(robotMesh, Crowd.robots(MAX_ROBOTS), 2)
        robots.instances = Crowd.ROBOTS
        carriers = batch(robotMesh, FloatArray(FrameState.MAX_SIGNS * 8), 2, dynamic = true)
        buddies = batch(Meshes.buddy(), Crowd.buddies(Crowd.BUDDIES), 3)
        val trees = Crowd.trees()
        val (pine, round) = (0 until Crowd.TREES).partition { trees[it * 12 + 7] > 0.5f }
        fun pick(idx: List<Int>) = FloatArray(idx.size * 12).also { a -> idx.forEachIndexed { k, i -> trees.copyInto(a, k * 12, i * 12, i * 12 + 12) } }
        roundTrees = batch(Meshes.roundTree(), pick(round), 3)
        pineTrees = batch(Meshes.pineTree(), pick(pine), 3)
        flowers = batch(Meshes.flower(), Crowd.flowers(), 3)
        val ball = Meshes.ball()
        hills = batch(ball, Crowd.hills(), 3)
        clouds = batch(Meshes.ball(16, 10), Crowd.clouds(), 3)
        val orbData = FloatArray(FrameState.MAX_LIGHTS * 12)
        for (i in 0 until FrameState.MAX_LIGHTS) floatArrayOf(0f, 0f, 0f, 0f, 0.32f, 0.32f, 0.32f, 0f, i.toFloat(), 0f, Crowd.PROP_ORB.toFloat(), 0f).copyInto(orbData, i * 12)
        orbs = batch(Meshes.ball(16, 10), orbData, 3)
        ground = batch(Meshes.ground(), null, 0)
        sign = batch(Meshes.sign(), null, 0)
        val p = Crowd.particles(Crowd.ROBOTS)
        particles = quadBatch(p)
        emptyVao = genVao()

        shadowTex = genTexture()
        GLES30.glBindTexture(GLES30.GL_TEXTURE_2D, shadowTex)
        GLES30.glTexStorage2D(GLES30.GL_TEXTURE_2D, 1, GLES30.GL_DEPTH_COMPONENT24, FrameState.SHADOW_SIZE, FrameState.SHADOW_SIZE)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_MIN_FILTER, GLES30.GL_LINEAR)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_MAG_FILTER, GLES30.GL_LINEAR)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_WRAP_S, GLES30.GL_CLAMP_TO_EDGE)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_WRAP_T, GLES30.GL_CLAMP_TO_EDGE)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_COMPARE_MODE, GLES30.GL_COMPARE_REF_TO_TEXTURE)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_COMPARE_FUNC, GLES30.GL_LEQUAL)
        shadowFbo = genFbo()
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, shadowFbo)
        GLES30.glFramebufferTexture2D(GLES30.GL_FRAMEBUFFER, GLES30.GL_DEPTH_ATTACHMENT, GLES30.GL_TEXTURE_2D, shadowTex, 0)
        GLES30.glDrawBuffers(1, intArrayOf(GLES30.GL_NONE), 0)
        GLES30.glReadBuffer(GLES30.GL_NONE)
        checkFbo("shadow")
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, 0)
        checkGl("init")
        Log.i(GlThread.TAG, "renderer ready in ${SystemClock.elapsedRealtime() - t0} ms: ${robotMesh.indices.size / 3} triangles a robot, $samples samples, max viewport ${maxViewport[0]}x${maxViewport[1]}")
    }

    /** After EGL_CONTEXT_LOST: the names died with the context. */
    fun forget() {
        progs.clear(); buffers.clear(); vaos.clear(); text.clear()
        msaaFbo = 0; msaaColor = 0; msaaDepth = 0; msaaW = 0; msaaH = 0; resolveFbo = 0; resolveColor = 0
        capFbo = 0; capColor = 0; capW = 0; capH = 0; shadowFbo = 0; shadowTex = 0
    }

    fun release() {
        for (p in progs.values) GLES30.glDeleteProgram(p.id)
        if (buffers.isNotEmpty()) GLES30.glDeleteBuffers(buffers.size, buffers.toIntArray(), 0)
        if (vaos.isNotEmpty()) GLES30.glDeleteVertexArrays(vaos.size, vaos.toIntArray(), 0)
        val tex = (text.values + listOf(shadowTex)).filter { it != 0 }
        if (tex.isNotEmpty()) GLES30.glDeleteTextures(tex.size, tex.toIntArray(), 0)
        val rbs = listOf(msaaColor, msaaDepth, resolveColor, capColor).filter { it != 0 }
        if (rbs.isNotEmpty()) GLES30.glDeleteRenderbuffers(rbs.size, rbs.toIntArray(), 0)
        val fbs = listOf(msaaFbo, resolveFbo, capFbo, shadowFbo).filter { it != 0 }
        if (fbs.isNotEmpty()) GLES30.glDeleteFramebuffers(fbs.size, fbs.toIntArray(), 0)
        forget()
    }

    fun drawToWindow(fs: FrameState, tile: Tile, robotCount: Int, stamp: Long) {
        render(fs, tile, robotCount)
        copyOut(tile, 0)
        if (stamp >= 0) drawStamp(tile.w, stamp)
    }

    /** The tile's part of the whole image, in GL's window coordinates (origin bottom left), clipped to the image. */
    private fun region(t: Tile): IntArray {
        val gy = t.fullH - t.y0 - t.h
        val x0 = maxOf(0, t.x0); val y0 = maxOf(0, gy)
        val x1 = minOf(t.fullW, t.x0 + t.w); val y1 = minOf(t.fullH, gy + t.h)
        return intArrayOf(x0, y0, maxOf(x0, x1), maxOf(y0, y1), gy)
    }

    /** Resolves the tile's samples and copies the tile to `target`'s origin; pixels outside the image are black. */
    private fun copyOut(t: Tile, target: Int) {
        val r = region(t)
        // a multisample resolve must keep its rectangle (GLES 3.0), so resolve in place first
        GLES30.glBindFramebuffer(GLES30.GL_READ_FRAMEBUFFER, msaaFbo)
        GLES30.glBindFramebuffer(GLES30.GL_DRAW_FRAMEBUFFER, resolveFbo)
        GLES30.glBlitFramebuffer(0, 0, t.fullW, t.fullH, 0, 0, t.fullW, t.fullH, GLES30.GL_COLOR_BUFFER_BIT, GLES30.GL_NEAREST)
        GLES30.glBindFramebuffer(GLES30.GL_READ_FRAMEBUFFER, resolveFbo)
        GLES30.glBindFramebuffer(GLES30.GL_DRAW_FRAMEBUFFER, target)
        if (r[0] > t.x0 || r[1] > r[4] || r[2] < t.x0 + t.w || r[3] < r[4] + t.h) {
            GLES30.glClearColor(0f, 0f, 0f, 1f)
            GLES30.glClear(GLES30.GL_COLOR_BUFFER_BIT)
        }
        GLES30.glBlitFramebuffer(r[0], r[1], r[2], r[3], r[0] - t.x0, r[1] - r[4], r[2] - t.x0, r[3] - r[4],
            GLES30.GL_COLOR_BUFFER_BIT, GLES30.GL_NEAREST)
    }

    /** The same frame into a single-sampled framebuffer, read back as RGBA rows, top row first. */
    fun drawOffscreen(fs: FrameState, tile: Tile, robotCount: Int): ByteArray {
        render(fs, tile, robotCount)
        if (capW != tile.w || capH != tile.h) {
            capW = 0; capH = 0
            if (capColor != 0) GLES30.glDeleteRenderbuffers(1, intArrayOf(capColor), 0)
            if (capFbo == 0) capFbo = genFbo()
            capColor = genRenderbuffer()
            GLES30.glBindRenderbuffer(GLES30.GL_RENDERBUFFER, capColor)
            GLES30.glRenderbufferStorage(GLES30.GL_RENDERBUFFER, GLES30.GL_RGBA8, tile.w, tile.h)
            GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, capFbo)
            GLES30.glFramebufferRenderbuffer(GLES30.GL_FRAMEBUFFER, GLES30.GL_COLOR_ATTACHMENT0, GLES30.GL_RENDERBUFFER, capColor)
            checkFbo("capture")
            capW = tile.w; capH = tile.h
        }
        copyOut(tile, capFbo)
        GLES30.glBindFramebuffer(GLES30.GL_READ_FRAMEBUFFER, capFbo)
        val buf = ByteBuffer.allocateDirect(tile.w * tile.h * 4).order(ByteOrder.nativeOrder())
        GLES30.glPixelStorei(GLES30.GL_PACK_ALIGNMENT, 1)
        GLES30.glReadPixels(0, 0, tile.w, tile.h, GLES30.GL_RGBA, GLES30.GL_UNSIGNED_BYTE, buf)
        checkGl("capture")
        val out = ByteArray(tile.w * tile.h * 4)
        val row = tile.w * 4
        for (y in 0 until tile.h) { buf.position((tile.h - 1 - y) * row); buf.get(out, y * row, row) }
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, 0)
        return out
    }

    /** Draws the scene offscreen at fixed t with 1, 4 and 16 times the robots and times each. */
    fun benchmark(scene: SceneInput, t: Double, w: Int, h: Int): Map<String, Any?> {
        val stages = ArrayList<Map<String, Any?>>()
        val s = scene.copy(rect = Rect(0.0, 0.0, 1.0, 1.0), imageAspect = null, previousRect = null, rectFromFrame = null)
        val tile = Tile.of(s.rect, w.toDouble() / h, w, h)
        val fs = FrameState.compute(s, t, w.toDouble() / h, Store.systemNight)
        for (n in listOf(Crowd.ROBOTS, Crowd.ROBOTS * 4, MAX_ROBOTS)) {
            repeat(3) { render(fs, tile, n) }
            GLES30.glFinish()
            val frames = 30
            val t0 = SystemClock.elapsedRealtimeNanos()
            repeat(frames) { render(fs, tile, n) }
            GLES30.glFinish()
            val ms = (SystemClock.elapsedRealtimeNanos() - t0) / 1e6 / frames
            stages.add(mapOf("robots" to n, "ms_per_frame" to Math.round(ms * 100) / 100.0, "fps" to Math.round(10000.0 / ms) / 10.0))
            Log.i(GlThread.TAG, String.format("benchmark robots=%d ms_per_frame=%.2f", n, ms))
        }
        checkGl("benchmark")
        return mapOf("w" to w, "h" to h, "samples" to samples, "renderer" to Store.renderer, "stages" to stages, "at_unix_ms" to System.currentTimeMillis())
    }

    private fun render(fs: FrameState, tile: Tile, robotCount: Int) {
        val lim = minOf(maxRenderbuffer, minOf(maxViewport[0], maxViewport[1]))
        if (tile.fullW > lim || tile.fullH > lim) {
            throw IllegalStateException("the whole image (${tile.fullW}x${tile.fullH}) is larger than this GPU's framebuffers ($lim)")
        }
        ensureTargets(tile.fullW, tile.fullH)
        robots.instances = robotCount.coerceIn(0, MAX_ROBOTS)
        val carrierData = FloatArray(fs.signs.size * 8)
        fs.signs.forEachIndexed { i, s -> s.carrier.copyInto(carrierData, i * 8) }
        carriers.instances = fs.signs.size
        if (fs.signs.isNotEmpty()) upload(carriers.instanceVbo, carrierData)
        orbs.instances = fs.lightCount

        // dithering (on by default) would make a blended pixel depend on more than its position
        GLES30.glDisable(GLES30.GL_DITHER)
        // the sun's shadow map: the same on every phone (it depends on the whole image's camera only)
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, shadowFbo)
        GLES30.glViewport(0, 0, FrameState.SHADOW_SIZE, FrameState.SHADOW_SIZE)
        GLES30.glDisable(GLES30.GL_BLEND)
        GLES30.glDisable(GLES30.GL_CULL_FACE)
        GLES30.glEnable(GLES30.GL_DEPTH_TEST)
        GLES30.glDepthFunc(GLES30.GL_LEQUAL)
        GLES30.glDepthMask(true)
        GLES30.glClear(GLES30.GL_DEPTH_BUFFER_BIT)
        GLES30.glEnable(GLES30.GL_POLYGON_OFFSET_FILL)
        GLES30.glPolygonOffset(1.5f, 3f)
        use("robot_shadow", fs, tile).let { draw(robots); draw(carriers) }
        use("prop_shadow", fs, tile).let { draw(buddies); draw(roundTrees); draw(pineTrees) }
        val ps = use("sign_shadow", fs, tile)
        for (s in fs.signs) { GLES30.glUniform3f(ps.u("u_signAt"), s.x, s.z, s.carrier[7] * 10f); draw(sign) }
        GLES30.glDisable(GLES30.GL_POLYGON_OFFSET_FILL)

        // the whole image, drawn only where this tile is
        val r = region(tile)
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, msaaFbo)
        GLES30.glViewport(0, 0, tile.fullW, tile.fullH)
        GLES30.glClearColor(fs.fogColor[0], fs.fogColor[1], fs.fogColor[2], 1f)
        GLES30.glClear(GLES30.GL_COLOR_BUFFER_BIT or GLES30.GL_DEPTH_BUFFER_BIT)
        GLES30.glEnable(GLES30.GL_SCISSOR_TEST)
        GLES30.glScissor(r[0], r[1], r[2] - r[0], r[3] - r[1])
        GLES30.glActiveTexture(GLES30.GL_TEXTURE1)
        GLES30.glBindTexture(GLES30.GL_TEXTURE_2D, shadowTex)
        GLES30.glActiveTexture(GLES30.GL_TEXTURE0)

        GLES30.glDisable(GLES30.GL_DEPTH_TEST)
        GLES30.glDepthMask(false)
        use("sky", fs, tile)
        GLES30.glBindVertexArray(emptyVao)
        GLES30.glDrawArrays(GLES30.GL_TRIANGLES, 0, 3)
        GLES30.glEnable(GLES30.GL_DEPTH_TEST)
        GLES30.glDepthMask(true)

        use("ground", fs, tile)
        draw(ground)
        use("prop", fs, tile)
        draw(hills); draw(clouds); draw(roundTrees); draw(pineTrees); draw(flowers); draw(buddies); draw(orbs)
        use("robot", fs, tile)
        draw(robots); draw(carriers)
        if (fs.signs.isNotEmpty()) {
            val p = use("sign", fs, tile)
            for (s in fs.signs) {
                val col = s.carrier[4].toInt()
                GLES30.glBindTexture(GLES30.GL_TEXTURE_2D, textTexture(s.text, col))
                GLES30.glUniform3f(p.u("u_signAt"), s.x, s.z, s.carrier[7] * 10f)
                GLES30.glUniform3f(p.u("u_signCol"), palette[col * 3], palette[col * 3 + 1], palette[col * 3 + 2])
                draw(sign)
            }
        }
        // Particles, blended (premultiplied alpha), over the whole image: under the scissor the
        // T4 blended a few of their pixels 1 level differently from the unscissored whole image.
        GLES30.glDisable(GLES30.GL_SCISSOR_TEST)
        GLES30.glEnable(GLES30.GL_BLEND)
        GLES30.glBlendFunc(GLES30.GL_ONE, GLES30.GL_ONE_MINUS_SRC_ALPHA)
        GLES30.glDepthMask(false)
        use("particle", fs, tile)
        draw(particles)
        GLES30.glDepthMask(true)
        GLES30.glDisable(GLES30.GL_BLEND)
        GLES30.glBindVertexArray(0)
    }

    private fun use(name: String, fs: FrameState, tile: Tile): Program {
        val p = progs.getValue(name)
        GLES30.glUseProgram(p.id)
        GLES30.glUniformMatrix4fv(p.u("u_viewProj"), 1, false, fs.viewProj, 0)
        GLES30.glUniformMatrix4fv(p.u("u_invViewProj"), 1, false, fs.invViewProj, 0)
        GLES30.glUniformMatrix4fv(p.u("u_lightViewProj"), 1, false, fs.lightViewProj, 0)
        GLES30.glUniform3fv(p.u("u_eye"), 1, fs.eye, 0)
        GLES30.glUniform3fv(p.u("u_camRight"), 1, fs.camRight, 0)
        GLES30.glUniform3fv(p.u("u_camUp"), 1, fs.camUp, 0)
        GLES30.glUniform1f(p.u("u_tw"), fs.tw)
        GLES30.glUniform1f(p.u("u_walkT"), fs.walkT)
        GLES30.glUniform1f(p.u("u_dance"), fs.dance)
        GLES30.glUniform1f(p.u("u_wave"), fs.wave)
        GLES30.glUniform1f(p.u("u_still"), fs.still)
        GLES30.glUniform1f(p.u("u_beat"), fs.beat)
        GLES30.glUniform1f(p.u("u_danceAge"), fs.danceAge)
        GLES30.glUniform1f(p.u("u_night"), fs.night)
        GLES30.glUniform2fv(p.u("u_spread"), 1, fs.spread, 0)
        GLES30.glUniform3fv(p.u("u_palette"), palette.size / 3, palette, 0)
        GLES30.glUniform3fv(p.u("u_sunDir"), 1, fs.sunDir, 0)
        GLES30.glUniform3fv(p.u("u_sunCol"), 1, fs.sunColor, 0)
        GLES30.glUniform3fv(p.u("u_skyCol"), 1, fs.skyColor, 0)
        GLES30.glUniform3fv(p.u("u_groundCol"), 1, fs.groundColor, 0)
        GLES30.glUniform3fv(p.u("u_fogCol"), 1, fs.fogColor, 0)
        GLES30.glUniform3fv(p.u("u_horizonCol"), 1, fs.horizonColor, 0)
        GLES30.glUniform3fv(p.u("u_zenithCol"), 1, fs.zenithColor, 0)
        GLES30.glUniform2f(p.u("u_vpSize"), tile.fullW.toFloat(), tile.fullH.toFloat())
        GLES30.glUniform1i(p.u("u_lightCount"), fs.lightCount)
        GLES30.glUniform3fv(p.u("u_lightPos"), FrameState.MAX_LIGHTS, fs.lightPos, 0)
        GLES30.glUniform3fv(p.u("u_lightCol"), FrameState.MAX_LIGHTS, fs.lightColor, 0)
        GLES30.glUniform1f(p.u("u_shadowTexel"), 1f / FrameState.SHADOW_SIZE)
        return p
    }

    private fun draw(b: Batch) {
        if (b.instances <= 0) return
        GLES30.glBindVertexArray(b.vao)
        if (b.strip) GLES30.glDrawArraysInstanced(GLES30.GL_TRIANGLE_STRIP, 0, b.vertexCount, b.instances)
        else if (b.instanceVbo == 0) GLES30.glDrawElements(GLES30.GL_TRIANGLES, b.indexCount, GLES30.GL_UNSIGNED_SHORT, 0)
        else GLES30.glDrawElementsInstanced(GLES30.GL_TRIANGLES, b.indexCount, GLES30.GL_UNSIGNED_SHORT, 0, b.instances)
    }

    /** The multisampled whole image and the single-sampled one it resolves into. */
    private fun ensureTargets(w: Int, h: Int) {
        if (w == msaaW && h == msaaH && msaaFbo != 0) return
        // forget the old size first: if an allocation below fails, the next frame rebuilds
        msaaW = 0; msaaH = 0
        if (msaaColor != 0) GLES30.glDeleteRenderbuffers(3, intArrayOf(msaaColor, msaaDepth, resolveColor), 0)
        if (msaaFbo == 0) { msaaFbo = genFbo(); resolveFbo = genFbo() }
        msaaColor = genRenderbuffer(); msaaDepth = genRenderbuffer(); resolveColor = genRenderbuffer()
        GLES30.glBindRenderbuffer(GLES30.GL_RENDERBUFFER, resolveColor)
        GLES30.glRenderbufferStorage(GLES30.GL_RENDERBUFFER, GLES30.GL_RGBA8, w, h)
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, resolveFbo)
        GLES30.glFramebufferRenderbuffer(GLES30.GL_FRAMEBUFFER, GLES30.GL_COLOR_ATTACHMENT0, GLES30.GL_RENDERBUFFER, resolveColor)
        checkFbo("resolve")
        GLES30.glBindRenderbuffer(GLES30.GL_RENDERBUFFER, msaaColor)
        GLES30.glRenderbufferStorageMultisample(GLES30.GL_RENDERBUFFER, samples, GLES30.GL_RGBA8, w, h)
        GLES30.glBindRenderbuffer(GLES30.GL_RENDERBUFFER, msaaDepth)
        GLES30.glRenderbufferStorageMultisample(GLES30.GL_RENDERBUFFER, samples, GLES30.GL_DEPTH_COMPONENT24, w, h)
        GLES30.glBindFramebuffer(GLES30.GL_FRAMEBUFFER, msaaFbo)
        GLES30.glFramebufferRenderbuffer(GLES30.GL_FRAMEBUFFER, GLES30.GL_COLOR_ATTACHMENT0, GLES30.GL_RENDERBUFFER, msaaColor)
        GLES30.glFramebufferRenderbuffer(GLES30.GL_FRAMEBUFFER, GLES30.GL_DEPTH_ATTACHMENT, GLES30.GL_RENDERBUFFER, msaaDepth)
        checkFbo("multisampled")
        msaaW = w; msaaH = h
    }

    /** The frame number in 32 black or white cells along the bottom 8 rows, for the phone streamer. */
    private fun drawStamp(w: Int, frame: Long) {
        GLES30.glEnable(GLES30.GL_SCISSOR_TEST)
        for (i in 0 until 32) {
            val on = (frame shr (31 - i)) and 1L == 1L
            val x0 = i * w / 32; val x1 = (i + 1) * w / 32
            GLES30.glScissor(x0, 0, x1 - x0, STAMP_ROWS)
            val c = if (on) 1f else 0f
            GLES30.glClearColor(c, c, c, 1f)
            GLES30.glClear(GLES30.GL_COLOR_BUFFER_BIT)
        }
        GLES30.glDisable(GLES30.GL_SCISSOR_TEST)
    }

    /** The word on a white board with a border in the carrier's colour, drawn by Android's text renderer. */
    private fun textTexture(word: String, colour: Int): Int {
        val key = "$colour:$word"
        text[key]?.let { return it }
        while (text.size >= 8) {
            val (k, id) = text.entries.first()
            GLES30.glDeleteTextures(1, intArrayOf(id), 0)
            text.remove(k)
        }
        val w = 960; val h = 288
        val bmp = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
        val c = Canvas(bmp)
        val rgb = Crowd.PALETTE_SRGB[colour]
        val p = Paint(Paint.ANTI_ALIAS_FLAG)
        c.drawColor(0xFF000000.toInt() or rgb)
        p.color = 0xFFFFFBFE.toInt()
        c.drawRoundRect(RectF(22f, 22f, w - 22f, h - 22f), 40f, 40f, p)
        p.color = 0xFF1D1B20.toInt()
        p.typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        p.textAlign = Paint.Align.CENTER
        var size = 150f
        p.textSize = size
        while (p.measureText(word) > w - 110 && size > 20f) { size -= 4f; p.textSize = size }
        val fm = p.fontMetrics
        c.drawText(word, w / 2f, h / 2f - (fm.ascent + fm.descent) / 2, p)
        val id = genTexture()
        GLES30.glBindTexture(GLES30.GL_TEXTURE_2D, id)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_MIN_FILTER, GLES30.GL_LINEAR)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_MAG_FILTER, GLES30.GL_LINEAR)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_WRAP_S, GLES30.GL_CLAMP_TO_EDGE)
        GLES30.glTexParameteri(GLES30.GL_TEXTURE_2D, GLES30.GL_TEXTURE_WRAP_T, GLES30.GL_CLAMP_TO_EDGE)
        GLUtils.texImage2D(GLES30.GL_TEXTURE_2D, 0, bmp, 0)
        bmp.recycle()
        text[key] = id
        return id
    }

    // ---- GL object helpers

    private fun batch(mesh: Mesh, instances: FloatArray?, vec4s: Int, dynamic: Boolean = false): Batch {
        val vao = genVao()
        GLES30.glBindVertexArray(vao)
        val vbo = genBuffer()
        GLES30.glBindBuffer(GLES30.GL_ARRAY_BUFFER, vbo)
        GLES30.glBufferData(GLES30.GL_ARRAY_BUFFER, mesh.vertices.size * 4, floats(mesh.vertices), GLES30.GL_STATIC_DRAW)
        val stride = Mesh.STRIDE * 4
        GLES30.glEnableVertexAttribArray(0); GLES30.glVertexAttribPointer(0, 3, GLES30.GL_FLOAT, false, stride, 0)
        GLES30.glEnableVertexAttribArray(1); GLES30.glVertexAttribPointer(1, 3, GLES30.GL_FLOAT, false, stride, 12)
        GLES30.glEnableVertexAttribArray(2); GLES30.glVertexAttribPointer(2, 2, GLES30.GL_FLOAT, false, stride, 24)
        val ibo = genBuffer()
        GLES30.glBindBuffer(GLES30.GL_ELEMENT_ARRAY_BUFFER, ibo)
        GLES30.glBufferData(GLES30.GL_ELEMENT_ARRAY_BUFFER, mesh.indices.size * 2, shorts(mesh.indices), GLES30.GL_STATIC_DRAW)
        var ivbo = 0
        var count = 1
        if (instances != null) {
            ivbo = genBuffer()
            GLES30.glBindBuffer(GLES30.GL_ARRAY_BUFFER, ivbo)
            GLES30.glBufferData(GLES30.GL_ARRAY_BUFFER, instances.size * 4, floats(instances), if (dynamic) GLES30.GL_DYNAMIC_DRAW else GLES30.GL_STATIC_DRAW)
            for (k in 0 until vec4s) {
                GLES30.glEnableVertexAttribArray(3 + k)
                GLES30.glVertexAttribPointer(3 + k, 4, GLES30.GL_FLOAT, false, vec4s * 16, k * 16)
                GLES30.glVertexAttribDivisor(3 + k, 1)
            }
            count = instances.size / (vec4s * 4)
        }
        GLES30.glBindVertexArray(0)
        return Batch(vao, mesh.indices.size, count, ivbo)
    }

    private fun quadBatch(instances: FloatArray): Batch {
        val vao = genVao()
        GLES30.glBindVertexArray(vao)
        val vbo = genBuffer()
        GLES30.glBindBuffer(GLES30.GL_ARRAY_BUFFER, vbo)
        val quad = floatArrayOf(-1f, -1f, 1f, -1f, -1f, 1f, 1f, 1f)
        GLES30.glBufferData(GLES30.GL_ARRAY_BUFFER, quad.size * 4, floats(quad), GLES30.GL_STATIC_DRAW)
        GLES30.glEnableVertexAttribArray(0); GLES30.glVertexAttribPointer(0, 2, GLES30.GL_FLOAT, false, 8, 0)
        val ivbo = genBuffer()
        GLES30.glBindBuffer(GLES30.GL_ARRAY_BUFFER, ivbo)
        GLES30.glBufferData(GLES30.GL_ARRAY_BUFFER, instances.size * 4, floats(instances), GLES30.GL_STATIC_DRAW)
        for (k in 0 until 3) {
            GLES30.glEnableVertexAttribArray(1 + k)
            GLES30.glVertexAttribPointer(1 + k, 4, GLES30.GL_FLOAT, false, 48, k * 16)
            GLES30.glVertexAttribDivisor(1 + k, 1)
        }
        GLES30.glBindVertexArray(0)
        return Batch(vao, 0, instances.size / 12, ivbo, strip = true, vertexCount = 4)
    }

    private fun upload(vbo: Int, data: FloatArray) {
        GLES30.glBindBuffer(GLES30.GL_ARRAY_BUFFER, vbo)
        GLES30.glBufferSubData(GLES30.GL_ARRAY_BUFFER, 0, data.size * 4, floats(data))
    }

    private fun link(name: String, vs: String, fs: String): Program {
        val v = compile(GLES30.GL_VERTEX_SHADER, vs, "$name.vert")
        val f = compile(GLES30.GL_FRAGMENT_SHADER, fs, "$name.frag")
        val p = GLES30.glCreateProgram()
        GLES30.glAttachShader(p, v); GLES30.glAttachShader(p, f)
        GLES30.glLinkProgram(p)
        val ok = IntArray(1)
        GLES30.glGetProgramiv(p, GLES30.GL_LINK_STATUS, ok, 0)
        GLES30.glDeleteShader(v); GLES30.glDeleteShader(f)
        if (ok[0] == 0) throw IllegalStateException("link $name: ${GLES30.glGetProgramInfoLog(p)}")
        return Program(p)
    }

    private fun compile(type: Int, src: String, name: String): Int {
        val s = GLES30.glCreateShader(type)
        GLES30.glShaderSource(s, src)
        GLES30.glCompileShader(s)
        val ok = IntArray(1)
        GLES30.glGetShaderiv(s, GLES30.GL_COMPILE_STATUS, ok, 0)
        if (ok[0] == 0) throw IllegalStateException("compile $name: ${GLES30.glGetShaderInfoLog(s)}")
        return s
    }

    private fun genBuffer() = IntArray(1).also { GLES30.glGenBuffers(1, it, 0); buffers.add(it[0]) }[0]
    private fun genVao() = IntArray(1).also { GLES30.glGenVertexArrays(1, it, 0); vaos.add(it[0]) }[0]
    private fun genTexture() = IntArray(1).also { GLES30.glGenTextures(1, it, 0) }[0]
    private fun genFbo() = IntArray(1).also { GLES30.glGenFramebuffers(1, it, 0) }[0]
    private fun genRenderbuffer() = IntArray(1).also { GLES30.glGenRenderbuffers(1, it, 0) }[0]

    private fun checkFbo(what: String) {
        val st = GLES30.glCheckFramebufferStatus(GLES30.GL_FRAMEBUFFER)
        if (st != GLES30.GL_FRAMEBUFFER_COMPLETE) throw IllegalStateException("$what framebuffer incomplete: 0x${Integer.toHexString(st)}")
    }

    private fun checkGl(what: String) {
        val e = GLES30.glGetError()
        if (e != GLES30.GL_NO_ERROR) Log.w(GlThread.TAG, "$what: GL error 0x${Integer.toHexString(e)}")
    }

    companion object {
        const val MAX_ROBOTS = Crowd.ROBOTS * 16
        const val STAMP_ROWS = 8

        fun floats(a: FloatArray): FloatBuffer =
            ByteBuffer.allocateDirect(a.size * 4).order(ByteOrder.nativeOrder()).asFloatBuffer().apply { put(a); position(0) }

        fun shorts(a: ShortArray): ShortBuffer =
            ByteBuffer.allocateDirect(a.size * 2).order(ByteOrder.nativeOrder()).asShortBuffer().apply { put(a); position(0) }
    }
}
