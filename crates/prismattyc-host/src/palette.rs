//! Pure command-palette state and keyboard behavior (PT-92, direction C):
//! a query row, filter chips, a RECENT section, a MATCHES section, and a
//! detail box for the selected row. No pixels here; `raster.rs` paints the
//! [`PaletteView`] this module builds.

use super::keybind::{Action, ActionGroup, KeyMap};
use super::{config, resolve_editor_command};
use std::path::Path;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Most recent actions kept for the RECENT section.
pub const RECENT_CAP: usize = 5;
/// File name under the prismattyc data dir that persists the recents.
pub const RECENT_FILE: &str = "palette-recent.json";
/// List rows a PageUp/PageDown jumps over; also the tallest list the panel
/// shows before it scrolls.
pub const PAGE_ROWS: usize = 12;

/// The result of handling one key while the command palette is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteVerdict {
    /// No palette was open and this event belongs to another modal or the PTY.
    NotHandled,
    /// The key was handled without changing palette mode.
    Consumed,
    /// Close the palette without running an action.
    Close,
    /// Run the selected action.
    Run(Action),
}

/// One selectable row: a single action, or a family that asks for a digit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEntry {
    Action(Action),
    /// `select_tab_1..9` collapsed; Enter, then a digit 1–9.
    SelectTabFamily,
    /// `layout_2..9` collapsed; Enter, then a digit 2–9.
    LayoutFamily,
}

impl PaletteEntry {
    /// The action that stands in for the entry (group, chord column).
    fn representative(self) -> Action {
        match self {
            PaletteEntry::Action(action) => action,
            PaletteEntry::SelectTabFamily => Action::SelectTab(1),
            PaletteEntry::LayoutFamily => Action::Layout(2),
        }
    }

    /// Name column text.
    pub fn name(self) -> String {
        match self {
            PaletteEntry::Action(action) => action.name(),
            PaletteEntry::SelectTabFamily => "select_tab_1…9".into(),
            PaletteEntry::LayoutFamily => "layout_2…9".into(),
        }
    }

    /// Description column text and the detail-box description.
    pub fn describe(self) -> String {
        match self {
            PaletteEntry::Action(Action::OpenConfig) => {
                let visual = std::env::var("VISUAL").ok();
                let editor = std::env::var("EDITOR").ok();
                let command = resolve_editor_command(visual.as_deref(), editor.as_deref());
                format!(
                    "edit the config file — {} {}",
                    command.program,
                    config::config_path().display()
                )
            }
            PaletteEntry::Action(action) => action.describe(),
            PaletteEntry::SelectTabFamily => "select a tab by number".into(),
            PaletteEntry::LayoutFamily => "even N-column layout, N = 2…9".into(),
        }
    }

    /// Filter chip the entry belongs to.
    pub fn group(self) -> ActionGroup {
        self.representative().group()
    }

    /// The `[keys]` name shown in the detail box.
    pub fn config_key(self) -> String {
        match self {
            PaletteEntry::Action(action) => action.name(),
            PaletteEntry::SelectTabFamily => "select_tab_N".into(),
            PaletteEntry::LayoutFamily => "layout_N".into(),
        }
    }

    /// Detail-box sentence: description plus the action's note.
    pub fn detail_text(self) -> String {
        let note = self.representative().note();
        match note {
            Some(note) => format!("{}. {note}", self.describe()),
            None => format!("{}.", self.describe()),
        }
    }

    /// Chord column: aliases joined by ` · `; families show the digit range.
    pub fn chord_label(self, keymap: &KeyMap) -> String {
        let label = keymap.palette_label(self.representative());
        if label.is_empty() {
            return "unbound".into();
        }
        match self {
            PaletteEntry::Action(_) => label,
            PaletteEntry::SelectTabFamily => digit_range(&label, '1'),
            PaletteEntry::LayoutFamily => digit_range(&label, '2'),
        }
    }

    /// The action a digit selects after Enter on a family row.
    pub fn digit_action(self, digit: char) -> Option<Action> {
        let n = digit.to_digit(10)?;
        let n = u8::try_from(n).ok()?;
        match self {
            PaletteEntry::SelectTabFamily if (1..=9).contains(&n) => Some(Action::SelectTab(n)),
            PaletteEntry::LayoutFamily if (2..=9).contains(&n) => Some(Action::Layout(n)),
            _ => None,
        }
    }
}

/// `C-S-1` → `C-S-1…9`; `C-S-F2` → `C-S-F2…9`. Leaves other labels alone.
fn digit_range(label: &str, first: char) -> String {
    let mut out = String::new();
    for chord in label.split(" · ") {
        if !out.is_empty() {
            out.push_str(" · ");
        }
        match chord.strip_suffix(first) {
            Some(prefix) => {
                out.push_str(prefix);
                out.push(first);
                out.push_str("…9");
            }
            None => out.push_str(chord),
        }
    }
    out
}

/// One row displayed by the command palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteRow {
    pub entry: PaletteEntry,
    pub name: String,
    pub describe: String,
    pub chord_label: String,
    pub full_width: bool,
}

impl PaletteRow {
    fn new(entry: PaletteEntry, keymap: &KeyMap) -> Self {
        PaletteRow {
            entry,
            name: entry.name(),
            describe: entry.describe(),
            chord_label: entry.chord_label(keymap),
            full_width: false,
        }
    }

    /// A plain row for pickers that reuse the palette panel (spaces).
    #[must_use]
    pub fn plain(name: String, describe: String, chord_label: String) -> Self {
        PaletteRow {
            entry: PaletteEntry::Action(Action::OpenSpace),
            name,
            describe,
            chord_label,
            full_width: false,
        }
    }
}

