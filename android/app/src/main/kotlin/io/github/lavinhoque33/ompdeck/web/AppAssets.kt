package io.github.lavinhoque33.ompdeck.web

import android.content.res.AssetManager
import android.net.Uri
import android.webkit.WebResourceResponse
import androidx.webkit.WebViewAssetLoader
import java.io.ByteArrayInputStream
import java.io.IOException

/** Maps `https://appassets.androidplatform.net/<path>` to `assets/www/<path>` and adds the security headers. */
class AppAssets(assets: AssetManager) {
    private val loader = WebViewAssetLoader.Builder()
        .setDomain(UrlPolicy.HOST)
        .setHttpAllowed(false)
        .addPathHandler("/", Handler(assets))
        .build()

    /** The response for [uri], or null when it is not on the app origin (the WebView then does its own, CSP-bound, loading). */
    fun intercept(uri: Uri): WebResourceResponse? {
        val response = loader.shouldInterceptRequest(uri) ?: return null
        response.responseHeaders = (response.responseHeaders ?: emptyMap()) + AssetPolicy.HEADERS
        return response
    }

    private class Handler(private val assets: AssetManager) : WebViewAssetLoader.PathHandler {
        override fun handle(path: String): WebResourceResponse {
            val relative = AssetPolicy.resolve(path) ?: return notFound()
            return try {
                val stream = assets.open("www/$relative")
                WebResourceResponse(AssetPolicy.mimeType(relative), if (AssetPolicy.isText(relative)) "utf-8" else null, stream)
            } catch (_: IOException) {
                notFound()
            }
        }

        private fun notFound() =
            WebResourceResponse("text/plain", "utf-8", 404, "Not Found", AssetPolicy.HEADERS, ByteArrayInputStream(ByteArray(0)))
    }
}
