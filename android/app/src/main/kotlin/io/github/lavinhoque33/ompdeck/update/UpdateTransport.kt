package io.github.lavinhoque33.ompdeck.update

import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.net.HttpURLConnection
import java.security.MessageDigest
import javax.net.ssl.HttpsURLConnection

internal object UpdateTransport {
    private const val MAX_REDIRECTS = 5
    private const val CONNECT_TIMEOUT_MS = 20_000
    private const val READ_TIMEOUT_MS = 30_000
    private const val TRANSFER_TIMEOUT_NS = 15L * 60 * 1_000_000_000

    fun text(url: String, maxBytes: Long): String {
        val output = ByteArrayOutputStream()
        fetch(url, maxBytes) { input, length, deadline -> copyBounded(input, output, maxBytes, length, deadline = deadline) }
        return output.toString(Charsets.UTF_8.name())
    }

    fun apk(release: UpdateRelease, output: OutputStream, progress: (Long) -> Unit): String {
        val digest = MessageDigest.getInstance("SHA-256")
        fetch(release.apkUrl, UpdatePolicy.MAX_APK_BYTES) { input, length, deadline ->
            if (length != null && length != release.size) throw UpdateFailure("The APK download size differs from the release.")
            val copied = copyBounded(input, output, release.size, release.size, digest, deadline, progress)
            if (copied != release.size) throw UpdateFailure("The APK download is incomplete.")
        }
        return hex(digest.digest())
    }

    /** Every redirect is validated before opening it; neither cookies nor Authorization are ever sent. */
    private fun fetch(url: String, limit: Long, read: (InputStream, Long?, Long) -> Unit) {
        var uri = UpdatePolicy.checkedUrl(url)
        val deadline = System.nanoTime() + TRANSFER_TIMEOUT_NS
        for (redirect in 0..MAX_REDIRECTS) {
            if (System.nanoTime() >= deadline) throw UpdateFailure("The update download timed out.")
            val connection = uri.toURL().openConnection() as HttpsURLConnection
            try {
                connection.instanceFollowRedirects = false
                connection.connectTimeout = CONNECT_TIMEOUT_MS
                connection.readTimeout = READ_TIMEOUT_MS
                connection.useCaches = false
                connection.setRequestProperty("User-Agent", "OmpDeck-Android-Updater")
                connection.setRequestProperty("Accept", if (url == UpdatePolicy.FEED) "application/vnd.github+json" else "application/octet-stream")
                connection.setRequestProperty("Accept-Encoding", "identity")
                when (connection.responseCode) {
                    HttpURLConnection.HTTP_OK -> {
                        if (connection.contentEncoding?.let { it != "identity" } == true) throw UpdateFailure("The update server returned an unsupported encoding.")
                        val size = connection.contentLengthLong.takeIf { it >= 0 }
                        if (size != null && size > limit) throw UpdateFailure("The update download exceeds its size limit.")
                        connection.inputStream.use { read(it, size, deadline) }
                        return
                    }
                    301, 302, 303, 307, 308 -> {
                        if (redirect == MAX_REDIRECTS) throw UpdateFailure("The update server redirected too many times.")
                        val location = connection.getHeaderField("Location") ?: throw UpdateFailure("The update redirect has no destination.")
                        uri = UpdatePolicy.checkedUrl(uri.resolve(location).toString())
                    }
                    404 -> throw UpdateFailure("No published Android update is available yet.")
                    403, 429 -> throw UpdateFailure("GitHub is limiting update requests. Try again later.")
                    else -> throw UpdateFailure("The update server returned HTTP ${connection.responseCode}.")
                }
            } finally {
                connection.disconnect()
            }
        }
    }

    internal fun copyBounded(
        input: InputStream,
        output: OutputStream,
        limit: Long,
        expected: Long?,
        digest: MessageDigest? = null,
        deadline: Long = Long.MAX_VALUE,
        progress: (Long) -> Unit = {},
    ): Long {
        val buffer = ByteArray(32 * 1024)
        var count = 0L
        while (true) {
            if (System.nanoTime() >= deadline) throw UpdateFailure("The update download timed out.")
            val size = input.read(buffer)
            if (size < 0) break
            if (size == 0) continue
            if (size.toLong() > limit - count) throw UpdateFailure("The update download exceeds its size limit.")
            count += size
            output.write(buffer, 0, size)
            digest?.update(buffer, 0, size)
            progress(count)
        }
        if (expected != null && count != expected) throw UpdateFailure("The update download is incomplete.")
        return count
    }

    fun hex(bytes: ByteArray): String {
        val alphabet = "0123456789abcdef"
        val chars = CharArray(bytes.size * 2)
        for (i in bytes.indices) {
            val value = bytes[i].toInt() and 0xff
            chars[i * 2] = alphabet[value ushr 4]
            chars[i * 2 + 1] = alphabet[value and 15]
        }
        return String(chars)
    }
}
