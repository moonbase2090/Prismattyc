#!/usr/bin/env python3
"""Operator-class MailAttentionSet after hive_send leaves depth > 0.

A cell principal cannot Hive peek (-32104). Mux Set does not need a
Hive cell token. Run under:

  systemd-run --user --wait --pipe --collect \\
    --setenv=XDG_RUNTIME_DIR=/run/user/$(id -u) \\
    --setenv=HIVE_SOCKET=/run/user/$(id -u)/hive/control.sock \\
    --setenv=HIVE_CELL= --setenv=HIVE_CELL_SOCKET= --setenv=HIVE_CELL_TOKEN= \\
    -- /path/to/scripts/mail-attention-set.py 'cell:<32hex>@gen'

Adopted seats are a session fence, not an env binding: systemd-run
creates a new session (that is the escalation). Spawned seats also
carry HIVE_CELL_TOKEN; clearing only HIVE_CELL is not enough.

Peek is Hive JSON-RPC attention.peek ({cell} only). Join walks bound_pid
ancestors until a mux ReadPane.child_pid matches. Clear is not this tool.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
from pathlib import Path

HIVE_PROTO = 2
MUX_PROTO = 1
MAX_LINE = 1 << 20


def xdg_runtime() -> Path:
    raw = os.environ.get("XDG_RUNTIME_DIR")
    if raw:
        return Path(raw)
    return Path(f"/run/user/{os.getuid()}")


def hive_sock() -> Path:
    for key in ("HIVE_SOCKET", "HIVE_SOCK"):
        override = os.environ.get(key)
        if override:
            return Path(override)
    return xdg_runtime() / "hive" / "control.sock"


def mux_sock() -> Path:
    override = (
        os.environ.get("MUX_SOCK")
        or os.environ.get("PMUX_SOCKET")
        or os.environ.get("PMUX_SOCKET")
    )
    if override:
        return Path(override)
    return xdg_runtime() / "prismattyc" / "pmux.sock"


def send_line(conn: socket.socket, obj: object) -> None:
    conn.sendall(json.dumps(obj, separators=(",", ":")).encode() + b"\n")


def read_line(conn: socket.socket) -> dict:
    data = bytearray()
    while len(data) <= MAX_LINE:
        chunk = conn.recv(min(65536, MAX_LINE + 1 - len(data)))
        if not chunk:
            raise SystemExit("peer closed before one JSON line")
        data.extend(chunk)
        nl = data.find(b"\n")
        if nl >= 0:
            return json.loads(data[:nl])
    raise SystemExit("response exceeded 1 MiB")


def hive_peek(cell: str) -> dict:
    sock = hive_sock()
    hello: dict = {
        "protocol": HIVE_PROTO,
        "plane": "control",
        "client": "prismattyc-mail-attention-set",
    }
    lease = sock.with_name(sock.name + ".lease")
    if lease.is_file():
        token = lease.read_text(encoding="utf-8").strip()
        if token:
            hello["lease"] = token
    req = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "attention.peek",
        "params": {"cell": cell},
    }
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(10)
        conn.connect(str(sock))
        send_line(conn, hello)
        send_line(conn, req)
        return read_line(conn)


class Mux:
    def __init__(self, path: Path) -> None:
        self.conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.conn.settimeout(10)
        self.conn.connect(str(path))
        self.next_id = 1

    def close(self) -> None:
        self.conn.close()

    def request(self, obj: dict) -> dict:
        if "request_id" not in obj:
            obj = dict(obj)
            obj["request_id"] = self.next_id
            self.next_id += 1
        send_line(self.conn, obj)
        return read_line(self.conn)


def ancestors(pid: int) -> list[int]:
    chain = []
    seen: set[int] = set()
    cur = pid
    while cur and cur not in seen:
        seen.add(cur)
        chain.append(cur)
        stat = Path(f"/proc/{cur}/stat")
        if not stat.is_file():
            break
        text = stat.read_text(encoding="utf-8", errors="replace")
        close = text.rfind(")")
        if close < 0:
            break
        fields = text[close + 2 :].split()
        if len(fields) < 2:
            break
        cur = int(fields[1])
        if cur <= 1:
            break
    return chain


def pane_for_bound_pid(mux: Mux, client_id: int, bound_pid: int) -> int:
    walk = set(ancestors(bound_pid))
    snap = mux.request({"type": "snapshot", "version": MUX_PROTO})
    sessions = snap["response"]["snapshot"]["sessions"]
    matches = []
    for sess in sessions:
        for win in sess.get("windows", []):
            for pane in win.get("panes", []):
                pane_id = pane["id"]
                reply = mux.request(
                    {
                        "type": "read_pane",
                        "version": MUX_PROTO,
                        "client_id": client_id,
                        "pane_id": pane_id,
                    }
                )
                child = (reply.get("response") or {}).get("content", {}).get(
                    "child_pid"
                )
                if child in walk:
                    matches.append(pane_id)
    if len(matches) != 1:
        raise SystemExit(
            f"expected one pane whose child_pid is an ancestor of {bound_pid}; "
            f"got {matches}"
        )
    return matches[0]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "cell",
        help="full cell:<32 hex>@generation (a@1 / 12@1 also accepted)",
    )
    args = parser.parse_args()
    peek = hive_peek(args.cell)
    if "error" in peek:
        err = peek["error"]
        print(json.dumps(peek, sort_keys=True), file=sys.stderr)
        code = err.get("code") if isinstance(err, dict) else None
        if code == -32104:
            print(
                "cell principal cannot attention.peek; rerun under "
                "systemd-run --user --setenv=HIVE_CELL=",
                file=sys.stderr,
            )
        return 1
    result = peek.get("result") or {}
    if result.get("found") is False:
        print("attention.peek found=false; do not Set", file=sys.stderr)
        return 2
    depth = result.get("depth")
    if depth is None:
        print("attention.peek depth is null (UNKNOWN); do not Set", file=sys.stderr)
        return 2
    if int(depth) == 0:
        print("attention.peek depth=0; do not Set", file=sys.stderr)
        return 2
    bound_pid = int(result["bound_pid"])
    queue_rev = int(result["queue_rev"])
    full_cell = str(result.get("cell") or args.cell)
    gen = int(full_cell.rsplit("@", 1)[1])

    mux = Mux(mux_sock())
    try:
        reg = mux.request({"type": "register_client", "version": MUX_PROTO})
        client_id = reg["response"]["client_id"]
        pane_id = pane_for_bound_pid(mux, client_id, bound_pid)
        set_reply = mux.request(
            {
                "type": "mail_attention_set",
                "version": MUX_PROTO,
                "client_id": client_id,
                "pane_id": pane_id,
                "cell": full_cell,
                "gen": gen,
                "queue_rev": queue_rev,
                "depth": int(depth),
                "bound_pid": bound_pid,
            }
        )
    finally:
        mux.close()
    print(json.dumps(set_reply, sort_keys=True))
    if set_reply.get("status") != "ok":
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
