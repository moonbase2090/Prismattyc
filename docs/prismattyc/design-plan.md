# Prismattyc Design Plan — Folding Switchboard into Prism

**Status:** Implemented (2026-08-26). Phases 0–3 landed; see
`implementation-plan.md` for the story-to-PR map. Two deliberate
deviations from this text: the `switchboard` compat shim and the
`SWITCHBOARD_SOCKET`/`SWITCHBOARD_AGENT` env vars were retired in
(identity is now `--as` / `$PMUX_AGENT` / pane session), and the
inject token is now `PMUX_MAIL` (superseding `HIVE_MAIL` and
`SWITCHBOARD_MAIL`). The attention-cell name is `mail` (superseding
`switchboard`). `MailChannel` is retired.
**Date:** 2026-08-25
**Authors:** operator-b, operator-a
**Supersedes/extends:** `fold-proposal-original.md` (kiro-sb, kiro-pm)

This plan takes the approved-direction fold proposal and adapts it to the
Prismattyc repo and the `pmux` command naming. Technical substance
(session-as-seat, in-process doorbell, SQLite store, condvar wait) is
unchanged from the original design unless explicitly noted here.

---

## 1. Naming convention

The project is **Prismattyc**; the invoking command is **`pmux`**. Nobody
types `prismattyc`.

| Thing | Old (Prism / Switchboard) | New (Prismattyc) |
|-------|---------------------------|------------------|
| Repo | `brandanmajeske/Prism` + `brandanmajeske/Switchboard` | `brandanmajeske/Prismattyc` |
| Mux client CLI | `prismattyc-mux` | **`pmux`** |
| Mux server | `prismattyc-mux-server` | **`pmuxd`** |
| Attach helper | `prism-mux-attach` | **`pmux-attach`** |
| Mail CLI | `switchboard` | **`pmux mail <verb>`** (native); `switchboard` kept as compat shim until Phase 3 |
| MCP adapter | `switchboard-mcp` | **`pmux-mcp`** (crate + binary); tools are `pmux_*` |
| Control socket | `$XDG_RUNTIME_DIR/prismattyc-mux/…` | `$XDG_RUNTIME_DIR/prismattyc/pmux.sock` |
| Mail database | `$XDG_DATA_HOME/switchboard/mail.db` | `$XDG_DATA_HOME/prismattyc/mail.db` |
| Config | `~/.config/prismattyc-mux/`, `~/.config/switchboard/` | `~/.config/prismattyc/` |
| systemd user unit | `prismattyc-mux.service`, `switchboardd.service` | `pmuxd.service` |
| launchd plist | `com.prism.mux*`, `com.switchboard.*` | `com.prismattyc.pmuxd.plist` |

### Crate names

Cargo package names for the Prismattyc crates were later aligned with
the product (`prismattyc-core`, `prismattyc-mux`, …). The remaining MCP
adapter crate is **`pmux-mcp`** (directory + package + binary).

Switchboard crates were imported under `crates/`
(`switchboard-store`, `switchboard-proto`, `switchboard-mcp`,
`switchboard-cli`) until their logic was ported or shimmed, then deleted
per the decommission plan. The adapter crate is `pmux-mcp`.

### Env vars

- `PMUX_SOCKET` — overrides control socket path (replaces both the Prism
  and `SWITCHBOARD_SOCKET` overrides; both legacy vars honored during the
  transition window, removed in Phase 3).
- `SWITCHBOARD_AGENT` — still honored by the compat shim and `pmux-mcp`;
  `PMUX_AGENT` added as the preferred spelling.

### Rename surface (from operator-a's review)

Cargo package names stay (`prismattyc-mux` crate, nested `prism` TUI,
`prismattyc-host`). Only `[[bin]]` names and paths change. must honor
these during the transition window and drop them in Phase 3:

