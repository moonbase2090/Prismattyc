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
import sys
import tempfile
import time


REPO = Path(__file__).resolve().parent.parent
PLUGIN = "org.moonbase.runtime-fixture"


def private_windows_directory(path):
    import ctypes as C
    from ctypes import wintypes as W
    kernel = C.WinDLL("kernel32", use_last_error=True)
    security = C.WinDLL("advapi32", use_last_error=True)
    kernel.GetCurrentProcess.restype = W.HANDLE
    kernel.CloseHandle.argtypes = [W.HANDLE]
    kernel.CloseHandle.restype = W.BOOL
    kernel.LocalFree.argtypes = [C.c_void_p]
    kernel.LocalFree.restype = C.c_void_p
    security.OpenProcessToken.argtypes = [W.HANDLE, W.DWORD, C.POINTER(W.HANDLE)]
    security.OpenProcessToken.restype = W.BOOL
    security.GetTokenInformation.argtypes = [W.HANDLE, C.c_int, C.c_void_p, W.DWORD, C.POINTER(W.DWORD)]
    security.GetTokenInformation.restype = W.BOOL
    security.ConvertSidToStringSidW.argtypes = [C.c_void_p, C.POINTER(C.c_void_p)]
    security.ConvertSidToStringSidW.restype = W.BOOL
    security.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [W.LPCWSTR, W.DWORD, C.POINTER(C.c_void_p), C.c_void_p]
    security.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = W.BOOL
    security.SetFileSecurityW.argtypes = [W.LPCWSTR, W.DWORD, C.c_void_p]
    security.SetFileSecurityW.restype = W.BOOL

    def checked(value):
        if not value:
            raise C.WinError(C.get_last_error())

    token = W.HANDLE()
    sid_text = C.c_void_p()
    descriptor = C.c_void_p()
    checked(security.OpenProcessToken(kernel.GetCurrentProcess(), 8, C.byref(token)))
    try:
        size = W.DWORD()
        security.GetTokenInformation(token, 1, None, 0, C.byref(size))
        checked(size.value)
        user = C.create_string_buffer(size.value)
        checked(security.GetTokenInformation(token, 1, user, size, C.byref(size)))
        # TOKEN_USER starts with SID_AND_ATTRIBUTES, whose first field is PSID.
        sid = C.cast(user, C.POINTER(C.c_void_p)).contents.value
        checked(security.ConvertSidToStringSidW(sid, C.byref(sid_text)))
        owner = C.wstring_at(sid_text)
        sddl = f"O:{owner}D:P(A;OICI;FA;;;{owner})"
        checked(security.ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, C.byref(descriptor), None))
        checked(security.SetFileSecurityW(str(path), 0x80000005, descriptor))
    finally:
        if descriptor.value:
            kernel.LocalFree(descriptor)
        if sid_text.value:
            kernel.LocalFree(sid_text)
        kernel.CloseHandle(token)


def mcp_rejects_unterminated_input(binary, arguments, env, log_path):
    with log_path.open("wb") as error_log:
        process = subprocess.Popen([str(binary), *arguments], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=error_log, env=env)
        try:
            try:
                process.stdin.write(b"{" + b" " * (4 * 1024 * 1024))
                process.stdin.flush()
            except BrokenPipeError:
                pass
            # Keep stdin open: an uncapped read_until would wait forever here.
            assert process.wait(timeout=10) != 0
        finally:
            try:
                process.stdin.close()
            except BrokenPipeError:
                pass
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)


