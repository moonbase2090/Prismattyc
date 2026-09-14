#!/usr/bin/env bash
# Run the APS acceptance features through the external Termwright adapter.
# Usage: ./scripts/acceptance.sh [classic-shell|classic-color]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.cargo/bin:${PATH}"

TOOLS_ROOT="${APS_TOOLS_ROOT:-$HOME/Projects/tools}"
BIN_DIR="${APS_BIN_DIR:-$TOOLS_ROOT/bin}"
PARSER="${APS_GHERKIN_PARSER:-$BIN_DIR/gherkin-parser}"
DRY_CHECKER="${APS_GHERKIN_DRY_CHECKER:-$BIN_DIR/gherkin-ir-dry-checker}"
ADAPTER="${APS_TERMWRIGHT_ADAPTER:-$BIN_DIR/aps-termwright}"
TERMWRIGHT_BIN="${TERMWRIGHT_BIN:-termwright}"
PRISMATTYC_BIN="${PRISMATTYC_BIN:-$ROOT/target/debug/prismattyc}"
FILTER="${1:-}"

for tool in "$PARSER" "$DRY_CHECKER" "$ADAPTER"; do
  if [[ ! -x "$tool" ]]; then
    echo "missing executable: $tool" >&2
    echo "Build the external APS tools under $TOOLS_ROOT first." >&2
    exit 2
  fi
done
if ! command -v "$TERMWRIGHT_BIN" >/dev/null 2>&1; then
  echo "termwright is not on PATH" >&2
  exit 2
fi

cargo build -p prismattyc --locked
if [[ ! -x "$PRISMATTYC_BIN" ]]; then
  echo "missing prismattyc binary: $PRISMATTYC_BIN" >&2
  exit 2
fi

run_feature() {
  local name="$1"
  local feature="$ROOT/features/$name.feature"
  local ir="$ROOT/build/acceptance/ir/$name.json"
  local dry="$ROOT/build/acceptance/dry/$name.json"
  local generated="$ROOT/build/acceptance/generated/$name"
  if [[ ! -f "$feature" ]]; then
    echo "missing feature: $feature" >&2
    return 2
  fi
  mkdir -p "$ROOT/build/acceptance/ir" "$ROOT/build/acceptance/dry"
  "$PARSER" "$feature" "$ir"
  "$DRY_CHECKER" "$ir" "$dry"
  APS_FEATURE_PATH="features/$name.feature" "$ADAPTER" \
    acceptance-entrypoint-generator "$ir" "$generated"
  APS_FEATURE_PATH="features/$name.feature" "$ADAPTER" \
    run "$ir" "$generated" --prismattyc "$PRISMATTYC_BIN" --termwright "$TERMWRIGHT_BIN"
  echo "PASS: $name"
}

ran=0
for name in classic-shell classic-color; do
  if [[ -n "$FILTER" && "$FILTER" != "$name" && "$FILTER" != "$name.feature" ]]; then
    continue
  fi
  ran=$((ran + 1))
  echo "== acceptance: $name =="
  run_feature "$name"
done

if [[ "$ran" -eq 0 ]]; then
  echo "no acceptance features matched: ${FILTER:-<all>}" >&2
  exit 2
fi
