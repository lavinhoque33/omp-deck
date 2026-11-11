package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class PushEnvelopeTest {
    // Known-answer vector produced by node:crypto (aes-256-gcm), independent of both the app and the Rust sender.
    private val katKey = Base64Url.decodeCanonical("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8", 32)!!
    private val katDesktop = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
    private val katNonce = "oKGio6Slpqeoqaqr"
    private val katCiphertext =
        "nToLSCeUcsoRDaXpP0rz7lyON3_m3iQF_29S7xDFVzupVDOW2042H2W-S6V5PuaaLDlqagC_fgdjZCkNwQP22dvSpQlZy42TnIebOqPsotu7YszFsaS7QMwdSu7B6Xe7oPZOx2LXof7mumsg6e3DS0s9To2tCPpeTO3yhsA0lW__M8RO8W6iZF41m-VmaKuw6bXeDsP82OmFvdSHyrRrXqhxZ-7bGWIsszIbEl_ihtFHFs2oMhtrp8NPi_jHipgKQfajsTO4"
    private val katPlain =
        """{"web_push":8030,"notification":{"title":"OmpDeck","body":"Session finished","navigate":"https://relay.example.com/#s=abc","tag":"t1","silent":false},"expires_unix_ms":1800000000000}"""

    private fun kat(
        desktop: String = katDesktop,
        nonce: String = katNonce,
        ciphertext: String = katCiphertext,
        v: String? = "1",
    ): Map<String, String> = buildMap {
        if (v != null) put("v", v)
        put("desktop", desktop)
        put("nonce", nonce)
        put("ciphertext", ciphertext)
    }

    @Test
    fun decryptsTheKnownAnswerVector() {
        val envelope = PushEnvelope.parse(kat())!!
        assertEquals(katDesktop, envelope.desktop)
        assertEquals(katPlain, String(envelope.decrypt(katKey)!!, Charsets.UTF_8))
    }

    @Test
    fun ciphertextBoundToItsDesktopViaAad() {
        // Same bytes claiming another desktop: the tag no longer verifies.
        val relabelled = PushEnvelope.parse(kat(desktop = DESKTOP_B))!!
        assertNull(relabelled.decrypt(katKey))
    }

    @Test
    fun wrongKeyAndTamperingFailAuthentication() {
        val envelope = PushEnvelope.parse(kat())!!
        assertNull(envelope.decrypt(ByteArray(32)))
        assertNull(envelope.decrypt(katKey.copyOf(31)))
        val flipped = Base64Url.decodeCanonical(katCiphertext, 4096 + 16)!!
        flipped[3] = (flipped[3].toInt() xor 1).toByte()
        assertNull(PushEnvelope.parse(kat(ciphertext = Base64Url.encode(flipped)))!!.decrypt(katKey))
        // A flipped tag byte.
        val tagFlipped = Base64Url.decodeCanonical(katCiphertext, 4096 + 16)!!
        tagFlipped[tagFlipped.size - 1] = (tagFlipped.last().toInt() xor 0x80).toByte()
        assertNull(PushEnvelope.parse(kat(ciphertext = Base64Url.encode(tagFlipped)))!!.decrypt(katKey))
        // A different nonce.
        val otherNonce = Base64Url.encode(ByteArray(12) { 7 })
        assertNull(PushEnvelope.parse(kat(nonce = otherNonce))!!.decrypt(katKey))
    }

    @Test
    fun roundTripsWithAnIndependentSender() {
        val key = ByteArray(32) { (it * 3).toByte() }
        val data = Sender.data(key, DESKTOP_A, Sender.nonce(1), "hello")
        assertArrayEquals("hello".toByteArray(), PushEnvelope.parse(data)!!.decrypt(key))
    }

    @Test
    fun rejectsEveryMalformedField() {
        assertNotNull(PushEnvelope.parse(kat()))
        assertNull("no version", PushEnvelope.parse(kat(v = null)))
        assertNull("version 2", PushEnvelope.parse(kat(v = "2")))
        assertNull("uppercase desktop", PushEnvelope.parse(kat(desktop = katDesktop.uppercase())))
        assertNull("short desktop", PushEnvelope.parse(kat(desktop = katDesktop.take(63))))
        assertNull("non-hex desktop", PushEnvelope.parse(kat(desktop = "g" + katDesktop.drop(1))))
        assertNull("11-byte nonce", PushEnvelope.parse(kat(nonce = Base64Url.encode(ByteArray(11)))))
        assertNull("13-byte nonce", PushEnvelope.parse(kat(nonce = Base64Url.encode(ByteArray(13)))))
        assertNull("padded", PushEnvelope.parse(kat(ciphertext = katCiphertext + "=")))
        assertNull("standard alphabet", PushEnvelope.parse(kat(ciphertext = katCiphertext.replace('-', '+'))))
        assertNull("whitespace", PushEnvelope.parse(kat(ciphertext = " $katCiphertext")))
        assertNull("tag only", PushEnvelope.parse(kat(ciphertext = Base64Url.encode(ByteArray(16)))))
        assertNull("empty", PushEnvelope.parse(kat(ciphertext = "")))
        assertNull("oversized", PushEnvelope.parse(kat(ciphertext = Base64Url.encode(ByteArray(4096 + 17)))))
        assertNull("missing field", PushEnvelope.parse(kat().minus("nonce")))
    }

    @Test
    fun extraDataKeysAreIgnored() {
        // FCM adds its own keys (e.g. from/collapse_key); they are unauthenticated and unused.
        assertNotNull(PushEnvelope.parse(kat() + ("collapse_key" to "x")))
    }
}
