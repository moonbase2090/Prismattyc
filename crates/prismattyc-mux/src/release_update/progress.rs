// SPDX-License-Identifier: MIT
//! Staged progress for `pmux update --progress`.
//!
//! Each event is one JSON object on its own stderr line, after [`PREFIX`], so
//! a reader can tell progress from ordinary messages. Stdout keeps the final
//! report unchanged.
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const PREFIX: &str = "pmux-progress ";

/// How often a running download reports its size.
const SAMPLE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Progress {
    /// Bytes received of the whole release download. `item` is the file
    /// being fetched now.
    Download {
        item: String,
        received: u64,
        total: u64,
    },
    /// Checking one part of the downloaded release. `index` counts from 1.
    Verify {
        item: String,
        index: usize,
        count: usize,
    },
    /// Putting the verified release in place. It has no measurable fraction.
    Install { item: String },
}

static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(crate) fn emit(progress: &Progress) {
    if ENABLED.load(Ordering::Relaxed) {
        eprintln!("{}", line(progress));
    }
}

/// The stderr line for one event.
pub fn line(progress: &Progress) -> String {
    format!(
        "{PREFIX}{}",
        serde_json::to_string(progress).expect("progress serializes")
    )
}

/// The event on a progress line, or `None` for any other line.
pub fn parse(line: &str) -> Option<Progress> {
    serde_json::from_str(line.trim_end().strip_prefix(PREFIX)?).ok()
}

/// One file of a download whose total size is known in advance.
pub(crate) struct Part<'a> {
    pub item: &'a str,
    pub path: &'a Path,
    /// Bytes of earlier files in the same download.
    pub before: u64,
    pub total: u64,
}

impl Part<'_> {
    fn event(&self) -> Progress {
        let current = std::fs::metadata(self.path).map_or(0, |meta| meta.len());
        Progress::Download {
            item: self.item.into(),
            received: (self.before + current).min(self.total),
            total: self.total,
        }
    }
}

/// Wait for a download child, reporting the size of its output file as it
/// grows, then once more when it exits.
pub(crate) fn wait_download(
    child: &mut Child,
    part: &Part<'_>,
    report: &mut dyn FnMut(&Progress),
) -> std::io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            report(&part.event());
            return Ok(status);
        }
        report(&part.event());
        std::thread::sleep(SAMPLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pmux-progress-{name}-{}-{}",
            std::process::id(),
            crate::host_render_status::unix_ms()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn events_round_trip_through_their_line() {
        let events = [
            Progress::Download {
                item: "pmuxd".into(),
                received: 512,
                total: 2048,
            },
            Progress::Verify {
                item: "pmux-attach".into(),
                index: 3,
                count: 6,
            },
            Progress::Install {
                item: "core binaries".into(),
            },
        ];
        for event in events {
            let text = line(&event);
            assert!(text.starts_with(PREFIX), "{text}");
            assert_eq!(parse(&format!("{text}\n")), Some(event));
        }
    }

    #[test]
    fn ordinary_messages_are_not_progress() {
        assert_eq!(parse("Downloading pmuxd 0.3.70"), None);
        assert_eq!(parse("pmux-progress not json"), None);
        assert_eq!(
            parse(r#"{"stage":"install","item":"x"}"#),
            None,
            "a bare object without the prefix is not progress"
        );
    }

    #[test]
    fn the_final_report_counts_earlier_files_and_the_whole_last_one() {
        let dir = temporary("final");
        let path = dir.join("pmuxd");
        let mut child = crate::platform::hidden_command(Path::new(env!("CARGO")))
            .arg("--version")
            .stdout(std::fs::File::create(&path).unwrap())
            .spawn()
            .unwrap();
        let part = Part {
            item: "pmuxd",
            path: &path,
            before: 1000,
            total: 1_000_000,
        };
        let mut seen = Vec::new();
        let status = wait_download(&mut child, &part, &mut |p| seen.push(p.clone())).unwrap();
        assert!(status.success());
        let written = std::fs::metadata(&path).unwrap().len();
        assert!(written > 0);
        assert_eq!(
            seen.last(),
            Some(&Progress::Download {
                item: "pmuxd".into(),
                received: 1000 + written,
                total: 1_000_000,
            })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn received_never_passes_the_total() {
        let dir = temporary("cap");
        let path = dir.join("big");
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        let part = Part {
            item: "big",
            path: &path,
            before: 0,
            total: 10,
        };
        assert_eq!(
            part.event(),
            Progress::Download {
                item: "big".into(),
                received: 10,
                total: 10,
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
