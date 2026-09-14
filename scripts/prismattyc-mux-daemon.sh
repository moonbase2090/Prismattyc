#!/usr/bin/env bash
# Compat wrapper: the mux front door is the `pmux` binary.
#
#   pmux up|attach|ls|new|status|stop|restart   (see pmux --help)
#
# `start` maps to `up`. `PMUX_SOCKET` overrides the default socket.
set -euo pipefail

pick_bin() {
  local candidate
  for candidate in \
    "${PMUX_BIN:-}" \
    "${CARGO_HOME:-$HOME/.cargo}/bin/pmux"; do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
      echo "$candidate"
      return 0
    fi
  done
  if command -v pmux >/dev/null 2>&1; then
    echo pmux
    return 0
  fi
  echo "missing pmux — run: cargo install --path crates/prismattyc-mux --bins --locked" >&2
  exit 1
}

BIN="$(pick_bin)"

args=()
if [[ -n "${PMUX_SOCKET:-}" ]]; then
  args+=(--socket "$PMUX_SOCKET")
fi

verb="${1:-status}"
shift || true
case "$verb" in
  start) verb=up ;;
esac
exec "$BIN" "${args[@]}" "$verb" "$@"
