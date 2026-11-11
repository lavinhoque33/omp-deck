package io.github.lavinhoque33.ompdeck.push

import android.util.Log
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * Receives FCM data messages and token refreshes with or without the UI alive (Firebase starts the process and
 * this service). Only data messages carrying the encrypted envelope are understood; there is no plaintext path.
 */
open class OmpDeckMessagingService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        PushRuntime.get(applicationContext).onNewToken(token)
    }

    override fun onMessageReceived(message: RemoteMessage) {
        val outcome = try {
            PushRuntime.get(applicationContext).processor.process(message.data)
        } catch (e: Exception) {
            Log.w(TAG, "push failed: ${e.javaClass.simpleName}")
            return
        }
        // Reasons only: never the payload, keys, nonce or token.
        when (outcome) {
            is PushOutcome.Rejected -> Log.i(TAG, "push dropped: ${outcome.reason}")
            is PushOutcome.Suppressed -> Log.i(TAG, "push suppressed: ${outcome.reason}")
            is PushOutcome.Shown -> Unit
        }
    }

    private companion object {
        const val TAG = "OmpDeckPush"
    }
}
