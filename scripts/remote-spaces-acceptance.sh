#!/usr/bin/env bash
# Native acceptance setup for remote Spaces (issue #24).
#
# `up` starts an isolated pmuxd with sample Spaces, writes an isolated host
# config with one `[[remote]]` destination (`loopback`), and an `ssh`
# wrapper that sends that alias to PRISMATTYC_SSH_TEST_TARGET with the test
# key and runs the remote `pmux` against the isolated daemon. It prints the
# command that launches prismattyc-host with that setup, so the live
# daemon, config and Spaces are never touched. `down` stops the daemon and
# removes the directory.
#
# Needs the same env as scripts/remote-ssh-tests.sh:
#   PRISMATTYC_SSH_TEST_TARGET, PRISMATTYC_SSH_TEST_KEY,
#   PRISMATTYC_SSH_TEST_KNOWN_HOSTS
# Usage:
#   scripts/remote-spaces-acceptance.sh up    # prints the launch command
#   PRISMATTYC_ACCEPT_DIR=<dir> scripts/remote-spaces-acceptance.sh down
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"

die() {
  echo "remote-spaces-acceptance: $*" >&2
  exit 1
}

up() {
  for name in PRISMATTYC_SSH_TEST_TARGET PRISMATTYC_SSH_TEST_KEY PRISMATTYC_SSH_TEST_KNOWN_HOSTS; do
    [ -n "${!name:-}" ] || die "set $name"
  done
  (cd "$ROOT" && cargo build --locked -p prismattyc-mux --bins && cargo build --locked -p prismattyc-host)
  local dir
  dir="$(mktemp -d "${TMPDIR:-/tmp}/prismattyc-accept.XXXXXX")"
  dir="$(cd "$dir" && pwd -P)"
  # If any step below fails, stop the half-started daemon and remove the
  # directory so a failed `up` never leaks either.
  cleanup_on_fail() {
    if [ -f "$dir/remote/pmuxd.pid" ]; then
      kill "$(cat "$dir/remote/pmuxd.pid")" 2>/dev/null || true
    fi
    rm -rf "$dir"
  }
  trap cleanup_on_fail ERR
  fail() { cleanup_on_fail; die "$*"; }
  case "$dir$BIN$PRISMATTYC_SSH_TEST_KEY$PRISMATTYC_SSH_TEST_KNOWN_HOSTS" in
    *[[:space:]\"\'\\]*) fail "paths must not contain spaces, quotes or backslashes" ;;
  esac
  mkdir -p "$dir/remote" "$dir/local" "$dir/bin"
  local rsock="$dir/remote/pmux.sock"

  PMUX_SOCKET="$rsock" XDG_DATA_HOME="$dir/remote" XDG_CONFIG_HOME="$dir/remote/config" \
    XDG_STATE_HOME="$dir/remote/state" nohup "$BIN/pmuxd" --socket "$rsock" \
    >"$dir/remote/pmuxd.log" 2>&1 &
  echo $! >"$dir/remote/pmuxd.pid"
  for _ in $(seq 1 100); do [ -S "$rsock" ] && break; sleep 0.05; done
  [ -S "$rsock" ] || fail "isolated pmuxd did not start"
  remote_pmux() {
    PMUX_SOCKET="$rsock" XDG_DATA_HOME="$dir/remote" XDG_CONFIG_HOME="$dir/remote/config" \
      XDG_STATE_HOME="$dir/remote/state" "$BIN/pmux" "$@"
  }
  remote_pmux space create work --no-attach >/dev/null
  remote_pmux space add work >/dev/null
  remote_pmux space create notes --no-attach >/dev/null

  # Test wrapper: alias "loopback" -> the test target with the test key;
  # the remote pmux runs against the isolated daemon.
  # Note on the joining below: "$before"/"$after" are glued with spaces and
  # re-split by the shell on receipt. That is safe here because up() rejects
  # spaces, quotes and backslashes in every interpolated path, and the ssh
  # target plus pmux arguments are single tokens.
  cat >"$dir/bin/ssh" <<EOF
#!/bin/sh
set -eu
before=""
after=""
seen=0
alias_done=0
for arg in "\$@"; do
  if [ \$seen -eq 0 ]; then
    if [ "\$arg" = "--" ]; then seen=1; else before="\$before \$arg"; fi
  elif [ \$alias_done -eq 0 ]; then
    [ "\$arg" = "loopback" ] || { echo "test ssh wrapper: unknown alias \$arg" >&2; exit 255; }
    alias_done=1
  elif [ "\$arg" = "pmux" ] && [ -z "\$after" ]; then
    after="env PMUX_SOCKET=$rsock XDG_DATA_HOME=$dir/remote XDG_CONFIG_HOME=$dir/remote/config XDG_STATE_HOME=$dir/remote/state PMUX_ATTACH=$BIN/pmux-attach $BIN/pmux"
  else
    after="\$after \$arg"
  fi
done
# shellcheck disable=SC2086
exec /usr/bin/ssh \$before -i $PRISMATTYC_SSH_TEST_KEY -o IdentitiesOnly=yes -o IdentityAgent=none \\
  -o UserKnownHostsFile=$PRISMATTYC_SSH_TEST_KNOWN_HOSTS -o GlobalKnownHostsFile=/dev/null \\
  -o StrictHostKeyChecking=yes -- $PRISMATTYC_SSH_TEST_TARGET \$after
EOF
  chmod 755 "$dir/bin/ssh"

  cat >"$dir/config.toml" <<EOF
[[remote]]
id = "loopback"
ssh = "loopback"
label = "loopback"
EOF
  cat >"$dir/launch.sh" <<EOF
#!/bin/sh
exec env PATH="$dir/bin:\$PATH" PRISMATTYC_CONFIG="$dir/config.toml" \\
  PMUX_SOCKET="$dir/local/pmux.sock" XDG_DATA_HOME="$dir/local" \\
  XDG_STATE_HOME="$dir/local/state" "$BIN/prismattyc-host" --no-splash
EOF
  chmod 755 "$dir/launch.sh"
  trap - ERR
  echo "PRISMATTYC_ACCEPT_DIR=$dir"
  echo "launch: $dir/launch.sh"
}

down() {
  local dir="${PRISMATTYC_ACCEPT_DIR:-}"
  [ -n "$dir" ] && [ -d "$dir" ] || die "set PRISMATTYC_ACCEPT_DIR"
  # Guard the rm -rf below: only remove directories this script created,
  # recognizable by the mktemp basename and the daemon pid file.
  case "$(basename "$dir")" in
    prismattyc-accept.*) ;;
    *) die "refusing to remove $dir: not an acceptance directory" ;;
  esac
  [ -f "$dir/remote/pmuxd.pid" ] || die "refusing to remove $dir: no remote/pmuxd.pid"
  kill "$(cat "$dir/remote/pmuxd.pid")" 2>/dev/null || true
  rm -rf "$dir"
  echo "removed $dir"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  *) die "usage: $0 up|down" ;;
esac
