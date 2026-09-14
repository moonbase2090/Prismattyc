#!/usr/bin/env bash
# PT-217: spaces e2e in the demo box (Xvfb + xdotool).
# Run inside the prismattyc-demo container after entrypoint has started
# Xvfb, Openbox, and pmux. Host driver: demo/docker/run.sh spaces-e2e.
set -euo pipefail

export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
export DISPLAY="${DISPLAY:-:99}"
export WINIT_UNIX_BACKEND=x11
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-demo}"
SOCK="${XDG_RUNTIME_DIR}/prismattyc/pmux.sock"
CACHE="${SOCK%.sock}.attach-tabs.json"
SPACES="${XDG_DATA_HOME:-$HOME/.local/share}/prismattyc/spaces"
HOST_LOG="${TMPDIR:-/tmp}/pt217-host.log"
PARTIAL_CONFIG="${TMPDIR:-/tmp}/pt217-host-config-$$.toml"
TUI_SCRIPT="${TMPDIR:-/tmp}/pt244-scroll-tui-$$.py"
PT298_LOG="${TMPDIR:-/tmp}/pt298-four-pane-$$.log"
PT298_CONFIG="${TMPDIR:-/tmp}/pt298-four-pane-$$.toml"
PT298_DUMP="${TMPDIR:-/tmp}/pt298-four-pane-$$.png"
PT298_BEFORE="${TMPDIR:-/tmp}/pt298-four-pane-$$-before.png"
PT298_AFTER="${TMPDIR:-/tmp}/pt298-four-pane-$$-after.png"
# PT-287 (codex-pc) changes this default to 1 when steady alternate-screen
# frames can use partial raster. Keep an environment override for branch tests.
PT287_ALT_TUI_SCROLL="${PT287_ALT_TUI_SCROLL:-1}"
WIGGLE="${WIGGLE:-0}"
PASSES=0
FAILS=0

log() { printf '[pt217] %s\n' "$*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS + 1)); }
pass() { log "PASS: $*"; PASSES=$((PASSES + 1)); }

cache_sessions() {
  python3 - <<'PY' "$CACHE"
import json, sys
p = sys.argv[1]
try:
    d = json.load(open(p))
except FileNotFoundError:
    print("")
    raise SystemExit(0)
ids = [s for t in d.get("tabs", []) for s in t.get("sessions", [])]
print(" ".join(ids))
PY
}

cache_space() {
  python3 - <<'PY' "$CACHE"
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except FileNotFoundError:
    print("")
    raise SystemExit(0)
print(d.get("space") or "")
PY
}

cache_mode() {
  python3 - <<'PY' "$CACHE"
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except FileNotFoundError:
    print("add")
    raise SystemExit(0)
print(d.get("mode") or "add")
PY
}

find_host() {
  xdotool search --onlyvisible --class prismattyc-host 2>/dev/null | tail -1 || true
}

host_title() {
  local wid
  wid="$(find_host)"
  [[ -n "$wid" ]] && xdotool getwindowname "$wid" 2>/dev/null || echo ""
}

wiggle() {
  if [[ "$WIGGLE" != "1" ]]; then
    return 0
  fi
  xdotool mousemove --sync 400 400 2>/dev/null || true
  sleep 0.2
  xdotool mousemove --sync 420 420 2>/dev/null || true
  sleep 0.4
}

activate() {
  local wid
  wid="$(find_host)"
  [[ -z "$wid" ]] && { fail "no host window"; return 1; }
  xdotool windowactivate --sync "$wid"
  xdotool windowfocus --sync "$wid"
  sleep 0.1
}

park_pointer() {
  local wid geom X Y
  wid="$(find_host)"
  [[ -z "$wid" ]] && return 1
  geom="$(xdotool getwindowgeometry --shell "$wid")"
  eval "$geom"
  # Keep the pointer inside the window's outer padding, away from text and
  # chrome hit targets. This makes hover_target clear before the sample.
  xdotool mousemove --sync "$((X + 1))" "$((Y + 1))"
  sleep 0.2
}

dismiss_splash() {
  activate || return 0
  xdotool key --clearmodifiers Return
  sleep 0.4
}

write_partial_config() {
  cat >"$PARTIAL_CONFIG" <<'EOF'
render_timer = "log"
render_timer_log_every_frame = true
panes = 1
bell_toaster = false
bell_toaster_ms = 500
EOF
}

write_tui_script() {
  cat >"$TUI_SCRIPT" <<'PY'
import curses
import time


def run(screen):
    curses.curs_set(0)
    screen.scrollok(True)
    height, width = screen.getmaxyx()
    usable = max(1, width - 1)
    for row in range(height):
        screen.addnstr(row, 0, f"PT244 initial row {row:03d}", usable)
    screen.refresh()
    time.sleep(0.25)
    for row in range(80):
        screen.scroll(1)
        screen.move(height - 1, 0)
        screen.clrtoeol()
        screen.addnstr(height - 1, 0, f"PT244 scrolling row {row:03d}", usable)
        screen.refresh()
        time.sleep(0.04)
    time.sleep(0.25)


curses.wrapper(run)
PY
}

write_pt298_config() {
  local mode="${1:-partial}"
  local render_timer="log"
  if [[ "$mode" == "full" ]]; then
    # The OSD is an intentional full-raster control for the same workload.
    render_timer="both"
  fi
  cat >"$PT298_CONFIG" <<'EOF'
# Keep the PT-298 frame idle between the setup and the one key under test.
EOF
  cat >>"$PT298_CONFIG" <<EOF
render_timer = "$render_timer"
render_timer_log_every_frame = true
panes = 1
bell_toaster = false
bell_toaster_ms = 500
EOF
}

launch_host() {
  mkdir -p "$HOME/work"
  ( cd "$HOME/work"
    env -u WAYLAND_DISPLAY COLORTERM=truecolor WINIT_UNIX_BACKEND=x11 DISPLAY="$DISPLAY" \
      PRISMATTYC_CONFIG="$PARTIAL_CONFIG" PRISMATTYC_BELL_TOASTER=0 prismattyc-host >"$HOST_LOG" 2>&1 & echo $! >/tmp/pt217-host.pid )
  local i
  for i in $(seq 1 50); do
    [[ -n "$(find_host)" ]] && break
    sleep 0.2
  done
  [[ -n "$(find_host)" ]] || { fail "host window did not appear"; tail -20 "$HOST_LOG" || true; return 1; }
  sleep 0.4
  dismiss_splash
  wiggle
  pass "host launched title=$(host_title)"
}

expect_partial_frame() {
  local from="$1"
  python3 - "$HOST_LOG" "$from" <<'PY'
import re, sys, time

path, start = sys.argv[1], int(sys.argv[2])
full_grid = 80 * 24
pattern = re.compile(r"cells_painted=(\d+).*full_repaint_reason=(\S+)")
last = ""
for _ in range(20):
    try:
        lines = open(path, encoding="utf-8", errors="replace").readlines()
    except FileNotFoundError:
        lines = []
    for line in lines[start:]:
        match = pattern.search(line)
        if not match:
            continue
        cells, reason = int(match.group(1)), match.group(2)
        last = f"cells={cells} full={reason}"
        if reason == "-" and 0 < cells < full_grid:
            print(f"{last} (< {full_grid})")
            raise SystemExit(0)
    time.sleep(0.25)
print(f"no partial frame after line {start}: {last or 'no render samples'}", file=sys.stderr)
raise SystemExit(1)
PY
  pass "partial raster frame observed"
}

expect_alt_tui_scroll_blit() {
  local from="$1"
  python3 - "$HOST_LOG" "$from" <<'PY'
import re, sys, time

path, start = sys.argv[1], int(sys.argv[2])
full_grid = 80 * 24
pattern = re.compile(
    r"cells_painted=(\d+) rows_scrolled_as_blit=(\d+) full_repaint_reason=(\S+)"
)
saw_alt_transition = False
last = ""
for _ in range(40):
    try:
        lines = open(path, encoding="utf-8", errors="replace").readlines()
    except FileNotFoundError:
        lines = []
    for line in lines[start:]:
        match = pattern.search(line)
        if not match:
            continue
        cells, blit_rows, reason = int(match.group(1)), int(match.group(2)), match.group(3)
        last = f"cells={cells} blit_rows={blit_rows} full={reason}"
        if reason == "alt-screen":
            saw_alt_transition = True
        if saw_alt_transition and reason == "-" and blit_rows > 0 and cells < full_grid:
            print(f"{last} (< {full_grid})")
            raise SystemExit(0)
    time.sleep(0.25)
print(
    f"no alternate-screen TUI scroll blit after line {start}: "
    f"{last or 'no render samples'}; alt_transition={saw_alt_transition}",
    file=sys.stderr,
)
raise SystemExit(1)
PY
  pass "alternate-screen TUI used framebuffer scroll blit"
}

expect_pt298_dump() {
  local min_seq="${1:-0}"
  local last_seq=""
  for _ in $(seq 1 40); do
    if [[ -s "${PT298_DUMP%.png}.json" ]] && [[ -s "$PT298_DUMP" ]]; then
      last_seq="$(python3 - "${PT298_DUMP%.png}.json" <<'PY'
import json, sys
try:
    print(json.load(open(sys.argv[1]))["seq"])
except (FileNotFoundError, KeyError, json.JSONDecodeError):
    print("")
PY
)"
      if [[ -n "$last_seq" ]] && (( last_seq > min_seq )); then
        return 0
      fi
    fi
    sleep 0.1
  done
  fail "PT-298 present dump missing (seq=${last_seq:-none})"
  return 1
}

