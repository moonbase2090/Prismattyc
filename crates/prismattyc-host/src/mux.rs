//! Multi-pane runtime binding for the windowed host.
#[path = "mux_local.rs"]
mod local;
pub(crate) use local::Recipe as LocalRecipe;

use crate::space_rail::RailSide;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::PtySize;
use prismattyc_core::Selection;
use prismattyc_emulator::{Emulator, PtySession};
use prismattyc_mux::{
    apply_arrangement, even_horizontal_row, even_two_row_grid, even_vertical_column,
    layout_to_rects, Arrangement, Axis, CellRect, ClientView, Domain, PaneId, PaneLayout,
    SessionId, SizeOwner, WindowId, DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS,
};
use prismattyc_protocol::{InputModifiers, PointerPhase, ViewerId};

use crate::attach_log::{self, LogMessage};
use crate::rich::{self, CapabilityGrant, ChildWrite, RichSession};

const FROM_PTY_CAP: usize = 64;
const TO_CHILD_CAP: usize = 32;
const MAX_PTY_DRAIN_PER_PANE: usize = 8;
/// Wakes the winit loop when a pane reader has bytes (or the child exits).
/// Optional so mux unit tests can spawn without an event loop.
pub(crate) type Wake = Arc<dyn Fn() + Send + Sync>;
/// How long after its last visible output a pane still counts as "active".
pub(crate) const ACTIVE_WINDOW: Duration = Duration::from_millis(1500);
/// Quiet gap before an unfocused pane's next output (or silence) badges.
pub(crate) const QUIET_GAP: Duration = Duration::from_secs(3);

/// Attention-based unseen badge. Focused panes never badge.
fn apply_unseen_v2(
    unseen: &mut bool,
    last_output_at: &mut Option<Instant>,
    finish_watch: &mut bool,
    now: Instant,
    focused: bool,
    content_changed: bool,
    bell: bool,
) {
    if focused {
        *unseen = false;
        *finish_watch = false;
        if content_changed {
            *last_output_at = Some(now);
        }
        return;
    }
    if bell {
        *unseen = true;
    }
    if content_changed {
        let quiet = last_output_at.is_none_or(|at| now.saturating_duration_since(at) >= QUIET_GAP);
        if quiet {
            *unseen = true;
        }
        *last_output_at = Some(now);
        return;
    }
    if *finish_watch
        && last_output_at.is_some_and(|at| now.saturating_duration_since(at) >= QUIET_GAP)
    {
        *unseen = true;
        *finish_watch = false;
    }
}
const MAX_SCROLLBACK: usize = 10_000;

/// Pixel geometry shared by paint, hit-testing, and PTY sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostGeom {
    pub cell_w: usize,
    pub cell_h: usize,
    /// Window edge to pane slot/chrome.
    pub window_pad: usize,
    /// Inter-pane gap. This is zero for a single-pane layout.
    pub pane_gap: usize,
    /// Inter-tab gap. Configured `pane_gap_px` even when `pane_gap` is 0
    /// (single-pane active tab). Rail paint and hit-test use this only.
    pub rail_gap: usize,
    /// Pane slot/chrome to terminal cells and tab content.
    pub inner_pad: usize,
    /// Reserved top strip when `tab_count > 1`. Zero on the single-tab path.
    pub top_chrome_px: usize,
    /// Right gutter reserved for the host scrollbar (PT-80). Subtracted from
    /// the content box before PTY cols. Zero when `inner_pad` already fits
    /// `SCROLLBAR_GUTTER_PX` (the bar sits in the right padding).
    pub scrollbar_gutter_px: usize,
    /// Horizontal shift applied to every pane origin so the pixels the cell
    /// grid cannot fill are split between both edges instead of piling on the
    /// right. Set by `refit_geom`; zero until the window size is known.
    pub slack_x: usize,
    /// Vertical counterpart of [`Self::slack_x`].
    pub slack_y: usize,
    /// Spaces rail edge (PT-91). `Off` reserves nothing.
    pub rail_side: RailSide,
    /// Reserved thickness of the spaces rail: a row height on the bottom or
    /// top edge, `rail_chip_cols` cells on the left or right edge.
    pub rail_px: usize,
    /// Fixed chip width of the spaces rail in cells.
    pub rail_chip_cols: usize,
}

/// Overlay width of the host scrollback scrollbar (matches raster).
pub(crate) const SCROLLBAR_GUTTER_PX: usize = 8;

/// Extra content-box pixels for the bar when right padding cannot hold it.
///
/// `pane_gap` sits between slots (half on each side), so it does not host
/// the bar at the slot's right edge. Only `inner_pad >= SCROLLBAR_GUTTER_PX`
/// keeps PTY columns unchanged.
pub(crate) fn scrollbar_gutter_for(inner_pad: usize) -> usize {
    if inner_pad >= SCROLLBAR_GUTTER_PX {
        0
    } else {
        SCROLLBAR_GUTTER_PX
    }
}

/// Palette / `[keys]` retile of the current tab's existing panes (PT-70).
/// None of these spawn or close a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutPreset {
    Single,
    SplitH,
    SplitV,
    Grid,
    MainVertical,
    MainHorizontal,
}

/// Outcome of [`MuxRuntime::apply_preset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetOutcome {
    Applied,
    /// Layout was left unchanged; show the string as a status message.
    Unchanged(&'static str),
}

/// Attach pane after its child exited (PT-68).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placeholder {
    pub reason: String,
    pub gone: bool,
}

/// Result of `MuxRuntime::detach_view`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetachView {
    /// Another tab remains; this view is gone.
    ClosedTab,
    /// Last tab. Caller must exit the host window.
    ExitHost,
}

/// Compact per-tab strip model for the windowed host (ADR-0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabInfo {
    pub title: String,
    pub selected: bool,
    pub unseen: bool,
    pub active: bool,
    pub attention: bool,
    /// The client-local zoom (PT-57) hides this tab's other panes.
    pub zoomed: bool,
    /// Pane handles ([`pane_handle_w`] wide) at the left of the chip when the tab has
    /// more than one pane (PT-69). Zero means the chip is the only handle.
    pub handles: usize,
    /// Handle index for the focused pane, when this tab has pane handles.
    pub focused_handle: Option<usize>,
    /// Hover label per handle (PT-148/PT-160): the pane title, else the
    /// attach name, else the attach session, else `pane N`. Same length as
    /// `handles`.
    pub handle_titles: Vec<String>,
    /// Per-handle live output (`is_active()`). Same length as `handles`.
    pub handle_active: Vec<bool>,
    /// Focused pane OSC title when set (PT-148 / PT-190). Single-pane tabs
    /// always carry it; selected multi-pane tabs carry the focused pane.
    pub pane_title: Option<String>,
    pub git_label: Option<String>,
}

/// Normalise an OSC 0/2 title into a pane title (PT-148). pmux-attach sends
/// `pmux: NAME` when nothing is set and appends ` — N mail` while letters
/// wait; both are chrome, not a title.
/// Whether an OSC title replaces the pane's current title: never while the
/// user's rename is pinned, else only when it differs (PT-221).
pub(crate) fn accept_osc_title(
    pinned: bool,
    current: &Option<String>,
    incoming: &Option<String>,
) -> bool {
    !pinned && incoming != current
}

pub(crate) fn pane_title_from_osc(text: &str) -> Option<String> {
    let mut text = text.trim();
    if let Some(idx) = text.rfind(" — ") {
        let tail = &text[idx + " — ".len()..];
        if let Some(count) = tail.strip_suffix(" mail") {
            if count.parse::<u32>().is_ok() {
                text = text[..idx].trim_end();
            }
        }
    }
    if text.is_empty() || text == "pmux-attach" || text.starts_with("pmux: ") {
        return None;
    }
    Some(text.to_string())
}

/// Hit in the tab strip (PT-69).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripHit {
    /// Tab chip body, or its close glyph.
    Tab { index: usize, close: bool },
    /// One-cell handle for a pane in a multi-pane tab.
    Pane {
        tab: usize,
        pane: PaneId,
        handle: usize,
    },
    /// Trailing empty drop target (new tab / move tab to end).
    EmptyEnd,
}

/// One draggable gap between the two halves of a split (PT-133).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Divider {
    /// Path from the window's root: `false` = first child, `true` = second.
    pub path: Vec<bool>,
    pub axis: Axis,
    /// Bounding cells of the whole split node.
    pub bounds: CellRect,
    /// First cell column (Horizontal) or row (Vertical) of the second half.
    pub boundary: usize,
}

impl HostGeom {
    /// Pixel band of a divider: the gap between the two slots, widened by
    /// `slop` on each side so a zero-gap layout is still grabbable.
    /// Returns `(x, y, w, h)`.
    pub(crate) fn divider_px(self, divider: &Divider, slop: usize) -> (usize, usize, usize, usize) {
        let (bx, by, bw, bh) = self.pane_slot_px(divider.bounds);
        match divider.axis {
            Axis::Horizontal => {
                let second = CellRect {
                    col: divider.boundary,
                    row: divider.bounds.row,
                    cols: 1,
                    rows: 1,
                };
                let (sx, _, _, _) = self.pane_slot_px(second);
                // The first slot ends `pane_gap` before the second begins.
                let gap_start = sx.saturating_sub(self.pane_gap);
                (
                    gap_start.saturating_sub(slop),
                    by,
                    self.pane_gap.saturating_add(slop.saturating_mul(2)),
                    bh,
                )
            }
            Axis::Vertical => {
                let second = CellRect {
                    col: divider.bounds.col,
                    row: divider.boundary,
                    cols: 1,
                    rows: 1,
                };
                let (_, sy, _, _) = self.pane_slot_px(second);
                let gap_start = sy.saturating_sub(self.pane_gap);
                (
                    bx,
                    gap_start.saturating_sub(slop),
                    bw,
                    self.pane_gap.saturating_add(slop.saturating_mul(2)),
                )
            }
        }
    }

    /// Fraction of the split node the pointer sits at along the divider's
    /// axis, in `0.0..=1.0`.
    pub(crate) fn divider_ratio_at(self, divider: &Divider, px: usize, py: usize) -> f64 {
        // Measure over the node's full cell extent (slots trim half a gap
        // on each side), so the ratio maps 1:1 onto the layout's cells.
        let (bx, by, _, _) = self.pane_slot_px(divider.bounds);
        let lead = self.pane_gap / 2;
        let (pos, len) = match divider.axis {
            Axis::Horizontal => (
                px.saturating_sub(bx.saturating_sub(lead)),
                divider.bounds.cols.saturating_mul(self.cell_w),
            ),
            Axis::Vertical => (
                py.saturating_sub(by.saturating_sub(lead)),
                divider.bounds.rows.saturating_mul(self.cell_h),
            ),
        };
        if len == 0 {
            return 0.5;
        }
        (pos as f64 / len as f64).clamp(0.0, 1.0)
    }

    /// Exact cell geometry used by mux-only tests and non-windowed callers.
    #[cfg(test)]
    pub(crate) fn tight(cell_w: usize, cell_h: usize) -> Self {
        Self {
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            window_pad: 0,
            slack_x: 0,
            slack_y: 0,
            pane_gap: 0,
            rail_gap: 0,
            inner_pad: 0,
            top_chrome_px: 0,
            scrollbar_gutter_px: 0,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        }
    }

    /// Vertical origin of the tab strip, below a top Spaces rail.
    pub(crate) fn tab_strip_y(self) -> usize {
        if self.rail_side == RailSide::Top {
            self.rail_px
        } else {
            0
        }
    }

    /// Chrome reserved above the pane area: top rail, then tab strip.
    pub(crate) fn chrome_top(self) -> usize {
        let rail = if self.rail_side == RailSide::Top {
            self.rail_px
        } else {
            0
        };
        self.top_chrome_px.saturating_add(rail)
    }

    /// Chrome reserved below the pane area (a bottom rail).
    pub(crate) fn chrome_bottom(self) -> usize {
        if self.rail_side == RailSide::Bottom {
            self.rail_px
        } else {
            0
        }
    }

    /// Chrome reserved left of the pane area (a left rail).
    pub(crate) fn chrome_left(self) -> usize {
        if self.rail_side == RailSide::Left {
            self.rail_px
        } else {
            0
        }
    }

    /// Chrome reserved right of the pane area (a right rail).
    pub(crate) fn chrome_right(self) -> usize {
        if self.rail_side == RailSide::Right {
            self.rail_px
        } else {
            0
        }
    }

    fn horizontal_inset(self) -> usize {
        self.inner_pad
            .saturating_mul(2)
            .saturating_add(self.pane_gap)
            .saturating_add(self.scrollbar_gutter_px)
    }

    fn vertical_inset(self) -> usize {
        self.inner_pad
            .saturating_mul(2)
            .saturating_add(self.pane_gap)
    }

    fn cells_for_pixels(pixels: usize, cell: usize) -> usize {
        pixels.saturating_add(cell.saturating_sub(1)) / cell.max(1)
    }

    fn min_cols(self) -> usize {
        DEFAULT_MIN_COLS
            .saturating_add(Self::cells_for_pixels(self.horizontal_inset(), self.cell_w))
    }

    fn min_rows(self) -> usize {
        DEFAULT_MIN_ROWS.saturating_add(Self::cells_for_pixels(self.vertical_inset(), self.cell_h))
    }

    /// Full slot for a layout cell rectangle, including chrome.
    pub(crate) fn pane_slot_px(self, rect: CellRect) -> (usize, usize, usize, usize) {
        let leading_gap = self.pane_gap / 2;
        let x = self
            .window_pad
            .saturating_add(self.slack_x)
            .saturating_add(self.chrome_left())
            .saturating_add(rect.col.saturating_mul(self.cell_w))
            .saturating_add(leading_gap);
        let y = self
            .window_pad
            .saturating_add(self.slack_y)
            .saturating_add(self.chrome_top())
            .saturating_add(rect.row.saturating_mul(self.cell_h))
            .saturating_add(leading_gap);
        let width = rect
            .cols
            .saturating_mul(self.cell_w)
            .saturating_sub(self.pane_gap);
        let height = rect
            .rows
            .saturating_mul(self.cell_h)
            .saturating_sub(self.pane_gap);
        (x, y, width, height)
    }

    /// Space inside a pane slot available to the terminal, before rounding
    /// down to whole cells.
    fn pane_avail_px(self, rect: CellRect) -> (usize, usize, usize, usize) {
        let (x, y, width, height) = self.pane_slot_px(rect);
        (
            x.saturating_add(self.inner_pad),
            y.saturating_add(self.inner_pad),
            width.saturating_sub(self.inner_pad.saturating_mul(2)),
            height.saturating_sub(self.inner_pad.saturating_mul(2)),
        )
    }

    /// Terminal content box inside a pane slot: exactly whole cells, centred
    /// in the space available.
    ///
    /// Rounding matters. `content_cells` floor-divides, so a box sized to the
    /// raw available space leaves up to `cell_h - 1` pixels that no glyph row
    /// ever paints — and the pane backdrop painted underneath shows through
    /// them as a band in a colour the cells never use. Whole cells only, and
    /// the surplus becomes padding split evenly on both sides.
    pub(crate) fn pane_content_px(self, rect: CellRect) -> (usize, usize, usize, usize) {
        let (x, y, avail_w, avail_h) = self.pane_avail_px(rect);
        let (cols, rows) = self.content_cells(rect);
        let used_w = cols.saturating_mul(self.cell_w);
        let used_h = rows.saturating_mul(self.cell_h);
        // The scrollbar gutter stays on the right; it is not free space.
        let extra_w = avail_w
            .saturating_sub(self.scrollbar_gutter_px)
            .saturating_sub(used_w);
        let extra_h = avail_h.saturating_sub(used_h);
        (
            x.saturating_add(extra_w / 2),
            y.saturating_add(extra_h / 2),
            used_w,
            used_h,
        )
    }

    /// PTY dimensions that fit wholly inside the pane content box.
    pub(crate) fn content_cells(self, rect: CellRect) -> (usize, usize) {
        let (_, _, width, height) = self.pane_avail_px(rect);
        let width = width.saturating_sub(self.scrollbar_gutter_px);
        (
            (width / self.cell_w.max(1)).max(DEFAULT_MIN_COLS),
            (height / self.cell_h.max(1)).max(DEFAULT_MIN_ROWS),
        )
    }

    /// Last cell's right edge in pixels, then the gutter, must fit in the slot.
    #[cfg(test)]
    pub(crate) fn cells_fit_left_of_gutter(self, rect: CellRect) -> bool {
        let (slot_x, _, slot_w, _) = self.pane_slot_px(rect);
        let (cx, _, _, _) = self.pane_content_px(rect);
        let (cols, _) = self.content_cells(rect);
        let last_cell_right = cx.saturating_add(cols.saturating_mul(self.cell_w));
        let bar_left = slot_x
            .saturating_add(slot_w)
            .saturating_sub(SCROLLBAR_GUTTER_PX.min(slot_w));
        last_cell_right <= bar_left
    }

    /// Track box for the host scrollbar: right `SCROLLBAR_GUTTER_PX` of the slot,
    /// vertically aligned with the content box.
    pub(crate) fn scrollbar_px(self, rect: CellRect) -> (usize, usize, usize, usize) {
        let (slot_x, _, slot_w, _) = self.pane_slot_px(rect);
        let (_, content_y, _, content_h) = self.pane_content_px(rect);
        let bar_w = SCROLLBAR_GUTTER_PX.min(slot_w);
        (
            slot_x.saturating_add(slot_w.saturating_sub(bar_w)),
            content_y,
            bar_w,
            content_h,
        )
    }
}

/// What a [`PaneRuntime`] reads its content from.
pub(crate) const EMPTY_SPACE_PROGRAM: &str = "__pmux_empty_space_view__";

enum Backing {
    Empty,
    Pty {
        session: PtySession,
        from_pty_rx: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    },
    Log(attach_log::LogPane),
}

/// Marker inherited by nested `pmux-attach` (PT-208). Attach suppresses the
/// space/session identity overlay when this is set; the host rail and tab
/// title already show those names.
fn host_pane_env() -> BTreeMap<String, String> {
    BTreeMap::from([(String::from("PRISMATTYC_HOST"), String::from("1"))])
}

/// Resolve a `pmux attach --session-id ID` spawn to a live pane log, unless
/// `PRISMATTYC_ATTACH_PTY=1` asks for the pre-PT-111 nested child.
fn log_backed_attach(
    program: &str,
    child_args: &[String],
    space_id: Option<&str>,
) -> Result<Option<attach_log::LogConnection>> {
    let Some(session_key) = attach_log::attach_target(program, child_args) else {
        return Ok(None);
    };
    if let Some(index) = child_args.iter().position(|arg| arg == "--host-pane-id") {
        let pane = child_args
            .get(index + 1)
            .context("missing target pane")?
            .parse()?;
        let pid = child_args
            .get(index + 2)
            .context("missing target process")?
            .parse()?;
        let connection = attach_log::LogConnection::exact_pane(&session_key, pane, pid)?;
        if let Some(owner) = space_id {
            connection.require_space(owner)?;
        }
        return Ok(Some(connection));
    }
    if attach_log::pty_fallback_requested() && space_id.is_none() {
        return Ok(None);
    }
    match attach_log::LogConnection::open(&session_key) {
        Ok(connection) => {
            if let Some(owner) = space_id {
                connection.require_space(owner)?;
            }
            Ok(Some(connection))
        }
        Err(error) if space_id.is_some() => Err(error),
        Err(error) => {
            eprintln!("prismattyc-host: subscribe session {session_key} failed: {error:#}");
            Ok(None)
        }
    }
}

/// One leaf's independent PTY/emulator and client-local view state.
///
/// Two backings (PT-111). A **PTY pane** owns `session` and reads bytes off
/// `from_pty_rx`. A **log-backed pane** owns `log` instead: it subscribes to
/// a pmuxd pane event log and feeds the same emulator, so an attached mux
/// session no longer costs a nested `pmux attach` child and a second
/// emulator. Both write input through `to_child_tx`.
pub(crate) struct PaneRuntime {
    pub(crate) emulator: Emulator,
    session: Option<PtySession>,
    from_pty_rx: Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    /// Set on a log-backed attach pane. Mutually exclusive with `session`.
    log: Option<attach_log::LogPane>,
    /// Exit text from a log `Exited` event, for the PT-68 placeholder.
    log_exit_reason: Option<String>,
    /// Toast after a log-backed writer dies. Pane stays read-only.
    log_write_notice: Option<String>,
    /// Host-side policy diagnostics for frames superseded before delivery.
    policy_superseded_frames: usize,
    policy_superseded_bytes: usize,
    policy_batches: usize,
    policy_frames: usize,
    pub(crate) to_child_tx: mpsc::SyncSender<ChildWrite>,
    grant_rx: mpsc::Receiver<CapabilityGrant>,
    pub(crate) rich: RichSession,
    rich_viewer_id: ViewerId,
    experimental_rich: bool,
    pub(crate) cols: usize,
    /// Full pane content height before a rich workspace reservation.
    pub(crate) outer_rows: usize,
    /// Current guest PTY height after subtracting a granted workspace.
    pub(crate) rows: usize,
    pub(crate) selection: Selection,
    pub(crate) keyboard_select_mode: bool,
    /// Rows above live bottom (primary history). Zero is the live view.
    pub(crate) view_scroll: usize,
    /// Live bottom moved while this pane is scrolled (chip `· new`).
    pub(crate) scroll_new_output: bool,
    pub(crate) last_content_epoch: u64,
    pub(crate) child_alive: bool,
    /// Pane title from the child's OSC 0/2 (PT-148): a `pmux rename-pane`
    /// title or guest status relayed by pmux-attach, or a local shell's own
    /// title. Shown on the strip handle hover.
    pub(crate) title: Option<String>,
    /// The user renamed this pane (PT-221). While set, OSC 0/2 titles from
    /// the child are ignored, like tmux `allow-rename off`; an empty rename
    /// clears it and the child's title shows again.
    pub(crate) title_pinned: bool,
    /// Mux session this pane attached, if any (PT-68). Local shells are None.
    attach_session: Option<String>,
    attach_name: Option<String>,
    /// Shown after an attach child exits; the layout slot stays.
    placeholder: Option<Placeholder>,
    /// Explicit local terminal; do not discard it as a launch placeholder.
    keep_local: bool,
    /// New terminal content arrived while another pane was focused.
    pub(crate) unseen_output: bool,
    /// MailAttention depth. Independent of `unseen_output`.
    pub(crate) mail_depth: u32,
    /// When visible content last changed, on any pane (focused included).
    pub(crate) last_output_at: Option<Instant>,
    /// Unfocused pane was streaming when focus left; badge once it goes quiet.
    finish_watch: bool,
    /// Latest agent-attention message for this pane until it is focused.
    pub(crate) attention: Option<String>,
    /// Server-reported size owner for the chip (PT-202).
    pub(crate) size_owner: Option<SizeOwner>,
    /// Cell pixel size reported on the PTY (`ws_xpixel`/`ws_ypixel` are the
    /// full window in pixels = cols*cell_w, rows*cell_h). Zero used to make
    /// Kitty-graphics producers emit tiny bitmaps.
    cell_w: usize,
    cell_h: usize,
}

