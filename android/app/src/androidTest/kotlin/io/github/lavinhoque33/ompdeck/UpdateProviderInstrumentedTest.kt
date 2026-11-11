package io.github.lavinhoque33.ompdeck

import androidx.core.content.FileProvider
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.File
import java.util.UUID

class UpdateProviderInstrumentedTest {
    @Test
    fun updaterProviderExposesOnlyCompletedApksAndRemainsNonExported() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val authority = "${context.packageName}.updates"
        val provider = context.packageManager.resolveContentProvider(authority, 0)!!
        assertFalse(provider.exported)
        assertTrue(provider.grantUriPermissions)
        val directory = File(context.cacheDir, "updater/apks")
        assertTrue(directory.isDirectory || directory.mkdirs())
        val apk = File(directory, "provider-${UUID.randomUUID()}.apk")
        try {
            apk.writeBytes(byteArrayOf(1, 2, 3))
            val uri = FileProvider.getUriForFile(context, authority, apk)
            assertEquals("content", uri.scheme)
            assertEquals(authority, uri.authority)
            context.contentResolver.openInputStream(uri)!!.use { assertTrue(byteArrayOf(1, 2, 3).contentEquals(it.readBytes())) }
            for (file in listOf(
                File(context.cacheDir, "updater/download.part"),
                File(context.cacheDir, "updater/verified.json"),
                File(context.cacheDir, "captures/photo.jpg"),
                File(context.filesDir, "pairing.json"),
                File(directory, "../verified.json"),
            )) {
                try {
                    FileProvider.getUriForFile(context, authority, file)
                    fail("Updater provider exposed $file")
                } catch (_: IllegalArgumentException) {
                    // Canonical path containment must reject private records, partials and unrelated app data.
                }
            }
            try {
                FileProvider.getUriForFile(context, "${context.packageName}.captures", apk)
                fail("Capture provider exposed an update APK")
            } catch (_: IllegalArgumentException) {
                // Providers must have separate path scopes as well as separate authorities.
            }
        } finally {
            apk.delete()
        }
    }
}
