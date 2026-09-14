//! Immutable GitHub releases, verified downloads, and atomic version activation.
//! No git checkout or relationship to the pre-0.2 repository is required.
use anyhow::{bail, ensure, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const REPOSITORY: &str = "moonbase2090/Prismattyc";
pub const BINARIES: [&str; 6] = [
    "pmux",
    "pmuxd",
    "pmux-attach",
    "pmux-mcp",
    "prismattyc",
    "prismattyc-host",
];
const MAX_ASSET: u64 = 256 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(default)]
    pub immutable: bool,
    pub assets: Vec<Asset>,
}
#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub digest: Option<String>,
    pub browser_download_url: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    repository: String,
    version: String,
    target: String,
    bin_dir: PathBuf,
}
#[derive(Default)]
struct Options {
    check: bool,
    json: bool,
    rollback: bool,
    bin_dir: Option<PathBuf>,
}

fn options(args: &[String]) -> Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => options.check = true,
            "--json" => options.json = true,
            "--rollback" => options.rollback = true,
            "--bin-dir" => options.bin_dir = Some(PathBuf::from(args.next().context("--bin-dir needs a directory")?)),
            "--all" => {},
            other => bail!("unknown release update option {other:?}; use --help (development builds: --source)"),
        }
    }
    ensure!(
        !(options.check && options.rollback),
        "--check and --rollback cannot be combined"
    );
    Ok(options)
}

pub fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => bail!("no release target for this platform"),
    }
}

pub fn asset_name(tag: &str, target: &str, binary: &str) -> String {
    format!("prismattyc-{tag}-{target}-{binary}")
}

fn release_version(release: &Release) -> Result<Version> {
    let raw = release
        .tag_name
        .strip_prefix('v')
        .context("release tag must start with v")?;
    let version = Version::parse(raw)?;
    ensure!(
        version >= Version::new(0, 2, 0),
        "the release channel starts at 0.2.0"
    );
    ensure!(
        version.pre.is_empty() && version.build.is_empty() && !release.draft && !release.prerelease,
        "only stable published releases are accepted"
    );
    ensure!(
        release.immutable,
        "release must be immutable before Update can install it"
    );
    Ok(version)
}

fn select_asset<'a>(release: &'a Release, target: &str, binary: &str) -> Result<&'a Asset> {
    let name = asset_name(&release.tag_name, target, binary);
    let matching: Vec<_> = release.assets.iter().filter(|a| a.name == name).collect();
    ensure!(
        matching.len() == 1,
        "release needs exactly one {name} asset"
    );
    let asset = matching[0];
    ensure!(
        asset.size > 0 && asset.size <= MAX_ASSET,
        "invalid size for {name}"
    );
    let digest = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .context("release asset has no SHA-256 digest")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256 digest"
    );
    let expected = format!(
        "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
        release.tag_name
    );
    ensure!(
        asset.browser_download_url == expected,
        "unexpected release asset origin"
    );
    Ok(asset)
}

fn curl() -> Command {
    let mut cmd = Command::new("curl");
    cmd.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "15",
        "--max-time",
        "300",
        "--retry",
        "2",
        "--user-agent",
        "Prismattyc-Updater",
    ]);
    cmd
}

fn latest_release() -> Result<Release> {
    let mut child = curl()
        .args([
            "--header",
            "Accept: application/vnd.github+json",
            "--max-filesize",
            "4194304",
        ])
        .arg(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/latest"
        ))
        .stdout(Stdio::piped())
        .spawn()
        .context("start HTTPS download (curl is required)")?;
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .context("download stdout")?
        .take(4_194_305)
        .read_to_end(&mut bytes);
    if result.is_err() || bytes.len() > 4_194_304 {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    ensure!(
        status.success() && bytes.len() <= 4_194_304,
        "no usable release from {REPOSITORY}; releases start at 0.2.0. Nothing was installed"
    );
    serde_json::from_slice(&bytes).context("parse release metadata")
}

fn verify(path: &Path, asset: &Asset) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == asset.size,
        "download size mismatch for {}",
        asset.name
    );
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let actual = format!("sha256:{:x}", hash.finalize());
    ensure!(
        asset
            .digest
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(&actual)),
        "download digest mismatch for {}",
        asset.name
    );
    Ok(())
}

