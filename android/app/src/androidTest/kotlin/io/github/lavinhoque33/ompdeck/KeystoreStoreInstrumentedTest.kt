package io.github.lavinhoque33.ompdeck

import androidx.test.platform.app.InstrumentationRegistry
import io.github.lavinhoque33.ompdeck.push.Base64Url
import io.github.lavinhoque33.ompdeck.push.KeystoreSecretBox
import io.github.lavinhoque33.ompdeck.push.PushStore
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import java.io.File
import java.util.UUID

/** The per-desktop key really is sealed by a hardware-backed Android Keystore key and survives a "process restart". */
class KeystoreStoreInstrumentedTest {
    private val alias = "ompdeck_test_${UUID.randomUUID()}"
    private lateinit var file: File
    private val desktop = "ab".repeat(32)

    @Before
    fun setUp() {
        val dir = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        file = File(dir, "keystore-test-${UUID.randomUUID()}.bin")
    }

    @After
    fun tearDown() {
        file.delete()
        KeystoreSecretBox(alias).deleteKey()
    }

    private fun contains(haystack: ByteArray, needle: ByteArray) =
        (0..haystack.size - needle.size).any { i -> needle.indices.all { haystack[i + it] == needle[it] } }

    @Test
    fun keysAreEncryptedAtRestAndSurviveReopening() {
        val key = PushStore(file, KeystoreSecretBox(alias)).getOrCreateKey(desktop)!!
        assertEquals(32, key.size)

        val bytes = file.readBytes()
        assertFalse("raw key bytes in file", contains(bytes, key))
        assertFalse("encoded key in file", contains(bytes, Base64Url.encode(key).toByteArray()))
        assertFalse("desktop id in file", contains(bytes, desktop.toByteArray()))

        assertArrayEquals(key, PushStore(file, KeystoreSecretBox(alias)).keyFor(desktop))
    }

    @Test
    fun aLostKeystoreKeyYieldsAnEmptyStoreThatRecovers() {
        PushStore(file, KeystoreSecretBox(alias)).getOrCreateKey(desktop)
        KeystoreSecretBox(alias).deleteKey()

        val reopened = PushStore(file, KeystoreSecretBox(alias))
        assertNull("old keys cannot be recovered without the Keystore key", reopened.keyFor(desktop))
        assertNotNull(reopened.getOrCreateKey(desktop))
        assertNotNull(PushStore(file, KeystoreSecretBox(alias)).keyFor(desktop))
    }
}
