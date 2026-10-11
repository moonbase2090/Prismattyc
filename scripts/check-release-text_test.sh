#!/usr/bin/env bash
# The checker fails when a forbidden marker is present, and its output is only file:line.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
checker="$root/scripts/check-release-text.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

home="$work/home"
mkdir -p "$home"
export HOME="$home"
unset RELEASE_DENYLIST
unset RELEASE_DENYLIST_FILE

clean_tree() {
  local dir="$1"
  mkdir -p "$dir/docs"
  printf '## [0.0.1]\n\nA public note.\n' >"$dir/CHANGELOG.md"
  printf '# Notes\n\nNothing private here.\n' >"$dir/README.md"
  printf 'Page/Home/End stays prose.\nbackground@2x.png is an asset name.\n' >"$dir/docs/sample.md"
}

assert_clean() {
  local dir="$1"
  local out err code=0
  out="$(mktemp)"
  err="$(mktemp)"
  "$checker" --root "$dir" >"$out" 2>"$err" || code=$?
  if [[ "$code" -ne 0 || -s "$out" || -s "$err" ]]; then
    echo "expected a clean tree to pass" >&2
    cat "$out" "$err" >&2
    exit 1
  fi
  rm -f "$out" "$err"
}

assert_hit() {
  local dir="$1"
  local forbidden="$2"
  local out err code=0
  out="$(mktemp)"
  err="$(mktemp)"
  "$checker" --root "$dir" >"$out" 2>"$err" || code=$?
  if [[ "$code" -eq 0 ]]; then
    echo "expected a hit" >&2
    exit 1
  fi
  if [[ ! -s "$out" ]]; then
    echo "expected file:line output" >&2
    exit 1
  fi
  if grep -F -q -- "$forbidden" "$out" "$err"; then
    echo "report included matched text" >&2
    exit 1
  fi
  if grep -E -v -q '^[^:]+:[0-9]+$' "$out"; then
    echo "report was not file:line" >&2
    cat "$out" >&2
    exit 1
  fi
  rm -f "$out" "$err"
}

clean_tree "$work/clean"
printf 'Contact 322824348+mb2090@users.noreply.github.com for the release.\n' >>"$work/clean/README.md"
assert_clean "$work/clean"

clean_tree "$work/desktop"
printf 'Machine DESKTOP-ABC123 was used.\n' >>"$work/desktop/CHANGELOG.md"
assert_hit "$work/desktop" "DESKTOP-ABC123"

clean_tree "$work/users"
printf 'See /Users/someone/notes.\n' >>"$work/users/README.md"
assert_hit "$work/users" "/Users/someone"

clean_tree "$work/home-path"
printf 'Built under /home/runner/work.\n' >>"$work/home-path/docs/sample.md"
assert_hit "$work/home-path" "/home/runner"

clean_tree "$work/win"
printf 'C:\\Users\\someone\\AppData\n' >>"$work/win/CHANGELOG.md"
assert_hit "$work/win" 'C:\\Users\\someone'

clean_tree "$work/mail"
printf 'Write someone@example.com for help.\n' >>"$work/mail/README.md"
assert_hit "$work/mail" "someone@example.com"

clean_tree "$work/token"
printf 'token ghp_aaaaaaaaaaaaaaaaaaaa leaked\n' >>"$work/token/CHANGELOG.md"
assert_hit "$work/token" "ghp_aaaaaaaaaaaaaaaaaaaa"

clean_tree "$work/ip"
printf 'peer 192.168.1.20\n' >>"$work/ip/docs/sample.md"
assert_hit "$work/ip" "192.168.1.20"

clean_tree "$work/public-ip"
printf 'resolver 8.8.8.8\n' >>"$work/public-ip/docs/sample.md"
assert_clean "$work/public-ip"

clean_tree "$work/deny"
printf 'marker ZZZTOKEN in the notes\n' >>"$work/deny/CHANGELOG.md"
export RELEASE_DENYLIST="ZZZTOKEN"
assert_hit "$work/deny" "ZZZTOKEN"
unset RELEASE_DENYLIST

clean_tree "$work/missing-deny"
assert_clean "$work/missing-deny"

clean_tree "$work/bad-deny"
export RELEASE_DENYLIST="("
out="$(mktemp)"
err="$(mktemp)"
code=0
"$checker" --root "$work/bad-deny" >"$out" 2>"$err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "invalid denylist pattern must fail" >&2
  exit 1
fi
if grep -F -q -- "(" "$err"; then
  echo "invalid pattern was echoed" >&2
  exit 1
fi
if ! grep -F -q "invalid pattern" "$err"; then
  echo "missing invalid-pattern notice" >&2
  exit 1
fi
unset RELEASE_DENYLIST
rm -f "$out" "$err"

artifact="$work/artifact.bin"
printf 'prefix\0DESKTOP-ZZZZ9999\0suffix\n' >"$artifact"
clean_tree "$work/bin"
out="$(mktemp)"
code=0
"$checker" --root "$work/bin" --artifact "$artifact" >"$out" 2>"$work/bin.err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "artifact marker must fail" >&2
  exit 1
fi
if grep -F -q "DESKTOP-ZZZZ9999" "$out" "$work/bin.err"; then
  echo "artifact report included matched text" >&2
  exit 1
fi
if ! grep -q "^${artifact}:[0-9][0-9]*$" "$out"; then
  echo "artifact report was not file:line" >&2
  cat "$out" >&2
  exit 1
fi

repo_out="$(mktemp)"
repo_err="$(mktemp)"
repo_code=0
"$checker" --root "$root" >"$repo_out" 2>"$repo_err" || repo_code=$?
if [[ "$repo_code" -ne 0 || -s "$repo_out" || -s "$repo_err" ]]; then
  echo "current tree matched a generic pattern" >&2
  cat "$repo_out" "$repo_err" >&2
  exit 1
fi
rm -f "$repo_out" "$repo_err"

echo "check-release-text tests passed"
