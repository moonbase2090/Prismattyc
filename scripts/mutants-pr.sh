#!/usr/bin/env bash
# PR cargo-mutants gate (PT-225).
# Usage: ./scripts/mutants-pr.sh
#
# Writes origin/<base>...HEAD to build/git.diff, runs cargo mutants
# with render routing and remainder shards per touched crate, then
# scripts/mutants-gate.py (60% caught per crate when scored >= 5).
# Two crates in one invocation hit the 8g job cap (PT-286). Do not
# raise MUTANTS_MEMORY to combine them. Refuse to start when host
# RAM or swap is below the PT-305 floor.
# Skips (exit 0) when the diff is empty, has no mutatable Rust, or
# cargo-mutants exits 0 with "No mutants to filter" and no outcomes.json
# (test-only change in a touched crate; PT-266).
# Missing git metadata is a failure, not a skip. Act copies a worktree
# overlay whose .git gitdir is on the host; the fallback runs git in a
# sidecar that bind-mounts that gitdir.
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
    python3 "${ROOT}/scripts/la-heavy-serial.py" --release --job mutants || true
  fi
}
trap cleanup_mutants_scratch EXIT

# When CI already holds the lock (LA_HEAVY_HELD=1), do not acquire or
# release here. The job Release step uses the same owner file as Acquire.
# Skip paths (no mutatable Rust) must leave that token in place.
if [ "${LA_HEAVY_HELD:-}" != 1 ]; then
  python3 "${ROOT}/scripts/la-heavy-serial.py" --acquire --wait --job mutants
  HEAVY_LOCKED=1
fi

python3 "${ROOT}/scripts/mutants-gate.py" --check-host-headroom

# act copies the tree and may set GIT_DIR to a broken value.
unset GIT_DIR GIT_WORK_TREE

MIN_CAUGHT="${MUTANTS_MIN_CAUGHT:-60}"
MIN_SCORED="${MUTANTS_MIN_SCORED:-5}"
DIFF_FILE="${MUTANTS_DIFF_FILE:-build/git.diff}"
# cargo mutants --output DIR creates DIR/mutants.out/. Per crate.
OUT_PARENT="${MUTANTS_OUT_PARENT:-.}"
BASE_REF="${MUTANTS_BASE_REF:-${GITHUB_BASE_REF:-main}}"
BASE_REF="${BASE_REF#refs/heads/}"
RANGE="origin/${BASE_REF}...HEAD"

mkdir -p build

dkr() {
  if docker info >/dev/null 2>&1; then
    docker "$@"
  elif sudo -n docker info >/dev/null 2>&1; then
    sudo -n docker "$@"
  else
    echo "error: docker is not available" >&2
    return 1
  fi
}

