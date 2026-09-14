//! Background Git labels for pane working directories.
use prismattyc_mux::PaneId;
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

type Labels = HashMap<PaneId, (PathBuf, Option<String>)>;
#[derive(Default)]
pub(crate) struct Cache {
    labels: Labels,
    pending: Option<mpsc::Receiver<Labels>>,
    last_poll: Option<Instant>,
}
impl Cache {
    pub(crate) fn label(&self, pane: PaneId) -> Option<&str> {
        self.labels
            .get(&pane)
            .and_then(|(_, label)| label.as_deref())
    }
    pub(crate) fn poll(&mut self, targets: Vec<(PaneId, PathBuf)>) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(labels) => {
                    changed = self.labels != labels;
                    self.labels = labels;
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let before = self.labels.len();
        // Discard results for a former directory or a closed pane.
        self.labels
            .retain(|pane, (path, _)| targets.iter().any(|(id, cwd)| id == pane && cwd == path));
        changed |= before != self.labels.len();
        if self.pending.is_none()
            && self
                .last_poll
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(2))
        {
            self.last_poll = Some(Instant::now());
            let (tx, rx) = mpsc::channel();
            self.pending = Some(rx);
            std::thread::spawn(move || {
                let mut by_cwd = HashMap::new();
                let labels = targets
                    .into_iter()
                    .map(|(pane, path)| {
                        let label = by_cwd
                            .entry(path.clone())
                            .or_insert_with(|| probe(&path))
                            .clone();
                        (pane, (path, label))
                    })
                    .collect();
                let _ = tx.send(labels);
            });
        }
        changed
    }
}

fn git(path: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(name);
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // Bound memory even for very large working trees. A full pipe must not block wait.
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let retain = n.min(65536usize.saturating_sub(out.len()));
            out.extend_from_slice(&chunk[..retain]);
        }
        out
    });
    let start = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let out = reader.join().ok()?;
    success.then(|| String::from_utf8_lossy(&out).trim_end().to_string())
}
fn probe(path: &Path) -> Option<String> {
    let root = git(path, &["rev-parse", "--show-toplevel"])?;
    let status = git(
        path,
        &[
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ],
    )?;
    let mut lines = status.lines();
    let header = lines.next()?.strip_prefix("## ")?;
    let branch = if header.starts_with("HEAD (") {
        format!("detached {}", git(path, &["rev-parse", "--short", "HEAD"])?)
    } else {
        header
            .strip_prefix("No commits yet on ")
            .or_else(|| header.strip_prefix("Initial commit on "))
            .unwrap_or(header)
            .split("...")
            .next()?
            .to_string()
    };
    let name = Path::new(&root).file_name()?.to_string_lossy();
    let dirty = if lines.next().is_some() { " *" } else { "" };
    Some(
        format!("{name}:{branch}{dirty}")
            .chars()
            .filter(|ch| !ch.is_control())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_branch_dirty_detached_and_non_repository() {
        let dir = std::env::temp_dir().join(format!("prism-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(probe(&dir), None);
        assert!(git(&dir, &["init", "-b", "main"]).is_some());
        assert!(probe(&dir).unwrap().ends_with(":main"));
        std::fs::write(dir.join("file"), "one").unwrap();
        assert!(probe(&dir).unwrap().ends_with(":main *"));
        git(&dir, &["add", "file"]).unwrap();
        git(
            &dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        assert!(probe(&dir).unwrap().ends_with(":main"));
        git(&dir, &["checkout", "-b", "feature"]).unwrap();
        assert!(probe(&dir).unwrap().ends_with(":feature"));
        git(&dir, &["checkout", "--detach"]).unwrap();
        assert!(probe(&dir).unwrap().contains(":detached "));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Keep branch and dirty state ahead of repository/session context when narrow.
pub(crate) fn compact(title: &str, git: &str, cells: usize) -> String {
    if cells == 0 {
        return String::new();
    }
    let full = format!("{title} · {git}");
    if width(&full) <= cells {
        return full;
    }
    let branch = git.split_once(':').map_or(git, |(_, branch)| branch);
    let dirty = if branch.ends_with(" *") {
        if cells == 1 {
            "*"
        } else {
            " *"
        }
    } else {
        ""
    };
    let branch = branch.strip_suffix(" *").unwrap_or(branch);
    let core = format!(
        "{}{}",
        ellipsis(branch, cells.saturating_sub(width(dirty))),
        dirty
    );
    let remaining = cells.saturating_sub(width(&core));
    if remaining >= 6 {
        format!("{} · {core}", ellipsis(title, remaining - 3))
    } else {
        core
    }
}
fn width(value: &str) -> usize {
    value.chars().map(prismattyc_core::char_display_width).sum()
}
fn ellipsis(value: &str, cells: usize) -> String {
    if width(value) <= cells {
        return value.into();
    }
    if cells == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let w = prismattyc_core::char_display_width(ch);
        if used + w >= cells {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    #[test]
    fn narrow_tabs_preserve_branch_dirty_marker_and_display_cell_budget() {
        assert!(compact(
            "A very long terminal title",
            "repository:feature/spaces *",
            24
        )
        .ends_with("feature/spaces *"));
        for cells in 0..40 {
            let label = compact("終端", "repository:very-long-branch-name *", cells);
            assert!(width(&label) <= cells, "{cells}: {label}");
            if cells > 0 {
                assert!(label.ends_with('*'));
            }
        }
        assert_eq!(compact("shell", "repo:main", 80), "shell · repo:main");
    }
}
