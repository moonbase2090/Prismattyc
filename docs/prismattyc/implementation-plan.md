# Prismattyc Implementation Plan — Fold Switchboard into Prism

**Status:** Complete (2026-08-26). Every story merged through PR review:
Phase 0–1 (#2–#9), 1b (#11, #13, #14, #16, #17), 2 (#15, #18–#21),
3 (#22, #25, #26). Remaining: 3.5 archives the Switchboard repo
(brandan executes).
**Date:** 2026-08-25
**Pairs with:** `design-plan.md`
**Tracking:** Waypoint project `prismattyc` (prefix PT)

All work happens in `brandanmajeske/Prismattyc`. The original task list
(`fold-tasks-original.md`) is preserved for reference; this plan re-numbers
work for the new repo and assigns owners. Owner convention: the non-owner
peer reviews; verdict PASS = merge authorized.

Owners: **operator-b** (Cursor agent), **operator-a** (Grok agent).

---

## Phase 0: Repo seed and rename — epic

| # | Story | Owner | Depends on |
|---|-------|-------|-----------|
| 0.1 | Seed Prismattyc from Prism (history + tags), protect main | operator-b | — (DONE) |
| 0.2 | Rename binaries: `prismattyc-mux`→`pmux`, `prismattyc-mux-server`→`pmuxd`, `prism-mux-attach`→`pmux-attach`; update socket/config/data paths to `prismattyc`; honor legacy env vars | operator-a | 0.1 |
| 0.3 | Import Switchboard crates into workspace (`switchboard-store`, `switchboard-proto`, `switchboard-mcp`, `switchboard-cli`) as-is; workspace builds green | operator-b | 0.1 |
| 0.4 | CI: extend `ci.yml` (Local Actions / act) to cover imported crates; verify `act` passes end-to-end | operator-b | 0.3 |
| 0.5 | Docs: README rename to Prismattyc, `pmux` quickstart, naming table | operator-a | 0.2 |

**Exit:** `act` green; `pmux --help` runs; Switchboard crates compile in-workspace, still talking to the legacy switchboardd.

## Phase 1: Mailbox foundation in pmuxd — epic

| # | Story | Owner | Depends on |
|---|-------|-------|-----------|
| 1.1 | `agent_id: Option<String>` on Session; `--agent`/`--no-agent` flags; `session_by_agent` lookup; persisted | operator-a | 0.2 |
| 1.2 | Mailbox store module (`src/mailbox/store.rs`): SQLite at new path, port `switchboard-store` ops + its 11 unit tests | operator-a | 0.3 |
| 1.3 | `MailboxWatch` condvar wait/notify (`src/mailbox/watch.rs`) | operator-b | 1.2 |
| 1.4 | `Mail*` protocol messages + dispatch on the mux socket | operator-b | 1.1, 1.2, 1.3 |
| 1.5 | In-process doorbell: `on_mail_stored` → attention + token injection | operator-b | 1.1, 1.2, 1.4 |
| 1.6 | Headless sessions (`pmux new --headless`) | operator-a | 1.1 |

**Exit:** unit tests green under `act`; Mail* roundtrip on the socket.

## Phase 1b: Dogfood — epic

| # | Story | Owner | Depends on |
|---|-------|-------|-----------|
| 1b.1 | End-to-end: two sessions, send → attention + injection → claim → commit, zero external watchers | operator-b | 1.5 |
| 1b.2 | Restart durability: held/open letters survive pmuxd restart | operator-a | 1b.1 |
| 1b.3 | Latency check: send → injection <10ms in-process | operator-a | 1b.1 |

**Exit:** external doorbell path demonstrably dead; latency criterion met.

## Phase 2: Client migration — epic

| # | Story | Owner | Depends on |
|---|-------|-------|-----------|
| 2.1 | `pmux mail <verb>` subcommand native on Mail* protocol (port of switchboard-cli UX, incl. `status`) | operator-b | 1.4 |
| 2.2 | `pmux-mcp` adapter (renamed switchboard-mcp) targeting pmux socket; 9 tools unchanged | operator-a | 1.4 |
| 2.3 | `switchboard` compat shim (execs `pmux mail`); `SWITCHBOARD_SOCKET`/`SWITCHBOARD_AGENT` honored | operator-b | 2.1 |
| 2.4 | First-run mail.db migration from `$XDG_DATA_HOME/switchboard/` | operator-a | 1.2 |
| 2.5 | Switch agent configs (`.cursor/mcp.json`, grok, codex) to `pmux-mcp`; dogfood both agents on Prismattyc | operator-b | 2.2, 2.3 |

**Exit:** both agents run their mail loop against pmuxd only; legacy switchboardd no longer started.

## Phase 3: Decommission — epic

| # | Story | Owner | Depends on |
|---|-------|-------|-----------|
| 3.1 | Remove imported switchboard crates whose logic was ported (`switchboard-store`, daemon leftovers); slim `switchboard-proto` into the shim | operator-a | 2.5 |
| 3.2 | Remove doorbell script, watcher units/plists, doorbell.map from contrib; update install scripts | operator-b | 2.5 |
| 3.3 | Remove compat shim + legacy env vars; rename attention cell if decided (design Q2) | operator-a | 3.1, 3.2 |
| 3.4 | Docs sweep: README, tutorial tool text, fold docs marked completed | operator-b | 3.3 |
| 3.5 | Archive Switchboard repo read-only; final pointer in its README | operator-b (brandan executes archive) | 3.4 |

**Exit:** one daemon (`pmuxd`), one repo, zero watcher processes.

3.2 note: the doorbell script, watcher units/plists, and `doorbell.map`
live in the **Switchboard repo's** `contrib/` — they were never imported,
and 3.5 archives that repo. In this repo 3.2 is the host-side unit and
binary removal (dogfood.md checklist step 7) plus confirming the install
scripts carry no switchboard references. The Hive-era
`scripts/mail-attention-*.py` operator tools are explicitly out of fold
scope (design plan).

---

## Dependency graph

```
0.1(done) ──► 0.2 ──► 0.5
        └───► 0.3 ──► 0.4
1.1 ◄── 0.2        1.2 ◄── 0.3        1.3 ◄── 1.2
1.4 ◄── 1.1 + 1.2 + 1.3
1.5 ◄── 1.1 + 1.2 + 1.4
1.6 ◄── 1.1
1b.1 ◄── 1.5 ──► 1b.2, 1b.3
2.1, 2.2 ◄── 1.4 (and dogfood sign-off)
2.3 ◄── 2.1 ; 2.4 ◄── 1.2 ; 2.5 ◄── 2.2 + 2.3
3.x ◄── 2.5
```

Parallelizable early: 0.2 ∥ 0.3; 1.1 ∥ 1.2; 1.6 ∥ 1.3–1.5.

## Working agreement

1. Every story = one PR; the non-owner reviews.
2. Review verdict PASS ⇒ merge immediately (standing authorization).
   CHANGES ⇒ discuss in PR comments or over switchboard mail.
3. CI gate is local: `act` must pass before review is requested.
4. Coordination over switchboard mail (operator-b ↔ operator-a seats);
   story state in Waypoint (`prismattyc` project).
5. Scope questions or design-plan changes → mail thread, both sign off
   before the plan doc is amended.
6. Docs and PR reviews follow ASD-STE100-style clarity (short, active,
   imperative, consistent terms) and the Google developer documentation
   style guide (task-oriented structure, present tense, numbered
   procedures). See AGENTS.md §Documentation and review style.
