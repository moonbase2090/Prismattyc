#!/usr/bin/env bash
# Run isolated native-window regression tests.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NATIVE="$(cd "$DIR/.." && pwd)"
REPO="$(cd "$NATIVE/../.." && pwd)"
IMAGE="${PRISMATTYC_NATIVE_TEST_IMAGE:-prismattyc-native-tests}"

build() {
  docker build -t "$IMAGE" "$DIR"
}

host_ux_e2e() {
  docker image inspect "$IMAGE" >/dev/null 2>&1 || {
    echo "ERROR: image $IMAGE is missing. Run: $0 build" >&2
    return 1
  }
  local name="pt303-host-ux-$$" cid src bin status
  local fixture_bins="${PRISMATTYC_BINS:-$REPO/target/debug}"
  local run_id="${HOST_UX_RUN_ID:-$(date -u +%Y%m%d-%H%M%S)-$$}"
  case "$run_id" in
    *[!a-zA-Z0-9._-]*|"") echo "ERROR: invalid HOST_UX_RUN_ID" >&2; return 1 ;;
  esac
  local destination="$REPO/build/host-ux-e2e/$run_id"
  [[ ! -e "$destination" ]] || { echo "ERROR: use a new HOST_UX_RUN_ID: $destination" >&2; return 1; }
  mkdir -p "$destination"
  cid="$(docker create --init --name "$name" --hostname prismattyc --shm-size 1g \
    -e DISPLAY=:99 -e WINIT_UNIX_BACKEND=x11 -e COLORTERM=truecolor \
    "$IMAGE" sleep infinity)"
  trap "docker rm -f '$name' >/dev/null 2>&1 || true" EXIT
  for bin in pmux pmuxd pmux-attach prismattyc-host; do
    src="$fixture_bins/$bin"
    [[ -x "$src" ]] || { echo "ERROR: missing tested binary $src" >&2; return 1; }
    docker cp "$src" "$cid:/usr/local/bin/$bin"
  done
  docker cp "$NATIVE/host-ux-e2e.sh" "$cid:/home/tester/host-ux-e2e.sh"
  docker cp "$NATIVE/host-ux-e2e.py" "$cid:/home/tester/host-ux-e2e.py"
  docker cp "$NATIVE/space-open-race.py" "$cid:/home/tester/space-open-race.py"
  docker cp "$NATIVE/restart-spaces-e2e.py" "$cid:/home/tester/restart-spaces-e2e.py"
  docker start "$cid" >/dev/null
  if docker exec -u tester -e DISPLAY=:99 -e HOST_UX_OUT=/tmp/host-ux-e2e \
    -e HOST_UX_ANIMATION="${HOST_UX_ANIMATION:-light-cycle}" \
    -e HOST_UX_CASE="${HOST_UX_CASE:-all}" \
    "$cid" bash /home/tester/host-ux-e2e.sh; then
    status=0
  else
    status=$?
  fi
  docker cp "$cid:/tmp/host-ux-e2e/." "$destination/" || return 1
  echo "Host UX evidence: $destination"
  [[ "$status" -eq 0 ]] || return "$status"
  python3 - "$destination" "${HOST_UX_CASE:-all}" <<'PY_CHECK'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
if sys.argv[2] == 'all':
    assert json.loads((root / 'result.json').read_text())['status'] == 'PASS'
    assert 'HOST_UX_E2E_COMPLETE: caret, light-cycle, and restore PASS' in (root / 'runner.log').read_text()
    assert json.loads((root / 'restart-spaces/result.json').read_text())['status'] == 'PASS'
    assert 'RESTART_SPACES_E2E_COMPLETE:' in (root / 'restart-spaces.log').read_text()
assert json.loads((root / 'space-open-race/result.json').read_text())['status'] == 'PASS'
assert 'SPACE_OPEN_RACE_COMPLETE:' in (root / 'space-open-race.log').read_text()
PY_CHECK
}

