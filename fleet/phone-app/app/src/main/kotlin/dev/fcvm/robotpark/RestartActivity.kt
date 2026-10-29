package dev.fcvm.robotpark

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.os.Process

/**
 * The restart action: the app starts this activity in a process of its own and exits; this
 * makes sure the old process is gone, starts the app again in a new one, and exits too.
 */
class RestartActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pid = intent.getIntExtra(EXTRA_PID, -1)
        if (pid > 0) Process.killProcess(pid)
        startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        finish()
        Runtime.getRuntime().exit(0)
    }

    companion object {
        private const val EXTRA_PID = "pid"

        fun restart(from: Activity) {
            // let go of EGL first, so the emulator's renderer has nothing of ours to clean up
            GlThread.current?.quit()
            from.startActivity(Intent(from, RestartActivity::class.java)
                .putExtra(EXTRA_PID, Process.myPid())
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            Runtime.getRuntime().exit(0)
        }
    }
}
