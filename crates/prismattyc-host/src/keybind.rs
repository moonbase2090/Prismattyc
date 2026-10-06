//! User keybindings for host actions (keybindings, PT-41).
//!
//! One table maps named [`Action`]s to [`Chord`]s. Dispatch, the chord strip,
//! `--help`, and `docs/config.md` all read this table, so a rebinding shows up
//! everywhere. `[keys]` in config.toml overrides the defaults per action
//! (D-K5: an entry replaces every default chord of that action).
//!
//! Matching is physical-scancode first (layout-independent, Space controls), then
//! the logical character. Ctrl/Shift/Alt must match exactly; Super is
//! required only when the chord names it (some compositors leave Super
//! sticky). A chord must carry Ctrl, Alt, or Super — plain and Shift-only
//! keys belong to the PTY (text selection D-H3).
//!
//! Validation rejects the whole file (D-K4): unknown action, unparsable
//! chord, missing Ctrl/Alt/Super, two actions on one chord, or a chord equal
//! to a fixed input (D-K3). Parsing is total; a bad chord never panics.

use std::collections::BTreeMap;
use std::fmt;

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

/// A host action a user can bind. Names are the `[keys]` table keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    SplitRight,
    SplitDown,
    ClosePane,
    Detach,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusBorderNext,
    FocusBorderPrev,
    /// Cycle the Graphite bar background preset (#108). Gated on
    /// `chrome_style = "graphite"`; the default chord is Ctrl+Shift+B.
    BarColorNext,
    BarColorPrev,
    /// Exchange the focused pane with the previous one in layout order (PT-125).
    SwapPanePrev,
    /// Exchange the focused pane with the next one in layout order (PT-125).
    SwapPaneNext,
    /// Rotate every pane one slot forward (tmux `rotate-window -D`) (PT-125).
    RotatePanes,
    /// Rotate every pane one slot back (tmux `rotate-window -U`) (PT-125).
    RotatePanesBack,
    /// Focus the pane that was focused before this one in the tab (PT-127).
    FocusLastPane,
    NewTab,
    NewBlankTab,
    NewSessionTab,
    BlankSplitRight,
    BlankSplitDown,
    SessionSplitRight,
    SessionSplitDown,
    TerminalSwitcher,
    AgentMessages,
    /// Focus the next pane that needs attention (issue #184).
    JumpNeedsYou,
    UpdateRestart,
    /// Recent status messages, including toasts hidden by `toasts` (#171).
    /// Unbound by default.
    RecentMessages,

    CloseTab,
    RenameTab,
    /// Set the focused pane's title (PT-148); shown on its strip handle hover
    /// and, for an attach pane, sent to the mux with `pmux rename-pane`.
    RenamePane,
    PrevTab,
    NextTab,
    /// Select the tab that was active before this one (PT-127).
    LastTab,
    /// 1-based tab index, 1..=9.
    SelectTab(u8),
    MovePanePrevTab,
    MovePaneNextTab,
    /// Extract the focused pane into its own tab (tmux `break-pane`) (PT-131).
    BreakPane,
    /// Join the focused pane into the previous tab (tmux `join-pane`) (PT-131).
    JoinPane,
    MoveTabLeft,
    MoveTabRight,
    /// Even layout with `n` columns (4 = 2×2 quadrants), 2..=9.
    Layout(u8),
    /// Retile existing panes to a single leaf when the tab has one pane
    /// (PT-70). No-op when `n > 1`. Does not spawn or close a pane.
    PresetSingle,
    /// Retile existing panes into an even horizontal row (PT-70).
    PresetSplitH,
    /// Retile existing panes into an even vertical column (PT-70).
    PresetSplitV,
    /// Retile existing panes into an even two-row grid (PT-70).
    PresetGrid,
    /// Focused pane left; remaining panes stacked on the right (PT-132).
    PresetMainVertical,
    /// Focused pane top; remaining panes in a row below (PT-132).
    PresetMainHorizontal,
    /// Arrange the tab as one pane: growing is a retile, shrinking zooms
    /// the focused pane and never closes one (issue #107).
    ArrangeSingle,
    /// Arrange the tab as a side-by-side split, spawning empty shells to
    /// fill it; extra panes zoom instead of closing (issue #107).
    ArrangeSplit,
    /// Arrange the tab as a 2×2 grid, spawning empty shells to fill it;
    /// extra panes zoom instead of closing (issue #107).
    ArrangeGrid,
    /// Toggle the focused pane between its split slot and the whole tab
    /// (client-local view, PT-57).
    ZoomPane,
    NewWindow,
    Quit,
    /// Open the host config in the user's editor. Unbound by default (PT-179).
    OpenConfig,
    CommandPalette,
    /// Next command-palette filter chip (PT-92). Unbound by default;
    /// Ctrl+Right inside the palette always works.
    PaletteFilterNext,
    /// Previous command-palette filter chip (PT-92). Unbound by default;
    /// Ctrl+Left inside the palette always works.
    PaletteFilterPrev,
    ThemePicker,
    /// Palette picker over saved spaces (PT-89). Unbound by default.
    OpenSpace,
    /// Palette picker that deletes a saved space (PT-89). Unbound by default.
    DeleteSpace,
    /// Move the focused mux-backed pane into another saved space (PT-182).
    /// Unbound by default.
    MovePaneToSpace,
    /// Move keyboard focus onto the spaces rail (PT-91). Unbound by default.
    SpaceRailFocus,
    SpaceSettings,
    UndoSpaceChange,
    /// Open the space after the current one in the rail (PT-91). Unbound.
    SpaceRailNext,
    /// Open the space before the current one in the rail (PT-91). Unbound.
    SpaceRailPrev,
    /// Save the live arrangement as a new space from the rail's inline name
    /// editor (PT-91). Unbound by default.
    SaveSpace,
    Find,
    ClearScrollback,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    /// Open or re-show the walkthrough caption (PT-193). Unbound by default.
    Walkthrough,
    /// Delete walkthrough.json and restart at level 0 (PT-195). Unbound.
    WalkthroughReset,
    Copy,
    Paste,
    SelectAll,
    ScrollLineUp,
    ScrollLineDown,
    /// Open the transparency dialog. Unbound. The palette lists it only when
    /// `chrome_style` is graphite (#112).
    Transparency,
    /// Open the Layout settings (Bars or Sidebar). Unbound. The palette lists
    /// it only when `chrome_style` is graphite (#150).
    ChromeLayout,
    /// Collapse or expand the Spaces sidebar (issue #174).
    SidebarCollapse,
    RichFocus,
}

impl Action {
    /// Every action, in documentation order.
    pub fn all() -> Vec<Action> {
        let mut all = vec![
            Action::SplitRight,
            Action::SplitDown,
            Action::ClosePane,
            Action::Detach,
            Action::FocusLeft,
            Action::FocusRight,
            Action::FocusUp,
            Action::FocusDown,
            Action::FocusBorderNext,
            Action::FocusBorderPrev,
            Action::BarColorNext,
            Action::BarColorPrev,
            Action::SwapPanePrev,
            Action::SwapPaneNext,
            Action::RotatePanes,
            Action::RotatePanesBack,
            Action::FocusLastPane,
            Action::NewTab,
            Action::NewBlankTab,
            Action::NewSessionTab,
            Action::BlankSplitRight,
            Action::BlankSplitDown,
            Action::SessionSplitRight,
            Action::SessionSplitDown,
            Action::TerminalSwitcher,
            Action::AgentMessages,
            Action::JumpNeedsYou,
            Action::UpdateRestart,
            Action::RecentMessages,
            Action::CloseTab,
            Action::RenameTab,
            Action::RenamePane,
            Action::PrevTab,
            Action::NextTab,
            Action::LastTab,
        ];
        all.extend((1..=9).map(Action::SelectTab));
        all.extend([
            Action::MovePanePrevTab,
            Action::MovePaneNextTab,
            Action::BreakPane,
            Action::JoinPane,
            Action::MoveTabLeft,
            Action::MoveTabRight,
        ]);
        all.extend((2..=9).map(Action::Layout));
        all.extend([
            Action::PresetSingle,
            Action::PresetSplitH,
            Action::PresetSplitV,
            Action::PresetGrid,
            Action::PresetMainVertical,
            Action::PresetMainHorizontal,
            Action::ArrangeSingle,
            Action::ArrangeSplit,
            Action::ArrangeGrid,
            Action::ZoomPane,
            Action::NewWindow,
            Action::Quit,
            Action::OpenConfig,
            Action::CommandPalette,
            Action::PaletteFilterNext,
            Action::PaletteFilterPrev,
            Action::ThemePicker,
            Action::OpenSpace,
            Action::DeleteSpace,
            Action::MovePaneToSpace,
            Action::SpaceRailFocus,
            Action::SpaceSettings,
            Action::UndoSpaceChange,
            Action::SpaceRailNext,
            Action::SpaceRailPrev,
            Action::SaveSpace,
            Action::Find,
            Action::ClearScrollback,
            Action::IncreaseFontSize,
            Action::DecreaseFontSize,
            Action::ResetFontSize,
            Action::Walkthrough,
            Action::WalkthroughReset,
            Action::Copy,
            Action::Paste,
            Action::SelectAll,
            Action::ScrollLineUp,
            Action::ScrollLineDown,
            Action::Transparency,
            Action::ChromeLayout,
            Action::SidebarCollapse,
            Action::RichFocus,
        ]);
        all
    }

