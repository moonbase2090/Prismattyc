#!/usr/bin/env bash
# Run APS acceptance mutation against the Prismattyc Termwright features.
# Usage: ./scripts/acceptance-mutate.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.cargo/bin:${PATH}"

TOOLS_ROOT="${APS_TOOLS_ROOT:-$HOME/Projects/tools}"
BIN_DIR="${APS_BIN_DIR:-$TOOLS_ROOT/bin}"
MUTATOR="${APS_GHERKIN_MUTATOR:-$BIN_DIR/gherkin-mutator}"
ADAPTER="${APS_TERMWRIGHT_ADAPTER:-$BIN_DIR/aps-termwright}"
PRISMATTYC_BIN="${PRISMATTYC_BIN:-$ROOT/target/debug/prismattyc}"

if [[ ! -x "$MUTATOR" || ! -x "$ADAPTER" ]]; then
  echo "missing APS executable; build the tools under $TOOLS_ROOT first" >&2
  exit 2
fi
cargo build -p prismattyc --locked
if [[ ! -x "$PRISMATTYC_BIN" ]]; then
  echo "missing prismattyc binary: $PRISMATTYC_BIN" >&2
  exit 2
fi

"$ROOT/scripts/acceptance.sh"

run_mutation() {
  local name="$1"
  local feature="$ROOT/features/$name.feature"
  local generated="$ROOT/build/acceptance/generated/$name"
  local work="$ROOT/build/acceptance-mutation/$name"
  mkdir -p "$work"
  cp "$feature" "$work/feature.feature"
  local report status
  set +e
  report=$("$MUTATOR" \
    --feature "$work/feature.feature" \
    --work-dir "$work" \
    --generated-dir "$generated" \
    --workers 2 \
    --timeout 5m \
    --status-interval 0 \
    --level full \
    --runner-worker "$ADAPTER worker --prismattyc $PRISMATTYC_BIN")
  status=$?
  set -e
  printf '%s\n' "$report"
  MUTATION_STATUS="$status"
  MUTATION_REPORT="$report"
}

summary_value() {
  local summary="$1"
  local key="$2"
  for token in $summary; do
    if [[ "$token" == "$key="* ]]; then
      printf '%s\n' "${token#*=}"
      return
    fi
  done
  printf '0\n'
}

total=0
killed=0
survived=0
errors=0
for name in classic-shell classic-color; do
  echo "== mutation: $name =="
  run_mutation "$name"
  first_line="${MUTATION_REPORT%%$'\n'*}"
  total=$((total + $(summary_value "$first_line" total)))
  killed=$((killed + $(summary_value "$first_line" killed)))
  survived=$((survived + $(summary_value "$first_line" survived)))
  errors=$((errors + $(summary_value "$first_line" errors)))
  if [[ "$MUTATION_STATUS" -ne 0 ]]; then
    overall_status=1
  fi
done

echo "total=$total killed=$killed survived=$survived errors=$errors"
if [[ "${overall_status:-0}" -ne 0 || "$survived" -ne 0 || "$errors" -ne 0 ]]; then
  exit 1
fi
