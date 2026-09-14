#!/usr/bin/env bash
# Linux cargo tests own private Xvfb displays, including coverage and mutants.
set -euo pipefail
# Xvfb supplies the server. winit loads these client libraries at runtime.
packages=(xvfb libx11-6 libx11-xcb1 libxcursor1 libxi6 libxrandr2
          libxkbcommon-x11-0 libxcb1 libxcb-shm0)
if dpkg-query -W -f='${Status}\n' "${packages[@]}" 2>/dev/null \
    | awk '$0 != "install ok installed" { bad = 1 } END { exit bad }'; then
  exit 0
fi
privilege=()
if [ "$(id -u)" -ne 0 ]; then
  privilege=(sudo)
fi
"${privilege[@]}" apt-get update -qq
"${privilege[@]}" apt-get install -y -qq "${packages[@]}"
command -v Xvfb >/dev/null
