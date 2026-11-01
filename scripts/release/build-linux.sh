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

export PRISMATTYC_PLUGIN_BUNDLE_DIR="$(python3 scripts/release/plugin_bundle_provenance.py prepare)"
export CARGO_ENCODED_RUSTFLAGS="$(python3 scripts/release/plugin_bundle_provenance.py flags --target "$target")"

CARGO_ZIGBUILD_ZIG_PATH=zig cargo zigbuild \
  --release --locked --workspace --bins \
  --target "$target.2.28"
# The activity monitor keeps its own lockfile outside the workspace. A shared
# target directory puts it beside the other release binaries.
CARGO_ZIGBUILD_ZIG_PATH=zig cargo zigbuild \
  --release --locked \
  --manifest-path tools/activity-monitor/Cargo.toml \
  --target-dir target \
  --target "$target.2.28"
