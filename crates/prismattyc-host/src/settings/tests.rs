// SPDX-License-Identifier: MIT
use super::*;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Fixture {
    dir: PathBuf,
    panel: Panel,
}
impl Fixture {
    fn new(extra: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "prism-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, format!("settings_panel = true\n{extra}\n")).unwrap();
        let panel = Panel::open(&path).unwrap();
        Self { dir, panel }
    }
    fn press(&mut self, key: NamedKey) {
        assert!(!self
            .panel
            .key(&Key::Named(key), ModifiersState::empty(), false));
    }
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.panel.pending.is_some() {
            self.panel.poll();
            assert!(Instant::now() < deadline, "writer timeout");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !self.panel.notice.starts_with("Could not save"),
            "{}",
            self.panel.notice
        );
    }
    fn editor_row(&mut self, predicate: impl Fn(&Command) -> bool) {
        let target = if let Focus::Editing(editor) = &self.panel.focus {
            self.panel
                .editor_rows(editor)
                .iter()
                .position(|row| predicate(&row.command))
                .expect("editor action exists")
        } else {
            panic!("expected editor")
        };
        let current = self.panel.selected();
        for _ in target..current {
            self.press(NamedKey::ArrowUp);
        }
        for _ in current..target {
            self.press(NamedKey::ArrowDown);
        }
        assert_eq!(self.panel.selected(), target);
    }
    fn open_key(&mut self, key: &str) {
        if matches!(self.panel.focus, Focus::Editing(_)) {
            self.press(NamedKey::Escape);
        }
        if !matches!(self.panel.focus, Focus::Sections) {
            while !matches!(self.panel.focus, Focus::Sections) {
                self.press(NamedKey::ArrowUp);
            }
        }
        for _ in 0..Section::ALL.len() {
            let indices = self.panel.indices();
            if let Some(row) = indices
                .iter()
                .position(|&index| self.panel.controls[index].key() == key)
            {
                self.press(NamedKey::ArrowDown);
                while let Focus::Rows(current) = self.panel.focus {
                    if current == 0 {
                        break;
                    }
                    self.press(NamedKey::ArrowUp);
                }
                if matches!(self.panel.focus, Focus::Sections) {
                    self.press(NamedKey::ArrowDown);
                }
                // Entering restores selection, so navigate from the real row.
                let current = match self.panel.focus {
                    Focus::Rows(row) => row,
                    _ => panic!("row focus"),
                };
                for _ in row..current {
                    self.press(NamedKey::ArrowUp);
                }
                for _ in current..row {
                    self.press(NamedKey::ArrowDown);
                }
                self.press(NamedKey::Enter);
                assert!(matches!(self.panel.focus, Focus::Editing(_)));
                return;
            }
            self.press(NamedKey::ArrowRight);
        }
        panic!("no reachable control for {key}");
    }
    fn text(&mut self, text: &str) {
        self.press(NamedKey::Enter);
        assert!(matches!(
            self.panel.focus,
            Focus::Editing(Editor {
                cursor: EditorFocus::Text { .. },
                ..
            })
        ));
        for ch in text.chars() {
            self.panel.key(
                &Key::Character(ch.to_string().into()),
                ModifiersState::empty(),
                false,
            );
        }
        self.press(NamedKey::Enter);
    }
    fn save(&mut self) {
        self.editor_row(|command| matches!(command, Command::Save));
        self.press(NamedKey::Enter);
        self.settle();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn schema_matches_controls_reached_through_real_rows_and_editors() {
    let mut fixture=Fixture::new("font_fallback = [\"bundled:ibm-plex-mono\"]\nfont_features=[\"calt\"]\n[[remote]]\nid=\"devbox\"\nssh=\"devbox\"");
    let expected: BTreeSet<_> = crate::config_schema::leaves()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let mut reached = BTreeSet::new();
    let mut ansi = BTreeSet::new();
    for _ in 0..Section::ALL.len() {
        fixture.press(NamedKey::ArrowDown);
        let count = fixture.panel.indices().len();
        for row in 0..count {
            let current = match fixture.panel.focus {
                Focus::Rows(row) => row,
                _ => panic!("row focus"),
            };
            for _ in row..current {
                fixture.press(NamedKey::ArrowUp);
            }
            for _ in current..row {
                fixture.press(NamedKey::ArrowDown);
            }
            let root_rows = fixture.panel.rows().1;
            assert_eq!(fixture.panel.selected(), row + 1);
            assert!(!root_rows[row + 1].name.is_empty());
            fixture.press(NamedKey::Enter);
            let needs_entry = matches!(&fixture.panel.focus,Focus::Editing(Editor{draft:Draft::Binding(values),..})if values.is_empty());
            if needs_entry {
                fixture.editor_row(|command| matches!(command, Command::Add));
                fixture.press(NamedKey::Enter);
            }
            let Focus::Editing(editor) = &fixture.panel.focus else {
                panic!("real editor opens")
            };
            let control = &fixture.panel.controls[editor.control];
            let visible = fixture.panel.rows().1;
            let editor_rows = fixture.panel.editor_rows(editor);
            assert_eq!(visible.len(), editor_rows.len());
            for (index, edited) in editor_rows.iter().enumerate() {
                assert!(!visible[index].name.is_empty());
                match (&editor.draft, &edited.command) {
                    (Draft::Fields(_), Command::Field(field)) => {
                        reached.insert(control.paths[*field].join("."));
                    }
                    (Draft::Strings(_) | Draft::Binding(_), Command::Field(_)) => {
                        reached.insert(control.key());
                    }
                    (Draft::Ansi(_), Command::Field(field)) => {
                        reached.insert(control.key());
                        ansi.insert(*field);
                    }
                    (Draft::Remote(_), Command::RemoteField(_, field)) => {
                        reached.insert(format!("remote[].{}", ["id", "ssh", "label"][*field]));
                    }
                    (Draft::Font(_), Command::Font(_)) | (Draft::Theme(_), Command::Theme(_)) => {
                        reached.insert(control.key());
                    }
                    _ => {}
                }
            }
            fixture.press(NamedKey::Escape);
        }
        while !matches!(fixture.panel.focus, Focus::Sections) {
            fixture.press(NamedKey::ArrowUp);
        }
        fixture.press(NamedKey::ArrowRight);
    }
    assert_eq!(reached, expected);
    assert_eq!(ansi, (0..16).collect());
}

#[test]
fn plain_arrows_separate_section_navigation_values_and_text_caret() {
    let mut fixture = Fixture::new("font_px=15.0\n");
    let original = std::fs::read_to_string(&fixture.panel.path).unwrap();
    fixture.press(NamedKey::ArrowRight);
    assert_eq!(fixture.panel.section, 1);
    fixture.press(NamedKey::ArrowLeft);
    assert_eq!(fixture.panel.section, 0);
    assert_eq!(
        std::fs::read_to_string(&fixture.panel.path).unwrap(),
        original
    );
    fixture.open_key("font_px");
    let section = fixture.panel.section;
    fixture.text("17.5");
    assert_eq!(fixture.panel.section, section);
    fixture.press(NamedKey::Enter); // exact-value text mode
    fixture.press(NamedKey::ArrowLeft);
    fixture.press(NamedKey::ArrowLeft);
    assert_eq!(fixture.panel.section, section);
    fixture.press(NamedKey::Escape); // cancel text
    fixture.press(NamedKey::Escape); // discard whole draft
    assert_eq!(
        std::fs::read_to_string(&fixture.panel.path).unwrap(),
        original
    );
    fixture.open_key("font_px");
    fixture.text("17.5");
    fixture.save();
    assert_eq!(
        config::load(&fixture.panel.path).unwrap().font_px,
        Some(17.5)
    );
    assert_eq!(fixture.panel.section, section);
}

fn json_at<'a>(root: &'a serde_json::Value, path: &[String]) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(root, |value, key| value.get(key))
}

