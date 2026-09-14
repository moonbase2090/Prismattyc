//! Optional user config file with hot reload.
//!
//! `$PRISMATTYC_CONFIG`, else `$XDG_CONFIG_HOME/prismattyc/config.toml`, else
//! `~/.config/prismattyc/config.toml`. CLI flags and `PRISMATTYC_*` env vars always win
//! over the file, at startup and across reloads. Reload delivery is a 1s
//! mtime poller on the file path (not the parent directory). Rename-replace
//! flips the inode, so editor saves are caught without inotify.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::MAX_INITIAL_PANES;

pub const FONT_PX_RANGE: std::ops::RangeInclusive<f32> = 6.0..=72.0;
pub const SPACING_PX_RANGE: std::ops::RangeInclusive<usize> = 0..=128;
pub const DEFAULT_WINDOW_PADDING_PX: usize = 3;
pub const DEFAULT_PANE_GAP_PX: usize = 3;
pub const DEFAULT_PANE_PADDING_PX: usize = 5;
pub const DEFAULT_BELL_TOASTER_MS: u64 = 10_000;
pub const BELL_TOASTER_MS_RANGE: std::ops::RangeInclusive<u64> = 500..=60_000;
pub const DEFAULT_BACKGROUND_OPACITY: f32 = 0.35;
pub const BACKGROUND_OPACITY_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;
pub const DEFAULT_BACKGROUND_BLUR_PX: u32 = 0;
pub const BACKGROUND_BLUR_PX_RANGE: std::ops::RangeInclusive<u32> = 0..=64;
pub const DEFAULT_PANE_OPACITY: f32 = 1.0;
/// Fully opaque window. Below 1.0 the window ground carries alpha (PT-87).
pub const DEFAULT_WINDOW_OPACITY: f32 = 1.0;
pub const DEFAULT_WINDOW_BLUR: bool = false;
/// Immediate chrome-hover blend. Dark themes brighten; light themes darken.
pub const DEFAULT_HOVER_BLEND: f32 = 0.10;
pub const HOVER_BLEND_RANGE: std::ops::RangeInclusive<f32> = 0.0..=0.3;
/// Spaces rail edge (PT-91).
pub const DEFAULT_SPACE_RAIL: &str = "bottom";
/// Widest spaces-rail chip in cells; chips fit their labels (PT-123).
/// `0` means the built-in cap of 28.
pub const DEFAULT_SPACE_RAIL_CHIP_COLS: usize = 0;
pub const SPACE_RAIL_CHIP_COLS_RANGE: std::ops::RangeInclusive<usize> = 6..=40;
pub const DEFAULT_WALKTHROUGH_VOICE: &str = "russ";

/// Render instrumentation output. Off keeps the hot path silent; `osd` paints
/// the last frame on screen; `log` writes it to stderr; `both` does both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderTimer {
    #[default]
    Off,
    Osd,
    Log,
    Both,
}

impl RenderTimer {
    pub fn shows_osd(self) -> bool {
        matches!(self, Self::Osd | Self::Both)
    }

