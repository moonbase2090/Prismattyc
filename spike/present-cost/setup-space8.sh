#!/bin/sh
# Isolated test instance for scenario (c): a test pmuxd, a Space `spike` with
# eight sessions, and four of them printing a 30-line listing every 200 ms.
# Never touches the live pmuxd: spk-env.sh pins PMUX_SOCKET under /private/tmp/pspk.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
E="$HERE/../render-thread/spk-env.sh"
mkdir -p /private/tmp/pspk/home /private/tmp/pspk/run /private/tmp/pspk/tmp /private/tmp/pspk/out
chmod 700 /private/tmp/pspk/run
"$E" pmux up > /private/tmp/pspk/up.log 2>&1 &
sleep 3
"$E" pmux space create spike --no-attach > /dev/null 2>&1 || true
for i in 2 3 4 5 6 7 8; do "$E" pmux space add spike --name "spk$i" > /dev/null 2>&1 || true; done
# Session ids 2..9 are the Space's sessions on a fresh test pmuxd.
python3 - <<'EOF'
import json
view = {"tabs": [{"title": "spike", "sessions": [str(i) for i in range(2, 10)]}],
        "space": "spike", "mode": "switch",
        "session_names": {str(i): n for i, n in zip(range(2, 10), ["spike-1"] + [f"spk{j}" for j in range(2, 9)])}}
json.dump(view, open("/private/tmp/pspk/view.json", "w"), indent=2)
EOF
for pane in 2 4 6 8; do
  "$E" pmux send "$pane" 'while :; do date; ls -la /usr/bin | head -30; sleep 0.2; done' --enter > /dev/null
done
"$E" pmux ls | grep -c pane
