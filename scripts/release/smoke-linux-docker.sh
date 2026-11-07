#!/usr/bin/env bash
# Smoke-test the Linux release executables in a clean Amazon Linux 2023
# container: the glibc floor check, then scripts/release/smoke-release.py
# (versions, sessions and mail, saved Spaces, Update and Restart).
#
#   smoke-linux-docker.sh BIN_DIR OUT_DIR [EXPECTED_VERSION]
#
# BIN_DIR holds the shipped executables under their plain names (pmux, pmuxd,
# ...), for the architecture of this machine's Docker. OUT_DIR must not exist.
set -euo pipefail

bin_dir="${1:?Usage: smoke-linux-docker.sh BIN_DIR OUT_DIR [EXPECTED_VERSION]}"
out_dir="${2:?Usage: smoke-linux-docker.sh BIN_DIR OUT_DIR [EXPECTED_VERSION]}"
expect="${3:-}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin_dir="$(cd "$bin_dir" && pwd)"
[[ ! -e "$out_dir" ]] || { echo "OUT_DIR already exists: $out_dir" >&2; exit 1; }
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

"$here/smoke-linux-al2023.sh" "$bin_dir" | tee "$out_dir/glibc.log"

tools="$(mktemp -d)"
trap 'rm -rf "$tools"' EXIT
cp "$here/smoke-release.py" \
  "$repo/.cursor/skills/verify-prismattyc/scripts/verify-cli.py" \
  "$repo/tests/native/update-restart-e2e.py" "$tools/"

docker run --rm \
  --volume "$bin_dir:/release:ro" \
  --volume "$tools:/tools:ro" \
  --volume "$out_dir:/out" \
  public.ecr.aws/amazonlinux/amazonlinux:2023 \
  bash -c '
    set -euo pipefail
    dnf --assumeyes --quiet install python3 procps-ng >/dev/null
    install -d /out/run
    python3 /tools/smoke-release.py --bins /release --out /out/run/smoke '"${expect:+--expect-version "$expect"}"'
  ' 2>&1 | tee "$out_dir/smoke.log"
