#!/bin/sh
# Host scenarios for the present-cost spike (isolated test instance only).
# Scenario (c): 8 panes in one tab, current Space, 4 chattering panes.
# Scenario (b): one pane running `yes`, with the #197 poll-drain fix so frames paint.
HERE=$(cd "$(dirname "$0")" && pwd)
RS="$HERE/../render-thread/run-scenario.sh"
YES="$HERE/../render-thread/yes-after-3.sh"
ATTACH="--attach-session 2 --attach-session 3 --attach-session 4 --attach-session 5 --attach-session 6 --attach-session 7 --attach-session 8 --attach-session 9"
VIEW="PMUX_VIEW_PATH=/private/tmp/pspk/view.json"
DIFF="PRISMATTYC_SPIKE_TILE_DIFF=1 PRISMATTYC_SPIKE_DAMAGE_STAGES=1 PRISMATTYC_SPIKE_DAMAGE_AUDIT=1"
run() { name=$1; shift; SPK_MORE="$1" "$RS" "$name" 200x60 8 20 $2 > /dev/null; echo "$name done"; }

# Damage attribution and the truly-changed oracle (tiles path).
run pc-c-oracle "$VIEW $DIFF" "$ATTACH"
# Present only truly changed tiles.
run pc-c-skip "$VIEW $DIFF PRISMATTYC_SPIKE_SKIP_UNCHANGED=1" "$ATTACH"
# Today's tiles vs IOSurface ring, both flushed so main-thread commit work is counted.
run pc-c-tiles-flush "$VIEW PRISMATTYC_SPIKE_FLUSH=1" "$ATTACH"
run pc-c-surface-flush "$VIEW PRISMATTYC_SPIKE_FLUSH=1 PRISMATTYC_SPIKE_PRESENT=iosurface" "$ATTACH"
# Same without flush (how the host runs today).
run pc-c-tiles "$VIEW" "$ATTACH"
run pc-c-surface "$VIEW PRISMATTYC_SPIKE_PRESENT=iosurface" "$ATTACH"
# Pixel identity against PRISMATTYC_DUMP_PRESENT.
run pc-c-surface-verify "$VIEW PRISMATTYC_SPIKE_PRESENT=iosurface PRISMATTYC_SPIKE_VERIFY=1 PRISMATTYC_DUMP_PRESENT=/private/tmp/pspk/out/dump/frame.png" "$ATTACH"
# Full-frame scrolling under yes.
run pc-b-tiles-flush "PRISMATTYC_SPIKE_POLL_DRAIN=1 PRISMATTYC_SPIKE_FLUSH=1" "-- $YES"
run pc-b-surface-flush "PRISMATTYC_SPIKE_POLL_DRAIN=1 PRISMATTYC_SPIKE_FLUSH=1 PRISMATTYC_SPIKE_PRESENT=iosurface" "-- $YES"
echo ALL DONE
