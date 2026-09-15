#!/usr/bin/env bash
# Run Spaces team scenarios through Termwright and the native X11 fixture.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../../.." && pwd)
bins=${PRISMATTYC_BINS:-$repo/target/debug}
run_id=${SPACES_TEAM_RUN_ID:-$(date -u +%Y%m%d-%H%M%S)-$$}
case "$run_id" in *[!a-zA-Z0-9._-]*|'') echo 'Invalid run ID' >&2; exit 2;; esac
out=$repo/build/spaces-team-box/$run_id
[[ ! -e "$out" ]] || { echo 'Use a new run ID' >&2; exit 2; }
mkdir -p "$out"
image=${PRISMATTYC_NATIVE_TEST_IMAGE:-prismattyc-native-tests}
docker image inspect "$image" > "$out/image.json"
python3 "$repo/scripts/la-heavy-serial.py" --acquire --job spaces-e2e
cid=''
cleanup() {
    if [[ -n "$cid" ]]; then docker rm -f "$cid" >/dev/null; fi
    python3 "$repo/scripts/la-heavy-serial.py" --release --job spaces-e2e
}
trap cleanup EXIT
cid=$(docker create --init --shm-size 1g --entrypoint /bin/bash "$image" -c 'sleep infinity')
for bin in pmux pmuxd pmux-attach prismattyc-host pmux-mcp; do
    [[ -x "$bins/$bin" ]] || { echo "Missing binary: $bin" >&2; exit 2; }
    docker cp "$bins/$bin" "$cid:/usr/local/bin/$bin"
done
docker cp "$(command -v termwright)" "$cid:/usr/local/bin/termwright"
docker cp "$repo/tests/native/spaces-team-e2e.py" "$cid:/home/tester/spaces-team-e2e.py"
docker cp "$repo/tests/native/spaces-move-target-e2e.py" "$cid:/home/tester/spaces-move-target-e2e.py"
docker cp "$repo/tests/native/spaces-daily-e2e.py" "$cid:/home/tester/spaces-daily-e2e.py"
docker cp "$repo/tests/native/host-ux-e2e.py" "$cid:/home/tester/host-ux-e2e.py"
docker cp "$repo/tests/native/restart-spaces-e2e.py" "$cid:/home/tester/restart-spaces-e2e.py"
docker cp "$repo/tests/native/space-open-race.py" "$cid:/home/tester/space-open-race.py"
docker start "$cid" >/dev/null
status=0
case "${SPACES_TEAM_CASE:-team}" in
move)
    docker exec -u tester -e MOVE_TARGET_OUT=/home/tester/move-evidence "$cid" python3 /home/tester/spaces-move-target-e2e.py > "$out/runner.log" 2>&1 || status=$?
    docker cp "$cid:/home/tester/move-evidence/." "$out/"
    ;;
daily)
    docker exec -u tester -e DAILY_SPACES_OUT=/home/tester/daily-evidence "$cid" python3 /home/tester/spaces-daily-e2e.py > "$out/runner.log" 2>&1 || status=$?
    docker cp "$cid:/home/tester/daily-evidence/." "$out/"
    ;;
team)
    docker exec -u tester -e SPACES_TEAM_OUT=/home/tester/team-evidence "$cid" \
        termwright screenshot --cols 120 --rows 36 --timeout 600 \
        --wait-for SPACES_TEAM_E2E_COMPLETE --output /home/tester/team-terminal.png \
        -- python3 /home/tester/spaces-team-e2e.py --native --termwright \
        > "$out/runner.log" 2>&1 || status=$?
    docker cp "$cid:/home/tester/team-evidence/." "$out/"
    docker cp "$cid:/home/tester/team-terminal.png" "$out/terminal.png" || status=1
    ;;
restart)
    docker exec -u tester -e RESTART_SPACES_OUT=/home/tester/restart-evidence "$cid" \
        python3 /home/tester/restart-spaces-e2e.py > "$out/runner.log" 2>&1 || status=$?
    docker cp "$cid:/home/tester/restart-evidence/." "$out/"
    ;;
race)
    docker exec -i -u tester -e HOST_UX_OUT=/home/tester/race-evidence -e HOST_UX_NO_WM=1 "$cid" \
        python3 - > "$out/runner.log" 2>&1 <<'PY' || status=$?
import os, subprocess
x = subprocess.Popen(['Xvfb', '-displayfd', '1', '-screen', '0', '1920x1080x24', '-nolisten', 'tcp'], stdout=subprocess.PIPE)
try:
    os.environ['DISPLAY'] = ':' + x.stdout.readline().decode().strip()
    subprocess.run(['python3', '/home/tester/space-open-race.py'], check=True)
finally:
    x.terminate()
    x.wait(timeout=5)
PY
    docker cp "$cid:/home/tester/race-evidence/space-open-race/." "$out/"
    ;;
*) echo 'Unknown case: use team, daily, move, restart, or race' >&2; exit 2;;
esac
if [[ $status -eq 0 ]]; then
    python3 - "$out/result.json" <<'PY'
import json, sys
assert json.load(open(sys.argv[1]))['status'] == 'PASS'
PY
fi
printf 'Spaces team box evidence: %s\n' "$out"
exit "$status"
