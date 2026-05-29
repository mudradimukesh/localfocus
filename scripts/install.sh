#!/usr/bin/env sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN_DIR="${LOCAL_FOCUS_BIN_DIR:-$HOME/.local/bin}"

cd "$ROOT_DIR"
cargo build --release

mkdir -p "$BIN_DIR"
cp "$ROOT_DIR/target/release/local-focus" "$BIN_DIR/local-focus"
chmod +x "$BIN_DIR/local-focus"

echo "Installed local-focus binary to: $BIN_DIR/local-focus"

if [ "$(uname -s)" = "Darwin" ]; then
  "$ROOT_DIR/scripts/package-mas.sh"
  APP_DEST="${LOCAL_FOCUS_APP_DIR:-$HOME/Applications}"
  mkdir -p "$APP_DEST"
  rm -rf "$APP_DEST/Local Focus.app"
  cp -R "$ROOT_DIR/target/macos/Local Focus.app" "$APP_DEST/Local Focus.app"
  echo "Installed macOS app to: $APP_DEST/Local Focus.app"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Tip: add $BIN_DIR to PATH to run 'local-focus' from any terminal." ;;
esac

echo "Start from terminal: $BIN_DIR/local-focus serve"
echo "Dashboard: http://127.0.0.1:4799"
