package io.github.lavinhoque33.ompdeck.push

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.security.UnrecoverableKeyException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Authenticated encryption at rest. [open] throws [SecretBoxException] when the data does not authenticate or the key is gone. */
interface SecretBox {
    fun seal(plain: ByteArray): ByteArray

    @Throws(SecretBoxException::class)
    fun open(sealed: ByteArray): ByteArray
}

class SecretBoxException(message: String, cause: Throwable? = null) : Exception(message, cause)

/** Frames `[version 1][iv 12][ciphertext||tag]`; the key comes from [keyProvider] (a non-exportable Keystore key in production). */
abstract class AesGcmSecretBox : SecretBox {
    protected abstract fun key(): SecretKey

    override fun seal(plain: ByteArray): ByteArray {
        try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, key())
            cipher.updateAAD(AAD)
            val iv = cipher.iv
            check(iv.size == IV_BYTES) { "unexpected IV length" }
            val body = cipher.doFinal(plain)
            return byteArrayOf(VERSION) + iv + body
        } catch (e: GeneralSecurityException) {
            throw IllegalStateException("sealing failed", e)
        }
    }

    override fun open(sealed: ByteArray): ByteArray {
        if (sealed.size < 1 + IV_BYTES + TAG_BYTES || sealed[0] != VERSION) throw SecretBoxException("bad frame")
        // Fetched outside the try: a Keystore that cannot be reached is not evidence that the data is bad.
        val key = key()
        try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_BYTES * 8, sealed, 1, IV_BYTES))
            cipher.updateAAD(AAD)
            return cipher.doFinal(sealed, 1 + IV_BYTES, sealed.size - 1 - IV_BYTES)
        } catch (e: GeneralSecurityException) {
            throw SecretBoxException("does not authenticate", e)
        }
    }

    private companion object {
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val VERSION: Byte = 1
        const val IV_BYTES = 12
        const val TAG_BYTES = 16
        val AAD = "ompdeck-android-store-v1".toByteArray(Charsets.UTF_8)
    }
}

/**
 * An AES-256-GCM key generated inside the Android Keystore under [alias] (never exportable, no user-auth requirement
 * so pushes can be decrypted while the phone is locked). Not backed up: [allowBackup=false] and the key is device-bound.
 */
class KeystoreSecretBox(private val alias: String = DEFAULT_ALIAS) : AesGcmSecretBox() {
    @Synchronized
    override fun key(): SecretKey {
        val store = KeyStore.getInstance(PROVIDER).apply { load(null) }
        val existing = try {
            store.getKey(alias, null) as? SecretKey
        } catch (_: UnrecoverableKeyException) {
            // The key was invalidated: it is as good as gone, so a fresh one takes its place (old data then fails to open).
            store.deleteEntry(alias)
            null
        }
        existing?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER)
        generator.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .setRandomizedEncryptionRequired(true)
                .build(),
        )
        return generator.generateKey()
    }

    /** Removes the Keystore key (tests; any data sealed with it becomes unreadable). */
    @Synchronized
    fun deleteKey() {
        KeyStore.getInstance(PROVIDER).apply { load(null) }.deleteEntry(alias)
    }

    companion object {
        const val DEFAULT_ALIAS = "ompdeck_push_store_v1"
        private const val PROVIDER = "AndroidKeyStore"
    }
}
