//! Live `[mux] experimental_activity`: pmuxd watches the config file and
//! starts or stops the activity feed without a restart.
//!
//! The watcher polls the file's metadata (modification time and length, plus
//! change time and inode on Unix). On a change it re-reads `[mux]` and applies
//! `flag || experimental_activity`. Turning the feed off drops its ring, so
//! readers and publishers get `activity_disabled`; turning it on starts a fresh
//! feed with a new boot epoch. Sessions, panes and mail are never touched.
//!
//! Only a readable file with an explicit value changes the feed. A missing,
//! empty, unreadable or invalid file keeps the current state, as the host's own
//! config watcher does for a save in progress, and is retried on the next poll
//! until it reads cleanly.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::config::{parse_mux_section, MuxSection};
use crate::control::ControlPlane;

/// How often pmuxd checks the config file.
pub const POLL: Duration = Duration::from_millis(500);

/// What a config file looked like at one check. `None` means it was missing.
/// On Unix the change time and inode catch an edit that restores the old
/// modification time and length, and an atomic replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(Option<Stamp>);

/// Modification time, length, and the Unix change time and inode (zero elsewhere).
type Stamp = (Option<SystemTime>, u64, (i64, i64, u64));

impl Fingerprint {
    pub fn of(path: &Path) -> Self {
        Self(
            std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.modified().ok(), meta.len(), change_stamp(&meta))),
        )
    }
}

#[cfg(unix)]
fn change_stamp(meta: &std::fs::Metadata) -> (i64, i64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.ctime(), meta.ctime_nsec(), meta.ino())
}

#[cfg(not(unix))]
fn change_stamp(_meta: &std::fs::Metadata) -> (i64, i64, u64) {
    (0, 0, 0)
}

/// Why a config check left the feed as it was.
#[derive(Debug, PartialEq, Eq)]
enum Pending {
    /// Missing or empty: likely a save in progress. Not logged.
    NoValue,
    /// Unreadable, invalid, or the feed failed to start. Logged once per file state.
    Failed(String),
}

/// The feed state a config read asks for. `None` keeps the current state.
pub fn wanted(flag: bool, file: Result<&MuxSection, &anyhow::Error>) -> Option<bool> {
    if flag {
        return Some(true);
    }
    file.ok().map(MuxSection::experimental_activity)
}

/// Background watcher; dropping it stops the thread.
pub struct Watcher {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// `seen` is the fingerprint taken before pmuxd's own startup read, so an
    /// edit between that read and this start is still applied.
    pub fn start(
        plane: Arc<Mutex<ControlPlane>>,
        config: PathBuf,
        flag: bool,
        seen: Fingerprint,
        poll: Duration,
    ) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pmux-activity-config".into())
            .spawn(move || {
                let mut seen = seen;
                let mut reported: Option<Fingerprint> = None;
                loop {
                    let now = Fingerprint::of(&config);
                    if now != seen {
                        // `seen` advances only once the file applied, so a
                        // failure is retried on the next poll.
                        match apply(&plane, &config, flag) {
                            Ok(()) => {
                                seen = now;
                                reported = None;
                            }
                            Err(Pending::NoValue) => {}
                            Err(Pending::Failed(why)) => {
                                if reported.as_ref() != Some(&now) {
                                    eprintln!("pmuxd: activity feed unchanged: {why}");
                                    reported = Some(now);
                                }
                            }
                        }
                    }
                    if !matches!(
                        receiver.recv_timeout(poll),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read the config and bring the feed to the state it asks for.
fn apply(plane: &Arc<Mutex<ControlPlane>>, config: &Path, flag: bool) -> Result<(), Pending> {
    let raw = match std::fs::read_to_string(config) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(Pending::NoValue),
        Err(error) => {
            return Err(Pending::Failed(format!(
                "read {}: {error}",
                config.display()
            )))
        }
    };
    if raw.trim().is_empty() {
        return Err(Pending::NoValue);
    }
    let file = parse_mux_section(&raw, config);
    let Some(on) = wanted(flag, file.as_ref()) else {
        let why = file
            .err()
            .map_or_else(String::new, |error| format!("{error:#}"));
        return Err(Pending::Failed(why));
    };
    let mut plane = plane
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if on == plane.activity_enabled() {
        return Ok(());
    }
    if on {
        plane
            .enable_activity()
            .map_err(|error| Pending::Failed(format!("feed could not start: {}", error.message)))?;
        eprintln!("pmuxd: activity feed on ([mux] experimental_activity)");
    } else {
        plane.disable_activity();
        eprintln!("pmuxd: activity feed off ([mux] experimental_activity)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(on: Option<bool>) -> MuxSection {
        MuxSection {
            experimental_activity: on,
            ..MuxSection::default()
        }
    }

    #[test]
    fn the_flag_keeps_the_feed_on_whatever_the_config_says() {
        let off = section(Some(false));
        assert_eq!(wanted(true, Ok(&off)), Some(true));
        let broken = anyhow::anyhow!("parse error");
        assert_eq!(wanted(true, Err(&broken)), Some(true));
    }

    #[test]
    fn the_config_key_decides_without_the_flag() {
        assert_eq!(wanted(false, Ok(&section(Some(true)))), Some(true));
        assert_eq!(wanted(false, Ok(&section(Some(false)))), Some(false));
        assert_eq!(wanted(false, Ok(&section(None))), Some(false));
    }

    #[test]
    fn an_unreadable_config_keeps_the_current_state() {
        let broken = anyhow::anyhow!("parse error");
        assert_eq!(wanted(false, Err(&broken)), None);
    }

    #[test]
    fn a_fingerprint_changes_with_length_and_presence() {
        let dir = std::env::temp_dir().join(format!("pmux-actcfg-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let missing = Fingerprint::of(&path);
        assert_eq!(missing, Fingerprint(None));
        std::fs::write(&path, "[mux]\n").unwrap();
        let first = Fingerprint::of(&path);
        assert_ne!(first, missing);
        std::fs::write(&path, "[mux]\nexperimental_activity = true\n").unwrap();
        assert_ne!(Fingerprint::of(&path), first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
