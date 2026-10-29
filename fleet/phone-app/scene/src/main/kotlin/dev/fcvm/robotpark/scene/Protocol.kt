package dev.fcvm.robotpark.scene

/** The fixed actions a phone takes. Nothing else a viewer sends reaches the app. */
enum class Action(val wire: String) {
    SCENE("scene"), INFO("info"), ROTATE("rotate"), DARK("dark"), LIGHT("light"),
    DANCE("dance"), WAVE("wave"), BENCHMARK("benchmark"), RESTART("restart");

    companion object {
        fun of(name: String): Action? = values().firstOrNull { it.wire == name }
    }
}

/** One request on the control socket (fleet/PROTOCOL.md, "Phone app"). */
sealed class Command {
    data class SetScene(val scene: SceneInput) : Command()
    data class Act(val action: Action, val t: Double?) : Command()
    data class Place(val fields: Map<String, String>) : Command()
    data class Clock(val unixMs: Long) : Command()
    data class Freeze(val t: Double?) : Command()
    data class Capture(val name: String, val t: Double, val rect: Rect, val imageAspect: Double, val w: Int, val h: Int) : Command()
    object Status : Command()
    object DropContext : Command()
}

class ProtocolError(message: String) : Exception(message)

object Protocol {
    const val SOCKET_NAME = "dev.fcvm.robotpark"
    const val MAX_LINE = 1 shl 20
    val PLACE_KEYS = listOf("vm", "slot", "host", "gpu", "region", "restored_in_ms")

    fun parse(line: String): Command {
        if (line.length > MAX_LINE) throw ProtocolError("line longer than $MAX_LINE bytes")
        val o = try { Json.parse(line) } catch (e: Json.Error) { throw ProtocolError("not JSON: ${e.message}") }
        if (o !is Map<*, *>) throw ProtocolError("a request is a JSON object")
        @Suppress("UNCHECKED_CAST")
        val m = o as Map<String, Any?>
        return when (val type = m["type"]) {
            "scene" -> Command.SetScene(parseScene(m))
            "action" -> {
                val name = m["name"] as? String ?: throw ProtocolError("name: a string")
                val a = Action.of(name) ?: throw ProtocolError("name: one of ${Action.values().joinToString(", ") { it.wire }}")
                Command.Act(a, optNum(m, "t"))
            }
            "place" -> {
                val f = LinkedHashMap<String, String>()
                for (k in PLACE_KEYS) {
                    val v = m[k] ?: continue
                    val s = if (v is Double) Json.write(v) else v as? String ?: throw ProtocolError("$k: a string or number")
                    f[k] = s.filter { it.code in 0x20..0x7e }.take(64)
                }
                Command.Place(f)
            }
            "clock" -> Command.Clock(long(m, "unix_ms"))
            "freeze" -> Command.Freeze(optNum(m, "t"))
            "capture" -> {
                val name = m["name"] as? String ?: throw ProtocolError("name: a string")
                if (!Regex("[a-z0-9-]{1,40}").matches(name)) throw ProtocolError("name: 1 to 40 of a-z, 0-9 and -")
                val w = num(m, "w").toInt(); val h = num(m, "h").toInt()
                if (w !in 16..8192 || h !in 16..8192) throw ProtocolError("w, h: 16 to 8192")
                Command.Capture(name, num(m, "t"), rect(m["rect"], "rect"), aspect(m), w, h)
            }
            "status" -> Command.Status
            "drop_context" -> Command.DropContext
            else -> throw ProtocolError("type: one of scene, action, place, clock, freeze, capture, status, drop_context (got ${Json.write(type)})")
        }
    }

