//! Checkout `main`, pull, and `cargo install` host / mux / pmux-mcp / prismattyc binaries.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Which packages to install after a fast-forward of `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatePlan {
    pub host: bool,
    pub mux: bool,
    pub prism: bool,
}

impl UpdatePlan {
    /// No flags ⇒ `--all` (host + mux bins + prismattyc).
    pub fn parse(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self> {
        let mut host = false;
        let mut mux = false;
        let mut all = false;
        for arg in args {
            match arg.as_ref() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "--host" => host = true,
                "--mux" => mux = true,
                "--all" => all = true,
                other => bail!("unknown update flag {other:?}"),
            }
        }
        if all || (!host && !mux) {
            return Ok(Self {
                host: true,
                mux: true,
                prism: true,
            });
        }
        Ok(Self {
            host,
            mux,
            prism: false,
        })
    }
}

pub fn print_help() {
    eprintln!(
        "\
pmux update --source — build a development checkout

USAGE:
    prismattyc update --source [--host] [--mux] [--all]
    pmux update --source [--host] [--mux] [--all]

    (no flags)   same as --all
    --host       cargo install prismattyc-host (binary prismattyc-host; macOS: also rebuild Prismattyc.app)
    --mux        cargo install prismattyc-mux --bins (pmux, pmux-attach, pmuxd)
                 and crates/pmux-mcp (pmux-mcp)
    --all        host + mux bins + pmux-mcp + prismattyc

    Installing prismattyc also installs the prismattyc(1) man page when help2man is
    available (macOS + Linux); a missing help2man is skipped with a hint.

    If `prismattyc` is not on PATH, run `pmux update` instead.

Finds the checkout by walking from cwd (or $PRISMATTYC_REPO). Refuses a dirty
tree. Checks out main and fast-forwards. Does not restart a running mux.
On macOS, installing the host also reassembles ~/Applications/Prismattyc.app and
refreshes any other existing Prismattyc.app (set APP_DEST to install elsewhere).
Quit the running app, then reopen it from the Dock — cargo install does not
update the bundle the Dock launches.
"
    );
}

