//! Classic VT parsing and PTY integration for Prismattyc.
//!
//! Rich document models stay out of this crate. A dependency-light
//! [`prismattyc_protocol::ApcCollector`] sidecar recovers APC bodies because VTE
//! 0.15 swallows APC without delivering the payload to [`vte::Perform`].
//! The collector is constructed only when experimental rich collection is
//! enabled so the classic path pays about one null check, not a scan.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use prismattyc_core::{
    Color, Screen, ScreenStateV1, Style, UnderlineStyle, MAX_HYPERLINK_URI_BYTES,
};
pub use prismattyc_core::{GridDamage, ScrollDamage};
use prismattyc_protocol::{ApcCollector, CollectedApc};
use serde::{Deserialize, Serialize};
use vte::{Params, Parser, Perform};

mod graphics;
pub use graphics::placeholder::{self as kitty_placeholder, PLACEHOLDER as KITTY_PLACEHOLDER};
pub use graphics::{
    GraphicsStateV1, PlacedImage, StoredImage, VirtualPlacement, VirtualPlacementStateV1,
    NOMINAL_CELL_H_PX, NOMINAL_CELL_W_PX,
};

/// DECSCUSR (`CSI Ps SP q`) caret shape. Ghostty default is a filled block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CursorShape {
    /// Steady/blinking block (`Ps` 0, 1, 2). Default.
    #[default]
    Block,
    /// Underline (`Ps` 3, 4).
    Underline,
    /// Vertical bar (`Ps` 5, 6).
    Bar,
}

impl CursorShape {
    /// Steady DECSCUSR for the outer terminal (nested host / mux-attach).
    pub const fn decscusr_steady_bytes(self) -> &'static [u8] {
        match self {
            Self::Block => b"\x1b[2 q",
            Self::Underline => b"\x1b[4 q",
            Self::Bar => b"\x1b[6 q",
        }
    }
}

/// `TIOCGWINSZ` window pixels (`ws_xpixel`/`ws_ypixel`) from cell metrics.
///
/// Producers (Claude Code Kitty graphics) divide by `cols`/`rows` to get cell
/// size; `0×0` made them emit a postage-stamp PNG. Zero cell metrics clamp to 1.
pub fn pty_size_with_cell_pixels(cols: usize, rows: usize, cell_w: u32, cell_h: u32) -> PtySize {
    let cols = cols.min(u16::MAX as usize);
    let rows = rows.min(u16::MAX as usize);
    let cell_w = cell_w.max(1);
    let cell_h = cell_h.max(1);
    PtySize {
        rows: rows as u16,
        cols: cols as u16,
        pixel_width: (cols as u32).saturating_mul(cell_w).min(u16::MAX as u32) as u16,
        pixel_height: (rows as u32).saturating_mul(cell_h).min(u16::MAX as u32) as u16,
    }
}

/// Application mouse tracking level (DECSET 1000 / 1002 / 1003).
///
/// Highest enabled level wins: [`Any`](Self::Any) > [`Drag`](Self::Drag) >
/// [`Click`](Self::Click) > [`Off`](Self::Off). See mouse input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    /// No 1000/1002/1003 enabled — host owns mouse (selection / scroll).
    #[default]
    Off,
    /// DECSET 1000: press and release only.
    Click,
    /// DECSET 1002: press, release, and button-motion (drag).
    Drag,
    /// DECSET 1003: any motion including bare move.
    Any,
}

impl MouseTracking {
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Report button-drag motion (1002 or 1003).
    pub const fn reports_drag(self) -> bool {
        matches!(self, Self::Drag | Self::Any)
    }

    /// Report bare pointer motion without buttons (1003 only).
    pub const fn reports_motion(self) -> bool {
        matches!(self, Self::Any)
    }
}

/// Stored DECSET mouse flags (mouse input). Not a full xterm mode table.
#[derive(Debug, Clone, Copy, Default)]
struct MouseModeFlags {
    /// DECSET 1000 click tracking.
    m1000: bool,
    /// DECSET 1002 cell-motion tracking.
    m1002: bool,
    /// DECSET 1003 any-motion tracking.
    m1003: bool,
    /// DECSET 1006 SGR encoding preferred.
    sgr: bool,
    /// Prismattyc-private DECSET 7700: report wheel as SGR/X10; buttons stay host-owned.
    wheel_only: bool,
}

