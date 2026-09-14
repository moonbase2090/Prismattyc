#!/usr/bin/env bash
# Reclaim sticky zram swap (PT-305).
# This script only runs swapoff and swapon on /dev/zram* devices
# listed in /proc/swaps. Install a root-owned copy at
# /usr/local/sbin/prismattyc-la-reclaim-zram and allow that path
# in a passwordless sudoers or polkit rule. On Arch (Nexus) the
# admin group is wheel, not sudo. See docs/testing-policy.md.
set -euo pipefail

SWAPS="${LA_RECLAIM_SWAPS:-/proc/swaps}"

devices=()
if [[ -f "${SWAPS}" ]]; then
  while read -r name _rest; do
    [[ "${name}" == Filename ]] && continue
    [[ "${name}" == /dev/zram* ]] || continue
    devices+=("${name}")
  done < "${SWAPS}"
fi

if [[ "${1:-}" == "--dry-run" ]]; then
  if [[ "${#devices[@]}" -eq 0 ]]; then
    echo "zram: none"
    exit 0
  fi
  printf 'zram: %s\n' "${devices[@]}"
  exit 0
fi

if [[ "${#devices[@]}" -eq 0 ]]; then
  echo "zram: none"
  exit 0
fi

for device in "${devices[@]}"; do
  swapoff "${device}"
  swapon "${device}"
  echo "zram: reclaimed ${device}"
done
