// SPDX-License-Identifier: MIT
//! Complete native settings state. Rendering and input use the same real rows.

mod controls;
mod fonts;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use crate::config_document::{Change, Destination, Edit};
use crate::{config, palette::PaletteRow};
use controls::{Control, Kind, Section};

#[derive(Clone, Debug)]
enum Focus {
    Sections,
    Rows(usize),
    Editing(Editor),
}

#[derive(Clone, Debug)]
struct Editor {
    control: usize,
    base: toml::Value,
    remote_before: Vec<Destination>,
    draft: Draft,
    cursor: EditorFocus,
}

#[derive(Clone, Debug)]
enum Draft {
    Fields(Vec<toml::Value>),
    Strings(Vec<String>),
    Ansi(Vec<String>),
    Remote(Vec<Destination>),
    Font(Option<String>),
    Theme(String),
    Binding(Vec<String>),
}

#[derive(Clone, Debug)]
enum EditorFocus {
    Row(usize),
    Text {
        row: usize,
        text: String,
        caret: usize,
        select_all: bool,
    },
    Capture {
        row: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Field(usize),
    Add,
    Remove(usize),
    Earlier(usize),
    Later(usize),
    RemoteField(usize, usize),
    Font(Option<String>),
    Theme(String),
    Custom,
    Save,
    Reset,
    ResetAll,
    Reload,
    Cancel,
}

struct EditorRow {
    label: String,
    detail: String,
    command: Command,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum InputResult {
    Consumed,
    Close,
    Paste,
}

pub(super) struct Panel {
    path: PathBuf,
    wake: crate::mux::Wake,
    document: toml_edit::DocumentMut,
    values: toml::Value,
    config: config::ConfigFile,
    controls: Vec<Control>,
    section: usize,
    remembered: Vec<usize>,
    focus: Focus,
    pending: Option<Receiver<std::result::Result<crate::config_document::Receipt, String>>>,
    font_scan: Option<Receiver<Vec<fonts::Choice>>>,
    fonts_loaded: bool,
    refresh_pending: bool,
    font_choices: Vec<fonts::Choice>,
    notice: String,
    effective_font: String,
    pub scroll: usize,
}

impl Panel {
    pub fn open(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).context("read settings"),
        };
        Ok(Self {
            path: path.to_path_buf(),
            wake: std::sync::Arc::new(|| {}),
            document: raw.parse()?,
            values: raw.parse()?,
            config: config::parse(&raw, path)?,
            controls: controls::catalog(),
            section: 0,
            remembered: vec![0; Section::ALL.len()],
            focus: Focus::Sections,
            pending: None,
            font_scan: None,
            fonts_loaded: false,
            refresh_pending: false,
            font_choices: fonts::bundled(),
            notice: String::new(),
            effective_font: String::new(),
            scroll: 0,
        })
    }

    fn indices(&self) -> Vec<usize> {
        self.controls
            .iter()
            .enumerate()
            .filter(|(_, row)| row.section == Section::ALL[self.section])
            .map(|(index, _)| index)
            .collect()
    }

    fn configured(&self, control: &Control) -> Option<toml::Value> {
        crate::config_document::value_at(&self.values, &control.paths[0]).cloned()
    }

    fn shown(&self, control: &Control) -> String {
        let Some(value) = self.configured(control) else {
            return "Default / inherit".into();
        };
        match value {
            toml::Value::String(text) => text,
            toml::Value::Array(items) => format!("{} entries", items.len()),
            other => other.to_string(),
        }
    }

    fn root_rows(&self) -> Vec<PaletteRow> {
        let mut rows = vec![PaletteRow::plain(
            Section::ALL[self.section].label().into(),
            if self.notice.is_empty() {
                "←/→ sections · ↓ settings · Escape close".into()
            } else {
                self.notice.clone()
            },
            String::new(),
        )];
        rows.extend(self.indices().into_iter().map(|index| {
            let control = &self.controls[index];
            let mut detail = format!("{} · {}", self.shown(control), control.timing);
            if control.key() == "font" && !self.effective_font.is_empty() {
                detail.push_str(&format!(" · Using {}", self.effective_font));
            }
            if control.key() == "font" && std::env::var_os("PRISMATTYC_HOST_FONT").is_some() {
                detail.push_str(" · Environment font takes precedence when loadable");
            }
            if control.key() == "focus_border"
                && (std::env::var_os("PRISMATTYC_FOCUS_BORDER").is_some()
                    || std::env::var_os("PRISMATTYC_FOCUS_COLOR").is_some())
            {
                detail.push_str(" · Environment/CLI override may take precedence");
            }
            if control.kind.cycles_on_enter() {
                detail.push_str(" · Enter cycles; Shift+Enter opens editor");
            }
            if !control.help.is_empty() {
                detail.push_str(&format!(" · {}", control.help));
            }
            PaletteRow::plain(
                control.label.clone(),
                detail,
                if control.kind.cycles_on_enter() {
                    "Cycle".into()
                } else {
                    "Enter".into()
                },
            )
        }));
        rows
    }

