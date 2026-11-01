#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Build contained fixtures and exercise the real CLI, daemon and workers.

Requires Rust 1.90 and its wasm32-unknown-unknown standard library. Packages
are embedded only into the isolated proof build, never installed into the
operator's running daemon. Run on each supported OS before release.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time


REPO = Path(__file__).resolve().parent.parent
PLUGIN = "org.moonbase.runtime-fixture"


def build(args, scratch):
    target = args.target_dir.resolve()
    env = os.environ.copy()
    env.pop("PRISMATTYC_PLUGIN_BUNDLE_DIR", None)
    env["CARGO_TARGET_DIR"] = str(target)
    cargo = ["cargo", f"+{args.toolchain}"]

    def run(*command, **overrides):
        subprocess.run([*cargo, *command], cwd=REPO, env={**env, **overrides}, check=True)

    guest_env = {key: value for key, value in env.items() if key not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
    subprocess.run([*cargo, "build", "-p", "pmux-plugin-fixture", "--release", "--target", "wasm32-unknown-unknown", "--locked"], cwd=REPO, env=guest_env, check=True)
    module = target / "wasm32-unknown-unknown/release/pmux_plugin_fixture.wasm"
    good = REPO / "crates/pmux-plugin-fixture/package"
    bad = scratch / "bad-readiness"
    shutil.copytree(good, bad)
    manifest = (bad / "plugin.toml").read_text().replace('version = "1.0.0"', 'version = "1.0.1"')
    manifest = manifest.replace('notices = ["LICENSE"]', 'notices = ["LICENSE"]\nassets = ["fixture-fail-ready"]')
    (bad / "plugin.toml").write_text(manifest)
    (bad / "fixture-fail-ready").write_text("reject readiness")
    bundle = scratch / "bundle"
    run("run", "-p", "prismattyc-mux", "--example", "package_plugin", "--locked", "--", str(bundle), str(good), str(module), str(bad), str(module))
    run("build", "-p", "prismattyc-mux", "--bin", "pmux", "--bin", "pmuxd", "--locked", PRISMATTYC_PLUGIN_BUNDLE_DIR=str(bundle))
    return target / "debug"


class Connection:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(45)
        self.socket.connect(str(path))
        self.reader = self.socket.makefile("rb")
        self.sequence = 0

    def close(self):
        self.reader.close()
        self.socket.close()

    def request(self, kind, **fields):
        self.sequence += 1
        self.socket.sendall((json.dumps({"version": 1, "request_id": self.sequence, "type": kind, **fields}) + "\n").encode())
        frame = self.reader.readline(4 * 1024 * 1024 + 1)
        assert frame.endswith(b"\n") and len(frame) <= 4 * 1024 * 1024
        reply = json.loads(frame)
        assert reply["version"] == 1 and reply["request_id"] == self.sequence
        return reply

    def plugin(self, operation, **fields):
        return self.request("plugin", operation={"operation": operation, **fields})


def exercise(bins, scratch):
    # Short socket paths also work with macOS's sockaddr_un limit.
    runtime = Path(tempfile.mkdtemp(prefix="pmux-plug-")).resolve()
    config = runtime / "config.toml"
    endpoint = runtime / "daemon.sock"
    env = {key: value for key, value in os.environ.items() if not key.startswith(("HIVE_", "PMUX_", "PRISMATTYC_"))}
    env.update(XDG_DATA_HOME=str(runtime / "data"), XDG_CONFIG_HOME=str(runtime / "config"), PRISMATTYC_CONFIG=str(config), PMUX_SESSION_AGENTS=str(runtime / "agents.json"))
    suffix = ".exe" if os.name == "nt" else ""
    pmux = bins / f"pmux{suffix}"
    pmuxd = bins / f"pmuxd{suffix}"
    daemon = None
    connections = []
    log = (scratch / "daemon.log").open("wb")

    def setting(on):
        temporary = config.with_suffix(".new")
        temporary.write_text(f"start_at_login = false\n[mux]\nexperimental_activity = {'true' if on else 'false'}\n")
        os.replace(temporary, config)

    def start():
        program = ["cmd.exe", "/d", "/c", "pause > nul"] if os.name == "nt" else ["/bin/sh", "-c", "read -r line"]
        process = subprocess.Popen([str(pmuxd), "--socket", str(endpoint), "--", *program], env=env, stdout=log, stderr=log)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("isolated daemon exited; inspect its proof log")
            try:
                if endpoint.with_suffix(".pid").read_text().strip() != str(process.pid):
                    time.sleep(0.02)
                    continue
                connection = Connection(endpoint)
                connections.append(connection)
                return process, connection
            except OSError:
                time.sleep(0.02)
        process.kill()
        process.wait()
        raise RuntimeError("isolated daemon did not become ready")

    def stop(process):
        process.terminate()
        try:
            process.wait(5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    def cli(*args, succeeds=True, text=False):
        result = subprocess.run([str(pmux), "--socket", str(endpoint), "plugin", *args], env=env, capture_output=True, text=True, timeout=45)
        assert (result.returncode == 0) == succeeds, result.stderr
        if not succeeds:
            return result.stderr
        return result.stdout if text else json.loads(result.stdout)

    def tool(name, arguments="{}"):
        result = cli("invoke", PLUGIN, name, arguments)
        assert result["result"] == "tool"
        return json.loads(result["json"])

    def feed(on):
        setting(on)
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            reply = connection.request("read_activity", cursor=None, limit=1)
            if on and reply["status"] == "ok":
                return reply["response"]["page"]["epoch"]
            if not on and reply["status"] == "error" and reply["error"]["code"] == "activity_disabled":
                return None
            time.sleep(0.05)
        raise AssertionError("feed setting did not apply live")

    try:
        setting(False)
        daemon, connection = start()
        pid = daemon.pid
        baseline = connection.request("snapshot")["response"]["snapshot"]["sessions"]
        assert len(cli("discover")["packages"]) == 2
        assert cli("list")["installed"] == []
        cli("invoke", PLUGIN, "poll", succeeds=False)
        installed = cli("install", PLUGIN, "--version", "1.0.0")["package"]
        assert installed["version"] == "1.0.0"
        cli("invoke", PLUGIN, "poll", succeeds=False)
        cli("enable", PLUGIN, "--version", "1.0.0", "--allow", "activity.read:local")
        assert tool("poll") == {"error": "feed-disabled"}
        assert tool("counter") == {"counter": 1}
        first_epoch = feed(True)
        event = {"schema_version": 1, "seat": "fixture-seat", "run_id": "run-1", "event_id": "event-1", "source": "hooks", "kind": "waiting"}
        assert connection.request("publish_activity", event=event)["status"] == "ok"
        assert tool("poll") == {"epoch": first_epoch, "sequence": 1, "seat": "fixture-seat"}
        feed(False)
        assert tool("poll") == {"error": "feed-disabled"}
        second_epoch = feed(True)
        assert first_epoch != second_epoch
        assert tool("poll") == {"epoch": second_epoch, "sequence": 0, "seat": None}
        assert tool("counter") == {"counter": 2}
        assert cli("run", PLUGIN, "main", "--snapshot", "--cols", "3", "--rows", "1", text=True) == "F  \n"
        assert connection.plugin("exports", allowlist=[])["response"]["reply"]["tools"] == []
        exports = connection.plugin("exports", allowlist=[PLUGIN])["response"]["reply"]["tools"]
        assert sorted(export["name"] for export in exports) == ["fixture_counter", "fixture_grow", "fixture_loop", "fixture_poll"]
        handle = next(export["handle"] for export in exports if export["name"] == "fixture_counter")
        forged = Connection(endpoint)
        connections.append(forged)
        assert forged.plugin("call_export", handle=handle, arguments="{}")["status"] == "error"
        cli("invoke", PLUGIN, "counter", '{"undeclared":true}', succeeds=False)
        assert tool("counter") == {"counter": 3}
        cli("install", PLUGIN, "--version", "1.0.1")
        cli("enable", PLUGIN, "--version", "1.0.1", "--allow", "activity.read:local", succeeds=False)
        assert cli("inspect", PLUGIN)["state"]["selected"]["version"] == "1.0.0"
        assert tool("counter") == {"counter": 4}
        assert daemon.poll() is None and daemon.pid == pid
        assert connection.request("snapshot")["response"]["snapshot"]["sessions"] == baseline
        cli("disable", PLUGIN)
        assert connection.plugin("call_export", handle=handle, arguments="{}")["status"] == "error"
        cli("invoke", PLUGIN, "counter", succeeds=False)
        for item in connections:
            item.close()
        connections.clear()
        stop(daemon)
        daemon, connection = start()
        assert not cli("inspect", PLUGIN)["state"]["enabled"]
        cli("invoke", PLUGIN, "counter", succeeds=False)
        print("PASS install stays disabled; explicit grants; typed calls and frames; persistent tool worker; live off/on epochs; unchanged sessions and daemon; default-empty MCP exports; connection-bound handles; schema rejection; failed readiness preserves old selection; durable disable after restart")
    finally:
        for item in connections:
            item.close()
        if daemon is not None and daemon.poll() is None:
            stop(daemon)
        log.close()
        shutil.rmtree(runtime)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=REPO / "target/plugin-proof")
    parser.add_argument("--toolchain", default="1.90.0")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="pmux-plugin-build-") as work:
        scratch = Path(work)
        bins = build(args, scratch)
        exercise(bins, scratch)


if __name__ == "__main__":
    main()
