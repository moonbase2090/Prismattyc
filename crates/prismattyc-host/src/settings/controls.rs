// SPDX-License-Identifier: MIT
//! Deliberate settings placement. Coverage comes independently from serde.

use crate::config_schema;
use crate::config_template::{ConfigValue, CONFIG_KEYS};
use crate::keybind::Action;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Section {
    Appearance,
    Fonts,
    Layout,
    Spaces,
    Input,
    Notifications,
    Startup,
    Accessibility,
    Colors,
    Remote,
    Mux,
    Keybindings,
    Advanced,
}
impl Section {
    pub const ALL: &[Self] = &[
        Self::Appearance,
        Self::Fonts,
        Self::Layout,
        Self::Spaces,
        Self::Input,
        Self::Notifications,
        Self::Startup,
        Self::Accessibility,
        Self::Colors,
        Self::Remote,
        Self::Mux,
        Self::Keybindings,
        Self::Advanced,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Fonts => "Fonts",
            Self::Layout => "Layout",
            Self::Spaces => "Spaces",
            Self::Input => "Input",
            Self::Notifications => "Notifications",
            Self::Startup => "Startup",
            Self::Accessibility => "Accessibility",
            Self::Colors => "Theme colors",
            Self::Remote => "Remote destinations",
            Self::Mux => "Multiplexer",
            Self::Keybindings => "Keybindings",
            Self::Advanced => "Advanced",
        }
    }
}
#[derive(Clone, Debug)]
pub(super) enum Kind {
    Bool,
    Integer { min: i64, max: i64, step: i64 },
    Float { min: f64, max: f64, step: f64 },
    ChipWidth,
    Choice(Vec<String>),
    BorderColor,
    Text,
    Font,
    Theme,
    Strings,
    Color,
    Pair,
    Ansi,
    Remote,
    Binding(Action),
}
impl Kind {
    pub(super) fn cycles_on_enter(&self) -> bool {
        matches!(self, Self::Bool | Self::Choice(_) | Self::BorderColor)
    }
}
#[derive(Clone, Debug)]
pub(super) struct Control {
    pub section: Section,
    pub paths: Vec<Vec<String>>,
    pub label: String,
    pub kind: Kind,
    pub help: String,
    pub timing: &'static str,
    pub default: Option<toml::Value>,
}
impl Control {
    pub fn key(&self) -> String {
        self.paths[0].join(".")
    }
}
fn integer(min: i64, max: i64, step: i64) -> Kind {
    Kind::Integer { min, max, step }
}
fn float(min: f64, max: f64, step: f64) -> Kind {
    Kind::Float { min, max, step }
}
fn choice(key: &str) -> Kind {
    let mut node = config_schema::schema();
    for segment in key.split('.') {
        node = &config_schema::properties(node).expect("schema object")[segment];
    }
    fn collect(node: &Value, out: &mut Vec<String>) {
        let node = config_schema::resolve(node);
        if let Some(values) = node.get("enum").and_then(Value::as_array) {
            out.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(value) = node.get("const").and_then(Value::as_str) {
            out.push(value.to_string());
        }
        for union in ["anyOf", "oneOf", "allOf"] {
            if let Some(values) = node.get(union).and_then(Value::as_array) {
                for value in values {
                    collect(value, out);
                }
            }
        }
    }
    let mut values = Vec::new();
    collect(node, &mut values);
    assert!(!values.is_empty(), "finite serde choice for {key}");
    Kind::Choice(values)
}
fn row(section: Section, key: &str, kind: Kind) -> Control {
    let meta = CONFIG_KEYS.iter().find(|meta| meta.name == key);
    let default = meta.and_then(|meta| match meta.value {
        ConfigValue::Bool(v) | ConfigValue::CommentedBool(v) => Some(v.into()),
        ConfigValue::Usize(v) => Some((v as i64).into()),
        ConfigValue::U32(v) => Some(i64::from(v).into()),
        ConfigValue::U64(v) => Some((v as i64).into()),
        ConfigValue::F32(v) => Some(f64::from(v).into()),
        ConfigValue::String(v) => Some(v.into()),
        ConfigValue::StringArray(v) => {
            Some(toml::Value::Array(v.iter().map(|v| (*v).into()).collect()))
        }
        ConfigValue::CommentedString(_) | ConfigValue::CommentedPath(_) => None,
    });
    let title = key.rsplit('.').next().unwrap_or(key).replace('_', " ");
    let mut chars = title.chars();
    let label = chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default();
    Control {
        section,
        paths: vec![key.split('.').map(str::to_owned).collect()],
        label,
        kind,
        help: meta.map_or_else(String::new, |meta| meta.doc.to_string()),
        timing: timing(key),
        default,
    }
}
fn timing(key: &str) -> &'static str {
    match key {
        "macos_present" | "async_file_writes" | "snapshot_client" | "a11y.os_tree" => {
            "Next host launch"
        }
        "panes" | "splash" | "space_startup" => "Next startup",
        "install_agent_skills" => "Next skill-install opportunity",
        "walkthrough_voice" => "Narration generation only; bundled playback unchanged",
        "session_naming" => "Next pane or tab",
        "mux.instance" | "mux.socket" => {
            "Next endpoint resolution / host launch; live backend unchanged"
        }
        "mux.attach_on_new" => "Next pmux new",
        "mux.allow_mixed_case_session_names" => "Next session name",
        "mux.space_open_runs_commands" => "Next Space open",
        "mux.remote_size" => "Next daemon start; no automatic restart",
        "mux.shell" => "Next shell selection",
        "window_opacity" | "chrome_opacity" => {
            "Live with alpha support; opaque non-macOS windows may need restart"
        }
        "window_blur" => "Live where compositor blur is available",
        "font_px" => "Live; logical pixels, physical rendering minimum 10px",
        "automatic_update_checks" => "Live automatic checks on macOS",
        "start_at_login" => "Updates login registration where supported",
        "settings_panel" => "Opens this editor on the next settings action",
        _ => "Live after config reload",
    }
}

