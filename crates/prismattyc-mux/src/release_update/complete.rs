//! The default `pmux update`: install the release, include the matching
//! official monitor launcher, restart what can restart safely, and print one
//! receipt.
//!
//! Each step after the install runs through the newly installed `pmux`, so the
//! new release's updater repairs the launcher and the new release restarts the
//! components. The install itself runs as `update --json` on this executable,
//! which keeps the install-only contract that apps call.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{bail, ensure, Context, Result};
use semver::Version;
use serde::Serialize;
use serde_json::Value;

/// What a complete update does, from `pmux update` flags.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// Restart components after installing (`--no-restart` clears it).
    restart: bool,
    json: bool,
    progress: bool,
    /// `--pre`, `--bin-dir PATH` and `--progress`, passed to the install.
    install_args: Vec<String>,
    bin_dir: Option<String>,
    /// `--instance NAME` / `--socket PATH`, passed to `pmux versions` and
    /// `pmux restart`.
    target: Vec<String>,
}

/// Flags that select another operation; a complete update never runs then.
const OTHER_OPERATIONS: [&str; 4] = [
    "--check",
    "--rollback",
    "--repair-monitor-launcher",
    "--from-monitor-version",
];

/// Split the complete-update flags out of `args`. Returns the plan when this
/// run is a complete update, and the arguments left for the install-only path
/// otherwise. `--json` alone keeps the install-only contract; `--restart` or
/// `--no-restart` select the complete update with a JSON receipt.
pub(super) fn select(args: &[String]) -> Result<(Option<Plan>, Vec<String>)> {
    let mut plan = Plan {
        restart: true,
        ..Plan::default()
    };
    let (mut restart, mut no_restart, mut other) = (false, false, false);
    let mut rest = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--restart" => restart = true,
            "--no-restart" => no_restart = true,
            "--instance" | "--socket" => {
                let value = args
                    .next()
                    .with_context(|| format!("{arg} needs a value"))?;
                plan.target.extend([arg.clone(), value.clone()]);
            }
            _ => {
                other |= OTHER_OPERATIONS.contains(&arg.as_str());
                plan.json |= arg == "--json";
                if arg == "--progress" {
                    plan.progress = true;
                    plan.install_args.push(arg.clone());
                }
                if arg == "--pre" {
                    plan.install_args.push(arg.clone());
                }
                if arg == "--bin-dir" {
                    let value = args.next().context("--bin-dir needs a directory")?;
                    plan.install_args.extend([arg.clone(), value.clone()]);
                    plan.bin_dir = Some(value.clone());
                    rest.extend([arg.clone(), value.clone()]);
                    continue;
                }
                rest.push(arg.clone());
            }
        }
    }
    ensure!(
        !(restart && no_restart),
        "--restart and --no-restart cannot be combined"
    );
    if other {
        ensure!(
            !(restart || no_restart),
            "--restart and --no-restart apply only to installing a release"
        );
        return Ok((None, rest));
    }
    if plan.json && !restart && !no_restart {
        return Ok((None, rest));
    }
    plan.restart = !no_restart;
    Ok((Some(plan), rest))
}

/// The core install's outcome, read from `update --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Core {
    /// `installed` or `current`.
    status: &'static str,
    version: String,
}

/// Read the install's JSON report (the last line on stdout).
pub(super) fn core_from(stdout: &str) -> Result<Core> {
    let report = last_json(stdout).context("the install did not report a result")?;
    match report["status"].as_str() {
        Some("installed") => Ok(Core {
            status: "installed",
            version: text(&report["version"]).context("install report has no version")?,
        }),
        Some("checked") if report["update_available"] == Value::Bool(false) => Ok(Core {
            status: "current",
            version: text(&report["installed"]).context("check report has no version")?,
        }),
        _ => bail!("unexpected install report: {report}"),
    }
}

