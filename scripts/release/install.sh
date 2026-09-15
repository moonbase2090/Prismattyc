#!/usr/bin/env bash
# Install a verified Linux archive without stopping running sessions.
set -euo pipefail
payload="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
prefix="${HOME}/.local"
if [[ "${1:-}" == --prefix && $# == 2 ]]; then
  prefix="$2"
elif [[ $# != 0 ]]; then
  echo "Usage: ./install.sh [--prefix /absolute/path]" >&2
  exit 2
fi
case "$prefix" in /*) ;; *) echo 'The prefix must be an absolute path.' >&2; exit 2;; esac
[[ "$(uname -s)" == Linux ]] || { echo 'This archive is for Linux.' >&2; exit 1; }
cd "$payload"
sha256sum --check SHA256SUMS
version="$(cat VERSION)"
mkdir -p "$prefix"
exec 9>"$prefix/.prismattyc-install.lock"
flock -n 9 || { echo 'Another Prismattyc installation is running.' >&2; exit 1; }
for name in pmux pmuxd pmux-attach pmux-mcp prismattyc prismattyc-host; do
  ./bin/"$name" --version >/dev/null
  if [[ -e "$prefix/bin/$name" || -L "$prefix/bin/$name" ]]; then
    [[ -f "$prefix/bin/$name" && ! -L "$prefix/bin/$name" ]] &&
      cmp --silent "bin/$name" "$prefix/bin/$name" || {
      echo "An existing $name is installed in $prefix/bin. Use pmux update for that installation." >&2
      exit 1
    }
  fi
done
mkdir -p "$prefix/bin"
stage="$(mktemp -d "$prefix/bin/.prismattyc-install.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT
install -m 755 bin/* "$stage/"
# Hard links publish complete files without replacing a concurrent writer.
# Real executables in bin also let pmux update find its installation directory.
for name in pmux pmuxd pmux-attach pmux-mcp prismattyc prismattyc-host; do
  if [[ ! -e "$prefix/bin/$name" ]]; then
    ln "$stage/$name" "$prefix/bin/$name"
  fi
done
mkdir -p "$prefix/share/applications" "$prefix/share/icons/hicolor/scalable/apps" "$prefix/share/man/man1"
install -m 644 share/prismattyc.svg "$prefix/share/icons/hicolor/scalable/apps/prismattyc.svg"
install -m 644 share/man/*.1 "$prefix/share/man/man1/"
mkdir -p "$prefix/share/licenses/prismattyc"
install -m 644 share/licenses/*.txt "$prefix/share/licenses/prismattyc/"
# Quote Exec according to the Desktop Entry specification.
exec_path="${prefix//\\/\\\\}/bin/prismattyc-host"
exec_path="${exec_path//\"/\\\"}"
exec_path="${exec_path//\$/\\\$}"
exec_path="${exec_path//\`/\\\`}"
exec_path="${exec_path//%/%%}"
cat > "$prefix/share/applications/prismattyc-host.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Prismattyc
Comment=Classic terminal. Modern surface.
Exec="$exec_path"
Icon=prismattyc
Terminal=false
Categories=System;TerminalEmulator;
StartupWMClass=prismattyc-host
EOF
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
fi
printf 'Installed Prismattyc %s. Launch %s/bin/prismattyc-host or use your application menu.\n' "$version" "$prefix"
case ":$PATH:" in *":$prefix/bin:"*) ;; *) printf 'Add %s/bin to your PATH to use pmux in new shells.\n' "$prefix";; esac