/// Detail box for the selected row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteDetail {
    pub name: String,
    pub text: String,
    pub chords: String,
    pub config_key: String,
}

/// What the palette shows for the current query, chip, and recents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaletteView {
    pub recent: Vec<PaletteRow>,
    pub matches: Vec<PaletteRow>,
}

impl PaletteView {
    /// Rows in selection order: recents first, then matches.
    #[must_use]
    pub fn len(&self) -> usize {
        self.recent.len() + self.matches.len()
    }

    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row at a selection index.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<&PaletteRow> {
        if index < self.recent.len() {
            self.recent.get(index)
        } else {
            self.matches.get(index - self.recent.len())
        }
    }

    /// Detail box for the row at `index`.
    #[must_use]
    pub fn detail(&self, index: usize) -> Option<PaletteDetail> {
        let row = self.row(index)?;
        Some(PaletteDetail {
            name: row.name.clone(),
            text: row.entry.detail_text(),
            chords: row.chord_label.clone(),
            config_key: row.entry.config_key(),
        })
    }
}

/// State for the command palette.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    pub query: String,
    pub selected: usize,
    /// First visible list line. Independent of `selected` so hover does not
    /// jump a scrolled window.
    pub scroll: usize,
    /// `None` is the All chip.
    pub filter: Option<ActionGroup>,
    /// Enter was pressed on a family row; the next digit picks the member.
    pub awaiting_digit: Option<PaletteEntry>,
    /// Most recent first, concrete actions only.
    pub recent: Vec<Action>,
}

impl Palette {
    /// Create a palette that shows `recent` (most recent first).
    #[must_use]
    pub fn with_recent(recent: Vec<Action>) -> Self {
        Palette {
            recent,
            ..Self::default()
        }
    }

    /// Chip labels in order: All, then every group.
    #[must_use]
    pub fn chip_labels() -> Vec<&'static str> {
        let mut labels = vec!["All"];
        labels.extend(ActionGroup::ALL.iter().map(|group| group.label()));
        labels
    }

    /// Index of the selected chip (0 = All).
    #[must_use]
    pub fn chip_index(&self) -> usize {
        match self.filter {
            None => 0,
            Some(group) => {
                1 + ActionGroup::ALL
                    .iter()
                    .position(|candidate| *candidate == group)
                    .unwrap_or(0)
            }
        }
    }

    /// Cycle the chip: All → Panes → … → View & Edit → All.
    pub fn cycle_filter(&mut self, direction: isize) {
        let count = ActionGroup::ALL.len() + 1;
        let index = self.chip_index();
        let next = if direction < 0 {
            (index + count - 1) % count
        } else {
            (index + 1) % count
        };
        self.filter = if next == 0 {
            None
        } else {
            Some(ActionGroup::ALL[next - 1])
        };
        self.selected = 0;
        self.scroll = 0;
        self.awaiting_digit = None;
    }

    /// Every selectable entry in documentation order, families collapsed.
    fn entries(rich: bool) -> Vec<PaletteEntry> {
        let mut entries = Vec::new();
        for action in Action::all() {
            if excluded(action, rich) {
                continue;
            }
            match action {
                Action::SelectTab(1) => entries.push(PaletteEntry::SelectTabFamily),
                Action::SelectTab(_) => {}
                Action::Layout(2) => entries.push(PaletteEntry::LayoutFamily),
                Action::Layout(_) => {}
                other => entries.push(PaletteEntry::Action(other)),
            }
        }
        entries
    }

    fn passes(&self, entry: PaletteEntry, query: &[char]) -> Option<MatchScore> {
        if self.filter.is_some_and(|group| entry.group() != group) {
            return None;
        }
        match_score(query, &folded(&entry.name()), &folded(&entry.describe()))
    }

    /// Return the sections for the current query, chip, and recents.
    #[must_use]
    pub fn view(&self, keymap: &KeyMap, rich: bool) -> PaletteView {
        let query = folded(&self.query);
        let recent = self
            .recent
            .iter()
            .copied()
            .filter(|action| !excluded(*action, rich))
            .map(PaletteEntry::Action)
            .filter(|entry| self.passes(*entry, &query).is_some())
            .take(RECENT_CAP)
            .map(|entry| PaletteRow::new(entry, keymap))
            .collect();

        let mut ranked = Vec::new();
        for (order, entry) in Self::entries(rich).into_iter().enumerate() {
            if let Some(score) = self.passes(entry, &query) {
                ranked.push((score, order, entry));
            }
        }
        ranked.sort_by(
            |(left_score, left_order, _), (right_score, right_order, _)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| left_order.cmp(right_order))
            },
        );
        let matches = ranked
            .into_iter()
            .map(|(_, _, entry)| PaletteRow::new(entry, keymap))
            .collect();
        PaletteView { recent, matches }
    }

    /// Handle one logical key event. `action` is the key table's verdict
    /// for the event, so a user-bound `palette_filter_next` works too.
    #[must_use]
    pub fn key(
        &mut self,
        key: &Key,
        mods: ModifiersState,
        action: Option<Action>,
        keymap: &KeyMap,
        rich: bool,
    ) -> PaletteVerdict {
        match action {
            Some(Action::PaletteFilterNext) => {
                self.cycle_filter(1);
                return PaletteVerdict::Consumed;
            }
            Some(Action::PaletteFilterPrev) => {
                self.cycle_filter(-1);
                return PaletteVerdict::Consumed;
            }
            _ => {}
        }
        if mods.control_key() && !mods.alt_key() && !mods.super_key() {
            match key {
                Key::Named(NamedKey::ArrowRight) => self.cycle_filter(1),
                Key::Named(NamedKey::ArrowLeft) => self.cycle_filter(-1),
                _ => {}
            }
            return PaletteVerdict::Consumed;
        }
        if mods.alt_key() || mods.super_key() {
            return PaletteVerdict::Consumed;
        }

        if let Some(family) = self.awaiting_digit {
            return match key {
                Key::Named(NamedKey::Escape) => {
                    self.awaiting_digit = None;
                    PaletteVerdict::Consumed
                }
                Key::Character(text) => match text.chars().find_map(|c| family.digit_action(c)) {
                    Some(action) => {
                        self.awaiting_digit = None;
                        PaletteVerdict::Run(action)
                    }
                    None => PaletteVerdict::Consumed,
                },
                _ => PaletteVerdict::Consumed,
            };
        }

        match key {
            Key::Character(text) => {
                let mut appended = false;
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.query.push(character);
                    appended = true;
                }
                if appended {
                    self.selected = 0;
                    self.scroll = 0;
                }
                PaletteVerdict::Consumed
            }
            Key::Named(named) => match named {
                NamedKey::Backspace => {
                    self.query.pop();
                    self.clamp(keymap, rich);
                    PaletteVerdict::Consumed
                }
                NamedKey::Space => {
                    self.query.push(' ');
                    self.selected = 0;
                    self.scroll = 0;
                    PaletteVerdict::Consumed
                }
                NamedKey::Escape => PaletteVerdict::Close,
                NamedKey::Enter => {
                    let view = self.view(keymap, rich);
                    match view.row(self.selected).map(|row| row.entry) {
                        Some(PaletteEntry::Action(action)) => PaletteVerdict::Run(action),
                        Some(family) => {
                            self.awaiting_digit = Some(family);
                            PaletteVerdict::Consumed
                        }
                        None => PaletteVerdict::Consumed,
                    }
                }
                NamedKey::ArrowUp => self.move_selection(-1, keymap, rich),
                NamedKey::ArrowDown => self.move_selection(1, keymap, rich),
                NamedKey::PageUp => self.page_selection(-1, keymap, rich),
                NamedKey::PageDown => self.page_selection(1, keymap, rich),
                NamedKey::Home => {
                    self.selected = 0;
                    PaletteVerdict::Consumed
                }
                NamedKey::End => {
                    let count = self.view(keymap, rich).len();
                    self.selected = count.saturating_sub(1);
                    PaletteVerdict::Consumed
                }
                _ => PaletteVerdict::Consumed,
            },
            _ => PaletteVerdict::Consumed,
        }
    }

    fn clamp(&mut self, keymap: &KeyMap, rich: bool) {
        let count = self.view(keymap, rich).len();
        self.selected = self.selected.min(count.saturating_sub(1));
    }

    fn move_selection(&mut self, direction: isize, keymap: &KeyMap, rich: bool) -> PaletteVerdict {
        let count = self.view(keymap, rich).len();
        if count == 0 {
            return PaletteVerdict::Consumed;
        }
        self.selected %= count;
        self.selected = if direction < 0 {
            (self.selected + count - 1) % count
        } else {
            (self.selected + 1) % count
        };
        PaletteVerdict::Consumed
    }

    fn page_selection(&mut self, direction: isize, keymap: &KeyMap, rich: bool) -> PaletteVerdict {
        let count = self.view(keymap, rich).len();
        if count == 0 {
            return PaletteVerdict::Consumed;
        }
        self.selected = self.selected.min(count - 1);
        if direction < 0 {
            self.selected = self.selected.saturating_sub(PAGE_ROWS);
        } else {
            self.selected = self.selected.saturating_add(PAGE_ROWS).min(count - 1);
        }
        PaletteVerdict::Consumed
    }
}

