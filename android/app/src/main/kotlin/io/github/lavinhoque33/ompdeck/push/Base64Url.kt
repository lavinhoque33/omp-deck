package io.github.lavinhoque33.ompdeck.push

import java.util.Base64

/** Unpadded base64url (RFC 4648 §5), the encoding of the FCM envelope fields and of the per-desktop key. */
object Base64Url {
    fun encode(bytes: ByteArray): String = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)

    /**
     * Decodes [text], or null unless it is exactly the canonical unpadded encoding of at most [maxBytes] bytes:
     * URL alphabet only (no `+`, `/`, `=`, whitespace) and zero trailing bits, so one byte string has one spelling.
     */
    fun decodeCanonical(text: String, maxBytes: Int): ByteArray? {
        if (text.isEmpty() || text.length > (maxBytes * 4 + 2) / 3) return null
        for (c in text) {
            val ok = c in 'A'..'Z' || c in 'a'..'z' || c in '0'..'9' || c == '-' || c == '_'
            if (!ok) return null
        }
        val bytes = try {
            Base64.getUrlDecoder().decode(text)
        } catch (_: IllegalArgumentException) {
            return null
        }
        if (bytes.size > maxBytes || encode(bytes) != text) return null
        return bytes
    }
}
