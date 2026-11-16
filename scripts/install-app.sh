#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/dist/RustVNC Agent.app"
DEST="/Applications/RustVNC Agent.app"
BIN_NAME="rustvnc-agent"

if [[ ! -x "$SRC/Contents/MacOS/$BIN_NAME" ]]; then
  echo "missing $SRC" >&2
  echo "run ./scripts/build-app.sh first" >&2
  exit 1
fi

if [[ -e "$DEST" && "${FORCE:-0}" != "1" ]]; then
  echo "$DEST already exists; set FORCE=1 to replace it" >&2
  exit 1
fi

launchctl bootout "gui/$(id -u)/com.rustvnc.agent" 2>/dev/null || true
"$SRC/Contents/MacOS/$BIN_NAME" --unregister 2>/dev/null || true

rm -rf "$DEST"
if ! cp -R "$SRC" "$DEST"; then
  echo "copy failed; /Applications usually needs admin rights: sudo $0" >&2
  exit 1
fi

# Re-register from the installed location so the login item does not point at
# the build directory.
"$DEST/Contents/MacOS/$BIN_NAME" --register
open "$DEST"

echo "installed: $DEST"
