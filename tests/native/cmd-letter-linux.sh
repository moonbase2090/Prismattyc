#!/usr/bin/env bash
# PR #396 Linux check: Super+letter must not type into the pane, Ctrl+C must
# still interrupt. Drives a real prismattyc-host window with scripted input.
#
#   cmd-letter-linux.sh --bins DIR --backend x11|wayland [--out DIR]
#
# x11:     needs DISPLAY with a window manager, and xdotool.
# wayland: needs wtype; uses WAYLAND_DISPLAY if set, else starts headless sway
#          (weston has no virtual-keyboard protocol, so wtype cannot type there).
# DIR holds prismattyc-host, pmux and pmuxd built from the head under test.
# Exit 0 only if every check passes. Results: OUT/result.txt.
set -uo pipefail

BINS="" BACKEND="" OUT="/tmp/cmd-letter-$$"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bins) BINS="$2"; shift 2 ;;
    --backend) BACKEND="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -x "$BINS/prismattyc-host" && -x "$BINS/pmuxd" ]] || { echo "need --bins with prismattyc-host and pmuxd" >&2; exit 2; }
[[ "$BACKEND" == x11 || "$BACKEND" == wayland ]] || { echo "need --backend x11|wayland" >&2; exit 2; }
mkdir -p "$OUT"
export PATH="$BINS:$PATH"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-$(id -u)}"
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
RESULT="$OUT/result.txt"; : >"$RESULT"
FAILS=0
log()  { printf '[cmd-letter] %s\n' "$*" | tee -a "$RESULT"; }
pass() { log "PASS: $*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS + 1)); }

SWAY_PID="" HOST_PID=""
cleanup() {
  [[ -n "$HOST_PID" ]] && kill "$HOST_PID" 2>/dev/null
  [[ -n "$SWAY_PID" ]] && kill "$SWAY_PID" 2>/dev/null
}
trap cleanup EXIT

# --- input backends ---------------------------------------------------------
if [[ "$BACKEND" == x11 ]]; then
  command -v xdotool >/dev/null || { echo "xdotool missing" >&2; exit 2; }
  [[ -n "${DISPLAY:-}" ]] || { echo "DISPLAY is not set" >&2; exit 2; }
  export WINIT_UNIX_BACKEND=x11
  focus()   { local w; w="$(xdotool search --onlyvisible --class prismattyc-host 2>/dev/null | tail -1)";
              [[ -n "$w" ]] && xdotool windowactivate --sync "$w" && xdotool windowfocus --sync "$w"; }
  typetext(){ xdotool type --clearmodifiers --delay 25 -- "$1"; }
  enter()   { xdotool key --clearmodifiers Return; }
  super()   { xdotool key --clearmodifiers "super+$1"; }
  ctrl()    { xdotool key --clearmodifiers "ctrl+$1"; }
  escape()  { xdotool key --clearmodifiers Escape; }
else
  command -v wtype >/dev/null || { echo "wtype missing" >&2; exit 2; }
  export WINIT_UNIX_BACKEND=wayland
  if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    command -v sway >/dev/null || { echo "no WAYLAND_DISPLAY and no sway" >&2; exit 2; }
    printf 'output * resolution 1600x1000\n' >"$OUT/sway.conf"
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
      sway -c "$OUT/sway.conf" >"$OUT/sway.log" 2>&1 & SWAY_PID=$!
    for _ in $(seq 1 100); do
      sock="$(ls "$XDG_RUNTIME_DIR" 2>/dev/null | grep -E '^wayland-[0-9]+$' | head -1)"
      [[ -n "$sock" ]] && break; sleep 0.1
    done
    [[ -n "${sock:-}" ]] || { echo "sway did not start; see $OUT/sway.log" >&2; exit 2; }
    export WAYLAND_DISPLAY="$sock"
  fi
  focus()   { :; }  # the new toplevel has keyboard focus
  # -s: each wtype call creates a new virtual keyboard; without a pause after
  # its keymap upload, the client drops the keys that follow.
  typetext(){ wtype -s 400 -d 25 -- "$1"; }
  enter()   { wtype -s 400 -k Return; }
  super()   { wtype -s 400 -M logo "$1" -m logo; }
  ctrl()    { wtype -s 400 -M ctrl "$1" -m ctrl; }
  escape()  { wtype -s 400 -k Escape; }
fi

# --- host -------------------------------------------------------------------
CFG="$OUT/config.toml"
printf 'automatic_update_checks = false\nspace_startup = "fresh"\nsession_naming = "auto"\n' >"$CFG"
PRISMATTYC_CONFIG="$CFG" PRISMATTYC_BELL_TOASTER=0 prismattyc-host >"$OUT/host.log" 2>&1 &
HOST_PID=$!
sleep 6
kill -0 "$HOST_PID" 2>/dev/null || { fail "host exited at startup (see host.log)"; exit 1; }
focus; typetext 'clear'; enter; sleep 1
log "backend=$BACKEND host=$("$BINS/prismattyc-host" --version 2>/dev/null | head -1)"

# 1. Super+C / Super+V in a shell pane: expect XY, the bug gives XcvY.
rm -f "$OUT/shell.txt"
focus; typetext 'echo X'; sleep 0.3
super c; sleep 0.4; super v; sleep 0.4
typetext "Y > $OUT/shell.txt"; enter; sleep 1.5
got="$(cat "$OUT/shell.txt" 2>/dev/null || echo '(not written)')"
[[ "$got" == XY ]] && pass "shell pane Super+C/Super+V typed nothing (got $got)" \
                   || fail "shell pane Super+C/Super+V: expected XY, got $got"

# 2. Ctrl+C still interrupts a foreground command.
rm -f "$OUT/int.txt" "$OUT/alive.txt"
focus; typetext "sleep 20; echo NOT_INTERRUPTED > $OUT/int.txt"; enter; sleep 2
ctrl c; sleep 1
typetext "echo ALIVE > $OUT/alive.txt"; enter; sleep 2
if [[ -f "$OUT/alive.txt" && ! -f "$OUT/int.txt" ]]; then
  pass "Ctrl+C interrupted sleep; prompt came back"
else
  fail "Ctrl+C did not interrupt (alive=$([[ -f $OUT/alive.txt ]] && echo yes || echo no), int=$([[ -f $OUT/int.txt ]] && echo yes || echo no))"
fi

# 3. Editors, when installed: insert X, Super+C, Super+V, Y; save; expect XY.
for ed in vim nvim; do
  command -v "$ed" >/dev/null || { log "SKIP: $ed not installed"; continue; }
  f="$OUT/$ed.txt"; rm -f "$f"
  focus; typetext "$ed -u NONE -i NONE $f"; enter; sleep 2
  typetext 'i'; typetext 'X'; super c; sleep 0.3; super v; sleep 0.3; typetext 'Y'
  escape; typetext ':wq'; enter; sleep 1.5
  got="$(cat "$f" 2>/dev/null || echo '(not written)')"
  [[ "$got" == XY ]] && pass "$ed Super+C/Super+V typed nothing (got $got)" \
                     || fail "$ed: expected XY, got $got"
done

# Late check: the interrupted command must not have finished after all.
sleep 18
[[ ! -f "$OUT/int.txt" ]] && pass "interrupted command never completed" \
                          || fail "interrupted command completed after 20s"

log "RESULT: $([[ $FAILS -eq 0 ]] && echo PASS || echo "FAIL ($FAILS)")"
exit $(( FAILS > 0 ))
