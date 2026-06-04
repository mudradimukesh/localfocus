# Local Focus Mobile Companion

Local Focus can run the same focus logic with phones and tablets by using the desktop app as the private local hub. The phone app tracks its own activity with native OS permissions, posts samples to the laptop, and polls for focus alerts from the laptop over the same Wi-Fi network.

No cloud service is required for same-Wi-Fi operation.

The installable Flutter phone app lives in:

```text
mobile/local_focus_mobile
```

Android debug APK output:

```text
mobile/local_focus_mobile/build/app/outputs/flutter-apk/app-debug.apk
```

## Local Protocol

Register a phone or tablet:

```http
POST http://<laptop-ip>:4799/api/mobile/register
Content-Type: application/json

{
  "name": "Mukesh iPhone",
  "kind": "phone",
  "endpoint": "mobile:mukesh-iphone"
}
```

Send phone activity:

```http
POST http://<laptop-ip>:4799/api/mobile/activity
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
GET http://<laptop-ip>:4799/api/device/events?since=<unix-seconds>&device=mobile%3Amukesh-iphone
```

Supported activity categories are `productive`, `distracting`, and `idle`. If the phone omits `category`, the desktop classifies the sample using the same focus apps, websites, block rules, and distraction rules as the laptop.

## Android Tracker

An Android companion can implement full phone-side tracking with:

- `UsageStatsManager` plus Usage Access permission for foreground app activity.
- Notification permission for receiver alerts.
- Accessibility Service or a local VPN/DNS approach for browser URL-level tracking, because Android does not expose arbitrary browser tab URLs through `UsageStatsManager`.
- Periodic local POSTs to `/api/mobile/activity`.
- Polling `/api/device/events` for receiver notifications.

## iPhone and iPad Tracker

iOS and iPadOS need a native app because Safari pages and other app activity are not visible to a normal web page.

Practical options:

- Use Apple Screen Time APIs, such as Family Controls and Device Activity, for app and category monitoring.
- Use local notifications for receiver alerts.
- Use Local Network privacy permission so the phone can talk to the laptop at `http://<laptop-ip>:4799`.
- For precise Safari URL tracking, iOS is intentionally restrictive; this usually needs a Safari extension, managed-device policy, or a local DNS/VPN-style approach.

## Receiver-Only Mode

If the phone app only needs to receive alerts, it can register once with `/api/mobile/register`, poll `/api/device/events`, and show native local notifications. It does not need to send `/api/mobile/activity` samples unless phone activity should appear in reports.
