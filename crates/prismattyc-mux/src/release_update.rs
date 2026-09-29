//! Immutable GitHub releases, verified downloads, and atomic version activation.
//! No git checkout or relationship to the pre-0.2 repository is required.
use anyhow::{bail, ensure, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(unix)]
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
        ("windows", "x86_64") => Ok(if cfg!(target_env = "msvc") {
            "x86_64-pc-windows-msvc"
        } else {
            "x86_64-pc-windows-gnu"
        }),
        _ => bail!("no release target for this platform"),
    }
}

pub fn asset_name(tag: &str, target: &str, binary: &str) -> String {
    let suffix = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    format!("prismattyc-{tag}-{target}-{binary}{suffix}")
}

const MACOS_CHECKSUMS_NAME: &str = "SHA256SUMS-macos";
const MACOS_MANIFEST_NAME: &str = "manifest-macos-universal.json";
#[cfg(any(test, target_os = "macos"))]
const MACOS_BUNDLE_TARGET: &str = "universal-apple-darwin";

fn macos_zip_name(tag: &str) -> String {
    format!("Prismattyc-{tag}-macos-universal.zip")
}

fn macos_dmg_name(tag: &str) -> String {
    format!("Prismattyc-{tag}-macos-universal.dmg")
}

fn macos_target(target: &str) -> bool {
    target.ends_with("-apple-darwin")
}

fn platform_label(target: &str) -> &'static str {
    if macos_target(target) {
        "macos"
    } else if target.contains("windows") {
        "windows"
    } else if target.contains("linux") {
        "linux"
    } else {
        "this platform"
    }
}

/// Where to install when `pmux` or `prismattyc` is not already inside an app.
/// An executable inside `Prismattyc.app` wins, then an existing system app,
/// then an existing user app, then `/Applications/Prismattyc.app`.
#[cfg(any(test, target_os = "macos"))]
fn preferred_macos_app(
    current_exe: Option<&Path>,
    home: Option<&Path>,
    system_exists: bool,
    user_exists: bool,
) -> PathBuf {
    if let Some(exe) = current_exe {
        if let Some(bundle) = app_bundle_from_executable(exe) {
            if bundle
                .file_name()
                .is_some_and(|name| name == "Prismattyc.app")
            {
                return bundle;
            }
        }
    }
    if system_exists {
        return PathBuf::from("/Applications/Prismattyc.app");
    }
    if user_exists {
        if let Some(home) = home {
            return home.join("Applications/Prismattyc.app");
        }
    }
    PathBuf::from("/Applications/Prismattyc.app")
}

#[cfg(any(test, target_os = "macos"))]
fn app_bundle_from_executable(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.extension()? != "app" {
        return None;
    }
    Some(app.to_path_buf())
}

fn manual_update_instructions(tag: &str, target: &str) -> String {
    if macos_target(target) {
        let dmg = macos_dmg_name(tag);
        let zip = macos_zip_name(tag);
        format!(
            "Update manually: download https://github.com/{REPOSITORY}/releases/download/{tag}/{dmg} \
             (or the zip https://github.com/{REPOSITORY}/releases/download/{tag}/{zip}), \
             open it, and replace /Applications/Prismattyc.app. Quit Prismattyc and reopen it from the Dock."
        )
    } else {
        format!(
            "Update manually: download the {target} assets from \
             https://github.com/{REPOSITORY}/releases/tag/{tag} \
             and replace the installed binaries. See docs/update-and-restart.md."
        )
    }
}

fn asset_match_error(release: &Release, target: &str, expected: &str, found: usize) -> String {
    let names = release
        .assets
        .iter()
        .map(|asset| asset.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let names = if names.is_empty() {
        "(none)".to_string()
    } else {
        names
    };
    let why = if found == 0 {
        format!("Nothing in this release is named {expected}.")
    } else {
        format!("{found} assets are named {expected}.")
    };
    format!(
        "release {} on {} target {target} needs exactly one {expected} asset, found {found}. {why} Candidate assets: {names}. {}",
        release.tag_name,
        platform_label(target),
        manual_update_instructions(&release.tag_name, target)
    )
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

#[derive(Debug)]
enum UpdatePlan<'a> {
    Binaries(Vec<&'a Asset>),
    MacosBundle {
        zip: &'a Asset,
        checksums: &'a Asset,
        manifest: Option<&'a Asset>,
    },
}

fn select_asset<'a>(release: &'a Release, target: &str, binary: &str) -> Result<&'a Asset> {
    select_named(
        release,
        target,
        &asset_name(&release.tag_name, target, binary),
    )
}

fn select_named<'a>(release: &'a Release, target: &str, name: &str) -> Result<&'a Asset> {
    let matching: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect();
    if matching.len() != 1 {
        bail!(
            "{}",
            asset_match_error(release, target, name, matching.len())
        );
    }
    let asset = matching[0];
    ensure!(
        asset.size > 0 && asset.size <= MAX_ASSET,
        "invalid size for {name}"
    );
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .with_context(|| format!("release asset {name} has no SHA-256 digest"))?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
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

/// Linux and Windows keep per-binary assets. macOS arm64 and x86_64 both use
/// the universal app zip already published for v0.2.21, plus SHA256SUMS-macos.
/// A manifest is optional so older releases stay installable.
fn select_plan<'a>(release: &'a Release, target: &str) -> Result<UpdatePlan<'a>> {
    if macos_target(target) {
        let zip = select_named(release, target, &macos_zip_name(&release.tag_name))?;
        let checksums = select_named(release, target, MACOS_CHECKSUMS_NAME)?;
        let manifest = match release
            .assets
            .iter()
            .filter(|asset| asset.name == MACOS_MANIFEST_NAME)
            .count()
        {
            0 => None,
            1 => Some(select_named(release, target, MACOS_MANIFEST_NAME)?),
            found => bail!(
                "{}",
                asset_match_error(release, target, MACOS_MANIFEST_NAME, found)
            ),
        };
        return Ok(UpdatePlan::MacosBundle {
            zip,
            checksums,
            manifest,
        });
    }
    let mut assets = Vec::with_capacity(BINARIES.len());
    for binary in BINARIES {
        assets.push(select_asset(release, target, binary)?);
    }
    Ok(UpdatePlan::Binaries(assets))
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Deserialize)]
struct MacosManifest {
    repository: String,
    version: String,
    target: String,
    assets: Vec<MacosManifestAsset>,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Deserialize)]
struct MacosManifestAsset {
    name: String,
    sha256: String,
}

/// Parse a `shasum -a 256` or `sha256sum` line list and return the hex digest.
#[cfg(any(test, target_os = "macos"))]
fn sha256_entry(sums: &str, name: &str) -> Result<String> {
    let mut found = None;
    for raw in sums.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().context("checksum line has no hash")?;
        let mut file = parts
            .next()
            .with_context(|| format!("checksum line has no filename: {line}"))?;
        ensure!(
            parts.next().is_none(),
            "checksum line has extra fields: {line}"
        );
        if let Some(stripped) = file.strip_prefix('*') {
            file = stripped;
        }
        ensure!(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid checksum hash for {file}"
        );
        if file == name {
            ensure!(found.is_none(), "checksum file lists {name} more than once");
            found = Some(hash.to_ascii_lowercase());
        }
    }
    found.with_context(|| format!("checksum file has no entry for {name}"))
}

