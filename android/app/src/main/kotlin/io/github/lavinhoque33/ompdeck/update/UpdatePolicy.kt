package io.github.lavinhoque33.ompdeck.update

import org.json.JSONObject
import java.io.IOException
import java.net.URI
import java.security.MessageDigest
import java.util.Locale

internal class UpdateFailure(message: String) : IOException(message)

internal data class UpdateVersion(val name: String, val code: Int) {
    companion object {
        fun parse(name: String): UpdateVersion {
            val match = Regex("(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)").matchEntire(name)
                ?: throw UpdateFailure("The release version is invalid.")
            val parts = match.groupValues.drop(1).map { it.toLongOrNull() ?: throw UpdateFailure("The release version is out of range.") }
            // Minor/patch must fit their versionCode slots; otherwise different versions collide.
            if (parts[0] > 2147 || parts[1] > 999 || parts[2] > 999) throw UpdateFailure("The release version is out of range.")
            val code = parts[0] * 1_000_000L + parts[1] * 1_000L + parts[2]
            if (code !in 1..Int.MAX_VALUE.toLong()) throw UpdateFailure("The release version is out of range.")
            return UpdateVersion(name, code.toInt())
        }
    }
}

internal data class UpdateRelease(val version: UpdateVersion, val size: Long) {
    val apkName: String get() = "OmpDeck-${version.name}.apk"
    val apkUrl: String get() = "https://github.com/lavinhoque33/omp-deck/releases/download/v${version.name}/$apkName"
    val checksumUrl: String get() = "$apkUrl.sha256"
}

internal data class ApkIdentity(val packageName: String, val versionName: String?, val versionCode: Long, val signers: Set<String>)

internal object UpdatePolicy {
    const val FEED = "https://api.github.com/repos/lavinhoque33/omp-deck/releases/latest"
    const val PACKAGE = "io.github.lavinhoque33.ompdeck"
    const val MAX_APK_BYTES = 128L * 1024 * 1024
    const val MAX_METADATA_BYTES = 1024L * 1024
    const val MAX_CHECKSUM_BYTES = 4096L
    const val CHECK_INTERVAL_MS = 6L * 60 * 60 * 1000
    private val hosts = setOf("api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com")

    fun checkedUrl(value: String): URI {
        val uri = try { URI(value) } catch (_: Exception) { throw UpdateFailure("The update URL is invalid.") }
        if (uri.scheme != "https" || uri.host?.lowercase(Locale.ROOT) !in hosts || uri.rawUserInfo != null ||
            (uri.port != -1 && uri.port != 443) || uri.rawFragment != null || uri.isOpaque) {
            throw UpdateFailure("The update server redirected outside trusted HTTPS hosts.")
        }
        return uri
    }

    fun parseRelease(json: JSONObject, currentCode: Long): UpdateRelease? {
        if (json.opt("draft") != false || json.opt("prerelease") != false) throw UpdateFailure("The latest release is not a stable release.")
        val tag = json.opt("tag_name") as? String ?: throw UpdateFailure("The release has no version tag.")
        if (!tag.startsWith('v')) throw UpdateFailure("The release version tag is invalid.")
        val version = UpdateVersion.parse(tag.substring(1))
        if (version.code <= currentCode) return null
        val assets = json.optJSONArray("assets") ?: throw UpdateFailure("The release has no Android assets.")
        var release: UpdateRelease? = null
        var checksumFound = false
        for (i in 0 until assets.length()) {
            val asset = assets.optJSONObject(i) ?: continue
            when (asset.opt("name")) {
                "OmpDeck-${version.name}.apk" -> {
                    if (release != null) throw UpdateFailure("The release contains duplicate APK assets.")
                    val size = exactLong(asset.opt("size"))
                    if (size !in 1..MAX_APK_BYTES) throw UpdateFailure("The APK size is invalid or exceeds 128 MiB.")
                    val candidate = UpdateRelease(version, size)
                    if (asset.opt("browser_download_url") != candidate.apkUrl) throw UpdateFailure("The APK does not belong to this repository and version.")
                    release = candidate
                }
                "OmpDeck-${version.name}.apk.sha256" -> {
                    if (checksumFound) throw UpdateFailure("The release contains duplicate checksum assets.")
                    if (exactLong(asset.opt("size")) !in 1..MAX_CHECKSUM_BYTES) throw UpdateFailure("The checksum file is too large or empty.")
                    val expected = UpdateRelease(version, 1).checksumUrl
                    if (asset.opt("browser_download_url") != expected) throw UpdateFailure("The checksum does not belong to this repository and version.")
                    checksumFound = true
                }
            }
        }
        return release?.takeIf { checksumFound } ?: throw UpdateFailure("The release is missing its APK or SHA-256 checksum.")
    }

    fun exactLong(value: Any?): Long {
        if (value !is Byte && value !is Short && value !is Int && value !is Long) throw UpdateFailure("The release size is invalid.")
        return (value as Number).toLong()
    }

    fun checksum(text: String, apkName: String): String {
        val line = text.trimEnd('\r', '\n')
        val match = Regex("([0-9a-fA-F]{64})(?: [ *]([^\\r\\n]+))?").matchEntire(line)
            ?: throw UpdateFailure("The published SHA-256 checksum is invalid.")
        val name = match.groupValues[2]
        if (name.isNotEmpty() && name != apkName) throw UpdateFailure("The checksum names a different APK.")
        return match.groupValues[1].lowercase(Locale.ROOT)
    }

    fun verifyDigest(expected: String, actual: String) {
        if (!MessageDigest.isEqual(expected.toByteArray(Charsets.US_ASCII), actual.toByteArray(Charsets.US_ASCII))) {
            throw UpdateFailure("The APK SHA-256 checksum does not match. The download was removed.")
        }
    }

    fun verifyIdentity(release: UpdateRelease, installed: ApkIdentity, archive: ApkIdentity) {
        if (installed.packageName != PACKAGE || archive.packageName != PACKAGE) throw UpdateFailure("The APK belongs to a different application.")
        if (release.version.code.toLong() <= installed.versionCode) throw UpdateFailure("Only a newer app version can be installed.")
        if (archive.versionCode != release.version.code.toLong() || archive.versionName != release.version.name) {
            throw UpdateFailure("The APK version does not match the release.")
        }
        // Deliberately compare current signers, not signing history: rotated/unrelated keys are not accepted.
        if (installed.signers.isEmpty() || archive.signers != installed.signers) throw UpdateFailure("The APK signing certificate does not match this installed app.")
    }

    fun automaticCheckDue(enabled: Boolean, foreground: Boolean, busy: Boolean, lastAttempt: Long?, now: Long): Boolean =
        enabled && foreground && !busy && (lastAttempt == null || now - lastAttempt >= CHECK_INTERVAL_MS)
}

/** Only an explicit install action authorizes a later installer UI. Backgrounding revokes it; rotation does not. */
internal class InstallConsent {
    var pending = false
        private set
    fun request() { pending = true }
    fun leave(changingConfigurations: Boolean) { if (!changingConfigurations) pending = false }
    fun take(resumed: Boolean): Boolean {
        if (!pending || !resumed) return false
        pending = false
        return true
    }
    fun clear() { pending = false }
}
