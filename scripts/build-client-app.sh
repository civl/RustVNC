#!/usr/bin/env bash
# Package the Windows-style RustVNC client as a macOS .app bundle.
#
# Same binary, same UI as on Windows: on macOS it is the controller (connect to
# another Mac) and it can hand the "share this Mac" card to the local Agent.
#
#   ./scripts/build-client-app.sh release
#
# Output: dist/RustVNC.app (ad-hoc signed; Developer ID signing is the same story
# as scripts/build-app.sh).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${1:-release}"

case "$PROFILE" in
  release) TARGET_DIR="release" ;;
  debug) TARGET_DIR="debug" ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
esac

BIN="$ROOT/target/$TARGET_DIR/rustvnc-client"
APP="$ROOT/dist/RustVNC.app"

# Always build: skipping when the binary exists shipped a stale app once.
echo "building rustvnc-client + rustvnc-agent ($PROFILE)…"
# The agent is the host half of the app: same bundle, second binary.
( cd "$ROOT" && cargo build "-p" rustvnc-client -p rustvnc-agent --"$PROFILE" )

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/rustvnc-client"
AGENT_BIN="$ROOT/target/$TARGET_DIR/rustvnc-agent"
if [[ -x "$AGENT_BIN" ]]; then
  cp "$AGENT_BIN" "$APP/Contents/MacOS/rustvnc-agent"
else
  echo "warning: $AGENT_BIN missing; the bundle will be controller-only" >&2
fi

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>RustVNC</string>
  <key>CFBundleDisplayName</key><string>RustVNC</string>
  <key>CFBundleIdentifier</key><string>com.rustvnc.client</string>
  <key>CFBundleExecutable</key><string>rustvnc-client</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleVersion</key><string>0.2.0</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <!-- Menu-bar app: no Dock icon, the tray icon is the entry point. -->
  <key>LSUIElement</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --sign "$CODESIGN_IDENTITY" "$APP"
else
  codesign --force --sign - "$APP"
fi

echo "built:  $APP"
echo "run:    open \"$APP\""
