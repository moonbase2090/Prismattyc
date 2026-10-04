//! Background release checks and install commands for the macOS app menu.

use std::process::{Command, Output, Stdio};
use std::thread;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use winit::event_loop::EventLoopProxy;

use crate::{find_mux_bin, UserAction};

#[derive(Debug, Deserialize)]
pub(super) struct ReleaseCheck {
    pub available: String,
    pub update_available: bool,
    #[serde(default)]
    pub release_notes: String,
}

#[derive(Debug)]
pub(super) struct InstallResult {
    pub notice: String,
}

#[derive(Deserialize)]
struct InstallReport {
    status: String,
    version: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct RestartReport {
    components: Vec<RestartComponent>,
}

#[derive(Deserialize)]
struct RestartComponent {
    component: String,
    status: String,
}

pub(super) fn start_check(proxy: EventLoopProxy<UserAction>) {
    thread::spawn(move || {
        let result = check_release().map_err(|error| format!("{error:#}"));
        let _ = proxy.send_event(UserAction::UpdateCheckFinished { result });
    });
}

pub(super) fn start_install(proxy: EventLoopProxy<UserAction>, rollback: bool) {
    thread::spawn(move || {
        let result = install_release(rollback).map_err(|error| format!("{error:#}"));
        let _ = proxy.send_event(UserAction::UpdateInstallFinished { rollback, result });
    });
}

fn check_release() -> Result<ReleaseCheck> {
    let output = run_pmux(&["update", "--check", "--json"])?;
    ensure!(output.status.success(), "{}", command_error(&output));
    serde_json::from_slice(&output.stdout).context("parse pmux update check")
}

fn install_release(rollback: bool) -> Result<InstallResult> {
    let pmux = find_mux_bin();
    let output = run(
        &pmux,
        if rollback {
            &["update", "--rollback", "--json"]
        } else {
            &["update", "--json"]
        },
    )?;
    ensure!(output.status.success(), "{}", command_error(&output));
    let report: InstallReport =
        serde_json::from_slice(&output.stdout).context("parse pmux update result")?;
    ensure!(
        matches!(report.status.as_str(), "installed" | "rolled_back"),
        "pmux update returned unexpected status {:?}",
        report.status
    );
    let daemon = restart_daemon(&pmux);
    let operation = if rollback { "rolled back" } else { "updated" };
    let version = report.version.as_deref().unwrap_or("the installed version");
    let message = report.message.unwrap_or_default();
    let message = if message.is_empty() {
        String::new()
    } else {
        format!(" {message}")
    };
    Ok(InstallResult {
        notice: format!("Prismattyc {operation} to {version}.{message} {daemon}"),
    })
}

fn restart_daemon(pmux: &std::path::Path) -> String {
    let Ok(output) = run(pmux, &["restart", "--daemon", "--json"]) else {
        return ensure_daemon(pmux);
    };
    if output.status.success() {
        if let Ok(report) = serde_json::from_slice::<RestartReport>(&output.stdout) {
            if let Some(daemon) = report
                .components
                .iter()
                .find(|component| component.component == "daemon")
            {
                return match daemon.status.as_str() {
                    "restarted" => "The pmux daemon restarted on the new version.".into(),
                    "deferred" => {
                        "Active pmux sessions are still running, so their daemon was left alone."
                            .into()
                    }
                    _ => format!("The pmux daemon reported status {}.", daemon.status),
                };
            }
        }
        return "The pmux daemon restart finished.".into();
    }
    ensure_daemon(pmux)
}

fn ensure_daemon(pmux: &std::path::Path) -> String {
    let Ok(output) = run(pmux, &["up"]) else {
        return "The pmux daemon will start when the next session opens.".into();
    };
    if output.status.success() {
        "The pmux daemon is running.".into()
    } else {
        "The pmux daemon could not be started; pmux sessions were left untouched.".into()
    }
}

fn run_pmux(args: &[&str]) -> Result<Output> {
    run(&find_mux_bin(), args)
}

fn run(pmux: &std::path::Path, args: &[&str]) -> Result<Output> {
    Command::new(pmux)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} {}", pmux.display(), args.join(" ")))
}

fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}
