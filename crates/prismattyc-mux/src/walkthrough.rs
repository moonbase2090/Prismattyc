//! Shared walkthrough catalog, progress, and detectors (PT-192–198).
//!
//! Pure parse, validation, XDG progress load/save, `detect_step`, and the
//! boss snapshot matcher. Host caption geometry and `WalkthroughLive` stay
//! in `prismattyc-host`. No `HostState`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::layout_file::{stub_space_session, SavedSpace, SavedSpaceTab, SAVED_SPACE_VERSION};

const BUNDLED_LEVELS: &str = include_str!("../walkthrough/levels.toml");
const BUNDLED_BOSS: &str = include_str!("../walkthrough/boss.json");
const SPACE_EVENTS: &[&str] = &["saved", "opened", "boss_snapshot_match"];
/// Catalog `schema_version` this crate loads.
pub const SCHEMA_VERSION: u32 = 1;
/// Progress file `schema_version` this crate loads.
pub const PROGRESS_SCHEMA: u32 = 1;
/// Caption band is two lines. Line one is the imperative sentence.
pub const CAPTION_MAX: usize = 80;

/// Parsed walkthrough catalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    #[serde(default)]
    pub level: Vec<Level>,
}

/// One named level.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Level {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub step: Vec<Step>,
}

/// One step inside a level.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Step {
    pub id: String,
    pub caption: String,
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub audio: Option<String>,
    pub expect: Expect,
    #[serde(default)]
    pub show_me: Option<ShowMe>,
}

/// Detector a step waits for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expect {
    HostAction {
        action: String,
        #[serde(default)]
        result: Option<String>,
    },
    MuxEvent {
        event: String,
        #[serde(default)]
        to_window: Option<String>,
        #[serde(default)]
        session: Option<String>,
    },
    SpaceEvent {
        event: String,
    },
    CommandEvent {
        command: String,
        #[serde(default)]
        result: Option<String>,
    },
}

/// Action the Show me control may dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShowMe {
    HostAction { action: String },
    MuxEvent { event: String },
    SpaceEvent { event: String },
    CommandEvent { command: String },
}

