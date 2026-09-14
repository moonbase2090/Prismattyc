#!/usr/bin/env python3
"""Audit equivalent work, then run five alternating measurement blocks.

Pass the directory containing VARIANT-bench-audit and VARIANT-bench-final,
then optional variant names (default: baseline candidate). Run inside one
container pinned to one CPU. Audit timings are excluded from final samples.
The driver writes final-state files beside the binaries.
"""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

root = Path(sys.argv[1]).resolve()
variants = sys.argv[2:] or ["baseline", "candidate"]
assert len(variants) >= 2 and len(set(variants)) == len(variants)
modes = ["scroll-ascii", "flood-ascii", "flood-unicode", "replica-ascii", "replica-unicode"]


def invoke(variant, mode, kind, block=None):
    path = root / f"{variant}-{mode}-{kind}.state"
    output = subprocess.check_output(
        [str(root / f"{variant}-bench-{kind}"), mode, "200000", str(path)], text=True
    )
    state = re.sub(r"max_scrollback_bytes: \d+, ", "", path.read_text())
    data = dict(item.split("=", 1) for item in output.splitlines()[0].split())
    data.update(
        variant=variant,
        mode=mode,
        kind=kind,
        block=block,
        state_sha256=hashlib.sha256(state.encode()).hexdigest(),
        state_bytes=len(state),
    )
    if kind == "audit":
        data["counts"] = json.loads(re.search(r"audit=(\[[^\n]+\])", output)[1])
    data["final"] = dict(item.split("=", 1) for item in output.splitlines()[-1].split())
    print(json.dumps(data), flush=True)
    return data


def require_equal(results, keys):
    for result in results[1:]:
        for key in keys:
            assert result[key] == results[0][key], (key, results)


for mode in modes:
    results = [invoke(variant, mode, "audit") for variant in variants]
    require_equal(results, ["counts", "state_sha256", "final"])

for block in range(5):
    offset = block % len(variants)
    order = variants[offset:] + variants[:offset]
    if block % 2:
        order.reverse()
    for mode in modes:
        results = [invoke(variant, mode, "final", block) for variant in order]
        require_equal(results, ["state_sha256", "final"])
