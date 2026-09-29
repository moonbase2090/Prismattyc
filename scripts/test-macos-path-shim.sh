#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

cargo test -p prismattyc-mux --lib --locked path_shim -- --test-threads=1
cargo test -p prismattyc-mux --lib --locked macos_plan_includes_only_the_owned_pmux_path_link -- --test-threads=1
