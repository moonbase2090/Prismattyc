#!/usr/bin/env bash
# The fetch helper stops a stalled attempt at its limit, starts again, and
# gives up with the command's status after the last attempt.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
helper="$root/scripts/fetch-with-time-limit.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0
# Nothing ever opens this pipe for writing, so reading it blocks until the
# helper stops the attempt: a download that never finishes, with no clock.
mkfifo "$work/never"

fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# A fake download: each run appends to a count file, then acts out the
# behavior listed for that run number (ok, fail, or hang).
fake() {
  local name="$1"
  shift
  local script="$work/$name"
  {
    echo '#!/usr/bin/env bash'
    echo "count_file='$work/$name.count'"
    echo 'echo run >>"$count_file"'
    echo 'n=$(wc -l <"$count_file" | tr -d " ")'
    echo 'case "$n" in'
    local i=1
    for behavior in "$@"; do
      case "$behavior" in
        ok) echo "  $i) exit 0 ;;" ;;
        fail) echo "  $i) exit 3 ;;" ;;
        hang) echo "  $i) exec cat '$work/never' ;;" ;;
      esac
      i=$((i + 1))
    done
    echo '  *) exit 9 ;;'
    echo 'esac'
  } >"$script"
  chmod +x "$script"
  echo "$script"
}

runs() {
  wc -l <"$work/$1.count" | tr -d ' '
}

check() {
  local label="$1" want_code="$2" want_runs="$3" name="$4"
  shift 4
  local code=0
  "$helper" "$@" 2>"$work/$name.err" || code=$?
  if [ "$code" -ne "$want_code" ]; then
    fail "$label: exit $code, want $want_code"
  fi
  if [ "$(runs "$name")" -ne "$want_runs" ]; then
    fail "$label: ran $(runs "$name") times, want $want_runs"
  fi
}

cmd="$(fake first ok)"
check "first attempt succeeds" 0 1 first 5 3 "$cmd"
if [ -s "$work/first.err" ]; then
  fail "first attempt succeeds: unexpected warning: $(cat "$work/first.err")"
fi

cmd="$(fake retry fail ok)"
check "failure is retried" 0 2 retry 5 3 "$cmd"
grep -q "exited 3 on attempt 1 of 3; starting again" "$work/retry.err" \
  || fail "failure is retried: no warning: $(cat "$work/retry.err")"

cmd="$(fake stall hang ok)"
check "stalled attempt is stopped and retried" 0 2 stall 1 3 "$cmd"
grep -q "ran past 1s on attempt 1 of 3" "$work/stall.err" \
  || fail "stalled attempt is stopped and retried: no warning: $(cat "$work/stall.err")"

cmd="$(fake stuck hang hang)"
check "gives up after the last stalled attempt" 124 2 stuck 1 2 "$cmd"
grep -q "ran past 1s on attempt 2 of 2; giving up" "$work/stuck.err" \
  || fail "gives up after the last stalled attempt: no error: $(cat "$work/stuck.err")"

cmd="$(fake broken fail fail fail)"
check "gives up with the command's status" 3 3 broken 5 3 "$cmd"

for args in "" "5" "5 3" "x 3 true" "5 0 true" "0 3 true"; do
  code=0
  # shellcheck disable=SC2086
  "$helper" $args 2>/dev/null || code=$?
  if [ "$code" -ne 2 ]; then
    fail "usage '$args': exit $code, want 2"
  fi
done

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo "fetch-with-time-limit: all checks passed"
