# Seat attention and the needs-you inbox on a phone

Status: design study. It maps what the daemon already knows about seats onto a small-screen companion and proposes a first slice. Nothing here is implemented by this change. The companion's catalog, terminal view and transport are separate studies; this note covers only attention: what the user sees in a list, what counts as needing them, how they are told, and how answering from the phone meets an input lease held by the desktop.

## Bounds

- Platforms: iPhone, iPad and Android in the same version.
- Depth: watch a pane, full terminal input, and a needs-you inbox of seats.
- Transport: SSH only, directly or over a private network the user already runs. No relay operated for the user, no anonymous listener, and the daemon's local control socket is never exposed to the network.
- No notification backend in the first version. Anything that needs one is listed as a gap.
- Approving or rejecting an agent's request from the phone needs a new approval protocol. It is out of scope here and listed under Deferred.
- Sessions and processes stay on the computer. Closing or losing the phone never stops them.

## What the daemon knows today

All locations are at main `3bba8e8f`.

### Seat state, worked out by the daemon

`ReadActivity` returns one entry per session that has an `agent_id`, with a state the daemon recomputes on each read (`activity_sessions` and `activity_session_state`, `crates/prismattyc-mux/src/control.rs`). Nothing is stored.

The state belongs to the session, not to a pane. The daemon folds all of the session's panes into three inputs:

- Live: at least one pane has a live child.
- Foreground: the most significant foreground across the panes. An agent in any pane wins, then any other program, then a shell. If no foreground can be read, it is unknown.
- Last output: the most recent output time across the panes, if any pane has printed.

| State | Rule, on those session-wide inputs |
| --- | --- |
| `stopped` | No pane has a live child. |
| `idle` | The foreground is a shell: no pane has an agent or another program in front. |
| `stuck` | An agent is in front in some pane, and no pane has printed for 5 minutes. |
| `working` | Some pane printed output in the last 60 seconds. |
| `waiting` | The fallback for every other live session. That includes an agent quiet for between 60 seconds and 5 minutes, another program quiet for over a minute, a foreground that cannot be read, and a session with no output time at all. |

Three consequences for a companion:

- Output in any pane marks the whole seat `working`. A build printing in a second pane hides an agent that is waiting in the first.
- `stuck` needs every pane to be quiet, so a seat with a busy second pane is never `stuck`.
- `waiting` mixes cases the user cares about differently: an agent thinking quietly, an agent at its own prompt, and a session the daemon could not classify. The state alone cannot tell them apart.

Nor does the state say which pane the agent is in.

### Activity events, reported by the agent

Agents publish events with `pmux activity-emit`, from their hooks or by hand (`crates/prismattyc-mux/src/bin/pmux/activity_emit.rs`). The event kinds are in `ActivityKind` (`crates/prismattyc-core/src/activity.rs`). The ones that matter for attention:

- `waiting`: emitted for a permission request hook. The agent is blocked on a person.
- `turn_finished`: the agent finished its turn and is at its prompt.
- `tool_failed`, `command_failed`, `edit_failed`, `test_failed`: something failed.
- `mail_received`, `room_message`.

Events live in an in-memory ring of 512 records with a random epoch, read with `ReadActivity{cursor, limit}`. They are not persisted, and there is no push. The activity monitor plug-in already folds these events into a seat state with its own order of urgency: `failed` and `stuck` first, then `waiting` (`crates/pmux-activity-monitor-plugin/src/model.rs`).

### Attention

- Terminal notifications (OSC 9, 777 and 99) set pane attention. The daemon emits `PaneAttention`, and the message shows in `PaneSnapshot.attention`.
- `RaiseAttention` creates an explicit request for human input (`AttentionRequest`, `crates/prismattyc-mux/src/team_attention.rs`): a message, a source, a raise time and a snooze time. `AttentionRequests` lists them, and `UpdateAttention` resolves or snoozes one. `pmux attention` is the command line for it. A terminal notification does not create a request.
- Any write to a pane clears both kinds of attention and emits `PaneAttentionCleared`.
- `PaneIdle` fires once each time a pane goes quiet for 60 seconds.
- The desktop host turns attention into a tab badge, a sound and a system notification. `pmux-attach` forwards pane attention to its outer terminal as OSC 9, so it already survives `ssh -t`.

### Mail

Letters are stored per seat in SQLite, each open or held. When a letter arrives the daemon does three things (`on_mail_stored`, `control.rs`):

1. It sets mail attention on the seat's first pane and emits `MailAttentionChanged{depth, wake, queue_rev}`. `PaneSnapshot.mail` carries the depth.
2. It tries to wake the agent by typing a fixed word and the agent's submit key, never the letter. The wake is gated: it takes a free input lease, and it waits while the pane is busy, while someone's typed input is unsent, or when no agent is in front.
3. It publishes a `mail_received` event with ids and a count only.

