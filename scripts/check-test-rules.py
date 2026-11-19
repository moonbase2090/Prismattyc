#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Flag fixed-time sleeps and retries in Rust tests against a shrink-only baseline.

Rules checked:
  * Fixed sleeps in test code (`thread::sleep`, `tokio::time::sleep`, ...).
    Test code is any file under a `tests/` directory, any item behind a
    `cfg` that needs `test`, any `#[test]`-style function, and any module
    file declared from test code. A sleep that keeps a fixture child
    process alive is allowed when its line, or the line above, carries a
    `keep-alive: <reason>` comment.
    Calls count through renamed imports too (`use std::thread::sleep as
    pause;`, `use std::thread as t;`).
  * Test retries: a `retries` key in any nextest config, `NEXTEST_RETRIES`
    anywhere, or a `--retries` flag in any file that runs nextest. These
    always fail; the baseline cannot hold them.
  * Kill timeouts in nextest config (`slow-timeout` with
    `terminate-after`), measured as period x terminate-after seconds.

The baseline (.config/test-rules-baseline.toml) pins exact per-file sleep
counts and per-profile timeout budgets. The check fails when a count or
budget goes up, when a new one appears, and when one goes down until the
baseline is lowered with --write. --write only ever lowers the baseline.

Usage:
  python3 scripts/check-test-rules.py            # check
  python3 scripts/check-test-rules.py --write    # lower the baseline
  python3 scripts/check-test-rules.py --init     # create a missing baseline
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

BASELINE = ".config/test-rules-baseline.toml"
SELF_FILES = {"scripts/check-test-rules.py", "scripts/check-test-rules_test.py"}
RETRY_SCAN_SUFFIXES = {".yml", ".yaml", ".sh", ".py", ".toml", ".ps1", ".bash"}
RETRY_SCAN_NAMES = {"Makefile", "justfile", "Justfile"}

