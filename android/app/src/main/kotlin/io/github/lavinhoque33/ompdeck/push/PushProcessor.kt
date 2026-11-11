package io.github.lavinhoque33.ompdeck.push

import java.io.IOException

enum class RejectReason { MALFORMED_ENVELOPE, UNKNOWN_DESKTOP, AUTH_FAILED, BAD_PAYLOAD, EXPIRED, TOO_FAR_IN_FUTURE, REPLAY, STORE_UNAVAILABLE }

enum class SuppressReason { DESKTOP_VISIBLE, NOTIFICATIONS_DISABLED }

sealed interface PushOutcome {
    class Shown(val notificationTag: String) : PushOutcome

    class Suppressed(val reason: SuppressReason) : PushOutcome

    class Rejected(val reason: RejectReason) : PushOutcome
}

/** Posts the local notification; [title] already carries the desktop label. Returns its tag, or null when the system will not show it. */
fun interface NotificationSink {
    fun show(notification: PushNotification, title: String, nonceText: String): String?
}

/**
 * One FCM data message, end to end: envelope fields -> key of exactly the named desktop -> AES-GCM verification ->
 * payload validation and expiry -> replay check -> visibility suppression -> local notification. Anything that fails
 * is dropped silently (no plaintext fallback, no notification), and no secret is ever logged.
 */
class PushProcessor(
    private val store: PushStore,
    private val presence: Presence,
    private val sink: NotificationSink,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    private class Stop(val reason: RejectReason) : Exception(null, null, false, false)

    private class Accepted(val envelope: PushEnvelope, val notification: PushNotification, val labels: Map<String, String>)

    fun process(data: Map<String, String>): PushOutcome {
        val accepted = try {
            accept(data)
        } catch (stop: Stop) {
            return PushOutcome.Rejected(stop.reason)
        } catch (_: IOException) {
            return PushOutcome.Rejected(RejectReason.STORE_UNAVAILABLE)
        }
        val desktop = accepted.envelope.desktop
        if (presence.isVisible(desktop)) return PushOutcome.Suppressed(SuppressReason.DESKTOP_VISIBLE)
        val label = if (accepted.labels.size >= 2) accepted.labels[desktop] else null
        val tag = sink.show(accepted.notification, titleWithLabel(accepted.notification.title, label), accepted.envelope.nonceText)
            ?: return PushOutcome.Suppressed(SuppressReason.NOTIFICATIONS_DISABLED)
        return PushOutcome.Shown(tag)
    }

    private fun accept(data: Map<String, String>): Accepted {
        val envelope = PushEnvelope.parse(data) ?: throw Stop(RejectReason.MALFORMED_ENVELOPE)
        val key = store.keyFor(envelope.desktop) ?: throw Stop(RejectReason.UNKNOWN_DESKTOP)
        val plain = try {
            envelope.decrypt(key)
        } finally {
            key.fill(0)
        } ?: throw Stop(RejectReason.AUTH_FAILED)
        val parsed = PushPayload.parse(plain, envelope.desktop, clock())
        if (parsed is ParsedPayload.Rejected) {
            throw Stop(
                when (parsed.error) {
                    PayloadError.MALFORMED -> RejectReason.BAD_PAYLOAD
                    PayloadError.EXPIRED -> RejectReason.EXPIRED
                    PayloadError.TOO_FAR_IN_FUTURE -> RejectReason.TOO_FAR_IN_FUTURE
                },
            )
        }
        val ok = parsed as ParsedPayload.Ok
        // Only authenticated, valid, unexpired pushes enter the replay cache, so garbage cannot fill it.
        if (!store.registerNonce(envelope.desktop, envelope.nonceText, ok.expiresAtMs + PushPayload.SKEW_MS)) {
            throw Stop(RejectReason.REPLAY)
        }
        return Accepted(envelope, ok.notification, store.labels())
    }

    companion object {
        /** `<title> — <label>`; the gateway's default label "OmpDeck on <host>" loses its "OmpDeck on " (the title says OmpDeck). */
        fun titleWithLabel(title: String, label: String?): String {
            val name = label?.removePrefix("OmpDeck on ").orEmpty()
            return if (name.isEmpty()) title else "$title — $name"
        }
    }
}
