# Host decision functions

PT-156 surveys pure decisions in the windowed host. Each function accepts
plain inputs and returns a decision. Keep window, mux, PTY, and framebuffer
side effects in the caller.

## Pilot extractions

| Candidate | Inputs | Return type | Branch replaced | Estimate |
| --- | --- | --- | --- | --- |
| `mux_command_plan` | `MuxCommand` | `MuxCommandPlan` | The focus-border, detach, preset, and general branches at the start of `handle_mux_command` | 20 lines |
| `wheel_input` | `MouseScrollDelta`, cell height | `WheelInput` | Line and pixel delta normalization in `window_event` | 35 lines |
| `wheel_decision` | Normalized delta, modifiers, rich-focus and hit flags, app-wheel and cursor flags, alternate-screen flag, page rows | `WheelDecision` | Rich-scroll, application-wheel, alternate-screen, and host-pan routing in `window_event` | 60 lines |
| `background_decision` | Cache dimensions and pixel count, frame dimensions, PNG availability, alpha flag | `BackgroundDecision` | Cache invalidation, rebuild, copy, alpha rewrite, and solid-fill choice in `rasterize_frame` | 45 lines |
| `blur_notice` | Requested flag, blur surface, installed state | `bool` | Startup and reload notice decision for compositor or AppKit blur | 10 lines |
| `pane_view_repaint` | Previous `(alternate-screen, scrollback)` state, current state, pane damage | `(bool, bool)` | PT-287. Marks alternate-screen transitions and scrollback changes. Steady alternate-screen output can use partial raster. No `HostState`. |
| `layout_transition` | Prior and current pane ids and geometry snapshots | `bool` | PT-289. Full-repaint guard for pane identity, topology, or geometry changes. It never keys off pane count. |
| `chrome_geometry_changed` | Prior and current bounded boxes, state markers, and animation steps | `bool` | PT-289. Detects known chip, badge, pulse, or light-cycle changes. Unbounded chrome changes use the snapshot signature and force a full repaint. |
| `compose_frame_damage` | Prior/current layout and chrome snapshots, pane row damage, full-frame flag | `FrameDamage` | PT-289. Maps cell-row damage to pixel rectangles, adds animation and focus boxes, and returns full for identity or topology changes, unbounded chrome, or the fixed chrome budget. The PT-244 buffer-age layer unions this result across aged slots. |
| `layout_snapshot` / `frame_chrome_snapshot` | Mux pane rectangles, host pixel geometry, focused pane, and animation state | Layout and chrome snapshots | PT-289. Captures pane ids and slots, measured chip boxes, pulse and light-cycle steps, and a full-repaint signature for the tab strip, spaces rail, scrollbar, and selection. Host wrappers gather `HostState`; the box, marker, and dirty-row mappers live in `frame_damage.rs`. |
| `frame_damage_intersects` | Frame damage and a pixel box | `bool` | PT-289. Decides whether a chrome-only damage box requires the pane paint path to run. |
| `pulse_phase_for_step` / `pulse_step_for_snapshot` | Quantized step, live-window flags, active count | `f32` or `Option<u8>` | PT-289. Maps pulse step `0..16` onto the paint phase. The snapshot stores the step only while an active pane is focused and unoccluded. |
| `strip_chrome_changed` | Prior and current chrome snapshots | `bool` | PT-289. True when the pulse step or a tab-strip marker changes. Pane markers do not count. |
| `append_tab_strip_markers` / `push_tab_strip_handle_boxes` / `tab_strip_badge_box` | Tab index, handle liveness, slot geometry | Markers and pixel boxes | PT-289. Bounded strip chrome. The host supplies slot bounds from mux helpers. |
| `push_pane_chrome_boxes` / `pane_chrome_bits` / `scrollbar_marker` | Slot, content, mail/unseen/active flags, scroll | Boxes and marker words | PT-289. Known pane chips and the bounded scrollbar marker. |
| `snapshot_dirty_rows` / `focus_affects_pane` | Assignment change, existing rows, focus ids, cursor rows | `Vec<usize>` | PT-289. Assignment changes dirty every row. A focus move dirty the old and new cursor rows. |
| `should_paint_tab_strip` / `composer_promotes_to_full` / `empty_partial_skips_paint` / `pane_paint_required` | Visibility, damage, and leftover pane rows | `bool` | PT-289. Rasterize-frame gates for strip paint, composer Full promotion, empty-frame skip, and chrome-only pane paint. |

