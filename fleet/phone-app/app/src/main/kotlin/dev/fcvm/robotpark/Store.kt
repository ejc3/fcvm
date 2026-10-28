package dev.fcvm.robotpark

import dev.fcvm.robotpark.scene.Crowd
import dev.fcvm.robotpark.scene.FrameState
import dev.fcvm.robotpark.scene.LocalMoves
import dev.fcvm.robotpark.scene.Move
import dev.fcvm.robotpark.scene.MoveKind
import dev.fcvm.robotpark.scene.SceneInput

/**
 * The app's state, shared by the control socket, the UI and the GL thread. Every field is an
 * immutable value swapped whole, so a reader never sees half an update.
 */
object Store {
    @Volatile var scene: SceneInput = SceneInput()
    /** dance and wave actions taken on this phone (the coordinator's own are in the scene). */
    @Volatile var local = LocalMoves()
    @Volatile var nightOverride: Boolean? = null
    @Volatile var systemNight = false
    @Volatile var frozenT: Double? = null
    @Volatile var clockOffsetMs = 0L
    @Volatile var place: Map<String, String> = emptyMap()
    @Volatile var robots = Crowd.ROBOTS

    // what the GL thread reports
    @Volatile var fps = 0.0
    @Volatile var frames = 0L
    @Volatile var frameMs = 0.0
    @Volatile var lastT = 0.0
    @Volatile var renderer = ""
    @Volatile var vendor = ""
    @Volatile var glVersion = ""
    @Volatile var glsl = ""
    @Volatile var contextGeneration = 0
    @Volatile var surfaceW = 0
    @Volatile var surfaceH = 0
    @Volatile var samples = 0
    @Volatile var benchmark: Map<String, Any?>? = null
    @Volatile var benchmarkRunning = false
    @Volatile var screen = "scene"
    @Volatile var lastError: String? = null

    /** When this process started (not when this object was first used). */
    val startedElapsedMs = android.os.Process.getStartElapsedRealtime()

    fun effectiveScene(): SceneInput {
        val s = scene
        return local.applyTo(s, nightOverride ?: s.night)
    }

    /**
     * The scene time of the frame due now: the 1/60 s grid every phone draws on, or the frozen t.
     * It takes the scene snapshot the frame draws, so the two never come from different scenes.
     */
    fun frameTime(scene: SceneInput): Double {
        frozenT?.let { return it }
        val k = FrameState.frameIndexAt(System.currentTimeMillis() + clockOffsetMs, scene.epochUnixMs)
        return k.toDouble() / FrameState.FPS
    }

    /**
     * Adds a move at `t`, or at the current frame's time, and returns that time. The time and
     * the epoch the move is filed under come from one scene snapshot; a scene of another epoch
     * arriving meanwhile clears the move with the rest of the old clock's (LocalMoves).
     */
    @Synchronized fun addMove(kind: MoveKind, t: Double?): Double {
        val s = scene
        val now = frameTime(s)
        val at = t ?: now
        local = local.add(Move(at, kind), s, now)
        return at
    }
}
