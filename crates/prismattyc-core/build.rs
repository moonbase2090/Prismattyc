//! Stamp a short git hash so `prismattyc --version` changes when main moves,
//! even if the Cargo semver has not.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.join("../..");
    println!(
        "cargo:rerun-if-changed={}",
        repo.join(".git/HEAD").display()
    );
    let refs = repo.join(".git/refs/heads");
    if refs.is_dir() {
        println!("cargo:rerun-if-changed={}", refs.display());
    }

    let hash = prismattyc_process::spawn(
        prismattyc_process::capture_command("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .current_dir(&repo),
    )
    .and_then(std::process::Child::wait_with_output)
    .ok()
    .filter(|out| out.status.success())
    .and_then(|out| String::from_utf8(out.stdout).ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_default();
    println!("cargo:rustc-env=PRISMATTYC_GIT_HASH={hash}");
}
