// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};

use super::manifest::validate_plugin_id;
use super::package::{digest, valid_digest, MAX_PACKAGE_BYTES};
use crate::release_update::{download_bounded, fetch_release_json, Release, REPOSITORY};

const CATALOG_TAG_PREFIX: &str = "pmux-plugins-v";
const CATALOG_ASSET: &str = "pmux-plugins.json";
include!(concat!(env!("OUT_DIR"), "/plugin_catalog.rs"));

pub(super) fn bundled_identity() -> Result<serde_json::Value> {
    let catalog = Catalog::bundled()?;
    Ok(serde_json::json!({
        "version": 1,
        "core_version": env!("CARGO_PKG_VERSION"),
        "core_revision": prismattyc_core::git_hash(),
        "catalog_sha256": digest(BUNDLED_INDEX),
        "pin_sha256": BUNDLED_PIN.map(digest),
        "receipt_sha256": BUNDLED_RECEIPT.map(digest),
        "packages": catalog.list(),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub bytes: u64,
}

impl CatalogEntry {
    pub(super) fn validate(&self) -> Result<()> {
        validate_plugin_id(&self.id)?;
        Version::parse(&self.version)?;
        ensure!(valid_digest(&self.sha256), "invalid package digest");
        ensure!(
            self.bytes > 0 && self.bytes <= MAX_PACKAGE_BYTES as u64,
            "invalid package size"
        );
        Ok(())
    }

    pub(super) fn verify(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        ensure!(
            bytes.len() as u64 == self.bytes && digest(bytes) == self.sha256,
            "package size or digest does not match its trusted catalog"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u16,
    packages: Vec<CatalogEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Provenance {
    CoreBundle { core: String, revision: String },
    OfficialRelease { tag: String },
}

impl Provenance {
    pub(super) fn validate(&self) -> Result<()> {
        match self {
            Self::CoreBundle { core, revision } => {
                Version::parse(core)?;
                ensure!(
                    revision.len() <= 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid core package provenance"
                );
            }
            Self::OfficialRelease { tag } => {
                catalog_version(tag).context("invalid official package provenance")?;
            }
        }
        Ok(())
    }
}

enum Source {
    Bundled(&'static [u8]),
    Official(String),
}

pub(super) struct TrustedEntry {
    pub entry: CatalogEntry,
    pub provenance: Provenance,
    source: Source,
}

impl TrustedEntry {
    pub(super) fn download(&self) -> Result<Vec<u8>> {
        match &self.source {
            Source::Bundled(bytes) => Ok(bytes.to_vec()),
            Source::Official(url) => download_bounded(url, self.entry.bytes as usize),
        }
    }
}

pub(super) struct Catalog(BTreeMap<(String, String), TrustedEntry>);

impl Catalog {
    pub(super) fn bundled() -> Result<Self> {
        let mut catalog = Self(BTreeMap::new());
        for entry in parse(BUNDLED_INDEX)? {
            let bytes = BUNDLED_PACKAGES
                .iter()
                .find(|(sha, _)| *sha == entry.sha256)
                .context("core package resource is missing")?
                .1;
            entry.verify(bytes)?;
            catalog.insert(TrustedEntry {
                entry,
                provenance: Provenance::CoreBundle {
                    core: prismattyc_core::package_version().into(),
                    revision: prismattyc_core::git_hash().into(),
                },
                source: Source::Bundled(bytes),
            })?;
        }
        Ok(catalog)
    }

    /// Explicit catalog refresh. Package releases use a separate tag grammar,
    /// so the core updater's semver release selection never selects them.
    pub(super) fn refreshed() -> Result<Self> {
        let releases: Vec<Release> = serde_json::from_slice(&fetch_release_json(&format!(
            "https://api.github.com/repos/{REPOSITORY}/releases?per_page=100"
        ))?)?;
        let release = releases
            .iter()
            .filter(|release| !release.draft && release.immutable && release.prerelease)
            .filter_map(|release| {
                catalog_version(&release.tag_name).map(|version| (version, release))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, release)| release)
            .context("no published immutable official plug-in catalog is available")?;
        let (_, published) = Self::published(release)?;
        let mut catalog = Self::bundled()?;
        for entry in published.0.into_values() {
            catalog.insert(entry)?;
        }
        Ok(catalog)
    }

    pub(super) fn at_tag(tag: &str) -> Result<(Vec<u8>, Self, Release)> {
        catalog_version(tag).context("invalid official catalog tag")?;
        let release: Release = serde_json::from_slice(&fetch_release_json(&format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}"
        ))?)?;
        ensure!(release.tag_name == tag, "official catalog tag mismatch");
        let (bytes, catalog) = Self::published(&release)?;
        Ok((bytes, catalog, release))
    }

    fn published(release: &Release) -> Result<(Vec<u8>, Self)> {
        ensure!(
            !release.draft && release.immutable && release.prerelease,
            "official plug-in catalogs must be published immutable prereleases"
        );
        catalog_version(&release.tag_name).context("invalid official catalog tag")?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == CATALOG_ASSET)
            .context("official plug-in catalog asset is missing")?;
        ensure!(asset.size <= 512 * 1024, "catalog exceeds its byte budget");
        let url = asset_url(&release.tag_name, CATALOG_ASSET);
        ensure!(
            asset.browser_download_url == url,
            "unexpected catalog publication source"
        );
        let bytes = download_bounded(&url, asset.size as usize)?;
        ensure!(
            bytes.len() as u64 == asset.size
                && asset.digest.as_deref() == Some(format!("sha256:{}", digest(&bytes)).as_str()),
            "catalog size or digest mismatch"
        );
        let mut catalog = Self(BTreeMap::new());
        for entry in parse(&bytes)? {
            let name = format!("{}.pmux-plugin", entry.sha256);
            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == name)
                .context("catalog package is missing from its immutable release")?;
            let url = asset_url(&release.tag_name, &name);
            ensure!(
                asset.size == entry.bytes
                    && asset.digest.as_deref() == Some(format!("sha256:{}", entry.sha256).as_str())
                    && asset.browser_download_url == url,
                "catalog package does not match its official release metadata"
            );
            catalog.insert(TrustedEntry {
                entry,
                provenance: Provenance::OfficialRelease {
                    tag: release.tag_name.clone(),
                },
                source: Source::Official(url),
            })?;
        }
        Ok((bytes, catalog))
    }

    pub(super) fn list(&self) -> Vec<CatalogEntry> {
        self.0.values().map(|entry| entry.entry.clone()).collect()
    }

    pub(super) fn get(&self, id: &str, version: Option<&str>) -> Result<&TrustedEntry> {
        self.0.values().filter(|entry| entry.entry.id == id && version.is_none_or(|version| entry.entry.version == version))
            .max_by_key(|entry| Version::parse(&entry.entry.version).expect("validated catalog version"))
            .context("plug-in version is not in the trusted catalog; refresh discovery or update the core package")
    }

    fn insert(&mut self, entry: TrustedEntry) -> Result<()> {
        let key = (entry.entry.id.clone(), entry.entry.version.clone());
        if let Some(old) = self.0.get(&key) {
            ensure!(
                old.entry == entry.entry,
                "published plug-in version changed its immutable bytes"
            );
        } else {
            self.0.insert(key, entry);
        }
        Ok(())
    }
}

pub(super) fn parse(bytes: &[u8]) -> Result<Vec<CatalogEntry>> {
    ensure!(bytes.len() <= 512 * 1024, "catalog exceeds its byte budget");
    let document: Document = serde_json::from_slice(bytes)?;
    ensure!(
        document.version == 1 && document.packages.len() <= 256,
        "unsupported or oversized plug-in catalog"
    );
    let mut seen = std::collections::BTreeSet::new();
    for entry in &document.packages {
        entry.validate()?;
        ensure!(
            seen.insert((&entry.id, &entry.version)),
            "duplicate catalog package version"
        );
    }
    Ok(document.packages)
}

pub(super) fn catalog_version(tag: &str) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix(CATALOG_TAG_PREFIX)?).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

pub(super) fn asset_url(tag: &str, name: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_versions_and_package_authority_are_separate_from_core_versions() {
        assert_eq!(
            catalog_version("pmux-plugins-v1.2.0").unwrap(),
            Version::new(1, 2, 0)
        );
        assert!(catalog_version("v0.3.53").is_none());
        assert!(catalog_version("pmux-plugins-v1.0.0-rc.1").is_none());
        assert!(parse(br#"{"version":1,"packages":[],"executable":"run"}"#).is_err());
        let entry = CatalogEntry {
            id: "org.moonbase.example".into(),
            version: "1.0.0".into(),
            sha256: digest(b"package"),
            bytes: 7,
        };
        entry.verify(b"package").unwrap();
        assert!(entry.verify(b"changed").is_err());
    }
}
