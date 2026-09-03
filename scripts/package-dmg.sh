#!/usr/bin/env sh
# Build the drag-to-Applications DMG that other people download.
#
# Usage:  sh scripts/package-dmg.sh
#
# This used to build the app with scripts/package-mas.sh — the Mac App Store
# pipeline, which signs with sandbox entitlements only when MAS_APP_SIGN_IDENTITY
# is set and otherwise leaves the app unsigned. The DMG you would hand someone
# therefore contained an unsigned app. It now uses the Developer ID pipeline,
# and signs and notarizes the DMG itself, since the DMG is what people download
# and what Gatekeeper checks.
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$ROOT_DIR/scripts/lib-signing.sh"

APP_NAME="Local Focus"
APP_DIR="$ROOT_DIR/target/macos/$APP_NAME.app"
DMG_ROOT="$ROOT_DIR/target/dmg-root"
DMG_PATH="$ROOT_DIR/target/macos/LocalFocus.dmg"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "DMG packaging is only available on macOS." >&2
  exit 1
fi

# Builds, signs, hardens, and (with credentials) notarizes the app itself.
"$ROOT_DIR/scripts/package-macos.sh"

rm -rf "$DMG_ROOT" "$DMG_PATH"
mkdir -p "$DMG_ROOT"
cp -R "$APP_DIR" "$DMG_ROOT/$APP_NAME.app"
ln -s /Applications "$DMG_ROOT/Applications"

hdiutil create \
  -volname "$APP_NAME" \
  -srcfolder "$DMG_ROOT" \
  -ov \
  -format UDZO \
  "$DMG_PATH"
rm -rf "$DMG_ROOT"

lf_pick_identity
if [ -n "$LF_IDENTITY" ]; then
  echo "==> Signing the disk image"
  codesign --force --timestamp --sign "$LF_IDENTITY" "$DMG_PATH"
fi

if [ "$LF_IS_DEVELOPER_ID" -eq 1 ] && lf_notarize_and_staple "$DMG_PATH"; then
  echo ""
  echo "Signed, notarized, and stapled: $DMG_PATH"
  echo "This downloads and opens cleanly on any Mac."
  exit 0
fi

lf_explain_missing_notarization
echo ""
echo "Built: $DMG_PATH"
