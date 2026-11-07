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
 * Whether the frame drawn now carries the held bit: from the first frame drawn held, and for
 * [Stamp.HELD_TAIL_FRAMES] frames after the last. The bars are lifted only after a held frame
 * reports it and come down when the phone is given back, so every frame drawn while they are
 * lifted carries the bit.
 */
class HeldLayout {
    private var tail = 0

    fun next(held: Boolean): Boolean {
        if (held) {
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