SLEEP_FNS = ("sleep", "sleep_until", "sleep_ms")
SLEEP_MODULES = ("thread", "time", "task")
USE_STMT = re.compile(r"\buse\s+([^;]*);")
USE_TOKEN = re.compile(r"[A-Za-z_]\w*|::|[{},*]")
FN_DEF_BEFORE = re.compile(r"\bfn\s*$")
KEEP_ALIVE = re.compile(r"//.*\bkeep-alive:\s*\S")
RETRY_ENV = re.compile(r"NEXTEST_RETRIES")
RETRY_FLAG = re.compile(r"(?<![\w-])--retries\b")
NEXTEST = re.compile(r"\bnextest\b")
ATTR_START = re.compile(r"#\s*(!?)\s*\[")
MOD_DECL = re.compile(r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(r#)?([A-Za-z_]\w*)\s*;")
PATH_ATTR = re.compile(r'^path\s*=\s*"([^"]*)"$')
DURATION = re.compile(r"^\s*(\d+(?:\.\d+)?)\s*(ms|s|m|h)?\s*$")


# --------------------------------------------------------------------------
# Rust lexing


def mask_rust(src: str) -> str:
    """Blank out comments and string/char literals, keeping offsets and newlines."""
    out = list(src)
    n = len(src)

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    token = re.compile(r"//|/\*|\"|'|\b(?:b|c|br|cr)?r#*\"|\bb'")
    i = 0
    while True:
        m = token.search(src, i)
        if not m:
            break
        start = m.start()
        tok = m.group()
        if tok == "//":
            end = src.find("\n", start)
            end = n if end < 0 else end
            blank(start, end)
            i = end
        elif tok == "/*":
            depth, k = 1, start + 2
            while k < n and depth:
                if src.startswith("/*", k):
                    depth += 1
                    k += 2
                elif src.startswith("*/", k):
                    depth -= 1
                    k += 2
                else:
                    k += 1
            blank(start, k)
            i = k
        elif tok != '"' and tok.endswith('"'):  # raw string
            hashes = tok.count("#")
            close = '"' + "#" * hashes
            end = src.find(close, m.end())
            end = n if end < 0 else end + len(close)
            blank(start, end)
            i = end
        elif tok == '"':
            k = start + 1
            while k < n and src[k] != '"':
                k += 2 if src[k] == "\\" else 1
            blank(start, k + 1)
            i = k + 1
        else:  # ' or b'
            q = m.end() - 1
            if q + 1 < n and src[q + 1] == "\\":
                k = q + 3  # past the escaped character, which may be a quote
                while k < n and src[k] != "'" and src[k] != "\n":
                    k += 1
                if k < n and src[k] == "'":
                    blank(start, k + 1)
                    i = k + 1
                else:
                    i = q + 1
            elif q + 2 < n and src[q + 2] == "'" and src[q + 1] != "\n":
                blank(start, q + 3)
                i = q + 3
            else:
                i = q + 1  # lifetime or label
    return "".join(out)


def matching(code: str, open_at: int) -> int:
    """Index just past the bracket matching the one at open_at."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[code[open_at]]]
    k = open_at + 1
    n = len(code)
    while k < n and stack:
        c = code[k]
        if c in pairs:
            stack.append(pairs[c])
        elif c in ")]}":
            if stack and c == stack[-1]:
                stack.pop()
        k += 1
    return k


def item_end(code: str, start: int) -> int:
    """End of the item that starts at `start` (after its attributes)."""
    k = start
    n = len(code)
    angle = 0
    while k < n:
        c = code[k]
        if c in "([":
            k = matching(code, k)
            continue
        if c == ";":
            return k + 1
        if c == "{":
            return matching(code, k)
        if c == "<":
            angle += 1
        elif c == ">" and code[k - 1] not in "-=" and angle:
            angle -= 1
        elif c in ")]}" or (c == "," and not angle):
            # A field, variant or argument: the item ends at its delimiter.
            return k
        k += 1
    return n


# --------------------------------------------------------------------------
# cfg predicates


def parse_cfg(text: str):
    """Parse a cfg predicate into nested tuples; atoms are strings."""
    pos = 0
    tokens = re.findall(r'"(?:[^"\\]|\\.)*"|[A-Za-z_][\w:]*|[(),=]', text)

    def pred():
        nonlocal pos
        name = tokens[pos]
        pos += 1
        if pos < len(tokens) and tokens[pos] == "(":
            pos += 1
            args = []
            while tokens[pos] != ")":
                args.append(pred())
                if tokens[pos] == ",":
                    pos += 1
            pos += 1
            return (name, args)
        if pos < len(tokens) and tokens[pos] == "=":
            pos += 2
            return ("kv", name)
        return name

    return pred()


def cfg_value(node, test: bool):
    """Three-valued evaluation: True, False, or None (unknown)."""
    if isinstance(node, str):
        return test if node == "test" else None
    name, args = node
    if name == "kv":
        return None
    vals = [cfg_value(a, test) for a in args]
    if name == "all":
        if False in vals:
            return False
        return True if all(v is True for v in vals) else None
    if name == "any":
        if True in vals:
            return True
        return False if all(v is False for v in vals) else None
    if name == "not" and len(vals) == 1:
        return None if vals[0] is None else not vals[0]
    return None


def attr_is_test(body: str) -> bool:
    body = " ".join(body.split())
    m = re.match(r"^cfg\s*\((.*)\)$", body)
    if m:
        try:
            return cfg_value(parse_cfg(m.group(1)), test=False) is False
        except (IndexError, ValueError):
            return False
    head = re.match(r"^([A-Za-z_][\w:]*)", body)
    if not head:
        return False
    name = head.group(1).replace(" ", "")
    return name == "test" or name.endswith("::test")


# --------------------------------------------------------------------------
# Test regions


@dataclass
class FileScan:
    whole: bool = False
    regions: list[tuple[int, int]] = field(default_factory=list)
    test_mods: list[str] = field(default_factory=list)  # resolved repo paths
    plain_mods: list[tuple[str, str | None]] = field(default_factory=list)


def module_paths(path: str, name: str, path_attr: str | None) -> list[str]:
    p = PurePosixPath(path)
    if path_attr is not None:
        return [str(p.parent / path_attr)]
    if p.name in ("lib.rs", "main.rs", "mod.rs"):
        base = p.parent
    else:
        base = p.parent / p.stem
    return [str(base / f"{name}.rs"), str(base / name / "mod.rs")]


def scan_file(path: str, code: str, src: str) -> FileScan:
    """Find test regions in masked `code`; attribute text comes from `src`."""
    scan = FileScan()
    k = 0
    n = len(code)
    while True:
        m = ATTR_START.search(code, k)
        if not m:
            break
        inner = m.group(1) == "!"
        bracket = m.end() - 1
        end = matching(code, bracket)
        body = src[bracket + 1 : end - 1].strip()
        if inner:
            if attr_is_test(body):
                scan.whole = True
            k = end
            continue
        # Gather the run of outer attributes on one item.
        attrs = [body]
        j = end
        while True:
            nxt = ATTR_START.match(code, _skip_ws(code, j))
            if not nxt or nxt.group(1) == "!":
                break
            b2 = nxt.end() - 1
            e2 = matching(code, b2)
            attrs.append(src[b2 + 1 : e2 - 1].strip())
            j = e2
        item_start = _skip_ws(code, j)
        path_attr = None
        for a in attrs:
            pm = PATH_ATTR.match(" ".join(a.split()))
            if pm:
                path_attr = pm.group(1)
        md = MOD_DECL.match(code, item_start)
        if any(attr_is_test(a) for a in attrs):
            stop = item_end(code, item_start)
            scan.regions.append((m.start(), stop))
            if md:
                scan.test_mods.extend(module_paths(path, md.group(2), path_attr))
            k = stop
            continue
        if md and path_attr is not None:
            scan.plain_mods.append((md.group(2), path_attr))
        k = j
    # Every `mod x;` matters when this file turns out to be test code: its
    # child modules are test code too.
    for line_start in _line_starts(code):
        md = MOD_DECL.match(code, _skip_ws(code, line_start, newline=False))
        if md:
            scan.plain_mods.append((md.group(2), None))
    return scan


def _skip_ws(code: str, k: int, newline: bool = True) -> int:
    ws = " \t\r\n" if newline else " \t"
    while k < len(code) and code[k] in ws:
        k += 1
    return k


def _line_starts(code: str):
    yield 0
    for m in re.finditer(r"\n", code):
        yield m.end()


def in_tests_dir(path: str) -> bool:
    return "tests" in PurePosixPath(path).parts[:-1]


def use_items(code: str) -> list[tuple[list[str], str]]:
    """Flatten every `use` tree into (path segments, local name) pairs."""
    items: list[tuple[list[str], str]] = []
    for stmt in USE_STMT.finditer(code):
        toks = USE_TOKEN.findall(stmt.group(1))
        pos = 0

        def tree(prefix: list[str]) -> None:
            nonlocal pos
            path = list(prefix)
            if pos < len(toks) and toks[pos] == "::":
                pos += 1
            while pos < len(toks):
                tok = toks[pos]
                if tok == "{":
                    pos += 1
                    while pos < len(toks) and toks[pos] != "}":
                        tree(path)
                        if pos < len(toks) and toks[pos] == ",":
                            pos += 1
                    pos += 1
                    return
                if tok == "*":
                    pos += 1
                    return
                if tok in ",}":
                    return
                path.append(tok)
                pos += 1
                if pos < len(toks) and toks[pos] == "::":
                    pos += 1
                    continue
                break
            name = path[-1] if path else ""
            if pos + 1 < len(toks) and toks[pos] == "as":
                name = toks[pos + 1]
                pos += 2
            if path and path[-1] == "self":
                path = path[:-1]
                name = name if name != "self" else (path[-1] if path else "")
            if path and name and name != "_":
                items.append((path, name))

        try:
            tree([])
        except IndexError:
            continue
    return items


def sleep_patterns(code: str) -> tuple[re.Pattern, re.Pattern | None]:
    """Qualified and bare sleep-call patterns, including renamed imports."""
    modules = set(SLEEP_MODULES)
    names: set[str] = set()
    for path, name in use_items(code):
        if path[-1] in SLEEP_FNS:
            names.add(name)
        elif path[-1] in SLEEP_MODULES:
            modules.add(name)
    fns = "|".join(SLEEP_FNS)
    qualified = re.compile(
        r"\b(?:" + "|".join(sorted(map(re.escape, modules))) + r")\s*::\s*(?:" + fns + r")\s*\("
    )
    bare = None
    if names:
        bare = re.compile(r"(?<![\w:.])(?:" + "|".join(sorted(map(re.escape, names))) + r")\s*\(")
    return qualified, bare


def count_sleeps(code: str, src: str, regions, whole: bool) -> list[int]:
    """Line numbers of fixed sleeps inside test code."""
    lines = src.split("\n")
    hits: list[int] = []
    qualified, bare = sleep_patterns(code)
    patterns = [qualified] + ([bare] if bare else [])
    seen: set[int] = set()
    for pat in patterns:
        for m in pat.finditer(code):
            at = m.start()
            if at in seen:
                continue
            if FN_DEF_BEFORE.search(code, max(0, at - 16), at):
                continue
            if not whole and not any(a <= at < b for a, b in regions):
                continue
            # The bare pattern also matches inside `thread::sleep(`; the
            # qualified match already counted it.
            if pat is bare and re.search(r"::\s*$", code[max(0, at - 8) : at]):
                continue
            seen.add(at)
            line = code.count("\n", 0, at)
            here = lines[line]
            above = lines[line - 1] if line > 0 else ""
            # A marker above counts only on a comment line of its own, so a
            # trailing marker never covers the next statement.
            if KEEP_ALIVE.search(here) or (
                above.lstrip().startswith("//") and KEEP_ALIVE.search(above)
            ):
                continue
            hits.append(line + 1)
    return sorted(hits)


def find_sleeps(root: Path, files: list[str]) -> dict[str, list[int]]:
    sources: dict[str, tuple[str, str]] = {}
    scans: dict[str, FileScan] = {}
    for rel in files:
        try:
            src = (root / rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError):
            continue
        code = mask_rust(src)
        sources[rel] = (src, code)
        scans[rel] = scan_file(rel, code, src)
    whole = {rel for rel, s in scans.items() if s.whole or in_tests_dir(rel)}
    # Module files declared from test code are test code, and so are their
    # own child modules.
    pending = list(whole) + [p for s in scans.values() for p in s.test_mods]
    while pending:
        rel = pending.pop()
        if rel not in scans:
            continue
        whole.add(rel)
        for name, attr in scans[rel].plain_mods:
            for child in module_paths(rel, name, attr):
                if child in scans and child not in whole:
                    pending.append(child)
    found: dict[str, list[int]] = {}
    for rel, (src, code) in sources.items():
        hits = count_sleeps(code, src, scans[rel].regions, rel in whole)
        if hits:
            found[rel] = hits
    return found


# --------------------------------------------------------------------------
# nextest config


def seconds(value) -> float | None:
    if isinstance(value, (int, float)):
        return float(value)
    if not isinstance(value, str):
        return None
    total = 0.0
    parts = value.split()
    for part in parts:
        m = DURATION.match(part)
        if not m:
            return None
        num = float(m.group(1))
        unit = m.group(2) or "s"
        total += num * {"ms": 0.001, "s": 1, "m": 60, "h": 3600}[unit]
    return total if parts else None


def kill_budget(value) -> float | None:
    if not isinstance(value, dict):
        return None
    after = value.get("terminate-after")
    period = seconds(value.get("period", "60s"))
    if not after or period is None:
        return None
    return period * int(after)


def scan_nextest(root: Path, configs: list[str]):
    retries: list[str] = []
    budgets: dict[str, float] = {}
    for rel in configs:
        try:
            data = tomllib.loads((root / rel).read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as err:
            retries.append(f"{rel}: cannot parse ({err})")
            continue

        def walk(node, where):
            if isinstance(node, dict):
                for key, val in node.items():
                    here = f"{where}.{key}" if where else key
                    if key == "retries":
                        retries.append(f"{rel}: {here} sets test retries")
                    walk(val, here)
            elif isinstance(node, list):
                for i, val in enumerate(node):
                    walk(val, f"{where}[{i}]")

        walk(data, "")
        for name, prof in (data.get("profile") or {}).items():
            scopes = [(f"profile.{name}", prof)]
            for i, ov in enumerate(prof.get("overrides") or []):
                scopes.append((f"profile.{name}.overrides[{i}]", ov))
            for scope, table in scopes:
                budget = kill_budget(table.get("slow-timeout"))
                if budget is not None:
                    budgets[f"{rel} {scope}"] = budget
    return retries, budgets


def scan_retry_text(root: Path, files: list[str]) -> list[str]:
    """Retries set through the environment or a nextest command line.

    A file that mentions nextest is checked for a `--retries` flag anywhere,
    so shell line continuations, argument lists and variables cannot split
    the flag from the command.
    """
    out = []
    for rel in files:
        p = PurePosixPath(rel)
        if rel in SELF_FILES or rel == BASELINE:
            continue
        if p.suffix not in RETRY_SCAN_SUFFIXES and p.name not in RETRY_SCAN_NAMES:
            continue
        try:
            text = (root / rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError):
            continue
        flag = RETRY_FLAG if NEXTEST.search(text) else None
        for no, line in enumerate(text.split("\n"), 1):
            if RETRY_ENV.search(line) or (flag and flag.search(line)):
                out.append(f"{rel}:{no}: test retries ({line.strip()[:80]})")
    return out


# --------------------------------------------------------------------------
# Baseline


def tracked_files(root: Path) -> list[str]:
    res = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True,
        capture_output=True,
    )
    return [p for p in res.stdout.decode().split("\0") if p]


def load_baseline(root: Path):
    path = root / BASELINE
    if not path.exists():
        return None
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    return (
        list(data.get("scan", {}).get("exclude", [])),
        {k: int(v) for k, v in data.get("sleeps", {}).items()},
        {k: float(v) for k, v in data.get("timeouts", {}).items()},
    )


def write_baseline(root: Path, exclude, sleeps, timeouts) -> None:
    def q(s: str) -> str:
        return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'

    lines = [
        "# Shrink-only baseline for scripts/check-test-rules.py.",
        "# Pins fixed sleeps in test code per file and nextest kill timeouts",
        "# (period x terminate-after, seconds). CI fails when a number rises,",
        "# when a new entry appears, and when one falls until this file is",
        "# lowered with `python3 scripts/check-test-rules.py --write`.",
        "",
        "[scan]",
        "exclude = [" + ", ".join(q(e) for e in exclude) + "]",
        "",
        "[sleeps]",
    ]
    lines += [f"{q(k)} = {sleeps[k]}" for k in sorted(sleeps)]
    lines += ["", "[timeouts]"]
    lines += [f"{q(k)} = {fmt(timeouts[k])}" for k in sorted(timeouts)]
    (root / BASELINE).parent.mkdir(parents=True, exist_ok=True)
    (root / BASELINE).write_text("\n".join(lines) + "\n", encoding="utf-8")


def compare(base: dict, now: dict):
    rises, falls = [], []
    for key in sorted(set(base) | set(now)):
        b, c = base.get(key, 0), now.get(key, 0)
        if c > b:
            rises.append((key, b, c))
        elif c < b:
            falls.append((key, b, c))
    return rises, falls


def fmt(v) -> str:
    return str(int(v)) if float(v).is_integer() else str(v)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--root", default=".", help="repository root (default: .)")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true", help="lower the baseline to the current counts")
    mode.add_argument("--init", action="store_true", help="create the baseline when it is missing")
    args = ap.parse_args(argv)
    root = Path(args.root).resolve()

    loaded = load_baseline(root)
    if args.init and loaded is not None:
        print(f"{BASELINE} already exists; use --write to lower it", file=sys.stderr)
        return 2
    if loaded is None and not args.init:
        print(f"{BASELINE} is missing; create it with --init", file=sys.stderr)
        return 2
    exclude, base_sleeps, base_timeouts = loaded or (["third_party/"], {}, {})

    files = [f for f in tracked_files(root) if not f.startswith(tuple(exclude))]
    rust = [f for f in files if f.endswith(".rs")]
    configs = [f for f in files if PurePosixPath(f).name == "nextest.toml"]

    sleeps = find_sleeps(root, rust)
    counts = {k: len(v) for k, v in sleeps.items()}
    retries, budgets = scan_nextest(root, configs)
    retries += scan_retry_text(root, files)

    if args.init:
        if retries:
            print("\n".join(retries), file=sys.stderr)
            print("test retries cannot be baselined; remove them first", file=sys.stderr)
            return 1
        write_baseline(root, exclude, counts, budgets)
        print(f"wrote {BASELINE}: {sum(counts.values())} sleeps in {len(counts)} files, {len(budgets)} timeouts")
        return 0

    s_rise, s_fall = compare(base_sleeps, counts)
    t_rise, t_fall = compare(base_timeouts, budgets)

    problems: list[str] = list(retries)
    for key, b, c in s_rise:
        where = ", ".join(str(n) for n in sleeps.get(key, []))
        problems.append(
            f"{key}: {c} fixed sleeps in test code, baseline {b} (lines {where}). "
            "Await the operation or a signal, or use a paused clock."
        )
    for key, b, c in t_rise:
        problems.append(f"{key}: nextest kill timeout {fmt(c)}s, baseline {fmt(b)}s. Timeouts only go down.")

    if args.write:
        if problems:
            print("\n".join(problems), file=sys.stderr)
            print("--write only lowers the baseline; fix the rises first", file=sys.stderr)
            return 1
        write_baseline(
            root,
            exclude,
            {k: v for k, v in counts.items() if v},
            {k: v for k, v in budgets.items() if v},
        )
        print(f"lowered {BASELINE}: {len(s_fall)} sleep entries, {len(t_fall)} timeouts")
        return 0

    for key, b, c in s_fall:
        problems.append(f"{key}: {c} fixed sleeps, baseline {b}. Lower the baseline with --write.")
    for key, b, c in t_fall:
        problems.append(f"{key}: kill timeout {fmt(c)}s, baseline {fmt(b)}s. Lower the baseline with --write.")

    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    print(
        f"test rules ok: {sum(counts.values())} baselined sleeps in {len(counts)} files, "
        f"{len(budgets)} pinned timeouts, no retries"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
