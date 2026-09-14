//! Named host themes: terminal defaults + ANSI 0-15 + Prismattyc chrome tokens.
//!
//! Built-ins are embedded from reviewable TOML files. A config may also name
//! a file in the sibling `themes/` directory or use an absolute file path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub type Rgb = [u8; 3];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub variant: ThemeVariant,
    pub source: String,
    pub license: String,
    pub default_fg: Rgb,
    pub default_bg: Rgb,
    pub chrome_fg: Rgb,
    pub chrome_bg: Rgb,
    /// Active tab-chip fill (PT-95). Derived from `chrome_bg` unless the
    /// theme file sets `tab_active_bg`.
    pub tab_active_bg: Rgb,
    /// `tab_active_bg` came from the theme file. Otherwise the host paints
    /// the active chip as a variation of the focus colour (PT-136).
    pub tab_active_bg_explicit: bool,
    /// Frame ground painted behind every pane when no `background_image` is
    /// set. Pane opacity blends the cell background toward this colour, so it
    /// must differ from `default_bg` or opacity is a no-op (PT-98).
    pub pane_backdrop: Rgb,
    pub pane_border: Rgb,
    pub overlay_bg: Rgb,
    pub unseen_badge: Rgb,
    pub mail_letter: Rgb,
    pub active_badge: Rgb,
    pub attention_badge: Rgb,
    pub cursor_fg: Option<Rgb>,
    pub cursor_bg: Option<Rgb>,
    pub selection_fg: Option<Rgb>,
    pub selection_bg: Option<Rgb>,
    pub ansi: [Rgb; 16],
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeVariant {
    Dark,
    Light,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    id: String,
    name: String,
    variant: ThemeVariant,
    source: String,
    license: String,
    default_fg: String,
    default_bg: String,
    chrome_fg: String,
    chrome_bg: String,
    #[serde(default)]
    tab_active_bg: Option<String>,
    #[serde(default)]
    pane_backdrop: Option<String>,
    pane_border: String,
    overlay_bg: String,
    unseen_badge: String,
    mail_letter: String,
    active_badge: String,
    #[serde(default)]
    attention_badge: Option<String>,
    cursor_fg: Option<String>,
    cursor_bg: Option<String>,
    selection_fg: Option<String>,
    selection_bg: Option<String>,
    ansi: [String; 16],
}

const BUILTIN_FILES: &[(&str, &str)] = &[
    (
        "prismattyc-default",
        include_str!("../themes/prismattyc-default.toml"),
    ),
    (
        "catppuccin-mocha",
        include_str!("../themes/catppuccin-mocha.toml"),
    ),
    ("tokyo-night", include_str!("../themes/tokyo-night.toml")),
    (
        "rose-pine-moon",
        include_str!("../themes/rose-pine-moon.toml"),
    ),
    ("monokai", include_str!("../themes/monokai.toml")),
    ("monokai-pro", include_str!("../themes/monokai-pro.toml")),
    (
        "monokai-spectrum",
        include_str!("../themes/monokai-spectrum.toml"),
    ),
    (
        "monokai-dimmed",
        include_str!("../themes/monokai-dimmed.toml"),
    ),
    (
        "monokai-remastered",
        include_str!("../themes/monokai-remastered.toml"),
    ),
    ("monokai-soda", include_str!("../themes/monokai-soda.toml")),
    (
        "monokai-vivid",
        include_str!("../themes/monokai-vivid.toml"),
    ),
    ("dracula", include_str!("../themes/dracula.toml")),
    ("ghost", include_str!("../themes/ghost.toml")),
    ("japanesque", include_str!("../themes/japanesque.toml")),
    (
        "hive-monochromatic",
        include_str!("../themes/hive-monochromatic.toml"),
    ),
    (
        "hive-two-tone",
        include_str!("../themes/hive-two-tone.toml"),
    ),
    (
        "hive-tri-tone",
        include_str!("../themes/hive-tri-tone.toml"),
    ),
    (
        "hive-complementary",
        include_str!("../themes/hive-complementary.toml"),
    ),
    (
        "hive-split-complementary",
        include_str!("../themes/hive-split-complementary.toml"),
    ),
    (
        "hive-analogous",
        include_str!("../themes/hive-analogous.toml"),
    ),
    ("hive-triadic", include_str!("../themes/hive-triadic.toml")),
    (
        "hive-high-contrast-dual",
        include_str!("../themes/hive-high-contrast-dual.toml"),
    ),
    (
        "hive-muted-professional",
        include_str!("../themes/hive-muted-professional.toml"),
    ),
    (
        "hive-night-hive",
        include_str!("../themes/hive-night-hive.toml"),
    ),
    (
        "hive-monochromatic-light",
        include_str!("../themes/hive-monochromatic-light.toml"),
    ),
    (
        "hive-tri-tone-light",
        include_str!("../themes/hive-tri-tone-light.toml"),
    ),
    (
        "hive-muted-professional-light",
        include_str!("../themes/hive-muted-professional-light.toml"),
    ),
];

