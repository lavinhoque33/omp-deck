package io.github.lavinhoque33.ompdeck.bridge

/**
 * Holds "open" requests (notification taps) until the page has completed its `info` handshake, so an initial
 * intent is never posted to a page that is not listening yet. Bounded; the oldest is dropped first. A new document
 * (reload, render-process restart) starts a new handshake via [reset].
 */
class EventGate(private val maxQueued: Int = MAX_QUEUED) {
    private val queued = ArrayDeque<String>()

    var isReady = false
        private set

    /** True when [url] can be sent now; otherwise it is held for [markReady]. */
    fun deliver(url: String): Boolean {
        if (isReady) return true
        if (queued.size >= maxQueued) queued.removeFirst()
        queued.addLast(url)
        return false
    }

    /** The page handshook: everything held, oldest first (and from now on [deliver] sends at once). */
    fun markReady(): List<String> {
        isReady = true
        val held = queued.toList()
        queued.clear()
        return held
    }

    fun reset() {
        isReady = false
    }

    companion object {
        const val MAX_QUEUED = 8
    }
}