def build(args, scratch):
    target = args.target_dir.resolve()
    env = os.environ.copy()
    env.pop("PRISMATTYC_PLUGIN_BUNDLE_DIR", None)
    env["CARGO_TARGET_DIR"] = str(target)
    cargo = ["cargo", f"+{args.toolchain}"]

    def run(*command, **overrides):
        profile = ["--release"] if args.release else []
        subprocess.run([*cargo, command[0], *profile, *command[1:]], cwd=REPO, env={**env, **overrides}, check=True)

    if args.bundle:
        bundle = args.bundle.resolve()
        run("build", "-p", "prismattyc-mux", "--example", "plugin_bundle", "--locked")
        helper = target / ("release" if args.release else "debug") / "examples" / ("plugin_bundle.exe" if os.name == "nt" else "plugin_bundle")
        subprocess.run([str(helper), "verify", str(bundle), "--candidate"], cwd=REPO, env=env, check=True)
    else:
        guest_env = {key: value for key, value in env.items() if key not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
        subprocess.run([*cargo, "build", "-p", "pmux-plugin-fixture", "-p", "pmux-activity-monitor-plugin", "-p", "pmux-activity-connector-plugin", "--release", "--target", "wasm32-unknown-unknown", "--locked"], cwd=REPO, env=guest_env, check=True)
        module = target / "wasm32-unknown-unknown/release/pmux_plugin_fixture.wasm"
        good = REPO / "crates/pmux-plugin-fixture/package"
        bad = scratch / "bad-readiness"
        shutil.copytree(good, bad)
        manifest = (bad / "plugin.toml").read_text().replace('version = "1.0.0"', 'version = "1.0.1"')
        manifest = manifest.replace('notices = ["LICENSE"]', 'notices = ["LICENSE"]\nassets = ["fixture-fail-ready"]')
        (bad / "plugin.toml").write_text(manifest)
        (bad / "fixture-fail-ready").write_text("reject readiness")
        bundle = scratch / "bundle"
        monitor = REPO / "crates/pmux-activity-monitor-plugin/package"
        monitor_module = target / "wasm32-unknown-unknown/release/pmux_activity_monitor_plugin.wasm"
        connector = REPO / "crates/pmux-activity-connector-plugin/package"
        connector_module = target / "wasm32-unknown-unknown/release/pmux_activity_connector_plugin.wasm"
        run("run", "-p", "prismattyc-mux", "--example", "package_plugin", "--locked", "--", str(bundle), str(good), str(module), str(bad), str(module), str(monitor), str(monitor_module), str(connector), str(connector_module))
    run("build", "-p", "prismattyc-mux", "--example", "plugin_frame_probe", "--locked")
    run("build", "-p", "prismattyc-mux", "--bin", "pmux", "--bin", "pmuxd", "--locked", PRISMATTYC_PLUGIN_BUNDLE_DIR=str(bundle))
    run("build", "-p", "pmux-mcp", "--locked", PRISMATTYC_PLUGIN_BUNDLE_DIR=str(bundle))
    run("build", "--manifest-path", "tools/activity-monitor/Cargo.toml", "--locked", PRISMATTYC_PLUGIN_BUNDLE_DIR=str(bundle))
    return target / ("release" if args.release else "debug")


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
        return self.request("plugin", plugin_api="1.0.0", operation={"operation": operation, **fields})


def exercise(bins, scratch, feature="all", expected_catalog=None, frame_probe=None):
    # Short socket paths also work with macOS's sockaddr_un limit.
    runtime = Path(tempfile.mkdtemp(prefix="pmux-plug-")).resolve()
    if os.name == "nt":
        private_windows_directory(runtime)
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
                log.flush()
                detail = (scratch / "daemon.log").read_text(errors="replace")[-16384:]
                detail = detail.replace(str(runtime), "<runtime>").replace(str(scratch), "<proof>")
                raise RuntimeError("isolated daemon exited:\n" + detail)
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

    def core(*args):
        result = subprocess.run([str(pmux), "--socket", str(endpoint), *args], env=env, capture_output=True, text=True, timeout=45)
        assert result.returncode == 0, result.stderr
        return result.stdout

    def mcp_proof(arguments, activity_only):
        import queue
        import threading
        messages = queue.Queue(maxsize=16)
        error_log = (scratch / "mcp-stderr.log").open("wb")
        process = subprocess.Popen([str(bins / f"pmux-mcp{suffix}"), *arguments], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=error_log, env={**env,"PMUX_SOCKET":str(endpoint),"PMUX_AGENT":"INVALID AMBIENT ID"})

        def receive():
            try:
                while True:
                    line = process.stdout.readline(4 * 1024 * 1024 + 1)
                    if not line:
                        break
                    assert line.endswith(b"\n") and len(line) <= 4 * 1024 * 1024
                    messages.put(json.loads(line), timeout=10)
            except Exception as error:
                messages.put(error, timeout=10)

        reader = threading.Thread(target=receive, daemon=True)
        reader.start()
        def request(identity, method, params):
            process.stdin.write((json.dumps({"jsonrpc":"2.0","id":identity,"method":method,"params":params})+"\n").encode())
            process.stdin.flush()
            while True:
                response = messages.get(timeout=30)
                assert isinstance(response, dict), response
                if response.get("id") == identity:
                    assert "result" in response, response
                    return response["result"]
        try:
            request(1,"initialize",{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"plugin-proof","version":"1"}})
            process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            process.stdin.flush()
            tools = request(2,"tools/list",{})["tools"]
            names = sorted(tool["name"] for tool in tools)
            if activity_only:
                assert names == ["pmux_activity_hosts","pmux_activity_poll","pmux_activity_recent","pmux_activity_seats"], names
                assert all("outputSchema" in tool for tool in tools)
                result = request(3,"tools/call",{"name":"pmux_activity_hosts","arguments":{}})
                assert not result.get("isError", False), result
                assert json.loads(result["content"][0]["text"])["hosts"] == [{"label":"local","endpoint":"local"}]
                result = request(4,"tools/call",{"name":"pmux_activity_seats","arguments":{}})
                assert not result.get("isError", False), result
                assert json.loads(result["content"][0]["text"])["retained_history_only"] is True
                result = request(5,"tools/call",{"name":"pmux_activity_poll","arguments":{}})
                assert result["isError"] and json.loads(result["content"][0]["text"])["error"]["code"] == "rate_limited", result
            else:
                assert "pmux_inbox" in names and not any(name.startswith("pmux_activity_") for name in names)
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            reader.join(timeout=2)
            error_log.close()
        assert process.returncode == 0, (scratch / "mcp-stderr.log").read_text()


    def terminal_view():
        command = [str(pmux), "--socket", str(endpoint), "plugin", "run", "org.moonbase.activity-monitor", "--no-pixels"]
        driver = runtime / "terminal-driver.py"
        modes = runtime / "terminal-modes.json"
        driver.write_text('''import ctypes, json, os, subprocess, sys
from pathlib import Path
def modes():
    if os.name == "nt":
        from ctypes import wintypes
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.GetStdHandle.argtypes = [wintypes.DWORD]
        kernel.GetStdHandle.restype = wintypes.HANDLE
        kernel.GetConsoleMode.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        result = []
        for stream in [-10, -11]:
            value = wintypes.DWORD()
            if not kernel.GetConsoleMode(kernel.GetStdHandle(stream), ctypes.byref(value)):
                raise ctypes.WinError(ctypes.get_last_error())
            result.append(value.value)
        return repr(result)
    import termios
    attributes = termios.tcgetattr(0)
    if sys.platform == "darwin":
        # XNU sets this queue-state bit whenever it restores ICANON.
        # https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c
        attributes[3] &= ~termios.PENDIN
    return repr(attributes)
before = modes()
result = subprocess.run(sys.argv[2:])
after = modes()
Path(sys.argv[1]).write_text(json.dumps({"before":before,"after":after,"exit":result.returncode}))
print("view-restored" if before == after and result.returncode == 0 else "view-mode-leaked", flush=True)
sys.stdin.readline()
''')
        command = [sys.executable, str(driver), str(modes), *command]
        core("new", "--no-attach", "--no-agent", "plugin-view-proof", "--", *command)
        session = next(session for session in connection.request("snapshot")["response"]["snapshot"]["sessions"] if session["name"] == "plugin-view-proof")
        window = session["windows"][0]
        pane = window["panes"][0]["id"]

        def screen(needle):
            deadline = time.monotonic() + 10
            last = ""
            while time.monotonic() < deadline:
                last = core("save-buffer", str(pane), "-")
                if needle in last:
                    return last
                time.sleep(0.05)
            raise AssertionError(f"live view did not show {needle!r}: {last!r}")

        screen("fixture-seat")
        feed(False)
        screen("feed off")
        feed(True)
        screen("connected")
        assert connection.request("publish_activity", event={"schema_version":1,"seat":"fixture-seat","run_id":"run-live","event_id":"live-1","source":"hooks","kind":"waiting"})["status"] == "ok"
        screen("fixture-seat")
        core("send", str(pane), "?", "--literal", "--force")
        screen("Keys")
        core("send", str(pane), "x", "--literal", "--force")
        assert connection.request("resize", window_id=window["id"], cols=42, rows=12)["status"] == "ok"
        screen("fixture-seat")
        core("send", str(pane), "q", "--literal", "--force")
        try:
            restored = screen("view-restored")
        except AssertionError:
            raise AssertionError(modes.read_text() if modes.exists() else "terminal driver did not return") from None
        assert "view-mode-leaked" not in restored

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

    def remote_proof():
        fixture = runtime / "endpoint-fixture.py"
        fixture.write_text('''import json, os, sys, time
from pathlib import Path
mode, marker = sys.argv[1:3]
if mode == "tail":
    Path(marker).write_text(str(os.getpid()))
    after, count, epoch = 0, 2, "def2"
else:
    with Path(marker).open("a") as output:
        output.write(json.dumps(sys.argv[3:])+"\\n")
    args = sys.argv[3:]
    assert args[:6] == ["activity","poll","--limit","256","--host-label","remote-query"]
    after = int(args[7].split(":")[1]) if len(args) == 8 else 0
    count, epoch = 300, "abc1"
end = min(after+256, count)
records = [{"sequence":n,"received_at_ms":1000+n,"event":{"schema_version":1,"seat":"remote-seat","run_id":"run-remote","event_id":f"event-{n}","source":"hooks","kind":"test_failed"}} for n in range(after+1,end+1)]
page = {"epoch":epoch,"oldest_sequence":1,"current_sequence":count,"records":records,"cursor":{"epoch":epoch,"sequence":end},"has_more":end<count,"reset":None}
if mode == "tail":
    print(json.dumps({"type":"activity","page":page}), flush=True)
    while True: time.sleep(1)
else:
    print(json.dumps({"schema_version":1,"host":"remote-query","page":page},indent=2),flush=True)
''')
        query_log = runtime / "query-arguments.jsonl"
        pid_file = runtime / "tail-helper.pid"
        query_argv = [sys.executable,str(fixture),"query",str(query_log)]
        tail_argv = [sys.executable,str(fixture),"tail",str(pid_file)]
        cli("endpoint","add","remote-query","--command-query",json.dumps(query_argv))
        cli("endpoint","add","remote-tail","--command-tail",json.dumps(tail_argv))
        cli("endpoint","add","other-socket","--socket",str(endpoint))
        cli("enable",connector,"--allow","activity.read:local","--allow","activity.read:remote-query","--allow","activity.read:remote-tail","--allow","activity.read:other-socket")
        cli("enable",monitor,"--allow","activity.read:local","--allow","activity.read:remote-query","--allow","activity.read:remote-tail","--allow","activity.read:other-socket")
        deadline = time.monotonic()+15
        while True:
            reply = cli("invoke",connector,"seats",json.dumps({"host":"remote-query"}))
            value = json.loads(reply["json"])
            if not reply["is_error"]: break
            assert time.monotonic() < deadline, value
            time.sleep(0.25)
        assert value["seats"][0]["retained_events"] == 300 and value["cursor"] == {"epoch":"abc1","sequence":300}, value
        text = cli("run",monitor,"--snapshot","--cols","100","--rows","24","--endpoint","remote-tail",text=True)
        assert "remote-tail" in text and "remote-seat" in text and "test_failed" in text, text
        launcher = bins / f"pmux-activity-monitor{suffix}"
        result = subprocess.run([str(launcher),"--socket",str(endpoint),"--host-cmd","remote-tail",json.dumps(tail_argv),"--snapshot","--size","100x24"],env=env,capture_output=True,text=True,timeout=45)
        assert result.returncode == 0 and "remote-seat" in result.stdout, (result.stdout,result.stderr)
        result = subprocess.run([str(launcher),"--socket",str(endpoint),"--host",f"other-socket={endpoint}","--snapshot"],env=env,capture_output=True,text=True,timeout=45)
        assert result.returncode == 0 and "other-socket" in result.stdout, (result.stdout,result.stderr)
        result = subprocess.run([str(launcher),"--socket",str(endpoint),"--seats"],env=env,capture_output=True,text=True,timeout=45)
        assert result.returncode == 0 and json.loads(result.stdout)["retained_history_only"] is True, result.stderr
        arguments = [json.loads(line) for line in query_log.read_text().splitlines()]
        assert arguments and all(args == ["activity","poll","--limit","256","--host-label","remote-query"] or args == ["activity","poll","--limit","256","--host-label","remote-query","--cursor","abc1:256"] or args == ["activity","poll","--limit","256","--host-label","remote-query","--cursor","abc1:300"] for args in arguments), arguments
        cli("endpoint","remove","remote-tail")
        assert "activity.read:remote-tail" not in cli("inspect",connector)["state"]["grants"]
        wait_gone(int(pid_file.read_text()))
        cli("endpoint","remove","remote-query")
        cli("endpoint","remove","other-socket")
        return tail_argv, pid_file

    def wait_gone(pid):
        def alive():
            if os.name == "nt":
                import ctypes
                from ctypes import wintypes
                kernel = ctypes.WinDLL("kernel32",use_last_error=True)
                kernel.OpenProcess.argtypes = [wintypes.DWORD,wintypes.BOOL,wintypes.DWORD]
                kernel.OpenProcess.restype = wintypes.HANDLE
                handle = kernel.OpenProcess(0x1000,False,pid)
                if not handle: return False
                code = wintypes.DWORD()
                kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE,ctypes.POINTER(wintypes.DWORD)]
                kernel.CloseHandle.argtypes = [wintypes.HANDLE]
                active = kernel.GetExitCodeProcess(handle,ctypes.byref(code)) and code.value == 259
                kernel.CloseHandle(handle)
                return active
            try: os.kill(pid,0); return True
            except ProcessLookupError: return False
        deadline = time.monotonic()+5
        while alive() and time.monotonic() < deadline: time.sleep(0.05)
        assert not alive(), f"owned endpoint helper {pid} survived cancellation"

    def endpoint_pressure():
        labels = [f"source-{index:02}" for index in range(31)]
        for index in range(512):
            event = {"schema_version":1,"seat":"quota-seat","run_id":"run-quota","event_id":f"quota-{index}","source":"hooks","kind":"waiting"}
            assert connection.request("publish_activity",event=event)["status"] == "ok"
        for label in labels:
            cli("endpoint","add",label,"--socket",str(endpoint))
        grants = ["--allow","activity.read:local"]
        for label in labels:
            grants.extend(["--allow",f"activity.read:{label}"])
        cli("enable",monitor,*grants)
        text = cli("run",monitor,"--snapshot","--cols","100","--rows","60",text=True)
        assert "32 waiting" in text and "quota-seat" in text, text
        for label in labels:
            cli("endpoint","remove",label)

    def legacy_import():
        marker = scratch / "unexpected-helper-start"
        hosts = scratch / "legacy-hosts.toml"
        long_label = "c" * 64
        query = [sys.executable,"-c",f"from pathlib import Path; Path({str(marker)!r}).touch()"]
        hosts.write_text('min_interval_ms = 1000\n[[host]]\nlabel = "local"\nsocket = "default"\n'
            + f'[[host]]\nlabel = "{long_label}"\ncommand = {json.dumps(query)}\n')
        arguments = ["--host",f"legacy-socket={endpoint}","--host-cmd","a-24-character-host-label",json.dumps(query),"--activity-hosts",str(hosts)]
        journal = endpoint.with_suffix(".plugins.json")
        before = journal.read_bytes()
        preview = cli("endpoint","import",*arguments)
        assert preview["applied"] is False and len(preview["proposed_endpoints"]) == 3
        assert journal.read_bytes() == before and not marker.exists()
        imported = cli("endpoint","import",*arguments,"--apply")["endpoints"]
        assert [item["source"]["transport"] for item in imported] == ["socket","command_tail","command_query"]
        assert cli("endpoint","import",*arguments,"--apply")["endpoints"] == imported
        assert not marker.exists()
        assert cli("inspect",monitor)["state"]["grants"] == ["activity.read:local"]
        before = journal.read_bytes()
        cli("endpoint","import","--apply","--host",f"must-not-appear={endpoint}","--host","legacy-socket=/tmp/conflicting.sock",succeeds=False)
        assert journal.read_bytes() == before
        assert all(item["label"] != "must-not-appear" for item in cli("endpoint","list")["endpoints"])
        for item in imported:
            cli("endpoint","remove",item["label"])
        # Count alone cannot bound the durable retry cache. Several successful
        # imports must remain possible when each cached reply is substantial.
        large_argv = json.dumps(["not-executed", *(["x" * 3500] * 3)])
        large = [f'[[host]]\nlabel = "large-{index}"\ncommand = {large_argv}\n' for index in range(20)]
        hosts.write_text("".join(large))
        imported = cli("endpoint","import","--activity-hosts",str(hosts),"--apply")["endpoints"]
        for _ in range(6):
            assert cli("endpoint","import","--activity-hosts",str(hosts),"--apply")["endpoints"] == imported
            assert journal.stat().st_size <= 1024 * 1024
        for item in imported:
            cli("endpoint","remove",item["label"])

    def monitor_frame_proof():
        text = cli("run", monitor, "main", "--snapshot", "--cols", "100", "--rows", "24", text=True)
        assert "fixture-seat" in text and "test_failed" in text
        probe_binary = frame_probe or bins / f"examples/plugin_frame_probe{suffix}"
        probe = subprocess.run([str(probe_binary), str(endpoint)], env=env, capture_output=True, text=True, timeout=45)
        assert probe.returncode == 0, probe.stderr
        frame = json.loads(probe.stdout)
        assert frame["cells"] == 2400 and "fixture-seat" in frame["text"]
        # The 12-column seat name leaves a 53-cell lane at 8 pixels per cell.
        assert frame["rasters"] == [{"width":424,"height":16,"bytes":27136}] * 2, frame["rasters"]
        assert frame["failure_pixels"] > 0

    try:
        setting(False)
        daemon, connection = start()
        pid = daemon.pid
        baseline = connection.request("snapshot")["response"]["snapshot"]["sessions"]
        monitor = "org.moonbase.activity-monitor"
        if feature == "production":
            assert expected_catalog is not None
            assert {entry["id"] for entry in expected_catalog} == {monitor, "org.moonbase.activity-connector"}
            assert sorted(cli("discover")["packages"], key=lambda entry: (entry["id"],entry["version"])) == sorted(expected_catalog, key=lambda entry: (entry["id"],entry["version"]))
            assert cli("list")["installed"] == []
            for entry in expected_catalog:
                package = cli("install",entry["id"],"--version",entry["version"])["package"]
                assert package["digest"] == entry["sha256"]
            connector = "org.moonbase.activity-connector"
            cli("run",monitor,"--snapshot",succeeds=False)
            cli("invoke",connector,"seats",succeeds=False)
            assert connection.plugin("exports",allowlist=[connector])["response"]["reply"]["tools"] == []
            for entry in expected_catalog:
                cli("enable",entry["id"],"--version",entry["version"],"--allow","activity.read:local")
            epoch = feed(True)
            event = {"schema_version":1,"seat":"fixture-seat","run_id":"artifact-proof","event_id":"failed-test","source":"hooks","kind":"test_failed"}
            assert connection.request("publish_activity",event=event)["status"] == "ok"
            monitor_frame_proof()
            reply = cli("invoke",connector,"seats")
            assert not reply["is_error"] and json.loads(reply["json"])["seats"][0]["seat"] == "fixture-seat"
            mcp_proof(["--activity-only"],True)
            mcp_proof(["--activity-only","--supervise"],True)
            exports = connection.plugin("exports",allowlist=[connector])["response"]["reply"]["tools"]
            assert len(exports) == 4
            cli("disable",connector)
            assert connection.plugin("exports",allowlist=[connector])["response"]["reply"]["tools"] == []
            assert connection.plugin("call_export",handle=exports[0]["handle"],arguments="{}")["status"] == "error"
            cli("enable",connector,"--version","1.0.0","--allow","activity.read:local")
            feed(False)
            reply = cli("invoke",connector,"poll")
            assert reply["is_error"] and json.loads(reply["json"])["error"]["code"] == "feed_disabled"
            assert feed(True) != epoch and daemon.pid == pid
            reply = cli("invoke",connector,"poll")
            assert not reply["is_error"] and json.loads(reply["json"])["page"]["records"] == []
            assert connection.request("publish_activity",event={**event,"event_id":"after-reenable"})["status"] == "ok"
            assert connection.request("snapshot")["response"]["snapshot"]["sessions"] == baseline
            terminal_view()
            stop(daemon)
            assert sorted(cli("discover")["packages"],key=lambda entry: (entry["id"],entry["version"])) == sorted(expected_catalog,key=lambda entry: (entry["id"],entry["version"]))
            saved = cli("list")
            assert len(saved["installed"]) == 2 and all(state["enabled"] and state["observed"] == "daemon_unavailable" for state in saved["states"])
            print("PASS exact production catalog/archive digests; empty store; installation stays disabled; explicit grants; real monitor cells/RGBA; connector/direct+supervised MCP; live terminal and hot feed without daemon restart; offline verified metadata; guests were not rebuilt")
            return
        if feature != "all":
            feed(True)
            cli("install",monitor)
            cli("enable",monitor,"--allow","activity.read:local")
            if feature == "endpoint-pressure":
                endpoint_pressure()
                print("PASS32 endpoints with512 retained events each")
            else:
                legacy_import()
                print("PASS atomic legacy import, stable identities, byte-bounded retry cache, and no helper launch")
            return
        assert len(cli("discover")["packages"]) == 4
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
        monitor = "org.moonbase.activity-monitor"
        cli("install", monitor)
        cli("enable", monitor, "--allow", "activity.read:local")
        failed = {**event, "event_id": "event-2", "kind": "test_failed"}
        assert connection.request("publish_activity", event=failed)["status"] == "ok"
        monitor_frame_proof()
        assert connection.plugin("exports", allowlist=[])["response"]["reply"]["tools"] == []
        connector = "org.moonbase.activity-connector"
        cli("install", connector)
        cli("enable", connector, "--allow", "activity.read:local")
        connector_exports = connection.plugin("exports", allowlist=[connector])["response"]["reply"]["tools"]
        assert sorted(export["name"] for export in connector_exports) == ["pmux_activity_hosts", "pmux_activity_poll", "pmux_activity_recent", "pmux_activity_seats"]

        def query(name, arguments=None):
            handle = next(export["handle"] for export in connector_exports if export["name"] == f"pmux_activity_{name}")
            response = connection.plugin("call_export", handle=handle, arguments=json.dumps(arguments or {}))
            assert response["status"] == "ok", response
            reply = response["response"]["reply"]
            return json.loads(reply["json"]), reply["is_error"]

        assert query("hosts") == ({"schema_version":1,"min_interval_ms":1000,"hosts":[{"label":"local","endpoint":"local"}]}, False)
        summary, is_error = query("seats")
        assert not is_error and summary["seats"][0]["seat"] == "fixture-seat"
        assert summary["seats"][0]["last_kind"] == "test_failed"
        limited, is_error = query("poll")
        assert is_error and limited["error"]["code"] == "rate_limited" and limited["error"]["retry_after_ms"] == 1000
        time.sleep(1.01)
        recent, is_error = query("recent", {"seat":"fixture-seat","limit":1})
        assert not is_error and recent["records"][0]["event"]["kind"] == "test_failed"
        time.sleep(1.01)
        filtered, is_error = query("poll", {"seat":"other-seat"})
        assert not is_error and filtered["page"]["records"] == []
        assert filtered["next_cursor"] == f"{second_epoch}:1"
        feed(False)
        time.sleep(1.01)
        disabled, is_error = query("poll")
        assert is_error and disabled["error"]["code"] == "feed_disabled"
        feed(True)
        mcp_proof(["--activity-only"], True)
        mcp_proof(["--activity-only","--supervise"], True)
        mcp_proof(["--as","fixture-agent"], False)
        for arguments in [["--activity-only"],["--activity-only","--supervise"]]:
            mcp_rejects_unterminated_input(bins / f"pmux-mcp{suffix}",arguments,env,scratch / "mcp-oversize.log")
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
        candidate = cli("inspect", PLUGIN, "--version", "1.0.1")
        assert candidate["manifest"]["version"] == "1.0.1" and candidate["state"]["selected"]["version"] == "1.0.0"
        assert tool("counter") == {"counter": 4}
        cli("enable", PLUGIN, "--allow", "activity.read:local")
        assert cli("inspect", PLUGIN)["state"]["selected"]["version"] == "1.0.0"
        assert tool("counter") == {"counter": 5}
        assert daemon.poll() is None and daemon.pid == pid
        assert connection.request("snapshot")["response"]["snapshot"]["sessions"] == baseline
        tail_argv, tail_pid = remote_proof()
        legacy_import()
        endpoint_pressure()
        assert connection.request("publish_activity", event={**event,"event_id":"before-live-view"})["status"] == "ok"
        terminal_view()
        assert daemon.poll() is None and daemon.pid == pid
        cli("disable", PLUGIN)
        assert connection.plugin("call_export", handle=handle, arguments="{}")["status"] == "error"
        cli("invoke", PLUGIN, "counter", succeeds=False)
        cli("endpoint","add","crash-tail","--command-tail",json.dumps(tail_argv))
        cli("enable",connector,"--allow","activity.read:local","--allow","activity.read:crash-tail")
        prior_tail = tail_pid.read_text()
        cli("invoke",connector,"hosts")
        deadline = time.monotonic()+5
        while tail_pid.read_text() == prior_tail and time.monotonic() < deadline: time.sleep(0.05)
        assert tail_pid.read_text() != prior_tail
        crash_pid = int(tail_pid.read_text())
        for item in connections:
            item.close()
        connections.clear()
        daemon.kill()
        daemon.wait(timeout=10)
        wait_gone(crash_pid)
        journal = endpoint.with_suffix(".plugins.json")
        before = journal.read_bytes()
        offline = cli("list")
        assert next(state for state in offline["states"] if state["id"] == connector)["observed"] == "daemon_unavailable"
        assert len(cli("discover")["packages"]) == 4
        assert cli("inspect", PLUGIN, "--version", "1.0.1")["manifest"]["version"] == "1.0.1"
        assert any(endpoint["label"] == "crash-tail" for endpoint in cli("endpoint", "list")["endpoints"])
        cli("disable", connector, succeeds=False)
        cli("install", monitor, succeeds=False)
        assert journal.read_bytes() == before
        daemon, connection = start()
        assert not cli("inspect", PLUGIN)["state"]["enabled"]
        cli("invoke", PLUGIN, "counter", succeeds=False)
        print("PASS install stays disabled; explicit grants; actual Wasm monitor cells/RGBA and connector queries; live terminal input/resize/restore and hot feed epochs; direct/supervised identity-free MCP and default-empty exports; connection-bound handles; schema rejection; failed readiness preserves selection; remote300-event catch-up/socket/tail and thin launcher; endpoint removal and parent-death helper cleanup; exact-version and read-only offline inspection; atomic legacy endpoint import without grants/helper launch;32-endpoint512-event catch-up; durable disable after restart")
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
    parser.add_argument("--bundle", type=Path, help="reuse a verified production artifact without compiling guests")
    parser.add_argument("--bins", type=Path, help="drive existing packaged production executables after verifying their physical bundle identities; build nothing")
    parser.add_argument("--frame-probe", type=Path, help="separately built native plugin_frame_probe helper, required with --bins")
    parser.add_argument("--toolchain", default="1.90.0")
    parser.add_argument("--release", action="store_true", dest="release", default=True, help="exercise optimized native binaries as shipped (default)")
    parser.add_argument("--debug", action="store_false", dest="release", help="use slower debug native binaries for local diagnostics")
    parser.add_argument("--feature", choices=["all","endpoint-pressure","legacy-import","production"], default="all")
    args = parser.parse_args()
    if bool(args.bundle) != (args.feature == "production"):
        parser.error("--bundle and --feature production must be supplied together")
    if args.bins and (args.feature != "production" or not args.frame_probe):
        parser.error("--bins requires --bundle, --feature production, and --frame-probe")
    if args.frame_probe and not args.bins:
        parser.error("--frame-probe requires --bins")
    with tempfile.TemporaryDirectory(prefix="pmux-plugin-build-") as work:
        scratch = Path(work)
        if args.bins:
            sys.path.insert(0, str(REPO / "scripts/release"))
            from plugin_bundle_provenance import verify_binaries
            bins = args.bins.resolve()
            identity = verify_binaries(bins, args.bundle)["identity"]
            print("PASS existing packaged consumers match source and bundle: " + json.dumps(identity), flush=True)
        else:
            bins = build(args, scratch)
        expected = json.loads((args.bundle / "pmux-plugins.json").read_bytes())["packages"] if args.bundle else None
        exercise(bins, scratch, args.feature, expected, args.frame_probe.resolve() if args.frame_probe else None)


if __name__ == "__main__":
    main()
