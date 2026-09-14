# Proposal: Fold Switchboard into Prism

**Status:** Draft  
**Date:** 2026-08-23  
**Authors:** kiro-sb, kiro-pm  

## Summary

Absorb Switchboard's durable agent mailbox into prismattyc-mux as a built-in
subsystem. prismattyc-mux sessions become the source of truth for agent presence,
eliminating the standalone daemon, the seat table, presence leases, the
death-watch reaper, and the doorbell shell loop.

## Motivation

Switchboard and prismattyc-mux both track "is this agent alive?" and "what
process backs it?" independently. The doorbell — a shell script bridging
the two — is the fragile seam between them. Today's architecture:

```
┌─────────────┐       ┌───────────────────┐
│ switchboardd│       │   prismattyc-mux       │
│             │       │                   │
│ seat table  │◄─────►│ session table     │
│ mail store  │       │ pane children     │
│ reaper      │       │ mail attention    │
│ presence    │       │                   │
└──────┬──────┘       └────────┬──────────┘
       │                       │
       │    doorbell.sh        │
       │  (watch + ring loop)  │
       └───────────────────────┘
```

Problems:
- Two daemons tracking overlapping state (process liveness, identity)
- Doorbell is a shell loop polling one system and shelling into another
- Socket path mismatches between launchd services (XDG_RUNTIME_DIR bug)
- Mail arrival → pane notification has 3 hops (store → watch wake →
  shell script → prismattyc-mux mail command) instead of 1

## Design

### Core change

The mux session IS the mailbox seat. No separate seat table, no
generation counters, no presence leases.

```
Session exists  →  agent addressable  →  mail delivered + attention armed
Session gone    →  mail queued in store (delivered on session recreate)
```

### Architecture after absorption

```
┌─────────────────────────────────────────┐
│              prismattyc-mux                  │
│                                         │
│  ┌────────────┐  ┌──────────────────┐  │
│  │ sessions   │  │ mailbox module   │  │
│  │            │  │                  │  │
│  │ agent_id   │──│ SQLite store     │  │
│  │ panes      │  │ claim/commit     │  │
│  │ children   │  │ wait (Notify)    │  │
│  │ attention  │◄─│ in-process ring  │  │
│  └────────────┘  └──────────────────┘  │
│                                         │
│  ┌────────────────────────────────────┐ │
│  │ mux socket protocol               │ │
│  │ (existing messages + Mail* verbs)  │ │
│  └────────────────────────────────────┘ │
└─────────────────────────────────────────┘
```

### Session metadata addition

```rust
struct Session {
    name: String,
    id: u32,
    agent_id: Option<String>,  // NEW — defaults to session name
    // ... existing fields
}
```

When `agent_id` is `Some`, the session is addressable as a mailbox
recipient. When `None`, the session has no mailbox (e.g. the `default`
session). This keeps backward compatibility.

### Mailbox protocol messages

New message types on the existing mux Unix socket:

```rust
// Client → Server
MailSend    { from: String, to: String, summary: String, body: String }
MailClaim   { agent: String }
MailCommit  { agent: String, ids: Vec<String> }
MailRelease { agent: String, ids: Vec<String> }
MailInbox   { agent: String }
MailWait    { agent: String, timeout_ms: u32 }
MailWho
MailBroadcast { from: String, summary: String, body: String }

// Server → Client
MailSent        { id: String, depth: u32 }
MailLetters     { letters: Vec<Envelope> }
MailCommitted   { committed: u32 }
MailReleased    { released: u32 }
MailDepth       { open: u32, held: u32 }
MailPeers       { peers: Vec<PeerInfo> }
MailBroadcasted { delivered: u32, recipients: Vec<String> }
```

These are namespaced under `Mail*` to coexist with existing mux
protocol messages without collision.

### In-process doorbell

The critical improvement. When a letter is stored:

```rust
fn on_mail_stored(&self, recipient_agent: &str) {
    if let Some(session) = self.session_by_agent(recipient_agent) {
        if let Some(pane) = session.active_pane() {
            // Direct in-process call — no shell, no external watcher
            pane.arm_mail_attention("switchboard", depth);
            pane.inject_doorbell_token(); // SWITCHBOARD_MAIL + submit bytes
        }
    }
}
```

Mail arrival → pane notification becomes a single synchronous call
within the server. No watcher service, no polling, no shell script.

