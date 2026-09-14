#!/usr/bin/env bash
# PT-295: double-click [show me] must advance the caption by one and
# must not skip. Accept the required session name after the clicks.
# Host driver: demo/docker/run.sh walkthrough-caption-e2e.
set -euo pipefail

export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
export DISPLAY="${DISPLAY:-:99}"
export WINIT_UNIX_BACKEND=x11
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-demo}"
SOCK="${XDG_RUNTIME_DIR}/prismattyc/pmux.sock"
HOST_LOG="${TMPDIR:-/tmp}/pt295-host.log"
DUMP="${PRISMATTYC_WALKTHROUGH_DUMP:-/tmp/pt295-caption.json}"
PROGRESS="${XDG_DATA_HOME:-$HOME/.local/share}/prismattyc/walkthrough.json"
PASSES=0
FAILS=0

log() { printf '[pt295] %s\n' "$*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS + 1)); }
pass() { log "PASS: $*"; PASSES=$((PASSES + 1)); }

find_host() {
  xdotool search --onlyvisible --class prismattyc-host 2>/dev/null | tail -1 || true
}

stop_host() {
  if [[ -f /tmp/pt295-host.pid ]]; then
    kill "$(cat /tmp/pt295-host.pid)" 2>/dev/null || true
    rm -f /tmp/pt295-host.pid
  fi
}

wait_dump() {
  local i
  for i in $(seq 1 50); do
    if [[ -s "$DUMP" ]] && python3 - "$DUMP" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
raise SystemExit(0 if d.get("show_me") else 1)
PY
    then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

launch_host() {
  mkdir -p "$HOME/work" "$(dirname "$PROGRESS")"
  rm -f "$PROGRESS" "$DUMP"
  ( cd "$HOME/work"
    env -u WAYLAND_DISPLAY COLORTERM=truecolor WINIT_UNIX_BACKEND=x11 DISPLAY="$DISPLAY" \
      PRISMATTYC_WALKTHROUGH_DUMP="$DUMP" PRISMATTYC_BELL_TOASTER=0 \
      prismattyc-host >"$HOST_LOG" 2>&1 & echo $! >/tmp/pt295-host.pid )
  local i
  for i in $(seq 1 50); do
    [[ -n "$(find_host)" ]] && break
    sleep 0.2
  done
  [[ -n "$(find_host)" ]] || { fail "host window did not appear"; tail -20 "$HOST_LOG" || true; return 1; }
  sleep 0.4
  pass "host launched"
}

start_level0() {
  local wid
  wid="$(find_host)"
  [[ -n "$wid" ]] || { fail "no host window for splash 4"; return 1; }
  xdotool windowactivate --sync "$wid"
  xdotool windowfocus --sync "$wid"
  sleep 0.1
  xdotool key --clearmodifiers 4
  if ! wait_dump; then
    fail "walkthrough dump missing after splash 4. log: $(tail -c 400 "$HOST_LOG" | tr '\n' ' ')"
    return 1
  fi
  python3 - "$DUMP" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("index") == 0, d
assert d.get("step_id") == "window.split-right", d
assert d.get("skipped") == [], d
print("index={index} step={step_id}".format(**d))
PY
  pass "level 0 caption armed $(python3 -c "import json; d=json.load(open('$DUMP')); print(d['step_id'], 'index', d['index'])")"
}

double_click_show_me() {
  local wid xy
  wid="$(find_host)"
  [[ -n "$wid" ]] || { fail "no host window for click"; return 1; }
  xy="$(python3 - "$DUMP" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
r = d["show_me"]
print(r["x"] + r["w"] // 2, r["y"] + r["h"] // 2)
PY
)"
  read -r cx cy <<<"$xy"
  log "double-click [show me] at window $cx $cy"
  xdotool windowactivate --sync "$wid"
  xdotool mousemove --window "$wid" --sync "$cx" "$cy"
  # Two presses at the same pixel within 500 ms. No wiggle. No Return.
  xdotool click --repeat 2 --delay 80 1
}

assert_one_advance() {
  sleep 2
  local dump_ok=0
  if [[ -s "$DUMP" ]]; then
    if python3 - "$DUMP" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
if d.get("index") != 1:
    print(f"dump index {d.get('index')} want 1", file=sys.stderr)
    raise SystemExit(1)
if d.get("skipped"):
    print(f"dump skipped {d.get('skipped')}", file=sys.stderr)
    raise SystemExit(1)
if d.get("step_id") != "window.focus-right":
    print(f"dump step {d.get('step_id')}", file=sys.stderr)
    raise SystemExit(1)
print("dump index=1 step=window.focus-right skipped=0")
PY
    then
      dump_ok=1
      pass "dump caption index +1 and not skipped"
    else
      fail "dump after idle: $(tr '\n' ' ' <"$DUMP")"
    fi
  else
    fail "dump missing after idle"
  fi
  if [[ -f "$PROGRESS" ]]; then
    if python3 - "$PROGRESS" <<'PY'
import json, sys
p = json.load(open(sys.argv[1]))
if p.get("current_step") != "window.focus-right":
    print(f"progress step {p.get('current_step')}", file=sys.stderr)
    raise SystemExit(1)
if p.get("skipped"):
    print(f"progress skipped {p.get('skipped')}", file=sys.stderr)
    raise SystemExit(1)
if "window.split-right" not in p.get("completed", []):
    print(f"progress completed {p.get('completed')}", file=sys.stderr)
    raise SystemExit(1)
print("progress current=window.focus-right skipped=0")
PY
    then
      pass "progress file advanced one step and is not skipped"
    else
      fail "progress after idle: $(tr '\n' ' ' <"$PROGRESS")"
    fi
  else
    fail "walkthrough progress file missing; walkthrough finished or never started"
  fi
  if grep -q 'walkthrough progress step=window.focus-right completed=1 skipped=0' "$HOST_LOG"; then
    pass "host log recorded one completed step and zero skips"
  else
    fail "host log missing one-step progress line. log: $(grep 'walkthrough progress' "$HOST_LOG" | tr '\n' ' ')"
  fi
  [[ "$dump_ok" -eq 1 ]]
}

cleanup() { stop_host; }
trap cleanup EXIT

log "socket $SOCK"
for _ in $(seq 1 30); do
  if pmux status 2>/dev/null | grep -q '^status: running$'; then
    break
  fi
  sleep 0.2
done

launch_host
start_level0
double_click_show_me
# The double click opens one naming popup. It must not finish the step yet.
sleep 2
python3 - "$DUMP" <<'PYWAIT'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["index"] == 0 and not d["completed"] and not d["skipped"], d
PYWAIT
pass "split waits for naming acceptance after the double click"
xdotool key --clearmodifiers Return
assert_one_advance

if [[ "$FAILS" -gt 0 ]]; then
  log "$PASSES passed, $FAILS failed"
  log "RESULT: FAIL"
  exit 1
fi
log "$PASSES passed, 0 failed"
log "RESULT: PASS"