/// Character to type into the palette query.
///
/// Shifted glyphs (`_`, `$`, `A`) come from `logical` / `text`. Chord
/// lookup still uses `unshifted` (`key_without_modifiers`).
pub(crate) fn typed_character_key(
    unshifted: &Key,
    logical: &Key,
    text: Option<&str>,
    mods: ModifiersState,
) -> Key {
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return unshifted.clone();
    }
    if matches!(unshifted, Key::Character(_)) {
        if let Some(text) = text {
            if text.chars().any(|character| !character.is_control()) {
                return Key::Character(text.into());
            }
        }
        return logical.clone();
    }
    unshifted.clone()
}

/// Actions that never enter the RECENT list: the palette's own controls.
#[must_use]
pub fn recordable(action: Action) -> bool {
    !matches!(
        action,
        Action::CommandPalette | Action::PaletteFilterNext | Action::PaletteFilterPrev
    )
}

/// Move `action` to the front of `recent`, dropping duplicates; keep
/// [`RECENT_CAP`] entries. Returns whether the list changed.
pub fn push_recent(recent: &mut Vec<Action>, action: Action) -> bool {
    if !recordable(action) {
        return false;
    }
    if recent.first() == Some(&action) {
        return false;
    }
    recent.retain(|candidate| *candidate != action);
    recent.insert(0, action);
    recent.truncate(RECENT_CAP);
    true
}

/// Read the recents file: a JSON array of action names, most recent first.
/// Unknown names are dropped; a missing or malformed file is empty.
#[must_use]
pub fn load_recent(path: &Path) -> Vec<Action> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(names) = serde_json::from_str::<Vec<String>>(&raw) else {
        return Vec::new();
    };
    let mut recent = Vec::new();
    for name in names {
        if let Some(action) = Action::from_name(&name) {
            if recordable(action) && !recent.contains(&action) {
                recent.push(action);
            }
        }
    }
    recent.truncate(RECENT_CAP);
    recent
}

