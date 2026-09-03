#!/usr/bin/env sh
# Shared signing and notarization helpers, so the app build and the DMG build
# cannot drift apart on how they sign things. Source this; do not run it.
#
# The rule everywhere: sign with the strongest identity present, use the
# hardened runtime (required for notarization), and notarize plus staple when
# Developer ID credentials exist. Say plainly when they do not, rather than
# quietly producing something the next Mac refuses to open.

NOTARY_PROFILE="${NOTARY_PROFILE:-notary}"

# Sets LF_IDENTITY to the identity to sign with (empty when the machine has
# none) and LF_IS_DEVELOPER_ID=1 only for a real Developer ID Application
# cert, which is the only kind Gatekeeper accepts on someone else's Mac.
# Sets variables rather than echoing, so callers do not have to run it in a
# subshell and lose LF_IS_DEVELOPER_ID.
lf_pick_identity() {
  LF_IS_DEVELOPER_ID=0
  if [ -n "${DEVELOPER_ID_IDENTITY:-}" ]; then
    LF_IDENTITY="$DEVELOPER_ID_IDENTITY"
    LF_IS_DEVELOPER_ID=1
    return
  fi
  LF_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
    | grep "Developer ID Application" | head -1 | sed 's/.*"\(.*\)".*/\1/')
  if [ -n "$LF_IDENTITY" ]; then
    LF_IS_DEVELOPER_ID=1
    return
  fi
  # Fall back to whatever is available (typically "Apple Development"). Good
  # enough to run here; Gatekeeper will still stop it elsewhere.
  LF_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
    | head -1 | sed 's/.*"\(.*\)".*/\1/')
}

lf_notary_available() {
  xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1
}

# lf_sign_app <app path> <entitlements path> <identity>
# Signs the nested server binary before the bundle that contains it, which
# codesign requires.
lf_sign_app() {
  _app="$1"; _entitlements="$2"; _identity="$3"
  if [ -z "$_identity" ]; then
    echo "==> No signing identity found; ad-hoc signing (this Mac only)."
    codesign --force --deep --sign - "$_app"
  else
    echo "==> Signing as: $_identity"
    codesign --force --options runtime --timestamp \
      --entitlements "$_entitlements" --sign "$_identity" "$_app/Contents/MacOS/local-focus-bin"
    codesign --force --options runtime --timestamp \
      --entitlements "$_entitlements" --sign "$_identity" "$_app"
  fi
  codesign --verify --deep --strict --verbose=2 "$_app"
}

# lf_notarize_and_staple <path to .dmg or .zip> [path to staple instead]
# Submits to Apple, waits, and staples the ticket. Returns non-zero without
# doing anything when credentials are missing, so callers can carry on and
# report what is needed.
lf_notarize_and_staple() {
  _artifact="$1"
  _staple_target="${2:-$1}"
  if ! lf_notary_available; then
    return 1
  fi
  echo "==> Notarizing (this waits for Apple, usually a few minutes)"
  xcrun notarytool submit "$_artifact" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$_staple_target"
  xcrun stapler validate "$_staple_target"
}

# What to tell someone when the build cannot be notarized.
lf_explain_missing_notarization() {
  echo ""
  if [ "${LF_IS_DEVELOPER_ID:-0}" -eq 0 ]; then
    echo "NOTE: no Developer ID Application certificate, so this build only"
    echo "opens on this Mac — Gatekeeper blocks it elsewhere."
    echo ""
    echo "To fix, once (needs a paid Apple Developer account, so it cannot be"
    echo "scripted for you):"
    echo "  1. Enrol: https://developer.apple.com/programs/  (99 USD/year)"
    echo "  2. Xcode > Settings > Accounts > Manage Certificates > + >"
    echo "     \"Developer ID Application\""
    echo "  3. xcrun notarytool store-credentials \"$NOTARY_PROFILE\" \\"
    echo "       --apple-id \"you@example.com\" --team-id \"YOURTEAMID\" \\"
    echo "       --password \"app-specific-password\""
  else
    echo "NOTE: signed with Developer ID, but no '$NOTARY_PROFILE' notary"
    echo "profile was found, so this is not notarized yet. Store credentials"
    echo "once with 'xcrun notarytool store-credentials', then re-run."
  fi
  echo ""
  echo "Until then, whoever you send it to has to bypass Gatekeeper by hand:"
  echo "  right-click the app > Open > Open, or"
  echo "  xattr -dr com.apple.quarantine \"/Applications/Local Focus.app\""
}
