# Local Focus

Local Focus is a privacy-first activity tracker and focus tool. The core is a small Rust binary that runs on Windows, Linux, and macOS, maps foreground apps, window titles, browser URLs, files, and projects into a reviewable local timeline, and serves a private dashboard. It includes Pomodoro-style focus sessions, OS-level distraction alerts, app/website blocking, and a local productivity score.

It also ships on phones: a **standalone Android app** that runs the exact same Rust core *on the device*, and an **iPhone companion** that drives a desktop instance over Wi-Fi (Apple's sandbox forbids on-device app monitoring/blocking). See [`mobile/README.md`](mobile/README.md).

No cloud account is used and no data is sent off your devices. The app records local JSONL files and the private dashboard is bound to `127.0.0.1` (only the device-companion endpoints are reachable over the LAN — see [Privacy and Security](#privacy-and-security)).

## Features

- Automatic activity timeline from the active app, window title, and browser URL when available.
- Local mapping for apps, websites, files, and project names visible in titles.
- Focus mode with Pomodoro timer; pause and resume.
- Optional focus targets, such as `Code`, `Pages`, `github.com`, `https://claude.ai/chat`, or a project name.
- Distraction detection and OS-level alerts during focus sessions.
- App and website **blocking** — quits blocked apps, closes blocked browser tabs, with optional password-protected and "High Focus" (block everything outside your focus list) modes. Add blocks from the dashboard.
- **Stop = master switch:** one tap halts all tracking, blocking, alerts, device notifications, and reminders until you resume.
- Daily journal with reminders, and day/week/month/year focus reports.
- Same-WiFi phone, tablet, and TV receiver alerts, plus a phone companion protocol.
- **Standalone Android app** (on-device tracking + app blocking) and **iPhone companion**.
- Cross-platform Rust core with no third-party crates beyond a QR generator.

## Install

Install Rust from <https://www.rust-lang.org/tools/install>, then run the local installer:

```sh
scripts/install.sh
```

This builds a release binary and copies it to `~/.local/bin/local-focus` without using `cargo install --path`.

On macOS, it also installs a local app bundle:

```text
~/Applications/Local Focus.app
```

Build a drag-to-Applications DMG on macOS:

```sh
scripts/package-dmg.sh
```

Output:

```text
target/macos/LocalFocus.dmg
```

Open the DMG, drag `Local Focus.app` to `Applications`, then launch it from Applications. The app opens in its own native Mac window and starts the private local server behind the scenes.

Start from terminal:

```sh
~/.local/bin/local-focus serve
```

Open:

```text
http://127.0.0.1:4799
```

Optional run-at-login helpers are in `scripts/`.

On Windows PowerShell:

```powershell
.\scripts\install.ps1
```

## Platform Notes

macOS uses AppleScript to read the frontmost app, window title, and active browser tab URL for supported browsers such as Safari, Chrome, Brave, Edge, and Arc. The first run may ask for Accessibility or Automation permission.

Linux uses `xdotool`, `xprop`, and `notify-send` when available:

```sh
sudo apt install xdotool x11-utils libnotify-bin
```

Windows uses PowerShell and Win32 APIs for active-window metadata. Notification support may vary by system policy.

## Mobile (Android and iPhone)

Local Focus runs on phones too. Full details and build/install steps are in [`mobile/README.md`](mobile/README.md) and [`mobile/local_focus_mobile/README.md`](mobile/local_focus_mobile/README.md).

- **Android — standalone.** A Flutter app embeds the same Rust core (`liblocalfocus.so`, cross-compiled per ABI by `mobile/build-android-server.sh`) and runs it on the phone, serving the identical dashboard in a WebView. A foreground service tracks the phone's app usage (Usage Access), and an Accessibility service blocks apps you add to your block list. A signed release APK builds with `flutter build apk --release`.
- **iPhone — companion.** iOS forbids apps from reading other apps' foreground state or blocking them, so the iPhone connects over Wi-Fi to a Mac/PC (or Android phone) running Local Focus: drive focus sessions, view reports, receive alerts, and log manual activity. It needs the iOS Local Network permission to reach the desktop link.

Other phones, tablets, and TVs can also connect as receiver-only devices via the dashboard's QR/connect link.

## Run at Login

After installing locally, run the helper for your OS:

```sh
scripts/autostart-macos.sh
scripts/autostart-linux.sh
```

On Windows PowerShell:

```powershell
.\scripts\autostart-windows.ps1
```

## Commands

```sh
local-focus serve
local-focus track
local-focus focus "Write proposal" 25 "Pages, https://claude.ai/chat"
local-focus report
local-focus data-dir
```

When focus targets are set, Local Focus checks the active app, window title, and detected source. Separate multiple allowed apps, sites, or projects with commas. URL targets also match by domain, so `https://claude.ai/chat` can match activity reported as `claude.ai`. During targeted focus, only matching activity counts as Productive time; every app or site outside your focus targets is tracked as Distracted. If the current activity no longer matches any target during focus mode, it sends an OS-level alert.

## Data

Default local data locations:

- macOS: `~/Library/Application Support/local-focus`
- Linux: `~/.local/share/local-focus`
- Windows: `%USERPROFILE%\AppData\Local\local-focus`

Override with:

```sh
LOCAL_FOCUS_DATA=/path/to/private/folder local-focus serve
```

Files:

- `activity.jsonl`: activity samples (pruned to a rolling 30-day window).
- `events.jsonl`: focus, distraction, and block events.
- `device_notifications.jsonl`: alerts sent to connected devices (also pruned to 30 days).
- `config.txt`: productive, distracting, and blocked keywords, plus connected devices.
- `focus.json`: active focus session state.
- `focus_sessions.jsonl` / `report_history.jsonl` / `daily_focus_reports.jsonl`: session and report history.
- `journal_entries.jsonl`, `journal_settings.json`, `journal_task_reminders.jsonl`: daily journal and reminders.

## Configure Blocking and Scoring

Edit `config.txt` in the data directory:

```text
productive=code,terminal,editor,docs,figma,notion,calendar,github,jira,linear
distracting=youtube,netflix,reddit,instagram,tiktok,x.com,twitter,facebook,game,steam
blocked=netflix,steam
```

Use the dashboard's block field to add an app, site, or keyword to `blocked=`. Blocking is enforced, not just warned:

- A blocked **website** closes the active browser tab; a blocked **app** is quit (on macOS via AppleScript, Windows via PowerShell, Linux via `wmctrl`/`pkill`).
- **Password mode** prompts for a password before allowing a blocked target.
- **High Focus mode** force-quits anything outside your current focus targets.
- On the standalone Android app, blocking is done by the Accessibility service, which returns you to the home screen.

Block rules apply whenever the app is running, independent of focus sessions — except when the app is **Stopped** (see below).

## Stop and Resume

The dashboard's **Stop** button is a master switch. Stopping ends any focus session and halts everything — tracking, blocking, alerts, device notifications, and journal reminders — until you resume. Resume via the **Resume** banner, by starting a new focus session, or by relaunching. The current state is exposed as `stopped` in `/api/state`.

## Privacy and Security

- **No data leaves your devices.** Everything is local JSONL/text in the data directory.
- **The dashboard and control API are loopback-only.** Sensitive endpoints (full dashboard, raw activity timeline, journal, block management, report history, master stop/resume) are served only to `127.0.0.1`. Requests from other machines get `403`.
- **Only the companion surface is reachable over the LAN:** device-receiver pages, QR images, the mobile/native APIs, and the read/focus endpoints the phone companion needs (`/api/state`, `/api/report`, `/api/focus-report`, `/api/focus-sessions`, `/api/focus/*`).
- **CSRF protection:** state-changing endpoints reject cross-site browser requests (checked via `Sec-Fetch-Site`/`Origin`); native companions are unaffected.
- **Hardening:** inbound sockets have read/write timeouts and request size caps; the high-frequency `activity.jsonl` and `device_notifications.jsonl` logs are pruned to a rolling 30-day window.

> Note: the LAN companion endpoints are not yet authenticated, so trust the networks you enable them on. Token-based auth is a planned improvement.

## Build Release Binaries

```sh
cargo build --release
```

The binary will be in:

```text
target/release/local-focus
```

On Windows, the executable is:

```text
target\release\local-focus.exe
```

## Mac App Store Packaging

Local Focus includes a Mac App Store-oriented `.app` packaging scaffold in `macos/`.

Build an unsigned app bundle:

```sh
scripts/package-mas.sh
```

Output:

```text
target/macos/Local Focus.app
```

For a Mac App Store upload, you need an Apple Developer Program account, an App Store Connect app record, a matching bundle id, and Mac App Store signing certificates. Example:

```sh
LOCAL_FOCUS_BUNDLE_ID=com.yourcompany.localfocus \
MAS_APP_SIGN_IDENTITY="3rd Party Mac Developer Application: Your Name (TEAMID)" \
MAS_INSTALLER_SIGN_IDENTITY="3rd Party Mac Developer Installer: Your Name (TEAMID)" \
scripts/package-mas.sh
```

More detail: `macos/README-App-Store.md`.
