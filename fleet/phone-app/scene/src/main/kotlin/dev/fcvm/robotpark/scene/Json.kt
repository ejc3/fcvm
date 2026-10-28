package dev.fcvm.robotpark.scene

/**
 * A small JSON reader and writer for the control protocol, so the scene module needs no
 * library and its tests run on any JVM. Objects read as [Map] (insertion order kept), arrays
 * as [List], numbers as [Double], plus [String], [Boolean] and null.
 */
object Json {
    class Error(message: String) : Exception(message)

    fun parse(text: String): Any? {
        val p = Parser(text)
        p.ws()
        val v = p.value(0)
        p.ws()
        if (p.i != text.length) throw Error("trailing characters at ${p.i}")
        return v
    }

    private class Parser(val s: String) {
        var i = 0
        fun ws() {
            while (i < s.length && (s[i] == ' ' || s[i] == '\t' || s[i] == '\n' || s[i] == '\r')) i++
        }
        fun value(depth: Int): Any? {
            if (depth > 32) throw Error("nested too deep")
            if (i >= s.length) throw Error("unexpected end")
            return when (val c = s[i]) {
                '{' -> obj(depth)
                '[' -> arr(depth)
                '"' -> str()
                't' -> lit("true", true)
                'f' -> lit("false", false)
                'n' -> lit("null", null)
                else -> if (c == '-' || c in '0'..'9') num() else throw Error("unexpected '$c' at $i")
            }
        }
        fun lit(word: String, v: Any?): Any? {
            if (!s.startsWith(word, i)) throw Error("bad literal at $i")
            i += word.length
            return v
        }
        fun num(): Double {
            val start = i
            if (s[i] == '-') i++
            while (i < s.length && (s[i] in '0'..'9' || s[i] == '.' || s[i] == 'e' || s[i] == 'E' || s[i] == '+' || s[i] == '-')) i++
            return s.substring(start, i).toDoubleOrNull() ?: throw Error("bad number at $start")
        }
        fun str(): String {
            i++
            val b = StringBuilder()
            while (true) {
                if (i >= s.length) throw Error("unterminated string")
                val c = s[i++]
                when {
                    c == '"' -> return b.toString()
                    c == '\\' -> {
                        if (i >= s.length) throw Error("unterminated string")
                        when (val e = s[i++]) {
                            '"', '\\', '/' -> b.append(e)
                            'b' -> b.append('\b')
                            'f' -> b.append('\u000c')
                            'n' -> b.append('\n')
                            'r' -> b.append('\r')
                            't' -> b.append('\t')
                            'u' -> {
                                if (i + 4 > s.length) throw Error("bad escape")
                                b.append(s.substring(i, i + 4).toIntOrNull(16)?.toChar() ?: throw Error("bad escape"))
                                i += 4
                            }
                            else -> throw Error("bad escape \\$e")
                        }
                    }
                    c < ' ' -> throw Error("control character in string")
                    else -> b.append(c)
                }
            }
        }
        fun obj(depth: Int): Map<String, Any?> {
            i++
            val m = LinkedHashMap<String, Any?>()
            ws()
            if (i < s.length && s[i] == '}') { i++; return m }
            while (true) {
                ws()
                if (i >= s.length || s[i] != '"') throw Error("expected a key at $i")
                val k = str()
                ws()
                if (i >= s.length || s[i] != ':') throw Error("expected ':' at $i")
                i++
                ws()
                m[k] = value(depth + 1)
                ws()
                if (i >= s.length) throw Error("unterminated object")
                if (s[i] == ',') { i++; continue }
                if (s[i] == '}') { i++; return m }
                throw Error("expected ',' or '}' at $i")
            }
        }
        fun arr(depth: Int): List<Any?> {
            i++
            val l = ArrayList<Any?>()
            ws()
            if (i < s.length && s[i] == ']') { i++; return l }
            while (true) {
                ws()
                l.add(value(depth + 1))
                ws()
                if (i >= s.length) throw Error("unterminated array")
                if (s[i] == ',') { i++; continue }
                if (s[i] == ']') { i++; return l }
                throw Error("expected ',' or ']' at $i")
            }
        }
    }

    fun write(v: Any?): String = StringBuilder().also { write(it, v) }.toString()

    private fun write(b: StringBuilder, v: Any?) {
        when (v) {
            null -> b.append("null")
            is Boolean -> b.append(v)
            is Int, is Long -> b.append(v)
            is Float -> write(b, v.toDouble())
            is Double -> if (v.isFinite() && v == Math.rint(v) && Math.abs(v) < 1e15) b.append(v.toLong()) else if (v.isFinite()) b.append(v) else b.append("null")
            is String -> {
                b.append('"')
                for (c in v) when {
                    c == '"' -> b.append("\\\"")
                    c == '\\' -> b.append("\\\\")
                    c == '\n' -> b.append("\\n")
                    c < ' ' -> b.append(String.format("\\u%04x", c.code))
                    else -> b.append(c)
                }
                b.append('"')
            }
            is Map<*, *> -> {
                b.append('{')
                var first = true
                for ((k, x) in v) {
                    if (!first) b.append(',')
                    first = false
                    write(b, k.toString()); b.append(':'); write(b, x)
                }
                b.append('}')
            }
            is Iterable<*> -> {
                b.append('[')
                var first = true
                for (x in v) { if (!first) b.append(','); first = false; write(b, x) }
                b.append(']')
            }
            is FloatArray -> write(b, v.toList())
            is IntArray -> write(b, v.toList())
            else -> write(b, v.toString())
        }
    }
}
