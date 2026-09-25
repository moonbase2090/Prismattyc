//! Registered windowed host next to the mux socket (PT-65).
//!
//! The host writes `{stem}.host.pid` at start and removes it on exit.
//! Alive means `kill(pid, 0)` succeeds. A second host does not overwrite a
//! live registration. `{stem}.host.ack` is a touch-file the host updates
//! after it reloads the attach-tabs cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::procinfo::pid_alive;

/// `{stem}.host.pid` beside the control socket.
#[must_use]
pub fn host_pid_path_from_socket(socket: &Path) -> PathBuf {
    sibling_from_socket(socket, ".host.pid")
}

/// `{stem}.host.ack` beside the control socket.
#[must_use]
pub fn host_ack_path_from_socket(socket: &Path) -> PathBuf {
    sibling_from_socket(socket, ".host.ack")
}

fn sibling_from_socket(socket: &Path, suffix: &str) -> PathBuf {
    let stem = socket.file_stem().map_or_else(
        || std::ffi::OsString::from("prism"),
        std::ffi::OsStr::to_os_string,
    );
    let mut file = stem;
    file.push(suffix);
    match socket.parent() {
        Some(dir) => dir.join(file),
        None => PathBuf::from(file),
    }
}

/// Pid in the file when that process is still alive.
#[must_use]
pub fn live_host_pid(path: &Path) -> Option<u32> {
    let pid = parse_pid_file(path)?;
    pid_alive(pid).then_some(pid)
}

/// Write `pid` when no other live host owns the slot. Returns whether this
/// process is now the registered host.
///
/// An exclusive flock on `{path}.lock` serializes claim and stale replace
/// so two hosts cannot both remove a dead pid and both return true.
pub fn register_host_pid(path: &Path, pid: u32) -> std::io::Result<bool> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    let _lock = exclusive_pid_lock(path)?;
    if let Some(existing) = live_host_pid(path) {
        return Ok(existing == pid);
    }
    let _ = fs::remove_file(path);
    try_create_pid_file(path, pid).map(|()| true)
}

fn exclusive_pid_lock(path: &Path) -> std::io::Result<fs::File> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))?;
    crate::platform::lock_exclusive(&lock)?;
    Ok(lock)
}

fn parse_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn try_create_pid_file(path: &Path, pid: u32) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    Ok(())
}

/// Remove the pid file when it still names `pid`.
pub fn unregister_host_pid(path: &Path, pid: u32) {
    if live_host_pid(path) == Some(pid) || file_names_pid(path, pid) {
        let _ = fs::remove_file(path);
    }
}

fn file_names_pid(path: &Path, pid: u32) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        == Some(pid)
}

/// Create or refresh the ack file so `pmux space open` can wait on it.
pub fn touch_host_ack(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    fs::write(path, b"ok\n")
}

