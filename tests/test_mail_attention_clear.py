#!/usr/bin/env python3
"""The drained precondition is enforced, not merely documented (PR #168 review).

A Clear while Hive still holds mail plants a tombstone that the campaign's own
Set then falls under, leaving the letter dark with mail waiting. These cases pin
the guard and, crucially, prove it can REFUSE -- the mux write must not be
reached when the mailbox is not drained.

Run: python3 tests/test_mail_attention_clear.py
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location(
    "mac", ROOT / "scripts" / "mail-attention-clear.py"
)
mac = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mac)

STORED = {"cell": "cell:0000000000000000000000000000000a@1", "queue_rev": 7}


class FakeMux:
    """Records whether a mail_attention_clear ever reached the wire."""

    def __init__(self) -> None:
        self.cleared: dict | None = None

    def request(self, obj: dict) -> dict:
        kind = obj["type"]
        if kind == "register_client":
            return {"response": {"client_id": 1}}
        if kind == "mail_attention_clear":
            self.cleared = obj
            return {"status": "ok", "response": {"attention": None}}
        raise AssertionError(f"unexpected mux call: {kind}")

    def close(self) -> None:
        pass


def run(peek: dict, stored: dict | None = STORED) -> tuple[int, FakeMux]:
    mux = FakeMux()
    mac.mas.Mux = lambda _path: mux
    mac.mas.pane_for_bound_pid = lambda *_a, **_k: 4
    mac.stored_mail = lambda *_a, **_k: stored
    mac.mas.hive_peek = lambda _cell: peek
    argv = sys.argv
    sys.argv = ["mail-attention-clear.py", "4242"]
    try:
        return mac.main(), mux
    finally:
        sys.argv = argv


def check(name: str, got: int, want: int, mux: FakeMux, want_clear: bool) -> bool:
    ok = got == want and (mux.cleared is not None) == want_clear
    print(f"{'PASS' if ok else 'FAIL'}  {name}: exit={got} (want {want}), "
          f"cleared={mux.cleared is not None} (want {want_clear})")
    return ok


def main() -> int:
    results = []

    # Refuses while mail is still queued -- the blocker this guard exists for.
    code, mux = run({"result": {"found": True, "depth": 2, "queue_rev": 9}})
    results.append(check("depth>0 refuses", code, 3, mux, False))

    # Null depth is UNKNOWN, never "zero".
    code, mux = run({"result": {"found": True, "depth": None}})
    results.append(check("null depth refuses", code, 3, mux, False))

    # Drained: found=false carries no depth, and is the normal stale-letter case.
    code, mux = run({"result": {"found": False}})
    results.append(check("found=false clears", code, 0, mux, True))

    # Drained explicitly.
    code, mux = run({"result": {"found": True, "depth": 0}})
    results.append(check("depth==0 clears", code, 0, mux, True))

    # A peek error must not fall through to a Clear.
    code, mux = run({"error": {"code": -32104, "message": "denied"}})
    results.append(check("peek error refuses", code, 1, mux, False))

    # Nothing stored: exit 2 before any peek or write.
    code, mux = run({"result": {"found": False}}, stored=None)
    results.append(check("no stored attention", code, 2, mux, False))

    # The rev actually written is stored_rev + 1.
    code, mux = run({"result": {"found": False}})
    rev_ok = mux.cleared is not None and mux.cleared["queue_rev"] == STORED["queue_rev"] + 1
    print(f"{'PASS' if rev_ok else 'FAIL'}  clear_rev is stored_rev+1: "
          f"{mux.cleared and mux.cleared.get('queue_rev')} (want {STORED['queue_rev'] + 1})")
    results.append(rev_ok)

    failed = results.count(False)
    print(f"\n{len(results) - failed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
