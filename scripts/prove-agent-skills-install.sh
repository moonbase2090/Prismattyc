#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/prismattyc-agent-skills-proof.XXXXXX")"
trap 'rm -rf -- "$tmp"' EXIT

if [[ -n "${PMUX_BIN:-}" ]]; then
  pmux_bin="$PMUX_BIN"
else
  (cd "$root" && cargo build --locked -p prismattyc-mux --bin pmux)
  pmux_bin="$root/target/debug/pmux"
fi

tools="$tmp/tools"
mkdir -p "$tools"
system_path="/usr/bin:/bin:/usr/sbin:/sbin"
proof_path="$tools:$system_path"
cat >"$tools/uname" <<'SH'
#!/bin/sh
if [ "${1:-}" = "-s" ]; then
  printf 'Linux\n'
else
  exec /usr/bin/uname "$@"
fi
SH
chmod +x "$tools/uname"

if ! PATH="$system_path" command -v sha256sum >/dev/null 2>&1; then
  cat >"$tools/sha256sum" <<'SH'
#!/usr/bin/env bash
if [[ "${1:-}" == --check ]]; then
  shift
  exec shasum -a 256 -c "$@"
fi
exec shasum -a 256 "$@"
SH
  chmod +x "$tools/sha256sum"
fi
if ! PATH="$system_path" command -v flock >/dev/null 2>&1; then
  cat >"$tools/flock" <<'SH'
#!/bin/sh
exit 0
SH
  chmod +x "$tools/flock"
fi

make_payload() {
  local payload="$1" name
  mkdir -p "$payload/bin" "$payload/share/man" "$payload/share/licenses"
  cp "$pmux_bin" "$payload/bin/pmux"
  for name in pmuxd pmux-attach pmux-mcp prismattyc prismattyc-host; do
    cat >"$payload/bin/$name" <<'SH'
#!/bin/sh
exit 0
SH
    chmod +x "$payload/bin/$name"
  done
  cp "$root/assets/brand/prismattyc-icon-tile.svg" "$payload/share/prismattyc.svg"
  cp "$root/docs/man/pmux-pane-write.1" "$payload/share/man/pmux.1"
  cp "$root/LICENSE" "$payload/share/licenses/MPL-2.0.txt"
  cp "$root/NOTICE.txt" "$payload/share/licenses/NOTICE.txt"
  cp "$root/scripts/release/install.sh" "$payload/install.sh"
  printf 'proof\n' >"$payload/VERSION"
  (
    cd "$payload"
    PATH="$proof_path" sha256sum bin/* share/prismattyc.svg share/man/* share/licenses/* install.sh VERSION >SHA256SUMS
  )
}

run_install() {
  local home="$1"
  shift
  HOME="$home" PATH="$proof_path" PRISMATTYC_NO_AGENT_SKILLS=0 \
    "$home/payload/install.sh" "$@"
}

home="$tmp/with-codex"
mkdir -p "$home/.codex" "$home/.claude" "$home/payload"
make_payload "$home/payload"
first="$(run_install "$home")"
printf '%s\n' "$first" | rg -F 'codex: installed'
printf '%s\n' "$first" | rg -F 'claude: installed'
cmp "$root/skills/pmux/SKILL.md" "$home/.codex/skills/pmux/SKILL.md"
cmp "$root/skills/pmux/SKILL.md" "$home/.claude/skills/pmux/SKILL.md"
for absent in .agents .cursor .kiro .muse; do
  [[ ! -e "$home/$absent" ]] || { printf 'unexpected agent directory: %s\n' "$absent" >&2; exit 1; }
done
[[ ! -e "$home/.config/muse" ]] || { echo 'unexpected Muse config directory' >&2; exit 1; }
before="$(stat -c %Y "$home/.codex/skills/pmux/SKILL.md" 2>/dev/null || stat -f %m "$home/.codex/skills/pmux/SKILL.md")"
sleep 1
second="$(run_install "$home")"
printf '%s\n' "$second" | rg -F 'codex: already current'
after="$(stat -c %Y "$home/.codex/skills/pmux/SKILL.md" 2>/dev/null || stat -f %m "$home/.codex/skills/pmux/SKILL.md")"
[[ "$before" == "$after" ]] || { echo 'a repeated install changed the skill file' >&2; exit 1; }

opt_out_home="$tmp/no-agent-skills"
mkdir -p "$opt_out_home/.codex" "$opt_out_home/payload"
make_payload "$opt_out_home/payload"
opt_out="$(run_install "$opt_out_home" --no-agent-skills)"
printf '%s\n' "$opt_out" | rg -F 'Skipped automatic pmux Agent Skill installation because'
[[ ! -e "$opt_out_home/.codex/skills" ]] || { echo '--no-agent-skills created an agent skill directory' >&2; exit 1; }

env_home="$tmp/env-opt-out"
mkdir -p "$env_home/.codex" "$env_home/payload"
make_payload "$env_home/payload"
env_opt_out="$(HOME="$env_home" PATH="$proof_path" PRISMATTYC_NO_AGENT_SKILLS=1 \
  "$env_home/payload/install.sh")"
printf '%s\n' "$env_opt_out" | rg -F 'Skipped automatic pmux Agent Skill installation because'
[[ ! -e "$env_home/.codex/skills" ]] || { echo 'environment opt-out created an agent skill directory' >&2; exit 1; }

config_home="$tmp/config-opt-out"
mkdir -p "$config_home/.codex" "$config_home/.config/prismattyc" "$config_home/payload"
printf 'install_agent_skills = false\n' >"$config_home/.config/prismattyc/config.toml"
make_payload "$config_home/payload"
config_opt_out="$(run_install "$config_home")"
printf '%s\n' "$config_opt_out" | rg -F 'because install_agent_skills = false'
[[ ! -e "$config_home/.codex/skills" ]] || { echo 'config opt-out created an agent skill directory' >&2; exit 1; }

printf 'Proof passed: only detected Codex and Claude agents were installed; rerun was a no-op; CLI, environment, and config opt-outs skipped skill creation.\n'
