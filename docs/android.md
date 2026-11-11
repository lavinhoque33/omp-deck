# Android companion

The APK wraps the existing React phone UI in an Android WebView. It bundles the UI rather than loading the relay's website. Sessions still travel through your self-hosted, end-to-end encrypted relay; the existing PWA remains available independently.

## Requirements

- Android 8.0/API 26 or later; an up-to-date Android System WebView.
- Google Play Services for native FCM notifications. Session access itself does not depend on Firebase.
- A configured HTTPS/WSS relay and an OmpDeck gateway with Android push support.

Install the APK, open it, and paste the desktop's **Mobile access → Pair a device** link. Compare fingerprints before approving. Browser pairings are separate and are not imported or removed. Each desktop retains its own pairing and notification preferences.

The app uses the system file picker/camera for attachments and the system save dialog for downloads. It does not request broad storage or direct camera/microphone access. External links open in their system handler, outside the privileged WebView.

Desktop updates must include `android_push`. Replacing the gateway with a build lacking it disables native delivery and can discard the saved native registration. Pairing remains intact; the APK restores its registration on reconnect to a compatible gateway.

## Native notifications: your Firebase project

FCM is optional and uses a Firebase project you control. There is no shared OmpDeck push service or compiled-in Firebase configuration.

1. In the [Firebase console](https://console.firebase.google.com/), create a project and register an Android app with package **`io.github.lavinhoque33.ompdeck`**. Analytics is not required.
2. Download that Android app's **`google-services.json`**. Keep it on the desktop, not on each phone.
3. For the desktop sender, enable **Firebase Cloud Messaging API** (`fcm.googleapis.com`) in the same Google Cloud project. Create a dedicated service account with **Firebase Cloud Messaging API Admin** (`roles/firebasecloudmessaging.admin`) and download its JSON private key.
4. Store that service-account JSON outside the source tree, readable only by the desktop user. Either put it at `fcm-service-account.json` inside the gateway's private state directory, or set **`OMP_DECK_FCM_CREDENTIALS`** to its absolute path in the gateway's launch environment. The private directory is reported in the gateway startup log; on Linux it is beneath `$XDG_CONFIG_HOME/omp-deck/gateway/gw-…/` (normally `~/.config/omp-deck/gateway/gw-…/`).
5. Put the public **`google-services.json`** in that same gateway directory, or set **`OMP_DECK_FCM_ANDROID_CONFIG`** to its path. The gateway validates its Android package and matching Firebase project, then exposes only the selected public Android client over the encrypted pairing connection. It never forwards the service-account file or arbitrary extra fields.
6. Restart the gateway through its normal maintenance/restart flow so it loads both files. The log must report **`Android push: enabled for Firebase project …`**. Reopen the phone's notification settings if it was connected before that restart.
7. On each newly paired phone, open **Settings → Notifications**, turn on **Push notifications**, and allow Android's notification permission. The public configuration comes automatically from the paired desktop; there is no phone-side JSON picker or manual file copy.

This automatic setup needs an APK and gateway containing the provisioning change; 0.28.2 and older still use the original phone-side import. Already-configured phones retain their existing configuration and keys.

Never put the service-account JSON in the APK, on the phone, in the repository or in a public web directory. On Unix, use `chmod 600` on it. The gateway retains only the credential-file path in its supervised startup environment, not the credential contents. Invalid/missing credentials disable the `android_push` capability rather than falling back to browser push inside the wrapper.

Native registration replaces only that device's browser/native push transport; event preferences survive registration refreshes. Other paired devices are unchanged. Disabling notifications unregisters that desktop and removes its local native key. Revoking a device on the desktop also removes its registration. Removing the sender's service-account file and restarting the gateway disables Android sends; revoke the key in Google Cloud to invalidate the credential itself.

The desktop encrypts notification content with a separate AES-256-GCM key for each desktop/phone registration. Google receives ciphertext, an opaque desktop id and delivery metadata, not the title, body or session destination. Android validates authentication, expiry and a durable replay cache before showing a private notification. Taps return to the bundled app and select the corresponding desktop/session. The currently visible, connected desktop is suppressed; other desktops are not. Event toggles remain governed by the gateway's canonical notification policy.

Android app settings can disable notifications or impose background/battery restrictions. A Google send acceptance is not proof that a handset received or displayed it. A force-stopped app must be opened again before Android delivers messages to it.

## Build

Tested with JDK 25, Android SDK platform 35/build tools 36, Bun 1.4 and the checked-in Gradle wrapper. `ANDROID_HOME` must point at the SDK. No Firebase file is required to build.

```sh
cd pwa
bun install --frozen-lockfile
cd ../android
export ANDROID_HOME="$HOME/Android/Sdk"
./gradlew --no-daemon :app:testDebugUnitTest :app:assembleDebug :app:assembleDebugAndroidTest
# With one authorized Android device connected:
./gradlew --no-daemon :app:connectedDebugAndroidTest
```

Gradle invokes `bun run build.ts --android` and bundles `pwa/dist-android` into `assets/www`. The page runs at `https://appassets.androidplatform.net/index.html`; there is no service worker or relay-hosted UI update in the APK. Use `-Pompdeck.bun=/absolute/path/to/bun` if Bun is not on Gradle's PATH, or `-Pompdeck.pwaDist=/absolute/path/to/built/assets` to bundle an existing Android web build.

Debug output: `android/app/build/outputs/apk/debug/app-debug.apk`. The device test APK is under `outputs/apk/androidTest/debug/`. The Android GitHub Actions workflow builds the debug/test APKs and JVM tests; it does not provision Firebase or prove Google-to-handset delivery.

## Signed releases and upgrades

Generate and securely back up a signing keystore outside the repository. Preserve its key for every upgrade: an APK signed with a different key cannot replace the installed app without uninstalling it, which loses app pairings and native keys. Debug and release builds share a package but have different signing keys.

The release build reads these environment variables:

- `OMPDECK_ANDROID_KEYSTORE`: absolute keystore path.
- `OMPDECK_ANDROID_KEYSTORE_PASSWORD`: store password.
- `OMPDECK_ANDROID_KEY_ALIAS`: signing-key alias (for example, `ompdeck`).
- `OMPDECK_ANDROID_KEY_PASSWORD`: optional distinct key password; otherwise the store password.

Load passwords without printing them or putting them in shell history, then run:

```sh
cd android
./gradlew --no-daemon :app:assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

The workspace version supplies `versionName` and a monotonic `major * 1000000 + minor * 1000 + patch` version code. Release WebView debugging is disabled; the bridge remains main-frame/origin-scoped, cleartext is blocked, and Android backups exclude app data. Update the APK to update the bundled UI; updating the relay alone does not update it.

Tagged GitHub releases build and publish `OmpDeck-<version>.apk`, its `.sha256` and a matching gateway. The Release
workflow requires encrypted repository Actions secrets `OMPDECK_ANDROID_KEYSTORE_BASE64`,
`OMPDECK_ANDROID_KEYSTORE_PASSWORD` and `OMPDECK_ANDROID_KEY_ALIAS`; `OMPDECK_ANDROID_KEY_PASSWORD` is optional
when distinct. The keystore is decoded only into the runner's temporary directory and removed after building.
The APK's signing-certificate fingerprint is pinned in the workflow so upgrades keep the same identity. A missing
signing secret, failed APK build or wrong certificate blocks publication. Firebase service-account credentials
must **never** be uploaded to CI; they remain on the desktop.

## In-app updates

The APK checks GitHub Releases on launch and every six hours while the app is open. Settings has an Android app
update card with **Check now** and an automatic-checks switch; an available-version banner offers the update.
The phone downloads an APK only when you choose to update. It verifies the release checksum, package, version
and installed signing identity before opening Android's installer. Incorrect, corrupt or older APKs are refused.

Android may first ask you to allow OmpDeck to **install unknown apps** (allow installs from this source), then
asks you to confirm the installation. There is no silent installation. Cancelling leaves the current app usable;
you can retry. Successful upgrades preserve app data, desktop pairings and native notification keys. The browser
app keeps its existing service-worker update flow; updating the relay alone still cannot update the APK.

## Verification

The checked-in JVM/device tests cover bridge validation/origin restrictions, file writes, Keystore storage, encryption/authentication/expiry/replay, per-desktop suppression and notification routing. Device tests inject messages into the real FCM service; that does **not** exercise Google's transport.

The ignored `deck-service` gate `android_push_live` sends through the production FCM sender. Its registration file must contain a real native `{token,key,desktop}` and be mode 0600. Supply `OMP_DECK_FCM_CREDENTIALS`, `OMPDECK_ANDROID_PUSH_REGISTRATION`, `OMPDECK_ANDROID_PUSH_ORIGIN` and a nonexistent `OMPDECK_ANDROID_PUSH_CONFIRMED` marker path. Optionally set `OMPDECK_ANDROID_PUSH_SESSION` to an existing session id for a meaningful, read-only tap destination, then run:

```sh
cargo test -p deck-service --test android_push_live -- --ignored --nocapture
```

Background the app. Only create the confirmation marker after observing the notification on the phone; timeout fails. Confirm a real notification tap, desktop selection and preference/visibility suppression separately. Never publish the registration token or AES key.