static BUILTINS: LazyLock<Vec<Theme>> = LazyLock::new(|| {
    BUILTIN_FILES
        .iter()
        .map(|(id, raw)| {
            parse(raw, Path::new(id)).expect("embedded Prismattyc theme must be valid")
        })
        .collect()
});

pub fn default_theme() -> &'static Theme {
    &BUILTINS[0]
}

pub fn builtins() -> &'static [Theme] {
    BUILTINS.as_slice()
}

/// First hyphen-separated token of a theme id (`monokai-spectrum` → `monokai`).
pub fn family_key(id: &str) -> &str {
    id.split('-')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(id)
}

fn family_label(key: &str) -> String {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// One row on the theme-settings surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerItem {
    Theme {
        index: usize,
    },
    Family {
        key: String,
        label: String,
        members: Vec<usize>,
    },
}

/// Root picker rows. Ids that share a family key nest under one family when
/// two or more built-ins share that key (Monokai, later Hive).
pub fn picker_root(themes: &[Theme]) -> Vec<PickerItem> {
    let mut counts = HashMap::<&str, usize>::new();
    for theme in themes {
        *counts.entry(family_key(&theme.id)).or_insert(0) += 1;
    }
    let mut emitted = HashSet::new();
    let mut rows = Vec::new();
    for (index, theme) in themes.iter().enumerate() {
        let key = family_key(&theme.id);
        if counts.get(key).copied().unwrap_or(0) < 2 {
            rows.push(PickerItem::Theme { index });
            continue;
        }
        if !emitted.insert(key.to_string()) {
            continue;
        }
        let members: Vec<usize> = themes
            .iter()
            .enumerate()
            .filter(|(_, candidate)| family_key(&candidate.id) == key)
            .map(|(member, _)| member)
            .collect();
        rows.push(PickerItem::Family {
            key: key.to_string(),
            label: family_label(key),
            members,
        });
    }
    rows
}

impl PickerItem {
    /// Theme index to preview or apply for this row.
    pub fn preview_index(&self, current_id: &str, themes: &[Theme]) -> Option<usize> {
        match self {
            PickerItem::Theme { index } => Some(*index),
            PickerItem::Family { members, .. } => members
                .iter()
                .copied()
                .find(|&index| {
                    themes
                        .get(index)
                        .is_some_and(|theme| theme.id == current_id)
                })
                .or_else(|| members.first().copied()),
        }
    }
}

/// Resolve app-supplied semantic status intent through the active Prismattyc
/// theme. The app never owns an RGB or ANSI index.
pub fn status_rgb(theme: &Theme, tone: prismattyc_protocol::StatusTone) -> Rgb {
    tone.ansi_index()
        .map_or(theme.default_fg, |index| theme.ansi[usize::from(index)])
}

/// Resolve a theme spec for a Prismattyc config file.
///
/// A bare name checks `<config-dir>/themes/` before built-ins, allowing a
/// user palette to override a shipped name. Paths must be absolute so a
/// launch directory cannot change theme resolution.
pub fn load(spec: Option<&str>, config_path: &Path) -> Result<Theme> {
    let Some(spec) = spec.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(default_theme().clone());
    };
    let path = Path::new(spec);
    if path.is_absolute() {
        return load_file(path);
    }
    if spec.contains('/') || spec.contains('\\') {
        bail!("theme path {spec:?} must be absolute");
    }

    if let Some(theme_dir) = config_path.parent().map(|dir| dir.join("themes")) {
        for candidate in user_candidates(&theme_dir, spec) {
            if candidate.is_file() {
                return load_file(&candidate);
            }
        }
    }

    if let Some(theme) = BUILTINS
        .iter()
        .find(|theme| theme.id.eq_ignore_ascii_case(spec) || theme.name.eq_ignore_ascii_case(spec))
    {
        return Ok(theme.clone());
    }

    let names = BUILTINS
        .iter()
        .map(|theme| theme.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("unknown theme {spec:?}; built-ins: {names}")
}

