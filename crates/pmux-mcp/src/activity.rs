// SPDX-License-Identifier: MIT
//! Activity feed endpoints for the read-only `pmux_activity_*` tools.
//!
//! Each configured host is a label plus one endpoint: a local pmuxd socket,
//! or a command that runs a remote `pmux` (for example over SSH). Every query
//! runs `pmux activity ...` against the endpoint and returns its JSON. Tool
//! arguments only name a configured host; they never supply a socket path or
//! a command.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

/// Label used when no hosts file is given.
pub const LOCAL_HOST: &str = "local";
const MAX_HOSTS: usize = 32;
const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(1000);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A pmuxd socket on this machine. `None` uses pmux's default socket.
    Socket(Option<PathBuf>),
    /// argv prefix for a `pmux` on another machine, e.g. `ssh -T host pmux`.
    Command(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub label: String,
    pub endpoint: Endpoint,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostsFile {
    #[serde(default)]
    min_interval_ms: Option<u64>,
    #[serde(default, rename = "host")]
    hosts: Vec<HostEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostEntry {
    label: String,
    #[serde(default)]
    socket: Option<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
}

/// The hosts this server may query, with a per-host minimum call interval.
#[derive(Debug)]
pub struct Hosts {
    hosts: Vec<Host>,
    min_interval: Duration,
    last_call: Mutex<HashMap<String, Instant>>,
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// A tool-level failure, returned as the same JSON shape `pmux activity` uses.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityError {
    pub code: &'static str,
    pub message: String,
    pub host: Option<String>,
    pub retry_after_ms: Option<u64>,
}

impl ActivityError {
    fn new(code: &'static str, message: impl Into<String>, host: Option<&str>) -> Self {
        Self {
            code,
            message: message.into(),
            host: host.map(str::to_owned),
            retry_after_ms: None,
        }
    }

    pub fn to_json(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(ms) = self.retry_after_ms {
            error["retry_after_ms"] = json!(ms);
        }
        let mut value = json!({"schema_version": 1, "error": error});
        if let Some(host) = &self.host {
            value["host"] = json!(host);
        }
        value
    }
}

impl Hosts {
    /// One host, `local`, on the given socket.
    pub fn local(socket: PathBuf) -> Self {
        Self::new(
            vec![Host {
                label: LOCAL_HOST.into(),
                endpoint: Endpoint::Socket(Some(socket)),
            }],
            DEFAULT_MIN_INTERVAL,
        )
    }

    fn new(hosts: Vec<Host>, min_interval: Duration) -> Self {
        Self {
            hosts,
            min_interval,
            last_call: Mutex::new(HashMap::new()),
        }
    }

    /// Parse a hosts file. See `docs/activity-connector.md` for the format.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: HostsFile = toml::from_str(text).map_err(|e| format!("hosts file: {e}"))?;
        if file.hosts.is_empty() {
            return Err("hosts file: add at least one [[host]]".into());
        }
        if file.hosts.len() > MAX_HOSTS {
            return Err(format!("hosts file: at most {MAX_HOSTS} hosts"));
        }
        let mut seen = HashSet::new();
        let mut hosts = Vec::new();
        for entry in file.hosts {
            if !valid_label(&entry.label) {
                return Err(format!(
                    "hosts file: label {:?} must be 1 to 64 letters, digits, '-', '_' or '.'",
                    entry.label
                ));
            }
            if !seen.insert(entry.label.clone()) {
                return Err(format!("hosts file: duplicate label {:?}", entry.label));
            }
            let endpoint = match (entry.socket, entry.command) {
                (Some(socket), None) if socket == "default" => Endpoint::Socket(None),
                (Some(socket), None) if Path::new(&socket).is_absolute() => {
                    Endpoint::Socket(Some(PathBuf::from(socket)))
                }
                (Some(_), None) => {
                    return Err(format!(
                        "hosts file: {}: socket must be an absolute path or \"default\"",
                        entry.label
                    ))
                }
                (None, Some(command))
                    if !command.is_empty() && command.iter().all(|a| !a.is_empty()) =>
                {
                    Endpoint::Command(command)
                }
                (None, Some(_)) => {
                    return Err(format!(
                        "hosts file: {}: command must be a non-empty list of non-empty arguments",
                        entry.label
                    ))
                }
                _ => {
                    return Err(format!(
                        "hosts file: {}: give exactly one of socket or command",
                        entry.label
                    ))
                }
            };
            hosts.push(Host {
                label: entry.label,
                endpoint,
            });
        }
        let min_interval = file
            .min_interval_ms
            .map_or(DEFAULT_MIN_INTERVAL, Duration::from_millis);
        Ok(Self::new(hosts, min_interval))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read hosts file {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Labels and endpoint kinds. Paths and commands stay private to the server.
    pub fn summary(&self) -> Value {
        let hosts: Vec<Value> = self
            .hosts
            .iter()
            .map(|host| {
                let kind = match host.endpoint {
                    Endpoint::Socket(_) => "local",
                    Endpoint::Command(_) => "command",
                };
                json!({"label": host.label, "endpoint": kind})
            })
            .collect();
        json!({
            "schema_version": 1,
            "min_interval_ms": self.min_interval.as_millis() as u64,
            "hosts": hosts
        })
    }

    /// Pick the host for a call. With one host, the label may be omitted.
    pub fn select(&self, label: Option<&str>) -> Result<&Host, ActivityError> {
        match label {
            Some(label) => self.hosts.iter().find(|h| h.label == label).ok_or_else(|| {
                ActivityError::new(
                    "unknown_host",
                    "host is not configured; call pmux_activity_hosts",
                    Some(label),
                )
            }),
            None if self.hosts.len() == 1 => Ok(&self.hosts[0]),
            None => Err(ActivityError::new(
                "host_required",
                "several hosts are configured; pass host (see pmux_activity_hosts)",
                None,
            )),
        }
    }

    /// Admit a call to `label` unless the previous one was too recent.
    pub fn admit(&self, label: &str, now: Instant) -> Result<(), ActivityError> {
        let mut last = self.last_call.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = last.get(label) {
            let elapsed = now.saturating_duration_since(*previous);
            if elapsed < self.min_interval {
                let mut error = ActivityError::new(
                    "rate_limited",
                    "too soon after the previous call to this host",
                    Some(label),
                );
                error.retry_after_ms = Some((self.min_interval - elapsed).as_millis() as u64 + 1);
                return Err(error);
            }
        }
        last.insert(label.to_owned(), now);
        Ok(())
    }
}

/// argv for `pmux activity ARGS` against `host`, with its label attached.
pub fn command_line(pmux: &Path, host: &Host, args: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = match &host.endpoint {
        Endpoint::Socket(Some(socket)) => vec![
            pmux.display().to_string(),
            "--socket".into(),
            socket.display().to_string(),
        ],
        Endpoint::Socket(None) => vec![pmux.display().to_string()],
        Endpoint::Command(prefix) => prefix.clone(),
    };
    argv.push("activity".into());
    argv.extend(args.iter().cloned());
    argv.extend(["--host-label".into(), host.label.clone()]);
    argv
}

/// Run one `pmux activity` query. `Ok` carries the JSON text and whether
/// pmux reported success; `Err` is a failure to get any JSON back.
pub async fn run(
    pmux: &Path,
    host: &Host,
    args: &[String],
) -> Result<(bool, String), ActivityError> {
    let argv = command_line(pmux, host, args);
    let timeout = match host.endpoint {
        Endpoint::Socket(_) => SOCKET_TIMEOUT,
        Endpoint::Command(_) => COMMAND_TIMEOUT,
    };
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let label = Some(host.label.as_str());
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(ActivityError::new(
                "host_unreachable",
                format!("cannot start the endpoint command: {error}"),
                label,
            ))
        }
        Err(_) => {
            return Err(ActivityError::new(
                "host_unreachable",
                format!("no answer within {} s", timeout.as_secs()),
                label,
            ))
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stdout.lines().rev().find(|line| !line.trim().is_empty());
    match last.map(serde_json::from_str::<Value>) {
        Some(Ok(_)) if output.stdout.len() <= MAX_OUTPUT => {
            Ok((output.status.success(), stdout.trim_end().to_owned()))
        }
        Some(Ok(_)) => Err(ActivityError::new(
            "request_failed",
            "endpoint output exceeds the size limit",
            label,
        )),
        _ => {
            // No JSON: the endpoint itself failed (SSH refused, pmux missing,
            // or a pmux without `activity`). Report a short stderr excerpt.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let excerpt: String = stderr.trim().chars().take(300).collect();
            Err(ActivityError::new(
                "host_unreachable",
                format!(
                    "endpoint exited with {} and no JSON: {excerpt}",
                    output.status
                ),
                label,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = r#"
min_interval_ms = 250
[[host]]
label = "mac"
socket = "default"
[[host]]
label = "win-1"
command = ["ssh", "-T", "win-1", "pmux"]
"#;

    #[test]
    fn hosts_file_maps_labels_to_socket_and_command_endpoints() -> Result<(), String> {
        let hosts = Hosts::parse(TWO)?;
        assert_eq!(
            hosts.select(Some("mac")).map(|h| &h.endpoint),
            Ok(&Endpoint::Socket(None))
        );
        let win = hosts.select(Some("win-1")).map_err(|e| e.message)?;
        assert_eq!(
            win.endpoint,
            Endpoint::Command(vec![
                "ssh".into(),
                "-T".into(),
                "win-1".into(),
                "pmux".into()
            ])
        );
        let summary = hosts.summary();
        assert_eq!(summary["min_interval_ms"], 250);
        assert_eq!(
            summary["hosts"][1],
            json!({"label": "win-1", "endpoint": "command"})
        );
        assert!(
            !summary.to_string().contains("ssh"),
            "commands stay private"
        );
        Ok(())
    }

    #[test]
    fn hosts_file_rejects_ambiguous_or_unsafe_entries() {
        for bad in [
            "",
            "[[host]]\nlabel = \"a\"",
            "[[host]]\nlabel = \"a\"\nsocket = \"/s\"\ncommand = [\"x\"]",
            "[[host]]\nlabel = \"a\"\nsocket = \"relative.sock\"",
            "[[host]]\nlabel = \"a\"\ncommand = []",
            "[[host]]\nlabel = \"a\"\ncommand = [\"\"]",
            "[[host]]\nlabel = \"has space\"\nsocket = \"default\"",
            "[[host]]\nlabel = \"a\"\nsocket = \"default\"\n[[host]]\nlabel = \"a\"\nsocket = \"default\"",
            "[[host]]\nlabel = \"a\"\nsocket = \"default\"\nextra = 1",
        ] {
            assert!(Hosts::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn host_is_optional_only_with_a_single_host() -> Result<(), String> {
        let one = Hosts::local(PathBuf::from("/tmp/x.sock"));
        assert_eq!(one.select(None).map(|h| h.label.as_str()), Ok(LOCAL_HOST));
        let two = Hosts::parse(TWO)?;
        assert_eq!(two.select(None).map_err(|e| e.code), Err("host_required"));
        assert_eq!(
            two.select(Some("other")).map_err(|e| e.code),
            Err("unknown_host")
        );
        Ok(())
    }

    #[test]
    fn calls_to_one_host_are_spaced_by_the_minimum_interval() -> Result<(), String> {
        let hosts = Hosts::parse(TWO)?;
        let start = Instant::now();
        assert!(hosts.admit("mac", start).is_ok());
        let error = hosts
            .admit("mac", start + Duration::from_millis(100))
            .expect_err("second call is too soon");
        assert_eq!(error.code, "rate_limited");
        assert_eq!(error.retry_after_ms, Some(151));
        assert!(
            hosts
                .admit("win-1", start + Duration::from_millis(100))
                .is_ok(),
            "hosts are independent"
        );
        assert!(hosts
            .admit("mac", start + Duration::from_millis(250))
            .is_ok());
        Ok(())
    }

    #[test]
    fn command_lines_target_the_endpoint_and_carry_the_label() -> Result<(), String> {
        let hosts = Hosts::parse(TWO)?;
        let pmux = Path::new("/bin/pmux");
        let args = vec!["poll".to_string(), "--limit".into(), "5".into()];
        let local = command_line(
            pmux,
            hosts.select(Some("mac")).map_err(|e| e.message)?,
            &args,
        );
        assert_eq!(
            local,
            [
                "/bin/pmux",
                "activity",
                "poll",
                "--limit",
                "5",
                "--host-label",
                "mac"
            ]
        );
        let remote = command_line(
            pmux,
            hosts.select(Some("win-1")).map_err(|e| e.message)?,
            &args,
        );
        assert_eq!(
            remote,
            [
                "ssh",
                "-T",
                "win-1",
                "pmux",
                "activity",
                "poll",
                "--limit",
                "5",
                "--host-label",
                "win-1"
            ]
        );
        let socket = command_line(
            pmux,
            &Hosts::local(PathBuf::from("/s.sock")).hosts[0],
            &args,
        );
        assert_eq!(&socket[..3], ["/bin/pmux", "--socket", "/s.sock"]);
        Ok(())
    }

    #[test]
    fn errors_use_the_pmux_activity_json_shape() {
        let mut error = ActivityError::new("rate_limited", "wait", Some("mac"));
        error.retry_after_ms = Some(10);
        assert_eq!(
            error.to_json(),
            json!({"schema_version": 1, "host": "mac",
                   "error": {"code": "rate_limited", "message": "wait", "retry_after_ms": 10}})
        );
    }
}