impl MouseModeFlags {
    const fn tracking(self) -> MouseTracking {
        if self.m1003 {
            MouseTracking::Any
        } else if self.m1002 {
            MouseTracking::Drag
        } else if self.m1000 {
            MouseTracking::Click
        } else {
            MouseTracking::Off
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Kitty keyboard progressive-enhancement flags (bitmask).
///
/// See <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.
pub const KITTY_DISAMBIGUATE: u16 = 0b1;
pub const KITTY_EVENT_TYPES: u16 = 0b10;
pub const KITTY_ALTERNATE_KEYS: u16 = 0b100;
pub const KITTY_REPORT_ALL: u16 = 0b1000;
pub const KITTY_REPORT_TEXT: u16 = 0b1_0000;

const KEYBOARD_STACK_MAX: usize = 16;

/// Per-screen Kitty keyboard mode (flags + push stack).
#[derive(Debug, Clone, Default)]
struct KeyboardModeStack {
    flags: u16,
    stack: Vec<u16>,
}

impl KeyboardModeStack {
    /// `CSI = flags ; mode u` — mode 1 replace, 2 set bits, 3 clear bits.
    fn apply(&mut self, flags: u16, mode: u16) {
        match mode {
            2 => self.flags |= flags,
            3 => self.flags &= !flags,
            _ => self.flags = flags,
        }
    }

    /// `CSI > flags u` — push current, set flags (omitted → 0).
    fn push(&mut self, flags: u16) {
        if self.stack.len() >= KEYBOARD_STACK_MAX {
            self.stack.remove(0);
        }
        self.stack.push(self.flags);
        self.flags = flags;
    }

    /// `CSI < n u` — pop n entries (default 1); empty stack → flags 0.
    fn pop(&mut self, n: usize) {
        let n = n.max(1);
        for _ in 0..n {
            match self.stack.pop() {
                Some(prev) => self.flags = prev,
                None => {
                    self.flags = 0;
                    break;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.flags = 0;
        self.stack.clear();
    }
}

const MAX_ATTENTION_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 1024;

const MAX_STATE_KEYBOARD_STACK: usize = 16;

/// Emulator snapshot format version. Independent from protocol and package versions.
pub const EMULATOR_STATE_FORMAT_VERSION: u16 = 1;

/// Serializable emulator state. Parser continuation state is intentionally not
/// represented; export rejects snapshots taken away from a parser boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmulatorStateV1 {
    pub format_version: u16,
    pub screen: ScreenStateV1,
    pub collects_apc: bool,
    pub bracketed_paste: bool,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub mouse: MouseModeStateV1,
    pub focus_report: bool,
    pub keyboard_main: KeyboardModeStateV1,
    pub keyboard_alt: KeyboardModeStateV1,
    pub cwd: Option<PathBuf>,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub graphics: GraphicsStateV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseModeStateV1 {
    pub m1000: bool,
    pub m1002: bool,
    pub m1003: bool,
    pub sgr: bool,
    pub wheel_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardModeStateV1 {
    pub flags: u16,
    pub stack: Vec<u16>,
}

/// Failure returned by snapshot export/import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    UnsupportedVersion { found: u16, expected: u16 },
    ParserNotAtBoundary,
    Invalid(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { found, expected } => {
                write!(
                    f,
                    "unsupported emulator state version {found}; expected {expected}"
                )
            }
            Self::ParserNotAtBoundary => {
                f.write_str("cannot export emulator state while a parser sequence is incomplete")
            }
            Self::Invalid(message) => write!(f, "invalid emulator state: {message}"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<prismattyc_core::ScreenStateError> for StateError {
    fn from(error: prismattyc_core::ScreenStateError) -> Self {
        Self::Invalid(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ParserBoundary {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    String,
    StringEscape,
    Utf8(u8),
}

impl ParserBoundary {
    fn is_ground(self) -> bool {
        matches!(self, Self::Ground)
    }

    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.push_byte(byte);
        }
    }

    fn push_byte(&mut self, byte: u8) {
        match *self {
            Self::Utf8(remaining) => {
                if (0x80..=0xbf).contains(&byte) {
                    *self = if remaining == 1 {
                        Self::Ground
                    } else {
                        Self::Utf8(remaining - 1)
                    };
                } else {
                    *self = Self::Ground;
                    self.push_byte(byte);
                }
            }
            Self::Ground => match byte {
                0x1b => *self = Self::Escape,
                0x9b => *self = Self::Csi,
                0x9d => *self = Self::Osc,
                0x90 | 0x98 | 0x9e | 0x9f => *self = Self::String,
                0xc2..=0xdf => *self = Self::Utf8(1),
                0xe0..=0xef => *self = Self::Utf8(2),
                0xf0..=0xf4 => *self = Self::Utf8(3),
                _ => {}
            },
            Self::Escape => match byte {
                0x1b => {}
                b'[' => *self = Self::Csi,
                b']' => *self = Self::Osc,
                b'P' | b'X' | b'^' | b'_' => *self = Self::String,
                0x20..=0x2f => {}
                0x30..=0x7e | 0x9c => *self = Self::Ground,
                _ => *self = Self::Ground,
            },
            Self::Csi => match byte {
                0x1b => *self = Self::Escape,
                0x9c => *self = Self::Ground,
                0x40..=0x7e => *self = Self::Ground,
                _ => {}
            },
            Self::Osc => match byte {
                0x07 | 0x9c => *self = Self::Ground,
                0x1b => *self = Self::OscEscape,
                _ => {}
            },
            Self::OscEscape => match byte {
                b'\\' | 0x9c => *self = Self::Ground,
                0x1b => {}
                _ => *self = Self::Osc,
            },
            Self::String => match byte {
                0x1b => *self = Self::StringEscape,
                0x9c => *self = Self::Ground,
                _ => {}
            },
            Self::StringEscape => match byte {
                b'\\' | 0x9c => *self = Self::Ground,
                0x1b => {}
                _ => *self = Self::String,
            },
        }
    }
}

/// Stateful decoder for the conservative VT subset supported by the MVP.
pub struct Emulator {
    parser: Parser,
    parser_boundary: ParserBoundary,
    screen: Screen,
    /// Present only when experimental APC collection is enabled.
    apc: Option<ApcCollector>,
    /// Child requested bracketed paste (DECSET/DECRST `?2004`).
    bracketed_paste: bool,
    /// Child cursor visibility (DECTCEM DECSET/DECRST `?25`). Default true.
    cursor_visible: bool,
    /// Child cursor shape (DECSCUSR `CSI Ps SP q`). Default block.
    cursor_shape: CursorShape,
    /// Application mouse tracking / SGR flags (mouse input).
    mouse: MouseModeFlags,
    /// Focus in/out reporting (DECSET `?1004`). Host sends CSI I / CSI O.
    focus_report: bool,
    /// Kitty keyboard progressive enhancement — primary screen stack.
    keyboard_main: KeyboardModeStack,
    /// Kitty keyboard progressive enhancement — alternate screen stack.
    keyboard_alt: KeyboardModeStack,
    /// Bytes the host must write to the child PTY (DSR/CPR, DA1, …).
    /// Drained via [`Self::take_pending_replies`] after each [`Self::feed`].
    pending_replies: Vec<Vec<u8>>,
    /// Last OSC 7 working directory reported by the child (if any).
    cwd: Option<PathBuf>,
    /// BEL (0x07) seen since the last [`Self::take_pending_bell`].
    pending_bell: bool,
    /// Latest validated agent-attention message since the last take.
    pending_attention: Option<String>,
    /// Window title from OSC 0/2 since the last [`Self::take_pending_title`].
    pending_title: Option<String>,
    /// Kitty graphics decoder state (always active; independent of rich mode).
    graphics: graphics::GraphicsState,
    /// Graphics APC sidecar (independent of the Prismattyc `apc` collector).
    graphics_apc: prismattyc_protocol::GraphicsApcCollector,
    /// Cell size in pixels for XTWINOPS (`CSI 16 t`) and window-pixel reports
    /// (`CSI 14 t`). Host updates these from FontMetrics. Ghostty keeps the
    /// same pair on `Terminal` (`width_px = cols * cell.width`).
    cell_width_px: u32,
    cell_height_px: u32,
}

impl Emulator {
    /// Classic Phase 0A path: no APC sidecar allocation or scan.
    pub fn new(columns: usize, rows: usize, max_scrollback: usize) -> Self {
        Self {
            parser: Parser::new(),
            parser_boundary: ParserBoundary::default(),
            screen: Screen::new(columns, rows, max_scrollback),
            apc: None,
            bracketed_paste: false,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            mouse: MouseModeFlags::default(),
            focus_report: false,
            keyboard_main: KeyboardModeStack::default(),
            keyboard_alt: KeyboardModeStack::default(),
            pending_replies: Vec::new(),
            cwd: None,
            pending_bell: false,
            pending_attention: None,
            pending_title: None,
            graphics: graphics::GraphicsState::new(),
            graphics_apc: prismattyc_protocol::GraphicsApcCollector::new(),
            cell_width_px: graphics::NOMINAL_CELL_W_PX,
            cell_height_px: graphics::NOMINAL_CELL_H_PX,
        }
    }

    /// Experimental path: construct the APC collector for control bodies.
    pub fn new_experimental(columns: usize, rows: usize, max_scrollback: usize) -> Self {
        Self {
            parser: Parser::new(),
            parser_boundary: ParserBoundary::default(),
            screen: Screen::new(columns, rows, max_scrollback),
            apc: Some(ApcCollector::new()),
            bracketed_paste: false,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            mouse: MouseModeFlags::default(),
            focus_report: false,
            keyboard_main: KeyboardModeStack::default(),
            keyboard_alt: KeyboardModeStack::default(),
            pending_replies: Vec::new(),
            cwd: None,
            pending_bell: false,
            pending_attention: None,
            pending_title: None,
            graphics: graphics::GraphicsState::new(),
            graphics_apc: prismattyc_protocol::GraphicsApcCollector::new(),
            cell_width_px: graphics::NOMINAL_CELL_W_PX,
            cell_height_px: graphics::NOMINAL_CELL_H_PX,
        }
    }

    /// Host FontMetrics → XTWINOPS / Kitty size reports.
    pub fn set_cell_pixels(&mut self, width: u32, height: u32) -> bool {
        let changed = self.cell_width_px != width.max(1) || self.cell_height_px != height.max(1);
        self.cell_width_px = width.max(1);
        self.cell_height_px = height.max(1);
        changed
    }

    /// Export stable emulator state. Call this only between complete parser
    /// sequences; split CSI/OSC/DCS/APC and UTF-8 input is rejected.
    pub fn export_state(&self) -> Result<EmulatorStateV1, StateError> {
        if !self.parser_boundary.is_ground() || self.apc_pending() {
            return Err(StateError::ParserNotAtBoundary);
        }
        let graphics = self
            .graphics
            .export_state()
            .map_err(|message| StateError::Invalid(message.into()))?;
        Ok(EmulatorStateV1 {
            format_version: EMULATOR_STATE_FORMAT_VERSION,
            screen: self.screen.export_state(),
            collects_apc: self.collects_apc(),
            bracketed_paste: self.bracketed_paste,
            cursor_visible: self.cursor_visible,
            cursor_shape: self.cursor_shape,
            mouse: MouseModeStateV1 {
                m1000: self.mouse.m1000,
                m1002: self.mouse.m1002,
                m1003: self.mouse.m1003,
                sgr: self.mouse.sgr,
                wheel_only: self.mouse.wheel_only,
            },
            focus_report: self.focus_report,
            keyboard_main: KeyboardModeStateV1 {
                flags: self.keyboard_main.flags,
                stack: self.keyboard_main.stack.clone(),
            },
            keyboard_alt: KeyboardModeStateV1 {
                flags: self.keyboard_alt.flags,
                stack: self.keyboard_alt.stack.clone(),
            },
            cwd: self.cwd.clone(),
            cell_width_px: self.cell_width_px,
            cell_height_px: self.cell_height_px,
            graphics,
        })
    }

    /// Import a complete emulator state into a fresh parser at a safe boundary.
    pub fn import_state(state: EmulatorStateV1) -> Result<Self, StateError> {
        if state.format_version != EMULATOR_STATE_FORMAT_VERSION {
            return Err(StateError::UnsupportedVersion {
                found: state.format_version,
                expected: EMULATOR_STATE_FORMAT_VERSION,
            });
        }
        validate_keyboard_state(&state.keyboard_main)?;
        validate_keyboard_state(&state.keyboard_alt)?;
        let screen = Screen::import_state(state.screen)?;
        let graphics =
            graphics::GraphicsState::import_state(state.graphics).map_err(StateError::Invalid)?;
        Ok(Self {
            parser: Parser::new(),
            parser_boundary: ParserBoundary::default(),
            screen,
            apc: state.collects_apc.then(ApcCollector::new),
            bracketed_paste: state.bracketed_paste,
            cursor_visible: state.cursor_visible,
            cursor_shape: state.cursor_shape,
            mouse: MouseModeFlags {
                m1000: state.mouse.m1000,
                m1002: state.mouse.m1002,
                m1003: state.mouse.m1003,
                sgr: state.mouse.sgr,
                wheel_only: state.mouse.wheel_only,
            },
            focus_report: state.focus_report,
            keyboard_main: KeyboardModeStack {
                flags: state.keyboard_main.flags,
                stack: state.keyboard_main.stack,
            },
            keyboard_alt: KeyboardModeStack {
                flags: state.keyboard_alt.flags,
                stack: state.keyboard_alt.stack,
            },
            pending_replies: Vec::new(),
            cwd: state.cwd,
            pending_bell: false,
            pending_attention: None,
            pending_title: None,
            graphics,
            graphics_apc: prismattyc_protocol::GraphicsApcCollector::new(),
            cell_width_px: state.cell_width_px.max(1),
            cell_height_px: state.cell_height_px.max(1),
        })
    }

    pub const fn collects_apc(&self) -> bool {
        self.apc.is_some()
    }

    /// True when the sidecar has consumed `ESC`/`ESC _` but not yet ST.
    pub fn apc_pending(&self) -> bool {
        self.graphics_apc.is_active()
            || self
                .apc
                .as_ref()
                .is_some_and(prismattyc_protocol::ApcCollector::is_active)
    }

    /// Kitty graphics images currently placed on the screen.
    pub fn images(&self) -> &[graphics::PlacedImage] {
        self.graphics.images()
    }

    pub fn image_by_id(&self, id: u32) -> Option<&graphics::StoredImage> {
        self.graphics.image_by_id(id)
    }

    pub fn virtual_placement(
        &self,
        image_id: u32,
        placement_id: u32,
    ) -> Option<&graphics::VirtualPlacement> {
        self.graphics.virtual_placement(image_id, placement_id)
    }

    pub const fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Whether the child wants the cursor visible (DECTCEM `CSI ? 25 h/l`).
    pub const fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// DECSCUSR shape. Host paints this; nested prism uses the outer caret.
    pub const fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    /// Highest enabled application mouse tracking level (mouse input).
    pub const fn mouse_tracking(&self) -> MouseTracking {
        self.mouse.tracking()
    }

    /// Whether the child prefers SGR mouse encoding (DECSET `?1006`).
    pub const fn mouse_sgr(&self) -> bool {
        self.mouse.sgr
    }

    /// Prismattyc-private wheel-only reporting (DECSET `?7700` / mouse input addendum).
    ///
    /// Does not change [`mouse_tracking`] — 1000/1002/1003 still own press/drag.
    pub const fn mouse_wheel_only(&self) -> bool {
        self.mouse.wheel_only
    }

    /// Encode a wheel report for the child (1000-level tracking or 7700).
    pub const fn reports_app_wheel(&self) -> bool {
        self.mouse_tracking().is_on() || self.mouse_wheel_only()
    }

    /// Whether the child requested focus in/out reports (DECSET `?1004`).
    pub const fn focus_report(&self) -> bool {
        self.focus_report
    }

    /// Active Kitty progressive-enhancement flags for the current screen.
    pub fn keyboard_flags(&self) -> u16 {
        if self.screen.alt_active() {
            self.keyboard_alt.flags
        } else {
            self.keyboard_main.flags
        }
    }

    /// True when the child wants press/repeat/release event-type reporting.
    pub fn keyboard_reports_event_types(&self) -> bool {
        self.keyboard_flags() & KITTY_EVENT_TYPES != 0
    }

    pub const fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Consume this frame's grid damage. The host calls this each paint.
    pub fn take_damage(&mut self) -> GridDamage {
        self.screen.take_damage()
    }

    // opt-in passthrough: rows scrolled off the alt screen feed
    /// history. Classic claim paths must never call this.
    pub fn set_retain_alt_history(&mut self, retain: bool) {
        self.screen.set_retain_alt_history(retain);
    }

    /// Working directory last reported by the child via OSC 7 (`file://…`).
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Take host→child reply bytes queued during the last feed(s) (DSR/CPR, DA1, …).
    pub fn take_pending_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_replies)
    }

    /// True if BEL was received since the previous take.
    pub fn take_pending_bell(&mut self) -> bool {
        std::mem::take(&mut self.pending_bell)
    }

    /// Take the latest validated agent-attention message, if one arrived.
    pub fn take_pending_attention(&mut self) -> Option<String> {
        self.pending_attention.take()
    }

    /// Take the window title from OSC 0/2, if one arrived since the last take.
    pub fn take_pending_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }

    /// Feed child output into the classic VT parser and optional APC sidecar.
    ///
    /// Returns complete APC bodies (or discards) when collection is enabled;
    /// otherwise always returns an empty vec without scanning.
    /// Side-channel replies (e.g. CPR for `CSI 6 n`) accumulate in
    /// [`Self::pending_replies`] for the host to forward to the child PTY.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<CollectedApc> {
        self.parser_boundary.push(bytes);
        let graphics_events = self.graphics_apc.push_with_offsets(bytes);
        let events = match self.apc.as_mut() {
            Some(apc) => apc.push(bytes),
            None => Vec::new(),
        };
        // Query replies first: kitty sends a=q then CSI c and treats DA1 as
        // the sync barrier. Placement still runs after CSI so a
        // same-chunk CUP lands before a=T.
        for event in &graphics_events {
            if graphics::GraphicsState::is_query(&event.apc) {
                self.graphics
                    .handle(&event.apc, &self.screen, &mut self.pending_replies);
            }
        }
        let mut offset = 0;
        for event in &graphics_events {
            let alt_before = self.screen.alt_active();
            let clears_before = self.screen.full_clears();
            self.advance_parser(&bytes[offset..event.end]);
            self.clear_graphics_if_screen_changed(alt_before, clears_before);
            if !graphics::GraphicsState::is_query(&event.apc) {
                self.graphics
                    .handle(&event.apc, &self.screen, &mut self.pending_replies);
            }
            offset = event.end;
        }
        let alt_before = self.screen.alt_active();
        let clears_before = self.screen.full_clears();
        self.advance_parser(&bytes[offset..]);
        self.clear_graphics_if_screen_changed(alt_before, clears_before);
        events
    }

    fn clear_graphics_if_screen_changed(&mut self, alt_before: bool, clears_before: u64) {
        if self.screen.alt_active() != alt_before || self.screen.full_clears() != clears_before {
            self.graphics.clear();
        }
    }

    fn advance_parser(&mut self, bytes: &[u8]) {
        let Self {
            parser,
            screen,
            bracketed_paste,
            cursor_visible,
            cursor_shape,
            mouse,
            focus_report,
            keyboard_main,
            keyboard_alt,
            pending_replies,
            cwd,
            pending_bell,
            pending_attention,
            pending_title,
            cell_width_px,
            cell_height_px,
            ..
        } = self;
        parser.advance(
            &mut ScreenPerformer {
                screen,
                bracketed_paste,
                cursor_visible,
                cursor_shape,
                mouse,
                focus_report,
                keyboard_main,
                keyboard_alt,
                pending_replies,
                cwd,
                pending_bell,
                pending_attention,
                pending_title,
                cell_width_px: *cell_width_px,
                cell_height_px: *cell_height_px,
            },
            bytes,
        );
    }

    /// Replay a resize emitted by a daemon before primary-screen reflow.
    pub fn resize_legacy(&mut self, columns: usize, rows: usize) {
        self.screen.resize(columns, rows);
        self.graphics.clear();
    }

    /// Resize the logical screen (host SIGWINCH path). Does not touch the PTY.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.screen.resize_reflow(columns, rows);
        self.graphics.clear();
    }
}

fn validate_keyboard_state(state: &KeyboardModeStateV1) -> Result<(), StateError> {
    if state.stack.len() > MAX_STATE_KEYBOARD_STACK {
        return Err(StateError::Invalid(
            "keyboard mode stack is too deep".into(),
        ));
    }
    Ok(())
}

struct ScreenPerformer<'a> {
    screen: &'a mut Screen,
    bracketed_paste: &'a mut bool,
    cursor_visible: &'a mut bool,
    cursor_shape: &'a mut CursorShape,
    mouse: &'a mut MouseModeFlags,
    focus_report: &'a mut bool,
    keyboard_main: &'a mut KeyboardModeStack,
    keyboard_alt: &'a mut KeyboardModeStack,
    pending_replies: &'a mut Vec<Vec<u8>>,
    cwd: &'a mut Option<PathBuf>,
    pending_bell: &'a mut bool,
    pending_attention: &'a mut Option<String>,
    pending_title: &'a mut Option<String>,
    cell_width_px: u32,
    cell_height_px: u32,
}

impl ScreenPerformer<'_> {
    fn keyboard_mut(&mut self) -> &mut KeyboardModeStack {
        if self.screen.alt_active() {
            self.keyboard_alt
        } else {
            self.keyboard_main
        }
    }
}

impl Perform for ScreenPerformer<'_> {
    fn print(&mut self, character: char) {
        self.screen.put_char(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => *self.pending_bell = true,
            0x08 => self.screen.backspace(),
            0x09 => self.screen.tab(),
            0x0a..=0x0c => self.screen.line_feed(),
            0x0d => self.screen.carriage_return(),
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}

    fn put(&mut self, _: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 9 ; body — agent attention (iTerm2-compatible producer path).
        if params.first().is_some_and(|p| *p == b"9") {
            if let Some(message) = join_attention_parts(params.get(1..).unwrap_or_default()) {
                *self.pending_attention = Some(message);
            }
            return;
        }
        // OSC 777 ; notify ; title ; body — common notification protocol.
        if params.first().is_some_and(|p| *p == b"777") {
            if params.get(1).is_some_and(|p| *p == b"notify") {
                if let (Some(title), Some(body_parts)) = (params.get(2), params.get(3..)) {
                    let title = validated_attention(title);
                    let body = join_attention_parts(body_parts);
                    if let (Some(title), Some(body)) = (title, body) {
                        let message = format!("{title}: {body}");
                        if let Some(message) = validated_attention(message.as_bytes()) {
                            *self.pending_attention = Some(message);
                        }
                    }
                }
            }
            return;
        }
        // OSC 99 ; metadata ; body — only consume completed notifications.
        // A preceding m=1 marker means more chunks are still pending.
        if params.first().is_some_and(|p| *p == b"99") {
            let Some(body) = params.last() else {
                return;
            };
            let more = params
                .get(1..params.len().saturating_sub(1))
                .unwrap_or_default()
                .iter()
                .any(|param| *param == b"m=1");
            if !more {
                if let Some(message) = validated_attention(body) {
                    *self.pending_attention = Some(message);
                }
            }
            return;
        }
        // OSC 8 ; params ; URI — hyperlink cells until OSC 8 ;; closes it.
        if params.first().is_some_and(|p| *p == b"8") {
            let Some(link_params) = params.get(1) else {
                self.screen.clear_hyperlink();
                return;
            };
            let uri_parts = params.get(2..).unwrap_or_default();
            let uri_len = uri_parts
                .iter()
                .enumerate()
                .try_fold(0usize, |total, (index, part)| {
                    total
                        .checked_add(part.len())?
                        .checked_add(usize::from(index > 0))
                });
            let Some(uri_len) = uri_len.filter(|len| *len <= MAX_HYPERLINK_URI_BYTES) else {
                self.screen.clear_hyperlink();
                return;
            };
            if uri_len == 0 {
                self.screen.clear_hyperlink();
                return;
            }
            let mut uri_bytes = Vec::with_capacity(uri_len);
            for (index, part) in uri_parts.iter().enumerate() {
                if index > 0 {
                    uri_bytes.push(b';');
                }
                uri_bytes.extend_from_slice(part);
            }
            let Some(uri) = std::str::from_utf8(&uri_bytes).ok() else {
                self.screen.clear_hyperlink();
                return;
            };
            let id = osc8_id(link_params);
            self.screen.set_hyperlink(id, uri);
            return;
        }
        // OSC 7 ; file://host/path — shell working directory (VTE / many prompts).
        if params.first().is_some_and(|p| *p == b"7") {
            if let Some(uri) = params.get(1) {
                if let Some(path) = parse_osc7_cwd(uri) {
                    *self.cwd = Some(path);
                }
            }
            return;
        }
        // OSC 0/2 ; title — window title (OSC 0 also sets the icon name).
        if params.first().is_some_and(|p| *p == b"0" || *p == b"2") {
            if let Some(text) = parse_osc_title(params) {
                *self.pending_title = Some(text);
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }

        // Private modes: CSI ? … h/l (DECSET/DECRST).
        // Kitty keyboard query: CSI ? u → CSI ? flags u
        if intermediates == b"?" {
            match action {
                'h' => apply_private_mode(
                    self.screen,
                    self.bracketed_paste,
                    self.cursor_visible,
                    self.mouse,
                    self.focus_report,
                    params,
                    true,
                ),
                'l' => apply_private_mode(
                    self.screen,
                    self.bracketed_paste,
                    self.cursor_visible,
                    self.mouse,
                    self.focus_report,
                    params,
                    false,
                ),
                'u' => {
                    let flags = self.keyboard_mut().flags;
                    self.pending_replies
                        .push(format!("\x1b[?{flags}u").into_bytes());
                }
                _ => {}
            }
            return;
        }

        // Soft terminal reset / DECSTR: CSI ! p.
        if intermediates == b"!" {
            if action == 'p' {
                self.screen.soft_reset();
                // Claimed-mode polish: unstick DECTCEM + focus report. Keep app
                // mouse (editors soft-reset mid-session) and bracketed paste.
                *self.cursor_visible = true;
                *self.cursor_shape = CursorShape::Block;
                *self.focus_report = false;
            }
            return;
        }

        // DECSCUSR — CSI Ps SP q. Ghostty/xterm: 0/1/2 block, 3/4 underline, 5/6 bar.
        if intermediates == b" " && action == 'q' {
            let ps = params_vec(params).first().copied().unwrap_or(0);
            *self.cursor_shape = match ps {
                0..=2 => CursorShape::Block,
                3..=4 => CursorShape::Underline,
                5..=6 => CursorShape::Bar,
                _ => return,
            };
            return;
        }

        // Kitty keyboard progressive enhancement (intermediate required):
        //   CSI = flags ; mode u  — set flags
        //   CSI > flags u         — push
        //   CSI < n u             — pop
        // Bare CSI u (no intermediate) is SCORC — handled below with CSI s.
        if action == 'u' && !intermediates.is_empty() {
            let values = params_vec(params);
            match intermediates {
                [b'='] => {
                    let flags = values.first().copied().unwrap_or(0);
                    let mode = values.get(1).copied().unwrap_or(1);
                    self.keyboard_mut().apply(flags, mode);
                }
                [b'>'] => {
                    let flags = values.first().copied().unwrap_or(0);
                    self.keyboard_mut().push(flags);
                }
                [b'<'] => {
                    let n = values.first().copied().unwrap_or(1) as usize;
                    self.keyboard_mut().pop(n);
                }
                _ => {}
            }
            return;
        }

        if !intermediates.is_empty() {
            return;
        }

        match action {
            'A' => self.screen.cursor_up(first_param(params, 1)),
            'B' => self.screen.cursor_down(first_param(params, 1)),
            'C' => self.screen.cursor_forward(first_param(params, 1)),
            'D' => self.screen.cursor_back(first_param(params, 1)),
            'G' => self
                .screen
                .set_cursor_position(self.screen.cursor().row, first_param(params, 1) - 1),
            // VPA — Vertical Position Absolute: set row (1-based), keep column.
            'd' => self
                .screen
                .set_cursor_position(first_param(params, 1) - 1, self.screen.cursor().column),
            'H' | 'f' => {
                let values = params_vec(params);
                let row = values.first().copied().unwrap_or(1).max(1) as usize - 1;
                let column = values.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.screen.set_cursor_position(row, column);
            }
            'J' => self.screen.erase_display(first_param(params, 0) as u16),
            'K' => self.screen.erase_line(first_param(params, 0) as u16),
            // CSI n @ — Insert Characters (ICH); CSI n P — Delete Characters (DCH);
            // CSI n X — Erase Characters (ECH). Default n=1.
            '@' => self.screen.insert_chars(first_param(params, 1)),
            'P' => self.screen.delete_chars(first_param(params, 1)),
            'X' => self.screen.erase_chars(first_param(params, 1)),
            // CSI n L — Insert Lines (IL); CSI n M — Delete Lines (DL). Default n=1.
            'L' => self.screen.insert_lines(first_param(params, 1)),
            'M' => self.screen.delete_lines(first_param(params, 1)),
            // CSI n S — Scroll Up (SU) within DECSTBM; default n=1.
            'S' => self.screen.scroll_up_region(first_param(params, 1)),
            // CSI n T — Scroll Down (SD). Only bare T (no intermediates, ≤1 param).
            // Five-param CSI … T is xterm highlight mouse tracking — leave no-op.
            'T' => {
                let values = params_vec(params);
                if values.len() <= 1 {
                    self.screen.scroll_down_region(first_param(params, 1));
                }
            }
            'm' => apply_sgr(self.screen, params),
            // XTWINOPS (Ghostty `size_report.zig`): CSI 14 t / 16 t / 18 t.
            // Kitty graphics clients use ioctl TIOCGWINSZ first, then these.
            't' => {
                let n = first_param(params, 0);
                let cols = self.screen.columns() as u32;
                let rows = self.screen.rows() as u32;
                let cw = self.cell_width_px.max(1);
                let ch = self.cell_height_px.max(1);
                match n {
                    14 => {
                        // ESC [ 4 ; height ; width t  (text area, pixels)
                        self.pending_replies.push(
                            format!(
                                "\x1b[4;{};{}t",
                                rows.saturating_mul(ch),
                                cols.saturating_mul(cw)
                            )
                            .into_bytes(),
                        );
                    }
                    16 => {
                        // ESC [ 6 ; cell_height ; cell_width t
                        self.pending_replies
                            .push(format!("\x1b[6;{ch};{cw}t").into_bytes());
                    }
                    18 => {
                        // ESC [ 8 ; rows ; cols t
                        self.pending_replies
                            .push(format!("\x1b[8;{rows};{cols}t").into_bytes());
                    }
                    _ => {}
                }
            }
            'r' => {
                let values = params_vec(params);
                let top = values.first().copied().unwrap_or(0) as usize;
                let bottom = values.get(1).copied().unwrap_or(0) as usize;
                if top == 0 && bottom == 0 {
                    self.screen.reset_scroll_region();
                } else {
                    self.screen.set_scroll_region(top.max(1), bottom);
                }
            }
            // DSR: CSI 6 n → CPR `CSI row;col R` (1-based).
            // CSI 5 n → status OK `CSI 0 n` (residual).
            'n' => {
                let ps = params_vec(params).first().copied().unwrap_or(0);
                match ps {
                    5 => {
                        // Device status: 0 = ready, no malfunction.
                        self.pending_replies.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // CPR is origin-relative when DECOM is set.
                        let cursor = self.screen.cursor_report();
                        let reply = format!(
                            "\x1b[{};{}R",
                            cursor.row.saturating_add(1),
                            cursor.column.saturating_add(1)
                        );
                        self.pending_replies.push(reply.into_bytes());
                    }
                    _ => {}
                }
            }
            // DA1 (Primary Device Attributes). CSI c / CSI 0 c.
            // VT100 + AVO. Same payload grok 1.0.5 accepted live.
            // Other Ps stay silent (xterm). CSI > c (DA2) is not this arm.
            'c' => {
                let ps = params_vec(params).first().copied().unwrap_or(0);
                if ps == 0 {
                    self.pending_replies.push(b"\x1b[?1;2c".to_vec());
                }
            }
            // SCOSC / SCORC (ANSI.SYS / xterm): CSI s save, CSI u restore.
            // Same cursor/wrap/SGR slot as DECSC/DECRC (ESC 7 / ESC 8) —.
            's' => self.screen.save_cursor(),
            'u' => self.screen.restore_cursor(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }

        match byte {
            b'7' => self.screen.save_cursor(),
            b'8' => self.screen.restore_cursor(),
            // RIS — Reset to Initial State (hard-ish subset).
            b'c' => {
                use prismattyc_core::AltScreenMode;
                if self.screen.alt_active() {
                    // Prefer 1049 leave: restore primary cursor/wrap then RIS homes.
                    self.screen.leave_alt_screen(AltScreenMode::Mode1049);
                }
                self.screen.ris_reset();
                *self.bracketed_paste = false;
                // mouse input: RIS clears application mouse modes.
                self.mouse.clear();
                *self.focus_report = false;
                // DECTCEM defaults to visible after RIS. DECSCUSR → block.
                *self.cursor_visible = true;
                *self.cursor_shape = CursorShape::Block;
                // Kitty keyboard protocol: clear both screen stacks.
                self.keyboard_main.clear();
                self.keyboard_alt.clear();
            }
            b'D' => self.screen.line_feed(), // IND — Index
            b'E' => {
                self.screen.carriage_return();
                self.screen.line_feed();
            }
            b'M' => self.screen.reverse_index(), // RI — Reverse Index
            _ => {}
        }
    }
}

fn validated_attention(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_ATTENTION_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    (!text.chars().any(char::is_control)).then(|| text.to_string())
}

fn join_attention_parts(parts: &[&[u8]]) -> Option<String> {
    let size = parts
        .iter()
        .enumerate()
        .try_fold(0usize, |total, (index, part)| {
            total
                .checked_add(part.len())?
                .checked_add(usize::from(index > 0))
        })?;
    if size > MAX_ATTENTION_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(size);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            bytes.push(b';');
        }
        bytes.extend_from_slice(part);
    }
    validated_attention(&bytes)
}

fn osc8_id(params: &[u8]) -> Option<&str> {
    params
        .split(|byte| *byte == b':')
        .find_map(|param| param.strip_prefix(b"id="))
        .and_then(|id| std::str::from_utf8(id).ok())
        .filter(|id| !id.is_empty())
}

fn params_vec(params: &Params) -> Vec<u16> {
    params
        .iter()
        .map(|subparams| subparams.first().copied().unwrap_or(0))
        .collect()
}

fn first_param(params: &Params, default: usize) -> usize {
    params_vec(params)
        .first()
        .copied()
        .filter(|value| *value != 0)
        .map_or(default, usize::from)
}

fn apply_private_mode(
    screen: &mut Screen,
    bracketed_paste: &mut bool,
    cursor_visible: &mut bool,
    mouse: &mut MouseModeFlags,
    focus_report: &mut bool,
    params: &Params,
    enable: bool,
) {
    use prismattyc_core::AltScreenMode;
    for value in params_vec(params) {
        match value {
            // DECTCEM: text cursor enable mode.
            25 => *cursor_visible = enable,
            47 => {
                let mode = AltScreenMode::Mode47;
                if enable {
                    screen.enter_alt_screen(mode);
                } else {
                    screen.leave_alt_screen(mode);
                }
            }
            1047 => {
                let mode = AltScreenMode::Mode1047;
                if enable {
                    screen.enter_alt_screen(mode);
                } else {
                    screen.leave_alt_screen(mode);
                }
            }
            1049 => {
                let mode = AltScreenMode::Mode1049;
                if enable {
                    screen.enter_alt_screen(mode);
                } else {
                    screen.leave_alt_screen(mode);
                }
            }
            2004 => *bracketed_paste = enable,
            // DECOM — origin mode: CUP relative to DECSTBM margins.
            6 => screen.set_origin_mode(enable),
            // DECAWM — auto-wrap (default on).
            7 => screen.set_autowrap(enable),
            // Focus in/out reporting: host sends CSI I / CSI O when enabled.
            1004 => *focus_report = enable,
            // Application mouse tracking (mouse input hybrid). Flags are independent;
            // host routes via highest enabled level. 1005/1015/1016 encodings are
            // accepted but not implemented — SGR (1006) or legacy X10 is used.
            1000 => mouse.m1000 = enable,
            1002 => mouse.m1002 = enable,
            1003 => mouse.m1003 = enable,
            1006 => mouse.sgr = enable,
            // Private: wheel reports without claiming buttons.
            7700 => mouse.wheel_only = enable,
            1005 | 1015 | 1016 => {}
            _ => {}
        }
    }
}

/// Apply truecolor / indexed color from components after mode `2` or `5`.
///
/// After mode `2`, ISO allows an optional colorspace `Pi` before `R:G:B`:
/// - ≥4 components → Pi:R:G:B (use R,G,B)
/// - exactly 3 → R:G:B
/// - fewer → incomplete (no color, no leak as independent SGR)
enum ColorDest {
    Fg,
    Bg,
    Underline,
}

fn set_dest_color(style: &mut Style, dest: ColorDest, color: Color) {
    match dest {
        ColorDest::Fg => style.foreground = color,
        ColorDest::Bg => style.background = color,
        ColorDest::Underline => style.underline_color = color,
    }
}

fn apply_extended_color(components: &[u16], dest: ColorDest, style: &mut Style) {
    if components.is_empty() {
        return;
    }
    match components[0] {
        5 => {
            if let Some(&idx) = components.get(1) {
                set_dest_color(style, dest, Color::Indexed(idx.min(255) as u8));
            }
        }
        2 => {
            let after = &components[1..];
            let (r, g, b) = if after.len() >= 4 {
                // ISO 8613-6 / xterm: 2:Pi:Pr:Pg:Pb
                (after[1], after[2], after[3])
            } else if after.len() >= 3 {
                // 2:R:G:B (colon without Pi, or semicolon R;G;B packed)
                (after[0], after[1], after[2])
            } else {
                // Truncated — do not reinterpret leftovers as SGR.
                return;
            };
            set_dest_color(
                style,
                dest,
                Color::Rgb {
                    r: r.min(255) as u8,
                    g: g.min(255) as u8,
                    b: b.min(255) as u8,
                },
            );
        }
        _ => {}
    }
}

fn apply_sgr(screen: &mut Screen, params: &Params) {
    // Walk VTE param *groups* (semicolon separators). Colon subparams stay
    // inside one group — critical so ISO Pi:R:G:B is not confused with
    // classic 38;2;r;g;b followed by more SGR in the same CSI.
    let groups: Vec<Vec<u16>> = {
        let g: Vec<Vec<u16>> = params
            .iter()
            .map(|sub| {
                if sub.is_empty() {
                    vec![0]
                } else {
                    sub.to_vec()
                }
            })
            .collect();
        if g.is_empty() {
            vec![vec![0]]
        } else {
            g
        }
    };

    let mut style = screen.style();
    let mut i = 0;
    while i < groups.len() {
        let group = &groups[i];
        let value = group[0];
        match value {
            0 => style = Style::default(),
            1 => style.bold = true,
            3 => style.italic = true,
            4 => {
                style.underline_style = match group.get(1).copied() {
                    None | Some(1) => UnderlineStyle::Single,
                    Some(0) => UnderlineStyle::None,
                    Some(2) => UnderlineStyle::Double,
                    Some(3) => UnderlineStyle::Curly,
                    Some(4) => UnderlineStyle::Dotted,
                    Some(5) => UnderlineStyle::Dashed,
                    Some(_) => UnderlineStyle::Single,
                };
                style.underline = style.underline_style != UnderlineStyle::None;
            }
            7 => style.inverse = true,
            22 => style.bold = false,
            23 => style.italic = false,
            24 => {
                style.underline = false;
                style.underline_style = UnderlineStyle::None;
            }
            27 => style.inverse = false,
            30..=37 => style.foreground = Color::Ansi((value - 30) as u8),
            39 => style.foreground = Color::Default,
            40..=47 => style.background = Color::Ansi((value - 40) as u8),
            49 => style.background = Color::Default,
            90..=97 => style.foreground = Color::Ansi((value - 90 + 8) as u8),
            100..=107 => style.background = Color::Ansi((value - 100 + 8) as u8),
            38 | 48 | 58 => {
                let dest = match value {
                    38 => ColorDest::Fg,
                    48 => ColorDest::Bg,
                    _ => ColorDest::Underline,
                };
                if group.len() > 1 {
                    // Colon form in-group: 38:2:… or 38:5:n (or hybrid).
                    apply_extended_color(&group[1..], dest, &mut style);
                } else {
                    // Semicolon form: mode and args are following groups.
                    if i + 1 >= groups.len() {
                        break;
                    }
                    let mode_group = &groups[i + 1];
                    let mode = mode_group[0];
                    if mode_group.len() > 1 {
                        apply_extended_color(mode_group, dest, &mut style);
                        i += 1;
                    } else {
                        match mode {
                            5 => {
                                if i + 2 < groups.len() {
                                    let idx = groups[i + 2][0].min(255) as u8;
                                    set_dest_color(&mut style, dest, Color::Indexed(idx));
                                    i += 2;
                                } else {
                                    i += 1;
                                }
                            }
                            2 => {
                                let avail = groups.len().saturating_sub(i + 2);
                                if avail >= 3 {
                                    let r = groups[i + 2][0].min(255) as u8;
                                    let g = groups[i + 3][0].min(255) as u8;
                                    let b = groups[i + 4][0].min(255) as u8;
                                    set_dest_color(&mut style, dest, Color::Rgb { r, g, b });
                                    i += 4;
                                } else if avail > 0 {
                                    i += 1 + avail;
                                } else {
                                    i += 1;
                                }
                            }
                            _ => {
                                i += 1;
                            }
                        }
                    }
                }
            }
            59 => style.underline_color = Color::Default,
            _ => {}
        }
        i += 1;
    }

    screen.set_style(style);
}

/// `TERM` value Prismattyc forces on every child PTY process.
///
/// **Choice:** `xterm-256color`, always set at spawn — never inherited
/// from the outer host.
///
/// Rationale against inheriting outer `TERM` (e.g. `xterm-kitty`, `alacritty`,
/// `ghostty`, `tmux-256color`): those entries advertise proprietary protocols
/// and richer CSI than Prismattyc implements, so apps enable features that fail
/// silently (layout corruption, stuck waits — amplifies 016).
///
/// Why not a narrower well-known name (`vt100`, `ansi`)?
/// - The published matrix claims **256-color + truecolor SGR (F10)**, **alt
///   screen (F4)**, **DECSTBM (F5)**, and **hybrid mouse (F13 / mouse input)**.
///   Those map to the common `xterm-256color` feature set better than
///   `vt100`/`ansi`.
/// - Bundled `prism-256color` (`use=xterm-256color`) rebrands that baseline
///   under Prismattyc's identity and is forced via `TERMINFO` when the database is
///   present; otherwise we fall back to system `xterm-256color`.
///
/// Default child `TERM`: 24-bit / direct color.
///
/// Never inherit the outer host `TERM`. `prismattyc-kitty` (alias
/// `prismattyc-direct`) uses `xterm-direct` caps (`colors#0x1000000`, RGB
/// `setaf`/`setab`) plus the Kitty-shaped user caps Prismattyc actually
/// implements (`fullkbd`, `Tc`, colon `setrgbf`/`setrgbb`, bracketed paste,
/// focus). The name contains `"kitty"` so producers that key on
/// `TERM.includes("kitty")` (Claude Code) emit graphics APC; we do not
/// `use=xterm-kitty` (256-color, styled underline, strikethrough, Sync).
/// `PRISMATTYC_COLOR` can pin 256 or 16. Missing baked entries
/// fall down that ladder; the last system fallback is [`CHILD_TERM_FALLBACK`]
/// (`xterm-256color`), never a bare `-direct` name with no database.
pub const CHILD_TERM: &str = "prismattyc-kitty";

/// 256-color child `TERM` when `PRISMATTYC_COLOR=256`.
pub const CHILD_TERM_256: &str = "prismattyc-256color";

/// ANSI-color child `TERM` when `PRISMATTYC_COLOR=16`.
pub const CHILD_TERM_16: &str = "prismattyc-16color";

/// Safe `TERM` when no Prismattyc database resolved (or after falling off the ladder).
/// `-direct` names are not ubiquitous on every host; stay on `xterm-256color`.
pub const CHILD_TERM_FALLBACK: &str = "xterm-256color";

/// System `TERM` when 16-color was selected and no Prismattyc database resolved.
pub const CHILD_TERM_16_FALLBACK: &str = "xterm";

/// `TERM_PROGRAM` value identifying Prismattyc as the child-facing emulator.
pub const CHILD_TERM_PROGRAM: &str = "prismattyc";

/// `COLORTERM` value for 24-bit and 256 modes (emulator parses 38;2 either way).
pub const CHILD_COLORTERM: &str = "truecolor";

/// `KITTY_WINDOW_ID` value advertised to children so Kitty-graphics producers
/// (e.g. Claude Code) emit real APC `_G` PNG payloads instead of Unicode
/// block-art. Prismattyc fully decodes the Kitty graphics protocol, so we present
/// its capability marker. Claude Code keys on this variable's presence (static,
/// no runtime probe), which is why we set it rather than strip it. The value is
/// a stable non-empty sentinel; the exact number is not read by producers.
pub const CHILD_KITTY_WINDOW_ID: &str = "1";

/// Advertised child color depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildColorDepth {
    Truecolor,
    Indexed256,
    Indexed16,
}

impl ChildColorDepth {
    /// `PRISMATTYC_COLOR` (`truecolor` / `256` / `16`).
    /// Missing or unknown → truecolor.
    pub fn from_env() -> Self {
        let raw = std::env::var("PRISMATTYC_COLOR").ok();
        Self::parse(raw.as_deref())
    }

    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or("") {
            "" | "truecolor" | "true-color" | "24bit" | "24-bit" | "direct" | "true" => {
                Self::Truecolor
            }
            "256" | "256color" | "256-color" => Self::Indexed256,
            "16" | "16color" | "16-color" | "ansi" | "8" | "8color" | "8-color" => Self::Indexed16,
            _ => Self::Truecolor,
        }
    }

    fn prism_term(self) -> &'static str {
        match self {
            Self::Truecolor => CHILD_TERM,
            Self::Indexed256 => CHILD_TERM_256,
            Self::Indexed16 => CHILD_TERM_16,
        }
    }

