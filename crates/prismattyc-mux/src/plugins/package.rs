// SPDX-License-Identifier: MIT

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::catalog::{CatalogEntry, Provenance, TrustedEntry};
use super::manifest::{valid_path, Contribution, Manifest};
use super::wire::CommandSchema;

pub(super) const MAX_PACKAGE_BYTES: usize = 24 * 1024 * 1024;

/// This format has only regular byte files. It cannot express links, device
/// entries, executable hooks, compression bombs, or filesystem metadata.
#[derive(Serialize, Deserialize)]
struct Archive {
    version: u16,
    files: BTreeMap<String, Vec<u8>>,
}

pub(super) struct Package {
    pub manifest: Manifest,
    pub component: Vec<u8>,
    pub assets: BTreeMap<String, Vec<u8>>,
    pub schemas: BTreeMap<String, CommandSchema>,
}

impl Package {
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_PACKAGE_BYTES,
            "plug-in package exceeds its byte budget"
        );
        let (archive, rest): (Archive, _) = postcard::take_from_bytes(bytes)?;
        ensure!(
            rest.is_empty() && archive.version == 1,
            "unsupported plug-in archive"
        );
        ensure!(
            postcard::to_allocvec(&archive)? == bytes,
            "non-canonical plug-in archive"
        );
        ensure!(archive.files.len() <= 192, "too many package files");
        let mut names = BTreeSet::new();
        for name in archive.files.keys() {
            ensure!(
                valid_path(name) && names.insert(name.to_ascii_lowercase()),
                "invalid or aliased package path"
            );
        }
        let manifest = Manifest::parse(
            archive
                .files
                .get("plugin.toml")
                .context("package has no manifest")?,
        )?;
        ensure!(
            manifest.files() == archive.files.keys().map(String::as_str).collect(),
            "package file list does not match its manifest"
        );
        let component = archive.files[&manifest.component].clone();
        ensure!(
            component.len() <= 16 * 1024 * 1024,
            "component exceeds its byte budget"
        );
        let mut assets = BTreeMap::new();
        let mut total = 0usize;
        for name in &manifest.assets {
            let bytes = &archive.files[name];
            total = total
                .checked_add(bytes.len())
                .context("asset size overflow")?;
            ensure!(
                bytes.len() <= 1024 * 1024 && total <= 4 * 1024 * 1024,
                "package assets exceed their byte budget"
            );
            assets.insert(name.clone(), bytes.clone());
        }
        let mut schemas = BTreeMap::new();
        for contribution in &manifest.contributions {
            if let Contribution::Command {
                id,
                input_schema,
                output_schema,
                ..
            } = contribution
            {
                let schema = |path: &str| -> Result<String> {
                    let bytes = &archive.files[path];
                    ensure!(
                        bytes.len() <= 64 * 1024,
                        "command schema exceeds its byte budget"
                    );
                    Ok(std::str::from_utf8(bytes)?.to_owned())
                };
                schemas.insert(
                    id.clone(),
                    CommandSchema {
                        input: schema(input_schema)?,
                        output: schema(output_schema)?,
                    },
                );
            }
        }
        Ok(Self {
            manifest,
            component,
            assets,
            schemas,
        })
    }
}

/// Build a regular-file archive for the release packager. This grants no
/// installation authority; the daemon accepts only its trusted catalog entries.
pub fn pack(files: BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let bytes = postcard::to_allocvec(&Archive { version: 1, files })?;
    Package::parse(&bytes)?;
    Ok(bytes)
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u16,
    entry: CatalogEntry,
    provenance: Provenance,
}

pub(super) struct Store {
    root: PathBuf,
}

impl Store {
    pub(super) fn new(root: PathBuf) -> Result<Self> {
        ensure!(root.is_absolute(), "plug-in store must be absolute");
        Ok(Self { root })
    }

    pub(super) fn default_root() -> Result<PathBuf> {
        let base = crate::platform::data_home()
            .map(PathBuf::from)
            .or_else(|| {
                crate::platform::home_dir().map(|home| PathBuf::from(home).join(".local/share"))
            })
            .context("cannot locate the user plug-in store")?;
        ensure!(
            base.is_absolute(),
            "plug-in data directory must be absolute"
        );
        Ok(base.join("prismattyc/plugins"))
    }

