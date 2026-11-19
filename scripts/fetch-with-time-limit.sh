#!/usr/bin/env bash
# Run a download command with a time limit per attempt, a few times.
# CI install steps use it so a stalled mirror fails over in minutes instead of
# using up the whole job. It is for fetching tools and packages, never for tests.
#
# Usage: fetch-with-time-limit.sh SECONDS ATTEMPTS COMMAND [ARGS...]
set -euo pipefail

usage() {
  echo "usage: $0 SECONDS ATTEMPTS COMMAND [ARGS...]" >&2
  exit 2
}

[ "$#" -ge 3 ] || usage
limit=$1
attempts=$2
shift 2
case "$limit" in '' | *[!0-9]* | 0) usage ;; esac
case "$attempts" in '' | *[!0-9]* | 0) usage ;; esac

attempt=1
while :; do
  status=0
  timeout --kill-after=10 "$limit" "$@" || status=$?
  if [ "$status" -eq 0 ]; then
    exit 0
  fi
  if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
    reason="ran past ${limit}s"
  else
    reason="exited $status"
  fi
  if [ "$attempt" -ge "$attempts" ]; then
    echo "error: '$*' $reason on attempt $attempt of $attempts; giving up" >&2
    exit "$status"
  fi
  echo "warning: '$*' $reason on attempt $attempt of $attempts; starting again" >&2
  attempt=$((attempt + 1))
done
