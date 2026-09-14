#!/usr/bin/env bash
# PT-290: native Wayland present in the demo box.
# Second job beside X11 spaces-e2e. Weston headless --socket=pt290-wayland.
# Proves wl_shm against a conforming compositor, not KWin buffer-release
# timing. See docs/testing-policy.md.
#
# Run 1: window_opacity 0.95 so want_alpha is true. Assert the host
# selected wl_shm. A nested XWayland window that looks fine is a fail.
# Then require an ordered full frame followed by a partial wl_shm
# frame that uses the scroll blit path.
#
# Run 2: window_opacity 1.0, window_blur false. Opaque native Wayland
# stays on softbuffer. Assert the startup line is not wl_shm. Dismiss
# the splash, type, and assert splash pixels are gone across two
# consecutive presents. That is the PT-294 catch.
set -euo pipefail

export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-demo}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

SOCK="${XDG_RUNTIME_DIR}/prismattyc/pmux.sock"
export PMUX_SOCKET="$SOCK"
HOST_LOG="${TMPDIR:-/tmp}/pt290-host.log"
OPAQUE_LOG="${TMPDIR:-/tmp}/pt290-opaque-host.log"
WAYLAND_CONFIG="${TMPDIR:-/tmp}/pt290-host-config-$$.toml"
export PRISMATTYC_DUMP_PRESENT="${PRISMATTYC_DUMP_PRESENT:-/tmp/pt290-present.png}"
export PRISMATTYC_E2E_DISMISS_SPLASH_MS="${PRISMATTYC_E2E_DISMISS_SPLASH_MS:-1500}"
PASSES=0
FAILS=0