| Shipping today | must honor |
|---|---|
| `PMUX_SOCKET` | plus `PMUX_SOCKET` |
| `PMUX_INSTANCE` | plus `PMUX_INSTANCE` |
| `PMUX_BIN` / `PMUX_SERVER` / `PMUX_ATTACH` | test harness + daemon wrapper |
| `PMUX_ATTACH_ON_NEW` | config.rs |
| Socket `$XDG_RUNTIME_DIR/prism-{instance}.sock` (today `prism-default.sock`) | new `$XDG_RUNTIME_DIR/prismattyc/pmux.sock` **and** connect-to-old-path during the window, or agents lose the daemon |
| `scripts/prismattyc-mux-daemon.sh` | wrapper → `pmux` |
| `PMUX_MAIL` inject token | one doorbell token; supersedes `HIVE_MAIL` and `SWITCHBOARD_MAIL` |
| `install-man.sh`, e2e yaml, `scripts/test-phase2*.sh` / `test-phase3*.sh` | bin names |
| systemd/launchd unit names | `pmuxd.service` / `com.prismattyc.pmuxd.plist` |
| Completions and `~/.cargo/bin/prism-mux*` | post-install |

Hive's `scripts/mail-attention-set.py` is **not** part of this fold.
The inject token is `PMUX_MAIL`; the attention cell is `mail`.

---

## 2. Architecture (target state)

```
┌─────────────────────────────────────────────┐
│                  pmuxd                       │
│                                             │
│  ┌────────────┐   ┌──────────────────────┐  │
│  │ sessions   │   │ mailbox module       │  │
│  │            │   │                      │  │
│  │ agent_id   │──►│ SQLite store         │  │
│  │ panes      │   │ claim/commit/release │  │
│  │ children   │   │ wait (Condvar)       │  │
│  │ attention  │◄──│ in-process doorbell  │  │
│  └────────────┘   └──────────────────────┘  │
│                                             │
│  ┌────────────────────────────────────────┐ │
│  │ pmux socket protocol                   │ │
│  │ (existing mux messages + Mail* verbs)  │ │
│  └────────────────────────────────────────┘ │
└─────────────────────────────────────────────┘
        ▲                    ▲
        │                    │
   pmux (CLI)          pmux-mcp (per-tool-call)
   pmux mail <verb>    9 tools, unchanged semantics
```

The mux session IS the mailbox seat. No seat table, no generation
counters, no presence leases, no death-watch reaper, no doorbell shell
loop, no standalone `switchboardd`.

Presence, liveness, and addressing follow the original proposal's mapping
table (session lifetime replaces lease; mux child-death detection replaces
the reaper). See `fold-design-original.md` §Presence model.

### Session identity constraints (decided in PR #1 review)

- **`agent_id` defaults to `None`.** Do not default to the session name.
  `pmux new work` must not become addressable as `work`. Opt in with
  `--agent <id>`; `--no-agent` is the explicit restatement of the
  default. The session named `default` stays a mailbox-less workspace.
  **Headless exception:** `pmux new --headless NAME` exists to
  be a mailbox, so the mailbox address defaults to `NAME` unless
  `--agent` or `--no-agent` is given. A pane-ful session never gets this
  default.
- **`agent_id` is unique** across live sessions. A second bind gets
  `MailRefused`.
- **Never trust `MailSend.from`.** Identity is the connection's agent
  (`--as` flag / `$PMUX_AGENT` / `$SWITCHBOARD_AGENT` / session
  `agent_id`), same as today's seated `from`. A client-supplied `from`
  is a spoof hole.
- **`N@G` seat addressing is dropped in one step.** `send.to` is an
  agent id or alias only. `MailWho` returns
  `{agent_id, session, pane_live, aliases}` — not `2@819`. MCP schema
  text that says "or live seat (`0@1`)" changes in story 2.2. No
  compatibility parser for stale seats.
- **Unknown recipients still queue** (FR-1). `MailWho` does not list an
  agent until its session exists; document this.

---

## 3. Wire protocol

Mail verbs live on the existing mux Unix socket as NDJSON messages,
namespaced `Mail*`, per the original design (`MailSend`, `MailClaim`,
`MailCommit`, `MailRelease`, `MailInbox`, `MailWait`, `MailWho`,
`MailAlias`, `MailBroadcast` and their `Mail*` responses, plus
`MailRefused`).

One adjustment from the original design: because the CLI surface becomes
`pmux mail <verb>`, the transport mapping is native — `pmux mail` speaks
`MailClientMessage` directly with no `ClientMessage`→`Mail*` translation
layer. The translation layer only exists inside the `switchboard` compat
shim (and only until Phase 3).

---

## 4. CLI surface

