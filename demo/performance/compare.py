#!/usr/bin/env python3
"""Compare immutable host binaries and Foot in private nested Wayland displays."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def terminate(process):
    if process is not None and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def sample(pid):
    fields = {
        line.split(":")[0]: int(line.split()[1])
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:]
        if ":" in line
    }
    # The parenthesized process name can contain spaces.
    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return dict(time_ns=time.monotonic_ns(), rss_kib=fields["Rss"],
                pss_kib=fields["Pss"],
                private_kib=fields.get("Private_Clean", 0) + fields.get("Private_Dirty", 0),
                cpu_ticks=int(stat[11]) + int(stat[12]))


def run_case(options, label, binary, trial):
    output = options.output / f"{label}-{trial}"
    output.mkdir()
    result = output / "workload.jsonl"
    samples = []
    display = compositor = terminal = None
    with tempfile.TemporaryDirectory(prefix="prismattyc-bench-") as temporary:
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("PMUX", "PRISMATTYC_", "XDG_"))
               and key not in ("WINIT_UNIX_BACKEND", "WAYLAND_DISPLAY", "XAUTHORITY")}
        env.update(HOME=temporary, XDG_RUNTIME_DIR=temporary,
                   WAYLAND_DISPLAY="bench-wayland", SHELL="/bin/bash",
                   LIBGL_ALWAYS_SOFTWARE="1")
        try:
            with (output / "xvfb.log").open("w") as log:
                display = subprocess.Popen(
                    ["Xvfb", "-displayfd", "1", "-screen", "0", "1920x1440x24",
                     "-nolisten", "tcp", "-noreset"],
                    stdout=subprocess.PIPE, stderr=log, start_new_session=True)
            env["DISPLAY"] = ":" + display.stdout.readline().decode().strip()
            with (output / "weston.log").open("w") as log:
                compositor = subprocess.Popen(
                    ["weston", "--backend=x11-backend.so", f"--renderer={options.renderer}",
                     "--socket=bench-wayland", "--idle-time=0", "--width=1600",
                     "--height=1200", "--no-config"], env=env, stdout=log,
                    stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + 15
            while not Path(temporary, "bench-wayland").exists():
                if compositor.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Weston did not start; inspect weston.log")
                time.sleep(0.05)
            time.sleep(0.7)
            config = output / "config.toml"
            config.write_text('splash = false\nfont_px = 16.0\nspace_startup = "fresh"\n'
                              'font = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"\n')
            if label == "foot":
                command = ["foot", "--window-size-chars=80x24",
                           "--font=DejaVu Sans Mono:pixelsize=16",
                           "--override=scrollback.lines=10000"]
            else:
                command = [str(binary), "--no-splash", "--"]
            command += ["python3", str(Path(__file__).with_name("workload.py").resolve())]
            env.update(PRISMATTYC_CONFIG=str(config), BENCH_RESULT=str(result),
                       BENCH_STARTED=str(time.monotonic_ns()), BENCH_SCALE=str(options.scale))
            with (output / "host.log").open("w") as log:
                terminal = subprocess.Popen(command, env=env, stdout=log,
                                            stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline and terminal.poll() is None:
                try:
                    samples.append(sample(terminal.pid))
                except (FileNotFoundError, ProcessLookupError):
                    pass
                if result.exists() and '"phase": "done"' in result.read_text():
                    break
                time.sleep(0.025)
            records = [json.loads(line) for line in result.read_text().splitlines()] if result.exists() else []
            success = bool(records and records[-1]["phase"] == "done")
            success = success and compositor.poll() is None
            success = success and records[0]["rows"] == 24 and records[0]["cols"] == 80
            row = dict(label=label, trial=trial, status="PASS" if success else "FAIL",
                       terminal_exit=terminal.poll(), compositor_exit=compositor.poll(),
                       records=records, peak_rss_kib=max((s["rss_kib"] for s in samples), default=0))
            (output / "samples.json").write_text(json.dumps(samples))
            return row
        finally:
            for process in (terminal, compositor, display):
                terminate(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--scale", type=int, default=1, help="multiply output workloads")
    parser.add_argument("--renderer", choices=("gl", "pixman"), default="gl")
    options = parser.parse_args()
    if options.trials < 1 or options.scale < 1:
        parser.error("--trials and --scale must be positive")
    options.output = options.output.resolve()
    binaries = {name: getattr(options, name).resolve() for name in ("baseline", "candidate")}
    metadata = {name: dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())
                for name, path in binaries.items()}
    for command in ("foot", "weston"):
        metadata[command] = subprocess.check_output([command, "--version"], text=True).strip()
    metadata["renderer"] = options.renderer
    metadata["scale"] = options.scale
    options.output.mkdir(parents=True, exist_ok=False)
    (options.output / "environment.json").write_text(json.dumps(metadata, indent=2))
    summaries = []
    for trial in range(options.trials):
        for label, binary in [*binaries.items(), ("foot", None)]:
            row = run_case(options, label, binary, trial)
            summaries.append(row)
            (options.output / "summary.json").write_text(json.dumps(summaries, indent=2))
            print(json.dumps(row), flush=True)
            if row["status"] != "PASS":
                raise SystemExit(f"Failed {label} trial {trial}; evidence retained. No automatic retry.")


if __name__ == "__main__":
    main()
