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

pub(crate) fn mux_bin_dir() -> &'static PathBuf {
    static BIN_DIR: OnceLock<PathBuf> = OnceLock::new();
    BIN_DIR.get_or_init(|| {
        let executable = std::env::current_exe().expect("locate host test executable");
        let directory = executable.parent().unwrap().parent().unwrap().to_path_buf();
        let names = ["pmux", "pmuxd", "pmux-attach"];
        if names.iter().any(|name| !directory.join(name).is_file()) {
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
            assert!(
                directory.join(name).is_file(),
                "missing fixture binary {name}"
            );
        }
        directory
    })
}