/// The monitor launcher step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Launcher {
    /// `current`, `repaired`, `committed_with_warning`, `in_app` (macOS) or
    /// `failed`.
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Launcher {
    /// Whether the step succeeded; a kept launcher is reported, not hidden.
    pub fn ok(&self) -> bool {
        self.status != "failed"
    }

    /// One line for the result: what happened to the launcher of `version`.
    pub fn summary(&self, version: &str) -> String {
        let line = match self.status.as_str() {
            "current" => format!("{version} already installed"),
            "repaired" => format!("installed {version}"),
            "committed_with_warning" => format!("installed {version} with a warning"),
            "in_app" => "included in Prismattyc.app".into(),
            "failed" => "kept the existing launcher".into(),
            other => other.into(),
        };
        match &self.detail {
            Some(detail) => format!("{line}: {detail}"),
            None => line,
        }
    }

    /// What to do about a kept launcher, for `version`'s core.
    pub fn next(&self, version: &str) -> Option<String> {
        (self.status == "failed").then(|| format!("Monitor launcher: preserve the existing pmux-activity-monitor, then run pmux update --repair-monitor-launcher --from-monitor-version VERSION with the official version it came from (installed core {version})."))
    }
}

/// Read a `--repair-monitor-launcher --json` run.
pub(super) fn launcher_from(success: bool, stdout: &str, stderr: &str) -> Launcher {
    let report = last_json(stdout);
    match report {
        Some(report) if success => Launcher {
            status: text(&report["status"]).unwrap_or_else(|| "unknown".into()),
            detail: text(&report["warning"]),
        },
        _ => Launcher {
            status: "failed".into(),
            detail: Some(last_error(stderr)),
        },
    }
}

/// One restarted, deferred or reported component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Component {
    component: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// What the user does next, for a deferred or unanswered component.
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

/// The restart step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Restart {
    /// Every running component already runs the installed release.
    NotNeeded,
    /// `--no-restart`: the components keep their current version.
    Skipped {
        stale: Vec<String>,
    },
    Ran {
        components: Vec<Component>,
    },
    Failed {
        detail: String,
    },
}

/// Read `pmux restart --json` into components, each with its next action.
pub(super) fn restart_from(stdout: &str) -> Result<Vec<Component>> {
    let report = last_json(stdout).context("the restart did not report a result")?;
    ensure!(
        report["status"] == "restart_results",
        "unexpected restart report: {report}"
    );
    let components = report["components"]
        .as_array()
        .context("restart report has no components")?;
    Ok(components.iter().map(component_from).collect())
}

fn component_from(value: &Value) -> Component {
    let name = text(&value["component"]).unwrap_or_else(|| "component".into());
    // Cooperative components answer with a response; the others carry a status.
    let response = &value["response"];
    let source = if response.is_object() {
        response
    } else {
        value
    };
    let status = text(&source["status"]).unwrap_or_else(|| "unknown".into());
    let detail = text(&source["detail"])
        .filter(|detail| !detail.is_empty())
        .or_else(|| {
            value["sessions"]
                .as_array()
                .map(|sessions| format!("{} sessions are running", sessions.len()))
        });
    let next = next_action(&name, &status);
    Component {
        pid: source["pid"].as_u64(),
        detail,
        next,
        status,
        component: name,
    }
}

/// The next action for a component that did not restart.
fn next_action(component: &str, status: &str) -> Option<String> {
    let next = match (component, status) {
        ("daemon", "deferred") => "Sessions keep running on the old daemon. To move them to the new release, choose Update and restart in the app (it saves and restores them), or run pmux restart --daemon --stop-sessions to end them.",
        ("plugins", "pending") => "They move to the new release when the daemon restarts.",
        ("plugins", "unconfirmed") => "Run pmux plugin list to check the official plug-ins.",
        ("host", "deferred") => "Fix what the window names, then run pmux restart --host, or close and reopen that window.",
        ("host", "idle") => "Click or type in that window to wake it, then run pmux restart --host.",
        (_, "pending") => "It did not answer within 15 seconds. Run pmux versions before restarting it again.",
        _ => return None,
    };
    Some(next.into())
}

