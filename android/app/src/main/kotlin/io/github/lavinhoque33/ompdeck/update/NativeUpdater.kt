package io.github.lavinhoque33.ompdeck.update

import android.content.Context
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import io.github.lavinhoque33.ompdeck.BuildConfig
import org.json.JSONObject
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.lang.ref.WeakReference
import java.security.MessageDigest
import java.util.concurrent.Executors

internal data class NativeUpdateState(
    val current: String = BuildConfig.VERSION_NAME,
    val latest: String? = null,
    val phase: String = "idle",
    val downloaded: Long = 0,
    val total: Long? = null,
    val checkedUnixMs: Long? = null,
    val autoCheck: Boolean = true,
    val error: String? = null,
) {
    fun json(): JSONObject = JSONObject()
        .put("current", current)
        .put("latest", latest ?: JSONObject.NULL)
        .put("phase", phase)
        .put("downloaded", downloaded)
        .put("total", total ?: JSONObject.NULL)
        .put("checked_unix_ms", checkedUnixMs ?: JSONObject.NULL)
        .put("auto_check", autoCheck)
        .put("error", error ?: JSONObject.NULL)
}

/** Main-thread state machine, one application-owned I/O worker, and only a weak reference to the foreground UI. */
internal class NativeUpdater private constructor(context: Context) {
    interface Listener {
        fun updateChanged(state: NativeUpdateState)
        fun installReady(file: File)
    }

    private data class Verified(val release: UpdateRelease, val sha256: String, val file: File)
    private val app = context.applicationContext
    private val main = Handler(Looper.getMainLooper())
    private val io = Executors.newSingleThreadExecutor()
    private val root by lazy { File(app.cacheDir, "updater") }
    private val apks by lazy { File(root, "apks") }
    private val record by lazy { File(root, "verified.json") }
    private val partial by lazy { File(root, "download.part") }
    private val consent = InstallConsent()
    private var listener = WeakReference<Listener>(null)
    private var resumed = false
    private var initialized = false
    private var pendingCheck = false
    private var autoOverride: Boolean? = null
    private var busy = false
    private var lastAttempt: Long? = null
    private var release: UpdateRelease? = null
    private var verified: Verified? = null
    private var state = NativeUpdateState()
    private val automatic = Runnable { scheduleAutomatic() }

    init {
        io.execute {
            val prefs = app.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
            val enabled = prefs.getBoolean("auto_check", true)
            val checked = prefs.getLong("checked_unix_ms", -1L).takeIf { it >= 0 }
            val cached = restoreCache()
            val remembered = cached?.release ?: rememberedRelease(prefs.getString("last_release", null))
            main.post {
                initialized = true
                verified = cached
                release = remembered
                state = state.copy(
                    autoCheck = autoOverride ?: enabled,
                    checkedUnixMs = checked,
                    latest = remembered?.version?.name,
                    phase = when {
                        state.phase == "installing" -> "installing"
                        cached != null -> "ready"
                        remembered != null -> "available"
                        else -> "idle"
                    },
                    downloaded = cached?.release?.size ?: 0,
                    total = cached?.release?.size,
                )
                publish()
                when {
                    consent.pending -> startInstall()
                    pendingCheck -> { pendingCheck = false; check() }
                    else -> scheduleAutomatic()
                }
            }
        }
    }

    fun snapshot(): JSONObject = state.json()

    /** Rehydrated Activity Result state is an existing explicit action, never a reason to start a new one. */
    fun holdExternalInteraction() {
        state = state.copy(phase = "installing")
        main.removeCallbacks(automatic)
    }

    fun attach(target: Listener) {
        listener = WeakReference(target)
        resumed = false
        target.updateChanged(state)
        scheduleAutomatic()
    }

    fun resume(target: Listener) {
        if (listener.get() !== target) return
        resumed = true
        deliverInstallation()
    }

    fun pause(target: Listener) {
        if (listener.get() === target) resumed = false
    }

    fun detach(target: Listener, changingConfigurations: Boolean) {
        if (listener.get() !== target) return
        listener.clear()
        resumed = false
        consent.leave(changingConfigurations)
        main.removeCallbacks(automatic)
    }

    fun configure(enabled: Boolean): JSONObject {
        autoOverride = enabled
        state = state.copy(autoCheck = enabled)
        io.execute { app.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit().putBoolean("auto_check", enabled).commit() }
        publish()
        scheduleAutomatic()
        return snapshot()
    }

