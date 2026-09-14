#!/usr/bin/env bash
# Staged Local Actions PR run for the 32 GiB Nexus host (PT-305).
# Do not use one-shot `local-actions run --event pull_request` on that
# host. This script runs light, mutants, CRAP, then e2e. After each
# local-actions run it waits for a terminal status. It does not
# reclaim until that job has finished. It checks host headroom
# before each heavy stage.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

EVENT="${LA_EVENT:-pull_request}"
FROM_STAGE=""
ONLY=""
DRY=0
SKIP_RECLAIM=0
ALLOW_MISSING_ZRAM=0
RECLAIM_FIRST=1

usage() {
  cat <<'EOF'
Usage: scripts/la-staged-pr.sh [options]

Options:
  --from STAGE          Start at this stage (light, mutants, crap, e2e)
  --only STAGE          Run one stage
  --event EVENT         local-actions event (default: pull_request)
  --dry-run             Print the plan. Do not run jobs.
  --skip-reclaim        Skip container, scratch, and zram reclaim
  --allow-missing-zram  Continue when zram sudo is missing
  --no-reclaim-first    Do not reclaim leftovers before the first stage

Single-job debug still uses:
  local-actions run --event pull_request --job NAME
  local-actions status <id>
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --from) FROM_STAGE="${2:?}"; shift 2 ;;
    --only) ONLY="${2:?}"; shift 2 ;;
    --event) EVENT="${2:?}"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --skip-reclaim) SKIP_RECLAIM=1; shift ;;
    --allow-missing-zram) ALLOW_MISSING_ZRAM=1; shift ;;
    --no-reclaim-first) RECLAIM_FIRST=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "error: unknown option $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

plan_flags=()
[[ -n "${FROM_STAGE}" ]] && plan_flags+=(--from-stage "${FROM_STAGE}")
[[ -n "${ONLY}" ]] && plan_flags+=(--only "${ONLY}")
if [[ "${RECLAIM_FIRST}" -eq 0 ]]; then
  # build_run_plan always prepends startup reclaim. Filter in the loop.
  :
fi

mapfile -t steps < <(python3 "${ROOT}/scripts/la-staged.py" --plan "${plan_flags[@]+"${plan_flags[@]}"}")

if [[ "${#steps[@]}" -eq 0 ]]; then
  echo "error: empty staged plan" >&2
  exit 1
fi

reclaim() {
  local label="$1"
  if [[ "${SKIP_RECLAIM}" -eq 1 ]]; then
    echo "reclaim: skipped (${label})"
    return 0
  fi
  local extra=()
  [[ "${DRY}" -eq 1 ]] && extra+=(--dry-run)
  [[ "${ALLOW_MISSING_ZRAM}" -eq 1 ]] && extra+=(--allow-missing-zram)
  "${ROOT}/scripts/la-reclaim-host.sh" "${extra[@]+"${extra[@]}"}" "${label}"
}

check_headroom() {
  local stage="$1"
  if [[ "${DRY}" -eq 1 ]]; then
    echo "dry-run: python3 scripts/mutants-gate.py --check-host-headroom (${stage})"
    return 0
  fi
  if python3 "${ROOT}/scripts/mutants-gate.py" --check-host-headroom; then
    return 0
  fi
  echo "error: reclaim did not restore headroom (PT-305) before stage ${stage}" >&2
  return 1
}

run_job() {
  local job="$1"
  if [[ "${DRY}" -eq 1 ]]; then
    echo "dry-run: local-actions run --event ${EVENT} --job ${job}"
    echo "dry-run: wait for local-actions status <id> until succeeded/failed/cancelled/lost"
    return 0
  fi
  if ! command -v local-actions >/dev/null 2>&1; then
    echo "error: local-actions is not on PATH" >&2
    return 1
  fi
  python3 "${ROOT}/scripts/la-staged.py" --run-and-wait \
    --event "${EVENT}" --job "${job}"
}

failed=()
for line in "${steps[@]}"; do
  kind="${line%%$'\t'*}"
  rest="${line#*$'\t'}"
  stage="${rest%%$'\t'*}"
  detail="${rest#*$'\t'}"
  if [[ "${detail}" == "${rest}" ]]; then
    detail=""
  fi
  if [[ "${kind}" == reclaim && "${stage}" == startup && "${RECLAIM_FIRST}" -eq 0 ]]; then
    continue
  fi
  case "${kind}" in
    reclaim)
      reclaim "${stage}"
      ;;
    headroom)
      check_headroom "${stage}" || exit 1
      ;;
    run)
      echo "stage ${stage}: ${detail}"
      if ! run_job "${detail}"; then
        failed+=("${detail}")
      fi
      ;;
    *)
      echo "error: unknown plan step ${kind}" >&2
      exit 1
      ;;
  esac
done

if [[ "${#failed[@]}" -gt 0 ]]; then
  echo "error: staged Local Actions failed: ${failed[*]}" >&2
  exit 1
fi
echo "staged Local Actions: all selected stages passed"
