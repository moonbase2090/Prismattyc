#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Run a private monitor seat with synthetic history for native OS captures.

Attach the printed seat using a real terminal on the same machine. Create the
output directory's stop file after capture; a deadline also owns teardown.
This fixture does not take a screenshot or claim that synthetic work occurred.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts"))
spec = importlib.util.spec_from_file_location("plugin_proof", REPO / "scripts/test-plugin-runtime.py")
proof = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proof)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=600)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 1800:
        parser.error("--seconds must be between 1 and 1800")

    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    for name in ("SIGTERM", "SIGHUP", "SIGBREAK"):
        if hasattr(signal, name):
            signal.signal(getattr(signal, name), interrupted)
    bins, output = args.bins.resolve(), args.out.resolve()
    suffix = ".exe" if os.name == "nt" else ""
    for binary in ("pmux", "pmuxd", "pmux-attach", "pmux-activity-monitor"):
        if not (bins / f"{binary}{suffix}").is_file():
            parser.error(f"missing {binary}; build the matching native binaries before capture")
    output.mkdir(parents=True, exist_ok=False)
    runtime = Path(tempfile.mkdtemp(prefix="pmux-graph-")).resolve()
    daemon = connection = None
    result = {"status": "FAIL", "scope": "native seat with synthetic sample history"}
    try:
        if os.name == "nt":
            proof.private_windows_directory(runtime)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("PMUX", "PRISMATTYC_", "XDG_"))}
        config, socket = runtime / "config.toml", runtime / "daemon.sock"
        config.write_text("start_at_login = false\ninstall_agent_skills = false\n[mux]\nexperimental_activity = true\n")
        env.update(HOME=str(runtime), USERPROFILE=str(runtime), APPDATA=str(runtime),
                   LOCALAPPDATA=str(runtime), XDG_DATA_HOME=str(runtime / "data"),
                   XDG_CONFIG_HOME=str(runtime / "config"), XDG_STATE_HOME=str(runtime / "state"),
                   XDG_RUNTIME_DIR=str(runtime), PRISMATTYC_CONFIG=str(config),
                   PMUX_SOCKET=str(socket), PRISMATTYC_DEFAULT_PLUGINS="0",
                   PMUX_SESSION_AGENTS=str(runtime / "agents.json"),
                   TERM="xterm-256color", LANG="en_US.UTF-8")
        pmux, launcher = bins / f"pmux{suffix}", bins / f"pmux-activity-monitor{suffix}"

        def cli(*arguments):
            command = [str(pmux), "--socket", str(socket), *arguments]
            completed = subprocess.run(command, env=env, capture_output=True, text=True,
                                       encoding="utf-8", timeout=45)
            with (output / "commands.jsonl").open("a") as log:
                log.write(json.dumps({"argv": [argument.replace(str(runtime), "<runtime>").replace(str(bins), "<bins>") for argument in arguments],
                                      "exit_code": completed.returncode,
                                      "stdout": completed.stdout,
                                      "stderr": completed.stderr.replace(str(runtime), "<runtime>")}) + "\n")
            assert completed.returncode == 0, completed.stderr
            return completed.stdout

        with (output / "daemon.log").open("w") as log:
            shell = ["cmd.exe", "/d", "/c", "pause > nul"] if os.name == "nt" else ["/bin/sh", "-c", "read -r line"]
            daemon = subprocess.Popen([str(bins / f"pmuxd{suffix}"), "--socket", str(socket), "--", *shell],
                                      env=env, stdout=log, stderr=log)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            assert daemon.poll() is None, "private daemon exited"
            try:
                connection = proof.Connection(socket)
                break
            except OSError:
                time.sleep(.05)
        assert connection is not None, "private daemon did not become ready"

        now = int(time.time() * 1000)
        records = []
        for index in range(240):
            seat = ("builder", "reviewer", "worker")[index % 3]
            kind = ("command_started", "edit_finished", "test_started", "test_failed", "waiting")[index // 3 % 5]
            if index >= 237:
                kind = ("test_failed", "waiting", "command_started")[index - 237]
            records.append({"sequence": index + 1, "received_at_ms": now - 290_000 + index * 1200,
                            "event": {"schema_version": 1, "seat": seat, "run_id": "sample-run",
                                      "event_id": f"sample-{index}", "source": "hooks", "kind": kind}})
        data = runtime / "history.json"
        data.write_text(json.dumps(records))
        provider = runtime / "sample-history.py"
        provider.write_text('''import json, sys, time
from pathlib import Path
records = json.loads(Path(sys.argv[1]).read_text())
arguments = sys.argv[2:]
after = int(arguments[arguments.index('--cursor') + 1].split(':')[1]) if '--cursor' in arguments else 0
page = {'epoch':'cafebabe','oldest_sequence':1,'current_sequence':len(records),
        'records':records[after:after+256], 'cursor':{'epoch':'cafebabe','sequence':len(records)},
        'has_more':False,'reset':None}
print(json.dumps({'type':'activity','page':page}), flush=True)
while True:
    time.sleep(1)
''')
        argv = [sys.executable, str(provider), str(data)]
        cli("plugin", "install", "org.moonbase.activity-monitor")
        cli("plugin", "endpoint", "add", "sample", "--command-tail", json.dumps(argv))
        cli("plugin", "enable", "org.moonbase.activity-monitor", "--allow", "activity.read:sample")
        cli("new", "--no-attach", "--no-agent", "graph-proof", "--", str(launcher),
            "--host-cmd", "sample", json.dumps(argv), "--no-pixels", "--no-motion", "--theme", "dark")
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            text = cli("save-buffer", "graph-proof", "-")
            if "builder" in text and any("\u2801" <= character <= "\u28ff" for character in text):
                break
            time.sleep(.1)
        else:
            raise AssertionError("monitor seat did not present default braille sample history")
        (output / "pane.txt").write_text(text, encoding="utf-8")
        ready = {"status": "READY", "daemon_pid": daemon.pid, "socket": str(socket),
                 "seat": "graph-proof", "attach": [str(pmux), "--socket", str(socket), "attach", "graph-proof"],
                 "version": cli("--version").strip(), "data": "synthetic sample history"}
        ready["environment"] = {key: value for key, value in env.items()
                                if key.startswith(("PMUX", "PRISMATTYC_", "XDG_"))
                                or key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA")}
        (output / "ready.json").write_text(json.dumps(ready, indent=2) + "\n")
        (output / "attach.py").write_text('''import json, os, subprocess
from pathlib import Path
ready = json.loads(Path(__file__).with_name('ready.json').read_text())
raise SystemExit(subprocess.call(ready['attach'], env={**os.environ, **ready['environment']}))
''')
        print(json.dumps(ready), flush=True)
        deadline = time.monotonic() + args.seconds
        while not (output / "stop").exists() and time.monotonic() < deadline:
            assert daemon.poll() is None, "private daemon exited during capture"
            time.sleep(.2)
        result["status"] = "PASS"
    finally:
        if connection is not None:
            connection.close()
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)
        shutil.rmtree(runtime)
        result.update(daemon_reaped=daemon is None or daemon.poll() is not None,
                      scratch_removed=not runtime.exists())
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
