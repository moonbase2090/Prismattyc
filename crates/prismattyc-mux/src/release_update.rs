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
    pub body: Option<String>,
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
    /// Include immutable prereleases. The app menu never sets this.
    pre: bool,
    bin_dir: Option<PathBuf>,
}

fn options(args: &[String]) -> Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        apply_update_option(&mut options, arg, &mut args)?;
    }
    ensure!(
        !(options.check && options.rollback),
        "--check and --rollback cannot be combined"
    );
    Ok(options)
}

/// Apply one CLI flag to `options`, consuming a value from `args` when the
/// flag needs one. Split out of [`options`] so each function stays within the
/// CRAP complexity budget.
fn apply_update_option<'a>(
    options: &mut Options,
    arg: &str,
    args: &mut impl Iterator<Item = &'a String>,
) -> Result<()> {
    if set_update_flag(options, arg) {
        return Ok(());
    }
    if arg == "--bin-dir" {
        options.bin_dir = Some(next_bin_dir(args)?);
        return Ok(());
    }
    bail!("unknown release update option {arg:?}; use --help (development builds: --source)")
}

/// The directory argument to `--bin-dir`.
fn next_bin_dir<'a>(args: &mut impl Iterator<Item = &'a String>) -> Result<PathBuf> {
    Ok(PathBuf::from(
        args.next().context("--bin-dir needs a directory")?,
    ))
}

/// Set a boolean flag; returns `true` when `arg` was a recognized boolean.
/// `--all` is accepted and ignored (the release path always installs all).
fn set_update_flag(options: &mut Options, arg: &str) -> bool {
    match arg {
        "--check" => options.check = true,
        "--json" => options.json = true,
        "--rollback" => options.rollback = true,
        "--pre" => options.pre = true,
        "--all" => {}
        _ => return false,
    }
    true
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

/// Where an existing app lives, or `/Applications/Prismattyc.app` for a first install.
/// An executable inside `Prismattyc.app` wins, then an existing system app,
/// then an existing user app. Writability is decided by `plan_macos_install`.
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
fn installed_app_exists(
    current_exe: Option<&Path>,
    home: Option<&Path>,
    system_exists: bool,
    user_exists: bool,
    selected: &Path,
) -> bool {
    if current_exe
        .and_then(app_bundle_from_executable)
        .is_some_and(|bundle| bundle == selected)
    {
        return true;
    }
    if selected == Path::new("/Applications/Prismattyc.app") {
        return system_exists;
    }
    home.is_some_and(|dir| selected == dir.join("Applications/Prismattyc.app")) && user_exists
}

/// Detect the app bundle path separately from checking for its executable.
/// An incomplete existing bundle must not be treated as a first install.
#[cfg(any(test, target_os = "macos"))]
fn app_bundle_path_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(any(test, target_os = "macos"))]
fn unwritable_installed_app(app: &Path, parent: &Path) -> String {
    format!(
        "could not update {} because {} is not writable. Staging the replacement needs a sibling in that folder, and this account cannot create one. The installed app was not changed. No second copy was installed. Run the update with admin rights, or move Prismattyc.app to ~/Applications and run pmux update again.",
        app.display(),
        parent.display()
    )
}

/// Choose the app to replace. An existing app whose folder is not writable is an
/// error. A first install may use `~/Applications` when `/Applications` is not writable.
#[cfg(any(test, target_os = "macos"))]
fn plan_macos_install(
    current_exe: Option<&Path>,
    home: Option<&Path>,
    system_exists: bool,
    user_exists: bool,
    writable: impl Fn(&Path) -> bool,
) -> Result<PathBuf> {
    let selected = preferred_macos_app(current_exe, home, system_exists, user_exists);
    let parent = selected.parent().context("Prismattyc.app parent")?;
    if installed_app_exists(current_exe, home, system_exists, user_exists, &selected) {
        ensure!(
            writable(parent),
            "{}",
            unwritable_installed_app(&selected, parent)
        );
        return Ok(selected);
    }
    if writable(parent) {
        return Ok(selected);
    }
    if let Some(home) = home {
        return Ok(home.join("Applications/Prismattyc.app"));
    }
    bail!("could not find a writable location for Prismattyc.app")
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

/// Stable updates reject prereleases. `--pre` still rejects drafts and
/// releases that are not immutable.
fn accepted_version(release: &Release, include_prerelease: bool) -> Result<Version> {
    let raw = release
        .tag_name
        .strip_prefix('v')
        .context("release tag must start with v")?;
    let version = Version::parse(raw)?;
    ensure!(
        version >= Version::new(0, 2, 0),
        "the release channel starts at 0.2.0"
    );
    if include_prerelease {
        ensure!(!release.draft, "draft releases are not installed");
    } else {
        ensure!(
            version.pre.is_empty()
                && version.build.is_empty()
                && !release.draft
                && !release.prerelease,
            "only stable published releases are accepted"
        );
    }
    ensure!(
        release.immutable,
        "release must be immutable before Update can install it"
    );
    Ok(version)
}

fn best_release_index(releases: &[Release], include_prerelease: bool) -> Result<usize> {
    let mut best: Option<(usize, Version)> = None;
    for (index, release) in releases.iter().enumerate() {
        let Ok(version) = accepted_version(release, include_prerelease) else {
            continue;
        };
        match &best {
            Some((_, current)) if version <= *current => {}
            _ => best = Some((index, version)),
        }
    }
    best.map(|(index, _)| index).with_context(|| {
        format!(
            "no usable release from {REPOSITORY}; releases start at 0.2.0. Nothing was installed"
        )
    })
}

fn take_best_release(mut releases: Vec<Release>, include_prerelease: bool) -> Result<Release> {
    let index = best_release_index(&releases, include_prerelease)?;
    Ok(releases.swap_remove(index))
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
    let bytes = fetch_release_json(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases/latest"
    ))?;
    serde_json::from_slice(&bytes).context("parse release metadata")
}

fn listed_releases() -> Result<Vec<Release>> {
    let bytes = fetch_release_json(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases?per_page=30"
    ))?;
    serde_json::from_slice(&bytes).context("parse release list")
}

fn fetch_release_json(url: &str) -> Result<Vec<u8>> {
    let mut child = curl()
        .args([
            "--header",
            "Accept: application/vnd.github+json",
            "--max-filesize",
            "4194304",
        ])
        .arg(url)
        .stdout(Stdio::piped())
        .spawn()
        .context("start HTTPS download (curl is required)")?;
    read_capped_child_stdout(&mut child, 4_194_304)
}