fn user_candidates(theme_dir: &Path, spec: &str) -> [PathBuf; 2] {
    [theme_dir.join(spec), theme_dir.join(format!("{spec}.toml"))]
}

fn load_file(path: &Path) -> Result<Theme> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read theme {}", path.display()))?;
    parse(&raw, path)
}

fn parse(raw: &str, path: &Path) -> Result<Theme> {
    let file: ThemeFile =
        toml::from_str(raw).with_context(|| format!("parse theme {}", path.display()))?;
    anyhow::ensure!(!file.id.trim().is_empty(), "theme id cannot be empty");
    anyhow::ensure!(!file.name.trim().is_empty(), "theme name cannot be empty");
    anyhow::ensure!(
        file.id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
        "theme id {:?} must be a lowercase slug",
        file.id
    );
    anyhow::ensure!(
        file.selection_fg.is_some() == file.selection_bg.is_some(),
        "selection_fg and selection_bg must be provided together"
    );
    anyhow::ensure!(
        file.cursor_fg.is_some() == file.cursor_bg.is_some(),
        "cursor_fg and cursor_bg must be provided together"
    );

    let parse_field = |name: &str, value: &str| {
        parse_hex(value).with_context(|| format!("theme {} field {name}", path.display()))
    };
    let mut ansi = [[0; 3]; 16];
    for (index, value) in file.ansi.iter().enumerate() {
        ansi[index] = parse_field(&format!("ansi[{index}]"), value)?;
    }

    let default_bg = parse_field("default_bg", &file.default_bg)?;
    let chrome_bg = parse_field("chrome_bg", &file.chrome_bg)?;
    let chrome_fg = parse_field("chrome_fg", &file.chrome_fg)?;
    let pane_backdrop = file
        .pane_backdrop
        .as_deref()
        .map(|value| parse_field("pane_backdrop", value))
        .transpose()?
        .unwrap_or_else(|| derived_pane_backdrop(file.variant, default_bg));
    let tab_active_bg_explicit = file.tab_active_bg.is_some();
    let tab_active_bg = file
        .tab_active_bg
        .as_deref()
        .map(|value| parse_field("tab_active_bg", value))
        .transpose()?
        .unwrap_or_else(|| derived_tab_active_bg(file.variant, chrome_bg, chrome_fg));

    Ok(Theme {
        id: file.id,
        name: file.name,
        variant: file.variant,
        source: file.source,
        license: file.license,
        default_fg: parse_field("default_fg", &file.default_fg)?,
        default_bg,
        chrome_fg,
        chrome_bg,
        tab_active_bg,
        tab_active_bg_explicit,
        pane_backdrop,
        pane_border: parse_field("pane_border", &file.pane_border)?,
        overlay_bg: parse_field("overlay_bg", &file.overlay_bg)?,
        unseen_badge: parse_field("unseen_badge", &file.unseen_badge)?,
        mail_letter: parse_field("mail_letter", &file.mail_letter)?,
        active_badge: parse_field("active_badge", &file.active_badge)?,
        attention_badge: file
            .attention_badge
            .as_deref()
            .map(|value| parse_field("attention_badge", value))
            .transpose()?
            .unwrap_or([0xff, 0x6b, 0x6b]),
        cursor_fg: file
            .cursor_fg
            .as_deref()
            .map(|value| parse_field("cursor_fg", value))
            .transpose()?,
        cursor_bg: file
            .cursor_bg
            .as_deref()
            .map(|value| parse_field("cursor_bg", value))
            .transpose()?,
        selection_fg: file
            .selection_fg
            .as_deref()
            .map(|value| parse_field("selection_fg", value))
            .transpose()?,
        selection_bg: file
            .selection_bg
            .as_deref()
            .map(|value| parse_field("selection_bg", value))
            .transpose()?,
        ansi,
    })
}

/// Smallest per-channel distance a derived `pane_backdrop` must keep from
/// `default_bg`. A pane at opacity 0.5 then moves each channel by at least 4.
const MIN_BACKDROP_DELTA: u8 = 8;

