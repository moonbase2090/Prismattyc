// SPDX-License-Identifier: MIT
//! Official plug-ins whose bundled update waits for a daemon restart.
//!
//! The official plug-ins ship inside `pmuxd`, and the daemon installs its bundled
//! versions only when it starts. After an update, the installed `pmuxd` may bundle
//! newer versions than the running daemon selected; those changes apply at the next
//! daemon restart. The running daemon asks the installed binary which versions it
//! bundles (`pmuxd --bundled-plugins`) and compares them with its own selections.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};

use super::catalog::Catalog;
use super::manager::DEFAULT_PLUGINS;

/// `pmuxd --bundled-plugins` prints this, one JSON document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundled {
    pub plugins: Vec<BundledPlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundledPlugin {
    pub id: String,
    pub version: String,
}

/// One official plug-in that changes when the daemon restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPlugin {
    pub id: String,
    /// The selected version now, or none when the plug-in is not installed yet.
    pub from: Option<String>,
    pub to: String,
}

/// The newest compatible version of each official plug-in this binary bundles.
pub fn bundled() -> Result<Bundled> {
    let catalog = Catalog::bundled()?;
    let mut plugins = Vec::new();
    for id in DEFAULT_PLUGINS {
        if let Some(entry) = catalog.newest_compatible_bundle(id, None)? {
            plugins.push(BundledPlugin {
                id: id.into(),
                version: entry.entry.version.clone(),
            });
        }
    }
    Ok(Bundled { plugins })
}

/// The official plug-ins `executable` bundles; this binary's own when there is none,
/// or when it is not newer than this one.
///
/// Only a newer `pmuxd` is asked with `--bundled-plugins`: every release newer than
/// this one knows the flag, while an older one would read it as a program to run.
/// Its version is read first with `--version`, which every release knows.
pub(super) fn bundled_by(executable: Option<&std::path::Path>) -> Result<Bundled> {
    bundled_newer_than(executable, prismattyc_core::package_version())
}

fn bundled_newer_than(executable: Option<&std::path::Path>, running: &str) -> Result<Bundled> {
    let Some(executable) = executable else {
        return bundled();
    };
    let label = crate::release_update::version_label(executable)?;
    let installed = label
        .split_whitespace()
        .nth(1)
        .and_then(|version| Version::parse(version).ok())
        .context("the installed daemon reported an unreadable version")?;
    if Version::parse(running).is_ok_and(|running| installed <= running) {
        return bundled();
    }
    let text = crate::release_update::bundled_plugins_label(executable)?;
    serde_json::from_str(text.trim())
        .context("the installed daemon reported unreadable plug-in versions")
}

/// The selection of one official plug-in, as startup reconciliation sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Selection {
    /// The user turned it off; reconciliation leaves it alone.
    Disabled,
    Enabled(String),
}