pub(super) fn catalog() -> Vec<Control> {
    let mut rows = vec![
        row(Section::Appearance, "settings_panel", Kind::Bool),
        row(Section::Appearance, "tab_strip", choice("tab_strip")),
        row(Section::Input, "link_click", choice("link_click")),
        row(Section::Input, "async_paste", Kind::Bool),
        row(Section::Appearance, "pane_titles", choice("pane_titles")),
        row(Section::Appearance, "chrome_style", choice("chrome_style")),
        row(Section::Appearance, "bar_color", choice("bar_color")),
        row(Section::Layout, "layout", choice("layout")),
        row(Section::Layout, "sidebar_width_px", integer(200, 2000, 10)),
        row(Section::Layout, "sidebar_collapsed", Kind::Bool),
        row(Section::Layout, "sidebar_harness_icons", Kind::Bool),
        row(Section::Advanced, "render_timer", choice("render_timer")),
        row(Section::Advanced, "macos_present", Kind::Text),
        row(
            Section::Advanced,
            "pty_drain_budget_ms",
            integer(1, 1000, 1),
        ),
        row(
            Section::Advanced,
            "render_timer_log_every_frame",
            Kind::Bool,
        ),
        row(Section::Advanced, "async_file_writes", Kind::Bool),
        row(Section::Appearance, "theme", Kind::Theme),
        row(Section::Appearance, "focus_border", Kind::BorderColor),
        row(
            Section::Appearance,
            "focus_border_animation",
            choice("focus_border_animation"),
        ),
        row(
            Section::Appearance,
            "focus_border_animation_ms",
            integer(50, 5000, 10),
        ),
        row(
            Section::Appearance,
            "focus_border_animation_head",
            Kind::Bool,
        ),
        row(Section::Startup, "splash", Kind::Bool),
        row(Section::Startup, "start_at_login", Kind::Bool),
        row(Section::Startup, "splash_animation", Kind::Bool),
        row(Section::Appearance, "reduced_motion", Kind::Bool),
        row(Section::Startup, "install_agent_skills", Kind::Bool),
        row(Section::Startup, "automatic_update_checks", Kind::Bool),
        row(Section::Fonts, "font", Kind::Font),
        row(Section::Fonts, "font_fallback", Kind::Strings),
        row(Section::Fonts, "font_px", float(6.0, 72.0, 0.5)),
        row(Section::Fonts, "font_ligatures", Kind::Bool),
        row(Section::Fonts, "font_features", Kind::Strings),
        row(Section::Layout, "panes", integer(1, 8, 1)),
        row(Section::Layout, "window_padding_px", integer(0, 128, 1)),
        row(Section::Layout, "pane_gap_px", integer(0, 128, 1)),
        row(Section::Layout, "pane_padding_px", integer(0, 128, 1)),
        row(Section::Spaces, "space_rail", choice("space_rail")),
        row(Section::Spaces, "space_reorder", Kind::Bool),
        row(Section::Advanced, "snapshot_client", Kind::Bool),
        row(Section::Advanced, "selective_border_rings", Kind::Bool),
        row(Section::Spaces, "space_rail_chip_cols", Kind::ChipWidth),
        row(Section::Spaces, "space_rail_width_cols", integer(8, 60, 1)),
        row(Section::Spaces, "restore_blank_terminals", Kind::Bool),
        row(Section::Input, "alt_screen_scrollback", Kind::Bool),
        row(Section::Spaces, "space_rail_pane_names", Kind::Bool),
        row(Section::Spaces, "space_autosave", Kind::Bool),
        row(Section::Spaces, "session_naming", choice("session_naming")),
        row(Section::Spaces, "space_startup", choice("space_startup")),
        row(Section::Notifications, "visual_bell", Kind::Bool),
        row(Section::Notifications, "pane_visual_bell", Kind::Bool),
        row(Section::Notifications, "audible_bell", Kind::Bool),
        row(Section::Notifications, "bell_toaster", Kind::Bool),
        row(
            Section::Notifications,
            "bell_toaster_ms",
            integer(500, 60000, 500),
        ),
        row(Section::Notifications, "drag_toaster", Kind::Bool),
        row(Section::Notifications, "toasts", choice("toasts")),
        row(Section::Notifications, "os_notify_bell", Kind::Bool),
        row(Section::Notifications, "attention_sound", Kind::Bool),
        row(Section::Notifications, "attention_badge", Kind::Bool),
        row(Section::Notifications, "os_notify_attention", Kind::Bool),
        row(Section::Startup, "walkthrough_audio", Kind::Bool),
        row(Section::Startup, "walkthrough_voice", Kind::Text),
        row(Section::Appearance, "background_image", Kind::Text),
        row(
            Section::Appearance,
            "background_opacity",
            float(0.0, 1.0, 0.05),
        ),
        row(Section::Appearance, "background_blur_px", integer(0, 64, 1)),
        row(
            Section::Appearance,
            "pane_opacity_active",
            float(0.0, 1.0, 0.05),
        ),
        row(
            Section::Appearance,
            "overlay_opacity",
            float(0.0, 1.0, 0.05),
        ),
        row(
            Section::Appearance,
            "pane_opacity_inactive",
            float(0.0, 1.0, 0.05),
        ),
        row(Section::Appearance, "window_opacity", float(0.0, 1.0, 0.05)),
        row(Section::Appearance, "chrome_opacity", float(0.0, 1.0, 0.05)),
        row(Section::Appearance, "window_blur", Kind::Bool),
        row(Section::Appearance, "hover_blend", float(0.0, 0.3, 0.01)),
        row(Section::Input, "macos_shortcuts", Kind::Bool),
        row(Section::Input, "smart_ctrl_c", Kind::Bool),
        row(Section::Accessibility, "a11y.os_tree", Kind::Bool),
        row(Section::Accessibility, "a11y.announce", Kind::Bool),
        row(Section::Spaces, "spaces.autosave", Kind::Bool),
        row(Section::Colors, "theme_overrides.default_fg", Kind::Color),
        row(Section::Colors, "theme_overrides.default_bg", Kind::Color),
        row(Section::Colors, "theme_overrides.chrome_fg", Kind::Color),
        row(Section::Colors, "theme_overrides.chrome_bg", Kind::Color),
        row(
            Section::Colors,
            "theme_overrides.tab_active_bg",
            Kind::Color,
        ),
        row(
            Section::Colors,
            "theme_overrides.pane_backdrop",
            Kind::Color,
        ),
        row(Section::Colors, "theme_overrides.pane_border", Kind::Color),
        row(Section::Colors, "theme_overrides.overlay_bg", Kind::Color),
        row(Section::Colors, "theme_overrides.unseen_badge", Kind::Color),
        row(Section::Colors, "theme_overrides.mail_letter", Kind::Color),
        row(Section::Colors, "theme_overrides.active_badge", Kind::Color),
        row(
            Section::Colors,
            "theme_overrides.attention_badge",
            Kind::Color,
        ),
        row(Section::Colors, "theme_overrides.cursor_fg", Kind::Pair),
        row(Section::Colors, "theme_overrides.selection_fg", Kind::Pair),
        row(Section::Colors, "theme_overrides.ansi", Kind::Ansi),
        row(Section::Remote, "remote", Kind::Remote),
        row(Section::Mux, "mux.instance", Kind::Text),
        row(Section::Mux, "mux.socket", Kind::Text),
        row(Section::Mux, "mux.attach_on_new", Kind::Bool),
        row(
            Section::Mux,
            "mux.allow_mixed_case_session_names",
            Kind::Bool,
        ),
        row(
            Section::Mux,
            "mux.space_open_runs_commands",
            choice("mux.space_open_runs_commands"),
        ),
        row(Section::Mux, "mux.remote_size", choice("mux.remote_size")),
        row(Section::Mux, "mux.shell", Kind::Text),
    ];
    for row in &mut rows {
        if matches!(row.kind, Kind::Pair) {
            let second = row.key().replace("_fg", "_bg");
            row.paths
                .push(second.split('.').map(str::to_owned).collect());
            row.label = if second.contains("cursor") {
                "Cursor colors"
            } else {
                "Selection colors"
            }
            .into();
            row.help = "Foreground and background save/reset together".into();
        }
        if row.key() == "space_autosave" {
            row.label = "Legacy autosave override".into();
            row.help =
                "Ignored while Spaces autosave is set; reset removes this legacy layer".into();
        }
        if row.key() == "spaces.autosave" {
            row.label = "Spaces autosave".into();
            row.default = Some(true.into());
            row.help = "Overrides the legacy autosave choice; Reset all removes both layers".into();
        }
        if row.key() == "a11y.os_tree"
            || row.key() == "a11y.announce"
            || row.key() == "mux.attach_on_new"
        {
            row.default = Some(true.into());
        }
    }
    for action in Action::all() {
        let mut control = row(
            Section::Keybindings,
            &format!("keys.{}", action.name()),
            Kind::Binding(action),
        );
        control.label = action.name().replace('_', " ");
        control.help = action.describe();
        rows.push(control);
    }
    rows
}
