#!/usr/bin/env bash
# Run the real-SSH tests for remote Spaces (issue #24).
#
# Needs an SSH target that accepts a key non-interactively. The key and its
# authorization are managed outside this repository and passed by env:
#   PRISMATTYC_SSH_TEST_TARGET       user@host (loopback is typical)
#   PRISMATTYC_SSH_TEST_KEY          private key authorized for the target
#   PRISMATTYC_SSH_TEST_KNOWN_HOSTS  known_hosts pinning the target host key
# The target must share this machine's filesystem: every remote command
# runs the freshly built binaries by absolute path under `env` with an
# isolated PMUX_SOCKET, so no live daemon is used. This script never edits
# ~/.ssh or authorized_keys.
#
# Usage: scripts/remote-ssh-tests.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
for name in PRISMATTYC_SSH_TEST_TARGET PRISMATTYC_SSH_TEST_KEY PRISMATTYC_SSH_TEST_KNOWN_HOSTS; do
  if [ -z "${!name:-}" ]; then
    echo "remote-ssh-tests: set $name" >&2
    exit 2
  fi
done
[ -r "$PRISMATTYC_SSH_TEST_KEY" ] || { echo "remote-ssh-tests: cannot read the key" >&2; exit 2; }
[ -r "$PRISMATTYC_SSH_TEST_KNOWN_HOSTS" ] || { echo "remote-ssh-tests: cannot read known_hosts" >&2; exit 2; }

/usr/bin/ssh -i "$PRISMATTYC_SSH_TEST_KEY" -o IdentitiesOnly=yes -o IdentityAgent=none \
  -o "UserKnownHostsFile=$PRISMATTYC_SSH_TEST_KNOWN_HOSTS" -o GlobalKnownHostsFile=/dev/null \
  -o StrictHostKeyChecking=yes -o BatchMode=yes -o ConnectTimeout=10 \
  -T -- "$PRISMATTYC_SSH_TEST_TARGET" true ||
  { echo "remote-ssh-tests: key login to $PRISMATTYC_SSH_TEST_TARGET failed" >&2; exit 1; }

cargo build --locked -p prismattyc-mux --bins
export PRISMATTYC_SSH_TEST_BIN="$ROOT/target/debug"
cargo test --locked -p prismattyc-mux --test remote_ssh -- --test-threads=1
cargo test --locked -p prismattyc-host --bin prismattyc-host real_ssh -- --test-threads=1