    /// The `[keys]` name.
    pub fn name(self) -> String {
        match self {
            Action::SplitRight => "split_right".into(),
            Action::SplitDown => "split_down".into(),
            Action::ClosePane => "close_pane".into(),
            Action::Detach => "detach".into(),
            Action::FocusLeft => "focus_left".into(),
            Action::FocusRight => "focus_right".into(),
            Action::FocusUp => "focus_up".into(),
            Action::FocusDown => "focus_down".into(),
            Action::FocusBorderNext => "focus_border_next".into(),
            Action::FocusBorderPrev => "focus_border_prev".into(),
            Action::BarColorNext => "bar_color_next".into(),
            Action::BarColorPrev => "bar_color_prev".into(),
            Action::SwapPanePrev => "swap_pane_prev".into(),
            Action::SwapPaneNext => "swap_pane_next".into(),
            Action::RotatePanes => "rotate_panes".into(),
            Action::RotatePanesBack => "rotate_panes_back".into(),
            Action::FocusLastPane => "focus_last_pane".into(),
            Action::LastTab => "last_tab".into(),
            Action::NewTab => "new_tab".into(),
            Action::NewBlankTab => "new_blank_tab".into(),
            Action::NewSessionTab => "new_session_tab".into(),
            Action::BlankSplitRight => "blank_split_right".into(),
            Action::BlankSplitDown => "blank_split_down".into(),
            Action::SessionSplitRight => "session_split_right".into(),
            Action::SessionSplitDown => "session_split_down".into(),
            Action::TerminalSwitcher => "terminal_switcher".into(),
            Action::AgentMessages => "agent_messages".into(),
            Action::JumpNeedsYou => "jump_needs_you".into(),
            Action::UpdateRestart => "update_restart".into(),
            Action::RecentMessages => "recent_messages".into(),

            Action::CloseTab => "close_tab".into(),
            Action::RenameTab => "rename_tab".into(),
            Action::RenamePane => "rename_pane".into(),
            Action::PrevTab => "prev_tab".into(),
            Action::NextTab => "next_tab".into(),
            Action::SelectTab(n) => format!("select_tab_{n}"),
            Action::MovePanePrevTab => "move_pane_prev_tab".into(),
            Action::MovePaneNextTab => "move_pane_next_tab".into(),
            Action::BreakPane => "break_pane".into(),
            Action::JoinPane => "join_pane".into(),
            Action::MoveTabLeft => "move_tab_left".into(),
            Action::MoveTabRight => "move_tab_right".into(),
            Action::Layout(n) => format!("layout_{n}"),
            Action::PresetSingle => "preset_single".into(),
            Action::PresetSplitH => "preset_split_h".into(),
            Action::PresetSplitV => "preset_split_v".into(),
            Action::PresetGrid => "preset_grid".into(),
            Action::PresetMainVertical => "preset_main_vertical".into(),
            Action::PresetMainHorizontal => "preset_main_horizontal".into(),
            Action::ArrangeSingle => "arrange_single".into(),
            Action::ArrangeSplit => "arrange_split".into(),
            Action::ArrangeGrid => "arrange_grid".into(),
            Action::ZoomPane => "zoom_pane".into(),
            Action::NewWindow => "new_window".into(),
            Action::Quit => "quit".into(),
            Action::OpenConfig => "open_config".into(),
            Action::CommandPalette => "command_palette".into(),
            Action::PaletteFilterNext => "palette_filter_next".into(),
            Action::PaletteFilterPrev => "palette_filter_prev".into(),
            Action::ThemePicker => "theme_picker".into(),
            Action::OpenSpace => "open_space".into(),
            Action::DeleteSpace => "delete_space".into(),
            Action::MovePaneToSpace => "move_pane_to_space".into(),
            Action::SpaceRailFocus => "space_rail_focus".into(),
            Action::SpaceSettings => "space_settings".into(),
            Action::UndoSpaceChange => "undo_space_change".into(),
            Action::SpaceRailNext => "space_rail_next".into(),
            Action::SpaceRailPrev => "space_rail_prev".into(),
            Action::SaveSpace => "save_space".into(),
            Action::Find => "find".into(),
            Action::ClearScrollback => "clear_scrollback".into(),
            Action::IncreaseFontSize => "increase_font_size".into(),
            Action::DecreaseFontSize => "decrease_font_size".into(),
            Action::ResetFontSize => "reset_font_size".into(),
            Action::Walkthrough => "walkthrough".into(),
            Action::WalkthroughReset => "walkthrough_reset".into(),
            Action::Copy => "copy".into(),
            Action::Paste => "paste".into(),
            Action::SelectAll => "select_all".into(),
            Action::ScrollLineUp => "scroll_line_up".into(),
            Action::ScrollLineDown => "scroll_line_down".into(),
            Action::Transparency => "transparency".into(),
            Action::ChromeLayout => "chrome_layout".into(),
            Action::SidebarCollapse => "sidebar_collapse".into(),
            Action::RichFocus => "rich_focus".into(),
        }
    }

    /// Parse a `[keys]` name.
    pub fn from_name(name: &str) -> Option<Action> {
        Action::all().into_iter().find(|a| a.name() == name)
    }

    /// Short description for `--help` and docs.
    pub fn describe(self) -> String {
        match self {
            Action::SplitRight => "split the focused pane to the right".into(),
            Action::SplitDown => "split the focused pane downward".into(),
            Action::ClosePane => "close the focused pane".into(),
            Action::Detach => "detach this session view (last tab exits)".into(),
            Action::FocusLeft => "focus the pane to the left".into(),
            Action::FocusRight => "focus the pane to the right".into(),
            Action::FocusUp => "focus the pane above".into(),
            Action::FocusDown => "focus the pane below".into(),
            Action::FocusBorderNext => "cycle the focus border color forward".into(),
            Action::FocusBorderPrev => "cycle the focus border color back".into(),
            Action::BarColorNext => "cycle the Graphite bar color forward".into(),
            Action::BarColorPrev => "cycle the Graphite bar color back".into(),
            Action::SwapPanePrev => "swap the focused pane with the previous pane".into(),
            Action::SwapPaneNext => "swap the focused pane with the next pane".into(),
            Action::RotatePanes => "rotate every pane one slot forward".into(),
            Action::RotatePanesBack => "rotate every pane one slot back".into(),
            Action::FocusLastPane => "focus the previously focused pane in this tab".into(),
            Action::LastTab => "select the previously active tab".into(),
            Action::NewTab => "new tab".into(),
            Action::NewBlankTab => "new blank terminal tab".into(),
            Action::NewSessionTab => "new automatically named session tab".into(),
            Action::BlankSplitRight => "split right with a blank terminal".into(),
            Action::BlankSplitDown => "split down with a blank terminal".into(),
            Action::SessionSplitRight => "split right with an automatically named session".into(),
            Action::SessionSplitDown => "split down with an automatically named session".into(),
            Action::TerminalSwitcher => "find a terminal across Spaces".into(),
            Action::AgentMessages => "view pending mail and pane input queue receipts".into(),
            Action::JumpNeedsYou => "focus the next pane that needs your attention".into(),
            Action::UpdateRestart => "update, restart components, and inspect versions".into(),
            Action::RecentMessages => "show recent status messages, including hidden toasts".into(),

            Action::CloseTab => "close the active tab".into(),
            Action::RenameTab => "rename the active tab".into(),
            Action::RenamePane => {
                "name the focused session and mailbox (local shells: pane title)".into()
            }
            Action::PrevTab => "previous tab".into(),
            Action::NextTab => "next tab".into(),
            Action::SelectTab(n) => format!("select tab {n}"),
            Action::MovePanePrevTab => "move the focused pane to the previous tab".into(),
            Action::MovePaneNextTab => "move the focused pane to the next tab".into(),
            Action::BreakPane => "extract the focused pane into its own tab".into(),
            Action::JoinPane => "join the focused pane into the previous tab".into(),
            Action::MoveTabLeft => "move the active tab one slot left".into(),
            Action::MoveTabRight => "move the active tab one slot right".into(),
            Action::Layout(4) => "even 2×2 quadrant layout".into(),
            Action::Layout(n) => format!("even {n}-column layout"),
            Action::PresetSingle => {
                "retile to one pane when the tab has one pane; no-op otherwise".into()
            }
            Action::PresetSplitH => "retile existing panes into even columns".into(),
            Action::PresetSplitV => "retile existing panes into even rows".into(),
            Action::PresetGrid => "retile existing panes into an even two-row grid".into(),
            Action::PresetMainVertical => {
                "retile: focused pane on the left, others stacked on the right".into()
            }
            Action::PresetMainHorizontal => {
                "retile: focused pane on top, others in a row below".into()
            }
            Action::ArrangeSingle => {
                "arrange the tab as one pane; extra panes zoom instead of closing".into()
            }
            Action::ArrangeSplit => {
                "arrange the tab as a side-by-side split; extra panes zoom instead of closing"
                    .into()
            }
            Action::ArrangeGrid => {
                "arrange the tab as a 2×2 grid; extra panes zoom instead of closing".into()
            }
            Action::ZoomPane => {
                "zoom the focused pane to the whole tab; again restores the split".into()
            }
            Action::NewWindow => "open a new OS window".into(),
            Action::Quit => "quit Prismattyc".into(),
            Action::OpenConfig => "edit the config file".into(),
            Action::CommandPalette => "open the command palette".into(),
            Action::PaletteFilterNext => "next command-palette filter chip".into(),
            Action::PaletteFilterPrev => "previous command-palette filter chip".into(),
            Action::ThemePicker => "open theme settings".into(),
            Action::OpenSpace => "open a saved space".into(),
            Action::DeleteSpace => "delete a saved space".into(),
            Action::MovePaneToSpace => "move the focused pane to another saved space".into(),
            Action::SpaceRailFocus => "focus and reorder saved spaces".into(),
            Action::SpaceSettings => "choose rail position, autosave, and startup behavior".into(),
            Action::UndoSpaceChange => "undo the last session removal or move".into(),
            Action::SpaceRailNext => "open the next saved space".into(),
            Action::SpaceRailPrev => "open the previous saved space".into(),
            Action::SaveSpace => "save the current space arrangement".into(),
            Action::Find => "find in scrollback".into(),
            Action::ClearScrollback => "clear the screen and scrollback".into(),
            Action::IncreaseFontSize => "increase the font size".into(),
            Action::DecreaseFontSize => "decrease the font size".into(),
            Action::ResetFontSize => "reset the font size".into(),
            Action::Walkthrough => "open the walkthrough caption".into(),
            Action::WalkthroughReset => "delete walkthrough progress and restart at level 0".into(),
            Action::Copy => "copy the selection".into(),
            Action::Paste => "paste the clipboard".into(),
            Action::SelectAll => "select the visible viewport".into(),
            Action::ScrollLineUp => "scroll history up one line".into(),
            Action::ScrollLineDown => "scroll history down one line".into(),
            Action::Transparency => "open transparency settings (graphite chrome only)".into(),
            Action::ChromeLayout => {
                "choose the Bars or Sidebar layout (graphite chrome only)".into()
            }
            Action::SidebarCollapse => "collapse or expand the spaces sidebar".into(),
            Action::RichFocus => "toggle rich focus (--experimental-rich)".into(),
        }
    }
}

/// Command-palette filter chip / section for an action (PT-92). The chips,
/// the palette sections and `--help` read this one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionGroup {
    Panes,
    Tabs,
    Layout,
    Spaces,
    ViewEdit,
}

impl ActionGroup {
    /// Chip order in the palette.
    pub const ALL: [ActionGroup; 5] = [
        ActionGroup::Panes,
        ActionGroup::Tabs,
        ActionGroup::Layout,
        ActionGroup::Spaces,
        ActionGroup::ViewEdit,
    ];

