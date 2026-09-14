#!/usr/bin/env bash
# Nightly full cargo-mutants run, one crate at a time (PT-225).
# Usage: ./scripts/mutants-nightly.sh [crate...]
#
# With no arguments, runs every workspace crate under crates/.
# Writes build/mutants/<crate>/outcomes.json. Does not fail on missed
# mutants. A failed unmutated baseline still fails that crate.
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

# cargo-mutants copies the tree under TMPDIR. Host /tmp is tmpfs
# (Nexus) and fills with multi-GiB leftovers. Prefer the durable SSD
# cache. Refuse /tmp and any tmpfs mount, not only the path /tmp
# (PT-305).
MUTANTS_TMPDIR="${MUTANTS_TMPDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/mutants}"
mkdir -p "${MUTANTS_TMPDIR}"
export TMPDIR="${MUTANTS_TMPDIR}"
python3 "${ROOT}/scripts/mutants-gate.py" --check-tmpdir
echo "TMPDIR=${TMPDIR}"
HEAVY_LOCKED=0
cleanup_mutants_scratch() {
  find "${TMPDIR}" -maxdepth 1 -name 'cargo-mutants-*' -prune -exec rm -rf {} +
  if [ "${HEAVY_LOCKED}" -eq 1 ]; then
    python3 "${ROOT}/scripts/la-heavy-serial.py" --release --job mutants-nightly || true
  fi
}
trap cleanup_mutants_scratch EXIT

if [ "${LA_HEAVY_HELD:-}" != 1 ]; then
  python3 "${ROOT}/scripts/la-heavy-serial.py" --acquire --wait --job mutants-nightly
  HEAVY_LOCKED=1
fi

python3 "${ROOT}/scripts/mutants-gate.py" --check-host-headroom

if [ "$#" -gt 0 ]; then
  crates=("$@")
else
  mapfile -t crates < <(
    python3 "${ROOT}/scripts/mutants-gate.py" --list-all-crates --repo "${ROOT}"
  )
fi

mkdir -p build/mutants
status=0
for crate in "${crates[@]}"; do
  echo "== mutants nightly: ${crate} =="
  out="build/mutants/${crate}"
  rm -rf "${out}"
  JOBS="${MUTANTS_JOBS:-1}"
  OOM_EVENTS="${MUTANTS_OOM_EVENTS:-/sys/fs/cgroup/memory.events}"
  oom_before="$(python3 "${ROOT}/scripts/mutants-gate.py" --read-oom-kill --oom-events "${OOM_EVENTS}")"
  set +e
  cargo mutants --jobs "${JOBS}" --no-shuffle -vV --annotations=none \
    -p "${crate}" \
    --output "${out}" \
    -- -- --test-threads=1
  crate_status=$?
  set -e
  oom_after="$(python3 "${ROOT}/scripts/mutants-gate.py" --read-oom-kill --oom-events "${OOM_EVENTS}")"
  is_oom_args=(--is-runner-oom --status "${crate_status}")
  if [ -n "${oom_before}" ] && [ -n "${oom_after}" ]; then
    is_oom_args+=(--oom-before "${oom_before}" --oom-after "${oom_after}")
  fi
  if python3 "${ROOT}/scripts/mutants-gate.py" "${is_oom_args[@]}"; then
    # Record the crate and keep going so later crates still archive.
    status=1
    continue
  fi
  # --output DIR creates DIR/mutants.out/.
  outcomes="${out}/mutants.out/outcomes.json"
  if [ ! -f "${outcomes}" ]; then
    echo "error: cargo mutants did not write ${outcomes} (exit ${crate_status})" >&2
    status=1
    continue
  fi
  # Report only. Nightly archives the tree; it is not the 60% merge gate.
  python3 "${ROOT}/scripts/mutants-gate.py" \
    --outcomes "${outcomes}" \
    --min-caught 0 || status=1
done

exit "${status}"
