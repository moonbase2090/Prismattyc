//! The `activity_monitor` action: open `pmux-activity-monitor` in a new tab,
//! pointed at the daemon this host uses.

use std::path::{Path, PathBuf};

use crate::HostState;

const LAUNCHER: &str = "pmux-activity-monitor";
const TITLE: &str = "Activity monitor";

#[cfg(test)]
thread_local! {
    /// A stand-in launcher for tests on this thread, so they never change the
    /// process environment that other tests read.
    pub(crate) static TEST_LAUNCHER: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// `PMUX_ACTIVITY_MONITOR` when set, else the launcher next to this binary,
/// else one in the installed app bundle, else the bare name for `PATH`.
fn launcher(executable: Option<&Path>) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_LAUNCHER.with(|launcher| launcher.borrow().clone()) {
        return path;
    }
    if let Some(path) = std::env::var_os("PMUX_ACTIVITY_MONITOR").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    executable
        .and_then(|executable| prismattyc_mux::executable::sibling(executable, &[LAUNCHER]))
        .or_else(|| prismattyc_mux::executable::app_companion(&[LAUNCHER]))
        .unwrap_or_else(|| PathBuf::from(prismattyc_mux::platform::executable_name(LAUNCHER)))
}

/// Program and arguments for the new tab. An explicit socket keeps the
/// monitor on this host's daemon whatever the pane environment says.
fn command(executable: Option<&Path>, socket: Option<&Path>) -> (String, Vec<String>) {
    let program = launcher(executable).to_string_lossy().into_owned();
    let args = socket.map_or_else(Vec::new, |socket| {
        vec!["--socket".into(), socket.to_string_lossy().into_owned()]
    });
    (program, args)
}

pub(crate) fn open(host: &mut HostState) {
    let executable = std::env::current_exe().ok();
    let socket = crate::host_mux_socket();
    let (program, args) = command(executable.as_deref(), socket.as_deref());
    match host.mux.new_tab(&program, &args) {
        Ok(_) => {
            let pane = host.mux.focused_id();
            // The monitor is a local view of the daemon, not a session to keep.
            host.mux.retain_local_terminal(pane);
            let _ = host.mux.rename_window(host.mux.active_window(), TITLE);
            host.dirty = true;
        }
        Err(error) => {
            crate::rail_error_toast(host, &format!(" Activity monitor did not open: {error:#} "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_next_to_the_host_wins_and_gets_the_host_socket() {
        let directory =
            std::env::temp_dir().join(format!("prism-monitor-tab-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let host = directory.join(prismattyc_mux::platform::executable_name("prismattyc-host"));
        let sibling = directory.join(prismattyc_mux::platform::executable_name(LAUNCHER));
        std::fs::write(&host, b"").unwrap();
        std::fs::write(&sibling, b"").unwrap();
        let socket = directory.join("pmux.sock");
        let (program, args) = command(Some(&host), Some(&socket));
        assert_eq!(
            Path::new(&program).canonicalize().unwrap(),
            sibling.canonicalize().unwrap()
        );
        assert_eq!(args, ["--socket", socket.to_str().unwrap()]);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn without_a_sibling_or_socket_the_launcher_comes_from_path() {
        let directory =
            std::env::temp_dir().join(format!("prism-monitor-tab-alone-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let host = directory.join(prismattyc_mux::platform::executable_name("prismattyc-host"));
        std::fs::write(&host, b"").unwrap();
        let (program, args) = command(Some(&host), None);
        // An installed app bundle may supply it; otherwise the bare name.
        assert!(
            program.ends_with(&*prismattyc_mux::platform::executable_name(LAUNCHER)),
            "{program}"
        );
        assert!(args.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