    pub fn rows(&self) -> (String, Vec<PaletteRow>) {
        if let Focus::Editing(editor) = &self.focus {
            let rows = self.editor_rows(editor).into_iter().enumerate().map(|(index,row)| {
                let active = matches!(&editor.cursor,EditorFocus::Text{row,..}|EditorFocus::Capture{row} if *row==index);
                let label = match &editor.cursor {
                    EditorFocus::Text{row:active_row,text,caret,..} if *active_row==index => {
                        let mut text:Vec<_>=text.chars().collect(); text.insert((*caret).min(text.len()), '│');
                        format!("{}: {}",row_label(&row.command,&row.label),text.into_iter().collect::<String>())
                    }
                    EditorFocus::Capture{row:active_row} if *active_row==index=>"Press the chord to bind (including Enter or Escape)".into(),
                    _=>row.label,
                };
                PaletteRow::plain(label,row.detail,if active {"Editing"}else{"Enter"}.into())
            }).collect();
            let notice = if self.notice.is_empty() {
                "↑/↓ rows · ←/→ value · Enter edit · Escape cancel"
            } else {
                &self.notice
            };
            (
                format!("{} — {}", self.controls[editor.control].label, notice),
                rows,
            )
        } else {
            (
                format!("SETTINGS — {}", Section::ALL[self.section].label()),
                self.root_rows(),
            )
        }
    }

    pub fn selected(&self) -> usize {
        match &self.focus {
            Focus::Sections => 0,
            Focus::Rows(row) => row + 1,
            Focus::Editing(editor) => match &editor.cursor {
                EditorFocus::Row(row)
                | EditorFocus::Text { row, .. }
                | EditorFocus::Capture { row } => *row,
            },
        }
    }

    /// Mouse selection uses the exact same row addresses as keyboard input.
    pub fn select(&mut self, index: usize) {
        let editor_len = if let Focus::Editing(editor) = &self.focus {
            self.editor_rows(editor).len()
        } else {
            0
        };
        match &mut self.focus {
            Focus::Editing(editor) => {
                editor.cursor = EditorFocus::Row(index.min(editor_len.saturating_sub(1)))
            }
            _ => {
                let row = (index.saturating_sub(1)).min(self.indices().len().saturating_sub(1));
                self.focus = if index == 0 {
                    Focus::Sections
                } else {
                    self.remembered[self.section] = row;
                    Focus::Rows(row)
                };
            }
        }
    }

    pub fn scroll_rows(&mut self, rows: i32) {
        if matches!(
            self.focus,
            Focus::Editing(Editor {
                cursor: EditorFocus::Text { .. } | EditorFocus::Capture { .. },
                ..
            })
        ) {
            return;
        }
        let len = self.rows().1.len();
        let selected = self
            .selected()
            .saturating_add_signed(rows as isize)
            .min(len.saturating_sub(1));
        self.select(selected);
    }

    pub fn hover(&mut self, index: usize) {
        if matches!(
            self.focus,
            Focus::Editing(Editor {
                cursor: EditorFocus::Text { .. } | EditorFocus::Capture { .. },
                ..
            })
        ) {
            return;
        }
        self.select(index);
    }

    pub fn activate(&mut self, index: usize) {
        let focus = std::mem::replace(&mut self.focus, Focus::Sections);
        self.focus = match focus {
            Focus::Editing(mut editor) => {
                if let EditorFocus::Text { row, text, .. } = &editor.cursor {
                    let row = *row;
                    let text = text.clone();
                    let cancel = self.editor_rows(&editor).get(index).is_some_and(|row| {
                        matches!(row.command, Command::Cancel | Command::Reload)
                    });
                    if !cancel {
                        if let Err(error) = self.set_editor_text(&mut editor, row, &text) {
                            self.notice = error;
                            self.focus = Focus::Editing(editor);
                            return;
                        }
                    }
                }
                Focus::Editing(editor)
            }
            other => other,
        };
        self.select(index);
        let _ = self.key(&Key::Named(NamedKey::Enter), ModifiersState::empty(), false);
    }

    /// Return true only when the modal closes. Every event is otherwise consumed.
    pub fn input_key(&mut self, key: &Key, mods: ModifiersState, repeat: bool) -> InputResult {
        let text_focus = matches!(
            self.focus,
            Focus::Editing(Editor {
                cursor: EditorFocus::Text { .. },
                ..
            })
        );
        if text_focus
            && (mods.control_key() || mods.super_key())
            && matches!(key,Key::Character(text)if text.eq_ignore_ascii_case("v"))
        {
            InputResult::Paste
        } else if self.key(key, mods, repeat) {
            InputResult::Close
        } else {
            InputResult::Consumed
        }
    }

