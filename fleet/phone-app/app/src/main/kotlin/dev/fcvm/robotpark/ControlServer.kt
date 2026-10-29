package dev.fcvm.robotpark

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.util.Log
import dev.fcvm.robotpark.scene.Json
import dev.fcvm.robotpark.scene.Protocol
import dev.fcvm.robotpark.scene.ProtocolError
import java.io.IOException
import java.io.InputStream

/**
 * The control socket: an abstract-namespace Unix socket named [Protocol.SOCKET_NAME], reached
 * from the phone container with `adb forward tcp:<port> localabstract:dev.fcvm.robotpark`.
 * One JSON request per line, one JSON reply per line, in order (fleet/PROTOCOL.md, "Phone app").
 */
class ControlServer(private val handle: (dev.fcvm.robotpark.scene.Command) -> Map<String, Any?>) : Thread("robotpark-control") {
    init { isDaemon = true }

    override fun run() {
        val server = try {
            LocalServerSocket(Protocol.SOCKET_NAME)
        } catch (e: IOException) {
            Log.e(TAG, "control socket: ${e.message}")
            Store.lastError = "control socket: ${e.message}"
            return
        }
        Log.i(TAG, "control socket listening on @${Protocol.SOCKET_NAME}")
        while (true) {
            val s = try { server.accept() } catch (e: IOException) { Log.e(TAG, "accept: ${e.message}"); continue }
            Thread({ serve(s) }, "robotpark-client").apply { isDaemon = true }.start()
        }
    }

    private fun serve(s: LocalSocket) {
        s.use {
            val input = s.inputStream
            val out = s.outputStream
            while (true) {
                val line = readLine(input) ?: return
                if (line.isBlank()) continue
                val reply: Map<String, Any?> = if (line.length > Protocol.MAX_LINE) {
                    mapOf("error" to "line longer than ${Protocol.MAX_LINE} bytes")
                } else try {
                    handle(Protocol.parse(line))
                } catch (e: ProtocolError) {
                    mapOf("error" to e.message)
                } catch (e: Exception) {
                    Log.e(TAG, "request failed", e)
                    mapOf("error" to "${e.javaClass.simpleName}: ${e.message}")
                }
                out.write((Json.write(reply) + "\n").toByteArray(Charsets.UTF_8))
                out.flush()
                if (line.length > Protocol.MAX_LINE) return
            }
        }
    }

    /** A UTF-8 line without its newline; null at end of stream. Stops reading past MAX_LINE. */
    private fun readLine(input: InputStream): String? {
        val buf = java.io.ByteArrayOutputStream()
        while (true) {
            val b = input.read()
            if (b < 0) return if (buf.size() == 0) null else buf.toString("UTF-8")
            if (b == '\n'.code) return buf.toString("UTF-8")
            if (buf.size() > Protocol.MAX_LINE) {
                // drain the rest of the line, then refuse it
                while (true) { val c = input.read(); if (c < 0 || c == '\n'.code) break }
                return "x".repeat(Protocol.MAX_LINE + 1)
            }
            buf.write(b)
        }
    }

    companion object { const val TAG = "ROBOTPARK" }
}
