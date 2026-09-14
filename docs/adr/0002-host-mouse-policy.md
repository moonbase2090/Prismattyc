# ADR-0002: Host mouse policy (classic 0.1.x)

- **Status:** Accepted (policy freeze for classic host UX); **superseded in part** by
  [ADR-0003](0003-hybrid-mouse.md) (Option B hybrid) on tips that include that ADR
- **Date:** 2026-07-31
- **Charter:** classic path first; host chrome must not corrupt the child PTY model
- **Related:** [ADR-0001](0001-host-selection-clipboard.md) (selection/clipboard), [fidelity-matrix-v1.md](../fidelity-matrix-v1.md) F6/F7 and exclusions

## Context

Prismattyc enables **host** mouse capture (`EnableMouseCapture`) so left-button
drag/select, multi-click word/line, and scrollback-view hit-test work (F6).

It does **not** emit application mouse reports to the child PTY (DECSET **1000** /
**1002** / **1003** / **1006**, X10/SGR/UTF-8 mouse). Fullscreen apps under a rich
`TERM` (vim, htop, less with mouse, etc.) therefore receive **no clicks** as
in-app events — only host selection chrome.

That gap was logged as: not a crash, but a silent mismatch if readers
assume “full terminal = app mouse.” Human smoke H12 confirmed current behavior
(clicks select on host, do not move the vim cursor) and treated it as expected
under exclusion.

Without a written decision, implementers thrash between “implement mouse tomorrow” and
“document forever.”

## Decision

### D-M1 — Classic 0.1.x claim: **host selection only**

For the **`prismattyc-classic/0.1.x`** classic claim:

| Mouse path | Owner | Child PTY |
|------------|--------|-----------|
| Left drag / multi-click / scroll gestures used for **host** UX | Host (`prismattyc` binary) | **Not** forwarded as app mouse |
| Wheel / Shift+wheel when used for **scrollback view** | Host | Not app mouse |
| DECSET 1000/1002/1003/1006 (and relatives) | **Ignored** (no mode table claim) | No SGR/X10 reports |

**Product wording:** Clicks are for **Prismattyc selection and scroll**, not for
moving the application caret. Matrix exclusion of application mouse reporting
is **intentional**, not an unfinished accident.

### D-M2 — Why not implement app mouse in this claim

1. **Selection ownership** is already a full ADR ([0001](0001-host-selection-clipboard.md));
   dual ownership without a hybrid rule causes double-consume bugs (click both
   selects and moves the app cursor).
2. **Alt-screen policy** (F4): host selection is suppressed on alt; app
   mouse would re-open key-ownership edge cases if added ad hoc.
3. **Fidelity honesty:** claiming app mouse requires a real mode table, protocol
   variants, and a large human-smoke matrix — out of scope for 0.1.x.

### D-M3 — Documented expected UX

| Workload | Expected with host-selection-only |
|----------|-----------------------------------|
| Shell / less (no app mouse) | Click-drag selects; copy chords work |
| vim / htop (want app mouse) | Click-drag **selects host text**; does **not** move app cursor / click UI |
| Scrollback view | Wheel / Shift+Page pan history; selection in history allowed |

Human smoke: **“click does not move vim cursor”** is **PASS** under this
policy (not a regression).

### D-M4 — Explicit non-goals (this ADR)

- Implementing DECSET mouse report families
- Hybrid “Shift+click selects, plain click to app” (allowed as a **future** ADR)
- Right-click context menu
- Changing F6/F7 selection behavior except to keep host capture

### D-M5 — Future options

| Option | Sketch | Status |
|--------|--------|--------|
| **A. Full app mouse** when child sets 1000/1006 | Forward reports; host selection only when mouse off or with modifier | Not selected |
| **B. Hybrid** | Modifier (e.g. Shift) = host select; plain = app | **Selected in [ADR-0003](0003-hybrid-mouse.md)** |
| **C. Keep exclusion forever** | Status quo | Superseded for tips with 0003 |

No code path was required to “close” under this ADR beyond **docs + matrix honesty**.
Hybrid implementation lives under ADR-0003.

## Consequences

**Positive**

- Contributors stop treating silent no-clicks as a P0 omission.
- README and the fidelity matrix can point to one decision.
- Host selection remains simple and testable.

**Negative**

- Nested mouse-driven TUIs still feel “broken” to users who expect Kitty-class
  app mouse — mitigated by clear exclusion text.

**Follow-ups**

- Optional human matrix note: which apps request mouse (informational only).
- If Option A/B is chosen later, open a new ADR **before** implementation; cite
  this document as superseded-in-part.

## Mapping to code (enforced)

| Behavior | Location | Status under D-M1 |
|----------|----------|-------------------|
| Host mouse capture | `enter_host_modes` / `EnableMouseCapture` | Required for F6 |
| Host selection / scroll / multi-click | `handle_mouse` (no child write queue) | Landed |
| App mouse CSI to child | — | **Never generated** |
| Private modes 1000/1002/1003/1005/1006/1015/1016 | `apply_private_mode` explicit no-op arms | **Ignored by policy** (not stored) |
| Evidence | `app_mouse_private_modes_are_ignored` | Landed |

## References

- Issues:
- Matrix: F6, F7, exclusions list (application mouse reporting)
