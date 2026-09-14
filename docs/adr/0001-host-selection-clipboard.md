# ADR-0001: Host selection, clipboard, and key ownership

- **Status:** Accepted (policy freeze for classic host UX)
- **Date:** 2026-07-29
- **Charter:** classic path first; host chrome must not corrupt the child PTY model
- **Related:** [fidelity-matrix-v1.md](../fidelity-matrix-v1.md) F6/F7,
  [hybrid-rendering.md](../hybrid-rendering.md) selection z-order

## Clean-room process (binding)

This decision was written under an explicit **clean-room** rule:

1. **Study** publicly documented terminal *behavior* and interop specs (what users expect;
   what OSC/DECSET mean on the wire).
2. **Write** Prismattyc policy in our own words in this file.
3. **Implement** only from this ADR + Prismattyc’s existing architecture — **original code**.
4. **Do not** copy source files, paste nontrivial snippets, or vendor terminal emulator
   trees into this repository.
5. **Do not** pull GPL-licensed implementation into Prismattyc without a separate, explicit
   copyleft decision (out of scope here).

**Studied for practice (ideas only — no code incorporated):**

| Kind | What we used it for |
|------|---------------------|
| Common terminal UX (Alacritty, WezTerm, Ghostty, Kitty *as products*) | Selection lifecycle, copy chords, paste wrapping expectations |
| [xterm ctlseqs](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html) | OSC 52, DECSET/DECRST 2004, SGR color forms |
| Kitty keyboard *protocol documentation* | Why hosts must negotiate modified keys; not Kitty’s C sources |
| Prismattyc dogfood (2026-07-29) | Ctrl+Shift+C stolen by outer terminal; stale selection after child output; Shift+arrow often unreported |

Attribution of *ideas*: “Policy informed by common terminal emulator practice; implementation original to Prismattyc.”

## Context

Prismattyc is both:

1. A **VT/PTY emulator** for the child (screen model, SGR, modes), and
2. A **host application** that owns mouse capture, keyboard routing, selection chrome,
   and clipboard writes toward the *outer* terminal (Ghostty/Kitty/…).

Dogfood showed that ad-hoc host UX patches (Shift+arrow, copy chords, when to clear
selection) thrash without a written ownership model. Outer hosts also **steal** chords
(e.g. Ctrl+Shift+C) before the app sees them. Policy must name **reliable** chords and
**clear triggers**, not only ideal ones.

## Decision

### D-H1 — Two layers stay separate

| Layer | Owner | Must not |
|-------|--------|----------|
| Child VT grid + cursor | `prismattyc-core` / `prismattyc-emulator` | Know about host menus or OSC 52 |
| Host selection + clipboard + key routing | `prismattyc` binary (composition root) | Mutate cell storage for selection chrome |

Selection is **viewport cell-range inverse paint** only (z-order after grid / cell-rect;
see hybrid freeze). It never writes into `Screen` cells.

### D-H2 — Selection lifecycle

**Begin**

- Left mouse down on a viewport cell, or
- Keyboard mark: **Ctrl+2** (reliable NUL/`^@` path), or **Ctrl+Space** / NUL **when the
  outer host delivers it** (often stolen by desktop/IME —), or
- First **Shift+arrow** when modifiers are reported (primary grow path; also starts select).

**Extend**

- Left drag, or
- Shift+arrow when SHIFT is reported, or
- Plain arrows while **keyboard-select mode** is on (entered by mark or Shift+arrow).

**Finish**

- Left mouse up → finish range; **auto-copy** if payload is eligible (see D-H4).
- Keyboard may leave an active finished range until cleared.

**Clear (drop range + leave keyboard-select mode)** when any of:

1. **Esc**
2. **Child PTY produces output** while a range or select mode is active (echo, command
   output, new prompt) — cell contents under the range are no longer the selected text
3. **content_epoch** change (scroll, alt-screen enter/leave, resize)
4. New left-down begins a fresh gesture (replaces prior range)

**Leave keyboard-select mode** (range may remain or be replaced by the mouse path) on
any left-button mouse gesture that host selection handles (down / drag / up) —
after Ctrl+Space then mouse drag, plain arrows must forward to the child again.

**Click-only** (down+up, no drag): no retained range, no auto-copy (avoids sticky one-cell
inverse).

**Click count (mouse, same cell, within ~500ms)** — original Prismattyc implementation of common
terminal practice (not foreign source):

| Clicks | Action |
|--------|--------|
| 1 | Cell selection (drag to extend) |
| 2 | Select **word** under pointer (run of same character class) |
| 3 | Select **entire viewport row** under pointer |

Character classes for word select: (1) whitespace, (2) ASCII alphanumeric + `_`,
(3) all other graphic cells. Expand while adjacent cells share the class.

Double/triple-click set a finished paintable range (`dragged`) and auto-copy on mouse-up
like a drag. A fourth click in the window restarts at 1.

