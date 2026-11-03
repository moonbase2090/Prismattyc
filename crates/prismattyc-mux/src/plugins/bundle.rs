// SPDX-License-Identifier: MIT
//! Build-time verification of one target-neutral bundle. This never installs
//! packages, executes guests, or grants runtime permissions.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use super::catalog::{self, Catalog, CatalogEntry};
use super::package::{digest, Package, MAX_PACKAGE_BYTES};

const INDEX: &str = "pmux-plugins.json";
const PIN: &str = "bundle-pin.json";
const RECEIPT: &str = "plugin-build-receipt.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageBuild {
    pub entry: CatalogEntry,
    pub source_commit: String,
    pub source_sha256: String,
    pub cargo_lock_sha256: String,
    pub rust_toolchain: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildReceipt {
    pub version: u16,
    pub source_commit: String,
    pub packages: Vec<PackageBuild>,
}

fn valid_commit(commit: &str) -> bool {
    commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn receipt(bytes: &[u8], entries: &[CatalogEntry]) -> Result<BuildReceipt> {
    ensure!(
        bytes.len() <= 512 * 1024,
        "build receipt exceeds its byte budget"
    );
    let receipt: BuildReceipt = serde_json::from_slice(bytes)?;
    ensure!(
        receipt.version == 1
            && valid_commit(&receipt.source_commit)
            && receipt.packages.len() == entries.len(),
        "invalid build receipt"
    );
    let mut seen = std::collections::BTreeSet::new();
    for package in &receipt.packages {
        ensure!(
            entries.contains(&package.entry)
                && seen.insert((&package.entry.id, &package.entry.version))
                && valid_commit(&package.source_commit)
                && super::package::valid_digest(&package.source_sha256)
                && super::package::valid_digest(&package.cargo_lock_sha256)
                && !package.rust_toolchain.is_empty()
                && package.rust_toolchain.len() <= 64
                && package
                    .rust_toolchain
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)),
            "build receipt does not describe the exact catalog"
        );
    }
    Ok(receipt)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BundleSource {
    Official { tag: String },
    Candidate { source_commit: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundlePin {
    pub version: u16,
    pub source: BundleSource,
    pub catalog_sha256: String,
    pub catalog_bytes: u64,
    pub receipt_sha256: String,
    pub receipt_bytes: u64,
}

impl BundlePin {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 16 * 1024,
            "bundle pin exceeds its byte budget"
        );
        let pin: Self = serde_json::from_slice(bytes)?;
        ensure!(
            pin.version == 1
                && super::package::valid_digest(&pin.catalog_sha256)
                && pin.catalog_bytes > 0
                && pin.catalog_bytes <= 512 * 1024
                && super::package::valid_digest(&pin.receipt_sha256)
                && pin.receipt_bytes > 0
                && pin.receipt_bytes <= 512 * 1024,
            "invalid bundle pin"
        );
        match &pin.source {
            BundleSource::Official { tag } => {
                catalog::catalog_version(tag).context("invalid official catalog tag")?;
            }
            BundleSource::Candidate { source_commit } => ensure!(
                valid_commit(source_commit),
                "candidate bundle requires an exact source commit"
            ),
        }
        Ok(pin)
    }
}

fn read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= cap as u64,
        "bundle input is not a bounded regular file"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= cap,
        "bundle input grew beyond its byte budget"
    );
    Ok(bytes)
}

fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        ensure!(
            read(path, bytes.len())? == bytes,
            "bundle destination contains different bytes"
        );
        return Ok(());
    }
    let temporary = path.with_file_name(format!(".bundle-{}.part", super::package::nonce()?));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // A hard link publishes complete bytes without replacing an existing
        // destination. Another writer can only converge on the exact same file.
        match fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    read(path, bytes.len())? == bytes,
                    "bundle destination contains different bytes"
                );
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn verify_contents(directory: &Path, index: &[u8]) -> Result<Vec<CatalogEntry>> {
    let entries = catalog::parse(index)?;
    ensure!(
        !entries.is_empty(),
        "a distributed bundle must contain packages"
    );
    let runtime = pmux_plugin_runtime::Runtime::new()?;
    for entry in &entries {
        let bytes = read(
            &directory.join(format!("{}.pmux-plugin", entry.sha256)),
            MAX_PACKAGE_BYTES,
        )?;
        entry.verify(&bytes)?;
        let package = Package::parse(&bytes)?;
        ensure!(
            package.manifest.id == entry.id && package.manifest.version == entry.version,
            "bundle archive identity differs from its catalog"
        );
        // A retained older API version remains inspectable after a core update.
        // Only compatible components can be statically linked by this runtime.
        if package.manifest.compatibility().is_ok() {
            runtime.validate_component(&package.component)?;
        }
    }
    Ok(entries)
}

