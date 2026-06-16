# Local Focus Mobile

Local Focus for phones. The two platforms work differently because of what each OS allows:

- **Android — standalone.** Runs the exact same Local Focus core as the Mac *on the phone*. The Rust server (`liblocalfocus.so`) is cross-compiled per ABI and exec'd on-device, serving the identical dashboard in a WebView. A foreground service feeds the phone's app usage in, and an Accessibility service enforces the block list. No desktop needed.
- **iPhone — companion.** iOS forbids apps from seeing which other app is foreground or blocking apps (the sandbox), so the iPhone can't track/block on its own. It connects over Wi‑Fi to a Mac (or Android phone) running Local Focus to drive focus sessions, view reports, and receive alerts.

All data stays on your devices.

---

## Android (standalone)

On the phone the app can:

- Show the full Local Focus dashboard, focus sessions, reports, and journal — served by the on‑device server.
- Track the phone's foreground app every 5 seconds (after Usage Access is granted).
- Block distracting apps you add to your block list (after the Accessibility service is enabled) by bouncing them to the home screen.

### Install the release APK

A signed release APK is produced at:

```text
build/app/outputs/flutter-apk/app-release.apk
```

Install it on a connected device (USB debugging on), or copy it to the phone and tap it:

```sh
adb install -r build/app/outputs/flutter-apk/app-release.apk
```

> Upgrading from a debug build? They're signed with different keys, so uninstall first:
> `adb uninstall com.localfocus.local_focus_mobile`

### On‑device setup

1. Open **Local Focus**.
2. Grant **Usage access** when asked (required for activity tracking).
3. Enable **Accessibility → Local Focus blocking** (optional — needed only for app blocking).
4. Allow notifications, and "Allow background" (ignore battery optimization) so tracking/blocking keep running.

A persistent notification shows while the foreground tracking service runs.

### Build it yourself

Prerequisites: Flutter, the Android SDK + NDK, and the Rust Android targets:

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
```

1. **Build the embedded server** (the Mac core, cross-compiled into `jniLibs/`). Prebuilt `.so`s are committed, so this is only needed after changing the Rust core:

   ```sh
   ../build-android-server.sh
   ```

2. **Debug APK** (no signing needed):

   ```sh
   flutter build apk --debug
   # -> build/app/outputs/flutter-apk/app-debug.apk
   ```

3. **Release APK** (signed). Create a keystore once and an `android/key.properties` (both are git‑ignored — never commit them):

   ```sh
   keytool -genkeypair -v -keystore android/app/localfocus-release.jks \
     -alias localfocus -keyalg RSA -keysize 2048 -validity 10000
   ```

   `android/key.properties`:

   ```properties
   storePassword=YOUR_STORE_PASSWORD
   keyPassword=YOUR_KEY_PASSWORD
   keyAlias=localfocus
   storeFile=localfocus-release.jks
   ```

   Then:

   ```sh
   flutter build apk --release
   # -> build/app/outputs/flutter-apk/app-release.apk (signed)
   ```

   If `key.properties` is absent the release build falls back to the debug key so it still builds. Minimum supported Android is API 24; the APK bundles `arm64-v8a`, `armeabi-v7a`, `x86_64`, and `x86`.

### Website blocking caveat

App/keyword blocks work fully (e.g. `youtube`, `instagram`). Blocking a specific **website inside a browser** is not enforced yet on Android — it needs reading the address bar via the Accessibility tree. Website rules still appear in the dashboard and apply on the desktop.

---

## iPhone (companion)

Connects to a Mac (or Android phone) running Local Focus over the same Wi‑Fi.

### Build & install

```sh
flutter build ios --release   # or: open ios/Runner.xcworkspace and Run from Xcode
```

Then install on your iPhone (Xcode "Run", or `flutter install -d <device-id>`). On the device, **Settings → General → VPN & Device Management** → trust your development profile.

### Connect

1. Open **Local Focus Mobile**.
2. Enter the Local Focus link in the **QR link** field, e.g. `http://192.168.4.22:4799`, and tap **Save and Connect**.
3. When iOS asks, **Allow** local‑network access (or enable it later in **Settings → Privacy & Security → Local Network → Local Focus Mobile**). Without it the app cannot reach the Mac.

Focus control, reports, alerts, and manual activity logging work. Automatic app/website tracking and blocking are **not possible on iPhone** — Apple provides no API for it; that capability would require the Screen Time / Family Controls entitlement and a different, restricted model.

---

## Phone browser URL tracking

Android Usage Access reports foreground apps, not the exact browser URL. Capturing exact Chrome/Safari URLs would require an Accessibility, browser-extension, managed-device, DNS, or VPN integration. Until then, browsers are recorded as app activity and you can send a manual website sample from the Tracking tab.
