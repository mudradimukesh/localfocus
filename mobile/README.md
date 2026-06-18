# Local Focus Mobile

Local Focus runs on phones two ways:

- **Android — standalone.** The full Local Focus core runs *on the phone*; no desktop needed. It embeds the same Rust server (`liblocalfocus.so`, cross-compiled per ABI) and serves the identical dashboard locally, feeds the phone's app usage in via Usage Access, and blocks apps via an Accessibility service. The installable app and full build/install steps are in [`local_focus_mobile/README.md`](local_focus_mobile/README.md).
- **iPhone — companion.** iOS forbids apps from reading other apps' foreground state or blocking them (the sandbox), so the iPhone connects to a desktop (or Android) Local Focus instance over the same Wi-Fi and acts as a remote: focus control, reports, alerts, and manual activity. It needs the iOS Local Network permission to reach the desktop link.

No cloud service and no device discovery — a companion connects only to the exact Local Focus URL you enter or scan.

The installable Flutter phone app lives in:

```text
mobile/local_focus_mobile
```

Cross-compile the embedded Android server (only needed after changing the Rust core; prebuilt binaries are committed):

```sh
mobile/build-android-server.sh
```

Android APK outputs:

```text
mobile/local_focus_mobile/build/app/outputs/flutter-apk/app-debug.apk
mobile/local_focus_mobile/build/app/outputs/flutter-apk/app-release.apk   # signed
```

## Companion protocol

A companion (or any device on the same Wi-Fi) talks to a running Local Focus instance. Only the endpoints below are reachable over the LAN; the rest of the API (full dashboard, raw timeline, journal, block management, master stop) is loopback-only.

Register a phone or tablet:

```http
POST http://<host-ip>:4799/api/mobile/register
Content-Type: application/json

{
  "name": "Mukesh iPhone",
  "kind": "phone",
  "endpoint": "mobile:mukesh-iphone"
}
```

Send phone activity:

```http
POST http://<host-ip>:4799/api/mobile/activity
Content-Type: application/json

{
  "device": "Mukesh iPhone",
  "app": "Safari",
  "title": "Claude",
  "source": "https://claude.ai/chat",
  "category": "productive",
  "timestamp": 1780342619
}
```

Poll focus alerts for the phone:

```http
GET http://<host-ip>:4799/api/device/events?since=<unix-seconds>&device=mobile%3Amukesh-iphone
```

Read state and reports, and control focus sessions:

```http
GET  http://<host-ip>:4799/api/state
GET  http://<host-ip>:4799/api/report
GET  http://<host-ip>:4799/api/focus-report
GET  http://<host-ip>:4799/api/focus-sessions
GET  http://<host-ip>:4799/api/focus/start?task=...&minutes=25&target=...
GET  http://<host-ip>:4799/api/focus/pause
GET  http://<host-ip>:4799/api/focus/stop
```

Supported activity categories are `productive`, `distracting`, and `idle`. If the phone omits `category`, the host classifies the sample using the same focus apps, websites, block rules, and distraction rules as the desktop.

> Security: these LAN endpoints are not yet authenticated, so enable them only on trusted networks. Sensitive data (raw timeline, journal, block management) stays loopback-only regardless. Cross-site browser requests to control endpoints are rejected (CSRF); native companions are unaffected.

## Android tracker (standalone)

The Android app implements full phone-side tracking and blocking:

- `UsageStatsManager` + Usage Access permission for foreground app activity.
- An Accessibility service that blocks apps on your block list by returning to the home screen.
- A foreground service that posts samples to the on-device server's `/api/mobile/activity`.
- Notification permission for alerts.
- Browser URL-level blocking is not implemented yet: Android does not expose arbitrary browser tab URLs through `UsageStatsManager`, so a specific website inside a browser is not blocked (it would need reading the address bar via the Accessibility tree). App/keyword blocks work fully.

## iPhone tracker (companion)

iOS provides no API for an app to see which other app is foreground or to quit other apps, so the iPhone is companion-only:

- Use the **Local Network** privacy permission so the phone can reach the desktop link at `http://<host-ip>:4799`. The app does not scan for nearby devices.
- Local notifications for receiver alerts.
- Focus control, reports, alerts, and manual activity logging work today.
- On-device app/category monitoring or blocking would require the Apple **Screen Time / Family Controls** + **Device Activity** entitlement — a restricted, opaque, picker-based model, not the free-form tracking the desktop and Android do. Precise Safari URL tracking is likewise restricted.

## Receiver-only mode

If a phone only needs to receive alerts, it can register once with `/api/mobile/register`, poll `/api/device/events`, and show native local notifications. It does not need to send `/api/mobile/activity` samples unless its activity should appear in reports.
