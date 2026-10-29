package dev.fcvm.robotpark.scene

/**
 * Indexed triangle meshes, built in code. A vertex is 8 floats: position, normal, then
 * (part, material). Parts are what the vertex shader moves (the robot's head, arms and legs);
 * materials pick the colour (0 the instance's colour, 1 its second colour, 2 white eyes that
 * glow at night, 3 dark detail). A sign vertex carries (u, v) instead of (part, material) and
 * the material in u's integer part (see [sign]).
 */
class Mesh(val vertices: FloatArray, val indices: ShortArray) {
    val vertexCount get() = vertices.size / STRIDE
    companion object { const val STRIDE = 8 }
}

class MeshBuilder {
    private val v = ArrayList<Float>()
    private val idx = ArrayList<Short>()
    private val n get() = v.size / Mesh.STRIDE

    fun build(): Mesh {
        check(n <= 65535) { "mesh has $n vertices" }
        return Mesh(v.toFloatArray(), idx.toShortArray())
    }

    private fun vert(p: Vec3, nrm: Vec3, a: Double, b: Double) {
        v.add(p.x.toFloat()); v.add(p.y.toFloat()); v.add(p.z.toFloat())
        val u = nrm.normalized()
        v.add(u.x.toFloat()); v.add(u.y.toFloat()); v.add(u.z.toFloat())
        v.add(a.toFloat()); v.add(b.toFloat())
    }

    /**
     * A surface of revolution about the local y axis from a profile of (radius, y) points,
     * placed by [place] (applied to positions; [placeN] to normals).
     */
    fun lathe(profile: List<Pair<Double, Double>>, segments: Int, part: Int, mat: Int,
              place: (Vec3) -> Vec3 = { it }, placeN: (Vec3) -> Vec3 = { it }) {
        val base = n
        for ((i, pr) in profile.withIndex()) {
            val prev = profile[maxOf(0, i - 1)]; val next = profile[minOf(profile.size - 1, i + 1)]
            val dr = next.first - prev.first; val dy = next.second - prev.second
            for (s in 0..segments) {
                val a = 2 * StrictMath.PI * s / segments
                val c = StrictMath.cos(a); val sn = StrictMath.sin(a)
                val p = Vec3(pr.first * sn, pr.second, pr.first * c)
                // normal of the profile curve (dy, -dr) rotated about y
                val nr = Vec3(dy * sn, -dr, dy * c)
                vert(place(p), placeN(if (nr.length() < 1e-9) Vec3(0.0, if (pr.second > 0.5) 1.0 else -1.0, 0.0) else nr), part.toDouble(), mat.toDouble())
            }
        }
        val ring = segments + 1
        for (i in 0 until profile.size - 1) for (s in 0 until segments) {
            val a = base + i * ring + s; val b = a + ring
            tri(a, b, a + 1); tri(a + 1, b, b + 1)
        }
    }

    private fun tri(a: Int, b: Int, c: Int) { idx.add(a.toShort()); idx.add(b.toShort()); idx.add(c.toShort()) }

    fun sphere(center: Vec3, r: Vec3, segments: Int, rings: Int, part: Int, mat: Int) {
        val prof = (0..rings).map { i ->
            val a = StrictMath.PI * i / rings
            StrictMath.sin(a) to -StrictMath.cos(a)
        }
        lathe(prof, segments, part, mat,
            { p -> Vec3(center.x + p.x * r.x, center.y + p.y * r.y, center.z + p.z * r.z) },
            { q -> Vec3(q.x / r.x, q.y / r.y, q.z / r.z) })
    }

    /** A capsule of radius r from `a` to `b`. */
    fun capsule(a: Vec3, b: Vec3, r: Double, segments: Int, part: Int, mat: Int) {
        val axis = b - a
        val len = axis.length()
        val prof = ArrayList<Pair<Double, Double>>()
        val k = 5
        for (i in 0..k) { val t = StrictMath.PI / 2 * i / k; prof.add(r * StrictMath.sin(t) to -r * StrictMath.cos(t)) }
        for (i in 0..k) { val t = StrictMath.PI / 2 * i / k; prof.add(r * StrictMath.cos(t) to len + r * StrictMath.sin(t)) }
        val (x, z) = basis(axis.normalized())
        val y = axis.normalized()
        lathe(prof, segments, part, mat,
            { p -> a + x * p.x + y * p.y + z * p.z },
            { q -> x * q.x + y * q.y + z * q.z })
    }

