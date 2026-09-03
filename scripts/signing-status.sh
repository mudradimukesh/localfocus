#!/usr/bin/env sh
# Report what a distribution build would produce on this machine, before
# spending a few minutes building one.
#
# Usage:  sh scripts/signing-status.sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$ROOT_DIR/scripts/lib-signing.sh"

echo "Signing identities on this Mac:"
security find-identity -v -p codesigning 2>/dev/null | sed 's/^/  /' | head -20
echo ""

lf_pick_identity
if [ -z "$LF_IDENTITY" ]; then
  echo "Identity to be used:  none (ad-hoc)"
else
  echo "Identity to be used:  $LF_IDENTITY"
fi

if lf_notary_available; then
  echo "Notary profile:       '$NOTARY_PROFILE' found"
else
  echo "Notary profile:       '$NOTARY_PROFILE' missing"
fi
echo ""

if [ "$LF_IS_DEVELOPER_ID" -eq 1 ] && lf_notary_available; then
  echo "Result: signed, hardened, notarized and stapled."
  echo "        Opens cleanly on any Mac."
  exit 0
fi

if [ "$LF_IS_DEVELOPER_ID" -eq 1 ]; then
  echo "Result: signed and hardened, but NOT notarized."
else
  echo "Result: NOT distributable. Gatekeeper will block this on other Macs."
fi
lf_explain_missing_notarization
