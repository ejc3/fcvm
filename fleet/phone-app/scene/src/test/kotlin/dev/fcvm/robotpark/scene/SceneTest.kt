package dev.fcvm.robotpark.scene

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.File
import java.util.concurrent.TimeUnit

class SceneTest {
    private val scene = Protocol.parseScene(
        Json.parse(
            """{"epoch_unix_ms": 1761100000000, "rect": {"x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0}, "image_aspect": 1.125,
               "camera": [{"t": 0, "pos": [0, 8, 30], "look": [0, 1.5, -4], "fov_deg": 40},
                          {"t": 20, "pos": [6, 9, 28], "look": [2, 1.5, -6], "fov_deg": 44},
                          {"t": 30, "pos": [6, 9, 28], "look": [2, 1.5, -6], "fov_deg": 44},
                          {"t": 50, "pos": [-4, 7, 31], "look": [0, 1.2, -3], "fov_deg": 38}],
               "words": [{"text": "HELLO PHONES", "t": 3}, {"text": "seams hold", "t": 9}],
               "lights": [{"pos": [2, 1.5, 4], "color": [1, 0.4, 0.1], "t": 1}],
               "moves": [{"t": 12, "move": "dance"}, {"t": 40, "move": "wave"}],
               "chrome": [{"t": 45, "on": true}]}""",
        ) as Map<String, Any?>,
    )

    private fun frame(s: SceneInput, t: Double) = FrameState.compute(s, t, 720.0 / 1280.0, nightDefault = false)

    /** The same inputs and t give the same bits, whatever was computed before. */
    @Test fun frameIsAPureFunctionOfInputsAndTime() {
        val times = listOf(0.0, 2.5, 12.3, 14.0, 19.99, 25.0, 41.0, 47.5, 60.0, 3600.25, 86400.5)
        val fresh = times.map { frame(scene, it).bits() }
        // in reverse, interleaved with other times and another scene, as a restored phone would
        val other = scene.copy(words = emptyList(), night = true)
        for ((i, t) in times.withIndex().reversed()) {
            frame(other, t * 0.37)
            frame(scene, t + 100)
            assertArrayEquals("t=$t", fresh[i], frame(scene, t).bits())
        }
    }

    /** Neighbours draw with identical uniforms: the rect moves only the viewport. */
    @Test fun frameDoesNotDependOnRect() {
        val left = scene.copy(rect = Rect(0.0, 0.0, 0.5, 1.0))
        for (t in listOf(0.0, 13.0, 44.0)) assertArrayEquals(frame(scene, t).bits(), frame(left, t).bits())
    }

    /** A frame drawn at a large t (hours after the epoch) still animates smoothly on floats. */
    @Test fun wrappedClocksStaySmall() {
        val f = frame(scene, 1.0e7 + 0.5)
        assertTrue(f.tw in 0f..1200f && f.walkT in 0f..1200f)
        val g = frame(scene, 1.0e7 + 0.5 + 1.0 / 60)
        assertEquals(1.0 / 60, (g.tw - f.tw).toDouble(), 1e-3)
    }

    @Test fun tilesOfAGridMeetWithoutGapsOnWholeImagePixels() {
        for (n in 1..9) {
            val aspect = n * 720.0 / 1280.0
            var x = 0
            for (k in 0 until n) {
                val tile = Tile.of(Rect(k.toDouble() / n, 0.0, 1.0 / n, 1.0), aspect, 720, 1280)
                assertEquals("n=$n k=$k", 1280, tile.fullH)
                assertEquals(StrictMath.round(1280 * aspect).toInt(), tile.fullW)
                assertTrue("n=$n k=$k: tile starts at ${tile.x0}, previous ends at $x", tile.x0 <= x && tile.x0 >= x - 1)
                x = tile.x0 + tile.w
                assertEquals(0, tile.y0)
            }
            assertTrue(x >= StrictMath.round(1280 * aspect))
        }
        // a 2 x 2 grid
        val t = Tile.of(Rect(0.5, 0.5, 0.5, 0.5), 9.0 / 16, 720, 1280)
        assertEquals(Tile(1440, 2560, 720, 1280, 720, 1280), t)
    }

