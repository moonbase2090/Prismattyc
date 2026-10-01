#!/usr/bin/env bash
set -euo pipefail

export HOME=/home/demo
export DISPLAY="${DISPLAY:-:99}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-demo}"
export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin"
export PS1='demo@prismattyc:\w\$ '

mkdir -p "$XDG_RUNTIME_DIR" "$HOME/Desktop" "$HOME/work" "$HOME/.cache"
chmod 700 "$XDG_RUNTIME_DIR"
cat > "$HOME/.bash_profile" <<'PROFILE'
export PS1='demo@prismattyc:\w\$ '
PROFILE

if [[ ! -S "/tmp/.X11-unix/X${DISPLAY#:}" ]]; then
  Xvfb "$DISPLAY" -screen 0 1920x1080x24 -ac +extension GLX +render -noreset \
    >/tmp/prismattyc-demo-xvfb.log 2>&1 &
  for _ in $(seq 1 100); do
    xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 && break
    sleep 0.1
  done
  xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 || {
    echo "ERROR: Xvfb did not become ready" >&2
    exit 1
  }
fi

xsetroot -solid "#121214"
openbox >/tmp/prismattyc-demo-openbox.log 2>&1 &
sleep 0.3

if [[ -f /workspace/Cargo.toml ]]; then
  export CARGO_HOME="$HOME/.cache/cargo-home"
  export CARGO_TARGET_DIR="$HOME/.cache/cargo-target"
  mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR"
  cd /workspace
  cargo build --locked --release \
    -p prismattyc -p prismattyc-host -p prismattyc-mux -p pmux-mcp --bins
  export PATH="$CARGO_TARGET_DIR/release:$PATH"
  export PRISMATTYC_BINS="$CARGO_TARGET_DIR/release"
fi

cd "$HOME/work"
exec "$@"
