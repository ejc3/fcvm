package dev.fcvm.robotpark.scene

/**
 * The dance and wave moves taken on this phone (the coordinator's own are in the scene), in
 * the scene clock of the epoch they were made on, with the walking time frozen by those pruned
 * ([FrameState.pruneMoves]) and the end of the last pruned window. One immutable value, so a
 * frame never pairs a pruned list with an old offset.
 */
data class LocalMoves(
    val epochUnixMs: Long = 0,
    val moves: List<Move> = emptyList(),
    val walkOffset: Double = 0.0,
    val from: Double = Double.NEGATIVE_INFINITY,
) {
    /** These moves in `scene`'s clock: none when the scene has another epoch, since its t restarted. */
    fun forScene(scene: SceneInput): LocalMoves = if (scene.epochUnixMs == epochUnixMs) this else LocalMoves(scene.epochUnixMs)

    fun add(move: Move, scene: SceneInput, now: Double, max: Int = MAX): LocalMoves {
        val base = forScene(scene)
        val (kept, gained, until) = FrameState.pruneMoves(base.moves + move, scene.moves, now, max)
        return LocalMoves(scene.epochUnixMs, kept, base.walkOffset + gained, maxOf(base.from, until))
    }

    /** `scene` with these moves added and `night` set, as the frame draws it. */
    fun applyTo(scene: SceneInput, night: Boolean?): SceneInput {
        val l = forScene(scene)
        if (l.moves.isEmpty() && night == scene.night && l.walkOffset == 0.0) return scene
        return scene.copy(moves = (scene.moves + l.moves).sortedBy { it.t }, night = night, walkOffset = l.walkOffset, walkOffsetFrom = l.from)
    }

    companion object { const val MAX = 64 }
}