    /** The renderer's `POST /scene` body, plus the phone's fields (moves, night, chrome, stamp). */
    fun parseScene(m: Map<String, Any?>): SceneInput {
        val camera = list(m, "camera", 4096).mapIndexed { i, k ->
            val o = obj(k, "camera[$i]")
            val fov = num(o, "fov_deg", "camera[].fov_deg")
            if (fov < 1 || fov > 170) throw ProtocolError("camera[].fov_deg: 1 to 170")
            CameraKey(num(o, "t", "camera[].t"), vec(o["pos"], "camera[].pos"), vec(o["look"], "camera[].look"), fov)
        }.sortedBy { it.t }
        val words = list(m, "words", 256).mapIndexed { i, w ->
            val o = obj(w, "words[$i]")
            val text = o["text"] as? String ?: throw ProtocolError("words[].text: a string")
            Word(SceneInput.cleanWord(text), num(o, "t", "words[].t"))
        }
        val lights = list(m, "lights", 32).mapIndexed { i, l ->
            val o = obj(l, "lights[$i]")
            val c = vec(o["color"], "lights[].color")
            if (listOf(c.x, c.y, c.z).any { it < 0 || it > 64 }) throw ProtocolError("lights[].color: components 0 to 64")
            Light(vec(o["pos"], "lights[].pos"), c, num(o, "t", "lights[].t"))
        }
        val moves = list(m, "moves", 256).mapIndexed { i, x ->
            val o = obj(x, "moves[$i]")
            val kind = MoveKind.values().firstOrNull { it.wire == o["move"] } ?: throw ProtocolError("moves[].move: dance or wave")
            Move(num(o, "t", "moves[].t"), kind)
        }.sortedBy { it.t }
        val chrome = list(m, "chrome", 64).mapIndexed { i, x ->
            val o = obj(x, "chrome[$i]")
            ChromeKey(num(o, "t", "chrome[].t"), o["on"] as? Boolean ?: throw ProtocolError("chrome[].on: a boolean"))
        }.sortedBy { it.t }
        val from = m["rect_from_frame"]?.let {
            val d = it as? Double ?: throw ProtocolError("rect_from_frame: a non-negative integer or null")
            if (d < 0 || d != StrictMath.rint(d)) throw ProtocolError("rect_from_frame: a non-negative integer or null")
            d.toLong()
        }
        val prev = m["previous_rect"]?.let { rect(it, "previous_rect") }
        if (prev != null && from == null) throw ProtocolError("previous_rect: needs rect_from_frame")
        return SceneInput(
            epochUnixMs = long(m, "epoch_unix_ms"),
            rect = rect(m["rect"], "rect"),
            imageAspect = aspect(m),
            camera = camera,
            words = words,
            lights = lights,
            moves = moves,
            chrome = chrome,
            night = m["night"]?.let { it as? Boolean ?: throw ProtocolError("night: a boolean") },
            stamp = m["stamp"]?.let { it as? Boolean ?: throw ProtocolError("stamp: a boolean") } ?: false,
            previousRect = prev,
            rectFromFrame = from,
        )
    }

    private fun aspect(m: Map<String, Any?>): Double {
        val a = num(m, "image_aspect")
        if (a < 0.1 || a > 10) throw ProtocolError("image_aspect: 0.1 to 10")
        return a
    }

    fun rect(v: Any?, name: String): Rect {
        val o = obj(v, name)
        val r = Rect(num(o, "x", "$name.x"), num(o, "y", "$name.y"), num(o, "w", "$name.w"), num(o, "h", "$name.h"))
        val inside = r.x >= 0 && r.y >= 0 && r.x + r.w <= 1 + 1e-9 && r.y + r.h <= 1 + 1e-9
        if (!inside || r.w < 1.0 / 256 || r.h < 1.0 / 256) throw ProtocolError("$name: inside the unit square, w and h at least 1/256")
        return r
    }

    private fun obj(v: Any?, name: String): Map<String, Any?> {
        @Suppress("UNCHECKED_CAST")
        return v as? Map<String, Any?> ?: throw ProtocolError("$name: an object")
    }

    private fun list(m: Map<String, Any?>, key: String, max: Int): List<Any?> {
        val v = m[key] ?: return emptyList()
        val l = v as? List<*> ?: throw ProtocolError("$key: a list")
        if (l.size > max) throw ProtocolError("$key: at most $max entries")
        return l
    }

    private fun num(m: Map<String, Any?>, key: String, name: String = key): Double {
        val v = m[key] as? Double ?: throw ProtocolError("$name: a number")
        if (!v.isFinite()) throw ProtocolError("$name: a finite number")
        return v
    }

    private fun optNum(m: Map<String, Any?>, key: String): Double? = if (m[key] == null) null else num(m, key)

    private fun long(m: Map<String, Any?>, key: String): Long {
        val v = num(m, key)
        if (v != StrictMath.rint(v) || StrictMath.abs(v) > 9.0e15) throw ProtocolError("$key: an integer")
        return v.toLong()
    }

    private fun vec(v: Any?, name: String): Vec3 {
        val l = v as? List<*> ?: throw ProtocolError("$name: [x, y, z]")
        if (l.size != 3 || l.any { it !is Double || !it.isFinite() }) throw ProtocolError("$name: [x, y, z]")
        return Vec3(l[0] as Double, l[1] as Double, l[2] as Double)
    }
}
