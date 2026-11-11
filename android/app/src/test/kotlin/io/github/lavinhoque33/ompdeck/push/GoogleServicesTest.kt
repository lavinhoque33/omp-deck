package io.github.lavinhoque33.ompdeck.push

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.fail
import org.junit.Test

class GoogleServicesTest {
    private val pkg = "io.github.lavinhoque33.ompdeck"

    private fun client(packageName: String, appId: String = "1:123456789012:android:0123456789abcdef", key: String? = "AIzaSyA-0123456789abcdefghijklmnopqrstuv") =
        """{"client_info":{"mobilesdk_app_id":"$appId","android_client_info":{"package_name":"$packageName"}},
            "api_key":[${if (key == null) "" else """{"current_key":"$key"}"""}]}"""

    private fun file(vararg clients: String, projectId: String = "ompdeck-android", number: String = "123456789012", extra: String = "") =
        """{"project_info":{"project_number":"$number","project_id":"$projectId","storage_bucket":"$projectId.firebasestorage.app"$extra},
            "client":[${clients.joinToString(",")}],"configuration_version":"1"}"""

    private fun failure(text: String): String {
        try {
            GoogleServices.parse(text, pkg)
        } catch (e: ConfigException) {
            return e.message!!
        }
        fail("accepted")
        return ""
    }

    @Test
    fun readsTheMatchingAndroidClient() {
        val config = GoogleServices.parse(
            file(
                client("com.other.app", appId = "1:123456789012:android:aaaaaaaaaaaaaaaa", key = "AIzaOtherAppKey0123456789"),
                client(pkg),
            ),
            pkg,
        )
        assertEquals("ompdeck-android", config.projectId)
        assertEquals("123456789012", config.senderId)
        assertEquals("1:123456789012:android:0123456789abcdef", config.appId)
        assertEquals("AIzaSyA-0123456789abcdefghijklmnopqrstuv", config.apiKey)
        assertEquals("ompdeck-android.firebasestorage.app", config.storageBucket)
    }

    @Test
    fun storageBucketIsOptional() {
        val text = """{"project_info":{"project_number":"123456789012","project_id":"ompdeck-android"},"client":[${client(pkg)}]}"""
        assertNull(GoogleServices.parse(text, pkg).storageBucket)
    }

    @Test
    fun refusesAFileForAnotherApp() {
        assertEquals("The file is not for the package $pkg.", failure(file(client("com.other.app"))))
        assertEquals("The file has no Android app.", failure(file()))
        assertEquals("The file has no Android app (client).", failure("""{"project_info":{"project_number":"123456789012","project_id":"ompdeck-android"}}"""))
    }

    @Test
    fun refusesSplicedOrMalformedValues() {
        // The app id embeds the project number: a client from another project spliced in does not match.
        failure(file(client(pkg, appId = "1:999999999999:android:0123456789abcdef")))
        failure(file(client(pkg, appId = "not-an-app-id")))
        failure(file(client(pkg), number = "12"))
        failure(file(client(pkg), projectId = "Bad Project"))
        failure(file(client(pkg), projectId = "x"))
        failure(file(client(pkg, key = null)))
        failure(file(client(pkg, key = "short")))
        failure(file(client(pkg, key = "AIza has spaces in it 0123456789")))
        failure("""{"project_info":{"project_number":"123456789012","project_id":"ompdeck-android","storage_bucket":"UPPER CASE"},"client":[${client(pkg)}]}""")
    }

    @Test
    fun refusesGarbageAndOversizedInput() {
        failure("not json")
        failure("[]")
        failure("{}")
        failure(" ".repeat(GoogleServices.MAX_BYTES + 1))
    }
}