main_git_from_git_file() {
  local gitdir main_git
  gitdir="$(sed -n 's/^gitdir:[[:space:]]*//p' .git | tr -d '\r')"
  if [ -z "${gitdir}" ]; then
    echo "error: .git file has no gitdir: line" >&2
    return 1
  fi
  case "${gitdir}" in
    */.git/worktrees/*)
      main_git="${gitdir%%/.git/worktrees/*}/.git"
      ;;
    *)
      echo "error: unsupported gitdir: ${gitdir}" >&2
      return 1
      ;;
  esac
  printf '%s\n' "${main_git}"
}

git_ok() {
  git rev-parse --is-inside-work-tree >/dev/null 2>&1
}

write_diff_local() {
  if git rev-parse --verify "origin/${BASE_REF}" >/dev/null 2>&1; then
    :
  else
    git fetch --no-tags origin "${BASE_REF}"
  fi
  git diff "${RANGE}" >"${DIFF_FILE}"
}

write_diff_host_docker() {
  local main_git image
  main_git="$(main_git_from_git_file)"
  image="${MUTANTS_GIT_IMAGE:-local-actions-runner:latest}"
  if ! dkr image inspect "${image}" >/dev/null 2>&1; then
    echo "error: image ${image} is missing (need git sidecar)" >&2
    return 1
  fi
  echo "git overlay missing; computing ${RANGE} via host sidecar (${main_git})"
  dkr run --rm \
    -v "${main_git}:${main_git}" \
    -v "${ROOT}:${ROOT}" \
    -w "${ROOT}" \
    -e HOME=/tmp \
    "${image}" \
    bash -lc "
      set -euo pipefail
      git -c safe.directory=${ROOT} rev-parse --is-inside-work-tree >/dev/null
      git -c safe.directory=${ROOT} rev-parse --verify origin/${BASE_REF} >/dev/null \
        || git -c safe.directory=${ROOT} fetch --no-tags origin ${BASE_REF}
      git -c safe.directory=${ROOT} diff ${RANGE}
    " >"${DIFF_FILE}"
}

if [ "${MUTANTS_FORCE_HOST_GIT:-}" = 1 ] || ! git_ok; then
  if [ ! -f .git ]; then
    echo "error: no git metadata in this runner; cannot compute --in-diff" >&2
    exit 1
  fi
  write_diff_host_docker
else
  write_diff_local
fi

if [ ! -s "${DIFF_FILE}" ]; then
  echo "no diff vs origin/${BASE_REF}; mutants PR gate skipped"
  exit 0
fi

crate_list="$(python3 "${ROOT}/scripts/mutants-gate.py" \
  --list-crates --repo "${ROOT}" --diff "${DIFF_FILE}")"
crates=()
if [ -n "${crate_list}" ]; then
  mapfile -t crates <<<"${crate_list}"
fi

if [ "${#crates[@]}" -eq 0 ]; then
  echo "no mutatable Rust in workspace crates; mutants PR gate skipped"
  exit 0
fi

echo "touched crates: ${crates[*]}"
echo "one crate at a time; render first pass, remainder shards, full-suite fallback"

# --jobs 1: parallel mutants OOM the host crate (PT-255). Exit 137 is
# only one OOM signal; the kernel usually kills rustc, not cargo-mutants
# (PT-261). Sample cgroup oom_kill before and after each crate.
if [ "${MUTANTS_JOBS:-1}" != 1 ]; then
  echo "error: PR mutation routing requires MUTANTS_JOBS=1" >&2
  exit 1
fi
OOM_EVENTS="${MUTANTS_OOM_EVENTS:-/sys/fs/cgroup/memory.events}"
failed=0
# cargo-mutants forwards args after `--` to `cargo test`. A second `--`
# sends `--test-threads=1` to the test harness. One `--` makes cargo
# reject the flag. Serialize tests: the mux suite flakes under parallel
# load (oversized_frame ConnectionReset, PT-249).
for crate in "${crates[@]}"; do
  echo "== mutants PR: ${crate} =="
  out="${OUT_PARENT}/build/mutants-pr/${crate}"
  rm -rf "${out}"
  mkdir -p "${out}"
  log="${out}/mutants-run.log"
  oom_before="$(python3 "${ROOT}/scripts/mutants-gate.py" --read-oom-kill --oom-events "${OOM_EVENTS}")"
  set +e
  python3 "${ROOT}/scripts/mutants-route.py" \
    --repo "${ROOT}" --diff "${DIFF_FILE}" --out "${out}" \
    --crate "${crate}" --oom-events "${OOM_EVENTS}" \
    2>&1 | tee "${log}"
  crate_status=${PIPESTATUS[0]}
  set -e
  oom_after="$(python3 "${ROOT}/scripts/mutants-gate.py" --read-oom-kill --oom-events "${OOM_EVENTS}")"
  is_oom_args=(--is-runner-oom --status "${crate_status}")
  if [ -n "${oom_before}" ] && [ -n "${oom_after}" ]; then
    is_oom_args+=(--oom-before "${oom_before}" --oom-after "${oom_after}")
  fi
  if python3 "${ROOT}/scripts/mutants-gate.py" "${is_oom_args[@]}"; then
    echo "error: runner OOM on crate ${crate}; not raising MUTANTS_MEMORY" >&2
    exit 137
  fi
  # A later phase can fail after an earlier green report. Never score that
  # report or let the small-sample floor turn a routing failure into a pass.
  if [ "${crate_status}" -ne 0 ]; then
    echo "error: mutation routing failed for ${crate} (exit ${crate_status})" >&2
    failed=1
    continue
  fi
  outcomes="${out}/mutants.out/outcomes.json"
  if [ ! -f "${outcomes}" ]; then
    no_mutants_args=()
    if python3 "${ROOT}/scripts/mutants-gate.py" --log-has-no-mutants --log "${log}"; then
      no_mutants_args+=(--no-mutants)
    fi
    set +e
    python3 "${ROOT}/scripts/mutants-gate.py" \
      --missing-outcomes \
      --status "${crate_status}" \
      "${no_mutants_args[@]}"
    miss=$?
    set -e
    if [ "${miss}" -eq 137 ]; then
      echo "error: runner OOM on crate ${crate}; not raising MUTANTS_MEMORY" >&2
      exit 137
    fi
    if [ "${miss}" -ne 0 ]; then
      failed=1
    fi
    continue
  fi
  if ! python3 "${ROOT}/scripts/mutants-gate.py" \
    --outcomes "${outcomes}" \
    --min-caught "${MIN_CAUGHT}" \
    --min-scored "${MIN_SCORED}"; then
    failed=1
  fi
done

if [ "${failed}" -ne 0 ]; then
  exit 1
fi