    /** Manual and scheduled checks coalesce, and neither may replace an in-flight download/installer. */
    fun check(): JSONObject {
        if (!initialized) { pendingCheck = true; return snapshot() }
        if (busy || state.phase == "installing") return snapshot()
        busy = true
        lastAttempt = SystemClock.elapsedRealtime()
        main.removeCallbacks(automatic)
        state = state.copy(phase = "checking", error = null)
        publish()
        val old = verified
        io.execute {
            try {
                val candidate = UpdatePolicy.parseRelease(
                    JSONObject(UpdateTransport.text(UpdatePolicy.FEED, UpdatePolicy.MAX_METADATA_BYTES)),
                    BuildConfig.VERSION_CODE.toLong()
                )
                val retained = old?.takeIf { it.release == candidate && it.file.isFile }
                if (retained == null) clearCache()
                val checked = System.currentTimeMillis().coerceAtLeast(0)
                val editor = app.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit().putLong("checked_unix_ms", checked)
                if (candidate == null) editor.remove("last_release") else {
                    editor.putString("last_release", JSONObject().put("version", candidate.version.name).put("size", candidate.size).toString())
                }
                editor.commit()
                main.post {
                    busy = false
                    release = candidate
                    verified = retained
                    state = state.copy(
                        latest = candidate?.version?.name,
                        phase = when { retained != null -> "ready"; candidate != null -> "available"; else -> "idle" },
                        downloaded = retained?.release?.size ?: 0,
                        total = retained?.release?.size,
                        checkedUnixMs = checked,
                        error = null,
                    )
                    publish()
                    if (consent.pending && candidate != null) startInstall() else { consent.clear(); scheduleAutomatic() }
                }
            } catch (e: Exception) {
                main.post { failed(e, "Could not check for an Android update. Check your connection and retry.") }
            }
        }
        return snapshot()
    }

    /** Called only by the bridge's explicit install action, or a saved result of its unknown-source settings flow. */
    fun install(): JSONObject {
        if (state.phase == "installing") return snapshot()
        consent.request()
        if (!initialized || busy) return snapshot()
        startInstall()
        return snapshot()
    }

    private fun startInstall() {
        if (!initialized || busy) return
        val candidate = release
        if (candidate == null) { check(); return }
        busy = true
        main.removeCallbacks(automatic)
        state = state.copy(phase = "downloading", downloaded = 0, total = candidate.size, error = null)
        publish()
        val cached = verified?.takeIf { it.release == candidate }
        io.execute {
            try {
                val result = if (cached != null) {
                    verify(cached)
                    cached
                } else {
                    if (!apks.isDirectory && !apks.mkdirs()) throw UpdateFailure("The private update cache is unavailable.")
                    partial.delete()
                    val expected = UpdatePolicy.checksum(UpdateTransport.text(candidate.checksumUrl, UpdatePolicy.MAX_CHECKSUM_BYTES), candidate.apkName)
                    var reported = 0L
                    val hash = FileOutputStream(partial).use { output ->
                        UpdateTransport.apk(candidate, output) { count ->
                            // Avoid flooding the WebView; final bytes are always published on completion.
                            val now = SystemClock.elapsedRealtime()
                            if (now - reported >= 150) {
                                reported = now
                                main.post {
                                    state = state.copy(downloaded = count.coerceIn(0, candidate.size))
                                    publish()
                                }
                            }
                        }.also { output.fd.sync() }
                    }
                    UpdatePolicy.verifyDigest(expected, hash)
                    verifyPackage(candidate, partial)
                    val destination = File(apks, candidate.apkName)
                    if (!partial.renameTo(destination)) throw UpdateFailure("The verified APK could not be saved.")
                    Verified(candidate, expected, destination).also { saveRecord(it) }
                }
                main.post {
                    busy = false
                    verified = result
                    state = state.copy(phase = "ready", downloaded = candidate.size, total = candidate.size, error = null)
                    publish()
                    deliverInstallation()
                    scheduleAutomatic()
                }
            } catch (e: Exception) {
                clearCache()
                main.post { verified = null; failed(e, "Could not download or verify the Android update. Retry when connected.") }
            }
        }
    }

    private fun deliverInstallation() {
        val target = listener.get() ?: return
        val cached = verified ?: return
        if (state.phase != "ready" || !consent.take(resumed)) return
        state = state.copy(phase = "installing", error = null)
        publish()
        target.installReady(cached.file)
    }

    fun awaitingPermission() {
        state = state.copy(error = "Allow OmpDeck to install app updates in Android settings, then return here. Android will still ask you to confirm installation.")
        publish()
    }

    fun permissionReturned(allowed: Boolean) {
        installationFinished(if (allowed) null else "Permission to install app updates was not granted. Tap Install to retry when ready.")
        if (allowed) install()
    }

    fun installationFinished(message: String?) {
        consent.clear()
        state = state.copy(phase = if (verified != null) "ready" else if (release != null) "available" else "idle", error = message)
        publish()
        scheduleAutomatic()
    }

    private fun failed(error: Exception, fallback: String) {
        busy = false
        consent.clear()
        // Keep the last known release and any verified APK; a failed refresh must not hide an available update.
        state = state.copy(phase = "error", error = if (error is UpdateFailure) error.message else fallback)
        publish()
        scheduleAutomatic()
    }

    private fun publish() { listener.get()?.updateChanged(state) }

