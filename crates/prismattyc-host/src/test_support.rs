//! Build private mux fixture binaries beside the running test executable.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

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
