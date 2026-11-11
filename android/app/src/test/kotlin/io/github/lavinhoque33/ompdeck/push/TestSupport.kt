package io.github.lavinhoque33.ompdeck.push

import java.io.File
import java.nio.file.Files
import javax.crypto.Cipher
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/** The store's AES-GCM sealing with an in-memory key (the Keystore only exists on a device). */
class TestBox(private val raw: ByteArray = ByteArray(32) { (it + 1).toByte() }) : AesGcmSecretBox() {
    override fun key(): SecretKey = SecretKeySpec(raw, "AES")
}

fun tempDir(): File = Files.createTempDirectory("ompdeck-test").toFile().also { it.deleteOnExit() }

const val DESKTOP_A = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
const val DESKTOP_B = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"

/**
 * An independent implementation of the gateway's envelope (plain javax.crypto, not the code under test):
 * AES-256-GCM, 12-byte nonce, AAD `ompdeck-android-push-v1:<desktop>`, ciphertext || tag, base64url unpadded.
 */
object Sender {
    fun encrypt(key: ByteArray, desktop: String, nonce: ByteArray, plain: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        cipher.updateAAD("ompdeck-android-push-v1:$desktop".toByteArray(Charsets.UTF_8))
        return cipher.doFinal(plain)
    }

    fun data(key: ByteArray, desktop: String, nonce: ByteArray, plain: String, claimedDesktop: String = desktop): Map<String, String> =
        mapOf(
            "v" to "1",
            "desktop" to claimedDesktop,
            "nonce" to Base64Url.encode(nonce),
            "ciphertext" to Base64Url.encode(encrypt(key, desktop, nonce, plain.toByteArray(Charsets.UTF_8))),
        )

    fun nonce(n: Int): ByteArray = ByteArray(12) { (n + it).toByte() }.also { it[0] = (n shr 8).toByte(); it[1] = n.toByte() }

    fun payload(
        expires: Long,
        title: String = "OmpDeck",
        body: String = "Session finished",
        navigate: String = "https://relay.example.com/#s=abc",
        tag: String? = "t1",
        silent: Boolean = false,
    ): String {
        val tagJson = if (tag == null) "null" else "\"$tag\""
        return """{"web_push":8030,"notification":{"title":"$title","body":"$body","navigate":"$navigate","tag":$tagJson,"silent":$silent},"expires_unix_ms":$expires}"""
    }
}