    private fun scheduleAutomatic() {
        main.removeCallbacks(automatic)
        if (!initialized || !state.autoCheck || listener.get() == null || busy || state.phase == "installing") return
        val now = SystemClock.elapsedRealtime()
        if (UpdatePolicy.automaticCheckDue(state.autoCheck, true, busy, lastAttempt, now)) {
            check()
        } else {
            val remaining = (UpdatePolicy.CHECK_INTERVAL_MS - (now - (lastAttempt ?: now))).coerceAtLeast(1)
            main.postDelayed(automatic, remaining)
        }
    }

    private fun rememberedRelease(value: String?): UpdateRelease? {
        if (value == null || value.length > UpdatePolicy.MAX_CHECKSUM_BYTES) return null
        return try {
            val json = JSONObject(value)
            val version = UpdateVersion.parse(json.getString("version"))
            val size = UpdatePolicy.exactLong(json.opt("size"))
            if (version.code <= BuildConfig.VERSION_CODE || size !in 1..UpdatePolicy.MAX_APK_BYTES) null else UpdateRelease(version, size)
        } catch (_: Exception) { null }
    }

    // All filesystem, preferences and PackageManager work below runs on the application-owned I/O worker.
    private fun restoreCache(): Verified? {
        partial.delete()
        return try {
            if (!record.isFile || record.length() !in 1..UpdatePolicy.MAX_CHECKSUM_BYTES) { clearCache(); return null }
            val json = JSONObject(record.readText(Charsets.UTF_8))
            val version = UpdateVersion.parse(json.getString("version"))
            val size = UpdatePolicy.exactLong(json.opt("size"))
            if (size !in 1..UpdatePolicy.MAX_APK_BYTES) throw UpdateFailure("The cached APK size is invalid.")
            val release = UpdateRelease(version, size)
            val hash = UpdatePolicy.checksum(json.getString("sha256"), release.apkName)
            Verified(release, hash, File(apks, release.apkName)).also { cached ->
                verify(cached)
                apks.listFiles()?.filter { it != cached.file }?.forEach { it.delete() }
            }
        } catch (_: Exception) { clearCache(); null }
    }

    private fun saveRecord(cached: Verified) {
        val temporary = File(root, "verified.part")
        val bytes = JSONObject().put("version", cached.release.version.name).put("size", cached.release.size).put("sha256", cached.sha256).toString().toByteArray(Charsets.UTF_8)
        FileOutputStream(temporary).use { it.write(bytes); it.fd.sync() }
        if (!temporary.renameTo(record)) throw UpdateFailure("The verified update record could not be saved.")
    }

    private fun clearCache() {
        partial.delete()
        record.delete()
        File(root, "verified.part").delete()
        apks.listFiles()?.forEach { it.delete() }
    }

    private fun verify(cached: Verified) {
        if (!cached.file.isFile || cached.file.length() != cached.release.size) throw UpdateFailure("The cached APK is missing or incomplete. Tap Install to download it again.")
        val hash = MessageDigest.getInstance("SHA-256")
        FileInputStream(cached.file).use { input ->
            val buffer = ByteArray(32 * 1024)
            var total = 0L
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                total += count
                if (total > cached.release.size) throw UpdateFailure("The cached APK exceeds its expected size.")
                hash.update(buffer, 0, count)
            }
            if (total != cached.release.size) throw UpdateFailure("The cached APK is incomplete.")
        }
        UpdatePolicy.verifyDigest(cached.sha256, UpdateTransport.hex(hash.digest()))
        verifyPackage(cached.release, cached.file)
    }

    @Suppress("DEPRECATION")
    private fun verifyPackage(release: UpdateRelease, file: File) {
        val flags = if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES else PackageManager.GET_SIGNATURES
        val installed = app.packageManager.getPackageInfo(app.packageName, flags)
        val archive = app.packageManager.getPackageArchiveInfo(file.absolutePath, flags)
            ?: throw UpdateFailure("Android could not read the downloaded APK.")
        UpdatePolicy.verifyIdentity(release, identity(installed), identity(archive))
    }

    @Suppress("DEPRECATION")
    private fun identity(info: PackageInfo): ApkIdentity {
        val signatures = if (Build.VERSION.SDK_INT >= 28) info.signingInfo?.apkContentsSigners else info.signatures
        val signers = signatures?.map { UpdateTransport.hex(MessageDigest.getInstance("SHA-256").digest(it.toByteArray())) }?.toSet() ?: emptySet()
        val code = if (Build.VERSION.SDK_INT >= 28) info.longVersionCode else info.versionCode.toLong()
        return ApkIdentity(info.packageName, info.versionName, code, signers)
    }

    companion object {
        private const val PREFERENCES = "android_updates"
        @Volatile private var instance: NativeUpdater? = null
        fun get(context: Context): NativeUpdater = instance ?: synchronized(this) {
            instance ?: NativeUpdater(context).also { instance = it }
        }
    }
}