#[cfg(any(test, target_os = "macos"))]
fn manifest_zip_digest(bytes: &[u8], version: &Version, zip_name: &str) -> Result<String> {
    let manifest: MacosManifest =
        serde_json::from_slice(bytes).context("parse macOS release manifest")?;
    ensure!(
        manifest.repository == REPOSITORY,
        "macOS manifest repository mismatch"
    );
    ensure!(
        manifest.version == version.to_string(),
        "macOS manifest version mismatch"
    );
    ensure!(
        manifest.target == MACOS_BUNDLE_TARGET,
        "macOS manifest target mismatch"
    );
    let matches: Vec<_> = manifest
        .assets
        .iter()
        .filter(|asset| asset.name == zip_name)
        .collect();
    ensure!(
        matches.len() == 1,
        "macOS manifest needs exactly one {zip_name}"
    );
    let hash = &matches[0].sha256;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid manifest sha256"
    );
    Ok(hash.to_ascii_lowercase())
}

#[cfg(any(test, target_os = "macos"))]
fn confirm_macos_zip(
    zip: &Asset,
    sums_text: &str,
    manifest_bytes: Option<&[u8]>,
    version: &Version,
    target: &str,
) -> Result<()> {
    let github = zip
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("macOS zip has no SHA-256 digest")?;
    let listed = sha256_entry(sums_text, &zip.name)?;
    let tag = format!("v{version}");
    ensure!(
        listed.eq_ignore_ascii_case(github),
        "SHA256SUMS-macos hash for {} is {listed}, but the release digest is {github}. The files disagree, so nothing was installed. {}",
        zip.name,
        manual_update_instructions(&tag, target)
    );
    if let Some(bytes) = manifest_bytes {
        let from_manifest = manifest_zip_digest(bytes, version, &zip.name)?;
        ensure!(
            from_manifest.eq_ignore_ascii_case(github),
            "manifest-macos-universal.json hash for {} does not match the release digest. Nothing was installed. {}",
            zip.name,
            manual_update_instructions(&tag, target)
        );
    }
    Ok(())
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
    let mut command = Command::new(executable);
    #[cfg(unix)]
    command.process_group(0);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    let mut child = command.spawn()?;
    #[cfg(windows)]
    let (mut child, probe_job) = crate::platform::spawn_probe(&mut command)?;
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
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    drop(probe_job);
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn root() -> Result<PathBuf> {
    let data = crate::platform::data_home()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(crate::platform::home_dir().unwrap_or_default()).join(".local/share")
        });
    ensure!(data.is_absolute(), "update data directory must be absolute");
    Ok(data.join("prismattyc/updates"))
}

fn lock(root: &Path) -> Result<File> {
    fs::create_dir_all(root)?;
    #[cfg(windows)]
    crate::platform::secure_directory(root)?;
    let file = crate::platform::private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("update.lock"))?;
    crate::platform::try_lock_exclusive(&file).context("another update or rollback is running")?;
    Ok(file)
}

#[cfg(unix)]
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
    let mut file = crate::platform::private_options()
        .write(true)
        .create_new(true)
        .open(directory.join("receipt.json"))?;
    file.write_all(&serde_json::to_vec_pretty(receipt)?)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    Ok(())
}

/// Resolve the active release, without depending on the old executable's path.
pub fn installed_binary(binary: &str) -> Option<PathBuf> {
    if !BINARIES.contains(&binary) {
        return None;
    }
    let root = root().ok()?;
    let path = current_directory(&root)
        .ok()?
        .join(crate::platform::executable_name(binary));
    path.is_file().then_some(path)
}

#[cfg(windows)]
pub fn replacement_binary(binary: &str) -> Result<PathBuf> {
    ensure!(BINARIES.contains(&binary), "unknown executable");
    let root = root()?;
    if !root.join("windows-current.json").try_exists()? {
        return std::env::current_exe().context("current executable");
    }
    let directory = current_directory(&root)?;
    windows_update::complete(&directory)?;
    Ok(directory.join(crate::platform::executable_name(binary)))
}

fn default_bin_dir(root: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    if root.join("windows-current.json").exists() {
        return Ok(windows_update::load(root)?.bin_dir);
    }
    if let Ok(directory) = current_directory(root) {
        if directory.join("receipt.json").exists() {
            return Ok(receipt(&directory)?.bin_dir);
        }
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
#[cfg(unix)]
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
        let mut file = crate::platform::private_options()
            .write(true)
            .create(true)
            .truncate(true)
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

#[cfg(unix)]
fn activate(root: &Path, version_dir: &Path, bin_dir: &Path) -> Result<()> {
    prepare_launchers(root, bin_dir)?;
    let old = fs::read_link(root.join("current"))?;
    atomic_link(&old, &root.join("previous"))?;
    atomic_link(version_dir, &root.join("current"))
}

#[cfg(unix)]
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
        println!("pmux update [--check] [--json] [--bin-dir PATH]\nprismattyc update [--check] [--json] [--bin-dir PATH]\npmux update --rollback\npmux update --source [--host|--mux|--all]\n\nDownload a complete stable release from {REPOSITORY} (0.2.0 onward).\nVerify immutable release metadata, asset sizes, and SHA-256 digests.\nOn Linux and Windows, stage all six binaries, then activate them together. --rollback restores that previous installation.\nOn macOS, download the universal app zip, verify SHA256SUMS-macos and the code signature, and replace Prismattyc.app. The old app is deleted after the new one is in place. --rollback is not supported; reinstall a version from its DMG on https://github.com/{REPOSITORY}/releases.\nprismattyc update is the same command as pmux update.\nUpdating never stops sessions. Quit Prismattyc and reopen it after a macOS update. Use pmux restart for Linux and Windows components.\n--source is an explicit development-only source build.");
        return Ok(());
    }
    let options = options(args)?;
    let root = root()?;
    let _lock = lock(&root)?;
    if options.rollback {
        if std::env::consts::OS == "macos" {
            bail!("{}", macos_rollback_error());
        }
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
    let plan = select_plan(&release, target)?;
    let installed = installed_label(&root, matches!(plan, UpdatePlan::MacosBundle { .. }))?;
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
                    "Run pmux update or prismattyc update to install."
                } else {
                    "You are up to date."
                }
            );
        }
        return Ok(());
    }
    match plan {
        UpdatePlan::MacosBundle {
            zip,
            checksums,
            manifest,
        } => install_macos_bundle(
            &root,
            MacosDownload {
                tag: &release.tag_name,
                version: &version,
                target,
                zip,
                checksums,
                manifest,
                json: options.json,
            },
        ),
        UpdatePlan::Binaries(assets) => install_binaries(
            &root,
            options.bin_dir.as_deref(),
            &version,
            target,
            &assets,
            options.json,
        ),
    }
}