**Geometry (0.1.x):** row-major stream order over the **visible view**. Coordinates are
always **viewport rows/cols** (what the user sees). When `view_scroll == 0` that is the
live primary bottom; when scrolled hit-test / paint / extract use the
history window (`view_cell`, `extract_text_view`, `selection_covers_cell_view`). Multi-row
ranges are stored full-width between anchor and free end (stream order). **Visual paint**
trims trailing space cells on each multi-row line; extract already trims trailing spaces
per line. Single-row ranges stay geometric so a deliberate drag over spaces still paints.

**Select all:** **Ctrl+Shift+A** selects the entire **visible** viewport (finished
paintable range), enters keyboard-select mode, and auto-copies if the payload is eligible
(D-H4). Plain **Ctrl+A** is **not** claimed (forwarded to the child for readline BOL).

**Keyboard motion (while select mode, or with Shift):** Left/Right/Up/Down, **Home**
(col 0), **End** (last col), **PageUp** / **PageDown** (≈ viewport height steps).

**Host ownership while scrolled:** copy chords (D-H4), Esc clear, and select-all must run
**before** “jump back to live on typing.” Clearing `view_scroll` first must not drop a
multi-cell history selection and turn Ctrl+C into child ETX.

**Out of scope for this ADR:** vi-mode, rectangular (block) select, right-click context
menu, cross-window/multiplexer selection.

### D-H3 — Key ownership

| Input | Host handles | Forwards to child |
|-------|--------------|-------------------|
| Shift+arrow / Shift+Home/End/PgUp/PgDn (SHIFT reported) | Extend selection | No |
| Arrow / Home / End / PgUp / PgDn while keyboard-select mode | Extend selection | No |
| **Ctrl+2** (mark; preferred) | Begin select mode | No |
| Ctrl+Space / NUL (when outer host delivers) | Begin select mode | No |
| Ctrl+Shift+A | Select entire viewport + optional auto-copy | No |
| Esc with selection/mode | Clear | No |
| Copy chords (D-H4) | OSC 52 to outer host | No |
| Other keys | Leave select mode (finish range) | Yes (existing encoder) |
| `Event::Paste` | Wrap if child DECSET 2004 | Yes (bytes to child) |

Outer terminals may **never deliver** some chords. Policy therefore includes **fallback
chords** that still reach the app (D-H4).

**Rebinding (ADR-0015):** `Ctrl+Shift+A` (select all), `Ctrl+Shift+C` (copy) and
`Ctrl+Shift+V` (paste) are user-rebindable through `[keys]` in config.toml. The rest of
this table — Shift+motion, mark keys, Esc, `Shift+Insert`, and plain `Ctrl+C` with a
multi-cell selection — is fixed: those are semantics and fallbacks, not chords.

Prefer negotiating **keyboard enhancement** (disambiguate + event types) so SHIFT on
arrows is reported when the host supports it. Do not depend on enhancement alone.

### D-H4 — Clipboard (copy out)

**Mechanism:** OSC 52 clipboard write to the **outer** terminal only
(`OSC 52;c;<base64> BEL`). Never write OSC 52 to the child PTY.

**When to copy**

| Trigger | Action |
|---------|--------|
| Mouse-up after a real drag | Auto-copy if eligible |
| **Ctrl+Shift+C** | Copy if range present (when outer host does not steal the chord) |
| **Ctrl+C** while a **multi-cell** host selection exists | Copy; **do not** send `^C` to the child |
| **Ctrl+C** with **no** multi-cell selection | Forward interrupt (`^C`) to the child |

**Ctrl+C vs one-cell mark:** A pure one-cell mark from **Ctrl+2** /
**Ctrl+Space** (no extension via arrow/drag) is paintable for inverse chrome and
keyboard grow, but does **not** count as a selection for the Ctrl+C copy chord.
Accidental mark then Ctrl+C must still interrupt the child. Multi-cell ranges
(mark + motion, Shift+arrow, mouse drag, word/line click spanning cells) still claim
Ctrl+C as copy.

**Outer-host mark delivery:** Product docs must not claim Ctrl+Space works
on all hosts. Preferred user-facing entry: **Shift+arrow**; reliable mark without
Shift reporting: **Ctrl+2**. Ctrl+Space is documented only with a host-capture caveat.

**Eligible payload** (no-op / no OSC 52 otherwise):

- Non-empty after extract
- Not **whitespace-only** (spaces/tabs/newlines with no other scalars)
- ≤ 64 KiB plain text
- No C0 except tab/LF, no DEL, no C1

Extract: row-major, trailing spaces trimmed per line, lines joined with `\n`.

**Paste in:** Host `Event::Paste` → child bytes.

1. **Normalize:** strip every layer of `\e[200~` … `\e[201~` from the paste string
   (nested outer hosts often leave an inner layer after crossterm
   removes one).
2. **Wrap once** with `CSI 200 ~` … `CSI 201 ~` only if the **child** enabled DECSET
   **2004**; otherwise send raw payload.
3. Never write paste wrappers to the outer host; never double-wrap.

Image and file paste uses the following fallback order:

1. Prefer nonblank clipboard text.
2. If the clipboard exposes one image file, paste that existing path.
3. If the clipboard exposes one `text/uri-list` image URI, decode it and paste that path.
4. Otherwise save clipboard image data as a PNG under
   `$XDG_RUNTIME_DIR/prism-paste` and paste the generated path.

