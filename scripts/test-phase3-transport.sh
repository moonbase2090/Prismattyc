#!/usr/bin/env bash
# Scripted capability transport rows T2 / T3 / T6.
# Not a PRD §5.6 pass. Requires tmux on PATH for T2/T3.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_TERM_COLOR=never

if ! command -v tmux >/dev/null 2>&1; then
  echo "phase3-transport: tmux missing; T2/T3 will skip inside the rust test" >&2
fi

echo "phase3-transport: prismattyc-protocol DCS wrapper + T2/T3/T6 fixtures"
cargo test --locked -p prismattyc-protocol tmux_passthrough -- --test-threads=1
cargo test --locked -p prismattyc --test transport_matrix -- --test-threads=1

echo "phase3-transport: PASS"
echo "phase3-transport: not a PRD §5.6 pass; A-6 remains deferred"