#[test]
fn every_setting_saves_literal_values_through_input_and_loads_them() {
    let mut fixture = Fixture::new("");
    let example_text = include_str!("probe.toml")
        .replace(
            "\"/tmp/settings-background.png\"",
            &toml::Value::String(
                fixture
                    .dir
                    .join("background.png")
                    .to_string_lossy()
                    .into_owned(),
            )
            .to_string(),
        )
        .replace(
            "\"/tmp/settings-example.sock\"",
            &toml::Value::String(
                fixture
                    .dir
                    .join("example.sock")
                    .to_string_lossy()
                    .into_owned(),
            )
            .to_string(),
        );
    let example: toml::Value = example_text.parse().unwrap();
    config::parse(&example_text, &fixture.panel.path).unwrap();
    // The dialog can open directly while its entry-point gate is disabled.
    std::fs::write(&fixture.panel.path, "settings_panel = false\n").unwrap();
    fixture.panel.refresh();
    let mut saved = BTreeSet::new();
    for control in controls::catalog()
        .into_iter()
        .filter(|control| !matches!(control.kind, Kind::Binding(_)))
    {
        let key = control.key();
        fixture.open_key(&key);
        let value = crate::config_document::value_at(&example, &control.paths[0])
            .unwrap_or_else(|| panic!("independent literal missing for {key}"));
        match &control.kind {
            Kind::Font => {
                fixture.editor_row(|command|matches!(command,Command::Font(Some(id))if id=="bundled:ibm-plex-mono"));
                fixture.press(NamedKey::Enter);
            }
            Kind::Theme => {
                fixture
                    .editor_row(|command| matches!(command,Command::Theme(id)if id=="tokyo-night"));
                fixture.press(NamedKey::Enter);
            }
            Kind::Strings => {
                for (index, value) in value.as_array().unwrap().iter().enumerate() {
                    fixture.editor_row(|command| matches!(command, Command::Add));
                    fixture.press(NamedKey::Enter);
                    fixture.editor_row(
                        |command| matches!(command,Command::Field(field)if *field==index),
                    );
                    fixture.text(value.as_str().unwrap());
                }
            }
            Kind::Ansi => {
                for (index, value) in value.as_array().unwrap().iter().enumerate() {
                    fixture.editor_row(
                        |command| matches!(command,Command::Field(field)if *field==index),
                    );
                    fixture.text(value.as_str().unwrap());
                }
            }
            Kind::Remote => {
                fixture.editor_row(|command| matches!(command, Command::Add));
                fixture.press(NamedKey::Enter);
                for (field, value) in ["sample-box", "sample-host", "Sample destination"]
                    .iter()
                    .enumerate()
                {
                    fixture.editor_row(
                        |command| matches!(command,Command::RemoteField(0,index)if *index==field),
                    );
                    fixture.text(value);
                }
            }
            _ => {
                for (index, path) in control.paths.iter().enumerate() {
                    let value =
                        crate::config_document::value_at(&example, path).expect("literal field");
                    fixture.editor_row(
                        |command| matches!(command,Command::Field(field)if *field==index),
                    );
                    fixture.text(&display_value(value));
                }
            }
        }
        fixture.save();
        let raw: toml::Value = std::fs::read_to_string(&fixture.panel.path)
            .unwrap()
            .parse()
            .unwrap();
        let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
        for path in &control.paths {
            let expected = crate::config_document::value_at(&example, path).unwrap();
            assert_eq!(
                crate::config_document::value_at(&raw, path),
                Some(expected),
                "saved {key}"
            );
            assert_eq!(
                json_at(&loaded, path),
                Some(&serde_json::to_value(expected).unwrap()),
                "loaded {key}"
            );
            if key == "remote" {
                saved.extend(["remote[].id", "remote[].ssh", "remote[].label"].map(str::to_owned));
            } else {
                saved.insert(path.join("."));
            }
        }
        fixture.open_key(&key);
        fixture.editor_row(|command| matches!(command, Command::Reset));
        fixture.press(NamedKey::Enter);
        fixture.settle();
        let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
        for path in &control.paths {
            if key == "remote" {
                assert_eq!(loaded["remote"], serde_json::json!([]));
            } else {
                assert!(
                    json_at(&loaded, path).is_none_or(serde_json::Value::is_null),
                    "reset {key}"
                );
            }
        }
    }
    let expected: BTreeSet<_> = crate::config_schema::leaves()
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| !path.starts_with("keys."))
        .collect();
    assert_eq!(saved, expected);
}

