package io.github.lavinhoque33.ompdeck.web

import java.net.URI
import java.net.URISyntaxException

/** Which URLs the WebView may load itself and which are handed to the system. */
object UrlPolicy {
    const val HOST = "appassets.androidplatform.net"
    const val ORIGIN = "https://$HOST"
    const val START_URL = "$ORIGIN/index.html"

    /** The only external schemes opened (as a plain VIEW of the system's handler): web links, mail and phone numbers. */
    private val EXTERNAL_SCHEMES = setOf("https", "http", "mailto", "tel")
    private const val MAX_URL = 4096

    /** True only for `https://appassets.androidplatform.net[/path]` itself: no userinfo, no other port, no look-alike host. */
    fun isAppUrl(url: String): Boolean {
        if (url.length > MAX_URL) return false
        val uri = parse(url) ?: return false
        return uri.scheme == "https" && uri.rawUserInfo == null && uri.port == -1 && uri.host == HOST
    }

    /**
     * The normalized URL to open in the system browser/mail/dialer, or null when [url] must not leave the app at
     * all (`intent:`, `file:`, `content:`, `javascript:`, custom schemes, malformed or oversized URLs).
     */
    fun externalTarget(url: String): String? {
        if (url.length > MAX_URL || url.any { it.isISOControl() }) return null
        val uri = parse(url) ?: return null
        val scheme = uri.scheme?.lowercase() ?: return null
        if (scheme !in EXTERNAL_SCHEMES) return null
        return when (scheme) {
            "https", "http" -> if (uri.host.isNullOrEmpty()) null else uri.toString()
            else -> if (uri.rawSchemeSpecificPart.isNullOrEmpty()) null else uri.toString()
        }
    }

    private fun parse(url: String): URI? = try {
        URI(url)
    } catch (_: URISyntaxException) {
        null
    }
}