    pub fn logs(self) -> bool {
        matches!(self, Self::Log | Self::Both)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabStripMode {
    #[default]
    Auto,
    Always,
    Multi,
}

/// Multi-pane title row (PT-190). `focused` shows the focused pane's OSC
/// title; `hover` keeps the PT-148 handle-hover preview only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneTitlesMode {
    #[default]
    Focused,
    Hover,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Tab strip visibility: `auto`, `always`, or `multi` (default `auto`).
    pub tab_strip: Option<TabStripMode>,
    /// Multi-pane title row: `focused` (default) or `hover`. Sibling of
    /// `tab_strip` because TOML cannot nest a table under `tab_strip = "auto"`.
    pub pane_titles: Option<PaneTitlesMode>,
    /// Render timing output. Default `off`; hot-reloaded.
    pub render_timer: Option<RenderTimer>,
    /// Log every rendered frame when `render_timer` includes `log`. Default false.
    pub render_timer_log_every_frame: Option<bool>,
    /// Named built-in theme, display name, config-local theme name, or an
    /// absolute path to a Prismattyc theme TOML file.
    pub theme: Option<String>,
    /// Parsed palette captured with this config delivery. Keeping the data in
    /// the snapshot means re-saving an unchanged config reloads a custom theme
    /// whose file contents changed.
    #[serde(skip)]
    pub(crate) resolved_theme: Option<crate::theme::Theme>,
    /// Focus border color: spectrum name (`coral`…`ink`) or 0-based index
    /// (bare integer or string). Validated by [`load`].
    #[serde(default, deserialize_with = "focus_border_spec")]
    pub focus_border: Option<String>,
    /// Focus border animation: `"light-cycle"` traces the border like a Tron
    /// light cycle on focus change; `"none"` (default) keeps it static.
    pub focus_border_animation: Option<String>,
    /// Light-cycle sweep duration in milliseconds (50–5000; default 280).
    pub focus_border_animation_ms: Option<u64>,
    /// Draw the bright "vehicle" box at the sweep's leading edge (default
    /// true); `false` leaves only the growing trail.
    pub focus_border_animation_head: Option<bool>,
    /// Show the launch splash on a bare first window. Default true.
    /// Startup only. `--no-splash` and `PRISMATTYC_NO_SPLASH` still hide it.
    pub splash: Option<bool>,
    /// Animate the launch splash's word art (beam sweep, reflection passes,
    /// lens flares). Default true; `false` shows the art static.
    pub splash_animation: Option<bool>,
    /// Primary font path. `PRISMATTYC_HOST_FONT` still wins when set.
    pub font: Option<PathBuf>,
    /// Extra fallback faces, tried after the built-in chain.
    pub font_fallback: Option<Vec<PathBuf>>,
    /// Cell font size in px before display scaling.
    pub font_px: Option<f32>,
    /// Enable host-only OpenType shaping for eligible terminal-grid runs.
    /// Default false so the legacy ASCII raster path remains unchanged.
    pub font_ligatures: Option<bool>,
    /// OpenType feature tags. A tag may be prefixed with `-` to disable it.
    /// Defaults to `calt` and `liga` when ligatures are enabled.
    pub font_features: Option<Vec<String>>,
    /// Initial pane count. Startup only — never hot-applied.
    pub panes: Option<usize>,
    /// Window edge to pane chrome, in physical pixels.
    pub window_padding_px: Option<usize>,
    /// Gap between pane chrome rectangles, in physical pixels.
    pub pane_gap_px: Option<usize>,
    /// Pane chrome to terminal cell content, in physical pixels.
    pub pane_padding_px: Option<usize>,
    /// Spaces rail edge: `bottom` (default), `left`, `top`, `right`, `off`.
    pub space_rail: Option<String>,
    /// Widest spaces-rail chip in cells (6–40); chips fit their labels up
    /// to it. `0` (default) means 28.
    pub space_rail_chip_cols: Option<usize>,
    /// Fixed side rail width in cells. Default 18.
    pub space_rail_width_cols: Option<usize>,
    /// Recreate blank terminal layouts and working directories on restore.
    pub restore_blank_terminals: Option<bool>,
    /// Show live pane names below each Space name. Default true.
    pub space_rail_pane_names: Option<bool>,
    /// Save changed Space layouts after a short idle period. Default false.
    pub space_autosave: Option<bool>,
    /// Ask for session names or assign suggested names automatically.
    pub session_naming: Option<String>,
    /// Startup behavior: ask (default), restore, or fresh.
    pub space_startup: Option<String>,
    /// Flash the window on BEL (invert for ~120ms). Default true.
    pub visual_bell: Option<bool>,
    /// Play the bundled "Zen" bell sound on BEL. Default true. Linux tries
    /// `paplay`, `pw-play`, `aplay`; macOS uses `afplay`.
    pub audible_bell: Option<bool>,
    /// Show a toast on the pane that rang BEL (" bell ", top-right).
    /// Default true.
    pub bell_toaster: Option<bool>,
    /// Bell toast linger in milliseconds (500–60000). Default 10000.
    pub bell_toaster_ms: Option<u64>,
    /// Show "Moving tab NAME → …" while a tab chip or pane handle is being
    /// dragged (PT-79). Default true.
    pub drag_toaster: Option<bool>,
    /// Raise an OS notification on BEL while the window is unfocused.
    /// Default false. Linux: `notify-send`; macOS: `osascript`.
    pub os_notify_bell: Option<bool>,
    /// Play the attention cue when an agent asks for human input. Default true.
    pub attention_sound: Option<bool>,
    /// Draw the attention badge in the tab strip. Default true.
    pub attention_badge: Option<bool>,
    /// Raise an OS notification for attention when the pane is not selected or
    /// the window is unfocused. Default true.
    pub os_notify_attention: Option<bool>,
    /// Play bundled walkthrough narration clips. Default true. Missing clips
    /// stay silent. Hot-reloaded.
    pub walkthrough_audio: Option<bool>,
    /// ElevenLabs voice name for `scripts/walkthrough-voice.sh`. Generation
    /// time only; playback uses whatever clips are bundled. Default `russ`.
    pub walkthrough_voice: Option<String>,
    /// Absolute path to a PNG painted under the cell grid. PNG only.
    pub background_image: Option<PathBuf>,
    /// How much of the image shows through (0 = flat theme bg). Default 0.35.
    pub background_opacity: Option<f32>,
    /// Box-blur radius in pixels (0..=64). Default 0.
    pub background_blur_px: Option<u32>,
    /// Cell-background opacity of the focused (or zoomed) pane. Default 1.0.
    pub pane_opacity_active: Option<f32>,
    /// Host-owned overlay surface opacity. When unset, follows the active
    /// pane opacity. Default 1.0 when set explicitly.
    pub overlay_opacity: Option<f32>,
    /// Cell-background opacity of every other pane. Default 1.0.
    pub pane_opacity_inactive: Option<f32>,
    /// Opacity of the window ground and default-background cells (0.0-1.0).
    /// Below 1.0 the window is created with an alpha visual so the desktop
    /// shows through. Default 1.0. Needs a present path that carries alpha.
    pub window_opacity: Option<f32>,
    /// Opacity of the chrome bars (tab strip, footer rail). Defaults to
    /// `window_opacity`.
    pub chrome_opacity: Option<f32>,
    /// Ask the compositor to blur what is behind the window. Default false.
    /// No-op where no blur protocol is reachable.
    pub window_blur: Option<bool>,
    /// Immediate interactive-chrome hover blend (0.0-0.3). Default 0.10.
    pub hover_blend: Option<f32>,
    /// Mux defaults. Host does not apply these; accepted so a shared
    /// `config.toml` with `[mux]` is not rejected.
    #[serde(default)]
    pub mux: Option<prismattyc_mux::MuxSection>,
    /// `[keys]`: host action name → chord string or array of chord strings
    /// (ADR-0015). Validated by [`load`]; an entry replaces that action's
    /// default chords. See `crate::keybind`.
    #[serde(default)]
    pub keys: Option<std::collections::BTreeMap<String, crate::keybind::KeysValue>>,
    /// `[a11y]` (ADR-0016). Defaults on when the table is absent.
    #[serde(default)]
    pub a11y: Option<A11ySection>,
    /// `[theme_overrides]` (PT-207). Recolours keys of the named theme;
    /// applied by [`load`] so hot reload and the picker both keep them.
    #[serde(default)]
    pub theme_overrides: Option<crate::theme::ThemeOverrides>,
}

/// Host accessibility switches (PT-173 / PT-175).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A11ySection {
    pub os_tree: Option<bool>,
    pub announce: Option<bool>,
}