    fn system_term(self) -> &'static str {
        match self {
            Self::Truecolor | Self::Indexed256 => CHILD_TERM_FALLBACK,
            Self::Indexed16 => CHILD_TERM_16_FALLBACK,
        }
    }

    fn colorterm(self) -> Option<&'static str> {
        match self {
            Self::Truecolor | Self::Indexed256 => Some(CHILD_COLORTERM),
            Self::Indexed16 => None,
        }
    }

    fn ladder(self) -> &'static [Self] {
        match self {
            Self::Truecolor => &[Self::Truecolor, Self::Indexed256, Self::Indexed16],
            Self::Indexed256 => &[Self::Indexed256, Self::Indexed16],
            Self::Indexed16 => &[Self::Indexed16],
        }
    }
}

/// Resolved child `TERM` / `COLORTERM` / `TERMINFO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildTermIdentity {
    pub term: &'static str,
    pub colorterm: Option<&'static str>,
    pub terminfo: Option<std::path::PathBuf>,
}

/// Outer-host identity / proprietary-protocol hints stripped from the child env.
///
/// Apps probe these to enable kitty graphics, VTE-specific behavior, etc.
/// Leaving them set while forcing a Prismattyc `TERM` would still leak the outer
/// host's richer identity.
const CHILD_ENV_STRIP: &[&str] = &[
    "ALACRITTY_LOG",
    "ALACRITTY_SOCKET",
    "ALACRITTY_WINDOW_ID",
    "GHOSTTY_BIN_DIR",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_SHELL_FEATURES",
    "ITERM_PROFILE",
    "ITERM_SESSION_ID",
    "KITTY_PID",
    "KITTY_PUBLIC_KEY",
    "KONSOLE_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "VTE_VERSION",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "WT_PROFILE_ID",
    "WT_SESSION",
];

