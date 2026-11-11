package io.github.lavinhoque33.ompdeck.push

import io.github.lavinhoque33.ompdeck.bridge.Text
import org.json.JSONException
import org.json.JSONObject
import java.net.URI
import java.net.URISyntaxException

/** What a push shows: [navigate] is a path and hash of the app (`/` or `/#…`, already naming [desktop] in the hash). */
class PushNotification(
    val desktop: String,
    val title: String,
    val body: String,
    val navigate: String,
    val tag: String?,
    val silent: Boolean,
)

enum class PayloadError { MALFORMED, EXPIRED, TOO_FAR_IN_FUTURE }

sealed interface ParsedPayload {
    /** [expiresAtMs] is the gateway's `expires_unix_ms`. */
    class Ok(val notification: PushNotification, val expiresAtMs: Long) : ParsedPayload

    class Rejected(val error: PayloadError) : ParsedPayload
}

/**
 * The decrypted declarative payload `{web_push: 8030, notification: {title, body, navigate, tag, silent},
 * expires_unix_ms}` (the Web Push payload plus the expiry the gateway adds for FCM). It is authenticated by the
 * envelope, but still validated: types, sizes and the expiry window.
 */
object PushPayload {
    const val WEB_PUSH_MARKER = 8030
    const val MAX_PAYLOAD_BYTES = 4096
    const val MAX_TITLE = 200
    const val MAX_BODY = 1000
    const val MAX_TAG = 128
    const val MAX_NAVIGATE = 2048
    const val MAX_HASH = 1024

    /** The gateway's FCM lifetime: `expires_unix_ms` is the enqueue time plus this. */
    const val TTL_MS = 3_600_000L

    /** Tolerated difference between the phone's and the desktop's clocks, each way. */
    const val SKEW_MS = 5 * 60_000L

    private val HASH_CHARS = Regex("[A-Za-z0-9._~%!$'()*+,;:@/?=&-]*")

    fun parse(plain: ByteArray, desktop: String, nowMs: Long): ParsedPayload {
        if (plain.size > MAX_PAYLOAD_BYTES) return ParsedPayload.Rejected(PayloadError.MALFORMED)
        val root = try {
            JSONObject(String(plain, Charsets.UTF_8))
        } catch (_: JSONException) {
            return ParsedPayload.Rejected(PayloadError.MALFORMED)
        }
        if (integer(root.opt("web_push")) != WEB_PUSH_MARKER.toLong()) return ParsedPayload.Rejected(PayloadError.MALFORMED)
        val expires = integer(root.opt("expires_unix_ms")) ?: return ParsedPayload.Rejected(PayloadError.MALFORMED)
        if (expires <= 0) return ParsedPayload.Rejected(PayloadError.MALFORMED)
        if (nowMs > expires + SKEW_MS) return ParsedPayload.Rejected(PayloadError.EXPIRED)
        if (expires > nowMs + TTL_MS + SKEW_MS) return ParsedPayload.Rejected(PayloadError.TOO_FAR_IN_FUTURE)

        val notification = root.opt("notification") as? JSONObject ?: return ParsedPayload.Rejected(PayloadError.MALFORMED)
        val title = notification.opt("title") as? String ?: return ParsedPayload.Rejected(PayloadError.MALFORMED)
        val cleanTitle = Text.clean(title, MAX_TITLE, keepNewlines = false)
        if (cleanTitle.isEmpty()) return ParsedPayload.Rejected(PayloadError.MALFORMED)
        val body = when (val value = notification.opt("body")) {
            null, JSONObject.NULL -> ""
            is String -> Text.clean(value, MAX_BODY, keepNewlines = true)
            else -> return ParsedPayload.Rejected(PayloadError.MALFORMED)
        }
        val tag = when (val value = notification.opt("tag")) {
            null, JSONObject.NULL -> null
            is String -> Text.clean(value, MAX_TAG, keepNewlines = false).ifEmpty { null }
            else -> return ParsedPayload.Rejected(PayloadError.MALFORMED)
        }
        val silent = when (val value = notification.opt("silent")) {
            null, JSONObject.NULL -> false
            is Boolean -> value
            else -> return ParsedPayload.Rejected(PayloadError.MALFORMED)
        }
        return ParsedPayload.Ok(
            PushNotification(desktop, cleanTitle, body, navigate(notification.opt("navigate"), desktop), tag, silent),
            expires,
        )
    }

    /** An integral JSON number (`8030`, not `8030.0` or `"8030"`), or null. */
    private fun integer(value: Any?): Long? = when (value) {
        is Int -> value.toLong()
        is Long -> value
        else -> null
    }

    /**
     * The in-app destination of the payload's `navigate` (an absolute `https://` URL on the relay's origin): the
     * origin is dropped (the app is served from its own bundle), the hash is kept with any `d` parameter replaced by
     * `d=<desktop>` so the tap opens that desktop. Anything that is not such a URL becomes `/#d=<desktop>`.
     */
    fun navigate(value: Any?, desktop: String): String {
        val fallback = "/#d=$desktop"
        if (value !is String || value.length > MAX_NAVIGATE) return fallback
        val uri = try {
            URI(value)
        } catch (_: URISyntaxException) {
            return fallback
        }
        if (uri.scheme != "https" || uri.rawUserInfo != null || uri.host.isNullOrEmpty()) return fallback
        val hash = uri.rawFragment.orEmpty()
        if (hash.length > MAX_HASH || !HASH_CHARS.matches(hash)) return fallback
        val kept = hash.split('&').filter { it.isNotEmpty() && it.substringBefore('=') != "d" }
        return "/#" + (kept + "d=$desktop").joinToString("&")
    }
}
