// SPDX-License-Identifier: MIT
//! One-step plug-in updates, shared by `pmux plugin update` and the settings
//! page: install the newest compatible version, then apply it with the
//! permissions already granted. A plug-in that is off stays off.

use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Serialize;

use super::{
    cli::admin, AdminAction, CatalogEntry, Client, Installed, Manifest, Reply, Request, Status,
};

/// What happened to one plug-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub id: String,
    /// The version it ran before, if any.
    pub from: Option<String>,
    /// The newest version offered, if newer than `from`.
    pub to: Option<String>,
    pub result: Result_,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum Result_ {
    /// Already the newest version.
    Current,
    /// Installed and applied with the permissions it already had.
    Updated,
    /// Installed only: the plug-in is off, so it stays off.
    InstalledStaysOff,
    /// Installed and turned on with its required permissions.
    InstalledAndEnabled,
    /// Not updated; the reason names what to do.
    Failed(String),
}

/// The newest catalog version of `id` that is newer than `than`.
fn newest<'a>(
    catalog: &'a [CatalogEntry],
    id: &str,
    than: Option<&str>,
) -> Option<&'a CatalogEntry> {
    let floor = than.and_then(|version| Version::parse(version).ok());
    catalog
        .iter()
        .filter(|entry| entry.id == id)
        .filter_map(|entry| Some((Version::parse(&entry.version).ok()?, entry)))
        .filter(|(version, _)| floor.as_ref().is_none_or(|floor| version > floor))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, entry)| entry)
}

/// Ids with a newer catalog version than the one in use, and that version.
pub fn available(catalog: &[CatalogEntry], states: &[Status]) -> Vec<(String, String)> {
    let mut ids = catalog
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .filter_map(|id| {
            let current = states
                .iter()
                .find(|state| state.id == id)
                .and_then(|state| state.selected.as_ref())
                .map(|package| package.version.as_str());
            states.iter().find(|state| state.id == id)?;
            let entry = newest(catalog, &id, current)?;
            Some((id, entry.version.clone()))
        })
        .collect()
}

fn installed_of(client: &mut Client, id: &str, version: &str) -> Result<(Installed, Manifest)> {
    let Reply::List { installed, .. } = client.request(Request::List)? else {
        bail!("unexpected installed package reply");
    };
    let package = installed
        .into_iter()
        .find(|package| package.id == id && package.version == version)
        .context("the new version did not install")?;
    let Reply::Inspection { manifest, .. } = client.request(Request::Inspect {
        id: id.into(),
        version: Some(version.into()),
    })?
    else {
        bail!("unexpected manifest reply");
    };
    Ok((
        package,
        manifest.context("the new version has no manifest")?,
    ))
}

/// Update `only` (or every plug-in the daemon knows) to its newest version.
/// With `enable_off`, a plug-in that is off is turned on with the permissions
/// its manifest requires; without it, it is installed and left off.
pub fn update(client: &mut Client, only: Option<&str>, enable_off: bool) -> Result<Vec<Outcome>> {
    let Reply::Catalog { packages: catalog } =
        client.request(Request::Discover { refresh: true })?
    else {
        bail!("unexpected catalog reply");
    };
    let Reply::List { states, .. } = client.request(Request::List)? else {
        bail!("unexpected installed package reply");
    };
    let mut ids = states
        .iter()
        .map(|state| state.id.clone())
        .collect::<Vec<_>>();
    if let Some(id) = only {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_owned());
        }
        ids.retain(|known| known == id);
    }
    let mut outcomes = Vec::new();
    for id in ids {
        let state = states.iter().find(|state| state.id == id);
        let from = state
            .and_then(|state| state.selected.as_ref())
            .map(|package| package.version.clone());
        let Some(entry) = newest(&catalog, &id, from.as_deref()) else {
            outcomes.push(Outcome {
                id,
                from,
                to: None,
                result: Result_::Current,
            });
            continue;
        };
        let to = Some(entry.version.clone());
        let result = apply(client, &id, &entry.version, state, enable_off)
            .unwrap_or_else(|error| Result_::Failed(format!("{error:#}")));
        outcomes.push(Outcome {
            id,
            from,
            to,
            result,
        });
    }
    Ok(outcomes)
}

fn apply(
    client: &mut Client,
    id: &str,
    version: &str,
    state: Option<&Status>,
    enable_off: bool,
) -> Result<Result_> {
    let reply = client.request(admin(AdminAction::Install {
        id: id.into(),
        version: Some(version.into()),
    })?)?;
    if let Reply::Retry { .. } = reply {
        bail!("the catalog changed while installing; run the update again");
    }
    let (package, manifest) = installed_of(client, id, version)?;
    manifest.compatibility().map_err(|error| {
        anyhow::anyhow!("version {version} does not fit this release ({error})")
    })?;
    let on = state.is_some_and(|state| state.enabled);
    if on {
        let grants = state.map(|state| state.grants.clone()).unwrap_or_default();
        client.request(admin(AdminAction::Enable { package, grants })?)?;
        Ok(Result_::Updated)
    } else if enable_off {
        client.request(admin(AdminAction::Enable {
            package,
            grants: manifest.required_grants(),
        })?)?;
        Ok(Result_::InstalledAndEnabled)
    } else {
        Ok(Result_::InstalledStaysOff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, version: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.into(),
            version: version.into(),
            sha256: "0".repeat(64),
            bytes: 1,
        }
    }

    fn state(id: &str, version: &str) -> Status {
        Status {
            id: id.into(),
            selected: Some(Installed {
                id: id.into(),
                version: version.into(),
                digest: "0".repeat(64),
            }),
            enabled: true,
            grants: Vec::new(),
            observed: super::super::Observed::Running,
            failure: None,
        }
    }

    #[test]
    fn the_newest_version_wins_by_semver_not_by_text() {
        let catalog = vec![
            entry("a", "1.9.0"),
            entry("a", "1.10.0"),
            entry("a", "1.2.0"),
        ];
        assert_eq!(newest(&catalog, "a", None).unwrap().version, "1.10.0");
        assert_eq!(
            newest(&catalog, "a", Some("1.9.0")).unwrap().version,
            "1.10.0"
        );
        assert!(
            newest(&catalog, "a", Some("1.10.0")).is_none(),
            "equal is not newer"
        );
        assert!(newest(&catalog, "b", None).is_none());
    }

    #[test]
    fn only_plugins_in_use_with_a_newer_version_are_available() {
        let catalog = vec![
            entry("a", "1.1.0"),
            entry("b", "2.0.0"),
            entry("c", "3.0.0"),
        ];
        let states = vec![state("a", "1.0.0"), state("b", "2.0.0")];
        assert_eq!(
            available(&catalog, &states),
            vec![("a".to_string(), "1.1.0".to_string())]
        );
    }
}