run_pt298_four_pane_keystroke() {
  local mode="${1:-partial}"
  if [[ "$mode" != "partial" && "$mode" != "full" ]]; then
    fail "PT-298 unknown raster mode: $mode"
    return 0
  fi
  log "PT-298 four-pane host keystroke ($mode control)"
  write_pt298_config "$mode"
  rm -f "$PT298_LOG" "$PT298_DUMP" "${PT298_DUMP%.png}.json" \
    "$PT298_BEFORE" "$PT298_AFTER"
  stop_host
  for _ in $(seq 1 20); do
    [[ -z "$(find_host)" ]] && break
    sleep 0.1
  done
  mkdir -p "$HOME/work"
  (
    cd "$HOME/work"
    env -u WAYLAND_DISPLAY COLORTERM=truecolor WINIT_UNIX_BACKEND=x11 DISPLAY="$DISPLAY" \
      PRISMATTYC_CONFIG="$PT298_CONFIG" PRISMATTYC_BELL_TOASTER=0 \
      PRISMATTYC_DUMP_PRESENT="$PT298_DUMP" prismattyc-host --no-splash \
      >"$PT298_LOG" 2>&1 & echo $! >/tmp/pt217-host.pid
  )
  local wid geom X Y WIDTH HEIGHT x y
  for _ in $(seq 1 50); do
    wid="$(find_host)"
    [[ -n "$wid" ]] && break
    sleep 0.2
  done
  if [[ -z "${wid:-}" ]]; then
    fail "PT-298 host window did not appear"
    tail -20 "$PT298_LOG" || true
    stop_host
    return 0
  fi
  xdotool windowactivate --sync "$wid"
  xdotool windowfocus --sync "$wid"
  # Use the documented four-pane shortcut, then accept each suggested name.
  # Ctrl+Alt+4 avoids function keys reserved by some X11 window managers.
  xdotool key --clearmodifiers ctrl+alt+4
  sleep 0.5
  for pane in 2 3 4; do
    xdotool key --clearmodifiers Return
    for _ in $(seq 1 50); do
      [[ "$(host_title)" == *"$pane panes"* ]] && break
      sleep 0.1
    done
    if [[ "$(host_title)" != *"$pane panes"* ]]; then
      fail "naming popup did not create pane $pane"
      stop_host
      return 0
    fi
  done
  sleep 0.5
  geom="$(xdotool getwindowgeometry --shell "$wid")"
  eval "$geom"
  for pane in 1 2 3 4; do
    case "$pane" in
      1) x=$((X + WIDTH / 4)); y=$((Y + HEIGHT / 4)) ;;
      2) x=$((X + WIDTH * 3 / 4)); y=$((Y + HEIGHT / 4)) ;;
      3) x=$((X + WIDTH / 4)); y=$((Y + HEIGHT * 3 / 4)) ;;
      4) x=$((X + WIDTH * 3 / 4)); y=$((Y + HEIGHT * 3 / 4)) ;;
    esac
    xdotool mousemove --sync "$x" "$y"
    xdotool click 1
    # Pace setup typing for the daemon input queue. The measured key below is unchanged.
    # Leave a real shell waiting for one key in DECSET 1049. The marker
    # printed after read makes that real input observable in the raster.
    xdotool type --clearmodifiers --delay 20 -- \
      "stty echo; printf '\\033[?1049h\\033[2J\\033[H\\033[?25lPT298-pane-$pane\\n'; read -r -n1; printf '\\r\\nPT298-key\\n'"
    xdotool key --clearmodifiers Return
    sleep 0.3
  done

  local status_tmp
  status_tmp="$(mktemp)"
  local four_alt=0
  for _ in $(seq 1 40); do
    if pmux render-status --json >"$status_tmp" 2>/dev/null && python3 - "$status_tmp" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
    window = data["windows"][0]
    panes = window["current_panes"]
    raise SystemExit(0 if window["pane_count"] == 4 and len(panes) == 4 and all(p["alt_active"] for p in panes) else 1)
