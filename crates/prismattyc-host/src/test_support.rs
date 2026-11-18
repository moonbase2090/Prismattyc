//! Build private mux fixture binaries beside the running test executable.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

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

/// The profile directory (`target/debug`) that holds the workspace binaries
/// beside a test executable. Older cargo puts the executable in
/// `<profile>/deps`; newer cargo nests it deeper under `<profile>/build`.
/// The target directory is the one cargo marked with `CACHEDIR.TAG`, so walk
/// up to the first directory whose parent carries that marker.
fn profile_dir(executable: &std::path::Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .skip(1)
        .find(|candidate| {
            candidate
                .parent()
                .is_some_and(|parent| parent.join("CACHEDIR.TAG").is_file())
        })
        .map(PathBuf::from)
}

pub(crate) fn mux_bin_dir() -> &'static PathBuf {
    static BIN_DIR: OnceLock<PathBuf> = OnceLock::new();
    BIN_DIR.get_or_init(|| {
        let executable = std::env::current_exe().expect("locate host test executable");
        let directory = profile_dir(&executable).expect("locate the cargo profile directory");
        let names = ["pmux", "pmuxd", "pmux-attach"];
        if names.iter().any(|name| {
            !directory
                .join(prismattyc_mux::platform::executable_name(name))
                .is_file()
        }) {
            let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let profile = directory.file_name().unwrap().to_str().unwrap();
            let status = Command::new("cargo")
                .args([
                    "build",
                    "-p",
                    "prismattyc-mux",
                    "--bins",
                    "--locked",
                    "--profile",
                ])
                .arg(if profile == "debug" { "dev" } else { profile })
                .arg("--target-dir")
                .arg(directory.parent().unwrap())
                .current_dir(workspace)
                .status()
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
    use super::profile_dir;

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
    fn the_profile_directory_is_found_in_the_old_and_new_target_layouts() {
        for layout in [
            "debug/deps/prismattyc_host-0123abcd",
            "release/deps/prismattyc_host-0123abcd",
            "debug/build/prismattyc-host/3558e706bb08c192/out/prismattyc_host-3558e706bb08c192",
        ] {
            let (root, exe) = target_with(layout);
            let expected = root.join(layout.split('/').next().unwrap());
            assert_eq!(profile_dir(&exe), Some(expected), "{layout}");
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn a_tree_without_a_cargo_target_marker_has_no_profile_directory() {
        let root =
            std::env::temp_dir().join(format!("prismattyc-no-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let exe = root.join("a/b/exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(profile_dir(&exe), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
