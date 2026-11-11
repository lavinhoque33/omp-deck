package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.File

class PushProcessorTest {
    private val now = 1_800_000_000_000L
    private var clock = now
    private lateinit var file: File
    private lateinit var store: PushStore
    private lateinit var presence: Presence
    private lateinit var keyA: ByteArray
    private lateinit var keyB: ByteArray
    private val shown = mutableListOf<Triple<PushNotification, String, String>>()
    private var sinkAnswer: String? = "tag"
    private lateinit var processor: PushProcessor

    @Before
    fun setUp() {
        file = File(tempDir(), "state.bin")
        store = PushStore(file, TestBox()) { clock }
        presence = Presence()
        keyA = store.getOrCreateKey(DESKTOP_A)!!
        keyB = store.getOrCreateKey(DESKTOP_B)!!
        processor = PushProcessor(store, presence, { n, title, nonce -> shown += Triple(n, title, nonce); sinkAnswer }, { clock })
    }

    private fun message(
        n: Int = 1,
        desktop: String = DESKTOP_A,
        key: ByteArray = keyA,
        expires: Long = now + PushPayload.TTL_MS,
        plain: String = Sender.payload(expires),
        claimed: String = desktop,
    ) = Sender.data(key, desktop, Sender.nonce(n), plain, claimed)

    private fun rejected(outcome: PushOutcome): RejectReason = (outcome as PushOutcome.Rejected).reason

    @Test
    fun decryptsAndShowsWithoutSecretsInTheTag() {
        val outcome = processor.process(message())
        assertEquals("tag", (outcome as PushOutcome.Shown).notificationTag)
        val (notification, title, _) = shown.single()
        assertEquals("OmpDeck", title)
        assertEquals("Session finished", notification.body)
        assertEquals("/#s=abc&d=$DESKTOP_A", notification.navigate)
        assertEquals(DESKTOP_A, notification.desktop)
    }

    @Test
    fun onlyAKnownDesktopIsAnswered() {
        val stranger = "ab".repeat(32)
        val data = Sender.data(ByteArray(32) { 9 }, stranger, Sender.nonce(5), Sender.payload(now + 1000))
        assertEquals(RejectReason.UNKNOWN_DESKTOP, rejected(processor.process(data)))
        assertTrue(shown.isEmpty())
    }

    @Test
    fun aCiphertextCannotBeMovedToAnotherDesktop() {
        // Encrypted for A under A's key, but the envelope names B.
        assertEquals(RejectReason.AUTH_FAILED, rejected(processor.process(message(desktop = DESKTOP_A, claimed = DESKTOP_B))))
        // Encrypted under B's key, envelope names A.
        assertEquals(RejectReason.AUTH_FAILED, rejected(processor.process(message(desktop = DESKTOP_B, key = keyB, claimed = DESKTOP_A))))
        // Encrypted with B's key for A's AAD: the wrong key.
        assertEquals(RejectReason.AUTH_FAILED, rejected(processor.process(message(desktop = DESKTOP_A, key = keyB))))
        assertTrue(shown.isEmpty())
    }

    @Test
    fun expiredAndImplausiblyFuturePushesAreDropped() {
        assertEquals(RejectReason.EXPIRED, rejected(processor.process(message(expires = now - PushPayload.SKEW_MS - 1))))
        assertEquals(
            RejectReason.TOO_FAR_IN_FUTURE,
            rejected(processor.process(message(n = 2, expires = now + PushPayload.TTL_MS + PushPayload.SKEW_MS + 1))),
        )
        assertTrue(shown.isEmpty())
        // Delivered late but still inside its window.
        clock = now + 30 * 60_000
        assertTrue(processor.process(message(n = 3)) is PushOutcome.Shown)
    }

    @Test
    fun aPushExpiringWhileQueuedIsDroppedWhenItFinallyArrives() {
        val data = message()
        clock = now + PushPayload.TTL_MS + PushPayload.SKEW_MS + 1
        assertEquals(RejectReason.EXPIRED, rejected(processor.process(data)))
    }

    @Test
    fun duplicateNonceIsAReplayEvenAfterARestart() {
        val data = message()
        assertTrue(processor.process(data) is PushOutcome.Shown)
        assertEquals(RejectReason.REPLAY, rejected(processor.process(data)))
        assertEquals(1, shown.size)

        // The process died: a fresh store on the same file still knows the nonce.
        val restarted = PushProcessor(PushStore(file, TestBox()) { clock }, presence, { n, t, nonce -> shown += Triple(n, t, nonce); "tag" }, { clock })
        assertEquals(RejectReason.REPLAY, rejected(restarted.process(data)))
        assertEquals(1, shown.size)
        // Another message (new nonce) is fine.
        assertTrue(restarted.process(message(n = 2)) is PushOutcome.Shown)
    }

    @Test
    fun sameNonceOnTwoDesktopsIsNotAReplay() {
        assertTrue(processor.process(message(n = 1)) is PushOutcome.Shown)
        assertTrue(processor.process(message(n = 1, desktop = DESKTOP_B, key = keyB)) is PushOutcome.Shown)
    }