spaces_e2e() {
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ERROR: image $IMAGE is missing. Run: $0 build" >&2
    exit 1
  fi
  local name="pt217-spaces-$$"
  local cid
  cid="$(docker create --init --name "$name" --hostname prismattyc --shm-size 1g \
    -e DISPLAY=:99 \
    -e WINIT_UNIX_BACKEND=x11 \
    -e COLORTERM=truecolor \
    "$IMAGE" sleep infinity)"
  # Expand $name now: EXIT runs after this function returns, under set -u.
  trap "docker rm -f '$name' >/dev/null 2>&1 || true" EXIT
  local src bin
  for bin in pmux pmuxd pmux-attach prismattyc-host; do
    src="$(resolve_e2e_bin "$bin")"
    echo "  e2e bin $bin <- $src"
    docker cp "$src" "$cid:/usr/local/bin/$bin"
  done
  docker cp "$NATIVE/spaces-e2e.sh" "$cid:/home/tester/spaces-e2e.sh"
  docker start "$cid" >/dev/null
  sleep 1
  docker exec -u tester -e DISPLAY=:99 -e WIGGLE="${WIGGLE:-0}" \
    "$cid" bash /home/tester/spaces-e2e.sh
}

# PT-295: double-click [show me] on a walkthrough caption. Same X11 test
# image as spaces-e2e. No --privileged.
walkthrough_caption_e2e() {
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ERROR: image $IMAGE is missing. Run: $0 build" >&2
    exit 1
  fi
  local name="pt295-caption-$$"
  local cid
  cid="$(docker create --init --name "$name" --hostname prismattyc --shm-size 1g \
    -e DISPLAY=:99 \
    -e WINIT_UNIX_BACKEND=x11 \
    -e COLORTERM=truecolor \
    "$IMAGE" sleep infinity)"
  trap "docker rm -f '$name' >/dev/null 2>&1 || true" EXIT
  local src bin
  for bin in pmux pmuxd pmux-attach prismattyc-host; do
    src="$(resolve_e2e_bin "$bin")"
    echo "  e2e bin $bin <- $src"
    docker cp "$src" "$cid:/usr/local/bin/$bin"
  done
  docker cp "$NATIVE/walkthrough-caption-e2e.sh" "$cid:/home/tester/walkthrough-caption-e2e.sh"
  docker start "$cid" >/dev/null
  docker exec -u 0 "$cid" chown tester:tester /home/tester/walkthrough-caption-e2e.sh
  sleep 1
  docker exec -u tester -e DISPLAY=:99 \
    "$cid" bash /home/tester/walkthrough-caption-e2e.sh
}

# PT-290: same prismattyc-native-tests image, weston headless. Sway ignored
# WAYLAND_DISPLAY and created wayland-1. Weston --socket=pt290-wayland
# is the unique name. No --privileged (PT-278). No extra caps.
spaces_e2e_wayland() {
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ERROR: image $IMAGE is missing. Run: $0 build" >&2
    exit 1
  fi
  local name="pt290-wayland-$$"
  local cid
  cid="$(docker create --init --name "$name" --hostname prismattyc --shm-size 1g \
    -e COLORTERM=truecolor \
    "$IMAGE" sleep infinity)"
  trap "docker rm -f '$name' >/dev/null 2>&1 || true" EXIT
  local src bin
  for bin in pmux pmuxd pmux-attach prismattyc-host; do
    src="$(resolve_e2e_bin "$bin")"
    echo "  e2e bin $bin <- $src"
    docker cp "$src" "$cid:/usr/local/bin/$bin"
  done
  docker cp "$NATIVE/spaces-e2e-wayland.sh" "$cid:/home/tester/spaces-e2e-wayland.sh"
  docker start "$cid" >/dev/null
  docker exec -u 0 "$cid" chown tester:tester /home/tester/spaces-e2e-wayland.sh
  sleep 1
  # Stale prismattyc-native-tests images predate a308241 (weston/grim/wtype in
  # the Dockerfile). Install the three together. The opaque splash
  # assert fail-closes without grim or weston-screenshooter.
  local need=0
  docker exec -u tester "$cid" command -v weston >/dev/null 2>&1 || need=1
  docker exec -u tester "$cid" command -v grim >/dev/null 2>&1 || need=1
  docker exec -u tester "$cid" command -v wtype >/dev/null 2>&1 || need=1
  if [[ "$need" -eq 1 ]]; then
    docker exec -u 0 "$cid" pacman -Sy --noconfirm --needed weston grim wtype \
      >/tmp/pt290-pacman.log 2>&1 || {
      echo "ERROR: pacman could not install weston grim wtype" >&2
      cat /tmp/pt290-pacman.log >&2 || true
      return 1
    }
  fi
  docker exec -u tester \
    -e XDG_RUNTIME_DIR=/tmp/runtime-tester \
    "$cid" env -u DISPLAY -u WINIT_UNIX_BACKEND \
    bash /home/tester/spaces-e2e-wayland.sh
}

