package dev.fcvm.robotpark.scene

/**
 * Robot Park's frame stamp (PROTOCOL.md, "Phone app", `stamp`): the frame number in 32 black or
 * white cells along the bottom [ROWS] rows of its surface, which the phone streamer and the GPU
 * mirror read. Held, the app lifts its bars by [ROWS] so the band shows below them, and sets
 * [HELD_BIT]: a reader then takes the frame for the whole screen with the band cropped, never
 * for a frame of the shared image.
 */
object Stamp {
    const val ROWS = 8
    const val HELD_BIT = 1L shl 31
    /** Frames the held bit stays set after the phone is given back, while its bars come down. */
    const val HELD_TAIL_FRAMES = 10

    /** The 32 bits a frame writes: its frame number in the low 31, the held bit on top. */
    fun value(frameIndex: Long, heldLayout: Boolean): Long =
        (frameIndex and (HELD_BIT - 1)) or (if (heldLayout) HELD_BIT else 0L)
}

/**
 * Whether the frame drawn now carries the held bit: while the phone is held or the UI says its
 * bars are still lifted, and for [Stamp.HELD_TAIL_FRAMES] frames after both are over, while
 * the lowered layout reaches the screen. The bars are lifted only after a held frame reports
 * it, so every frame drawn while they are lifted carries the bit, however long the UI thread
 * takes to lower them.
 */
class HeldLayout {
    private var tail = 0

    fun next(held: Boolean, barsLifted: Boolean): Boolean {
        if (held || barsLifted) {
            tail = Stamp.HELD_TAIL_FRAMES
            return true
        }
        if (tail > 0) {
            tail--
            return true
        }
        return false
    }
}

/**
 * The UI side of the held layout: whether the bars can still be lifted on screen. [lift] sets
 * it at once. Lowering is asked for with [lowerRequested], and the UI hands its token back to
 * [lowerDrawn] from a draw callback once the lowered layout has been drawn. A lift in between
 * makes the token stale, so the callback of an earlier lowering cannot clear a later one before
 * the later one's layout is drawn.
 */
class LiftedBars {
    var lifted = false
        private set
    private var generation = 0L
    private var pending = false

    fun lift() {
        lifted = true
        generation++
        pending = false
    }

    /** A token to hand to [lowerDrawn], or null while nothing is lifted or a lowering since the last lift is pending. */
    fun lowerRequested(): Long? {
        if (!lifted || pending) return null
        pending = true
        return generation
    }

    fun lowerDrawn(token: Long) {
        if (token != generation || !pending) return
        lifted = false
        pending = false
    }
}