    @Test fun crowdEntriesDoNotDependOnTheCount() {
        val small = Crowd.robots(100); val big = Crowd.robots(Crowd.ROBOTS)
        assertArrayEquals(small, big.copyOf(small.size), 0f)
        assertArrayEquals(Crowd.robots(Crowd.ROBOTS), big, 0f)
        val kinds = (0 until Crowd.ROBOTS).map { Crowd.robot(it)[6].toInt() }.groupingBy { it }.eachCount()
        assertTrue(kinds.keys == setOf(Crowd.KIND_WALKER, Crowd.KIND_HOPPER, Crowd.KIND_LEAPER))
        for (i in 0 until Crowd.ROBOTS) {
            val r = Crowd.robot(i)
            assertEquals("whole laps per period", r[2].toDouble(), StrictMath.rint(r[2].toDouble()), 0.0)
            assertEquals("whole cycles per period", r[3].toDouble(), StrictMath.rint(r[3].toDouble()), 0.0)
            assertTrue(r[2] != 0f && r[3] > 0f)
        }
    }

    @Test fun movesFreezeWalkingContinuously() {
        val w = FrameState.merge(listOf(10.0, 15.0, 40.0))
        assertEquals(listOf(10.0 to 23.0, 40.0 to 48.0), w)
        var prev = -1.0
        var t = 0.0
        while (t < 60) {
            val walk = t - FrameState.frozen(w, t)
            assertTrue("walking time never runs backwards at $t", walk >= prev - 1e-12)
            if (prev >= 0) assertTrue("walking time never jumps at $t", walk - prev <= 0.01 + 1e-9)
            prev = walk; t += 0.01
        }
        // a whole window less its two half ramps is frozen
        assertEquals(13.0 - FrameState.MOVE_RAMP, FrameState.frozen(w, 30.0), 1e-9)
        assertEquals(0.0, FrameState.weight(w, 9.99), 0.0)
        assertEquals(1.0, FrameState.weight(w, 16.0), 0.0)
    }

    /**
     * Pruning the phone's own old moves leaves every frame from the end of the pruned windows
     * on bit-identical, and refuses frames before it.
     */
    @Test fun pruningLocalMovesKeepsLaterFramesAndRefusesEarlierOnes() {
        val bare = scene.copy(moves = emptyList())
        val moves = (0 until 100).map { Move(10.0 + 20.0 * it, if (it % 2 == 0) MoveKind.DANCE else MoveKind.WAVE) }
        var pruned = LocalMoves(bare.epochUnixMs)
        var kept = LocalMoves(bare.epochUnixMs)
        for (m in moves) { pruned = pruned.add(m, bare, 2010.0); kept = kept.add(m, bare, 2010.0, max = 1000) }
        assertEquals(64, pruned.moves.size)
        for (t in listOf(pruned.from, 2010.0, 2013.0, 2040.5)) {
            assertArrayEquals("t=$t", frame(kept.applyTo(bare, null), t).bits(), frame(pruned.applyTo(bare, null), t).bits())
        }
        try { frame(pruned.applyTo(bare, null), 5.0); fail("drew t=5 after the moves before it were pruned") } catch (e: IllegalArgumentException) { /* refused */ }
        // a window still running is kept
        assertEquals(66, FrameState.pruneMoves(moves.take(66), emptyList(), 15.0, 64).first.size)
    }

