//! Live `[mux] experimental_activity`: pmuxd watches the config file and
//! starts or stops the activity feed without a restart.
//!
//! The watcher polls the file's modification time and length. On a change it
//! re-reads `[mux]` and applies `flag || experimental_activity`. Turning the
//! feed off drops its ring, so readers and publishers get `activity_disabled`;
//! turning it on starts a fresh feed with a new boot epoch. Sessions, panes and
//! mail are never touched. A config that cannot be read or parsed keeps the
//! current state, so a half-written save never flips the feed.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::config::{load_mux_section, MuxSection};
use crate::control::ControlPlane;

/// How often pmuxd checks the config file.
pub const POLL: Duration = Duration::from_millis(500);

/// What a config file looked like at one check. `None` means it was missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(Option<(SystemTime, u64)>);

impl Fingerprint {
    pub fn of(path: &Path) -> Self {
        Self(
            std::fs::metadata(path)
                .ok()
                .and_then(|meta| Some((meta.modified().ok()?, meta.len()))),
        )
    }
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
                loop {
                    let now = Fingerprint::of(&config);
                    if now != seen {
                        seen = now;
                        apply(&plane, &config, flag);
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

fn apply(plane: &Arc<Mutex<ControlPlane>>, config: &Path, flag: bool) {
    let file = load_mux_section(config);
    let Some(on) = wanted(flag, file.as_ref()) else {
        if let Err(error) = &file {
            eprintln!("pmuxd: activity feed unchanged; config not applied: {error:#}");
        }
        return;
    };
    let mut plane = plane
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if on == plane.activity_enabled() {
        return;
    }
    if on {
        match plane.enable_activity() {
            Ok(()) => eprintln!("pmuxd: activity feed on ([mux] experimental_activity)"),
            Err(error) => eprintln!("pmuxd: activity feed could not start: {}", error.message),
        }
    } else {
        plane.disable_activity();
        eprintln!("pmuxd: activity feed off ([mux] experimental_activity)");
    }
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