    pub fn key(&mut self, key: &Key, mods: ModifiersState, repeat: bool) -> bool {
        if repeat && matches!(key, Key::Named(NamedKey::Enter | NamedKey::Escape)) {
            return false;
        }
        let focus = std::mem::replace(&mut self.focus, Focus::Sections);
        match focus {
            Focus::Sections => match key {
                Key::Named(NamedKey::Escape) => return true,
                Key::Named(NamedKey::ArrowLeft) => {
                    self.section = (self.section + Section::ALL.len() - 1) % Section::ALL.len();
                    self.scroll = 0;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.section = (self.section + 1) % Section::ALL.len();
                    self.scroll = 0;
                }
                Key::Named(NamedKey::ArrowDown | NamedKey::Enter) => {
                    self.focus = Focus::Rows(
                        self.remembered[self.section].min(self.indices().len().saturating_sub(1)),
                    )
                }
                _ => {}
            },
            Focus::Rows(mut row) => {
                let indices = self.indices();
                row = row.min(indices.len().saturating_sub(1));
                self.focus = Focus::Rows(row);
                match key {
                    Key::Named(NamedKey::Escape) => self.focus = Focus::Sections,
                    Key::Named(NamedKey::ArrowUp) => {
                        self.focus = if row == 0 {
                            Focus::Sections
                        } else {
                            Focus::Rows(row - 1)
                        }
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        self.focus = Focus::Rows((row + 1).min(indices.len().saturating_sub(1)))
                    }
                    Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight)
                        if self.pending.is_none() =>
                    {
                        if let Some(&index) = indices.get(row) {
                            self.step(index, matches!(key, Key::Named(NamedKey::ArrowRight)));
                        }
                    }
                    Key::Named(NamedKey::Enter) if self.pending.is_none() => {
                        if let Some(&index) = indices.get(row) {
                            if mods.shift_key() || !self.controls[index].kind.cycles_on_enter() {
                                self.edit(index)
                            } else {
                                self.cycle(index);
                            }
                        }
                    }
                    _ => {}
                }
                if let Focus::Rows(row) = self.focus {
                    self.remembered[self.section] = row;
                }
            }
            Focus::Editing(mut editor) => {
                self.editor_key(&mut editor, key, mods);
                if !matches!(self.focus, Focus::Rows(_)) {
                    self.focus = Focus::Editing(editor);
                }
            }
        }
        false
    }

    /// Enter on a discrete row: a boolean flips; other kinds step forward.
    fn cycle(&mut self, index: usize) {
        let control = &self.controls[index];
        let forward = match &control.kind {
            Kind::Bool => !self
                .configured(control)
                .as_ref()
                .or(control.default.as_ref())
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            _ => true,
        };
        self.step(index, forward);
    }

    fn step(&mut self, index: usize, right: bool) {
        let control = &self.controls[index];
        let before = self.configured(control);
        let current = before.as_ref().or(control.default.as_ref());
        let next = match &control.kind {
            Kind::Bool => Some(toml::Value::Boolean(right)),
            Kind::Integer { min, max, step } => Some(toml::Value::Integer(
                current
                    .and_then(toml::Value::as_integer)
                    .unwrap_or(*min)
                    .saturating_add(if right { *step } else { -*step })
                    .clamp(*min, *max),
            )),
            Kind::Float { min, max, step } => {
                let value = current
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                    .unwrap_or(*min);
                Some(toml::Value::Float(
                    ((value + if right { *step } else { -*step }).clamp(*min, *max) * 10000.0)
                        .round()
                        / 10000.0,
                ))
            }
            Kind::ChipWidth => {
                let old = current.and_then(toml::Value::as_integer).unwrap_or(0);
                let value = if right {
                    if old == 0 {
                        6
                    } else {
                        old.saturating_add(1).min(40)
                    }
                } else if old <= 6 {
                    0
                } else {
                    old.saturating_sub(1)
                };
                Some(value.into())
            }
            Kind::Choice(choices) => {
                let normalized = if control.key() == "space_rail" {
                    Some(self.config.space_rail().as_str().into())
                } else {
                    None
                };
                Some(cycle_choice(
                    choices,
                    normalized.as_ref().or(current),
                    right,
                ))
            }
            Kind::BorderColor => {
                let index = current
                    .and_then(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| value.as_integer().map(|index| index.to_string()))
                    })
                    .and_then(|value| crate::raster::parse_focus_border(&value))
                    .unwrap_or(0);
                let index = if right {
                    crate::raster::cycle_focus_border(index)
                } else {
                    crate::raster::cycle_focus_border_back(index)
                };
                Some(crate::raster::FOCUS_BORDER_PALETTE[index].0.into())
            }
            _ => {
                self.notice = "Enter opens this editor and its reset controls".into();
                None
            }
        };
        if let Some(value) = next {
            self.submit(Edit {
                changes: vec![Change {
                    path: control.paths[0].clone(),
                    expected: before,
                    value: Some(value),
                }],
                remote: None,
            });
        }
    }

    fn edit(&mut self, index: usize) {
        let control = &self.controls[index];
        let value = self.configured(control);
        let draft = match &control.kind {
            Kind::Strings => Draft::Strings(strings(value.as_ref())),
            Kind::Binding(action) => {
                let values = match value {
                    Some(toml::Value::String(value)) => vec![value],
                    Some(toml::Value::Array(values)) => values
                        .into_iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect(),
                    _ => self.config.loaded_keymap().spellings(*action),
                };
                Draft::Binding(values)
            }
            Kind::Ansi => Draft::Ansi(
                value
                    .as_ref()
                    .map(|v| strings(Some(v)))
                    .unwrap_or_else(|| self.config.loaded_theme().ansi.iter().map(rgb).collect()),
            ),
            Kind::Remote => Draft::Remote(crate::config_document::destinations(&self.config)),
            Kind::Font => {
                if !self.fonts_loaded && self.font_scan.is_none() {
                    let (tx, rx) = mpsc::channel();
                    let wake = self.wake.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(fonts::installed());
                        wake();
                    });
                    self.font_scan = Some(rx);
                }
                Draft::Font(value.and_then(|v| v.as_str().map(str::to_owned)))
            }
            Kind::Theme => Draft::Theme(
                value
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
            ),
            Kind::Pair => {
                let theme = self.config.loaded_theme();
                let cursor = control.key().contains("cursor");
                let defaults = if cursor {
                    [
                        theme.cursor_fg.unwrap_or(theme.default_bg),
                        theme.cursor_bg.unwrap_or(theme.default_fg),
                    ]
                } else {
                    [
                        theme.selection_fg.unwrap_or(theme.default_fg),
                        theme.selection_bg.unwrap_or(theme.default_bg),
                    ]
                };
                Draft::Fields(
                    control
                        .paths
                        .iter()
                        .zip(defaults)
                        .map(|(path, default)| {
                            crate::config_document::value_at(&self.values, path)
                                .cloned()
                                .unwrap_or_else(|| rgb(&default).into())
                        })
                        .collect(),
                )
            }
            _ => Draft::Fields(vec![value
                .or_else(|| control.default.clone())
                .unwrap_or_else(|| {
                    if matches!(control.kind, Kind::Color) {
                        "#808080".into()
                    } else {
                        "".into()
                    }
                })]),
        };
        self.notice.clear();
        self.scroll = 0;
        self.focus = Focus::Editing(Editor {
            control: index,
            base: self.values.clone(),
            remote_before: crate::config_document::destinations(&self.config),
            draft,
            cursor: EditorFocus::Row(0),
        });
    }

    fn submit(&mut self, edit: Edit) {
        if self.pending.is_some() {
            return;
        }
        let path = self.path.clone();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.notice = "Saving…".into();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let _ = tx.send(
                crate::config_document::commit(&path, edit).map_err(|error| format!("{error:#}")),
            );
            wake();
        });
    }

    /// Receipts settle on completion; no exact watcher-revision equality wait.
    /// The watcher may legitimately coalesce several saves into a later snapshot.
    pub fn poll(&mut self) -> (bool, bool) {
        let mut changed = false;
        let editing_text = matches!(
            self.focus,
            Focus::Editing(Editor {
                cursor: EditorFocus::Text { .. } | EditorFocus::Capture { .. },
                ..
            })
        );
        if !editing_text {
            if let Some(choices) = self.font_scan.as_ref().and_then(|rx| rx.try_recv().ok()) {
                let selected = if let Focus::Editing(editor) = &self.focus {
                    self.editor_rows(editor)
                        .get(self.selected())
                        .map(|row| row.command.clone())
                } else {
                    None
                };
                self.font_choices.extend(choices);
                self.font_scan = None;
                self.fonts_loaded = true;
                changed = true;
                if let Some(command) = selected {
                    let position = if let Focus::Editing(editor) = &self.focus {
                        self.editor_rows(editor)
                            .iter()
                            .position(|row| row.command == command)
                    } else {
                        None
                    };
                    if let (Some(position), Focus::Editing(editor)) = (position, &mut self.focus) {
                        editor.cursor = EditorFocus::Row(position);
                    }
                }
            }
        }
        let result = self.pending.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("settings writer stopped".into())),
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some(result) = result {
            changed = true;
            self.pending = None;
            match result {
                Ok(receipt) => {
                    self.values = receipt
                        .document
                        .to_string()
                        .parse()
                        .expect("validated receipt document");
                    self.document = receipt.document;
                    self.config = receipt.config;
                    self.notice = receipt
                        .durability_warning
                        .map(|warning| format!("Saved; durability warning: {warning}"))
                        .unwrap_or_else(|| "Saved. Live options reload automatically.".into());
                    self.focus = Focus::Rows(self.remembered[self.section]);
                    if self.refresh_pending {
                        self.refresh_pending = false;
                        self.refresh();
                        self.notice
                            .push_str(" Current file refreshed after a newer reload.");
                    }
                    return (true, !self.config.settings_panel.unwrap_or(false));
                }
                Err(error) => {
                    self.notice = format!("Could not save: {error}");
                    if self.refresh_pending {
                        self.refresh_pending = false;
                        self.refresh();
                    }
                }
            }
        }
        (changed, false)
    }

    pub fn refresh(&mut self) {
        if self.pending.is_some() {
            self.refresh_pending = true;
            return;
        }
        if let Ok(raw) = std::fs::read_to_string(&self.path) {
            if let (Ok(document), Ok(values), Ok(config)) =
                (raw.parse(), raw.parse(), config::parse(&raw, &self.path))
            {
                self.document = document;
                self.values = values;
                self.config = config;
            }
        }
    }
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn rgb(value: &[u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", value[0], value[1], value[2])
}
fn cycle_choice(choices: &[String], current: Option<&toml::Value>, right: bool) -> toml::Value {
    let selected = current
        .and_then(toml::Value::as_str)
        .and_then(|value| choices.iter().position(|choice| choice == value))
        .unwrap_or(0);
    let next = if right {
        (selected + 1) % choices.len()
    } else {
        (selected + choices.len() - 1) % choices.len()
    };
    choices[next].clone().into()
}
fn row_label<'a>(_command: &Command, label: &'a str) -> &'a str {
    label.split(':').next().unwrap_or(label)
}