/// Require the exact reviewed pin and archive bytes before any native build.
/// Candidate mode is explicit and is never sufficient for publication.
pub fn verify_bundle(directory: &Path, allow_candidate: bool) -> Result<BundlePin> {
    let pin = BundlePin::parse(&read(&directory.join(PIN), 16 * 1024)?)?;
    ensure!(allow_candidate || matches!(pin.source, BundleSource::Official { .. }),
        "candidate bundles require explicit candidate mode; an official pin is required for release");
    let index = read(&directory.join(INDEX), 512 * 1024)?;
    ensure!(
        index.len() as u64 == pin.catalog_bytes && digest(&index) == pin.catalog_sha256,
        "bundle catalog differs from its reviewed pin"
    );
    let entries = verify_contents(directory, &index)?;
    let bytes = read(&directory.join(RECEIPT), 512 * 1024)?;
    ensure!(
        bytes.len() as u64 == pin.receipt_bytes && digest(&bytes) == pin.receipt_sha256,
        "build receipt differs from its reviewed pin"
    );
    let receipt = receipt(&bytes, &entries)?;
    if let BundleSource::Candidate { source_commit } = &pin.source {
        ensure!(
            *source_commit == receipt.source_commit,
            "candidate receipt has another source commit"
        );
    }
    Ok(pin)
}

/// Mark an unpublished source build for private proof and CI only.
pub fn pin_candidate_bundle(directory: &Path, source_commit: String) -> Result<BundlePin> {
    let index = read(&directory.join(INDEX), 512 * 1024)?;
    let entries = verify_contents(directory, &index)?;
    let bytes = read(&directory.join(RECEIPT), 512 * 1024)?;
    ensure!(
        receipt(&bytes, &entries)?.source_commit == source_commit,
        "candidate receipt has another source commit"
    );
    let pin = BundlePin {
        version: 1,
        source: BundleSource::Candidate { source_commit },
        catalog_sha256: digest(&index),
        catalog_bytes: index.len() as u64,
        receipt_sha256: digest(&bytes),
        receipt_bytes: bytes.len() as u64,
    };
    BundlePin::parse(&serde_json::to_vec(&pin)?)?;
    write_pin(directory, &pin)?;
    Ok(pin)
}

fn write_pin(directory: &Path, pin: &BundlePin) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(pin)?;
    let path = directory.join(PIN);
    if path.exists() {
        ensure!(
            BundlePin::parse(&read(&path, 16 * 1024)?)? == *pin,
            "bundle directory already has a different pin"
        );
    } else {
        publish(&path, &bytes)?;
    }
    Ok(())
}

