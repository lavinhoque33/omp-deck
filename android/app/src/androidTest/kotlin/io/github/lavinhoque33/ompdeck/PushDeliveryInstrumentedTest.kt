package io.github.lavinhoque33.ompdeck

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.service.notification.StatusBarNotification
import androidx.test.platform.app.InstrumentationRegistry
import com.google.firebase.messaging.RemoteMessage
import io.github.lavinhoque33.ompdeck.push.Base64Url
import io.github.lavinhoque33.ompdeck.push.OmpDeckMessagingService
import io.github.lavinhoque33.ompdeck.push.Presence
import io.github.lavinhoque33.ompdeck.push.PushRuntime
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * An FCM data message injected into the real service, through the Keystore-sealed store and Android notifier,
 * without an Activity: authentication, replay, suppression and lock-screen privacy. BridgeInstrumentedTest covers
 * authenticated open routes; a real notification-shade tap is part of the live FCM smoke.
 */
class PushDeliveryInstrumentedTest {
    private class TestService : OmpDeckMessagingService() {
        fun attach(context: Context) = attachBaseContext(context)
    }

    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext
    private val runtime = PushRuntime.get(context)
    private val manager = context.getSystemService(NotificationManager::class.java)
    private val random = SecureRandom()
    private val service = TestService().also { it.attach(context) }

    // Random desktop ids: the real store may hold the user's own desktops, which must stay untouched.
    private val desktop = hex()
    private val otherDesktop = hex()
    private lateinit var key: ByteArray
    private lateinit var savedLabels: Map<String, String>

    private fun hex() = ByteArray(32).also(random::nextBytes).joinToString("") { "%02x".format(it) }

