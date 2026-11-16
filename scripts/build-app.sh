#!/usr/bin/env bash
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

IDENTITY="${CODESIGN_IDENTITY:--}"
echo "codesign identity: $IDENTITY"
codesign --force --sign "$IDENTITY" --identifier "com.rustvnc.agent" "$APP/Contents/MacOS/rustvnc-agent"
codesign --force --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=1 "$APP"

echo
echo "built:  $APP"
echo "run:    open \"$APP\""
echo "status: \"$APP/Contents/MacOS/rustvnc-agent\" --status"
echo
echo "If SMAppService returns kSMErrorInvalidSignature, rebuild with a real signing identity:"
echo "  CODESIGN_IDENTITY=\"Apple Development: Your Name (TEAMID)\" $0"
