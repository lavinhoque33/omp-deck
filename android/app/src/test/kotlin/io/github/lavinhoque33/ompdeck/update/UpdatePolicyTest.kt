package io.github.lavinhoque33.ompdeck.update

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.security.MessageDigest

class UpdatePolicyTest {
    private val release = UpdateRelease(UpdateVersion.parse("0.28.0"), 1234)
    private val installed = ApkIdentity(UpdatePolicy.PACKAGE, "0.27.4", 27004, setOf("trusted-certificate"))
    private val archive = ApkIdentity(UpdatePolicy.PACKAGE, "0.28.0", 28000, installed.signers)

    private fun refuses(block: () -> Unit) {
        try { block() } catch (_: UpdateFailure) { return }
        fail("Unsafe update was accepted")
    }

    private fun metadata(): JSONObject = JSONObject()
        .put("tag_name", "v0.28.0").put("draft", false).put("prerelease", false)
        .put("assets", JSONArray()
            .put(JSONObject().put("name", release.apkName).put("size", release.size).put("browser_download_url", release.apkUrl))
            .put(JSONObject().put("name", "${release.apkName}.sha256").put("size", 100).put("browser_download_url", release.checksumUrl)))

    @Test
    fun versionSlotsAreCanonicalMonotonicAndFitAndroid() {
        assertEquals(1_002_003, UpdateVersion.parse("1.2.3").code)
        assertEquals(Int.MAX_VALUE, UpdateVersion.parse("2147.483.647").code)
        for (name in listOf("0.0.0", "0.28", "v0.28.0", "00.28.0", "0.28.0-beta", "0.1000.0", "0.27.1000", "2147.483.648", "99999999999999999999.0.0", "-1.0.0")) {
            refuses { UpdateVersion.parse(name) }
        }
    }

    @Test
    fun onlyExactStableRepositoryVersionAssetsAreAccepted() {
        assertEquals(release, UpdatePolicy.parseRelease(metadata(), installed.versionCode))
        refuses { UpdatePolicy.parseRelease(metadata().put("prerelease", true), installed.versionCode) }
        refuses { UpdatePolicy.parseRelease(metadata().put("draft", true), installed.versionCode) }
        refuses { UpdatePolicy.parseRelease(metadata().put("tag_name", "0.28.0"), installed.versionCode) }
        refuses { UpdatePolicy.parseRelease(metadata().put("assets", JSONArray()), installed.versionCode) }
        for (url in listOf(release.apkUrl.replace("lavinhoque33", "attacker"), release.apkUrl.replace("v0.28.0/", "v0.27.4/"), release.apkUrl + "?redirect=elsewhere")) {
            val json = metadata()
            json.getJSONArray("assets").getJSONObject(0).put("browser_download_url", url)
            refuses { UpdatePolicy.parseRelease(json, installed.versionCode) }
        }
        for (size in listOf<Any>(0, UpdatePolicy.MAX_APK_BYTES + 1, "1234", 1234.5)) {
            val json = metadata()
            json.getJSONArray("assets").getJSONObject(0).put("size", size)
            refuses { UpdatePolicy.parseRelease(json, installed.versionCode) }
        }
        val duplicate = metadata()
        duplicate.getJSONArray("assets").put(duplicate.getJSONArray("assets").getJSONObject(0))
        refuses { UpdatePolicy.parseRelease(duplicate, installed.versionCode) }
        val checksum = metadata()
        checksum.getJSONArray("assets").getJSONObject(1).put("browser_download_url", release.checksumUrl.replace("github.com", "evil.example"))
        refuses { UpdatePolicy.parseRelease(checksum, installed.versionCode) }
    }

    @Test
    fun currentOrOlderReleaseNeedsNoApkButNewerReleaseStillRequiresIt() {
        val withoutApk = metadata().put("assets", JSONArray())
        assertNull(UpdatePolicy.parseRelease(withoutApk, 28000))
        assertNull(UpdatePolicy.parseRelease(withoutApk, 29000))
        refuses { UpdatePolicy.parseRelease(withoutApk, 27005) }
    }

    @Test
    fun redirectsStayOnBoundedHttpsGitHubHostsWithoutCredentialsOrOtherPorts() {
        for (url in listOf(UpdatePolicy.FEED, release.apkUrl, "https://objects.githubusercontent.com/path?token=x", "https://release-assets.githubusercontent.com/path?signature=x", "https://github.com:443/path")) {
            assertEquals(url, UpdatePolicy.checkedUrl(url).toString())
        }
        for (url in listOf("http://github.com/path", "https://github.com.evil.example/path", "https://github.com@evil.example/path", "https://user@github.com/path", "https://github.com:444/path", "https://github.com./path", "https://raw.githubusercontent.com/path", "https://127.0.0.1/path", "file:///tmp/update.apk", "https://github.com/path#fragment")) {
            refuses { UpdatePolicy.checkedUrl(url) }
        }
    }

