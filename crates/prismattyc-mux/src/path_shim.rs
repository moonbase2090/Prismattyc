//! Installs and removes the app-bundled `pmux` PATH link on macOS.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const PMUX: &str = "pmux";
const MARKER: &str = ".pmux-prismattyc-shim.json";
const MARKER_VERSION: u8 = 1;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedLink {
    version: u8,
    target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallReport {
    link: PathBuf,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveOutcome {
    Removed,
    Absent,
    Preserved,
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_current_app() -> Result<()> {
    let executable = std::env::current_exe().context("current executable")?;
    let Some(bundle) = bundle_from_executable(&executable) else {
        return Ok(());
    };
    ensure_app_bundle(&bundle)
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_app_bundle(bundle: &Path) -> Result<()> {
    let home = crate::platform::home_dir()
        .map(PathBuf::from)
        .context("HOME is unset; cannot choose ~/.local/bin")?;
    let target = bundle.join("Contents/MacOS/pmux");
    let system_bin = PathBuf::from("/usr/local/bin");
    let local_bin = home.join(".local/bin");
    anyhow::ensure!(local_bin.is_absolute(), "HOME must be an absolute path");
    let system_bin_allowed = crate::uninstall::Dirs::from_env().system_bin_dir.is_some();
    let path_entries = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();

    let report = ensure_link_with(
        &target,
        &system_bin,
        &local_bin,
        &path_entries,
        |directory| system_bin_allowed && dir_is_writable(directory),
    )?;
    for warning in report.warnings {
        eprintln!("pmux: warning: {warning}");
    }
    Ok(())
}

fn bundle_from_executable(executable: &Path) -> Option<PathBuf> {
    if executable.file_name()? != PMUX && executable.file_name()? != "prismattyc-host" {
        return None;
    }
    let macos = executable.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.file_name()? == "Prismattyc.app").then(|| bundle.to_path_buf())
}

fn ensure_link_with(
    target: &Path,
    system_bin: &Path,
    local_bin: &Path,
    path_entries: &[PathBuf],
    system_writable: impl Fn(&Path) -> bool,
) -> Result<InstallReport> {
    anyhow::ensure!(target.is_absolute(), "app pmux path must be absolute");
    anyhow::ensure!(
        is_app_pmux(target),
        "app pmux path must be inside Prismattyc.app"
    );
    anyhow::ensure!(
        is_executable(target),
        "app pmux is missing or not executable"
    );

    let use_system_bin = system_writable(system_bin) && system_bin.is_dir();
    let mut candidate_dirs = Vec::new();
    if use_system_bin {
        candidate_dirs.push(system_bin.to_path_buf());
    }
    if !candidate_dirs
        .iter()
        .any(|dir| same_directory(dir, local_bin))
    {
        candidate_dirs.push(local_bin.to_path_buf());
    }

    let mut conflicts = Vec::new();
    for directory in &candidate_dirs {
        if same_directory(directory, local_bin) {
            if let Err(error) = fs::create_dir_all(directory) {
                conflicts.push(format!("cannot create {}: {error}", directory.display()));
                continue;
            }
        }
        if !directory.is_dir() {
            conflicts.push(format!("{} is not a directory", directory.display()));
            continue;
        }

        let link = directory.join(PMUX);
        if managed_or_missing_link(&link) {
            if fs::read_link(&link).is_ok_and(|existing| existing == target) {
                let mut warnings = path_warnings(&link, path_entries);
                cleanup_other_owned_links(
                    &link,
                    use_system_bin.then_some(system_bin),
                    local_bin,
                    &mut warnings,
                );
                return Ok(InstallReport { link, warnings });
            }
            match remove_owned_link(&link) {
                Ok(RemoveOutcome::Removed | RemoveOutcome::Absent) => {}
                Ok(RemoveOutcome::Preserved) => {}
                Err(error) => {
                    conflicts.push(format!(
                        "cannot clear the previous Prismattyc link at {}: {error}",
                        link.display()
                    ));
                    continue;
                }
            }
        }

        match fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if fs::read_link(&link).is_ok_and(|existing| existing == target) {
                    let mut warnings = path_warnings(&link, path_entries);
                    cleanup_other_owned_links(
                        &link,
                        use_system_bin.then_some(system_bin),
                        local_bin,
                        &mut warnings,
                    );
                    return Ok(InstallReport { link, warnings });
                }
                conflicts.push(format!("{} already points to another pmux", link.display()));
                continue;
            }
            Ok(_) => {
                conflicts.push(format!(
                    "{} already exists and was left unchanged",
                    link.display()
                ));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                conflicts.push(format!("cannot inspect {}: {error}", link.display()));
                continue;
            }
        }

        let marker = marker_path(&link);
        match read_record(&marker) {
            Ok(None) => {}
            Ok(Some(_)) => {
                conflicts.push(format!(
                    "{} has a stale or mismatched ownership record",
                    marker.display()
                ));
                continue;
            }
            Err(error) => {
                conflicts.push(format!("cannot read {}: {error}", marker.display()));
                continue;
            }
        }

        if let Err(error) = create_symlink(target, &link) {
            if fs::read_link(&link).is_ok_and(|existing| existing == target) {
                let mut warnings = path_warnings(&link, path_entries);
                cleanup_other_owned_links(
                    &link,
                    use_system_bin.then_some(system_bin),
                    local_bin,
                    &mut warnings,
                );
                return Ok(InstallReport { link, warnings });
            }
            conflicts.push(format!("cannot create {}: {error}", link.display()));
            continue;
        }
        if let Err(error) = write_record(&marker, target) {
            return Err(error).context(format!(
                "created {} but could not record ownership; the link was left in place and uninstall will preserve it",
                link.display()
            ));
        }

        let mut warnings = path_warnings(&link, path_entries);
        cleanup_other_owned_links(
            &link,
            use_system_bin.then_some(system_bin),
            local_bin,
            &mut warnings,
        );
        return Ok(InstallReport { link, warnings });
    }

    bail!(
        "could not add the app-bundled pmux to PATH. {} If the existing command is an older Cargo install, run `cargo uninstall pmux`; otherwise remove or move the conflicting pmux, then try again.",
        conflicts.join("; ")
    )
}

fn cleanup_other_owned_links(
    installed_link: &Path,
    system_bin: Option<&Path>,
    local_bin: &Path,
    warnings: &mut Vec<String>,
) {
    for directory in system_bin.into_iter().chain([local_bin]) {
        let old_link = directory.join(PMUX);
        if same_path(&old_link, installed_link) || !managed_or_missing_link(&old_link) {
            continue;
        }
        if let Err(error) = remove_owned_link(&old_link) {
            warnings.push(format!(
                "could not remove the previous Prismattyc link at {}: {error}",
                old_link.display()
            ));
        }
    }
}

fn path_warnings(link: &Path, path_entries: &[PathBuf]) -> Vec<String> {
    let directory_index = path_entries
        .iter()
        .position(|entry| same_directory(entry, link.parent().unwrap_or(Path::new("."))));
    let mut warnings = Vec::new();
    if directory_index.is_none() {
        warnings.push(format!(
            "the app link is at {}, but that directory is not on PATH. Add it to PATH in your shell configuration; Prismattyc did not edit shell profiles.",
            link.display()
        ));
    }

    let earlier = path_entries
        .iter()
        .enumerate()
        .find_map(|(index, directory)| {
            let executable = directory.join(PMUX);
            is_executable(&executable).then_some((index, executable))
        });
    if let Some((index, executable)) = earlier {
        let resolves_to_app = fs::canonicalize(&executable)
            .ok()
            .zip(fs::canonicalize(link).ok())
            .is_some_and(|(first, app)| first == app);
        if !resolves_to_app && directory_index.is_none_or(|shim_index| index < shim_index) {
            warnings.push(format!(
                "{} appears earlier on PATH than {} and will run first. If it is an older Cargo install, run `cargo uninstall pmux`; otherwise remove that earlier pmux or move {} earlier on PATH, then open a new terminal.",
                executable.display(),
                link.display(),
                link.parent().unwrap_or(Path::new(".")).display()
            ));
        }
    }
    warnings
}

pub(crate) fn is_managed_link_or_absent(link: &Path) -> bool {
    managed_or_missing_link(link)
}

fn managed_or_missing_link(link: &Path) -> bool {
    let marker = marker_path(link);
    let Ok(Some(record)) = read_record(&marker) else {
        return false;
    };
    if !is_app_pmux(&record.target) || !record.target.is_absolute() {
        return false;
    }
    match fs::symlink_metadata(link) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::read_link(link).is_ok_and(|target| target == record.target)
        }
        _ => false,
    }
}