    /// Chip label.
    pub fn label(self) -> &'static str {
        match self {
            ActionGroup::Panes => "Panes",
            ActionGroup::Tabs => "Tabs",
            ActionGroup::Layout => "Layout",
            ActionGroup::Spaces => "Spaces",
            ActionGroup::ViewEdit => "View & Edit",
        }
    }
}

impl Action {
    /// Palette group. Exhaustive: a new action must pick one.
    pub fn group(self) -> ActionGroup {
        match self {
            Action::SplitRight
            | Action::SplitDown
            | Action::ClosePane
            | Action::Detach
            | Action::FocusLeft
            | Action::FocusRight
            | Action::FocusUp
            | Action::FocusDown
            | Action::FocusBorderNext
            | Action::FocusBorderPrev
            | Action::BarColorNext
            | Action::BarColorPrev
            | Action::SwapPanePrev
            | Action::SwapPaneNext
            | Action::RotatePanes
            | Action::RotatePanesBack
            | Action::FocusLastPane
            | Action::ZoomPane
            | Action::MovePanePrevTab
            | Action::MovePaneNextTab
            | Action::BreakPane
            | Action::JoinPane
            | Action::RenamePane => ActionGroup::Panes,
            Action::NewBlankTab
            | Action::NewSessionTab
            | Action::BlankSplitRight
            | Action::BlankSplitDown
            | Action::SessionSplitRight
            | Action::SessionSplitDown
            | Action::NewTab
            | Action::CloseTab
            | Action::RenameTab
            | Action::PrevTab
            | Action::NextTab
            | Action::LastTab
            | Action::SelectTab(_)
            | Action::MoveTabLeft
            | Action::MoveTabRight => ActionGroup::Tabs,
            Action::Layout(_)
            | Action::PresetSingle
            | Action::PresetSplitH
            | Action::PresetSplitV
            | Action::PresetGrid
            | Action::PresetMainVertical
            | Action::PresetMainHorizontal
            | Action::ArrangeSingle
            | Action::ArrangeSplit
            | Action::ArrangeGrid => ActionGroup::Layout,
            Action::AgentMessages
            | Action::JumpNeedsYou
            | Action::TerminalSwitcher
            | Action::OpenSpace
            | Action::DeleteSpace
            | Action::MovePaneToSpace
            | Action::SpaceRailFocus
            | Action::SpaceSettings
            | Action::UndoSpaceChange
            | Action::SpaceRailNext
            | Action::SpaceRailPrev
            | Action::SaveSpace => ActionGroup::Spaces,
            Action::UpdateRestart
            | Action::RecentMessages
            | Action::NewWindow
            | Action::Quit
            | Action::OpenConfig
            | Action::CommandPalette
            | Action::PaletteFilterNext
            | Action::PaletteFilterPrev
            | Action::ThemePicker
            | Action::Find
            | Action::ClearScrollback
            | Action::IncreaseFontSize
            | Action::DecreaseFontSize
            | Action::ResetFontSize
            | Action::Walkthrough
            | Action::WalkthroughReset
            | Action::Copy
            | Action::Paste
            | Action::SelectAll
            | Action::ScrollLineUp
            | Action::ScrollLineDown
            | Action::Transparency
            | Action::ChromeLayout
            | Action::SidebarCollapse
            | Action::RichFocus => ActionGroup::ViewEdit,
        }
    }

    /// Extra sentence for the palette detail box, after [`Action::describe`].
    pub fn note(self) -> Option<&'static str> {
        match self {
            Action::SplitRight | Action::SplitDown => Some("New pane inherits the cwd."),
            Action::ClosePane => Some("The last pane in a tab closes the tab."),
            Action::Detach => Some("The mux session keeps running; reattach later."),
            Action::ZoomPane => Some("Any split, move, or close leaves zoom."),
            Action::SwapPanePrev | Action::SwapPaneNext => {
                Some("Panes trade places; splits and ratios stay; focus follows the pane.")
            }
            Action::RotatePanes | Action::RotatePanesBack => {
                Some("tmux rotate-window: every pane shifts one slot; the tree stays.")
            }
            Action::SelectTab(_) => Some("Enter, then press the tab digit."),
            Action::Layout(_) => Some("Enter, then press the column count. Spawns panes up to N."),
            Action::AgentMessages => Some("Read agent messages and inspect pending deliveries."),
            Action::JumpNeedsYou => Some("Clears attention on the pane you focus."),
            Action::UpdateRestart => Some("Check installed versions, update, or restart components."),
            Action::RecentMessages => Some("Settings choose which toasts show: all, errors, or off."),
            Action::PresetSingle
            | Action::PresetSplitH
            | Action::PresetSplitV
            | Action::PresetGrid
            | Action::PresetMainVertical
            | Action::PresetMainHorizontal => {
                Some("Retiles existing panes only; nothing spawns or closes.")
            }
            Action::ArrangeSingle | Action::ArrangeSplit | Action::ArrangeGrid => {
                Some("Growing spawns empty shells with focus unchanged; shrinking zooms the focused pane and never closes one.")
            }
            Action::OpenSpace => {
                Some("Picks from pmux space ls; opens tabs and panes in this window.")
            }
            Action::DeleteSpace => Some("Asks once before the file is removed."),
            Action::MovePaneToSpace => {
                Some("Move the focused pane to another saved Space. Blank terminals keep their local shell.")
            }
            Action::SpaceRailFocus => {
                Some("Arrows move; when space_reorder is enabled, Shift+arrows reorder. Enter opens, F2 renames, Delete asks, Esc returns to the pane.")
            }
            Action::SpaceRailNext | Action::SpaceRailPrev => {
                Some("Wraps around the rail; the chip becomes current.")
            }
            Action::SaveSpace => Some("Save the current Space. Use + to create a fresh Space."),
            Action::Find => Some("n / N step through matches; Esc leaves the prompt."),
            Action::Walkthrough => Some("Starts level 0. The pane stays usable."),
            Action::WalkthroughReset => {
                Some("Deletes walkthrough.json. The caption restarts at the first step.")
            }
            Action::RichFocus => Some("Needs --experimental-rich."),
            Action::Transparency => Some(
                "Writes the existing opacity keys. Listed in the palette only when chrome_style is graphite.",
            ),
            Action::ChromeLayout => Some(
                "Writes the layout key and applies it at once. Listed in the palette only when chrome_style is graphite.",
            ),
            Action::SidebarCollapse => Some(
                "Saves the collapsed state with the sidebar width. Applies when layout is sidebar.",
            ),
            _ => None,
        }
    }
}

/// The key half of a chord, normalized: letters lowercase, shifted glyphs
/// folded to the unshifted key on the same cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeySpec {
    Char(char),
    Named(NamedKey),
}

/// A modifier set plus one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
    pub key: KeySpec,
}

/// Unshifted key for a shifted US glyph (`|` → `\`).
fn unshift(c: char) -> char {
    match c {
        '~' => '`',
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        other => other.to_ascii_lowercase(),
    }
}

/// Physical scancodes that produce this character on a US layout, plus the
/// aliases Space controls matching already accepted.
fn codes_for_char(c: char) -> &'static [KeyCode] {
    match c {
        'a' => &[KeyCode::KeyA],
        'b' => &[KeyCode::KeyB],
        'c' => &[KeyCode::KeyC],
        'd' => &[KeyCode::KeyD],
        'e' => &[KeyCode::KeyE],
        'f' => &[KeyCode::KeyF],
        'g' => &[KeyCode::KeyG],
        'h' => &[KeyCode::KeyH],
        'i' => &[KeyCode::KeyI],
        'j' => &[KeyCode::KeyJ],
        'k' => &[KeyCode::KeyK],
        'l' => &[KeyCode::KeyL],
        'm' => &[KeyCode::KeyM],
        'n' => &[KeyCode::KeyN],
        'o' => &[KeyCode::KeyO],
        'p' => &[KeyCode::KeyP],
        'q' => &[KeyCode::KeyQ],
        'r' => &[KeyCode::KeyR],
        's' => &[KeyCode::KeyS],
        't' => &[KeyCode::KeyT],
        'u' => &[KeyCode::KeyU],
        'v' => &[KeyCode::KeyV],
        'w' => &[KeyCode::KeyW],
        'x' => &[KeyCode::KeyX],
        'y' => &[KeyCode::KeyY],
        'z' => &[KeyCode::KeyZ],
        '0' => &[KeyCode::Digit0],
        '1' => &[KeyCode::Digit1],
        '2' => &[KeyCode::Digit2],
        '3' => &[KeyCode::Digit3],
        '4' => &[KeyCode::Digit4],
        '5' => &[KeyCode::Digit5],
        '6' => &[KeyCode::Digit6],
        '7' => &[KeyCode::Digit7],
        '8' => &[KeyCode::Digit8],
        '9' => &[KeyCode::Digit9],
        '`' => &[KeyCode::Backquote],
        '-' => &[KeyCode::Minus, KeyCode::NumpadSubtract],
        '=' => &[KeyCode::Equal],
        '[' => &[KeyCode::BracketLeft],
        ']' => &[KeyCode::BracketRight],
        '\\' => &[KeyCode::Backslash, KeyCode::IntlBackslash],
        ';' => &[KeyCode::Semicolon],
        '\'' => &[KeyCode::Quote],
        ',' => &[KeyCode::Comma],
        '.' => &[KeyCode::Period],
        '/' => &[KeyCode::Slash],
        _ => &[],
    }
}