/// Only the invariant tests need this now that the backdrop is always a
/// shade of `default_bg` rather than a conditional reuse of `chrome_bg`.
#[cfg(test)]
fn min_channel_delta(a: Rgb, b: Rgb) -> u8 {
    (0..3).map(|i| a[i].abs_diff(b[i])).min().unwrap_or(0)
}

/// Shade `rgb` by `percent`, never by less than [`MIN_BACKDROP_DELTA`].
///
/// Channels darker than the required step move *up* instead of down: a
/// near-black `default_bg` cannot be darkened far enough, and clamping at 0
/// would hand back the theme background itself — the PT-98 no-op.
fn shade(rgb: Rgb, percent: u32) -> Rgb {
    let step = |channel: u8| {
        let scaled = (u32::from(channel) * percent) / 100;
        let delta = scaled.max(u32::from(MIN_BACKDROP_DELTA)) as u8;
        match channel.checked_sub(delta) {
            Some(darker) => darker,
            None => channel.saturating_add(delta),
        }
    };
    [step(rgb[0]), step(rgb[1]), step(rgb[2])]
}

const TAB_ACTIVE_DARK_BLEND: f32 = 0.08;
const TAB_ACTIVE_LIGHT_BLEND: f32 = 0.06;
const MIN_TAB_ACTIVE_DELTA: u8 = 8;
const MAX_TAB_ACTIVE_DELTA: u8 = 40;
const HOVER_LIGHT_SCALE: f32 = 0.8;
const MAX_HOVER_DELTA: u8 = 30;

/// Default active-tab fill: blend `chrome_bg` toward `chrome_fg`.
///
/// Dark themes take 8%; light themes take 6%. The max per-channel delta is
/// then clamped to 8..=40 so the chip stays subtle and still distinct.
pub fn derived_tab_active_bg(variant: ThemeVariant, chrome_bg: Rgb, chrome_fg: Rgb) -> Rgb {
    let t = match variant {
        ThemeVariant::Dark => TAB_ACTIVE_DARK_BLEND,
        ThemeVariant::Light => TAB_ACTIVE_LIGHT_BLEND,
    };
    let mix = |from: u8, toward: u8| {
        (f32::from(from) + (f32::from(toward) - f32::from(from)) * t).round() as u8
    };
    let mut out = [
        mix(chrome_bg[0], chrome_fg[0]),
        mix(chrome_bg[1], chrome_fg[1]),
        mix(chrome_bg[2], chrome_fg[2]),
    ];
    let max_delta = (0..3)
        .map(|i| out[i].abs_diff(chrome_bg[i]))
        .max()
        .unwrap_or(0);
    if max_delta == 0 {
        return shade(chrome_bg, 8);
    }
    let scale = if max_delta < MIN_TAB_ACTIVE_DELTA {
        f32::from(MIN_TAB_ACTIVE_DELTA) / f32::from(max_delta)
    } else if max_delta > MAX_TAB_ACTIVE_DELTA {
        f32::from(MAX_TAB_ACTIVE_DELTA) / f32::from(max_delta)
    } else {
        return out;
    };
    for i in 0..3 {
        let signed = i32::from(out[i]) - i32::from(chrome_bg[i]);
        let scaled = (signed as f32 * scale).round() as i32;
        out[i] = (i32::from(chrome_bg[i]) + scaled).clamp(0, 255) as u8;
    }
    out
}

/// Immediate hover colour composed over `base` (PT-96).
///
/// Dark themes use the configured blend toward `chrome_fg`. Light themes use
/// 80% of it, which gives the specified 8% darkening at the default 0.10.
/// The maximum per-channel delta is capped at 30 so hover stays subtle.
pub fn hover_rgb(variant: ThemeVariant, base: Rgb, chrome_fg: Rgb, blend: f32) -> Rgb {
    let t = blend.clamp(0.0, 0.3)
        * if variant == ThemeVariant::Light {
            HOVER_LIGHT_SCALE
        } else {
            1.0
        };
    let mix = |from: u8, toward: u8| {
        (f32::from(from) + (f32::from(toward) - f32::from(from)) * t).round() as u8
    };
    let mut out = [
        mix(base[0], chrome_fg[0]),
        mix(base[1], chrome_fg[1]),
        mix(base[2], chrome_fg[2]),
    ];
    let max_delta = (0..3).map(|i| out[i].abs_diff(base[i])).max().unwrap_or(0);
    if max_delta > MAX_HOVER_DELTA {
        let scale = f32::from(MAX_HOVER_DELTA) / f32::from(max_delta);
        for i in 0..3 {
            out[i] = (f32::from(base[i]) + (f32::from(out[i]) - f32::from(base[i])) * scale).round()
                as u8;
        }
    }
    out
}

