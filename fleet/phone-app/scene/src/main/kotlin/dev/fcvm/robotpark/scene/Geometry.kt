package dev.fcvm.robotpark.scene

/**
 * Where a phone's surface sits in the whole image:
 *
 *   full_h = round(surface_h / rect.h)    full_w = round(full_h * image_aspect)
 *   x0 = round(rect.x * full_w)           y0 = round(rect.y * full_h)
 *
 * The surface shows whole-image pixels [x0, x0 + w) x [y0, y0 + h) (the renderer's tile
 * geometry, with the surface's own size). A phone whose surface is wider than its rect shows
 * more of the image, which its neighbour draws identically; pixels beyond the image are black.
 */
data class Tile(val fullW: Int, val fullH: Int, val x0: Int, val y0: Int, val w: Int, val h: Int) {
    companion object {
        fun of(rect: Rect, imageAspect: Double, w: Int, h: Int): Tile {
            val fullH = maxOf(1L, StrictMath.round(h / rect.h)).toInt()
            val fullW = maxOf(1L, StrictMath.round(fullH * imageAspect)).toInt()
            return Tile(fullW, fullH, StrictMath.round(rect.x * fullW).toInt(), StrictMath.round(rect.y * fullH).toInt(), w, h)
        }
    }
}

/** Column-major 4x4 matrices, computed in doubles and handed to GL as floats. */
object Mat {
    fun mul(a: DoubleArray, b: DoubleArray): DoubleArray {
        val r = DoubleArray(16)
        for (c in 0 until 4) for (row in 0 until 4) {
            var s = 0.0
            for (k in 0 until 4) s += a[k * 4 + row] * b[c * 4 + k]
            r[c * 4 + row] = s
        }
        return r
    }

    fun lookAt(eye: Vec3, at: Vec3, up: Vec3): DoubleArray {
        val f = (at - eye).normalized()
        var s = f.cross(up)
        if (s.length() < 1e-9) s = f.cross(Vec3(0.0, 0.0, 1.0))
        s = s.normalized()
        val u = s.cross(f)
        return doubleArrayOf(
            s.x, u.x, -f.x, 0.0,
            s.y, u.y, -f.y, 0.0,
            s.z, u.z, -f.z, 0.0,
            -s.dot(eye), -u.dot(eye), f.dot(eye), 1.0,
        )
    }

    fun perspective(fovYDeg: Double, aspect: Double, near: Double, far: Double): DoubleArray {
        val f = 1.0 / StrictMath.tan(StrictMath.toRadians(fovYDeg) / 2)
        return doubleArrayOf(
            f / aspect, 0.0, 0.0, 0.0,
            0.0, f, 0.0, 0.0,
            0.0, 0.0, (far + near) / (near - far), -1.0,
            0.0, 0.0, 2 * far * near / (near - far), 0.0,
        )
    }

    fun ortho(l: Double, r: Double, b: Double, t: Double, n: Double, f: Double): DoubleArray = doubleArrayOf(
        2 / (r - l), 0.0, 0.0, 0.0,
        0.0, 2 / (t - b), 0.0, 0.0,
        0.0, 0.0, -2 / (f - n), 0.0,
        -(r + l) / (r - l), -(t + b) / (t - b), -(f + n) / (f - n), 1.0,
    )

    fun transform(m: DoubleArray, p: Vec3, w: Double = 1.0): DoubleArray = DoubleArray(4) { row ->
        m[row] * p.x + m[4 + row] * p.y + m[8 + row] * p.z + m[12 + row] * w
    }