/// `PRISMATTYC_HOST=1` on a pane spawned by the windowed host.
///
/// `true`/`false` replacements are follow-up coverage
/// (PT-306 mux scrollbar/seat-route). Callers still pass the
/// value into `should_host_route_seat`, which stays mutatable.
#[must_use]
#[mutants::skip]
pub fn host_pane_nested() -> bool {
    matches!(
        std::env::var("PRISMATTYC_HOST").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// `PRISMATTYC_ATTACH_PTY=1` keeps the pre-PT-111 nested attach child.
#[must_use]
pub fn attach_pty_fallback() -> bool {
    matches!(
        std::env::var("PRISMATTYC_ATTACH_PTY").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// True when an interactive seat attach under the host must write the
/// attach-tabs cache instead of nesting `pmux-attach` (PT-306).
#[must_use]
pub fn should_host_route_seat(under_host: bool, pty_fallback: bool, dump_or_write: bool) -> bool {
    under_host && !pty_fallback && !dump_or_write
}

/// Write `session_id` into the attach-tabs cache so a live host opens a
/// log-replica pane. Returns `false` when no host is registered. The
/// caller waits on `{stem}.host.ack` the same way `pmux space open` does.
pub fn route_seat_to_host(socket: &Path, session_id: &str, title: &str) -> std::io::Result<bool> {
    if live_host_pid(&host_pid_path_from_socket(socket)).is_none() {
        return Ok(false);
    }
    let path = crate::attach_tabs::layout_path_from_socket(socket);
    let file = crate::attach_tabs::plan_host_seat_cache(
        crate::attach_tabs::load(&path),
        session_id,
        title,
    );
    let ack = host_ack_path_from_socket(socket);
    let _ = fs::remove_file(&ack);
    crate::attach_tabs::save(&path, &file)?;
    Ok(true)
}

/// Wait until `ack` exists and its mtime is at least `since`.
#[must_use]
pub fn wait_host_ack(ack: &Path, since: SystemTime, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ack_is_fresh(ack, since) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ack_is_fresh(ack: &Path, since: SystemTime) -> bool {
    let Ok(meta) = fs::metadata(ack) else {
        return false;
    };
    meta.modified().is_ok_and(|mtime| mtime >= since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pmux-host-reg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn pid_and_ack_paths_sit_beside_the_socket() {
        let socket = Path::new("/run/user/1000/prismattyc/pmux.sock");
        assert_eq!(
            host_pid_path_from_socket(socket),
            PathBuf::from("/run/user/1000/prismattyc/pmux.host.pid")
        );
        assert_eq!(
            host_ack_path_from_socket(socket),
            PathBuf::from("/run/user/1000/prismattyc/pmux.host.ack")
        );
        assert_eq!(
            crate::attach_tabs::layout_path_from_socket(socket),
            PathBuf::from("/run/user/1000/prismattyc/pmux.attach-tabs.json")
        );
    }

    #[test]
    fn live_dead_and_missing_pid_files() {
        let dir = temp_dir("disc");
        let path = dir.join("pmux.host.pid");
        assert_eq!(live_host_pid(&path), None, "missing");

        fs::write(&path, "not-a-pid\n").unwrap();
        assert_eq!(live_host_pid(&path), None, "unparseable");

        let child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        let _ = child.wait_with_output();
        fs::write(&path, format!("{dead}\n")).unwrap();
        assert_eq!(live_host_pid(&path), None, "dead pid");

        let live = std::process::id();
        fs::write(&path, format!("{live}\n")).unwrap();
        assert_eq!(live_host_pid(&path), Some(live));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_does_not_overwrite_a_live_slot() {
        let dir = temp_dir("reg");
        let path = dir.join("pmux.host.pid");
        let live = std::process::id();
        assert!(register_host_pid(&path, live).unwrap());
        assert_eq!(live_host_pid(&path), Some(live));
        assert!(
            !register_host_pid(&path, live.wrapping_add(1).max(1)).unwrap(),
            "second host must not steal a live slot"
        );
        assert_eq!(live_host_pid(&path), Some(live));
        unregister_host_pid(&path, live);
        assert_eq!(live_host_pid(&path), None);
        assert!(register_host_pid(&path, live).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_pid_file_is_replaced() {
        let dir = temp_dir("stale");
        let path = dir.join("pmux.host.pid");
        let child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        let _ = child.wait_with_output();
        fs::write(&path, format!("{dead}\n")).unwrap();
        let live = std::process::id();
        assert!(register_host_pid(&path, live).unwrap());
        assert_eq!(live_host_pid(&path), Some(live));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_register_leaves_one_live_owner() {
        for round in 0..8 {
            let dir = temp_dir(&format!("race{round}"));
            let path = dir.join("pmux.host.pid");
            let mut left = Command::new("sleep").arg("8").spawn().unwrap();
            let mut right = Command::new("sleep").arg("8").spawn().unwrap();
            let left_pid = left.id();
            let right_pid = right.id();
            let path_a = path.clone();
            let path_b = path.clone();
            let (ok_a, ok_b) = std::thread::scope(|scope| {
                let a = scope.spawn(|| register_host_pid(&path_a, left_pid).unwrap());
                let b = scope.spawn(|| register_host_pid(&path_b, right_pid).unwrap());
                (a.join().unwrap(), b.join().unwrap())
            });
            assert!(
                ok_a ^ ok_b,
                "round {round}: exactly one host must win (left={ok_a} right={ok_b} file={:?})",
                fs::read_to_string(&path).ok()
            );
            let winner = live_host_pid(&path).expect("winner still live");
            assert!(winner == left_pid || winner == right_pid);
            assert_eq!(ok_a, winner == left_pid);
            assert_eq!(ok_b, winner == right_pid);
            let _ = left.kill();
            let _ = right.kill();
            let _ = left.wait();
            let _ = right.wait();
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn concurrent_stale_replace_leaves_one_live_owner() {
        for round in 0..8 {
            let dir = temp_dir(&format!("stale-race{round}"));
            let path = dir.join("pmux.host.pid");
            let dead = Command::new("true").spawn().unwrap();
            let dead_pid = dead.id();
            let _ = dead.wait_with_output();
            fs::write(&path, format!("{dead_pid}\n")).unwrap();
            let mut left = Command::new("sleep").arg("8").spawn().unwrap();
            let mut right = Command::new("sleep").arg("8").spawn().unwrap();
            let left_pid = left.id();
            let right_pid = right.id();
            let path_a = path.clone();
            let path_b = path.clone();
            let (ok_a, ok_b) = std::thread::scope(|scope| {
                let a = scope.spawn(|| register_host_pid(&path_a, left_pid).unwrap());
                let b = scope.spawn(|| register_host_pid(&path_b, right_pid).unwrap());
                (a.join().unwrap(), b.join().unwrap())
            });
            assert!(
                ok_a ^ ok_b,
                "round {round}: stale replace must elect one (left={ok_a} right={ok_b} file={:?})",
                fs::read_to_string(&path).ok()
            );
            let winner = live_host_pid(&path).expect("winner still live");
            assert!(winner == left_pid || winner == right_pid);
            let _ = left.kill();
            let _ = right.kill();
            let _ = left.wait();
            let _ = right.wait();
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn host_route_seat_writes_cache_only_when_a_host_is_registered() {
        let cases = [
            (true, false, false, true),
            (true, true, false, false),
            (true, false, true, false),
            (true, true, true, false),
            (false, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, true, true, false),
        ];
        for (under_host, pty_fallback, dump_or_write, route) in cases {
            assert_eq!(
                should_host_route_seat(under_host, pty_fallback, dump_or_write),
                route,
                "host={under_host} pty={pty_fallback} dump={dump_or_write}"
            );
        }

        let dir = temp_dir("route");
        let socket = dir.join("pmux.sock");
        assert!(!route_seat_to_host(&socket, "5", "astra-pc").unwrap());
        assert!(
            crate::attach_tabs::load(&crate::attach_tabs::layout_path_from_socket(&socket))
                .is_none()
        );

        let live = std::process::id();
        register_host_pid(&host_pid_path_from_socket(&socket), live).unwrap();
        assert!(route_seat_to_host(&socket, "5", "astra-pc").unwrap());
        let file = crate::attach_tabs::load(&crate::attach_tabs::layout_path_from_socket(&socket))
            .expect("cache");
        assert_eq!(file.tabs[0].sessions, ["5"]);
        assert_eq!(file.tabs[0].title, "astra-pc");
        assert_eq!(file.focused_session.as_deref(), Some("5"));
        unregister_host_pid(&host_pid_path_from_socket(&socket), live);
        let _ = fs::remove_dir_all(&dir);
    }
}
