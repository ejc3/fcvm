package dev.fcvm.robotpark.scene

/**
 * The dance and wave moves taken on this phone (the coordinator's own are in the scene), in
 * the scene clock of the epoch they were made on, and the end of the last pruned window. They
 * set the crowd's pose and never its walk, so dropping old ones changes no later frame.
 */
data class LocalMoves(
    val epochUnixMs: Long = 0,
    val moves: List<Move> = emptyList(),
    val from: Double = Double.NEGATIVE_INFINITY,
) {
    /** These moves in `scene`'s clock: none when the scene has another epoch, since its t restarted. */
    fun forScene(scene: SceneInput): LocalMoves = if (scene.epochUnixMs == epochUnixMs) this else LocalMoves(scene.epochUnixMs)

    /**
     * These moves with `move` added (and pruned to `max`). A move timed before the pruned
     * history is refused: frames at that time can no longer be drawn exactly.
     */
    fun add(move: Move, scene: SceneInput, now: Double, max: Int = MAX): LocalMoves {
        val base = forScene(scene)
        require(move.t >= base.from) { "t: before t=${base.from}, where this phone's own older moves were pruned" }
        val (kept, until) = FrameState.pruneMoves(base.moves + move, scene.moves, now, max)
        return LocalMoves(scene.epochUnixMs, kept, maxOf(base.from, until))
    }

    /** `scene` with these moves and `night` set, as the frame draws it. */
    fun applyTo(scene: SceneInput, night: Boolean?): SceneInput {
        val l = forScene(scene)
        if (l.moves.isEmpty() && night == scene.night && l.from == Double.NEGATIVE_INFINITY) return scene
        return scene.copy(localMoves = l.moves, night = night, historyFrom = l.from)
    }

    companion object { const val MAX = 64 }
}
