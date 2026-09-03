#!/usr/bin/env sh
# Build "Local Focus.app" for distribution outside the Mac App Store:
# Developer ID signature + hardened runtime + notarization + stapling.
#
# Usage:  sh scripts/package-macos.sh
#
# It picks the strongest signing it can do with what is on this machine and
# says plainly what is missing, rather than silently shipping something the
# next Mac will refuse to open:
#
#   1. A "Developer ID Application" certificate  -> signed, hardened, and (if
#      notary credentials exist) notarized + stapled. This is the only
#      combination that opens cleanly on someone else's Mac.
#   2. Any other identity (e.g. "Apple Development") -> signed and hardened,
#      but Gatekeeper will still warn off this machine. Fine for local use.
#   3. No identity at all -> ad-hoc, same as before. Local use only.
#
# One-time setup for the real thing (needs a paid Apple Developer account, so
# it cannot be scripted for you):
#   1. Enrol at https://developer.apple.com/programs/ (99 USD/year).
#   2. In Xcode: Settings > Accounts > Manage Certificates > + >
#      "Developer ID Application". It lands in your login keychain.
#   3. Store notary credentials once:
#        xcrun notarytool store-credentials "notary" \
#          --apple-id "you@example.com" \
#          --team-id "YOURTEAMID" \
#          --password "app-specific-password"
#      (App-specific password: https://account.apple.com > Sign-In and Security.)
# Then re-run this script and it will notarize and staple automatically.
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$ROOT_DIR/scripts/lib-signing.sh"
APP_NAME="Local Focus"
APP_DIR="$ROOT_DIR/target/macos/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
INFO_PLIST="$CONTENTS_DIR/Info.plist"
ENTITLEMENTS="$ROOT_DIR/macos/LocalFocusHardened.entitlements"
BUNDLE_ID="${LOCAL_FOCUS_BUNDLE_ID:-com.localfocus.app}"
ICONSET_DIR="$ROOT_DIR/target/macos/AppIcon.iconset"

cd "$ROOT_DIR"
cargo build --release

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"
cp "$ROOT_DIR/target/release/local-focus" "$MACOS_DIR/local-focus-bin"
cp "$ROOT_DIR/macos/Info.plist" "$INFO_PLIST"
swiftc \
  -parse-as-library \
  "$ROOT_DIR/macos/LocalFocusHost.swift" \
  -framework Cocoa \
  -framework WebKit \
  -framework UserNotifications \
  -o "$MACOS_DIR/local-focus"

rm -rf "$ICONSET_DIR"
python3 "$ROOT_DIR/macos/make-icon.py" "$ICONSET_DIR"
iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/AppIcon.icns"

/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID" "$INFO_PLIST"

lf_pick_identity
lf_sign_app "$APP_DIR" "$ENTITLEMENTS" "$LF_IDENTITY"

if [ "$LF_IS_DEVELOPER_ID" -eq 0 ] || ! lf_notary_available; then
  lf_explain_missing_notarization
  echo ""
  echo "Built: $APP_DIR"
  exit 0
fi

# Notarize the app itself by submitting a zip of it; distribution to other
# people should go through scripts/package-dmg.sh, which notarizes the DMG.
ZIP_PATH="$ROOT_DIR/target/macos/LocalFocus-notarize.zip"
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_DIR" "$ZIP_PATH"
lf_notarize_and_staple "$ZIP_PATH" "$APP_DIR"
rm -f "$ZIP_PATH"

echo ""
echo "Signed, hardened, notarized, and stapled: $APP_DIR"
echo "This opens cleanly on any Mac."
