package io.github.lavinhoque33.ompdeck.bridge

import org.json.JSONException
import org.json.JSONObject

/** The bridge methods the web page may call (the contract's list; anything else is refused). */
enum class BridgeMethod(val wire: String) {
    INFO("info"),
    PUSH_STATE("pushState"),
    PUSH_ENABLE("pushEnable"),
    PUSH_SUBSCRIPTION("pushSubscription"),
    PUSH_FORGET("pushForget"),
    PUSH_LABELS("pushLabels"),
    SET_PRESENCE("setPresence"),
    SAVE_FILE("saveFile"),
    UPDATE_STATE("updateState"),
    UPDATE_CHECK("updateCheck"),
    UPDATE_INSTALL("updateInstall"),
    UPDATE_CONFIGURE("updateConfigure"),
    FINISH("finish");

    companion object {
        private val byWire = entries.associateBy { it.wire }
        fun of(name: String): BridgeMethod? = byWire[name]
    }
}

/** A message the native side refuses; [message] is shown to the user verbatim by the web page. */
class BridgeException(message: String) : Exception(message)

class BridgeRequest(val id: String, val method: BridgeMethod, val params: JSONObject)

/** Why a request could not be answered with a result: `id` is null when the message had no usable id (nothing can be answered). */
class BridgeRejection(val id: String?, val error: String)

sealed interface ParsedRequest {
    class Ok(val request: BridgeRequest) : ParsedRequest
    class Rejected(val rejection: BridgeRejection) : ParsedRequest
}

/** Parsing and validation of the JSON the page posts, and encoding of what native sends back. */
object BridgeProtocol {
    /** Everything but saveFile is tiny. */
    const val MAX_MESSAGE_CHARS = 64 * 1024

    /** `saveFile` carries up to 20 MiB of data as standard base64 (4/3 expansion, padding included) plus a small JSON frame. */
    const val MAX_SAVE_BYTES = 20 * 1024 * 1024
    const val MAX_SAVE_MESSAGE_CHARS = (MAX_SAVE_BYTES + 2) / 3 * 4 + 4 * 1024

    const val MAX_ID_CHARS = 64
    const val MAX_LABEL_CHARS = 128
    const val MAX_LABELS = 64
    const val MAX_FILE_NAME_CHARS = 120
    const val MAX_CONFIG_CHARS = 48 * 1024

    private val ID = Regex("[A-Za-z0-9_.:-]{1,$MAX_ID_CHARS}")
    private val DESKTOP = Regex("[0-9a-f]{64}")
    private val MIME = Regex("[a-z0-9][a-z0-9!#$&^_.+-]{0,126}/[a-z0-9][a-z0-9!#$&^_.+-]{0,126}")

    fun isDesktopId(text: Any?): Boolean = text is String && DESKTOP.matches(text)

    /** Parses one posted message. Cheap length gates come first, so an oversized message is never parsed. */
    fun parse(raw: String): ParsedRequest {
        // The id is needed to answer; extract it only from a bounded, well-formed object.
        if (raw.length > MAX_SAVE_MESSAGE_CHARS) return ParsedRequest.Rejected(BridgeRejection(null, "message_too_large"))
        val json = try {
            JSONObject(raw)
        } catch (_: JSONException) {
            return ParsedRequest.Rejected(BridgeRejection(null, "malformed"))
        }
        val id = json.opt("id")
        if (id !is String || !ID.matches(id)) return ParsedRequest.Rejected(BridgeRejection(null, "bad_id"))
        val name = json.opt("method")
        val method = (name as? String)?.let(BridgeMethod::of) ?: return ParsedRequest.Rejected(BridgeRejection(id, "unknown_method"))
        if (method != BridgeMethod.SAVE_FILE && raw.length > MAX_MESSAGE_CHARS) {
            return ParsedRequest.Rejected(BridgeRejection(id, "message_too_large"))
        }
        val params = json.opt("params")
        if (params !is JSONObject) return ParsedRequest.Rejected(BridgeRejection(id, "bad_params"))
        return ParsedRequest.Ok(BridgeRequest(id, method, params))
    }

    fun result(id: String, result: Any?): String =
        JSONObject().put("id", id).put("result", result ?: JSONObject.NULL).toString()

    fun error(id: String, message: String): String = JSONObject().put("id", id).put("error", message).toString()

    fun event(name: String, detail: JSONObject): String = JSONObject().put("event", name).put("detail", detail).toString()

    // ---- parameter validation ------------------------------------------------------------------------------

    fun emptyUpdateParams(params: JSONObject) {
        if (params.length() != 0) throw BridgeException("bad_params")
    }

    fun updateConfigureParams(params: JSONObject): Boolean {
        val enabled = params.opt("auto_check")
        if (params.length() != 1 || enabled !is Boolean) throw BridgeException("bad_params")
        return enabled
    }

