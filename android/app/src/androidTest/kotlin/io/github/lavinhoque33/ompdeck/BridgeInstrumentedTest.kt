package io.github.lavinhoque33.ompdeck

import android.content.Intent
import android.webkit.WebView
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import androidx.lifecycle.Lifecycle
import io.github.lavinhoque33.ompdeck.push.OpenRequest
import io.github.lavinhoque33.ompdeck.push.PushRuntime
import io.github.lavinhoque33.ompdeck.web.AssetPolicy
import io.github.lavinhoque33.ompdeck.web.UrlPolicy
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** The bundled page, its bridge and the WebView's lock-down, on a real WebView (offline: nothing here needs the network). */
class BridgeInstrumentedTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private var scenario: ActivityScenario<MainActivity>? = null

    @After
    fun tearDown() {
        scenario?.close()
    }

    private fun launch(intent: Intent? = null): ActivityScenario<MainActivity> {
        val s = if (intent == null) ActivityScenario.launch(MainActivity::class.java) else ActivityScenario.launch<MainActivity>(intent)
        scenario = s
        return s
    }

    private fun webView(s: ActivityScenario<MainActivity>): WebView {
        var view: WebView? = null
        s.onActivity { view = it.webViewForTesting() }
        return view!!
    }

    /** Evaluates [script] on the page and returns the JSON-encoded result. */
    private fun js(view: WebView, script: String): String {
        val latch = CountDownLatch(1)
        var out = "null"
        instrumentation.runOnMainSync {
            view.evaluateJavascript(script) {
                out = it
                latch.countDown()
            }
        }
        assertTrue("script timed out", latch.await(20, TimeUnit.SECONDS))
        return out
    }

    private fun waitFor(what: String, timeoutMs: Long = 30_000, check: () -> Boolean) {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (check()) return
            Thread.sleep(150)
        }
        throw AssertionError("timed out waiting for $what")
    }

    private fun loaded(view: WebView) {
        waitFor("the bundled page") { js(view, "document.readyState") == "\"complete\"" && js(view, "typeof window.OmpDeckAndroid") == "\"object\"" }
    }

    /** Listens (without taking over the app's own `onmessage`), sends [request] and returns the reply with the same id. */
    private fun call(view: WebView, id: String, method: String, params: String = "{}"): JSONObject {
        js(
            view,
            """(function(){
              if (!window.__t) { window.__t = {replies:{}, events:[]};
                window.OmpDeckAndroid.addEventListener('message', function(e){
                  try { var m = JSON.parse(e.data); if (m.id) window.__t.replies[m.id] = m; else if (m.event) window.__t.events.push(m); } catch (x) {}
                }); }
              window.OmpDeckAndroid.postMessage(JSON.stringify({id:'$id',method:'$method',params:$params})); return 1; })()""",
        )
        var reply: JSONObject? = null
        waitFor("reply $id", 20_000) {
            val text = js(view, "JSON.stringify(window.__t.replies['$id'] || null)")
            // evaluateJavascript JSON-encodes the returned string once more.
            val decoded = if (text == "null") null else org.json.JSONTokener(text).nextValue() as String
            reply = if (decoded == null || decoded == "null") null else JSONObject(decoded)
            reply != null
        }
        return reply!!
    }

    private fun events(view: WebView): List<JSONObject> {
        val text = org.json.JSONTokener(js(view, "JSON.stringify(window.__t ? window.__t.events : [])")).nextValue() as String
        val array = org.json.JSONArray(text)
        return (0 until array.length()).map { array.getJSONObject(it) }
    }

    @Test
    fun bundledPageLoadsOfflineAndTheBridgeAnswersTheContractMethods() {
        val view = webView(launch())
        loaded(view)
        assertEquals("\"${UrlPolicy.ORIGIN}\"", js(view, "location.origin"))

        val info = call(view, "t-info", "info").getJSONObject("result")
        assertEquals("android", info.getString("platform"))
        val version = instrumentation.targetContext.packageManager.getPackageInfo(instrumentation.targetContext.packageName, 0).versionName
        assertEquals(version, info.getString("version"))

        val state = call(view, "t-state", "pushState").getJSONObject("result")
        assertTrue(state.getString("permission") in setOf("default", "granted", "denied"))
        assertTrue(state.has("configured"))
        assertTrue(state.has("token"))

        assertEquals("unknown_method", call(view, "t-bad", "evaluateScript").getString("error"))
        assertEquals("bad_desktop", call(view, "t-forget", "pushForget", """{"desktop":"../../x"}""").getString("error"))
        assertEquals("bad_visible", call(view, "t-presence", "setPresence", """{"desktop":null,"visible":"yes"}""").getString("error"))
    }

    @Test
    fun responsesCarryTheCspAndUnknownPathsAre404() {
        val view = webView(launch())
        loaded(view)
        // fetch is async: poll a global for its outcome.
        js(view, "window.__csp = null; fetch('/index.html').then(r => { window.__csp = r.headers.get('content-security-policy') }); 1")
        waitFor("fetch") { js(view, "window.__csp") != "null" }
        assertEquals("\"${AssetPolicy.CSP}\"", js(view, "window.__csp"))

        js(view, "window.__s = null; fetch('/does-not-exist.js').then(r => { window.__s = r.status }); 1")
        waitFor("404") { js(view, "window.__s") != "null" }
        assertEquals("404", js(view, "window.__s"))
    }

    @Test
    fun thePageCannotReachThirdPartiesOrCleartext() {
        val view = webView(launch())
        loaded(view)
        for (url in listOf("http://example.com/", "https://example.com/", "http://127.0.0.1:9/")) {
            js(view, "window.__r = null; fetch('$url').then(() => { window.__r = 'reached' }, () => { window.__r = 'blocked' }); 1")
            waitFor("fetch $url") { js(view, "window.__r") != "null" }
            assertEquals(url, "\"blocked\"", js(view, "window.__r"))
        }
    }

    @Test
    fun scriptsCannotNavigateTheAppAwayOrOpenWindows() {
        val s = launch()
        val view = webView(s)
        loaded(view)
        val before = js(view, "location.href")
        // No user gesture: neither a redirect nor window.open may leave the bundle or reach the system browser.
        js(view, "location.href = 'https://example.com/'; 1")
        js(view, "window.open('https://example.com/'); 1")
        Thread.sleep(2_000)
        assertEquals(before, js(view, "location.href"))
        var url: String? = null
        instrumentation.runOnMainSync { url = view.url }
        assertTrue(url!!, UrlPolicy.isAppUrl(url!!))
    }

    @Test
    fun backIsForwardedToThePageOnceItHasHandshaken() {
        val s = launch()
        val view = webView(s)
        loaded(view)
        call(view, "t-info", "info")
        // The app's own back handler would finish the activity at its root: swallow that request for this test.
        js(
            view,
            """(function(){ var send = window.OmpDeckAndroid.postMessage.bind(window.OmpDeckAndroid);
              window.OmpDeckAndroid.postMessage = function(m){ try { if (JSON.parse(m).method === 'finish') return; } catch (x) {} send(m); }; return 1; })()""",
        )
        s.onActivity { it.onBackPressedDispatcher.onBackPressed() }
        waitFor("back event") { events(view).any { it.getString("event") == "back" } }
    }

    @Test
    fun updaterSettingsAndEventsSurviveStopStartAndActivityRecreation() {
        val s = launch()
        var view = webView(s)
        loaded(view)
        val original = call(view, "u-original", "updateState").getJSONObject("result").getBoolean("auto_check")
        try {
            assertFalse(call(view, "u-disable", "updateConfigure", """{"auto_check":false}""").getJSONObject("result").getBoolean("auto_check"))
            waitFor("persisted automatic-check setting") {
                !instrumentation.targetContext.getSharedPreferences("android_updates", 0).getBoolean("auto_check", true)
            }
            assertEquals("bad_params", call(view, "u-url", "updateInstall", """{"url":"https://example.com/update.apk"}""").getString("error"))
            val before = events(view).count { it.getString("event") == "update" }
            s.moveToState(Lifecycle.State.CREATED)
            s.moveToState(Lifecycle.State.RESUMED)
            waitFor("reattached updater listener") { events(view).count { it.getString("event") == "update" } > before }
            s.recreate()
            view = webView(s)
            loaded(view)
            val restored = call(view, "u-restored", "updateState").getJSONObject("result")
            assertFalse(restored.getBoolean("auto_check"))
            assertEquals(BuildConfig.VERSION_NAME, restored.getString("current"))
            assertTrue(restored.getString("phase") !in setOf("downloading", "installing"))
            call(view, "u-events", "updateConfigure", """{"auto_check":false}""")
            waitFor("recreated updater listener") { events(view).any { it.getString("event") == "update" && !it.getJSONObject("detail").getBoolean("auto_check") } }
        } finally {
            call(view, "u-restore", "updateConfigure", """{"auto_check":$original}""")
        }
    }

    @Test
    fun onlyAnIntentCarryingTheInstallSecretOpensARoute() {
        val context = instrumentation.targetContext
        val secret = PushRuntime.get(context).store.openSecret()
        val desktop = "cd".repeat(32)
        fun open(token: String?) = Intent(context, MainActivity::class.java)
            .setAction(OpenRequest.ACTION)
            .putExtra(OpenRequest.EXTRA_URL, "/#s=abc&d=$desktop")
            .putExtra(OpenRequest.EXTRA_DESKTOP, desktop)
            .apply { if (token != null) putExtra(OpenRequest.EXTRA_SECRET, token) }

        var accepted: List<String> = listOf("unset")
        val forged = launch(open("guessed-secret"))
        forged.onActivity { accepted = it.acceptedOpensForTesting() }
        assertEquals(emptyList<String>(), accepted)
        forged.close()
        scenario = null

        val none = launch(open(null))
        none.onActivity { accepted = it.acceptedOpensForTesting() }
        assertEquals(emptyList<String>(), accepted)
        none.close()
        scenario = null

        val genuine = launch(open(secret))
        genuine.onActivity { accepted = it.acceptedOpensForTesting() }
        assertEquals(listOf("/#s=abc&d=$desktop"), accepted)
        assertFalse(accepted.isEmpty())
    }
}
