package dev.fcvm.robotpark

import android.app.Application
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import dev.fcvm.robotpark.scene.Action
import dev.fcvm.robotpark.scene.Command
import dev.fcvm.robotpark.scene.MoveKind

/** Starts the control socket in the app's main process and routes every command. */
class PhoneApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // RestartActivity runs in a process of its own; only the app's main process listens.
        if (getProcessName() != packageName) return
        ControlServer(::handle).start()
    }

    companion object {
        private val main = Handler(Looper.getMainLooper())
        /** The activity on screen, which carries out the actions that need one. */
        @Volatile var ui: MainActivity? = null

        fun handle(c: Command): Map<String, Any?> = when (c) {
            is Command.SetScene -> { Store.scene = c.scene; ok() }
            is Command.Act -> act(c.action, c.t)
            is Command.Place -> { Store.place = c.fields; ok() }
            is Command.Clock -> { Store.clockOffsetMs = c.unixMs - System.currentTimeMillis(); ok("offset_ms" to Store.clockOffsetMs) }
            is Command.Freeze -> { Store.frozenT = c.t; ok() }
            is Command.Capture -> GlThread.current?.capture(c) ?: mapOf("error" to "no renderer: the app is not on screen")
            is Command.Status -> status()
            is Command.DropContext -> GlThread.current?.dropContext() ?: mapOf("error" to "no renderer: the app is not on screen")
        }

        fun act(a: Action, t: Double?): Map<String, Any?> {
            Log.i(ControlServer.TAG, "action ${a.wire}" + (t?.let { " t=$it" } ?: ""))
            when (a) {
                Action.DANCE, Action.WAVE -> {
                    val at = t ?: Store.frameTime()
                    Store.addMove(if (a == Action.DANCE) MoveKind.DANCE else MoveKind.WAVE, at)
                    return ok("t" to at)
                }
                Action.DARK -> Store.nightOverride = true
                Action.LIGHT -> Store.nightOverride = false
                else -> {}
            }
            val activity = ui
            if (activity == null) {
                return if (a == Action.DARK || a == Action.LIGHT) ok() else mapOf("error" to "the app is not on screen")
            }
            main.post { activity.perform(a) }
            return ok()
        }

        fun status(): Map<String, Any?> = ok(
            "t" to Store.lastT,
            "fps" to Store.fps,
            "frames" to Store.frames,
            "frame_ms" to Store.frameMs,
            "renderer" to Store.renderer,
            "vendor" to Store.vendor,
            "gl_version" to Store.glVersion,
            "surface" to listOf(Store.surfaceW, Store.surfaceH),
            "samples" to Store.samples,
            "context_generation" to Store.contextGeneration,
            "screen" to Store.screen,
            "night" to (Store.effectiveScene().night ?: Store.systemNight),
            "robots" to Store.robots,
            "benchmark" to (Store.benchmark ?: if (Store.benchmarkRunning) mapOf("running" to true) else null),
            "uptime_ms" to (SystemClock.elapsedRealtime() - Store.startedElapsedMs),
            "device" to "${Build.MANUFACTURER} ${Build.MODEL}",
            "android" to "${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})",
            "place" to Store.place,
            "last_error" to Store.lastError,
        ).filterValues { it != null }

        fun ok(vararg kv: Pair<String, Any?>): Map<String, Any?> = linkedMapOf<String, Any?>("ok" to true).apply { putAll(kv) }
    }
}
