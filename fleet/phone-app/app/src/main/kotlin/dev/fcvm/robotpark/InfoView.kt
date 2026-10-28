package dev.fcvm.robotpark

import android.content.Context
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.SystemClock
import android.util.TypedValue
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView

/**
 * The info tab: the device, the GPU this app's GLES context runs on, frames per second,
 * uptime, where the phone runs (the fcvm VM and host, from the agent's `place`), the
 * benchmark, and the attribution and trademark lines.
 */
class InfoView(context: Context) : ScrollView(context) {
    private val column = LinearLayout(context).apply { orientation = LinearLayout.VERTICAL }
    private val values = HashMap<String, TextView>()
    private val cards = ArrayList<LinearLayout>()
    private val texts = ArrayList<TextView>()
    private val subtle = ArrayList<TextView>()
    private var night = false
    private val tick = object : Runnable {
        override fun run() { if (isShown) refresh(); postDelayed(this, 500) }
    }

    init {
        val d = resources.displayMetrics.density
        column.setPadding((16 * d).toInt(), (84 * d).toInt(), (16 * d).toInt(), (96 * d).toInt())
        addView(column)
        heading("Phone")
        card("Device", "Android version", "Uptime")
        heading("GPU")
        card("Renderer", "Vendor", "OpenGL ES", "Frames per second", "Frame time", "Multisampling", "Context")
        heading("Where it runs")
        card("fcvm VM", "Slot", "Host", "Host GPU", "Region", "Restored in")
        heading("Scene")
        card("Scene time", "Surface", "Robots", "Benchmark")
        heading("About")
        val about = newCard()
        for (s in listOf(
            "Robot Park draws one scene across a wall of phones; each phone draws its own part of it.",
            ATTRIBUTION,
            "Android is a trademark of Google LLC.",
            "Robot Park is not made, sponsored or endorsed by Google.",
        )) about.addView(text(s, 14f, false).also { subtle.add(it) })
        setNight(false)
        post(tick)
    }

    fun setNight(n: Boolean) {
        night = n
        setBackgroundColor(if (n) 0xFF141218.toInt() else 0xFFFEF7FF.toInt())
        for (c in cards) (c.background as GradientDrawable).setColor(if (n) 0xFF2B2930.toInt() else 0xFFF3EDF7.toInt())
        for (t in texts) t.setTextColor(if (n) 0xFFE6E0E9.toInt() else 0xFF1D1B20.toInt())
        for (t in subtle) t.setTextColor(if (n) 0xFFCAC4D0.toInt() else 0xFF49454F.toInt())
    }

    fun refresh() {
        val up = (SystemClock.elapsedRealtime() - Store.startedElapsedMs) / 1000
        set("Device", "${Build.MANUFACTURER} ${Build.MODEL}")
        set("Android version", "${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})")
        set("Uptime", "app %d:%02d:%02d, phone %d min".format(up / 3600, up / 60 % 60, up % 60, SystemClock.elapsedRealtime() / 60000))
        set("Renderer", Store.renderer.ifEmpty { "not started" })
        set("Vendor", Store.vendor)
        set("OpenGL ES", Store.glVersion)
        set("Frames per second", "%.1f".format(Store.fps))
        set("Frame time", "%.2f ms of CPU and GL calls".format(Store.frameMs))
        set("Multisampling", "${Store.samples}x")
        set("Context", "generation ${Store.contextGeneration}")
        val p = Store.place
        set("fcvm VM", p["vm"] ?: "not set by the agent")
        set("Slot", p["slot"] ?: "")
        set("Host", p["host"] ?: "")
        set("Host GPU", p["gpu"] ?: "")
        set("Region", p["region"] ?: "")
        set("Restored in", p["restored_in_ms"]?.let { "$it ms" } ?: "")
        set("Scene time", "%.3f s".format(Store.lastT))
        set("Surface", "${Store.surfaceW} x ${Store.surfaceH}")
        set("Robots", "${Store.robots}")
        @Suppress("UNCHECKED_CAST")
        val stages = Store.benchmark?.get("stages") as? List<Map<String, Any?>>
        set("Benchmark", when {
            Store.benchmarkRunning -> "running"
            stages != null -> stages.joinToString("\n") { "${it["robots"]} robots: ${it["ms_per_frame"]} ms (${it["fps"]} fps)" }
            else -> "not run"
        })
    }

    private fun set(k: String, v: String) { values[k]?.text = v }

    private fun heading(s: String) {
        column.addView(text(s, 15f, true).also {
            it.setPadding(it.paddingLeft, (18 * resources.displayMetrics.density).toInt(), 0, (6 * resources.displayMetrics.density).toInt())
            it.setTextColor(0xFF6750A4.toInt())
        })
    }

    private fun newCard(): LinearLayout {
        val d = resources.displayMetrics.density
        val c = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setPadding((16 * d).toInt(), (12 * d).toInt(), (16 * d).toInt(), (12 * d).toInt())
            background = GradientDrawable().apply { cornerRadius = 20 * d }
        }
        column.addView(c)
        cards.add(c)
        return c
    }

    private fun card(vararg keys: String) {
        val c = newCard()
        for (k in keys) {
            c.addView(text(k, 12f, false).also { subtle.add(it) })
            val v = text("", 16f, true)
            v.setPadding(0, 0, 0, (8 * resources.displayMetrics.density).toInt())
            values[k] = v
            c.addView(v)
        }
    }

    private fun text(s: String, sp: Float, bold: Boolean) = TextView(context).apply {
        text = s
        setTextSize(TypedValue.COMPLEX_UNIT_SP, sp)
        if (bold) typeface = Typeface.DEFAULT_BOLD
        texts.add(this)
    }

    companion object {
        const val ATTRIBUTION = "The Android robot is reproduced or modified from work created and shared by Google and " +
            "used according to terms described in the Creative Commons 3.0 Attribution License."
    }
}