/// Bound both runtime and output, including descendants that inherit stdout.
pub fn version_label(executable: &Path) -> Result<String> {
    let mut child = Command::new(executable)
        .arg("--version")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().context("version stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .by_ref()
            .take(4097)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut data = None;
    let result = (|| -> Result<String> {
        loop {
            if data.is_none() {
                if let Ok(value) = rx.try_recv() {
                    data = Some(value?);
                }
            }
            ensure!(
                data.as_ref().is_none_or(|b| b.len() <= 4096),
                "version output exceeds limit"
            );
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "version check failed");
                if let Some(bytes) = data.take() {
                    return Ok(String::from_utf8(bytes)?.trim().to_owned());
                }
            }
            ensure!(Instant::now() < deadline, "version check timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // This process group belongs only to this probe, never a user's terminal.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn root() -> Result<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    ensure!(data.is_absolute(), "update data directory must be absolute");
    Ok(data.join("prismattyc/updates"))
}

fn lock(root: &Path) -> Result<File> {
    fs::create_dir_all(root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("update.lock"))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("another update or rollback is running")?;
    Ok(file)
}

fn atomic_link(target: &Path, link: &Path) -> Result<()> {
    let temporary = link.with_extension(format!("{}.tmp", std::process::id()));
    // Our lock owns these names. Remove an interrupted attempt before retrying.
    if fs::symlink_metadata(&temporary).is_ok() {
        fs::remove_file(&temporary)?;
    }
    symlink(target, &temporary)?;
    fs::rename(&temporary, link)?;
    File::open(link.parent().context("link parent")?)?.sync_all()?;
    Ok(())
}

fn receipt(directory: &Path) -> Result<Receipt> {
    serde_json::from_slice(&fs::read(directory.join("receipt.json"))?)
        .context("read installed release receipt")
}

fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("receipt.json"))?;
    file.write_all(&serde_json::to_vec_pretty(receipt)?)?;
    file.sync_all()?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

/// Resolve the active release, without depending on the old executable's path.
pub fn installed_binary(binary: &str) -> Option<PathBuf> {
    if !BINARIES.contains(&binary) {
        return None;
    }
    let root = root().ok()?;
    let path = root.join("current").join(binary);
    path.is_file().then_some(path)
}

fn default_bin_dir(root: &Path) -> Result<PathBuf> {
    if root.join("current/receipt.json").exists() {
        return Ok(receipt(&root.join("current"))?.bin_dir);
    }
    if root.join("installation.json").exists() {
        return Ok(serde_json::from_slice(&fs::read(
            root.join("installation.json"),
        )?)?);
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("executable directory")?;
    ensure!(
        !dir.ends_with("debug") && !dir.ends_with("release"),
        "this is a build-tree binary; select an installation with --bin-dir PATH"
    );
    Ok(dir.to_path_buf())
}

/// Convert a legacy install to indirection while keeping all old binaries available.
/// Every launcher initially resolves to the old version; one current-link rename
/// then activates all six new binaries. Interrupted migrations can be resumed.
fn prepare_launchers(root: &Path, bin_dir: &Path) -> Result<()> {
    ensure!(bin_dir.is_absolute(), "--bin-dir must be absolute");
    fs::create_dir_all(bin_dir)?;
    let installation = root.join("installation.json");
    if installation.exists() {
        let original: PathBuf = serde_json::from_slice(&fs::read(&installation)?)?;
        ensure!(
            original == bin_dir,
            "this update store belongs to {}; use its installation directory",
            original.display()
        );
    } else {
        // Preflight before replacing any launchers. Partial legacy installs need
        // repair first so rollback always restores a complete usable set.
        for binary in BINARIES {
            ensure!(
                bin_dir.join(binary).is_file(),
                "existing installation is missing {binary}"
            );
        }
        let temporary = root.join("installation.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(bin_dir)?)?;
        file.sync_all()?;
        fs::rename(temporary, &installation)?;
        File::open(root)?.sync_all()?;
    }
    let legacy = root.join("legacy");
    if !root.join("current").exists() {
        fs::create_dir_all(&legacy)?;
        for binary in BINARIES {
            let source = bin_dir.join(binary);
            if source.exists() && !legacy.join(binary).exists() {
                let temporary = legacy.join(format!("{binary}.tmp"));
                fs::copy(&source, &temporary)?;
                File::open(&temporary)?.sync_all()?;
                fs::rename(temporary, legacy.join(binary))?;
            }
        }
        File::open(&legacy)?.sync_all()?;
        atomic_link(Path::new("legacy"), &root.join("current"))?;
    }
    for binary in BINARIES {
        let link = bin_dir.join(binary);
        let expected = root.join("current").join(binary);
        if fs::read_link(&link).ok().as_ref() == Some(&expected) {
            continue;
        }
        if link.exists() {
            ensure!(
                legacy.join(binary).exists(),
                "unmanaged binary at {}; preserve it before updating",
                link.display()
            );
        }
        atomic_link(&expected, &link)?;
    }
    Ok(())
}

fn activate(root: &Path, version_dir: &Path, bin_dir: &Path) -> Result<()> {
    prepare_launchers(root, bin_dir)?;
    let old = fs::read_link(root.join("current"))?;
    atomic_link(&old, &root.join("previous"))?;
    atomic_link(version_dir, &root.join("current"))
}

fn rollback(root: &Path) -> Result<String> {
    let old =
        fs::read_link(root.join("previous")).context("no previous installation to restore")?;
    ensure!(
        old.components().count() == 1 && root.join(&old).is_dir(),
        "invalid previous installation"
    );
    for binary in BINARIES {
        ensure!(
            root.join(&old).join(binary).is_file(),
            "previous installation is incomplete"
        );
    }
    let current = fs::read_link(root.join("current"))?;
    atomic_link(&old, &root.join("current"))?;
    atomic_link(&current, &root.join("previous"))?;
    Ok(old.to_string_lossy().into_owned())
}

pub fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("pmux update [--check] [--json] [--bin-dir PATH]\npmux update --rollback\npmux update --source [--host|--mux|--all]\n\nDownload a complete stable release from {REPOSITORY} (0.2.0 onward).\nVerify immutable release metadata, asset sizes, and SHA-256 digests.\nStage all six binaries, then activate them together. Retain the previous version.\nUpdating never stops sessions. Use pmux restart separately.\n--source is an explicit development-only source build.");
        return Ok(());
    }
    let options = options(args)?;
    let root = root()?;
    let _lock = lock(&root)?;
    if options.rollback {
        let restored = rollback(&root)?;
        println!(
            "{}",
            serde_json::json!({"status":"rolled_back","version":restored,"restart_required":true})
        );
        return Ok(());
    }
    let release = latest_release()?;
    let version = release_version(&release)?;
    let target = target()?;
    let assets: Vec<_> = BINARIES
        .iter()
        .map(|b| select_asset(&release, target, b))
        .collect::<Result<_>>()?;
    let installed = receipt(&root.join("current"))
        .ok()
        .map(|r| r.version)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let current = Version::parse(&installed)?;
    if options.check || version <= current {
        if options.json {
            println!(
                "{}",
                serde_json::json!({"repository":REPOSITORY,"installed":installed,"available":version.to_string(),"update_available":version>current,"status":"checked"})
            );
        } else {
            println!(
                "Installed: {installed}\nAvailable: {version}\nSource: {REPOSITORY}\n{}",
                if version > current {
                    "Run pmux update to install."
                } else {
                    "You are up to date."
                }
            );
        }
        return Ok(());
    }
    let bin_dir = match options.bin_dir {
        Some(ref p) => p.clone(),
        None => default_bin_dir(&root)?,
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory_name = format!("v{version}-{target}-{nonce}");
    let directory = root.join(&directory_name);
    // The update lock proves no other updater owns these abandoned downloads.
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|n| n.strip_prefix(".stage-"))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
            && entry.file_type()?.is_dir()
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    let staging = root.join(format!(".stage-{}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        for (binary, asset) in BINARIES.iter().zip(assets) {
            eprintln!("Downloading {binary} {version}");
            let path = staging.join(binary);
            let status = curl()
                .arg("--max-filesize")
                .arg(MAX_ASSET.to_string())
                .arg("--output")
                .arg(&path)
                .arg(&asset.browser_download_url)
                .status()?;
            ensure!(
                status.success(),
                "download failed for {binary}; installed version unchanged"
            );
            verify(&path, asset)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            File::open(&path)?.sync_all()?;
        }
        // The complete set is trusted before any downloaded program executes.
        for binary in BINARIES {
            let text = version_label(&staging.join(binary))?;
            ensure!(
                text.split_whitespace().any(|w| w == version.to_string()),
                "{binary} did not report release version {version}"
            );
        }
        write_receipt(
            &staging,
            &Receipt {
                repository: REPOSITORY.into(),
                version: version.to_string(),
                target: target.into(),
                bin_dir: bin_dir.clone(),
            },
        )?;
        fs::rename(&staging, &directory)?;
        activate(&root, Path::new(&directory_name), &bin_dir)?;
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    if options.json {
        println!(
            "{}",
            serde_json::json!({"status":"installed","version":version.to_string(),"repository":REPOSITORY,"restart_required":true})
        );
    } else {
        println!("Installed {version} from {REPOSITORY}. Running components keep their current version.\nUse pmux restart to review and apply component restarts. Use pmux update --rollback to restore the previous installation.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Release {
        Release {
            tag_name: "v0.2.0".into(),
            draft: false,
            prerelease: false,
            immutable: true,
            assets: BINARIES
                .iter()
                .map(|b| {
                    let name = asset_name("v0.2.0", "x86_64-unknown-linux-gnu", b);
                    Asset {
                        browser_download_url: format!(
                            "https://github.com/{REPOSITORY}/releases/download/v0.2.0/{name}"
                        ),
                        name,
                        size: 3,
                        digest: Some(format!("sha256:{:x}", Sha256::digest(b"old"))),
                    }
                })
                .collect(),
        }
    }
    fn temporary() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pmux-release-{}-{}",
            std::process::id(),
            crate::host_render_status::unix_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn release_channel_rejects_unpublished_mutable_and_old_releases() {
        let mut release = fixture();
        assert_eq!(release_version(&release).unwrap(), Version::new(0, 2, 0));
        release.immutable = false;
        assert!(release_version(&release).is_err());
        release.immutable = true;
        release.draft = true;
        assert!(release_version(&release).is_err());
        release.draft = false;
        for version in ["v0.1.999", "v0.2.0-beta.1", "v0.2.0+untrusted", "../../bad"] {
            release.tag_name = version.into();
            assert!(release_version(&release).is_err());
        }
    }
    #[test]
    fn assets_require_exact_repo_platform_digest_and_complete_set() {
        let mut release = fixture();
        let target = "x86_64-unknown-linux-gnu";
        for binary in BINARIES {
            assert!(select_asset(&release, target, binary).is_ok());
        }
        assert!(select_asset(&release, "aarch64-apple-darwin", "pmux").is_err());
        release.assets[0].browser_download_url = release.assets[0]
            .browser_download_url
            .replace(REPOSITORY, "other/repo");
        assert!(select_asset(&release, target, "pmux").is_err());
        release = fixture();
        release.assets[0].digest = None;
        assert!(select_asset(&release, target, "pmux").is_err());
        release = fixture();
        release.assets[0].size = MAX_ASSET + 1;
        assert!(select_asset(&release, target, "pmux").is_err());
        release = fixture();
        release.assets.remove(0);
        assert!(select_asset(&release, target, "pmux").is_err());
    }
    #[test]
    fn digest_rejects_same_size_corruption() {
        let dir = temporary();
        let path = dir.join("binary");
        let release = fixture();
        fs::write(&path, b"old").unwrap();
        verify(&path, &release.assets[0]).unwrap();
        fs::write(&path, b"bad").unwrap();
        assert!(verify(&path, &release.assets[0]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn activation_switches_complete_set_and_rollback_restores_original() {
        let dir = temporary();
        let root = dir.join("updates");
        let bins = dir.join("bin");
        fs::create_dir_all(root.join("v0.2.0-test")).unwrap();
        fs::create_dir(&bins).unwrap();
        for binary in BINARIES {
            fs::write(bins.join(binary), b"old").unwrap();
            fs::write(root.join("v0.2.0-test").join(binary), b"new").unwrap();
        }
        prepare_launchers(&root, &bins).unwrap();
        for binary in BINARIES {
            assert_eq!(fs::read(bins.join(binary)).unwrap(), b"old");
        }
        // Repeat preparation after a simulated interruption; no old file is lost.
        prepare_launchers(&root, &bins).unwrap();
        activate(&root, Path::new("v0.2.0-test"), &bins).unwrap();
        for binary in BINARIES {
            assert_eq!(fs::read(bins.join(binary)).unwrap(), b"new");
        }
        rollback(&root).unwrap();
        for binary in BINARIES {
            assert_eq!(fs::read(bins.join(binary)).unwrap(), b"old");
        }
        rollback(&root).unwrap();
        assert_eq!(fs::read(bins.join("pmux")).unwrap(), b"new");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn update_lock_refuses_overlap() {
        let dir = temporary();
        let held = lock(&dir).unwrap();
        assert!(lock(&dir).is_err());
        drop(held);
        assert!(lock(&dir).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn incomplete_legacy_install_does_not_replace_any_launcher() {
        let dir = temporary();
        let root = dir.join("updates");
        let bins = dir.join("bin");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(&bins).unwrap();
        fs::write(bins.join("pmux"), b"old").unwrap();
        assert!(prepare_launchers(&root, &bins).is_err());
        assert!(!root.join("current").exists());
        assert_eq!(fs::read(bins.join("pmux")).unwrap(), b"old");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn version_probe_bounds_output_and_reaps_descendants() {
        let dir = temporary();
        let script = dir.join("probe");
        fs::write(&script, b"#!/bin/sh\necho pmux 0.2.0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(version_label(&script).unwrap(), "pmux 0.2.0");
        fs::write(
            &script,
            b"#!/bin/sh\nwhile :; do echo too-much-output; done\n",
        )
        .unwrap();
        assert!(version_label(&script).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