impl PaneRuntime {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        pane: PaneId,
        program: &str,
        child_args: &[String],
        cols: usize,
        rows: usize,
        cwd: Option<&Path>,
        experimental_rich: bool,
        wake: Option<Wake>,
        cell_w: usize,
        cell_h: usize,
        space_id: Option<&str>,
    ) -> Result<Self> {
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);
        if program == EMPTY_SPACE_PROGRAM {
            let (tx, _rx) = mpsc::sync_channel(1);
            let (_grant_tx, grant_rx) = mpsc::channel();
            let mut runtime = Self::assemble(
                Backing::Empty,
                tx,
                grant_rx,
                cols,
                rows,
                experimental_rich,
                cell_w,
                cell_h,
            )?;
            runtime.child_alive = false;
            runtime.placeholder = Some(Placeholder {
                reason: "empty space".into(),
                gone: false,
            });
            return Ok(runtime);
        }
        // PT-111: `pmux attach --session-id ID` becomes a log subscription,
        // not a PTY child. Any failure here keeps the old nested child.
        if let Some(connection) = log_backed_attach(program, child_args, space_id)? {
            match Self::spawn_log_backed(
                pane,
                connection,
                cols,
                rows,
                experimental_rich,
                wake.clone(),
                cell_w,
                cell_h,
            ) {
                Ok(runtime) => return Ok(runtime),
                Err(error) if space_id.is_some() => return Err(error),
                Err(error) => eprintln!(
                    "prismattyc-host: log-backed attach failed, using pmux attach: {error:#}"
                ),
            }
        }
        let pty_size = pty_size(cols, rows, cell_w, cell_h);
        let mut session =
            PtySession::spawn_config(program, child_args, cwd, &host_pane_env(), pty_size)
                .with_context(|| {
                    format!(
                        "spawn {program:?} for pane {pane}{}",
                        cwd.map(|p| format!(" in {}", p.display()))
                            .unwrap_or_default()
                    )
                })?;
        let mut child_writer = session.take_input_writer()?;
        let mut pty_reader = session.take_reader()?;

        let (to_child_tx, to_child_rx) = mpsc::sync_channel::<ChildWrite>(TO_CHILD_CAP);
        let (grant_tx, grant_rx) = mpsc::channel();
        thread::Builder::new()
            .name(format!("prism-pane-{pane}-write"))
            .spawn(move || {
                while let Ok(msg) = to_child_rx.recv() {
                    if child_writer.write_all(&msg.bytes).is_err() {
                        break;
                    }
                    let _ = child_writer.flush();
                    if let Some(features) = msg.capability_grant {
                        let _ = grant_tx.send(features);
                    }
                }
            })?;

        let (from_pty_tx, from_pty_rx) =
            mpsc::sync_channel::<std::io::Result<Vec<u8>>>(FROM_PTY_CAP);
        let exit_tx = from_pty_tx.clone();
        let exit_wake = wake.clone();
        let child_pid = session.process_id();
        thread::Builder::new()
            .name(format!("prism-pane-{pane}-wait"))
            .spawn(move || {
                wait_for_child_exit(child_pid);
                let _ = exit_tx.send(Ok(Vec::new()));
                if let Some(wake) = exit_wake {
                    wake();
                }
            })?;
        thread::Builder::new()
            .name(format!("prism-pane-{pane}-read"))
            .spawn(move || {
                let mut buf = vec![0u8; 8192];
                loop {
                    match pty_reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = from_pty_tx.send(Ok(Vec::new()));
                            if let Some(wake) = &wake {
                                wake();
                            }
                            break;
                        }
                        Ok(n) => {
                            if from_pty_tx.send(Ok(buf[..n].to_vec())).is_err() {
                                break;
                            }
                            if let Some(wake) = &wake {
                                wake();
                            }
                        }
                        Err(error) => {
                            let _ = from_pty_tx.send(Err(error));
                            if let Some(wake) = &wake {
                                wake();
                            }
                            break;
                        }
                    }
                }
            })?;

        Self::assemble(
            Backing::Pty {
                session,
                from_pty_rx,
            },
            to_child_tx,
            grant_rx,
            cols,
            rows,
            experimental_rich,
            cell_w,
            cell_h,
        )
    }

    /// Attach a pmuxd session by subscribing to its pane event log (PT-111).
    /// No PTY child, one emulator, real scrollback.
    #[allow(clippy::too_many_arguments)]
    fn spawn_log_backed(
        pane: PaneId,
        connection: attach_log::LogConnection,
        cols: usize,
        rows: usize,
        experimental_rich: bool,
        wake: Option<Wake>,
        cell_w: usize,
        cell_h: usize,
    ) -> Result<Self> {
        let (to_child_tx, to_child_rx) = mpsc::sync_channel::<ChildWrite>(TO_CHILD_CAP);
        // Capability grants come from a rich guest over its own PTY; a log
        // replica never sees one, so this end stays empty.
        let (_grant_tx, grant_rx) = mpsc::channel();
        let session_key = connection.session_key().to_string();
        let (server_cols, server_rows) = connection.initial_size;
        let pane_title = connection.pane_title.clone();
        let title_pinned = connection.title_pinned;
        let log = connection
            .into_pane(cols, rows, cell_w, cell_h, to_child_rx, wake)
            .with_context(|| format!("subscribe pane for session {session_key} (pane {pane})"))?;
        let mut runtime = Self::assemble(
            Backing::Log(log),
            to_child_tx,
            grant_rx,
            cols,
            rows,
            experimental_rich,
            cell_w,
            cell_h,
        )?;
        runtime.apply_server_title(&pane_title, title_pinned);
        // Regroup may create this viewer in a temporary half-width slot and
        // expand it before the writer sends its first resize. If the final
        // size already matches pmuxd, no Resize event is emitted. Start the
        // replica at the server's size so output cannot wrap at that temporary
        // width. Later Resize events still apply in log order.
        runtime
            .emulator
            .resize(server_cols.max(1), server_rows.max(1));
        Ok(runtime)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        backing: Backing,
        to_child_tx: mpsc::SyncSender<ChildWrite>,
        grant_rx: mpsc::Receiver<CapabilityGrant>,
        cols: usize,
        rows: usize,
        experimental_rich: bool,
        cell_w: usize,
        cell_h: usize,
    ) -> Result<Self> {
        let mut emulator = if experimental_rich {
            Emulator::new_experimental(cols, rows, MAX_SCROLLBACK)
        } else {
            Emulator::new(cols, rows, MAX_SCROLLBACK)
        };
        emulator.set_cell_pixels(cell_w as u32, cell_h as u32);
        let last_content_epoch = emulator.screen().content_epoch();
        let mut viewer_bytes = [0u8; 16];
        getrandom::fill(&mut viewer_bytes)
            .map_err(|error| anyhow::anyhow!("mint rich viewer identity: {error}"))?;
        let (session, from_pty_rx, log) = match backing {
            Backing::Empty => (None, None, None),
            Backing::Pty {
                session,
                from_pty_rx,
            } => (Some(session), Some(from_pty_rx), None),
            Backing::Log(log) => (None, None, Some(log)),
        };
        Ok(Self {
            emulator,
            session,
            from_pty_rx,
            log,
            log_exit_reason: None,
            log_write_notice: None,
            policy_superseded_frames: 0,
            policy_superseded_bytes: 0,
            policy_batches: 0,
            policy_frames: 0,
            to_child_tx,
            grant_rx,
            rich: RichSession::default(),
            rich_viewer_id: ViewerId::from_bytes(viewer_bytes),
            experimental_rich,
            cols,
            outer_rows: rows,
            rows,
            selection: Selection::default(),
            keyboard_select_mode: false,
            view_scroll: 0,
            scroll_new_output: false,
            last_content_epoch,
            child_alive: true,
            title: None,
            title_pinned: false,
            attach_session: None,
            attach_name: None,
            placeholder: None,
            keep_local: false,
            unseen_output: false,
            mail_depth: 0,
            last_output_at: None,
            finish_watch: false,
            attention: None,
            size_owner: None,
            cell_w,
            cell_h,
        })
    }

    /// Output arrived recently enough that the pane reads as "working now".
    /// Unlike `unseen_output` this decays on its own and applies to the
    /// focused pane too.
    pub(crate) fn is_active_at(&self, now: Instant) -> bool {
        self.last_output_at
            .is_some_and(|at| now.duration_since(at) <= ACTIVE_WINDOW)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.is_active_at(Instant::now())
    }

    fn resize_cells(
        &mut self,
        cols: usize,
        rows: usize,
        cell_w: usize,
        cell_h: usize,
    ) -> Result<()> {
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);
        if self.cols == cols
            && self.outer_rows == rows
            && self.cell_w == cell_w
            && self.cell_h == cell_h
        {
            return Ok(());
        }
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.cols = cols;
        self.outer_rows = rows;
        self.reconcile_workspace_geometry()?;
        Ok(())
    }

    fn resize_guest(&mut self, cols: usize, rows: usize) -> Result<()> {
        // AC4: a log-backed pane owns no PTY. Host geometry goes to the
        // server as `Resize`; the server logs it at its byte position and
        // the replica emulator follows when that event arrives, so the
        // replica is never resized ahead of the log.
        if let Some(log) = self.log.as_ref() {
            log.request_resize(cols, rows, self.cell_w, self.cell_h);
            self.emulator
                .set_cell_pixels(self.cell_w as u32, self.cell_h as u32);
            self.cols = cols;
            self.rows = rows;
            return Ok(());
        }
        let size = pty_size(cols, rows, self.cell_w, self.cell_h);
        let cells_changed = self.emulator.screen().columns() != cols || self.rows != rows;
        if let Some(session) = self.session.as_ref() {
            session.resize(size)?;
        }
        self.emulator
            .set_cell_pixels(self.cell_w as u32, self.cell_h as u32);
        if !cells_changed {
            return Ok(());
        }
        self.emulator.resize(cols, rows);
        self.selection.clear();
        self.keyboard_select_mode = false;
        self.view_scroll = 0;
        self.last_content_epoch = self.emulator.screen().content_epoch();
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn reconcile_workspace_geometry(&mut self) -> Result<()> {
        let pane_rows = u16::try_from(self.outer_rows).unwrap_or(u16::MAX);
        let pane_cols = u16::try_from(self.cols).unwrap_or(u16::MAX);
        let workspace_rows = match self.rich.workspace_layout(pane_rows, pane_cols) {
            Ok(layout) => layout.map_or(0, |layout| usize::from(layout.rows)),
            Err(_) => {
                self.rich.drop_workspace();
                0
            }
        };
        self.resize_guest(
            self.cols,
            self.outer_rows.saturating_sub(workspace_rows).max(1),
        )
    }

    pub(crate) fn workspace_layout(&self) -> Option<prismattyc_render::WorkspaceLayout> {
        self.rich
            .workspace_layout(
                u16::try_from(self.outer_rows).unwrap_or(u16::MAX),
                u16::try_from(self.cols).unwrap_or(u16::MAX),
            )
            .ok()
            .flatten()
    }

    pub(crate) fn child_pid(&self) -> Option<u32> {
        self.session.as_ref().and_then(PtySession::process_id)
    }

    pub(crate) fn attach_policy_stats(&self) -> (usize, usize, usize, usize) {
        (
            self.policy_batches,
            self.policy_frames,
            self.policy_superseded_frames,
            self.policy_superseded_bytes,
        )
    }

    pub(crate) fn attach_queue_stats(&self) -> Option<attach_log::HostQueueStats> {
        self.log.as_ref().map(attach_log::LogPane::queue_stats)
    }

    pub(crate) fn attach_policy_notice(&self) -> Option<String> {
        let (batches, frames, superseded, bytes) = self.attach_policy_stats();
        (batches > 0).then(|| {
            format!(
                "attach policy active: {batches} batches / {frames} frames; coalesced {superseded} frames / {bytes} bytes"
            )
        })
    }

    /// Best-effort working directory for a new split: OSC 7 from the emulator,
    /// else the child process cwd (`/proc` on Linux, libproc on macOS).
    fn cwd_for_split(&self) -> Option<PathBuf> {
        if let Some(path) = self.emulator.cwd() {
            if path.is_absolute() {
                // Prefer existing dirs; still return path if it vanished mid-flight.
                return Some(path.to_path_buf());
            }
        }
        if let Some(pid) = self.child_pid() {
            if let Some(path) = prismattyc_mux::procinfo::cwd_of(pid) {
                return Some(path);
            }
        }
        None
    }

    fn drain(&mut self) -> (bool, bool, bool) {
        if self.log.is_some() {
            return self.drain_log();
        }
        // Lend the receiver out for the loop; `drain_pty` needs `&mut self`.
        let Some(from_pty_rx) = self.from_pty_rx.take() else {
            return (false, false, false);
        };
        let result = self.drain_pty(&from_pty_rx);
        self.from_pty_rx = Some(from_pty_rx);
        result
    }

    fn drain_pty(
        &mut self,
        from_pty_rx: &mpsc::Receiver<std::io::Result<Vec<u8>>>,
    ) -> (bool, bool, bool) {
        let mut dirty = false;
        let mut content_changed = false;
        let mut more = false;
        for i in 0..MAX_PTY_DRAIN_PER_PANE {
            match from_pty_rx.try_recv() {
                Ok(Ok(bytes)) if bytes.is_empty() => {
                    self.child_alive = false;
                    return (true, content_changed, false);
                }
                Ok(Ok(bytes)) => {
                    while let Ok(grant) = self.grant_rx.try_recv() {
                        self.rich.apply_grant(grant);
                    }
                    if self.experimental_rich {
                        rich::process_rich_chunk(
                            &mut self.emulator,
                            &mut self.rich,
                            &self.to_child_tx,
                            &bytes,
                        );
                        if self.reconcile_workspace_geometry().is_err() {
                            self.rich.drop_workspace();
                            let _ = self.resize_guest(self.cols, self.outer_rows.max(1));
                        }
                    } else {
                        let _ = self.emulator.feed(&bytes);
                        let replies = self.emulator.take_pending_replies();
                        for reply in replies {
                            let _ = self.to_child_tx.try_send(ChildWrite::bytes(reply));
                        }
                    }
                    if self.emulator.screen().alt_active() {
                        self.view_scroll = 0;
                    } else {
                        self.view_scroll = self
                            .view_scroll
                            .min(self.emulator.screen().max_view_scroll());
                    }
                    let epoch = self.emulator.screen().content_epoch();
                    let epoch_changed = epoch != self.last_content_epoch;
                    self.last_content_epoch = epoch;
                    content_changed |= epoch_changed;
                    if self.view_scroll == 0 {
                        self.scroll_new_output = false;
                    } else if epoch_changed {
                        self.scroll_new_output = true;
                    }
                    let mid_drag = self.selection.active && self.selection.anchor.is_some();
                    if epoch_changed
                        && !mid_drag
                        && (self.selection.range().is_some() || self.keyboard_select_mode)
                    {
                        self.selection.clear();
                        self.keyboard_select_mode = false;
                    }
                    dirty = true;
                    if i + 1 == MAX_PTY_DRAIN_PER_PANE {
                        more = true;
                    }
                }
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.child_alive = false;
                    return (true, content_changed, false);
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        // PTY read can stay blocked after the child dies; reap so the
        // cascade still runs on the next pump (live wiring).
        if self.child_alive
            && self
                .session
                .as_mut()
                .and_then(|session| session.try_wait().ok().flatten())
                .is_some()
        {
            self.child_alive = false;
            dirty = true;
        }
        (dirty, content_changed, more)
    }

    /// Apply pane-log frames to the replica emulator (PT-111). Same
    /// bookkeeping as [`Self::drain_pty`], different source.
    fn drain_log(&mut self) -> (bool, bool, bool) {
        let mut dirty = false;
        let mut content_changed = false;
        let mut more = false;
        for i in 0..MAX_PTY_DRAIN_PER_PANE {
            let Some(log) = self.log.as_ref() else {
                break;
            };
            match log.try_recv() {
                Ok(LogMessage::Batch {
                    snapshot,
                    events,
                    replay_through,
                    reservation,
                    counters,
                }) => {
                    // Ring overrun: repaint from the server snapshot, then
                    // keep tailing. Prior replica scrollback survives.
                    if let Some(styled) = snapshot {
                        let cols = usize::try_from(styled.content.cols)
                            .unwrap_or(self.cols)
                            .max(1);
                        let rows = usize::try_from(styled.content.rows)
                            .unwrap_or(self.rows)
                            .max(1);
                        if self.emulator.screen().columns() != cols
                            || self.emulator.screen().rows() != rows
                        {
                            self.emulator.resize(cols, rows);
                        }
                        let bytes = attach_log::snapshot_to_ansi(&styled);
                        self.feed_replica(&bytes);
                    }
                    let event_count = events.len();
                    for frame in events {
                        let replay = frame.seq <= replay_through;
                        self.apply_log_event(frame.event);
                        if replay {
                            // Restore terminal state, but do not sound old BEL
                            // or attention signals when a Space is reopened.
                            let _ = self.emulator.take_pending_bell();
                            let _ = self.emulator.take_pending_attention();
                        }
                    }
                    self.policy_superseded_frames = self
                        .policy_superseded_frames
                        .saturating_add(counters.superseded_frames);
                    self.policy_superseded_bytes = self
                        .policy_superseded_bytes
                        .saturating_add(counters.superseded_bytes);
                    self.policy_batches = self.policy_batches.saturating_add(1);
                    self.policy_frames = self.policy_frames.saturating_add(event_count);
                    drop(reservation);
                }
                Ok(LogMessage::Reset) => {
                    // StaleSequence: the retained log was rebuilt. Drop replica
                    // history so a replay from seq 1 does not stack into it.
                    let mut emulator = if self.experimental_rich {
                        Emulator::new_experimental(self.cols, self.rows, MAX_SCROLLBACK)
                    } else {
                        Emulator::new(self.cols, self.rows, MAX_SCROLLBACK)
                    };
                    emulator.set_cell_pixels(self.cell_w as u32, self.cell_h as u32);
                    self.emulator = emulator;
                    self.selection.clear();
                    self.keyboard_select_mode = false;
                    self.view_scroll = 0;
                }
                Ok(LogMessage::Ended { reason }) => {
                    self.log_exit_reason = self.log_exit_reason.take().or(reason);
                    self.child_alive = false;
                    return (true, content_changed, false);
                }
                Ok(LogMessage::WriteFailed { reason }) => {
                    eprintln!("prismattyc-host: attach write failed: {reason}");
                    self.log_write_notice = Some(attach_log::WRITE_FAILED_TOAST.to_string());
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.child_alive = false;
                    return (true, content_changed, false);
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
            if self.emulator.screen().alt_active() {
                self.view_scroll = 0;
            } else {
                self.view_scroll = self
                    .view_scroll
                    .min(self.emulator.screen().max_view_scroll());
            }
            let epoch = self.emulator.screen().content_epoch();
            let epoch_changed = epoch != self.last_content_epoch;
            self.last_content_epoch = epoch;
            content_changed |= epoch_changed;
            if self.view_scroll == 0 {
                self.scroll_new_output = false;
            } else if epoch_changed {
                self.scroll_new_output = true;
            }
            let mid_drag = self.selection.active && self.selection.anchor.is_some();
            if epoch_changed
                && !mid_drag
                && (self.selection.range().is_some() || self.keyboard_select_mode)
            {
                self.selection.clear();
                self.keyboard_select_mode = false;
            }
            dirty = true;
            if i + 1 == MAX_PTY_DRAIN_PER_PANE {
                more = true;
            }
        }
        (dirty, content_changed, more)
    }

    fn apply_log_event(&mut self, event: prismattyc_mux::PaneEvent) {
        use prismattyc_mux::PaneEvent;
        match event {
            PaneEvent::Output { bytes } => self.feed_replica(&bytes),
            PaneEvent::Resize {
                cols,
                rows,
                cell_px,
                size_owner,
                reflow,
            } => {
                let cols = usize::from(cols).max(1);
                let rows = usize::from(rows).max(1);
                self.size_owner = size_owner;
                self.emulator
                    .set_cell_pixels(cell_px.0.max(1), cell_px.1.max(1));
                if self.emulator.screen().columns() != cols || self.emulator.screen().rows() != rows
                {
                    if reflow {
                        self.emulator.resize(cols, rows);
                    } else {
                        self.emulator.resize_legacy(cols, rows);
                    }
                    self.selection.clear();
                    self.keyboard_select_mode = false;
                    self.view_scroll = 0;
                }
            }
            PaneEvent::SizeOwnerChanged { owner } => self.size_owner = owner,
            PaneEvent::MailDepth { depth } => self.mail_depth = depth,
            PaneEvent::Exited { code, signal } => {
                self.log_exit_reason = Some(match (code, signal) {
                    (_, Some(signal)) => format!("killed by {signal}"),
                    (Some(code), None) => format!("exited {code}"),
                    (None, None) => "exited".into(),
                });
                self.child_alive = false;
            }
            // Title, Cwd, Status and Attention are all derivable from the
            // Output bytes the replica already applied. Re-applying them
            // would double-count PT-53 attention.
            PaneEvent::Title { .. }
            | PaneEvent::Cwd { .. }
            | PaneEvent::Status { .. }
            | PaneEvent::Attention { .. } => {}
        }
    }

    fn apply_server_title(&mut self, title: &str, pinned: bool) {
        let trimmed = title.trim();
        self.title = (!trimmed.is_empty()).then(|| trimmed.to_string());
        self.title_pinned = pinned;
    }

    /// Feed replica bytes. Terminal replies (DSR/CPR/DA) are taken so they
    /// cannot accumulate and **dropped**: only the PTY owner answers them
    /// (`docs/pane-event-log-spike.md` §2).
    fn feed_replica(&mut self, bytes: &[u8]) {
        let _ = self.emulator.feed(bytes);
        let _ = self.emulator.take_pending_replies();
    }

    pub(crate) fn try_send_bytes(
        &self,
        bytes: Vec<u8>,
    ) -> Result<(), mpsc::TrySendError<ChildWrite>> {
        self.to_child_tx.try_send(ChildWrite::bytes(bytes))
    }

    #[cfg(test)]
    pub(crate) fn send_bytes(&self, bytes: Vec<u8>) -> Result<(), mpsc::SendError<ChildWrite>> {
        self.to_child_tx.send(ChildWrite::bytes(bytes))
    }

    pub(crate) fn experimental_rich(&self) -> bool {
        self.experimental_rich
    }
}

impl MuxRuntime {
    pub(crate) fn toggle_rich_focus(&mut self) -> bool {
        if !self.focused().experimental_rich() {
            return false;
        }
        let viewer_id = self.focused().rich_viewer_id;
        let bytes = self
            .focused_mut()
            .rich
            .toggle_structured_focus(viewer_id)
            .or_else(|| self.focused_mut().rich.toggle_focus());
        let Some(bytes) = bytes else {
            return false;
        };
        let _ = self.focused().try_send_bytes(bytes);
        true
    }

    pub(crate) fn rich_focus_active(&self) -> bool {
        let pane = self.focused();
        pane.experimental_rich()
            && (pane.rich.focus_id().is_some()
                || pane.rich.structured_focus_active(pane.rich_viewer_id))
    }

    pub(crate) fn send_rich_focus_key(&mut self, key: &str, modifiers: InputModifiers) -> bool {
        if !self.rich_focus_active() {
            return false;
        }
        let viewer_id = self.focused().rich_viewer_id;
        let bytes = self
            .focused_mut()
            .rich
            .encode_structured_key(viewer_id, key.to_string(), modifiers)
            .or_else(|| self.focused().rich.encode_focused_key(key));
        let Some(bytes) = bytes else {
            return false;
        };
        self.focused().try_send_bytes(bytes).is_ok()
    }

    pub(crate) fn focused_rich_region(&self) -> Option<(i32, u16, u16, u16)> {
        self.focused()
            .rich
            .focused_attachment()
            .map(|(_, row, col, rows, cols)| (row, col, rows, cols))
    }

    pub(crate) fn revoke_rich_focus(&mut self, pane: PaneId) {
        let Some(runtime) = self.panes.get_mut(&pane) else {
            return;
        };
        let bytes = runtime
            .rich
            .revoke_structured_focus(runtime.rich_viewer_id)
            .or_else(|| runtime.rich.revoke_focus());
        if let Some(bytes) = bytes {
            let _ = runtime.try_send_bytes(bytes);
        }
    }

    pub(crate) fn workspace_hit(
        &self,
        pane: PaneId,
        row: u16,
        col: u16,
    ) -> Option<crate::rich::WorkspaceHit> {
        let runtime = self.panes.get(&pane)?;
        runtime.rich.workspace_hit(
            u16::try_from(runtime.outer_rows).ok()?,
            u16::try_from(runtime.cols).ok()?,
            row,
            col,
        )
    }

    pub(crate) fn send_rich_pointer(
        &mut self,
        pane: PaneId,
        hit: crate::rich::WorkspaceHit,
        phase: PointerPhase,
    ) -> bool {
        let Some(runtime) = self.panes.get_mut(&pane) else {
            return false;
        };
        let Some(bytes) =
            runtime
                .rich
                .encode_structured_pointer(runtime.rich_viewer_id, hit, phase)
        else {
            return false;
        };
        runtime.try_send_bytes(bytes).is_ok()
    }

    pub(crate) fn send_rich_scroll(
        &mut self,
        pane: PaneId,
        hit: crate::rich::WorkspaceHit,
        delta: i16,
    ) -> bool {
        let Some(runtime) = self.panes.get_mut(&pane) else {
            return false;
        };
        let Some(bytes) = runtime
            .rich
            .encode_structured_scroll(runtime.rich_viewer_id, hit, delta)
        else {
            return false;
        };
        runtime.try_send_bytes(bytes).is_ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Rail-end inset matches pane slots: `window_pad`, no extra floor.
/// Close-glyph air comes from `TAB_CLOSE_INSET`, not a shrunken rail.
pub(crate) fn effective_tab_end_pad(window_pad: usize, stride_px: usize) -> usize {
    window_pad.min(stride_px / 2)
}

/// Unseen/active badge size on the tab strip.
pub(crate) const TAB_BADGE_SIZE: usize = 6;

/// Horizontal centerline shared by the close glyph (y=0, height `bar_h`)
/// and the 6×6 badges.
pub(crate) fn tab_chrome_center_y(bar_h: usize) -> usize {
    bar_h / 2
}

/// Top pixel of a badge so its center sits on `tab_chrome_center_y`.
pub(crate) fn tab_badge_top(bar_h: usize) -> usize {
    tab_chrome_center_y(bar_h).saturating_sub(TAB_BADGE_SIZE / 2)
}

/// Left origin and per-tab slot width inside the window-padded strip.
/// Gaps (`pane_gap`) sit between slots and are not part of any slot.
pub(crate) fn tab_content_origin_and_slot(
    stride_px: usize,
    n: usize,
    window_pad: usize,
    pane_gap: usize,
) -> Option<(usize, usize)> {
    if n == 0 || stride_px == 0 {
        return None;
    }
    let pad = effective_tab_end_pad(window_pad, stride_px);
    let gaps = pane_gap.saturating_mul(n.saturating_sub(1));
    let inner = stride_px
        .saturating_sub(pad.saturating_mul(2))
        .saturating_sub(gaps);
    let slot = inner / n;
    (slot > 0).then_some((pad, slot))
}

pub(crate) fn tab_slot_bounds(
    index: usize,
    n: usize,
    stride_px: usize,
    window_pad: usize,
    pane_gap: usize,
) -> Option<(usize, usize)> {
    let (origin, slot) = tab_content_origin_and_slot(stride_px, n, window_pad, pane_gap)?;
    let right = stride_px.saturating_sub(origin);
    let x0 = origin.saturating_add(index.saturating_mul(slot.saturating_add(pane_gap)));
    if x0 >= right {
        return None;
    }
    let end = if index + 1 == n {
        right
    } else {
        x0.saturating_add(slot).min(right)
    };
    let width = end.saturating_sub(x0);
    (width > 0).then_some((x0, width))
}

/// Pull the close cell in from the slot's right edge. F467 ink ends
/// 3–6px past `cell_w`; 4px left every × flush against the
/// next divider or the window.
pub(crate) const TAB_CLOSE_INSET: usize = 8;

/// Width of one pane handle (chip) on the tab strip: 1.5 cells, so the
/// chips read as targets rather than ticks. The raster and the hit-test
/// share this value.
pub(crate) fn pane_handle_w(cell_w: usize) -> usize {
    cell_w.max(1).saturating_mul(3) / 2
}

/// Left edge of the 1-cell close target inside a tab slot.
/// `None` when the slot cannot hold the cell plus the inset — drop
/// the glyph rather than clip it against the next divider.
pub(crate) fn tab_close_left(x0: usize, width: usize, cell_w: usize) -> Option<usize> {
    tab_close_left_with_inset(x0, width, cell_w, 0)
}

/// Left edge of the close target with pane-local content padding.
///
/// The legacy bearing-safe inset remains the floor for small or zero pane
/// padding; larger pane padding aligns right-side controls with the pane
/// content below.
pub(crate) fn tab_close_left_with_inset(
    x0: usize,
    width: usize,
    cell_w: usize,
    content_inset: usize,
) -> Option<usize> {
    let cell_w = cell_w.max(1);
    let end_inset = TAB_CLOSE_INSET.max(content_inset);
    let need = cell_w.saturating_add(end_inset);
    if width < need {
        return None;
    }
    Some(x0 + width - need)
}

/// Domain topology plus one live runtime per pane leaf.
pub(crate) struct MuxRuntime {
    pub(crate) space_id: Option<String>,
    git_info: crate::git_info::Cache,
    /// Previously focused pane per window, for `focus_last_pane` (PT-127).
    last_pane: HashMap<WindowId, PaneId>,
    /// Previously selected tab, for `select_last_tab` (PT-127).
    last_window: Option<WindowId>,
    domain: Domain,
    view: ClientView,
    panes: HashMap<PaneId, PaneRuntime>,
    rects: Vec<(PaneId, CellRect)>,
    cols: usize,
    rows: usize,
    geom: HostGeom,
    experimental_rich: bool,
    wake: Option<Wake>,
    /// Panes that rang BEL since the last [`Self::take_pending_bells`].
    pending_bells: Vec<PaneId>,
    /// Panes that emitted attention since the last [`Self::take_pending_attentions`].
    pending_attentions: Vec<(PaneId, String)>,
    /// Log-backed writer-death toasts since the last [`Self::take_pending_toasts`].
    pending_toasts: Vec<(PaneId, String)>,
    /// Client-local zoom (ADR-0007: a view projection, never a topology
    /// mutation). While set, the pane takes the whole window of the tab that
    /// owns it and its siblings keep their PTY sizes untouched in the tree.
    zoomed: Option<PaneId>,
}

impl MuxRuntime {
    #[cfg(test)]
    pub(crate) fn spawn(
        program: &str,
        child_args: &[String],
        cols: usize,
        rows: usize,
    ) -> Result<Self> {
        Self::spawn_with_geom(
            program,
            child_args,
            cols,
            rows,
            HostGeom::tight(1, 1),
            false,
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_wake(
        program: &str,
        child_args: &[String],
        cols: usize,
        rows: usize,
        wake: Wake,
    ) -> Result<Self> {
        Self::spawn_with_geom(
            program,
            child_args,
            cols,
            rows,
            HostGeom::tight(1, 1),
            false,
            Some(wake),
        )
    }

    #[cfg(test)]
    pub(crate) fn spawn_experimental(
        program: &str,
        child_args: &[String],
        cols: usize,
        rows: usize,
    ) -> Result<Self> {
        Self::spawn_with_geom(
            program,
            child_args,
            cols,
            rows,
            HostGeom::tight(1, 1),
            true,
            None,
        )
    }

    pub(crate) fn spawn_with_geom(
        program: &str,
        child_args: &[String],
        cols: usize,
        rows: usize,
        geom: HostGeom,
        experimental_rich: bool,
        wake: Option<Wake>,
    ) -> Result<Self> {
        let mut domain = Domain::bootstrap("default")?;
        let session = domain.sessions().next().context("bootstrap session")?.id;
        let window = domain
            .session(session)
            .and_then(|session| session.windows.first().copied())
            .context("bootstrap window")?;
        let pane = domain
            .window(window)
            .and_then(|window| window.layout.panes().first().copied())
            .context("bootstrap pane")?;
        let client = domain.mint_client()?;
        let view = ClientView::attach_session(&domain, client, session);
        let rects = rects_for(&domain, window, cols, rows, geom, None)?;
        let root_rect = rects
            .iter()
            .find_map(|(id, rect)| (*id == pane).then_some(*rect))
            .context("root pane missing from geometry")?;
        let (pty_cols, pty_rows) = geom.content_cells(root_rect);
        let runtime = PaneRuntime::spawn(
            pane,
            program,
            child_args,
            pty_cols,
            pty_rows,
            None,
            experimental_rich,
            wake.clone(),
            geom.cell_w,
            geom.cell_h,
            None,
        )?;
        let mut panes = HashMap::new();
        panes.insert(pane, runtime);
        Ok(Self {
            space_id: None,
            git_info: Default::default(),
            domain,
            view,
            panes,
            rects,
            cols,
            rows,
            geom,
            experimental_rich,
            wake,
            pending_bells: Vec::new(),
            pending_attentions: Vec::new(),
            pending_toasts: Vec::new(),
            last_pane: HashMap::new(),
            last_window: None,
            zoomed: None,
        })
    }

    pub(crate) fn empty_view(&self) -> Result<Self> {
        Self::spawn_with_geom(
            EMPTY_SPACE_PROGRAM,
            &[] as &[String],
            self.cols,
            self.rows,
            self.geom,
            self.experimental_rich,
            self.wake.clone(),
        )
    }

    pub(crate) const fn geom(&self) -> HostGeom {
        self.geom
    }

    pub(crate) fn active_window(&self) -> WindowId {
        self.view
            .window
            .or_else(|| {
                self.view.session.and_then(|session| {
                    self.domain
                        .session(session)
                        .and_then(|session| session.windows.first().copied())
                })
            })
            .expect("live mux always has an active window")
    }

    fn session_id(&self) -> SessionId {
        self.view.session.expect("live mux always has a session")
    }

    pub(crate) fn pane_window(&self, pane: PaneId) -> Option<WindowId> {
        self.domain.pane_owner(pane)
    }

    pub(crate) fn domain_pane_count(&self, window: WindowId) -> Option<usize> {
        self.domain
            .window(window)
            .map(|win| win.layout.pane_count())
    }

    pub(crate) fn tab_layouts(&self) -> Vec<(String, String)> {
        self.window_ids()
            .iter()
            .filter_map(|id| self.domain.window(*id))
            .map(|window| (window.title.clone(), format!("{:?}", window.layout)))
            .collect()
    }

    pub(crate) fn window_ids(&self) -> Vec<WindowId> {
        self.domain
            .session(self.session_id())
            .map(|session| session.windows.clone())
            .unwrap_or_default()
    }

    pub(crate) fn tab_count(&self) -> usize {
        self.window_ids().len()
    }

    /// Legacy auto-mode predicate retained for geometry tests.
    #[allow(dead_code)]
    pub(crate) fn tab_strip_needed(&self) -> bool {
        self.tab_count() > 1
            || self.window_ids().iter().any(|window| {
                self.domain
                    .window(*window)
                    .is_some_and(|win| win.layout.pane_count() > 1)
            })
    }

    /// Every tab in strip order: title and its panes in layout order.
    pub(crate) fn tab_panes(&self) -> Vec<(String, Vec<PaneId>)> {
        self.window_ids()
            .into_iter()
            .filter_map(|window| {
                let win = self.domain.window(window)?;
                Some((win.title.clone(), win.layout.panes()))
            })
            .collect()
    }

    pub(crate) fn refresh_git_info(&mut self, snapshot: Option<&prismattyc_mux::Snapshot>) -> bool {
        let targets = self
            .panes
            .iter()
            .filter_map(|(id, pane)| {
                let cwd = if let Some(remote) = self.remote_pane_id(*id) {
                    let remote = snapshot?
                        .sessions
                        .iter()
                        .flat_map(|s| &s.windows)
                        .flat_map(|w| &w.panes)
                        .find(|p| p.id == remote)?;
                    remote
                        .child_pid
                        .and_then(prismattyc_mux::procinfo::cwd_of)
                        .or_else(|| pane.emulator.cwd().map(Path::to_path_buf))
                        .or_else(|| remote.spawn.as_ref().and_then(|s| s.cwd.clone()))
                } else {
                    pane.cwd_for_split()
                }?;
                Some((*id, cwd))
            })
            .collect();
        self.git_info.poll(targets)
    }

    pub(crate) fn tab_git_labels(&self) -> Vec<Option<String>> {
        self.window_ids()
            .iter()
            .map(|window| {
                self.view
                    .focused_pane(*window)
                    .and_then(|id| self.git_info.label(id))
                    .map(str::to_string)
            })
            .collect()
    }

    pub(crate) fn local_terminal_rows(
        &self,
    ) -> Vec<(PaneId, String, Option<PathBuf>, Option<u32>)> {
        self.tab_panes()
            .into_iter()
            .flat_map(|(title, panes)| {
                panes.into_iter().filter_map(move |id| {
                    let pane = self.panes.get(&id)?;
                    Some((
                        id,
                        pane.attach_name
                            .clone()
                            .or_else(|| pane.title.clone())
                            .unwrap_or_else(|| {
                                if pane.keep_local {
                                    "Blank terminal".into()
                                } else {
                                    title.clone()
                                }
                            }),
                        pane.cwd_for_split(),
                        pane.child_pid(),
                    ))
                })
            })
            .collect()
    }

    /// Move the PTY runtime, including scrollback, into another view in this window.
    pub(crate) fn transfer_local(&mut self, target: &mut Self, pane: PaneId) -> Result<PaneId> {
        anyhow::ensure!(
            self.is_retained_local_terminal(pane),
            "not a blank terminal"
        );
        let window = self.pane_window(pane).context("pane has no tab")?;
        let tab_index = self
            .window_ids()
            .iter()
            .position(|id| *id == window)
            .unwrap();
        self.select_tab(tab_index)?;
        self.focus(pane);
        let empty_target = target.pane_count() == 1
            && target.is_placeholder(target.focused_id())
            && target.attach_session_of(target.focused_id()).is_none();
        if !empty_target {
            target.new_tab(EMPTY_SPACE_PROGRAM, &[])?;
        }
        let destination = target.focused_id();
        let old_domain = self.domain.clone();
        let old_view = self.view.clone();
        let old_rects = self.rects.clone();
        let runtime = self
            .panes
            .remove(&pane)
            .context("blank terminal disappeared")?;
        let result = if self.active_pane_count() > 1 {
            self.close_pane(pane)
        } else if self.tab_count() > 1 {
            self.close_tab().map(|_| ())
        } else {
            Ok(())
        };
        if let Err(error) = result {
            self.domain = old_domain;
            self.view = old_view;
            self.rects = old_rects;
            self.panes.insert(pane, runtime);
            if !empty_target {
                let _ = target.close_tab();
            }
            return Err(error);
        }
        let placeholder = target
            .panes
            .insert(destination, runtime)
            .expect("empty destination");
        if self.domain.pane(pane).is_some() {
            self.panes.insert(pane, placeholder);
            self.domain.rename_window(window, "Empty Space")?;
        }
        target
            .domain
            .rename_window(target.active_window(), "Terminal")?;
        Ok(destination)
    }

    pub(crate) fn tab_infos(&self) -> Vec<TabInfo> {
        let active = self.active_window();
        self.window_ids()
            .into_iter()
            .map(|window| {
                let title = self
                    .domain
                    .window(window)
                    .map(|win| win.title.clone())
                    .unwrap_or_default();
                let panes = self
                    .domain
                    .window(window)
                    .map(|win| win.layout.panes())
                    .unwrap_or_default();
                let unseen = panes.iter().any(|pane| {
                    self.panes
                        .get(pane)
                        .is_some_and(|runtime| runtime.unseen_output)
                });
                let active_output = panes.iter().any(|pane| {
                    self.panes
                        .get(pane)
                        .is_some_and(|runtime| runtime.is_active())
                });
                let attention = panes.iter().any(|pane| {
                    self.panes
                        .get(pane)
                        .is_some_and(|runtime| runtime.attention.is_some())
                });
                let git = self
                    .view
                    .focused_pane(window)
                    .and_then(|id| self.git_info.label(id));
                TabInfo {
                    title,
                    selected: window == active,
                    unseen,
                    active: active_output,
                    attention,
                    zoomed: self.zoomed.is_some_and(|zoomed| panes.contains(&zoomed)),
                    handles: if panes.len() > 1 { panes.len() } else { 0 },
                    focused_handle: (panes.len() > 1 && window == active)
                        .then(|| panes.iter().position(|pane| *pane == self.focused_id()))
                        .flatten(),
                    handle_titles: if panes.len() > 1 {
                        panes
                            .iter()
                            .enumerate()
                            .map(|(index, pane)| {
                                self.panes
                                    .get(pane)
                                    .and_then(|runtime| {
                                        runtime
                                            .attach_name
                                            .clone()
                                            .or_else(|| runtime.title.clone())
                                            .or_else(|| runtime.attach_session.clone())
                                    })
                                    .unwrap_or_else(|| format!("pane {}", index + 1))
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                    handle_active: if panes.len() > 1 {
                        panes
                            .iter()
                            .map(|pane| {
                                self.panes
                                    .get(pane)
                                    .is_some_and(|runtime| runtime.is_active())
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                    git_label: git.map(str::to_string),
                    pane_title: {
                        let focused = if panes.len() == 1 {
                            Some(panes[0])
                        } else if window == active {
                            Some(self.focused_id())
                        } else {
                            None
                        };
                        focused.and_then(|pane| {
                            self.panes.get(&pane).and_then(|runtime| {
                                runtime
                                    .attach_name
                                    .clone()
                                    .or_else(|| runtime.title.clone())
                            })
                        })
                    },
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn set_geom(&mut self, geom: HostGeom) -> Result<()> {
        self.resize_with_geom(self.cols, self.rows, geom)
    }

    pub(crate) fn split_focused(
        &mut self,
        program: &str,
        child_args: &[String],
        axis: Axis,
        ratio: f64,
    ) -> Result<PaneId> {
        self.unzoom()?;
        let prior = self.focused_id();
        let inherit_cwd = self.panes.get(&prior).and_then(PaneRuntime::cwd_for_split);
        let prior_rects = self.rects.clone();
        let pane = self.domain.split_pane(
            self.active_window(),
            prior,
            axis,
            ratio,
            Some((
                self.cols,
                self.rows,
                self.geom.min_cols(),
                self.geom.min_rows(),
            )),
        )?;
        let rects = match self.window_rects(self.active_window()) {
            Ok(rects) => rects,
            Err(error) => {
                let _ = self.domain.close_pane(self.active_window(), pane, prior);
                return Err(error);
            }
        };
        let Some(rect) = rects
            .iter()
            .find_map(|(id, rect)| (*id == pane).then_some(*rect))
        else {
            let _ = self.domain.close_pane(self.active_window(), pane, prior);
            anyhow::bail!("new pane missing from geometry");
        };
        let (pty_cols, pty_rows) = self.geom.content_cells(rect);
        let runtime = match PaneRuntime::spawn(
            pane,
            program,
            child_args,
            pty_cols,
            pty_rows,
            inherit_cwd.as_deref(),
            self.experimental_rich,
            self.wake.clone(),
            self.geom.cell_w,
            self.geom.cell_h,
            self.space_id.as_deref(),
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self.domain.close_pane(self.active_window(), pane, prior);
                return Err(error);
            }
        };
        self.panes.insert(pane, runtime);
        self.view.set_focused_pane(self.active_window(), pane);
        if let Err(error) = self.apply_rects(rects, self.geom) {
            self.panes.remove(&pane);
            let rollback = self.domain.close_pane(self.active_window(), pane, prior);
            self.view.set_focused_pane(self.active_window(), prior);
            self.rects = prior_rects;
            if let Err(rollback_error) = rollback {
                return Err(error.context(format!(
                    "also failed to roll back pane {pane}: {rollback_error}"
                )));
            }
            return Err(error);
        }
        // Recorded only after the whole split (domain, focus, rects) held, so
        // a rolled-back split leaves the last-pane history untouched (PT-127).
        self.last_pane.insert(self.active_window(), prior);
        Ok(pane)
    }

    /// Spawn until the active window has `count` panes, then retile them as an
    /// even-width horizontal row.
    pub(crate) fn ensure_even_columns(
        &mut self,
        program: &str,
        child_args: &[String],
        count: usize,
    ) -> Result<bool> {
        anyhow::ensure!(count >= 1, "column count must be at least 1");
        self.unzoom()?;
        while self.active_pane_count() < count {
            self.split_focused(program, child_args, Axis::Horizontal, 0.5)?;
        }
        let window = self.active_window();
        let ids = self
            .domain
            .window(window)
            .map(|win| win.layout.panes())
            .unwrap_or_default();
        let new_layout = even_horizontal_row(&ids).map_err(|e| anyhow::anyhow!("{e}"))?;
        self.domain
            .set_layout(window, new_layout)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rects = self.window_rects(window)?;
        self.apply_rects(rects, self.geom)?;
        Ok(true)
    }

    /// Spawn until four panes, then retile as a 2×2 (or two even rows if more).
    pub(crate) fn ensure_even_quadrants(
        &mut self,
        program: &str,
        child_args: &[String],
    ) -> Result<bool> {
        self.unzoom()?;
        while self.active_pane_count() < 4 {
            self.split_focused(program, child_args, Axis::Horizontal, 0.5)?;
        }
        let window = self.active_window();
        let ids = self
            .domain
            .window(window)
            .map(|win| win.layout.panes())
            .unwrap_or_default();
        let new_layout = even_two_row_grid(&ids).map_err(|e| anyhow::anyhow!("{e}"))?;
        self.domain
            .set_layout(window, new_layout)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rects = self.window_rects(window)?;
        self.apply_rects(rects, self.geom)?;
        Ok(true)
    }

    /// Pane ids in the selected tab, layout order.
    pub(crate) fn active_pane_ids(&self) -> Vec<PaneId> {
        self.tab_panes()
            .into_iter()
            .nth(self.selected_tab_index())
            .map(|(_, panes)| panes)
            .unwrap_or_default()
    }

    /// Mark panes spawned since `before` as attached to `session_id`.
    pub(crate) fn register_new_active_attaches(&mut self, before: &[PaneId], session_id: &str) {
        let before: HashSet<PaneId> = before.iter().copied().collect();
        for pane in self.active_pane_ids() {
            if !before.contains(&pane) {
                self.mark_attach_session(pane, session_id.to_string(), session_id.to_string());
            }
        }
    }

    /// Retile the active tab's existing panes. Never spawns or closes a pane.
    /// Unzooms first, except `Single` with more than one pane (no-op; zoom
    /// stays PT-57). A retile that cannot satisfy minima leaves the layout
    /// unchanged.
    pub(crate) fn apply_preset(&mut self, preset: LayoutPreset) -> Result<PresetOutcome> {
        let n = self.active_pane_count();
        if preset == LayoutPreset::Single {
            if n > 1 {
                return Ok(PresetOutcome::Unchanged("preset single needs one pane"));
            }
            self.unzoom()?;
            return Ok(PresetOutcome::Applied);
        }
        self.unzoom()?;
        let window = self.active_window();
        let ids = self
            .domain
            .window(window)
            .map(|win| win.layout.panes())
            .unwrap_or_default();
        let focused = self.focused_id();
        let new_layout = match preset {
            LayoutPreset::Single => unreachable!("handled above"),
            LayoutPreset::SplitH => {
                even_horizontal_row(&ids).map_err(|e| anyhow::anyhow!("{e}"))?
            }
            LayoutPreset::SplitV => {
                even_vertical_column(&ids).map_err(|e| anyhow::anyhow!("{e}"))?
            }
            LayoutPreset::Grid => even_two_row_grid(&ids).map_err(|e| anyhow::anyhow!("{e}"))?,
            LayoutPreset::MainVertical => {
                apply_arrangement(Arrangement::MainVertical, &ids, Some(focused))
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            }
            LayoutPreset::MainHorizontal => {
                apply_arrangement(Arrangement::MainHorizontal, &ids, Some(focused))
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            }
        };
        let prior_layout = self
            .domain
            .window(window)
            .context("mux window missing")?
            .layout
            .clone();
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: self.cols,
            rows: self.rows,
        };
        let rects = match layout_to_rects(
            &new_layout,
            bounds,
            self.geom.min_cols(),
            self.geom.min_rows(),
        ) {
            Ok(rects) => rects,
            Err(error) => return Err(anyhow::anyhow!("{error}")),
        };
        self.domain
            .set_layout(window, new_layout)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let prior_rects = self.rects.clone();
        if let Err(error) = self.apply_rects(rects, self.geom) {
            let rollback = self
                .domain
                .set_layout(window, prior_layout)
                .map_err(|e| anyhow::anyhow!("{e}"));
            self.rects = prior_rects;
            if let Err(rollback_error) = rollback {
                return Err(
                    error.context(format!("also failed to roll back layout: {rollback_error}"))
                );
            }
            return Err(error);
        }
        Ok(PresetOutcome::Applied)
    }

    /// Toggle the client-local zoom of the focused pane (PT-57). Zoom is a
    /// view projection: the split tree stays as it is, the focused pane
    /// takes the whole tab, and hidden siblings keep their PTY geometry.
    /// Returns `Ok(false)` when the tab has a single pane.
    pub(crate) fn toggle_zoom(&mut self) -> Result<bool> {
        if self.zoomed_here().is_some() {
            return self.set_zoom(None);
        }
        if self.active_pane_count() < 2 {
            return Ok(false);
        }
        self.set_zoom(Some(self.focused_id()))
    }

    #[cfg(test)]
    pub(crate) fn zoomed_pane(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// True while the active tab shows a zoomed pane.
    pub(crate) fn is_zoomed(&self) -> bool {
        self.zoomed_here().is_some()
    }

    /// The zoomed pane when it lives in the active tab. Zoom on another
    /// tab is left alone by edits made here.
    fn zoomed_here(&self) -> Option<PaneId> {
        self.zoomed.filter(|zoomed| {
            self.domain
                .window(self.active_window())
                .is_some_and(|window| window.layout.contains_pane(*zoomed))
        })
    }

    #[cfg(test)]
    fn active_layout(&self) -> PaneLayout {
        self.domain
            .window(self.active_window())
            .expect("active window")
            .layout
            .clone()
    }

    pub(crate) fn selected_tab_index(&self) -> usize {
        let active = self.active_window();
        self.window_ids()
            .iter()
            .position(|window| *window == active)
            .unwrap_or(0)
    }

    /// Select `tab_index` (out of range → 0) and focus `pane` if it is on
    /// that tab. Always unzooms.
    pub(crate) fn seed_tab_and_focus(
        &mut self,
        tab_index: usize,
        pane: Option<PaneId>,
    ) -> Result<()> {
        let n = self.tab_count();
        let tab = if n == 0 {
            0
        } else if tab_index < n {
            tab_index
        } else {
            0
        };
        let _ = self.select_tab(tab)?;
        if let Some(pane) = pane {
            let _ = self.focus(pane);
        }
        if self.zoomed.is_some() {
            self.set_zoom(None)?;
        }
        Ok(())
    }

    fn set_zoom(&mut self, zoomed: Option<PaneId>) -> Result<bool> {
        let prior = self.zoomed;
        self.zoomed = zoomed;
        let applied = self
            .window_rects(self.active_window())
            .and_then(|rects| self.apply_rects(rects, self.geom));
        if let Err(error) = applied {
            self.zoomed = prior;
            return Err(error);
        }
        Ok(true)
    }

    /// Leave zoom before a topology edit (split, retile, move); tmux does
    /// the same so the new pane is visible. Clears zoom even when it
    /// lives on an inactive tab.
    pub(crate) fn unzoom(&mut self) -> Result<()> {
        if self.zoomed.is_some() {
            self.set_zoom(None)?;
        }
        Ok(())
    }

    fn window_rects(&self, window: WindowId) -> Result<Vec<(PaneId, CellRect)>> {
        rects_for(
            &self.domain,
            window,
            self.cols,
            self.rows,
            self.geom,
            self.zoomed,
        )
    }

    #[cfg(test)]
    pub(crate) fn resize(&mut self, cols: usize, rows: usize) -> Result<()> {
        self.resize_with_geom(cols, rows, self.geom)
    }

    pub(crate) fn resize_with_geom(
        &mut self,
        cols: usize,
        rows: usize,
        geom: HostGeom,
    ) -> Result<()> {
        let rects = rects_for(
            &self.domain,
            self.active_window(),
            cols,
            rows,
            geom,
            self.zoomed,
        )?;
        self.apply_rects(rects, geom)?;
        self.cols = cols;
        self.rows = rows;
        self.geom = geom;
        Ok(())
    }

    fn apply_rects(&mut self, rects: Vec<(PaneId, CellRect)>, geom: HostGeom) -> Result<()> {
        let mut resized = Vec::with_capacity(rects.len());
        for (pane, rect) in &rects {
            let Some(runtime) = self.panes.get_mut(pane) else {
                self.restore_dimensions(&resized);
                anyhow::bail!("missing runtime for pane {pane}");
            };
            let prior = (runtime.cols, runtime.rows);
            let (cols, rows) = geom.content_cells(*rect);
            if let Err(error) = runtime.resize_cells(cols, rows, geom.cell_w, geom.cell_h) {
                self.restore_dimensions(&resized);
                return Err(error).with_context(|| format!("resize pane {pane}"));
            }
            resized.push((*pane, prior.0, prior.1));
        }
        self.rects = rects;
        Ok(())
    }

    fn restore_dimensions(&mut self, dimensions: &[(PaneId, usize, usize)]) {
        for (pane, cols, rows) in dimensions.iter().rev() {
            if let Some(runtime) = self.panes.get_mut(pane) {
                let cell_w = runtime.cell_w;
                let cell_h = runtime.cell_h;
                let _ = runtime.resize_cells(*cols, *rows, cell_w, cell_h);
            }
        }
    }

    fn close_pane(&mut self, pane: PaneId) -> Result<()> {
        let window = self
            .domain
            .pane_owner(pane)
            .with_context(|| format!("pane {pane} has no owning window"))?;
        let prior_domain = self.domain.clone();
        let prior_view = self.view.clone();
        let prior_rects = self.rects.clone();
        let prior_zoom = self.zoomed;
        if prior_zoom == Some(pane) {
            self.zoomed = None;
        }
        let prior_focus = self.view.focused_pane(window).unwrap_or(pane);
        let next_focus = self
            .domain
            .close_pane(window, pane, prior_focus)
            .with_context(|| format!("close pane {pane}"))?;
        self.view.set_focused_pane(window, next_focus);
        // Inactive tabs keep their layout in the domain; rects/PTYs stay
        // those of the visible window (no focus steal, no stale visible rects).
        if window == self.active_window() {
            let rects = match self.window_rects(window) {
                Ok(rects) => rects,
                Err(error) => {
                    self.domain = prior_domain;
                    self.view = prior_view;
                    self.zoomed = prior_zoom;
                    return Err(error);
                }
            };
            if let Err(error) = self.apply_rects(rects, self.geom) {
                self.domain = prior_domain;
                self.view = prior_view;
                self.rects = prior_rects;
                self.zoomed = prior_zoom;
                return Err(error);
            }
            if let Some(runtime) = self.panes.get_mut(&next_focus) {
                runtime.unseen_output = false;
                runtime.attention = None;
            }
        }
        self.panes.remove(&pane);
        Ok(())
    }

    pub(crate) fn focus(&mut self, pane: PaneId) -> bool {
        if self.panes.contains_key(&pane)
            && self
                .domain
                .window(self.active_window())
                .is_some_and(|window| window.layout.contains_pane(pane))
        {
            // Focusing a hidden sibling leaves zoom (tmux `select-pane`).
            if self.zoomed_here().is_some_and(|zoomed| zoomed != pane)
                && self.set_zoom(None).is_err()
            {
                return false;
            }
            let prior = self.focused_id();
            if prior != pane {
                if let Some(old) = self.panes.get_mut(&prior) {
                    if old.is_active() {
                        old.finish_watch = true;
                    }
                }
                self.revoke_rich_focus(prior);
                self.last_pane.insert(self.active_window(), prior);
            }
            self.view.set_focused_pane(self.active_window(), pane);
            let focused = self.panes.get_mut(&pane).expect("checked");
            focused.unseen_output = false;
            focused.attention = None;
            focused.finish_watch = false;
            true
        } else {
            false
        }
    }

    pub(crate) fn focus_neighbor(&mut self, direction: FocusDirection) -> bool {
        // Neighbors are hidden while zoomed; leave zoom, then navigate.
        if self.zoomed_here().is_some() && self.set_zoom(None).is_err() {
            return false;
        }
        let focused = self.focused_id();
        let Some((_, current)) = self.rects.iter().find(|(pane, _)| *pane == focused) else {
            return false;
        };
        let current_center = (
            current.col.saturating_mul(2).saturating_add(current.cols),
            current.row.saturating_mul(2).saturating_add(current.rows),
        );
        let mut candidates = self
            .rects
            .iter()
            .filter(|(pane, _)| *pane != focused)
            .filter_map(|(pane, rect)| {
                let center = (
                    rect.col.saturating_mul(2).saturating_add(rect.cols),
                    rect.row.saturating_mul(2).saturating_add(rect.rows),
                );
                let in_direction = match direction {
                    FocusDirection::Left => center.0 < current_center.0,
                    FocusDirection::Right => center.0 > current_center.0,
                    FocusDirection::Up => center.1 < current_center.1,
                    FocusDirection::Down => center.1 > current_center.1,
                };
                if !in_direction {
                    return None;
                }
                let horizontal = matches!(direction, FocusDirection::Left | FocusDirection::Right);
                let (primary, orthogonal, overlaps) = if horizontal {
                    (
                        center.0.abs_diff(current_center.0),
                        center.1.abs_diff(current_center.1),
                        ranges_overlap(
                            current.row,
                            current.row.saturating_add(current.rows),
                            rect.row,
                            rect.row.saturating_add(rect.rows),
                        ),
                    )
                } else {
                    (
                        center.1.abs_diff(current_center.1),
                        center.0.abs_diff(current_center.0),
                        ranges_overlap(
                            current.col,
                            current.col.saturating_add(current.cols),
                            rect.col,
                            rect.col.saturating_add(rect.cols),
                        ),
                    )
                };
                Some((!overlaps, primary, orthogonal, pane.get(), *pane))
            })
            .collect::<Vec<_>>();
        candidates
            .sort_unstable_by_key(|candidate| (candidate.0, candidate.1, candidate.2, candidate.3));
        candidates
            .first()
            .is_some_and(|(_, _, _, _, pane)| self.focus(*pane))
    }

    pub(crate) fn close_focused(&mut self) -> Result<bool> {
        let pane = self.focused_id();
        let active_panes = self
            .domain
            .window(self.active_window())
            .map(|window| window.layout.pane_count())
            .unwrap_or(0);
        if active_panes > 1 {
            self.close_pane(pane)?;
            return Ok(true);
        }
        if self.tab_count() > 1 {
            return self.close_tab();
        }
        if self.is_placeholder(pane) {
            self.panes.remove(&pane);
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn focused_id(&self) -> PaneId {
        self.view
            .focused_pane(self.active_window())
            .or_else(|| {
                self.domain
                    .window(self.active_window())
                    .and_then(|window| window.layout.panes().first().copied())
            })
            .expect("live mux window always has one pane")
    }

    pub(crate) fn take_semantic_copy(&mut self) -> Option<String> {
        self.focused_mut().rich.take_semantic_copy()
    }

    pub(crate) fn focused(&self) -> &PaneRuntime {
        self.panes
            .get(&self.focused_id())
            .expect("focused pane always has a runtime")
    }

    pub(crate) fn focused_mut(&mut self) -> &mut PaneRuntime {
        let pane = self.focused_id();
        self.panes
            .get_mut(&pane)
            .expect("focused pane always has a runtime")
    }

    pub(crate) fn pane(&self, id: PaneId) -> Option<&PaneRuntime> {
        self.panes.get(&id)
    }

    pub(crate) fn panes_and_rects(&self) -> impl Iterator<Item = (PaneId, &PaneRuntime, CellRect)> {
        self.rects
            .iter()
            .filter_map(|(pane, rect)| self.panes.get(pane).map(|runtime| (*pane, runtime, *rect)))
    }

    /// Take per-pane emulator damage for the next frame (PT-243).
    pub(crate) fn take_pane_damage(&mut self) -> HashMap<PaneId, prismattyc_core::GridDamage> {
        self.panes
            .iter_mut()
            .map(|(id, pane)| (*id, pane.emulator.take_damage()))
            .collect()
    }

    pub(crate) fn rects(&self) -> impl Iterator<Item = (PaneId, CellRect)> + '_ {
        self.rects.iter().copied()
    }

    pub(crate) fn workspace_rows(&self, pane: PaneId) -> usize {
        self.panes
            .get(&pane)
            .and_then(PaneRuntime::workspace_layout)
            .map_or(0, |layout| usize::from(layout.rows))
    }

    #[cfg(test)]
    pub(crate) fn pane_at_cell(&self, col: usize, row: usize) -> Option<(PaneId, usize, usize)> {
        self.rects.iter().find_map(|(pane, rect)| {
            let inside = col >= rect.col
                && row >= rect.row
                && col < rect.col.saturating_add(rect.cols)
                && row < rect.row.saturating_add(rect.rows);
            inside.then_some((*pane, row - rect.row, col - rect.col))
        })
    }

    pub(crate) const fn cols(&self) -> usize {
        self.cols
    }

    pub(crate) const fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn pane_count(&self) -> usize {
        self.rects.len()
    }

    /// Authoritative leaf count for the active window's domain layout.
    /// Use this for `host_geom` (pane_gap on/off). `pane_count()` is the
    /// cached visible rects and can lag a tab switch by one command.
    pub(crate) fn active_pane_count(&self) -> usize {
        self.domain
            .window(self.active_window())
            .map(|win| win.layout.panes().len())
            .unwrap_or(0)
    }

    pub(crate) fn unseen_count(&self) -> usize {
        self.panes
            .values()
            .filter(|pane| pane.unseen_output)
            .count()
    }

    /// Sum of MailAttention depths. Title chrome only; the glyph is binary.
    pub(crate) fn mail_depth_total(&self) -> u32 {
        self.panes.values().map(|pane| pane.mail_depth).sum()
    }

    /// Panes whose mail depth rose since `last`. Updates `last`.
    ///
    /// The first observation of a pane is a baseline, not a rise. Dead pane
    /// keys are dropped so the map does not grow without bound.
    pub(crate) fn take_mail_rises(
        &self,
        last: &mut std::collections::BTreeMap<u64, u32>,
    ) -> Vec<(String, u32)> {
        let mut rises = Vec::new();
        let mut live = std::collections::BTreeSet::new();
        for (id, pane) in &self.panes {
            let key = id.get();
            live.insert(key);
            match last.get(&key).copied() {
                None => {
                    last.insert(key, pane.mail_depth);
                }
                Some(prev) => {
                    last.insert(key, pane.mail_depth);
                    if pane.mail_depth > prev {
                        let agent = self
                            .pane_session_name(*id)
                            .unwrap_or_else(|| "agent".into());
                        rises.push((agent, pane.mail_depth));
                    }
                }
            }
        }
        last.retain(|key, _| live.contains(key));
        rises
    }

    /// Mail depth per tab, same order as [`Self::tab_infos`].
    pub(crate) fn tab_mail_depths(&self) -> Vec<u32> {
        self.window_ids()
            .into_iter()
            .map(|window| {
                self.domain
                    .window(window)
                    .map(|win| {
                        win.layout
                            .panes()
                            .iter()
                            .map(|pane| {
                                self.panes
                                    .get(pane)
                                    .map(|runtime| runtime.mail_depth)
                                    .unwrap_or(0)
                            })
                            .sum()
                    })
                    .unwrap_or(0)
            })
            .collect()
    }

    /// Panes that rang BEL since the previous take (PT-39 visual bell /
    /// OS notification). Independent of the unseen-output badge.
    pub(crate) fn take_pending_bells(&mut self) -> Vec<PaneId> {
        std::mem::take(&mut self.pending_bells)
    }

    /// Attention messages emitted since the previous take.
    pub(crate) fn take_pending_attentions(&mut self) -> Vec<(PaneId, String)> {
        std::mem::take(&mut self.pending_attentions)
    }

    /// Writer-death toasts since the previous take.
    pub(crate) fn take_pending_toasts(&mut self) -> Vec<(PaneId, String)> {
        std::mem::take(&mut self.pending_toasts)
    }

    /// Title of the tab that owns `pane`, for notification text.
    pub(crate) fn pane_tab_title(&self, pane: PaneId) -> Option<String> {
        let window = self.domain.pane_owner(pane)?;
        self.domain.window(window).map(|win| win.title.clone())
    }

    /// Whether `pane` belongs to the currently selected tab.
    pub(crate) fn pane_tab_selected(&self, pane: PaneId) -> bool {
        self.domain
            .pane_owner(pane)
            .is_some_and(|window| window == self.active_window())
    }

    /// Session name that owns `pane`, for notification text.
    pub(crate) fn pane_session_name(&self, pane: PaneId) -> Option<String> {
        let window = self.domain.pane_owner(pane)?;
        let session = self
            .domain
            .sessions()
            .find(|session| session.windows.contains(&window))?;
        Some(session.name.clone())
    }

    /// Apply `MailAttentionChanged`. Depth 0 clears. Unknown pane is a no-op.
    pub(crate) fn apply_mail_attention(&mut self, pane_raw: u64, depth: u32) -> bool {
        let Some(runtime) = self
            .panes
            .iter_mut()
            .find(|(id, _)| id.get() == pane_raw)
            .map(|(_, runtime)| runtime)
        else {
            return false;
        };
        if runtime.mail_depth == depth {
            return false;
        }
        runtime.mail_depth = depth;
        true
    }

    pub(crate) fn active_count(&self) -> usize {
        let now = Instant::now();
        self.panes
            .values()
            .filter(|pane| pane.is_active_at(now))
            .count()
    }

    pub(crate) fn drain_all(&mut self) -> (bool, bool) {
        let pane_ids: Vec<_> = self.panes.keys().copied().collect();
        let focused = self.focused_id();
        let now = Instant::now();
        let mut dirty = false;
        let mut more = false;
        let mut bells = Vec::new();
        let mut attentions = Vec::new();
        let mut toasts = Vec::new();
        for pane in pane_ids {
            if let Some(runtime) = self.panes.get_mut(&pane) {
                // Inactive tabs keep draining so unseen badges accumulate.
                let (pane_dirty, content_changed, pane_more) = runtime.drain();
                let bell = runtime.emulator.take_pending_bell();
                if bell {
                    bells.push(pane);
                }
                if let Some(message) = runtime.emulator.take_pending_attention() {
                    runtime.attention = Some(message.clone());
                    attentions.push((pane, message));
                }
                if let Some(text) = runtime.emulator.take_pending_title() {
                    let title = pane_title_from_osc(&text);
                    if accept_osc_title(runtime.title_pinned, &runtime.title, &title) {
                        runtime.title = title;
                        dirty = true;
                    }
                }
                if let Some(label) = runtime.log_write_notice.take() {
                    toasts.push((pane, label));
                }
                let prior_unseen = runtime.unseen_output;
                apply_unseen_v2(
                    &mut runtime.unseen_output,
                    &mut runtime.last_output_at,
                    &mut runtime.finish_watch,
                    now,
                    pane == focused,
                    content_changed,
                    bell,
                );
                dirty |= pane_dirty || runtime.unseen_output != prior_unseen;
                more |= pane_more;
            }
        }
        self.pending_bells.extend(bells);
        self.pending_attentions.extend(attentions);
        self.pending_toasts.extend(toasts);
        let exited: Vec<_> = self
            .panes
            .iter()
            .filter_map(|(pane, runtime)| (!runtime.child_alive).then_some(*pane))
            .collect();
        for pane in exited {
            if !self.panes.contains_key(&pane) {
                continue;
            }
            dirty |= self.close_exited_pane(pane).unwrap_or(false);
        }
        (dirty, more)
    }

    /// Child-exit cascade: close the pane; empty tab closes and
    /// selects the neighbor (same rule as C-S-Q); last pane of the last
    /// tab is left in place so `all_children_exited` can tear down the host.
    fn close_exited_pane(&mut self, pane: PaneId) -> Result<bool> {
        if self
            .panes
            .get(&pane)
            .is_some_and(|runtime| runtime.attach_session.is_some())
        {
            return self.enter_placeholder(pane);
        }
        let Some(window) = self.domain.pane_owner(pane) else {
            self.panes.remove(&pane);
            return Ok(true);
        };
        let pane_count = self
            .domain
            .window(window)
            .map(|win| win.layout.pane_count())
            .unwrap_or(0);
        if pane_count > 1 {
            self.close_pane(pane)?;
            return Ok(true);
        }
        if self.tab_count() > 1 {
            let ids = self.window_ids();
            let Some(index) = ids.iter().position(|id| *id == window) else {
                return Ok(false);
            };
            return self.close_tab_at(index);
        }
        Ok(false)
    }

    fn enter_placeholder(&mut self, pane: PaneId) -> Result<bool> {
        self.unzoom()?;
        let Some(runtime) = self.panes.get_mut(&pane) else {
            return Ok(false);
        };
        if runtime.placeholder.is_some() {
            return Ok(false);
        }
        let reason = runtime.log_exit_reason.clone().unwrap_or_else(|| {
            let status = runtime
                .session
                .as_mut()
                .and_then(|session| session.try_wait().ok().flatten());
            format_exit_status(status)
        });
        let name = runtime
            .attach_name
            .clone()
            .unwrap_or_else(|| "session".into());
        runtime.placeholder = Some(Placeholder {
            reason: reason.clone(),
            gone: false,
        });
        runtime.child_alive = false;
        write_placeholder_screen(runtime, &name, &reason, false);
        Ok(true)
    }

    pub(crate) fn mark_attach_session(&mut self, pane: PaneId, id: String, name: String) {
        if let Some(runtime) = self.panes.get_mut(&pane) {
            runtime.attach_session = Some(id);
            runtime.attach_name = Some(name);
        }
    }

    /// Restore a saved seat without starting a command or an attach helper.
    pub(crate) fn saved_session_placeholder(&mut self, pane: PaneId, name: &str) {
        if let Some(runtime) = self.panes.get_mut(&pane) {
            runtime.placeholder = Some(Placeholder {
                reason: "session is stopped".into(),
                gone: false,
            });
            write_placeholder_screen(runtime, name, "session is stopped", false);
        }
    }

    /// Keep a paintable empty slot without a PTY or an input connection.
    pub(crate) fn empty_space_view(&mut self, pane: PaneId) -> Result<()> {
        let old = self
            .panes
            .get(&pane)
            .ok_or_else(|| anyhow::anyhow!("missing pane"))?;
        let (tx, _rx) = mpsc::sync_channel(1);
        let (_grant_tx, grant_rx) = mpsc::channel();
        let mut empty = PaneRuntime::assemble(
            Backing::Empty,
            tx,
            grant_rx,
            old.cols,
            old.outer_rows,
            old.experimental_rich,
            old.cell_w,
            old.cell_h,
        )?;
        empty.child_alive = false;
        empty.placeholder = Some(Placeholder {
            reason: "empty space".into(),
            gone: false,
        });
        empty.emulator.feed(
            b"\x1b[2J\x1b[HEmpty space\r\nMove a session here or create a new space with +.\r\n",
        );
        self.panes.insert(pane, empty);
        Ok(())
    }

    pub(crate) fn remote_pane_id(&self, pane: PaneId) -> Option<u64> {
        self.panes.get(&pane)?.log.as_ref().map(|log| log.pane_id)
    }

    /// True when the pane paints from a pane-log subscription (PT-111).
    pub(crate) fn is_log_backed(&self, pane: PaneId) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|runtime| runtime.log.is_some())
    }

    /// Replace a nested PTY attach with a log replica of `session_key`.
    ///
    /// Used when a host pane's shell ran `pmux-attach` (PT-306). Failure
    /// leaves the PTY child in place.
    pub(crate) fn promote_to_log_replica(
        &mut self,
        pane: PaneId,
        session_key: &str,
        session_name: &str,
        socket: &Path,
    ) -> Result<bool> {
        if attach_log::pty_fallback_requested() {
            return Ok(false);
        }
        let Some(runtime) = self.panes.get(&pane) else {
            return Ok(false);
        };
        if runtime.log.is_some() {
            return Ok(true);
        }
        let connection = match attach_log::LogConnection::open_on(socket, session_key) {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("prismattyc-host: promote session {session_key} failed: {error:#}");
                return Ok(false);
            }
        };
        let cols = runtime.cols;
        let rows = runtime.outer_rows;
        let cell_w = runtime.cell_w;
        let cell_h = runtime.cell_h;
        let experimental_rich = runtime.experimental_rich;
        let resolved = connection.session_key().to_string();
        let mut replacement = PaneRuntime::spawn_log_backed(
            pane,
            connection,
            cols,
            rows,
            experimental_rich,
            self.wake.clone(),
            cell_w,
            cell_h,
        )?;
        replacement.attach_session = Some(resolved);
        replacement.attach_name = Some(session_name.to_string());
        if let Some(old) = self.panes.get_mut(&pane) {
            if let Some(session) = old.session.as_mut() {
                session.kill_and_reap();
            }
        }
        self.panes.insert(pane, replacement);
        Ok(true)
    }

    /// Drop an attach mark: an adopted nested attach exited (PT-210).
    pub(crate) fn clear_attach_session(&mut self, pane: PaneId) {
        if let Some(runtime) = self.panes.get_mut(&pane) {
            runtime.attach_session = None;
            runtime.attach_name = None;
        }
    }

    pub(crate) fn is_placeholder(&self, pane: PaneId) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|runtime| runtime.placeholder.is_some())
    }

    pub(crate) fn placeholder_gone(&mut self, pane: PaneId, name: &str) {
        let Some(runtime) = self.panes.get_mut(&pane) else {
            return;
        };
        if let Some(placeholder) = runtime.placeholder.as_mut() {
            placeholder.gone = true;
            let reason = placeholder.reason.clone();
            write_placeholder_screen(runtime, name, &reason, true);
        }
    }

    pub(crate) fn attach_session_of(&self, pane: PaneId) -> Option<&str> {
        self.panes
            .get(&pane)
            .and_then(|runtime| runtime.attach_session.as_deref())
    }

    /// Pane title shown on the strip handle hover (PT-148).
    pub(crate) fn pane_title(&self, pane: PaneId) -> Option<&str> {
        self.panes
            .get(&pane)
            .and_then(|runtime| runtime.title.as_deref())
    }

    /// Set the pane title locally (the server copy follows through
    /// `pmux rename-pane`; the attach re-emits it as OSC 2). A name pins
    /// the title against the child's OSC updates; an empty name unpins it
    /// (PT-221).
    pub(crate) fn set_pane_title(&mut self, pane: PaneId, title: Option<String>) {
        if let Some(runtime) = self.panes.get_mut(&pane) {
            runtime.title = title.filter(|title| !title.trim().is_empty());
            runtime.title_pinned = runtime.title.is_some();
        }
    }

    /// Adopt the server pane title and pin from a mux snapshot (PT-230).
    pub(crate) fn apply_server_pane_title(&mut self, pane: PaneId, title: &str, pinned: bool) {
        if let Some(runtime) = self.panes.get_mut(&pane) {
            runtime.apply_server_title(title, pinned);
        }
    }

    pub(crate) fn attach_name_of(&self, pane: PaneId) -> Option<&str> {
        self.panes
            .get(&pane)
            .and_then(|runtime| runtime.attach_name.as_deref())
    }

    pub(crate) fn reopen_placeholder(
        &mut self,
        pane: PaneId,
        program: &str,
        child_args: &[String],
    ) -> Result<bool> {
        let Some(runtime) = self.panes.get(&pane) else {
            return Ok(false);
        };
        if runtime.placeholder.is_none() {
            return Ok(false);
        }
        let cols = runtime.cols;
        let rows = runtime.outer_rows;
        let cell_w = runtime.cell_w;
        let cell_h = runtime.cell_h;
        let experimental_rich = runtime.experimental_rich;
        let attach_session = runtime.attach_session.clone();
        let attach_name = runtime.attach_name.clone();
        let cwd = runtime.cwd_for_split();
        let mut replacement = PaneRuntime::spawn(
            pane,
            program,
            child_args,
            cols,
            rows,
            cwd.as_deref(),
            experimental_rich,
            self.wake.clone(),
            cell_w,
            cell_h,
            self.space_id.as_deref(),
        )?;
        replacement.attach_session = attach_session;
        replacement.attach_name = attach_name;
        if let Some(old) = self.panes.get_mut(&pane) {
            if let Some(session) = old.session.as_mut() {
                session.kill_and_reap();
            }
        }
        self.panes.insert(pane, replacement);
        Ok(true)
    }

    pub(crate) fn all_children_exited(&self) -> bool {
        self.panes
            .values()
            .all(|pane| !pane.child_alive && pane.placeholder.is_none())
    }

    pub(crate) fn retain_local_terminal(&mut self, pane: PaneId) {
        if let Some(runtime) = self.panes.get_mut(&pane) {
            runtime.keep_local = true;
        }
    }

    pub(crate) fn is_retained_local_terminal(&self, pane: PaneId) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|runtime| runtime.keep_local)
    }

    pub(crate) fn new_tab(&mut self, program: &str, child_args: &[String]) -> Result<WindowId> {
        let (window, pane) = self
            .domain
            .create_window(self.session_id(), "tab")
            .context("create tab")?;
        let rects = match self.window_rects(window) {
            Ok(rects) => rects,
            Err(error) => {
                let _ = self.domain.destroy_window(window);
                return Err(error);
            }
        };
        let root = rects
            .iter()
            .find_map(|(id, rect)| (*id == pane).then_some(*rect))
            .context("new tab pane missing from geometry")?;
        let (pty_cols, pty_rows) = self.geom.content_cells(root);
        let runtime = match PaneRuntime::spawn(
            pane,
            program,
            child_args,
            pty_cols,
            pty_rows,
            None,
            self.experimental_rich,
            self.wake.clone(),
            self.geom.cell_w,
            self.geom.cell_h,
            self.space_id.as_deref(),
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self.domain.destroy_window(window);
                return Err(error);
            }
        };
        self.panes.insert(pane, runtime);
        self.last_window = self.view.window;
        self.view.window = Some(window);
        self.view.set_focused_pane(window, pane);
        if let Err(error) = self.apply_rects(rects, self.geom) {
            self.panes.remove(&pane);
            let _ = self.domain.destroy_window(window);
            return Err(error);
        }
        Ok(window)
    }

    pub(crate) fn close_tab(&mut self) -> Result<bool> {
        let ids = self.window_ids();
        let window = self.active_window();
        let index = ids.iter().position(|id| *id == window).unwrap_or(0);
        self.close_tab_at(index)
    }

    /// Leave the current session view without destroying mux-server children.
    ///
    /// Extra tabs close like `C-S-Q`. The last tab tells the host to exit so
    /// attach children drop and the server-owned session stays.
    pub(crate) fn detach_view(&mut self) -> Result<DetachView> {
        if self.tab_count() > 1 {
            self.close_tab()?;
            return Ok(DetachView::ClosedTab);
        }
        Ok(DetachView::ExitHost)
    }

    pub(crate) fn close_tab_at(&mut self, index: usize) -> Result<bool> {
        let ids = self.window_ids();
        if ids.len() <= 1 {
            return Ok(false);
        }
        let Some(&window) = ids.get(index) else {
            return Ok(false);
        };
        let next = if window == self.active_window() {
            if index == 0 {
                ids[1]
            } else {
                ids[index - 1]
            }
        } else {
            self.active_window()
        };
        let next_rects = self.window_rects(next)?;
        self.apply_rects(next_rects, self.geom)?;
        let panes = self
            .domain
            .window(window)
            .map(|win| win.layout.panes())
            .unwrap_or_default();
        self.domain.destroy_window(window).context("destroy tab")?;
        for pane in panes {
            self.panes.remove(&pane);
        }
        if self
            .zoomed
            .is_some_and(|zoomed| !self.panes.contains_key(&zoomed))
        {
            self.zoomed = None;
        }
        // The closed tab cannot be returned to; the tab we land on is the
        // sensible "last" only if it was already the previous one.
        if self.last_window == Some(window) {
            self.last_window = None;
        }
        self.last_pane.remove(&window);
        self.view.window = Some(next);
        self.view.pane_focus.remove(&window);
        Ok(true)
    }

    pub(crate) fn select_tab(&mut self, index: usize) -> Result<bool> {
        let ids = self.window_ids();
        let Some(&window) = ids.get(index) else {
            return Ok(false);
        };
        if Some(window) == self.view.window {
            return Ok(false);
        }
        let prior = self.focused_id();
        if let Some(old) = self.panes.get_mut(&prior) {
            if old.is_active() {
                old.finish_watch = true;
            }
        }
        self.revoke_rich_focus(prior);
        let rects = self.window_rects(window)?;
        self.last_window = self.view.window;
        self.view.window = Some(window);
        self.apply_rects(rects, self.geom)?;
        if let Some(focus) = self.view.focused_pane(window) {
            if let Some(runtime) = self.panes.get_mut(&focus) {
                runtime.unseen_output = false;
                runtime.attention = None;
                runtime.finish_watch = false;
            }
        }
        Ok(true)
    }

    pub(crate) fn rename_window(&mut self, window: WindowId, title: &str) -> Result<bool> {
        self.domain.rename_window(window, title)?;
        Ok(true)
    }

    /// Presentation index for a pixel in the top tab strip, if any.
    /// Slots sit in the window-padded content box so they stay aligned
    /// with `rasterize_tab_strip`. `close` is the right-edge close target.
    #[cfg(test)]
    pub(crate) fn tab_hit_at_px(
        &self,
        px: usize,
        py: usize,
        stride_px: usize,
    ) -> Option<(usize, bool)> {
        match self.tab_strip_hit(px, py, stride_px, false) {
            Some(StripHit::Tab { index, close }) => Some((index, close)),
            Some(StripHit::Pane { tab, .. }) => Some((tab, false)),
            _ => None,
        }
    }

    /// Strip hit testing. `reserve_end` leaves a one-cell empty drop
    /// target on the right so a drag can create a tab even when only
    /// one tab is showing.
    pub(crate) fn tab_strip_hit(
        &self,
        px: usize,
        py: usize,
        stride_px: usize,
        reserve_end: bool,
    ) -> Option<StripHit> {
        let n = self.tab_count();
        let chrome = self.geom.top_chrome_px;
        let py = py.checked_sub(self.geom.tab_strip_y())?;
        if n == 0 || chrome == 0 || py >= chrome || stride_px == 0 {
            return None;
        }
        let origin = effective_tab_end_pad(self.geom.window_pad, stride_px);
        let right = stride_px.saturating_sub(origin);
        if px < origin || px >= right {
            return None;
        }
        let end_w = if reserve_end {
            self.geom.cell_w.max(1)
        } else {
            0
        };
        if reserve_end && px >= right.saturating_sub(end_w) {
            return Some(StripHit::EmptyEnd);
        }
        let inner_stride = stride_px.saturating_sub(end_w);
        let index = (0..n).find(|&i| {
            tab_slot_bounds(i, n, inner_stride, self.geom.window_pad, self.geom.rail_gap)
                .is_some_and(|(x0, width)| px >= x0 && px < x0.saturating_add(width))
        })?;
        let (x0, width) = tab_slot_bounds(
            index,
            n,
            inner_stride,
            self.geom.window_pad,
            self.geom.rail_gap,
        )?;
        let title_h = self.geom.cell_h.max(1);
        let handle_row = chrome > title_h;
        let in_handle_row = handle_row && py >= title_h;
        let close = !in_handle_row
            && tab_close_left_with_inset(x0, width, self.geom.cell_w, self.geom.inner_pad)
                .is_some_and(|left| px >= left);
        if !close {
            let panes = self
                .window_at_tab(index)
                .and_then(|window| self.domain.window(window).map(|win| win.layout.panes()))
                .unwrap_or_default();
            if panes.len() > 1 && (!handle_row || in_handle_row) {
                let handle_w = pane_handle_w(self.geom.cell_w);
                let handle_origin = x0.saturating_add(self.geom.inner_pad);
                for (i, pane) in panes.iter().enumerate() {
                    let hx0 = handle_origin.saturating_add(i.saturating_mul(handle_w));
                    if px >= hx0
                        && px < hx0.saturating_add(handle_w)
                        && hx0 + handle_w <= x0 + width
                    {
                        return Some(StripHit::Pane {
                            tab: index,
                            pane: *pane,
                            handle: i,
                        });
                    }
                }
            }
        }
        Some(StripHit::Tab { index, close })
    }

    /// Every split of the active window as a draggable divider, in
    /// depth-first order. Empty when zoomed or single-pane.
    pub(crate) fn dividers(&self) -> Vec<Divider> {
        if self.zoomed.is_some() {
            return Vec::new();
        }
        let Some(window) = self.domain.window(self.active_window()) else {
            return Vec::new();
        };
        let rects: HashMap<PaneId, CellRect> = self.rects.iter().copied().collect();
        let mut out = Vec::new();
        collect_dividers(&window.layout, &rects, &mut Vec::new(), &mut out);
        out
    }

    /// The divider under a window pixel, if any.
    pub(crate) fn divider_at(&self, px: usize, py: usize, slop: usize) -> Option<Divider> {
        self.dividers().into_iter().find(|divider| {
            let (x, y, w, h) = self.geom.divider_px(divider, slop);
            px >= x && px < x.saturating_add(w) && py >= y && py < y.saturating_add(h)
        })
    }

    /// Set the ratio of the split at `path` in the active window and
    /// re-fit every pane. `Ok(false)` when the new ratio would violate the
    /// minimum pane size (the layout is left unchanged).
    pub(crate) fn resize_split(&mut self, path: &[bool], ratio: f64) -> Result<bool> {
        let window = self.active_window();
        let Some(current) = self.domain.window(window).map(|win| win.layout.clone()) else {
            return Ok(false);
        };
        let ratio = ratio.clamp(0.02, 0.98);
        let Some(next) = with_ratio(&current, path, ratio) else {
            return Ok(false);
        };
        self.domain
            .set_layout(window, next)
            .context("resize split")?;
        match self.window_rects(window) {
            Ok(rects) => {
                self.apply_rects(rects, self.geom)?;
                Ok(true)
            }
            Err(_) => {
                self.domain
                    .set_layout(window, current)
                    .context("restore split")?;
                Ok(false)
            }
        }
    }

    /// Replace the active window's tree and re-fit every pane; `Ok(false)`
    /// when the layout cannot fit (left unchanged) (PT-125).
    fn replace_layout(&mut self, next: PaneLayout) -> Result<bool> {
        let window = self.active_window();
        let Some(current) = self.domain.window(window).map(|win| win.layout.clone()) else {
            return Ok(false);
        };
        self.domain
            .set_layout(window, next)
            .context("replace layout")?;
        match self.window_rects(window) {
            Ok(rects) => {
                self.apply_rects(rects, self.geom)?;
                Ok(true)
            }
            Err(_) => {
                self.domain
                    .set_layout(window, current)
                    .context("restore layout")?;
                Ok(false)
            }
        }
    }

    /// Exchange the focused pane with its neighbour `delta` slots away in
    /// depth-first order (wrapping); focus follows the pane (PT-125).
    pub(crate) fn swap_focused(&mut self, delta: i32) -> Result<bool> {
        self.unzoom()?;
        let window = self.active_window();
        let Some(layout) = self.domain.window(window).map(|win| win.layout.clone()) else {
            return Ok(false);
        };
        let order = layout.panes();
        let n = order.len();
        let focused = self.focused_id();
        let Some(index) = order.iter().position(|p| *p == focused) else {
            return Ok(false);
        };
        if n < 2 {
            return Ok(false);
        }
        let other = order[(index as i32 + delta).rem_euclid(n as i32) as usize];
        let Some(next) = layout.swap_leaves(focused, other) else {
            return Ok(false);
        };
        self.replace_layout(next)
    }

    /// Rotate every pane of the active window one slot (+1 forward, -1
    /// back); focus stays with the same pane (PT-125).
    pub(crate) fn rotate_panes(&mut self, delta: i32) -> Result<bool> {
        self.unzoom()?;
        let window = self.active_window();
        let Some(layout) = self.domain.window(window).map(|win| win.layout.clone()) else {
            return Ok(false);
        };
        let Some(next) = layout.rotate_leaves(delta) else {
            return Ok(false);
        };
        self.replace_layout(next)
    }

    pub(crate) fn reorder_tab(&mut self, from: usize, to: usize) -> Result<bool> {
        self.unzoom()?;
        self.domain
            .reorder_window(self.session_id(), from, to)
            .context("reorder tab")
    }

    pub(crate) fn move_pane_to_new_tab(&mut self, pane: PaneId) -> Result<bool> {
        let Some(src) = self.domain.pane_owner(pane) else {
            return Ok(false);
        };
        if self
            .domain
            .window(src)
            .is_some_and(|window| window.layout.pane_count() == 1)
        {
            return Ok(false);
        }
        self.unzoom()?;
        let window = self
            .domain
            .open_window_with_pane(self.session_id(), "tab", pane)
            .context("new tab from pane")?;
        self.last_window = self.view.window;
        self.view.window = Some(window);
        self.view.set_focused_pane(window, pane);
        self.retarget_src_focus_after_move(src, pane);
        let rects = self.window_rects(window)?;
        self.apply_rects(rects, self.geom)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn tab_index_at_px(&self, px: usize, py: usize, stride_px: usize) -> Option<usize> {
        self.tab_hit_at_px(px, py, stride_px)
            .map(|(index, _)| index)
    }

    pub(crate) fn window_at_tab(&self, index: usize) -> Option<WindowId> {
        self.window_ids().get(index).copied()
    }

    pub(crate) fn cycle_tab(&mut self, delta: i32) -> Result<bool> {
        let ids = self.window_ids();
        if ids.len() <= 1 {
            return Ok(false);
        }
        let current = ids
            .iter()
            .position(|id| *id == self.active_window())
            .unwrap_or(0);
        let len = ids.len() as i32;
        let next = (current as i32 + delta).rem_euclid(len) as usize;
        self.select_tab(next)
    }

    /// Jump back to the pane that was focused before the current one in
    /// this tab (tmux `last-pane`). False when there is none or it is gone.
    pub(crate) fn focus_last_pane(&mut self) -> bool {
        let window = self.active_window();
        let Some(pane) = self.last_pane.get(&window).copied() else {
            return false;
        };
        if pane == self.focused_id() {
            return false;
        }
        if !self.panes.contains_key(&pane) {
            self.last_pane.remove(&window);
            return false;
        }
        self.focus(pane)
    }

    /// Select the tab that was active before the current one (tmux
    /// `last-window`). False when there is none or it was closed.
    pub(crate) fn select_last_tab(&mut self) -> Result<bool> {
        let Some(window) = self.last_window else {
            return Ok(false);
        };
        let Some(index) = self.window_ids().iter().position(|id| *id == window) else {
            self.last_window = None;
            return Ok(false);
        };
        self.select_tab(index)
    }

    pub(crate) fn move_focused_to_tab(&mut self, index: usize) -> Result<bool> {
        let ids = self.window_ids();
        let Some(&dest) = ids.get(index) else {
            return Ok(false);
        };
        self.move_pane_to_window(self.focused_id(), dest)
    }

    /// Move `pane` onto `dest`. Identity is preserved (ADR-0008).
    pub(crate) fn move_pane_to_window(&mut self, pane: PaneId, dest: WindowId) -> Result<bool> {
        let Some(src) = self
            .domain
            .pane_owner(pane)
            .or_else(|| Some(self.active_window()))
        else {
            return Ok(false);
        };
        if dest == src {
            return Ok(false);
        }
        self.unzoom()?;
        let in_dest = |pane: PaneId| {
            self.domain
                .window(dest)
                .is_some_and(|window| window.layout.contains_pane(pane))
        };
        let target = self
            .view
            .focused_pane(dest)
            .filter(|pane| in_dest(*pane))
            .or_else(|| {
                self.domain
                    .window(dest)
                    .and_then(|window| window.layout.panes().first().copied())
            });
        let Some(target) = target else {
            return Ok(false);
        };
        let probe = Some((
            self.cols,
            self.rows,
            self.geom.min_cols(),
            self.geom.min_rows(),
        ));
        let prior_domain = self.domain.clone();
        let prior_view = self.view.clone();
        let prior_rects = self.rects.clone();
        self.domain
            .move_pane(src, dest, pane, target, Axis::Horizontal, 0.5, probe, probe)
            .context("move pane to tab")?;
        if self.view.window != Some(dest) {
            self.last_window = self.view.window;
        }
        // The pane leaves `src`: a stale last-pane entry there must not
        // point at it; in `dest` it displaces the previous focus.
        if self.last_pane.get(&src) == Some(&pane) {
            self.last_pane.remove(&src);
        }
        if let Some(previous) = self.view.focused_pane(dest) {
            if previous != pane {
                self.last_pane.insert(dest, previous);
            }
        }
        self.view.window = Some(dest);
        self.view.set_focused_pane(dest, pane);
        self.retarget_src_focus_after_move(src, pane);
        let fitted = self
            .window_rects(dest)
            .and_then(|rects| self.apply_rects(rects, self.geom));
        if let Err(error) = fitted {
            self.domain = prior_domain;
            self.view = prior_view;
            let _ = self.apply_rects(prior_rects, self.geom);
            return Err(error);
        }
        Ok(true)
    }

    pub(crate) fn move_active_tab(&mut self, delta: i32) -> Result<bool> {
        let ids = self.window_ids();
        if ids.len() <= 1 || delta == 0 {
            return Ok(false);
        }
        let current = ids
            .iter()
            .position(|id| *id == self.active_window())
            .unwrap_or(0);
        let dest = (current as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize;
        if dest == current {
            return Ok(false);
        }
        self.reorder_tab(current, dest)
    }

    pub(crate) fn move_focused_to_relative_tab(&mut self, delta: i32) -> Result<bool> {
        let ids = self.window_ids();
        if ids.len() <= 1 {
            return Ok(false);
        }
        let current = ids
            .iter()
            .position(|id| *id == self.active_window())
            .unwrap_or(0);
        let len = ids.len() as i32;
        let next = (current as i32 + delta).rem_euclid(len) as usize;
        self.move_focused_to_tab(next)
    }

    /// Join the focused pane into the previously active tab, else the next tab.
    pub(crate) fn join_focused_pane(&mut self) -> Result<bool> {
        if let Some(dest) = self.last_window {
            if self.window_ids().contains(&dest) {
                if dest != self.active_window() {
                    return self.move_pane_to_window(self.focused_id(), dest);
                }
            } else {
                self.last_window = None;
            }
        }
        self.move_focused_to_relative_tab(1)
    }

    fn retarget_src_focus_after_move(&mut self, src: WindowId, moved: PaneId) {
        if self.domain.window(src).is_none() {
            self.view.pane_focus.remove(&src);
            return;
        }
        if self.view.focused_pane(src) != Some(moved) {
            return;
        }
        if let Some(remaining) = self
            .domain
            .window(src)
            .and_then(|window| window.layout.panes().into_iter().find(|id| *id != moved))
        {
            self.view.set_focused_pane(src, remaining);
        }
    }

    #[cfg(test)]
    pub(crate) fn pane_unseen(&self, pane: PaneId) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|runtime| runtime.unseen_output)
    }

    #[cfg(test)]
    pub(crate) fn active_window_id(&self) -> WindowId {
        self.active_window()
    }

    #[cfg(test)]
    pub(crate) fn window_ids_for_test(&self) -> Vec<WindowId> {
        self.window_ids()
    }

    #[cfg(test)]
    pub(crate) fn pane_alive(&self, pane: PaneId) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|runtime| runtime.child_alive)
    }
}

/// Block until the child is a zombie or gone. Does not reap — `drain`
/// calls `try_wait` so Drop does not signal a recycled PID.
fn wait_for_child_exit(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(target_os = "linux")]
    {
        let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
        loop {
            match std::fs::read_to_string(&stat_path) {
                Err(_) => break,
                Ok(stat) if proc_state_is_zombie(&stat) => break,
                Ok(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        while prismattyc_mux::procinfo::pid_alive(pid) {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_state_is_zombie(stat: &str) -> bool {
    stat.rsplit(')')
        .next()
        .and_then(|rest| rest.split_whitespace().next())
        == Some("Z")
}

fn format_exit_status(status: Option<portable_pty::ExitStatus>) -> String {
    match status {
        Some(status) if status.success() => "exited 0".into(),
        Some(status) => {
            let text = status.to_string();
            if text.is_empty() {
                "exited".into()
            } else {
                text
            }
        }
        None => "exited".into(),
    }
}

fn write_placeholder_screen(runtime: &mut PaneRuntime, name: &str, reason: &str, gone: bool) {
    let mut body = format!("\x1b[2J\x1b[H{name}\r\n{reason}\r\n");
    if gone {
        body.push_str(&format!("session {name} is gone\r\n"));
    }
    body.push_str("Enter to reopen\r\n");
    let _ = runtime.emulator.feed(body.as_bytes());
}

fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < b_end && b_start < a_end
}

/// Visible rects for `window`. A `zoomed` pane that lives in `window`
/// projects as a single leaf over the whole area; the split tree in the
/// domain is not consulted for its siblings (ADR-0007 zoom-as-view).
/// Bounding cells of every leaf under `layout`, from the live rects.
fn subtree_bounds(layout: &PaneLayout, rects: &HashMap<PaneId, CellRect>) -> Option<CellRect> {
    let mut acc: Option<(usize, usize, usize, usize)> = None;
    for pane in layout.panes() {
        let Some(rect) = rects.get(&pane) else {
            continue;
        };
        let (x0, y0, x1, y1) = (
            rect.col,
            rect.row,
            rect.col.saturating_add(rect.cols),
            rect.row.saturating_add(rect.rows),
        );
        acc = Some(match acc {
            None => (x0, y0, x1, y1),
            Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
        });
    }
    acc.map(|(x0, y0, x1, y1)| CellRect {
        col: x0,
        row: y0,
        cols: x1.saturating_sub(x0),
        rows: y1.saturating_sub(y0),
    })
}

fn collect_dividers(
    layout: &PaneLayout,
    rects: &HashMap<PaneId, CellRect>,
    path: &mut Vec<bool>,
    out: &mut Vec<Divider>,
) {
    let PaneLayout::Split(split) = layout else {
        return;
    };
    if let (Some(bounds), Some(second)) = (
        subtree_bounds(layout, rects),
        subtree_bounds(&split.second, rects),
    ) {
        out.push(Divider {
            path: path.clone(),
            axis: split.axis,
            bounds,
            boundary: match split.axis {
                Axis::Horizontal => second.col,
                Axis::Vertical => second.row,
            },
        });
    }
    path.push(false);
    collect_dividers(&split.first, rects, path, out);
    path.pop();
    path.push(true);
    collect_dividers(&split.second, rects, path, out);
    path.pop();
}

/// `layout` with the split at `path` given `ratio`; `None` when the path
/// does not end on a split.
fn with_ratio(layout: &PaneLayout, path: &[bool], ratio: f64) -> Option<PaneLayout> {
    match (layout, path.split_first()) {
        (PaneLayout::Split(split), None) => Some(PaneLayout::Split(prismattyc_mux::Split {
            axis: split.axis,
            ratio,
            first: split.first.clone(),
            second: split.second.clone(),
        })),
        (PaneLayout::Split(split), Some((&second, rest))) => {
            let child = if second { &split.second } else { &split.first };
            let replaced = with_ratio(child, rest, ratio)?;
            let (first, second_child) = if second {
                (split.first.clone(), Box::new(replaced))
            } else {
                (Box::new(replaced), split.second.clone())
            };
            Some(PaneLayout::Split(prismattyc_mux::Split {
                axis: split.axis,
                ratio: split.ratio,
                first,
                second: second_child,
            }))
        }
        (PaneLayout::Leaf(_), _) => None,
    }
}

fn rects_for(
    domain: &Domain,
    window: WindowId,
    cols: usize,
    rows: usize,
    geom: HostGeom,
    zoomed: Option<PaneId>,
) -> Result<Vec<(PaneId, CellRect)>> {
    let layout = &domain.window(window).context("mux window missing")?.layout;
    let zoomed_leaf = zoomed
        .filter(|pane| layout.contains_pane(*pane))
        .map(PaneLayout::leaf);
    Ok(layout_to_rects(
        zoomed_leaf.as_ref().unwrap_or(layout),
        CellRect {
            col: 0,
            row: 0,
            cols,
            rows,
        },
        geom.min_cols(),
        geom.min_rows(),
    )?)
}

/// `pixel_*` are the full visible window in pixels (`winsize.ws_xpixel` /
/// `ws_ypixel`), not per-cell. Producers (Claude Code Kitty graphics) divide
/// by `cols`/`rows` to get cell size; 0×0 made them emit a tiny bitmap.
fn pty_size(cols: usize, rows: usize, cell_w: usize, cell_h: usize) -> PtySize {
    prismattyc_emulator::pty_size_with_cell_pixels(cols, rows, cell_w as u32, cell_h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn pty_size_reports_window_pixels_not_per_cell() {
        let size = pty_size(80, 24, 10, 23);
        assert_eq!(size.cols, 80);
        assert_eq!(size.rows, 24);
        assert_eq!(size.pixel_width, 800, "ws_xpixel = cols * cell_w");
        assert_eq!(size.pixel_height, 552, "ws_ypixel = rows * cell_h");
        let tiny = pty_size(80, 24, 0, 0);
        assert_eq!(tiny.pixel_width, 80);
        assert_eq!(tiny.pixel_height, 24);
    }

    #[test]
    fn a_plain_shell_pane_is_not_log_backed() {
        let runtime = MuxRuntime::spawn("/bin/sh", &[], 8, 4).expect("test mux runtime");
        let pane = runtime.focused_id();
        assert!(!runtime.is_log_backed(pane));
    }

    #[test]
    fn take_pane_damage_returns_and_clears_damage_without_a_window() {
        let mut runtime = MuxRuntime::spawn("sh", &[], 4, 2).expect("test mux runtime");
        let pane = runtime.focused_id();
        let _ = runtime.focused_mut().emulator.feed(b"X");

        let damage = runtime.take_pane_damage();
        let pane_damage = damage.get(&pane).expect("focused pane damage");
        assert!(pane_damage.dirty_cell_count() > 0);

        let drained = runtime.take_pane_damage();
        let drained_damage = drained.get(&pane).expect("focused pane damage");
        assert_eq!(drained_damage.dirty_cell_count(), 0);
        assert!(drained_damage.scroll_events().is_empty());
    }

    fn visible_text(pane: &PaneRuntime) -> String {
        pane.emulator
            .screen()
            .viewport_range()
            .map(|range| pane.emulator.screen().extract_text(range))
            .unwrap_or_default()
    }

    fn drain_until(runtime: &mut MuxRuntime, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            let _ = runtime.drain_all();
            if runtime
                .panes
                .values()
                .any(|pane| visible_text(pane).contains(needle))
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {needle:?}");
    }

    /// Drain until one specific pane shows `needle`.
    ///
    /// Prefer this over [`drain_until`] when the awaited text is produced by a
    /// command whose typed form the shell echoes back: an `any pane` marker
    /// match can fire on the echoed command line before the command's real
    /// output (e.g. `stty size`) reaches the screen, so wait for the output
    /// itself in the pane that runs it.
    fn drain_pane_until(runtime: &mut MuxRuntime, pane: PaneId, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            let _ = runtime.drain_all();
            if runtime
                .panes
                .get(&pane)
                .is_some_and(|pane| visible_text(pane).contains(needle))
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for {needle:?} in pane {pane}: {:?}",
            runtime.panes.get(&pane).map(visible_text)
        );
    }

    /// Drain a pane without sending input. Policy seam assertions must remain
    /// valid after an idle host has had at least two seconds to poll.
    fn idle_for(runtime: &mut MuxRuntime, duration: Duration) {
        assert!(
            duration >= Duration::from_secs(2),
            "host-state assertions require at least two seconds of idle time"
        );
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let _ = runtime.drain_all();
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn drain_title_until(runtime: &mut MuxRuntime, pane: PaneId, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let _ = runtime.drain_all();
            if runtime.pane_title(pane) == Some(expected) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for title {expected:?}, got {:?}",
            runtime.pane_title(pane)
        );
    }

    struct PrivateMuxServer {
        child: Child,
        socket: PathBuf,
        data: PathBuf,
    }

    impl Drop for PrivateMuxServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.socket);
            let _ = std::fs::remove_dir_all(&self.data);
        }
    }

    fn mux_binary(name: &str) -> PathBuf {
        crate::test_support::mux_bin_dir().join(name)
    }

    fn private_mux_server() -> PrivateMuxServer {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("prismattyc-host-seam-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create private mux test directory");
        let socket = root.join("pmuxd.sock");
        let data = root.join("data");
        std::fs::create_dir_all(&data).expect("create private mux data directory");
        let pmuxd = mux_binary("pmuxd");
        let mut command = Command::new(pmuxd);
        for key in [
            "PMUX_SOCKET",
            "PMUX_PANE_LOG",
            "PRISMATTYC_PANE_ID",
            "PMUX_SPACE",
        ] {
            command.env_remove(key);
        }
        let guard = PrivateMuxServer {
            child: command
                .arg("--socket")
                .arg(&socket)
                .args(["--", "/bin/sh", "-c", "exec sleep 999"])
                .env("XDG_DATA_HOME", &data)
                .env("XDG_CONFIG_HOME", &data)
                .env("PMUX_SESSION_AGENTS", data.join("session-agents.json"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn target/debug/pmuxd; build prismattyc-mux first"),
            socket: socket.clone(),
            data: root,
        };
        for _ in 0..200 {
            if socket.exists() {
                return guard;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("private pmuxd did not publish {}", socket.display());
    }

    fn shell_quote(value: &Path) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
    }

    fn real_attach_command(socket: &Path) -> String {
        let attach = mux_binary("pmux-attach");
        format!(
            "{} --socket {} --session default --watch & wait",
            shell_quote(&attach),
            shell_quote(socket)
        )
    }

    #[test]
    fn real_nested_attach_promotes_to_a_log_replica() {
        let server = private_mux_server();
        let command = real_attach_command(&server.socket);
        let mut runtime = MuxRuntime::spawn("/bin/sh", &["-c".into(), command], 80, 24).unwrap();
        let pane = runtime.focused_id();
        let root = runtime.panes[&pane]
            .child_pid()
            .expect("fixture shell has a process id");
        let mut adopted = crate::attach_adopt::Adopted::default();
        let directory = crate::attach_log::session_directory_at(&server.socket);
        assert!(
            !directory.is_empty(),
            "private pmuxd must expose its default session"
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let _ = runtime.drain_all();
            let _ = crate::attach_adopt::adopt_candidates(
                &mut runtime,
                &mut adopted,
                &[(pane, root)],
                &server.socket,
                &directory,
            );
            if runtime.is_log_backed(pane) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "real pmux-attach was not promoted below pane root {root}"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let session_id = runtime
            .attach_session_of(pane)
            .expect("promoted replica keeps its session id")
            .to_string();
        let session_name = runtime
            .attach_name_of(pane)
            .expect("promoted replica keeps its session name")
            .to_string();
        idle_for(&mut runtime, Duration::from_secs(2));
        assert_eq!(runtime.attach_name_of(pane), Some(session_name.as_str()));
        assert_eq!(runtime.attach_session_of(pane), Some(session_id.as_str()));
        assert!(
            runtime.is_log_backed(pane),
            "bash → pmux-attach must become a log replica"
        );
        assert!(
            adopted.by_pane.is_empty(),
            "a promoted replica is not an adopted nested attach"
        );
        assert!(
            runtime.panes[&pane].child_pid().is_none(),
            "promote must kill the nested PTY child"
        );
        let gone = crate::attach_adopt::clear_gone(&mut runtime, &mut adopted, &[pane]);
        assert!(gone.is_empty(), "a replica mark must survive idle");
        assert_eq!(runtime.attach_session_of(pane), Some(session_id.as_str()));
    }

    #[test]
    fn real_osc_timer_respects_pin_until_idle_revert() {
        let script = r#"while :; do printf '\033]0;timer-zero\a'; sleep 0.25; printf '\033]2;timer-two\a'; sleep 0.25; done"#;
        let mut runtime =
            MuxRuntime::spawn("/bin/sh", &["-c".into(), script.into()], 80, 24).unwrap();
        let pane = runtime.focused_id();
        drain_title_until(&mut runtime, pane, "timer-zero");

        runtime.set_pane_title(pane, Some("manual".into()));
        idle_for(&mut runtime, Duration::from_secs(2));
        assert_eq!(runtime.pane_title(pane), Some("manual"));

        runtime.set_pane_title(pane, Some("   ".into()));
        idle_for(&mut runtime, Duration::from_secs(2));
        assert!(
            matches!(runtime.pane_title(pane), Some("timer-zero" | "timer-two")),
            "clearing a manual title must restore OSC titles: {:?}",
            runtime.pane_title(pane)
        );
    }

    #[test]
    fn unseen_v2_quiet_burst_badges_stream_does_not() {
        let now = Instant::now();
        let mut unseen = false;
        let mut last = None;
        let mut watch = false;
        apply_unseen_v2(&mut unseen, &mut last, &mut watch, now, false, true, false);
        assert!(unseen, "first unfocused output after idle badges");

        unseen = false;
        apply_unseen_v2(
            &mut unseen,
            &mut last,
            &mut watch,
            now + Duration::from_millis(10),
            false,
            true,
            false,
        );
        assert!(
            !unseen,
            "spinner/stream without a quiet gap must not re-badge"
        );

        apply_unseen_v2(
            &mut unseen,
            &mut last,
            &mut watch,
            now + QUIET_GAP + Duration::from_millis(20),
            false,
            true,
            false,
        );
        assert!(unseen, "quiet then burst badges");
    }

    #[test]
    fn unseen_v2_finish_while_away_and_bell() {
        let now = Instant::now();
        let mut unseen = false;
        let mut last = Some(now);
        let mut watch = true;
        apply_unseen_v2(
            &mut unseen,
            &mut last,
            &mut watch,
            now + Duration::from_millis(100),
            false,
            false,
            false,
        );
        assert!(!unseen, "still streaming: no finish badge yet");
        apply_unseen_v2(
            &mut unseen,
            &mut last,
            &mut watch,
            now + QUIET_GAP + Duration::from_millis(10),
            false,
            false,
            false,
        );
        assert!(unseen, "silence after unfocused stream badges once");
        assert!(!watch);

        unseen = false;
        apply_unseen_v2(&mut unseen, &mut last, &mut watch, now, false, false, true);
        assert!(unseen, "BEL on unfocused pane badges immediately");

        unseen = true;
        watch = true;
        apply_unseen_v2(&mut unseen, &mut last, &mut watch, now, true, false, true);
        assert!(!unseen, "focused pane never badges, including BEL");
        assert!(!watch);
    }

    #[test]
    fn unfocused_bel_badges_focused_bel_does_not() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.focus(first);
        runtime.panes.get_mut(&second).unwrap().unseen_output = false;
        let _ = runtime
            .panes
            .get_mut(&second)
            .unwrap()
            .emulator
            .feed(b"\x07");
        let _ = runtime.drain_all();
        assert!(runtime.panes[&second].unseen_output);
        runtime.focus(second);
        assert!(!runtime.panes[&second].unseen_output);
        let _ = runtime.focused_mut().emulator.feed(b"\x07");
        let _ = runtime.drain_all();
        assert!(!runtime.panes[&second].unseen_output);
    }

    #[test]
    fn attention_drains_sets_badge_and_focus_clears_without_touching_unseen() {
        // Keep the child quiet so unrelated shell prompts cannot race this
        // attention-only assertion and set the unseen-output badge.
        let mut runtime = MuxRuntime::spawn("/bin/cat", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/cat", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.focus(first);
        runtime.panes.get_mut(&second).unwrap().unseen_output = false;
        let _ = runtime
            .panes
            .get_mut(&second)
            .unwrap()
            .emulator
            .feed(b"\x1b]9;permission needed\x07");
        let _ = runtime.drain_all();
        let pending = runtime.take_pending_attentions();
        assert_eq!(pending, vec![(second, "permission needed".to_string())]);
        assert!(runtime.panes[&second].attention.is_some());
        assert!(!runtime.panes[&second].unseen_output);
        let tab = runtime.tab_infos().into_iter().next().unwrap();
        assert!(tab.attention);

        assert!(runtime.focus(second));
        assert!(runtime.panes[&second].attention.is_none());
        assert!(!runtime.panes[&second].unseen_output);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_detects_zombie_state() {
        assert!(proc_state_is_zombie("123 (sh) Z 1 123 123"));
        assert!(!proc_state_is_zombie("123 (sh) S 1 123 123"));
        assert!(proc_state_is_zombie("9 (a b) Z 1"));
    }

    #[test]
    fn host_geometry_applies_outer_gap_and_inner_padding() {
        let geom = HostGeom {
            cell_w: 10,
            cell_h: 20,
            window_pad: 5,
            slack_x: 0,
            slack_y: 0,
            pane_gap: 5,
            rail_gap: 5,
            inner_pad: 5,
            top_chrome_px: 0,
            scrollbar_gutter_px: 8,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        };
        let rect = CellRect {
            col: 2,
            row: 1,
            cols: 10,
            rows: 5,
        };

        assert_eq!(geom.pane_slot_px(rect), (27, 27, 95, 95));
        // 85px available inside the padding. Horizontally the 8px scrollbar
        // gutter comes off first, leaving 77 for 7 columns of 10 = 70, so 7
        // surplus splits 3 left. Vertically 4 rows of 20 = 80 of 85, so 5
        // surplus splits 2 above. The box is whole cells: no strip is left
        // for the pane backdrop to show through.
        assert_eq!(geom.pane_content_px(rect), (35, 34, 70, 80));
        assert_eq!(geom.content_cells(rect), (7, 4));
        assert_eq!((geom.min_cols(), geom.min_rows()), (5, 2));

        let with_strip = HostGeom {
            top_chrome_px: 16,
            ..geom
        };
        let (_, y, _, _) = with_strip.pane_slot_px(rect);
        assert_eq!(y, 43);
    }

    #[test]
    fn scrollbar_gutter_sits_in_padding_when_inner_pad_fits() {
        assert_eq!(scrollbar_gutter_for(0), SCROLLBAR_GUTTER_PX);
        assert_eq!(scrollbar_gutter_for(7), SCROLLBAR_GUTTER_PX);
        assert_eq!(scrollbar_gutter_for(8), 0);
        assert_eq!(scrollbar_gutter_for(12), 0);
    }

    #[test]
    fn content_cells_leave_room_for_the_scrollbar_across_a_cell_boundary() {
        // Slot width is `cols * cell_w - pane_gap`. Leftover that used to sit
        // under the bar is `(slot_w - 2*inner_pad) % cell_w`. Sweep both.
        let cell_w = 10usize;
        let cols = 12usize;
        for inner_pad in 0..cell_w {
            for pane_gap in 0..cell_w {
                let geom = HostGeom {
                    cell_w,
                    cell_h: 20,
                    window_pad: 0,
                    slack_x: 0,
                    slack_y: 0,
                    pane_gap,
                    rail_gap: pane_gap,
                    inner_pad,
                    top_chrome_px: 0,
                    scrollbar_gutter_px: scrollbar_gutter_for(inner_pad),
                    rail_side: RailSide::Off,
                    rail_px: 0,
                    rail_chip_cols: 0,
                };
                let rect = CellRect {
                    col: 0,
                    row: 0,
                    cols,
                    rows: 8,
                };
                let (_, _, content_w, _) = geom.pane_content_px(rect);
                let leftover = content_w % cell_w;
                assert!(
                    geom.cells_fit_left_of_gutter(rect),
                    "pad={inner_pad} gap={pane_gap} leftover={leftover} last cell must sit left of the bar"
                );
                let (cx, _, _, _) = geom.pane_content_px(rect);
                let (n_cols, _) = geom.content_cells(rect);
                let (bar_x, _, _, _) = geom.scrollbar_px(rect);
                let last_cell_right = cx + n_cols * cell_w;
                assert!(
                    last_cell_right <= bar_x,
                    "pad={inner_pad} gap={pane_gap} leftover={leftover} cell_right={last_cell_right} bar_x={bar_x}"
                );
            }
        }
    }

    fn slot_gap(runtime: &MuxRuntime) -> (usize, usize, usize) {
        let mut slots: Vec<_> = runtime
            .rects()
            .map(|(_, rect)| {
                let (x, y, w, _) = runtime.geom().pane_slot_px(rect);
                (x, y, w)
            })
            .collect();
        slots.sort_by_key(|slot| slot.0);
        assert_eq!(slots.len(), 2);
        (slots[0].0 + slots[0].2, slots[1].0, slots[0].1)
    }

    #[test]
    fn panes_first_then_new_tab_then_select_restores_gap() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let base = HostGeom {
            cell_w: 8,
            cell_h: 16,
            window_pad: 5,
            slack_x: 0,
            slack_y: 0,
            pane_gap: 5,
            rail_gap: 5,
            inner_pad: 5,
            top_chrome_px: 0,
            scrollbar_gutter_px: 8,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        };
        runtime.set_geom(base).unwrap();
        assert_eq!(runtime.active_pane_count(), 2);

        runtime.new_tab("/bin/sh", &[]).unwrap();
        let after_new = HostGeom {
            pane_gap: if runtime.active_pane_count() > 1 {
                5
            } else {
                0
            },
            top_chrome_px: 16,
            ..base
        };
        runtime.set_geom(after_new).unwrap();
        assert_eq!(runtime.active_pane_count(), 1);
        assert_eq!(runtime.geom().pane_gap, 0);
        assert_eq!(runtime.tab_count(), 2);

        assert!(runtime.select_tab(0).unwrap());
        assert_eq!(
            runtime.active_pane_count(),
            2,
            "domain leaf count of the split tab, not the 1-pane tab we left"
        );
        let after_back = HostGeom {
            pane_gap: if runtime.active_pane_count() > 1 {
                5
            } else {
                0
            },
            top_chrome_px: 16,
            ..base
        };
        runtime.set_geom(after_back).unwrap();
        assert_eq!(runtime.geom().pane_gap, 5);
        assert_eq!(runtime.geom().window_pad, 5);
        assert_eq!(runtime.geom().top_chrome_px, 16);
        let (left_end, right_start, y) = slot_gap(&runtime);
        assert!(
            right_start >= left_end + 5,
            "gap after panes-first then tab: left_end={left_end} right_start={right_start}"
        );
        assert_eq!(y, 5 + 16 + 2, "window_pad + top_chrome + gap/2");
    }

    #[test]
    fn tabs_first_then_split_keeps_gap() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let chrome = HostGeom {
            cell_w: 8,
            cell_h: 16,
            window_pad: 5,
            slack_x: 0,
            slack_y: 0,
            pane_gap: if runtime.active_pane_count() > 1 {
                5
            } else {
                0
            },
            rail_gap: 5,
            inner_pad: 5,
            top_chrome_px: 16,
            scrollbar_gutter_px: 8,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        };
        runtime.set_geom(chrome).unwrap();
        assert_eq!(runtime.geom().pane_gap, 0);
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let after_split = HostGeom {
            pane_gap: if runtime.active_pane_count() > 1 {
                5
            } else {
                0
            },
            ..chrome
        };
        runtime.set_geom(after_split).unwrap();
        assert_eq!(runtime.active_pane_count(), 2);
        assert_eq!(runtime.geom().pane_gap, 5);
        let (left_end, right_start, y) = slot_gap(&runtime);
        assert!(
            right_start >= left_end + 5,
            "gap after tabs-first then split: left_end={left_end} right_start={right_start}"
        );
        assert_eq!(y, 5 + 16 + 2);
    }

    #[test]
    fn configured_geometry_resizes_every_live_pty_to_its_content_box() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let geom = HostGeom {
            cell_w: 10,
            cell_h: 20,
            window_pad: 5,
            slack_x: 0,
            slack_y: 0,
            pane_gap: 5,
            rail_gap: 5,
            inner_pad: 5,
            top_chrome_px: 0,
            scrollbar_gutter_px: 8,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        };

        runtime.resize_with_geom(80, 24, geom).unwrap();

        assert_eq!(
            (runtime.panes[&first].cols, runtime.panes[&first].rows),
            (37, 23)
        );
        assert_eq!(
            (runtime.panes[&second].cols, runtime.panes[&second].rows),
            (37, 23)
        );
        assert_eq!(runtime.panes[&first].emulator.screen().columns(), 37);
        assert_eq!(runtime.panes[&second].emulator.screen().rows(), 23);
    }

    #[test]
    fn phase2a_live_multi_pty_wrong_pane_input_isolation() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(runtime.panes.len(), 2);
        assert_eq!(runtime.panes[&first].emulator.screen().columns(), 40);
        assert_eq!(runtime.panes[&second].emulator.screen().columns(), 40);

        runtime.focus(first);
        runtime
            .focused()
            .send_bytes(b"printf 'FIRST_ONLY\\n'\n".to_vec())
            .unwrap();
        drain_until(&mut runtime, "FIRST_ONLY");
        assert!(visible_text(&runtime.panes[&first]).contains("FIRST_ONLY"));
        assert!(!visible_text(&runtime.panes[&second]).contains("FIRST_ONLY"));

        runtime.focus(second);
        runtime
            .focused()
            .send_bytes(b"printf 'SECOND_ONLY\\n'\n".to_vec())
            .unwrap();
        drain_until(&mut runtime, "SECOND_ONLY");
        assert!(visible_text(&runtime.panes[&second]).contains("SECOND_ONLY"));
        assert!(!visible_text(&runtime.panes[&first]).contains("SECOND_ONLY"));

        let third = runtime
            .split_focused("/bin/sh", &[], Axis::Vertical, 0.5)
            .unwrap();
        runtime
            .focused()
            .send_bytes(b"printf 'THIRD_ONLY\n'\n".to_vec())
            .unwrap();
        drain_until(&mut runtime, "THIRD_ONLY");
        assert!(visible_text(&runtime.panes[&third]).contains("THIRD_ONLY"));
        assert!(!visible_text(&runtime.panes[&first]).contains("THIRD_ONLY"));
        assert!(!visible_text(&runtime.panes[&second]).contains("THIRD_ONLY"));
        assert_eq!(runtime.panes.len(), 3);

        let total_area: usize = runtime
            .rects
            .iter()
            .map(|(_, rect)| rect.cols * rect.rows)
            .sum();
        assert_eq!(total_area, 80 * 24);
        for (index, (_, first_rect)) in runtime.rects.iter().enumerate() {
            for (_, second_rect) in runtime.rects.iter().skip(index + 1) {
                let separated = first_rect.col + first_rect.cols <= second_rect.col
                    || second_rect.col + second_rect.cols <= first_rect.col
                    || first_rect.row + first_rect.rows <= second_rect.row
                    || second_rect.row + second_rect.rows <= first_rect.row;
                assert!(separated, "pane rectangles overlap");
            }
        }
    }

    #[test]
    fn phase2a_resize_applies_distinct_geometry_to_every_pty_and_emulator() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.25)
            .unwrap();
        runtime.resize(100, 30).unwrap();

        assert_eq!(
            (runtime.panes[&first].cols, runtime.panes[&first].rows),
            (25, 30)
        );
        assert_eq!(
            (runtime.panes[&second].cols, runtime.panes[&second].rows),
            (75, 30)
        );
        assert_eq!(runtime.panes[&first].emulator.screen().columns(), 25);
        assert_eq!(runtime.panes[&second].emulator.screen().columns(), 75);

        runtime.focus(first);
        runtime
            .focused()
            .send_bytes(b"stty size; printf 'FIRST_SIZE\\n'\n".to_vec())
            .unwrap();
        runtime.focus(second);
        runtime
            .focused()
            .send_bytes(b"stty size; printf 'SECOND_SIZE\\n'\n".to_vec())
            .unwrap();
        // Wait for the real `stty size` output in each pane, not the echoed
        // command line (which repeats FIRST_SIZE/SECOND_SIZE before stty runs).
        drain_pane_until(&mut runtime, first, "30 25");
        drain_pane_until(&mut runtime, second, "30 75");
    }

    #[test]
    fn hit_testing_returns_pane_local_cells() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_eq!(runtime.pane_at_cell(3, 4), Some((first, 4, 3)));
        assert_eq!(runtime.pane_at_cell(43, 4), Some((second, 4, 3)));
        assert_eq!(runtime.pane_at_cell(80, 4), None);
    }

    #[test]
    fn failed_child_spawn_rolls_back_split_topology_and_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let prior_focus = runtime.focused_id();
        let prior_rects = runtime.rects.clone();

        let error = runtime
            .split_focused(
                "/definitely/not/a/prism-test-program",
                &[],
                Axis::Horizontal,
                0.5,
            )
            .unwrap_err();

        assert!(error.to_string().contains("spawn"));
        assert_eq!(runtime.focused_id(), prior_focus);
        assert_eq!(runtime.rects, prior_rects);
        assert_eq!(runtime.panes.len(), 1);
        assert_eq!(
            runtime
                .domain
                .window(runtime.active_window_id())
                .unwrap()
                .layout
                .panes(),
            &[prior_focus]
        );
    }

    #[test]
    fn attach_exit_becomes_placeholder_and_does_not_collapse() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.mark_attach_session(second, "2".into(), "seat".into());
        assert!(runtime.toggle_zoom().unwrap());
        runtime.panes.get_mut(&second).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        assert!(runtime.is_placeholder(second));
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(runtime.panes.len(), 2);
        assert_eq!(runtime.active_pane_count(), 2);
        assert!(!runtime.all_children_exited());
        let text = visible_text(&runtime.panes[&second]);
        assert!(text.contains("seat"), "{text:?}");
        assert!(text.contains("Enter to reopen"), "{text:?}");
        assert!(runtime.panes.contains_key(&first));
    }

    #[test]
    fn placeholder_close_intermediate_last_tab_and_last_slot() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime.mark_attach_session(first, "1".into(), "a".into());
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.mark_attach_session(second, "2".into(), "b".into());
        runtime.panes.get_mut(&second).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        runtime.focus(second);
        assert!(runtime.close_focused().unwrap());
        assert_eq!(runtime.panes.len(), 1);
        assert!(runtime.panes.contains_key(&first));

        runtime.new_tab("/bin/sh", &[]).unwrap();
        let extra_pane = runtime.focused_id();
        runtime.mark_attach_session(extra_pane, "3".into(), "c".into());
        runtime.panes.get_mut(&extra_pane).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        assert!(runtime.is_placeholder(extra_pane));
        assert!(runtime.close_focused().unwrap());
        assert_eq!(runtime.tab_count(), 1);

        runtime.panes.get_mut(&first).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        assert!(runtime.is_placeholder(first));
        assert!(!runtime.all_children_exited());
        runtime.focus(first);
        assert!(runtime.close_focused().unwrap());
        assert!(runtime.all_children_exited());
    }

    #[test]
    fn placeholder_reopen_respawns_child() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        runtime.mark_attach_session(pane, "9".into(), "work".into());
        runtime.panes.get_mut(&pane).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        assert!(runtime.is_placeholder(pane));
        assert!(runtime.reopen_placeholder(pane, "/bin/sh", &[]).unwrap());
        assert!(!runtime.is_placeholder(pane));
        assert!(runtime.panes[&pane].child_alive);
        assert_eq!(runtime.attach_session_of(pane), Some("9"));
    }

    #[test]
    fn placeholder_gone_keeps_slot_and_names_the_session() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        runtime.mark_attach_session(pane, "4".into(), "mail".into());
        runtime.panes.get_mut(&pane).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        runtime.placeholder_gone(pane, "mail");
        assert!(runtime.is_placeholder(pane));
        assert!(!runtime.all_children_exited());
        let text = visible_text(&runtime.panes[&pane]);
        assert!(text.contains("session mail is gone"), "{text:?}");
        assert!(text.contains("Enter to reopen"), "{text:?}");
    }

    #[test]
    fn exited_nonfinal_pane_collapses_layout_and_resizes_survivor() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.panes.get_mut(&second).unwrap().child_alive = false;

        assert!(runtime.drain_all().0);
        assert_eq!(runtime.focused_id(), first);
        assert_eq!(runtime.panes.len(), 1);
        assert_eq!(
            (runtime.panes[&first].cols, runtime.panes[&first].rows),
            (80, 24)
        );
        assert_eq!(runtime.rects.len(), 1);
        assert_eq!(runtime.rects[0].0, first);
        assert_eq!(runtime.rects[0].1.cols, 80);
    }

    #[test]
    fn ensure_even_columns_three_makes_even_widths() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 90, 24).unwrap();
        assert!(runtime.ensure_even_columns("/bin/sh", &[], 3).unwrap());
        assert_eq!(runtime.active_pane_count(), 3);
        let widths: Vec<usize> = runtime.rects().map(|(_, r)| r.cols).collect();
        assert_eq!(widths.len(), 3);
        assert_eq!(widths.iter().sum::<usize>(), 90);
        let min = *widths.iter().min().unwrap();
        let max = *widths.iter().max().unwrap();
        assert!(max - min <= 1, "uneven {widths:?}");
    }

    #[test]
    fn ensure_even_columns_two_is_fifty_fifty() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        assert!(runtime.ensure_even_columns("/bin/sh", &[], 2).unwrap());
        assert_eq!(runtime.active_pane_count(), 2);
        let widths: Vec<usize> = runtime.rects().map(|(_, r)| r.cols).collect();
        assert_eq!(widths.iter().sum::<usize>(), 80);
        let min = *widths.iter().min().unwrap();
        let max = *widths.iter().max().unwrap();
        assert!(max - min <= 1, "uneven {widths:?}");
    }

    fn spawn_n_panes(n: usize, cols: usize, rows: usize) -> MuxRuntime {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], cols, rows).unwrap();
        for _ in 1..n {
            runtime
                .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
                .unwrap();
        }
        runtime
    }

    fn layout_shape(layout: &PaneLayout) -> String {
        match layout {
            PaneLayout::Leaf(_) => "L".into(),
            PaneLayout::Split(split) => {
                let axis = match split.axis {
                    Axis::Horizontal => "H",
                    Axis::Vertical => "V",
                };
                format!(
                    "[{axis} {} {}]",
                    layout_shape(&split.first),
                    layout_shape(&split.second)
                )
            }
        }
    }

    #[test]
    fn layout_presets_tree_shape_one_to_four_panes() {
        for n in 1..=4usize {
            let mut runtime = spawn_n_panes(n, 80, 24);
            let ids = runtime.active_layout().panes();
            assert_eq!(ids.len(), n);

            if n == 1 {
                assert_eq!(
                    runtime.apply_preset(LayoutPreset::Single).unwrap(),
                    PresetOutcome::Applied
                );
                assert_eq!(layout_shape(&runtime.active_layout()), "L");
            } else {
                let prior = runtime.active_layout();
                assert_eq!(
                    runtime.apply_preset(LayoutPreset::Single).unwrap(),
                    PresetOutcome::Unchanged("preset single needs one pane")
                );
                assert_eq!(runtime.active_layout(), prior);
                assert_eq!(runtime.active_pane_count(), n);
            }

            let expected_h = match n {
                1 => "L",
                2 => "[H L L]",
                3 => "[H L [H L L]]",
                4 => "[H L [H L [H L L]]]",
                _ => unreachable!(),
            };
            let expected_v = expected_h.replace('H', "V");
            let expected_grid = match n {
                1 => "L",
                2 => "[V L L]",
                3 => "[V [H L L] L]",
                4 => "[V [H L L] [H L L]]",
                _ => unreachable!(),
            };

            assert_eq!(
                runtime.apply_preset(LayoutPreset::SplitH).unwrap(),
                PresetOutcome::Applied
            );
            assert_eq!(runtime.active_pane_count(), n, "split-h must not spawn");
            assert_eq!(layout_shape(&runtime.active_layout()), expected_h);
            assert_eq!(runtime.active_layout().panes(), ids);

            assert_eq!(
                runtime.apply_preset(LayoutPreset::SplitV).unwrap(),
                PresetOutcome::Applied
            );
            assert_eq!(runtime.active_pane_count(), n, "split-v must not spawn");
            assert_eq!(layout_shape(&runtime.active_layout()), expected_v);
            assert_eq!(runtime.active_layout().panes(), ids);

            assert_eq!(
                runtime.apply_preset(LayoutPreset::Grid).unwrap(),
                PresetOutcome::Applied
            );
            assert_eq!(runtime.active_pane_count(), n, "grid must not spawn");
            assert_eq!(layout_shape(&runtime.active_layout()), expected_grid);
            assert_eq!(runtime.active_layout().panes(), ids);
        }
    }

    #[test]
    fn layout_preset_single_leaves_zoom_when_noop() {
        let mut runtime = spawn_n_panes(2, 80, 24);
        assert!(runtime.toggle_zoom().unwrap());
        let zoomed = runtime.zoomed_pane();
        let prior = runtime.active_layout();
        assert_eq!(
            runtime.apply_preset(LayoutPreset::Single).unwrap(),
            PresetOutcome::Unchanged("preset single needs one pane")
        );
        assert_eq!(runtime.zoomed_pane(), zoomed);
        assert_eq!(runtime.active_layout(), prior);
    }

    #[test]
    fn layout_preset_unzooms_before_retile() {
        let mut runtime = spawn_n_panes(2, 80, 24);
        assert!(runtime.toggle_zoom().unwrap());
        assert_eq!(
            runtime.apply_preset(LayoutPreset::SplitH).unwrap(),
            PresetOutcome::Applied
        );
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(layout_shape(&runtime.active_layout()), "[H L L]");
    }

    #[test]
    fn layout_preset_minima_fail_leaves_layout_unchanged() {
        let mut runtime = spawn_n_panes(1, 80, 24);
        assert!(runtime.ensure_even_quadrants("/bin/sh", &[]).unwrap());
        assert_eq!(runtime.active_pane_count(), 4);
        runtime.resize(6, 24).unwrap();
        let prior = runtime.active_layout();
        let err = runtime
            .apply_preset(LayoutPreset::SplitH)
            .expect_err("4 even columns need 8 cols");
        assert!(
            err.to_string()
                .contains("layout cannot satisfy minimum pane geometry"),
            "{err}"
        );
        assert_eq!(runtime.active_layout(), prior);
        assert_eq!(runtime.active_pane_count(), 4);
    }

    #[test]
    fn ensure_even_quadrants_four_is_two_by_two() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        assert!(runtime.ensure_even_quadrants("/bin/sh", &[]).unwrap());
        assert_eq!(runtime.active_pane_count(), 4);
        let rects: Vec<_> = runtime.rects().collect();
        assert_eq!(rects.len(), 4);
        let rows: Vec<usize> = rects.iter().map(|(_, r)| r.row).collect();
        let distinct_rows: std::collections::BTreeSet<_> = rows.iter().copied().collect();
        assert_eq!(distinct_rows.len(), 2);
        let cols_sum_top: usize = rects
            .iter()
            .filter(|(_, r)| r.row == *distinct_rows.iter().next().unwrap())
            .map(|(_, r)| r.cols)
            .sum();
        assert_eq!(cols_sum_top, 80);
        let heights: std::collections::BTreeSet<_> = rects.iter().map(|(_, r)| r.rows).collect();
        assert!(heights.len() <= 2);
        let hmin = *heights.iter().min().unwrap();
        let hmax = *heights.iter().max().unwrap();
        assert!(hmax - hmin <= 1, "uneven heights {heights:?}");
    }

    fn full_rect(runtime: &MuxRuntime) -> CellRect {
        CellRect {
            col: 0,
            row: 0,
            cols: runtime.cols(),
            rows: runtime.rows(),
        }
    }

    #[test]
    fn zoom_projects_the_focused_pane_over_the_whole_tab_and_restores() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let left = runtime.focused_id();
        let right = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let split_rects: Vec<_> = runtime.rects().collect();
        let left_pty = (runtime.panes[&left].cols, runtime.panes[&left].rows);

        assert!(runtime.toggle_zoom().unwrap());
        assert_eq!(runtime.zoomed_pane(), Some(right));
        let zoomed: Vec<_> = runtime.rects().collect();
        assert_eq!(zoomed, vec![(right, full_rect(&runtime))]);
        let (cols, rows) = runtime.geom().content_cells(full_rect(&runtime));
        assert_eq!(
            (runtime.panes[&right].cols, runtime.panes[&right].rows),
            (cols, rows)
        );
        assert_eq!(
            (runtime.panes[&left].cols, runtime.panes[&left].rows),
            left_pty,
            "hidden sibling keeps its PTY size"
        );
        assert_eq!(runtime.active_pane_count(), 2, "topology is untouched");
        assert!(runtime.tab_infos()[0].zoomed);

        assert!(runtime.toggle_zoom().unwrap());
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(runtime.rects().collect::<Vec<_>>(), split_rects);
        assert!(!runtime.tab_infos()[0].zoomed);
    }

    #[test]
    fn zoom_is_a_noop_on_a_single_pane() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        assert!(!runtime.toggle_zoom().unwrap());
        assert_eq!(runtime.zoomed_pane(), None);
    }

    #[test]
    fn seed_tab_and_focus_selects_pane_and_clears_zoom() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let third = runtime.focused_id();
        runtime.select_tab(0).unwrap();
        runtime.focus(second);
        assert!(runtime.toggle_zoom().unwrap());
        assert!(runtime.zoomed_pane().is_some());

        runtime.seed_tab_and_focus(1, Some(third)).unwrap();
        assert_eq!(
            runtime.tab_infos().iter().position(|tab| tab.selected),
            Some(1)
        );
        assert_eq!(runtime.focused_id(), third);
        assert_eq!(runtime.zoomed_pane(), None);

        runtime.seed_tab_and_focus(0, Some(second)).unwrap();
        assert_eq!(
            runtime.tab_infos().iter().position(|tab| tab.selected),
            Some(0)
        );
        assert_eq!(runtime.focused_id(), second);
        assert_eq!(runtime.zoomed_pane(), None);

        runtime.seed_tab_and_focus(9, Some(first)).unwrap();
        assert_eq!(
            runtime.tab_infos().iter().position(|tab| tab.selected),
            Some(0)
        );
        assert_eq!(runtime.focused_id(), first);
    }

    #[test]
    fn resize_while_zoomed_keeps_the_single_projection() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        runtime.resize(100, 30).unwrap();
        let zoomed: Vec<_> = runtime.rects().collect();
        assert_eq!(zoomed, vec![(runtime.focused_id(), full_rect(&runtime))]);
    }

    #[test]
    fn split_and_retile_leave_zoom_first() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        runtime
            .split_focused("/bin/sh", &[], Axis::Vertical, 0.5)
            .unwrap();
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(runtime.rects().count(), 3);

        assert!(runtime.toggle_zoom().unwrap());
        assert!(runtime.ensure_even_columns("/bin/sh", &[], 3).unwrap());
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(runtime.rects().count(), 3);
    }

    #[test]
    fn closing_the_zoomed_pane_clears_zoom() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let left = runtime.focused_id();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        assert!(runtime.close_focused().unwrap());
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(
            runtime.rects().collect::<Vec<_>>(),
            vec![(left, full_rect(&runtime))]
        );
    }

    #[test]
    fn focus_moves_leave_zoom() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let left = runtime.focused_id();
        let right = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        assert!(runtime.focus(right), "focusing the zoomed pane keeps zoom");
        assert_eq!(runtime.zoomed_pane(), Some(right));

        assert!(runtime.focus_neighbor(FocusDirection::Left));
        assert_eq!(runtime.focused_id(), left);
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(runtime.rects().count(), 2);

        assert!(runtime.toggle_zoom().unwrap());
        assert!(runtime.focus(right));
        assert_eq!(runtime.zoomed_pane(), None);
        assert_eq!(runtime.rects().count(), 2);
    }

    #[test]
    fn zoom_follows_its_tab_across_tab_switches_and_dies_with_it() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let right = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.toggle_zoom().unwrap());

        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_eq!(runtime.rects().count(), 1);
        assert_ne!(runtime.rects().next().unwrap().0, right);
        let tabs = runtime.tab_infos();
        assert!(
            tabs[0].zoomed && !tabs[1].zoomed,
            "marker stays on the zoomed tab"
        );
        assert!(
            !runtime.toggle_zoom().unwrap(),
            "single-pane tab: nothing to zoom"
        );
        assert_eq!(
            runtime.zoomed_pane(),
            Some(right),
            "the other tab's zoom survives"
        );

        assert!(runtime.select_tab(0).unwrap());
        assert_eq!(
            runtime.rects().collect::<Vec<_>>(),
            vec![(right, full_rect(&runtime))]
        );

        assert!(runtime.close_tab_at(0).unwrap());
        assert_eq!(runtime.zoomed_pane(), None);
    }

    #[test]
    fn spatial_focus_navigation_and_close_follow_visible_layout() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let left = runtime.focused_id();
        let upper_right = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let lower_right = runtime
            .split_focused("/bin/sh", &[], Axis::Vertical, 0.5)
            .unwrap();
        assert_eq!(runtime.focused_id(), lower_right);

        assert!(runtime.focus_neighbor(FocusDirection::Up));
        assert_eq!(runtime.focused_id(), upper_right);
        assert!(runtime.focus_neighbor(FocusDirection::Left));
        assert_eq!(runtime.focused_id(), left);
        assert!(runtime.focus_neighbor(FocusDirection::Right));
        assert_eq!(runtime.focused_id(), upper_right);
        assert!(runtime.focus_neighbor(FocusDirection::Down));
        assert_eq!(runtime.focused_id(), lower_right);

        assert!(runtime.close_focused().unwrap());
        assert_eq!(runtime.pane_count(), 2);
        assert_ne!(runtime.focused_id(), lower_right);
        assert!(runtime.focus(runtime.focused_id()));
    }

    #[test]
    fn split_inherits_cwd_via_proc_pid() {
        // Spawn a shell that stays in a known directory (no OSC 7 required).
        let tmp = std::env::temp_dir().join(format!("prism-split-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // The kernel reports a process cwd in canonical form. On macOS the
        // temp dir lives under the `/var -> /private/var` symlink, so compare
        // against the resolved path (a no-op where temp_dir is already real).
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        // Drive the first shell into tmp with an explicit cd.
        let cd = format!("cd {}\n", tmp.display());
        runtime.focused_mut().send_bytes(cd.into_bytes()).unwrap();
        // Wait until /proc reflects the cwd (shell processed cd).
        let first_id = runtime.focused_id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let _ = runtime.drain_all();
            let pane = runtime.panes.get(&first_id).unwrap();
            if pane.cwd_for_split().as_ref().is_some_and(|p| p == &tmp) {
                break;
            }
            if std::time::Instant::now() > deadline {
                let got = pane.cwd_for_split();
                let _ = std::fs::remove_dir_all(&tmp);
                panic!("timed out waiting for shell cwd; got {got:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let new_pane = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        // New child should start with cwd = tmp; verify via /proc while dir still exists.
        let new = runtime.panes.get(&new_pane).unwrap();
        let pid = new.child_pid().expect("pid");
        let link = prismattyc_mux::procinfo::cwd_of(pid).expect("cwd");
        assert_eq!(link, tmp, "split child must inherit focused pane cwd");
        drop(runtime);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn background_output_sets_badge_and_focus_clears_it() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.focus(first);
        runtime.focus(second);
        runtime.panes.get_mut(&second).unwrap().unseen_output = false;
        runtime.panes.get_mut(&second).unwrap().last_output_at =
            Some(Instant::now() - QUIET_GAP - Duration::from_millis(20));
        runtime
            .panes
            .get(&second)
            .unwrap()
            .send_bytes(b"printf 'BACKGROUND_BADGE\n'\n".to_vec())
            .unwrap();
        runtime.focus(first);
        drain_until(&mut runtime, "BACKGROUND_BADGE");

        assert!(runtime.panes[&second].unseen_output);
        assert_eq!(runtime.unseen_count(), 1);
        assert!(runtime.focus(second));
        assert!(!runtime.panes[&second].unseen_output);
        assert_eq!(runtime.unseen_count(), 0);
    }

    #[test]
    fn output_marks_pane_active_and_activity_decays() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime
            .focused()
            .send_bytes(b"printf 'ACTIVE_MARK\\n'\n".to_vec())
            .unwrap();
        drain_until(&mut runtime, "ACTIVE_MARK");

        // Focused panes count as active too — unlike the unseen badge.
        let pane = runtime.panes.get(&first).unwrap();
        let stamped = pane.last_output_at.expect("stamped on content change");
        assert!(pane.is_active_at(stamped));
        assert!(pane.is_active_at(stamped + ACTIVE_WINDOW));
        assert!(!pane.is_active_at(stamped + ACTIVE_WINDOW + Duration::from_millis(1)));
        assert!(!pane.unseen_output, "focused output must not set unseen");
        assert_eq!(runtime.active_count(), 1);
    }

    #[test]
    fn mail_attention_is_independent_of_unseen_and_survives_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let focused = runtime.focused_id();
        assert_eq!(runtime.mail_depth_total(), 0);
        assert!(runtime.apply_mail_attention(focused.get(), 2));
        assert_eq!(runtime.panes[&focused].mail_depth, 2);
        assert!(!runtime.panes[&focused].unseen_output);
        assert_eq!(runtime.mail_depth_total(), 2);
        runtime.panes.get_mut(&focused).unwrap().unseen_output = true;
        assert_eq!(runtime.panes[&focused].mail_depth, 2);
        assert!(runtime.apply_mail_attention(focused.get(), 0));
        assert_eq!(runtime.panes[&focused].mail_depth, 0);
        assert!(runtime.panes[&focused].unseen_output);
        assert!(!runtime.apply_mail_attention(focused.get(), 0));
    }

    #[test]
    fn take_mail_rises_skips_baseline_then_reports_increase() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let focused = runtime.focused_id();
        assert!(runtime.apply_mail_attention(focused.get(), 1));
        let mut last = std::collections::BTreeMap::new();
        assert!(runtime.take_mail_rises(&mut last).is_empty());
        assert!(runtime.apply_mail_attention(focused.get(), 3));
        let rises = runtime.take_mail_rises(&mut last);
        assert_eq!(rises.len(), 1);
        assert_eq!(rises[0].1, 3);
        assert!(runtime.take_mail_rises(&mut last).is_empty());
    }

    #[test]
    fn reopening_log_pane_restores_output_without_replaying_alerts() {
        let server = private_mux_server();
        let created = Command::new(mux_binary("pmux"))
            .arg("--socket")
            .arg(&server.socket)
            .args([
                "new", "--no-attach", "alerts", "--", "/bin/sh", "-c",
                "printf '\\007\\033]9;old alert\\007OLD_OUTPUT\\n'; while IFS= read -r label; do printf '\\007\\033]9;%s\\007%s\\n' \"$label\" \"$label\"; done",
            ])
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");

        // Wait for the server to retain the initial output before attaching.
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let output = Command::new(mux_binary("pmux"))
                .arg("--socket")
                .arg(&server.socket)
                .args(["attach", "alerts", "--json"])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&output.stdout).contains("OLD_OUTPUT") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "initial output missing: {output:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // Each new replica follows the same attach path used on Space return.
        // The second attach also restores the previous iteration's live alert.
        for iteration in 0..2 {
            let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
            let pane = runtime.focused_id();
            assert!(runtime
                .promote_to_log_replica(pane, "alerts", "alerts", &server.socket)
                .unwrap());
            let previous = if iteration == 0 {
                "OLD_OUTPUT"
            } else {
                "LIVE_0"
            };
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                runtime.drain_all();
                assert!(runtime.take_pending_bells().is_empty(), "replayed BEL");
                assert!(
                    runtime.take_pending_attentions().is_empty(),
                    "replayed attention"
                );
                let screen = runtime.focused_mut().emulator.screen();
                let text: String = (0..screen.rows())
                    .flat_map(|row| screen.row(row).unwrap().iter().map(|cell| cell.character))
                    .collect();
                if text.contains(previous) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "replay did not restore {previous}"
                );
                thread::sleep(Duration::from_millis(10));
            }

            let label = format!("LIVE_{iteration}");
            runtime
                .focused_mut()
                .send_bytes(format!("{label}\n").into_bytes())
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(6);
            let mut rang = false;
            let mut attention = false;
            while !(rang && attention) {
                runtime.drain_all();
                rang |= runtime.take_pending_bells().contains(&pane);
                attention |= runtime
                    .take_pending_attentions()
                    .iter()
                    .any(|(id, message)| *id == pane && message == &label);
                assert!(Instant::now() < deadline, "new alerts were suppressed");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn bell_surfaces_per_pane_through_drain() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        assert!(runtime.take_pending_bells().is_empty());
        runtime
            .focused_mut()
            .send_bytes(b"printf '\\a'\n".to_vec())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut rang = false;
        while Instant::now() < deadline {
            let _ = runtime.drain_all();
            if runtime.take_pending_bells().contains(&pane) {
                rang = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(rang, "BEL from the child must surface per pane");
        assert!(runtime.take_pending_bells().is_empty(), "take drains");
    }

    #[test]
    fn pane_tab_title_names_the_owning_tab() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        let window = runtime.active_window_id();
        runtime.rename_window(window, "ops").unwrap();
        assert_eq!(runtime.pane_tab_title(pane).as_deref(), Some("ops"));
    }

    #[test]
    fn pinned_title_ignores_osc_until_cleared() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        assert!(accept_osc_title(false, &None, &Some("Spaces UX".into())));
        runtime.set_pane_title(pane, Some("fable-pc".into()));
        let current = runtime.pane_title(pane).map(str::to_string);
        assert_eq!(current.as_deref(), Some("fable-pc"));
        assert!(!accept_osc_title(true, &current, &Some("Spaces UX".into())));
        runtime.set_pane_title(pane, Some("   ".into()));
        assert_eq!(runtime.pane_title(pane), None);
        assert!(accept_osc_title(false, &None, &Some("Spaces UX".into())));
    }

    #[test]
    fn server_snapshot_pin_blocks_osc_after_host_restart() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        runtime.apply_server_pane_title(pane, "build server", true);
        let current = runtime.pane_title(pane).map(str::to_string);
        assert_eq!(current.as_deref(), Some("build server"));
        assert!(
            !accept_osc_title(true, &current, &Some("Spaces UX".into())),
            "a restarted host must adopt the server pin before OSC drain"
        );
        runtime.apply_server_pane_title(pane, "", false);
        assert_eq!(runtime.pane_title(pane), None);
        assert!(accept_osc_title(false, &None, &Some("Spaces UX".into())));
    }

    #[test]
    fn pane_title_from_osc_keeps_labels_and_drops_attach_chrome() {
        assert_eq!(pane_title_from_osc("  build  "), Some("build".into()));
        assert_eq!(
            pane_title_from_osc("build — 2 mail"),
            Some("build".into()),
            "mail suffix is chrome"
        );
        assert_eq!(pane_title_from_osc("pmux: grok-pc"), None);
        assert_eq!(pane_title_from_osc("pmux: grok-pc — 3 mail"), None);
        assert_eq!(pane_title_from_osc("pmux-attach"), None);
        assert_eq!(pane_title_from_osc(""), None);
        assert_eq!(
            pane_title_from_osc("a — b mail"),
            Some("a — b mail".into()),
            "only a numeric mail suffix is stripped"
        );
        assert_eq!(
            pane_title_from_osc("brandan@nexus: ~"),
            Some("brandan@nexus: ~".into()),
            "a local shell title passes through"
        );
    }

    #[test]
    fn host_pane_env_marks_nested_attach() {
        let env = super::host_pane_env();
        assert_eq!(env.get("PRISMATTYC_HOST").map(String::as_str), Some("1"));
    }

    #[test]
    fn tab_infos_carry_handle_titles_for_multi_pane_tabs() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        assert!(
            runtime.tab_infos()[0].handle_titles.is_empty(),
            "single pane: no handles"
        );
        let only = runtime.focused_id();
        runtime.set_pane_title(only, Some("solo".into()));
        assert_eq!(
            runtime.tab_infos()[0].pane_title.as_deref(),
            Some("solo"),
            "single-pane tab carries the pane title"
        );
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let panes = runtime.tab_panes()[0].1.clone();
        runtime.set_pane_title(panes[1], Some("build".into()));
        runtime.set_pane_title(panes[0], Some("   ".into()));
        let info = &runtime.tab_infos()[0];
        assert_eq!(info.handles, 2);
        assert_eq!(
            info.handle_titles,
            vec!["pane 1".to_string(), "build".to_string()]
        );
        assert_eq!(
            info.handle_active.len(),
            info.handles,
            "handle_active matches handle count"
        );
        runtime.panes.get_mut(&panes[0]).unwrap().last_output_at = None;
        runtime.panes.get_mut(&panes[1]).unwrap().last_output_at = Some(Instant::now());
        assert_eq!(
            runtime.tab_infos()[0].handle_active,
            vec![false, true],
            "handle_active follows pane is_active()"
        );
        let focused = runtime.focused_id();
        let focused_title = runtime.pane_title(focused);
        assert_eq!(
            info.pane_title.as_deref(),
            focused_title,
            "selected multi-pane tab carries the focused pane OSC title"
        );
        assert_eq!(runtime.pane_title(panes[1]), Some("build"));
        assert_eq!(runtime.pane_title(panes[0]), None, "blank title clears");
    }

    #[test]
    fn tab_infos_aggregate_badges_per_window() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.panes.get_mut(&first).unwrap().unseen_output = true;
        let infos = runtime.tab_infos();
        assert_eq!(infos.len(), 2);
        assert!(infos[0].unseen);
        assert!(!infos[0].selected);
        assert!(infos[1].selected);
        assert!(!infos[1].unseen);
    }

    #[test]
    fn tab_switch_preserves_per_tab_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first_tab = runtime.active_window_id();
        let first_left = runtime.focused_id();
        let first_right = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.focus(first_right);
        let second_tab = runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_ne!(second_tab, first_tab);
        let second_focus = runtime.focused_id();
        assert_eq!(runtime.tab_count(), 2);
        assert_eq!(runtime.pane_count(), 1);

        runtime.select_tab(0).unwrap();
        assert_eq!(runtime.active_window_id(), first_tab);
        assert_eq!(runtime.focused_id(), first_right);
        runtime.select_tab(1).unwrap();
        assert_eq!(runtime.focused_id(), second_focus);
        let _ = first_left;
    }

    #[test]
    fn move_pane_to_tab_preserves_identity_and_pty() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.select_tab(0).unwrap();
        runtime.focus(second);
        assert!(runtime.pane_alive(second));
        runtime.move_focused_to_tab(1).unwrap();
        assert_eq!(runtime.focused_id(), second);
        assert!(runtime.pane_alive(second));
        assert_ne!(runtime.active_window_id(), runtime.window_ids_for_test()[0]);
        assert!(runtime
            .domain
            .window(runtime.active_window_id())
            .unwrap()
            .layout
            .contains_pane(second));
        assert!(!runtime
            .domain
            .window(runtime.window_ids_for_test()[0])
            .unwrap()
            .layout
            .contains_pane(second));
        let _ = first;
    }

    #[test]
    fn reorder_tab_swaps_strip_order_and_unzooms() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.active_window_id();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let second = runtime.active_window_id();
        runtime.select_tab(0).unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        assert!(runtime.reorder_tab(0, 1).unwrap());
        assert_eq!(runtime.window_ids_for_test(), vec![second, first]);
        assert_eq!(runtime.zoomed_pane(), None);
    }

    #[test]
    fn move_pane_to_new_tab_extracts_and_closes_empty_source() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_eq!(runtime.tab_count(), 1);
        assert!(runtime.move_pane_to_new_tab(second).unwrap());
        assert_eq!(runtime.tab_count(), 2);
        assert_eq!(runtime.focused_id(), second);
        assert!(runtime
            .domain
            .window(runtime.active_window_id())
            .unwrap()
            .layout
            .contains_pane(second));
        assert!(runtime
            .domain
            .window(runtime.window_ids_for_test()[0])
            .unwrap()
            .layout
            .contains_pane(first));
    }

    #[test]
    fn join_focused_pane_uses_last_window() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.move_pane_to_new_tab(second).unwrap());
        assert_eq!(runtime.tab_count(), 2);
        assert!(runtime.join_focused_pane().unwrap());
        assert_eq!(runtime.tab_count(), 1);
        assert_eq!(runtime.focused_id(), second);
        let window = runtime.domain.window(runtime.active_window_id()).unwrap();
        assert!(window.layout.contains_pane(first));
        assert!(window.layout.contains_pane(second));
    }

    #[test]
    fn break_pane_on_a_single_pane_tab_is_a_noop() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let only = runtime.focused_id();
        let window = runtime.active_window_id();
        assert!(!runtime.move_pane_to_new_tab(only).unwrap());
        assert_eq!(runtime.tab_count(), 1);
        assert_eq!(runtime.active_window_id(), window);
        assert_eq!(runtime.focused_id(), only);
        assert!(!runtime.join_focused_pane().unwrap());
        assert_eq!(runtime.tab_count(), 1);
    }

    #[test]
    fn breaking_an_unfocused_pane_does_not_steal_source_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let _second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let third = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_eq!(runtime.focused_id(), third);
        assert!(runtime.move_pane_to_new_tab(first).unwrap());
        let src = runtime.window_ids_for_test()[0];
        assert_eq!(runtime.view.focused_pane(src), Some(third));
        assert_eq!(runtime.focused_id(), first);
    }

    #[test]
    fn breaking_the_focused_pane_retargets_source_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_eq!(runtime.focused_id(), second);
        assert!(runtime.move_pane_to_new_tab(second).unwrap());
        let src = runtime.window_ids_for_test()[0];
        assert_eq!(runtime.view.focused_pane(src), Some(first));
        assert_eq!(runtime.focused_id(), second);
    }

    #[test]
    fn join_focused_pane_drops_a_dead_last_window() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_eq!(runtime.tab_count(), 2);
        let dead = runtime.window_ids_for_test()[0];
        assert!(runtime.close_tab_at(0).unwrap());
        runtime.last_window = Some(dead);
        assert!(!runtime.join_focused_pane().unwrap());
        assert!(runtime.last_window.is_none());
        assert_eq!(runtime.tab_count(), 1);
    }

    #[test]
    fn tab_strip_hit_chip_pane_handle_and_empty_end() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.select_tab(0).unwrap();
        runtime
            .set_geom(HostGeom {
                cell_w: 10,
                cell_h: 16,
                window_pad: 0,
                slack_x: 0,
                slack_y: 0,
                pane_gap: 0,
                rail_gap: 0,
                inner_pad: 0,
                top_chrome_px: 32,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();
        let stride = 200;
        assert_eq!(
            runtime.tab_strip_hit(5, 20, stride, false),
            Some(StripHit::Pane {
                tab: 0,
                pane: first,
                handle: 0,
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(15, 20, stride, false),
            Some(StripHit::Pane {
                tab: 0,
                pane: second,
                handle: 1,
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(5, 4, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: false
            }),
            "title row is the tab, not a handle"
        );
        let (x0, width) = tab_slot_bounds(0, 2, stride, 0, 0).unwrap();
        let close = tab_close_left(x0, width, 10).unwrap();
        assert_eq!(
            runtime.tab_strip_hit(close, 4, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: true
            }),
            "title-row close glyph is hittable"
        );
        assert_eq!(
            runtime.tab_strip_hit(x0 + 30, 20, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: false
            }),
            "empty handle-row space selects the owning tab"
        );
        let mid = x0 + width / 2;
        assert_eq!(
            runtime.tab_strip_hit(mid, 4, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: false
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(stride - 1, 4, stride, true),
            Some(StripHit::EmptyEnd)
        );
        assert_eq!(
            runtime.tab_strip_hit(stride - 1, 20, stride, true),
            Some(StripHit::EmptyEnd)
        );
        assert_eq!(runtime.tab_strip_hit(mid, 40, stride, false), None);
        // Moving the strip below a top rail preserves title, close, and handle hits.
        let mut geom = runtime.geom();
        geom.rail_side = RailSide::Top;
        geom.rail_px = 48;
        runtime.set_geom(geom).unwrap();
        assert_eq!(runtime.tab_strip_hit(mid, 4, stride, false), None);
        assert_eq!(runtime.tab_strip_hit(mid, 47, stride, false), None);
        assert_eq!(
            runtime.tab_strip_hit(close, 52, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: true
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(15, 68, stride, false),
            Some(StripHit::Pane {
                tab: 0,
                pane: second,
                handle: 1
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(stride - 1, 52, stride, true),
            Some(StripHit::EmptyEnd)
        );
        assert_eq!(runtime.tab_strip_hit(mid, 80, stride, false), None);
    }

    #[test]
    fn tab_strip_content_and_hits_follow_pane_padding() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.select_tab(0).unwrap();
        runtime
            .set_geom(HostGeom {
                cell_w: 10,
                cell_h: 16,
                window_pad: 0,
                slack_x: 0,
                slack_y: 0,
                pane_gap: 0,
                rail_gap: 0,
                inner_pad: 16,
                top_chrome_px: 32,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();

        let stride = 200;
        let (x0, width) = tab_slot_bounds(0, 2, stride, 0, 0).unwrap();
        assert_eq!(
            tab_close_left_with_inset(x0, width, 10, 16),
            Some(x0 + width - 10 - 16)
        );
        assert_eq!(
            runtime.tab_strip_hit(x0 + 15, 20, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: false
            }),
            "left inset remains part of the tab, not the first pane handle"
        );
        assert_eq!(
            runtime.tab_strip_hit(x0 + 16, 20, stride, false),
            Some(StripHit::Pane {
                tab: 0,
                pane: first,
                handle: 0,
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(x0 + 31, 20, stride, false),
            Some(StripHit::Pane {
                tab: 0,
                pane: second,
                handle: 1,
            })
        );
        let close = tab_close_left_with_inset(x0, width, 10, 16).unwrap();
        assert_eq!(
            runtime.tab_strip_hit(close - 1, 4, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: false
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(close, 4, stride, false),
            Some(StripHit::Tab {
                index: 0,
                close: true
            })
        );
    }

    #[test]
    fn one_tab_split_shows_strip_and_moves_pane_to_new_tab() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_eq!(runtime.tab_count(), 1);
        assert!(runtime.tab_strip_needed());
        runtime
            .set_geom(HostGeom {
                cell_w: 10,
                cell_h: 16,
                window_pad: 0,
                slack_x: 0,
                slack_y: 0,
                pane_gap: 0,
                rail_gap: 0,
                inner_pad: 0,
                top_chrome_px: 16,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();
        assert_eq!(
            runtime.tab_strip_hit(5, 4, 200, true),
            Some(StripHit::Pane {
                tab: 0,
                pane: first,
                handle: 0,
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(199, 4, 200, true),
            Some(StripHit::EmptyEnd)
        );
        assert!(runtime.move_pane_to_new_tab(second).unwrap());
        assert_eq!(runtime.tab_count(), 2);
        assert!(runtime
            .domain
            .window(runtime.window_ids_for_test()[0])
            .unwrap()
            .layout
            .contains_pane(first));
    }

    #[test]
    fn move_last_pane_to_other_tab_closes_source() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let second = runtime.focused_id();
        runtime.select_tab(0).unwrap();
        runtime.focus(first);
        assert!(runtime.move_focused_to_tab(1).unwrap());
        assert_eq!(runtime.tab_count(), 1);
        assert!(runtime
            .domain
            .window(runtime.active_window_id())
            .unwrap()
            .layout
            .contains_pane(first));
        assert!(runtime
            .domain
            .window(runtime.active_window_id())
            .unwrap()
            .layout
            .contains_pane(second));
    }

    #[test]
    fn move_pane_clears_zoom_on_inactive_tab() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let other = runtime.focused_id();
        runtime.select_tab(0).unwrap();
        assert!(runtime.zoomed_pane().is_some());
        runtime.select_tab(1).unwrap();
        runtime.focus(other);
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.select_tab(1).unwrap();
        assert!(runtime.move_focused_to_tab(2).unwrap());
        assert_eq!(runtime.zoomed_pane(), None);
    }

    #[test]
    fn move_tab_at_boundary_is_noop_and_keeps_zoom() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.select_tab(0).unwrap();
        assert!(runtime.toggle_zoom().unwrap());
        assert!(!runtime.move_active_tab(-1).unwrap());
        assert!(runtime.zoomed_pane().is_some());
        runtime.select_tab(1).unwrap();
        assert!(!runtime.move_active_tab(1).unwrap());
    }

    #[test]
    fn placeholder_pane_is_a_strip_handle() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.mark_attach_session(second, "2".into(), "seat".into());
        runtime.panes.get_mut(&second).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        assert!(runtime.is_placeholder(second));
        runtime
            .set_geom(HostGeom {
                cell_w: 10,
                cell_h: 16,
                window_pad: 0,
                slack_x: 0,
                slack_y: 0,
                pane_gap: 0,
                rail_gap: 0,
                inner_pad: 0,
                top_chrome_px: 16,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();
        assert_eq!(
            runtime.tab_strip_hit(15, 4, 200, true),
            Some(StripHit::Pane {
                tab: 0,
                pane: second,
                handle: 1,
            })
        );
        let _ = first;
    }

    #[test]
    fn unseen_badge_accumulates_on_inactive_tab() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first_pane = runtime.focused_id();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.panes.get_mut(&first_pane).unwrap().last_output_at =
            Some(Instant::now() - QUIET_GAP - Duration::from_millis(20));
        runtime
            .panes
            .get(&first_pane)
            .unwrap()
            .send_bytes(b"printf 'INACTIVE_TAB\\n'\n".to_vec())
            .unwrap();
        drain_until(&mut runtime, "INACTIVE_TAB");
        assert!(runtime.pane_unseen(first_pane));
        runtime.select_tab(0).unwrap();
        assert!(!runtime.pane_unseen(first_pane));
    }

    #[test]
    fn flag_off_emulator_does_not_collect_apc() {
        let runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        assert!(!runtime.focused().emulator.collects_apc());
        assert!(!runtime.focused().experimental_rich());
    }

    #[test]
    fn rich_focus_toggle_is_noop_when_flag_off() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        assert!(!runtime.focused().experimental_rich());
        assert!(!runtime.toggle_rich_focus());
        assert!(!runtime.rich_focus_active());
    }

    #[test]
    fn closing_a_rich_pane_drops_its_regions() {
        let mut runtime = MuxRuntime::spawn_experimental("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        {
            let pane = runtime.focused_mut();
            pane.rich.apply_grant(CapabilityGrant {
                features: prismattyc_protocol::Feature::V1.into_iter().collect(),
                region_limit: 8,
            });
            let attach = prismattyc_protocol::encode_attach_cell_rect(
                &prismattyc_protocol::AttachCellRect {
                    id: 1,
                    row: 0,
                    col: 0,
                    rows: 1,
                    cols: 4,
                    text: "STAT".into(),
                },
            )
            .unwrap();
            rich::process_rich_chunk(
                &mut pane.emulator,
                &mut pane.rich,
                &pane.to_child_tx,
                &attach,
            );
            assert_eq!(pane.rich.attachments_len_for_test(), 1);
        }
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.focus(first);
        assert!(runtime.close_focused().unwrap());
        assert!(!runtime.panes.contains_key(&first));
        assert!(runtime.panes.contains_key(&second));
        assert_eq!(
            runtime.panes[&second].rich.attachments_len_for_test(),
            0,
            "surviving pane must not inherit the closed pane's regions"
        );
    }

    #[test]
    fn flag_on_emulator_collects_apc() {
        let mut runtime = MuxRuntime::spawn_experimental("/bin/sh", &[], 80, 24).unwrap();
        assert!(runtime.focused().emulator.collects_apc());
        assert!(runtime.focused().experimental_rich());
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert!(runtime.panes[&second].emulator.collects_apc());
    }

    #[test]
    fn tab_slot_bounds_align_with_window_pad() {
        let stride = 200;
        for window_pad in [0usize, 5] {
            let pad = effective_tab_end_pad(window_pad, stride);
            assert_eq!(pad, window_pad, "pad={window_pad}");
            let (origin, _) = tab_content_origin_and_slot(stride, 2, window_pad, 0).unwrap();
            assert_eq!(origin, window_pad);
            let (x0, w0) = tab_slot_bounds(0, 2, stride, window_pad, 0).unwrap();
            let (x1, w1) = tab_slot_bounds(1, 2, stride, window_pad, 0).unwrap();
            assert_eq!(x0, window_pad);
            assert_eq!(x1 + w1, stride - window_pad);
            assert!(x0 + w0 <= x1);
            let close1 = tab_close_left(x1, w1, 8).unwrap();
            assert!(
                close1 + 8 + TAB_CLOSE_INSET <= stride - window_pad,
                "last close must sit inside end pad={window_pad}"
            );
            assert!(tab_close_left(0, 8, 8).is_none());
            assert_eq!(tab_close_left(10, 8 + TAB_CLOSE_INSET, 8), Some(10));
        }
        let (origin, _) = tab_content_origin_and_slot(stride, 2, 40, 0).unwrap();
        assert_eq!(origin, 40);
        let (x1, w1) = tab_slot_bounds(1, 2, stride, 40, 0).unwrap();
        assert_eq!(x1 + w1, stride - 40);
    }

    #[test]
    fn tab_gaps_are_chrome_wide_and_dead_to_hits() {
        let stride = 200;
        let pad = 5;
        let gap = 6;
        let (x0, w0) = tab_slot_bounds(0, 2, stride, pad, gap).unwrap();
        let (x1, w1) = tab_slot_bounds(1, 2, stride, pad, gap).unwrap();
        assert_eq!(x0, pad);
        assert_eq!(x1 + w1, stride - pad);
        assert_eq!(x1, x0 + w0 + gap);
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime
            .set_geom(HostGeom {
                cell_w: 8,
                cell_h: 16,
                window_pad: pad,
                slack_x: 0,
                slack_y: 0,
                pane_gap: gap,
                rail_gap: gap,
                inner_pad: 0,
                top_chrome_px: 16,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();
        assert_eq!(runtime.tab_index_at_px(x0, 4, stride), Some(0));
        assert_eq!(runtime.tab_index_at_px(x1, 4, stride), Some(1));
        for px in x0 + w0..x1 {
            assert_eq!(
                runtime.tab_index_at_px(px, 4, stride),
                None,
                "gap pixel {px} must be dead"
            );
        }
    }

    #[test]
    fn rail_slots_ignore_active_tab_pane_count() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert!(runtime.select_tab(0).unwrap());
        let configured = 5usize;
        let stride = 200;
        let pad = 5;
        let split = HostGeom {
            cell_w: 8,
            cell_h: 16,
            window_pad: pad,
            slack_x: 0,
            slack_y: 0,
            pane_gap: 5,
            rail_gap: configured,
            inner_pad: 0,
            top_chrome_px: 16,
            scrollbar_gutter_px: 0,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        };
        runtime.set_geom(split).unwrap();
        assert_eq!(runtime.active_pane_count(), 2);
        let before: Vec<_> = (0..2)
            .map(|i| tab_slot_bounds(i, 2, stride, pad, runtime.geom().rail_gap).unwrap())
            .collect();
        assert_eq!(
            before,
            [
                tab_slot_bounds(0, 2, stride, pad, configured).unwrap(),
                tab_slot_bounds(1, 2, stride, pad, configured).unwrap()
            ]
        );

        assert!(runtime.select_tab(1).unwrap());
        let single = HostGeom {
            pane_gap: 0,
            rail_gap: configured,
            ..split
        };
        runtime.set_geom(single).unwrap();
        assert_eq!(runtime.active_pane_count(), 1);
        assert_eq!(runtime.geom().pane_gap, 0);
        assert_eq!(runtime.geom().rail_gap, configured);
        let after: Vec<_> = (0..2)
            .map(|i| tab_slot_bounds(i, 2, stride, pad, runtime.geom().rail_gap).unwrap())
            .collect();
        assert_eq!(
            after, before,
            "rail slots must not move when pane_gap drops"
        );
        let collapsed: Vec<_> = (0..2)
            .map(|i| tab_slot_bounds(i, 2, stride, pad, runtime.geom().pane_gap).unwrap())
            .collect();
        assert_ne!(
            collapsed, before,
            "using pane_gap would still collapse the rail"
        );
        assert_eq!(runtime.tab_index_at_px(before[0].0, 4, stride), Some(0));
        assert_eq!(runtime.tab_index_at_px(before[1].0, 4, stride), Some(1));
    }

    #[test]
    fn tab_badge_center_matches_close_centerline() {
        for bar_h in [12usize, 16, 17, 24] {
            let badge_center = tab_badge_top(bar_h) + TAB_BADGE_SIZE / 2;
            assert_eq!(badge_center, tab_chrome_center_y(bar_h), "bar_h={bar_h}");
        }
    }

    #[test]
    fn tab_index_at_px_maps_strip_and_ignores_content() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let geom = HostGeom {
            cell_w: 8,
            cell_h: 16,
            window_pad: 0,
            slack_x: 0,
            slack_y: 0,
            pane_gap: 0,
            rail_gap: 0,
            inner_pad: 0,
            top_chrome_px: 16,
            scrollbar_gutter_px: 0,
            rail_side: RailSide::Off,
            rail_px: 0,
            rail_chip_cols: 0,
        };
        runtime.set_geom(geom).unwrap();
        let stride = 160;
        assert_eq!(runtime.tab_index_at_px(0, 4, stride), Some(0));
        assert_eq!(runtime.tab_index_at_px(90, 4, stride), Some(1));
        assert_eq!(runtime.tab_index_at_px(stride - 1, 4, stride), Some(1));
        assert_eq!(runtime.tab_index_at_px(10, 20, stride), None);

        let padded = HostGeom {
            window_pad: 40,
            slack_x: 0,
            slack_y: 0,
            ..geom
        };
        runtime.set_geom(padded).unwrap();
        assert_eq!(runtime.tab_index_at_px(10, 4, stride), None);
        assert_eq!(runtime.tab_index_at_px(45, 4, stride), Some(0));
        assert_eq!(runtime.tab_index_at_px(100, 4, stride), Some(1));
        assert_eq!(runtime.tab_index_at_px(155, 4, stride), None);
        assert_eq!(runtime.tab_hit_at_px(45, 4, stride), Some((0, false)));
        assert_eq!(runtime.tab_hit_at_px(79, 4, stride), Some((0, true)));
        let (x1, w1) = tab_slot_bounds(1, 2, stride, 40, 0).unwrap();
        let close1 = tab_close_left(x1, w1, 8).unwrap();
        assert_eq!(runtime.tab_hit_at_px(close1, 4, stride), Some((1, true)));
        assert!(close1 + 8 <= stride - 40);
        runtime.set_geom(geom).unwrap();
        assert_eq!(runtime.tab_hit_at_px(0, 4, stride), Some((0, false)));
        assert_eq!(runtime.tab_hit_at_px(75, 4, stride), Some((0, true)));
        runtime.select_tab(1).unwrap();
        assert!(runtime.tab_infos()[1].selected);
        runtime
            .rename_window(runtime.window_at_tab(1).unwrap(), "notes")
            .unwrap();
        assert_eq!(runtime.tab_infos()[1].title, "notes");
    }

    #[test]
    fn tab_strip_hit_selects_empty_handle_row_and_distinguishes_pane_handle() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime
            .set_geom(HostGeom {
                cell_w: 8,
                cell_h: 16,
                window_pad: 0,
                slack_x: 0,
                slack_y: 0,
                pane_gap: 0,
                rail_gap: 0,
                inner_pad: 0,
                top_chrome_px: 32,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();

        assert_eq!(
            runtime.tab_strip_hit(24, 20, 160, false),
            Some(StripHit::Tab {
                index: 0,
                close: false
            })
        );
        assert_eq!(
            runtime.tab_strip_hit(12, 20, 160, false),
            Some(StripHit::Pane {
                tab: 0,
                pane,
                handle: 1,
            })
        );
    }

    #[test]
    fn detach_view_closes_extra_tab_then_asks_host_exit() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_eq!(runtime.tab_count(), 2);
        assert_eq!(runtime.detach_view().unwrap(), DetachView::ClosedTab);
        assert_eq!(runtime.tab_count(), 1);
        assert!(!runtime.all_children_exited());
        assert_eq!(runtime.detach_view().unwrap(), DetachView::ExitHost);
        assert_eq!(runtime.tab_count(), 1);
        assert!(!runtime.all_children_exited());
    }

    #[test]
    fn close_tab_at_closes_without_requiring_select() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_eq!(runtime.tab_count(), 3);
        runtime.select_tab(0).unwrap();
        assert!(runtime.close_tab_at(2).unwrap());
        assert_eq!(runtime.tab_count(), 2);
        assert!(runtime.tab_infos()[0].selected);
        assert!(runtime.close_tab_at(0).unwrap());
        assert_eq!(runtime.tab_count(), 1);
        assert!(!runtime.close_tab_at(0).unwrap());
    }

    #[test]
    fn only_pane_exit_closes_tab_and_selects_neighbor() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.select_tab(1).unwrap();
        let mid = runtime.focused_id();
        runtime.panes.get_mut(&mid).unwrap().child_alive = false;
        assert!(runtime.drain_all().0);
        assert_eq!(runtime.tab_count(), 2);
        assert!(runtime.tab_infos()[0].selected);
        assert!(!runtime.all_children_exited());
        assert_eq!(runtime.rects.len(), 1);
        assert_eq!(runtime.rects[0].1.cols, 80);
        assert_eq!(
            runtime.pane_at_cell(3, 4),
            Some((runtime.focused_id(), 4, 3))
        );
    }

    #[test]
    fn last_pane_of_last_tab_exit_leaves_host_teardown_signal() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let only = runtime.focused_id();
        runtime.panes.get_mut(&only).unwrap().child_alive = false;
        let _ = runtime.drain_all();
        assert_eq!(runtime.tab_count(), 1);
        assert_eq!(runtime.panes.len(), 1);
        assert!(runtime.all_children_exited());
        assert!(!runtime.close_tab().unwrap());
    }

    #[test]
    fn unfocused_only_pane_exit_does_not_steal_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first_pane = runtime.focused_id();
        let first_tab = runtime.active_window_id();
        let second_tab = runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.panes.get_mut(&first_pane).unwrap().child_alive = false;
        let _ = runtime.drain_all();
        assert_eq!(runtime.tab_count(), 1);
        assert_eq!(runtime.active_window_id(), second_tab);
        assert_ne!(runtime.active_window_id(), first_tab);
        assert!(!runtime.panes.contains_key(&first_pane));
        assert_eq!(runtime.rects.len(), 1);
        assert_eq!(runtime.rects[0].1.cols, 80);
        assert_eq!(
            runtime.pane_at_cell(3, 4),
            Some((runtime.focused_id(), 4, 3))
        );
        assert!(!runtime.all_children_exited());
    }

    #[test]
    fn unfocused_nonfinal_pane_exit_does_not_steal_focus() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let second_tab = runtime.new_tab("/bin/sh", &[]).unwrap();
        runtime.panes.get_mut(&second).unwrap().child_alive = false;
        let _ = runtime.drain_all();
        assert_eq!(runtime.tab_count(), 2);
        assert_eq!(runtime.active_window_id(), second_tab);
        assert!(runtime.panes.contains_key(&first));
        assert!(!runtime.panes.contains_key(&second));
        assert_eq!(runtime.rects.len(), 1);
        assert_eq!(runtime.rects[0].0, runtime.focused_id());
        assert_ne!(runtime.focused_id(), first);
    }

    #[test]
    fn real_child_exit_wakes_and_closes_only_pane_tab() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let woke = Arc::new(AtomicBool::new(false));
        let flag = woke.clone();
        let wake: Wake = Arc::new(move || {
            flag.store(true, Ordering::Relaxed);
        });
        let mut runtime = MuxRuntime::spawn_with_wake("/bin/sh", &[], 80, 24, wake).unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_eq!(runtime.tab_count(), 2);
        runtime.focused().send_bytes(b"exit\n".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let _ = runtime.drain_all();
            if runtime.tab_count() == 1 && woke.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            woke.load(Ordering::Relaxed),
            "child exit must proxy-wake the event loop"
        );
        assert_eq!(runtime.tab_count(), 1);
        assert!(!runtime.all_children_exited());
    }

    #[test]
    fn dividers_follow_the_split_tree_and_resize_moves_the_boundary() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        runtime
            .set_geom(HostGeom {
                cell_w: 10,
                cell_h: 16,
                window_pad: 0,
                slack_x: 0,
                slack_y: 0,
                pane_gap: 4,
                rail_gap: 4,
                inner_pad: 0,
                top_chrome_px: 0,
                scrollbar_gutter_px: 0,
                rail_side: RailSide::Off,
                rail_px: 0,
                rail_chip_cols: 0,
            })
            .unwrap();
        let dividers = runtime.dividers();
        assert_eq!(dividers.len(), 1);
        let divider = &dividers[0];
        assert_eq!(divider.axis, Axis::Horizontal);
        assert!(divider.path.is_empty(), "root split");
        assert_eq!(divider.bounds.cols, 80);
        assert_eq!(divider.boundary, 40, "second half starts mid-window");
        // The divider band is the gap (plus slop) at the boundary.
        let geom = runtime.geom();
        // Slots carry half the gap on each side: the first ends at 398, the
        // second starts at 402, so the band is 398..402 widened by the slop.
        let (x, _, w, _) = geom.divider_px(divider, 3);
        assert_eq!((x, w), (398 - 3, 4 + 6));
        assert!(runtime.divider_at(40 * 10 - 2, 100, 3).is_some());
        assert!(runtime.divider_at(200, 100, 3).is_none());
        assert_eq!(geom.divider_ratio_at(divider, 200, 0), 0.25);
        // Resize to 25 % and the boundary follows; both panes still exist.
        assert!(runtime.resize_split(&[], 0.25).unwrap());
        let moved = runtime.dividers();
        assert_eq!(moved[0].boundary, 20);
        let rects: HashMap<PaneId, CellRect> = runtime.rects().collect();
        assert_eq!(rects[&first].cols, 20);
        assert_eq!(rects[&second].col, 20);
        // Below the minimum: the geometry clamps the first pane to its
        // minimum width instead of collapsing it.
        assert!(runtime.resize_split(&[], 0.001).unwrap());
        let clamped = runtime.dividers()[0].boundary;
        assert!(
            (DEFAULT_MIN_COLS..20).contains(&clamped),
            "boundary {clamped} must sit at the minimum"
        );
        let rects: HashMap<PaneId, CellRect> = runtime.rects().collect();
        assert!(rects[&first].cols >= DEFAULT_MIN_COLS);
        // A path into a leaf is not a split.
        assert!(!runtime.resize_split(&[true], 0.5).unwrap());
        // Nested split: the child divider carries its path.
        runtime
            .split_focused("/bin/sh", &[], Axis::Vertical, 0.5)
            .unwrap();
        let nested = runtime.dividers();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[1].axis, Axis::Vertical);
        assert_eq!(nested[1].path.len(), 1);
        // Zoomed: no dividers.
        runtime.toggle_zoom().unwrap();
        assert!(runtime.dividers().is_empty());
    }

    #[test]
    fn swap_and_rotate_move_panes_and_keep_focus_on_the_pane() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let a = runtime.focused_id();
        assert!(!runtime.swap_focused(1).unwrap(), "single pane");
        assert!(!runtime.rotate_panes(1).unwrap());
        let b = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        let c = runtime
            .split_focused("/bin/sh", &[], Axis::Vertical, 0.5)
            .unwrap();
        let order = |r: &MuxRuntime| r.domain.window(r.active_window()).unwrap().layout.panes();
        assert_eq!(order(&runtime), vec![a, b, c]);
        assert_eq!(runtime.focused_id(), c);
        // Swap c with its previous neighbour b: order a c b, focus still c.
        assert!(runtime.swap_focused(-1).unwrap());
        assert_eq!(order(&runtime), vec![a, c, b]);
        assert_eq!(runtime.focused_id(), c);
        // Rotate forward: last leaf moves to the first slot.
        assert!(runtime.rotate_panes(1).unwrap());
        assert_eq!(order(&runtime), vec![b, a, c]);
        assert_eq!(runtime.focused_id(), c);
        // Rects follow the new order: the pane in slot 0 owns the left column.
        let rects: HashMap<PaneId, CellRect> = runtime.rects().collect();
        assert_eq!(rects[&b].col, 0);
        assert!(rects[&a].col > 0);
        // Zoomed swap leaves zoom first and still works.
        runtime.toggle_zoom().unwrap();
        assert!(runtime.swap_focused(1).unwrap());
        assert!(runtime.zoomed_pane().is_none());
    }

    #[test]
    fn last_pane_and_last_tab_jump_back_and_forget_closed_targets() {
        let mut runtime = MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        assert!(!runtime.focus_last_pane(), "nothing to go back to");
        let second = runtime
            .split_focused("/bin/sh", &[], Axis::Horizontal, 0.5)
            .unwrap();
        assert_eq!(runtime.focused_id(), second);
        assert!(runtime.focus_last_pane());
        assert_eq!(runtime.focused_id(), first);
        assert!(runtime.focus_last_pane(), "toggles back");
        assert_eq!(runtime.focused_id(), second);
        // Tabs: new tab, then last-tab returns, then again forwards.
        runtime.new_tab("/bin/sh", &[]).unwrap();
        assert_eq!(runtime.selected_tab_index(), 1);
        assert!(runtime.select_last_tab().unwrap());
        assert_eq!(runtime.selected_tab_index(), 0);
        assert!(runtime.select_last_tab().unwrap());
        assert_eq!(runtime.selected_tab_index(), 1);
        // Move the focused pane of tab 1 into tab 0: the view lands on tab 0,
        // last-tab points back at tab 1, and the displaced pane of tab 0 is
        // that tab's last pane.
        let tab1_pane = runtime.focused_id();
        assert!(runtime.move_focused_to_tab(0).unwrap());
        assert_eq!(runtime.selected_tab_index(), 0);
        assert_eq!(runtime.focused_id(), tab1_pane);
        assert!(runtime.focus_last_pane(), "displaced pane is the last pane");
        assert_ne!(runtime.focused_id(), tab1_pane);
        // Tab 1 is now empty and gone, so last-tab has nothing to return to.
        assert_eq!(runtime.tab_count(), 1);
        assert!(!runtime.select_last_tab().unwrap());
        assert_eq!(runtime.selected_tab_index(), 0);
    }
}
