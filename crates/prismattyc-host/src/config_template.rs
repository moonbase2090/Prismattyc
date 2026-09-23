//! Full commented `config.toml` template (PT-84).
//!
//! [`CONFIG_KEYS`] is the source of truth for names, docs, ranges, and
//! defaults. The generator, `--help`, and the field-coverage test read it.

use std::path::Path;

use anyhow::{Context, Result};
use prismattyc_mux::{merge_mux_section, render_mux_section, write_config_atomic};

use crate::config::{
    DEFAULT_BACKGROUND_BLUR_PX, DEFAULT_BACKGROUND_OPACITY, DEFAULT_BELL_TOASTER_MS,
    DEFAULT_PANE_GAP_PX, DEFAULT_PANE_OPACITY, DEFAULT_PANE_PADDING_PX, DEFAULT_SPACE_RAIL,
    DEFAULT_SPACE_RAIL_CHIP_COLS, DEFAULT_WINDOW_BLUR, DEFAULT_WINDOW_OPACITY,
    DEFAULT_WINDOW_PADDING_PX,
};
use crate::keybind::{self, Action};

/// Default cell size in px before display scaling (`FONT_PX` in main).
pub const DEFAULT_FONT_PX: f32 = 15.0;
const DEFAULT_THEME: &str = "prismattyc-default";
const DEFAULT_FOCUS_BORDER: &str = "blue";
const DEFAULT_FOCUS_ANIMATION: &str = "none";
const DEFAULT_FOCUS_ANIMATION_MS: u64 = 280;
const DEFAULT_PANES: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ConfigGroup {
    Appearance,
    FocusBorder,
    Font,
    Layout,
    BellsAttention,
    Background,
    Mux,
    Keys,
    A11y,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigValue {
    Bool(bool),
    Usize(usize),
    U32(u32),
    U64(u64),
    F32(f32),
    String(&'static str),
    StringArray(&'static [&'static str]),
    CommentedPath(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigKey {
    pub name: &'static str,
    pub group: ConfigGroup,
    pub doc: &'static str,
    pub range: &'static str,
    pub value: ConfigValue,
}

/// Every `ConfigFile` field except the skipped `resolved_theme`.
pub const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey {
        name: "theme",
        group: ConfigGroup::Appearance,
        doc:
            "Named theme: built-in slug, display name, sibling themes/ file, or absolute TOML path",
        range: "theme slug or absolute path",
        value: ConfigValue::String(DEFAULT_THEME),
    },
    ConfigKey {
        name: "render_timer",
        group: ConfigGroup::Appearance,
        doc: "Render timings and counters",
        range: "off|osd|log|both",
        value: ConfigValue::String("off"),
    },
    ConfigKey {
        name: "render_timer_log_every_frame",
        group: ConfigGroup::Appearance,
        doc: "Log every render frame when render_timer includes log; use for benches only",
        range: "true|false",
        value: ConfigValue::Bool(false),
    },
    ConfigKey {
        name: "splash",
        group: ConfigGroup::Appearance,
        doc: "Show the launch splash on bare launches",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "splash_animation",
        group: ConfigGroup::Appearance,
        doc: "Animate the launch splash word art",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "tab_strip",
        group: ConfigGroup::Appearance,
        doc: "Tab strip visibility: auto and always show one tab; multi needs two tabs",
        range: "auto|always|multi",
        value: ConfigValue::String("auto"),
    },
    ConfigKey {
        name: "pane_titles",
        group: ConfigGroup::Appearance,
        doc: "Multi-pane title row: focused pane OSC title, or handle hover only",
        range: "focused|hover",
        value: ConfigValue::String("focused"),
    },
    ConfigKey {
        name: "hover_blend",
        group: ConfigGroup::Appearance,
        doc: "Immediate hover blend for interactive strip, rail, and scrollbar chrome",
        range: "0.0-0.3",
        value: ConfigValue::F32(crate::config::DEFAULT_HOVER_BLEND),
    },
    ConfigKey {
        name: "focus_border",
        group: ConfigGroup::FocusBorder,
        doc: "Focus border color",
        range: "coral amber yellow green blue violet ink, or 0-6",
        value: ConfigValue::String(DEFAULT_FOCUS_BORDER),
    },
    ConfigKey {
        name: "focus_border_animation",
        group: ConfigGroup::FocusBorder,
        doc: "Focus-change animation",
        range: "\"none\" | \"light-cycle\"",
        value: ConfigValue::String(DEFAULT_FOCUS_ANIMATION),
    },
    ConfigKey {
        name: "focus_border_animation_ms",
        group: ConfigGroup::FocusBorder,
        doc: "Light-cycle sweep duration in ms",
        range: "50-5000",
        value: ConfigValue::U64(DEFAULT_FOCUS_ANIMATION_MS),
    },
    ConfigKey {
        name: "focus_border_animation_head",
        group: ConfigGroup::FocusBorder,
        doc: "Draw the bright vehicle box at the sweep head",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "font",
        group: ConfigGroup::Font,
        doc: "Primary font path; falls back to the built-in chain if unset or unreadable",
        range: "absolute path",
        value: ConfigValue::CommentedPath("/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf"),
    },
    ConfigKey {
        name: "font_fallback",
        group: ConfigGroup::Font,
        doc: "Extra fallback faces, tried after the built-in chain",
        range: "array of absolute paths",
        value: ConfigValue::CommentedPath("/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf"),
    },
    ConfigKey {
        name: "font_px",
        group: ConfigGroup::Font,
        doc: "Cell size in px before display scaling",
        range: "6-72",
        value: ConfigValue::F32(DEFAULT_FONT_PX),
    },
    ConfigKey {
        name: "font_ligatures",
        group: ConfigGroup::Font,
        doc: "Host-only OpenType ligatures for eligible terminal-grid text",
        range: "true|false",
        value: ConfigValue::Bool(false),
    },
    ConfigKey {
        name: "font_features",
        group: ConfigGroup::Font,
        doc: "OpenType feature tags; prefix with '-' to disable",
        range: "four ASCII alphanumeric characters or spaces",
        value: ConfigValue::StringArray(&["calt", "liga"]),
    },
    ConfigKey {
        name: "panes",
        group: ConfigGroup::Layout,
        doc: "Initial pane count; startup only",
        range: "1-8",
        value: ConfigValue::Usize(DEFAULT_PANES),
    },
    ConfigKey {
        name: "window_padding_px",
        group: ConfigGroup::Layout,
        doc: "Window edge to pane chrome, physical pixels",
        range: "0-128",
        value: ConfigValue::Usize(DEFAULT_WINDOW_PADDING_PX),
    },
    ConfigKey {
        name: "pane_gap_px",
        group: ConfigGroup::Layout,
        doc: "Space between pane chrome rectangles; used with 2+ panes",
        range: "0-128",
        value: ConfigValue::Usize(DEFAULT_PANE_GAP_PX),
    },
    ConfigKey {
        name: "pane_padding_px",
        group: ConfigGroup::Layout,
        doc: "Pane chrome to terminal cells and tab content, physical pixels",
        range: "0-128",
        value: ConfigValue::Usize(DEFAULT_PANE_PADDING_PX),
    },
    ConfigKey {
        name: "space_rail",
        group: ConfigGroup::Layout,
        doc: "Edge that shows the saved-spaces rail; off hides it",
        range: "\"bottom\" | \"left\" | \"top\" | \"right\" | \"off\"",
        value: ConfigValue::String(DEFAULT_SPACE_RAIL),
    },
    ConfigKey {
        name: "space_autosave",
        group: ConfigGroup::Layout,
        doc: "Save changed Space layouts after two idle seconds",
        range: "true | false",
        value: ConfigValue::Bool(false),
    },
    ConfigKey {
        name: "session_naming",
        group: ConfigGroup::Layout,
        doc: "Choose naming prompts, automatic sessions, or blank terminals",
        range: "\"ask\" | \"auto\" | \"blank\"",
        value: ConfigValue::String("ask"),
    },
    ConfigKey {
        name: "space_startup",
        group: ConfigGroup::Layout,
        doc: "Startup choice; restore reconnects live sessions without launching stopped ones",
        range: "\"ask\" | \"restore\" | \"fresh\"",
        value: ConfigValue::String("ask"),
    },
    ConfigKey {
        name: "space_rail_width_cols", group: ConfigGroup::Layout,
        doc: "Fixed width of left and right Space rails; drag the edge to resize",
        range: "8-60", value: ConfigValue::Usize(18),
    },
    ConfigKey {
        name: "restore_blank_terminals", group: ConfigGroup::Layout,
        doc: "Recreate blank terminal tabs, split layouts, and directories with fresh shells",
        range: "true | false", value: ConfigValue::Bool(false),
    },
    ConfigKey {
        name: "space_rail_chip_cols",
        group: ConfigGroup::Layout,
        doc: "Widest space chip in cells (chips fit their labels); 0 = 28",
        range: "0 or 6-40",
        value: ConfigValue::Usize(DEFAULT_SPACE_RAIL_CHIP_COLS),
    },
    ConfigKey {
        name: "visual_bell",
        group: ConfigGroup::BellsAttention,
        doc: "Flash the window on BEL (~120ms invert)",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "pane_visual_bell",
        group: ConfigGroup::BellsAttention,
        doc: "Use a 120ms pane perimeter instead of the window flash and visible BEL toast; requires visual_bell",
        range: "true|false",
        value: ConfigValue::Bool(false),
    },
    ConfigKey {
        name: "audible_bell",
        group: ConfigGroup::BellsAttention,
        doc: "Play the bundled Zen bell on BEL",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "bell_toaster",
        group: ConfigGroup::BellsAttention,
        doc: "Show a toast on the pane that rang BEL",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "bell_toaster_ms",
        group: ConfigGroup::BellsAttention,
        doc: "Bell toast linger in ms",
        range: "500-60000",
        value: ConfigValue::U64(DEFAULT_BELL_TOASTER_MS),
    },
    ConfigKey {
        name: "drag_toaster",
        group: ConfigGroup::BellsAttention,
        doc: "Show 'Moving tab NAME → target' while a tab or pane is dragged",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "os_notify_bell",
        group: ConfigGroup::BellsAttention,
        doc: "OS notification on BEL while the window is unfocused",
        range: "true|false",
        value: ConfigValue::Bool(false),
    },
    ConfigKey {
        name: "attention_sound",
        group: ConfigGroup::BellsAttention,
        doc: "Play the attention cue on OSC 9 / 777 / 99",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "attention_badge",
        group: ConfigGroup::BellsAttention,
        doc: "Draw a coral attention badge on the tab",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "os_notify_attention",
        group: ConfigGroup::BellsAttention,
        doc: "OS notification when the attention pane is not selected or the window is unfocused",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "walkthrough_audio",
        group: ConfigGroup::BellsAttention,
        doc: "Play bundled walkthrough narration; missing clips stay silent",
        range: "true|false",
        value: ConfigValue::Bool(true),
    },
    ConfigKey {
        name: "walkthrough_voice",
        group: ConfigGroup::BellsAttention,
        doc: "ElevenLabs voice name for scripts/walkthrough-voice.sh (generation only)",
        range: "voice name",
        value: ConfigValue::String(crate::config::DEFAULT_WALKTHROUGH_VOICE),
    },
    ConfigKey {
        name: "background_image",
        group: ConfigGroup::Background,
        doc: "Window background PNG; absolute path, PNG only",
        range: "absolute path",
        value: ConfigValue::CommentedPath("/usr/share/backgrounds/example.png"),
    },
    ConfigKey {
        name: "background_opacity",
        group: ConfigGroup::Background,
        doc: "How much of the image shows (0 = flat theme bg, 1 = image)",
        range: "0.0-1.0",
        value: ConfigValue::F32(DEFAULT_BACKGROUND_OPACITY),
    },
    ConfigKey {
        name: "background_blur_px",
        group: ConfigGroup::Background,
        doc: "Box-blur radius in pixels",
        range: "0-64",
        value: ConfigValue::U32(DEFAULT_BACKGROUND_BLUR_PX),
    },
    ConfigKey {
        name: "pane_opacity_active",
        group: ConfigGroup::Background,
        doc: "Focused/zoomed pane surface opacity; try 0.6-0.8",
        range: "0.0-1.0",
        value: ConfigValue::F32(DEFAULT_PANE_OPACITY),
    },
    ConfigKey {
        name: "overlay_opacity",
        group: ConfigGroup::Background,
        doc: "Host overlay surface opacity; try 0.6-0.8 (remove to follow pane_opacity_active)",
        range: "0.0-1.0",
        value: ConfigValue::F32(DEFAULT_PANE_OPACITY),
    },
    ConfigKey {
        name: "pane_opacity_inactive",
        group: ConfigGroup::Background,
        doc: "Every other pane surface opacity; try 0.6-0.8",
        range: "0.0-1.0",
        value: ConfigValue::F32(DEFAULT_PANE_OPACITY),
    },
    ConfigKey {
        name: "window_opacity",
        group: ConfigGroup::Background,
        doc: "Window ground opacity; text and explicit cell backgrounds stay opaque. Hot reload works on macOS; other platforms may need a restart below 1.0",
        range: "0.0-1.0",
        value: ConfigValue::F32(DEFAULT_WINDOW_OPACITY),
    },
    ConfigKey {
        name: "chrome_opacity",
        group: ConfigGroup::Background,
        doc: "Tab strip, Space rail, and footer opacity; defaults to window_opacity",
        range: "0.0-1.0",
        value: ConfigValue::F32(DEFAULT_WINDOW_OPACITY),
    },
    ConfigKey {
        name: "window_blur",
        group: ConfigGroup::Background,
        doc: "Blur behind translucent window grounds; use window_opacity below 1.0. Hot reload works on macOS; no-op where unavailable",
        range: "true|false",
        value: ConfigValue::Bool(DEFAULT_WINDOW_BLUR),
    },
];

fn group_header(group: ConfigGroup) -> &'static str {
    match group {
        ConfigGroup::Appearance => "appearance / theme",
        ConfigGroup::FocusBorder => "focus border",
        ConfigGroup::Font => "font",
        ConfigGroup::Layout => "layout",
        ConfigGroup::BellsAttention => "bells and attention",
        ConfigGroup::Background => "background",
        ConfigGroup::Mux => "mux",
        ConfigGroup::Keys => "keys",
        ConfigGroup::A11y => "accessibility",
    }
}

fn format_value(value: ConfigValue) -> String {
    match value {
        ConfigValue::Bool(v) => v.to_string(),
        ConfigValue::Usize(v) => v.to_string(),
        ConfigValue::U32(v) => v.to_string(),
        ConfigValue::U64(v) => v.to_string(),
        ConfigValue::F32(v) => {
            if (v - v.round()).abs() < f32::EPSILON {
                format!("{v:.1}")
            } else {
                format!("{v}")
            }
        }
        ConfigValue::String(v) => format!("\"{v}\""),
        ConfigValue::StringArray(items) => {
            let inner = items
                .iter()
                .map(|item| format!("\"{item}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        }
        ConfigValue::CommentedPath(path) => format!("\"{path}\""),
    }
}

fn emit_key(out: &mut String, key: &ConfigKey) {
    out.push_str(&format!("# {}. {}.\n", key.doc, key.range));
    let rendered = format_value(key.value);
    match key.value {
        ConfigValue::CommentedPath(_) if key.name == "font_fallback" => {
            out.push_str(&format!("# {} = [{rendered}]\n", key.name));
        }
        ConfigValue::CommentedPath(_) => {
            out.push_str(&format!("# {} = {rendered}\n", key.name));
        }
        _ => out.push_str(&format!("{} = {rendered}\n", key.name)),
    }
}

fn emit_keys_table(out: &mut String) {
    out.push_str("[keys]\n");
    out.push_str("# Host actions. An entry replaces that action's default chords.\n");
    out.push_str("# Value is a chord string or an array of chord strings.\n");
    let keymap = keybind::KeyMap::default();
    for action in Action::all() {
        let name = action.name();
        let chords = keybind::default_chords(action);
        out.push_str(&format!("# {}.\n", action.describe()));
        if chords.is_empty() {
            assert!(keymap.spellings(action).is_empty());
            // Unbound by default: shown commented so the name is discoverable
            // without an explicit `[]` line (PT-207).
            out.push_str(&format!("# {name} = []\n"));
            continue;
        }
        if chords.len() == 1 {
            let escaped = toml_edit::value(chords[0]).to_string();
            out.push_str(&format!("{name} = {escaped}\n"));
        } else {
            let items = chords
                .iter()
                .map(|chord| toml_edit::value(*chord).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("{name} = [{items}]\n"));
        }
    }
}

/// Full template: every key at its default, grouped, with comments.
pub fn render_template() -> String {
    let mut out = String::from("# Prismattyc host config. Every key is at its default.\n");
    out.push_str("# Edit in place. CLI flags and PRISMATTYC_* env vars still win.\n\n");
    let mut current: Option<ConfigGroup> = None;
    for key in CONFIG_KEYS {
        if current != Some(key.group) {
            if current.is_some() {
                out.push('\n');
            }
            out.push_str(&format!("# -- {} --\n", group_header(key.group)));
            current = Some(key.group);
        }
        emit_key(&mut out, key);
    }
    out.push('\n');
    out.push_str(&render_theme_overrides_section());
    out.push('\n');
    out.push_str(&render_mux_section());
    out.push('\n');
    emit_keys_table(&mut out);
    out.push('\n');
    out.push_str(&render_a11y_section());
    out
}

fn render_a11y_section() -> String {
    let mut out = String::from("# -- accessibility --\n");
    out.push_str("[a11y]\n");
    out.push_str("# Expose host chrome through AccessKit (VoiceOver / Orca). true|false.\n");
    out.push_str("os_tree = true\n");
    out.push_str("# Speak mail, attention, pane-title notices, and cursor-line changes (PT-175). true|false.\n");
    out.push_str("announce = true\n");
    out
}

/// One `[theme_overrides]` key: name, doc, and the prismattyc-default value
/// rendered as a TOML literal (PT-207).
pub struct ThemeOverrideKey {
    pub name: &'static str,
    pub doc: &'static str,
    pub literal: String,
}

const THEME_OVERRIDES_HEADER: &str = "# -- theme overrides --\n\
# Recolour the named theme without copying a theme file. Uncomment a key\n\
# to override it; values are #RRGGBB. Shown at the prismattyc-default\n\
# values. Hot-reloaded with the rest of this file; the theme picker keeps them.\n";

/// Every `[theme_overrides]` key with the prismattyc-default value.
pub fn theme_override_keys() -> Vec<ThemeOverrideKey> {
    use crate::theme::{default_theme, hex};
    let theme = default_theme();
    let quoted = |rgb: [u8; 3]| toml_edit::Value::from(hex(rgb)).to_string();
    let pair = |value: Option<[u8; 3]>, fallback: [u8; 3]| quoted(value.unwrap_or(fallback));
    let mut ansi = toml_edit::Array::new();
    for rgb in theme.ansi {
        ansi.push(hex(rgb));
    }
    let key = |name: &'static str, doc: &'static str, literal: String| ThemeOverrideKey {
        name,
        doc,
        literal,
    };
    vec![
        key("default_fg", "Terminal text", quoted(theme.default_fg)),
        key("default_bg", "Terminal ground", quoted(theme.default_bg)),
        key(
            "chrome_fg",
            "Chrome text: tabs, rails, footer",
            quoted(theme.chrome_fg),
        ),
        key("chrome_bg", "Chrome ground", quoted(theme.chrome_bg)),
        key(
            "tab_active_bg",
            "Active tab chip fill; follows the focus colour when unset",
            quoted(theme.tab_active_bg),
        ),
        key(
            "pane_backdrop",
            "Frame ground behind panes; pane opacity blends toward it",
            quoted(theme.pane_backdrop),
        ),
        key(
            "pane_border",
            "Unfocused pane border",
            quoted(theme.pane_border),
        ),
        key(
            "overlay_bg",
            "Toast and overlay ground",
            quoted(theme.overlay_bg),
        ),
        key(
            "unseen_badge",
            "Unseen-output tab badge",
            quoted(theme.unseen_badge),
        ),
        key(
            "mail_letter",
            "Mail envelope ink",
            quoted(theme.mail_letter),
        ),
        key(
            "active_badge",
            "Working tab badge and breathing handle chip",
            quoted(theme.active_badge),
        ),
        key(
            "attention_badge",
            "Agent attention tab badge",
            quoted(theme.attention_badge),
        ),
        key(
            "cursor_fg",
            "Cursor text; set together with cursor_bg",
            pair(theme.cursor_fg, theme.default_bg),
        ),
        key(
            "cursor_bg",
            "Cursor block; set together with cursor_fg",
            pair(theme.cursor_bg, theme.default_fg),
        ),
        key(
            "selection_fg",
            "Selection text; set together with selection_bg (unset: inverse video)",
            pair(theme.selection_fg, theme.default_bg),
        ),
        key(
            "selection_bg",
            "Selection ground; set together with selection_fg (unset: inverse video)",
            pair(theme.selection_bg, theme.default_fg),
        ),
        key(
            "ansi",
            "ANSI colours 0-15",
            toml_edit::Value::Array(ansi).to_string(),
        ),
    ]
}

fn render_theme_overrides_section() -> String {
    let mut out = String::from(THEME_OVERRIDES_HEADER);
    out.push_str("[theme_overrides]\n");
    for key in theme_override_keys() {
        out.push_str(&format!("# {}. #RRGGBB.\n", key.doc));
        out.push_str(&format!("# {} = {}\n", key.name, key.literal));
    }
    out
}

fn merge_theme_overrides_section(document: &mut toml_edit::DocumentMut) {
    // Commented keys re-parse as comments on the next item, not as table
    // entries, so look at the whole document text before inserting.
    let text = document.to_string();
    if document
        .get("theme_overrides")
        .and_then(|item| item.as_table())
        .is_none()
    {
        let mut table = toml_edit::Table::new();
        table.set_implicit(false);
        table
            .decor_mut()
            .set_prefix(format!("\n{THEME_OVERRIDES_HEADER}"));
        document["theme_overrides"] = toml_edit::Item::Table(table);
    }
    let Some(table) = document["theme_overrides"].as_table_mut() else {
        return;
    };
    for key in theme_override_keys() {
        if table.contains_key(key.name) || document_has_key_or_comment_text(&text, key.name) {
            continue;
        }
        let value = match key.literal.parse::<toml_edit::Value>() {
            Ok(value) => value,
            Err(_) => continue,
        };
        table.insert(key.name, toml_edit::Item::Value(value));
        if let Some(mut entry) = table.key_mut(key.name) {
            entry
                .leaf_decor_mut()
                .set_prefix(format!("# {}. #RRGGBB.\n# ", key.doc));
        }
    }
}

fn insert_toml_value(document: &mut toml_edit::DocumentMut, key: &ConfigKey) {
    if document.contains_key(key.name) {
        return;
    }
    match key.value {
        ConfigValue::Bool(v) => document[key.name] = toml_edit::value(v),
        ConfigValue::Usize(v) => {
            document[key.name] = toml_edit::value(i64::try_from(v).unwrap_or(i64::MAX));
        }
        ConfigValue::U32(v) => document[key.name] = toml_edit::value(i64::from(v)),
        ConfigValue::U64(v) => {
            document[key.name] = toml_edit::value(i64::try_from(v).unwrap_or(i64::MAX));
        }
        ConfigValue::F32(v) => document[key.name] = toml_edit::value(f64::from(v)),
        ConfigValue::String(v) => document[key.name] = toml_edit::value(v),
        ConfigValue::StringArray(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(*item);
            }
            document[key.name] = toml_edit::Item::Value(toml_edit::Value::Array(array));
        }
        ConfigValue::CommentedPath(path) => {
            insert_commented_root_key(document, key.name, path, key.name == "font_fallback");
        }
    }
}

fn document_has_key_or_comment(document: &toml_edit::DocumentMut, name: &str) -> bool {
    document.contains_key(name) || document_has_key_or_comment_text(&document.to_string(), name)
}

/// `name = ...` or `# name = ...` at the start of any line of `text`.
fn document_has_key_or_comment_text(text: &str, name: &str) -> bool {
    let needle = format!("# {name} =");
    let live = format!("{name} =");
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with(&needle) || trimmed.starts_with(&live)
    })
}

fn insert_commented_root_key(
    document: &mut toml_edit::DocumentMut,
    name: &str,
    path: &str,
    as_array: bool,
) {
    if document_has_key_or_comment(document, name) {
        return;
    }
    if as_array {
        let mut array = toml_edit::Array::new();
        array.push(path);
        document[name] = toml_edit::Item::Value(toml_edit::Value::Array(array));
    } else {
        document[name] = toml_edit::value(path);
    }
    if let Some(mut key) = document.key_mut(name) {
        key.leaf_decor_mut().set_prefix("# ");
    }
}

fn merge_keys_table(document: &mut toml_edit::DocumentMut) {
    let text = document.to_string();
    if document
        .get("keys")
        .and_then(|item| item.as_table())
        .is_none()
    {
        let mut table = toml_edit::Table::new();
        table.set_implicit(false);
        table.decor_mut().set_prefix("\n");
        document["keys"] = toml_edit::Item::Table(table);
    }
    let Some(table) = document["keys"].as_table_mut() else {
        return;
    };
    for action in Action::all() {
        let name = action.name();
        if table.contains_key(&name) {
            continue;
        }
        let chords = keybind::default_chords(action);
        if chords.is_empty() {
            if !document_has_key_or_comment_text(&text, &name) {
                table.insert(
                    &name,
                    toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())),
                );
                if let Some(mut key) = table.key_mut(&name) {
                    key.leaf_decor_mut().set_prefix("# ");
                }
            }
            continue;
        }
        if chords.len() == 1 {
            table.insert(&name, toml_edit::value(chords[0]));
            continue;
        }
        let mut array = toml_edit::Array::new();
        for chord in chords {
            array.push(chord);
        }
        table.insert(
            &name,
            toml_edit::Item::Value(toml_edit::Value::Array(array)),
        );
    }
}

