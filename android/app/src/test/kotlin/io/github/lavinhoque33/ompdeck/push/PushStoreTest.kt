package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class PushStoreTest {
    private var clock = 1_000_000L

    private fun store(file: File, box: SecretBox = TestBox()) = PushStore(file, box) { clock }

    private fun contains(haystack: ByteArray, needle: ByteArray): Boolean =
        needle.isNotEmpty() && (0..haystack.size - needle.size).any { i -> needle.indices.all { haystack[i + it] == needle[it] } }

    @Test
    fun keysTokenAndConfigSurviveARestart() {
        val file = File(tempDir(), "s.bin")
        val first = store(file)
        val key = first.getOrCreateKey(DESKTOP_A)!!
        assertEquals(32, key.size)
        assertArrayEquals("same key on repeat", key, first.getOrCreateKey(DESKTOP_A))
        first.setToken("tok:1234567890123456789")
        first.setConfig(FirebaseConfig("AIzaKey", "1:123456:android:abc", "proj", "123456", null))
        first.setLabel(DESKTOP_A, "Laptop")

        val second = store(file)
        assertArrayEquals(key, second.keyFor(DESKTOP_A))
        assertEquals("tok:1234567890123456789", second.token())
        assertEquals("proj", second.config()?.projectId)
        assertEquals(mapOf(DESKTOP_A to "Laptop"), second.labels())
        assertNull(second.keyFor(DESKTOP_B))
    }

    @Test
    fun theFileHoldsNothingReadable() {
        val file = File(tempDir(), "s.bin")
        val s = store(file)
        val key = s.getOrCreateKey(DESKTOP_A)!!
        s.setToken("tok:ABCDEFGHIJKLMNOPQRSTUVWXYZ")
        s.setConfig(FirebaseConfig("AIzaSecretApiKey", "1:123456:android:abc", "proj-x", "123456", null))
        s.registerNonce(DESKTOP_A, "nonceNonceNonce", clock + 1000)
        val bytes = file.readBytes()
        for (secret in listOf(
            key,
            Base64Url.encode(key).toByteArray(),
            DESKTOP_A.toByteArray(),
            "AIzaSecretApiKey".toByteArray(),
            "tok:ABCDEFGHIJKLMNOPQRSTUVWXYZ".toByteArray(),
            "nonceNonceNonce".toByteArray(),
            "proj-x".toByteArray(),
        )) {
            assertFalse(contains(bytes, secret))
        }
    }

    @Test
    fun aFileSealedByAnotherKeyOrAlteredStartsEmptyAndRecovers() {
        val file = File(tempDir(), "s.bin")
        store(file).getOrCreateKey(DESKTOP_A)

        // The Keystore key was lost: a different box cannot open the file.
        val lost = store(file, TestBox(ByteArray(32) { 99 }))
        assertNull(lost.keyFor(DESKTOP_A))
        assertNotNull(lost.getOrCreateKey(DESKTOP_B))
        assertNotNull("recovered store persists", store(file, TestBox(ByteArray(32) { 99 })).keyFor(DESKTOP_B))

        // A flipped byte in the sealed file does not authenticate.
        val file2 = File(tempDir(), "t.bin")
        store(file2).getOrCreateKey(DESKTOP_A)
        val bytes = file2.readBytes()
        bytes[bytes.size / 2] = (bytes[bytes.size / 2].toInt() xor 1).toByte()
        file2.writeBytes(bytes)
        assertNull(store(file2).keyFor(DESKTOP_A))

        // So does a truncated one.
        val file3 = File(tempDir(), "u.bin")
        store(file3).getOrCreateKey(DESKTOP_A)
        file3.writeBytes(file3.readBytes().copyOf(10))
        assertNull(store(file3).keyFor(DESKTOP_A))
    }

    @Test
    fun forgetRemovesOnlyThatDesktop() {
        val file = File(tempDir(), "s.bin")
        val s = store(file)
        s.getOrCreateKey(DESKTOP_A)
        val keyB = s.getOrCreateKey(DESKTOP_B)!!
        s.replaceLabels(mapOf(DESKTOP_A to "a", DESKTOP_B to "b"))
        s.registerNonce(DESKTOP_A, "n1", clock + 1000)
        s.registerNonce(DESKTOP_B, "n1", clock + 1000)
        s.forget(DESKTOP_A)
        val again = store(file)
        assertNull(again.keyFor(DESKTOP_A))
        assertArrayEquals(keyB, again.keyFor(DESKTOP_B))
        assertEquals(mapOf(DESKTOP_B to "b"), again.labels())
        assertTrue("A's nonce cache was dropped with it", again.registerNonce(DESKTOP_A, "n1", clock + 1000))
        assertFalse("B's was not", again.registerNonce(DESKTOP_B, "n1", clock + 1000))
    }

    @Test
    fun replacingLabelsNeverDeletesKeys() {
        val s = store(File(tempDir(), "s.bin"))
        s.getOrCreateKey(DESKTOP_A)
        s.replaceLabels(emptyMap())
        assertNotNull(s.keyFor(DESKTOP_A))
        assertEquals(setOf(DESKTOP_A), s.desktops())
    }

    @Test
    fun replayCacheIsDurableBoundedAndExpires() {
        val file = File(tempDir(), "s.bin")
        val s = store(file)
        assertTrue(s.registerNonce(DESKTOP_A, "first", clock + 1_000_000))
        assertFalse(s.registerNonce(DESKTOP_A, "first", clock + 1_000_000))
        assertFalse("durable", store(file).registerNonce(DESKTOP_A, "first", clock + 1_000_000))

        // Expired entries are pruned: after their window the nonce space is free (the payload is refused by its expiry anyway).
        assertTrue(s.registerNonce(DESKTOP_A, "short", clock + 10))
        clock += 11
        assertTrue(s.registerNonce(DESKTOP_A, "short", clock + 10))

        // Bounded: the oldest entries are evicted first.
        val bounded = store(File(tempDir(), "b.bin"))
        for (i in 0 until PushStore.MAX_REPLAY + 5) assertTrue(bounded.registerNonce(DESKTOP_A, "n$i", clock + 1_000_000))
        assertTrue("oldest evicted", bounded.registerNonce(DESKTOP_A, "n0", clock + 1_000_000))
        assertFalse("newest kept", bounded.registerNonce(DESKTOP_A, "n${PushStore.MAX_REPLAY + 4}", clock + 1_000_000))
    }

    @Test
    fun desktopCountIsCapped() {
        val s = store(File(tempDir(), "s.bin"))
        for (i in 0 until PushStore.MAX_DESKTOPS) assertNotNull(s.getOrCreateKey("%064x".format(i)))
        assertNull(s.getOrCreateKey("%064x".format(PushStore.MAX_DESKTOPS)))
        assertNotNull("existing desktops are unaffected", s.getOrCreateKey("%064x".format(0)))
    }

    @Test
    fun keysAreRandomPerDesktop() {
        val s = store(File(tempDir(), "s.bin"))
        val a = s.getOrCreateKey(DESKTOP_A)!!
        val b = s.getOrCreateKey(DESKTOP_B)!!
        assertNotEquals(a.toList(), b.toList())
        // Returned arrays are copies: mutating one does not change the stored key.
        a.fill(0)
        assertNotEquals(a.toList(), s.keyFor(DESKTOP_A)!!.toList())
    }

    @Test
    fun openSecretIsStableAndChecked() {
        val file = File(tempDir(), "s.bin")
        val s = store(file)
        val secret = s.openSecret()
        assertEquals(secret, store(file).openSecret())
        assertTrue(s.checkOpenSecret(secret))
        assertFalse(s.checkOpenSecret(null))
        assertFalse(s.checkOpenSecret(""))
        assertFalse(s.checkOpenSecret(secret + "x"))
        assertFalse(s.checkOpenSecret(secret.dropLast(1)))
    }

    @Test
    fun clearConfigKeepsDesktopKeys() {
        val s = store(File(tempDir(), "s.bin"))
        val key = s.getOrCreateKey(DESKTOP_A)!!
        s.setConfig(FirebaseConfig("k".repeat(20), "1:123456:android:abcdef12", "proj", "123456", "b.appspot.com"))
        s.setToken("t".repeat(30))
        s.clearConfig()
        assertNull(s.config())
        assertNull(s.token())
        assertArrayEquals(key, s.keyFor(DESKTOP_A))
    }
}
