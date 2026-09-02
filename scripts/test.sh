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

# The dashboard's HTML/CSS/JS lives inside a Rust raw string, so a broken
# script still compiles and still passes cargo test — it just breaks in the
# browser. Parse it for real when node is available.
if command -v node >/dev/null 2>&1; then
  echo "==> Dashboard JS syntax (node --check)"
  # node --check infers the parser from the extension, so it has to end in .js.
  DASHBOARD_DIR=$(mktemp -d -t local-focus-dashboard)
  ( cd "$ROOT_DIR" && cargo run --quiet -- dump-dashboard ) \
    | python3 -c "import re,sys; sys.stdout.write('\n'.join(re.findall(r'<script>(.*?)</script>', sys.stdin.read(), re.S)))" \
    > "$DASHBOARD_DIR/dashboard.js"
  node --check "$DASHBOARD_DIR/dashboard.js"
  rm -rf "$DASHBOARD_DIR"
else
  echo "==> Dashboard JS syntax skipped (node not found)"
fi

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