### Mail store

SQLite, same proven schema:

```sql
CREATE TABLE letters (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    recipient TEXT NOT NULL,
    from_agent TEXT NOT NULL,
    summary  TEXT NOT NULL,
    body     TEXT NOT NULL DEFAULT '',
    state    TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open', 'held'))
);
CREATE INDEX idx_letters_recipient ON letters(recipient);
```

Location: `$XDG_DATA_HOME/prismattyc-mux/mail.db` (or alongside existing
mux state).

The `seats` table is eliminated — session persistence replaces it.

### Wait mechanic (sync, not async)

prismattyc-mux is synchronous (threads + channels), NOT tokio-based. The
wait mechanic stays as a condvar — the same pattern Switchboard already
uses. No async conversion needed.

```rust
struct MailboxWatch {
    inner: Mutex<()>,
    cvar: Condvar,
}

impl MailboxWatch {
    fn ring(&self) {
        let _lock = self.inner.lock().unwrap();
        self.cvar.notify_all();
    }

    fn wait_until(&self, agent: &str, timeout: Duration, store: &Store) -> Depth {
        let deadline = Instant::now() + timeout;
        let mut lock = self.inner.lock().unwrap();
        loop {
            let depth = store.depth(agent);
            if depth.open > 0 { return depth; }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return depth; }
            let (new_lock, _) = self.cvar.wait_timeout(lock, remaining).unwrap();
            lock = new_lock;
        }
    }
}
```

This is a direct port of Switchboard's existing `watch.rs` — no
paradigm shift required.

### Presence model

| Concept | Old (Switchboard) | New (prismattyc-mux) |
|---------|-------------------|-----------------|
| Agent identity | AgentId in seat table | `session.agent_id` |
| Addressable? | Seat exists + within lease | Session exists + has agent_id |
| Liveness signal | Death-watch reaper (pidfd/kqueue) | Mux child-death detection (existing) |
| Reconnection | Generation bump on lease expiry | N/A — sessions persist, no reconnection concept |
| Presence lease | 300s timer after process exit | Session lifetime (survives child exit) |
| Who | Query seat table | Query sessions with agent_id set |

The generation counter and presence lease are eliminated entirely.
Sessions don't "reconnect" — they persist across client
attach/detach cycles. The MCP adapter connects per tool call, but
presence is session-exists, not connection-exists.

### Headless agents

Agents without a terminal pane (CI bots, background workers):

```bash
prismattyc-mux new --headless worker-bot
```

Creates a session with a mailbox but no pane. The agent polls
`MailInbox` / `MailClaim` on the mux socket. No doorbell rings
(no pane to inject into). The session persists until explicitly
removed.

Implementation: a session with `panes: vec![]` and
`agent_id: Some("worker-bot")`. The mailbox module checks for a
live pane before attempting doorbell injection.

### CLI surface

Keep the `switchboard` binary as the user-facing CLI. Change its
transport from the switchboardd socket to the prismattyc-mux socket:

```rust
// Before (switchboard-proto/client.rs)
fn exchange(agent: &AgentId, op: &ClientMessage) -> Result<DaemonMessage, ExchangeError> {
    exchange_at(&socket_path(), agent, op)  // switchboardd socket
}

// After
fn exchange(agent: &AgentId, op: &ClientMessage) -> Result<DaemonMessage, ExchangeError> {
    exchange_at(&mux_socket_path(), agent, op)  // prismattyc-mux socket
}
```

The CLI keeps its existing UX (`switchboard send`, `switchboard claim`,
etc.). Users and scripts don't change. Only the transport target moves.

The `switchboard status` verb would resolve and check the mux socket
instead.

### MCP adapter

The MCP adapter (`switchboard-mcp`) retargets the mux socket. The
`Switchboard` struct changes its socket path:

```rust
// Before
let server = Switchboard::new(config.agent, socket_path());

// After
let server = Switchboard::new(config.agent, mux_socket_path());
```

The tool surface (9 tools) and their semantics are unchanged. The
adapter still does handshake-then-drop per tool call — now against
the mux socket's Mail* messages instead of the switchboardd protocol.

Alternative: the MCP adapter could become a built-in prismattyc-mux
capability (mux serves MCP directly). This is a larger change and
not required for the initial absorption.

## What gets eliminated