/// Read up to `cap` bytes from a spawned child's stdout, killing it on
/// overflow or error, and fail unless it exited cleanly within the cap.
/// Split out of [`latest_release`] to keep each function's CRAP low.
fn read_capped_child_stdout(child: &mut std::process::Child, cap: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .context("download stdout")?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes);
    let overflowed = result.is_err() || bytes.len() > cap;
    if overflowed {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    ensure!(
        status.success() && bytes.len() <= cap,
        "no usable release from {REPOSITORY}; releases start at 0.2.0. Nothing was installed"
    );
    Ok(bytes)
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

/// Login services keep the stable launcher path across release activation.
/// Development builds use their own executable and never select another install.
pub fn login_launcher() -> Result<PathBuf> {
    let current = std::env::current_exe()?.canonicalize()?;
    if let Ok(root) = root() {
        #[cfg(windows)]
        if root.join("windows-current.json").is_file() {
            return Ok(windows_update::load(&root)?.bin_dir.join("pmux.exe"));
        }
        if let Ok(directory) = default_bin_dir(&root) {
            let candidate = directory.join(crate::platform::executable_name("pmux"));
            if candidate.canonicalize().ok().as_ref() == Some(&current) {
                return Ok(candidate);
            }
        }
    }
    Ok(current)
}

/// On macOS, create the app-bundled `pmux` PATH link when the current process
/// belongs to `Prismattyc.app`. Other launches do not create a link.
#[cfg(target_os = "macos")]
pub fn ensure_pmux_path_shim_for_current_app() -> Result<()> {
    crate::path_shim::ensure_current_app()
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
#[cfg_attr(target_os = "macos", allow(dead_code))]
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
    if args.iter().any(is_help_flag) {
        print_update_help();
        return Ok(());
    }
    let options = options(args)?;
    let root = root()?;
    run_locked(&root, &options)
}

/// Take the update lock, then roll back or run the update flow. Split out of
/// [`run`] so each function stays within the CRAP budget.
fn run_locked(root: &Path, options: &Options) -> Result<()> {
    let _lock = lock(root)?;
    if options.rollback {
        return run_rollback(root);
    }
    run_update_flow(root, options)
}

/// True for `--help` / `-h`.
fn is_help_flag(arg: &String) -> bool {
    arg == "--help" || arg == "-h"
}

fn print_update_help() {
    println!("pmux update [--check] [--json] [--pre] [--bin-dir PATH]\nprismattyc update [--check] [--json] [--pre] [--bin-dir PATH]\npmux update --rollback\npmux update --source [--host|--mux|--all]\n\nDownload a complete stable release from {REPOSITORY} (0.2.0 onward).\n--pre includes immutable prereleases such as v0.3.0-rc.3. The default channel and the app menu stay on stable releases such as v0.3.0.\nVerify immutable release metadata, asset sizes, and SHA-256 digests.\nOn Linux and Windows, stage all six binaries, then activate them together. --rollback restores that previous installation.\nOn macOS, verify SHA256SUMS-macos, Developer ID Team ID S24C53PD3Y, and Gatekeeper notarization. Keep the previous verified app for --rollback.\nprismattyc update is the same command as pmux update.\nUpdating does not stop sessions. The app menu restarts the host and safely restarts the daemon when it has no active sessions.\n--source is an explicit development-only source build.");
}

/// Fetch the latest release, compare versions, and either report or install.
/// Split out of [`run`] so each function stays within the CRAP budget.
fn run_update_flow(root: &Path, options: &Options) -> Result<()> {
    let release = if options.pre {
        take_best_release(listed_releases()?, true)?
    } else {
        latest_release()?
    };
    let target = target()?;
    let plan = select_plan(&release, target)?;
    finish_update_flow(root, options, &release, target, plan)
}

/// Resolve versions and either report or install. Split out of
/// [`run_update_flow`] so each function stays within the CRAP budget.
fn finish_update_flow(
    root: &Path,
    options: &Options,
    release: &Release,
    target: &'static str,
    plan: UpdatePlan,
) -> Result<()> {
    let resolved = resolve_versions(root, release, &plan, options.pre)?;
    if should_only_report(options, &resolved.available, &resolved.current) {
        report_check(
            &resolved.installed,
            &resolved.available,
            &resolved.current,
            options.json,
            release.body.as_deref(),
        );
        return Ok(());
    }
    run_install(
        root,
        release,
        &resolved.available,
        target,
        plan,
        options.json,
        options.bin_dir.as_deref(),
    )
}

/// The installed label and the available/current versions for a plan.
struct ResolvedVersions {
    installed: String,
    available: Version,
    current: Version,
}

/// Resolve the available release version and the installed version. Split out
/// of [`run_update_flow`] to keep each function within the CRAP budget.
/// `include_prerelease` is set only for `pmux update --pre`.
fn resolve_versions(
    root: &Path,
    release: &Release,
    plan: &UpdatePlan,
    include_prerelease: bool,
) -> Result<ResolvedVersions> {
    let available = accepted_version(release, include_prerelease)?;
    let installed = installed_label(root, matches!(plan, UpdatePlan::MacosBundle { .. }))?;
    let current = Version::parse(&installed)?;
    Ok(ResolvedVersions {
        installed,
        available,
        current,
    })
}

/// Whether the run should stop at a report: `--check`, or already current.
fn should_only_report(options: &Options, available: &Version, current: &Version) -> bool {
    options.check || available <= current
}

/// Roll back the previous installation. Split out of [`run`] to keep its
/// complexity low.
fn run_rollback(root: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        run_macos_rollback(root)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let restored = rollback(root)?;
        println!(
            "{}",
            serde_json::json!({"status":"rolled_back","version":restored,"restart_required":true})
        );
        Ok(())
    }
}

/// Print the check / up-to-date report in JSON or human form.
fn report_check(
    installed: &str,
    version: &Version,
    current: &Version,
    json: bool,
    release_notes: Option<&str>,
) {
    let update_available = version > current;
    if json {
        println!(
            "{}",
            serde_json::json!({"repository":REPOSITORY,"installed":installed,"available":version.to_string(),"update_available":update_available,"release_notes":release_notes.unwrap_or_default(),"status":"checked"})
        );
        return;
    }
    let hint = if update_available {
        "Run pmux update or prismattyc update to install."
    } else {
        "You are up to date."
    };
    println!("Installed: {installed}\nAvailable: {version}\nSource: {REPOSITORY}\n{hint}");
}

