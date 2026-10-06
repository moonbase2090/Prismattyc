#!/bin/sh
# Isolated Prismattyc test instance: never the live pmuxd or user state.
R=$(cd "$(dirname "$0")/../.." && pwd)/target/release
P=/private/tmp/pspk
exec env -i \
  HOME=$P/home XDG_CONFIG_HOME=$P/home/.config XDG_DATA_HOME=$P/home/.local/share \
  XDG_STATE_HOME=$P/home/.local/state XDG_CACHE_HOME=$P/home/.cache XDG_RUNTIME_DIR=$P/run \
  PMUX_SOCKET=$P/run/prismattyc/pmux.sock \
  PATH=$R:/usr/bin:/bin:/usr/sbin:/sbin SHELL=/bin/zsh TERM=xterm-256color LANG=en_US.UTF-8 \
  USER=$USER LOGNAME=$USER TMPDIR=$P/tmp/ \
  PRISMATTYC_NO_SPLASH=1 PRISMATTYC_NO_AGENT_SKILLS=1 \
  ${SPK_EXTRA_ENV} "$@"
