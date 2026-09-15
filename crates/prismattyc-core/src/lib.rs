//! Shared classic-terminal screen model for Prismattyc.

mod cell_grid;
#[cfg(test)]
mod cluster_tests;
mod clusters;
mod damage;
mod history_budget;
#[cfg(test)]
mod history_budget_tests;
mod reflow;
#[cfg(test)]
mod resize_tests;
mod test_time;
mod version;

pub mod splash;

pub use damage::{GridDamage, ScrollDamage};
pub use test_time::{parse_test_time_scale, test_time_budget, test_time_scale};
pub use version::{bin_version, git_hash, git_suffix, package_version, release_label};

use std::collections::VecDeque;

use cell_grid::CellGrid;
use clusters::ClusterStore;

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

/// Display columns for a single Unicode scalar (ADR-0004).
///
/// Uses Unicode East Asian Width via `unicode-width`. Returns `0` for
/// non-spacing / combining marks, `1` for narrow, `2` for wide. Values above 2
/// are clamped to 2.
///
/// **Placement** may treat some non-zero-width scalars as cluster extenders
/// (emoji modifiers, ZWJ continuations) so they do not advance the cursor —
/// see [`Screen::put_char`].
pub fn char_display_width(character: char) -> usize {
    match character.width() {
        None | Some(0) => 0,
        Some(1) => 1,
        Some(_) => 2,
    }
}

/// Display columns occupied by `text` (sum of [`char_display_width`]).
pub fn line_display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

/// Visit each scalar with its lead cell column. Combining marks reuse the
/// previous base column and do not advance.
pub fn for_each_display_scalar(text: &str, mut visit: impl FnMut(usize, char, usize)) {
    let mut col = 0usize;
    let mut last_col = 0usize;
    for ch in text.chars() {
        let width = char_display_width(ch);
        let at = if width == 0 { last_col } else { col };
        visit(at, ch, width);
        if width > 0 {
            last_col = col;
            col = col.saturating_add(width);
        }
    }
}

/// U+200D ZERO WIDTH JOINER — bridges emoji bases into one cluster.
pub const ZWJ: char = '\u{200d}';

/// Fitzpatrick emoji skin-tone modifiers (U+1F3FB..=U+1F3FF).
///
/// `unicode-width` reports these as width 2, but terminals attach them to the
/// preceding emoji base without advancing (ADR-0004 ZWJ / grapheme slice).
#[inline]
pub const fn is_emoji_modifier(c: char) -> bool {
    matches!(c, '\u{1f3fb}'..='\u{1f3ff}')
}

/// Regional Indicator symbols (U+1F1E6..=U+1F1FF) used for flag pairs.
#[inline]
pub const fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1f1e6}'..='\u{1f1ff}')
}

#[inline]
pub const fn is_zwj(c: char) -> bool {
    c == ZWJ
}

/// Return the display width of one serialized grapheme cluster.
///
/// The terminal treats regional-indicator pairs as one wide glyph and treats
/// U+FE0F as the wide emoji presentation selector for a narrow base. Keep this
/// rule shared by live placement and state import.
pub fn grapheme_display_width(character: char, combining: &[char]) -> usize {
    let width = char_display_width(character);
    if width == 0 {
        return 0;
    }
    if is_regional_indicator(character)
        && combining
            .first()
            .is_some_and(|mark| is_regional_indicator(*mark))
    {
        return 2;
    }
    if width == 1 && combining.contains(&'\u{fe0f}') {
        return 2;
    }
    width
}

#[inline]
fn is_zwj_joinable(character: char) -> bool {
    char_display_width(character) > 0 && !character.is_whitespace()
}

/// A terminal color understood by the classic renderer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    /// One of the terminal's sixteen ANSI palette entries (SGR 30–37 / 90–97).
    Ansi(u8),
    /// 256-color palette index (SGR 38;5;n / 48;5;n), full 0–255 range.
    Indexed(u8),
    /// 24-bit RGB (SGR 38;2;r;g;b / 48;2;r;g;b).
    Rgb { r: u8, g: u8, b: u8 },
}

/// Underline rendition selected by SGR 4:x.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Rendition attributes attached to a cell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    /// Compatibility flag. It stays true whenever `underline_style` is not `None`.
    pub underline: bool,
    pub underline_style: UnderlineStyle,
    pub inverse: bool,
    pub foreground: Color,
    pub background: Color,
    /// SGR 58 underline color. Kitty uses this field as the Unicode-placeholder
    /// placement id; `Default` means the current foreground for underlines and 0
    /// for Kitty placement.
    pub underline_color: Color,
}

/// Max trailing scalars stored on one cell (combining marks + ZWJ cluster tail).
///
/// Sized for common emoji ZWJ sequences (e.g. family of four with skin tones)
/// without unbounded per-cell growth. Excess scalars are dropped.
pub const MAX_COMBINING_MARKS: usize = 12;

/// Maximum OSC 8 URI bytes retained for one hyperlink.
pub const MAX_HYPERLINK_URI_BYTES: usize = 2048;

/// Maximum OSC 8 `id` parameter bytes retained for one hyperlink.
pub const MAX_HYPERLINK_ID_BYTES: usize = 256;

/// Maximum distinct OSC 8 hyperlinks retained by one screen.
const MAX_HYPERLINKS: usize = 4096;

/// Stable handle from a painted cell into its screen's OSC 8 URI table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HyperlinkId(u32);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hyperlink {
    id: Option<String>,
    uri: String,
}

/// One printable cell in the terminal grid.
///
/// Copies and equality use screen-local handles. Use [`CellView`] to read
/// graphemes or compare cells from different screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub character: char,
    pub style: Style,
    /// Trailing half of a double-width glyph (ADR-0004). Not painted or extracted.
    pub wide_cont: bool,
    hyperlink: Option<HyperlinkId>,
    /// Handle into the owning screen. Zero means no trailing scalars.
    cluster: u32,
    /// First tail is RI, contains VS16, and ends with ZWJ, respectively.
    cluster_flags: u8,
    /// Synthetic gap before a wrapped wide glyph. Never copied as user text.
    wrap_padding: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            style: Style::default(),
            wide_cont: false,
            hyperlink: None,
            cluster: 0,
            cluster_flags: 0,
            wrap_padding: false,
        }
    }
}

impl Cell {
    /// Lead cell for a glyph (narrow or wide).
    pub fn glyph(character: char, style: Style) -> Self {
        Self {
            character,
            style,
            wide_cont: false,
            hyperlink: None,
            cluster: 0,
            cluster_flags: 0,
            wrap_padding: false,
        }
    }

    /// Continuation cell after a wide lead.
    pub fn wide_continuation(style: Style) -> Self {
        Self {
            character: ' ',
            style,
            wide_cont: true,
            hyperlink: None,
            cluster: 0,
            cluster_flags: 0,
            wrap_padding: false,
        }
    }

    /// OSC 8 hyperlink handle attached when this cell was painted.
    pub const fn hyperlink_id(&self) -> Option<HyperlinkId> {
        self.hyperlink
    }

    fn with_hyperlink(mut self, hyperlink: Option<HyperlinkId>) -> Self {
        self.hyperlink = hyperlink;
        self
    }

    /// Display columns occupied by this lead cell's complete grapheme.
    pub fn display_width(&self) -> usize {
        if self.wide_cont {
            0
        } else {
            let width = char_display_width(self.character);
            if (is_regional_indicator(self.character) && self.cluster_flags & 1 != 0)
                || (width == 1 && self.cluster_flags & 2 != 0)
            {
                2
            } else {
                width
            }
        }
    }

    /// True when the last trailing scalar is ZWJ.
    pub fn ends_with_zwj(&self) -> bool {
        self.cluster_flags & 4 != 0
    }
}

/// A cell borrowed together with its screen-owned grapheme storage.
///
/// Read text through this view. A copied [`Cell`] contains local handles only.
/// A view cannot outlive its screen or remain borrowed while the screen changes.
///
/// ```compile_fail
/// use prismattyc_core::Screen;
/// let mut screen = Screen::new(8, 2, 0);
/// let marks = screen.view_cell(0, 0, 0).combining_marks();
/// screen.resize(4, 1);
/// println!("{marks:?}");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct CellView<'a> {
    cell: &'a Cell,
    clusters: &'a ClusterStore,
}

impl std::ops::Deref for CellView<'_> {
    type Target = Cell;

    fn deref(&self) -> &Cell {
        self.cell
    }
}

impl PartialEq for CellView<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.character == other.character
            && self.style == other.style
            && self.wide_cont == other.wide_cont
            && self.hyperlink == other.hyperlink
            && self.combining_marks() == other.combining_marks()
    }
}

impl Eq for CellView<'_> {}

impl<'a> CellView<'a> {
    /// Trailing scalars attached to the base, in their original order.
    pub fn combining_marks(self) -> &'a [char] {
        self.clusters.get(self.cell.cluster)
    }

    /// Append the complete grapheme as UTF-8. Skip wide continuation cells.
    pub fn write_grapheme_into(self, out: &mut String) {
        if self.wide_cont || self.wrap_padding {
            return;
        }
        out.push(self.character);
        for &mark in self.combining_marks() {
            out.push(mark);
        }
    }
}

static BLANK_CELL: std::sync::LazyLock<Cell> = std::sync::LazyLock::new(Cell::default);

/// Zero-based cursor coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

/// DECSC / DECRC saved state: position, delayed wrap, and SGR pen.
///
/// xterm-class `ESC 7` / `ESC 8` and CSI `?1049` enter/leave share this bundle
/// (wrap attributes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SavedCursor {
    cursor: Cursor,
    wrap_pending: bool,
    style: Style,
}

/// Private-mode alternate screen variant (xterm CSI ? 47 / 1047 / 1049).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltScreenMode {
    /// CSI ? 47 — switch to alt without clearing existing alt content.
    Mode47,
    /// CSI ? 1047 — clear alt then switch (no primary cursor save).
    Mode1047,
    /// CSI ? 1049 — save primary cursor state (DECSC), clear alt, switch;
    /// restore on leave (DECRC).
    Mode1049,
}

/// A match in primary history / live grid from [`Screen::find_in_history`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryMatch {
    /// Absolute history row (oldest = 0).
    pub abs_row: usize,
    /// Inclusive start column.
    pub start_col: usize,
    /// Inclusive end column.
    pub end_col: usize,
}

/// Inclusive cell range on the visible grid (viewport coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRange {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
}

impl CellRange {
    /// Normalize so start is lexicographically ≤ end.
    pub fn normalized(self) -> Self {
        if (self.start_row, self.start_col) <= (self.end_row, self.end_col) {
            self
        } else {
            Self {
                start_row: self.end_row,
                start_col: self.end_col,
                end_row: self.start_row,
                end_col: self.start_col,
            }
        }
    }

    /// Whether a viewport cell is inside this range in stream (row-major) order.
    pub fn contains(self, row: usize, col: usize) -> bool {
        let r = self.normalized();
        if row < r.start_row || row > r.end_row {
            return false;
        }
        if r.start_row == r.end_row {
            return col >= r.start_col && col <= r.end_col;
        }
        if row == r.start_row {
            return col >= r.start_col;
        }
        if row == r.end_row {
            return col <= r.end_col;
        }
        true
    }
}

/// Active grid-native selection (Phase 1 / US-2). Does not mutate cell storage.
///
/// A plain click (down+up without moving) must **not** leave a sticky one-cell
/// highlight: [`Self::range`] is `None` until the pointer moves from the anchor
/// ([`Self::dragged`]), and the host clears on mouse-up when still not dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: Option<Cursor>,
    pub cursor: Option<Cursor>,
    pub active: bool,
    /// True once `update` moves the free end away from the anchor.
    pub dragged: bool,
}

impl Selection {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn begin(&mut self, row: usize, col: usize) {
        let point = Cursor { row, column: col };
        self.anchor = Some(point);
        self.cursor = Some(point);
        self.active = true;
        self.dragged = false;
    }

    pub fn update(&mut self, row: usize, col: usize) {
        if self.active {
            let point = Cursor { row, column: col };
            if self.anchor != Some(point) {
                self.dragged = true;
            }
            self.cursor = Some(point);
        }
    }

    pub fn finish(&mut self) {
        self.active = false;
    }

    /// Set a finished paintable range (word/line click or programmatic select).
    pub fn set_range(
        &mut self,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) {
        self.anchor = Some(Cursor {
            row: start_row,
            column: start_col,
        });
        self.cursor = Some(Cursor {
            row: end_row,
            column: end_col,
        });
        self.active = false;
        self.dragged = true;
    }

    pub fn range(&self) -> Option<CellRange> {
        if !self.dragged {
            return None;
        }
        let a = self.anchor?;
        let b = self.cursor?;
        Some(
            CellRange {
                start_row: a.row,
                start_col: a.column,
                end_row: b.row,
                end_col: b.column,
            }
            .normalized(),
        )
    }
}

/// Character class for double-click word selection (D-H2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Other,
}

fn char_class(c: char) -> CharClass {
    if c == ' ' || c == '\t' || c.is_whitespace() {
        CharClass::Space
    } else if c.is_ascii_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// Cell-window equality for history find. Case-insensitive compares use
/// Unicode lowercase (per scalar); column indices stay 1:1 with the grid.
fn history_window_eq(hay: &[char], needle: &[char], case_sensitive: bool) -> bool {
    if hay.len() != needle.len() {
        return false;
    }
    if case_sensitive {
        return hay == needle;
    }
    hay.iter()
        .zip(needle.iter())
        .all(|(a, b)| a == b || a.to_lowercase().eq(b.to_lowercase()))
}

/// One buffer's cells + cursor (primary or alternate).
#[derive(Debug, Clone, PartialEq, Eq)]
struct GridBuffer {
    cells: CellGrid,
    /// Per row: did this line end because autowrap ran out of columns rather
    /// than because a line feed was written? Copy joins a wrapped row to the
    /// next with no newline. `wrap_pending` below is transient cursor state
    /// and cannot answer this — it is cleared the moment the wrap executes.
    wrapped: Vec<bool>,
    cursor: Cursor,
    saved_cursor: SavedCursor,
    style: Style,
    wrap_pending: bool,
    /// Inclusive scroll region top row.
    scroll_top: usize,
    /// Inclusive scroll region bottom row.
    scroll_bottom: usize,
}

impl GridBuffer {
    fn eq_with(&self, other: &Self, a: &ClusterStore, b: &ClusterStore) -> bool {
        self.cells.len() == other.cells.len()
            && self.cells.columns() == other.cells.columns()
            && self
                .cells
                .chunks(self.cells.columns())
                .zip(other.cells.chunks(other.cells.columns()))
                .all(|(left, right)| cells_equal(left, right, a, b))
            && self.wrapped == other.wrapped
            && self.cursor == other.cursor
            && self.saved_cursor == other.saved_cursor
            && self.style == other.style
            && self.wrap_pending == other.wrap_pending
            && self.scroll_top == other.scroll_top
            && self.scroll_bottom == other.scroll_bottom
    }

    fn new(columns: usize, rows: usize) -> Self {
        Self {
            cells: CellGrid::from_flat(vec![Cell::default(); columns * rows], columns),
            wrapped: vec![false; rows],
            cursor: Cursor::default(),
            saved_cursor: SavedCursor::default(),
            style: Style::default(),
            wrap_pending: false,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
        }
    }
}

fn cells_equal(a: &[Cell], b: &[Cell], a_store: &ClusterStore, b_store: &ClusterStore) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            CellView {
                cell: a,
                clusters: a_store,
            } == CellView {
                cell: b,
                clusters: b_store,
            }
        })
}

/// A bounded primary terminal grid, optional alternate buffer, and scrollback.
///
/// [`PartialEq`] ignores [`GridDamage`]: damage is transient paint state, not
/// snapshot state (PT-242). [`ScreenStateV1`] also omits it. Import starts from
/// [`GridDamage::full`].
#[derive(Debug, Clone)]
pub struct Screen {
    columns: usize,
    rows: usize,
    primary: GridBuffer,
    clusters: ClusterStore,
    alt: Option<GridBuffer>,
    alt_active: bool,
    scrollback: VecDeque<Vec<Cell>>,
    /// Wrapped flag per `scrollback` line, same order and length. Kept in
    /// lockstep at the three sites that mutate `scrollback`.
    scrollback_wrapped: VecDeque<bool>,
    max_scrollback: usize,
    max_scrollback_bytes: usize,
    /// Diagnostic state. It does not affect logical equality or snapshots.
    scrollback_budget_warned: bool,
    /// Cumulative primary-grid scroll-up events. Used by cell-rect attachments.
    scrolled_lines: u64,
    /// Monotonic epoch for selection invalidation: any region scroll, alt
    /// enter/leave, or resize bumps this even when final alt_active is unchanged.
    content_epoch: u64,
    /// DECOM (CSI ? 6): CUP/VPA/etc. are relative to the scroll region; cursor
    /// is confined to the margins. Default off (absolute addressing).
    origin_mode: bool,
    /// DECAWM (CSI ? 7): auto-wrap when writing past the last column. Default on.
    autowrap: bool,
    /// Opt-in: rows scrolled off the top of the ALT screen also feed
    /// `scrollback`. Default off — classic hosts keep real-terminal semantics
    /// (alt never enters history). The mux server enables it so attach
    /// clients can page through TUI output via `history_view_cell`.
    retain_alt_history: bool,
    /// Count of full-viewport clears (ED mode 2/3). Used by callers (e.g. the
    /// emulator's Kitty-graphics store) to invalidate stale rasters without
    /// over-invalidating on every write, unlike `content_epoch`.
    full_clears: u64,
    /// OSC 8 target applied to subsequently painted cells.
    active_hyperlink: Option<HyperlinkId>,
    /// Bounded, append-only table. Handles remain valid while cells are in history.
    hyperlinks: Vec<Hyperlink>,
    /// Per-frame cell/row dirty bits and scroll events (PT-242).
    damage: GridDamage,
}

impl PartialEq for Screen {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.rows == other.rows
            && self
                .primary
                .eq_with(&other.primary, &self.clusters, &other.clusters)
            && match (&self.alt, &other.alt) {
                (Some(a), Some(b)) => a.eq_with(b, &self.clusters, &other.clusters),
                (None, None) => true,
                _ => false,
            }
            && self.alt_active == other.alt_active
            && self.scrollback.len() == other.scrollback.len()
            && self
                .scrollback
                .iter()
                .zip(&other.scrollback)
                .all(|(a, b)| cells_equal(a, b, &self.clusters, &other.clusters))
            && self.scrollback_wrapped == other.scrollback_wrapped
            && self.max_scrollback == other.max_scrollback
            && self.max_scrollback_bytes == other.max_scrollback_bytes
            && self.scrolled_lines == other.scrolled_lines
            && self.content_epoch == other.content_epoch
            && self.origin_mode == other.origin_mode
            && self.autowrap == other.autowrap
            && self.retain_alt_history == other.retain_alt_history
            && self.full_clears == other.full_clears
            && self.active_hyperlink == other.active_hyperlink
            && self.hyperlinks == other.hyperlinks
    }
}

impl Eq for Screen {}

