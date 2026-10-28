package dev.fcvm.robotpark.scene

/** A point or direction in the park, in metres: x across the image, y up, z toward the camera. */
data class Vec3(val x: Double, val y: Double, val z: Double) {
    operator fun plus(o: Vec3) = Vec3(x + o.x, y + o.y, z + o.z)
    operator fun minus(o: Vec3) = Vec3(x - o.x, y - o.y, z - o.z)
    operator fun times(s: Double) = Vec3(x * s, y * s, z * s)
    fun dot(o: Vec3) = x * o.x + y * o.y + z * o.z
    fun cross(o: Vec3) = Vec3(y * o.z - z * o.y, z * o.x - x * o.z, x * o.y - y * o.x)
    fun length() = StrictMath.sqrt(dot(this))
    fun normalized(): Vec3 = length().let { if (it > 0) this * (1.0 / it) else this }
}

/** A part of the whole image as fractions of its width and height, origin top left. */
data class Rect(val x: Double, val y: Double, val w: Double, val h: Double)

data class CameraKey(val t: Double, val pos: Vec3, val look: Vec3, val fovDeg: Double)
data class Word(val text: String, val t: Double)
data class Light(val pos: Vec3, val color: Vec3, val t: Double)
enum class MoveKind(val wire: String) { DANCE("dance"), WAVE("wave") }
data class Move(val t: Double, val kind: MoveKind)
data class ChromeKey(val t: Double, val on: Boolean)

/**
 * Everything a frame depends on besides its time: the coordinator's scene (or the defaults
 * when none has arrived). Two phones with equal inputs and equal `t` draw the same image.
 */
data class SceneInput(
    val epochUnixMs: Long = 0,
    val rect: Rect = Rect(0.0, 0.0, 1.0, 1.0),
    /** Whole image width / height; null draws the phone's own surface as the whole image. */
    val imageAspect: Double? = null,
    val camera: List<CameraKey> = emptyList(),
    val words: List<Word> = emptyList(),
    val lights: List<Light> = emptyList(),
    val moves: List<Move> = emptyList(),
    val chrome: List<ChromeKey> = emptyList(),
    val night: Boolean? = null,
    val stamp: Boolean = false,
    val previousRect: Rect? = null,
    val rectFromFrame: Long? = null,
) {
    /** The rect frame `t` is drawn with: `previous_rect` before `rect_from_frame` (on the 1/30 s grid). */
    fun rectAt(t: Double): Rect {
        val from = rectFromFrame ?: return rect
        return if (StrictMath.floor(t * 30.0 + 1e-9) < from) previousRect ?: rect else rect
    }

    /**
     * The rect and image aspect a `w x h` surface draws at t. A surface turned the other way
     * from its rect (a phone rotated out of the wall) draws the whole image on its own, at its
     * own aspect; otherwise the scene's rect, or the whole surface when no aspect was given.
     */
    fun viewAt(t: Double, w: Int, h: Int): Pair<Rect, Double> {
        val surface = w.toDouble() / h
        val aspect = imageAspect ?: return Rect(0.0, 0.0, 1.0, 1.0) to surface
        val r = rectAt(t)
        val rectAspect = r.w * aspect / r.h
        if ((rectAspect > 1) != (surface > 1)) return Rect(0.0, 0.0, 1.0, 1.0) to surface
        return r to aspect
    }

    fun chromeAt(t: Double): Boolean = chrome.lastOrNull { it.t <= t }?.on ?: false

    companion object {
        const val MAX_WORD_CHARS = 28

        /** The renderer's rule: printable ASCII kept, other bytes below 0x80 become '?', the rest dropped. */
        fun cleanWord(s: String): String {
            val b = StringBuilder()
            for (c in s) {
                if (c.code >= 0x80) continue
                b.append(if (c.code in 0x20..0x7e) c else '?')
                if (b.length == MAX_WORD_CHARS) break
            }
            return b.toString()
        }
    }
}