/// Why a catalog failed to load or validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    Parse(String),
    SchemaVersion(u32),
    Empty,
    EmptyLevel(String),
    BadId { what: &'static str, id: String },
    DuplicateId(String),
    Caption(String),
    ShowMeMismatch(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "parse walkthrough catalog: {msg}"),
            Self::SchemaVersion(v) => write!(f, "unsupported walkthrough schema_version {v}"),
            Self::Empty => write!(f, "walkthrough catalog has no levels"),
            Self::EmptyLevel(id) => write!(f, "level {id} has no steps"),
            Self::BadId { what, id } => write!(f, "invalid {what} id {id:?}"),
            Self::DuplicateId(id) => write!(f, "duplicate walkthrough id {id}"),
            Self::Caption(id) => write!(f, "caption on {id} is empty, too long, or has a newline"),
            Self::ShowMeMismatch(id) => {
                write!(f, "show_me on {id} does not match the step detector")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

/// Load and validate the bundled `levels.toml`.
pub fn bundled_catalog() -> Result<Catalog, CatalogError> {
    parse_catalog(BUNDLED_LEVELS)
}

/// Parse TOML and apply the spike validation rules.
pub fn parse_catalog(toml: &str) -> Result<Catalog, CatalogError> {
    let catalog: Catalog = toml::from_str(toml).map_err(|e| CatalogError::Parse(e.to_string()))?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

/// Check IDs, captions, expect, and show_me pairing. No `HostState`.
pub fn validate_catalog(catalog: &Catalog) -> Result<(), CatalogError> {
    if catalog.schema_version != SCHEMA_VERSION {
        return Err(CatalogError::SchemaVersion(catalog.schema_version));
    }
    if catalog.level.is_empty() {
        return Err(CatalogError::Empty);
    }
    let mut seen = HashSet::new();
    for level in &catalog.level {
        check_id("level", &level.id)?;
        if !seen.insert(level.id.clone()) {
            return Err(CatalogError::DuplicateId(level.id.clone()));
        }
        if level.title.trim().is_empty() {
            return Err(CatalogError::BadId {
                what: "level title",
                id: level.id.clone(),
            });
        }
        if level.step.is_empty() {
            return Err(CatalogError::EmptyLevel(level.id.clone()));
        }
        for step in &level.step {
            check_id("step", &step.id)?;
            if !seen.insert(step.id.clone()) {
                return Err(CatalogError::DuplicateId(step.id.clone()));
            }
            if !caption_ok(&step.caption) {
                return Err(CatalogError::Caption(step.id.clone()));
            }
            if let Some(hint) = step.hint.as_deref() {
                if !caption_ok(hint) {
                    return Err(CatalogError::Caption(step.id.clone()));
                }
            }
            if let Expect::SpaceEvent { event } = &step.expect {
                if !SPACE_EVENTS.contains(&event.as_str()) {
                    return Err(CatalogError::BadId {
                        what: "space_event",
                        id: event.clone(),
                    });
                }
            }
            if let Some(show_me) = step.show_me.as_ref() {
                if !show_me_matches(&step.expect, show_me) {
                    return Err(CatalogError::ShowMeMismatch(step.id.clone()));
                }
            }
        }
    }
    Ok(())
}

fn check_id(what: &'static str, id: &str) -> Result<(), CatalogError> {
    let valid = id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && !id.contains("..")
        && !id.ends_with(['-', '.']);
    if valid {
        Ok(())
    } else {
        Err(CatalogError::BadId {
            what,
            id: id.to_string(),
        })
    }
}

fn caption_ok(text: &str) -> bool {
    !text.is_empty() && text.len() <= CAPTION_MAX && !text.contains('\n')
}

fn show_me_matches(expect: &Expect, show_me: &ShowMe) -> bool {
    match (expect, show_me) {
        (Expect::HostAction { action, .. }, ShowMe::HostAction { action: shown }) => {
            action == shown
        }
        (Expect::MuxEvent { event, .. }, ShowMe::MuxEvent { event: shown }) => event == shown,
        (Expect::SpaceEvent { event }, ShowMe::SpaceEvent { event: shown }) => event == shown,
        (Expect::CommandEvent { command, .. }, ShowMe::CommandEvent { command: shown }) => {
            command == shown
        }
        _ => false,
    }
}

/// Saved walkthrough progress (PT-195). Step IDs only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub schema_version: u32,
    pub current_level: String,
    pub current_step: String,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<String>,
    pub updated_at: String,
}

/// `$XDG_DATA_HOME/prismattyc/walkthrough.json`, else
/// `$HOME/.local/share/prismattyc/walkthrough.json`.
#[must_use]
pub fn progress_path() -> PathBuf {
    progress_path_from(crate::platform::data_home(), crate::platform::home_dir())
}

#[must_use]
pub fn progress_path_from(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let base = xdg
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| Path::new(&home).join(".local/share")));
    base.map_or_else(
        || PathBuf::from("prismattyc-walkthrough.json"),
        |base| base.join("prismattyc").join("walkthrough.json"),
    )
}

/// Missing or invalid file yields `None`. The file is left in place.
pub fn load_progress(path: &Path) -> Option<Progress> {
    let raw = fs::read_to_string(path).ok()?;
    let progress: Progress = serde_json::from_str(&raw).ok()?;
    (progress.schema_version == PROGRESS_SCHEMA).then_some(progress)
}

/// Atomic replace: temp in the same directory, flush, rename.
pub fn save_progress(path: &Path, progress: &Progress) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    let tmp = {
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    };
    let body = serde_json::to_vec_pretty(progress)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let write = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(&body)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Delete the progress file. Missing is success.
