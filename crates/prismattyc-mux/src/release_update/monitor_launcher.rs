// SPDX-License-Identifier: MIT
//! Explicit auxiliary repair, independent of the six-member core transaction.

use super::*;
use std::fs::OpenOptions;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
};

const NAME: &str = "pmux-activity-monitor";
const MAX_RECEIPT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    bytes: u64,
    sha256: String,
}

impl Fingerprint {
    fn asset(asset: &Asset) -> Result<Self> {
        Ok(Self {
            bytes: asset.size,
            sha256: asset
                .digest
                .as_deref()
                .and_then(|s| s.strip_prefix("sha256:"))
                .context("launcher asset has no digest")?
                .to_ascii_lowercase(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleIdentity {
    version: u16,
    format: String,
    core_version: String,
    core_revision: String,
    catalog_sha256: String,
    pin_sha256: Option<String>,
    receipt_sha256: Option<String>,
    packages: Vec<crate::plugins::CatalogEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Previous {
    fingerprint: Fingerprint,
    source_release: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuxiliaryReceipt {
    version: u16,
    repository: String,
    bin_dir: PathBuf,
    target: String,
    installed_release: String,
    asset_name: String,
    installed: Fingerprint,
    previous: Option<Previous>,
    bundle: BundleIdentity,
}

struct Installation {
    directory: PathBuf,
    bin_dir: PathBuf,
    version: Version,
    target: &'static str,
}

fn installation(root: &Path, options: &Options) -> Result<Installation> {
    ensure!(
        !cfg!(target_os = "macos"),
        "macOS repairs the monitor through verified application replacement"
    );
    let directory = current_directory(root)?;
    let installed = receipt(&directory).context(
        "repair requires an active managed core release; run the six-binary core update first",
    )?;
    let target = target()?;
    ensure!(
        installed.repository == REPOSITORY && installed.target == target,
        "active core receipt belongs to another repository or target"
    );
    let version = Version::parse(&installed.version)?;
    ensure!(
        version.to_string() == installed.version && version >= Version::new(0, 2, 0),
        "invalid active core release version"
    );
    ensure!(
        installed.bin_dir.is_absolute(),
        "registered bin directory must be absolute"
    );
    let bin_dir = fs::canonicalize(&installed.bin_dir)?;
    if let Some(explicit) = &options.bin_dir {
        ensure!(
            explicit.is_absolute() && fs::canonicalize(explicit)? == bin_dir,
            "repair must use the active receipt's registered installation directory"
        );
    }
    ensure!(
        fs::canonicalize(&directory)?.starts_with(fs::canonicalize(root)?),
        "active release escapes the update store"
    );
    for binary in BINARIES {
        ensure!(
            directory
                .join(crate::platform::executable_name(binary))
                .is_file(),
            "active six-binary core installation is incomplete"
        );
    }
    #[cfg(windows)]
    ensure!(
        fs::canonicalize(windows_update::load(root)?.bin_dir)? == bin_dir,
        "active Windows installation directory differs from its receipt"
    );
    Ok(Installation {
        directory,
        bin_dir,
        version,
        target,
    })
}

fn exact_release(version: &str) -> Result<Release> {
    let canonical = Version::parse(version)?;
    ensure!(
        canonical.to_string() == version,
        "use a canonical release version"
    );
    let tag = format!("v{version}");
    let release: Release = serde_json::from_slice(&fetch_release_json(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}"
    ))?)?;
    ensure!(
        release.tag_name == tag && accepted_version(&release, true)? == canonical,
        "exact installed release metadata differs"
    );
    Ok(release)
}

fn open_regular(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "launcher or receipt must be a regular file, not a symlink"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "reparse-point launcher or receipt refused"
        );
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "launcher or receipt is not a regular file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "reparse-point launcher or receipt refused"
        );
    }
    Ok(file)
}

fn fingerprint(path: &Path) -> Result<Option<Fingerprint>> {
    let mut file = match open_regular(path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(bytes <= MAX_ASSET, "launcher file exceeds its byte budget");
        hash.update(&buffer[..count]);
    }
    Ok(Some(Fingerprint {
        bytes,
        sha256: format!("{:x}", hash.finalize()),
    }))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn contains_identity_marker(path: &Path) -> Result<bool> {
    const MARKER: &[u8] = b"pmux-contained-plugin-consumer-v1";
    let file = open_regular(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_ASSET,
        "executable exceeds its byte budget"
    );
    let mut file = file.take(MAX_ASSET);
    let mut tail = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(false);
        }
        tail.extend_from_slice(&buffer[..count]);
        if tail.windows(MARKER.len()).any(|window| window == MARKER) {
            return Ok(true);
        }
        tail.drain(..tail.len().saturating_sub(MARKER.len() - 1));
    }
}

fn bundle_identity(path: &Path, version: &Version) -> Result<BundleIdentity> {
    ensure!(
        contains_identity_marker(path)?,
        "executable predates contained plug-in identity support; it was not executed"
    );
    let identity: BundleIdentity = serde_json::from_str(&executable_probe(
        path,
        "--plugin-bundle",
        MAX_RECEIPT,
        "bundle identity",
    )?)
    .context("installed core and new launcher must support contained plug-ins")?;
    validate_bundle_identity(&identity, version)?;
    Ok(identity)
}

fn validate_bundle_identity(identity: &BundleIdentity, version: &Version) -> Result<()> {
    let base = Version::new(version.major, version.minor, version.patch).to_string();
    let ids: std::collections::BTreeSet<_> =
        identity.packages.iter().map(|p| p.id.as_str()).collect();
    ensure!(
        identity.version == 1
            && identity.format == "pmux-contained-plugin-consumer-v1"
            && identity.core_version == base
            && identity.core_revision.len() == 12
            && identity
                .core_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            && valid_digest(&identity.catalog_sha256)
            && identity.pin_sha256.as_deref().is_some_and(valid_digest)
            && identity.receipt_sha256.as_deref().is_some_and(valid_digest)
            && ids
                == std::collections::BTreeSet::from([
                    "org.moonbase.activity-monitor",
                    "org.moonbase.activity-connector"
                ]),
        "core has no verified production plug-in bundle"
    );
    Ok(())
}

fn auxiliary_receipt(
    directory: &Path,
    installation: &Installation,
) -> Result<Option<AuxiliaryReceipt>> {
    let path = directory.join("receipt.json");
    if !plain_directory(directory)? {
        return Ok(None);
    }
    let mut file = match open_regular(&path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECEIPT,
        "auxiliary receipt exceeds its byte budget"
    );
    let saved: AuxiliaryReceipt = serde_json::from_slice(&bytes)?;
    ensure!(
        saved.version == 1
            && saved.repository == REPOSITORY
            && saved.target == installation.target
            && saved.bin_dir == installation.bin_dir
            && valid_digest(&saved.installed.sha256)
            && saved.installed.bytes > 0
            && saved.installed.bytes <= MAX_ASSET,
        "auxiliary receipt belongs to another installation or is invalid"
    );
    let version = Version::parse(&saved.installed_release)?;
    ensure!(
        version.to_string() == saved.installed_release
            && saved.asset_name == asset_name(&format!("v{version}"), installation.target, NAME),
        "invalid auxiliary release identity"
    );
    validate_bundle_identity(&saved.bundle, &version)?;
    if let Some(previous) = &saved.previous {
        ensure!(
            valid_digest(&previous.fingerprint.sha256)
                && previous.fingerprint.bytes > 0
                && previous.fingerprint.bytes <= MAX_ASSET,
            "invalid backup identity"
        );
        Version::parse(&previous.source_release)?;
    }
    Ok(Some(saved))
}

fn admit(
    current: &Option<Fingerprint>,
    saved: Option<&AuxiliaryReceipt>,
    hint: Option<&str>,
    target: &str,
) -> Result<Option<Previous>> {
    let Some(current) = current else {
        return Ok(None);
    };
    if let Some(saved) = saved {
        if *current == saved.installed {
            return Ok(Some(Previous {
                fingerprint: current.clone(),
                source_release: saved.installed_release.clone(),
            }));
        }
        if saved
            .previous
            .as_ref()
            .is_some_and(|p| p.fingerprint == *current)
        {
            return Ok(saved.previous.clone());
        }
        bail!("launcher differs from both recorded states; preserve the file and investigate before repair");
    }
    let source = hint.context("existing monitor is unreceipted; use --from-monitor-version VERSION to verify its official asset, or preserve/move it explicitly")?;
    let release = exact_release(source)?;
    let asset = select_asset(&release, target, NAME)?;
    ensure!(*current == Fingerprint::asset(asset)?, "existing monitor does not match the immutable official source asset; it was not executed or replaced");
    Ok(Some(Previous {
        fingerprint: current.clone(),
        source_release: source.into(),
    }))
}

struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn stage(directory: &Path, executable: bool) -> Result<(Stage, File)> {
    let extension = if cfg!(windows) && executable {
        "exe"
    } else {
        "tmp"
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    for attempt in 0..100 {
        let path = directory.join(format!(
            ".pmux-monitor-stage-{}-{stamp}-{attempt}.{extension}",
            std::process::id()
        ));
        match crate::platform::private_options()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((Stage(path), file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("cannot allocate an auxiliary staging file")
}

fn plain_directory(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "auxiliary directory must be a plain directory"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "auxiliary directory cannot be a reparse point"
        );
    }
    Ok(true)
}

fn private_directory(path: &Path) -> Result<()> {
    plain_directory(path)?;
    crate::private_fs::create_dir(path)?;
    ensure!(
        fs::symlink_metadata(path)?.is_dir()
            && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "auxiliary directory cannot be a symlink"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "auxiliary directory cannot be a reparse point"
        );
        crate::platform::secure_directory(path)?;
    }
    Ok(())
}

fn durable_record(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("receipt parent")?;
    let (temporary, mut file) = stage(parent, false)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    crate::platform::replace_file(&temporary.0, path)?;
    crate::platform::sync_directory(parent)?;
    Ok(())
}

fn backup(directory: &Path, destination: &Path, previous: &Previous) -> Result<()> {
    let path = directory.join(format!("backup-{}", previous.fingerprint.sha256));
    if let Some(actual) = fingerprint(&path)? {
        ensure!(
            actual == previous.fingerprint,
            "recorded backup has other bytes"
        );
        return Ok(());
    }
    let (temporary, mut output) = stage(directory, false)?;
    std::io::copy(
        &mut open_regular(destination)?.take(MAX_ASSET + 1),
        &mut output,
    )?;
    output.sync_all()?;
    drop(output);
    ensure!(
        fingerprint(&temporary.0)? == Some(previous.fingerprint.clone()),
        "launcher changed while preserving its backup"
    );
    fs::hard_link(&temporary.0, &path)?;
    crate::platform::sync_directory(directory)?;
    Ok(())
}

fn prefix_lock(bin_dir: &Path) -> Result<File> {
    let prefix = bin_dir.parent().context("registered installation prefix")?;
    let path = prefix.join(".prismattyc-install.lock");
    let mut options = crate::platform::private_options();
    options.read(true).write(true).create(true).truncate(false);
    let file = crate::private_fs::open(&path, &options)?;
    crate::platform::try_lock_exclusive(&file).context("another installation is running")?;
    Ok(file)
}

fn report(
    status: &str,
    installation: &Installation,
    destination: &Path,
    desired: &Fingerprint,
    json: bool,
    warning: Option<String>,
) {
    let result = serde_json::json!({"status":status,"binary":NAME,"version":installation.version.to_string(),
        "destination":destination,"sha256":desired.sha256,"warning":warning,"restart_required":false});
    if json {
        println!("{result}");
    } else {
        println!("Monitor launcher: {status} for installed core {}. Core rollback manages its six executables separately.", installation.version);
        if let Some(warning) = warning {
            eprintln!("Launcher replacement committed; directory sync warning: {warning}");
        }
    }
}

pub(super) fn run(root: &Path, options: &Options) -> Result<()> {
    let installation = installation(root, options)?;
    let _prefix_lock = prefix_lock(&installation.bin_dir)?;
    let destination = installation
        .bin_dir
        .join(crate::platform::executable_name(NAME));
    let directory = root.join("monitor-launcher");
    let current = fingerprint(&destination)?;
    let saved = auxiliary_receipt(&directory, &installation)?;
    let previous = admit(
        &current,
        saved.as_ref(),
        options.from_monitor_version.as_deref(),
        installation.target,
    )?;
    let release = exact_release(&installation.version.to_string())?;
    let asset = select_asset(&release, installation.target, NAME)?;
    let desired = Fingerprint::asset(asset)?;
    let core = bundle_identity(
        &installation
            .directory
            .join(crate::platform::executable_name("pmux")),
        &installation.version,
    )?;
    if saved.as_ref().is_some_and(|s| s.installed == desired) && current.as_ref() == Some(&desired)
    {
        ensure!(
            saved.as_ref().is_some_and(|saved| saved.bundle == core),
            "installed launcher bundle differs from active core"
        );
        ensure!(
            fingerprint(&destination)? == current,
            "launcher changed during verification"
        );
        report(
            "current",
            &installation,
            &destination,
            &desired,
            options.json,
            None,
        );
        return Ok(());
    }
    let stage_dir = if options.check {
        std::env::temp_dir()
    } else {
        installation.bin_dir.clone()
    };
    let (incoming, file) = stage(&stage_dir, true)?;
    drop(file);
    download_one_binary(&incoming.0, NAME, asset)?;
    ensure!(
        bundle_identity(&incoming.0, &installation.version)? == core,
        "verified launcher does not match the active core's contained plug-in bundle"
    );
    let label = version_label(&incoming.0)?;
    ensure!(
        label.starts_with("pmux-activity-monitor "),
        "verified asset is not the monitor frontend"
    );
    let base = Version::new(
        installation.version.major,
        installation.version.minor,
        installation.version.patch,
    )
    .to_string();
    ensure!(
        label
            .split_whitespace()
            .any(|v| v == installation.version.to_string() || v == base),
        "new launcher reports another core version"
    );

    ensure!(
        fingerprint(&destination)? == current,
        "launcher changed during verification"
    );
    if options.check {
        report(
            "would_repair",
            &installation,
            &destination,
            &desired,
            options.json,
            None,
        );
        return Ok(());
    }
    private_directory(&directory)?;
    crate::platform::sync_directory(root)?;
    if let Some(previous) = &previous {
        backup(&directory, &destination, previous)?;
    }
    if let Some(saved) = saved {
        let bytes = serde_json::to_vec_pretty(&saved)?;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        durable_record(&directory.join(format!("history-{hash}.json")), &bytes)?;
    }
    let planned = AuxiliaryReceipt {
        version: 1,
        repository: REPOSITORY.into(),
        bin_dir: installation.bin_dir.clone(),
        target: installation.target.into(),
        installed_release: installation.version.to_string(),
        asset_name: asset.name.clone(),
        installed: desired.clone(),
        previous,
        bundle: core,
    };
    durable_record(
        &directory.join("receipt.json"),
        &serde_json::to_vec_pretty(&planned)?,
    )?;
    ensure!(
        fingerprint(&destination)? == current,
        "launcher changed before atomic replacement"
    );
    crate::platform::replace_file(&incoming.0, &destination)?;
    let warning = crate::platform::sync_directory(&installation.bin_dir)
        .err()
        .map(|e| e.to_string());
    let status = if warning.is_some() {
        "committed_with_warning"
    } else {
        "repaired"
    };
    report(
        status,
        &installation,
        &destination,
        &desired,
        options.json,
        warning,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "pmux-monitor-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn old() -> Fingerprint {
        Fingerprint {
            bytes: 3,
            sha256: "cba06b5736faf67e54b07b561eae94395e774c517a7d910a54369e1263ccfbd4".into(),
        }
    }
    fn new() -> Fingerprint {
        Fingerprint {
            bytes: 3,
            sha256: "11507a0e2f5e69d5dfa40a62a1bd7b6ee57e6bcd85c67c9b8431b36fff21c437".into(),
        }
    }
    fn saved(bin_dir: PathBuf) -> AuxiliaryReceipt {
        AuxiliaryReceipt {
            version: 1,
            repository: REPOSITORY.into(),
            bin_dir,
            target: "x86_64-unknown-linux-gnu".into(),
            installed_release: "0.3.53".into(),
            asset_name: "prismattyc-v0.3.53-x86_64-unknown-linux-gnu-pmux-activity-monitor".into(),
            installed: new(),
            previous: Some(Previous {
                fingerprint: old(),
                source_release: "0.3.52".into(),
            }),
            bundle: BundleIdentity {
                version: 1,
                format: "pmux-contained-plugin-consumer-v1".into(),
                core_version: "0.3.53".into(),
                core_revision: "111111111111".into(),
                catalog_sha256: "2".repeat(64),
                pin_sha256: Some("3".repeat(64)),
                receipt_sha256: Some("4".repeat(64)),
                packages: vec![
                    crate::plugins::CatalogEntry {
                        id: "org.moonbase.activity-monitor".into(),
                        version: "1.0.0".into(),
                        sha256: "5".repeat(64),
                        bytes: 1,
                    },
                    crate::plugins::CatalogEntry {
                        id: "org.moonbase.activity-connector".into(),
                        version: "1.0.0".into(),
                        sha256: "6".repeat(64),
                        bytes: 1,
                    },
                ],
            },
        }
    }

    #[test]
    fn executable_identity_marker_can_cross_a_read_boundary() {
        let scratch = Scratch::new();
        let path = scratch.0.join("binary");
        let mut bytes = vec![b'x'; 65530];
        bytes.extend_from_slice(b"pmux-contained-plugin-consumer-v1");
        fs::write(&path, bytes).unwrap();
        assert!(contains_identity_marker(&path).unwrap());
        fs::write(&path, b"an older native monitor").unwrap();
        assert!(!contains_identity_marker(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn an_older_executable_is_rejected_before_any_probe_runs() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let program = scratch.0.join("old-monitor");
        let marker = scratch.0.join("executed");
        fs::write(
            &program,
            format!("#!/bin/sh\nprintf called > '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let error = bundle_identity(&program, &Version::new(0, 3, 53)).unwrap_err();
        assert!(error.to_string().contains("was not executed"));
        assert!(!marker.exists());
    }

    #[test]
    fn interrupted_receipt_admits_only_its_old_or_new_bytes() {
        let saved = saved(PathBuf::from("/fixture/bin"));
        let before = admit(&Some(old()), Some(&saved), None, "x86_64-unknown-linux-gnu")
            .unwrap()
            .unwrap();
        assert_eq!(before.fingerprint, old());
        assert_eq!(before.source_release, "0.3.52");
        let after = admit(&Some(new()), Some(&saved), None, "x86_64-unknown-linux-gnu")
            .unwrap()
            .unwrap();
        assert_eq!(after.fingerprint, new());
        assert_eq!(after.source_release, "0.3.53");
        let foreign = Fingerprint {
            bytes: 5,
            sha256: "d9298a10d1b0735837dc4bd85dac641b0f3cef27a47e5d53a54f2f3f5b2fcffa".into(),
        };
        assert!(admit(
            &Some(foreign),
            Some(&saved),
            None,
            "x86_64-unknown-linux-gnu"
        )
        .is_err());
        assert!(admit(&Some(old()), None, None, "x86_64-unknown-linux-gnu").is_err());
        assert!(admit(&None, Some(&saved), None, "x86_64-unknown-linux-gnu")
            .unwrap()
            .is_none());
    }

    #[test]
    fn durable_receipt_is_bound_to_its_registered_installation() {
        let scratch = Scratch::new();
        let auxiliary = scratch.0.join("aux");
        private_directory(&auxiliary).unwrap();
        let bin_dir = scratch.0.join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let saved = saved(bin_dir.clone());
        durable_record(
            &auxiliary.join("receipt.json"),
            &serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        let mut installation = Installation {
            directory: scratch.0.clone(),
            bin_dir,
            version: Version::new(0, 3, 53),
            target: "x86_64-unknown-linux-gnu",
        };
        let actual = auxiliary_receipt(&auxiliary, &installation)
            .unwrap()
            .unwrap();
        assert_eq!(actual.installed, new());
        assert_eq!(actual.previous.unwrap().fingerprint, old());
        installation.bin_dir = scratch.0.join("unrelated-bin");
        assert!(auxiliary_receipt(&auxiliary, &installation).is_err());
        assert_eq!(fs::read_dir(&auxiliary).unwrap().count(), 1);
    }

    #[test]
    fn partial_download_stage_is_removed_without_touching_neighbor() {
        let scratch = Scratch::new();
        let neighbor = scratch.0.join("keep");
        fs::write(&neighbor, b"untouched").unwrap();
        let path = {
            let (temporary, mut file) = stage(&scratch.0, false).unwrap();
            file.write_all(b"partial").unwrap();
            temporary.0.clone()
        };
        assert!(!path.exists());
        assert_eq!(fs::read(neighbor).unwrap(), b"untouched");
    }

    #[test]
    fn backup_preserves_verified_bytes_and_refuses_an_unknown_backup() {
        let scratch = Scratch::new();
        let launcher = scratch.0.join("monitor");
        fs::write(&launcher, b"old").unwrap();
        let previous = Previous {
            fingerprint: old(),
            source_release: "0.3.52".into(),
        };
        backup(&scratch.0, &launcher, &previous).unwrap();
        let path = scratch
            .0
            .join("backup-cba06b5736faf67e54b07b561eae94395e774c517a7d910a54369e1263ccfbd4");
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(fingerprint(&launcher).unwrap(), Some(old()));
        fs::write(&path, b"foreign").unwrap();
        assert!(backup(&scratch.0, &launcher, &previous).is_err());
        assert_eq!(fs::read(path).unwrap(), b"foreign");
        assert_eq!(fs::read(launcher).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn inspection_does_not_chmod_executables_or_follow_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let scratch = Scratch::new();
        let launcher = scratch.0.join("monitor");
        fs::write(&launcher, b"old").unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(fingerprint(&launcher).unwrap(), Some(old()));
        assert_eq!(
            fs::metadata(&launcher).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let link = scratch.0.join("alias");
        symlink(&launcher, &link).unwrap();
        assert!(fingerprint(&link).is_err());
        let directory_link = scratch.0.join("aux-alias");
        symlink(&scratch.0, &directory_link).unwrap();
        assert!(plain_directory(&directory_link).is_err());
    }

    #[test]
    fn repair_flags_reject_source_channel_and_core_rollback_combinations() {
        for args in [
            vec!["--repair-monitor-launcher", "--rollback"],
            vec!["--repair-monitor-launcher", "--pre"],
            vec!["--from-monitor-version", "0.3.52"],
            vec![
                "--repair-monitor-launcher",
                "--from-monitor-version",
                "../../foreign",
            ],
        ] {
            assert!(options(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
        assert!(crate::run_update(["--source", "--repair-monitor-launcher"]).is_err());
        let selected =
            options(&["--repair-monitor-launcher", "--check", "--json"].map(String::from)).unwrap();
        assert!(selected.repair_monitor && selected.check && selected.json);
    }
}
