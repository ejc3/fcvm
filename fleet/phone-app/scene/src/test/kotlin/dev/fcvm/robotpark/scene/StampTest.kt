package dev.fcvm.robotpark.scene

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
        assertFalse(h.next(false))
        assertTrue(h.next(true))
        assertTrue(h.next(true))
        repeat(Stamp.HELD_TAIL_FRAMES) { assertTrue("tail frame $it", h.next(false)) }
        assertFalse(h.next(false))
    }
}