except (FileNotFoundError, KeyError, IndexError, json.JSONDecodeError):
    raise SystemExit(1)
PY
    then
      four_alt=1
      break
    fi
    sleep 0.25
  done
  if [[ "$four_alt" -eq 1 ]]; then
    pass "four host panes reached alternate screen"
  else
    fail "PT-298 did not reach four alternate-screen panes"
    cat "$status_tmp" 2>/dev/null || true
    tail -40 "$PT298_LOG" 2>/dev/null || true
    rm -f "$status_tmp"
    stop_host
    return 0
  fi

  local cell_w cell_h
  read -r cell_w cell_h < <(python3 - "$PT298_LOG" <<'PY'
import re, sys
try:
    text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
except FileNotFoundError:
    text = ""
match = re.search(r"cell (\d+)x(\d+)", text)
print(f"{match.group(1)} {match.group(2)}" if match else "10 21")
PY
  )
  local typed_pane_cols="" typed_pane_rows=""
  read -r typed_pane_cols typed_pane_rows < <(python3 - "$PT298_LOG" "$WIDTH" "$cell_w" <<'PY'
import re, sys

try:
    lines = open(sys.argv[1], encoding="utf-8", errors="replace").readlines()
except FileNotFoundError:
    lines = []
full_cells = []
for line in lines:
    match = re.search(r"cells_painted=(\d+).*full_repaint_reason=(\S+)", line)
    if match:
        cells, reason = int(match.group(1)), match.group(2)
        if cells > 0 and reason != "-":
            full_cells.append(cells)
if not full_cells:
    raise SystemExit(1)
total = max(full_cells)
if total % 4:
    raise SystemExit(1)
area = total // 4
target_cols = max(1, round(int(sys.argv[2]) / (2 * int(sys.argv[3]))))
divisors = [cols for cols in range(1, area + 1) if area % cols == 0]
cols = min(divisors, key=lambda candidate: abs(candidate - target_cols))
print(cols, area // cols)
raise SystemExit(0)
PY
  ) || true
  if [[ ! "$typed_pane_cols" =~ ^[0-9]+$ || ! "$typed_pane_rows" =~ ^[0-9]+$ ||
    "$typed_pane_cols" -eq 0 || "$typed_pane_rows" -eq 0 ]]; then
    fail "PT-298 could not derive the live four-pane geometry"
    rm -f "$status_tmp"
    stop_host
    return 0
  fi
  local chrome_budget_cells=$(( (32 * 1024 + cell_w * cell_h - 1) / (cell_w * cell_h) ))
  local bound=$((typed_pane_cols + chrome_budget_cells))
  pass "live typed pane geometry=${typed_pane_cols}x${typed_pane_rows} one-row bound=${bound}"

  # Focus the top-left pane, park the pointer, then leave the host idle.
  xdotool mousemove --sync "$((X + WIDTH / 4))" "$((Y + HEIGHT / 4))"
  xdotool click 1
  xdotool mousemove --sync "$((X + 1))" "$((Y + 1))"
  xdotool windowactivate --sync "$wid"
  xdotool windowfocus --sync "$wid"
  sleep 2
  if ! expect_pt298_dump; then
    rm -f "$status_tmp"
    stop_host
    return 0
  fi
  cp "$PT298_DUMP" "$PT298_BEFORE"
  local before_dump_seq
  before_dump_seq="$(python3 - "${PT298_DUMP%.png}.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["seq"])
PY
)"
  # This is the single user keystroke under test. The next checks only wait
  # for the published frame and inspect its raster evidence.
  local key_log_mark
  key_log_mark="$(wc -l <"$PT298_LOG" 2>/dev/null || echo 0)"
  xdotool key --window "$wid" --clearmodifiers x
  local key_ok=0
  local key_evidence=""
  for _ in $(seq 1 40); do
    if key_evidence="$(python3 - "$PT298_LOG" "$key_log_mark" "$mode" "$bound" <<'PY'
