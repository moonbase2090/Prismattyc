// SPDX-License-Identifier: MIT
//! One owner for the consent, installation, restart, and restoration flow.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    Restore(Resume),
}

fn show(app: &mut App, text: &str, actions: &[(&str, Action)]) {
    for host in app.windows.values_mut() {
        space_panel::update_page(host, text, actions);
    }
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
            app.full_update = Flow::Busy;
            show(app, "Downloading and verifying the release, then installing and restarting all components. Please wait.", &[]);
            let proxy = app.event_proxy.clone();
            std::thread::spawn(move || {
                let result = install(plan, matches!(action, Action::SaveAndContinue))
                    .map_err(|e| format!("{e:#}"));
                let _ = proxy.send_event(UserAction::FullUpdate(Event::Installed(result)));
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
        Event::Installed(Ok(resume)) => {
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
        Event::Prepared(Err(error))
        | Event::Installed(Err(error))
        | Event::Restored(Err(error)) => {
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

fn install(plan: Plan, save: bool) -> Result<Resume> {
    let pmux = find_mux_bin();
    require_same_sessions(&plan, &pmux)?;
    let file = snapshot_path("update")?;
    run(&pmux, &["session-config", "save", &file.to_string_lossy()])?;
    let backup = save.then_some(file.clone());
    let result = complete_install(&plan, &pmux, &file, !save);
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
) -> Result<String> {
    if plan.update_available {
        let report: Value = serde_json::from_str(&run(pmux, &["update", "--json"])?)?;
        anyhow::ensure!(
            report["status"] == "installed",
            "The release was not installed. {}",
            space_panel::maintenance_result(&report.to_string())
        );
        anyhow::ensure!(report["version"].as_str() == Some(plan.available.as_str()), "The installed release changed during the update. No sessions were stopped. Check again.");
    }
    // Resolve the replacement after installation, so the daemon, CLI and
    // MCP supervisor all use the same activated release.
    let installed = prismattyc_mux::release_update::installed_binary("pmux")
        .unwrap_or_else(|| pmux.to_path_buf());
    let installed_versions: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    verify_installed_versions(&installed_versions, &plan.available)?;
    require_same_sessions(plan, &installed)?;
    // Every other open app process must restart too, or it keeps running the
    // old release while this result reports success. They restart first, while
    // their sessions still exist: a window whose sessions were already stopped
    // cannot save its view and would defer.
    let own = std::process::id();
    let others_before: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    let others = other_host_pids(&others_before, own);
    let host_restart = if others.is_empty() {
        None
    } else {
        let own = own.to_string();
        let args = ["restart", "--host", "--except-pid", own.as_str(), "--json"];
        Some(
            run(&installed, &args)
                .context("The files are installed, but restarting the other app windows failed. No sessions were stopped")?,
        )
    };
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
    run(&installed, &restart_args)
        .context("The files are installed, but the daemon restart failed")?;
    let mcp = run(&installed, &["restart", "--mcp", "--json"])
        .context("The daemon restarted, but the MCP restart failed")?;
    let mut versions: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    verify_installed_versions(&versions, &plan.available)?;
    // This host is replaced next; do not present its old heartbeat as the
    // final running version in the replacement's result view.
    if let Some(running) = versions["running"].as_array_mut() {
        running.retain(|component| {
            component["component"] != "host" || component["pid"].as_u64() != Some(u64::from(own))
        });
    }
    let host_lines = other_host_lines(host_restart.as_deref(), &versions, &plan.available);
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
    Ok(format!(
        "Prismattyc {} is installed. The daemon restarted on {}.\n{}\n{}{}",
        plan.available,
        actual,
        space_panel::maintenance_result(&mcp),
        space_panel::maintenance_result(&versions.to_string()),
        host_lines
    ))
}

/// Process ids of every live app process except this one.
fn other_host_pids(versions: &Value, own: u32) -> Vec<u64> {
    versions["running"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|component| component["component"] == "host")
        .filter_map(|component| component["pid"].as_u64())
        .filter(|pid| *pid != u64::from(own))
        .collect()
}

/// One line per other app process that is not on the new release, with the
/// exact next action. Empty when every other app process was restarted.
fn other_host_lines(restart: Option<&str>, versions: &Value, expected: &str) -> String {
    let mut lines = Vec::new();
    let results: Value = restart
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null);
    for entry in results["components"].as_array().into_iter().flatten() {
        let response = entry.get("response").unwrap_or(entry);
        match response["status"].as_str().unwrap_or("") {
            "restarted" => {}
            "deferred" => lines.push(format!(
                "Another Prismattyc app window was not restarted and still runs the previous version: {} Next: fix that, then choose Update and restart all components again, or close and reopen that window.",
                response["detail"].as_str().unwrap_or("it asked to wait.")
            )),
            _ => lines.push(
                "Another Prismattyc app window did not confirm its restart. Next: close and reopen it, then check running versions."
                    .to_owned(),
            ),
        }
    }
    let own = u64::from(std::process::id());
    for component in versions["running"].as_array().into_iter().flatten() {
        if component["component"] != "host" || component["pid"].as_u64() == Some(own) {
            continue;
        }
        let version = component["version"].as_str().unwrap_or("");
        if version != expected {
            lines.push(format!(
                "Prismattyc app process {} still reports {}, not {expected}.",
                component["pid"],
                if version.is_empty() {
                    "an unreadable version"
                } else {
                    version
                }
            ));
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n{}", lines.join("\n"))
    }
}

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

/// Run a pmux command and return its standard output.
///
/// Output is collected in private files, not pipes. A daemon the command
/// starts can end up holding a copy of a pipe's write end (Windows children
/// inherit every inheritable handle), and then the end of output never comes,
/// so waiting on a pipe could block until that daemon exits.
pub(super) fn run(pmux: &Path, args: &[&str]) -> Result<String> {
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
    let capture = Capture::new()?;
    let status = cmd
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(capture.create("stdout")?)
        .stderr(capture.create("stderr")?)
        .status()
        .context("run update command")?;
    let stdout = capture.read("stdout")?;
    anyhow::ensure!(status.success(), "{stdout}{}", capture.read("stderr")?);
    Ok(stdout.trim().into())
}

/// A private directory for one command's output, removed when dropped.
struct Capture {
    dir: PathBuf,
}

impl Capture {
    fn new() -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-run-{}-{}-{}",
            std::process::id(),
            prismattyc_mux::host_render_status::unix_ms(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        prismattyc_mux::private_fs::create_dir(&dir).context("create command output directory")?;
        Ok(Capture { dir })
    }

    fn create(&self, name: &str) -> Result<std::fs::File> {
        prismattyc_mux::platform::private_options()
            .write(true)
            .create_new(true)
            .open(self.dir.join(name))
            .with_context(|| format!("create command {name} file"))
    }

    fn read(&self, name: &str) -> Result<String> {
        let bytes =
            std::fs::read(self.dir.join(name)).with_context(|| format!("read command {name}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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
    fn a_child_left_holding_the_output_does_not_block_the_result() {
        // The command starts a background process that keeps its standard
        // output open, as a started daemon could. Its pid is kept to end it.
        let pmux = fake_pmux(
            "held",
            r#"sleep 30 &
echo $! > "$(dirname "$0")/held.pid"
echo done
echo note >&2"#,
        );
        let started = std::time::Instant::now();
        let output = run(&pmux, &[]).unwrap();
        let elapsed = started.elapsed();
        let pid = std::fs::read_to_string(pmux.parent().unwrap().join("held.pid")).unwrap();
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        assert_eq!(output, "done");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "returned when the command exited, not when its child did: {elapsed:?}"
        );
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_command_reports_both_outputs_and_leaves_no_files() {
        let pmux = fake_pmux("fails", "echo out; echo err >&2; exit 3");
        let before: std::collections::BTreeSet<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| {
                name.to_string_lossy()
                    .starts_with(&format!("prismattyc-run-{}-", std::process::id()))
            })
            .collect();
        let error = run(&pmux, &[]).unwrap_err().to_string();
        assert!(error.contains("out") && error.contains("err"), "{error}");
        let after: std::collections::BTreeSet<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| {
                name.to_string_lossy()
                    .starts_with(&format!("prismattyc-run-{}-", std::process::id()))
            })
            .collect();
        assert!(after.is_subset(&before), "output files removed: {after:?}");
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }
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
    fn only_other_app_processes_are_restarted_and_reported() {
        let versions = serde_json::json!({"running": [
            {"component":"host","pid":10,"version":"0.3.69"},
            {"component":"host","pid":11,"version":"0.3.69"},
            {"component":"mcp","pid":12,"version":"0.3.69"},
            {"component":"host","pid":13,"version":"0.3.69"}
        ]});
        assert_eq!(other_host_pids(&versions, 10), vec![11, 13]);
        assert!(other_host_pids(&serde_json::json!({"running": []}), 10).is_empty());
    }

    #[test]
    fn other_windows_left_on_the_old_release_are_named_with_a_next_action() {
        let versions = serde_json::json!({"running": [
            {"component":"host","pid":11,"version":"0.3.70"},
            {"component":"host","pid":13,"version":"0.3.69"}
        ]});
        let restart = r#"{"status":"restart_results","components":[
            {"component":"host","response":{"status":"restarted","detail":"","version":"0.3.70"}},
            {"component":"host","response":{"status":"deferred","detail":"Restart deferred: this window owns blank terminals; close those terminals before restarting the host","version":"0.3.69"}}
        ]}"#;
        let lines = other_host_lines(Some(restart), &versions, "0.3.70");
        assert!(lines.contains("blank terminals"), "{lines}");
        assert!(lines.contains("Next:"), "{lines}");
        assert!(
            lines.contains("process 13 still reports 0.3.69, not 0.3.70"),
            "{lines}"
        );
        assert!(!lines.contains("process 11"), "{lines}");
    }

    #[test]
    fn nothing_is_reported_when_every_other_window_restarted() {
        let versions = serde_json::json!({"running": [
            {"component":"host","pid":11,"version":"0.3.70"}
        ]});
        let restart = r#"{"components":[{"component":"host","response":{"status":"restarted","detail":""}}]}"#;
        assert_eq!(other_host_lines(Some(restart), &versions, "0.3.70"), "");
        assert_eq!(other_host_lines(None, &versions, "0.3.70"), "");
    }

    #[test]
    fn an_unconfirmed_restart_asks_for_a_reopen() {
        let restart =
            r#"{"components":[{"component":"host","status":"pending","request_id":"x"}]}"#;
        let lines = other_host_lines(Some(restart), &serde_json::json!({"running": []}), "0.3.70");
        assert!(lines.contains("did not confirm"), "{lines}");
        assert!(lines.contains("close and reopen"), "{lines}");
    }
}