impl Panel {
    fn editor_rows(&self, editor: &Editor) -> Vec<EditorRow> {
        let mut rows = Vec::new();
        let mut add = |label: String, detail: &str, command: Command| {
            rows.push(EditorRow {
                label,
                detail: detail.into(),
                command,
            })
        };
        match &editor.draft {
            Draft::Fields(values) => {
                for (index, value) in values.iter().enumerate() {
                    let label = if values.len() == 2 {
                        if index == 0 {
                            "Foreground"
                        } else {
                            "Background"
                        }
                    } else {
                        "Value"
                    };
                    add(
                        format!("{label}: {}", display_value(value)),
                        "←/→ adjusts choices; Enter enters an exact value",
                        Command::Field(index),
                    );
                }
            }
            Draft::Strings(values) | Draft::Binding(values) => {
                for (index, value) in values.iter().enumerate() {
                    add(
                        format!("{}: {value}", index + 1),
                        if matches!(editor.draft, Draft::Binding(_)) {
                            "Enter captures a chord"
                        } else {
                            "Enter edits this entry"
                        },
                        Command::Field(index),
                    );
                    add(
                        format!("Move entry {} earlier", index + 1),
                        "Preserve the entry and its position-dependent priority",
                        Command::Earlier(index),
                    );
                    add(
                        format!("Move entry {} later", index + 1),
                        "",
                        Command::Later(index),
                    );
                    add(
                        format!("Remove entry {}", index + 1),
                        "Removes this entry only",
                        Command::Remove(index),
                    );
                }
                add("Add entry".into(), "", Command::Add);
            }
            Draft::Ansi(values) => {
                for (index, value) in values.iter().enumerate() {
                    add(
                        format!("ANSI {index}: {value}"),
                        "Hex RGB; all 16 colors save together",
                        Command::Field(index),
                    );
                }
            }
            Draft::Remote(records) => {
                for (index, record) in records.iter().enumerate() {
                    for (field, (label, value)) in [
                        ("ID", record.id.as_str()),
                        ("SSH alias", record.ssh.as_str()),
                        ("Label", record.label.as_deref().unwrap_or("")),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        add(
                            format!("Destination {} {label}: {value}", index + 1),
                            "All destination changes save together",
                            Command::RemoteField(index, field),
                        );
                    }
                    add(
                        format!("Move destination {} earlier", index + 1),
                        "",
                        Command::Earlier(index),
                    );
                    add(
                        format!("Move destination {} later", index + 1),
                        "",
                        Command::Later(index),
                    );
                    add(
                        format!("Delete destination {}", index + 1),
                        "Removes this whole record, including its extensions",
                        Command::Remove(index),
                    );
                }
                add(
                    "Add destination".into(),
                    "Enter an ID and SSH Host alias before saving",
                    Command::Add,
                );
            }
            Draft::Font(selected) => {
                for choice in &self.font_choices {
                    add(
                        format!(
                            "{}{}",
                            if *selected == choice.identity {
                                "✓ "
                            } else {
                                ""
                            },
                            choice.label
                        ),
                        "Keeps the normal glyph fallback chain",
                        Command::Font(choice.identity.clone()),
                    );
                }
                add(
                    format!("Custom font path: {}", selected.as_deref().unwrap_or("")),
                    "Enter a font file path",
                    Command::Custom,
                );
            }
            Draft::Theme(selected) => {
                add(
                    format!(
                        "{}Default theme",
                        if selected.is_empty() { "✓ " } else { "" }
                    ),
                    "Follow the application default",
                    Command::Theme(String::new()),
                );
                for theme in crate::theme::builtins() {
                    add(
                        format!(
                            "{}{}",
                            if selected == &theme.id { "✓ " } else { "" },
                            theme.name
                        ),
                        "Independent color overrides are retained",
                        Command::Theme(theme.id.clone()),
                    );
                }
                if let Some(parent) = self.path.parent() {
                    if let Ok(entries) = std::fs::read_dir(parent.join("themes")) {
                        let mut names: Vec<_> = entries
                            .flatten()
                            .filter_map(|entry| {
                                entry
                                    .path()
                                    .file_stem()
                                    .and_then(|name| name.to_str())
                                    .map(str::to_owned)
                            })
                            .collect();
                        names.sort();
                        for name in names {
                            add(
                                format!("Local theme: {name}"),
                                "Config-local themes directory",
                                Command::Theme(name),
                            );
                        }
                    }
                }
                add(
                    format!("Custom theme name/path: {selected}"),
                    "Built-in/local name or absolute TOML theme path",
                    Command::Custom,
                );
            }
        }
        add(
            "Save".into(),
            "Validate and save the complete edit",
            Command::Save,
        );
        add(
            "Reset / inherit".into(),
            "Remove this override; retain unrelated values",
            Command::Reset,
        );
        if self.controls[editor.control].key() == "spaces.autosave"
            || self.controls[editor.control].key() == "space_autosave"
        {
            add(
                "Reset both autosave layers".into(),
                "Remove canonical and legacy overrides",
                Command::ResetAll,
            );
        }
        add(
            "Reload from disk".into(),
            "Discard this draft and read current values",
            Command::Reload,
        );
        add("Cancel".into(), "Discard the draft", Command::Cancel);
        rows
    }