/// Default `pane_backdrop` for a theme that does not declare one.
///
/// Always a slight shade of `default_bg`. It paints the window frame *and*
/// serves as the blend target under a dimmed pane, so it must read as a
/// near-neighbour of the terminal interior — `chrome_bg` is far too dark for
/// that job on most themes.
pub fn derived_pane_backdrop(variant: ThemeVariant, default_bg: Rgb) -> Rgb {
    match variant {
        ThemeVariant::Dark => shade(default_bg, 8),
        ThemeVariant::Light => shade(default_bg, 6),
    }
}

/// `[theme_overrides]` in `config.toml` (PT-207): recolour keys of the named
/// theme without copying a whole theme file. Every key is optional and takes
/// `#RRGGBB`. `ansi` must list all 16 colours.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeOverrides {
    pub default_fg: Option<String>,
    pub default_bg: Option<String>,
    pub chrome_fg: Option<String>,
    pub chrome_bg: Option<String>,
    pub tab_active_bg: Option<String>,
    pub pane_backdrop: Option<String>,
    pub pane_border: Option<String>,
    pub overlay_bg: Option<String>,
    pub unseen_badge: Option<String>,
    pub mail_letter: Option<String>,
    pub active_badge: Option<String>,
    pub attention_badge: Option<String>,
    pub cursor_fg: Option<String>,
    pub cursor_bg: Option<String>,
    pub selection_fg: Option<String>,
    pub selection_bg: Option<String>,
    pub ansi: Option<Vec<String>>,
}

/// Apply `[theme_overrides]` on top of a loaded theme (PT-207). A bad value
/// rejects the config the same way a bad theme file does, so the running
/// host keeps its last good palette.
pub fn apply_overrides(theme: &mut Theme, overrides: &ThemeOverrides) -> Result<()> {
    let field = |name: &str, value: &Option<String>| -> Result<Option<Rgb>> {
        value
            .as_deref()
            .map(|value| parse_hex(value).with_context(|| format!("theme_overrides.{name}")))
            .transpose()
    };
    macro_rules! set {
        ($name:ident) => {
            if let Some(rgb) = field(stringify!($name), &overrides.$name)? {
                theme.$name = rgb;
            }
        };
    }
    set!(default_fg);
    set!(default_bg);
    set!(chrome_fg);
    set!(chrome_bg);
    set!(pane_backdrop);
    set!(pane_border);
    set!(overlay_bg);
    set!(unseen_badge);
    set!(mail_letter);
    set!(active_badge);
    set!(attention_badge);
    if let Some(rgb) = field("tab_active_bg", &overrides.tab_active_bg)? {
        theme.tab_active_bg = rgb;
        theme.tab_active_bg_explicit = true;
    }
    macro_rules! set_opt {
        ($name:ident) => {
            if let Some(rgb) = field(stringify!($name), &overrides.$name)? {
                theme.$name = Some(rgb);
            }
        };
    }
    set_opt!(cursor_fg);
    set_opt!(cursor_bg);
    set_opt!(selection_fg);
    set_opt!(selection_bg);
    anyhow::ensure!(
        theme.cursor_fg.is_some() == theme.cursor_bg.is_some(),
        "theme_overrides: cursor_fg and cursor_bg must be provided together"
    );
    anyhow::ensure!(
        theme.selection_fg.is_some() == theme.selection_bg.is_some(),
        "theme_overrides: selection_fg and selection_bg must be provided together"
    );
    if let Some(list) = &overrides.ansi {
        anyhow::ensure!(
            list.len() == 16,
            "theme_overrides.ansi must list 16 colours, got {}",
            list.len()
        );
        for (index, value) in list.iter().enumerate() {
            theme.ansi[index] =
                parse_hex(value).with_context(|| format!("theme_overrides.ansi[{index}]"))?;
        }
    }
    Ok(())
}

