# Supported fidelity matrix — prismattyc-classic/0.1.1

**Status:** Published supported classic claim — exact-head gated.
**Release id:** `prismattyc-classic/0.1.1` (classic claim). Workspace package
version is **`0.2.9`** and can move without widening this claim.
**Kind:** Supported classic product subset — not universal xterm parity,
not a modern-terminal marketing claim outside the rows below.

See [terminal input](input.md) and [hybrid rendering](hybrid-rendering.md)
for input ownership and composition rules.

## Reference behavior

| Field | Value |
|-------|--------|
| Primary reference | **xterm** control sequences, **patch #410** (2026-04-19; ctlseqs online: [invisible-island.net/xterm/ctlseqs](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)); **VT100** DECSTBM ([vt100.net DECSTBM](https://vt100.net/docs/vt100-ug/chapter3.html#DECSTBM)); Kitty keyboard protocol (clean-room, [keyboard encoding](input.md)) |
| Reference posture | Match **enumerated** row behaviors against those references; not a full binary differential harness vs a distro xterm package |
| Secondary notes | VTE-derived parse boundaries via the `vte` 0.15 crate; Prismattyc owns grid/PTY semantics |
| Host OS primary | Linux |
| Prismattyc experimental rich (`--experimental-rich`) | **Out of product claim** (experimental only) |

## Workloads in scope

| Workload | Expected |
|----------|----------|
| Interactive shell (`/bin/sh`, bash/zsh as child) | Printable output, line editing via child, clean exit restores host |
| Simple pagers / `printf` / `ls` style CLI | Grid text, 16-color + 256/truecolor SGR |
| Full-screen TUIs using **alternate screen + cursor/erase + DECSTBM + DECOM/DECAWM** | Usable when they stay within claimed CSI/private-mode rows |
| Editors requiring mouse reporting | **Claimed hybrid** — mouse input (1000/1002/1003 + SGR 1006; Shift=host select) |
| Editors requiring wide graphemes / combining | **Partial** — wide text width-2 + combining + ZWJ/skin/RI cluster join; not full UAX #29 |
| Apps using Kitty CSI-u progressive keyboard | **Partial** — keyboard encoding first claim slice |

The windowed host may enable OpenType ligatures for terminal-grid rendering.
This option is host-only and render-only. It does not change cell widths, PTY
sizes, selection ownership, hit testing, or the classic nested host.

## Required rows (must be green)

Each row lists: inputs, expected state, and evidence. P0 = any failure of the
expected state for that row at a claimed tip.

### F1 — Spike Baseline v0

| | |
|--|--|
| **Inputs** | Parser, grid, and control-sequence fixtures in `crates/prismattyc-emulator/tests/` |
| **Expected** | All baseline fixtures green; allowlisted gaps unchanged as non-P0 |
| **Evidence** | `cargo test --workspace --locked` includes baseline tests |

### F2 — Host restore

| | |
|--|--|
| **Inputs** | Interactive `prismattyc` session; normal child exit, abnormal PTY EIO, and Unix terminate signals (SIGINT/SIGTERM/SIGHUP) |
| **Expected** | Host raw mode cleared; host leave-alt-screen; mouse capture disabled; focus change disabled; cursor shown; partial `enter` failure best-effort rolls back already-emitted host modes; signal path restores via flag + Drop (idempotent with Drop) |
| **Evidence** | `TerminalGuard` Drop + `restore_host_terminal_once`; Unix signal flag install; `take_host_terminal_ownership_is_idempotent`; `enter_host_modes_*` injected-writer normal/EIO/fail-once rollback; live `real_binary_printf_transcript_contains_output`, `real_binary_sigterm_exits_under_pty_flood` |

### F3 — Live resize

| | |
|--|--|
| **Inputs** | Host `Event::Resize(cols, rows)` while a child is running |
| **Expected** | (1) Child PTY `winsize` updated **first** (fail-closed: on error, grid is not mutated); (2) then primary logical-line reflow and alternate-grid clip/pad resize; (3) no crash; (4) soft wraps reflow; explicit line breaks remain separate; (5) selection cleared |
| **Evidence** | `resize_clips_grid_without_panic`, `resize_updates_dimensions`, `real_pty_resize_updates_child_winsize` (child-observed TIOCGWINSZ/`stty size` after host `PtySession::resize`; **no** separate SIGWINCH-handler assertion), `host_resize_fail_closed_skips_grid_mutation` |

### F4 — Alternate screen (47 / 1047 / 1049)

