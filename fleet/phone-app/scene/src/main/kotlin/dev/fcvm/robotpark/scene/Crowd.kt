package dev.fcvm.robotpark.scene

/**
 * The park's population: a fixed table per kind, each entry a function of its index only, so
 * the first n robots are the same robots whatever the count, and every phone builds the same
 * tables. The motion of each entry is computed on the GPU from these numbers and the frame's
 * time uniforms (assets/shaders/common.glsl), so nothing in the scene carries state.
 *
 * Every motion repeats with [PERIOD]: a walker covers a whole number of [LOOP_X] laps in it, a
 * step or hop cycle a whole number of times, so the GPU works on t mod PERIOD (small floats)
 * and the wrap is seamless.
 */
object Crowd {
    const val PERIOD = 1200.0
    const val LOOP_X = 260.0
    const val CLOUD_LOOP = 900.0

    const val KIND_WALKER = 0
    const val KIND_HOPPER = 1
    const val KIND_LEAPER = 2
    const val KIND_CARRIER = 4

    const val PROP_STATIC = 0
    const val PROP_CLOUD = 1
    const val PROP_BUDDY = 2
    const val PROP_TREE = 3
    const val PROP_ORB = 4

    const val PARTICLE_CONFETTI = 0
    const val PARTICLE_AMBIENT = 1
    const val PARTICLE_SPARKLE = 2
    const val PARTICLE_HALO = 3
    const val SPARKLES_PER_LEAPER = 10

    const val ROBOTS = 2400
    const val BUDDIES = 360

    /** sRGB colours: Material palette tones, index 0 the robot's green. */
    val PALETTE_SRGB = intArrayOf(
        0x3DDC84, 0x2196F3, 0xFFCA28, 0xF06292, 0x9575CD, 0xFF8A65, 0x4DB6AC, 0x4FC3F7,
        0xFAFAFA, 0x263238, 0x43A047, 0x2E7D32, 0x9CCC65, 0x689F38, 0x8D6E63, 0xC0CA33,
    )

    fun paletteLinear(): FloatArray = FloatArray(PALETTE_SRGB.size * 3) { i ->
        val c = PALETTE_SRGB[i / 3] shr (16 - 8 * (i % 3)) and 0xff
        StrictMath.pow(c / 255.0, 2.2).toFloat()
    }

    /** A 32-bit integer hash (lowbias32), as an unsigned value in [0, 1). */
    fun u(i: Int, k: Int): Double {
        var x = i * 0x9E3779B1.toInt() + k * 0x85EBCA77.toInt()
        x = x xor (x ushr 16); x *= 0x7feb352d
        x = x xor (x ushr 15); x *= 0x846ca68b.toInt()
        x = x xor (x ushr 16)
        return (x.toLong() and 0xffffffffL) / 4294967296.0
    }

    private fun pick(v: Double, vararg options: Int) = options[minOf(options.size - 1, (v * options.size).toInt())]

    /** Robot i: (x0, z, laps, steps-or-cycles) and (colour, scale, kind, seed). */
    fun robot(i: Int): FloatArray {
        val kindRoll = u(i, 1)
        val kind = when {
            kindRoll < 0.08 -> KIND_LEAPER
            kindRoll < 0.20 -> KIND_HOPPER
            else -> KIND_WALKER
        }
        val dir = if (u(i, 2) < 0.5) -1 else 1
        val z = -46.0 + 62.0 * StrictMath.pow(u(i, 3), 1.3)
        val x0 = (u(i, 4) - 0.5) * LOOP_X
        val kid = u(i, 5) < 0.12
        val scale = if (kid) 0.62 + 0.1 * u(i, 6) else 0.85 + 0.4 * u(i, 6)
        val colour = if (u(i, 7) < 0.55) 0 else 1 + (u(i, 8) * 7).toInt().coerceAtMost(6)
        val laps: Int
        val cycles: Int
        when (kind) {
            KIND_LEAPER -> {
                laps = dir * (9 + (u(i, 9) * 3).toInt())
                // one leap about every 6 s, a whole number of them per PERIOD
                cycles = 180 + (u(i, 10) * 40).toInt()
            }
            KIND_HOPPER -> {
                laps = dir * pick(u(i, 9), 2, 3)
                cycles = 1300 + (u(i, 10) * 500).toInt() // hops
            }
            else -> {
                laps = dir * pick(u(i, 9), 2, 3, 3)
                val speed = StrictMath.abs(laps) * LOOP_X / PERIOD
                cycles = StrictMath.round(PERIOD * 2.3 * speed / scale).toInt() // steps
            }
        }
        return floatArrayOf(x0.toFloat(), z.toFloat(), laps.toFloat(), cycles.toFloat(), colour.toFloat(), scale.toFloat(), kind.toFloat(), u(i, 11).toFloat())
    }

    fun robots(n: Int): FloatArray = FloatArray(n * 8).also { a -> for (i in 0 until n) robot(i).copyInto(a, i * 8) }

    private fun prop(a: FloatArray, at: Int, x: Double, y: Double, z: Double, yaw: Double, sx: Double, sy: Double, sz: Double, motion: Double, colA: Int, colB: Int, kind: Int, seed: Double) {
        val v = floatArrayOf(x.toFloat(), y.toFloat(), z.toFloat(), yaw.toFloat(), sx.toFloat(), sy.toFloat(), sz.toFloat(), motion.toFloat(), colA.toFloat(), colB.toFloat(), kind.toFloat(), seed.toFloat())
        v.copyInto(a, at * 12)
    }

