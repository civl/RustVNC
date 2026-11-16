#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$ROOT/dist/RustVNC Agent.app"
BIN="$APP/Contents/MacOS/rustvnc-agent"

if [[ ! -x "$BIN" ]]; then
  echo "missing $BIN" >&2
  echo "run ./scripts/build-app.sh first" >&2
  exit 1
fi

launchctl bootout "gui/$(id -u)/com.rustvnc.agent" 2>/dev/null || true
"$BIN" --unregister 2>/dev/null || true

# Give ServiceManagement/BTM a moment to settle after bootout.
sleep 1

"$BIN" --register
"$BIN" --status
