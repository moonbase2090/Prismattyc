// SPDX-License-Identifier: MIT
//! `pmux space monitor`: make sure the Activity Monitor Space exists and its
//! one session is running the monitor, so the host can switch to it.

use super::*;

/// The Space, its session and its tab all carry this name.
pub(in super::super) const NAME: &str = "Activity Monitor";
const LAUNCHER: &str = "pmux-activity-monitor";
const PLUGINS: [&str; 2] = [
    "org.moonbase.activity-connector",
    "org.moonbase.activity-monitor",
];
const USAGE: &str = "usage: pmux space monitor";

pub(in super::super) fn run(paths: &Paths, args: Vec<String>) -> Result<()> {
    anyhow::ensure!(args.is_empty(), "{USAGE}");
    ensure_live_server(paths)?;
    // Best effort: without the plug-ins the monitor itself says how to install and enable them.
    if let Err(error) = ensure_plugin(&paths.socket) {
        eprintln!("pmux: could not enable the activity plug-ins: {error:#}");
    }
    let command = command(&launcher(), &prismattyc_mux::platform::default_shell());
    anyhow::ensure!(
        !command_has_line_break(&command),
        "the monitor path contains a line break"
    );
    let (mut client, client_id) = connect_registered(paths)?;
    let store = Store::lock(&mut client)?;
    let path = prismattyc_mux::layout_path(&spaces_dir(), NAME)?;
    let (space, record, created) = if path.exists() {
        let space = prepare_open(&store, &mut client, NAME)?;
        let record = space
            .sessions
            .iter()
            .find(|record| record.name == NAME)
            .cloned()
            .with_context(|| {
                format!("the {NAME} Space has no {NAME} session; remove the Space to recreate it")
            })?;
        (space, record, false)
    } else {
        let mut record = fresh_record(&mut client, NAME, Some(NAME), true)?;
        if let SavedNode::Leaf { command: saved, .. } = &mut record.windows[0].root {
            *saved = Some(command.clone());
        }
        let mut space = from_sessions(&[]);
        identify(&mut space)?;
        prismattyc_mux::space_add_session(&mut space, record.clone(), Some(NAME))?;
        store.commit(
            &mut client,
            Transaction {
                releases: vec![],
                pane_move: None,
                deleted: vec![],
                files: vec![(NAME.into(), space.clone())],
                sessions: vec![],
                from: None,
                to: space.id.clone(),
            },
        )?;
        (space, record, true)
    };
    let result = recreate(&mut client, &space, &record)?;
    claim_saved_session(&mut client, &space, NAME)?;
    drop(store);
    // A session that was already live may still be running the monitor, so
    // it only gets the command when its shell is idle (after `q`, say).
    let fresh = created || !matches!(result, ApplyResult::Skipped);
    let snapshot = take_snapshot(&mut client)?;
    let pane = snapshot
        .sessions
        .iter()
        .find(|session| session.name == NAME)
        .and_then(|session| session.windows.first())
        .and_then(|window| live_leaf_ids(&window.layout).first().copied())
        .context("the Activity Monitor session has no pane")?;
    match write_pane_command(&mut client, client_id, pane, &command, !fresh)? {
        WritePaneCommand::Wrote => println!("{NAME}: started the monitor"),
        WritePaneCommand::SkipRunning => println!("{NAME}: the monitor is running"),
    }
    Ok(())
}

/// Choosing the Activity Monitor is the approval to read this machine's own activity: a fresh
/// install has the plug-ins neither installed nor enabled, and the monitor refuses to start
/// without them. Each gets the local read grant only. One already enabled is left alone,
/// whatever it was granted.
fn ensure_plugin(socket: &Path) -> Result<()> {
    use prismattyc_mux::plugins::{Client, Reply, Request};
    for id in PLUGINS {
        let mut client = Client::connect(socket)?;
        let Reply::List { installed, states } = client.request(Request::List)? else {
            anyhow::bail!("unexpected plug-in status");
        };
        drop(client);
        if states.iter().any(|state| state.id == id && state.enabled) {
            continue;
        }
        if !installed.iter().any(|package| package.id == id) {
            prismattyc_mux::plugins::run_cli(socket, vec!["install".into(), id.into()])?;
        }
        let enable = ["enable", id, "--allow", "activity.read:local"];
        prismattyc_mux::plugins::run_cli(socket, enable.map(String::from).to_vec())?;
    }
    Ok(())
}

/// `PMUX_ACTIVITY_MONITOR`, else the launcher next to this pmux, else the one
/// in the installed app, else the bare name for `PATH`.
fn launcher() -> PathBuf {
    if let Some(path) = std::env::var_os("PMUX_ACTIVITY_MONITOR").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| prismattyc_mux::executable::sibling(&exe, &[LAUNCHER]))
        .or_else(|| prismattyc_mux::executable::app_companion(&[LAUNCHER]))
        .unwrap_or_else(|| PathBuf::from(prismattyc_mux::platform::executable_name(LAUNCHER)))
}

/// The line typed into the pane's shell. The pane already exports
/// `PMUX_SOCKET`, so the monitor reads this daemon without `--socket`.
fn command(program: &Path, shell: &str) -> String {
    let program = program.to_string_lossy();
    // A canonicalized Windows path is verbatim (`\\?\C:\...`), which cmd.exe cannot run.
    let program = match program.strip_prefix(r"\\?\") {
        Some(plain) if !plain.starts_with("UNC\\") => std::borrow::Cow::Borrowed(plain),
        _ => program,
    };
    // Either separator: a Windows shell path is parsed the same everywhere.
    let shell = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    let shell = shell.to_ascii_lowercase();
    match shell.strip_suffix(".exe").unwrap_or(&shell) {
        "pwsh" | "powershell" => format!("& '{}'", program.replace('\'', "''")),
        "cmd" => format!("\"{program}\""),
        _ => prismattyc_mux::procinfo::shell_join(&[program.into_owned()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_quoted_for_the_pane_shell() {
        let program = Path::new("/Applications/My Apps/pmux-activity-monitor");
        assert_eq!(
            command(program, "/bin/zsh"),
            "'/Applications/My Apps/pmux-activity-monitor'"
        );
        assert_eq!(
            command(Path::new("/usr/bin/pmux-activity-monitor"), "/bin/sh"),
            "/usr/bin/pmux-activity-monitor"
        );
        let windows =
            Path::new(r"C:\Users\O'Neil\AppData\Local\Prismattyc\bin\pmux-activity-monitor.exe");
        assert_eq!(
            command(windows, r"C:\Program Files\PowerShell\7\pwsh.exe"),
            r"& 'C:\Users\O''Neil\AppData\Local\Prismattyc\bin\pmux-activity-monitor.exe'"
        );
        let verbatim = Path::new(r"\\?\C:\Users\me\bin\pmux-activity-monitor.exe");
        assert_eq!(
            command(verbatim, r"C:\Windows\System32\cmd.exe"),
            r#""C:\Users\me\bin\pmux-activity-monitor.exe""#
        );
        assert_eq!(
            command(windows, r"C:\Windows\System32\cmd.exe"),
            r#""C:\Users\O'Neil\AppData\Local\Prismattyc\bin\pmux-activity-monitor.exe""#
        );
    }
}
