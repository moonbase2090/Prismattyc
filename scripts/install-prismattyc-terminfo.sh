#!/bin/sh
# Explicitly install the portable database locally or on one SSH destination.
set -eu
base=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
source_file=
for candidate in "$base/../Resources/terminfo/portable.src" "$base/../share/terminfo/portable.src" "$base/../terminfo/portable.src"; do
    if [ -f "$candidate" ]; then source_file=$candidate; break; fi
done
if [ -z "$source_file" ]; then
    echo 'Prismattyc terminal database is missing from this installation.' >&2
    exit 1
fi
case "$#:${1-}" in
    0:)
        command -v tic >/dev/null 2>&1 || { echo 'Install ncurses tic first.' >&2; exit 1; }
        umask 077
        mkdir -p "$HOME/.terminfo"
        tic -x -o "$HOME/.terminfo" "$source_file"
        ;;
    2:--ssh)
        case "$2" in ''|-*) echo 'Use an SSH host alias or user@host destination.' >&2; exit 2;; esac
        # Host options belong in ~/.ssh/config. The destination is never shell code.
        ssh -- "$2" 'command -v tic >/dev/null 2>&1 || { echo "Install ncurses tic, or connect with TERM=xterm-256color ssh." >&2; exit 1; }; umask 077; mkdir -p "$HOME/.terminfo" && tic -x -o "$HOME/.terminfo" -' < "$source_file"
        ;;
    *) echo 'Usage: install-prismattyc-terminfo.sh [--ssh user@host]' >&2; exit 2;;
esac