impl ConfigFile {
    /// The effective key table. Falls back to the defaults if the table
    /// somehow fails validation (it cannot after [`load`], but never panic).
    pub fn loaded_keymap(&self) -> crate::keybind::KeyMap {
        crate::keybind::KeyMap::from_config(self.keys.as_ref())
            .unwrap_or_else(|_| crate::keybind::KeyMap::default())
    }

    pub fn loaded_theme(&self) -> crate::theme::Theme {
        self.resolved_theme
            .clone()
            .unwrap_or_else(|| crate::theme::default_theme().clone())
    }

    pub fn render_timer(&self) -> RenderTimer {
        self.render_timer.unwrap_or_default()
    }

    pub fn render_timer_log_every_frame(&self) -> bool {
        self.render_timer_log_every_frame.unwrap_or(false)
    }

    pub fn tab_strip(&self) -> TabStripMode {
        self.tab_strip.unwrap_or_default()
    }

    pub fn pane_titles(&self) -> PaneTitlesMode {
        self.pane_titles.unwrap_or_default()
    }

    /// Show the launch splash on a bare first window. Default true.
    pub fn splash(&self) -> bool {
        self.splash.unwrap_or(true)
    }

    /// ADR-0016: AccessKit registration. Default on.
    pub fn a11y_os_tree(&self) -> bool {
        self.a11y
            .as_ref()
            .and_then(|section| section.os_tree)
            .unwrap_or(true)
    }

    /// ADR-0016: live-region speech. Default on.
    pub fn a11y_announce(&self) -> bool {
        self.a11y
            .as_ref()
            .and_then(|section| section.announce)
            .unwrap_or(true)
    }

    pub fn window_padding_px(&self) -> usize {
        self.window_padding_px.unwrap_or(DEFAULT_WINDOW_PADDING_PX)
    }

    pub fn pane_gap_px(&self) -> usize {
        self.pane_gap_px.unwrap_or(DEFAULT_PANE_GAP_PX)
    }

    pub fn pane_padding_px(&self) -> usize {
        self.pane_padding_px.unwrap_or(DEFAULT_PANE_PADDING_PX)
    }

    /// Validated by [`load`]; an unknown spelling never reaches here.
    pub fn space_rail(&self) -> crate::space_rail::RailSide {
        self.space_rail
            .as_deref()
            .and_then(crate::space_rail::RailSide::parse)
            .unwrap_or_default()
    }

    pub fn space_rail_chip_cols(&self) -> usize {
        self.space_rail_chip_cols
            .unwrap_or(DEFAULT_SPACE_RAIL_CHIP_COLS)
    }

    pub fn visual_bell(&self) -> bool {
        self.visual_bell.unwrap_or(true)
    }

    pub fn audible_bell(&self) -> bool {
        self.audible_bell.unwrap_or(true)
    }

    pub fn bell_toaster(&self) -> bool {
        self.bell_toaster.unwrap_or(true)
    }

    pub fn bell_toaster_ms(&self) -> u64 {
        self.bell_toaster_ms.unwrap_or(DEFAULT_BELL_TOASTER_MS)
    }

    pub fn drag_toaster(&self) -> bool {
        self.drag_toaster.unwrap_or(true)
    }

    pub fn os_notify_bell(&self) -> bool {
        self.os_notify_bell.unwrap_or(false)
    }

    pub fn attention_sound(&self) -> bool {
        self.attention_sound.unwrap_or(true)
    }

    pub fn attention_badge(&self) -> bool {
        self.attention_badge.unwrap_or(true)
    }

    pub fn os_notify_attention(&self) -> bool {
        self.os_notify_attention.unwrap_or(true)
    }

    pub fn walkthrough_audio(&self) -> bool {
        self.walkthrough_audio.unwrap_or(true)
    }

    pub fn background_opacity(&self) -> f32 {
        self.background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY)
    }

    pub fn background_blur_px(&self) -> u32 {
        self.background_blur_px
            .unwrap_or(DEFAULT_BACKGROUND_BLUR_PX)
    }

    pub fn pane_opacity_active(&self) -> f32 {
        self.pane_opacity_active.unwrap_or(DEFAULT_PANE_OPACITY)
    }

    /// Overlay surfaces follow the focused pane unless they have their own
    /// configured opacity. Clamp direct snapshots as a final safety net.
    pub fn overlay_opacity(&self) -> f32 {
        self.overlay_opacity
            .unwrap_or_else(|| self.pane_opacity_active())
            .clamp(0.0, 1.0)
    }

    pub fn pane_opacity_inactive(&self) -> f32 {
        self.pane_opacity_inactive.unwrap_or(DEFAULT_PANE_OPACITY)
    }

    pub fn window_opacity(&self) -> f32 {
        self.window_opacity.unwrap_or(DEFAULT_WINDOW_OPACITY)
    }

    /// Chrome bars follow `window_opacity` unless the key is set explicitly.
    pub fn chrome_opacity(&self) -> f32 {
        self.chrome_opacity.unwrap_or_else(|| self.window_opacity())
    }

    pub fn window_blur(&self) -> bool {
        self.window_blur.unwrap_or(DEFAULT_WINDOW_BLUR)
    }

    pub fn hover_blend(&self) -> f32 {
        self.hover_blend.unwrap_or(DEFAULT_HOVER_BLEND)
    }

    pub fn font_ligatures(&self) -> bool {
        self.font_ligatures.unwrap_or(false)
    }

    pub fn font_features(&self) -> Vec<String> {
        self.font_features
            .clone()
            .unwrap_or_else(|| vec!["calt".to_string(), "liga".to_string()])
    }
}

/// OpenType feature tags use four ASCII alphanumeric characters or spaces.
/// A leading `-` disables the feature.
fn valid_feature_tag(tag: &str) -> bool {
    let body = tag.strip_prefix('-').unwrap_or(tag);
    body.len() == 4
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b' ')
}