    pub(super) fn install(&self, executable: &Path, entry: &TrustedEntry) -> Result<Installed> {
        let bytes = entry.download()?;
        entry.entry.verify(&bytes)?;
        let package = Package::parse(&bytes)?;
        package.manifest.compatibility()?;
        ensure!(
            package.manifest.id == entry.entry.id
                && package.manifest.version == entry.entry.version,
            "catalog/package identity mismatch"
        );
        super::worker::validate(executable, package.component, package.schemas)?;
        private_directory(&self.root)?;
        let lock = crate::private_fs::open(
            &self.root.join("store.lock"),
            OpenOptions::new().read(true).write(true).create(true),
        )?;
        crate::platform::try_lock_exclusive(&lock)
            .context("plug-in store is busy; retry installation")?;
        let selected = Installed {
            id: entry.entry.id.clone(),
            version: entry.entry.version.clone(),
            digest: entry.entry.sha256.clone(),
        };
        let objects = self.root.join("objects");
        private_directory(&objects)?;
        let destination = objects.join(&selected.digest);
        if destination.exists() {
            self.load(&selected)?;
            return Ok(selected);
        }
        let staging = objects.join(format!(".stage-{}", nonce()?));
        private_directory(&staging)?;
        let result = (|| -> Result<()> {
            let mut file = crate::private_fs::open(
                &staging.join("package.pmux-plugin"),
                OpenOptions::new().write(true).create_new(true),
            )?;
            use std::io::Write;
            file.write_all(&bytes)?;
            file.sync_all()?;
            let receipt = Receipt {
                version: 1,
                entry: entry.entry.clone(),
                provenance: entry.provenance.clone(),
            };
            crate::config::write_file_atomic(
                &staging.join("receipt.json"),
                &serde_json::to_string(&receipt)?,
            )?;
            fs::rename(&staging, &destination)?;
            // Once renamed, publication succeeded even if directory sync is not
            // supported. Never report a failed operation after committing it.
            #[cfg(unix)]
            if let Err(error) = fs::File::open(&objects).and_then(|file| file.sync_all()) {
                eprintln!("plug-in package committed; directory sync warning: {error}");
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        Ok(selected)
    }

    pub(super) fn load(&self, selected: &Installed) -> Result<Package> {
        ensure!(
            valid_digest(&selected.digest),
            "invalid installed package digest"
        );
        let object = self.root.join("objects").join(&selected.digest);
        require_directory(&self.root)?;
        require_directory(&self.root.join("objects"))?;
        require_directory(&object)?;
        let receipt: Receipt =
            serde_json::from_slice(&read_private(&object.join("receipt.json"), 64 * 1024)?)?;
        ensure!(
            receipt.version == 1
                && receipt.entry.id == selected.id
                && receipt.entry.version == selected.version
                && receipt.entry.sha256 == selected.digest,
            "installed package receipt mismatch"
        );
        receipt.entry.validate()?;
        receipt.provenance.validate()?;
        let bytes = read_private(&object.join("package.pmux-plugin"), MAX_PACKAGE_BYTES)?;
        receipt.entry.verify(&bytes)?;
        let package = Package::parse(&bytes)?;
        ensure!(
            package.manifest.id == selected.id && package.manifest.version == selected.version,
            "installed manifest identity mismatch"
        );
        Ok(package)
    }

    pub(super) fn list(&self) -> Result<Vec<Installed>> {
        let root = self.root.join("objects");
        if !root.try_exists()? {
            return Ok(Vec::new());
        }
        require_directory(&self.root)?;
        require_directory(&root)?;
        let mut installed = Vec::new();
        for (index, object) in fs::read_dir(root)?.enumerate() {
            ensure!(
                index < 512,
                "too many package store entries; remove interrupted staging directories"
            );
            let object = object?;
            let name = object.file_name().to_string_lossy().into_owned();
            if !valid_digest(&name) {
                continue;
            }
            require_directory(&object.path())?;
            let receipt: Receipt = serde_json::from_slice(&read_private(
                &object.path().join("receipt.json"),
                64 * 1024,
            )?)?;
            let selected = Installed {
                id: receipt.entry.id,
                version: receipt.entry.version,
                digest: name,
            };
            self.load(&selected)?;
            installed.push(selected);
            ensure!(
                installed.len() <= 256,
                "too many installed plug-in packages"
            );
        }
        installed
            .sort_by(|a, b| (&a.id, &a.version, &a.digest).cmp(&(&b.id, &b.version, &b.digest)));
        Ok(installed)
    }
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn nonce() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("cannot mint plug-in handle"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    crate::private_fs::create_dir(path)?;
    #[cfg(windows)]
    crate::platform::secure_directory(path)?;
    require_directory(path)
}

fn require_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "plug-in storage directory is not regular"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "plug-in storage directory is a reparse point"
        );
        crate::platform::require_private_directory(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "plug-in directory must be private and owned"
        );
    }
    Ok(())
}

pub(super) fn read_private(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let file = crate::private_fs::open(path, OpenOptions::new().read(true))?;
    ensure!(
        file.metadata()?.len() <= cap as u64,
        "plug-in file exceeds its byte budget"
    );
    file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cap, "plug-in file exceeds its byte budget");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                "plugin.toml".into(),
                br#"manifest_version = 1
id = "org.moonbase.example"
version = "1.0.0"
plugin_api = "^1"
component = "view.wasm"
notices = ["LICENSE"]
[requires]
"pmux:view/frame" = "^1"
[[contributions]]
kind = "view"
id = "main"
title = "Example"
"#
                .to_vec(),
            ),
            ("LICENSE".into(), b"MIT".to_vec()),
            (
                "view.wasm".into(),
                b"invalid component, not instantiated by archive parsing".to_vec(),
            ),
        ])
    }

    #[test]
    fn package_parsing_cannot_expand_links_hidden_files_or_aliased_names() {
        let bytes = pack(files()).unwrap();
        assert_eq!(
            Package::parse(&bytes).unwrap().manifest.id,
            "org.moonbase.example"
        );
        let mut hidden = files();
        hidden.insert("install.sh".into(), b"run native code".to_vec());
        assert!(pack(hidden).is_err());
        let mut aliased = files();
        aliased.insert("VIEW.WASM".into(), Vec::new());
        assert!(pack(aliased).is_err());
        let mut traversal = files();
        traversal.insert("../escape".into(), Vec::new());
        assert!(pack(traversal).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(Package::parse(&trailing).is_err());
    }
}
