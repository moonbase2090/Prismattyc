#!/usr/bin/env bash
# PT-246: render bench in the native test container (Xvfb + xdotool).
# Host driver: tests/native/docker/run.sh render-bench
#
# Eight experiments. The script records per-frame cells_max/mean and
# blit_sum from `render_timer = "log"` plus
# `render_timer_log_every_frame = true`. Foot, when present, is the
# wall-clock reference at the same font size.
set -euo pipefail

export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
export DISPLAY="${DISPLAY:-:99}"
export WINIT_UNIX_BACKEND=x11
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-tester}"
SOCK="${XDG_RUNTIME_DIR}/prismattyc/pmux.sock"
export PMUX_SOCKET="$SOCK"
HOST_LOG="${TMPDIR:-/tmp}/pt246-host.log"
HOST_LOG2="${TMPDIR:-/tmp}/pt246-host2.log"
# `prismattyc-host 0.1.242 (hash)` — field 2 is the package version.
VERSION="$(prismattyc-host --version 2>/dev/null | awk '{print $2; exit}' || echo unknown)"
OUTDIR="${RENDER_BENCH_OUT:-$HOME/build/render-bench/$VERSION}"
FONT_PX="${RENDER_BENCH_FONT_PX:-16}"
FOOT_FONT="JetBrainsMono Nerd Font:size=${FONT_PX}"
# Opt-in PT-240 per-frame thresholds. Off in CI until PT-243/244 land.
TARGETS="${RENDER_BENCH_TARGETS:-0}"
PROFILE="${RENDER_BENCH_PROFILE:-unknown}"
BINS_NOTE="${RENDER_BENCH_BINS:-}"
PASSES=0
FAILS=0

