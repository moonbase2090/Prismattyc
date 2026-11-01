// SPDX-License-Identifier: MIT
//! Explicit conversion of legacy host definitions, without executing transports.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

use super::endpoint::{validate_label, Endpoint, EndpointDefinition, EndpointSource};

/// Validate the entire batch before its one journal transition. Matching
/// definitions retain their identities, including after an uncertain retry.
pub(super) fn plan(
    existing: &BTreeMap<String, Endpoint>,
    definitions: Vec<EndpointDefinition>,
) -> Result<Vec<Endpoint>> {
    ensure!(
        definitions.len() <= 31,
        "at most 31 remote hosts may be imported"
    );
    let mut labels = BTreeSet::new();
    let mut result = Vec::new();
    let mut additions = 0;
    for definition in definitions {
        validate_label(&definition.label)?;
        ensure!(
            definition.label != "local",
            "local is reserved for the selected daemon"
        );
        definition.source.validate()?;
        ensure!(
            labels.insert(definition.label.clone()),
            "duplicate import label {}; correct the collision",
            definition.label
        );
        if let Some(endpoint) = existing.get(&definition.label) {
            ensure!(endpoint.source == definition.source, "endpoint label {} already has another transport; correct the collision before import", definition.label);
            result.push(endpoint.clone());
        } else {
            additions += 1;
            result.push(Endpoint {
                id: super::package::nonce()?,
                label: definition.label,
                source: definition.source,
            });
        }
    }
    ensure!(
        existing.len() + additions <= 31,
        "too many configured endpoints"
    );
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostsFile {
    min_interval_ms: Option<u64>,
    #[serde(default, rename = "host")]
    hosts: Vec<Host>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Host {
    label: String,
    socket: Option<String>,
    command: Option<Vec<String>>,
}

pub(super) struct Import {
    pub apply: bool,
    pub endpoints: Vec<EndpointDefinition>,
}

impl Import {
    pub fn parse(args: &[String], socket: &Path) -> Result<Self> {
        let mut apply = false;
        let mut endpoints = Vec::new();
        let mut args = args.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--apply" if !apply => apply = true,
                "--host" => {
                    let host = args.next().context("--host requires LABEL=SOCKET")?;
                    let (label, path) = host.split_once('=').context("--host requires LABEL=SOCKET")?;
                    endpoints.push(EndpointDefinition {
                        label: label.into(),
                        source: EndpointSource::Socket { path: path.into() },
                    });
                }
                "--host-cmd" => {
                    let label = args.next().context("--host-cmd requires LABEL JSON_ARGV")?.clone();
                    let argv = serde_json::from_str(args.next().context("--host-cmd requires LABEL JSON_ARGV")?)
                        .context("--host-cmd requires a JSON argv array")?;
                    endpoints.push(EndpointDefinition { label, source: EndpointSource::CommandTail { argv } });
                }
                "--activity-hosts" => {
                    let path = args.next().context("--activity-hosts requires a TOML file")?;
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)?.take(512 * 1024 + 1).read_to_end(&mut bytes)?;
                    ensure!(bytes.len() <= 512 * 1024, "legacy hosts file exceeds 512 KiB");
                    let file: HostsFile = toml::from_str(std::str::from_utf8(&bytes)?)
                        .context("invalid legacy activity hosts file")?;
                    ensure!(file.min_interval_ms.unwrap_or(1000) == 1000,
                        "legacy min_interval_ms must match the MCP adapter's 1000 ms cadence; correct it before import");
                    ensure!(!file.hosts.is_empty() && file.hosts.len() <= 32, "legacy file requires 1 to 32 hosts");
                    for host in file.hosts {
                        let source = match (host.socket, host.command) {
                            (Some(path), None) => EndpointSource::Socket {
                                path: if path == "default" { socket.into() } else { path.into() },
                            },
                            (None, Some(argv)) => EndpointSource::CommandQuery { argv },
                            _ => bail!("legacy host requires exactly one socket or command"),
                        };
                        endpoints.push(EndpointDefinition { label: host.label, source });
                    }
                }
                _ => bail!("usage: pmux plugin endpoint import [--apply] [--host LABEL=SOCKET] [--host-cmd LABEL JSON_ARGV] [--activity-hosts FILE]"),
            }
        }
        ensure!(
            !endpoints.is_empty() && endpoints.len() <= 32,
            "import requires 1 to 32 explicit hosts"
        );
        let mut labels = BTreeSet::new();
        for endpoint in &endpoints {
            validate_label(&endpoint.label)?;
            endpoint.source.validate()?;
            ensure!(
                labels.insert(&endpoint.label),
                "duplicate import label {}; correct the collision",
                endpoint.label
            );
            if endpoint.label == "local" {
                ensure!(
                    endpoint.source
                        == (EndpointSource::Socket {
                            path: socket.into()
                        }),
                    "local is reserved for the selected daemon; rename the conflicting legacy host"
                );
            }
        }
        // The selected daemon already owns the local scope and its identity.
        endpoints.retain(|endpoint| endpoint.label != "local");
        ensure!(
            endpoints.len() <= 31,
            "at most 31 remote hosts may be imported"
        );
        Ok(Self { apply, endpoints })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn monitor_import_preserves_tail_argv_and_never_applies_implicitly() {
        let import = Import::parse(
            &args(&[
                "--host",
                "remote=/tmp/remote.sock",
                "--host-cmd",
                "a-24-character-host-label",
                r#"["ssh","remote","pmux","activity","tail","--from-oldest"]"#,
            ]),
            Path::new("/tmp/local.sock"),
        )
        .unwrap();
        assert!(!import.apply);
        assert_eq!(import.endpoints.len(), 2);
        assert_eq!(
            import.endpoints[1].source,
            EndpointSource::CommandTail {
                argv: args(&["ssh", "remote", "pmux", "activity", "tail", "--from-oldest"]),
            }
        );
    }

    #[test]
    fn import_rejects_collisions_before_any_administration() {
        for values in [
            args(&[
                "--host",
                "duplicate=/tmp/one",
                "--host",
                "duplicate=/tmp/two",
            ]),
            args(&["--host", "local=/tmp/other.sock"]),
            args(&["--host", "relative=socket"]),
            args(&["--host-cmd", "empty", "[]"]),
        ] {
            assert!(Import::parse(&values, Path::new("/tmp/local.sock")).is_err());
        }
        let import = Import::parse(
            &args(&["--apply", "--host", "local=/tmp/local.sock"]),
            Path::new("/tmp/local.sock"),
        )
        .unwrap();
        assert!(import.apply);
        assert!(import.endpoints.is_empty());
    }

    #[test]
    fn plan_retains_identity_and_rejects_a_conflicting_batch() {
        let endpoint = Endpoint {
            id: "a".repeat(32),
            label: "remote".into(),
            source: EndpointSource::Socket {
                path: "/tmp/remote.sock".into(),
            },
        };
        let existing = BTreeMap::from([("remote".into(), endpoint.clone())]);
        let unchanged = EndpointDefinition {
            label: "remote".into(),
            source: endpoint.source.clone(),
        };
        assert_eq!(plan(&existing, vec![unchanged]).unwrap(), vec![endpoint]);
        let new = EndpointDefinition {
            label: "new".into(),
            source: EndpointSource::Socket {
                path: "/tmp/new.sock".into(),
            },
        };
        let conflict = EndpointDefinition {
            label: "remote".into(),
            source: EndpointSource::Socket {
                path: "/tmp/other.sock".into(),
            },
        };
        assert!(plan(&existing, vec![new, conflict]).is_err());
        assert_eq!(existing.len(), 1);
    }
}
