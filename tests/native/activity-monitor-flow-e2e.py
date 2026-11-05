#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""User flow for the Activity Monitor Space, driven through the real host.

The host, pmuxd and seats run in a private scratch tree. Native input goes to
the host only: Quartz events posted to its PID on macOS, xdotool aimed at its
window on Linux (X11 or Xvfb), and SendInput after focusing its window on
Windows. Every step is asserted against what pmux reports: the saved Spaces,
the host's render status, and the monitor pane's text from `pmux save-buffer`.

  activity-monitor-flow-e2e.py --bins target/release --out /tmp/flow

Steps, each recorded in result.json with PASS or FAIL:
  open        palette -> Activity Monitor: the Space, its tab and the monitor
  reopen      palette again: the same Space and session, no second monitor
  keys        every list, timeline, details and help key, checked on screen
  quit        q leaves the session's shell; the palette starts the monitor again
  close       close the tab and remove the Space: no monitor process is left
"""
import argparse
import hashlib
import importlib.util
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from native_control import control_socket  # noqa: E402

SPACE = "Activity Monitor"
SEATS = ["alpha-seat", "bravo-seat", "charlie-seat", "delta-seat"]
# The monitor's own session is a working seat, so it heads the list.
ROWS = ["activity-monitor"] + sorted(SEATS)
PALETTE = "ctrl+alt+f7"
CLOSE_TAB = "ctrl+alt+f8"
WINDOWS = os.name == "nt"
EXE = ".exe" if WINDOWS else ""


class Failed(AssertionError):
    pass


def wait_for(check, label, timeout=20.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
            last = error
        time.sleep(0.1)
    raise Failed(f"timed out: {label}" + (f" ({last})" if last else ""))


# Each child leads its own process group, so teardown reaches whatever it
# started (pane shells, the monitor, a launcher) and not this script.
GROUP = ({"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if sys.platform == "win32"
         else {"start_new_session": True})


def kill_tree(process):
    if sys.platform == "win32":
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(process.pid)], capture_output=True, timeout=60)
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(10)
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        process.wait(10)
    except subprocess.TimeoutExpired:
        process.kill()


def kill_pid(pid):
    if sys.platform == "win32":
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(pid)], capture_output=True, timeout=60)
    else:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def poll(check, timeout):
    """Like wait_for, but a timeout returns False so the caller can retry."""
    try:
        return wait_for(check, "poll", timeout)
    except Failed:
        return False


# ---------------------------------------------------------------------------
# Native input. Each backend sends one named key, optionally with modifiers,
# and types plain text.

MAC_KEYS = {
    "up": 126, "down": 125, "left": 123, "right": 124, "pageup": 116, "pagedown": 121,
    "home": 115, "end": 119, "enter": 36, "escape": 53, "tab": 48, "backspace": 51,
    "f7": 98, "f8": 100, "space": 49, "/": 44, "_": 27, "-": 27,
}
MAC_LETTERS = {"a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
               "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "o": 31, "u": 32,
               "i": 34, "p": 35, "l": 37, "j": 38, "k": 40, "n": 45, "m": 46}
MAC_FLAGS = {"shift": 1 << 17, "ctrl": 1 << 18, "alt": 1 << 19, "cmd": 1 << 20}


class MacInput:
    def __init__(self, pid):
        import ctypes
        self.pid = pid
        self.c = ctypes
        quartz = ctypes.CDLL("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices")
        quartz.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
        quartz.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
        quartz.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
        quartz.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
        quartz.CFRelease.argtypes = [ctypes.c_void_p]
        self.quartz = quartz
        # Keys posted to a background app are dropped; bring the host forward.
        subprocess.run(["osascript", "-e", 'tell application "System Events" to set frontmost of '
                        f'(first process whose unix id is {pid}) to true'], capture_output=True, timeout=20)
        time.sleep(0.5)

    def _post(self, code, flags):
        for pressed in (True, False):
            event = self.quartz.CGEventCreateKeyboardEvent(None, code, pressed)
            assert event, "keyboard event allocation"
            self.quartz.CGEventSetFlags(event, flags)
            self.quartz.CGEventPostToPid(self.pid, event)
            self.quartz.CFRelease(event)
        time.sleep(0.08)

    def key(self, chord):
        *mods, name = chord.split("+")
        flags = 0
        for mod in mods:
            flags |= MAC_FLAGS[mod]
        if name in MAC_LETTERS:
            code = MAC_LETTERS[name]
        elif len(name) == 1 and name.isupper():
            code, flags = MAC_LETTERS[name.lower()], flags | MAC_FLAGS["shift"]
        elif name == "?":
            code, flags = MAC_KEYS["/"], flags | MAC_FLAGS["shift"]
        elif name == "_":
            code, flags = MAC_KEYS["_"], flags | MAC_FLAGS["shift"]
        else:
            code = MAC_KEYS[name]
        self._post(code, flags)
        time.sleep(0.15)

    def type(self, text):
        for ch in text:
            self.key("space" if ch == " " else ch)


XDO_KEYS = {"up": "Up", "down": "Down", "left": "Left", "right": "Right", "pageup": "Prior",
            "pagedown": "Next", "home": "Home", "end": "End", "enter": "Return", "escape": "Escape",
            "tab": "Tab", "backspace": "BackSpace", "f7": "F7", "f8": "F8", "?": "question",
            "_": "underscore", "space": "space"}


class XdotoolInput:
    def __init__(self, pid):
        out = wait_for(lambda: subprocess.check_output(
            ["xdotool", "search", "--onlyvisible", "--pid", str(pid)], text=True).strip(),
            "host window")
        self.window = out.splitlines()[-1]
        subprocess.run(["xdotool", "windowactivate", "--sync", self.window], check=False)
        subprocess.run(["xdotool", "windowfocus", "--sync", self.window], check=True)

    def key(self, chord):
        *mods, name = chord.split("+")
        name = XDO_KEYS.get(name, name)
        sym = "+".join([{"ctrl": "ctrl", "alt": "alt", "shift": "shift"}[m] for m in mods] + [name])
        # No --window: that sends synthetic events, which the host ignores.
        # XTEST input goes to the focused window, which is the host.
        subprocess.run(["xdotool", "windowfocus", "--sync", self.window], check=True)
        subprocess.run(["xdotool", "key", "--clearmodifiers", sym], check=True)
        time.sleep(0.15)

    def type(self, text):
        subprocess.run(["xdotool", "windowfocus", "--sync", self.window], check=True)
        subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "40", text], check=True)
        time.sleep(0.15)


WIN_VK = {"up": 0x26, "down": 0x28, "left": 0x25, "right": 0x27, "pageup": 0x21, "pagedown": 0x22,
          "home": 0x24, "end": 0x23, "enter": 0x0D, "escape": 0x1B, "tab": 0x09, "backspace": 0x08,
          "f7": 0x76, "f8": 0x77, "space": 0x20}
WIN_MOD = {"shift": 0x10, "ctrl": 0x11, "alt": 0x12}
EXTENDED = {0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28}


class WindowsInput:
    def __init__(self, pid):
        import ctypes
        from ctypes import wintypes
        self.c, self.w = ctypes, wintypes
        self.user = ctypes.WinDLL("user32", use_last_error=True)
        self.pid = pid
        self.hwnd = wait_for(self._find, "host window")
        self._focus()

    def _find(self):
        found = []
        proc = self.c.WINFUNCTYPE(self.c.c_bool, self.w.HWND, self.w.LPARAM)

        def each(hwnd, _):
            owner = self.w.DWORD()
            self.user.GetWindowThreadProcessId(hwnd, self.c.byref(owner))
            if owner.value == self.pid and self.user.IsWindowVisible(hwnd):
                found.append(hwnd)
            return True

        self.user.EnumWindows(proc(each), 0)
        return found[0] if found else None

    def _focus(self):
        self.user.ShowWindow(self.hwnd, 9)
        self.user.SetForegroundWindow(self.hwnd)
        time.sleep(0.3)

    def _vk(self, vk, up):
        extended = 0x1 if vk in EXTENDED else 0
        self.user.keybd_event(vk, 0, extended | (0x2 if up else 0), 0)

    def key(self, chord):
        *mods, name = chord.split("+")
        if self.user.GetForegroundWindow() != self.hwnd:
            self._focus()
        if name in WIN_VK:
            vk, shift = WIN_VK[name], False
        else:
            scan = self.user.VkKeyScanW(ord(name))
            vk, shift = scan & 0xFF, bool(scan & 0x100)
        held = [WIN_MOD[m] for m in mods] + ([WIN_MOD["shift"]] if shift and "shift" not in mods else [])
        for mod in held:
            self._vk(mod, False)
        self._vk(vk, False)
        self._vk(vk, True)
        for mod in reversed(held):
            self._vk(mod, True)
        time.sleep(0.15)

    def type(self, text):
        for ch in text:
            self.key("space" if ch == " " else ch)


def native_input(pid):
    system = platform.system()
    if system == "Darwin":
        return MacInput(pid)
    if system == "Windows":
        return WindowsInput(pid)
    return XdotoolInput(pid)


# ---------------------------------------------------------------------------
# Processes and pmux.

KEY_BYTES = {
    "up": "\x1b[A", "down": "\x1b[B", "right": "\x1b[C", "left": "\x1b[D",
    "home": "\x1b[H", "end": "\x1b[F", "pageup": "\x1b[5~", "pagedown": "\x1b[6~",
    "enter": "\r", "escape": "\x1b", "backspace": "\x7f", "tab": "\t",
    "shift+tab": "\x1b[Z", "space": " ",
}


class PaneInput:
    """Keys written to the monitor pane's terminal with `pmux send`."""

    def __init__(self, flow):
        self.flow = flow

    def key(self, chord):
        pane = self.flow.monitor_pane()["id"]
        self.flow.cli("send", pane, "--literal", "--force", "--", KEY_BYTES.get(chord, chord))

    def type(self, text):
        for character in text:
            self.key(character)