/// Walk from `start` for `crates/prismattyc-host` + `crates/prismattyc-mux`.
pub fn find_repo_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join("crates/prismattyc-host").is_dir() && dir.join("crates/prismattyc-mux").is_dir()
        {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn find_repo() -> Result<PathBuf> {
    if let Ok(raw) = std::env::var("PRISMATTYC_REPO") {
        let path = PathBuf::from(raw.trim());
        if path.join("crates/prismattyc-host").is_dir()
            && path.join("crates/prismattyc-mux").is_dir()
        {
            return Ok(path);
        }
        bail!(
            "$PRISMATTYC_REPO is not a Prismattyc checkout: {}",
            path.display()
        );
    }
    let cwd = std::env::current_dir().context("current directory")?;
    find_repo_from(&cwd).with_context(|| {
        format!(
            "not inside a Prismattyc checkout (started at {}); cd there or set PRISMATTYC_REPO",
            cwd.display()
        )
    })
}

fn run_git(repo: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed ({status})", args.join(" "));
    }
    Ok(())
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cargo_install(repo: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(repo)
        .status()
        .with_context(|| format!("cargo {}", args.join(" ")))?;
    if !status.success() {
        bail!("cargo {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Update from immutable GitHub release artifacts. Source builds are explicit.
pub fn run_update(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<()> {
    let mut args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    if let Some(index) = args.iter().position(|a| a == "--source") {
        args.remove(index);
        return run_source_update(args);
    }
    #[cfg(unix)]
    {
        crate::release_update::run(&args)
    }
    #[cfg(not(unix))]
    {
        bail!("Release installation is not supported on this platform")
    }
}

/// Pull `main` and install the selected packages. Does not restart mux.
fn run_source_update(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<()> {
    let plan = UpdatePlan::parse(args)?;
    let repo = find_repo()?;
    eprintln!("prismattyc update: repo {}", repo.display());

    let dirty = git_stdout(&repo, &["status", "--porcelain"])?;
    if !dirty.trim().is_empty() {
        bail!("working tree is dirty; commit or stash before update");
    }

    run_git(&repo, &["checkout", "main"])?;
    run_git(&repo, &["pull", "--ff-only"])?;
    let head = git_stdout(&repo, &["rev-parse", "--short", "HEAD"])?;
    eprintln!("prismattyc update: main @ {}", head.trim());

    if plan.host {
        eprintln!("prismattyc update: installing prismattyc-host");
        cargo_install(
            &repo,
            &[
                "install",
                "--path",
                "crates/prismattyc-host",
                "--force",
                "--locked",
            ],
        )?;
    }
    if plan.mux {
        eprintln!("prismattyc update: installing prismattyc-mux --bins");
        cargo_install(
            &repo,
            &[
                "install",
                "--path",
                "crates/prismattyc-mux",
                "--bins",
                "--force",
                "--locked",
            ],
        )?;
        eprintln!("prismattyc update: installing pmux-mcp");
        cargo_install(
            &repo,
            &[
                "install",
                "--path",
                "crates/pmux-mcp",
                "--force",
                "--locked",
            ],
        )?;
    }
    if plan.prism {
        eprintln!("prismattyc update: installing prismattyc");
        cargo_install(
            &repo,
            &[
                "install",
                "--path",
                "crates/prismattyc",
                "--force",
                "--locked",
            ],
        )?;
        install_man_pages(&repo);
    }

    // `cargo install` only refreshes ~/.cargo/bin/prismattyc-host. On macOS the
    // Dock/menu-bar app is a separate Prismattyc.app bundle that embeds its own copy
    // of the binary, so it stays stale until the bundle is reassembled. Rebuild
    // it whenever the host was installed. No-op off macOS.
    let bundle_ok = if plan.host {
        rebuild_macos_bundle(&repo)
    } else {
        true
    };

    eprintln!(
        "prismattyc update: done ({}). Running mux/host keep the old image until you restart them.",
        prismattyc_core::bin_version("prismattyc")
    );
    if !bundle_ok {
        bail!(
            "Prismattyc.app was not rebuilt; ~/.cargo/bin is current. Quit Prismattyc.app \
             and run scripts/install-prismattyc-host-macos.sh, then reopen from the Dock."
        );
    }
    Ok(())
}

/// macOS: reassemble and reinstall the Prismattyc.app bundle from the freshly built
/// host binary via the checked-in install script, so the Dock/menu-bar app
/// matches ~/.cargo/bin. Returns false if the script failed; cargo binaries
/// are already installed either way.
#[cfg(target_os = "macos")]
fn rebuild_macos_bundle(repo: &Path) -> bool {
    let script = repo.join("scripts/install-prismattyc-host-macos.sh");
    eprintln!("prismattyc update: rebuilding macOS Prismattyc.app bundle");
    match Command::new("bash").arg(&script).current_dir(repo).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!(
                "prismattyc update: Prismattyc.app rebuild failed ({status}); \
                 cargo binaries are installed. Run `{}` manually.",
                script.display()
            );
            false
        }
        Err(error) => {
            eprintln!(
                "prismattyc update: could not run `{}`: {error}; \
                 cargo binaries are installed.",
                script.display()
            );
            false
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn rebuild_macos_bundle(_repo: &Path) -> bool {
    true
}

/// Generate and install the prismattyc(1) man page via the checked-in script.
/// Cross-platform (macOS + Linux). Best-effort: a failure here — including a
/// missing help2man — only prints a warning; the cargo binaries are already
/// installed, and the man page is a convenience, not a requirement.
#[cfg(unix)]
fn install_man_pages(repo: &Path) {
    let script = repo.join("scripts/install-man.sh");
    eprintln!("prismattyc update: installing man page");
    match Command::new("bash").arg(&script).current_dir(repo).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!(
            "prismattyc update: warning: man page install failed ({status}); \
             cargo binaries are installed. Run `{}` manually.",
            script.display()
        ),
        Err(error) => eprintln!(
            "prismattyc update: warning: could not run `{}`: {error}; \
             cargo binaries are installed.",
            script.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{find_repo_from, UpdatePlan};
    use std::fs;

    #[test]
    fn no_flags_means_all() {
        let plan = UpdatePlan::parse(None::<&str>).unwrap();
        assert!(plan.host && plan.mux && plan.prism);
    }

    #[test]
    fn all_flag_means_all() {
        let plan = UpdatePlan::parse(["--all"]).unwrap();
        assert!(plan.host && plan.mux && plan.prism);
    }

    #[test]
    fn host_only() {
        let plan = UpdatePlan::parse(["--host"]).unwrap();
        assert_eq!((plan.host, plan.mux, plan.prism), (true, false, false));
    }

    #[test]
    fn mux_only() {
        let plan = UpdatePlan::parse(["--mux"]).unwrap();
        assert_eq!((plan.host, plan.mux, plan.prism), (false, true, false));
    }

    #[test]
    fn host_and_mux_skip_nested() {
        let plan = UpdatePlan::parse(["--host", "--mux"]).unwrap();
        assert_eq!((plan.host, plan.mux, plan.prism), (true, true, false));
    }

    #[test]
    fn unknown_flag_is_error() {
        assert!(UpdatePlan::parse(["--tty"]).is_err());
    }

    #[test]
    fn find_repo_walks_to_workspace() {
        let tmp = std::env::temp_dir().join(format!(
            "prism-update-repo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let nested = tmp.join("crates/prismattyc-host/src");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(tmp.join("crates/prismattyc-mux")).unwrap();
        assert_eq!(find_repo_from(&nested).as_deref(), Some(tmp.as_path()));
        assert!(find_repo_from(&std::env::temp_dir()).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }
}

#[cfg(windows)]
fn install_man_pages(_repo: &Path) {}