Mail is the agent's work, not the user's. It needs the user only when the wake could not be delivered and the letters sit unread.

There is no way to read letter summaries without claiming them, and claiming changes their state. No call lists letters across seats.

### Input ownership

- A pane has at most one controller lease. `AcquireLease` fails while another client holds it, `TakeoverLease` always succeeds, and releasing or disconnecting frees it.
- The holder may write, and anyone may write to a pane with no holder (`allow_write`, `crates/prismattyc-mux/src/domain.rs`).
- `WritePane` refuses a writer without the lease while another client's typed input is still unsent.
- `pmux-attach` takes the lease on the first key and drops it after 750 ms without input. `--read-only` never takes it.
- The desktop host acquires the lease before it writes.
- A lease holder is identified by a client id. There is no label saying which device or app the client is.

### What a thin client can read over SSH today

The phone reaches the computer by SSH and runs the `pmux` command line there, as the remote Space catalog does (`docs/design/remote-spaces-ssh.md`). Through it the phone can read:

- the topology, with per-pane mail, attention, controller and input state (`Snapshot`);
- events since a sequence number (`Events`, a poll over a ring of 256 with a resnapshot on a gap);
- seat states and the activity ring (`ReadActivity`);
- the attention requests (`AttentionRequests`);
- a pane's text and style runs (`ReadPane`, `ReadPaneStyled`), or a stream of its output (`SubscribePane`).

The remote catalog's schema (`crates/prismattyc-mux/src/remote_catalog.rs`) has only Space and session ids and names, and it rejects unknown fields, so state cannot be added to it without a version change.

## What counts as needing you

A seat needs the user when it cannot go on without a person, or when something failed that the user should see. In order of urgency:

| Rank | Signal | Source | Leaves the inbox when |
| --- | --- | --- | --- |
| 1 | An open attention request that is not snoozed | `AttentionRequests` | It is resolved or snoozed, or the user writes to the pane |
| 2 | Blocked on a permission request | the latest activity event is `waiting` | The next event is not `waiting`, or the user writes |
| 3 | Stuck | seat state `stuck`, which is session-wide | Output resumes in any pane, or the user writes |
| 4 | Failed | the latest activity event is a `*_failed` kind | The next event starts new work |
| 5 | A terminal notification not yet cleared | `PaneSnapshot.attention` | The user writes to the pane |
| 6 | Finished its turn and waiting on you | the latest event is `turn_finished` and no input since | The user writes, or a new turn starts |
| 7 | Mail the agent has not taken | mail depth above zero and the wake not delivered | The agent claims it |

Not needing the user: `working`, `idle`, `stopped` with no failure, and `waiting` with no event that explains it. Those seats show in the full list but not in the inbox. Because seat state is session-wide, the inbox does not rank on `working` or `waiting` at all. Ranks 2 and 6 come from the agent's own events, which are not blurred by other panes. Rank 3 can be missed when another pane in the seat keeps printing. That is a known limit until the daemon reports state per pane (gap 11).

Opening a seat from the inbox shows the pane that raised the signal when the signal names one: an attention request, a terminal notification, or mail attention. Otherwise it shows the seat's first pane, the same pane the daemon uses for the mail doorbell.

The rule for clearing follows the daemon's own: a write to the pane clears pane attention. Answering a seat from any device, the desktop included, takes it out of the inbox everywhere.

Rank 6 is the noisiest. A seat that finishes many short turns would badge often. It is on by default, as features ship on, with a per-seat mute in the inbox. The other ranks cannot be muted, only snoozed (rank 1) or answered.

## List and inbox

The companion opens on the inbox for the paired computer. Seats are grouped by computer when more than one is paired.

```
Needs you                        3
● build-agent   Waiting for permission   2m
  "Allow running the test suite?"
● docs-agent    Stuck: no output for 7m
● api-agent     Test failed              12m

All seats                        8
  build-agent   waiting                  2m
  ...
```

- Each row shows the seat name, the reason in plain words, and how long it has been in that state.
- An attention request shows its message as a second line. Other reasons show no second line.
- Rows sort by rank, then by age, oldest first.
- The app badge is the number of rows in Needs you. A muted seat does not count.
- Mail shows as a count on the seat's row in All seats. It joins the inbox only at rank 7.
- On iPad, and on Android tablets, the list stays as a sidebar beside the open pane. On phones, opening a row replaces the list, and Back returns to it.
- A seat whose computer is not connected keeps its last known row, greyed, with the time it was last seen. A stale row never shows as live.

## Answering from the phone

Opening a row shows the pane read-only. Watching never takes the lease, so the desktop is never interrupted by someone looking.

Typing is an explicit step:

