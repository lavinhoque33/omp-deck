package io.github.lavinhoque33.ompdeck.web

/** The pure rules of the bundled-asset server: which request paths are files, their content types and the response headers. */
object AssetPolicy {
    /**
     * The page's CSP: the relay's own policy, widened only for the relay connection itself (`wss:`, the one
     * scheme pairing allows) because the page is served from the bundle, not from the relay.
     */
    const val CSP = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; " +
        "img-src 'self' data: blob:; font-src 'self'; connect-src 'self' wss:; manifest-src 'self'; " +
        "worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"

    val HEADERS = mapOf(
        "Content-Security-Policy" to CSP,
        "X-Content-Type-Options" to "nosniff",
        "Referrer-Policy" to "no-referrer",
        "Cache-Control" to "no-store",
    )

    private const val MAX_PATH = 512

    /**
     * The asset path under `www/` for the request path [path] (relative to the origin root), or null when it is not
     * a plain file path: no empty, `.` or `..` segments, backslashes or control characters. The root is `index.html`.
     */
    fun resolve(path: String): String? {
        if (path.length > MAX_PATH) return null
        if (path.isEmpty()) return "index.html"
        if (path.any { it == '\\' || it.isISOControl() }) return null
        val segments = path.split('/')
        if (segments.any { it.isEmpty() || it == "." || it == ".." }) return null
        return path
    }

    fun mimeType(path: String): String = when (path.substringAfterLast('.', "").lowercase()) {
        "html" -> "text/html"
        "js", "mjs" -> "text/javascript"
        "css" -> "text/css"
        "json" -> "application/json"
        "webmanifest" -> "application/manifest+json"
        "svg" -> "image/svg+xml"
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "gif" -> "image/gif"
        "webp" -> "image/webp"
        "ico" -> "image/x-icon"
        "woff2" -> "font/woff2"
        "woff" -> "font/woff"
        "ttf" -> "font/ttf"
        "txt" -> "text/plain"
        else -> "application/octet-stream"
    }

    fun isText(path: String): Boolean = mimeType(path).let {
        it.startsWith("text/") || it == "application/json" || it == "application/manifest+json" || it == "image/svg+xml"
    }
}
