#!/usr/bin/env bash
# Run one round-robin shard, or a current-head --in-diff pass, for CI.
# Usage: MUTANTS_SHARD=0/159 ./scripts/mutants-nightly.sh
#        MUTANTS_IN_DIFF_FILE=build/in-diff.patch ./scripts/mutants-nightly.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${MUTANTS_WORKSPACE:-$ROOT}"
cd "$WORKSPACE"

cargo_env="${CARGO_HOME:-$HOME/.cargo}/env"
if [ -f "$cargo_env" ]; then
  # shellcheck source=/dev/null
  . "$cargo_env"
elif [ -f "$HOME/.cargo/env" ]; then
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi
export PATH="${HOME}/.cargo/bin:${PATH}"

in_diff_file="${MUTANTS_IN_DIFF_FILE:-}"
if [ -n "$in_diff_file" ]; then
  shard="in-diff"
  out="build/mutants/in-diff"
else
  shard="${MUTANTS_SHARD:-0/1}"
  if [[ ! "$shard" =~ ^([0-9]+)/([1-9][0-9]*)$ ]]; then
    echo "error: MUTANTS_SHARD must have the form k/N, got '$shard'" >&2
    exit 2
  fi
  shard_index="${BASH_REMATCH[1]}"
  shard_count="${BASH_REMATCH[2]}"
  if (( shard_index >= shard_count )); then
    echo "error: shard index $shard_index must be less than $shard_count" >&2
    exit 2
  fi
  out="build/mutants/shard-${shard_index}"
fi

# Keep test tempfiles and cargo-mutants scratch on runner disk, not tmpfs.
MUTANTS_TMPDIR="${MUTANTS_TMPDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/mutants}"
mkdir -p "$MUTANTS_TMPDIR"
export TMPDIR="$MUTANTS_TMPDIR"
python3 "$ROOT/scripts/mutants-gate.py" --check-tmpdir
echo "TMPDIR=$TMPDIR"

HEAVY_LOCKED=0
cleanup_mutants_scratch() {
  python3 "$ROOT/scripts/mutants-nightly-process-cleanup.py" --phase after || true
  find "$TMPDIR" -maxdepth 1 -name 'cargo-mutants-*' -prune -exec rm -rf {} +
  if [ "$HEAVY_LOCKED" -eq 1 ]; then
    python3 "$ROOT/scripts/la-heavy-serial.py" --release --job mutants-nightly || true
  fi
}
trap cleanup_mutants_scratch EXIT

if [ "${LA_HEAVY_HELD:-}" != 1 ]; then
  python3 "$ROOT/scripts/la-heavy-serial.py" --acquire --wait --job mutants-nightly
  HEAVY_LOCKED=1
fi

python3 "$ROOT/scripts/mutants-gate.py" --check-host-headroom
python3 "$ROOT/scripts/mutants-nightly-process-cleanup.py" --phase before

rm -rf "$out"
mkdir -p "$(dirname "$out")"
timeout_seconds="${MUTANTS_TIMEOUT_SECONDS:-300}"
nextest_profile="${MUTANTS_NEXTTEST_PROFILE:-mutants}"
if [[ ! "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "error: MUTANTS_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi
case "$nextest_profile" in
  mutants|mutants-slow) ;;
  *) echo "error: unsupported MUTANTS_NEXTTEST_PROFILE '$nextest_profile'" >&2; exit 2 ;;
esac
oom_events="${MUTANTS_OOM_EVENTS:-/sys/fs/cgroup/memory.events}"
oom_before="$(python3 "$ROOT/scripts/mutants-gate.py" --read-oom-kill --oom-events "$oom_events")"

echo "== mutants nightly: ${shard} =="
echo "== nextest ${nextest_profile} profile; package-local tests; timeout ${timeout_seconds}s =="
set +e
mutant_args=(
  mutants --in-place --baseline=skip --sharding round-robin
  --timeout "$timeout_seconds" --profile mutants --test-tool nextest
  --test-workspace=false --workspace -vV --annotations=none
)
if [ -n "$in_diff_file" ]; then
  mutant_args+=(--in-diff "$in_diff_file")
else
  mutant_args+=(--shard "$shard")
fi
mutant_args+=(--output "$out" -- --profile "$nextest_profile")
cargo "${mutant_args[@]}"
mutants_status=$?
set -e

oom_after="$(python3 "$ROOT/scripts/mutants-gate.py" --read-oom-kill --oom-events "$oom_events")"
oom_args=(--is-runner-oom --status "$mutants_status")
if [ -n "$oom_before" ] && [ -n "$oom_after" ]; then
  oom_args+=(--oom-before "$oom_before" --oom-after "$oom_after")
fi
if python3 "$ROOT/scripts/mutants-gate.py" "${oom_args[@]}"; then
  exit 1
fi

outcomes="$out/mutants.out/outcomes.json"
if [ ! -f "$outcomes" ]; then
  echo "error: cargo mutants did not write $outcomes (exit $mutants_status)" >&2
  exit 1
fi

case "$mutants_status" in
  0|2|3) ;;
  *)
    echo "error: cargo mutants failed with exit $mutants_status" >&2
    exit 1
    ;;
esac

# Missed mutants and timeouts are report-only for this nightly run.
python3 "$ROOT/scripts/mutants-gate.py" --outcomes "$outcomes" --min-caught 0