```
pmux new [--headless] [--agent <name> | --no-agent] <session>
         # --headless: no pane; mailbox defaults to <session> unless --no-agent
pmux attach <session>          # via pmux-attach
pmux mail send <to> --summary <s> [--body <b>]
pmux mail claim [--json | --ids]
pmux mail commit <id>...
pmux mail release <id>...
pmux mail inbox
pmux mail watch [--timeout <secs>]
pmux mail who
pmux mail alias <name>
pmux mail broadcast --summary <s> [--body <b>]
pmux mail status # daemon/socket health (Switchboard verb, ported)
```

Identity resolution order: `--as <agent>` flag, `$PMUX_AGENT`,
`$SWITCHBOARD_AGENT`, then the session's `agent_id` when invoked inside a
pmux pane.

The `switchboard` binary remains installable through Phase 2 as a shim
execing `pmux mail …` with identical flags, so existing agent configs,
watcher recipes, and scripts keep working.

---

## 5. MCP surface

`pmux-mcp` serves the same 9 tools with identical names and semantics
(`send_letter`, `claim_letters`, … as today). Agent `.mcp.json` /
`.cursor/mcp.json` configs only change the `command` path. The
`pmux_tutorial` tool text is updated in Phase 3 to teach the
`pmux mail` spellings.

---

## 6. Doorbell (in-process)

Unchanged from the original design: `on_mail_stored` resolves
agent → session → active pane, arms sticky mail attention on the
`mail` attention cell, and injects the `PMUX_MAIL` token
plus the session's submit bytes — one synchronous in-process call, no
watcher, no shell. Headless sessions (no pane) skip injection; mail queues.

The attention cell name is `mail`. `switchboard` and `MailChannel` are retired.

---

## 7. Storage

- Schema unchanged (`letters` table, `msg:{seq:016x}` ids).
- New location `$XDG_DATA_HOME/prismattyc/mail.db`.
- `pmuxd` restarts: letters and session `agent_id` mappings persist;
  held letters re-surface on next claim.
- `pmuxd` does not import legacy Switchboard databases.

---

## 8. What gets eliminated

As the original proposal's table, plus the rename-era additions:

| Component | Fate |
|-----------|------|
| `switchboardd` binary + crate | Removed (logic → pmuxd mailbox module) |
| Seat table, reaper, death-watch, presence lease, generation counters | Removed |
| `switchboard-doorbell.sh`, watcher units/plists, `doorbell.map` | Removed |
| `switchboard` CLI binary | Removed in Phase 3 (shim until then) |
| `switchboard-mcp` crate + binary | Renamed `pmux-mcp`; tools are `pmux_*` |
| `prismattyc-mux*` binary names | Renamed `pmux*` |
| Switchboard repo | Archived read-only after Phase 3 |

---

## 9. CI

Local Actions (`act` + Docker) is the merge gate — same model Prism
already uses: jobs carry `if: github.actor == 'nektos/act'` so they no-op
on github.com and run under `act`. The seeded repo already contains that
`ci.yml`. New mailbox jobs (store tests, doorbell integration test,
migration test) are added to the same file as they land.

Merge rule for this project (per brandan): **PR + merge is pre-authorized
once the peer review verdict is PASS** — operator-b reviews operator-a's PRs,
operator-a reviews operator-b's PRs, no third party needed.

---

## 10. Open questions — resolved in PR #1 review (operator-a, 2026-08-25)

1. **Server binary: `pmuxd`.** A distinct daemon binary. `pmux --serve`
   would collide with the client CLI and with how systemd/launchd exec
   the server. Optional: alias `pmuxd` → same bin so both names appear
   in `--help`.
2. **Attention cell is `mail`.** `switchboard` and `MailChannel`
   (`Hive` / `Switchboard`) are retired. The inject token is
   `PMUX_MAIL`. The Hive supervisor path is separate.
3. **Keep condvar `MailWait`** on the per-client thread
   (`prism-control-client`). Dozens of agents is fine; no long-poll
   unless we measure a stuck-thread problem. Constraint: wait on
   `MailboxWatch` only; never park while holding the control-plane
   mutex.
4. **Exec shim is enough.** No Prism crate links `switchboard-proto`
   (only `switchboard-cli`, `switchboard-mcp`, and the excluded
   `switchboardd` do). Keep the proto crate until `pmux mail` and
   `pmux-mcp` speak `Mail*` natively, then slim it in 3.1.
