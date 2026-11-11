package io.github.lavinhoque33.ompdeck

import android.Manifest
import android.annotation.SuppressLint
import android.app.AlertDialog
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.Message
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.GeolocationPermissions
import android.webkit.PermissionRequest
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContract
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.FileProvider
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.WebMessageCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import io.github.lavinhoque33.ompdeck.bridge.BridgeException
import io.github.lavinhoque33.ompdeck.bridge.BridgeMethod
import io.github.lavinhoque33.ompdeck.bridge.BridgeProtocol
import io.github.lavinhoque33.ompdeck.bridge.BridgeRequest
import io.github.lavinhoque33.ompdeck.bridge.EventGate
import io.github.lavinhoque33.ompdeck.bridge.ParsedRequest
import io.github.lavinhoque33.ompdeck.bridge.SaveWriter
import io.github.lavinhoque33.ompdeck.push.Base64Url
import io.github.lavinhoque33.ompdeck.push.ConfigException
import io.github.lavinhoque33.ompdeck.push.GoogleServices
import io.github.lavinhoque33.ompdeck.push.OpenRequest
import io.github.lavinhoque33.ompdeck.push.Presence
import io.github.lavinhoque33.ompdeck.push.PushEvents
import io.github.lavinhoque33.ompdeck.push.PushRuntime
import io.github.lavinhoque33.ompdeck.update.NativeUpdateState
import io.github.lavinhoque33.ompdeck.update.NativeUpdater
import io.github.lavinhoque33.ompdeck.web.AppAssets
import io.github.lavinhoque33.ompdeck.web.UrlPolicy
import org.json.JSONObject
import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.util.UUID
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Hosts the bundled phone UI in a locked-down WebView and the native bridge it talks to.
 *
 * Trust boundary: the page is served only from the bundled assets (https://appassets.androidplatform.net); every
 * other navigation goes to the system browser. The bridge is a WebMessageListener scoped to exactly that origin and
 * the main frame, with a method allow-list and validated parameters ([BridgeProtocol]); there is no
 * `addJavascriptInterface`.
 */
class MainActivity : ComponentActivity() {
    private lateinit var runtime: PushRuntime
    private lateinit var updater: NativeUpdater
    private var activityResumed = false
    private var updateSettingsPending = false
    private var updatePermissionReturnPending = false
    private var updateInstallerPending = false
    private val updateListener = object : NativeUpdater.Listener {
        override fun updateChanged(state: NativeUpdateState) { sendEvent("update", state.json()) }
        override fun installReady(file: File) { launchUpdateInstaller(file) }
    }
    private lateinit var appAssets: AppAssets
    private lateinit var webView: WebView
    private val main = Handler(Looper.getMainLooper())
    private val io: ExecutorService = Executors.newFixedThreadPool(3)
    private var destroyed = false

    // ---- bridge state (main thread) --------------------------------------------------------------------------
    private var proxy: JavaScriptReplyProxy? = null
    private val openGate = EventGate()
    private val acceptedOpens = mutableListOf<String>()
    private var started = false
    private var lastPushSnapshot: String? = null
    private var tokenKicked = false

    // ---- pending native interactions (main thread) -----------------------------------------------------------
    private var fileCallback: ValueCallback<Array<Uri>>? = null
    private var captureFile: File? = null
    private var pendingSave: PendingSave? = null
    private var enableReply: Reply? = null
    private var permissionNext: (() -> Unit)? = null
    private var lastExternalAt = 0L

    private class PendingSave(val reply: Reply, val bytes: ByteArray)

    private val filePicker = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        val callback = fileCallback ?: return@registerForActivityResult
        fileCallback = null
        callback.onReceiveValue(WebChromeClient.FileChooserParams.parseResult(result.resultCode, result.data))
    }

    private val takePicture = registerForActivityResult(ActivityResultContracts.TakePicture()) { taken ->
        val callback = fileCallback
        val file = captureFile
        fileCallback = null
        captureFile = null
        if (callback == null) return@registerForActivityResult
        if (taken && file != null && file.length() > 0) {
            callback.onReceiveValue(arrayOf(FileProvider.getUriForFile(this, "$packageName.captures", file)))
        } else {
            file?.delete()
            callback.onReceiveValue(null)
        }
    }

    private val importPicker = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri -> onConfigPicked(uri) }

    private val permissionLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        val next = permissionNext
        permissionNext = null
        next?.invoke()
    }

    private val updateSettings = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) {
        if (updateSettingsPending) {
            updateSettingsPending = false
            updatePermissionReturnPending = true
            continueUpdatePermission()
        }
    }

    private val updateInstaller = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        if (updateInstallerPending) {
            updateInstallerPending = false
            updater.installationFinished(
                if (result.resultCode == RESULT_OK) "Installation finished. Restart OmpDeck to use the new version."
                else "Installation was canceled or did not finish. The verified APK is ready to retry.",
            )
        }
    }

    private class SaveInput(val name: String, val mime: String)

    private val savePicker = registerForActivityResult(
        object : ActivityResultContract<SaveInput, Uri?>() {
            override fun createIntent(context: Context, input: SaveInput): Intent =
                Intent(Intent.ACTION_CREATE_DOCUMENT)
                    .addCategory(Intent.CATEGORY_OPENABLE)
                    .setType(input.mime)
                    .putExtra(Intent.EXTRA_TITLE, input.name)

            override fun parseResult(resultCode: Int, intent: Intent?): Uri? =
                if (resultCode == RESULT_OK) intent?.data else null
        },
    ) { uri -> onSavePicked(uri) }

    // ---- lifecycle -------------------------------------------------------------------------------------------

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT), navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT))
        super.onCreate(savedInstanceState)
        runtime = PushRuntime.get(this)
        updater = NativeUpdater.get(this)
        updateSettingsPending = savedInstanceState?.getBoolean("update_settings_pending") ?: false
        updatePermissionReturnPending = savedInstanceState?.getBoolean("update_permission_return_pending") ?: false
        updateInstallerPending = savedInstanceState?.getBoolean("update_installer_pending") ?: false
        if (updateSettingsPending || updatePermissionReturnPending || updateInstallerPending) updater.holdExternalInteraction()
        appAssets = AppAssets(assets)
        clearOldCaptures()

        val root = FrameLayout(this)
        root.setBackgroundColor(getColor(R.color.ompdeck_background))
        setContentView(root)
        // System bars and the keyboard shrink the web view instead of overlapping it (the page cannot read insets).
        ViewCompat.setOnApplyWindowInsetsListener(root) { view, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
            view.setPadding(bars.left, bars.top, bars.right, maxOf(bars.bottom, ime.bottom))
            WindowInsetsCompat.CONSUMED
        }

        onBackPressedDispatcher.addCallback(
            this,
            object : OnBackPressedCallback(true) {
                override fun handleOnBackPressed() {
                    when {
                        openGate.isReady && proxy != null -> sendEvent("back", JSONObject())
                        ::webView.isInitialized && webView.canGoBack() -> webView.goBack()
                        else -> finish()
                    }
                }
            },
        )
        PushEvents.setListener { main.post { if (!destroyed) sendEvent("push_changed", JSONObject()) } }

        if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
            AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
                .setTitle(R.string.webview_unsupported_title)
                .setMessage(R.string.webview_unsupported_message)
                .setPositiveButton(android.R.string.ok) { _, _ -> finish() }
                .setOnCancelListener { finish() }
                .show()
            return
        }

        webView = createWebView()
        root.addView(webView, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        if (savedInstanceState == null) handleIntent(intent)
        webView.loadUrl(UrlPolicy.START_URL)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        started = true
        Presence.shared.setStarted(true)
        sendVisibility()
        checkPushChanged()
        updater.attach(updateListener)
    }

    override fun onResume() {
        super.onResume()
        activityResumed = true
        updater.resume(updateListener)
        continueUpdatePermission()
    }

    override fun onPause() {
        activityResumed = false
        updater.pause(updateListener)
        super.onPause()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        outState.putBoolean("update_settings_pending", updateSettingsPending)
        outState.putBoolean("update_permission_return_pending", updatePermissionReturnPending)
        outState.putBoolean("update_installer_pending", updateInstallerPending)
        super.onSaveInstanceState(outState)
    }

    override fun onStop() {
        started = false
        updater.detach(updateListener, isChangingConfigurations)
        Presence.shared.setStarted(false)
        // The page must hear this while still running, so the desktop learns the phone is no longer watching.
        sendVisibility()
        super.onStop()
    }

    override fun onDestroy() {
        destroyed = true
        PushEvents.setListener(null)
        Presence.shared.setStarted(false)
        fileCallback?.onReceiveValue(null)
        fileCallback = null
        io.shutdown()
        if (::webView.isInitialized) {
            (webView.parent as? ViewGroup)?.removeView(webView)
            webView.destroy()
        }
        super.onDestroy()
    }

    // ---- web view --------------------------------------------------------------------------------------------

    private fun createWebView(): WebView {
        WebView.setWebContentsDebuggingEnabled(BuildConfig.DEBUG)
        val view = WebView(this)
        view.setBackgroundColor(getColor(R.color.ompdeck_background))
        applyLockedDownSettings(view.settings)
        CookieManager.getInstance().setAcceptCookie(false)
        CookieManager.getInstance().setAcceptThirdPartyCookies(view, false)
        WebViewCompat.addWebMessageListener(view, BRIDGE_NAME, setOf(UrlPolicy.ORIGIN), BridgeListener())
        view.webViewClient = AppClient()
        view.webChromeClient = AppChrome()
        return view
    }

    @SuppressLint("SetJavaScriptEnabled") // The bundled page is the app; only it ever loads here (see UrlPolicy).
    private fun applyLockedDownSettings(settings: WebSettings) {
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        settings.allowFileAccess = false
        settings.allowContentAccess = false
        settings.mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
        settings.javaScriptCanOpenWindowsAutomatically = false
        settings.setSupportMultipleWindows(true)
        settings.setGeolocationEnabled(false)
        settings.setSupportZoom(false)
        settings.builtInZoomControls = false
        settings.mediaPlaybackRequiresUserGesture = true
    }

    private inner class AppClient : WebViewClient() {
        override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? =
            appAssets.intercept(request.url)

        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
            val url = request.url.toString()
            if (UrlPolicy.isAppUrl(url)) return false
            // Anything else never loads here: a link the user tapped goes to the system, the rest is dropped.
            if (request.hasGesture()) openExternal(url)
            return true
        }

        override fun onPageStarted(view: WebView, url: String?, favicon: android.graphics.Bitmap?) {
            // A new document starts a new handshake; events queue until its `info` arrives.
            openGate.reset()
            proxy = null
        }

        override fun onRenderProcessGone(view: WebView, detail: android.webkit.RenderProcessGoneDetail): Boolean {
            main.post { if (!destroyed) recreate() }
            return true
        }
    }

    private inner class AppChrome : WebChromeClient() {
        override fun onShowFileChooser(
            webView: WebView,
            callback: ValueCallback<Array<Uri>>,
            params: FileChooserParams,
        ): Boolean {
            fileCallback?.onReceiveValue(null)
            fileCallback = callback
            val imageOnly = params.acceptTypes.isNotEmpty() && params.acceptTypes.all { it.trim().startsWith("image/") }
            if (params.isCaptureEnabled && imageOnly) {
                startCapture()
                return true
            }
            return try {
                filePicker.launch(params.createIntent())
                true
            } catch (_: ActivityNotFoundException) {
                fileCallback = null
                callback.onReceiveValue(null)
                true
            }
        }

        override fun onCreateWindow(view: WebView, isDialog: Boolean, isUserGesture: Boolean, resultMsg: Message): Boolean {
            if (!isUserGesture) return false
            // window.open / target=_blank: a throwaway, script-less, network-less view only to learn the URL.
            val sink = WebView(this@MainActivity)
            sink.settings.javaScriptEnabled = false
            sink.settings.allowFileAccess = false
            sink.settings.allowContentAccess = false
            val routed = AtomicBoolean(false)
            fun route(url: String) {
                if (!routed.compareAndSet(false, true)) return
                main.post {
                    if (!UrlPolicy.isAppUrl(url)) openExternal(url)
                    sink.stopLoading()
                    sink.destroy()
                }
            }
            sink.webViewClient = object : WebViewClient() {
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                    route(request.url.toString())
                    return true
                }

                override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
                    if (request.isForMainFrame) route(request.url.toString())
                    return WebResourceResponse("text/plain", "utf-8", ByteArrayInputStream(ByteArray(0)))
                }
            }
            (resultMsg.obj as WebView.WebViewTransport).webView = sink
            resultMsg.sendToTarget()
            return true
        }

        // The page never gets the camera, microphone or location.
        override fun onPermissionRequest(request: PermissionRequest) = request.deny()

        override fun onGeolocationPermissionsShowPrompt(origin: String, callback: GeolocationPermissions.Callback) =
            callback.invoke(origin, false, false)
    }

    private fun openExternal(url: String) {
        val target = UrlPolicy.externalTarget(url) ?: return
        val now = SystemClock.elapsedRealtime()
        if (now - lastExternalAt < EXTERNAL_MIN_INTERVAL_MS) return
        lastExternalAt = now
        try {
            startActivity(
                Intent(Intent.ACTION_VIEW, Uri.parse(target))
                    .addCategory(Intent.CATEGORY_BROWSABLE)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } catch (_: ActivityNotFoundException) {
            // No handler installed: nothing to open.
        }
    }

    private fun startCapture() {
        val dir = File(cacheDir, CAPTURE_DIR).apply { mkdirs() }
        val file = File(dir, "${UUID.randomUUID()}.jpg")
        captureFile = file
        try {
            takePicture.launch(FileProvider.getUriForFile(this, "$packageName.captures", file))
        } catch (_: ActivityNotFoundException) {
            captureFile = null
            fileCallback?.onReceiveValue(null)
            fileCallback = null
        }
    }

    private fun clearOldCaptures() {
        File(cacheDir, CAPTURE_DIR).listFiles()?.forEach { it.delete() }
    }

    // ---- bridge ----------------------------------------------------------------------------------------------

    private inner class BridgeListener : WebViewCompat.WebMessageListener {
        override fun onPostMessage(
            view: WebView,
            message: WebMessageCompat,
            sourceOrigin: Uri,
            isMainFrame: Boolean,
            replyProxy: JavaScriptReplyProxy,
        ) {
            // The listener is already scoped to the app origin; re-checked so a mistake there cannot widen it.
            if (!isMainFrame || sourceOrigin.toString() != UrlPolicy.ORIGIN) return
            if (message.type != WebMessageCompat.TYPE_STRING) return
            val raw = message.data ?: return
            proxy = replyProxy
            when (val parsed = BridgeProtocol.parse(raw)) {
                is ParsedRequest.Rejected -> {
                    val id = parsed.rejection.id ?: return
                    replyProxy.postMessage(BridgeProtocol.error(id, parsed.rejection.error))
                }
                is ParsedRequest.Ok -> {
                    val reply = Reply(replyProxy, parsed.request.id)
                    try {
                        dispatch(parsed.request, reply)
                    } catch (e: BridgeException) {
                        reply.fail(e.message ?: "error")
                    } catch (e: Exception) {
                        Log.w(TAG, "bridge ${parsed.request.method.wire} failed: ${e.javaClass.simpleName}")
                        reply.fail("internal_error")
                    }
                }
            }
        }
    }

    /** One answer per request, from any thread. */
    private inner class Reply(val proxy: JavaScriptReplyProxy, private val id: String) {
        private val answered = AtomicBoolean(false)

        fun ok(result: Any? = null) = answer(BridgeProtocol.result(id, result))

        fun fail(message: String) = answer(BridgeProtocol.error(id, message))

        private fun answer(text: String) {
            if (!answered.compareAndSet(false, true)) return
            runOnMain { proxy.postMessage(text) }
        }
    }

    private fun runOnMain(block: () -> Unit) {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            if (!destroyed) block()
        } else {
            main.post { if (!destroyed) block() }
        }
    }

    /** Runs [block] off the main thread and answers [reply] with its result or error. */
    private fun background(reply: Reply, block: () -> Any?) {
        try {
            io.execute {
                try {
                    reply.ok(block())
                } catch (e: BridgeException) {
                    reply.fail(e.message ?: "error")
                } catch (e: IOException) {
                    Log.w(TAG, "storage unavailable: ${e.javaClass.simpleName}")
                    reply.fail("storage_unavailable")
                } catch (e: Exception) {
                    Log.w(TAG, "bridge task failed: ${e.javaClass.simpleName}")
                    reply.fail("internal_error")
                }
            }
        } catch (_: java.util.concurrent.RejectedExecutionException) {
            reply.fail("shutting_down")
        }
    }

    private fun dispatch(request: BridgeRequest, reply: Reply) {
        val params = request.params
        when (request.method) {
            BridgeMethod.INFO -> {
                reply.ok(JSONObject().put("platform", "android").put("version", BuildConfig.VERSION_NAME))
                val held = openGate.markReady()
                sendVisibility()
                for (url in held) sendEvent("open", JSONObject().put("url", url))
                sendEvent("update", updater.snapshot())
            }
            BridgeMethod.PUSH_STATE -> background(reply) { pushState().also { kickTokenFetch() } }
            BridgeMethod.PUSH_ENABLE -> pushEnable(reply)
            BridgeMethod.PUSH_SUBSCRIPTION -> {
                val subscription = BridgeProtocol.subscriptionParams(params)
                background(reply) { pushSubscription(subscription) }
            }
            BridgeMethod.PUSH_FORGET -> {
                val desktop = BridgeProtocol.desktopParam(params)
                background(reply) {
                    runtime.store.forget(desktop)
                    runtime.notifier.cancelDesktop(desktop)
                    null
                }
            }
            BridgeMethod.PUSH_LABELS -> {
                val labels = BridgeProtocol.labelsParams(params)
                background(reply) {
                    runtime.store.replaceLabels(labels)
                    null
                }
            }
            BridgeMethod.SET_PRESENCE -> {
                val presence = BridgeProtocol.presenceParams(params)
                Presence.shared.set(presence.desktop, presence.visible)
                reply.ok(null)
            }
            BridgeMethod.SAVE_FILE -> saveFile(BridgeProtocol.saveParams(params), reply)
            BridgeMethod.UPDATE_STATE -> {
                BridgeProtocol.emptyUpdateParams(params)
                reply.ok(updater.snapshot())
            }
            BridgeMethod.UPDATE_CHECK -> {
                BridgeProtocol.emptyUpdateParams(params)
                reply.ok(updater.check())
            }
            BridgeMethod.UPDATE_INSTALL -> {
                BridgeProtocol.emptyUpdateParams(params)
                if (!activityResumed) throw BridgeException("app_not_foreground")
                reply.ok(updater.install())
            }
            BridgeMethod.UPDATE_CONFIGURE -> reply.ok(updater.configure(BridgeProtocol.updateConfigureParams(params)))
            BridgeMethod.FINISH -> {
                reply.ok(null)
                finish()
            }
        }
    }

    private fun sendEvent(name: String, detail: JSONObject) {
        val target = proxy
        if (!openGate.isReady || target == null) return
        target.postMessage(BridgeProtocol.event(name, detail))
    }

    private fun sendVisibility() {
        sendEvent("visibility", JSONObject().put("visible", started))
    }

    // No settings/installer UI is reachable from update checks or lifecycle callbacks alone.
    private fun launchUpdateInstaller(file: File) {
        if (!activityResumed) {
            updater.installationFinished("The update is ready. Return to OmpDeck and tap Install to continue.")
            return
        }
        try {
            if (!packageManager.canRequestPackageInstalls()) {
                updateSettingsPending = true
                updater.awaitingPermission()
                updateSettings.launch(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:$packageName")))
                return
            }
            val uri = FileProvider.getUriForFile(this, "$packageName.updates", file)
            val intent = Intent(Intent.ACTION_INSTALL_PACKAGE)
                .setDataAndType(uri, "application/vnd.android.package-archive")
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                .putExtra(Intent.EXTRA_RETURN_RESULT, true)
            updateInstallerPending = true
            updateInstaller.launch(intent)
        } catch (_: ActivityNotFoundException) {
            updateSettingsPending = false
            updateInstallerPending = false
            updater.installationFinished("Android has no app-update installer or install-permission settings available.")
        } catch (_: Exception) {
            updateSettingsPending = false
            updateInstallerPending = false
            updater.installationFinished("Android could not open the app installer. Tap Install to verify and retry.")
        }
    }

    private fun continueUpdatePermission() {
        if (!activityResumed || !updatePermissionReturnPending) return
        updatePermissionReturnPending = false
        updater.permissionReturned(packageManager.canRequestPackageInstalls())
    }

    // ---- push ------------------------------------------------------------------------------------------------

    private fun pushState(): JSONObject {
        val configured = runtime.isConfigured()
        return JSONObject()
            .put("permission", runtime.permission())
            .put("configured", configured)
            .put("token", runtime.store.token() ?: JSONObject.NULL)
    }

    /** A configured app with no token yet (first start after an import) asks Firebase once; the result arrives as push_changed. */
    private fun kickTokenFetch() {
        if (tokenKicked || !runtime.isConfigured() || runtime.store.token() != null) return
        tokenKicked = true
        try {
            io.execute {
                try {
                    runtime.fetchToken()
                } catch (_: Exception) {
                    // Surfaces through pushEnable / pushSubscription, which report the reason.
                }
            }
        } catch (_: java.util.concurrent.RejectedExecutionException) {
            // Activity is going away.
        }
    }

    private fun pushSubscription(subscription: BridgeProtocol.Subscription): JSONObject {
        if (!runtime.isConfigured()) throw BridgeException("firebase_not_configured")
        if (runtime.permission() != "granted") throw BridgeException("notifications_not_allowed")
        val token = runtime.fetchToken()
        val key = runtime.store.getOrCreateKey(subscription.desktop) ?: throw BridgeException("too_many_desktops")
        if (subscription.label.isNotEmpty()) runtime.store.setLabel(subscription.desktop, subscription.label)
        return JSONObject().put("token", token).put("key", Base64Url.encode(key)).put("desktop", subscription.desktop)
    }

    /** Reports push_changed when the state the page last saw is not the current one. */
    private fun checkPushChanged() {
        try {
            io.execute {
                try {
                    val state = pushState()
                    val snapshot = "${state.getString("permission")}|${state.getBoolean("configured")}|${state.opt("token")}"
                    runOnMain {
                        val before = lastPushSnapshot
                        lastPushSnapshot = snapshot
                        if (before != null && before != snapshot) sendEvent("push_changed", JSONObject())
                    }
                } catch (_: Exception) {
                    // Nothing to report.
                }
            }
        } catch (_: java.util.concurrent.RejectedExecutionException) {
            // Activity is going away.
        }
    }

    // pushEnable: [import config] -> [notification permission] -> [token] -> state. Every step ends in finishEnable.

    private fun pushEnable(reply: Reply) {
        if (enableReply != null) {
            reply.fail("busy")
            return
        }
        enableReply = reply
        io.execute {
            val configured = try {
                runtime.isConfigured()
            } catch (_: Exception) {
                false
            }
            runOnMain { if (configured) continueEnable() else askImport() }
        }
    }

    private fun finishEnable() {
        val reply = enableReply ?: return
        enableReply = null
        background(reply) { pushState() }
        checkPushChanged()
    }

    private fun askImport() {
        AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
            .setTitle(R.string.push_setup_title)
            .setMessage(getString(R.string.push_setup_message, packageName))
            .setPositiveButton(R.string.push_setup_import) { _, _ -> launchImportPicker() }
            .setNegativeButton(R.string.push_setup_cancel) { _, _ -> finishEnable() }
            .setOnCancelListener { finishEnable() }
            .show()
    }

    private fun launchImportPicker() {
        try {
            importPicker.launch(arrayOf("*/*"))
        } catch (_: ActivityNotFoundException) {
            showImportError(getString(R.string.save_unavailable))
        }
    }

    private fun onConfigPicked(uri: Uri?) {
        if (uri == null) {
            finishEnable()
            return
        }
        io.execute {
            try {
                val text = readBounded(uri, GoogleServices.MAX_BYTES)
                runtime.importConfig(text)
                runOnMain { continueEnable() }
            } catch (e: ConfigException) {
                runOnMain { showImportError(e.message ?: "") }
            } catch (e: BridgeException) {
                // Already configured (a race with another import): carry on with what is there.
                runOnMain { continueEnable() }
            } catch (e: Exception) {
                Log.w(TAG, "config import failed: ${e.javaClass.simpleName}")
                runOnMain { showImportError(getString(R.string.save_unavailable)) }
            }
        }
    }

    private fun readBounded(uri: Uri, max: Int): String {
        val stream = contentResolver.openInputStream(uri) ?: throw ConfigException("The file could not be opened.")
        stream.use {
            val buffer = java.io.ByteArrayOutputStream()
            val chunk = ByteArray(8192)
            while (true) {
                val n = it.read(chunk)
                if (n < 0) break
                if (buffer.size() + n > max) throw ConfigException("The file is too large to be a google-services.json.")
                buffer.write(chunk, 0, n)
            }
            return String(buffer.toByteArray(), Charsets.UTF_8)
        }
    }

    private fun showImportError(message: String) {
        AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
            .setTitle(R.string.push_setup_error_title)
            .setMessage(message)
            .setPositiveButton(R.string.push_setup_retry) { _, _ -> askImport() }
            .setNegativeButton(R.string.push_setup_cancel) { _, _ -> finishEnable() }
            .setOnCancelListener { finishEnable() }
            .show()
    }

    private fun continueEnable() {
        runtime.notifier.ensureChannel()
        io.execute {
            val permission = try {
                runtime.permission()
            } catch (_: Exception) {
                "denied"
            }
            runOnMain { requestPermission(permission) { fetchTokenStep() } }
        }
    }

    private fun requestPermission(permission: String, next: () -> Unit) {
        val canAsk = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED &&
            (permission == "default" || shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS))
        when {
            permission == "granted" -> next()
            canAsk -> {
                permissionNext = next
                io.execute { runtime.store.markPermissionAsked() }
                permissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
            }
            else -> AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
                .setTitle(R.string.push_denied_title)
                .setMessage(R.string.push_denied_message)
                .setPositiveButton(R.string.push_denied_open_settings) { _, _ ->
                    try {
                        startActivity(
                            Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                                .putExtra(Settings.EXTRA_APP_PACKAGE, packageName),
                        )
                    } catch (_: ActivityNotFoundException) {
                        // No settings app to open.
                    }
                    next()
                }
                .setNegativeButton(R.string.push_setup_cancel) { _, _ -> next() }
                .setOnCancelListener { next() }
                .show()
        }
    }

    private fun fetchTokenStep() {
        io.execute {
            val failed = try {
                runtime.fetchToken()
                false
            } catch (_: Exception) {
                true
            }
            runOnMain { if (failed) askResetConfig() else finishEnable() }
        }
    }

    private fun askResetConfig() {
        AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
            .setTitle(R.string.push_token_failed_title)
            .setMessage(R.string.push_token_failed_message)
            .setPositiveButton(R.string.push_token_failed_reset) { _, _ ->
                io.execute {
                    try {
                        runtime.resetConfig()
                    } catch (e: Exception) {
                        Log.w(TAG, "reset failed: ${e.javaClass.simpleName}")
                    }
                    runOnMain { askImport() }
                }
            }
            .setNegativeButton(R.string.push_setup_cancel) { _, _ -> finishEnable() }
            .setOnCancelListener { finishEnable() }
            .show()
    }

    // ---- saveFile --------------------------------------------------------------------------------------------

    private fun saveFile(request: BridgeProtocol.SaveRequest, reply: Reply) {
        if (pendingSave != null) {
            reply.fail("busy")
            return
        }
        // Decoding up to 20 MiB stays off the main thread; the picker opens once the bytes are known to be valid.
        io.execute {
            val bytes = try {
                BridgeProtocol.decodeSaveData(request.data)
            } catch (e: BridgeException) {
                reply.fail(e.message ?: "bad_data")
                return@execute
            }
            runOnMain {
                if (pendingSave != null) {
                    reply.fail("busy")
                    return@runOnMain
                }
                pendingSave = PendingSave(reply, bytes)
                try {
                    savePicker.launch(SaveInput(request.name, request.mime))
                } catch (_: ActivityNotFoundException) {
                    pendingSave = null
                    reply.fail(getString(R.string.save_unavailable))
                }
            }
        }
    }

    private fun onSavePicked(uri: Uri?) {
        val save = pendingSave ?: return
        pendingSave = null
        if (uri == null) {
            save.reply.fail("cancelled")
            return
        }
        io.execute {
            try {
                SaveWriter.write(contentResolver, uri, save.bytes)
                save.reply.ok(null)
            } catch (e: Exception) {
                Log.w(TAG, "save failed: ${e.javaClass.simpleName}")
                save.reply.fail("save_failed")
            }
        }
    }

    // ---- notification taps -----------------------------------------------------------------------------------

    private fun handleIntent(intent: Intent?) {
        intent ?: return
        val request = try {
            OpenRequest.validate(
                intent.action,
                intent.getStringExtra(OpenRequest.EXTRA_URL),
                intent.getStringExtra(OpenRequest.EXTRA_DESKTOP),
                intent.getStringExtra(OpenRequest.EXTRA_SECRET),
                runtime.store,
            )
        } catch (_: Exception) {
            // A malformed foreign intent (or unreadable state) opens nothing.
            null
        } ?: return
        deliverOpen(request.url)
    }

    private fun deliverOpen(url: String) {
        acceptedOpens += url
        // Held until the page's `info` handshake: an initial intent must not be dropped before the page listens.
        if (openGate.deliver(url)) sendEvent("open", JSONObject().put("url", url))
    }

    // ---- test hooks ------------------------------------------------------------------------------------------

    internal fun webViewForTesting(): WebView = webView

    internal fun acceptedOpensForTesting(): List<String> = acceptedOpens.toList()

    private companion object {
        const val TAG = "OmpDeck"
        const val BRIDGE_NAME = "OmpDeckAndroid"
        const val CAPTURE_DIR = "captures"
        const val EXTERNAL_MIN_INTERVAL_MS = 400L
    }
}
