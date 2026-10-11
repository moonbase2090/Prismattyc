#!/usr/bin/env python3
"""Check file permission hardening and session/mail preservation in an isolated daemon.

Pass --before-bins to exercise an upgrade from an older build. Without it,
legacy modes are seeded explicitly before restarting the current build.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import stat
import tempfile

spec = importlib.util.spec_from_file_location(
    "login_restore", Path(__file__).with_name("login-restore-e2e.py"))
restore = importlib.util.module_from_spec(spec)
spec.loader.exec_module(restore)


def mode(path, expected):
    actual = stat.S_IMODE(path.stat().st_mode)
    assert actual == expected, f"{path.name}: {actual:o}, expected {expected:o}"


def run(f, after):
    f.env["PMUX_PANE_LOG"] = str(f.root / "data/prismattyc/pane-log-default.json")
    f.cli("up", "--", "/bin/sh", "-c", "exec sleep 120")
    f.cli("new", "--no-attach", "--agent", "worker", "retained",
          "--", "/bin/sh", "-c", "printf 'retained output\\n'; exec sleep 120")
    f.cli("mail", "--as", "sender", "send", "worker",
          "--summary", "retained mail", "--body", "retained message body")
    before = restore.topology(f.snapshot())
    data = f.root / "data/prismattyc"
    checkpoint = restore.wait_for(
        lambda: next((p for p in (data / "workspaces").glob("*/workspace.json")
                      if '"retained"' in p.read_text()), None), "saved session")
    pane_log = restore.wait_for(lambda: next(iter(data.glob("pane-log-*.json")), None), "pane log")
    f.cli("stop")
    restore.wait_for(lambda: not f.socket.exists(), "daemon stopped")
    # Legacy runtime records that may not be rewritten during daemon startup.
    for name in ("pmux.pid", "pmux.host.pid", "pmux.host.render.json", "pmux.attach-tabs.json", "pmux.restart.log"):
        (f.runtime / name).write_text("retained runtime record\n")
    files = [f.runtime / name for name in (
        "pmux.log", "pmux.pid", "pmux.sock.spaces-identity", "pmux.host.pid",
        "pmux.host.render.json", "pmux.attach-tabs.json", "pmux.restart.log")]
    files += [data / "mail.db", data / "session-agents.json", pane_log]
    for path in files:
        assert path.is_file(), path.name
        path.chmod(0o644)
    f.runtime.chmod(0o755)
    data.chmod(0o755)
    identity = (f.runtime / "pmux.sock.spaces-identity").read_bytes()
    f.binaries = after
    f.env["PATH"] = str(after) + ":/usr/bin:/bin"
    f.cli("up", "--", "/bin/sh", "-c", "exec sleep 120")
    assert restore.topology(f.snapshot()) == before, "session topology changed"
    letters = json.loads(f.cli("mail", "--as", "worker", "claim", "--json"))["letters"]
    assert [letter["body"] for letter in letters] == ["retained message body"], letters
    f.cli("mail", "--as", "worker", "commit", letters[0]["id"])
    assert "open: 0 held: 0" in f.cli("mail", "--as", "worker", "inbox")
    mode(f.runtime, 0o700)
    mode(data, 0o700)
    mode(f.socket, 0o600)
    for path in files:
        mode(path, 0o600)
    assert (f.runtime / "pmux.sock.spaces-identity").read_bytes() == identity
    assert (f.runtime / "pmux.restart.log").read_text() == "retained runtime record\n"
    assert "retained output" in f.cli("save-buffer", "retained", "-"), "pane output was lost"
    assert checkpoint.exists(), "workspace was lost"
    return {"result": "PASS", "directories": "0700", "files": "0600",
            "sessions": "preserved", "mail": "claimed and committed", "pane_log": "preserved"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", type=Path, required=True)
    parser.add_argument("--before-bins", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="pmux-modes-", dir="/tmp") as root:
        f = restore.Fixture((args.before_bins or args.bins).resolve(), Path(root))
        try:
            proof = run(f, args.bins.resolve())
        finally:
            f.close()
    print(json.dumps(proof))


if __name__ == "__main__":
    main()
