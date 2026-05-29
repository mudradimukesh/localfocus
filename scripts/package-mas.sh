#!/usr/bin/env sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
APP_NAME="Local Focus"
APP_DIR="$ROOT_DIR/target/macos/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
INFO_PLIST="$CONTENTS_DIR/Info.plist"
ENTITLEMENTS="$ROOT_DIR/macos/LocalFocus.entitlements"
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
  -o "$MACOS_DIR/local-focus"

rm -rf "$ICONSET_DIR"
python3 "$ROOT_DIR/macos/make-icon.py" "$ICONSET_DIR"
iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/AppIcon.icns"

/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID" "$INFO_PLIST"

if [ -n "${MAS_APP_SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --entitlements "$ENTITLEMENTS" --sign "$MAS_APP_SIGN_IDENTITY" "$APP_DIR"
  codesign --verify --deep --strict --verbose=2 "$APP_DIR"
else
  echo "Created unsigned app bundle: $APP_DIR"
  echo "Set MAS_APP_SIGN_IDENTITY to sign for Mac App Store distribution."
fi

if [ -n "${MAS_INSTALLER_SIGN_IDENTITY:-}" ]; then
  productbuild --component "$APP_DIR" /Applications --sign "$MAS_INSTALLER_SIGN_IDENTITY" "$ROOT_DIR/target/macos/LocalFocus.pkg"
  echo "Created signed Mac App Store package: $ROOT_DIR/target/macos/LocalFocus.pkg"
fi