pub fn reset_progress(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// First catalog step that is neither completed nor skipped.
#[must_use]
pub fn resume_index(catalog: &Catalog, progress: &Progress) -> Option<(usize, usize)> {
    let done: HashSet<&str> = progress
        .completed
        .iter()
        .chain(progress.skipped.iter())
        .map(String::as_str)
        .collect();
    for (level_i, level) in catalog.level.iter().enumerate() {
        for (step_i, step) in level.step.iter().enumerate() {
            if !done.contains(step.id.as_str()) {
                return Some((level_i, step_i));
            }
        }
    }
    None
}

/// RFC3339 UTC timestamp with second precision. `now` is injected for tests.
#[must_use]
pub fn rfc3339_utc(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400) as u64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let h = tod / 3600;
    let min = (tod % 3600) / 60;
    let s = tod % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

/// In-memory walkthrough cursor plus completed/skipped IDs (PT-196).
/// Host wraps this with caption state. `pmux tutorial --play` uses it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub catalog: Catalog,
    pub level: usize,
    pub step: usize,
    pub completed: Vec<String>,
    pub skipped: Vec<String>,
}

impl Cursor {
    /// Resume at the first unrecorded step, or at `level_id` when set.
    /// All-done progress replays from step 0 (or the named level) and keeps
    /// the records. Unknown `level_id` yields `None`.
    #[must_use]
    pub fn resume(
        catalog: Catalog,
        progress: Option<&Progress>,
        level_id: Option<&str>,
    ) -> Option<Self> {
        if catalog.level.first()?.step.is_empty() {
            return None;
        }
        let (level, step) = if let Some(id) = level_id {
            let level = catalog.level.iter().position(|level| level.id == id)?;
            let step = progress
                .and_then(|progress| {
                    let done: HashSet<&str> = progress
                        .completed
                        .iter()
                        .chain(progress.skipped.iter())
                        .map(String::as_str)
                        .collect();
                    catalog.level[level]
                        .step
                        .iter()
                        .position(|step| !done.contains(step.id.as_str()))
                })
                .unwrap_or(0);
            (level, step)
        } else {
            progress
                .and_then(|progress| resume_index(&catalog, progress))
                .unwrap_or((0, 0))
        };
        let (completed, skipped) = match progress {
            Some(progress) => (progress.completed.clone(), progress.skipped.clone()),
            None => (Vec::new(), Vec::new()),
        };
        Some(Self {
            catalog,
            level,
            step,
            completed,
            skipped,
        })
    }

    #[must_use]
    pub fn current_level(&self) -> Option<&Level> {
        self.catalog.level.get(self.level)
    }

    #[must_use]
    pub fn current_step(&self) -> Option<&Step> {
        self.catalog.level.get(self.level)?.step.get(self.step)
    }

    fn current_id(&self) -> Option<String> {
        Some(self.current_step()?.id.clone())
    }

    fn record(&mut self, skipped: bool) {
        let Some(id) = self.current_id() else {
            return;
        };
        let list = if skipped {
            &mut self.skipped
        } else {
            &mut self.completed
        };
        if !list.contains(&id) {
            list.push(id);
        }
    }

    fn advance(&mut self) -> bool {
        let Some(level) = self.catalog.level.get(self.level) else {
            return false;
        };
        if self.step + 1 < level.step.len() {
            self.step += 1;
            return true;
        }
        if self.level + 1 < self.catalog.level.len() {
            self.level += 1;
            self.step = 0;
            return self.current_step().is_some();
        }
        false
    }

    /// Record the current step completed and advance. False means finished.
    pub fn complete(&mut self) -> bool {
        self.record(false);
        self.advance()
    }

    /// Record the current step skipped and advance. False means finished.
    pub fn skip(&mut self) -> bool {
        self.record(true);
        self.advance()
    }

    /// Snapshot for the progress file. `now` is injected for tests.
    #[must_use]
    pub fn snapshot(&self, now: SystemTime) -> Option<Progress> {
        let level = self.catalog.level.get(self.level)?;
        let step = self
            .current_step()
            .or_else(|| level.step.last())
            .or_else(|| {
                self.catalog
                    .level
                    .last()
                    .and_then(|level| level.step.last())
            })?;
        Some(Progress {
            schema_version: PROGRESS_SCHEMA,
            current_level: level.id.clone(),
            current_step: step.id.clone(),
            completed: self.completed.clone(),
            skipped: self.skipped.clone(),
            updated_at: rfc3339_utc(now),
        })
    }
}