/// Write the recents file (creates the directory).
pub fn save_recent(path: &Path, recent: &[Action]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let names: Vec<String> = recent.iter().map(|action| action.name()).collect();
    let json = serde_json::to_string(&names).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Palette overlay over saved spaces (PT-89).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpacePickerKind {
    Open,
    Delete,
    MovePane,
    MoveSession,
}

/// One saved space shown in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpacePickerRow {
    pub name: String,
    pub sessions: usize,
    pub saved_at_unix: u64,
}

/// Filter/select/confirm state for open_space, delete_space, and
/// move_pane_to_space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpacePicker {
    pub kind: SpacePickerKind,
    pub query: String,
    pub selected: usize,
    /// First visible list line. Independent of `selected`.
    pub scroll: usize,
    /// When set, Enter confirms delete of this name.
    pub confirm: Option<String>,
    pub status: Option<String>,
}

/// Result of a key while a space picker is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpacePickerVerdict {
    Consumed,
    Close,
    Open(String),
    Deleted(String),
    Move(String),
}

/// Shared keyboard state for the chip and pane context menus (PT-214/PT-215).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuKind {
    SpaceChip,
    Pane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMenu {
    pub kind: ContextMenuKind,
    pub selected: usize,
    /// A destructive item that needs a second Enter.
    pub confirm: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuVerdict {
    Consumed,
    Close,
    Activate(usize),
    Confirm(usize),
}

impl ContextMenu {
    #[must_use]
    pub fn new(kind: ContextMenuKind) -> Self {
        Self {
            kind,
            selected: 0,
            confirm: None,
        }
    }

    #[must_use]
    pub fn key(&mut self, key: &Key, mods: ModifiersState, row_count: usize) -> ContextMenuVerdict {
        if mods.control_key() || mods.alt_key() || mods.super_key() || row_count == 0 {
            return ContextMenuVerdict::Consumed;
        }
        match key {
            Key::Named(NamedKey::Escape) => ContextMenuVerdict::Close,
            Key::Named(NamedKey::Home) => {
                self.confirm = None;
                self.selected = 0;
                ContextMenuVerdict::Consumed
            }
            Key::Named(NamedKey::End) => {
                self.confirm = None;
                self.selected = row_count - 1;
                ContextMenuVerdict::Consumed
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.confirm = None;
                self.selected = (self.selected + row_count - 1) % row_count;
                ContextMenuVerdict::Consumed
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.confirm = None;
                self.selected = (self.selected + 1) % row_count;
                ContextMenuVerdict::Consumed
            }
            Key::Named(NamedKey::Enter) => {
                if self.confirm == Some(self.selected) {
                    // Activation reads this flag before it closes the menu.
                    ContextMenuVerdict::Confirm(self.selected)
                } else {
                    ContextMenuVerdict::Activate(self.selected)
                }
            }
            _ => ContextMenuVerdict::Consumed,
        }
    }
}

impl SpacePicker {
    #[must_use]
    pub fn new(kind: SpacePickerKind) -> Self {
        Self {
            kind,
            query: String::new(),
            selected: 0,
            scroll: 0,
            confirm: None,
            status: None,
        }
    }

    #[must_use]
    pub fn ranked<'a>(&self, spaces: &'a [SpacePickerRow]) -> Vec<&'a SpacePickerRow> {
        let query = folded(&self.query);
        let mut ranked = Vec::new();
        for (order, space) in spaces.iter().enumerate() {
            let name = folded(&space.name);
            let Some(score) = match_score(&query, &name, &[]) else {
                continue;
            };
            ranked.push((score, order, space));
        }
        ranked.sort_by(
            |(left_score, left_order, _), (right_score, right_order, _)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| left_order.cmp(right_order))
            },
        );
        ranked.into_iter().map(|(_, _, space)| space).collect()
    }

    #[must_use]
    pub fn key(
        &mut self,
        key: &Key,
        mods: ModifiersState,
        spaces: &[SpacePickerRow],
    ) -> SpacePickerVerdict {
        if mods.control_key() || mods.alt_key() || mods.super_key() {
            return SpacePickerVerdict::Consumed;
        }
        if let Some(name) = self.confirm.clone() {
            return match key {
                Key::Named(NamedKey::Escape) => {
                    self.confirm = None;
                    SpacePickerVerdict::Consumed
                }
                Key::Named(NamedKey::Enter) => {
                    self.confirm = None;
                    self.status = Some(format!("deleted {name}"));
                    SpacePickerVerdict::Deleted(name)
                }
                _ => SpacePickerVerdict::Consumed,
            };
        }
        match key {
            Key::Character(text) => {
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.query.push(character);
                }
                self.selected = 0;
                self.scroll = 0;
                SpacePickerVerdict::Consumed
            }
            Key::Named(named) => match named {
                NamedKey::Backspace => {
                    self.query.pop();
                    self.selected = self
                        .selected
                        .min(self.ranked(spaces).len().saturating_sub(1));
                    SpacePickerVerdict::Consumed
                }
                NamedKey::Escape => SpacePickerVerdict::Close,
                NamedKey::Enter => {
                    let Some(row) = self.ranked(spaces).get(self.selected).copied() else {
                        return SpacePickerVerdict::Consumed;
                    };
                    match self.kind {
                        SpacePickerKind::Open => SpacePickerVerdict::Open(row.name.clone()),
                        SpacePickerKind::MovePane | SpacePickerKind::MoveSession => {
                            SpacePickerVerdict::Move(row.name.clone())
                        }
                        SpacePickerKind::Delete => {
                            self.confirm = Some(row.name.clone());
                            SpacePickerVerdict::Consumed
                        }
                    }
                }
                NamedKey::ArrowUp | NamedKey::ArrowDown => {
                    let count = self.ranked(spaces).len();
                    if count == 0 {
                        return SpacePickerVerdict::Consumed;
                    }
                    if matches!(named, NamedKey::ArrowUp) {
                        self.selected = (self.selected + count - 1) % count;
                    } else {
                        self.selected = (self.selected + 1) % count;
                    }
                    SpacePickerVerdict::Consumed
                }
                _ => SpacePickerVerdict::Consumed,
            },
            _ => SpacePickerVerdict::Consumed,
        }
    }
}