/// Keep user values and comments; append missing keys.
pub fn merge_template(existing: &str) -> Result<String> {
    let mut document = existing
        .parse::<toml_edit::DocumentMut>()
        .context("parse existing config for merge")?;
    for key in CONFIG_KEYS {
        insert_toml_value(&mut document, key);
    }
    merge_theme_overrides_section(&mut document);
    merge_mux_section(&mut document);
    merge_keys_table(&mut document);
    merge_a11y_section(&mut document);
    Ok(document.to_string())
}

fn merge_a11y_section(document: &mut toml_edit::DocumentMut) {
    if document
        .get("a11y")
        .and_then(|item| item.as_table())
        .is_none()
    {
        let mut table = toml_edit::Table::new();
        table.set_implicit(false);
        table.decor_mut().set_prefix("\n");
        document["a11y"] = toml_edit::Item::Table(table);
    }
    let Some(table) = document["a11y"].as_table_mut() else {
        return;
    };
    if !table.contains_key("os_tree") {
        table.insert("os_tree", toml_edit::value(true));
    }
    if !table.contains_key("announce") {
        table.insert("announce", toml_edit::value(true));
    }
}

/// Write the template, or merge into an existing file. `None` prints to stdout.
pub fn write_config(path: Option<&Path>, merge: bool) -> Result<()> {
    match path {
        None => {
            print!("{}", render_template());
            Ok(())
        }
        Some(path) if path.as_os_str() == "-" => {
            print!("{}", render_template());
            Ok(())
        }
        Some(path) if merge => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read {} for --merge", path.display()))?;
            let merged = merge_template(&raw)?;
            write_config_atomic(path, &merged)
        }
        Some(path) => write_config_atomic(path, &render_template()),
    }
}

