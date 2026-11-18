#!/usr/bin/env bash
# Capture Apple's private pseudo-encoding bodies from a real macOS session.
#
# The password is read without echo and exported only to the probe's process.
# It is never written to the capture file. Output includes the raw body hex for
# 1104/1105/1107/1109/1110.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HOST="${RUSTVNC_MAC_HOST:-127.0.0.1}"
PORT="${RUSTVNC_MAC_PORT:-5900}"
USER_NAME="${RUSTVNC_MAC_USER:-$(whoami)}"
SECONDS_TO_RUN="${RUSTVNC_CAPTURE_SECONDS:-12}"
mkdir -p target/apple-capture
OUT="target/apple-capture/apple-$(date +%Y%m%d-%H%M%S).txt"

printf 'Mac host [%s]: ' "$HOST"
read -r input_host
[ -n "$input_host" ] && HOST="$input_host"
printf 'Mac user [%s]: ' "$USER_NAME"
read -r input_user
[ -n "$input_user" ] && USER_NAME="$input_user"
printf 'Password (not echoed): '
read -rs VNC_PASSWORD
printf '\n'
export VNC_PASSWORD

# The probe runs in the background so we can move the remote pointer and
# change the display mode while it is connected. macOS only emits 1104 on
# pointer motion and 1105 on a display-geometry change.
cargo run --quiet -p rfb-probe -- \
  --host "$HOST" \
  --port "$PORT" \
  --user "$USER_NAME" \
  --apple \
  --wiggle \
  --seconds "$SECONDS_TO_RUN" \
  --save target/apple-capture/last-frame.png \
  >"$OUT" 2>&1 &
probe_pid=$!

sleep 3
if [ "${RUSTVNC_CAPTURE_DISPLAY_SWITCH:-1}" = "1" ]; then
  cargo run --quiet -p rustvnc-agent --bin display-switch -- 1920 1080 \
    >target/apple-capture/display-switch.log 2>&1 || \
    echo "warning: display-switch failed; see target/apple-capture/display-switch.log"
fi

wait "$probe_pid"
unset VNC_PASSWORD
cat "$OUT"

echo
echo "capture: $OUT"
echo "请把文件里的 apple encoding 1105/1104 行发回（密码不在文件里）。"
