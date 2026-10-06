# Write a fixed 32 MiB of short lines to the terminal; record elapsed seconds.
import os, sys, time
time.sleep(3)
line = b"y" * 79 + b"\n"
chunk = line * 1024
total = 32 * 1024 * 1024
start = time.monotonic()
written = 0
while written < total:
    written += os.write(1, chunk)
elapsed = time.monotonic() - start
with open(sys.argv[1], "w") as out:
    out.write(f"bytes={written} seconds={elapsed:.2f} MiB_per_s={written / elapsed / 1048576:.2f}\n")
time.sleep(600)
