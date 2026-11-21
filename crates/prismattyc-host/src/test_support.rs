//! Build private mux fixture binaries beside the running test executable.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

/// Write a script and make it executable without ever leaving it busy to exec.
///
/// A script written and executed from one process can fail with "Text file
/// busy": while this process holds the file open for writing, another test
/// thread's fork can copy the descriptor into a child that has not yet
/// exec'd. The finished file is copied into place by a child that has exited
/// and renamed over the original, so this process holds no write descriptor on
/// the inode that gets executed.
#[cfg(unix)]
pub(crate) fn write_script(path: &std::path::Path, body: &str, mode: &str) {
    let mut staged = path.as_os_str().to_owned();
    staged.push(".staged");
    let staged = std::path::PathBuf::from(staged);
    std::fs::write(&staged, body).unwrap();
    install_over(&staged, path, mode);
    std::fs::remove_file(&staged).unwrap();
}

/// As `write_script`, for a script that is already written and closed.
#[cfg(unix)]
pub(crate) fn make_executable(path: &std::path::Path, mode: &str) {
    install_over(path, path, mode);
}

/// Copy `source` through an exited `install` child to a sibling of `path`,
/// then rename that finished file over `path`.
#[cfg(unix)]
fn install_over(source: &std::path::Path, path: &std::path::Path, mode: &str) {
    let mut installed = path.as_os_str().to_owned();
    installed.push(".installed");
    let installed = std::path::PathBuf::from(installed);
    let status = prismattyc_process::status(
        Command::new("install")
            .args(["-m", mode])
            .arg(source)
            .arg(&installed),
    )
    .unwrap();
    assert!(status.success(), "install the test script");
    std::fs::rename(&installed, path).unwrap();
}

