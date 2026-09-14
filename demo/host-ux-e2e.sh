#!/usr/bin/env bash
# PT-303: native X11 caret, focus-border, and cold-start regression checks.
set -euo pipefail
export DISPLAY="${DISPLAY:-:99}"
export WINIT_UNIX_BACKEND=x11
export HOST_UX_OUT="${HOST_UX_OUT:-/tmp/host-ux-e2e}"
mkdir -p "$HOST_UX_OUT"
for program in python3 xdotool ffmpeg pmux prismattyc-host; do
  command -v "$program" >/dev/null || { echo "missing $program" >&2; exit 1; }
done
case "${HOST_UX_CASE:-all}" in
  all) python3 "$(dirname "${BASH_SOURCE[0]}")/host-ux-e2e.py" 2>&1 | tee "$HOST_UX_OUT/runner.log" ;;
  space-open-race) ;;
  *) echo "unknown HOST_UX_CASE" >&2; exit 1 ;;
esac

python3 "$(dirname "${BASH_SOURCE[0]}")/space-open-race.py" 2>&1 | tee "$HOST_UX_OUT/space-open-race.log"
if [[ "${HOST_UX_CASE:-all}" == all ]]; then
  RESTART_SPACES_OUT="$HOST_UX_OUT/restart-spaces" \
    python3 "$(dirname "${BASH_SOURCE[0]}")/restart-spaces-e2e.py" 2>&1 | tee "$HOST_UX_OUT/restart-spaces.log"
fi
