package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PushPayloadTest {
    private val now = 1_800_000_000_000L
    private val ttl = PushPayload.TTL_MS
    private val skew = PushPayload.SKEW_MS

    private fun parse(json: String, at: Long = now) = PushPayload.parse(json.toByteArray(Charsets.UTF_8), DESKTOP_A, at)

    private fun ok(json: String, at: Long = now): PushNotification = (parse(json, at) as ParsedPayload.Ok).notification

    private fun error(json: String, at: Long = now): PayloadError = (parse(json, at) as ParsedPayload.Rejected).error

    @Test
    fun readsTheGatewaysDeclarativePayload() {
        val n = ok(Sender.payload(now + ttl, title = "OmpDeck", body = "Finished", tag = "s-1"))
        assertEquals("OmpDeck", n.title)
        assertEquals("Finished", n.body)
        assertEquals("s-1", n.tag)
        assertFalse(n.silent)
        assertEquals("/#s=abc&d=$DESKTOP_A", n.navigate)
        assertEquals(DESKTOP_A, n.desktop)
    }

    @Test
    fun expiryWindowIsEnforced() {
        assertEquals(PayloadError.EXPIRED, error(Sender.payload(now - skew - 1)))
        assertTrue(parse(Sender.payload(now - skew)) is ParsedPayload.Ok)
        assertTrue(parse(Sender.payload(now + ttl + skew)) is ParsedPayload.Ok)
        assertEquals(PayloadError.TOO_FAR_IN_FUTURE, error(Sender.payload(now + ttl + skew + 1)))
        // A gateway-clock timestamp from the Unix epoch era is expired, not "fine".
        assertEquals(PayloadError.EXPIRED, error(Sender.payload(1)))
    }

    @Test
    fun expiryIsRequiredAndMustBeAnInteger() {
        val noExpiry = """{"web_push":8030,"notification":{"title":"t"}}"""
        assertEquals(PayloadError.MALFORMED, error(noExpiry))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t"},"expires_unix_ms":"${now + 1000}"}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t"},"expires_unix_ms":${now + 1000}.5}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t"},"expires_unix_ms":0}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t"},"expires_unix_ms":-5}"""))
    }

    @Test
    fun rejectsPayloadsThatAreNotTheDeclarativeShape() {
        val e = now + ttl
        assertEquals(PayloadError.MALFORMED, error("not json"))
        assertEquals(PayloadError.MALFORMED, error("""{"notification":{"title":"t"},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":1,"notification":{"title":"t"},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":"8030","notification":{"title":"t"},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":"x","expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":""},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"  \n"},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":5},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t","body":5},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t","silent":"yes"},"expires_unix_ms":$e}"""))
        assertEquals(PayloadError.MALFORMED, error("""{"web_push":8030,"notification":{"title":"t","tag":7},"expires_unix_ms":$e}"""))
        val huge = "x".repeat(PushPayload.MAX_PAYLOAD_BYTES)
        assertEquals(PayloadError.MALFORMED, error(Sender.payload(e, body = huge)))
    }

    @Test
    fun optionalFieldsDefaultAndTextIsBounded() {
        val n = ok("""{"web_push":8030,"notification":{"title":"t"},"expires_unix_ms":${now + ttl}}""")
        assertEquals("", n.body)
        assertNull(n.tag)
        assertFalse(n.silent)
        assertEquals("/#d=$DESKTOP_A", n.navigate)

        val long = ok(Sender.payload(now + ttl, title = "T".repeat(500), body = "b".repeat(3000)))
        assertEquals(PushPayload.MAX_TITLE, long.title.length)
        assertEquals(PushPayload.MAX_BODY, long.body.length)
        val controls = ok("""{"web_push":8030,"notification":{"title":"a\u0007b","body":"x\u0000y\nz"},"expires_unix_ms":${now + ttl}}""")
        assertEquals("ab", controls.title)
        assertEquals("xy\nz", controls.body)
    }

    @Test
    fun navigationKeepsTheHashAndNamesTheRightDesktop() {
        assertEquals("/#d=$DESKTOP_A", PushPayload.navigate("https://relay.example.com/", DESKTOP_A))
        assertEquals("/#s=abc%20d&d=$DESKTOP_A", PushPayload.navigate("https://relay.example.com/#s=abc%20d", DESKTOP_A))
        assertEquals(
            "/#route=conversation&live=k1&d=$DESKTOP_A",
            PushPayload.navigate("https://relay.example.com:8443/#route=conversation&live=k1", DESKTOP_A),
        )
        // A `d` the sender (or an attacker-controlled payload) put in the hash never survives: the phone decides the desktop.
        assertEquals("/#s=1&d=$DESKTOP_A", PushPayload.navigate("https://r.example/#d=$DESKTOP_B&s=1&d=zz", DESKTOP_A))
        // Query and path are dropped: the app is a bundle, only its hash routes.
        assertEquals("/#s=1&d=$DESKTOP_A", PushPayload.navigate("https://r.example/some/path?x=1#s=1", DESKTOP_A))
    }

    @Test
    fun navigationRefusesAnythingButAnHttpsUrl() {
        val fallback = "/#d=$DESKTOP_A"
        for (bad in listOf(
            "http://r.example/#s=1",
            "javascript:alert(1)",
            "intent://x#Intent;scheme=https;end",
            "file:///etc/passwd#s=1",
            "https://user:pw@r.example/#s=1",
            "//r.example/#s=1",
            "/#s=1",
            "https:///#s=1",
            "https://r.example/#s=<script>",
            "https://r.example/#" + "a".repeat(PushPayload.MAX_HASH + 1),
            "",
        )) {
            assertEquals(bad, fallback, PushPayload.navigate(bad, DESKTOP_A))
        }
        assertEquals(fallback, PushPayload.navigate(null, DESKTOP_A))
        assertEquals(fallback, PushPayload.navigate(42, DESKTOP_A))
    }
}
