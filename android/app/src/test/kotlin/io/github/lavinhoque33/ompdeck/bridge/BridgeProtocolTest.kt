package io.github.lavinhoque33.ompdeck.bridge

import io.github.lavinhoque33.ompdeck.push.DESKTOP_A
import io.github.lavinhoque33.ompdeck.push.DESKTOP_B
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.util.Base64

class BridgeProtocolTest {
    private fun request(id: String = "1", method: String = "info", params: String = "{}") =
        """{"id":"$id","method":"$method","params":$params}"""

    private fun rejection(raw: String) = (BridgeProtocol.parse(raw) as ParsedRequest.Rejected).rejection

    private fun accepted(raw: String) = (BridgeProtocol.parse(raw) as ParsedRequest.Ok).request

    private fun failure(block: () -> Unit): String {
        try {
            block()
        } catch (e: BridgeException) {
            return e.message!!
        }
        fail("accepted")
        return ""
    }

    @Test
    fun acceptsEveryContractMethodAndNothingElse() {
        for (method in BridgeMethod.entries) assertEquals(method, accepted(request(method = method.wire)).method)
        assertEquals(
            listOf("info", "pushState", "pushEnable", "pushSubscription", "pushForget", "pushLabels", "setPresence", "saveFile", "updateState", "updateCheck", "updateInstall", "updateConfigure", "finish"),
            BridgeMethod.entries.map { it.wire },
        )
        assertEquals("unknown_method", rejection(request(method = "eval")).error)
        assertEquals("unknown_method", rejection(request(method = "Info")).error)
        assertEquals("unknown_method", rejection(request(method = "")).error)
        assertEquals("unknown_method", rejection("""{"id":"1","method":5,"params":{}}""").error)
        assertEquals("1", rejection(request(method = "openUrl")).id)
    }

    @Test
    fun updaterAcceptsNoUrlsFilesOrCoercedSettings() {
        BridgeProtocol.emptyUpdateParams(JSONObject())
        assertEquals("bad_params", failure { BridgeProtocol.emptyUpdateParams(JSONObject().put("url", "https://example.com/app.apk")) })
        assertTrue(BridgeProtocol.updateConfigureParams(JSONObject().put("auto_check", true)))
        assertFalse(BridgeProtocol.updateConfigureParams(JSONObject().put("auto_check", false)))
        for (params in listOf(JSONObject(), JSONObject().put("auto_check", "true"), JSONObject().put("auto_check", 1), JSONObject().put("auto_check", true).put("file", "/tmp/a.apk"))) {
            assertEquals("bad_params", failure { BridgeProtocol.updateConfigureParams(params) })
        }
    }

    @Test
    fun malformedMessagesGetNoAnswerWithoutAUsableId() {
        assertNull(rejection("not json").id)
        assertNull(rejection("[]").id)
        assertNull(rejection("""{"method":"info","params":{}}""").id)
        assertNull(rejection("""{"id":1,"method":"info","params":{}}""").id)
        assertNull(rejection(request(id = "")).id)
        assertNull(rejection(request(id = "a".repeat(BridgeProtocol.MAX_ID_CHARS + 1))).id)
        assertNull(rejection(request(id = "has space")).id)
        assertNull(rejection(request(id = "<script>")).id)
        assertEquals("bad_params", rejection("""{"id":"1","method":"info"}""").error)
        assertEquals("bad_params", rejection("""{"id":"1","method":"info","params":[]}""").error)
        assertEquals("bad_params", rejection("""{"id":"1","method":"info","params":"x"}""").error)
    }

    @Test
    fun sizeLimitsApplyPerMethod() {
        val big = "x".repeat(BridgeProtocol.MAX_MESSAGE_CHARS)
        assertEquals("message_too_large", rejection(request(method = "pushLabels", params = """{"labels":{},"pad":"$big"}""")).error)
        // saveFile alone may carry a large payload.
        val data = "A".repeat(BridgeProtocol.MAX_MESSAGE_CHARS * 2)
        assertEquals(BridgeMethod.SAVE_FILE, accepted(request(method = "saveFile", params = """{"name":"a","mime":"text/plain","data":"$data"}""")).method)
        val tooBig = "A".repeat(BridgeProtocol.MAX_SAVE_MESSAGE_CHARS + 1)
        assertEquals("message_too_large", BridgeProtocol.parse(tooBig).let { (it as ParsedRequest.Rejected).rejection.error })
    }

