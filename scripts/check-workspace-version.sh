#!/usr/bin/env bash
# Thin wrapper so CI and humans can call the check as a shell script.
set -euo pipefail
exec python3 "$(dirname "$0")/check-workspace-version.py" "$@"