| Component | Status |
|-----------|--------|
| `switchboardd` binary | Removed |
| `crates/switchboardd/` | Removed (logic moves to prismattyc-mux mailbox module) |
| `crates/switchboard-core/src/seat.rs` | Removed (session replaces seat) |
| `crates/switchboardd/src/seat_table.rs` | Removed |
| `crates/switchboardd/src/death_watch.rs` | Removed (mux has its own) |
| `crates/switchboardd/src/reaper.rs` | Removed |
| `crates/switchboardd/src/watch_presence.rs` | Removed or simplified |
| `contrib/switchboard-doorbell.sh` | Removed |
| `contrib/switchboard-watch@.service` | Removed |
| `contrib/com.switchboard.watch@.plist` | Removed |
| `~/.config/switchboard/doorbell.map` | Removed |
| Separate launchd watcher services | Removed |
| Presence lease timer | Removed |
| Generation counters | Removed |

## What moves into prismattyc-mux

| Component | Source | Destination |
|-----------|--------|-------------|
| Mail store (SQLite schema) | `switchboard-store` | New `prismattyc-mux` mailbox module |
| Two-phase claim/commit | `switchboard-store` | Mailbox module |
| Letter delivery + broadcast | `switchboardd/serve.rs` | Mailbox module |
| Wait/notify | `switchboardd/watch.rs` | Async Notify in mailbox module |
| Agent addressing | `switchboard-core/agent.rs` | Session metadata |
| Envelope type | `switchboard-core/envelope.rs` | Kept (shared type) |

## What stays as-is (re-pointed)

| Component | Change |
|-----------|--------|
| `switchboard` CLI binary | Targets mux socket instead of switchboardd socket |
| `switchboard-mcp` adapter | Targets mux socket instead of switchboardd socket |
| `switchboard-proto` crate | Wire types kept; `socket_path()` returns mux socket |
| MCP tool surface (9 tools) | Unchanged semantics |

## Migration plan

### Phase 1: Foundation (prismattyc-mux changes)

1. Add `agent_id: Option<String>` to session metadata
2. Implement mailbox module (SQLite store, claim/commit/release)
3. Add `Mail*` message types to mux protocol
4. Wire in-process doorbell (mail stored → attention armed → token injected)
5. Expose `MailWho` as session-based presence query

### Phase 1b: Dogfood

6. End-to-end test: send via mux socket, verify attention lights +
   inject fires without the external watcher. The external doorbell
   (switchboard-doorbell.sh + launchd) becomes dead the moment this works.

### Phase 2: Client migration

7. Update `switchboard-proto::socket_path()` to resolve the mux socket
8. Adapt the `switchboard` CLI protocol to Mail* messages
9. Update MCP adapter to target mux socket
10. Add `prismattyc-mux new --headless` for pane-less agents

### Phase 3: Decommission

11. Remove switchboardd binary and crate
12. Remove doorbell script, watcher services, doorbell.map
13. Remove seat table, reaper, death-watch, presence lease code
14. Update documentation

### Data migration

Existing letters in `$XDG_DATA_HOME/switchboard/mail.db` need a
one-time migration to the new store location. A migration script
(or first-run detection in prismattyc-mux) copies the `letters` table.
Seat affinity data is discarded (sessions replace it).

## Risks and mitigations

| Risk | Mitigation |
|------|-----------|
| prismattyc-mux restart loses in-flight state | SQLite store persists letters; sessions restore on restart |
| Mux socket unavailable blocks all mail | Same single-point-of-failure as today (switchboardd down = no mail). No regression. |
| Breaking change for CLI/MCP users | Wire types stay compatible; only transport target changes. Feature-flag or env var for transition period. |
| Headless agents lose standalone operation | `--headless` sessions provide equivalent capability |
| Increased mux complexity | Mailbox module is well-bounded (~300 lines of store + ~100 lines of protocol dispatch). Minimal coupling to existing mux internals. Same threading model (sync + condvar) — no paradigm mismatch. |

## Success criteria

- `switchboard send/claim/commit/who` work unchanged from user perspective
- Mail arrival → pane notification in <10ms (vs current ~15s poll + shell overhead)
- Zero external watcher services running
- Single daemon (prismattyc-mux) instead of three processes (daemon + 2 watchers)
- All existing MCP tools pass integration tests unchanged
