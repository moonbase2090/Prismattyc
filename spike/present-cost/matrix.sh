#!/bin/sh
# Present-cost matrix: every backend x damage x thread at five size/scale points.
P=$(cd "$(dirname "$0")/../.." && pwd)/target/release/examples/present_cost_probe
OUT=/private/tmp/pspk/present/matrix.txt
: > "$OUT"
for size in "1600x1000 1" "3528x1764 1" "1600x1000 2" "3528x1764 2" "5120x2880 2"; do
  set -- $size
  for backend in tiles ring inplace metal; do
    for damage in full band cell; do
      for thread in main bg; do
        "$P" "$backend" "$1" "$2" "$damage" "$thread" 150 >> "$OUT" 2>/dev/null || echo "FAILED $backend $1 $2 $damage $thread" >> "$OUT"
      done
    done
  done
done
echo DONE >> "$OUT"