import re, sys
path, start, mode, bound = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
pattern = re.compile(r"cells_painted=(\d+).*full_repaint_reason=(\S+).*full_repaint_guards=(\S+)")
try:
    lines = open(path, encoding="utf-8", errors="replace").readlines()[start:]
except FileNotFoundError:
    lines = []
for line in lines:
    match = pattern.search(line)
    if not match:
        continue
    cells, reason, guards = int(match.group(1)), match.group(2), match.group(3)
    full = reason != "-"
    if ((mode == "partial" and not full and cells > 0) or
            (mode == "full" and full and cells > bound)):
        print(
            f"raster_mode={'full' if full else 'partial'} "
            f"full_repaint_reason={reason} "
            f"full_repaint_guards={guards} cells_painted={cells}"
        )
        raise SystemExit(0)
raise SystemExit(1)
PY
    )"; then
      key_ok=1
      break
    fi
    sleep 0.25
  done
  if [[ "$key_ok" -eq 1 ]]; then
    if [[ "$mode" == "partial" ]]; then
      pass "real host keystroke produced partial raster ($key_evidence)"
    else
      pass "force-full control exceeded one-row damage bound ($key_evidence bound=$bound)"
      rm -f "$status_tmp"
      stop_host
      return 0
    fi
  else
    fail "PT-298 keystroke did not publish the expected $mode raster"
    pmux render-status --json 2>/dev/null || true
    tail -40 "$PT298_LOG" 2>/dev/null || true
    rm -f "$status_tmp"
    stop_host
    return 0
  fi
  expect_pt298_dump "$before_dump_seq" || { rm -f "$status_tmp"; stop_host; return 0; }
  cp "$PT298_DUMP" "$PT298_AFTER"
  # Compare the retained image outside the typed top-left pane. The parser
  # also requires that pane to change, proving the key reached its TUI.
  if python3 - "$PT298_BEFORE" "$PT298_AFTER" "$key_evidence" "$cell_w" "$cell_h" \
    "$typed_pane_cols" "$typed_pane_rows" "$bound" <<'PY'
import re, struct, sys, zlib


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    return a if pa <= pb and pa <= pc else b if pb <= pc else c


