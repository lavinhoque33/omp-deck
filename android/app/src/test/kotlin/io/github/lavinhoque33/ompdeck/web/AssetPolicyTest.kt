package io.github.lavinhoque33.ompdeck.web

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AssetPolicyTest {
    @Test
    fun servesOnlyPlainFilesUnderWww() {
        assertEquals("index.html", AssetPolicy.resolve(""))
        assertEquals("index.html", AssetPolicy.resolve("index.html"))
        assertEquals("assets/index-k0fxp2vt.js", AssetPolicy.resolve("assets/index-k0fxp2vt.js"))
        for (bad in listOf(
            "../secret",
            "assets/../../secret",
            "assets/./x",
            "./x",
            "assets//x",
            "assets/",
            "/etc/passwd",
            "a\\..\\b",
            "a/b\u0000.js",
            "a/b\n.js",
            "a/" + "b".repeat(600),
        )) {
            assertNull(bad, AssetPolicy.resolve(bad))
        }
    }

    @Test
    fun contentTypesFollowTheExtension() {
        assertEquals("text/html", AssetPolicy.mimeType("index.html"))
        assertEquals("text/javascript", AssetPolicy.mimeType("assets/a.js"))
        assertEquals("text/css", AssetPolicy.mimeType("assets/a.css"))
        assertEquals("font/ttf", AssetPolicy.mimeType("assets/Geist.ttf"))
        assertEquals("application/octet-stream", AssetPolicy.mimeType("assets/noext"))
        assertEquals("application/octet-stream", AssetPolicy.mimeType("a.JS.exe"))
        assertTrue(AssetPolicy.isText("a.js"))
        assertFalse(AssetPolicy.isText("a.ttf"))
    }

    @Test
    fun cspKeepsTheRelaysPolicyAndOnlyWidensTheRelayConnection() {
        val directives = AssetPolicy.CSP.split(';').map { it.trim() }
        assertTrue("default-src 'none'" in directives)
        assertTrue("script-src 'self'" in directives)
        assertTrue("connect-src 'self' wss:" in directives)
        assertTrue("base-uri 'none'" in directives)
        assertTrue("form-action 'none'" in directives)
        assertFalse(AssetPolicy.CSP.contains("'unsafe-eval'"))
        assertFalse(AssetPolicy.CSP.contains("script-src 'self' 'unsafe-inline'"))
        assertEquals(AssetPolicy.CSP, AssetPolicy.HEADERS["Content-Security-Policy"])
    }
}
