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
APP_NAME="Local Focus"
APP_DIR="$ROOT_DIR/target/macos/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
INFO_PLIST="$CONTENTS_DIR/Info.plist"
ENTITLEMENTS="$ROOT_DIR/macos/LocalFocusHardened.entitlements"
BUNDLE_ID="${LOCAL_FOCUS_BUNDLE_ID:-com.localfocus.app}"
ICONSET_DIR="$ROOT_DIR/target/macos/AppIcon.iconset"
NOTARY_PROFILE="${NOTARY_PROFILE:-notary}"

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

# Pick the best identity available unless one was named explicitly.
IDENTITY="${DEVELOPER_ID_IDENTITY:-}"
IS_DEVELOPER_ID=0
if [ -n "$IDENTITY" ]; then
  IS_DEVELOPER_ID=1
else
  IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
    | grep "Developer ID Application" | head -1 | sed 's/.*"\(.*\)".*/\1/')
  if [ -n "$IDENTITY" ]; then
    IS_DEVELOPER_ID=1
  else
    IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
      | head -1 | sed 's/.*"\(.*\)".*/\1/')
  fi
fi

# The nested server binary must be signed before the bundle that contains it.
if [ -n "$IDENTITY" ]; then
  echo "==> Signing as: $IDENTITY"
  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" "$MACOS_DIR/local-focus-bin"
  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" "$APP_DIR"
else
  echo "==> No signing identity found; ad-hoc signing (this Mac only)."
  codesign --force --deep --sign - "$APP_DIR"
fi

codesign --verify --deep --strict --verbose=2 "$APP_DIR"

if [ "$IS_DEVELOPER_ID" -eq 0 ]; then
  echo ""
  echo "NOTE: not signed with a Developer ID Application certificate, so this"
  echo "build is for THIS Mac only — Gatekeeper will block it elsewhere."
  echo "See the header of this script for the one-time setup."
  exit 0
fi

if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
  echo ""
  echo "NOTE: signed with Developer ID, but no '$NOTARY_PROFILE' notary profile"
  echo "was found, so the app is not notarized yet. Store credentials once with"
  echo "'xcrun notarytool store-credentials' (see the header of this script),"
  echo "then re-run to notarize and staple."
  exit 0
fi

ZIP_PATH="$ROOT_DIR/target/macos/LocalFocus-notarize.zip"
echo "==> Notarizing (this waits for Apple, usually a few minutes)"
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_DIR" "$ZIP_PATH"
xcrun notarytool submit "$ZIP_PATH" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$APP_DIR"
xcrun stapler validate "$APP_DIR"
rm -f "$ZIP_PATH"

echo ""
echo "Signed, hardened, notarized, and stapled: $APP_DIR"
echo "This opens cleanly on any Mac."