1. The user taps the keyboard or Take control. The phone asks for the lease with `AcquireLease`.
2. If no one holds it, the phone holds it while its keyboard is open and releases it when the keyboard closes, when the app leaves the foreground, or after a short time without input, matching `pmux-attach`.
3. If the desktop holds it, the phone says the pane is in use on another client and offers Take over. Taking over needs a confirmation. A desktop user mid-line loses the lease only after that confirmation, never silently.
4. If another client's typed input is still unsent, the daemon refuses the write. The phone shows that reason and does not retry on its own.

Writing to the pane clears its attention, which takes the seat out of the inbox on every device.

A raw terminal write is the only answer in this version. A request from an agent is answered by typing to it, as at the desk. Answering a request with a button, without opening the pane, is the approval protocol deferred below.

## Telling the user without a notification backend

A notification needs something running that can see the computer. With no backend, that can only be the phone itself or the desktop.

- While the companion is in the foreground, it keeps one SSH channel open and polls the inbox every few seconds. The badge and list stay current.
- On Android, the companion can keep that channel open in the background as a foreground service with its ongoing notification, and post a local notification when a seat joins the inbox. This is on by default while a computer is paired, and the ongoing notification says so.
- On iPhone and iPad, an app cannot keep a connection open in the background. The companion asks the system for background refresh and, when it runs, polls once and posts local notifications for new rows. The system decides when that is, so delivery is best effort and can be minutes or hours late.
- The desktop already notifies its own user. The companion does not try to forward through it.

Timely notifications on iPhone and iPad need a push sent from outside the phone. That means either a service the user does not run, which the bounds rule out, or the desktop sending through the platform's push service, which needs a push credential on the computer. Both are listed as gaps.

## Protocol gaps

1. No single inbox read. The phone has to join `Snapshot`, `ReadActivity` and `AttentionRequests` itself. Proposed: a versioned, read-only `pmux attention inbox --json` that returns the ranked rows above, so the rules live in one place and the desktop can use them too.
2. The remote catalog rejects unknown fields, so seat state cannot be added to it. The inbox command above is separate and versioned, which avoids that.
3. No push. `Events` and `ReadActivity` are polls, and the events ring is small (256). Polling over one SSH channel is enough for the first version. A subscription stream is a later change.
4. The `waiting` state does not distinguish an agent at its prompt from one thinking. The inbox uses the latest event instead, which needs the agent's hooks. A seat without hooks only ever reaches the inbox through `stuck`, a failure, or a terminal notification.
5. A terminal notification does not create an attention request, so it cannot be snoozed, and it shows no source. Proposed: the inbox treats it as a request with source `terminal`.
6. A lease holder has no label. To say "in use on the desktop", the daemon needs a client label given at hello, such as an app and device name.
7. No read-only mail view. Reading a summary means claiming the letter. The first version shows only counts, and never claims mail for a seat.
8. Muting a seat (rank 6) and "seen on this device" have no place to live in the daemon. The first version keeps them on the phone.
9. Docs refer to a `pmux activity tail` command that does not exist. The inbox command would cover what a companion needs.
10. A background push on iPhone and iPad needs either a push service or a push credential on the desktop. Neither exists.
11. Seat state is per session. Output in one pane hides a quiet agent in another, `stuck` needs every pane quiet, and the state does not name the agent's pane. Proposed: `ReadActivity` also returns per-pane state and the pane the agent is in, so the inbox can rank and open the right pane.

## Deferred

- Approve or reject a request from the phone without opening the pane. It needs seat permission modes and an approval message the agent can wait on, which is a new protocol with its own design.
- Reading letters on the phone, and sending mail from it.
- Pushed notifications on iPhone and iPad, until the owner decides how a push may be sent.
- A combined inbox across computers that are not connected at the same time.

## Recommended first slice

1. A read-only `pmux attention inbox --json`, versioned, built from the existing reads, with the rank and clearing rules above. It is useful on the desktop and from `ssh` alone, before any companion exists.
2. The companion's inbox and full list over SSH, polling while in the foreground, with the badge.
3. Open a row to watch the pane read-only. Typing takes the lease as described, with Take over behind a confirmation.
4. Local notifications from the Android background service, and best-effort background refresh on iPhone and iPad.
5. A client label at hello, so a refused lease names the other client.

Each step can ship alone, and each is on by default.

## Demonstration

The companion's catalog and terminal prototype is not in the repository yet, so this note has no runnable companion demonstration. The first step of the slice, the inbox command, can be demonstrated without it once built: start an isolated daemon under its own `PMUX_SOCKET`, start sample seats, raise an attention request with `pmux attention`, emit a `waiting` and a `test_failed` event with `pmux activity-emit`, and read the inbox over a loopback SSH test account. Never point it at the live development daemon.
