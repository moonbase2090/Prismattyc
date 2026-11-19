// SPDX-License-Identifier: MIT
//! One owner for the consent, installation, restart, and restoration flow.
use super::*;
use prismattyc_mux::release_update::progress::{self, Progress};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use update_meter::Meter;

/// How often a step without a fraction moves its bar.
const TICK: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Clone, Copy, Debug)]
pub(super) enum Action {
    Start,
    SaveAndContinue,
    Continue,
    Cancel,
    Restore,
    KeepSaved,
}

#[derive(Debug, Deserialize, Clone)]
pub(super) struct Plan {
    installed: String,
    available: String,
    update_available: bool,
    sessions: Vec<(u64, String)>,
}

#[derive(Debug)]
pub(super) enum Event {
    Prepared(Result<Plan, String>),
    /// A stage reported by `pmux update --progress`.
    Progress(Progress),
    /// The app is restarting component `index` of `count`.
    Restart(usize, usize, &'static str),
    /// Move the bar of a step that has no fraction.
    Tick,
    Installed(Result<Resume, String>),
    Restored(Result<String, String>),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct Resume {
    notice: String,
    backup: Option<PathBuf>,
}

#[derive(Default)]
pub(super) enum Flow {
    #[default]
    Idle,
    Busy,
    Confirm(Plan),
    /// Installing and restarting; the flag stops the meter's ticker.
    Installing(Meter, Arc<AtomicBool>),
    Restore(Resume),
}

fn show(app: &mut App, text: &str, actions: &[(&str, Action)]) {
    for host in app.windows.values_mut() {
        space_panel::update_page(host, text, actions);
    }
}

fn show_meter(app: &mut App, meter: &Meter, text: &str, actions: &[(&str, Action)]) {
    for host in app.windows.values_mut() {
        space_panel::update_page_with_meter(host, text, Some(meter), actions);
    }
}

/// Redraw the meter after it changed, while the install is still running.
fn refresh_meter(app: &mut App, change: impl FnOnce(&mut Meter) -> bool) {
    let Flow::Installing(meter, _) = &mut app.full_update else {
        return;
    };
    let before = meter.step_line();
    if !change(meter) {
        return;
    }
    let meter = meter.clone();
    let after = meter.step_line();
    if after != before {
        // Steps only, never byte counts, so the log stays short.
        eprintln!("prismattyc-host: update progress: {after}");
    }
    show_meter(app, &meter, meter.note(), &[]);
}

pub(super) fn action(app: &mut App, action: Action) {
    match action {
        Action::Start if !matches!(app.full_update, Flow::Busy) => {
            app.full_update = Flow::Busy;
            show(
                app,
                "Checking for updates. Your sessions are still running.",
                &[],
            );
            let proxy = app.event_proxy.clone();
            std::thread::spawn(move || {
                let result = prepare().map_err(|e| format!("{e:#}"));
                let _ = proxy.send_event(UserAction::FullUpdate(Event::Prepared(result)));
            });
        }
        Action::SaveAndContinue | Action::Continue => {
            let Flow::Confirm(plan) = &app.full_update else {
                return;
            };
            let plan = plan.clone();
            restart_modal::close(app);
            let meter = Meter::new(plan.update_available);
            let stop = Arc::new(AtomicBool::new(false));
            show_meter(app, &meter, meter.note(), &[]);
            app.full_update = Flow::Installing(meter, stop.clone());
            let ticker = app.event_proxy.clone();
            let ticking = stop.clone();
            std::thread::spawn(move || {
                while !ticking.load(Ordering::Relaxed) {
                    std::thread::sleep(TICK);
                    if ticker
                        .send_event(UserAction::FullUpdate(Event::Tick))
                        .is_err()
                    {
                        return;
                    }
                }
            });
            let proxy = app.event_proxy.clone();
            std::thread::spawn(move || {
                let report = |event| {
                    let _ = proxy.send_event(UserAction::FullUpdate(event));
                };
                let result = install(plan, matches!(action, Action::SaveAndContinue), &report)
                    .map_err(|e| format!("{e:#}"));
                report(Event::Installed(result));
            });
        }
        Action::Restore => {
            let Flow::Restore(resume) = &app.full_update else {
                return;
            };
            let Some(backup) = resume.backup.clone() else {
                return;
            };
            app.full_update = Flow::Busy;
            show(
                app,
                "Restoring saved sessions and relaunching their commands. Please wait.",
                &[],
            );
            let proxy = app.event_proxy.clone();
            std::thread::spawn(move || {
                let result = run(
                    &find_mux_bin(),
                    &["session-config", "restore", &backup.to_string_lossy()],
                )
                .map_err(|e| format!("{e:#}"));
                let _ = proxy.send_event(UserAction::FullUpdate(Event::Restored(result)));
            });
        }
        Action::Cancel => {
            restart_modal::close(app);
            app.full_update = Flow::Idle;
            show(
                app,
                "Update canceled. Your sessions are still running.",
                &[],
            );
        }
        Action::KeepSaved => {
            app.full_update = Flow::Idle;
            show(
                app,
                "No sessions were restored. Any saved session configuration remains available.",
                &[],
            );
        }
        Action::Start => {}
    }
}

pub(super) fn event(app: &mut App, event: Event) {
    match event {
        Event::Prepared(Ok(plan)) => {
            let names: Vec<String> = plan.sessions.iter().map(|(_, name)| name.clone()).collect();
            restart_modal::open(
                app,
                &plan.installed,
                plan.update_available.then_some(plan.available.as_str()),
                &names,
            );
            app.full_update = Flow::Confirm(plan);
        }
        Event::Progress(progress) => refresh_meter(app, |meter| {
            meter.apply(&progress);
            true
        }),
        Event::Restart(index, count, item) => refresh_meter(app, |meter| {
            meter.restart(index, count, item);
            true
        }),
        Event::Tick => refresh_meter(app, Meter::tick),
        Event::Installed(Err(error)) => {
            let text = format!("The operation could not finish.\n{error}");
            let actions = [("Check again", Action::Start)];
            match std::mem::take(&mut app.full_update) {
                Flow::Installing(mut meter, stop) => {
                    stop.store(true, Ordering::Relaxed);
                    meter.fail();
                    eprintln!("prismattyc-host: update progress: {}", meter.step_line());
                    show_meter(app, &meter, &text, &actions);
                }
                _ => show(app, &text, &actions),
            }
        }
        Event::Installed(Ok(resume)) => {
            if let Flow::Installing(_, stop) = &app.full_update {
                stop.store(true, Ordering::Relaxed);
            }
            if let Err(error) = relaunch(&resume) {
                app.full_update = Flow::Restore(resume.clone());
                show(app, &format!("{}\nCould not restart the app: {error:#}. Quit and reopen Prismattyc. Your session configuration is saved at {}.", resume.notice, resume.backup.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "no backup was requested".into())), &[]);
            }
        }
        Event::Restored(Ok(text)) => {
            app.full_update = Flow::Idle;
            for host in app.windows.values_mut() {
                host.restore_prompt = host
                    .attach_layout_path
                    .as_ref()
                    .and_then(|path| attach_tabs::load(path))
                    .and_then(restore_prompt::RestorePrompt::new);
                if host.restore_prompt.is_some() {
                    restore_prompt::finish(host, true);
                }
                host.cache_writer = true;
                host.local_views.fresh = false;
            }
            show(app, &format!("{text}\nSession restoration finished. Open a saved Space or select a session to view it."), &[]);
        }
        Event::Prepared(Err(error)) | Event::Restored(Err(error)) => {
            app.full_update = Flow::Idle;
            show(
                app,
                &format!("The operation could not finish.\n{error}"),
                &[("Check again", Action::Start)],
            );
        }
    }
}

fn prepare() -> Result<Plan> {
    let check: Value =
        serde_json::from_str(&run(&find_mux_bin(), &["update", "--check", "--json"])?)?;
    Ok(Plan {
        installed: check["installed"]
            .as_str()
            .context("update check omitted the installed version")?
            .into(),
        available: check["available"]
            .as_str()
            .context("update check omitted release version")?
            .into(),
        update_available: check["update_available"]
            .as_bool()
            .context("update check omitted availability")?,
        sessions: sessions(&find_mux_bin())?,
    })
}

fn sessions(pmux: &Path) -> Result<Vec<(u64, String)>> {
    let value: Value = serde_json::from_str(&run(pmux, &["restart", "--plan", "--json"])?)?;
    serde_json::from_value(value["sessions"].clone()).context("read running sessions")
}

fn require_same_sessions(plan: &Plan, pmux: &Path) -> Result<()> {
    let mut current = sessions(pmux)?;
    let mut consented = plan.sessions.clone();
    current.sort();
    consented.sort();
    anyhow::ensure!(current == consented, "The running sessions changed since confirmation. No sessions were stopped. Check again to review the current list.");
    Ok(())
}

/// A fresh, unused file in the private session-configuration directory.
/// `session-config save` refuses to overwrite, so the name carries the pid and
/// a millisecond timestamp.
pub(super) fn snapshot_path(prefix: &str) -> Result<PathBuf> {
    let directory = prismattyc_mux::spaces_dir()
        .parent()
        .context("configuration data directory")?
        .join("session-configurations");
    prismattyc_mux::private_fs::create_dir(&directory)?;
    Ok(directory.join(format!(
        "{prefix}-{}-{}.json",
        std::process::id(),
        prismattyc_mux::host_render_status::unix_ms()
    )))
}

fn install(plan: Plan, save: bool, report: &dyn Fn(Event)) -> Result<Resume> {
    let pmux = find_mux_bin();
    require_same_sessions(&plan, &pmux)?;
    let file = snapshot_path("update")?;
    run(&pmux, &["session-config", "save", &file.to_string_lossy()])?;
    let backup = save.then_some(file.clone());
    let result = complete_install(&plan, &pmux, &file, !save, report);
    match result {
        Ok(notice) => Ok(Resume { notice, backup }),
        Err(error) => {
            if !save {
                let _ = std::fs::remove_file(&file);
            }
            let context = backup.map_or_else(
                || {
                    "No restore backup was requested; the temporary session snapshot was discarded"
                        .into()
                },
                |path| format!("Session configuration remains saved at {}", path.display()),
            );
            Err(error).context(context)
        }
    }
}

fn complete_install(
    plan: &Plan,
    pmux: &Path,
    consent: &Path,
    delete_consent: bool,
    report: &dyn Fn(Event),
) -> Result<String> {
    if plan.update_available {
        let installed: Value = serde_json::from_str(&update_with_progress(pmux, report)?)?;
        anyhow::ensure!(
            installed["status"] == "installed",
            "The release was not installed. {}",
            space_panel::maintenance_result(&installed.to_string())
        );
        anyhow::ensure!(installed["version"].as_str() == Some(plan.available.as_str()), "The installed release changed during the update. No sessions were stopped. Check again.");
    }
    // Resolve the replacement after installation, so the daemon, CLI and
    // MCP supervisor all use the same activated release.
    let installed = prismattyc_mux::release_update::installed_binary("pmux")
        .unwrap_or_else(|| pmux.to_path_buf());
    let installed_versions: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    verify_installed_versions(&installed_versions, &plan.available)?;
    require_same_sessions(plan, &installed)?;
    let consent = consent.to_string_lossy();
    let mut restart = vec![
        "restart".to_string(),
        "--daemon".to_string(),
        "--stop-sessions".to_string(),
        "--consented-config".to_string(),
        consent.into_owned(),
    ];
    if delete_consent {
        restart.push("--delete-consented-config".to_string());
    }
    restart.extend(["--worker".to_string(), "--json".to_string()]);
    let restart_args = restart.iter().map(String::as_str).collect::<Vec<_>>();
    report(Event::Restart(1, RESTARTS, "daemon and sessions"));
    run(&installed, &restart_args)
        .context("The files are installed, but the daemon restart failed")?;
    report(Event::Restart(2, RESTARTS, "MCP adapter"));
    let mcp = run(&installed, &["restart", "--mcp", "--json"])
        .context("The daemon restarted, but the MCP restart failed")?;
    let mut versions: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    verify_installed_versions(&versions, &plan.available)?;
    // This host is replaced next; do not present its old heartbeat as the
    // final running version in the replacement's result view.
    if let Some(running) = versions["running"].as_array_mut() {
        running.retain(|component| component["component"] != "host");
    }
    let actual = versions["daemon"]["version"].as_str().with_context(|| {
        format!(
            "could not verify the restarted daemon version: {}",
            versions["daemon"]["detail"]
                .as_str()
                .unwrap_or("daemon version unavailable")
        )
    })?;
    anyhow::ensure!(
        actual == plan.available,
        "The running daemon is {actual}; expected {}. The app has not restarted.",
        plan.available
    );
    report(Event::Restart(3, RESTARTS, "app"));
    Ok(format!(
        "Prismattyc {} is installed. The daemon restarted on {}.\n{}\n{}",
        plan.available,
        actual,
        space_panel::maintenance_result(&mcp),
        space_panel::maintenance_result(&versions.to_string())
    ))
}

/// Components the app restarts after installing: the daemon with its
/// sessions, the MCP adapter, then the app itself.
const RESTARTS: usize = 3;

fn verify_installed_versions(value: &Value, expected: &str) -> Result<()> {
    let installed = value["installed"]
        .as_array()
        .context("installed component versions were not reported")?;
    let required: &[&str] = if cfg!(target_os = "macos") {
        &["pmux", "pmuxd", "pmux-attach", "prismattyc-host"]
    } else {
        &prismattyc_mux::release_update::BINARIES
    };
    for name in required {
        anyhow::ensure!(
            installed.iter().any(
                |component| component["component"] == *name && component["version"].is_string()
            ),
            "The installed {name} version could not be read. No complete restart was performed."
        );
    }
    for component in installed {
        // Older macOS bundles did not include the optional command-line
        // frontend or MCP adapter. Their absence is not an install failure.
        let Some(label) = component["version"].as_str() else {
            continue;
        };
        anyhow::ensure!(
            label.split_whitespace().any(|word| word == expected),
            "Installed {} is {}, expected {}. The app has not restarted.",
            component["component"].as_str().unwrap_or("component"),
            label,
            expected
        );
    }
    Ok(())
}

fn command(pmux: &Path) -> std::process::Command {
    let mut cmd = prismattyc_mux::platform::hidden_command(pmux);
    if let Some(socket) = host_mux_socket() {
        cmd.arg("--socket").arg(socket);
    }
    if let Some(parent) = pmux.parent() {
        let server = parent.join(prismattyc_mux::platform::executable_name("pmuxd"));
        if server.is_file() {
            cmd.env("PMUX_SERVER", server);
        }
    }
    cmd
}

/// `pmux update --json`, with staged progress when the CLI supports it. A
/// command-line tool from before progress reporting rejects the option before
/// it downloads anything, so the update runs again without it.
fn update_with_progress(pmux: &Path, report: &dyn Fn(Event)) -> Result<String> {
    let mut forward = |progress| report(Event::Progress(progress));
    match run_with_progress(pmux, &["update", "--json", "--progress"], &mut forward) {
        Err(error) if format!("{error:#}").contains(r#"option "--progress""#) => {
            run(pmux, &["update", "--json"])
        }
        result => result,
    }
}

/// Run a command that reports progress lines on stderr. Each event goes to
/// `on_progress` as it arrives; other stderr lines are kept for the error.
fn run_with_progress(
    pmux: &Path,
    args: &[&str],
    on_progress: &mut dyn FnMut(Progress),
) -> Result<String> {
    let mut child = command(pmux)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("run update command")?;
    let mut stdout = child.stdout.take().context("update command output")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let stderr = child.stderr.take().context("update command messages")?;
    let messages = read_progress(BufReader::new(stderr), on_progress);
    let status = child.wait().context("wait for update command")?;
    let stdout = reader.join().unwrap_or_default();
    anyhow::ensure!(status.success(), "{stdout}{messages}");
    Ok(stdout.trim().into())
}

/// Hand each progress line to `on_progress` and return the other lines.
fn read_progress(input: impl BufRead, on_progress: &mut dyn FnMut(Progress)) -> String {
    let mut messages = String::new();
    for line in input.lines().map_while(Result::ok) {
        match progress::parse(&line) {
            Some(event) => on_progress(event),
            None => {
                messages.push_str(&line);
                messages.push('\n');
            }
        }
    }
    messages
}

pub(super) fn run(pmux: &Path, args: &[&str]) -> Result<String> {
    let output = command(pmux)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("run update command")?;
    anyhow::ensure!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

fn relaunch(resume: &Resume) -> Result<()> {
    let exe = prismattyc_mux::release_update::installed_binary("prismattyc-host")
        .or_else(|| std::env::current_exe().ok())
        .context("installed app executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.env("PRISMATTYC_COMPLETE_UPDATE", serde_json::to_string(resume)?);
    #[cfg(unix)]
    {
        use prismattyc_mux::platform::Exec;
        Err(cmd.exec()).context("launch updated app")
    }
    #[cfg(windows)]
    {
        instance::release();
        cmd.spawn().context("launch updated app")?;
        std::process::exit(0);
    }
}

pub(super) fn resume(app: &mut App) {
    let Some(raw) = std::env::var_os("PRISMATTYC_COMPLETE_UPDATE") else {
        return;
    };
    std::env::remove_var("PRISMATTYC_COMPLETE_UPDATE");
    let Ok(mut resume) = serde_json::from_str::<Resume>(&raw.to_string_lossy()) else {
        return;
    };
    resume.notice.push_str(&format!(
        "\nThe app restarted on {}.",
        env!("CARGO_PKG_VERSION")
    ));
    for host in app.windows.values_mut() {
        host.restore_prompt = None;
    }
    if let Some(path) = &resume.backup {
        let text = format!("{}\nSession configuration saved at {}. Restore these sessions and relaunch their saved commands?", resume.notice, path.display());
        app.full_update = Flow::Restore(resume);
        show(
            app,
            &text,
            &[
                ("Restore sessions", Action::Restore),
                ("Keep configuration for later", Action::KeepSaved),
            ],
        );
    } else {
        show(app, &resume.notice, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_or_mixed_installations_cannot_report_success() {
        let absent = serde_json::json!({"installed": []});
        assert!(verify_installed_versions(&absent, "0.3.41").is_err());
        let mixed = serde_json::json!({"installed": [
            {"component":"pmux", "version":"pmux 0.3.41"},
            {"component":"pmuxd", "version":"pmuxd 0.3.38"},
            {"component":"pmux-attach", "version":"pmux-attach 0.3.41"},
            {"component":"prismattyc-host", "version":"prismattyc-host 0.3.41"},
            {"component":"pmux-mcp", "version":"pmux-mcp 0.3.41"},
            {"component":"prismattyc", "version":"prismattyc 0.3.41"}
        ]});
        let error = verify_installed_versions(&mixed, "0.3.41")
            .unwrap_err()
            .to_string();
        assert!(error.contains("pmuxd 0.3.38"), "{error}");
        assert!(error.contains("expected 0.3.41"), "{error}");
    }

    #[test]
    fn progress_lines_are_forwarded_in_order_and_other_lines_kept() {
        let first = Progress::Download {
            item: "pmux".into(),
            received: 10,
            total: 40,
        };
        let second = Progress::Verify {
            item: "pmux".into(),
            index: 1,
            count: 6,
        };
        let input = format!(
            "Downloading pmux 0.3.70\n{}\n{}\nwarning: slow mirror\n",
            progress::line(&first),
            progress::line(&second)
        );
        let mut seen = Vec::new();
        let messages = read_progress(input.as_bytes(), &mut |event| seen.push(event));
        assert_eq!(seen, [first, second]);
        assert_eq!(messages, "Downloading pmux 0.3.70\nwarning: slow mirror\n");
    }

    #[cfg(unix)]
    fn fake_pmux(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "update-flow-{name}-{}-{}",
            std::process::id(),
            prismattyc_mux::host_render_status::unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pmux");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_running_update_streams_progress_and_returns_its_report() {
        let event = Progress::Install {
            item: "core binaries".into(),
        };
        let pmux = fake_pmux(
            "stream",
            &format!(
                "echo 'Downloading pmux' >&2\necho '{}' >&2\necho '{{\"status\":\"installed\"}}'",
                progress::line(&event)
            ),
        );
        let seen = std::cell::RefCell::new(Vec::new());
        let report = |event| seen.borrow_mut().push(event);
        let output = update_with_progress(&pmux, &report).unwrap();
        assert_eq!(output, r#"{"status":"installed"}"#);
        let seen = seen.into_inner();
        assert!(
            matches!(seen.as_slice(), [Event::Progress(p)] if *p == event),
            "{seen:?}"
        );
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_update_reports_its_messages_but_not_progress_lines() {
        let pmux = fake_pmux(
            "fail",
            &format!(
                "echo '{}' >&2\necho 'download digest mismatch for pmuxd' >&2\nexit 1",
                progress::line(&Progress::Download {
                    item: "pmuxd".into(),
                    received: 1,
                    total: 2
                })
            ),
        );
        let error = update_with_progress(&pmux, &|_| {})
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("download digest mismatch for pmuxd"),
            "{error}"
        );
        assert!(!error.contains(progress::PREFIX), "{error}");
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_older_cli_without_progress_still_updates() {
        // The release CLI's wording for an option it does not know.
        let pmux = fake_pmux(
            "older",
            r#"for arg in "$@"; do
  if [ "$arg" = --progress ]; then
    echo 'unknown release update option "--progress"; use --help' >&2
    exit 1
  fi
done
echo '{"status":"installed"}'"#,
        );
        let output = update_with_progress(&pmux, &|_| {}).unwrap();
        assert_eq!(output, r#"{"status":"installed"}"#);
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }
}