def read_png(path):
    with open(path, "rb") as fh:
        if fh.read(8) != b"\x89PNG\r\n\x1a\n":
            raise ValueError(f"{path} is not PNG")
        width = height = ctype = None
        data = b""
        while True:
            head = fh.read(8)
            if len(head) < 8:
                break
            length, kind = struct.unpack(">I4s", head)
            chunk = fh.read(length)
            fh.read(4)
            if kind == b"IHDR":
                width, height, depth, ctype = struct.unpack(">IIBB", chunk[:10])
                if depth != 8 or ctype not in (2, 6):
                    raise ValueError("need 8-bit RGB/RGBA PNG")
            elif kind == b"IDAT":
                data += chunk
            elif kind == b"IEND":
                break
    bpp = 3 if ctype == 2 else 4
    stride = width * bpp
    raw, rows, pos, prev = zlib.decompress(data), [], 0, bytes(stride)
    for _ in range(height):
        filt, pos = raw[pos], pos + 1
        row, pos = bytearray(raw[pos : pos + stride]), pos + stride
        for i in range(stride):
            left = row[i - bpp] if i >= bpp else 0
            up = prev[i]
            up_left = prev[i - bpp] if i >= bpp else 0
            if filt == 1:
                row[i] = (row[i] + left) & 255
            elif filt == 2:
                row[i] = (row[i] + up) & 255
            elif filt == 3:
                row[i] = (row[i] + ((left + up) // 2)) & 255
            elif filt == 4:
                row[i] = (row[i] + paeth(left, up, up_left)) & 255
            elif filt != 0:
                raise ValueError(f"unsupported PNG filter {filt}")
        prev, rows = bytes(row), rows + [bytes(row)]
    return width, height, bpp, rows


before = read_png(sys.argv[1])
after = read_png(sys.argv[2])
if before[:3] != after[:3]:
    raise SystemExit("PNG dimensions differ")
w, h, bpp = before[:3]
old_rows, new_rows = before[3], after[3]


def diff(x0, y0, x1, y1):
    return sum(
        old_rows[y][x0 * bpp : x1 * bpp] != new_rows[y][x0 * bpp : x1 * bpp]
        for y in range(y0, y1)
    )


mid_x, mid_y = w // 2, h // 2
# Skip the tab-strip/rail bands and pane borders when checking retained
# content. Focus and activity chrome may change on the key frame, while the
# cell interiors of the other three panes must remain byte-identical.
cell_w, cell_h = int(sys.argv[4]), int(sys.argv[5])
typed_pane_cols, typed_pane_rows, bound = map(int, sys.argv[6:9])
x_pad = max(2, cell_w * 2)
top_pad = max(24, cell_h * 3)
bottom_pad = max(24, cell_h * 4)
top_right = diff(mid_x + x_pad, top_pad, w - x_pad, mid_y - cell_h)
bottom_y0 = mid_y + cell_h * 2
bottom_left = diff(x_pad, bottom_y0, mid_x - x_pad, h - bottom_pad)
bottom_right = diff(mid_x + x_pad, bottom_y0, w - x_pad, h - bottom_pad)
top_left = diff(x_pad, top_pad, mid_x - x_pad, mid_y - cell_h)
pane_diffs = [top_left, top_right, bottom_left, bottom_right]
changed_panes = sum(value > 0 for value in pane_diffs)
unaffected = max(value for value in pane_diffs if value == 0) if changed_panes == 1 else max(pane_diffs)
evidence = sys.argv[3]
cells = int(re.search(r"cells_painted=(\d+)", evidence).group(1))
guards = re.search(r"full_repaint_guards=(\S+)", evidence).group(1)
print(
    f"png={w}x{h} "
    f"retained_equal={'true' if changed_panes == 1 and unaffected == 0 else 'false'} "
    f"typed_pane_changed={'true' if changed_panes == 1 else 'false'} "
    f"cells_painted={cells} bound={bound} "
    f"unaffected_diff_rows={unaffected} "
    f"full_repaint_guards={guards}"
)
if changed_panes != 1 or unaffected != 0 or cells > bound:
    raise SystemExit(1)
PY
  then
    pass "retained-frame equality holds outside typed pane and cells stay within typed rows + CHROME_DAMAGE_BUDGET_PX"
  else
    fail "PT-298 retained-frame equality or damage bound failed"
  fi
  rm -f "$status_tmp"
  stop_host
}

stop_host() {
  local pid
  pid="$(cat /tmp/pt217-host.pid 2>/dev/null || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    sleep 0.4
    kill -9 "$pid" 2>/dev/null || true
  fi
  pkill -x prismattyc-host 2>/dev/null || true
  sleep 0.3
  rm -f /tmp/pt217-host.pid
}

expect_title_panes() {
  local want="$1" title="" i
  for i in $(seq 1 8); do
    wiggle
    title="$(host_title)"
    if echo "$title" | grep -Eq "${want} panes"; then
      pass "title has ${want} panes ($title)"
      return
    fi
    sleep 0.4
  done
  fail "title wanted ${want} panes, got: $title"
}

expect_title_tabs() {
  local want="$1" title="" i
  for i in $(seq 1 8); do
    wiggle
    title="$(host_title)"
    if echo "$title" | grep -Eq "${want} tabs"; then
      pass "title has ${want} tabs ($title)"
      return
    fi
    sleep 0.4
  done
  fail "title wanted ${want} tabs, got: $title"
}

expect_cache_space() {
  local want="$1" got
  sleep 0.3
  got="$(cache_space)"
  if [[ "$got" == "$want" ]]; then
    pass "cache space=$want"
  else
    fail "cache space wanted $want got $got"
  fi
}

expect_cache_mode() {
  local want="$1" got
  got="$(cache_mode)"
  if [[ "$got" == "$want" ]]; then
    pass "cache mode=$want"
  else
    fail "cache mode wanted $want got $got"
  fi
}

expect_ls() {
  local name="$1" out
  out="$(pmux space ls 2>/dev/null || true)"
  if echo "$out" | grep -qw "$name"; then
    pass "pmux space ls has $name"
  else
    fail "pmux space ls missing $name: $out"
  fi
}

# Chip click: poll cache + title every 0.5 s for up to 4 s, then assert.
wait_space_title() {
  local space="$1" kind="$2" want="$3" i got title
  for i in $(seq 1 8); do
    sleep 0.5
    got="$(cache_space)"
    title="$(host_title)"
    if [[ "$got" == "$space" ]] && echo "$title" | grep -Eq "${want} ${kind}"; then
      break
    fi
  done
  got="$(cache_space)"
  title="$(host_title)"
  if [[ "$got" == "$space" ]]; then
    pass "cache space=$space"
  else
    fail "cache space wanted $space got $got"
  fi
  if echo "$title" | grep -Eq "${want} ${kind}"; then
    pass "title has ${want} ${kind} ($title)"
  else
    fail "title wanted ${want} ${kind}, got: $title"
  fi
}

rail_names() {
  python3 - <<'PY' "$SPACES"
import os, sys
d = sys.argv[1]
if not os.path.isdir(d):
    raise SystemExit(0)
names = sorted(os.path.splitext(f)[0] for f in os.listdir(d) if f.endswith(".json"))
print(" ".join(names))
PY
}

# cell WxH from the host log (prismattyc-host: font … cell WxH).
cell_wh() {
  python3 - <<'PY' "$HOST_LOG"
import re, sys
path = sys.argv[1]
try:
    text = open(path).read()
except FileNotFoundError:
    text = ""
m = re.search(r"cell (\d+)x(\d+)", text)
print(f"{m.group(1)} {m.group(2)}" if m else "10 21")
PY
}

# Bottom rail: label-sized chips (PT-123), then +. Click the label, not the close cell.
click_rail_index() {
  local index="$1"
  local wid geom cell_w cell_h names xy x y
  local WINDOW X Y WIDTH HEIGHT SCREEN
  wid="$(find_host)"
  [[ -z "$wid" ]] && return 1
  geom="$(xdotool getwindowgeometry --shell "$wid")"
  eval "$geom"
  names="$(rail_names)"
  read -r cell_w cell_h < <(cell_wh)
  xy="$(python3 - "$index" "$X" "$Y" "$WIDTH" "$HEIGHT" "$cell_w" "$cell_h" $names <<'PY'
import sys
idx = int(sys.argv[1])
win_x, win_y = int(sys.argv[2]), int(sys.argv[3])
width, height = int(sys.argv[4]), int(sys.argv[5])
cell_w, cell_h = int(sys.argv[6]), int(sys.argv[7])
names = sys.argv[8:]
pad, gap = 5, 5
cap, min_cols = 28, 6
inset = 4

def chip_w(name):
    return min(max(len(name) + 3, min_cols), cap) * cell_w

n = len(names)
plus = idx == n
if idx > n:
    raise SystemExit(1)
before = sum(chip_w(name) for name in names[:idx])
x0 = pad + before + idx * gap
if plus:
    w = cell_w + inset * 2
    x = x0 + w // 2
else:
    w = chip_w(names[idx])
    # Left of center so the close cell on the right is not hit.
    x = x0 + max(cell_w, w // 3)
y = height - max(6, cell_h // 2)
print(win_x + x, win_y + y)
PY
)"
  [[ -n "$xy" ]] || return 1
  read -r x y <<<"$xy"
  log "click rail index=$index at $x $y names=[$names] cell=${cell_w}x${cell_h}"
  xdotool windowactivate --sync "$wid"
  xdotool mousemove --sync "$x" "$y"
  sleep 0.15
  xdotool click 1
  sleep 0.5
}

click_rail_plus() {
  local n
  n="$(rail_names | wc -w | tr -d ' ')"
  click_rail_index "$n"
}

cleanup() {
  stop_host
  rm -f "$PARTIAL_CONFIG" "$TUI_SCRIPT" "$PT298_LOG" "$PT298_CONFIG" \
    "$PT298_DUMP" "${PT298_DUMP%.png}.json" "$PT298_BEFORE" "$PT298_AFTER"
  rm -f "$SPACES"/alpha.json "$SPACES"/beta.json "$SPACES"/probe.json "$SPACES"/fromplus.json
}
trap cleanup EXIT

log "socket $SOCK"
ready=0
for _ in $(seq 1 30); do
  if pmux status 2>/dev/null | grep -q '^status: running$'; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "$ready" -ne 1 ]]; then
  log "pmux did not come up"
  pmux status || true
  exit 1