    @Test
    fun checksumMustBeOneDigestForThisApkAndMustMatchBytes() {
        val hash = "ab".repeat(32)
        assertEquals(hash, UpdatePolicy.checksum(hash.uppercase() + "  ${release.apkName}\n", release.apkName))
        assertEquals(hash, UpdatePolicy.checksum(hash + " *${release.apkName}\r\n", release.apkName))
        assertEquals(hash, UpdatePolicy.checksum(hash, release.apkName))
        for (text in listOf("ab", "$hash  other.apk", "$hash\n$hash", "$hash  ../${release.apkName}", " $hash", "$hash trailing")) {
            refuses { UpdatePolicy.checksum(text, release.apkName) }
        }
        UpdatePolicy.verifyDigest(hash, hash)
        refuses { UpdatePolicy.verifyDigest(hash, "cd".repeat(32)) }
    }

    @Test
    fun actualPackageVersionAndCurrentSignerSetMustAllMatch() {
        UpdatePolicy.verifyIdentity(release, installed, archive)
        refuses { UpdatePolicy.verifyIdentity(release, installed, archive.copy(packageName = "other.package")) }
        refuses { UpdatePolicy.verifyIdentity(release, installed, archive.copy(versionCode = 28001)) }
        refuses { UpdatePolicy.verifyIdentity(release, installed, archive.copy(versionName = "0.28.0-beta")) }
        refuses { UpdatePolicy.verifyIdentity(release, installed, archive.copy(signers = setOf("unrelated-certificate"))) }
        refuses { UpdatePolicy.verifyIdentity(release, installed, archive.copy(signers = installed.signers + "extra-signer")) }
        refuses { UpdatePolicy.verifyIdentity(release, installed.copy(signers = emptySet()), archive.copy(signers = emptySet())) }
        refuses { UpdatePolicy.verifyIdentity(release, installed.copy(versionCode = 28000), archive) }
        refuses { UpdatePolicy.verifyIdentity(release, installed.copy(versionCode = 29000), archive) }
    }

    @Test
    fun streamsRejectOversizeAndTruncationBeforeExposingAnApk() {
        val bytes = ByteArray(80_000) { (it % 251).toByte() }
        val output = ByteArrayOutputStream()
        val progress = mutableListOf<Long>()
        val digest = MessageDigest.getInstance("SHA-256")
        assertEquals(bytes.size.toLong(), UpdateTransport.copyBounded(ByteArrayInputStream(bytes), output, bytes.size.toLong(), bytes.size.toLong(), digest, progress = { progress.add(it) }))
        assertTrue(bytes.contentEquals(output.toByteArray()))
        assertEquals(UpdateTransport.hex(MessageDigest.getInstance("SHA-256").digest(bytes)), UpdateTransport.hex(digest.digest()))
        assertTrue(progress.zipWithNext().all { (a, b) -> a < b })
        assertTrue(progress.all { it in 1..bytes.size.toLong() })
        refuses { UpdateTransport.copyBounded(ByteArrayInputStream(bytes), ByteArrayOutputStream(), 79_999, null) }
        refuses { UpdateTransport.copyBounded(ByteArrayInputStream(bytes), ByteArrayOutputStream(), 80_001, 80_001) }
        refuses { UpdateTransport.copyBounded(ByteArrayInputStream(bytes), ByteArrayOutputStream(), 80_000, null, deadline = System.nanoTime() - 1) }
    }

    @Test
    fun automaticChecksRequireEnabledForegroundIdleAndSixHoursSinceAnyAttempt() {
        assertTrue(UpdatePolicy.automaticCheckDue(true, true, false, null, 0))
        assertFalse(UpdatePolicy.automaticCheckDue(false, true, false, null, 0))
        assertFalse(UpdatePolicy.automaticCheckDue(true, false, false, null, 0))
        assertFalse(UpdatePolicy.automaticCheckDue(true, true, true, null, 0))
        val interval = UpdatePolicy.CHECK_INTERVAL_MS
        assertFalse(UpdatePolicy.automaticCheckDue(true, true, false, 7, interval + 6))
        assertTrue(UpdatePolicy.automaticCheckDue(true, true, false, 7, interval + 7))
    }

    @Test
    fun consentSurvivesRotationButNotBackgroundingAndIsConsumedOnce() {
        val consent = InstallConsent()
        assertFalse(consent.take(true)) // A check or restored cached APK cannot open installer UI.
        consent.request()
        assertFalse(consent.take(false))
        consent.leave(true)
        assertTrue(consent.take(true))
        assertFalse(consent.take(true))
        consent.request()
        consent.leave(false)
        assertFalse(consent.take(true))
        consent.request() // A new user action retries after cancellation/denial.
        consent.clear()
        assertFalse(consent.take(true))
        consent.request()
        assertTrue(consent.take(true))
    }
}