#[test]
fn every_binding_captures_saves_disables_and_resets() {
    let mut fixture = Fixture::new("");
    for action in crate::keybind::Action::all() {
        let key = format!("keys.{}", action.name());
        // Start with an explicitly disabled action, so no default aliases remain.
        config::save_preference(
            &fixture.panel.path,
            "keys",
            toml_edit::value(toml_edit::InlineTable::new()),
        )
        .unwrap();
        prismattyc_mux::transact_config(&fixture.panel.path, |doc| {
            doc["keys"][action.name()] = toml_edit::value(toml_edit::Array::new());
            Ok(())
        })
        .unwrap();
        fixture.panel.refresh();
        fixture.open_key(&key);
        fixture.editor_row(|command| matches!(command, Command::Add));
        fixture.press(NamedKey::Enter);
        fixture.editor_row(|command| matches!(command, Command::Field(0)));
        fixture.press(NamedKey::Enter);
        fixture.panel.key(
            &Key::Named(NamedKey::F24),
            ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT,
            false,
        );
        fixture.save();
        let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
        assert_eq!(
            loaded["keys"][action.name()],
            serde_json::json!(["ctrl+shift+alt+f24"]),
            "{key}"
        );
        fixture.open_key(&key);
        fixture.editor_row(|command| matches!(command, Command::Remove(0)));
        fixture.press(NamedKey::Enter);
        fixture.save();
        let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
        assert_eq!(loaded["keys"][action.name()], serde_json::json!([]));
        fixture.open_key(&key);
        fixture.editor_row(|command| matches!(command, Command::Reset));
        fixture.press(NamedKey::Enter);
        fixture.settle();
        let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
        assert!(loaded["keys"].get(action.name()).is_none());
    }
}

