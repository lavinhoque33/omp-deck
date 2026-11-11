package io.github.lavinhoque33.ompdeck.push

/**
 * A request to open a destination in the app, carried by a notification's PendingIntent. The launcher activity is
 * exported, so any app can send it an intent: only one that carries this install's secret is believed, and the
 * destination is re-validated (a path and hash of the app, naming a desktop) before it reaches the page.
 */
class OpenRequest(val url: String, val desktop: String) {
    companion object {
        const val ACTION = "io.github.lavinhoque33.ompdeck.OPEN"
        const val EXTRA_URL = "u"
        const val EXTRA_DESKTOP = "d"
        const val EXTRA_SECRET = "t"

        private val URL = Regex("/(#[A-Za-z0-9._~%!$'()*+,;:@/?=&-]{0,${PushPayload.MAX_HASH}})?")
        private val DESKTOP = Regex("[0-9a-f]{64}")

        fun validate(action: String?, url: String?, desktop: String?, secret: String?, store: PushStore): OpenRequest? {
            if (action != ACTION || url == null || desktop == null) return null
            if (!store.checkOpenSecret(secret)) return null
            if (!URL.matches(url) || !DESKTOP.matches(desktop)) return null
            return OpenRequest(url, desktop)
        }
    }
}