/// First run: write the template when the path does not exist.
pub fn ensure_template(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    write_config_atomic(path, &render_template())?;
    Ok(true)
}

/// Range text used by `--help`.
pub fn help_lines() -> Vec<String> {
    let mut lines = CONFIG_KEYS
        .iter()
        .map(|key| format!("    {:<28} {} ({})", key.name, key.doc, key.range))
        .collect::<Vec<_>>();
    lines.push("    [theme_overrides]".to_string());
    for key in theme_override_keys() {
        lines.push(format!(
            "    theme_overrides.{:<12} {} (#RRGGBB)",
            key.name, key.doc
        ));
    }
    lines.push("    [mux]".to_string());
    for key in prismattyc_mux::MUX_KEYS {
        lines.push(format!(
            "    mux.{:<23} {} ({})",
            key.name, key.doc, key.range
        ));
    }
    lines.push("    [a11y]".to_string());
    lines.push(
        "    a11y.os_tree                 Expose host chrome through AccessKit (VoiceOver / Orca) (true|false)"
            .to_string(),
    );
    lines.push(
        "    a11y.announce                Speak mail, attention, pane-title notices, and cursor-line changes (true|false)"
            .to_string(),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        load, ConfigFile, BACKGROUND_BLUR_PX_RANGE, BACKGROUND_OPACITY_RANGE,
        BELL_TOASTER_MS_RANGE, FONT_PX_RANGE, SPACING_PX_RANGE,
    };
    use crate::keybind::KeyMap;
    use crate::MAX_INITIAL_PANES;
    use prismattyc_mux::MUX_KEYS;

    const CONFIG_FILE_FIELDS: &[&str] = &[
        "theme",
        "render_timer",
        "render_timer_log_every_frame",
        "tab_strip",
        "pane_titles",
        "focus_border",
        "focus_border_animation",
        "focus_border_animation_ms",
        "focus_border_animation_head",
        "splash",
        "splash_animation",
        "font",
        "font_fallback",
        "font_px",
        "font_ligatures",
        "font_features",
        "panes",
        "window_padding_px",
        "pane_gap_px",
        "pane_padding_px",
        "space_rail",
        "space_autosave",
        "session_naming",
        "space_startup",
        "space_rail_width_cols",
        "restore_blank_terminals",
        "space_rail_chip_cols",
        "visual_bell",
        "pane_visual_bell",
        "audible_bell",
        "bell_toaster",
        "bell_toaster_ms",
        "drag_toaster",
        "os_notify_bell",
        "attention_sound",
        "attention_badge",
        "os_notify_attention",
        "walkthrough_audio",
        "walkthrough_voice",
        "background_image",
        "background_opacity",
        "background_blur_px",
        "pane_opacity_active",
        "overlay_opacity",
        "pane_opacity_inactive",
        "window_opacity",
        "chrome_opacity",
        "window_blur",
        "hover_blend",
        "mux",
        "keys",
        "a11y",
        "theme_overrides",
    ];

    #[test]
    fn table_and_template_cover_every_config_file_field() {
        let template = render_template();
        for field in CONFIG_FILE_FIELDS {
            let present = template.contains(&format!("{field} ="))
                || template.contains(&format!("# {field} ="))
                || template.contains(&format!("[{field}]"));
            assert!(present, "template missing {field}");
            if !matches!(*field, "mux" | "keys" | "a11y" | "theme_overrides") {
                assert!(
                    CONFIG_KEYS.iter().any(|key| key.name == *field),
                    "CONFIG_KEYS missing {field}"
                );
            }
        }
        for key in CONFIG_KEYS {
            assert!(
                CONFIG_FILE_FIELDS.contains(&key.name),
                "CONFIG_KEYS extra {}",
                key.name
            );
        }
        for key in MUX_KEYS {
            assert!(
                template.contains(&format!("{} =", key.name))
                    || template.contains(&format!("# {} =", key.name)),
                "mux template missing {}",
                key.name
            );
        }
        let _ = FONT_PX_RANGE;
        let _ = SPACING_PX_RANGE;
        let _ = BACKGROUND_OPACITY_RANGE;
        let _ = BACKGROUND_BLUR_PX_RANGE;
        let _ = BELL_TOASTER_MS_RANGE;
        let _ = MAX_INITIAL_PANES;
    }

    #[test]
    fn generated_template_parses_to_builtin_defaults() {
        let template = render_template();
        let parsed = load_from_str(&template);
        assert_eq!(parsed.theme.as_deref(), Some(DEFAULT_THEME));
        assert_eq!(parsed.render_timer(), crate::config::RenderTimer::Off);
        assert!(!parsed.render_timer_log_every_frame());
        assert_eq!(parsed.tab_strip(), crate::config::TabStripMode::Auto);
        assert_eq!(parsed.pane_titles(), crate::config::PaneTitlesMode::Focused);
        assert!(parsed.splash());
        assert_eq!(parsed.splash, Some(true));
        assert_eq!(parsed.focus_border.as_deref(), Some(DEFAULT_FOCUS_BORDER));
        assert_eq!(parsed.font_px, Some(DEFAULT_FONT_PX));
        assert_eq!(parsed.panes, Some(DEFAULT_PANES));
        assert_eq!(parsed.window_padding_px(), DEFAULT_WINDOW_PADDING_PX);
        assert!(parsed.visual_bell());
        assert!(!parsed.os_notify_bell());
        assert_eq!(parsed.background_opacity(), DEFAULT_BACKGROUND_OPACITY);
        assert_eq!(parsed.pane_opacity_active(), DEFAULT_PANE_OPACITY);
        assert_eq!(parsed.overlay_opacity(), DEFAULT_PANE_OPACITY);
        assert_eq!(parsed.window_opacity(), DEFAULT_WINDOW_OPACITY);
        assert_eq!(parsed.chrome_opacity(), DEFAULT_WINDOW_OPACITY);
        assert!(!parsed.window_blur());
        assert!(parsed.font.is_none());
        assert!(parsed.background_image.is_none());
        assert_eq!(parsed.loaded_keymap(), KeyMap::default());
        for action in Action::all() {
            let listed = parsed
                .keys
                .as_ref()
                .is_some_and(|keys| keys.contains_key(&action.name()));
            let bound = !keybind::default_chords(action).is_empty();
            assert_eq!(
                listed,
                bound,
                "{}: bound actions are live keys; unbound ones stay commented",
                action.name()
            );
            if !bound {
                assert!(
                    template.contains(&format!("# {} = []", action.name())),
                    "template must show unbound {} commented",
                    action.name()
                );
            }
        }
        assert_eq!(
            parsed.theme_overrides,
            Some(crate::theme::ThemeOverrides::default()),
            "the commented [theme_overrides] table parses empty"
        );
        assert_eq!(parsed.loaded_theme(), *crate::theme::default_theme());
        for key in theme_override_keys() {
            assert!(
                template.contains(&format!("# {} = {}", key.name, key.literal)),
                "template missing commented theme override {}",
                key.name
            );
        }
    }

    /// PT-207: uncommenting an override recolours the named theme; the
    /// example values are exactly the prismattyc-default palette.
    #[test]
    fn uncommented_theme_overrides_recolour_the_named_theme() {
        let template = render_template();
        let live = template.replace("# attention_badge = \"", "attention_badge = \"");
        assert_ne!(live, template);
        let parsed = load_from_str(&live);
        assert_eq!(parsed.loaded_theme(), *crate::theme::default_theme());

        let all_live = theme_override_keys()
            .iter()
            .fold(template.clone(), |acc, key| {
                acc.replace(&format!("# {} = ", key.name), &format!("{} = ", key.name))
            });
        let parsed = load_from_str(&all_live);
        let mut expected = crate::theme::default_theme().clone();
        // Explicit keys: the chip stops following the focus colour and the
        // selection example (inverse video spelled out) becomes explicit.
        expected.tab_active_bg_explicit = true;
        expected.selection_fg = Some(expected.default_bg);
        expected.selection_bg = Some(expected.default_fg);
        assert_eq!(
            parsed.loaded_theme(),
            expected,
            "every example value equals the prismattyc-default palette"
        );

        let recoloured = live.replace(
            &format!(
                "attention_badge = {}",
                toml_edit::Value::from(crate::theme::hex(
                    crate::theme::default_theme().attention_badge
                ))
            ),
            "attention_badge = \"#123456\"",
        );
        let parsed = load_from_str(&recoloured);
        assert_eq!(parsed.loaded_theme().attention_badge, [0x12, 0x34, 0x56]);
        assert_eq!(
            parsed.loaded_theme().mail_letter,
            crate::theme::default_theme().mail_letter,
            "other keys keep the named theme"
        );
    }

    #[test]
    fn generated_template_has_sibling_nested_tables() {
        let template = render_template();
        let document = template
            .parse::<toml_edit::DocumentMut>()
            .expect("generated template must be valid TOML");
        for name in ["theme_overrides", "mux", "keys", "a11y"] {
            assert!(
                document
                    .get(name)
                    .and_then(|item| item.as_table())
                    .is_some(),
                "template missing [{name}] table"
            );
        }
        let overrides = template
            .find("\n[theme_overrides]\n")
            .expect("[theme_overrides] section");
        let mux = template.find("\n[mux]\n").expect("[mux] section");
        let keys = template.find("\n[keys]\n").expect("[keys] section");
        let a11y = template.find("\n[a11y]\n").expect("[a11y] section");
        assert!(
            overrides < mux && mux < keys && keys < a11y,
            "nested tables must be ordered"
        );
        assert!(document["mux"]
            .as_table()
            .unwrap()
            .get("instance")
            .is_some());
        assert!(document["keys"]
            .as_table()
            .unwrap()
            .get("split_right")
            .is_some());
        let a11y = document["a11y"].as_table().unwrap();
        assert_eq!(
            a11y.get("os_tree").and_then(|item| item.as_bool()),
            Some(true)
        );
        assert_eq!(
            a11y.get("announce").and_then(|item| item.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn help_lines_include_nested_config_tables() {
        let lines = help_lines();
        for expected in [
            "[theme_overrides]",
            "theme_overrides.default_fg",
            "theme_overrides.ansi",
            "[mux]",
            "mux.instance",
            "[a11y]",
            "a11y.os_tree",
        ] {
            assert!(
                lines.iter().any(|line| line.contains(expected)),
                "help lines missing {expected}: {lines:?}"
            );
        }
    }

    #[test]
    fn ensure_template_writes_missing_file_once() {
        let dir = std::env::temp_dir().join(format!(
            "pt-179-ensure-template-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert!(ensure_template(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), render_template());
        assert!(!ensure_template(&path).unwrap());
        let malformed = "theme = [";
        std::fs::write(&path, malformed).unwrap();
        assert!(!ensure_template(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn merge_keeps_user_values_and_comments() {
        let existing = "# keep me\ntheme = \"dracula\"  # user theme\npanes = 3\n\n[mux]\ninstance = \"user\"\n\n[keys]\nsplit_right = \"ctrl+alt+enter\"\n\n[a11y]\nannounce = false\n";
        let merged = merge_template(existing).unwrap();
        assert!(merged.contains("# keep me"));
        assert!(merged.contains("theme = \"dracula\"  # user theme"));
        assert!(merged.contains("panes = 3"));
        assert!(merged.contains("font_px"));
        assert!(merged.contains("[mux]"));
        assert!(merged.contains("[keys]"));
        assert!(merged.contains("[a11y]"));
        assert!(merged.contains("instance = \"user\""));
        assert!(merged.contains("split_right = \"ctrl+alt+enter\""));
        assert!(merged.contains("split_down ="));
        assert!(merged.contains("announce = false"));
        assert!(merged.contains("os_tree = true"));
        assert!(
            merged.contains("# font ="),
            "merge must append commented font: {merged}"
        );
        assert!(
            merged.contains("# font_fallback ="),
            "merge must append commented font_fallback: {merged}"
        );
        assert!(
            merged.contains("# background_image ="),
            "merge must append commented background_image: {merged}"
        );
        assert!(
            merged.contains("# socket ="),
            "merge must append commented mux socket: {merged}"
        );
        assert!(
            merged.contains("[theme_overrides]") && merged.contains("# default_fg ="),
            "merge must append the commented theme overrides table: {merged}"
        );
        assert!(
            merged.contains("# swap_pane_prev = []") && !merged.contains("\nswap_pane_prev = []"),
            "merge must append unbound actions commented: {merged}"
        );
        let again = merge_template(&merged).unwrap();
        assert_eq!(
            again.matches("# font =").count(),
            merged.matches("# font =").count(),
            "second merge must not duplicate commented font"
        );
        assert_eq!(
            again.matches("# default_fg =").count(),
            1,
            "second merge must not duplicate commented theme overrides"
        );
        assert_eq!(
            again.matches("# swap_pane_prev =").count(),
            1,
            "second merge must not duplicate commented unbound actions"
        );
        let parsed = load_from_str(&merged);
        assert_eq!(parsed.theme.as_deref(), Some("dracula"));
        assert_eq!(parsed.panes, Some(3));
        assert_eq!(parsed.font_px, Some(DEFAULT_FONT_PX));
        assert!(parsed.font.is_none());
        assert!(parsed.background_image.is_none());
    }

    #[test]
    fn docs_config_example_matches_generator() {
        let docs = include_str!("../../../docs/config.md");
        let start = docs.find("```toml\n").expect("docs/config.md toml fence");
        let body = &docs[start + "```toml\n".len()..];
        let end = body.find("```").expect("closing fence");
        let example = &body[..end];
        assert_eq!(example, render_template());
    }

    fn load_from_str(raw: &str) -> ConfigFile {
        let dir = std::env::temp_dir().join(format!(
            "pt-84-template-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, raw).unwrap();
        let parsed = load(&path).expect("template must parse");
        let _ = std::fs::remove_dir_all(&dir);
        parsed
    }
}