Use table-driven tests for all five functions. Preserve route precedence.
The route order is rich scroll, application wheel, alternate-screen consume,
then host pan. A zero wheel delta remains consumed in the rich and application
routes. A malformed same-sized background cache fills instead of rebuilding.

## Shipped after the pilot

| Function | Inputs | Return type | Notes |
| --- | --- | --- | --- |
| `backend_supports_partial_raster` | Present backend kind and native Wayland flag | `bool` | PT-294. Softbuffer is false on native Wayland because softbuffer 0.4 rotates buffers. The current `WaylandShm` and GPU paths are false. No `HostState`. |
| `use_wayland_shm` | Alpha request and native Wayland flags | `bool` | Selects the ARGB8888 `WaylandShm` path only when both flags are true. No `HostState`. |
| `dump_present_path` | `PRISMATTYC_DUMP_PRESENT` | `Option<PathBuf>` | PT-290. Empty or unset means no dump. The host captures the path once at window create. Paint writes the same CPU slice handed to `present` as an 8-bit RGB PNG, plus a sidecar JSON with `full`, `full_repaint_reason`, and monotonic `seq`. |
| `splash_key_plan` | `Option<splash::Action>` | `SplashKeyPlan` | PT-290. Maps splash `key_action` to dismiss, quit, walkthrough, topic, or back. `dispatch_splash_key` applies it for winit keys and the e2e dismiss timer. No `HostState`. |
| `e2e_dismiss_splash_ms` | `PRISMATTYC_E2E_DISMISS_SPLASH_MS` | `Option<u64>` | PT-290. Empty, unset, zero, or non-numeric means off. When set, the host feeds one Enter through `dispatch_splash_key` after N ms. No `HostState`. |
| `e2e_dismiss_due` | elapsed ms, deadline ms | `bool` | PT-290. True when elapsed is at or past the deadline. No `HostState`. |
| `present_png_empty` | width, height | `bool` | PT-290. True when either dimension is zero. No `HostState`. |
| `present_png_parent` | dump path | `Option<&Path>` | PT-290. Directory to create; empty parent is `None`. No `HostState`. |
| `a11y::build_chrome_tree` | `ChromeSnapshot` | `(focus id, Vec<TreeNode>)` | PT-173. Window, tabs, panes, overlays, scrollbar, space rail. No `HostState`. `action_for` maps a node id to a chrome action. |
| `a11y::viewport_document` | viewport lines, optional cursor char index, optional selection char indexes | `DocumentSnap` | PT-174. Row-major plain text, caret offset, selection offsets. Map cells with `chars_before_cell` first. No `HostState`. |
| `a11y::announce_decision` | `AnnounceFacts`, `AnnounceMemory` | `(Option<LiveSnap>, AnnounceMemory)` | PT-175. Mail and attention win (assertive, may join). Then selection (polite). Then cursor line on pane or row change, coalesced to 400 ms. Identical consecutive utterances toggle a trailing U+200B. `announce = false` returns no utterance. The live node stays in the tree with an empty value when silent. No `HostState`. |
| `mouse_input_decision` | Button, element state, Shift, tracking mode, guest-alt policy, cursor focus, left-button state | `MouseInputDecision` | PT-162. Rich, URL, tracking, guest-alt, and host selection. Guest-alt block requires no Shift. Effects stay at the call site. No `HostState`. |
| `footer_visibility` | Control and Shift flags, optional linger deadline, current time | `bool` | PT-163. True while Ctrl+Shift is held, or while `now` is strictly before the linger deadline. No `HostState`. |
| `overlay_paint_decision` | Focus, live-view, cursor, preedit, IME modal, footer, scroll-chip, find, and palette flags | `OverlayPaintDecision` | PT-164. Routes input overlay choice and independent scroll-chip, find-prompt, and palette paint gates. No `HostState`. |
| `overlay_clip` | Pane rectangle, footer rows, cell height, window height, overlay geometry | `Option<ClipRect>` | PT-165. Intersect the overlay with the pane after a bottom footer reserve. Empty when the overlay misses the usable pane. No `HostState`. |
| `startup_attach_plan` | Registered host ownership and explicit attach-target presence | `StartupAttachPlan` | PT-178. Bare launches do not read shared attach grouping; unregistered explicit targets attach one per target; the registered owner may restore cached grouping. No `HostState`. |
| `startup_attach_targets` | First-window state and process CLI attach targets | Attach-target slice | PT-178 review. Process CLI attach targets apply only to the first window. Later `NewWindow` windows start with one local shell. No `HostState`. |
| `startup_window_plan` | First-window state, registered ownership, and explicit-target presence | `StartupWindowPlan` | PT-178 review. Later in-process windows are bare and never write the shared cache. The first registered window keeps owner-gated restore and persistence. No `HostState`. |
| `resolve_editor_command` | `$VISUAL`, `$EDITOR` command strings | `EditorCommand` | PT-179. Chooses the first valid non-empty editor without invoking a shell. No `HostState`. |
| `move_pane_eligibility` | Optional session name, optional current space, target space name | `MovePaneEligibility` | PT-182. Local shell (no session) and same-space target are no-ops. `Move` means the host may add, drop, and rewrite. No `HostState`. |
| `target_space_has_live_host` | Live registered-host flag, optional space name in the attach-tabs cache, target space name | `bool` | PT-182. True only when a live registered host already has the target open. The host then reuses `pmux space open --no-attach`. Otherwise the pane waits until that space is opened. No `HostState`. |
| `can_drop_focused_pane` | Active pane count, tab count, placeholder flag | `bool` | PT-182. Same gate as `close_focused`: drop only when another pane, another tab, or a placeholder remains. |
| `classify_current_space_load` | Named-current flag and optional `(in current, count)` load | `CurrentSpaceProbe` | PT-182. A named current space whose file cannot be read is `Unreadable`. Do not treat a load error as "session not in the space". No `HostState`. |
| `move_pane_preflight` | Can-drop flag and `CurrentSpaceProbe` | `MovePanePreflight` | PT-182. Refuses the last pane of the last tab, an unreadable current space file, and the last session in the current space before any file edit. |
| `drop_failed_toast` | Rollback success flag, target name, session name | toast text | PT-182. A failed drop says the files are unchanged only when the target add was undone. A failed undo names `pmux space remove`. |
| `title_row_decision` | Pane-titles mode, handle count, tab title, focused OSC title, handle titles, hover handle, live notice | `TitleRowDecision` | PT-190. Hover wins, then an unfocused-pane notice, then the focused pane title when the mode allows it, then the tab name. No `HostState`. |
| `title_notice_from_diff` | Previous handle titles, current handle titles, focused handle per tab | `Option<TitleNotice>` | PT-190. First snapshot is silent. A later change on an unfocused handle returns that title. No `HostState`. |
| `pulse_mix` | Base RGB, toward RGB, pulse phase | `[u8; 3]` | PT-191. Sine brightness 0.6..=1.0. Active handle chips mix toward `active_badge`. The tab badge uses the same curve from `default_bg`. No `HostState`. |
| `pane_working_name` | Pane title and working flag | `String` | PT-191. Busy panes append `, working` so the chrome tree matches the breathing chip. No `HostState`. |
| `parse_catalog` / `validate_catalog` | Walkthrough TOML text or a parsed `Catalog` | `Result<Catalog, CatalogError>` | PT-192. Unique IDs, one expect per step, `show_me` matches the detector, short captions. Unknown event kinds fail at parse. No `HostState`. |
| `caption_view` | Step and live keymap chord | `CaptionView` | PT-193. Line 1 is the caption. Line 2 prefers the live chord, then `command`, then `hint`. No `HostState`. |
| `caption_band` | Pane rectangle, cell size, window pad, and `CaptionView` | `Option<CaptionBand>` | PT-193. Centers a two-line subtitle inside the pane. `[show me]` and `[skip]` sit on the right of line two so a shorter next chord cannot slide `[skip]` under the pointer. Does not change pane geometry. No `HostState`. |
| `caption_hit` | Caption band and pointer | `Option<CaptionHit>` | PT-193. Only `×`, `[show me]`, and `[skip]` consume the click. The rest of the band falls through. Hit-test at press time from the pointer, not the last hover target. No `HostState`. |
| `caption_press_px` | Optional pointer `(f64, f64)` | `Option<(usize, usize)>` | PT-295. Finite, non-negative coordinates truncate toward zero. `None`, NaN, infinities, and negative axes miss. No `HostState`. |
| `caption_click_result` | Pointer, previous control press, elapsed time, optional hit | `CaptionClickResult` | PT-295. Miss, consumed repeat, or dispatch. Repeat wins over hit. No `HostState`. |
| `caption_rect_json` | Optional `CaptionRect` | `String` | PT-295. Exact `{"x","y","w","h"}` object, or `null`. No `HostState`. |
| `caption_repeat_click` | Previous control press pixel, elapsed time, current pixel | `bool` | True when this press is a bounce or double-click on the same control. The caller consumes it and must not dispatch Show me or Skip. 500 ms window, 8 px slop. No `HostState`. |
| `caption_escape_decision` | Caption visible flag | `CaptionEscape` | PT-193 review. Escape dismisses a showing caption with no hover. Escape passes through when no caption is showing. No `HostState`. |
| `caption_paint_decision` | Session caption-visible flag and overlay-open flag | `bool` | PT-193 review. Palette, theme picker, find, and space picker hide the caption. The step stays armed. No `HostState`. |
| `space_open_report` | Mode, previous space, target, detaching count, opened sessions/tabs | `String` | PT-213. One-line stdout after `pmux space open` writes the cache. Switch says `detaching` because the host acts after the line. Lives in `prismattyc-mux::attach_tabs`. No `HostState`. |
| `space_save_span_warning` | Cache space, live session names, space membership lists | `Option<String>` | PT-213. Warns from the live window: a live session not in `cache.space`, or no cache space and sessions map to more than one file. Shared seats across files do not warn. Lives in `prismattyc-mux::attach_tabs`. No `HostState`. |
| `keep_caller_session` | Attach-tabs file, caller session id and name | `bool` | PT-213. Switch keeps the pane that ran `pmux space open` by appending a tab. No `HostState`. |
| `cache_sessions_in_tab_order` | Attach-tabs file and id→name pairs | `Vec<String>` | PT-213. Live-window session names for a no-list `space save`. Unknown ids skipped. No `HostState`. |
| `merge_add_cache` | Previous cache, incoming space file, live session ids | `AttachTabsFile` | PT-213. `--add` union: keep previous live tabs that the new space does not name, then append the new tabs. No `HostState`. |
| `space_open_cli_args` | Space name and `SpaceOpenMode` | `Vec<String>` | PT-213. Chip click is switch (`pmux space open NAME --no-attach`). Add and new-window pass those flags. No `HostState`. |
| `remote_size_chip` | Size owner, this client's id, replica size | `Option<(u32, u32)>` | PT-202. Show `remote WxH` when another client owns the size. The server is the source of truth. No `HostState`. |
| `resize_decision` | Policy, role, reported size, fit flag, host-chosen size, applied cells | `Option<Viewport>` | PT-200 review. Same applied cells return `None`. No `HostState`. |
| `remember_host_size` | Previous host-chosen, requested cells, replica cells | `Option<(u32, u32)>` | PT-200 review. An echo of the replica does not replace host-chosen. No `HostState`. |
| `record_host_chosen` | Reported cells, applied cells, remote-latest flag | `bool` | PT-200 review. Host-tagged Resize must not store the remote-applied size. No `HostState`. |
| `palette_geom` | Window cols, window rows, cell height | `PaletteGeom` | PT-201. An 80×24 or smaller window keeps today's compact panel (max 90 cells, 19-cell name, row pitch = `cell_h`). A larger window grows to `min(120 cells, 85 % of the window)` and never shrinks below the compact width. The name column grows to 30 cells at full width. Query row is `cell_h + 8`. List pitch is `cell_h + 6`. No `HostState`. |
| `palette_scroll_for_selection` | Current scroll, selected line, visible lines, total lines | `usize` | PT-201 review. Keeps scroll unless the selected line left the visible window. Hover does not jump the list. No `HostState`. |
| `palette_hit` | `PaletteLayout` and pointer | `Option<usize>` | PT-201. The hit band is the row pitch. Keyboard and a11y still select by name. No `HostState`. |
| `detect_step` | Catalog `Expect` and a `Detected` host, mux, space, or command fact | `DetectOutcome` | PT-194. Match advances. Same action or command with a non-ok result fails. Other facts ignore. Lives in `prismattyc-mux`. No `HostState`. |
| `mux_detected` | Event kind and optional window/session predicates | `Detected` | PT-194. Builds a mux fact for `SessionCreated`, `PaneMoved`, and the other control-event kinds. Lives in `prismattyc-mux`. No `HostState`. |
| `progress_path_from` | `XDG_DATA_HOME` and `HOME` | `PathBuf` | PT-195. `$XDG_DATA_HOME/prismattyc/walkthrough.json`, else `~/.local/share/prismattyc/walkthrough.json`. Lives in `prismattyc-mux`. No `HostState`. |
| `resume_index` | Catalog and `Progress` | `Option<(usize, usize)>` | PT-195. First step that is neither completed nor skipped. None when every step is recorded. Lives in `prismattyc-mux`. No `HostState`. |
| `Cursor::resume` | Catalog, optional `Progress`, optional level id | `Option<Cursor>` | PT-196. Shared progress cursor for the host session and `pmux tutorial --play`. No `HostState`. |
| `play_decision` | Audio enabled, clip present, time since last play | `PlayDecision` | PT-197. Skip when audio is off, the clip is missing, or the last play was under one second ago. No `HostState`. |
| `players_for` | Player list and clip path | Filtered player names | PT-197. OGG drops `aplay`. No `HostState`. |
| `boss_matches` | Target `SavedSpace` and live `SavedSpace` | `BossVerdict` | PT-198. Session count, tab count, and panes-per-tab multiset. Order-free. Names, cwd, agents, and pids are ignored. No `HostState`. |
| `save_space_modal_open` | Optional rail edit | `bool` | #364. True only when the `+` / `save_space` name prompt is open (`target` is `None`). Inline rename stays on the chip. The host uses this flag as a transient overlay so typing forces a full frame. No `HostState`. |