/// Download only an existing immutable official catalog. An expected pin binds
/// normal core builds to reviewed bytes rather than whichever catalog is newest.
pub fn fetch_bundle(
    tag: &str,
    directory: &Path,
    expected: Option<&BundlePin>,
) -> Result<BundlePin> {
    let (index, catalog, release) = Catalog::at_tag(tag)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == RECEIPT)
        .context("official build receipt is missing")?;
    let url = catalog::asset_url(tag, RECEIPT);
    ensure!(
        asset.size > 0 && asset.size <= 512 * 1024 && asset.browser_download_url == url,
        "unexpected official build receipt publication source"
    );
    let bytes = crate::release_update::download_bounded(&url, asset.size as usize)?;
    ensure!(
        bytes.len() as u64 == asset.size
            && asset.digest.as_deref() == Some(format!("sha256:{}", digest(&bytes)).as_str()),
        "official build receipt size or digest mismatch"
    );
    receipt(&bytes, &catalog.list())?;
    let pin = BundlePin {
        version: 1,
        source: BundleSource::Official { tag: tag.into() },
        catalog_sha256: digest(&index),
        catalog_bytes: index.len() as u64,
        receipt_sha256: digest(&bytes),
        receipt_bytes: bytes.len() as u64,
    };
    if let Some(expected) = expected {
        ensure!(
            *expected == pin,
            "official catalog differs from its reviewed pin"
        );
    }
    // A failed fetch may leave verified archives. Re-running reuses only exact
    // bytes; the pin is written last, after the complete bundle is verified.
    fs::create_dir_all(directory)?;
    for entry in catalog.list() {
        let path = directory.join(format!("{}.pmux-plugin", entry.sha256));
        if path.exists() {
            entry.verify(&read(&path, MAX_PACKAGE_BYTES)?)?;
        } else {
            let bytes = catalog.get(&entry.id, Some(&entry.version))?.download()?;
            entry.verify(&bytes)?;
            publish(&path, &bytes)?;
        }
    }
    verify_contents(directory, &index)?;
    publish(&directory.join(RECEIPT), &bytes)?;
    let index_path = directory.join(INDEX);
    if index_path.exists() {
        ensure!(
            read(&index_path, 512 * 1024)? == index,
            "bundle directory contains a different catalog"
        );
    } else {
        publish(&index_path, &index)?;
    }
    write_pin(directory, &pin)?;
    Ok(pin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Fixture, CatalogEntry) {
        let directory = std::env::temp_dir().join(format!(
            "pmux-bundle-{}",
            super::super::package::nonce().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmux-plugin-fixture/package");
        let manifest =
            super::super::Manifest::parse(&fs::read(source.join("plugin.toml")).unwrap()).unwrap();
        let mut files = BTreeMap::new();
        for name in manifest.files() {
            files.insert(
                name.into(),
                if name == manifest.component {
                    pmux_plugin_fixture::component().to_vec()
                } else {
                    fs::read(source.join(name)).unwrap()
                },
            );
        }
        let bytes = super::super::package::pack(files).unwrap();
        let entry = CatalogEntry {
            id: manifest.id,
            version: manifest.version,
            sha256: digest(&bytes),
            bytes: bytes.len() as u64,
        };
        fs::write(
            directory.join(format!("{}.pmux-plugin", entry.sha256)),
            bytes,
        )
        .unwrap();
        fs::write(
            directory.join(INDEX),
            serde_json::to_vec(&serde_json::json!({"version":1,"packages":[entry]})).unwrap(),
        )
        .unwrap();
        let receipt = BuildReceipt {
            version: 1,
            source_commit: "a".repeat(40),
            packages: vec![PackageBuild {
                entry: entry.clone(),
                source_commit: "a".repeat(40),
                source_sha256: "b".repeat(64),
                cargo_lock_sha256: "c".repeat(64),
                rust_toolchain: "1.90.0".into(),
            }],
        };
        fs::write(
            directory.join(RECEIPT),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        (Fixture(directory), entry)
    }

    #[test]
    fn candidate_verification_binds_real_component_and_source_receipt_without_execution() {
        let (directory, entry) = fixture();
        let pin = pin_candidate_bundle(&directory.0, "a".repeat(40)).unwrap();
        assert!(verify_bundle(&directory.0, false).is_err());
        assert_eq!(verify_bundle(&directory.0, true).unwrap(), pin);
        assert_eq!(
            pin_candidate_bundle(&directory.0, "a".repeat(40)).unwrap(),
            pin
        );
        assert!(pin_candidate_bundle(&directory.0, "d".repeat(40)).is_err());
        let receipt_path = directory.0.join(RECEIPT);
        let original = fs::read(&receipt_path).unwrap();
        let mut changed = original.clone();
        changed.push(b' ');
        fs::write(&receipt_path, changed).unwrap();
        assert!(verify_bundle(&directory.0, true).is_err());
        fs::write(receipt_path, original).unwrap();
        let path = directory.0.join(format!("{}.pmux-plugin", entry.sha256));
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        fs::write(path, bytes).unwrap();
        assert!(verify_bundle(&directory.0, true).is_err());
    }

    #[test]
    fn archive_identity_must_match_the_pinned_catalog() {
        let (directory, mut entry) = fixture();
        entry.id = "org.moonbase.different".into();
        let index =
            serde_json::to_vec(&serde_json::json!({"version":1,"packages":[entry]})).unwrap();
        assert!(verify_contents(&directory.0, &index)
            .unwrap_err()
            .to_string()
            .contains("archive identity"));
    }

    #[test]
    fn atomic_publication_reuses_exact_bytes_and_cannot_replace_another_artifact() {
        let directory = Fixture(std::env::temp_dir().join(format!(
            "pmux-publish-{}",
            super::super::package::nonce().unwrap()
        )));
        fs::create_dir(&directory.0).unwrap();
        let path = directory.0.join("artifact");
        publish(&path, b"reviewed bytes").unwrap();
        publish(&path, b"reviewed bytes").unwrap();
        assert!(publish(&path, b"changed bytes").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"reviewed bytes");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
}