/// Versioned, serialization-friendly representation of [`Screen`].
///
/// The DTO is separate from the runtime screen layout. Keep its fields stable
/// when changing the implementation, and bump the containing emulator format
/// when the representation must change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStateV1 {
    pub columns: u64,
    pub rows: u64,
    pub styles: Vec<StyleStateV1>,
    pub primary: GridBufferStateV1,
    pub alternate: Option<GridBufferStateV1>,
    pub alternate_active: bool,
    pub scrollback: Vec<RowStateV1>,
    pub max_scrollback: u64,
    #[serde(default = "default_scrollback_byte_budget")]
    pub max_scrollback_bytes: u64,
    pub scrolled_lines: u64,
    pub content_epoch: u64,
    pub origin_mode: bool,
    pub autowrap: bool,
    pub retain_alt_history: bool,
    pub full_clears: u64,
    pub active_hyperlink: Option<u32>,
    pub hyperlinks: Vec<HyperlinkStateV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridBufferStateV1 {
    pub rows: Vec<RowStateV1>,
    pub cursor: CursorStateV1,
    pub saved_cursor: SavedCursorStateV1,
    pub style: u32,
    pub wrap_pending: bool,
    pub scroll_top: u64,
    pub scroll_bottom: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowStateV1 {
    /// Grapheme text. Wide continuation cells are omitted and reconstructed.
    pub text: String,
    /// Style runs cover every cell column, including wide continuations.
    pub styles: Vec<StyleRunStateV1>,
    pub hyperlinks: Vec<HyperlinkCellStateV1>,
    pub wrapped: bool,
    /// Synthetic wide-wrap gaps; absent in older snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrap_padding: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleRunStateV1 {
    pub len: u32,
    pub style: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperlinkCellStateV1 {
    pub column: u32,
    pub handle: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorStateV1 {
    pub row: u64,
    pub column: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCursorStateV1 {
    pub cursor: CursorStateV1,
    pub wrap_pending: bool,
    pub style: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleStateV1 {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub underline_style: UnderlineStyle,
    pub inverse: bool,
    pub foreground: Color,
    pub background: Color,
    pub underline_color: Color,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperlinkStateV1 {
    pub id: Option<String>,
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenStateError {
    Invalid(String),
    SizeOverflow,
}

impl std::fmt::Display for ScreenStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid screen state: {message}"),
            Self::SizeOverflow => f.write_str("screen state size does not fit the host"),
        }
    }
}

impl std::error::Error for ScreenStateError {}

/// Default retained-row allocation budget per screen (96 MiB).
/// Shared cluster storage and hyperlinks are accounted separately.
pub const DEFAULT_SCROLLBACK_BYTE_BUDGET: usize = 96 * 1024 * 1024;

fn default_scrollback_byte_budget() -> u64 {
    DEFAULT_SCROLLBACK_BYTE_BUDGET as u64
}

impl Screen {
    pub fn new(columns: usize, rows: usize, max_scrollback: usize) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let slots =
            history_budget::row_limit(columns, max_scrollback, DEFAULT_SCROLLBACK_BYTE_BUDGET);
        Self {
            columns,
            rows,
            primary: GridBuffer::new(columns, rows),
            clusters: ClusterStore::default(),
            alt: None,
            alt_active: false,
            scrollback: VecDeque::with_capacity(slots),
            scrollback_wrapped: VecDeque::with_capacity(slots),
            max_scrollback,
            max_scrollback_bytes: DEFAULT_SCROLLBACK_BYTE_BUDGET,
            scrollback_budget_warned: false,
            scrolled_lines: 0,
            content_epoch: 0,
            origin_mode: false,
            autowrap: true,
            retain_alt_history: false,
            full_clears: 0,
            active_hyperlink: None,
            hyperlinks: Vec::new(),
            damage: GridDamage::full(rows, columns),
        }
    }

    /// Export the complete logical screen into a compact, versioned DTO.
    pub fn export_state(&self) -> ScreenStateV1 {
        let mut styles = Vec::new();
        let primary = export_grid_buffer(&self.primary, self.columns, &mut styles, &self.clusters);
        let alternate = self
            .alt
            .as_ref()
            .map(|buffer| export_grid_buffer(buffer, self.columns, &mut styles, &self.clusters));
        let scrollback = self
            .scrollback
            .iter()
            .zip(self.scrollback_wrapped.iter())
            .map(|(line, wrapped)| export_row(line, *wrapped, &mut styles, &self.clusters))
            .collect();
        ScreenStateV1 {
            columns: self.columns as u64,
            rows: self.rows as u64,
            styles,
            primary,
            alternate,
            alternate_active: self.alt_active,
            scrollback,
            max_scrollback: self.max_scrollback as u64,
            max_scrollback_bytes: self.max_scrollback_bytes as u64,
            scrolled_lines: self.scrolled_lines,
            content_epoch: self.content_epoch,
            origin_mode: self.origin_mode,
            autowrap: self.autowrap,
            retain_alt_history: self.retain_alt_history,
            full_clears: self.full_clears,
            active_hyperlink: self.active_hyperlink.map(|id| id.0),
            hyperlinks: self
                .hyperlinks
                .iter()
                .map(|link| HyperlinkStateV1 {
                    id: link.id.clone(),
                    uri: link.uri.clone(),
                })
                .collect(),
        }
    }

    /// Import a validated screen DTO. Evict oldest rows to fit its byte budget.
    /// Budget-driven eviction emits one diagnostic on stderr.
    pub fn import_state(state: ScreenStateV1) -> Result<Self, ScreenStateError> {
        validate_screen_state(&state)?;
        let columns = usize::try_from(state.columns).map_err(|_| ScreenStateError::SizeOverflow)?;
        let rows = usize::try_from(state.rows).map_err(|_| ScreenStateError::SizeOverflow)?;
        let max_scrollback =
            usize::try_from(state.max_scrollback).map_err(|_| ScreenStateError::SizeOverflow)?;
        let max_scrollback_bytes = usize::try_from(state.max_scrollback_bytes)
            .map_err(|_| ScreenStateError::SizeOverflow)?;
        let slots = history_budget::row_limit(columns, max_scrollback, max_scrollback_bytes);
        let skipped_rows = state.scrollback.len().saturating_sub(slots);
        let mut clusters = ClusterStore::default();
        let primary = import_grid_buffer(
            &state.primary,
            columns,
            rows,
            &state.styles,
            state.hyperlinks.len(),
            &mut clusters,
        )?;
        let alternate = state
            .alternate
            .as_ref()
            .map(|buffer| {
                import_grid_buffer(
                    buffer,
                    columns,
                    rows,
                    &state.styles,
                    state.hyperlinks.len(),
                    &mut clusters,
                )
            })
            .transpose()?;
        let scrollback = state
            .scrollback
            .iter()
            .skip(skipped_rows)
            .map(|row| {
                import_row(
                    row,
                    columns,
                    &state.styles,
                    state.hyperlinks.len(),
                    &mut clusters,
                )
            })
            .collect::<Result<Vec<Vec<Cell>>, ScreenStateError>>()?;
        let scrollback_wrapped = state
            .scrollback
            .iter()
            .skip(skipped_rows)
            .map(|row| row.wrapped)
            .collect();
        let hyperlinks = state
            .hyperlinks
            .into_iter()
            .map(|link| Hyperlink {
                id: link.id,
                uri: link.uri,
            })
            .collect();
        clusters.finish_collection();
        let mut screen = Self {
            columns,
            rows,
            primary,
            clusters,
            alt: alternate,
            alt_active: state.alternate_active,
            scrollback: scrollback.into_iter().collect(),
            scrollback_wrapped,
            max_scrollback,
            max_scrollback_bytes,
            scrollback_budget_warned: false,
            scrolled_lines: state.scrolled_lines,
            content_epoch: state.content_epoch,
            origin_mode: state.origin_mode,
            autowrap: state.autowrap,
            retain_alt_history: state.retain_alt_history,
            full_clears: state.full_clears,
            active_hyperlink: state.active_hyperlink.map(HyperlinkId),
            hyperlinks,
            damage: GridDamage::full(rows, columns),
        };
        screen.configure_scrollback(columns);
        if skipped_rows > 0 {
            screen.note_scrollback_budget(columns);
        }
        screen.enforce_scrollback_budget();
        Ok(screen)
    }

    /// used; classic claim paths must never turn this on.
    pub fn set_retain_alt_history(&mut self, retain: bool) {
        self.retain_alt_history = retain;
    }

    pub const fn columns(&self) -> usize {
        self.columns
    }

    pub const fn rows(&self) -> usize {
        self.rows
    }

    pub const fn alt_active(&self) -> bool {
        self.alt_active
    }

    /// Count of full-viewport clears (ED mode 2/3). See [`Screen::full_clears`] field docs.
    pub const fn full_clears(&self) -> u64 {
        self.full_clears
    }

    /// DECOM origin mode (CSI ? 6 h/l).
    pub const fn origin_mode(&self) -> bool {
        self.origin_mode
    }

    /// Enable or disable DECOM. Does not move the cursor by itself.
    pub fn set_origin_mode(&mut self, enabled: bool) {
        self.origin_mode = enabled;
        // xterm: enabling origin mode does not force-home; apps typically CUP after.
        // Constrain cursor into margins if now outside.
        if enabled {
            self.clamp_cursor_to_origin_region();
        }
    }

    /// DECAWM auto-wrap mode (CSI ? 7 h/l). Default true.
    pub const fn autowrap(&self) -> bool {
        self.autowrap
    }

    pub fn set_autowrap(&mut self, enabled: bool) {
        self.autowrap = enabled;
        if !enabled {
            // Pending wrap only applies when auto-wrap is on.
            self.active_mut().wrap_pending = false;
        }
    }

    fn clamp_cursor_to_origin_region(&mut self) {
        if !self.origin_mode {
            return;
        }
        let top = self.active().scroll_top;
        let bottom = self.active().scroll_bottom;
        let buf = self.active_mut();
        if buf.cursor.row < top {
            buf.cursor.row = top;
        } else if buf.cursor.row > bottom {
            buf.cursor.row = bottom;
        }
    }

    /// Zero-based cursor for CPR: relative to scroll-top when origin mode is on.
    pub fn cursor_report(&self) -> Cursor {
        let c = self.cursor();
        if self.origin_mode {
            let top = self.active().scroll_top;
            Cursor {
                row: c.row.saturating_sub(top),
                column: c.column,
            }
        } else {
            c
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.active().cursor
    }

    pub fn style(&self) -> Style {
        self.active().style
    }

    pub const fn scrolled_lines(&self) -> u64 {
        self.scrolled_lines
    }

    /// Monotonic content/viewport epoch for selection invalidation (matrix F6).
    pub const fn content_epoch(&self) -> u64 {
        self.content_epoch
    }

    fn bump_epoch(&mut self) {
        self.content_epoch = self.content_epoch.saturating_add(1);
    }

    /// Damage since the last take. PT-243 reads dirty rows + scroll list.
    pub fn damage(&self) -> &GridDamage {
        &self.damage
    }

    /// Consume this frame's damage and leave an empty accumulator.
    pub fn take_damage(&mut self) -> GridDamage {
        self.damage.take()
    }

    /// Rebuild this screen from `src` using only `damage` (PT-242/PT-243).
    ///
    /// Apply scroll events as row moves first, then copy dirty cells.
    pub fn apply_damage(&mut self, src: &Screen, damage: &GridDamage) {
        if src.columns != self.columns || src.rows != self.rows {
            self.resize(src.columns, src.rows);
        }
        if src.alt_active != self.alt_active {
            if src.alt_active {
                self.enter_alt_screen(AltScreenMode::Mode47);
            } else {
                self.leave_alt_screen(AltScreenMode::Mode47);
            }
        }
        for event in damage.scroll_events() {
            self.apply_scroll_event(*event);
        }
        let columns = self.columns;
        let rows = self.rows;
        for row in 0..rows {
            for col in 0..columns {
                if damage.is_cell_dirty(row, col) {
                    let source = src.view_cell(0, row, col);
                    let mut cell = *source;
                    if cell.cluster != 0 {
                        cell.cluster = self.clusters.import(&src.clusters, cell.cluster);
                    }
                    let buf = self.active_mut();
                    let index = row * columns + col;
                    if index < buf.cells.len() {
                        buf.cells[index] = cell;
                    }
                }
            }
        }
        if damage.scroll_overflowed() {
            // Full copying replaces the row moves that carried wrap flags.
            self.active_mut().wrapped.clone_from(&src.active().wrapped);
        }
        let cursor = src.cursor();
        self.active_mut().cursor = cursor;
        let retired_rows = damage.retired_rows();
        if self.clusters.retire_rows(
            retired_rows,
            self.rows
                .saturating_mul(2)
                .saturating_add(self.scrollback.len()),
        ) {
            self.collect_clusters();
        } else {
            self.collect_clusters_if_needed();
        }
    }

    fn apply_scroll_event(&mut self, event: ScrollDamage) {
        if event.delta == 0 || event.bottom < event.top {
            return;
        }
        let top = event.top.min(self.rows.saturating_sub(1));
        let bottom = event.bottom.min(self.rows.saturating_sub(1));
        if bottom < top {
            return;
        }
        let height = bottom - top + 1;
        let n = (event.delta.unsigned_abs() as usize).min(height);
        let buf = self.active_mut();
        if event.delta > 0 {
            if n < height {
                buf.cells.scroll_up(top, bottom, n);
                if bottom < buf.wrapped.len() {
                    buf.wrapped.copy_within(top + n..=bottom, top);
                }
            }
        } else if n < height {
            buf.cells.scroll_down(top, bottom, n);
            if bottom < buf.wrapped.len() {
                buf.wrapped.copy_within(top..=bottom - n, top + n);
            }
        }
    }

    fn mark_cursor_cells(&mut self, old: Cursor, new: Cursor) {
        self.damage.mark_cell(old.row, old.column);
        self.damage.mark_cell(new.row, new.column);
    }

    pub fn set_style(&mut self, style: Style) {
        self.active_mut().style = style;
    }

    /// Start an OSC 8 hyperlink for subsequently painted cells.
    ///
    /// Repeated `(id, URI)` pairs reuse one stable handle. Invalid or over-limit
    /// values clear the active hyperlink and return `false`.
    pub fn set_hyperlink(&mut self, id: Option<&str>, uri: &str) -> bool {
        if uri.is_empty() {
            self.active_hyperlink = None;
            return true;
        }
        let valid_uri = uri.len() <= MAX_HYPERLINK_URI_BYTES
            && !uri.chars().any(|ch| ch <= '\u{1f}' || ch == '\u{7f}');
        let valid_id = id.is_none_or(|value| {
            value.len() <= MAX_HYPERLINK_ID_BYTES
                && !value.chars().any(|ch| ch <= '\u{1f}' || ch == '\u{7f}')
        });
        if !valid_uri || !valid_id {
            self.active_hyperlink = None;
            return false;
        }
        if let Some(index) = self
            .hyperlinks
            .iter()
            .position(|link| link.id.as_deref() == id && link.uri == uri)
        {
            self.active_hyperlink = Some(HyperlinkId(index as u32));
            return true;
        }
        if self.hyperlinks.len() >= MAX_HYPERLINKS {
            self.active_hyperlink = None;
            return false;
        }
        let hyperlink = HyperlinkId(self.hyperlinks.len() as u32);
        self.hyperlinks.push(Hyperlink {
            id: id.map(str::to_owned),
            uri: uri.to_owned(),
        });
        self.active_hyperlink = Some(hyperlink);
        true
    }

    /// End the active OSC 8 hyperlink. Existing linked cells remain linked.
    pub fn clear_hyperlink(&mut self) {
        self.active_hyperlink = None;
    }

    /// Explicit OSC 8 URI attached to a viewport cell, if any.
    pub fn hyperlink_uri_at_view(
        &self,
        scroll_offset: usize,
        row: usize,
        col: usize,
    ) -> Option<&str> {
        let cell = self.view_cell(scroll_offset, row, col);
        let index = cell.hyperlink?.0 as usize;
        self.hyperlinks.get(index).map(|link| link.uri.as_str())
    }

    fn cell_view<'a>(&'a self, cell: &'a Cell) -> CellView<'a> {
        CellView {
            cell,
            clusters: &self.clusters,
        }
    }

    /// Borrow a live row with its grapheme storage.
    pub fn view_row(
        &self,
        row: usize,
    ) -> Option<impl ExactSizeIterator<Item = CellView<'_>> + DoubleEndedIterator> {
        self.row(row)
            .map(|cells| cells.iter().map(|cell| self.cell_view(cell)))
    }

    /// Raw cells for attribute reads. Use [`Self::view_row`] to read text.
    pub fn row(&self, row: usize) -> Option<&[Cell]> {
        let start = row.checked_mul(self.columns)?;
        self.active().cells.get(start..start + self.columns)
    }

    pub fn scrollback(&self) -> &VecDeque<Vec<Cell>> {
        &self.scrollback
    }

    /// Flag the cursor's row as ending in an autowrap, not a line feed.
    fn mark_row_wrapped(&mut self) {
        let row = self.active().cursor.row;
        if let Some(flag) = self.active_mut().wrapped.get_mut(row) {
            *flag = true;
        }
    }

    /// Did the absolute history line `abs_row` end in an autowrap?
    ///
    /// Indexing matches [`Self::history_cell`]: `scrollback || primary`.
    /// Copy uses this to join a wrapped line to the next with no newline.
    pub fn history_line_wrapped(&self, abs_row: usize) -> bool {
        let sb = self.scrollback.len();
        if abs_row < sb {
            return self
                .scrollback_wrapped
                .get(abs_row)
                .copied()
                .unwrap_or(false);
        }
        self.primary
            .wrapped
            .get(abs_row - sb)
            .copied()
            .unwrap_or(false)
    }

    /// How far the host may scroll up from the live bottom (primary scrollback
    /// only). Alternate screen has no history view — returns 0.
    pub fn max_view_scroll(&self) -> usize {
        if self.alt_active {
            0
        } else {
            self.scrollback.len()
        }
    }

    /// History depth regardless of screen mode. Unlike
    /// [`Screen::max_view_scroll`] this does not zero out on the alt screen;
    /// attach scroll mode uses it so TUI output stays reachable.
    pub fn history_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Like [`Screen::view_cell`] but mode-agnostic: offsets walk
    /// `scrollback || active grid` even while the alt screen is active.
    /// Offset 0 is the live active grid in either mode.
    pub fn history_view_cell(&self, scroll_offset: usize, row: usize, col: usize) -> CellView<'_> {
        if row >= self.rows || col >= self.columns {
            return self.cell_view(&BLANK_CELL);
        }
        if scroll_offset == 0 {
            return self
                .row(row)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL));
        }
        let sb = self.scrollback.len();
        let offset = scroll_offset.min(sb);
        let start = sb.saturating_sub(offset);
        let abs = start + row;
        if abs < sb {
            self.scrollback
                .get(abs)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL))
        } else {
            let vrow = abs - sb;
            self.row(vrow)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL))
        }
    }

    /// Absolute history row for a viewport cell under `scroll_offset` (ADR abs select).
    ///
    /// Combined history (oldest → newest): `scrollback || primary`. Live view
    /// (`scroll_offset == 0`) maps viewport row `r` to `scrollback.len() + r`.
    pub fn abs_row_at_view(&self, scroll_offset: usize, view_row: usize) -> usize {
        if self.alt_active {
            return view_row.min(self.rows.saturating_sub(1));
        }
        let sb = self.scrollback.len();
        let offset = scroll_offset.min(sb);
        let start = sb.saturating_sub(offset);
        start.saturating_add(view_row)
    }

    /// Cell at viewport `(row, col)` when scrolled up `scroll_offset` rows from
    /// the live bottom. Offset 0 is the live active grid. Offsets > 0 walk
    /// primary scrollback then the primary buffer (never alt). Out-of-range
    /// coordinates yield a blank default cell.
    ///
    /// Combined history (oldest → newest): `scrollback[0..N) || primary[0..rows)`.
    /// Live view shows the last `rows` lines; offset `k` shows the window that
    /// ends `k` lines above the live bottom.
    pub fn view_cell(&self, scroll_offset: usize, row: usize, col: usize) -> CellView<'_> {
        if row >= self.rows || col >= self.columns {
            return self.cell_view(&BLANK_CELL);
        }
        if scroll_offset == 0 || self.alt_active {
            return self
                .row(row)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL));
        }
        let sb = self.scrollback.len();
        let offset = scroll_offset.min(sb);
        // start abs index into [scrollback || primary]
        let start = sb.saturating_sub(offset);
        let abs = start + row;
        if abs < sb {
            self.scrollback
                .get(abs)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL))
        } else {
            let vrow = abs - sb;
            self.primary_row(vrow)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL))
        }
    }

    fn primary_row(&self, row: usize) -> Option<&[Cell]> {
        let start = row.checked_mul(self.columns)?;
        self.primary.cells.get(start..start + self.columns)
    }

    /// Total lines in primary history + live grid (oldest → newest). Alt: live only.
    pub fn history_line_count(&self) -> usize {
        if self.alt_active {
            self.rows
        } else {
            self.scrollback.len().saturating_add(self.rows)
        }
    }

    /// Plain text of one history line (absolute index, oldest = 0).
    pub fn history_line_text(&self, abs_row: usize) -> String {
        let cols = self.columns;
        if cols == 0 {
            return String::new();
        }
        let mut s = String::with_capacity(cols);
        for col in 0..cols {
            let cell = self.history_cell(abs_row, col);
            cell.write_grapheme_into(&mut s);
        }
        while s.ends_with(' ') {
            s.pop();
        }
        s
    }

    fn history_cell(&self, abs_row: usize, col: usize) -> CellView<'_> {
        if self.alt_active {
            return self
                .row(abs_row)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL));
        }
        let sb = self.scrollback.len();
        if abs_row < sb {
            self.scrollback
                .get(abs_row)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL))
        } else {
            let vrow = abs_row - sb;
            self.primary_row(vrow)
                .and_then(|line| line.get(col))
                .map(|cell| self.cell_view(cell))
                .unwrap_or_else(|| self.cell_view(&BLANK_CELL))
        }
    }

    /// `view_scroll` so `abs_row` is visible (prefer ~1/3 from top).
    pub fn view_scroll_for_history_row(&self, abs_row: usize) -> usize {
        if self.alt_active || self.rows == 0 {
            return 0;
        }
        let sb = self.scrollback.len();
        let prefer = self.rows / 3;
        let start = abs_row.saturating_sub(prefer);
        sb.saturating_sub(start)
    }

    /// Viewport row for `abs_row` given a `view_scroll` (clamped).
    pub fn viewport_row_for_history(&self, abs_row: usize, view_scroll: usize) -> Option<usize> {
        if self.alt_active {
            return (abs_row < self.rows).then_some(abs_row);
        }
        let sb = self.scrollback.len();
        let offset = view_scroll.min(sb);
        let start = sb.saturating_sub(offset);
        if abs_row < start {
            return None;
        }
        let row = abs_row - start;
        (row < self.rows).then_some(row)
    }

    /// Substring search over history lines (wraps forward).
    ///
    /// Starts after `(after_abs, after_col)` exclusive, or from the beginning
    /// when `after` is `None`. Returns absolute row + inclusive columns.
    /// Default is **case-insensitive** (Unicode lowercase equality per cell).
    pub fn find_in_history(
        &self,
        query: &str,
        after: Option<(usize, usize)>,
    ) -> Option<HistoryMatch> {
        self.find_in_history_opts(query, after, false)
    }

    /// Like [`find_in_history`] with explicit case sensitivity.
    pub fn find_in_history_opts(
        &self,
        query: &str,
        after: Option<(usize, usize)>,
        case_sensitive: bool,
    ) -> Option<HistoryMatch> {
        if query.is_empty() || self.columns == 0 {
            return None;
        }
        let n = self.history_line_count();
        if n == 0 {
            return None;
        }
        let q: Vec<char> = query.chars().collect();
        if q.is_empty() {
            return None;
        }
        let (start_abs, start_col) = match after {
            Some((a, c)) => (a, c.saturating_add(1)),
            None => (0, 0),
        };
        for pass in 0..2 {
            let from = if pass == 0 { start_abs } else { 0 };
            let to = if pass == 0 {
                n
            } else {
                start_abs.saturating_add(1).min(n)
            };
            for abs in from..to {
                let chars: Vec<char> = self.history_line_text(abs).chars().collect();
                if q.len() > chars.len() {
                    continue;
                }
                let search_from = if abs == start_abs && pass == 0 {
                    start_col.min(chars.len())
                } else {
                    0
                };
                if search_from >= chars.len() {
                    continue;
                }
                if let Some(rel) = chars[search_from..]
                    .windows(q.len())
                    .position(|w| history_window_eq(w, &q, case_sensitive))
                {
                    let col = search_from + rel;
                    let end = col + q.len() - 1;
                    return Some(HistoryMatch {
                        abs_row: abs,
                        start_col: col,
                        end_col: end.min(self.columns.saturating_sub(1)),
                    });
                }
            }
        }
        None
    }

    /// Reverse substring search over history lines (wraps backward).
    ///
    /// Starts *before* the last match: when `before` is `Some((row, start_col))`,
    /// candidates must be lexicographically earlier than that start cell. When
    /// `before` is `None`, search from the end of history. Case folding matches
    /// [`find_in_history_opts`].
    pub fn find_in_history_rev(
        &self,
        query: &str,
        before: Option<(usize, usize)>,
        case_sensitive: bool,
    ) -> Option<HistoryMatch> {
        if query.is_empty() || self.columns == 0 {
            return None;
        }
        let n = self.history_line_count();
        if n == 0 {
            return None;
        }
        let q: Vec<char> = query.chars().collect();
        if q.is_empty() {
            return None;
        }
        let (start_abs, start_col) = match before {
            Some((a, c)) => (a, c),
            None => (n.saturating_sub(1), usize::MAX),
        };
        for pass in 0..2 {
            // pass 0: strictly before `before` (older / same-row left of start).
            // pass 1: wrap — newest match overall (may re-hit sole match).
            let row_range_rev: Vec<usize> = if pass == 0 {
                let abs_start = start_abs.min(n.saturating_sub(1));
                (0..=abs_start).rev().collect()
            } else {
                (0..n).rev().collect()
            };
            for abs in row_range_rev {
                let chars: Vec<char> = self.history_line_text(abs).chars().collect();
                if q.len() > chars.len() {
                    continue;
                }
                // Exclusive upper bound on match *start* column.
                let limit = if pass == 0 && abs == start_abs {
                    start_col.min(chars.len().saturating_sub(q.len()).saturating_add(1))
                } else {
                    chars.len().saturating_sub(q.len()).saturating_add(1)
                };
                if limit == 0 {
                    continue;
                }
                let mut found: Option<usize> = None;
                for start in (0..limit).rev() {
                    if history_window_eq(&chars[start..start + q.len()], &q, case_sensitive) {
                        found = Some(start);
                        break;
                    }
                }
                if let Some(col) = found {
                    let end = col + q.len() - 1;
                    return Some(HistoryMatch {
                        abs_row: abs,
                        start_col: col,
                        end_col: end.min(self.columns.saturating_sub(1)),
                    });
                }
            }
        }
        None
    }

    /// Count all substring matches of `query` in history (same rules as find).
    ///
    /// Matches may overlap (e.g. `"aa"` in `"aaa"` → 2). Empty query → 0.
    pub fn count_history_matches(&self, query: &str, case_sensitive: bool) -> usize {
        self.history_matches(query, case_sensitive).len()
    }

    /// 1-based rank of `m` among all matches of `query`, plus total count.
    ///
    /// Returns `None` if `m` is not a match for `query` under the same rules.
    pub fn history_match_rank(
        &self,
        query: &str,
        m: HistoryMatch,
        case_sensitive: bool,
    ) -> Option<(usize, usize)> {
        let all = self.history_matches(query, case_sensitive);
        let total = all.len();
        let idx = all.iter().position(|x| *x == m)?;
        Some((idx + 1, total))
    }

    /// All history matches in forward order (oldest → newest, left → right).
    pub fn history_matches(&self, query: &str, case_sensitive: bool) -> Vec<HistoryMatch> {
        if query.is_empty() || self.columns == 0 {
            return Vec::new();
        }
        let n = self.history_line_count();
        if n == 0 {
            return Vec::new();
        }
        let q: Vec<char> = query.chars().collect();
        if q.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for abs in 0..n {
            let chars: Vec<char> = self.history_line_text(abs).chars().collect();
            if q.len() > chars.len() {
                continue;
            }
            let max_start = chars.len().saturating_sub(q.len());
            for start in 0..=max_start {
                if history_window_eq(&chars[start..start + q.len()], &q, case_sensitive) {
                    let end = start + q.len() - 1;
                    out.push(HistoryMatch {
                        abs_row: abs,
                        start_col: start,
                        end_col: end.min(self.columns.saturating_sub(1)),
                    });
                }
            }
        }
        out
    }

    fn active(&self) -> &GridBuffer {
        if self.alt_active {
            // never panics if invariant is broken; fall back to primary
            // after debug_assert (heal path for corrupt state).
            debug_assert!(self.alt.is_some(), "alt_active but alt buffer missing");
            match self.alt.as_ref() {
                Some(alt) => alt,
                None => &self.primary,
            }
        } else {
            &self.primary
        }
    }

    fn active_with_clusters_mut(&mut self) -> (&mut GridBuffer, &mut ClusterStore) {
        let buffer = if self.alt_active {
            self.alt.as_mut().expect("active alternate buffer exists")
        } else {
            &mut self.primary
        };
        (buffer, &mut self.clusters)
    }

    fn collect_clusters_if_needed(&mut self) {
        if !self.clusters.needs_collection() {
            return;
        }
        self.collect_clusters();
    }

    fn collect_clusters(&mut self) {
        if self.clusters.len() == 0 {
            return;
        }
        let old = std::mem::take(&mut self.clusters);
        let mut remap = vec![0; old.len()];
        let cells = self
            .primary
            .cells
            .iter_mut()
            .chain(
                self.alt
                    .iter_mut()
                    .flat_map(|buffer| buffer.cells.iter_mut()),
            )
            .chain(self.scrollback.iter_mut().flat_map(|row| row.iter_mut()));
        for cell in cells {
            if cell.cluster != 0 {
                let mapped = &mut remap[cell.cluster as usize - 1];
                if *mapped == 0 {
                    *mapped = self.clusters.intern(old.get(cell.cluster));
                }
                cell.cluster = *mapped;
            }
        }
        self.clusters.finish_collection();
    }

    fn active_mut(&mut self) -> &mut GridBuffer {
        if self.alt_active {
            debug_assert!(self.alt.is_some(), "alt_active but alt buffer missing");
            if self.alt.is_none() {
                // Self-heal: allocate empty alt so subsequent ops stay on alt.
                let mut alt = GridBuffer::new(self.columns, self.rows);
                alt.style = self.primary.style;
                self.alt = Some(alt);
            }
            self.alt.as_mut().expect("alt just allocated")
        } else {
            &mut self.primary
        }
    }

    /// Best-effort resize: clip/pad cells; no smart reflow (matrix F3).
    ///
    /// Preserves DECSTBM margins (clamped to the new row count; invalid
    /// one-line regions fall back to full screen). A region that already
    /// covered the **old full screen** grows/shrinks with the new height so
    /// default margins stay full-screen after row growth (codex dual-sign).
    /// Rewrites scrollback row widths to the new column count.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.resize_impl(columns, rows, false);
    }

    fn resize_impl(&mut self, columns: usize, rows: usize, reflow: bool) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        if columns == self.columns && rows == self.rows {
            return;
        }
        let old_rows = self.rows;
        let primary_margins = (self.primary.scroll_top, self.primary.scroll_bottom);
        let alt_margins = self
            .alt
            .as_ref()
            .map(|alt| (alt.scroll_top, alt.scroll_bottom));
        if reflow {
            self.reflow_primary(columns, rows);
        } else {
            self.primary = resize_buffer(&self.primary, self.columns, self.rows, columns, rows);
        }
        apply_clamped_scroll_region(
            &mut self.primary,
            primary_margins.0,
            primary_margins.1,
            old_rows,
            rows,
        );
        if let Some(alt) = self.alt.take() {
            let mut next = resize_buffer(&alt, self.columns, self.rows, columns, rows);
            if let Some((top, bottom)) = alt_margins {
                apply_clamped_scroll_region(&mut next, top, bottom, old_rows, rows);
            }
            self.alt = Some(next);
        }
        let evicted = self.configure_scrollback(columns);
        for line in &mut self.scrollback {
            resize_scrollback_line(line, columns);
        }
        // Keep flags for surviving rows and default newly added rows.
        self.primary.wrapped.resize(rows, false);
        if let Some(alt) = self.alt.as_mut() {
            alt.wrapped.resize(rows, false);
        }
        let shrinking = columns < self.columns || rows < self.rows;
        self.columns = columns;
        self.rows = rows;
        self.enforce_scrollback_budget();
        if shrinking || evicted {
            self.collect_clusters();
        }
        self.damage.resize(rows, columns);
        self.bump_epoch();
    }

    /// Enter alternate screen for a specific private mode.
    ///
    /// xterm-class distinctions (0.1.0):
    /// - **1049**: DECSC on primary (cursor + wrap + SGR), clear alt, switch
    /// - **1047**: clear alt, switch to alt (no primary cursor save)
    /// - **47**: switch to alt without clearing existing alt content
    pub fn enter_alt_screen(&mut self, mode: AltScreenMode) {
        if self.alt_active {
            // Already on alt: 1049/1047 re-clear; 47 leaves content.
            // Re-clear keeps the active pen (terminal-wide SGR).
            if matches!(mode, AltScreenMode::Mode1049 | AltScreenMode::Mode1047) {
                if let Some(alt) = self.alt.as_mut() {
                    let pen = alt.style;
                    clear_alt_buffer(alt, self.rows, pen);
                }
                self.bump_epoch();
            }
            self.damage.mark_all();
            return;
        }
        if matches!(mode, AltScreenMode::Mode1049) {
            // Active is still primary: same bundle as ESC 7 / DECSC.
            self.save_cursor();
        }
        // xterm-class pen is terminal-wide: carry primary SGR onto alt.
        let pen = self.primary.style;
        let clear = matches!(mode, AltScreenMode::Mode1049 | AltScreenMode::Mode1047);
        match self.alt.as_mut() {
            Some(alt) if clear => clear_alt_buffer(alt, self.rows, pen),
            Some(alt) => {
                // Mode 47 soft switch: cells preserved, pen follows terminal.
                alt.style = pen;
            }
            None => {
                let mut alt = GridBuffer::new(self.columns, self.rows);
                alt.style = pen;
                self.alt = Some(alt);
            }
        }
        self.alt_active = true;
        self.damage.mark_all();
        self.bump_epoch();
    }

    /// Leave alternate screen; restore primary cells and (for 1049) DECSC state.
    ///
    /// For **1047** / **1049**, clear the alt buffer on leave so a later mode-47
    /// enter does not resurrect stale alt content. Mode **47** keeps
    /// alt cells (xterm-class soft switch).
    pub fn leave_alt_screen(&mut self, mode: AltScreenMode) {
        if !self.alt_active {
            return;
        }
        self.damage.mark_all();
        // Capture active (alt) pen before switching buffers (reverse path).
        let alt_pen = self.active().style;
        self.alt_active = false;
        if matches!(mode, AltScreenMode::Mode1049) {
            // Active is primary again: same restore as ESC 8 / DECRC (saved pen).
            self.restore_cursor();
        } else {
            // Modes 47 / 1047: pen is terminal-wide — carry alt pen back to primary.
            self.primary.style = alt_pen;
        }
        if matches!(mode, AltScreenMode::Mode1049 | AltScreenMode::Mode1047) {
            if let Some(alt) = self.alt.as_mut() {
                // Leave clears cells; pen default is fine (active is primary).
                clear_alt_buffer(alt, self.rows, Style::default());
            }
        }
        self.bump_epoch();
    }

    /// Set inclusive scroll region (DECSTBM). Params are 1-based; 0 means default.
    ///
    /// VT100: minimum region height is two lines (`top < bottom` after conversion).
    /// Invalid (top>bottom, one-line, or out of range collapse) → full screen.
    /// Cursor homes to the top-left of the (possibly reset) region.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let rows = self.rows;
        let last = rows.saturating_sub(1);
        let top0 = if top == 0 {
            0
        } else {
            top.saturating_sub(1).min(last)
        };
        let bottom0 = if bottom == 0 {
            last
        } else {
            bottom.saturating_sub(1).min(last)
        };
        // VT100 requires at least two lines: top0 < bottom0.
        let (top0, bottom0) = if top0 < bottom0 {
            (top0, bottom0)
        } else {
            (0, last)
        };
        let origin = self.origin_mode;
        {
            let buf = self.active_mut();
            buf.scroll_top = top0;
            buf.scroll_bottom = bottom0;
            // DECSTBM homes: absolute 1;1 when DECOM off; top-left of region when on.
            buf.cursor = if origin {
                Cursor {
                    row: top0,
                    column: 0,
                }
            } else {
                Cursor::default()
            };
            buf.wrap_pending = false;
        }
        self.damage.mark_all();
    }

    /// Reset scroll region to full screen and home cursor (VT100 DECSTBM default).
    pub fn reset_scroll_region(&mut self) {
        let rows = self.rows;
        {
            let buf = self.active_mut();
            buf.scroll_top = 0;
            buf.scroll_bottom = rows.saturating_sub(1);
            // Full-screen region: origin and absolute home are both (0,0).
            buf.cursor = Cursor::default();
            buf.wrap_pending = false;
        }
        self.damage.mark_all();
    }

    pub fn put_char(&mut self, character: char) {
        // ADR-0004 grapheme slice: non-spacing marks, emoji modifiers, and
        // ZWJ-joined bases attach to the previous cell without advancing.
        let width = char_display_width(character);
        if width == 0 || is_emoji_modifier(character) {
            self.attach_cluster_scalar(character);
            return;
        }
        if self.previous_base_ends_with_zwj() && is_zwj_joinable(character) {
            self.attach_cluster_scalar(character);
            return;
        }
        // Flag pairs: second regional indicator joins the first into one wide cell.
        if is_regional_indicator(character) && self.try_extend_regional_indicator_pair(character) {
            return;
        }

        if self.autowrap && self.active().wrap_pending {
            self.mark_row_wrapped();
            self.carriage_return();
            self.line_feed();
        }

        // Wide glyph that does not fit on this line → wrap first when auto-wrap on.
        if self.autowrap
            && width == 2
            && self.columns > 1
            && self.active().cursor.column + width > self.columns
        {
            let index = self.active().cursor.row * self.columns + self.active().cursor.column;
            self.active_mut().cells[index] = Cell {
                wrap_padding: true,
                ..Cell::default()
            };
            self.mark_row_wrapped();
            self.carriage_return();
            self.line_feed();
        }

        let columns = self.columns;
        let style = self.active().style;
        let hyperlink = self.active_hyperlink;
        let old_cursor = self.active().cursor;
        let row = old_cursor.row;
        let col = old_cursor.column;

        // Overwriting either half of a wide pair clears both.
        self.clear_wide_pair_covering(row, col);
        if width == 2 && col + 1 < columns {
            self.clear_wide_pair_covering(row, col + 1);
        }

        let autowrap = self.autowrap;
        let new_cursor = {
            let buf = self.active_mut();
            let cells = buf.cells.row_mut(row);
            cells[col] = Cell::glyph(character, style).with_hyperlink(hyperlink);
            if width == 2 && col + 1 < columns {
                cells[col + 1] = Cell::wide_continuation(style).with_hyperlink(hyperlink);
            }

            let next_col = col + width;
            if next_col >= columns {
                if autowrap {
                    buf.wrap_pending = true;
                }
                // Stay on last column (overwrite next put when wrap off).
                buf.cursor.column = columns.saturating_sub(1);
            } else {
                buf.cursor.column = next_col;
            }
            buf.cursor
        };
        self.mark_cursor_cells(old_cursor, new_cursor);
        // Cell mutation: epoch consumers see put_char, not only scroll/alt.
        self.bump_epoch();
    }

    /// Column of the base cell immediately before the cursor (skips wide_cont).
    fn previous_base_column(&self) -> Option<usize> {
        let columns = self.columns;
        if columns == 0 {
            return None;
        }
        let row = self.active().cursor.row;
        let col = self.active().cursor.column;
        let wrap_pending = self.active().wrap_pending;
        // After a base glyph, cursor sits on the next column (or last col with
        // wrap_pending). Target is the previous column, skipping wide_cont.
        let target_col = if wrap_pending {
            columns.saturating_sub(1)
        } else if col > 0 {
            col - 1
        } else {
            return None;
        };
        let buf = self.active();
        let cells = buf.cells.row(row);
        let base_col = if cells[target_col].wide_cont && target_col > 0 {
            target_col - 1
        } else {
            target_col
        };
        let base = &cells[base_col];
        if base.wide_cont {
            return None;
        }
        Some(base_col)
    }

    fn previous_base_ends_with_zwj(&self) -> bool {
        let Some(base_col) = self.previous_base_column() else {
            return false;
        };
        let row = self.active().cursor.row;
        self.active().cells.row(row)[base_col].ends_with_zwj()
    }

    /// Attach a cluster scalar to the most recent base cell (not continuation).
    fn attach_cluster_scalar(&mut self, mark: char) {
        let Some(base_col) = self.previous_base_column() else {
            return;
        };
        let columns = self.columns;
        let row = self.active().cursor.row;
        let (old_width, new_width, appended) = {
            let (buf, clusters) = self.active_with_clusters_mut();
            let cell = &mut buf.cells[row * columns + base_col];
            let old_width = cell.display_width();
            let appended = clusters.append(cell, mark);
            (old_width, cell.display_width(), appended)
        };
        if appended {
            self.damage.mark_cell(row, base_col);
            if new_width > old_width {
                self.expand_cluster_to_wide(row, base_col);
            }
            self.bump_epoch();
            self.collect_clusters_if_needed();
        }
    }

    fn expand_cluster_to_wide(&mut self, row: usize, base_col: usize) {
        if base_col + 1 >= self.columns {
            return;
        }
        self.clear_wide_pair_covering(row, base_col + 1);
        let columns = self.columns;
        let lead = self.active().cells[row * columns + base_col];
        let style = lead.style;
        let hyperlink = lead.hyperlink;
        let autowrap = self.autowrap;
        let buf = self.active_mut();
        buf.cells[row * columns + base_col + 1] =
            Cell::wide_continuation(style).with_hyperlink(hyperlink);
        let next = base_col + 2;
        if next >= columns {
            if autowrap {
                buf.wrap_pending = true;
            }
            buf.cursor.column = columns.saturating_sub(1);
        } else {
            buf.cursor.column = next;
            buf.wrap_pending = false;
        }
    }

    /// Second regional indicator joins the first into one width-2 cell (flags).
    ///
    /// Returns true when the scalar was consumed (joined or dropped for space).
    fn try_extend_regional_indicator_pair(&mut self, second: char) -> bool {
        let Some(base_col) = self.previous_base_column() else {
            return false;
        };
        let columns = self.columns;
        let row = self.active().cursor.row;
        {
            let cell = &self.active().cells[row * columns + base_col];
            if !is_regional_indicator(cell.character) || cell.cluster != 0 {
                return false;
            }
            // Already wide (should not happen for a lone RI) — still append.
        }

        // Need a continuation column: if the first RI was narrow at the last
        // column, we cannot expand; still append text but leave width 1.
        let needs_expand = {
            let cell = &self.active().cells[row * columns + base_col];
            cell.display_width() == 1
                && base_col + 1 < columns
                && !self.active().cells[row * columns + base_col + 1].wide_cont
        };

        if needs_expand {
            // Clear whatever occupies the continuation slot, then mark wide.
            self.clear_wide_pair_covering(row, base_col + 1);
            let lead = self.active().cells[row * columns + base_col];
            let style = lead.style;
            let hyperlink = lead.hyperlink;
            let autowrap = self.autowrap;
            let (buf, clusters) = self.active_with_clusters_mut();
            if !clusters.append(&mut buf.cells[row * columns + base_col], second) {
                return true;
            }
            buf.cells[row * columns + base_col + 1] =
                Cell::wide_continuation(style).with_hyperlink(hyperlink);
            // Cursor was at base_col+1 after the first RI; advance one more.
            let next = base_col + 2;
            if next >= columns {
                if autowrap {
                    buf.wrap_pending = true;
                }
                buf.cursor.column = columns.saturating_sub(1);
            } else {
                buf.cursor.column = next;
                buf.wrap_pending = false;
            }
            self.bump_epoch();
            return true;
        }

        self.attach_cluster_scalar(second);
        true
    }

    /// Clear a wide pair if `row`/`col` is a lead or continuation half (ADR-0004).
    fn clear_wide_pair_covering(&mut self, row: usize, col: usize) {
        let columns = self.columns;
        if row >= self.rows || col >= columns {
            return;
        }
        let buf = self.active_mut();
        let cells = buf.cells.row_mut(row);
        if cells[col].wide_cont {
            if col > 0 && !cells[col - 1].wide_cont {
                cells[col - 1] = Cell::default();
            }
            cells[col] = Cell::default();
            return;
        }
        if col + 1 < columns && cells[col + 1].wide_cont {
            cells[col + 1] = Cell::default();
        }
        cells[col] = Cell::default();
    }

    pub fn carriage_return(&mut self) {
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.cursor.column = 0;
        buf.wrap_pending = false;
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    /// Index / IND (`ESC D`) and line feed: move down or scroll region up.
    ///
    /// Outside the scrolling region, advances within the screen only — never
    /// jumps backward into the region (VT100 IND/LF outside margins).
    pub fn line_feed(&mut self) {
        let columns = self.columns;
        let rows = self.rows;
        let alt_active = self.alt_active;
        let max_scrollback = self.max_scrollback;
        let old_cursor = self.active().cursor;

        let cursor_only = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let bottom = buf.scroll_bottom;
            let top = buf.scroll_top;
            let row = buf.cursor.row;
            if row < top || row > bottom {
                if row < rows.saturating_sub(1) {
                    buf.cursor.row = row + 1;
                }
                Some(buf.cursor)
            } else if row < bottom {
                buf.cursor.row = row + 1;
                Some(buf.cursor)
            } else {
                None
            }
        };
        if let Some(new_cursor) = cursor_only {
            self.mark_cursor_cells(old_cursor, new_cursor);
            return;
        }

        let top = self.active().scroll_top;
        if top == 0 && max_scrollback > 0 && (!alt_active || self.retain_alt_history) {
            let wrapped = self.active().wrapped.first().copied().unwrap_or(false);
            self.retain_scrolled_row(top, wrapped);
        }
        let (top, bottom) = {
            let buf = self.active_mut();
            let bottom = buf.scroll_bottom;
            let top = buf.scroll_top;

            // At bottom margin: scroll the region only.
            // Fill scrolled-in row with space + current SGR (xterm/VT; matches erase_*).
            buf.cells.scroll_up(top, bottom, 1);
            let blank_start = bottom * columns;
            let blank = Cell::glyph(' ', buf.style);
            buf.cells[blank_start..blank_start + columns].fill(blank);
            // Flags travel with their rows. The freed bottom row starts clean.
            if bottom < buf.wrapped.len() {
                buf.wrapped.copy_within(top + 1..=bottom, top);
                buf.wrapped[bottom] = false;
            }
            (top, buf.scroll_bottom)
        };

        self.damage.push_scroll(ScrollDamage {
            top,
            bottom,
            delta: 1,
        });
        self.damage.mark_row_cells(bottom);
        // Any region scroll bumps the content epoch (selection invalidation).
        self.bump_epoch();
        // Absolute primary-row translation stays primary-only, even when
        // alternate-screen history is retained by explicit preference.
        if top == 0 && !alt_active {
            self.scrolled_lines = self.scrolled_lines.saturating_add(1);
        }
        if self.clusters.retire_rows(
            1,
            self.rows
                .saturating_mul(2)
                .saturating_add(self.scrollback.len()),
        ) {
            self.collect_clusters();
        }
    }

    /// Reverse Index / RI (`ESC M`): move up or scroll region down.
    ///
    /// VT100 / xterm:
    /// - **At top margin** (`scroll_top`): scroll the region **down** one line;
    ///   insert a blank line at the top of the region filled with the **current
    ///   SGR** (fill policy; erase paths already do this).
    /// - **Inside region below top**: move cursor up one row.
    /// - **Outside region**: move cursor up one if possible; clamp at row 0.
    ///   Does not scroll and does not warp into the region from below except by
    ///   normal single-step cursor motion.
    pub fn reverse_index(&mut self) {
        let columns = self.columns;
        let old_cursor = self.cursor();
        let scrolled = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let top = buf.scroll_top;
            let bottom = buf.scroll_bottom;
            let row = buf.cursor.row;

            // Outside the scrolling region: move up within the screen only.
            if row < top || row > bottom {
                if row > 0 {
                    buf.cursor.row = row - 1;
                }
                let new_cursor = buf.cursor;
                self.mark_cursor_cells(old_cursor, new_cursor);
                return;
            }

            // Inside region, below top margin: move up.
            if row > top {
                buf.cursor.row = row - 1;
                let new_cursor = buf.cursor;
                self.mark_cursor_cells(old_cursor, new_cursor);
                return;
            }

            // At top margin: scroll the region down (insert blank at top).
            let start = top * columns;
            buf.cells.scroll_down(top, bottom, 1);
            let style = buf.style;
            let blank = Cell::glyph(' ', style);
            buf.cells[start..start + columns].fill(blank);
            true
        };

        if scrolled {
            let top = self.active().scroll_top;
            self.damage.push_scroll(ScrollDamage {
                top,
                bottom: self.active().scroll_bottom,
                delta: -1,
            });
            self.damage.mark_row_cells(top);
            self.bump_epoch();
        }
    }

    pub fn backspace(&mut self) {
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.wrap_pending = false;
        buf.cursor.column = buf.cursor.column.saturating_sub(1);
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    pub fn tab(&mut self) {
        let columns = self.columns;
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.wrap_pending = false;
        let next_stop = ((buf.cursor.column / 8) + 1) * 8;
        buf.cursor.column = next_stop.min(columns - 1);
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    pub fn cursor_up(&mut self, count: usize) {
        let min_row = if self.origin_mode {
            self.active().scroll_top
        } else {
            0
        };
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.wrap_pending = false;
        buf.cursor.row = buf.cursor.row.saturating_sub(count).max(min_row);
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    pub fn cursor_down(&mut self, count: usize) {
        let max_row = if self.origin_mode {
            self.active().scroll_bottom
        } else {
            self.rows.saturating_sub(1)
        };
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.wrap_pending = false;
        buf.cursor.row = buf.cursor.row.saturating_add(count).min(max_row);
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    pub fn cursor_forward(&mut self, count: usize) {
        let columns = self.columns;
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.wrap_pending = false;
        buf.cursor.column = buf.cursor.column.saturating_add(count).min(columns - 1);
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    pub fn cursor_back(&mut self, count: usize) {
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        buf.wrap_pending = false;
        buf.cursor.column = buf.cursor.column.saturating_sub(count);
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    /// Absolute or origin-relative cursor address (CUP / HVP / VPA / CHA).
    ///
    /// When DECOM is on, `row` is relative to the top scroll margin and the
    /// result is clamped into the scroll region. Column is always absolute.
    pub fn set_cursor_position(&mut self, row: usize, column: usize) {
        let columns = self.columns;
        let (min_row, max_row, abs_row) = if self.origin_mode {
            let top = self.active().scroll_top;
            let bottom = self.active().scroll_bottom;
            let abs = top.saturating_add(row).min(bottom);
            (top, bottom, abs)
        } else {
            let last = self.rows.saturating_sub(1);
            (0, last, row.min(last))
        };
        let _ = (min_row, max_row);
        let old_cursor = self.active().cursor;
        let new_cursor = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            buf.cursor = Cursor {
                row: abs_row,
                column: column.min(columns.saturating_sub(1)),
            };
            buf.cursor
        };
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    /// DECSC (`ESC 7`): save cursor position, delayed wrap, and SGR pen.
    pub fn save_cursor(&mut self) {
        let buf = self.active_mut();
        buf.saved_cursor = SavedCursor {
            cursor: buf.cursor,
            wrap_pending: buf.wrap_pending,
            style: buf.style,
        };
    }

    /// DECRC (`ESC 8`): restore cursor position, delayed wrap, and SGR pen.
    ///
    /// Does not route through `set_cursor_position` (which clears wrap).
    pub fn restore_cursor(&mut self) {
        let rows = self.rows;
        let columns = self.columns;
        let old_cursor = self.cursor();
        let buf = self.active_mut();
        let saved = buf.saved_cursor;
        buf.cursor = Cursor {
            row: saved.cursor.row.min(rows.saturating_sub(1)),
            column: saved.cursor.column.min(columns.saturating_sub(1)),
        };
        buf.wrap_pending = saved.wrap_pending;
        buf.style = saved.style;
        let new_cursor = buf.cursor;
        self.mark_cursor_cells(old_cursor, new_cursor);
    }

    /// Insert `n` blank lines at the cursor row within the scroll region (CSI L / IL).
    ///
    /// Lines from the cursor through the bottom margin shift down; lines pushed past
    /// the bottom margin are discarded. New cells are spaces with the current SGR.
    /// Cursor position is unchanged. No-op when the cursor is outside the region.
    pub fn insert_lines(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let changed = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let top = buf.scroll_top;
            let bottom = buf.scroll_bottom;
            let row = buf.cursor.row;
            // Outside scroll region: no-op (VT100 / xterm).
            if row < top || row > bottom {
                false
            } else {
                let available = bottom - row + 1;
                let n = n.min(available);
                let style = buf.style;
                if n < available {
                    // Shift [row .. bottom-n] down by n → [row+n .. bottom].
                    buf.cells.scroll_down(row, bottom, n);
                    buf.wrapped.copy_within(row..bottom + 1 - n, row + n);
                }
                // Fill the inserted lines (cursor row .. cursor+n-1).
                let fill_start = row * columns;
                let fill_end = (row + n) * columns;
                erase_range(&mut buf.cells, fill_start, fill_end, style);
                let flags = buf.wrapped.len();
                buf.wrapped[row.min(flags)..(row + n).min(flags)].fill(false);
                true
            }
        };
        if changed {
            let row = self.active().cursor.row;
            let bottom = self.active().scroll_bottom;
            let n = {
                let available = bottom - row + 1;
                n.min(available)
            };
            self.damage.push_scroll(ScrollDamage {
                top: row,
                bottom,
                delta: -(n as i32),
            });
            for r in row..row + n {
                self.damage.mark_row_cells(r);
            }
            self.bump_epoch();
        }
    }

    /// Delete `n` lines at the cursor row within the scroll region (CSI M / DL).
    ///
    /// Lines below the cursor up through the bottom margin shift up; blank lines
    /// (spaces + current SGR) fill the bottom of the region. Cursor unchanged.
    /// No-op when the cursor is outside the region.
    pub fn delete_lines(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let changed = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let top = buf.scroll_top;
            let bottom = buf.scroll_bottom;
            let row = buf.cursor.row;
            if row < top || row > bottom {
                false
            } else {
                let available = bottom - row + 1;
                let n = n.min(available);
                let style = buf.style;
                if n < available {
                    // Shift [row+n .. bottom] up by n → [row .. bottom-n].
                    buf.cells.scroll_up(row, bottom, n);
                    buf.wrapped.copy_within(row + n..bottom + 1, row);
                }
                // Fill the vacated lines at the bottom of the region.
                let fill_start = (bottom + 1 - n) * columns;
                let fill_end = (bottom + 1) * columns;
                erase_range(&mut buf.cells, fill_start, fill_end, style);
                let flags = buf.wrapped.len();
                buf.wrapped[(bottom + 1 - n).min(flags)..(bottom + 1).min(flags)].fill(false);
                true
            }
        };
        if changed {
            let row = self.active().cursor.row;
            let bottom = self.active().scroll_bottom;
            let n = n.min(bottom - row + 1);
            self.damage.push_scroll(ScrollDamage {
                top: row,
                bottom,
                delta: n as i32,
            });
            for r in (bottom + 1 - n)..=bottom {
                self.damage.mark_row_cells(r);
            }
            self.bump_epoch();
        }
    }

    /// Scroll the DECSTBM region up by `n` lines (CSI S / SU).
    ///
    /// Lines leaving the top of the region are discarded (not scrollback).
    /// Blank lines (spaces + current SGR) are inserted at the bottom margin.
    /// Cursor position is unchanged. Unlike IL/DL, operates on the full region
    /// regardless of cursor row.
    pub fn scroll_up_region(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let changed = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let top = buf.scroll_top;
            let bottom = buf.scroll_bottom;
            let height = bottom - top + 1;
            let n = n.min(height);
            let style = buf.style;
            if n < height {
                // Shift [top+n .. bottom] up by n → [top .. bottom-n].
                buf.cells.scroll_up(top, bottom, n);
            }
            // Fill vacated lines at the bottom of the region.
            let fill_start = (bottom + 1 - n) * columns;
            let fill_end = (bottom + 1) * columns;
            erase_range(&mut buf.cells, fill_start, fill_end, style);
            true
        };
        if changed {
            let top = self.active().scroll_top;
            let bottom = self.active().scroll_bottom;
            let height = bottom - top + 1;
            let n = n.min(height);
            self.damage.push_scroll(ScrollDamage {
                top,
                bottom,
                delta: n as i32,
            });
            for r in (bottom + 1 - n)..=bottom {
                self.damage.mark_row_cells(r);
            }
            self.bump_epoch();
        }
    }

    /// Scroll the DECSTBM region down by `n` lines (CSI T / SD).
    ///
    /// Lines leaving the bottom of the region are discarded. Blank lines
    /// (spaces + current SGR) are inserted at the top margin. Cursor unchanged.
    /// Operates on the full region regardless of cursor row.
    pub fn scroll_down_region(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let changed = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let top = buf.scroll_top;
            let bottom = buf.scroll_bottom;
            let height = bottom - top + 1;
            let n = n.min(height);
            let style = buf.style;
            if n < height {
                // Shift [top .. bottom-n] down by n → [top+n .. bottom].
                buf.cells.scroll_down(top, bottom, n);
            }
            // Fill vacated lines at the top of the region.
            let fill_start = top * columns;
            let fill_end = (top + n) * columns;
            erase_range(&mut buf.cells, fill_start, fill_end, style);
            true
        };
        if changed {
            let top = self.active().scroll_top;
            let bottom = self.active().scroll_bottom;
            let height = bottom - top + 1;
            let n = n.min(height);
            self.damage.push_scroll(ScrollDamage {
                top,
                bottom,
                delta: -(n as i32),
            });
            for r in top..top + n {
                self.damage.mark_row_cells(r);
            }
            self.bump_epoch();
        }
    }

    /// Insert `n` blank characters at the cursor within the current line (CSI @ / ICH).
    ///
    /// Cells from the cursor through the end of the row shift right; cells pushed past
    /// the right edge are discarded. New cells are spaces with the current SGR.
    /// Cursor position is unchanged. `n` is clamped to the remaining columns.
    ///
    /// Wide pairs (ADR-0004): starting on a continuation snaps to the lead; after
    /// the shift, orphaned wide halves on the row are healed.
    pub fn insert_chars(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let row = {
            let (buf, clusters) = self.active_with_clusters_mut();
            buf.wrap_pending = false;
            let mut col = buf.cursor.column;
            // Inserting mid-wide: treat as insert at the lead so the pair is not split.
            let row_start = buf.cursor.row * columns;
            if col < columns && buf.cells[row_start + col].wide_cont && col > 0 {
                col -= 1;
                buf.cursor.column = col;
            }
            let available = columns.saturating_sub(col);
            if available == 0 {
                return;
            }
            let n = n.min(available);
            let style = buf.style;
            if n < available {
                // Shift [col .. columns-n) right by n → [col+n .. columns).
                let src_start = row_start + col;
                let src_end = row_start + columns - n;
                let dst = row_start + col + n;
                buf.cells.copy_within(src_start..src_end, dst);
            }
            // Fill the inserted span (cursor col .. col+n-1).
            erase_range(&mut buf.cells, row_start + col, row_start + col + n, style);
            heal_wide_pairs_in_row(&mut buf.cells, row_start, columns, clusters);
            buf.cursor.row
        };
        self.damage.mark_row_cells(row);
        self.bump_epoch();
        self.collect_clusters_if_needed();
    }

    /// Delete `n` characters at the cursor within the current line (CSI P / DCH).
    ///
    /// Cells to the right of the deleted span shift left; the vacated tail of the row
    /// is filled with spaces + current SGR. Cursor unchanged. `n` is clamped to the
    /// remaining columns.
    ///
    /// Wide pairs (ADR-0004): the delete span is expanded to whole lead+cont pairs;
    /// the row is healed after the shift.
    pub fn delete_chars(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let row = {
            let (buf, clusters) = self.active_with_clusters_mut();
            buf.wrap_pending = false;
            let col = buf.cursor.column;
            let available = columns.saturating_sub(col);
            if available == 0 {
                return;
            }
            let n = n.min(available);
            let row_start = buf.cursor.row * columns;
            let style = buf.style;
            let (start, end) =
                expand_cell_span_for_wide(&buf.cells, row_start, columns, col, col + n);
            let n = end - start;
            if n == 0 {
                return;
            }
            if start + n < columns {
                // Shift [end .. columns) left → [start ..).
                let src_start = row_start + end;
                let src_end = row_start + columns;
                let dst = row_start + start;
                buf.cells.copy_within(src_start..src_end, dst);
            }
            // Fill the vacated cells at the end of the row.
            erase_range(
                &mut buf.cells,
                row_start + columns - n,
                row_start + columns,
                style,
            );
            heal_wide_pairs_in_row(&mut buf.cells, row_start, columns, clusters);
            buf.cursor.row
        };
        self.damage.mark_row_cells(row);
        self.bump_epoch();
        self.collect_clusters_if_needed();
    }

    /// Erase `n` characters from the cursor without shifting (CSI X / ECH).
    ///
    /// Replaces cells with spaces + current SGR. Cursor unchanged. `n` is clamped
    /// to the remaining columns on the row.
    ///
    /// Wide pairs (ADR-0004): erase expands to whole pairs; row is healed after.
    pub fn erase_chars(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let columns = self.columns;
        let (row, start, end) = {
            let (buf, clusters) = self.active_with_clusters_mut();
            buf.wrap_pending = false;
            let col = buf.cursor.column;
            let available = columns.saturating_sub(col);
            if available == 0 {
                return;
            }
            let n = n.min(available);
            let row = buf.cursor.row;
            let row_start = row * columns;
            let style = buf.style;
            let (start, end) =
                expand_cell_span_for_wide(&buf.cells, row_start, columns, col, col + n);
            erase_range(&mut buf.cells, row_start + start, row_start + end, style);
            heal_wide_pairs_in_row(&mut buf.cells, row_start, columns, clusters);
            (row, start, end)
        };
        for col in start..end {
            self.damage.mark_cell(row, col);
        }
        self.bump_epoch();
        self.collect_clusters_if_needed();
    }

    /// Soft terminal reset / DECSTR (`CSI ! p`).
    ///
    /// Resets the active buffer's scroll region to the full screen, SGR to
    /// default, clears pending wrap, **DECOM off**, **DECAWM on**. **Preserves**
    /// grid content and cursor position (unlike RIS / `ESC c`). Does not touch
    /// alt-screen membership or scrollback.
    ///
    /// Emulator-owned flags (mouse, paste, focus, DECTCEM) are handled by the
    /// emulator's DECSTR path.
    pub fn soft_reset(&mut self) {
        let rows = self.rows;
        self.origin_mode = false;
        self.autowrap = true;
        self.active_hyperlink = None;
        let buf = self.active_mut();
        buf.scroll_top = 0;
        buf.scroll_bottom = rows.saturating_sub(1);
        buf.style = Style::default();
        buf.wrap_pending = false;
    }
    /// Hard-ish reset subset for RIS (`ESC c`) after any alt leave.
    ///
    /// Choice: full xterm RIS wipes almost all terminal state;
    /// Prismattyc implements a useful classic subset that unsticks modes after TUIs:
    /// - full scroll region + cursor home + clear wrap
    /// - DECOM off, DECAWM on
    /// - SGR default
    /// - erase active display **and** primary scrollback (`ED 3` / `CSI 3 J`)
    ///
    /// Soft reset (`CSI ! p`) still preserves scrollback and grid content.
    /// Caller (emulator) should leave alt first (1049 restore path) and clear
    /// emulator-owned flags (bracketed paste, mouse, focus, DECTCEM).
    ///
    /// Not a full xterm private-mode table (keyboard protocol modes, etc.).
    pub fn ris_reset(&mut self) {
        self.origin_mode = false;
        self.autowrap = true;
        self.active_hyperlink = None;
        self.reset_scroll_region();
        self.set_style(Style::default());
        // ED 3: clear visible grid + drop primary scrollback (xterm-class RIS).
        self.erase_display(3);
        self.hyperlinks.clear();
    }
    pub fn erase_line(&mut self, mode: u16) {
        let columns = self.columns;
        let mutated = {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let row_start = buf.cursor.row * columns;
            let cursor = row_start + buf.cursor.column;
            let row_end = row_start + columns;
            let style = buf.style;
            let row = buf.cursor.row;
            match mode {
                0 => {
                    erase_range(&mut buf.cells, cursor, row_end, style);
                    // The tail is gone, so the line no longer runs on.
                    if let Some(flag) = buf.wrapped.get_mut(row) {
                        *flag = false;
                    }
                    true
                }
                1 => {
                    erase_range(&mut buf.cells, row_start, cursor + 1, style);
                    true
                }
                2 => {
                    erase_range(&mut buf.cells, row_start, row_end, style);
                    if let Some(flag) = buf.wrapped.get_mut(row) {
                        *flag = false;
                    }
                    true
                }
                _ => false,
            }
        };
        if mutated {
            let row = self.active().cursor.row;
            self.damage.mark_row_cells(row);
            self.bump_epoch();
        }
    }

    pub fn erase_display(&mut self, mode: u16) {
        let columns = self.columns;
        {
            let buf = self.active_mut();
            buf.wrap_pending = false;
            let cursor = buf.cursor.row * columns + buf.cursor.column;
            let style = buf.style;
            let len = buf.cells.len();
            let row = buf.cursor.row;
            match mode {
                0 => {
                    erase_range(&mut buf.cells, cursor, len, style);
                    let from = row.min(buf.wrapped.len());
                    buf.wrapped[from..].fill(false);
                }
                1 => erase_range(&mut buf.cells, 0, cursor + 1, style),
                // Mode 2: clear viewport only. Mode 3: same viewport clear, then
                // drop scrollback (xterm ED3 / CSI 3 J). Both mutate cells → epoch.
                2 | 3 => {
                    erase_range(&mut buf.cells, 0, len, style);
                    buf.wrapped.fill(false);
                }
                _ => return,
            }
        }
        if mode == 3 {
            self.scrollback = VecDeque::new();
            self.scrollback_wrapped = VecDeque::new();
            self.collect_clusters();
        }
        if mode == 2 || mode == 3 {
            self.full_clears += 1;
        }
        // Mode 0/1/2/3 all rewrite cells; mode 3 also cleared scrollback.
        if mode == 2 || mode == 3 {
            self.damage.mark_all();
        } else {
            let cursor = self.active().cursor;
            match mode {
                0 => {
                    for row in cursor.row..self.rows {
                        self.damage.mark_row_cells(row);
                    }
                }
                1 => {
                    for row in 0..=cursor.row {
                        self.damage.mark_row_cells(row);
                    }
                }
                _ => {}
            }
        }
        self.bump_epoch();
    }

    /// Inclusive column bounds of the word (same character-class run) at `row`/`col`.
    ///
    /// Returns `None` if the row is missing. Empty/space runs expand across spaces.
    pub fn word_range_at(&self, row: usize, col: usize) -> Option<CellRange> {
        self.word_range_at_view(0, row, col)
    }

    /// Like [`word_range_at`] but for a scrolled history window (`scroll_offset`).
    pub fn word_range_at_view(
        &self,
        scroll_offset: usize,
        row: usize,
        col: usize,
    ) -> Option<CellRange> {
        if row >= self.rows || self.columns == 0 {
            return None;
        }
        let col = col.min(self.columns - 1);
        let class = char_class(self.view_cell(scroll_offset, row, col).character);
        let mut start = col;
        let mut end = col;
        while start > 0
            && char_class(self.view_cell(scroll_offset, row, start - 1).character) == class
        {
            start -= 1;
        }
        while end + 1 < self.columns
            && char_class(self.view_cell(scroll_offset, row, end + 1).character) == class
        {
            end += 1;
        }
        Some(CellRange {
            start_row: row,
            start_col: start,
            end_row: row,
            end_col: end,
        })
    }

    /// Full viewport row as a selection range (triple-click).
    pub fn line_range_at(&self, row: usize) -> Option<CellRange> {
        if row >= self.rows || self.columns == 0 {
            return None;
        }
        Some(CellRange {
            start_row: row,
            start_col: 0,
            end_row: row,
            end_col: self.columns - 1,
        })
    }

    /// Inclusive column span of `range` on a single viewport `row` (stream order).
    ///
    /// Returns `None` if the row is outside the range, the grid is empty, or the
    /// range starts past the last column of the grid for this row (wholly OOB —
    /// do not invent a cell by clamping `start_col` inward).
    pub fn selection_col_span(&self, range: CellRange, row: usize) -> Option<(usize, usize)> {
        let range = range.normalized();
        if self.columns == 0 || row < range.start_row || row > range.end_row || row >= self.rows {
            return None;
        }
        let last = self.columns - 1;
        let (start_col, end_col) = if range.start_row == range.end_row {
            (range.start_col, range.end_col)
        } else if row == range.start_row {
            (range.start_col, last)
        } else if row == range.end_row {
            (0, range.end_col)
        } else {
            (0, last)
        };
        // Wholly past the grid: no cells to cover/extract.
        if start_col > last {
            return None;
        }
        let end_col = end_col.min(last);
        // Normal in-range clamp when end is past the grid or inverted after min.
        let start_col = start_col.min(end_col);
        Some((start_col, end_col))
    }

    /// Last column within `start_col..=end_col` whose character is not a space.
    ///
    /// Used for multi-row selection paint trim (ADR-0001 D-H2 visual polish).
    /// Returns `None` when every cell in the span is a space (or the row is missing).
    /// Whether host selection chrome should inverse-paint this cell.
    ///
    /// Geometry matches stream-order [`CellRange::contains`]. For **multi-row**
    /// ranges, trailing space cells on each line are not highlighted (visual EOL
    /// trim). Single-row ranges stay geometric so a deliberate drag over spaces
    /// still paints. All-space lines still highlight their full span so blank
    /// lines in a multi-line select remain visible.
    pub fn selection_covers_cell(&self, range: CellRange, row: usize, col: usize) -> bool {
        self.selection_covers_cell_view(0, range, row, col)
    }

    /// Like [`selection_covers_cell`] for a scrolled history window.
    pub fn selection_covers_cell_view(
        &self,
        scroll_offset: usize,
        range: CellRange,
        row: usize,
        col: usize,
    ) -> bool {
        let range = range.normalized();
        if !range.contains(row, col) {
            return false;
        }
        if range.start_row == range.end_row {
            return true;
        }
        let Some((start_col, end_col)) = self.selection_col_span(range, row) else {
            return false;
        };
        if col < start_col || col > end_col {
            return false;
        }
        match self.last_non_space_col_view(scroll_offset, row, start_col, end_col) {
            Some(last) => col <= last,
            None => true,
        }
    }

    fn last_non_space_col_view(
        &self,
        scroll_offset: usize,
        row: usize,
        start_col: usize,
        end_col: usize,
    ) -> Option<usize> {
        if row >= self.rows || self.columns == 0 {
            return None;
        }
        let end_col = end_col.min(self.columns - 1);
        let start_col = start_col.min(end_col);
        (start_col..=end_col).rfind(|&c| {
            let cell = self.view_cell(scroll_offset, row, c);
            !cell.wide_cont && cell.character != ' '
        })
    }

    /// Full viewport as a selection range (select-all).
    pub fn viewport_range(&self) -> Option<CellRange> {
        if self.rows == 0 || self.columns == 0 {
            return None;
        }
        Some(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: self.rows - 1,
            end_col: self.columns - 1,
        })
    }

    /// Plain-text extract for a viewport cell range (selection copy).
    /// Trailing spaces on each line are trimmed; lines joined with `\n`.
    pub fn extract_text(&self, range: CellRange) -> String {
        self.extract_text_view(0, range)
    }

    /// Like [`extract_text`] but reads characters from a scrolled history window.
    ///
    /// `range` uses **viewport** row coordinates relative to `scroll_offset`.
    pub fn extract_text_view(&self, scroll_offset: usize, range: CellRange) -> String {
        let range = range.normalized();
        let mut lines = Vec::new();
        for row in range.start_row..=range.end_row.min(self.rows.saturating_sub(1)) {
            let Some((start_col, end_col)) = self.selection_col_span(range, row) else {
                continue;
            };
            let mut line = String::new();
            for col in start_col..=end_col {
                let cell = self.view_cell(scroll_offset, row, col);
                cell.write_grapheme_into(&mut line);
            }
            while line.ends_with(' ') {
                line.pop();
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    /// Extract plain text for a selection stored in **absolute** history rows.
    pub fn extract_text_abs(&self, range: CellRange) -> String {
        let range = range.normalized();
        let max_abs = self.history_line_count().saturating_sub(1);
        let mut lines = Vec::new();
        for abs_row in range.start_row..=range.end_row.min(max_abs) {
            let Some((start_col, end_col)) = self.selection_col_span_abs(range, abs_row) else {
                continue;
            };
            let mut line = String::new();
            for col in start_col..=end_col {
                self.history_cell(abs_row, col)
                    .write_grapheme_into(&mut line);
            }
            // A wrapped line continues into the next row, so it keeps both its
            // trailing spaces (the wrap may fall on the word separator) and its
            // join to the following row. Only a real line end is trimmed.
            let wrapped = self.history_line_wrapped(abs_row) && abs_row < range.end_row;
            if !wrapped {
                while line.ends_with(' ') {
                    line.pop();
                }
            }
            lines.push((line, wrapped));
        }
        let mut text = String::new();
        for (index, (line, wrapped)) in lines.iter().enumerate() {
            text.push_str(line);
            if index + 1 < lines.len() && !wrapped {
                text.push('\n');
            }
        }
        text
    }

    /// Column span of an absolute-row range on one absolute history line.
    fn selection_col_span_abs(&self, range: CellRange, abs_row: usize) -> Option<(usize, usize)> {
        let range = range.normalized();
        if self.columns == 0 || abs_row < range.start_row || abs_row > range.end_row {
            return None;
        }
        let last = self.columns - 1;
        let (start_col, end_col) = if range.start_row == range.end_row {
            (range.start_col, range.end_col)
        } else if abs_row == range.start_row {
            (range.start_col, last)
        } else if abs_row == range.end_row {
            (0, range.end_col)
        } else {
            (0, last)
        };
        if start_col > last {
            return None;
        }
        let end_col = end_col.min(last);
        let start_col = start_col.min(end_col);
        Some((start_col, end_col))
    }

    /// Selection chrome for absolute-row ranges painted into a viewport window.
    pub fn selection_covers_abs_at_view(
        &self,
        scroll_offset: usize,
        abs_range: CellRange,
        view_row: usize,
        col: usize,
    ) -> bool {
        let abs_row = self.abs_row_at_view(scroll_offset, view_row);
        let range = abs_range.normalized();
        if !range.contains(abs_row, col) {
            return false;
        }
        if range.start_row == range.end_row {
            return true;
        }
        let Some((start_col, end_col)) = self.selection_col_span_abs(range, abs_row) else {
            return false;
        };
        if col < start_col || col > end_col {
            return false;
        }
        match self.last_non_space_col_abs(abs_row, start_col, end_col) {
            Some(last) => col <= last,
            None => true,
        }
    }

    fn last_non_space_col_abs(
        &self,
        abs_row: usize,
        start_col: usize,
        end_col: usize,
    ) -> Option<usize> {
        if self.columns == 0 {
            return None;
        }
        let end_col = end_col.min(self.columns - 1);
        let start_col = start_col.min(end_col);
        (start_col..=end_col).rfind(|&c| {
            let cell = self.history_cell(abs_row, c);
            !cell.wide_cont && cell.character != ' '
        })
    }
}

fn erase_range(cells: &mut CellGrid, start: usize, end: usize, style: Style) {
    let blank = Cell::glyph(' ', style);
    let end = end.min(cells.len());
    let start = start.min(end);
    cells.fill_range(start..end, blank);
}

/// Expand `[start, end)` on a row so it does not bisect a wide pair (ADR-0004).
///
/// `start`/`end` are column offsets within the row (`end` exclusive).
fn expand_cell_span_for_wide(
    cells: &CellGrid,
    row_start: usize,
    columns: usize,
    start: usize,
    end: usize,
) -> (usize, usize) {
    let mut start = start.min(columns);
    let mut end = end.min(columns);
    if start >= end {
        return (start, end);
    }
    if cells[row_start + start].wide_cont && start > 0 {
        start -= 1;
    }
    // If the last cell in the span is a wide lead, include its continuation.
    if end > start && end <= columns {
        let last = end - 1;
        if !cells[row_start + last].wide_cont
            && cells[row_start + last].display_width() == 2
            && last + 1 < columns
        {
            end = last + 2;
        }
    }
    (start, end.min(columns))
}

/// Remove a cluster scalar that makes a narrow base occupy two columns.
fn strip_cluster_widening(cell: &mut Cell, clusters: &mut ClusterStore) {
    let Some(index) = clusters.get(cell.cluster).iter().position(|&mark| {
        mark == '\u{fe0f}' || (is_regional_indicator(cell.character) && is_regional_indicator(mark))
    }) else {
        return;
    };
    let old = clusters.get(cell.cluster);
    let mut marks = ['\0'; MAX_COMBINING_MARKS];
    marks[..index].copy_from_slice(&old[..index]);
    marks[index..old.len() - 1].copy_from_slice(&old[index + 1..]);
    let len = old.len() - 1;
    clusters.set(cell, &marks[..len]);
}

/// Remove orphaned wide halves after insert/delete/erase on a single row.
fn heal_wide_pairs_in_row(
    cells: &mut CellGrid,
    row_start: usize,
    columns: usize,
    clusters: &mut ClusterStore,
) {
    if columns == 0 {
        return;
    }
    let mut col = 0;
    while col < columns {
        let i = row_start + col;
        if cells[i].wide_cont {
            // Continuation must follow a width-2 lead.
            let ok = col > 0 && !cells[i - 1].wide_cont && cells[i - 1].display_width() == 2;
            if !ok {
                cells[i] = Cell::default();
            }
            col += 1;
            continue;
        }
        if cells[i].display_width() == 2 {
            if col + 1 < columns && cells[row_start + col + 1].wide_cont {
                col += 2;
                continue;
            }
            // A cluster-widened lead can occupy one cell at the final column.
            if char_display_width(cells[i].character) == 1 {
                if col + 1 >= columns {
                    col += 1;
                    continue;
                }
                // DCH can move a cluster-widened lead left without its
                // continuation. Restore the pair when the next cell is blank.
                let next = &cells[row_start + col + 1];
                if !next.wide_cont && next.character == ' ' && next.cluster == 0 {
                    let lead = cells[i];
                    cells[row_start + col + 1] =
                        Cell::wide_continuation(lead.style).with_hyperlink(lead.hyperlink);
                    col += 2;
                    continue;
                }
                // Do not discard the lead when the next cell is occupied.
                // Demote the cluster so its text matches its one-cell layout.
                strip_cluster_widening(&mut cells[i], clusters);
                col += 1;
                continue;
            }
            // Orphan intrinsically wide lead: clear (cannot display safely in
            // one cell).
            cells[i] = Cell::default();
            col += 1;
            continue;
        }
        col += 1;
    }
}

fn resize_buffer(
    old: &GridBuffer,
    old_cols: usize,
    old_rows: usize,
    new_cols: usize,
    new_rows: usize,
) -> GridBuffer {
    let mut next = GridBuffer::new(new_cols, new_rows);
    let copy_rows = old_rows.min(new_rows);
    let copy_cols = old_cols.min(new_cols);
    for row in 0..copy_rows {
        for col in 0..copy_cols {
            let src = row * old_cols + col;
            let dst = row * new_cols + col;
            next.cells[dst] = old.cells[src];
        }
        restore_padded_wide_pair(
            &mut next.cells[row * new_cols..(row + 1) * new_cols],
            old_cols,
        );
    }
    next.cursor = Cursor {
        row: old.cursor.row.min(new_rows - 1),
        column: old.cursor.column.min(new_cols - 1),
    };
    next.saved_cursor = SavedCursor {
        cursor: Cursor {
            row: old.saved_cursor.cursor.row.min(new_rows - 1),
            column: old.saved_cursor.cursor.column.min(new_cols - 1),
        },
        wrap_pending: old.saved_cursor.wrap_pending,
        style: old.saved_cursor.style,
    };
    next.style = old.style;
    next.wrap_pending = false;
    // Default full-screen region; caller restores clamped DECSTBM if desired.
    next.scroll_top = 0;
    next.scroll_bottom = new_rows.saturating_sub(1);
    next
}

/// Clamp prior DECSTBM margins into a resized buffer.
///
/// - If the old region was the **full old screen** (`top==0 && bottom==old_last`),
///   keep full-screen after resize (grow/shrink with `new_rows`).
/// - Otherwise clamp explicit margins; one-line/inverted → full screen
///   (matches [`Screen::set_scroll_region`] policy).
fn apply_clamped_scroll_region(
    buf: &mut GridBuffer,
    top: usize,
    bottom: usize,
    old_rows: usize,
    new_rows: usize,
) {
    let old_last = old_rows.saturating_sub(1);
    let new_last = new_rows.saturating_sub(1);
    if top == 0 && bottom == old_last {
        buf.scroll_top = 0;
        buf.scroll_bottom = new_last;
        return;
    }
    let top0 = top.min(new_last);
    let bottom0 = bottom.min(new_last);
    if top0 < bottom0 {
        buf.scroll_top = top0;
        buf.scroll_bottom = bottom0;
    } else {
        buf.scroll_top = 0;
        buf.scroll_bottom = new_last;
    }
}

/// Pad or truncate a scrollback row so its length matches the live grid width.
fn resize_scrollback_line(line: &mut Vec<Cell>, new_cols: usize) {
    // Enqueue and import retain full-width rows, including trailing blanks.
    // Every resize updates all history rows, so this is their previous width.
    let old_cols = line.len();
    if old_cols == new_cols {
        return;
    }
    // Replace the allocation so narrowing also releases excess capacity.
    let mut resized = vec![Cell::default(); new_cols];
    let keep = old_cols.min(new_cols);
    resized[..keep].copy_from_slice(&line[..keep]);
    restore_padded_wide_pair(&mut resized, old_cols);
    *line = resized;
}

/// A wide lead at the old right edge occupied one cell after clipping.
/// When padding creates its second column, restore the continuation there.
fn restore_padded_wide_pair(row: &mut [Cell], old_columns: usize) {
    if old_columns == 0 || old_columns >= row.len() {
        return;
    }
    let lead = row[old_columns - 1];
    if !lead.wide_cont && lead.display_width() == 2 {
        row[old_columns] = Cell::wide_continuation(lead.style).with_hyperlink(lead.hyperlink);
    }
}

/// Max plain-text bytes accepted for OSC 52 copy (0.1.0 policy).
pub const OSC52_MAX_PLAIN_BYTES: usize = 64 * 1024;

/// Encode plain text as a host OSC 52 clipboard write (base64 payload).
///
/// Policy (0.1.0 + post-ship polish):
/// - allow printable Unicode scalar values, tab, and newline only;
/// - reject empty or **whitespace-only** text (spaces/tabs/newlines with no
///   other scalars — avoids copying a blank drag as a screenful of `\n`);
/// - reject oversize, C0 (except tab/newline), DEL, and C1 (U+0080–U+009F).
///
/// Returns `None` → host no-op (no clipboard write).
pub fn encode_osc52_clipboard(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || text.len() > OSC52_MAX_PLAIN_BYTES {
        return None;
    }
    // Blank grid selection extracts as "" or "\n\n…" after per-line space trim.
    if !text.chars().any(|c| !c.is_whitespace()) {
        return None;
    }
    for c in text.chars() {
        let u = c as u32;
        let forbidden = matches!(u, 0x00..=0x08 | 0x0b..=0x1f | 0x7f | 0x80..=0x9f);
        if forbidden {
            return None;
        }
    }
    let encoded = base64_encode(text.as_bytes());
    let mut out = Vec::with_capacity(encoded.len() + 16);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(encoded.as_bytes());
    out.extend_from_slice(b"\x07");
    Some(out)
}

/// Clear alt cells/cursor/region; install `pen` as the active SGR.
fn clear_alt_buffer(alt: &mut GridBuffer, rows: usize, pen: Style) {
    alt.cells.fill(Cell::default());
    alt.cursor = Cursor::default();
    alt.saved_cursor = SavedCursor::default();
    alt.wrap_pending = false;
    alt.scroll_top = 0;
    alt.scroll_bottom = rows.saturating_sub(1);
    alt.style = pen;
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from);
        let b2 = chunk.get(2).copied().map(u32::from);
        let n = (b0 << 16) | (b1.unwrap_or(0) << 8) | b2.unwrap_or(0);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if b1.is_some() {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if b2.is_some() {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn export_style(style: Style) -> StyleStateV1 {
    StyleStateV1 {
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        underline_style: style.underline_style,
        inverse: style.inverse,
        foreground: style.foreground,
        background: style.background,
        underline_color: style.underline_color,
    }
}

fn import_style(style: StyleStateV1) -> Style {
    Style {
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        underline_style: style.underline_style,
        inverse: style.inverse,
        foreground: style.foreground,
        background: style.background,
        underline_color: style.underline_color,
    }
}

fn export_cursor(cursor: Cursor) -> CursorStateV1 {
    CursorStateV1 {
        row: cursor.row as u64,
        column: cursor.column as u64,
    }
}

fn import_cursor(cursor: CursorStateV1) -> Result<Cursor, ScreenStateError> {
    Ok(Cursor {
        row: usize::try_from(cursor.row).map_err(|_| ScreenStateError::SizeOverflow)?,
        column: usize::try_from(cursor.column).map_err(|_| ScreenStateError::SizeOverflow)?,
    })
}

fn style_index(styles: &mut Vec<StyleStateV1>, style: Style) -> u32 {
    let state = export_style(style);
    if let Some(index) = styles.iter().position(|existing| *existing == state) {
        return index as u32;
    }
    let index = styles.len() as u32;
    styles.push(state);
    index
}

fn push_style_run(runs: &mut Vec<StyleRunStateV1>, style: u32) {
    if let Some(last) = runs.last_mut() {
        if last.style == style {
            last.len = last.len.saturating_add(1);
            return;
        }
    }
    runs.push(StyleRunStateV1 { len: 1, style });
}

fn export_row(
    cells: &[Cell],
    wrapped: bool,
    styles: &mut Vec<StyleStateV1>,
    clusters: &ClusterStore,
) -> RowStateV1 {
    let mut text = String::new();
    let mut style_runs = Vec::new();
    let mut hyperlinks = Vec::new();
    for (column, cell) in cells.iter().enumerate() {
        if cell.wrap_padding {
            text.push(' ');
        } else if !cell.wide_cont {
            CellView { cell, clusters }.write_grapheme_into(&mut text);
        }
        push_style_run(&mut style_runs, style_index(styles, cell.style));
        if let Some(handle) = cell.hyperlink {
            hyperlinks.push(HyperlinkCellStateV1 {
                column: column as u32,
                handle: handle.0,
            });
        }
    }
    RowStateV1 {
        text,
        styles: style_runs,
        hyperlinks,
        wrapped,
        wrap_padding: cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.wrap_padding.then_some(i as u32))
            .collect(),
    }
}

fn export_grid_buffer(
    buffer: &GridBuffer,
    columns: usize,
    styles: &mut Vec<StyleStateV1>,
    clusters: &ClusterStore,
) -> GridBufferStateV1 {
    let rows = buffer
        .cells
        .chunks(columns)
        .zip(buffer.wrapped.iter())
        .map(|(cells, wrapped)| export_row(cells, *wrapped, styles, clusters))
        .collect();
    GridBufferStateV1 {
        rows,
        cursor: export_cursor(buffer.cursor),
        saved_cursor: SavedCursorStateV1 {
            cursor: export_cursor(buffer.saved_cursor.cursor),
            wrap_pending: buffer.saved_cursor.wrap_pending,
            style: style_index(styles, buffer.saved_cursor.style),
        },
        style: style_index(styles, buffer.style),
        wrap_pending: buffer.wrap_pending,
        scroll_top: buffer.scroll_top as u64,
        scroll_bottom: buffer.scroll_bottom as u64,
    }
}

fn graphemes_from_text(text: &str) -> Vec<(char, Vec<char>)> {
    let mut out: Vec<(char, Vec<char>)> = Vec::new();
    for character in text.chars() {
        let attaches = out.last().is_some_and(|(base, combining)| {
            char_display_width(character) == 0
                || is_emoji_modifier(character)
                || (combining
                    .last()
                    .is_some_and(|last| is_zwj(*last) && is_zwj_joinable(character)))
                || (combining.is_empty()
                    && is_regional_indicator(*base)
                    && is_regional_indicator(character))
        });
        if attaches {
            out.last_mut().expect("checked above").1.push(character);
        } else {
            out.push((character, Vec::new()));
        }
    }
    out
}

fn decode_row_text(
    text: &str,
    columns: usize,
    clusters: &mut ClusterStore,
) -> Result<Vec<Cell>, ScreenStateError> {
    let mut cells = vec![Cell::default(); columns];
    let mut column = 0usize;
    for (character, combining) in graphemes_from_text(text) {
        let intrinsic_width = grapheme_display_width(character, &combining);
        let width = if intrinsic_width == 2 && column.checked_add(1) == Some(columns) {
            // Live placement keeps a wide cluster narrow when it lands in the
            // final column. Preserve that representation on import.
            1
        } else {
            intrinsic_width
        };
        if width == 0 || column.checked_add(width).is_none_or(|end| end > columns) {
            return Err(ScreenStateError::Invalid(
                "row text does not fit the grid".into(),
            ));
        }
        if combining.len() > MAX_COMBINING_MARKS {
            return Err(ScreenStateError::Invalid(
                "row grapheme has too many combining scalars".into(),
            ));
        }
        let mut cell = Cell::glyph(character, Style::default());
        clusters.set(&mut cell, &combining);
        cells[column] = cell;
        if width == 2 {
            cells[column + 1] = Cell::wide_continuation(Style::default());
        }
        column += width;
    }
    Ok(cells)
}

fn import_row(
    row: &RowStateV1,
    columns: usize,
    styles: &[StyleStateV1],
    hyperlink_count: usize,
    clusters: &mut ClusterStore,
) -> Result<Vec<Cell>, ScreenStateError> {
    let mut cells = decode_row_text(&row.text, columns, clusters)?;
    let mut position = 0usize;
    for run in &row.styles {
        let style = styles
            .get(usize::try_from(run.style).map_err(|_| ScreenStateError::SizeOverflow)?)
            .copied()
            .ok_or_else(|| ScreenStateError::Invalid("row style index is out of range".into()))?;
        let length = usize::try_from(run.len).map_err(|_| ScreenStateError::SizeOverflow)?;
        let end = position
            .checked_add(length)
            .ok_or(ScreenStateError::SizeOverflow)?;
        if length == 0 || end > columns {
            return Err(ScreenStateError::Invalid(
                "row style runs do not match dimensions".into(),
            ));
        }
        for cell in &mut cells[position..end] {
            cell.style = import_style(style);
        }
        position = end;
    }
    if position != columns {
        return Err(ScreenStateError::Invalid(
            "row style runs do not cover the row".into(),
        ));
    }
    let mut linked = vec![false; columns];
    for link in &row.hyperlinks {
        let column = usize::try_from(link.column).map_err(|_| ScreenStateError::SizeOverflow)?;
        let handle = usize::try_from(link.handle).map_err(|_| ScreenStateError::SizeOverflow)?;
        if column >= columns || handle >= hyperlink_count || linked[column] {
            return Err(ScreenStateError::Invalid(
                "row hyperlink entry is invalid".into(),
            ));
        }
        linked[column] = true;
        cells[column].hyperlink = Some(HyperlinkId(link.handle));
    }
    for &column in &row.wrap_padding {
        let cell = cells
            .get_mut(column as usize)
            .ok_or_else(|| ScreenStateError::Invalid("wrap padding is outside the row".into()))?;
        if *cell != Cell::default() {
            return Err(ScreenStateError::Invalid(
                "wrap padding must be an empty cell".into(),
            ));
        }
        cell.wrap_padding = true;
    }
    Ok(cells)
}

fn import_grid_buffer(
    buffer: &GridBufferStateV1,
    columns: usize,
    rows: usize,
    styles: &[StyleStateV1],
    hyperlink_count: usize,
    clusters: &mut ClusterStore,
) -> Result<GridBuffer, ScreenStateError> {
    let cursor = import_cursor(buffer.cursor)?;
    let saved_cursor = import_cursor(buffer.saved_cursor.cursor)?;
    if cursor.row >= rows
        || cursor.column >= columns
        || saved_cursor.row >= rows
        || saved_cursor.column >= columns
    {
        return Err(ScreenStateError::Invalid(
            "cursor is outside the grid".into(),
        ));
    }
    let scroll_top =
        usize::try_from(buffer.scroll_top).map_err(|_| ScreenStateError::SizeOverflow)?;
    let scroll_bottom =
        usize::try_from(buffer.scroll_bottom).map_err(|_| ScreenStateError::SizeOverflow)?;
    if scroll_top > scroll_bottom || scroll_bottom >= rows {
        return Err(ScreenStateError::Invalid(
            "scroll margins are outside the grid".into(),
        ));
    }
    let rows_state = buffer
        .rows
        .iter()
        .map(|row| import_row(row, columns, styles, hyperlink_count, clusters))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GridBuffer {
        cells: CellGrid::from_flat(rows_state.into_iter().flatten().collect(), columns),
        wrapped: buffer.rows.iter().map(|row| row.wrapped).collect(),
        cursor,
        saved_cursor: SavedCursor {
            cursor: saved_cursor,
            wrap_pending: buffer.saved_cursor.wrap_pending,
            style: import_style(styles[buffer.saved_cursor.style as usize]),
        },
        style: import_style(styles[buffer.style as usize]),
        wrap_pending: buffer.wrap_pending,
        scroll_top,
        scroll_bottom,
    })
}

fn validate_grid_buffer(
    buffer: &GridBufferStateV1,
    columns: usize,
    rows: usize,
    styles: &[StyleStateV1],
    hyperlink_count: usize,
    clusters: &mut ClusterStore,
) -> Result<(), ScreenStateError> {
    if buffer.rows.len() != rows {
        return Err(ScreenStateError::Invalid(
            "grid row count does not match dimensions".into(),
        ));
    }
    let cursor = import_cursor(buffer.cursor)?;
    let saved = import_cursor(buffer.saved_cursor.cursor)?;
    if cursor.row >= rows
        || cursor.column >= columns
        || saved.row >= rows
        || saved.column >= columns
    {
        return Err(ScreenStateError::Invalid(
            "cursor is outside the grid".into(),
        ));
    }
    let top = usize::try_from(buffer.scroll_top).map_err(|_| ScreenStateError::SizeOverflow)?;
    let bottom =
        usize::try_from(buffer.scroll_bottom).map_err(|_| ScreenStateError::SizeOverflow)?;
    if top > bottom || bottom >= rows {
        return Err(ScreenStateError::Invalid(
            "scroll margins are outside the grid".into(),
        ));
    }
    for index in [buffer.style, buffer.saved_cursor.style] {
        if usize::try_from(index).map_or(true, |index| index >= styles.len()) {
            return Err(ScreenStateError::Invalid(
                "buffer style index is invalid".into(),
            ));
        }
    }
    for row in &buffer.rows {
        import_row(row, columns, styles, hyperlink_count, clusters)?;
    }
    Ok(())
}

fn validate_screen_state(state: &ScreenStateV1) -> Result<(), ScreenStateError> {
    let columns = usize::try_from(state.columns).map_err(|_| ScreenStateError::SizeOverflow)?;
    let rows = usize::try_from(state.rows).map_err(|_| ScreenStateError::SizeOverflow)?;
    if columns == 0 || rows == 0 || state.columns > u64::from(u32::MAX) {
        return Err(ScreenStateError::Invalid(
            "dimensions must be non-zero and fit row runs".into(),
        ));
    }
    let max_scrollback =
        usize::try_from(state.max_scrollback).map_err(|_| ScreenStateError::SizeOverflow)?;
    if state.scrollback.len() > max_scrollback {
        return Err(ScreenStateError::Invalid(
            "scrollback length exceeds its configured bound".into(),
        ));
    }
    if state.styles.len() > u32::MAX as usize {
        return Err(ScreenStateError::Invalid("style table is too large".into()));
    }
    let visible_cells = columns
        .checked_mul(rows)
        .ok_or(ScreenStateError::SizeOverflow)?;
    let history_cells = state
        .scrollback
        .len()
        .checked_mul(columns)
        .ok_or(ScreenStateError::SizeOverflow)?;
    if visible_cells
        .checked_mul(if state.alternate.is_some() { 2 } else { 1 })
        .and_then(|n| n.checked_add(history_cells))
        .is_none_or(|n| n > 64 * 1024 * 1024)
    {
        return Err(ScreenStateError::Invalid(
            "screen state is too large".into(),
        ));
    }
    if state.hyperlinks.len() > MAX_HYPERLINKS {
        return Err(ScreenStateError::Invalid("too many hyperlinks".into()));
    }
    for link in &state.hyperlinks {
        if link.uri.is_empty()
            || link.uri.len() > MAX_HYPERLINK_URI_BYTES
            || link.uri.chars().any(|ch| ch <= '\u{1f}' || ch == '\u{7f}')
            || link.id.as_ref().is_some_and(|id| {
                id.len() > MAX_HYPERLINK_ID_BYTES
                    || id.chars().any(|ch| ch <= '\u{1f}' || ch == '\u{7f}')
            })
        {
            return Err(ScreenStateError::Invalid(
                "hyperlink table entry is invalid".into(),
            ));
        }
    }
    if state
        .active_hyperlink
        .is_some_and(|handle| usize::try_from(handle).map_or(true, |i| i >= state.hyperlinks.len()))
    {
        return Err(ScreenStateError::Invalid(
            "active hyperlink is out of range".into(),
        ));
    }
    let mut clusters = ClusterStore::default();
    validate_grid_buffer(
        &state.primary,
        columns,
        rows,
        &state.styles,
        state.hyperlinks.len(),
        &mut clusters,
    )?;
    if let Some(alternate) = &state.alternate {
        validate_grid_buffer(
            alternate,
            columns,
            rows,
            &state.styles,
            state.hyperlinks.len(),
            &mut clusters,
        )?;
    } else if state.alternate_active {
        return Err(ScreenStateError::Invalid(
            "alternate screen is active without a buffer".into(),
        ));
    }
    for row in &state.scrollback {
        import_row(
            row,
            columns,
            &state.styles,
            state.hyperlinks.len(),
            &mut clusters,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_rejects_zero_sized_screen_state() {
        let mut state = Screen::new(80, 24, 128).export_state();
        state.columns = 0;
        assert!(matches!(
            Screen::import_state(state),
            Err(ScreenStateError::Invalid(message)) if message.contains("dimensions")
        ));
    }

    #[test]
    fn import_rejects_scrollback_beyond_configured_bound() {
        let mut state = Screen::new(80, 24, 128).export_state();
        state.max_scrollback = 0;
        state.scrollback.push(state.primary.rows[0].clone());
        assert!(matches!(
            Screen::import_state(state),
            Err(ScreenStateError::Invalid(message)) if message.contains("scrollback length")
        ));
    }

    fn text(screen: &Screen, row: usize) -> String {
        screen
            .row(row)
            .unwrap()
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    /// PT-122: a line that ended because autowrap ran out of columns is one
    /// logical line. Copy must not put a newline at the wrap point.
    #[test]
    fn copy_joins_soft_wrapped_rows_and_keeps_hard_newlines() {
        let mut screen = Screen::new(4, 3, 10);
        // "abcdefg" wraps at column 4; then an explicit newline; then "hi".
        for character in "abcdefg".chars() {
            screen.put_char(character);
        }
        screen.carriage_return();
        screen.line_feed();
        for character in "hi".chars() {
            screen.put_char(character);
        }
        let all = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 2,
            end_col: 3,
        };
        assert_eq!(screen.extract_text_abs(all), "abcdefg\nhi");
    }

    /// The wrap can land on the space between two words. Trimming trailing
    /// spaces on a wrapped row would glue those words together.
    #[test]
    fn copy_keeps_the_space_a_wrap_lands_on() {
        let mut screen = Screen::new(4, 2, 10);
        // "abc def": the space fills column 3, so "def" wraps to the next row.
        for character in "abc def".chars() {
            screen.put_char(character);
        }
        let all = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 3,
        };
        assert_eq!(screen.extract_text_abs(all), "abc def");
    }

    /// The flag has to survive the row being evicted into scrollback, which is
    /// where a long paragraph actually lives by the time it is selected.
    #[test]
    fn wrapped_flag_follows_a_row_into_scrollback() {
        let mut screen = Screen::new(4, 2, 10);
        for character in "abcdefghij".chars() {
            screen.put_char(character);
        }
        assert!(!screen.scrollback().is_empty(), "rows must have scrolled");
        assert!(
            screen.history_line_wrapped(0),
            "the evicted row ended in a wrap"
        );
        let rows = screen.history_line_count();
        let all = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: rows - 1,
            end_col: 3,
        };
        assert_eq!(screen.extract_text_abs(all), "abcdefghij");
    }

    /// Erasing to end of line ends the line; a stale flag would glue it to
    /// whatever is written on the next row.
    #[test]
    fn erase_line_clears_the_wrapped_flag() {
        let mut screen = Screen::new(4, 2, 10);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        screen.put_char('e');
        assert!(screen.history_line_wrapped(0));
        screen.set_cursor_position(0, 0);
        screen.erase_line(0);
        assert!(!screen.history_line_wrapped(0));
    }

    #[test]
    fn alt_history_is_opt_in_and_classic_view_stays_zero() {
        // Default off: alt scroll evicts rows to nowhere (real-terminal).
        let mut screen = Screen::new(3, 2, 10);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        for character in "abcdefghi".chars() {
            screen.put_char(character);
        }
        assert!(screen.alt_active());
        assert_eq!(screen.scrollback().len(), 0);
        assert_eq!(screen.max_view_scroll(), 0);

        // Opt-in: evicted alt rows feed history; classic view still refuses.
        let mut screen = Screen::new(3, 2, 10);
        screen.set_retain_alt_history(true);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        for character in "abcdefghi".chars() {
            screen.put_char(character);
        }
        assert!(screen.alt_active());
        assert!(screen.history_len() >= 1, "alt eviction reaches history");
        assert_eq!(
            screen.max_view_scroll(),
            0,
            "classic host view must still refuse alt history"
        );
        // The first evicted alt row is readable through the view.
        let depth = screen.history_len();
        let first: String = (0..3)
            .map(|col| screen.history_view_cell(depth, 0, col).character)
            .collect();
        assert_eq!(first, "abc");
        // scrolled_lines is primary-only: no cell-rect translation from alt.
        assert_eq!(screen.scrolled_lines(), 0);
    }

    #[test]
    fn wraps_only_when_the_next_character_arrives() {
        let mut screen = Screen::new(3, 2, 10);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        assert_eq!(text(&screen, 0), "abc");
        assert_eq!(text(&screen, 1), "d  ");
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 1 });
    }

    #[test]
    fn scrolling_is_bounded() {
        let mut screen = Screen::new(2, 1, 1);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        assert_eq!(screen.scrollback().len(), 1);
        let saved: String = screen.scrollback()[0]
            .iter()
            .map(|cell| cell.character)
            .collect();
        assert_eq!(saved, "cd");
        assert_eq!(text(&screen, 0), "ef");
    }

    #[test]
    fn default_style_has_no_underline_style() {
        assert_eq!(Style::default().underline_style, UnderlineStyle::None);
    }

    #[test]
    fn erase_line_uses_current_style() {
        let mut screen = Screen::new(4, 1, 0);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 1);
        let style = Style {
            bold: true,
            ..Style::default()
        };
        screen.set_style(style);
        screen.erase_line(0);
        assert_eq!(text(&screen, 0), "a   ");
        assert_eq!(screen.row(0).unwrap()[1].style, style);
    }

    #[test]
    fn insert_chars_shifts_right_and_fills_with_style() {
        let mut screen = Screen::new(6, 1, 0);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 2); // on 'c'
        let style = Style {
            bold: true,
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(style);
        screen.insert_chars(2);
        // "ab" + blanks + "cd" (ef pushed off)
        assert_eq!(text(&screen, 0), "ab  cd");
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        let row = screen.row(0).unwrap();
        assert_eq!(row[2].character, ' ');
        assert_eq!(row[2].style, style);
        assert_eq!(row[3].character, ' ');
        assert_eq!(row[3].style, style);
        // Unshifted prefix keeps prior style (default).
        assert_eq!(row[0].style, Style::default());
    }

    #[test]
    fn delete_chars_shifts_left_and_fills_tail() {
        let mut screen = Screen::new(6, 1, 0);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 1); // on 'b'
        let style = Style {
            underline: true,
            ..Style::default()
        };
        screen.set_style(style);
        screen.delete_chars(2); // delete 'b','c' → "adef" + blanks
        assert_eq!(text(&screen, 0), "adef  ");
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 1 });
        let row = screen.row(0).unwrap();
        assert_eq!(row[4].character, ' ');
        assert_eq!(row[4].style, style);
        assert_eq!(row[5].style, style);
    }

    #[test]
    fn erase_chars_clears_without_shifting() {
        let mut screen = Screen::new(6, 1, 0);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 2); // on 'c'
        let style = Style {
            italic: true,
            ..Style::default()
        };
        screen.set_style(style);
        screen.erase_chars(2); // erase 'c','d' only
        assert_eq!(text(&screen, 0), "ab  ef");
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        let row = screen.row(0).unwrap();
        assert_eq!(row[2].style, style);
        assert_eq!(row[3].style, style);
        assert_eq!(row[4].character, 'e'); // not shifted
    }

    #[test]
    fn insert_delete_erase_chars_clamp_to_line_end() {
        let mut screen = Screen::new(4, 1, 0);
        for character in "wxyz".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 2);
        screen.insert_chars(100);
        assert_eq!(text(&screen, 0), "wx  "); // remaining span filled

        let mut screen = Screen::new(4, 1, 0);
        for character in "wxyz".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 1);
        screen.delete_chars(100);
        assert_eq!(text(&screen, 0), "w   ");

        let mut screen = Screen::new(4, 1, 0);
        for character in "wxyz".chars() {
            screen.put_char(character);
        }
        screen.set_cursor_position(0, 1);
        screen.erase_chars(100);
        assert_eq!(text(&screen, 0), "w   ");
    }

    #[test]
    fn insert_chars_does_not_affect_other_rows() {
        let mut screen = Screen::new(4, 2, 0);
        for c in "abcd".chars() {
            screen.put_char(c);
        }
        screen.carriage_return();
        screen.line_feed();
        for c in "efgh".chars() {
            screen.put_char(c);
        }
        screen.set_cursor_position(0, 1);
        screen.insert_chars(1);
        assert_eq!(text(&screen, 0), "a bc"); // d dropped
        assert_eq!(text(&screen, 1), "efgh");
    }

    #[test]
    fn line_feed_scroll_fill_uses_current_style() {
        // Full-screen scroll at bottom: blank row inherits current SGR.
        let mut screen = Screen::new(4, 2, 0);
        for c in "abcd".chars() {
            screen.put_char(c);
        }
        screen.carriage_return();
        screen.line_feed();
        for c in "efgh".chars() {
            screen.put_char(c);
        }
        let style = Style {
            bold: true,
            background: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(style);
        screen.line_feed(); // at bottom → scroll; new bottom row must use `style`
        assert_eq!(text(&screen, 0), "efgh");
        assert_eq!(text(&screen, 1), "    ");
        for cell in screen.row(1).unwrap() {
            assert_eq!(cell.character, ' ');
            assert_eq!(cell.style, style);
        }
    }

    #[test]
    fn margin_scroll_fill_uses_current_style() {
        // DECSTBM margin scroll fill also uses current SGR.
        let mut screen = Screen::new(2, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        let style = Style {
            underline: true,
            foreground: Color::Ansi(2),
            ..Style::default()
        };
        screen.set_style(style);
        screen.set_cursor_position(2, 0);
        screen.line_feed(); // scroll region; new bottom of region gets `style`
        assert_eq!(text(&screen, 0), "a "); // outside region unchanged
        assert_eq!(text(&screen, 3), "d ");
        assert_eq!(text(&screen, 1), "c "); // scrolled up
        assert_eq!(text(&screen, 2), "  "); // blank fill
        for cell in screen.row(2).unwrap() {
            assert_eq!(cell.character, ' ');
            assert_eq!(cell.style, style);
        }
    }

    #[test]
    fn erase_display_mode2_clears_viewport_keeps_scrollback() {
        // CSI 2 J: full viewport erase only; scrollback and epoch unchanged.
        let mut screen = Screen::new(2, 1, 4);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        assert_eq!(screen.scrollback().len(), 2);
        assert_eq!(text(&screen, 0), "ef");
        let epoch = screen.content_epoch();
        screen.erase_display(2);
        assert_eq!(text(&screen, 0), "  ");
        assert_eq!(
            screen.scrollback().len(),
            2,
            "mode 2 must not drop scrollback"
        );
        let saved: String = screen.scrollback()[1]
            .iter()
            .map(|cell| cell.character)
            .collect();
        assert_eq!(saved, "cd");
        assert!(
            screen.content_epoch() > epoch,
            "mode 2 must bump content_epoch (cell mutation)"
        );
    }

    #[test]
    fn erase_display_mode3_clears_viewport_and_scrollback() {
        // CSI 3 J / ED3: clear viewport like mode 2 and drop scrollback.
        let mut screen = Screen::new(2, 1, 4);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        assert_eq!(screen.scrollback().len(), 2);
        assert_eq!(text(&screen, 0), "ef");
        let epoch = screen.content_epoch();
        screen.erase_display(3);
        assert_eq!(text(&screen, 0), "  ");
        assert!(
            screen.scrollback().is_empty(),
            "mode 3 must clear scrollback"
        );
        assert!(
            screen.content_epoch() > epoch,
            "mode 3 must bump content_epoch"
        );
    }

    #[test]
    fn soft_reset_clears_style_region_wrap_keeps_grid_and_cursor() {
        let mut screen = Screen::new(4, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // homes to 0,0
        screen.set_cursor_position(2, 2);
        screen.set_style(Style {
            bold: true,
            foreground: Color::Ansi(1),
            ..Style::default()
        });
        // Fill last cell on a row to arm delayed autowrap.
        screen.set_cursor_position(0, 0);
        for ch in ['w', 'x', 'y', 'z'] {
            screen.put_char(ch);
        }
        // wrap_pending true at (0, 3); soft reset must clear it without moving cursor.
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 3 });
        screen.soft_reset();
        assert_eq!(screen.style(), Style::default());
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 3 });
        // Grid content preserved (region rows still hold prior glyphs).
        assert_eq!(text(&screen, 1).chars().next(), Some('b'));
        assert_eq!(text(&screen, 2).chars().next(), Some('c'));
        // Wrap cleared: next put overwrites last column instead of wrapping to next row.
        screen.put_char('Q');
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 3 });
        assert_eq!(text(&screen, 0).chars().nth(3), Some('Q'));
        assert_eq!(text(&screen, 1).chars().next(), Some('b'));
        // Scroll region is full again: LF at bottom scrolls entire screen.
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_cursor_position(3, 0);
        screen.line_feed();
        assert_eq!(text(&screen, 0).chars().next(), Some('b'));
    }

    #[test]
    fn ris_reset_homes_clears_display_style_and_scrollback() {
        let mut screen = Screen::new(4, 2, 8);
        // Build scrollback + styled content.
        for ch in "abcdefghij".chars() {
            screen.put_char(ch);
        }
        assert!(!screen.scrollback().is_empty());
        screen.set_style(Style {
            underline: true,
            background: Color::Ansi(4),
            ..Style::default()
        });
        screen.set_cursor_position(1, 2);
        screen.put_char('X');
        screen.set_scroll_region(1, 2);
        screen.set_cursor_position(1, 3);
        screen.ris_reset();
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 0 });
        assert_eq!(screen.style(), Style::default());
        assert_eq!(text(&screen, 0), "    ");
        assert_eq!(text(&screen, 1), "    ");
        assert!(
            screen.scrollback().is_empty(),
            "RIS wipes primary scrollback"
        );
        // Full region: fill bottom and LF scrolls whole screen.
        screen.set_cursor_position(1, 0);
        screen.put_char('z');
        screen.line_feed();
        assert_eq!(text(&screen, 0).chars().next(), Some('z'));
    }

    #[test]
    fn cursor_only_operations_damage_old_and_new_cells() {
        type CursorOperation = (&'static str, fn(&mut Screen), Cursor);
        let operations: &[CursorOperation] = &[
            ("backspace", Screen::backspace, Cursor { row: 2, column: 3 }),
            (
                "carriage return",
                Screen::carriage_return,
                Cursor { row: 2, column: 0 },
            ),
            ("tab", Screen::tab, Cursor { row: 2, column: 8 }),
            ("up", |s| s.cursor_up(1), Cursor { row: 1, column: 4 }),
            ("down", |s| s.cursor_down(1), Cursor { row: 3, column: 4 }),
            (
                "forward",
                |s| s.cursor_forward(1),
                Cursor { row: 2, column: 5 },
            ),
            ("back", |s| s.cursor_back(1), Cursor { row: 2, column: 3 }),
            (
                "restore",
                Screen::restore_cursor,
                Cursor { row: 0, column: 0 },
            ),
            (
                "reverse index",
                Screen::reverse_index,
                Cursor { row: 1, column: 4 },
            ),
        ];
        for (name, operation, expected) in operations {
            let mut screen = Screen::new(12, 5, 0);
            screen.set_cursor_position(2, 4);
            screen.take_damage();
            operation(&mut screen);
            assert_eq!(screen.cursor(), *expected, "{name}");
            let damage = screen.take_damage();
            assert!(damage.is_cell_dirty(2, 4), "{name}: old caret must clear");
            assert!(
                damage.is_cell_dirty(expected.row, expected.column),
                "{name}: new caret must paint"
            );
            assert_eq!(
                damage.dirty_cell_count(),
                2,
                "{name}: bounded cursor damage"
            );
        }
    }

    #[test]
    fn cursor_movement_is_clamped_to_the_grid() {
        let mut screen = Screen::new(4, 3, 0);
        screen.set_cursor_position(20, 20);
        screen.cursor_forward(5);
        screen.cursor_down(5);
        assert_eq!(screen.cursor(), Cursor { row: 2, column: 3 });
        screen.cursor_up(20);
        screen.cursor_back(20);
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 0 });
    }

    #[test]
    fn resize_clips_grid_without_panic() {
        let mut screen = Screen::new(4, 2, 0);
        for c in "abcd".chars() {
            screen.put_char(c);
        }
        screen.resize(2, 3);
        assert_eq!(screen.columns(), 2);
        assert_eq!(screen.rows(), 3);
        assert_eq!(text(&screen, 0), "ab");
    }

    /// live resize must keep DECSTBM margins (clamped), not force full screen.
    #[test]
    fn resize_preserves_decstbm_margins() {
        let mut screen = Screen::new(8, 6, 0);
        screen.set_scroll_region(2, 5); // 0-based rows 1..=4
        assert_eq!(screen.active().scroll_top, 1);
        assert_eq!(screen.active().scroll_bottom, 4);
        screen.resize(10, 6);
        assert_eq!(screen.active().scroll_top, 1);
        assert_eq!(screen.active().scroll_bottom, 4);
        // Shrink rows so bottom clamps; region still multi-line.
        screen.resize(10, 4);
        assert_eq!(screen.active().scroll_top, 1);
        assert_eq!(screen.active().scroll_bottom, 3);
    }

    /// when clamped margins become a single line, fall back to full screen.
    #[test]
    fn resize_invalidates_one_line_decstbm_to_full_screen() {
        let mut screen = Screen::new(4, 4, 0);
        screen.set_scroll_region(3, 4); // 0-based 2..=3
        screen.resize(4, 2); // last row index 1 → top0=1 bottom0=1 → full
        assert_eq!(screen.active().scroll_top, 0);
        assert_eq!(screen.active().scroll_bottom, 1);
    }

    // / dual-sign: default full-screen region must grow with row count.
    #[test]
    fn resize_full_screen_margins_grow_with_rows() {
        let mut screen = Screen::new(2, 2, 0);
        assert_eq!(screen.active().scroll_top, 0);
        assert_eq!(screen.active().scroll_bottom, 1);
        screen.resize(2, 3);
        assert_eq!(screen.active().scroll_top, 0);
        assert_eq!(
            screen.active().scroll_bottom,
            2,
            "full-screen bottom must track new last row"
        );
        // LF at new bottom must scroll (region is full 3 rows).
        screen.set_cursor_position(0, 0);
        screen.put_char('a');
        screen.set_cursor_position(1, 0);
        screen.put_char('b');
        screen.set_cursor_position(2, 0);
        screen.put_char('c');
        screen.line_feed();
        assert_eq!(text(&screen, 0), "b ");
        assert_eq!(text(&screen, 1), "c ");
        assert_eq!(text(&screen, 2), "  ");
    }

    /// scrollback lines must match new column width after resize.
    #[test]
    fn resize_rewrites_scrollback_line_widths() {
        let mut screen = Screen::new(2, 1, 1);
        for character in "abcdef".chars() {
            screen.put_char(character);
        }
        assert_eq!(screen.scrollback().len(), 1);
        assert_eq!(screen.scrollback()[0].len(), 2);
        screen.resize(5, 1);
        assert_eq!(screen.scrollback()[0].len(), 5);
        screen.resize(1, 1);
        assert_eq!(screen.scrollback()[0].len(), 1);
    }

    /// view_cell walks scrollback + primary; offset 0 is live grid.
    #[test]
    fn view_cell_offset_zero_matches_live_row() {
        let mut screen = Screen::new(4, 2, 10);
        for c in "abcd".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "efgh".chars() {
            screen.put_char(c);
        }
        assert_eq!(screen.view_cell(0, 0, 0).character, 'a');
        assert_eq!(screen.view_cell(0, 1, 0).character, 'e');
        assert_eq!(screen.max_view_scroll(), 0);
    }

    #[test]
    fn view_cell_scrolled_shows_scrollback_then_primary() {
        // 2-row screen; push one line into scrollback.
        let mut screen = Screen::new(2, 2, 10);
        for c in "ab".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "cd".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "ef".chars() {
            screen.put_char(c);
        }
        // scrollback: ["ab"], live: ["cd","ef"]
        assert_eq!(screen.scrollback().len(), 1);
        assert_eq!(screen.max_view_scroll(), 1);
        // offset 1: window ends 1 above live bottom → ["ab","cd"]
        assert_eq!(screen.view_cell(1, 0, 0).character, 'a');
        assert_eq!(screen.view_cell(1, 0, 1).character, 'b');
        assert_eq!(screen.view_cell(1, 1, 0).character, 'c');
        assert_eq!(screen.view_cell(1, 1, 1).character, 'd');
        // live still at offset 0
        assert_eq!(screen.view_cell(0, 0, 0).character, 'c');
        assert_eq!(screen.view_cell(0, 1, 0).character, 'e');
    }

    #[test]
    fn find_in_history_finds_and_wraps() {
        let mut screen = Screen::new(8, 2, 20);
        for c in "hello".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "world".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "hello!".chars() {
            screen.put_char(c);
        }
        let m = screen.find_in_history("hello", None).expect("first");
        assert_eq!(m.start_col, 0);
        let m2 = screen
            .find_in_history("hello", Some((m.abs_row, m.end_col)))
            .expect("second");
        assert!(m2.abs_row >= m.abs_row);
        assert!(screen.find_in_history("nope", None).is_none());
    }

    #[test]
    fn find_in_history_is_case_insensitive_by_default() {
        let mut screen = Screen::new(12, 2, 10);
        for c in "Hello World".chars() {
            screen.put_char(c);
        }
        let m = screen.find_in_history("hello", None).expect("ci");
        assert_eq!(m.start_col, 0);
        assert_eq!(m.end_col, 4);
        let m2 = screen.find_in_history("WORLD", None).expect("ci upper q");
        assert_eq!(m2.start_col, 6);
        // Sensitive path still requires exact case.
        assert!(screen.find_in_history_opts("hello", None, true).is_none());
        assert!(screen.find_in_history_opts("Hello", None, true).is_some());
    }

    #[test]
    fn find_in_history_rev_finds_previous_and_wraps() {
        let mut screen = Screen::new(8, 2, 20);
        for c in "aaa".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "bbb".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "aaa!".chars() {
            screen.put_char(c);
        }
        // Forward first → second match (later "aaa").
        let first = screen.find_in_history("aaa", None).expect("first");
        let second = screen
            .find_in_history("aaa", Some((first.abs_row, first.end_col)))
            .expect("second");
        assert!((second.abs_row, second.start_col) > (first.abs_row, first.start_col));
        // Reverse from second start → first.
        let prev = screen
            .find_in_history_rev("aaa", Some((second.abs_row, second.start_col)), false)
            .expect("prev");
        assert_eq!(
            (prev.abs_row, prev.start_col),
            (first.abs_row, first.start_col)
        );
        // Reverse again wraps to the later match.
        let wrap = screen
            .find_in_history_rev("aaa", Some((prev.abs_row, prev.start_col)), false)
            .expect("wrap");
        assert_eq!(
            (wrap.abs_row, wrap.start_col),
            (second.abs_row, second.start_col)
        );
        // Case-insensitive reverse.
        assert!(screen
            .find_in_history_rev("AAA", Some((second.abs_row, second.start_col)), false)
            .is_some());
        // Sole match: reverse wraps to itself.
        let only = screen.find_in_history("bbb", None).expect("bbb");
        let only_prev = screen
            .find_in_history_rev("bbb", Some((only.abs_row, only.start_col)), false)
            .expect("wrap sole");
        assert_eq!(
            (only_prev.abs_row, only_prev.start_col),
            (only.abs_row, only.start_col)
        );
    }

    #[test]
    fn history_match_rank_and_count() {
        let mut screen = Screen::new(12, 2, 20);
        for c in "foo".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "bar".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "FOO".chars() {
            screen.put_char(c);
        }
        assert_eq!(screen.count_history_matches("foo", false), 2);
        assert_eq!(screen.count_history_matches("foo", true), 1);
        let first = screen.find_in_history("foo", None).expect("first");
        assert_eq!(screen.history_match_rank("foo", first, false), Some((1, 2)));
        let second = screen
            .find_in_history("foo", Some((first.abs_row, first.end_col)))
            .expect("second");
        assert_eq!(
            screen.history_match_rank("foo", second, false),
            Some((2, 2))
        );
        assert!(screen
            .history_match_rank(
                "foo",
                HistoryMatch {
                    abs_row: 0,
                    start_col: 0,
                    end_col: 0
                },
                false
            )
            .is_none());
    }

    #[test]
    fn extract_text_view_reads_scrolled_history() {
        let mut screen = Screen::new(2, 2, 10);
        for c in "ab".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "cd".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "ef".chars() {
            screen.put_char(c);
        }
        // offset 1 shows ["ab","cd"]
        let range = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1,
        };
        assert_eq!(screen.extract_text_view(1, range), "ab");
        assert!(screen.selection_covers_cell_view(1, range, 0, 0));
        assert!(!screen.selection_covers_cell_view(1, range, 1, 0));
    }

    #[test]
    fn view_cell_alt_ignores_scroll_offset() {
        let mut screen = Screen::new(2, 2, 10);
        for c in "ab".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "cd".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        screen.put_char('Z');
        assert_eq!(screen.max_view_scroll(), 0);
        assert_eq!(screen.view_cell(5, 0, 0).character, 'Z');
    }

    #[test]
    fn word_range_expands_alphanumeric_run() {
        let mut screen = Screen::new(12, 1, 0);
        for c in "ab_cd-xy".chars() {
            screen.put_char(c);
        }
        // "ab_cd" is one word class run; '-' is Other.
        let r = screen.word_range_at(0, 2).expect("word");
        assert_eq!(r.start_col, 0);
        assert_eq!(r.end_col, 4);
        let dash = screen.word_range_at(0, 5).expect("dash");
        assert_eq!(dash.start_col, 5);
        assert_eq!(dash.end_col, 5);
    }

    #[test]
    fn line_range_spans_full_row() {
        let screen = Screen::new(8, 2, 0);
        let r = screen.line_range_at(1).expect("line");
        assert_eq!(r.start_row, 1);
        assert_eq!(r.start_col, 0);
        assert_eq!(r.end_col, 7);
    }

    #[test]
    fn click_only_selection_has_no_range() {
        let mut selection = Selection::default();
        selection.begin(1, 2);
        // Same cell update (mouse-up without drag) must not create a sticky range.
        selection.update(1, 2);
        selection.finish();
        assert!(!selection.dragged);
        assert!(selection.range().is_none());
    }

    #[test]
    fn drag_selection_retains_range_after_finish() {
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 3);
        assert!(selection.dragged);
        selection.finish();
        let range = selection.range().expect("dragged range");
        assert_eq!(range.start_row, 0);
        assert_eq!(range.start_col, 0);
        assert_eq!(range.end_row, 0);
        assert_eq!(range.end_col, 3);
    }

    #[test]
    fn begin_clears_prior_dragged_selection() {
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 5);
        selection.finish();
        assert!(selection.range().is_some());
        selection.begin(2, 1);
        assert!(selection.range().is_none());
        assert!(!selection.dragged);
    }

    /// After clear (e.g. mid-drag PTY output), `update` alone must not invent a
    /// range — host Drag should re-`begin`.
    #[test]
    fn update_without_begin_leaves_range_none() {
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.update(0, 4);
        assert!(selection.range().is_some());
        selection.clear();
        assert!(selection.anchor.is_none());
        assert!(!selection.active);
        assert!(!selection.dragged);
        // Stale free-end motion with no active gesture: no-op.
        selection.update(1, 3);
        assert!(selection.range().is_none());
        assert!(selection.anchor.is_none());
        assert!(!selection.dragged);
        // Restart (what host Drag does after clear) rebuilds a real range.
        selection.begin(1, 0);
        selection.dragged = true;
        selection.update(1, 5);
        let range = selection.range().expect("restarted range");
        assert_eq!(range.start_row, 1);
        assert_eq!(range.start_col, 0);
        assert_eq!(range.end_col, 5);
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut screen = Screen::new(4, 1, 0);
        for c in "host".chars() {
            screen.put_char(c);
        }
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        assert!(screen.alt_active());
        for c in "ALT!".chars() {
            screen.put_char(c);
        }
        assert_eq!(text(&screen, 0), "ALT!");
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert!(!screen.alt_active());
        assert_eq!(text(&screen, 0), "host");
    }

    /// DECSC/DECRC must restore delayed wrap (not only position).
    /// Fill last column → wrap pending → ESC 7 → CR LF → ESC 8 → next char wraps.
    #[test]
    fn decsc_restores_wrap_pending() {
        let mut screen = Screen::new(3, 2, 0);
        for c in "abc".chars() {
            screen.put_char(c);
        }
        // Cursor stays on last col with wrap_pending; next put would wrap.
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        screen.save_cursor();
        screen.carriage_return();
        screen.line_feed();
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 0 });
        screen.restore_cursor();
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        screen.put_char('X');
        // Without wrap restore, 'X' would overwrite last col → "abX".
        assert_eq!(text(&screen, 0), "abc");
        assert_eq!(text(&screen, 1), "X  ");
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 1 });
    }

    /// DECSC/DECRC must restore SGR pen with the cursor.
    #[test]
    fn decsc_restores_sgr_style() {
        let mut screen = Screen::new(4, 1, 0);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(red);
        screen.save_cursor();
        screen.set_style(Style::default());
        assert_eq!(screen.style(), Style::default());
        screen.restore_cursor();
        assert_eq!(screen.style(), red);
        screen.put_char('R');
        assert_eq!(screen.row(0).unwrap()[0].style, red);
        assert_eq!(screen.row(0).unwrap()[0].character, 'R');
    }

    /// CSI ? 1049 leave restores wrap_pending saved at enter (DECSC).
    #[test]
    fn mode1049_leave_restores_wrap_pending() {
        let mut screen = Screen::new(3, 2, 0);
        for c in "abc".chars() {
            screen.put_char(c);
        }
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        for c in "zz".chars() {
            screen.put_char(c);
        }
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        screen.put_char('X');
        assert_eq!(text(&screen, 0), "abc");
        assert_eq!(text(&screen, 1), "X  ");
    }

    // 1049: leave restores SGR pen saved at enter.
    #[test]
    fn mode1049_leave_restores_sgr_style() {
        let mut screen = Screen::new(4, 2, 0);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(red);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        // Alt pen is independent / cleared; primary saved red.
        screen.set_style(Style::default());
        screen.put_char('a');
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(screen.style(), red);
        screen.put_char('R');
        assert_eq!(screen.row(0).unwrap()[0].style, red);
    }

    #[test]
    fn mode47_preserves_alt_content_on_reenter() {
        let mut screen = Screen::new(4, 1, 0);
        screen.enter_alt_screen(AltScreenMode::Mode47);
        for c in "keep".chars() {
            screen.put_char(c);
        }
        screen.leave_alt_screen(AltScreenMode::Mode47);
        screen.enter_alt_screen(AltScreenMode::Mode47);
        assert_eq!(text(&screen, 0), "keep");
    }

    /// SGR pen is terminal-wide — alt enter must not reset to default.
    #[test]
    fn alt_enter_1049_preserves_sgr_pen_from_primary() {
        let mut screen = Screen::new(4, 2, 0);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(red);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(screen.style(), red, "alt pen must match primary at enter");
        screen.put_char('x');
        assert_eq!(
            screen.row(0).unwrap()[0].style,
            red,
            "glyph on alt must use carried pen"
        );
    }

    #[test]
    fn alt_enter_1047_preserves_sgr_pen_from_primary() {
        let mut screen = Screen::new(4, 2, 0);
        let bold = Style {
            bold: true,
            foreground: Color::Ansi(2),
            ..Style::default()
        };
        screen.set_style(bold);
        screen.enter_alt_screen(AltScreenMode::Mode1047);
        assert_eq!(screen.style(), bold);
        screen.put_char('y');
        assert_eq!(screen.row(0).unwrap()[0].style, bold);
    }

    #[test]
    fn alt_enter_mode47_adopts_primary_pen() {
        let mut screen = Screen::new(4, 2, 0);
        // Seed stale alt style, then soft-switch with a new primary pen.
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        screen.set_style(Style {
            foreground: Color::Ansi(4),
            ..Style::default()
        });
        screen.put_char('z');
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(red);
        // Mode 47 reuses prior alt cells after 1049 leave cleared them; pen must follow.
        screen.enter_alt_screen(AltScreenMode::Mode47);
        assert_eq!(screen.style(), red);
    }

    /// leave 47/1047 carries alt pen back to primary (not only enter).
    #[test]
    fn alt_leave_1047_carries_pen_back_to_primary() {
        let mut screen = Screen::new(4, 2, 0);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        let blue = Style {
            foreground: Color::Ansi(4),
            ..Style::default()
        };
        screen.set_style(red);
        screen.enter_alt_screen(AltScreenMode::Mode1047);
        assert_eq!(screen.style(), red);
        screen.set_style(blue);
        screen.leave_alt_screen(AltScreenMode::Mode1047);
        assert_eq!(
            screen.style(),
            blue,
            "1047 leave must keep terminal-wide pen on primary"
        );
        screen.put_char('p');
        assert_eq!(screen.row(0).unwrap()[0].style, blue);
    }

    #[test]
    fn alt_leave_mode47_carries_pen_back_to_primary() {
        let mut screen = Screen::new(4, 2, 0);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        let blue = Style {
            foreground: Color::Ansi(4),
            ..Style::default()
        };
        screen.set_style(red);
        screen.enter_alt_screen(AltScreenMode::Mode47);
        screen.set_style(blue);
        screen.leave_alt_screen(AltScreenMode::Mode47);
        assert_eq!(screen.style(), blue);
        // Re-enter 47: enter path copies primary pen onto alt.
        screen.enter_alt_screen(AltScreenMode::Mode47);
        assert_eq!(screen.style(), blue);
    }

    #[test]
    fn alt_leave_1049_still_restores_decsc_pen() {
        let mut screen = Screen::new(4, 2, 0);
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(red);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        screen.set_style(Style::default());
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(
            screen.style(),
            red,
            "1049 leave must restore DECSC pen, not leave-time alt pen"
        );
    }

    #[test]
    fn mode1049_clears_alt_on_enter() {
        let mut screen = Screen::new(4, 1, 0);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        for c in "gone".chars() {
            screen.put_char(c);
        }
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(text(&screen, 0), "    ");
    }

    /// 1049 leave clears alt so mode-47 re-enter does not show stale cells.
    #[test]
    fn mode1049_leave_clears_alt_for_mode47_reenter() {
        let mut screen = Screen::new(4, 1, 0);
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        for c in "stale".chars() {
            screen.put_char(c);
        }
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        screen.enter_alt_screen(AltScreenMode::Mode47);
        assert_eq!(text(&screen, 0), "    ");
    }

    /// 1047 leave also clears alt (enter already clears; leave closes the gap).
    #[test]
    fn mode1047_leave_clears_alt_for_mode47_reenter() {
        let mut screen = Screen::new(4, 1, 0);
        screen.enter_alt_screen(AltScreenMode::Mode1047);
        for c in "stale".chars() {
            screen.put_char(c);
        }
        screen.leave_alt_screen(AltScreenMode::Mode1047);
        screen.enter_alt_screen(AltScreenMode::Mode47);
        assert_eq!(text(&screen, 0), "    ");
    }

    #[test]
    fn scroll_up_region_within_decstbm_preserves_outside() {
        // 1-col × 4-row: a/b/c/d, region rows 1..=2 (DECSTBM 2;3). SU 1.
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
                                        // Cursor elsewhere must not affect region-wide SU.
        screen.set_cursor_position(0, 0);
        let epoch = screen.content_epoch();
        let scrolled_before = screen.scrolled_lines();
        screen.scroll_up_region(1);
        assert_eq!(text(&screen, 0), "a"); // above preserved
        assert_eq!(text(&screen, 1), "c"); // former bottom of region
        assert_eq!(text(&screen, 2), " "); // blank at bottom margin
        assert_eq!(text(&screen, 3), "d"); // below preserved
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 0 });
        assert!(screen.content_epoch() > epoch);
        assert_eq!(
            screen.scrolled_lines(),
            scrolled_before,
            "SU must not advance scrolled_lines / scrollback"
        );
        assert!(screen.scrollback().is_empty());
    }

    #[test]
    fn scroll_down_region_within_decstbm_preserves_outside() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(3, 0);
        let epoch = screen.content_epoch();
        screen.scroll_down_region(1);
        assert_eq!(text(&screen, 0), "a"); // above preserved
        assert_eq!(text(&screen, 1), " "); // blank at top margin
        assert_eq!(text(&screen, 2), "b"); // former top of region
        assert_eq!(text(&screen, 3), "d"); // below preserved (c discarded)
        assert_eq!(screen.cursor(), Cursor { row: 3, column: 0 });
        assert!(screen.content_epoch() > epoch);
    }

    #[test]
    fn scroll_up_region_fills_with_current_style() {
        let mut screen = Screen::new(2, 3, 0);
        for (row, ch) in ['a', 'b', 'c'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        let style = Style {
            bold: true,
            background: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(style);
        // Full-screen region by default.
        screen.scroll_up_region(1);
        assert_eq!(text(&screen, 0).chars().next(), Some('b'));
        assert_eq!(text(&screen, 1).chars().next(), Some('c'));
        assert_eq!(text(&screen, 2), "  ");
        for cell in screen.row(2).unwrap() {
            assert_eq!(cell.character, ' ');
            assert_eq!(cell.style, style);
        }
    }

    #[test]
    fn scroll_down_region_fills_with_current_style() {
        let mut screen = Screen::new(2, 3, 0);
        for (row, ch) in ['a', 'b', 'c'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        let style = Style {
            underline: true,
            foreground: Color::Ansi(2),
            ..Style::default()
        };
        screen.set_style(style);
        screen.scroll_down_region(1);
        assert_eq!(text(&screen, 0), "  ");
        for cell in screen.row(0).unwrap() {
            assert_eq!(cell.character, ' ');
            assert_eq!(cell.style, style);
        }
        assert_eq!(text(&screen, 1).chars().next(), Some('a'));
        assert_eq!(text(&screen, 2).chars().next(), Some('b'));
    }

    #[test]
    fn scroll_up_region_n_ge_height_blanks_region_only() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2, height 2
        screen.scroll_up_region(99);
        assert_eq!(text(&screen, 0), "a");
        assert_eq!(text(&screen, 1), " ");
        assert_eq!(text(&screen, 2), " ");
        assert_eq!(text(&screen, 3), "d");
    }

    #[test]
    fn scroll_region_zero_count_is_noop() {
        let mut screen = Screen::new(1, 3, 0);
        for (row, ch) in ['a', 'b', 'c'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        let epoch = screen.content_epoch();
        screen.scroll_up_region(0);
        screen.scroll_down_region(0);
        assert_eq!(text(&screen, 0), "a");
        assert_eq!(text(&screen, 1), "b");
        assert_eq!(text(&screen, 2), "c");
        assert_eq!(screen.content_epoch(), epoch);
    }

    #[test]
    fn scroll_region_limits_line_feed() {
        let mut screen = Screen::new(2, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // 1-based → rows 1..=2
        screen.set_cursor_position(2, 0);
        screen.line_feed();
        assert_eq!(text(&screen, 0).chars().next(), Some('a'));
    }

    #[test]
    fn insert_lines_within_decstbm_shifts_down_and_preserves_outside() {
        // 4 rows, region rows 1..=2 (1-based DECSTBM 2;3). Fill one col each.
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(1, 0); // top of region ('b')
        let epoch = screen.content_epoch();
        screen.insert_lines(1);
        // Outside region preserved; blank inserted at cursor; prior region lines shift.
        assert_eq!(text(&screen, 0), "a");
        assert_eq!(text(&screen, 1), " "); // inserted blank
        assert_eq!(text(&screen, 2), "b"); // former row 1
        assert_eq!(text(&screen, 3), "d"); // below region preserved (c dropped)
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 0 });
        assert!(screen.content_epoch() > epoch);
    }

    #[test]
    fn delete_lines_within_decstbm_shifts_up_and_preserves_outside() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(1, 0);
        let epoch = screen.content_epoch();
        screen.delete_lines(1);
        assert_eq!(text(&screen, 0), "a"); // above preserved
        assert_eq!(text(&screen, 1), "c"); // former bottom of region
        assert_eq!(text(&screen, 2), " "); // blank filled at bottom margin
        assert_eq!(text(&screen, 3), "d"); // below preserved
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 0 });
        assert!(screen.content_epoch() > epoch);
    }

    #[test]
    fn insert_delete_lines_outside_scroll_region_are_noop() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        let before: Vec<String> = (0..4).map(|r| text(&screen, r)).collect();
        let epoch = screen.content_epoch();
        // Above region
        screen.set_cursor_position(0, 0);
        screen.insert_lines(1);
        screen.delete_lines(1);
        // Below region
        screen.set_cursor_position(3, 0);
        screen.insert_lines(2);
        screen.delete_lines(2);
        let after: Vec<String> = (0..4).map(|r| text(&screen, r)).collect();
        assert_eq!(before, after);
        assert_eq!(screen.content_epoch(), epoch);
    }

    #[test]
    fn insert_lines_fills_with_current_style() {
        let mut screen = Screen::new(2, 3, 0);
        screen.set_style(Style {
            bold: true,
            foreground: Color::Ansi(1),
            ..Style::default()
        });
        for (row, ch) in ['a', 'b', 'c'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        // Full-screen region by default.
        screen.set_cursor_position(0, 0);
        screen.insert_lines(1);
        let row0 = screen.row(0).unwrap();
        assert_eq!(row0[0].character, ' ');
        assert!(row0[0].style.bold);
        assert_eq!(row0[0].style.foreground, Color::Ansi(1));
        assert_eq!(text(&screen, 1).chars().next(), Some('a'));
    }

    #[test]
    fn delete_lines_fills_bottom_with_current_style() {
        let mut screen = Screen::new(2, 3, 0);
        let style = Style {
            underline: true,
            background: Color::Ansi(4),
            ..Style::default()
        };
        screen.set_style(style);
        for (row, ch) in ['a', 'b', 'c'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_cursor_position(0, 0);
        screen.delete_lines(1);
        assert_eq!(text(&screen, 0).chars().next(), Some('b'));
        assert_eq!(text(&screen, 1).chars().next(), Some('c'));
        let bottom = screen.row(2).unwrap();
        assert_eq!(bottom[0].character, ' ');
        assert_eq!(bottom[0].style, style);
    }

    #[test]
    fn insert_delete_lines_clamp_count_to_region_tail() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 4); // rows 1..=3
        screen.set_cursor_position(2, 0); // mid-region at 'c'
        screen.insert_lines(99);
        assert_eq!(text(&screen, 0), "a");
        assert_eq!(text(&screen, 1), "b");
        assert_eq!(text(&screen, 2), " ");
        assert_eq!(text(&screen, 3), " ");

        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 4);
        screen.set_cursor_position(2, 0);
        screen.delete_lines(99);
        assert_eq!(text(&screen, 0), "a");
        assert_eq!(text(&screen, 1), "b");
        assert_eq!(text(&screen, 2), " ");
        assert_eq!(text(&screen, 3), " ");
    }

    #[test]
    fn scroll_region_invalid_margins_become_full_screen() {
        let mut screen = Screen::new(2, 4, 0);
        // top > bottom → full screen per matrix F5.
        screen.set_scroll_region(4, 2);
        // Fill and LF at bottom of full screen.
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_cursor_position(3, 0);
        screen.line_feed();
        // Full-screen scroll: top row leaves viewport (scrollback when enabled).
        assert_eq!(text(&screen, 0).chars().next(), Some('b'));
    }

    #[test]
    fn scroll_region_one_line_rejected_as_full_screen() {
        let mut screen = Screen::new(2, 4, 0);
        // VT100 minimum region height is two lines.
        screen.set_scroll_region(2, 2);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_cursor_position(3, 0);
        screen.line_feed();
        assert_eq!(text(&screen, 0).chars().next(), Some('b'));
    }

    #[test]
    fn scroll_region_lf_below_bottom_does_not_jump_into_region() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(3, 0); // below bottom margin
        screen.line_feed();
        // Stay at last row; do not jump to bottom margin or scroll region.
        assert_eq!(screen.cursor().row, 3);
        assert_eq!(text(&screen, 1), "b");
        assert_eq!(text(&screen, 2), "c");
        assert_eq!(text(&screen, 3), "d");
    }

    #[test]
    fn reset_scroll_region_homes_cursor() {
        let mut screen = Screen::new(4, 4, 0);
        screen.set_scroll_region(2, 3);
        screen.set_cursor_position(2, 2);
        screen.reset_scroll_region();
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 0 });
    }

    #[test]
    fn scroll_region_preserves_rows_outside_margins() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(2, 0);
        screen.line_feed(); // scroll region only
        assert_eq!(text(&screen, 0), "a"); // above region
        assert_eq!(text(&screen, 3), "d"); // below region
    }

    #[test]
    fn reverse_index_at_top_margin_scrolls_region_down_with_current_sgr() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // 1-based → rows 1..=2
        let style = Style {
            bold: true,
            background: Color::Ansi(1),
            ..Style::default()
        };
        screen.set_style(style);
        screen.set_cursor_position(1, 0); // top margin
        let epoch = screen.content_epoch();
        screen.reverse_index();
        // Region scrolled down: former row1 "b" → row2; new blank at top of region.
        assert_eq!(text(&screen, 0), "a"); // above region preserved
        assert_eq!(text(&screen, 1), " "); // blank inserted at top margin
        assert_eq!(text(&screen, 2), "b"); // previous top of region
        assert_eq!(text(&screen, 3), "d"); // below region preserved (c discarded)
        assert_eq!(
            screen.row(1).unwrap()[0].style,
            style,
            "fill uses current SGR"
        );
        assert_eq!(screen.cursor().row, 1, "cursor stays at top margin");
        assert!(screen.content_epoch() > epoch, "region scroll bumps epoch");
    }

    #[test]
    fn reverse_index_mid_region_moves_cursor_up() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(2, 0); // bottom of region
        let epoch = screen.content_epoch();
        screen.reverse_index();
        assert_eq!(screen.cursor().row, 1);
        assert_eq!(text(&screen, 1), "b");
        assert_eq!(text(&screen, 2), "c");
        assert_eq!(
            screen.content_epoch(),
            epoch,
            "cursor-only RI must not bump epoch"
        );
    }

    #[test]
    fn reverse_index_above_region_moves_up_without_scrolling() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(3, 4); // rows 2..=3
        screen.set_cursor_position(1, 0); // above region
        screen.reverse_index();
        assert_eq!(screen.cursor().row, 0);
        assert_eq!(text(&screen, 2), "c");
        assert_eq!(text(&screen, 3), "d");
        // At row 0, further RI is a no-op (clamp).
        screen.reverse_index();
        assert_eq!(screen.cursor().row, 0);
        assert_eq!(text(&screen, 0), "a");
    }

    #[test]
    fn reverse_index_below_region_moves_up_without_region_scroll() {
        let mut screen = Screen::new(1, 4, 0);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2
        screen.set_cursor_position(3, 0); // below bottom margin
        screen.reverse_index();
        // Normal single-step up; does not scroll the region.
        assert_eq!(screen.cursor().row, 2);
        assert_eq!(text(&screen, 1), "b");
        assert_eq!(text(&screen, 2), "c");
    }

    #[test]
    fn selection_does_not_mutate_cells() {
        let mut screen = Screen::new(4, 1, 0);
        for c in "abcd".chars() {
            screen.put_char(c);
        }
        let before = text(&screen, 0);
        let mut sel = Selection::default();
        sel.begin(0, 0);
        sel.update(0, 3);
        sel.finish();
        let _ = screen.extract_text(sel.range().unwrap());
        assert_eq!(text(&screen, 0), before);
    }

    #[test]
    fn selection_extract_and_osc52() {
        let mut screen = Screen::new(5, 2, 0);
        for c in "hello".chars() {
            screen.put_char(c);
        }
        screen.carriage_return();
        screen.line_feed();
        for c in "world".chars() {
            screen.put_char(c);
        }
        let mut sel = Selection::default();
        sel.begin(0, 0);
        sel.update(0, 4);
        sel.finish();
        let text = screen.extract_text(sel.range().unwrap());
        assert_eq!(text, "hello");
        let osc = encode_osc52_clipboard(&text).expect("valid plain text");
        assert!(osc.starts_with(b"\x1b]52;c;"));
        assert!(osc.ends_with(b"\x07"));
        assert!(encode_osc52_clipboard("bad\x1bpayload").is_none());
        assert!(encode_osc52_clipboard("").is_none());
        // C1 NEL (U+0085) must be rejected despite being multi-byte UTF-8.
        assert!(encode_osc52_clipboard("\u{0085}").is_none());
        assert!(encode_osc52_clipboard("a\u{009b}b").is_none());
        assert!(encode_osc52_clipboard("ok\tline\n").is_some());
        let max = "a".repeat(OSC52_MAX_PLAIN_BYTES);
        assert!(encode_osc52_clipboard(&max).is_some());
        assert!(encode_osc52_clipboard(&format!("{max}x")).is_none());
    }

    /// put_char and erase mutate cells and must advance content_epoch.
    #[test]
    fn keystroke_dirties_at_most_two_cells() {
        let mut screen = Screen::new(80, 24, 0);
        let _ = screen.take_damage();
        screen.put_char('x');
        let damage = screen.take_damage();
        assert!(
            damage.dirty_cell_count() <= 2,
            "one keystroke dirties old+new cursor cells, got {}",
            damage.dirty_cell_count()
        );
        assert!(damage.is_cell_dirty(0, 0));
        assert!(damage.scroll_events().is_empty());
    }

    #[test]
    fn newline_on_full_screen_records_one_scroll_row() {
        let mut screen = Screen::new(4, 3, 10);
        screen.set_cursor_position(2, 0);
        for _ in 0..4 {
            screen.put_char('x');
        }
        let _ = screen.take_damage();
        screen.line_feed();
        let damage = screen.take_damage();
        assert_eq!(damage.scroll_events().len(), 1, "one scroll-damage entry");
        assert_eq!(damage.scroll_events()[0].delta, 1);
        assert_eq!(damage.scroll_events()[0].top, 0);
        assert_eq!(damage.scroll_events()[0].bottom, 2);
    }

    #[test]
    fn full_clear_marks_every_row() {
        let mut screen = Screen::new(8, 5, 0);
        let _ = screen.take_damage();
        screen.erase_display(2);
        let damage = screen.take_damage();
        assert_eq!(damage.dirty_row_count(), 5);
        assert!(damage.dirty_rows().eq(0..5));
    }

    #[test]
    fn alt_switch_marks_all_rows() {
        let mut screen = Screen::new(4, 3, 0);
        let _ = screen.take_damage();
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(screen.take_damage().dirty_row_count(), 3);
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(screen.take_damage().dirty_row_count(), 3);
    }

    #[test]
    fn decstbm_and_resize_mark_all_rows() {
        let mut screen = Screen::new(4, 6, 0);
        let _ = screen.take_damage();
        screen.set_scroll_region(2, 5);
        assert_eq!(screen.take_damage().dirty_row_count(), 6);
        screen.resize(8, 4);
        assert_eq!(screen.take_damage().dirty_row_count(), 4);
    }

    #[test]
    fn screen_equality_ignores_damage() {
        let mut a = Screen::new(2, 2, 0);
        let mut b = Screen::new(2, 2, 0);
        let _ = a.take_damage();
        a.put_char('x');
        b.put_char('x');
        assert_eq!(a, b, "damage is transient and must not affect Screen eq");
    }

    #[test]
    fn apply_damage_write_then_insert_line_matches() {
        let mut live = Screen::new(4, 4, 0);
        let mut replica = live.clone();
        let _ = live.take_damage();
        live.put_char('A');
        live.insert_lines(1);
        let damage = live.take_damage();
        replica.apply_damage(&live, &damage);
        assert_eq!(
            replica.view_cell(0, 1, 0),
            live.view_cell(0, 1, 0),
            "glyph scrolled by IL must appear on the replica"
        );
        for row in 0..4 {
            for col in 0..4 {
                assert_eq!(
                    replica.view_cell(0, row, col),
                    live.view_cell(0, row, col),
                    "mismatch at ({row},{col})"
                );
            }
        }
    }

    #[test]
    fn content_epoch_bumps_on_put_char_and_erase() {
        let mut screen = Screen::new(4, 2, 0);
        let e0 = screen.content_epoch();
        screen.put_char('a');
        assert!(screen.content_epoch() > e0, "put_char must bump epoch");
        let e1 = screen.content_epoch();
        screen.erase_line(2);
        assert!(screen.content_epoch() > e1, "erase_line must bump epoch");
        let e2 = screen.content_epoch();
        screen.erase_display(2);
        assert!(
            screen.content_epoch() > e2,
            "erase_display(2) must bump epoch"
        );
        let e3 = screen.content_epoch();
        screen.put_char('b');
        screen.erase_chars(1);
        assert!(screen.content_epoch() > e3, "erase_chars must bump epoch");
    }

    #[test]
    fn content_epoch_bumps_on_margin_scroll_and_alt_roundtrip() {
        let mut screen = Screen::new(2, 4, 0);
        let e0 = screen.content_epoch();
        screen.set_scroll_region(2, 3);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_cursor_position(2, 0);
        screen.line_feed(); // margin scroll at bottom of region
        assert!(screen.content_epoch() > e0, "margin scroll must bump epoch");

        let e1 = screen.content_epoch();
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert!(
            screen.content_epoch() > e1,
            "enter+leave alt must bump epoch even if final alt_active is false"
        );
        assert!(!screen.alt_active());
    }

    #[test]
    fn osc52_rejects_controls_and_enforces_size_bounds() {
        // Exact 64 KiB of printable ASCII is allowed; +1 is not.
        let max = "a".repeat(OSC52_MAX_PLAIN_BYTES);
        assert!(encode_osc52_clipboard(&max).is_some());
        assert!(encode_osc52_clipboard(&(max + "a")).is_none());
        // C0 / DEL / C1 (including NEL U+0085) rejected; tab/newline/UTF-8 ok.
        assert!(encode_osc52_clipboard("bad\x1bpayload").is_none());
        assert!(encode_osc52_clipboard("del\u{007f}").is_none());
        assert!(encode_osc52_clipboard("\u{0085}").is_none());
        assert!(encode_osc52_clipboard("c1\u{009b}").is_none());
        assert!(encode_osc52_clipboard("ok\tline\n").is_some());
        assert!(encode_osc52_clipboard("café").is_some()); // printable Unicode
        assert!(encode_osc52_clipboard("").is_none());
        // Whitespace-only (blank drag) must not fire OSC 52.
        assert!(encode_osc52_clipboard("\n\n\n").is_none());
        assert!(encode_osc52_clipboard("   ").is_none());
        assert!(encode_osc52_clipboard(" \t \n ").is_none());
        assert!(encode_osc52_clipboard("  x  ").is_some()); // still has content
    }

    #[test]
    fn blank_grid_selection_extract_does_not_osc52() {
        let screen = Screen::new(8, 4, 0);
        // Leave grid blank; drag a multi-row empty region.
        let mut sel = Selection::default();
        sel.begin(0, 0);
        sel.update(3, 7);
        sel.finish();
        let text = screen.extract_text(sel.range().expect("dragged range"));
        assert!(
            encode_osc52_clipboard(&text).is_none(),
            "blank extract {text:?} must not emit OSC 52"
        );
    }

    #[test]
    fn content_epoch_bumps_on_margin_scroll_without_scrolled_lines() {
        let mut screen = Screen::new(1, 4, 10);
        for (row, ch) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            screen.set_cursor_position(row, 0);
            screen.put_char(*ch);
        }
        screen.set_scroll_region(2, 3); // rows 1..=2 (DECSTBM margin)
        let before_epoch = screen.content_epoch();
        let before_scrolled = screen.scrolled_lines();
        screen.set_cursor_position(2, 0);
        screen.line_feed(); // margin scroll — must not look like full-screen scroll
        assert_eq!(
            screen.scrolled_lines(),
            before_scrolled,
            "margin scroll must not advance scrolled_lines"
        );
        assert!(
            screen.content_epoch() > before_epoch,
            "margin scroll must bump content_epoch"
        );
    }

    #[test]
    fn content_epoch_bumps_on_same_chunk_alt_round_trip() {
        let mut screen = Screen::new(4, 1, 0);
        let before = screen.content_epoch();
        let before_alt = screen.alt_active();
        screen.enter_alt_screen(AltScreenMode::Mode1049);
        screen.leave_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(screen.alt_active(), before_alt);
        assert!(
            screen.content_epoch() > before,
            "enter+leave must advance epoch even when final alt flag matches start"
        );
    }

    #[test]
    fn cell_range_contains_stream_order() {
        let range = CellRange {
            start_row: 0,
            start_col: 2,
            end_row: 1,
            end_col: 1,
        };
        assert!(range.contains(0, 3));
        assert!(range.contains(1, 0));
        assert!(!range.contains(0, 1));
        assert!(!range.contains(1, 2));
    }

    #[test]
    fn extract_text_trims_trailing_spaces_per_line() {
        let mut screen = Screen::new(8, 2, 0);
        for c in "hi".chars() {
            screen.put_char(c);
        }
        screen.carriage_return();
        screen.line_feed();
        for c in "bye".chars() {
            screen.put_char(c);
        }
        let range = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 7,
        };
        assert_eq!(screen.extract_text(range), "hi\nbye");
    }

    #[test]
    fn multi_row_selection_visual_trim_skips_trailing_spaces() {
        let mut screen = Screen::new(8, 2, 0);
        for c in "ab".chars() {
            screen.put_char(c);
        }
        screen.carriage_return();
        screen.line_feed();
        for c in "cd".chars() {
            screen.put_char(c);
        }
        let range = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 7,
        };
        // Stream geometry includes trailing blanks; visual paint does not.
        assert!(range.contains(0, 5));
        assert!(!screen.selection_covers_cell(range, 0, 5));
        assert!(screen.selection_covers_cell(range, 0, 0));
        assert!(screen.selection_covers_cell(range, 0, 1));
        assert!(screen.selection_covers_cell(range, 1, 0));
        assert!(screen.selection_covers_cell(range, 1, 1));
        assert!(!screen.selection_covers_cell(range, 1, 4));
    }

    #[test]
    fn single_row_selection_covers_spaces_geometrically() {
        let screen = Screen::new(8, 1, 0);
        let range = CellRange {
            start_row: 0,
            start_col: 1,
            end_row: 0,
            end_col: 5,
        };
        // Deliberate single-row drag over blanks still paints.
        assert!(screen.selection_covers_cell(range, 0, 3));
    }

    #[test]
    fn oob_start_col_selection_col_span_is_none() {
        // Range wholly past last column must not invent a cell.
        let screen = Screen::new(4, 2, 0);
        let range = CellRange {
            start_row: 0,
            start_col: 10,
            end_row: 0,
            end_col: 12,
        };
        assert_eq!(screen.selection_col_span(range, 0), None);
        // Multi-row start past last col on the start row.
        let multi = CellRange {
            start_row: 0,
            start_col: 8,
            end_row: 1,
            end_col: 1,
        };
        assert_eq!(screen.selection_col_span(multi, 0), None);
        // End row still has a normal span from col 0.
        assert_eq!(screen.selection_col_span(multi, 1), Some((0, 1)));
    }

    #[test]
    fn oob_start_col_extract_is_empty_and_paint_cover_false() {
        let mut screen = Screen::new(4, 1, 0);
        for c in "abcd".chars() {
            screen.put_char(c);
        }
        let range = CellRange {
            start_row: 0,
            start_col: 10,
            end_row: 0,
            end_col: 15,
        };
        assert_eq!(screen.extract_text(range), "");
        // Must not claim any real cell is covered (old clamp would hit last col).
        for col in 0..4 {
            assert!(
                !screen.selection_covers_cell(range, 0, col),
                "col {col} must not be covered by wholly-OOB range"
            );
        }
        // In-range partial clamp still works: start in grid, end past last.
        let partial = CellRange {
            start_row: 0,
            start_col: 2,
            end_row: 0,
            end_col: 99,
        };
        assert_eq!(screen.selection_col_span(partial, 0), Some((2, 3)));
        assert_eq!(screen.extract_text(partial), "cd");
        assert!(screen.selection_covers_cell(partial, 0, 2));
        assert!(screen.selection_covers_cell(partial, 0, 3));
        assert!(!screen.selection_covers_cell(partial, 0, 1));
    }

    #[test]
    fn viewport_range_covers_full_grid() {
        let screen = Screen::new(4, 3, 0);
        let r = screen.viewport_range().expect("range");
        assert_eq!((r.start_row, r.start_col), (0, 0));
        assert_eq!((r.end_row, r.end_col), (2, 3));
        // Screen::new clamps zero dims to 1×1, so range is always Some after construction.
        let tiny = Screen::new(0, 0, 0);
        let tr = tiny.viewport_range().expect("clamped 1x1");
        assert_eq!((tr.end_row, tr.end_col), (0, 0));
    }

    /// ADR-0004: fullwidth CJK occupies two cells; cursor advances by 2.
    #[test]
    fn put_char_wide_cjk_uses_two_columns() {
        let mut screen = Screen::new(8, 1, 0);
        // U+4E2D CJK UNIFIED IDEOGRAPH-4E2D (中) is width 2.
        screen.put_char('中');
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, '中');
        assert!(!row[0].wide_cont);
        assert!(row[1].wide_cont);
        assert_eq!(
            screen.extract_text(CellRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 1,
            }),
            "中"
        );
    }

    #[test]
    fn put_char_narrow_still_advances_one() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('a');
        assert_eq!(screen.cursor().column, 1);
        assert!(!screen.row(0).unwrap()[0].wide_cont);
    }

    #[test]
    fn put_char_combining_mark_attaches_to_previous_base() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('a');
        // Combining grave accent (U+0300) is width 0.
        screen.put_char('\u{0300}');
        assert_eq!(screen.cursor().column, 1);
        let cell = screen.view_cell(0, 0, 0);
        assert_eq!(cell.character, 'a');
        assert_eq!(cell.combining_marks(), &['\u{0300}']);
        assert_eq!(
            screen.extract_text(CellRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 0,
            }),
            "a\u{0300}"
        );
    }

    #[test]
    fn put_char_multiple_combining_marks_stack() {
        let mut screen = Screen::new(4, 1, 0);
        screen.put_char('e');
        screen.put_char('\u{0301}'); // acute
        screen.put_char('\u{0302}'); // circumflex
        let cell = screen.view_cell(0, 0, 0);
        assert_eq!(cell.combining_marks(), &['\u{0301}', '\u{0302}']);
    }

    #[test]
    fn put_char_wide_wraps_when_not_enough_room() {
        let mut screen = Screen::new(3, 2, 0);
        screen.put_char('a');
        screen.put_char('b');
        // column 2 — only one cell left; wide must wrap to next line.
        screen.put_char('中');
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 2 });
        assert_eq!(screen.row(0).unwrap()[0].character, 'a');
        assert_eq!(screen.row(0).unwrap()[1].character, 'b');
        assert_eq!(screen.row(1).unwrap()[0].character, '中');
        assert!(screen.row(1).unwrap()[1].wide_cont);
    }

    #[test]
    fn put_char_over_wide_clears_continuation() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('中');
        screen.set_cursor_position(0, 0);
        screen.put_char('X');
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, 'X');
        assert!(!row[0].wide_cont);
        assert!(!row[1].wide_cont);
        assert_eq!(row[1].character, ' ');
    }

    #[test]
    fn char_display_width_classes() {
        assert_eq!(char_display_width('a'), 1);
        assert_eq!(char_display_width('中'), 2);
        assert_eq!(char_display_width('\u{0300}'), 0);
        let mut walked = Vec::new();
        for_each_display_scalar("警e\u{0301}", |col, ch, width| {
            walked.push((col, ch, width));
        });
        assert_eq!(walked, vec![(0, '警', 2), (2, 'e', 1), (2, '\u{0301}', 0)]);
        assert_eq!(line_display_width("警e\u{0301}"), 3);
        assert_eq!(char_display_width(ZWJ), 0);
        // unicode-width reports skin tones as 2; placement still attaches.
        assert_eq!(char_display_width('\u{1f3fb}'), 2);
        assert!(is_emoji_modifier('\u{1f3fb}'));
        assert!(is_regional_indicator('\u{1f1fa}'));
    }

    /// Family emoji ZWJ sequence occupies one wide cell; extract is the full cluster.
    #[test]
    fn put_char_zwj_family_is_single_wide_cell() {
        let mut screen = Screen::new(8, 1, 0);
        // 👨 U+1F468 ZWJ 👩 U+1F469 ZWJ 👧 U+1F467 ZWJ 👦 U+1F466
        for c in "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}".chars() {
            screen.put_char(c);
        }
        assert_eq!(
            screen.cursor(),
            Cursor { row: 0, column: 2 },
            "cluster display width is 2"
        );
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, '\u{1f468}');
        assert!(!row[0].wide_cont);
        assert!(row[1].wide_cont);
        assert_eq!(
            screen.view_cell(0, 0, 0).combining_marks(),
            &[
                '\u{200d}',
                '\u{1f469}',
                '\u{200d}',
                '\u{1f467}',
                '\u{200d}',
                '\u{1f466}',
            ]
        );
        assert_eq!(row[2].character, ' ');
        let text = screen.extract_text(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1,
        });
        assert_eq!(
            text,
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}"
        );
    }

    #[test]
    fn put_char_emoji_skin_tone_attaches_without_advance() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('\u{1f468}'); // man (width 2)
        screen.put_char('\u{1f3fb}'); // Fitzpatrick type-1-2 (width 2 in unicode-width)
        assert_eq!(screen.cursor().column, 2);
        let cell = screen.view_cell(0, 0, 0);
        assert_eq!(cell.character, '\u{1f468}');
        assert_eq!(cell.combining_marks(), &['\u{1f3fb}']);
        assert!(screen.row(0).unwrap()[1].wide_cont);
    }

    #[test]
    fn put_char_regional_indicator_pair_is_one_wide_cell() {
        let mut screen = Screen::new(8, 1, 0);
        // 🇺🇸 = U+1F1FA U+1F1F8
        screen.put_char('\u{1f1fa}');
        assert_eq!(screen.cursor().column, 1);
        screen.put_char('\u{1f1f8}');
        assert_eq!(screen.cursor().column, 2);
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, '\u{1f1fa}');
        assert_eq!(screen.view_cell(0, 0, 0).combining_marks(), &['\u{1f1f8}']);
        assert!(row[1].wide_cont);
        assert_eq!(
            screen.extract_text(CellRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 1,
            }),
            "\u{1f1fa}\u{1f1f8}"
        );
    }

    #[test]
    fn put_char_after_zwj_cluster_places_next_glyph() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('\u{1f468}');
        screen.put_char(ZWJ);
        screen.put_char('\u{1f469}');
        screen.put_char('X');
        assert_eq!(screen.cursor().column, 3);
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, '\u{1f468}');
        assert_eq!(
            screen.view_cell(0, 0, 0).combining_marks(),
            &[ZWJ, '\u{1f469}']
        );
        assert!(row[1].wide_cont);
        assert_eq!(row[2].character, 'X');
    }

    #[test]
    fn wide_cluster_edits_keep_pairs_whole() {
        for flag in [false, true] {
            for edit in 0..3 {
                let mut screen = Screen::new(8, 1, 0);
                screen.put_char('a');
                if flag {
                    screen.put_char('\u{1f1fa}');
                    screen.put_char('\u{1f1f8}');
                } else {
                    screen.put_char('\u{263a}');
                    screen.put_char('\u{fe0f}');
                }
                screen.put_char('b');
                screen.set_cursor_position(0, 1);
                match edit {
                    0 => screen.delete_chars(1),
                    1 => screen.insert_chars(1),
                    2 => screen.erase_line(0),
                    _ => unreachable!(),
                }
                let row = screen.row(0).unwrap();
                for column in 0..row.len() {
                    if row[column].wide_cont {
                        assert!(column > 0);
                        assert!(!row[column - 1].wide_cont);
                        assert_eq!(row[column - 1].display_width(), 2);
                    }
                    if row[column].display_width() == 2 {
                        if column + 1 < row.len() {
                            assert!(row[column + 1].wide_cont);
                        } else {
                            assert_eq!(char_display_width(row[column].character), 1);
                        }
                    }
                }
            }
        }
    }

    /// ADR-0004 follow-up: DCH on the lead of a wide pair removes both cells.
    #[test]
    fn delete_chars_removes_whole_wide_pair() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('中');
        screen.put_char('a');
        screen.set_cursor_position(0, 0);
        screen.delete_chars(1);
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, 'a');
        assert!(!row[0].wide_cont);
        assert!(!row[1].wide_cont);
        assert_eq!(row[1].character, ' ');
    }

    #[test]
    fn erase_chars_clears_whole_wide_pair() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('中');
        screen.put_char('b');
        screen.set_cursor_position(0, 0);
        screen.erase_chars(1);
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, ' ');
        assert!(!row[0].wide_cont);
        assert_eq!(row[1].character, ' ');
        assert!(!row[1].wide_cont);
        assert_eq!(row[2].character, 'b');
    }

    #[test]
    fn insert_chars_on_wide_cont_snaps_to_lead() {
        let mut screen = Screen::new(8, 1, 0);
        screen.put_char('中');
        screen.set_cursor_position(0, 1); // continuation half
        screen.insert_chars(1);
        assert_eq!(
            screen.cursor().column,
            0,
            "cursor snaps to lead before insert"
        );
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].character, ' ');
        // Wide pair shifted right by one.
        assert_eq!(row[1].character, '中');
        assert!(row[2].wide_cont);
    }

    /// DECOM: CUP is relative to scroll top; cursor stays inside margins.
    #[test]
    fn decom_origin_mode_cup_relative_to_scroll_region() {
        let mut screen = Screen::new(10, 6, 0);
        screen.set_scroll_region(2, 4); // rows 1..=3 zero-based
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 0 }); // DECOM off
        screen.set_origin_mode(true);
        // Enabling DECOM clamps into region if needed.
        screen.set_cursor_position(0, 0);
        assert_eq!(screen.cursor(), Cursor { row: 1, column: 0 });
        screen.set_cursor_position(1, 3);
        assert_eq!(screen.cursor(), Cursor { row: 2, column: 3 });
        // Past region height → clamp to bottom margin.
        screen.set_cursor_position(99, 0);
        assert_eq!(screen.cursor().row, 3);
        // Cursor up cannot leave the region.
        screen.cursor_up(10);
        assert_eq!(screen.cursor().row, 1);
        // DECSTBM while origin on homes to region top-left.
        screen.set_scroll_region(3, 5);
        assert_eq!(screen.cursor(), Cursor { row: 2, column: 0 });
        // CPR-style report is origin-relative.
        screen.set_cursor_position(1, 2);
        assert_eq!(screen.cursor_report(), Cursor { row: 1, column: 2 });
        assert_eq!(screen.cursor().row, 3); // absolute = top(2)+1
    }

    #[test]
    fn soft_reset_and_ris_clear_origin_mode() {
        let mut screen = Screen::new(4, 4, 0);
        screen.set_origin_mode(true);
        assert!(screen.origin_mode());
        screen.soft_reset();
        assert!(!screen.origin_mode());
        screen.set_origin_mode(true);
        screen.ris_reset();
        assert!(!screen.origin_mode());
    }

    /// DECAWM off: last-column write overwrites in place (no wrap to next line).
    #[test]
    fn decawm_off_does_not_wrap_to_next_line() {
        let mut screen = Screen::new(3, 2, 0);
        assert!(screen.autowrap());
        screen.set_autowrap(false);
        assert!(!screen.autowrap());
        screen.put_char('a');
        screen.put_char('b');
        screen.put_char('c'); // last column
        screen.put_char('X'); // overwrite last, stay on row 0
        assert_eq!(screen.cursor(), Cursor { row: 0, column: 2 });
        assert_eq!(screen.row(0).unwrap()[2].character, 'X');
        assert_eq!(screen.row(1).unwrap()[0].character, ' ');
    }

    #[test]
    fn soft_reset_restores_autowrap() {
        let mut screen = Screen::new(4, 2, 0);
        screen.set_autowrap(false);
        screen.soft_reset();
        assert!(screen.autowrap());
    }

    #[test]
    fn hyperlink_follows_painted_cells_wrap_and_scrollback() {
        let mut screen = Screen::new(3, 2, 10);
        assert!(screen.set_hyperlink(Some("build"), "https://example.com/build"));
        for ch in "abcd".chars() {
            screen.put_char(ch);
        }
        assert_eq!(
            screen.hyperlink_uri_at_view(0, 0, 0),
            Some("https://example.com/build")
        );
        assert_eq!(
            screen.hyperlink_uri_at_view(0, 1, 0),
            Some("https://example.com/build"),
            "active OSC 8 must cross automatic wrap"
        );
        screen.clear_hyperlink();
        screen.put_char('x');
        assert_eq!(screen.hyperlink_uri_at_view(0, 1, 1), None);

        screen.line_feed();
        assert_eq!(screen.max_view_scroll(), 1);
        assert_eq!(
            screen.hyperlink_uri_at_view(1, 0, 0),
            Some("https://example.com/build"),
            "scrollback cells must retain their target"
        );
    }

    #[test]
    fn hyperlink_id_reuses_exact_id_uri_pair_and_clears_on_reset() {
        let mut screen = Screen::new(6, 1, 0);
        assert!(screen.set_hyperlink(Some("same"), "https://example.com/a"));
        screen.put_char('a');
        screen.clear_hyperlink();
        assert!(screen.set_hyperlink(Some("same"), "https://example.com/a"));
        screen.put_char('b');
        assert_eq!(
            screen.row(0).unwrap()[0].hyperlink_id(),
            screen.row(0).unwrap()[1].hyperlink_id()
        );

        screen.soft_reset();
        screen.put_char('c');
        assert_eq!(screen.row(0).unwrap()[2].hyperlink_id(), None);
        assert_eq!(
            screen.hyperlink_uri_at_view(0, 0, 0),
            Some("https://example.com/a"),
            "soft reset keeps existing cell targets"
        );

        assert!(screen.set_hyperlink(Some("same"), "https://example.com/b"));
        screen.put_char('d');
        assert_ne!(
            screen.row(0).unwrap()[0].hyperlink_id(),
            screen.row(0).unwrap()[3].hyperlink_id(),
            "one id must not alias two different targets"
        );
    }

    #[test]
    fn invalid_hyperlink_clears_active_target_and_wide_cells_share_handle() {
        let mut screen = Screen::new(5, 1, 0);
        assert!(screen.set_hyperlink(None, "https://example.com/wide"));
        screen.put_char('中');
        let row = screen.row(0).unwrap();
        assert_eq!(row[0].hyperlink_id(), row[1].hyperlink_id());
        assert!(row[1].wide_cont);

        let oversized = "x".repeat(MAX_HYPERLINK_URI_BYTES + 1);
        assert!(!screen.set_hyperlink(None, &oversized));
        screen.put_char('x');
        assert_eq!(screen.row(0).unwrap()[2].hyperlink_id(), None);
    }

    #[test]
    fn last_column_clusters_survive_wide_pair_healing() {
        for regional in [false, true] {
            for operation in 0..3 {
                let mut screen = Screen::new(8, 1, 0);
                for _ in 0..7 {
                    screen.put_char('x');
                }
                if regional {
                    screen.put_char('\u{1f1fa}');
                    screen.put_char('\u{1f1f8}');
                } else {
                    screen.put_char('#');
                    screen.put_char('\u{fe0f}');
                }
                assert_eq!(screen.row(0).unwrap()[7].display_width(), 2);
                assert!(!screen.row(0).unwrap()[7].wide_cont);

                screen.set_cursor_position(0, 0);
                match operation {
                    0 => screen.erase_chars(1),
                    1 => screen.delete_chars(1),
                    2 => screen.insert_chars(1),
                    _ => unreachable!(),
                }

                let row = screen.row(0).unwrap();
                match operation {
                    0 => {
                        assert_eq!(row[7].display_width(), 2);
                        assert!(!row[7].wide_cont);
                    }
                    1 => {
                        assert_eq!(row[6].display_width(), 2);
                        assert!(row[7].wide_cont);
                    }
                    2 => {
                        assert_eq!(row[7].character, 'x');
                        assert!(!row[7].wide_cont);
                    }
                    _ => unreachable!(),
                }
                let state = screen.export_state();
                let back = Screen::import_state(state.clone()).expect("state round-trip");
                assert_eq!(back.export_state(), state);
            }
        }
    }
}
