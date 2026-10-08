#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

mkdir -p /private/tmp/piosurf
proof_dir="$(mktemp -d /private/tmp/piosurf/macos-present-proof.XXXXXX)"
dump_path="$proof_dir/present.png"
readback_path="$proof_dir/present.readback.png"

PRISMATTYC_DUMP_PRESENT="$dump_path" \
    cargo +1.90.0 run -p prismattyc-host --example macos_present_probe --locked

test -s "$dump_path"
test -s "$readback_path"
dump_sha="$(shasum -a 256 "$dump_path" | awk '{print $1}')"
readback_sha="$(shasum -a 256 "$readback_path" | awk '{print $1}')"
if [[ "$dump_sha" != "$readback_sha" ]]; then
    printf 'FAIL dump/readback SHA-256 differs: %s != %s\n' "$dump_sha" "$readback_sha" >&2
    exit 1
fi
printf 'PASS dump/readback SHA-256 %s\n' "$dump_sha"
