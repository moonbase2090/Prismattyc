#!/usr/bin/env bash
set -euo pipefail

bin_dir="${1:?Usage: smoke-linux-al2023.sh BIN_DIR}"
bin_dir="$(cd "$bin_dir" && pwd)"

for name in pmux pmuxd pmux-attach pmux-mcp prismattyc prismattyc-host; do
  if [[ ! -x "$bin_dir/$name" ]]; then
    printf 'missing executable: %s/%s\n' "$bin_dir" "$name" >&2
    exit 1
  fi
done

docker run --rm --interactive \
  --volume "$bin_dir:/release:ro" \
  amazonlinux:2023 \
  bash -s <<'AL2023'
set -euo pipefail

dnf --assumeyes install binutils

for name in pmux pmuxd pmux-attach pmux-mcp prismattyc prismattyc-host; do
  binary="/release/$name"
  max_version="$(objdump -T "$binary" \
    | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' \
    | sed 's/^GLIBC_//' \
    | sort -V \
    | tail -n 1)"
  printf '%s max GLIBC_%s\n' "$name" "$max_version"
  if [[ "$(printf '%s\n' "$max_version" 2.28 | sort -V | tail -n 1)" != 2.28 ]]; then
    printf '%s requires GLIBC_%s, above the 2.28 release floor\n' \
      "$name" "$max_version" >&2
    exit 1
  fi
done

printf '\nAmazon Linux 2023 smoke commands (%s)\n' "$(uname -m)"
/release/prismattyc --version
/release/pmux --version
/release/pmuxd --version
AL2023
