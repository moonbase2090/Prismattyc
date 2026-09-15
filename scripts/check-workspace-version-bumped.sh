#!/usr/bin/env bash
# Fail a PR whose [workspace.package] version still matches origin/main.
# Skip when HEAD is the base (already merged). Classic claim is separate.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
base="${VERSION_BASE_REF:-origin/main}"

if ! git rev-parse --verify "$base" >/dev/null 2>&1; then
  echo "skip: $base is not available"
  exit 0
fi

if [ "$(git rev-parse HEAD)" = "$(git rev-parse "$base")" ]; then
  echo "skip: HEAD is $base"
  exit 0
fi

workspace_version() {
  python3 -c '
import re, sys
text = sys.stdin.read()
m = re.search(r"\[workspace\.package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", text, re.M | re.S)
if not m:
    sys.exit("no [workspace.package] version")
print(m.group(1))
'
}

ours="$(workspace_version <"$root/Cargo.toml")"
theirs="$(git show "$base:Cargo.toml" | workspace_version)"

if [ "$ours" = "$theirs" ]; then
  echo "workspace.package.version is $ours, same as $base."
  echo "Bump one patch in Cargo.toml for this PR (CONTRIBUTING.md, Version a change)."
  exit 1
fi

echo "version $theirs ($base) -> $ours"
