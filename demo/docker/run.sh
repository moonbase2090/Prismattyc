#!/usr/bin/env bash
# Build and run the public Linux/Xvfb demo recorder in an isolated container.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR="$(cd "$DIR/.." && pwd)"
REPO="$(cd "$DEMO_DIR/.." && pwd)"
NATIVE_DIR="$REPO/tests/native/docker"
NATIVE_IMAGE="${PRISMATTYC_NATIVE_TEST_IMAGE:-prismattyc-native-tests:latest}"
DEMO_IMAGE="${PRISMATTYC_DEMO_IMAGE:-prismattyc-demo:latest}"
PLATFORM="${PRISMATTYC_DEMO_PLATFORM:-linux/amd64}"
CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc-demo"
CRED_ROOT="$CACHE_ROOT/creds"
MODE="${1:-full}"

usage() {
  echo "usage: $0 [build|--check|--dry|--clips]" >&2
  exit 2
}

case "$MODE" in
  build|--check|--dry|--clips|full) ;;
  *) usage ;;
esac
[[ "$#" -le 1 ]] || usage

copy_credential() {
  local kind="$1" source="$2" destination
  destination="$CRED_ROOT/$kind/auth.json"
  python3 - "$kind" "$source" "$destination" <<'PY'
import os
import pathlib
import shutil
import sys
import tempfile

kind, source, destination = sys.argv[1:]
src = pathlib.Path(source).expanduser()
dst = pathlib.Path(destination)
if not src.is_file():
    print(f"ERROR: {kind} credential source is missing", file=sys.stderr)
    raise SystemExit(1)
dst.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
os.chmod(dst.parent, 0o700)
fd, temporary = tempfile.mkstemp(prefix=".auth-", dir=dst.parent)
try:
    with os.fdopen(fd, "wb") as target, src.open("rb") as origin:
        shutil.copyfileobj(origin, target)
    os.chmod(temporary, 0o600)
    os.replace(temporary, dst)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
}

stage_credentials() {
  umask 077
  mkdir -p "$CRED_ROOT"
  chmod 700 "$CACHE_ROOT" "$CRED_ROOT"
  copy_credential codex "${PRISMATTYC_CODEX_AUTH_FILE:-$HOME/.codex/auth.json}"
  copy_credential muse "${PRISMATTYC_MUSE_AUTH_FILE:-${XDG_CONFIG_HOME:-$HOME/.config}/muse/auth.json}"

  local eleven_source="${PRISMATTYC_DEMO_ELEVEN_ENV:-$DEMO_DIR/.eleven.env}"
  if [[ -f "$eleven_source" ]]; then
    cp "$eleven_source" "$CRED_ROOT/eleven.env.tmp"
    chmod 600 "$CRED_ROOT/eleven.env.tmp"
    mv -f "$CRED_ROOT/eleven.env.tmp" "$CRED_ROOT/eleven.env"
  fi
}

build_images() {
  if ! docker image inspect "$NATIVE_IMAGE" >/dev/null 2>&1; then
    echo "Build the public native test image."
    docker build --platform "$PLATFORM" -t "$NATIVE_IMAGE" "$NATIVE_DIR"
  fi
  echo "Build the Linux demo image."
  docker build --platform "$PLATFORM" --build-arg "NATIVE_IMAGE=$NATIVE_IMAGE" \
    -f "$DEMO_DIR/docker/Dockerfile" -t "$DEMO_IMAGE" "$REPO"
}

run_demo() {
  local argument="$1" run_id output_dir output_container
  run_id="${PRISMATTYC_DEMO_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
  case "$run_id" in
    *[!a-zA-Z0-9._-]*|"") echo "ERROR: invalid demo run id" >&2; return 2 ;;
  esac

  local name="prismattyc-demo-${run_id}"
  local -a mounts=(
    --mount "type=bind,src=$REPO,dst=/workspace,readonly"
    --mount "type=bind,src=$CRED_ROOT,dst=/home/demo/creds,readonly"
    --mount "type=volume,src=prismattyc-demo-cargo,dst=/home/demo/.cache"
  )
  local -a envs=(
    -e DISPLAY=:99
    -e WINIT_UNIX_BACKEND=x11
    -e COLORTERM=truecolor
    -e TERM=xterm-256color
    -e PRISMATTYC_DEMO_DIR=/workspace/demo
    -e PRISMATTYC_DEMO_CREDS=/home/demo/creds
    -e PRISMATTYC_DEMO_ENV_FILE=/home/demo/creds/eleven.env
    -e NARRDIR=/home/demo/.cache/prismattyc-demo/narr
    -e CARGO_HOME=/home/demo/.cache/cargo-home
    -e CARGO_TARGET_DIR=/home/demo/.cache/cargo-target
  )
  case "$argument" in
    --check)
      output_dir=""
      output_container=""
      ;;
    --dry)
      output_dir="$REPO/build/demo-reel-dry/$run_id"
      output_container=/home/demo/Desktop
      ;;
    --clips)
      output_dir="$REPO/build/demo-reel-clips/$run_id"
      output_container=/home/demo/Desktop
      ;;
    full)
      output_dir="$REPO/build/demo-reel/$run_id"
      output_container=/home/demo/Desktop
      ;;
    *) usage ;;
  esac

  if [[ -n "$output_dir" ]]; then
    [[ ! -e "$output_dir" ]] || { echo "ERROR: run output already exists" >&2; return 1; }
    mkdir -p "$output_dir"
    chmod 777 "$output_dir"
    mounts+=(--mount "type=bind,src=$output_dir,dst=$output_container")
  fi
  docker volume create prismattyc-demo-cargo >/dev/null
  echo "Run the isolated demo container with a two CPU limit."
  local -a command=(bash /workspace/demo/record-demo.sh)
  [[ "$argument" == "full" ]] || command+=("$argument")
  docker run --platform "$PLATFORM" --rm --init --cpus=2 --memory=8g --memory-swap=8g \
    --name "$name" --hostname prismattyc --shm-size=1g \
    -w /home/demo/work "${mounts[@]}" "${envs[@]}" \
    "$DEMO_IMAGE" "${command[@]}"
  if [[ "$argument" == "--dry" ]]; then
    echo "DRY_OUTPUT=build/demo-reel-dry/$run_id/demo-reel.mp4"
  elif [[ "$argument" == "full" ]]; then
    echo "OUTPUT=build/demo-reel/$run_id/demo-reel.mp4"
  fi
}

if [[ "$MODE" != "build" ]]; then
  stage_credentials
fi
build_images

case "$MODE" in
  build) ;;
  --check|--dry|--clips|full) run_demo "$MODE" ;;
esac