    fn editor_key(&mut self, editor: &mut Editor, key: &Key, mods: ModifiersState) {
        let cursor = std::mem::replace(&mut editor.cursor, EditorFocus::Row(0));
        match cursor {
            EditorFocus::Capture { row } => {
                editor.cursor = EditorFocus::Capture { row };
                let spec = match key {
                    Key::Character(text) if text.chars().count() == 1 => {
                        Some(crate::keybind::KeySpec::Char(text.chars().next().unwrap()))
                    }
                    Key::Named(named) => Some(crate::keybind::KeySpec::Named(*named)),
                    _ => None,
                };
                if let Some(spec) = spec {
                    let chord = crate::keybind::Chord {
                        ctrl: mods.control_key(),
                        shift: mods.shift_key(),
                        alt: mods.alt_key(),
                        super_key: mods.super_key(),
                        key: spec,
                    };
                    if let Ok(chord) = crate::keybind::Chord::parse(&chord.to_string()) {
                        if let Some(Command::Field(index)) = self
                            .editor_rows(editor)
                            .get(row)
                            .map(|row| row.command.clone())
                        {
                            if let Draft::Binding(values) = &mut editor.draft {
                                values[index] = chord.to_string();
                            }
                        }
                        editor.cursor = EditorFocus::Row(row);
                    }
                }
            }
            EditorFocus::Text {
                row,
                mut text,
                mut caret,
                mut select_all,
            } => {
                let mut chars: Vec<char> = text.chars().collect();
                let mut finish = false;
                let mut cancel = false;
                match key {
                    Key::Named(NamedKey::Escape) => cancel = true,
                    Key::Named(NamedKey::Enter) => finish = true,
                    Key::Named(NamedKey::ArrowLeft) => {
                        caret = caret.saturating_sub(1);
                        select_all = false;
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        caret = (caret + 1).min(chars.len());
                        select_all = false;
                    }
                    Key::Named(NamedKey::Home) => {
                        caret = 0;
                        select_all = false;
                    }
                    Key::Named(NamedKey::End) => {
                        caret = chars.len();
                        select_all = false;
                    }
                    Key::Named(NamedKey::Backspace) => {
                        if select_all {
                            chars.clear();
                            caret = 0;
                            select_all = false;
                        } else if caret > 0 {
                            caret -= 1;
                            chars.remove(caret);
                        }
                    }
                    Key::Named(NamedKey::Delete) => {
                        if select_all {
                            chars.clear();
                            caret = 0;
                            select_all = false;
                        } else if caret < chars.len() {
                            chars.remove(caret);
                        }
                    }
                    Key::Character(value)
                        if (mods.control_key() || mods.super_key())
                            && value.eq_ignore_ascii_case("a") =>
                    {
                        select_all = true
                    }
                    Key::Character(value)
                        if !mods.control_key() && !mods.super_key() && !mods.alt_key() =>
                    {
                        if select_all {
                            chars.clear();
                            caret = 0;
                            select_all = false;
                        }
                        for ch in value.chars().filter(|ch| !ch.is_control()) {
                            if chars.len() >= 4096 {
                                break;
                            }
                            chars.insert(caret, ch);
                            caret += 1;
                        }
                    }
                    _ => {}
                }
                text = chars.into_iter().collect();
                if finish {
                    match self.set_editor_text(editor, row, &text) {
                        Ok(()) => editor.cursor = EditorFocus::Row(row),
                        Err(error) => {
                            self.notice = error;
                            editor.cursor = EditorFocus::Text {
                                row,
                                text,
                                caret,
                                select_all,
                            };
                        }
                    }
                } else if cancel {
                    editor.cursor = EditorFocus::Row(row);
                } else {
                    editor.cursor = EditorFocus::Text {
                        row,
                        text,
                        caret,
                        select_all,
                    };
                }
            }
            EditorFocus::Row(mut row) => {
                let rows = self.editor_rows(editor);
                row = row.min(rows.len().saturating_sub(1));
                editor.cursor = EditorFocus::Row(row);
                match key {
                    Key::Named(NamedKey::Escape) => {
                        self.focus = Focus::Rows(self.remembered[self.section]);
                        self.notice.clear();
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        editor.cursor = EditorFocus::Row(row.saturating_sub(1))
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        editor.cursor =
                            EditorFocus::Row((row + 1).min(rows.len().saturating_sub(1)))
                    }
                    Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
                        if let Some(Command::Field(index)) =
                            rows.get(row).map(|row| row.command.clone())
                        {
                            self.step_draft(
                                editor,
                                index,
                                matches!(key, Key::Named(NamedKey::ArrowRight)),
                            );
                        }
                    }
                    Key::Named(NamedKey::Enter) if self.pending.is_none() => {
                        if let Some(action) = rows.get(row).map(|row| row.command.clone()) {
                            self.editor_action(editor, row, action)
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn step_draft(&mut self, editor: &mut Editor, index: usize, right: bool) {
        if let Draft::Fields(values) = &mut editor.draft {
            let kind = &self.controls[editor.control].kind;
            let value = &mut values[index];
            match kind {
                Kind::Bool => *value = right.into(),
                Kind::Choice(choices) => *value = cycle_choice(choices, Some(value), right),
                Kind::Integer { min, max, step } => {
                    *value = value
                        .as_integer()
                        .unwrap_or(*min)
                        .saturating_add(if right { *step } else { -*step })
                        .clamp(*min, *max)
                        .into()
                }
                Kind::Float { min, max, step } => {
                    *value = ((value
                        .as_float()
                        .or_else(|| value.as_integer().map(|v| v as f64))
                        .unwrap_or(*min)
                        + if right { *step } else { -*step })
                    .clamp(*min, *max))
                    .into()
                }
                _ => {}
            }
        }
    }

    fn editor_action(&mut self, editor: &mut Editor, row: usize, action: Command) {
        match action {
            Command::Field(_) | Command::RemoteField(_, _) | Command::Custom => {
                if matches!(editor.draft, Draft::Binding(_)) {
                    editor.cursor = EditorFocus::Capture { row };
                } else {
                    let text = self.editor_text(editor, &action);
                    editor.cursor = EditorFocus::Text {
                        row,
                        caret: text.chars().count(),
                        text,
                        select_all: true,
                    };
                }
            }
            Command::Font(value) => editor.draft = Draft::Font(value),
            Command::Theme(value) => editor.draft = Draft::Theme(value),
            Command::Add => match &mut editor.draft {
                Draft::Strings(values) | Draft::Binding(values) => values.push(String::new()),
                Draft::Remote(records) => records.push(Destination {
                    original_id: None,
                    id: String::new(),
                    ssh: String::new(),
                    label: None,
                }),
                _ => {}
            },
            Command::Remove(index) => match &mut editor.draft {
                Draft::Strings(values) | Draft::Binding(values) => {
                    values.remove(index);
                }
                Draft::Remote(records) => {
                    records.remove(index);
                }
                _ => {}
            },
            Command::Earlier(index) | Command::Later(index) => {
                let earlier = matches!(action, Command::Earlier(_));
                let swap = |len: usize| {
                    if earlier {
                        index.saturating_sub(1)
                    } else {
                        (index + 1).min(len.saturating_sub(1))
                    }
                };
                match &mut editor.draft {
                    Draft::Strings(values) | Draft::Binding(values) => {
                        let next = swap(values.len());
                        values.swap(index, next);
                    }
                    Draft::Remote(values) => {
                        let next = swap(values.len());
                        values.swap(index, next);
                    }
                    _ => {}
                }
            }
            Command::Save => match self.editor_edit(editor) {
                Ok(edit) => self.submit(edit),
                Err(error) => self.notice = error,
            },
            Command::Reset => {
                let control = &self.controls[editor.control];
                if matches!(control.kind, Kind::Remote) {
                    self.submit(Edit {
                        changes: Vec::new(),
                        remote: Some((editor.remote_before.clone(), Vec::new())),
                    });
                } else {
                    let changes = control
                        .paths
                        .iter()
                        .map(|path| Change {
                            path: path.clone(),
                            expected: crate::config_document::value_at(&editor.base, path).cloned(),
                            value: None,
                        })
                        .collect();
                    self.submit(Edit {
                        changes,
                        remote: None,
                    });
                }
            }
            Command::ResetAll => {
                let changes = [
                    vec!["spaces".into(), "autosave".into()],
                    vec!["space_autosave".into()],
                ]
                .into_iter()
                .map(|path| Change {
                    expected: crate::config_document::value_at(&editor.base, &path).cloned(),
                    path,
                    value: None,
                })
                .collect();
                self.submit(Edit {
                    changes,
                    remote: None,
                });
            }
            Command::Reload => {
                self.refresh();
                self.edit(editor.control);
                if let Focus::Editing(reloaded) =
                    std::mem::replace(&mut self.focus, Focus::Sections)
                {
                    *editor = reloaded;
                }
                self.notice = "Reloaded; draft discarded".into();
            }
            Command::Cancel => {
                self.focus = Focus::Rows(self.remembered[self.section]);
                self.notice.clear();
            }
        }
        let count = self.editor_rows(editor).len();
        if let EditorFocus::Row(index) = &mut editor.cursor {
            *index = (*index).min(count.saturating_sub(1));
        }
    }

    fn editor_text(&self, editor: &Editor, command: &Command) -> String {
        match (&editor.draft, command) {
            (Draft::Fields(values), Command::Field(index)) => display_value(&values[*index]),
            (
                Draft::Strings(values) | Draft::Ansi(values) | Draft::Binding(values),
                Command::Field(index),
            ) => values[*index].clone(),
            (Draft::Remote(records), Command::RemoteField(index, field)) => match field {
                0 => records[*index].id.clone(),
                1 => records[*index].ssh.clone(),
                _ => records[*index].label.clone().unwrap_or_default(),
            },
            (Draft::Font(value), Command::Custom) => value.clone().unwrap_or_default(),
            (Draft::Theme(value), Command::Custom) => value.clone(),
            _ => String::new(),
        }
    }

    fn set_editor_text(
        &self,
        editor: &mut Editor,
        row: usize,
        text: &str,
    ) -> std::result::Result<(), String> {
        let command = self
            .editor_rows(editor)
            .get(row)
            .map(|row| row.command.clone())
            .ok_or("Edit target disappeared")?;
        match (&mut editor.draft, command) {
            (Draft::Fields(values), Command::Field(index)) => {
                let kind = &self.controls[editor.control].kind;
                values[index] = match kind {
                    Kind::Bool => text
                        .parse::<bool>()
                        .map(Into::into)
                        .map_err(|_| "Use true or false".to_string())?,
                    Kind::Integer { .. } | Kind::ChipWidth => {
                        text.parse::<i64>()
                            .map(Into::into)
                            .map_err(|_| "Enter a whole number".to_string())?
                    }
                    Kind::Float { .. } => text
                        .parse::<f64>()
                        .map(Into::into)
                        .map_err(|_| "Enter a number".to_string())?,
                    _ => text.into(),
                };
            }
            (Draft::Strings(values) | Draft::Ansi(values), Command::Field(index)) => {
                values[index] = text.into()
            }
            (Draft::Remote(records), Command::RemoteField(index, field)) => match field {
                0 => records[index].id = text.into(),
                1 => records[index].ssh = text.into(),
                _ => records[index].label = (!text.is_empty()).then(|| text.into()),
            },
            (Draft::Font(value), Command::Custom) => {
                *value = (!text.is_empty()).then(|| text.into())
            }
            (Draft::Theme(value), Command::Custom) => *value = text.into(),
            _ => return Err("This row is not a text field".into()),
        }
        Ok(())
    }

    fn editor_edit(&self, editor: &Editor) -> std::result::Result<Edit, String> {
        let control = &self.controls[editor.control];
        let values = match &editor.draft {
            Draft::Fields(values) => values.iter().cloned().map(Some).collect::<Vec<_>>(),
            Draft::Strings(values) | Draft::Ansi(values) | Draft::Binding(values) => vec![Some(
                toml::Value::Array(values.iter().cloned().map(Into::into).collect()),
            )],
            Draft::Font(value) => vec![value.clone().map(Into::into)],
            Draft::Theme(value) => vec![(!value.is_empty()).then(|| value.clone().into())],
            Draft::Remote(records) => {
                return Ok(Edit {
                    changes: Vec::new(),
                    remote: Some((editor.remote_before.clone(), records.clone())),
                })
            }
        };
        Ok(Edit {
            changes: control
                .paths
                .iter()
                .zip(values)
                .map(|(path, value)| Change {
                    path: path.clone(),
                    expected: crate::config_document::value_at(&editor.base, path).cloned(),
                    value,
                })
                .collect(),
            remote: None,
        })
    }

    pub fn paste(&mut self, text: &str) {
        if let Focus::Editing(Editor {
            cursor:
                EditorFocus::Text {
                    row: _,
                    text: value,
                    caret,
                    select_all,
                },
            ..
        }) = &mut self.focus
        {
            let mut chars: Vec<_> = value.chars().collect();
            if *select_all {
                chars.clear();
                *caret = 0;
                *select_all = false;
            }
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                if chars.len() >= 4096 {
                    break;
                }
                chars.insert(*caret, ch);
                *caret += 1;
            }
            *value = chars.into_iter().collect();
        }
    }
}

fn display_value(value: &toml::Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn prepare_entry(
    session_prompt: bool,
    restore_prompt: bool,
    rail: &mut crate::space_rail::SpaceRail,
) -> bool {
    if session_prompt || restore_prompt {
        return false;
    }
    rail.leave();
    true
}

pub(super) fn open(host: &mut crate::HostState) {
    if !prepare_entry(
        host.session_prompt.is_some(),
        host.restore_prompt.is_some(),
        &mut host.space_rail,
    ) {
        return;
    }
    let mut panel = match Panel::open(&config::config_path()) {
        Ok(panel) => panel,
        Err(error) => {
            host.config_error = Some(format!("Could not open settings: {error:#}"));
            host.dirty = true;
            return;
        }
    };
    panel.wake = host.ui_wake.clone();
    panel.effective_font = host.font.primary_name().to_string();
    host.palette = None;
    host.theme_picker = None;
    host.transparency = None;
    host.space_picker = None;
    host.move_target = None;
    host.terminal_targets = None;
    host.space_panel = None;
    host.tab_rename = None;
    crate::close_find(&mut host.find);
    host.preedit.text.clear();
    host.preedit.cursor = None;
    host.settings = Some(panel);
    host.context_menu = Some(crate::palette::ContextMenu::new(
        crate::palette::ContextMenuKind::SpaceChip,
    ));
    host.context_menu_target = None;
    host.palette_layout = None;
    host.window.set_title("Prismattyc — settings");
    host.dirty = true;
    host.window.request_redraw();
}

pub(super) fn handle_key(host: &mut crate::HostState, event: &winit::event::KeyEvent) -> bool {
    let Some(panel) = host.settings.as_mut() else {
        return false;
    };
    let logical = crate::palette::typed_character_key(
        &event.key_without_modifiers(),
        &event.logical_key,
        event.text.as_deref(),
        host.modifiers,
    );
    let close = match panel.input_key(&logical, host.modifiers, event.repeat) {
        InputResult::Paste => {
            if let Ok(text) =
                arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text())
            {
                panel.paste(&text);
            }
            false
        }
        InputResult::Close => true,
        InputResult::Consumed => false,
    };
    if close {
        crate::close_context_menu(host);
    } else if let Some(menu) = host.context_menu.as_mut() {
        menu.selected = panel.selected();
    }
    host.dirty = true;
    host.window.request_redraw();
    true
}

pub(super) fn poll(host: &mut crate::HostState) {
    let Some(panel) = host.settings.as_mut() else {
        return;
    };
    let font_changed = panel.effective_font != host.font.primary_name();
    if font_changed {
        panel.effective_font = host.font.primary_name().to_string();
    }
    let (changed, disabled) = panel.poll();
    if disabled {
        crate::close_context_menu(host);
        crate::space_panel::settings(host);
        return;
    }
    if let Some(menu) = host.context_menu.as_mut() {
        menu.selected = panel.selected();
    }
    if changed || font_changed {
        host.dirty = true;
        host.window.request_redraw();
    }
}

#[cfg(test)]
mod tests;