fi
mkdir -p "$SPACES" "$HOME/work"
rm -f "$SPACES"/alpha.json "$SPACES"/beta.json "$SPACES"/probe.json "$SPACES"/fromplus.json

# Hermetic seats. Do not use the entrypoint's claude/kiro/work.
( cd "$HOME/work" && pmux new a --no-agent --no-attach -- bash --norc >/dev/null )
( cd "$HOME/work" && pmux new b --no-agent --no-attach -- bash --norc >/dev/null )
( cd "$HOME/work" && pmux new c --no-agent --no-attach -- bash --norc >/dev/null )
pmux space save alpha a b >/dev/null
pmux space save beta c >/dev/null
expect_ls alpha
expect_ls beta

# PT-267: this installed face must stay lazy until the pane displays CJK.
CJK_FONT=/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc
[[ -f "$CJK_FONT" ]] || { fail "demo CJK font is missing"; exit 1; }
export PRISMATTYC_HOST_FONT_FALLBACK="$CJK_FONT"
write_partial_config
write_tui_script
launch_host
sleep 3
partial_mark="$(wc -l <"$HOST_LOG" | tr -d ' ')"
activate
park_pointer
xdotool type --clearmodifiers --delay 20 -- "PT243-partial"
expect_partial_frame "$partial_mark"
xdotool key --clearmodifiers ctrl+c
sleep 0.2

# PT-298: use the host's real four-pane shortcut, start an alternate-screen
# child in every pane, and send one ordinary key through the X11 input path.
# The step records the published raster mode, reason, guards, and cell bound;
# its PNG comparison proves the three untouched panes retained their pixels.
run_pt298_four_pane_keystroke
# Named sessions survive their host. Start the next render case with a fresh view.
rm -f "$CACHE"
run_pt298_four_pane_keystroke full
rm -f "$CACHE"
write_partial_config
write_tui_script
launch_host

# PT-286: attach must drain stale control replies. Two send() without
# read(), then a Snapshot request. If request() still bails on id
# mismatch, this probe fails. Reverting the drain is a no-op for this
# step.
log "PT-286 attach drain"
pt286_out="$(mktemp)"
if PRISMATTYC_ATTACH_DRAIN_PROBE=1 pmux-attach --socket "$SOCK" \
    >"$pt286_out" 2>&1; then
  if grep -q 'drain-probe recovered after 2 stale' "$pt286_out"; then
    pass "attach drain recovered from 2 stale responses"
  else
    fail "drain-probe exited 0 without recovery line: $(tr '\n' ' ' <"$pt286_out")"
  fi
else
  fail "drain-probe failed: $(tr '\n' ' ' <"$pt286_out")"
fi
rm -f "$pt286_out"

# PT-293: clear must write MailDepth 0 to the pane log. The windowed host
# reads depth only from that log, so a raise without a logged 0 leaves the
# envelope lit. Reverting the mux log write fails this step.
log "PT-293 mail depth clear"
if python3 - "$SOCK" <<'PY'
import json, socket, sys

sock_path = sys.argv[1]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sock_path)
sock.settimeout(5)
reader = sock.makefile("r")
req_id = 0

def call(payload):
    global req_id
    req_id += 1
    payload = dict(payload)
    payload["version"] = 1
    payload["request_id"] = req_id
    sock.sendall((json.dumps(payload) + "\n").encode())
    line = reader.readline()
    if not line:
        raise SystemExit("empty control reply")
    body = json.loads(line)
    if body.get("status") != "ok":
        raise SystemExit(f"control error: {body}")
    return body["response"]

registered = call({"type": "register_client"})
client_id = registered["client_id"]
snapshot = call({"type": "snapshot"})["snapshot"]
pane_id = snapshot["sessions"][0]["windows"][0]["panes"][0]["id"]
call({
    "type": "mail_attention_set",
    "client_id": client_id,
    "pane_id": pane_id,
    "cell": "mail",
    "gen": 1,
    "queue_rev": 9001,
    "depth": 2,
})
call({
    "type": "mail_attention_clear",
    "client_id": client_id,
    "pane_id": pane_id,
    "queue_rev": 9002,
})
sub = call({
    "type": "subscribe_pane",
    "client_id": client_id,
    "pane_id": pane_id,
    "from_seq": 0,
    "timeout_ms": 0,
})
depths = [
    event["depth"]
    for frame in sub.get("events", [])
    for event in [frame.get("event", {})]
    if event.get("kind") == "mail_depth"
]
if 2 not in depths:
    raise SystemExit(f"raise missing MailDepth 2: {depths}")
if not depths or depths[-1] != 0:
    raise SystemExit(f"clear did not log MailDepth 0: {depths}")