#[cfg(target_os = "linux")]
pub(crate) fn wait_for_attach_write(host: &crate::HostState) {
    let Some(path) = host.attach_layout_path.as_deref() else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.file_writer.attach_tabs_write_pending(path) {
        assert!(
            Instant::now() < deadline,
            "attach-tabs write did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn wait_for_render_status(app: &crate::App) {
    let Some((pid_path, _)) = app.registered_host.as_ref() else {
        return;
    };
    app.file_writer.handle().wait_for_render_status(pid_path);
}

/// Where cargo put a test executable: the target directory it was marked
/// with `CACHEDIR.TAG`, an optional target-triple directory (an explicit
/// `--target`), and the profile directory that holds the workspace binaries.
#[derive(Debug, PartialEq)]
struct CargoLayout {
    target_dir: PathBuf,
    triple: Option<String>,
    profile_dir: PathBuf,
}

/// Older cargo puts the executable in `<profile>/deps`; newer cargo nests it
/// deeper under `<profile>/build`. An explicit `--target` adds a triple
/// directory above the profile. Read the layout from the path below the
/// target directory instead of counting levels from the executable.
fn locate(
    executable: &std::path::Path,
    target_hint: Option<&std::path::Path>,
) -> Option<CargoLayout> {
    let target_dir = executable
        .ancestors()
        .skip(1)
        .find(|candidate| candidate.join("CACHEDIR.TAG").is_file())
        .or_else(|| {
            target_hint.filter(|target| target.is_absolute() && executable.starts_with(target))
        })?;
    let parts: Vec<_> = executable
        .strip_prefix(target_dir)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let is_build_dir = |name: &str| name == "deps" || name == "build";
    let (triple, profile) = match parts.as_slice() {
        [profile, inner, ..] if is_build_dir(inner) => (None, *profile),
        [triple, profile, inner, ..] if is_build_dir(inner) => (Some(triple.to_string()), *profile),
        _ => return None,
    };
    let mut profile_dir = target_dir.to_path_buf();
    profile_dir.extend(triple.iter());
    profile_dir.push(profile);
    Some(CargoLayout {
        target_dir: target_dir.to_path_buf(),
        triple,
        profile_dir,
    })
}

pub(crate) fn mux_bin_dir() -> &'static PathBuf {
    static BIN_DIR: OnceLock<PathBuf> = OnceLock::new();
    BIN_DIR.get_or_init(|| {
        let executable = std::env::current_exe().expect("locate host test executable");
        let target_hint = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
        let layout =
            locate(&executable, target_hint.as_deref()).expect("locate the cargo target layout");
        let directory = layout.profile_dir.clone();
        let names = ["pmux", "pmuxd", "pmux-attach"];
        if names.iter().any(|name| {
            !directory
                .join(prismattyc_mux::platform::executable_name(name))
                .is_file()
        }) {
            let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let profile = directory.file_name().unwrap().to_str().unwrap();
            let mut build = Command::new("cargo");
            if let Some(triple) = &layout.triple {
                build.args(["build", "--target", triple]);
            } else {
                build.arg("build");
            }
            let status = prismattyc_process::status(
                build
                    .args(["-p", "prismattyc-mux", "--bins", "--locked", "--profile"])
                    .arg(if profile == "debug" { "dev" } else { profile })
                    .arg("--target-dir")
                    .arg(&layout.target_dir)
                    .current_dir(workspace),
            )
            .expect("build private mux fixture binaries");
            assert!(status.success(), "private mux fixture binary build failed");
        }
        for name in names {
            let executable = prismattyc_mux::platform::executable_name(name);
            assert!(
                directory.join(executable).is_file(),
                "missing fixture binary {name}"
            );
        }
        directory
    })
}

#[cfg(test)]
mod tests {
    use super::{locate, CargoLayout};

    fn target_with(layout: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "prismattyc-profile-dir-{}-{}",
            std::process::id(),
            layout.replace('/', "_")
        ));
        let _ = std::fs::remove_dir_all(&root);
        let exe = root.join(layout);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(
            root.join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        std::fs::write(&exe, b"").unwrap();
        (root, exe)
    }

    #[test]
    fn the_layout_is_read_for_old_new_and_explicit_target_paths() {
        let triple = "x86_64-unknown-linux-gnu";
        let cases = [
            ("debug/deps/prismattyc_host-0123abcd", None, "debug"),
            ("release/deps/prismattyc_host-0123abcd", None, "release"),
            (
                "debug/build/prismattyc-host/3558e706bb08c192/out/prismattyc_host-3558e706bb08c192",
                None,
                "debug",
            ),
            (
                "x86_64-unknown-linux-gnu/debug/deps/prismattyc_host-0123abcd",
                Some(triple),
                "debug",
            ),
            (
                "x86_64-unknown-linux-gnu/release/build/prismattyc-host/3558e706bb08c192/out/prismattyc_host-3558e706bb08c192",
                Some(triple),
                "release",
            ),
        ];
        for (path, expect_triple, profile) in cases {
            let (root, exe) = target_with(path);
            let mut profile_dir = root.clone();
            if let Some(triple) = expect_triple {
                profile_dir.push(triple);
            }
            profile_dir.push(profile);
            assert_eq!(
                locate(&exe, None),
                Some(CargoLayout {
                    target_dir: root.clone(),
                    triple: expect_triple.map(String::from),
                    profile_dir,
                }),
                "{path}"
            );
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn a_tree_without_a_cargo_target_marker_has_no_layout() {
        let root =
            std::env::temp_dir().join(format!("prismattyc-no-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let exe = root.join("a/b/exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(locate(&exe, None), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_explicit_target_directory_does_not_require_a_cache_marker() {
        let (root, exe) = target_with("debug/deps/prismattyc_host-explicit");
        std::fs::remove_file(root.join("CACHEDIR.TAG")).unwrap();
        assert_eq!(
            locate(&exe, Some(&root)),
            Some(CargoLayout {
                target_dir: root.clone(),
                triple: None,
                profile_dir: root.join("debug"),
            })
        );
        assert_eq!(locate(&exe, Some(&root.join("unrelated"))), None);
        assert_eq!(locate(&exe, None), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