/// Dispatch the selected install plan. Split out of [`run`] so both stay
/// within the CRAP budget.
#[allow(clippy::too_many_arguments)]
fn run_install(
    root: &Path,
    release: &Release,
    version: &Version,
    target: &'static str,
    plan: UpdatePlan,
    json: bool,
    bin_dir: Option<&Path>,
) -> Result<()> {
    match plan {
        UpdatePlan::MacosBundle {
            zip,
            checksums,
            manifest,
        } => install_macos_bundle(
            root,
            MacosDownload {
                tag: &release.tag_name,
                version,
                target,
                zip,
                checksums,
                manifest,
                json,
            },
        ),
        UpdatePlan::Binaries(assets) => {
            install_binaries(root, bin_dir, version, target, &assets, json)
        }
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
        if is_abandoned_stage(&entry)? {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

/// True when `entry` is a leftover `.stage-<pid>` directory. Split out of
/// [`clear_abandoned_stages`] to keep each function's CRAP low.
fn is_abandoned_stage(entry: &fs::DirEntry) -> Result<bool> {
    let name = entry.file_name();
    let looks_like_stage = name
        .to_str()
        .and_then(|entry_name| entry_name.strip_prefix(".stage-"))
        .is_some_and(is_pid_suffix);
    Ok(looks_like_stage && entry.file_type()?.is_dir())
}

/// A non-empty, all-ASCII-digit process-id suffix.
fn is_pid_suffix(pid: &str) -> bool {
    !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())
}

fn install_binaries(
    root: &Path,
    bin_dir_override: Option<&Path>,
    version: &Version,
    target: &str,
    assets: &[&Asset],
    json: bool,
) -> Result<()> {
    let bin_dir = resolve_bin_dir(root, bin_dir_override)?;
    let directory_name = versioned_dir_name(version, target)?;
    let directory = root.join(&directory_name);
    // The update lock proves no other updater owns these abandoned downloads.
    clear_abandoned_stages(root)?;
    let staging = root.join(format!(".stage-{}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = stage_and_activate(
        root,
        &staging,
        &directory,
        &directory_name,
        &bin_dir,
        version,
        target,
        assets,
    );
    finish_install(&staging, result, version, json)
}

/// The managed bin dir: an explicit override, else the platform default.
fn resolve_bin_dir(root: &Path, bin_dir_override: Option<&Path>) -> Result<PathBuf> {
    match bin_dir_override {
        Some(path) => Ok(path.to_path_buf()),
        None => default_bin_dir(root),
    }
}

/// The `v<version>-<target>-<nonce>` install directory name.
fn versioned_dir_name(version: &Version, target: &str) -> Result<String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    Ok(format!("v{version}-{target}-{nonce}"))
}

/// Remove the staging dir, propagate the staged result, then report success.
/// Split out of [`install_binaries`] to keep each function's CRAP low.
fn finish_install(staging: &Path, result: Result<()>, version: &Version, json: bool) -> Result<()> {
    if staging.exists() {
        let _ = fs::remove_dir_all(staging);
    }
    result?;
    report_install(version, json);
    Ok(())
}

/// Download, verify, receipt, and activate the staged binary set. Split out of
/// [`install_binaries`] so each function stays within the CRAP budget.
#[allow(clippy::too_many_arguments)]
fn stage_and_activate(
    root: &Path,
    staging: &Path,
    directory: &Path,
    directory_name: &str,
    bin_dir: &Path,
    version: &Version,
    target: &str,
    assets: &[&Asset],
) -> Result<()> {
    download_binary_set(staging, version, assets)?;
    verify_staged_versions(staging, version)?;
    write_receipt(
        staging,
        &Receipt {
            repository: REPOSITORY.into(),
            version: version.to_string(),
            target: target.into(),
            bin_dir: bin_dir.to_path_buf(),
        },
    )?;
    fs::rename(staging, directory)?;
    activate(root, Path::new(directory_name), bin_dir)
}

/// Download each binary into `staging`, verifying size, digest, and mode.
fn download_binary_set(staging: &Path, version: &Version, assets: &[&Asset]) -> Result<()> {
    for (binary, asset) in BINARIES.iter().zip(assets) {
        eprintln!("Downloading {binary} {version}");
        download_one_binary(
            &staging.join(crate::platform::executable_name(binary)),
            binary,
            asset,
        )?;
    }
    Ok(())
}

/// Download, verify, and mark one binary executable. Split out of
/// [`download_binary_set`] to keep each function's CRAP low.
fn download_one_binary(path: &Path, binary: &str, asset: &Asset) -> Result<()> {
    fetch_binary(path, binary, asset)?;
    finalize_binary(path, asset)
}

/// Curl one asset to `path`, failing if the download did not succeed.
fn fetch_binary(path: &Path, binary: &str, asset: &Asset) -> Result<()> {
    let status = curl()
        .arg("--max-filesize")
        .arg(MAX_ASSET.to_string())
        .arg("--output")
        .arg(path)
        .arg(&asset.browser_download_url)
        .status()?;
    ensure!(
        status.success(),
        "download failed for {binary}; installed version unchanged"
    );
    Ok(())
}

/// Verify the digest, then set mode and fsync a downloaded binary.
fn finalize_binary(path: &Path, asset: &Asset) -> Result<()> {
    verify(path, asset)?;
    crate::platform::set_mode(path, 0o755)?;
    crate::platform::sync_file(path)?;
    Ok(())
}

/// The complete set is trusted before any downloaded program executes.
fn verify_staged_versions(staging: &Path, version: &Version) -> Result<()> {
    for binary in BINARIES {
        let text = version_label(&staging.join(crate::platform::executable_name(binary)))?;
        ensure!(
            text.split_whitespace()
                .any(|word| word == version.to_string()),
            "{binary} did not report release version {version}"
        );
    }
    Ok(())
}

/// Print the post-install report in JSON or human form.
fn report_install(version: &Version, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({"status":"installed","version":version.to_string(),"repository":REPOSITORY,"restart_required":true})
        );
    } else {
        println!("Installed {version} from {REPOSITORY}. Running components keep their current version.\nUse pmux restart to review and apply component restarts. Use pmux update --rollback or prismattyc update --rollback to restore the previous installation.");
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn macos_rollback_error() -> String {
    format!(
        "no previous verified Prismattyc.app is available to roll back. Download a release DMG from https://github.com/{REPOSITORY}/releases to install a specific version."
    )
}

#[cfg(target_os = "macos")]
#[derive(Debug, Serialize, Deserialize)]
struct MacosAppState {
    repository: String,
    version: String,
    target: String,
    app: PathBuf,
    #[serde(default)]
    previous_app: Option<PathBuf>,
    #[serde(default)]
    previous_version: Option<String>,
}

#[cfg(target_os = "macos")]
fn macos_state_path(root: &Path) -> PathBuf {
    root.join("macos-app.json")
}

#[cfg(target_os = "macos")]
fn write_macos_state(root: &Path, state: &MacosAppState) -> Result<()> {
    let path = macos_state_path(root);
    let temporary = root.join(format!("macos-app.json.{}.tmp", std::process::id()));
    write_json_atomic(&temporary, &path, &serde_json::to_vec_pretty(state)?)?;
    let _ = File::open(root).and_then(|dir| dir.sync_all());
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_macos_state(root: &Path) -> Result<MacosAppState> {
    let path = macos_state_path(root);
    serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("parse macOS app state at {}", path.display()))
}

#[cfg(target_os = "macos")]
fn run_macos_rollback(root: &Path) -> Result<()> {
    let state = read_macos_state(root).context("no macOS update state is available")?;
    let previous = state
        .previous_app
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!(macos_rollback_error()))?;
    let previous_version = Version::parse(
        state
            .previous_version
            .as_deref()
            .context("the previous app version was not recorded")?,
    )?;
    let tag = format!("v{previous_version}");
    verify_macos_trust(previous, &tag, MACOS_BUNDLE_TARGET)?;
    reported_release_version(&previous.join("Contents/MacOS/pmux"), &previous_version)?;

    let swap = replace_app_bundle(&state.app, previous)?;
    let verify = verify_macos_trust(&state.app, &tag, MACOS_BUNDLE_TARGET).and_then(|()| {
        reported_release_version(&state.app.join("Contents/MacOS/pmux"), &previous_version)
    });
    recover_or_fail(&state.app, &swap, verify)?;
    let rolled_back = write_macos_state(
        root,
        &MacosAppState {
            repository: REPOSITORY.into(),
            version: previous_version.to_string(),
            target: MACOS_BUNDLE_TARGET.into(),
            app: state.app.clone(),
            previous_app: None,
            previous_version: None,
        },
    );
    recover_or_fail(&state.app, &swap, rolled_back)?;
    let leftover = discard_displaced(swap.displaced.as_deref());
    println!(
        "{}",
        serde_json::json!({"status":"rolled_back","version":previous_version.to_string(),"restart_required":true,"app":state.app,"leftover_app":leftover})
    );
    Ok(())
}

/// Write `bytes` to `temporary`, fsync, then rename onto `path`. Split out of
/// [`write_macos_state`] to keep each function within the CRAP budget.
#[cfg(target_os = "macos")]
fn write_json_atomic(temporary: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = crate::platform::private_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
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

#[cfg(any(test, target_os = "macos"))]
struct ReplacedApp {
    displaced: Option<PathBuf>,
    directory_sync_warning: Option<String>,
}

#[cfg(any(test, target_os = "macos"))]
fn parent_sync_warning(parent: &Path, error: &dyn std::fmt::Display, had_previous: bool) -> String {
    let next = if had_previous {
        "The update will continue, check the new app, and then remove the old one."
    } else {
        "The update will continue and check the new app."
    };
    format!(
        "Saving the folder {} failed after Prismattyc.app was replaced: {error}. The new app is already in that folder. {next}",
        parent.display()
    )
}

/// Move `incoming` onto `destination` on the same volume. When an app was
/// already installed, it is left at `displaced` until the caller deletes it.
/// A failed swap puts that app back at `destination`. A failed parent-directory
/// sync after a successful swap is reported and does not drop `displaced`.
#[cfg(any(test, target_os = "macos"))]
fn replace_app_bundle(destination: &Path, incoming: &Path) -> Result<ReplacedApp> {
    replace_app_bundle_syncing(destination, incoming, sync_parent)
}

#[cfg(any(test, target_os = "macos"))]
fn replace_app_bundle_syncing(
    destination: &Path,
    incoming: &Path,
    sync_dir: impl Fn(&Path) -> Result<()>,
) -> Result<ReplacedApp> {
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
        let directory_sync_warning = match sync_dir(&parent) {
            Ok(()) => None,
            Err(error) => Some(parent_sync_warning(&parent, &error, false)),
        };
        return Ok(ReplacedApp {
            displaced: None,
            directory_sync_warning,
        });
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
    let directory_sync_warning = match sync_dir(&parent) {
        Ok(()) => None,
        Err(error) => Some(parent_sync_warning(&parent, &error, true)),
    };
    Ok(ReplacedApp {
        displaced: Some(displaced),
        directory_sync_warning,
    })
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
    move_failed_install_aside(destination, &holding)?;
    swap_previous_app_back(displaced, destination, &holding)?;
    remove_failed_copy(destination, &holding)?;
    sync_restored_parent(destination, &parent)
}

/// Move a failed install at `destination` into `holding`, clearing any prior
/// holding dir first. Split out of [`restore_displaced_app`] for CRAP budget.
#[cfg(any(test, target_os = "macos"))]
fn move_failed_install_aside(destination: &Path, holding: &Path) -> Result<()> {
    if holding.exists() {
        fs::remove_dir_all(holding)?;
    }
    if destination.exists() {
        fs::rename(destination, holding).with_context(|| {
            format!(
                "could not move the failed install aside at {}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

/// Move the displaced previous app back to `destination`; on failure, put the
/// failed install back from `holding` and report both errors.
#[cfg(any(test, target_os = "macos"))]
fn swap_previous_app_back(displaced: &Path, destination: &Path, holding: &Path) -> Result<()> {
    let Err(error) = fs::rename(displaced, destination) else {
        return Ok(());
    };
    if holding.exists() {
        if let Err(restore) = fs::rename(holding, destination) {
            bail!(
                "could not restore the previous app ({error}) and putting the new app back also failed ({restore}). The previous app is at {}.",
                displaced.display()
            );
        }
    }
    Err(error).context(format!(
        "could not move the previous app back to {}. It is still at {}",
        destination.display(),
        displaced.display()
    ))
}

/// Remove the held failed copy once the previous app is restored.
#[cfg(any(test, target_os = "macos"))]
fn remove_failed_copy(destination: &Path, holding: &Path) -> Result<()> {
    if holding.exists() {
        fs::remove_dir_all(holding).with_context(|| {
            format!(
                "the previous app is restored at {}, but the failed copy remains at {}",
                destination.display(),
                holding.display()
            )
        })?;
    }
    Ok(())
}

/// fsync the parent after a restore, reporting a clear message on failure.
#[cfg(any(test, target_os = "macos"))]
fn sync_restored_parent(destination: &Path, parent: &Path) -> Result<()> {
    if let Err(error) = sync_parent(parent) {
        bail!(
            "the previous app is restored at {}, but saving {} failed: {error}",
            destination.display(),
            parent.display()
        );
    }
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
        "The new Prismattyc.app is installed, but the old app is still at {}. It was left there because deleting it failed: {error}. Remove that directory yourself with `rm -rf '{}'`.",
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

/// Locate the installed app for a version check.
/// Install selection goes through `plan_macos_install`, which refuses an
/// unwritable folder instead of choosing a different copy.
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
    let exe = std::env::current_exe().ok();
    let home = crate::platform::home_dir().map(PathBuf::from);
    let system = PathBuf::from("/Applications/Prismattyc.app");
    let user = home
        .as_ref()
        .map(|dir| dir.join("Applications/Prismattyc.app"));
    let system_installed = app_bundle_path_exists(&system);
    let user_installed = user
        .as_ref()
        .is_some_and(|path| app_bundle_path_exists(path));
    let destination = plan_macos_install(
        exe.as_deref(),
        home.as_deref(),
        system_installed,
        user_installed,
        directory_writable,
    )
    .map_err(|error| anyhow::anyhow!("{error} {}", manual_update_instructions(tag, target)))?;
    require_app_destination(&destination)?;
    ensure_parent_dir(&destination)?;
    Ok(destination)
}

/// Create the parent directory of `destination` when the app does not yet
/// exist. Split out of [`choose_macos_app`] to keep its CRAP low.
#[cfg(target_os = "macos")]
fn ensure_parent_dir(destination: &Path) -> Result<()> {
    if destination.exists() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
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
    let identity = Command::new("codesign")
        .args(["--display", "--verbose=4"])
        .arg(app)
        .output()
        .context("codesign is required to inspect Prismattyc.app identity")?;
    let identity_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&identity.stdout),
        String::from_utf8_lossy(&identity.stderr)
    );
    ensure!(
        identity.status.success()
            && identity_text.lines().any(|line| {
                line.trim() == "TeamIdentifier=S24C53PD3Y"
            }),
        "{} is not signed by Prismattyc Developer ID Team S24C53PD3Y. The installed app was not changed. {}",
        app.display(),
        manual_update_instructions(tag, target)
    );
    let gatekeeper = Command::new("spctl")
        .args(["--assess", "--verbose=4", "--type", "exec"])
        .arg(app)
        .output()
        .context("spctl is required to assess Prismattyc.app")?;
    let assessment = format!(
        "{}\n{}",
        String::from_utf8_lossy(&gatekeeper.stdout),
        String::from_utf8_lossy(&gatekeeper.stderr)
    );
    ensure!(
        gatekeeper.status.success() && assessment.contains("source=Notarized Developer ID"),
        "spctl rejected {} ({}). Gatekeeper must report a notarized Developer ID app. {}",
        app.display(),
        assessment.trim(),
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
    let result = install_macos_bundle_steps(
        root,
        &staging,
        &destination,
        &mut incoming,
        MacosDownload {
            tag,
            version,
            target,
            zip,
            checksums,
            manifest,
            json,
        },
    );
    cleanup_macos_staging(&staging, incoming.as_deref());
    result
}

/// Remove the staging dir and any leftover incoming bundle after an install
/// attempt. Split out of [`install_macos_bundle_here`] to keep its CRAP low.
#[cfg(target_os = "macos")]
fn cleanup_macos_staging(staging: &Path, incoming: Option<&Path>) {
    if staging.exists() {
        let _ = fs::remove_dir_all(staging);
    }
    if let Some(path) = incoming {
        if path.exists() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Download, verify, stage, swap, and report a macOS bundle install. On
/// success `incoming` is cleared; on an early failure it names the staged
/// bundle so the caller can clean it up. Split out of
/// [`install_macos_bundle_here`] so each function stays within the CRAP budget.
#[cfg(target_os = "macos")]
fn install_macos_bundle_steps(
    root: &Path,
    staging: &Path,
    destination: &Path,
    incoming: &mut Option<PathBuf>,
    download: MacosDownload<'_>,
) -> Result<()> {
    let MacosDownload {
        tag,
        version,
        target,
        zip,
        checksums,
        manifest,
        json,
    } = download;
    let extracted =
        download_and_extract_macos_zip(staging, zip, checksums, manifest, version, tag, target)?;
    let staged = stage_macos_bundle(destination, &extracted, tag, target)?;
    *incoming = Some(staged.clone());
    let swap = replace_app_bundle(destination, &staged)?;
    *incoming = None;
    let leftover = verify_and_record_swap(root, destination, version, tag, target, &swap)?;
    if let Err(error) = crate::path_shim::ensure_app_bundle(destination) {
        eprintln!("pmux update: warning: could not put the app-bundled pmux on PATH: {error:#}");
    }
    print_macos_install_report(
        destination,
        version,
        json,
        leftover.as_deref(),
        swap.directory_sync_warning.as_deref(),
    );
    Ok(())
}

/// Download the checksums, zip, and optional manifest into `staging`, confirm
/// the zip digest, extract it, clear quarantine, and verify trust + version.
/// Returns the extracted bundle path.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn download_and_extract_macos_zip(
    staging: &Path,
    zip: &Asset,
    checksums: &Asset,
    manifest: Option<&Asset>,
    version: &Version,
    tag: &str,
    target: &str,
) -> Result<PathBuf> {
    let zip_path = download_macos_inputs(staging, zip, checksums, manifest, version)?;
    confirm_and_extract_macos_zip(
        staging, &zip_path, zip, checksums, manifest, version, tag, target,
    )
}

/// Download the checksums, zip, and optional manifest into `staging`.
/// Returns the downloaded zip path.
#[cfg(target_os = "macos")]
fn download_macos_inputs(
    staging: &Path,
    zip: &Asset,
    checksums: &Asset,
    manifest: Option<&Asset>,
    version: &Version,
) -> Result<PathBuf> {
    eprintln!("Downloading {MACOS_CHECKSUMS_NAME} {version}");
    download_asset(checksums, &staging.join(MACOS_CHECKSUMS_NAME))?;
    eprintln!("Downloading {} {version}", zip.name);
    let zip_path = staging.join(&zip.name);
    download_asset(zip, &zip_path)?;
    download_macos_manifest(staging, manifest, version)?;
    Ok(zip_path)
}

/// Confirm the zip digest against the checksums and manifest, extract it,
/// clear quarantine, and verify trust + version. Returns the extracted bundle.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn confirm_and_extract_macos_zip(
    staging: &Path,
    zip_path: &Path,
    zip: &Asset,
    checksums: &Asset,
    manifest: Option<&Asset>,
    version: &Version,
    tag: &str,
    target: &str,
) -> Result<PathBuf> {
    confirm_downloaded_macos_zip(staging, zip, checksums, manifest, version, target)?;
    let extracted = extract_macos_zip(zip_path, &staging.join("unpacked"), tag, target)?;
    clear_quarantine(&extracted);
    verify_extracted_bundle(&extracted, version, tag, target)?;
    Ok(extracted)
}

/// Confirm the downloaded zip's digest against the checksums file and, when
/// present, the manifest. Split out to keep each function's CRAP low.
#[cfg(target_os = "macos")]
fn confirm_downloaded_macos_zip(
    staging: &Path,
    zip: &Asset,
    checksums: &Asset,
    manifest: Option<&Asset>,
    version: &Version,
    target: &str,
) -> Result<()> {
    let manifest_bytes = read_macos_manifest_bytes(staging, manifest)?;
    let sums_text = fs::read_to_string(staging.join(MACOS_CHECKSUMS_NAME))
        .with_context(|| format!("{} is not text", checksums.name))?;
    confirm_macos_zip(zip, &sums_text, manifest_bytes.as_deref(), version, target)
}

/// Verify code signature and reported version of an extracted bundle.
#[cfg(target_os = "macos")]
fn verify_extracted_bundle(
    extracted: &Path,
    version: &Version,
    tag: &str,
    target: &str,
) -> Result<()> {
    verify_macos_trust(extracted, tag, target)?;
    reported_release_version(&extracted.join("Contents/MacOS/pmux"), version)
}

/// Read the manifest bytes previously downloaded into `staging`, if any.
#[cfg(target_os = "macos")]
fn read_macos_manifest_bytes(staging: &Path, manifest: Option<&Asset>) -> Result<Option<Vec<u8>>> {
    match manifest {
        Some(_) => Ok(Some(fs::read(staging.join(MACOS_MANIFEST_NAME))?)),
        None => Ok(None),
    }
}

/// Download the optional manifest asset into `staging` when present.
#[cfg(target_os = "macos")]
fn download_macos_manifest(
    staging: &Path,
    manifest: Option<&Asset>,
    version: &Version,
) -> Result<()> {
    let Some(manifest) = manifest else {
        return Ok(());
    };
    eprintln!("Downloading {MACOS_MANIFEST_NAME} {version}");
    download_asset(manifest, &staging.join(MACOS_MANIFEST_NAME))
}

/// Copy the extracted bundle into a sibling `.Prismattyc.app.incoming-<pid>`
/// of `destination`, then clear quarantine and verify trust. Returns the
/// staged path ready for the atomic swap.
#[cfg(target_os = "macos")]
fn stage_macos_bundle(
    destination: &Path,
    extracted: &Path,
    tag: &str,
    target: &str,
) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .context("Prismattyc.app parent")?
        .to_path_buf();
    fs::create_dir_all(&parent)?;
    let staged = parent.join(format!(".Prismattyc.app.incoming-{}", std::process::id()));
    ditto_copy(extracted, &staged)?;
    clear_quarantine(&staged);
    verify_macos_trust(&staged, tag, target)?;
    Ok(staged)
}

/// Copy `extracted` to `staged` with `ditto`, replacing any prior staged copy.
#[cfg(target_os = "macos")]
fn ditto_copy(extracted: &Path, staged: &Path) -> Result<()> {
    if staged.exists() {
        fs::remove_dir_all(staged)?;
    }
    let copied = Command::new("ditto")
        .arg(extracted)
        .arg(staged)
        .status()
        .context("ditto is required to stage Prismattyc.app")?;
    ensure!(
        copied.success(),
        "staging Prismattyc.app failed ({copied}). The installed app was not changed."
    );
    Ok(())
}

/// After the swap, verify the destination and record macOS state. On failure,
/// restore the displaced app (or remove the destination). Keep the displaced
/// app as the one-step rollback copy.
#[cfg(target_os = "macos")]
fn verify_and_record_swap(
    root: &Path,
    destination: &Path,
    version: &Version,
    tag: &str,
    target: &str,
    swap: &ReplacedApp,
) -> Result<Option<String>> {
    let verify = verify_macos_trust(destination, tag, target)
        .and_then(|()| reported_release_version(&destination.join("Contents/MacOS/pmux"), version));
    recover_or_fail(destination, swap, verify)?;

    let old_state = read_macos_state(root).ok();
    let previous_app = swap.displaced.clone();
    let previous_version = previous_app
        .as_deref()
        .and_then(macos_app_version)
        .map(|version| version.to_string());
    let state = write_macos_state(
        root,
        &MacosAppState {
            repository: REPOSITORY.into(),
            version: version.to_string(),
            target: MACOS_BUNDLE_TARGET.into(),
            app: destination.to_path_buf(),
            previous_app: previous_app.clone(),
            previous_version,
        },
    );
    recover_or_fail(destination, swap, state)?;

    if let Some(old_previous) = old_state
        .and_then(|state| state.previous_app)
        .filter(|path| Some(path) != previous_app.as_ref())
    {
        if let Err(error) = discard_replaced_app(&old_previous) {
            return Ok(Some(leftover_app_notice(&old_previous, &error)));
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn macos_app_version(app: &Path) -> Option<Version> {
    version_label(&app.join("Contents/MacOS/pmux"))
        .ok()?
        .split_whitespace()
        .find_map(|word| Version::parse(word).ok())
}

/// If `outcome` failed, restore the displaced app (or remove the destination)
/// and propagate the error with any sync warning attached.
#[cfg(target_os = "macos")]
fn recover_or_fail(destination: &Path, swap: &ReplacedApp, outcome: Result<()>) -> Result<()> {
    let Err(error) = outcome else {
        return Ok(());
    };
    if let Some(path) = &swap.displaced {
        restore_displaced_app(destination, path).with_context(|| error.to_string())?;
    } else {
        let _ = fs::remove_dir_all(destination);
    }
    match &swap.directory_sync_warning {
        Some(warning) => Err(error.context(warning.clone())),
        None => Err(error),
    }
}

/// Discard the displaced old app; returns a leftover notice if that failed.
#[cfg(target_os = "macos")]
fn discard_displaced(displaced: Option<&Path>) -> Option<String> {
    let path = displaced?;
    match discard_replaced_app(path) {
        Ok(()) => None,
        Err(error) => Some(leftover_app_notice(path, &error)),
    }
}

/// Print the macOS install report in JSON or human form.
#[cfg(target_os = "macos")]
fn print_macos_install_report(
    destination: &Path,
    version: &Version,
    json: bool,
    leftover: Option<&str>,
    sync_warning: Option<&str>,
) {
    let message = macos_install_message(destination, version, leftover, sync_warning);
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
                "directory_sync_warning": sync_warning,
                "message": message,
            })
        );
    } else {
        println!("{message}");
    }
}

/// Build the human-readable install message, including restart guidance and
/// any outside-binary, leftover, and sync notes.
#[cfg(target_os = "macos")]
fn macos_install_message(
    destination: &Path,
    version: &Version,
    leftover: Option<&str>,
    sync_warning: Option<&str>,
) -> String {
    let restart = if app_process_running(destination) {
        "A restart is required. The app menu requests a safe host restart after installation. Existing pmux sessions keep running."
    } else {
        "A restart is required to use this version. Open Prismattyc from the Dock. Existing pmux sessions keep running."
    };
    let outside = if running_inside_bundle(destination) {
        String::new()
    } else {
        " This updated Prismattyc.app. A pmux or prismattyc binary outside that app stays on its current build; open the app to run the new one.".to_string()
    };
    let leftover_text = leftover.map(|text| format!(" {text}")).unwrap_or_default();
    let sync_text = sync_warning
        .map(|text| format!(" {text}"))
        .unwrap_or_default();
    format!(
        "Installed {version} from {REPOSITORY} at {}. {restart}{outside}{leftover_text}{sync_text}",
        destination.display()
    )
}

/// True when the current executable lives inside `destination`.
#[cfg(target_os = "macos")]
fn running_inside_bundle(destination: &Path) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| app_bundle_from_executable(&exe))
        .is_some_and(|bundle| bundle == destination)
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
    fn bare_release(tag: &str, prerelease: bool, draft: bool, immutable: bool) -> Release {
        Release {
            tag_name: tag.into(),
            draft,
            prerelease,
            body: None,
            immutable,
            assets: Vec::new(),
        }
    }

    #[test]
    fn stable_channel_skips_prereleases_and_pre_picks_the_newest() {
        let releases = vec![
            bare_release("v0.3.0-rc.3", true, false, true),
            bare_release("v0.3.0-rc.3", false, false, true),
            bare_release("v0.3.0", false, false, true),
            bare_release("v0.2.30", false, false, true),
            bare_release("v0.2.29", false, false, true),
        ];
        let stable = best_release_index(&releases, false).unwrap();
        assert_eq!(releases[stable].tag_name, "v0.3.0");
        let pre = best_release_index(&releases, true).unwrap();
        assert_eq!(releases[pre].tag_name, "v0.3.0");
        let rc = &releases[0];
        assert!(accepted_version(rc, false).is_err());
        assert_eq!(
            accepted_version(rc, true).unwrap().to_string(),
            "0.3.0-rc.3"
        );
        assert_eq!(
            accepted_version(&releases[stable], false)
                .unwrap()
                .to_string(),
            "0.3.0"
        );
    }

    #[test]
    fn pre_channel_still_skips_drafts_and_mutable_releases() {
        let releases = vec![
            bare_release("v0.3.0-rc.3", true, true, true),
            bare_release("v0.3.0-rc.1", true, false, false),
            bare_release("v0.3.0", false, false, true),
            bare_release("v0.2.30", false, false, true),
        ];
        let pre = best_release_index(&releases, true).unwrap();
        assert_eq!(releases[pre].tag_name, "v0.3.0");
    }

    fn fixture() -> Release {
        Release {
            tag_name: "v0.2.0".into(),
            draft: false,
            prerelease: false,
            body: None,
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
        assert_eq!(
            accepted_version(&release, false).unwrap(),
            Version::new(0, 2, 0)
        );
        release.immutable = false;
        assert!(accepted_version(&release, false).is_err());
        release.immutable = true;
        release.draft = true;
        assert!(accepted_version(&release, false).is_err());
        release.draft = false;
        for version in ["v0.1.999", "v0.2.0-beta.1", "v0.2.0+untrusted", "../../bad"] {
            release.tag_name = version.into();
            assert!(accepted_version(&release, false).is_err());
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
            body: None,
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
        let replaced = replace_app_bundle(&app, &incoming).unwrap();
        assert!(replaced.directory_sync_warning.is_none());
        let displaced = replaced.displaced.unwrap();
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
        let displaced = replace_app_bundle(&app, &again).unwrap().displaced.unwrap();
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
        let created_swap = replace_app_bundle(&created, &staged).unwrap();
        assert!(created_swap.displaced.is_none());
        assert!(created_swap.directory_sync_warning.is_none());
        assert_eq!(
            fs::read(created.join("Contents/MacOS/pmux")).unwrap(),
            b"first"
        );
        assert!(replace_app_bundle(&dir.join("not-the-app"), &created).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unwritable_existing_app_is_not_replaced_with_a_home_copy() {
        let home = Path::new("/Users/example");
        let user_app = home.join("Applications/Prismattyc.app");
        let system = Path::new("/Applications/Prismattyc.app");
        let outside = Path::new("/usr/local/bin/pmux");
        let error = plan_macos_install(Some(outside), Some(home), true, true, |_| false)
            .unwrap_err()
            .to_string();
        assert!(error.contains(system.to_str().unwrap()), "{error}");
        assert!(error.contains("/Applications is not writable"), "{error}");
        assert!(error.contains("admin rights"), "{error}");
        assert!(error.contains("~/Applications"), "{error}");
        assert!(error.contains("No second copy was installed"), "{error}");
        assert!(!error.contains(user_app.to_str().unwrap()), "{error}");
        let inside = Path::new("/Applications/Prismattyc.app/Contents/MacOS/pmux");
        let error = plan_macos_install(Some(inside), Some(home), true, true, |_| false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("/Applications is not writable"), "{error}");
        assert!(
            error.contains("could not update /Applications/Prismattyc.app"),
            "{error}"
        );
        let fresh = plan_macos_install(None, Some(home), false, false, |_| false).unwrap();
        assert_eq!(fresh, user_app);
        let kept = plan_macos_install(Some(outside), Some(home), true, true, |_| true).unwrap();
        assert_eq!(kept, system);
    }

    #[test]
    fn incomplete_existing_bundle_is_not_treated_as_a_first_install() {
        let dir = temporary();
        let incomplete_system_app = dir.join("Applications/Prismattyc.app");
        fs::create_dir_all(&incomplete_system_app).unwrap();
        assert!(app_bundle_path_exists(&incomplete_system_app));
        assert!(!incomplete_system_app.join("Contents/MacOS/pmux").is_file());

        let home = dir.join("home");
        let user_app = home.join("Applications/Prismattyc.app");
        let outside = dir.join("bin/pmux");
        let error = plan_macos_install(
            Some(&outside),
            Some(&home),
            app_bundle_path_exists(&incomplete_system_app),
            app_bundle_path_exists(&user_app),
            |_| false,
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("could not update /Applications/Prismattyc.app"),
            "{error}"
        );
        assert!(error.contains("No second copy was installed"), "{error}");
        assert!(!error.contains(user_app.to_str().unwrap()), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn app_swap_parent_sync_failure_does_not_strand_the_old_app() {
        let dir = temporary();
        let app = dir.join("Prismattyc.app");
        write_fake_app(&app, "old");
        let incoming = dir.join(".Prismattyc.app.incoming");
        write_fake_app(&incoming, "new");
        let replaced =
            replace_app_bundle_syncing(&app, &incoming, |_| bail!("disk sync failed")).unwrap();
        let warning = replaced
            .directory_sync_warning
            .expect("sync failure is reported");
        assert!(warning.contains(&dir.display().to_string()), "{warning}");
        assert!(warning.contains("disk sync failed"), "{warning}");
        assert!(
            warning.contains("The new app is already in that folder"),
            "{warning}"
        );
        assert!(warning.contains("remove the old one"), "{warning}");
        let displaced = replaced.displaced.expect("displaced app stays reachable");
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"new");
        assert_eq!(
            fs::read(displaced.join("Contents/MacOS/pmux")).unwrap(),
            b"old"
        );
        assert!(!dir.join("Prismattyc.app.previous").exists());
        discard_replaced_app(&displaced).unwrap();
        assert!(!displaced.exists());
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"new");

        write_fake_app(&app, "old");
        let incoming = dir.join(".Prismattyc.app.incoming-again");
        write_fake_app(&incoming, "newer");
        let replaced =
            replace_app_bundle_syncing(&app, &incoming, |_| bail!("disk sync failed")).unwrap();
        let displaced = replaced.displaced.unwrap();
        restore_displaced_app(&app, &displaced).unwrap();
        assert_eq!(fs::read(app.join("Contents/MacOS/pmux")).unwrap(), b"old");
        assert!(!displaced.exists());
        assert!(
            !dir.read_dir().unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".Prismattyc.app.displaced-")),
            "old app must not remain at a displaced path"
        );

        let fresh_dir = dir.join("fresh");
        fs::create_dir_all(&fresh_dir).unwrap();
        let created = fresh_dir.join("Prismattyc.app");
        let staged = fresh_dir.join(".incoming");
        write_fake_app(&staged, "first");
        let created_swap =
            replace_app_bundle_syncing(&created, &staged, |_| bail!("disk sync failed")).unwrap();
        assert!(created_swap.displaced.is_none());
        let warning = created_swap.directory_sync_warning.unwrap();
        assert!(warning.contains("disk sync failed"), "{warning}");
        assert_eq!(
            fs::read(created.join("Contents/MacOS/pmux")).unwrap(),
            b"first"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn macos_rollback_explains_when_no_previous_bundle_is_available() {
        let error = macos_rollback_error();
        assert!(
            error.contains("no previous verified Prismattyc.app"),
            "{error}"
        );
        assert!(
            error.contains("https://github.com/moonbase2090/Prismattyc/releases"),
            "{error}"
        );
        assert!(error.contains("DMG"), "{error}");
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
            notice.contains("`rm -rf '/Applications/.Prismattyc.app.displaced-9'`."),
            "{notice}"
        );
        assert!(
            notice.contains("The new Prismattyc.app is installed"),
            "{notice}"
        );
    }

    #[test]
    fn options_parse_flags_and_reject_unknown() {
        let o = options(&["--check".into(), "--json".into()]).unwrap();
        assert!(o.check && o.json && !o.rollback && !o.pre);
        let o = options(&["--pre".into()]).unwrap();
        assert!(o.pre && !o.check);
        let o = options(&["--all".into()]).unwrap();
        assert!(!o.check && !o.json && !o.rollback, "--all is a no-op");
        let o = options(&["--bin-dir".into(), "/tmp/x".into()]).unwrap();
        assert_eq!(o.bin_dir.as_deref(), Some(Path::new("/tmp/x")));
        assert!(options(&["--rollback".into(), "--check".into()]).is_err());
        assert!(options(&["--bin-dir".into()]).is_err());
        assert!(options(&["--nope".into()]).is_err());
    }

    #[test]
    fn set_update_flag_recognizes_booleans() {
        let mut o = Options::default();
        assert!(set_update_flag(&mut o, "--check"));
        assert!(set_update_flag(&mut o, "--json"));
        assert!(set_update_flag(&mut o, "--rollback"));
        assert!(set_update_flag(&mut o, "--pre"));
        assert!(set_update_flag(&mut o, "--all"));
        assert!(!set_update_flag(&mut o, "--bin-dir"));
        assert!(o.check && o.json && o.rollback && o.pre);
    }

    #[test]
    fn is_help_flag_matches_help_and_h() {
        assert!(is_help_flag(&"--help".to_string()));
        assert!(is_help_flag(&"-h".to_string()));
        assert!(!is_help_flag(&"--check".to_string()));
    }

    #[test]
    fn is_pid_suffix_accepts_only_nonempty_digits() {
        assert!(is_pid_suffix("123"));
        assert!(!is_pid_suffix(""));
        assert!(!is_pid_suffix("12a"));
        assert!(!is_pid_suffix("abc"));
    }

    #[test]
    fn clear_abandoned_stages_removes_only_stage_pid_dirs() {
        let dir = temporary();
        fs::create_dir(dir.join(".stage-123")).unwrap();
        fs::create_dir(dir.join(".stage-9")).unwrap();
        fs::create_dir(dir.join(".stage-notpid")).unwrap();
        fs::create_dir(dir.join("keepme")).unwrap();
        fs::write(dir.join(".stage-file"), b"x").unwrap();
        clear_abandoned_stages(&dir).unwrap();
        assert!(!dir.join(".stage-123").exists());
        assert!(!dir.join(".stage-9").exists());
        assert!(dir.join(".stage-notpid").exists(), "non-pid suffix kept");
        assert!(dir.join("keepme").exists(), "unrelated dir kept");
        assert!(dir.join(".stage-file").exists(), "non-dir kept");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_capped_child_stdout_reads_and_caps() {
        // Within cap: returns the bytes.
        let mut child = Command::new("printf")
            .arg("hello")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let bytes = read_capped_child_stdout(&mut child, 64).unwrap();
        assert_eq!(bytes, b"hello");

        // Over cap: errors and does not return oversized data.
        let mut big = Command::new("sh")
            .args(["-c", "printf 'aaaaaaaaaa'"]) // 10 bytes
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(read_capped_child_stdout(&mut big, 3).is_err());
    }
}