/// Named keys the grammar accepts: config spellings, winit logical key,
/// physical scancode, and the strip label.
const NAMED: &[(&[&str], NamedKey, Option<KeyCode>, &str)] = &[
    (
        &["enter", "return"],
        NamedKey::Enter,
        Some(KeyCode::Enter),
        "Enter",
    ),
    (&["tab"], NamedKey::Tab, Some(KeyCode::Tab), "Tab"),
    (
        &["backspace"],
        NamedKey::Backspace,
        Some(KeyCode::Backspace),
        "Backspace",
    ),
    (
        &["delete", "del"],
        NamedKey::Delete,
        Some(KeyCode::Delete),
        "Del",
    ),
    (
        &["escape", "esc"],
        NamedKey::Escape,
        Some(KeyCode::Escape),
        "Esc",
    ),
    (&["space"], NamedKey::Space, Some(KeyCode::Space), "Space"),
    (
        &["insert", "ins"],
        NamedKey::Insert,
        Some(KeyCode::Insert),
        "Ins",
    ),
    (&["up"], NamedKey::ArrowUp, Some(KeyCode::ArrowUp), "Up"),
    (
        &["down"],
        NamedKey::ArrowDown,
        Some(KeyCode::ArrowDown),
        "Down",
    ),
    (
        &["left"],
        NamedKey::ArrowLeft,
        Some(KeyCode::ArrowLeft),
        "Left",
    ),
    (
        &["right"],
        NamedKey::ArrowRight,
        Some(KeyCode::ArrowRight),
        "Right",
    ),
    (&["home"], NamedKey::Home, Some(KeyCode::Home), "Home"),
    (&["end"], NamedKey::End, Some(KeyCode::End), "End"),
    (
        &["pageup", "pgup"],
        NamedKey::PageUp,
        Some(KeyCode::PageUp),
        "PgUp",
    ),
    (
        &["pagedown", "pgdn"],
        NamedKey::PageDown,
        Some(KeyCode::PageDown),
        "PgDn",
    ),
    (&["f1"], NamedKey::F1, Some(KeyCode::F1), "F1"),
    (&["f2"], NamedKey::F2, Some(KeyCode::F2), "F2"),
    (&["f3"], NamedKey::F3, Some(KeyCode::F3), "F3"),
    (&["f4"], NamedKey::F4, Some(KeyCode::F4), "F4"),
    (&["f5"], NamedKey::F5, Some(KeyCode::F5), "F5"),
    (&["f6"], NamedKey::F6, Some(KeyCode::F6), "F6"),
    (&["f7"], NamedKey::F7, Some(KeyCode::F7), "F7"),
    (&["f8"], NamedKey::F8, Some(KeyCode::F8), "F8"),
    (&["f9"], NamedKey::F9, Some(KeyCode::F9), "F9"),
    (&["f10"], NamedKey::F10, Some(KeyCode::F10), "F10"),
    (&["f11"], NamedKey::F11, Some(KeyCode::F11), "F11"),
    (&["f12"], NamedKey::F12, Some(KeyCode::F12), "F12"),
    (&["f13"], NamedKey::F13, Some(KeyCode::F13), "F13"),
    (&["f14"], NamedKey::F14, Some(KeyCode::F14), "F14"),
    (&["f15"], NamedKey::F15, Some(KeyCode::F15), "F15"),
    (&["f16"], NamedKey::F16, Some(KeyCode::F16), "F16"),
    (&["f17"], NamedKey::F17, Some(KeyCode::F17), "F17"),
    (&["f18"], NamedKey::F18, Some(KeyCode::F18), "F18"),
    (&["f19"], NamedKey::F19, Some(KeyCode::F19), "F19"),
    (&["f20"], NamedKey::F20, Some(KeyCode::F20), "F20"),
    (&["f21"], NamedKey::F21, Some(KeyCode::F21), "F21"),
    (&["f22"], NamedKey::F22, Some(KeyCode::F22), "F22"),
    (&["f23"], NamedKey::F23, Some(KeyCode::F23), "F23"),
    (&["f24"], NamedKey::F24, Some(KeyCode::F24), "F24"),
];

/// Spelled-out punctuation accepted in place of the glyph.
const SPELLED: &[(&str, char)] = &[
    ("backslash", '\\'),
    ("minus", '-'),
    ("dash", '-'),
    ("equal", '='),
    ("equals", '='),
    ("bracketleft", '['),
    ("bracketright", ']'),
    ("semicolon", ';'),
    ("quote", '\''),
    ("apostrophe", '\''),
    ("comma", ','),
    ("period", '.'),
    ("dot", '.'),
    ("slash", '/'),
    ("backquote", '`'),
    ("grave", '`'),
    ("plus", '+'),
];

fn named_entry(
    key: NamedKey,
) -> Option<&'static (
    &'static [&'static str],
    NamedKey,
    Option<KeyCode>,
    &'static str,
)> {
    NAMED.iter().find(|entry| entry.1 == key)
}

impl Chord {
    /// Parse `mod+…+key` (case-insensitive, whitespace ignored). Shifted
    /// glyphs fold to their key (`|` → `\`); Shift must be named explicitly.
    pub fn parse(text: &str) -> Result<Chord, String> {
        let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if cleaned.is_empty() {
            return Err("empty chord".into());
        }
        // Split on '+' but keep a trailing '+' as the key itself ("ctrl++").
        let mut parts: Vec<&str> = Vec::new();
        let mut rest = cleaned.as_str();
        loop {
            match rest.find('+') {
                Some(0) => {
                    // A leading '+' is the key (only valid as the last part).
                    parts.push("+");
                    rest = &rest[1..];
                    if rest.is_empty() {
                        break;
                    }
                    return Err(format!("{text:?}: unexpected text after '+' key"));
                }
                Some(i) => {
                    parts.push(&rest[..i]);
                    rest = &rest[i + 1..];
                    if rest.is_empty() {
                        // "ctrl+shift+" — trailing separator with no key.
                        return Err(format!("{text:?}: missing key after '+'"));
                    }
                }
                None => {
                    parts.push(rest);
                    break;
                }
            }
        }
        let (key_text, mods) = parts.split_last().expect("at least one part");
        let mut chord = Chord {
            ctrl: false,
            shift: false,
            alt: false,
            super_key: false,
            key: KeySpec::Char('a'),
        };
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" | "option" | "opt" => chord.alt = true,
                "super" | "cmd" | "command" | "meta" | "win" | "logo" => chord.super_key = true,
                other => return Err(format!("{text:?}: unknown modifier {other:?}")),
            }
        }
        chord.key =
            parse_key(key_text).ok_or_else(|| format!("{text:?}: unknown key {key_text:?}"))?;
        if !(chord.ctrl || chord.alt || chord.super_key) {
            return Err(format!(
                "{text:?}: a chord needs ctrl, alt, or super (plain and shift-only keys go to the terminal)"
            ));
        }
        Ok(chord)
    }

    /// True when this key event is this chord. Ctrl/Shift/Alt exact; Super
    /// only when named. Physical scancode first, then the logical character
    /// (either glyph on the cap, case-insensitive).
    pub fn matches(&self, logical: &Key, physical: PhysicalKey, modifiers: ModifiersState) -> bool {
        if modifiers.control_key() != self.ctrl
            || modifiers.shift_key() != self.shift
            || modifiers.alt_key() != self.alt
        {
            return false;
        }
        if self.super_key && !modifiers.super_key() {
            return false;
        }
        match self.key {
            KeySpec::Char(c) => {
                if let PhysicalKey::Code(code) = physical {
                    if codes_for_char(c).contains(&code) {
                        return true;
                    }
                }
                match logical {
                    Key::Character(text) => {
                        let mut chars = text.chars();
                        match (chars.next(), chars.next()) {
                            (Some(got), None) => unshift(got) == c,
                            _ => false,
                        }
                    }
                    Key::Named(NamedKey::Space) => c == ' ',
                    _ => false,
                }
            }
            KeySpec::Named(named) => {
                if let Key::Named(got) = logical {
                    if *got == named {
                        return true;
                    }
                }
                if let (PhysicalKey::Code(code), Some(entry)) = (physical, named_entry(named)) {
                    if entry.2 == Some(code) {
                        return true;
                    }
                }
                if named == NamedKey::Space {
                    return matches!(logical, Key::Character(t) if t == " ");
                }
                false
            }
        }
    }

    /// Compact chord-strip label: `C-S-\`, `A-Left`, `Su-N`.
    pub fn label(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("C-");
        }
        if self.shift {
            out.push_str("S-");
        }
        if self.alt {
            out.push_str("A-");
        }
        if self.super_key {
            out.push_str("Su-");
        }
        match self.key {
            KeySpec::Char(c) => out.push(c.to_ascii_uppercase()),
            KeySpec::Named(n) => out.push_str(named_entry(n).map_or("?", |e| e.3)),
        }
        out
    }
}

impl fmt::Display for Chord {
    /// Canonical config spelling: `ctrl+shift+\`, `alt+left`, `super+n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.shift {
            f.write_str("shift+")?;
        }
        if self.alt {
            f.write_str("alt+")?;
        }
        if self.super_key {
            f.write_str("super+")?;
        }
        match self.key {
            KeySpec::Char(c) => write!(f, "{c}"),
            KeySpec::Named(n) => f.write_str(named_entry(n).map_or("?", |e| e.0[0])),
        }
    }
}

fn parse_key(text: &str) -> Option<KeySpec> {
    let lower = text.to_ascii_lowercase();
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let base = unshift(c);
        if base == ' ' {
            return Some(KeySpec::Named(NamedKey::Space));
        }
        if !codes_for_char(base).is_empty() {
            return Some(KeySpec::Char(base));
        }
        return None;
    }
    if let Some((_, c)) = SPELLED.iter().find(|(name, _)| *name == lower) {
        return Some(KeySpec::Char(unshift(*c)));
    }
    NAMED
        .iter()
        .find(|entry| entry.0.contains(&lower.as_str()))
        .map(|entry| KeySpec::Named(entry.1))
}

/// One `[keys]` value: a chord string or an array of chord strings.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(untagged)]
pub enum KeysValue {
    One(String),
    Many(Vec<String>),
}