    @Test
    fun desktopIdsMustBeLowercaseHex64() {
        fun params(desktop: Any?) = JSONObject().put("desktop", desktop)
        assertEquals(DESKTOP_A, BridgeProtocol.desktopParam(params(DESKTOP_A)))
        for (bad in listOf(DESKTOP_A.uppercase(), DESKTOP_A.dropLast(1), DESKTOP_A + "0", "", "../../etc", JSONObject.NULL, 5)) {
            assertEquals("bad_desktop", failure { BridgeProtocol.desktopParam(params(bad)) })
        }
        assertEquals("bad_desktop", failure { BridgeProtocol.desktopParam(JSONObject()) })
    }

    @Test
    fun presenceRequiresBothFields() {
        val visible = BridgeProtocol.presenceParams(JSONObject("""{"desktop":"$DESKTOP_A","visible":true}"""))
        assertEquals(DESKTOP_A, visible.desktop)
        assertTrue(visible.visible)
        val none = BridgeProtocol.presenceParams(JSONObject("""{"desktop":null,"visible":true}"""))
        assertNull(none.desktop)
        assertEquals("bad_visible", failure { BridgeProtocol.presenceParams(JSONObject("""{"desktop":null,"visible":"yes"}""")) })
        assertEquals("bad_visible", failure { BridgeProtocol.presenceParams(JSONObject("""{"desktop":null}""")) })
        assertEquals("bad_desktop", failure { BridgeProtocol.presenceParams(JSONObject("""{"visible":true}""")) })
        assertEquals("bad_desktop", failure { BridgeProtocol.presenceParams(JSONObject("""{"desktop":"zz","visible":true}""")) })
    }

    @Test
    fun labelsAreBoundedAndCleaned() {
        val ok = BridgeProtocol.labelsParams(
            JSONObject("""{"labels":{"$DESKTOP_A":"  OmpDeck on host\u0007 ","$DESKTOP_B":""}}"""),
        )
        assertEquals(mapOf(DESKTOP_A to "OmpDeck on host"), ok)
        assertEquals(
            BridgeProtocol.MAX_LABEL_CHARS,
            BridgeProtocol.labelsParams(JSONObject("""{"labels":{"$DESKTOP_A":"${"é".repeat(500)}"}}""")).getValue(DESKTOP_A).length,
        )
        assertEquals("bad_labels", failure { BridgeProtocol.labelsParams(JSONObject("""{"labels":[]}""")) })
        assertEquals("bad_labels", failure { BridgeProtocol.labelsParams(JSONObject("""{}""")) })
        assertEquals("bad_desktop", failure { BridgeProtocol.labelsParams(JSONObject("""{"labels":{"nope":"x"}}""")) })
        assertEquals("bad_label", failure { BridgeProtocol.labelsParams(JSONObject("""{"labels":{"$DESKTOP_A":5}}""")) })
        val many = (0..BridgeProtocol.MAX_LABELS).joinToString(",") { "\"%064x\":\"l\"".format(it) }
        assertEquals("too_many_labels", failure { BridgeProtocol.labelsParams(JSONObject("""{"labels":{$many}}""")) })
    }

    @Test
    fun subscriptionNeedsDesktopAndStringLabel() {
        val s = BridgeProtocol.subscriptionParams(JSONObject("""{"desktop":"$DESKTOP_A","label":"Home"}"""))
        assertEquals("Home", s.label)
        assertEquals("bad_label", failure { BridgeProtocol.subscriptionParams(JSONObject("""{"desktop":"$DESKTOP_A"}""")) })
        assertEquals("bad_desktop", failure { BridgeProtocol.subscriptionParams(JSONObject("""{"desktop":"x","label":"Home"}""")) })
    }