The child receives the path as plain text. Cursor-agent detection prefixes the path
with `@`. The host shows `pasted image → <basename>` in the existing toast area
after the child accepts the complete payload. The host keeps the last 8 generated
PNG files and does not remove source files from a file manager.

If the clipboard has multiple files, a non-image file, malformed URI data, or no
readable image data, the host does not send a fallback path. This behavior depends
on the native clipboard API. It is available only where arboard exposes image or
file-list clipboard data; remote `pmux-attach` image preview and Kitty graphics
remain deferred.

### D-H5 — Explicit non-goals (this ADR)

- Right-click context menu
- Depending on outer-terminal native selection instead of host grid selection
- Vendoring or copying Alacritty/WezTerm/Kitty/Ghostty source
- Full keyboard protocol product claim beyond what we negotiate for modifiers
- Rectangular / multi-pane selection

## Mapping to current code (implementation status)

| Policy | Location (approx.) | Status |
|--------|-------------------|--------|
| Selection model | `prismattyc-core` `Selection` / `CellRange` | Landed |
| Click-only no sticky cell | `Selection::dragged` + mouse-up clear | Landed |
| Clear on child output | `prismattyc` event loop PTY drain | Landed |
| Clear on epoch | same | Landed |
| Mouse drag + auto OSC 52 | `handle_mouse` | Landed |
| Ctrl+Shift+C / Ctrl+C+multi-cell | `is_copy_chord` + `selection_claims_ctrl_c` + `copy_selection_osc52` | Landed |
| One-cell mark does not claim Ctrl+C | `selection_claims_ctrl_c` | Landed |
| Selection + copy in scrollback view | `view_scroll` + `extract_text_view`; copy/Esc **before** jump-to-live | Landed (dual-offset fix) |
| Nested outer-PTY UX scripts | `tests/nested_pty_ux` + `tests/support` | Landed |
| Mouse leaves keyboard-select mode | `handle_mouse` + `keyboard_select_mode` | Landed |
| Ctrl+Space + arrows | `keyboard_select_mode` | Landed |
| Bracketed paste | `Emulator::bracketed_paste` + `handle_paste` | Landed |
| Image/file paste fallback | `prismattyc-mux::image_paste` + windowed host | Landed (PT-43) |
| Whitespace-only no OSC 52 | `encode_osc52_clipboard` | Landed |
| Keyboard enhancement push | `enter_host_modes` | Landed (best-effort) |
| Double-click word / triple-click line | `Screen::word_range_at` / `line_range_at` + multi-click | Landed (0.1.x) |
| Multi-row visual EOL trim | `Screen::selection_covers_cell` | Landed (0.1.x) |
| Select-all Ctrl+Shift+A | `viewport_range` + `is_select_all_chord` | Landed (0.1.x) |
| Home/End/PgUp/PgDn select motion | `extend_selection_keyboard` | Landed (0.1.x) |
| Context menu | — | **Not started** |
| Selection while scrolled + absolute history rows | `abs_row_at_view` + `extract_text_abs` + edge autoscroll | **Landed** |


Future work implements **against this ADR**, not against chat lore. Gaps cite
section IDs (D-H2, D-H4, …).

## Consequences

- Host UX changes require an ADR update (or a superseding ADR), not silent chord invention.
- Outer-host chord theft is a **known environmental constraint**; fallbacks stay first-class.
- Clean-room rule keeps license risk low while still allowing us to match user expectations.
- Falsifier: a decision to vendor a full selection engine from another terminal under an
  incompatible license — that would supersede D-H5 and require a new legal/product decision.

## Alternatives rejected

- **Copy Alacritty/WezTerm selection modules** — license and maintenance cost; violates
  clean-room choice; couples us to foreign architecture.
- **Rely only on outer-terminal selection** — broken under full-grid alt-screen + mouse
  capture; child and host fight over the mouse.
- **Ctrl+Shift+C only** — empirically stolen by Kitty/Ghostty; dogfood failed.
- **Never clear selection except Esc** — stale inverse over new bash output (observed).
- **Clear selection on every keystroke including arrows in select mode** — would break
  keyboard grow.

## Change log

| Date | Change |
|------|--------|
| 2026-07-29 | Initial clean-room policy freeze from dogfood + common terminal practice |
| 2026-07-29 | D-H2: double-click word / triple-click line (click-count) |
| 2026-07-30 | D-H2/D-H3: multi-row visual EOL trim; Ctrl+Shift+A select-all; Home/End/Pg motion |
| 2026-07-30 | D-H2/D-H4: one-cell mark does not claim Ctrl+C; mouse leaves keyboard-select mode |
| 2026-08-26 | Reformulated for Prismattyc naming |
| 2026-08-27 | D-H3: rebindable vs fixed inputs split per [ADR-0015](0015-user-keybindings.md) (PT-41) |
| 2026-08-28 | D-H4: PT-43 image/file paste fallback, generated PNG retention, and success feedback |
