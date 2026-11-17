#!/usr/bin/env bash
# Build, sign (with hardened runtime) and optionally notarize the agent.
#
#   ./scripts/build-app.sh release            # ad-hoc, local testing only
#   CODESIGN_IDENTITY="Developer ID Application: X (TEAM)" ./scripts/build-app.sh release
#   CODESIGN_IDENTITY="Developer ID Application: X (TEAM)" NOTARY_PROFILE=rustvnc ./scripts/build-app.sh release
#
# Why the identity matters beyond "Gatekeeper is happier": TCC permissions
# (System Audio Recording, Screen Recording) are bound to the code signing
# identity. Rebuilt ad-hoc binaries look like a different program every time, so
# the user is asked for the same permission over and over and SMAppService
# refuses to register the LaunchAgent (kSMErrorInvalidSignature).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="${1:-debug}"
case "$PROFILE" in
  debug)
    CARGO_ARGS=()
    BIN="$ROOT/target/debug/rustvnc-agent"
    ;;
  release|--release)
    CARGO_ARGS=(--release)
    BIN="$ROOT/target/release/rustvnc-agent"
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

cargo build "${CARGO_ARGS[@]}"

APP="$ROOT/dist/RustVNC Agent.app"
ENTITLEMENTS="$ROOT/resources/RustVNCAgent.entitlements"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"
mkdir -p "$APP/Contents/Library/LaunchAgents"

cp "$BIN" "$APP/Contents/MacOS/rustvnc-agent"
cp "$ROOT/resources/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/resources/com.rustvnc.agent.plist" "$APP/Contents/Library/LaunchAgents/com.rustvnc.agent.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

plutil -lint "$APP/Contents/Info.plist" >/dev/null
plutil -lint "$APP/Contents/Library/LaunchAgents/com.rustvnc.agent.plist" >/dev/null
plutil -lint "$ENTITLEMENTS" >/dev/null

IDENTITY="${CODESIGN_IDENTITY:--}"
echo "codesign identity: $IDENTITY"

if [ "$IDENTITY" = "-" ]; then
  # Ad-hoc: fine for a local smoke test, but TCC/LaunchAgent will not survive a
  # rebuild, and the bundle cannot be notarized.
  codesign --force --sign - --identifier "com.rustvnc.agent" \
    --entitlements "$ENTITLEMENTS" "$APP/Contents/MacOS/rustvnc-agent"
  codesign --force --sign - --entitlements "$ENTITLEMENTS" "$APP"
else
  # Hardened runtime is mandatory for notarization.
  codesign --force --sign "$IDENTITY" --identifier "com.rustvnc.agent" \
    --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" "$APP/Contents/MacOS/rustvnc-agent"
  codesign --force --sign "$IDENTITY" \
    --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" "$APP"
fi

codesign --verify --deep --strict --verbose=2 "$APP"

if [ "$IDENTITY" != "-" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
  "$ROOT/scripts/notarize.sh" "$APP" "$NOTARY_PROFILE"
elif [ "$IDENTITY" != "-" ]; then
  echo
  echo "Signed but not notarized. To notarize:"
  echo "  xcrun notarytool store-credentials rustvnc --apple-id ... --team-id ... --password ..."
  echo "  NOTARY_PROFILE=rustvnc CODESIGN_IDENTITY=\"$IDENTITY\" $0 $PROFILE"
fi

echo
echo "built:  $APP"
echo "run:    open \"$APP\""
echo "status: \"$APP/Contents/MacOS/rustvnc-agent\" --status"