fn installed_label(root: &Path, macos_bundle: bool) -> Result<String> {
    if macos_bundle {
        if let Some(version) = current_macos_app_version() {
            return Ok(version);
        }
    }
    Ok(current_directory(root)
        .and_then(|path| receipt(&path))
        .ok()
        .map(|saved| saved.version)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()))
}

fn current_macos_app_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let app = discover_macos_app()?;
        let text = version_label(&app.join("Contents/MacOS/pmux")).ok()?;
        text.split_whitespace()
            .find(|word| Version::parse(word).is_ok())
            .map(ToString::to_string)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn clear_abandoned_stages(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|entry_name| entry_name.strip_prefix(".stage-"))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
            && entry.file_type()?.is_dir()
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn install_binaries(
    root: &Path,
    bin_dir_override: Option<&Path>,
    version: &Version,
    target: &str,
    assets: &[&Asset],
    json: bool,
) -> Result<()> {
    let bin_dir = match bin_dir_override {
        Some(path) => path.to_path_buf(),
        None => default_bin_dir(root)?,
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory_name = format!("v{version}-{target}-{nonce}");
    let directory = root.join(&directory_name);
    // The update lock proves no other updater owns these abandoned downloads.
    clear_abandoned_stages(root)?;
    let staging = root.join(format!(".stage-{}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        for (binary, asset) in BINARIES.iter().zip(assets) {
            eprintln!("Downloading {binary} {version}");
            let path = staging.join(crate::platform::executable_name(binary));
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
            crate::platform::set_mode(&path, 0o755)?;
            crate::platform::sync_file(&path)?;
        }
        // The complete set is trusted before any downloaded program executes.
        for binary in BINARIES {
            let text = version_label(&staging.join(crate::platform::executable_name(binary)))?;
            ensure!(
                text.split_whitespace()
                    .any(|word| word == version.to_string()),
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
        activate(root, Path::new(&directory_name), &bin_dir)?;
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    if json {
        println!(
            "{}",
            serde_json::json!({"status":"installed","version":version.to_string(),"repository":REPOSITORY,"restart_required":true})
        );
    } else {
        println!("Installed {version} from {REPOSITORY}. Running components keep their current version.\nUse pmux restart to review and apply component restarts. Use pmux update --rollback or prismattyc update --rollback to restore the previous installation.");
    }
    Ok(())
}

fn macos_rollback_error() -> String {
    format!(
        "rollback is not supported on macOS. Prismattyc.app updates replace the installed app and do not keep a previous copy. \
         Reinstall a specific version by downloading its DMG from https://github.com/{REPOSITORY}/releases, \
         opening it, and replacing /Applications/Prismattyc.app. Quit Prismattyc and reopen it from the Dock."
    )
}

#[cfg(target_os = "macos")]
#[derive(Debug, Serialize, Deserialize)]
struct MacosAppState {
    repository: String,
    version: String,
    target: String,
    app: PathBuf,
}

#[cfg(target_os = "macos")]
fn macos_state_path(root: &Path) -> PathBuf {
    root.join("macos-app.json")
}

#[cfg(target_os = "macos")]
fn write_macos_state(root: &Path, state: &MacosAppState) -> Result<()> {
    let path = macos_state_path(root);
    let temporary = root.join(format!("macos-app.json.{}.tmp", std::process::id()));
    let mut file = crate::platform::private_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    let _ = File::open(root).and_then(|dir| dir.sync_all());
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn require_app_destination(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Prismattyc.app path must be absolute");
    ensure!(
        path.file_name()
            .is_some_and(|name| name == "Prismattyc.app"),
        "refusing to replace {} because it is not Prismattyc.app",
        path.display()
    );
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn sync_parent(parent: &Path) -> Result<()> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Move `incoming` onto `destination` on the same volume. When an app was
/// already installed, it is left at the returned path until the caller deletes
/// it. A failed swap puts that app back at `destination`.
#[cfg(any(test, target_os = "macos"))]
fn replace_app_bundle(destination: &Path, incoming: &Path) -> Result<Option<PathBuf>> {
    require_app_destination(destination)?;
    ensure!(
        incoming.is_dir(),
        "staged app is missing at {}",
        incoming.display()
    );
    ensure!(
        incoming.join("Contents/MacOS/pmux").is_file(),
        "staged app has no Contents/MacOS/pmux"
    );
    let parent = destination
        .parent()
        .context("Prismattyc.app parent")?
        .to_path_buf();
    if !destination.exists() {
        fs::rename(incoming, destination)
            .with_context(|| format!("could not install {}", destination.display()))?;
        sync_parent(&parent)?;
        return Ok(None);
    }
    let meta = fs::symlink_metadata(destination)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "refusing to replace a symlink at {}",
        destination.display()
    );
    ensure!(
        meta.is_dir(),
        "refusing to replace {} because it is not an app bundle",
        destination.display()
    );
    let displaced = parent.join(format!(".Prismattyc.app.displaced-{}", std::process::id()));
    if displaced.exists() {
        fs::remove_dir_all(&displaced)?;
    }
    fs::rename(destination, &displaced).with_context(|| {
        format!(
            "could not move the installed app aside at {}",
            destination.display()
        )
    })?;
    if let Err(error) = fs::rename(incoming, destination) {
        let restored = fs::rename(&displaced, destination);
        if let Err(restore) = restored {
            bail!(
                "install failed ({error}) and restoring {} also failed ({restore}). The previous app is at {}.",
                destination.display(),
                displaced.display()
            );
        }
        return Err(error).context(format!(
            "install failed and the previous app was restored at {}",
            destination.display()
        ));
    }
    sync_parent(&parent)?;
    Ok(Some(displaced))
}

/// Put the displaced app back at `destination` after a failed install.
/// The copy that was briefly installed is removed.
#[cfg(any(test, target_os = "macos"))]
fn restore_displaced_app(destination: &Path, displaced: &Path) -> Result<()> {
    require_app_destination(destination)?;
    ensure!(
        displaced.is_dir(),
        "displaced app is missing at {}",
        displaced.display()
    );
    let parent = destination
        .parent()
        .context("Prismattyc.app parent")?
        .to_path_buf();
    let holding = parent.join(format!(".Prismattyc.app.failed-{}", std::process::id()));
    if holding.exists() {
        fs::remove_dir_all(&holding)?;
    }
    if destination.exists() {
        fs::rename(destination, &holding).with_context(|| {
            format!(
                "could not move the failed install aside at {}",
                destination.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(displaced, destination) {
        if holding.exists() {
            let restored = fs::rename(&holding, destination);
            if let Err(restore) = restored {
                bail!(
                    "could not restore the previous app ({error}) and putting the new app back also failed ({restore}). The previous app is at {}.",
                    displaced.display()
                );
            }
        }
        return Err(error).context(format!(
            "could not move the previous app back to {}. It is still at {}",
            destination.display(),
            displaced.display()
        ));
    }
    if holding.exists() {
        fs::remove_dir_all(&holding).with_context(|| {
            format!(
                "the previous app is restored at {}, but the failed copy remains at {}",
                destination.display(),
                holding.display()
            )
        })?;
    }
    sync_parent(&parent)?;
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn discard_replaced_app(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn leftover_app_notice(path: &Path, error: &dyn std::fmt::Display) -> String {
    format!(
        "The new Prismattyc.app is installed, but the old app is still at {}. It was left there because deleting it failed: {error}. Remove that directory yourself with `rm -rf '{}'.",
        path.display(),
        path.display()
    )
}

struct MacosDownload<'a> {
    tag: &'a str,
    version: &'a Version,
    target: &'a str,
    zip: &'a Asset,
    checksums: &'a Asset,
    manifest: Option<&'a Asset>,
    json: bool,
}

fn install_macos_bundle(root: &Path, download: MacosDownload<'_>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        install_macos_bundle_here(root, download)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            root,
            download.version,
            download.zip,
            download.checksums,
            download.manifest,
            download.json,
        );
        bail!(
            "macOS app installation must run on macOS. {}",
            manual_update_instructions(download.tag, download.target)
        )
    }
}

#[cfg(target_os = "macos")]
fn directory_writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = dir.as_os_str().as_bytes().to_vec();
    if bytes.contains(&0) {
        return false;
    }
    bytes.push(0);
    // SAFETY: `bytes` is a NUL-terminated path and contains no interior NUL.
    unsafe { libc::access(bytes.as_ptr().cast(), libc::W_OK) == 0 }
}

#[cfg(target_os = "macos")]
fn discover_macos_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    let home = crate::platform::home_dir().map(PathBuf::from);
    let system = PathBuf::from("/Applications/Prismattyc.app");
    let user = home
        .as_ref()
        .map(|dir| dir.join("Applications/Prismattyc.app"));
    let chosen = preferred_macos_app(
        exe.as_deref(),
        home.as_deref(),
        system.join("Contents/MacOS/pmux").is_file(),
        user.as_ref()
            .is_some_and(|path| path.join("Contents/MacOS/pmux").is_file()),
    );
    chosen
        .join("Contents/MacOS/pmux")
        .is_file()
        .then_some(chosen)
}

#[cfg(target_os = "macos")]
fn choose_macos_app(tag: &str, target: &str) -> Result<PathBuf> {
    if let Some(app) = discover_macos_app() {
        require_app_destination(&app)?;
        return Ok(app);
    }
    let home = crate::platform::home_dir().map(PathBuf::from);
    let user = home
        .as_ref()
        .map(|dir| dir.join("Applications/Prismattyc.app"));
    let chosen = preferred_macos_app(None, home.as_deref(), false, false);
    if chosen
        .parent()
        .is_some_and(|parent| directory_writable(parent))
    {
        return Ok(chosen);
    }
    if let Some(user) = user {
        if let Some(parent) = user.parent() {
            fs::create_dir_all(parent)?;
        }
        return Ok(user);
    }
    bail!(
        "could not find a writable location for Prismattyc.app. {}",
        manual_update_instructions(tag, target)
    )
}

#[cfg(target_os = "macos")]
fn download_asset(asset: &Asset, path: &Path) -> Result<()> {
    let status = curl()
        .arg("--max-filesize")
        .arg(asset.size.to_string())
        .arg("--output")
        .arg(path)
        .arg(&asset.browser_download_url)
        .status()
        .with_context(|| format!("download {}", asset.name))?;
    ensure!(
        status.success(),
        "download failed for {}; installed version unchanged",
        asset.name
    );
    verify(path, asset)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_macos_trust(app: &Path, tag: &str, target: &str) -> Result<()> {
    let codesign = Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(app)
        .status()
        .context("codesign is required to verify Prismattyc.app")?;
    ensure!(
        codesign.success(),
        "codesign rejected {} ({codesign}). The installed app was not changed because the downloaded bundle failed code signature verification. {}",
        app.display(),
        manual_update_instructions(tag, target)
    );
    let gatekeeper = Command::new("spctl")
        .args(["--assess", "--verbose", "--type", "exec"])
        .arg(app)
        .status()
        .context("spctl is required to assess Prismattyc.app")?;
    ensure!(
        gatekeeper.success(),
        "spctl rejected {} ({gatekeeper}). The installed app was not changed because Gatekeeper did not accept the downloaded bundle. {}",
        app.display(),
        manual_update_instructions(tag, target)
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn extract_macos_zip(zip: &Path, dest: &Path, tag: &str, target: &str) -> Result<PathBuf> {
    let status = Command::new("ditto")
        .args(["-x", "-k"])
        .arg(zip)
        .arg(dest)
        .status()
        .context("ditto is required to unpack the macOS update")?;
    ensure!(
        status.success(),
        "unpacking {} failed ({status}). The installed app was not changed. {}",
        zip.display(),
        manual_update_instructions(tag, target)
    );
    let app = dest.join("Prismattyc.app");
    ensure!(
        app.join("Contents/MacOS/pmux").is_file()
            && app.join("Contents/MacOS/prismattyc-host").is_file(),
        "the zip does not contain Prismattyc.app with pmux and prismattyc-host. The installed app was not changed. {}",
        manual_update_instructions(tag, target)
    );
    Ok(app)
}

#[cfg(target_os = "macos")]
fn clear_quarantine(app: &Path) {
    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(app)
        .status();
}

#[cfg(target_os = "macos")]
fn app_process_running(app: &Path) -> bool {
    let Ok(output) = Command::new("/bin/ps").args(["-axo", "command="]).output() else {
        return true;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let marker = app.to_string_lossy().into_owned();
    text.lines().any(|line| line.contains(&marker))
}

#[cfg(target_os = "macos")]
fn reported_release_version(executable: &Path, version: &Version) -> Result<()> {
    let text = version_label(executable)?;
    ensure!(
        text.split_whitespace()
            .any(|word| word == version.to_string()),
        "{} did not report release version {version}",
        executable.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos_bundle_here(root: &Path, download: MacosDownload<'_>) -> Result<()> {
    let MacosDownload {
        tag,
        version,
        target,
        zip,
        checksums,
        manifest,
        json,
    } = download;
    let destination = choose_macos_app(tag, target)?;
    clear_abandoned_stages(root)?;
    let staging = root.join(format!(".stage-{}", std::process::id()));
    fs::create_dir(&staging)?;
    let mut incoming = None;
    let result = (|| -> Result<()> {
        eprintln!("Downloading {MACOS_CHECKSUMS_NAME} {version}");
        let sums_path = staging.join(MACOS_CHECKSUMS_NAME);
        download_asset(checksums, &sums_path)?;
        eprintln!("Downloading {} {version}", zip.name);
        let zip_path = staging.join(&zip.name);
        download_asset(zip, &zip_path)?;
        let manifest_bytes = if let Some(manifest) = manifest {
            eprintln!("Downloading {MACOS_MANIFEST_NAME} {version}");
            let manifest_path = staging.join(MACOS_MANIFEST_NAME);
            download_asset(manifest, &manifest_path)?;
            Some(fs::read(manifest_path)?)
        } else {
            None
        };
        let sums_text = fs::read_to_string(&sums_path)
            .with_context(|| format!("{} is not text", checksums.name))?;
        confirm_macos_zip(zip, &sums_text, manifest_bytes.as_deref(), version, target)?;
        let extracted = extract_macos_zip(&zip_path, &staging.join("unpacked"), tag, target)?;
        clear_quarantine(&extracted);
        verify_macos_trust(&extracted, tag, target)?;
        reported_release_version(&extracted.join("Contents/MacOS/pmux"), version)?;
        let parent = destination
            .parent()
            .context("Prismattyc.app parent")?
            .to_path_buf();
        fs::create_dir_all(&parent)?;
        let staged = parent.join(format!(".Prismattyc.app.incoming-{}", std::process::id()));
        if staged.exists() {
            fs::remove_dir_all(&staged)?;
        }
        let copied = Command::new("ditto")
            .arg(&extracted)
            .arg(&staged)
            .status()
            .context("ditto is required to stage Prismattyc.app")?;
        ensure!(
            copied.success(),
            "staging Prismattyc.app failed ({copied}). The installed app was not changed."
        );
        incoming = Some(staged.clone());
        clear_quarantine(&staged);
        verify_macos_trust(&staged, tag, target)?;
        let displaced = replace_app_bundle(&destination, &staged)?;
        incoming = None;
        if let Err(error) = verify_macos_trust(&destination, tag, target).and_then(|()| {
            reported_release_version(&destination.join("Contents/MacOS/pmux"), version)
        }) {
            if let Some(path) = &displaced {
                restore_displaced_app(&destination, path).with_context(|| error.to_string())?;
            } else {
                let _ = fs::remove_dir_all(&destination);
            }
            return Err(error);
        }
        if let Err(error) = write_macos_state(
            root,
            &MacosAppState {
                repository: REPOSITORY.into(),
                version: version.to_string(),
                target: MACOS_BUNDLE_TARGET.into(),
                app: destination.clone(),
            },
        ) {
            if let Some(path) = &displaced {
                restore_displaced_app(&destination, path).with_context(|| error.to_string())?;
            } else {
                let _ = fs::remove_dir_all(&destination);
            }
            return Err(error);
        }
        let mut leftover = None;
        if let Some(path) = displaced {
            if let Err(error) = discard_replaced_app(&path) {
                leftover = Some(leftover_app_notice(&path, &error));
            }
        }
        let running = app_process_running(&destination);
        let restart = if running {
            "A restart is required. Prismattyc is still running. Quit it (Cmd+Q) and reopen it from the Dock. Mux sessions keep running; reopen the app to use this version in windows."
        } else {
            "A restart is required to use this version. Open Prismattyc from the Dock. If it is already running, quit it (Cmd+Q) and reopen it. Mux sessions keep running until you restart them."
        };
        let inside = std::env::current_exe()
            .ok()
            .and_then(|exe| app_bundle_from_executable(&exe))
            .is_some_and(|bundle| bundle == destination);
        let outside = if inside {
            String::new()
        } else {
            " This updated Prismattyc.app. A pmux or prismattyc binary outside that app stays on its current build; open the app to run the new one.".to_string()
        };
        let leftover_text = leftover
            .as_ref()
            .map(|text| format!(" {text}"))
            .unwrap_or_default();
        let message = format!(
            "Installed {version} from {REPOSITORY} at {}. {restart}{outside}{leftover_text}",
            destination.display()
        );
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "installed",
                    "version": version.to_string(),
                    "repository": REPOSITORY,
                    "restart_required": true,
                    "app": destination,
                    "leftover_app": leftover,
                    "message": message,
                })
            );
        } else {
            println!("{message}");
        }
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    if let Some(path) = incoming {
        if path.exists() {
            let _ = fs::remove_dir_all(path);
        }
    }
    result
}

#[cfg(unix)]
fn current_directory(root: &Path) -> Result<PathBuf> {
    Ok(root.join("current"))
}
#[cfg(windows)]
fn current_directory(root: &Path) -> Result<PathBuf> {
    Ok(root.join(windows_update::load(root)?.current))
}
#[cfg(windows)]
fn activate(root: &Path, version_dir: &Path, bin_dir: &Path) -> Result<()> {
    windows_update::activate(root, version_dir, bin_dir)
}
#[cfg(windows)]
fn rollback(root: &Path) -> Result<String> {
    windows_update::rollback(root)
}

#[cfg(windows)]
pub(crate) fn is_windows_forwarding_pair(parent: &Path, child: &Path) -> bool {
    let check = || -> Result<bool> {
        let root = root()?;
        let parent = parent.canonicalize()?;
        let child = child.canonicalize()?;
        let name = parent.file_name().context("forwarder filename")?;
        if child.file_name() != Some(name) {
            return Ok(false);
        }
        let directory = child.parent().context("forwarded version directory")?;
        let root = root.canonicalize()?;
        if directory.parent() != Some(root.as_path()) {
            return Ok(false);
        }
        let component = directory
            .file_name()
            .and_then(|s| s.to_str())
            .context("version directory name")?;
        if component == "legacy" {
            let state = windows_update::load(&root)?;
            if parent.parent() != Some(state.bin_dir.canonicalize()?.as_path()) {
                return Ok(false);
            }
        } else {
            let installed = receipt(directory)?;
            if installed.repository != REPOSITORY
                || installed.target != target()?
                || !installed.bin_dir.is_absolute()
                || parent.parent() != Some(installed.bin_dir.canonicalize()?.as_path())
            {
                return Ok(false);
            }
            let version = Version::parse(&installed.version)?;
            let prefix = format!("v{version}-{}-", installed.target);
            if !component
                .strip_prefix(prefix.as_str())
                .is_some_and(|nonce| !nonce.is_empty() && nonce.bytes().all(|b| b.is_ascii_digit()))
            {
                return Ok(false);
            }
        }
        windows_update::complete(directory)?;
        Ok(true)
    };
    check().unwrap_or(false)
}

/// Windows keeps stable launch executables and atomically selects a versioned
/// directory. No running executable is replaced and no symlink privilege is needed.
pub fn forward_installed(binary: &str) -> Result<()> {
    #[cfg(windows)]
    {
        windows_update::forward(binary)
    }
    #[cfg(not(windows))]
    {
        let _ = binary;
        Ok(())
    }
}
#[cfg(windows)]
mod windows_update {
    use super::*;
    #[derive(serde::Serialize, serde::Deserialize)]
    pub(super) struct State {
        pub current: PathBuf,
        previous: Option<PathBuf>,
        pub(super) bin_dir: PathBuf,
    }
    fn validate_component(path: &Path) -> Result<()> {
        ensure!(
            matches!(
                (path.components().next(), path.components().count()),
                (Some(std::path::Component::Normal(_)), 1)
            ),
            "invalid update directory"
        );
        Ok(())
    }
    pub(super) fn load(root: &Path) -> Result<State> {
        let state: State = serde_json::from_slice(&fs::read(root.join("windows-current.json"))?)?;
        validate_component(&state.current)?;
        if let Some(previous) = state.previous.as_ref() {
            validate_component(previous)?;
        }
        ensure!(
            state.bin_dir.is_absolute(),
            "invalid installation directory"
        );
        Ok(state)
    }
    fn save(root: &Path, state: &State) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        let temp = root.join("windows-current.tmp");
        let mut file = crate::platform::private_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        drop(file);
        let wide = |p: &Path| {
            p.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        let destination = root.join("windows-current.json");
        unsafe {
            ensure!(
                MoveFileExW(
                    wide(&temp).as_ptr(),
                    wide(&destination).as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
                ) != 0,
                "activate Windows update: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
    pub(super) fn complete(directory: &Path) -> Result<()> {
        for binary in BINARIES {
            ensure!(
                directory
                    .join(crate::platform::executable_name(binary))
                    .is_file(),
                "incomplete version directory"
            );
        }
        Ok(())
    }
    pub(super) fn activate(root: &Path, version_dir: &Path, bin_dir: &Path) -> Result<()> {
        validate_component(version_dir)?;
        complete(&root.join(version_dir))?;
        ensure!(
            bin_dir.is_absolute(),
            "installation directory must be absolute"
        );
        let bin_dir = bin_dir.canonicalize()?;
        let mut state = if root.join("windows-current.json").exists() {
            let state = load(root)?;
            ensure!(
                state.bin_dir == bin_dir,
                "update store belongs to another installation"
            );
            state
        } else {
            complete(&bin_dir)?;
            let legacy = root.join("legacy");
            fs::create_dir_all(&legacy)?;
            for binary in BINARIES {
                let name = crate::platform::executable_name(binary);
                fs::copy(bin_dir.join(&name), legacy.join(&name))?;
                crate::platform::sync_file(&legacy.join(name))?;
            }
            State {
                current: PathBuf::from("legacy"),
                previous: None,
                bin_dir,
            }
        };
        state.previous = Some(state.current);
        state.current = version_dir.into();
        save(root, &state)
    }
    pub(super) fn rollback(root: &Path) -> Result<String> {
        let mut state = load(root)?;
        let old = state
            .previous
            .take()
            .context("no previous Windows installation")?;
        complete(&root.join(&old))?;
        state.previous = Some(state.current);
        state.current = old;
        save(root, &state)?;
        Ok(state.current.to_string_lossy().into_owned())
    }
    pub(super) fn forward(binary: &str) -> Result<()> {
        ensure!(BINARIES.contains(&binary), "unknown executable");
        let root = root()?;
        if !root.join("windows-current.json").exists() {
            return Ok(());
        }
        let state = load(&root)?;
        let executable = std::env::current_exe()?.canonicalize()?;
        if executable.parent() != Some(state.bin_dir.as_path()) {
            return Ok(());
        }
        let target = root
            .join(state.current)
            .join(crate::platform::executable_name(binary));
        complete(target.parent().context("version directory")?)?;
        let status = Command::new(target)
            .args(std::env::args_os().skip(1))
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pmux-release-{}-{}-{}",
            std::process::id(),
            crate::host_render_status::unix_ms(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn durable_receipt_selects_installation_without_overwriting_prior_metadata() {
        let dir = temporary();
        let current = dir.join("current");
        fs::create_dir(&current).unwrap();
        let original = Receipt {
            repository: REPOSITORY.into(),
            version: "0.2.0".into(),
            target: target().unwrap().into(),
            bin_dir: dir.join("custom-bin"),
        };
        let remembered = dir.join("legacy-bin");
        fs::write(
            dir.join("installation.json"),
            serde_json::to_vec(&remembered).unwrap(),
        )
        .unwrap();
        assert_eq!(default_bin_dir(&dir).unwrap(), remembered);
        write_receipt(&current, &original).unwrap();
        assert_eq!(default_bin_dir(&dir).unwrap(), original.bin_dir);
        let saved = receipt(&current).unwrap();
        assert_eq!(saved.version, "0.2.0");
        assert_eq!(saved.repository, REPOSITORY);
        assert_eq!(saved.target, original.target);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(current.join("receipt.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let bytes = fs::read(current.join("receipt.json")).unwrap();
        assert!(write_receipt(&current, &original).is_err());
        assert_eq!(fs::read(current.join("receipt.json")).unwrap(), bytes);
        fs::write(current.join("receipt.json"), b"invalid").unwrap();
        assert!(
            default_bin_dir(&dir).is_err(),
            "a corrupt receipt must not silently select another installation"
        );
        fs::remove_dir_all(dir).unwrap();
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
    fn canonical_github_asset_url_is_accepted() {
        let mut release = fixture();
        release.assets[0].browser_download_url = "https://github.com/moonbase2090/Prismattyc/releases/download/v0.2.0/prismattyc-v0.2.0-x86_64-unknown-linux-gnu-pmux".into();
        assert!(select_asset(&release, "x86_64-unknown-linux-gnu", "pmux").is_ok());
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
        // A parallel spawn can briefly inherit the descriptor between fork and
        // exec. CLOEXEC closes it before the child program starts.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if lock(&dir).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "update lock remained held");
            std::thread::yield_now();
        }
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
        crate::platform::set_mode(&script, 0o755).unwrap();
        assert_eq!(version_label(&script).unwrap(), "pmux 0.2.0");
        fs::write(
            &script,
            b"#!/bin/sh\nwhile :; do echo too-much-output; done\n",
        )
        .unwrap();
        assert!(version_label(&script).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    fn named_asset(tag: &str, name: &str) -> Asset {
        Asset {
            browser_download_url: format!(
                "https://github.com/{REPOSITORY}/releases/download/{tag}/{name}"
            ),
            name: name.to_string(),
            size: 3,
            digest: Some(format!("sha256:{:x}", Sha256::digest(b"old"))),
        }
    }

    fn release_with(tag: &str, names: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            immutable: true,
            assets: names.iter().map(|name| named_asset(tag, name)).collect(),
        }
    }

    fn binaries_release(tag: &str, target: &str) -> Release {
        let names: Vec<String> = BINARIES
            .iter()
            .map(|binary| asset_name(tag, target, binary))
            .collect();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        release_with(tag, &borrowed)
    }

    /// Asset names published on the immutable v0.2.21 release.
    fn published_v021_names() -> Vec<&'static str> {
        vec![
            "manifest-aarch64-unknown-linux-gnu.json",
            "manifest-x86_64-pc-windows-msvc.json",
            "manifest-x86_64-unknown-linux-gnu.json",
            "MPL-2.0.txt",
            "NOTICE.txt",
            "prismattyc-aarch64-unknown-linux-gnu.tar.gz",
            "prismattyc-v0.2.21-aarch64-unknown-linux-gnu-pmux",
            "prismattyc-v0.2.21-aarch64-unknown-linux-gnu-pmux-attach",
            "prismattyc-v0.2.21-aarch64-unknown-linux-gnu-pmux-mcp",
            "prismattyc-v0.2.21-aarch64-unknown-linux-gnu-pmuxd",
            "prismattyc-v0.2.21-aarch64-unknown-linux-gnu-prismattyc",
            "prismattyc-v0.2.21-aarch64-unknown-linux-gnu-prismattyc-host",
            "Prismattyc-v0.2.21-macos-universal.dmg",
            "Prismattyc-v0.2.21-macos-universal.zip",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc-pmux-attach.exe",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc-pmux-mcp.exe",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc-pmux.exe",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc-pmuxd.exe",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc-prismattyc-host.exe",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc-prismattyc.exe",
            "prismattyc-v0.2.21-x86_64-pc-windows-msvc.zip",
            "prismattyc-v0.2.21-x86_64-unknown-linux-gnu-pmux",
            "prismattyc-v0.2.21-x86_64-unknown-linux-gnu-pmux-attach",
            "prismattyc-v0.2.21-x86_64-unknown-linux-gnu-pmux-mcp",
            "prismattyc-v0.2.21-x86_64-unknown-linux-gnu-pmuxd",
            "prismattyc-v0.2.21-x86_64-unknown-linux-gnu-prismattyc",
            "prismattyc-v0.2.21-x86_64-unknown-linux-gnu-prismattyc-host",
            "prismattyc-x86_64-unknown-linux-gnu.tar.gz",
            "SHA256SUMS",
            "SHA256SUMS-macos",
            "SHA256SUMS-windows",
        ]
    }

    fn assert_bundle<'a>(plan: UpdatePlan<'a>, zip_name: &str) {
        match plan {
            UpdatePlan::MacosBundle {
                zip,
                checksums,
                manifest,
            } => {
                assert_eq!(zip.name, zip_name);
                assert_eq!(checksums.name, MACOS_CHECKSUMS_NAME);
                assert!(manifest.is_none());
            }
            UpdatePlan::Binaries(_) => panic!("macOS must select the universal app zip"),
        }
    }

    #[test]
    fn macos_arm64_and_x86_64_select_published_universal_zip() {
        let release = release_with("v0.2.21", &published_v021_names());
        let zip = macos_zip_name("v0.2.21");
        assert_bundle(select_plan(&release, "aarch64-apple-darwin").unwrap(), &zip);
        assert_bundle(select_plan(&release, "x86_64-apple-darwin").unwrap(), &zip);
    }

    #[test]
    fn linux_targets_select_per_binary_assets() {
        for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
            let release = binaries_release("v0.2.21", target);
            match select_plan(&release, target).unwrap() {
                UpdatePlan::Binaries(assets) => {
                    assert_eq!(assets.len(), BINARIES.len());
                    for (binary, asset) in BINARIES.iter().zip(assets) {
                        assert_eq!(asset.name, asset_name("v0.2.21", target, binary));
                        assert!(!asset.name.ends_with(".exe"));
                    }
                }
                UpdatePlan::MacosBundle { .. } => panic!("{target} is not a macOS bundle"),
            }
        }
    }

    #[test]
    fn windows_target_selects_exe_assets() {
        let target = "x86_64-pc-windows-msvc";
        let release = binaries_release("v0.2.21", target);
        match select_plan(&release, target).unwrap() {
            UpdatePlan::Binaries(assets) => {
                assert_eq!(assets.len(), BINARIES.len());
                for (binary, asset) in BINARIES.iter().zip(assets) {
                    assert_eq!(asset.name, asset_name("v0.2.21", target, binary));
                    assert!(asset.name.ends_with(".exe"), "{}", asset.name);
                }
            }
            UpdatePlan::MacosBundle { .. } => panic!("windows is not a macOS bundle"),
        }
    }

    #[test]
    fn zero_match_names_platform_candidates_and_manual_download() {
        let mut names = published_v021_names();
        names.retain(|name| *name != "Prismattyc-v0.2.21-macos-universal.zip");
        let release = release_with("v0.2.21", &names);
        let error = select_plan(&release, "aarch64-apple-darwin")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("on macos target aarch64-apple-darwin"),
            "{error}"
        );
        assert!(
            error.contains(
                "needs exactly one Prismattyc-v0.2.21-macos-universal.zip asset, found 0"
            ),
            "{error}"
        );
        assert!(
            error.contains(
                "Nothing in this release is named Prismattyc-v0.2.21-macos-universal.zip"
            ),
            "{error}"
        );
        assert!(
            error.contains("prismattyc-v0.2.21-x86_64-unknown-linux-gnu-pmux"),
            "{error}"
        );
        assert!(error.contains("SHA256SUMS-macos"), "{error}");
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases/download/v0.2.21/Prismattyc-v0.2.21-macos-universal.dmg"),
            "{error}"
        );
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases/download/v0.2.21/Prismattyc-v0.2.21-macos-universal.zip"),
            "{error}"
        );
        let _ = release.tag_name;
    }

    #[test]
    fn multi_match_names_platform_candidates_and_manual_download() {
        let mut names = published_v021_names();
        names.push("Prismattyc-v0.2.21-macos-universal.zip");
        let release = release_with("v0.2.21", &names);
        let error = select_plan(&release, "x86_64-apple-darwin")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("on macos target x86_64-apple-darwin"),
            "{error}"
        );
        assert!(
            error.contains(
                "needs exactly one Prismattyc-v0.2.21-macos-universal.zip asset, found 2"
            ),
            "{error}"
        );
        assert!(
            error.contains("2 assets are named Prismattyc-v0.2.21-macos-universal.zip"),
            "{error}"
        );
        assert!(
            error.contains("prismattyc-v0.2.21-aarch64-unknown-linux-gnu-pmux"),
            "{error}"
        );
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases/download/v0.2.21/Prismattyc-v0.2.21-macos-universal.dmg"),
            "{error}"
        );
        let mut doubled = binaries_release("v0.2.0", "x86_64-unknown-linux-gnu");
        let duplicate = doubled.assets[0].name.clone();
        doubled.assets.push(named_asset("v0.2.0", &duplicate));
        let error = select_plan(&doubled, "x86_64-unknown-linux-gnu")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("on linux target x86_64-unknown-linux-gnu"),
            "{error}"
        );
        assert!(error.contains("found 2"), "{error}");
        assert!(
            error.contains("prismattyc-v0.2.0-x86_64-unknown-linux-gnu-pmux"),
            "{error}"
        );
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases/tag/v0.2.0"),
            "{error}"
        );
    }

    #[test]
    fn sha256sums_macos_accepts_published_v021_bytes() {
        let sums = "\
4ca5ef039c61b3113346e740923cae901f6a880d8ca2bad50d71c0915abdfd0c  Prismattyc-v0.2.21-macos-universal.dmg
993b13dddccad4f3c494b60e39fd58b35de4d9b654f6b64ac1c73d361be9a454  Prismattyc-v0.2.21-macos-universal.zip
";
        assert_eq!(
            sha256_entry(sums, "Prismattyc-v0.2.21-macos-universal.zip").unwrap(),
            "993b13dddccad4f3c494b60e39fd58b35de4d9b654f6b64ac1c73d361be9a454"
        );
        let mut zip = named_asset("v0.2.21", "Prismattyc-v0.2.21-macos-universal.zip");
        zip.digest =
            Some("sha256:993b13dddccad4f3c494b60e39fd58b35de4d9b654f6b64ac1c73d361be9a454".into());
        let version = Version::new(0, 2, 21);
        confirm_macos_zip(&zip, sums, None, &version, "aarch64-apple-darwin").unwrap();
        let manifest = r#"{
            "repository": "moonbase2090/Prismattyc",
            "version": "0.2.21",
            "target": "universal-apple-darwin",
            "assets": [{
                "name": "Prismattyc-v0.2.21-macos-universal.zip",
                "size": 26836143,
                "sha256": "993b13dddccad4f3c494b60e39fd58b35de4d9b654f6b64ac1c73d361be9a454"
            }]
        }"#;
        confirm_macos_zip(
            &zip,
            sums,
            Some(manifest.as_bytes()),
            &version,
            "aarch64-apple-darwin",
        )
        .unwrap();
        let bad_sums = sums.replace(
            "993b13dddccad4f3c494b60e39fd58b35de4d9b654f6b64ac1c73d361be9a454",
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let error = confirm_macos_zip(&zip, &bad_sums, None, &version, "aarch64-apple-darwin")
            .unwrap_err()
            .to_string();
        assert!(error.contains("SHA256SUMS-macos"), "{error}");
        assert!(
            error.contains("nothing was installed") || error.contains("Nothing was installed"),
            "{error}"
        );
        assert!(
            error.contains("Prismattyc-v0.2.21-macos-universal.dmg"),
            "{error}"
        );
        let bad_manifest = manifest.replace(
            "993b13dddccad4f3c494b60e39fd58b35de4d9b654f6b64ac1c73d361be9a454",
            "1111111111111111111111111111111111111111111111111111111111111111",
        );
        let error = confirm_macos_zip(
            &zip,
            sums,
            Some(bad_manifest.as_bytes()),
            &version,
            "x86_64-apple-darwin",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("manifest-macos-universal.json"), "{error}");
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases/download/v0.2.21/Prismattyc-v0.2.21-macos-universal.zip"),
            "{error}"
        );
    }

    #[test]
    fn app_bundle_path_prefers_the_running_prismattyc_app() {
        let inside = Path::new("/Applications/Prismattyc.app/Contents/MacOS/pmux");
        assert_eq!(
            app_bundle_from_executable(inside).unwrap(),
            Path::new("/Applications/Prismattyc.app")
        );
        assert!(app_bundle_from_executable(Path::new("/usr/local/bin/pmux")).is_none());
        let home = Path::new("/Users/example");
        let user_exe = Path::new("/Users/example/Applications/Prismattyc.app/Contents/MacOS/pmux");
        assert_eq!(
            preferred_macos_app(Some(user_exe), Some(home), true, true),
            Path::new("/Users/example/Applications/Prismattyc.app")
        );
        assert_eq!(
            preferred_macos_app(
                Some(Path::new("/usr/local/bin/pmux")),
                Some(home),
                true,
                true
            ),
            Path::new("/Applications/Prismattyc.app")
        );
        assert_eq!(
            preferred_macos_app(None, Some(home), false, true),
            home.join("Applications/Prismattyc.app")
        );
        assert_eq!(
            preferred_macos_app(None, Some(home), false, false),
            Path::new("/Applications/Prismattyc.app")
        );
    }

    fn write_fake_app(path: &Path, marker: &str) {
        let macos = path.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::write(macos.join("pmux"), marker).unwrap();
    }

    #[test]
    fn app_swap_restores_until_commit_then_discards_the_old_app() {
        let dir = temporary();
        let app = dir.join("Prismattyc.app");
        write_fake_app(&app, "old");
        let incoming = dir.join(".Prismattyc.app.incoming");
        write_fake_app(&incoming, "new");
        let displaced = replace_app_bundle(&app, &incoming).unwrap().unwrap();
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"new");
        assert_eq!(
            fs::read(displaced.join("Contents/MacOS/pmux")).unwrap(),
            b"old"
        );
        assert!(!dir.join("Prismattyc.app.previous").exists());
        restore_displaced_app(&app, &displaced).unwrap();
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"old");
        assert!(!displaced.exists());
        let again = dir.join(".Prismattyc.app.incoming-again");
        write_fake_app(&again, "final");
        let displaced = replace_app_bundle(&app, &again).unwrap().unwrap();
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"final");
        discard_replaced_app(&displaced).unwrap();
        assert!(!displaced.exists());
        assert!(!dir.join("Prismattyc.app.previous").exists());
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"final");
        let fresh = dir.join("missing");
        fs::create_dir_all(&fresh).unwrap();
        let created = fresh.join("Prismattyc.app");
        let staged = fresh.join(".incoming");
        write_fake_app(&staged, "first");
        assert!(replace_app_bundle(&created, &staged).unwrap().is_none());
        assert_eq!(
            fs::read(created.join("Contents/MacOS/pmux")).unwrap(),
            b"first"
        );
        assert!(replace_app_bundle(&dir.join("not-the-app"), &created).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn macos_rollback_explains_reinstall_from_the_dmg() {
        let error = macos_rollback_error();
        assert!(
            error.contains("rollback is not supported on macOS"),
            "{error}"
        );
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases"),
            "{error}"
        );
        assert!(error.contains("DMG"), "{error}");
        assert!(error.contains("/Applications/Prismattyc.app"), "{error}");
    }

    #[test]
    fn leftover_old_app_notice_names_the_path_and_how_to_remove_it() {
        let path = Path::new("/Applications/.Prismattyc.app.displaced-9");
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let notice = leftover_app_notice(path, &error);
        assert!(notice.contains(path.to_str().unwrap()), "{notice}");
        assert!(notice.contains("permission denied"), "{notice}");
        assert!(notice.contains("rm -rf"), "{notice}");
        assert!(
            notice.contains("The new Prismattyc.app is installed"),
            "{notice}"
        );
    }
}