pub(crate) fn remove_owned_link(link: &Path) -> Result<RemoveOutcome> {
    let marker = marker_path(link);
    let Some(record) = read_record(&marker)? else {
        return Ok(RemoveOutcome::Preserved);
    };
    anyhow::ensure!(
        record.version == MARKER_VERSION && is_app_pmux(&record.target),
        "invalid Prismattyc pmux ownership record"
    );

    match fs::symlink_metadata(link) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            remove_marker(&marker).with_context(|| format!("remove {}", marker.display()))?;
            Ok(RemoveOutcome::Absent)
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(link)? != record.target {
                remove_marker(&marker).with_context(|| format!("remove {}", marker.display()))?;
                return Ok(RemoveOutcome::Preserved);
            }
            fs::remove_file(link)
                .with_context(|| format!("remove owned pmux link {}", link.display()))?;
            remove_marker(&marker).with_context(|| format!("remove {}", marker.display()))?;
            Ok(RemoveOutcome::Removed)
        }
        Ok(_) => {
            remove_marker(&marker).with_context(|| format!("remove {}", marker.display()))?;
            Ok(RemoveOutcome::Preserved)
        }
        Err(error) => Err(error).with_context(|| format!("inspect {}", link.display())),
    }
}

fn read_record(marker: &Path) -> Result<Option<ManagedLink>> {
    let metadata = match fs::symlink_metadata(marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", marker.display())),
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{} is not a regular file",
        marker.display()
    );
    let record: ManagedLink = serde_json::from_slice(
        &fs::read(marker).with_context(|| format!("read {}", marker.display()))?,
    )
    .with_context(|| format!("parse {}", marker.display()))?;
    anyhow::ensure!(
        record.version == MARKER_VERSION,
        "unsupported pmux shim record"
    );
    Ok(Some(record))
}

