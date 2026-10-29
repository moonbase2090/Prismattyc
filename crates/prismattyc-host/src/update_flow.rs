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
            let text = confirmation(&plan);
            app.full_update = Flow::Confirm(plan);
            show(
                app,
                &text,
                &[
                    ("Save and continue", Action::SaveAndContinue),
                    ("Continue without saving", Action::Continue),
                    ("Cancel", Action::Cancel),
                ],
            );
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

fn confirmation(plan: &Plan) -> String {
    let release = if plan.update_available {
        format!("Prismattyc {} is available. It will be downloaded, verified, and installed before anything is stopped.", plan.available)
    } else {
        format!("Prismattyc {} is already installed. Continue to restart all components on that version.", plan.available)
    };
    let mut text = format!("{release}\n{} sessions are running and will be stopped. All programs in them will end. Open app windows and local terminals will also restart.", plan.sessions.len());
    for (_, name) in &plan.sessions {
        text.push_str(&format!("\n{name}"));
    }
    text.push_str("\nSave the session configuration first? This keeps names, directories, launch commands, and pane layouts. After restart you can choose whether to recreate the sessions and relaunch their commands. Unsaved work inside programs is not saved.");
    text
}

fn prepare() -> Result<Plan> {
    let check: Value =
        serde_json::from_str(&run(&find_mux_bin(), &["update", "--check", "--json"])?)?;
    Ok(Plan {
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

fn install(plan: Plan, save: bool) -> Result<Resume> {
    let pmux = find_mux_bin();
    require_same_sessions(&plan, &pmux)?;
    let directory = prismattyc_mux::spaces_dir()
        .parent()
        .context("configuration data directory")?
        .join("session-configurations");
    prismattyc_mux::private_fs::create_dir(&directory)?;
    let file = directory.join(format!(
        "update-{}-{}.json",
        std::process::id(),
        prismattyc_mux::host_render_status::unix_ms()
    ));
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
        running.retain(|component| component["component"] != "host");
    }
    let actual = versions["daemon"]["version"]
        .as_str()
        .context("could not verify the restarted daemon version")?;
    anyhow::ensure!(
        actual == plan.available,
        "The running daemon is {actual}; expected {}. The app has not restarted.",
        plan.available
    );
    Ok(format!(
        "Prismattyc {} is installed. The daemon restarted on {}.\n{}\n{}",
        plan.available,
        actual,
        space_panel::maintenance_result(&mcp),
        space_panel::maintenance_result(&versions.to_string())
    ))
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

fn run(pmux: &Path, args: &[&str]) -> Result<String> {
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
    let output = cmd
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
    fn confirmation_names_every_session_and_requires_a_choice() {
        let plan = Plan {
            available: "0.3.41".into(),
            update_available: true,
            sessions: vec![(1, "work".into()), (2, "build".into())],
        };
        let text = confirmation(&plan);
        assert!(text.contains("2 sessions are running and will be stopped."));
        assert!(text.contains("\nwork\nbuild\n"));
        assert!(text.contains("Unsaved work inside programs is not saved."));
        assert!(text.contains("After restart you can choose"));
    }
}
