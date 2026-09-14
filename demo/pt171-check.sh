#!/bin/bash
# PT-171 proof: a bare-launched host must register on the default instance so
# `pmux space open` regroups it instead of spawning a second host.
set -uo pipefail
export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
cd "$HOME/demo" && ./record-demo.sh --launch >/dev/null 2>&1
sleep 2
sock=/tmp/runtime-demo/prismattyc/pmux.sock
echo "host.pid: $(cat "${sock%.sock}.host.pid" 2>/dev/null || echo MISSING)  host procs: $(pgrep -c -x prismattyc-host)"
cd "$HOME/work" && pmux space save agents claude kiro >/dev/null 2>&1
out=$(pmux space open agents 2>&1)
echo "$out" | tail -3
echo "host procs after open: $(pgrep -c -x prismattyc-host)"
echo "$out" | grep -q "opened host pid" && echo "RESULT: FAIL (second host spawned)" || echo "RESULT: PASS (live host regrouped)"
