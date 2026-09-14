# Requirements: Fold Switchboard into Prism

## Overview

Absorb Switchboard's durable agent mailbox into prismattyc-mux as a built-in
subsystem, eliminating the standalone daemon and external doorbell watcher
while preserving all existing user-facing behavior.

## Stakeholders

- **Agent developers** — use `switchboard` CLI and MCP tools to send/receive mail
- **Operators** — manage daemon services, troubleshoot delivery
- **Agents** — automated consumers of the mailbox (kiro-sb, kiro-pm, etc.)

---

## Functional Requirements

### FR-1: Mail Delivery

WHEN a client sends a letter to an agent name
THE SYSTEM SHALL resolve the agent name to a prismattyc-mux session and store the letter in the mailbox

WHEN a client sends a letter to an agent whose session exists
THE SYSTEM SHALL store the letter with state "open" and return the letter id and mailbox depth

WHEN a client sends a letter to an agent whose session does not exist
THE SYSTEM SHALL store the letter with state "open" for future delivery when the session is created

WHEN a letter is stored for an agent with a live pane
THE SYSTEM SHALL arm mail attention on the pane and inject the PMUX_MAIL token within 10ms

### FR-2: Mail Claiming

WHEN an agent issues a claim
THE SYSTEM SHALL transition all open letters for that agent to "held" and return them

WHEN an agent issues a claim with held letters from a previous claim
THE SYSTEM SHALL return the held letters (crash recovery)

### FR-3: Mail Commit and Release

WHEN an agent commits letter ids
THE SYSTEM SHALL permanently delete those held letters

WHEN an agent releases letter ids
THE SYSTEM SHALL transition those held letters back to "open"

### FR-4: Inbox and Wait

WHEN an agent queries its inbox
THE SYSTEM SHALL return the count of open and held letters

WHEN an agent issues a wait
THE SYSTEM SHALL block until open mail arrives or the timeout elapses

WHEN mail arrives for a waiting agent
THE SYSTEM SHALL wake the waiter immediately

### FR-5: Broadcast

WHEN an agent broadcasts a message
THE SYSTEM SHALL deliver one letter to every session with an agent_id, except the sender

### FR-6: Presence (Who)

WHEN a client queries presence
THE SYSTEM SHALL return all sessions that have an agent_id set, with their session name and aliases

WHEN a session is created with an agent_id
THE SYSTEM SHALL make that agent addressable immediately

WHEN a session is destroyed
THE SYSTEM SHALL remove the agent from the presence list

### FR-7: Aliases

WHEN an agent registers an alias
THE SYSTEM SHALL bind that alias to the agent's session

WHEN a client sends to an alias
THE SYSTEM SHALL resolve it to the owning agent and deliver

### FR-8: Headless Agents

WHEN a session is created with --headless and an agent_id
THE SYSTEM SHALL make the agent addressable without a pane

WHEN mail arrives for a headless agent
THE SYSTEM SHALL store the letter but NOT attempt doorbell injection

### FR-9: In-Process Doorbell

WHEN a letter is stored for an agent with a live pane
THE SYSTEM SHALL arm sticky mail attention on the pane's "mail" cell

WHEN the attention is armed
THE SYSTEM SHALL inject the fixed token PMUX_MAIL plus the session's submit bytes into the pane

WHEN the agent claims all mail (depth reaches 0)
THE SYSTEM SHALL allow the agent to clear attention via the existing mail_attention_set protocol

### FR-10: Session Agent Identity

WHEN a session is created with a name
THE SYSTEM SHALL default agent_id to the session name unless overridden

WHEN a session has agent_id set to None
THE SYSTEM SHALL exclude it from mailbox addressing and presence

---

## Non-Functional Requirements

### NFR-1: Performance

THE SYSTEM SHALL deliver mail and arm pane attention in under 10ms end-to-end (in-process path)

THE SYSTEM SHALL support at least 100 concurrent agent sessions without degradation

### NFR-2: Durability

THE SYSTEM SHALL persist all letters in SQLite, surviving daemon restarts

THE SYSTEM SHALL persist session agent_id mappings, surviving daemon restarts

WHEN prismattyc-mux restarts
THE SYSTEM SHALL restore all undelivered mail and make it available on the next claim

### NFR-3: Backward Compatibility

THE SYSTEM SHALL accept the same CLI commands (`switchboard send/claim/commit/release/inbox/watch/who/broadcast/alias`)

THE SYSTEM SHALL expose the same 9 MCP tools with identical semantics

THE SYSTEM SHALL NOT require changes to agent-side mail handling logic (claim → act → commit)

### NFR-4: Operational Simplicity

THE SYSTEM SHALL run as a single daemon (prismattyc-mux) instead of three processes (daemon + 2 watchers)

THE SYSTEM SHALL NOT require external watcher services, doorbell.map, or shell scripts for mail notification

### NFR-5: Reliability

WHEN the daemon crashes after a claim but before commit
THE SYSTEM SHALL preserve held letters for re-claim on restart

WHEN a headless agent's polling connection fails
THE SYSTEM SHALL queue mail silently until the next successful connection

---

## Out of Scope

- Multi-host / networked mailbox (single-machine only)
- Authentication or encryption on the mux socket (same trust model as today)
- Mail expiry or TTL (letters persist indefinitely)
- Priority or ordering guarantees beyond FIFO per recipient
- Serving MCP directly from prismattyc-mux (adapter remains a separate process)