/// Add each open window in `idle` that the restart report does not name.
/// A restart from an older release skips such windows and may report no
/// window at all; the receipt names them with what to do instead.
pub(super) fn with_idle_hosts(mut components: Vec<Component>, idle: &[u32]) -> Vec<Component> {
    let named = |pid: u32| {
        components
            .iter()
            .any(|c| c.component == "host" && c.pid == Some(u64::from(pid)))
    };
    let missing: Vec<u32> = idle.iter().copied().filter(|pid| !named(*pid)).collect();
    if missing.is_empty() {
        return components;
    }
    components.retain(|c| !(c.component == "host" && c.status == "not_registered"));
    for pid in missing {
        components.push(Component {
            component: "host".into(),
            status: "idle".into(),
            pid: Some(u64::from(pid)),
            detail: Some("open but idle, so it was not asked to restart".into()),
            next: next_action("host", "idle"),
        });
    }
    components
}

/// The daemon socket the restart targets: `--socket` from the plan, or the
/// default instance's.
fn plan_socket(plan: &Plan) -> PathBuf {
    plan.target
        .windows(2)
        .find(|pair| pair[0] == "--socket")
        .map(|pair| PathBuf::from(&pair[1]))
        .or_else(|| {
            let instance = plan
                .target
                .windows(2)
                .find(|pair| pair[0] == "--instance")
                .map_or("default", |pair| pair[1].as_str());
            crate::default_socket_path(instance).ok()
        })
        .unwrap_or_default()
}

/// Components whose running version differs from `installed`, from
/// `pmux versions` JSON.
pub(super) fn stale_components(versions: &Value, installed: &str) -> Vec<String> {
    let Ok(installed) = Version::parse(installed) else {
        return Vec::new();
    };
    let differs = |label: &Value| {
        label
            .as_str()
            .and_then(semver_in)
            .is_some_and(|running| running != installed)
    };
    let mut stale = Vec::new();
    for entry in versions["running"].as_array().into_iter().flatten() {
        if differs(&entry["version"]) {
            let component = text(&entry["component"]).unwrap_or_else(|| "component".into());
            let pid = entry["pid"].as_u64().unwrap_or_default();
            stale.push(format!("{component} {pid}"));
        }
    }
    if differs(&versions["daemon"]["version"]) {
        stale.push("daemon".into());
    }
    stale
}

/// The single receipt a complete update prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Receipt {
    /// `updated`, `current`, `updated_with_deferrals` or `incomplete`.
    status: &'static str,
    previous: String,
    version: String,
    core: Core,
    monitor_launcher: Launcher,
    restart: Restart,
}

impl Receipt {
    pub(super) fn new(previous: String, core: Core, launcher: Launcher, restart: Restart) -> Self {
        let failed = !launcher.ok() || matches!(restart, Restart::Failed { .. });
        let deferred = match &restart {
            Restart::Ran { components } => components.iter().any(|c| c.next.is_some()),
            Restart::Skipped { stale } => !stale.is_empty(),
            _ => false,
        };
        let status = if failed {
            "incomplete"
        } else if deferred {
            "updated_with_deferrals"
        } else if core.status == "current" && launcher.status == "current" {
            "current"
        } else {
            "updated"
        };
        Self {
            status,
            previous,
            version: core.version.clone(),
            core,
            monitor_launcher: launcher,
            restart,
        }
    }

    fn ok(&self) -> bool {
        self.status != "incomplete"
    }