/// `focus_border = "violet"` and `focus_border = 4` both work: TOML integers
/// are folded into the string form that `parse_focus_border` accepts.
fn focus_border_spec<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Spec {
        Text(String),
        Index(i64),
    }
    Ok(
        Option::<Spec>::deserialize(deserializer)?.map(|spec| match spec {
            Spec::Text(text) => text,
            Spec::Index(index) => index.to_string(),
        }),
    )
}

pub fn config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("PRISMATTYC_CONFIG") {
        return PathBuf::from(explicit);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")));
    base.map_or_else(
        || PathBuf::from("prismattyc-config.toml"),
        |base| base.join("prismattyc").join("config.toml"),
    )
}

/// Missing file is not an error (all defaults); unreadable or invalid is.
pub fn load(path: &Path) -> Result<ConfigFile> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigFile::default())
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    parse(&raw, path)
}

/// Persist a built-in theme choice without reformatting the rest of the user
/// config. A sibling temporary file is fsynced and renamed so the config
/// poller sees one complete snapshot.
pub fn save_theme(path: &Path, theme_id: &str) -> Result<()> {
    crate::theme::load(Some(theme_id), path)?;
    save_preference(path, "theme", toml_edit::value(theme_id))
}

/// Save one preference atomically and preserve unrelated settings and comments.
pub fn save_preference(path: &Path, key: &str, value: toml_edit::Item) -> Result<()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut document = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {} before saving theme", path.display()))?;
    document[key] = value;
    parse(&document.to_string(), path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".{filename}.prism-theme-{}-{nonce}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(document.to_string().as_bytes())
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temporary, metadata.permissions())
                .with_context(|| format!("preserve permissions for {}", path.display()))?;
        }
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replace {} with {}", path.display(), temporary.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn parse(raw: &str, path: &Path) -> Result<ConfigFile> {
    let mut config: ConfigFile =
        toml::from_str(raw).with_context(|| format!("parse {}", path.display()))?;
    if let Some(spec) = config.focus_border.as_deref() {
        anyhow::ensure!(
            crate::raster::parse_focus_border(spec).is_some(),
            "focus_border {spec:?} is not a spectrum name or 0-6 index"
        );
    }
    let mut theme = crate::theme::load(config.theme.as_deref(), path)?;
    if let Some(overrides) = &config.theme_overrides {
        crate::theme::apply_overrides(&mut theme, overrides)?;
    }
    config.resolved_theme = Some(theme);
    if let Some(animation) = config.focus_border_animation.as_deref() {
        anyhow::ensure!(
            matches!(animation, "light-cycle" | "none"),
            "focus_border_animation {animation:?} is not \"light-cycle\" or \"none\""
        );
    }
    if let Some(ms) = config.focus_border_animation_ms {
        anyhow::ensure!(
            (50..=5000).contains(&ms),
            "focus_border_animation_ms {ms} outside 50..=5000"
        );
    }
    if let Some(ms) = config.bell_toaster_ms {
        anyhow::ensure!(
            BELL_TOASTER_MS_RANGE.contains(&ms),
            "bell_toaster_ms {ms} outside {:?}",
            BELL_TOASTER_MS_RANGE
        );
    }
    if let Some(px) = config.font_px {
        anyhow::ensure!(
            FONT_PX_RANGE.contains(&px),
            "font_px {px} outside {:?}",
            FONT_PX_RANGE
        );
    }
    if let Some(features) = config.font_features.as_ref() {
        for feature in features {
            anyhow::ensure!(
                valid_feature_tag(feature),
                "font_features tag {feature:?} must be four ASCII alphanumeric characters or spaces, optionally prefixed with '-'"
            );
        }
    }
    if let Some(panes) = config.panes {
        anyhow::ensure!(
            (1..=MAX_INITIAL_PANES).contains(&panes),
            "panes {panes} outside 1..={MAX_INITIAL_PANES}"
        );
    }
    if let Some(keys) = config.keys.as_ref() {
        crate::keybind::KeyMap::from_config(Some(keys)).map_err(|e| anyhow::anyhow!(e))?;
    }
    for (name, value) in [
        ("window_padding_px", config.window_padding_px),
        ("pane_gap_px", config.pane_gap_px),
        ("pane_padding_px", config.pane_padding_px),
    ] {
        if let Some(value) = value {
            anyhow::ensure!(
                SPACING_PX_RANGE.contains(&value),
                "{name} {value} outside {:?}",
                SPACING_PX_RANGE
            );
        }
    }
    if let Some(mode) = config.session_naming.as_deref() {
        anyhow::ensure!(
            matches!(mode, "ask" | "auto" | "blank"),
            "session_naming must be ask, auto, or blank"
        );
    }
    if let Some(startup) = config.space_startup.as_deref() {
        anyhow::ensure!(
            matches!(startup, "ask" | "restore" | "fresh"),
            "space_startup must be ask, restore, or fresh"
        );
    }
    if let Some(spec) = config.space_rail.as_deref() {
        anyhow::ensure!(
            crate::space_rail::RailSide::parse(spec).is_some(),
            "space_rail {spec:?} must be bottom, left, top, right, or off"
        );
    }
    if let Some(cols) = config.space_rail_width_cols {
        anyhow::ensure!(
            (8..=60).contains(&cols),
            "space_rail_width_cols must be between 8 and 60"
        );
    }
    if let Some(cols) = config.space_rail_chip_cols {
        anyhow::ensure!(
            cols == 0 || SPACE_RAIL_CHIP_COLS_RANGE.contains(&cols),
            "space_rail_chip_cols {cols} must be 0 (auto) or inside {:?}",
            SPACE_RAIL_CHIP_COLS_RANGE
        );
    }
    if let Some(path) = config.background_image.as_ref() {
        anyhow::ensure!(
            path.is_absolute(),
            "background_image {:?} must be absolute",
            path
        );
    }
    if let Some(opacity) = config.background_opacity {
        anyhow::ensure!(
            BACKGROUND_OPACITY_RANGE.contains(&opacity),
            "background_opacity {opacity} outside {:?}",
            BACKGROUND_OPACITY_RANGE
        );
    }
    if let Some(blend) = config.hover_blend {
        anyhow::ensure!(
            HOVER_BLEND_RANGE.contains(&blend),
            "hover_blend {blend} outside {:?}",
            HOVER_BLEND_RANGE
        );
    }
    if let Some(blur) = config.background_blur_px {
        anyhow::ensure!(
            BACKGROUND_BLUR_PX_RANGE.contains(&blur),
            "background_blur_px {blur} outside {:?}",
            BACKGROUND_BLUR_PX_RANGE
        );
    }
    for (name, value) in [
        ("pane_opacity_active", config.pane_opacity_active),
        ("overlay_opacity", config.overlay_opacity),
        ("pane_opacity_inactive", config.pane_opacity_inactive),
        ("window_opacity", config.window_opacity),
        ("chrome_opacity", config.chrome_opacity),
    ] {
        if let Some(opacity) = value {
            anyhow::ensure!(
                BACKGROUND_OPACITY_RANGE.contains(&opacity),
                "{name} {opacity} outside {:?}",
                BACKGROUND_OPACITY_RANGE
            );
        }
    }
    Ok(config)
}

