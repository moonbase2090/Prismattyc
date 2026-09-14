#!/usr/bin/env bash
# Classic claim (prismattyc-classic/0.1.1) local test gate.
# Mirrors docs/fidelity-matrix-v1.md F9 + optional interactive smoke.
#
# Usage:
#   ./scripts/test-phase1.sh              # automated gate only
#   ./scripts/test-phase1.sh --interactive  # gate, then open prism under /bin/sh
#   ./scripts/test-phase1.sh --quick      # test + build only (skip fmt/clippy/doc)
#   ./scripts/test-phase1.sh --help
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

QUICK=0
INTERACTIVE=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    --interactive|-i) INTERACTIVE=1 ;;
    --help|-h)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

step() {
  echo
  echo "==> $*"
}

fail() {
  echo
  echo "FAIL: $*" >&2
  exit 1
}

pass_banner() {
  echo
  echo "=========================================="
  echo " Classic claim automated gate: PASS"
  echo " tip: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo " claim: prismattyc-classic/0.1.1 (F1–F19)"
  echo " matrix: docs/fidelity-matrix-v1.md"
  echo "=========================================="
}

# --- automated gate (matrix F9) ---------------------------------------------

if [[ "$QUICK" -eq 0 ]]; then
  step "cargo fmt --all -- --check"
  cargo fmt --all -- --check

  step "cargo check --workspace --locked"
  cargo check --workspace --locked
fi

step "cargo test --workspace --locked"
cargo test --workspace --locked

if [[ "$QUICK" -eq 0 ]]; then
  step "cargo clippy --workspace --all-targets --locked -- -D warnings"
  cargo clippy --workspace --all-targets --locked -- -D warnings
fi

step "cargo build --bin prismattyc --locked"
cargo build --bin prismattyc --locked

if [[ "$QUICK" -eq 0 ]]; then
  step "cargo doc --workspace --no-deps"
  cargo doc --workspace --no-deps
fi

# Optional: whitespace check on Phase 1 span (ignore if base missing)
if git rev-parse --verify 534284b >/dev/null 2>&1; then
  step "git diff --check 534284b..HEAD"
  git diff --check 534284b..HEAD || fail "git diff --check reported issues"
fi

pass_banner

# --- interactive smoke (manual) ---------------------------------------------

if [[ "$INTERACTIVE" -eq 1 ]]; then
  if [[ ! -t 0 || ! -t 1 ]]; then
    fail "interactive smoke needs a real TTY (stdin+stdout). Run from a real terminal, not a pipe."
  fi
  echo
  echo "Interactive smoke (you drive) — prismattyc-classic/0.1.1:"
  echo "  1. printf 'hello\\n'     — text appears"
  echo "  2. printf '\\e[38;2;255;0;0mred\\e[0m\\n'  — F10 truecolor (outer host must support)"
  echo "  3. resize window         — grid follows (F3)"
  echo "  4. drag-select; Ctrl+Shift+C or multi-cell Ctrl+C — F6/F7 / OSC 52"
  echo "  5. flood lines; wheel or Shift+PageUp — F18 scrollback; Shift-drag select in history"
  echo "  6. Ctrl+Shift+; find (Kitty-safe); Enter next; Esc exit"
  echo "  7. printf '中 👨‍👩‍👧‍👦 🇺🇸\\n' — F14 wide + ZWJ + flag"
  echo "  8. vim/htop: plain mouse → app; Shift-drag → host select (F13 hybrid)"
  echo "  9. exit                  — host TTY restored (not stuck raw/alt)"
  echo "  Claimed rows: F1–F19 (see docs/fidelity-matrix-v1.md)"
  echo "  Often stolen by outer host: Ctrl+Space mark, Super chords, Ctrl+Shift+C"
  echo
  exec cargo run -p prismattyc --locked -- /bin/sh
fi

echo
echo "Interactive (optional, real TTY only):"
echo "  ./scripts/test-phase1.sh --interactive"
echo "  # or: cargo run -p prismattyc --locked -- /bin/sh"