    /** A dance late in a window merged with other moves keeps its confetti through pruning. */
    @Test fun pruningWaitsForTheConfettiOfADanceLateInAWindow() {
        val withWave = scene.copy(moves = listOf(Move(0.0, MoveKind.WAVE)))
        val local = listOf(Move(7.0, MoveKind.DANCE)) + (0 until 64).map { Move(100.0 + 20.0 * it, MoveKind.WAVE) }
        var pruned = LocalMoves(withWave.epochUnixMs)
        var kept = LocalMoves(withWave.epochUnixMs)
        for (m in local) { pruned = pruned.add(m, withWave, 16.0); kept = kept.add(m, withWave, 16.0, max = 1000) }
        for (t in listOf(16.0, 17.0)) {
            assertArrayEquals("t=$t", frame(kept.applyTo(withWave, null), t).bits(), frame(pruned.applyTo(withWave, null), t).bits())
        }
    }

    /** The phone's own moves set the pose and never the walk; the scene's moves stop it. */
    @Test fun onlyTheScenesMovesStopTheCrowd() {
        val bare = scene.copy(moves = emptyList())
        val danced = LocalMoves(bare.epochUnixMs).add(Move(100.0, MoveKind.DANCE), bare, 100.0).applyTo(bare, null)
        assertEquals(frame(bare, 104.0).walkT, frame(danced, 104.0).walkT)
        assertEquals(1f, frame(danced, 104.0).dance)
        assertEquals(0f, frame(danced, 104.0).still)
        val stopped = bare.copy(moves = listOf(Move(100.0, MoveKind.DANCE)))
        assertEquals(1f, frame(stopped, 104.0).still)
        assertTrue(frame(stopped, 104.0).walkT < frame(bare, 104.0).walkT)
    }

    /**
     * Pruning the phone's own moves changes no frame after the pruned ones ended, whatever scene
     * arrives later: here a scene that adds a move over a pruned window (the case that counted
     * that window's walking time twice when local moves froze the walk).
     */
    @Test fun prunedLocalMovesChangeNoLaterFrame() {
        val bare = scene.copy(moves = emptyList())
        val moves = (0 until 65).map { Move(10.0 + 20.0 * it, MoveKind.DANCE) }
        var local = LocalMoves(bare.epochUnixMs)
        var kept = LocalMoves(bare.epochUnixMs)
        for (m in moves) { local = local.add(m, bare, 2000.0); kept = kept.add(m, bare, 2000.0, max = 1000) }
        assertEquals("one was pruned", 64, local.moves.size)
        val later = bare.copy(moves = listOf(Move(10.0, MoveKind.WAVE)))
        val full = kept.applyTo(later, later.night)
        val pruned = local.applyTo(later, later.night)
        for (t in listOf(1300.0, 1999.0, 2000.0, 2030.5)) {
            assertEquals("walking clock at t=$t", frame(full, t).walkT, frame(pruned, t).walkT)
        }
    }

    /** A scene with a new epoch restarts t, so the phone's own moves (in the old clock) are cleared. */
    @Test fun aNewEpochClearsThePhonesOwnMoves() {
        val before = SceneInput() // the app's default before any scene: epoch 0
        var local = LocalMoves()
        for (i in 0 until 100) local = local.add(Move(1.7e9 + 20.0 * i, MoveKind.DANCE), before, 1.7e9 + 2010.0)
        assertTrue("some were pruned", local.from > 1.7e9)
        val s = local.applyTo(scene, scene.night)
        assertEquals(scene.moves, s.moves)
        assertEquals(emptyList<Move>(), s.localMoves)
        assertEquals(12.0, frame(s, 12.0).t, 0.0) // draws: the old clock's boundary does not apply
        // the same epoch keeps them
        assertEquals(64, local.applyTo(before, null).localMoves.size)
    }

    /** An action timed before the pruned history is refused: its window may already be counted. */
    @Test fun anActionBeforeThePrunedHistoryIsRefused() {
        val bare = scene.copy(moves = emptyList())
        var local = LocalMoves(bare.epochUnixMs)
        for (i in 0 until 65) local = local.add(Move(10.0 + 20.0 * i, MoveKind.DANCE), bare, 2000.0)
        assertTrue("the first move was pruned", local.from > 10.0)
        try {
            local = local.add(Move(10.0, MoveKind.DANCE), bare, 2000.0)
            fail("took a move at t=10 behind the pruned history (now ${local.moves.size} moves)")
        } catch (e: IllegalArgumentException) { /* refused */ }
    }

