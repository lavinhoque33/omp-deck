package io.github.lavinhoque33.ompdeck.bridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EventGateTest {
    @Test
    fun anInitialOpenWaitsForTheHandshakeAndIsNotLost() {
        val gate = EventGate()
        assertFalse(gate.deliver("/#s=1"))
        assertFalse(gate.deliver("/#s=2"))
        assertEquals(listOf("/#s=1", "/#s=2"), gate.markReady())
        assertTrue(gate.isReady)
        assertTrue("after the handshake opens go straight out", gate.deliver("/#s=3"))
        assertEquals("nothing is replayed twice", emptyList<String>(), gate.markReady())
    }

    @Test
    fun aNewDocumentStartsANewHandshake() {
        val gate = EventGate()
        gate.markReady()
        gate.reset()
        assertFalse(gate.isReady)
        assertFalse(gate.deliver("/#s=1"))
        assertEquals(listOf("/#s=1"), gate.markReady())
    }

    @Test
    fun theQueueIsBoundedAndDropsTheOldest() {
        val gate = EventGate(maxQueued = 3)
        for (i in 1..5) gate.deliver("/#s=$i")
        assertEquals(listOf("/#s=3", "/#s=4", "/#s=5"), gate.markReady())
    }
}
