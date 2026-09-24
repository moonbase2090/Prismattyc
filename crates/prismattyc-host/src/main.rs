//! Windowed OS host for Prismattyc.
//!
//! Opens its own window — does **not** nest inside Kitty/Ghostty.
//! Classic nested host remains `cargo run -p prismattyc`.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod a11y;
mod attach_adopt;
mod attach_log;
mod attach_tabs;
mod border_underlay;
mod config;
mod config_template;
mod frame_damage;
mod git_info;
#[cfg(feature = "gpu")]
mod gpu;
mod hyperlink;
mod icon;
mod keybind;
mod keys;
mod local_views;
#[cfg(target_os = "macos")]
mod mac_present;
#[cfg(target_os = "macos")]
mod macos_menu;
#[cfg(target_os = "macos")]
mod macos_window;
mod move_target;
mod mux;
mod notify;
mod palette;
mod pane_bell;
mod pixel_alpha;
#[cfg(any(target_os = "macos", test))]
mod present_tiles;
mod rail_resize;
mod raster;
mod regroup;
mod render_diagnostics;
mod restart;
mod terminal_switcher;
#[cfg(test)]
mod test_support;
// cargo-mutants 27.1 does not recognize nested cfg(all(test, ...)).
// Keep cfg(test) separate so mutation targets exclude the test fixture.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod render_window_tests;
mod restore_prompt;
mod rich;
mod session_prompt;
mod space_open;
#[cfg(test)]
#[cfg(target_os = "linux")]
mod space_open_window_tests;
mod space_outcome;
mod space_panel;
mod space_rail;
mod space_view;
mod spaces_polish;
mod splash;
mod system_fonts;
mod theme;
mod title_row;
mod walkthrough;
mod walkthrough_audio;
#[cfg(target_os = "linux")]
mod wayland_shm;

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use frame_damage::{
    append_tab_strip_markers, assignment_changed_content, chrome_geometry_changed,
    compose_frame_damage, composer_promotes_to_full, empty_partial_skips_paint, focus_affects_pane,
    frame_damage_covers, layout_transition, light_cycle_step_for_snapshot, pane_chrome_bits,
    pane_marker_word, pane_paint_required, pulse_live, pulse_phase_if, pulse_step_for_snapshot,
    push_pane_chrome_boxes, push_tab_strip_handle_boxes, scrollbar_marker, should_paint_tab_strip,
    should_record_scrollbar_box, snapshot_dirty_rows, strip_chrome_changed, tab_strip_badge_box,
    tab_strip_inner_stride, tab_strip_visible_for_damage, ChromeSnapshot, FrameDamage,
    LayoutSnapshot, PaneDamageSnapshot, PaneLayoutSnapshot, PixelRect, TabStripHandlePlan,
};
use palette::{
    ContextMenu, ContextMenuKind, ContextMenuVerdict, Palette, PaletteRow, PaletteVerdict,
    SpacePicker, SpacePickerKind, SpacePickerRow, SpacePickerVerdict,
};
use prismattyc_core::{
    encode_osc52_clipboard, Color, GridDamage, HistoryMatch, Screen, ScrollDamage, Selection, Style,
};
use prismattyc_emulator::{CursorShape, Emulator};
use prismattyc_mux::{
    layout_path, load_space, plan, space_bind_agent, spaces_dir, PaneId, SavedSpaceSession,
    WindowId as MuxWindowId,
};
use prismattyc_protocol::{InputModifiers, PointerPhase};
use prismattyc_render::paint_display_row;
use raster::{
    blit_direct_kitty_images, blit_kitty_placeholders, build_background_layer, contrast_ink,
    cycle_focus_border, cycle_focus_border_back, fill_rect_argb, focus_border_name,
    focus_border_rgb, mix_rgb, opacity_to_alpha, opacity_to_weight, pack_argb, palette_hit,
    parse_focus_border, premultiply_in_place, rasterize_bell_toast, rasterize_find_prompt,
    rasterize_footer, rasterize_mail_letter_with_theme, rasterize_overlays_at_with_theme,
    rasterize_palette, rasterize_pane_chrome_with_theme, rasterize_preedit_at,
    rasterize_region_focus_ring, rasterize_render_timer, rasterize_screen_at_with_theme,
    rasterize_screen_at_with_theme_options_filtered, rasterize_scroll_chip, rasterize_scrollbar,
    rasterize_space_rail, rasterize_splash, rasterize_tab_strip_with_theme, rasterize_theme_picker,
    rasterize_walkthrough_caption, scrollbar_layout, scrollbar_scroll_from_thumb_y,
    scrollbar_thumb_y_for_pointer, set_rect_alpha, theme_picker_visible_rows, FontMetrics,
    OverlaySurface, PaletteFrame, PaletteLayout, PaletteSection, ScreenPaint, ScrollbarLayout,
    ThemePickerRow, TitleRowStyle, DEFAULT_FOCUS_BORDER_INDEX, OPAQUE_ALPHA,
    THEME_PICKER_HINT_FAMILY, THEME_PICKER_HINT_ROOT,
};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Window, WindowId};

const MAX_COLS: usize = 512;
const MAX_ROWS: usize = 256;
const FONT_PX: f32 = 15.0;
const MULTI_CLICK_MS: u128 = 500;
const MAX_INITIAL_PANES: usize = 8;
/// No permanent chrome row: the PTY grid owns the full window. Chord help is a
/// temporary bottom **overlay** (does not resize the child) while Ctrl+Shift is
/// held, then lingers so the strip can be read after the keys are released.
const CHROME_OVERLAY_ROWS: usize = 1;
const FOOTER_LINGER: Duration = Duration::from_millis(3000);
/// Cap paste payload before normalize (parity with classic host).
const MAX_PASTE_BYTES: usize = 1024 * 1024;
/// Chunk size for host→child paste enqueue (parity).
const PASTE_CHUNK_BYTES: usize = 4 * 1024;
/// Wall-clock budget for enqueueing one paste (never block forever).
const PASTE_SEND_BUDGET: Duration = Duration::from_millis(250);
/// Extra wait to close a partially-delivered bracketed paste (`CSI 201 ~`).
const PASTE_BRACKET_CLOSE_TIMEOUT: Duration = Duration::from_millis(50);
const PASTE_POLL_INTERVAL: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneSpacing {
    window_padding_px: usize,
    pane_gap_px: usize,
    pane_padding_px: usize,
    /// Spaces rail edge (PT-91); part of the geometry so a config change
    /// reflows through the same path as the paddings.
    space_rail: space_rail::RailSide,
    space_rail_chip_cols: usize,
    space_rail_width_cols: usize,
    space_rail_pane_names: bool,
}

impl From<&config::ConfigFile> for PaneSpacing {
    fn from(config: &config::ConfigFile) -> Self {
        Self {
            window_padding_px: config.window_padding_px(),
            pane_gap_px: config.pane_gap_px(),
            pane_padding_px: config.pane_padding_px(),
            space_rail: config.space_rail(),
            space_rail_chip_cols: config.space_rail_chip_cols(),
            space_rail_width_cols: config.space_rail_width_cols.unwrap_or(18),
            space_rail_pane_names: config.space_rail_pane_names.unwrap_or(true),
        }
    }
}

/// Return the physical IME anchor for a focused pane's terminal cursor.
fn ime_cursor_area(
    geom: mux::HostGeom,
    rect: prismattyc_mux::CellRect,
    guest_y: usize,
    cursor: (usize, usize),
    width_cells: usize,
) -> (usize, usize, usize, usize) {
    let (content_x, _, _, _) = geom.pane_content_px(rect);
    let (row, col) = cursor;
    (
        content_x.saturating_add(col.saturating_mul(geom.cell_w)),
        guest_y.saturating_add(row.saturating_mul(geom.cell_h)),
        geom.cell_w.saturating_mul(width_cells.max(1)),
        geom.cell_h,
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RenderTiming {
    parse_us: u64,
    damage_us: u64,
    raster_us: u64,
    present_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullRepaintReason {
    Resize,
    AltScreen,
    Theme,
    Scrollback,
    Overflow,
    Fallback,
    NoDamage,
}

impl FullRepaintReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Resize => "resize",
            Self::AltScreen => "alt-screen",
            Self::Theme => "theme",
            Self::Scrollback => "scrollback",
            Self::Overflow => "overflow",
            Self::Fallback => "fallback",
            Self::NoDamage => "no-damage",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Resize => 0,
            Self::AltScreen => 1,
            Self::Theme => 2,
            Self::Scrollback => 3,
            Self::Overflow => 4,
            Self::Fallback => 5,
            Self::NoDamage => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RenderFrame {
    timing: RenderTiming,
    cells_painted: u64,
    rows_scrolled_as_blit: u64,
    full_repaint_reason: Option<FullRepaintReason>,
    guards: render_diagnostics::GuardMask,
    raster_at_unix_ms: Option<u64>,
    present_succeeded: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RenderWindowSummary {
    last_raster_us: u64,
    max_raster_us: u64,
    frame_count: u64,
    max_cells_painted: u64,
    blit_sum: u64,
    dominant_full_repaint_reason: Option<FullRepaintReason>,
}

#[derive(Debug, Default)]
struct RenderWindow {
    started_at: Option<Instant>,
    last_raster_us: u64,
    max_raster_us: u64,
    frame_count: u64,
    max_cells_painted: u64,
    blit_sum: u64,
    full_repaint_counts: [u64; 7],
}

impl RenderWindow {
    fn record(&mut self, frame: RenderFrame, now: Instant) -> Option<RenderWindowSummary> {
        let started_at = *self.started_at.get_or_insert(now);
        self.last_raster_us = frame.timing.raster_us;
        self.max_raster_us = self.max_raster_us.max(frame.timing.raster_us);
        self.frame_count = self.frame_count.saturating_add(1);
        self.max_cells_painted = self.max_cells_painted.max(frame.cells_painted);
        self.blit_sum = self.blit_sum.saturating_add(frame.rows_scrolled_as_blit);
        if let Some(reason) = frame.full_repaint_reason {
            self.full_repaint_counts[reason.index()] =
                self.full_repaint_counts[reason.index()].saturating_add(1);
        }
        if now.duration_since(started_at) < Duration::from_secs(1) {
            return None;
        }

        let mut dominant = None;
        let mut dominant_count = 0;
        for (index, count) in self.full_repaint_counts.iter().copied().enumerate() {
            if count > dominant_count {
                dominant_count = count;
                dominant = Some(match index {
                    0 => FullRepaintReason::Resize,
                    1 => FullRepaintReason::AltScreen,
                    2 => FullRepaintReason::Theme,
                    3 => FullRepaintReason::Scrollback,
                    4 => FullRepaintReason::Overflow,
                    5 => FullRepaintReason::Fallback,
                    _ => FullRepaintReason::NoDamage,
                });
            }
        }
        let summary = RenderWindowSummary {
            last_raster_us: self.last_raster_us,
            max_raster_us: self.max_raster_us,
            frame_count: self.frame_count,
            max_cells_painted: self.max_cells_painted,
            blit_sum: self.blit_sum,
            dominant_full_repaint_reason: dominant,
        };
        *self = Self {
            started_at: Some(now),
            ..Self::default()
        };
        Some(summary)
    }
}

struct Cli {
    program: String,
    child_args: Vec<String>,
    panes: usize,
    /// Index into brand spectrum for the focused-pane border.
    focus_border: usize,
    /// True when `--panes` was given: the config file must not override it.
    panes_pinned: bool,
    /// True when the border came from CLI or env: pinned against config, at
    /// startup and across hot reloads.
    focus_border_pinned: bool,
    /// Opt-in Tron light-cycle sweep on focus change (config-only key
    /// `focus_border_animation = "light-cycle"`; default off).
    light_cycle: bool,
    /// Sweep duration in ms (config `focus_border_animation_ms`).
    light_cycle_ms: u128,
    /// Draw the bright vehicle box at the sweep head (config
    /// `focus_border_animation_head`).
    light_cycle_head: bool,
    /// Opt-in rich attachments (APC collect + z1 cell-rect paint).
    experimental_rich: bool,
    /// Opt-in GPU present. Softbuffer stays the default.
    gpu: bool,
    /// Mux sessions to attach (`prismattyc-mux attach --all`).
    /// `session` is the opaque id passed to `prismattyc-mux attach --session-id`.
    /// Host groups these into tabs of panes (not one tab per session).
    attach_sessions: Vec<AttachTarget>,
    /// Suppress the launch splash (`--no-splash`; `PRISMATTYC_NO_SPLASH=1` also
    /// works). Config `splash = false` is the durable opt-out. The splash only
    /// appears on bare launches anyway: an explicit PROGRAM or
    /// `--attach-session` skips it, matching the `prism` CLI.
    no_splash: bool,
    /// Animate the launch splash's word art (config `splash_animation`;
    /// default on). Off shows the art static and arms no frame timer.
    splash_animation: bool,
    /// True when the user named a PROGRAM (or used `--`): not a bare launch.
    explicit_program: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachTarget {
    session: String,
    title: String,
}

impl Cli {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut program = None;
        let mut child_args = Vec::new();
        let mut panes = 1;
        let mut panes_pinned = false;
        let env_focus_border = focus_border_from_env();
        let mut focus_border = env_focus_border.unwrap_or(DEFAULT_FOCUS_BORDER_INDEX);
        let mut focus_border_pinned = env_focus_border.is_some();
        let mut experimental_rich = env_flag_enabled("PRISMATTYC_EXPERIMENTAL_RICH");
        let mut gpu = env_flag_enabled("PRISMATTYC_GPU");
        let mut no_splash = false;
        let mut explicit_program = false;
        let mut attach_sessions = Vec::new();
        while let Some(a) = args.next() {
            if a == "--" {
                // The documented `prismattyc-host -- /bin/bash -l` form uses the
                // first post-separator token as the program, not as an
                // argument to the default shell.
                if program.is_none() {
                    program = args.next();
                    explicit_program = true;
                }
                child_args.extend(args);
                break;
            }
            if a == "-h" || a == "--help" {
                print_help();
                std::process::exit(0);
            }
            if a == "-V" || a == "--version" {
                println!("{}", prismattyc_core::bin_version("prismattyc-host"));
                std::process::exit(0);
            }
            if a == "--write-config" && program.is_none() {
                let mut merge = false;
                let mut path: Option<String> = None;
                for next in args.by_ref() {
                    if next == "--merge" {
                        merge = true;
                    } else if path.is_none() {
                        path = Some(next);
                    } else {
                        bail!("--write-config takes at most one path");
                    }
                }
                let dest = path.as_deref().map(std::path::Path::new);
                config_template::write_config(dest, merge)?;
                std::process::exit(0);
            }
            if a == "--experimental-rich" && program.is_none() {
                experimental_rich = true;
                continue;
            }
            if a == "--gpu" && program.is_none() {
                gpu = true;
                continue;
            }
            if a == "--no-splash" && program.is_none() {
                no_splash = true;
                continue;
            }
            if (a == "--attach-session" || a == "--attach-sessions") && program.is_none() {
                let value = args
                    .next()
                    .context("--attach-session requires a mux session id")?;
                if value.is_empty() {
                    bail!("--attach-session requires a mux session id");
                }
                attach_sessions.push(AttachTarget {
                    session: value.clone(),
                    title: value,
                });
                continue;
            }
            if a == "--attach-title" && program.is_none() {
                let value = args.next().context("--attach-title requires a tab title")?;
                let Some(target) = attach_sessions.last_mut() else {
                    bail!("--attach-title must follow --attach-session");
                };
                target.title = value;
                continue;
            }
            if a == "--panes" && program.is_none() {
                let value = args
                    .next()
                    .context("--panes requires a value from 1 through 8")?;
                panes = value
                    .parse::<usize>()
                    .ok()
                    .filter(|count| (1..=MAX_INITIAL_PANES).contains(count))
                    .context("--panes must be an integer from 1 through 8")?;
                panes_pinned = true;
                continue;
            }
            if (a == "--focus-border" || a == "--focus-color") && program.is_none() {
                let value = args
                    .next()
                    .context("--focus-border requires a name or index")?;
                focus_border = parse_focus_border(&value).with_context(|| {
                    format!(
                        "unknown focus border {value:?}; try coral|amber|yellow|green|blue|violet|ink or 0-6"
                    )
                })?;
                focus_border_pinned = true;
                continue;
            }
            if program.is_none() {
                program = Some(a);
                explicit_program = true;
            } else {
                child_args.push(a);
            }
        }
        // With no explicit program (e.g. launched from Finder/Dock, where the
        // process inherits only launchd's minimal environment), start the
        // user's login shell. `-l` runs ~/.zprofile / ~/.profile so PATH
        // matches Terminal.app instead of the bare launchd PATH; otherwise
        // Homebrew / user tools are missing from the shell.
        let (program, child_args) = match program {
            Some(program) => (program, child_args),
            None => {
                let mut command = prismattyc_mux::platform::default_shell_command();
                let program = command.remove(0);
                (program, command)
            }
        };
        Ok(Self {
            program,
            child_args,
            panes,
            focus_border,
            panes_pinned,
            focus_border_pinned,
            light_cycle: false,
            light_cycle_ms: DEFAULT_LIGHT_CYCLE_MS,
            light_cycle_head: true,
            experimental_rich,
            gpu,
            attach_sessions,
            no_splash,
            splash_animation: true,
            explicit_program,
        })
    }

    /// Fold in the config file wherever the CLI/env did not pin a value.
    fn apply_config(&mut self, file: &config::ConfigFile) {
        if !self.focus_border_pinned {
            if let Some(spec) = file.focus_border.as_deref() {
                match parse_focus_border(spec) {
                    Some(index) => self.focus_border = index,
                    None => {
                        eprintln!("prismattyc-host: config focus_border {spec:?} unknown; ignored")
                    }
                }
            }
        }
        if !self.panes_pinned {
            if let Some(panes) = file.panes {
                self.panes = panes;
            }
        }
        self.light_cycle = matches!(file.focus_border_animation.as_deref(), Some("light-cycle"));
        self.light_cycle_ms = file
            .focus_border_animation_ms
            .map_or(DEFAULT_LIGHT_CYCLE_MS, u128::from);
        self.light_cycle_head = file.focus_border_animation_head.unwrap_or(true);
        self.splash_animation = file.splash_animation.unwrap_or(true);
    }
}

fn focus_border_from_env() -> Option<usize> {
    let raw = std::env::var("PRISMATTYC_FOCUS_BORDER")
        .or_else(|_| std::env::var("PRISMATTYC_FOCUS_COLOR"))
        .ok()?;
    parse_focus_border(&raw)
}

/// Locate `pmux` for host actions.
///
/// Finder-launched macOS apps do not inherit the shell's PATH. The app
/// bundle carries the mux CLI, and local development installs can also use
/// the standard user locations before falling back to PATH.
fn find_mux_bin() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("PMUX") {
        if !path.is_empty() {
            return std::path::PathBuf::from(path);
        }
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let sibling = dir.join(prismattyc_mux::platform::executable_name("pmux"));
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut candidates = Vec::new();
        if let Ok(cargo_home) = std::env::var("CARGO_HOME") {
            if !cargo_home.is_empty() {
                candidates.push(std::path::PathBuf::from(cargo_home).join("bin/pmux"));
            }
        } else if let Ok(home) = std::env::var("HOME") {
            candidates.push(std::path::PathBuf::from(home).join(".cargo/bin/pmux"));
        }
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(std::path::PathBuf::from(home).join(".local/bin/pmux"));
        }
        candidates.extend([
            std::path::PathBuf::from("/opt/homebrew/bin/pmux"),
            std::path::PathBuf::from("/usr/local/bin/pmux"),
        ]);
        if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
            return path;
        }
    }
    std::path::PathBuf::from("pmux")
}

fn attach_session_args(target: &AttachTarget) -> Vec<String> {
    vec![
        "attach".into(),
        "--session-id".into(),
        target.session.clone(),
    ]
}

fn attach_boot_command(
    sessions: &[AttachTarget],
    cli: &Cli,
    groups: &[(String, Vec<AttachTarget>)],
) -> (String, Vec<String>) {
    match groups
        .first()
        .and_then(|(_, members)| members.first())
        .or(sessions.first())
    {
        Some(target) => (
            find_mux_bin().to_string_lossy().into_owned(),
            attach_session_args(target),
        ),
        None => (cli.program.clone(), cli.child_args.clone()),
    }
}

/// Open one tab per group (the boot pane is the first member of the first
/// group) and return which mux session each pane attached.
fn open_attach_session_tabs(
    mux: &mut mux::MuxRuntime,
    groups: &[(String, Vec<AttachTarget>)],
) -> Result<Vec<(PaneId, String)>> {
    let mux_bin = find_mux_bin();
    let mux_bin = mux_bin.to_string_lossy();
    let mut pane_sessions = Vec::new();
    for (index, (title, members)) in groups.iter().enumerate() {
        let start = if index == 0 {
            mux.rename_window(mux.active_window(), title)?;
            let pane = mux.focused_id();
            mux.mark_attach_session(pane, members[0].session.clone(), members[0].title.clone());
            pane_sessions.push((pane, members[0].session.clone()));
            1
        } else {
            let first = &members[0];
            let window = mux.new_tab(&mux_bin, &attach_session_args(first))?;
            mux.rename_window(window, title)?;
            let pane = mux.focused_id();
            mux.mark_attach_session(pane, first.session.clone(), first.title.clone());
            pane_sessions.push((pane, first.session.clone()));
            1
        };
        for target in members.iter().skip(start) {
            let pane = mux.split_focused(
                &mux_bin,
                &attach_session_args(target),
                prismattyc_mux::Axis::Horizontal,
                0.5,
            )?;
            mux.mark_attach_session(pane, target.session.clone(), target.title.clone());
            pane_sessions.push((pane, target.session.clone()));
        }
        let count = mux.active_pane_count();
        if count > 1 {
            mux.ensure_even_columns(&mux_bin, &[], count)?;
        }
    }
    Ok(pane_sessions)
}

fn seed_attach_focus(
    mux: &mut mux::MuxRuntime,
    grouped: &attach_tabs::AttachGroups,
    pane_sessions: &[(PaneId, String)],
) {
    let tab = grouped.active_tab;
    let want = grouped.focused_session.as_deref().or_else(|| {
        grouped
            .groups
            .get(tab)
            .and_then(|(_, members)| members.first().map(|target| target.session.as_str()))
    });
    let pane = want.and_then(|id| {
        pane_sessions
            .iter()
            .find_map(|(pane, session)| (session == id).then_some(*pane))
    });
    let _ = mux.seed_tab_and_focus(tab, pane);
}

fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Nested-prism parity: chip/title default on; `0`/`false`/`off` disables.
fn env_flag_enabled_default_true(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

fn print_help() {
    eprintln!(
        "\
prismattyc-host — windowed Prismattyc host

USAGE:
    prismattyc-host [-V|--version] [--panes N] [--focus-border NAME] [--experimental-rich] [--gpu] [PROGRAM [ARGS...]]
    prismattyc-host --attach-session ID [--attach-title NAME] ...
    prismattyc-host --write-config [PATH] [--merge]
    prismattyc-host -- /bin/bash -l
    prismattyc-host --panes 3 --focus-border violet -- /bin/bash -l

`--attach-session ID` attaches that mux session (`pmux attach --session-id ID`).
`--attach-title NAME` names the session's tab. Tab grouping comes from
`{{stem}}.attach-tabs.json` next to the mux socket (host cache; see
docs/mux-cli.md). Prefer `pmux attach --all`. Do not comma-split a session id.

Opens an OS window (no outer terminal emulator required).
Default program: $SHELL as a login shell (-l), so ~/.zprofile / ~/.profile
set PATH the same as Terminal.app (important when launched from the Dock).
Direct mux keys: Ctrl+Shift+\\ or Ctrl+Shift+E  split right;
                 Ctrl+Shift+- or Ctrl+Shift+D   split down;
                 Ctrl+Shift+F2 even two-column layout;
                 Ctrl+Shift+F3 even three-column layout;
                 Ctrl+Shift+F4 even 2×2 quadrants;
                 Ctrl+Shift+F5–F9 even n-column layout;
                 macOS: Control-F2…F8 and Cmd+Shift+F<n> are system shortcuts
                 (accessibility, Mission Control). Use Ctrl+Alt+2…9.
                 Ctrl+Shift+1…9 still select tabs.
                 Ctrl+Shift+W close pane; Ctrl+Shift+X detach session;
                 Alt+Arrow focus pane;
                 Ctrl+Shift+] / Ctrl+Shift+[ cycle focus border color
                 forward / back (brand spectrum).
                 Ctrl+Shift+, open theme settings (preview + apply).
                 Super+N (Linux) / Cmd+N (macOS) open a new OS window.
Paste:           Ctrl+Shift+V or Shift+Insert (not plain Ctrl+V).
Scroll chrome:   bottom-right `N/M` chip + window title while in history
                 view (PRISMATTYC_SCROLL_CHIP=0 / PRISMATTYC_SCROLL_TITLE=0 opt out).
Focus border:    coral amber yellow green blue violet ink
                 (also PRISMATTYC_FOCUS_BORDER=name|index)
Config file:     ~/.config/prismattyc/config.toml (or $PRISMATTYC_CONFIG), hot-reloaded.
                 First run writes the full template when the file is missing.
                 --write-config [PATH] prints (PATH omitted or -) or writes it;
                 --write-config --merge appends missing keys to an existing file.
                 CLI flags and PRISMATTYC_* env vars always win over the file.
Config keys:
Rich attach:     --experimental-rich or PRISMATTYC_EXPERIMENTAL_RICH=1
GPU present:     --gpu or PRISMATTYC_GPU=1 (needs --features gpu; else errors)
Launch splash:   shown on bare launches; --no-splash, PRISMATTYC_NO_SPLASH=1,
                 or config splash = false opts out. An explicit PROGRAM or
                 --attach-session skips it. Classic prismattyc has no config
                 file; use the flag or env there.
Nested classic claim host: cargo run -p prismattyc -- /bin/sh

See docs/rendering.md.
"
    );
    for line in config_template::help_lines() {
        eprintln!("{line}");
    }
    // Generated from the same table the dispatcher uses (keybindings), so the
    // list cannot drift from what the keys do.
    let keymap = keybind::KeyMap::default();
    eprintln!(
        "Key actions ([keys] in config.toml; value = \"chord\" or [\"chord\", ...];\n\
         chord = mod+...+key with ctrl/shift/alt/super; see docs/config.md):"
    );
    for action in keybind::Action::all() {
        let chords = keymap.spellings(action).join(", ");
        let chords = if chords.is_empty() {
            "(unbound)".to_string()
        } else {
            chords
        };
        eprintln!(
            "    {:<20} {:<12} {:<44} {}",
            action.name(),
            action.group().label(),
            chords,
            action.describe()
        );
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Preedit {
    text: String,
    /// UTF-8 byte range selected by the IME inside `text`.
    cursor: Option<(usize, usize)>,
}

fn ime_action(event: &Ime, preedit: &mut Preedit) -> Option<Vec<u8>> {
    match event {
        Ime::Enabled => {
            *preedit = Preedit::default();
            None
        }
        Ime::Preedit(text, cursor) => {
            preedit.text.clone_from(text);
            preedit.cursor = *cursor;
            None
        }
        Ime::Commit(text) => {
            preedit.text.clear();
            preedit.cursor = None;
            Some(text.as_bytes().to_vec())
        }
        Ime::Disabled => {
            *preedit = Preedit::default();
            None
        }
    }
}

/// A non-empty IME preedit owns the pressed keys until the IME commits it.
fn ime_blocks_host_keyboard(preedit: &Preedit) -> bool {
    !preedit.text.is_empty()
}

struct HostState {
    window: Arc<Window>,
    present: Option<PresentBackend>,
    font: FontMetrics,
    /// Host-only terminal-grid OpenType shaping settings.
    font_ligatures: bool,
    font_features: Vec<String>,
    render_timer: config::RenderTimer,
    render_timer_log_every_frame: bool,
    render_frame: RenderFrame,
    render_window: RenderWindow,
    render_osd: RenderWindowSummary,
    pending_full_repaint: Option<FullRepaintReason>,
    /// Damage taken from panes for the current raster pass (PT-243).
    pane_damage: HashMap<PaneId, GridDamage>,
    /// Last alternate-screen and scrollback state observed for each pane.
    /// Transitions require a full repaint; steady views can use partial raster.
    last_pane_views: HashMap<PaneId, (bool, usize)>,
    /// Cursor row last committed to the retained framebuffer for each pane.
    /// Scroll blits repaint its moved destination to avoid a ghost caret.
    last_painted_cursor_rows: HashMap<PaneId, Option<usize>>,
    /// Last pane geometry used by the pure PT-289 damage composer.
    last_layout_snapshot: Option<LayoutSnapshot>,
    /// Last known bounded chrome model used by the pure PT-289 composer.
    last_chrome_snapshot: Option<ChromeSnapshot>,
    /// Whether a transient overlay was visible in the previous raster pass.
    /// A closed overlay needs one full frame to clear its old pixels.
    last_transient_overlay_visible: bool,
    last_frame_size: Option<(u32, u32)>,
    last_render_log: Option<Instant>,
    mux: mux::MuxRuntime,
    local_views: local_views::Views,
    rail_resizing: bool,
    terminal_targets: Option<Vec<terminal_switcher::Entry>>,
    terminal_messages: bool,
    move_target: Option<move_target::Target>,
    modifiers: ModifiersState,
    cursor_cell: Option<(PaneId, usize, usize)>,
    left_button_down: bool,
    /// Swallow the matching left-release after a consumed press (URL open,
    /// bell-toast dismiss).
    suppress_left_release: bool,
    /// Last caption-control press. A bounce or double-click at the same
    /// pixel must not fire Show me then Skip after the caption advances.
    caption_click: Option<(Instant, usize, usize)>,
    rich_pointer: Option<RichPointerGesture>,
    /// Button currently owned by child application mouse tracking.
    app_mouse_button: Option<u8>,
    /// Last reported application-mouse cell; pane-aware so focus changes do not deduplicate.
    last_app_mouse_cell: Option<(PaneId, usize, usize)>,
    multi_click: MultiClick,
    /// Kept alive for Linux selection ownership; see arboard's X11/Wayland contract.
    clipboard: Option<arboard::Clipboard>,
    /// Terminal defaults, ANSI 0-15, and host chrome colors. The brand focus
    /// spectrum remains independent of the selected theme.
    theme: theme::Theme,
    theme_picker: Option<ThemePicker>,
    palette: Option<Palette>,
    /// Last painted palette / space-picker list geometry (PT-201).
    palette_layout: Option<PaletteLayout>,
    /// RECENT section source, most recent first (PT-92).
    palette_recent: Vec<keybind::Action>,
    /// `$XDG_DATA_HOME/prismattyc/palette-recent.json`, when resolvable.
    palette_recent_path: Option<PathBuf>,
    space_picker: Option<SpacePicker>,
    /// Shared right-click menu overlay for spaces and panes (PT-214/PT-215).
    context_menu: Option<ContextMenu>,
    space_panel: Option<space_panel::Panel>,
    space_polish: spaces_polish::State,
    space_team_focus: Option<(String, u64, Instant)>,
    team_attention_feed: space_panel::AttentionFeed,
    context_menu_target: Option<ContextMenuTarget>,
    /// Saved-spaces rail (PT-91): chip list, current space, keyboard mode.
    space_rail: space_rail::SpaceRail,
    /// Host-rendered text from the active input method. It is never sent to the PTY.
    preedit: Preedit,
    /// Incremental find over the focused pane's history (PT-37).
    find: FindMode,
    /// Launch splash overlay (first window of a bare launch). While shown
    /// it owns all input and paints above everything; the session runs
    /// underneath, untouched, until the splash is dismissed.
    splash: Option<splash::Splash>,
    restore_prompt: Option<restore_prompt::RestorePrompt>,
    session_prompt: Option<session_prompt::Prompt>,
    /// Job-only: `(start, deadline_ms)` for one Enter through `dispatch_splash_key`.
    e2e_dismiss_at: Option<(Instant, u64)>,
    /// Job-only: path captured once at window create from `PRISMATTYC_DUMP_PRESENT`.
    dump_present: Option<PathBuf>,
    dump_present_seq: u64,
    /// Job-only: one extra dirty after e2e dismiss so two post-dismiss seqs exist.
    e2e_second_dump_at: Option<Instant>,
    /// In-memory walkthrough (PT-193). XDG progress is PT-195.
    walkthrough: Option<walkthrough::WalkthroughLive>,
    /// Effective key table for labels (the dispatcher reads `App::keymap`).
    keymap: Arc<keybind::KeyMap>,
    /// Whether the experimental rich action is enabled for palette rows.
    experimental_rich: bool,
    /// Brand spectrum index for the focused-pane border (thin, 1px).
    focus_border: usize,
    /// Configured tab-strip visibility mode.
    tab_strip_mode: config::TabStripMode,
    /// Multi-pane title row: focused pane OSC title, or handle hover only.
    pane_titles: config::PaneTitlesMode,
    /// Configured pane spacing. The inter-pane gap is activated only for
    /// multi-pane layouts when converted to `HostGeom`.
    spacing: PaneSpacing,
    /// Shared origin for the active-dot pulse; all panes breathe in sync.
    pulse_epoch: Instant,
    /// Last quantized pulse step painted; repaints only on step changes so an
    /// active-but-quiet pane animates at ~PULSE_STEPS fps, not the poll rate.
    last_pulse_step: u8,
    /// Window focus, tracked so decorative animation pauses when the window is
    /// in the background. Agent panes are active around the clock, so the dot
    /// pulse is otherwise a PERMANENT ~16 repaints/second — measured at 25-46%
    /// of a core (plus the compositor's share) for a window nobody was looking
    /// at. PTY output still repaints as before; only the idle breathing stops.
    window_focused: bool,
    /// Compositor-reported occlusion (`WindowEvent::Occluded`); same gate for
    /// a window that is visible-stack-wise focused but fully covered. Not all
    /// backends deliver it — absence just means the focus gate does the work.
    window_occluded: bool,
    /// Opt-in: sweep the focus border like a Tron light cycle on focus change.
    light_cycle: bool,
    /// Sweep duration; user-tunable via `focus_border_animation_ms`.
    light_cycle_ms: u128,
    /// Whether the bright vehicle box rides the sweep's leading edge.
    light_cycle_head: bool,
    /// Focused pane at the last paint; a difference starts the sweep.
    last_focused: PaneId,
    /// Sweep start time while a light-cycle animation is running.
    border_anim: Option<Instant>,
    /// Pixels beneath the current animated border; empty outside a sweep.
    border_underlay: border_underlay::BorderUnderlay,
    /// Last quantized sweep step painted (same repaint-throttle idea as the
    /// pulse dot).
    last_cycle_step: u8,
    /// Config `visual_bell` (default true): invert the frame briefly on BEL.
    visual_bell: bool,
    pane_visual_bell: bool,
    pane_bells: pane_bell::PaneBells,
    /// Config `audible_bell` (default true): play a short bell cue on BEL.
    audible_bell: bool,
    /// Config `walkthrough_audio` (default true): play bundled walkthrough clips.
    walkthrough_audio: bool,
    /// Last walkthrough clip play; enforces the one-second gap.
    last_walkthrough_sound: Option<Instant>,
    /// Config `bell_toaster` (default true): toast the pane that rang BEL.
    bell_toaster: bool,
    /// Config `bell_toaster_ms` (default 10s): toast linger.
    bell_toaster_ms: Duration,
    /// Live bell toasts, one per pane; a re-ring extends the linger.
    bell_toasts: Vec<BellToast>,
    /// Config `drag_toaster` (default true): "Moving tab NAME → …" chip
    /// while a strip drag is in progress (PT-79).
    drag_toaster: bool,
    /// Config `os_notify_bell` (default false): OS notification on BEL while
    /// the window is unfocused.
    os_notify_bell: bool,
    /// Config `attention_sound` (default true): play the attention cue.
    attention_sound: bool,
    /// Config `attention_badge` (default true): draw attention tab badges.
    attention_badge: bool,
    /// Config `os_notify_attention` (default true): notify when attention is
    /// not on the selected tab or the window is unfocused.
    os_notify_attention: bool,
    /// Last attention notification time per pane.
    last_attention_notify: Vec<(PaneId, Instant)>,
    /// Flash start while the visual bell is lit.
    bell_flash: Option<Instant>,
    /// Last OS bell notification; throttles a BEL storm.
    last_bell_notify: Option<Instant>,
    /// Last bell cue played; a BEL storm must not machine-gun audio.
    last_bell_sound: Option<Instant>,
    /// Latest config parse/validation failure; shown as a coral footer bar
    /// until a valid save clears it.
    config_error: Option<String>,
    /// Resolved config path shown in the dedicated config editor window.
    config_path: Option<PathBuf>,
    /// Keep the Ctrl+Shift chord strip visible until this instant (inclusive).
    footer_until: Option<Instant>,
    dirty: bool,
    /// Attach-tabs cache needs a rewrite; flushed in `about_to_wait`.
    layout_dirty: bool,
    tab_rename: Option<TabRename>,
    pointer_px: Option<(f64, f64)>,
    /// Interactive chrome element under the pointer. Changes, rather than raw
    /// pointer motion, invalidate the frame (PT-96).
    hover_target: Option<HoverTarget>,
    hyperlink_hover: Option<(HyperlinkHoverKey, bool)>,
    /// Configured immediate hover blend (0.0-0.3).
    hover_blend: f32,
    /// Drag state for the host scrollback scrollbar (PT-40).
    scrollbar_drag: Option<ScrollbarDrag>,
    /// Tab-strip drag (PT-69). None when the pointer is not capturing the strip.
    strip_drag: Option<StripDrag>,
    /// Divider drag (PT-133): the split whose ratio follows the pointer.
    divider_drag: Option<mux::Divider>,
    /// A resize cursor is showing (over a divider or while dragging one).
    divider_cursor: bool,
    /// Saved `attach --all` tab titles. None when not attaching.
    attach_layout: Option<attach_tabs::AttachTabsFile>,
    attach_layout_path: Option<PathBuf>,
    /// Attach pane → mux session id, so tab membership can be re-derived
    /// from the live tabs after a pane moves (PT-60).
    attach_pane_sessions: HashMap<PaneId, String>,
    /// Panes whose local shell runs a nested `pmux-attach` (PT-210).
    adopted: attach_adopt::Adopted,
    /// Serialize helpers and fence cache writes until their layout applies.
    space_opens: space_open::Opens,
    space_open_observation: Option<space_outcome::Observation>,
    last_space_open: Option<space_outcome::Report>,
    last_space_refresh: Option<Instant>,
    observed_space_sessions: std::collections::HashSet<String>,
    /// Last attach-tabs stamp we already handled (mtime, len).
    attach_cache_stamp: Option<(SystemTime, u64)>,
    /// Stamp of a cache write this host made; the poll ignores it.
    attach_own_stamp: Option<(SystemTime, u64)>,
    /// This window may persist its view file. Only the registered default
    /// window uses the global CLI target; other windows use private paths.
    cache_writer: bool,
    /// Decoded PNG bytes for the optional window background.
    background_png: Option<Vec<u8>>,
    background_opacity: f32,
    background_blur_px: u32,
    pane_opacity_active: f32,
    overlay_opacity: f32,
    pane_opacity_inactive: f32,
    background: Option<BgCache>,
    /// Straight alpha of the window ground. `255` unless `window_opacity` < 1
    /// *and* the present path carries alpha (see [`PresentBackend::carries_alpha`]).
    window_alpha: u8,
    /// Straight alpha of the chrome bars (tab strip, footer rail).
    chrome_alpha: u8,
    /// Was the window created with an alpha visual? Decided once, at creation.
    /// The alpha *bytes* above can return to `255` when the config asks for
    /// full opacity, so they cannot stand in for this.
    alpha_visual: bool,
    /// Whether the native macOS backdrop is active.
    #[cfg(target_os = "macos")]
    window_blur_active: bool,
    /// AccessKit adapter. `None` when `[a11y] os_tree = false`.
    a11y: Option<accesskit_winit::Adapter>,
    /// `[a11y] announce`. Default on. Hot-reloaded.
    a11y_announce: bool,
    announce_memory: a11y::AnnounceMemory,
    pending_mail_announce: Option<String>,
    pending_attention_announce: Option<String>,
    pending_selection_announce: Option<String>,
    pending_title_announce: Option<String>,
    last_mail_depths: BTreeMap<u64, u32>,
    /// Previous multi-pane `handle_titles` snapshot (PT-190 notice diff).
    last_handle_titles: Vec<Vec<String>>,
    /// Unfocused-pane title notice linger.
    title_notice: Option<LiveTitleNotice>,
}

struct LiveTitleNotice {
    tab: usize,
    handle: usize,
    title: String,
    until: Instant,
}

struct BgCache {
    w: u32,
    h: u32,
    px: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackgroundCacheMeta {
    width: u32,
    height: u32,
    pixel_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundDecision {
    Rebuild,
    Copy { rewrite_alpha: bool },
    Fill,
}

fn background_decision(
    cache: Option<BackgroundCacheMeta>,
    width: u32,
    height: u32,
    has_png: bool,
    rewrite_alpha: bool,
) -> BackgroundDecision {
    match cache {
        Some(cache) if cache.width != width || cache.height != height => {
            if has_png {
                BackgroundDecision::Rebuild
            } else {
                BackgroundDecision::Fill
            }
        }
        Some(cache) if cache.pixel_len == width as usize * height as usize => {
            BackgroundDecision::Copy { rewrite_alpha }
        }
        Some(_) => BackgroundDecision::Fill,
        None if has_png => BackgroundDecision::Rebuild,
        None => BackgroundDecision::Fill,
    }
}

/// Chord strip is visible while Ctrl+Shift is held, or while `now` is
/// strictly before the linger deadline. The `<` check matches the
/// `rasterize_frame` predicate (the wait loop clears at `now >= until`).
fn footer_visibility(
    control: bool,
    shift: bool,
    linger_until: Option<Instant>,
    now: Instant,
) -> bool {
    (control && shift) || linger_until.is_some_and(|until| now < until)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOverlay {
    None,
    Cursor,
    Preedit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayPaintContext {
    pane_focused: bool,
    live_view: bool,
    cursor_visible: bool,
    preedit_present: bool,
    ime_modal: bool,
    footer_visible: bool,
    scroll_chip_enabled: bool,
    find_active: bool,
    palette_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayPaintDecision {
    input: InputOverlay,
    paint_ime_cursor_area: bool,
    paint_scroll_chip: bool,
    paint_find_prompt: bool,
    paint_palette: bool,
    footer_visible: bool,
}

/// Decide the per-pane input and independent overlay paint gates.
///
/// `live_view` must mirror the rasterizer's `scroll == 0` rule. Keep window,
/// mux, environment, and framebuffer effects at the call site.
fn overlay_paint_decision(context: OverlayPaintContext) -> OverlayPaintDecision {
    let input = if !context.pane_focused || !context.live_view {
        InputOverlay::None
    } else if context.preedit_present {
        if context.ime_modal {
            InputOverlay::None
        } else {
            InputOverlay::Preedit
        }
    } else if context.cursor_visible {
        InputOverlay::Cursor
    } else {
        InputOverlay::None
    };

    OverlayPaintDecision {
        input,
        paint_ime_cursor_area: context.pane_focused && context.live_view && !context.ime_modal,
        paint_scroll_chip: !context.live_view && context.scroll_chip_enabled,
        paint_find_prompt: context.pane_focused && context.find_active,
        paint_palette: context.palette_active,
        footer_visible: context.footer_visible,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClipRect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// Intersect `overlay` with the pane box after a bottom footer reserve.
/// When `pane.y + pane.h + footer_h` exceeds `window_h`, the usable pane
/// height is `pane.h - footer_h` (same predicate as `rasterize_frame`).
fn overlay_clip(
    pane: ClipRect,
    footer_rows: usize,
    cell_h: usize,
    window_h: usize,
    overlay: ClipRect,
) -> Option<ClipRect> {
    let footer_h = footer_rows.saturating_mul(cell_h);
    let usable_h = if pane.y.saturating_add(pane.h).saturating_add(footer_h) > window_h {
        pane.h.saturating_sub(footer_h)
    } else {
        pane.h
    };
    let usable = ClipRect {
        x: pane.x,
        y: pane.y,
        w: pane.w,
        h: usable_h,
    };
    intersect_clip(overlay, usable)
}

fn intersect_clip(a: ClipRect, b: ClipRect) -> Option<ClipRect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = a.x.saturating_add(a.w).min(b.x.saturating_add(b.w));
    let y1 = a.y.saturating_add(a.h).min(b.y.saturating_add(b.h));
    if x0 < x1 && y0 < y1 {
        Some(ClipRect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarDrag {
    pane: PaneId,
    grab_off: i32,
    max_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripDragKind {
    Tab(usize),
    Pane {
        tab: usize,
        pane: prismattyc_mux::PaneId,
    },
}

#[derive(Debug, Clone, Copy)]
struct StripDrag {
    kind: StripDragKind,
    start_x: f64,
    start_y: f64,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverTarget {
    Caption(Option<walkthrough::CaptionHit>),
    Strip(mux::StripHit),
    Rail(space_rail::RailHit),
    ScrollbarThumb(PaneId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HyperlinkHoverKey {
    pane: PaneId,
    row: usize,
    col: usize,
    scroll: usize,
    epoch: u64,
    size: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuTarget {
    SpaceChip(usize),
    Pane(PaneId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpaceContextAction {
    Details,
    Settings,
    Undo,
    Open(SpaceOpenMode),
    Save,
    AddSession,
    Rename,
    Move,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneContextAction {
    SplitRight,
    SplitDown,
    Zoom,
    Rename,
    MovePaneNextTab,
    MoveToSpace,
    MoveSessionToSpace,
    RemoveSessionFromSpace,
    RemoveAndKillSession,
    Detach,
    Close,
    SaveSpace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuAction {
    Space {
        chip: usize,
        action: SpaceContextAction,
    },
    Pane {
        pane: PaneId,
        action: PaneContextAction,
    },
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuChoice {
    Space(SpaceContextAction),
    Pane(PaneContextAction),
    Noop,
}

fn context_menu_choice(kind: ContextMenuKind, index: usize) -> ContextMenuChoice {
    match kind {
        ContextMenuKind::SpaceChip => {
            let action = match index {
                0 => SpaceContextAction::Open(SpaceOpenMode::Switch),
                1 => SpaceContextAction::AddSession,
                2 => SpaceContextAction::Open(SpaceOpenMode::NewWindow),
                3 => SpaceContextAction::Save,
                4 => SpaceContextAction::Rename,
                5 => SpaceContextAction::Move,
                6 => SpaceContextAction::Delete,
                7 => SpaceContextAction::Details,
                8 => SpaceContextAction::Settings,
                9 => SpaceContextAction::Undo,
                _ => return ContextMenuChoice::Noop,
            };
            ContextMenuChoice::Space(action)
        }
        ContextMenuKind::Pane => {
            let action = match index {
                0 => PaneContextAction::SplitRight,
                1 => PaneContextAction::SplitDown,
                2 => PaneContextAction::Zoom,
                3 => PaneContextAction::MovePaneNextTab,
                4 => PaneContextAction::MoveToSpace,
                5 => PaneContextAction::Close,
                6 => PaneContextAction::Rename,
                7 => PaneContextAction::Detach,
                8 => PaneContextAction::SaveSpace,
                9 => PaneContextAction::MoveSessionToSpace,
                10 => PaneContextAction::RemoveSessionFromSpace,
                11 => PaneContextAction::RemoveAndKillSession,
                _ => return ContextMenuChoice::Noop,
            };
            ContextMenuChoice::Pane(action)
        }
    }
}

fn context_menu_action(target: ContextMenuTarget, index: usize) -> ContextMenuAction {
    match target {
        ContextMenuTarget::SpaceChip(chip) => {
            match context_menu_choice(ContextMenuKind::SpaceChip, index) {
                ContextMenuChoice::Space(action) => ContextMenuAction::Space { chip, action },
                ContextMenuChoice::Pane(_) | ContextMenuChoice::Noop => ContextMenuAction::Noop,
            }
        }
        ContextMenuTarget::Pane(pane) => match context_menu_choice(ContextMenuKind::Pane, index) {
            ContextMenuChoice::Pane(action) => ContextMenuAction::Pane { pane, action },
            ContextMenuChoice::Space(_) | ContextMenuChoice::Noop => ContextMenuAction::Noop,
        },
    }
}

fn context_menu_needs_confirmation(kind: ContextMenuKind, index: usize, confirmed: bool) -> bool {
    !confirmed
        && matches!(
            (kind, index),
            (ContextMenuKind::SpaceChip, 3 | 6) | (ContextMenuKind::Pane, 11)
        )
}

#[derive(Debug, Clone, Copy)]
struct RichPointerGesture {
    pane: PaneId,
    hit: rich::WorkspaceHit,
    start_x: f64,
    start_y: f64,
    cancelled: bool,
}

/// Present path for the CPU `u32` framebuffer. macOS uses an alpha-capable
/// Core Animation layer. Other platforms default to softbuffer;
/// on native Wayland with transparency configured, our own `wl_shm`
/// ARGB8888 path (PT-118) takes over because softbuffer presents XRGB
/// there; wgpu is opt-in (`--features gpu` + `--gpu`) and falls back to
/// softbuffer on init failure.
enum PresentBackend {
    #[cfg(target_os = "macos")]
    Mac(Box<mac_present::MacPresent>),
    Softbuffer {
        /// Kept alive for `surface` (softbuffer requires context outlive use).
        _context: softbuffer::Context<Arc<Window>>,
        surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
        /// True when softbuffer presents through native Wayland.
        wayland: bool,
        // Inject one error through App::paint without replacing its backend.
        #[cfg(test)]
        fail_paint: bool,
    },
    #[cfg(target_os = "linux")]
    WaylandShm(Box<wayland_shm::WaylandShmPresent>),
    #[cfg(feature = "gpu")]
    Gpu(Box<gpu::GpuPresent>),
    #[cfg(all(test, target_os = "linux"))]
    Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialRasterBackend {
    #[cfg(target_os = "macos")]
    Mac,
    Softbuffer {
        wayland: bool,
    },
    #[cfg(target_os = "linux")]
    WaylandShm,
    #[cfg(feature = "gpu")]
    Gpu,
}

fn backend_supports_partial_raster(backend: PartialRasterBackend) -> bool {
    match backend {
        #[cfg(target_os = "macos")]
        PartialRasterBackend::Mac => true,
        PartialRasterBackend::Softbuffer { wayland } => !wayland,
        #[cfg(target_os = "linux")]
        PartialRasterBackend::WaylandShm => true,
        #[cfg(feature = "gpu")]
        PartialRasterBackend::Gpu => false,
    }
}

fn partial_raster_allowed(
    backend: PartialRasterBackend,
    render_timer: config::RenderTimer,
) -> bool {
    backend_supports_partial_raster(backend) && !render_timer.shows_osd()
}

#[cfg(target_os = "linux")]
fn use_wayland_shm(want_alpha: bool, wayland: bool) -> bool {
    want_alpha && wayland
}

/// Path for `PRISMATTYC_DUMP_PRESENT`. Empty or unset means no dump.
fn dump_present_path() -> Option<PathBuf> {
    let value = std::env::var_os("PRISMATTYC_DUMP_PRESENT")?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Write the CPU framebuffer that is about to present as an 8-bit RGBA PNG.
/// Callers keep the side effect out of `rasterize_frame`.
fn present_png_empty(width: u32, height: u32) -> bool {
    width == 0 || height == 0
}

fn present_png_parent(path: &Path) -> Option<&Path> {
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

fn write_present_png(path: &Path, pixels: &[u32], width: u32, height: u32) -> Result<()> {
    if present_png_empty(width, height) {
        bail!("dump present png: empty frame");
    }
    let expected = width as usize * height as usize;
    if pixels.len() < expected {
        bail!(
            "dump present png: pixel len {} < {}x{}",
            pixels.len(),
            width,
            height
        );
    }
    if let Some(parent) = present_png_parent(path) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut rgba = vec![0u8; expected * 4];
        for (i, px) in pixels.iter().take(expected).enumerate() {
            let o = i * 4;
            rgba[o] = ((*px >> 16) & 0xff) as u8;
            rgba[o + 1] = ((*px >> 8) & 0xff) as u8;
            rgba[o + 2] = (*px & 0xff) as u8;
            rgba[o + 3] = (*px >> 24) as u8;
        }
        writer.write_image_data(&rgba)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn maybe_dump_present(
    path: Option<&Path>,
    seq: &mut u64,
    pixels: &[u32],
    width: u32,
    height: u32,
    full_reason: Option<FullRepaintReason>,
) {
    let Some(path) = path else {
        return;
    };
    *seq = seq.saturating_add(1);
    if let Err(error) = write_present_png(path, pixels, width, height) {
        eprintln!("prismattyc-host: dump present png failed: {error}");
    }
    let sidecar = path.with_extension("json");
    let body = format!(
        "{{\"width\":{},\"height\":{},\"full\":{},\"full_repaint_reason\":{},\"seq\":{}}}\n",
        width,
        height,
        full_reason.is_some(),
        full_reason.map_or("null".into(), |r| format!("\"{}\"", r.as_str())),
        *seq,
    );
    if let Err(error) = std::fs::write(&sidecar, body) {
        eprintln!("prismattyc-host: dump present sidecar failed: {error}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashKeyPlan {
    None,
    Dismiss,
    Quit,
    StartWalkthrough,
    SetPage(splash::Page),
}

fn splash_key_plan(action: Option<splash::Action>) -> SplashKeyPlan {
    match action {
        None => SplashKeyPlan::None,
        Some(splash::Action::Dismiss) => SplashKeyPlan::Dismiss,
        Some(splash::Action::Quit) => SplashKeyPlan::Quit,
        Some(splash::Action::Show(prismattyc_core::splash::Topic::Walkthrough)) => {
            SplashKeyPlan::StartWalkthrough
        }
        Some(splash::Action::Show(topic)) => SplashKeyPlan::SetPage(splash::Page::Topic(topic)),
        Some(splash::Action::Back) => SplashKeyPlan::SetPage(splash::Page::Main),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SplashApply {
    quit: bool,
    start_walkthrough: bool,
    dirty: bool,
}

fn apply_splash_outcome(
    splash: &mut Option<splash::Splash>,
    splash_state: splash::Splash,
    plan: SplashKeyPlan,
) -> SplashApply {
    match plan {
        SplashKeyPlan::None => SplashApply {
            quit: false,
            start_walkthrough: false,
            dirty: true,
        },
        SplashKeyPlan::Dismiss => {
            *splash = None;
            SplashApply {
                quit: false,
                start_walkthrough: false,
                dirty: true,
            }
        }
        SplashKeyPlan::Quit => SplashApply {
            quit: true,
            start_walkthrough: false,
            dirty: false,
        },
        SplashKeyPlan::StartWalkthrough => SplashApply {
            quit: false,
            start_walkthrough: true,
            dirty: true,
        },
        SplashKeyPlan::SetPage(page) => {
            *splash = Some(splash::Splash {
                page,
                ..splash_state
            });
            SplashApply {
                quit: false,
                start_walkthrough: false,
                dirty: true,
            }
        }
    }
}

fn finish_splash_apply(dirty: &mut bool, applied: SplashApply) -> bool {
    *dirty |= applied.dirty;
    applied.quit
}

/// Job-only. Empty, unset, zero, or non-numeric means off.
fn e2e_dismiss_splash_ms() -> Option<u64> {
    let raw = std::env::var("PRISMATTYC_E2E_DISMISS_SPLASH_MS").ok()?;
    let ms = raw.parse::<u64>().ok()?;
    (ms > 0).then_some(ms)
}

fn e2e_dismiss_due(elapsed_ms: u64, deadline_ms: u64) -> bool {
    elapsed_ms >= deadline_ms
}

/// Apply a splash key. Quit is `SplashApply.quit` from `splash_key_plan`.
/// Shared by winit `KeyboardInput` and `PRISMATTYC_E2E_DISMISS_SPLASH_MS`.
fn dispatch_splash_key(host: &mut HostState, key: &Key, modifiers: ModifiersState) {
    let Some(splash_state) = host.splash else {
        return;
    };
    let applied = apply_splash_outcome(
        &mut host.splash,
        splash_state,
        splash_key_plan(splash::key_action(splash_state.page, key, modifiers)),
    );
    if applied.start_walkthrough {
        start_walkthrough(host);
    }
    let _ = finish_splash_apply(&mut host.dirty, applied);
}

fn maybe_e2e_dismiss_splash(host: &mut HostState) {
    let Some((start, deadline_ms)) = host.e2e_dismiss_at else {
        return;
    };
    let elapsed_ms = start.elapsed().as_millis() as u64;
    if !e2e_dismiss_due(elapsed_ms, deadline_ms) {
        return;
    }
    host.e2e_dismiss_at = None;
    dispatch_splash_key(host, &Key::Named(NamedKey::Enter), ModifiersState::empty());
    host.e2e_second_dump_at = Some(Instant::now() + Duration::from_millis(400));
}

fn softbuffer_partial_raster_allowed(backend_allowed: bool, age: u8, alpha_visual: bool) -> bool {
    // Translucent X11 buffers already contain premultiplied pixels. Repaint
    // them before conversion so retained pixels do not darken each frame.
    backend_allowed && age == 1 && !alpha_visual
}

impl PresentBackend {
    fn softbuffer(window: Arc<Window>, wayland: bool) -> Result<Self> {
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer context: {e}"))?;
        let surface = softbuffer::Surface::new(&context, window)
            .map_err(|e| anyhow::anyhow!("softbuffer surface: {e}"))?;
        Ok(Self::Softbuffer {
            _context: context,
            surface,
            wayland,
            #[cfg(test)]
            fail_paint: false,
        })
    }

    fn paint(&mut self, host: &mut HostState, width: u32, height: u32) -> Result<()> {
        // The OSD rewrites a moving host-drawn rectangle after terminal
        // rasterization. Keep it on the conservative full-frame path until
        // its exact rectangle is part of FrameDamage.
        let partial_allowed = partial_raster_allowed(
            match self {
                #[cfg(target_os = "macos")]
                Self::Mac(_) => PartialRasterBackend::Mac,
                Self::Softbuffer { wayland, .. } => {
                    PartialRasterBackend::Softbuffer { wayland: *wayland }
                }
                #[cfg(target_os = "linux")]
                Self::WaylandShm(_) => PartialRasterBackend::WaylandShm,
                #[cfg(feature = "gpu")]
                Self::Gpu(_) => PartialRasterBackend::Gpu,
                #[cfg(all(test, target_os = "linux"))]
                Self::Probe => PartialRasterBackend::Softbuffer { wayland: true },
            },
            host.render_timer,
        );
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(mac) => {
                let retained = mac.prepare(width, height)?;
                let raster_started = Instant::now();
                let damage = rasterize_frame(
                    host,
                    mac.pixels_mut(),
                    width,
                    height,
                    partial_allowed && retained,
                );
                if host.render_timer.shows_osd() {
                    rasterize_render_timer(
                        mac.pixels_mut(),
                        width,
                        height,
                        &host.font,
                        &host.theme,
                        host.render_osd,
                    );
                }
                host.render_frame.timing.raster_us = raster_started.elapsed().as_micros() as u64;
                maybe_dump_present(
                    host.dump_present.as_deref(),
                    &mut host.dump_present_seq,
                    mac.pixels_mut(),
                    width,
                    height,
                    host.render_frame.full_repaint_reason,
                );
                let present_started = Instant::now();
                mac.present(damage)?;
                host.render_frame.timing.present_us = present_started.elapsed().as_micros() as u64;
            }
            #[cfg(all(test, target_os = "linux"))]
            Self::Probe => unreachable!("probe backend cannot paint"),
            Self::Softbuffer {
                surface,
                #[cfg(test)]
                fail_paint,
                ..
            } => {
                #[cfg(test)]
                if *fail_paint {
                    anyhow::bail!("injected present failure");
                }
                surface
                    .resize(
                        NonZeroU32::new(width).unwrap(),
                        NonZeroU32::new(height).unwrap(),
                    )
                    .map_err(|e| anyhow::anyhow!("surface resize: {e}"))?;
                let mut buffer = surface
                    .buffer_mut()
                    .map_err(|e| anyhow::anyhow!("buffer_mut: {e}"))?;
                let raster_started = Instant::now();
                // Only age 1 contains the immediately preceding frame. Core
                // Graphics returns a new zeroed buffer (age 0) every time;
                // older buffers also need a full repaint without damage history.
                let partial_allowed = softbuffer_partial_raster_allowed(
                    partial_allowed,
                    buffer.age(),
                    host.alpha_visual,
                );
                let _damage = rasterize_frame(host, &mut buffer, width, height, partial_allowed);
                if host.render_timer.shows_osd() {
                    rasterize_render_timer(
                        &mut buffer,
                        width,
                        height,
                        &host.font,
                        &host.theme,
                        host.render_osd,
                    );
                }
                host.render_frame.timing.raster_us = raster_started.elapsed().as_micros() as u64;
                maybe_dump_present(
                    host.dump_present.as_deref(),
                    &mut host.dump_present_seq,
                    &buffer,
                    width,
                    height,
                    host.render_frame.full_repaint_reason,
                );
                let present_started = Instant::now();
                // The X11 present path expects premultiplied ARGB. Skip the
                // pass on an opaque window so the default path pays nothing.
                if host.alpha_visual {
                    premultiply_in_place(&mut buffer);
                }
                buffer
                    .present()
                    .map_err(|e| anyhow::anyhow!("present: {e}"))?;
                host.render_frame.timing.present_us = present_started.elapsed().as_micros() as u64;
            }
            #[cfg(target_os = "linux")]
            Self::WaylandShm(shm) => {
                shm.prepare(width, height)?;
                let raster_started = Instant::now();
                let damage =
                    rasterize_frame(host, shm.pixels_mut(), width, height, partial_allowed);
                if host.render_timer.shows_osd() {
                    rasterize_render_timer(
                        shm.pixels_mut(),
                        width,
                        height,
                        &host.font,
                        &host.theme,
                        host.render_osd,
                    );
                }
                host.render_frame.timing.raster_us = raster_started.elapsed().as_micros() as u64;
                maybe_dump_present(
                    host.dump_present.as_deref(),
                    &mut host.dump_present_seq,
                    shm.pixels_mut(),
                    width,
                    height,
                    host.render_frame.full_repaint_reason,
                );
                let present_started = Instant::now();
                shm.present(damage)?;
                host.render_frame.timing.present_us = present_started.elapsed().as_micros() as u64;
            }
            #[cfg(feature = "gpu")]
            Self::Gpu(gpu) => {
                gpu.resize(width, height)?;
                let raster_started = Instant::now();
                let _damage =
                    rasterize_frame(host, gpu.pixels_mut(), width, height, partial_allowed);
                if host.render_timer.shows_osd() {
                    rasterize_render_timer(
                        gpu.pixels_mut(),
                        width,
                        height,
                        &host.font,
                        &host.theme,
                        host.render_osd,
                    );
                }
                host.render_frame.timing.raster_us = raster_started.elapsed().as_micros() as u64;
                maybe_dump_present(
                    host.dump_present.as_deref(),
                    &mut host.dump_present_seq,
                    gpu.pixels_mut(),
                    width,
                    height,
                    host.render_frame.full_repaint_reason,
                );
                let present_started = Instant::now();
                gpu.present()?;
                host.render_frame.timing.present_us = present_started.elapsed().as_micros() as u64;
            }
        }
        Ok(())
    }

    /// Can this backend carry per-pixel alpha to the compositor?
    ///
    /// softbuffer 0.4 posts `wl_shm` `Xrgb8888` on Wayland, so the alpha
    /// byte is dropped there; its X11 backend accepts depth-32 visuals, so
    /// a window created with `with_transparent(true)` under X11/XWayland
    /// does carry premultiplied alpha. The PT-118 shm path is ARGB8888.
    fn carries_alpha(&self, window: &Window) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(_) => true,
            Self::Softbuffer { .. } => {
                use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
                let Ok(handle) = window.window_handle() else {
                    return false;
                };
                matches!(
                    handle.as_raw(),
                    RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_)
                )
            }
            #[cfg(target_os = "linux")]
            Self::WaylandShm(_) => true,
            #[cfg(feature = "gpu")]
            Self::Gpu(_) => false,
            #[cfg(all(test, target_os = "linux"))]
            Self::Probe => false,
        }
    }

    /// Is a compositor background blur live for this window?
    fn blur_active(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::WaylandShm(shm) => shm.blur_active(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TabRename {
    index: usize,
    window: MuxWindowId,
    /// `Some` when the editor renames a pane title (PT-148) instead of the
    /// tab; the text still edits in the tab chip.
    pane: Option<PaneId>,
    buffer: String,
    /// Pre-fill is fully selected on open; typing/backspace replaces it.
    selected: bool,
}

/// Incremental find over primary history (host chords; see [`is_find_chord`]).
#[derive(Debug, Default, Clone)]
struct FindMode {
    active: bool,
    query: String,
    last: Option<HistoryMatch>,
    /// 1-based index and total (`2/5`); `None` on miss or empty query.
    rank: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct ThemePicker {
    /// Theme to restore when the picker is cancelled.
    original: theme::Theme,
    /// Row under preview in the current list (root or family). `None` means a
    /// custom theme is active and no shipped row has been chosen yet.
    selected: Option<usize>,
    /// `None` is the root list. `Some` is a family submenu (Monokai, Hive).
    family: Option<String>,
    /// First row shown in the current list when the window cannot fit all rows.
    scroll: usize,
}

/// One breath of the active dot, and how many repaints it costs at most.
const PULSE_PERIOD_MS: u128 = 1000;
const PULSE_STEPS: u128 = frame_damage::PULSE_STEPS as u128;

/// One light-cycle border sweep, and its repaint budget. Runs once per focus
/// change to completion, so the cost is a burst, not a steady drain.
const DEFAULT_LIGHT_CYCLE_MS: u128 = 280;
const LIGHT_CYCLE_STEPS: u128 = 24;

/// Visual bell: full-frame invert stays lit this long (PT-39).
const BELL_FLASH_MS: u128 = 120;
/// Minimum gap between OS bell notifications; a BEL storm (`yes $'\a'`)
/// must not spam the notification daemon.
const BELL_NOTIFY_MIN_GAP: Duration = Duration::from_secs(5);
/// Minimum gap between attention OS notifications for one pane.
const ATTENTION_NOTIFY_MIN_GAP: Duration = Duration::from_secs(5);
/// Minimum gap between audible bell cues.
const BELL_SOUND_MIN_GAP: Duration = Duration::from_secs(1);
/// Toast chip text; also sizes the click target.
const BELL_TOAST_LABEL: &str = " bell ";

/// A live toast on the pane that produced feedback. `until` is fixed at the
/// event time from the config in force then; a later reload never moves it.
#[derive(Debug, Clone)]
struct BellToast {
    pane: PaneId,
    until: Instant,
    label: String,
}

/// Writer-death chip text is not a BEL toast (PT-119).
fn is_write_fail_toast(label: &str) -> bool {
    label == attach_log::WRITE_FAILED_TOAST
}

/// Enqueue log-backed WriteFailed chips. Not gated on `bell_toaster`.
fn apply_write_fail_toasts(
    bell_toasts: &mut Vec<BellToast>,
    toasts: Vec<(PaneId, String)>,
    linger: Duration,
    now: Instant,
) -> bool {
    if toasts.is_empty() {
        return false;
    }
    let until = now + linger;
    for (pane, label) in toasts {
        match bell_toasts.iter_mut().find(|toast| toast.pane == pane) {
            Some(toast) => {
                toast.label = label;
                toast.until = until;
            }
            None => bell_toasts.push(BellToast { pane, until, label }),
        }
    }
    true
}

/// Drop BEL/paste chips when `bell_toaster` turns off. Keep write-fail chips.
fn settle_bell_toasts_on_toaster_off(bell_toasts: &mut Vec<BellToast>) -> bool {
    let before = bell_toasts.len();
    bell_toasts.retain(|toast| is_write_fail_toast(&toast.label));
    bell_toasts.len() != before
}

/// Drop chips whose linger has elapsed. Each deadline is fixed at ring time.
fn expire_bell_toasts(toasts: &mut Vec<BellToast>, now: Instant) -> bool {
    retain_live_deadlines(toasts, now, |toast| toast.until)
}

fn retain_live_deadlines<T>(
    items: &mut Vec<T>,
    now: Instant,
    until: impl Fn(&T) -> Instant,
) -> bool {
    let before = items.len();
    items.retain(|item| now < until(item));
    items.len() != before
}

/// Chip rect (x, y, w, h) at the pane's top-right, matching
/// [`rasterize_bell_toast`]. `None` when there is no room to draw.
fn bell_toast_chip_rect(
    label: &str,
    cell_w: usize,
    cell_h: usize,
    content_x: usize,
    guest_y: usize,
    content_w: usize,
    guest_h: usize,
) -> Option<(usize, usize, usize, usize)> {
    if content_w == 0 || guest_h == 0 || cell_w == 0 {
        return None;
    }
    let cols = label.chars().count().max(1);
    let chip_w = cols.saturating_mul(cell_w).min(content_w);
    let chip_h = cell_h.min(guest_h);
    let x0 = content_x.saturating_add(content_w.saturating_sub(chip_w));
    Some((x0, guest_y, chip_w, chip_h))
}

/// Earlier of two optional deadlines (footer bar, bell flash).
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Idle hosts `Wait` (no 8ms poll). A running light-cycle sweep or active-dot
/// pulse arms `WaitUntil` for the next quantized frame only. PTY bytes wake
/// via `EventLoopProxy`, not this timer.
fn next_control_flow(
    now: Instant,
    border_anim: Option<Instant>,
    light_cycle_ms: u128,
    pulse_active: bool,
    pulse_epoch: Instant,
    extra_deadline: Option<Instant>,
) -> ControlFlow {
    let mut deadline: Option<Instant> = None;
    if let Some(start) = border_anim {
        let elapsed_ms = now.saturating_duration_since(start).as_millis();
        if elapsed_ms < light_cycle_ms {
            let step_ms = (light_cycle_ms / LIGHT_CYCLE_STEPS).max(1);
            let next_ms = ((elapsed_ms / step_ms) + 1) * step_ms;
            let next = start + Duration::from_millis(next_ms.min(light_cycle_ms) as u64);
            deadline = Some(if next > now {
                next
            } else {
                now + Duration::from_millis(1)
            });
        }
    }
    if pulse_active {
        let elapsed_ms = now.saturating_duration_since(pulse_epoch).as_millis();
        let step_ms = (PULSE_PERIOD_MS / PULSE_STEPS).max(1);
        let next_ms = ((elapsed_ms / step_ms) + 1) * step_ms;
        let next = pulse_epoch + Duration::from_millis(next_ms as u64);
        let next = if next > now {
            next
        } else {
            now + Duration::from_millis(1)
        };
        deadline = Some(match deadline {
            Some(existing) => existing.min(next),
            None => next,
        });
    }
    if let Some(extra) = extra_deadline {
        if extra > now {
            deadline = Some(match deadline {
                Some(existing) => existing.min(extra),
                None => extra,
            });
        }
    }
    match deadline {
        Some(when) => ControlFlow::WaitUntil(when),
        None => ControlFlow::Wait,
    }
}

impl Deref for HostState {
    type Target = mux::PaneRuntime;

    fn deref(&self) -> &Self::Target {
        self.mux.focused()
    }
}

impl DerefMut for HostState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.mux.focused_mut()
    }
}

#[derive(Debug, Default)]
struct MultiClick {
    last_at: Option<Instant>,
    pane: Option<PaneId>,
    row: usize,
    col: usize,
    count: u8,
}

impl MultiClick {
    fn on_left_down(&mut self, pane: PaneId, row: usize, col: usize) -> u8 {
        let now = Instant::now();
        let same = self.last_at.is_some_and(|then| {
            now.duration_since(then).as_millis() <= MULTI_CLICK_MS
                && self.row == row
                && self.col == col
                && self.pane == Some(pane)
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
        self.pane = Some(pane);
        self.row = row;
        self.col = col;
        self.count
    }
}

/// `NewWindow` requests a bare later window. `OpenConfig` requests a bare later
/// window whose only pane runs the user's editor.
#[derive(Debug)]
enum UserAction {
    Wake,
    // Constructed only by the macOS menu/Dock action target; on other
    // platforms ⌘N calls `open_window` directly, so the variant is matched
    // but never built. Silence the resulting dead_code lint off-macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    NewWindow,
    OpenConfig,
    AccessKit(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for UserAction {
    fn from(event: accesskit_winit::Event) -> Self {
        UserAction::AccessKit(event)
    }
}

impl PartialEq for UserAction {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Wake, Self::Wake)
            | (Self::NewWindow, Self::NewWindow)
            | (Self::OpenConfig, Self::OpenConfig) => true,
            (Self::AccessKit(left), Self::AccessKit(right)) => left.window_id == right.window_id,
            _ => false,
        }
    }
}

struct App {
    cli: Cli,
    /// One entry per OS window, keyed by winit's `WindowId`. Empty means no
    /// windows remain and the process exits (see `pump`/`window_event`).
    windows: std::collections::HashMap<WindowId, HostState>,
    exit_code: i32,
    /// Currently applied config-file state (defaults when no file exists).
    file_config: config::ConfigFile,
    /// Live-reload channel; `None` when the watcher could not start.
    config_rx: Option<std::sync::mpsc::Receiver<config::WatchDelivery>>,
    /// Startup config load failure, surfaced in the footer once the window
    /// exists (stderr is invisible from inside the window).
    startup_config_error: Option<String>,
    /// Keeps the config poller thread alive for the process lifetime.
    _config_watcher: Option<config::ConfigWatch>,
    /// Coalesced wake from PTY reader threads and the config watcher.
    wake: mux::Wake,
    wake_pending: Arc<AtomicBool>,
    /// Effective host key table (`[keys]` over the defaults, keybindings).
    /// Shared with the dispatcher per event; replaced on hot reload.
    keymap: Arc<keybind::KeyMap>,
    /// Defers operations that need a fresh mutable App borrow until the
    /// current window event releases its HostState borrow.
    event_proxy: EventLoopProxy<UserAction>,
    /// Live `{stem}.host.pid` this process registered (PT-65).
    registered_host: Option<(PathBuf, u32)>,
    /// Throttle for [`App::retry_register_host_pid`].
    last_register_try: Option<Instant>,
    last_render_status: Option<Instant>,
    render_status_seq: u64,
    last_component_poll: Option<Instant>,
    restart_view: Option<PathBuf>,
    #[cfg(windows)]
    restart_resume: Option<restart::Resume>,
}

impl App {
    fn new(
        cli: Cli,
        file_config: config::ConfigFile,
        startup_config_error: Option<String>,
        proxy: EventLoopProxy<UserAction>,
    ) -> Result<Self> {
        let wake_pending = Arc::new(AtomicBool::new(false));
        let pending = wake_pending.clone();
        let event_proxy = proxy.clone();
        let wake: mux::Wake = Arc::new(move || {
            if !pending.swap(true, Ordering::Relaxed) {
                let _ = proxy.send_event(UserAction::Wake);
            }
        });
        let config_path = config::config_path();
        let (watcher, rx) = match config::watch_with_wake(config_path.clone(), Some(wake.clone())) {
            Ok((watcher, rx)) => (Some(watcher), Some(rx)),
            Err(error) => {
                eprintln!("prismattyc-host: config hot reload disabled: {error:#}");
                (None, None)
            }
        };
        let keymap = Arc::new(file_config.loaded_keymap());
        Ok(Self {
            cli,
            windows: std::collections::HashMap::new(),
            exit_code: 0,
            file_config,
            config_rx: rx,
            startup_config_error,
            _config_watcher: watcher,
            wake,
            wake_pending,
            keymap,
            event_proxy,
            registered_host: None,
            last_register_try: None,
            last_render_status: None,
            render_status_seq: 0,
            last_component_poll: None,
            restart_view: None,
            #[cfg(windows)]
            restart_resume: None,
        })
    }

    /// Apply the newest reloaded config, if any. CLI/env-pinned settings and
    /// startup-only settings (`panes`) are never touched at runtime.
    fn poll_config_reload(&mut self) {
        let Some(rx) = self.config_rx.as_ref() else {
            return;
        };
        let mut newest = None;
        let mut reload_error = None;
        while let Ok(delivery) = rx.try_recv() {
            match delivery {
                Ok(file) => {
                    newest = Some(file);
                    reload_error = None;
                }
                Err(message) => reload_error = Some(message),
            }
        }
        // Banner state: the newest delivery wins — an error shows it, a
        // valid config clears it (including a startup error).
        for host in self.windows.values_mut() {
            match reload_error.clone() {
                Some(message) => {
                    if host.config_error.as_deref() != Some(message.as_str()) {
                        host.config_error = Some(message);
                        host.dirty = true;
                    }
                }
                None => {
                    if newest.is_some() && host.config_error.is_some() {
                        host.config_error = None;
                        host.dirty = true;
                    }
                }
            }
        }
        let Some(newest) = newest else { return };
        if newest == self.file_config {
            return;
        }
        let prior = std::mem::replace(&mut self.file_config, newest);
        // A valid config reload can change host-drawn pixels without terminal
        // damage (theme, background removal, opacity, chrome, or geometry).
        // Consume one conservative full frame before row-filtered raster can
        // resume.
        for host in self.windows.values_mut() {
            host.pending_full_repaint = Some(FullRepaintReason::Fallback);
        }
        if self.file_config.keys != prior.keys {
            // Rebuild the key table; the chord strip labels repaint with it.
            self.keymap = Arc::new(self.file_config.loaded_keymap());
            for host in self.windows.values_mut() {
                host.keymap = self.keymap.clone();
                host.dirty = true;
            }
        }
        if self.windows.is_empty() {
            // Window(s) not spawned yet: fold into the startup values instead
            // so the spawn picks the reload up (CLI/env pins still hold).
            self.cli.apply_config(&self.file_config);
            return;
        };

        for host in self.windows.values_mut() {
            let render_timer = self.file_config.render_timer();
            let render_timer_log_every_frame = self.file_config.render_timer_log_every_frame();
            if render_timer != prior.render_timer()
                || render_timer_log_every_frame != prior.render_timer_log_every_frame()
            {
                host.render_timer = render_timer;
                host.render_timer_log_every_frame = render_timer_log_every_frame;
                host.render_window = RenderWindow::default();
                host.render_osd = RenderWindowSummary::default();
                host.last_render_log = None;
                host.dirty = true;
            }
            if self.file_config.tab_strip != prior.tab_strip {
                host.tab_strip_mode = self.file_config.tab_strip();
                host.dirty = true;
            }
            if self.file_config.pane_titles() != prior.pane_titles() {
                host.pane_titles = self.file_config.pane_titles();
                host.dirty = true;
            }
            if self.file_config.hover_blend() != prior.hover_blend() {
                host.hover_blend = self.file_config.hover_blend();
                host.dirty = true;
            }
            if !self.cli.focus_border_pinned && self.file_config.focus_border != prior.focus_border
            {
                match self.file_config.focus_border.as_deref() {
                    Some(spec) => {
                        match parse_focus_border(spec) {
                            Some(index) => {
                                host.focus_border = index;
                                host.dirty = true;
                            }
                            None => {
                                eprintln!("prismattyc-host: config focus_border {spec:?} unknown; ignored")
                            }
                        }
                    }
                    // Key removed: back to the default.
                    None => {
                        host.focus_border = DEFAULT_FOCUS_BORDER_INDEX;
                        host.dirty = true;
                    }
                }
            }

            let reloaded_theme = self.file_config.loaded_theme();
            if reloaded_theme != prior.loaded_theme() {
                eprintln!("prismattyc-host: theme reloaded: {}", reloaded_theme.name);
                host.theme = reloaded_theme;
                host.pending_full_repaint = Some(FullRepaintReason::Theme);
                host.background = None;
                host.dirty = true;
            }

            let image_changed = self.file_config.background_image != prior.background_image;
            let opacity_changed =
                self.file_config.background_opacity() != prior.background_opacity();
            let blur_changed = self.file_config.background_blur_px() != prior.background_blur_px();
            if image_changed {
                host.background_png =
                    load_background_png(self.file_config.background_image.as_deref());
                host.background = None;
                host.dirty = true;
            }
            if opacity_changed || blur_changed {
                host.background_opacity = self.file_config.background_opacity();
                host.background_blur_px = self.file_config.background_blur_px();
                host.background = None;
                host.dirty = true;
            }
            let pane_opacity_active = self.file_config.pane_opacity_active();
            let pane_opacity_inactive = self.file_config.pane_opacity_inactive();
            if host.pane_opacity_active != pane_opacity_active
                || host.pane_opacity_inactive != pane_opacity_inactive
            {
                host.pane_opacity_active = pane_opacity_active;
                host.pane_opacity_inactive = pane_opacity_inactive;
                host.dirty = true;
            }
            let overlay_opacity = self.file_config.overlay_opacity();
            if host.overlay_opacity != overlay_opacity {
                host.overlay_opacity = overlay_opacity;
                host.dirty = true;
            }
            // The alpha visual is chosen once, at window creation. Reloading a
            // lower window_opacity into an opaque window would paint the
            // premultiplied ground as if over black, so only follow the value
            // when this window already has alpha.
            if host.alpha_visual {
                let window_alpha = opacity_to_alpha(self.file_config.window_opacity());
                let chrome_alpha = opacity_to_alpha(self.file_config.chrome_opacity());
                if host.window_alpha != window_alpha || host.chrome_alpha != chrome_alpha {
                    host.window_alpha = window_alpha;
                    host.chrome_alpha = chrome_alpha;
                    host.background = None;
                    host.dirty = true;
                }
            }
            if !host.alpha_visual && self.file_config.window_opacity() != prior.window_opacity() {
                eprintln!(
                    "prismattyc-host: window_opacity changed to {}; restart the host to \
                     recreate the window with an alpha visual",
                    self.file_config.window_opacity()
                );
            }

            #[cfg(target_os = "macos")]
            if self.file_config.window_blur() != prior.window_blur() {
                let wanted = self.file_config.window_blur();
                let active = if wanted && host.alpha_visual {
                    macos_window::set_window_blur(&host.window, true)
                } else {
                    let _ = macos_window::set_window_blur(&host.window, false);
                    false
                };
                if host.window_blur_active != active {
                    host.window_blur_active = active;
                    host.dirty = true;
                }
                if blur_notice(wanted, BlurSurface::Macos, active) {
                    eprintln!("prismattyc-host: {}", BLUR_UNSUPPORTED_NOTICE);
                }
            }

            if self.file_config.splash_animation != prior.splash_animation {
                if let Some(splash) = host.splash.as_mut() {
                    splash.animated = self.file_config.splash_animation.unwrap_or(true);
                    host.dirty = true;
                }
            }
            if self.file_config.focus_border_animation != prior.focus_border_animation {
                host.light_cycle = matches!(
                    self.file_config.focus_border_animation.as_deref(),
                    Some("light-cycle")
                );
                if !host.light_cycle {
                    // Turning the animation off mid-sweep settles the border.
                    host.border_anim = None;
                    host.dirty = true;
                }
            }
            host.light_cycle_ms = self
                .file_config
                .focus_border_animation_ms
                .map_or(DEFAULT_LIGHT_CYCLE_MS, u128::from);
            host.light_cycle_head = self.file_config.focus_border_animation_head.unwrap_or(true);

            let visual_bell = self.file_config.visual_bell();
            if host.visual_bell != visual_bell {
                host.visual_bell = visual_bell;
                if !visual_bell && host.bell_flash.is_some() {
                    // Turning the flash off mid-lit settles the frame.
                    host.bell_flash = None;
                    host.pending_full_repaint = Some(FullRepaintReason::Fallback);
                    host.dirty = true;
                }
            }
            let pane_visual_bell = self.file_config.pane_visual_bell.unwrap_or(false);
            if host.pane_visual_bell != pane_visual_bell {
                host.pane_visual_bell = pane_visual_bell;
                if host.bell_flash.take().is_some() {
                    host.pending_full_repaint = Some(FullRepaintReason::Fallback);
                }
                host.dirty = true;
            }
            if !host.visual_bell || !host.pane_visual_bell {
                host.dirty |= host.pane_bells.cancel();
            }
            host.audible_bell = self.file_config.audible_bell();
            host.walkthrough_audio = self.file_config.walkthrough_audio();
            host.bell_toaster_ms = Duration::from_millis(self.file_config.bell_toaster_ms());
            let bell_toaster = self.file_config.bell_toaster();
            if host.bell_toaster != bell_toaster {
                host.bell_toaster = bell_toaster;
                if !bell_toaster {
                    // BEL/paste chips go away. Write-fail chips stay (PT-119).
                    if settle_bell_toasts_on_toaster_off(&mut host.bell_toasts) {
                        host.dirty = true;
                    }
                }
            }
            host.os_notify_bell = self.file_config.os_notify_bell();
            let drag_toaster = self.file_config.drag_toaster();
            if host.drag_toaster != drag_toaster {
                host.drag_toaster = drag_toaster;
                host.dirty = true;
            }
            let attention_badge = self.file_config.attention_badge();
            if host.attention_badge != attention_badge {
                host.attention_badge = attention_badge;
                host.dirty = true;
            }
            host.attention_sound = self.file_config.attention_sound();
            host.os_notify_attention = self.file_config.os_notify_attention();
            host.a11y_announce = self.file_config.a11y_announce();

            let font_ligatures = self.file_config.font_ligatures();
            let font_features = self.file_config.font_features();
            if host.font_ligatures != font_ligatures || host.font_features != font_features {
                host.font_ligatures = font_ligatures;
                host.font_features = font_features;
                host.dirty = true;
            }

            let font_changed = self.file_config.font != prior.font
                || self.file_config.font_fallback != prior.font_fallback
                || self.file_config.font_px != prior.font_px;
            let requested_spacing = PaneSpacing::from(&self.file_config);
            let spacing_changed = requested_spacing != host.spacing;
            let tab_strip_changed = self.file_config.tab_strip != prior.tab_strip;
            let mut geometry_applied = false;
            let mut geometry_changed = false;
            if font_changed {
                let scale = host.window.scale_factor() as f32;
                let px = (self.file_config.font_px.unwrap_or(FONT_PX) * scale).max(10.0);
                let fallbacks = self.file_config.font_fallback.clone().unwrap_or_default();
                match FontMetrics::load_with(px, self.file_config.font.as_deref(), &fallbacks) {
                    Ok(font) => {
                        // Resize the PTY/emulator grid FIRST: if that fails, keep
                        // the prior font so metrics and grid never desync.
                        let geom = host_geom(
                            &font,
                            host.mux.active_pane_count() > 1,
                            show_tab_strip(host),
                            strip_handle_row(host),
                            requested_spacing,
                            host.space_rail.longest_name_cells(),
                        );
                        let (cols, rows) = size_to_cells(host.window.inner_size(), &font, geom);
                        if host.mux.resize_with_geom(cols, rows, geom).is_err() {
                            eprintln!(
                            "prismattyc-host: grid resize for reloaded font failed; keeping prior font"
                        );
                        } else {
                            host.font = font;
                            host.spacing = requested_spacing;
                            host.left_button_down = false;
                            host.cursor_cell = None;
                            host.dirty = true;
                            geometry_applied = true;
                            geometry_changed = true;
                        }
                    }
                    Err(error) => {
                        eprintln!("prismattyc-host: config font reload failed: {error:#}")
                    }
                }
            }

            if (spacing_changed || tab_strip_changed) && !geometry_applied {
                let geom = host_geom(
                    &host.font,
                    host.mux.active_pane_count() > 1,
                    show_tab_strip(host),
                    strip_handle_row(host),
                    requested_spacing,
                    host.space_rail.longest_name_cells(),
                );
                let (cols, rows) = size_to_cells(host.window.inner_size(), &host.font, geom);
                match host.mux.resize_with_geom(cols, rows, geom) {
                    Ok(()) => {
                        host.spacing = requested_spacing;
                        host.left_button_down = false;
                        host.cursor_cell = None;
                        if geom.rail_side == space_rail::RailSide::Off {
                            host.space_rail.leave();
                        }
                        host.dirty = true;
                        geometry_changed = true;
                    }
                    Err(error) => {
                        eprintln!("prismattyc-host: config pane spacing reload failed: {error:#}")
                    }
                }
            }
            if tab_strip_changed {
                host.window
                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
            }
            if geometry_changed {
                // Font, strip, spacing, and rail reloads can move the target
                // under a stationary pointer.
                sync_chrome_hover(host);
            }
        }

        if self.file_config.panes != prior.panes {
            eprintln!("prismattyc-host: config 'panes' applies at startup only");
        }
    }

    fn register_host_pid(&mut self) {
        self.register_host_pid_with(false);
    }

    /// `quiet` drops the error line; the once-a-second retry would
    /// otherwise fill the log when the socket directory is unwritable.
    fn register_host_pid_with(&mut self, quiet: bool) {
        if self.registered_host.is_some() {
            return;
        }
        let Some(socket) = host_mux_socket() else {
            return;
        };
        let path = prismattyc_mux::host_pid_path_from_socket(&socket);
        let pid = std::process::id();
        match prismattyc_mux::register_host_pid(&path, pid) {
            Ok(true) => self.registered_host = Some((path, pid)),
            Ok(false) => {}
            Err(error) => {
                if !quiet {
                    eprintln!("prismattyc-host: could not register host pid: {error}");
                }
            }
        }
    }

    /// An unregistered window (`--new-window`, or a bare launch while an
    /// older host held `{stem}.host.pid`) takes the registration over once
    /// that host is gone, so its spaces rail and attach-tabs cache come
    /// alive without a relaunch (PT-123). Once a second at most.
    fn retry_register_host_pid(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let Some(default_view) =
            host_mux_socket().map(|socket| attach_tabs::layout_path_from_socket(&socket))
        else {
            return;
        };
        if self.registered_host.is_some()
            && self
                .windows
                .values()
                .any(|host| host.attach_layout_path.as_ref() == Some(&default_view))
        {
            return;
        }
        if self
            .windows
            .values()
            .any(|host| host.space_opens.blocks_persist())
        {
            return;
        }
        let now = Instant::now();
        if self
            .last_register_try
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
        {
            return;
        }
        self.last_register_try = Some(now);
        if self.registered_host.is_none() {
            self.register_host_pid_with(true);
        }
        if self.registered_host.is_none() {
            return;
        }
        // Exactly one window writes the shared cache: the focused one, else
        // the first.
        let writer = self
            .windows
            .iter()
            .find(|(_, host)| host.window_focused)
            .or_else(|| self.windows.iter().next())
            .map(|(id, _)| *id);
        if let Some(host) = writer.and_then(|id| self.windows.get_mut(&id)) {
            host.cache_writer = true;
            host.attach_layout_path = Some(default_view);
            host.attach_layout = Some(attach_tabs::AttachTabsFile::default());
            persist_attach_layout_from_live(host);
            rail_toast(host, " this window now handles CLI host requests ");
        }
    }

    fn unregister_host_pid(&mut self) {
        if let Some((path, pid)) = self.registered_host.take() {
            prismattyc_mux::unregister_host_pid(&path, pid);
        }
    }

    fn poll_attach_tabs(&mut self) {
        for host in self.windows.values_mut() {
            poll_host_attach_tabs(host);
            advance_space_opens(host);
            refresh_space_views(host);
            local_views::persist_and_restore(host, false);
        }
    }

    fn pump(&mut self, event_loop: &ActiveEventLoop) {
        // ONE drain pass per event-loop cycle, then yield. Looping on `more`
        // pinned the main (UI) thread: under sustained PTY output, drain_pty
        // always reports leftover work, so the old continue never returned.
        // RedrawRequested calls pump() *before* paint(), so no frame presented
        // and the OS run loop starved (#183). Multi-window: drain every window,
        // fold WaitUntil to the earliest deadline, apply once. If a pane still
        // has buffered output or a wake raced in mid-drain, re-arm one coalesced
        // wake; the resulting user_event re-enters pump next cycle. Clearing the
        // flag *before* drain still avoids dropping a child-EOF wake that
        // arrives while we are inside pump (live cascade).
        self.wake_pending.store(false, Ordering::Relaxed);
        restart::poll(self);
        self.poll_config_reload();
        self.poll_attach_tabs();
        let mut more = false;
        let mut closed: Vec<WindowId> = Vec::new();
        let mut next_deadline: Option<Instant> = None;
        self.retry_register_host_pid();
        let rail_now = Instant::now();
        for (id, host) in self.windows.iter_mut() {
            if host.space_rail.poll(&spaces_dir(), rail_now) {
                rail_changed(host);
            }
            adopt_nested_attaches(host, rail_now);
            if !host.space_opens.blocks_persist()
                && host.space_rail.current_index().is_none()
                && !host.space_rail.names.is_empty()
            {
                let live = live_tab_session_names(host);
                if host.space_rail.infer_current(&spaces_dir(), &live) {
                    let inferred = host.space_rail.current.clone();
                    host.space_rail.current = None;
                    set_current_space(host, inferred);
                }
            }
            if Self::drain_pty(host) {
                more = true;
            }
            maybe_e2e_dismiss_splash(host);
            if let Some(at) = host.e2e_second_dump_at {
                if Instant::now() >= at {
                    host.e2e_second_dump_at = None;
                    host.dirty = true;
                }
            }
            if host.dirty {
                host.window.request_redraw();
            }
            if host.mux.all_children_exited() {
                closed.push(*id);
                continue;
            }
            let now = Instant::now();
            if let Some(until) = host.footer_until {
                if now >= until {
                    host.footer_until = None;
                    host.dirty = true;
                }
            }
            // A lit bell flash must arm a deadline too: an idle host in
            // `Wait` would otherwise keep the inverted frame until the next
            // PTY byte or event happens to wake it.
            let flash_end = host
                .bell_flash
                .map(|start| start + Duration::from_millis(BELL_FLASH_MS as u64))
                .into_iter()
                .chain(host.pane_bells.deadline())
                .min();
            let toast_end = host.bell_toasts.iter().map(|toast| toast.until).min();
            let notice_end = host.title_notice.as_ref().map(|notice| notice.until);
            // The splash attract loop arms its own frame deadline while the
            // window is visible; it ticks in drain_pty like the other
            // animations.
            let splash_frame = host
                .splash
                .as_ref()
                .filter(|_| !host.window_occluded)
                .and_then(splash::Splash::next_frame_at);
            // The shared-cache poll, the spaces rail, and nested-attach
            // adoption all run from this pump. An idle host (no PTY bytes,
            // no input) sat in `Wait` forever, so `pmux space open` found
            // "host did not reload" and a rail chip looked dead until the
            // pointer moved (PT-171). Heartbeat once a second while this
            // window owns the cache.
            let cache_poll = host
                .attach_layout_path
                .as_ref()
                .filter(|_| host.cache_writer)
                .map(|_| now + CACHE_POLL_HEARTBEAT);
            let extra_deadline = earliest(
                earliest(host.footer_until, flash_end),
                earliest(
                    earliest(toast_end, notice_end),
                    earliest(splash_frame, cache_poll),
                ),
            );
            if let ControlFlow::WaitUntil(when) = next_control_flow(
                now,
                host.border_anim,
                host.light_cycle_ms,
                // The pulse only arms the timer while someone can see it; an
                // unfocused or covered window rests in `Wait` until real
                // output (EventLoopProxy) or focus wakes it.
                host.mux.active_count() > 0 && host.window_focused && !host.window_occluded,
                host.pulse_epoch,
                extra_deadline,
            ) {
                next_deadline = Some(match next_deadline {
                    Some(existing) => existing.min(when),
                    None => when,
                });
            }
        }
        for id in closed {
            self.windows.remove(&id);
        }
        if self.windows.is_empty() {
            self.unregister_host_pid();
            event_loop.exit();
            return;
        }
        self.publish_render_status();
        event_loop.set_control_flow(match next_deadline {
            Some(when) => ControlFlow::WaitUntil(when),
            None => ControlFlow::Wait,
        });
        if more || self.wake_pending.load(Ordering::Relaxed) {
            (self.wake)();
        }
    }

    fn open_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        config_editor: bool,
    ) -> Result<WindowId> {
        let editor_command = if config_editor {
            let path = config::config_path();
            config_template::ensure_template(&path)?;
            let visual = std::env::var("VISUAL").ok();
            let editor = std::env::var("EDITOR").ok();
            let mut command = resolve_editor_command(visual.as_deref(), editor.as_deref());
            command.args.push(path.to_string_lossy().into_owned());
            Some(command)
        } else {
            None
        };
        // Provisional size; cell metrics re-resolved after we know scale factor.
        // `mut` is used only by the Wayland app-id block below (Linux/BSD).
        #[allow(unused_mut)]
        let os_tree = self.file_config.a11y_os_tree();
        let mut attrs = Window::default_attributes()
            .with_title("Prismattyc")
            .with_window_icon(icon::load_window_icon())
            .with_inner_size(winit::dpi::LogicalSize::new(80.0 * 9.0, 24.0 * 18.0))
            .with_visible(!os_tree);
        // App id / WM_CLASS so desktop files and compositors match the brand icon.
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs = attrs.with_name("prismattyc-host", "Prismattyc");
        }
        // Request alpha at creation time. Other platforms keep their opaque
        // visual unless configured otherwise. macOS always supports alpha so
        // opacity and blur can be enabled through hot reload.
        let want_alpha = cfg!(target_os = "macos") || wants_alpha_visual(&self.file_config);
        if want_alpha {
            attrs = attrs.with_transparent(true);
        }
        if self.cli.gpu {
            #[cfg(not(feature = "gpu"))]
            bail!(
                "prismattyc-host --gpu requires a build with --features gpu \
                 (CPU rendering remains the default; see docs/rendering.md)"
            );
        }
        let window = Arc::new(event_loop.create_window(attrs)?);
        let a11y = os_tree.then(|| {
            accesskit_winit::Adapter::with_event_loop_proxy(
                event_loop,
                window.as_ref(),
                self.event_proxy.clone(),
            )
        });
        if a11y.is_some() {
            window.set_visible(true);
        }
        window.set_ime_allowed(true);
        #[cfg(target_os = "macos")]
        icon::apply_macos_app_icon();
        #[cfg(target_os = "macos")]
        if self.windows.is_empty() {
            macos_menu::install_dock_menu();
        }
        let scale = window.scale_factor() as f32;
        let base_px = self.file_config.font_px.unwrap_or(FONT_PX);
        let fallbacks = self.file_config.font_fallback.clone().unwrap_or_default();
        let font = FontMetrics::load_with(
            (base_px * scale).max(10.0),
            self.file_config.font.as_deref(),
            &fallbacks,
        )?;
        let spacing = PaneSpacing::from(&self.file_config);
        let theme = self.file_config.loaded_theme();
        // Claim ownership before reading the shared layout. A second host
        // must not use the registered host's grouping cache for startup.
        let first_window = self.windows.is_empty();
        self.register_host_pid();
        let mut attach_sessions = if config_editor {
            Vec::new()
        } else {
            startup_attach_targets(
                first_window || self.restart_view.is_some(),
                &self.cli.attach_sessions,
            )
            .to_vec()
        };
        let startup = if config_editor {
            StartupWindowPlan {
                attach: StartupAttachPlan::Bare,
                cache_writer: false,
            }
        } else {
            startup_window_plan(
                first_window,
                self.registered_host.is_some(),
                !attach_sessions.is_empty(),
            )
        };
        let explicit_view = self.restart_view.clone().or_else(|| {
            first_window
                .then(|| std::env::var_os("PMUX_VIEW_PATH"))
                .flatten()
                .map(PathBuf::from)
        });
        let attach_layout_path = explicit_view.clone().or_else(|| {
            host_mux_socket().map(|socket| {
                if startup.cache_writer {
                    attach_tabs::layout_path_from_socket(&socket)
                } else {
                    attach_tabs::window_layout_path(
                        &socket,
                        std::process::id(),
                        u64::from(window.id()),
                    )
                }
            })
        });
        let stored_layout = match if explicit_view.is_some() && !attach_sessions.is_empty() {
            StartupAttachPlan::ExplicitWithCache
        } else {
            startup.attach
        } {
            StartupAttachPlan::ExplicitWithCache => attach_layout_path
                .as_ref()
                .and_then(|path| attach_tabs::load(path)),
            StartupAttachPlan::Bare | StartupAttachPlan::ExplicitOneTabPerTarget => None,
        };
        // An explicit session selection must not inherit another Space's
        // global cache merely because this process becomes the registered host.
        let stored_layout = stored_layout.filter(|layout| {
            explicit_view.is_some()
                || attach_sessions.iter().all(|target| {
                    layout
                        .tabs
                        .iter()
                        .any(|tab| tab.sessions.contains(&target.session))
                })
        });
        let startup_space = if (first_window || self.restart_view.is_some()) && !config_editor {
            stored_layout
                .as_ref()
                .and_then(|layout| layout.space.clone())
                .or_else(live_env_space)
        } else {
            None
        };
        let startup_owner = startup_space
            .as_deref()
            .and_then(|name| load_space(&spaces_dir(), name).ok())
            .and_then(|space| space.id);
        if startup_space.is_some() {
            let snapshot = attach_log::live_snapshot();
            attach_sessions.retain(|target| {
                startup_owner
                    .as_ref()
                    .zip(snapshot.as_ref())
                    .is_some_and(|(owner, snapshot)| {
                        snapshot.sessions.iter().any(|session| {
                            session.id.to_string() == target.session
                                && session.space_id.as_ref() == Some(owner)
                        })
                    })
            });
        }
        let prior_cache_stamp = attach_layout_path
            .as_ref()
            .and_then(|path| cache_stamp(path));
        let restore_prompt = if startup.cache_writer && startup.attach == StartupAttachPlan::Bare {
            attach_layout_path
                .as_ref()
                .and_then(|path| attach_tabs::load(path))
                .and_then(restore_prompt::RestorePrompt::new)
        } else {
            None
        };
        // Consume this cache stamp once. While the choice is open the poll is
        // held; declining must not regroup this same cache on the next tick.
        let startup_cache_stamp = restore_prompt.as_ref().and(prior_cache_stamp);
        let grouped = attach_tabs::group_attach_targets(&attach_sessions, stored_layout.as_ref());
        let attach_groups = &grouped.groups;
        let (boot_program, boot_args) = if let Some(command) = editor_command {
            (command.program, command.args)
        } else if startup_space.is_some() && attach_sessions.is_empty() {
            (mux::EMPTY_SPACE_PROGRAM.to_string(), Vec::new())
        } else {
            attach_boot_command(&attach_sessions, &self.cli, attach_groups)
        };
        let tab_strip_mode = self.file_config.tab_strip();
        let pane_count = if config_editor { 1 } else { self.cli.panes };
        let initial_tab_count = attach_groups.len().max(1);
        let initial_multi_pane = (pane_count > 1 && attach_sessions.is_empty())
            || attach_groups.iter().any(|(_, members)| members.len() > 1);
        let mut space_rail =
            space_rail::SpaceRail::new(if first_window { live_env_space() } else { None });
        space_rail.refresh(&spaces_dir());
        let geom = host_geom(
            &font,
            initial_multi_pane,
            tab_strip_visible(tab_strip_mode, initial_tab_count, initial_multi_pane),
            attach_groups.iter().any(|(_, members)| members.len() > 1),
            spacing,
            space_rail.longest_name_cells(),
        );
        // Splash on the first window of a bare launch only; attach targets
        // and explicit programs skip it, as do later (Super+N) windows.
        let show_splash = restore_prompt.is_none()
            && startup_space.is_none()
            && !config_editor
            && first_window
            && splash::should_show(
                self.cli.no_splash,
                self.cli.explicit_program || !attach_sessions.is_empty(),
                env_flag_enabled("PRISMATTYC_NO_SPLASH"),
                self.file_config.splash(),
            );
        // Preserve the single-pane 80×24 content baseline after configured
        // insets. A splash launch opens wide enough to fit the word art and
        // its flare margins, plus breathing room, so the art is not clipped.
        let (init_cols, init_rows) = if show_splash {
            splash::window_cells(80, 24)
        } else {
            (80, 24)
        };
        let _ = window.request_inner_size(initial_window_size(&font, geom, init_cols, init_rows));
        let present = open_present_backend(
            window.clone(),
            self.cli.gpu,
            want_alpha,
            self.file_config.window_blur(),
        )?;
        let alpha_visual = want_alpha && present.carries_alpha(&window);
        if wants_alpha_visual(&self.file_config) && !alpha_visual {
            eprintln!("prismattyc-host: {}", ALPHA_UNSUPPORTED_NOTICE);
        }
        #[cfg(target_os = "macos")]
        let native_window_blur = alpha_visual
            && self.file_config.window_blur()
            && macos_window::set_window_blur(&window, true);
        #[cfg(not(target_os = "macos"))]
        let native_window_blur = false;
        let blur_installed = native_window_blur || present.blur_active();
        let blur_surface = if cfg!(target_os = "macos") {
            BlurSurface::Macos
        } else {
            BlurSurface::Compositor
        };
        if blur_notice(self.file_config.window_blur(), blur_surface, blur_installed) {
            eprintln!("prismattyc-host: {}", BLUR_UNSUPPORTED_NOTICE);
        }

        let (cols, rows) = size_to_cells(window.inner_size(), &font, geom);
        let mut attach_pane_sessions: HashMap<PaneId, String> = HashMap::new();
        let mut mux = mux::MuxRuntime::spawn_with_geom(
            &boot_program,
            &boot_args,
            cols,
            rows,
            geom,
            self.cli.experimental_rich,
            Some(self.wake.clone()),
        )
        .with_context(|| format!("spawn {boot_program:?}"))?;
        mux.space_id = startup_owner;
        if attach_sessions.is_empty() && startup_space.is_none() {
            for index in 1..pane_count {
                let axis = if index % 2 == 1 {
                    prismattyc_mux::Axis::Horizontal
                } else {
                    prismattyc_mux::Axis::Vertical
                };
                mux.split_focused(&self.cli.program, &self.cli.child_args, axis, 0.5)?;
            }
        } else {
            let pane_sessions = open_attach_session_tabs(&mut mux, attach_groups)?;
            seed_attach_focus(&mut mux, &grouped, &pane_sessions);
            attach_pane_sessions = pane_sessions.into_iter().collect();
        }
        if let Ok(raw) = std::env::var("PRISMATTYC_MAIL_ATTENTION") {
            if let Ok(depth) = raw.parse::<u32>() {
                let pane = mux.focused_id().get();
                let _ = mux.apply_mail_attention(pane, depth);
            }
        }
        window.set_title(&window_title(
            &mux,
            tab_strip_visible(tab_strip_mode, mux.tab_count(), mux.active_pane_count() > 1),
        ));
        let clipboard = match arboard::Clipboard::new() {
            Ok(clipboard) => Some(clipboard),
            Err(error) => {
                eprintln!("prismattyc-host: native clipboard unavailable: {error}");
                None
            }
        };

        if startup_space.is_some() && attach_sessions.is_empty() {
            mux.empty_space_view(mux.focused_id())?;
        }
        let initial_focus = mux.focused_id();
        let id = window.id();
        let splash = if show_splash {
            let mut splash = splash::Splash::new(self.cli.splash_animation);
            splash.resume = walkthrough::load_progress(&walkthrough::progress_path()).is_some();
            Some(splash)
        } else {
            None
        };
        let walkthrough = None;
        let palette_recent_path = palette_recent_path();
        self.windows.insert(
            id,
            HostState {
                window,
                present: Some(present),
                font,
                font_ligatures: self.file_config.font_ligatures(),
                font_features: self.file_config.font_features(),
                render_timer: self.file_config.render_timer(),
                render_timer_log_every_frame: self.file_config.render_timer_log_every_frame(),
                render_frame: RenderFrame::default(),
                render_window: RenderWindow::default(),
                render_osd: RenderWindowSummary::default(),
                pending_full_repaint: None,
                pane_damage: HashMap::new(),
                last_pane_views: HashMap::new(),
                last_painted_cursor_rows: HashMap::new(),
                last_layout_snapshot: None,
                last_chrome_snapshot: None,
                last_transient_overlay_visible: false,
                last_frame_size: None,
                last_render_log: None,
                mux,
                modifiers: ModifiersState::empty(),
                cursor_cell: None,
                left_button_down: false,
                suppress_left_release: false,
                caption_click: None,
                rich_pointer: None,
                app_mouse_button: None,
                last_app_mouse_cell: None,
                multi_click: MultiClick::default(),
                clipboard,
                theme,
                theme_picker: None,
                palette: None,
                palette_layout: None,
                palette_recent: palette_recent_path
                    .as_deref()
                    .map(palette::load_recent)
                    .unwrap_or_default(),
                palette_recent_path,
                space_picker: None,
                context_menu: None,
                space_panel: None,
                space_polish: Default::default(),
                local_views: Default::default(),
                rail_resizing: false,
                terminal_targets: None,
                terminal_messages: false,
                move_target: None,
                space_team_focus: None,
                team_attention_feed: Default::default(),
                context_menu_target: None,
                space_rail,
                preedit: Preedit::default(),
                find: FindMode::default(),
                splash,
                restore_prompt,
                session_prompt: None,
                e2e_dismiss_at: e2e_dismiss_splash_ms().map(|ms| (Instant::now(), ms)),
                dump_present: dump_present_path(),
                dump_present_seq: 0,
                e2e_second_dump_at: None,
                walkthrough,
                keymap: self.keymap.clone(),
                experimental_rich: self.cli.experimental_rich,
                focus_border: self.cli.focus_border,
                tab_strip_mode,
                pane_titles: self.file_config.pane_titles(),
                spacing,
                pulse_epoch: Instant::now(),
                last_pulse_step: 0,
                window_focused: true,
                window_occluded: false,
                light_cycle: self.cli.light_cycle,
                light_cycle_ms: self.cli.light_cycle_ms,
                light_cycle_head: self.cli.light_cycle_head,
                last_focused: initial_focus,
                border_anim: None,
                border_underlay: Default::default(),
                last_cycle_step: 0,
                visual_bell: self.file_config.visual_bell(),
                pane_visual_bell: self.file_config.pane_visual_bell.unwrap_or(false),
                pane_bells: Default::default(),
                audible_bell: self.file_config.audible_bell(),
                walkthrough_audio: self.file_config.walkthrough_audio(),
                last_walkthrough_sound: None,
                bell_toaster: self.file_config.bell_toaster(),
                bell_toaster_ms: Duration::from_millis(self.file_config.bell_toaster_ms()),
                bell_toasts: Vec::new(),
                drag_toaster: self.file_config.drag_toaster(),
                os_notify_bell: self.file_config.os_notify_bell(),
                attention_sound: self.file_config.attention_sound(),
                attention_badge: self.file_config.attention_badge(),
                os_notify_attention: self.file_config.os_notify_attention(),
                last_attention_notify: Vec::new(),
                bell_flash: None,
                last_bell_notify: None,
                last_bell_sound: None,
                config_error: self.startup_config_error.take(),
                config_path: config_editor.then(config::config_path),
                footer_until: None,
                dirty: true,
                layout_dirty: false,
                tab_rename: None,
                pointer_px: None,
                hover_target: None,
                hyperlink_hover: None,
                hover_blend: self.file_config.hover_blend(),
                scrollbar_drag: None,
                strip_drag: None,
                divider_drag: None,
                divider_cursor: false,
                attach_layout: None,
                attach_layout_path,
                attach_pane_sessions,
                adopted: attach_adopt::Adopted::default(),
                space_opens: space_open::Opens::default(),
                space_open_observation: None,
                last_space_open: None,
                last_space_refresh: None,
                observed_space_sessions: Default::default(),
                attach_cache_stamp: startup_cache_stamp,
                attach_own_stamp: None,
                cache_writer: false,
                background_png: load_background_png(self.file_config.background_image.as_deref()),
                background_opacity: self.file_config.background_opacity(),
                background_blur_px: self.file_config.background_blur_px(),
                pane_opacity_active: self.file_config.pane_opacity_active(),
                overlay_opacity: self.file_config.overlay_opacity(),
                pane_opacity_inactive: self.file_config.pane_opacity_inactive(),
                background: None,
                window_alpha: if alpha_visual {
                    opacity_to_alpha(self.file_config.window_opacity())
                } else {
                    OPAQUE_ALPHA
                },
                chrome_alpha: if alpha_visual {
                    opacity_to_alpha(self.file_config.chrome_opacity())
                } else {
                    OPAQUE_ALPHA
                },
                alpha_visual,
                #[cfg(target_os = "macos")]
                window_blur_active: native_window_blur,
                a11y,
                a11y_announce: self.file_config.a11y_announce(),
                announce_memory: a11y::AnnounceMemory::default(),
                pending_mail_announce: None,
                pending_attention_announce: None,
                pending_selection_announce: None,
                pending_title_announce: None,
                last_mail_depths: BTreeMap::new(),
                last_handle_titles: Vec::new(),
                title_notice: None,
            },
        );
        if let Some(host) = self.windows.get_mut(&id) {
            host.cache_writer = !config_editor;
            host.local_views.fresh = self.file_config.space_startup.as_deref() == Some("fresh");
            if host.restore_prompt.is_some() {
                match self.file_config.space_startup.as_deref() {
                    Some("restore") => restore_prompt::finish(host, true),
                    Some("fresh") => restore_prompt::finish(host, false),
                    _ => {}
                }
            }
            if host.cache_writer {
                persist_attach_layout_from_live(host);
            }
            if !config_editor {
                maybe_space_rail_hint(host);
            }
            if config_editor {
                host.window.focus_window();
            }
        }
        Ok(id)
    }

    fn run_present_paint<B, F>(
        backend: &mut B,
        width: u32,
        height: u32,
        timed: bool,
        paint: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut B, u32, u32) -> Result<()>,
    {
        let started = timed.then(Instant::now);
        paint(backend, width, height)?;
        if let Some(started) = started {
            eprintln!(
                "prismattyc-host: paint {}x{} in {:?}",
                width,
                height,
                started.elapsed()
            );
        }
        Ok(())
    }

    fn finish_paint(host: &mut HostState) {
        host.render_frame.present_succeeded = true;
        if let Some(summary) = host.render_window.record(host.render_frame, Instant::now()) {
            host.render_osd = summary;
        }
        let now = Instant::now();
        if host.render_timer.logs()
            && (host.render_timer_log_every_frame
                || should_log_render_frame(&mut host.last_render_log, now))
        {
            let frame = host.render_frame;
            eprintln!(
                "prismattyc-host: render parse={}us damage={}us raster={}us present={}us cells_painted={} rows_scrolled_as_blit={} full_repaint_reason={} full_repaint_guards={}",
                frame.timing.parse_us,
                frame.timing.damage_us,
                frame.timing.raster_us,
                frame.timing.present_us,
                frame.cells_painted,
                frame.rows_scrolled_as_blit,
                frame.full_repaint_reason.map_or("-", FullRepaintReason::as_str),
                frame.guards,
            );
        }
        host.dirty = false;
    }

    fn paint(host: &mut HostState) -> Result<()> {
        let size = host.window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let mut present = host
            .present
            .take()
            .expect("present backend is installed for a live host");
        let result = Self::run_present_paint(
            &mut present,
            width,
            height,
            gpu_timing_enabled(),
            |present, width, height| present.paint(host, width, height),
        );
        host.present = Some(present);
        if let Err(error) = result {
            // Raster consumes damage and advances view latches. A failed
            // present must retry the entire frame even if no new bytes arrive.
            host.pending_full_repaint = Some(FullRepaintReason::Fallback);
            host.render_frame.present_succeeded = false;
            return Err(error);
        }
        Self::finish_paint(host);
        Ok(())
    }

    fn handle_accesskit(&mut self, event_loop: &ActiveEventLoop, event: accesskit_winit::Event) {
        match event.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested
            | accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                if let Some(host) = self.windows.get_mut(&event.window_id) {
                    publish_a11y(host);
                }
            }
            accesskit_winit::WindowEvent::ActionRequested(request) => {
                if request.action != accesskit::Action::Click {
                    return;
                }
                let outcome = {
                    let Some(host) = self.windows.get_mut(&event.window_id) else {
                        return;
                    };
                    let snap = chrome_snapshot(host, None);
                    match a11y::action_for(request.target_node.0, &snap) {
                        Some(a11y::ChromeAction::SelectTab(index)) => {
                            if host.mux.select_tab(index).unwrap_or(false) {
                                host.window
                                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
                                host.dirty = true;
                                host.window.request_redraw();
                            }
                            None
                        }
                        Some(a11y::ChromeAction::OverlayActivate(index)) => {
                            Some(dispatch_overlay_activate(
                                host,
                                index,
                                &self.cli.program,
                                &self.cli.child_args,
                            ))
                        }
                        Some(a11y::ChromeAction::OpenSpace(index)) => {
                            if let Some(name) = host.space_rail.names.get(index).cloned() {
                                open_space_from_host(host, &name, SpaceOpenMode::Switch);
                            }
                            None
                        }
                        None => None,
                    }
                };
                match outcome {
                    Some(Dispatch::Exit) => event_loop.exit(),
                    Some(Dispatch::OpenWindow) => {
                        let _ = self.event_proxy.send_event(UserAction::NewWindow);
                    }
                    Some(Dispatch::OpenConfig) => {
                        let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                    }
                    Some(Dispatch::Handled) | None => {}
                }
            }
        }
    }
}

fn gpu_timing_enabled() -> bool {
    env_flag_enabled("PRISMATTYC_GPU_TIMING")
}

fn publish_a11y(host: &mut HostState) {
    let live = take_live_announce(host);
    let mut adapter = host.a11y.take();
    if let Some(adapter) = adapter.as_mut() {
        adapter.update_if_active(|| a11y::tree_update(&chrome_snapshot(host, live.clone())));
    }
    host.a11y = adapter;
}

fn announce_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn take_live_announce(host: &mut HostState) -> Option<a11y::LiveSnap> {
    let mail = host.pending_mail_announce.take();
    let attention = host.pending_attention_announce.take();
    let selection = host.pending_selection_announce.take();
    let notice = host.pending_title_announce.take();
    let (cursor_row, cursor_line) = focused_cursor_line(host);
    let pane = host.mux.focused_id().get();
    let facts = a11y::AnnounceFacts {
        enabled: host.a11y_announce,
        now_ms: announce_now_ms(),
        mail,
        attention,
        notice,
        selection,
        cursor_row,
        cursor_line,
        pane,
    };
    let (live, memory) = a11y::announce_decision(&facts, &host.announce_memory);
    host.announce_memory = memory;
    live
}

fn focused_cursor_line(host: &HostState) -> (Option<usize>, String) {
    let pane = host.mux.focused();
    let screen = pane.emulator.screen();
    let scroll = pane.view_scroll.min(screen.max_view_scroll());
    if scroll != 0 {
        return (None, String::new());
    }
    let row = screen.cursor().row;
    let mut line = String::new();
    for col in 0..screen.columns() {
        screen.view_cell(0, row, col).write_grapheme_into(&mut line);
    }
    (Some(row), line.trim_end_matches(' ').to_string())
}

fn queue_selection_announce(host: &mut HostState) {
    if let Some(text) = selected_clipboard_text(&host.selection, host.emulator.screen()) {
        let mut text = text;
        if text.chars().count() > 200 {
            text = text.chars().take(200).collect();
            text.push('…');
        }
        if !text.is_empty() {
            host.pending_selection_announce = Some(text);
        }
    }
}

fn chrome_snapshot(host: &HostState, live: Option<a11y::LiveSnap>) -> a11y::ChromeSnapshot {
    let mails = host.mux.tab_mail_depths();
    let tabs = host
        .mux
        .tab_infos()
        .into_iter()
        .enumerate()
        .map(|(index, info)| {
            let mail = mails.get(index).copied().unwrap_or(0);
            let panes = info
                .handle_titles
                .iter()
                .enumerate()
                .map(|(handle, name)| a11y::PaneSnap {
                    name: a11y::pane_working_name(
                        name,
                        info.handle_active.get(handle).copied().unwrap_or(false),
                    ),
                    focused: info.focused_handle == Some(handle),
                })
                .collect();
            let hover_handle = match host.hover_target {
                Some(HoverTarget::Strip(mux::StripHit::Pane { tab, handle, .. }))
                    if tab == index =>
                {
                    Some(handle)
                }
                _ => None,
            };
            let notice = host.title_notice.as_ref().and_then(|notice| {
                (notice.tab == index && Instant::now() < notice.until)
                    .then_some((notice.handle, notice.title.as_str()))
            });
            let title = title_row::title_row_decision(
                host.pane_titles,
                info.handles,
                info.title.as_str(),
                info.pane_title.as_deref(),
                &info.handle_titles,
                hover_handle,
                notice,
            )
            .label
            .to_string();
            let title = info
                .git_label
                .as_ref()
                .map(|git| format!("{title} · {git}"))
                .unwrap_or(title);
            a11y::TabSnap {
                title,
                selected: info.selected,
                description: a11y::tab_description(mail, info.unseen, info.attention),
                panes,
            }
        })
        .collect();
    let overlay = chrome_overlay(host);
    let scroll = {
        let pane = host.mux.focused();
        let max = pane.emulator.screen().max_view_scroll();
        let scroll = pane.view_scroll.min(max);
        (scroll > 0).then(|| format!("{scroll}/{max}"))
    };
    let rail = if host.mux.geom().rail_side == space_rail::RailSide::Off {
        Vec::new()
    } else {
        host.space_rail.names.clone()
    };
    a11y::ChromeSnapshot {
        window_title: window_title(&host.mux, show_tab_strip(host)),
        tabs,
        overlay,
        scroll,
        rail,
        rail_current: host.space_rail.current_index(),
        document: Some(focused_document(host)),
        live,
        caption: walkthrough_caption_view(host).map(|view| {
            if view.line2.is_empty() {
                view.caption
            } else {
                format!("{} {}", view.caption, view.line2)
            }
        }),
    }
}

/// Focused pane viewport as one document (accessibility D-A4). Scrollback stays
/// out of the live node.
fn focused_document(host: &HostState) -> a11y::DocumentSnap {
    let pane = host.mux.focused();
    let screen = pane.emulator.screen();
    let scroll = pane.view_scroll.min(screen.max_view_scroll());
    let rows = screen.rows();
    let cols = screen.columns();
    let mut lines = Vec::with_capacity(rows);
    let mut widths = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut line = String::new();
        let mut cell_chars = Vec::with_capacity(cols);
        for col in 0..cols {
            let start = line.len();
            screen
                .view_cell(scroll, row, col)
                .write_grapheme_into(&mut line);
            cell_chars.push(line[start..].chars().count());
        }
        lines.push(line);
        widths.push(cell_chars);
    }
    let char_at = |row: usize, col: usize| {
        a11y::chars_before_cell(widths.get(row).map(Vec::as_slice).unwrap_or(&[]), col)
    };
    let cursor = (scroll == 0).then(|| {
        let cursor = screen.cursor();
        (cursor.row, char_at(cursor.row, cursor.column))
    });
    let selection = pane.selection.range().and_then(|range| {
        if rows == 0 {
            return None;
        }
        let first = screen.abs_row_at_view(scroll, 0);
        let last = screen.abs_row_at_view(scroll, rows - 1);
        if range.end_row < first || range.start_row > last {
            return None;
        }
        let start = range.start_row.max(first);
        let end = range.end_row.min(last);
        let start_col = if range.start_row < first {
            0
        } else {
            range.start_col
        };
        let end_col = if range.end_row > last {
            cols.saturating_sub(1)
        } else {
            range.end_col
        };
        let start_row = start - first;
        let end_row = end - first;
        Some((
            (start_row, char_at(start_row, start_col)),
            (end_row, char_at(end_row, end_col)),
        ))
    });
    a11y::viewport_document(&lines, cursor, selection)
}

fn chrome_overlay(host: &HostState) -> a11y::OverlayKind {
    if host.space_panel.is_some() {
        if let Some((header, rows)) = space_panel::rows(host) {
            return a11y::OverlayKind::Palette {
                rows: rows
                    .into_iter()
                    .map(|row| format!("{header}: {} {}", row.name, row.describe))
                    .collect(),
                selected: host.context_menu.as_ref().map_or(0, |menu| menu.selected),
            };
        }
    }
    if let Some(prompt) = &host.session_prompt {
        return session_prompt::accessibility(prompt);
    }
    if let Some(prompt) = &host.restore_prompt {
        return a11y::OverlayKind::RestorePrompt {
            rows: vec!["Restore".into(), "Start fresh".into()],
            selected: prompt.selected,
        };
    }
    if let Some(splash) = host.splash.as_ref() {
        let rows = vec![
            "1. What's new".into(),
            "2. Docs".into(),
            "3. Changelog".into(),
            if splash.resume {
                "4. Resume walkthrough".into()
            } else {
                "4. Walkthrough".into()
            },
        ];
        let selected = match splash.page {
            splash::Page::Main => 0,
            splash::Page::Topic(prismattyc_core::splash::Topic::WhatsNew) => 0,
            splash::Page::Topic(prismattyc_core::splash::Topic::Docs) => 1,
            splash::Page::Topic(prismattyc_core::splash::Topic::Changelog) => 2,
            splash::Page::Topic(prismattyc_core::splash::Topic::Walkthrough) => 3,
        };
        return a11y::OverlayKind::Splash { rows, selected };
    }
    if let Some(palette) = host.palette.as_ref() {
        let view = palette.view(&host.keymap, host.experimental_rich);
        let rows = (0..view.len())
            .filter_map(|index| view.row(index).map(|row| row.name.clone()))
            .collect();
        return a11y::OverlayKind::Palette {
            rows,
            selected: palette.selected,
        };
    }
    if host.find.active {
        return a11y::OverlayKind::Find {
            query: host.find.query.clone(),
        };
    }
    if let Some(picker) = host.theme_picker.as_ref() {
        let items = picker_items(picker.family.as_deref());
        let rows = items
            .iter()
            .map(|item| match item {
                theme::PickerItem::Theme { index } => theme::builtins()
                    .get(*index)
                    .map(|theme| theme.name.clone())
                    .unwrap_or_else(|| format!("theme {index}")),
                theme::PickerItem::Family { label, .. } => label.clone(),
            })
            .collect();
        return a11y::OverlayKind::Theme {
            rows,
            selected: picker.selected.unwrap_or(0),
        };
    }
    if let Some(picker) = host.space_picker.as_ref() {
        let rows = terminal_switcher::rows(host, picker.kind);
        return a11y::OverlayKind::Choices {
            title: if host.terminal_messages && host.terminal_targets.is_some() {
                "Agent messages: queue receipts do not confirm execution"
            } else if host.terminal_targets.is_some() {
                "Find terminal"
            } else {
                "Spaces"
            }
            .into(),
            rows: picker
                .ranked(&rows)
                .iter()
                .map(|row| row.name.clone())
                .collect(),
            selected: picker.selected,
        };
    }
    a11y::OverlayKind::None
}

fn dispatch_overlay_activate(
    host: &mut HostState,
    index: usize,
    program: &str,
    child_args: &[String],
) -> Dispatch {
    if host.space_panel.is_some() {
        space_panel::activate(host, index);
        return Dispatch::Handled;
    }
    if let Some(picker) = host.space_picker.as_ref() {
        let kind = picker.kind;
        let rows = terminal_switcher::rows(host, kind);
        let picker = host.space_picker.as_mut().unwrap();
        if index < picker.ranked(&rows).len() {
            picker.selected = index;
            let verdict = picker.key(&Key::Named(NamedKey::Enter), ModifiersState::empty(), &rows);
            apply_space_picker_verdict(host, kind, verdict);
        }
        return Dispatch::Handled;
    }
    if host.session_prompt.is_some() {
        session_prompt::activate(host, index);
        return Dispatch::Handled;
    }
    if host.restore_prompt.is_some() {
        if index < 2 {
            restore_prompt::finish(host, index == 0);
        }
        return Dispatch::Handled;
    }
    if let Some(splash) = host.splash.as_mut() {
        if index == 3 {
            start_walkthrough(host);
            host.window.request_redraw();
            return Dispatch::Handled;
        }
        let topic = match index {
            1 => prismattyc_core::splash::Topic::Docs,
            2 => prismattyc_core::splash::Topic::Changelog,
            _ => prismattyc_core::splash::Topic::WhatsNew,
        };
        splash.page = splash::Page::Topic(topic);
        host.dirty = true;
        host.window.request_redraw();
        return Dispatch::Handled;
    }
    if let Some(palette) = host.palette.as_ref() {
        let view = palette.view(&host.keymap, host.experimental_rich);
        if let Some(row) = view.row(index) {
            if let palette::PaletteEntry::Action(action) = row.entry {
                host.palette = None;
                host.palette_layout = None;
                host.window
                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
                host.dirty = true;
                host.window.request_redraw();
                return dispatch_action(host, action, program, child_args);
            }
        }
    }
    Dispatch::Handled
}

fn open_present_backend(
    window: Arc<Window>,
    want_gpu: bool,
    want_alpha: bool,
    want_blur: bool,
) -> Result<PresentBackend> {
    #[cfg(target_os = "linux")]
    let wayland = wayland_shm::is_wayland(&window);
    #[cfg(not(target_os = "linux"))]
    let wayland = false;

    if want_gpu {
        #[cfg(feature = "gpu")]
        {
            match gpu::GpuPresent::try_init(window.clone()) {
                Ok(gpu) => return Ok(PresentBackend::Gpu(Box::new(gpu))),
                Err(error) => {
                    eprintln!(
                        "prismattyc-host: GPU init failed ({error:#}); falling back to softbuffer"
                    );
                }
            }
        }
        return PresentBackend::softbuffer(window, wayland);
    }
    // PT-118: on native Wayland softbuffer presents XRGB and drops alpha, so
    // a transparency config needs our own ARGB8888 wl_shm present. On init
    // failure fall back to softbuffer; the startup notice then tells the
    // user why window_opacity is ignored.
    #[cfg(target_os = "linux")]
    if use_wayland_shm(want_alpha, wayland) {
        match wayland_shm::WaylandShmPresent::try_init(&window, want_blur) {
            Ok(shm) => {
                let blur = if shm.blur_active() { " + blur" } else { "" };
                eprintln!("prismattyc-host: wayland shm present (ARGB8888){blur}");
                return Ok(PresentBackend::WaylandShm(Box::new(shm)));
            }
            Err(error) => {
                eprintln!(
                    "prismattyc-host: wayland shm init failed ({error:#}); \
                     falling back to softbuffer"
                );
            }
        }
    }
    let _ = (want_alpha, want_blur);
    #[cfg(target_os = "macos")]
    {
        let mac = mac_present::MacPresent::new(window)?;
        eprintln!("prismattyc-host: Core Animation present (premultiplied ARGB)");
        Ok(PresentBackend::Mac(Box::new(mac)))
    }
    #[cfg(not(target_os = "macos"))]
    PresentBackend::softbuffer(window, wayland)
}

/// Which surface is responsible for providing the requested blur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlurSurface {
    Macos,
    Compositor,
}

/// Decide whether a requested blur needs an unsupported notice.
///
/// Keep platform capability and native-view state as plain inputs. The caller
/// owns AppKit and compositor side effects.
fn blur_notice(wanted: bool, surface: BlurSurface, installed: bool) -> bool {
    if !wanted || installed {
        return false;
    }
    match surface {
        BlurSurface::Macos | BlurSurface::Compositor => true,
    }
}

/// Should the window be created with an alpha visual?
///
/// Only when the config actually asks for transparency or compositor blur, so
/// the default path keeps the plain opaque visual it has always had.
fn wants_alpha_visual(config: &config::ConfigFile) -> bool {
    config.window_blur() || config.window_opacity() < 1.0 || config.chrome_opacity() < 1.0
}

/// Printed once at startup when `window_opacity` cannot be honoured.
#[cfg(target_os = "macos")]
const ALPHA_UNSUPPORTED_NOTICE: &str =
    "window_opacity ignored: this present path cannot carry per-pixel alpha on macOS.";
#[cfg(not(target_os = "macos"))]
const ALPHA_UNSUPPORTED_NOTICE: &str = "window_opacity ignored: this present path cannot carry \
     alpha. On Wayland the ARGB8888 shm present failed to initialize; on \
     X11 a compositor with a depth-32 visual is required.";

/// Printed when `window_blur = true` but no blur surface is live.
#[cfg(target_os = "macos")]
const BLUR_UNSUPPORTED_NOTICE: &str =
    "window_blur ignored: the macOS NSVisualEffectView could not be installed.";
#[cfg(not(target_os = "macos"))]
const BLUR_UNSUPPORTED_NOTICE: &str = "window_blur ignored: no compositor blur protocol is \
     reachable from this session. On KWin 6.7+ blur uses \
     ext-background-effect-v1; on Hyprland use a windowrule (see \
     docs/hyprland.md).";

fn load_background_png(path: Option<&Path>) -> Option<Vec<u8>> {
    let path = path?;
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            eprintln!(
                "prismattyc-host: background_image {}: {error}",
                path.display()
            );
            None
        }
    }
}

fn copy_background_layer(
    buffer: &mut [u32],
    cache: &BgCache,
    rewrite_alpha: bool,
    window_alpha: u8,
) -> bool {
    if cache.px.len() != buffer.len() {
        return false;
    }
    buffer.copy_from_slice(&cache.px);
    if rewrite_alpha {
        let alpha = u32::from(window_alpha) << 24;
        for px in buffer.iter_mut() {
            *px = (*px & 0x00ff_ffff) | alpha;
        }
    }
    true
}

fn render_cells_painted(host: &HostState) -> u64 {
    host.mux
        .active_pane_ids()
        .into_iter()
        .filter_map(|pane| host.mux.pane(pane))
        .map(|pane| {
            let screen = pane.emulator.screen();
            (screen.rows() as u64).saturating_mul(screen.columns() as u64)
        })
        .sum()
}

fn should_log_render_frame(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
        *last = Some(now);
        true
    } else {
        false
    }
}

fn current_full_repaint_reason(
    host: &mut HostState,
    width: u32,
    height: u32,
    overflowed: bool,
) -> Option<FullRepaintReason> {
    let resized = host.last_frame_size != Some((width, height));
    host.last_frame_size = Some((width, height));
    let active_panes = host.mux.active_pane_ids();
    host.last_pane_views
        .retain(|id, _| active_panes.contains(id));
    let mut alt_screen = false;
    let mut scrollback_view = false;
    for id in active_panes {
        let Some(pane) = host.mux.pane(id) else {
            continue;
        };
        let current = (pane.emulator.screen().alt_active(), pane.view_scroll);
        let previous = host.last_pane_views.insert(id, current);
        let damaged = host
            .pane_damage
            .get(&id)
            .is_some_and(pane_damage_requires_repaint);
        let (alt_changed, scroll_changed) = pane_view_repaint(previous, current, damaged);
        alt_screen |= alt_changed;
        scrollback_view |= scroll_changed;
    }
    use render_diagnostics::Guard;
    host.render_frame.guards = render_diagnostics::GuardMask::default();
    let guards = &mut host.render_frame.guards;
    guards.set(Guard::Pending, host.pending_full_repaint.is_some());
    guards.set(Guard::Resize, resized);
    guards.set(Guard::AltScreen, alt_screen);
    guards.set(Guard::Scrollback, scrollback_view);
    guards.set(Guard::ScrollOverflow, overflowed);
    host.pending_full_repaint.take().or_else(|| {
        resized
            .then_some(FullRepaintReason::Resize)
            .or_else(|| alt_screen.then_some(FullRepaintReason::AltScreen))
            .or_else(|| scrollback_view.then_some(FullRepaintReason::Scrollback))
            .or_else(|| overflowed.then_some(FullRepaintReason::Overflow))
    })
}

/// Live grid row indices cannot describe an offset scrollback viewport.
/// Idle scrollback needs no work; output and offset changes need a full frame.
fn pane_view_repaint(
    previous: Option<(bool, usize)>,
    current: (bool, usize),
    damaged: bool,
) -> (bool, bool) {
    let alt = previous.map_or(current.0, |old| old.0 != current.0);
    let scroll =
        previous.map_or(current.1 > 0, |old| old.1 != current.1) || (current.1 > 0 && damaged);
    (alt, scroll)
}

fn pane_damage_requires_repaint(damage: &GridDamage) -> bool {
    damage.dirty_row_count() > 0 || !damage.scroll_events().is_empty() || damage.scroll_overflowed()
}

fn damage_rows(damage: &GridDamage) -> Vec<usize> {
    let mut rows: Vec<usize> = damage.dirty_rows().collect();
    for event in damage.scroll_events() {
        let top = event.top.min(damage.rows());
        let bottom = event.bottom.min(damage.rows().saturating_sub(1));
        if top <= bottom {
            rows.extend(top..=bottom);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

fn damage_painted_cells(damage: &GridDamage, rows: &[usize]) -> u64 {
    if damage.scroll_events().is_empty() && damage.dirty_cell_count() > 0 {
        damage.dirty_cell_count() as u64
    } else {
        rows.len() as u64 * damage.columns() as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FramebufferScrollRect {
    x: usize,
    y: usize,
    width: usize,
    row_height: usize,
    rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramebufferScrollDirection {
    Forward,
    Reverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FramebufferScrollCopy {
    src_y: usize,
    dst_y: usize,
    scanlines: usize,
    direction: FramebufferScrollDirection,
}

fn framebuffer_scroll_plan(
    rect: FramebufferScrollRect,
    stride: usize,
    frame_height: usize,
    buffer_len: usize,
    events: &[ScrollDamage],
) -> Option<(Vec<FramebufferScrollCopy>, u64)> {
    if rect.width == 0 || rect.row_height == 0 || rect.rows == 0 {
        return None;
    }
    if rect.x.checked_add(rect.width)? > stride
        || rect
            .y
            .checked_add(rect.rows.checked_mul(rect.row_height)?)?
            > frame_height
        || stride.checked_mul(frame_height)? > buffer_len
    {
        return None;
    }

    let mut copies = Vec::with_capacity(events.len());
    let mut copied_rows = 0u64;
    let last = rect.rows - 1;
    for event in events {
        if event.delta == 0 || event.bottom < event.top {
            continue;
        }
        let top = event.top.min(last);
        let bottom = event.bottom.min(last);
        if bottom < top {
            continue;
        }
        let height = scroll_span(top, bottom)?;
        let amount = (event.delta.unsigned_abs() as usize).min(height);
        if amount >= height {
            return None;
        }
        let surviving = height - amount;
        let (src_row, dst_row, direction) = if event.delta.is_positive() {
            (top + amount, top, FramebufferScrollDirection::Forward)
        } else {
            (top, top + amount, FramebufferScrollDirection::Reverse)
        };
        let src_y = rect.y.checked_add(src_row.checked_mul(rect.row_height)?)?;
        let dst_y = rect.y.checked_add(dst_row.checked_mul(rect.row_height)?)?;
        let scanlines = surviving.checked_mul(rect.row_height)?;
        src_y.checked_add(scanlines)?;
        dst_y.checked_add(scanlines)?;
        copies.push(FramebufferScrollCopy {
            src_y,
            dst_y,
            scanlines,
            direction,
        });
        copied_rows = copied_rows.saturating_add(surviving as u64);
    }
    Some((copies, copied_rows))
}

fn apply_framebuffer_scroll_blits(
    buffer: &mut [u32],
    stride: usize,
    frame_height: usize,
    rect: FramebufferScrollRect,
    events: &[ScrollDamage],
    chrome_boxes: &[PixelRect],
) -> Option<u64> {
    let (copies, copied_rows) =
        framebuffer_scroll_plan(rect, stride, frame_height, buffer.len(), events)?;
    if copies.iter().any(|copy| {
        chrome_boxes.iter().any(|chrome| {
            chrome.width > 0
                && chrome.height > 0
                && chrome.x < rect.x + rect.width
                && rect.x < chrome.x.saturating_add(chrome.width)
                && chrome.y < copy.src_y + copy.scanlines
                && copy.src_y < chrome.y.saturating_add(chrome.height)
        })
    }) {
        return None;
    }
    for copy in copies {
        for offset in 0..copy.scanlines {
            let line = match copy.direction {
                FramebufferScrollDirection::Forward => offset,
                FramebufferScrollDirection::Reverse => copy.scanlines - 1 - offset,
            };
            let src = (copy.src_y + line) * stride + rect.x;
            let dst = (copy.dst_y + line) * stride + rect.x;
            buffer.copy_within(src..src + rect.width, dst);
        }
    }
    Some(copied_rows)
}

fn scroll_span(top: usize, bottom: usize) -> Option<usize> {
    bottom.checked_sub(top)?.checked_add(1)
}

fn row_after_scrolls(mut row: usize, rows: usize, events: &[ScrollDamage]) -> Option<usize> {
    if rows == 0 || row >= rows {
        return None;
    }
    let last = rows - 1;
    for event in events {
        if event.delta == 0 || event.bottom < event.top {
            continue;
        }
        let top = event.top.min(last);
        let bottom = event.bottom.min(last);
        if row < top || row > bottom || bottom < top {
            continue;
        }
        let height = scroll_span(top, bottom)?;
        let amount = (event.delta.unsigned_abs() as usize).min(height);
        if amount >= height {
            return None;
        }
        if event.delta.is_positive() {
            if row < top + amount {
                return None;
            }
            row -= amount;
        } else {
            if row > bottom - amount {
                return None;
            }
            row += amount;
        }
    }
    Some(row)
}

fn insert_paint_row(rows: &mut Vec<usize>, row: usize, screen_rows: usize) {
    if row < screen_rows {
        rows.push(row);
        rows.sort_unstable();
        rows.dedup();
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PartialRasterBlockers {
    partial_disallowed: bool,
    osd: bool,
    background: bool,
    bell_flash: bool,
    layout_transition: bool,
    transient_overlay: bool,
    unsupported_pane_content: bool,
}

fn unsupported_pane_content(guards: render_diagnostics::GuardMask) -> bool {
    guards.contains(render_diagnostics::Guard::WorkspaceLayout)
        || guards.contains(render_diagnostics::Guard::ExperimentalRich)
        || guards.contains(render_diagnostics::Guard::StoredImages)
}

fn partial_raster_blocked(blockers: PartialRasterBlockers) -> bool {
    blockers.partial_disallowed
        || blockers.osd
        || blockers.background
        || blockers.bell_flash
        || blockers.layout_transition
        || blockers.transient_overlay
        || blockers.unsupported_pane_content
}

fn damage_is_empty(damage: &GridDamage) -> bool {
    damage.dirty_row_count() == 0 && damage.scroll_events().is_empty()
}

#[derive(Debug, Clone, Copy)]
struct ScrollBlitEligibility {
    damage_rows: usize,
    screen_rows: usize,
    damage_columns: usize,
    screen_columns: usize,
    pixel_width: usize,
    content_width: usize,
    pixel_height: usize,
    guest_height: usize,
    selection_active: bool,
    scroll_events_empty: bool,
}

fn scroll_blit_eligible(input: ScrollBlitEligibility) -> bool {
    if input.damage_rows != input.screen_rows {
        return false;
    }
    if input.damage_columns != input.screen_columns {
        return false;
    }
    if input.pixel_width > input.content_width {
        return false;
    }
    if input.pixel_height > input.guest_height {
        return false;
    }
    if input.selection_active {
        return false;
    }
    !input.scroll_events_empty
}

fn blit_was_applied(copied_rows: u64) -> bool {
    copied_rows != 0
}

fn painted_cells_for_rows(damage: &GridDamage, rows: &[usize]) -> u64 {
    if rows.is_empty() {
        0
    } else {
        damage_painted_cells(damage, rows)
    }
}

fn should_record_cursor_row(full: bool, blit_applied: bool, cursor_row_painted: bool) -> bool {
    full || blit_applied || cursor_row_painted
}

/// Inputs that can leave host-drawn transient pixels in the framebuffer.
#[derive(Debug, Clone, Copy, Default)]
struct TransientOverlayState {
    footer_visible: bool,
    splash: bool,
    restore_prompt: bool,
    palette: bool,
    theme_picker: bool,
    space_picker: bool,
    context_menu: bool,
    find_active: bool,
    tab_rename: bool,
    walkthrough: bool,
    drag_toast: bool,
    config_path: bool,
    config_error: bool,
    preedit: bool,
    bell_toasts: bool,
    title_notice: bool,
    hover_target: bool,
    /// `+` / save_space modal. The rail chip stays `+`, so chrome snapshots
    /// do not see the typed name (#364).
    save_space: bool,
}

/// Return whether host-drawn transient pixels are present in this frame.
///
/// Partial raster preserves the previous framebuffer, so every transient
/// painter must force a full frame while it is visible. The live drag toast
/// is gated before this pure predicate by its active drag label.
fn transient_overlay_visible(state: TransientOverlayState) -> bool {
    state.footer_visible
        || state.splash
        || state.restore_prompt
        || state.palette
        || state.theme_picker
        || state.space_picker
        || state.context_menu
        || state.find_active
        || state.tab_rename
        || state.walkthrough
        || state.drag_toast
        || state.config_path
        || state.config_error
        || state.preedit
        || state.bell_toasts
        || state.title_notice
        || state.hover_target
        || state.save_space
}

/// True when the `+` chip / `save_space` name prompt is the host-drawn overlay.
///
/// The rail's `+` chip stays a compact glyph. Chrome snapshots therefore do
/// not see the typed name. Partial raster must treat this modal like the
/// command palette: force a full frame while it is visible.
fn save_space_modal_open(edit: Option<&space_rail::RailEdit>) -> bool {
    edit.is_some_and(|edit| edit.target.is_none())
}

fn overlay_requires_full_repaint(previously_visible: bool, visible: bool) -> bool {
    previously_visible || visible
}

fn layout_snapshot(mux: &mux::MuxRuntime, geom: mux::HostGeom) -> LayoutSnapshot {
    LayoutSnapshot {
        panes: mux
            .panes_and_rects()
            .map(|(id, _pane, rect)| {
                let (sx, sy, sw, sh) = geom.pane_slot_px(rect);
                let (cx, cy, cw, ch) = geom.pane_content_px(rect);
                PaneLayoutSnapshot {
                    id: id.get(),
                    slot: PixelRect::new(sx, sy, sw, sh),
                    content: PixelRect::new(cx, cy, cw, ch),
                }
            })
            .collect(),
    }
}

fn append_tab_strip_chrome(
    host: &HostState,
    geom: mux::HostGeom,
    boxes: &mut Vec<PixelRect>,
    markers: &mut Vec<(u64, u64)>,
) {
    if !tab_strip_visible_for_damage(show_tab_strip(host), geom.top_chrome_px) {
        return;
    }
    let width = host.window.inner_size().width as usize;
    let bar_h = geom.top_chrome_px;
    let tabs = host.mux.tab_infos();
    let inner_stride = tab_strip_inner_stride(width, host.font.cell_w, reserve_strip_end(host));
    let title_h = host.font.cell_h.max(1);
    let badge_y = mux::tab_badge_top(title_h.min(bar_h));
    let badge_span = mux::TAB_BADGE_SIZE.saturating_mul(3).saturating_add(4);
    let boxes_start = boxes.len();
    for (tab_index, tab) in tabs.iter().enumerate() {
        let Some((x0, slot_width)) = mux::tab_slot_bounds(
            tab_index,
            tabs.len(),
            inner_stride,
            geom.window_pad,
            geom.rail_gap,
        ) else {
            continue;
        };
        append_tab_strip_markers(tab_index, tab.active, &tab.handle_active, markers);
        push_tab_strip_handle_boxes(
            TabStripHandlePlan {
                x0,
                slot_width,
                inner_pad: geom.inner_pad,
                bar_h,
                title_h,
                handle_w: mux::pane_handle_w(host.font.cell_w.max(1)),
                handles: tab.handles,
            },
            boxes,
        );
        boxes.push(tab_strip_badge_box(
            x0,
            slot_width,
            mux::tab_close_left_with_inset(x0, slot_width, host.font.cell_w.max(1), geom.inner_pad),
            badge_span,
            badge_y,
            bar_h,
            mux::TAB_BADGE_SIZE,
        ));
    }
    for rect in &mut boxes[boxes_start..] {
        rect.y = rect.y.saturating_add(geom.tab_strip_y());
    }
}

fn frame_chrome_snapshot(
    host: &HostState,
    geom: mux::HostGeom,
    focused: PaneId,
    pulse_step: Option<u8>,
    light_cycle_step: Option<u8>,
    now: Instant,
) -> ChromeSnapshot {
    let mux = &host.mux;
    let mut focused_slot = None;
    let mut boxes = Vec::new();
    let mut markers = Vec::new();
    let multi_pane = mux.pane_count() > 1;
    for (id, pane, rect) in mux.panes_and_rects() {
        let (x, y, width, height) = geom.pane_slot_px(rect);
        let slot = PixelRect::new(x, y, width, height);
        if id == focused {
            focused_slot = Some(slot);
        }
        let (content_x, content_y, content_w, content_h) = geom.pane_content_px(rect);
        let content = PixelRect::new(content_x, content_y, content_w, content_h);
        let active = multi_pane
            && width >= 10
            && height >= 10
            && pane.is_active_at(now)
            && pulse_live(host.window_focused, host.window_occluded);
        let unseen = multi_pane && width >= 24 && height >= 10 && pane.unseen_output;
        push_pane_chrome_boxes(
            slot,
            content,
            multi_pane,
            pane.mail_depth > 0,
            unseen,
            active,
            &mut boxes,
        );
        let max_scroll = pane.emulator.screen().max_view_scroll();
        let scroll = pane.view_scroll.min(max_scroll);
        if should_record_scrollbar_box(max_scroll) {
            let (bar_x, bar_y, bar_w, bar_h) = geom.scrollbar_px(rect);
            boxes.push(PixelRect::new(bar_x, bar_y, bar_w, bar_h));
        }
        // Keep scrollbar state in the bounded marker path. The full signature
        // must not change for ordinary scrollback growth or thumb movement.
        markers.push(pane_marker_word(
            id.get(),
            pane_chrome_bits(pane.mail_depth > 0, unseen, active),
            scrollbar_marker(max_scroll, scroll),
        ));
    }
    append_tab_strip_chrome(host, geom, &mut boxes, &mut markers);
    ChromeSnapshot {
        focused_pane: Some(focused.get()),
        focused_slot,
        boxes,
        markers,
        pulse_step,
        light_cycle_step,
        unbounded_state: unbounded_chrome_state(host, geom),
    }
}

fn unbounded_chrome_state(host: &HostState, geom: mux::HostGeom) -> String {
    let tabs: Vec<_> = host
        .mux
        .tab_infos()
        .into_iter()
        .map(|tab| {
            (
                tab.title,
                tab.selected,
                tab.unseen,
                tab.attention,
                tab.zoomed,
                tab.handles,
                tab.focused_handle,
                tab.handle_titles,
                tab.pane_title,
                tab.git_label,
            )
        })
        .collect();
    let rail: Vec<_> = host
        .space_rail
        .views()
        .into_iter()
        .map(|view| {
            (
                view.label,
                view.pane_names,
                view.current,
                view.focused,
                view.editing,
                view.confirm,
                view.plus,
            )
        })
        .collect();
    let mut panes: Vec<_> = host
        .mux
        .panes_and_rects()
        .map(|(id, pane, _rect)| {
            let selection = pane.selection.range().map(|range| {
                (
                    range.start_row,
                    range.start_col,
                    range.end_row,
                    range.end_col,
                )
            });
            (id.get(), selection)
        })
        .collect();
    panes.sort_unstable_by_key(|(id, ..)| *id);
    format!(
        "strip={} attention={} rail_side={:?} tabs={tabs:?} rail={rail:?} panes={panes:?}",
        show_tab_strip(host),
        host.attention_badge,
        geom.rail_side,
    )
}

struct FrameDamageSnapshot {
    layout: LayoutSnapshot,
    chrome: ChromeSnapshot,
    panes: Vec<PaneDamageSnapshot>,
    paint_rows: HashMap<PaneId, Vec<usize>>,
    layout_changed: bool,
    chrome_changed: bool,
}

fn frame_damage_snapshot(
    host: &HostState,
    geom: mux::HostGeom,
    focused: PaneId,
    now: Instant,
) -> FrameDamageSnapshot {
    let layout = layout_snapshot(&host.mux, geom);
    let pulse_step = pulse_step_for_snapshot(
        host.mux.active_count(),
        pulse_live(host.window_focused, host.window_occluded),
        host.last_pulse_step,
    );
    let light_cycle_step =
        light_cycle_step_for_snapshot(host.border_anim.is_some(), host.last_cycle_step);
    let chrome = frame_chrome_snapshot(host, geom, focused, pulse_step, light_cycle_step, now);
    let layout_changed = layout_transition(host.last_layout_snapshot.as_ref(), &layout);
    let chrome_changed = chrome_geometry_changed(host.last_chrome_snapshot.as_ref(), &chrome);
    let prior_focus = host
        .last_chrome_snapshot
        .as_ref()
        .and_then(|chrome| chrome.focused_pane);
    let focus_changed = prior_focus != Some(focused.get());
    let panes: Vec<PaneDamageSnapshot> = host
        .mux
        .panes_and_rects()
        .map(|(pane_id, pane, rect)| {
            let (content_x, content_y, content_w, content_h) = geom.pane_content_px(rect);
            let content = PixelRect::new(content_x, content_y, content_w, content_h);
            let prior_content = host
                .last_layout_snapshot
                .as_ref()
                .and_then(|prior| prior.panes.iter().find(|old| old.id == pane_id.get()))
                .map(|old| old.content);
            let assignment_changed = assignment_changed_content(prior_content, content);
            let existing = host
                .pane_damage
                .get(&pane_id)
                .map(damage_rows)
                .unwrap_or_default();
            let dirty_rows = snapshot_dirty_rows(
                assignment_changed,
                pane.emulator.screen().rows(),
                &existing,
                focus_affects_pane(focus_changed, prior_focus, pane_id.get(), focused.get()),
                pane.emulator.screen().cursor().row,
                host.last_painted_cursor_rows
                    .get(&pane_id)
                    .copied()
                    .flatten(),
            );
            PaneDamageSnapshot {
                pane_id: pane_id.get(),
                content,
                row_height: host.font.cell_h,
                dirty_rows,
                blit: None,
            }
        })
        .collect();
    let paint_rows = panes
        .iter()
        .filter_map(|pane| {
            host.mux
                .active_pane_ids()
                .into_iter()
                .find(|id| id.get() == pane.pane_id)
                .map(|id| (id, pane.dirty_rows.clone()))
        })
        .collect();
    FrameDamageSnapshot {
        layout,
        chrome,
        panes,
        paint_rows,
        layout_changed,
        chrome_changed,
    }
}

fn compose_host_frame_damage(
    host: &mut HostState,
    snapshot: FrameDamageSnapshot,
    full: bool,
) -> (FrameDamage, HashMap<PaneId, Vec<usize>>) {
    let damage = compose_frame_damage(
        host.last_layout_snapshot.as_ref(),
        &snapshot.layout,
        host.last_chrome_snapshot.as_ref(),
        &snapshot.chrome,
        &snapshot.panes,
        full,
    );
    host.last_layout_snapshot = Some(snapshot.layout);
    host.last_chrome_snapshot = Some(snapshot.chrome);
    (damage, snapshot.paint_rows)
}

/// Bound pending bell state to the current visible owner and geometry.
fn settle_pane_bells(host: &mut HostState, now: Instant) {
    if host.pane_bells.is_empty() {
        return;
    }
    if host.window_occluded || !host.visual_bell || !host.pane_visual_bell {
        host.dirty |= host.pane_bells.cancel();
        return;
    }
    let mux = &host.mux;
    let geom = mux.geom();
    host.dirty |= host.pane_bells.settle(now, mux.space_id.as_deref(), |id| {
        mux.panes_and_rects().find_map(|(pane, runtime, rect)| {
            (pane.get() == id).then(|| {
                let (x, y, w, h) = geom.pane_slot_px(rect);
                (runtime.view_identity(), PixelRect::new(x, y, w, h))
            })
        })
    });
}

fn rasterize_frame(
    host: &mut HostState,
    buffer: &mut [u32],
    width: u32,
    height: u32,
    partial_allowed: bool,
) -> FrameDamage {
    host.render_frame.raster_at_unix_ms = Some(prismattyc_mux::host_render_status::unix_ms());
    host.render_frame.present_succeeded = false;
    host.pane_damage = host.mux.take_pane_damage();
    let overflowed = host.pane_damage.values().any(GridDamage::scroll_overflowed);
    let mut reason = current_full_repaint_reason(host, width, height, overflowed);
    let geom = host.mux.geom();
    let focused = host.mux.focused_id();
    let frame_now = Instant::now();
    settle_pane_bells(host, frame_now);
    let damage_snapshot = frame_damage_snapshot(host, geom, focused, frame_now);
    let layout_changed = damage_snapshot.layout_changed;
    let chrome_changed = damage_snapshot.chrome_changed;
    let strip_changed = host
        .last_chrome_snapshot
        .as_ref()
        .is_some_and(|prior| strip_chrome_changed(prior, &damage_snapshot.chrome));
    let footer_visible = footer_visibility(
        host.modifiers.control_key(),
        host.modifiers.shift_key(),
        host.footer_until,
        Instant::now(),
    );
    let overlay_state = TransientOverlayState {
        footer_visible,
        splash: host.splash.is_some(),
        restore_prompt: host.restore_prompt.is_some() || host.session_prompt.is_some(),
        palette: host.palette.is_some(),
        theme_picker: host.theme_picker.is_some(),
        space_picker: host.space_picker.is_some(),
        context_menu: host.context_menu.is_some(),
        find_active: host.find.active,
        tab_rename: host.tab_rename.is_some(),
        walkthrough: host.walkthrough.is_some(),
        drag_toast: host.drag_toaster && drag_toast_label(host, width as usize).is_some(),
        config_path: host.config_path.is_some(),
        config_error: host.config_error.is_some(),
        preedit: !host.preedit.text.is_empty(),
        bell_toasts: !host.bell_toasts.is_empty() || git_hover_label(host).is_some(),
        title_notice: host.title_notice.is_some(),
        hover_target: host.hover_target.is_some(),
        save_space: save_space_modal_open(host.space_rail.edit.as_ref()),
    };
    render_diagnostics::record_overlay_guards(&mut host.render_frame.guards, overlay_state);
    let transient_overlay = transient_overlay_visible(overlay_state);
    let overlay_requires_full =
        overlay_requires_full_repaint(host.last_transient_overlay_visible, transient_overlay);
    host.render_frame.guards.set(
        render_diagnostics::Guard::PreviousOverlay,
        host.last_transient_overlay_visible,
    );
    host.last_transient_overlay_visible = transient_overlay;
    use render_diagnostics::Guard;
    let guards = &mut host.render_frame.guards;
    let osd_visible = host.render_timer.shows_osd();
    guards.set(Guard::BackgroundImage, host.background_png.is_some());
    guards.set(Guard::LayoutTransition, layout_changed);
    guards.set(Guard::ChromeGeometry, chrome_changed);
    for pane_id in host.mux.active_pane_ids() {
        if let Some(pane) = host.mux.pane(pane_id) {
            guards.add(Guard::WorkspaceLayout, pane.workspace_layout().is_some());
            guards.add(Guard::ExperimentalRich, pane.experimental_rich());
            guards.add(Guard::StoredImages, !pane.emulator.images().is_empty());
        }
    }
    let blockers = PartialRasterBlockers {
        partial_disallowed: !partial_allowed && !osd_visible,
        osd: osd_visible,
        background: host.background_png.is_some(),
        bell_flash: host.bell_flash.is_some(),
        layout_transition: layout_changed,
        transient_overlay: overlay_requires_full,
        unsupported_pane_content: unsupported_pane_content(*guards),
    };
    guards.set(Guard::Backend, blockers.partial_disallowed);
    guards.set(Guard::Osd, blockers.osd);
    guards.set(Guard::BellFlash, blockers.bell_flash);
    let unsupported_partial = partial_raster_blocked(blockers);
    if reason.is_none() && unsupported_partial {
        reason = Some(FullRepaintReason::Fallback);
    }
    host.render_frame.full_repaint_reason = reason;
    let mut full = reason.is_some();
    host.render_frame.cells_painted = if full { render_cells_painted(host) } else { 0 };
    host.render_frame.rows_scrolled_as_blit = 0;
    let (mut frame_damage, composed_paint_rows) =
        compose_host_frame_damage(host, damage_snapshot, full);
    if composer_promotes_to_full(full, &frame_damage) {
        full = true;
        reason = Some(FullRepaintReason::Fallback);
        host.render_frame.full_repaint_reason = reason;
        host.render_frame.cells_painted = render_cells_painted(host);
    }
    // A sweep may begin and finish between chrome snapshots. Its retained
    // underlay independently carries cleanup damage until the next paint.
    host.pane_bells
        .restore(buffer, width as usize, &mut frame_damage);
    host.border_underlay
        .restore(buffer, width as usize, &mut frame_damage);
    if empty_partial_skips_paint(
        full,
        &frame_damage,
        host.pane_damage.values().all(damage_is_empty),
    ) {
        return frame_damage;
    }
    let overlay_surface = host_overlay_surface(host);
    if host.find.active && host.emulator.screen().alt_active() {
        close_find(&mut host.find);
    }
    if let Some(picker) = host.theme_picker.as_mut() {
        let count = picker_items(picker.family.as_deref()).len();
        let visible_rows =
            theme_picker_visible_rows(&host.font, count, width as usize, height as usize);
        picker.scroll =
            theme_picker_scroll_for_selection(picker.scroll, picker.selected, visible_rows, count);
    }
    if full {
        // Clear full window then paint each pane's inset content and outer slot.
        // The window *frame* (the band outside the pane area) is `pane_backdrop`,
        // a slight shade of `default_bg`. Everything inside that frame — pane
        // gaps and the padding ring around each terminal — starts as `default_bg`.
        // Each pane slot is repainted below at that pane's surface alpha, while
        // shared gaps keep `window_alpha`. `pane_backdrop` is painted again under
        // each content rect as the blend target a dimmed pane recedes toward
        // (PT-98). PT-87: the ground carries `window_alpha`.
        let bg = pack_argb(host.window_alpha, host.theme.pane_backdrop);
        let cache_meta = host.background.as_ref().map(|cache| BackgroundCacheMeta {
            width: cache.w,
            height: cache.h,
            pixel_len: cache.px.len(),
        });
        let rewrite_alpha = host.window_alpha != OPAQUE_ALPHA;
        let decision = background_decision(
            cache_meta,
            width,
            height,
            host.background_png.is_some(),
            rewrite_alpha,
        );
        if matches!(decision, BackgroundDecision::Rebuild) {
            host.background = None;
            if let Some(png) = host.background_png.as_deref() {
                host.background = build_background_layer(
                    png,
                    width as usize,
                    height as usize,
                    host.theme.default_bg,
                    host.background_opacity,
                    host.background_blur_px,
                )
                .map(|px| BgCache {
                    w: width,
                    h: height,
                    px,
                });
            }
        } else if matches!(decision, BackgroundDecision::Fill)
            && cache_meta.is_some_and(|cache| cache.width != width || cache.height != height)
        {
            host.background = None;
        }
        match decision {
            BackgroundDecision::Copy { rewrite_alpha } => {
                let copied = host.background.as_ref().is_some_and(|cache| {
                    copy_background_layer(buffer, cache, rewrite_alpha, host.window_alpha)
                });
                if !copied {
                    buffer.fill(bg);
                }
            }
            BackgroundDecision::Rebuild => {
                let copied = host.background.as_ref().is_some_and(|cache| {
                    copy_background_layer(buffer, cache, rewrite_alpha, host.window_alpha)
                });
                if !copied {
                    buffer.fill(bg);
                }
            }
            BackgroundDecision::Fill => buffer.fill(bg),
        }
    }
    let ime_modal = host.restore_prompt.is_some()
        || host.session_prompt.is_some()
        || host.splash.is_some()
        || host.theme_picker.is_some()
        || host.palette.is_some()
        || host.context_menu.is_some()
        || host.find.active
        || host.tab_rename.is_some();
    let scroll_chip_enabled = env_flag_enabled_default_true("PRISMATTYC_SCROLL_CHIP");
    let zoomed = host.mux.is_zoomed();
    // A focus change starts the opt-in light-cycle sweep. Detected here so
    // every focus path (Alt+arrow, click, split, pane close) is covered.
    if focused != host.last_focused {
        host.last_focused = focused;
        if host.light_cycle && host.mux.pane_count() > 1 {
            host.border_anim = Some(Instant::now());
            host.last_cycle_step = 0;
        }
    }
    let cycle_progress = host.border_anim.map(|start| {
        (start.elapsed().as_millis() as f32 / host.light_cycle_ms.max(1) as f32).min(1.0)
    });
    if full {
        if let Some(layout) = host.space_rail.layout(
            geom,
            width as usize,
            height as usize,
            host.spacing.space_rail_pane_names,
        ) {
            rasterize_space_rail(
                &host.theme,
                &host.font,
                &layout,
                &host.space_rail.views(),
                buffer,
                width as usize,
                focus_border_rgb(host.focus_border),
                focus_border_rgb(0),
                match host.hover_target {
                    Some(HoverTarget::Rail(hit)) => Some(hit),
                    _ => None,
                },
                host.hover_blend,
                host.chrome_alpha,
            );
        }
        if host.background.is_none() {
            let inner_x = geom
                .window_pad
                .saturating_add(geom.chrome_left())
                .min(width as usize);
            let inner_y = geom
                .window_pad
                .saturating_add(geom.chrome_top())
                .min(height as usize);
            fill_rect_argb(
                buffer,
                width as usize,
                inner_x,
                inner_y,
                (width as usize)
                    .saturating_sub(inner_x)
                    .saturating_sub(geom.window_pad.saturating_add(geom.chrome_right())),
                (height as usize)
                    .saturating_sub(inner_y)
                    .saturating_sub(geom.window_pad.saturating_add(geom.chrome_bottom())),
                host.theme.default_bg,
                host.window_alpha,
            );
        }
    }
    if should_paint_tab_strip(
        show_tab_strip(host),
        geom.top_chrome_px,
        full,
        strip_changed,
    ) {
        let editing = host
            .tab_rename
            .as_ref()
            .map(|edit| (edit.index, edit.buffer.as_str(), edit.selected));
        let mut tabs = host.mux.tab_infos();
        if !host.attention_badge {
            for tab in &mut tabs {
                tab.attention = false;
            }
        }
        let strip_start = geom
            .tab_strip_y()
            .saturating_mul(width as usize)
            .min(buffer.len());
        rasterize_tab_strip_with_theme(
            &host.theme,
            &host.font,
            &tabs,
            &mut buffer[strip_start..],
            width as usize,
            geom.top_chrome_px,
            focus_border_rgb(host.focus_border),
            editing,
            geom.window_pad,
            geom.rail_gap,
            geom.inner_pad,
            reserve_strip_end(host),
            match host.hover_target {
                Some(HoverTarget::Strip(hit)) => Some(hit),
                _ => None,
            },
            host.hover_blend,
            host.chrome_alpha,
            TitleRowStyle {
                mode: host.pane_titles,
                notice: host.title_notice.as_ref().and_then(|notice| {
                    (Instant::now() < notice.until).then_some((
                        notice.tab,
                        notice.handle,
                        notice.title.as_str(),
                    ))
                }),
            },
            pulse_phase_if(
                host.mux.active_count() > 0
                    && pulse_live(host.window_focused, host.window_occluded),
                host.last_pulse_step,
            ),
        );
    }
    for (pane_id, pane, rect) in host.mux.panes_and_rects() {
        let (slot_x, slot_y, slot_width, slot_height) = geom.pane_slot_px(rect);
        let (content_x, content_y, content_w, content_h) = geom.pane_content_px(rect);
        let pane_full = frame_damage_covers(
            &frame_damage,
            PixelRect::new(slot_x, slot_y, slot_width, slot_height),
        );
        if !full && pane_full {
            let screen = pane.emulator.screen();
            host.render_frame.cells_painted = host
                .render_frame
                .cells_painted
                .saturating_add(screen.columns() as u64 * screen.rows() as u64);
        }
        let surface_alpha = pane_surface_alpha(host, pane_id, focused, zoomed);
        let damage = host.pane_damage.get(&pane_id);
        let mut paint_rows = composed_paint_rows
            .get(&pane_id)
            .cloned()
            .unwrap_or_default();
        if let Some(damage) = damage {
            paint_rows.extend(damage_rows(damage));
            paint_rows.sort_unstable();
            paint_rows.dedup();
        }
        // Whole-slot damage must restore the surface as well as terminal rows.
        // Otherwise old light-cycle heads remain inside the border padding.
        if pane_full {
            if host.background.is_none() {
                fill_rect_argb(
                    buffer,
                    width as usize,
                    slot_x,
                    slot_y,
                    slot_width,
                    slot_height,
                    host.theme.default_bg,
                    surface_alpha,
                );
                fill_rect_argb(
                    buffer,
                    width as usize,
                    content_x,
                    content_y,
                    content_w,
                    content_h,
                    host.theme.pane_backdrop,
                    surface_alpha,
                );
            } else {
                set_rect_alpha(
                    buffer,
                    width as usize,
                    slot_x,
                    slot_y,
                    slot_width,
                    slot_height,
                    surface_alpha,
                );
            }
        }
        let scroll = pane
            .view_scroll
            .min(pane.emulator.screen().max_view_scroll());
        let overlay = overlay_paint_decision(OverlayPaintContext {
            pane_focused: pane_id == focused,
            live_view: scroll == 0,
            cursor_visible: pane.emulator.cursor_visible(),
            preedit_present: !host.preedit.text.is_empty(),
            ime_modal,
            footer_visible,
            scroll_chip_enabled,
            find_active: host.find.active,
            palette_active: host.palette.is_some() || host.context_menu.is_some(),
        });
        let workspace = pane.workspace_layout();
        let dock_px = workspace.as_ref().map_or(0, |layout| {
            usize::from(layout.rows).saturating_mul(host.font.cell_h)
        });
        if let Some(layout) = &workspace {
            let mut workspace_screen =
                Screen::new(usize::from(layout.cols), usize::from(layout.rows), 0);
            for (row, line) in layout.lines.iter().enumerate() {
                paint_display_row(&mut workspace_screen, row, line, |col| {
                    let inverse = layout.selected_runs.iter().any(|run| {
                        usize::from(run.row) == row
                            && col >= usize::from(run.col)
                            && col < usize::from(run.col.saturating_add(run.cols))
                    });
                    let foreground = layout.status_runs.iter().find_map(|run| {
                        (usize::from(run.row) == row
                            && col >= usize::from(run.col)
                            && col < usize::from(run.col.saturating_add(run.cols)))
                        .then(|| {
                            let [r, g, b] = theme::status_rgb(&host.theme, run.tone);
                            Color::Rgb { r, g, b }
                        })
                    });
                    Style {
                        inverse,
                        foreground: foreground.unwrap_or_default(),
                        ..Style::default()
                    }
                });
            }
            rasterize_screen_at_with_theme(
                &host.theme,
                &workspace_screen,
                &host.font,
                false,
                CursorShape::Block,
                0,
                None,
                buffer,
                width as usize,
                content_x,
                content_y,
                content_w,
                dock_px.min(content_h),
                pane_screen_paint(host, pane_id, focused, zoomed),
            );
        }
        let guest_y = content_y.saturating_add(dock_px);
        let guest_h = content_h.saturating_sub(dock_px);
        let current_cursor_row = matches!(overlay.input, InputOverlay::Cursor)
            .then(|| pane.emulator.screen().cursor().row);
        let mut blit_applied = false;
        if !pane_full {
            if let Some(damage) = damage {
                let screen = pane.emulator.screen();
                let pixel_width = damage.columns().checked_mul(host.font.cell_w);
                let pixel_height = damage.rows().checked_mul(host.font.cell_h);
                let blit_rect =
                    pixel_width
                        .zip(pixel_height)
                        .and_then(|(pixel_width, pixel_height)| {
                            scroll_blit_eligible(ScrollBlitEligibility {
                                damage_rows: damage.rows(),
                                screen_rows: screen.rows(),
                                damage_columns: damage.columns(),
                                screen_columns: screen.columns(),
                                pixel_width,
                                content_width: content_w,
                                pixel_height,
                                guest_height: guest_h,
                                selection_active: pane.selection.range().is_some(),
                                scroll_events_empty: damage.scroll_events().is_empty(),
                            })
                            .then_some(FramebufferScrollRect {
                                x: content_x,
                                y: guest_y,
                                width: pixel_width,
                                row_height: host.font.cell_h,
                                rows: damage.rows(),
                            })
                        });
                if let Some(rect) = blit_rect {
                    if let Some(copied_rows) = apply_framebuffer_scroll_blits(
                        buffer,
                        width as usize,
                        height as usize,
                        rect,
                        damage.scroll_events(),
                        host.last_chrome_snapshot
                            .as_ref()
                            .map_or(&[], |chrome| chrome.boxes.as_slice()),
                    ) {
                        frame_damage.push_rect(PixelRect::new(
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.rows.saturating_mul(rect.row_height),
                        ));
                        paint_rows = damage.dirty_rows().collect();
                        if let Some(previous_row) = host
                            .last_painted_cursor_rows
                            .get(&pane_id)
                            .copied()
                            .flatten()
                            .and_then(|row| {
                                row_after_scrolls(row, damage.rows(), damage.scroll_events())
                            })
                        {
                            insert_paint_row(&mut paint_rows, previous_row, damage.rows());
                        }
                        if let Some(row) = current_cursor_row {
                            insert_paint_row(&mut paint_rows, row, damage.rows());
                        }
                        host.render_frame.rows_scrolled_as_blit = host
                            .render_frame
                            .rows_scrolled_as_blit
                            .saturating_add(copied_rows);
                        blit_applied = blit_was_applied(copied_rows);
                    }
                }
                let painted_cells = painted_cells_for_rows(damage, &paint_rows);
                host.render_frame.cells_painted = host
                    .render_frame
                    .cells_painted
                    .saturating_add(painted_cells);
            }
            if !pane_paint_required(
                paint_rows.is_empty(),
                &frame_damage,
                PixelRect::new(slot_x, slot_y, slot_width, slot_height),
            ) {
                continue;
            }
        }
        if !pane_full {
            for row in &paint_rows {
                let y = guest_y.saturating_add(row.saturating_mul(host.font.cell_h));
                let row_height = host
                    .font
                    .cell_h
                    .min(guest_y.saturating_add(guest_h).saturating_sub(y));
                frame_damage.push_rect(PixelRect::new(content_x, y, content_w, row_height));
                fill_rect_argb(
                    buffer,
                    width as usize,
                    content_x,
                    y,
                    content_w,
                    row_height,
                    host.theme.pane_backdrop,
                    surface_alpha,
                );
            }
        }
        rasterize_screen_at_with_theme_options_filtered(
            &host.theme,
            pane.emulator.screen(),
            &host.font,
            matches!(overlay.input, InputOverlay::Cursor),
            pane.emulator.cursor_shape(),
            scroll,
            pane.selection.range(),
            buffer,
            width as usize,
            content_x,
            guest_y,
            content_w,
            guest_h,
            host.font_ligatures,
            &host.font_features,
            pane_screen_paint(host, pane_id, focused, zoomed),
            pane.emulator.images(),
            pane.emulator.screen().scrolled_lines(),
            (!pane_full).then_some(paint_rows.as_slice()),
        );
        let cursor_row_painted =
            current_cursor_row.is_some_and(|row| paint_rows.binary_search(&row).is_ok());
        if should_record_cursor_row(pane_full, blit_applied, cursor_row_painted) {
            host.last_painted_cursor_rows
                .insert(pane_id, current_cursor_row);
        }
        // Unicode placeholders replace the U+10EEEE glyph (Ghostty skip + tile).
        blit_kitty_placeholders(
            &pane.emulator,
            &host.font,
            scroll,
            buffer,
            width as usize,
            content_x,
            guest_y,
            content_w,
            guest_h,
        );
        // Inline Kitty-graphics images for this pane (any mode).
        blit_direct_kitty_images(
            pane.emulator.images(),
            pane.emulator.screen().scrolled_lines(),
            scroll,
            &host.font,
            buffer,
            width as usize,
            content_x,
            guest_y,
            content_w,
            guest_h,
        );
        if overlay.paint_ime_cursor_area {
            let screen = pane.emulator.screen();
            let cursor = screen.cursor();
            if cursor.row < screen.rows() && cursor.column < screen.columns() {
                let wide = cursor.column + 1 < screen.columns()
                    && screen
                        .view_cell(scroll, cursor.row, cursor.column + 1)
                        .wide_cont;
                let area = ime_cursor_area(
                    geom,
                    rect,
                    guest_y,
                    (cursor.row, cursor.column),
                    if wide { 2 } else { 1 },
                );
                if matches!(overlay.input, InputOverlay::Preedit) {
                    rasterize_preedit_at(
                        &host.theme,
                        &host.font,
                        &host.preedit.text,
                        host.preedit.cursor,
                        buffer,
                        width as usize,
                        area.0,
                        area.1,
                        content_w.saturating_sub(area.0.saturating_sub(content_x)),
                        guest_h.saturating_sub(area.1.saturating_sub(guest_y)),
                    );
                }
                host.window.set_ime_cursor_area(
                    PhysicalPosition::new(area.0 as i32, area.1 as i32),
                    PhysicalSize::new(area.2 as u32, area.3 as u32),
                );
            }
        }
        let pane_box = ClipRect {
            x: content_x,
            y: guest_y,
            w: content_w,
            h: guest_h,
        };
        let footer_rows = if overlay.footer_visible {
            CHROME_OVERLAY_ROWS
        } else {
            0
        };
        if overlay.paint_scroll_chip {
            if let Some(clip) = overlay_clip(
                pane_box,
                footer_rows,
                host.font.cell_h,
                height as usize,
                pane_box,
            ) {
                let max = pane.emulator.screen().max_view_scroll();
                let label = if pane.scroll_new_output {
                    format!(" {scroll}/{max} · new ")
                } else {
                    format!(" {scroll}/{max} ")
                };
                rasterize_scroll_chip(
                    &host.font,
                    &label,
                    buffer,
                    width as usize,
                    clip.x,
                    clip.y,
                    clip.w,
                    clip.h,
                    host.theme.default_fg,
                    host.theme.default_bg,
                );
            }
        }
        if overlay.paint_find_prompt {
            if let Some(clip) = overlay_clip(
                pane_box,
                footer_rows,
                host.font.cell_h,
                height as usize,
                pane_box,
            ) {
                let chip_reserve = if scroll > 0 {
                    host.font.cell_w.saturating_mul(14)
                } else {
                    0
                };
                let label = find_prompt_label(&host.find.query, host.find.rank);
                rasterize_find_prompt(
                    &host.font,
                    &label,
                    buffer,
                    width as usize,
                    clip.x,
                    clip.y,
                    clip.w,
                    clip.h,
                    chip_reserve,
                    host.theme.default_fg,
                    host.theme.default_bg,
                );
            }
        }
        let (bar_x, _, bar_w, _) = geom.scrollbar_px(rect);
        if let Some(bar) = scrollbar_layout(
            bar_x,
            guest_y,
            bar_w,
            guest_h,
            scroll,
            pane.emulator.screen().max_view_scroll(),
            pane.emulator.screen().rows(),
        ) {
            let thumb = mix_rgb(host.theme.default_bg, host.theme.default_fg, 160);
            let thumb = if host.hover_target == Some(HoverTarget::ScrollbarThumb(pane_id)) {
                theme::hover_rgb(
                    host.theme.variant,
                    thumb,
                    host.theme.chrome_fg,
                    host.hover_blend,
                )
            } else {
                thumb
            };
            rasterize_scrollbar(
                bar,
                buffer,
                width as usize,
                mix_rgb(host.theme.default_bg, host.theme.default_fg, 40),
                thumb,
            );
        }
        // Bell toast (PT-39): top-right HUD on the pane that rang, mirroring
        // the pmux-attach "session is attached" toast.
        let mut toast_rows = 0usize;
        if let Some(toast) = host.bell_toasts.iter().find(|toast| toast.pane == pane_id) {
            let fill = focus_border_rgb(host.focus_border);
            rasterize_bell_toast(
                &host.font,
                &toast.label,
                buffer,
                width as usize,
                content_x,
                guest_y,
                content_w,
                guest_h,
                fill,
                contrast_ink(fill),
            );
            toast_rows = 1;
        }
        let replica = (
            u32::try_from(pane.emulator.screen().columns()).unwrap_or(u32::MAX),
            u32::try_from(pane.emulator.screen().rows()).unwrap_or(u32::MAX),
        );
        if let Some((w, h)) = prismattyc_mux::remote_size_chip(pane.size_owner, None, replica) {
            let fill = focus_border_rgb(host.focus_border);
            let chip_y = guest_y.saturating_add(toast_rows.saturating_mul(host.font.cell_h));
            rasterize_bell_toast(
                &host.font,
                &format!(" remote {w}x{h} "),
                buffer,
                width as usize,
                content_x,
                chip_y,
                content_w,
                guest_h.saturating_sub(toast_rows.saturating_mul(host.font.cell_h)),
                fill,
                contrast_ink(fill),
            );
        }
        if pane.experimental_rich() {
            let overlays = rich::overlays_for_scroll(
                rich::visible_overlays(&pane.emulator, &pane.rich),
                scroll,
            );
            if !overlays.is_empty() {
                let (_, _, content_w, _) = geom.pane_content_px(rect);
                rasterize_overlays_at_with_theme(
                    &host.theme,
                    &overlays.cell_rect,
                    &host.font,
                    buffer,
                    width as usize,
                    content_x,
                    guest_y,
                    content_x,
                    guest_y,
                    content_w,
                    guest_h,
                    pane.emulator.screen().columns(),
                    pane.emulator.screen().rows(),
                );
                rasterize_overlays_at_with_theme(
                    &host.theme,
                    &overlays.viewport,
                    &host.font,
                    buffer,
                    width as usize,
                    content_x,
                    guest_y,
                    content_x,
                    guest_y,
                    content_w,
                    guest_h,
                    pane.emulator.screen().columns(),
                    pane.emulator.screen().rows(),
                );
            }
        }
        // No structural outline for a single full-window pane — only multi-pane
        // layouts need focus/unfocused borders.
        if let Some((row, col, rows, cols)) = host
            .mux
            .focused_rich_region()
            .filter(|_| pane_id == focused && scroll == 0)
        {
            rasterize_region_focus_ring(
                buffer,
                width as usize,
                content_x,
                guest_y,
                host.font.cell_w,
                host.font.cell_h,
                row,
                col,
                rows,
                cols,
                focus_border_rgb(host.focus_border),
            );
        }
        if host.mux.pane_count() > 1 {
            if pane_id == focused && cycle_progress.is_some_and(|progress| progress < 1.0) {
                host.border_underlay.capture(
                    buffer,
                    width as usize,
                    PixelRect::new(slot_x, slot_y, slot_width, slot_height),
                );
            }
            rasterize_pane_chrome_with_theme(
                &host.theme,
                buffer,
                width as usize,
                slot_x,
                slot_y,
                slot_width,
                slot_height,
                pane_id == focused,
                pane.unseen_output,
                pulse_phase_if(
                    pane.is_active_at(frame_now)
                        && pulse_live(host.window_focused, host.window_occluded),
                    host.last_pulse_step,
                ),
                cycle_progress.filter(|_| pane_id == focused),
                host.light_cycle_head,
                focus_border_rgb(host.focus_border),
                surface_alpha,
            );
        }
        if pane.mail_depth > 0 {
            let (ox, oy, ow, oh) = if host.mux.pane_count() > 1 {
                (slot_x, slot_y, slot_width, slot_height)
            } else {
                geom.pane_content_px(rect)
            };
            rasterize_mail_letter_with_theme(
                &host.theme,
                buffer,
                width as usize,
                ox,
                oy,
                ow,
                oh,
                true,
                pane_id == focused && focus_border_name(host.focus_border) == "amber",
            );
        }
    }
    if let Some(label) = git_hover_label(host) {
        let cols = (width as usize / host.font.cell_w.max(1))
            .saturating_sub(2)
            .max(1);
        let lines = space_panel::wrap_text(&label, cols);
        for (index, line) in lines.iter().enumerate() {
            let y = geom.chrome_top() + index * host.font.cell_h;
            if y + host.font.cell_h > height as usize {
                break;
            }
            let bg = focus_border_rgb(host.focus_border);
            rasterize_bell_toast(
                &host.font,
                line,
                buffer,
                width as usize,
                host.font.cell_w,
                y,
                cols * host.font.cell_w,
                host.font.cell_h,
                bg,
                contrast_ink(bg),
            );
        }
    }
    // Chord cheat-sheet while Ctrl+Shift is held, then a short linger. It
    // sits above a bottom spaces rail, never over it (PT-91).
    let footer_bottom = (height as usize).saturating_sub(host.mux.geom().chrome_bottom());
    if footer_visible {
        let help = chord_help_text(&host.mux, &host.keymap, show_tab_strip(host));
        rasterize_footer(
            &host.font,
            &help,
            buffer,
            width as usize,
            footer_bottom,
            CHROME_OVERLAY_ROWS,
            focus_border_rgb(host.focus_border),
            host.chrome_alpha,
        );
    } else if host.config_path.is_some() || host.config_error.is_some() {
        let notice = match (host.config_path.as_deref(), host.config_error.as_deref()) {
            (Some(path), Some(error)) => {
                format!(" edit config: {} · rejected: {error}", path.display())
            }
            (Some(path), None) => format!(" edit config: {}", path.display()),
            (None, Some(error)) => format!(" config rejected: {error}"),
            (None, None) => unreachable!("config footer branch requires a notice"),
        };
        let (fill, alpha) = if host.config_error.is_some() {
            (focus_border_rgb(0), OPAQUE_ALPHA)
        } else {
            (focus_border_rgb(host.focus_border), host.chrome_alpha)
        };
        rasterize_footer(
            &host.font,
            &notice,
            buffer,
            width as usize,
            footer_bottom,
            CHROME_OVERLAY_ROWS,
            fill,
            alpha,
        );
    }
    if let (Some(view), Some(band)) = (walkthrough_caption_view(host), walkthrough_band(host)) {
        rasterize_walkthrough_caption(
            &host.font,
            &view,
            band,
            buffer,
            width as usize,
            host.theme.chrome_bg,
            host.theme.chrome_fg,
        );
    }
    // Drag toast (PT-79): "Moving tab NAME → tab OTHER" at the bottom-right,
    // above a bottom rail, so it never covers the strip or the drop target.
    if host.drag_toaster {
        if let Some(label) = drag_toast_label(host, width as usize) {
            let fill = focus_border_rgb(host.focus_border);
            let chip_h = host.font.cell_h.min(footer_bottom);
            let pad = host.mux.geom().window_pad;
            rasterize_bell_toast(
                &host.font,
                &label,
                buffer,
                width as usize,
                pad,
                footer_bottom.saturating_sub(chip_h).saturating_sub(pad),
                (width as usize).saturating_sub(pad.saturating_mul(2)),
                chip_h,
                fill,
                contrast_ink(fill),
            );
        }
    }
    if let Some(picker) = host.theme_picker.as_ref() {
        let items = picker_items(picker.family.as_deref());
        let rows: Vec<ThemePickerRow<'_>> = items
            .iter()
            .map(|item| match item {
                theme::PickerItem::Theme { index } => ThemePickerRow {
                    label: theme::builtins()[*index].name.as_str(),
                    branch: false,
                },
                theme::PickerItem::Family { label, .. } => ThemePickerRow {
                    label: label.as_str(),
                    branch: true,
                },
            })
            .collect();
        let hint = if picker.family.is_some() {
            THEME_PICKER_HINT_FAMILY
        } else {
            THEME_PICKER_HINT_ROOT
        };
        rasterize_theme_picker(
            &host.font,
            &rows,
            picker.selected,
            picker.scroll,
            &host.theme,
            hint,
            overlay_surface,
            buffer,
            width as usize,
            height as usize,
            focus_border_rgb(host.focus_border),
        );
    }
    // Palette is a global layer. The decision function owns the per-pane
    // gates; HostState is authoritative for this single global paint pass.
    let painted_palette = if let Some(palette) = host.palette.as_ref() {
        let focus = focus_border_rgb(host.focus_border);
        let view = palette.view(&host.keymap, host.experimental_rich);
        let chips = Palette::chip_labels();
        let detail = view.detail(palette.selected);
        let sections = [
            PaletteSection {
                header: "RECENT",
                subtitle: "",
                rows: &view.recent,
            },
            PaletteSection {
                header: "MATCHES",
                subtitle: &palette.query,
                rows: &view.matches,
            },
        ];
        let footer = match palette.awaiting_digit {
            Some(palette::PaletteEntry::SelectTabFamily) => "press the tab digit 1–9 · Esc back",
            Some(palette::PaletteEntry::LayoutFamily) => "press the column count 2–9 · Esc back",
            _ => "Enter run · Esc close · ↑↓ move · C-←/→ filter",
        };
        let frame = PaletteFrame {
            query: Some(&palette.query),
            chips: Some((&chips, palette.chip_index())),
            sections: &sections,
            selected: palette.selected,
            scroll: palette.scroll,
            detail: detail.as_ref(),
            footer,
        };
        paint_palette_overlay(
            &host.font,
            &host.theme,
            focus,
            &frame,
            overlay_surface,
            buffer,
            width as usize,
            height as usize,
        )
    } else {
        None
    };
    if let Some(layout) = painted_palette {
        if let Some(palette) = host.palette.as_mut() {
            palette.scroll = layout.start;
        }
        host.palette_layout = Some(layout);
    }
    let painted_space = if let Some(picker) = host.space_picker.as_ref() {
        let focus = focus_border_rgb(host.focus_border);
        let spaces = terminal_switcher::rows(host, picker.kind);
        let ranked = picker.ranked(&spaces);
        let rows: Vec<PaletteRow> = ranked
            .iter()
            .map(|space| {
                let sessions = if space.sessions == 1 {
                    "session"
                } else {
                    "sessions"
                };
                let describe = if let Some(entries) = &host.terminal_targets {
                    entries
                        .iter()
                        .find(|entry| entry.label == space.name)
                        .map(|entry| entry.detail.clone())
                        .unwrap_or_default()
                } else if picker.confirm.as_deref() == Some(space.name.as_str()) {
                    format!("Delete space {}? Enter to confirm · Esc", space.name)
                } else {
                    format!("{} {sessions}", space.sessions)
                };
                let label = host
                    .terminal_targets
                    .as_ref()
                    .and_then(|entries| entries.iter().find(|entry| entry.label == space.name))
                    .map(|entry| entry.display_label.clone())
                    .unwrap_or_else(|| space.name.clone());
                PaletteRow::plain(label, describe, String::new())
            })
            .collect();
        let query = picker
            .status
            .clone()
            .unwrap_or_else(|| picker.query.clone());
        let (header, footer) = match picker.kind {
            SpacePickerKind::Open if host.terminal_messages && host.terminal_targets.is_some() => (
                "AGENT MESSAGES",
                "Queue receipts are not execution · Enter focus exact pane · Esc close",
            ),
            SpacePickerKind::Open if host.terminal_targets.is_some() => (
                "FIND TERMINAL",
                "Type Space, terminal, or directory · Enter focus · Esc close",
            ),
            SpacePickerKind::Open => ("OPEN SPACE", "Enter open · Esc close · ↑↓ move"),
            SpacePickerKind::Delete => ("DELETE SPACE", "Enter delete · Esc close · ↑↓ move"),
            SpacePickerKind::MovePane => ("MOVE PANE TO SPACE", "Enter move · Esc close · ↑↓ move"),
            SpacePickerKind::MoveSession => {
                ("MOVE SESSION TO SPACE", "Enter move · Esc close · ↑↓ move")
            }
        };
        let move_label = host
            .move_target
            .as_ref()
            .and_then(|target| target.remote.as_ref())
            .map(|target| format!("{} · pane {}", target.name, target.pane))
            .unwrap_or_default();
        let sections = [PaletteSection {
            header,
            subtitle: &move_label,
            rows: &rows,
        }];
        let detail = host
            .terminal_targets
            .as_ref()
            .and_then(|entries| {
                ranked
                    .get(picker.selected)
                    .and_then(|row| entries.iter().find(|entry| entry.label == row.name))
            })
            .map(|entry| palette::PaletteDetail {
                name: entry.display_label.clone(),
                text: entry.detail.clone(),
                chords: String::new(),
                config_key: String::new(),
            });
        let frame = PaletteFrame {
            query: Some(&query),
            chips: None,
            sections: &sections,
            selected: picker.selected,
            scroll: picker.scroll,
            detail: detail.as_ref(),
            footer,
        };
        paint_palette_overlay(
            &host.font,
            &host.theme,
            focus,
            &frame,
            overlay_surface,
            buffer,
            width as usize,
            height as usize,
        )
    } else {
        None
    };
    if let Some(layout) = painted_space {
        if let Some(picker) = host.space_picker.as_mut() {
            picker.scroll = layout.start;
        }
        host.palette_layout = Some(layout);
    }
    let painted_context = if host.context_menu.is_some() {
        let focus = focus_border_rgb(host.focus_border);
        context_menu_rows(host).and_then(|(header, rows)| {
            let selected = host.context_menu.as_ref()?.selected;
            let sections = if host.space_panel.is_none()
                && matches!(host.context_menu_target, Some(ContextMenuTarget::Pane(_)))
            {
                vec![
                    PaletteSection {
                        header: "LAYOUT",
                        subtitle: &header,
                        rows: &rows[..6],
                    },
                    PaletteSection {
                        header: "SESSION",
                        subtitle: "",
                        rows: &rows[6..8],
                    },
                    PaletteSection {
                        header: "SPACE",
                        subtitle: if host
                            .context_menu
                            .as_ref()
                            .is_some_and(|m| m.confirm == Some(11))
                        {
                            &header
                        } else {
                            ""
                        },
                        rows: &rows[8..],
                    },
                ]
            } else {
                vec![PaletteSection {
                    header: &header,
                    subtitle: "",
                    rows: &rows,
                }]
            };
            let frame = PaletteFrame {
                query: None,
                chips: None,
                sections: &sections,
                selected,
                scroll: host.space_panel.as_ref().map_or(0, |panel| panel.scroll),
                detail: None,
                footer: "Enter select · Esc close · ↑↓ move",
            };
            paint_palette_overlay(
                &host.font,
                &host.theme,
                focus,
                &frame,
                overlay_surface,
                buffer,
                width as usize,
                height as usize,
            )
        })
    } else {
        None
    };
    if let Some(layout) = painted_context {
        if let Some(panel) = host.space_panel.as_mut() {
            panel.scroll = layout.start;
        }
        host.palette_layout = Some(layout);
    }
    // `+` / save_space: modal name prompt over the panes (PT-123). The
    // rail's inline editor is for renames; a new space asks first.
    let painted_save = if let Some(edit) = host
        .space_rail
        .edit
        .as_ref()
        .filter(|edit| save_space_modal_open(Some(edit)))
    {
        let focus = focus_border_rgb(host.focus_border);
        let hint = match host.space_rail.notice {
            Some(notice) if !notice.is_empty() => notice.to_string(),
            _ => "type a name; Enter creates a fresh shell".to_string(),
        };
        let rows = [PaletteRow::plain(hint, String::new(), String::new())];
        let sections = [PaletteSection {
            header: "NEW SPACE",
            subtitle: "create a space with one fresh shell",
            rows: &rows,
        }];
        let frame = PaletteFrame {
            query: Some(&edit.buffer),
            chips: None,
            sections: &sections,
            selected: 0,
            scroll: 0,
            detail: None,
            footer: "Enter save · Esc cancel",
        };
        paint_palette_overlay(
            &host.font,
            &host.theme,
            focus,
            &frame,
            overlay_surface,
            buffer,
            width as usize,
            height as usize,
        )
    } else {
        None
    };
    if painted_save.is_some() {
        host.palette_layout = painted_save;
    }
    host.pane_bells.paint(buffer, width as usize);
    // Topmost: the launch splash covers panes, chrome, and other overlays.
    if let Some(splash) = host.splash.as_ref() {
        let animation_ms = splash.frame_clock();
        let lines = splash::layout_with_resume(
            splash.page,
            prismattyc_core::package_version(),
            splash.tip,
            animation_ms,
            splash.resume,
        );
        rasterize_splash(
            &host.font,
            &lines,
            buffer,
            width as usize,
            height as usize,
            host.theme.default_bg,
            animation_ms,
        );
    }
    restore_prompt::paint(host, buffer, width as usize, height as usize);
    session_prompt::paint(host, buffer, width as usize, height as usize);
    // Visual bell (PT-39): invert the whole frame while the flash is lit.
    // A post-pass keeps every painter above unaware of the flash, and works
    // identically on the softbuffer and wgpu present paths.
    if host
        .bell_flash
        .is_some_and(|start| start.elapsed().as_millis() < BELL_FLASH_MS)
    {
        for px in buffer.iter_mut() {
            *px ^= 0x00FF_FFFF;
        }
    }
    if full {
        FrameDamage::Full
    } else {
        frame_damage
    }
}

impl App {
    fn resize_grid(host: &mut HostState, physical: PhysicalSize<u32>) {
        Self::refit_geom(host, physical, None);
    }

    /// PT-124: reload the font at a new compositor scale factor. No-op when
    /// the effective pixel size is unchanged, so the startup
    /// `ScaleFactorChanged` some compositors send costs nothing. Grid refit
    /// is left to the `Resized` event winit sends right after.
    fn refit_font_for_scale(
        file_config: &config::ConfigFile,
        host: &mut HostState,
        scale_factor: f64,
    ) {
        let px = scaled_font_px(file_config.font_px.unwrap_or(FONT_PX), scale_factor);
        if (px - host.font.px).abs() < 0.01 {
            return;
        }
        let fallbacks = file_config.font_fallback.clone().unwrap_or_default();
        match FontMetrics::load_with(px, file_config.font.as_deref(), &fallbacks) {
            Ok(font) => {
                eprintln!(
                    "prismattyc-host: scale factor {scale_factor}: font {:.1}px -> {px:.1}px",
                    host.font.px
                );
                host.font = font;
                host.dirty = true;
            }
            Err(error) => {
                eprintln!(
                    "prismattyc-host: font reload for scale {scale_factor} failed: {error:#}"
                );
            }
        }
    }

    /// Recompute cell grid + spacing from the active window's domain
    /// layout (not cached rects). Tab create/switch must go through here
    /// so a 1-pane tab does not permanently zero `pane_gap`.
    fn refit_geom(host: &mut HostState, physical: PhysicalSize<u32>, why: Option<&str>) {
        let geom = host_geom(
            &host.font,
            host.mux.active_pane_count() > 1,
            show_tab_strip(host),
            strip_handle_row(host),
            host.spacing,
            host.space_rail.longest_name_cells(),
        );
        let (cols, rows) = size_to_cells(physical, &host.font, geom);
        // Centre the cell grid. `size_to_cells` floor-divides, so the pixels
        // no column could fill would otherwise all sit on the right edge, and
        // an odd `pane_gap` splits unevenly on top of that. Measure the real
        // margins and give half the difference back to the leading edge.
        let mut geom = geom;
        let trailing_gap = geom.pane_gap - geom.pane_gap / 2;
        let left = geom.window_pad + geom.chrome_left() + geom.pane_gap / 2;
        let right = (physical.width as usize)
            .saturating_sub(geom.window_pad + geom.chrome_left() + cols * host.font.cell_w)
            .saturating_sub(geom.chrome_right())
            .saturating_add(trailing_gap);
        geom.slack_x = right.saturating_sub(left) / 2;
        let top = geom.window_pad + geom.chrome_top() + geom.pane_gap / 2;
        let bottom = (physical.height as usize)
            .saturating_sub(geom.window_pad + geom.chrome_top() + rows * host.font.cell_h)
            .saturating_sub(geom.chrome_bottom())
            .saturating_add(trailing_gap);
        geom.slack_y = bottom.saturating_sub(top) / 2;
        if cols == host.mux.cols() && rows == host.mux.rows() && geom == host.mux.geom() {
            return;
        }
        if let Err(error) = host.mux.resize_with_geom(cols, rows, geom) {
            if let Some(why) = why {
                eprintln!("prismattyc-host: re-fit after {why} failed: {error:#}");
            }
            return;
        }
        host.left_button_down = false;
        host.cursor_cell = None;
        host.pending_full_repaint = Some(FullRepaintReason::Resize);
        host.dirty = true;
        sync_chrome_hover(host);
    }

    fn drain_pty(host: &mut HostState) -> bool {
        let prior_unseen = host.mux.unseen_count();
        let prior_panes = host.mux.pane_count();
        let prior_tabs = host.mux.tab_count();
        let prior_active = host.mux.active_count();
        let parse_started = Instant::now();
        let parked_more = local_views::drain(host);
        let (pty_dirty, more) = host.mux.drain_all();
        if pty_dirty {
            host.hyperlink_hover = None;
        }
        let more = more || parked_more;
        host.render_frame.timing.parse_us = parse_started.elapsed().as_micros() as u64;
        let damage_started = Instant::now();
        host.dirty |= pty_dirty;
        let bells = host.mux.take_pending_bells();
        if !bells.is_empty() {
            if host.visual_bell {
                if host.pane_visual_bell {
                    if !host.window_occluded {
                        let now = Instant::now();
                        let geom = host.mux.geom();
                        for (id, runtime, rect) in host.mux.panes_and_rects() {
                            if bells.contains(&id) {
                                let (x, y, w, h) = geom.pane_slot_px(rect);
                                host.dirty |= host.pane_bells.ring(
                                    id.get(),
                                    runtime.view_identity(),
                                    host.mux.space_id.as_deref(),
                                    PixelRect::new(x, y, w, h),
                                    now,
                                );
                            }
                        }
                    }
                } else {
                    host.bell_flash = Some(Instant::now());
                    host.dirty = true;
                }
            }
            if host.audible_bell {
                let now = Instant::now();
                if host
                    .last_bell_sound
                    .is_none_or(|at| now.duration_since(at) >= BELL_SOUND_MIN_GAP)
                {
                    host.last_bell_sound = Some(now);
                    notify::bell_sound();
                }
            }
            if host.bell_toaster {
                let until = Instant::now() + host.bell_toaster_ms;
                for pane in &bells {
                    if host.visual_bell
                        && host.pane_visual_bell
                        && !host.window_occluded
                        && host.mux.panes_and_rects().any(|(id, _, _)| id == *pane)
                    {
                        continue;
                    }
                    match host.bell_toasts.iter_mut().find(|t| t.pane == *pane) {
                        Some(toast) => {
                            toast.label = BELL_TOAST_LABEL.to_string();
                            toast.until = until;
                        }
                        None => host.bell_toasts.push(BellToast {
                            pane: *pane,
                            until,
                            label: BELL_TOAST_LABEL.to_string(),
                        }),
                    }
                    host.dirty = true;
                }
            }
            if host.os_notify_bell && !host.window_focused {
                let now = Instant::now();
                if host
                    .last_bell_notify
                    .is_none_or(|at| now.duration_since(at) >= BELL_NOTIFY_MIN_GAP)
                {
                    host.last_bell_notify = Some(now);
                    let tab = bells
                        .first()
                        .and_then(|pane| host.mux.pane_tab_title(*pane));
                    notify::bell(tab.as_deref());
                }
            }
        }
        let toasts = host.mux.take_pending_toasts();
        // Writer-death chips are not BEL. Show them even when bell_toaster is
        // off (PT-119). Linger still uses bell_toaster_ms.
        if apply_write_fail_toasts(
            &mut host.bell_toasts,
            toasts,
            host.bell_toaster_ms,
            Instant::now(),
        ) {
            host.dirty = true;
        }
        let attentions = host.mux.take_pending_attentions();
        if !attentions.is_empty() {
            if host.attention_sound {
                let now = Instant::now();
                if host
                    .last_bell_sound
                    .is_none_or(|at| now.duration_since(at) >= BELL_SOUND_MIN_GAP)
                {
                    host.last_bell_sound = Some(now);
                    notify::bell_sound();
                }
            }
            for (pane, message) in attentions {
                let detected = prismattyc_mux::detect_inject_agent(
                    host.mux.pane(pane).and_then(|runtime| runtime.child_pid()),
                    None,
                );
                let agent = match detected {
                    prismattyc_mux::InjectAgent::Claude => "Claude".to_string(),
                    prismattyc_mux::InjectAgent::Grok => "Grok".to_string(),
                    prismattyc_mux::InjectAgent::Cursor => "Cursor".to_string(),
                    prismattyc_mux::InjectAgent::Codex => "Codex".to_string(),
                    prismattyc_mux::InjectAgent::Kiro => "Kiro".to_string(),
                    prismattyc_mux::InjectAgent::Unknown => host
                        .mux
                        .pane_tab_title(pane)
                        .unwrap_or_else(|| "Agent".to_string()),
                };
                let session = host.mux.pane_session_name(pane).unwrap_or_default();
                let title = if !session.is_empty() && session != agent {
                    format!("{agent} needs you — {session}")
                } else {
                    format!("{agent} needs you")
                };
                let needs_os_notify = !host.mux.pane_tab_selected(pane) || !host.window_focused;
                if host.os_notify_attention && needs_os_notify {
                    let now = Instant::now();
                    let last = host
                        .last_attention_notify
                        .iter()
                        .find_map(|(id, at)| (*id == pane).then_some(*at));
                    if last.is_none_or(|at| now.duration_since(at) >= ATTENTION_NOTIFY_MIN_GAP) {
                        if let Some(entry) = host
                            .last_attention_notify
                            .iter_mut()
                            .find(|(id, _)| *id == pane)
                        {
                            entry.1 = now;
                        } else {
                            host.last_attention_notify.push((pane, now));
                        }
                        notify::attention(&title, &message);
                    }
                }
                host.pending_attention_announce = Some(format!("{title}: {message}"));
            }
        }
        let rises = host.mux.take_mail_rises(&mut host.last_mail_depths);
        if !rises.is_empty() {
            host.pending_mail_announce = Some(
                rises
                    .iter()
                    .map(|(agent, depth)| format!("{agent}: {depth} mail"))
                    .collect::<Vec<_>>()
                    .join(". "),
            );
        }
        if let Some(text) = host.mux.take_semantic_copy() {
            write_clipboard_text(host, text);
        }
        let active = host.mux.active_count();
        if prior_panes != host.mux.pane_count() || prior_tabs != host.mux.tab_count() {
            // Tab records stay (PT-68). Persist only the new selection.
            persist_attach_selection(host);
            Self::refit_geom(host, host.window.inner_size(), Some("pane exit"));
        }
        if pty_dirty
            || prior_unseen != host.mux.unseen_count()
            || prior_panes != host.mux.pane_count()
            || prior_active != active
        {
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            // Active decays with wall-clock time, so the transition itself must
            // schedule a repaint — no PTY bytes arrive to mark the frame dirty.
            host.dirty = true;
        }
        // Drive the dot pulse for active-but-momentarily-quiet panes. Quantized
        // to PULSE_STEPS so animation costs at most ~16 repaints/second; while
        // output streams, frames repaint anyway and this adds nothing. Gated on
        // visibility: agent panes stay active for hours, and 16 fps of full
        // repaints for a background window is heat, not information.
        if active > 0 && host.window_focused && !host.window_occluded {
            let step = ((host.pulse_epoch.elapsed().as_millis() * PULSE_STEPS / PULSE_PERIOD_MS)
                % PULSE_STEPS) as u8;
            if step != host.last_pulse_step {
                host.last_pulse_step = step;
                host.dirty = true;
            }
        }
        settle_pane_bells(host, Instant::now());
        // Settle an expired bell flash with one final repaint.
        if host
            .bell_flash
            .is_some_and(|start| start.elapsed().as_millis() >= BELL_FLASH_MS)
        {
            host.bell_flash = None;
            host.dirty = true;
        }
        // Drop expired bell toasts the same way. Each toast carries its own
        // deadline, fixed at ring time, so a config reload never moves a
        // live toast. Leave last_raster.guards unchanged here: publish reads
        // the live chip list and must see the mismatch so it can emit now.
        let now = Instant::now();
        if expire_bell_toasts(&mut host.bell_toasts, now) {
            host.dirty = true;
        }
        if host
            .title_notice
            .as_ref()
            .is_some_and(|notice| now >= notice.until)
        {
            host.title_notice = None;
            host.dirty = true;
        }
        // Drive the splash attract loop the same way: quantized frames while
        // the window is visible. Not gated on focus — the splash is the first
        // thing a bare launch shows, and some WMs deliver focus late.
        if !host.window_occluded {
            if let Some(splash) = host.splash.as_mut() {
                if splash.tick() {
                    host.dirty = true;
                }
            }
        }
        // Drive the light-cycle sweep the same way: quantized repaints while
        // it runs, one final repaint to settle on the static border.
        if let Some(start) = host.border_anim {
            let elapsed = start.elapsed().as_millis();
            if elapsed >= host.light_cycle_ms {
                host.border_anim = None;
                host.dirty = true;
            } else {
                let step = (elapsed * LIGHT_CYCLE_STEPS / host.light_cycle_ms.max(1)) as u8;
                if step != host.last_cycle_step {
                    host.last_cycle_step = step;
                    host.dirty = true;
                }
            }
        }
        sync_pane_title_notices(host);
        host.render_frame.timing.damage_us = damage_started.elapsed().as_micros() as u64;
        more
    }
}

fn sync_pane_title_notices(host: &mut HostState) {
    let infos = host.mux.tab_infos();
    let current: Vec<Vec<String>> = infos.iter().map(|tab| tab.handle_titles.clone()).collect();
    let focused: Vec<Option<usize>> = infos.iter().map(|tab| tab.focused_handle).collect();
    if let Some(notice) =
        title_row::title_notice_from_diff(&host.last_handle_titles, &current, &focused)
    {
        host.pending_title_announce = Some(notice.title.clone());
        host.title_notice = Some(LiveTitleNotice {
            tab: notice.tab,
            handle: notice.handle,
            title: notice.title,
            until: Instant::now() + host.bell_toaster_ms,
        });
        host.dirty = true;
    }
    host.last_handle_titles = current;
}

fn begin_tab_rename(host: &mut HostState, index: Option<usize>) {
    if !show_tab_strip(host) {
        return;
    }
    host.space_rail.leave();
    let index = index.unwrap_or_else(|| {
        host.mux
            .tab_infos()
            .iter()
            .position(|tab| tab.selected)
            .unwrap_or(0)
    });
    let Some(window) = host.mux.window_at_tab(index) else {
        return;
    };
    if let Some((_, panes)) = host.mux.tab_panes().get(index) {
        if panes.len() == 1 && session_prompt::rename(host, panes[0], keybind::Action::RenameTab) {
            return;
        }
    }
    let title = host
        .mux
        .tab_infos()
        .get(index)
        .map(|tab| tab.title.clone())
        .unwrap_or_default();
    host.tab_rename = Some(TabRename {
        index,
        window,
        pane: None,
        buffer: title,
        selected: true,
    });
    host.dirty = true;
}

/// Edit the focused pane's title in the tab chip (PT-148). Commit stores it
/// locally and, for an attach pane, runs `pmux rename-pane SESSION TITLE`.
fn begin_pane_rename(host: &mut HostState) {
    let index = host
        .mux
        .tab_infos()
        .iter()
        .position(|tab| tab.selected)
        .unwrap_or(0);
    let pane = host.mux.focused_id();
    begin_pane_rename_for(host, index, pane);
}

fn begin_pane_rename_for(host: &mut HostState, index: usize, pane: PaneId) {
    if session_prompt::rename(host, pane, keybind::Action::RenamePane) {
        return;
    }
    if !show_tab_strip(host) {
        rail_toast(
            host,
            " pane titles need the tab strip (tab_strip = always) ",
        );
        return;
    }
    host.space_rail.leave();
    let Some(window) = host.mux.window_at_tab(index) else {
        return;
    };
    let title = host.mux.pane_title(pane).unwrap_or_default().to_string();
    host.tab_rename = Some(TabRename {
        index,
        window,
        pane: Some(pane),
        buffer: title,
        selected: true,
    });
    host.dirty = true;
}

fn cancel_tab_rename(host: &mut HostState) {
    if host.tab_rename.take().is_some() {
        host.dirty = true;
    }
}

fn attach_records_from_live(host: &HostState) -> attach_tabs::AttachTabsFile {
    let mut file = attach_tabs::records_from_runtime(&host.mux, &host.attach_pane_sessions);
    file.space = live_cache_space(host.space_rail.current.clone(), &spaces_dir());
    file.space_id = host.mux.space_id.clone();
    file.session_names = host
        .attach_pane_sessions
        .iter()
        .filter_map(|(pane, id)| Some((id.clone(), host.mux.attach_name_of(*pane)?.to_string())))
        .collect();
    file
}

fn mark_layout_dirty(host: &mut HostState) {
    host.layout_dirty = true;
    observe_boss_walkthrough(host);
}

fn live_walkthrough_space(host: &HostState) -> prismattyc_mux::SavedSpace {
    let tabs = host.mux.tab_panes();
    let counts: Vec<usize> = tabs.iter().map(|(_, panes)| panes.len()).collect();
    let seats: usize = counts.iter().sum();
    walkthrough::space_from_pane_counts(seats, &counts)
}

fn observe_boss_walkthrough(host: &mut HostState) {
    if !host
        .walkthrough
        .as_ref()
        .is_some_and(walkthrough::WalkthroughLive::boss_step_armed)
    {
        return;
    }
    let Ok(target) = walkthrough::bundled_boss() else {
        return;
    };
    let live = live_walkthrough_space(host);
    match walkthrough::boss_matches(&target, &live) {
        walkthrough::BossVerdict::Match => {
            observe_walkthrough(
                host,
                walkthrough::Detected::SpaceEvent {
                    event: "boss_snapshot_match".into(),
                },
            );
        }
        walkthrough::BossVerdict::Mismatch(diff) => {
            if let Some(session) = host.walkthrough.as_mut() {
                session.set_mismatch_hint(diff);
                host.dirty = true;
            }
        }
    }
}

fn sync_attach_pane_sessions(host: &mut HostState) {
    host.attach_pane_sessions = host
        .mux
        .tab_panes()
        .into_iter()
        .flat_map(|(_, panes)| panes)
        .filter_map(|pane| {
            host.mux
                .attach_session_of(pane)
                .map(|id| (pane, id.to_string()))
        })
        .collect();
}

/// Once a second: mark panes whose shell runs a nested `pmux-attach` on
/// this host's socket, and unmark adopted panes whose attach exited
/// (PT-210). Without the mark, `pmux new` / `pmux attach` typed into a
/// pane left the tab cache empty, `pmux space save` recorded no tabs, and
/// `space open` fanned the sessions out one tab per session.
fn adopt_nested_attaches(host: &mut HostState, now: Instant) {
    if !host.adopted.due(now) {
        return;
    }
    let mut changed = false;
    let live_panes: Vec<PaneId> = host
        .mux
        .tab_panes()
        .into_iter()
        .flat_map(|(_, panes)| panes)
        .collect();
    let gone = attach_adopt::clear_gone(&mut host.mux, &mut host.adopted, &live_panes);
    if !gone.is_empty() {
        for pane in gone {
            host.attach_pane_sessions.remove(&pane);
        }
        changed = true;
    }
    // A bare shell has no children: no /proc walk for it.
    let candidates: Vec<(PaneId, u32)> = live_panes
        .iter()
        .filter(|pane| host.mux.attach_session_of(**pane).is_none())
        .filter_map(|pane| {
            host.mux
                .pane(*pane)
                .and_then(|runtime| runtime.child_pid())
                .map(|pid| (*pane, pid))
        })
        .filter(|(_, pid)| !prismattyc_mux::procinfo::children_of(*pid).is_empty())
        .collect();
    if !candidates.is_empty() {
        if let Some(socket) = host_mux_socket() {
            let directory = attach_log::session_directory();
            let assignments = attach_adopt::adopt_candidates(
                &mut host.mux,
                &mut host.adopted,
                &candidates,
                &socket,
                &directory,
            );
            for (pane, _pid, id, _name) in assignments {
                apply_attach_title_pin(host, pane, &id);
                host.attach_pane_sessions.insert(pane, id);
                changed = true;
            }
        }
    }
    if changed {
        mark_layout_dirty(host);
        host.dirty = true;
    }
}

fn session_id_from_attach_spawn(program: &str, args: &[String]) -> Option<String> {
    attach_log::attach_target(program, args)
}

fn register_spawned_attach(host: &mut HostState, pane: PaneId, program: &str, args: &[String]) {
    let Some((id, name)) = spawned_attach_registration(program, args, &attach_log::session_names())
    else {
        return;
    };
    host.mux.mark_attach_session(pane, id.clone(), name);
    host.attach_pane_sessions.insert(pane, id.clone());
    apply_attach_title_pin(host, pane, &id);
}

/// Match `key` to a live session id and display name.
///
/// `key` may be the numeric id or the stable name. No match returns
/// `(key, key)` so a brand-new seat still marks.
fn resolve_attach_session_key(key: &str, names: &HashMap<String, String>) -> (String, String) {
    names
        .iter()
        .find(|(id, name)| id.as_str() == key || name.as_str() == key)
        .map(|(id, name)| (id.clone(), name.clone()))
        .unwrap_or_else(|| (key.to_string(), key.to_string()))
}

/// `(session id, display name)` for a host attach spawn, or `None` when
/// the argv is not an attach seat (shell, dump flags, `--all`).
fn spawned_attach_registration(
    program: &str,
    args: &[String],
    names: &HashMap<String, String>,
) -> Option<(String, String)> {
    let key = session_id_from_attach_spawn(program, args)?;
    Some(resolve_attach_session_key(&key, names))
}

fn apply_attach_title_pin(host: &mut HostState, pane: PaneId, session_key: &str) {
    let Some(snapshot) = attach_log::live_snapshot() else {
        return;
    };
    let Some((title, pinned)) = attach_log::session_title_pin(&snapshot, session_key) else {
        return;
    };
    host.mux.apply_server_pane_title(pane, &title, pinned);
}

fn register_even_layout_attaches(
    host: &mut HostState,
    before: &[PaneId],
    program: &str,
    args: &[String],
) {
    let Some(id) = session_id_from_attach_spawn(program, args) else {
        return;
    };
    host.mux.register_new_active_attaches(before, &id);
    sync_attach_pane_sessions(host);
}

fn persist_attach_layout_from_live(host: &mut HostState) {
    if host.restore_prompt.is_some() {
        return;
    }
    if !may_write_shared_cache(host.cache_writer) {
        host.layout_dirty = false;
        return;
    }
    if host.space_opens.blocks_persist() {
        return;
    }
    let Some(path) = host.attach_layout_path.clone() else {
        host.layout_dirty = false;
        return;
    };
    sync_attach_pane_sessions(host);
    if host.attach_layout.is_none() && host.attach_pane_sessions.is_empty() {
        host.layout_dirty = false;
        return;
    }
    let new = attach_records_from_live(host);
    match attach_tabs::persist_if_changed(&path, &mut host.attach_layout, new) {
        Ok(true) => {
            let stamp = cache_stamp(&path);
            host.attach_own_stamp = stamp;
            host.attach_cache_stamp = stamp;
        }
        Ok(false) => {}
        Err(error) => {
            eprintln!("prismattyc-host: could not save attach tab layout: {error}");
        }
    }
    host.layout_dirty = false;
}

/// Startup attach grouping is allowed to read the shared cache only for the
/// registered host. Explicit targets still attach without importing another
/// host's tab grouping when this process is unregistered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAttachPlan {
    Bare,
    ExplicitOneTabPerTarget,
    ExplicitWithCache,
}

fn startup_attach_plan(registered_owner: bool, has_explicit_targets: bool) -> StartupAttachPlan {
    match (registered_owner, has_explicit_targets) {
        (true, true) => StartupAttachPlan::ExplicitWithCache,
        (false, true) => StartupAttachPlan::ExplicitOneTabPerTarget,
        (_, false) => StartupAttachPlan::Bare,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartupWindowPlan {
    attach: StartupAttachPlan,
    cache_writer: bool,
}

/// A process's CLI attach targets apply only to its first window. Later
/// windows opened by `NewWindow` start with one local shell.
fn startup_attach_targets(first_window: bool, cli_targets: &[AttachTarget]) -> &[AttachTarget] {
    if first_window {
        cli_targets
    } else {
        &[]
    }
}

/// Later in-process windows never read or write the process's shared cache,
/// even though the process may own the host PID for its first window.
fn startup_window_plan(
    first_window: bool,
    registered_owner: bool,
    has_explicit_targets: bool,
) -> StartupWindowPlan {
    if !first_window {
        return StartupWindowPlan {
            attach: StartupAttachPlan::Bare,
            cache_writer: false,
        };
    }
    StartupWindowPlan {
        attach: startup_attach_plan(registered_owner, has_explicit_targets),
        cache_writer: registered_owner,
    }
}

/// Unregistered hosts (`--new-window` while another host is live) and later
/// in-process windows must not poll or persist `{stem}.attach-tabs.json`.
const fn may_write_shared_cache(cache_writer: bool) -> bool {
    cache_writer
}

/// Idle wake cadence for the registered host's cache poll (PT-171).
const CACHE_POLL_HEARTBEAT: Duration = Duration::from_secs(1);

fn cache_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Child-exit must not drop tab records (PT-68). Update only the selection.
fn persist_attach_selection(host: &mut HostState) {
    if host.restore_prompt.is_some() || host.space_opens.blocks_persist() {
        return;
    }
    if !may_write_shared_cache(host.cache_writer) {
        return;
    }
    let Some(path) = host.attach_layout_path.as_ref() else {
        return;
    };
    let Some(mut file) = host.attach_layout.clone() else {
        return;
    };
    let selected = host.mux.selected_tab_index();
    let live = host.mux.tab_panes();
    let live_sessions: Vec<String> = live
        .get(selected)
        .map(|(_, panes)| {
            panes
                .iter()
                .filter_map(|pane| host.attach_pane_sessions.get(pane).cloned())
                .collect()
        })
        .unwrap_or_default();
    attach_tabs::overlay_selection(
        &mut file,
        &live_sessions,
        host.attach_pane_sessions
            .get(&host.mux.focused_id())
            .cloned(),
    );
    match attach_tabs::persist_if_changed(path, &mut host.attach_layout, file) {
        Ok(true) => {
            let stamp = cache_stamp(path);
            host.attach_own_stamp = stamp;
            host.attach_cache_stamp = stamp;
        }
        Ok(false) => {}
        Err(error) => {
            eprintln!("prismattyc-host: could not save attach tab layout: {error}");
        }
    }
    host.layout_dirty = false;
}

fn commit_tab_rename(host: &mut HostState) {
    let Some(edit) = host.tab_rename.take() else {
        return;
    };
    if let Some(pane) = edit.pane {
        let title = edit.buffer.trim().to_string();
        host.mux
            .set_pane_title(pane, (!title.is_empty()).then(|| title.clone()));
        // `attach_session` is the mux session id; `--session` resolves an
        // id or a name, so an all-digit id is never read as a pane id.
        let live_attach = !host.mux.is_placeholder(pane);
        let mut pane_ok = true;
        if let Some(session) = host
            .mux
            .attach_session_of(pane)
            .filter(|_| live_attach)
            .map(str::to_string)
        {
            let mut args = vec!["rename-pane".to_string(), "--session".to_string(), session];
            if !title.is_empty() {
                args.push(title);
            }
            let status = std::process::Command::new(pmux_bin())
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .status();
            pane_ok = matches!(status, Ok(status) if status.success());
            if !pane_ok {
                rail_toast(host, " pmux rename-pane failed; see the log ");
            }
        }
        host.dirty = true;
        observe_host_action(host, keybind::Action::RenamePane, pane_ok);
        return;
    }
    match host.mux.rename_window(edit.window, &edit.buffer) {
        Ok(_) => {
            mark_layout_dirty(host);
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            observe_host_action(host, keybind::Action::RenameTab, true);
        }
        Err(_) => {
            host.tab_rename = Some(edit);
            host.dirty = true;
            observe_host_action(host, keybind::Action::RenameTab, false);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameStroke {
    Insert(char),
    Backspace,
    DropSelection,
    Commit,
    Cancel,
}

/// winit 0.30 delivers Space as `NamedKey::Space`, not `Character(" ")`.
/// Mapping it as `_` dropped spaces (`x y z` → `xyz`).
fn rename_stroke_from_logical(logical: &Key) -> Option<RenameStroke> {
    match logical {
        Key::Named(NamedKey::Enter) => Some(RenameStroke::Commit),
        Key::Named(NamedKey::Escape) => Some(RenameStroke::Cancel),
        Key::Named(NamedKey::Backspace) => Some(RenameStroke::Backspace),
        Key::Named(NamedKey::Space) => Some(RenameStroke::Insert(' ')),
        Key::Character(text) => text.chars().next().map(RenameStroke::Insert),
        _ => Some(RenameStroke::DropSelection),
    }
}

fn apply_rename_stroke(
    buffer: &mut String,
    selected: &mut bool,
    stroke: RenameStroke,
) -> Option<bool> {
    match stroke {
        RenameStroke::Insert(ch) => {
            if *selected {
                buffer.clear();
                *selected = false;
            }
            if !ch.is_control() && buffer.len() < 64 {
                buffer.push(ch);
            }
            None
        }
        RenameStroke::Backspace => {
            if *selected {
                buffer.clear();
                *selected = false;
            } else {
                buffer.pop();
            }
            None
        }
        RenameStroke::DropSelection => {
            *selected = false;
            None
        }
        RenameStroke::Commit => Some(true),
        RenameStroke::Cancel => Some(false),
    }
}

fn handle_rename_key(host: &mut HostState, event: &winit::event::KeyEvent) -> bool {
    if host.tab_rename.is_none() {
        return false;
    }
    if host.modifiers.control_key() || host.modifiers.alt_key() || host.modifiers.super_key() {
        return true;
    }
    let Some(stroke) = rename_stroke_from_logical(&event.logical_key) else {
        return true;
    };
    let Some(edit) = host.tab_rename.as_mut() else {
        return true;
    };
    match apply_rename_stroke(&mut edit.buffer, &mut edit.selected, stroke) {
        Some(true) => commit_tab_rename(host),
        Some(false) => cancel_tab_rename(host),
        None => host.dirty = true,
    }
    true
}

/// The current space changed (opened from the rail, the picker, or by
/// `pmux space open` through the attach-tabs cache). `PMUX_SPACE` follows so
/// [`loaded_space`] (placeholder recreate) reads the same file.
fn set_current_space(host: &mut HostState, name: Option<String>) {
    if host.space_rail.current == name {
        return;
    }
    match name.as_deref() {
        Some(name) => std::env::set_var("PMUX_SPACE", name),
        None => std::env::remove_var("PMUX_SPACE"),
    }
    host.space_polish.failed = false;
    host.space_polish.changed = None;
    host.space_rail.save_status.clear();
    host.space_rail.set_current(name);
    // The attach-tabs cache carries the space name; rewrite it so a
    // restart, regroup, or `pmux space save` sees the new current space.
    mark_layout_dirty(host);
    host.dirty = true;
}

/// Feedback chip for a rail action, anchored to the focused pane. Not a
/// bell, so not gated on `bell_toaster` (same rule as the write-fail toast).
fn rail_toast(host: &mut HostState, label: &str) {
    let pane = host.mux.focused_id();
    let until = Instant::now() + host.bell_toaster_ms;
    match host.bell_toasts.iter_mut().find(|toast| toast.pane == pane) {
        Some(toast) => {
            toast.label = label.to_string();
            toast.until = until;
        }
        None => host.bell_toasts.push(BellToast {
            pane,
            until,
            label: label.to_string(),
        }),
    }
    host.dirty = true;
}

/// The chip list changed: refit when the chips size themselves to the
/// longest name (`space_rail_chip_cols = 0`).
fn rail_changed(host: &mut HostState) {
    host.dirty = true;
    // A side rail's column follows the longest name; a horizontal rail's
    // chips are re-laid at paint time, so the refit is a no-op there.
    App::refit_geom(host, host.window.inner_size(), Some("spaces changed"));
    sync_chrome_hover(host);
}

/// Re-read the spaces directory and reflow if the chip width follows it.
fn refresh_rail(host: &mut HostState) {
    if host.space_rail.refresh(&spaces_dir()) {
        rail_changed(host);
    }
}

fn maybe_space_rail_hint(host: &mut HostState) {
    if host.mux.geom().rail_side == space_rail::RailSide::Off || host.space_rail.names.is_empty() {
        return;
    }
    let dir = spaces_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let flag = dir.join(".rail-hint-shown");
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(flag)
        .is_ok();
    if created {
        rail_toast(host, " click opens · right-click for more ");
    }
}

/// Live tabs as session names (attach panes only), for
/// [`space_rail::SpaceRail::infer_current`].
fn live_tab_session_names(host: &HostState) -> Vec<Vec<String>> {
    host.mux
        .tab_panes()
        .into_iter()
        .map(|(_, panes)| {
            panes
                .into_iter()
                .filter_map(|pane| host.mux.attach_name_of(pane).map(str::to_string))
                .collect::<Vec<String>>()
        })
        .filter(|tab| !tab.is_empty())
        .collect()
}

/// Sibling `pmux` next to this binary, else `$PMUX` / PATH.
fn pmux_bin() -> PathBuf {
    find_mux_bin()
}

/// The mux socket this host belongs to: `PMUX_SOCKET` when set and
/// non-empty, else the default instance — the same instance a bare `pmux`
/// picks. A bare launch (Dock, Finder, a recorder, `prismattyc-host` from a
/// shell) therefore registers, acks, and opens spaces on the default
/// instance instead of silently doing nothing, which let `pmux space open`
/// spawn a second host beside a live one (PT-171).
fn host_mux_socket() -> Option<PathBuf> {
    resolve_host_socket(std::env::var_os("PMUX_SOCKET"), || {
        prismattyc_mux::default_socket_path("default")
    })
}

/// Pure half of [`host_mux_socket`]: `env` wins when non-empty, else the
/// default instance; a failing default is reported once and yields `None`.
fn resolve_host_socket(
    env: Option<std::ffi::OsString>,
    default: impl FnOnce() -> std::io::Result<PathBuf>,
) -> Option<PathBuf> {
    if let Some(raw) = env {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    match default() {
        Ok(socket) => Some(socket),
        Err(error) => {
            eprintln!("prismattyc-host: no default mux socket: {error}");
            None
        }
    }
}

use space_open::Mode as SpaceOpenMode;

fn targeted_space_args(
    name: &str,
    mode: SpaceOpenMode,
    path: Option<&Path>,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = space_open_cli_args(name, mode)
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect();
    if mode != SpaceOpenMode::NewWindow {
        if let Some(path) = path {
            args.extend([
                std::ffi::OsString::from("--view-path"),
                path.as_os_str().to_owned(),
            ]);
        }
    }
    args
}

fn space_open_cli_args(name: &str, mode: SpaceOpenMode) -> Vec<String> {
    match mode {
        SpaceOpenMode::Create => vec![
            "space".into(),
            "create".into(),
            name.into(),
            "--no-attach".into(),
        ],
        SpaceOpenMode::Switch => {
            vec![
                "space".into(),
                "open".into(),
                name.into(),
                "--no-run".into(),
                "--no-attach".into(),
            ]
        }
        SpaceOpenMode::NewWindow => {
            vec![
                "space".into(),
                "open".into(),
                name.into(),
                "--no-run".into(),
                "--new-window".into(),
            ]
        }
    }
}

/// Open a saved space in THIS host: `pmux space open NAME --no-attach`
/// regroups the live tabs through the attach-tabs cache (PT-65) and the
/// poll picks the new arrangement up. Only an applied layout sets the chip.
fn open_space_from_host(host: &mut HostState, name: &str, mode: SpaceOpenMode) {
    if !host.space_opens.blocks_persist()
        && mode == SpaceOpenMode::Switch
        && host.space_rail.current.as_deref() == Some(name)
    {
        rail_toast(host, &format!(" {name} is the current space "));
        return;
    }
    host.space_opens.enqueue(name, mode);
    rail_toast(host, &format!(" opening space {name} "));
    advance_space_opens(host);
}

/// Reap the previous helper before the next request may write this view.
fn advance_space_opens(host: &mut HostState) {
    let current = host.attach_layout_path.as_deref().and_then(cache_stamp);
    if let Some(completed) = host.space_opens.poll(current) {
        report_space_open(host, completed);
    }
    let Some(request) = host.space_opens.next() else {
        return;
    };
    let before = host.attach_layout_path.as_deref().and_then(cache_stamp);
    host.space_open_observation = Some(space_outcome::Observation::capture(&request.name));
    match std::process::Command::new(pmux_bin())
        .args({
            let mut args = targeted_space_args(
                &request.name,
                request.mode,
                host.attach_layout_path.as_deref(),
            );
            if let Some(name) = &request.session_name {
                args.extend([std::ffi::OsString::from("--session-name"), name.into()]);
            }
            args
        })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => host.space_opens.started(request, child, before),
        Err(error) => report_space_open(
            host,
            space_open::Completion {
                name: request.name,
                mode: request.mode,
                applied: None,
                error: Some(format!("could not run pmux: {error}")),
                session_name: request.session_name,
            },
        ),
    }
}

/// Keep the result after its toast expires. Render status exposes the receipt.
fn report_space_open(host: &mut HostState, completed: space_open::Completion) {
    if completed.mode == SpaceOpenMode::Create && completed.applied.is_none() {
        if let (Some(name), Some(error)) = (&completed.session_name, &completed.error) {
            // A failed create must remain editable. Do not retry after the
            // Space was saved or after another popup took focus.
            if !space_json_exists(&completed.name)
                && host.session_prompt.is_none()
                && !host.space_opens.busy()
            {
                session_prompt::retry_space(
                    host,
                    completed.name.clone(),
                    name.clone(),
                    error.clone(),
                );
            }
        }
    }
    let view = match (completed.mode, completed.applied) {
        (SpaceOpenMode::NewWindow, _) => space_outcome::View::NewWindowRequested,
        (_, Some(true)) => space_outcome::View::Applied,
        (_, Some(false)) => space_outcome::View::Partial,
        (_, None) => space_outcome::View::NotApplied,
    };
    let observation = host
        .space_open_observation
        .take()
        .unwrap_or_else(|| space_outcome::Observation::capture(&completed.name));
    let mut report = observation.finish(
        completed.name,
        view,
        completed.error,
        attach_log::live_snapshot().map(space_outcome::sessions),
    );
    report.sequence = host
        .last_space_open
        .as_ref()
        .map_or(1, |previous| previous.sequence.saturating_add(1));
    report.mode = match completed.mode {
        SpaceOpenMode::Create => "create",
        SpaceOpenMode::Switch => "switch",
        SpaceOpenMode::NewWindow => "new_window",
    };
    let label = report.label();
    if let (Ok(space), Some(view)) = (
        load_space(&spaces_dir(), &report.name),
        host.attach_layout_path.clone(),
    ) {
        if let Ok(mut value) = serde_json::to_value(&report) {
            value["recorded_at_ms"] = prismattyc_mux::host_render_status::unix_ms().into();
            std::thread::spawn(move || {
                if let Err(error) =
                    prismattyc_mux::space_team::record_result(&spaces_dir(), &space, &view, value)
                {
                    eprintln!("prismattyc-host: could not retain Space result: {error:#}");
                }
            });
        }
    }
    eprintln!("prismattyc-host: {label}");
    rail_toast(host, &format!(" {label} "));
    host.last_space_open = Some(report);
}

/// Follow a renamed Space without adopting a new owner under its old name.
fn resolve_host_space(host: &mut HostState) -> Option<prismattyc_mux::SavedSpace> {
    let name = host.space_rail.current.as_deref()?;
    let (resolved, space) =
        space_view::resolve_space(&spaces_dir(), name, host.mux.space_id.as_deref())?;
    if resolved != name {
        set_current_space(host, Some(resolved));
        refresh_rail(host);
        persist_attach_layout_from_live(host);
    }
    Some(space)
}

/// Refresh all chips and reconcile transfers without reopening launch recipes.
fn refresh_space_views(host: &mut HostState) {
    space_panel::poll(host);
    spaces_polish::poll(host);
    let now = Instant::now();
    if host
        .last_space_refresh
        .is_some_and(|last| now.duration_since(last) < CACHE_POLL_HEARTBEAT)
    {
        return;
    }
    host.last_space_refresh = Some(now);
    let snapshot = attach_log::live_snapshot();
    if host.mux.refresh_git_info(snapshot.as_ref()) {
        host.dirty = true;
    }
    let Some(snapshot) = snapshot else {
        if !host.space_rail.attention_counts.is_empty() {
            host.space_rail.attention_counts.clear();
            host.dirty = true;
        }
        return;
    };
    let bindings: Vec<_> = host
        .attach_pane_sessions
        .iter()
        .filter_map(|(pane, id)| {
            let session = snapshot.sessions.iter().find(|s| s.id.to_string() == *id)?;
            (host.mux.attach_name_of(*pane) != Some(session.name.as_str()))
                .then(|| (*pane, id.clone(), session.name.clone()))
        })
        .collect();
    for (pane, id, name) in bindings {
        host.mux.mark_attach_session(pane, id, name);
        mark_layout_dirty(host);
        host.dirty = true;
    }
    let mut pane_names = HashMap::new();
    let requests = space_panel::attention_requests(host);
    let mut attention_counts = HashMap::new();
    for name in &host.space_rail.names {
        if let Ok(space) = load_space(&spaces_dir(), name) {
            pane_names.insert(name.clone(), space_view::pane_names(&space, &snapshot));
            let details = prismattyc_mux::space_team::describe(
                name,
                &space,
                Default::default(),
                Some(&snapshot),
                &requests,
                prismattyc_mux::host_render_status::unix_ms(),
            );
            if details.sessions_needing_input > 0 {
                attention_counts.insert(name.clone(), details.sessions_needing_input);
            }
        }
    }
    if host.space_rail.attention_counts != attention_counts {
        host.space_rail.attention_counts = attention_counts;
        host.dirty = true;
    }
    if host.space_rail.live_pane_names != pane_names {
        host.space_rail.live_pane_names = pane_names;
        host.dirty = true;
    }
    if host.space_opens.blocks_persist() || host.restore_prompt.is_some() {
        return;
    }
    let Some(space) = resolve_host_space(host) else {
        return;
    };
    let Some(name) = host.space_rail.current.clone() else {
        return;
    };
    let Some(owner) = space.id.as_deref() else {
        return;
    };
    host.mux.space_id = Some(owner.to_string());
    let exited: Vec<_> = host
        .attach_pane_sessions
        .iter()
        .filter_map(|(pane, id)| {
            let name = host.mux.attach_name_of(*pane)?;
            (host.mux.is_placeholder(*pane)
                && space.sessions.iter().any(|saved| saved.name == name)
                && !snapshot
                    .sessions
                    .iter()
                    .any(|session| session.id.to_string() == *id || session.name == name))
            .then(|| (*pane, name.to_string()))
        })
        .collect();
    for (pane, name) in &exited {
        host.mux
            .mark_attach_session(*pane, name.clone(), name.clone());
    }
    // A pane can move independently while its old session remains. Discard
    // stale subscriptions before regroup reconnects the session's remaining pane.
    let stale: Vec<_> = host
        .attach_pane_sessions
        .iter()
        .filter_map(|(pane, id)| {
            if exited.iter().any(|(saved, _)| saved == pane) {
                return None;
            }
            let remote = host.mux.remote_pane_id(*pane)?;
            let valid = snapshot.sessions.iter().any(|session| {
                session.id.to_string() == *id
                    && session.space_id.as_deref() == Some(owner)
                    && session
                        .windows
                        .iter()
                        .flat_map(|window| &window.panes)
                        .any(|p| p.id == remote)
            });
            (!valid).then_some(*pane)
        })
        .collect();
    let detached = !stale.is_empty();
    for pane in stale {
        if let Some(id) = host.attach_pane_sessions.get(&pane) {
            host.observed_space_sessions.remove(id);
        }
        if let Err(error) = host.mux.empty_space_view(pane) {
            eprintln!("prismattyc-host: detach moved pane: {error}");
            return;
        }
        host.attach_pane_sessions.remove(&pane);
        host.dirty = true;
    }
    sync_attach_pane_sessions(host);
    let current = local_views::records(host);
    let observed: std::collections::HashSet<String> = snapshot
        .sessions
        .iter()
        .filter(|session| session.space_id.as_deref() == Some(owner))
        .map(|session| session.id.to_string())
        .collect();
    let mut visible = snapshot.clone();
    visible.sessions.retain(|session| {
        let id = session.id.to_string();
        !host.observed_space_sessions.contains(&id)
            || current.tabs.iter().any(|tab| tab.sessions.contains(&id))
    });
    host.observed_space_sessions = observed;
    let desired = local_views::layout(host, &name, &space, &visible, &current);
    if desired.tabs == current.tabs && !detached {
        return;
    }
    let names = snapshot
        .sessions
        .iter()
        .map(|s| (s.id.to_string(), s.name.clone()))
        .collect();
    host.mux.space_id = Some(owner.to_string());
    let focused = host.mux.focused_id();
    match regroup::apply(
        &mut host.mux,
        &mut host.attach_pane_sessions,
        &desired,
        &find_mux_bin().to_string_lossy(),
        &names,
    ) {
        Ok(_) => {
            local_views::restore_local_focus(host, focused);
            host.attach_layout = Some(desired);
            persist_attach_layout_from_live(host);
            App::refit_geom(host, host.window.inner_size(), Some("space ownership"));
            host.dirty = true;
        }
        Err(error) => rail_toast(host, &format!(" space view update failed: {error} ")),
    }
}

/// Apply one external cache write. The helper can still be waiting for this ACK.
fn poll_host_attach_tabs(host: &mut HostState) {
    if !may_write_shared_cache(host.cache_writer) || host.restore_prompt.is_some() {
        return;
    }
    let Some(path) = host.attach_layout_path.clone() else {
        return;
    };
    let now = cache_stamp(&path);
    if now == host.attach_cache_stamp {
        return;
    }
    if now == host.attach_own_stamp {
        host.attach_cache_stamp = now;
        return;
    }
    let Some(mut file) = attach_tabs::load(&path) else {
        return;
    };
    if let Some(name) = file.space.as_deref() {
        let permitted = load_space(&spaces_dir(), name)
            .ok()
            .zip(attach_log::live_snapshot())
            .is_some_and(|(space, snapshot)| space_view::permits_layout(&space, &snapshot, &file));
        if !permitted {
            host.attach_cache_stamp = now;
            host.space_opens
                .cache_applied(now, file.space.as_deref(), file.mode, false);
            rail_toast(host, " space ownership could not be verified ");
            return;
        }
    }
    let prior_owner = host.mux.space_id.clone();
    let prior_name = host.space_rail.current.clone();
    let owner = file
        .space
        .as_deref()
        .and_then(|name| load_space(&spaces_dir(), name).ok())
        .and_then(|space| space.id);
    let restored = match local_views::switch(host, owner.clone()) {
        Ok(restored) => restored,
        Err(error) => {
            host.attach_cache_stamp = now;
            host.space_opens
                .cache_applied(now, file.space.as_deref(), file.mode, false);
            rail_toast(host, &format!("Could not switch Space view: {error}"));
            return;
        }
    };
    host.mux.space_id = owner;
    let current = local_views::records(host);
    let preserve_view = restored || local_views::has_local(&host.mux);
    if preserve_view {
        if let Some((space, snapshot)) = file
            .space
            .as_deref()
            .and_then(|name| load_space(&spaces_dir(), name).ok())
            .zip(attach_log::live_snapshot())
        {
            file = local_views::layout(
                host,
                file.space.as_deref().unwrap(),
                &space,
                &snapshot,
                &current,
            );
        }
    }
    let focused = host.mux.focused_id();
    let mux_bin = find_mux_bin();
    let names = attach_log::session_names();
    let result = if preserve_view && file.tabs == current.tabs {
        Ok(false)
    } else {
        regroup::apply(
            &mut host.mux,
            &mut host.attach_pane_sessions,
            &file,
            &mux_bin.to_string_lossy(),
            &names,
        )
    };
    host.attach_cache_stamp = now;
    host.space_opens
        .cache_applied(now, file.space.as_deref(), file.mode, result.is_ok());
    match result {
        Ok(_) => {
            if preserve_view {
                local_views::restore_local_focus(host, focused);
            }
            let mut file = file;
            match file.space.as_deref() {
                Some(name) if space_json_exists(name) => {
                    set_current_space(host, file.space.clone());
                }
                Some(_) => {
                    file.space = None;
                    set_current_space(host, None);
                }
                None => {}
            }
            host.observed_space_sessions = file
                .tabs
                .iter()
                .flat_map(|tab| tab.sessions.iter().cloned())
                .collect();
            host.attach_layout = Some(file);
            host.layout_dirty = false;
            host.dirty = true;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.window.focus_window();
            if let Some(socket) = host_mux_socket() {
                let ack = prismattyc_mux::host_ack_path_from_socket(&socket);
                if let Err(error) = prismattyc_mux::touch_host_ack(&ack) {
                    eprintln!("prismattyc-host: could not ack attach-tabs reload: {error}");
                }
            }
            App::refit_geom(host, host.window.inner_size(), Some("space regroup"));
        }
        Err(error) => {
            // Regroup may have moved some panes before it failed.
            if local_views::switch(host, prior_owner).unwrap_or(false) {
                set_current_space(host, prior_name);
                App::refit_geom(
                    host,
                    host.window.inner_size(),
                    Some("Space switch rollback"),
                );
            } else {
                set_current_space(host, None);
            }
            host.dirty = true;
            eprintln!("prismattyc-host: attach-tabs regroup failed: {error:#}");
        }
    }
}

/// Save the current Space from this window's live arrangement.
fn save_space_from_host(host: &mut HostState, name: &str) {
    if host.space_rail.current.as_deref() != Some(name) {
        rail_toast(host, " open this space before saving it ");
        return;
    }
    if host.space_opens.blocks_persist() {
        rail_toast(host, " wait for the space layout to apply before saving ");
        return;
    }
    persist_attach_layout_from_live(host);
    match std::process::Command::new(pmux_bin())
        .args(["space", "save", name])
        .args(
            host.attach_layout_path
                .as_ref()
                .into_iter()
                .flat_map(|path| {
                    [
                        std::ffi::OsString::from("--view-path"),
                        path.as_os_str().to_owned(),
                    ]
                }),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
    {
        Ok(status) if status.success() => {
            refresh_rail(host);
            set_current_space(host, Some(name.to_string()));
            host.space_polish.failed = false;
            rail_toast(host, " Saved ");
        }
        Ok(_) | Err(_) => {
            host.space_polish.failed = true;
            rail_toast(host, " Save failed — use Save current space to retry ");
        }
    }
}

/// Act on what the rail decided after a key or a click.
fn apply_rail_verdict(host: &mut HostState, verdict: space_rail::RailVerdict) {
    use space_rail::RailVerdict;
    match verdict {
        RailVerdict::Consumed | RailVerdict::Leave | RailVerdict::Invalid(_) => {}
        RailVerdict::Menu { index } => open_context_menu(host, ContextMenuTarget::SpaceChip(index)),
        RailVerdict::Open(name) => open_space_from_host(host, &name, SpaceOpenMode::Switch),
        RailVerdict::Rename { old, new } => {
            match run_pmux_space(&["space".into(), "rename".into(), old.clone(), new.clone()]) {
                Ok(_) => {
                    resolve_host_space(host);
                    refresh_rail(host);
                    if host.space_rail.keyboard {
                        if let Some(index) = host.space_rail.names.iter().position(|n| *n == new) {
                            host.space_rail.focus = Some(index);
                        }
                    }
                }
                Err(error) => {
                    eprintln!("prismattyc-host: rename space {old} to {new}: {error:#}");
                    host.space_rail.notice = Some("rename failed; see the log");
                }
            }
        }
        RailVerdict::Delete(name) => {
            if let Err(error) = run_pmux_space(&["space".into(), "rm".into(), name.clone()]) {
                eprintln!("prismattyc-host: delete space {name}: {error:#}");
            }
            refresh_rail(host);
        }
        RailVerdict::Create(name) => session_prompt::create_space(host, name),
    }
    host.dirty = true;
}

fn open_context_menu(host: &mut HostState, target: ContextMenuTarget) {
    let kind = match target {
        ContextMenuTarget::SpaceChip(_) => ContextMenuKind::SpaceChip,
        ContextMenuTarget::Pane(_) => ContextMenuKind::Pane,
    };
    host.context_menu = Some(ContextMenu::new(kind));
    host.context_menu_target = Some(target);
    host.palette = None;
    host.space_picker = None;
    host.terminal_targets = None;
    host.palette_layout = None;
    host.tab_rename = None;
    host.window.set_title("Prismattyc — context menu");
    host.dirty = true;
    host.window.request_redraw();
    sync_chrome_hover(host);
}

fn close_context_menu(host: &mut HostState) {
    host.space_panel = None;
    if host.context_menu.take().is_some() {
        host.context_menu_target = None;
        host.palette_layout = None;
        host.window
            .set_title(&window_title(&host.mux, show_tab_strip(host)));
        host.dirty = true;
        host.window.request_redraw();
        sync_chrome_hover(host);
    }
}

fn saved_space_names_for_session(dir: &Path, session: &str) -> Vec<String> {
    let mut names = prismattyc_mux::list_spaces(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let space = load_space(dir, &entry.name).ok()?;
            space
                .sessions
                .iter()
                .any(|saved| saved.name == session)
                .then_some(entry.name)
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn pane_space_membership(session: &str, spaces: &[String]) -> String {
    match spaces {
        [] => format!("{session} · not in a saved space"),
        [space] => format!("{session} · space {space}"),
        spaces => format!("{session} · spaces {}", spaces.join(", ")),
    }
}

fn context_menu_rows(host: &HostState) -> Option<(String, Vec<PaletteRow>)> {
    if host.space_panel.is_some() {
        return space_panel::rows(host);
    }
    let target = host.context_menu_target?;
    let menu = host.context_menu.as_ref()?;
    let rows = match target {
        ContextMenuTarget::SpaceChip(index) => {
            let name = host.space_rail.names.get(index)?.clone();
            let mut header = match load_space(&spaces_dir(), &name) {
                Ok(space) => {
                    let sessions = if space.sessions.len() == 1 {
                        "session"
                    } else {
                        "sessions"
                    };
                    let tab_count = space.tabs.len().max(space.sessions.len());
                    let tabs = if tab_count == 1 { "tab" } else { "tabs" };
                    let agents = space
                        .sessions
                        .iter()
                        .map(|session| session.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "{name} · {} {sessions} · {} {tabs} · {agents}",
                        space.sessions.len(),
                        tab_count
                    )
                }
                Err(_) => format!("{name} · saved space"),
            };
            if let Some(names) = host
                .space_rail
                .live_pane_names
                .get(&name)
                .filter(|names| !names.is_empty())
            {
                header.push_str(&format!(" · panes: {}", names.join(", ")));
            }
            let labels = [
                ("Open (switch)", "switch to this space"),
                ("New session in this space", "create a fresh shell session"),
                ("Open in new window", "start another host window"),
                ("Save current space", "update this space from its window"),
                ("Rename", "change the space name"),
                ("Move focused pane here", "transfer only the focused pane"),
                ("Delete…", "remove this saved space"),
                (
                    "Team details…",
                    "sessions, attention, context links, and templates",
                ),
                ("Spaces settings…", "rail position, autosave, and startup"),
                ("Undo last removal or move", spaces_polish::undo_label(host)),
            ];
            let rows = labels
                .into_iter()
                .enumerate()
                .map(|(index, (label, description))| {
                    let describe = if menu.confirm == Some(index) {
                        format!("{description} · press Enter again to confirm")
                    } else {
                        description.to_string()
                    };
                    PaletteRow::plain(label.to_string(), describe, String::new())
                })
                .collect();
            (header, rows)
        }
        ContextMenuTarget::Pane(pane) => {
            let mut header = host
                .mux
                .attach_name_of(pane)
                .or_else(|| host.mux.attach_session_of(pane))
                .map(|session| {
                    let spaces = saved_space_names_for_session(&spaces_dir(), session);
                    pane_space_membership(session, &spaces)
                })
                .unwrap_or_else(|| "local shell".to_string());
            if menu.confirm == Some(11) {
                header = format!(
                    "Kill {} in {}?",
                    host.mux.attach_name_of(pane).unwrap_or("this session"),
                    host.space_rail.current.as_deref().unwrap_or("this Space")
                );
            }
            let labels = [
                ("Split right", "Split the pane horizontally"),
                ("Split down", "Split the pane vertically"),
                ("Zoom", "Show only this pane"),
                ("Move to next tab", "Move this pane to the next tab"),
                ("Move pane to space ▸", "Transfer only this pane"),
                (
                    "Close pane",
                    if host.mux.attach_session_of(pane).is_some() {
                        "Keep session and Space membership"
                    } else {
                        "Close this local shell"
                    },
                ),
                if host.mux.attach_session_of(pane).is_some() {
                    (
                        "Rename session",
                        "Change the session name and mailbox address",
                    )
                } else {
                    ("Rename title", "Edit the pane title")
                },
                ("Detach session", "Disconnect view; keep session running"),
                ("Save current space", "Save the current arrangement"),
                ("Move session to space ▸", "Move session and all its panes"),
                (
                    "Remove session from space",
                    "Remove membership; keep the session running",
                ),
                (
                    "Remove and kill session…",
                    "Remove from Space and stop its processes",
                ),
            ];
            (
                header,
                labels
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, description))| {
                        let description = if menu.confirm == Some(index) {
                            "Enter: kill session · Esc: cancel".to_string()
                        } else {
                            description.to_string()
                        };
                        PaletteRow::plain(label.to_string(), description, String::new())
                    })
                    .collect(),
            )
        }
    };
    Some(rows)
}

fn open_move_space_picker(host: &mut HostState) {
    if !move_target::begin(host) {
        return;
    }
    host.space_picker = Some(SpacePicker::new(SpacePickerKind::MovePane));
    host.palette_layout = None;
    host.window.set_title("Prismattyc — spaces");
    host.dirty = true;
    sync_chrome_hover(host);
}

fn focused_tab_index(host: &HostState, pane: PaneId) -> usize {
    host.mux
        .tab_panes()
        .iter()
        .position(|(_, panes)| panes.contains(&pane))
        .unwrap_or(0)
}

fn apply_space_context_action(host: &mut HostState, chip: usize, action: SpaceContextAction) {
    let Some(name) = host.space_rail.names.get(chip).cloned() else {
        return;
    };
    match action {
        SpaceContextAction::Details => space_panel::open(host, name),
        SpaceContextAction::Settings => space_panel::settings(host),
        SpaceContextAction::Undo => spaces_polish::undo(host),
        SpaceContextAction::Open(mode) => open_space_from_host(host, &name, mode),
        SpaceContextAction::Save => save_space_from_host(host, &name),
        SpaceContextAction::AddSession => session_prompt::add_to_space(host, name),
        SpaceContextAction::Rename => {
            host.space_rail.begin_rename(chip);
            host.dirty = true;
        }
        SpaceContextAction::Move => {
            if move_target::begin(host) {
                move_to_space_from_host(host, &name, false);
            }
        }
        SpaceContextAction::Delete => {
            if let Err(error) = run_pmux_space(&["space".into(), "rm".into(), name.clone()]) {
                eprintln!("prismattyc-host: delete space {name}: {error:#}");
            }
            refresh_rail(host);
        }
    }
}

fn apply_pane_context_action(
    host: &mut HostState,
    pane: PaneId,
    action: PaneContextAction,
    program: &str,
    child_args: &[String],
) -> Dispatch {
    if host.mux.focus(pane) {
        mark_layout_dirty(host);
    }
    if matches!(
        action,
        PaneContextAction::MoveToSpace | PaneContextAction::MoveSessionToSpace
    ) && host.mux.focused_id() != pane
    {
        rail_toast(
            host,
            "Move cancelled: the selected pane is no longer available",
        );
        return Dispatch::Handled;
    }
    let action = match action {
        PaneContextAction::SplitRight => Some(keybind::Action::SplitRight),
        PaneContextAction::SplitDown => Some(keybind::Action::SplitDown),
        PaneContextAction::Zoom => Some(keybind::Action::ZoomPane),
        PaneContextAction::Rename => {
            begin_pane_rename_for(host, focused_tab_index(host, pane), pane);
            None
        }
        PaneContextAction::MovePaneNextTab => Some(keybind::Action::MovePaneNextTab),
        PaneContextAction::MoveToSpace => {
            open_move_space_picker(host);
            None
        }
        PaneContextAction::MoveSessionToSpace => {
            open_move_space_picker(host);
            if let Some(picker) = host.space_picker.as_mut() {
                picker.kind = SpacePickerKind::MoveSession;
            }
            None
        }
        PaneContextAction::RemoveSessionFromSpace => {
            remove_session_from_space(host, pane, false);
            None
        }
        PaneContextAction::RemoveAndKillSession => {
            remove_session_from_space(host, pane, true);
            None
        }
        PaneContextAction::Detach => Some(keybind::Action::Detach),
        PaneContextAction::Close => Some(keybind::Action::ClosePane),
        PaneContextAction::SaveSpace => {
            if let Some(name) = host.space_rail.current.clone() {
                save_space_from_host(host, &name);
            } else {
                rail_toast(host, " create or open a space first ");
            }
            None
        }
    };
    action.map_or(Dispatch::Handled, |action| {
        dispatch_action(host, action, program, child_args)
    })
}

fn activate_context_menu(
    host: &mut HostState,
    index: usize,
    program: &str,
    child_args: &[String],
) -> Dispatch {
    if host.space_panel.is_some() {
        space_panel::activate(host, index);
        return Dispatch::Handled;
    }
    let Some(target) = host.context_menu_target else {
        return Dispatch::Handled;
    };
    let kind = match target {
        ContextMenuTarget::SpaceChip(_) => ContextMenuKind::SpaceChip,
        ContextMenuTarget::Pane(_) => ContextMenuKind::Pane,
    };
    let confirmed = host
        .context_menu
        .as_ref()
        .is_some_and(|menu| menu.confirm == Some(index));
    if context_menu_needs_confirmation(kind, index, confirmed) {
        if let Some(menu) = host.context_menu.as_mut() {
            menu.confirm = Some(index);
        }
        host.dirty = true;
        host.window.request_redraw();
        return Dispatch::Handled;
    }
    close_context_menu(host);
    match context_menu_action(target, index) {
        ContextMenuAction::Space { chip, action } => {
            apply_space_context_action(host, chip, action);
            Dispatch::Handled
        }
        ContextMenuAction::Pane { pane, action } => {
            apply_pane_context_action(host, pane, action, program, child_args)
        }
        ContextMenuAction::Noop => Dispatch::Handled,
    }
}

fn handle_context_menu_key(
    host: &mut HostState,
    event: &winit::event::KeyEvent,
    program: &str,
    child_args: &[String],
) -> Option<Dispatch> {
    if space_panel::input_key(host, event) {
        return Some(Dispatch::Handled);
    }
    let _menu = host.context_menu.as_ref()?;
    let (_, rows) = context_menu_rows(host)?;
    let logical = event.key_without_modifiers();
    let verdict = host
        .context_menu
        .as_mut()
        .expect("context menu is open")
        .key(&logical, host.modifiers, rows.len());
    Some(match verdict {
        ContextMenuVerdict::Consumed => {
            host.dirty = true;
            Dispatch::Handled
        }
        ContextMenuVerdict::Close => {
            close_context_menu(host);
            Dispatch::Handled
        }
        ContextMenuVerdict::Activate(index) | ContextMenuVerdict::Confirm(index) => {
            activate_context_menu(host, index, program, child_args)
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpaceRailKeyDecision {
    Unhandled,
    Leave,
    Key(space_rail::RailKey),
}

fn space_rail_key_decision(
    active: bool,
    side: space_rail::RailSide,
    modifiers: ModifiersState,
    logical_key: &Key,
) -> SpaceRailKeyDecision {
    use space_rail::{EditStroke, RailKey};
    if !active {
        return SpaceRailKeyDecision::Unhandled;
    }
    if side == space_rail::RailSide::Off
        || modifiers.control_key()
        || modifiers.alt_key()
        || modifiers.super_key()
    {
        return SpaceRailKeyDecision::Leave;
    }
    let vertical = !side.horizontal();
    let key = match logical_key {
        Key::Named(NamedKey::Enter) => RailKey::Enter,
        Key::Named(NamedKey::Escape) => RailKey::Escape,
        Key::Named(NamedKey::Delete) => RailKey::Delete,
        Key::Named(NamedKey::F2) => RailKey::Rename,
        Key::Named(NamedKey::ArrowLeft) if !vertical => RailKey::Prev,
        Key::Named(NamedKey::ArrowRight) if !vertical => RailKey::Next,
        Key::Named(NamedKey::ArrowUp) if vertical => RailKey::Prev,
        Key::Named(NamedKey::ArrowDown) if vertical => RailKey::Next,
        Key::Named(NamedKey::F10) if modifiers.shift_key() => RailKey::Menu,
        Key::Named(NamedKey::ContextMenu) => RailKey::Menu,
        Key::Named(NamedKey::Tab) => {
            if modifiers.shift_key() {
                RailKey::Prev
            } else {
                RailKey::Next
            }
        }
        Key::Named(NamedKey::Home) => RailKey::First,
        Key::Named(NamedKey::End) => RailKey::Last,
        Key::Named(NamedKey::Backspace) => RailKey::Edit(EditStroke::Backspace),
        Key::Named(NamedKey::Space) => RailKey::Edit(EditStroke::Insert(' ')),
        Key::Character(text) => match text.chars().next() {
            Some(ch) => RailKey::Edit(EditStroke::Insert(ch)),
            None => RailKey::Edit(EditStroke::DropSelection),
        },
        _ => RailKey::Edit(EditStroke::DropSelection),
    };
    SpaceRailKeyDecision::Key(key)
}

fn handle_space_rail_key(host: &mut HostState, event: &winit::event::KeyEvent) -> bool {
    let decision = space_rail_key_decision(
        host.space_rail.is_active(),
        host.mux.geom().rail_side,
        host.modifiers,
        &event.logical_key,
    );
    match decision {
        SpaceRailKeyDecision::Unhandled => false,
        SpaceRailKeyDecision::Leave => {
            host.space_rail.leave();
            host.dirty = true;
            false
        }
        SpaceRailKeyDecision::Key(key) => {
            let verdict = host.space_rail.key(key);
            apply_rail_verdict(host, verdict);
            true
        }
    }
}

/// Name editing owns shortcuts too. Ctrl+A or paste must not dismiss the
/// dialog and send the following text to a shell.
fn handle_space_name_key(host: &mut HostState, event: &winit::event::KeyEvent) {
    if event.repeat
        && matches!(
            event.logical_key,
            Key::Named(NamedKey::Enter | NamedKey::Escape)
        )
    {
        return;
    }
    if host.modifiers.control_key() || host.modifiers.super_key() {
        if let Key::Character(text) = &event.logical_key {
            if text.eq_ignore_ascii_case("a") {
                host.space_rail.edit.as_mut().unwrap().selected = true;
            } else if text.eq_ignore_ascii_case("v") {
                if host.clipboard.is_none() {
                    host.clipboard = arboard::Clipboard::new().ok();
                }
                if let Some(text) = host.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
                    for ch in text.trim().chars() {
                        host.space_rail.key(space_rail::RailKey::Edit(
                            space_rail::EditStroke::Insert(ch),
                        ));
                    }
                }
            }
        }
    } else if !host.modifiers.alt_key() {
        handle_space_rail_key(host, event);
    }
    host.dirty = true;
    host.window.request_redraw();
}

fn host_overlay_surface(host: &HostState) -> OverlaySurface {
    #[cfg(target_os = "macos")]
    let native_blur = host.window_blur_active;
    #[cfg(not(target_os = "macos"))]
    let native_blur = false;
    let compositor_blur = host
        .present
        .as_ref()
        .is_some_and(PresentBackend::blur_active);
    OverlaySurface::from_window(
        host.overlay_opacity,
        host.background_blur_px,
        native_blur || compositor_blur,
    )
}

#[allow(clippy::too_many_arguments)]
fn paint_palette_overlay(
    font: &FontMetrics,
    theme: &theme::Theme,
    focus_rgb: [u8; 3],
    frame: &PaletteFrame<'_>,
    surface: OverlaySurface,
    buffer: &mut [u32],
    width: usize,
    height: usize,
) -> Option<PaletteLayout> {
    rasterize_palette(
        font, frame, theme, surface, buffer, width, height, focus_rgb,
    )
}

fn apply_palette_pointer(host: &mut HostState) {
    if host.palette.is_none() && host.space_picker.is_none() && host.context_menu.is_none() {
        return;
    }
    let Some(layout) = host.palette_layout.as_ref() else {
        return;
    };
    let Some((x, y)) = host.pointer_px else {
        return;
    };
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return;
    }
    let Some(row) = palette_hit(layout, x as usize, y as usize) else {
        return;
    };
    if let Some(palette) = host.palette.as_mut() {
        if palette.selected != row {
            palette.selected = row;
            host.dirty = true;
        }
    }
    if let Some(picker) = host.space_picker.as_mut() {
        if picker.selected != row {
            picker.selected = row;
            host.dirty = true;
        }
    }
    if let Some(menu) = host.context_menu.as_mut() {
        if menu.selected != row {
            menu.selected = row;
            host.dirty = true;
        }
    }
}

fn pointer_hover_blocked(host: &HostState) -> bool {
    host.restore_prompt.is_some()
        || host.session_prompt.is_some()
        || host.theme_picker.is_some()
        || host.palette.is_some()
        || host.space_picker.is_some()
        || host.context_menu.is_some()
        || host.splash.is_some()
        || host
            .space_rail
            .edit
            .as_ref()
            .is_some_and(|edit| edit.target.is_none())
}

fn hover_target_at_pointer(host: &HostState) -> Option<HoverTarget> {
    if pointer_hover_blocked(host) {
        return None;
    }
    let (x, y) = host
        .pointer_px
        .filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0)?;
    let (px, py) = (x as usize, y as usize);
    if let Some(band) = walkthrough_band(host) {
        if walkthrough::caption_hover(&band, px, py) {
            return Some(HoverTarget::Caption(walkthrough::caption_hit(
                &band, px, py,
            )));
        }
    }
    let size = host.window.inner_size();
    let stride = size.width as usize;
    if show_tab_strip(host) {
        if let Some(hit) = host
            .mux
            .tab_strip_hit(px, py, stride, reserve_strip_end(host))
        {
            return Some(HoverTarget::Strip(hit));
        }
    }
    if let Some(layout) = host.space_rail.layout(
        host.mux.geom(),
        stride,
        size.height as usize,
        host.spacing.space_rail_pane_names,
    ) {
        match layout.hit(px, py, host.space_rail.names.len()) {
            Some(space_rail::RailHit::Chip { index, close }) => {
                return Some(HoverTarget::Rail(space_rail::RailHit::Chip {
                    index,
                    close,
                }));
            }
            Some(space_rail::RailHit::Plus) => {
                return Some(HoverTarget::Rail(space_rail::RailHit::Plus));
            }
            Some(space_rail::RailHit::Overflow) => {
                return Some(HoverTarget::Rail(space_rail::RailHit::Overflow))
            }
            Some(space_rail::RailHit::Empty) | None => {}
        }
    }
    pane_scrollbar_at(host, px, py)
        .filter(|(_, bar, _)| bar.thumb_contains(py))
        .map(|(pane, _, _)| HoverTarget::ScrollbarThumb(pane))
}

fn cursor_for_hover(
    hover: Option<HoverTarget>,
    strip_dragging: bool,
    scrollbar_dragging: bool,
    divider_axis: Option<prismattyc_mux::Axis>,
    hyperlink: bool,
) -> CursorIcon {
    if let Some(axis) = divider_axis {
        match axis {
            prismattyc_mux::Axis::Horizontal => CursorIcon::ColResize,
            prismattyc_mux::Axis::Vertical => CursorIcon::RowResize,
        }
    } else if strip_dragging || scrollbar_dragging {
        CursorIcon::Grab
    } else if hyperlink
        || matches!(
            hover,
            Some(HoverTarget::Caption(Some(_)))
                | Some(HoverTarget::Strip(_))
                | Some(HoverTarget::Rail(_))
        )
    {
        CursorIcon::Pointer
    } else {
        CursorIcon::Default
    }
}

fn hyperlink_hover_at_pointer(host: &mut HostState) -> bool {
    if pointer_hover_blocked(host) || host.left_button_down {
        return false;
    }
    let Some((x, y)) = host.pointer_px else {
        return false;
    };
    let Some((pane, row, col)) =
        cell_at_position(PhysicalPosition::new(x, y), &host.font, &host.mux)
    else {
        return false;
    };
    let Some(runtime) = host.mux.pane(pane) else {
        return false;
    };
    let screen = runtime.emulator.screen();
    let scroll = runtime.view_scroll.min(screen.max_view_scroll());
    let key = HyperlinkHoverKey {
        pane,
        row,
        col,
        scroll,
        epoch: screen.content_epoch(),
        size: (screen.columns(), screen.rows()),
    };
    if let Some((previous, hit)) = host.hyperlink_hover {
        if previous == key {
            return hit;
        }
    }
    // Reuse click detection, but do not rescan the grid on every pixel of motion.
    let hit = hyperlink::url_at(screen, scroll, row, col).is_some();
    host.hyperlink_hover = Some((key, hit));
    hit
}

/// Refresh chrome hover after geometry or pointer state changes. Raw motion
/// inside one target does not dirty the frame.
fn git_hover_label(host: &HostState) -> Option<String> {
    let tab = match hover_target_at_pointer(host)? {
        HoverTarget::Strip(mux::StripHit::Tab { index, .. }) => index,
        HoverTarget::Strip(mux::StripHit::Pane { tab, .. }) => tab,
        _ => return None,
    };
    let info = host.mux.tab_infos().into_iter().nth(tab)?;
    Some(format!(
        " {} · {} ",
        info.pane_title.unwrap_or(info.title),
        info.git_label?
    ))
}

fn sync_chrome_hover(host: &mut HostState) -> bool {
    let next = hover_target_at_pointer(host);
    let strip_dragging = host.strip_drag.as_ref().is_some_and(|drag| drag.active);
    let divider_axis = divider_axis_for_cursor(host);
    let hyperlink = next.is_none() && hyperlink_hover_at_pointer(host);
    let cursor = cursor_for_hover(
        next,
        strip_dragging,
        host.scrollbar_drag.is_some(),
        divider_axis,
        hyperlink,
    );
    host.window
        .set_cursor(if rail_resize::at_edge(host) || host.rail_resizing {
            CursorIcon::EwResize
        } else {
            cursor
        });
    host.divider_cursor = divider_axis.is_some();
    if host.hover_target == next {
        return false;
    }
    host.hover_target = next;
    host.dirty = true;
    true
}

/// Pointer press on the spaces rail. Returns whether the press was inside
/// the rail. A press anywhere else drops the rail's keyboard focus.
fn handle_rail_click(host: &mut HostState, button: MouseButton) -> bool {
    use space_rail::{RailHit, RailVerdict};
    let Some((x, y)) = host.pointer_px else {
        return false;
    };
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return false;
    }
    let size = host.window.inner_size();
    let Some(layout) = host.space_rail.layout(
        host.mux.geom(),
        size.width as usize,
        size.height as usize,
        host.spacing.space_rail_pane_names,
    ) else {
        return false;
    };
    let n = host.space_rail.names.len();
    let Some(hit) = layout.hit(x as usize, y as usize, n) else {
        if host.space_rail.is_active() {
            host.space_rail.leave();
            host.dirty = true;
        }
        return false;
    };
    cancel_tab_rename(host);
    let was_confirm = host.space_rail.confirm;
    let was_edit = host.space_rail.edit.clone();
    match (button, hit) {
        (MouseButton::Left, RailHit::Chip { index, close: true }) => {
            if was_confirm == Some(index) {
                let verdict = host.space_rail.key(space_rail::RailKey::Enter);
                apply_rail_verdict(host, verdict);
            } else if host.space_rail.current_index() == Some(index) {
                // The marker on the current chip is not a close target.
                host.space_rail.focus = Some(index);
                host.dirty = true;
            } else {
                host.space_rail.begin_confirm(index);
                host.dirty = true;
            }
        }
        (
            MouseButton::Left,
            RailHit::Chip {
                index,
                close: false,
            },
        ) => {
            if was_edit
                .as_ref()
                .is_some_and(|edit| edit.target == Some(index))
            {
                // Click inside the open editor: keep editing.
                return true;
            }
            host.space_rail.leave();
            if let Some(name) = host.space_rail.names.get(index).cloned() {
                apply_rail_verdict(host, RailVerdict::Open(name));
            }
        }
        (MouseButton::Left, RailHit::Overflow) => {
            host.space_picker = Some(SpacePicker::new(SpacePickerKind::Open));
            host.palette_layout = None;
            host.dirty = true;
        }
        (MouseButton::Right, RailHit::Empty | RailHit::Plus | RailHit::Overflow) => {
            space_panel::settings(host)
        }
        (MouseButton::Left, RailHit::Plus) => {
            if was_edit.as_ref().is_some_and(|edit| edit.target.is_none()) {
                return true;
            }
            host.space_rail.begin_new();
            host.dirty = true;
        }
        (MouseButton::Right, RailHit::Chip { index, .. }) => {
            host.space_rail.leave();
            open_context_menu(host, ContextMenuTarget::SpaceChip(index));
        }
        (MouseButton::Middle, RailHit::Chip { index, .. }) => {
            host.space_rail.begin_confirm(index);
            host.dirty = true;
        }
        _ => {
            if host.space_rail.edit.is_some() || host.space_rail.confirm.is_some() {
                host.space_rail.leave();
                host.dirty = true;
            }
        }
    }
    true
}

fn tab_strip_visible(mode: config::TabStripMode, tab_count: usize, multi_pane: bool) -> bool {
    match mode {
        config::TabStripMode::Auto => tab_count > 0 || multi_pane,
        config::TabStripMode::Always => true,
        config::TabStripMode::Multi => tab_count > 1,
    }
}

/// What a strip drag is holding (PT-79).
#[derive(Debug, Clone, PartialEq, Eq)]
enum DragSubject<'a> {
    Tab(&'a str),
    /// 1-based pane index in its tab and the attached session name, if any.
    Pane {
        index: usize,
        session: Option<&'a str>,
    },
}

/// `Moving tab NAME` / `Moving pane N · SESSION` (session omitted for a
/// local shell).
fn drag_toast_subject(subject: &DragSubject<'_>) -> String {
    match subject {
        DragSubject::Tab(name) => format!("Moving tab {name}"),
        DragSubject::Pane {
            index,
            session: Some(session),
        } => format!("Moving pane {index} · {session}"),
        DragSubject::Pane {
            index,
            session: None,
        } => format!("Moving pane {index}"),
    }
}

/// `→ tab NAME` / `→ new tab` / `→ (no target)` for the pointer's strip hit.
fn drag_toast_target(hit: Option<mux::StripHit>, titles: &[String]) -> String {
    match hit {
        Some(mux::StripHit::Tab { index, .. }) | Some(mux::StripHit::Pane { tab: index, .. }) => {
            match titles.get(index) {
                Some(title) => format!("→ tab {title}"),
                None => "→ (no target)".to_string(),
            }
        }
        Some(mux::StripHit::EmptyEnd) => "→ new tab".to_string(),
        None => "→ (no target)".to_string(),
    }
}

/// The live drag toast text, or `None` when no strip drag is active.
fn drag_toast_label(host: &HostState, stride: usize) -> Option<String> {
    let drag = host.strip_drag.as_ref().filter(|drag| drag.active)?;
    let titles: Vec<String> = host
        .mux
        .tab_infos()
        .into_iter()
        .map(|tab| tab.title)
        .collect();
    let subject = match drag.kind {
        StripDragKind::Tab(index) => drag_toast_subject(&DragSubject::Tab(
            titles.get(index).map(String::as_str).unwrap_or("?"),
        )),
        StripDragKind::Pane { tab, pane } => {
            let index = host
                .mux
                .tab_panes()
                .get(tab)
                .and_then(|(_, panes)| panes.iter().position(|p| *p == pane))
                .map_or(1, |position| position + 1);
            drag_toast_subject(&DragSubject::Pane {
                index,
                session: host.mux.attach_name_of(pane),
            })
        }
    };
    let hit = host.pointer_px.and_then(|(x, y)| {
        (x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0)
            .then(|| host.mux.tab_strip_hit(x as usize, y as usize, stride, true))
            .flatten()
    });
    Some(format!(" {subject} {} ", drag_toast_target(hit, &titles)))
}

/// Pixels on each side of a pane gap that still grab the divider (PT-133).
const DIVIDER_SLOP_PX: usize = 3;

fn pointer_cells(host: &HostState) -> Option<(usize, usize)> {
    let (x, y) = host.pointer_px?;
    (x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0).then_some((x as usize, y as usize))
}

/// Left press on the gap between two panes starts a divider drag.
fn handle_divider_press(host: &mut HostState, button: MouseButton) -> bool {
    if button != MouseButton::Left {
        return false;
    }
    let Some((px, py)) = pointer_cells(host) else {
        return false;
    };
    let Some(divider) = host.mux.divider_at(px, py, DIVIDER_SLOP_PX) else {
        return false;
    };
    cancel_tab_rename(host);
    host.divider_drag = Some(divider);
    host.cursor_cell = None;
    update_divider_cursor(host);
    true
}

/// While dragging, the split ratio follows the pointer; the layout refuses
/// a ratio that would shrink a pane below its minimum and stays put.
fn handle_divider_drag_move(host: &mut HostState) -> bool {
    let Some(divider) = host.divider_drag.clone() else {
        return false;
    };
    let Some((px, py)) = pointer_cells(host) else {
        return true;
    };
    let ratio = host.mux.geom().divider_ratio_at(&divider, px, py);
    match host.mux.resize_split(&divider.path, ratio) {
        Ok(true) => {
            host.cursor_cell = None;
            host.dirty = true;
        }
        Ok(false) => {}
        Err(error) => eprintln!("prismattyc-host: divider resize failed: {error:#}"),
    }
    true
}

/// col-resize / row-resize over a divider (or during a drag), default
/// otherwise. Only touches the cursor when the state changes.
fn update_divider_cursor(host: &mut HostState) {
    sync_chrome_hover(host);
}

fn divider_axis_for_cursor(host: &HostState) -> Option<prismattyc_mux::Axis> {
    host.divider_drag
        .as_ref()
        .map(|divider| divider.axis)
        .or_else(|| {
            pointer_cells(host)
                .and_then(|(px, py)| host.mux.divider_at(px, py, DIVIDER_SLOP_PX))
                .map(|divider| divider.axis)
        })
}

fn show_tab_strip(host: &HostState) -> bool {
    tab_strip_visible(
        host.tab_strip_mode,
        host.mux.tab_count(),
        host.mux.active_pane_count() > 1,
    ) || host.strip_drag.as_ref().is_some_and(|drag| drag.active)
}

fn reserve_strip_end(host: &HostState) -> bool {
    host.strip_drag.as_ref().is_some_and(|drag| drag.active)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripClickResult {
    NotHandled,
    Handled,
    Exit,
}

fn close_tab_from_strip(host: &mut HostState, index: usize) -> StripClickResult {
    let result = if host.mux.tab_count() == 1 {
        host.mux.detach_view().map(|view| match view {
            mux::DetachView::ExitHost => StripClickResult::Exit,
            mux::DetachView::ClosedTab => StripClickResult::Handled,
        })
    } else {
        host.mux.close_tab_at(index).map(|closed| {
            if closed {
                StripClickResult::Handled
            } else {
                StripClickResult::NotHandled
            }
        })
    };
    match result {
        Ok(StripClickResult::Exit) => {
            persist_attach_layout_from_live(host);
            StripClickResult::Exit
        }
        Ok(StripClickResult::Handled) => {
            mark_layout_dirty(host);
            App::refit_geom(host, host.window.inner_size(), Some("tab close"));
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            StripClickResult::Handled
        }
        Ok(StripClickResult::NotHandled) => StripClickResult::NotHandled,
        Err(error) => {
            eprintln!("prismattyc-host: tab close failed: {error:#}");
            StripClickResult::Handled
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripClickHit {
    Tab { index: usize, close: bool },
    Pane { tab: usize },
    EmptyEnd,
}

fn strip_click_hit(hit: mux::StripHit) -> StripClickHit {
    match hit {
        mux::StripHit::Tab { index, close } => StripClickHit::Tab { index, close },
        mux::StripHit::Pane { tab, .. } => StripClickHit::Pane { tab },
        mux::StripHit::EmptyEnd => StripClickHit::EmptyEnd,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StripClickDecision {
    NotHandled,
    Handled,
    CancelRename,
    CloseTab(usize),
    StartTabDrag { index: usize, x: f64, y: f64 },
    StartPaneDrag { tab: usize, x: f64, y: f64 },
    RenameTab(usize),
    RenamePane { tab: usize },
}

fn strip_click_decision(
    button: MouseButton,
    hit: Option<StripClickHit>,
    title_row: bool,
    tab_rename_active: bool,
    x: f64,
    y: f64,
) -> StripClickDecision {
    let Some(hit) = hit else {
        return if tab_rename_active {
            StripClickDecision::CancelRename
        } else {
            StripClickDecision::NotHandled
        };
    };
    match button {
        MouseButton::Left => match hit {
            StripClickHit::Tab { index, close: true } if title_row => {
                StripClickDecision::CloseTab(index)
            }
            StripClickHit::Tab {
                index,
                close: false,
            } => StripClickDecision::StartTabDrag { index, x, y },
            StripClickHit::Pane { tab } => StripClickDecision::StartPaneDrag { tab, x, y },
            StripClickHit::EmptyEnd | StripClickHit::Tab { .. } => StripClickDecision::Handled,
        },
        MouseButton::Right => match hit {
            StripClickHit::Tab { index, .. } if title_row => StripClickDecision::RenameTab(index),
            StripClickHit::Pane { tab } => StripClickDecision::RenamePane { tab },
            StripClickHit::Tab { .. } | StripClickHit::EmptyEnd => StripClickDecision::Handled,
        },
        MouseButton::Middle => {
            if tab_rename_active && matches!(hit, StripClickHit::EmptyEnd) {
                return StripClickDecision::CancelRename;
            }
            let tab = match hit {
                StripClickHit::Tab { index, .. } => Some(index),
                StripClickHit::Pane { tab } => Some(tab),
                StripClickHit::EmptyEnd => None,
            };
            tab.map_or(StripClickDecision::Handled, StripClickDecision::CloseTab)
        }
        _ => {
            if tab_rename_active {
                StripClickDecision::CancelRename
            } else {
                StripClickDecision::NotHandled
            }
        }
    }
}

fn handle_strip_click(host: &mut HostState, button: MouseButton) -> StripClickResult {
    if !show_tab_strip(host) {
        return StripClickResult::NotHandled;
    }
    let Some((x, y)) = host.pointer_px else {
        return StripClickResult::NotHandled;
    };
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return StripClickResult::NotHandled;
    }
    let stride = host.window.inner_size().width as usize;
    let hit = host
        .mux
        .tab_strip_hit(x as usize, y as usize, stride, reserve_strip_end(host));
    let title_row = (y as usize)
        .checked_sub(host.mux.geom().tab_strip_y())
        .is_some_and(|local_y| local_y < host.font.cell_h.max(1));
    match strip_click_decision(
        button,
        hit.map(strip_click_hit),
        title_row,
        host.tab_rename.is_some(),
        x,
        y,
    ) {
        StripClickDecision::NotHandled => StripClickResult::NotHandled,
        StripClickDecision::Handled => StripClickResult::Handled,
        StripClickDecision::CancelRename => {
            cancel_tab_rename(host);
            StripClickResult::Handled
        }
        StripClickDecision::CloseTab(index) => {
            cancel_tab_rename(host);
            close_tab_from_strip(host, index)
        }
        StripClickDecision::StartTabDrag { index, x, y } => {
            cancel_tab_rename(host);
            host.strip_drag = Some(StripDrag {
                kind: StripDragKind::Tab(index),
                start_x: x,
                start_y: y,
                active: false,
            });
            StripClickResult::Handled
        }
        StripClickDecision::StartPaneDrag { tab, x, y } => {
            let Some(mux::StripHit::Pane { pane, .. }) = hit else {
                return StripClickResult::NotHandled;
            };
            cancel_tab_rename(host);
            host.strip_drag = Some(StripDrag {
                kind: StripDragKind::Pane { tab, pane },
                start_x: x,
                start_y: y,
                active: false,
            });
            StripClickResult::Handled
        }
        StripClickDecision::RenamePane { tab } => {
            let Some(mux::StripHit::Pane { pane, .. }) = hit else {
                return StripClickResult::NotHandled;
            };
            let _ = host.mux.select_tab(tab);
            host.mux.focus(pane);
            open_context_menu(host, ContextMenuTarget::Pane(pane));
            StripClickResult::Handled
        }
        StripClickDecision::RenameTab(index) => {
            let _ = host.mux.select_tab(index);
            if host.mux.active_pane_count() == 1 {
                let pane = host.mux.focused_id();
                open_context_menu(host, ContextMenuTarget::Pane(pane));
            } else {
                begin_tab_rename(host, Some(index));
            }
            StripClickResult::Handled
        }
    }
}

fn handle_strip_drag_move(host: &mut HostState) -> bool {
    let Some((x, y)) = host.pointer_px else {
        return false;
    };
    let Some(drag) = host.strip_drag.as_mut() else {
        return false;
    };
    if drag.active {
        return true;
    }
    let dx = x - drag.start_x;
    let dy = y - drag.start_y;
    if dx * dx + dy * dy < 16.0 {
        return true;
    }
    drag.active = true;
    host.dirty = true;
    true
}

fn finish_strip_drag(host: &mut HostState) -> bool {
    let Some(drag) = host.strip_drag.take() else {
        return false;
    };
    let Some((x, y)) = host.pointer_px else {
        return true;
    };
    if !drag.active {
        match drag.kind {
            StripDragKind::Tab(index) => {
                if host.mux.select_tab(index).unwrap_or(false) {
                    mark_layout_dirty(host);
                    App::refit_geom(host, host.window.inner_size(), Some("tab select"));
                    host.window
                        .set_title(&window_title(&host.mux, show_tab_strip(host)));
                    host.dirty = true;
                }
            }
            StripDragKind::Pane { tab, pane } => {
                let selected = host.mux.select_tab(tab).unwrap_or(false);
                let focused = host.mux.focus(pane);
                if selected || focused {
                    mark_layout_dirty(host);
                    App::refit_geom(host, host.window.inner_size(), Some("pane focus"));
                    host.window
                        .set_title(&window_title(&host.mux, show_tab_strip(host)));
                    host.dirty = true;
                }
            }
        }
        return true;
    }
    let stride = host.window.inner_size().width as usize;
    let hit = host.mux.tab_strip_hit(x as usize, y as usize, stride, true);
    let changed = match (drag.kind, hit) {
        (StripDragKind::Tab(from), Some(mux::StripHit::Tab { index: to, .. })) => {
            host.mux.reorder_tab(from, to).unwrap_or(false)
        }
        (StripDragKind::Tab(from), Some(mux::StripHit::EmptyEnd)) => {
            let last = host.mux.tab_count().saturating_sub(1);
            host.mux.reorder_tab(from, last).unwrap_or(false)
        }
        (StripDragKind::Pane { pane, .. }, Some(mux::StripHit::Tab { index, .. })) => host
            .mux
            .window_at_tab(index)
            .map(|dest| host.mux.move_pane_to_window(pane, dest).unwrap_or(false))
            .unwrap_or(false),
        (StripDragKind::Pane { pane, .. }, Some(mux::StripHit::EmptyEnd)) => {
            host.mux.move_pane_to_new_tab(pane).unwrap_or(false)
        }
        _ => false,
    };
    if changed {
        mark_layout_dirty(host);
        host.window
            .set_title(&window_title(&host.mux, show_tab_strip(host)));
        host.dirty = true;
    }
    App::refit_geom(host, host.window.inner_size(), Some("tab drag"));
    true
}

/// True when every `layout_N` binding is `layout_2`'s with `f2`→`fN` and
/// `+2`→`+N`, so one "Fn"/"n" summary describes them all.
fn layouts_share_pattern(keymap: &keybind::KeyMap) -> bool {
    let base = keymap.spellings(keybind::Action::Layout(2));
    (3..=9u8).all(|n| {
        let expected: Vec<String> = base
            .iter()
            .map(|s| {
                s.replace("f2", &format!("f{n}"))
                    .replace("+2", &format!("+{n}"))
            })
            .collect();
        keymap.spellings(keybind::Action::Layout(n)) == expected
    })
}

fn chord_help_text(mux: &mux::MuxRuntime, keymap: &keybind::KeyMap, show_tabs: bool) -> String {
    use keybind::Action;
    let unseen = mux.unseen_count();
    let active = mux.active_count();
    let mut badge = String::new();
    if active > 0 {
        badge.push_str(&format!(" | *{active}"));
    }
    if unseen > 0 {
        badge.push_str(&format!(" | !{unseen}"));
    }
    // One segment per bound action; an unbound action leaves no segment.
    let seg = |label: String, what: &str| {
        if label.is_empty() {
            String::new()
        } else {
            format!(" | {label} {what}")
        }
    };
    // Pair labels sharing a prefix: "C-S-[/]", "C-S-PgUp/PgDn".
    let pair = |first: Action, second: Action| {
        let (a, b) = (keymap.label(first), keymap.label(second));
        match (a.is_empty(), b.is_empty()) {
            (true, true) => String::new(),
            (false, true) => a,
            (true, false) => b,
            (false, false) => format!("{a}/{}", b.rsplit('-').next().unwrap_or(&b)),
        }
    };
    // Even layouts: one summary from layout_2 ("C-S-Fn/C-A-n") when every
    // layout_N follows the same pattern; otherwise the layout_2 label with
    // an ellipsis, so a rebinding of one layout is not implied for all. The
    // macOS Cmd+Shift and the Alt-co-held aliases are omitted from the strip.
    let even = if layouts_share_pattern(keymap) {
        keymap
            .chords(Action::Layout(2))
            .iter()
            .filter(|c| !(c.super_key || (c.shift && c.alt)))
            .map(|c| c.label().replace("F2", "Fn").replace('2', "n"))
            .collect::<Vec<_>>()
            .join("/")
    } else {
        let label = keymap.label(Action::Layout(2));
        if label.is_empty() {
            label
        } else {
            format!("{label}\u{2026}")
        }
    };
    let focus = {
        let default_alt_arrow = keymap.chords(Action::FocusLeft).iter().any(|c| {
            c.alt
                && !c.ctrl
                && !c.shift
                && !c.super_key
                && c.key == keybind::KeySpec::Named(NamedKey::ArrowLeft)
        });
        if default_alt_arrow {
            "Alt+arrow".to_string()
        } else {
            keymap.label(Action::FocusLeft).replace("Left", "arrow")
        }
    };
    let tabs = if show_tabs {
        format!(
            "{}{}{}",
            seg(keymap.label(Action::NewTab), "tab"),
            seg(keymap.label(Action::RenameTab), "rename"),
            seg(pair(Action::PrevTab, Action::NextTab), "")
        )
    } else {
        String::new()
    };
    // Zoomed: the domain pane count plus a Z (tmux style), not the one
    // visible rect, so the strip still says how many panes the tab holds.
    let panes = if mux.is_zoomed() {
        format!("{}Z", mux.active_pane_count())
    } else {
        mux.pane_count().to_string()
    };
    let mut out = format!(" Prismattyc [{panes}]");
    out.push_str(&seg(keymap.label(Action::ThemePicker), "themes"));
    out.push_str(&seg(keymap.label(Action::Paste), "paste"));
    out.push_str(&seg(keymap.label(Action::Copy), "copy"));
    out.push_str(&seg(keymap.label(Action::SplitRight), "split>"));
    out.push_str(&seg(keymap.label(Action::SplitDown), "splitv"));
    out.push_str(&seg(even, "even"));
    out.push_str(&seg(keymap.label(Action::ClosePane), "close"));
    out.push_str(&seg(keymap.label(Action::Detach), "detach"));
    out.push_str(&seg(
        pair(Action::FocusBorderPrev, Action::FocusBorderNext),
        "color",
    ));
    if !focus.is_empty() {
        out.push_str(&format!(" | {focus}"));
    }
    out.push_str(&tabs);
    out.push_str(&badge);
    out.trim_end().to_string()
}

/// Fixed find fallbacks (keybindings D-K3): punctuation chords that match the
/// nested host, because outer terminals often steal Ctrl+Shift+F. The
/// primary chord is the `find` action in the key table.
fn is_find_fallback_chord(logical: &Key, modifiers: ModifiersState) -> bool {
    modifiers.control_key()
        && modifiers.shift_key()
        && !modifiers.alt_key()
        && (is_logical_char(logical, '/')
            || is_logical_char(logical, '?')
            || is_logical_char(logical, ';')
            || is_logical_char(logical, ':')
            || is_logical_char(logical, '\'')
            || is_logical_char(logical, '"')
            || is_logical_char(logical, '.')
            || is_logical_char(logical, '>'))
}

fn is_scroll_slash_find(logical: &Key, modifiers: ModifiersState, view_scroll: usize) -> bool {
    view_scroll > 0
        && !modifiers.control_key()
        && !modifiers.alt_key()
        && !modifiers.shift_key()
        && is_logical_char(logical, '/')
}

fn find_prompt_label(query: &str, rank: Option<(usize, usize)>) -> String {
    match rank {
        Some((i, n)) => format!(" Find: {query}█ {i}/{n} "),
        None if query.is_empty() => " Find: █ ".to_string(),
        None => format!(" Find: {query}█ 0/0 "),
    }
}

fn close_find(find: &mut FindMode) {
    find.active = false;
    find.query.clear();
    find.last = None;
    find.rank = None;
}

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
        selection.clear();
        find.last = None;
        find.rank = None;
        return;
    };
    find.last = Some(m);
    find.rank = emulator.screen().history_match_rank(&find.query, m, false);
    *view_scroll = emulator.screen().view_scroll_for_history_row(m.abs_row);
    selection.set_range(m.abs_row, m.start_col, m.abs_row, m.end_col);
    selection.dragged = true;
}

fn load_space_picker_rows() -> Vec<SpacePickerRow> {
    let Ok(entries) = prismattyc_mux::list_spaces(&spaces_dir()) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .map(|entry| SpacePickerRow {
            name: entry.name,
            sessions: entry.sessions,
            saved_at_unix: entry.saved_at_unix,
        })
        .collect()
}

fn space_picker_rows(kind: SpacePickerKind, current: Option<&str>) -> Vec<SpacePickerRow> {
    let mut spaces = load_space_picker_rows();
    if matches!(
        kind,
        SpacePickerKind::MovePane | SpacePickerKind::MoveSession
    ) {
        if let Some(current) = current {
            spaces.retain(|row| row.name != current);
        }
    }
    spaces
}

/// Whether the focused pane can move into `target`.
fn run_pmux_space(args: &[String]) -> Result<(), String> {
    let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
    match std::process::Command::new(pmux_bin())
        .args(&args_ref)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("pmux {} exited {status}", args.join(" "))),
        Err(error) => Err(format!("could not run pmux: {error}")),
    }
}

/// Release saved membership, then detach every local view of that session.
fn remove_session_from_space(host: &mut HostState, pane: PaneId, kill: bool) {
    let Some(session) = host
        .mux
        .attach_name_of(pane)
        .or_else(|| host.mux.attach_session_of(pane))
        .map(str::to_string)
    else {
        rail_toast(host, " this pane is not an attached session ");
        return;
    };
    let Some(space) = resolve_host_space(host) else {
        rail_toast(host, " open a saved space first ");
        return;
    };
    let Some(name) = host.space_rail.current.clone() else {
        return;
    };
    if !space.sessions.iter().any(|saved| saved.name == session) {
        rail_toast(host, " this session is not saved in the current space ");
        return;
    }
    let target = if kill {
        host.mux
            .attach_session_of(pane)
            .filter(|key| key.parse::<u64>().is_ok())
            .unwrap_or(&session)
            .to_string()
    } else {
        session.clone()
    };
    let mut args = vec![
        "space".into(),
        "remove".into(),
        name.clone(),
        "--session".into(),
        target,
    ];
    let undo_file = spaces_polish::undo_path(host);
    if kill {
        args.push("--kill".into());
    } else {
        let _ = std::fs::remove_file(&undo_file);
        args.extend([
            "--undo-file".into(),
            undo_file.to_string_lossy().into_owned(),
        ]);
    }
    if let Err(error) = run_pmux_space(&args) {
        rail_toast(host, &format!(" remove failed: {error} "));
        return;
    }
    if !kill && undo_file.exists() {
        host.space_polish.undo = Some(undo_file);
    }
    let panes: Vec<_> = host
        .mux
        .tab_panes()
        .into_iter()
        .flat_map(|(_, panes)| panes)
        .filter(|pane| {
            host.mux
                .attach_name_of(*pane)
                .or_else(|| host.mux.attach_session_of(*pane))
                == Some(session.as_str())
        })
        .collect();
    for pane in panes {
        let tab = focused_tab_index(host, pane);
        let result = host.mux.select_tab(tab).and_then(|_| {
            host.mux.focus(pane);
            if host.mux.active_pane_count() == 1 && host.mux.tab_count() == 1 {
                host.mux.empty_space_view(pane)?;
                host.mux
                    .rename_window(host.mux.active_window(), "Empty space")
                    .map(|_| ())
            } else {
                host.mux.close_focused().map(|_| ())
            }
        });
        if let Err(error) = result {
            rail_toast(
                host,
                &format!(" removed from {name}; view refresh failed: {error} "),
            );
            return;
        }
        if let Some(id) = host.attach_pane_sessions.remove(&pane) {
            host.observed_space_sessions.remove(&id);
        }
    }
    persist_attach_layout_from_live(host);
    host.last_space_refresh = None;
    refresh_rail(host);
    App::refit_geom(
        host,
        host.window.inner_size(),
        Some("remove session from space"),
    );
    let outcome = if kill {
        "session killed"
    } else {
        "session kept · Undo: Spaces menu"
    };
    rail_toast(host, &format!(" removed {session} from {name}; {outcome} "));
}

/// Transfer one daemon identity. The daemon validates exclusive ownership.
fn move_to_space_from_host(host: &mut HostState, target: &str, whole_session: bool) {
    let selected = match move_target::take_valid(host) {
        Ok(target) => target,
        Err(error) => {
            rail_toast(host, &format!("Move cancelled: {error}"));
            return;
        }
    };
    let Some(remote) = selected.remote.as_ref() else {
        if let Err(error) = local_views::move_blank(host, target) {
            rail_toast(host, &format!("Move failed: {error}"));
        }
        return;
    };
    let pane = selected.host_pane;
    let (flag, identity) = if whole_session {
        ("--session-id", remote.session.to_string())
    } else {
        ("--pane", remote.pane.to_string())
    };
    let undo_file = spaces_polish::undo_path(host);
    let _ = std::fs::remove_file(&undo_file);
    let args = vec![
        "space".into(),
        "move".into(),
        target.into(),
        flag.into(),
        identity,
        "--undo-file".into(),
        undo_file.to_string_lossy().into_owned(),
    ];
    match run_pmux_space(&args) {
        Ok(()) => {
            if undo_file.exists() {
                host.space_polish.undo = Some(undo_file);
            }
            // Stop writes to the transferred pane immediately. Other windows
            // reconcile against the same daemon ownership snapshot.
            if !selected.viewers.is_empty() {
                if let Err(error) = move_target::detach_viewer(&selected) {
                    rail_toast(
                        host,
                        &format!("Moved; could not detach nested viewer: {error}"),
                    );
                }
            } else {
                if let Some(id) = host.attach_pane_sessions.get(&pane) {
                    host.observed_space_sessions.remove(id);
                }
                if host.mux.active_pane_count() == 1 && host.mux.tab_count() == 1 {
                    if let Err(error) = host.mux.empty_space_view(pane) {
                        rail_toast(host, &format!(" moved; view refresh failed: {error} "));
                        return;
                    }
                } else if let Err(error) = host.mux.close_focused() {
                    rail_toast(host, &format!(" moved; view refresh failed: {error} "));
                    return;
                }
                host.attach_pane_sessions.remove(&pane);
            }
            persist_attach_layout_from_live(host);
            host.last_space_refresh = None;
            refresh_rail(host);
            rail_toast(host, &format!(" moved to {target} · Undo: Spaces menu "));
        }
        Err(error) => rail_toast(host, &format!(" move failed: {error} ")),
    }
}

fn handle_space_picker_key(
    host: &mut HostState,
    event: &winit::event::KeyEvent,
    action: Option<keybind::Action>,
) -> bool {
    if host.space_picker.is_none() {
        let kind = match action {
            Some(keybind::Action::OpenSpace) => SpacePickerKind::Open,
            Some(keybind::Action::DeleteSpace) => SpacePickerKind::Delete,
            Some(keybind::Action::MovePaneToSpace) => {
                if !move_target::begin(host) {
                    return true;
                }
                SpacePickerKind::MovePane
            }
            _ => return false,
        };
        host.space_picker = Some(SpacePicker::new(kind));
        host.palette_layout = None;
        host.tab_rename = None;
        host.window.set_title("Prismattyc — spaces");
        host.dirty = true;
        sync_chrome_hover(host);
        return true;
    }
    let logical = event.key_without_modifiers();
    let kind = host
        .space_picker
        .as_ref()
        .expect("space picker is open")
        .kind;
    let spaces = terminal_switcher::rows(host, kind);
    let verdict = host
        .space_picker
        .as_mut()
        .expect("space picker is open")
        .key(&logical, host.modifiers, &spaces);
    apply_space_picker_verdict(host, kind, verdict)
}
fn apply_space_picker_verdict(
    host: &mut HostState,
    kind: SpacePickerKind,
    verdict: SpacePickerVerdict,
) -> bool {
    match verdict {
        SpacePickerVerdict::Consumed => {
            host.dirty = true;
            true
        }
        SpacePickerVerdict::Close => {
            host.move_target = None;
            host.terminal_targets = None;
            host.space_picker = None;
            host.palette_layout = None;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            sync_chrome_hover(host);
            true
        }
        SpacePickerVerdict::Open(name) => {
            host.space_picker = None;
            host.palette_layout = None;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            if !terminal_switcher::activate(host, &name) {
                open_space_from_host(host, &name, SpaceOpenMode::Switch);
            }
            sync_chrome_hover(host);
            true
        }
        SpacePickerVerdict::Move(name) => {
            host.space_picker = None;
            host.palette_layout = None;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            move_to_space_from_host(host, &name, kind == SpacePickerKind::MoveSession);
            sync_chrome_hover(host);
            true
        }
        SpacePickerVerdict::Deleted(name) => {
            if let Err(error) = run_pmux_space(&["space".into(), "rm".into(), name.clone()]) {
                rail_toast(host, &format!(" delete failed: {error} "));
            }
            refresh_rail(host);
            if let Some(picker) = host.space_picker.as_mut() {
                picker.status = Some(format!("deleted {name}"));
            }
            host.dirty = true;
            true
        }
    }
}

/// Modal command palette. While open, every pressed key remains host-owned.
/// Enter closes the palette before the selected action is dispatched.
fn open_command_palette(host: &mut HostState, action: keybind::Action) {
    if host.theme_picker.is_some() || host.find.active {
        return;
    }
    if host.palette.is_some() {
        observe_host_action(host, keybind::Action::CommandPalette, true);
        return;
    }
    let mut palette = Palette::with_recent(host.palette_recent.clone());
    match action {
        keybind::Action::PaletteFilterNext => palette.cycle_filter(1),
        keybind::Action::PaletteFilterPrev => palette.cycle_filter(-1),
        _ => {}
    }
    host.palette = Some(palette);
    host.palette_layout = None;
    host.tab_rename = None;
    host.window.set_title("Prismattyc — command palette");
    host.dirty = true;
    sync_chrome_hover(host);
    observe_host_action(host, keybind::Action::CommandPalette, true);
}

fn open_find_prompt(host: &mut HostState) {
    if host.emulator.screen().alt_active() {
        return;
    }
    if host.find.active {
        observe_host_action(host, keybind::Action::Find, true);
        return;
    }
    host.find.active = true;
    host.find.query.clear();
    host.find.last = None;
    host.find.rank = None;
    host.tab_rename = None;
    host.dirty = true;
    observe_host_action(host, keybind::Action::Find, true);
}

fn handle_palette_key(
    host: &mut HostState,
    event: &winit::event::KeyEvent,
    action: Option<keybind::Action>,
) -> PaletteVerdict {
    if host.palette.is_none() {
        let opens = matches!(
            action,
            Some(keybind::Action::CommandPalette)
                | Some(keybind::Action::PaletteFilterNext)
                | Some(keybind::Action::PaletteFilterPrev)
        );
        if host.theme_picker.is_some() || host.find.active || !opens {
            return PaletteVerdict::NotHandled;
        }
        open_command_palette(host, action.unwrap_or(keybind::Action::CommandPalette));
        return PaletteVerdict::Consumed;
    }

    let keymap = host.keymap.clone();
    let typed = palette::typed_character_key(
        &event.key_without_modifiers(),
        &event.logical_key,
        event.text.as_deref(),
        host.modifiers,
    );
    let result = host.palette.as_mut().expect("palette is open").key(
        &typed,
        host.modifiers,
        action,
        &keymap,
        host.experimental_rich,
    );
    match result {
        PaletteVerdict::Consumed => PaletteVerdict::Consumed,
        PaletteVerdict::Close => {
            host.palette = None;
            host.palette_layout = None;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            sync_chrome_hover(host);
            PaletteVerdict::Consumed
        }
        PaletteVerdict::Run(action) => {
            host.palette = None;
            host.palette_layout = None;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            sync_chrome_hover(host);
            PaletteVerdict::Run(action)
        }
        PaletteVerdict::NotHandled => unreachable!("Palette::key never returns NotHandled"),
    }
}

/// Modal find overlay. While open, keys stay host-owned (no PTY inject).
/// `action` is the key table's verdict for this (non-repeat) event.
fn handle_find_key(
    host: &mut HostState,
    event: &winit::event::KeyEvent,
    action: Option<keybind::Action>,
) -> bool {
    if host.emulator.screen().alt_active() {
        let had = host.find.active;
        close_find(&mut host.find);
        if had {
            host.dirty = true;
        }
        return false;
    }
    let logical = event.key_without_modifiers();
    if !host.find.active {
        let open = action == Some(keybind::Action::Find)
            || is_find_fallback_chord(&logical, host.modifiers)
            || is_scroll_slash_find(&logical, host.modifiers, host.view_scroll);
        if !open || event.repeat {
            return false;
        }
        open_find_prompt(host);
        return true;
    }

    if matches!(logical, Key::Named(NamedKey::Escape)) {
        close_find(&mut host.find);
        host.selection.clear();
        host.dirty = true;
        return true;
    }
    if is_copy_chord(&logical, host.modifiers, host.selection.range().is_some()) {
        let _ = copy_selection_native(host);
        return true;
    }
    let shift = host.modifiers.shift_key();
    let ctrl = host.modifiers.control_key();
    let alt = host.modifiers.alt_key();
    if matches!(logical, Key::Named(NamedKey::Enter | NamedKey::F3)) && !ctrl && !alt {
        let pane = host.mux.focused_mut();
        apply_find_step(
            &mut host.find,
            &mut pane.selection,
            &mut pane.view_scroll,
            &pane.emulator,
            shift,
        );
        host.dirty = true;
        return true;
    }
    if matches!(logical, Key::Named(NamedKey::Backspace)) && !ctrl && !alt {
        host.find.query.pop();
        host.find.last = None;
        host.find.rank = None;
        if host.find.query.is_empty() {
            host.mux.focused_mut().selection.clear();
        } else {
            let pane = host.mux.focused_mut();
            apply_find_step(
                &mut host.find,
                &mut pane.selection,
                &mut pane.view_scroll,
                &pane.emulator,
                false,
            );
        }
        host.dirty = true;
        return true;
    }
    if !ctrl && !alt {
        if let Key::Character(ref text) = logical {
            if text.chars().count() == 1 {
                if let Some(c) = text.chars().next().filter(|c| !c.is_control()) {
                    host.find.query.push(c);
                    host.find.last = None;
                    host.find.rank = None;
                    let pane = host.mux.focused_mut();
                    apply_find_step(
                        &mut host.find,
                        &mut pane.selection,
                        &mut pane.view_scroll,
                        &pane.emulator,
                        false,
                    );
                    host.dirty = true;
                    return true;
                }
            }
        }
    }
    // Swallow remaining keys so they never reach the PTY.
    true
}

fn cycle_theme_index(selected: Option<usize>, count: usize, forward: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (selected, forward) {
        (None, true) => 0,
        (None, false) => count - 1,
        (Some(index), true) => (index + 1) % count,
        (Some(index), false) => (index + count - 1) % count,
    })
}

fn picker_items(family: Option<&str>) -> Vec<theme::PickerItem> {
    let root = theme::picker_root(theme::builtins());
    let Some(key) = family else {
        return root;
    };
    root.into_iter()
        .find_map(|item| match item {
            theme::PickerItem::Family {
                key: family_key,
                members,
                ..
            } if family_key == key => Some(
                members
                    .into_iter()
                    .map(|index| theme::PickerItem::Theme { index })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn picker_root_row_for_theme(theme_id: &str) -> Option<usize> {
    theme::picker_root(theme::builtins())
        .iter()
        .position(|item| match item {
            theme::PickerItem::Theme { index } => theme::builtins()
                .get(*index)
                .is_some_and(|theme| theme.id == theme_id),
            theme::PickerItem::Family { members, .. } => members.iter().any(|&index| {
                theme::builtins()
                    .get(index)
                    .is_some_and(|theme| theme.id == theme_id)
            }),
        })
}

fn preview_picker_row(host: &mut HostState, item: &theme::PickerItem) {
    let Some(index) = item.preview_index(&host.theme.id, theme::builtins()) else {
        return;
    };
    host.theme = theme::builtins()[index].clone();
}

fn theme_picker_scroll_for_selection(
    scroll: usize,
    selected: Option<usize>,
    visible_rows: usize,
    count: usize,
) -> usize {
    let visible_rows = visible_rows.max(1);
    let max_scroll = count.saturating_sub(visible_rows);
    let mut scroll = scroll.min(max_scroll);
    let Some(selected) = selected else {
        return scroll;
    };
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll.saturating_add(visible_rows) {
        scroll = selected.saturating_add(1).saturating_sub(visible_rows);
    }
    scroll.min(max_scroll)
}

/// Modal theme settings. While open, every pressed key remains host-owned;
/// arrows preview, Enter persists through the config seam, and Escape restores
/// the exact pre-picker theme (including a custom file theme).
fn handle_theme_picker_key(
    host: &mut HostState,
    event: &winit::event::KeyEvent,
    action: Option<keybind::Action>,
) -> bool {
    if host.theme_picker.is_none() {
        if action != Some(keybind::Action::ThemePicker) {
            return false;
        }
        let selected = picker_root_row_for_theme(&host.theme.id);
        let count = picker_items(None).len();
        let size = host.window.inner_size();
        let visible_rows =
            theme_picker_visible_rows(&host.font, count, size.width as usize, size.height as usize);
        host.theme_picker = Some(ThemePicker {
            original: host.theme.clone(),
            selected,
            family: None,
            scroll: theme_picker_scroll_for_selection(0, selected, visible_rows, count),
        });
        host.tab_rename = None;
        host.window.set_title("Prismattyc — theme settings");
        host.dirty = true;
        sync_chrome_hover(host);
        return true;
    }

    let logical = event.key_without_modifiers();
    if matches!(logical, Key::Named(NamedKey::Escape)) {
        let picker = host.theme_picker.take().expect("picker is open");
        host.theme = picker.original;
        host.window
            .set_title(&window_title(&host.mux, show_tab_strip(host)));
        host.dirty = true;
        sync_chrome_hover(host);
        return true;
    }
    if matches!(logical, Key::Named(NamedKey::Enter)) && !event.repeat {
        let (family, selected) = host
            .theme_picker
            .as_ref()
            .map(|picker| (picker.family.clone(), picker.selected))
            .expect("picker is open");
        let items = picker_items(family.as_deref());
        let Some(row) = selected.and_then(|index| items.get(index)) else {
            host.theme_picker = None;
            host.window
                .set_title(&window_title(&host.mux, show_tab_strip(host)));
            host.dirty = true;
            sync_chrome_hover(host);
            return true;
        };
        let Some(theme_index) = row.preview_index(&host.theme.id, theme::builtins()) else {
            return true;
        };
        let chosen = &theme::builtins()[theme_index];
        match config::save_theme(&config::config_path(), &chosen.id) {
            Ok(()) => {
                host.theme = chosen.clone();
                host.theme_picker = None;
                host.window
                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
            }
            Err(error) => {
                let message = format!("theme save failed: {error:#}");
                eprintln!("prismattyc-host: {message}");
                host.config_error = Some(message);
            }
        }
        host.dirty = true;
        sync_chrome_hover(host);
        return true;
    }

    let family = host
        .theme_picker
        .as_ref()
        .and_then(|picker| picker.family.clone());
    let items = picker_items(family.as_deref());
    let count = items.len();
    let size = host.window.inner_size();
    let visible_rows =
        theme_picker_visible_rows(&host.font, count, size.width as usize, size.height as usize);
    let current = host
        .theme_picker
        .as_ref()
        .and_then(|picker| picker.selected);
    let page = visible_rows.max(1);

    if matches!(logical, Key::Named(NamedKey::ArrowRight)) {
        if let Some(theme::PickerItem::Family { key, members, .. }) =
            current.and_then(|index| items.get(index))
        {
            let inner = members
                .iter()
                .position(|&index| theme::builtins()[index].id == host.theme.id)
                .unwrap_or(0);
            let theme_index = members[inner];
            let picker = host.theme_picker.as_mut().expect("picker is open");
            picker.family = Some(key.clone());
            picker.selected = Some(inner);
            picker.scroll = 0;
            host.theme = theme::builtins()[theme_index].clone();
            host.dirty = true;
        }
        return true;
    }
    if matches!(logical, Key::Named(NamedKey::ArrowLeft)) {
        if family.is_some() {
            let row = picker_root_row_for_theme(&host.theme.id);
            let picker = host.theme_picker.as_mut().expect("picker is open");
            picker.family = None;
            picker.selected = row;
            picker.scroll = 0;
            host.dirty = true;
        }
        return true;
    }

    let next = match logical {
        Key::Named(NamedKey::ArrowDown) => cycle_theme_index(current, count, true),
        Key::Named(NamedKey::ArrowUp) => cycle_theme_index(current, count, false),
        Key::Named(NamedKey::PageDown) if count > 0 => Some(
            current
                .map(|index| index.saturating_add(page).min(count.saturating_sub(1)))
                .unwrap_or(0),
        ),
        Key::Named(NamedKey::PageUp) if count > 0 => {
            Some(current.unwrap_or(0).saturating_sub(page))
        }
        Key::Named(NamedKey::Home) if count > 0 => Some(0),
        Key::Named(NamedKey::End) if count > 0 => Some(count - 1),
        _ => return true,
    };
    if let Some(index) = next {
        if let Some(item) = items.get(index) {
            preview_picker_row(host, item);
        }
        let picker = host.theme_picker.as_mut().expect("picker is open");
        picker.selected = Some(index);
        picker.scroll =
            theme_picker_scroll_for_selection(picker.scroll, picker.selected, visible_rows, count);
        host.dirty = true;
    }
    true
}

fn window_title(mux: &mux::MuxRuntime, show_tabs: bool) -> String {
    let mut title = if show_tabs {
        format!(
            "Prismattyc — {} tabs — {} panes",
            mux.tab_count(),
            mux.pane_count()
        )
    } else {
        format!("Prismattyc — {} panes", mux.pane_count())
    };
    if env_flag_enabled_default_true("PRISMATTYC_SCROLL_TITLE") {
        let pane = mux.focused();
        let max = pane.emulator.screen().max_view_scroll();
        let scroll = pane.view_scroll.min(max);
        if scroll > 0 {
            if pane.scroll_new_output {
                title.push_str(&format!(" — scroll {scroll}/{max} · new"));
            } else {
                title.push_str(&format!(" — scroll {scroll}/{max}"));
            }
        }
    }
    let active = mux.active_count();
    if active > 0 {
        title.push_str(&format!(" — {active} active"));
    }
    let unseen = mux.unseen_count();
    if unseen > 0 {
        title.push_str(&format!(" — {unseen} unseen"));
    }
    let mail = mux.mail_depth_total();
    if mail > 0 {
        title.push_str(&format!(" — {mail} mail"));
    }
    title
}

/// `rail_longest_cells` is the longest saved space name; it sizes the chips
/// when `space_rail_chip_cols` is 0 (auto, PT-123).
fn host_geom(
    font: &FontMetrics,
    multi_pane: bool,
    show_tabs: bool,
    handle_row: bool,
    spacing: PaneSpacing,
    rail_longest_cells: usize,
) -> mux::HostGeom {
    let rail_chip_cap = space_rail::chip_cap(spacing.space_rail_chip_cols);
    let rail_column_cols = if spacing.space_rail.horizontal() {
        space_rail::chip_cells_for(rail_longest_cells, rail_chip_cap)
    } else {
        spacing.space_rail_width_cols
    };
    mux::HostGeom {
        cell_w: font.cell_w,
        cell_h: font.cell_h,
        window_pad: spacing.window_padding_px,
        slack_x: 0,
        slack_y: 0,
        pane_gap: if multi_pane { spacing.pane_gap_px } else { 0 },
        rail_gap: spacing.pane_gap_px,
        inner_pad: spacing.pane_padding_px,
        top_chrome_px: if show_tabs {
            font.cell_h.saturating_mul(if handle_row { 2 } else { 1 })
        } else {
            0
        },
        scrollbar_gutter_px: mux::scrollbar_gutter_for(spacing.pane_padding_px),
        rail_side: spacing.space_rail,
        rail_px: space_rail::rail_thickness_px(
            spacing.space_rail,
            font.cell_w,
            font.cell_h
                .saturating_mul(if spacing.space_rail_pane_names { 2 } else { 1 }),
            rail_column_cols,
        ),
        rail_chip_cols: rail_chip_cap,
    }
}

fn pane_screen_paint(
    host: &HostState,
    pane_id: prismattyc_mux::PaneId,
    focused: prismattyc_mux::PaneId,
    zoomed: bool,
) -> ScreenPaint {
    let active = zoomed || pane_id == focused;
    let opacity = if active {
        host.pane_opacity_active
    } else {
        host.pane_opacity_inactive
    };
    ScreenPaint {
        skip_default_bg: host.background.is_some() && opacity >= 1.0,
        bg_weight: opacity_to_weight(opacity),
        // Ghostty parity: only the *default* background is made translucent,
        // and a dimmed pane is more translucent than the focused one. Without
        // an alpha visual the frame stays fully opaque, so nothing is scaled.
        default_bg_alpha: pane_alpha(host.window_alpha, opacity, host.alpha_visual),
    }
}

fn pane_surface_alpha(
    host: &HostState,
    pane_id: prismattyc_mux::PaneId,
    focused: prismattyc_mux::PaneId,
    zoomed: bool,
) -> u8 {
    let active = zoomed || pane_id == focused;
    let opacity = if active {
        host.pane_opacity_active
    } else {
        host.pane_opacity_inactive
    };
    pane_alpha(host.window_alpha, opacity, host.alpha_visual)
}

fn pane_alpha(window_alpha: u8, pane_opacity: f32, alpha_visual: bool) -> u8 {
    if alpha_visual {
        scale_alpha(window_alpha, pane_opacity)
    } else {
        OPAQUE_ALPHA
    }
}

/// Scale a straight alpha byte by a 0.0-1.0 opacity.
fn scale_alpha(alpha: u8, opacity: f32) -> u8 {
    (f32::from(alpha) * opacity.clamp(0.0, 1.0)).round() as u8
}

fn strip_handle_row(host: &HostState) -> bool {
    host.mux.tab_infos().iter().any(|tab| tab.handles > 0)
}

fn initial_window_size(
    font: &FontMetrics,
    geom: mux::HostGeom,
    content_cols: usize,
    content_rows: usize,
) -> PhysicalSize<u32> {
    let extra_cols = (geom
        .inner_pad
        .saturating_mul(2)
        .saturating_add(geom.pane_gap)
        .saturating_add(geom.scrollbar_gutter_px)
        .saturating_add(font.cell_w.saturating_sub(1)))
        / font.cell_w;
    let extra_rows = (geom
        .inner_pad
        .saturating_mul(2)
        .saturating_add(geom.pane_gap)
        .saturating_add(font.cell_h.saturating_sub(1)))
        / font.cell_h;
    PhysicalSize::new(
        (content_cols
            .saturating_add(extra_cols)
            .saturating_mul(font.cell_w)
            .saturating_add(geom.window_pad.saturating_mul(2))
            .saturating_add(geom.chrome_left())
            .saturating_add(geom.chrome_right())) as u32,
        (content_rows
            .saturating_add(extra_rows)
            .saturating_mul(font.cell_h)
            .saturating_add(geom.window_pad.saturating_mul(2))
            .saturating_add(geom.chrome_top())
            .saturating_add(geom.chrome_bottom())) as u32,
    )
}

fn cell_at_position(
    position: PhysicalPosition<f64>,
    font: &FontMetrics,
    mux: &mux::MuxRuntime,
) -> Option<(PaneId, usize, usize)> {
    if !position.x.is_finite() || !position.y.is_finite() || position.x < 0.0 || position.y < 0.0 {
        return None;
    }
    let px = position.x as usize;
    let py = position.y as usize;
    let geom = mux.geom();
    mux.rects().find_map(|(pane, rect)| {
        let (x, y, width, height) = geom.pane_content_px(rect);
        let inside =
            px >= x && py >= y && px < x.saturating_add(width) && py < y.saturating_add(height);
        if !inside {
            return None;
        }
        let col = (px - x) / font.cell_w;
        let outer_row = (py - y) / font.cell_h;
        let dock_rows = mux.workspace_rows(pane);
        if outer_row < dock_rows {
            return None;
        }
        let row = outer_row - dock_rows;
        let (cols, rows) = geom.content_cells(rect);
        (col < cols && row < rows.saturating_sub(dock_rows)).then_some((pane, row, col))
    })
}

fn workspace_hit_at_position(
    position: PhysicalPosition<f64>,
    font: &FontMetrics,
    mux: &mux::MuxRuntime,
) -> Option<(PaneId, rich::WorkspaceHit)> {
    if !position.x.is_finite() || !position.y.is_finite() || position.x < 0.0 || position.y < 0.0 {
        return None;
    }
    let px = position.x as usize;
    let py = position.y as usize;
    let geom = mux.geom();
    mux.rects().find_map(|(pane, rect)| {
        let (x, y, width, height) = geom.pane_content_px(rect);
        if px < x || py < y || px >= x.saturating_add(width) || py >= y.saturating_add(height) {
            return None;
        }
        let row = (py - y) / font.cell_h;
        let col = (px - x) / font.cell_w;
        let dock_rows = mux.workspace_rows(pane);
        if row >= dock_rows {
            return None;
        }
        Some((
            pane,
            mux.workspace_hit(pane, u16::try_from(row).ok()?, u16::try_from(col).ok()?)?,
        ))
    })
}

fn rich_pointer_dragged(
    start_x: f64,
    start_y: f64,
    position: PhysicalPosition<f64>,
    scale_factor: f64,
) -> bool {
    let threshold = 4.0 * scale_factor.max(0.25);
    (position.x - start_x).abs() >= threshold || (position.y - start_y).abs() >= threshold
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuxCommand {
    Split(prismattyc_mux::Axis),
    Close,
    Focus(mux::FocusDirection),
    /// tmux `swap-pane` with the neighbour `delta` slots away (PT-125).
    SwapPane(i32),
    /// tmux `rotate-window` by `delta` slots (PT-125).
    RotatePanes(i32),
    /// tmux `last-pane` (PT-127).
    FocusLastPane,
    /// tmux `last-window` (PT-127).
    LastTab,
    /// Cycle focused-pane border through brand spectrum colors.
    CycleFocusBorder,
    /// Same cycle, reverse direction.
    CycleFocusBorderBack,
    /// Spawn until N panes, then even-width columns (C-S-Fn except F4).
    EvenColumns(usize),
    /// Spawn until four panes, then a 2×2 grid (C-S-F4).
    EvenQuadrants,
    /// Retile existing panes (PT-70). Does not spawn or close.
    Preset(mux::LayoutPreset),
    /// Toggle the focused pane's client-local zoom (PT-57).
    ZoomPane,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    MovePaneToTab(i32),
    /// Extract the focused pane into a new tab (PT-131).
    BreakPane,
    /// Join the focused pane into the previous tab (PT-131).
    JoinPane,
    MoveTab(i32),
    RenameTab,
    /// Edit the focused pane title (PT-148).
    RenamePane,
    /// Leave the current mux session view. Last tab exits the host.
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuxLayoutCommand {
    Split(prismattyc_mux::Axis),
    EvenColumns(usize),
    EvenQuadrants,
    ZoomPane,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuxNavigationCommand {
    Focus(mux::FocusDirection),
    SwapPane(i32),
    RotatePanes(i32),
    FocusLastPane,
    LastTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuxTabCommand {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    MovePaneToTab(i32),
    BreakPane,
    JoinPane,
    MoveTab(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuxCommandPlan {
    FocusBorder(bool),
    Detach,
    Preset(mux::LayoutPreset),
    Layout(MuxLayoutCommand),
    Navigation(MuxNavigationCommand),
    Tab(MuxTabCommand),
    RenameTab,
    RenamePane,
}

fn mux_command_plan(command: &MuxCommand) -> MuxCommandPlan {
    match command {
        MuxCommand::CycleFocusBorder => MuxCommandPlan::FocusBorder(true),
        MuxCommand::CycleFocusBorderBack => MuxCommandPlan::FocusBorder(false),
        MuxCommand::Detach => MuxCommandPlan::Detach,
        MuxCommand::Preset(preset) => MuxCommandPlan::Preset(*preset),
        MuxCommand::Split(axis) => MuxCommandPlan::Layout(MuxLayoutCommand::Split(*axis)),
        MuxCommand::EvenColumns(count) => {
            MuxCommandPlan::Layout(MuxLayoutCommand::EvenColumns(*count))
        }
        MuxCommand::EvenQuadrants => MuxCommandPlan::Layout(MuxLayoutCommand::EvenQuadrants),
        MuxCommand::ZoomPane => MuxCommandPlan::Layout(MuxLayoutCommand::ZoomPane),
        MuxCommand::Close => MuxCommandPlan::Layout(MuxLayoutCommand::Close),
        MuxCommand::Focus(direction) => {
            MuxCommandPlan::Navigation(MuxNavigationCommand::Focus(*direction))
        }
        MuxCommand::SwapPane(delta) => {
            MuxCommandPlan::Navigation(MuxNavigationCommand::SwapPane(*delta))
        }
        MuxCommand::RotatePanes(delta) => {
            MuxCommandPlan::Navigation(MuxNavigationCommand::RotatePanes(*delta))
        }
        MuxCommand::FocusLastPane => {
            MuxCommandPlan::Navigation(MuxNavigationCommand::FocusLastPane)
        }
        MuxCommand::LastTab => MuxCommandPlan::Navigation(MuxNavigationCommand::LastTab),
        MuxCommand::NewTab => MuxCommandPlan::Tab(MuxTabCommand::NewTab),
        MuxCommand::CloseTab => MuxCommandPlan::Tab(MuxTabCommand::CloseTab),
        MuxCommand::NextTab => MuxCommandPlan::Tab(MuxTabCommand::NextTab),
        MuxCommand::PrevTab => MuxCommandPlan::Tab(MuxTabCommand::PrevTab),
        MuxCommand::SelectTab(index) => MuxCommandPlan::Tab(MuxTabCommand::SelectTab(*index)),
        MuxCommand::MovePaneToTab(delta) => {
            MuxCommandPlan::Tab(MuxTabCommand::MovePaneToTab(*delta))
        }
        MuxCommand::BreakPane => MuxCommandPlan::Tab(MuxTabCommand::BreakPane),
        MuxCommand::JoinPane => MuxCommandPlan::Tab(MuxTabCommand::JoinPane),
        MuxCommand::MoveTab(delta) => MuxCommandPlan::Tab(MuxTabCommand::MoveTab(*delta)),
        MuxCommand::RenameTab => MuxCommandPlan::RenameTab,
        MuxCommand::RenamePane => MuxCommandPlan::RenamePane,
    }
}

/// Mux command for a key-table action; `None` for host-side actions.
/// Chords themselves live in `keybind` (keybindings); this is the only place
/// that knows which actions the mux runtime implements.
fn mux_command_for(action: keybind::Action) -> Option<MuxCommand> {
    use keybind::Action;
    Some(match action {
        Action::SplitRight => MuxCommand::Split(prismattyc_mux::Axis::Horizontal),
        Action::SplitDown => MuxCommand::Split(prismattyc_mux::Axis::Vertical),
        Action::ClosePane => MuxCommand::Close,
        Action::Detach => MuxCommand::Detach,
        Action::FocusLeft => MuxCommand::Focus(mux::FocusDirection::Left),
        Action::FocusRight => MuxCommand::Focus(mux::FocusDirection::Right),
        Action::FocusUp => MuxCommand::Focus(mux::FocusDirection::Up),
        Action::FocusDown => MuxCommand::Focus(mux::FocusDirection::Down),
        Action::SwapPanePrev => MuxCommand::SwapPane(-1),
        Action::SwapPaneNext => MuxCommand::SwapPane(1),
        Action::RotatePanes => MuxCommand::RotatePanes(1),
        Action::RotatePanesBack => MuxCommand::RotatePanes(-1),
        Action::FocusLastPane => MuxCommand::FocusLastPane,
        Action::LastTab => MuxCommand::LastTab,
        Action::FocusBorderNext => MuxCommand::CycleFocusBorder,
        Action::FocusBorderPrev => MuxCommand::CycleFocusBorderBack,
        Action::NewTab => MuxCommand::NewTab,
        Action::CloseTab => MuxCommand::CloseTab,
        Action::RenameTab => MuxCommand::RenameTab,
        Action::RenamePane => MuxCommand::RenamePane,
        Action::PrevTab => MuxCommand::PrevTab,
        Action::NextTab => MuxCommand::NextTab,
        Action::SelectTab(n) => MuxCommand::SelectTab(usize::from(n.saturating_sub(1))),
        Action::MovePanePrevTab => MuxCommand::MovePaneToTab(-1),
        Action::MovePaneNextTab => MuxCommand::MovePaneToTab(1),
        Action::BreakPane => MuxCommand::BreakPane,
        Action::JoinPane => MuxCommand::JoinPane,
        Action::MoveTabLeft => MuxCommand::MoveTab(-1),
        Action::MoveTabRight => MuxCommand::MoveTab(1),
        Action::Layout(4) => MuxCommand::EvenQuadrants,
        Action::Layout(n) => MuxCommand::EvenColumns(usize::from(n)),
        Action::PresetSingle => MuxCommand::Preset(mux::LayoutPreset::Single),
        Action::PresetSplitH => MuxCommand::Preset(mux::LayoutPreset::SplitH),
        Action::PresetSplitV => MuxCommand::Preset(mux::LayoutPreset::SplitV),
        Action::PresetGrid => MuxCommand::Preset(mux::LayoutPreset::Grid),
        Action::PresetMainVertical => MuxCommand::Preset(mux::LayoutPreset::MainVertical),
        Action::PresetMainHorizontal => MuxCommand::Preset(mux::LayoutPreset::MainHorizontal),
        Action::ZoomPane => MuxCommand::ZoomPane,
        Action::NewWindow
        | Action::OpenConfig
        | Action::CommandPalette
        | Action::PaletteFilterNext
        | Action::PaletteFilterPrev
        | Action::ThemePicker
        | Action::Find
        | Action::Walkthrough
        | Action::WalkthroughReset
        | Action::Copy
        | Action::Paste
        | Action::SelectAll
        | Action::ScrollLineUp
        | Action::ScrollLineDown
        | Action::OpenSpace
        | Action::DeleteSpace
        | Action::MovePaneToSpace
        | Action::SpaceRailFocus
        | Action::SpaceSettings
        | Action::UndoSpaceChange
        | Action::SpaceRailNext
        | Action::SpaceRailPrev
        | Action::SaveSpace
        | Action::RichFocus
        | Action::NewBlankTab
        | Action::NewSessionTab
        | Action::BlankSplitRight
        | Action::BlankSplitDown
        | Action::SessionSplitRight
        | Action::SessionSplitDown
        | Action::TerminalSwitcher
        | Action::AgentMessages
        | Action::UpdateRestart => return None,
    })
}

/// Key-table lookup for one event. Prefers the un-modified key (Ctrl would
/// otherwise obscure letters), then retries with the raw logical key when
/// the platform reports them differently.
fn event_action(
    keymap: &keybind::KeyMap,
    event: &winit::event::KeyEvent,
    modifiers: ModifiersState,
) -> Option<keybind::Action> {
    let shortcut = event.key_without_modifiers();
    keymap
        .action(&shortcut, event.physical_key, modifiers)
        .or_else(|| {
            if shortcut != event.logical_key {
                keymap.action(&event.logical_key, event.physical_key, modifiers)
            } else {
                None
            }
        })
}

enum MuxApplyResult {
    Changed(bool),
    Exit,
}

/// New host panes inside a Space are daemon-owned fresh sessions.
fn spawn_owned_space_pane(
    host: &mut HostState,
    axis: Option<prismattyc_mux::Axis>,
    requested: Option<&str>,
) -> Result<()> {
    let space = resolve_host_space(host).context("current Space ownership is unresolved")?;
    let name = host
        .space_rail
        .current
        .as_deref()
        .context("no current Space")?;
    let owner = space.id.clone().context("Space ownership is unresolved")?;
    let output = std::process::Command::new(pmux_bin())
        .args(["space", "add", name])
        .args(requested.into_iter().flat_map(|name| ["--name", name]))
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!(
            "create session: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)?;
    let session_name = stdout
        .trim()
        .strip_prefix("added ")
        .and_then(|line| line.strip_suffix(&format!(" to {name}")))
        .context("session created, but pmux returned no session identity; reopen this Space")?;
    let snapshot =
        attach_log::live_snapshot().context("session created; daemon snapshot unavailable")?;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| {
            session.name == session_name && session.space_id.as_deref() == Some(owner.as_str())
        })
        .context("new session owner could not be verified")?;
    host.mux.space_id = Some(owner);
    let id = session.id.to_string();
    let args = vec!["attach".into(), "--session-id".into(), id.clone()];
    let program = find_mux_bin().to_string_lossy().into_owned();
    let empty =
        host.mux.is_placeholder(host.mux.focused_id()) && host.attach_pane_sessions.is_empty();
    let old = host.mux.focused_id();
    let pane = if let Some(axis) = axis.or(empty.then_some(prismattyc_mux::Axis::Horizontal)) {
        host.mux.split_focused(&program, &args, axis, 0.5)?
    } else {
        host.mux.new_tab(&program, &args)?;
        host.mux.focused_id()
    };
    host.mux
        .mark_attach_session(pane, id.clone(), session_name.into());
    if axis.is_none() || empty {
        let title = space_view::session_title(&space, session);
        host.mux.rename_window(host.mux.active_window(), &title)?;
    }
    host.attach_pane_sessions.insert(pane, id);
    if empty {
        host.mux.focus(old);
        host.mux.close_focused()?;
        host.mux.focus(pane);
    }
    Ok(())
}

fn apply_mux_layout_command(
    host: &mut HostState,
    command: MuxLayoutCommand,
    program: &str,
    child_args: &[String],
) -> Result<MuxApplyResult> {
    if host.space_rail.current.is_some() {
        match command {
            MuxLayoutCommand::Split(axis) => {
                spawn_owned_space_pane(host, Some(axis), None)?;
                return Ok(MuxApplyResult::Changed(true));
            }
            MuxLayoutCommand::EvenColumns(count) => {
                while host.mux.active_pane_count() < count {
                    spawn_owned_space_pane(host, Some(prismattyc_mux::Axis::Horizontal), None)?;
                }
            }
            MuxLayoutCommand::EvenQuadrants => {
                while host.mux.active_pane_count() < 4 {
                    spawn_owned_space_pane(host, Some(prismattyc_mux::Axis::Horizontal), None)?;
                }
            }
            _ => {}
        }
    }
    match command {
        MuxLayoutCommand::Split(axis) => host
            .mux
            .split_focused(program, child_args, axis, 0.5)
            .map(|pane| {
                register_spawned_attach(host, pane, program, child_args);
                MuxApplyResult::Changed(true)
            }),
        MuxLayoutCommand::EvenColumns(count) => {
            let before = host.mux.active_pane_ids();
            host.mux
                .ensure_even_columns(program, child_args, count)
                .inspect(|_| register_even_layout_attaches(host, &before, program, child_args))
                .map(MuxApplyResult::Changed)
        }
        MuxLayoutCommand::EvenQuadrants => {
            let before = host.mux.active_pane_ids();
            host.mux
                .ensure_even_quadrants(program, child_args)
                .inspect(|_| register_even_layout_attaches(host, &before, program, child_args))
                .map(MuxApplyResult::Changed)
        }
        MuxLayoutCommand::ZoomPane => host.mux.toggle_zoom().map(MuxApplyResult::Changed),
        MuxLayoutCommand::Close => match host.mux.close_focused()? {
            _changed if host.mux.all_children_exited() => Ok(MuxApplyResult::Exit),
            changed => Ok(MuxApplyResult::Changed(changed)),
        },
    }
}

fn apply_mux_navigation_command(
    host: &mut HostState,
    command: MuxNavigationCommand,
) -> Result<MuxApplyResult> {
    let changed = match command {
        MuxNavigationCommand::Focus(direction) => host.mux.focus_neighbor(direction),
        MuxNavigationCommand::SwapPane(delta) => {
            return host.mux.swap_focused(delta).map(MuxApplyResult::Changed)
        }
        MuxNavigationCommand::RotatePanes(delta) => {
            return host.mux.rotate_panes(delta).map(MuxApplyResult::Changed)
        }
        MuxNavigationCommand::FocusLastPane => host.mux.focus_last_pane(),
        MuxNavigationCommand::LastTab => {
            return host.mux.select_last_tab().map(MuxApplyResult::Changed)
        }
    };
    Ok(MuxApplyResult::Changed(changed))
}

fn apply_mux_tab_command(
    host: &mut HostState,
    command: MuxTabCommand,
    program: &str,
    child_args: &[String],
) -> Result<MuxApplyResult> {
    if command == MuxTabCommand::NewTab && host.space_rail.current.is_some() {
        spawn_owned_space_pane(host, None, None)?;
        return Ok(MuxApplyResult::Changed(true));
    }
    match command {
        MuxTabCommand::NewTab => host.mux.new_tab(program, child_args).map(|_| {
            register_spawned_attach(host, host.mux.focused_id(), program, child_args);
            MuxApplyResult::Changed(true)
        }),
        MuxTabCommand::CloseTab => host.mux.close_tab().map(MuxApplyResult::Changed),
        MuxTabCommand::NextTab => host.mux.cycle_tab(1).map(MuxApplyResult::Changed),
        MuxTabCommand::PrevTab => host.mux.cycle_tab(-1).map(MuxApplyResult::Changed),
        MuxTabCommand::SelectTab(index) => host.mux.select_tab(index).map(MuxApplyResult::Changed),
        MuxTabCommand::MovePaneToTab(delta) => host
            .mux
            .move_focused_to_relative_tab(delta)
            .map(MuxApplyResult::Changed),
        MuxTabCommand::BreakPane => host
            .mux
            .move_pane_to_new_tab(host.mux.focused_id())
            .map(MuxApplyResult::Changed),
        MuxTabCommand::JoinPane => host.mux.join_focused_pane().map(MuxApplyResult::Changed),
        MuxTabCommand::MoveTab(delta) => {
            host.mux.move_active_tab(delta).map(MuxApplyResult::Changed)
        }
    }
}

fn apply_mux_command(
    host: &mut HostState,
    plan: MuxCommandPlan,
    program: &str,
    child_args: &[String],
) -> Result<MuxApplyResult> {
    match plan {
        MuxCommandPlan::Layout(command) => {
            apply_mux_layout_command(host, command, program, child_args)
        }
        MuxCommandPlan::Navigation(command) => apply_mux_navigation_command(host, command),
        MuxCommandPlan::Tab(command) => apply_mux_tab_command(host, command, program, child_args),
        MuxCommandPlan::RenameTab => {
            begin_tab_rename(host, None);
            Ok(MuxApplyResult::Changed(false))
        }
        MuxCommandPlan::RenamePane => {
            begin_pane_rename(host);
            Ok(MuxApplyResult::Changed(false))
        }
        MuxCommandPlan::FocusBorder(_) | MuxCommandPlan::Detach | MuxCommandPlan::Preset(_) => {
            unreachable!("special mux commands are handled before applying the general plan")
        }
    }
}

/// Returns true when the host window must exit (last-tab detach).
fn handle_mux_command(
    host: &mut HostState,
    command: MuxCommand,
    action: keybind::Action,
    program: &str,
    child_args: &[String],
) -> bool {
    match command {
        MuxCommand::NewTab => {
            session_prompt::create_pane(host, None);
            return false;
        }
        MuxCommand::Split(axis) => {
            session_prompt::create_pane(host, Some(axis));
            return false;
        }
        MuxCommand::EvenColumns(count) if host.mux.active_pane_count() < count => {
            session_prompt::create_layout(host, count, false);
            return false;
        }
        MuxCommand::EvenQuadrants if host.mux.active_pane_count() < 4 => {
            session_prompt::create_layout(host, 4, true);
            return false;
        }
        _ => {}
    }
    let plan = mux_command_plan(&command);
    if let MuxCommandPlan::FocusBorder(forward) = plan {
        host.focus_border = if forward {
            cycle_focus_border(host.focus_border)
        } else {
            cycle_focus_border_back(host.focus_border)
        };
        let name = focus_border_name(host.focus_border);
        host.window
            .set_title(&format!("Prismattyc — focus border: {name}"));
        host.dirty = true;
        return false;
    }
    if matches!(plan, MuxCommandPlan::Detach) {
        match host.mux.detach_view() {
            Ok(mux::DetachView::ExitHost) => {
                persist_attach_layout_from_live(host);
                return true;
            }
            Ok(mux::DetachView::ClosedTab) => {
                persist_attach_layout_from_live(host);
                App::refit_geom(host, host.window.inner_size(), Some("mux detach"));
                host.app_mouse_button = None;
                host.last_app_mouse_cell = None;
                host.window
                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
                host.dirty = true;
                return false;
            }
            Err(error) => {
                eprintln!("prismattyc-host: mux command failed: {error:#}");
                host.window
                    .set_title(&format!("Prismattyc — mux failed: {error}"));
                host.dirty = true;
                return false;
            }
        }
    }
    if let MuxCommandPlan::Preset(preset) = plan {
        match host.mux.apply_preset(preset) {
            Ok(mux::PresetOutcome::Applied) => {
                mark_layout_dirty(host);
                App::refit_geom(host, host.window.inner_size(), Some("mux change"));
                host.app_mouse_button = None;
                host.last_app_mouse_cell = None;
                host.window
                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
                host.dirty = true;
                observe_host_action(host, action, true);
            }
            Ok(mux::PresetOutcome::Unchanged(message)) => {
                host.window.set_title(&format!("Prismattyc — {message}"));
                host.dirty = true;
                observe_host_action(host, action, true);
            }
            Err(error) => {
                eprintln!("prismattyc-host: mux command failed: {error:#}");
                host.window.set_title(&format!("Prismattyc — {error}"));
                host.dirty = true;
                observe_host_action(host, action, false);
            }
        }
        return false;
    }
    let result = apply_mux_command(host, plan, program, child_args);
    match result {
        Ok(MuxApplyResult::Exit) => {
            persist_attach_layout_from_live(host);
            true
        }
        Ok(MuxApplyResult::Changed(changed)) => {
            if changed {
                mark_layout_dirty(host);
                if matches!(command, MuxCommand::BreakPane | MuxCommand::JoinPane) {
                    persist_attach_layout_from_live(host);
                }
                App::refit_geom(host, host.window.inner_size(), Some("mux change"));
                host.app_mouse_button = None;
                host.last_app_mouse_cell = None;
                host.window
                    .set_title(&window_title(&host.mux, show_tab_strip(host)));
                host.dirty = true;
            }
            if !matches!(command, MuxCommand::RenameTab | MuxCommand::RenamePane) {
                observe_host_action(host, action, true);
            }
            false
        }
        Err(error) => {
            // Desktop launches often have nowhere for stderr — surface in title.
            eprintln!("prismattyc-host: mux command failed: {error:#}");
            host.window
                .set_title(&format!("Prismattyc — mux failed: {error}"));
            host.dirty = true;
            observe_host_action(host, action, false);
            false
        }
    }
}

fn selection_claims_ctrl_c(selection: &Selection) -> bool {
    let Some(range) = selection.range() else {
        return false;
    };
    range.start_row != range.end_row || range.start_col != range.end_col
}

/// Host-owned rich-focus input. No-op when `--experimental-rich` is off so
/// the `rich_focus` chord still reaches the child. When a region is granted,
/// regular keys become bounded APC frames instead of VT bytes.
fn handle_rich_focus_input(
    host: &mut HostState,
    event: &winit::event::KeyEvent,
    action: Option<keybind::Action>,
) -> bool {
    if !host.mux.focused().experimental_rich() {
        return false;
    }
    if action == Some(keybind::Action::RichFocus) {
        if host.mux.toggle_rich_focus() {
            host.dirty = true;
        }
        return true;
    }
    if !host.mux.rich_focus_active() {
        return false;
    }
    if matches!(event.logical_key, Key::Named(NamedKey::Escape)) {
        let pane = host.mux.focused_id();
        host.mux.revoke_rich_focus(pane);
        host.dirty = true;
        return true;
    }
    if !rich_focus_captures(host.modifiers) {
        return false;
    }
    if event.repeat {
        return true;
    }
    if let Some(token) = rich_focus_key_token(&event.logical_key, host.modifiers) {
        let _ = host
            .mux
            .send_rich_focus_key(&token, structured_modifiers(host.modifiers));
    }
    true
}

/// Ctrl+Shift chords stay host-owned while rich focus is granted (scroll
/// pan, copy/paste, tab nav); only unmodified keys belong to the region.
fn rich_focus_captures(modifiers: ModifiersState) -> bool {
    !(modifiers.control_key() && modifiers.shift_key())
}

fn structured_modifiers(modifiers: ModifiersState) -> InputModifiers {
    let mut bits = 0;
    if modifiers.shift_key() {
        bits |= InputModifiers::SHIFT;
    }
    if modifiers.control_key() {
        bits |= InputModifiers::CONTROL;
    }
    if modifiers.alt_key() {
        bits |= InputModifiers::ALT;
    }
    if modifiers.super_key() {
        bits |= InputModifiers::SUPER;
    }
    InputModifiers::new(bits).expect("winit modifiers use only frozen input bits")
}

fn rich_focus_key_token(logical: &Key, modifiers: ModifiersState) -> Option<String> {
    let name = match logical {
        Key::Named(NamedKey::Enter) => "Enter",
        Key::Named(NamedKey::Tab) => "Tab",
        Key::Named(NamedKey::Backspace) => "Backspace",
        Key::Named(NamedKey::Delete) => "Delete",
        Key::Named(NamedKey::ArrowUp) => "Up",
        Key::Named(NamedKey::ArrowDown) => "Down",
        Key::Named(NamedKey::ArrowLeft) => "Left",
        Key::Named(NamedKey::ArrowRight) => "Right",
        Key::Named(NamedKey::Home) => "Home",
        Key::Named(NamedKey::End) => "End",
        Key::Named(NamedKey::PageUp) => "PageUp",
        Key::Named(NamedKey::PageDown) => "PageDown",
        Key::Named(NamedKey::Space) => "Space",
        Key::Named(
            NamedKey::Control
            | NamedKey::Shift
            | NamedKey::Alt
            | NamedKey::AltGraph
            | NamedKey::Super
            | NamedKey::Meta
            | NamedKey::Hyper
            | NamedKey::Escape,
        ) => return None,
        Key::Character(text) => {
            let mut chs = text.chars();
            let ch = chs.next()?;
            if chs.next().is_some() {
                return None;
            }
            let mut token = String::new();
            if modifiers.control_key() {
                token.push_str("C-");
            }
            if modifiers.shift_key() {
                token.push_str("S-");
            }
            if modifiers.alt_key() {
                token.push_str("A-");
            }
            token.push(ch);
            return Some(token);
        }
        _ => return None,
    };
    let mut token = String::new();
    if modifiers.control_key() {
        token.push_str("C-");
    }
    if modifiers.shift_key() {
        token.push_str("S-");
    }
    if modifiers.alt_key() {
        token.push_str("A-");
    }
    token.push_str(name);
    Some(token)
}

fn is_logical_char(logical: &Key, expected: char) -> bool {
    matches!(logical, Key::Character(text) if text.chars().count() == 1
        && text.chars().next().is_some_and(|c| c.eq_ignore_ascii_case(&expected)))
}

fn apply_modifier_key_event(modifiers: &mut ModifiersState, logical: &Key, state: ElementState) {
    let pressed = state == ElementState::Pressed;
    let Key::Named(named) = logical else {
        return;
    };
    match named {
        NamedKey::Control => {
            modifiers.set(ModifiersState::CONTROL, pressed);
        }
        NamedKey::Shift => {
            modifiers.set(ModifiersState::SHIFT, pressed);
        }
        NamedKey::Alt | NamedKey::AltGraph => {
            modifiers.set(ModifiersState::ALT, pressed);
        }
        NamedKey::Super | NamedKey::Meta | NamedKey::Hyper => {
            modifiers.set(ModifiersState::SUPER, pressed);
        }
        _ => {}
    }
}

fn apply_modifier_physical(
    modifiers: &mut ModifiersState,
    physical: PhysicalKey,
    state: ElementState,
) {
    let PhysicalKey::Code(code) = physical else {
        return;
    };
    let pressed = state == ElementState::Pressed;
    match code {
        KeyCode::ControlLeft | KeyCode::ControlRight => {
            modifiers.set(ModifiersState::CONTROL, pressed);
        }
        KeyCode::ShiftLeft | KeyCode::ShiftRight => {
            modifiers.set(ModifiersState::SHIFT, pressed);
        }
        KeyCode::AltLeft | KeyCode::AltRight => {
            modifiers.set(ModifiersState::ALT, pressed);
        }
        KeyCode::SuperLeft | KeyCode::SuperRight => {
            modifiers.set(ModifiersState::SUPER, pressed);
        }
        _ => {}
    }
}

fn is_copy_chord(logical: &Key, modifiers: ModifiersState, has_selection: bool) -> bool {
    if !modifiers.control_key() || modifiers.alt_key() || !is_logical_char(logical, 'c') {
        return false;
    }
    modifiers.shift_key() || has_selection
}

/// Paste fallbacks that stay fixed (keybindings D-K3): **Shift+Insert**
/// (classic X11) and the dedicated Paste key. The primary chord is the
/// `paste` action in the key table. Plain Ctrl+V is **not** claimed — many
/// apps (vim, readline) own it.
fn is_paste_fallback(logical: &Key, physical: PhysicalKey, modifiers: ModifiersState) -> bool {
    if modifiers.alt_key() || !modifiers.shift_key() {
        return false;
    }
    // The dedicated Paste key pastes with Shift or Ctrl+Shift, as before.
    if matches!(logical, Key::Named(NamedKey::Paste)) {
        return true;
    }
    !modifiers.control_key()
        && (matches!(logical, Key::Named(NamedKey::Insert))
            || matches!(physical, PhysicalKey::Code(KeyCode::Insert)))
}

fn is_mark_key(logical: &Key, modifiers: ModifiersState) -> bool {
    if !modifiers.control_key() || modifiers.alt_key() || modifiers.shift_key() {
        return false;
    }
    matches!(logical, Key::Named(NamedKey::Space))
        || is_logical_char(logical, ' ')
        || is_logical_char(logical, '2')
}

fn selection_motion(logical: &Key) -> Option<NamedKey> {
    let Key::Named(named) = logical else {
        return None;
    };
    matches!(
        named,
        NamedKey::ArrowLeft
            | NamedKey::ArrowRight
            | NamedKey::ArrowUp
            | NamedKey::ArrowDown
            | NamedKey::Home
            | NamedKey::End
            | NamedKey::PageUp
            | NamedKey::PageDown
    )
    .then_some(*named)
}

fn extend_selection_keyboard(
    selection: &mut Selection,
    screen: &Screen,
    scroll: usize,
    motion: NamedKey,
) -> bool {
    let cols = screen.columns();
    let rows = screen.rows();
    if cols == 0 || rows == 0 {
        return false;
    }
    let scroll = scroll.min(screen.max_view_scroll());
    let first_abs = screen.abs_row_at_view(scroll, 0);
    let last_abs = screen.abs_row_at_view(scroll, rows - 1);
    if selection.range().is_none() {
        let caret = screen.cursor();
        // When scrolled, mark at top-left of the visible window.
        let mark_row = if scroll == 0 {
            screen.abs_row_at_view(0, caret.row)
        } else {
            first_abs
        };
        let mark_col = if scroll == 0 { caret.column } else { 0 };
        selection.begin(mark_row, mark_col);
    } else {
        selection.active = true;
    }
    let free = selection.cursor.unwrap_or_else(|| {
        let caret = screen.cursor();
        prismattyc_core::Cursor {
            row: if scroll == 0 {
                screen.abs_row_at_view(0, caret.row)
            } else {
                first_abs
            },
            column: if scroll == 0 { caret.column } else { 0 },
        }
    });
    let (mut row, mut col) = (free.row, free.column);
    let page = rows.saturating_sub(1).max(1);
    match motion {
        NamedKey::ArrowLeft => col = col.saturating_sub(1),
        NamedKey::ArrowRight => col = (col + 1).min(cols - 1),
        NamedKey::ArrowUp => row = row.saturating_sub(1).max(first_abs),
        NamedKey::ArrowDown => row = (row + 1).min(last_abs),
        NamedKey::Home => col = 0,
        NamedKey::End => col = cols - 1,
        NamedKey::PageUp => row = row.saturating_sub(page).max(first_abs),
        NamedKey::PageDown => row = (row + page).min(last_abs),
        _ => return false,
    }
    selection.update(row, col);
    selection.dragged = true;
    true
}

fn selected_clipboard_text(selection: &Selection, screen: &Screen) -> Option<String> {
    let text = screen.extract_text_abs(selection.range()?);
    // Reuse the classic host's exact D-H4 eligibility gate. The encoded OSC
    // bytes are intentionally discarded because this host writes natively.
    encode_osc52_clipboard(&text)?;
    Some(text)
}

fn write_clipboard_text(host: &mut HostState, text: String) -> bool {
    if prismattyc_core::encode_osc52_clipboard(&text).is_none() {
        return false;
    }
    if host.clipboard.is_none() {
        match arboard::Clipboard::new() {
            Ok(clipboard) => host.clipboard = Some(clipboard),
            Err(error) => {
                eprintln!("prismattyc-host: native clipboard unavailable: {error}");
                return false;
            }
        }
    }
    let Some(clipboard) = host.clipboard.as_mut() else {
        return false;
    };
    if let Err(error) = clipboard.set_text(text.clone()) {
        eprintln!("prismattyc-host: native clipboard write failed: {error}");
        host.clipboard = None;
        return false;
    }
    note_copied_announce(host, &text);
    true
}

fn copy_selection_native(host: &mut HostState) -> bool {
    let Some(text) = selected_clipboard_text(&host.selection, host.emulator.screen()) else {
        return false;
    };
    if host.clipboard.is_none() {
        match arboard::Clipboard::new() {
            Ok(clipboard) => host.clipboard = Some(clipboard),
            Err(error) => {
                eprintln!("prismattyc-host: native clipboard unavailable: {error}");
                return false;
            }
        }
    }
    let Some(clipboard) = host.clipboard.as_mut() else {
        return false;
    };
    if let Err(error) = clipboard.set_text(text.clone()) {
        eprintln!("prismattyc-host: native clipboard write failed: {error}");
        host.clipboard = None;
        return false;
    }
    note_copied_announce(host, &text);
    true
}

fn note_copied_announce(host: &mut HostState, text: &str) {
    let mut text = text.to_string();
    if text.chars().count() > 200 {
        text = text.chars().take(200).collect();
        text.push('…');
    }
    if !text.is_empty() {
        host.pending_selection_announce = Some(text);
    }
}

/// Read system clipboard and deliver to the focused child PTY (text selection paste-in).
///
/// Text wins when the clipboard has a non-empty string. Image-only clipboards
/// become a temp PNG path so agent CLIs can read the file. A single image file
/// copied from a file manager is pasted by path without copying it.
fn paste_clipboard_native(host: &mut HostState) -> bool {
    if host.clipboard.is_none() {
        match arboard::Clipboard::new() {
            Ok(clipboard) => host.clipboard = Some(clipboard),
            Err(error) => {
                eprintln!("prismattyc-host: native clipboard unavailable: {error}");
                return false;
            }
        }
    }
    let raw_text = {
        let Some(clipboard) = host.clipboard.as_mut() else {
            return false;
        };
        clipboard.get_text().ok()
    };
    let uri_path = raw_text
        .as_deref()
        .and_then(prismattyc_mux::image_path_from_uri_list);
    let text = raw_text
        .filter(|text| !prismattyc_mux::is_empty_bracketed_or_blank(text) && uri_path.is_none());
    let (text, image_path) = match text {
        Some(text) => (text, None),
        None => {
            let Some(path) = host.clipboard.as_mut().and_then(|clipboard| {
                prismattyc_mux::clipboard_image_to_png_with(clipboard)
                    .or_else(|| prismattyc_mux::clipboard_image_file_with(clipboard))
                    .or_else(|| uri_path.clone())
            }) else {
                return false;
            };
            let agent = prismattyc_mux::detect_inject_agent(host.child_pid(), None);
            (prismattyc_mux::paste_reference(&path, agent), Some(path))
        }
    };

    if host.selection.range().is_some() || host.keyboard_select_mode {
        host.selection.clear();
        host.keyboard_select_mode = false;
        host.dirty = true;
    }
    if host.view_scroll != 0 {
        host.view_scroll = 0;
        host.dirty = true;
    }

    let child_wants_bracketed = host.emulator.bracketed_paste();
    let bytes = paste_payload(&text, child_wants_bracketed);
    if bytes.is_empty() {
        return false;
    }

    let result = enqueue_paste_chunks(&host.to_child_tx, &bytes, PASTE_SEND_BUDGET);
    if child_wants_bracketed && matches!(result, PasteEnqueueResult::Partial) {
        let close_deadline = Instant::now() + PASTE_BRACKET_CLOSE_TIMEOUT;
        let _ = try_send_chunk_until(&host.to_child_tx, b"\x1b[201~".to_vec(), close_deadline);
    }
    if matches!(
        result,
        PasteEnqueueResult::Partial | PasteEnqueueResult::Dropped
    ) {
        eprintln!("prismattyc-host: paste to child incomplete ({result:?})");
        return false;
    }
    if let Some(path) = image_path {
        show_paste_toast(host, &path);
    }
    true
}

/// Build the bytes sent to the child for a normalized paste payload.
fn paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    let payload = normalize_paste_text(text);
    if payload.is_empty() {
        return Vec::new();
    }
    if bracketed {
        let mut out = Vec::with_capacity(payload.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        payload.into_bytes()
    }
}

fn show_paste_toast(host: &mut HostState, path: &Path) {
    if !host.bell_toaster {
        return;
    }
    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    let label = format!(" pasted image → {basename} ");
    let pane = host.mux.focused_id();
    let until = Instant::now() + host.bell_toaster_ms;
    match host.bell_toasts.iter_mut().find(|toast| toast.pane == pane) {
        Some(toast) => {
            toast.label = label;
            toast.until = until;
        }
        None => host.bell_toasts.push(BellToast { pane, until, label }),
    }
    host.dirty = true;
}

/// Strip nested bracketed-paste wrappers (text selection / classic-host parity).
///
/// O(n) scan: skip complete START/END matches; pop suffix-synthesized delimiters.
fn normalize_paste_text(text: &str) -> String {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";

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
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteEnqueueResult {
    Queued,
    Partial,
    Dropped,
}

fn enqueue_paste_chunks<T: From<Vec<u8>>>(
    to_child: &mpsc::SyncSender<T>,
    bytes: &[u8],
    budget: Duration,
) -> PasteEnqueueResult {
    if bytes.is_empty() {
        return PasteEnqueueResult::Queued;
    }
    let deadline = Instant::now() + budget;
    let mut offset = 0;
    let mut any = false;
    while offset < bytes.len() {
        let end = (offset + PASTE_CHUNK_BYTES).min(bytes.len());
        let chunk = bytes[offset..end].to_vec();
        match try_send_chunk_until(to_child, chunk, deadline) {
            Ok(()) => {
                any = true;
                offset = end;
            }
            Err(()) => {
                return if any {
                    PasteEnqueueResult::Partial
                } else {
                    PasteEnqueueResult::Dropped
                };
            }
        }
    }
    PasteEnqueueResult::Queued
}

fn try_send_chunk_until<T: From<Vec<u8>>>(
    to_child: &mpsc::SyncSender<T>,
    chunk: Vec<u8>,
    deadline: Instant,
) -> Result<(), ()> {
    let mut held = Some(chunk);
    loop {
        let Some(bytes) = held.take() else {
            return Ok(());
        };
        match to_child.try_send(T::from(bytes.clone())) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => return Err(()),
            Err(mpsc::TrySendError::Full(_)) => {
                if Instant::now() >= deadline {
                    return Err(());
                }
                held = Some(bytes);
                thread::sleep(PASTE_POLL_INTERVAL);
            }
        }
    }
}

/// Ctrl/Cmd+left-press on a detected http(s) URL: open and consume the gesture.
///
/// Hit wins: no selection (text selection) and no app-mouse report (mouse input).
fn try_open_url_at_cursor(host: &mut HostState) -> bool {
    let open_gesture = hyperlink::is_open_url_click(host.modifiers);
    if !open_gesture {
        return false;
    }
    let Some((pane, row, col)) = host.cursor_cell else {
        return false;
    };
    if pane != host.mux.focused_id() {
        return false;
    }
    let url = {
        let screen = host.emulator.screen();
        let scroll = host.view_scroll.min(screen.max_view_scroll());
        hyperlink::url_at(screen, scroll, row, col)
    };
    let Some(url) = url else {
        return false;
    };
    if !hyperlink::click_owns_url(open_gesture, true) {
        return false;
    }
    if !hyperlink::spawn_open(&url) {
        hyperlink::ring_host_bell();
    }
    host.left_button_down = false;
    host.suppress_left_release = true;
    true
}

fn begin_pointer_selection(host: &mut HostState, pane: PaneId, row: usize, col: usize) {
    host.keyboard_select_mode = false;
    host.left_button_down = true;
    let clicks = host.multi_click.on_left_down(pane, row, col);
    let (abs, range) = {
        let screen = host.emulator.screen();
        let scroll = host.view_scroll.min(screen.max_view_scroll());
        let abs = screen.abs_row_at_view(scroll, row);
        let range = match clicks {
            2 => screen
                .word_range_at_view(scroll, row, col)
                .map(|word| (word.start_col, word.end_col)),
            3 => Some((0, screen.columns().saturating_sub(1))),
            _ => None,
        };
        (abs, range)
    };
    if let Some((start, end)) = range {
        host.selection.set_range(abs, start, abs, end);
    } else {
        host.selection.begin(abs, col);
    }
    host.dirty = true;
}

fn pan_view_scroll(host: &mut HostState, delta_rows: isize) {
    if host.emulator.screen().alt_active() {
        host.view_scroll = 0;
        return;
    }
    let max = host.emulator.screen().max_view_scroll();
    let before = host.view_scroll;
    if delta_rows > 0 {
        host.view_scroll = (host.view_scroll + delta_rows as usize).min(max);
    } else {
        host.view_scroll = host.view_scroll.saturating_sub((-delta_rows) as usize);
    }
    if host.view_scroll != before {
        // Pan clears finished selection chrome (nested parity); mid-drag keeps anchor.
        if !host.left_button_down {
            host.selection.clear();
            host.keyboard_select_mode = false;
        }
        if host.view_scroll == 0 {
            host.scroll_new_output = false;
        }
        host.window
            .set_title(&window_title(&host.mux, show_tab_strip(host)));
        host.dirty = true;
    }
}

fn set_view_scroll(host: &mut HostState, scroll: usize) {
    if host.emulator.screen().alt_active() {
        host.view_scroll = 0;
        return;
    }
    let max = host.emulator.screen().max_view_scroll();
    let next = scroll.min(max);
    if host.view_scroll == next {
        return;
    }
    host.view_scroll = next;
    host.selection.clear();
    host.keyboard_select_mode = false;
    if next == 0 {
        host.scroll_new_output = false;
    }
    host.window
        .set_title(&window_title(&host.mux, show_tab_strip(host)));
    host.dirty = true;
}

fn pane_scrollbar_at(
    host: &HostState,
    px: usize,
    py: usize,
) -> Option<(PaneId, ScrollbarLayout, usize)> {
    if host.theme_picker.is_some() || host.palette.is_some() || host.splash.is_some() {
        return None;
    }
    let geom = host.mux.geom();
    host.mux.rects().find_map(|(pane_id, rect)| {
        let pane = host.mux.pane(pane_id)?;
        let (_, content_y, _, content_h) = geom.pane_content_px(rect);
        let (bar_x, _, bar_w, _) = geom.scrollbar_px(rect);
        let dock_px = host
            .mux
            .workspace_rows(pane_id)
            .saturating_mul(host.font.cell_h);
        let guest_y = content_y.saturating_add(dock_px);
        let guest_h = content_h.saturating_sub(dock_px);
        let max = pane.emulator.screen().max_view_scroll();
        let scroll = pane.view_scroll.min(max);
        let bar = scrollbar_layout(
            bar_x,
            guest_y,
            bar_w,
            guest_h,
            scroll,
            max,
            pane.emulator.screen().rows(),
        )?;
        bar.contains(px, py).then_some((pane_id, bar, max))
    })
}

fn walkthrough_overlay_open(host: &HostState) -> bool {
    host.palette.is_some()
        || host.theme_picker.is_some()
        || host.find.active
        || host.space_picker.is_some()
}

fn walkthrough_caption_view(host: &HostState) -> Option<walkthrough::CaptionView> {
    let live = host.walkthrough.as_ref()?;
    let step = live.current_step()?;
    let chord = step
        .command
        .as_deref()
        .and_then(keybind::Action::from_name)
        .map(|action| host.keymap.label(action))
        .unwrap_or_default();
    let view = live.view(&chord)?;
    walkthrough::caption_paint_decision(true, walkthrough_overlay_open(host)).then_some(view)
}

fn focused_pane_content_px(host: &HostState) -> Option<(usize, usize, usize, usize)> {
    let focused = host.mux.focused_id();
    host.mux.panes_and_rects().find_map(|(pane, _, rect)| {
        (pane == focused).then(|| host.mux.geom().pane_content_px(rect))
    })
}

fn walkthrough_band(host: &HostState) -> Option<walkthrough::CaptionBand> {
    let view = walkthrough_caption_view(host)?;
    let (x, y, w, h) = focused_pane_content_px(host)?;
    let band = walkthrough::caption_band(
        (x, y, w, h),
        host.font.cell_w,
        host.font.cell_h,
        host.mux.geom().window_pad,
        &view,
    )?;
    dump_walkthrough_caption(host, &band);
    Some(band)
}

/// Box step (PT-295): write caption index and control rects when
/// `PRISMATTYC_WALKTHROUGH_DUMP` is set. Overwritten each layout.
fn dump_walkthrough_caption(host: &HostState, band: &walkthrough::CaptionBand) {
    let Some(path) = std::env::var_os("PRISMATTYC_WALKTHROUGH_DUMP") else {
        return;
    };
    let Some(live) = host.walkthrough.as_ref() else {
        return;
    };
    let step_id = live
        .current_step()
        .map(|step| step.id.as_str())
        .unwrap_or("");
    let show = band.show_me;
    let skip = band.skip;
    let json = format!(
        "{{\n  \"index\": {},\n  \"step_id\": {},\n  \"completed\": {},\n  \"skipped\": {},\n  \"show_me\": {},\n  \"skip\": {}\n}}\n",
        live.caption_index(),
        serde_json::to_string(step_id).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(live.completed_ids()).unwrap_or_else(|_| "[]".into()),
        serde_json::to_string(live.skipped_ids()).unwrap_or_else(|_| "[]".into()),
        walkthrough::caption_rect_json(show),
        walkthrough::caption_rect_json(skip),
    );
    let path = std::path::PathBuf::from(path);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn play_walkthrough_clips(host: &mut HostState, step_ids: &[&str]) {
    let present = step_ids
        .iter()
        .any(|id| walkthrough_audio::clip_lookup(id).is_some());
    let since = host
        .last_walkthrough_sound
        .map(|at| Instant::now().saturating_duration_since(at));
    match walkthrough_audio::play_decision(host.walkthrough_audio, present, since) {
        walkthrough_audio::PlayDecision::Play => {}
        walkthrough_audio::PlayDecision::SkipDisabled
        | walkthrough_audio::PlayDecision::SkipMissing
        | walkthrough_audio::PlayDecision::SkipGap => return,
    }
    host.last_walkthrough_sound = Some(Instant::now());
    let ids: Vec<String> = step_ids.iter().map(|id| (*id).to_string()).collect();
    thread::spawn(move || {
        for id in ids {
            let Some((bytes, ext)) = walkthrough_audio::clip_lookup(&id) else {
                continue;
            };
            let Some(path) = walkthrough_audio::materialize_clip(&id, bytes, ext) else {
                continue;
            };
            let _ = notify::play_file_to_end(&path);
        }
    });
}

fn play_current_walkthrough_step(host: &mut HostState) {
    let id = host
        .walkthrough
        .as_ref()
        .and_then(walkthrough::WalkthroughLive::current_step)
        .map(|step| step.audio.clone().unwrap_or_else(|| step.id.clone()));
    let Some(id) = id else {
        return;
    };
    play_walkthrough_clips(host, &[id.as_str()]);
}

fn persist_walkthrough(host: &HostState) {
    let Some(live) = host.walkthrough.as_ref() else {
        return;
    };
    let Some(progress) = live.progress(std::time::SystemTime::now()) else {
        return;
    };
    eprintln!(
        "prismattyc-host: walkthrough progress step={} completed={} skipped={}",
        progress.current_step,
        progress.completed.len(),
        progress.skipped.len()
    );
    if let Err(error) = walkthrough::save_progress(&walkthrough::progress_path(), &progress) {
        eprintln!("prismattyc-host: could not save walkthrough progress: {error}");
    }
}

fn start_walkthrough(host: &mut HostState) {
    host.splash = None;
    match walkthrough::bundled_catalog().ok().and_then(|catalog| {
        let loaded = walkthrough::load_progress(&walkthrough::progress_path());
        match loaded.as_ref() {
            Some(progress) => walkthrough::WalkthroughLive::start_with(catalog, Some(progress)),
            None => walkthrough::WalkthroughLive::start(catalog),
        }
    }) {
        Some(live) => host.walkthrough = Some(live),
        None => eprintln!("prismattyc-host: walkthrough catalog is invalid"),
    }
    let step_id = host
        .walkthrough
        .as_ref()
        .and_then(walkthrough::WalkthroughLive::current_step)
        .and_then(|step| step.audio.clone().or_else(|| Some(step.id.clone())));
    match step_id.as_deref() {
        Some(step) => play_walkthrough_clips(host, &["intro", step]),
        None => play_walkthrough_clips(host, &["intro"]),
    }
    host.dirty = true;
}

fn reset_walkthrough(host: &mut HostState) {
    if let Err(error) = walkthrough::reset_progress(&walkthrough::progress_path()) {
        eprintln!("prismattyc-host: could not reset walkthrough progress: {error}");
    }
    host.walkthrough = None;
    start_walkthrough(host);
}

fn apply_walkthrough_action(host: &mut HostState) {
    if let Some(live) = host.walkthrough.as_mut() {
        live.reveal_caption();
    } else {
        start_walkthrough(host);
    }
    host.dirty = true;
}

fn observe_walkthrough(host: &mut HostState, fact: walkthrough::Detected) {
    let Some(live) = host.walkthrough.as_mut() else {
        return;
    };
    match live.note(&fact) {
        walkthrough::DetectOutcome::Finished => {
            persist_walkthrough(host);
            host.walkthrough = None;
            host.dirty = true;
        }
        walkthrough::DetectOutcome::Advance => {
            persist_walkthrough(host);
            play_current_walkthrough_step(host);
            host.dirty = true;
        }
        walkthrough::DetectOutcome::Fail => {
            host.dirty = true;
        }
        walkthrough::DetectOutcome::Ignore => {}
    }
}

fn observe_host_action(host: &mut HostState, action: keybind::Action, ok: bool) {
    if host.walkthrough.is_none() {
        return;
    }
    observe_walkthrough(
        host,
        walkthrough::Detected::HostAction {
            action: action.name(),
            result: if ok { "ok".into() } else { "err".into() },
        },
    );
}

fn handle_caption_click(
    host: &mut HostState,
    button: MouseButton,
    program: &str,
    child_args: &[String],
) -> bool {
    if button != MouseButton::Left {
        return false;
    }
    let Some((px, py)) = walkthrough::caption_press_px(host.pointer_px) else {
        return false;
    };
    let now = Instant::now();
    let prev = host.caption_click.map(|(_, x, y)| (x, y));
    let elapsed = host
        .caption_click
        .map(|(at, _, _)| now.saturating_duration_since(at));
    let hit = if walkthrough::caption_repeat_click(prev, elapsed, px, py) {
        None
    } else {
        walkthrough_band(host)
            .as_ref()
            .and_then(|band| walkthrough::caption_hit(band, px, py))
    };
    let result = walkthrough::caption_click_result(host.pointer_px, prev, elapsed, hit);
    match result {
        walkthrough::CaptionClickResult::Miss => result.consumed(),
        walkthrough::CaptionClickResult::ConsumeRepeat => {
            host.suppress_left_release = true;
            result.consumed()
        }
        walkthrough::CaptionClickResult::Dispatch(hit) => {
            host.caption_click = Some((now, px, py));
            match hit {
                walkthrough::CaptionHit::Dismiss => {
                    if let Some(live) = host.walkthrough.as_mut() {
                        live.dismiss_caption();
                    }
                }
                walkthrough::CaptionHit::Skip => {
                    let finished = host.walkthrough.as_mut().is_some_and(|live| !live.skip());
                    persist_walkthrough(host);
                    if finished {
                        host.walkthrough = None;
                    } else {
                        play_current_walkthrough_step(host);
                    }
                }
                walkthrough::CaptionHit::ShowMe => {
                    let action = host
                        .walkthrough
                        .as_ref()
                        .and_then(walkthrough::WalkthroughLive::show_me_action)
                        .and_then(keybind::Action::from_name);
                    if let Some(action) = action {
                        let _ = dispatch_action(host, action, program, child_args);
                    } else if let Some(fact) = host
                        .walkthrough
                        .as_ref()
                        .and_then(walkthrough::WalkthroughLive::show_me_stub)
                    {
                        observe_walkthrough(host, fact);
                    }
                }
            }
            host.dirty = true;
            result.consumed()
        }
    }
}

fn handle_caption_escape(host: &mut HostState) -> bool {
    let visible = walkthrough_caption_view(host).is_some();
    match walkthrough::caption_escape_decision(visible) {
        walkthrough::CaptionEscape::PassThrough => false,
        walkthrough::CaptionEscape::Dismiss => {
            if let Some(live) = host.walkthrough.as_mut() {
                live.dismiss_caption();
                host.dirty = true;
                return true;
            }
            false
        }
    }
}

/// Left-click on a bell toast chip dismisses it and swallows the click.
fn handle_bell_toast_click(host: &mut HostState, button: MouseButton) -> bool {
    if button != MouseButton::Left || host.bell_toasts.is_empty() {
        return false;
    }
    let Some((px, py)) = host
        .pointer_px
        .filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0)
    else {
        return false;
    };
    let (px, py) = (px as usize, py as usize);
    let geom = host.mux.geom();
    let hit = host.bell_toasts.iter().position(|toast| {
        let Some((_, rect)) = host.mux.rects().find(|(id, _)| *id == toast.pane) else {
            return false;
        };
        let (content_x, content_y, content_w, content_h) = geom.pane_content_px(rect);
        let dock_px = host
            .mux
            .workspace_rows(toast.pane)
            .saturating_mul(host.font.cell_h);
        let guest_y = content_y.saturating_add(dock_px);
        let guest_h = content_h.saturating_sub(dock_px);
        bell_toast_chip_rect(
            &toast.label,
            host.font.cell_w,
            host.font.cell_h,
            content_x,
            guest_y,
            content_w,
            guest_h,
        )
        .is_some_and(|(x0, y0, w, h)| px >= x0 && px < x0 + w && py >= y0 && py < y0 + h)
    });
    let Some(idx) = hit else {
        return false;
    };
    host.bell_toasts.remove(idx);
    host.left_button_down = false;
    host.suppress_left_release = true;
    host.dirty = true;
    true
}

/// Swallow the left-release that pairs with a consumed press (URL open,
/// toast dismiss). Returns true when the event was suppressed; the flag
/// is one-shot so a later, unrelated release still reaches the app.
fn take_suppressed_left_release(
    button: MouseButton,
    state: ElementState,
    suppress: &mut bool,
) -> bool {
    if button == MouseButton::Left && state == ElementState::Released && *suppress {
        *suppress = false;
        return true;
    }
    false
}

fn handle_scrollbar_press(host: &mut HostState, button: MouseButton) -> bool {
    if button != MouseButton::Left {
        return false;
    }
    let Some((px, py)) = host
        .pointer_px
        .filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0)
    else {
        return false;
    };
    let px = px as usize;
    let py = py as usize;
    let Some((pane, bar, max)) = pane_scrollbar_at(host, px, py) else {
        return false;
    };
    if host.mux.focus(pane) {
        mark_layout_dirty(host);
    }
    let grab_off = if bar.thumb_contains(py) {
        py as i32 - bar.thumb_y as i32
    } else {
        (bar.thumb_h / 2) as i32
    };
    let thumb_y = scrollbar_thumb_y_for_pointer(bar, py, grab_off);
    let scroll = scrollbar_scroll_from_thumb_y(bar, thumb_y, max);
    set_view_scroll(host, scroll);
    host.scrollbar_drag = Some(ScrollbarDrag {
        pane,
        grab_off,
        max_scroll: max,
    });
    host.left_button_down = false;
    true
}

fn handle_scrollbar_drag(host: &mut HostState) -> bool {
    let Some(drag) = host.scrollbar_drag else {
        return false;
    };
    let Some((px, py)) = host
        .pointer_px
        .filter(|(x, y)| x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0)
    else {
        return true;
    };
    let py = py as usize;
    let Some((_, bar, max)) = pane_scrollbar_at(host, px as usize, py) else {
        // Keep dragging even if the pointer leaves the track: reuse last pane layout.
        let geom = host.mux.geom();
        let Some(rect) = host
            .mux
            .rects()
            .find(|(id, _)| *id == drag.pane)
            .map(|(_, r)| r)
        else {
            return true;
        };
        let pane = match host.mux.pane(drag.pane) {
            Some(pane) => pane,
            None => return true,
        };
        let (_, content_y, _, content_h) = geom.pane_content_px(rect);
        let (bar_x, _, bar_w, _) = geom.scrollbar_px(rect);
        let dock_px = host
            .mux
            .workspace_rows(drag.pane)
            .saturating_mul(host.font.cell_h);
        let guest_y = content_y.saturating_add(dock_px);
        let guest_h = content_h.saturating_sub(dock_px);
        let max = pane.emulator.screen().max_view_scroll();
        let Some(bar) = scrollbar_layout(
            bar_x,
            guest_y,
            bar_w,
            guest_h,
            pane.view_scroll.min(max),
            max,
            pane.emulator.screen().rows(),
        ) else {
            return true;
        };
        let thumb_y = scrollbar_thumb_y_for_pointer(bar, py, drag.grab_off);
        let scroll = scrollbar_scroll_from_thumb_y(bar, thumb_y, drag.max_scroll.min(max));
        if host.mux.focus(drag.pane) {
            mark_layout_dirty(host);
        }
        set_view_scroll(host, scroll);
        return true;
    };
    if host.mux.focus(drag.pane) {
        mark_layout_dirty(host);
    }
    let thumb_y = scrollbar_thumb_y_for_pointer(bar, py, drag.grab_off);
    let scroll = scrollbar_scroll_from_thumb_y(bar, thumb_y, max);
    set_view_scroll(host, scroll);
    true
}

/// guest TUIs on alt (vim) refuse host select unless Shift.
/// Attach chrome uses 1049 + 7700 without 1000-level tracking — that is not a
/// guest TUI, so plain drag must still select.
fn guest_alt_blocks_host_select(emulator: &Emulator) -> bool {
    emulator.screen().alt_active()
        && (!emulator.mouse_wheel_only() || emulator.mouse_tracking().is_on())
}
#[allow(clippy::too_many_arguments)] // single report encoder site
fn encode_app_mouse_report(
    emulator: &Emulator,
    col: usize,
    row: usize,
    button: u8,
    is_release: bool,
    is_motion: bool,
    alt: bool,
    ctrl: bool,
) -> Option<Vec<u8>> {
    let tracking = emulator.mouse_tracking();
    let is_wheel = button >= 64;
    if is_wheel {
        if !emulator.reports_app_wheel() {
            return None;
        }
    } else if !tracking.is_on() {
        return None;
    }
    if is_motion {
        if button == 3 {
            // Bare motion requires 1003.
            if !tracking.reports_motion() {
                return None;
            }
        } else if !tracking.reports_drag() {
            // Button motion requires 1002+.
            return None;
        }
    }
    let cols = emulator.screen().columns();
    let rows = emulator.screen().rows();
    let cx = col.min(cols.saturating_sub(1)) + 1;
    let cy = row.min(rows.saturating_sub(1)) + 1;
    let mut cb = button;
    if is_motion && button < 64 {
        cb += 32;
    }
    if alt {
        cb += 8;
    }
    if ctrl {
        cb += 16;
    }
    if emulator.mouse_sgr() {
        let final_byte = if is_release { b'm' } else { b'M' };
        Some(format!("\x1b[<{cb};{cx};{cy}{}", final_byte as char).into_bytes())
    } else {
        let enc_b = if is_release {
            3u8.saturating_add(32)
        } else {
            cb.saturating_add(32)
        };
        let enc_x = (cx.min(223) as u8).saturating_add(32);
        let enc_y = (cy.min(223) as u8).saturating_add(32);
        Some(vec![0x1b, b'[', b'M', enc_b, enc_x, enc_y])
    }
}

fn mouse_button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

fn finish_pointer_selection(host: &mut HostState) {
    host.left_button_down = false;
    if !host.selection.dragged {
        host.selection.clear();
    } else {
        host.selection.finish();
        let _ = copy_selection_native(host);
    }
    host.dirty = true;
}

/// `select_all` action: select the visible viewport and auto-copy it
/// (text selection D-H3).
fn select_all_viewport(host: &mut HostState) {
    if let Some(range) = host.emulator.screen().viewport_range() {
        let screen = host.emulator.screen();
        let scroll = host.view_scroll.min(screen.max_view_scroll());
        let start = screen.abs_row_at_view(scroll, range.start_row);
        let end = screen.abs_row_at_view(scroll, range.end_row);
        host.selection
            .set_range(start, range.start_col, end, range.end_col);
        host.keyboard_select_mode = true;
        host.dirty = true;
        let _ = copy_selection_native(host);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaceholderReopen {
    Attach { id: String },
    Recreate { name: String },
    Gone { name: String },
}

fn plan_placeholder_reopen(
    _id: &str,
    name: &str,
    live: &[(String, String)],
    space_names: &[String],
) -> PlaceholderReopen {
    if let Some((live_id, _)) = live.iter().find(|(_, live_name)| live_name == name) {
        return PlaceholderReopen::Attach {
            id: live_id.clone(),
        };
    }
    if space_names.iter().any(|session| session == name) {
        return PlaceholderReopen::Recreate {
            name: name.to_string(),
        };
    }
    PlaceholderReopen::Gone {
        name: name.to_string(),
    }
}

struct SpaceRestore {
    agent: Option<String>,
    cwd: Option<PathBuf>,
}

fn restore_from_space_session(session: &SavedSpaceSession) -> SpaceRestore {
    SpaceRestore {
        agent: space_bind_agent(session.agent.as_deref(), &session.name),
        cwd: session
            .windows
            .first()
            .and_then(|window| plan(&window.root).0),
    }
}

fn pmux_new_args(name: &str, restore: &SpaceRestore) -> Vec<String> {
    let mut args = vec!["new".into(), "--no-attach".into()];
    if let Some(agent) = &restore.agent {
        args.push("--agent".into());
        args.push(agent.clone());
    }
    args.push(name.to_string());
    args
}

fn handle_placeholder_key(host: &mut HostState, event: &winit::event::KeyEvent) -> bool {
    if event.state != winit::event::ElementState::Pressed || event.repeat {
        return false;
    }
    let pane = host.mux.focused_id();
    if !host.mux.is_placeholder(pane) {
        return false;
    }
    let enter = matches!(event.logical_key, Key::Named(NamedKey::Enter))
        && !host.modifiers.control_key()
        && !host.modifiers.alt_key()
        && !host.modifiers.shift_key();
    if !enter {
        return true;
    }
    let Some(id) = host.mux.attach_session_of(pane).map(str::to_string) else {
        return true;
    };
    let name = host
        .mux
        .attach_name_of(pane)
        .unwrap_or("session")
        .to_string();
    let live = live_sessions();
    let space = space_session_names(host);
    match plan_placeholder_reopen(&id, &name, &live, &space) {
        PlaceholderReopen::Attach { id } => reopen_attach(host, pane, &id, &name),
        PlaceholderReopen::Recreate { name } => {
            if let Err(error) = recreate_session(host, &name) {
                rail_toast(host, &format!(" could not reopen {name}: {error:#} "));
                return true;
            }
            let live = live_sessions();
            if let Some((id, _)) = live.iter().find(|(_, live_name)| live_name == &name) {
                reopen_attach(host, pane, id, &name);
            } else {
                host.mux.placeholder_gone(pane, &name);
            }
        }
        PlaceholderReopen::Gone { name } => host.mux.placeholder_gone(pane, &name),
    }
    host.dirty = true;
    true
}

fn reopen_attach(host: &mut HostState, pane: PaneId, id: &str, name: &str) {
    let mux_bin = find_mux_bin();
    let args = attach_session_args(&AttachTarget {
        session: id.to_string(),
        title: name.to_string(),
    });
    if let Err(error) = host
        .mux
        .reopen_placeholder(pane, &mux_bin.to_string_lossy(), &args)
    {
        eprintln!("prismattyc-host: reopen attach failed: {error:#}");
        rail_toast(host, &format!(" could not reopen {name}: {error:#} "));
        return;
    }
    host.mux
        .mark_attach_session(pane, id.to_string(), name.to_string());
    host.attach_pane_sessions.insert(pane, id.to_string());
    mark_layout_dirty(host);
    observe_walkthrough(
        host,
        walkthrough::mux_detected("SessionCreated", None, Some("current")),
    );
}

fn parse_live_sessions(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("session ")?;
            let (name, id_part) = rest.rsplit_once(" (id ")?;
            let id = id_part.strip_suffix(')')?;
            if name.is_empty() || id.is_empty() {
                return None;
            }
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

fn live_sessions() -> Vec<(String, String)> {
    let Some(socket) = host_mux_socket() else {
        return Vec::new();
    };
    let output = std::process::Command::new(find_mux_bin())
        .arg("--socket")
        .arg(&socket)
        .arg("ls")
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    parse_live_sessions(&String::from_utf8_lossy(&output.stdout))
}

fn space_session_names(host: &HostState) -> Vec<String> {
    loaded_space(host)
        .map(|space| {
            space
                .sessions
                .into_iter()
                .map(|session| session.name)
                .collect()
        })
        .unwrap_or_default()
}

fn space_json_exists(name: &str) -> bool {
    space_json_exists_in(&spaces_dir(), name)
}

fn space_json_exists_in(dir: &Path, name: &str) -> bool {
    layout_path(dir, name)
        .ok()
        .is_some_and(|path| path.is_file())
}

/// `$PMUX_SPACE` when that space file still exists. A deleted name is
/// dropped so attach children do not inherit a zombie label (PT-205).
fn live_env_space() -> Option<String> {
    let name = std::env::var("PMUX_SPACE")
        .ok()
        .filter(|value| !value.is_empty())?;
    if space_json_exists(&name) {
        Some(name)
    } else {
        std::env::remove_var("PMUX_SPACE");
        None
    }
}

/// Cached space name if `spaces/<name>.json` still exists.
fn live_cache_space(space: Option<String>, dir: &Path) -> Option<String> {
    space.filter(|name| space_json_exists_in(dir, name))
}

fn loaded_space(host: &HostState) -> Option<prismattyc_mux::SavedSpace> {
    let name = host
        .space_rail
        .current
        .clone()
        .or_else(|| std::env::var("PMUX_SPACE").ok())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".into());
    if !space_json_exists(&name) {
        return None;
    }
    load_space(&spaces_dir(), &name).ok()
}

fn recreate_session(host: &HostState, name: &str) -> Result<()> {
    if let Some(space) = &host.space_rail.current {
        let output = std::process::Command::new(pmux_bin())
            .args(["session", "reopen", name, "--space", space])
            .stdin(std::process::Stdio::null())
            .output()?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        return Ok(());
    }
    let restore = loaded_space(host)
        .and_then(|space| {
            space
                .sessions
                .into_iter()
                .find(|session| session.name == name)
        })
        .map(|session| restore_from_space_session(&session));
    let mux_bin = find_mux_bin();
    let args = match &restore {
        Some(restore) => pmux_new_args(name, restore),
        None => pmux_new_args(
            name,
            &SpaceRestore {
                agent: space_bind_agent(None, name),
                cwd: None,
            },
        ),
    };
    let mut command = std::process::Command::new(&mux_bin);
    command.args(&args);
    if let Some(cwd) = restore.as_ref().and_then(|restore| restore.cwd.as_ref()) {
        command.current_dir(cwd);
    }
    if let Some(socket) = host_mux_socket() {
        command.env("PMUX_SOCKET", socket);
    }
    let output = command.stdin(std::process::Stdio::null()).output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn handle_selection_key(host: &mut HostState, logical: &Key) -> bool {
    // Guest alt (vim/less): refuse host selection so Ctrl+C interrupts the child.
    // Attach 1049+7700 is chrome, not a guest TUI.
    if guest_alt_blocks_host_select(&host.emulator) {
        let had = host.selection.range().is_some() || host.keyboard_select_mode;
        host.selection.clear();
        host.keyboard_select_mode = false;
        host.view_scroll = 0;
        if had {
            host.dirty = true;
        }
        return false;
    }

    // Fixed copy fallback (text selection D-H4): plain Ctrl+C with a multi-cell
    // selection. The Ctrl+Shift+C chord is the `copy` table action and is
    // dispatched before this point, so only the unshifted form is checked.
    if !host.modifiers.shift_key()
        && is_copy_chord(
            logical,
            host.modifiers,
            selection_claims_ctrl_c(&host.selection),
        )
    {
        let _ = copy_selection_native(host);
        return true;
    }

    if matches!(logical, Key::Named(NamedKey::Escape))
        && (host.selection.range().is_some() || host.keyboard_select_mode)
    {
        host.selection.clear();
        host.keyboard_select_mode = false;
        host.dirty = true;
        return true;
    }

    if is_mark_key(logical, host.modifiers) {
        let (row, column) = {
            let screen = host.emulator.screen();
            let caret = screen.cursor();
            (screen.abs_row_at_view(0, caret.row), caret.column)
        };
        host.selection.begin(row, column);
        host.selection.dragged = true;
        host.keyboard_select_mode = true;
        host.dirty = true;
        return true;
    }

    if let Some(motion) = selection_motion(logical) {
        if host.modifiers.shift_key() || host.keyboard_select_mode {
            host.keyboard_select_mode = true;
            let scroll = host.view_scroll;
            let changed = {
                let pane = host.mux.focused_mut();
                extend_selection_keyboard(
                    &mut pane.selection,
                    pane.emulator.screen(),
                    scroll,
                    motion,
                )
            };
            host.dirty |= changed;
            return true;
        }
    }

    if host.keyboard_select_mode {
        host.keyboard_select_mode = false;
        host.selection.finish();
        queue_selection_announce(host);
    }
    // Typing jumps back to live view (nested parity) unless a multi-cell
    // selection is claiming the key as a host chord (already returned above).
    if host.view_scroll != 0 {
        host.view_scroll = 0;
        host.dirty = true;
    }
    false
}

/// PT-124: effective font pixel size at a compositor scale factor, clamped
/// to the same floor used at window open and config reload.
fn scaled_font_px(base_px: f32, scale_factor: f64) -> f32 {
    (base_px * scale_factor as f32).max(10.0)
}

fn size_to_cells(
    size: PhysicalSize<u32>,
    font: &FontMetrics,
    geom: mux::HostGeom,
) -> (usize, usize) {
    let usable_width = (size.width as usize)
        .saturating_sub(geom.window_pad.saturating_mul(2))
        .saturating_sub(geom.chrome_left())
        .saturating_sub(geom.chrome_right());
    let usable_height = (size.height as usize)
        .saturating_sub(geom.window_pad.saturating_mul(2))
        .saturating_sub(geom.chrome_top())
        .saturating_sub(geom.chrome_bottom());
    let cols = (usable_width / font.cell_w).clamp(2, MAX_COLS);
    let rows = (usable_height / font.cell_h).clamp(1, MAX_ROWS);
    (cols, rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    Handled,
    Exit,
    OpenWindow,
    OpenConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorCommand {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

/// Resolve an editor without invoking a shell. Empty or malformed candidates
/// are skipped so a bad `$VISUAL` does not hide a usable `$EDITOR`.
pub(crate) fn resolve_editor_command(visual: Option<&str>, editor: Option<&str>) -> EditorCommand {
    visual
        .into_iter()
        .chain(editor)
        .find_map(parse_editor_command)
        .unwrap_or_else(|| EditorCommand {
            program: "nano".into(),
            args: Vec::new(),
        })
}

fn parse_editor_command(value: &str) -> Option<EditorCommand> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = None;
    let mut escaped = false;
    let mut started = false;
    for character in value.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if let Some(quote) = quoted {
            match character {
                c if c == quote => quoted = None,
                '\\' => escaped = true,
                c => word.push(c),
            }
            started = true;
            continue;
        }
        match character {
            '\\' => {
                escaped = true;
                started = true;
            }
            '\'' | '"' => {
                quoted = Some(character);
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if escaped || quoted.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    let mut words = words.into_iter();
    let program = words.next().filter(|program| !program.is_empty())?;
    Some(EditorCommand {
        program,
        args: words.collect(),
    })
}

/// `$XDG_DATA_HOME/prismattyc/palette-recent.json` (next to the spaces dir).
fn palette_recent_path() -> Option<PathBuf> {
    prismattyc_mux::spaces_dir()
        .parent()
        .map(|dir| dir.join(palette::RECENT_FILE))
}

/// Remember an action for the palette's RECENT section and persist it.
/// Called for every dispatched action, palette or chord (PT-92).
fn record_recent(host: &mut HostState, action: keybind::Action) {
    if !palette::push_recent(&mut host.palette_recent, action) {
        return;
    }
    if let Some(path) = host.palette_recent_path.as_ref() {
        if let Err(error) = palette::save_recent(path, &host.palette_recent) {
            eprintln!("prismattyc-host: could not save palette recents: {error}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionRoute {
    OpenWindow,
    OpenConfig,
    Scroll(isize),
    Paste,
    Copy,
    SelectAll,
    RichFocus,
    Walkthrough,
    WalkthroughReset,
    Palette,
    Find,
    Noop,
    SpaceRailFocus,
    SpaceSettings,
    UndoSpaceChange,
    SpaceRailMove(i32),
    SaveSpace,
    Mux(MuxCommand),
}

fn action_route(action: keybind::Action) -> ActionRoute {
    use keybind::Action;
    match action {
        Action::NewWindow => ActionRoute::OpenWindow,
        Action::OpenConfig => ActionRoute::OpenConfig,
        Action::ScrollLineUp => ActionRoute::Scroll(1),
        Action::ScrollLineDown => ActionRoute::Scroll(-1),
        Action::Paste => ActionRoute::Paste,
        Action::Copy => ActionRoute::Copy,
        Action::SelectAll => ActionRoute::SelectAll,
        Action::RichFocus => ActionRoute::RichFocus,
        Action::Walkthrough => ActionRoute::Walkthrough,
        Action::WalkthroughReset => ActionRoute::WalkthroughReset,
        Action::CommandPalette | Action::PaletteFilterNext | Action::PaletteFilterPrev => {
            ActionRoute::Palette
        }
        Action::Find => ActionRoute::Find,
        Action::ThemePicker | Action::OpenSpace | Action::DeleteSpace | Action::MovePaneToSpace => {
            ActionRoute::Noop
        }
        Action::SpaceRailFocus => ActionRoute::SpaceRailFocus,
        Action::SpaceSettings => ActionRoute::SpaceSettings,
        Action::UndoSpaceChange => ActionRoute::UndoSpaceChange,
        Action::SpaceRailNext => ActionRoute::SpaceRailMove(1),
        Action::SpaceRailPrev => ActionRoute::SpaceRailMove(-1),
        Action::SaveSpace => ActionRoute::SaveSpace,
        other => mux_command_for(other)
            .map(ActionRoute::Mux)
            .unwrap_or(ActionRoute::Noop),
    }
}

fn apply_space_rail_focus_action(host: &mut HostState) {
    if host.mux.geom().rail_side != space_rail::RailSide::Off {
        cancel_tab_rename(host);
        host.space_rail.focus_rail();
        host.dirty = true;
    }
}

fn apply_space_rail_move_action(host: &mut HostState, delta: i32) {
    if let Some(name) = host.space_rail.neighbour(delta) {
        open_space_from_host(host, &name, SpaceOpenMode::Switch);
    }
}

fn apply_save_space_action(host: &mut HostState) {
    if let Some(name) = host.space_rail.current.clone() {
        save_space_from_host(host, &name);
    } else {
        host.space_rail.begin_new();
        host.dirty = true;
        host.window.request_redraw();
    }
}

fn dispatch_action(
    host: &mut HostState,
    action: keybind::Action,
    program: &str,
    child_args: &[String],
) -> Dispatch {
    record_recent(host, action);
    use keybind::Action as A;
    let direct = match action {
        A::NewBlankTab => Some((None, true)),
        A::NewSessionTab => Some((None, false)),
        A::BlankSplitRight => Some((Some(prismattyc_mux::Axis::Horizontal), true)),
        A::BlankSplitDown => Some((Some(prismattyc_mux::Axis::Vertical), true)),
        A::SessionSplitRight => Some((Some(prismattyc_mux::Axis::Horizontal), false)),
        A::SessionSplitDown => Some((Some(prismattyc_mux::Axis::Vertical), false)),
        _ => None,
    };
    if let Some((axis, blank)) = direct {
        session_prompt::direct(host, axis, blank);
        return Dispatch::Handled;
    }
    if action == A::AgentMessages {
        terminal_switcher::messages(host);
        return Dispatch::Handled;
    }
    if action == A::UpdateRestart {
        space_panel::maintenance(host);
        return Dispatch::Handled;
    }
    if action == A::TerminalSwitcher {
        terminal_switcher::open(host);
        return Dispatch::Handled;
    }
    match action_route(action) {
        ActionRoute::OpenWindow => Dispatch::OpenWindow,
        ActionRoute::OpenConfig => Dispatch::OpenConfig,
        ActionRoute::Scroll(lines) => {
            pan_view_scroll(host, lines);
            Dispatch::Handled
        }
        ActionRoute::Paste => {
            let _ = paste_clipboard_native(host);
            Dispatch::Handled
        }
        ActionRoute::Copy => {
            if !guest_alt_blocks_host_select(&host.emulator) {
                let _ = copy_selection_native(host);
            }
            Dispatch::Handled
        }
        ActionRoute::SelectAll => {
            if !guest_alt_blocks_host_select(&host.emulator) {
                select_all_viewport(host);
            }
            Dispatch::Handled
        }
        ActionRoute::RichFocus => Dispatch::Handled,
        ActionRoute::Walkthrough => {
            apply_walkthrough_action(host);
            Dispatch::Handled
        }
        ActionRoute::WalkthroughReset => {
            reset_walkthrough(host);
            Dispatch::Handled
        }
        ActionRoute::Palette => {
            open_command_palette(host, action);
            Dispatch::Handled
        }
        ActionRoute::Find => {
            open_find_prompt(host);
            Dispatch::Handled
        }
        ActionRoute::Noop => Dispatch::Handled,
        ActionRoute::SpaceSettings => {
            space_panel::settings(host);
            Dispatch::Handled
        }
        ActionRoute::UndoSpaceChange => {
            spaces_polish::undo(host);
            Dispatch::Handled
        }
        ActionRoute::SpaceRailFocus => {
            apply_space_rail_focus_action(host);
            Dispatch::Handled
        }
        ActionRoute::SpaceRailMove(delta) => {
            apply_space_rail_move_action(host, delta);
            Dispatch::Handled
        }
        ActionRoute::SaveSpace => {
            apply_save_space_action(host);
            Dispatch::Handled
        }
        ActionRoute::Mux(command) => {
            if handle_mux_command(host, command, action, program, child_args) {
                Dispatch::Exit
            } else {
                Dispatch::Handled
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WheelInput {
    direction: ScrollDirection,
    host_lines: isize,
}

fn wheel_input(delta: &MouseScrollDelta, cell_h: usize) -> WheelInput {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => WheelInput {
            direction: if *y > 0.0 {
                ScrollDirection::Up
            } else if *y < 0.0 {
                ScrollDirection::Down
            } else {
                ScrollDirection::None
            },
            host_lines: if *y > 0.0 {
                3
            } else if *y < 0.0 {
                -3
            } else {
                0
            },
        },
        MouseScrollDelta::PixelDelta(position) => {
            let step = (position.y / cell_h.max(1) as f64).round() as isize;
            WheelInput {
                direction: if position.y > 0.0 {
                    ScrollDirection::Up
                } else if position.y < 0.0 {
                    ScrollDirection::Down
                } else {
                    ScrollDirection::None
                },
                host_lines: if step != 0 {
                    step
                } else if position.y > 0.0 {
                    1
                } else if position.y < 0.0 {
                    -1
                } else {
                    0
                },
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WheelContext {
    shift: bool,
    rich_focus_active: bool,
    rich_hit_focused: bool,
    app_wheel: bool,
    app_cursor_focused: bool,
    alt_active: bool,
    page_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelDecision {
    Rich { step: i16 },
    App { button: u8 },
    Host { rows: isize },
    Consume,
}

fn wheel_decision(input: WheelInput, context: WheelContext) -> WheelDecision {
    if !context.shift && context.rich_focus_active && context.rich_hit_focused {
        return WheelDecision::Rich {
            step: match input.direction {
                ScrollDirection::Up => -1,
                ScrollDirection::Down => 1,
                ScrollDirection::None => 0,
            },
        };
    }
    if !context.shift && context.app_wheel {
        if !context.app_cursor_focused {
            return WheelDecision::Consume;
        }
        return match input.direction {
            ScrollDirection::Up => WheelDecision::App { button: 64 },
            ScrollDirection::Down => WheelDecision::App { button: 65 },
            ScrollDirection::None => WheelDecision::Consume,
        };
    }
    if context.alt_active {
        return WheelDecision::Consume;
    }
    let page = context.page_rows.saturating_sub(1).max(1) as isize;
    let rows = if context.shift {
        match input.direction {
            ScrollDirection::Up => page,
            ScrollDirection::Down => -page,
            ScrollDirection::None => 0,
        }
    } else {
        input.host_lines
    };
    if rows == 0 {
        WheelDecision::Consume
    } else {
        WheelDecision::Host { rows }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RichPointerFacts {
    cancelled: bool,
    same_node: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseInputContext {
    state: ElementState,
    button: MouseButton,
    shift: bool,
    workspace_hit: Option<(PaneId, rich::WorkspaceHit)>,
    rich_hit_focused: bool,
    rich_pointer: Option<RichPointerFacts>,
    url_openable: bool,
    suppress_left_release: bool,
    tracking: prismattyc_emulator::MouseTracking,
    guest_alt_blocks: bool,
    cursor_cell: Option<(PaneId, usize, usize)>,
    left_button_down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseInputDecision {
    RichShiftDrag {
        pane: PaneId,
        hit: rich::WorkspaceHit,
    },
    RichPress {
        pane: PaneId,
        hit: rich::WorkspaceHit,
    },
    RichRelease {
        activate: bool,
        finish_selection: bool,
    },
    OpenUrl,
    SuppressLeftRelease,
    App {
        button: Option<u8>,
    },
    GuestAltBlock,
    BeginSelection {
        pane: PaneId,
        row: usize,
        col: usize,
    },
    FinishSelection,
    Ignore {
        clear_app_button: bool,
    },
}

fn mouse_input_decision(context: MouseInputContext) -> MouseInputDecision {
    if context.button == MouseButton::Left {
        if context.state == ElementState::Pressed {
            if context.shift {
                if let Some((pane, hit)) = context.workspace_hit {
                    return MouseInputDecision::RichShiftDrag { pane, hit };
                }
            } else if context.rich_hit_focused {
                let (pane, hit) = context
                    .workspace_hit
                    .expect("focused rich hit must have a workspace hit");
                return MouseInputDecision::RichPress { pane, hit };
            }
        }
        if context.state == ElementState::Released {
            if let Some(pointer) = context.rich_pointer {
                return MouseInputDecision::RichRelease {
                    activate: !pointer.cancelled && pointer.same_node,
                    finish_selection: context.left_button_down,
                };
            }
        }
        if context.state == ElementState::Pressed && context.url_openable {
            return MouseInputDecision::OpenUrl;
        }
        if context.state == ElementState::Released && context.suppress_left_release {
            return MouseInputDecision::SuppressLeftRelease;
        }
    }

    if context.tracking.is_on() && !context.shift {
        return MouseInputDecision::App {
            button: mouse_button_code(context.button),
        };
    }

    let clear_app_button = context.state == ElementState::Released;
    if context.button != MouseButton::Left {
        return MouseInputDecision::Ignore { clear_app_button };
    }
    match context.state {
        ElementState::Pressed if context.guest_alt_blocks && !context.shift => {
            MouseInputDecision::GuestAltBlock
        }
        ElementState::Pressed => context
            .cursor_cell
            .map(|(pane, row, col)| MouseInputDecision::BeginSelection { pane, row, col })
            .unwrap_or(MouseInputDecision::Ignore {
                clear_app_button: false,
            }),
        ElementState::Released if context.left_button_down => MouseInputDecision::FinishSelection,
        ElementState::Released => MouseInputDecision::Ignore { clear_app_button },
    }
}

impl ApplicationHandler<UserAction> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        if restart::resume(self, event_loop) {
            return;
        }
        if let Err(e) = self.open_window(event_loop, false) {
            eprintln!("prismattyc-host: failed to start: {e:#}");
            self.exit_code = 1;
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(host) = self.windows.get_mut(&id) {
            if let Some(adapter) = host.a11y.as_mut() {
                adapter.process_event(&host.window, &event);
            }
        }
        if matches!(event, WindowEvent::RedrawRequested) {
            // Drain on the paint path so a child-EOF wake that only
            // produced a redraw still runs the exit cascade.
            self.pump(event_loop);
            if let Some(host) = self.windows.get_mut(&id) {
                // Output and scrollback can change the link under a stationary pointer.
                sync_chrome_hover(host);
                let dirty = host.dirty;
                if let Err(e) = Self::paint(host) {
                    eprintln!("prismattyc-host: paint error: {e:#}");
                } else if dirty && host.a11y.is_some() {
                    // D-A3: focus and chrome names follow the host. A
                    // compositor expose with dirty=false does not rebuild
                    // the tree. update_if_active is a no-op without an AT.
                    publish_a11y(host);
                }
            }
            return;
        }

        if let WindowEvent::CloseRequested = event {
            if let Some(host) = self.windows.get_mut(&id) {
                local_views::persist_and_restore(host, true);
            }
            self.windows.remove(&id);
            if self.windows.is_empty() {
                self.unregister_host_pid();
                event_loop.exit();
            }
            return;
        }

        let Some(host) = self.windows.get_mut(&id) else {
            return;
        };

        if session_prompt::handle_pointer(host, &event) {
            return;
        }
        if restore_prompt::handle_pointer(host, &event) {
            return;
        }

        if (host.theme_picker.is_some()
            || host.palette.is_some()
            || host.context_menu.is_some()
            || host.splash.is_some())
            && matches!(
                &event,
                WindowEvent::CursorMoved { .. }
                    | WindowEvent::CursorLeft { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::MouseInput { .. }
            )
        {
            match &event {
                WindowEvent::CursorMoved { position, .. } => {
                    host.pointer_px = Some((position.x, position.y));
                    host.cursor_cell = None;
                    apply_palette_pointer(host);
                }
                WindowEvent::CursorLeft { .. } => {
                    host.pointer_px = None;
                    host.cursor_cell = None;
                    host.last_app_mouse_cell = None;
                }
                WindowEvent::MouseWheel { .. } => {
                    apply_palette_pointer(host);
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    if rail_resize::button(host, *state, *button) {
                        return;
                    }
                    apply_palette_pointer(host);
                    if host.context_menu.is_some()
                        && *state == ElementState::Pressed
                        && *button == MouseButton::Left
                    {
                        let row = host.pointer_px.and_then(|(x, y)| {
                            (x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0)
                                .then(|| {
                                    host.palette_layout.as_ref().and_then(|layout| {
                                        palette_hit(layout, x as usize, y as usize)
                                    })
                                })
                                .flatten()
                        });
                        if let Some(row) = row {
                            match activate_context_menu(
                                host,
                                row,
                                &self.cli.program,
                                &self.cli.child_args,
                            ) {
                                Dispatch::Exit => {
                                    event_loop.exit();
                                    return;
                                }
                                Dispatch::OpenWindow => {
                                    let _ = self.event_proxy.send_event(UserAction::NewWindow);
                                    return;
                                }
                                Dispatch::OpenConfig => {
                                    let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                                    return;
                                }
                                Dispatch::Handled => {}
                            }
                        }
                    }
                }
                _ => unreachable!("modal pointer filter only admits pointer events"),
            }
            sync_chrome_hover(host);
            host.window.request_redraw();
            return;
        }

        match event {
            WindowEvent::CloseRequested => unreachable!("handled above"),
            WindowEvent::Resized(size) => {
                Self::resize_grid(host, size);
                host.window.request_redraw();
            }
            // PT-124: Wayland compositors (Hyprland fractional scaling, KWin)
            // change scale without a logical resize. winit guarantees a
            // `Resized` after this event, so here we only swap in a font
            // rasterized at the new scale; the `Resized` arm refits the grid
            // with the new cell metrics.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                Self::refit_font_for_scale(&self.file_config, host, scale_factor);
                host.window.request_redraw();
            }
            WindowEvent::RedrawRequested => unreachable!("handled above"),
            WindowEvent::Ime(ime) => {
                if let Some(bytes) = ime_action(&ime, &mut host.preedit) {
                    let _ = host.try_send_bytes(bytes);
                }
                host.dirty = true;
                host.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                let prior_chord = host.modifiers.control_key() && host.modifiers.shift_key();
                host.modifiers = m.state();
                let now_chord = host.modifiers.control_key() && host.modifiers.shift_key();
                if now_chord {
                    host.footer_until = Some(Instant::now() + FOOTER_LINGER);
                }
                // Show/hide bottom chord strip without resizing the PTY.
                if prior_chord != now_chord {
                    host.dirty = true;
                    host.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Wayland can deliver KeyboardInput before ModifiersChanged for the
                // same physical chord. Fold modifier keys from this event into state.
                let prior_chord = host.modifiers.control_key() && host.modifiers.shift_key();
                apply_modifier_key_event(&mut host.modifiers, &event.logical_key, event.state);
                apply_modifier_physical(&mut host.modifiers, event.physical_key, event.state);
                let now_chord = host.modifiers.control_key() && host.modifiers.shift_key();
                if now_chord {
                    host.footer_until = Some(Instant::now() + FOOTER_LINGER);
                }
                if prior_chord != now_chord {
                    host.dirty = true;
                    host.window.request_redraw();
                }

                if event.state != ElementState::Pressed {
                    return;
                }
                if host.session_prompt.is_some() {
                    session_prompt::dispatch_key(host, &event.logical_key, event.repeat);
                    return;
                }
                if host.restore_prompt.is_some() {
                    restore_prompt::dispatch_key(host, &event.logical_key, event.repeat);
                    return;
                }
                if host.space_rail.edit.is_some() {
                    handle_space_name_key(host, &event);
                    return;
                }
                // The splash owns all input while it is up: nothing reaches
                // the session, the mux chords, or the new-window chord.
                if host.splash.is_some() {
                    let quit = host.splash.is_some_and(|splash_state| {
                        splash_key_plan(splash::key_action(
                            splash_state.page,
                            &event.logical_key,
                            host.modifiers,
                        )) == SplashKeyPlan::Quit
                    });
                    dispatch_splash_key(host, &event.logical_key, host.modifiers);
                    if quit {
                        self.exit_code = 0;
                        event_loop.exit();
                        return;
                    }
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                if ime_blocks_host_keyboard(&host.preedit) {
                    return;
                }
                // One key-table lookup per event (keybindings). Repeats never
                // fire one-shot actions; history scroll may repeat.
                let keymap = self.keymap.clone();
                let action_any = event_action(&keymap, &event, host.modifiers);
                let action = if event.repeat { None } else { action_any };
                match handle_palette_key(host, &event, action) {
                    PaletteVerdict::NotHandled => {}
                    PaletteVerdict::Consumed | PaletteVerdict::Close => {
                        host.window.request_redraw();
                        return;
                    }
                    PaletteVerdict::Run(selected) => {
                        let modal = handle_theme_picker_key(host, &event, Some(selected))
                            || handle_space_picker_key(host, &event, Some(selected))
                            || handle_find_key(host, &event, Some(selected));
                        if !modal {
                            if selected == keybind::Action::RichFocus
                                && handle_rich_focus_input(host, &event, Some(selected))
                            {
                                host.window.request_redraw();
                                return;
                            }
                            match dispatch_action(
                                host,
                                selected,
                                &self.cli.program,
                                &self.cli.child_args,
                            ) {
                                Dispatch::Exit => {
                                    event_loop.exit();
                                    return;
                                }
                                Dispatch::OpenWindow => {
                                    let _ = self.event_proxy.send_event(UserAction::NewWindow);
                                    return;
                                }
                                Dispatch::OpenConfig => {
                                    let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                                    return;
                                }
                                Dispatch::Handled => {}
                            }
                        }
                        host.window.request_redraw();
                        return;
                    }
                }
                if let Some(menu_dispatch) =
                    handle_context_menu_key(host, &event, &self.cli.program, &self.cli.child_args)
                {
                    match menu_dispatch {
                        Dispatch::Exit => {
                            event_loop.exit();
                            return;
                        }
                        Dispatch::OpenWindow => {
                            let _ = self.event_proxy.send_event(UserAction::NewWindow);
                            return;
                        }
                        Dispatch::OpenConfig => {
                            let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                            return;
                        }
                        Dispatch::Handled => {}
                    }
                    host.window.request_redraw();
                    return;
                }
                if handle_space_rail_key(host, &event) {
                    host.window.request_redraw();
                    return;
                }
                if handle_theme_picker_key(host, &event, action) {
                    host.window.request_redraw();
                    return;
                }
                if handle_space_picker_key(host, &event, action) {
                    host.window.request_redraw();
                    return;
                }
                if handle_rename_key(host, &event) {
                    host.window.request_redraw();
                    return;
                }
                if handle_find_key(host, &event, action) {
                    host.window.request_redraw();
                    return;
                }
                if matches!(event.logical_key, Key::Named(NamedKey::Escape))
                    && handle_caption_escape(host)
                {
                    host.window.request_redraw();
                    return;
                }
                // Host selection actions yield inside a guest alt screen
                // (vim/less) exactly like the fixed selection keys do.
                let host_select = !guest_alt_blocks_host_select(&host.emulator);
                match action_any {
                    Some(keybind::Action::ScrollLineUp) if host_select => {
                        match dispatch_action(
                            host,
                            keybind::Action::ScrollLineUp,
                            &self.cli.program,
                            &self.cli.child_args,
                        ) {
                            Dispatch::Exit => {
                                event_loop.exit();
                                return;
                            }
                            Dispatch::OpenWindow => {
                                let _ = self.event_proxy.send_event(UserAction::NewWindow);
                                return;
                            }
                            Dispatch::OpenConfig => {
                                let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                                return;
                            }
                            Dispatch::Handled => {}
                        }
                        host.window.request_redraw();
                        return;
                    }
                    Some(keybind::Action::ScrollLineDown) if host_select => {
                        match dispatch_action(
                            host,
                            keybind::Action::ScrollLineDown,
                            &self.cli.program,
                            &self.cli.child_args,
                        ) {
                            Dispatch::Exit => {
                                event_loop.exit();
                                return;
                            }
                            Dispatch::OpenWindow => {
                                let _ = self.event_proxy.send_event(UserAction::NewWindow);
                                return;
                            }
                            Dispatch::OpenConfig => {
                                let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                                return;
                            }
                            Dispatch::Handled => {}
                        }
                        host.window.request_redraw();
                        return;
                    }
                    _ => {}
                }
                if let Some(action) = action {
                    let blocked_by_guest_alt = matches!(
                        action,
                        keybind::Action::Copy
                            | keybind::Action::SelectAll
                            | keybind::Action::ScrollLineUp
                            | keybind::Action::ScrollLineDown
                    ) && !host_select;
                    if !blocked_by_guest_alt {
                        if action == keybind::Action::RichFocus
                            && handle_rich_focus_input(host, &event, Some(action))
                        {
                            host.window.request_redraw();
                            return;
                        }
                        match dispatch_action(host, action, &self.cli.program, &self.cli.child_args)
                        {
                            Dispatch::Exit => {
                                event_loop.exit();
                                return;
                            }
                            Dispatch::OpenWindow => {
                                let _ = self.event_proxy.send_event(UserAction::NewWindow);
                                return;
                            }
                            Dispatch::OpenConfig => {
                                let _ = self.event_proxy.send_event(UserAction::OpenConfig);
                                return;
                            }
                            Dispatch::Handled => {}
                        }
                        host.window.request_redraw();
                        return;
                    }
                }
                if handle_rich_focus_input(host, &event, action) {
                    host.window.request_redraw();
                    return;
                }
                if !event.repeat
                    && is_paste_fallback(&event.logical_key, event.physical_key, host.modifiers)
                {
                    let _ = paste_clipboard_native(host);
                    host.window.request_redraw();
                    return;
                }
                if handle_selection_key(host, &event.logical_key) {
                    host.window.request_redraw();
                    return;
                }
                if handle_placeholder_key(host, &event) {
                    host.window.request_redraw();
                    return;
                }
                // Named + Character + text + physical fallbacks (see keys.rs).
                // Space is NamedKey::Space; do not rely on Character(" ") alone.
                let text = event.text.as_ref().map(|s| s.as_str());
                if let Some(bytes) = keys::encode_key_event(
                    &event.logical_key,
                    event.physical_key,
                    text,
                    host.modifiers,
                ) {
                    let _ = host.try_send_bytes(bytes);
                }
                // Request redraw after key so the next PTY echo paints promptly.
                host.window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                if rail_resize::motion(host, position.x, position.y) {
                    return;
                }
                host.pointer_px = Some((position.x, position.y));
                host.cursor_cell = cell_at_position(position, &host.font, &host.mux);
                if handle_divider_drag_move(host) {
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                let hover_changed = sync_chrome_hover(host);
                if handle_strip_drag_move(host) {
                    if host.strip_drag.as_ref().is_some_and(|drag| drag.active) {
                        App::refit_geom(host, host.window.inner_size(), Some("tab drag"));
                    }
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                if handle_scrollbar_drag(host) {
                    host.window.request_redraw();
                    return;
                }
                let focused = host.mux.focused_id();
                if let Some(mut gesture) = host.rich_pointer {
                    if !gesture.cancelled
                        && rich_pointer_dragged(
                            gesture.start_x,
                            gesture.start_y,
                            position,
                            host.window.scale_factor(),
                        )
                    {
                        gesture.cancelled = true;
                        host.rich_pointer = Some(gesture);
                    }
                    if gesture.cancelled && !host.left_button_down {
                        if let Some((pane, row, col)) = host
                            .cursor_cell
                            .filter(|(pane, _, _)| *pane == gesture.pane && *pane == focused)
                        {
                            begin_pointer_selection(host, pane, row, col);
                        }
                    }
                    host.window.request_redraw();
                    return;
                }
                let app_owns_mouse =
                    host.emulator.mouse_tracking().is_on() && !host.modifiers.shift_key();
                if app_owns_mouse {
                    if let Some((pane, row, col)) =
                        host.cursor_cell.filter(|(pane, _, _)| *pane == focused)
                    {
                        let cell = (pane, row, col);
                        if host.last_app_mouse_cell != Some(cell) {
                            let button = host.app_mouse_button.unwrap_or(3);
                            if let Some(report) = encode_app_mouse_report(
                                &host.emulator,
                                col,
                                row,
                                button,
                                false,
                                true,
                                host.modifiers.alt_key(),
                                host.modifiers.control_key(),
                            ) {
                                let _ = host.try_send_bytes(report);
                            }
                            host.last_app_mouse_cell = Some(cell);
                        }
                    }
                } else if host.left_button_down && host.selection.active {
                    if let Some((_, row, col)) =
                        host.cursor_cell.filter(|(pane, _, _)| *pane == focused)
                    {
                        let scroll = host
                            .view_scroll
                            .min(host.emulator.screen().max_view_scroll());
                        let abs = host.emulator.screen().abs_row_at_view(scroll, row);
                        host.selection.update(abs, col);
                        host.dirty = true;
                        host.window.request_redraw();
                    }
                }
                if hover_changed {
                    host.window.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                host.pointer_px = None;
                host.cursor_cell = None;
                host.last_app_mouse_cell = None;
                if let Some(gesture) = host.rich_pointer.as_mut() {
                    gesture.cancelled = true;
                }
                if sync_chrome_hover(host) {
                    host.window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let shift = host.modifiers.shift_key();
                let rich_hit = if !shift && host.mux.rich_focus_active() {
                    host.pointer_px
                        .and_then(|(x, y)| {
                            workspace_hit_at_position(
                                PhysicalPosition::new(x, y),
                                &host.font,
                                &host.mux,
                            )
                        })
                        .filter(|(pane, _)| *pane == host.mux.focused_id())
                } else {
                    None
                };
                let app_cursor_focused = host
                    .cursor_cell
                    .is_some_and(|(pane, _, _)| pane == host.mux.focused_id());
                let decision = wheel_decision(
                    wheel_input(&delta, host.font.cell_h),
                    WheelContext {
                        shift,
                        rich_focus_active: host.mux.rich_focus_active(),
                        rich_hit_focused: rich_hit.is_some(),
                        app_wheel: host.emulator.reports_app_wheel(),
                        app_cursor_focused,
                        alt_active: host.emulator.screen().alt_active(),
                        page_rows: host.rows,
                    },
                );
                match decision {
                    WheelDecision::Rich { step } => {
                        if let Some((pane, hit)) = rich_hit {
                            if step != 0 && host.mux.send_rich_scroll(pane, hit, step) {
                                host.dirty = true;
                            }
                        }
                    }
                    WheelDecision::App { button } => {
                        if let Some((_, row, col)) = host
                            .cursor_cell
                            .filter(|(pane, _, _)| *pane == host.mux.focused_id())
                        {
                            if let Some(report) = encode_app_mouse_report(
                                &host.emulator,
                                col,
                                row,
                                button,
                                false,
                                false,
                                host.modifiers.alt_key(),
                                host.modifiers.control_key(),
                            ) {
                                let _ = host.try_send_bytes(report);
                            }
                        }
                    }
                    WheelDecision::Host { rows } => pan_view_scroll(host, rows),
                    WheelDecision::Consume => {}
                }
                host.window.request_redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if rail_resize::button(host, state, button) {
                    return;
                }
                if state == ElementState::Pressed
                    && button == MouseButton::Right
                    && !host.modifiers.shift_key()
                {
                    if let Some((pane, _, _)) = host.cursor_cell {
                        if host.mux.focus(pane) {
                            mark_layout_dirty(host);
                            host.window
                                .set_title(&window_title(&host.mux, show_tab_strip(host)));
                        }
                        host.left_button_down = false;
                        host.rich_pointer = None;
                        host.app_mouse_button = None;
                        open_context_menu(host, ContextMenuTarget::Pane(pane));
                        host.window.request_redraw();
                        return;
                    }
                }
                if state == ElementState::Pressed && handle_rail_click(host, button) {
                    host.left_button_down = false;
                    host.rich_pointer = None;
                    host.app_mouse_button = None;
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Pressed
                    && !(button == MouseButton::Right && host.modifiers.shift_key())
                {
                    match handle_strip_click(host, button) {
                        StripClickResult::Exit => {
                            event_loop.exit();
                            return;
                        }
                        StripClickResult::Handled => {
                            host.left_button_down = false;
                            host.rich_pointer = None;
                            host.app_mouse_button = None;
                            sync_chrome_hover(host);
                            host.window.request_redraw();
                            return;
                        }
                        StripClickResult::NotHandled => {}
                    }
                }
                if state == ElementState::Pressed
                    && handle_caption_click(host, button, &self.cli.program, &self.cli.child_args)
                {
                    host.left_button_down = false;
                    host.rich_pointer = None;
                    host.app_mouse_button = None;
                    host.suppress_left_release = true;
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Pressed && handle_bell_toast_click(host, button) {
                    host.left_button_down = false;
                    host.rich_pointer = None;
                    host.app_mouse_button = None;
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Pressed && handle_divider_press(host, button) {
                    host.left_button_down = false;
                    host.rich_pointer = None;
                    host.app_mouse_button = None;
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Released && host.divider_drag.take().is_some() {
                    update_divider_cursor(host);
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Pressed && handle_scrollbar_press(host, button) {
                    host.rich_pointer = None;
                    host.app_mouse_button = None;
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Released && finish_strip_drag(host) {
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                if state == ElementState::Released && host.scrollbar_drag.take().is_some() {
                    sync_chrome_hover(host);
                    host.window.request_redraw();
                    return;
                }
                let shift = host.modifiers.shift_key();
                let workspace_hit = host.pointer_px.and_then(|(x, y)| {
                    workspace_hit_at_position(PhysicalPosition::new(x, y), &host.font, &host.mux)
                });
                if state == ElementState::Pressed {
                    let pane = workspace_hit
                        .map(|(pane, _)| pane)
                        .or_else(|| host.cursor_cell.map(|(pane, _, _)| pane));
                    if let Some(pane) = pane {
                        if host.mux.focus(pane) {
                            mark_layout_dirty(host);
                            host.last_app_mouse_cell = None;
                            host.window
                                .set_title(&window_title(&host.mux, show_tab_strip(host)));
                        }
                    }
                }
                let focused = host.mux.focused_id();
                let rich_pointer = host.rich_pointer.map(|gesture| RichPointerFacts {
                    cancelled: gesture.cancelled,
                    same_node: workspace_hit.is_some_and(|(pane, hit)| {
                        pane == gesture.pane
                            && hit.node_id == gesture.hit.node_id
                            && hit.action_id == gesture.hit.action_id
                    }),
                });
                let url_openable = if button == MouseButton::Left
                    && state == ElementState::Pressed
                    && hyperlink::is_open_url_click(host.modifiers)
                {
                    host.cursor_cell
                        .filter(|(pane, _, _)| *pane == focused)
                        .is_some_and(|(_, row, col)| {
                            let screen = host.emulator.screen();
                            let scroll = host.view_scroll.min(screen.max_view_scroll());
                            hyperlink::url_at(screen, scroll, row, col).is_some()
                        })
                } else {
                    false
                };
                let decision = mouse_input_decision(MouseInputContext {
                    state,
                    button,
                    shift,
                    workspace_hit,
                    rich_hit_focused: host.mux.rich_focus_active()
                        && workspace_hit.is_some_and(|(pane, _)| pane == focused),
                    rich_pointer,
                    url_openable,
                    suppress_left_release: host.suppress_left_release,
                    tracking: host.emulator.mouse_tracking(),
                    guest_alt_blocks: guest_alt_blocks_host_select(&host.emulator),
                    cursor_cell: host.cursor_cell,
                    left_button_down: host.left_button_down,
                });
                match decision {
                    MouseInputDecision::RichShiftDrag { pane, hit } => {
                        let (start_x, start_y) = host.pointer_px.unwrap_or((0.0, 0.0));
                        host.rich_pointer = Some(RichPointerGesture {
                            pane,
                            hit,
                            start_x,
                            start_y,
                            cancelled: true,
                        });
                        host.window.request_redraw();
                    }
                    MouseInputDecision::RichPress { pane, hit } => {
                        let (start_x, start_y) = host.pointer_px.unwrap_or((0.0, 0.0));
                        if host.mux.send_rich_pointer(pane, hit, PointerPhase::Press) {
                            host.rich_pointer = Some(RichPointerGesture {
                                pane,
                                hit,
                                start_x,
                                start_y,
                                cancelled: false,
                            });
                            host.selection.clear();
                            host.keyboard_select_mode = false;
                            host.left_button_down = false;
                            host.dirty = true;
                        }
                        host.window.request_redraw();
                    }
                    MouseInputDecision::RichRelease {
                        activate,
                        finish_selection,
                    } => {
                        let gesture = host.rich_pointer.take().expect("checked gesture");
                        if activate {
                            if let Some((_, hit)) = workspace_hit {
                                let _ = host.mux.send_rich_pointer(
                                    gesture.pane,
                                    hit,
                                    PointerPhase::Activate,
                                );
                                host.dirty = true;
                            }
                        }
                        if finish_selection {
                            finish_pointer_selection(host);
                        }
                        host.window.request_redraw();
                    }
                    MouseInputDecision::OpenUrl => {
                        // The pure predicate owns the gesture; opener failure must not fall through to selection.
                        let _ = try_open_url_at_cursor(host);
                        host.window.request_redraw();
                    }
                    MouseInputDecision::SuppressLeftRelease => {
                        let _ = take_suppressed_left_release(
                            button,
                            state,
                            &mut host.suppress_left_release,
                        );
                        host.window.request_redraw();
                    }
                    MouseInputDecision::App { button: app_button } => {
                        if let Some(code) = app_button {
                            match state {
                                ElementState::Pressed => host.app_mouse_button = Some(code),
                                ElementState::Released => host.app_mouse_button = None,
                            }
                            if let Some((pane, row, col)) =
                                host.cursor_cell.filter(|(pane, _, _)| *pane == focused)
                            {
                                let (is_release, is_motion) = match state {
                                    ElementState::Pressed => (false, false),
                                    ElementState::Released => (true, false),
                                };
                                if let Some(report) = encode_app_mouse_report(
                                    &host.emulator,
                                    col,
                                    row,
                                    code,
                                    is_release,
                                    is_motion,
                                    host.modifiers.alt_key(),
                                    host.modifiers.control_key(),
                                ) {
                                    host.last_app_mouse_cell = Some((pane, row, col));
                                    host.selection.clear();
                                    host.keyboard_select_mode = false;
                                    host.left_button_down = false;
                                    let _ = host.try_send_bytes(report);
                                    host.dirty = true;
                                }
                            }
                        }
                        host.window.request_redraw();
                    }
                    MouseInputDecision::GuestAltBlock => {
                        host.selection.clear();
                        host.left_button_down = false;
                        host.view_scroll = 0;
                        host.dirty = true;
                        host.window.request_redraw();
                    }
                    MouseInputDecision::BeginSelection { pane, row, col } => {
                        begin_pointer_selection(host, pane, row, col);
                        host.window.request_redraw();
                    }
                    MouseInputDecision::FinishSelection => {
                        finish_pointer_selection(host);
                        host.window.request_redraw();
                    }
                    MouseInputDecision::Ignore { clear_app_button } => {
                        if clear_app_button {
                            host.app_mouse_button = None;
                        }
                        if button == MouseButton::Left {
                            host.window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                host.window_focused = focused;
                if !focused {
                    if host.palette.take().is_some() {
                        host.palette_layout = None;
                        host.window
                            .set_title(&window_title(&host.mux, show_tab_strip(host)));
                        host.dirty = true;
                    }
                    if host.context_menu.is_some() {
                        close_context_menu(host);
                    }
                    host.left_button_down = false;
                    host.suppress_left_release = false;
                    host.rich_pointer = None;
                    host.scrollbar_drag = None;
                    let pane = host.mux.focused_id();
                    host.mux.revoke_rich_focus(pane);
                    if host.selection.active {
                        host.selection.finish();
                        queue_selection_announce(host);
                    }
                } else {
                    // The pulse was frozen while unfocused; repaint so the dot
                    // resumes from the current phase instead of a stale step.
                    host.dirty = true;
                }
                host.window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                host.window_occluded = occluded;
                if occluded {
                    host.dirty |= host.pane_bells.cancel();
                }
                // Publish the occlusion state even when the status throttle
                // would otherwise defer this event.
                self.last_render_status = None;
                if !occluded {
                    host.dirty = true;
                    host.window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserAction) {
        match event {
            UserAction::Wake => self.pump(event_loop),
            UserAction::NewWindow => {
                if let Err(e) = self.open_window(event_loop, false) {
                    eprintln!("prismattyc-host: new window failed: {e:#}");
                }
            }
            UserAction::OpenConfig => {
                if let Err(e) = self.open_window(event_loop, true) {
                    eprintln!("prismattyc-host: config window failed: {e:#}");
                }
            }
            UserAction::AccessKit(event) => self.handle_accesskit(event_loop, event),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        for host in self.windows.values_mut() {
            if host.layout_dirty {
                persist_attach_layout_from_live(host);
            }
        }
        self.pump(event_loop);
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        for host in self.windows.values_mut() {
            persist_attach_layout_from_live(host);
        }
        // Explicitly drop the clipboard owner before winit tears down platform
        // state. This also lets clipboard managers persist the final copy.
        self.windows.clear();
    }
}

fn main() -> Result<()> {
    #[cfg(windows)]
    let restart_resume = restart::receive()?;
    #[cfg(windows)]
    if restart_resume.is_none() {
        prismattyc_mux::release_update::forward_installed("prismattyc-host")?;
    }
    #[cfg(unix)]
    prismattyc_mux::release_update::forward_installed("prismattyc-host")?;
    // Parse first so --help / --version / --write-config never create
    // the default config path (PT-84 review).
    let mut cli = Cli::parse(std::env::args().skip(1))?;
    let config_path = config::config_path();
    match config_template::ensure_template(&config_path) {
        Ok(true) => eprintln!("prismattyc-host: wrote config {}", config_path.display()),
        Ok(false) => {}
        Err(error) => eprintln!("prismattyc-host: could not write config: {error:#}"),
    }
    let mut startup_config_error = None;
    let file_config = config::load(&config_path).unwrap_or_else(|error| {
        eprintln!("prismattyc-host: ignoring config: {error:#}");
        startup_config_error = Some(format!("{error:#}"));
        config::ConfigFile::default()
    });
    cli.apply_config(&file_config);
    // Windowed host requires a display; fail clearly in pure SSH/CI. macOS and
    // Windows have no WAYLAND_DISPLAY/DISPLAY; winit reports its own error there.
    #[cfg(all(unix, not(target_os = "macos")))]
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        bail!(
            "prismattyc-host needs WAYLAND_DISPLAY or DISPLAY (windowed host). \
             For nested classic: cargo run -p prismattyc -- /bin/sh"
        );
    }

    let event_loop: EventLoop<UserAction> =
        EventLoop::with_user_event().build().context("event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    {
        // Menu bar must exist before the app finishes launching so ⌘N and
        // the File menu are live from the first window.
        macos_menu::install_main_menu(proxy.clone());
    }

    let mut app = App::new(cli, file_config, startup_config_error, proxy)?;
    #[cfg(windows)]
    {
        app.restart_resume = restart_resume;
    }
    event_loop.run_app(&mut app).context("run_app")?;
    if app.exit_code != 0 {
        std::process::exit(app.exit_code);
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod chrome_contract_tests;
#[cfg(test)]
mod modifier_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn softbuffer_partial_raster_requires_the_previous_frame() {
        assert!(!softbuffer_partial_raster_allowed(true, 0, false));
        assert!(softbuffer_partial_raster_allowed(true, 1, false));
        assert!(!softbuffer_partial_raster_allowed(true, 2, false));
        assert!(!softbuffer_partial_raster_allowed(true, 255, false));
        assert!(!softbuffer_partial_raster_allowed(false, 1, false));
        for age in [0, 1, 2, 255] {
            assert!(!softbuffer_partial_raster_allowed(true, age, true));
        }
    }

    static DUMP_PRESENT_ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn mux_command_plan_table_covers_dispatch_groups() {
        let cases = [
            (
                MuxCommand::CycleFocusBorder,
                MuxCommandPlan::FocusBorder(true),
            ),
            (MuxCommand::Detach, MuxCommandPlan::Detach),
            (
                MuxCommand::Preset(mux::LayoutPreset::Grid),
                MuxCommandPlan::Preset(mux::LayoutPreset::Grid),
            ),
            (
                MuxCommand::Split(prismattyc_mux::Axis::Horizontal),
                MuxCommandPlan::Layout(MuxLayoutCommand::Split(prismattyc_mux::Axis::Horizontal)),
            ),
            (
                MuxCommand::EvenColumns(3),
                MuxCommandPlan::Layout(MuxLayoutCommand::EvenColumns(3)),
            ),
            (
                MuxCommand::EvenQuadrants,
                MuxCommandPlan::Layout(MuxLayoutCommand::EvenQuadrants),
            ),
            (
                MuxCommand::Close,
                MuxCommandPlan::Layout(MuxLayoutCommand::Close),
            ),
            (
                MuxCommand::Focus(mux::FocusDirection::Left),
                MuxCommandPlan::Navigation(MuxNavigationCommand::Focus(mux::FocusDirection::Left)),
            ),
            (
                MuxCommand::SwapPane(-1),
                MuxCommandPlan::Navigation(MuxNavigationCommand::SwapPane(-1)),
            ),
            (
                MuxCommand::LastTab,
                MuxCommandPlan::Navigation(MuxNavigationCommand::LastTab),
            ),
            (
                MuxCommand::NewTab,
                MuxCommandPlan::Tab(MuxTabCommand::NewTab),
            ),
            (
                MuxCommand::SelectTab(2),
                MuxCommandPlan::Tab(MuxTabCommand::SelectTab(2)),
            ),
            (
                MuxCommand::MovePaneToTab(1),
                MuxCommandPlan::Tab(MuxTabCommand::MovePaneToTab(1)),
            ),
            (
                MuxCommand::BreakPane,
                MuxCommandPlan::Tab(MuxTabCommand::BreakPane),
            ),
            (
                MuxCommand::JoinPane,
                MuxCommandPlan::Tab(MuxTabCommand::JoinPane),
            ),
            (
                MuxCommand::CycleFocusBorderBack,
                MuxCommandPlan::FocusBorder(false),
            ),
            (
                MuxCommand::ZoomPane,
                MuxCommandPlan::Layout(MuxLayoutCommand::ZoomPane),
            ),
            (
                MuxCommand::RotatePanes(1),
                MuxCommandPlan::Navigation(MuxNavigationCommand::RotatePanes(1)),
            ),
            (
                MuxCommand::FocusLastPane,
                MuxCommandPlan::Navigation(MuxNavigationCommand::FocusLastPane),
            ),
            (
                MuxCommand::CloseTab,
                MuxCommandPlan::Tab(MuxTabCommand::CloseTab),
            ),
            (
                MuxCommand::NextTab,
                MuxCommandPlan::Tab(MuxTabCommand::NextTab),
            ),
            (
                MuxCommand::PrevTab,
                MuxCommandPlan::Tab(MuxTabCommand::PrevTab),
            ),
            (
                MuxCommand::MoveTab(-1),
                MuxCommandPlan::Tab(MuxTabCommand::MoveTab(-1)),
            ),
            (MuxCommand::RenameTab, MuxCommandPlan::RenameTab),
            (MuxCommand::RenamePane, MuxCommandPlan::RenamePane),
        ];
        for (command, expected) in cases {
            assert_eq!(mux_command_plan(&command), expected, "{command:?}");
        }
    }

    #[test]
    fn action_route_table_preserves_host_and_mux_routes() {
        use keybind::Action;
        let cases = [
            (Action::NewWindow, ActionRoute::OpenWindow),
            (Action::OpenConfig, ActionRoute::OpenConfig),
            (Action::ScrollLineUp, ActionRoute::Scroll(1)),
            (Action::ScrollLineDown, ActionRoute::Scroll(-1)),
            (Action::Paste, ActionRoute::Paste),
            (Action::Copy, ActionRoute::Copy),
            (Action::SelectAll, ActionRoute::SelectAll),
            (Action::RichFocus, ActionRoute::RichFocus),
            (Action::Walkthrough, ActionRoute::Walkthrough),
            (Action::WalkthroughReset, ActionRoute::WalkthroughReset),
            (Action::CommandPalette, ActionRoute::Palette),
            (Action::PaletteFilterNext, ActionRoute::Palette),
            (Action::PaletteFilterPrev, ActionRoute::Palette),
            (Action::Find, ActionRoute::Find),
            (Action::ThemePicker, ActionRoute::Noop),
            (Action::OpenSpace, ActionRoute::Noop),
            (Action::DeleteSpace, ActionRoute::Noop),
            (Action::MovePaneToSpace, ActionRoute::Noop),
            (Action::SpaceRailFocus, ActionRoute::SpaceRailFocus),
            (Action::SpaceRailNext, ActionRoute::SpaceRailMove(1)),
            (Action::SpaceRailPrev, ActionRoute::SpaceRailMove(-1)),
            (Action::SaveSpace, ActionRoute::SaveSpace),
            (
                Action::SplitRight,
                ActionRoute::Mux(MuxCommand::Split(prismattyc_mux::Axis::Horizontal)),
            ),
        ];
        for (action, expected) in cases {
            assert_eq!(action_route(action), expected, "{action:?}");
        }
        for action in Action::all() {
            match action_route(action) {
                ActionRoute::Mux(command) => assert_eq!(mux_command_for(action), Some(command)),
                _ => assert!(mux_command_for(action).is_none(), "{action:?}"),
            }
        }
    }

    #[test]
    fn context_menu_choice_tables_cover_space_and_pane_rows() {
        let spaces = [
            (0, SpaceContextAction::Open(SpaceOpenMode::Switch)),
            (1, SpaceContextAction::AddSession),
            (2, SpaceContextAction::Open(SpaceOpenMode::NewWindow)),
            (3, SpaceContextAction::Save),
            (4, SpaceContextAction::Rename),
            (5, SpaceContextAction::Move),
            (6, SpaceContextAction::Delete),
            (7, SpaceContextAction::Details),
        ];
        for (index, expected) in spaces {
            assert_eq!(
                context_menu_choice(ContextMenuKind::SpaceChip, index),
                ContextMenuChoice::Space(expected),
                "space row {index}"
            );
        }
        assert_eq!(
            context_menu_choice(ContextMenuKind::SpaceChip, 10),
            ContextMenuChoice::Noop
        );

        let panes = [
            (0, PaneContextAction::SplitRight),
            (1, PaneContextAction::SplitDown),
            (2, PaneContextAction::Zoom),
            (3, PaneContextAction::MovePaneNextTab),
            (4, PaneContextAction::MoveToSpace),
            (5, PaneContextAction::Close),
            (6, PaneContextAction::Rename),
            (7, PaneContextAction::Detach),
            (8, PaneContextAction::SaveSpace),
            (9, PaneContextAction::MoveSessionToSpace),
            (10, PaneContextAction::RemoveSessionFromSpace),
            (11, PaneContextAction::RemoveAndKillSession),
        ];
        for (index, expected) in panes {
            assert_eq!(
                context_menu_choice(ContextMenuKind::Pane, index),
                ContextMenuChoice::Pane(expected),
                "pane row {index}"
            );
        }
        assert_eq!(
            context_menu_choice(ContextMenuKind::Pane, 12),
            ContextMenuChoice::Noop
        );
        assert_eq!(
            context_menu_action(ContextMenuTarget::SpaceChip(4), 2),
            ContextMenuAction::Space {
                chip: 4,
                action: SpaceContextAction::Open(SpaceOpenMode::NewWindow),
            }
        );
        assert_eq!(
            context_menu_action(ContextMenuTarget::SpaceChip(4), 99),
            ContextMenuAction::Noop
        );
    }

    #[test]
    fn context_menu_confirmation_table_covers_destructive_actions() {
        let cases = [
            (ContextMenuKind::SpaceChip, 3, false, true),
            (ContextMenuKind::SpaceChip, 3, true, false),
            (ContextMenuKind::SpaceChip, 6, false, true),
            (ContextMenuKind::SpaceChip, 2, false, false),
            (ContextMenuKind::Pane, 3, false, false),
            (ContextMenuKind::Pane, 6, false, false),
            (ContextMenuKind::Pane, 10, false, false),
            (ContextMenuKind::Pane, 11, false, true),
            (ContextMenuKind::Pane, 11, true, false),
        ];
        for (kind, index, confirmed, expected) in cases {
            assert_eq!(
                context_menu_needs_confirmation(kind, index, confirmed),
                expected,
                "{kind:?}, row {index}, confirmed={confirmed}"
            );
        }
    }

    #[test]
    fn space_rail_key_decision_table_preserves_orientation_and_chords() {
        let plain = ModifiersState::empty();
        let mut ctrl = ModifiersState::empty();
        ctrl.set(ModifiersState::CONTROL, true);
        let mut alt = ModifiersState::empty();
        alt.set(ModifiersState::ALT, true);
        let mut super_key = ModifiersState::empty();
        super_key.set(ModifiersState::SUPER, true);
        let mut shift = ModifiersState::empty();
        shift.set(ModifiersState::SHIFT, true);
        let cases = [
            (
                false,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Enter),
                SpaceRailKeyDecision::Unhandled,
            ),
            (
                true,
                space_rail::RailSide::Off,
                plain,
                Key::Named(NamedKey::Enter),
                SpaceRailKeyDecision::Leave,
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                ctrl,
                Key::Named(NamedKey::Enter),
                SpaceRailKeyDecision::Leave,
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                alt,
                Key::Named(NamedKey::Enter),
                SpaceRailKeyDecision::Leave,
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                super_key,
                Key::Named(NamedKey::Enter),
                SpaceRailKeyDecision::Leave,
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Enter),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Enter),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::ArrowLeft),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Prev),
            ),
            (
                true,
                space_rail::RailSide::Left,
                plain,
                Key::Named(NamedKey::ArrowUp),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Prev),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Character("x".into()),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Edit(
                    space_rail::EditStroke::Insert('x'),
                )),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Escape),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Escape),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Delete),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Delete),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::F2),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Rename),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                shift,
                Key::Named(NamedKey::F10),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Menu),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::F10),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Edit(
                    space_rail::EditStroke::DropSelection,
                )),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::ContextMenu),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Menu),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Tab),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Next),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                shift,
                Key::Named(NamedKey::Tab),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Prev),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Backspace),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Edit(
                    space_rail::EditStroke::Backspace,
                )),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::Space),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Edit(
                    space_rail::EditStroke::Insert(' '),
                )),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Character("".into()),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Edit(
                    space_rail::EditStroke::DropSelection,
                )),
            ),
            (
                true,
                space_rail::RailSide::Bottom,
                plain,
                Key::Named(NamedKey::ArrowRight),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Next),
            ),
            (
                true,
                space_rail::RailSide::Left,
                plain,
                Key::Named(NamedKey::ArrowDown),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Next),
            ),
            (
                true,
                space_rail::RailSide::Left,
                plain,
                Key::Named(NamedKey::ArrowLeft),
                SpaceRailKeyDecision::Key(space_rail::RailKey::Edit(
                    space_rail::EditStroke::DropSelection,
                )),
            ),
        ];
        for (active, side, modifiers, key, expected) in cases {
            assert_eq!(
                space_rail_key_decision(active, side, modifiers, &key),
                expected,
                "{active:?}, {side:?}, {key:?}"
            );
        }
    }

    #[test]
    fn strip_click_decision_table_preserves_click_routes() {
        let tab = StripClickHit::Tab {
            index: 2,
            close: false,
        };
        let pane = StripClickHit::Pane { tab: 1 };
        let cases = [
            (
                MouseButton::Left,
                Some(StripClickHit::Tab {
                    index: 2,
                    close: true,
                }),
                true,
                false,
                StripClickDecision::CloseTab(2),
            ),
            (
                MouseButton::Left,
                Some(tab),
                true,
                false,
                StripClickDecision::StartTabDrag {
                    index: 2,
                    x: 10.0,
                    y: 5.0,
                },
            ),
            (
                MouseButton::Left,
                Some(pane),
                true,
                false,
                StripClickDecision::StartPaneDrag {
                    tab: 1,
                    x: 10.0,
                    y: 5.0,
                },
            ),
            (
                MouseButton::Left,
                Some(StripClickHit::Tab {
                    index: 2,
                    close: true,
                }),
                false,
                false,
                StripClickDecision::Handled,
            ),
            (
                MouseButton::Left,
                Some(StripClickHit::EmptyEnd),
                true,
                false,
                StripClickDecision::Handled,
            ),
            (
                MouseButton::Right,
                Some(StripClickHit::Tab {
                    index: 2,
                    close: false,
                }),
                true,
                false,
                StripClickDecision::RenameTab(2),
            ),
            (
                MouseButton::Right,
                Some(pane),
                false,
                false,
                StripClickDecision::RenamePane { tab: 1 },
            ),
            (
                MouseButton::Right,
                Some(tab),
                false,
                false,
                StripClickDecision::Handled,
            ),
            (
                MouseButton::Middle,
                Some(tab),
                false,
                false,
                StripClickDecision::CloseTab(2),
            ),
            (
                MouseButton::Middle,
                Some(StripClickHit::EmptyEnd),
                false,
                true,
                StripClickDecision::CancelRename,
            ),
            (
                MouseButton::Middle,
                Some(StripClickHit::EmptyEnd),
                false,
                false,
                StripClickDecision::Handled,
            ),
            (
                MouseButton::Right,
                Some(StripClickHit::EmptyEnd),
                false,
                false,
                StripClickDecision::Handled,
            ),
            (
                MouseButton::Left,
                Some(StripClickHit::EmptyEnd),
                false,
                true,
                StripClickDecision::Handled,
            ),
            (
                MouseButton::Other(4),
                None,
                false,
                true,
                StripClickDecision::CancelRename,
            ),
            (
                MouseButton::Other(4),
                None,
                false,
                false,
                StripClickDecision::NotHandled,
            ),
        ];
        for (button, hit, title_row, rename_active, expected) in cases {
            assert_eq!(
                strip_click_decision(button, hit, title_row, rename_active, 10.0, 5.0),
                expected,
                "{button:?}, {hit:?}"
            );
        }
    }

    #[test]
    fn run_present_paint_preserves_dimensions_timing_and_errors() {
        struct PaintProbe {
            calls: Vec<(u32, u32)>,
            fail: bool,
        }

        let mut probe = PaintProbe {
            calls: Vec::new(),
            fail: false,
        };
        App::run_present_paint(&mut probe, 0, 24, true, |probe, width, height| {
            probe.calls.push((width, height));
            if probe.fail {
                Err(anyhow::anyhow!("paint probe failure"))
            } else {
                Ok(())
            }
        })
        .expect("successful paint should return");
        assert_eq!(probe.calls, vec![(0, 24)]);

        probe.fail = true;
        let error = App::run_present_paint(&mut probe, 80, 24, false, |probe, width, height| {
            probe.calls.push((width, height));
            if probe.fail {
                Err(anyhow::anyhow!("paint probe failure"))
            } else {
                Ok(())
            }
        })
        .expect_err("failed paint should propagate");
        assert_eq!(error.to_string(), "paint probe failure");
        assert_eq!(probe.calls, vec![(0, 24), (80, 24)]);
    }

    #[test]
    fn partial_raster_policy_covers_backend_support_and_render_timer() {
        let cases = [
            (
                "supported without OSD",
                PartialRasterBackend::Softbuffer { wayland: false },
                config::RenderTimer::Off,
                true,
            ),
            (
                "supported with OSD",
                PartialRasterBackend::Softbuffer { wayland: false },
                config::RenderTimer::Osd,
                false,
            ),
            (
                "supported with both timer outputs",
                PartialRasterBackend::Softbuffer { wayland: false },
                config::RenderTimer::Both,
                false,
            ),
            (
                "unsupported without OSD",
                PartialRasterBackend::Softbuffer { wayland: true },
                config::RenderTimer::Off,
                false,
            ),
            (
                "unsupported with OSD",
                PartialRasterBackend::Softbuffer { wayland: true },
                config::RenderTimer::Osd,
                false,
            ),
        ];

        for (name, backend, render_timer, expected) in cases {
            assert_eq!(
                partial_raster_allowed(backend, render_timer),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn partial_raster_blockers_cover_each_fallback_reason() {
        let clear = PartialRasterBlockers::default();
        assert!(!partial_raster_blocked(clear));
        for (name, blockers) in [
            (
                "backend",
                PartialRasterBlockers {
                    partial_disallowed: true,
                    ..clear
                },
            ),
            ("osd", PartialRasterBlockers { osd: true, ..clear }),
            (
                "background",
                PartialRasterBlockers {
                    background: true,
                    ..clear
                },
            ),
            (
                "bell flash",
                PartialRasterBlockers {
                    bell_flash: true,
                    ..clear
                },
            ),
            (
                "layout transition",
                PartialRasterBlockers {
                    layout_transition: true,
                    ..clear
                },
            ),
            (
                "transient overlay",
                PartialRasterBlockers {
                    transient_overlay: true,
                    ..clear
                },
            ),
            (
                "pane content",
                PartialRasterBlockers {
                    unsupported_pane_content: true,
                    ..clear
                },
            ),
        ] {
            assert!(partial_raster_blocked(blockers), "{name}");
        }
    }

    #[test]
    fn pane_damage_repaint_table_covers_each_signal() {
        let mut dirty = GridDamage::empty(3, 4);
        dirty.mark_row(0);
        let mut scrolled = GridDamage::empty(3, 4);
        scrolled.push_scroll(ScrollDamage {
            top: 0,
            bottom: 2,
            delta: 1,
        });
        let mut overflowed = GridDamage::empty(3, 4);
        for _ in 0..=256 {
            overflowed.push_scroll(ScrollDamage {
                top: 0,
                bottom: 2,
                delta: 1,
            });
        }
        for (name, damage, expected) in [
            ("empty", GridDamage::empty(3, 4), false),
            ("dirty row", dirty, true),
            ("scroll event", scrolled, true),
            ("scroll overflow", overflowed, true),
        ] {
            assert_eq!(pane_damage_requires_repaint(&damage), expected, "{name}");
        }
    }

    #[test]
    fn content_guards_have_boundary_rows() {
        let clear = render_diagnostics::GuardMask::default();
        assert!(!unsupported_pane_content(clear));
        for guard in [
            render_diagnostics::Guard::WorkspaceLayout,
            render_diagnostics::Guard::ExperimentalRich,
            render_diagnostics::Guard::StoredImages,
        ] {
            let mut mask = clear;
            mask.set(guard, true);
            assert!(unsupported_pane_content(mask), "{guard:?}");
        }
    }

    #[test]
    fn damage_empty_requires_no_dirty_rows_or_scroll_events() {
        let empty = GridDamage::empty(3, 4);
        assert!(damage_is_empty(&empty));

        let mut dirty = GridDamage::empty(3, 4);
        dirty.mark_cell(0, 0);
        assert!(!damage_is_empty(&dirty));

        let mut scrolled = GridDamage::empty(3, 4);
        scrolled.push_scroll(ScrollDamage {
            top: 0,
            bottom: 2,
            delta: 1,
        });
        assert!(!damage_is_empty(&scrolled));
    }

    #[test]
    fn scroll_blit_eligibility_rejects_each_mismatch() {
        let eligible = ScrollBlitEligibility {
            damage_rows: 3,
            screen_rows: 3,
            damage_columns: 4,
            screen_columns: 4,
            pixel_width: 8,
            content_width: 9,
            pixel_height: 6,
            guest_height: 7,
            selection_active: false,
            scroll_events_empty: false,
        };
        assert!(scroll_blit_eligible(eligible));

        let cases = [
            (
                "rows",
                ScrollBlitEligibility {
                    damage_rows: 2,
                    ..eligible
                },
            ),
            (
                "columns",
                ScrollBlitEligibility {
                    damage_columns: 3,
                    ..eligible
                },
            ),
            (
                "width",
                ScrollBlitEligibility {
                    pixel_width: 10,
                    ..eligible
                },
            ),
            (
                "height",
                ScrollBlitEligibility {
                    pixel_height: 8,
                    ..eligible
                },
            ),
            (
                "selection",
                ScrollBlitEligibility {
                    selection_active: true,
                    ..eligible
                },
            ),
            (
                "scroll events",
                ScrollBlitEligibility {
                    scroll_events_empty: true,
                    ..eligible
                },
            ),
        ];
        for (name, input) in cases {
            assert!(!scroll_blit_eligible(input), "{name}");
        }
    }

    #[test]
    fn retained_raster_accounting_decisions_cover_boundaries() {
        assert!(!blit_was_applied(0));
        assert!(blit_was_applied(1));

        let mut damage = GridDamage::empty(1, 1);
        damage.mark_cell(0, 0);
        assert_eq!(painted_cells_for_rows(&damage, &[]), 0);
        assert_eq!(painted_cells_for_rows(&damage, &[0]), 1);

        assert!(!should_record_cursor_row(false, false, false));
        assert!(should_record_cursor_row(true, false, false));
        assert!(should_record_cursor_row(false, true, false));
        assert!(should_record_cursor_row(false, false, true));
    }

    #[test]
    fn full_repaint_reason_indices_cover_all_buckets() {
        let cases = [
            (FullRepaintReason::Resize, 0),
            (FullRepaintReason::AltScreen, 1),
            (FullRepaintReason::Theme, 2),
            (FullRepaintReason::Scrollback, 3),
            (FullRepaintReason::Overflow, 4),
            (FullRepaintReason::Fallback, 5),
            (FullRepaintReason::NoDamage, 6),
        ];
        for (reason, expected) in cases {
            assert_eq!(reason.index(), expected, "{reason:?}");
        }
    }

    #[test]
    fn render_window_records_each_full_repaint_bucket() {
        let cases = [
            FullRepaintReason::Resize,
            FullRepaintReason::AltScreen,
            FullRepaintReason::Theme,
            FullRepaintReason::Scrollback,
            FullRepaintReason::Overflow,
            FullRepaintReason::Fallback,
            FullRepaintReason::NoDamage,
        ];
        for reason in cases {
            let now = Instant::now();
            let mut window = RenderWindow {
                started_at: Some(now - Duration::from_secs(1)),
                ..RenderWindow::default()
            };
            let summary = window
                .record(
                    RenderFrame {
                        full_repaint_reason: Some(reason),
                        ..RenderFrame::default()
                    },
                    now,
                )
                .expect("one-second render window should emit a summary");
            assert_eq!(summary.dominant_full_repaint_reason, Some(reason));
        }
    }

    #[test]
    fn damage_rows_table_covers_dirty_and_scroll_regions() {
        let cases = [
            (0, vec![]),
            (1, vec![2]),
            (2, vec![1, 2]),
            (3, vec![0, 1, 2]),
            (4, vec![2, 3]),
        ];
        for (kind, expected) in cases {
            let mut damage = GridDamage::empty(4, 3);
            match kind {
                1 => damage.mark_cell(2, 1),
                2 => damage.push_scroll(prismattyc_core::ScrollDamage {
                    top: 1,
                    bottom: 2,
                    delta: 1,
                }),
                3 => {
                    damage.mark_cell(0, 1);
                    damage.push_scroll(prismattyc_core::ScrollDamage {
                        top: 1,
                        bottom: 2,
                        delta: 1,
                    });
                }
                4 => damage.push_scroll(prismattyc_core::ScrollDamage {
                    top: 2,
                    bottom: 99,
                    delta: 1,
                }),
                _ => {}
            }
            assert_eq!(damage_rows(&damage), expected, "case {kind}");
        }
    }

    #[test]
    fn damage_painted_cells_table_covers_dirty_and_scroll_accounting() {
        let cases = [0, 1, 2];
        for kind in cases {
            let (damage, rows, expected) = match kind {
                0 => (GridDamage::empty(3, 4), vec![0, 2], 8),
                1 => {
                    let mut damage = GridDamage::empty(3, 4);
                    damage.mark_cell(0, 0);
                    damage.mark_cell(2, 1);
                    (damage, vec![0, 2], 2)
                }
                2 => {
                    let mut damage = GridDamage::empty(3, 4);
                    damage.push_scroll(prismattyc_core::ScrollDamage {
                        top: 0,
                        bottom: 2,
                        delta: 1,
                    });
                    (damage, vec![1, 2], 8)
                }
                _ => unreachable!(),
            };
            assert_eq!(
                damage_painted_cells(&damage, &rows),
                expected,
                "case {kind}"
            );
        }
    }

    #[test]
    fn framebuffer_scroll_plan_rejects_each_invalid_geometry() {
        let cases = [
            (
                "zero width",
                FramebufferScrollRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    row_height: 1,
                    rows: 1,
                },
                1,
                1,
                1,
            ),
            (
                "zero row height",
                FramebufferScrollRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    row_height: 0,
                    rows: 1,
                },
                1,
                1,
                1,
            ),
            (
                "zero rows",
                FramebufferScrollRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    row_height: 1,
                    rows: 0,
                },
                1,
                1,
                1,
            ),
            (
                "past right edge",
                FramebufferScrollRect {
                    x: 1,
                    y: 0,
                    width: 1,
                    row_height: 1,
                    rows: 1,
                },
                1,
                1,
                1,
            ),
            (
                "past bottom edge",
                FramebufferScrollRect {
                    x: 0,
                    y: 1,
                    width: 1,
                    row_height: 1,
                    rows: 1,
                },
                1,
                1,
                1,
            ),
            (
                "short backing buffer",
                FramebufferScrollRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    row_height: 1,
                    rows: 1,
                },
                1,
                1,
                0,
            ),
        ];

        for (name, rect, stride, frame_height, buffer_len) in cases {
            assert!(
                framebuffer_scroll_plan(rect, stride, frame_height, buffer_len, &[]).is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn framebuffer_scroll_plan_covers_event_boundaries() {
        let rect = FramebufferScrollRect {
            x: 0,
            y: 0,
            width: 1,
            row_height: 1,
            rows: 3,
        };
        let cases = [
            (
                "clipped bottom",
                ScrollDamage {
                    top: 0,
                    bottom: 3,
                    delta: 1,
                },
                Some((
                    vec![FramebufferScrollCopy {
                        src_y: 1,
                        dst_y: 0,
                        scanlines: 2,
                        direction: FramebufferScrollDirection::Forward,
                    }],
                    2,
                )),
            ),
            (
                "singleton",
                ScrollDamage {
                    top: 1,
                    bottom: 1,
                    delta: 1,
                },
                None,
            ),
            (
                "downward at frame bottom",
                ScrollDamage {
                    top: 0,
                    bottom: 2,
                    delta: -1,
                },
                Some((
                    vec![FramebufferScrollCopy {
                        src_y: 0,
                        dst_y: 1,
                        scanlines: 2,
                        direction: FramebufferScrollDirection::Reverse,
                    }],
                    2,
                )),
            ),
            (
                "inverted bounds",
                ScrollDamage {
                    top: 4,
                    bottom: 3,
                    delta: 1,
                },
                Some((Vec::new(), 0)),
            ),
        ];

        for (name, event, expected) in cases {
            assert_eq!(
                framebuffer_scroll_plan(rect, 1, 3, 3, &[event]),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn scroll_span_uses_checked_inclusive_bounds() {
        assert_eq!(scroll_span(2, 5), Some(4));
        assert_eq!(scroll_span(5, 4), None);
        assert_eq!(scroll_span(1, usize::MAX), Some(usize::MAX));
        assert_eq!(scroll_span(0, usize::MAX), None);
    }

    #[test]
    fn framebuffer_scroll_blit_moves_only_terminal_pixels_in_both_directions() {
        let stride = 7;
        let frame_height = 7;
        let rect = FramebufferScrollRect {
            x: 2,
            y: 1,
            width: 3,
            row_height: 1,
            rows: 4,
        };
        let original: Vec<u32> = (0..stride * frame_height)
            .map(|pixel| pixel as u32)
            .collect();

        let mut up = original.clone();
        assert_eq!(
            apply_framebuffer_scroll_blits(
                &mut up,
                stride,
                frame_height,
                rect,
                &[ScrollDamage {
                    top: 0,
                    bottom: 3,
                    delta: 1,
                }],
                &[],
            ),
            Some(3)
        );
        for row in 0..3 {
            let dst = (rect.y + row) * stride + rect.x;
            let src = (rect.y + row + 1) * stride + rect.x;
            assert_eq!(&up[dst..dst + rect.width], &original[src..src + rect.width]);
        }

        let mut down = original.clone();
        assert_eq!(
            apply_framebuffer_scroll_blits(
                &mut down,
                stride,
                frame_height,
                rect,
                &[ScrollDamage {
                    top: 0,
                    bottom: 3,
                    delta: -1,
                }],
                &[],
            ),
            Some(3)
        );
        for row in 1..4 {
            let dst = (rect.y + row) * stride + rect.x;
            let src = (rect.y + row - 1) * stride + rect.x;
            assert_eq!(
                &down[dst..dst + rect.width],
                &original[src..src + rect.width]
            );
        }

        for y in 0..frame_height {
            for x in 0..stride {
                if x < rect.x || x >= rect.x + rect.width || y < rect.y || y >= rect.y + rect.rows {
                    let index = y * stride + x;
                    assert_eq!(up[index], original[index]);
                    assert_eq!(down[index], original[index]);
                }
            }
        }
    }

    #[test]
    fn framebuffer_scroll_blit_preserves_event_order_and_rejects_no_reuse() {
        let rect = FramebufferScrollRect {
            x: 0,
            y: 0,
            width: 1,
            row_height: 1,
            rows: 5,
        };
        let events = [
            ScrollDamage {
                top: 0,
                bottom: 4,
                delta: 1,
            },
            ScrollDamage {
                top: 1,
                bottom: 3,
                delta: -1,
            },
        ];
        let mut buffer = vec![0, 1, 2, 3, 4];
        assert_eq!(
            apply_framebuffer_scroll_blits(&mut buffer, 1, 5, rect, &events, &[]),
            Some(6)
        );
        assert_eq!(buffer, vec![1, 2, 2, 3, 4]);

        let original = vec![0, 1, 2, 3, 4];
        let mut rejected = original.clone();
        assert_eq!(
            apply_framebuffer_scroll_blits(
                &mut rejected,
                1,
                5,
                rect,
                &[ScrollDamage {
                    top: 1,
                    bottom: 3,
                    delta: 3,
                }],
                &[],
            ),
            None
        );
        assert_eq!(rejected, original);
    }

    #[test]
    fn framebuffer_scroll_blit_rejects_only_overlapping_chrome_sources() {
        let rect = FramebufferScrollRect {
            x: 2,
            y: 1,
            width: 3,
            row_height: 2,
            rows: 4,
        };
        let original: Vec<u32> = (0..70).collect();
        let events = [ScrollDamage {
            top: 0,
            bottom: 3,
            delta: -1,
        }];
        for (chrome, rejected) in [
            (PixelRect::new(2, 1, 1, 1), true),
            (PixelRect::new(4, 6, 1, 1), true),
            (PixelRect::new(5, 1, 1, 1), false),
            (PixelRect::new(2, 7, 3, 2), false),
            (PixelRect::new(2, 1, 0, 1), false),
        ] {
            let mut pixels = original.clone();
            let result =
                apply_framebuffer_scroll_blits(&mut pixels, 7, 10, rect, &events, &[chrome]);
            assert_eq!(result, if rejected { None } else { Some(3) });
            if rejected {
                assert_eq!(pixels, original);
            } else {
                assert_eq!(&pixels[23..26], &original[9..12]);
            }
        }
        let mut pixels = original.clone();
        let events = [
            ScrollDamage {
                top: 2,
                bottom: 3,
                delta: -1,
            },
            ScrollDamage {
                top: 0,
                bottom: 3,
                delta: -1,
            },
        ];
        assert_eq!(
            apply_framebuffer_scroll_blits(
                &mut pixels,
                7,
                10,
                rect,
                &events,
                &[PixelRect::new(2, 1, 1, 1)]
            ),
            None,
        );
        assert_eq!(pixels, original);
    }

    #[test]
    fn row_after_scrolls_tracks_copied_cursor_pixels() {
        struct Case {
            name: &'static str,
            row: usize,
            rows: usize,
            events: Vec<ScrollDamage>,
            expected: Option<usize>,
        }
        let cases = [
            Case {
                name: "outside screen",
                row: 3,
                rows: 3,
                events: vec![],
                expected: None,
            },
            Case {
                name: "clipped negative bottom",
                row: 2,
                rows: 3,
                events: vec![ScrollDamage {
                    top: 0,
                    bottom: 3,
                    delta: -1,
                }],
                expected: None,
            },
            Case {
                name: "zero delta ignored",
                row: 1,
                rows: 3,
                events: vec![ScrollDamage {
                    top: 0,
                    bottom: 2,
                    delta: 0,
                }],
                expected: Some(1),
            },
            Case {
                name: "singleton exposed",
                row: 1,
                rows: 3,
                events: vec![ScrollDamage {
                    top: 1,
                    bottom: 1,
                    delta: 1,
                }],
                expected: None,
            },
            Case {
                name: "row before event",
                row: 0,
                rows: 3,
                events: vec![ScrollDamage {
                    top: 1,
                    bottom: 2,
                    delta: 1,
                }],
                expected: Some(0),
            },
            Case {
                name: "positive first survivor",
                row: 2,
                rows: 3,
                events: vec![ScrollDamage {
                    top: 1,
                    bottom: 2,
                    delta: 1,
                }],
                expected: Some(1),
            },
            Case {
                name: "nonzero top span",
                row: 5,
                rows: 6,
                events: vec![ScrollDamage {
                    top: 2,
                    bottom: 5,
                    delta: 3,
                }],
                expected: Some(2),
            },
            Case {
                name: "negative last survivor",
                row: 2,
                rows: 5,
                events: vec![ScrollDamage {
                    top: 1,
                    bottom: 4,
                    delta: -2,
                }],
                expected: Some(4),
            },
            Case {
                name: "negative exposed",
                row: 4,
                rows: 6,
                events: vec![ScrollDamage {
                    top: 1,
                    bottom: 4,
                    delta: -1,
                }],
                expected: None,
            },
        ];

        for case in cases {
            assert_eq!(
                row_after_scrolls(case.row, case.rows, &case.events),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn insert_paint_row_sorts_deduplicates_and_rejects_the_bottom_edge() {
        let mut rows = vec![2, 0, 2];
        insert_paint_row(&mut rows, 1, 3);
        assert_eq!(rows, vec![0, 1, 2]);

        let mut rows = vec![0, 1];
        insert_paint_row(&mut rows, 3, 3);
        assert_eq!(rows, vec![0, 1]);
    }

    #[test]
    fn transient_overlay_table_covers_each_painter() {
        let cases = [
            (
                "footer",
                TransientOverlayState {
                    footer_visible: true,
                    ..Default::default()
                },
            ),
            (
                "restore prompt",
                TransientOverlayState {
                    restore_prompt: true,
                    ..Default::default()
                },
            ),
            (
                "splash",
                TransientOverlayState {
                    splash: true,
                    ..Default::default()
                },
            ),
            (
                "palette",
                TransientOverlayState {
                    palette: true,
                    ..Default::default()
                },
            ),
            (
                "theme-picker",
                TransientOverlayState {
                    theme_picker: true,
                    ..Default::default()
                },
            ),
            (
                "space-picker",
                TransientOverlayState {
                    space_picker: true,
                    ..Default::default()
                },
            ),
            (
                "context-menu",
                TransientOverlayState {
                    context_menu: true,
                    ..Default::default()
                },
            ),
            (
                "find",
                TransientOverlayState {
                    find_active: true,
                    ..Default::default()
                },
            ),
            (
                "tab-rename",
                TransientOverlayState {
                    tab_rename: true,
                    ..Default::default()
                },
            ),
            (
                "walkthrough",
                TransientOverlayState {
                    walkthrough: true,
                    ..Default::default()
                },
            ),
            (
                "drag-toast",
                TransientOverlayState {
                    drag_toast: true,
                    ..Default::default()
                },
            ),
            (
                "config-path",
                TransientOverlayState {
                    config_path: true,
                    ..Default::default()
                },
            ),
            (
                "config-error",
                TransientOverlayState {
                    config_error: true,
                    ..Default::default()
                },
            ),
            (
                "preedit",
                TransientOverlayState {
                    preedit: true,
                    ..Default::default()
                },
            ),
            (
                "bell-toasts",
                TransientOverlayState {
                    bell_toasts: true,
                    ..Default::default()
                },
            ),
            (
                "title-notice",
                TransientOverlayState {
                    title_notice: true,
                    ..Default::default()
                },
            ),
            (
                "hover-target",
                TransientOverlayState {
                    hover_target: true,
                    ..Default::default()
                },
            ),
            (
                "save-space",
                TransientOverlayState {
                    save_space: true,
                    ..Default::default()
                },
            ),
        ];
        assert!(!transient_overlay_visible(TransientOverlayState::default()));
        for (name, state) in cases {
            assert!(transient_overlay_visible(state), "{name}");
        }
    }

    #[test]
    fn save_space_modal_open_is_only_the_plus_name_prompt() {
        assert!(!save_space_modal_open(None));
        let rename = space_rail::RailEdit {
            target: Some(0),
            buffer: "alpha".into(),
            selected: true,
        };
        assert!(
            !save_space_modal_open(Some(&rename)),
            "inline rename stays on the chip"
        );
        let mut typed = space_rail::RailEdit {
            target: None,
            buffer: String::new(),
            selected: false,
        };
        assert!(save_space_modal_open(Some(&typed)));
        typed.buffer.push_str("wori");
        assert!(
            save_space_modal_open(Some(&typed)),
            "typing must keep the modal on the full-repaint overlay path"
        );
        assert!(overlay_requires_full_repaint(true, true));
    }

    #[test]
    fn expire_bell_toasts_drops_elapsed_deadlines() {
        let now = Instant::now();
        let mut deadlines = vec![now, now + Duration::from_secs(1)];
        assert!(retain_live_deadlines(&mut deadlines, now, |until| *until));
        assert_eq!(deadlines, vec![now + Duration::from_secs(1)]);
        assert!(!retain_live_deadlines(&mut deadlines, now, |until| *until));
        assert!(retain_live_deadlines(
            &mut deadlines,
            now + Duration::from_secs(1),
            |until| *until
        ));
        assert!(deadlines.is_empty());
    }

    #[test]
    fn pane_space_membership_formats_saved_space_count() {
        assert_eq!(pane_space_membership("a", &[]), "a · not in a saved space");
        assert_eq!(
            pane_space_membership("a", &["alpha".into()]),
            "a · space alpha"
        );
        assert_eq!(
            pane_space_membership("a", &["alpha".into(), "web".into()]),
            "a · spaces alpha, web"
        );
    }

    #[test]
    fn editor_command_resolution_uses_first_valid_nonempty_candidate() {
        let cases = [
            (
                None,
                None,
                EditorCommand {
                    program: "nano".into(),
                    args: vec![],
                },
            ),
            (
                Some(""),
                Some("  "),
                EditorCommand {
                    program: "nano".into(),
                    args: vec![],
                },
            ),
            (
                Some("code --wait"),
                Some("vim"),
                EditorCommand {
                    program: "code".into(),
                    args: vec!["--wait".into()],
                },
            ),
            (
                Some("'editor with spaces' --new"),
                None,
                EditorCommand {
                    program: "editor with spaces".into(),
                    args: vec!["--new".into()],
                },
            ),
            (
                Some("unterminated\""),
                Some("nvim --clean"),
                EditorCommand {
                    program: "nvim".into(),
                    args: vec!["--clean".into()],
                },
            ),
        ];
        for (visual, editor, expected) in cases {
            assert_eq!(resolve_editor_command(visual, editor), expected);
        }
    }

    #[test]
    fn partial_raster_policy_tracks_present_backend() {
        #[cfg(target_os = "macos")]
        assert!(backend_supports_partial_raster(PartialRasterBackend::Mac));
        assert!(backend_supports_partial_raster(
            PartialRasterBackend::Softbuffer { wayland: false }
        ));
        assert!(!backend_supports_partial_raster(
            PartialRasterBackend::Softbuffer { wayland: true }
        ));
        #[cfg(target_os = "linux")]
        assert!(backend_supports_partial_raster(
            PartialRasterBackend::WaylandShm
        ));
        #[cfg(feature = "gpu")]
        assert!(!backend_supports_partial_raster(PartialRasterBackend::Gpu));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_shm_requires_alpha_and_native_wayland() {
        assert!(!use_wayland_shm(false, false));
        assert!(!use_wayland_shm(false, true));
        assert!(!use_wayland_shm(true, false));
        assert!(use_wayland_shm(true, true));
    }

    #[test]
    fn write_present_png_round_trips_rgba() {
        let dir = std::env::temp_dir().join(format!(
            "pt290-dump-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frame.png");
        // 3x2 so width*height != width/height; decode every pixel.
        let pixels = [
            0xffff6e63u32,
            0x75d0d0d0,
            0x0000aa11,
            0x01112233,
            0x80abcdef,
            0xfe010203,
        ];
        write_present_png(&path, &pixels, 3, 2).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let decoder = png::Decoder::new(file);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.width, 3);
        assert_eq!(info.height, 2);
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(
            &buf[..24],
            &[
                0xff, 0x6e, 0x63, 0xff, 0xd0, 0xd0, 0xd0, 0x75, 0x00, 0xaa, 0x11, 0x00, 0x11, 0x22,
                0x33, 0x01, 0xab, 0xcd, 0xef, 0x80, 0x01, 0x02, 0x03, 0xfe
            ]
        );
        let empty_w = write_present_png(&path, &[0], 0, 1)
            .unwrap_err()
            .to_string();
        assert!(empty_w.contains("empty frame"), "{empty_w}");
        let empty_h = write_present_png(&path, &[0], 1, 0)
            .unwrap_err()
            .to_string();
        assert!(empty_h.contains("empty frame"), "{empty_h}");
        assert!(write_present_png(&path, &[0, 1, 2, 3, 4], 3, 2).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _guard = DUMP_PRESENT_ENV.lock().unwrap();
        assert!(dump_present_path().is_none());
    }

    #[test]
    fn maybe_dump_present_writes_png_when_env_set() {
        let _guard = DUMP_PRESENT_ENV.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "pt290-maybe-dump-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frame.png");
        let mut seq = 0;
        maybe_dump_present(None, &mut seq, &[0x00ff6e63u32], 1, 1, None);
        assert_eq!(seq, 0);
        maybe_dump_present(
            Some(&path),
            &mut seq,
            &[0x00ff6e63u32, 0x00d0d0d0],
            2,
            1,
            None,
        );
        assert_eq!(seq, 1);
        assert!(path.is_file(), "dump png missing");
        let sidecar = path.with_extension("json");
        assert!(sidecar.is_file(), "dump sidecar missing");
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("\"width\":2"), "{body}");
        assert!(body.contains("\"height\":1"), "{body}");
        assert!(body.contains("\"full\":false"), "{body}");
        assert!(body.contains("\"seq\":1"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splash_key_plan_table_covers_every_action() {
        use prismattyc_core::splash::Topic;
        let cases = [
            (None, SplashKeyPlan::None),
            (Some(splash::Action::Dismiss), SplashKeyPlan::Dismiss),
            (Some(splash::Action::Quit), SplashKeyPlan::Quit),
            (
                Some(splash::Action::Show(Topic::Walkthrough)),
                SplashKeyPlan::StartWalkthrough,
            ),
            (
                Some(splash::Action::Show(Topic::Docs)),
                SplashKeyPlan::SetPage(splash::Page::Topic(Topic::Docs)),
            ),
            (
                Some(splash::Action::Back),
                SplashKeyPlan::SetPage(splash::Page::Main),
            ),
        ];
        for (action, expected) in cases {
            assert_eq!(splash_key_plan(action), expected, "{action:?}");
        }
    }

    #[test]
    fn apply_splash_outcome_table() {
        use prismattyc_core::splash::Topic;
        let base = splash::Splash {
            page: splash::Page::Main,
            tip: 3,
            shown_at: Instant::now(),
            last_tick_ms: 40,
            animated: true,
            resume: true,
        };
        let mut splash = Some(base);
        let none = apply_splash_outcome(&mut splash, base, SplashKeyPlan::None);
        assert_eq!(
            none,
            SplashApply {
                quit: false,
                start_walkthrough: false,
                dirty: true
            }
        );
        assert_eq!(splash.unwrap().page, splash::Page::Main);

        let mut splash = Some(base);
        let dismiss = apply_splash_outcome(&mut splash, base, SplashKeyPlan::Dismiss);
        assert_eq!(
            dismiss,
            SplashApply {
                quit: false,
                start_walkthrough: false,
                dirty: true
            }
        );
        assert!(splash.is_none());

        let mut splash = Some(base);
        let quit = apply_splash_outcome(&mut splash, base, SplashKeyPlan::Quit);
        assert_eq!(
            quit,
            SplashApply {
                quit: true,
                start_walkthrough: false,
                dirty: false
            }
        );
        assert!(splash.is_some());

        let mut splash = Some(base);
        let walk = apply_splash_outcome(&mut splash, base, SplashKeyPlan::StartWalkthrough);
        assert_eq!(
            walk,
            SplashApply {
                quit: false,
                start_walkthrough: true,
                dirty: true
            }
        );
        assert_eq!(splash.unwrap().page, splash::Page::Main);

        let mut splash = Some(base);
        let show = apply_splash_outcome(
            &mut splash,
            base,
            SplashKeyPlan::SetPage(splash::Page::Topic(Topic::Docs)),
        );
        assert_eq!(
            show,
            SplashApply {
                quit: false,
                start_walkthrough: false,
                dirty: true
            }
        );
        let shown = splash.unwrap();
        assert_eq!(shown.page, splash::Page::Topic(Topic::Docs));
        assert_eq!(shown.tip, 3);
        assert_eq!(shown.last_tick_ms, 40);
        assert!(shown.animated);
        assert!(shown.resume);

        let topic = splash::Splash {
            page: splash::Page::Topic(Topic::Docs),
            ..base
        };
        let mut splash = Some(topic);
        let back = apply_splash_outcome(
            &mut splash,
            topic,
            SplashKeyPlan::SetPage(splash::Page::Main),
        );
        assert_eq!(
            back,
            SplashApply {
                quit: false,
                start_walkthrough: false,
                dirty: true
            }
        );
        assert_eq!(splash.unwrap().page, splash::Page::Main);
    }

    #[test]
    fn finish_splash_apply_table() {
        let cases = [
            (
                "quit-false-or-keeps-dirty",
                true,
                SplashApply {
                    quit: false,
                    start_walkthrough: false,
                    dirty: false,
                },
                true,
                false,
            ),
            (
                "quit-false-or-sets-dirty",
                false,
                SplashApply {
                    quit: false,
                    start_walkthrough: false,
                    dirty: true,
                },
                true,
                false,
            ),
            (
                "quit-true-leaves-clean",
                false,
                SplashApply {
                    quit: true,
                    start_walkthrough: false,
                    dirty: false,
                },
                false,
                true,
            ),
            (
                "quit-true-or-keeps-dirty",
                true,
                SplashApply {
                    quit: true,
                    start_walkthrough: false,
                    dirty: false,
                },
                true,
                true,
            ),
        ];
        for (name, initial, applied, expect_dirty, expect_quit) in cases {
            let mut dirty = initial;
            let quit = finish_splash_apply(&mut dirty, applied);
            assert_eq!(quit, expect_quit, "{name} quit");
            assert_eq!(dirty, expect_dirty, "{name} dirty");
        }
    }

    #[test]
    fn e2e_dismiss_splash_ms_table() {
        let _guard = DUMP_PRESENT_ENV.lock().unwrap();
        let key = "PRISMATTYC_E2E_DISMISS_SPLASH_MS";
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        assert_eq!(e2e_dismiss_splash_ms(), None);
        std::env::set_var(key, "");
        assert_eq!(e2e_dismiss_splash_ms(), None);
        std::env::set_var(key, "0");
        assert_eq!(e2e_dismiss_splash_ms(), None);
        std::env::set_var(key, "abc");
        assert_eq!(e2e_dismiss_splash_ms(), None);
        std::env::set_var(key, "1500");
        assert_eq!(e2e_dismiss_splash_ms(), Some(1500));
        match prev {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn e2e_dismiss_due_table() {
        let cases = [
            ("below", 1499, 1500, false),
            ("equal", 1500, 1500, true),
            ("above", 1501, 1500, true),
        ];
        for (name, elapsed, deadline, expected) in cases {
            assert_eq!(e2e_dismiss_due(elapsed, deadline), expected, "{name}");
        }
    }

    #[test]
    fn present_png_empty_table() {
        assert!(present_png_empty(0, 1));
        assert!(present_png_empty(1, 0));
        assert!(!present_png_empty(1, 1));
        assert!(!present_png_empty(3, 2));
    }

    #[test]
    fn present_png_parent_table() {
        assert!(present_png_parent(Path::new("frame.png")).is_none());
        assert_eq!(
            present_png_parent(Path::new("dir/frame.png")).map(|p| p.as_os_str()),
            Some(std::ffi::OsStr::new("dir"))
        );
    }

    /// PT-87/PT-203: request an alpha visual for transparency or native blur,
    /// so the default opaque path remains untouched.
    #[test]
    fn alpha_visual_is_requested_only_below_full_opacity() {
        let default = config::ConfigFile::default();
        assert!(!wants_alpha_visual(&default));
        assert!(!wants_alpha_visual(&config::ConfigFile {
            window_opacity: Some(1.0),
            ..config::ConfigFile::default()
        }));
        assert!(wants_alpha_visual(&config::ConfigFile {
            window_opacity: Some(0.8),
            ..config::ConfigFile::default()
        }));
        assert!(wants_alpha_visual(&config::ConfigFile {
            chrome_opacity: Some(0.5),
            ..config::ConfigFile::default()
        }));
        assert!(wants_alpha_visual(&config::ConfigFile {
            window_blur: Some(true),
            ..config::ConfigFile::default()
        }));
    }

    #[test]
    fn blur_notice_covers_requested_surface_states() {
        let cases = [
            (false, BlurSurface::Macos, false, false),
            (true, BlurSurface::Macos, false, true),
            (true, BlurSurface::Macos, true, false),
            (true, BlurSurface::Compositor, false, true),
            (true, BlurSurface::Compositor, true, false),
        ];
        for (wanted, surface, installed, expected) in cases {
            assert_eq!(blur_notice(wanted, surface, installed), expected);
        }
    }

    /// PT-124: font pixel size tracks the compositor scale factor and keeps
    /// the 10px floor shared with window open / config reload.
    #[test]
    fn scaled_font_px_tracks_scale_with_floor() {
        assert_eq!(scaled_font_px(16.0, 1.0), 16.0);
        assert_eq!(scaled_font_px(16.0, 1.5), 24.0);
        assert_eq!(scaled_font_px(16.0, 2.0), 32.0);
        assert_eq!(scaled_font_px(16.0, 0.5), 10.0);
    }

    #[test]
    fn space_open_cli_args_pass_create_switch_and_new_window() {
        assert_eq!(
            space_open_cli_args("web", SpaceOpenMode::Switch),
            ["space", "open", "web", "--no-run", "--no-attach"]
        );
        assert_eq!(
            space_open_cli_args("web", SpaceOpenMode::Create),
            ["space", "create", "web", "--no-attach"]
        );
        assert_eq!(
            space_open_cli_args("web", SpaceOpenMode::NewWindow),
            ["space", "open", "web", "--no-run", "--new-window"]
        );
    }

    /// PT-87: a dimmed pane's default background is more translucent than the
    /// focused pane's, so more desktop shows through it.
    #[test]
    fn pane_opacity_scales_the_window_alpha() {
        assert_eq!(scale_alpha(204, 1.0), 204);
        assert_eq!(scale_alpha(204, 0.6), 122);
        assert_eq!(scale_alpha(OPAQUE_ALPHA, 1.0), OPAQUE_ALPHA);
        assert!(scale_alpha(204, 0.6) < scale_alpha(204, 1.0));
    }

    fn mods(ctrl: bool, shift: bool) -> ModifiersState {
        let mut value = ModifiersState::empty();
        value.set(ModifiersState::CONTROL, ctrl);
        value.set(ModifiersState::SHIFT, shift);
        value
    }

    fn mods_logo() -> ModifiersState {
        let mut value = ModifiersState::empty();
        value.set(ModifiersState::SUPER, true);
        value
    }

    #[test]
    fn wheel_input_normalizes_direction_and_lines() {
        let cases = [
            (
                MouseScrollDelta::LineDelta(0.0, 1.0),
                WheelInput {
                    direction: ScrollDirection::Up,
                    host_lines: 3,
                },
            ),
            (
                MouseScrollDelta::LineDelta(0.0, 0.0),
                WheelInput {
                    direction: ScrollDirection::None,
                    host_lines: 0,
                },
            ),
            (
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -15.0)),
                WheelInput {
                    direction: ScrollDirection::Down,
                    host_lines: -2,
                },
            ),
            (
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 2.0)),
                WheelInput {
                    direction: ScrollDirection::Up,
                    host_lines: 1,
                },
            ),
        ];
        for (delta, expected) in cases {
            assert_eq!(wheel_input(&delta, 10), expected);
        }
    }

    #[test]
    fn wheel_decision_routes_modes_table() {
        let cases = [
            (
                WheelInput {
                    direction: ScrollDirection::Up,
                    host_lines: 3,
                },
                false,
                true,
                true,
                true,
                true,
                false,
                24,
                WheelDecision::Rich { step: -1 },
            ),
            (
                WheelInput {
                    direction: ScrollDirection::None,
                    host_lines: 0,
                },
                false,
                true,
                true,
                false,
                false,
                false,
                24,
                WheelDecision::Rich { step: 0 },
            ),
            (
                WheelInput {
                    direction: ScrollDirection::Down,
                    host_lines: -3,
                },
                false,
                false,
                false,
                true,
                true,
                false,
                24,
                WheelDecision::App { button: 65 },
            ),
            (
                WheelInput {
                    direction: ScrollDirection::Up,
                    host_lines: 3,
                },
                false,
                false,
                false,
                true,
                false,
                false,
                24,
                WheelDecision::Consume,
            ),
            (
                WheelInput {
                    direction: ScrollDirection::Up,
                    host_lines: 3,
                },
                false,
                false,
                false,
                false,
                true,
                true,
                24,
                WheelDecision::Consume,
            ),
            (
                WheelInput {
                    direction: ScrollDirection::Up,
                    host_lines: 3,
                },
                true,
                true,
                true,
                true,
                true,
                false,
                24,
                WheelDecision::Host { rows: 23 },
            ),
            (
                WheelInput {
                    direction: ScrollDirection::Down,
                    host_lines: -3,
                },
                false,
                false,
                false,
                false,
                true,
                false,
                24,
                WheelDecision::Host { rows: -3 },
            ),
            (
                WheelInput {
                    direction: ScrollDirection::None,
                    host_lines: 0,
                },
                false,
                false,
                false,
                false,
                true,
                false,
                1,
                WheelDecision::Consume,
            ),
        ];
        for (
            input,
            shift,
            rich_focus_active,
            rich_hit_focused,
            app_wheel,
            app_cursor_focused,
            alt_active,
            page_rows,
            expected,
        ) in cases
        {
            assert_eq!(
                wheel_decision(
                    input,
                    WheelContext {
                        shift,
                        rich_focus_active,
                        rich_hit_focused,
                        app_wheel,
                        app_cursor_focused,
                        alt_active,
                        page_rows,
                    },
                ),
                expected,
                "{input:?} shift={shift} rich={rich_focus_active}/{rich_hit_focused} app={app_wheel}/{app_cursor_focused} alt={alt_active}"
            );
        }
    }

    #[test]
    fn mouse_input_decision_routes_modes_table() {
        let pane = mux::MuxRuntime::spawn("/bin/sh", &[], 2, 2)
            .unwrap()
            .focused_id();
        let hit = rich::WorkspaceHit {
            node_id: 7,
            action_id: 11,
            row: 1,
            col: 2,
        };
        let base = |state, button| MouseInputContext {
            state,
            button,
            shift: false,
            workspace_hit: None,
            rich_hit_focused: false,
            rich_pointer: None,
            url_openable: false,
            suppress_left_release: false,
            tracking: prismattyc_emulator::MouseTracking::Off,
            guest_alt_blocks: false,
            cursor_cell: None,
            left_button_down: false,
        };
        let cases = [
            (
                "shift rich drag",
                MouseInputContext {
                    shift: true,
                    workspace_hit: Some((pane, hit)),
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::RichShiftDrag { pane, hit },
            ),
            (
                "rich press",
                MouseInputContext {
                    workspace_hit: Some((pane, hit)),
                    rich_hit_focused: true,
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::RichPress { pane, hit },
            ),
            (
                "rich release activates same node",
                MouseInputContext {
                    rich_pointer: Some(RichPointerFacts {
                        cancelled: false,
                        same_node: true,
                    }),
                    ..base(ElementState::Released, MouseButton::Left)
                },
                MouseInputDecision::RichRelease {
                    activate: true,
                    finish_selection: false,
                },
            ),
            (
                "cancelled rich drag finishes selection",
                MouseInputContext {
                    rich_pointer: Some(RichPointerFacts {
                        cancelled: true,
                        same_node: false,
                    }),
                    left_button_down: true,
                    ..base(ElementState::Released, MouseButton::Left)
                },
                MouseInputDecision::RichRelease {
                    activate: false,
                    finish_selection: true,
                },
            ),
            (
                "URL owns tracked left press",
                MouseInputContext {
                    url_openable: true,
                    tracking: prismattyc_emulator::MouseTracking::Click,
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::OpenUrl,
            ),
            (
                "suppressed left release",
                MouseInputContext {
                    suppress_left_release: true,
                    tracking: prismattyc_emulator::MouseTracking::Click,
                    ..base(ElementState::Released, MouseButton::Left)
                },
                MouseInputDecision::SuppressLeftRelease,
            ),
            (
                "tracked left press",
                MouseInputContext {
                    tracking: prismattyc_emulator::MouseTracking::Click,
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::App { button: Some(0) },
            ),
            (
                "tracked right release",
                MouseInputContext {
                    tracking: prismattyc_emulator::MouseTracking::Any,
                    ..base(ElementState::Released, MouseButton::Right)
                },
                MouseInputDecision::App { button: Some(2) },
            ),
            (
                "tracked unknown button",
                MouseInputContext {
                    tracking: prismattyc_emulator::MouseTracking::Click,
                    ..base(ElementState::Pressed, MouseButton::Other(7))
                },
                MouseInputDecision::App { button: None },
            ),
            (
                "shift bypasses tracking",
                MouseInputContext {
                    shift: true,
                    tracking: prismattyc_emulator::MouseTracking::Click,
                    cursor_cell: Some((pane, 2, 3)),
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::BeginSelection {
                    pane,
                    row: 2,
                    col: 3,
                },
            ),
            (
                "guest alt blocks plain left",
                MouseInputContext {
                    guest_alt_blocks: true,
                    cursor_cell: Some((pane, 2, 3)),
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::GuestAltBlock,
            ),
            (
                "shift bypasses guest alt block",
                MouseInputContext {
                    shift: true,
                    guest_alt_blocks: true,
                    cursor_cell: Some((pane, 2, 3)),
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::BeginSelection {
                    pane,
                    row: 2,
                    col: 3,
                },
            ),
            (
                "wheel-only alt remains selectable",
                MouseInputContext {
                    cursor_cell: Some((pane, 2, 3)),
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::BeginSelection {
                    pane,
                    row: 2,
                    col: 3,
                },
            ),
            (
                "ordinary host press begins selection",
                MouseInputContext {
                    cursor_cell: Some((pane, 2, 3)),
                    ..base(ElementState::Pressed, MouseButton::Left)
                },
                MouseInputDecision::BeginSelection {
                    pane,
                    row: 2,
                    col: 3,
                },
            ),
            (
                "left release finishes selection",
                MouseInputContext {
                    left_button_down: true,
                    ..base(ElementState::Released, MouseButton::Left)
                },
                MouseInputDecision::FinishSelection,
            ),
            (
                "left release without selection",
                base(ElementState::Released, MouseButton::Left),
                MouseInputDecision::Ignore {
                    clear_app_button: true,
                },
            ),
            (
                "untracked non-left input",
                base(ElementState::Pressed, MouseButton::Right),
                MouseInputDecision::Ignore {
                    clear_app_button: false,
                },
            ),
        ];
        for (name, context, expected) in cases {
            assert_eq!(mouse_input_decision(context), expected, "{name}");
        }
    }

    #[test]
    fn footer_visibility_routes_held_and_linger_table() {
        let now = Instant::now();
        let cases = [
            ("held-chord", true, true, None, now, true),
            (
                "linger-active",
                false,
                false,
                Some(now + Duration::from_millis(1)),
                now,
                true,
            ),
            ("linger-expired", false, false, Some(now), now, false),
            ("neither", false, false, None, now, false),
            (
                "held-chord wins over expired linger",
                true,
                true,
                Some(now),
                now,
                true,
            ),
            ("control only", true, false, None, now, false),
            ("shift only", false, true, None, now, false),
        ];
        for (name, control, shift, linger_until, now, expected) in cases {
            assert_eq!(
                footer_visibility(control, shift, linger_until, now),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn overlay_clip_routes_footer_and_pane_table() {
        let pane = ClipRect {
            x: 0,
            y: 0,
            w: 100,
            h: 200,
        };
        let cell_h = 20;
        let window_h = 200;
        let cases = [
            (
                "no footer",
                0,
                ClipRect {
                    x: 10,
                    y: 20,
                    w: 40,
                    h: 30,
                },
                Some(ClipRect {
                    x: 10,
                    y: 20,
                    w: 40,
                    h: 30,
                }),
            ),
            (
                "footer inside pane",
                1,
                ClipRect {
                    x: 0,
                    y: 100,
                    w: 100,
                    h: 50,
                },
                Some(ClipRect {
                    x: 0,
                    y: 100,
                    w: 100,
                    h: 50,
                }),
            ),
            (
                "footer covering the overlay fully",
                1,
                ClipRect {
                    x: 0,
                    y: 180,
                    w: 100,
                    h: 20,
                },
                None,
            ),
            (
                "overlay outside the pane",
                0,
                ClipRect {
                    x: 200,
                    y: 0,
                    w: 50,
                    h: 50,
                },
                None,
            ),
        ];
        for (name, footer_rows, overlay, expected) in cases {
            assert_eq!(
                overlay_clip(pane, footer_rows, cell_h, window_h, overlay),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn background_decision_routes_cache_states_table() {
        let exact = Some(BackgroundCacheMeta {
            width: 4,
            height: 3,
            pixel_len: 12,
        });
        let mismatched = Some(BackgroundCacheMeta {
            width: 5,
            height: 3,
            pixel_len: 15,
        });
        let malformed = Some(BackgroundCacheMeta {
            width: 4,
            height: 3,
            pixel_len: 11,
        });
        let cases = [
            (None, true, false, BackgroundDecision::Rebuild),
            (None, false, false, BackgroundDecision::Fill),
            (mismatched, true, false, BackgroundDecision::Rebuild),
            (mismatched, false, false, BackgroundDecision::Fill),
            (
                exact,
                false,
                false,
                BackgroundDecision::Copy {
                    rewrite_alpha: false,
                },
            ),
            (
                exact,
                false,
                true,
                BackgroundDecision::Copy {
                    rewrite_alpha: true,
                },
            ),
            (malformed, true, true, BackgroundDecision::Fill),
        ];
        for (cache, has_png, rewrite_alpha, expected) in cases {
            assert_eq!(
                background_decision(cache, 4, 3, has_png, rewrite_alpha),
                expected,
                "cache={cache:?} png={has_png} alpha={rewrite_alpha}"
            );
        }
    }

    #[test]
    fn overlay_paint_decision_routes_modes_table() {
        let base = OverlayPaintContext {
            pane_focused: true,
            live_view: true,
            cursor_visible: true,
            preedit_present: false,
            ime_modal: false,
            footer_visible: false,
            scroll_chip_enabled: true,
            find_active: false,
            palette_active: false,
        };
        let expected = |input, ime, chip, find, palette, footer| OverlayPaintDecision {
            input,
            paint_ime_cursor_area: ime,
            paint_scroll_chip: chip,
            paint_find_prompt: find,
            paint_palette: palette,
            footer_visible: footer,
        };
        let cases = [
            (
                "focused live visible cursor",
                base,
                expected(InputOverlay::Cursor, true, false, false, false, false),
            ),
            (
                "hidden cursor",
                OverlayPaintContext {
                    cursor_visible: false,
                    ..base
                },
                expected(InputOverlay::None, true, false, false, false, false),
            ),
            (
                "unfocused pane",
                OverlayPaintContext {
                    pane_focused: false,
                    ..base
                },
                expected(InputOverlay::None, false, false, false, false, false),
            ),
            (
                "preedit suppresses cursor",
                OverlayPaintContext {
                    preedit_present: true,
                    ..base
                },
                expected(InputOverlay::Preedit, true, false, false, false, false),
            ),
            (
                "scrolled pane with chip enabled",
                OverlayPaintContext {
                    live_view: false,
                    ..base
                },
                expected(InputOverlay::None, false, true, false, false, false),
            ),
            (
                "scrolled pane with chip disabled",
                OverlayPaintContext {
                    live_view: false,
                    scroll_chip_enabled: false,
                    ..base
                },
                expected(InputOverlay::None, false, false, false, false, false),
            ),
            (
                "find active at live tail",
                OverlayPaintContext {
                    find_active: true,
                    ime_modal: true,
                    ..base
                },
                expected(InputOverlay::Cursor, false, false, true, false, false),
            ),
            (
                "find active while scrolled",
                OverlayPaintContext {
                    live_view: false,
                    find_active: true,
                    ..base
                },
                expected(InputOverlay::None, false, true, true, false, false),
            ),
            (
                "find active on unfocused pane",
                OverlayPaintContext {
                    pane_focused: false,
                    live_view: false,
                    find_active: true,
                    ..base
                },
                expected(InputOverlay::None, false, true, false, false, false),
            ),
            (
                "palette active",
                OverlayPaintContext {
                    palette_active: true,
                    ime_modal: true,
                    ..base
                },
                expected(InputOverlay::Cursor, false, false, false, true, false),
            ),
            (
                "other IME modal suppresses preedit",
                OverlayPaintContext {
                    ime_modal: true,
                    preedit_present: true,
                    ..base
                },
                expected(InputOverlay::None, false, false, false, false, false),
            ),
            (
                "footer does not change paint gates",
                OverlayPaintContext {
                    footer_visible: true,
                    ..base
                },
                expected(InputOverlay::Cursor, true, false, false, false, true),
            ),
            (
                "find and palette remain independent",
                OverlayPaintContext {
                    find_active: true,
                    palette_active: true,
                    ime_modal: true,
                    ..base
                },
                expected(InputOverlay::Cursor, false, false, true, true, false),
            ),
        ];
        for (name, context, expected) in cases {
            assert_eq!(overlay_paint_decision(context), expected, "{name}");
        }
    }

    fn table_action(
        logical: &Key,
        physical: PhysicalKey,
        modifiers: ModifiersState,
    ) -> Option<keybind::Action> {
        keybind::KeyMap::default().action(logical, physical, modifiers)
    }

    /// Default-table mux lookup (what the old `mux_command` predicate did).
    fn mux_command(
        logical: &Key,
        physical: PhysicalKey,
        modifiers: ModifiersState,
    ) -> Option<MuxCommand> {
        table_action(logical, physical, modifiers).and_then(mux_command_for)
    }

    #[test]
    fn ime_action_tracks_lifecycle_and_commits_utf8() {
        let mut preedit = Preedit {
            text: "stale".into(),
            cursor: Some((1, 2)),
        };
        assert_eq!(ime_action(&Ime::Enabled, &mut preedit), None);
        assert_eq!(preedit, Preedit::default());

        assert_eq!(
            ime_action(&Ime::Preedit("日本語".into(), Some((3, 6))), &mut preedit,),
            None
        );
        assert_eq!(preedit.text, "日本語");
        assert_eq!(preedit.cursor, Some((3, 6)));

        assert_eq!(
            ime_action(&Ime::Commit("é界".into()), &mut preedit),
            Some("é界".as_bytes().to_vec())
        );
        assert_eq!(preedit, Preedit::default());

        preedit.text = "残り".into();
        preedit.cursor = Some((0, 3));
        assert_eq!(ime_action(&Ime::Disabled, &mut preedit), None);
        assert_eq!(preedit, Preedit::default());
    }

    #[test]
    fn ime_cursor_area_uses_content_origin_and_wide_cell_size() {
        let geom = mux::HostGeom::tight(8, 16);
        let rect = prismattyc_mux::CellRect {
            col: 2,
            row: 3,
            cols: 20,
            rows: 10,
        };
        assert_eq!(ime_cursor_area(geom, rect, 64, (4, 5), 1), (56, 128, 8, 16));
        assert_eq!(
            ime_cursor_area(geom, rect, 64, (4, 5), 2),
            (56, 128, 16, 16)
        );
    }

    #[test]
    fn nonempty_preedit_blocks_palette_find_and_forwarding_gate() {
        let empty = Preedit::default();
        assert!(!ime_blocks_host_keyboard(&empty));
        let active = Preedit {
            text: "かな".into(),
            cursor: Some((0, 3)),
        };
        assert!(ime_blocks_host_keyboard(&active));
        assert_eq!(
            table_action(
                &Key::Character("p".into()),
                PhysicalKey::Code(KeyCode::KeyP),
                mods(true, true),
            ),
            Some(keybind::Action::CommandPalette)
        );
        assert!(is_find_fallback_chord(
            &Key::Character("/".into()),
            mods(true, true)
        ));
    }

    #[test]
    fn suppressed_left_release_is_one_shot_and_left_only() {
        let mut suppress = true;
        // The paired release is swallowed exactly once.
        assert!(take_suppressed_left_release(
            MouseButton::Left,
            ElementState::Released,
            &mut suppress
        ));
        assert!(!suppress);
        // A later, unrelated release must pass through to the app.
        assert!(!take_suppressed_left_release(
            MouseButton::Left,
            ElementState::Released,
            &mut suppress
        ));
        // A press never consumes the flag; non-left releases never do.
        let mut suppress = true;
        assert!(!take_suppressed_left_release(
            MouseButton::Left,
            ElementState::Pressed,
            &mut suppress
        ));
        assert!(!take_suppressed_left_release(
            MouseButton::Right,
            ElementState::Released,
            &mut suppress
        ));
        assert!(suppress, "flag survives until the paired left release");
    }

    #[test]
    fn bell_toast_chip_rect_anchors_top_right() {
        // 6-char label at 8px cells = 48px wide chip, 16px tall.
        assert_eq!(
            bell_toast_chip_rect(BELL_TOAST_LABEL, 8, 16, 10, 20, 100, 50),
            Some((10 + 100 - 48, 20, 48, 16))
        );
        // Narrow pane clamps the chip to the content width.
        assert_eq!(
            bell_toast_chip_rect(BELL_TOAST_LABEL, 8, 16, 10, 20, 40, 50),
            Some((10, 20, 40, 16))
        );
        // No room means no chip.
        assert_eq!(
            bell_toast_chip_rect(BELL_TOAST_LABEL, 8, 16, 10, 20, 0, 50),
            None
        );
        assert_eq!(
            bell_toast_chip_rect(BELL_TOAST_LABEL, 8, 16, 10, 20, 100, 0),
            None
        );
        assert_eq!(
            bell_toast_chip_rect(BELL_TOAST_LABEL, 0, 16, 10, 20, 100, 50),
            None
        );
    }

    fn rects_overlap(a: (usize, usize, usize, usize), b: (usize, usize, usize, usize)) -> bool {
        a.0 < b.0.saturating_add(b.2)
            && b.0 < a.0.saturating_add(a.2)
            && a.1 < b.1.saturating_add(b.3)
            && b.1 < a.1.saturating_add(a.3)
    }

    #[test]
    fn remote_chip_shows_stacks_and_misses_mail_and_title() {
        let owner = prismattyc_mux::SizeOwner {
            client_id: 2,
            kind: prismattyc_mux::SizeOwnerKind::Remote,
        };
        assert_eq!(
            prismattyc_mux::remote_size_chip(Some(owner), Some(1), (40, 20)),
            Some((40, 20))
        );
        let host_owner = prismattyc_mux::SizeOwner {
            client_id: 1,
            kind: prismattyc_mux::SizeOwnerKind::Host,
        };
        assert_eq!(
            prismattyc_mux::remote_size_chip(Some(host_owner), Some(1), (40, 20)),
            None,
            "echo of a clamped replica must not chip when the host owns size"
        );

        let cell_w = 8;
        let cell_h = 16;
        let content_x = 0;
        let tab_h = 20;
        let guest_y = tab_h;
        let content_w = 200;
        let guest_h = 80;
        let toast = bell_toast_chip_rect(
            BELL_TOAST_LABEL,
            cell_w,
            cell_h,
            content_x,
            guest_y,
            content_w,
            guest_h,
        )
        .expect("toast");
        let chip = bell_toast_chip_rect(
            " remote 40x20 ",
            cell_w,
            cell_h,
            content_x,
            guest_y.saturating_add(cell_h),
            content_w,
            guest_h.saturating_sub(cell_h),
        )
        .expect("chip under toast");
        assert_eq!(chip.1, toast.1.saturating_add(toast.3));
        assert!(!rects_overlap(toast, chip));

        // Mail letter is top-left of the pane content (raster.rs inset 3, 8×6).
        let mail = (content_x + 3, guest_y + 3, 8, 6);
        assert!(
            !rects_overlap(chip, mail),
            "remote chip is top-right; mail letter is top-left"
        );

        // PT-190 title notice and attention badges live in the tab strip.
        let title_notice = (content_x, 0, content_w, tab_h);
        let attention = (content_w.saturating_sub(12), 2, 6, 6);
        assert!(chip.1 >= guest_y);
        assert!(!rects_overlap(chip, title_notice));
        assert!(!rects_overlap(chip, attention));
    }

    #[test]
    fn write_fail_toast_is_not_a_bell() {
        assert!(is_write_fail_toast(attach_log::WRITE_FAILED_TOAST));
        assert!(!is_write_fail_toast(BELL_TOAST_LABEL));
        assert!(!is_write_fail_toast(" pasted image → shot.png "));
    }

    #[test]
    fn toaster_off_keeps_write_fail_chip() {
        let mut labels = vec![
            BELL_TOAST_LABEL.to_string(),
            attach_log::WRITE_FAILED_TOAST.to_string(),
            " pasted image → shot.png ".to_string(),
        ];
        labels.retain(|label| is_write_fail_toast(label));
        assert_eq!(labels.as_slice(), [attach_log::WRITE_FAILED_TOAST]);
    }

    #[test]
    fn cmd_n_requests_new_window() {
        let logo = mods_logo();
        let n = Key::Character("n".into());
        let phys = PhysicalKey::Code(KeyCode::KeyN);
        assert_eq!(
            table_action(&n, phys, logo),
            Some(keybind::Action::NewWindow)
        );
        assert_eq!(table_action(&n, phys, ModifiersState::empty()), None);
        assert_ne!(
            table_action(&n, phys, mods(true, true)),
            Some(keybind::Action::NewWindow),
            "Ctrl+Shift+N is not new-window"
        );
        let mut super_shift = mods_logo();
        super_shift.set(ModifiersState::SHIFT, true);
        assert_ne!(
            table_action(&n, phys, super_shift),
            Some(keybind::Action::NewWindow),
            "Super+Shift+N is not new-window"
        );
    }

    #[test]
    fn user_keybinding_fires_its_action_and_retires_the_default() {
        let keys = std::collections::BTreeMap::from([(
            "split_right".to_string(),
            keybind::KeysValue::One("ctrl+alt+enter".into()),
        )]);
        let map = keybind::KeyMap::from_config(Some(&keys)).unwrap();
        let mut ctrl_alt = ModifiersState::empty();
        ctrl_alt.set(ModifiersState::CONTROL, true);
        ctrl_alt.set(ModifiersState::ALT, true);
        let fired = map
            .action(
                &Key::Named(NamedKey::Enter),
                PhysicalKey::Code(KeyCode::Enter),
                ctrl_alt,
            )
            .and_then(mux_command_for);
        assert_eq!(
            fired,
            Some(MuxCommand::Split(prismattyc_mux::Axis::Horizontal))
        );
        assert_eq!(
            map.action(
                &Key::Character("|".into()),
                PhysicalKey::Code(KeyCode::Backslash),
                mods(true, true)
            ),
            None,
            "the replaced default no longer fires"
        );
        // Other defaults are intact.
        assert_eq!(
            map.action(
                &Key::Character("w".into()),
                PhysicalKey::Code(KeyCode::KeyW),
                mods(true, true)
            )
            .and_then(mux_command_for),
            Some(MuxCommand::Close)
        );
    }

    #[test]
    fn window_map_last_close_empties() {
        // Model of the routing rule: closing the last window empties the map,
        // which is the process-exit trigger used by `window_event`/`pump`.
        let mut ids: Vec<u64> = vec![1, 2];
        ids.retain(|&id| id != 1);
        assert_eq!(ids, vec![2], "closing one keeps the rest");
        ids.retain(|&id| id != 2);
        assert!(ids.is_empty(), "closing the last empties the map -> exit");
    }

    #[test]
    fn rich_focus_yields_host_chords_but_captures_plain_keys() {
        assert!(
            !rich_focus_captures(mods(true, true)),
            "C-S chords (scroll pan, copy/paste, tab nav) stay host-owned"
        );
        assert!(rich_focus_captures(mods(false, false)));
        assert!(rich_focus_captures(mods(true, false)));
        assert!(rich_focus_captures(mods(false, true)));
    }

    #[test]
    fn rich_pointer_activation_cancels_at_four_logical_pixels() {
        assert!(!rich_pointer_dragged(
            10.0,
            10.0,
            PhysicalPosition::new(13.9, 10.0),
            1.0,
        ));
        assert!(rich_pointer_dragged(
            10.0,
            10.0,
            PhysicalPosition::new(14.0, 10.0),
            1.0,
        ));
        assert!(!rich_pointer_dragged(
            10.0,
            10.0,
            PhysicalPosition::new(17.9, 10.0),
            2.0,
        ));
        assert!(rich_pointer_dragged(
            10.0,
            10.0,
            PhysicalPosition::new(18.0, 10.0),
            2.0,
        ));
    }

    #[test]
    fn one_cell_mark_does_not_claim_ctrl_c() {
        let mut selection = Selection::default();
        selection.begin(0, 0);
        selection.dragged = true;
        assert!(!selection_claims_ctrl_c(&selection));
        selection.update(0, 1);
        assert!(selection_claims_ctrl_c(&selection));
    }

    #[test]
    fn copy_chords_preserve_plain_ctrl_c_interrupt() {
        let c = Key::Character("c".into());
        assert!(!is_copy_chord(&c, mods(true, false), false));
        assert!(is_copy_chord(&c, mods(true, false), true));
        assert!(is_copy_chord(&c, mods(true, true), false));
    }

    #[test]
    fn paste_is_ctrl_shift_v_by_table_with_shift_insert_fixed() {
        let v = Key::Character("v".into());
        let bare = PhysicalKey::Code(KeyCode::KeyA);
        let phys_v = PhysicalKey::Code(KeyCode::KeyV);
        assert_eq!(
            table_action(&v, bare, mods(true, false)),
            None,
            "plain Ctrl+V is the app's"
        );
        assert_eq!(
            table_action(&v, bare, mods(true, true)),
            Some(keybind::Action::Paste)
        );
        assert_eq!(
            table_action(&Key::Character("\u{16}".into()), phys_v, mods(true, true)),
            Some(keybind::Action::Paste),
            "physical KeyV wins over control-char text"
        );
        assert!(is_paste_fallback(
            &Key::Named(NamedKey::Insert),
            PhysicalKey::Code(KeyCode::Insert),
            mods(false, true)
        ));
        assert!(!is_paste_fallback(
            &Key::Named(NamedKey::Insert),
            PhysicalKey::Code(KeyCode::Insert),
            mods(false, false)
        ));
        assert!(
            !is_paste_fallback(&v, phys_v, mods(true, true)),
            "C-S-V is the table's, not a fallback"
        );
        // The dedicated Paste key needs Shift (or Ctrl+Shift), as before.
        let paste_key = Key::Named(NamedKey::Paste);
        assert!(is_paste_fallback(&paste_key, bare, mods(false, true)));
        assert!(is_paste_fallback(&paste_key, bare, mods(true, true)));
        assert!(!is_paste_fallback(&paste_key, bare, mods(false, false)));
    }

    #[test]
    fn normalize_paste_strips_bracket_wrappers() {
        assert_eq!(normalize_paste_text("plain"), "plain");
        assert_eq!(normalize_paste_text("\x1b[200~hello\x1b[201~"), "hello");
        assert_eq!(
            normalize_paste_text("\x1b[200~\x1b[200~nested\x1b[201~\x1b[201~"),
            "nested"
        );
        let cleaned = normalize_paste_text("before\x1b[201~after");
        assert_eq!(cleaned, "beforeafter");
        assert!(!cleaned.as_bytes().windows(6).any(|w| w == b"\x1b[201~"));
    }

    #[test]
    fn paste_payload_normalizes_and_wraps_once() {
        assert_eq!(paste_payload("plain", false), b"plain");
        assert_eq!(
            paste_payload("\x1b[200~@/run/user/1000/prism-paste/a.png\x1b[201~", true),
            b"\x1b[200~@/run/user/1000/prism-paste/a.png\x1b[201~"
        );
    }

    #[test]
    fn enqueue_paste_chunks_partial_when_budget_expires() {
        let (tx, _rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let big = vec![b'x'; PASTE_CHUNK_BYTES * 3];
        let result = enqueue_paste_chunks(&tx, &big, Duration::from_millis(30));
        assert_eq!(result, PasteEnqueueResult::Partial);
    }

    #[test]
    fn cli_default_program_is_a_login_shell() {
        // No program given: start $SHELL as a login shell so ~/.zprofile runs
        // and PATH matches Terminal.app (Finder/Dock launch has minimal PATH).
        let cli = Cli::parse(std::iter::empty()).expect("parse");
        assert_eq!(cli.child_args, ["-l"]);
        let expected = prismattyc_mux::platform::default_shell();
        assert_eq!(cli.program, expected);
    }

    #[test]
    fn cli_explicit_program_is_not_forced_to_login() {
        // An explicit program keeps exactly the args the user passed.
        let cli = Cli::parse(["--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        assert_eq!(cli.program, "/bin/sh");
        assert!(cli.child_args.is_empty());
    }

    #[test]
    fn cli_separator_takes_the_following_program() {
        let cli =
            Cli::parse(["--", "/bin/bash", "-l"].into_iter().map(str::to_owned)).expect("parse");
        assert_eq!(cli.program, "/bin/bash");
        assert_eq!(cli.child_args, ["-l"]);
        assert_eq!(cli.panes, 1);
    }

    #[test]
    fn rich_focus_key_token_is_bounded_and_escaped_shape() {
        let mut mods = ModifiersState::empty();
        mods.set(ModifiersState::CONTROL, true);
        mods.set(ModifiersState::SHIFT, true);
        let token = rich_focus_key_token(&Key::Character(";".into()), mods).expect("token");
        assert_eq!(token, "C-S-;");
        assert!(token.len() <= 32);
        assert!(rich_focus_key_token(&Key::Named(NamedKey::Control), mods).is_none());
        assert_eq!(
            rich_focus_key_token(&Key::Named(NamedKey::Enter), ModifiersState::empty()).as_deref(),
            Some("Enter")
        );
    }

    #[test]
    fn experimental_rich_cli_flag_enables() {
        let previous = std::env::var_os("PRISMATTYC_EXPERIMENTAL_RICH");
        std::env::remove_var("PRISMATTYC_EXPERIMENTAL_RICH");
        let off = Cli::parse(["--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        assert!(!off.experimental_rich);
        let on = Cli::parse(
            ["--experimental-rich", "--", "/bin/sh"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("parse");
        assert!(on.experimental_rich);
        match previous {
            Some(value) => std::env::set_var("PRISMATTYC_EXPERIMENTAL_RICH", value),
            None => std::env::remove_var("PRISMATTYC_EXPERIMENTAL_RICH"),
        }
    }

    #[test]
    fn gpu_cli_flag_enables() {
        let previous = std::env::var_os("PRISMATTYC_GPU");
        std::env::remove_var("PRISMATTYC_GPU");
        let off = Cli::parse(["--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        assert!(!off.gpu);
        let on =
            Cli::parse(["--gpu", "--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        assert!(on.gpu);
        match previous {
            Some(value) => std::env::set_var("PRISMATTYC_GPU", value),
            None => std::env::remove_var("PRISMATTYC_GPU"),
        }
    }

    #[test]
    fn cli_accepts_bounded_initial_pane_count() {
        let cli = Cli::parse(
            ["--panes", "3", "--", "/bin/sh", "-l"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("parse");
        assert_eq!(cli.panes, 3);
        assert_eq!(cli.program, "/bin/sh");
        assert_eq!(cli.child_args, ["-l"]);

        let error = Cli::parse(["--panes", "0"].into_iter().map(str::to_owned))
            .err()
            .expect("zero panes rejected");
        assert!(error.to_string().contains("1 through 8"));
    }

    #[test]
    fn placeholder_reopen_plan_attaches_recreates_or_keeps_gone() {
        let live = vec![("12".into(), "work".into())];
        let space = vec!["work".into(), "mail".into()];
        assert_eq!(
            plan_placeholder_reopen("12", "work", &live, &space),
            PlaceholderReopen::Attach { id: "12".into() }
        );
        assert_eq!(
            plan_placeholder_reopen("99", "work", &live, &space),
            PlaceholderReopen::Attach { id: "12".into() }
        );
        assert_eq!(
            plan_placeholder_reopen("99", "mail", &live, &space),
            PlaceholderReopen::Recreate {
                name: "mail".into()
            }
        );
        assert_eq!(
            plan_placeholder_reopen("99", "gone", &live, &space),
            PlaceholderReopen::Gone {
                name: "gone".into()
            }
        );
    }

    #[test]
    fn parse_live_sessions_reads_ls_tree_ids() {
        let stdout = "\
session work (id 12)
  window 1 \"tab\" — 80x24
    pane 3 alive (pid 9), rev 1
session mail (id 15)
";
        assert_eq!(
            parse_live_sessions(stdout),
            vec![("12".into(), "work".into()), ("15".into(), "mail".into())]
        );
    }

    #[test]
    fn restore_from_space_session_uses_agent_and_leaf_cwd() {
        let session = SavedSpaceSession {
            name: "mail".into(),
            agent: Some("kiro-pc".into()),
            windows: vec![prismattyc_mux::SavedWindow {
                title: "main".into(),
                cols: 80,
                rows: 24,
                root: prismattyc_mux::SavedNode::Leaf {
                    cwd: Some("/tmp/mail".into()),
                    program: None,
                    command: None,
                    title: None,
                },
            }],
        };
        let restore = restore_from_space_session(&session);
        assert_eq!(restore.agent.as_deref(), Some("kiro-pc"));
        assert_eq!(
            restore.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/mail"))
        );
        assert_eq!(
            pmux_new_args("mail", &restore),
            ["new", "--no-attach", "--agent", "kiro-pc", "mail"]
        );
    }

    #[test]
    fn cli_parses_attach_sessions() {
        let cli = Cli::parse(
            [
                "--attach-session",
                "2",
                "--attach-title",
                "alpha-pm",
                "--attach-session",
                "a,b",
                "--attach-title",
                "comma",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("parse");
        assert_eq!(
            cli.attach_sessions,
            [
                AttachTarget {
                    session: "2".into(),
                    title: "alpha-pm".into(),
                },
                AttachTarget {
                    session: "a,b".into(),
                    title: "comma".into(),
                },
            ]
        );
    }

    #[test]
    fn startup_attach_plan_routes_owner_and_targets_table() {
        let cases = [
            (
                "unregistered bare host",
                false,
                false,
                StartupAttachPlan::Bare,
            ),
            (
                "unregistered explicit targets",
                false,
                true,
                StartupAttachPlan::ExplicitOneTabPerTarget,
            ),
            ("registered bare host", true, false, StartupAttachPlan::Bare),
            (
                "registered explicit targets",
                true,
                true,
                StartupAttachPlan::ExplicitWithCache,
            ),
        ];
        for (name, registered_owner, has_explicit_targets, expected) in cases {
            assert_eq!(
                startup_attach_plan(registered_owner, has_explicit_targets),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn in_process_new_window_stays_bare_while_sibling_owns_cache() {
        let cli_targets = [AttachTarget {
            session: "sibling-session".into(),
            title: "sibling".into(),
        }];
        let attach_targets = startup_attach_targets(false, &cli_targets);
        let plan = startup_window_plan(false, true, !attach_targets.is_empty());

        assert!(attach_targets.is_empty());
        assert_eq!(plan.attach, StartupAttachPlan::Bare);
        assert!(!plan.cache_writer);
    }

    #[test]
    fn first_window_keeps_cli_targets_and_owner_cache_policy() {
        let cli_targets = [AttachTarget {
            session: "initial-session".into(),
            title: "initial".into(),
        }];
        let attach_targets = startup_attach_targets(true, &cli_targets);
        let plan = startup_window_plan(true, true, !attach_targets.is_empty());

        assert_eq!(attach_targets, cli_targets);
        assert_eq!(plan.attach, StartupAttachPlan::ExplicitWithCache);
        assert!(plan.cache_writer);
    }

    #[test]
    fn unregistered_new_window_must_not_write_shared_cache() {
        assert!(may_write_shared_cache(true));
        assert!(!may_write_shared_cache(false));
    }

    #[test]
    fn attach_spawn_args_yield_session_id() {
        assert_eq!(
            session_id_from_attach_spawn(
                "pmux",
                &["attach".into(), "--session-id".into(), "6".into()]
            )
            .as_deref(),
            Some("6")
        );
        assert_eq!(
            session_id_from_attach_spawn("/bin/sh", &[]).as_deref(),
            None
        );
        assert_eq!(
            session_id_from_attach_spawn("pmux-attach", &["--session".into(), "astra-pc".into()])
                .as_deref(),
            Some("astra-pc")
        );
    }

    fn attach_name_catalog(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, name)| ((*id).to_string(), (*name).to_string()))
            .collect()
    }

    fn attach_argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn resolve_attach_session_key_matches_id_or_name() {
        let catalog = attach_name_catalog(&[("7", "astra-pc"), ("8", "other")]);
        let cases = [
            ("7", "7", "astra-pc"),
            ("astra-pc", "7", "astra-pc"),
            ("8", "8", "other"),
            ("other", "8", "other"),
            ("missing", "missing", "missing"),
        ];
        for (key, id, name) in cases {
            assert_eq!(
                resolve_attach_session_key(key, &catalog),
                (id.to_string(), name.to_string()),
                "key={key}"
            );
        }
        assert_eq!(
            resolve_attach_session_key("7", &HashMap::new()),
            ("7".into(), "7".into())
        );
        assert_eq!(
            resolve_attach_session_key("", &catalog),
            (String::new(), String::new())
        );
    }

    #[test]
    fn spawned_attach_registration_table() {
        let catalog = attach_name_catalog(&[("7", "astra-pc")]);
        let check = |program: &str, argv: &[&str], expected: Option<(&str, &str)>| {
            assert_eq!(
                spawned_attach_registration(program, &attach_argv(argv), &catalog),
                expected.map(|(id, name)| (id.to_string(), name.to_string())),
                "{program} {argv:?}"
            );
        };
        check(
            "pmux",
            &["attach", "--session-id", "7"],
            Some(("7", "astra-pc")),
        );
        check(
            "pmux",
            &["attach", "--session", "astra-pc"],
            Some(("7", "astra-pc")),
        );
        check("pmux", &["attach", "astra-pc"], Some(("7", "astra-pc")));
        check(
            "pmux-attach",
            &["--session", "astra-pc"],
            Some(("7", "astra-pc")),
        );
        check(
            "pmux-attach",
            &["--session-id", "7"],
            Some(("7", "astra-pc")),
        );
        check("/bin/sh", &[], None);
        check("pmux", &["ls"], None);
        check("pmux-attach", &["--json", "--session", "astra-pc"], None);
        check("pmux", &["attach", "--all"], None);
        assert_eq!(
            spawned_attach_registration(
                "pmux-attach",
                &attach_argv(&["--session", "astra-pc"]),
                &HashMap::new()
            ),
            Some(("astra-pc".into(), "astra-pc".into()))
        );
    }

    #[test]
    fn attach_without_layout_gives_one_tab_per_session() {
        let sessions = [
            AttachTarget {
                session: "1".into(),
                title: "default".into(),
            },
            AttachTarget {
                session: "2".into(),
                title: "alpha-pm".into(),
            },
            AttachTarget {
                session: "3".into(),
                title: "beta-pm".into(),
            },
        ];
        let grouped = attach_tabs::group_attach_targets(&sessions, None);
        let groups = grouped.groups;
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, "default");
        assert_eq!(groups[1].0, "alpha-pm");
        assert_eq!(groups[2].0, "beta-pm");
        assert!(groups.iter().all(|(_, members)| members.len() == 1));
    }

    #[test]
    fn keyboard_selection_stays_inside_visible_view() {
        let screen = Screen::new(4, 2, 8);
        let mut selection = Selection::default();
        assert!(extend_selection_keyboard(
            &mut selection,
            &screen,
            0,
            NamedKey::ArrowDown
        ));
        let range = selection.range().expect("paintable range");
        assert!(range.start_row >= screen.abs_row_at_view(0, 0));
        assert!(range.end_row <= screen.abs_row_at_view(0, screen.rows() - 1));
    }

    #[test]
    fn native_copy_eligibility_matches_classic_d_h4() {
        let mut screen = Screen::new(8, 1, 0);
        for ch in "copy me".chars() {
            screen.put_char(ch);
        }
        let mut selection = Selection::default();
        selection.set_range(0, 0, 0, 6);
        assert_eq!(
            selected_clipboard_text(&selection, &screen).as_deref(),
            Some("copy me")
        );

        let blank = Screen::new(4, 1, 0);
        selection.set_range(0, 0, 0, 3);
        assert!(selected_clipboard_text(&selection, &blank).is_none());
    }

    #[test]
    fn rename_strokes_edit_commit_and_cancel() {
        let mut buf = String::from("tab");
        let mut selected = false;
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Backspace),
            None
        );
        assert_eq!(buf, "ta");
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Insert('x')),
            None
        );
        assert_eq!(buf, "tax");
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Commit),
            Some(true)
        );
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Cancel),
            Some(false)
        );
    }

    #[test]
    fn rename_select_all_on_open_type_replaces_enter_keeps_backspace_clears() {
        let mut buf = String::from("old");
        let mut selected = true;
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Insert('x')),
            None
        );
        assert_eq!(buf, "x");
        assert!(!selected);

        let mut buf = String::from("old");
        let mut selected = true;
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Commit),
            Some(true)
        );
        assert_eq!(buf, "old");

        let mut buf = String::from("old");
        let mut selected = true;
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Backspace),
            None
        );
        assert_eq!(buf, "");
        assert!(!selected);
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Commit),
            Some(true)
        );

        let mut buf = String::from("old");
        let mut selected = true;
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::DropSelection),
            None
        );
        assert!(!selected);
        assert_eq!(
            apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Insert('x')),
            None
        );
        assert_eq!(buf, "oldx");
    }

    #[test]
    fn rename_named_space_inserts_spaces() {
        assert!(matches!(
            rename_stroke_from_logical(&Key::Named(NamedKey::Space)),
            Some(RenameStroke::Insert(' '))
        ));
        let mut buf = String::new();
        let mut selected = false;
        for ch in "x y z".chars() {
            assert_eq!(
                apply_rename_stroke(&mut buf, &mut selected, RenameStroke::Insert(ch)),
                None
            );
        }
        assert_eq!(buf, "x y z");
    }

    #[test]
    fn chord_help_lists_detach() {
        let runtime = mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let help = chord_help_text(&runtime, &keybind::KeyMap::default(), true);
        assert!(
            help.contains("C-S-X detach"),
            "footer must name detach: {help}"
        );
        // Defaults render the same strip as before user keybindings.
        assert!(help.contains("C-S-, themes | C-S-V paste | C-S-C copy | C-S-\\/E split> | C-S--/D splitv | C-S-Fn/C-A-n even | C-S-W close | C-S-X detach | C-S-[/] color | Alt+arrow"), "{help}");
        // A rebinding shows up in the strip; an unbinding drops the segment.
        let keys = std::collections::BTreeMap::from([
            (
                "detach".to_string(),
                keybind::KeysValue::One("ctrl+alt+d".into()),
            ),
            ("theme_picker".to_string(), keybind::KeysValue::Many(vec![])),
        ]);
        let custom = chord_help_text(
            &runtime,
            &keybind::KeyMap::from_config(Some(&keys)).unwrap(),
            true,
        );
        assert!(
            custom.contains("C-A-D detach") && !custom.contains("themes"),
            "{custom}"
        );
        // Rebinding one layout stops the "Fn/n" summary from speaking for all.
        let keys = std::collections::BTreeMap::from([(
            "layout_2".to_string(),
            keybind::KeysValue::One("ctrl+alt+z".into()),
        )]);
        let odd = chord_help_text(
            &runtime,
            &keybind::KeyMap::from_config(Some(&keys)).unwrap(),
            true,
        );
        assert!(
            odd.contains("C-A-Z\u{2026} even") && !odd.contains("Fn"),
            "{odd}"
        );
        assert!(layouts_share_pattern(&keybind::KeyMap::default()));
    }

    #[test]
    fn mux_chords_are_direct_and_do_not_require_a_leader() {
        let bare = PhysicalKey::Code(KeyCode::KeyA);
        assert_eq!(
            mux_command(&Key::Character("|".into()), bare, mods(true, true)),
            Some(MuxCommand::Split(prismattyc_mux::Axis::Horizontal))
        );
        assert_eq!(
            mux_command(&Key::Character("_".into()), bare, mods(true, true)),
            Some(MuxCommand::Split(prismattyc_mux::Axis::Vertical))
        );
        assert_eq!(
            mux_command(&Key::Character("w".into()), bare, mods(true, true)),
            Some(MuxCommand::Close)
        );
        assert_eq!(
            mux_command(&Key::Character("x".into()), bare, mods(true, true)),
            Some(MuxCommand::Detach)
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::KeyX),
                mods(true, true)
            ),
            Some(MuxCommand::Detach)
        );
        assert_eq!(
            mux_command(&Key::Character("w".into()), bare, mods(true, false)),
            None
        );
        assert_eq!(
            mux_command(
                &Key::Character("r".into()),
                PhysicalKey::Code(KeyCode::KeyR),
                mods(true, true)
            ),
            Some(MuxCommand::RenameTab)
        );
        // Physical scancodes win even when logical text is empty/odd.
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::Backslash),
                mods(true, true)
            ),
            Some(MuxCommand::Split(prismattyc_mux::Axis::Horizontal))
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::Minus),
                mods(true, true)
            ),
            Some(MuxCommand::Split(prismattyc_mux::Axis::Vertical))
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::KeyE),
                mods(true, true)
            ),
            Some(MuxCommand::Split(prismattyc_mux::Axis::Horizontal))
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::KeyD),
                mods(true, true)
            ),
            Some(MuxCommand::Split(prismattyc_mux::Axis::Vertical))
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::BracketRight),
                mods(true, true)
            ),
            Some(MuxCommand::CycleFocusBorder)
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::BracketLeft),
                mods(true, true)
            ),
            Some(MuxCommand::CycleFocusBorderBack)
        );

        let mut alt = ModifiersState::empty();
        alt.set(ModifiersState::ALT, true);
        assert_eq!(
            mux_command(&Key::Named(NamedKey::ArrowLeft), bare, alt),
            Some(MuxCommand::Focus(mux::FocusDirection::Left))
        );
        assert_eq!(
            mux_command(
                &Key::Character("t".into()),
                PhysicalKey::Code(KeyCode::KeyT),
                mods(true, true)
            ),
            Some(MuxCommand::NewTab)
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::PageDown),
                mods(true, true)
            ),
            Some(MuxCommand::NextTab)
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::PageUp),
                mods(true, true)
            ),
            Some(MuxCommand::PrevTab)
        );
        assert_eq!(
            mux_command(
                &Key::Character("1".into()),
                PhysicalKey::Code(KeyCode::Digit1),
                mods(true, true)
            ),
            Some(MuxCommand::SelectTab(0))
        );
        assert_eq!(
            mux_command(
                &Key::Character("3".into()),
                PhysicalKey::Code(KeyCode::Digit3),
                mods(true, true)
            ),
            Some(MuxCommand::SelectTab(2))
        );
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F3),
                PhysicalKey::Code(KeyCode::F3),
                mods(true, true)
            ),
            Some(MuxCommand::EvenColumns(3))
        );
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F2),
                PhysicalKey::Code(KeyCode::F2),
                mods(true, true)
            ),
            Some(MuxCommand::EvenColumns(2))
        );
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F4),
                PhysicalKey::Code(KeyCode::F4),
                mods(true, true)
            ),
            Some(MuxCommand::EvenQuadrants)
        );
        let mut move_mods = mods(true, true);
        move_mods.set(ModifiersState::ALT, true);
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F3),
                PhysicalKey::Code(KeyCode::F3),
                move_mods
            ),
            Some(MuxCommand::EvenColumns(3))
        );
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F2),
                PhysicalKey::Code(KeyCode::F2),
                move_mods
            ),
            Some(MuxCommand::EvenColumns(2))
        );
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F4),
                PhysicalKey::Code(KeyCode::F4),
                move_mods
            ),
            Some(MuxCommand::EvenQuadrants)
        );
        assert_eq!(
            mux_command(
                &Key::Character("".into()),
                PhysicalKey::Code(KeyCode::PageDown),
                move_mods
            ),
            Some(MuxCommand::MovePaneToTab(1))
        );

        let unidentified = PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified);
        assert_eq!(
            mux_command(&Key::Named(NamedKey::F2), unidentified, mods(true, true)),
            Some(MuxCommand::EvenColumns(2)),
            "macOS Fn often reports NamedKey with Unidentified physical"
        );
        assert_eq!(
            mux_command(&Key::Named(NamedKey::F4), unidentified, mods(true, true)),
            Some(MuxCommand::EvenQuadrants)
        );

        let mut super_shift = ModifiersState::empty();
        super_shift.set(ModifiersState::SUPER, true);
        super_shift.set(ModifiersState::SHIFT, true);
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F2),
                PhysicalKey::Code(KeyCode::F2),
                super_shift
            ),
            Some(MuxCommand::EvenColumns(2)),
            "Cmd+Shift+Fn is the macOS chord that the system does not steal"
        );
        assert_eq!(
            mux_command(&Key::Named(NamedKey::F3), unidentified, super_shift),
            Some(MuxCommand::EvenColumns(3))
        );
        assert_eq!(
            mux_command(
                &Key::Named(NamedKey::F4),
                PhysicalKey::Code(KeyCode::F4),
                super_shift
            ),
            Some(MuxCommand::EvenQuadrants)
        );

        let mut ctrl_alt = ModifiersState::empty();
        ctrl_alt.set(ModifiersState::CONTROL, true);
        ctrl_alt.set(ModifiersState::ALT, true);
        assert_eq!(
            mux_command(
                &Key::Character("2".into()),
                PhysicalKey::Code(KeyCode::Digit2),
                ctrl_alt
            ),
            Some(MuxCommand::EvenColumns(2)),
            "Ctrl+Alt+digit is the no-Fn macOS layout chord"
        );
        assert_eq!(
            mux_command(
                &Key::Character("4".into()),
                PhysicalKey::Code(KeyCode::Digit4),
                ctrl_alt
            ),
            Some(MuxCommand::EvenQuadrants)
        );
        assert_eq!(
            mux_command(
                &Key::Character("2".into()),
                PhysicalKey::Code(KeyCode::Digit2),
                mods(true, true)
            ),
            Some(MuxCommand::SelectTab(1)),
            "Ctrl+Shift+Digit2 stays SelectTab, not EvenColumns"
        );

        assert_eq!(
            mux_command_for(keybind::Action::PresetSingle),
            Some(MuxCommand::Preset(mux::LayoutPreset::Single))
        );
        assert_eq!(
            mux_command_for(keybind::Action::PresetSplitH),
            Some(MuxCommand::Preset(mux::LayoutPreset::SplitH))
        );
        assert_eq!(
            mux_command_for(keybind::Action::PresetSplitV),
            Some(MuxCommand::Preset(mux::LayoutPreset::SplitV))
        );
        assert_eq!(
            mux_command_for(keybind::Action::PresetGrid),
            Some(MuxCommand::Preset(mux::LayoutPreset::Grid))
        );
        assert_eq!(
            mux_command_for(keybind::Action::PresetMainVertical),
            Some(MuxCommand::Preset(mux::LayoutPreset::MainVertical))
        );
        assert_eq!(
            mux_command_for(keybind::Action::PresetMainHorizontal),
            Some(MuxCommand::Preset(mux::LayoutPreset::MainHorizontal))
        );
    }

    #[test]
    fn focus_border_cli_and_env_parse() {
        let cli = Cli::parse(
            ["--focus-border", "violet", "--", "/bin/sh"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("parse");
        assert_eq!(focus_border_name(cli.focus_border), "violet");
    }

    #[test]
    fn theme_picker_chord_is_host_owned_and_theme_rows_wrap() {
        let chord = mods(true, true);
        assert_eq!(
            table_action(
                &Key::Character("<".into()),
                PhysicalKey::Code(KeyCode::Comma),
                chord
            ),
            Some(keybind::Action::ThemePicker)
        );
        assert_eq!(
            mux_command(
                &Key::Character("<".into()),
                PhysicalKey::Code(KeyCode::Comma),
                chord
            ),
            None,
            "theme settings must not collide with a mux command"
        );
        assert_eq!(cycle_theme_index(None, 6, true), Some(0));
        assert_eq!(cycle_theme_index(None, 6, false), Some(5));
        assert_eq!(cycle_theme_index(Some(5), 6, true), Some(0));
        assert_eq!(cycle_theme_index(Some(0), 6, false), Some(5));
    }

    #[test]
    fn command_palette_chord_is_host_owned() {
        let chord = mods(true, true);
        assert_eq!(
            table_action(
                &Key::Character("p".into()),
                PhysicalKey::Code(KeyCode::KeyP),
                chord
            ),
            Some(keybind::Action::CommandPalette)
        );
        assert_eq!(
            mux_command(
                &Key::Character("p".into()),
                PhysicalKey::Code(KeyCode::KeyP),
                chord
            ),
            None,
            "command palette must not collide with a mux command"
        );
    }

    #[test]
    fn find_chord_is_ctrl_shift_f_and_punctuation() {
        let chord = mods(true, true);
        let bare = PhysicalKey::Code(KeyCode::KeyA);
        assert_eq!(
            table_action(&Key::Character("f".into()), bare, chord),
            Some(keybind::Action::Find)
        );
        assert_eq!(
            table_action(&Key::Character("F".into()), bare, chord),
            Some(keybind::Action::Find)
        );
        assert!(is_find_fallback_chord(&Key::Character(";".into()), chord));
        assert!(is_find_fallback_chord(&Key::Character("'".into()), chord));
        assert!(is_find_fallback_chord(&Key::Character(".".into()), chord));
        assert!(
            !is_find_fallback_chord(&Key::Character("f".into()), chord),
            "F is the table's"
        );
        assert_eq!(
            table_action(&Key::Character("f".into()), bare, mods(true, false)),
            None
        );
        assert!(!is_scroll_slash_find(
            &Key::Character("/".into()),
            ModifiersState::empty(),
            0
        ));
        assert!(is_scroll_slash_find(
            &Key::Character("/".into()),
            ModifiersState::empty(),
            3
        ));
        assert_eq!(
            mux_command(
                &Key::Character("f".into()),
                PhysicalKey::Code(KeyCode::KeyF),
                chord
            ),
            None,
            "find chord must not collide with a mux command"
        );
    }

    #[test]
    fn find_step_selects_first_match_and_rank() {
        let mut emulator = Emulator::new(40, 4, 50);
        let _ = emulator.feed(b"alpha\nbeta unique_token gamma\nalpha\n");
        let mut find = FindMode {
            active: true,
            query: "unique_token".into(),
            last: None,
            rank: None,
        };
        let mut selection = Selection::default();
        let mut view_scroll = 0usize;
        apply_find_step(
            &mut find,
            &mut selection,
            &mut view_scroll,
            &emulator,
            false,
        );
        assert!(selection.range().is_some());
        assert_eq!(find.rank, Some((1, 1)));
        find.query = "no_such".into();
        find.last = None;
        apply_find_step(
            &mut find,
            &mut selection,
            &mut view_scroll,
            &emulator,
            false,
        );
        assert!(selection.range().is_none());
        assert!(find.rank.is_none());
    }

    #[test]
    fn find_prompt_label_shows_rank_or_miss() {
        assert_eq!(find_prompt_label("", None), " Find: █ ");
        assert_eq!(find_prompt_label("ab", Some((1, 2))), " Find: ab█ 1/2 ");
        assert_eq!(find_prompt_label("zz", None), " Find: zz█ 0/0 ");
    }

    #[test]
    fn config_file_yields_to_cli_pins() {
        let file = config::ConfigFile {
            focus_border: Some("coral".into()),
            panes: Some(3),
            ..Default::default()
        };

        let mut pinned = Cli::parse(
            ["--focus-border", "violet", "--panes", "2", "--", "/bin/sh"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("parse");
        pinned.apply_config(&file);
        assert_eq!(focus_border_name(pinned.focus_border), "violet");
        assert_eq!(pinned.panes, 2);

        let mut unpinned =
            Cli::parse(["--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        pinned_env_guard(&mut unpinned);
        unpinned.apply_config(&file);
        assert_eq!(focus_border_name(unpinned.focus_border), "coral");
        assert_eq!(unpinned.panes, 3);

        let mut bad = Cli::parse(["--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        pinned_env_guard(&mut bad);
        bad.apply_config(&config::ConfigFile {
            focus_border: Some("nope".into()),
            ..Default::default()
        });
        assert_eq!(bad.focus_border, DEFAULT_FOCUS_BORDER_INDEX);
    }

    #[test]
    fn idle_control_flow_is_wait_not_poll() {
        let now = Instant::now();
        assert_eq!(
            next_control_flow(now, None, DEFAULT_LIGHT_CYCLE_MS, false, now, None),
            ControlFlow::Wait
        );
        // A lit bell flash arms WaitUntil even on an otherwise idle host.
        let flash_end = now + Duration::from_millis(BELL_FLASH_MS as u64);
        assert_eq!(
            next_control_flow(
                now,
                None,
                DEFAULT_LIGHT_CYCLE_MS,
                false,
                now,
                Some(flash_end)
            ),
            ControlFlow::WaitUntil(flash_end)
        );
        // Expired sweep must not keep arming WaitUntil.
        let started = now
            .checked_sub(Duration::from_millis(DEFAULT_LIGHT_CYCLE_MS as u64 + 50))
            .expect("epoch");
        assert_eq!(
            next_control_flow(now, Some(started), DEFAULT_LIGHT_CYCLE_MS, false, now, None),
            ControlFlow::Wait
        );
    }

    #[test]
    fn earliest_picks_the_sooner_deadline() {
        let now = Instant::now();
        let later = now + Duration::from_millis(500);
        assert_eq!(earliest(None, None), None);
        assert_eq!(earliest(Some(now), None), Some(now));
        assert_eq!(earliest(None, Some(later)), Some(later));
        assert_eq!(earliest(Some(later), Some(now)), Some(now));
    }

    #[test]
    fn light_cycle_and_pulse_arm_wait_until_then_settle() {
        let now = Instant::now();
        match next_control_flow(now, Some(now), DEFAULT_LIGHT_CYCLE_MS, false, now, None) {
            ControlFlow::WaitUntil(when) => {
                assert!(when > now, "deadline must be in the future");
                assert!(
                    when <= now + Duration::from_millis(DEFAULT_LIGHT_CYCLE_MS as u64),
                    "deadline must fall inside the sweep"
                );
            }
            other => panic!("expected WaitUntil during sweep, got {other:?}"),
        }
        match next_control_flow(now, None, DEFAULT_LIGHT_CYCLE_MS, true, now, None) {
            ControlFlow::WaitUntil(when) => {
                assert!(when > now);
                assert!(when <= now + Duration::from_millis(PULSE_PERIOD_MS as u64));
            }
            other => panic!("expected WaitUntil while pulse is active, got {other:?}"),
        }
    }

    #[test]
    fn pane_alpha_scales_the_whole_surface_only_with_an_alpha_visual() {
        assert_eq!(pane_alpha(242, 0.95, true), 230);
        assert_eq!(pane_alpha(242, 0.93, true), 225);
        assert_eq!(pane_alpha(242, 0.50, false), OPAQUE_ALPHA);
    }

    #[test]
    fn light_cycle_is_config_opt_in_and_reversible() {
        let mut cli = Cli::parse(["--", "/bin/sh"].into_iter().map(str::to_owned)).expect("parse");
        assert!(!cli.light_cycle, "animation must default off");
        assert_eq!(cli.light_cycle_ms, DEFAULT_LIGHT_CYCLE_MS);
        assert!(cli.light_cycle_head, "vehicle head defaults on");
        cli.apply_config(&config::ConfigFile {
            focus_border_animation: Some("light-cycle".into()),
            focus_border_animation_ms: Some(600),
            focus_border_animation_head: Some(false),
            ..Default::default()
        });
        assert!(cli.light_cycle);
        assert_eq!(cli.light_cycle_ms, 600);
        assert!(!cli.light_cycle_head);
        // Explicit "none" and a removed key both turn it back off.
        cli.apply_config(&config::ConfigFile {
            focus_border_animation: Some("none".into()),
            ..Default::default()
        });
        assert!(!cli.light_cycle);
        cli.apply_config(&config::ConfigFile::default());
        assert!(!cli.light_cycle);
        // Removed keys restore the speed/head defaults too.
        assert_eq!(cli.light_cycle_ms, DEFAULT_LIGHT_CYCLE_MS);
        assert!(cli.light_cycle_head);
    }

    /// CI may export PRISMATTYC_FOCUS_BORDER; force the unpinned baseline the test needs.
    fn pinned_env_guard(cli: &mut Cli) {
        cli.focus_border = DEFAULT_FOCUS_BORDER_INDEX;
        cli.focus_border_pinned = false;
    }

    /// Leftover pixels the cell grid cannot fill are split between both
    /// edges, so the frame is even left/right and top/bottom rather than
    /// piling every spare pixel on the right and bottom.
    #[test]
    fn refit_centres_the_grid_within_the_window() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let spacing = PaneSpacing {
            window_padding_px: 3,
            pane_gap_px: 3,
            pane_padding_px: 5,
            space_rail: space_rail::RailSide::Off,
            space_rail_chip_cols: 16,
            space_rail_width_cols: 18,
            space_rail_pane_names: false,
        };
        let mut geom = host_geom(&font, false, false, false, spacing, 0);
        // Deliberately awkward: not a whole number of cells in either axis.
        let physical = PhysicalSize::new(1299, 1021);
        let (cols, rows) = size_to_cells(physical, &font, geom);
        let trailing_gap = geom.pane_gap - geom.pane_gap / 2;
        let left = geom.window_pad + geom.chrome_left() + geom.pane_gap / 2;
        let right = (physical.width as usize)
            .saturating_sub(geom.window_pad + geom.chrome_left() + cols * font.cell_w)
            .saturating_sub(geom.chrome_right())
            .saturating_add(trailing_gap);
        geom.slack_x = right.saturating_sub(left) / 2;
        let top = geom.window_pad + geom.chrome_top() + geom.pane_gap / 2;
        let bottom = (physical.height as usize)
            .saturating_sub(geom.window_pad + geom.chrome_top() + rows * font.cell_h)
            .saturating_sub(geom.chrome_bottom())
            .saturating_add(trailing_gap);
        geom.slack_y = bottom.saturating_sub(top) / 2;

        let rect = prismattyc_mux::CellRect {
            col: 0,
            row: 0,
            cols,
            rows,
        };
        let (x, y, w, h) = geom.pane_slot_px(rect);
        let margin_left = x;
        let margin_right = (physical.width as usize).saturating_sub(x + w);
        let margin_top = y;
        let margin_bottom = (physical.height as usize).saturating_sub(y + h);
        assert!(
            margin_left.abs_diff(margin_right) <= 1,
            "left {margin_left} vs right {margin_right}"
        );
        assert!(
            margin_top.abs_diff(margin_bottom) <= 1,
            "top {margin_top} vs bottom {margin_bottom}"
        );
    }

    #[test]
    fn tab_strip_modes_control_single_tab_chrome_and_rename_gate() {
        assert!(tab_strip_visible(config::TabStripMode::Auto, 1, false));
        assert!(tab_strip_visible(config::TabStripMode::Always, 1, false));
        assert!(!tab_strip_visible(config::TabStripMode::Multi, 1, false));
        assert!(tab_strip_visible(config::TabStripMode::Auto, 1, true));
        assert!(tab_strip_visible(config::TabStripMode::Multi, 2, false));

        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let spacing = PaneSpacing {
            window_padding_px: 3,
            pane_gap_px: 3,
            pane_padding_px: 5,
            space_rail: space_rail::RailSide::Off,
            space_rail_chip_cols: 16,
            space_rail_width_cols: 18,
            space_rail_pane_names: false,
        };
        let auto = host_geom(
            &font,
            false,
            tab_strip_visible(config::TabStripMode::Auto, 1, false),
            false,
            spacing,
            0,
        );
        let always = host_geom(
            &font,
            false,
            tab_strip_visible(config::TabStripMode::Always, 1, false),
            false,
            spacing,
            0,
        );
        assert_eq!(auto.top_chrome_px, font.cell_h);
        assert_eq!(always.top_chrome_px, font.cell_h);
    }

    #[test]
    fn spaces_rail_reserves_its_edge_and_shrinks_the_grid() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let base = PaneSpacing {
            window_padding_px: 5,
            pane_gap_px: 5,
            pane_padding_px: 5,
            space_rail: space_rail::RailSide::Off,
            space_rail_chip_cols: 16,
            space_rail_width_cols: 18,
            space_rail_pane_names: false,
        };
        let off = host_geom(&font, false, false, false, base, 0);
        assert_eq!(off.rail_px, 0);
        let size = initial_window_size(&font, off, 80, 24);
        let (cols_off, rows_off) = size_to_cells(size, &font, off);
        let rect = |cols, rows| prismattyc_mux::CellRect {
            col: 0,
            row: 0,
            cols,
            rows,
        };
        for side in [
            space_rail::RailSide::Bottom,
            space_rail::RailSide::Top,
            space_rail::RailSide::Left,
            space_rail::RailSide::Right,
        ] {
            let spacing = PaneSpacing {
                space_rail: side,
                ..base
            };
            let geom = host_geom(&font, false, false, false, spacing, 0);
            let horizontal = side.horizontal();
            // Side rails reserve the configured width independently of names.
            let expect_px = if horizontal {
                font.cell_h
            } else {
                font.cell_w * spacing.space_rail_width_cols
            };
            assert_eq!(geom.rail_px, expect_px, "{side:?}");
            // The same window holds fewer rows (or columns) with the rail on.
            let (cols, rows) = size_to_cells(size, &font, geom);
            if horizontal {
                assert_eq!(cols, cols_off, "{side:?}");
                assert!(rows < rows_off, "{side:?}: rows {rows} vs {rows_off}");
            } else {
                assert_eq!(rows, rows_off, "{side:?}");
                assert!(cols < cols_off, "{side:?}: cols {cols} vs {cols_off}");
            }
            // The pane slot never overlaps the rail box.
            let layout = space_rail::RailLayout::for_window(
                geom,
                size.width as usize,
                size.height as usize,
                &[],
            )
            .expect("rail fits");
            let (x, y, w, h) = geom.pane_slot_px(rect(cols, rows));
            match side {
                space_rail::RailSide::Bottom => assert!(y + h <= layout.y, "{side:?}"),
                space_rail::RailSide::Top => assert!(y >= layout.y + layout.h, "{side:?}"),
                space_rail::RailSide::Left => assert!(x >= layout.x + layout.w, "{side:?}"),
                space_rail::RailSide::Right => assert!(x + w <= layout.x, "{side:?}"),
                space_rail::RailSide::Off => unreachable!(),
            }
            // A window sized for 80x24 with the rail on still yields 80x24.
            let sized = initial_window_size(&font, geom, 80, 24);
            let (c, r) = size_to_cells(sized, &font, geom);
            assert_eq!(geom.content_cells(rect(c, r)), (80, 24), "{side:?}");
        }
        // A top rail sits above the tab strip.
        let spacing = PaneSpacing {
            space_rail: space_rail::RailSide::Top,
            ..base
        };
        let tabbed = host_geom(&font, false, true, false, spacing, 0);
        let layout = space_rail::RailLayout::for_window(tabbed, 800, 600, &[]).unwrap();
        assert_eq!(layout.y, 0);
        assert_eq!(tabbed.tab_strip_y(), layout.h);
        assert_eq!(tabbed.chrome_top(), tabbed.top_chrome_px + font.cell_h);
    }

    #[test]
    fn grid_size_uses_full_window_for_terminal_default() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let spacing = PaneSpacing {
            window_padding_px: 5,
            pane_gap_px: 5,
            pane_padding_px: 5,
            space_rail: space_rail::RailSide::Off,
            space_rail_chip_cols: 16,
            space_rail_width_cols: 18,
            space_rail_pane_names: false,
        };
        let geom = host_geom(&font, false, false, false, spacing, 0);
        assert_eq!(geom.scrollbar_gutter_px, mux::SCROLLBAR_GUTTER_PX);
        let size = initial_window_size(&font, geom, 80, 24);
        let (cols, rows) = size_to_cells(size, &font, geom);
        assert_eq!(
            geom.content_cells(prismattyc_mux::CellRect {
                col: 0,
                row: 0,
                cols,
                rows
            }),
            (80, 24)
        );
        assert_eq!(geom.top_chrome_px, 0);

        let tabbed = host_geom(&font, false, true, false, spacing, 0);
        let tabbed_size = initial_window_size(&font, tabbed, 80, 24);
        let (tab_cols, tab_rows) = size_to_cells(tabbed_size, &font, tabbed);
        assert_eq!(tabbed.top_chrome_px, font.cell_h);
        assert_eq!(
            tabbed.content_cells(prismattyc_mux::CellRect {
                col: 0,
                row: 0,
                cols: tab_cols,
                rows: tab_rows
            }),
            (80, 24)
        );

        let split_tabs = host_geom(&font, true, true, true, spacing, 0);
        assert_eq!(split_tabs.window_pad, 5);
        assert_eq!(split_tabs.pane_gap, 5);
        assert_eq!(split_tabs.rail_gap, 5);
        assert_eq!(split_tabs.top_chrome_px, font.cell_h * 2);
        let single_tabs = host_geom(&font, false, true, false, spacing, 0);
        assert_eq!(single_tabs.window_pad, 5);
        assert_eq!(single_tabs.pane_gap, 0);
        assert_eq!(
            single_tabs.rail_gap, split_tabs.rail_gap,
            "rail gap is configured spacing, not multi_pane"
        );
        let (cols_on, rows_on) = size_to_cells(size, &font, split_tabs);
        let (cols_off, rows_off) = size_to_cells(size, &font, geom);
        assert_eq!(cols_on, cols_off);
        assert!(
            rows_on < rows_off,
            "reserving the tab strip must shrink the cell grid (on={rows_on} off={rows_off})"
        );
    }

    #[test]
    fn app_mouse_motion_respects_tracking_level() {
        let mut emulator = Emulator::new(40, 10, 0);
        let _ = emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        assert!(encode_app_mouse_report(&emulator, 4, 1, 0, false, true, false, false).is_none());

        let _ = emulator.feed(b"\x1b[?1002h");
        assert_eq!(
            encode_app_mouse_report(&emulator, 4, 1, 0, false, true, false, false)
                .expect("button motion under 1002"),
            b"\x1b[<32;5;2M"
        );
        assert!(encode_app_mouse_report(&emulator, 4, 1, 3, false, true, false, false).is_none());

        let _ = emulator.feed(b"\x1b[?1003h");
        assert_eq!(
            encode_app_mouse_report(&emulator, 4, 1, 3, false, true, false, false)
                .expect("bare motion under 1003"),
            b"\x1b[<35;5;2M"
        );
    }

    #[test]
    fn wheel_only_7700_reports_wheel_not_buttons() {
        let mut emulator = Emulator::new(40, 10, 0);
        let _ = emulator.feed(b"\x1b[?7700h\x1b[?1006h");
        assert_eq!(
            emulator.mouse_tracking(),
            prismattyc_emulator::MouseTracking::Off
        );
        assert!(encode_app_mouse_report(&emulator, 4, 1, 0, false, false, false, false).is_none());
        assert_eq!(
            encode_app_mouse_report(&emulator, 4, 1, 64, false, false, false, false)
                .expect("wheel under 7700"),
            b"\x1b[<64;5;2M"
        );
    }

    #[test]
    fn attach_7700_on_alt_does_not_block_host_select() {
        let mut emulator = Emulator::new(40, 10, 0);
        let _ = emulator.feed(b"\x1b[?1049h\x1b[?7700h\x1b[?1006h");
        assert!(emulator.screen().alt_active());
        assert!(!guest_alt_blocks_host_select(&emulator));

        let _ = emulator.feed(b"\x1b[?1000h");
        assert!(
            guest_alt_blocks_host_select(&emulator),
            "real app mouse on alt still blocks plain select"
        );
    }

    #[test]
    fn user_action_variants_are_distinct() {
        assert_ne!(UserAction::Wake, UserAction::NewWindow);
        assert_eq!(UserAction::Wake, UserAction::Wake);
        assert_eq!(UserAction::NewWindow, UserAction::NewWindow);
    }

    #[test]
    fn drag_toast_subject_names_tab_or_pane_with_session() {
        assert_eq!(
            drag_toast_subject(&DragSubject::Tab("WEBSITE")),
            "Moving tab WEBSITE"
        );
        assert_eq!(
            drag_toast_subject(&DragSubject::Pane {
                index: 2,
                session: Some("grok-pc")
            }),
            "Moving pane 2 · grok-pc"
        );
        assert_eq!(
            drag_toast_subject(&DragSubject::Pane {
                index: 1,
                session: None
            }),
            "Moving pane 1",
            "a local shell has no session"
        );
    }

    #[test]
    fn drag_toast_target_follows_the_strip_hit() {
        let titles = vec!["PRISMATTYC".to_string(), "WEBSITE".to_string()];
        assert_eq!(
            drag_toast_target(
                Some(mux::StripHit::Tab {
                    index: 1,
                    close: false
                }),
                &titles
            ),
            "→ tab WEBSITE"
        );
        assert_eq!(
            drag_toast_target(Some(mux::StripHit::EmptyEnd), &titles),
            "→ new tab"
        );
        assert_eq!(drag_toast_target(None, &titles), "→ (no target)");
        assert_eq!(
            drag_toast_target(
                Some(mux::StripHit::Tab {
                    index: 9,
                    close: false
                }),
                &titles
            ),
            "→ (no target)",
            "a stale index never panics"
        );
    }

    #[test]
    fn chrome_cursor_matches_hover_and_drag_state() {
        let tab = Some(HoverTarget::Strip(mux::StripHit::Tab {
            index: 0,
            close: false,
        }));
        assert_eq!(
            cursor_for_hover(None, false, false, None, true),
            CursorIcon::Pointer
        );
        let rail = Some(HoverTarget::Rail(space_rail::RailHit::Plus));
        let pane = mux::MuxRuntime::spawn("/bin/sh", &[], 2, 2)
            .unwrap()
            .focused_id();
        let scrollbar = Some(HoverTarget::ScrollbarThumb(pane));
        assert_eq!(
            cursor_for_hover(tab, false, false, None, false),
            CursorIcon::Pointer
        );
        assert_eq!(
            cursor_for_hover(rail, false, false, None, false),
            CursorIcon::Pointer
        );
        assert_eq!(
            cursor_for_hover(scrollbar, false, false, None, false),
            CursorIcon::Default
        );
        assert_eq!(
            cursor_for_hover(tab, true, false, None, true),
            CursorIcon::Grab
        );
        assert_eq!(
            cursor_for_hover(scrollbar, false, true, None, true),
            CursorIcon::Grab
        );
        assert_eq!(
            cursor_for_hover(None, false, false, None, false),
            CursorIcon::Default
        );
        assert_eq!(
            cursor_for_hover(
                tab,
                false,
                false,
                Some(prismattyc_mux::Axis::Horizontal),
                true
            ),
            CursorIcon::ColResize,
            "divider resize wins over chrome hover"
        );
    }

    /// PT-171: a bare launch resolves the default instance instead of
    /// giving up; a set `PMUX_SOCKET` wins; an empty one is ignored.
    #[test]
    fn host_socket_resolves_env_then_default() {
        let default = || Ok(PathBuf::from("/run/user/1000/prismattyc/pmux.sock"));
        assert_eq!(
            resolve_host_socket(Some("/tmp/x/pmux.sock".into()), default),
            Some(PathBuf::from("/tmp/x/pmux.sock"))
        );
        assert_eq!(
            resolve_host_socket(None, default),
            Some(PathBuf::from("/run/user/1000/prismattyc/pmux.sock"))
        );
        assert_eq!(
            resolve_host_socket(Some("".into()), default),
            Some(PathBuf::from("/run/user/1000/prismattyc/pmux.sock"))
        );
        assert_eq!(
            resolve_host_socket(None, || Err(std::io::Error::other("no runtime dir"))),
            None
        );
    }

    #[test]
    fn render_timer_modes_match_requested_outputs() {
        assert!(!config::RenderTimer::Off.shows_osd());
        assert!(!config::RenderTimer::Off.logs());
        assert!(config::RenderTimer::Osd.shows_osd());
        assert!(!config::RenderTimer::Osd.logs());
        assert!(!config::RenderTimer::Log.shows_osd());
        assert!(config::RenderTimer::Log.logs());
        assert!(config::RenderTimer::Both.shows_osd());
        assert!(config::RenderTimer::Both.logs());
    }

    #[test]
    fn full_repaint_reason_labels_are_stable_for_logs_and_osd() {
        assert_eq!(FullRepaintReason::Resize.as_str(), "resize");
        assert_eq!(FullRepaintReason::AltScreen.as_str(), "alt-screen");
        assert_eq!(FullRepaintReason::Theme.as_str(), "theme");
        assert_eq!(FullRepaintReason::Scrollback.as_str(), "scrollback");
        assert_eq!(FullRepaintReason::NoDamage.as_str(), "no-damage");
    }

    #[test]
    fn render_log_is_throttled_to_once_per_second() {
        let start = Instant::now();
        let mut last = None;
        assert!(should_log_render_frame(&mut last, start));
        assert!(!should_log_render_frame(
            &mut last,
            start + Duration::from_millis(999)
        ));
        assert!(should_log_render_frame(
            &mut last,
            start + Duration::from_secs(1)
        ));
    }

    #[test]
    fn render_window_rolls_up_one_second_of_frames() {
        let start = Instant::now();
        let mut window = RenderWindow::default();
        let mut first = RenderFrame::default();
        first.timing.raster_us = 12;
        first.cells_painted = 100;
        first.rows_scrolled_as_blit = 2;
        first.full_repaint_reason = Some(FullRepaintReason::Resize);
        assert!(window.record(first, start).is_none());

        let mut second = RenderFrame::default();
        second.timing.raster_us = 20;
        second.cells_painted = 300;
        second.rows_scrolled_as_blit = 3;
        second.full_repaint_reason = Some(FullRepaintReason::NoDamage);
        assert_eq!(
            window.record(second, start + Duration::from_secs(1)),
            Some(RenderWindowSummary {
                last_raster_us: 20,
                max_raster_us: 20,
                frame_count: 2,
                max_cells_painted: 300,
                blit_sum: 5,
                dominant_full_repaint_reason: Some(FullRepaintReason::Resize),
            })
        );
        assert!(window
            .record(RenderFrame::default(), start + Duration::from_millis(1500))
            .is_none());
    }

    #[test]
    fn render_frame_starts_with_zero_counters() {
        let frame = RenderFrame::default();
        assert_eq!(frame.timing, RenderTiming::default());
        assert_eq!(frame.cells_painted, 0);
        assert_eq!(frame.rows_scrolled_as_blit, 0);
        assert_eq!(frame.full_repaint_reason, None);
    }

    #[test]
    fn transient_overlay_open_and_close_each_require_full_repaint() {
        assert!(!overlay_requires_full_repaint(false, false));
        assert!(overlay_requires_full_repaint(false, true));
        assert!(overlay_requires_full_repaint(true, true));
        assert!(overlay_requires_full_repaint(true, false));
    }
}
