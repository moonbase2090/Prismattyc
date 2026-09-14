#!/usr/bin/env bash
# Deterministic Phase 2B detach/reattach durability proof.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_TERM_COLOR=never

cargo build --locked -p prismattyc-mux --bins

BIN_DIR="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
SERVER_BIN="$BIN_DIR/pmuxd"
ATTACH_BIN="$BIN_DIR/pmux-attach"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
ARTIFACT_DIR="$ROOT/e2e/artifacts/phase2b-detach/$RUN_ID"
SOCKET="/tmp/pmux-detach-${RUN_ID}.sock"
mkdir -p "$ARTIFACT_DIR"

MARKER_ONE="PM41-ONE-$RUN_ID"
MARKER_TWO="PM41-TWO-$RUN_ID"
MARKER_PAYLOAD="${MARKER_ONE}"$'\n'"${MARKER_TWO}"$'\n'

"$SERVER_BIN" --socket "$SOCKET" -- /bin/sh -c \
  'printf "PM41-SERVER-READY\n"; exec /bin/cat' \
  >"$ARTIFACT_DIR/server.stdout" 2>"$ARTIFACT_DIR/server.stderr" &
SERVER_PID=$!

stop_server() {
  if kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID"
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap stop_server EXIT INT TERM

for _ in $(seq 1 100); do
  if test -S "$SOCKET"; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "phase2b-detach: server exited before publishing its socket" >&2
    exit 1
  fi
  sleep 0.02
done
test -S "$SOCKET"

# First client obtains a server-minted identity, writes two distinct markers,
# reads one frame, and exits. Its connection drop is the detach boundary.
"$ATTACH_BIN" --socket "$SOCKET" --write "$MARKER_PAYLOAD" \
  >"$ARTIFACT_DIR/first-attach.json"

first_pane_id="$(rg -o '"pane_id":[0-9]+' "$ARTIFACT_DIR/first-attach.json" | head -1 | cut -d: -f2)"
child_pid="$(rg -o '"child_pid":[0-9]+' "$ARTIFACT_DIR/first-attach.json" | head -1 | cut -d: -f2)"
test -n "$first_pane_id"
test -n "$child_pid"
rg -Fq '"child_alive":true' "$ARTIFACT_DIR/first-attach.json"

# Detach must preserve both lifetime owners before any second client exists.
kill -0 "$SERVER_PID"
kill -0 "$child_pid"

# A fresh connection receives a new ClientId, but must see the same pane,
# emulator content, child PID, and alive state without respawning anything.
reattached=false
for _ in $(seq 1 100); do
  "$ATTACH_BIN" --socket "$SOCKET" >"$ARTIFACT_DIR/reattach.json"
  if rg -Fq "$MARKER_ONE" "$ARTIFACT_DIR/reattach.json" \
    && rg -Fq "$MARKER_TWO" "$ARTIFACT_DIR/reattach.json"; then
    reattached=true
    break
  fi
  sleep 0.02
done
test "$reattached" = true

second_pane_id="$(rg -o '"pane_id":[0-9]+' "$ARTIFACT_DIR/reattach.json" | head -1 | cut -d: -f2)"
second_child_pid="$(rg -o '"child_pid":[0-9]+' "$ARTIFACT_DIR/reattach.json" | head -1 | cut -d: -f2)"
test "$second_pane_id" = "$first_pane_id"
test "$second_child_pid" = "$child_pid"
rg -Fq '"child_alive":true' "$ARTIFACT_DIR/reattach.json"
kill -0 "$SERVER_PID"
kill -0 "$child_pid"

{
  printf 'phase2b-detach: PASS\n'
  printf 'server_pid=%s\n' "$SERVER_PID"
  printf 'child_pid=%s\n' "$child_pid"
  printf 'pane_id=%s\n' "$first_pane_id"
  printf 'marker_one=%s\n' "$MARKER_ONE"
  printf 'marker_two=%s\n' "$MARKER_TWO"
  printf 'server_alive_after_detach=true\n'
  printf 'child_alive_after_detach=true\n'
  printf 'same_topology_after_reattach=true\n'
  printf 'same_child_after_reattach=true\n'
} >"$ARTIFACT_DIR/summary.txt"

cat "$ARTIFACT_DIR/summary.txt"
printf 'artifacts=%s\n' "$ARTIFACT_DIR"

stop_server
trap - EXIT INT TERM