    @Test
    fun enableParamsAcceptOnlyABoundedConfigString() {
        assertNull(BridgeProtocol.enableParams(JSONObject("{}")))
        assertEquals("{\"a\":1}", BridgeProtocol.enableParams(JSONObject().put("config", "{\"a\":1}")))
        assertEquals("bad_params", failure { BridgeProtocol.enableParams(JSONObject("""{"config":null}""")) })
        assertEquals("bad_params", failure { BridgeProtocol.enableParams(JSONObject("""{"config":{"a":1}}""")) })
        assertEquals("bad_params", failure { BridgeProtocol.enableParams(JSONObject("""{"config":"  "}""")) })
        assertEquals("bad_params", failure { BridgeProtocol.enableParams(JSONObject("""{"config":"{}","extra":1}""")) })
        assertEquals("bad_params", failure { BridgeProtocol.enableParams(JSONObject("""{"other":"{}"}""")) })
        val big = "x".repeat(BridgeProtocol.MAX_CONFIG_CHARS + 1)
        assertEquals("bad_params", failure { BridgeProtocol.enableParams(JSONObject().put("config", big)) })
    }

    @Test
    fun saveFileNamesAndMimeTypesAreSanitized() {
        fun save(name: String, mime: String) = BridgeProtocol.saveParams(JSONObject().put("name", name).put("mime", mime).put("data", ""))
        assertEquals("report.txt", save("report.txt", "text/plain").name)
        assertEquals("passwd", save("../../etc/passwd", "text/plain").name)
        assertEquals("evil.sh", save("C:\\Windows\\evil.sh", "text/plain").name)
        assertEquals("hidden", save("...hidden", "text/plain").name)
        assertEquals("a b", save("a\u0000 \u0007b", "text/plain").name)
        assertEquals("download", save("", "text/plain").name)
        assertEquals("download", save("../", "text/plain").name)
        assertEquals(BridgeProtocol.MAX_FILE_NAME_CHARS, save("n".repeat(1000), "text/plain").name.length)
        assertEquals("text/plain", save("a", " Text/Plain ").mime)
        assertEquals("application/json", save("a", "application/json").mime)
        for (bad in listOf("", "*/*", "text", "text/plain; charset=x", "text/pl ain", "../x", "a/b/c", "intent:", "t".repeat(300) + "/x")) {
            assertEquals(bad, "application/octet-stream", save("a", bad).mime)
        }
        assertEquals("bad_params", failure { BridgeProtocol.saveParams(JSONObject("""{"name":"a","mime":"text/plain"}""")) })
        assertEquals("bad_params", failure { BridgeProtocol.saveParams(JSONObject("""{"name":1,"mime":"text/plain","data":""}""")) })
    }

    @Test
    fun saveDataIsStrictBase64WithinTheCap() {
        val bytes = ByteArray(1000) { it.toByte() }
        assertEquals(bytes.toList(), BridgeProtocol.decodeSaveData(Base64.getEncoder().encodeToString(bytes)).toList())
        assertEquals("bad_data", failure { BridgeProtocol.decodeSaveData("not base64!") })
        assertEquals("bad_data", failure { BridgeProtocol.decodeSaveData("AAA") })
        assertEquals("bad_data", failure { BridgeProtocol.decodeSaveData("AA\nAA") })
        assertEquals("bad_data", failure { BridgeProtocol.decodeSaveData("AAA-") })
        val atCap = ByteArray(BridgeProtocol.MAX_SAVE_BYTES)
        assertEquals(atCap.size, BridgeProtocol.decodeSaveData(Base64.getEncoder().encodeToString(atCap)).size)
        val over = ByteArray(BridgeProtocol.MAX_SAVE_BYTES + 1)
        assertEquals("file_too_large", failure { BridgeProtocol.decodeSaveData(Base64.getEncoder().encodeToString(over)) })
    }

    @Test
    fun repliesAreTheContractsJson() {
        val empty = JSONObject(BridgeProtocol.result("7", null))
        assertEquals("7", empty.getString("id"))
        assertTrue(empty.has("result") && empty.isNull("result"))
        val failed = JSONObject(BridgeProtocol.error("7", "cancelled"))
        assertEquals("7", failed.getString("id"))
        assertEquals("cancelled", failed.getString("error"))
        assertFalse(failed.has("result"))
        val result = JSONObject(BridgeProtocol.result("7", JSONObject().put("platform", "android")))
        assertEquals("android", result.getJSONObject("result").getString("platform"))
        val event = JSONObject(BridgeProtocol.event("visibility", JSONObject().put("visible", false)))
        assertEquals("visibility", event.getString("event"))
        assertEquals(false, event.getJSONObject("detail").getBoolean("visible"))
    }
}