    /// The receipt as text: one heading, one line per step, then what to do
    /// next.
    pub(super) fn text(&self) -> String {
        let heading = match self.status {
            "current" => format!("Prismattyc {} is up to date.", self.version),
            "incomplete" => format!(
                "Update to {} is incomplete (from {}).",
                self.version, self.previous
            ),
            _ if self.core.status == "installed" => {
                format!("Updated Prismattyc {} to {}.", self.previous, self.version)
            }
            _ => format!("Prismattyc {} is installed.", self.version),
        };
        let mut lines = vec![heading];
        let core = match self.core.status {
            "installed" => format!("installed {}", self.version),
            _ => format!("{} already installed", self.version),
        };
        lines.push(format!("  Core: {core}"));
        lines.push(format!(
            "  Monitor launcher: {}",
            self.monitor_launcher.summary(&self.version)
        ));
        let mut next = Vec::new();
        match &self.restart {
            Restart::NotNeeded => lines
                .push("  Restart: not needed; every running component runs this release".into()),
            Restart::Skipped { stale } if stale.is_empty() => {
                lines.push("  Restart: skipped (--no-restart)".into())
            }
            Restart::Skipped { stale } => {
                lines.push(format!(
                    "  Restart: skipped (--no-restart); still on the old release: {}",
                    stale.join(", ")
                ));
                next.push("Run pmux restart to apply the release to them.".to_string());
            }
            Restart::Failed { detail } => {
                lines.push(format!("  Restart: failed: {detail}"));
                next.push("Run pmux restart --plan, then pmux restart.".into());
            }
            Restart::Ran { components } => {
                for component in components {
                    lines.push(format!("  {}", component_line(component)));
                    next.extend(
                        component
                            .next
                            .as_ref()
                            .map(|n| format!("{}: {n}", label(&component.component))),
                    );
                }
            }
        }
        next.extend(self.monitor_launcher.next(&self.version));
        if !next.is_empty() {
            lines.push("Next:".into());
            lines.extend(next.into_iter().map(|n| format!("  - {n}")));
        }
        lines.join("\n")
    }
}

fn label(component: &str) -> &str {
    match component {
        "host" => "App window",
        "mcp" => "MCP adapter",
        "daemon" => "Daemon",
        "plugins" => "Official plug-ins",
        other => other,
    }
}

fn component_line(component: &Component) -> String {
    let mut line = label(&component.component).to_string();
    if let Some(pid) = component.pid {
        line.push_str(&format!(" {pid}"));
    }
    let status = match (component.component.as_str(), component.status.as_str()) {
        (_, "not_registered") => "none running".to_string(),
        ("plugins", "reconciled") => "moved to the new release".to_string(),
        (_, status) => status.replace('_', " "),
    };
    line.push_str(&format!(": {status}"));
    if let Some(detail) = component
        .detail
        .as_ref()
        .filter(|_| component.status != "not_registered")
    {
        line.push_str(&format!(" ({detail})"));
    }
    line
}

/// The last line of `stdout` that parses as a JSON object.
fn last_json(stdout: &str) -> Option<Value> {
    stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(Value::is_object)
}

/// The last non-empty line of a child's stderr, without anyhow's prefix.
fn last_error(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with(super::progress::PREFIX.trim_end()))
        .map(|line| line.trim_start_matches("Error: ").to_string())
        .unwrap_or_else(|| "the launcher repair failed without a message".into())
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(ToString::to_string)
}

/// The first canonical version inside a label such as `pmux 0.3.72 (abc)`.
fn semver_in(label: &str) -> Option<Version> {
    label
        .split_whitespace()
        .find_map(|word| Version::parse(word.trim_start_matches('v')).ok())
}

/// Run the complete update and print its receipt. Fails (after printing the
/// receipt) when a step failed.
pub(super) fn run(plan: &Plan) -> Result<()> {
    let root = super::root()?;
    let previous = super::installed_label(&root, cfg!(target_os = "macos"))?;
    let core = install(plan)?;
    let launcher = launcher(plan, &previous);
    let restart = restart(plan, &core);
    let receipt = Receipt::new(previous, core, launcher, restart);
    if plan.json {
        println!("{}", serde_json::to_string(&receipt)?);
    } else {
        println!("{}", receipt.text());
    }
    ensure!(
        receipt.ok(),
        "the update is incomplete; see the receipt above"
    );
    Ok(())
}

