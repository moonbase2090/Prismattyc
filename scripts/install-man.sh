#!/usr/bin/env bash
# Generate and install the Prismattyc command man pages. Cross-platform (macOS + Linux).
#
# Best-effort: if help2man is missing we warn and exit 0 so `prismattyc update`
# still succeeds — the man page is a convenience, not a hard requirement.
#
# Generate command pages from each installed binary. Supplement pmux with
# the intentional pane-write contract and examples in docs/man/.
set -euo pipefail

if ! command -v help2man >/dev/null 2>&1; then
  echo "install-man: help2man not found; skipping man page." >&2
  echo "install-man: install it (brew install help2man / apt install help2man) and rerun." >&2
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
MAN_TMP="$(mktemp -d)"
trap 'rm -rf "$MAN_TMP"' EXIT

# Pick a man1 install dir from a stable, cross-platform preference list. We do
# NOT scan the full manpath: it can surface transient, tool-managed dirs (e.g. a
# version-specific nvm share/man) that vanish on the next upgrade. The Homebrew
# and /usr/local dirs are already on the default manpath; the XDG fallback may
# need a MANPATH hint (printed below).
is_writable_dir() {
  # Writable if the dir exists and is writable, or its nearest existing
  # ancestor is writable (so we can mkdir it).
  local d="$1"
  while [[ -n "$d" && "$d" != "/" ]]; do
    if [[ -e "$d" ]]; then
      [[ -w "$d" ]] && return 0 || return 1
    fi
    d="$(dirname "$d")"
  done
  return 1
}

pick_dir() {
  local candidate
  for candidate in \
    /opt/homebrew/share/man \
    /usr/local/share/man \
    "$HOME/.local/share/man"; do
    if is_writable_dir "$candidate"; then
      echo "$candidate/man1"
      return 0
    fi
  done
  echo "$HOME/.local/share/man/man1"
}

DEST_DIR="${PMUX_MAN_DIR:-$(pick_dir)}"
mkdir -p "$DEST_DIR"
for name in prismattyc prismattyc-host pmux pmuxd pmux-attach pmux-mcp; do
  if [[ -n "${PRISMATTYC_BINS:-}" ]]; then
    binary="$PRISMATTYC_BINS/$name"
  else
    binary="$(command -v "$name" 2>/dev/null || true)"
  fi
  if [[ ! -x "$binary" ]]; then
    echo "install-man: missing $name; skipping." >&2
    continue
  fi
  extra=()
  if [[ "$name" == pmux ]]; then
    extra+=(--include "$REPO_DIR/docs/man/pmux.inc")
  fi
  if ! help2man --no-info --no-discard-stderr --name "Prismattyc $name command interface" \
    ${extra[@]+"${extra[@]}"} --output "$MAN_TMP/$name.1" "$binary"; then
    echo "install-man: could not render $name; skipping." >&2
    continue
  fi
  install -m 644 "$MAN_TMP/$name.1" "$DEST_DIR/$name.1"
  echo "install-man: installed $DEST_DIR/$name.1"
done
install -m 644 "$REPO_DIR/docs/man/pmux-pane-write.1" "$DEST_DIR/pmux-pane-write.1"
echo "install-man: installed $DEST_DIR/pmux-pane-write.1"

# If the destination is the XDG fallback, it may not be on the default
# manpath; hint the user how to make `man prismattyc` resolve it.
case "$DEST_DIR" in
  "$HOME/.local/share/man/man1")
    if ! manpath 2>/dev/null | tr ':' '\n' | grep -qx "$HOME/.local/share/man"; then
      echo "install-man: add to your shell profile so 'man prismattyc' works:" >&2
      echo "  export MANPATH=\"\$HOME/.local/share/man:\$(manpath 2>/dev/null)\"" >&2
    fi
    ;;
esac
