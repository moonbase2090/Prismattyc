#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive `pmux activity` and the pmux-mcp activity tools against private daemons.

Starts two pmuxd with --experimental-activity and one without, each on a private
socket, publishes synthetic runtime events with `pmux activity-emit`, and
checks every connector command and error path, then pmux-mcp in full and
activity-only modes, including a hosts file with a command endpoint. Written
for Unix and Windows. Writes OUT/result.json.

    python3 scripts/verify-activity-connector.py --bins target/debug --out DIR
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

WINDOWS = os.name == "nt"
SHELL = os.environ.get("COMSPEC", "cmd.exe") if WINDOWS else "/bin/sh"


def exe(path: Path) -> Path:
    return path.with_name(path.name + ".exe") if WINDOWS else path


# pmuxd on Windows requires an owner-only, inheritable DACL on the socket's
# parent directory; chmod does not set one.
OWNER_ONLY_DACL = """$ErrorActionPreference='Stop';
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User;
$acl=New-Object Security.AccessControl.DirectorySecurity;
$acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false);
$rule=New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow');
$acl.AddAccessRule($rule);
Set-Acl -LiteralPath $env:ACTIVITY_CONNECTOR_SCRATCH -AclObject $acl;
"""


def make_private(directory: Path) -> None:
    directory.chmod(0o700)
    if WINDOWS:
        subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", OWNER_ONLY_DACL],
                       env=dict(os.environ, ACTIVITY_CONNECTOR_SCRATCH=str(directory)),
                       capture_output=True, timeout=30, check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    bins = args.bins.resolve()
    for tool in (exe(bins / "pmux"), exe(bins / "pmuxd"), exe(bins / "pmux-mcp")):
        if not tool.is_file():
            parser.error(f"missing {tool}")
    if args.out.exists():
        parser.error("--out must be a new directory")
    args.out.mkdir(parents=True)

    # A short private directory: Unix socket paths are limited to ~104 bytes.
    scratch = Path(tempfile.mkdtemp(prefix="pmux-act-"))
    make_private(scratch)
    env = dict(os.environ, HOME=str(scratch), XDG_RUNTIME_DIR=str(scratch), XDG_CONFIG_HOME=str(scratch / "config"))
    if WINDOWS:
        env.update(USERPROFILE=str(scratch), LOCALAPPDATA=str(scratch), APPDATA=str(scratch))
    env.pop("PMUX_SOCKET", None)
    env.pop("PMUX_AGENT", None)
    on, on2, off = scratch / "on.sock", scratch / "on2.sock", scratch / "off.sock"

    def daemon(socket, feed):
        flags = ["--experimental-activity"] if feed else []
        return subprocess.Popen([str(exe(bins / "pmuxd")), *flags, "--socket", str(socket), "--", SHELL],
                                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    daemons = [daemon(on, True), daemon(on2, True), daemon(off, False)]
    checks, failures = [], []

    def check(label, ok, detail=None):
        checks.append({"check": label, "pass": bool(ok), "detail": detail})
        print(("PASS " if ok else "FAIL ") + label + (f": {detail}" if detail is not None else ""))
        if not ok:
            failures.append(label)

    def emit(seat, event_id, kind, socket=on):
        event = {"schema_version": 1, "seat": seat, "run_id": "run-1", "event_id": event_id,
                 "source": "runtime", "kind": kind}
        subprocess.run([str(exe(bins / "pmux")), "--socket", str(socket), "activity-emit", "--seat", seat,
                        "--event", json.dumps(event)], env=env, check=True)

    def pa(socket, *words):
        done = subprocess.run([str(exe(bins / "pmux")), "--socket", str(socket), "activity", *words],
                              env=env, capture_output=True, text=True, timeout=30)
        lines = [json.loads(line) for line in done.stdout.splitlines() if line.strip()]
        return done.returncode, lines

    try:
        for _ in range(100):
            if on.exists() and on2.exists() and off.exists():
                break
            time.sleep(0.05)

        code, [empty] = pa(on, "seats", "--host-label", "host-a")
        check("seats on an empty feed", code == 0 and empty["seats"] == [] and empty["host"] == "host-a")
        emit("worker-a", "e1", "session_started")
        emit("worker-a", "e2", "working")
        emit("worker-b", "e3", "waiting")
        emit("worker-a", "e4", "command_started")

        code, [seats] = pa(on, "seats")
        summary = [(s["seat"], s["retained_events"], s["last_kind"]) for s in seats["seats"]]
        check("seats summarizes each seat", code == 0 and summary == [("worker-a", 3, "command_started"),
                                                                      ("worker-b", 1, "waiting")], summary)
        code, [recent] = pa(on, "recent", "worker-a", "--limit", "2")
        ids = [r["event"]["event_id"] for r in recent["records"]]
        check("recent keeps the newest N for one seat", code == 0 and ids == ["e2", "e4"] and recent["truncated"], ids)

        code, [first] = pa(on, "poll", "--limit", "2")
        check("poll starts at the oldest event", code == 0
              and [r["sequence"] for r in first["page"]["records"]] == [1, 2] and first["page"]["has_more"])
        code, [second] = pa(on, "poll", "--cursor", first["next_cursor"], "--seat", "worker-a")
        ids = [r["event"]["event_id"] for r in second["page"]["records"]]
        check("poll resumes from next_cursor; filter still advances", code == 0 and ids == ["e4"]
              and second["next_cursor"].endswith(":4"), ids)

        def later():
            time.sleep(0.8)
            emit("worker-b", "e5", "tool_started")
            time.sleep(0.4)
            emit("worker-a", "e6", "working")
            time.sleep(0.4)
            emit("worker-b", "e7", "tool_finished")

        producer = threading.Thread(target=later)
        producer.start()
        code, tail = pa(on, "tail", "--seat", "worker-b", "--max-events", "2", "--max-seconds", "20")
        producer.join()
        (args.out / "tail.jsonl").write_text("".join(json.dumps(line) + "\n" for line in tail))
        seen = [r["event"]["event_id"] for line in tail if line.get("type") == "activity"
                for r in line["page"]["records"]]
        end = tail[-1] if tail else {}
        check("tail from now sees only new events for the seat", code == 0 and seen == ["e5", "e7"], seen)
        check("tail ends on max_events with a resume cursor", end.get("type") == "end"
              and end.get("reason") == "max_events" and str(end.get("next_cursor")).endswith(":7"), end)

        emit("worker-a", "e8", "stopped")
        code, resumed = pa(on, "tail", "--cursor", end["next_cursor"], "--max-seconds", "1")
        seen = [r["event"]["event_id"] for line in resumed if line.get("type") == "activity"
                for r in line["page"]["records"]]
        check("tail --cursor resumes after the cursor", code == 0 and seen == ["e8"]
              and resumed[-1].get("reason") == "max_seconds", seen)
        code, replay = pa(on, "tail", "--from-oldest", "--max-events", "1")
        check("tail --from-oldest replays retained history", code == 0
              and len(replay[0]["page"]["records"]) == 8 and replay[-1]["type"] == "end")

        code, [stale] = pa(on, "poll", "--cursor", "00:1")
        check("a foreign epoch returns a successful reset page", code == 0
              and stale["page"]["reset"] == "restarted" and len(stale["page"]["records"]) == 8)

        for label, socket, words, want_code, want in [
            ("feed off: seats", off, ["seats"], 1, "feed_disabled"),
            ("feed off: tail fails fast", off, ["tail", "--max-seconds", "10"], 1, "feed_disabled"),
            ("no daemon", scratch / "missing.sock", ["seats"], 1, "daemon_unreachable"),
            ("usage error", on, ["poll", "--limit", "0"], 2, "invalid_argument"),
        ]:
            started = time.monotonic()
            code, lines = pa(socket, *words)
            error = lines[-1].get("error", {}) if lines else {}
            check(label, code == want_code and error.get("code") == want and time.monotonic() - started < 5,
                  {"exit": code, "error": error})

        check_mcp(bins, env, scratch, on, off, check)
        emit("win-worker", "w1", "turn_started", socket=on2)
        check_multi_host(bins, env, scratch, on, on2, check)
    finally:
        for daemon in daemons:
            daemon.terminate()
        for daemon in daemons:
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
        subprocess.run(["rm", "-rf", str(scratch)], check=False)

    result = {"status": "PASS" if not failures else "FAIL", "failures": failures, "checks": checks,
              "scratch_removed": not scratch.exists()}
    (args.out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print("RESULT:", result["status"])
    return 0 if not failures else 1


class Mcp:
    """Minimal MCP stdio client for pmux-mcp."""

    def __init__(self, bins, env, socket, args=(), agent="activity-check"):
        env = dict(env, PMUX_SOCKET=str(socket))
        if agent:
            env["PMUX_AGENT"] = agent
        self.proc = subprocess.Popen([str(exe(bins / "pmux-mcp")), *args], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
        self.next_id = 1
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "verify-activity-connector", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("pmux-mcp closed stdout")
            reply = json.loads(line)
            if reply.get("id") == request_id:
                return reply

    def tool(self, name, arguments=None):
        result = self.request("tools/call", {"name": name, "arguments": arguments or {}})["result"]
        return result.get("isError", False), json.loads(result["content"][0]["text"])

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=5)


def hosts_file(scratch, name, text):
    path = scratch / name
    path.write_text(text)
    return ["--activity-hosts", str(path)]


def toml_str(value):
    return json.dumps(str(value))


def check_mcp(bins, env, scratch, on, off, check):
    # Interval 0: these checks call one host back to back.
    unlimited = hosts_file(scratch, "local.toml",
                           f"min_interval_ms = 0\n[[host]]\nlabel = \"local\"\nsocket = {toml_str(on)}\n")
    mcp = Mcp(bins, env, on, unlimited)
    tools = sorted(t["name"] for t in mcp.request("tools/list", {})["result"]["tools"]
                   if t["name"].startswith("pmux_activity_"))
    check("mcp: four read-only activity tools",
          tools == ["pmux_activity_hosts", "pmux_activity_poll", "pmux_activity_recent", "pmux_activity_seats"],
          tools)
    error, seats = mcp.tool("pmux_activity_seats")
    check("mcp: seats", not error and [s["seat"] for s in seats["seats"]] == ["worker-a", "worker-b"])
    error, recent = mcp.tool("pmux_activity_recent", {"seat": "worker-b", "limit": 1})
    check("mcp: recent", not error and [r["event"]["event_id"] for r in recent["records"]] == ["e7"])
    error, first = mcp.tool("pmux_activity_poll", {"limit": 3})
    error2, second = mcp.tool("pmux_activity_poll", {"cursor": first["next_cursor"], "seat": "worker-a"})
    check("mcp: poll follows next_cursor", not error and not error2
          and [r["sequence"] for r in second["page"]["records"]] == [4, 6, 8])
    error, bad = mcp.tool("pmux_activity_poll", {"cursor": "not-a-cursor"})
    check("mcp: bad cursor is an error result", error and bad["error"]["code"] == "invalid_argument")
    error, bad = mcp.tool("pmux_activity_recent", {"seat": "a;id"})
    check("mcp: shell metacharacters in a seat are refused", error and bad["error"]["code"] == "invalid_argument")
    mcp.close()
    disabled = Mcp(bins, env, off)
    error, result = disabled.tool("pmux_activity_seats")
    check("mcp: disabled feed is an error result", error and result["error"]["code"] == "feed_disabled")
    disabled.close()


def check_multi_host(bins, env, scratch, on, on2, check):
    # "win" stands in for a remote host: a command endpoint that runs another
    # pmux, as `ssh -T host pmux` would. "down" is an endpoint that fails.
    pmux = str(exe(bins / "pmux"))
    missing = str(exe(scratch / "no-such-pmux"))
    hosts = hosts_file(scratch, "hosts.toml", f"""min_interval_ms = 300
[[host]]
label = "mac"
socket = {toml_str(on)}
[[host]]
label = "win"
command = [{toml_str(pmux)}, "--socket", {toml_str(on2)}]
[[host]]
label = "down"
command = [{toml_str(missing)}]
""")
    for label, extra in [("activity-only", []), ("activity-only under --supervise", ["--supervise"])]:
        mcp = Mcp(bins, env, on, ["--activity-only", *hosts, *extra], agent=None)
        names = sorted(t["name"] for t in mcp.request("tools/list", {})["result"]["tools"])
        check(f"{label}: only the four activity tools, no identity",
              names == ["pmux_activity_hosts", "pmux_activity_poll", "pmux_activity_recent",
                        "pmux_activity_seats"], names)
        mcp.close()
    mcp = Mcp(bins, env, on, ["--activity-only", *hosts], agent=None)
    _, listed = mcp.tool("pmux_activity_hosts")
    check("hosts lists labels and endpoint kinds, not paths",
          [(h["label"], h["endpoint"]) for h in listed["hosts"]]
          == [("mac", "local"), ("win", "command"), ("down", "command")]
          and str(on2) not in json.dumps(listed), listed["hosts"])
    error, result = mcp.tool("pmux_activity_seats")
    check("several hosts: host is required", error and result["error"]["code"] == "host_required")
    error, result = mcp.tool("pmux_activity_seats", {"host": "nope"})
    check("unknown host is refused", error and result["error"]["code"] == "unknown_host")
    error, mac = mcp.tool("pmux_activity_seats", {"host": "mac"})
    error2, win = mcp.tool("pmux_activity_seats", {"host": "win"})
    check("each host returns its own seats, labeled", not error and not error2
          and mac["host"] == "mac" and win["host"] == "win"
          and [s["seat"] for s in win["seats"]] == ["win-worker"]
          and "win-worker" not in [s["seat"] for s in mac["seats"]],
          {"mac": [s["seat"] for s in mac.get("seats", [])], "win": [s["seat"] for s in win.get("seats", [])]})
    error, limited = mcp.tool("pmux_activity_seats", {"host": "mac"})
    retry = limited.get("error", {}).get("retry_after_ms", 0)
    check("a second call to one host within the interval is rate_limited",
          error and limited["error"]["code"] == "rate_limited" and 0 < retry <= 301, limited.get("error"))
    time.sleep(retry / 1000 + 0.05)
    error, again = mcp.tool("pmux_activity_poll", {"host": "mac", "limit": 1})
    check("after retry_after_ms the host answers again", not error and again["host"] == "mac")
    error, down = mcp.tool("pmux_activity_seats", {"host": "down"})
    check("a failing endpoint is host_unreachable", error and down["error"]["code"] == "host_unreachable"
          and down["host"] == "down", down.get("error", {}).get("code"))
    mcp.close()


if __name__ == "__main__":
    sys.exit(main())
