package io.github.lavinhoque33.ompdeck.push

import android.annotation.SuppressLint
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import io.github.lavinhoque33.ompdeck.MainActivity
import io.github.lavinhoque33.ompdeck.R
import java.security.MessageDigest

/**
 * Local notifications for decrypted pushes. Content is private on the lock screen (a generic public version is
 * shown instead), a tap opens the app's own activity through an explicit, immutable PendingIntent, and tags are
 * namespaced per desktop (`<desktop>:<tag>`) so two desktops' notifications never replace each other.
 */
class Notifier(private val context: Context, private val store: PushStore) : NotificationSink {
    fun ensureChannel() {
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) != null) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            context.getString(R.string.notification_channel_name),
            NotificationManager.IMPORTANCE_HIGH,
        )
        channel.description = context.getString(R.string.notification_channel_description)
        manager.createNotificationChannel(channel)
    }

    /** False when the system would drop the notification (permission refused, app or channel blocked). */
    fun canNotify(): Boolean {
        if (!NotificationManagerCompat.from(context).areNotificationsEnabled()) return false
        val channel = context.getSystemService(NotificationManager::class.java).getNotificationChannel(CHANNEL_ID)
        return channel == null || channel.importance != NotificationManager.IMPORTANCE_NONE
    }

    @SuppressLint("MissingPermission") // canNotify() checked the POST_NOTIFICATIONS state just above.
    override fun show(notification: PushNotification, title: String, nonceText: String): String? {
        if (!canNotify()) return null
        ensureChannel()
        val tag = tagFor(notification.desktop, notification.tag, nonceText)
        val open = Intent(context, MainActivity::class.java)
            .setAction(OpenRequest.ACTION)
            // A distinct data URI per notification keeps their PendingIntents (and extras) apart.
            .setData(Uri.Builder().scheme("ompdeck").authority("notification").appendPath(digest(tag)).build())
            .putExtra(OpenRequest.EXTRA_URL, notification.navigate)
            .putExtra(OpenRequest.EXTRA_DESKTOP, notification.desktop)
            .putExtra(OpenRequest.EXTRA_SECRET, store.openSecret())
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        val pending = PendingIntent.getActivity(
            context,
            tag.hashCode(),
            open,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val publicVersion = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_stat_ompdeck)
            .setContentTitle(context.getString(R.string.app_name))
            .setContentText(context.getString(R.string.notification_public_text))
            .build()
        val built = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_stat_ompdeck)
            .setColor(context.getColor(R.color.ompdeck_accent))
            .setContentTitle(title)
            .setContentText(notification.body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(notification.body))
            .setContentIntent(pending)
            .setAutoCancel(true)
            .setSilent(notification.silent)
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setPublicVersion(publicVersion)
            .build()
        NotificationManagerCompat.from(context).notify(tag, NOTIFICATION_ID, built)
        return tag
    }

    /** Dismisses every notification that came from [desktop], leaving other desktops' untouched. */
    fun cancelDesktop(desktop: String) {
        val manager = context.getSystemService(NotificationManager::class.java)
        for (active in manager.activeNotifications) {
            val tag = active.tag ?: continue
            if (tag.startsWith("$desktop:")) manager.cancel(tag, active.id)
        }
    }

    companion object {
        const val CHANNEL_ID = "ompdeck_events"
        const val NOTIFICATION_ID = 1

        /** The system tag of a notification: tags coalesce per desktop; a push without a tag is never coalesced. */
        fun tagFor(desktop: String, tag: String?, nonceText: String): String =
            if (tag != null) "$desktop:$tag" else "$desktop:n:$nonceText"

        private fun digest(text: String): String =
            MessageDigest.getInstance("SHA-256").digest(text.toByteArray(Charsets.UTF_8)).take(8).joinToString("") { "%02x".format(it) }
    }
}