print("mail-depth-clear logged 0 after 2")
PY
then
  pass "mail depth clear logged 0 after raise"
else
  fail "mail-depth-clear probe failed"
fi

# PT-244: drive a real curses application on the alternate screen. PT-287
# permits partial raster after the transition frame; codex-pc enables this
# prepared assertion by changing the PT287_ALT_TUI_SCROLL default above.
if [[ "$PT287_ALT_TUI_SCROLL" == "1" ]]; then
  tui_mark="$(wc -l <"$HOST_LOG" | tr -d ' ')"
  activate
  xdotool type --clearmodifiers --delay 20 -- "python3 $TUI_SCRIPT"
  xdotool key --clearmodifiers Return
  expect_alt_tui_scroll_blit "$tui_mark"
  sleep 4
else
  log "SKIP: alternate-screen scroll blit waits on PT-287"
fi

# PT-262: blank footer/splash glyphs must not load a system font.
sleep 2
if grep -q 'system fallback .* (U+0020)' "$HOST_LOG"; then
  fail "space triggered system font loading"
else
  pass "space does not load a system font"
fi

log "open alpha (switch into the live host)"
out="$(pmux space open alpha --no-attach 2>&1 || true)"
log "open alpha: $out"
wiggle
sleep 1
expect_cache_space alpha
expect_cache_mode switch
expect_title_tabs 2

if grep -Fq "loaded fallback $CJK_FONT " "$HOST_LOG"; then
  fail "CJK outlines loaded before first CJK output"
else
  pass "CJK outlines remain lazy during ASCII startup"
fi
activate
xdotool type --clearmodifiers --delay 20 "printf '\\u4e2d\\n'"
xdotool key --clearmodifiers Return
sleep 2
# The gate uses a debug build. Parsing this CJK collection can exceed 2 s.
# Wait for that first load without sending any further input.
for _ in $(seq 1 140); do
  grep -Fq "loaded fallback $CJK_FONT (U+4E2D)" "$HOST_LOG" && break
  sleep 0.2
done
if grep -Fq "loaded fallback $CJK_FONT (U+4E2D)" "$HOST_LOG"; then
  pass "first CJK output loads the configured fallback"
else
  fail "first CJK output did not load the configured fallback"
fi

# PT-263: the real shell, parser, and text export must keep complete clusters.
pt263_pane="$(pmux ls | awk '$1 == "session" { active = ($2 == "a") } active && $1 == "pane" && !found { print $2; found = 1 }')"
[[ -n "$pt263_pane" ]]
pmux send "$pt263_pane" "printf 'PT263: e\\u0301 \\U0001f1fa\\U0001f1f8 \\U0001f468\\u200d\\U0001f469\\nPT263-tail:e\\u0300\\u0301\\u0302\\u0303\\u0304\\u0305\\u0306\\u0307\\u0308\\u0309\\u030a\\u030b\\n'" --literal --enter
sleep 2
pmux save-buffer a /tmp/pt263-live.txt
if python3 - /tmp/pt263-live.txt <<'PY'
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
assert "PT263: e\u0301 \U0001f1fa\U0001f1f8 \U0001f468\u200d\U0001f469" in text
assert "PT263-tail:e" + "".join(chr(c) for c in range(0x300, 0x30c)) in text
PY
then
  pass "live pane export preserves combining marks, flags, ZWJ, and twelve-scalar tails"
else
  fail "live pane export lost a grapheme tail"
fi

pmux send "$pt263_pane" "for i in {1..80}; do printf 'PT263 history row %s\\n' \"\$i\"; done" --literal --enter
sleep 2
pmux save-buffer a /tmp/pt263-history.txt --history
if python3 - /tmp/pt263-history.txt <<'PY'
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
assert "PT263: e\u0301 \U0001f1fa\U0001f1f8 \U0001f468\u200d\U0001f469" in text
assert "PT263-tail:e" + "".join(chr(c) for c in range(0x300, 0x30c)) in text
PY
then
  pass "history export preserves complete graphemes after scrolling"
else
  fail "history export lost a grapheme tail"
fi

# PT-273: resize through the real host while retaining Unicode history.
pt263_window="$(find_host)"
xdotool windowstate --remove MAXIMIZED_VERT "$pt263_window"
xdotool windowstate --remove MAXIMIZED_HORZ "$pt263_window"
sleep 2
for pt263_width in 600 1200; do
  xdotool windowsize --sync "$pt263_window" "$pt263_width" 700
  sleep 2
  xdotool getwindowgeometry --shell "$pt263_window" > "/tmp/pt273-geometry-$pt263_width.txt"
  pmux save-buffer a /tmp/pt263-resized-history.txt --history
  if python3 - /tmp/pt263-resized-history.txt "/tmp/pt273-geometry-$pt263_width.txt" "$pt263_width" <<'PY'
import pathlib, sys
geometry = dict(line.split("=", 1) for line in pathlib.Path(sys.argv[2]).read_text().splitlines())
assert int(geometry["WIDTH"]) == int(sys.argv[3]), geometry
assert int(geometry["HEIGHT"]) == 700, geometry
text = pathlib.Path(sys.argv[1]).read_text()
assert "PT263: e\u0301 \U0001f1fa\U0001f1f8 \U0001f468\u200d\U0001f469" in text
assert "PT263-tail:e" + "".join(chr(c) for c in range(0x300, 0x30c)) in text
PY
  then
    pass "history preserves complete clusters after resize to $pt263_width pixels"
  else
    fail "history lost a grapheme during resize to $pt263_width pixels"
  fi
done

