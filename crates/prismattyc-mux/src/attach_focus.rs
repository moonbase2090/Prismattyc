//! Live, window-local identity for a nested terminal attach. No daemon restart required.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Serialize, Deserialize)]
struct Focus {
    pid: u32,
    pane: u64,
    at_ms: u64,
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn path(socket: &Path, pid: u32, kind: &str) -> PathBuf {
    socket.with_extension(format!("attach-{pid}.{kind}"))
}
fn write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, body)?;
    fs::rename(tmp, path)
}
/// Only a live, recently reporting attach can select the pane for a host action.
pub fn read(socket: &Path, pid: u32) -> Option<u64> {
    if !crate::procinfo::pid_alive(pid) {
        return None;
    }
    let raw = fs::read(path(socket, pid, "focus")).ok()?;
    if raw.len() > 1024 {
        return None;
    }
    let focus: Focus = serde_json::from_slice(&raw).ok()?;
    (focus.pid == pid && now_ms().checked_sub(focus.at_ms)? < 2000).then_some(focus.pane)
}
/// Ask the exact viewer to leave cleanly, restoring the parent shell's terminal modes.
pub fn request_detach(socket: &Path, pid: u32, pane: u64) -> std::io::Result<()> {
    write(&path(socket, pid, "detach"), pane.to_string().as_bytes())
}
pub struct Reporter {
    socket: PathBuf,
    pid: u32,
    last: Option<(u64, Instant)>,
}
impl Reporter {
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.into(),
            pid: std::process::id(),
            last: None,
        }
    }
    /// True means this exact target received a detach request.
    pub fn update(&mut self, pane: u64) -> bool {
        let detach = path(&self.socket, self.pid, "detach");
        if let Ok(raw) = fs::read_to_string(&detach) {
            let _ = fs::remove_file(detach);
            if raw.parse::<u64>().ok() == Some(pane) {
                return true;
            }
        }
        if self
            .last
            .is_none_or(|(old, at)| old != pane || at.elapsed() >= Duration::from_millis(250))
        {
            let focus = Focus {
                pid: self.pid,
                pane,
                at_ms: now_ms(),
            };
            if let Ok(raw) = serde_json::to_vec(&focus) {
                if write(&path(&self.socket, self.pid, "focus"), &raw).is_ok() {
                    self.last = Some((pane, Instant::now()));
                }
            }
        }
        false
    }
}
impl Drop for Reporter {
    fn drop(&mut self) {
        for kind in ["focus", "detach"] {
            let _ = fs::remove_file(path(&self.socket, self.pid, kind));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_focus_and_exact_detach_are_live_and_cleaned_up() {
        let dir = std::env::temp_dir().join(format!("attach-focus-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mux.sock");
        let pid = std::process::id();
        let mut reporter = Reporter::new(&socket);
        assert!(!reporter.update(12));
        assert_eq!(read(&socket, pid), Some(12));
        assert!(!reporter.update(15));
        assert_eq!(read(&socket, pid), Some(15));
        request_detach(&socket, pid, 12).unwrap();
        assert!(!reporter.update(15));
        request_detach(&socket, pid, 15).unwrap();
        assert!(reporter.update(15));
        drop(reporter);
        assert_eq!(read(&socket, pid), None);
        let stale = Focus {
            pid,
            pane: 12,
            at_ms: now_ms() - 3000,
        };
        fs::write(
            path(&socket, pid, "focus"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        assert_eq!(read(&socket, pid), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