fn excluded(action: Action, rich: bool) -> bool {
    matches!(
        action,
        Action::CommandPalette | Action::PaletteFilterNext | Action::PaletteFilterPrev
    ) || (!rich && action == Action::RichFocus)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatchScore {
    class: u8,
    word_start_hits: usize,
}

/// Rank: exact name (4) > name prefix (3) > name contains the query (2) >
/// name subsequence (1) > description-only subsequence (0); word-start hits
/// break ties. A name hit always beats a description-only hit, so typing
/// `tab` lists `new_tab` before "focus the pane below".
fn match_score(query: &[char], name: &[char], describe: &[char]) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(MatchScore {
            class: 0,
            word_start_hits: 0,
        });
    }
    if query == name {
        return Some(MatchScore {
            class: 4,
            word_start_hits: 0,
        });
    }
    if name.starts_with(query) {
        return Some(MatchScore {
            class: 3,
            word_start_hits: 0,
        });
    }
    if name.windows(query.len()).any(|window| window == query) {
        return Some(MatchScore {
            class: 2,
            word_start_hits: 0,
        });
    }
    if let Some(positions) = subsequence_positions(query, name) {
        return Some(MatchScore {
            class: 1,
            word_start_hits: word_start_hits(&positions, name),
        });
    }
    let positions = subsequence_positions(query, describe)?;
    Some(MatchScore {
        class: 0,
        word_start_hits: word_start_hits(&positions, describe),
    })
}

fn subsequence_positions(query: &[char], text: &[char]) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(query.len());
    let mut text_index = 0;
    for wanted in query {
        let found = text[text_index..]
            .iter()
            .position(|candidate| candidate == wanted)?;
        text_index += found + 1;
        positions.push(text_index - 1);
    }
    Some(positions)
}

fn word_start_hits(positions: &[usize], text: &[char]) -> usize {
    positions
        .iter()
        .filter(|&&position| position == 0 || !text[position - 1].is_alphanumeric())
        .count()
}

