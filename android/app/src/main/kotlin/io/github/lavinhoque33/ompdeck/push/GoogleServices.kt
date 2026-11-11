package io.github.lavinhoque33.ompdeck.push

import org.json.JSONException
import org.json.JSONObject

/** The runtime Firebase options, from the user's google-services.json. Nothing here is compiled into the app. */
data class FirebaseConfig(
    val apiKey: String,
    val appId: String,
    val projectId: String,
    val senderId: String,
    val storageBucket: String?,
)

/** A google-services.json that cannot be used; [message] is shown to the user. */
class ConfigException(message: String) : Exception(message)

object GoogleServices {
    const val MAX_BYTES = 256 * 1024

    private val PROJECT_ID = Regex("[a-z][a-z0-9-]{3,28}[a-z0-9]")
    private val PROJECT_NUMBER = Regex("[0-9]{6,20}")
    private val APP_ID = Regex("1:[0-9]{6,20}:android:[0-9a-f]{8,64}")
    private val API_KEY = Regex("[A-Za-z0-9_-]{20,128}")
    private val BUCKET = Regex("[a-z0-9._-]{3,222}")

    /** Picks the Android client of [packageName] out of [text]; throws [ConfigException] when the file is not for this app. */
    fun parse(text: String, packageName: String): FirebaseConfig {
        if (text.length > MAX_BYTES) throw ConfigException("The file is too large to be a google-services.json.")
        val root = try {
            JSONObject(text)
        } catch (_: JSONException) {
            throw ConfigException("The file is not valid JSON.")
        }
        val info = root.optJSONObject("project_info") ?: throw ConfigException("project_info is missing.")
        val projectId = info.optString("project_id")
        val projectNumber = info.optString("project_number")
        if (!PROJECT_ID.matches(projectId)) throw ConfigException("project_id is missing or malformed.")
        if (!PROJECT_NUMBER.matches(projectNumber)) throw ConfigException("project_number is missing or malformed.")
        val bucket = info.optString("storage_bucket").takeIf { it.isNotEmpty() }
        if (bucket != null && !BUCKET.matches(bucket)) throw ConfigException("storage_bucket is malformed.")

        val clients = root.optJSONArray("client") ?: throw ConfigException("The file has no Android app (client).")
        var match: JSONObject? = null
        var seenPackages = 0
        for (i in 0 until clients.length()) {
            val client = clients.optJSONObject(i) ?: continue
            val pkg = client.optJSONObject("client_info")?.optJSONObject("android_client_info")?.optString("package_name") ?: continue
            seenPackages++
            if (pkg == packageName) {
                match = client
                break
            }
        }
        if (match == null) {
            throw ConfigException(
                if (seenPackages == 0) "The file has no Android app." else "The file is not for the package $packageName.",
            )
        }
        val appId = match.optJSONObject("client_info")?.optString("mobilesdk_app_id").orEmpty()
        if (!APP_ID.matches(appId)) throw ConfigException("mobilesdk_app_id is missing or malformed.")
        // The app id embeds the project number: a mismatch means a spliced file.
        if (appId.split(':')[1] != projectNumber) throw ConfigException("The app id does not belong to this project.")

        var apiKey: String? = null
        val keys = match.optJSONArray("api_key")
        if (keys != null) {
            for (k in 0 until keys.length()) {
                val candidate = keys.optJSONObject(k)?.optString("current_key").orEmpty()
                if (API_KEY.matches(candidate)) {
                    apiKey = candidate
                    break
                }
            }
        }
        if (apiKey == null) throw ConfigException("The file has no usable API key.")
        return FirebaseConfig(apiKey, appId, projectId, projectNumber, bucket)
    }
}
