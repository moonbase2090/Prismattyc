//! Live `[mux] experimental_activity`: pmuxd watches the config file and
//! starts or stops the activity feed without a restart.
//!
//! The watcher polls the file's metadata (modification time and length, plus
//! change time and inode on Unix). On a change it re-reads `[mux]` and applies
//! `flag || experimental_activity`, where a key left out means on. Turning
//! the feed off drops its ring, so readers and publishers get
//! `activity_disabled`; turning it on starts a fresh feed with a new boot
//! epoch. Sessions, panes and mail are never touched.
//!
//! Only a readable, non-empty file changes the feed. A missing,
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
    file.ok().map(|file| {
        file.experimental_activity()
            || (file.activity_webhook_url.is_some() && file.activity_webhook_secret_file.is_some())
    })
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
    if let Ok(section) = file.as_ref() {
        if section.activity_webhook_url.is_some()
            || section.activity_webhook_secret_file.is_some()
            || section.activity_webhook_secret.is_some()
        {
            crate::activity_webhook::WebhookConfig::from_mux(section)
                .map_err(|error| Pending::Failed(error.to_string()))?;
        }
    }
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
            activity_webhook_url: None,
            activity_webhook_secret_file: None,
            activity_webhook_secret: None,
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
        assert_eq!(
            wanted(false, Ok(&section(None))),
            Some(true),
            "a key left out is the default, on"
        );
    }

    #[test]
    fn webhook_is_opt_in_and_enables_the_feed_only_with_both_settings() {
        let mut config = section(Some(false));
        assert_eq!(wanted(false, Ok(&config)), Some(false));
        config.activity_webhook_url = Some("https://example.invalid/activity".into());
        assert_eq!(wanted(false, Ok(&config)), Some(false));
        config.activity_webhook_secret_file = Some("/private/key".into());
        assert_eq!(wanted(false, Ok(&config)), Some(true));
    }

    #[test]
    fn an_unreadable_config_keeps_the_current_state() {
        let broken = anyhow::anyhow!("parse error");
        assert_eq!(wanted(false, Err(&broken)), None);
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pmux-actcfg-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn config(&self) -> PathBuf {
            self.0.join("config.toml")
        }

        fn write(&self, text: &str) {
            std::fs::write(self.config(), text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn plane() -> Arc<Mutex<ControlPlane>> {
        let domain = crate::domain::Domain::bootstrap("test").unwrap();
        let window = domain.sessions().next().unwrap().windows[0];
        let plane = ControlPlane::new(
            domain,
            [crate::control::WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(8),
        )
        .unwrap();
        Arc::new(Mutex::new(plane))
    }

    fn on(plane: &Arc<Mutex<ControlPlane>>) -> bool {
        plane.lock().unwrap().activity_enabled()
    }

    #[test]
    fn apply_turns_the_feed_on_and_off_from_the_key() {
        let tmp = Scratch::new("apply");
        let plane = plane();
        tmp.write("[mux]\nexperimental_activity = true\n");
        assert_eq!(apply(&plane, &tmp.config(), false), Ok(()));
        assert!(on(&plane));
        // Already on: nothing to do, still applied.
        assert_eq!(apply(&plane, &tmp.config(), false), Ok(()));
        assert!(on(&plane));
        tmp.write("[mux]\nexperimental_activity = false\n");
        assert_eq!(apply(&plane, &tmp.config(), false), Ok(()));
        assert!(!on(&plane));
        tmp.write("[mux]\n");
        assert_eq!(apply(&plane, &tmp.config(), false), Ok(()));
        assert!(on(&plane), "an absent key is the default, on");
    }

    #[test]
    fn apply_keeps_the_state_for_a_missing_empty_or_invalid_file() {
        let tmp = Scratch::new("keep");
        let plane = plane();
        plane.lock().unwrap().enable_activity().unwrap();
        assert_eq!(apply(&plane, &tmp.config(), false), Err(Pending::NoValue));
        tmp.write("  \n");
        assert_eq!(apply(&plane, &tmp.config(), false), Err(Pending::NoValue));
        tmp.write("[mux\nexperimental_activity = false\n");
        assert!(matches!(
            apply(&plane, &tmp.config(), false),
            Err(Pending::Failed(why)) if why.contains("parse")
        ));
        assert!(on(&plane), "the feed stays on through all three");
    }

    #[test]
    fn apply_reports_an_unreadable_path() {
        let tmp = Scratch::new("dir");
        // A directory where the file should be cannot be read as text.
        std::fs::create_dir_all(tmp.config()).unwrap();
        let plane = plane();
        assert!(matches!(
            apply(&plane, &tmp.config(), false),
            Err(Pending::Failed(why)) if why.starts_with("read ")
        ));
        assert!(!on(&plane));
    }

    #[test]
    fn apply_with_the_flag_turns_the_feed_on_even_when_the_key_is_false() {
        let tmp = Scratch::new("flag");
        let plane = plane();
        tmp.write("[mux]\nexperimental_activity = false\n");
        assert_eq!(apply(&plane, &tmp.config(), true), Ok(()));
        assert!(on(&plane));
    }

    #[test]
    fn the_watcher_applies_a_repaired_file_after_a_broken_one() {
        let tmp = Scratch::new("retry");
        let plane = plane();
        tmp.write("[mux\nbroken");
        let seen = Fingerprint(None);
        let watcher = Watcher::start(
            Arc::clone(&plane),
            tmp.config(),
            false,
            seen,
            Duration::from_millis(20),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!on(&plane));
        tmp.write("[mux]\nexperimental_activity = true\n");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !on(&plane) {
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never applied the repaired file"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(watcher);
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
