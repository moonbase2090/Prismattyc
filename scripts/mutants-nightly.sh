#!/usr/bin/env bash
# Run one full-workspace cargo-mutants shard for the nightly workflow.
# Usage: MUTANTS_SHARD=0/8 ./scripts/mutants-nightly.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo_env="${CARGO_HOME:-$HOME/.cargo}/env"
if [ -f "$cargo_env" ]; then
  # shellcheck source=/dev/null
  . "$cargo_env"
elif [ -f "$HOME/.cargo/env" ]; then
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi
export PATH="${HOME}/.cargo/bin:${PATH}"

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

# cargo-mutants copies the tree under TMPDIR. Keep its scratch off tmpfs.
MUTANTS_TMPDIR="${MUTANTS_TMPDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/mutants}"
mkdir -p "$MUTANTS_TMPDIR"
export TMPDIR="$MUTANTS_TMPDIR"
python3 "$ROOT/scripts/mutants-gate.py" --check-tmpdir
echo "TMPDIR=$TMPDIR"

HEAVY_LOCKED=0
cleanup_mutants_scratch() {
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

out="build/mutants/shard-${shard_index}"
rm -rf "$out"
mkdir -p "$(dirname "$out")"
jobs="${MUTANTS_JOBS:-1}"
oom_events="${MUTANTS_OOM_EVENTS:-/sys/fs/cgroup/memory.events}"
oom_before="$(python3 "$ROOT/scripts/mutants-gate.py" --read-oom-kill --oom-events "$oom_events")"

echo "== mutants nightly: shard ${shard} =="
set +e
cargo mutants --jobs "$jobs" --no-shuffle -vV --annotations=none \
  --shard "$shard" \
  --output "$out" \
  -- -- --test-threads=1
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