## PT-237 dispatcher extractions

These decisions keep effects in the caller. The table tests cover the extracted
routes, invalid indices, orientations, modifiers, pointer targets, and button
cases listed below. The refactor makes no behavior or performance claim.

| Function | Inputs | Return type | Preserved rule |
| --- | --- | --- | --- |
| `mux_command_plan` | `MuxCommand` | `MuxCommandPlan` | Keep focus-border, detach, preset, layout, navigation, tab, and rename routing unchanged. |
| `action_route` | `keybind::Action` | `ActionRoute` | Keep host actions separate from mux commands. Preserve scroll direction and palette actions. |
| `context_menu_choice` | `ContextMenuKind`, row index | `ContextMenuChoice` | Preserve row order. Return `Noop` for an invalid row. |
| `context_menu_action` | menu target, row index | `ContextMenuAction` | Preserve target identity while mapping a valid row. Return `Noop` for an invalid row. |
| `context_menu_needs_confirmation` | `ContextMenuKind`, row index, confirmed flag | `bool` | Confirm only space save and delete actions. |
| `space_rail_key_decision` | active flag, rail side, modifiers, logical key | `SpaceRailKeyDecision` | Leave on hidden rails or host chords. Preserve horizontal and vertical arrow routing. |
| `strip_click_hit` | mux strip hit | `StripClickHit` | Preserve tab, pane, and empty-end hit identity. |
| `strip_click_decision` | mouse button, strip hit, title-row flag, rename flag, pointer position | `StripClickDecision` | Preserve close, drag, rename, middle-click, empty-end, and fall-through behavior. |

For PT-237, use the existing spaces e2e and golden frames as the unchanged-
behavior seam. Do not add a helper action around the tested input.


## PT-294 validation boundary

The pure policy test covers native Wayland and non-Wayland Softbuffer. The
PT-283 Xvfb fixture covers the non-Wayland present path. The Wayland box
job's second weston run (`window_opacity = 1.0`, blur off) is the
compositor proof: startup is not `wl_shm`, then dismiss splash, type,
and assert no splash pixels across two presents.

## Design rules

1. Keep pure functions private to `prismattyc-host` unless another crate needs the decision.
2. Pass derived plain values into a helper. Do not pass `HostState`.
3. Return routing or paint decisions. Keep effects at the call site.
4. Add a table row when you add a new route or command variant.
5. Preserve the existing precedence and zero-value behavior.
