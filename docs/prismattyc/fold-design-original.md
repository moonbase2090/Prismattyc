# Design: Fold Switchboard into Prism

## Overview

Absorb Switchboard's durable agent mailbox into prismattyc-mux. The mux session
becomes the source of truth for agent presence. The mailbox, doorbell, and
presence tracking become internal modules within the prismattyc-mux server.

#[[file:docs/fold-switchboard-into-prism.md]]

---

## Architecture

### Current state (before)

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

### Target state (after)

```
┌─────────────────────────────────────────┐
│              prismattyc-mux                  │
│                                         │
│  ┌────────────┐  ┌──────────────────┐  │
│  │ sessions   │  │ mailbox module   │  │
│  │            │  │                  │  │
│  │ agent_id   │──│ SQLite store     │  │
│  │ panes      │  │ claim/commit     │  │
│  │ children   │  │ wait (Condvar)   │  │
│  │ attention  │◄─│ in-process ring  │  │
│  └────────────┘  └──────────────────┘  │
│                                         │
│  ┌────────────────────────────────────┐ │
│  │ mux socket protocol               │ │
│  │ (existing messages + Mail* verbs)  │ │
│  └────────────────────────────────────┘ │
└─────────────────────────────────────────┘
```

---

## Components

### 1. Session metadata extension

```rust
struct Session {
    name: String,
    id: u32,
    agent_id: Option<String>,  // NEW — defaults to session name
    // ... existing fields (windows, panes, etc.)
}
```

- `agent_id: Some(name)` → session is addressable as a mailbox recipient
- `agent_id: None` → session excluded from mailbox (e.g. the `default` session)
- Default: `agent_id = Some(session.name)` unless overridden or opted out

### 2. Mailbox store module

SQLite database at `$XDG_DATA_HOME/prismattyc-mux/mail.db`:

```sql
CREATE TABLE letters (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    recipient TEXT NOT NULL,
    from_agent TEXT NOT NULL,
    summary   TEXT NOT NULL,
    body      TEXT NOT NULL DEFAULT '',
    state     TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open', 'held'))
);
CREATE INDEX idx_letters_recipient ON letters(recipient);
```

Operations (direct port from `switchboard-store`):
- `send(recipient, from, summary, body) → (id, depth)`
- `broadcast(from, recipients, summary, body) → (delivered, recipient_list)`
- `claim(agent) → Vec<Envelope>`
- `commit(agent, ids) → committed_count`
- `release(agent, ids) → released_count`
- `depth(agent) → (open, held)`

The `seats` table is eliminated — session persistence replaces it.

### 3. Mailbox protocol messages

New message types on the existing mux Unix socket (NDJSON framing):

```rust
// Client → Server
enum MailClientMessage {
    MailSend      { from: String, to: String, summary: String, body: String },
    MailClaim     { agent: String },
    MailCommit    { agent: String, ids: Vec<String> },
    MailRelease   { agent: String, ids: Vec<String> },
    MailInbox     { agent: String },
    MailWait      { agent: String, timeout_ms: u32 },
    MailWho,
    MailAlias     { agent: String, name: String },
    MailBroadcast { from: String, summary: String, body: String },
}

// Server → Client
enum MailServerMessage {
    MailSent        { id: String, depth: u32 },
    MailLetters     { letters: Vec<Envelope> },
    MailCommitted   { committed: u32 },
    MailReleased    { released: u32 },
    MailDepth       { open: u32, held: u32 },
    MailPeers       { peers: Vec<PeerInfo> },
    MailAliased     { name: String, agent: String },
    MailBroadcasted { delivered: u32, recipients: Vec<String> },
    MailRefused     { reason: String },
}
```

These coexist with existing mux protocol messages on the same socket,
distinguished by the `type` field in NDJSON.

### 4. In-process doorbell

When a letter is stored, the server triggers notification directly:

```rust
impl MailboxModule {
    fn on_mail_stored(&self, recipient: &str, depth: u32) {
        // Resolve agent → session
        let Some(session) = self.control_plane.session_by_agent(recipient) else {
            return; // headless or unknown — mail queues silently
        };

        // Find active pane
        let Some(pane) = session.active_pane() else {
            return; // no live pane — mail queues
        };

        // Arm attention + inject doorbell token (in-process, <1ms)
        pane.arm_mail_attention("switchboard", depth);
        pane.inject_doorbell_token(); // SWITCHBOARD_MAIL + submit bytes
    }
}
```

No external watcher, no shell script, no polling.

### 5. Wait mechanic

Synchronous condvar (prismattyc-mux is thread-based, not async):

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

Lock ordering: watch → store (same as current Switchboard).

### 6. Headless sessions

```bash
prismattyc-mux new --headless worker-bot
```

Creates a session with `agent_id: Some("worker-bot")`, `panes: vec![]`.
The agent is addressable, mail queues, but no doorbell injection occurs.
The session persists until explicitly removed.

---

## Presence model

| Concept | Old (Switchboard) | New (prismattyc-mux) |
|---------|-------------------|-----------------|
| Agent identity | AgentId in seat table | `session.agent_id` |
| Addressable? | Seat exists + within lease | Session exists + has agent_id |
| Liveness signal | Death-watch reaper (pidfd/kqueue) | Mux child-death (existing) |
| Reconnection | Generation bump on lease expiry | N/A — sessions persist |
| Presence lease | 300s timer after process exit | Session lifetime |
| Who | Query seat table | Query sessions with agent_id |

---

## Client transport changes

### CLI (`switchboard` binary)

The binary remains as-is from a UX perspective. Only the socket target
changes:

```rust
// switchboard-proto/paths.rs
pub fn socket_path() -> PathBuf {
    // Before: XDG_RUNTIME_DIR/switchboard/control.sock
    // After:  XDG_RUNTIME_DIR/prismattyc-mux/prism-default.sock (or configured instance)
    mux_socket_path()
}
```