impl KeysValue {
    fn chords(&self) -> Vec<&str> {
        match self {
            KeysValue::One(s) => vec![s.as_str()],
            KeysValue::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// Inputs that stay hard-coded (D-K3). A user chord equal to one of these is
/// a load error: it would be swallowed before the table, or break a
/// semantic rule (Ctrl+C with a selection copies, otherwise interrupts).
const FIXED: &[(&str, &str)] = &[
    (
        "ctrl+c",
        "plain Ctrl+C: copy with a selection, else interrupt (text selection D-H4)",
    ),
    ("ctrl+2", "keyboard-select mark (text selection D-H3)"),
    ("ctrl+space", "keyboard-select mark (text selection D-H3)"),
    ("shift+insert", "paste fallback (text selection D-H3)"),
    ("ctrl+shift+/", "find fallback for nested hosts"),
    ("ctrl+shift+;", "find fallback for nested hosts"),
    ("ctrl+shift+'", "find fallback for nested hosts"),
    ("ctrl+shift+.", "find fallback for nested hosts"),
];

fn fixed_chord(text: &str) -> Chord {
    // FIXED entries that lack ctrl/alt/super (shift+insert) are still
    // comparable; build them without the modifier rule.
    let mut chord = Chord {
        ctrl: false,
        shift: false,
        alt: false,
        super_key: false,
        key: KeySpec::Char('a'),
    };
    let mut parts: Vec<&str> = text.split('+').collect();
    let key = parts.pop().expect("key");
    for m in parts {
        match m {
            "ctrl" => chord.ctrl = true,
            "shift" => chord.shift = true,
            "alt" => chord.alt = true,
            "super" => chord.super_key = true,
            _ => {}
        }
    }
    chord.key = parse_key(key).expect("fixed table key parses");
    chord
}

/// Additional standard macOS Command shortcuts, opt-in through
/// `macos_shortcuts = true`. Cmd+Q and Cmd+N are existing defaults.
/// Entries in `[keys]` replace these along with each action's other defaults.
fn macos_chord(action: Action) -> Option<&'static str> {
    Some(match action {
        Action::NewTab => "super+t",
        Action::CloseTab => "super+w",
        Action::Find => "super+f",
        Action::Copy => "super+c",
        Action::Paste => "super+v",
        Action::SelectAll => "super+a",
        Action::ClearScrollback => "super+k",
        Action::IncreaseFontSize => "super+shift+=",
        Action::DecreaseFontSize => "super+-",
        Action::ResetFontSize => "super+0",
        _ => return None,
    })
}

/// Runtime defaults. macOS moves pane focus off Option+Left/Right unless
/// `[keys]` sets those actions. [`default_chords`] itself stays portable
/// because the config template is generated from it.
fn platform_default_chords(action: Action, mac_platform: bool) -> Vec<&'static str> {
    if mac_platform {
        match action {
            Action::FocusLeft => return vec!["ctrl+alt+left"],
            Action::FocusRight => return vec!["ctrl+alt+right"],
            _ => {}
        }
    }
    default_chords(action)
}

/// Default chords per action (keybindings D-K1). Reproduces the chords shipped
/// before user keybindings, including the macOS layout alternates.
pub(crate) fn default_chords(action: Action) -> Vec<&'static str> {
    match action {
        Action::SplitRight => vec!["ctrl+shift+\\", "ctrl+shift+e"],
        Action::SplitDown => vec!["ctrl+shift+-", "ctrl+shift+d"],
        Action::ClosePane => vec!["ctrl+shift+w"],
        Action::Detach => vec!["ctrl+shift+x"],
        Action::FocusLeft => vec!["alt+left"],
        Action::FocusRight => vec!["alt+right"],
        Action::FocusUp => vec!["alt+up"],
        Action::FocusDown => vec!["alt+down"],
        Action::FocusBorderNext => vec!["ctrl+shift+]"],
        Action::FocusBorderPrev => vec!["ctrl+shift+["],
        Action::BarColorNext => vec!["ctrl+shift+b"],
        Action::BarColorPrev => vec![],
        Action::NewTab => vec!["ctrl+shift+t"],
        Action::NewBlankTab => vec!["ctrl+alt+shift+t"],
        Action::NewSessionTab => vec!["ctrl+alt+shift+n"],
        Action::BlankSplitRight => vec!["ctrl+alt+shift+e"],
        Action::BlankSplitDown => vec!["ctrl+alt+shift+d"],
        Action::SessionSplitRight => vec!["ctrl+alt+shift+r"],
        Action::SessionSplitDown => vec!["ctrl+alt+shift+b"],
        Action::TerminalSwitcher => vec!["ctrl+shift+o"],
        Action::AgentMessages | Action::UpdateRestart | Action::RecentMessages => vec![],
        Action::JumpNeedsYou => vec!["ctrl+shift+u"],

        Action::CloseTab => vec!["ctrl+shift+q"],
        Action::RenameTab => vec!["ctrl+shift+r"],
        Action::RenamePane => vec![],
        Action::PrevTab => vec!["ctrl+shift+pageup"],
        Action::NextTab => vec!["ctrl+shift+pagedown"],
        Action::SelectTab(n) => vec![match n {
            1 => "ctrl+shift+1",
            2 => "ctrl+shift+2",
            3 => "ctrl+shift+3",
            4 => "ctrl+shift+4",
            5 => "ctrl+shift+5",
            6 => "ctrl+shift+6",
            7 => "ctrl+shift+7",
            8 => "ctrl+shift+8",
            _ => "ctrl+shift+9",
        }],
        Action::MovePanePrevTab => vec!["ctrl+shift+alt+pageup"],
        Action::MovePaneNextTab => vec!["ctrl+shift+alt+pagedown"],
        Action::BreakPane | Action::JoinPane => vec![],
        Action::MoveTabLeft | Action::MoveTabRight => vec![],
        Action::ZoomPane => vec!["ctrl+shift+z"],
        Action::PresetSingle
        | Action::PresetSplitH
        | Action::PresetSplitV
        | Action::PresetGrid
        | Action::PresetMainVertical
        | Action::PresetMainHorizontal
        | Action::ArrangeSingle
        | Action::ArrangeSplit
        | Action::ArrangeGrid => vec![],
        // Ctrl+Shift+Fn, plus the pre-ADR aliases: Alt co-held (some boards
        // report it with Fn), Cmd+Shift+Fn and Ctrl+Alt+digit for macOS,
        // which steals Control-F2…F8.
        Action::Layout(n) => match n {
            2 => vec![
                "ctrl+shift+f2",
                "ctrl+shift+alt+f2",
                "super+shift+f2",
                "ctrl+alt+2",
            ],
            3 => vec![
                "ctrl+shift+f3",
                "ctrl+shift+alt+f3",
                "super+shift+f3",
                "ctrl+alt+3",
            ],
            4 => vec![
                "ctrl+shift+f4",
                "ctrl+shift+alt+f4",
                "super+shift+f4",
                "ctrl+alt+4",
            ],
            5 => vec![
                "ctrl+shift+f5",
                "ctrl+shift+alt+f5",
                "super+shift+f5",
                "ctrl+alt+5",
            ],
            6 => vec![
                "ctrl+shift+f6",
                "ctrl+shift+alt+f6",
                "super+shift+f6",
                "ctrl+alt+6",
            ],
            7 => vec![
                "ctrl+shift+f7",
                "ctrl+shift+alt+f7",
                "super+shift+f7",
                "ctrl+alt+7",
            ],
            8 => vec![
                "ctrl+shift+f8",
                "ctrl+shift+alt+f8",
                "super+shift+f8",
                "ctrl+alt+8",
            ],
            _ => vec![
                "ctrl+shift+f9",
                "ctrl+shift+alt+f9",
                "super+shift+f9",
                "ctrl+alt+9",
            ],
        },
        Action::NewWindow => vec!["super+n"],
        Action::ClearScrollback
        | Action::IncreaseFontSize
        | Action::DecreaseFontSize
        | Action::ResetFontSize => vec![],
        Action::Quit => vec!["super+q"],
        Action::OpenConfig => vec![],
        Action::CommandPalette => vec!["ctrl+shift+p"],
        Action::PaletteFilterNext | Action::PaletteFilterPrev => vec![],
        Action::ThemePicker => vec!["ctrl+shift+,"],
        Action::OpenSpace => vec![],
        Action::DeleteSpace => vec![],
        Action::MovePaneToSpace => vec![],
        Action::SwapPanePrev => vec![],
        Action::SwapPaneNext => vec![],
        Action::RotatePanes => vec![],
        Action::RotatePanesBack => vec![],
        Action::FocusLastPane => vec![],
        Action::LastTab => vec![],
        Action::SpaceRailFocus | Action::SpaceSettings | Action::UndoSpaceChange => vec![],
        Action::SpaceRailNext => vec![],
        Action::SpaceRailPrev => vec![],
        Action::SaveSpace => vec![],
        Action::Find => vec!["ctrl+shift+f"],
        Action::Walkthrough => vec![],
        Action::WalkthroughReset => vec![],
        Action::Copy => vec!["ctrl+shift+c"],
        Action::Paste => vec!["ctrl+shift+v"],
        Action::SelectAll => vec!["ctrl+shift+a"],
        Action::ScrollLineUp => vec!["ctrl+shift+up"],
        Action::ScrollLineDown => vec!["ctrl+shift+down"],
        Action::Transparency => vec![],
        Action::ChromeLayout => vec![],
        Action::SidebarCollapse => vec!["ctrl+alt+shift+s"],
        Action::RichFocus => vec!["ctrl+shift+g"],
    }
}

/// The effective binding table: defaults with user overrides applied and
/// validated (D-K4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    /// Ordered (action, chord) pairs; no chord appears twice.
    bindings: Vec<(Action, Chord)>,
}

impl Default for KeyMap {
    fn default() -> Self {
        KeyMap::from_config(None).expect("default key table is valid")
    }
}

impl KeyMap {
    /// Build from the `[keys]` table. `None` or an empty table is the
    /// default map. Errors name the offending entry.
    pub fn from_config(keys: Option<&BTreeMap<String, KeysValue>>) -> Result<KeyMap, String> {
        Self::from_config_with_macos(keys, false)
    }

    /// Build defaults with optional macOS Command shortcuts, then apply
    /// per-action config overrides. Config entries replace all defaults.
    /// The portable map keeps `alt+left` / `alt+right`. The live host passes
    /// [`Self::from_config_with_platform`] so macOS can move focus off Option.
    pub fn from_config_with_macos(
        keys: Option<&BTreeMap<String, KeysValue>>,
        macos_shortcuts: bool,
    ) -> Result<KeyMap, String> {
        Self::from_config_with_platform(keys, macos_shortcuts, false)
    }

    /// Same as [`Self::from_config_with_macos`], plus the macOS runtime
    /// overlay. When `mac_platform` is set and `[keys]` omits
    /// `focus_left` / `focus_right`, those defaults become
    /// `ctrl+alt+left` / `ctrl+alt+right` so Option+Left/Right can reach
    /// the shell as word jumps. The generated template leaves those two
    /// keys commented; a live `focus_left = "alt+left"` still pins Option.
    pub fn from_config_with_platform(
        keys: Option<&BTreeMap<String, KeysValue>>,
        macos_shortcuts: bool,
        mac_platform: bool,
    ) -> Result<KeyMap, String> {
        let mut bindings: Vec<(Action, Chord)> = Vec::new();
        let mut user_chords: Vec<(Action, Chord)> = Vec::new();
        for action in Action::all() {
            let name = action.name();
            let user = keys.and_then(|k| k.get(&name));
            match user {
                Some(value) => {
                    for text in value.chords() {
                        let chord = Chord::parse(text).map_err(|e| format!("keys.{name}: {e}"))?;
                        if !bindings.contains(&(action, chord)) {
                            bindings.push((action, chord));
                            user_chords.push((action, chord));
                        }
                    }
                }
                None => {
                    for text in platform_default_chords(action, mac_platform) {
                        let chord = Chord::parse(text).expect("default chord parses");
                        bindings.push((action, chord));
                    }
                    if macos_shortcuts {
                        if let Some(text) = macos_chord(action) {
                            let chord = Chord::parse(text).expect("macOS chord parses");
                            if !bindings.contains(&(action, chord)) {
                                bindings.push((action, chord));
                            }
                        }
                    }
                }
            }
        }
        if let Some(keys) = keys {
            for name in keys.keys() {
                if Action::from_name(name).is_none() {
                    return Err(format!(
                        "keys: unknown action {name:?}; see `prismattyc-host --help` for the action names"
                    ));
                }
            }
        }
        for (action, chord) in &user_chords {
            for (text, why) in FIXED {
                if fixed_chord(text) == *chord {
                    return Err(format!(
                        "keys.{}: {chord} is reserved — {why}",
                        action.name()
                    ));
                }
            }
        }
        for (i, (a, chord)) in bindings.iter().enumerate() {
            if let Some((b, _)) = bindings[i + 1..].iter().find(|(_, other)| other == chord) {
                return Err(format!(
                    "keys: {} and {} are both bound to {chord}",
                    a.name(),
                    b.name()
                ));
            }
        }
        Ok(KeyMap { bindings })
    }