/// A real host or mux result. Not a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    HostAction {
        action: String,
        result: String,
    },
    MuxEvent {
        event: String,
        to_window: Option<String>,
        session: Option<String>,
    },
    SpaceEvent {
        event: String,
    },
    CommandEvent {
        command: String,
        result: String,
    },
}

/// Match a catalog expectation against a real result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectOutcome {
    Advance,
    Finished,
    Fail,
    Ignore,
}

/// Compare one expectation to one observed fact. No `HostState`.
pub fn detect_step(expect: &Expect, fact: &Detected) -> DetectOutcome {
    match (expect, fact) {
        (
            Expect::HostAction { action, result },
            Detected::HostAction {
                action: got,
                result: got_result,
            },
        ) => {
            if action != got {
                return DetectOutcome::Ignore;
            }
            let want = result.as_deref().unwrap_or("ok");
            if got_result == want {
                DetectOutcome::Advance
            } else {
                DetectOutcome::Fail
            }
        }
        (
            Expect::MuxEvent {
                event,
                to_window,
                session,
            },
            Detected::MuxEvent {
                event: got,
                to_window: got_window,
                session: got_session,
            },
        ) => {
            if event != got {
                return DetectOutcome::Ignore;
            }
            if !opt_pred_match(to_window, got_window) || !opt_pred_match(session, got_session) {
                return DetectOutcome::Ignore;
            }
            DetectOutcome::Advance
        }
        (Expect::SpaceEvent { event }, Detected::SpaceEvent { event: got }) => {
            if event == got {
                DetectOutcome::Advance
            } else {
                DetectOutcome::Ignore
            }
        }
        (
            Expect::CommandEvent { command, result },
            Detected::CommandEvent {
                command: got,
                result: got_result,
            },
        ) => {
            if command != got {
                return DetectOutcome::Ignore;
            }
            let want = result.as_deref().unwrap_or("ok");
            if got_result == want {
                DetectOutcome::Advance
            } else {
                DetectOutcome::Fail
            }
        }
        _ => DetectOutcome::Ignore,
    }
}

fn opt_pred_match(want: &Option<String>, got: &Option<String>) -> bool {
    match want.as_deref() {
        None => true,
        Some(want) => got.as_deref() == Some(want),
    }
}

/// Map a pmuxd control-event kind to a detector fact.
pub fn mux_detected(event: &str, to_window: Option<&str>, session: Option<&str>) -> Detected {
    Detected::MuxEvent {
        event: event.to_string(),
        to_window: to_window.map(str::to_string),
        session: session.map(str::to_string),
    }
}

/// Scripted fact that satisfies one catalog expectation. Used by e2e fixtures.
#[must_use]
pub fn scripted_fact(expect: &Expect) -> Detected {
    match expect {
        Expect::HostAction { action, result } => Detected::HostAction {
            action: action.clone(),
            result: result.clone().unwrap_or_else(|| "ok".into()),
        },
        Expect::MuxEvent {
            event,
            to_window,
            session,
        } => Detected::MuxEvent {
            event: event.clone(),
            to_window: to_window.clone(),
            session: session.clone(),
        },
        Expect::SpaceEvent { event } => Detected::SpaceEvent {
            event: event.clone(),
        },
        Expect::CommandEvent { command, result } => Detected::CommandEvent {
            command: command.clone(),
            result: result.clone().unwrap_or_else(|| "ok".into()),
        },
    }
}

/// Boss snapshot comparison (PT-198). Session count, tab count, and
/// panes-per-tab multiset. Names, cwd, agents, and pids are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BossVerdict {
    Match,
    Mismatch(String),
}

pub fn bundled_boss() -> Result<SavedSpace, serde_json::Error> {
    serde_json::from_str(BUNDLED_BOSS)
}

fn panes_per_tab(space: &SavedSpace) -> Vec<usize> {
    if space.tabs.is_empty() {
        vec![1; space.sessions.len()]
    } else {
        space.tabs.iter().map(|tab| tab.sessions.len()).collect()
    }
}