log() { printf '[pt246] %s\n' "$*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS + 1)); }
pass() { log "PASS: $*"; PASSES=$((PASSES + 1)); }

find_host() {
  xdotool search --onlyvisible --class prismattyc-host 2>/dev/null | tail -1 || true
}

activate() {
  local wid
  wid="$(find_host)"
  [[ -z "$wid" ]] && return 1
  xdotool windowactivate --sync "$wid"
  xdotool windowfocus --sync "$wid"
  sleep 0.1
}

dismiss_splash() {
  activate || return 0
  xdotool key --clearmodifiers Return
  sleep 0.4
}

write_bench_config() {
  mkdir -p "$HOME/.config/prismattyc"
  cat >"$HOME/.config/prismattyc/config.toml" <<EOF
theme = "prismattyc-default"
render_timer = "log"
render_timer_log_every_frame = true
font_px = ${FONT_PX}.0
panes = 1
tab_strip = "always"
EOF
}

launch_host() {
  local logf="${1:-$HOST_LOG}"
  mkdir -p "$HOME/work"
  : >"$logf"
  ( cd "$HOME/work"
    env -u WAYLAND_DISPLAY COLORTERM=truecolor WINIT_UNIX_BACKEND=x11 DISPLAY="$DISPLAY" \
      prismattyc-host >"$logf" 2>&1 & echo $! >/tmp/pt246-host.pid )
  local i
  for i in $(seq 1 50); do
    [[ -n "$(find_host)" ]] && break
    sleep 0.2
  done
  if [[ -z "$(find_host)" ]]; then
    fail "host window did not appear"
    tail -20 "$logf" || true
    return 1
  fi
  sleep 0.4
  dismiss_splash
  pass "host launched"
}

stop_host() {
  local pid
  pid="$(cat /tmp/pt246-host.pid 2>/dev/null || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    sleep 0.4
    kill -9 "$pid" 2>/dev/null || true
  fi
  pkill -x prismattyc-host 2>/dev/null || true
  sleep 0.3
  rm -f /tmp/pt246-host.pid
}

log_mark() {
  wc -l <"$HOST_LOG" | tr -d ' '
}

# Parse render_timer=log lines after a starting line count.
# Per-frame cells (max/mean) and blit sum — PT-240 targets are per frame.
parse_metrics() {
  local from="${1:-0}"
  python3 - "$HOST_LOG" "$from" <<'PY'
import collections, re, sys
path, start = sys.argv[1], int(sys.argv[2])
pat = re.compile(
    r"prismattyc-host: render parse=(\d+)us damage=(\d+)us raster=(\d+)us "
    r"present=(\d+)us cells_painted=(\d+) rows_scrolled_as_blit=(\d+) "
    r"full_repaint_reason=(\S+)"
)
rows = []
with open(path, encoding="utf-8", errors="replace") as fh:
    for i, line in enumerate(fh, start=1):
        if i <= start:
            continue
        m = pat.search(line)
        if not m:
            continue
        rows.append({
            "parse_us": int(m.group(1)),
            "damage_us": int(m.group(2)),
            "raster_us": int(m.group(3)),
            "present_us": int(m.group(4)),
            "cells_painted": int(m.group(5)),
            "rows_scrolled_as_blit": int(m.group(6)),
            "full_repaint_reason": m.group(7),
        })
if not rows:
    print("frames=0")
    raise SystemExit(0)
n = len(rows)
cells = [r["cells_painted"] for r in rows]
raster = [r["raster_us"] for r in rows]
full = collections.Counter(r["full_repaint_reason"] for r in rows).most_common(1)[0][0]
print(
    "frames={} cells_max={} cells_mean={} blit_sum={} "
    "raster_max={} raster_mean={} parse_us={} present_us={} full={}".format(
        n,
        max(cells),
        sum(cells) // n,
        sum(r["rows_scrolled_as_blit"] for r in rows),
        max(raster),
        sum(raster) // n,
        sum(r["parse_us"] for r in rows) // n,
        sum(r["present_us"] for r in rows) // n,
        full,
    )
)
PY
}

# PT-240 per-frame bars. Skip unless RENDER_BENCH_TARGETS=1.
target_verdict() {
  python3 - "$1" "$2" "$TARGETS" <<'PY'
import sys
name, metrics, enabled = sys.argv[1], sys.argv[2], sys.argv[3]
fields = {}
for item in metrics.split():
    if "=" in item:
        k, v = item.split("=", 1)
        fields[k] = int(v) if v.lstrip("-").isdigit() else v
rules = {
    "single_key": [("cells_max", "<=", 2)],
    "cursor_blink": [("cells_max", "<=", 1)],
    "one_line_scroll": [("blit_sum", ">=", 1)],
    "vtebench_scrolling": [("blit_sum", ">=", 1)],
}
wanted = rules.get(name)
if not wanted:
    print("skip")
    raise SystemExit(0)
if enabled not in ("1", "true", "yes"):
    print("skip")
    raise SystemExit(0)
if int(fields.get("frames") or 0) == 0:
    if name == "cursor_blink":
        print("pass")
        raise SystemExit(0)
    print("fail:frames=0")
    raise SystemExit(0)
failed = []
for key, op, bound in wanted:
    val = fields.get(key)
    if val is None:
        failed.append("%s=missing" % key)
        continue
    ok = val <= bound if op == "<=" else val >= bound
    if not ok:
        failed.append("%s=%s%s%s" % (key, val, op, bound))
print("fail:" + ",".join(failed) if failed else "pass")
PY
}

record() {
  local name="$1" metrics="$2" extra="${3:-}"
  local verdict
  verdict="$(target_verdict "$name" "$metrics")"
  printf '%s\t%s\t%s\t%s\n' "$name" "$metrics" "$extra" "$verdict" >>"$OUTDIR/summary.tsv"
  log "$name  $metrics  $extra  $verdict"
  if [[ "$metrics" == frames=0* ]]; then
    # Idle cursor blink may paint no frames. That is a pass.
    if [[ "$name" == cursor_blink ]]; then
      pass "$name idle (frames=0)"
    else
      fail "$name produced no render_timer log lines"
    fi
  else
    pass "$name logged render_timer"
  fi
  if [[ "$TARGETS" == 1 || "$TARGETS" == true || "$TARGETS" == yes ]]; then
    if [[ "$verdict" == fail:* ]]; then
      fail "$name target $verdict"
    fi
  fi
}

# `pmux send` takes a pane id, not a session name.
work_pane_id() {
  pmux --socket "$SOCK" ls 2>/dev/null | python3 -c '
import sys
in_work = False
for line in sys.stdin:
    if line.startswith("session work "):
        in_work = True
        continue
    if line.startswith("session "):
        in_work = False
        continue
    stripped = line.strip()
    if in_work and stripped.startswith("pane "):
        parts = stripped.split()
        if len(parts) >= 2:
            print(parts[1])
            break
'
}

# Type into the focused host window so the visible pane paints.
host_type() {
  activate || return 1
  xdotool type --clearmodifiers --delay 1 -- "$1"
  xdotool key --clearmodifiers Return
  sleep 0.15
}

send_cmd() {
  shift
  host_type "$*"
}

# Write bytes to the visible pane by having the shell cat a temp file
# (raw CSI on the command line would be eaten by readline).
send_bytes() {
  local payload="$2"
  printf '%s' "$payload" >/tmp/pt246.payload
  host_type "cat /tmp/pt246.payload"
}

wait_samples() {
  local from="$1"
  local i metrics="frames=0"
  for i in $(seq 1 12); do
    sleep 0.25
    metrics="$(parse_metrics "$from")"
    if [[ "$metrics" != frames=0* ]]; then
      printf '%s' "$metrics"
      return 0
    fi
  done
  printf '%s' "$metrics"
}

payload_cursor_motion() {
  python3 - <<'PY'
print("\x1b[H", end="")
for i in range(400):
    r = 1 + (i % 20)
    c = 1 + (i % 70)
    print(f"\x1b[{r};{c}H*", end="")
print("\x1b[H", end="")
PY
}

payload_light_cells() {
  python3 - <<'PY'
print("\x1b[2J\x1b[H", end="")
for row in range(1, 21):
    print(f"\x1b[{row};1H" + ("." * 8), end="")
print()
PY
}

payload_dense_cells() {
  python3 - <<'PY'
print("\x1b[2J\x1b[H", end="")
block = "".join(f"\x1b[38;5;{n}m#" for n in range(16, 48))
for _ in range(24):
    print(block[:80])
print("\x1b[0m", end="")
PY
}

payload_scrolling() {
  python3 - <<'PY'
print("\x1b[2J\x1b[H", end="")
for i in range(200):
    print(f"scroll-line-{i:04d}-" + ("x" * 40))
PY
}

run_foot_ref() {
  local name="$1" payload="$2"
  if ! command -v foot >/dev/null 2>&1; then
    echo "foot=missing"
    return 0
  fi
  local t0 t1
  t0="$(date +%s%N)"
  timeout 3s foot -o "main.font=${FOOT_FONT}" \
    -e /bin/sh -c "printf '%s' \"\$1\"; sleep 0.4" sh "$payload" \
    >/dev/null 2>&1 || true
  t1="$(date +%s%N)"
  python3 - "$t0" "$t1" <<'PY'
import sys
t0, t1 = int(sys.argv[1]), int(sys.argv[2])
print("foot_ms={}".format(max(0, (t1 - t0) // 1_000_000)))
PY
}

mkdir -p "$OUTDIR" "$HOME/work"
write_bench_config
: >"$OUTDIR/summary.tsv"
printf 'experiment\tmetrics\tref\ttargets\n' >"$OUTDIR/summary.tsv"

log "version=$VERSION out=$OUTDIR font_px=$FONT_PX foot_font=$FOOT_FONT profile=$PROFILE targets=$TARGETS bins=$BINS_NOTE"

# One painted pane: drop agent seats so the host shows `work`.
pmux --socket "$SOCK" stop claude >/dev/null 2>&1 || true
pmux --socket "$SOCK" stop kiro >/dev/null 2>&1 || true
if ! pmux --socket "$SOCK" ls 2>/dev/null | grep -q '^session work '; then
  ( cd "$HOME/work" && pmux new work --no-attach --no-agent -- bash -l >/dev/null 2>&1 ) || true
fi
sleep 0.3

launch_host || { echo "host failed"; exit 1; }
trap 'stop_host' EXIT
PANE="$(work_pane_id)"
if [[ -z "$PANE" ]]; then
  fail "no work pane id"
  pmux --socket "$SOCK" ls || true
  exit 1
fi
log "work pane=$PANE"

# 1. single key
mark="$(log_mark)"
activate || true
send_bytes "$PANE" "x" || fail "single_key send"
record "single_key" "$(wait_samples "$mark")"

# 2. cursor blink (idle). frames=0 is PASS.
mark="$(log_mark)"
sleep 2.2
record "cursor_blink" "$(parse_metrics "$mark")"

# 3. one-line scroll: fill, then one newline
mark="$(log_mark)"
send_bytes "$PANE" "$(python3 -c 'print("\n"*40, end="")')" || fail "one_line_scroll send"
send_bytes "$PANE" $'\n' || true
record "one_line_scroll" "$(wait_samples "$mark")"

# 4. PTY flood
mark="$(log_mark)"
send_bytes "$PANE" "$(python3 -c 'print("\n".join(f"flood-{i:04d}" for i in range(120)))')" \
  || fail "pty_flood send"
record "pty_flood" "$(wait_samples "$mark")"

# 5. full grid
mark="$(log_mark)"
send_bytes "$PANE" "$(python3 -c 'print("\x1b[2J\x1b[H" + ("W"*80 + "\n")*24, end="")')" \
  || fail "full_grid send"
record "full_grid" "$(wait_samples "$mark")"

# 6. second window
mark="$(log_mark)"
before="$(pgrep -c -x prismattyc-host || true)"
( env -u WAYLAND_DISPLAY COLORTERM=truecolor WINIT_UNIX_BACKEND=x11 DISPLAY="$DISPLAY" \
    prismattyc-host >"$HOST_LOG2" 2>&1 & echo $! >/tmp/pt246-host2.pid )
sleep 1.2
after="$(pgrep -c -x prismattyc-host || true)"
pid2="$(cat /tmp/pt246-host2.pid 2>/dev/null || true)"
if [[ -n "$pid2" ]]; then
  kill "$pid2" 2>/dev/null || true
  sleep 0.3
  kill -9 "$pid2" 2>/dev/null || true
fi
rm -f /tmp/pt246-host2.pid
record "second_window" "$(parse_metrics "$mark")" "hosts ${before:-0}->${after:-0}"
if [[ "${after:-0}" -gt "${before:-0}" ]]; then
  pass "second host process appeared"
else
  fail "second host did not spawn ($before -> $after)"
fi

# 7. vtebench-shaped payloads (no vtebench binary required)
for mode in cursor_motion light_cells dense_cells scrolling; do
  mark="$(log_mark)"
  payload="$("payload_$mode")"
  send_bytes "$PANE" "$payload" || fail "vtebench_$mode send"
  ref="$(run_foot_ref "vtebench_$mode" "$payload")"
  record "vtebench_$mode" "$(wait_samples "$mark")" "$ref"
done

# 8. notcurses-demo
mark="$(log_mark)"
if command -v notcurses-demo >/dev/null 2>&1; then
  send_cmd "$PANE" "timeout 6 notcurses-demo -k 2>/dev/null || true" \
    || fail "notcurses_demo send"
  sleep 7
  record "notcurses_demo" "$(parse_metrics "$mark")" "notcurses=present"
else
  # Fallback: dense unicode/color grid so the slot still produces metrics.
  send_bytes "$PANE" "$(python3 -c 'print("\x1b[2J\x1b[H" + "".join(f"\x1b[38;5;{n}m◆" for n in range(32))*12)')" \
    || fail "notcurses_demo send"
  record "notcurses_demo" "$(wait_samples "$mark")" "notcurses=missing fallback=sgr"
fi

python3 - "$OUTDIR" "$VERSION" "$FONT_PX" "$FOOT_FONT" "$PROFILE" "$BINS_NOTE" "$TARGETS" <<'PY'
import json, pathlib, sys
out, version, font_px, foot_font, profile, bins_note, targets_flag = (
    pathlib.Path(sys.argv[1]),
    sys.argv[2],
    sys.argv[3],
    sys.argv[4],
    sys.argv[5],
    sys.argv[6],
    sys.argv[7],
)
rows = []
for line in (out / "summary.tsv").read_text(encoding="utf-8").splitlines()[1:]:
    if not line.strip():
        continue
    parts = line.split("\t")
    name = parts[0]
    metrics = parts[1] if len(parts) > 1 else ""
    ref = parts[2] if len(parts) > 2 else ""
    targets = parts[3] if len(parts) > 3 else ""
    fields = {}
    for item in metrics.split():
        if "=" in item:
            k, v = item.split("=", 1)
            fields[k] = int(v) if v.lstrip("-").isdigit() else v
    rows.append(
        {
            "experiment": name,
            "metrics": fields,
            "ref": ref,
            "targets": targets,
        }
    )
(out / "metrics.json").write_text(
    json.dumps(
        {
            "version": version,
            "profile": profile,
            "bins": bins_note,
            "targets_enabled": targets_flag in ("1", "true", "yes"),
            "font_px": int(font_px),
            "foot_font": foot_font,
            "experiments": rows,
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
print(str(out / "metrics.json"))
PY

log "wrote $OUTDIR/summary.tsv and $OUTDIR/metrics.json"
log "passes=$PASSES fails=$FAILS"
if [[ "$FAILS" -gt 0 ]]; then
  exit 1
fi
exit 0
