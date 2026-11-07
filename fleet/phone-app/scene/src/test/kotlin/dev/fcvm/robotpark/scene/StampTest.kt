package dev.fcvm.robotpark.scene

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class StampTest {
    /** The held bit is the top of the 32 the streamer reads; the frame number keeps the rest. */
    @Test
    fun aHeldFrameCarriesTheTopBitAndItsFrameNumber() {
        assertEquals(76_139_400L, Stamp.value(76_139_400L, heldLayout = false))
        assertEquals(0x8000_0000L or 76_139_400L, Stamp.value(76_139_400L, heldLayout = true))
        // a frame number past 31 bits does not reach the held bit
        assertEquals(5L, Stamp.value((1L shl 31) or 5L, heldLayout = false))
    }

    /** The bit is set from the first held frame and stays set while the bars come down. */
    @Test
    fun theHeldBitOutlastsTheHoldByTheTail() {
        val h = HeldLayout()
        assertFalse(h.next(held = false, barsLifted = false))
        assertTrue(h.next(held = true, barsLifted = false))
        assertTrue(h.next(held = true, barsLifted = true))
        repeat(Stamp.HELD_TAIL_FRAMES) { assertTrue("tail frame $it", h.next(held = false, barsLifted = false)) }
        assertFalse(h.next(held = false, barsLifted = false))
    }

    /**
     * The app lowers the bars in [LiftedBars] only from a draw callback, after the lowered
     * layout has been drawn (a layout change waits for the next traversal), and gives the GL
     * thread nothing but its state: a source check of MainActivity, which has no JVM test.
     */
    @Test
    fun theAppClearsTheLiftedFlagOnlyAfterTheLoweredLayoutIsDrawn() {
        val src = java.io.File(System.getProperty("appSrcDir"), "MainActivity.kt").readLines()
        val drawn = src.indices.filter { src[it].contains("bars.lowerDrawn(") }
        assertEquals("one place reports a drawn lowering", 1, drawn.size)
        val before = src.subList(maxOf(0, drawn[0] - 6), drawn[0]).joinToString("\n")
        assertTrue("reported inside onDraw's posted callback: $before", before.contains("override fun onDraw()") && before.contains("root.post"))
        val sets = src.filter { it.contains("gl.barsLifted =") }.map { it.substringAfter("gl.barsLifted =").trim() }
        assertTrue("the GL thread's flag is only ever the bars' state: $sets", sets.isNotEmpty() && sets.all { it == "bars.lifted" })
    }

    /**
     * A release, a hold and a second release before the first release's draw callback runs:
     * that callback is stale and leaves the bars lifted until the second lowering is drawn.
     */
    @Test
    fun aStaleLoweringDoesNotClearALaterOne() {
        val b = LiftedBars()
        assertNull(b.lowerRequested())
        b.lift()
        val first = b.lowerRequested()!!
        assertNull("one lowering pending at a time", b.lowerRequested())
        b.lift()
        val second = b.lowerRequested()!!
        b.lowerDrawn(first)
        assertTrue("the first lowering's callback is stale", b.lifted)
        b.lowerDrawn(second)
        assertFalse(b.lifted)
        b.lift()
        b.lowerDrawn(second)
        assertTrue("a lift after a lowering is not undone by its callback", b.lifted)
    }

    /** Given back, the bit stays set for as long as the UI has not lowered the bars, then the tail. */
    @Test
    fun theHeldBitStaysWhileTheBarsAreStillLifted() {
        val h = HeldLayout()
        assertTrue(h.next(held = true, barsLifted = true))
        repeat(10 * Stamp.HELD_TAIL_FRAMES) { assertTrue("lifted frame $it", h.next(held = false, barsLifted = true)) }
        repeat(Stamp.HELD_TAIL_FRAMES) { assertTrue("tail frame $it", h.next(held = false, barsLifted = false)) }
        assertFalse(h.next(held = false, barsLifted = false))
    }
}
