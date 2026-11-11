package io.github.lavinhoque33.ompdeck.push

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import com.google.android.gms.tasks.Tasks
import com.google.firebase.FirebaseApp
import com.google.firebase.FirebaseOptions
import com.google.firebase.messaging.FirebaseMessaging
import io.github.lavinhoque33.ompdeck.bridge.BridgeException
import java.io.File
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException

/**
 * Process-wide push state: the sealed store, the notifier and processor the FCM service uses, and Firebase itself,
 * which exists only at runtime (the user imports a google-services.json; nothing is compiled in). It is reachable
 * without any Activity, so a push arriving while the UI is closed uses exactly the same objects.
 */
class PushRuntime private constructor(private val app: Context) {
    val store = PushStore(File(app.noBackupFilesDir, STATE_FILE), KeystoreSecretBox())
    val notifier = Notifier(app, store)
    val processor = PushProcessor(store, Presence.shared, notifier)

    @Volatile
    private var firebaseReady = false

    /** Starts the default FirebaseApp from the stored config (no-op without one); called from Application.onCreate. */
    @Synchronized
    fun bootFirebase(): Boolean {
        if (firebaseReady) return true
        val config = try {
            store.config()
        } catch (e: Exception) {
            Log.w(TAG, "push state unavailable: ${e.javaClass.simpleName}")
            return false
        } ?: return false
        return startFirebase(config)
    }

    private fun startFirebase(config: FirebaseConfig): Boolean = try {
        if (FirebaseApp.getApps(app).none { it.name == FirebaseApp.DEFAULT_APP_NAME }) {
            val options = FirebaseOptions.Builder()
                .setApiKey(config.apiKey)
                .setApplicationId(config.appId)
                .setProjectId(config.projectId)
                .setGcmSenderId(config.senderId)
                .apply { config.storageBucket?.let { setStorageBucket(it) } }
                .build()
            FirebaseApp.initializeApp(app, options)
        }
        firebaseReady = true
        true
    } catch (e: Exception) {
        Log.w(TAG, "firebase init failed: ${e.javaClass.simpleName}")
        false
    }

    @Synchronized
    fun isConfigured(): Boolean = firebaseReady || bootFirebase()

    /** Validates, starts and then stores [json] (a google-services.json). Refused while a config is active: [resetConfig] first. */
    @Synchronized
    fun importConfig(json: String): FirebaseConfig {
        if (firebaseReady || store.config() != null) throw BridgeException("firebase_already_configured")
        val config = GoogleServices.parse(json, app.packageName)
        if (!startFirebase(config)) throw ConfigException("Firebase rejected the configuration.")
        store.setConfig(config)
        return config
    }

    /** Drops the imported config (and its token) so another google-services.json can be imported; desktop keys are kept. */
    @Synchronized
    fun resetConfig() {
        store.clearConfig()
        try {
            FirebaseApp.getInstance().delete()
        } catch (_: IllegalStateException) {
            // No default app was running.
        }
        firebaseReady = false
        PushEvents.changed()
    }

    /** The current FCM token, fetched from Firebase (blocks up to [TOKEN_TIMEOUT_S]; never call on the main thread) and stored. */
    fun fetchToken(): String {
        if (!isConfigured()) throw BridgeException("firebase_not_configured")
        val token = try {
            Tasks.await(FirebaseMessaging.getInstance().token, TOKEN_TIMEOUT_S, TimeUnit.SECONDS)
        } catch (_: ExecutionException) {
            throw BridgeException("token_unavailable")
        } catch (_: TimeoutException) {
            throw BridgeException("token_unavailable")
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
            throw BridgeException("token_unavailable")
        }
        if (!validToken(token)) throw BridgeException("token_unavailable")
        if (store.setToken(token)) PushEvents.changed()
        return token
    }

    /** Called by the FCM service when Firebase issues a new token (possibly with no UI alive). */
    fun onNewToken(token: String) {
        if (!validToken(token)) return
        try {
            if (store.setToken(token)) PushEvents.changed()
        } catch (e: Exception) {
            Log.w(TAG, "could not store token: ${e.javaClass.simpleName}")
        }
    }

    /** `'granted'` when notifications can be shown, `'default'` before the user was asked (Android 13+), else `'denied'`. */
    fun permission(): String {
        if (notifier.canNotify()) return "granted"
        val needsPrompt = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            app.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED &&
            !store.permissionAsked()
        return if (needsPrompt) "default" else "denied"
    }

    /** True when the system notification setting for the app is off even though the permission is granted. */
    fun notificationsBlockedBySettings(): Boolean = !NotificationManagerCompat.from(app).areNotificationsEnabled()

    companion object {
        private const val TAG = "OmpDeckPush"
        private const val STATE_FILE = "push-state.bin"
        private const val TOKEN_TIMEOUT_S = 20L
        private val TOKEN = Regex("[A-Za-z0-9:_.-]{20,4096}")

        fun validToken(token: String): Boolean = TOKEN.matches(token)

        @Volatile
        private var instance: PushRuntime? = null

        fun get(context: Context): PushRuntime =
            instance ?: synchronized(this) { instance ?: PushRuntime(context.applicationContext).also { instance = it } }
    }
}