    /** General 4x4 inverse (cofactors). */
    fun invert(m: DoubleArray): DoubleArray {
        val inv = DoubleArray(16)
        inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15] + m[9] * m[7] * m[14] + m[13] * m[6] * m[11] - m[13] * m[7] * m[10]
        inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15] - m[8] * m[7] * m[14] - m[12] * m[6] * m[11] + m[12] * m[7] * m[10]
        inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15] + m[8] * m[7] * m[13] + m[12] * m[5] * m[11] - m[12] * m[7] * m[9]
        inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14] - m[8] * m[6] * m[13] - m[12] * m[5] * m[10] + m[12] * m[6] * m[9]
        inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15] - m[9] * m[3] * m[14] - m[13] * m[2] * m[11] + m[13] * m[3] * m[10]
        inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15] + m[8] * m[3] * m[14] + m[12] * m[2] * m[11] - m[12] * m[3] * m[10]
        inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15] - m[8] * m[3] * m[13] - m[12] * m[1] * m[11] + m[12] * m[3] * m[9]
        inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14] + m[8] * m[2] * m[13] + m[12] * m[1] * m[10] - m[12] * m[2] * m[9]
        inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15] + m[5] * m[3] * m[14] + m[13] * m[2] * m[7] - m[13] * m[3] * m[6]
        inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15] - m[4] * m[3] * m[14] - m[12] * m[2] * m[7] + m[12] * m[3] * m[6]
        inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15] + m[4] * m[3] * m[13] + m[12] * m[1] * m[7] - m[12] * m[3] * m[5]
        inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14] - m[4] * m[2] * m[13] - m[12] * m[1] * m[6] + m[12] * m[2] * m[5]
        inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11] - m[5] * m[3] * m[10] - m[9] * m[2] * m[7] + m[9] * m[3] * m[6]
        inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11] + m[4] * m[3] * m[10] + m[8] * m[2] * m[7] - m[8] * m[3] * m[6]
        inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11] - m[4] * m[3] * m[9] - m[8] * m[1] * m[7] + m[8] * m[3] * m[5]
        inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10] + m[4] * m[2] * m[9] + m[8] * m[1] * m[6] - m[8] * m[2] * m[5]
        val det = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12]
        return DoubleArray(16) { inv[it] / det }
    }

    fun toFloat(m: DoubleArray) = FloatArray(m.size) { m[it].toFloat() }
}

/**
 * The camera at scene time t, with the renderer's rules ("Camera interpolation and holds" in
 * fleet/PROTOCOL.md): Catmull-Rom between keyframes that differ, tangents scaled to each
 * segment's duration, linear field of view, zero tangents next to a hold and at the ends.
 * Without keyframes, the park's own slow drift.
 */
object CameraPath {
    data class Pose(val pos: Vec3, val look: Vec3, val fovDeg: Double)

    fun at(keys: List<CameraKey>, t: Double): Pose {
        if (keys.isEmpty()) return builtIn(t)
        if (t <= keys.first().t) return keys.first().pose()
        if (t >= keys.last().t) return keys.last().pose()
        var i = 0
        while (i + 1 < keys.size && keys[i + 1].t <= t) i++
        val a = keys[i]; val b = keys[i + 1]
        val dt = b.t - a.t
        if (dt <= 0 || same(a, b)) return a.pose()
        val s = (t - a.t) / dt
        val ta = tangent(keys, i); val tb = tangent(keys, i + 1)
        fun herm(p0: Vec3, m0: Vec3, p1: Vec3, m1: Vec3): Vec3 {
            val s2 = s * s; val s3 = s2 * s
            return p0 * (2 * s3 - 3 * s2 + 1) + m0 * ((s3 - 2 * s2 + s) * dt) + p1 * (-2 * s3 + 3 * s2) + m1 * ((s3 - s2) * dt)
        }
        return Pose(herm(a.pos, ta.first, b.pos, tb.first), herm(a.look, ta.second, b.look, tb.second), a.fovDeg + (b.fovDeg - a.fovDeg) * s)
    }

    private fun CameraKey.pose() = Pose(pos, look, fovDeg)
    private fun same(a: CameraKey, b: CameraKey) = a.pos == b.pos && a.look == b.look && a.fovDeg == b.fovDeg
    private val ZERO = Vec3(0.0, 0.0, 0.0)

    /** Per-second tangents of pos and look at key i; zero at the ends and next to a hold. */
    private fun tangent(k: List<CameraKey>, i: Int): Pair<Vec3, Vec3> {
        if (i == 0 || i == k.size - 1) return ZERO to ZERO
        if (same(k[i], k[i - 1]) || same(k[i], k[i + 1])) return ZERO to ZERO
        val dt = k[i + 1].t - k[i - 1].t
        if (dt <= 0) return ZERO to ZERO
        return (k[i + 1].pos - k[i - 1].pos) * (1 / dt) to (k[i + 1].look - k[i - 1].look) * (1 / dt)
    }

    /** The park's own camera: a slow sway over the lawn, the same on every phone. */
    fun builtIn(t: Double): Pose {
        val a = 2 * StrictMath.PI * Time.wrap(t, 240.0) / 240.0
        val b = 2 * StrictMath.PI * Time.wrap(t, 150.0) / 150.0
        return Pose(
            Vec3(5.0 * StrictMath.sin(a), 8.0 + 0.8 * StrictMath.sin(b), 30.0),
            Vec3(1.5 * StrictMath.sin(a), 1.6, -4.0),
            40.0,
        )
    }
}

object Time {
    /** t mod period in [0, period), computed in doubles so a large t keeps its precision. */
    fun wrap(t: Double, period: Double): Double {
        val r = t - StrictMath.floor(t / period) * period
        return if (r >= period) 0.0 else r
    }
}
