#!/usr/bin/env bash
# Run Prismattyc nested-host E2E scenarios via Termwright.
# Usage:
#   ./scripts/termwright-e2e.sh              # all scenarios
#   ./scripts/termwright-e2e.sh classic-shell
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.cargo/bin:${PATH}"

termwright_bin="${TERMWRIGHT_BIN:-termwright}"
if ! command -v "$termwright_bin" >/dev/null 2>&1; then
  echo "termwright not on PATH; installing 0.2.0 via cargo..."
  cargo install termwright --locked --version 0.2.0
  termwright_bin="${CARGO_HOME:-$HOME/.cargo}/bin/termwright"
fi

echo "== termwright $($termwright_bin --version 2>/dev/null || echo unknown) =="

echo "== cargo build -p prismattyc --locked =="
cargo build -p prismattyc --locked
PRISMATTYC_BIN="${PRISMATTYC_BIN:-$ROOT/target/debug/prismattyc}"
if [[ ! -x "$PRISMATTYC_BIN" ]]; then
  echo "missing prismattyc binary: $PRISMATTYC_BIN" >&2
  exit 2
fi

ART_ROOT="$ROOT/e2e/artifacts"
mkdir -p "$ART_ROOT"
RUN_ID="$(date +%Y%m%d-%H%M%S)"
RUN_DIR="$ART_ROOT/$RUN_ID"
mkdir -p "$RUN_DIR"

filter="${1:-}"
failed=0
ran=0

run_one() {
  local name="$1"
  if [[ "$name" == classic-reflow ]]; then
    python3 "$ROOT/e2e/classic-reflow.py" --termwright "$termwright_bin" --binary "$PRISMATTYC_BIN" --out "$RUN_DIR/classic-reflow"
    return $?
  fi
  local src="$ROOT/e2e/${name}.yaml"
  if [[ ! -f "$src" ]]; then
    echo "missing scenario: $src" >&2
    return 2
  fi
  local work="$RUN_DIR/$name"
  mkdir -p "$work"
  # Rewrite command → absolute binary; artifacts → this run dir.
  python3 - "$src" "$PRISMATTYC_BIN" "$work" <<'PY'
import sys, pathlib, re
src, prism, art = sys.argv[1:]
text = pathlib.Path(src).read_text()
text = re.sub(
    r"(?m)^(  command:\s*).*$",
    r'\1' + prism,
    text,
    count=1,
)
text = re.sub(
    r"(?m)^(  dir:\s*).*$",
    r"  dir: " + art,
    text,
    count=1,
)
out = pathlib.Path(art) / "steps.yaml"
out.write_text(text)
print(out)
PY
  local yaml="$work/steps.yaml"
  echo
  echo "== scenario: $name =="
  if "$termwright_bin" run-steps --trace "$yaml"; then
    echo "PASS: $name"
    # Promote PNGs to run root for easy browsing
    find "$work" -name '*.png' -exec cp -n {} "$RUN_DIR/" \; 2>/dev/null || true
    return 0
  else
    echo "FAIL: $name" >&2
    return 1
  fi
}

scenarios=(classic-shell classic-color classic-keys classic-reflow a6-nested-marker rich-attach pt-176-key-burst)
for s in "${scenarios[@]}"; do
  if [[ -n "$filter" && "$filter" != "$s" && "$filter" != "${s}.yaml" ]]; then
    continue
  fi
  ran=$((ran + 1))
  if ! run_one "$s"; then
    failed=$((failed + 1))
  fi
done

if [[ "$ran" -eq 0 ]]; then
  echo "no scenarios matched filter: ${filter:-<all>}" >&2
  exit 2
fi

echo
echo "=========================================="
echo " Termwright E2E: ran=$ran failed=$failed"
echo " artifacts: $RUN_DIR"
echo " screenshots: $(find "$RUN_DIR" -name '*.png' | wc -l)"
echo "=========================================="
ls -la "$RUN_DIR"/*.png 2>/dev/null || true

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi
