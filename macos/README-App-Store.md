# Mac App Store packaging

This folder contains a Mac App Store-oriented packaging scaffold for Local Focus.

Important: the Mac App Store build cannot be fully completed from this repo alone. Final submission requires:

- Apple Developer Program membership.
- A Mac App Store app record in App Store Connect.
- A bundle identifier that matches the App Store Connect record.
- Mac App Store signing certificates/profiles installed in Keychain.
- App Sandbox enabled.
- App Review justification for Apple Events / Automation usage.

## Why this is special

Local Focus tracks activity locally by reading the foreground app, window title, and supported browser URLs. On macOS that uses AppleScript/Apple Events. Mac App Store apps must run in App Sandbox, so the app bundle includes sandbox and automation entitlements.

Apple may ask why the Apple Events temporary exceptions are needed. A concise review note:

> Local Focus is a privacy-first local activity tracker. It uses Apple Events only to read the active browser tab URL and foreground app context so the user can review their own local focus timeline. All data is stored locally on the user's Mac and no data is transmitted to external servers.

## Build an unsigned `.app`

From the repo root:

```sh
scripts/package-mas.sh
```

Output:

```text
target/macos/Local Focus.app
```

This app bundle launches the same local dashboard server as:

```sh
local-focus serve
```

The user opens:

```text
http://127.0.0.1:4799
```

## Build a signed package for App Store upload

Set your real bundle id and signing identities:

```sh
LOCAL_FOCUS_BUNDLE_ID=com.yourcompany.localfocus \
MAS_APP_SIGN_IDENTITY="3rd Party Mac Developer Application: Your Name (TEAMID)" \
MAS_INSTALLER_SIGN_IDENTITY="3rd Party Mac Developer Installer: Your Name (TEAMID)" \
scripts/package-mas.sh
```

Output:

```text
target/macos/LocalFocus.pkg
```

Upload the package with Apple Transporter or from Xcode Organizer, depending on your App Store Connect workflow.

## App Store review checklist

- Update `CFBundleIdentifier` or pass `LOCAL_FOCUS_BUNDLE_ID`.
- Update version/build numbers in `macos/Info.plist`.
- Add App Privacy answers in App Store Connect. The intended answer is that activity data is not collected by the developer because it stays local.
- Include screenshots showing the local dashboard and privacy-first copy.
- Explain Automation permission in Review Notes.
- Test on a clean macOS user account, including Accessibility/Automation prompts.
- Verify sandbox behavior with Console logs before submitting.
