# Termwright — E2E TUI testing for Prismattyc

**Status:** Required for agent and CI-style E2E of nested host UX.
**Tool:** [fcoury/termwright](https://github.com/fcoury/termwright) v0.2.x (Playwright-like for terminal apps).
**Binary:** `~/.cargo/bin/termwright` (ensure `PATH` includes `$HOME/.cargo/bin`).

## What it is

Termwright wraps a child in a **PTY**, lets you **type / press keys**, **wait for text**, **assert screen content**, and capture **PNG screenshots** + JSON cell dumps. Screenshots are the operator-facing view of the nested TUI path.

## What it tests (and what it does not)

| Path | Termwright? | Notes |
|------|-------------|--------|
| Nested host `prismattyc` | **Yes — primary** | Real classic claim surface under PTY. `a6-nested-marker` is a §5.6.1 step-3 analog only |
| Windowed host `prismattyc-host` | **No (not PTY)** | OS window via winit; A-6 mux steps 1–9 need human/pilot dogfood + `scripts/test-phase2*.sh`; nested covers shared VT |
| Unit / nested_pty_ux | Still required | Faster, deterministic; Termwright is the E2E layer on top |

**Rule for agents:** After any change that affects host UX, classic paint, keys, selection, or scrollback, run the Termwright E2E suite and **read the PNGs** (not only text asserts).

## Install

```bash
cargo install termwright --locked --version 0.2.0
export PATH="$HOME/.cargo/bin:$PATH"
termwright --version   # expect 0.2.0+
```

## Quick commands

```bash
# Smoke nested prismattyc
./scripts/termwright-e2e.sh

# Single shot capture
termwright run --cols 80 --rows 24 --wait-for "hello" --format text -- \
  ./target/debug/prismattyc /bin/sh -c 'printf hello; sleep 2'

# Screenshot for human review
termwright screenshot --wait-for "hello" -o /tmp/prism.png -- \
  ./target/debug/prismattyc /bin/sh -c 'printf hello; sleep 2'

# Steps file (preferred)
termwright run-steps --trace e2e/classic-shell.yaml
```

Artifacts land under `e2e/artifacts/<timestamp>/` (gitignored): `*.png`, `step-*-screen.txt`, `step-*-screen.json`.

## Step file shape

```yaml
session:
  command: /absolute/path/to/prismattyc   # wrapper injects this
  args: ["/bin/sh"]
  cols: 80
  rows: 24
  env:
    TERM: xterm-256color

steps:
  - waitForIdle: {idleMs: 400, timeoutMs: 10000}
  - screenshot: {name: "01-ready"}
  - type: {text: "echo hello world"}
  - press: {key: Enter}
  - waitForText: {text: "hello world", timeoutMs: 10000}
  - expectText: {text: "hello world"}
  - screenshot: {name: "02-echo"}

artifacts:
  mode: always   # or onFailure
  dir: ./e2e/artifacts
```

Discover steps: `termwright info steps` · keys: `termwright info keys` · protocols: `termwright info protocols`.

## Daemon mode (interactive debugging)

```bash
SOCK=$(termwright daemon --background --cols 80 --rows 24 -- ./target/debug/prismattyc /bin/sh)
termwright exec --socket "$SOCK" --method wait_for_text --params '{"text":"$","timeout_ms":10000}'
termwright exec --socket "$SOCK" --method type --params '{"text":"ls"}'
termwright exec --socket "$SOCK" --method press --params '{"key":"Enter"}'
termwright exec --socket "$SOCK" --method screen --params '{"format":"text"}'
termwright exec --socket "$SOCK" --method screenshot | jq -r '.result.png_base64' | base64 -d > shot.png
termwright exec --socket "$SOCK" --method close
```