/// Install with this executable's install-only path. Its progress and errors
/// reach the user's stderr as they happen.
fn install(plan: &Plan) -> Result<Core> {
    let exe = std::env::current_exe().context("locate this executable")?;
    let mut command = Command::new(exe);
    command
        .args(["update", "--json"])
        .args(&plan.install_args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    let output = capture(&mut command)?;
    ensure!(
        output.status.success(),
        "the release was not installed; nothing was restarted"
    );
    core_from(&String::from_utf8_lossy(&output.stdout))
}

/// The newly installed `pmux`, or this executable when none is registered
/// (a development build).
fn installed_pmux() -> Result<PathBuf> {
    match super::installed_binary("pmux") {
        Some(path) => Ok(path),
        None => std::env::current_exe().context("locate this executable"),
    }
}

fn launcher(plan: &Plan, previous: &str) -> Launcher {
    if cfg!(target_os = "macos") {
        return Launcher {
            status: "in_app".into(),
            detail: None,
        };
    }
    match installed_pmux() {
        Ok(pmux) => repair_monitor_launcher(&pmux, previous, plan.bin_dir.as_deref()),
        Err(error) => launcher_from(false, "", &format!("{error:#}")),
    }
}

/// Install or confirm the official monitor launcher for the core that `pmux`
/// belongs to (Linux and Windows), through that `pmux`'s own updater.
/// `previous` is the core release installed before this update: the launcher
/// installed with it is that release's asset, and repair checks its bytes
/// against that asset before replacing it. A launcher with a repair receipt,
/// or no launcher, ignores the hint.
pub fn repair_monitor_launcher(pmux: &Path, previous: &str, bin_dir: Option<&str>) -> Launcher {
    let mut command = Command::new(pmux);
    command.args(["update", "--repair-monitor-launcher", "--json"]);
    if Version::parse(previous).is_ok_and(|v| v.to_string() == previous) {
        command.args(["--from-monitor-version", previous]);
    }
    if let Some(bin_dir) = bin_dir {
        command.args(["--bin-dir", bin_dir]);
    }
    command.stdin(Stdio::null()).stderr(Stdio::piped());
    match capture(&mut command) {
        Ok(output) => launcher_from(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ),
        Err(error) => launcher_from(false, "", &format!("{error:#}")),
    }
}

fn restart(plan: &Plan, core: &Core) -> Restart {
    let stale = if core.status == "installed" {
        None
    } else {
        Some(running_stale(plan, &core.version))
    };
    if !plan.restart {
        let stale = stale.unwrap_or_else(|| running_stale(plan, &core.version));
        return Restart::Skipped { stale };
    }
    if stale.as_ref().is_some_and(Vec::is_empty) {
        return Restart::NotNeeded;
    }
    match run_restart(plan) {
        Ok(components) => {
            let idle = crate::component_restart::idle_components(&plan_socket(plan), "host");
            Restart::Ran {
                components: with_idle_hosts(components, &idle),
            }
        }
        Err(error) => Restart::Failed {
            detail: format!("{error:#}"),
        },
    }
}

/// Running components on another release. A failed `pmux versions` counts
/// as stale so the restart runs and reports what it finds.
fn running_stale(plan: &Plan, installed: &str) -> Vec<String> {
    let versions = (|| -> Result<Value> {
        let mut command = Command::new(installed_pmux()?);
        command
            .args(&plan.target)
            .arg("versions")
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        let output = capture(&mut command)?;
        ensure!(output.status.success(), "pmux versions failed");
        last_json(&String::from_utf8_lossy(&output.stdout)).context("pmux versions printed no JSON")
    })();
    match versions {
        Ok(versions) => stale_components(&versions, installed),
        Err(_) => vec!["unknown".into()],
    }
}

fn run_restart(plan: &Plan) -> Result<Vec<Component>> {
    let mut command = Command::new(installed_pmux()?);
    command.args(&plan.target).args(["restart", "--json"]);
    if plan.progress {
        command.arg("--progress");
    }
    command.stdin(Stdio::null()).stderr(Stdio::inherit());
    let output = capture(&mut command)?;
    ensure!(output.status.success(), "pmux restart failed");
    restart_from(&String::from_utf8_lossy(&output.stdout))
}

fn capture(command: &mut Command) -> Result<Output> {
    command.stdout(Stdio::piped());
    prismattyc_process::spawn(command)
        .and_then(std::process::Child::wait_with_output)
        .with_context(|| format!("run {:?}", command.get_program()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn plan(list: &[&str]) -> Plan {
        select(&args(list)).unwrap().0.expect("a complete update")
    }

    #[test]
    fn plain_update_is_the_complete_update() {
        let plan = plan(&[]);
        assert!(plan.restart);
        assert!(!plan.json);
    }

    #[test]
    fn json_alone_keeps_the_install_only_contract() {
        let (complete, rest) = select(&args(&["--json", "--progress"])).unwrap();
        assert_eq!(complete, None);
        assert_eq!(rest, args(&["--json", "--progress"]));
        assert!(plan(&["--json", "--restart"]).json);
        let quiet = plan(&["--json", "--no-restart"]);
        assert!(quiet.json && !quiet.restart);
    }

    #[test]
    fn other_operations_never_run_the_complete_update() {
        for operation in [
            &["--check"][..],
            &["--rollback"],
            &["--repair-monitor-launcher"],
            &[
                "--repair-monitor-launcher",
                "--from-monitor-version",
                "0.3.1",
            ],
        ] {
            assert_eq!(select(&args(operation)).unwrap().0, None, "{operation:?}");
        }
        assert!(select(&args(&["--check", "--restart"])).is_err());
        assert!(select(&args(&["--rollback", "--no-restart"])).is_err());
        assert!(select(&args(&["--restart", "--no-restart"])).is_err());
    }

    #[test]
    fn install_flags_pass_through_and_the_target_reaches_the_restart() {
        let (complete, rest) = select(&args(&[
            "--socket",
            "/run/p.sock",
            "--pre",
            "--bin-dir",
            "/opt/bin",
            "--progress",
        ]))
        .unwrap();
        let plan = complete.unwrap();
        assert_eq!(
            plan.install_args,
            args(&["--pre", "--bin-dir", "/opt/bin", "--progress"])
        );
        assert_eq!(plan.bin_dir.as_deref(), Some("/opt/bin"));
        assert_eq!(plan.target, args(&["--socket", "/run/p.sock"]));
        assert!(plan.progress);
        assert!(!rest.contains(&"--socket".to_string()));
    }

    #[test]
    fn the_install_report_names_what_was_installed() {
        let installed = core_from(
            "Downloading\n{\"status\":\"installed\",\"version\":\"0.3.73\",\"restart_required\":true}\n",
        )
        .unwrap();
        assert_eq!(installed.status, "installed");
        assert_eq!(installed.version, "0.3.73");
        let current = core_from(
            "{\"status\":\"checked\",\"installed\":\"0.3.73\",\"available\":\"0.3.73\",\"update_available\":false}",
        )
        .unwrap();
        assert_eq!(
            (current.status, current.version.as_str()),
            ("current", "0.3.73")
        );
        assert!(core_from(
            "{\"status\":\"checked\",\"installed\":\"0.3.72\",\"update_available\":true}"
        )
        .is_err());
        assert!(core_from("").is_err());
    }

    #[test]
    fn a_failed_launcher_repair_keeps_its_reason() {
        let ok = launcher_from(true, "{\"status\":\"repaired\",\"warning\":null}", "");
        assert_eq!(ok.status, "repaired");
        assert_eq!(ok.detail, None);
        let failed = launcher_from(
            false,
            "",
            "pmux-progress {\"stage\":\"verify\"}\nError: existing monitor does not match the immutable official source asset\n",
        );
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.detail.as_deref(),
            Some("existing monitor does not match the immutable official source asset")
        );
    }

    fn restart_report() -> String {
        json!({"status":"restart_results","components":[
            {"component":"daemon","status":"deferred","detail":"sessions are running; use --stop-sessions only when ready to end them","sessions":["a","b"]},
            {"component":"plugins","status":"pending","detail":"bundled plug-in migration waits for the daemon restart"},
            {"component":"host","response":{"id":"r1","pid":41,"status":"restarted","detail":"","version":"0.3.73"}},
            {"component":"host","response":{"id":"r2","pid":42,"status":"deferred","detail":"owns a local terminal","version":"0.3.72"}},
            {"component":"mcp","status":"not_registered","detail":"no cooperative component found"},
        ]})
        .to_string()
    }

    #[test]
    fn deferred_components_carry_their_next_action() {
        let components = restart_from(&restart_report()).unwrap();
        let by = |name: &str, pid: Option<u64>| {
            components
                .iter()
                .find(|c| c.component == name && (pid.is_none() || c.pid == pid))
                .unwrap()
                .clone()
        };
        let daemon = by("daemon", None);
        assert_eq!(daemon.status, "deferred");
        assert!(daemon.next.as_deref().unwrap().contains("--stop-sessions"));
        assert!(by("plugins", None).next.is_some());
        let window = by("host", Some(42));
        assert_eq!(
            (window.status.as_str(), window.detail.as_deref()),
            ("deferred", Some("owns a local terminal"))
        );
        assert!(window
            .next
            .as_deref()
            .unwrap()
            .contains("pmux restart --host"));
        assert_eq!(by("host", Some(41)).next, None);
        assert_eq!(by("mcp", None).next, None);
        assert!(restart_from("{\"status\":\"planned\"}").is_err());
    }

    #[test]
    fn an_idle_window_the_restart_skipped_is_named_with_what_to_do() {
        let skipped = restart_from(
            &json!({"status":"restart_results","components":[
                {"component":"host","status":"not_registered","detail":"no cooperative component found"},
                {"component":"mcp","status":"not_registered","detail":"no cooperative component found"},
            ]})
            .to_string(),
        )
        .unwrap();
        let components = with_idle_hosts(skipped, &[77]);
        let hosts: Vec<&Component> = components
            .iter()
            .filter(|c| c.component == "host")
            .collect();
        assert_eq!(hosts.len(), 1, "{components:?}");
        assert_eq!((hosts[0].status.as_str(), hosts[0].pid), ("idle", Some(77)));
        assert!(hosts[0]
            .next
            .as_deref()
            .unwrap()
            .contains("pmux restart --host"));
        assert!(
            components.iter().any(|c| c.component == "mcp"),
            "other components stay"
        );
        let receipt = Receipt::new(
            "0.3.72".into(),
            installed(),
            repaired(),
            Restart::Ran { components },
        );
        assert_eq!(receipt.status, "updated_with_deferrals");
        assert!(
            receipt
                .text()
                .contains("  App window 77: idle (open but idle, so it was not asked to restart)"),
            "{}",
            receipt.text()
        );
        // A window the restart already named is not added twice.
        let named = restart_from(
            &json!({"status":"restart_results","components":[
                {"component":"host","status":"idle","pid":77,"detail":"open but idle, so it was not asked to restart"},
            ]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(with_idle_hosts(named.clone(), &[77]), named);
        assert_eq!(with_idle_hosts(named.clone(), &[]), named);
    }

    #[test]
    fn only_components_on_another_release_are_stale() {
        let versions = json!({
            "installed":[{"component":"pmux","version":"pmux 0.3.73 (abc)"}],
            "running":[
                {"component":"host","pid":7,"version":"0.3.72"},
                {"component":"host","pid":8,"version":"0.3.73"},
                {"component":"mcp","pid":9,"version":"0.3.73"},
            ],
            "daemon":{"version":"0.3.73","pid":3},
        });
        assert_eq!(stale_components(&versions, "0.3.73"), vec!["host 7"]);
        let idle = json!({"running":[],"daemon":{"status":"not_running"}});
        assert!(stale_components(&idle, "0.3.73").is_empty());
        let old_daemon = json!({"running":[],"daemon":{"version":"0.3.71","pid":3}});
        assert_eq!(stale_components(&old_daemon, "0.3.73"), vec!["daemon"]);
    }

    fn installed() -> Core {
        Core {
            status: "installed",
            version: "0.3.73".into(),
        }
    }

    fn repaired() -> Launcher {
        Launcher {
            status: "repaired".into(),
            detail: None,
        }
    }

    #[test]
    fn the_receipt_lists_every_step_and_what_to_do_next() {
        let components = restart_from(&restart_report()).unwrap();
        let receipt = Receipt::new(
            "0.3.72".into(),
            installed(),
            repaired(),
            Restart::Ran { components },
        );
        assert_eq!(receipt.status, "updated_with_deferrals");
        let text = receipt.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Updated Prismattyc 0.3.72 to 0.3.73.");
        assert_eq!(lines[1], "  Core: installed 0.3.73");
        assert_eq!(lines[2], "  Monitor launcher: installed 0.3.73");
        assert!(lines.contains(&"  Daemon: deferred (sessions are running; use --stop-sessions only when ready to end them)"), "{text}");
        assert!(lines.contains(&"  App window 41: restarted"), "{text}");
        assert!(
            lines.contains(&"  App window 42: deferred (owns a local terminal)"),
            "{text}"
        );
        assert!(lines.contains(&"  MCP adapter: none running"), "{text}");
        let next = lines.iter().position(|l| *l == "Next:").expect(&text);
        assert!(lines[next + 1].starts_with("  - Daemon: "), "{text}");
        assert!(
            lines[next..]
                .iter()
                .any(|l| l.starts_with("  - App window: ")),
            "{text}"
        );
        let value = serde_json::to_value(&receipt).unwrap();
        assert_eq!(value["status"], "updated_with_deferrals");
        assert_eq!(value["previous"], "0.3.72");
        assert_eq!(value["monitor_launcher"]["status"], "repaired");
        assert_eq!(value["restart"]["status"], "ran");
    }

    #[test]
    fn an_update_with_nothing_left_over_has_no_next_steps() {
        let components = restart_from(
            &json!({"status":"restart_results","components":[
                {"component":"daemon","status":"restarted"},
                {"component":"plugins","status":"reconciled"},
            ]})
            .to_string(),
        )
        .unwrap();
        let receipt = Receipt::new(
            "0.3.72".into(),
            installed(),
            repaired(),
            Restart::Ran { components },
        );
        assert_eq!(receipt.status, "updated");
        assert!(!receipt.text().contains("Next:"), "{}", receipt.text());
        assert!(receipt
            .text()
            .contains("  Official plug-ins: moved to the new release"));
    }

    #[test]
    fn an_up_to_date_install_says_so_and_restarts_nothing() {
        let core = Core {
            status: "current",
            version: "0.3.73".into(),
        };
        let launcher = Launcher {
            status: "current".into(),
            detail: None,
        };
        let receipt = Receipt::new("0.3.73".into(), core, launcher, Restart::NotNeeded);
        assert_eq!(receipt.status, "current");
        assert_eq!(
            receipt.text().lines().next(),
            Some("Prismattyc 0.3.73 is up to date.")
        );
    }

    #[test]
    fn a_failed_launcher_makes_the_update_incomplete_and_names_the_repair() {
        let launcher = launcher_from(
            false,
            "",
            "Error: launcher differs from both recorded states\n",
        );
        let receipt = Receipt::new("0.3.72".into(), installed(), launcher, Restart::NotNeeded);
        assert_eq!(receipt.status, "incomplete");
        assert!(!receipt.ok());
        let text = receipt.text();
        assert!(text.contains("  Monitor launcher: kept the existing launcher: launcher differs from both recorded states"), "{text}");
        assert!(
            text.contains("--repair-monitor-launcher --from-monitor-version"),
            "{text}"
        );
    }

    #[test]
    fn no_restart_names_what_still_runs_the_old_release() {
        let receipt = Receipt::new(
            "0.3.72".into(),
            installed(),
            repaired(),
            Restart::Skipped {
                stale: vec!["host 7".into(), "daemon".into()],
            },
        );
        assert_eq!(receipt.status, "updated_with_deferrals");
        let text = receipt.text();
        assert!(
            text.contains("still on the old release: host 7, daemon"),
            "{text}"
        );
        assert!(text.contains("  - Run pmux restart"), "{text}");
    }
}