class Flow:
    def __init__(self, bins, out, headless=False):
        self.bins, self.out = bins, out
        # headless: no GUI host. Keys go to the pane's terminal as bytes, the way the
        # host writes them, so this runs where no desktop session exists.
        self.headless = headless
        # Short root: Unix socket paths have a small limit.
        self.root = Path(tempfile.mkdtemp(prefix="amf-"))
        runtime = self.root / "run"
        runtime.mkdir(mode=0o700)
        if WINDOWS:
            # pmuxd requires an owner-only, inheritable DACL on the socket's parent.
            spec = importlib.util.spec_from_file_location(
                "plugin_runtime", Path(__file__).resolve().parents[2] / "scripts" / "test-plugin-runtime.py")
            helper = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(helper)
            helper.private_windows_directory(runtime)
        for name in ("home", "config", "data", "state"):
            (self.root / name).mkdir()
        self.config = self.root / "config.toml"
        self.config.write_text(
            'splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
            'install_agent_skills = false\nspace_rail = "left"\n'
            f'[keys]\ncommand_palette = "{PALETTE}"\nclose_tab = "{CLOSE_TAB}"\n'
            '[mux]\nexperimental_activity = true\n')
        env = {k: v for k, v in os.environ.items() if not k.startswith(("PMUX", "PRISMATTYC_", "XDG_"))}
        socket_path = runtime / "pmux.sock"
        env.update(HOME=str(self.root / "home"), PMUX_SOCKET=str(socket_path),
                   XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(self.root / "config"),
                   XDG_DATA_HOME=str(self.root / "data"), XDG_STATE_HOME=str(self.root / "state"),
                   PRISMATTYC_CONFIG=str(self.config), PRISMATTYC_NO_AGENT_SKILLS="1",
                   PRISMATTYC_DUMP_PRESENT=str(out / "present.png"),
                   PATH=f"{bins}{os.pathsep}{env.get('PATH', '')}")
        if WINDOWS:
            env["USERPROFILE"] = env["HOME"]
            env["LOCALAPPDATA"] = str(self.root / "data")
        else:
            env["SHELL"] = "/bin/sh"
        self.env, self.socket = env, socket_path
        self.daemon = self.host = None
        self.steps = []

    def cli(self, *args, check=True, timeout=30):
        done = subprocess.run([str(self.bins / f"pmux{EXE}"), *map(str, args)], env=self.env,
                              capture_output=True, text=True, encoding="utf-8", timeout=timeout)
        if check and done.returncode:
            raise subprocess.CalledProcessError(done.returncode, args, done.stdout, done.stderr)
        return done.stdout

    def request(self, kind, **fields):
        stream = control_socket(self.socket)
        try:
            stream.sendall((json.dumps({"version": 1, "request_id": 1, "type": kind, **fields}) + "\n").encode())
            return json.loads(stream.makefile("rb").readline(8 << 20))
        finally:
            stream.close()

    def snapshot(self):
        return self.request("snapshot")["response"]["snapshot"]

    def status(self):
        data = json.loads(self.cli("render-status", "--json"))
        if data.get("host_pid") != self.host.pid or not data.get("windows"):
            return {}
        return data["windows"][0]

    def start(self):
        log = (self.out / "daemon.log").open("w")
        shell = ["cmd.exe"] if WINDOWS else ["/bin/sh"]
        self.daemon = subprocess.Popen([str(self.bins / f"pmuxd{EXE}"), "--socket", str(self.socket), "--", *shell],
                                       env=self.env, stdout=log, stderr=log, **GROUP)
        wait_for(lambda: self.socket.exists(), "daemon socket")
        # A build with default plug-ins enables both; otherwise enable them here.
        def enabled():
            listing = json.loads(self.cli("plugin", "list"))
            return {state.get("id") for state in listing.get("states", []) if state.get("enabled") or state.get("state") == "enabled"}

        time.sleep(3)
        self.seeded = {"org.moonbase.activity-monitor", "org.moonbase.activity-connector"} <= enabled()
        if not self.seeded and not self.headless:
            for plugin in ("org.moonbase.activity-connector", "org.moonbase.activity-monitor"):
                self.cli("plugin", "install", plugin)
                self.cli("plugin", "enable", plugin, "--allow", "activity.read:local")
        program = "cmd.exe" if WINDOWS else "/bin/sh"
        argv = ["/d", "/c", "pause > nul"] if WINDOWS else ["-c", "read -r line"]
        for seat in SEATS:
            reply = self.request("create_session", name=seat, agent_id=seat, headless=True,
                                 spawn={"program": program, "argv": argv, "cwd": None, "env": {}})
            assert reply["status"] == "ok", reply
            run = hashlib.sha256(seat.encode()).hexdigest()
            for index, kind in enumerate(["turn_started", "test_started", "test_passed", "turn_finished"] * 3):
                event = {"schema_version": 1, "seat": seat, "run_id": run, "source": "hooks", "kind": kind,
                         "event_id": hashlib.sha256(f"{seat}:{index}".encode()).hexdigest()}
                reply = self.request("publish_activity", event=event)
                assert reply["status"] == "ok", reply
        if self.headless:
            self.keys = PaneInput(self)
            return
        log = (self.out / "host.log").open("w")
        self.host = subprocess.Popen([str(self.bins / f"prismattyc-host{EXE}"), "--no-splash"],
                                     env=self.env, stdout=log, stderr=log, **GROUP)
        wait_for(lambda: self.status().get("space") is not None or self.status(), "host render status", 60)
        self.keys = native_input(self.host.pid)
        self.wait_ready()

    def stop(self):
        """Tear down the host, the daemon and everything they started.

        Returns what was still running from this build's binaries after
        both trees were stopped, after killing it too.
        """
        for process in (self.host, self.daemon):
            if process and process.poll() is None:
                kill_tree(process)
        time.sleep(1)
        swept = self.leftovers()
        for pid, command in swept:
            kill_pid(pid)
        return [command for _, command in swept]

    def leftovers(self):
        """Processes still running from this build's binaries, as (pid, command)."""
        bins = str(self.bins)
        if WINDOWS:
            script = ("Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and "
                      f"$_.ExecutablePath.StartsWith('{bins}', 'OrdinalIgnoreCase') }} | "
                      "ForEach-Object { \"$($_.ProcessId)`t$($_.Name)\" }")
            out = subprocess.run(["powershell", "-NoProfile", "-Command", script],
                                 capture_output=True, text=True, timeout=60).stdout
        else:
            out = subprocess.run(["ps", "-A", "-o", "pid=,command="], capture_output=True, text=True).stdout
            # Match the executable only: this script's own command line names
            # the binaries directory too.
            rows = (line.strip().partition(" ") for line in out.splitlines())
            out = "\n".join(f"{pid}\t{command}" for pid, _, command in rows if command.startswith(bins + os.sep))
        found = []
        for line in out.splitlines():
            pid, _, command = line.partition("\t")
            if pid.strip().isdigit() and int(pid) != os.getpid():
                found.append((int(pid), Path(command.split()[0]).name if command else "?"))
        return found

    # -- observations -------------------------------------------------------

    def monitor_session(self):
        return next((s for s in self.snapshot()["sessions"] if s["name"] == SPACE), None)

    def monitor_pane(self):
        session = self.monitor_session()
        return session["windows"][0]["panes"][0] if session else None

    def screen(self):
        pane = self.monitor_pane()
        return self.cli("save-buffer", str(pane["id"]), "-") if pane else ""

    def selected(self):
        for line in self.screen().splitlines():
            if "▌" in line:
                for seat in ROWS:
                    if seat in line:
                        return seat
        return None

    def space_sessions(self):
        for line in self.cli("space", "ls").splitlines():
            if line.startswith(SPACE + "  "):
                return int(line[len(SPACE):].split()[0])
        return None

    def monitor_processes(self):
        name = f"pmux-activity-monitor{EXE}"
        if WINDOWS:
            out = subprocess.run(["tasklist", "/FI", f"IMAGENAME eq {name}", "/FO", "CSV", "/NH"],
                                 capture_output=True, text=True).stdout
            return [line for line in out.splitlines() if name.lower() in line.lower()]
        out = subprocess.run(["ps", "-A", "-o", "pid=,command="], capture_output=True, text=True).stdout
        mine = str(self.bins / name)
        return [line for line in out.splitlines() if mine in line]

    # -- steps --------------------------------------------------------------

    def step(self, name, body):
        started = time.monotonic()
        record = {"step": name}
        try:
            record.update(body() or {})
            record["status"] = "PASS"
        except Exception as error:  # recorded, then re-raised after the report
            record.update(status="FAIL", error=f"{type(error).__name__}: {error}")
            self.diagnose(name)
            raise
        finally:
            record["seconds"] = round(time.monotonic() - started, 2)
            self.steps.append(record)
            print(f"{record['status']} {name}", flush=True)

    def diagnose(self, name):
        """Everything pmux can say about a failed step, next to result.json."""
        report = {}
        for label, probe in [("render_status", lambda: json.loads(self.cli("render-status", "--json"))),
                             ("spaces", lambda: self.cli("space", "ls")),
                             ("sessions", lambda: self.cli("ls"))]:
            try:
                report[label] = probe()
            except Exception as error:  # a broken probe must not hide the others
                report[label] = f"{type(error).__name__}: {error}"
        screens = {}
        try:
            for session in self.snapshot()["sessions"]:
                for window in session["windows"]:
                    for pane in window["panes"]:
                        screens[f"{session['name']}/{pane['id']}"] = self.cli("save-buffer", str(pane["id"]), "-", check=False)
        except Exception as error:
            screens["error"] = str(error)
        report["screens"] = screens
        (self.out / f"diagnose-{name}.json").write_text(json.dumps(report, indent=2) + "\n")

    def palette(self):
        """The open palette's query, or None while it is closed."""
        return self.status().get("palette")

    def palette_open(self):
        # Native input can be lost while a window settles, so confirm each
        # stage through render-status rather than trusting the keys landed.
        query = "activity monitor"
        last = None
        for _ in range(3):
            if self.palette() is not None:
                self.keys.key("escape")
                wait_for(lambda: self.palette() is None, "palette closed before retry", 10)
            self.keys.key(PALETTE)
            if not poll(lambda: self.palette() == "", 5):
                last = "the palette shortcut did not open the palette"
                continue
            self.keys.type(query)
            if not poll(lambda: self.palette() == query, 5):
                last = f"palette query was {self.palette()!r}"
                continue
            self.keys.key("enter")
            wait_for(lambda: self.palette() is None, "palette ran the entry and closed", 10)
            return
        raise Failed(f"palette never took the query: {last}")

    def wait_ready(self):
        """The host has focus and the default pane's shell has drawn a prompt."""
        def ready():
            if not self.status().get("focused"):
                return False
            for session in self.snapshot()["sessions"]:
                if session["name"] == "default":
                    pane = session["windows"][0]["panes"][0]["id"]
                    return bool(self.cli("save-buffer", str(pane), "-", check=False).strip())
            return False
        wait_for(ready, "host focused with a shell prompt", 60)

    def open(self):
        self.palette_open()
        wait_for(lambda: self.space_sessions() == 1, "Space with one session")
        wait_for(lambda: self.status().get("space") == SPACE, "host shows the Space", 30)
        wait_for(lambda: all(seat in self.screen() for seat in SEATS), "monitor lists every seat", 30)
        tabs = self.status().get("tabs") or []
        return {"space": SPACE, "pane": self.monitor_pane()["id"], "tabs": len(tabs),
                "monitor_processes": len(self.monitor_processes())}

    def open_headless(self):
        self.cli("space", "monitor")
        wait_for(lambda: all(seat in self.screen() for seat in SEATS), "monitor lists every seat", 60)
        return {"pane": self.monitor_pane()["id"], "monitor_processes": len(self.monitor_processes())}

    def quit_headless(self):
        self.keys.key("q")
        wait_for(lambda: not any(seat in self.screen() for seat in SEATS), "monitor quit to the shell")
        wait_for(lambda: not self.monitor_processes(), "monitor process exited")
        return {"monitor_processes": 0}

    def reopen(self):
        pane = self.monitor_pane()["id"]
        pid = self.monitor_pane().get("child_pid")
        before = len(self.monitor_processes())
        self.palette_open()
        time.sleep(2)
        wait_for(lambda: self.status().get("space") == SPACE, "host still on the Space")
        assert self.space_sessions() == 1, "a second session appeared"
        assert self.monitor_pane()["id"] == pane, "the monitor pane changed"
        assert self.monitor_pane().get("child_pid") == pid, "the pane restarted"
        assert len(self.monitor_processes()) == before, "a second monitor started"
        return {"pane": pane}

    def keys_step(self):
        checked = []

        def press(chord, expect, label):
            self.keys.key(chord)
            wait_for(expect, f"{label} after {chord}", 10)
            checked.append(f"{chord}: {label}")

        order = ROWS
        press("home", lambda: self.selected() == order[0], "first seat")
        press("down", lambda: self.selected() == order[1], "next seat")
        press("j", lambda: self.selected() == order[2], "next seat")
        press("up", lambda: self.selected() == order[1], "previous seat")
        press("k", lambda: self.selected() == order[0], "previous seat")
        press("end", lambda: self.selected() == order[-1], "last seat")
        press("g", lambda: self.selected() == order[0], "first seat")
        press("G", lambda: self.selected() == order[-1], "last seat")
        press("pageup", lambda: self.selected() == order[0], "page up")
        press("pagedown", lambda: self.selected() == order[-1], "page down")
        press("home", lambda: self.selected() == order[0], "first seat")
        press("enter", lambda: "turn_finished" in self.screen() and "▌" not in self.screen(), "timeline")
        press("down", lambda: True, "timeline scroll")
        press("pageup", lambda: True, "timeline page")
        press("escape", lambda: self.selected() == order[0], "back to the list")
        press("l", lambda: "▌" not in self.screen(), "timeline")
        press("h", lambda: self.selected() == order[0], "back to the list")
        press("right", lambda: "▌" not in self.screen(), "timeline")
        press("left", lambda: self.selected() == order[0], "back to the list")
        press("d", lambda: "▌" not in self.screen() and order[0] in self.screen(), "details")
        press("backspace", lambda: self.selected() == order[0], "back to the list")
        press("tab", lambda: self.selected() is not None, "next host")
        press("shift+tab", lambda: self.selected() is not None, "previous host")
        press("?", lambda: "first or last" in self.screen(), "help")
        press("space", lambda: "first or last" not in self.screen(), "help closed")
        press("f", lambda: True, "filter")
        press("f", lambda: True, "filter")
        press("f", lambda: all(seat in self.screen() for seat in SEATS), "filter back to all")
        press("s", lambda: self.selected() is not None, "sort")
        press("i", lambda: True, "ids")
        press("i", lambda: True, "ids")
        press("p", lambda: True, "pixels")
        press("p", lambda: True, "pixels")
        return {"keys": checked}

    def quit_step(self):
        self.keys.key("q")
        wait_for(lambda: not any(seat in self.screen() for seat in SEATS), "monitor quit to the shell")
        wait_for(lambda: not self.monitor_processes(), "monitor process exited")
        assert self.space_sessions() == 1, "quitting changed the Space"
        self.palette_open()
        wait_for(lambda: all(seat in self.screen() for seat in SEATS), "palette started the monitor again", 30)
        return {"monitor_processes": len(self.monitor_processes())}

    def close_step(self):
        self.keys.key("q")
        wait_for(lambda: not self.monitor_processes(), "monitor exited")
        self.keys.key(CLOSE_TAB)
        wait_for(lambda: self.monitor_session() is None, "tab close ended the session", 30)
        self.cli("space", "rm", SPACE)
        wait_for(lambda: self.space_sessions() is None, "Space removed")
        time.sleep(1)
        assert not self.monitor_processes(), "a monitor process was left"
        assert self.host.poll() is None, "the host exited"
        return {"monitor_processes": 0}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bins", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--no-gui", action="store_true",
                        help="drive the monitor pane through its terminal, without a GUI host")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    flow = Flow(args.bins.resolve(), args.out.resolve(), args.no_gui)
    started = time.monotonic()
    status = "FAIL"
    swept = []

    # A killed run (timeout, cancel, closed terminal) still tears down.
    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)
    for name in ("SIGTERM", "SIGHUP", "SIGBREAK"):
        if hasattr(signal, name):
            signal.signal(getattr(signal, name), interrupted)
    try:
        flow.start()
        if args.no_gui:
            flow.step("open", flow.open_headless)
            flow.step("keys", flow.keys_step)
            flow.step("quit", flow.quit_headless)
        else:
            flow.step("open", flow.open)
            flow.step("reopen", flow.reopen)
            flow.step("keys", flow.keys_step)
            flow.step("quit", flow.quit_step)
            flow.step("close", flow.close_step)
        status = "PASS"
    finally:
        swept = flow.stop()
        if swept:
            # Anything left after both trees stopped is a leak, even in a pass.
            print(f"FAIL leftover processes after teardown: {swept}", flush=True)
            status = "FAIL"
        result = {"status": status, "platform": platform.platform(), "leftovers_after_teardown": swept,
                  "default_plugins_seeded": getattr(flow, "seeded", None), "seconds": round(time.monotonic() - started, 1),
                  "steps": flow.steps}
        (args.out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        shutil.rmtree(flow.root, ignore_errors=True)
    print(f"{status} activity monitor flow", flush=True)
    if status != "PASS":
        sys.exit(1)


if __name__ == "__main__":
    main()