    /** A cone or cylinder (r0 at y0 to r1 at y1) with caps, about the y axis at `at`. */
    fun cone(at: Vec3, r0: Double, y0: Double, r1: Double, y1: Double, segments: Int, part: Int, mat: Int) {
        lathe(listOf(0.0 to y0, r0 to y0, r0 to y0, r1 to y1, r1 to y1, 0.0 to y1), segments, part, mat, { p -> p + at })
    }

    /** An axis-aligned box; with `uvFront`, the +z face carries texture coordinates for the sign. */
    fun box(lo: Vec3, hi: Vec3, mat: Int, uvFront: Boolean = false) {
        val faces = listOf(
            Vec3(0.0, 0.0, 1.0), Vec3(0.0, 0.0, -1.0), Vec3(1.0, 0.0, 0.0),
            Vec3(-1.0, 0.0, 0.0), Vec3(0.0, 1.0, 0.0), Vec3(0.0, -1.0, 0.0),
        )
        for (f in faces) {
            val (u, w) = basis(f)
            val c = Vec3((lo.x + hi.x) / 2, (lo.y + hi.y) / 2, (lo.z + hi.z) / 2)
            val h = Vec3((hi.x - lo.x) / 2, (hi.y - lo.y) / 2, (hi.z - lo.z) / 2)
            val base = n
            for ((su, sw) in listOf(-1.0 to -1.0, 1.0 to -1.0, 1.0 to 1.0, -1.0 to 1.0)) {
                val d = f + u * su + w * sw
                val p = Vec3(c.x + d.x * h.x, c.y + d.y * h.y, c.z + d.z * h.z)
                val front = uvFront && f.z > 0.5
                // the front face's (u, v) runs left to right, top to bottom over the board
                val uu = if (front) (p.x - lo.x) / (hi.x - lo.x) else 0.0
                val vv = if (front) (hi.y - p.y) / (hi.y - lo.y) else 0.0
                vert(p, f, if (front) uu else mat.toDouble() + 10.0, vv)
            }
            // wind counter-clockwise seen from outside
            val cr = (u.cross(w)).dot(f)
            if (cr > 0) { tri(base, base + 1, base + 2); tri(base, base + 2, base + 3) }
            else { tri(base, base + 2, base + 1); tri(base, base + 3, base + 2) }
        }
    }

    companion object {
        /** Two unit vectors perpendicular to `d` and to each other, (x, z) of a frame whose y is d. */
        fun basis(d: Vec3): Pair<Vec3, Vec3> {
            val ref = if (StrictMath.abs(d.y) < 0.9) Vec3(0.0, 1.0, 0.0) else Vec3(1.0, 0.0, 0.0)
            val x = ref.cross(d).normalized()
            val z = x.cross(d).normalized()
            return x to z
        }
    }
}

object Meshes {
    const val PART_BODY = 0
    const val PART_HEAD = 1
    const val PART_ARM_L = 2
    const val PART_ARM_R = 3
    const val PART_LEG_L = 4
    const val PART_LEG_R = 5
    const val MAT_COLOUR = 0
    const val MAT_SECOND = 1
    const val MAT_EYE = 2
    const val MAT_DARK = 3

    /** The robot, about 1 m to the top of its head, facing +z, feet at y = 0. */
    fun robot(seg: Int = 18): Mesh = MeshBuilder().apply {
        // body: a cylinder with a rounded bottom edge and a slightly rounded top edge
        val body = ArrayList<Pair<Double, Double>>()
        body.add(0.0 to 0.2)
        for (i in 0..4) { val a = StrictMath.PI / 2 * i / 4; body.add(0.24 + 0.08 * StrictMath.sin(a) to 0.28 - 0.08 * StrictMath.cos(a)) }
        body.add(0.32 to 0.6)
        body.add(0.31 to 0.635)
        body.add(0.0 to 0.64)
        lathe(body, seg, PART_BODY, MAT_COLOUR)
        // head: a dome over a flat base, with a gap above the body
        val head = ArrayList<Pair<Double, Double>>()
        head.add(0.0 to 0.67)
        for (i in 0..8) { val a = StrictMath.PI / 2 * i / 8; head.add(0.31 * StrictMath.cos(a) to 0.67 + 0.31 * StrictMath.sin(a)) }
        lathe(head, seg, PART_HEAD, MAT_COLOUR)
        for (sx in listOf(1.0, -1.0)) {
            val polar = StrictMath.toRadians(38.0)
            val base = Vec3(sx * 0.31 * StrictMath.sin(polar) * 0.92, 0.67 + 0.31 * StrictMath.cos(polar) * 0.92, 0.0)
            val dir = Vec3(sx * StrictMath.sin(StrictMath.toRadians(30.0)), StrictMath.cos(StrictMath.toRadians(30.0)), 0.0)
            capsule(base, base + dir * 0.15, 0.022, 8, PART_HEAD, MAT_COLOUR)
            sphere(Vec3(sx * 0.115, 0.80, 0.252), Vec3(0.042, 0.042, 0.02), 10, 6, PART_HEAD, MAT_EYE)
            capsule(Vec3(sx * 0.42, 0.56, 0.0), Vec3(sx * 0.42, 0.33, 0.0), 0.072, 12, if (sx > 0) PART_ARM_L else PART_ARM_R, MAT_COLOUR)
            capsule(Vec3(sx * 0.14, 0.2, 0.0), Vec3(sx * 0.14, 0.085, 0.0), 0.085, 12, if (sx > 0) PART_LEG_L else PART_LEG_R, MAT_COLOUR)
        }
    }.build()