The handshake changes slightly: instead of a Switchboard Hello/Seated
exchange, the CLI sends Mail* messages directly on the mux protocol
(which doesn't require a Hello — the mux socket is already multiplexed).

### MCP adapter (`switchboard-mcp`)

Same change — retargets the mux socket:

```rust
let server = Switchboard::new(config.agent, mux_socket_path());
```

All 9 tools keep identical semantics. The adapter is still a separate
process connecting per tool call.

---

## Data flow: send end-to-end

```
Agent A (kiro-sb)                  prismattyc-mux                    Agent B (kiro-pm)
      │                                │                              │
      │  MailSend{to:"kiro-pm",...}    │                              │
      │───────────────────────────────►│                              │
      │                                │  1. store letter (SQLite)    │
      │                                │  2. resolve "kiro-pm" session│
      │                                │  3. arm attention pane 2     │
      │                                │  4. inject SWITCHBOARD_MAIL  │
      │                                │──────────────────────────────►
      │  MailSent{id, depth}           │                              │
      │◄───────────────────────────────│                              │
      │                                │         MailClaim{agent}     │
      │                                │◄─────────────────────────────│
      │                                │  5. held ← open letters      │
      │                                │         MailLetters{...}     │
      │                                │──────────────────────────────►
      │                                │         MailCommit{ids}      │
      │                                │◄─────────────────────────────│
      │                                │  6. delete letters           │
      │                                │         MailCommitted{n}     │
      │                                │──────────────────────────────►
```

---

## What gets eliminated

| Component | Reason |
|-----------|--------|
| `switchboardd` binary | Mailbox lives in prismattyc-mux |
| `crates/switchboardd/` | Logic moves to prismattyc-mux mailbox module |
| `crates/switchboard-core/src/seat.rs` | Session replaces seat |
| `switchboardd/src/seat_table.rs` | Session persistence replaces it |
| `switchboardd/src/death_watch.rs` | Mux has its own child watching |
| `switchboardd/src/reaper.rs` | No separate reaper needed |
| `switchboardd/src/watch_presence.rs` | No external watcher to detect |
| `contrib/switchboard-doorbell.sh` | In-process doorbell replaces it |
| `contrib/switchboard-watch@.service` | No external watcher |
| `contrib/com.switchboard.watch@.plist` | No external watcher |
| `~/.config/switchboard/doorbell.map` | Agent = session, resolved internally |
| Separate launchd watcher services | No external watcher |
| Presence lease timers | Sessions persist natively |
| Generation counters | No reconnection concept |

## What moves into prismattyc-mux

| Component | Source | Destination |
|-----------|--------|-------------|
| Mail store (SQLite) | `switchboard-store` | Mailbox module |
| Two-phase claim/commit | `switchboard-store` | Mailbox module |
| Letter delivery + broadcast | `switchboardd/serve.rs` | Mailbox dispatch |
| Wait/notify | `switchboardd/watch.rs` | MailboxWatch (Condvar) |
| Agent addressing | `switchboard-core/agent.rs` | Session metadata |
| Envelope type | `switchboard-core/envelope.rs` | Shared crate or inline |
| Alias registry | `switchboardd/seat_table.rs` | Session-level alias map |

## What stays (re-pointed)

| Component | Change |
|-----------|--------|
| `switchboard` CLI | Targets mux socket |
| `switchboard-mcp` adapter | Targets mux socket |
| `switchboard-proto` crate | Wire types kept; socket_path() returns mux path |
| 9 MCP tools | Unchanged semantics |

---

## Error handling

| Scenario | Behavior |
|----------|----------|
| Send to unknown agent (no session) | Store letter; deliver when session created |
| Send to headless agent | Store letter; no doorbell |
| Claim with no mail | Return empty letters list |
| Commit unknown ids | Return committed: 0 |
| Wait timeout | Return current depth with open: 0 |
| Mux restart mid-wait | Client gets connection reset; reconnects and re-waits |
| Mux restart with held letters | Letters survive in SQLite; next claim re-surfaces them |

---

## Testing strategy

### Unit tests

- Mailbox store: send/claim/commit/release/depth CRUD operations
- Presence: session create/destroy updates agent addressability
- Wait: condvar wake on mail arrival, timeout behavior
- Alias: bind/resolve/unbind
- Broadcast: fan-out to all sessions with agent_id except sender

### Integration tests

- End-to-end: CLI send → store → doorbell fires → CLI claim → commit
- Restart durability: send, restart mux, claim recovers letters
- Headless: send to headless agent, no doorbell, claim works
- Concurrent: multiple agents sending/claiming simultaneously

### Dogfood test (Phase 1b)

- Live test with kiro-sb and kiro-pm sessions
- Send via mux socket, verify attention badge lights up
- Verify SWITCHBOARD_MAIL injection into pane
- Verify claim/commit clears the badge
- Compare latency to external doorbell path

---

## Risks and mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Mux restart loses state | Mail lost | SQLite persists all letters; restore on restart |
| Single point of failure | All mail blocked | Same as today (switchboardd down = no mail) |
| Breaking CLI/MCP users | Agents fail | Wire types compatible; env var for transition |
| Headless agents orphaned | Background workers fail | `--headless` sessions provide equivalent |
| Mux complexity growth | Maintenance burden | Mailbox module well-bounded (~400 LOC); same threading model |
| Migration data loss | Letters lost | One-time SQLite copy; verify before decommission |

---

## Success criteria

- [ ] `switchboard send/claim/commit/who` work unchanged from user perspective
- [ ] Mail arrival → pane notification in <10ms (vs ~15s current)
- [ ] Zero external watcher services running
- [ ] Single daemon (prismattyc-mux) instead of three processes
- [ ] All 9 MCP tools pass integration tests unchanged
- [ ] Held letters survive mux restart
- [ ] Headless agents can send/receive without a pane
