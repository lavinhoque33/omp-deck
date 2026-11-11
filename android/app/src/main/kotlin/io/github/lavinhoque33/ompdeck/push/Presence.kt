package io.github.lavinhoque33.ompdeck.push

/**
 * Whether the user is looking at a desktop right now, for suppressing its notifications: the activity must be
 * started (native knowledge) and the page must have reported that desktop active and visible (`setPresence`).
 * In-process only: a dead process shows nothing on screen.
 */
class Presence {
    private var started = false
    private var desktop: String? = null
    private var visible = false

    @Synchronized
    fun setStarted(started: Boolean) {
        this.started = started
    }

    @Synchronized
    fun set(desktop: String?, visible: Boolean) {
        this.desktop = desktop
        this.visible = visible && desktop != null
    }

    @Synchronized
    fun isVisible(desktop: String): Boolean = started && visible && this.desktop == desktop

    companion object {
        /** The process-wide instance shared by the activity and the FCM service. */
        val shared = Presence()
    }
}

/** Tells the live activity (if any) that token, config or permission state changed; a no-op without one. */
object PushEvents {
    @Volatile
    private var listener: (() -> Unit)? = null

    fun setListener(listener: (() -> Unit)?) {
        this.listener = listener
    }

    fun changed() {
        listener?.invoke()
    }
}
