#!/usr/bin/env bash
set -euo pipefail

target="${1:?Usage: build-linux.sh TARGET}"
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
  *)
    printf 'unsupported Linux release target: %s\n' "$target" >&2
    exit 2
    ;;
esac

CARGO_ZIGBUILD_ZIG_PATH=python-zig cargo zigbuild \
  --release --locked --workspace --bins \
  --target "$target.2.28"
