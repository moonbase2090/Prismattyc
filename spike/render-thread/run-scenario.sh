#!/bin/sh
# usage: run-scenario.sh NAME CELLS WARMUP_S MEASURE_S [-- host args...]
# Starts an isolated host, drops warm-up samples, measures, stops only that host.
S=$(dirname "$0"); NAME=$1; CELLS=$2; WARM=$3; MEAS=$4; shift 4
OUT=/private/tmp/pspk/out/$NAME.txt
SPK_EXTRA_ENV="PRISMATTYC_SPIKE_TIMING=$OUT PRISMATTYC_SPIKE_CELLS=$CELLS $SPK_MORE" \
  "$S/spk-env.sh" prismattyc-host "$@" > /private/tmp/pspk/out/$NAME.log 2>&1 &
HOST=$!
echo "host pid $HOST"
sleep "$WARM"
touch "${OUT%.txt}.reset"
[ -n "$SPK_DURING" ] && sh -c "$SPK_DURING" &
sleep "$MEAS"
sleep 3
cp "$OUT" "/private/tmp/pspk/out/$NAME.final.txt"
kill "$HOST"; wait "$HOST" 2>/dev/null
cat "/private/tmp/pspk/out/$NAME.final.txt"
