package dev.fcvm.robotpark.scene

/**
 * Everything the GPU needs for one frame, as a pure function of the scene inputs and t.
 * It does not depend on the phone's rect: the rect only moves the viewport ([Tile]), so
 * neighbours draw with identical uniforms. Doubles throughout, StrictMath for the
 * transcendental functions, rounded to floats at the end, so equal inputs give equal bits on
 * every phone (and on the JVM the tests run on).
 */
class FrameState(
    val t: Double,
    val viewProj: FloatArray,
    val invViewProj: FloatArray,
    val lightViewProj: FloatArray,
    val eye: FloatArray,
    val camRight: FloatArray,
    val camUp: FloatArray,
    /** t mod PERIOD and the walking clock mod PERIOD (it stands still while robots dance or wave). */
    val tw: Float,
    val walkT: Float,
    val dance: Float,
    val wave: Float,
    /** 0..1: how far the scene's moves have stopped the crowd (the phone's own moves do not). */
    val still: Float,
    val beat: Float,
    val danceAge: Float,
    val night: Float,
    val sunDir: FloatArray,
    val sunColor: FloatArray,
    val skyColor: FloatArray,
    val groundColor: FloatArray,
    val fogColor: FloatArray,
    val horizonColor: FloatArray,
    val zenithColor: FloatArray,
    /** x range of the visible lawn (whole image) at the crowd's depth, for confetti and signs. */
    val spread: FloatArray,
    val lightPos: FloatArray,
    val lightColor: FloatArray,
    val lightCount: Int,
    val signs: List<Sign>,
    val chrome: Boolean,
    val frameIndex: Long,
) {
    /** A word on a sign, carried across the whole image by a robot: (x, z) and its robot's attributes. */
    data class Sign(val text: String, val x: Float, val z: Float, val carrier: FloatArray)

    /** Every number the frame uses, in order, for comparing frames bit for bit. */
    fun bits(): IntArray {
        val all = ArrayList<Float>()
        for (a in listOf(viewProj, invViewProj, lightViewProj, eye, camRight, camUp, sunDir, sunColor, skyColor, groundColor, fogColor, horizonColor, zenithColor, spread, lightPos, lightColor)) all.addAll(a.toList())
        all.addAll(listOf(tw, walkT, dance, wave, still, beat, danceAge, night, lightCount.toFloat(), if (chrome) 1f else 0f, frameIndex.toFloat()))
        for (s in signs) { all.add(s.x); all.add(s.z); all.addAll(s.carrier.toList()); all.add(s.text.hashCode().toFloat()) }
        return IntArray(all.size) { all[it].toRawBits() }
    }

    companion object {
        const val FPS = 60
        const val MOVE_SECS = 8.0
        const val MOVE_RAMP = 0.6
        /** A dance's confetti falls until this long after it starts (a piece starts up to 2.5 s late and lasts 8 s; particle.vert). */
        const val CONFETTI_SECS = 10.5
        const val SIGN_SECS = 45.0
        const val MAX_SIGNS = 3
        const val MAX_LIGHTS = 16
        const val NEAR = 0.5
        const val FAR = 900.0
        const val SHADOW_FAR = 75.0
        const val SHADOW_SIZE = 2048

        /** Scene time of the frame due at `unixMs`: the 1/60 s grid every phone draws on. */
        fun frameIndexAt(unixMs: Long, epochUnixMs: Long): Long = Math.floorDiv((unixMs - epochUnixMs) * FPS, 1000L)

        fun compute(scene: SceneInput, t: Double, surfaceAspect: Double, nightDefault: Boolean): FrameState {
            require(t >= scene.historyFrom) {
                "t=$t is before t=${scene.historyFrom}, where this phone's own older moves were pruned; it cannot be drawn exactly"
            }
            val aspect = scene.imageAspect ?: surfaceAspect
            val cam = CameraPath.at(scene.camera, t)
            val view = Mat.lookAt(cam.pos, cam.look, Vec3(0.0, 1.0, 0.0))
            val proj = Mat.perspective(cam.fovDeg, aspect, NEAR, FAR)
            val vp = Mat.mul(proj, view)
            val fwd = (cam.look - cam.pos).normalized()
            val right = fwd.cross(Vec3(0.0, 1.0, 0.0)).normalized()
            val up = right.cross(fwd)

            val night = scene.night ?: nightDefault
            val sun = if (night) Vec3(0.45, 0.62, -0.35).normalized() else Vec3(-0.52, 0.72, 0.42).normalized()

            // Moves: each lasts MOVE_SECS with smooth ramps. The scene's moves stop the walk (every
            // phone shows them together); the phone's own set the pose only, so dropping old ones
            // never moves the crowd.
            val all = merge(scene.moves.map { it.t })
            val posed = scene.moves + scene.localMoves
            val dances = merge(posed.filter { it.kind == MoveKind.DANCE }.map { it.t })
            val waves = merge(posed.filter { it.kind == MoveKind.WAVE }.map { it.t })
            val walk = t - frozen(all, t)
            val activeDance = dances.lastOrNull { it.first <= t && t < it.second }?.first
            val danceStart = activeDance ?: dances.lastOrNull { it.first <= t }?.first
            // past its confetti, a dance leaves nothing in the frame: the same age as no dance at all
            val danceAge = if (danceStart == null || t - danceStart >= CONFETTI_SECS) 1.0e4 else t - danceStart

            // Lights: the newest MAX_LIGHTS that have appeared, fading in over 1.5 s.
            val lights = scene.lights.filter { it.t <= t }.sortedBy { it.t }.takeLast(MAX_LIGHTS)
            val lp = FloatArray(MAX_LIGHTS * 3); val lc = FloatArray(MAX_LIGHTS * 3)
            lights.forEachIndexed { i, l ->
                val f = smooth((t - l.t) / 1.5)
                lp[i * 3] = l.pos.x.toFloat(); lp[i * 3 + 1] = l.pos.y.toFloat(); lp[i * 3 + 2] = l.pos.z.toFloat()
                lc[i * 3] = (l.color.x * f).toFloat(); lc[i * 3 + 1] = (l.color.y * f).toFloat(); lc[i * 3 + 2] = (l.color.z * f).toFloat()
            }

            // Signs walk across the whole image's visible width at their lane, left to right.
            // A word keeps one lane for its whole walk: its place among all words, mod MAX_SIGNS.
            val signs = scene.words.sortedBy { it.t }.withIndex()
                .filter { (_, w) -> w.t <= t && t - w.t < SIGN_SECS && w.text.isNotEmpty() }
                .takeLast(MAX_SIGNS).map { (index, w) ->
                    val z = 9.0 - 3.0 * (index % MAX_SIGNS)
                    val (xl, xr) = spanAt(vp, cam.pos, z)
                    val p = (t - w.t) / SIGN_SECS
                    val x = xl - 4 + (xr - xl + 8) * p
                    val stepPhase = Time.wrap((t - w.t) * 2.0, 1.0) * 2 * StrictMath.PI
                    val k = (w.t * 1000).toLong().toInt() xor w.text.hashCode()
                    val colour = 1 + (Crowd.u(k, 1) * 7).toInt().coerceAtMost(6)
                    Sign(w.text, x.toFloat(), z.toFloat(), floatArrayOf(x.toFloat(), z.toFloat(), stepPhase.toFloat(), 0f, colour.toFloat(), 1.15f, Crowd.KIND_CARRIER.toFloat(), Crowd.u(k, 2).toFloat()))
                }
            val (sl, sr) = spanAt(vp, cam.pos, -6.0)

            val shadow = shadowMatrix(Mat.invert(vp), sun, cam.pos)
            fun v(a: Vec3) = floatArrayOf(a.x.toFloat(), a.y.toFloat(), a.z.toFloat())
            fun srgb(c: Int, k: Double = 1.0) = FloatArray(3) { i -> (StrictMath.pow(((c shr (16 - 8 * i)) and 0xff) / 255.0, 2.2) * k).toFloat() }
            return FrameState(
                t = t,
                viewProj = Mat.toFloat(vp),
                invViewProj = Mat.toFloat(Mat.invert(vp)),
                lightViewProj = Mat.toFloat(shadow),
                eye = v(cam.pos),
                camRight = v(right),
                camUp = v(up),
                tw = Time.wrap(t, Crowd.PERIOD).toFloat(),
                walkT = Time.wrap(walk, Crowd.PERIOD).toFloat(),
                dance = weight(dances, t).toFloat(),
                wave = weight(waves, t).toFloat(),
                still = weight(all, t).toFloat(),
                // 120 bpm from the start of the dance window being danced (0 when none is)
                beat = (if (activeDance == null) 0.0 else Time.wrap((t - activeDance) * 2.0, 64.0)).toFloat(),
                danceAge = danceAge.toFloat(),
                night = if (night) 1f else 0f,
                sunDir = v(sun),
                sunColor = if (night) srgb(0x8FA8FF, 0.32) else srgb(0xFFF1D6, 2.0),
                skyColor = if (night) srgb(0x1A237E, 0.3) else srgb(0x8CCBFF, 0.62),
                groundColor = if (night) srgb(0x1B3A2A, 0.2) else srgb(0x9CCC65, 0.3),
                fogColor = if (night) srgb(0x141C3C) else srgb(0xA9D4F5),
                horizonColor = if (night) srgb(0x26306A) else srgb(0xB8DDFA),
                zenithColor = if (night) srgb(0x070B24) else srgb(0x1F6FD6),
                spread = floatArrayOf(sl.toFloat(), sr.toFloat()),
                lightPos = lp,
                lightColor = lc,
                lightCount = lights.size,
                signs = signs,
                chrome = scene.chromeAt(t),
                frameIndex = StrictMath.round(t * FPS),
            )
        }

        fun smooth(x: Double): Double { val c = x.coerceIn(0.0, 1.0); return c * c * (3 - 2 * c) }

        /**
         * Keeps the phone's own moves (`local`) to `max`: drops the oldest only as whole merged
         * windows (with the scene's moves) that ended before `now`, so no frame from `now` on
         * changes, and returns the end of the last window dropped ([SceneInput.historyFrom]:
         * frames before it cannot be drawn exactly any more). A window still running is kept,
         * so the list can stay longer than `max` until it ends.
         */
        fun pruneMoves(local: List<Move>, scene: List<Move>, now: Double, max: Int): Pair<List<Move>, Double> {
            val sorted = local.sortedBy { it.t }
            if (sorted.size <= max) return sorted to Double.NEGATIVE_INFINITY
            val windows = merge((sorted + scene).map { it.t })
            // a dance's confetti lasts CONFETTI_SECS from the start of its dance window, which can
            // start later than the window of all moves it is merged into
            val dances = merge((sorted + scene).filter { it.kind == MoveKind.DANCE }.map { it.t })
            var from = 0
            var until = Double.NEGATIVE_INFINITY
            while (sorted.size - from > max) {
                val w = windows.first { sorted[from].t >= it.first && sorted[from].t < it.second }
                // a window is over when its moves are, and the confetti of its dances has fallen
                var over = w.second
                for (d in dances) if (d.first >= w.first && d.first < w.second) over = StrictMath.max(over, d.first + CONFETTI_SECS)
                if (over > now) break
                until = StrictMath.max(until, over)
                while (from < sorted.size && sorted[from].t < w.second) from++
            }
            return sorted.subList(from, sorted.size) to until
        }

        /** Disjoint [start, end) windows covering moves that start at `starts`. */
        fun merge(starts: List<Double>): List<Pair<Double, Double>> {
            val out = ArrayList<Pair<Double, Double>>()
            for (s in starts.sorted()) {
                val last = out.lastOrNull()
                if (last != null && s <= last.second) out[out.size - 1] = last.first to StrictMath.max(last.second, s + MOVE_SECS)
                else out.add(s to s + MOVE_SECS)
            }
            return out
        }

        /** A window's weight: smoothstep up over MOVE_RAMP, 1, smoothstep down over MOVE_RAMP. */
        fun weight(windows: List<Pair<Double, Double>>, t: Double): Double {
            val w = windows.lastOrNull { it.first <= t } ?: return 0.0
            if (t >= w.second) return 0.0
            return StrictMath.min(smooth((t - w.first) / MOVE_RAMP), smooth((w.second - t) / MOVE_RAMP))
        }

        /** Seconds of walking skipped before t: the integral of the windows' weight. */
        fun frozen(windows: List<Pair<Double, Double>>, t: Double): Double {
            var sum = 0.0
            // the integral of smoothstep over a ramp of length r is r/2: x^3 - x^4/2 at x in [0, 1]
            fun rampInt(x: Double) = MOVE_RAMP * (x * x * x - x * x * x * x / 2)
            for ((s, e) in windows) {
                if (t <= s) break
                val len = e - s
                val r = MOVE_RAMP
                if (len < 2 * r) {
                    // too short to reach full weight never happens: MOVE_SECS > 2 * MOVE_RAMP
                    throw IllegalStateException("move window shorter than its ramps")
                }
                val u = StrictMath.min(t, e) - s
                sum += when {
                    u <= r -> rampInt(u / r)
                    u <= len - r -> rampInt(1.0) + (u - r)
                    else -> rampInt(1.0) + (len - 2 * r) + (rampInt(1.0) - rampInt((len - u) / r))
                }
            }
            return sum
        }

        /** The visible x range at depth z, where the whole image's left and right edges meet y = 1. */
        fun spanAt(vp: DoubleArray, eye: Vec3, z: Double): Pair<Double, Double> {
            val inv = Mat.invert(vp)
            fun at(ndcX: Double): Double? {
                val a = Mat.transform(inv, Vec3(ndcX, -0.25, -1.0)); val b = Mat.transform(inv, Vec3(ndcX, -0.25, 1.0))
                val p = Vec3(a[0] / a[3], a[1] / a[3], a[2] / a[3]); val q = Vec3(b[0] / b[3], b[1] / b[3], b[2] / b[3])
                val d = q - p
                if (StrictMath.abs(d.z) < 1e-9) return null
                val s = (z - p.z) / d.z
                return if (s > 0) p.x + d.x * s else null
            }
            val l = at(-1.0); val r = at(1.0)
            if (l == null || r == null || r <= l) return eye.x - 30 to eye.x + 30
            return StrictMath.max(l, eye.x - 400) to StrictMath.min(r, eye.x + 400)
        }

        /**
         * The sun's orthographic shadow matrix: it covers the whole image's view out to SHADOW_FAR,
         * on a fixed texel grid so the shadows do not crawl as the camera moves.
         */
        fun shadowMatrix(invVp: DoubleArray, sun: Vec3, eye: Vec3): DoubleArray {
            val corners = ArrayList<Vec3>()
            val farNdc = ndcDepth(SHADOW_FAR)
            for (x in listOf(-1.0, 1.0)) for (y in listOf(-1.0, 1.0)) for (z in listOf(-1.0, farNdc)) {
                val p = Mat.transform(invVp, Vec3(x, y, z))
                corners.add(Vec3(p[0] / p[3], p[1] / p[3], p[2] / p[3]))
            }
            val center = Vec3(eye.x, 0.0, eye.z - 30)
            val view = Mat.lookAt(center + sun * 300.0, center, Vec3(0.0, 1.0, 0.0))
            var minX = Double.MAX_VALUE; var maxX = -Double.MAX_VALUE; var minY = Double.MAX_VALUE; var maxY = -Double.MAX_VALUE
            for (c in corners) {
                // the ground under the frustum, not the sky above it
                val g = Vec3(c.x, StrictMath.max(-1.0, StrictMath.min(c.y, 12.0)), c.z)
                val p = Mat.transform(view, g)
                minX = StrictMath.min(minX, p[0]); maxX = StrictMath.max(maxX, p[0])
                minY = StrictMath.min(minY, p[1]); maxY = StrictMath.max(maxY, p[1])
            }
            val size = StrictMath.ceil(StrictMath.max(maxX - minX, maxY - minY) / 16.0) * 16.0 + 16.0
            val texel = size / SHADOW_SIZE
            val cx = StrictMath.floor((minX + maxX) / 2 / texel) * texel
            val cy = StrictMath.floor((minY + maxY) / 2 / texel) * texel
            return Mat.mul(Mat.ortho(cx - size / 2, cx + size / 2, cy - size / 2, cy + size / 2, 100.0, 520.0), view)
        }

        private fun ndcDepth(d: Double): Double {
            val a = (FAR + NEAR) / (NEAR - FAR); val b = 2 * FAR * NEAR / (NEAR - FAR)
            return (a * -d + b) / d
        }
    }
}
