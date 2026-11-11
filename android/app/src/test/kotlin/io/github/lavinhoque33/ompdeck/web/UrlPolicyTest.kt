package io.github.lavinhoque33.ompdeck.web

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class UrlPolicyTest {
    @Test
    fun onlyTheBundledOriginIsTheApp() {
        assertTrue(UrlPolicy.isAppUrl("https://appassets.androidplatform.net/index.html"))
        assertTrue(UrlPolicy.isAppUrl("https://appassets.androidplatform.net/"))
        assertTrue(UrlPolicy.isAppUrl("https://appassets.androidplatform.net/#s=abc&d=1"))
        assertTrue(UrlPolicy.isAppUrl(UrlPolicy.START_URL))
        for (other in listOf(
            "http://appassets.androidplatform.net/index.html",
            "https://appassets.androidplatform.net.evil.example/index.html",
            "https://evilappassets.androidplatform.net/",
            "https://appassets.androidplatform.net@evil.example/",
            "https://appassets.androidplatform.net:8443/",
            "https://user@appassets.androidplatform.net/",
            "https://evil.example/https://appassets.androidplatform.net/",
            "https://evil.example/#https://appassets.androidplatform.net/",
            "file:///android_asset/www/index.html",
            "content://appassets.androidplatform.net/",
            "javascript:alert(1)",
            "about:blank",
            "data:text/html,<script>1</script>",
            "intent://appassets.androidplatform.net#Intent;end",
            "",
            "https://appassets.androidplatform.net/" + "a".repeat(5000),
        )) {
            assertFalse(other, UrlPolicy.isAppUrl(other))
        }
    }

    @Test
    fun externalLinksAreLimitedToSafeSchemes() {
        assertEquals("https://example.com/a?b=c#d", UrlPolicy.externalTarget("https://example.com/a?b=c#d"))
        assertEquals("http://example.com/", UrlPolicy.externalTarget("http://example.com/"))
        assertEquals("mailto:someone@example.com", UrlPolicy.externalTarget("mailto:someone@example.com"))
        assertEquals("tel:+15551234567", UrlPolicy.externalTarget("tel:+15551234567"))
        for (bad in listOf(
            "intent://scan/#Intent;scheme=zxing;end",
            "intent:#Intent;action=android.intent.action.CALL;end",
            "android-app://com.example/",
            "file:///data/data/io.github.lavinhoque33.ompdeck/files/push-state.bin",
            "content://contacts/people",
            "javascript:alert(1)",
            "data:text/html,x",
            "market://details?id=x",
            "sms:123",
            "ftp://example.com/",
            "HTTPS-evil://example.com",
            "https://",
            "https:///path",
            "mailto:",
            "tel:",
            "example.com/no-scheme",
            "https://exa mple.com/",
            "https://example.com/\u0000",
            "https://example.com/\n",
            "",
            "https://example.com/" + "a".repeat(5000),
        )) {
            assertNull(bad, UrlPolicy.externalTarget(bad))
        }
    }

    @Test
    fun schemeCaseDoesNotBypassTheAllowList() {
        assertEquals("HTTPS://example.com/", UrlPolicy.externalTarget("HTTPS://example.com/"))
        assertNull(UrlPolicy.externalTarget("JaVaScRiPt:alert(1)"))
        assertNull(UrlPolicy.externalTarget("INTENT://x#Intent;end"))
    }
}
