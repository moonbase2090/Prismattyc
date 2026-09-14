#!/bin/bash
# Kitty graphics: transmit + display a PNG at the cursor (a=T, f=100), chunked.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMG="${1:-$DIR/prismattyc-256.png}"
COLS="${2:-24}"
ROWS="${3:-12}"
python3 - "$IMG" "$COLS" "$ROWS" <<'PY'
import base64, sys
data = base64.b64encode(open(sys.argv[1], "rb").read()).decode()
cols, rows = sys.argv[2], sys.argv[3]
chunks = [data[i:i+4096] for i in range(0, len(data), 4096)]
out = sys.stdout
for i, chunk in enumerate(chunks):
    more = 1 if i < len(chunks) - 1 else 0
    ctrl = f"a=T,f=100,t=d,i=7,c={cols},r={rows},q=2,m={more}" if i == 0 else f"m={more}"
    out.write(f"\x1b_G{ctrl};{chunk}\x1b\\")
out.write("\n" * (int(rows)))
out.write("  ^ a PNG, drawn inline with the kitty graphics protocol\n")
out.flush()
PY