    /** A round friend: a ball with eyes and one antenna, radius 0.5, resting on y = 0. */
    fun buddy(): Mesh = MeshBuilder().apply {
        sphere(Vec3(0.0, 0.5, 0.0), Vec3(0.5, 0.5, 0.5), 20, 12, 0, MAT_COLOUR)
        for (sx in listOf(1.0, -1.0)) sphere(Vec3(sx * 0.17, 0.62, 0.455), Vec3(0.075, 0.09, 0.04), 10, 6, 0, MAT_EYE)
        capsule(Vec3(0.0, 0.98, 0.0), Vec3(0.06, 1.16, 0.0), 0.025, 8, 0, MAT_COLOUR)
        sphere(Vec3(0.07, 1.2, 0.0), Vec3(0.06, 0.06, 0.06), 10, 6, 0, MAT_EYE)
    }.build()

    fun roundTree(): Mesh = MeshBuilder().apply {
        cone(Vec3(0.0, 0.0, 0.0), 0.13, 0.0, 0.09, 1.3, 10, 1, MAT_SECOND)
        sphere(Vec3(0.0, 1.75, 0.0), Vec3(0.78, 0.72, 0.78), 16, 10, 0, MAT_COLOUR)
        sphere(Vec3(0.42, 1.38, 0.12), Vec3(0.5, 0.46, 0.5), 12, 8, 0, MAT_COLOUR)
        sphere(Vec3(-0.36, 1.45, -0.18), Vec3(0.55, 0.5, 0.55), 12, 8, 0, MAT_COLOUR)
    }.build()

    fun pineTree(): Mesh = MeshBuilder().apply {
        cone(Vec3(0.0, 0.0, 0.0), 0.12, 0.0, 0.08, 0.8, 10, 1, MAT_SECOND)
        cone(Vec3(0.0, 0.0, 0.0), 0.85, 0.6, 0.0, 1.6, 14, 0, MAT_COLOUR)
        cone(Vec3(0.0, 0.0, 0.0), 0.68, 1.2, 0.0, 2.15, 14, 0, MAT_COLOUR)
        cone(Vec3(0.0, 0.0, 0.0), 0.5, 1.75, 0.0, 2.6, 14, 0, MAT_COLOUR)
    }.build()

    fun flower(): Mesh = MeshBuilder().apply {
        cone(Vec3(0.0, 0.0, 0.0), 0.014, 0.0, 0.012, 0.24, 5, 1, MAT_SECOND)
        sphere(Vec3(0.0, 0.26, 0.0), Vec3(0.085, 0.04, 0.085), 8, 5, 0, MAT_COLOUR)
        sphere(Vec3(0.0, 0.29, 0.0), Vec3(0.03, 0.02, 0.03), 6, 4, 0, MAT_EYE)
    }.build()

    /** A unit sphere on its base (hills, clouds and lights are scaled spheres). */
    fun ball(seg: Int = 24, rings: Int = 14): Mesh = MeshBuilder().apply {
        sphere(Vec3(0.0, 0.0, 0.0), Vec3(1.0, 1.0, 1.0), seg, rings, 0, MAT_COLOUR)
    }.build()

    /** The sign: a board on a pole. The board's front face carries (u, v); every other vertex 10 + its material in u. */
    fun sign(): Mesh = MeshBuilder().apply {
        box(Vec3(-1.5, 2.05, -0.05), Vec3(1.5, 2.95, 0.05), MAT_COLOUR, uvFront = true)
        box(Vec3(-0.04, 0.85, -0.04), Vec3(0.04, 2.08, 0.04), MAT_SECOND)
    }.build()

    /** The ground: one quad over the whole park, in world coordinates. */
    fun ground(): Mesh = MeshBuilder().apply {
        box(Vec3(-900.0, -0.02, -480.0), Vec3(900.0, 0.0, 60.0), MAT_COLOUR)
    }.build()
}