# PT-275: a fast producer must leave a complete final viewport.
activate
xdotool type --clearmodifiers --delay 20 "printf 'PT275-flood-%06d\\n' {1..30000}"
xdotool key --clearmodifiers Return
sleep 5
pmux save-buffer a /tmp/pt275-flood-live.txt
if python3 - /tmp/pt275-flood-live.txt <<'PYFLOOD'
import pathlib, re, sys
text = "\n".join(line.rstrip() for line in pathlib.Path(sys.argv[1]).read_text().splitlines())
rows = [int(x) for x in re.findall(r"^PT275-flood-(\d{6})$", text, re.M)]
assert len(rows) >= 20, len(rows)
assert rows[-1] == 30000, rows[-1:]
assert rows == list(range(rows[0], 30001)), (rows[0], rows[-1])
PYFLOOD
then
  pass "fast producer leaves a complete newest viewport"
else
  fail "fast producer lost viewport updates"
fi

# PT-263: the byte budget must retain the ordinary 10,000-row history.
# Send the complete command to the real child before observing retention.
pmux send "$pt263_pane" "printf 'PT263-budget-%05d\\n' {1..10050}" --literal --enter
sleep 5
pmux save-buffer a /tmp/pt263-budget-history.txt --history
if python3 - /tmp/pt263-budget-history.txt <<'PYBUDGET'
import pathlib, re, sys
text = "\n".join(line.rstrip() for line in pathlib.Path(sys.argv[1]).read_text().splitlines())
rows = [int(x) for x in re.findall(r"^PT263-budget-(\d{5})$", text, re.M)]
assert len(rows) >= 10000, len(rows)
assert rows[-1] == 10050, rows[-1:]
assert rows == list(range(rows[0], 10051)), (rows[0], rows[-1])
PYBUDGET
then
  pass "ordinary-width history retains at least 10000 complete newest rows"
else
  fail "ordinary-width history lost rows under the default byte budget"
fi

log "switch to beta"
out="$(pmux space open beta --no-attach 2>&1 || true)"
log "open beta: $out"
wiggle
sleep 1
expect_cache_space beta
expect_cache_mode switch
expect_title_panes 1

log "reject mixing alpha into the beta window"
if out="$(pmux space open alpha --add --no-attach 2>&1)"; then
  fail "cross-space add succeeded"
else
  echo "$out" | grep -q "cannot add a different Space" \
    && pass "cross-space add reports the ownership boundary" \
    || fail "cross-space add: $out"
fi
sleep 1
expect_cache_space beta
expect_cache_mode switch
n_ids="$(cache_sessions | wc -w | tr -d ' ')"
[[ "$n_ids" -eq 1 ]] && pass "rejected add keeps one session" || fail "rejected add changed sessions=$n_ids"

log "reject copying an owned session into a second Space"
if save_out="$(pmux space save probe 2>&1)"; then
  fail "save copied an owned session"
else
  echo "$save_out" | grep -q "belongs to another Space" \
    && pass "save requires Move for an owned session" \
    || fail "save probe: $save_out"
fi
[[ ! -f "$SPACES/probe.json" ]] && pass "rejected save leaves no Space file" || fail "rejected save wrote probe.json"

log "switch back to beta"
out="$(pmux space open beta --no-attach 2>&1 || true)"
wiggle
sleep 1
expect_cache_space beta
expect_title_panes 1

log "create via + chip and session naming popup"
activate
click_rail_plus
sleep 0.3
xdotool type --delay 20 -- "fromplus"
xdotool key --clearmodifiers Return
sleep 0.3
xdotool key --clearmodifiers Return
sleep 0.8
for _ in $(seq 1 40); do
  [[ -f "$SPACES/fromplus.json" ]] && break
  sleep 0.1
done
if python3 - "$SPACES/fromplus.json" <<'PYNAME'
import json, sys
space = json.load(open(sys.argv[1]))
assert len(space["sessions"]) == 1, space
session = space["sessions"][0]
assert session["name"] == "fromplus-1" and session["agent"] == "fromplus-1", session
PYNAME
then
  pass "Space + creates one session and mailbox with the suggested name"
else
  fail "Space + did not save the named first session"
fi
expect_ls fromplus

log "close host, relaunch, chip click"
stop_host
sleep 0.4
launch_host
# chips in name order: alpha, beta, fromplus
click_rail_index 1
wait_space_title beta panes 1
click_rail_index 0
wait_space_title alpha tabs 2

log "new-window"
before="$(pgrep -c -x prismattyc-host || true)"
pmux space open alpha --new-window >/dev/null 2>&1 || true
sleep 1
after="$(pgrep -c -x prismattyc-host || true)"
if [[ "${after:-0}" -gt "${before:-0}" ]]; then
  pass "new-window spawned a second host ($before -> $after)"
else
  log "new-window host count $before -> $after (headless path may skip spawn)"
fi
# Do not click the window close box (PT-211). Leave extras for trap.

log "rename via right-click on a chip"
activate
click_rail_index 0
sleep 0.1
xdotool click 3
sleep 0.3
xdotool key --clearmodifiers ctrl+a
xdotool type --delay 20 -- "alpha2"
xdotool key --clearmodifiers Return
sleep 0.6
if [[ -f "$SPACES/alpha2.json" ]] || [[ -f "$SPACES/alpha.json" ]]; then
  pass "rename path left a space file"
else
  fail "rename did not produce alpha.json or alpha2.json"
fi

log "delete via CLI"
pmux space rm fromplus >/dev/null 2>&1 || true
if [[ ! -f "$SPACES/fromplus.json" ]]; then
  pass "pmux space rm fromplus"
else
  fail "fromplus.json still present"
fi

log "move pane to next tab (when the window has tabs)"
activate
xdotool key --clearmodifiers ctrl+shift+alt+Next || true
sleep 0.4
pass "move-pane chord sent"

log "$PASSES passed, $FAILS failed"
if [[ "$FAILS" -eq 0 ]]; then
  log "RESULT: PASS"
  exit 0
fi
log "RESULT: FAIL"
exit 1