    @Before
    fun setUp() {
        // Granted at install (`installation` block in app/build.gradle.kts): no UiAutomation involved.
        assertEquals(
            "POST_NOTIFICATIONS must be granted at install; grant it with `adb shell pm grant ${context.packageName} android.permission.POST_NOTIFICATIONS`",
            PackageManager.PERMISSION_GRANTED,
            context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS),
        )
        savedLabels = runtime.store.labels()
        key = runtime.store.getOrCreateKey(desktop)!!
        Presence.shared.set(null, false)
        cancelOurs()
    }

    @After
    fun tearDown() {
        cancelOurs()
        runtime.store.forget(desktop)
        runtime.store.forget(otherDesktop)
        runtime.store.replaceLabels(savedLabels)
        Presence.shared.set(null, false)
        Presence.shared.setStarted(false)
    }

    private fun ours(): List<StatusBarNotification> =
        manager.activeNotifications.filter { it.tag?.startsWith("$desktop:") == true || it.tag?.startsWith("$otherDesktop:") == true }

    private fun cancelOurs() = ours().forEach { manager.cancel(it.tag, it.id) }

    private fun payload(expires: Long = System.currentTimeMillis() + 3_600_000, navigate: String = "https://relay.example.com/#s=session-1", tag: String = "turn-1") =
        """{"web_push":8030,"notification":{"title":"OmpDeck","body":"Session finished","navigate":"$navigate","tag":"$tag","silent":false},"expires_unix_ms":$expires}"""

    private fun seal(plain: String, nonce: ByteArray, forDesktop: String = desktop, withKey: ByteArray = key): String {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(withKey, "AES"), GCMParameterSpec(128, nonce))
        cipher.updateAAD("ompdeck-android-push-v1:$forDesktop".toByteArray())
        return Base64Url.encode(cipher.doFinal(plain.toByteArray()))
    }

    private fun nonce() = ByteArray(12).also(random::nextBytes)

    private fun message(
        plain: String = payload(),
        nonce: ByteArray = nonce(),
        claimed: String = desktop,
        sealedFor: String = desktop,
        withKey: ByteArray = key,
        tamper: Boolean = false,
    ): RemoteMessage {
        var ciphertext = seal(plain, nonce, sealedFor, withKey)
        if (tamper) ciphertext = (if (ciphertext[0] == 'A') "B" else "A") + ciphertext.drop(1)
        return RemoteMessage.Builder("ompdeck-test@fcm.googleapis.com")
            .setData(mapOf("v" to "1", "desktop" to claimed, "nonce" to Base64Url.encode(nonce), "ciphertext" to ciphertext))
            .build()
    }

    @Test
    fun aValidPushBecomesAPrivateNotification() {
        service.onMessageReceived(message())

        val posted = ours().single()
        assertEquals("$desktop:turn-1", posted.tag)
        val notification = posted.notification
        assertEquals("OmpDeck", notification.extras.getString(Notification.EXTRA_TITLE))
        assertEquals("Session finished", notification.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
        assertEquals(Notification.VISIBILITY_PRIVATE, notification.visibility)
        // The lock screen sees a generic version, never the session text.
        val publicText = notification.publicVersion.extras.getCharSequence(Notification.EXTRA_TEXT).toString()
        assertTrue(publicText != "Session finished")
    }


    @Test
    fun forgedWrongDesktopExpiredAndTamperedPushesShowNothing() {
        // Authentic bytes for another desktop's AAD, relabelled to ours.
        service.onMessageReceived(message(claimed = desktop, sealedFor = otherDesktop))
        // Ciphertext for our desktop moved to a desktop we do not know.
        service.onMessageReceived(message(claimed = otherDesktop, sealedFor = desktop))
        // A flipped ciphertext byte.
        service.onMessageReceived(message(tamper = true))
        // Encrypted under a key that is not ours.
        service.onMessageReceived(message(withKey = ByteArray(32) { 5 }))
        // Valid crypto but already expired.
        service.onMessageReceived(message(plain = payload(expires = System.currentTimeMillis() - 20 * 60_000)))
        // Plaintext FCM (no envelope at all) is not understood.
        service.onMessageReceived(
            RemoteMessage.Builder("ompdeck-test@fcm.googleapis.com").setData(mapOf("title" to "x", "body" to "leak")).build(),
        )
        assertEquals(emptyList<String>(), ours().map { it.tag })
    }

    @Test
    fun aDuplicateNonceIsPostedOnce() {
        val n = nonce()
        service.onMessageReceived(message(nonce = n))
        assertEquals(1, ours().size)
        cancelOurs()
        service.onMessageReceived(message(nonce = n))
        assertEquals("replay must not pop up again", 0, ours().size)
        service.onMessageReceived(message(nonce = nonce()))
        assertEquals(1, ours().size)
    }

    @Test
    fun theDesktopBeingWatchedIsSilentOthersAreNot() {
        runtime.store.getOrCreateKey(otherDesktop)!!.let { otherKey ->
            Presence.shared.setStarted(true)
            Presence.shared.set(desktop, true)
            service.onMessageReceived(message())
            assertEquals(0, ours().size)
            service.onMessageReceived(message(claimed = otherDesktop, sealedFor = otherDesktop, withKey = otherKey, plain = payload(tag = "t2")))
            assertEquals(listOf("$otherDesktop:t2"), ours().map { it.tag })
        }
    }

    @Test
    fun twoDesktopsKeepTheirOwnNotificationsAndTitles() {
        val otherKey = runtime.store.getOrCreateKey(otherDesktop)!!
        runtime.store.replaceLabels(mapOf(desktop to "OmpDeck on work", otherDesktop to "Home PC"))
        service.onMessageReceived(message(plain = payload(tag = "same")))
        service.onMessageReceived(message(claimed = otherDesktop, sealedFor = otherDesktop, withKey = otherKey, plain = payload(tag = "same")))

        val byTag = ours().associateBy { it.tag }
        assertEquals(setOf("$desktop:same", "$otherDesktop:same"), byTag.keys)
        assertEquals("OmpDeck — work", byTag.getValue("$desktop:same").notification.extras.getString(Notification.EXTRA_TITLE))
        assertEquals("OmpDeck — Home PC", byTag.getValue("$otherDesktop:same").notification.extras.getString(Notification.EXTRA_TITLE))
    }

    @Test
    fun forgettingADesktopDismissesItsNotificationsAndStopsNewOnes() {
        val otherKey = runtime.store.getOrCreateKey(otherDesktop)!!
        service.onMessageReceived(message())
        service.onMessageReceived(message(claimed = otherDesktop, sealedFor = otherDesktop, withKey = otherKey))
        assertEquals(2, ours().size)

        runtime.store.forget(desktop)
        runtime.notifier.cancelDesktop(desktop)
        assertEquals(listOf(otherDesktop), ours().map { it.tag!!.substringBefore(':') })

        service.onMessageReceived(message())
        assertEquals("a revoked desktop gets no notification", 1, ours().size)
        assertNull(runtime.store.keyFor(desktop))
        assertNotNull(runtime.store.keyFor(otherDesktop))
    }
}
