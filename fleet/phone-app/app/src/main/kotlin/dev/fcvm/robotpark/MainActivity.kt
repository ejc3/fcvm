package dev.fcvm.robotpark

import android.app.Activity
import android.content.Intent
import android.content.pm.ActivityInfo
import android.content.res.Configuration
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import dev.fcvm.robotpark.scene.Action

/**
 * The app's one window: the scene on a SurfaceView, full screen with the system bars hidden;
 * over it the app's own interface when the scene asks for it (zoomed out) or the info tab is
 * open: a top bar, the info screen, and a bottom bar with the two tabs.
 *
 * Actions arrive as an intent (`am start -a dev.fcvm.robotpark.ACTION --es name dance`) or on
 * the control socket; both end in [perform]. Only the names in [Action] are taken.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {
    private lateinit var gl: GlThread
    private lateinit var root: FrameLayout
    private lateinit var topBar: LinearLayout
    private lateinit var tabBar: LinearLayout
    private lateinit var info: InfoView
    private lateinit var banner: TextView
    private lateinit var tabScene: TextView
    private lateinit var tabInfo: TextView
    private var chromeFromScene = false
    private val hideBanner = Runnable { banner.visibility = View.GONE }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        window.setDecorFitsSystemWindows(false)
        Store.systemNight = isSystemNight(resources.configuration)

        root = FrameLayout(this)
        val surface = SurfaceView(this)
        surface.holder.addCallback(this)
        root.addView(surface, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        info = InfoView(this)
        info.visibility = View.GONE
        root.addView(info, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        topBar = bar(Gravity.TOP)
        topBar.addView(label("Robot Park", 22f, true))
        root.addView(topBar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.TOP))
        tabBar = bar(Gravity.BOTTOM)
        tabScene = tab("Scene") { perform(Action.SCENE) }
        tabInfo = tab("Info") { perform(Action.INFO) }
        tabBar.addView(tabScene, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        tabBar.addView(tabInfo, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        root.addView(tabBar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM))
        banner = label("", 18f, true).apply {
            visibility = View.GONE
            setPadding(dp(20), dp(12), dp(20), dp(12))
            background = GradientDrawable().apply { cornerRadius = dp(24).toFloat(); setColor(0xE6202124.toInt()) }
            setTextColor(Color.WHITE)
        }
        root.addView(banner, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER))
        setContentView(root)
        hideSystemBars()

        gl = GlThread(applicationContext)
        gl.onChrome = { on -> root.post { chromeFromScene = on; layoutChrome() } }
        gl.start()
        GlThread.current = gl
        PhoneApp.ui = this
        theme()
        layoutChrome()
        handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    override fun onResume() { super.onResume(); gl.setPaused(false); hideSystemBars() }
    override fun onPause() { super.onPause(); gl.setPaused(true) }

    override fun onDestroy() {
        if (PhoneApp.ui === this) PhoneApp.ui = null
        if (GlThread.current === gl) GlThread.current = null
        gl.quit()
        super.onDestroy()
    }

    override fun onConfigurationChanged(c: Configuration) {
        super.onConfigurationChanged(c)
        Store.systemNight = isSystemNight(c)
        theme()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        Log.i(GlThread.TAG, "surface created")
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        Log.i(GlThread.TAG, "surface changed ${width}x$height")
        gl.setWindow(holder.surface, width, height)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        Log.i(GlThread.TAG, "surface destroyed")
        gl.setWindow(null, 0, 0)
    }

    private fun handleIntent(i: Intent?) {
        if (i?.action != ACTION) return
        val name = i.getStringExtra("name") ?: return
        val a = Action.of(name) ?: run { Log.w(GlThread.TAG, "intent: no action '$name'"); return }
        val t = if (i.hasExtra("t")) (i.extras?.get("t") as? Number)?.toDouble() ?: i.getStringExtra("t")?.toDoubleOrNull() else null
        val r = PhoneApp.act(a, t)
        Log.i(GlThread.TAG, "intent ${a.wire}: $r")
    }

    /** Carries out an action on the UI thread (PhoneApp.act has done the scene part already). */
    fun perform(a: Action) {
        when (a) {
            Action.SCENE -> { Store.screen = "scene"; info.visibility = View.GONE }
            Action.INFO -> { Store.screen = "info"; info.visibility = View.VISIBLE; info.refresh() }
            Action.ROTATE -> {
                requestedOrientation = if (resources.configuration.orientation == Configuration.ORIENTATION_LANDSCAPE)
                    ActivityInfo.SCREEN_ORIENTATION_PORTRAIT else ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
                show("Rotating")
            }
            Action.DARK, Action.LIGHT -> { theme(); show(if (a == Action.DARK) "Night in the park" else "Day in the park") }
            Action.DANCE -> show("Dance!")
            Action.WAVE -> show("Everybody wave")
            Action.BENCHMARK -> {
                show("GPU benchmark running")
                gl.benchmark()
                root.postDelayed({ benchmarkDone() }, 500)
            }
            Action.RESTART -> RestartActivity.restart(this)
        }
        layoutChrome()
    }

    private fun benchmarkDone() {
        if (Store.benchmarkRunning) { root.postDelayed({ benchmarkDone() }, 500); return }
        @Suppress("UNCHECKED_CAST")
        val stages = Store.benchmark?.get("stages") as? List<Map<String, Any?>> ?: return
        show(stages.joinToString("\n") { "${it["robots"]} robots: ${it["ms_per_frame"]} ms" }, 6000)
        info.refresh()
    }

    private fun layoutChrome() {
        val visible = chromeFromScene || Store.screen == "info"
        topBar.visibility = if (visible) View.VISIBLE else View.GONE
        tabBar.visibility = if (visible) View.VISIBLE else View.GONE
        val night = night()
        val on = if (night) 0xFFD0BCFF.toInt() else 0xFF6750A4.toInt()
        val off = if (night) 0xFFCAC4D0.toInt() else 0xFF49454F.toInt()
        tabScene.setTextColor(if (Store.screen == "scene") on else off)
        tabInfo.setTextColor(if (Store.screen == "info") on else off)
    }

    private fun theme() {
        val night = night()
        val bar = if (night) 0xF21D1B20.toInt() else 0xF2FEF7FF.toInt()
        val fg = if (night) 0xFFE6E0E9.toInt() else 0xFF1D1B20.toInt()
        topBar.setBackgroundColor(bar)
        tabBar.setBackgroundColor(bar)
        for (i in 0 until topBar.childCount) (topBar.getChildAt(i) as? TextView)?.setTextColor(fg)
        info.setNight(night)
        layoutChrome()
    }

    private fun night() = Store.effectiveScene().night ?: Store.systemNight

    private fun show(text: String, ms: Long = 2500) {
        banner.text = text
        banner.visibility = View.VISIBLE
        banner.removeCallbacks(hideBanner)
        banner.postDelayed(hideBanner, ms)
    }

    private fun hideSystemBars() {
        window.insetsController?.let {
            it.hide(WindowInsets.Type.systemBars())
            it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    private fun bar(gravity: Int) = LinearLayout(this).apply {
        orientation = LinearLayout.HORIZONTAL
        this.gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(20), dp(14), dp(20), dp(14))
        visibility = View.GONE
        if (gravity == Gravity.TOP) elevation = dp(3).toFloat()
    }

    private fun label(s: String, sp: Float, bold: Boolean) = TextView(this).apply {
        text = s
        setTextSize(TypedValue.COMPLEX_UNIT_SP, sp)
        if (bold) setTypeface(typeface, android.graphics.Typeface.BOLD)
    }

    private fun tab(s: String, onClick: () -> Unit) = label(s, 16f, true).apply {
        gravity = Gravity.CENTER
        setPadding(0, dp(8), 0, dp(8))
        setOnClickListener { onClick() }
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun isSystemNight(c: Configuration) = (c.uiMode and Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES

    companion object {
        const val ACTION = "dev.fcvm.robotpark.ACTION"
    }
}