    @Test fun cameraHoldsAreStillAndPathsAreContinuous() {
        val k = scene.camera
        assertEquals(CameraPath.at(k, 21.0), CameraPath.at(k, 29.0))
        var prev = CameraPath.at(k, 0.0)
        var t = 0.0
        while (t < 60) {
            t += 0.05
            val p = CameraPath.at(k, t)
            assertTrue("camera jumps at $t", (p.pos - prev.pos).length() < 0.2)
            prev = p
        }
        assertEquals(k.last().pos, CameraPath.at(k, 1e6).pos)
    }

    @Test fun signsCrossTheWholeImage() {
        fun span(t: Double): Pair<Double, Double> {
            val c = CameraPath.at(scene.camera, t)
            val vp = Mat.mul(Mat.perspective(c.fovDeg, 1.125, FrameState.NEAR, FrameState.FAR), Mat.lookAt(c.pos, c.look, Vec3(0.0, 1.0, 0.0)))
            return FrameState.spanAt(vp, c.pos, 9.0)
        }
        val t1 = 3.0 + FrameState.SIGN_SECS - 0.01
        val s0 = frame(scene, 3.0).signs.single(); val s1 = frame(scene, t1).signs.first { it.text == "HELLO PHONES" }
        assertTrue("enters left of the image", s0.x < span(3.0).first)
        assertTrue("leaves right of the image", s1.x > span(t1).second)
        assertTrue(frame(scene, 3.0 + FrameState.SIGN_SECS + 1).signs.none { it.text == "HELLO PHONES" })
    }

    @Test fun protocolTakesOnlyTheFixedActions() {
        for (a in Action.values()) {
            val c = Protocol.parse("""{"type":"action","name":"${a.wire}"}""")
            assertEquals(a, (c as Command.Act).action)
        }
        for (bad in listOf("""{"type":"action","name":"shell"}""", """{"type":"action","name":"am start x"}""", """{"type":"action"}""",
            """{"type":"exec","cmd":"ls"}""", """not json""", """[1]""")) {
            try { Protocol.parse(bad); fail("accepted $bad") } catch (e: ProtocolError) { /* refused */ }
        }
        assertEquals(12.5, (Protocol.parse("""{"type":"action","name":"dance","t":12.5}""") as Command.Act).t)
    }

    @Test fun protocolValidatesScenesLikeTheRenderer() {
        fun err(body: String): String = try { Protocol.parse(body); "" } catch (e: ProtocolError) { e.message ?: "" }
        val ok = """"epoch_unix_ms":1,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":1.5"""
        assertEquals("", err("""{"type":"scene",$ok}"""))
        assertTrue(err("""{"type":"scene","epoch_unix_ms":1,"rect":{"x":0.9,"y":0,"w":0.5,"h":1},"image_aspect":1.5}""").startsWith("rect:"))
        assertTrue(err("""{"type":"scene","epoch_unix_ms":1,"rect":{"x":0,"y":0,"w":1,"h":1},"image_aspect":40}""").startsWith("image_aspect:"))
        assertTrue(err("""{"type":"scene",$ok,"moves":[{"t":1,"move":"jump"}]}""").startsWith("moves[].move:"))
        assertTrue(err("""{"type":"scene",$ok,"camera":[{"t":0,"pos":[0,1],"look":[0,0,0],"fov_deg":40}]}""").startsWith("camera[].pos:"))
        assertTrue(err("""{"type":"scene",$ok,"previous_rect":{"x":0,"y":0,"w":1,"h":1}}""").startsWith("previous_rect:"))
        // the renderer's fields the phone does not use are accepted and ignored
        assertEquals("", err("""{"type":"scene",$ok,"time_of_day":0.3,"weather":[],"budget":1000000,"layout_generation":3}"""))
        val s = (Protocol.parse("""{"type":"scene",$ok,"words":[{"text":"héllo\u0001 world, a very long word indeed","t":2}]}""") as Command.SetScene).scene
        assertEquals("hllo? world, a very long wor", s.words.single().text)
    }