    /// Actions whose macOS Command shortcut is suppressed by an explicit
    /// non-empty `[keys]` entry that does not bind the Command chord.
    /// Each entry is the action plus the suppressed macOS chord spelling.
    /// Empty entries (`[]`) intentionally unbind the action and are skipped.
    /// Unparseable user chords are skipped here; `from_config_with_macos`
    /// still rejects them as load errors.
    pub fn macos_suppressed_actions(
        keys: Option<&BTreeMap<String, KeysValue>>,
    ) -> Vec<(Action, String)> {
        let Some(keys) = keys else {
            return Vec::new();
        };
        let mut suppressed = Vec::new();
        for action in Action::all() {
            let Some(macos) = macos_chord(action) else {
                continue;
            };
            let Some(value) = keys.get(&action.name()) else {
                continue;
            };
            let texts = value.chords();
            if texts.is_empty() {
                continue;
            }
            let Ok(expected) = Chord::parse(macos) else {
                continue;
            };
            let mut keeps_macos = false;
            for text in texts {
                if let Ok(chord) = Chord::parse(text) {
                    if chord == expected {
                        keeps_macos = true;
                        break;
                    }
                }
            }
            if !keeps_macos {
                suppressed.push((action, macos.to_string()));
            }
        }
        suppressed
    }