render_bench() {
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "ERROR: image $IMAGE is missing. Run: $0 build" >&2
    exit 1
  fi
  local name="pt246-bench-$$"
  local cid
  cid="$(docker create --init --name "$name" --hostname prismattyc --shm-size 1g \
    -e DISPLAY=:99 \
    -e WINIT_UNIX_BACKEND=x11 \
    -e COLORTERM=truecolor \
    "$IMAGE" sleep infinity)"
  trap "docker rm -f '$name' >/dev/null 2>&1 || true" EXIT
  local src bin
  for bin in pmux pmuxd pmux-attach prismattyc-host; do
    src="$(resolve_e2e_bin "$bin")"
    echo "  bench bin $bin <- $src"
    docker cp "$src" "$cid:/usr/local/bin/$bin"
  done
  docker cp "$NATIVE/render-bench.sh" "$cid:/home/tester/render-bench.sh"
  docker start "$cid" >/dev/null
  sleep 1
  # Foot is the wall-clock reference. notcurses-demo is experiment 8.
  # Ignore pacman failure: the script still records prismattyc metrics.
  docker exec -u 0 "$cid" pacman -Sy --noconfirm --needed foot notcurses \
    >/tmp/pt246-pacman.log 2>&1 || true
  local ver profile host_bin
  ver="$(docker exec -u tester "$cid" prismattyc-host --version 2>/dev/null \
    | awk '{print $2; exit}' || echo unknown)"
  host_bin="$(resolve_e2e_bin prismattyc-host)"
  case "$host_bin" in
    */target/release/*) profile=release ;;
    */target/debug/*) profile=debug ;;
    *) profile=installed ;;
  esac
  set +e
  docker exec -u tester -e DISPLAY=:99 \
    -e RENDER_BENCH_OUT="/tmp/render-bench/$ver" \
    -e RENDER_BENCH_TARGETS="${RENDER_BENCH_TARGETS:-0}" \
    -e RENDER_BENCH_PROFILE="$profile" \
    -e RENDER_BENCH_BINS="$host_bin" \
    "$cid" bash /home/tester/render-bench.sh
  local st=$?
  set -e
  mkdir -p "$REPO/build/render-bench/$ver"
  docker cp "$cid:/tmp/render-bench/$ver/." "$REPO/build/render-bench/$ver/" 2>/dev/null || true
  echo "archived $REPO/build/render-bench/$ver"
  return "$st"
}

# Prefer the branch under test: PRISMATTYC_BINS, else repo target/debug,
# else PATH (with a warning).
resolve_e2e_bin() {
  local name="$1" src
  if [[ -n "${PRISMATTYC_BINS:-}" ]]; then
    src="${PRISMATTYC_BINS%/}/$name"
    if [[ -x "$src" ]]; then
      echo "$src"
      return 0
    fi
    echo "ERROR: PRISMATTYC_BINS=$PRISMATTYC_BINS has no $name" >&2
    return 1
  fi
  src="$REPO/target/debug/$name"
  if [[ -x "$src" ]]; then
    echo "$src"
    return 0
  fi
  src="$(command -v "$name" || true)"
  if [[ -n "$src" ]]; then
    echo "WARNING: using PATH $name at $src (not the branch under test)" >&2
    echo "$src"
    return 0
  fi
  echo "ERROR: $name not in PRISMATTYC_BINS, $REPO/target/debug, or PATH" >&2
  return 1
}

case "${1:-}" in
  build) build ;;
  spaces-e2e) spaces_e2e ;;
  host-ux-e2e) host_ux_e2e ;;
  spaces-e2e-wayland) spaces_e2e_wayland ;;
  walkthrough-caption-e2e) walkthrough_caption_e2e ;;
  render-bench) render_bench ;;
  *) echo "Usage: $0 build|spaces-e2e|host-ux-e2e|spaces-e2e-wayland|walkthrough-caption-e2e|render-bench" >&2; exit 2 ;;
esac
