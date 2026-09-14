#!/usr/bin/env bash
# Install prismattyc-host binary, PATH symlink, FreeDesktop icons, and .desktop entry.
#
# Plasma/KRunner resolve Exec with a minimal PATH that does **not** include
# ~/.cargo/bin. We therefore:
#   1) cargo install → $CARGO_HOME/bin/prismattyc-host
#   2) symlink → ~/.local/bin/prismattyc-host (usually on login PATH)
#   3) write .desktop with absolute Exec=/…/.local/bin/prismattyc-host
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_THEME="$PREFIX/icons/hicolor"
APPS="$PREFIX/applications"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
LOCAL_BIN="${HOME}/.local/bin"
WRAPPER="$LOCAL_BIN/prismattyc-host"

install_icons() {
  # Full-bleed dark squircle. The transparent mark (prismattyc-icon.svg) sits
  # on the DE's light chrome; GNOME/KDE prefer the scalable SVG over PNG, so
  # the SVG must include the #121214 body.
  local size
  for size in 16 24 32 48 64 128 256 512; do
    local src="$ROOT/assets/brand/png/prismattyc-tile-${size}.png"
    local dest_dir="$ICON_THEME/${size}x${size}/apps"
    if [[ -f "$src" ]]; then
      mkdir -p "$dest_dir"
      install -m 644 "$src" "$dest_dir/prismattyc.png"
      echo "icon ${size}x${size} → $dest_dir/prismattyc.png"
    fi
  done
  if [[ -f "$ROOT/assets/brand/prismattyc-icon-tile.svg" ]]; then
    mkdir -p "$ICON_THEME/scalable/apps"
    install -m 644 "$ROOT/assets/brand/prismattyc-icon-tile.svg" "$ICON_THEME/scalable/apps/prismattyc.svg"
    echo "icon scalable → $ICON_THEME/scalable/apps/prismattyc.svg"
  fi
  # Pre-rename names so DEs do not keep the old mark next to the new one.
  local stale
  for size in 16 24 32 48 64 128 256 512; do
    stale="$ICON_THEME/${size}x${size}/apps/prism-host.png"
    if [[ -e "$stale" ]]; then
      rm -f "$stale"
      echo "removed stale $stale"
    fi
  done
  stale="$ICON_THEME/scalable/apps/prism-host.svg"
  if [[ -e "$stale" ]]; then
    rm -f "$stale"
    echo "removed stale $stale"
  fi
  stale="$APPS/prism-host.desktop"
  if [[ -e "$stale" ]]; then
    rm -f "$stale"
    echo "removed stale $stale"
  fi
}

install_binary() {
  if [[ "${SKIP_CARGO_INSTALL:-}" == "1" ]]; then
    echo "skip cargo install (SKIP_CARGO_INSTALL=1)"
  else
    echo "cargo install --path crates/prismattyc-host --force --locked"
    (cd "$ROOT" && cargo install --path crates/prismattyc-host --force --locked)
  fi
  if [[ ! -x "$CARGO_BIN/prismattyc-host" ]]; then
    echo "error: missing $CARGO_BIN/prismattyc-host — run without SKIP_CARGO_INSTALL" >&2
    exit 1
  fi
  mkdir -p "$LOCAL_BIN"
  ln -sfn "$CARGO_BIN/prismattyc-host" "$WRAPPER"
  echo "binary → $CARGO_BIN/prismattyc-host"
  echo "PATH link → $WRAPPER"
}

install_desktop() {
  mkdir -p "$APPS"
  local template="$ROOT/assets/brand/prismattyc-host.desktop.in"
  local dest="$APPS/prismattyc-host.desktop"
  if [[ -f "$template" ]]; then
    sed -e "s|@EXEC@|${WRAPPER}|g" -e "s|@TRYEXEC@|${WRAPPER}|g" \
      "$template" >"$dest"
  else
    # Fallback if .in is missing
    cat >"$dest" <<EOF
[Desktop Entry]
Type=Application
Name=Prismattyc
GenericName=Terminal
Comment=Classic terminal. Modern surface.
Exec=${WRAPPER}
TryExec=${WRAPPER}
Icon=prismattyc
Terminal=false
Categories=System;TerminalEmulator;
StartupNotify=true
StartupWMClass=prismattyc-host
Keywords=terminal;shell;console;pty;mux;
EOF
  fi
  chmod 644 "$dest"
  echo "desktop → $dest"
  if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$dest" || true
  fi
}

refresh_caches() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS" 2>/dev/null || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$ICON_THEME" 2>/dev/null || true
  fi
  # Nudge KDE / freedesktop caches when available
  if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 --noincremental 2>/dev/null || true
  elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 --noincremental 2>/dev/null || true
  fi
}

main() {
  install_icons
  install_binary
  install_desktop
  refresh_caches
  echo
  echo "Done."
  echo "  CLI:  $WRAPPER"
  echo "  Menu: Prismattyc (System / Terminal)"
  echo "If the menu still fails, log out/in or run: kbuildsycoca6"
}

main "$@"
