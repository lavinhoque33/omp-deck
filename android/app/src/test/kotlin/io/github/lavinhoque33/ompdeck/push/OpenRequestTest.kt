package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import java.io.File

class OpenRequestTest {
    private lateinit var store: PushStore
    private lateinit var secret: String

    @Before
    fun setUp() {
        store = PushStore(File(tempDir(), "s.bin"), TestBox())
        secret = store.openSecret()
    }

    private fun validate(
        action: String? = OpenRequest.ACTION,
        url: String? = "/#s=abc&d=$DESKTOP_A",
        desktop: String? = DESKTOP_A,
        token: String? = secret,
    ) = OpenRequest.validate(action, url, desktop, token, store)

    @Test
    fun acceptsOurOwnPendingIntent() {
        val request = validate()!!
        assertEquals("/#s=abc&d=$DESKTOP_A", request.url)
        assertEquals(DESKTOP_A, request.desktop)
        assertNotNull(validate(url = "/"))
    }

    @Test
    fun foreignIntentsOpenNothing() {
        // The launcher activity is exported: another app can send it anything, but cannot know the install secret.
        assertNull(validate(token = null))
        assertNull(validate(token = ""))
        assertNull(validate(token = "guess"))
        assertNull(validate(action = "android.intent.action.VIEW"))
        assertNull(validate(action = null))
        assertNull(validate(url = null))
        assertNull(validate(desktop = null))
    }

    @Test
    fun evenWithTheSecretTheDestinationMustBeAnInAppHash() {
        for (bad in listOf(
            "https://evil.example/#s=1",
            "//evil.example/",
            "javascript:alert(1)",
            "/other",
            "/../x",
            "/#<script>",
            "/#pair=v1.x y",
            "/#" + "a".repeat(PushPayload.MAX_HASH + 1),
            "",
        )) {
            assertNull(bad, validate(url = bad))
        }
        assertNull(validate(desktop = DESKTOP_A.uppercase()))
        assertNull(validate(desktop = "x"))
    }
}
