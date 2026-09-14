# Tasks: Fold Switchboard into Prism

## Phase 1: Foundation (prismattyc-mux changes)

### Task 1.1: Add agent_id to session metadata
- **Depends on:** none
- **Crate:** prismattyc-mux (Prism repo)
- **Changes:**
  - Add `agent_id: Option<String>` field to `Session` struct
  - Default to `Some(session.name.clone())` on session creation
  - Add `--no-agent` flag to `prismattyc-mux new` to opt out (agent_id = None)
  - Add `--agent <name>` flag to override the default
  - Persist agent_id in session state (survives restart)
  - Add `session_by_agent(&str) -> Option<&Session>` lookup to ControlPlane
- **Tests:** session creation sets agent_id; --no-agent sets None; lookup resolves correctly

### Task 1.2: Implement mailbox store module
- **Depends on:** none (can parallel with 1.1)
- **Crate:** prismattyc-mux (Prism repo) — new module `src/mailbox/store.rs`
- **Changes:**
  - Create SQLite database at `$XDG_DATA_HOME/prismattyc-mux/mail.db`
  - Schema: `letters` table (seq, recipient, from_agent, summary, body, state)
  - Implement: `send()`, `broadcast()`, `claim()`, `commit()`, `release()`, `depth()`
  - Letter id format: `msg:{seq:016x}` (same as current Switchboard)
  - Open database on server startup; handle first-run schema creation
- **Tests:** port existing `switchboard-store` unit tests (11 tests)

### Task 1.3: Implement MailboxWatch (wait/notify)
- **Depends on:** 1.2
- **Crate:** prismattyc-mux — new module `src/mailbox/watch.rs`
- **Changes:**
  - `MailboxWatch` struct with `Mutex<()>` + `Condvar`
  - `ring()` method: notify_all
  - `wait_until(agent, timeout, store) -> Depth`: loop with condvar wait_timeout
  - Lock ordering documented: watch → store
  - Call `ring()` after every `send()` and `broadcast()` that changes depth
- **Tests:** wake on ring; timeout returns depth; spurious wake re-checks

