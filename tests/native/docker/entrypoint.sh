#!/usr/bin/env bash
# Start a private display, window manager, and test sessions.
set -euo pipefail
export DISPLAY="${DISPLAY:-:99}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-tester}"
mkdir -p "$XDG_RUNTIME_DIR" "$HOME/.config/prismattyc"
chmod 700 "$XDG_RUNTIME_DIR"
if [[ ! -S /tmp/.X11-unix/X${DISPLAY#:} ]]; then
  Xvfb "$DISPLAY" -screen 0 1920x1080x24 -ac +extension GLX +render -noreset \
    >/tmp/xvfb.log 2>&1 &
  for _ in $(seq 1 50); do
    xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 && break
    sleep 0.1
  done
fi
xsetroot -solid "#121214"
openbox >/tmp/openbox.log 2>&1 &
sleep 0.3

pmux up
sleep 0.4
# Two agent seats (session name = agent id) plus a plain work session.
mkdir -p "$HOME/work"
( cd "$HOME/work" && pmux new claude --no-attach -- bash -l >/dev/null 2>&1 || true )
( cd "$HOME/work" && pmux new kiro   --no-attach -- bash -l >/dev/null 2>&1 || true )
( cd "$HOME/work" && pmux new work   --no-attach --no-agent -- bash -l >/dev/null 2>&1 || true )

exec "$@"