/// Watch for config changes. Each successful re-parse is sent as `Ok`; a
/// parse/validation failure of real file content is sent as `Err(message)` so
/// the host can surface it to the user (footer bar) instead of stderr only.
/// Mid-save transients (editor rename-away leaving the path briefly missing,
/// zero-length truncate-then-write) are silently skipped, never applied and
/// never reported — a delivery only ever comes from real file content. To
/// reset to defaults at runtime, save a file containing just a comment. The
/// returned watcher must be kept alive.
pub type WatchDelivery = std::result::Result<ConfigFile, String>;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Keep-alive for the config poller thread. Dropping it stops the loop.
pub struct ConfigWatch {
    _stop: mpsc::Sender<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
    ino: u64,
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        mtime: meta.modified().ok()?,
        len: meta.len(),
        ino: {
            #[cfg(unix)]
            {
                std::os::unix::fs::MetadataExt::ino(&meta)
            }
            #[cfg(not(unix))]
            {
                0
            }
        },
    })
}

/// 1s mtime poller on the config *file*. `wake`, when set, rings a
/// `ControlFlow::Wait` host so a reload applies without waiting for the next
/// window event.
pub fn watch_with_wake(
    path: PathBuf,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<(ConfigWatch, mpsc::Receiver<WatchDelivery>)> {
    if let Some(dir) = path.parent() {
        // Ensure the directory exists so a config created later still hot-loads.
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let (tx, rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let mut last = stamp(&path);
    thread::Builder::new()
        .name("prism-config-poll".into())
        .spawn(move || loop {
            match stop_rx.recv_timeout(POLL_INTERVAL) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let now = stamp(&path);
            if now == last {
                continue;
            }
            last = now;
            // ENOENT: keep polling so a late-created file still hot-loads.
            if now.is_none() {
                continue;
            }
            // Re-read here (not via `load`) so mid-save transients are separable:
            // a missing or zero-length file is an editor in mid-flight, not intent.
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) if raw.is_empty() => continue,
                Ok(raw) => raw,
                Err(_) => continue,
            };
            match parse(&raw, &path) {
                Ok(config) => {
                    let _ = tx.send(Ok(config));
                }
                Err(error) => {
                    eprintln!("prismattyc-host: config reload skipped: {error:#}");
                    let _ = tx.send(Err(format!("{error:#}")));
                }
            }
            if let Some(wake) = &wake {
                wake();
            }
        })?;
    Ok((ConfigWatch { _stop: stop_tx }, rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("prism-config-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_defaults_and_partial_files_parse() {
        let dir = temp_dir("parse");
        let path = dir.join("config.toml");
        assert_eq!(load(&path).unwrap(), ConfigFile::default());

        std::fs::write(
            &path,
            "focus_border = \"violet\"\nfont_px = 18.0\nwindow_padding_px = 7\npane_gap_px = 9\npane_padding_px = 11\n",
        )
        .unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.focus_border.as_deref(), Some("violet"));
        assert_eq!(config.font_px, Some(18.0));
        assert_eq!(config.font, None);
        assert_eq!(config.window_padding_px(), 7);
        assert_eq!(config.pane_gap_px(), 9);
        assert_eq!(config.pane_padding_px(), 11);
        assert_eq!(config.font_ligatures, None);
        assert_eq!(config.font_features, None);
        assert!(!ConfigFile::default().font_ligatures());
        assert_eq!(
            ConfigFile::default().font_features(),
            vec!["calt".to_string(), "liga".to_string()]
        );

        // Bare-integer spelling folds into the index string form.
        std::fs::write(&path, "focus_border = 4\n").unwrap();
        assert_eq!(load(&path).unwrap().focus_border.as_deref(), Some("4"));
        // Both animation values load; anything else is rejected above.
        std::fs::write(
            &path,
            "[keys]\nsplit_right = \"ctrl+alt+enter\"\nfind = [\"ctrl+shift+f\", \"ctrl+alt+f\"]\n",
        )
        .unwrap();
        let keys = load(&path).unwrap();
        let map = keys.loaded_keymap();
        assert_eq!(
            map.spellings(crate::keybind::Action::SplitRight),
            vec!["ctrl+alt+enter".to_string()]
        );
        assert_eq!(map.chords(crate::keybind::Action::Find).len(), 2);
        assert_eq!(
            ConfigFile::default().loaded_keymap(),
            crate::keybind::KeyMap::default()
        );
        for bad in [
            "[keys]\nsplit_rite = \"ctrl+alt+enter\"\n",
            "[keys]\nfind = \"ctrl+bogus\"\n",
            "[keys]\nfind = \"shift+f\"\n",
            "[keys]\nsplit_right = \"ctrl+shift+w\"\n",
            "[keys]\ncopy = \"ctrl+c\"\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            let err = load(&path).unwrap_err().to_string();
            assert!(err.contains("keys"), "{bad}: {err}");
        }
        std::fs::write(
            &path,
            "font_ligatures = true\nfont_features = [\"liga\", \"-calt\"]\n",
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.font_features,
            Some(vec!["liga".to_string(), "-calt".to_string()])
        );
        assert!(loaded.font_ligatures());
        assert_eq!(loaded.font_features(), vec!["liga", "-calt"]);
        std::fs::write(&path, "splash = false\n").unwrap();
        assert_eq!(load(&path).unwrap().splash, Some(false));
        assert!(!load(&path).unwrap().splash());
        assert_eq!(ConfigFile::default().splash, None);
        assert!(ConfigFile::default().splash());
        std::fs::write(&path, "splash_animation = false\n").unwrap();
        assert_eq!(load(&path).unwrap().splash_animation, Some(false));
        assert_eq!(ConfigFile::default().splash_animation, None);
        std::fs::write(&path, "focus_border_animation = \"light-cycle\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().focus_border_animation.as_deref(),
            Some("light-cycle")
        );
        std::fs::write(&path, "render_timer = \"osd\"\n").unwrap();
        assert_eq!(load(&path).unwrap().render_timer(), RenderTimer::Osd);
        std::fs::write(&path, "render_timer_log_every_frame = true\n").unwrap();
        assert!(load(&path).unwrap().render_timer_log_every_frame());
        assert!(!ConfigFile::default().render_timer_log_every_frame());
        for value in ["log", "both", "off"] {
            std::fs::write(&path, format!("render_timer = \"{value}\"\n")).unwrap();
            assert_eq!(load(&path).unwrap().render_timer().logs(), value != "off");
        }
        std::fs::write(&path, "render_timer = \"bogus\"\n").unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, "focus_border_animation = \"none\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().focus_border_animation.as_deref(),
            Some("none")
        );
        std::fs::write(
            &path,
            "focus_border_animation_ms = 500\nfocus_border_animation_head = false\n",
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.focus_border_animation_ms, Some(500));
        assert_eq!(loaded.focus_border_animation_head, Some(false));
        std::fs::write(&path, "theme = \"Rosé Pine Moon\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().theme.as_deref(),
            Some("Rosé Pine Moon")
        );
        std::fs::write(&path, "[mux]\ninstance = \"work\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().mux.and_then(|mux| mux.instance),
            Some("work".into())
        );
        // Bell keys: defaults, then explicit values.
        assert!(ConfigFile::default().visual_bell());
        assert!(ConfigFile::default().audible_bell());
        assert!(ConfigFile::default().bell_toaster());
        assert_eq!(ConfigFile::default().bell_toaster_ms(), 10_000);
        assert!(!ConfigFile::default().os_notify_bell());
        assert!(ConfigFile::default().attention_sound());
        assert!(ConfigFile::default().attention_badge());
        assert!(ConfigFile::default().os_notify_attention());
        assert_eq!(
            ConfigFile::default().background_opacity(),
            DEFAULT_BACKGROUND_OPACITY
        );
        assert_eq!(ConfigFile::default().background_blur_px(), 0);
        assert_eq!(ConfigFile::default().pane_opacity_active(), 1.0);
        assert_eq!(ConfigFile::default().overlay_opacity(), 1.0);
        assert_eq!(ConfigFile::default().pane_opacity_inactive(), 1.0);
        assert_eq!(
            ConfigFile::default().window_opacity(),
            DEFAULT_WINDOW_OPACITY
        );
        assert_eq!(
            ConfigFile::default().chrome_opacity(),
            DEFAULT_WINDOW_OPACITY
        );
        assert!(!ConfigFile::default().window_blur());
        assert_eq!(ConfigFile::default().hover_blend(), DEFAULT_HOVER_BLEND);
        assert_eq!(ConfigFile::default().pane_titles(), PaneTitlesMode::Focused);
        std::fs::write(&path, "pane_titles = \"hover\"\n").unwrap();
        assert_eq!(load(&path).unwrap().pane_titles(), PaneTitlesMode::Hover);
        // chrome_opacity follows window_opacity unless set explicitly.
        let follows = ConfigFile {
            window_opacity: Some(0.8),
            ..ConfigFile::default()
        };
        assert_eq!(follows.chrome_opacity(), 0.8);
        let explicit = ConfigFile {
            window_opacity: Some(0.8),
            chrome_opacity: Some(0.5),
            ..ConfigFile::default()
        };
        assert_eq!(explicit.chrome_opacity(), 0.5);
        std::fs::write(&path, "window_opacity = 0.8\nchrome_opacity = 0.5\n").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.window_opacity(), 0.8);
        assert_eq!(loaded.chrome_opacity(), 0.5);
        assert!(ConfigFile::default().background_image.is_none());
        std::fs::write(
            &path,
            "visual_bell = false\naudible_bell = false\nbell_toaster = false\nbell_toaster_ms = 15000\nos_notify_bell = true\nattention_sound = false\nattention_badge = false\nos_notify_attention = false\n",
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert!(!loaded.visual_bell());
        assert!(!loaded.audible_bell());
        assert!(!loaded.bell_toaster());
        assert_eq!(loaded.bell_toaster_ms(), 15_000);
        assert!(loaded.os_notify_bell());
        assert!(!loaded.attention_sound());
        assert!(!loaded.attention_badge());
        assert!(!loaded.os_notify_attention());
        assert!(ConfigFile::default().walkthrough_audio());
        assert!(ConfigFile::default().walkthrough_voice.is_none());
        std::fs::write(&path, "walkthrough_voice = \"Daniel\"\n").unwrap();
        assert_eq!(
            load(&path).unwrap().walkthrough_voice.as_deref(),
            Some("Daniel")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_opacity_follows_pane_unless_explicit_and_clamps() {
        let follows = ConfigFile {
            pane_opacity_active: Some(0.65),
            ..ConfigFile::default()
        };
        assert_eq!(follows.overlay_opacity(), 0.65);

        let explicit = ConfigFile {
            pane_opacity_active: Some(0.65),
            overlay_opacity: Some(0.35),
            ..ConfigFile::default()
        };
        assert_eq!(explicit.overlay_opacity(), 0.35);

        let high = ConfigFile {
            overlay_opacity: Some(1.5),
            ..ConfigFile::default()
        };
        let low = ConfigFile {
            overlay_opacity: Some(-0.5),
            ..ConfigFile::default()
        };
        assert_eq!(high.overlay_opacity(), 1.0);
        assert_eq!(low.overlay_opacity(), 0.0);
    }

    #[test]
    fn invalid_values_and_unknown_keys_are_rejected() {
        let dir = temp_dir("invalid");
        let path = dir.join("config.toml");
        for bad in [
            "font_px = 200.0",
            "panes = 0",
            "panes = 99",
            "not_a_setting = true",
            "font_px = \"large\"",
            "focus_border = \"nope\"",
            "focus_border = 99",
            "window_padding_px = 129",
            "pane_gap_px = 129",
            "pane_padding_px = 129",
            "focus_border_animation = \"strobe\"",
            "focus_border_animation = 1",
            "focus_border_animation_ms = 10",
            "focus_border_animation_ms = 9000",
            "focus_border_animation_head = \"yes\"",
            "visual_bell = \"yes\"",
            "audible_bell = \"loud\"",
            "bell_toaster = 0",
            "bell_toaster_ms = 100",
            "bell_toaster_ms = 300000",
            "os_notify_bell = 1",
            "font_features = [\"ligatures\"]",
            "font_features = [\"ca!t\"]",
            "theme = \"not-a-theme\"",
            "theme = \"../relative/theme.toml\"",
            "background_opacity = 1.5",
            "background_opacity = -0.1",
            "background_blur_px = 999",
            "background_image = \"relative.png\"",
            "pane_opacity_active = 1.5",
            "pane_opacity_active = -0.1",
            "overlay_opacity = 1.5",
            "overlay_opacity = -0.1",
            "pane_opacity_inactive = 2.0",
            "pane_opacity_inactive = -0.01",
            "window_opacity = 1.5",
            "window_opacity = -0.1",
            "chrome_opacity = 1.01",
            "window_blur = \"yes\"",
            "hover_blend = -0.01",
            "hover_blend = 0.31",
            "[a11y]\nnot_a_setting = true",
            "pane_titles = \"always\"",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(load(&path).is_err(), "should reject: {bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PT-207: `[theme_overrides]` recolours the named theme and rejects
    /// bad values the way a bad theme file does.
    #[test]
    fn theme_overrides_recolour_and_validate() {
        let dir = temp_dir("theme-overrides");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "theme = \"monokai\"\n[theme_overrides]\nattention_badge = \"#123456\"\ntab_active_bg = \"#0a0b0c\"\n",
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        let theme = loaded.loaded_theme();
        assert_eq!(theme.id, "monokai");
        assert_eq!(theme.attention_badge, [0x12, 0x34, 0x56]);
        assert_eq!(theme.tab_active_bg, [0x0a, 0x0b, 0x0c]);
        assert!(theme.tab_active_bg_explicit);
        assert_eq!(
            theme.unseen_badge,
            crate::theme::load(Some("monokai"), &path)
                .unwrap()
                .unseen_badge,
            "untouched keys keep the named theme"
        );

        std::fs::write(&path, "[theme_overrides]\n").unwrap();
        assert_eq!(
            load(&path).unwrap().loaded_theme(),
            *crate::theme::default_theme(),
            "an empty table changes nothing"
        );

        for (raw, needle) in [
            (
                "[theme_overrides]\ndefault_fg = \"red\"\n",
                "theme_overrides.default_fg",
            ),
            (
                "[theme_overrides]\nansi = [\"#000000\"]\n",
                "must list 16 colours",
            ),
            (
                "[theme_overrides]\nselection_fg = \"#000000\"\n",
                "selection_fg and selection_bg",
            ),
            (
                "[theme_overrides]\nnot_a_colour = \"#000000\"\n",
                "not_a_colour",
            ),
        ] {
            std::fs::write(&path, raw).unwrap();
            let error = load(&path).unwrap_err().to_string();
            let chain = load(&path)
                .unwrap_err()
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(": ");
            assert!(
                chain.contains(needle),
                "{raw:?} must reject with {needle:?}: {error} / {chain}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a11y_os_tree_defaults_on_and_can_be_disabled() {
        assert!(ConfigFile::default().a11y_os_tree());
        let dir = temp_dir("a11y");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[a11y]\nos_tree = false\nannounce = false\n").unwrap();
        let loaded = load(&path).unwrap();
        assert!(!loaded.a11y_os_tree());
        assert!(!loaded.a11y_announce());
        std::fs::write(&path, "[a11y]\n").unwrap();
        assert!(load(&path).unwrap().a11y_os_tree());
        assert!(load(&path).unwrap().a11y_announce());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchanged_config_snapshot_detects_custom_theme_file_edits() {
        let dir = temp_dir("theme-reload");
        let path = dir.join("config.toml");
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(&path, "theme = \"custom\"\n").unwrap();
        let first = include_str!("../themes/prismattyc-default.toml");
        std::fs::write(themes.join("custom.toml"), first).unwrap();
        let before = load(&path).unwrap();

        let second = first.replace("default_bg = \"#121214\"", "default_bg = \"#232136\"");
        std::fs::write(themes.join("custom.toml"), second).unwrap();
        let after = load(&path).unwrap();

        assert_eq!(before.theme, after.theme, "config text stayed unchanged");
        assert_ne!(before, after, "resolved theme data participates in reload");
        assert_eq!(after.loaded_theme().default_bg, [0x23, 0x21, 0x36]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_theme_preserves_existing_toml_and_comments() {
        let dir = temp_dir("save-theme");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# keep this comment\nfocus_border = \"violet\"\ntheme = \"dracula\"\n",
        )
        .unwrap();

        save_theme(&path, "rose-pine-moon").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# keep this comment"), "{raw}");
        assert!(raw.contains("focus_border = \"violet\""), "{raw}");
        assert!(raw.contains("theme = \"rose-pine-moon\""), "{raw}");
        assert_eq!(load(&path).unwrap().loaded_theme().id, "rose-pine-moon");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_preferences_preserve_config_and_reject_invalid_values() {
        let dir = temp_dir("space-preferences");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# keep\nfont_px = 17.0\n[keys]\ncopy = \"ctrl+shift+c\"\n",
        )
        .unwrap();
        save_preference(&path, "space_rail", toml_edit::value("right")).unwrap();
        save_preference(&path, "space_autosave", toml_edit::value(true)).unwrap();
        save_preference(&path, "space_startup", toml_edit::value("restore")).unwrap();
        save_preference(&path, "session_naming", toml_edit::value("auto")).unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.session_naming.as_deref(), Some("auto"));
        assert_eq!(config.space_rail(), crate::space_rail::RailSide::Right);
        assert_eq!(config.space_autosave, Some(true));
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("# keep"));
        assert!(before.contains("font_px = 17.0"));
        assert!(save_preference(&path, "space_startup", toml_edit::value("invalid")).is_err());
        assert!(save_preference(&path, "session_naming", toml_edit::value("invalid")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pane_spacing_defaults() {
        let config = ConfigFile::default();
        assert_eq!(config.window_padding_px(), DEFAULT_WINDOW_PADDING_PX);
        assert_eq!(config.pane_gap_px(), DEFAULT_PANE_GAP_PX);
        assert_eq!(config.window_padding_px(), 3);
        assert_eq!(config.pane_gap_px(), 3);
        assert_eq!(config.pane_padding_px(), 5);
    }

    #[test]
    fn watcher_delivers_new_config_on_write() {
        let dir = temp_dir("watch");
        let path = dir.join("config.toml");
        let (_watcher, rx) = watch_with_wake(path.clone(), None).unwrap();
        std::fs::write(
            &path,
            "focus_border = \"coral\"\nfont_ligatures = true\nfont_features = [\"liga\", \"-calt\"]\n",
        )
        .unwrap();
        let mut latest = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(config)) => {
                    let done = config.focus_border.as_deref() == Some("coral")
                        && config.font_ligatures == Some(true)
                        && config.font_features
                            == Some(vec!["liga".to_string(), "-calt".to_string()]);
                    latest = Some(config);
                    if done {
                        break;
                    }
                }
                Ok(Err(error)) => panic!("valid save reported as error: {error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => panic!("watcher channel died: {error}"),
            }
        }
        assert_eq!(
            latest.and_then(|config| config.focus_border),
            Some("coral".to_string())
        );

        // Mid-save transients must not deliver defaults: rename the file away,
        // then truncate to zero bytes. Stale duplicates of the prior "coral"
        // content may still be in flight — only a defaults config is a bug.
        std::fs::rename(&path, dir.join("config.toml.bak")).unwrap();
        std::fs::write(&path, "").unwrap();
        while let Ok(delivery) = rx.recv_timeout(Duration::from_millis(400)) {
            let config = delivery.expect("transient must not be reported as an error");
            assert_eq!(
                config.focus_border.as_deref(),
                Some("coral"),
                "transient must not deliver a defaults config"
            );
        }
        // Real content flows again after the transient.
        std::fs::write(&path, "focus_border = \"amber\"\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut recovered = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(config)) = rx.recv_timeout(Duration::from_millis(200)) {
                if config.focus_border.as_deref() == Some("amber") {
                    recovered = true;
                    break;
                }
            }
        }
        assert!(recovered, "watcher must keep delivering after transients");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_reports_invalid_content_as_error_delivery() {
        let dir = temp_dir("watch-err");
        let path = dir.join("config.toml");
        let (_watcher, rx) = watch_with_wake(path.clone(), None).unwrap();
        std::fs::write(&path, "focus_border = \"nope\"\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut reported = None;
        while std::time::Instant::now() < deadline {
            if let Ok(Err(message)) = rx.recv_timeout(Duration::from_millis(200)) {
                reported = Some(message);
                break;
            }
        }
        let message = reported.expect("invalid content must deliver an error");
        assert!(message.contains("focus_border"), "{message}");
        // A valid save afterwards flows as Ok, clearing the condition.
        std::fs::write(&path, "focus_border = \"amber\"\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut recovered = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(config)) = rx.recv_timeout(Duration::from_millis(200)) {
                if config.focus_border.as_deref() == Some("amber") {
                    recovered = true;
                    break;
                }
            }
        }
        assert!(recovered, "watcher must deliver Ok after an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_delivers_rewrite_within_two_seconds() {
        let dir = temp_dir("watch-2s");
        let path = dir.join("config.toml");
        std::fs::write(&path, "focus_border = \"coral\"\n").unwrap();
        let (_watcher, rx) = watch_with_wake(path.clone(), None).unwrap();
        // Longer payload so (mtime, len, ino) changes even on 1s mtime granularity.
        std::fs::write(&path, "focus_border = \"violet\"\n").unwrap();
        let start = std::time::Instant::now();
        let deadline = start + Duration::from_secs(2);
        let mut delivered = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(config)) = rx.recv_timeout(Duration::from_millis(100)) {
                if config.focus_border.as_deref() == Some("violet") {
                    delivered = true;
                    break;
                }
            }
        }
        assert!(
            delivered,
            "rewrite must deliver within 2s (elapsed {:?})",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