log() { printf '[pt290] %s\n' "$*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS + 1)); }
pass() { log "PASS: $*"; PASSES=$((PASSES + 1)); }
trap 'log "FAIL: line ${LINENO}: ${BASH_COMMAND} (exit $?)"; exit 1' ERR

die_weston() {
  log "FAIL: $*"
  [[ -f /tmp/weston.log ]] && tail -40 /tmp/weston.log >&2 || true
  log "runtime dir: $(ls -l "$XDG_RUNTIME_DIR" 2>/dev/null | tr '\n' ' ')"
  exit 1
}

start_weston() {
  # Unique socket we create. Do not attach to wayland-0/wayland-1.
  export WAYLAND_DISPLAY=pt290-wayland
  unset DISPLAY || true
  unset WINIT_UNIX_BACKEND || true
  # --debug publishes the screenshooter protocol. grim talks
  # wlr-screencopy, which headless weston does not implement.
  weston --backend=headless --socket=pt290-wayland \
    --width=1920 --height=1080 --idle-time=0 --debug \
    >/tmp/weston.log 2>&1 &
  echo $! >/tmp/pt290-weston.pid
  local i
  for i in $(seq 1 50); do
    if [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]; then
      return 0
    fi
    sleep 0.1
  done
  die_weston "weston did not publish $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
}

assert_native_wayland_env() {
  if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    fail "WAYLAND_DISPLAY is empty"
    return 1
  fi
  if [[ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]; then
    fail "WAYLAND_DISPLAY socket missing: $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
    return 1
  fi
  if [[ -n "${WINIT_UNIX_BACKEND:-}" && "${WINIT_UNIX_BACKEND}" == "x11" ]]; then
    fail "WINIT_UNIX_BACKEND=x11 would force XWayland; unset it for this job"
    return 1
  fi
  pass "wayland env WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
}

write_alpha_config() {
  # want_alpha is window_opacity < 1. Without this the host stays on
  # opaque softbuffer even on native Wayland.
  cat >"$WAYLAND_CONFIG" <<'EOF'
window_opacity = 0.95
render_timer = "log"
render_timer_log_every_frame = true
panes = 1
bell_toaster = false
EOF
}

write_opaque_config() {
  cat >"$WAYLAND_CONFIG" <<'EOF'
window_opacity = 1.0
window_blur = false
render_timer = "log"
render_timer_log_every_frame = true
panes = 1
bell_toaster = false
EOF
}

launch_host() {
  mkdir -p "$HOME/work"
  : >"$HOST_LOG"
  ( cd "$HOME/work"
    env -u DISPLAY -u WINIT_UNIX_BACKEND \
      WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
      XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
      COLORTERM=truecolor \
      PRISMATTYC_CONFIG="$WAYLAND_CONFIG" \
      PRISMATTYC_BELL_TOASTER=0 \
      prismattyc-host -- sh -c '
        sleep 1
        i=0
        while [ "$i" -lt 200 ]; do
          printf "PT244 row %03d\n" "$i"
          i=$((i + 1))
          sleep 0.01
        done
        sleep 2
      ' >"$HOST_LOG" 2>&1 &
    echo $! >/tmp/pt290-host.pid )
}

# Bare launch so the splash shows. An explicit program skips it.
launch_opaque_host() {
  mkdir -p "$HOME/work"
  : >"$OPAQUE_LOG"
  rm -f "${PRISMATTYC_DUMP_PRESENT:-/tmp/pt290-present.png}"
  ( cd "$HOME/work"
    env -u DISPLAY -u WINIT_UNIX_BACKEND -u PRISMATTYC_NO_SPLASH \
      WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
      XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
      COLORTERM=truecolor \
      PRISMATTYC_CONFIG="$WAYLAND_CONFIG" \
      PRISMATTYC_BELL_TOASTER=0 \
      PRISMATTYC_DUMP_PRESENT="${PRISMATTYC_DUMP_PRESENT:-/tmp/pt290-present.png}" \
      PRISMATTYC_E2E_DISMISS_SPLASH_MS="${PRISMATTYC_E2E_DISMISS_SPLASH_MS:-1500}" \
      PMUX_SOCKET="$SOCK" \
      prismattyc-host >"$OPAQUE_LOG" 2>&1 &
    echo $! >/tmp/pt290-host.pid )
}

stop_host() {
  local pid
  pid="$(cat /tmp/pt290-host.pid 2>/dev/null || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    sleep 0.4
    kill -9 "$pid" 2>/dev/null || true
  fi
  pkill -x prismattyc-host 2>/dev/null || true
  sleep 0.3
  rm -f /tmp/pt290-host.pid
}

assert_shm_backend() {
  local i line
  for i in $(seq 1 50); do
    if grep -q 'prismattyc-host: wayland shm present (ARGB8888)' "$HOST_LOG" 2>/dev/null; then
      line="$(grep 'prismattyc-host: wayland shm present (ARGB8888)' "$HOST_LOG" | head -1)"
      if grep -q 'falling back to softbuffer' "$HOST_LOG"; then
        fail "host fell back to softbuffer: $(tr '\n' ' ' <"$HOST_LOG" | tail -c 400)"
        return 1
      fi
      pass "host selected wl_shm: $line"
      return 0
    fi
    if grep -q 'wayland shm init failed' "$HOST_LOG" 2>/dev/null; then
      fail "wl_shm init failed: $(grep 'wayland shm' "$HOST_LOG" | tr '\n' ' ')"
      return 1
    fi
    sleep 0.2
  done
  fail "host did not print wl_shm present line. log: $(tr '\n' ' ' <"$HOST_LOG" | tail -c 600)"
  return 1
}

assert_shm_partial_scroll() {
  python3 - "$HOST_LOG" <<'PY'
import re
import sys
import time

path = sys.argv[1]
pattern = re.compile(
    r"cells_painted=(\d+) rows_scrolled_as_blit=(\d+) full_repaint_reason=(\S+)"
)
saw_full = False
last = ""
for _ in range(80):
    try:
        lines = open(path, encoding="utf-8", errors="replace").readlines()
    except FileNotFoundError:
        lines = []
    for line in lines:
        match = pattern.search(line)
        if not match:
            continue
        cells, blit_rows, reason = int(match.group(1)), int(match.group(2)), match.group(3)
        last = f"cells={cells} blit_rows={blit_rows} full={reason}"
        if reason != "-":
            saw_full = True
            continue
        if saw_full and blit_rows > 0:
            print(last)
            raise SystemExit(0)
    time.sleep(0.25)
print(
    f"no partial wl_shm scroll blit after a full frame: "
    f"{last or 'no render samples'}; full_frame={saw_full}",
    file=sys.stderr,
)
raise SystemExit(1)
PY
  pass "wl_shm presented a partial scroll-blit frame after a full frame"
}

present_count() {
  local n
  n="$(grep -c 'cells_painted=' "$1" 2>/dev/null || true)"
  echo "${n:-0}"
}

wait_presents() {
  local path="$1"
  local need="$2"
  local i
  for i in $(seq 1 50); do
    if [[ "$(present_count "$path")" -ge "$need" ]]; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}

dump_sidecar() {
  local dump="${PRISMATTYC_DUMP_PRESENT:-/tmp/pt290-present.png}"
  echo "${dump%.png}.json"
}

read_dump_seq() {
  local sidecar="$1"
  python3 -c 'import json,sys
try:
    print(int(json.load(open(sys.argv[1])).get("seq", 0)))
except Exception:
    print(0)
' "$sidecar"
}

wait_dump_seq_gt() {
  local sidecar="$1"
  local prev="$2"
  local i s
  for i in $(seq 1 80); do
    if [[ -s "$sidecar" ]]; then
      s="$(read_dump_seq "$sidecar")"
      if [[ "$s" -gt "$prev" ]]; then
        echo "$s"
        return 0
      fi
    fi
    sleep 0.2
  done
  return 1
}

assert_opaque_not_shm() {
  if ! wait_presents "$OPAQUE_LOG" 1; then
    fail "opaque host did not present. log: $(tr '\n' ' ' <"$OPAQUE_LOG" | tail -c 600)"
    return 1
  fi
  if grep -q 'prismattyc-host: wayland shm present (ARGB8888)' "$OPAQUE_LOG"; then
    fail "opaque window selected wl_shm: $(grep 'wayland shm present' "$OPAQUE_LOG" | head -1)"
    return 1
  fi
  if grep -q 'falling back to softbuffer' "$OPAQUE_LOG"; then
    fail "opaque host hit the shm-init fallback path: $(tr '\n' ' ' <"$OPAQUE_LOG" | tail -c 400)"
    return 1
  fi
  pass "opaque native Wayland did not select wl_shm"
}

capture_compositor() {
  local out="$1"
  local dump="${PRISMATTYC_DUMP_PRESENT:-/tmp/pt290-present.png}"
  rm -f "$out" "${out}.err"
  # Host dump of the last presented CPU framebuffer. Do not wait on grim
  # or weston-screenshooter: weston has no wlr-screencopy, and
  # weston-screenshooter hung the 1788708660-82636dbd run.
  if [[ -s "$dump" ]]; then
    cp -f "$dump" "$out"
    return 0
  fi
  fail "present dump missing at $dump; opaque splash assert needs host pixels"
  return 1
}

# Brand splash spectrum from prismattyc-core splash.rs.
# SPECTRUM-only (no INK): grey glyphs and chrome are not the signature.
# SLOP=12 covers the tinted beam. MAX_STALE=0: any leftover SPECTRUM
# pixel after dismiss is the PT-294 leftover-buffer catch. AA edges of
# INK text are excluded because INK is not in the signature.
splash_pixel_cmd() {
  python3 - "$@" <<'PY'
import struct
import sys
import zlib

SPECTRUM = (
    (0xFF, 0x6E, 0x63),
    (0xFF, 0xB4, 0x54),
    (0xFF, 0xE0, 0x66),
    (0x7B, 0xD8, 0x8F),
    (0x62, 0xA8, 0xFF),
    (0x7B, 0x8C, 0xFA),
    (0x9B, 0x8C, 0xF5),
)
SLOP = 12
MIN_SPLASH = 4000
MAX_STALE = 0


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def read_png_rgb(path):
    with open(path, "rb") as fh:
        if fh.read(8) != b"\x89PNG\r\n\x1a\n":
            raise SystemExit(f"{path} is not a PNG")
        width = height = None
        ctype = None
        idat = b""
        while True:
            header = fh.read(8)
            if len(header) < 8:
                break
            length, kind = struct.unpack(">I4s", header)
            data = fh.read(length)
            fh.read(4)
            if kind == b"IHDR":
                width, height, _bit, ctype = struct.unpack(">IIBB", data[:10])
            elif kind == b"IDAT":
                idat += data
            elif kind == b"IEND":
                break
    if width is None or ctype not in (2, 6):
        raise SystemExit(f"{path}: need 8-bit RGB or RGBA PNG")
    raw = zlib.decompress(idat)
    bpp = 3 if ctype == 2 else 4
    stride = width * bpp
    rows = []
    idx = 0
    prev = bytes(stride)
    for _ in range(height):
        filt = raw[idx]
        idx += 1
        row = bytearray(raw[idx : idx + stride])
        idx += stride
        if filt == 1:
            for x in range(stride):
                row[x] = (row[x] + (row[x - bpp] if x >= bpp else 0)) & 255
        elif filt == 2:
            for x in range(stride):
                row[x] = (row[x] + prev[x]) & 255
        elif filt == 3:
            for x in range(stride):
                left = row[x - bpp] if x >= bpp else 0
                row[x] = (row[x] + ((left + prev[x]) // 2)) & 255
        elif filt == 4:
            for x in range(stride):
                left = row[x - bpp] if x >= bpp else 0
                up_left = prev[x - bpp] if x >= bpp else 0
                row[x] = (row[x] + paeth(left, prev[x], up_left)) & 255
        elif filt != 0:
            raise SystemExit(f"{path}: unsupported PNG filter {filt}")
        prev = bytes(row)
        rows.append(bytes(row))
    return width, height, bpp, rows


def is_splash_rgb(r, g, b):
    for sr, sg, sb in SPECTRUM:
        if abs(r - sr) <= SLOP and abs(g - sg) <= SLOP and abs(b - sb) <= SLOP:
            return True
    return False


def splash_coords(path):
    width, height, bpp, rows = read_png_rgb(path)
    coords = []
    for y, row in enumerate(rows):
        for x in range(width):
            i = x * bpp
            if is_splash_rgb(row[i], row[i + 1], row[i + 2]):
                coords.append((x, y))
    return width, height, bpp, rows, coords


mode = sys.argv[1]
if mode == "count":
    _w, _h, _b, _rows, coords = splash_coords(sys.argv[2])
    print(len(coords))
    raise SystemExit(0 if len(coords) >= MIN_SPLASH else 1)
if mode == "stale":
    _w, _h, bpp, rows, coords = splash_coords(sys.argv[2])
    if len(coords) < MIN_SPLASH:
        print(f"splash frame too thin: {len(coords)} pixels", file=sys.stderr)
        raise SystemExit(1)
    leftover = 0
    for later in sys.argv[3:]:
        lw, lh, lb, lrows = read_png_rgb(later)[:4]
        if (lw, lh, lb) != (_w, _h, bpp):
            print(f"{later} size mismatch", file=sys.stderr)
            raise SystemExit(1)
        still = 0
        for x, y in coords:
            i = x * bpp
            pix = lrows[y]
            if is_splash_rgb(pix[i], pix[i + 1], pix[i + 2]):
                still += 1
        leftover = max(leftover, still)
        print(f"{later}: stale={still} of {len(coords)}")
    raise SystemExit(0 if leftover <= MAX_STALE else 1)
raise SystemExit(f"unknown mode {mode}")
PY
}

assert_splash_then_gone() {
  local dump sidecar seq0 seq1 seq2 pane ls_out
  dump="${PRISMATTYC_DUMP_PRESENT:-/tmp/pt290-present.png}"
  sidecar="$(dump_sidecar)"
  # Wait until the dump exists and the exact SPECTRUM signature is on it.
  local splash_n i
  seq0=0
  splash_n=0
  for i in $(seq 1 80); do
    if [[ -s "$dump" ]]; then
      cp -f "$dump" /tmp/pt290-opaque-splash.png
      seq0="$(read_dump_seq "$sidecar")"
      if splash_n="$(splash_pixel_cmd count /tmp/pt290-opaque-splash.png)"; then
        break
      fi
      splash_n=0
    fi
    sleep 0.1
  done
  if [[ "${splash_n:-0}" -lt 4000 ]]; then
    fail "splash pixels missing after launch (count=${splash_n:-0}, seq=$seq0)"
    return 1
  fi
  pass "splash visible before dismiss ($splash_n signature pixels, seq=$seq0)"

  # Wait until the dump is no longer a splash frame (dismiss timer).
  local gone=0
  for i in $(seq 1 80); do
    if [[ -s "$dump" ]]; then
      cp -f "$dump" /tmp/pt290-opaque-dismiss.png
      if ! splash_pixel_cmd count /tmp/pt290-opaque-dismiss.png >/tmp/pt290-gone-n 2>/dev/null; then
        gone=1
        seq1="$(read_dump_seq "$sidecar")"
        break
      fi
    fi
    sleep 0.1
  done
  if [[ "$gone" -ne 1 ]]; then
    fail "splash signature still on dump after dismiss wait (seq=$(read_dump_seq "$sidecar"))"
    return 1
  fi
  if ! capture_compositor /tmp/pt290-opaque-present1.png; then
    return 1
  fi
  log "present1 seq=$seq1 (splash gone)"

  # weston has no zwp_virtual_keyboard_manager_v1, so wtype cannot type.
  # Type through pmux send (PT-126) into the real session PTY.
  ls_out="$(pmux --socket "$SOCK" ls 2>&1 || true)"
  pane="$(printf '%s\n' "$ls_out" | awk '/^[[:space:]]*pane [0-9]+/ { print $2 }' | sort -n \
    | comm -13 /tmp/pt290-panes-before - | tail -1)"
  if [[ -z "$pane" ]]; then
    pane="$(printf '%s\n' "$ls_out" | awk '/^[[:space:]]*pane [0-9]+/ { print $2 }' | tail -1)"
  fi
  log "send-keys pane=$pane ls=$(printf '%s' "$ls_out" | tr '\n' '|')"
  if [[ -z "$pane" ]]; then
    fail "no pane id from pmux ls for send-keys: $(printf '%s' "$ls_out" | tr '\n' ' ')"
    return 1
  fi
  local p sent_ok=0
  while read -r p; do
    [[ -z "$p" ]] && continue
    if pmux --socket "$SOCK" send "$p" PT290opaque --enter --force 2>/tmp/pt290-send.err; then
      sent_ok=1
    fi
  done < <(printf '%s\n' "$ls_out" | awk '/^[[:space:]]*pane [0-9]+/ { print $2 }')
  if [[ "$sent_ok" -ne 1 ]]; then
    fail "pmux send PT290opaque failed: $(tr '\n' ' ' </tmp/pt290-send.err)"
    return 1
  fi
  if ! seq2="$(wait_dump_seq_gt "$sidecar" "$seq1")"; then
    fail "no second present after pmux send (seq stayed $seq1, presents=$(present_count "$OPAQUE_LOG"))"
    return 1
  fi
  if ! capture_compositor /tmp/pt290-opaque-present2.png; then
    return 1
  fi
  log "present2 seq=$seq2"
  if [[ "$seq2" -le "$seq1" ]]; then
    fail "present dumps are not distinct seqs: seq1=$seq1 seq2=$seq2"
    return 1
  fi
  if splash_pixel_cmd stale \
    /tmp/pt290-opaque-splash.png \
    /tmp/pt290-opaque-present1.png \
    /tmp/pt290-opaque-present2.png
  then
    pass "no splash pixels across two presents after dismiss+type (seq $seq1 then $seq2)"
    return 0
  fi
  fail "splash pixels remained after dismiss+type (PT-294 catch)"
  return 1
}

if ! command -v weston >/dev/null 2>&1; then
  fail "weston missing; install it in the box before this script"
  exit 1
fi
if ! command -v prismattyc-host >/dev/null 2>&1; then
  fail "prismattyc-host missing"
  exit 1
fi

# Demo entrypoint still starts pmux. Wait briefly if the socket is new.
for _ in $(seq 1 20); do
  [[ -S "$SOCK" ]] && break
  sleep 0.2
done

log "start weston headless socket=pt290-wayland"
start_weston
assert_native_wayland_env
write_alpha_config
log "launch host on $WAYLAND_DISPLAY"
launch_host
assert_shm_backend
assert_shm_partial_scroll

if command -v grim >/dev/null 2>&1; then
  if grim /tmp/pt290-wayland.png 2>/tmp/pt290-grim.err; then
    pass "grim captured /tmp/pt290-wayland.png"
  else
    log "grim skipped: $(tr '\n' ' ' </tmp/pt290-grim.err)"
  fi
fi

stop_host
write_opaque_config
pmux --socket "$SOCK" ls 2>/dev/null \
  | awk '/^[[:space:]]*pane [0-9]+/ { print $2 }' \
  | sort -n >/tmp/pt290-panes-before || true
log "launch opaque host (opacity 1.0, blur off) on $WAYLAND_DISPLAY"
launch_opaque_host
assert_opaque_not_shm
assert_splash_then_gone || true
stop_host

if [[ "$FAILS" -gt 0 ]]; then
  log "$PASSES passed, $FAILS failed"
  log "RESULT: FAIL"
  exit 1
fi
log "$PASSES passed, 0 failed"
log "RESULT: PASS"
exit 0