    /** Round friends that bounce along: 12 floats each, the prop layout. */
    fun buddies(n: Int): FloatArray = FloatArray(n * 12).also { a ->
        for (i in 0 until n) {
            val k = 1_000_000 + i
            val dir = if (u(k, 1) < 0.5) -1 else 1
            val s = 0.55 + 0.3 * u(k, 2)
            prop(a, i, (u(k, 3) - 0.5) * LOOP_X, 0.0, -42.0 + 58.0 * u(k, 4), 0.0, s, s, s, (dir * (2 + (u(k, 5) * 2).toInt())).toDouble(), 1 + (u(k, 6) * 7).toInt().coerceAtMost(6), 8, PROP_BUDDY, u(k, 7))
        }
    }

    const val TREES = 420
    fun trees(): FloatArray = FloatArray(TREES * 12).also { a ->
        for (i in 0 until TREES) {
            val k = 2_000_000 + i
            val back = u(k, 1) < 0.86
            val z = if (back) -58.0 - 70.0 * u(k, 2) else -40.0 + 50.0 * u(k, 2)
            val x = (u(k, 3) - 0.5) * (if (back) 420.0 else LOOP_X)
            val h = (if (back) 1.6 else 1.1) * (0.8 + 0.6 * u(k, 4))
            val pine = u(k, 5) < 0.35
            val foliage = pick(u(k, 6), 10, 11, 6, 13, 15)
            prop(a, i, x, 0.0, z, u(k, 7) * 6.283, h, h, h, if (pine) 1.0 else 0.0, foliage, 14, PROP_TREE, u(k, 8))
        }
    }

    const val FLOWERS = 4000
    fun flowers(): FloatArray = FloatArray(FLOWERS * 12).also { a ->
        for (i in 0 until FLOWERS) {
            val k = 3_000_000 + i
            val s = 0.7 + 0.6 * u(k, 1)
            prop(a, i, (u(k, 2) - 0.5) * LOOP_X, 0.0, -50.0 + 76.0 * u(k, 3), u(k, 4) * 6.283, s, s, s, 0.0, pick(u(k, 5), 2, 3, 8, 4, 5, 7), 10, PROP_STATIC, u(k, 6))
        }
    }

    const val HILLS = 22
    fun hills(): FloatArray = FloatArray(HILLS * 12).also { a ->
        for (i in 0 until HILLS) {
            val k = 4_000_000 + i
            val x = -520.0 + 1040.0 * (i + u(k, 1) * 0.6) / HILLS
            val z = -190.0 - 150.0 * u(k, 2)
            val r = 60.0 + 70.0 * u(k, 3)
            prop(a, i, x, -r * 0.55, z, 0.0, r * 1.4, r * (0.55 + 0.3 * u(k, 4)), r, 0.0, pick(u(k, 5), 12, 13, 10), 13, PROP_STATIC, u(k, 6))
        }
    }

    const val CLOUDS = 30
    const val PUFFS = 5
    fun clouds(): FloatArray = FloatArray(CLOUDS * PUFFS * 12).also { a ->
        for (c in 0 until CLOUDS) {
            val k = 5_000_000 + c
            val x = (u(k, 1) - 0.5) * CLOUD_LOOP
            val y = 38.0 + 30.0 * u(k, 2)
            val z = -170.0 - 140.0 * u(k, 3)
            val size = 7.0 + 7.0 * u(k, 4)
            val laps = (1 + (u(k, 5) * 2).toInt()).toDouble()
            for (p in 0 until PUFFS) {
                val q = k * 8 + p
                val r = size * (0.55 + 0.45 * u(q, 6)) * (if (p == 2) 1.3 else 1.0)
                prop(a, c * PUFFS + p, x + (p - 2) * size * 0.75, y + (u(q, 7) - 0.3) * size * 0.4, z + (u(q, 8) - 0.5) * size * 0.5, 0.0, r * 1.15, r * 0.8, r, laps, 8, 8, PROP_CLOUD, u(q, 9))
            }
        }
    }

    const val CONFETTI = 3000
    const val AMBIENT = 1500

    /** Particles: (kind, index, seed, extra) followed by the robot attributes a sparkle follows (zeros otherwise). */
    fun particles(robots: Int): FloatArray {
        val leapers = (0 until robots).filter { robot(it)[6].toInt() == KIND_LEAPER }
        val n = CONFETTI + AMBIENT + leapers.size * SPARKLES_PER_LEAPER + 16
        val a = FloatArray(n * 12)
        var at = 0
        fun put(kind: Int, index: Int, seed: Double, extra: Double, robotAttrs: FloatArray?) {
            a[at * 12] = kind.toFloat(); a[at * 12 + 1] = index.toFloat(); a[at * 12 + 2] = seed.toFloat(); a[at * 12 + 3] = extra.toFloat()
            robotAttrs?.copyInto(a, at * 12 + 4)
            at++
        }
        for (i in 0 until CONFETTI) put(PARTICLE_CONFETTI, i, u(6_000_000 + i, 1), u(6_000_000 + i, 2), null)
        for (i in 0 until AMBIENT) put(PARTICLE_AMBIENT, i, u(7_000_000 + i, 1), u(7_000_000 + i, 2), null)
        for (r in leapers) for (s in 0 until SPARKLES_PER_LEAPER) put(PARTICLE_SPARKLE, s, u(r * 16 + s, 13), u(r * 16 + s, 14), robot(r))
        for (l in 0 until 16) put(PARTICLE_HALO, l, 0.0, 0.0, null)
        return a
    }
}