/// `#rrggbb` spelling used by the config template and docs.
pub fn hex(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

fn parse_hex(value: &str) -> Result<Rgb> {
    let digits = value
        .strip_prefix('#')
        .with_context(|| format!("color {value:?} must be #RRGGBB"))?;
    anyhow::ensure!(digits.len() == 6, "color {value:?} must be #RRGGBB");
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&digits[range], 16)
            .with_context(|| format!("color {value:?} must be hexadecimal"))
    };
    Ok([channel(0..2)?, channel(2..4)?, channel(4..6)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn all_embedded_themes_are_valid_and_unique() {
        assert_eq!(builtins().len(), 27);
        for (index, theme) in builtins().iter().enumerate() {
            assert!(theme.ansi.iter().any(|color| color != &theme.ansi[0]));
            assert!(builtins()[..index]
                .iter()
                .all(|prior| prior.id != theme.id && prior.name != theme.name));
        }
    }

    #[test]
    fn every_builtin_has_a_pane_backdrop_distinct_from_default_bg() {
        for theme in builtins() {
            assert!(
                min_channel_delta(theme.pane_backdrop, theme.default_bg) >= MIN_BACKDROP_DELTA,
                "theme {} backdrop {:?} too close to default_bg {:?}",
                theme.id,
                theme.pane_backdrop,
                theme.default_bg
            );
        }
        // Ghost's backdrop is a slight shade of its own default_bg, not the
        // much darker chrome_bg.
        let ghost = load(Some("ghost"), Path::new("/tmp/prism/config.toml")).unwrap();
        assert_ne!(ghost.pane_backdrop, ghost.chrome_bg);
        assert_eq!(ghost.default_bg, [0x28, 0x2c, 0x34]);
        assert_eq!(ghost.pane_backdrop, [0x20, 0x24, 0x2c]);
    }

    #[test]
    fn derived_tab_active_bg_stays_subtle_on_every_builtin() {
        for theme in builtins() {
            let derived = derived_tab_active_bg(theme.variant, theme.chrome_bg, theme.chrome_fg);
            let max_delta = (0..3)
                .map(|i| derived[i].abs_diff(theme.chrome_bg[i]))
                .max()
                .unwrap_or(0);
            assert!(
                (MIN_TAB_ACTIVE_DELTA..=MAX_TAB_ACTIVE_DELTA).contains(&max_delta),
                "theme {} tab_active_bg {:?} vs chrome_bg {:?} max_delta {max_delta}",
                theme.id,
                derived,
                theme.chrome_bg
            );
            if theme.tab_active_bg != derived {
                // File override; skip matching the derived default.
                continue;
            }
            assert_eq!(
                theme.tab_active_bg, derived,
                "theme {} default fill",
                theme.id
            );
        }
    }

    #[test]
    fn hover_rgb_stays_subtle_and_composes_over_active_fill() {
        for theme in builtins() {
            let hovered = hover_rgb(theme.variant, theme.tab_active_bg, theme.chrome_fg, 0.10);
            let max_delta = (0..3)
                .map(|i| hovered[i].abs_diff(theme.tab_active_bg[i]))
                .max()
                .unwrap_or(0);
            assert!(
                (4..=MAX_HOVER_DELTA).contains(&max_delta),
                "theme {} hover {:?} vs active {:?} max_delta {max_delta}",
                theme.id,
                hovered,
                theme.tab_active_bg
            );
        }
    }

    #[test]
    fn theme_file_may_override_tab_active_bg() {
        let dir = std::env::temp_dir().join(format!(
            "prism-tab-active-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, "theme = \"custom\"\n").unwrap();
        let raw = include_str!("../themes/ghost.toml").replace(
            "chrome_bg = \"#1d1f21\"",
            "chrome_bg = \"#1d1f21\"\ntab_active_bg = \"#112233\"",
        );
        std::fs::write(dir.join("themes/custom.toml"), raw).unwrap();
        let loaded = load(Some("custom"), &config).unwrap();
        assert_eq!(loaded.tab_active_bg, [0x11, 0x22, 0x33]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pane_backdrop_falls_back_to_a_darkened_default_bg() {
        // Dark theme whose chrome_bg is nearly its default_bg.
        let flat = derived_pane_backdrop(ThemeVariant::Dark, [0x0c, 0x0c, 0x0c]);
        assert_eq!(flat, [0x04, 0x04, 0x04]);
        // Light themes step 6% darker.
        let light = derived_pane_backdrop(ThemeVariant::Light, [0xf2, 0xed, 0xe4]);
        assert_eq!(light, [0xe4, 0xdf, 0xd7]);
    }

    /// A near-black `default_bg` cannot be darkened by
    /// [`MIN_BACKDROP_DELTA`], so the ground goes lighter instead of
    /// collapsing onto the theme background (which is the PT-98 no-op).
    #[test]
    fn pane_backdrop_lightens_when_default_bg_is_too_dark_to_darken() {
        for default_bg in [[0x00, 0x00, 0x00], [0x04, 0x06, 0x02], [0x07, 0x07, 0x07]] {
            let backdrop = derived_pane_backdrop(ThemeVariant::Dark, default_bg);
            assert!(
                min_channel_delta(backdrop, default_bg) >= MIN_BACKDROP_DELTA,
                "backdrop {backdrop:?} too close to default_bg {default_bg:?}"
            );
        }
        assert_eq!(
            derived_pane_backdrop(ThemeVariant::Dark, [0, 0, 0]),
            [0x08, 0x08, 0x08]
        );
    }

    #[test]
    fn theme_file_may_override_pane_backdrop() {
        let raw = include_str!("../themes/ghost.toml")
            .replace("chrome_bg = ", "pane_backdrop = \"#010203\"\nchrome_bg = ");
        let theme = parse(&raw, Path::new("ghost-override")).unwrap();
        assert_eq!(theme.pane_backdrop, [0x01, 0x02, 0x03]);
    }

    #[test]
    fn picker_root_nests_shared_family_keys() {
        let rows = picker_root(builtins());
        let families: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                PickerItem::Family {
                    key,
                    label,
                    members,
                } => Some((key.as_str(), label.as_str(), members.len())),
                PickerItem::Theme { .. } => None,
            })
            .collect();
        assert_eq!(
            families,
            vec![("monokai", "Monokai", 7), ("hive", "Hive", 13)]
        );
        assert_eq!(
            rows.len(),
            builtins().len() - 6 - 12,
            "Monokai and Hive collapse to one root row each"
        );
        let members = match &rows
            .iter()
            .find(|row| matches!(row, PickerItem::Family { key, .. } if key == "monokai"))
        {
            Some(PickerItem::Family { members, .. }) => members.as_slice(),
            _ => panic!("Monokai family"),
        };
        assert_eq!(builtins()[members[0]].id, "monokai");
        assert_eq!(builtins()[members[1]].id, "monokai-pro");
        assert_eq!(builtins()[members[2]].id, "monokai-spectrum");
        assert_eq!(builtins()[members[6]].id, "monokai-vivid");
        assert!(rows.iter().any(|row| matches!(
            row,
            PickerItem::Theme { index } if builtins()[*index].id == "japanesque"
        )));
        assert!(rows.iter().any(|row| matches!(
            row,
            PickerItem::Theme { index } if builtins()[*index].id == "ghost"
        )));
        assert!(!rows.iter().any(|row| matches!(
            row,
            PickerItem::Theme { index } if builtins()[*index].id == "monokai-spectrum"
        )));
    }

    #[test]
    fn status_tones_resolve_through_each_active_theme() {
        for theme in builtins() {
            let neutral = status_rgb(theme, prismattyc_protocol::StatusTone::Neutral);
            let info = status_rgb(theme, prismattyc_protocol::StatusTone::Info);
            let success = status_rgb(theme, prismattyc_protocol::StatusTone::Success);
            let danger = status_rgb(theme, prismattyc_protocol::StatusTone::Danger);
            let warning = status_rgb(theme, prismattyc_protocol::StatusTone::Warning);
            assert_eq!(neutral, theme.default_fg, "theme {}", theme.id);
            assert_eq!(info, theme.ansi[4], "theme {}", theme.id);
            assert_eq!(success, theme.ansi[2], "theme {}", theme.id);
            assert_eq!(warning, theme.ansi[3], "theme {}", theme.id);
            assert_eq!(danger, theme.ansi[1], "theme {}", theme.id);
            assert_ne!(success, danger, "theme {}", theme.id);
            assert_ne!(warning, theme.default_bg, "theme {}", theme.id);
        }
        assert_ne!(
            status_rgb(&builtins()[0], prismattyc_protocol::StatusTone::Info),
            status_rgb(&builtins()[5], prismattyc_protocol::StatusTone::Info)
        );
    }

    #[test]
    fn builtin_accepts_slug_and_display_name_case_insensitively() {
        let config = Path::new("/tmp/prism/config.toml");
        assert_eq!(
            load(Some("rose-pine-moon"), config).unwrap().name,
            "Rosé Pine Moon"
        );
        assert_eq!(load(Some("DRACULA"), config).unwrap().id, "dracula");
        assert_eq!(load(Some("Tokyo Night"), config).unwrap().id, "tokyo-night");
        assert_eq!(
            load(Some("Monokai Spectrum"), config).unwrap().id,
            "monokai-spectrum"
        );
        assert_eq!(load(Some("Ghost"), config).unwrap().id, "ghost");
        assert_eq!(
            load(Some("Dimmed Monokai"), config).unwrap().id,
            "monokai-dimmed"
        );
        assert_eq!(load(Some("japanesque"), config).unwrap().name, "Japanesque");
        assert_eq!(
            load(Some("Monokai Pro (CE)"), config).unwrap().id,
            "monokai-pro"
        );
        assert_eq!(load(Some("ghost"), config).unwrap().name, "Ghost");
    }

    #[test]
    fn ghost_matches_published_peer_defaults() {
        let theme = load(Some("ghost"), Path::new("/tmp/prism/config.toml")).unwrap();
        assert_eq!(theme.default_fg, [0xff, 0xff, 0xff]);
        assert_eq!(theme.default_bg, [0x28, 0x2c, 0x34]);
        assert_eq!(theme.cursor_bg, Some([0xff, 0xff, 0xff]));
        assert_eq!(theme.cursor_fg, Some([0x28, 0x2c, 0x34]));
        assert_eq!(theme.selection_bg, Some([0xff, 0xff, 0xff]));
        assert_eq!(theme.selection_fg, Some([0x28, 0x2c, 0x34]));
        assert_eq!(
            theme.ansi,
            [
                [0x1d, 0x1f, 0x21],
                [0xcc, 0x66, 0x66],
                [0xb5, 0xbd, 0x68],
                [0xf0, 0xc6, 0x74],
                [0x81, 0xa2, 0xbe],
                [0xb2, 0x94, 0xbb],
                [0x8a, 0xbe, 0xb7],
                [0xc5, 0xc8, 0xc6],
                [0x66, 0x66, 0x66],
                [0xd5, 0x4e, 0x53],
                [0xb9, 0xca, 0x4a],
                [0xe7, 0xc5, 0x47],
                [0x7a, 0xa6, 0xda],
                [0xc3, 0x97, 0xd8],
                [0x70, 0xc0, 0xb1],
                [0xea, 0xea, 0xea],
            ]
        );
    }

    #[test]
    fn relative_paths_and_unknown_names_are_rejected() {
        let config = Path::new("/tmp/prism/config.toml");
        assert!(load(Some("../theme.toml"), config)
            .unwrap_err()
            .to_string()
            .contains("must be absolute"));
        assert!(load(Some("not-a-theme"), config)
            .unwrap_err()
            .to_string()
            .contains("unknown theme"));
    }

    #[test]
    fn legacy_custom_theme_uses_coral_attention_badge_default() {
        let raw = include_str!("../themes/prismattyc-default.toml")
            .lines()
            .filter(|line| !line.starts_with("attention_badge ="))
            .collect::<Vec<_>>()
            .join("\n");
        let theme = parse(&raw, Path::new("legacy-custom"))
            .expect("legacy theme without attention_badge remains valid");
        assert_eq!(theme.attention_badge, [0xff, 0x6b, 0x6b]);
    }

    #[test]
    fn config_local_theme_precedes_builtin() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("prism-theme-test-{nonce}"));
        let themes = root.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        let custom = include_str!("../themes/prismattyc-default.toml")
            .replace("name = \"Prismattyc Default\"", "name = \"User Override\"");
        std::fs::write(themes.join("prismattyc-default.toml"), custom).unwrap();
        let loaded = load(Some("prismattyc-default"), &root.join("config.toml")).unwrap();
        assert_eq!(loaded.name, "User Override");
        std::fs::remove_dir_all(root).unwrap();
    }
}
