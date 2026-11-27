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

/// Which confirmation Restart now and the update notice use.
/// The command palette keeps the full review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ConfirmStyle {
    /// Three choices, with the session list folded.
    #[default]
    Review,
    /// The session list is shown, and Save and restart is the one confirm.
    OneConfirm,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Action {
    /// Command palette: open the full review.
    Start,
    /// Restart now, or the post-update prompt: one confirm.
    StartOneConfirm,
    SaveAndContinue,
    Continue,
    /// Restart the pmux server after the last session ends.
    RestartWhenIdle,
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

impl Plan {
    /// The version every component runs afterwards: the new release, or the
    /// installed one when there is nothing newer to install. A build newer
    /// than the newest release restarts on itself.
    fn target(&self) -> &str {
        if self.update_available {
            &self.available
        } else {
            &self.installed
        }
    }
}

#[derive(Debug)]
pub(super) enum Event {
    Prepared(Result<Plan, String>),
    /// A stage reported by `pmux update --progress` or `pmux restart
    /// --progress`, or a restart step the app takes itself.
    Progress(Progress),
    /// A check the app makes itself within the current step.
    Doing(&'static str),
    /// Move the bar of a step that has no fraction.
    Tick,
    Installed(Result<Box<Resume>, String>),
    Restored(Result<String, String>),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct Resume {
    notice: String,
    backup: Option<PathBuf>,
    /// Set when the restarted daemon already brought the saved sessions
    /// back. An app from before this field asks before restoring instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restored: Option<Restored>,
    /// Every app window's tabs, size and place when the user chose Save and
    /// restart. Used only when the sessions came back with their IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    views: Vec<restart::WindowState>,
    /// The update's meter, which the relaunched app finishes. An app from
    /// before this field relaunches without it and shows only the notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<Carried>,
}

/// The progress an update carries into the relaunched app.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct Carried {
    installed: String,
    available: Option<String>,
    /// The meter as the app left it, at the relaunch step.
    meter: Option<Meter>,
    /// The last step, which the relaunched app takes: opening its windows.
    open_windows: Progress,
}

impl Carried {
    /// Take the last step and finish the meter; `None` without a meter.
    fn finish(&self) -> Option<Meter> {
        let mut meter = self.meter.clone()?;
        meter.apply(&self.open_windows);
        eprintln!("prismattyc-host: update progress: {}", meter.step_line());
        meter.finish();
        eprintln!("prismattyc-host: update progress: {}", meter.step_line());
        Some(meter)
    }
}

/// Which saved sessions the restarted daemon brought back, by name.
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub(super) struct Restored {
    restored: Vec<String>,
    lost: Vec<String>,
}

impl Restored {
    /// The `sessions` entry of a `pmux restart --json` report, when the
    /// restarted daemon confirmed which sessions it brought back. An
    /// unconfirmed or malformed entry is `None`, so the app asks instead.
    fn from_report(report: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(report).ok()?;
        let entry = value["components"]
            .as_array()?
            .iter()
            .find(|component| component["component"] == "sessions")?;
        if !matches!(entry["status"].as_str(), Some("restored" | "partial"))
            || !entry["restored"].is_array()
            || !entry["lost"].is_array()
        {
            return None;
        }
        let names = |key: &str| -> Vec<String> {
            entry[key]
                .as_array()
                .map(|names| {
                    names
                        .iter()
                        .filter_map(|name| name.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Self {
            restored: names("restored"),
            lost: names("lost"),
        })
    }

    fn summary(&self) -> String {
        let mut text = match self.restored.len() {
            0 => "No sessions were restored.".to_string(),
            1 => format!(
                "Restored 1 session with its layout and scrollback: {}.",
                self.restored[0]
            ),
            n => format!(
                "Restored {n} sessions with their layouts and scrollback: {}.",
                self.restored.join(", ")
            ),
        };
        if !self.restored.is_empty() {
            text.push_str(" Their programs started again.");
        }
        if !self.lost.is_empty() {
            text.push_str(&format!(
                "\nNot restored: {}. Restore them from the saved configuration.",
                self.lost.join(", ")
            ));
        }
        text
    }
}

#[derive(Default)]
pub(super) enum Flow {
    #[default]
    Idle,
    Busy,
    Confirm(Plan),
    /// Installing and restarting; the flag stops the meter's ticker.
    Installing(Meter, Arc<AtomicBool>),
    Restore(Box<Resume>),
}

fn show(app: &mut App, text: &str, actions: &[(&str, Action)]) {
    for host in app.windows.values_mut() {
        space_panel::update_page(host, text, actions);
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
    restart_modal::show_progress(app, &meter);
}

fn check_again(app: &App) -> Action {
    match app.update_confirm {
        ConfirmStyle::OneConfirm => Action::StartOneConfirm,
        ConfirmStyle::Review => Action::Start,
    }
}

/// Check the release, then open the confirmation for `app.update_confirm`.
fn start_prepare(app: &mut App) {
    // Check again after a failed update closes its dialog first.
    restart_modal::close(app);
    app.full_update = Flow::Busy;
    let text = if app.idle_restart_commit {
        "The last session ended. Checking the pmux server before it restarts."
    } else if app.update_confirm == ConfirmStyle::OneConfirm {
        "Checking the pmux server. Your sessions are still running."
    } else {
        "Checking for updates. Your sessions are still running."
    };
    show(app, text, &[]);
    let proxy = app.event_proxy.clone();
    std::thread::spawn(move || {
        let result = prepare().map_err(|e| format!("{e:#}"));
        let _ = proxy.send_event(UserAction::FullUpdate(Event::Prepared(result)));
    });
}

pub(super) fn action(app: &mut App, action: Action) {
    // No session is running, so waiting would only delay the restart.
    let idle_sessions = match (&action, &app.full_update) {
        (Action::RestartWhenIdle, Flow::Confirm(plan)) => {
            Some((plan.sessions.is_empty(), plan.clone()))
        }
        (Action::RestartWhenIdle, _) => return,
        _ => None,
    };
    let action = if let Some((empty, plan)) = idle_sessions {
        if empty {
            Action::SaveAndContinue
        } else {
            let consent = match idle_consent_for(&plan) {
                Ok(consent) => consent,
                Err(error) => {
                    show(
                        app,
                        &format!("The pmux server was not set to restart later.\n{error:#}"),
                        &[("Check again", check_again(app))],
                    );
                    return;
                }
            };
            if let Err(error) = remember_idle_restart(&idle_restart_file(), &consent) {
                show(
                    app,
                    &format!("The pmux server was not set to restart later.\n{error}"),
                    &[("Check again", check_again(app))],
                );
                return;
            }
            restart_modal::close(app);
            app.full_update = Flow::Idle;
            show(
                app,
                "The pmux server will restart after the last session ends. Nothing stops now.",
                &[],
            );
            ensure_idle_watch(app);
            return;
        }
    } else {
        action
    };
    match action {
        Action::Start | Action::StartOneConfirm if !matches!(app.full_update, Flow::Busy) => {
            app.idle_restart_commit = false;
            app.update_confirm = if matches!(action, Action::StartOneConfirm) {
                ConfirmStyle::OneConfirm
            } else {
                ConfirmStyle::Review
            };
            start_prepare(app);
        }
        Action::SaveAndContinue | Action::Continue => {
            clear_idle_restart(&idle_restart_file());
            app.idle_restart_commit = false;
            let Flow::Confirm(plan) = &app.full_update else {
                return;
            };
            let plan = plan.clone();
            let save = matches!(action, Action::SaveAndContinue);
            // Record each window now, while its sessions are still running.
            // A window without sessions (only blank terminals) is not kept;
            // a session window whose view cannot be saved stops the update
            // before anything is stopped.
            let views = if save {
                match app
                    .windows
                    .values_mut()
                    .map(restart::save_window_view)
                    .collect::<Result<Vec<_>>>()
                {
                    Ok(views) => views.into_iter().flatten().collect(),
                    Err(error) => {
                        restart_modal::close(app);
                        app.full_update = Flow::Idle;
                        show(
                            app,
                            &format!("The update did not start: a window could not be saved ({error:#}). Your sessions are still running."),
                            &[("Check again", check_again(app))],
                        );
                        return;
                    }
                }
            } else {
                Vec::new()
            };
            let meter = Meter::new(plan.update_available);
            let stop = Arc::new(AtomicBool::new(false));
            // The dialog turns into the update's progress.
            restart_modal::show_progress(app, &meter);
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
                let result = install(plan, save, views, &report).map_err(|e| format!("{e:#}"));
                report(Event::Installed(result.map(Box::new)));
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
        Action::RestartWhenIdle => {}
        Action::Cancel => {
            withdraw_idle_restart();
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
        Action::Start | Action::StartOneConfirm => {}
    }
}

pub(super) fn event(app: &mut App, event: Event) {
    match event {
        Event::Prepared(Ok(plan)) => {
            if std::mem::take(&mut app.idle_restart_commit) {
                if !plan.sessions.is_empty() {
                    app.full_update = Flow::Idle;
                    show(
                        app,
                        "A session is still running. The pmux server restarts when the last session ends.",
                        &[],
                    );
                    ensure_idle_watch(app);
                    return;
                }
                let saved = idle_restart_consent(&idle_restart_file());
                let fresh = idle_consent_for(&plan).ok();
                if idle_restart_matches(saved.as_ref(), fresh.as_ref()) {
                    let installed = plan.installed.clone();
                    let available = plan.update_available.then(|| plan.available.clone());
                    clear_idle_restart(&idle_restart_file());
                    restart_modal::open(
                        app,
                        &installed,
                        available.as_deref(),
                        &[],
                        ConfirmStyle::OneConfirm,
                    );
                    app.full_update = Flow::Confirm(plan);
                    action(app, Action::SaveAndContinue);
                } else {
                    withdraw_idle_restart();
                    let installed = plan.installed.clone();
                    let available = plan.update_available.then(|| plan.available.clone());
                    let names: Vec<String> =
                        plan.sessions.iter().map(|(_, name)| name.clone()).collect();
                    restart_modal::open(
                        app,
                        &installed,
                        available.as_deref(),
                        &names,
                        ConfirmStyle::OneConfirm,
                    );
                    app.full_update = Flow::Confirm(plan);
                    show(
                            app,
                            "The release or the pending plug-ins changed, so that restart was withdrawn. Review this one.",
                            &[],
                        );
                }
                return;
            }
            let names: Vec<String> = plan.sessions.iter().map(|(_, name)| name.clone()).collect();
            let style = app.update_confirm;
            restart_modal::open(
                app,
                &plan.installed,
                plan.update_available.then_some(plan.available.as_str()),
                &names,
                style,
            );
            app.full_update = Flow::Confirm(plan);
        }
        Event::Progress(progress) => refresh_meter(app, |meter| {
            meter.apply(&progress);
            true
        }),
        Event::Doing(text) => refresh_meter(app, |meter| {
            meter.doing(text);
            true
        }),
        Event::Tick => refresh_meter(app, Meter::tick),
        Event::Installed(Err(error)) => {
            let text = format!("The operation could not finish.\n{error}");
            let again = check_again(app);
            let actions = [("Check again", again)];
            match std::mem::take(&mut app.full_update) {
                Flow::Installing(mut meter, stop) => {
                    stop.store(true, Ordering::Relaxed);
                    meter.fail();
                    eprintln!("prismattyc-host: update progress: {}", meter.step_line());
                    if restart_modal::is_open(app) {
                        restart_modal::fail(app, &meter, &error);
                    } else {
                        show(app, &text, &actions);
                    }
                }
                _ => show(app, &text, &actions),
            }
        }
        Event::Installed(Ok(mut resume)) => {
            if let Flow::Installing(meter, stop) = &app.full_update {
                stop.store(true, Ordering::Relaxed);
                if let Some(carried) = resume.progress.as_mut() {
                    carried.meter = Some(meter.clone());
                }
            }
            if let Err(error) = relaunch(&resume) {
                app.full_update = Flow::Restore(resume.clone());
                show(app, &format!("{}\nCould not restart the app: {error:#}. Quit and reopen Prismattyc. Your session configuration is saved at {}.", resume.notice, resume.backup.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "no backup was requested".into())), &[]);
            }
        }
        Event::Restored(Ok(text)) => {
            app.full_update = Flow::Idle;
            reopen_views(app);
            show(app, &format!("{text}\nSession restoration finished. Open a saved Space or select a session to view it."), &[]);
        }
        Event::Prepared(Err(error)) | Event::Restored(Err(error)) => {
            let idle = std::mem::take(&mut app.idle_restart_commit);
            app.full_update = Flow::Idle;
            show(
                app,
                &format!("The operation could not finish.\n{error}"),
                &[("Check again", check_again(app))],
            );
            if idle {
                ensure_idle_watch(app);
            }
        }
    }
}

/// Reopen each window's saved tabs on the sessions that now exist, and let
/// the windows write their view again.
fn reopen_views(app: &mut App) {
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

fn install(
    plan: Plan,
    save: bool,
    views: Vec<restart::WindowState>,
    report: &dyn Fn(Event),
) -> Result<Resume> {
    let pmux = find_mux_bin();
    require_same_sessions(&plan, &pmux)?;
    let file = snapshot_path("update")?;
    report(Event::Doing("Saving the session configuration"));
    run(&pmux, &["session-config", "save", &file.to_string_lossy()])?;
    let backup = save.then_some(file.clone());
    let result = complete_install(&plan, &pmux, &file, !save, report);
    match result {
        Ok((notice, restored, steps)) => Ok(Resume {
            notice,
            backup,
            views: if restored.is_some() {
                views
            } else {
                Vec::new()
            },
            restored,
            progress: Some(Carried {
                installed: plan.installed.clone(),
                available: plan.update_available.then(|| plan.available.clone()),
                meter: None,
                open_windows: restart_progress("Opening the windows", steps + 3, steps),
            }),
        }),
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
) -> Result<(String, Option<Restored>, usize)> {
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
    let launcher = monitor_launcher(&installed, plan, report);
    report(Event::Doing("Checking the installed versions"));
    let installed_versions: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    verify_installed_versions(&installed_versions, plan.target())?;
    report(Event::Doing("Checking the sessions"));
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
    let (restarted, steps) = restart_daemon(&installed, &restart_args, report)
        .context("The files are installed, but the daemon restart failed")?;
    let restored = (!delete_consent)
        .then(|| Restored::from_report(&restarted))
        .flatten();
    report(restart_step("Restarting the MCP adapter", steps + 1, steps));
    let mcp = run(&installed, &["restart", "--mcp", "--json"])
        .context("The daemon restarted, but the MCP restart failed")?;
    report(Event::Doing("Checking the running versions"));
    let mut versions: Value = serde_json::from_str(&run(&installed, &["versions"])?)?;
    verify_installed_versions(&versions, plan.target())?;
    // This host is replaced next; do not present its old heartbeat as the
    // final running version in the replacement's result view.
    if let Some(running) = versions["running"].as_array_mut() {
        running.retain(|component| {
            component["component"] != "host" || component["pid"].as_u64() != Some(u64::from(own))
        });
    }
    let host_lines = other_host_lines(host_restart.as_deref(), &versions, plan.target());
    let actual = versions["daemon"]["version"].as_str().with_context(|| {
        format!(
            "could not verify the restarted daemon version: {}",
            versions["daemon"]["detail"]
                .as_str()
                .unwrap_or("daemon version unavailable")
        )
    })?;
    anyhow::ensure!(
        actual == plan.target(),
        "The running daemon is {actual}; expected {}. The app has not restarted.",
        plan.target()
    );
    report(restart_step("Relaunching the app", steps + 2, steps));
    Ok((
        format!(
            "Prismattyc {} is installed. The daemon restarted on {}.\n{}{}\n{}{}",
            plan.target(),
            actual,
            launcher,
            space_panel::maintenance_result(&mcp),
            space_panel::maintenance_result(&versions.to_string()),
            host_lines
        ),
        restored,
        steps,
    ))
}

/// The monitor launcher is part of the update on Linux and Windows (the
/// macOS app carries its own). Returns the result's line for it, ending in a
/// newline, or nothing on macOS. A launcher that cannot be replaced is kept
/// and named with what to do; it does not stop the restart.
fn monitor_launcher(pmux: &Path, plan: &Plan, report: &dyn Fn(Event)) -> String {
    if cfg!(target_os = "macos") {
        return String::new();
    }
    report(Event::Doing("Updating the monitor launcher"));
    let launcher =
        prismattyc_mux::release_update::repair_monitor_launcher(pmux, &plan.installed, None);
    let mut line = format!("Monitor launcher: {}\n", launcher.summary(plan.target()));
    if let Some(next) = launcher.next(plan.target()) {
        line.push_str(&next);
        line.push('\n');
    }
    line
}

/// Steps the app takes itself after the daemon restart: the MCP adapter,
/// the relaunch, then (in the relaunched app) opening the windows.
const AFTER_DAEMON: usize = 3;

/// Restart step `index`, counted with the app's own steps after the
/// daemon's `daemon_steps`.
fn restart_progress(item: &str, index: usize, daemon_steps: usize) -> Progress {
    Progress::Restart {
        item: item.into(),
        index,
        count: daemon_steps + AFTER_DAEMON,
    }
}

fn restart_step(item: &str, index: usize, daemon_steps: usize) -> Event {
    Event::Progress(restart_progress(item, index, daemon_steps))
}

/// Run the daemon restart and report each of its steps; returns its report
/// and how many steps it took. A command-line tool from before restart
/// progress rejects the option before it stops anything, so the restart
/// then runs without it, as one step.
fn restart_daemon(pmux: &Path, args: &[&str], report: &dyn Fn(Event)) -> Result<(String, usize)> {
    let steps = std::cell::Cell::new(0);
    let mut forward = |progress| {
        if let Progress::Restart { item, index, count } = progress {
            steps.set(count);
            report(restart_step(&item, index, count));
        }
    };
    let with_progress = [args, &["--progress"]].concat();
    match run_with_progress(pmux, &with_progress, &mut forward) {
        Err(error) if format!("{error:#}").contains(r#"option "--progress""#) => {
            report(restart_step("Restarting the daemon and sessions", 1, 1));
            Ok((run(pmux, args)?, 1))
        }
        result => result.map(|report| (report, steps.get())),
    }
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
    let mut child = prismattyc_process::spawn(
        command(pmux)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    )
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

/// Run a pmux command and return its standard output.
///
/// Output is collected in private files, not pipes. A daemon the command
/// starts can end up holding a copy of a pipe's write end (Windows children
/// inherit every inheritable handle), and then the end of output never comes,
/// so waiting on a pipe could block until that daemon exits.
pub(super) fn run(pmux: &Path, args: &[&str]) -> Result<String> {
    let capture = Capture::new()?;
    let status = prismattyc_process::status(
        command(pmux)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(capture.create("stdout")?)
            .stderr(capture.create("stderr")?),
    )
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
        Err(prismattyc_process::exec(&mut cmd)).context("launch updated app")
    }
    #[cfg(windows)]
    {
        instance::release();
        prismattyc_process::spawn(&mut cmd).context("launch updated app")?;
        std::process::exit(0);
    }
}

/// After an update that kept the sessions, open every saved app window at
/// its saved size and place, each on its own saved view file. Returns false
/// when there is nothing to open, so the app opens its usual first window.
/// [`resume`] then applies each window's tabs and shows the update notice.
pub(super) fn open_saved_windows(
    app: &mut App,
    event_loop: &ActiveEventLoop,
) -> (bool, Vec<String>) {
    let Some(raw) = std::env::var_os("PRISMATTYC_COMPLETE_UPDATE") else {
        return (false, Vec::new());
    };
    let Ok(resume) = serde_json::from_str::<Resume>(&raw.to_string_lossy()) else {
        return (false, Vec::new());
    };
    if resume.restored.is_none() || resume.views.is_empty() {
        return (false, Vec::new());
    }
    let (opened, failures) = restart::reopen(app, event_loop, &resume.views);
    (opened > 0, failures)
}

/// `unopened` names each saved window the relaunch could not reopen, and why.
pub(super) fn resume(app: &mut App, unopened: &[String]) {
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
    // The previous app's meter ends here, on its receipt.
    if let Some(carried) = &resume.progress {
        if let Some(meter) = carried.finish() {
            restart_modal::show_finished(
                app,
                &carried.installed,
                carried.available.as_deref(),
                &meter,
            );
        }
    }
    if let Some(restored) = &resume.restored {
        // The restarted daemon already brought the sessions back with their
        // IDs, so the saved tabs open on them directly. The user agreed to
        // this when choosing Save and restart; there is nothing more to ask.
        let mut text = format!("{}\n{}", resume.notice, restored.summary());
        for reason in unopened {
            text.push_str(&format!(
                "\nA saved window could not be reopened: {reason}."
            ));
        }
        if let Some(path) = &resume.backup {
            text.push_str(&format!(
                "\nThe session configuration also remains saved at {}.",
                path.display()
            ));
        }
        app.full_update = Flow::Idle;
        // Every window, reopened from its saved view or the usual first
        // one, applies its saved tabs to the sessions that came back.
        reopen_views(app);
        show(app, &text, &[]);
        return;
    }
    if let Some(path) = &resume.backup {
        let text = format!("{}\nSession configuration saved at {}. Restore these sessions and relaunch their saved commands?", resume.notice, path.display());
        app.full_update = Flow::Restore(Box::new(resume));
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

/// A bare waiting file from before the consent was recorded. It is withdrawn.
const STALE_IDLE_MARK: &[u8] = b"waiting\n";
/// How often the idle watcher asks the daemon whether any session is left.
/// The request can sit for days, so this is not once a second.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(15);

/// The release and official plug-ins the user agreed would apply at idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IdleConsent {
    /// The release to install. None means restart without installing a release.
    release: Option<String>,
    /// Sorted `id`, previous version, and next version for each pending plug-in.
    plugins: Vec<String>,
}

/// Where a restart-when-idle choice is remembered, next to this server's spaces.
fn idle_restart_file() -> PathBuf {
    prismattyc_mux::spaces_dir().join("restart-when-idle")
}

fn idle_consent_for(plan: &Plan) -> Result<IdleConsent> {
    Ok(IdleConsent {
        release: plan.update_available.then(|| plan.available.clone()),
        plugins: pending_plugins(&find_mux_bin())?,
    })
}

fn pending_plugins(pmux: &Path) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(&run(pmux, &["plugin", "pending"])?)
        .context("the pending official plug-ins were not readable")?;
    if let Some(reason) = value["unchecked"].as_str() {
        anyhow::bail!("the pending official plug-ins could not be checked: {reason}");
    }
    anyhow::ensure!(
        value["result"] == "restart_pending",
        "the pending official plug-ins were not listed"
    );
    let mut plugins = Vec::new();
    for plugin in value["plugins"]
        .as_array()
        .context("the pending official plug-ins were not listed")?
    {
        let id = plugin["id"]
            .as_str()
            .context("a pending plug-in omitted its id")?;
        let from = plugin["from"].as_str().unwrap_or("");
        let to = plugin["to"]
            .as_str()
            .context("a pending plug-in omitted its version")?;
        anyhow::ensure!(
            !id.contains(['\t', '\n', '\r'])
                && !from.contains(['\t', '\n', '\r'])
                && !to.contains(['\t', '\n', '\r']),
            "a pending plug-in id or version is not a single line"
        );
        plugins.push(format!("{id}\t{from}\t{to}"));
    }
    plugins.sort();
    plugins.dedup();
    Ok(plugins)
}

fn encode_idle_consent(consent: &IdleConsent) -> String {
    let mut text = format!("v1\nrelease={}\n", consent.release.as_deref().unwrap_or(""));
    for plugin in &consent.plugins {
        text.push_str("plugin=");
        text.push_str(plugin);
        text.push('\n');
    }
    text
}

fn decode_idle_consent(bytes: &[u8]) -> Option<IdleConsent> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "v1" {
        return None;
    }
    let release = lines.next()?.strip_prefix("release=")?;
    let release = if release.is_empty() {
        None
    } else {
        Some(release.to_string())
    };
    let mut plugins = Vec::new();
    for line in lines {
        let record = line.strip_prefix("plugin=")?;
        if record.split('\t').count() != 3 {
            return None;
        }
        plugins.push(record.to_string());
    }
    Some(IdleConsent { release, plugins })
}

/// Remember the release and plug-ins the user agreed to, until no session is running.
pub(super) fn remember_idle_restart(path: &Path, consent: &IdleConsent) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        prismattyc_mux::private_fs::create_dir(parent)?;
    }
    prismattyc_mux::private_fs::write(path, encode_idle_consent(consent).as_bytes())
}

/// Whether an idle restart may continue. A missing record, a failed fresh
/// check, or any difference withdraws it.
pub(super) fn idle_restart_matches(
    saved: Option<&IdleConsent>,
    fresh: Option<&IdleConsent>,
) -> bool {
    matches!((saved, fresh), (Some(saved), Some(fresh)) if saved == fresh)
}

/// The consent stored at `path`, if it is one this build understands.
pub(super) fn idle_restart_consent(path: &Path) -> Option<IdleConsent> {
    let mut file =
        match prismattyc_mux::private_fs::open(path, std::fs::OpenOptions::new().read(true)) {
            Ok(file) => file,
            Err(_) => return None,
        };
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return None;
    }
    if buf == STALE_IDLE_MARK {
        clear_idle_restart(path);
        return None;
    }
    decode_idle_consent(&buf)
}

/// Whether `path` is a restart-when-idle request. Anything else is ignored.
pub(super) fn idle_restart_waiting(path: &Path) -> bool {
    idle_restart_consent(path).is_some()
}

pub(super) fn clear_idle_restart(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("prismattyc-host: could not clear the idle restart: {error}")
        }
    }
}

/// Withdraw a pending idle restart, including one left by an older build.
pub(super) fn withdraw_idle_restart() {
    clear_idle_restart(&idle_restart_file());
}

/// A waiting restart runs only after the last session has ended.
/// A session that starts later still waits.
pub(super) fn idle_restart_ready(waiting: bool, running_sessions: usize) -> bool {
    waiting && running_sessions == 0
}

/// Start the watcher when a request is waiting. The thread leaves the UI
/// thread alone and sends one event when a snapshot has no sessions.
pub(super) fn ensure_idle_watch(app: &App) {
    if !idle_restart_waiting(&idle_restart_file()) {
        return;
    }
    if app.idle_watch.swap(true, Ordering::Relaxed) {
        return;
    }
    let proxy = app.event_proxy.clone();
    let flag = Arc::clone(&app.idle_watch);
    let _ = std::thread::Builder::new()
        .name("idle-restart".into())
        .spawn(move || loop {
            std::thread::sleep(IDLE_POLL);
            if !idle_restart_waiting(&idle_restart_file()) {
                flag.store(false, Ordering::Relaxed);
                return;
            }
            let Some(running) =
                crate::attach_log::live_snapshot().map(|snapshot| snapshot.sessions.len())
            else {
                continue;
            };
            if idle_restart_ready(idle_restart_waiting(&idle_restart_file()), running) {
                flag.store(false, Ordering::Relaxed);
                let _ = proxy.send_event(UserAction::IdleRestartReady);
                return;
            }
        });
}

/// The watcher saw no sessions. Prepare again, and restart only if that is still true.
pub(super) fn begin_idle_restart(app: &mut App) {
    if !idle_restart_waiting(&idle_restart_file()) {
        return;
    }
    if app.idle_restart_commit || matches!(app.full_update, Flow::Busy | Flow::Installing(..)) {
        return;
    }
    app.update_confirm = ConfirmStyle::OneConfirm;
    app.idle_restart_commit = true;
    start_prepare(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    /// Tests that run commands through [`run`] hold this, so the check that
    /// a failed command leaves no output directory sees only its own.
    fn run_lock() -> std::sync::MutexGuard<'static, ()> {
        static RUN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        RUN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(unix)]
    fn fake_pmux(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "update-flow-{name}-{}-{}",
            std::process::id(),
            prismattyc_mux::host_render_status::unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pmux");
        crate::test_support::write_script(&path, &format!("#!/bin/sh\n{body}\n"), "755");
        path
    }

    #[test]
    fn an_idle_restart_waits_while_a_session_remains_and_runs_when_none_do() {
        let dir = std::env::temp_dir().join(format!(
            "idle-restart-{}-{}",
            std::process::id(),
            prismattyc_mux::host_render_status::unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("restart-when-idle");
        assert!(!idle_restart_waiting(&file));
        assert!(
            !idle_restart_ready(false, 0),
            "nothing was requested, so an empty server does not restart"
        );
        let consent = IdleConsent {
            release: Some("0.3.78".into()),
            plugins: vec!["monitor\t1.0.0\t1.1.0".into()],
        };
        remember_idle_restart(&file, &consent).expect("remember");
        assert_eq!(idle_restart_consent(&file), Some(consent));
        assert!(idle_restart_waiting(&file));
        assert!(
            !idle_restart_ready(true, 2),
            "sessions that are still running must keep the pmux server up"
        );
        assert!(idle_restart_ready(true, 0));
        std::fs::write(&file, b"no\n").unwrap();
        assert!(
            !idle_restart_waiting(&file),
            "a different file is not a request"
        );
        let again = IdleConsent {
            release: None,
            plugins: Vec::new(),
        };
        remember_idle_restart(&file, &again).unwrap();
        clear_idle_restart(&file);
        assert!(!idle_restart_waiting(&file));
        assert!(!file.exists());
        std::fs::write(&file, STALE_IDLE_MARK).unwrap();
        assert!(
            idle_restart_consent(&file).is_none(),
            "a bare waiting file is not consent"
        );
        assert!(!file.exists(), "a bare waiting file is withdrawn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_idle_restart_runs_only_for_the_release_and_plug_ins_that_were_confirmed() {
        let consent = IdleConsent {
            release: Some("0.3.78".into()),
            plugins: vec!["monitor\t1.0.0\t1.1.0".into()],
        };
        let same = decode_idle_consent(encode_idle_consent(&consent).as_bytes()).unwrap();
        assert_eq!(same, consent);
        let newer = IdleConsent {
            release: Some("0.3.79".into()),
            plugins: consent.plugins.clone(),
        };
        assert_ne!(newer, consent, "a later release was not confirmed");
        let extra = IdleConsent {
            release: consent.release.clone(),
            plugins: vec!["monitor\t1.0.0\t1.1.0".into(), "other\t\t1.0.0".into()],
        };
        assert_ne!(extra, consent, "a new plug-in was not confirmed");
        let none = IdleConsent {
            release: None,
            plugins: Vec::new(),
        };
        assert_ne!(
            IdleConsent {
                release: Some("0.3.78".into()),
                plugins: Vec::new(),
            },
            none,
            "a release that appeared later was not confirmed"
        );
        assert!(decode_idle_consent(b"waiting\n").is_none());
        assert!(decode_idle_consent(b"v1\nrelease=0.3.78\nplugin=broken\n").is_none());
        assert!(
            idle_restart_matches(Some(&consent), Some(&consent)),
            "the same release and plug-ins may restart"
        );
        assert!(
            !idle_restart_matches(Some(&consent), Some(&newer)),
            "a later release must not restart"
        );
        assert!(
            !idle_restart_matches(Some(&consent), Some(&extra)),
            "a new plug-in must not restart"
        );
        assert!(
            !idle_restart_matches(Some(&none), Some(&consent)),
            "a release that was not confirmed must not restart"
        );
        assert!(
            !idle_restart_matches(None, Some(&consent)),
            "a missing record must not restart"
        );
        assert!(
            !idle_restart_matches(Some(&consent), None),
            "a failed fresh check must not restart"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_left_holding_the_output_does_not_block_the_result() {
        let _run = run_lock();
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
        let _ = prismattyc_process::status(std::process::Command::new("kill").arg(pid.trim()));
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
        let _run = run_lock();
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
    fn a_restart_report_names_the_sessions_the_daemon_brought_back() {
        let report = r#"{"components":[{"component":"daemon","status":"restarted"},{"component":"sessions","lost":["gone"],"restored":["work","notes"],"status":"partial"},{"component":"plugins","status":"reconciled"}],"status":"restart_results"}"#;
        let restored = Restored::from_report(report).expect("sessions entry");
        assert_eq!(restored.restored, ["work", "notes"]);
        assert_eq!(restored.lost, ["gone"]);
        let text = restored.summary();
        assert!(text.starts_with("Restored 2 sessions"), "{text}");
        assert!(text.contains("work, notes"), "{text}");
        assert!(text.contains("Not restored: gone."), "{text}");

        let without = r#"{"components":[{"component":"daemon","status":"restarted"}],"status":"restart_results"}"#;
        assert_eq!(
            Restored::from_report(without),
            None,
            "a CLI that did not restore"
        );
        assert_eq!(Restored::from_report("not json"), None);
    }

    #[test]
    fn an_unconfirmed_restore_keeps_the_saved_configuration_question() {
        // The restart could not read the new daemon's sessions: nothing is
        // known to be back, so the app must still offer the saved file.
        let unconfirmed = r#"{"components":[{"component":"sessions","status":"unconfirmed","saved":["work"],"detail":"connect failed"}],"status":"restart_results"}"#;
        assert_eq!(Restored::from_report(unconfirmed), None);
        let without_lists = r#"{"components":[{"component":"sessions","status":"restored"}],"status":"restart_results"}"#;
        assert_eq!(Restored::from_report(without_lists), None);
        let unknown = r#"{"components":[{"component":"sessions","status":"later","restored":[],"lost":[]}],"status":"restart_results"}"#;
        assert_eq!(Restored::from_report(unknown), None);
    }

    #[test]
    fn an_older_app_relaunch_still_asks_before_restoring() {
        // The relaunch payload written by an app from before automatic
        // restore has no `restored` field; the new app must then ask.
        let older: Resume =
            serde_json::from_str(r#"{"notice":"done","backup":"/tmp/saved.json"}"#).unwrap();
        assert!(older.restored.is_none());
        assert!(older.backup.is_some());
        assert!(older.progress.is_none(), "no meter to finish");
        let newer = Resume {
            notice: "done".into(),
            backup: None,
            restored: Some(Restored {
                restored: vec!["work".into()],
                lost: Vec::new(),
            }),
            views: Vec::new(),
            progress: None,
        };
        let round: Resume = serde_json::from_str(&serde_json::to_string(&newer).unwrap()).unwrap();
        assert_eq!(round.restored, newer.restored);
    }

    #[test]
    fn the_relaunched_app_finishes_the_meter_the_update_carried() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Install {
            item: "core binaries".into(),
        });
        meter.apply(&restart_progress("Relaunching the app", 7, 5));
        let resume = Resume {
            notice: "done".into(),
            backup: None,
            restored: None,
            views: Vec::new(),
            progress: Some(Carried {
                installed: "0.3.71".into(),
                available: Some("0.3.72".into()),
                meter: Some(meter),
                open_windows: restart_progress("Opening the windows", 8, 5),
            }),
        };
        // The meter crosses the relaunch in the environment, as text.
        let relaunched: Resume =
            serde_json::from_str(&serde_json::to_string(&resume).unwrap()).unwrap();
        let carried = relaunched.progress.unwrap();
        let finished = carried.finish().unwrap();
        assert_eq!(
            finished.lines(),
            [
                "Finished step 4 of 4: Restart",
                "Opening the windows (8 of 8)"
            ]
        );
        assert_eq!(finished.bar().1, "100%");
        assert_eq!(
            finished.receipt(),
            [
                "Download: done",
                "Verify: done",
                "Install: done",
                "Restart: done"
            ]
        );
        let without_meter = Carried {
            meter: None,
            ..carried
        };
        assert!(without_meter.finish().is_none(), "nothing to finish");
    }

    // The launcher-step tests that use it run on Linux and macOS only.
    #[cfg(unix)]
    fn update_plan(installed: &str, available: &str) -> Plan {
        Plan {
            installed: installed.into(),
            available: available.into(),
            update_available: true,
            sessions: Vec::new(),
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_update_includes_the_monitor_launcher_checked_against_the_previous_release() {
        let pmux = fake_pmux(
            "launcher",
            r#"echo "$@" > "$(dirname "$0")/args"
echo '{"status":"repaired","binary":"pmux-activity-monitor","version":"0.3.73","warning":null}'"#,
        );
        let steps = std::cell::RefCell::new(Vec::new());
        let line = monitor_launcher(&pmux, &update_plan("0.3.72", "0.3.73"), &|event| {
            steps.borrow_mut().push(format!("{event:?}"));
        });
        assert_eq!(line, "Monitor launcher: installed 0.3.73\n");
        let args = std::fs::read_to_string(pmux.with_file_name("args")).unwrap();
        assert_eq!(
            args.trim(),
            "update --repair-monitor-launcher --json --from-monitor-version 0.3.72"
        );
        assert_eq!(
            *steps.borrow(),
            ["Doing(\"Updating the monitor launcher\")"]
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_launcher_that_cannot_be_replaced_is_kept_and_named_without_stopping_the_update() {
        let pmux = fake_pmux(
            "launcher-kept",
            "echo 'Error: existing monitor does not match the immutable official source asset' >&2\nexit 1",
        );
        let line = monitor_launcher(&pmux, &update_plan("0.3.72", "0.3.73"), &|_| {});
        let lines: Vec<&str> = line.lines().collect();
        assert_eq!(
            lines[0],
            "Monitor launcher: kept the existing launcher: existing monitor does not match the immutable official source asset"
        );
        assert!(
            lines[1].contains("pmux update --repair-monitor-launcher --from-monitor-version"),
            "{line}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_app_carries_its_own_monitor_launcher() {
        let pmux = fake_pmux("launcher-macos", r#"echo ran > "$(dirname "$0")/args""#);
        let line = monitor_launcher(&pmux, &update_plan("0.3.72", "0.3.73"), &|_| {});
        assert_eq!(line, "");
        assert!(!pmux.with_file_name("args").exists());
    }

    #[test]
    fn a_restart_without_a_new_release_expects_the_installed_version() {
        let plan = |installed: &str, available: &str, update_available| Plan {
            installed: installed.into(),
            available: available.into(),
            update_available,
            sessions: Vec::new(),
        };
        // A build newer than the newest release restarts on itself, not on
        // the older release it would never install.
        assert_eq!(plan("0.3.72", "0.3.71", false).target(), "0.3.72");
        assert_eq!(plan("0.3.71", "0.3.71", false).target(), "0.3.71");
        assert_eq!(plan("0.3.70", "0.3.71", true).target(), "0.3.71");
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
        let _run = run_lock();
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
    #[cfg(unix)]
    fn restarts(seen: Vec<Event>) -> Vec<(String, usize, usize)> {
        seen.into_iter()
            .map(|event| match event {
                Event::Progress(Progress::Restart { item, index, count }) => (item, index, count),
                other => panic!("not a restart step: {other:?}"),
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn the_daemon_restart_steps_count_the_apps_own_steps() {
        let step = |item: &str, index| {
            progress::line(&Progress::Restart {
                item: item.into(),
                index,
                count: 5,
            })
        };
        let pmux = fake_pmux(
            "restart-steps",
            &format!(
                "echo '{}' >&2\necho 'a plain message' >&2\necho '{}' >&2\necho '{{\"status\":\"restart_results\"}}'",
                step("Saving the sessions", 1),
                step("Stopping the old daemon", 2)
            ),
        );
        let seen = std::cell::RefCell::new(Vec::new());
        let report = |event| seen.borrow_mut().push(event);
        let (output, steps) = restart_daemon(&pmux, &["restart", "--json"], &report).unwrap();
        assert_eq!(output, r#"{"status":"restart_results"}"#);
        assert_eq!(steps, 5);
        assert_eq!(
            restarts(seen.into_inner()),
            [
                ("Saving the sessions".into(), 1, 8),
                ("Stopping the old daemon".into(), 2, 8)
            ],
            "the app's MCP adapter, relaunch and window steps follow the daemon's five"
        );
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_older_cli_restarts_the_daemon_as_one_step() {
        let _run = run_lock();
        // The restart command's wording for an option it does not know.
        let pmux = fake_pmux(
            "restart-older",
            r#"for arg in "$@"; do
  if [ "$arg" = --progress ]; then
    echo 'Error: unknown restart option "--progress"; use pmux restart --help' >&2
    exit 1
  fi
done
echo '{"status":"restart_results"}'"#,
        );
        let seen = std::cell::RefCell::new(Vec::new());
        let report = |event| seen.borrow_mut().push(event);
        let (output, steps) = restart_daemon(&pmux, &["restart", "--json"], &report).unwrap();
        assert_eq!(output, r#"{"status":"restart_results"}"#);
        assert_eq!(steps, 1);
        assert_eq!(
            restarts(seen.into_inner()),
            [("Restarting the daemon and sessions".into(), 1, 4)]
        );
        std::fs::remove_dir_all(pmux.parent().unwrap()).unwrap();
    }
}