fn write_record(marker: &Path, target: &Path) -> Result<()> {
    let parent = marker.parent().context("pmux shim marker parent")?;
    let temp = loop {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{MARKER}.tmp-{}-{id}", std::process::id()));
        match open_private_new(&candidate) {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()))
            }
        }
    };
    let (temp_path, mut file) = temp;
    let result = (|| -> Result<()> {
        serde_json::to_writer(
            &mut file,
            &ManagedLink {
                version: MARKER_VERSION,
                target: target.to_path_buf(),
            },
        )?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, marker).with_context(|| format!("save {}", marker.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
}

#[cfg(unix)]
fn open_private_new(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

fn remove_marker(marker: &Path) -> io::Result<()> {
    match fs::remove_file(marker) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn marker_path(link: &Path) -> PathBuf {
    link.parent().unwrap_or(Path::new(".")).join(MARKER)
}

fn is_app_pmux(path: &Path) -> bool {
    let Some(macos) = path.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    let Some(bundle) = contents.parent() else {
        return false;
    };
    path.file_name().is_some_and(|name| name == PMUX)
        && macos.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && bundle
            .file_name()
            .is_some_and(|name| name == "Prismattyc.app")
}

fn dir_is_writable(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn same_directory(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
        || (!left.exists() && !right.exists() && normalize(left) == normalize(right))
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalize(left) == normalize(right)
}

fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "prismattyc-path-shim-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn app_binary(root: &Path) -> PathBuf {
        let app = root.join("Prismattyc.app/Contents/MacOS/pmux");
        fs::create_dir_all(app.parent().unwrap()).unwrap();
        fs::write(&app, b"pmux").unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).unwrap();
        app
    }

    #[test]
    fn writable_system_bin_gets_the_link() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();

        let report = ensure_link_with(
            &target,
            &system,
            &local,
            std::slice::from_ref(&system),
            |_| true,
        )
        .unwrap();

        assert_eq!(report.link, system.join(PMUX));
        assert_eq!(fs::read_link(&report.link).unwrap(), target);
        assert!(managed_or_missing_link(&report.link));
    }

    #[test]
    fn repeated_launch_keeps_the_existing_owned_link() {
        use std::os::unix::fs::MetadataExt;

        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();
        let entries = [system.clone()];

        let first = ensure_link_with(&target, &system, &local, &entries, |_| true).unwrap();
        let first_inode = fs::symlink_metadata(&first.link).unwrap().ino();
        let second = ensure_link_with(&target, &system, &local, &entries, |_| true).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            fs::symlink_metadata(&second.link).unwrap().ino(),
            first_inode
        );
    }

    #[test]
    fn unwritable_system_bin_falls_back_to_local_bin() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();

        let report = ensure_link_with(
            &target,
            &system,
            &local,
            std::slice::from_ref(&local),
            |_| false,
        )
        .unwrap();

        assert_eq!(report.link, local.join(PMUX));
        assert_eq!(fs::read_link(&report.link).unwrap(), target);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn warns_when_the_fallback_bin_dir_is_not_on_path() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();

        let report = ensure_link_with(&target, &system, &local, &[], |_| false).unwrap();

        assert_eq!(report.link, local.join(PMUX));
        assert!(report.warnings.iter().any(|warning| {
            warning.contains("not on PATH") && warning.contains("did not edit shell profiles")
        }));
    }

    #[test]
    fn disabled_system_bin_does_not_clean_up_its_existing_link() {
        let tmp = Scratch::new();
        let old_target = app_binary(&tmp.path().join("old"));
        let new_target = app_binary(&tmp.path().join("new"));
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();
        let old = ensure_link_with(
            &old_target,
            &system,
            &local,
            std::slice::from_ref(&system),
            |_| true,
        )
        .unwrap();

        let new = ensure_link_with(
            &new_target,
            &system,
            &local,
            std::slice::from_ref(&local),
            |_| false,
        )
        .unwrap();

        assert_eq!(new.link, local.join(PMUX));
        assert_eq!(fs::read_link(&old.link).unwrap(), old_target);
        assert!(marker_path(&old.link).exists());
    }

    #[test]
    fn older_cargo_pmux_before_the_link_gets_a_recovery_warning() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let cargo = tmp.path().join("home/.cargo/bin");
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&cargo).unwrap();
        fs::create_dir_all(&system).unwrap();
        let older = cargo.join(PMUX);
        fs::write(&older, b"older cargo pmux").unwrap();
        fs::set_permissions(&older, fs::Permissions::from_mode(0o755)).unwrap();

        let report = ensure_link_with(
            &target,
            &system,
            &local,
            &[cargo.clone(), system.clone()],
            |_| true,
        )
        .unwrap();

        assert!(report.warnings.iter().any(|warning| {
            warning.contains(&older.display().to_string())
                && warning.contains("cargo uninstall pmux")
        }));
        assert!(older.exists());
    }

    #[test]
    fn system_bin_conflict_is_preserved_and_local_bin_is_used() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();
        let existing = system.join(PMUX);
        fs::write(&existing, b"existing command").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();

        let report = ensure_link_with(
            &target,
            &system,
            &local,
            &[system.clone(), local.clone()],
            |_| true,
        )
        .unwrap();

        assert_eq!(report.link, local.join(PMUX));
        assert_eq!(fs::read(&existing).unwrap(), b"existing command");
        assert!(report.warnings.iter().any(|warning| {
            warning.contains(&existing.display().to_string())
                && warning.contains("cargo uninstall pmux")
        }));
    }

    #[test]
    fn uninstall_removes_only_the_owned_link_and_keeps_the_app_binary() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();
        let report = ensure_link_with(
            &target,
            &system,
            &local,
            std::slice::from_ref(&system),
            |_| true,
        )
        .unwrap();

        assert!(is_managed_link_or_absent(&report.link));
        assert_eq!(
            remove_owned_link(&report.link).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!report.link.exists());
        assert!(!marker_path(&report.link).exists());
        assert!(target.exists());
    }

    #[test]
    fn uninstall_leaves_an_unowned_pmux_untouched() {
        let tmp = Scratch::new();
        let link = tmp.path().join("home/.local/bin/pmux");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&link, b"a separate pmux install").unwrap();
        fs::set_permissions(&link, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!is_managed_link_or_absent(&link));
        assert_eq!(remove_owned_link(&link).unwrap(), RemoveOutcome::Preserved);
        assert!(link.exists());
        assert_eq!(fs::read(link).unwrap(), b"a separate pmux install");
    }

    #[test]
    fn uninstall_preserves_a_link_that_changed_after_install() {
        let tmp = Scratch::new();
        let target = app_binary(tmp.path());
        let other = tmp.path().join("other/pmux");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, b"replacement").unwrap();
        let system = tmp.path().join("usr-local-bin");
        let local = tmp.path().join("home/.local/bin");
        fs::create_dir_all(&system).unwrap();
        let report = ensure_link_with(
            &target,
            &system,
            &local,
            std::slice::from_ref(&system),
            |_| true,
        )
        .unwrap();
        fs::remove_file(&report.link).unwrap();
        create_symlink(&other, &report.link).unwrap();

        assert!(!is_managed_link_or_absent(&report.link));
        assert_eq!(
            remove_owned_link(&report.link).unwrap(),
            RemoveOutcome::Preserved
        );
        assert_eq!(fs::read_link(&report.link).unwrap(), other);
        assert!(target.exists());
    }

    #[test]
    fn executable_inside_the_app_selects_that_bundle() {
        let app = Path::new("/tmp/Prismattyc.app");
        assert_eq!(
            bundle_from_executable(&app.join("Contents/MacOS/prismattyc-host")),
            Some(app.to_path_buf())
        );
        assert_eq!(
            bundle_from_executable(Path::new("/tmp/prismattyc-host")),
            None
        );
    }
}
