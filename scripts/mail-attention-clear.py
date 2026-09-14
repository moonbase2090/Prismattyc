#!/usr/bin/env python3
"""Operator-class MailAttentionClear for a pane whose Hive mailbox is drained.

Counterpart to mail-attention-set.py, which states "Clear is not this tool".

The documented flow is claim+commit -> depth 0 -> mux polls attention.peek ->
MailAttentionClear (docs/agents.md). That auto-clear does not always fire: a
pane can hold stored attention while Hive reports the mailbox empty. The letter
stays lit, `hive inbox <own-cell>` returns total 0, and `attention.peek` returns
found=false. Clearing is then the recipient's own move.

THE WATERMARK RULE (ADR-0008). Each pane keeps a queue_rev watermark that
survives Clear (a tombstone). Any Set or Clear at a rev <= the watermark is a
no-op -- first write of a rev wins. So a Clear must carry a rev strictly greater
than the stored one. This uses stored_rev + 1: the minimum that applies, and the
same rule the mux's own drain path uses (clear_mail_after_hive_drain ->
state.queue_rev.saturating_add(1)).

DRAINED IS A PRECONDITION, NOT A HINT (PR #168 review). A Clear while
the mailbox still has depth > 0 plants a tombstone that the campaign's own Set
then falls under (rev <= watermark), so the letter stays DARK while mail is
still waiting. This script therefore peeks Hive on the cell the pane stores and
refuses unless the mailbox is drained. Once drained, advancing the watermark is
safe: the next real Set carries a queue_rev from a NEW enqueue, necessarily
greater than the tombstone.

Do not try to source the rev from Hive. At depth 0 attention.peek returns
found=false and carries no queue_rev, and cells.list exposes unread but not
queue_rev. Read the stored rev off the mux snapshot's per-pane `mail` object.

Takes YOUR PID, not a cell address: the pane is found by walking bound_pid
ancestors until a mux ReadPane.child_pid matches -- the same join Set uses.

Run under the same operator escalation as Set (systemd-run --user opens a new
session, which escapes the ADR-0038 adopt fence; scrub all three cell vars,
because a spawned seat wins on HIVE_CELL_TOKEN):

  UID_RT="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
  systemd-run --user --wait --pipe --collect \\
    --setenv=XDG_RUNTIME_DIR="$UID_RT" \\
    --setenv=HIVE_SOCKET="$UID_RT/hive/control.sock" \\
    --setenv=HIVE_CELL= --setenv=HIVE_CELL_SOCKET= --setenv=HIVE_CELL_TOKEN= \\
    -- scripts/mail-attention-clear.py <your-agent-pid>

Exit: 0 cleared - 1 mux refused - 2 pane had no stored attention -
      3 refused: Hive still has mail, or depth is UNKNOWN.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
from pathlib import Path

# Reuse mail-attention-set.py as a library so socket paths, line framing, and
# the ancestor walk live in exactly one place and cannot drift apart. Resolved
# next to this file rather than by absolute path, so a checkout anywhere works.
_SET = Path(__file__).resolve().parent / "mail-attention-set.py"
_spec = importlib.util.spec_from_file_location("mail_attention_set", _SET)
if _spec is None or _spec.loader is None:  # pragma: no cover
    raise SystemExit(f"cannot load sibling helper: {_SET}")
mas = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(mas)


def stored_mail(mux: "mas.Mux", pane_id: int) -> dict | None:
    snap = mux.request({"type": "snapshot", "version": mas.MUX_PROTO})
    for sess in snap["response"]["snapshot"]["sessions"]:
        for win in sess.get("windows", []):
            for pane in win.get("panes", []):
                if pane["id"] == pane_id:
                    return pane.get("mail")
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "pid",
        type=int,
        help="your agent process pid (the pane is found by ancestor walk)",
    )
    args = parser.parse_args()

    mux = mas.Mux(mas.mux_sock())
    try:
        reg = mux.request({"type": "register_client", "version": mas.MUX_PROTO})
        client_id = reg["response"]["client_id"]
        pane_id = mas.pane_for_bound_pid(mux, client_id, args.pid)

        stored = stored_mail(mux, pane_id)
        if not stored:
            print(
                f"pane {pane_id} has no stored attention; nothing to clear",
                file=sys.stderr,
            )
            return 2

        # Enforce the drained precondition. Clearing while mail is still queued
        # plants a tombstone the campaign's own Set then falls under, leaving
        # the letter dark with mail waiting. Peek the cell the PANE stores; the
        # caller supplies only a pid, so there is no cell argument to trust.
        cell = str(stored["cell"])
        peek = mas.hive_peek(cell)
        if "error" in peek:
            print(json.dumps(peek, sort_keys=True), file=sys.stderr)
            err = peek.get("error") or {}
            if isinstance(err, dict) and err.get("code") == -32104:
                print(
                    "cell principal cannot attention.peek; rerun under "
                    "systemd-run --user --setenv=HIVE_CELL=",
                    file=sys.stderr,
                )
            return 1
        result = peek.get("result") or {}
        if result.get("found") is not False:
            depth = result.get("depth")
            if depth is None:
                print(
                    f"{cell}: attention.peek depth is null (UNKNOWN); "
                    "refusing to Clear",
                    file=sys.stderr,
                )
                return 3
            if int(depth) > 0:
                print(
                    f"{cell}: Hive still has depth={depth}; refusing to Clear. "
                    "Drain the mailbox (claim + commit) first.",
                    file=sys.stderr,
                )
                return 3

        rev = int(stored["queue_rev"]) + 1
        reply = mux.request(
            {
                "type": "mail_attention_clear",
                "version": mas.MUX_PROTO,
                "client_id": client_id,
                "pane_id": pane_id,
                "queue_rev": rev,
            }
        )
    finally:
        mux.close()

    print(
        json.dumps(
            {
                "pane_id": pane_id,
                "stored_rev": stored["queue_rev"],
                "clear_rev": rev,
                "reply": reply,
            },
            sort_keys=True,
        )
    )
    return 0 if reply.get("status") == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