### Task 1.4: Add Mail* protocol messages to mux socket
- **Depends on:** 1.1, 1.2, 1.3
- **Crate:** prismattyc-mux — extend existing protocol module
- **Changes:**
  - Define `MailClientMessage` and `MailServerMessage` enums (or add variants to existing enums)
  - Add serde serialization with `"type": "mail_send"` etc. discriminators
  - Dispatch Mail* messages in the connection handler to mailbox module
  - MailWait blocks the connection thread (same as Switchboard's wait)
  - MailWho queries sessions with agent_id set
  - MailAlias binds to session (stored alongside session metadata)
- **Tests:** wire format roundtrip; dispatch smoke test

### Task 1.5: Wire in-process doorbell
- **Depends on:** 1.1, 1.2, 1.4
- **Crate:** prismattyc-mux — `src/mailbox/doorbell.rs`
- **Changes:**
  - After `send()` stores a letter, call `on_mail_stored(recipient, depth)`
  - Resolve recipient → session → active pane
  - Call existing `arm_mail_attention("switchboard", depth, gen)` on the pane
  - Call existing doorbell injection (SWITCHBOARD_MAIL + submit bytes)
  - If no session or no pane: no-op (mail queues silently)
  - After `broadcast()`: call `on_mail_stored()` for each recipient
- **Tests:** send triggers attention on target pane; headless skips injection

### Task 1.6: Add headless session support
- **Depends on:** 1.1
- **Crate:** prismattyc-mux
- **Changes:**
  - `prismattyc-mux new --headless <name>` creates session with no pane
  - Session has `agent_id: Some(name)`, `panes: vec![]`
  - Session appears in MailWho (addressable)
  - Doorbell injection is skipped (no pane)
  - Session persists until `prismattyc-mux stop <name>`
- **Tests:** headless session addressable; mail queues; no injection crash

---

## Phase 1b: Dogfood

### Task 1b.1: End-to-end integration test
- **Depends on:** 1.5
- **Changes:**
  - Start prismattyc-mux with two sessions (kiro-sb, kiro-pm)
  - Send a letter from kiro-sb to kiro-pm via the mux socket Mail* protocol
  - Verify: letter stored, attention armed on kiro-pm's pane, SWITCHBOARD_MAIL injected
  - Claim from kiro-pm, verify letter content
  - Commit, verify depth is 0
  - Verify attention can be cleared
- **Acceptance:** Full loop works without any external watcher process

### Task 1b.2: Restart durability test
- **Depends on:** 1b.1
- **Changes:**
  - Send letters to an agent
  - Restart prismattyc-mux
  - Claim from the agent
  - Verify all letters recovered
- **Acceptance:** No data loss across restart

### Task 1b.3: Latency comparison
- **Depends on:** 1b.1
- **Changes:**
  - Measure time from send to SWITCHBOARD_MAIL injection (in-process path)
  - Compare to current external doorbell path (~15s worst case)
- **Acceptance:** <10ms end-to-end for in-process path

---

## Phase 2: Client Migration

### Task 2.1: Update switchboard-proto socket_path()
- **Depends on:** Phase 1 complete
- **Crate:** switchboard-proto (this repo)
- **Changes:**
  - `socket_path()` returns the prismattyc-mux socket path instead of switchboardd path
  - Support `SWITCHBOARD_SOCKET` env var for override during transition
  - Fallback: try mux socket, then legacy switchboardd socket (transition period)
- **Tests:** resolves mux socket; env override works; fallback logic

### Task 2.2: Adapt CLI protocol to Mail* messages
- **Depends on:** 2.1
- **Crate:** switchboard-proto, switchboard-cli (this repo)
- **Changes:**
  - `exchange()` sends Mail* messages instead of Hello+op sequence
  - No Hello/Seated handshake needed on mux socket (messages are self-identifying)
  - Map existing `ClientMessage` variants to `MailClientMessage` wire format
  - Map `MailServerMessage` responses back to existing `DaemonMessage` types
  - Or: refactor to use Mail* types directly throughout
- **Tests:** all 15 existing CLI unit tests pass; integration test against mux

### Task 2.3: Update MCP adapter transport
- **Depends on:** 2.1
- **Crate:** switchboard-mcp (this repo)
- **Changes:**
  - `Switchboard::new(agent, mux_socket_path())` — targets mux socket
  - `run_op()` uses updated exchange() from switchboard-proto
  - All 9 tools unchanged in semantics
  - Supervisor mode: health check targets mux socket
- **Tests:** MCP tool smoke tests against mux

### Task 2.4: Add prismattyc-mux new --headless to CLI
- **Depends on:** 1.6
- **Changes:**
  - Ensure `prismattyc-mux new --headless` is available from the CLI
  - Document usage for CI bots and background workers
- **Tests:** create headless session, send mail, claim mail

### Task 2.5: Migration script for existing mail.db
- **Depends on:** 1.2
- **Changes:**
  - Script to copy letters from `$XDG_DATA_HOME/switchboard/mail.db`
    to `$XDG_DATA_HOME/prismattyc-mux/mail.db`
  - Handles: letters table copy, id preservation
  - Discards: seats table (no longer needed)
  - First-run detection: if prismattyc-mux mail.db doesn't exist and
    switchboard mail.db does, offer migration
- **Tests:** letters survive migration; ids stable

---

## Phase 3: Decommission

### Task 3.1: Remove switchboardd daemon
- **Depends on:** Phase 2 complete + dogfood confirmed
- **Changes:**
  - Remove `crates/switchboardd/` directory
  - Remove switchboardd from workspace Cargo.toml
  - Remove `contrib/switchboardd.service` (systemd)
  - Unload and remove launchd daemon plist (`com.switchboard.daemon`)
  - Remove switchboardd binary from `~/.cargo/bin/`

### Task 3.2: Remove doorbell infrastructure
- **Depends on:** 3.1
- **Changes:**
  - Remove `contrib/switchboard-doorbell.sh`
  - Remove `contrib/switchboard-watch@.service`
  - Remove `contrib/com.switchboard.watch@.plist`
  - Unload and remove launchd watcher plists
  - Remove `~/.config/switchboard/doorbell.map`
  - Remove `~/.local/bin/switchboard-doorbell`
  - Remove `~/.local/state/switchboard/` (log dir)

### Task 3.3: Remove eliminated code
- **Depends on:** 3.1
- **Changes:**
  - Remove `crates/switchboard-core/src/seat.rs` (SeatId type)
  - Remove seat-related code from `switchboard-core/lib.rs`
  - Remove `crates/switchboard-store/` (replaced by prismattyc-mux mailbox module)
  - Clean up `switchboard-proto` — remove Hello/Seated handshake (if Mail* is direct)
  - Remove `crates/switchboard-broker/` if unused
  - Update workspace Cargo.toml members list

### Task 3.4: Update documentation
- **Depends on:** 3.1, 3.2, 3.3
- **Changes:**
  - Update `README.md` — reflect that mailbox is built into prismattyc-mux
  - Update `docs/doorbell.md` — mark as historical or remove
  - Update `docs/fold-switchboard-into-prism.md` — mark as completed
  - Update switchboard_tutorial MCP tool text
  - Update any vault memories referencing switchboardd setup

### Task 3.5: Remove legacy socket and data paths
- **Depends on:** 3.1, migration confirmed
- **Changes:**
  - Remove stale socket at old switchboardd path
  - Archive or remove `$XDG_DATA_HOME/switchboard/mail.db` (after migration verified)
  - Remove `$XDG_RUNTIME_DIR/switchboard/` directory
  - Clean up any remaining switchboard-specific env vars from launchd plists

---

## Dependency graph

```
Phase 1 (parallel starts):
  1.1 ─────┐
  1.2 ──┐  │
        │  │
  1.3 ──┤  │
        │  │
        ▼  ▼
  1.4 (needs 1.1 + 1.2 + 1.3)
        │
        ▼
  1.5 (needs 1.1 + 1.2 + 1.4)
        │
  1.6 (needs 1.1 only, can parallel with 1.2-1.5)

Phase 1b (sequential):
  1b.1 → 1b.2 → 1b.3

Phase 2 (after Phase 1):
  2.1 → 2.2 → 2.3
  2.4 (parallel, needs 1.6)
  2.5 (parallel, needs 1.2)

Phase 3 (after Phase 2 dogfood):
  3.1 → 3.2, 3.3 → 3.4, 3.5
```

---

## Estimated effort

| Phase | Tasks | Estimate |
|-------|-------|----------|
| Phase 1 | 6 tasks | 1–2 weeks |
| Phase 1b | 3 tasks | 1–2 days |
| Phase 2 | 5 tasks | 1 week |
| Phase 3 | 5 tasks | 2–3 days |
| **Total** | **19 tasks** | **3–4 weeks** |

Note: Phase 1 work happens in the Prism repo. Phases 2–3 happen in this
repo (Switchboard). Phase 1b bridges both.