    @Test fun aRotatedPhoneDrawsTheWholeImageOnItsOwn() {
        assertEquals(scene.rect to 1.125, scene.viewAt(5.0, 720, 1280))
        assertEquals(Rect(0.0, 0.0, 1.0, 1.0) to 1280.0 / 720, scene.viewAt(5.0, 1280, 720))
        assertEquals(Rect(0.0, 0.0, 1.0, 1.0) to 0.5625, SceneInput().viewAt(0.0, 720, 1280))
    }

    @Test fun layoutSwitchUsesThePreviousRectBeforeItsFrame() {
        val s = scene.copy(previousRect = Rect(0.0, 0.0, 0.5, 1.0), rectFromFrame = 300)
        assertEquals(Rect(0.0, 0.0, 0.5, 1.0), s.rectAt(9.99))
        assertEquals(scene.rect, s.rectAt(10.0))
    }

    @Test fun meshesFitSixteenBitIndices() {
        for ((name, m) in listOf("robot" to Meshes.robot(), "buddy" to Meshes.buddy(), "round" to Meshes.roundTree(), "pine" to Meshes.pineTree(),
            "flower" to Meshes.flower(), "ball" to Meshes.ball(), "sign" to Meshes.sign(), "ground" to Meshes.ground())) {
            assertTrue(name, m.vertexCount in 4..65535)
            assertTrue(name, m.indices.all { (it.toInt() and 0xffff) < m.vertexCount })
            assertTrue(name, m.vertices.all { it.isFinite() })
        }
        assertArrayEquals(Meshes.robot().vertices, Meshes.robot().vertices, 0f)
        assertNotEquals(0, Meshes.robot().indices.size)
    }

    @Test fun jsonRoundTrips() {
        val v = Json.parse("""{"a":[1,2.5,-3e2,true,false,null],"b":"x\"y\\z\n","c":{}}""")
        assertEquals(v, Json.parse(Json.write(v)))
        assertFalse(Json.write(mapOf("n" to 2.0)).contains("2.0"))
    }

    private fun appSource(name: String) = File(System.getProperty("appSrcDir") ?: error("appSrcDir is not set"), name).readText()

    /**
     * A frame's time comes from the same scene snapshot as the frame: reading Store.scene again
     * for the epoch could pair one scene's inputs with another's clock when a scene arrives
     * between the two reads. Pinned on the source, since the interleaving needs Android.
     */
    @Test fun frameTimeComesFromTheFramesOwnSceneSnapshot() {
        assertTrue("Store.frameTime takes the snapshot", Regex("""fun frameTime\(scene: SceneInput\)""").containsMatchIn(appSource("Store.kt")))
        for (f in listOf("GlThread.kt", "PhoneApp.kt", "MainActivity.kt", "SceneRenderer.kt")) {
            assertFalse("$f reads the time without a snapshot", appSource(f).contains("frameTime()"))
        }
    }

    /**
     * A local action's default time and the epoch its move is filed under come from one scene
     * snapshot, inside Store.addMove: computed in PhoneApp from one read and filed by addMove
     * under another, a scene arriving in between put the move in the wrong clock.
     */
    @Test fun aLocalActionIsTimedAndFiledInOneSceneSnapshot() {
        assertTrue("addMove takes the optional time and returns the one it used",
            Regex("""fun addMove\(kind: MoveKind, t: Double\?\): Double""").containsMatchIn(appSource("Store.kt")))
        assertFalse("PhoneApp computes no frame time of its own", appSource("PhoneApp.kt").contains("frameTime("))
    }

