#!/usr/bin/env bash
# PT-268 mechanism box. Run inside the demo Docker image.
# It runs one real prismattyc-host event loop with four real attached panes.
# Each pane receives the same deterministic 60-second PT-250-shaped flood.
# The budgeted run keeps the 8 MiB per-stream host admission budget. The
# unbudgeted run sets the documented box-only override to zero. Queue metrics,
# not allocator-dependent heap deltas, are the acceptance gate.
set -euo pipefail

export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
export DISPLAY="${DISPLAY:-:99}"
export WINIT_UNIX_BACKEND=x11
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-demo}"
SOCK="${XDG_RUNTIME_DIR}/prismattyc/pmux.sock"
OUT="${PT268_OUT:-$HOME/pt268-flood-box}"
DURATION="${PT268_DURATION_S:-60}"
LIMIT_BYTES=$((64 * 1024 * 1024))
READER_BUDGET_BYTES=$((8 * 1024 * 1024))
ATTACHED_STREAMS="${PT268_ATTACHED_STREAMS:-8}"
# PT-250 used three panes under a producer-faster-than-consumer workload.
# Eight streams × four concurrent writers keeps the box deterministic in the
# one-GiB demo container while making aggregate high-water observable.
OUTPUT_WORKERS_PER_STREAM="${PT268_OUTPUT_WORKERS_PER_STREAM:-4}"
STATUS_WORKERS_PER_STREAM="${PT268_STATUS_WORKERS_PER_STREAM:-1}"
STATUS_BATCH_SIZE="${PT268_STATUS_BATCH_SIZE:-32}"
FRAME_COLUMNS=189
FRAME_ROWS=44
FRAMES_PER_WRITE="${PT268_FRAMES_PER_WRITE:-1}"
BURST_WRITES="${PT268_BURST_WRITES:-64}"
UNBUDGETED_MULTIPLE=1
mkdir -p "$OUT"

log() { printf '[pt268] %s\n' "$*"; }
fail() { log "FAIL: $*" >&2; exit 1; }

command -v heaptrack >/dev/null || fail "heaptrack is not installed in the demo image"
command -v heaptrack_print >/dev/null || fail "heaptrack_print is not installed in the demo image"
[[ -S "$SOCK" ]] || fail "pmux socket is missing: $SOCK"

wait_for_file() {
  local path="$1" seconds="${2:-10}"
  for _ in $(seq 1 $((seconds * 10))); do
    [[ -e "$path" ]] && return 0
    sleep 0.1
  done
  return 1
}