    /** `pushEnable {config?: string}`: the desktop's public google-services.json, used only by a phone with no Firebase configuration. */
    fun enableParams(params: JSONObject): String? {
        val keys = params.keys().asSequence().toList()
        if (keys.isEmpty()) return null
        val config = params.opt("config")
        if (keys != listOf("config") || config !is String || config.isBlank() || config.length > MAX_CONFIG_CHARS) {
            throw BridgeException("bad_params")
        }
        return config
    }

    fun desktopParam(params: JSONObject): String {
        val desktop = params.opt("desktop")
        if (!isDesktopId(desktop)) throw BridgeException("bad_desktop")
        return desktop as String
    }

    fun labelText(value: Any?): String {
        if (value !is String) throw BridgeException("bad_label")
        return Text.clean(value, MAX_LABEL_CHARS, keepNewlines = false)
    }

    /** `pushSubscription {desktop,label}`. */
    class Subscription(val desktop: String, val label: String)

    fun subscriptionParams(params: JSONObject): Subscription =
        Subscription(desktopParam(params), labelText(params.opt("label")))

    /** `pushLabels {labels: {desktop: label}}`: the full map of paired desktops. */
    fun labelsParams(params: JSONObject): Map<String, String> {
        val labels = params.opt("labels")
        if (labels !is JSONObject) throw BridgeException("bad_labels")
        if (labels.length() > MAX_LABELS) throw BridgeException("too_many_labels")
        val out = LinkedHashMap<String, String>()
        for (key in labels.keys()) {
            if (!isDesktopId(key)) throw BridgeException("bad_desktop")
            val label = labelText(labels.opt(key))
            if (label.isNotEmpty()) out[key] = label
        }
        return out
    }

    /** `setPresence {desktop: id|null, visible: boolean}`. */
    class Presence(val desktop: String?, val visible: Boolean)

    fun presenceParams(params: JSONObject): Presence {
        val visible = params.opt("visible")
        if (visible !is Boolean) throw BridgeException("bad_visible")
        if (!params.has("desktop")) throw BridgeException("bad_desktop")
        val desktop = params.opt("desktop")
        return when {
            desktop == null || desktop == JSONObject.NULL -> Presence(null, visible)
            isDesktopId(desktop) -> Presence(desktop as String, visible)
            else -> throw BridgeException("bad_desktop")
        }
    }

    /** `saveFile {name, mime, data}`; [bytes] is decoded (and size-checked) by the caller off the main thread via [decodeSaveData]. */
    class SaveRequest(val name: String, val mime: String, val data: String)

    fun saveParams(params: JSONObject): SaveRequest {
        val name = params.opt("name")
        val mime = params.opt("mime")
        val data = params.opt("data")
        if (name !is String || mime !is String || data !is String) throw BridgeException("bad_params")
        val cleanMime = mime.trim().lowercase()
        return SaveRequest(
            name = sanitizeFileName(name),
            mime = if (MIME.matches(cleanMime)) cleanMime else "application/octet-stream",
            data = data,
        )
    }

    /** Standard (padded) base64, at most [MAX_SAVE_BYTES] decoded; strict about the alphabet. */
    fun decodeSaveData(data: String): ByteArray {
        if (data.length > MAX_SAVE_MESSAGE_CHARS) throw BridgeException("file_too_large")
        // Standard base64 is padded to a multiple of four; the JDK decoder would also accept it unpadded.
        if (data.length % 4 != 0) throw BridgeException("bad_data")
        val bytes = try {
            java.util.Base64.getDecoder().decode(data)
        } catch (_: IllegalArgumentException) {
            throw BridgeException("bad_data")
        }
        if (bytes.size > MAX_SAVE_BYTES) throw BridgeException("file_too_large")
        return bytes
    }

    /** A name that is safe as a document title: no path parts, control characters or leading dots; 'download' when nothing is left. */
    fun sanitizeFileName(name: String): String {
        val base = name.substringAfterLast('/').substringAfterLast('\\')
        val cleaned = Text.clean(base, MAX_FILE_NAME_CHARS, keepNewlines = false).trimStart('.', ' ', '\t').trim()
        return cleaned.ifEmpty { "download" }
    }
}

/** Text hygiene for strings that end up on screen. */
object Text {
    /** [text] without control characters (newlines kept on request), at most [maxCodePoints] code points, trimmed. */
    fun clean(text: String, maxCodePoints: Int, keepNewlines: Boolean): String {
        val out = StringBuilder(minOf(text.length, maxCodePoints * 2))
        var count = 0
        var i = 0
        while (i < text.length && count < maxCodePoints) {
            val cp = text.codePointAt(i)
            i += Character.charCount(cp)
            val control = Character.isISOControl(cp) || cp == 0x2028 || cp == 0x2029
            if (control && !(keepNewlines && cp == '\n'.code)) continue
            out.appendCodePoint(cp)
            count++
        }
        return out.toString().trim()
    }
}
