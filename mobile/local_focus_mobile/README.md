# Local Focus Mobile

Installable iOS and Android companion app for Local Focus.

The phone app can:

- Connect to the Local Focus desktop app over the same Wi-Fi network.
- Start, pause, resume, and stop focus sessions.
- Manage focus task names, focus apps, focus websites, warning delay, and optional laptop move-to app.
- Show day, week, month, and year focus reports.
- Receive Local Focus alerts on the phone.
- Send phone activity samples into the same local report.
- On Android, run a foreground tracking service that samples the current foreground app every five seconds after Usage Access is granted.

Data still stays local. The phone talks directly to the laptop URL, such as:

```text
http://192.168.4.22:4799
```

## Android Install

Build the debug APK:

```sh
flutter build apk --debug
```

APK output:

```text
build/app/outputs/flutter-apk/app-debug.apk
```

Install with Android Debug Bridge:

```sh
adb install -r build/app/outputs/flutter-apk/app-debug.apk
```

On the phone:

1. Open Local Focus.
2. Set the laptop URL, for example `http://192.168.4.22:4799`.
3. Tap `Save and Connect`.
4. Open `Tracking`.
5. Tap `Open` next to Usage Access and allow Local Focus.
6. Turn on `Track this phone`.

Android will show an ongoing Local Focus tracking notification while the foreground service is running.

## iPhone Install

Open the iOS project in Xcode:

```sh
open ios/Runner.xcworkspace
```

Then:

1. Select a development team.
2. Choose your iPhone as the run destination.
3. Build and run.
4. In the app, set the laptop URL and tap `Save and Connect`.

iPhone receiver alerts and focus/report management work now. Full iPhone app and Safari activity tracking requires Apple Screen Time entitlements through Family Controls and Device Activity.

## Phone Browser URL Tracking

Android Usage Access reports foreground apps, not the exact browser URL. Exact Chrome/Safari URL capture requires a separate Accessibility, browser-extension, managed-device, DNS, or VPN-style integration. Until that is added, the app records browser apps as phone activity and lets the user send a manual website sample from the Tracking tab.