Distinct xterm private modes (ctlseqs):

| Mode | Enter (`CSI ? Nm h`) | Leave (`CSI ? Nm l`) |
|------|----------------------|----------------------|
| **1049** | Save primary cursor; clear alt; switch | Restore primary cells + saved cursor |
| **1047** | Clear alt; switch (no primary cursor save) | Restore primary cells |
| **47** | Switch to alt **without** clearing alt content | Restore primary cells |

| | |
|--|--|
| **Inputs** | `CSI ? 47/1047/1049 h` / `l` around printed primary text; optional primary cell-rect attach |
| **Expected** | Mode semantics per table; primary preserved; on **alt**, host does **not** paint primary rich attachments or selection (suspend); CSI never leaks as grid text |
| **Evidence** | `alternate_screen_switches_buffers`, `alt_screen_preserves_primary`, `mode47_*`, `mode1049_*`; `paint_policy_skips_primary_overlays_on_alt`, `paint_policy_primary_overlay_absent_on_alt_then_resumes`; live `real_binary_alt_screen_child_output_appears` |

### F5 — Scroll region (DECSTBM)

Reference: VT100 DECSTBM ([vt100.net](https://vt100.net/docs/vt100-ug/chapter3.html#DECSTBM)). Origin mode is **F15** (DECOM).

| | |
|--|--|
| **Inputs** | `CSI top ; bottom r`; `CSI r` reset; LF inside/outside region |
| **Expected** | (1) Default/full on invalid (top≥bottom after conversion, or one-line); (2) min height two lines; (3) set/reset **home cursor to screen 1;1** (when DECOM off); (4) LF at bottom margin scrolls region only; (5) LF **outside** region advances within screen, never jumps into margin; (6) rows outside region preserved; (7) no CSI leak |
| **Evidence** | `scroll_region_*`, `reset_scroll_region_homes_cursor`, `decstbm_is_accepted_without_leaking` |

### F6 — Grid selection

| | |
|--|--|
| **Inputs** | Host left-button down/drag/up on viewport cells (mouse capture enabled); when app mouse is off, or with **Shift** while tracking is on ([mouse input](input.md)); keyboard selection paths (**F12**); scrolled history view (**F18**) |
| **Expected** | (1) Selection range uses **absolute history row indices** (viewport or scrollback view); (2) **does not mutate** cells; (3) Esc clears; (4) **invalidate** when `content_epoch` changes **or** child PTY produces output under a finished selection (mid-drag may keep the gesture); (5) **click-only** (no drag) leaves **no** sticky one-cell highlight; (6) new left-down starts a fresh gesture; (7) **double-click** selects word (char-class run); **triple-click** selects row; (8) multi-row **visual** paint trims trailing spaces (`selection_covers_cell` / abs); (9) **Ctrl+Shift+A** selects full viewport ([text selection](input.md) D-H2); (10) alt-screen refuses host selection gestures |
| **Evidence** | `selection_does_not_mutate_cells`, `content_epoch_bumps_on_margin_scroll_and_alt_roundtrip`, `selection_cleared_on_margin_scroll`, `selection_cleared_on_same_chunk_alt_roundtrip`, `click_only_selection_has_no_range`, `drag_selection_retains_range_after_finish`, `begin_clears_prior_dragged_selection`, `word_range_expands_alphanumeric_run`, `line_range_spans_full_row`, `multi_click_cycles_one_two_three`, `multi_row_selection_visual_trim_skips_trailing_spaces`, `viewport_range_covers_full_grid`, `select_all_chord_selects_viewport_without_forwarding`, `abs_selection_spans_history_after_edge_autoscroll`; host uses `content_epoch` + clear-on-output |

### F7 — Plain-text copy (OSC 52)

| | |
|--|--|
| **Inputs** | Non-empty selection; mouse-up auto-copy **or** Ctrl+Shift+C **or** Ctrl+C while a host selection exists **or** Ctrl+Shift+A (select-all auto-copy) |
| **Expected** | (1) Row-major plain text, trailing spaces trimmed, `\n` joins; (2) host `OSC 52;c;<base64> BEL` only if payload is non-empty, **not whitespace-only**, ≤ **64 KiB**, and free of C0 (except tab/LF), DEL, and **C1 U+0080–U+009F**; else no-op; (3) never write OSC 52 to child PTY; (4) Ctrl+C with multi-cell selection does **not** forward `^C` to the child; one-cell mark still forwards interrupt |
| **Evidence** | `selection_extract_and_osc52`, `osc52_rejects_controls_and_enforces_size_bounds`, `blank_grid_selection_extract_does_not_osc52`, `selection_copy_path_extracts_plain_text`, `extract_text_trims_trailing_spaces_per_line`, `is_copy_chord_ctrl_shift_c_and_ctrl_c_with_selection`, `handle_host_key_ctrl_c_with_selection_does_not_forward`, `ctrl_space_one_cell_mark_ctrl_c_forwards_interrupt`, `OSC52_MAX_PLAIN_BYTES` |

### F8 — Containment + bounds

| | |
|--|--|
| **Inputs** | Unsupported OSC/DCS/APC; malformed control; experimental-off APC; PTY output flood; non-reading child; large/host paste under full `to_child` queue |
| **Expected** | No payload as grid text; no hang/crash; no grid corruption; experimental-off generates no capability replies; **bounded** child→host queue (backpressure); host→child **key bursts** coalesce encoded bytes and use one short budgeted `try_send` path, preserving ordinary burst order without waiting forever on a stuck child; capability/control replies remain non-blocking; **paste** is chunked with its separate short polled `try_send` budget and signals BEL on drop/partial — all paths remain bounded |
| **Evidence** | Spike Baseline + 0B; `from_pty_queue_capacity_is_bounded`, `from_pty_backpressure_blocks_sender_until_drained`, `host_to_child_control_try_send_is_nonblocking_when_full`, `non_reading_child_control_try_send_returns_quickly_when_full`, `key_burst_coalesces_and_reassembles_without_loss`, `key_burst_reports_drop_within_budget_when_queue_stuck`, `pending_key_burst_flushes_before_bracketed_paste`, `paste_chunks_across_queue_and_reassembles`, `paste_reports_dropped_quickly_when_queue_full_and_stuck`, `enqueue_paste_chunks_partial_when_budget_expires`; final paint before EOF exit: `queued_data_then_eof_requires_paint_before_exit`, `real_binary_printf_transcript_contains_output`, `real_binary_alt_screen_child_output_appears`, `real_binary_double_dash_printf_transcript` |

### F9 — CI / exact-head gate

| | |
|--|--|
| **Inputs** | Every claimed release tip |
| **Expected** | Commands below green on the release tip; live GHA success on that tip |
| **Evidence** | `.github/workflows/ci.yml` + release tip run id; local `./scripts/test-phase1.sh` |

```bash
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --bin prismattyc --locked
cargo doc --workspace --no-deps
git diff --check origin/main...HEAD
```

### F10 — 256-color + truecolor SGR (0.1.x pack)

| | |
|--|--|
| **Inputs** | `CSI 38;5;n m` / `48;5;n m`; `CSI 38;2;r;g;b m` / `48;2;…` (semicolon); ISO colon `38:2:Pi:Pr:Pg:Pb` / `38:2::R:G:B` / `38:2:R:G:B` |
| **Expected** | Grid stores `Color::Indexed` / `Color::Rgb`; ANSI paint emits matching `38;5` / `38;2` (and background 48); basic 16-color SGR unchanged; incomplete RGB does not re-parse leftovers as SGR |
| **Evidence** | `parses_256_and_truecolor_sgr`; `parses_truecolor_sgr_colon_subparams` (Pi colorspace, empty Pi, semicolon, truncated reject); `truecolor_semicolon_then_bold_does_not_consume_trailing_sgr_as_rgb`; `truecolor_semicolon_fg_then_bg_chain`; `truecolor_colon_with_trailing_bold`; render test for indexed/RGB emission |

### F11 — Bracketed paste DECSET 2004 (0.1.x pack)

| | |
|--|--|
| **Inputs** | Child `CSI ? 2004 h/l`; host `Event::Paste` |
| **Expected** | Host tracks child mode; on paste: strip nested `\e[200~…\e[201~` layers (including embedded mid-payload delimiters), then wrap **once** iff child enabled 2004; never double-wrap; never write wrappers to outer host; payload size capped (1 MiB after normalize) |
| **Evidence** | `bracketed_paste_mode_tracks_decset_2004`, `paste_wraps_when_bracketed_mode_on`, `normalize_paste_strips_nested_bracket_layers`, `normalize_paste_neutralizes_embedded_bracket_delimiters`, `paste_does_not_double_wrap_nested_host_brackets`, `paste_payload_capped_at_one_mib` |

### F12 — Viewport keyboard selection (0.1.x pack)

| | |
|--|--|
| **Inputs** | Shift+arrow (primary); Ctrl+2 mark; Ctrl+Space when delivered; plain arrows in select mode; Home/End/PgUp/PgDn; copy chords |
| **Expected** | Host-local selection grows without forwarding motion to child; Esc clears; see [text selection](input.md) D-H2/D-H3 |
| **Evidence** | `shift_arrow_starts_viewport_keyboard_selection`, `handle_host_key_shift_left_does_not_forward_to_child`, `ctrl_space_then_arrow_selects_without_shift`, `extend_selection_home_end_and_page`, `select_all_chord_selects_viewport_without_forwarding` |

### F13 — Hybrid application mouse (mouse input)

| | |
|--|--|
| **Inputs** | Child DECSET `1000`/`1002`/`1003` (+ `1006` SGR); host mouse events with capture enabled |
| **Expected** | (1) Modes tracked (highest of 1000/1002/1003); (2) plain mouse → SGR (or X10 if no 1006) on child PTY; (3) **Shift** → host selection/scroll, no report; (4) level filters drag/motion; (5) RIS clears modes; (6) alt + plain forwards when tracking on |
| **Evidence** | `app_mouse_private_modes_are_tracked`, `ris_clears_mouse_tracking_modes`, `encode_mouse_report_*`, `hybrid_plain_click_forwards_sgr_not_selection`, `hybrid_shift_drag_selects_host_not_app`, `hybrid_alt_plain_click_forwards_sgr`, `hybrid_plain_wheel_forwards_not_scroll_view` |

### F14 — Wide Unicode display width (wide text)

| | |
|--|--|
| **Inputs** | Fullwidth / East Asian wide code points (e.g. CJK); narrow ASCII; zero-width marks; emoji ZWJ sequences; skin-tone modifiers; regional indicator pairs |
| **Expected** | (1) Wide glyphs occupy two cells (lead + `wide_cont`); (2) cursor advances by display width; (3) wrap when width-2 does not fit; (4) overwrite clears both halves; (5) paint/extract emit the lead once; (6) combining marks attach to previous base; (7) ICH/DCH/ECH expand/heal pairs; (8) ZWJ-joined emoji stay one cell (width of first base); (9) skin tones attach without advance; (10) RI pairs become one width-2 cell |
| **Evidence** | `put_char_wide_cjk_uses_two_columns`, `put_char_wide_wraps_when_not_enough_room`, `put_char_over_wide_clears_continuation`, `put_char_combining_mark_attaches_to_previous_base`, `put_char_multiple_combining_marks_stack`, `put_char_zwj_family_is_single_wide_cell`, `put_char_emoji_skin_tone_attaches_without_advance`, `put_char_regional_indicator_pair_is_one_wide_cell`, `put_char_after_zwj_cluster_places_next_glyph`, `char_display_width_classes`, `delete_chars_removes_whole_wide_pair`, `erase_chars_clears_whole_wide_pair`, `insert_chars_on_wide_cont_snaps_to_lead` |

### F15 — DECOM origin mode

| | |
|--|--|
| **Inputs** | `CSI ? 6 h/l` with DECSTBM set; CUP / VPA; DSR CPR (`CSI 6 n`) |
| **Expected** | When DECOM on: CUP/VPA relative to top margin; cursor confined to scroll region; CPR reports origin-relative row/col; DECOM cleared by DECSTR/RIS |
| **Evidence** | `decom_private_mode_tracks_and_affects_cup_cpr`, `decom_origin_mode_cup_relative_to_scroll_region`, `soft_reset_and_ris_clear_origin_mode` |

### F16 — DECAWM auto-wrap

| | |
|--|--|
| **Inputs** | `CSI ? 7 h/l`; printable chars at last column |
| **Expected** | Default on; when off, overwrite last column without wrap; DECSTR/RIS restore auto-wrap on |
| **Evidence** | `decawm_off_does_not_wrap_to_next_line`, `soft_reset_restores_autowrap` |

### F17 — Focus in/out (DECSET 1004)

| | |
|--|--|
| **Inputs** | Child `CSI ? 1004 h/l`; host focus gained/lost events |
| **Expected** | Mode tracked; when enabled, host sends `CSI I` (gained) / `CSI O` (lost) to child; host enables focus change in enter path and disables on restore |
| **Evidence** | `focus_report_mode_tracks_decset_1004`; host `EnableFocusChange` / `Event::FocusGained` / `FocusLost` path in `main` |

### F18 — Scrollback view + absolute selection

| | |
|--|--|
| **Inputs** | Primary scrollback present; mouse wheel / Shift+Page / Shift+Home/End; drag-select while scrolled; edge pan near top/bottom while dragging |
| **Expected** | (1) Wheel and Shift+Page pan history (bare Page goes to child); (2) paint uses `view_cell` / `render_scrolled`; (3) selection rows are absolute history indices; extract via abs/view APIs; (4) edge autoscroll keeps selection anchor; (5) alt forces live view |
| **Evidence** | `extract_text_view_reads_scrolled_history`, `shift_pageup_scrolls_view_when_scrollback_exists`, `mouse_wheel_scrolls_view`, `abs_selection_spans_history_after_edge_autoscroll` |

### F19 — Extended keyboard + Kitty CSI-u (keyboard encoding)

| | |
|--|--|
| **Inputs** | Legacy modified keys; Super; Shift+Tab; Kitty progressive enhancement (`CSI = flags ; mode u`, push/pop/query) |
| **Expected** | (1) Legacy: Alt+char ESC prefix; modified cursor/Page/Home/End/F-keys as xterm CSI with mod; Super in mod param; Shift+Tab `CSI Z`; Ctrl+letter CSI `27` form when needed; (2) Kitty: set/push/pop/query flags; main and alt independent stacks (cap 16); RIS clears stacks; when flags non-zero encode per keyboard encoding slice (disambiguate, event types, report-all, report-text); (3) host chords (selection/find) still intercept before encode |
| **Evidence** | `encode_key_alt_b_is_esc_b`, `encode_key_f1_is_xterm_ss3`, `encode_key_ctrl_left_right_are_modified_csi`, `encode_key_ctrl_up_down_and_alt_arrows`, `encode_key_ctrl_shift_letter_is_csi27`, `encode_key_backtab_is_csi_z`, `encode_key_super_mod_in_arrow`, `encode_key_modified_page_home_and_fkeys`, `kitty_keyboard_push_pop_set_and_query`, `kitty_keyboard_alt_screen_has_independent_stack`, `ris_clears_kitty_keyboard_flags`, `kitty_disambiguate_encodes_esc_and_ctrl_as_csi_u`, `kitty_report_all_encodes_plain_keys_as_csi_u`, `kitty_event_types_encode_repeat_and_release` |

### H1 — Windowed-host styled underlines (PT-45)

This optional row applies only to the windowed `prismattyc-host` raster path.
It does not widen the supported classic claim. Classic renderer re-emission and
mux/protocol styled-underline passthrough remain deferred.

| | |
|--|--|
| **Inputs** | Windowed host output using SGR `4`, `4:0..5`, `24`, and optional SGR `58` underline colors; the PT-45 acceptance command `printf '\\e[4:3mcurly\\e[0m \\e[58:2:255:0:0m\\e[4:3mred\\e[0m\\n'` |
| **Expected** | Render single, double, curly, dotted, and dashed underlines. Clear the underline with `4:0` or `24`. Use the effective foreground when no explicit underline color is set. Use explicit SGR `58` colors for normal text. Preserve Kitty placeholder placement semantics. |
| **Evidence** | Core style tests, emulator SGR tests, host raster pattern/color tests, `cargo test --workspace --locked`, and live PT-45 dogfood |

## Compatibility limits

- Alternate mouse encodings **1005 / 1015 / 1016** (X10/SGR 1006 only under F13)
- Full **UAX #29** grapheme segmentation (F14 is East Asian Width + combining + ZWJ/skin/RI slice only)
- Kitty CSI-u **alternate-key base-layout** depth and **lock modifiers** (Caps/Num) beyond keyboard encoding first slice
- Reflow of alternate-screen application grids (intentionally retained as grids)
- Nested tmux / SSH transport fidelity claims
- Production rich/APC product claim (Phase 0B remains experimental)
- Full xterm private-mode table on DECSTR/RIS (partial: claimed modes only — residual)

## Non-claims

- Not “modern-terminal compatible” as a blanket phrase.
- Not charter-complete VT fidelity.
- Not a rich-protocol product release.
- Not validated app-author adoption (discovery gates remain separate).

Primary resize reflow also covers authored spaces, wide-wrap padding, combining
clusters, cursor placement, copy extraction, and bounded history. The low-level
`Screen::resize` API retains clip/pad semantics; interactive emulators use
`Screen::resize_reflow`.
