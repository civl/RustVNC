#!/usr/bin/env bash
# Submit an already-signed .app for notarization and staple the ticket.
#
#   xcrun notarytool store-credentials rustvnc \
#     --apple-id "you@example.com" --team-id TEAMID --password "app-specific-password"
#   ./scripts/notarize.sh "dist/RustVNC Agent.app" rustvnc
#
# The bundle must be signed with a Developer ID identity and the hardened
# runtime (`scripts/build-app.sh` does both when CODESIGN_IDENTITY is set);
# notarytool rejects ad-hoc signatures.
set -euo pipefail

APP="${1:?usage: notarize.sh <path-to-.app> <keychain-profile>}"
PROFILE="${2:?usage: notarize.sh <path-to-.app> <keychain-profile>}"

if [ ! -d "$APP" ]; then
  echo "not a bundle: $APP" >&2
  exit 2
fi

echo "checking signature before submission…"
codesign --verify --deep --strict --verbose=2 "$APP"
if ! codesign -dv --verbose=4 "$APP" 2>&1 | grep -q "flags=0x10000(runtime)"; then
  echo "refusing to notarize: bundle was not signed with the hardened runtime" >&2
  exit 3
fi

ZIP="$(mktemp -d)/agent.zip"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "submitting $APP …"
xcrun notarytool submit "$ZIP" --keychain-profile "$PROFILE" --wait

echo "stapling ticket…"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP"

echo "notarized: $APP"
