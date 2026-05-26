# Local Focus

Local Focus is a privacy-first Rust activity tracker for Windows, Linux, and macOS. It maps foreground apps, website-like window titles, files, projects, and laptop activity into a reviewable local timeline. It also includes Pomodoro-style focus sessions, distraction notifications, blocked keyword rules, and a local productivity score.

No cloud account is used. No data is sent off the machine. The app records local JSONL files and serves a private dashboard on `127.0.0.1`.

## Features

- Automatic activity timeline from the active app and window title.
- Local mapping for apps, websites, files, and project names visible in titles.
- Focus mode with Pomodoro timer.
- Optional focus targets, such as `Code`, `Pages`, `github.com`, `https://claude.ai/chat`, or a project name.
- Distraction detection during focus sessions.
- User-editable productive, distracting, and blocked keyword rules, including adding blocks from the dashboard.
- Productivity report for the last 24 hours.
- Cross-platform Rust binary with no third-party crates.

## Install

Install Rust from <https://www.rust-lang.org/tools/install>, then run:

```sh
cargo install --path .
```

Start the app:

```sh
local-focus serve
```

Open:

```text
http://127.0.0.1:4799
```

Optional run-at-login helpers are in `scripts/`.

## Platform Notes

macOS uses AppleScript to read the frontmost app and window title. The first run may ask for Accessibility or Automation permission.

Linux uses `xdotool`, `xprop`, and `notify-send` when available:

```sh
sudo apt install xdotool x11-utils libnotify-bin
```

Windows uses PowerShell and Win32 APIs for active-window metadata. Notification support may vary by system policy.

## Run at Login

After `cargo install --path .`, run the helper for your OS:

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

When focus targets are set, Local Focus checks the active app, window title, and detected source. Separate multiple allowed apps, sites, or projects with commas. URL targets also match by domain, so `https://claude.ai/chat` can match activity reported as `claude.ai`. During targeted focus, only matching activity counts as Productive time; productive-keyword activity outside your focus targets is treated as Neutral. If the current activity no longer matches any target during focus mode, it sends a local notification.

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

- `activity.jsonl`: activity samples.
- `events.jsonl`: focus and distraction events.
- `config.txt`: productive, distracting, and blocked keywords.
- `focus.json`: active focus session state.

## Configure Blocking and Scoring

Edit `config.txt` in the data directory:

```text
productive=code,terminal,editor,docs,figma,notion,calendar,github,jira,linear
distracting=youtube,netflix,reddit,instagram,tiktok,x.com,twitter,facebook,game,steam
blocked=netflix,steam
```

The current app does notification-based blocking. It warns during focus mode when a distracting or blocked activity is detected. Use the dashboard's block field to add an app, site, or keyword to `blocked=`. Hard network or app blocking can be added later through OS-specific integrations, but this first version stays conservative and transparent.

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
