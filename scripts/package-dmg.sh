#!/usr/bin/env sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
APP_NAME="Local Focus"
APP_DIR="$ROOT_DIR/target/macos/$APP_NAME.app"
DMG_ROOT="$ROOT_DIR/target/dmg-root"
DMG_PATH="$ROOT_DIR/target/macos/LocalFocus.dmg"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "DMG packaging is only available on macOS." >&2
  exit 1
fi

"$ROOT_DIR/scripts/package-mas.sh"

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

echo "Created drag-to-Applications DMG: $DMG_PATH"