wait_for_server() {
  local seconds="${1:-10}"
  for _ in $(seq 1 $((seconds * 10))); do
    if pmux status >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# Return a live attach child. A just-restarted pmuxd can leave its socket
# pathname in place before it accepts a new client; retry only that startup
# race and preserve all other attach errors in the log.
launch_attach() {
  local attach_log_path="$1" session="$2"
  for _ in $(seq 1 50); do
    wait_for_server 2 || return 1
    : >"$attach_log_path"
    pmux-attach --socket "$SOCK" --session "$session" --watch --json \
      >"$attach_log_path" 2>&1 &
    ATTACH_PID=$!
    sleep 0.2
    if kill -0 "$ATTACH_PID" 2>/dev/null; then
      return 0
    fi
    wait "$ATTACH_PID" 2>/dev/null || true
    if ! grep -q 'stale control socket' "$attach_log_path" 2>/dev/null; then
      return 1
    fi
    sleep 0.2
  done
  return 1
}

wait_for_policy() {
  local mode="$1" budget="$2" streams="$3" seconds="${4:-15}"
  for _ in $(seq 1 $((seconds * 5))); do
    if pmux render-status --json 2>/dev/null | python3 -c '
import json, sys
mode = sys.argv[1]
budget = int(sys.argv[2])
streams = int(sys.argv[3])
reference_budget = int(sys.argv[4])
unbudgeted_multiple = int(sys.argv[5])
try:
    body = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
queue = body.get("attach_queue", {})
panes = [
    pane.get("attach_policy", {})
    for window in body.get("windows", [])
    for pane in window.get("current_panes", [])
]
attached = [pane for pane in panes if pane.get("reader_queue_budget_bytes", 0) > 0]
if len(attached) < streams or queue.get("stream_count", 0) < streams:
    raise SystemExit(1)
if not all(
    pane.get("notice") and pane.get("batches", 0) > 0 and pane.get("frames", 0) > 0
    for pane in attached
):
    raise SystemExit(1)
if mode == "budgeted":
    engaged = (
        queue.get("reader_blocked_ms", 0) > 0
        or any(pane.get("coalesced_frames", 0) > 0 for pane in attached)
    )
    accepted = all(
        pane.get("reader_queue_high_water_bytes", 0) <= budget for pane in attached
    ) and engaged
else:
    accepted = queue.get("reader_queue_high_water_bytes", 0) > unbudgeted_multiple * reference_budget
if accepted:
    raise SystemExit(0)
raise SystemExit(1)
' "$mode" "$budget" "$streams" "$READER_BUDGET_BYTES" "$UNBUDGETED_MULTIPLE"; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}

policy_metrics() {
  pmux render-status --json 2>/dev/null | python3 -c '
import json, sys
try:
    body = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
queue = body.get("attach_queue", {})
panes = [
    pane.get("attach_policy", {})
    for window in body.get("windows", [])
    for pane in window.get("current_panes", [])
]
attached = [pane for pane in panes if pane.get("reader_queue_budget_bytes", 0) > 0]
if not attached:
    raise SystemExit(1)
print(
    queue.get("reader_queue_high_water_bytes", 0),
    queue.get("reader_blocked_ms", 0),
    max((pane.get("reader_queue_high_water_bytes", 0) for pane in attached), default=0),
    max((pane.get("coalesced_frames", 0) for pane in attached), default=0),
    max((pane.get("dropped_frames", 0) for pane in attached), default=0),
    max((pane.get("dropped_bytes", 0) for pane in attached), default=0),
    queue.get("reader_queue_bytes", 0),
    len(attached),
)
'
}

heap_bytes() {
  local report="$1"
  python3 - "$report" <<'PY'
import re, sys
text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
patterns = [
    r"peak heap memory consumption:\s*([0-9.]+)\s*(B|K|M|G|KiB|MiB|GiB)",
    r"peak heap memory consumption:\s*([0-9.]+)\s*([KMGT]?i?B)",
]
for pattern in patterns:
    match = re.search(pattern, text, re.IGNORECASE)
    if not match:
        continue
    value = float(match.group(1))
    unit = match.group(2).lower()
    scale = {
        "b": 1,
        "k": 1024,
        "m": 1024**2,
        "g": 1024**3,
        "kib": 1024,
        "mib": 1024**2,
        "gib": 1024**3,
    }.get(unit)
    if scale is None:
        scale = {"kb": 1000, "mb": 1000**2, "gb": 1000**3}.get(unit)
    if scale is not None:
        print(int(value * scale))
        raise SystemExit(0)
print("0")
PY
}

sample_memory() {
  local pid="$1" samples="$2"
  local start="$(date +%s.%N)"
  : >"$samples"
  while kill -0 "$pid" 2>/dev/null; do
    local now rss hwm
    now="$(date +%s.%N)"
    rss="$(awk '/^VmRSS:/ {print $2 * 1024}' "/proc/$pid/status" 2>/dev/null || echo 0)"
    hwm="$(awk '/^VmHWM:/ {print $2 * 1024}' "/proc/$pid/status" 2>/dev/null || echo 0)"
    printf '{"elapsed_s":%.3f,"host_rss_bytes":%s,"host_hwm_bytes":%s}\n' \
      "$(python3 - "$start" "$now" <<'PY'
import sys
print(float(sys.argv[2]) - float(sys.argv[1]))
PY
      )" "$rss" "$hwm" >>"$samples"
    sleep 1
  done
}

flood_command() {
  local marker_file="$1"
  cat <<EOF
set -eu
sleep 5
pmux status-set "PT268 flood active" >/dev/null 2>&1 || true
printf 'PT268-FLOOD-START\\n' > "${marker_file}"
status_flood() {
  python3 - <<PY
import json
import os
import socket
import time

socket_path = os.path.join(
    os.environ.get("XDG_RUNTIME_DIR", "/tmp/runtime-demo"),
    "prismattyc",
    "pmux.sock",
)
pane_id = int(os.environ["PRISMATTYC_PANE_ID"])
deadline = time.monotonic() + ${DURATION}
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(socket_path)
reader = client.makefile("rb")

client.sendall((json.dumps({
    "type": "register_client", "version": 1, "request_id": 1
}, separators=(",", ":")) + "\\n").encode())
if not reader.readline():
    raise RuntimeError("pmuxd closed the status flood connection")
request_id = 2
while time.monotonic() < deadline:
    requests = []
    for _ in range(${STATUS_BATCH_SIZE}):
        text = "PT268-" + str(request_id).zfill(8) + "-" + ("X" * 49)
        requests.append({
            "type": "set_pane_status",
            "version": 1,
            "request_id": request_id,
            "pane_id": pane_id,
            "text": text,
        })
        request_id += 1
    client.sendall(("\\n".join(
        json.dumps(request, separators=(",", ":")) for request in requests
    ) + "\\n").encode())
    for _ in requests:
        if not reader.readline():
            raise RuntimeError("pmuxd closed the status flood connection")
PY
}
for _ in \$(seq 1 ${STATUS_WORKERS_PER_STREAM}); do
  status_flood &
done
flood_worker() {
  python3 - <<PY
import os
import time

line = bytes((65 + (index % 26) for index in range(${FRAME_COLUMNS}))) + b"\\r\\n"
counter = 0
deadline = time.monotonic() + ${DURATION}
while time.monotonic() < deadline:
    for _ in range(${BURST_WRITES}):
        color = f"\\x1b[38;5;{16 + counter % 216}m".encode()
        payload = b"\\x1b[?1049h\\x1b[2J\\x1b[H" + (color + line) * ${FRAME_ROWS} * ${FRAMES_PER_WRITE}
        os.write(1, payload)
        counter += 1
PY
}
for _ in \$(seq 1 ${OUTPUT_WORKERS_PER_STREAM}); do
  flood_worker &
done
wait
printf 'PT268-FLOOD-DONE\\n' >> "${marker_file}"
pmux status-set "PT268 flood complete" >/dev/null 2>&1 || true
sleep 3
EOF
}

run_baseline() {
  local trace_base="$OUT/baseline.heaptrack"
  local trace="${trace_base}.zst"
  local report="$OUT/baseline.heaptrack.txt"
  local wrapper target peak
  rm -f "$trace_base" "$trace" "$report"
  log "baseline: measuring host startup heap"
  heaptrack --record-only -o "$trace_base" \
    prismattyc-host --no-splash -- /bin/sh -c 'sleep 12' \
    >"$OUT/baseline.host.log" 2>&1 &
  wrapper=$!
  sleep 5
  target="$(pgrep -x prismattyc-host | head -1 || true)"
  [[ -n "$target" ]] || fail "baseline: host did not start"
  kill "$target" 2>/dev/null || true
  for _ in $(seq 1 100); do
    kill -0 "$wrapper" 2>/dev/null || break
    sleep 0.1
  done
  kill "$wrapper" 2>/dev/null || true
  wait "$wrapper" 2>/dev/null || true
  wait_for_file "$trace" 10 || fail "baseline: heaptrack trace was not produced"
  heaptrack_print -f "$trace" -a 0 -T 0 -p 0 >"$report" 2>&1
  peak="$(heap_bytes "$report")"
  [[ "$peak" -gt 0 ]] || fail "baseline: heaptrack report has no peak heap value"
  printf '%s\n' "$peak" >"$OUT/baseline.bytes"
  log "baseline peak_live_heap_bytes=$peak"
}

run_mode() {
  local mode="$1" budget="$2"
  local trace_base="$OUT/${mode}.heaptrack"
  local trace="${trace_base}.zst"
  local report="$OUT/${mode}.heaptrack.txt"
  local samples="$OUT/${mode}.samples.jsonl"
  local host_log="$OUT/${mode}.host.log"
  local attach_log="$OUT/${mode}.attach.log"
  local host_pid host_wrapper attach_pid
  local observer_session="pt268-observer-${mode}-$$"
  local sessions=() markers=() attach_args=()

  rm -f "$trace_base" "$trace" "$report" "$samples" "$host_log" "$attach_log"
  log "$mode: creating ${ATTACHED_STREAMS}-pane, ${DURATION}-second PT-250-shaped flood"
  log "$mode: producer rate=${OUTPUT_WORKERS_PER_STREAM} output writers + ${STATUS_WORKERS_PER_STREAM} status writers per stream; frame=${FRAME_COLUMNS}x${FRAME_ROWS} repeats=${FRAMES_PER_WRITE} burst=${BURST_WRITES}"
  pmux new "$observer_session" --no-attach --no-agent -- /bin/sh -c 'sleep 600' >/dev/null
  for index in $(seq 1 "$ATTACHED_STREAMS"); do
    local session="pt268-flood-${mode}-$$-${index}"
    local marker="$OUT/${mode}.markers.${index}"
    sessions+=("$session")
    markers+=("$marker")
    pmux new "$session" --no-attach --no-agent -- /bin/sh -c "$(flood_command "$marker")" >/dev/null
  done
  sleep 2

  for session in "${sessions[@]}"; do
    attach_args+=(--attach-session "$session")
  done
  log "$mode: launching real pmux-attach child for observer session $observer_session"
  launch_attach "$attach_log" "$observer_session" \
    || fail "$mode: could not keep a real pmux-attach child alive"
  attach_pid="$ATTACH_PID"

  log "$mode: profiling prismattyc-host with budget=$budget"
  PRISMATTYC_HOST_EVENT_BUDGET_BYTES="$budget" \
    heaptrack --record-only -o "$trace_base" \
    prismattyc-host --no-splash "${attach_args[@]}" \
    >"$host_log" 2>&1 &
  host_wrapper=$!

  sleep 2
  host_pid=""
  for _ in $(seq 1 50); do
    host_pid="$(pgrep -x prismattyc-host | head -1 || true)"
    [[ -n "$host_pid" ]] && break
    sleep 0.2
  done
  [[ -n "$host_pid" ]] || fail "$mode: profiled prismattyc-host did not start"
  sample_memory "$host_pid" "$samples" &

  local start_seen=0
  local done_seen=0
  for _ in $(seq 1 $((DURATION + 60))); do
    if (( start_seen == 0 )); then
      start_seen=1
      for marker in "${markers[@]}"; do
        if ! grep -q 'PT268-FLOOD-START' "$marker" 2>/dev/null; then
          start_seen=0
          break
        fi
      done
    fi
    if (( done_seen == 0 )); then
      done_seen=1
      for marker in "${markers[@]}"; do
        if ! grep -q 'PT268-FLOOD-DONE' "$marker" 2>/dev/null; then
          done_seen=0
          break
        fi
      done
    fi
    if (( done_seen == 1 )); then
      break
    fi
    sleep 1
  done
  (( start_seen == 1 )) \
    || fail "$mode: attached flood panes saw no complete start set"
  (( done_seen == 1 )) \
    || fail "$mode: attached flood panes saw no complete completion set"
  if ! wait_for_policy "$mode" "$budget" "$ATTACHED_STREAMS" "$((DURATION + 60))"; then
    log "$mode: render-status snapshot:"
    pmux render-status --json 2>&1 || true
    log "$mode: host log tail:"
    tail -40 "$host_log" 2>/dev/null || true
    log "$mode: attach log tail:"
    tail -c 2000 "$attach_log" 2>/dev/null || true
    fail "$mode: queue policy or status counters were not observed for all streams"
  fi
  local queue_metrics aggregate_high aggregate_blocked max_stream_high coalesced
  local dropped_frames dropped_bytes queue_bytes stream_count
  queue_metrics="$(policy_metrics)" || fail "$mode: queue metrics were not published"
  read -r aggregate_high aggregate_blocked max_stream_high coalesced \
    dropped_frames dropped_bytes queue_bytes stream_count <<<"$queue_metrics"

  sleep 5
  kill "$host_pid" 2>/dev/null || true
  kill "$attach_pid" 2>/dev/null || true
  for _ in $(seq 1 100); do
    kill -0 "$host_wrapper" 2>/dev/null || break
    sleep 0.1
  done
  kill "$host_wrapper" 2>/dev/null || true
  wait "$host_wrapper" 2>/dev/null || true
  wait "$attach_pid" 2>/dev/null || true
  for session in "${sessions[@]}"; do
    pmux stop "$session" >/dev/null 2>&1 || true
  done
  pmux stop "$observer_session" >/dev/null 2>&1 || true
  wait_for_file "$trace" 10 || fail "$mode: heaptrack trace was not produced"
  heaptrack_print -f "$trace" -a 0 -T 0 -p 0 >"$report" 2>&1
  local peak attach_peak
  peak="$(heap_bytes "$report")"
  if [[ "$peak" -le 0 ]]; then
    log "$mode: heaptrack report tail:"
    tail -80 "$report" 2>/dev/null || true
    log "$mode: host log tail:"
    tail -80 "$host_log" 2>/dev/null || true
    fail "$mode: heaptrack report has no peak heap value"
  fi
  attach_peak="$((peak > BASELINE_PEAK ? peak - BASELINE_PEAK : 0))"
  log "RESULT: $mode reader_queue_high_water_bytes=$aggregate_high max_stream_reader_queue_high_water_bytes=$max_stream_high reader_queue_bytes=$queue_bytes reader_blocked_ms=$aggregate_blocked coalesced_frames=$coalesced dropped_frames=$dropped_frames dropped_bytes=$dropped_bytes stream_count=$stream_count peak_attach_live_heap_bytes=$attach_peak raw_peak_live_heap_bytes=$peak baseline_live_heap_bytes=$BASELINE_PEAK informational_heap_limit_bytes=$LIMIT_BYTES samples=$samples trace=$trace"
  printf '%s %s %s %s %s %s %s %s %s\n' "$mode" "$aggregate_high" "$max_stream_high" "$aggregate_blocked" "$coalesced" "$dropped_frames" "$dropped_bytes" "$queue_bytes" "$stream_count" >>"$OUT/results.txt"
}

: >"$OUT/results.txt"
run_baseline
BASELINE_PEAK="$(<"$OUT/baseline.bytes")"
run_mode budgeted "$READER_BUDGET_BYTES"
run_mode unbudgeted 0

python3 - "$OUT/results.txt" "$READER_BUDGET_BYTES" "$UNBUDGETED_MULTIPLE" <<'PY'
import sys

values = {}
for line in open(sys.argv[1], encoding="utf-8"):
    (
        mode,
        aggregate_high,
        max_stream_high,
        blocked,
        coalesced,
        dropped_frames,
        dropped_bytes,
        queued,
        stream_count,
    ) = line.split()
    values[mode] = {
        "aggregate_high": int(aggregate_high),
        "max_stream_high": int(max_stream_high),
        "blocked": int(blocked),
        "coalesced": int(coalesced),
        "dropped_frames": int(dropped_frames),
        "dropped_bytes": int(dropped_bytes),
        "queued": int(queued),
        "stream_count": int(stream_count),
    }
budget = int(sys.argv[2])
multiple = int(sys.argv[3])
budgeted = values["budgeted"]
unbudgeted = values["unbudgeted"]
if budgeted["max_stream_high"] > budget:
    raise SystemExit(
        "budgeted per-stream reader queue high-water "
        f"{budgeted['max_stream_high']} exceeds {budget}"
    )
if budgeted["blocked"] <= 0 and budgeted["coalesced"] <= 0:
    raise SystemExit("budgeted reader did not show blocked time or coalesced frames")
if unbudgeted["aggregate_high"] <= multiple * budget:
    raise SystemExit(
        "unbudgeted aggregate reader queue high-water "
        f"{unbudgeted['aggregate_high']} does not exceed {multiple * budget}"
    )
print(
    "RESULT: PASS "
    f"streams={budgeted['stream_count']} "
    f"budgeted_reader_queue_high_water_bytes={budgeted['aggregate_high']} "
    f"budgeted_max_stream_reader_queue_high_water_bytes={budgeted['max_stream_high']} "
    f"unbudgeted_reader_queue_high_water_bytes={unbudgeted['aggregate_high']} "
    f"budgeted_reader_blocked_ms={budgeted['blocked']} "
    f"budgeted_coalesced_frames={budgeted['coalesced']}"
)
PY