#[must_use]
pub fn space_from_pane_counts(session_count: usize, panes_per_tab: &[usize]) -> SavedSpace {
    let mut sessions = Vec::new();
    let mut tabs = Vec::new();
    let mut n = 0usize;
    for (tab_i, panes) in panes_per_tab.iter().enumerate() {
        let mut names = Vec::new();
        for _ in 0..*panes {
            n += 1;
            let name = format!("seat-{n}");
            sessions.push(stub_space_session(&name));
            names.push(name);
        }
        tabs.push(SavedSpaceTab {
            title: format!("tab-{tab_i}"),
            sessions: names,
        });
    }
    while sessions.len() < session_count {
        n += 1;
        sessions.push(stub_space_session(format!("seat-{n}")));
    }
    SavedSpace {
        id: None,
        version: SAVED_SPACE_VERSION,
        created_at_unix_ms: None,
        saved_at_unix: 0,
        sessions,
        tabs,
        active_tab: 0,
        focused_session: None,
    }
}

/// Restrict a live space to the host arrangement. When `tabs` is present,
/// keep only sessions named in those tabs. When it is missing, drop the
/// leftover `default` session.
#[must_use]
pub fn scoped_boss_space(mut live: SavedSpace, tabs: Option<&[SavedSpaceTab]>) -> SavedSpace {
    match tabs {
        Some(tabs) if !tabs.is_empty() => {
            let mut named = Vec::new();
            for tab in tabs {
                for name in &tab.sessions {
                    if !named.iter().any(|existing| existing == name) {
                        named.push(name.clone());
                    }
                }
            }
            live.sessions
                .retain(|session| named.iter().any(|name| name == &session.name));
            for name in &named {
                if !live.sessions.iter().any(|session| &session.name == name) {
                    live.sessions.push(stub_space_session(name));
                }
            }
            live.tabs = tabs.to_vec();
            live
        }
        _ => {
            live.sessions.retain(|session| session.name != "default");
            live.tabs.clear();
            live
        }
    }
}

