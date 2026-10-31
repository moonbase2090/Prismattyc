#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive `pmux activity` and the pmux-mcp activity tools against private daemons.

Starts one pmuxd with --experimental-activity and one without, each on a private
socket, publishes synthetic runtime events with the activity-emit example, and
checks every connector command and error path. Writes OUT/result.json.

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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", required=True, type=Path)
    parser.add_argument("--emitter", type=Path, help="default: BINS/examples/activity-emit")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    bins = args.bins.resolve()
    emitter = (args.emitter or bins / "examples" / "activity-emit").resolve()
    for tool in (bins / "pmux", bins / "pmuxd", bins / "pmux-mcp", emitter):
        if not tool.is_file():
            parser.error(f"missing {tool}")
    if args.out.exists():
        parser.error("--out must be a new directory")
    args.out.mkdir(parents=True)

    # A short private directory: Unix socket paths are limited to ~104 bytes.
    scratch = Path(tempfile.mkdtemp(prefix="pmux-act-"))
    scratch.chmod(0o700)
    env = dict(os.environ, HOME=str(scratch), XDG_RUNTIME_DIR=str(scratch), XDG_CONFIG_HOME=str(scratch / "config"))
    env.pop("PMUX_SOCKET", None)
    on, off = scratch / "on.sock", scratch / "off.sock"
    daemons = [
        subprocess.Popen([str(bins / "pmuxd"), "--experimental-activity", "--socket", str(on), "--", "/bin/sh"],
                         env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([str(bins / "pmuxd"), "--socket", str(off), "--", "/bin/sh"],
                         env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    checks, failures = [], []

    def check(label, ok, detail=None):
        checks.append({"check": label, "pass": bool(ok), "detail": detail})
        print(("PASS " if ok else "FAIL ") + label + (f": {detail}" if detail is not None else ""))
        if not ok:
            failures.append(label)

    def emit(seat, event_id, kind):
        event = {"schema_version": 1, "seat": seat, "run_id": "run-1", "event_id": event_id,
                 "source": "runtime", "kind": kind}
        subprocess.run([str(emitter), "--socket", str(on), "--seat", seat, "--event", json.dumps(event)],
                       env=env, check=True)

    def pa(socket, *words):
        done = subprocess.run([str(bins / "pmux"), "--socket", str(socket), "activity", *words],
                              env=env, capture_output=True, text=True, timeout=30)
        lines = [json.loads(line) for line in done.stdout.splitlines() if line.strip()]
        return done.returncode, lines

    try:
        for _ in range(100):
            if on.exists() and off.exists():
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

        check_mcp(bins, env, on, off, check)
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

    def __init__(self, bins, env, socket):
        env = dict(env, PMUX_SOCKET=str(socket), PMUX_AGENT="activity-check")
        self.proc = subprocess.Popen([str(bins / "pmux-mcp")], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, env=env)
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


def check_mcp(bins, env, on, off, check):
    mcp = Mcp(bins, env, on)
    tools = sorted(t["name"] for t in mcp.request("tools/list", {})["result"]["tools"]
                   if t["name"].startswith("pmux_activity_"))
    check("mcp: three read-only activity tools",
          tools == ["pmux_activity_poll", "pmux_activity_recent", "pmux_activity_seats"], tools)
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
    mcp.close()
    disabled = Mcp(bins, env, off)
    error, result = disabled.tool("pmux_activity_seats")
    check("mcp: disabled feed is an error result", error and result["error"]["code"] == "feed_disabled")
    disabled.close()


if __name__ == "__main__":
    sys.exit(main())