    /// One row per host action, including actions without an active chord.
    pub fn listing(&self) -> String {
        Action::all()
            .into_iter()
            .map(|action| {
                let chords = self.spellings(action);
                let chords = if chords.is_empty() {
                    "(unbound)".to_string()
                } else {
                    chords.join(", ")
                };
                format!("{:<22} {chords}", action.name())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The action for a key event, if any chord matches.
    pub fn action(
        &self,
        logical: &Key,
        physical: PhysicalKey,
        modifiers: ModifiersState,
    ) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(_, chord)| chord.matches(logical, physical, modifiers))
            .map(|(action, _)| *action)
    }

    /// Chords bound to `action`, in table order.
    pub fn chords(&self, action: Action) -> Vec<Chord> {
        self.bindings
            .iter()
            .filter(|(a, _)| *a == action)
            .map(|(_, c)| *c)
            .collect()
    }

    /// Chord-strip label for an action: chords joined with `/`, sharing the
    /// modifier prefix when all chords have the same modifiers (`C-S-\/E`).
    /// Empty string when unbound.
    pub fn label(&self, action: Action) -> String {
        let chords = self.chords(action);
        let Some(first) = chords.first() else {
            return String::new();
        };
        let same_mods = chords.iter().all(|c| {
            (c.ctrl, c.shift, c.alt, c.super_key)
                == (first.ctrl, first.shift, first.alt, first.super_key)
        });
        if same_mods {
            let full = first.label();
            let prefix_len = full.len() - key_label(first).len();
            let prefix = &full[..prefix_len];
            let keys: Vec<String> = chords.iter().map(key_label).collect();
            format!("{prefix}{}", keys.join("/"))
        } else {
            chords
                .iter()
                .map(Chord::label)
                .collect::<Vec<_>>()
                .join("/")
        }
    }

    /// Canonical config spellings for `--help` and docs.
    pub fn spellings(&self, action: Action) -> Vec<String> {
        self.chords(action)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Palette chord column: the chords that share the first chord's
    /// modifiers, each in full, joined by ` · ` (`C-S-\ · C-S-E`). The
    /// Alt-co-held, Cmd and Ctrl+Alt aliases are omitted. Empty when unbound.
    pub fn palette_label(&self, action: Action) -> String {
        let chords = self.chords(action);
        let Some(first) = chords.first() else {
            return String::new();
        };
        chords
            .iter()
            .filter(|c| {
                (c.ctrl, c.shift, c.alt, c.super_key)
                    == (first.ctrl, first.shift, first.alt, first.super_key)
            })
            .map(Chord::label)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

fn key_label(chord: &Chord) -> String {
    match chord.key {
        KeySpec::Char(c) => c.to_ascii_uppercase().to_string(),
        KeySpec::Named(n) => named_entry(n).map_or("?", |e| e.3).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, shift: bool, alt: bool, sup: bool) -> ModifiersState {
        let mut m = ModifiersState::empty();
        m.set(ModifiersState::CONTROL, ctrl);
        m.set(ModifiersState::SHIFT, shift);
        m.set(ModifiersState::ALT, alt);
        m.set(ModifiersState::SUPER, sup);
        m
    }

    fn one(name: &str, value: &str) -> BTreeMap<String, KeysValue> {
        BTreeMap::from([(name.to_string(), KeysValue::One(value.to_string()))])
    }

    fn unbound_by_default(action: Action) -> bool {
        matches!(
            action,
            Action::AgentMessages
                | Action::UpdateRestart
                | Action::RecentMessages
                | Action::PresetSingle
                | Action::PresetSplitH
                | Action::PresetSplitV
                | Action::PresetGrid
                | Action::PresetMainVertical
                | Action::PresetMainHorizontal
                | Action::ArrangeSingle
                | Action::ArrangeSplit
                | Action::ArrangeGrid
                | Action::RenamePane
                | Action::MoveTabLeft
                | Action::MoveTabRight
                | Action::OpenSpace
                | Action::DeleteSpace
                | Action::MovePaneToSpace
                | Action::SwapPanePrev
                | Action::SwapPaneNext
                | Action::BreakPane
                | Action::JoinPane
                | Action::RotatePanes
                | Action::RotatePanesBack
                | Action::FocusLastPane
                | Action::LastTab
                | Action::SpaceRailFocus
                | Action::SpaceSettings
                | Action::UndoSpaceChange
                | Action::SpaceRailNext
                | Action::SpaceRailPrev
                | Action::SaveSpace
                | Action::OpenConfig
                | Action::PaletteFilterNext
                | Action::PaletteFilterPrev
                | Action::Walkthrough
                | Action::WalkthroughReset
                | Action::ClearScrollback
                | Action::IncreaseFontSize
                | Action::DecreaseFontSize
                | Action::ResetFontSize
                | Action::BarColorPrev
                | Action::Transparency
                | Action::ChromeLayout
        )
    }

    #[test]
    fn break_and_join_pane_actions_exist_with_no_default_chord() {
        let map = KeyMap::default();
        for action in [Action::BreakPane, Action::JoinPane] {
            assert_eq!(Action::from_name(&action.name()), Some(action));
            assert!(
                map.chords(action).is_empty(),
                "{} must ship unbound",
                action.name()
            );
            assert_eq!(map.label(action), "");
            assert_eq!(action.group(), ActionGroup::Panes);
        }
        let bound = KeyMap::from_config(Some(&one("break_pane", "ctrl+alt+b"))).unwrap();
        assert_eq!(
            bound.spellings(Action::BreakPane),
            vec!["ctrl+alt+b".to_string()]
        );
        assert!(bound.chords(Action::JoinPane).is_empty());
    }

    #[test]
    fn move_tab_actions_exist_with_no_default_chord() {
        let map = KeyMap::default();
        for action in [Action::MoveTabLeft, Action::MoveTabRight] {
            assert_eq!(Action::from_name(&action.name()), Some(action));
            assert!(
                map.chords(action).is_empty(),
                "{} must ship unbound",
                action.name()
            );
            assert_eq!(map.label(action), "");
        }
        let bound = KeyMap::from_config(Some(&one("move_tab_left", "ctrl+alt+pageup"))).unwrap();
        assert_eq!(
            bound.spellings(Action::MoveTabLeft),
            vec!["ctrl+alt+pageup".to_string()]
        );
        assert!(bound.chords(Action::MoveTabRight).is_empty());
    }

    #[test]
    fn preset_actions_exist_with_no_default_chord() {
        let map = KeyMap::default();
        for action in [
            Action::PresetSingle,
            Action::PresetSplitH,
            Action::PresetSplitV,
            Action::PresetGrid,
            Action::PresetMainVertical,
            Action::PresetMainHorizontal,
        ] {
            assert_eq!(Action::from_name(&action.name()), Some(action));
            assert!(
                map.chords(action).is_empty(),
                "{} must ship unbound",
                action.name()
            );
            assert_eq!(map.label(action), "");
        }
        let bound = KeyMap::from_config(Some(&one("preset_grid", "ctrl+alt+g"))).unwrap();
        assert_eq!(
            bound.spellings(Action::PresetGrid),
            vec!["ctrl+alt+g".to_string()]
        );
        assert!(bound.chords(Action::PresetSingle).is_empty());
    }

    #[test]
    fn arrange_actions_exist_with_no_default_chord() {
        let map = KeyMap::default();
        for action in [
            Action::ArrangeSingle,
            Action::ArrangeSplit,
            Action::ArrangeGrid,
        ] {
            assert_eq!(Action::from_name(&action.name()), Some(action));
            assert!(
                map.chords(action).is_empty(),
                "{} must ship unbound",
                action.name()
            );
            assert_eq!(map.label(action), "");
            assert_eq!(action.group(), ActionGroup::Layout);
            assert!(action.note().is_some_and(|note| !note.is_empty()));
        }
        assert_eq!(Action::ArrangeSingle.name(), "arrange_single");
        assert_eq!(Action::ArrangeSplit.name(), "arrange_split");
        assert_eq!(Action::ArrangeGrid.name(), "arrange_grid");
        let bound = KeyMap::from_config(Some(&one("arrange_grid", "ctrl+alt+g"))).unwrap();
        assert_eq!(
            bound.spellings(Action::ArrangeGrid),
            vec!["ctrl+alt+g".to_string()]
        );
        assert!(bound.chords(Action::ArrangeSingle).is_empty());
    }

    #[test]
    fn recent_messages_action_is_listed_and_unbound() {
        let map = KeyMap::default();
        let action = Action::RecentMessages;
        assert_eq!(action.name(), "recent_messages");
        assert_eq!(Action::from_name("recent_messages"), Some(action));
        assert!(map.chords(action).is_empty());
        assert_eq!(action.group(), ActionGroup::ViewEdit);
        assert!(action.describe().contains("hidden"));
    }

    #[test]
    fn space_rail_actions_exist_with_no_default_chord() {
        let map = KeyMap::default();
        for action in [
            Action::SpaceRailFocus,
            Action::SpaceSettings,
            Action::UndoSpaceChange,
            Action::SpaceRailNext,
            Action::SpaceRailPrev,
            Action::SaveSpace,
        ] {
            assert_eq!(Action::from_name(&action.name()), Some(action));
            assert!(
                map.chords(action).is_empty(),
                "{} must ship unbound",
                action.name()
            );
            assert_eq!(map.label(action), "");
            assert_eq!(action.group(), ActionGroup::Spaces);
        }
        let bound = KeyMap::from_config(Some(&one("space_rail_focus", "ctrl+alt+s"))).unwrap();
        assert_eq!(
            bound.spellings(Action::SpaceRailFocus),
            vec!["ctrl+alt+s".to_string()]
        );
        assert!(bound.chords(Action::SpaceRailNext).is_empty());
    }

    #[test]
    fn space_picker_actions_exist_with_no_default_chord() {
        let map = KeyMap::default();
        for action in [
            Action::OpenSpace,
            Action::DeleteSpace,
            Action::MovePaneToSpace,
        ] {
            assert_eq!(Action::from_name(&action.name()), Some(action));
            assert!(
                map.chords(action).is_empty(),
                "{} must ship unbound",
                action.name()
            );
            assert_eq!(map.label(action), "");
        }
        let bound = KeyMap::from_config(Some(&one("open_space", "ctrl+alt+o"))).unwrap();
        assert_eq!(
            bound.spellings(Action::OpenSpace),
            vec!["ctrl+alt+o".to_string()]
        );
        assert!(bound.chords(Action::DeleteSpace).is_empty());
    }

    #[test]
    fn action_names_round_trip_and_are_unique() {
        let all = Action::all();
        let mut names: Vec<String> = all.iter().map(|a| a.name()).collect();
        for (action, name) in all.iter().zip(&names) {
            assert_eq!(Action::from_name(name), Some(*action));
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len());
        assert_eq!(Action::from_name("nope"), None);
    }

    #[test]
    fn chord_grammar_parses_modifiers_keys_and_aliases() {
        let c = Chord::parse("Ctrl + Shift + \\").unwrap();
        assert!(c.ctrl && c.shift && !c.alt && !c.super_key);
        assert_eq!(c.key, KeySpec::Char('\\'));
        assert_eq!(
            Chord::parse("ctrl+shift+|").unwrap(),
            c,
            "shifted glyph folds"
        );
        assert_eq!(Chord::parse("control+shift+backslash").unwrap(), c);
        assert_eq!(c.to_string(), "ctrl+shift+\\");
        assert_eq!(c.label(), "C-S-\\");

        let f = Chord::parse("cmd+shift+F2").unwrap();
        assert!(f.super_key && f.shift);
        assert_eq!(f.key, KeySpec::Named(NamedKey::F2));
        assert_eq!(
            f.to_string(),
            "shift+super+f2",
            "canonical order is ctrl, shift, alt, super"
        );
        assert_eq!(f.label(), "S-Su-F2");

        let pg = Chord::parse("alt+PgUp").unwrap();
        assert_eq!(pg.key, KeySpec::Named(NamedKey::PageUp));
        assert_eq!(pg.to_string(), "alt+pageup");
        assert_eq!(Chord::parse("ctrl++").unwrap().key, KeySpec::Char('='));
        assert_eq!(
            Chord::parse("ctrl+space").unwrap().key,
            KeySpec::Named(NamedKey::Space)
        );
    }

    #[test]
    fn chord_grammar_rejects_bad_input_without_panicking() {
        for bad in [
            "",
            "+",
            "ctrl+",
            "ctrl+shift+",
            "hyper+a",
            "ctrl+f99",
            "ctrl+ä",
            "a",
            "shift+a",
            "shift+f2",
            "ctrl+a+b",
        ] {
            assert!(Chord::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(Chord::parse("ctrl+shift+")
            .unwrap_err()
            .contains("missing key"));
        assert!(Chord::parse("shift+a")
            .unwrap_err()
            .contains("needs ctrl, alt, or super"));
    }

    #[test]
    fn matching_is_physical_first_then_logical_and_exact_on_ctrl_shift_alt() {
        let c = Chord::parse("ctrl+shift+\\").unwrap();
        let cs = mods(true, true, false, false);
        // Physical wins even with empty logical text.
        assert!(c.matches(
            &Key::Character("".into()),
            PhysicalKey::Code(KeyCode::Backslash),
            cs
        ));
        assert!(c.matches(
            &Key::Character("".into()),
            PhysicalKey::Code(KeyCode::IntlBackslash),
            cs
        ));
        // Logical glyph on either side of Shift.
        let bare = PhysicalKey::Code(KeyCode::KeyA);
        assert!(c.matches(&Key::Character("|".into()), bare, cs));
        assert!(c.matches(&Key::Character("\\".into()), bare, cs));
        // Wrong modifiers.
        assert!(!c.matches(
            &Key::Character("|".into()),
            bare,
            mods(true, false, false, false)
        ));
        assert!(!c.matches(
            &Key::Character("|".into()),
            bare,
            mods(true, true, true, false)
        ));
        // Super tolerated when not named.
        assert!(c.matches(
            &Key::Character("|".into()),
            bare,
            mods(true, true, false, true)
        ));
        // Super required when named.
        let n = Chord::parse("super+n").unwrap();
        assert!(n.matches(
            &Key::Character("n".into()),
            bare,
            mods(false, false, false, true)
        ));
        assert!(!n.matches(
            &Key::Character("n".into()),
            bare,
            mods(false, false, false, false)
        ));
        assert!(!n.matches(
            &Key::Character("N".into()),
            bare,
            mods(false, true, false, true)
        ));
        // Case-insensitive letters.
        let w = Chord::parse("ctrl+shift+w").unwrap();
        assert!(w.matches(&Key::Character("W".into()), bare, cs));
        // Named keys: logical, physical, and the macOS Fn shape.
        let f2 = Chord::parse("ctrl+shift+f2").unwrap();
        let unidentified = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        assert!(f2.matches(&Key::Named(NamedKey::F2), unidentified, cs));
        assert!(f2.matches(
            &Key::Named(NamedKey::Fn),
            PhysicalKey::Code(KeyCode::F2),
            cs
        ));
        assert!(!f2.matches(&Key::Named(NamedKey::F3), unidentified, cs));
        let minus = Chord::parse("ctrl+shift+-").unwrap();
        assert!(minus.matches(
            &Key::Character("".into()),
            PhysicalKey::Code(KeyCode::NumpadSubtract),
            cs
        ));
        assert!(minus.matches(&Key::Character("_".into()), bare, cs));
    }

    #[test]
    fn default_map_has_no_overlaps_and_covers_every_action() {
        let map = KeyMap::default();
        for action in Action::all() {
            if unbound_by_default(action) {
                assert!(
                    map.chords(action).is_empty(),
                    "{} must be unbound by default",
                    action.name()
                );
                continue;
            }
            assert!(
                !map.chords(action).is_empty(),
                "{} has no default",
                action.name()
            );
        }
        let cs = mods(true, true, false, false);
        assert_eq!(
            map.action(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::KeyF),
                cs
            ),
            Some(Action::Find)
        );
        assert_eq!(
            map.action(
                &Key::Character("<".into()),
                PhysicalKey::Code(KeyCode::Comma),
                cs
            ),
            Some(Action::ThemePicker)
        );
        assert_eq!(
            map.action(
                &Key::Character("p".into()),
                PhysicalKey::Code(KeyCode::KeyP),
                cs
            ),
            Some(Action::CommandPalette)
        );
        assert_eq!(
            map.action(
                &Key::Character("2".into()),
                PhysicalKey::Code(KeyCode::Digit2),
                cs
            ),
            Some(Action::SelectTab(2))
        );
        assert_eq!(
            map.action(
                &Key::Character("2".into()),
                PhysicalKey::Code(KeyCode::Digit2),
                mods(true, false, true, false)
            ),
            Some(Action::Layout(2))
        );
        assert_eq!(
            map.action(
                &Key::Named(NamedKey::ArrowLeft),
                PhysicalKey::Code(KeyCode::ArrowLeft),
                mods(false, false, true, false)
            ),
            Some(Action::FocusLeft)
        );
        assert_eq!(
            map.action(
                &Key::Named(NamedKey::ArrowLeft),
                PhysicalKey::Code(KeyCode::ArrowLeft),
                mods(true, false, true, false)
            ),
            None
        );
        assert_eq!(
            map.action(
                &Key::Character("n".into()),
                PhysicalKey::Code(KeyCode::KeyN),
                mods(false, false, false, true)
            ),
            Some(Action::NewWindow)
        );
        assert_eq!(
            map.action(
                &Key::Character("c".into()),
                PhysicalKey::Code(KeyCode::KeyC),
                mods(true, false, false, false)
            ),
            None,
            "plain Ctrl+C is not a table action"
        );
    }

    #[test]
    fn labels_share_modifier_prefix_when_possible() {
        let map = KeyMap::default();
        assert_eq!(map.label(Action::SplitRight), "C-S-\\/E");
        assert_eq!(map.label(Action::SplitDown), "C-S--/D");
        assert_eq!(map.label(Action::ThemePicker), "C-S-,");
        assert_eq!(map.label(Action::FocusLeft), "A-Left");
        assert_eq!(
            map.label(Action::Layout(2)),
            "C-S-F2/C-S-A-F2/S-Su-F2/C-A-2"
        );
        assert_eq!(
            map.spellings(Action::Paste),
            vec!["ctrl+shift+v".to_string()]
        );
    }

    #[test]
    fn user_entry_replaces_defaults_and_can_unbind() {
        let map = KeyMap::from_config(Some(&one("split_right", "ctrl+alt+enter"))).unwrap();
        assert_eq!(
            map.spellings(Action::SplitRight),
            vec!["ctrl+alt+enter".to_string()]
        );
        let cs = mods(true, true, false, false);
        assert_eq!(
            map.action(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::Backslash),
                cs
            ),
            None,
            "old default gone"
        );
        assert_eq!(
            map.action(
                &Key::Named(NamedKey::Enter),
                PhysicalKey::Code(KeyCode::Enter),
                mods(true, false, true, false)
            ),
            Some(Action::SplitRight)
        );
        // Other actions keep their defaults.
        assert_eq!(map.label(Action::SplitDown), "C-S--/D");

        let unbound = KeyMap::from_config(Some(&BTreeMap::from([(
            "theme_picker".to_string(),
            KeysValue::Many(vec![]),
        )])))
        .unwrap();
        assert!(unbound.chords(Action::ThemePicker).is_empty());
        assert_eq!(unbound.label(Action::ThemePicker), "");

        let aliases = KeyMap::from_config(Some(&BTreeMap::from([(
            "find".to_string(),
            KeysValue::Many(vec![
                "ctrl+shift+f".into(),
                "ctrl+alt+f".into(),
                "ctrl+shift+f".into(),
            ]),
        )])))
        .unwrap();
        assert_eq!(
            aliases.chords(Action::Find).len(),
            2,
            "duplicate alias collapses"
        );
    }

    #[test]
    fn macos_shortcuts_are_opt_in_and_user_entries_replace_them() {
        let defaults = KeyMap::default();
        assert_eq!(defaults.spellings(Action::Copy), vec!["ctrl+shift+c"]);
        assert_eq!(defaults.spellings(Action::Quit), vec!["super+q"]);

        let mac = KeyMap::from_config_with_macos(None, true).unwrap();
        assert_eq!(mac.spellings(Action::Copy), vec!["ctrl+shift+c", "super+c"]);
        assert_eq!(
            mac.spellings(Action::NewTab),
            vec!["ctrl+shift+t", "super+t"]
        );
        assert_eq!(mac.spellings(Action::Quit), vec!["super+q"]);
        assert_eq!(
            mac.spellings(Action::FocusLeft),
            vec!["alt+left"],
            "the portable macOS-shortcuts map does not move focus"
        );
        assert_eq!(
            mac.spellings(Action::IncreaseFontSize),
            vec!["shift+super+="]
        );
        let listing = mac.listing();
        assert!(listing.contains("clear_scrollback"));
        assert!(listing.contains("super+k"));

        let overrides = BTreeMap::from([
            ("copy".to_string(), KeysValue::Many(vec![])),
            ("quit".to_string(), KeysValue::Many(vec![])),
            (
                "new_window".to_string(),
                KeysValue::One("ctrl+alt+n".into()),
            ),
        ]);
        let custom = KeyMap::from_config_with_macos(Some(&overrides), true).unwrap();
        assert!(custom.chords(Action::Copy).is_empty());
        assert!(custom.chords(Action::Quit).is_empty());
        assert_eq!(custom.spellings(Action::NewWindow), vec!["ctrl+alt+n"]);
    }

    #[test]
    fn macos_platform_moves_horizontal_focus_off_option_unless_overridden() {
        let mac = KeyMap::from_config_with_platform(None, false, true).unwrap();
        assert_eq!(mac.spellings(Action::FocusLeft), vec!["ctrl+alt+left"]);
        assert_eq!(mac.spellings(Action::FocusRight), vec!["ctrl+alt+right"]);
        assert_eq!(mac.spellings(Action::FocusUp), vec!["alt+up"]);
        assert_eq!(mac.spellings(Action::FocusDown), vec!["alt+down"]);
        assert_eq!(mac.label(Action::FocusLeft), "C-A-Left");
        let alt = mods(false, false, true, false);
        let ctrl_alt = mods(true, false, true, false);
        assert_eq!(
            mac.action(
                &Key::Named(NamedKey::ArrowLeft),
                PhysicalKey::Code(KeyCode::ArrowLeft),
                alt
            ),
            None,
            "Option+Left is free for the shell word jump"
        );
        assert_eq!(
            mac.action(
                &Key::Named(NamedKey::ArrowLeft),
                PhysicalKey::Code(KeyCode::ArrowLeft),
                ctrl_alt
            ),
            Some(Action::FocusLeft)
        );
        assert_eq!(
            mac.action(
                &Key::Named(NamedKey::ArrowUp),
                PhysicalKey::Code(KeyCode::ArrowUp),
                alt
            ),
            Some(Action::FocusUp)
        );

        let linux = KeyMap::from_config_with_platform(None, false, false).unwrap();
        assert_eq!(linux, KeyMap::default());
        assert_eq!(linux.spellings(Action::FocusLeft), vec!["alt+left"]);

        let keep =
            KeyMap::from_config_with_platform(Some(&one("focus_left", "alt+left")), false, true)
                .unwrap();
        assert_eq!(keep.spellings(Action::FocusLeft), vec!["alt+left"]);
        assert_eq!(keep.spellings(Action::FocusRight), vec!["ctrl+alt+right"]);
        assert_eq!(
            keep.action(
                &Key::Named(NamedKey::ArrowLeft),
                PhysicalKey::Code(KeyCode::ArrowLeft),
                alt
            ),
            Some(Action::FocusLeft),
            "an explicit [keys] entry still consumes Option+Left"
        );

        let conflict = BTreeMap::from([(
            "split_right".to_string(),
            KeysValue::One("ctrl+alt+left".into()),
        )]);
        let err = KeyMap::from_config_with_platform(Some(&conflict), false, true).unwrap_err();
        assert!(
            err.contains("focus_left") && err.contains("split_right"),
            "{err}"
        );
    }

    #[test]
    fn macos_suppressed_actions_reports_overrides_but_not_unbinds() {
        // No table: nothing is suppressed.
        assert!(KeyMap::macos_suppressed_actions(None).is_empty());

        // The issue example: an explicit copy binding without super+c.
        let overrides =
            BTreeMap::from([("copy".to_string(), KeysValue::One("ctrl+shift+c".into()))]);
        let suppressed = KeyMap::macos_suppressed_actions(Some(&overrides));
        assert_eq!(suppressed, vec![(Action::Copy, "super+c".to_string())]);
        // The replacement rule itself is unchanged: only the explicit chord.
        let map = KeyMap::from_config_with_macos(Some(&overrides), true).unwrap();
        assert_eq!(map.spellings(Action::Copy), vec!["ctrl+shift+c"]);

        // Keeping the Command chord (alone or as an alias) is not suppressed.
        let keeps = BTreeMap::from([
            (
                "copy".to_string(),
                KeysValue::Many(vec!["ctrl+shift+c".into(), "super+c".into()]),
            ),
            ("paste".to_string(), KeysValue::One("super+v".into())),
        ]);
        assert!(KeyMap::macos_suppressed_actions(Some(&keeps)).is_empty());

        // Intentional unbinding with [] stays silent.
        let unbound = BTreeMap::from([
            ("copy".to_string(), KeysValue::Many(vec![])),
            ("new_tab".to_string(), KeysValue::Many(vec![])),
        ]);
        assert!(KeyMap::macos_suppressed_actions(Some(&unbound)).is_empty());

        // Actions without a macOS chord never report, even when overridden.
        let other = BTreeMap::from([(
            "split_right".to_string(),
            KeysValue::One("ctrl+alt+enter".into()),
        )]);
        assert!(KeyMap::macos_suppressed_actions(Some(&other)).is_empty());
    }

    #[test]
    fn conflicts_are_load_errors_that_name_both_sides() {
        let err = KeyMap::from_config(Some(&one("split_right", "ctrl+shift+w"))).unwrap_err();
        assert!(
            err.contains("split_right")
                && err.contains("close_pane")
                && err.contains("ctrl+shift+w"),
            "{err}"
        );
        let err = KeyMap::from_config(Some(&BTreeMap::from([
            ("find".to_string(), KeysValue::One("ctrl+alt+k".into())),
            ("copy".to_string(), KeysValue::One("ctrl+alt+k".into())),
        ])))
        .unwrap_err();
        assert!(err.contains("find") && err.contains("copy"), "{err}");
        // Moving a default onto another action is fine once the old owner moved.
        let map = KeyMap::from_config(Some(&BTreeMap::from([
            (
                "close_pane".to_string(),
                KeysValue::One("ctrl+shift+k".into()),
            ),
            (
                "split_right".to_string(),
                KeysValue::One("ctrl+shift+w".into()),
            ),
        ])))
        .unwrap();
        assert_eq!(
            map.spellings(Action::SplitRight),
            vec!["ctrl+shift+w".to_string()]
        );
    }

    #[test]
    fn unknown_actions_bad_chords_and_fixed_inputs_are_rejected() {
        let err = KeyMap::from_config(Some(&one("split_rite", "ctrl+shift+e"))).unwrap_err();
        assert!(
            err.contains("unknown action") && err.contains("split_rite"),
            "{err}"
        );
        let err = KeyMap::from_config(Some(&one("find", "ctrl+bogus"))).unwrap_err();
        assert!(
            err.contains("keys.find") && err.contains("unknown key"),
            "{err}"
        );
        let err = KeyMap::from_config(Some(&one("find", "shift+f"))).unwrap_err();
        assert!(err.contains("needs ctrl, alt, or super"), "{err}");
        for reserved in [
            "ctrl+c",
            "ctrl+2",
            "ctrl+space",
            "ctrl+shift+/",
            "ctrl+shift+?",
        ] {
            let err = KeyMap::from_config(Some(&one("copy", reserved))).unwrap_err();
            assert!(err.contains("reserved"), "{reserved}: {err}");
        }
    }
}

#[cfg(test)]
mod detail_contract_tests {
    use super::*;

    #[test]
    fn palette_notes_describe_messages_maintenance_and_local_pane_moves() {
        assert!(Action::AgentMessages.note().unwrap().contains("messages"));
        assert!(Action::UpdateRestart
            .note()
            .unwrap()
            .contains("restart components"));
        assert!(Action::MovePaneToSpace
            .note()
            .unwrap()
            .contains("Blank terminals"));
        for action in Action::all() {
            if let Some(note) = action.note() {
                assert!(!note.is_empty());
                assert!(!note.contains('\n'));
            }
        }
    }

    #[test]
    fn shifted_punctuation_matches_the_same_physical_shortcut_key() {
        for (shifted, base) in "~!@#$%^&*()_+{}|:\"<>?"
            .chars()
            .zip("`1234567890-=[]\\;',./".chars())
        {
            assert_eq!(unshift(shifted), base);
        }
        assert_eq!(unshift('A'), 'a');
        assert_eq!(unshift('é'), 'é');
    }
}