#[test]
fn plain_arrows_reach_every_finite_choice() {
    let cases: &[(&str, &[&str])] = &[
        ("tab_strip", &["auto", "always", "multi"]),
        ("link_click", &["plain", "modifier"]),
        ("pane_titles", &["focused", "hover"]),
        ("chrome_style", &["classic", "graphite"]),
        ("bar_color", &["graphite", "harbor", "moss", "plum"]),
        ("layout", &["bars", "sidebar"]),
        ("render_timer", &["off", "osd", "log", "both"]),
        ("focus_border_animation", &["none", "light-cycle"]),
        ("space_rail", &["bottom", "left", "top", "right", "off"]),
        ("session_naming", &["ask", "auto", "blank"]),
        ("space_startup", &["ask", "restore", "fresh"]),
        ("toasts", &["all", "errors", "off"]),
        ("mux.space_open_runs_commands", &["agents", "all", "none"]),
        ("mux.remote_size", &["latest", "host"]),
    ];
    let mut fixture = Fixture::new("");
    let mut covered = BTreeSet::new();
    for &(key, expected) in cases {
        fixture.open_key(key);
        fixture.press(NamedKey::Escape);
        let mut actual = BTreeSet::new();
        for _ in 0..expected.len() {
            fixture.press(NamedKey::ArrowRight);
            fixture.settle();
            let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
            let path: Vec<_> = key.split('.').map(str::to_owned).collect();
            actual.insert(
                json_at(&loaded, &path)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        assert_eq!(
            actual,
            expected.iter().map(|value| value.to_string()).collect(),
            "{key}"
        );
        covered.insert(key.to_string());
    }
    let controls = controls::catalog()
        .into_iter()
        .filter(|control| matches!(control.kind, Kind::Choice(_)))
        .map(|control| control.key())
        .collect();
    assert_eq!(covered, controls);
}

#[test]
fn scrolling_and_pointer_hover_preserve_text_focus() {
    let mut fixture = Fixture::new("");
    fixture.panel.scroll_rows(4);
    assert_eq!(fixture.panel.selected(), 4);
    fixture.panel.scroll_rows(-2);
    assert_eq!(fixture.panel.selected(), 2);
    fixture.open_key("font_px");
    fixture.press(NamedKey::Enter);
    fixture.panel.hover(5);
    fixture.panel.scroll_rows(9);
    assert_eq!(fixture.panel.selected(), 0);
    assert!(matches!(
        fixture.panel.focus,
        Focus::Editing(Editor {
            cursor: EditorFocus::Text { .. },
            ..
        })
    ));
}

#[test]
fn reload_coalescing_settles_receipt_and_shows_latest_file() {
    let mut fixture = Fixture::new("font_px=15.0\n");
    fixture.open_key("font_px");
    fixture.text("18.5");
    fixture.editor_row(|command| matches!(command, Command::Save));
    fixture.press(NamedKey::Enter);
    // Wait for the commit without consuming its receipt, then supersede it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while config::load(&fixture.panel.path).unwrap().font_px != Some(18.5) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    config::save_preference(&fixture.panel.path, "font_px", toml_edit::value(22.0)).unwrap();
    config::save_preference(&fixture.panel.path, "pane_gap_px", toml_edit::value(13)).unwrap();
    fixture.panel.refresh(); // Only the final watcher snapshot is delivered.
    fixture.settle();
    assert_eq!(fixture.panel.config.font_px, Some(22.0));
    assert_eq!(fixture.panel.config.pane_gap_px, Some(13));
    assert_eq!(fixture.panel.values["font_px"], toml::Value::Float(22.0));
    assert!(fixture.panel.notice.contains("newer reload"));
}

#[test]
fn stale_draft_rejects_same_field_change_after_watcher_refresh() {
    let mut fixture = Fixture::new("font_px=15.0\n");
    fixture.open_key("font_px");
    fixture.text("18.5");
    config::save_preference(&fixture.panel.path, "font_px", toml_edit::value(22.0)).unwrap();
    fixture.panel.refresh();
    fixture.editor_row(|command| matches!(command, Command::Save));
    fixture.press(NamedKey::Enter);
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.panel.pending.is_some() {
        fixture.panel.poll();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        fixture.panel.notice.contains("changed in another editor"),
        "{}",
        fixture.panel.notice
    );
    assert_eq!(
        config::load(&fixture.panel.path).unwrap().font_px,
        Some(22.0)
    );
    assert!(
        matches!(&fixture.panel.focus,Focus::Editing(Editor{draft:Draft::Fields(values),..})if values==&vec![toml::Value::Float(18.5)])
    );
}

#[test]
fn compound_colors_validate_together_and_unrelated_external_edit_survives() {
    let mut fixture=Fixture::new("# keep root\nfuture_setting = { untouched = 73 }\n[theme_overrides]\n# preserve this explanation\nfuture_color = \"#aabbcc\"\n");
    fixture.open_key("theme_overrides.cursor_fg");
    fixture.text("#123456");
    fixture.editor_row(|command| matches!(command, Command::Field(1)));
    fixture.text("#654321");
    config::save_preference(&fixture.panel.path, "pane_gap_px", toml_edit::value(13)).unwrap();
    fixture.save();
    let loaded = config::load(&fixture.panel.path).unwrap();
    let theme = loaded.loaded_theme();
    assert_eq!(theme.cursor_fg, Some([0x12, 0x34, 0x56]));
    assert_eq!(theme.cursor_bg, Some([0x65, 0x43, 0x21]));
    assert_eq!(loaded.pane_gap_px, Some(13));
    fixture.open_key("theme_overrides.cursor_fg");
    fixture.editor_row(|command| matches!(command, Command::Reset));
    fixture.press(NamedKey::Enter);
    fixture.settle();
    let raw = std::fs::read_to_string(&fixture.panel.path).unwrap();
    assert!(raw.contains("# keep root"));
    assert!(raw.contains("# preserve this explanation"));
    assert!(raw.contains("future_color = \"#aabbcc\""));
    let parsed: toml::Value = raw.parse().unwrap();
    assert_eq!(
        parsed["future_setting"]["untouched"],
        toml::Value::Integer(73)
    );
    assert!(parsed["theme_overrides"].get("cursor_fg").is_none());
    assert!(parsed["theme_overrides"].get("cursor_bg").is_none());
}

#[test]
fn ordered_list_actions_keep_values_and_entry_comments() {
    let mut fixture=Fixture::new("font_features = [\n  # ligature explanation\n  \"liga\",\n  # contextual explanation\n  \"calt\",\n]\n");
    fixture.open_key("font_features");
    fixture.editor_row(|command| matches!(command, Command::Later(0)));
    fixture.press(NamedKey::Enter);
    fixture.editor_row(|command| matches!(command, Command::Field(0)));
    fixture.text("-calt");
    fixture.editor_row(|command| matches!(command, Command::Remove(1)));
    fixture.press(NamedKey::Enter);
    fixture.editor_row(|command| matches!(command, Command::Add));
    fixture.press(NamedKey::Enter);
    fixture.editor_row(|command| matches!(command, Command::Field(1)));
    fixture.text("ss01");
    fixture.save();
    assert_eq!(
        config::load(&fixture.panel.path).unwrap().font_features,
        Some(vec!["-calt".into(), "ss01".into()])
    );
    let raw = std::fs::read_to_string(&fixture.panel.path).unwrap();
    assert!(raw.contains("# ligature explanation"));
    assert!(raw.contains("# contextual explanation"));
}

#[test]
fn remote_edit_reorders_and_deletes_records_with_explicit_extension_ownership() {
    let mut fixture=Fixture::new("[[remote]]\nid=\"first\"\nssh=\"host-a\"\nfuture = { color=\"violet\" }\n[[remote]]\nid=\"second\"\nssh=\"host-b\"\nlabel=\"Old label\" # label explanation\n");
    fixture.open_key("remote");
    fixture.editor_row(|command| matches!(command, Command::Later(0)));
    fixture.press(NamedKey::Enter);
    fixture.editor_row(|command| matches!(command, Command::RemoteField(0, 2)));
    fixture.text("New label");
    fixture.save();
    let raw = std::fs::read_to_string(&fixture.panel.path).unwrap();
    let parsed: toml::Value = raw.parse().unwrap();
    assert_eq!(parsed["remote"][0]["id"].as_str(), Some("second"));
    assert_eq!(parsed["remote"][0]["label"].as_str(), Some("New label"));
    assert_eq!(
        parsed["remote"][1]["future"]["color"].as_str(),
        Some("violet")
    );
    assert!(raw.contains("# label explanation"));
    fixture.open_key("remote");
    fixture.editor_row(|command| matches!(command, Command::Remove(0)));
    fixture.press(NamedKey::Enter);
    fixture.save();
    let loaded = config::load(&fixture.panel.path).unwrap();
    let entries = loaded.remote.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "first");
    let raw: toml::Value = std::fs::read_to_string(&fixture.panel.path)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(raw["remote"][0]["future"]["color"].as_str(), Some("violet"));
}

#[test]
fn invalid_known_values_leave_disk_and_draft_intact() {
    for (key, input) in [
        ("font_px", "9999"),
        ("focus_border_animation_ms", "1"),
        ("theme_overrides.default_fg", "invalid-color"),
        ("font_features", "invalid-tag"),
    ] {
        let mut fixture = Fixture::new("");
        let before = std::fs::read_to_string(&fixture.panel.path).unwrap();
        fixture.open_key(key);
        if key == "font_features" {
            fixture.editor_row(|command| matches!(command, Command::Add));
            fixture.press(NamedKey::Enter);
            fixture.editor_row(|command| matches!(command, Command::Field(0)));
        }
        fixture.text(input);
        fixture.editor_row(|command| matches!(command, Command::Save));
        fixture.press(NamedKey::Enter);
        let deadline = Instant::now() + Duration::from_secs(5);
        while fixture.panel.pending.is_some() {
            fixture.panel.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            fixture.panel.notice.starts_with("Could not save:"),
            "{key}: {}",
            fixture.panel.notice
        );
        assert_eq!(
            std::fs::read_to_string(&fixture.panel.path).unwrap(),
            before,
            "{key}"
        );
        assert!(matches!(fixture.panel.focus, Focus::Editing(_)));
    }
}

#[test]
fn legacy_autosave_writer_keeps_inline_extensions_and_reset_keeps_comments() {
    let fixture=Fixture::new("spaces = { autosave = false, future_policy = \"keep\" } # inline explanation\n# font explanation\nfont_features = [\n  # entry explanation\n  \"liga\",\n]\n");
    config::save_spaces_autosave(&fixture.panel.path, true).unwrap();
    config::clear_preference(&fixture.panel.path, "font_features").unwrap();
    let raw = std::fs::read_to_string(&fixture.panel.path).unwrap();
    let values: toml::Value = raw.parse().unwrap();
    assert_eq!(values["spaces"]["autosave"], toml::Value::Boolean(true));
    assert_eq!(values["spaces"]["future_policy"].as_str(), Some("keep"));
    assert!(values.get("font_features").is_none());
    for comment in [
        "# inline explanation",
        "# font explanation",
        "# entry explanation",
    ] {
        assert!(raw.contains(comment), "lost {comment}: {raw}");
    }
}

#[test]
fn host_input_adapter_captures_paste_chord_and_pastes_only_in_text() {
    let mut fixture = Fixture::new("[keys]\ncopy = []\n");
    fixture.open_key("keys.copy");
    fixture.editor_row(|command| matches!(command, Command::Add));
    fixture.press(NamedKey::Enter);
    fixture.editor_row(|command| matches!(command, Command::Field(0)));
    fixture.press(NamedKey::Enter);
    assert_eq!(
        fixture
            .panel
            .input_key(&Key::Character("v".into()), ModifiersState::CONTROL, false),
        InputResult::Consumed
    );
    fixture.save();
    let loaded = serde_json::to_value(config::load(&fixture.panel.path).unwrap()).unwrap();
    assert_eq!(loaded["keys"]["copy"], serde_json::json!(["ctrl+v"]));
    fixture.open_key("walkthrough_voice");
    fixture.press(NamedKey::Enter);
    assert_eq!(
        fixture
            .panel
            .input_key(&Key::Character("v".into()), ModifiersState::CONTROL, false),
        InputResult::Paste
    );
    fixture.panel.paste("clipboard voice");
    fixture.press(NamedKey::Enter);
    fixture.save();
    assert_eq!(
        config::load(&fixture.panel.path)
            .unwrap()
            .walkthrough_voice
            .as_deref(),
        Some("clipboard voice")
    );
}

#[test]
fn host_reload_boundary_updates_saved_startup_values_before_effective_dedup() {
    let mut fixture = Fixture::new("async_file_writes=false\n");
    let running = config::load(&fixture.panel.path).unwrap();
    config::save_preference(
        &fixture.panel.path,
        "async_file_writes",
        toml_edit::value(true),
    )
    .unwrap();
    let newest = config::load(&fixture.panel.path).unwrap();
    assert!(
        crate::prepare_config_reload(newest, &running, std::iter::once(&mut fixture.panel))
            .is_none()
    );
    assert_eq!(fixture.panel.config.async_file_writes, Some(true));
    #[cfg(target_os = "macos")]
    {
        config::save_preference(
            &fixture.panel.path,
            "macos_present",
            toml_edit::value("iosurface"),
        )
        .unwrap();
        let newest = config::load(&fixture.panel.path).unwrap();
        assert!(crate::prepare_config_reload(
            newest,
            &running,
            std::iter::once(&mut fixture.panel)
        )
        .is_none());
        assert_eq!(
            fixture.panel.config.macos_present.as_deref(),
            Some("iosurface")
        );
    }
    config::save_preference(&fixture.panel.path, "font_px", toml_edit::value(22.0)).unwrap();
    let newest = config::load(&fixture.panel.path).unwrap();
    let effective =
        crate::prepare_config_reload(newest, &running, std::iter::once(&mut fixture.panel))
            .unwrap();
    assert_eq!(effective.font_px, Some(22.0));
    assert_eq!(effective.async_file_writes, Some(false));
    assert_eq!(fixture.panel.config.async_file_writes, Some(true));
}

#[test]
fn host_settings_entry_keeps_blocking_prompt_and_releases_rail_editor() {
    for (session, restore) in [(true, false), (false, true), (false, false)] {
        let mut rail = crate::space_rail::SpaceRail::new(Some("sample".into()));
        rail.focus = Some(0);
        rail.keyboard = true;
        rail.edit = Some(crate::space_rail::RailEdit {
            target: Some(0),
            buffer: "unsaved name".into(),
            selected: true,
        });
        let opened = prepare_entry(session, restore, &mut rail);
        if session || restore {
            assert!(!opened);
            assert_eq!(rail.edit.unwrap().buffer, "unsaved name");
        } else {
            assert!(opened);
            assert!(rail.edit.is_none());
            assert!(rail.focus.is_none());
            assert!(!rail.keyboard);
        }
        assert_eq!(rail.current.as_deref(), Some("sample"));
    }
}

#[test]
fn settings_painters_keep_selected_rows_visible_and_clickable_in_both_chromes() {
    use crate::raster::{
        FontMetrics, OverlaySurface, PaletteFrame, PaletteLayoutMode, PaletteSection,
    };
    let font = FontMetrics::load_with(15.0, None, &[]).unwrap();
    let theme = crate::theme::shipped_default();
    let mut fixture = Fixture::new("font_px=18.5\n");
    for editor in [false, true] {
        if editor {
            fixture.open_key("font_px");
        }
        let (header, rows) = fixture.panel.rows();
        assert_eq!(
            rows[fixture.panel.selected()].name,
            if editor { "Value: 18.5" } else { "Appearance" }
        );
        for graphite in [false, true] {
            let sections = [PaletteSection {
                header: &header,
                subtitle: "",
                rows: &rows,
            }];
            let frame = PaletteFrame {
                layout_mode: PaletteLayoutMode::FixedHeight,
                query: None,
                query_focused: false,
                chips: None,
                sections: &sections,
                selected: fixture.panel.selected(),
                scroll: 0,
                detail: None,
                footer: "↑/↓ rows · ←/→ section or value · Enter edit/reset · Escape back",
            };
            let (width, height) = (1200, 800);
            let mut pixels = vec![crate::raster::pack_argb(255, theme.default_bg); width * height];
            let layout = crate::paint_palette_overlay(
                &font,
                &theme,
                crate::mux::ChromeGeom {
                    graphite,
                    scale_milli: 1000,
                },
                [0x62, 0xa8, 0xff],
                &frame,
                OverlaySurface::default(),
                &mut pixels,
                width,
                height,
            )
            .expect("settings paints");
            let row = layout
                .rows
                .iter()
                .find(|row| row.global == fixture.panel.selected())
                .expect("selected setting is visible");
            assert_eq!(
                crate::raster::palette_hit(
                    &layout,
                    layout.panel_x + layout.panel_w / 2,
                    row.y + layout.geom.row_pitch_px / 2
                ),
                Some(fixture.panel.selected())
            );
            let mut blank_rows = rows.clone();
            blank_rows[fixture.panel.selected()].name.clear();
            let blank_sections = [PaletteSection {
                header: &header,
                subtitle: "",
                rows: &blank_rows,
            }];
            let blank_frame = PaletteFrame {
                sections: &blank_sections,
                ..frame
            };
            let mut blank_pixels =
                vec![crate::raster::pack_argb(255, theme.default_bg); width * height];
            crate::paint_palette_overlay(
                &font,
                &theme,
                crate::mux::ChromeGeom {
                    graphite,
                    scale_milli: 1000,
                },
                [0x62, 0xa8, 0xff],
                &blank_frame,
                OverlaySurface::default(),
                &mut blank_pixels,
                width,
                height,
            )
            .unwrap();
            let band = row.y * width..(row.y + layout.geom.row_pitch_px).min(height) * width;
            assert!(
                pixels[band.clone()] != blank_pixels[band],
                "selected label paints visible glyphs"
            );
            if let Some(dir) = std::env::var_os("PRISMATTYC_DUMP_SETTINGS") {
                let dir = PathBuf::from(dir);
                std::fs::create_dir_all(&dir).unwrap();
                crate::write_present_png(
                    &dir.join(format!(
                        "{}-{}.png",
                        if graphite { "graphite" } else { "classic" },
                        if editor { "font-size" } else { "sections" }
                    )),
                    &pixels,
                    width as u32,
                    height as u32,
                )
                .unwrap();
            }
        }
    }
}

#[test]
fn root_rows_show_saved_values_before_help_text() {
    let fixture = Fixture::new("focus_border_animation_ms=850\n");
    let rows = fixture.panel.rows().1;
    let row = rows
        .iter()
        .find(|row| row.name == "Focus border animation ms")
        .unwrap();
    assert!(
        row.describe.starts_with("850 · Live after config reload"),
        "{}",
        row.describe
    );
}

#[test]
fn stepping_an_extreme_integer_clamps_instead_of_overflowing() {
    let mut fixture = Fixture::new("");
    fixture.open_key("panes");
    fixture.text("9223372036854775807");
    fixture.press(NamedKey::ArrowRight);
    fixture.save();
    let saved = std::fs::read_to_string(&fixture.panel.path).unwrap();
    assert!(saved.contains("panes = 8"), "{saved}");

    fixture.open_key("panes");
    fixture.text("-9223372036854775808");
    fixture.press(NamedKey::ArrowLeft);
    fixture.save();
    let saved = std::fs::read_to_string(&fixture.panel.path).unwrap();
    assert!(saved.contains("panes = 1"), "{saved}");
}
