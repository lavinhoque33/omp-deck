package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNull
import org.junit.Test

class Base64UrlTest {
    private val alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"

    @Test
    fun roundTripsAndIsCanonical() {
        for (size in 1..40) {
            val bytes = ByteArray(size) { (it * 7 + size).toByte() }
            assertArrayEquals(bytes, Base64Url.decodeCanonical(Base64Url.encode(bytes), 64))
        }
    }

    @Test
    fun refusesNonCanonicalTrailingBits() {
        // 17 bytes leave two spare bits in the last character: setting one decodes to the same bytes but is not canonical.
        val text = Base64Url.encode(ByteArray(17) { 1 })
        val bumped = text.dropLast(1) + alphabet[alphabet.indexOf(text.last()) + 1]
        assertNull(Base64Url.decodeCanonical(bumped, 64))
    }

    @Test
    fun refusesOversizeAndBadAlphabet() {
        assertNull(Base64Url.decodeCanonical(Base64Url.encode(ByteArray(33)), 32))
        assertNull(Base64Url.decodeCanonical("AAAA\n", 32))
        assertNull(Base64Url.decodeCanonical("AA+A", 32))
        assertNull(Base64Url.decodeCanonical("AAA=", 32))
    }
}
