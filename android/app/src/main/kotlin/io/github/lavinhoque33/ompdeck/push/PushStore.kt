package io.github.lavinhoque33.ompdeck.push

import org.json.JSONException
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest
import java.security.SecureRandom

/**
 * Everything push needs to survive a process restart, in one file sealed by a [SecretBox] (Android Keystore in the
 * app): the Firebase config, the FCM token, the 32-byte key shared with each desktop, the desktops' labels, the
 * permission-asked flag, the per-install secret for notification taps and the replay cache of seen push nonces.
 *
 * Thread-safe (the FCM service, the bridge and the activity all use it). A file that does not authenticate (the
 * Keystore key is gone, or the file was altered) starts an empty state: the phone then has no keys, so nothing is
 * decrypted, and the web app registers again. A file that cannot be read at all raises [IOException] instead of
 * being overwritten.
 */
class PushStore(
    private val file: File,
    private val box: SecretBox,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    private class State {
        var config: FirebaseConfig? = null
        var token: String? = null
        var permissionAsked = false
        var openSecret: String? = null
        val keys = LinkedHashMap<String, ByteArray>()
        val labels = LinkedHashMap<String, String>()

        /** `desktop:nonce` -> the time after which the nonce can no longer arrive (the payload's expiry plus skew). */
        val replay = LinkedHashMap<String, Long>()
    }

    private var loaded: State? = null
    private val random = SecureRandom()

    // ---- Firebase config and token ---------------------------------------------------------------------------

    @Synchronized
    fun config(): FirebaseConfig? = state().config

    @Synchronized
    fun setConfig(config: FirebaseConfig) {
        val s = state()
        s.config = config
        persist(s)
    }

    /** Forgets the Firebase config and the token that belongs to it (desktop keys stay: they do not depend on the project). */
    @Synchronized
    fun clearConfig() {
        val s = state()
        s.config = null
        s.token = null
        persist(s)
    }

    @Synchronized
    fun token(): String? = state().token

    /** Stores [token]; true when it differs from the stored one. */
    @Synchronized
    fun setToken(token: String): Boolean {
        val s = state()
        if (s.token == token) return false
        s.token = token
        persist(s)
        return true
    }

    @Synchronized
    fun permissionAsked(): Boolean = state().permissionAsked

    @Synchronized
    fun markPermissionAsked() {
        val s = state()
        if (s.permissionAsked) return
        s.permissionAsked = true
        persist(s)
    }

    // ---- desktop keys and labels -----------------------------------------------------------------------------

    @Synchronized
    fun keyFor(desktop: String): ByteArray? = state().keys[desktop]?.copyOf()

    @Synchronized
    fun desktops(): Set<String> = state().keys.keys.toSet()

    /** The key shared with [desktop], created (CSPRNG, 32 bytes) on first use; null when [MAX_DESKTOPS] others exist. */
    @Synchronized
    fun getOrCreateKey(desktop: String): ByteArray? {
        val s = state()
        s.keys[desktop]?.let { return it.copyOf() }
        if (s.keys.size >= MAX_DESKTOPS) return null
        val key = ByteArray(PushEnvelope.KEY_BYTES).also(random::nextBytes)
        s.keys[desktop] = key
        persist(s)
        return key.copyOf()
    }

    /** Removes [desktop]'s key, label and replay entries; other desktops are untouched. */
    @Synchronized
    fun forget(desktop: String) {
        val s = state()
        val had = s.keys.remove(desktop) != null
        val hadLabel = s.labels.remove(desktop) != null
        val replayed = s.replay.keys.removeIf { it.startsWith("$desktop:") }
        if (had || hadLabel || replayed) persist(s)
    }

    /** Sets one desktop's label (blank removes it). */
    @Synchronized
    fun setLabel(desktop: String, label: String) {
        val s = state()
        val changed = if (label.isEmpty()) s.labels.remove(desktop) != null else s.labels.put(desktop, label) != label
        if (changed) persist(s)
    }

    /** Replaces every label (the web app sends the full map). Keys are never removed because a label is missing. */
    @Synchronized
    fun replaceLabels(labels: Map<String, String>) {
        val s = state()
        if (s.labels == labels) return
        s.labels.clear()
        s.labels.putAll(labels)
        persist(s)
    }

    @Synchronized
    fun labels(): Map<String, String> = LinkedHashMap(state().labels)

    // ---- replay cache ----------------------------------------------------------------------------------------

    /**
     * Records that push [nonceText] for [desktop] was accepted: true the first time, false for a replay. The entry
     * lives until [validUntilMs] (its payload's expiry plus skew, after which the payload is refused anyway); the cache
     * is bounded by [MAX_REPLAY] (oldest first) and written before returning, so a replay after a restart is caught.
     */
    @Synchronized
    fun registerNonce(desktop: String, nonceText: String, validUntilMs: Long): Boolean {
        val s = state()
        val now = clock()
        s.replay.values.removeIf { it < now }
        val id = "$desktop:$nonceText"
        if (s.replay.containsKey(id)) return false
        s.replay[id] = validUntilMs
        while (s.replay.size > MAX_REPLAY) {
            val oldest = s.replay.keys.iterator()
            oldest.next()
            oldest.remove()
        }
        persist(s)
        return true
    }

    // ---- per-install secret ----------------------------------------------------------------------------------

    /** The secret that notification PendingIntents carry; the exported launcher activity accepts "open" requests only with it. */
    @Synchronized
    fun openSecret(): String {
        val s = state()
        s.openSecret?.let { return it }
        val secret = Base64Url.encode(ByteArray(OPEN_SECRET_BYTES).also(random::nextBytes))
        s.openSecret = secret
        persist(s)
        return secret
    }

    @Synchronized
    fun checkOpenSecret(candidate: String?): Boolean {
        if (candidate == null) return false
        return MessageDigest.isEqual(candidate.toByteArray(Charsets.UTF_8), openSecret().toByteArray(Charsets.UTF_8))
    }

    // ---- persistence -----------------------------------------------------------------------------------------

    private fun state(): State {
        loaded?.let { return it }
        val s = read()
        loaded = s
        return s
    }

    private fun read(): State {
        if (!file.exists()) return State()
        val sealed = file.readBytes()
        val plain = try {
            box.open(sealed)
        } catch (_: SecretBoxException) {
            return State()
        }
        return try {
            decode(JSONObject(String(plain, Charsets.UTF_8)))
        } catch (_: JSONException) {
            State()
        }
    }

    private fun persist(s: State) {
        val sealed = box.seal(encode(s).toString().toByteArray(Charsets.UTF_8))
        val dir = file.absoluteFile.parentFile ?: throw IOException("no parent directory")
        dir.mkdirs()
        val tmp = File(dir, file.name + ".tmp")
        FileOutputStream(tmp).use { out ->
            out.write(sealed)
            out.fd.sync()
        }
        Files.move(tmp.toPath(), file.toPath(), StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING)
    }

    private fun encode(s: State): JSONObject {
        val root = JSONObject().put("v", 1).put("asked", s.permissionAsked)
        s.config?.let {
            root.put(
                "config",
                JSONObject().put("apiKey", it.apiKey).put("appId", it.appId).put("projectId", it.projectId)
                    .put("senderId", it.senderId).put("storageBucket", it.storageBucket ?: JSONObject.NULL),
            )
        }
        s.token?.let { root.put("token", it) }
        s.openSecret?.let { root.put("open", it) }
        root.put("keys", JSONObject().also { o -> s.keys.forEach { (d, k) -> o.put(d, Base64Url.encode(k)) } })
        root.put("labels", JSONObject().also { o -> s.labels.forEach { (d, l) -> o.put(d, l) } })
        root.put("replay", JSONObject().also { o -> s.replay.forEach { (id, until) -> o.put(id, until) } })
        return root
    }

    private fun decode(root: JSONObject): State {
        val s = State()
        s.permissionAsked = root.optBoolean("asked", false)
        root.optJSONObject("config")?.let { c ->
            val bucket = c.opt("storageBucket") as? String
            val apiKey = c.optString("apiKey")
            val appId = c.optString("appId")
            val projectId = c.optString("projectId")
            val senderId = c.optString("senderId")
            if (apiKey.isNotEmpty() && appId.isNotEmpty() && projectId.isNotEmpty() && senderId.isNotEmpty()) {
                s.config = FirebaseConfig(apiKey, appId, projectId, senderId, bucket)
            }
        }
        s.token = root.optString("token").takeIf { it.isNotEmpty() }
        s.openSecret = root.optString("open").takeIf { it.isNotEmpty() }
        root.optJSONObject("keys")?.let { o ->
            for (desktop in o.keys()) {
                val key = Base64Url.decodeCanonical(o.optString(desktop), PushEnvelope.KEY_BYTES)
                if (key != null && key.size == PushEnvelope.KEY_BYTES) s.keys[desktop] = key
            }
        }
        root.optJSONObject("labels")?.let { o -> for (d in o.keys()) s.labels[d] = o.optString(d) }
        root.optJSONObject("replay")?.let { o -> for (id in o.keys()) s.replay[id] = o.optLong(id) }
        return s
    }

    companion object {
        const val MAX_DESKTOPS = 64
        const val MAX_REPLAY = 1024
        private const val OPEN_SECRET_BYTES = 16
    }
}
