#!/bin/bash
K=$HOME/kit; R=$K/run; rm -rf $R; mkdir -p $R/home $R/config $R/data $R/state $R/project $R/rt
echo old > $R/mode
cat > $R/config.toml <<'E'
splash = false
start_at_login = false
automatic_update_checks = false
install_agent_skills = false
session_naming = "auto"
E
cat > $R/shell <<'E'
#!/bin/sh
R=$(dirname "$0")
c=0; [ -f $R/launch-count ] && c=$(cat $R/launch-count)
n=$((c+1)); echo $n > $R/launch-count
mode=$(cat $R/mode)
: > $R/input-$n.bin
echo "$n $mode" >> $R/launches.txt
stty raw -echo
if [ "$mode" = old ]; then
  printf 'OLD_SCREEN_MARKER\r\n\033[?1000;1002;1003;1004;1006;2004;7700h'
  printf 'CHILD_%s_OLD_REPORTING_ON\r\n' $n
  exec cat >> $R/input-$n.bin
fi
printf 'CHILD_%s_NEW_NOT_OPTED_IN\r\n' $n
dd bs=1 count=1 of=$R/input-$n.bin 2>/dev/null
printf '\033[?1003;1006h'
printf 'CHILD_%s_NEW_REPORTING_ON\r\n' $n
exec cat >> $R/input-$n.bin
E
chmod 700 $R/shell
cat > $R/server <<E
#!/bin/sh
exec $K/bins/pmuxd --restore-workspace "\$@"
E
chmod 700 $R/server
cat > $R/env.sh <<E
export HOME=$R/home SHELL=$R/shell PMUX_SERVER=$R/server PRISMATTYC_DEFAULT_SPACE=0 PATH=$K/bins:/usr/bin:/bin:/usr/sbin:/sbin PMUX_SOCKET=$R/rt/pmux.sock XDG_RUNTIME_DIR=$R/rt XDG_CONFIG_HOME=$R/config XDG_DATA_HOME=$R/data XDG_STATE_HOME=$R/state PRISMATTYC_CONFIG=$R/config.toml PRISMATTYC_NO_AGENT_SKILLS=1 PMUX_PANE_LOG=$R/pane-log.json
E
. $R/env.sh
cd $R/project
$R/server --socket $PMUX_SOCKET -- $R/shell > $R/daemon.log 2>&1 &
for i in $(seq 1 100); do [ -S $PMUX_SOCKET ] && break; sleep 0.1; done
exec prismattyc-host --no-splash -- $R/shell > $R/host.log 2>&1