    /**
     * Every EGL setup step after the context exists (the pbuffer, making it current, building
     * the renderer) tears the context down when it fails, so the next pass builds it again;
     * a context left with no renderer drew nothing until the app restarted.
     */
    @Test fun aContextWhoseSetupFailsIsTornDown() {
        val src = appSource("GlThread.kt")
        val body = src.substring(src.indexOf("private fun ensureContext()"), src.indexOf("private fun rebind("))
        val created = body.indexOf("eglCreateContext(")
        val tryAt = body.indexOf("try {", created)
        val catchAt = body.indexOf("catch (e: RuntimeException)", tryAt)
        assertTrue("the setup after eglCreateContext runs in a try", created >= 0 && tryAt > created)
        assertTrue("a failed setup step tears the context down", catchAt > tryAt && body.indexOf("teardown(", catchAt) > catchAt)
        assertTrue("the pbuffer is checked", body.contains("check(pbuffer != EGL14.EGL_NO_SURFACE)"))
        assertTrue("making it current is checked", body.contains("check(EGL14.eglMakeCurrent(dpy, pbuffer, pbuffer, ctx))"))
    }

    /**
     * A queued task (a capture, a context drop, the benchmark) leaves the queue only once a
     * context is current, so a context that cannot be made current delays it instead of
     * dropping it (a dropped benchmark left benchmarkRunning set for good).
     */
    @Test fun queuedTasksLeaveTheQueueOnlyWithACurrentContext() {
        val src = appSource("GlThread.kt")
        val loop = src.substring(src.indexOf("private fun loop()"), src.indexOf("private fun frame("))
        val take = loop.indexOf("tasks.removeFirstOrNull()")
        assertTrue("the loop takes tasks", take >= 0)
        assertTrue("a task is taken only after makeCurrent succeeds", loop.lastIndexOf("makeCurrent(", take) in 0 until take)
        assertEquals("the loop takes a task in one place", take, loop.lastIndexOf("tasks.removeFirstOrNull()"))
    }

    /**
     * A framebuffer's cached size is cleared before its storage is replaced and set only once
     * the framebuffer is complete: a failed allocation (out of GPU memory for a large image)
     * must not leave a size that makes the next frame at that size skip the rebuild and draw
     * into incomplete attachments.
     */
    @Test fun framebufferSizesArePublishedOnlyAfterTheirAllocationSucceeds() {
        val src = appSource("SceneRenderer.kt")
        for ((fn, v) in listOf("private fun ensureTargets(" to "msaaW", "fun drawOffscreen(" to "capW")) {
            val start = src.indexOf(fn)
            val body = src.substring(start, src.indexOf("\n    }\n", start))
            val clear = body.indexOf("$v = 0")
            val alloc = body.indexOf("RenderbufferStorage")
            val check = body.indexOf("checkFbo(")
            val publish = body.indexOf("$v = ", check)
            assertTrue("$fn clears $v before replacing storage", clear in 0 until alloc)
            assertTrue("$fn sets $v after checkFbo", check > alloc && publish > check)
        }
    }

    /** Every program the app links compiles as GLSL ES 3.00 (glslangValidator, in the build container). */
    @Test fun shadersCompile() {
        val dir = File(System.getProperty("shaderDir") ?: error("shaderDir is not set"))
        val out = File(System.getProperty("java.io.tmpdir"), "robotpark-shaders").apply { mkdirs() }
        for (p in Shaders.PROGRAMS) for ((stage, files) in listOf("vert" to p.vertex, "frag" to p.fragment)) {
            val f = File(out, "${p.name}.$stage")
            f.writeText(Shaders.source(files, p.defines) { File(dir, it).readText() })
            val proc = try {
                ProcessBuilder("glslangValidator", f.path).redirectErrorStream(true).start()
            } catch (e: java.io.IOException) {
                fail("glslangValidator is not installed (run the tests in the build container: make -C fleet/phone-app test)"); return
            }
            val text = proc.inputStream.bufferedReader().readText()
            assertTrue(proc.waitFor(60, TimeUnit.SECONDS))
            assertEquals("${p.name}.$stage:\n$text", 0, proc.exitValue())
        }
    }
}
