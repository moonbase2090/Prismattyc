#!/usr/bin/env bash
# Between-stage Local Actions reclaim on the 32 GiB host (PT-305).
# Tear down leftover act containers, drop a stale heavy-job lock,
# clear SSD mutants scratch, then reclaim zram.
# Never writes mutants leftovers into host /tmp.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DRY=0
ALLOW_MISSING_ZRAM=0
STAGE="all"

usage() {
  cat <<'EOF'
Usage: scripts/la-reclaim-host.sh [--dry-run] [--allow-missing-zram] [stage]

stage is a label for logs. It does not change which leftovers are cleared.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --allow-missing-zram) ALLOW_MISSING_ZRAM=1; shift ;;
    -h|--help) usage; exit 0 ;;
    -*)
      echo "error: unknown option $1" >&2
      exit 2
      ;;
    *) STAGE="$1"; shift ;;
  esac
done

echo "reclaim: stage=${STAGE}"

docker_bin=()
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  docker_bin=(docker)
elif command -v docker >/dev/null 2>&1 && sudo -n docker info >/dev/null 2>&1; then
  docker_bin=(sudo -n docker)
fi

run() {
  if [[ "${DRY}" -eq 1 ]]; then
    printf 'dry-run:'
    printf ' %q' "$@"
    printf '\n'
    return 0
  fi
  "$@"
}

containers=()
running_heavy=()
lock_present=0
if [[ "${#docker_bin[@]}" -gt 0 ]]; then
  while read -r name; do
    [[ -n "${name}" ]] || continue
    containers+=("${name}")
    if [[ "${name}" == prismattyc-la-heavy ]]; then
      lock_present=1
    fi
  done < <("${docker_bin[@]}" ps -a --format '{{.Names}}' 2>/dev/null || true)
  while read -r name; do
    [[ -n "${name}" ]] || continue
    [[ "${name}" == prismattyc-la-heavy ]] && continue
    labels="$("${docker_bin[@]}" inspect -f '{{index .Config.Labels "prismattyc.la.job"}}' "${name}" 2>/dev/null || true)"
    env_blob="$("${docker_bin[@]}" inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "${name}" 2>/dev/null || true)"
    if [[ -n "${labels}" ]] || printf '%s\n' "${env_blob}" | grep -qx 'LA_HEAVY=1'; then
      running_heavy+=("${name}")
    fi
  done < <("${docker_bin[@]}" ps --format '{{.Names}}' 2>/dev/null || true)
fi

mapfile -t scratch_dirs < <(python3 "${ROOT}/scripts/la-staged.py" --scratch-dirs)

zram_helper="/usr/local/sbin/prismattyc-la-reclaim-zram"
if [[ ! -x "${zram_helper}" ]]; then
  zram_helper="${ROOT}/scripts/la-reclaim-zram.sh"
fi

zram_devices=()
if [[ -f /proc/swaps ]]; then
  while read -r name _rest; do
    [[ "${name}" == /dev/zram* ]] && zram_devices+=("${name}")
  done < /proc/swaps
fi

export LA_RECLAIM_LOCK_PRESENT="${lock_present}"
export LA_RECLAIM_ZRAM_HELPER="${zram_helper}"
export LA_RECLAIM_CONTAINERS="$(printf '%s\n' "${containers[@]+"${containers[@]}"}")"
export LA_RECLAIM_RUNNING_HEAVY="$(printf '%s\n' "${running_heavy[@]+"${running_heavy[@]}"}")"
export LA_RECLAIM_SCRATCH="$(printf '%s\n' "${scratch_dirs[@]+"${scratch_dirs[@]}"}")"
export LA_RECLAIM_ZRAM="$(printf '%s\n' "${zram_devices[@]+"${zram_devices[@]}"}")"

