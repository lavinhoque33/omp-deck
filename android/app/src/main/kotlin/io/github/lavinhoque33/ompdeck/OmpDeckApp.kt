package io.github.lavinhoque33.ompdeck

import android.app.Application
import io.github.lavinhoque33.ompdeck.push.PushRuntime

class OmpDeckApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // Firebase has no compiled-in config: bring it up from the user's imported one before any FCM service runs.
        PushRuntime.get(this).bootFirebase()
    }
}