/// Which official plug-ins the installed binary would install or update at the next
/// daemon start. Only official plug-ins are considered, a disabled one stays as it
/// is, and a bundle that is not newer than the selection changes nothing.
pub(super) fn pending(
    installed: &Bundled,
    selections: &BTreeMap<String, Selection>,
) -> Vec<PendingPlugin> {
    installed
        .plugins
        .iter()
        .filter(|plugin| DEFAULT_PLUGINS.contains(&plugin.id.as_str()))
        .filter_map(|plugin| {
            let to = Version::parse(&plugin.version).ok()?;
            let from = match selections.get(&plugin.id) {
                Some(Selection::Disabled) => return None,
                Some(Selection::Enabled(version)) => Some(version),
                None => None,
            };
            if from.is_some_and(|from| Version::parse(from).is_ok_and(|from| from >= to)) {
                return None;
            }
            Some(PendingPlugin {
                id: plugin.id.clone(),
                from: from.cloned(),
                to: plugin.version.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: &str = "org.moonbase.activity-monitor";
    const CONNECTOR: &str = "org.moonbase.activity-connector";

    fn installed(plugins: &[(&str, &str)]) -> Bundled {
        Bundled {
            plugins: plugins
                .iter()
                .map(|(id, version)| BundledPlugin {
                    id: (*id).into(),
                    version: (*version).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_newer_bundle_is_pending_and_an_equal_or_older_one_is_not() {
        let selections = BTreeMap::from([
            (MONITOR.to_owned(), Selection::Enabled("1.0.0".into())),
            (CONNECTOR.to_owned(), Selection::Enabled("2.1.0".into())),
        ]);
        let pending = pending(
            &installed(&[(MONITOR, "1.1.0"), (CONNECTOR, "2.1.0")]),
            &selections,
        );
        assert_eq!(
            pending,
            [PendingPlugin {
                id: MONITOR.into(),
                from: Some("1.0.0".into()),
                to: "1.1.0".into()
            }]
        );
        assert!(super::pending(&installed(&[(MONITOR, "0.9.0")]), &selections).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_daemon_is_asked_for_its_bundles_and_a_bad_answer_is_an_error() {
        let dir = std::env::temp_dir().join(format!("pmux-bundled-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Install each script through a child that has exited: a file this process
        // still has open for writing can be copied into another test's fork and then
        // fail to exec as busy.
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            let source = dir.join(format!("{name}.source"));
            std::fs::write(&source, format!("#!/bin/sh\n{body}\n")).unwrap();
            let installed = prismattyc_process::status(
                std::process::Command::new("install")
                    .args(["-m", "755"])
                    .arg(&source)
                    .arg(&path),
            )
            .unwrap();
            assert!(installed.success(), "install the fake pmuxd");
            std::fs::remove_file(&source).unwrap();
            path
        };
        let asked = dir.join("asked");
        let newer = script(
            "pmuxd-newer",
            &format!(
                r#"case "$1" in --version) echo 'pmuxd 9.0.0 (abc)';; --bundled-plugins) printf '{{"plugins":[{{"id":"{MONITOR}","version":"9.0.0"}}]}}\n';; *) exit 9;; esac"#
            ),
        );
        let found = bundled_newer_than(Some(&newer), "1.0.0").unwrap();
        let selections = BTreeMap::from([(MONITOR.to_owned(), Selection::Enabled("1.0.0".into()))]);
        assert_eq!(
            pending(&found, &selections),
            [PendingPlugin {
                id: MONITOR.into(),
                from: Some("1.0.0".into()),
                to: "9.0.0".into()
            }]
        );
        // A newer daemon that cannot answer is an error, not "nothing pending".
        let broken = script(
            "pmuxd-broken",
            r#"case "$1" in --version) echo 'pmuxd 9.0.0 (abc)';; *) exit 2;; esac"#,
        );
        let error = bundled_newer_than(Some(&broken), "1.0.0").unwrap_err();
        assert!(
            format!("{error:#}").contains("bundled plug-ins"),
            "{error:#}"
        );
        // A daemon that is not newer is never run with the new flag: an older release
        // would take it for a program to start.
        let older = script(
            "pmuxd-older",
            &format!(
                r#"case "$1" in --version) echo 'pmuxd 1.0.0 (abc)';; *) touch '{}';; esac"#,
                asked.display()
            ),
        );
        assert_eq!(
            bundled_newer_than(Some(&older), "1.0.0").unwrap(),
            bundled().unwrap()
        );
        assert!(
            !asked.exists(),
            "a daemon that is not newer was run with --bundled-plugins"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_disabled_plugin_stays_off_and_a_missing_one_is_installed_at_restart() {
        let selections = BTreeMap::from([(MONITOR.to_owned(), Selection::Disabled)]);
        let pending = pending(
            &installed(&[(MONITOR, "1.1.0"), (CONNECTOR, "2.0.0")]),
            &selections,
        );
        assert_eq!(
            pending,
            [PendingPlugin {
                id: CONNECTOR.into(),
                from: None,
                to: "2.0.0".into()
            }]
        );
    }

    #[test]
    fn only_official_plugins_and_valid_versions_count() {
        let pending = pending(
            &installed(&[("org.example.other", "9.0.0"), (MONITOR, "not-a-version")]),
            &BTreeMap::new(),
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn this_binary_reports_only_official_plugins_and_nothing_is_pending_against_itself() {
        // A build without the release bundle carries no packages; one with it carries
        // the official plug-ins. Either way only official ids with real versions appear.
        let own = bundled().unwrap();
        for plugin in &own.plugins {
            assert!(
                DEFAULT_PLUGINS.contains(&plugin.id.as_str()),
                "{}",
                plugin.id
            );
            Version::parse(&plugin.version).unwrap();
        }
        // A daemon compared with its own binary has nothing pending after startup.
        let selections = own
            .plugins
            .iter()
            .map(|plugin| {
                (
                    plugin.id.clone(),
                    Selection::Enabled(plugin.version.clone()),
                )
            })
            .collect();
        assert!(pending(&own, &selections).is_empty());
        let json = serde_json::to_string(&own).unwrap();
        assert_eq!(serde_json::from_str::<Bundled>(&json).unwrap(), own);
    }
}
