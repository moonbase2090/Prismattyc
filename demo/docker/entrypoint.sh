#!/bin/bash
# Xvfb + Openbox + a fresh pmux with two agent seats for the generic demo user.
set -euo pipefail
export DISPLAY="${DISPLAY:-:99}"
export PRISMATTYC_DEMO_GRAB="${PRISMATTYC_DEMO_GRAB:-display}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-demo}"
export HOME="${HOME:-/home/demo}"
export PATH="/usr/local/bin:$HOME/.local/bin:/usr/bin:$PATH"
mkdir -p "$XDG_RUNTIME_DIR" "$HOME/Desktop" "$HOME/.config/prismattyc" "$HOME/.claude"
chmod 700 "$XDG_RUNTIME_DIR"

# Agent credentials, mounted read-write copies at $HOME/creds (never the
# operator's live files). Seed them into the places the CLIs read.
CREDS="$HOME/creds"
if [[ -d "$CREDS" ]]; then
  # Claude's private config directory is mounted by run.sh. Keep refreshes there.
  [[ -f "$CREDS/claude.json" ]] && cp "$CREDS/claude.json" "$HOME/.claude.json"
  [[ -d "$CREDS/kiro-cli" ]] && { mkdir -p "$HOME/.local/share"; rm -rf "$HOME/.local/share/kiro-cli"; cp -r "$CREDS/kiro-cli" "$HOME/.local/share/kiro-cli"; }
  [[ -d "$CREDS/aws" ]] && { rm -rf "$HOME/.aws"; cp -r "$CREDS/aws" "$HOME/.aws"; }
fi

# Claude: trust the demo directories (no dialog on first run), register the
# pmux MCP server, and pre-allow its tools so the letter beat runs unprompted.
python3 - <<'PY'
import json, os, pathlib
p = pathlib.Path(os.environ["HOME"]) / ".claude.json"
d = json.loads(p.read_text()) if p.exists() else {}
d.setdefault("hasCompletedOnboarding", True)
projects = d.setdefault("projects", {})
for path in ("/home/demo", "/home/demo/demo", "/home/demo/work"):
    proj = projects.setdefault(path, {})
    proj["hasTrustDialogAccepted"] = True
    proj["hasClaudeMdExternalIncludesApproved"] = True
    proj.setdefault("allowedTools", [])
    if "mcp__pmux__*" not in proj["allowedTools"]:
        proj["allowedTools"].append("mcp__pmux__*")
d["mcpServers"] = {"pmux": {"type": "stdio", "command": "pmux-mcp", "args": ["--as", "claude"], "env": {}}}
for proj in projects.values():
    proj["mcpServers"] = {}
p.write_text(json.dumps(d))
PY
mkdir -p "$HOME/.claude"
cat > "$HOME/.claude/settings.json" <<'JSON'
{ "permissions": { "allow": ["mcp__pmux__*", "Bash(pmux *)"] }, "theme": "dark" }
JSON

if [[ ! -S /tmp/.X11-unix/X${DISPLAY#:} ]]; then
  Xvfb "$DISPLAY" -screen 0 1920x1080x24 -ac +extension GLX +render -noreset \
    >/tmp/xvfb.log 2>&1 &
  for _ in $(seq 1 50); do
    xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 && break
    sleep 0.1
  done
fi
xsetroot -solid "#121214"
openbox >/tmp/openbox.log 2>&1 &
sleep 0.3

pmux up
sleep 0.4
# Two agent seats (session name = agent id) plus a plain work session.
mkdir -p "$HOME/work"
( cd "$HOME/work" && pmux new claude --no-attach -- bash -l >/dev/null 2>&1 || true )
( cd "$HOME/work" && pmux new kiro   --no-attach -- bash -l >/dev/null 2>&1 || true )
( cd "$HOME/work" && pmux new work   --no-attach --no-agent -- bash -l >/dev/null 2>&1 || true )

cd "$HOME/demo"
# Freeze the driver so editing the mounted source cannot corrupt a running take.
if [[ "${1:-}" == "./record-demo.sh" ]]; then
  cp ./record-demo.sh /tmp/prismattyc-record-demo.sh
  shift
  export PRISMATTYC_DEMO_DIR="$HOME/demo"
  exec bash /tmp/prismattyc-record-demo.sh "$@"
fi
exec "$@"