mapfile -t plan < <(python3 - <<PY
import importlib.util
import os
from pathlib import Path
root = Path("${ROOT}")
spec = importlib.util.spec_from_file_location("la_staged", root / "scripts" / "la-staged.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
actions = mod.plan_reclaim(
    containers=[c for c in os.environ.get("LA_RECLAIM_CONTAINERS", "").splitlines() if c],
    lock_present=os.environ.get("LA_RECLAIM_LOCK_PRESENT") == "1",
    running_heavy=[c for c in os.environ.get("LA_RECLAIM_RUNNING_HEAVY", "").splitlines() if c],
    scratch_dirs=[s for s in os.environ.get("LA_RECLAIM_SCRATCH", "").splitlines() if s],
    zram_devices=[z for z in os.environ.get("LA_RECLAIM_ZRAM", "").splitlines() if z],
    zram_helper=os.environ["LA_RECLAIM_ZRAM_HELPER"],
)
for kind, target in actions:
    print(f"{kind}\t{target}")
PY
)

if [[ "${#plan[@]}" -eq 0 ]]; then
  echo "reclaim: nothing to do"
  exit 0
fi

for line in "${plan[@]}"; do
  kind="${line%%$'\t'*}"
  target="${line#*$'\t'}"
  case "${kind}" in
    stop-container)
      echo "reclaim: stop ${target}"
      if [[ "${#docker_bin[@]}" -gt 0 ]]; then
        run "${docker_bin[@]}" rm -f "${target}" || true
      else
        echo "reclaim: docker unavailable; skip ${target}"
      fi
      ;;
    drop-stale-lock)
      echo "reclaim: drop stale ${target}"
      if [[ "${#docker_bin[@]}" -gt 0 ]]; then
        run "${docker_bin[@]}" rm -f "${target}" || true
      fi
      lock_dir="${LA_HEAVY_LOCK_DIR:-}"
      if [[ -z "${lock_dir}" ]]; then
        if [[ -n "${XDG_RUNTIME_DIR:-}" && "${XDG_RUNTIME_DIR}" != /tmp ]]; then
          lock_dir="${XDG_RUNTIME_DIR}/prismattyc-la"
        else
          lock_dir="${XDG_CACHE_HOME:-${HOME}/.cache}/prismattyc/la-heavy"
        fi
      fi
      if [[ -e "${lock_dir}/heavy.json" || -e "${lock_dir}/held" ]]; then
        run rm -f "${lock_dir}/heavy.json" "${lock_dir}/heavy.lock" "${lock_dir}/heavy.holder" || true
        run rmdir "${lock_dir}/held" 2>/dev/null || true
      fi
      ;;
    clear-scratch)
      if [[ "${target}" == /tmp || "${target}" == /tmp/* ]]; then
        echo "error: refuse to clear scratch under /tmp: ${target}" >&2
        exit 1
      fi
      echo "reclaim: clear cargo-mutants-* in ${target}"
      if [[ "${DRY}" -eq 1 ]]; then
        echo "dry-run: find ${target} -maxdepth 1 -name cargo-mutants-* -prune -exec rm -rf {} +"
      elif [[ -d "${target}" ]]; then
        find "${target}" -maxdepth 1 -name 'cargo-mutants-*' -prune -exec rm -rf {} +
      fi
      ;;
    zram)
      echo "reclaim: zram via ${target}"
      if [[ "${DRY}" -eq 1 ]]; then
        if [[ -x "${target}" ]]; then
          LA_RECLAIM_SWAPS="${LA_RECLAIM_SWAPS:-/proc/swaps}" "${target}" --dry-run || true
        else
          echo "dry-run: ${target} --dry-run"
        fi
        continue
      fi
      if sudo -n -l -- "${target}" >/dev/null 2>&1; then
        sudo -n -- "${target}"
      elif [[ ! -e /proc/swaps ]] || ! grep -q '^/dev/zram' /proc/swaps; then
        echo "reclaim: no zram swap; skip"
      elif [[ "${ALLOW_MISSING_ZRAM}" -eq 1 ]]; then
        echo "reclaim: zram helper needs passwordless sudo; skipped"
      else
        echo "error: zram reclaim needs passwordless sudo for ${target}" >&2
        echo "install scripts/la-reclaim-zram.sh at /usr/local/sbin/prismattyc-la-reclaim-zram" >&2
        echo "and add a sudoers rule for %wheel on Arch. See docs/testing-policy.md" >&2
        exit 1
      fi
      ;;
    *)
      echo "error: unknown reclaim action ${kind}" >&2
      exit 1
      ;;
  esac
done