fn folded(text: &str) -> Vec<char> {
    text.chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_mods() -> ModifiersState {
        ModifiersState::empty()
    }

    fn ctrl() -> ModifiersState {
        ModifiersState::CONTROL
    }

    fn keymap() -> KeyMap {
        KeyMap::default()
    }

    fn press(palette: &mut Palette, key: Key) -> PaletteVerdict {
        palette.key(&key, empty_mods(), None, &keymap(), false)
    }

    fn names(rows: &[PaletteRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn preset_actions_appear_and_filter() {
        let view = Palette::default().view(&keymap(), false);
        let names = names(&view.matches);
        for name in [
            "preset_single",
            "preset_split_h",
            "preset_split_v",
            "preset_grid",
        ] {
            assert!(names.contains(&name), "missing {name} in {names:?}");
        }
        let grid = view
            .matches
            .iter()
            .find(|row| row.entry == PaletteEntry::Action(Action::PresetGrid))
            .expect("preset_grid row");
        assert_eq!(grid.chord_label, "unbound");

        let filtered = Palette {
            query: "preset_grid".into(),
            ..Palette::default()
        };
        let view = filtered.view(&keymap(), false);
        assert_eq!(
            view.matches.first().map(|row| row.entry),
            Some(PaletteEntry::Action(Action::PresetGrid))
        );
        assert!(view.matches.iter().all(|row| row.name.contains("preset")));
    }

    #[test]
    fn open_config_is_a_palette_only_view_edit_action() {
        let palette = Palette {
            query: "open_config".into(),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        let row = view
            .matches
            .iter()
            .find(|row| row.entry == PaletteEntry::Action(Action::OpenConfig))
            .expect("open_config row");
        assert_eq!(row.entry.group(), ActionGroup::ViewEdit);
        assert_eq!(row.name, "open_config");
        assert!(row.describe.starts_with("edit the config file — "));
        assert!(row
            .describe
            .contains(&config::config_path().display().to_string()));
        assert!(row.entry.detail_text().contains("edit the config file — "));
        assert_eq!(row.chord_label, "unbound");
    }

    fn shift() -> ModifiersState {
        ModifiersState::SHIFT
    }

    #[test]
    fn typed_character_key_keeps_shifted_glyphs_without_ctrl() {
        let minus = Key::Character("-".into());
        let underscore = Key::Character("_".into());
        let four = Key::Character("4".into());
        let dollar = Key::Character("$".into());
        let r_lower = Key::Character("r".into());
        let r_upper = Key::Character("R".into());
        assert_eq!(
            typed_character_key(&minus, &underscore, Some("_"), shift()),
            underscore
        );
        assert_eq!(
            typed_character_key(&four, &dollar, Some("$"), shift()),
            dollar
        );
        assert_eq!(
            typed_character_key(&r_lower, &r_upper, Some("R"), shift()),
            r_upper
        );
        let mut ctrl_shift = shift();
        ctrl_shift |= ModifiersState::CONTROL;
        assert_eq!(
            typed_character_key(&minus, &underscore, Some("_"), ctrl_shift),
            minus,
            "ctrl keeps the unshifted key for chord lookup"
        );
        assert_eq!(
            typed_character_key(
                &Key::Named(NamedKey::Escape),
                &Key::Named(NamedKey::Escape),
                None,
                shift()
            ),
            Key::Named(NamedKey::Escape)
        );
    }

    #[test]
    fn underscore_dollar_and_uppercase_reach_the_query_and_match_rename_pane() {
        let mut palette = Palette::default();
        for key in ["r", "e", "n", "a", "m", "e", "_", "p", "a", "n", "e"] {
            assert_eq!(
                press(&mut palette, Key::Character(key.into())),
                PaletteVerdict::Consumed
            );
        }
        assert_eq!(palette.query, "rename_pane");
        let view = palette.view(&keymap(), false);
        assert_eq!(
            view.matches.first().map(|row| row.entry),
            Some(PaletteEntry::Action(Action::RenamePane))
        );
        assert!(names(&view.matches).contains(&"rename_pane"));

        let mut symbols = Palette::default();
        assert_eq!(
            press(&mut symbols, Key::Character("$".into())),
            PaletteVerdict::Consumed
        );
        assert_eq!(
            press(&mut symbols, Key::Character("R".into())),
            PaletteVerdict::Consumed
        );
        assert_eq!(symbols.query, "$R");
        let renamed = Palette {
            query: "rename_pane".into(),
            ..Palette::default()
        };
        assert_eq!(
            renamed
                .view(&keymap(), false)
                .matches
                .first()
                .map(|row| row.entry),
            Some(PaletteEntry::Action(Action::RenamePane))
        );
    }

    #[test]
    fn unmatched_query_returns_empty_list() {
        let palette = Palette {
            query: "zzzz".into(),
            ..Palette::default()
        };
        assert!(palette.view(&keymap(), false).is_empty());
    }

    #[test]
    fn empty_query_returns_documentation_order_with_families_collapsed() {
        let view = Palette::default().view(&keymap(), false);
        let names = names(&view.matches);
        assert_eq!(names.first(), Some(&"split_right"));
        assert!(!names.contains(&"rich_focus"));
        assert!(!names.contains(&"command_palette"));
        assert!(!names.contains(&"palette_filter_next"));
        assert_eq!(
            names.iter().filter(|n| n.starts_with("select_tab")).count(),
            1
        );
        assert_eq!(names.iter().filter(|n| n.starts_with("layout_")).count(), 1);
        assert!(names.contains(&"select_tab_1…9"));
        assert!(names.contains(&"layout_2…9"));
        assert!(names.contains(&"preset_grid"));
        let family = view
            .matches
            .iter()
            .find(|row| row.entry == PaletteEntry::SelectTabFamily)
            .unwrap();
        assert_eq!(family.chord_label, "C-S-1…9");
        let layout = view
            .matches
            .iter()
            .find(|row| row.entry == PaletteEntry::LayoutFamily)
            .unwrap();
        assert_eq!(layout.chord_label, "C-S-F2…9");
    }

    #[test]
    fn chord_column_joins_same_modifier_aliases_only() {
        let view = Palette::default().view(&keymap(), false);
        let split = view
            .matches
            .iter()
            .find(|row| row.entry == PaletteEntry::Action(Action::SplitRight))
            .unwrap();
        assert_eq!(split.chord_label, "C-S-\\ · C-S-E");
    }

    #[test]
    fn family_enter_then_digit_runs_the_member() {
        let mut palette = Palette {
            query: "select_tab".into(),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        assert_eq!(
            view.matches.first().map(|row| row.entry),
            Some(PaletteEntry::SelectTabFamily)
        );
        assert_eq!(
            press(&mut palette, Key::Named(NamedKey::Enter)),
            PaletteVerdict::Consumed
        );
        assert_eq!(palette.awaiting_digit, Some(PaletteEntry::SelectTabFamily));
        assert_eq!(
            press(&mut palette, Key::Character("x".into())),
            PaletteVerdict::Consumed
        );
        assert_eq!(
            press(&mut palette, Key::Character("3".into())),
            PaletteVerdict::Run(Action::SelectTab(3))
        );
        assert_eq!(palette.awaiting_digit, None);

        let mut layout = Palette {
            query: "layout_2".into(),
            ..Palette::default()
        };
        let _ = press(&mut layout, Key::Named(NamedKey::Enter));
        assert_eq!(
            press(&mut layout, Key::Character("1".into())),
            PaletteVerdict::Consumed,
            "layout has no member 1"
        );
        assert_eq!(
            press(&mut layout, Key::Named(NamedKey::Escape)),
            PaletteVerdict::Consumed,
            "Esc cancels the digit wait, not the palette"
        );
        assert_eq!(layout.awaiting_digit, None);
        let _ = press(&mut layout, Key::Named(NamedKey::Enter));
        assert_eq!(
            press(&mut layout, Key::Character("4".into())),
            PaletteVerdict::Run(Action::Layout(4))
        );
    }

    #[test]
    fn chips_filter_by_group_and_cycle_both_ways() {
        let mut palette = Palette::default();
        assert_eq!(palette.chip_index(), 0);
        assert_eq!(
            palette.key(
                &Key::Named(NamedKey::ArrowRight),
                ctrl(),
                None,
                &keymap(),
                false
            ),
            PaletteVerdict::Consumed
        );
        assert_eq!(palette.filter, Some(ActionGroup::Panes));
        let view = palette.view(&keymap(), false);
        assert!(view
            .matches
            .iter()
            .all(|row| row.entry.group() == ActionGroup::Panes));
        assert!(names(&view.matches).contains(&"split_right"));
        assert!(!names(&view.matches).contains(&"new_tab"));

        let _ = palette.key(
            &Key::Named(NamedKey::ArrowLeft),
            ctrl(),
            None,
            &keymap(),
            false,
        );
        assert_eq!(palette.filter, None);
        let _ = palette.key(
            &Key::Named(NamedKey::ArrowLeft),
            ctrl(),
            None,
            &keymap(),
            false,
        );
        assert_eq!(palette.filter, Some(ActionGroup::ViewEdit));
        assert_eq!(palette.chip_index(), Palette::chip_labels().len() - 1);

        // The bindable actions cycle too, whatever key carried them.
        let _ = palette.key(
            &Key::Character("j".into()),
            empty_mods(),
            Some(Action::PaletteFilterNext),
            &keymap(),
            false,
        );
        assert_eq!(palette.filter, None);
        assert_eq!(palette.query, "", "a bound chip key does not type");
    }

    #[test]
    fn query_and_chip_combine() {
        let palette = Palette {
            query: "sp".into(),
            filter: Some(ActionGroup::Spaces),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        assert_eq!(
            names(&view.matches),
            vec![
                "space_rail_focus",
                "space_settings",
                "space_rail_next",
                "space_rail_prev",
                "open_space",
                "delete_space",
                "move_pane_to_space",
                "undo_space_change",
                "save_space",
                "terminal_switcher"
            ]
        );
        let panes = Palette {
            query: "sp".into(),
            filter: Some(ActionGroup::Panes),
            ..Palette::default()
        };
        let view = panes.view(&keymap(), false);
        assert_eq!(
            names(&view.matches).first(),
            Some(&"split_right"),
            "{:?}",
            names(&view.matches)
        );
        assert!(!names(&view.matches).contains(&"open_space"));
    }

    #[test]
    fn recents_lead_the_selection_and_follow_query_and_chip() {
        let mut recent = Vec::new();
        assert!(push_recent(&mut recent, Action::Find));
        assert!(push_recent(&mut recent, Action::SplitRight));
        assert!(push_recent(&mut recent, Action::RenameTab));
        assert!(
            push_recent(&mut recent, Action::SplitRight),
            "moves to the front"
        );
        assert!(
            !push_recent(&mut recent, Action::SplitRight),
            "already first"
        );
        assert!(!push_recent(&mut recent, Action::CommandPalette));
        assert_eq!(
            recent,
            vec![Action::SplitRight, Action::RenameTab, Action::Find]
        );
        for n in 1..=4 {
            assert!(push_recent(&mut recent, Action::SelectTab(n)));
        }
        assert_eq!(recent.len(), RECENT_CAP);
        assert_eq!(recent[0], Action::SelectTab(4));
        assert!(!recent.contains(&Action::Find), "oldest falls off");

        let mut palette = Palette::with_recent(recent.clone());
        let view = palette.view(&keymap(), false);
        assert_eq!(view.recent.len(), RECENT_CAP);
        assert_eq!(view.recent[0].name, "select_tab_4");
        assert_eq!(
            view.row(0).map(|row| row.entry),
            Some(PaletteEntry::Action(Action::SelectTab(4)))
        );
        assert_eq!(
            press(&mut palette, Key::Named(NamedKey::Enter)),
            PaletteVerdict::Run(Action::SelectTab(4))
        );

        palette.query = "split".into();
        let view = palette.view(&keymap(), false);
        assert_eq!(names(&view.recent), vec!["split_right"]);
        assert_eq!(names(&view.matches).first(), Some(&"split_right"));

        palette.query.clear();
        palette.filter = Some(ActionGroup::Tabs);
        let view = palette.view(&keymap(), false);
        assert_eq!(
            names(&view.recent),
            vec![
                "select_tab_4",
                "select_tab_3",
                "select_tab_2",
                "select_tab_1"
            ],
            "split_right is Panes; rename_tab fell off the cap"
        );
    }

    #[test]
    fn recents_persist_round_trip_and_ignore_junk() {
        let dir = std::env::temp_dir().join(format!("pt92-recent-{}", std::process::id()));
        let path = dir.join(RECENT_FILE);
        assert!(load_recent(&path).is_empty(), "missing file is empty");
        let recent = vec![Action::ZoomPane, Action::Layout(4), Action::Find];
        save_recent(&path, &recent).unwrap();
        assert_eq!(load_recent(&path), recent);
        std::fs::write(
            &path,
            r#"["find","nope","command_palette","find","split_down","a","b","c","d","e"]"#,
        )
        .unwrap();
        let loaded = load_recent(&path);
        assert_eq!(loaded, vec![Action::Find, Action::SplitDown]);
        std::fs::write(&path, "not json").unwrap();
        assert!(load_recent(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detail_box_names_the_config_key_and_note() {
        let palette = Palette {
            query: "split_right".into(),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        let detail = view.detail(0).unwrap();
        assert_eq!(detail.name, "split_right");
        assert_eq!(
            detail.text,
            "split the focused pane to the right. New pane inherits the cwd."
        );
        assert_eq!(detail.chords, "C-S-\\ · C-S-E");
        assert_eq!(detail.config_key, "split_right");
        let family = Palette {
            query: "layout".into(),
            ..Palette::default()
        };
        let view = family.view(&keymap(), false);
        let detail = view.detail(0).unwrap();
        assert_eq!(detail.config_key, "layout_N");
        assert!(detail.text.contains("Spawns panes up to N"));
    }

    #[test]
    fn split_ranking_prefers_name_prefix_and_keeps_ties_in_order() {
        let palette = Palette {
            query: "split".into(),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        assert_eq!(names(&view.matches)[..2], ["split_right", "split_down"]);
    }

    #[test]
    fn subsequence_query_matches_spr() {
        let palette = Palette {
            query: "spr".into(),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        assert_eq!(names(&view.matches).first(), Some(&"split_right"));
    }

    #[test]
    fn name_hits_outrank_description_hits() {
        let palette = Palette {
            query: "tab".into(),
            ..Palette::default()
        };
        let view = palette.view(&keymap(), false);
        let top: Vec<&str> = names(&view.matches).into_iter().take(5).collect();
        assert!(
            top.iter().all(|name| name.contains("tab")),
            "name matches lead: {top:?}"
        );
        assert!(
            names(&view.matches).contains(&"focus_down"),
            "description hits still list"
        );
    }

    #[test]
    fn up_and_down_wrap_across_recent_and_matches() {
        let mut palette = Palette {
            query: "focus".into(),
            ..Palette::with_recent(vec![Action::FocusLeft])
        };
        let count = palette.view(&keymap(), false).len();
        assert!(count > 2);
        let _ = press(&mut palette, Key::Named(NamedKey::ArrowUp));
        assert_eq!(palette.selected, count - 1);
        let _ = press(&mut palette, Key::Named(NamedKey::ArrowDown));
        assert_eq!(palette.selected, 0);
        let _ = press(&mut palette, Key::Named(NamedKey::End));
        assert_eq!(palette.selected, count - 1);
        let _ = press(&mut palette, Key::Named(NamedKey::Home));
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn enter_runs_selected_action_and_escape_closes() {
        let mut palette = Palette {
            query: "split_right".into(),
            ..Palette::default()
        };
        assert_eq!(
            press(&mut palette, Key::Named(NamedKey::Enter)),
            PaletteVerdict::Run(Action::SplitRight)
        );
        let mut palette = Palette::default();
        assert_eq!(
            press(&mut palette, Key::Named(NamedKey::Escape)),
            PaletteVerdict::Close
        );
        assert_eq!(
            press(&mut palette, Key::Named(NamedKey::Tab)),
            PaletteVerdict::Consumed
        );
    }

    fn sample_spaces() -> Vec<SpacePickerRow> {
        vec![
            SpacePickerRow {
                name: "alpha".into(),
                sessions: 2,
                saved_at_unix: 10,
            },
            SpacePickerRow {
                name: "beta".into(),
                sessions: 1,
                saved_at_unix: 20,
            },
        ]
    }

    #[test]
    fn space_picker_filters_and_deletes_on_confirm() {
        let spaces = sample_spaces();
        let mut picker = SpacePicker::new(SpacePickerKind::Delete);
        assert_eq!(picker.ranked(&spaces).len(), 2);
        let _ = picker.key(&Key::Character("b".into()), empty_mods(), &spaces);
        let ranked = picker.ranked(&spaces);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].name, "beta");
        assert_eq!(
            picker.key(&Key::Named(NamedKey::Enter), empty_mods(), &spaces),
            SpacePickerVerdict::Consumed
        );
        assert_eq!(picker.confirm.as_deref(), Some("beta"));
        assert_eq!(
            picker.key(&Key::Named(NamedKey::Enter), empty_mods(), &spaces),
            SpacePickerVerdict::Deleted("beta".into())
        );
        assert_eq!(picker.status.as_deref(), Some("deleted beta"));
    }

    #[test]
    fn space_picker_open_enter_returns_name() {
        let spaces = sample_spaces();
        let mut picker = SpacePicker::new(SpacePickerKind::Open);
        assert_eq!(
            picker.key(&Key::Named(NamedKey::Enter), empty_mods(), &spaces),
            SpacePickerVerdict::Open("alpha".into())
        );
    }

    #[test]
    fn space_picker_move_enter_returns_name() {
        let spaces = sample_spaces();
        let mut picker = SpacePicker::new(SpacePickerKind::MovePane);
        assert_eq!(
            picker.key(&Key::Named(NamedKey::Enter), empty_mods(), &spaces),
            SpacePickerVerdict::Move("alpha".into())
        );
    }

    #[test]
    fn context_menus_wrap_consume_modifiers_and_confirm_for_both_targets() {
        for kind in [ContextMenuKind::SpaceChip, ContextMenuKind::Pane] {
            let mut menu = ContextMenu::new(kind);
            assert_eq!(
                menu.key(&Key::Named(NamedKey::ArrowUp), empty_mods(), 3),
                ContextMenuVerdict::Consumed
            );
            assert_eq!(menu.selected, 2);
            assert_eq!(
                menu.key(&Key::Named(NamedKey::ArrowDown), empty_mods(), 3),
                ContextMenuVerdict::Consumed
            );
            assert_eq!(menu.selected, 0);
            assert_eq!(
                menu.key(&Key::Named(NamedKey::Enter), ModifiersState::CONTROL, 3),
                ContextMenuVerdict::Consumed
            );
            assert_eq!(
                menu.key(&Key::Named(NamedKey::Enter), empty_mods(), 3),
                ContextMenuVerdict::Activate(0)
            );
            menu.confirm = Some(0);
            assert_eq!(
                menu.key(&Key::Named(NamedKey::Enter), empty_mods(), 3),
                ContextMenuVerdict::Confirm(0)
            );
            assert_eq!(menu.confirm, Some(0));
            assert_eq!(
                menu.key(&Key::Named(NamedKey::Escape), empty_mods(), 3),
                ContextMenuVerdict::Close
            );
            assert_eq!(
                menu.key(&Key::Named(NamedKey::ArrowDown), empty_mods(), 0),
                ContextMenuVerdict::Consumed
            );
        }
    }
}
