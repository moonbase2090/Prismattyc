#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

mode="${1:-tiles}"
if [[ "$mode" != tiles && "$mode" != iosurface ]]; then
    printf 'usage: %s [tiles|iosurface]\n' "$0" >&2
    exit 2
fi
mkdir -p /private/tmp/piosurf
proof_dir="$(mktemp -d /private/tmp/piosurf/macos-present-proof.XXXXXX)"
dump_path="$proof_dir/$mode.png"
readback_path="$proof_dir/$mode.readback.png"

PRISMATTYC_DUMP_PRESENT="$dump_path" \
    cargo +1.90.0 run -p prismattyc-host --example macos_present_probe --locked -- "$mode"

test -s "$dump_path"
test -s "$readback_path"
dump_sha="$(shasum -a 256 "$dump_path" | awk '{print $1}')"
readback_sha="$(shasum -a 256 "$readback_path" | awk '{print $1}')"
if [[ "$dump_sha" != "$readback_sha" ]]; then
    printf 'FAIL %s dump/readback SHA-256 differs: %s != %s\n' "$mode" "$dump_sha" "$readback_sha" >&2
    exit 1
fi
printf 'PASS %s dump/readback SHA-256 %s\n' "$mode" "$dump_sha"