/// Color-suppressor variables stripped from the child env.
///
/// Agent sandboxes and CI shells export these so tool output stays plain; a
/// daemon started from such a shell would pass them to every pane. Shell
/// profiles run after spawn, so users who want them keep them.
const CHILD_ENV_COLOR_STRIP: &[&str] = &[
    "CARGO_TERM_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "FORCE_COLOR",
    "NO_COLOR",
    "NPM_CONFIG_COLOR",
    "PIP_NO_COLOR",
];

/// Directory containing a compiled ncurses terminfo tree with Prismattyc entries.
///
/// Search order:
/// 1. `PRISMATTYC_TERMINFO` (explicit override)
/// 2. Per-user stable install (`$XDG_DATA_HOME/prismattyc/terminfo`, else
///    `~/.local/share/prismattyc/terminfo`), materialized from bytes baked into
///    this crate when missing or content-stale
/// 3. Next to the running binary (`terminfo/`, `../terminfo`, `../share/terminfo`)
/// 4. `cfg(debug_assertions)` only: workspace `terminfo/` via `CARGO_MANIFEST_DIR`
///
/// Release binaries never return a compile-time build directory.
pub fn resolve_child_terminfo_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    for key in ["PRISMATTYC_TERMINFO"] {
        if let Ok(raw) = std::env::var(key) {
            let p = PathBuf::from(raw);
            if has_terminfo_entry(&p) {
                return p.canonicalize().ok().or(Some(p));
            }
        }
    }

    if let Some(root) = default_stable_terminfo_root() {
        if let Ok(dir) = materialize_bundled_terminfo(&root) {
            if has_terminfo_entry(&dir) {
                return dir.canonicalize().ok().or(Some(dir));
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            for rel in ["terminfo", "../terminfo", "../share/terminfo"] {
                let cand = parent.join(rel);
                if has_terminfo_entry(&cand) {
                    return cand.canonicalize().ok().or(Some(cand));
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    {
        let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../terminfo");
        if has_terminfo_entry(&bundled) {
            return bundled.canonicalize().ok().or(Some(bundled));
        }
    }

    None
}

// Compiled terminfo magics (little-endian). User capabilities (fullkbd, Tc,
// setrgbf/setrgbb, paste, focus) exist only in the 32-bit extended format.
const TERMINFO_MAGIC_EXTENDED: [u8; 2] = [0x1e, 0x02]; // 0x021E
const TERMINFO_MAGIC_LEGACY: [u8; 2] = [0x1a, 0x01]; // 0x011A

// Dual bake: letter subdir `p/` gets 32-bit extended entries (Homebrew /
// Linux ncurses). Hex subdir `70/` gets 16-bit legacy entries (macOS system
// ncurses 6.0.x). Apple ncurses cannot read 0x021E ("missing or unsuitable
// terminal"). Homebrew ncurses on macOS uses `p/` and gets the extras.
// Truecolor on the 16-bit file still works via setaf/setab + COLORTERM;
// only colors# clamps to 32767. prism-16color is already 16-bit.
const BUNDLED_PRISMATTYC_KITTY_EXTENDED: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../terminfo/p/prismattyc-kitty"
));
const BUNDLED_PRISMATTYC_KITTY_LEGACY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../terminfo/legacy/p/prismattyc-kitty"
));
const BUNDLED_PRISMATTYC_256_EXTENDED: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../terminfo/p/prismattyc-256color"
));
const BUNDLED_PRISMATTYC_256_LEGACY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../terminfo/legacy/p/prismattyc-256color"
));
const BUNDLED_PRISMATTYC_16COLOR: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../terminfo/p/prismattyc-16color"
));

const BAKED_EXTENDED: &[(&str, &[u8])] = &[
    ("prismattyc-kitty", BUNDLED_PRISMATTYC_KITTY_EXTENDED),
    ("prismattyc-direct", BUNDLED_PRISMATTYC_KITTY_EXTENDED),
    ("prismattyc-256color", BUNDLED_PRISMATTYC_256_EXTENDED),
    ("prismattyc-16color", BUNDLED_PRISMATTYC_16COLOR),
];

const BAKED_LEGACY: &[(&str, &[u8])] = &[
    ("prismattyc-kitty", BUNDLED_PRISMATTYC_KITTY_LEGACY),
    ("prismattyc-direct", BUNDLED_PRISMATTYC_KITTY_LEGACY),
    ("prismattyc-256color", BUNDLED_PRISMATTYC_256_LEGACY),
    ("prismattyc-16color", BUNDLED_PRISMATTYC_16COLOR),
];

/// The two-char lowercase hex subdirectory ncurses uses for a terminfo name,
/// e.g. `prism-direct` -> `70` (0x70 == 'p'). macOS system ncurses hashes
/// entries into hex subdirs; Debian/Ubuntu ncurses uses first-letter subdirs.
fn terminfo_hex_subdir(name: &str) -> String {
    format!("{:02x}", name.as_bytes().first().copied().unwrap_or(b'p'))
}

fn has_named_terminfo_entry(dir: &std::path::Path, name: &str) -> bool {
    // Accept either layout: the letter subdir (Linux) or the hex subdir (macOS).
    dir.join("p").join(name).is_file() || dir.join(terminfo_hex_subdir(name)).join(name).is_file()
}

fn has_terminfo_entry(dir: &std::path::Path) -> bool {
    BAKED_EXTENDED
        .iter()
        .any(|(name, _)| has_named_terminfo_entry(dir, name))
}

fn default_stable_terminfo_root() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("prismattyc").join("terminfo"));
        }
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("prismattyc")
            .join("terminfo")
    })
}

/// Write baked terminfo under `root/p/` (32-bit extended, letter hash) and
/// `root/<hex>/` (16-bit legacy, macOS system-ncurses hash) when missing or
/// stale. The two layouts carry different compiled bytes so Homebrew curses
/// sees extra caps and Apple ncurses 6.0.x still loads.
pub fn materialize_bundled_terminfo(root: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::fs;
    let dir_p = root.join("p");
    fs::create_dir_all(&dir_p)?;
    for (name, bytes) in BAKED_EXTENDED {
        debug_assert!(
            *name == "prismattyc-16color"
                || (bytes.len() >= 2 && bytes[..2] == TERMINFO_MAGIC_EXTENDED),
            "extended terminfo {name} must be magic 0x021E"
        );
        materialize_one(&dir_p, name, bytes)?;
    }
    for (name, bytes) in BAKED_LEGACY {
        debug_assert!(
            bytes.len() >= 2 && bytes[..2] == TERMINFO_MAGIC_LEGACY,
            "legacy terminfo {name} must be magic 0x011A"
        );
        let dir_hex = root.join(terminfo_hex_subdir(name));
        fs::create_dir_all(&dir_hex)?;
        materialize_one(&dir_hex, name, bytes)?;
    }
    Ok(root.to_path_buf())
}

fn materialize_one(dir_p: &std::path::Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::fs;
    let dest = dir_p.join(name);
    if dest.is_file() && fs::read(&dest).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    let tmp = dir_p.join(format!(
        ".{}.{}.{}.tmp",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    fs::write(&tmp, bytes)?;
    if let Err(err) = fs::rename(&tmp, &dest) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Effective child `TERM` after `PRISMATTYC_COLOR` and terminfo fallback.
pub fn effective_child_term() -> &'static str {
    child_term_identity().term
}

/// Resolve `TERM`, optional `COLORTERM`, and `TERMINFO` for a new child.
pub fn child_term_identity() -> ChildTermIdentity {
    let want = ChildColorDepth::from_env();
    if let Some(dir) = resolve_child_terminfo_dir() {
        for depth in want.ladder() {
            if has_named_terminfo_entry(&dir, depth.prism_term()) {
                return ChildTermIdentity {
                    term: depth.prism_term(),
                    colorterm: depth.colorterm(),
                    terminfo: Some(dir),
                };
            }
        }
    }
    ChildTermIdentity {
        term: want.system_term(),
        colorterm: want.colorterm(),
        terminfo: None,
    }
}

/// Apply Prismattyc's child terminal identity to a [`CommandBuilder`].
///
/// Forces the policy `TERM` / [`CHILD_TERM_PROGRAM`], sets `COLORTERM` for
/// truecolor and 256 (not 16), points `TERMINFO` at the bundled database when
/// found, and removes outer-host identity variables in [`CHILD_ENV_STRIP`]
/// plus inherited color suppressors in [`CHILD_ENV_COLOR_STRIP`].
pub fn apply_child_term_env(command: &mut CommandBuilder) {
    let identity = child_term_identity();
    command.env("TERM", identity.term);
    if let Some(dir) = identity.terminfo {
        command.env("TERMINFO", dir);
    } else {
        command.env_remove("TERMINFO");
    }
    command.env("TERM_PROGRAM", CHILD_TERM_PROGRAM);
    // Advertise Kitty-graphics capability; Prismattyc decodes the protocol in full.
    command.env("KITTY_WINDOW_ID", CHILD_KITTY_WINDOW_ID);
    match identity.colorterm {
        Some(value) => {
            command.env("COLORTERM", value);
        }
        None => {
            command.env_remove("COLORTERM");
        }
    }
    for key in CHILD_ENV_STRIP {
        command.env_remove(*key);
    }
    for key in CHILD_ENV_COLOR_STRIP {
        command.env_remove(*key);
    }
}

/// A child process connected to a native pseudo-terminal.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Output reader; `None` after [`take_reader`] (one-shot).
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    /// True after a successful `wait` / `try_wait` / kill+wait so Drop must not
    /// signal a recycled PID (dual-sign).
    reaped: bool,
}

/// Parse OSC 0/2 title params. Semicolons in the title are rejoined.
fn parse_osc_title(params: &[&[u8]]) -> Option<String> {
    let parts = params.get(1..).unwrap_or_default();
    let mut bytes = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            bytes.push(b';');
        }
        bytes.extend_from_slice(part);
        if bytes.len() > MAX_TITLE_BYTES {
            return None;
        }
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return None;
    }
    Some(text.to_string())
}