    @Test
    fun aForgeryDoesNotBurnTheRealNonce() {
        val real = message()
        val forged = real.toMutableMap().also {
            val ct = Base64Url.decodeCanonical(it.getValue("ciphertext"), 5000)!!
            ct[0] = (ct[0].toInt() xor 1).toByte()
            it["ciphertext"] = Base64Url.encode(ct)
        }
        assertEquals(RejectReason.AUTH_FAILED, rejected(processor.process(forged)))
        assertTrue(processor.process(real) is PushOutcome.Shown)
    }

    @Test
    fun invalidAuthenticatedPayloadsAreDropped() {
        assertEquals(RejectReason.BAD_PAYLOAD, rejected(processor.process(message(plain = "{}"))))
        assertEquals(RejectReason.MALFORMED_ENVELOPE, rejected(processor.process(mapOf("v" to "1"))))
        assertEquals(RejectReason.MALFORMED_ENVELOPE, rejected(processor.process(message() + ("v" to "9"))))
        assertTrue(shown.isEmpty())
    }

    @Test
    fun forgettingADesktopStopsItsNotificationsOnly() {
        store.forget(DESKTOP_A)
        assertEquals(RejectReason.UNKNOWN_DESKTOP, rejected(processor.process(message())))
        assertTrue(processor.process(message(n = 2, desktop = DESKTOP_B, key = keyB)) is PushOutcome.Shown)
        // Registering again creates a new key: the old key's pushes no longer verify.
        val newKey = store.getOrCreateKey(DESKTOP_A)!!
        assertEquals(RejectReason.AUTH_FAILED, rejected(processor.process(message(n = 3, key = keyA))))
        assertTrue(processor.process(message(n = 4, key = newKey)) is PushOutcome.Shown)
    }

    @Test
    fun visibleDesktopIsSuppressedOthersAreNot() {
        presence.setStarted(true)
        presence.set(DESKTOP_A, true)
        val outcome = processor.process(message(n = 1))
        assertEquals(SuppressReason.DESKTOP_VISIBLE, (outcome as PushOutcome.Suppressed).reason)
        assertTrue(processor.process(message(n = 2, desktop = DESKTOP_B, key = keyB)) is PushOutcome.Shown)
        assertEquals(1, shown.size)
        // A suppressed message is still consumed: its redelivery must not pop up later.
        assertEquals(RejectReason.REPLAY, rejected(processor.process(message(n = 1))))

        presence.set(DESKTOP_A, false)
        assertTrue(processor.process(message(n = 3)) is PushOutcome.Shown)
        presence.set(DESKTOP_A, true)
        presence.setStarted(false)
        assertTrue("activity stopped: not visible", processor.process(message(n = 4)) is PushOutcome.Shown)
    }

    @Test
    fun blockedNotificationsAreReportedNotCrashed() {
        sinkAnswer = null
        val outcome = processor.process(message())
        assertEquals(SuppressReason.NOTIFICATIONS_DISABLED, (outcome as PushOutcome.Suppressed).reason)
    }

    @Test
    fun titleNamesTheDesktopOnlyWithTwoOrMoreLabels() {
        store.replaceLabels(mapOf(DESKTOP_A to "OmpDeck on work-laptop"))
        processor.process(message(n = 1))
        assertEquals("OmpDeck", shown.last().second)

        store.replaceLabels(mapOf(DESKTOP_A to "OmpDeck on work-laptop", DESKTOP_B to "Home PC"))
        processor.process(message(n = 2))
        assertEquals("OmpDeck — work-laptop", shown.last().second)
        processor.process(message(n = 3, desktop = DESKTOP_B, key = keyB))
        assertEquals("OmpDeck — Home PC", shown.last().second)
        // A desktop without a label keeps the plain title, and its key is not dropped for lacking one.
        store.replaceLabels(mapOf(DESKTOP_B to "Home PC", "cd".repeat(32) to "Other"))
        processor.process(message(n = 4))
        assertEquals("OmpDeck", shown.last().second)
        assertTrue(store.keyFor(DESKTOP_A) != null)
    }

    @Test
    fun unreadableStoreDropsInsteadOfCrashing() {
        val broken = File(tempDir(), "state.bin")
        broken.mkdirs() // a directory where the file should be: reading it fails with an IOException
        val p = PushProcessor(PushStore(broken, TestBox()) { clock }, presence, { _, _, _ -> "x" }, { clock })
        assertEquals(RejectReason.STORE_UNAVAILABLE, rejected(p.process(message())))
    }

    @Test
    fun titleLabelHelperStripsTheDefaultPrefix() {
        assertEquals("OmpDeck — host", PushProcessor.titleWithLabel("OmpDeck", "OmpDeck on host"))
        assertEquals("OmpDeck — Mine", PushProcessor.titleWithLabel("OmpDeck", "Mine"))
        assertEquals("OmpDeck", PushProcessor.titleWithLabel("OmpDeck", null))
        assertEquals("OmpDeck", PushProcessor.titleWithLabel("OmpDeck", "OmpDeck on "))
    }
}
