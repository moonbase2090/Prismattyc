---
name: pmux
description: 'use this when running inside a Prismattyc/pmux pane, or when sending or reading mail, writing to another pane, or coordinating with other agents'
---

# Work in a Prismattyc pmux pane

Use pmux's live seat and command help as the source of truth. Do not guess a
session name, pane ID, agent ID, mail verb, or option.

## Identify your seat

Run `pmux whoami` from the pane you occupy. It reports the live session name,
session ID, pane ID, and bound agent name. The agent name is your mailbox
address. Run `pmux ls` to see sessions, windows, and pane IDs when you need to
address another pane.

Do not run bare `pmux` with no command: it attaches interactively and can leave
an agent waiting. Use an explicit command such as `pmux whoami`, `pmux ls`, or
`pmux mail inbox`.

## Send and read mail

Use `pmux mail --help` for the mailbox's current command list. Common forms:

```sh
pmux mail send prismattyc-2 --summary READY --body 'PR: https://...; tip: abc123'
pmux mail inbox
pmux mail claim --json
pmux mail commit LETTER_ID
```

`inbox` peeks at open and held mail. `claim` fetches open letters and holds
them for you; use `claim --json` to read their summaries, bodies, and IDs, or
`claim --ids` when you only need IDs. After acting on a letter, acknowledge it
with `commit` and its ID. A committed letter is removed. Use only IDs returned
by the mailbox.

`READY` and `SHIP` are ordinary mail summaries, not special pmux verbs or
server states. Use `READY` when work is complete and ready for review; include
the PR link and tip SHA. Use `SHIP` only after a release or artifact has
actually shipped; include its version and where to get it. A READY letter does
not mean a change was merged or shipped.

Mail delivery does not start or wake an idle agent. Check `pmux mail inbox`
after each task, and do not assume a sent letter has been read or acted on.
When waiting in an agent loop, `pmux mail watch` can wait for a letter; after
it returns, use `pmux mail claim` to read it. A timeout is not a mux failure.

## Write to a pane

Use `pmux send PANE TEXT [--enter] [--literal] [--force]` for terminal keys.
It sends text as key input; Enter is the `--enter` option, which appends a
carriage return. To keep typing and submitting as separate keypresses:

```sh
pmux send 17 'the text to type'
pmux send 17 --enter
```

`--force` takes over a pane with a live controller or unsubmitted input. Use
it only when taking over that pane is intended. `--literal` disables pmux's
backslash escape decoding. `send` is terminal input, not mailbox delivery.

Use `pmux pane-write PANE --text TEXT [--submit auto|enter|none] [--json]`
when deliberately delivering literal text to one agent pane. Its default
`auto` submit mode uses the detected foreground agent's submit sequence;
`enter` appends Enter and `none` leaves the text unsubmitted. Busy or dirty
input is refused. A receipt means text was queued, not that the recipient read
it. Do not retry a partial write automatically.

## Read pane output

Use `pmux save-buffer PANE|SESSION FILE [--history]` to read visible pane
text. `-` writes it to stdout. `--history` includes scrollback. Find the pane
ID with `pmux ls` first; pane IDs and session IDs are different.

## Sessions, tabs, and Spaces

- A **session** is pmuxd's mailbox and agent unit. `pmux ls` lists sessions.
- A **tab** is a host window. Its panes come from sessions; `pmux ls` shows
  the windows and panes. There is no `pmux tabs` command.
- A **Space** exclusively owns sessions. A session can belong to one Space.
  Use `pmux space ls` to list Spaces. `pmux space open NAME` switches to one;
  `--new-window` opens it in another host window.

Do not confuse a pane ID, session ID, session name, and agent mailbox name.
Use the value accepted by the command you are running.

## Attention and status

`pmux attention SESSION [MESSAGE]` raises an attention signal for that
session's pane. It is a notification, not a pane write or mail message.

`pmux status-set TEXT [--pane ID]` sets attach chrome status for a pane.
`pmux status-set --clear [--pane ID]` clears it. With no `--pane`, status-set
uses the current pane when run inside pmux.

## Common mistakes

- Do not run `pmux stop`. It stops the mux or a session and can destroy other
  agents' work.
- Do not use `pmux send` to deliver a coordination message; use
  `pmux mail send`.
- Do not assume mail starts an agent. Check the inbox after each task.
- Do not assume pane-write's queue receipt means the other agent accepted the
  message; inspect the pane or wait for a reply.
- Do not pass `--force` casually. It takes the input lease from a busy pane.