/// Parse OSC 7 payload into a local path.
///
/// Accepts `file://hostname/abs/path`, `file:///abs/path`, or a bare absolute
/// path. Percent-encoding is decoded. Non-local schemes are ignored.
pub fn parse_osc7_cwd(uri: &[u8]) -> Option<PathBuf> {
    let raw = std::str::from_utf8(uri).ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let path_part = if let Some(rest) = raw.strip_prefix("file://") {
        if rest.starts_with('/') {
            rest
        } else {
            // file://hostname/path → skip host
            rest.find('/').map(|i| &rest[i..])?
        }
    } else if raw.starts_with('/') {
        raw
    } else {
        return None;
    };
    let decoded = percent_decode_path(path_part)?;
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode_path(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = (bytes[i + 1] as char).to_digit(16)?;
                let l = (bytes[i + 2] as char).to_digit(16)?;
                out.push(((h << 4) | l) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

impl PtySession {
    pub fn spawn<I, S>(program: &str, arguments: I, size: PtySize) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect::<Vec<_>>();
        Self::spawn_config(program, &arguments, None, &BTreeMap::new(), size)
    }

    /// Spawn a child from a structured argv/cwd/environment specification.
    ///
    /// This is the server-side counterpart to the mux control protocol's
    /// `SpawnSpec`: no shell command string is parsed or evaluated.
    pub fn spawn_config<S>(
        program: &str,
        arguments: &[S],
        cwd: Option<&Path>,
        environment: &BTreeMap<String, String>,
        size: PtySize,
    ) -> Result<Self>
    where
        S: AsRef<OsStr>,
    {
        let pair = native_pty_system().openpty(size)?;
        let mut command = CommandBuilder::new(program);
        for argument in arguments {
            command.arg(argument);
        }
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        }
        // Drop inherited discovery keys. The mux stamps the authoritative
        // values into `environment`. Host in-process PTYs omit them.
        command.env_remove("PRISMATTYC_PANE_ID");
        command.env_remove("PMUX_SOCKET");
        command.env_remove("PRISMATTYC_SESSION_ID");
        command.env_remove("PMUX_AGENT");
        command.env_remove("PMUX_TUTORIAL_PACK");
        for (key, value) in environment {
            command.env(key, value);
        }
        // ADR-0017 req 4: cells must not inherit the operator control socket path.
        command.env_remove("HIVE_SOCKET");
        apply_child_term_env(&mut command);
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        Ok(Self {
            master: pair.master,
            child,
            reader: Some(reader),
            writer: Some(writer),
            reaped: false,
        })
    }

    pub fn read_output(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self.reader.as_mut() {
            Some(reader) => reader.read(buffer),
            // Second path after take_reader: fail loudly, not silent empty EOF.
            None => Err(std::io::Error::other("PTY output reader was already taken")),
        }
    }

    /// Take the slave output reader for a dedicated reader thread (one-shot).
    ///
    /// A second call returns an error rather than an empty cursor that looks like
    /// a clean EOF.
    pub fn take_reader(&mut self) -> Result<Box<dyn Read + Send>> {
        self.reader
            .take()
            .ok_or_else(|| anyhow!("PTY output reader was already taken"))
    }

    pub fn take_input_writer(&mut self) -> Result<Box<dyn Write + Send>> {
        self.writer
            .take()
            .ok_or_else(|| anyhow!("PTY input writer was already taken"))
    }

    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.master.resize(size)
    }

    /// Operating-system child PID when the backend exposes one.
    ///
    /// This is diagnostic metadata for local process-lifetime proofs, never a
    /// stable mux identity or a control target.
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn wait(&mut self) -> Result<portable_pty::ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }

    /// Non-blocking reap probe. Returns `Ok(None)` if the child is still running
    /// or was already reaped. On first successful status, marks the session reaped
    /// so Drop will not dual-signal.
    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        if self.reaped {
            return Ok(None);
        }
        match self.child.try_wait()? {
            Some(status) => {
                self.reaped = true;
                Ok(Some(status))
            }
            None => Ok(None),
        }
    }

    /// Best-effort terminate + reap the child.
    ///
    /// Idempotent: after a successful wait/`try_wait`, or a prior kill+wait,
    /// does **not** call `kill` (avoids signalling a recycled PID on Unix when
    /// Drop runs after the normal EOF path already reaped the child).
    pub fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        // Child already exited but not yet marked (race / prior wait path).
        if let Ok(Some(_)) = self.child.try_wait() {
            self.reaped = true;
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Only kill if not already reaped (wait-then-Drop must not
        // signal a recycled PID).
        self.kill_and_reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::Cursor;
    use std::sync::Mutex;

    static TERMINFO_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// True if `pid` still exists. Linux uses `/proc`; other Unix uses `kill -0`.
    #[cfg(unix)]
    fn test_pid_alive(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }
        #[cfg(not(target_os = "linux"))]
        {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }
    }

    fn row_text(emulator: &Emulator, row: usize) -> String {
        emulator
            .screen()
            .row(row)
            .unwrap()
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    #[test]
    fn feed_bel_sets_pending_bell() {
        let mut emulator = Emulator::new(8, 1, 0);
        assert!(!emulator.take_pending_bell());
        let _ = emulator.feed(b"hi\x07there");
        assert!(emulator.take_pending_bell());
        assert!(!emulator.take_pending_bell());
    }

    #[test]
    fn parses_text_cursor_motion_and_erasure() {
        let mut emulator = Emulator::new(8, 2, 10);
        let _ = emulator.feed(b"hello\r\nworld\x1b[2D\x1b[K");
        assert_eq!(row_text(&emulator, 0), "hello   ");
        assert_eq!(row_text(&emulator, 1), "wor     ");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 1, column: 3 });
    }

    #[test]
    fn parses_basic_sgr_attributes_and_colors() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[1;31;44mX\x1b[0mY");
        let row = emulator.screen().row(0).unwrap();
        assert!(row[0].style.bold);
        assert_eq!(row[0].style.foreground, Color::Ansi(1));
        assert_eq!(row[0].style.background, Color::Ansi(4));
        assert_eq!(row[1].style, Style::default());
    }

    #[test]
    fn parses_256_and_truecolor_sgr() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38;5;196mA\x1b[48;2;10;20;30mB");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(row[0].style.foreground, Color::Indexed(196));
        assert_eq!(
            row[1].style.background,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        );
    }

    #[test]
    fn parses_truecolor_sgr_colon_subparams() {
        // ISO/xterm with colorspace Pi=0: CSI 38:2:0:R:G:B m
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38:2:0:10:20:30mA");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            },
            "ISO colon truecolor with colorspace Pi=0"
        );

        // Empty colorspace 38:2::R:G:B — VTE flattens empty subparam as 0 → [38,2,0,r,g,b]
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38:2::255:0:0mB");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb { r: 255, g: 0, b: 0 },
            "colon empty colorspace flattens to Pi=0 then RGB"
        );

        // Colon without colorspace (exactly 3 components after mode 2)
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38:2:1:2:3mC");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb { r: 1, g: 2, b: 3 },
            "colon truecolor without colorspace"
        );

        // Semicolon form must keep working
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38;2;1;2;3mD");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb { r: 1, g: 2, b: 3 },
            "semicolon truecolor 38;2;r;g;b"
        );

        // Incomplete RGB must not reinterpret leftovers as bold/red
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[0m\x1b[38;2;1;31mE");
        let row = emulator.screen().row(0).unwrap();
        assert!(
            !row[0].style.bold,
            "truncated truecolor must not apply bold"
        );
        assert_eq!(
            row[0].style.foreground,
            Color::Default,
            "truncated 38;2;R;G must not apply R/G as SGR colors"
        );

        // Background ISO form with colorspace
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[48:2:0:10:20:30mF");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.background,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            },
            "ISO colon truecolor background with colorspace"
        );
    }

    #[test]
    fn truecolor_semicolon_then_bold_does_not_consume_trailing_sgr_as_rgb() {
        // Classic: CSI 38;2;10;20;30;1 m  → RGB(10,20,30) + bold
        // Bug if flat-tail ≥4 treats as Pi:R:G:B → RGB(20,30,1), no bold.
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38;2;10;20;30;1mX");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            },
            "semicolon RGB must not treat trailing SGR as colorspace RGB"
        );
        assert!(row[0].style.bold, "trailing SGR 1 must still apply bold");
    }

    #[test]
    fn truecolor_semicolon_fg_then_bg_chain() {
        // CSI 38;2;10;20;30;48;2;1;2;3 m → fg RGB(10,20,30) and bg RGB(1,2,3)
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38;2;10;20;30;48;2;1;2;3mX");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            },
            "semicolon fg truecolor in fg+bg chain"
        );
        assert_eq!(
            row[0].style.background,
            Color::Rgb { r: 1, g: 2, b: 3 },
            "semicolon bg truecolor in fg+bg chain"
        );
    }

    #[test]
    fn underline_color_sgr_58_and_59() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[58:2:1:2:3mA");
        assert_eq!(
            emulator.screen().row(0).unwrap()[0].style.underline_color,
            Color::Rgb { r: 1, g: 2, b: 3 }
        );
        let _ = emulator.feed(b"\x1b[0m\x1b[58;5;9mB");
        assert_eq!(
            emulator.screen().row(0).unwrap()[1].style.underline_color,
            Color::Indexed(9)
        );
        let _ = emulator.feed(b"\x1b[59mC");
        assert_eq!(
            emulator.screen().row(0).unwrap()[2].style.underline_color,
            Color::Default
        );
        let _ = emulator.feed(b"\x1b[0m\x1b[58;2;4;5;6mD");
        assert_eq!(
            emulator.screen().row(0).unwrap()[3].style.underline_color,
            Color::Rgb { r: 4, g: 5, b: 6 }
        );
    }

    #[test]
    fn underline_style_sgr_variants_and_resets() {
        let mut emulator = Emulator::new(8, 1, 0);
        let _ = emulator.feed(b"\x1b[4mA\x1b[4:2mB\x1b[4:3mC\x1b[4:4mD\x1b[4:5mE\x1b[4:0mF");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(row[0].style.underline_style, UnderlineStyle::Single);
        assert_eq!(row[1].style.underline_style, UnderlineStyle::Double);
        assert_eq!(row[2].style.underline_style, UnderlineStyle::Curly);
        assert_eq!(row[3].style.underline_style, UnderlineStyle::Dotted);
        assert_eq!(row[4].style.underline_style, UnderlineStyle::Dashed);
        assert_eq!(row[5].style.underline_style, UnderlineStyle::None);
        assert!(!row[5].style.underline);

        let _ = emulator.feed(b"\x1b[4:3mG\x1b[24mH");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(row[6].style.underline_style, UnderlineStyle::Curly);
        assert_eq!(row[7].style.underline_style, UnderlineStyle::None);
        assert!(!row[7].style.underline);
    }

    #[test]
    fn truecolor_colon_with_trailing_bold() {
        // Colon without Pi + trailing bold: CSI 38:2:1:2:3;1 m
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38:2:1:2:3;1mX");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb { r: 1, g: 2, b: 3 },
            "colon no-Pi RGB with trailing bold"
        );
        assert!(row[0].style.bold, "trailing SGR 1 after colon truecolor");

        // ISO with Pi + trailing bold: CSI 38:2:0:10:20:30;1 m
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b[38:2:0:10:20:30;1mY");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(
            row[0].style.foreground,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            },
            "ISO Pi colon RGB with trailing bold"
        );
        assert!(row[0].style.bold, "trailing SGR 1 after ISO Pi truecolor");
    }

    #[test]
    fn bracketed_paste_mode_tracks_decset_2004() {
        let mut emulator = Emulator::new(4, 1, 0);
        assert!(!emulator.bracketed_paste());
        let _ = emulator.feed(b"\x1b[?2004h");
        assert!(emulator.bracketed_paste());
        let _ = emulator.feed(b"\x1b[?2004l");
        assert!(!emulator.bracketed_paste());
    }

    /// mouse input: DECSET 1000/1002/1003/1006 are stored; highest tracking wins.
    #[test]
    fn app_mouse_private_modes_are_tracked() {
        let mut emulator = Emulator::new(40, 10, 0);
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Off);
        assert!(!emulator.mouse_sgr());

        let _ = emulator.feed(b"\x1b[?1000h");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Click);

        let _ = emulator.feed(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Drag);
        assert!(emulator.mouse_sgr());

        let _ = emulator.feed(b"\x1b[?1003h");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Any);

        // Clearing only 1003 demotes to Drag while 1002 remains.
        let _ = emulator.feed(b"\x1b[?1003l");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Drag);

        let _ = emulator.feed(b"\x1b[?1002l\x1b[?1000l\x1b[?1006l");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Off);
        assert!(!emulator.mouse_sgr());

        let _ = emulator.feed(b"\x1b[?7700h\x1b[?1006h");
        assert!(emulator.mouse_wheel_only());
        assert!(emulator.reports_app_wheel());
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Off);
        let _ = emulator.feed(b"\x1b[?7700l");
        assert!(!emulator.mouse_wheel_only());
        assert!(!emulator.reports_app_wheel());

        // 1015 accepted as no-op encoding; must not break paste mode.
        let _ = emulator.feed(b"\x1b[?1015h\x1b[?2004h");
        assert!(emulator.bracketed_paste());
        assert!(
            emulator.take_pending_replies().is_empty(),
            "mouse modes must not enqueue DSR/mouse reports"
        );
        let _ = emulator.feed(b"ok");
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(row[0].character, 'o');
        assert_eq!(row[1].character, 'k');
    }

    #[test]
    fn ris_clears_mouse_tracking_modes() {
        let mut emulator = Emulator::new(4, 2, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?7700h");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Drag);
        assert!(emulator.mouse_sgr());
        assert!(emulator.mouse_wheel_only());
        let _ = emulator.feed(b"\x1bc");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Off);
        assert!(!emulator.mouse_sgr());
        assert!(!emulator.mouse_wheel_only());
    }

    #[test]
    fn ris_restores_cursor_visible() {
        let mut emulator = Emulator::new(4, 2, 0);
        let _ = emulator.feed(b"\x1b[?25l");
        assert!(!emulator.cursor_visible());
        let _ = emulator.feed(b"\x1bc");
        assert!(
            emulator.cursor_visible(),
            "RIS must restore DECTCEM default visible"
        );
    }

    #[test]
    fn focus_report_mode_tracks_decset_1004() {
        let mut emulator = Emulator::new(4, 2, 0);
        assert!(!emulator.focus_report());
        let _ = emulator.feed(b"\x1b[?1004h");
        assert!(emulator.focus_report());
        let _ = emulator.feed(b"\x1b[?1004l");
        assert!(!emulator.focus_report());
        let _ = emulator.feed(b"\x1b[?1004h\x1bc");
        assert!(!emulator.focus_report(), "RIS must clear focus reporting");
    }

    #[test]
    fn kitty_keyboard_push_pop_set_and_query() {
        let mut emulator = Emulator::new(4, 2, 0);
        assert_eq!(emulator.keyboard_flags(), 0);
        // Quickstart: CSI > 1 u → push 0, set disambiguate
        let _ = emulator.feed(b"\x1b[>1u");
        assert_eq!(emulator.keyboard_flags(), KITTY_DISAMBIGUATE);
        // Query
        let _ = emulator.feed(b"\x1b[?u");
        assert_eq!(emulator.take_pending_replies(), vec![b"\x1b[?1u".to_vec()]);
        // Set replace to report-all | disambiguate
        let _ = emulator.feed(b"\x1b[=9u");
        assert_eq!(
            emulator.keyboard_flags(),
            KITTY_DISAMBIGUATE | KITTY_REPORT_ALL
        );
        // Mode 2: set event-types bit without clearing others
        let _ = emulator.feed(b"\x1b[=2;2u");
        assert_eq!(
            emulator.keyboard_flags(),
            KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES | KITTY_REPORT_ALL
        );
        // Pop → back to 0 (what was under the push)
        let _ = emulator.feed(b"\x1b[<u");
        assert_eq!(emulator.keyboard_flags(), 0);
    }

    #[test]
    fn kitty_keyboard_alt_screen_has_independent_stack() {
        let mut emulator = Emulator::new(4, 2, 0);
        let _ = emulator.feed(b"\x1b[>1u");
        assert_eq!(emulator.keyboard_flags(), KITTY_DISAMBIGUATE);
        let _ = emulator.feed(b"\x1b[?1049h");
        assert_eq!(emulator.keyboard_flags(), 0, "alt stack starts clear");
        let _ = emulator.feed(b"\x1b[>8u");
        assert_eq!(emulator.keyboard_flags(), KITTY_REPORT_ALL);
        let _ = emulator.feed(b"\x1b[?1049l");
        assert_eq!(
            emulator.keyboard_flags(),
            KITTY_DISAMBIGUATE,
            "primary stack preserved across alt"
        );
    }

    #[test]
    fn ris_clears_kitty_keyboard_flags() {
        let mut emulator = Emulator::new(4, 2, 0);
        let _ = emulator.feed(b"\x1b[>1u\x1bc");
        assert_eq!(emulator.keyboard_flags(), 0);
    }

    #[test]
    fn decom_private_mode_tracks_and_affects_cup_cpr() {
        let mut emulator = Emulator::new(10, 6, 0);
        assert!(!emulator.screen().origin_mode());
        // DECSTBM rows 2..4 (1-based), then DECOM on, CUP 1;1 → absolute row 2.
        let _ = emulator.feed(b"\x1b[2;4r\x1b[?6h\x1b[1;1H");
        assert!(emulator.screen().origin_mode());
        assert_eq!(
            emulator.screen().cursor(),
            prismattyc_core::Cursor { row: 1, column: 0 }
        );
        let _ = emulator.feed(b"\x1b[6n");
        let replies = emulator.take_pending_replies();
        assert_eq!(replies, vec![b"\x1b[1;1R".to_vec()], "CPR origin-relative");
        let _ = emulator.feed(b"\x1b[?6l\x1b[1;1H");
        assert!(!emulator.screen().origin_mode());
        assert_eq!(
            emulator.screen().cursor(),
            prismattyc_core::Cursor { row: 0, column: 0 }
        );
    }

    #[test]
    fn soft_reset_preserves_mouse_tracking_modes() {
        let mut emulator = Emulator::new(4, 2, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let _ = emulator.feed(b"\x1b[!p");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Click);
        assert!(emulator.mouse_sgr());
    }

    // claim polish: DECSTR restores DECOM/DECAWM/DECTCEM/focus defaults
    /// but keeps app mouse (editors soft-reset mid-session).
    #[test]
    fn soft_reset_restores_claimed_modes_keeps_mouse() {
        let mut emulator = Emulator::new(10, 6, 0);
        let _ = emulator.feed(
            b"\x1b[2;4r\x1b[?6h\x1b[?7l\x1b[?25l\x1b[?1004h\x1b[?1000h\x1b[?1006h\x1b[?2004h",
        );
        assert!(emulator.screen().origin_mode());
        assert!(!emulator.screen().autowrap());
        assert!(!emulator.cursor_visible());
        assert!(emulator.focus_report());
        assert!(emulator.bracketed_paste());
        let _ = emulator.feed(b"\x1b[!p");
        assert!(!emulator.screen().origin_mode(), "DECSTR clears DECOM");
        assert!(emulator.screen().autowrap(), "DECSTR restores DECAWM");
        assert!(emulator.cursor_visible(), "DECSTR restores DECTCEM");
        assert!(!emulator.focus_report(), "DECSTR clears focus report");
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Click);
        assert!(
            emulator.mouse_sgr(),
            "DECSTR keeps mouse (mouse input soft path)"
        );
        assert!(
            emulator.bracketed_paste(),
            "DECSTR keeps bracketed paste (RIS clears it)"
        );
    }

    /// RIS clears every claimed private mode that apps leave stuck after TUIs.
    #[test]
    fn ris_clears_claimed_private_modes() {
        let mut emulator = Emulator::new(10, 6, 20);
        let _ = emulator.feed(
            b"\x1b[2;4r\x1b[?6h\x1b[?7l\x1b[?25l\x1b[?1004h\x1b[?1000h\x1b[?1006h\x1b[?2004hMAIN\r\n",
        );
        assert!(emulator.screen().origin_mode());
        assert!(!emulator.screen().autowrap());
        assert!(!emulator.cursor_visible());
        assert!(emulator.focus_report());
        assert!(emulator.bracketed_paste());
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Click);
        let _ = emulator.feed(b"\x1bc");
        assert!(!emulator.screen().origin_mode());
        assert!(emulator.screen().autowrap());
        assert!(emulator.cursor_visible());
        assert!(!emulator.focus_report());
        assert!(!emulator.bracketed_paste());
        assert_eq!(emulator.mouse_tracking(), MouseTracking::Off);
        assert!(!emulator.mouse_sgr());
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 0 });
    }

    #[test]
    fn cursor_visibility_tracks_dectcem_25() {
        let mut emulator = Emulator::new(4, 1, 0);
        assert!(emulator.cursor_visible(), "DECTCEM defaults to visible");
        let _ = emulator.feed(b"\x1b[?25l");
        assert!(!emulator.cursor_visible());
        let _ = emulator.feed(b"\x1b[?25h");
        assert!(emulator.cursor_visible());
        // Combined private-mode params still track 25.
        let _ = emulator.feed(b"\x1b[?25;2004l");
        assert!(!emulator.cursor_visible());
        assert!(!emulator.bracketed_paste());
    }

    #[test]
    fn unsupported_sequences_do_not_leak_payload_text() {
        let mut emulator = Emulator::new(12, 1, 0);
        let _ = emulator.feed(b"left\x1b]0;title\x07right");
        assert_eq!(row_text(&emulator, 0), "leftright   ");
    }

    #[test]
    fn osc_zero_and_two_set_pending_title() {
        let mut emulator = Emulator::new(12, 1, 0);
        let _ = emulator.feed(b"\x1b]0;hello;world\x07");
        assert_eq!(
            emulator.take_pending_title().as_deref(),
            Some("hello;world")
        );
        assert!(emulator.take_pending_title().is_none());
        let _ = emulator.feed(b"\x1b]2;tab\x1b\\");
        assert_eq!(emulator.take_pending_title().as_deref(), Some("tab"));
        let _ = emulator.feed(b"\x1b]0;\x07");
        assert_eq!(emulator.take_pending_title().as_deref(), Some(""));
    }

    #[test]
    fn classic_path_does_not_collect_apc() {
        let mut emulator = Emulator::new(16, 1, 0);
        assert!(!emulator.collects_apc());
        let events = emulator.feed(b"left\x1b_Prismattyc;cap;q;id=1;max=0.1\x1b\\right");
        assert!(events.is_empty());
        // VTE swallows APC; classic grid only sees surrounding text.
        assert_eq!(row_text(&emulator, 0), "leftright       ");
    }

    #[test]
    fn experimental_apc_bodies_are_collected_without_leaking_into_the_grid() {
        let mut emulator = Emulator::new_experimental(16, 1, 0);
        assert!(emulator.collects_apc());
        let events = emulator.feed(b"left\x1b_Prismattyc;cap;q;id=1;max=0.1\x1b\\right");
        assert_eq!(row_text(&emulator, 0), "leftright       ");
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], CollectedApc::Body(body) if body.starts_with("Prismattyc;cap;q"))
        );
    }

    #[test]
    fn alternate_screen_switches_buffers() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"MAIN");
        let _ = emulator.feed(b"\x1b[?1049h");
        assert!(emulator.screen().alt_active());
        let _ = emulator.feed(b"ALT!");
        assert_eq!(row_text(&emulator, 0), "ALT!");
        let _ = emulator.feed(b"\x1b[?1049l");
        assert!(!emulator.screen().alt_active());
        assert_eq!(row_text(&emulator, 0), "MAIN");
    }

    #[test]
    fn decstbm_is_accepted_without_leaking() {
        let mut emulator = Emulator::new(4, 4, 0);
        let _ = emulator.feed(b"\x1b[2;3r\x1b[2;1Habcd");
        let joined: String = (0..4).map(|r| row_text(&emulator, r)).collect();
        assert!(!joined.contains('['));
        assert!(joined.contains('a') || joined.contains('b'));
    }

    #[test]
    fn esc_m_reverse_index_scrolls_decstbm_region_down() {
        // Fill rows 0..=3 with a/b/c/d, set region rows 1..=2, cursor at top margin, ESC M.
        let mut emulator = Emulator::new(1, 4, 0);
        let _ = emulator.feed(b"a\r\nb\r\nc\r\nd");
        let _ = emulator.feed(b"\x1b[2;3r"); // DECSTBM rows 2..3 (1-based)
        let _ = emulator.feed(b"\x1b[2;1H"); // cursor to top of region
        let _ = emulator.feed(b"\x1bM"); // RI
        assert_eq!(row_text(&emulator, 0), "a");
        assert_eq!(row_text(&emulator, 1), " "); // blank at top of region
        assert_eq!(row_text(&emulator, 2), "b");
        assert_eq!(row_text(&emulator, 3), "d"); // below region preserved
        assert_eq!(emulator.screen().cursor().row, 1);
    }

    #[test]
    fn csi_d_vpa_with_cha_positions_write() {
        // terminfo VPA+CHA: CSI 10 d + CSI 5 G → zero-based (9, 4).
        let mut emulator = Emulator::new(10, 12, 0);
        let _ = emulator.feed(b"\x1b[10d\x1b[5G*");
        assert_eq!(
            emulator.screen().cursor(),
            Cursor { row: 9, column: 5 },
            "after write, cursor advances one column from (9,4)"
        );
        assert_eq!(
            emulator.screen().row(9).unwrap()[4].character,
            '*',
            "VPA+CHA should place glyph at (9,4)"
        );
    }

    #[test]
    fn csi_il_dl_within_decstbm() {
        // 1-col × 4-row grid: fill a/b/c/d, DECSTBM rows 2–3, IL then DL.
        let mut emulator = Emulator::new(1, 4, 0);
        let _ = emulator.feed(b"a\r\nb\r\nc\r\nd");
        // DECSTBM 2;3, cursor to row 2 col 1, insert one line.
        let _ = emulator.feed(b"\x1b[2;3r\x1b[2;1H\x1b[L");
        assert_eq!(row_text(&emulator, 0), "a");
        assert_eq!(row_text(&emulator, 1), " ");
        assert_eq!(row_text(&emulator, 2), "b");
        assert_eq!(row_text(&emulator, 3), "d");
        // Delete the blank line at cursor (row 2): region becomes b + blank.
        let _ = emulator.feed(b"\x1b[M");
        assert_eq!(row_text(&emulator, 0), "a");
        assert_eq!(row_text(&emulator, 1), "b");
        assert_eq!(row_text(&emulator, 2), " ");
        assert_eq!(row_text(&emulator, 3), "d");
    }

    #[test]
    fn csi_il_param_zero_defaults_to_one() {
        let mut emulator = Emulator::new(1, 3, 0);
        let _ = emulator.feed(b"a\r\nb\r\nc\x1b[1;1H\x1b[0L");
        assert_eq!(row_text(&emulator, 0), " ");
        assert_eq!(row_text(&emulator, 1), "a");
        assert_eq!(row_text(&emulator, 2), "b");
    }

    #[test]
    fn csi_dl_blanks_use_current_sgr() {
        let mut emulator = Emulator::new(2, 3, 0);
        let _ = emulator.feed(b"aa\r\nbb\r\ncc\x1b[1;1H\x1b[1;41m\x1b[M");
        assert_eq!(row_text(&emulator, 0), "bb");
        assert_eq!(row_text(&emulator, 1), "cc");
        assert_eq!(row_text(&emulator, 2), "  ");
        let cell = &emulator.screen().row(2).unwrap()[0];
        assert!(cell.style.bold);
        assert_eq!(cell.style.background, Color::Ansi(1));
    }

    #[test]
    fn csi_su_sd_within_decstbm() {
        // 1-col × 4-row: a/b/c/d, DECSTBM 2;3, CSI S then CSI T.
        let mut emulator = Emulator::new(1, 4, 0);
        let _ = emulator.feed(b"a\r\nb\r\nc\r\nd");
        let _ = emulator.feed(b"\x1b[2;3r"); // DECSTBM rows 2..3 (1-based)
        let _ = emulator.feed(b"\x1b[S"); // SU 1
        assert_eq!(row_text(&emulator, 0), "a");
        assert_eq!(row_text(&emulator, 1), "c");
        assert_eq!(row_text(&emulator, 2), " ");
        assert_eq!(row_text(&emulator, 3), "d");
        let _ = emulator.feed(b"\x1b[T"); // SD 1 — undo region scroll
        assert_eq!(row_text(&emulator, 0), "a");
        assert_eq!(row_text(&emulator, 1), " ");
        assert_eq!(row_text(&emulator, 2), "c");
        assert_eq!(row_text(&emulator, 3), "d");
    }

    #[test]
    fn csi_su_count_and_sd_default() {
        let mut emulator = Emulator::new(1, 5, 0);
        let _ = emulator.feed(b"a\r\nb\r\nc\r\nd\r\ne");
        // Full screen; SU 2 lines.
        let _ = emulator.feed(b"\x1b[2S");
        assert_eq!(row_text(&emulator, 0), "c");
        assert_eq!(row_text(&emulator, 1), "d");
        assert_eq!(row_text(&emulator, 2), "e");
        assert_eq!(row_text(&emulator, 3), " ");
        assert_eq!(row_text(&emulator, 4), " ");
        // Bare SD (default 1).
        let _ = emulator.feed(b"\x1b[T");
        assert_eq!(row_text(&emulator, 0), " ");
        assert_eq!(row_text(&emulator, 1), "c");
        assert_eq!(row_text(&emulator, 2), "d");
        assert_eq!(row_text(&emulator, 3), "e");
        assert_eq!(row_text(&emulator, 4), " ");
    }

    #[test]
    fn csi_t_multi_param_is_not_sd() {
        // xterm highlight-mouse form CSI Ps;Ps;Ps;Ps;Ps T must not scroll.
        let mut emulator = Emulator::new(1, 3, 0);
        let _ = emulator.feed(b"a\r\nb\r\nc");
        let before: Vec<String> = (0..3).map(|r| row_text(&emulator, r)).collect();
        let _ = emulator.feed(b"\x1b[1;2;3;4;5T");
        let after: Vec<String> = (0..3).map(|r| row_text(&emulator, r)).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn csi_ich_dch_ech() {
        // ICH (CSI @): insert blanks at cursor, shift right (right edge discarded).
        // 6-col: "abcdef", CUP 1;3 → col index 2, insert 1 → "ab cde".
        let mut emulator = Emulator::new(6, 1, 0);
        let _ = emulator.feed(b"abcdef\x1b[1;3H\x1b[@");
        assert_eq!(row_text(&emulator, 0), "ab cde");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 2 });

        // DCH (CSI P): delete at cursor, shift left, blank tail.
        let mut emulator = Emulator::new(6, 1, 0);
        let _ = emulator.feed(b"abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(row_text(&emulator, 0), "adef  ");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 1 });

        // ECH (CSI X): erase without shifting.
        let mut emulator = Emulator::new(6, 1, 0);
        let _ = emulator.feed(b"abcdef\x1b[1;3H\x1b[2X");
        assert_eq!(row_text(&emulator, 0), "ab  ef");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 2 });

        // Default param is 1 (CSI @ with no number).
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"wxyz\x1b[1;2H\x1b[@");
        assert_eq!(row_text(&emulator, 0), "w xy");
    }

    #[test]
    fn csi_3_j_clears_scrollback_via_erase_display() {
        // CSI 3 J maps to erase_display(3): viewport + scrollback.
        let mut emulator = Emulator::new(2, 1, 4);
        let _ = emulator.feed(b"abcdef");
        assert_eq!(emulator.screen().scrollback().len(), 2);
        assert_eq!(row_text(&emulator, 0), "ef");
        let epoch = emulator.screen().content_epoch();
        let _ = emulator.feed(b"\x1b[3J");
        assert_eq!(row_text(&emulator, 0), "  ");
        assert!(emulator.screen().scrollback().is_empty());
        assert!(emulator.screen().content_epoch() > epoch);
        // CSI 2 J clears viewport only (rebuild scrollback first from home).
        let _ = emulator.feed(b"\x1b[Habcdef");
        assert_eq!(emulator.screen().scrollback().len(), 2);
        let _ = emulator.feed(b"\x1b[2J");
        assert_eq!(row_text(&emulator, 0), "  ");
        assert_eq!(
            emulator.screen().scrollback().len(),
            2,
            "CSI 2 J must leave scrollback intact"
        );
    }

    #[test]
    fn take_damage_after_key_newline_and_clear() {
        let mut emulator = Emulator::new(4, 3, 10);
        let _ = emulator.take_damage();
        let _ = emulator.feed(b"x");
        let key = emulator.take_damage();
        assert!(key.dirty_cell_count() <= 2);
        let _ = emulator.feed(b"\x1b[3;1Hxxxx");
        let _ = emulator.take_damage();
        let _ = emulator.feed(b"\n");
        let scroll = emulator.take_damage();
        assert_eq!(scroll.scroll_events().len(), 1);
        assert_eq!(scroll.scroll_events()[0].delta, 1);
        let _ = emulator.feed(b"\x1b[2J");
        let clear = emulator.take_damage();
        assert_eq!(clear.dirty_row_count(), 3);
    }

    #[test]
    fn soft_reset_csi_bang_p_resets_style_region_wrap_keeps_grid() {
        // DECSTR: CSI ! p — keep glyphs; clear SGR, DECSTBM, pending wrap.
        let mut emulator = Emulator::new(4, 3, 0);
        let _ = emulator.feed(b"ABCD\x1b[2;3r\x1b[1;31m\x1b[2;2H");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 1, column: 1 });
        assert_ne!(emulator.screen().style(), Style::default());
        let _ = emulator.feed(b"\x1b[!p");
        assert_eq!(emulator.screen().style(), Style::default());
        assert_eq!(
            emulator.screen().cursor(),
            Cursor { row: 1, column: 1 },
            "soft reset preserves cursor"
        );
        assert_eq!(row_text(&emulator, 0), "ABCD");
        // Full region again: LF at bottom scrolls entire screen.
        let _ = emulator.feed(b"\x1b[3;1H\n");
        assert_eq!(row_text(&emulator, 0).chars().next(), Some(' '));
    }

    #[test]
    fn ris_esc_c_leaves_alt_clears_modes_display_and_scrollback() {
        // RIS: leave 1049 alt, home, ED3 (viewport+scrollback), default SGR, clear paste.
        let mut emulator = Emulator::new(4, 2, 20);
        let _ = emulator.feed(b"MAIN\r\nLINE\r\nMORE\r\n");
        assert!(!emulator.screen().scrollback().is_empty());
        let _ = emulator.feed(b"\x1b[?1049h\x1b[1;33mALT!\x1b[?2004h\x1b[1;2r");
        assert!(emulator.screen().alt_active());
        assert!(emulator.bracketed_paste());
        assert_ne!(emulator.screen().style(), Style::default());
        let _ = emulator.feed(b"\x1bc");
        assert!(!emulator.screen().alt_active());
        assert!(!emulator.bracketed_paste());
        assert_eq!(emulator.screen().style(), Style::default());
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 0 });
        // Primary restored then erased; scrollback wiped (RIS subset).
        assert_eq!(row_text(&emulator, 0), "    ");
        assert!(
            emulator.screen().scrollback().is_empty(),
            "RIS must clear primary scrollback"
        );
        assert_eq!(row_text(&emulator, 1), "    ");
    }

    #[test]
    fn ris_esc_c_on_primary_erases_without_alt() {
        let mut emulator = Emulator::new(4, 2, 0);
        let _ = emulator.feed(b"\x1b[1;41mTEXT\x1b[?2004h\x1bc");
        assert!(!emulator.bracketed_paste());
        assert_eq!(emulator.screen().style(), Style::default());
        assert_eq!(row_text(&emulator, 0), "    ");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 0 });
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut emulator = Emulator::new(8, 4, 0);
        let _ = emulator.feed(b"hello");
        emulator.resize(4, 2);
        assert_eq!(emulator.screen().columns(), 4);
        assert_eq!(emulator.screen().rows(), 2);
        assert!(row_text(&emulator, 0).starts_with("hell"));
    }

    #[test]
    fn csi_6_n_queues_cursor_position_report() {
        // Cursor at (0,0) → CPR `\x1b[1;1R`. No real PTY: host drains pending_replies.
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[6n");
        let replies = emulator.take_pending_replies();
        assert_eq!(replies, vec![b"\x1b[1;1R".to_vec()]);
        assert!(emulator.take_pending_replies().is_empty());
    }

    #[test]
    fn csi_6_n_reports_current_cursor_after_cup() {
        let mut emulator = Emulator::new(80, 24, 0);
        // CUP row 3 col 5 (1-based) → zero-based (2, 4) → CPR 3;5
        let _ = emulator.feed(b"\x1b[3;5H\x1b[6n");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 2, column: 4 });
        let replies = emulator.take_pending_replies();
        assert_eq!(replies, vec![b"\x1b[3;5R".to_vec()]);
    }

    #[test]
    fn csi_5_n_queues_device_status_ok() {
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[5n");
        assert_eq!(
            emulator.take_pending_replies(),
            vec![b"\x1b[0n".to_vec()],
            "CSI 5 n → status ready"
        );
    }

    #[test]
    fn pty_size_with_cell_pixels_is_window_not_per_cell() {
        let size = pty_size_with_cell_pixels(80, 24, 10, 23);
        assert_eq!(size.cols, 80);
        assert_eq!(size.rows, 24);
        assert_eq!(size.pixel_width, 800);
        assert_eq!(size.pixel_height, 552);
        let tiny = pty_size_with_cell_pixels(80, 24, 0, 0);
        assert_eq!(tiny.pixel_width, 80);
        assert_eq!(tiny.pixel_height, 24);
    }

    #[test]
    fn csi_xtwinops_matches_ghostty_size_report() {
        // Ghostty size_report.zig: 14 t window pixels (h,w), 16 t cell (h,w), 18 t cells.
        let mut emulator = Emulator::new(80, 24, 0);
        emulator.set_cell_pixels(10, 23);
        let _ = emulator.feed(b"\x1b[14t\x1b[16t\x1b[18t");
        let replies = emulator.take_pending_replies();
        assert_eq!(
            replies,
            vec![
                b"\x1b[4;552;800t".to_vec(),
                b"\x1b[6;23;10t".to_vec(),
                b"\x1b[8;24;80t".to_vec(),
            ]
        );
        // Unknown XTWINOPS is a no-op (not SD — that is CSI T).
        let _ = emulator.feed(b"\x1b[13t");
        assert!(emulator.take_pending_replies().is_empty());
    }

    /// SCO CSI s / CSI u share the DECSC/DECRC saved slot (incl. SGR).
    #[test]
    fn sco_csi_s_u_save_restore_cursor_and_sgr() {
        let mut emulator = Emulator::new(40, 10, 0);
        // Bold, CUP (3,5) 1-based → (2,4), save with CSI s.
        let _ = emulator.feed(b"\x1b[1m\x1b[3;5H\x1b[s");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 2, column: 4 });
        assert!(emulator.screen().style().bold);
        // Move + clear bold, then SCO restore.
        let _ = emulator.feed(b"\x1b[0m\x1b[8;10H\x1b[u");
        assert_eq!(
            emulator.screen().cursor(),
            Cursor { row: 2, column: 4 },
            "CSI u restores SCOSC position"
        );
        assert!(
            emulator.screen().style().bold,
            "CSI u restores SGR like DECRC"
        );
        // Round-trip with DECSC slot: CSI s then ESC 8 should restore same.
        let _ = emulator.feed(b"\x1b[1;1H\x1b[0m\x1b[s\x1b[5;6H\x1b8");
        assert_eq!(emulator.screen().cursor(), Cursor { row: 0, column: 0 });
    }

    #[test]
    fn csi_unknown_dsr_does_not_queue_reply() {
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[0n\x1b[n");
        assert!(
            emulator.take_pending_replies().is_empty(),
            "unknown DSR params stay silent"
        );
    }

    /// grok 1.0.5 blocks on DA1. Reply VT100+AVO (`CSI ? 1;2 c`).
    #[test]
    fn csi_c_queues_da1_vt100_avo() {
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[c");
        assert_eq!(
            emulator.take_pending_replies(),
            vec![b"\x1b[?1;2c".to_vec()]
        );
        assert!(emulator.take_pending_replies().is_empty());
    }

    #[test]
    fn csi_0_c_queues_same_da1_as_bare_c() {
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[0c");
        assert_eq!(
            emulator.take_pending_replies(),
            vec![b"\x1b[?1;2c".to_vec()]
        );
    }

    #[test]
    fn decscusr_sets_cursor_shape_and_ris_restores_block() {
        let mut emulator = Emulator::new(80, 24, 0);
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[4 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Underline);
        let _ = emulator.feed(b"\x1b[6 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Bar);
        let _ = emulator.feed(b"\x1b[0 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[4 q\x1bc");
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[6 q\x1b[!p");
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[6 q\x1b[7 q");
        assert_eq!(
            emulator.cursor_shape(),
            CursorShape::Bar,
            "unknown DECSCUSR Ps leaves the current shape"
        );
        let _ = emulator.feed(b"\x1b[ q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[1 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[2 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Block);
        let _ = emulator.feed(b"\x1b[3 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Underline);
        let _ = emulator.feed(b"\x1b[5 q");
        assert_eq!(emulator.cursor_shape(), CursorShape::Bar);
        assert_eq!(CursorShape::Block.decscusr_steady_bytes(), b"\x1b[2 q");
        assert_eq!(CursorShape::Underline.decscusr_steady_bytes(), b"\x1b[4 q");
        assert_eq!(CursorShape::Bar.decscusr_steady_bytes(), b"\x1b[6 q");
    }

    #[test]
    fn csi_1_c_does_not_queue_da1() {
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b[1c");
        assert!(
            emulator.take_pending_replies().is_empty(),
            "xterm answers DA1 only for Ps=0 / omitted"
        );
    }

    #[test]
    fn graphics_query_then_da1_both_reply() {
        // Kitty graphics clients send a=q then CSI c and wait for DA1.
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c");
        let replies = emulator.take_pending_replies();
        assert_eq!(
            replies,
            vec![b"\x1b_Gi=31;OK\x1b\\".to_vec(), b"\x1b[?1;2c".to_vec()],
            "graphics OK must precede DA1 so clients do not time out"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_captures_child_output() {
        let mut session = PtySession::spawn(
            "/bin/sh",
            ["-c", "printf 'pty-ok\\n'"],
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .unwrap();
        let mut buffer = [0_u8; 128];
        let count = session.read_output(&mut buffer).unwrap();
        assert!(String::from_utf8_lossy(&buffer[..count]).contains("pty-ok"));
        session.wait().unwrap();
    }

    /// second take_reader must error (not silent empty EOF cursor).
    #[cfg(unix)]
    #[test]
    fn take_reader_is_one_shot() {
        let mut session = PtySession::spawn(
            "/bin/sh",
            ["-c", "printf x; sleep 0.05"],
            PtySize {
                rows: 8,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .unwrap();
        let mut r1 = session.take_reader().expect("first take");
        match session.take_reader() {
            Ok(_) => panic!("second take_reader must fail"),
            Err(err) => assert!(err.to_string().contains("already taken"), "got {err}"),
        }
        let mut buf = [0u8; 8];
        let _ = r1.read(&mut buf);
        let read_err = session
            .read_output(&mut buf)
            .expect_err("read after take must fail");
        assert_eq!(read_err.kind(), std::io::ErrorKind::Other);
        session.wait().ok();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_resize_updates_child_winsize() {
        // Child prints TIOCGWINSZ rows/cols after a short wait so the host can resize first.
        let mut session = PtySession::spawn(
            "/bin/sh",
            [
                "-c",
                "sleep 0.2; stty size 2>/dev/null || true; printf 'done\\n'",
            ],
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .unwrap();
        session
            .resize(PtySize {
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut buffer = [0_u8; 256];
        let mut collected = String::new();
        loop {
            match session.read_output(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    collected.push_str(&String::from_utf8_lossy(&buffer[..n]));
                    if collected.contains("done") {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("pty read: {error}"),
            }
        }
        let _ = session.wait();
        // `stty size` prints "rows cols".
        assert!(
            collected.contains("40 100") || collected.contains("40\t100"),
            "expected child winsize 40x100, got {collected:?}"
        );
    }

    /// Drop/kill_and_reap must terminate an abandoned child (no zombies).
    #[cfg(unix)]
    #[test]
    fn pty_session_drop_kills_and_reaps_child() {
        let size = PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut session = PtySession::spawn("sleep", ["60"], size).expect("spawn sleep");
        let pid = session.child.process_id().expect("child pid");
        // Explicit reap path (same as Drop).
        session.kill_and_reap();
        assert!(session.reaped);
        assert!(
            !test_pid_alive(pid),
            "child pid {pid} should be gone after kill_and_reap"
        );
        // Second call is idempotent (no panic).
        session.kill_and_reap();
    }

    // dual-sign: successful wait then Drop must not call kill again.
    #[cfg(unix)]
    #[test]
    fn pty_session_wait_then_drop_does_not_re_kill() {
        let size = PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut session = PtySession::spawn("true", [] as [&str; 0], size).expect("spawn true");
        let _status = session.wait().expect("wait");
        assert!(session.reaped, "wait must mark reaped");
        // kill_and_reap / Drop must be no-ops — would race PID reuse if kill ran.
        session.kill_and_reap();
        assert!(session.reaped);
        drop(session);
    }

    #[test]
    fn apply_child_term_env_forces_prism_identity() {
        let _guard = TERMINFO_ENV_LOCK.lock().expect("lock");
        let old_color = std::env::var_os("PRISMATTYC_COLOR");
        unsafe {
            std::env::remove_var("PRISMATTYC_COLOR");
        }
        let mut command = CommandBuilder::new("/bin/sh");
        // Simulate an outer host that would otherwise leak into the child.
        command.env("TERM", "xterm-kitty");
        command.env("TERM_PROGRAM", "ghostty");
        command.env("COLORTERM", "noforce");
        command.env("KITTY_WINDOW_ID", "42");
        command.env("VTE_VERSION", "7600");
        command.env("TERM_PROGRAM_VERSION", "1.2.3");

        apply_child_term_env(&mut command);

        let expected_term = effective_child_term();
        assert_eq!(command.get_env("TERM"), Some(OsStr::new(expected_term)));
        if expected_term == CHILD_TERM {
            assert!(
                command.get_env("TERMINFO").is_some(),
                "bundled terminfo must set TERMINFO for prismattyc-kitty"
            );
        }
        assert_eq!(
            command.get_env("TERM_PROGRAM"),
            Some(OsStr::new(CHILD_TERM_PROGRAM))
        );
        assert_eq!(
            command.get_env("COLORTERM"),
            Some(OsStr::new(CHILD_COLORTERM))
        );
        // Prismattyc advertises its own Kitty-graphics capability marker, overriding
        // any outer value, so producers (e.g. Claude Code) emit real graphics.
        assert_eq!(
            command.get_env("KITTY_WINDOW_ID"),
            Some(OsStr::new(CHILD_KITTY_WINDOW_ID))
        );
        assert_eq!(command.get_env("VTE_VERSION"), None);
        assert_eq!(command.get_env("TERM_PROGRAM_VERSION"), None);
        unsafe {
            match old_color {
                Some(v) => std::env::set_var("PRISMATTYC_COLOR", v),
                None => std::env::remove_var("PRISMATTYC_COLOR"),
            }
        }
    }

    #[test]
    fn apply_child_term_env_strips_inherited_color_suppressors() {
        let mut command = CommandBuilder::new("/bin/sh");
        // Simulate a daemon started from an agent-sandboxed shell.
        command.env("NO_COLOR", "1");
        command.env("CLICOLOR", "0");
        command.env("CLICOLOR_FORCE", "0");
        command.env("FORCE_COLOR", "0");
        command.env("PIP_NO_COLOR", "1");
        command.env("NPM_CONFIG_COLOR", "false");
        command.env("CARGO_TERM_COLOR", "never");

        apply_child_term_env(&mut command);

        for key in [
            "NO_COLOR",
            "CLICOLOR",
            "CLICOLOR_FORCE",
            "FORCE_COLOR",
            "PIP_NO_COLOR",
            "NPM_CONFIG_COLOR",
            "CARGO_TERM_COLOR",
        ] {
            assert_eq!(command.get_env(key), None, "{key} must not reach the child");
        }
    }

    #[test]
    fn child_color_depth_parses_and_defaults_to_truecolor() {
        assert_eq!(ChildColorDepth::parse(None), ChildColorDepth::Truecolor);
        assert_eq!(ChildColorDepth::parse(Some("")), ChildColorDepth::Truecolor);
        assert_eq!(
            ChildColorDepth::parse(Some("direct")),
            ChildColorDepth::Truecolor
        );
        assert_eq!(
            ChildColorDepth::parse(Some("256")),
            ChildColorDepth::Indexed256
        );
        assert_eq!(
            ChildColorDepth::parse(Some("ansi")),
            ChildColorDepth::Indexed16
        );
        assert_eq!(
            ChildColorDepth::parse(Some("nope")),
            ChildColorDepth::Truecolor
        );
    }

    #[test]
    fn prism_color_env_selects_256_and_16() {
        let _guard = TERMINFO_ENV_LOCK.lock().expect("lock");
        let old = std::env::var_os("PRISMATTYC_COLOR");
        let restore = || unsafe {
            match &old {
                Some(v) => std::env::set_var("PRISMATTYC_COLOR", v),
                None => std::env::remove_var("PRISMATTYC_COLOR"),
            }
        };
        unsafe {
            std::env::set_var("PRISMATTYC_COLOR", "256");
        }
        let mut command = CommandBuilder::new("/bin/sh");
        apply_child_term_env(&mut command);
        assert_eq!(command.get_env("TERM"), Some(OsStr::new(CHILD_TERM_256)));
        assert_eq!(
            command.get_env("COLORTERM"),
            Some(OsStr::new(CHILD_COLORTERM)),
            "256 mode keeps COLORTERM"
        );
        unsafe {
            std::env::set_var("PRISMATTYC_COLOR", "16");
        }
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("COLORTERM", "leftover");
        apply_child_term_env(&mut command);
        assert_eq!(command.get_env("TERM"), Some(OsStr::new(CHILD_TERM_16)));
        assert_eq!(command.get_env("COLORTERM"), None);
        restore();
    }

    #[test]
    fn bundled_prism_terminfo_resolves_in_workspace() {
        let _guard = TERMINFO_ENV_LOCK.lock().expect("lock");
        let old_color = std::env::var_os("PRISMATTYC_COLOR");
        unsafe {
            std::env::remove_var("PRISMATTYC_COLOR");
        }
        let dir = resolve_child_terminfo_dir().expect("baked terminfo must resolve");
        assert!(
            dir.join("p").join(CHILD_TERM).is_file(),
            "compiled {CHILD_TERM} missing under {dir:?}"
        );
        assert!(
            dir.join("p").join("prismattyc-direct").is_file(),
            "compiled prismattyc-direct alias missing under {dir:?}"
        );
        assert!(
            dir.join("p").join("prismattyc-256color").is_file(),
            "compiled prismattyc-256color missing under {dir:?}"
        );
        let kitty = std::fs::read(dir.join("p").join(CHILD_TERM)).expect("read kitty terminfo");
        assert_eq!(
            &kitty[..2],
            super::TERMINFO_MAGIC_EXTENDED,
            "letter-dir {CHILD_TERM} must be 32-bit extended so extra caps load"
        );
        assert_eq!(effective_child_term(), CHILD_TERM);
        assert!(
            CHILD_TERM.contains("kitty"),
            "truecolor TERM must contain kitty so producers enable graphics"
        );
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            !dir.starts_with(manifest),
            "TERMINFO must not point at the crate build directory: {dir:?}"
        );
        unsafe {
            match old_color {
                Some(v) => std::env::set_var("PRISMATTYC_COLOR", v),
                None => std::env::remove_var("PRISMATTYC_COLOR"),
            }
        }
    }

    #[test]
    fn materialize_bundled_terminfo_writes_missing_and_refreshes_stale() {
        let root = std::env::temp_dir().join(format!(
            "prism-pm102-materialize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let dest = root.join("p").join(CHILD_TERM);
        let dest_hex = root.join("70").join(CHILD_TERM);
        assert!(!dest.exists());
        materialize_bundled_terminfo(&root).expect("materialize missing");
        for name in [
            "prismattyc-kitty",
            "prismattyc-direct",
            "prismattyc-256color",
            "prismattyc-16color",
        ] {
            assert!(
                root.join("p").join(name).is_file(),
                "missing baked letter entry {name}"
            );
            assert!(
                root.join("70").join(name).is_file(),
                "missing baked hex entry {name}"
            );
        }
        let first = std::fs::read(&dest).expect("read dest");
        assert_eq!(first, super::BUNDLED_PRISMATTYC_KITTY_EXTENDED);
        assert_eq!(&first[..2], super::TERMINFO_MAGIC_EXTENDED);
        let hex = std::fs::read(&dest_hex).expect("read hex");
        assert_eq!(hex, super::BUNDLED_PRISMATTYC_KITTY_LEGACY);
        assert_eq!(&hex[..2], super::TERMINFO_MAGIC_LEGACY);
        assert_ne!(
            first, hex,
            "letter dir must be 32-bit extras; hex dir is 16-bit Apple ncurses"
        );
        std::fs::write(&dest, b"stale").expect("stale");
        materialize_bundled_terminfo(&root).expect("refresh stale");
        let refreshed = std::fs::read(&dest).expect("read refreshed");
        assert_eq!(refreshed, super::BUNDLED_PRISMATTYC_KITTY_EXTENDED);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_child_terminfo_prefers_prism_terminfo_then_xdg() {
        let _guard = TERMINFO_ENV_LOCK.lock().expect("lock");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let override_root = std::env::temp_dir().join(format!(
            "prism-pm102-override-{}-{stamp}",
            std::process::id()
        ));
        let xdg =
            std::env::temp_dir().join(format!("prism-pm102-xdg-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(override_root.join("p")).unwrap();
        std::fs::write(
            override_root.join("p").join("prismattyc-256color"),
            b"override",
        )
        .unwrap();
        let old_override = std::env::var_os("PRISMATTYC_TERMINFO");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: lock serializes env mutation in this test process.
        unsafe {
            std::env::set_var("PRISMATTYC_TERMINFO", &override_root);
            std::env::set_var("XDG_DATA_HOME", &xdg);
        }
        let resolved = resolve_child_terminfo_dir().expect("override must resolve");
        assert_eq!(
            resolved.canonicalize().unwrap(),
            override_root.canonicalize().unwrap()
        );
        unsafe {
            std::env::remove_var("PRISMATTYC_TERMINFO");
        }
        let via_xdg = resolve_child_terminfo_dir().expect("xdg materialize must resolve");
        assert!(
            via_xdg.starts_with(&xdg)
                || via_xdg
                    .canonicalize()
                    .unwrap()
                    .starts_with(xdg.canonicalize().unwrap()),
            "expected XDG path, got {via_xdg:?}"
        );
        assert!(via_xdg.join("p").join("prismattyc-256color").is_file());
        // An unusable user cache must not prevent a child from finding the
        // installed or bundled terminal description.
        let blocked = xdg.join("not-a-directory");
        std::fs::write(&blocked, b"blocked").unwrap();
        unsafe {
            std::env::set_var("PRISMATTYC_TERMINFO", &blocked);
            std::env::set_var("XDG_DATA_HOME", &blocked);
        }
        let fallback = resolve_child_terminfo_dir().expect("unwritable cache must fall back");
        assert!(super::has_terminfo_entry(&fallback));
        assert!(!fallback.starts_with(&blocked));
        unsafe {
            match old_override {
                Some(v) => std::env::set_var("PRISMATTYC_TERMINFO", v),
                None => std::env::remove_var("PRISMATTYC_TERMINFO"),
            }
            match old_xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&override_root);
        let _ = std::fs::remove_dir_all(&xdg);
    }

    #[cfg(unix)]
    #[test]
    fn spawned_child_terminfo_uses_stable_xdg_path() {
        let _guard = TERMINFO_ENV_LOCK.lock().expect("lock");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let xdg = std::env::temp_dir().join(format!(
            "prism-pm102-live-xdg-{}-{stamp}",
            std::process::id()
        ));
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let old_override = std::env::var_os("PRISMATTYC_TERMINFO");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &xdg);
            std::env::remove_var("PRISMATTYC_TERMINFO");
        }
        let result = (|| {
            let mut session = PtySession::spawn(
                "/bin/sh",
                ["-c", "printf 'TERMINFO=%s\\n' \"${TERMINFO-}\""],
                PtySize {
                    rows: 8,
                    cols: 40,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            )?;
            let mut buffer = [0_u8; 256];
            let mut collected = String::new();
            loop {
                match session.read_output(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        collected.push_str(&String::from_utf8_lossy(&buffer[..n]));
                        if collected.contains("TERMINFO=") {
                            break;
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(error) => return Err(anyhow!("pty read: {error}")),
                }
            }
            let _ = session.wait();
            Ok::<_, anyhow::Error>(collected)
        })();
        unsafe {
            match old_xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match old_override {
                Some(v) => std::env::set_var("PRISMATTYC_TERMINFO", v),
                None => std::env::remove_var("PRISMATTYC_TERMINFO"),
            }
        }
        let expected = xdg.join("prismattyc").join("terminfo");
        let collected = result.expect("spawn/read TERMINFO");
        assert!(
            collected.contains(&format!("TERMINFO={}", expected.display()))
                || expected
                    .canonicalize()
                    .ok()
                    .is_some_and(|c| collected.contains(&format!("TERMINFO={}", c.display()))),
            "child TERMINFO must be the stable XDG prismattyc/terminfo dir, got {collected:?}"
        );
        assert!(
            !collected.contains("/tmp/claude"),
            "child TERMINFO must not use a scratchpad: {collected:?}"
        );
        let _ = std::fs::remove_dir_all(&xdg);
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_child_sees_prism_term_not_outer() {
        // Even if the host process env looks like a richer outer terminal, the
        // child must observe Prismattyc's forced identity.
        let _guard = TERMINFO_ENV_LOCK.lock().expect("lock");
        let previous_prism_color = std::env::var_os("PRISMATTYC_COLOR");
        let previous_term = std::env::var_os("TERM");
        let previous_term_program = std::env::var_os("TERM_PROGRAM");
        let previous_colorterm = std::env::var_os("COLORTERM");
        let previous_kitty = std::env::var_os("KITTY_WINDOW_ID");
        // SAFETY: single-threaded test process; vars restored before return.
        unsafe {
            std::env::remove_var("PRISMATTYC_COLOR");
            std::env::set_var("TERM", "xterm-kitty");
            std::env::set_var("TERM_PROGRAM", "iTerm.app");
            std::env::set_var("COLORTERM", "outer-host");
            std::env::set_var("KITTY_WINDOW_ID", "99");
        }

        let result = (|| {
            let mut session = PtySession::spawn(
                "/bin/sh",
                [
                    "-c",
                    "printf 'TERM=%s\\nTERM_PROGRAM=%s\\nCOLORTERM=%s\\nKITTY=%s\\n' \
                     \"${TERM-}\" \"${TERM_PROGRAM-}\" \"${COLORTERM-}\" \"${KITTY_WINDOW_ID-}\"",
                ],
                PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            )?;
            let mut buffer = [0_u8; 256];
            let mut collected = String::new();
            loop {
                match session.read_output(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        collected.push_str(&String::from_utf8_lossy(&buffer[..n]));
                        if collected.contains("KITTY=") {
                            break;
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(error) => return Err(anyhow!("pty read: {error}")),
                }
            }
            let _ = session.wait();
            Ok::<_, anyhow::Error>(collected)
        })();

        // SAFETY: restore process env after the spawn test.
        unsafe {
            match previous_term {
                Some(v) => std::env::set_var("TERM", v),
                None => std::env::remove_var("TERM"),
            }
            match previous_term_program {
                Some(v) => std::env::set_var("TERM_PROGRAM", v),
                None => std::env::remove_var("TERM_PROGRAM"),
            }
            match previous_colorterm {
                Some(v) => std::env::set_var("COLORTERM", v),
                None => std::env::remove_var("COLORTERM"),
            }
            match previous_kitty {
                Some(v) => std::env::set_var("KITTY_WINDOW_ID", v),
                None => std::env::remove_var("KITTY_WINDOW_ID"),
            }
            match previous_prism_color {
                Some(v) => std::env::set_var("PRISMATTYC_COLOR", v),
                None => std::env::remove_var("PRISMATTYC_COLOR"),
            }
        }

        let collected = result.expect("spawn/read child env");
        assert!(
            collected.contains(&format!("TERM={CHILD_TERM}")),
            "child must see forced TERM={CHILD_TERM}, got {collected:?}"
        );
        assert!(
            collected.contains(&format!("TERM_PROGRAM={CHILD_TERM_PROGRAM}")),
            "child must see TERM_PROGRAM={CHILD_TERM_PROGRAM}, got {collected:?}"
        );
        assert!(
            collected.contains(&format!("COLORTERM={CHILD_COLORTERM}")),
            "child must see COLORTERM={CHILD_COLORTERM}, got {collected:?}"
        );
        assert!(
            collected.contains(&format!("KITTY={CHILD_KITTY_WINDOW_ID}")),
            "child must see Prismattyc's KITTY_WINDOW_ID={CHILD_KITTY_WINDOW_ID}, got {collected:?}"
        );
        assert!(
            !collected.contains("KITTY=99"),
            "outer KITTY_WINDOW_ID must not reach the child, got {collected:?}"
        );
        assert!(
            !collected.contains("xterm-kitty"),
            "outer TERM must not reach the child, got {collected:?}"
        );
    }

    #[test]
    fn parse_osc7_file_uri_and_bare_path() {
        assert_eq!(
            parse_osc7_cwd(b"file:///home/brandan/Hive"),
            Some(PathBuf::from("/home/brandan/Hive"))
        );
        assert_eq!(
            parse_osc7_cwd(b"file://localhost/tmp/foo%20bar"),
            Some(PathBuf::from("/tmp/foo bar"))
        );
        assert_eq!(parse_osc7_cwd(b"/var/tmp"), Some(PathBuf::from("/var/tmp")));
        assert!(parse_osc7_cwd(b"http://example.com/x").is_none());
        assert!(parse_osc7_cwd(b"").is_none());
    }

    #[test]
    fn feed_osc_attention_protocols_and_latest_wins() {
        let mut emulator = Emulator::new(40, 10, 100);
        let _ = emulator.feed(b"\x1b]9;permission needed\x07");
        let _ = emulator.feed(b"\x1b]777;notify;Claude;needs input\x1b\\");
        let _ = emulator.feed(b"\x1b]99;i=1;question for you\x1b\\");
        assert_eq!(
            emulator.take_pending_attention().as_deref(),
            Some("question for you")
        );
        assert!(emulator.take_pending_attention().is_none());
    }

    #[test]
    fn feed_osc99_ignores_incomplete_chunks() {
        let mut emulator = Emulator::new(40, 10, 100);
        let _ = emulator.feed(b"\x1b]99;i=1;m=1;partial\x1b\\");
        assert!(emulator.take_pending_attention().is_none());
        let _ = emulator.feed(b"\x1b]99;i=1;complete\x1b\\");
        assert_eq!(
            emulator.take_pending_attention().as_deref(),
            Some("complete")
        );
    }

    #[test]
    fn feed_osc_attention_rejects_oversized_non_utf8_and_controls() {
        let oversized = format!("\x1b]9;{}\x1b\\", "x".repeat(MAX_ATTENTION_BYTES + 1));
        let mut emulator = Emulator::new(40, 10, 100);
        let _ = emulator.feed(oversized.as_bytes());
        assert!(emulator.take_pending_attention().is_none());

        let mut emulator = Emulator::new(40, 10, 100);
        let _ = emulator.feed(b"\x1b]9;\xff\x1b\\");
        assert!(emulator.take_pending_attention().is_none());

        assert!(validated_attention(b"bad\nmessage").is_none());
        assert!(validated_attention(b"bad\tmessage").is_none());
    }

    #[test]
    fn bell_termination_does_not_create_attention() {
        let mut emulator = Emulator::new(40, 10, 100);
        let _ = emulator.feed(b"\x1b]9;needs input\x07");
        assert_eq!(
            emulator.take_pending_attention().as_deref(),
            Some("needs input")
        );
        assert!(!emulator.take_pending_bell());

        let _ = emulator.feed(b"\x07");
        assert!(emulator.take_pending_attention().is_none());
        assert!(emulator.take_pending_bell());
    }

    #[test]
    fn feed_osc7_updates_emulator_cwd() {
        let mut emulator = Emulator::new(40, 10, 100);
        assert!(emulator.cwd().is_none());
        // BEL-terminated OSC 7
        let _ = emulator.feed(b"\x1b]7;file:///home/user/project\x07");
        assert_eq!(emulator.cwd(), Some(Path::new("/home/user/project")));
        // ST-terminated update
        let _ = emulator.feed(b"\x1b]7;file:///tmp\x1b\\");
        assert_eq!(emulator.cwd(), Some(Path::new("/tmp")));
    }

    #[test]
    fn feed_osc8_marks_cells_until_close_and_crosses_wrap() {
        let mut emulator = Emulator::new(4, 2, 10);
        let _ = emulator
            .feed(b"\x1b]8;id=manual;https://example.com/manual\x1b\\abcdef\x1b]8;;\x1b\\x");
        for (row, col) in [(0, 0), (0, 3), (1, 0), (1, 1)] {
            assert_eq!(
                emulator.screen().hyperlink_uri_at_view(0, row, col),
                Some("https://example.com/manual"),
                "linked cell at {row},{col}"
            );
        }
        assert_eq!(emulator.screen().hyperlink_uri_at_view(0, 1, 2), None);
    }

    #[test]
    fn feed_osc8_preserves_semicolons_and_accepts_bel_termination() {
        let mut emulator = Emulator::new(8, 1, 0);
        let _ = emulator.feed(b"\x1b]8;foo=bar:id=semi;https://example.com/a;b\x07L\x1b]8;;\x07");
        assert_eq!(
            emulator.screen().hyperlink_uri_at_view(0, 0, 0),
            Some("https://example.com/a;b")
        );
    }

    #[test]
    fn malformed_osc8_clears_the_previous_active_target() {
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b"\x1b]8;;https://example.com\x1b\\a");
        let _ = emulator.feed(b"\x1b]8;only-params\x1b\\b");
        assert!(emulator.screen().row(0).unwrap()[0]
            .hyperlink_id()
            .is_some());
        assert_eq!(emulator.screen().row(0).unwrap()[1].hyperlink_id(), None);
    }

    #[test]
    fn feed_renders_kitty_query_reply_in_classic_mode() {
        // Classic (non-experimental) emulator must still answer the graphics query.
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        let replies = emulator.take_pending_replies();
        assert!(
            replies.iter().any(|r| r == b"\x1b_Gi=31;OK\x1b\\"),
            "expected graphics OK reply, got {replies:?}"
        );
    }

    #[test]
    fn feed_stores_inline_png_image() {
        const RED_1X1_PNG_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        let mut emulator = Emulator::new(80, 24, 0);
        let seq = format!("\x1b_Ga=T,t=d,f=100,i=7;{RED_1X1_PNG_B64}\x1b\\");
        let _ = emulator.feed(seq.as_bytes());
        assert_eq!(emulator.images().len(), 1);
        assert_eq!(emulator.images()[0].id, 7);
    }

    #[test]
    fn feed_unicode_virtual_placement_and_placeholder_cells() {
        const RED_1X1_PNG_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        let mut emulator = Emulator::new(4, 2, 0);
        let transmit = format!("\x1b_Ga=t,t=d,f=100,i=7;{RED_1X1_PNG_B64}\x1b\\");
        let _ = emulator.feed(transmit.as_bytes());
        let _ = emulator.feed(b"\x1b_Ga=p,U=1,i=7,c=2,r=1\x1b\\");
        let cells =
            format!("\x1b[38:2:0:0:7m{KITTY_PLACEHOLDER}\u{0305}\u{0305}{KITTY_PLACEHOLDER}");
        let _ = emulator.feed(cells.as_bytes());
        assert!(emulator.images().is_empty());
        assert_eq!(emulator.image_by_id(7).map(|i| i.width), Some(1));
        let v = emulator.virtual_placement(7, 0).expect("virtual");
        assert_eq!((v.cols, v.rows), (2, 1));
        let row = emulator.screen().row(0).unwrap();
        assert_eq!(row[0].character, KITTY_PLACEHOLDER);
        assert_eq!(
            emulator.screen().view_cell(0, 0, 0).combining_marks(),
            &['\u{0305}', '\u{0305}']
        );
        assert_eq!(row[1].character, KITTY_PLACEHOLDER);
        assert!(emulator
            .screen()
            .view_cell(0, 0, 1)
            .combining_marks()
            .is_empty());
        assert_eq!(row[0].style.foreground, Color::Rgb { r: 0, g: 0, b: 7 });
    }

    #[test]
    fn resize_invalidates_images() {
        const B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(format!("\x1b_Ga=T,t=d,f=100,i=1;{B64}\x1b\\").as_bytes());
        assert_eq!(emulator.images().len(), 1);
        emulator.resize(100, 30);
        assert!(
            emulator.images().is_empty(),
            "resize must invalidate images"
        );
    }

    #[test]
    fn full_clear_invalidates_images() {
        const B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        let mut emulator = Emulator::new(80, 24, 0);
        let _ = emulator.feed(format!("\x1b_Ga=T,t=d,f=100,i=1;{B64}\x1b\\").as_bytes());
        let _ = emulator.feed(b"\x1b[2J");
        assert!(emulator.images().is_empty());
    }
}