#[must_use]
pub fn boss_matches(target: &SavedSpace, live: &SavedSpace) -> BossVerdict {
    let want_seats = target.sessions.len();
    let got_seats = live.sessions.len();
    let want_tabs = if target.tabs.is_empty() {
        want_seats
    } else {
        target.tabs.len()
    };
    let got_tabs = if live.tabs.is_empty() {
        got_seats
    } else {
        live.tabs.len()
    };
    let want_ppt = {
        let mut v = panes_per_tab(target);
        v.sort_unstable();
        v
    };
    let got_ppt = {
        let mut v = panes_per_tab(live);
        v.sort_unstable();
        v
    };
    if want_seats == got_seats && want_tabs == got_tabs && want_ppt == got_ppt {
        return BossVerdict::Match;
    }
    let mut bits = Vec::new();
    if got_seats != want_seats {
        bits.push(format!("{got_seats} of {want_seats} seats"));
    }
    if got_ppt != want_ppt {
        let mut remain = want_ppt;
        let live_ppt = panes_per_tab(live);
        let mut named = false;
        for (index, count) in live_ppt.iter().enumerate() {
            if let Some(pos) = remain.iter().position(|want| want == count) {
                remain.remove(pos);
            } else if let Some(need) = remain.first() {
                bits.push(format!("tab {} needs {need} panes", index + 1));
                named = true;
                break;
            }
        }
        if !named {
            if let Some(need) = remain.first() {
                bits.push(format!("need a tab with {need} panes"));
            }
        }
    } else if got_tabs != want_tabs {
        bits.push(format!("{got_tabs} of {want_tabs} tabs"));
    }
    BossVerdict::Mismatch(bits.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap_window(body: &str) -> String {
        format!(
            r#"
schema_version = 1
[[level]]
id = "window"
title = "The window"
{body}
"#
        )
    }

    fn progress_fixture_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pt-196-progress-{}-{}-{}",
            std::process::id(),
            tag,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("walkthrough.json")
    }

    #[test]
    fn bundled_catalog_loads_levels_0_to_4() {
        let catalog = bundled_catalog().expect("bundled levels.toml");
        let ids: Vec<_> = catalog
            .level
            .iter()
            .map(|level| level.id.as_str())
            .collect();
        assert_eq!(ids, ["window", "tabs", "seats", "spaces", "power", "boss"]);
        let steps: Vec<_> = catalog
            .level
            .iter()
            .flat_map(|level| level.step.iter().map(|step| step.id.as_str()))
            .collect();
        assert_eq!(
            steps,
            [
                "window.split-right",
                "window.focus-right",
                "window.close-pane",
                "tabs.new",
                "tabs.rename",
                "tabs.move-pane",
                "tabs.drag",
                "seats.new",
                "seats.attach-all",
                "seats.mail",
                "spaces.save",
                "spaces.open",
                "spaces.reopen",
                "power.palette",
                "power.preset",
                "power.zoom",
                "power.find",
                "boss.reproduce-space",
            ]
        );
    }

    #[test]
    fn unknown_expect_kind_fails_before_runtime() {
        let toml = wrap_window(
            r#"
[[level.step]]
id = "window.split-right"
caption = "Split the pane to the right."
expect = { kind = "key_press", action = "split_right" }
"#,
        );
        match parse_catalog(&toml) {
            Err(CatalogError::Parse(_)) => {}
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_step_id_fails() {
        let toml = wrap_window(
            r#"
[[level.step]]
id = "window.split-right"
caption = "Split the pane to the right."
expect = { kind = "host_action", action = "split_right", result = "ok" }
[[level.step]]
id = "window.split-right"
caption = "Split again."
expect = { kind = "host_action", action = "split_down", result = "ok" }
"#,
        );
        assert_eq!(
            parse_catalog(&toml),
            Err(CatalogError::DuplicateId("window.split-right".into()))
        );
    }

    #[test]
    fn empty_level_fails() {
        let toml = r#"
schema_version = 1
[[level]]
id = "window"
title = "The window"
"#;
        assert_eq!(
            parse_catalog(toml),
            Err(CatalogError::EmptyLevel("window".into()))
        );
    }

    #[test]
    fn show_me_must_match_expect() {
        let toml = wrap_window(
            r#"
[[level.step]]
id = "window.split-right"
caption = "Split the pane to the right."
expect = { kind = "host_action", action = "split_right", result = "ok" }
show_me = { kind = "host_action", action = "split_down" }
"#,
        );
        assert_eq!(
            parse_catalog(&toml),
            Err(CatalogError::ShowMeMismatch("window.split-right".into()))
        );
    }

    #[test]
    fn bad_id_and_long_caption_fail() {
        let bad_id = wrap_window(
            r#"
[[level.step]]
id = "Window Split"
caption = "Split the pane to the right."
expect = { kind = "host_action", action = "split_right", result = "ok" }
"#,
        );
        assert!(matches!(
            parse_catalog(&bad_id),
            Err(CatalogError::BadId { what: "step", .. })
        ));
        let long = "S".repeat(CAPTION_MAX + 1);
        let long_cap = wrap_window(&format!(
            r#"
[[level.step]]
id = "window.split-right"
caption = "{long}"
expect = {{ kind = "host_action", action = "split_right", result = "ok" }}
"#
        ));
        assert_eq!(
            parse_catalog(&long_cap),
            Err(CatalogError::Caption("window.split-right".into()))
        );
    }

    #[test]
    fn wrong_schema_version_fails() {
        let toml = r#"
schema_version = 2
[[level]]
id = "window"
title = "The window"
[[level.step]]
id = "window.split-right"
caption = "Split the pane to the right."
expect = { kind = "host_action", action = "split_right", result = "ok" }
"#;
        assert_eq!(parse_catalog(toml), Err(CatalogError::SchemaVersion(2)));
    }

    #[test]
    fn unknown_space_event_fails_before_runtime() {
        let toml = wrap_window(
            r#"
[[level.step]]
id = "window.split-right"
caption = "Split the pane to the right."
expect = { kind = "space_event", event = "teleport" }
"#,
        );
        assert_eq!(
            parse_catalog(&toml),
            Err(CatalogError::BadId {
                what: "space_event",
                id: "teleport".into(),
            })
        );
    }

    #[test]
    fn mux_session_created_current_matches() {
        let expect = Expect::MuxEvent {
            event: "SessionCreated".into(),
            to_window: None,
            session: Some("current".into()),
        };
        assert_eq!(
            detect_step(
                &expect,
                &mux_detected("SessionCreated", None, Some("current"))
            ),
            DetectOutcome::Advance
        );
        assert_eq!(
            detect_step(&expect, &mux_detected("PaneMoved", None, Some("current"))),
            DetectOutcome::Ignore
        );
        assert_eq!(
            detect_step(
                &expect,
                &mux_detected("SessionCreated", None, Some("other"))
            ),
            DetectOutcome::Ignore
        );
        assert_eq!(
            detect_step(
                &Expect::MuxEvent {
                    event: "PaneMoved".into(),
                    to_window: Some("current".into()),
                    session: None,
                },
                &mux_detected("PaneMoved", Some("current"), None)
            ),
            DetectOutcome::Advance
        );
    }

    #[test]
    fn command_and_space_expect_match() {
        let command = Expect::CommandEvent {
            command: "pmux_new".into(),
            result: Some("ok".into()),
        };
        assert_eq!(
            detect_step(
                &command,
                &Detected::CommandEvent {
                    command: "pmux_new".into(),
                    result: "ok".into(),
                }
            ),
            DetectOutcome::Advance
        );
        assert_eq!(
            detect_step(
                &command,
                &Detected::CommandEvent {
                    command: "pmux_new".into(),
                    result: "err".into(),
                }
            ),
            DetectOutcome::Fail
        );
        assert_eq!(
            detect_step(
                &Expect::SpaceEvent {
                    event: "saved".into(),
                },
                &Detected::SpaceEvent {
                    event: "saved".into(),
                }
            ),
            DetectOutcome::Advance
        );
    }

    #[test]
    fn rfc3339_utc_pins_epoch_and_a_known_instant() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000)),
            "2001-09-09T01:46:40Z"
        );
    }

    #[test]
    fn progress_path_prefers_xdg_then_home() {
        assert_eq!(
            progress_path_from(Some("/xdg".into()), Some("/home/me".into())),
            PathBuf::from("/xdg/prismattyc/walkthrough.json")
        );
        assert_eq!(
            progress_path_from(None, Some("/home/me".into())),
            PathBuf::from("/home/me/.local/share/prismattyc/walkthrough.json")
        );
    }

    #[test]
    fn progress_round_trip_and_atomic_write_leaves_no_temp() {
        let path = progress_fixture_path("round");
        let saved = Progress {
            schema_version: PROGRESS_SCHEMA,
            current_level: "window".into(),
            current_step: "window.focus-right".into(),
            completed: vec!["window.split-right".into()],
            skipped: Vec::new(),
            updated_at: rfc3339_utc(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        };
        save_progress(&path, &saved).unwrap();
        assert!(path.is_file());
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(!PathBuf::from(&tmp).exists(), "temp must not remain");
        let loaded = load_progress(&path).unwrap();
        assert_eq!(loaded, saved);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn invalid_progress_file_is_ignored_and_kept() {
        let path = progress_fixture_path("bad");
        fs::write(&path, "{not json").unwrap();
        assert!(load_progress(&path).is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn resume_index_skips_completed_and_skipped_ids() {
        let catalog = bundled_catalog().unwrap();
        let progress = Progress {
            schema_version: 1,
            current_level: "window".into(),
            current_step: "window.split-right".into(),
            completed: vec!["window.split-right".into()],
            skipped: vec!["window.focus-right".into()],
            updated_at: "1970-01-01T00:00:00Z".into(),
        };
        let (level, step) = resume_index(&catalog, &progress).unwrap();
        assert_ne!(catalog.level[level].step[step].id, "window.split-right");
        assert_ne!(catalog.level[level].step[step].id, "window.focus-right");
        assert_eq!(catalog.level[level].step[step].id, "window.close-pane");
    }

    #[test]
    fn reset_progress_deletes_or_ignores_missing() {
        let path = progress_fixture_path("reset");
        let saved = Progress {
            schema_version: PROGRESS_SCHEMA,
            current_level: "window".into(),
            current_step: "window.split-right".into(),
            completed: Vec::new(),
            skipped: vec!["window.split-right".into()],
            updated_at: rfc3339_utc(UNIX_EPOCH),
        };
        save_progress(&path, &saved).unwrap();
        reset_progress(&path).unwrap();
        assert!(!path.exists());
        reset_progress(&path).unwrap();
        assert!(load_progress(&path).is_none());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cursor_resume_skip_complete_and_snapshot() {
        let catalog = bundled_catalog().unwrap();
        let mut cursor = Cursor::resume(catalog.clone(), None, None).unwrap();
        assert_eq!(cursor.current_step().unwrap().id, "window.split-right");
        let first = cursor.current_step().unwrap().id.clone();
        assert!(cursor.skip());
        assert_eq!(cursor.current_step().unwrap().id, "window.focus-right");
        assert_eq!(cursor.skipped, [first.as_str()]);
        assert!(cursor.complete());
        let saved = cursor.snapshot(UNIX_EPOCH).unwrap();
        assert_eq!(saved.completed, ["window.focus-right"]);
        let again = Cursor::resume(catalog, Some(&saved), None).unwrap();
        assert_eq!(again.current_step().unwrap().id, "window.close-pane");
        assert!(again.skipped.contains(&first));
    }

    #[test]
    fn cursor_resume_named_level_and_unknown_id() {
        let catalog = bundled_catalog().unwrap();
        let cursor = Cursor::resume(catalog.clone(), None, Some("tabs")).unwrap();
        assert_eq!(cursor.current_level().unwrap().id, "tabs");
        assert_eq!(cursor.current_step().unwrap().id, "tabs.new");
        assert!(Cursor::resume(catalog, None, Some("missing")).is_none());
    }

    #[test]
    fn boss_fixture_matches_three_seat_shape() {
        let target = bundled_boss().expect("boss.json");
        assert_eq!(target.sessions.len(), 3);
        assert_eq!(panes_per_tab(&target), [2, 1]);
        assert_eq!(boss_matches(&target, &target), BossVerdict::Match);
        let live = space_from_pane_counts(3, &[1, 2]);
        assert_eq!(boss_matches(&target, &live), BossVerdict::Match);
    }

    #[test]
    fn boss_two_seats_and_wrong_split_mismatch() {
        let target = bundled_boss().expect("boss.json");
        let two = space_from_pane_counts(2, &[1, 1]);
        assert_eq!(
            boss_matches(&target, &two),
            BossVerdict::Mismatch("2 of 3 seats; tab 2 needs 2 panes".into())
        );
        let split = space_from_pane_counts(3, &[1, 1, 1]);
        assert_eq!(
            boss_matches(&target, &split),
            BossVerdict::Mismatch("tab 2 needs 2 panes".into())
        );
    }

    #[test]
    fn scoped_boss_space_keeps_attach_tab_sessions() {
        let live = space_from_pane_counts(5, &[1, 1, 1, 1, 1]);
        assert_eq!(live.sessions.len(), 5);
        let tabs = vec![
            SavedSpaceTab {
                title: "pair".into(),
                sessions: vec!["seat-1".into(), "seat-2".into()],
            },
            SavedSpaceTab {
                title: "solo".into(),
                sessions: vec!["seat-3".into()],
            },
        ];
        let scoped = scoped_boss_space(live, Some(&tabs));
        assert_eq!(scoped.sessions.len(), 3);
        assert_eq!(
            boss_matches(&bundled_boss().unwrap(), &scoped),
            BossVerdict::Match
        );
        let with_default = space_from_pane_counts(5, &[1, 1, 1, 1, 1]);
        let mut with_default = with_default;
        with_default.sessions[0].name = "default".into();
        let fallback = scoped_boss_space(with_default, None);
        assert_eq!(fallback.sessions.len(), 4);
        assert!(fallback.tabs.is_empty());
    }
}
