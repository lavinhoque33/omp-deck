package io.github.lavinhoque33.ompdeck.push

import java.security.GeneralSecurityException
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * The FCM `data` map the gateway sends: `{v:"1", desktop:<64 lowercase hex>, nonce:<base64url 12 bytes>,
 * ciphertext:<base64url AES-256-GCM ciphertext || 16-byte tag>}`. The key is the 32-byte secret this phone shared
 * with that desktop; the AAD is `ompdeck-android-push-v1:<desktop>` so a ciphertext cannot be replayed under another
 * desktop. Every field is validated before any key is looked up.
 */
class PushEnvelope private constructor(
    val desktop: String,
    /** The nonce, in the exact spelling it arrived in (the replay cache key). */
    val nonceText: String,
    private val nonce: ByteArray,
    private val ciphertext: ByteArray,
) {
    /** The plaintext, or null when the authentication tag does not verify (wrong key, other desktop, tampering). */
    fun decrypt(key: ByteArray): ByteArray? {
        if (key.size != KEY_BYTES) return null
        return try {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BYTES * 8, nonce))
            cipher.updateAAD((AAD_PREFIX + desktop).toByteArray(Charsets.UTF_8))
            cipher.doFinal(ciphertext)
        } catch (_: GeneralSecurityException) {
            null
        }
    }

    companion object {
        const val VERSION = "1"
        const val AAD_PREFIX = "ompdeck-android-push-v1:"
        const val KEY_BYTES = 32
        const val NONCE_BYTES = 12
        const val TAG_BYTES = 16

        /** FCM caps a message's data at 4 KiB; a larger ciphertext cannot be genuine. */
        const val MAX_PLAINTEXT_BYTES = 4096

        private val DESKTOP = Regex("[0-9a-f]{64}")

        /** Reads the envelope out of FCM data, or null when any field is missing, mistyped or non-canonical. */
        fun parse(data: Map<String, String>): PushEnvelope? {
            if (data["v"] != VERSION) return null
            val desktop = data["desktop"] ?: return null
            if (!DESKTOP.matches(desktop)) return null
            val nonceText = data["nonce"] ?: return null
            val nonce = Base64Url.decodeCanonical(nonceText, NONCE_BYTES) ?: return null
            if (nonce.size != NONCE_BYTES) return null
            val ciphertext = Base64Url.decodeCanonical(data["ciphertext"] ?: return null, MAX_PLAINTEXT_BYTES + TAG_BYTES) ?: return null
            // At least one plaintext byte plus the tag.
            if (ciphertext.size <= TAG_BYTES) return null
            return PushEnvelope(desktop, nonceText, nonce, ciphertext)
        }
    }
}
