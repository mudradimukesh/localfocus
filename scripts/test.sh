#!/usr/bin/env sh
# Run the full Local Focus test suite: Rust core/server tests + the Flutter
# companion analyze & tests. Run this before every deploy so regressions like
# the "Move to app" warning action are caught automatically.
#
# Usage:  sh scripts/test.sh
# Flutter is not on PATH on this machine; override its location with FLUTTER_BIN.
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
MOBILE_DIR="$ROOT_DIR/mobile/local_focus_mobile"
FLUTTER="${FLUTTER_BIN:-/Users/mukeshkumarmudradi/Documents/projects/flutter/flutter/bin/flutter}"

echo "==> Rust core/server tests (cargo test)"
( cd "$ROOT_DIR" && cargo test )

if [ -x "$FLUTTER" ] || command -v flutter >/dev/null 2>&1; then
  [ -x "$FLUTTER" ] || FLUTTER=flutter
  echo "==> Flutter analyze (companion)"
  ( cd "$MOBILE_DIR" && "$FLUTTER" analyze lib )
  echo "==> Flutter tests (companion)"
  ( cd "$MOBILE_DIR" && "$FLUTTER" test )
else
  echo "WARNING: flutter not found (set FLUTTER_BIN) — skipped Flutter checks."
  exit 1
fi

echo ""
echo "✅ All Local Focus tests passed."
