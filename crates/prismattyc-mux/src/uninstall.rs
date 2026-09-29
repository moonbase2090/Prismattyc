//! `pmux uninstall` / `prismattyc uninstall` — one shared command that
//! removes everything Prismattyc installed or wrote on this machine.
//!
//! Mirrors the shared [`crate::update`] command: both front-door binaries
//! call [`run_uninstall`], which resolves the same base directories used by
//! the installer and the self-updater ([`crate::platform`]) and never
//! touches a path outside the product's own inventory.
//!
//! # Behaviour
//!
//! - Print exactly what will be removed, then confirm (unless `--yes`).
//! - After confirmation, stop the daemon and sessions cleanly, warning first.
//! - Default removes everything, including Spaces and config data.
//! - `--keep-data` keeps Spaces and config.
//! - `--dry-run` lists the inventory and removes nothing.
//! - Anything that cannot be removed is reported with what, why and the fix;
//!   the rest is still removed.
//! - Exit non-zero if anything is left; end by confirming it is fully gone or
//!   naming the leftovers.
//!
//! Path safety: only paths in the computed inventory are ever removed. Every
//! absolute path in the inventory comes from an injected [`Dirs`] field, so
//! tests build `Dirs` from a tempdir root and can never target a real machine
//! path such as `/Applications`. Host-global paths (`/Applications`,
//! `/usr/local/bin`, `/tmp/prismattyc-<uid>`) are included only when the user directories are
//! NOT redirected (see [`EnvSnapshot::is_redirected`]): an invocation with a
//! sandboxed HOME/XDG can never target the real system app or the shared
//! `/tmp` runtime dir, and `stop_daemons` scans only the resolved runtime
//! dirs. There is no globbing outside the product's own directories, and
//! symlinks are removed as links — the target is never followed or deleted. A
//! runtime directory that still hosts a live pmux daemon is refused (never
//! deleted), so uninstall can never yank a live socket out from under a
//! running daemon.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// The six shipped executables. Kept in sync with
/// [`crate::release_update`]'s `BINARIES`.
pub const BINARIES: [&str; 6] = [
    "prismattyc",
    "prismattyc-host",
    "pmux",
    "pmuxd",
    "pmux-attach",
    "pmux-mcp",
];

/// Which OS an inventory item applies to. An item only lands in the plan when
/// it matches the running OS, so a macOS `.app` never appears on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Any,
    Macos,
    Linux,
    Windows,
}

impl Os {
    /// The OS this build runs on.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    fn matches(self, current: Self) -> bool {
        matches!(self, Self::Any) || self == current
    }
}

/// A grouping for the printed plan. `Data` items are the ones `--keep-data`
/// preserves (Spaces, layouts, config, walkthrough, session state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Installed binaries, the macOS app bundle, and PATH launchers/shims.
    Binaries,
    /// The self-update store (`…/prismattyc/updates`).
    Updates,
    /// Sockets, pid/lock/log/ack files (per-instance runtime state).
    Runtime,
    /// Spaces, layouts, mailbox, session state — user data (`--keep-data`).
    Data,
    /// `config.toml` — user configuration (`--keep-data`).
    Config,
    /// Desktop entry, icons, systemd unit, man pages, terminfo, shortcuts.
    Integration,
}

impl Category {
    /// Whether `--keep-data` preserves items in this category.
    #[must_use]
    pub fn is_data(self) -> bool {
        matches!(self, Self::Data | Self::Config)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Binaries => "binaries and app bundle",
            Self::Updates => "self-update store",
            Self::Runtime => "runtime state (sockets, pids, logs)",
            Self::Data => "user data (Spaces, layouts, mail, sessions)",
            Self::Config => "configuration",
            Self::Integration => "OS integration",
        }
    }
}

/// One thing the uninstaller may remove: a single path, tagged so the plan
/// can group it and `--keep-data` can skip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: PathBuf,
    pub category: Category,
    /// Human note for the plan ("Prismattyc.app bundle", "mailbox database").
    pub what: String,
}

/// The base directories the inventory is computed from. Injecting these keeps
/// [`inventory`] a pure function: **every** absolute path it emits comes from
/// one of these fields, so tests that build `Dirs` from a tempdir root can
/// never produce a real machine path such as `/Applications`.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub home: Option<PathBuf>,
    pub config_home: Option<PathBuf>,
    pub data_home: Option<PathBuf>,
    /// Unix socket runtime dir(s) (e.g. `$XDG_RUNTIME_DIR/prismattyc`,
    /// `/tmp/prismattyc-<uid>`). Empty on Windows (see `windows_run`).
    pub runtime_dirs: Vec<PathBuf>,
    /// Windows `%LOCALAPPDATA%\Prismattyc\run` runtime dir.
    pub windows_run: Option<PathBuf>,
    /// Directories on `PATH` that may hold launchers/symlinks to the bins.
    pub bin_dirs: Vec<PathBuf>,
    /// System-wide macOS Applications dir (`/Applications` in production).
    /// Injectable so tests never target the real one.
    pub system_app_dir: Option<PathBuf>,
    /// System-wide PATH directory used for the app's `pmux` link. It is
    /// omitted when HOME or XDG paths are redirected.
    pub system_bin_dir: Option<PathBuf>,
    pub os: Os,
}

impl Dirs {
    /// Resolve from the real environment, using the same helpers as the
    /// installer and self-updater. Delegates to the pure [`Dirs::resolve`] so
    /// the redirection logic can be tested without mutating process env.
    ///
    /// Host-global paths (`/Applications`, `/usr/local/bin`,
    /// `/tmp/prismattyc-<uid>`) are the
    /// only real machine paths introduced here, and only when the user
    /// directories are NOT redirected — see [`EnvSnapshot::is_redirected`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::resolve(&EnvSnapshot::from_env())
    }

    /// Pure resolver: build [`Dirs`] from an explicit environment snapshot.
    /// When the user directories are redirected (a sandboxed / non-default
    /// HOME or XDG), host-global paths are omitted so an invocation with fake
    /// user directories can never target the system app, PATH link, or shared
    /// `/tmp` runtime dir.
    #[must_use]
    pub fn resolve(env: &EnvSnapshot) -> Self {
        let home = env.home.clone();
        let config_home = env
            .xdg_config_home
            .clone()
            .filter(|v| !v.as_os_str().is_empty())
            .or_else(|| {
                if cfg!(windows) {
                    env.appdata.clone()
                } else {
                    None
                }
            })
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let data_home = env
            .xdg_data_home
            .clone()
            .filter(|v| !v.as_os_str().is_empty())
            .or_else(|| {
                if cfg!(windows) {
                    env.localappdata.clone()
                } else {
                    None
                }
            })
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));

        let redirected = env.is_redirected();

        let mut runtime_dirs = Vec::new();
        if let Some(rt) = env
            .xdg_runtime_dir
            .clone()
            .filter(|v| !v.as_os_str().is_empty())
        {
            runtime_dirs.push(rt.join("prismattyc"));
        }
        // The shared /tmp/prismattyc-<uid> dir is host-global: include it only
        // when the user directories are at their real defaults.
        if env.os == Os::Linux || env.os == Os::Macos {
            if let (false, Some(uid)) = (redirected, env.uid) {
                runtime_dirs.push(PathBuf::from(format!("/tmp/prismattyc-{uid}")));
            }
        }

        let windows_run = if env.os == Os::Windows {
            data_home.as_ref().map(|d| d.join("Prismattyc").join("run"))
        } else {
            None
        };

        let mut bin_dirs = Vec::new();
        if let Some(h) = home.as_ref() {
            bin_dirs.push(h.join(".local/bin"));
            bin_dirs.push(h.join(".cargo/bin"));
        }
        if let Some(cargo_home) = env.cargo_home.clone().filter(|v| !v.as_os_str().is_empty()) {
            bin_dirs.push(cargo_home.join("bin"));
        }

        // /Applications is host-global: include it only on a non-redirected
        // macOS invocation.
        let system_app_dir = if env.os == Os::Macos && !redirected {
            Some(PathBuf::from("/Applications"))
        } else {
            None
        };
        let system_bin_dir = if env.os == Os::Macos && !redirected {
            Some(PathBuf::from("/usr/local/bin"))
        } else {
            None
        };

        Self {
            home,
            config_home,
            data_home,
            runtime_dirs,
            windows_run,
            bin_dirs,
            system_app_dir,
            system_bin_dir,
            os: env.os,
        }
    }
}

/// An explicit snapshot of the environment inputs the resolver reads. Captured
/// once from the process (see [`EnvSnapshot::from_env`]) so [`Dirs::resolve`]
/// stays a pure function that tests can drive with redirected values without
/// mutating global process state.
#[derive(Debug, Clone)]
pub struct EnvSnapshot {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub cargo_home: Option<PathBuf>,
    /// Windows base dirs (unused on unix; mirrors `platform::config/data_home`).
    pub appdata: Option<PathBuf>,
    pub localappdata: Option<PathBuf>,
    /// The OS's notion of the real user home, used to detect a redirected
    /// `HOME`. `None` when it cannot be determined.
    pub real_home: Option<PathBuf>,
    pub uid: Option<u32>,
    pub os: Os,
}

impl EnvSnapshot {
    /// Capture from the real process environment.
    #[must_use]
    pub fn from_env() -> Self {
        fn nonempty(key: &str) -> Option<PathBuf> {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        }
        let home = crate::platform::home_dir().map(PathBuf::from);

        #[cfg(unix)]
        let (real_home, uid) = {
            let uid = rustix::process::geteuid().as_raw();
            (real_home_for_uid(uid), Some(uid))
        };
        #[cfg(not(unix))]
        let (real_home, uid) = (home.clone(), None);

        Self {
            home,
            xdg_config_home: nonempty("XDG_CONFIG_HOME"),
            xdg_data_home: nonempty("XDG_DATA_HOME"),
            xdg_runtime_dir: nonempty("XDG_RUNTIME_DIR"),
            cargo_home: nonempty("CARGO_HOME"),
            appdata: nonempty("APPDATA"),
            localappdata: nonempty("LOCALAPPDATA"),
            real_home,
            uid,
            os: Os::current(),
        }
    }

    /// True when the user directories are redirected away from their defaults,
    /// e.g. a sandbox that sets `HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, or
    /// `CARGO_HOME` under a temp dir. In that case host-global paths
    /// (`/Applications`, `/usr/local/bin`, `/tmp/prismattyc-<uid>`) are omitted: the caller has
    /// signalled it is not operating on the real user account.
    ///
    /// Fails safe: host-global paths are emitted **only** when we can
    /// positively confirm this is the real account — `HOME` is set and equals
    /// the OS's real home for this uid. If the real home cannot be resolved
    /// (`getpwuid` returned nothing) or `HOME` is unset, we treat the run as
    /// redirected so a destructive uninstall never targets `/Applications`,
    /// `/usr/local/bin`, or the shared `/tmp/prismattyc-<uid>` on an unverified environment.
    #[must_use]
    pub fn is_redirected(&self) -> bool {
        // Any explicit XDG override or CARGO_HOME is a redirection signal.
        if self.xdg_config_home.is_some()
            || self.xdg_data_home.is_some()
            || self.xdg_runtime_dir.is_some()
            || self.cargo_home.is_some()
        {
            return true;
        }
        // Not redirected only when HOME is confirmed to be the real home.
        // Any other case — HOME differs, HOME unset, or the real home could
        // not be resolved — is treated as redirected (fail safe).
        match (&self.home, &self.real_home) {
            (Some(home), Some(real)) => home != real,
            _ => true,
        }
    }
}

/// The OS's real home directory for `uid`, independent of `$HOME`, so a
/// redirected `HOME` can be detected. Uses the passwd database on unix.
#[cfg(unix)]
fn real_home_for_uid(uid: u32) -> Option<PathBuf> {
    // Safety: getpwuid returns a pointer into a static buffer; we copy the
    // dir string immediately and never retain the pointer.
    unsafe {
        let pw = libc::getpwuid(uid as libc::uid_t);
        if pw.is_null() {
            return None;
        }
        let dir = (*pw).pw_dir;
        if dir.is_null() {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(dir);
        let bytes = cstr.to_bytes();
        if bytes.is_empty() {
            return None;
        }
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
}

/// The Prismattyc data subdirectory / file names, relative to `data_home`.
/// Single source of truth for the data-file inventory, mirroring the per-
/// domain helpers (mailbox, layout_file, walkthrough, pane_log_persist).
const DATA_FILES: &[&str] = &["mail.db", "session-agents.json", "walkthrough.json"];
const DATA_DIRS: &[&str] = &["spaces", "layouts"];

/// Build the full inventory for the given directories and OS, filtered to the
/// running OS. Pure: it never reads or writes the filesystem, so tests can
/// assert the exact set of paths for a fake environment.
#[must_use]
pub fn inventory(dirs: &Dirs) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let os = dirs.os;
    let mut push = |path: PathBuf, category: Category, what: &str, applies: Os| {
        if applies.matches(os) {
            items.push(Item {
                path,
                category,
                what: what.to_string(),
            });
        }
    };

    // --- Binaries: PATH launchers / symlinks in the managed bin dirs. ---
    for dir in &dirs.bin_dirs {
        for bin in BINARIES {
            // On macOS, `pmux` may be an app-managed symlink or an independent
            // Cargo install. Only the symlink with its ownership record is
            // removed; never delete an arbitrary `pmux` at this path.
            if os == Os::Macos && bin == "pmux" {
                continue;
            }
            let name = crate::platform::executable_name(bin);
            push(
                dir.join(&name),
                Category::Binaries,
                &format!("{bin} launcher/binary"),
                Os::Any,
            );
        }
    }

    // --- macOS app bundle(s). ---
    if let Some(home) = &dirs.home {
        push(
            home.join("Applications/Prismattyc.app"),
            Category::Binaries,
            "Prismattyc.app bundle (user Applications)",
            Os::Macos,
        );
    }
    if let Some(system_apps) = &dirs.system_app_dir {
        push(
            system_apps.join("Prismattyc.app"),
            Category::Binaries,
            "Prismattyc.app bundle (system Applications)",
            Os::Macos,
        );
    }

    // --- Windows preview install tree + Start Menu shortcut. ---
    if let Some(data) = &dirs.data_home {
        // %LOCALAPPDATA%\Programs\Prismattyc holds versioned preview installs.
        push(
            data.join("Programs").join("Prismattyc"),
            Category::Binaries,
            "Windows preview install tree",
            Os::Windows,
        );
    }
    if let Some(run) = &dirs.windows_run {
        push(
            run.clone(),
            Category::Runtime,
            "Windows runtime dir (%LOCALAPPDATA%\\Prismattyc\\run)",
            Os::Windows,
        );
    }

    // --- Self-update store: …/prismattyc/updates (all the current/previous/
    //     legacy symlinks, versioned dirs, receipts, lock live inside). ---
    if let Some(data) = &dirs.data_home {
        push(
            data.join("prismattyc").join("updates"),
            Category::Updates,
            "self-update store (versions, current/previous links, lock)",
            Os::Any,
        );
    }

    // --- Persistent data (Spaces, layouts, mailbox, session state). ---
    if let Some(data) = &dirs.data_home {
        let base = data.join("prismattyc");
        for file in DATA_FILES {
            push(
                base.join(file),
                Category::Data,
                &format!("data file {file}"),
                Os::Any,
            );
        }
        for dir in DATA_DIRS {
            push(
                base.join(dir),
                Category::Data,
                &format!("data directory {dir}/"),
                Os::Any,
            );
        }
    }

    // --- Config: config.toml. ---
    if let Some(config) = &dirs.config_home {
        push(
            config.join("prismattyc").join("config.toml"),
            Category::Config,
            "config.toml",
            Os::Any,
        );
    }

    // --- Runtime: per-instance sockets, pid/log/ack files. ---
    //     Only files inside the known runtime dirs matching the product's own
    //     naming (pmux*.sock and their siblings) are enumerated. No globbing
    //     outside these product dirs.
    for dir in &dirs.runtime_dirs {
        push(
            dir.clone(),
            Category::Runtime,
            "runtime dir (sockets, pids, logs)",
            Os::Any,
        );
    }

    // --- OS integration. ---
    if let Some(data) = &dirs.data_home {
        push(
            data.join("applications").join("prismattyc-host.desktop"),
            Category::Integration,
            "desktop entry",
            Os::Linux,
        );
        // Icons: only the product's own icon files, by exact name.
        for size in ["16", "24", "32", "48", "64", "128", "256", "512"] {
            push(
                data.join(format!("icons/hicolor/{size}x{size}/apps/prismattyc.png")),
                Category::Integration,
                "hicolor icon",
                Os::Linux,
            );
        }
        push(
            data.join("icons/hicolor/scalable/apps/prismattyc.svg"),
            Category::Integration,
            "scalable icon",
            Os::Linux,
        );
        // Man pages under the XDG data home man dir.
        for page in [
            "prismattyc",
            "prismattyc-host",
            "pmux",
            "pmuxd",
            "pmux-attach",
            "pmux-mcp",
            "pmux-pane-write",
        ] {
            push(
                data.join(format!("man/man1/{page}.1")),
                Category::Integration,
                "man page",
                Os::Any,
            );
        }
    }
    if let Some(config) = &dirs.config_home {
        push(
            config.join("systemd/user/pmuxd.service"),
            Category::Integration,
            "systemd user unit",
            Os::Linux,
        );
    }
    if let Some(home) = &dirs.home {
        // terminfo entries the installer compiled into ~/.terminfo. Only the
        // product's own capability files, by exact name — never the whole dir.
        for entry in ["p/prismattyc", "p/prismattyc-host"] {
            push(
                home.join(".terminfo").join(entry),
                Category::Integration,
                "terminfo entry",
                Os::Any,
            );
        }
    }

    // Stable order, de-duplicated (bin dirs may overlap, e.g. CARGO_HOME/bin).
    items.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut seen = BTreeSet::new();
    items.retain(|item| seen.insert(item.path.clone()));
    items
}

/// Parsed uninstall flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub yes: bool,
    pub dry_run: bool,
    pub keep_data: bool,
}

impl Options {
    /// Parse `uninstall` flags. Unknown flags error with the bad flag and the
    /// fix; `--help`/`-h` prints usage and exits 0.
    pub fn parse(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self> {
        let mut opts = Self::default();
        for arg in args {
            match arg.as_ref() {
                "--yes" | "-y" => opts.yes = true,
                "--dry-run" | "-n" => opts.dry_run = true,
                "--keep-data" => opts.keep_data = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!(
                    "uninstall: unknown flag {other}\n\
                     valid flags: --yes/-y, --dry-run/-n, --keep-data.\n\
                     Run `pmux uninstall --help` for usage."
                ),
            }
        }
        Ok(opts)
    }
}

pub fn print_help() {
    eprintln!(
        "\
pmux uninstall — remove everything Prismattyc installed on this machine

USAGE:
    prismattyc uninstall [--yes] [--dry-run] [--keep-data]
    pmux uninstall [--yes] [--dry-run] [--keep-data]

    (no flags)   remove everything, including Spaces and config, after a prompt
    --yes, -y    skip the confirmation prompt
    --dry-run,-n list what would be removed and exit; remove nothing
    --keep-data  keep Spaces, layouts, mailbox, session state and config

The daemon and sessions are stopped first (with a warning). Anything that
cannot be removed is reported with what, why and the fix; the rest is still
removed. Exit status is non-zero if anything is left behind."
    );
}

/// Result of trying to remove one item.
#[derive(Debug)]
enum Outcome {
    Removed,
    Absent,
    Failed { why: String, fix: String },
}

/// Entry point shared by both binaries.
pub fn run_uninstall(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<()> {
    let opts = Options::parse(args)?;
    let dirs = Dirs::from_env();
    run_with(
        &opts,
        &dirs,
        &mut std::io::stdin().lock(),
        true,
        &live_socket_under,
    )
}

/// Testable core. `interactive` gates the confirmation prompt and daemon
/// shutdown side effects; tests pass `false` and a scripted reader.
/// `live_probe` reports a live pmux socket under a runtime dir, so the
/// live-daemon guard can be exercised without a real daemon; production
/// passes [`live_socket_under`].
pub fn run_with(
    opts: &Options,
    dirs: &Dirs,
    input: &mut impl std::io::BufRead,
    interactive: bool,
    live_probe: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<()> {
    run_with_shutdown(opts, dirs, input, interactive, live_probe, &stop_daemons)
}

fn run_with_shutdown(
    opts: &Options,
    dirs: &Dirs,
    input: &mut impl std::io::BufRead,
    interactive: bool,
    live_probe: &dyn Fn(&Path) -> Option<PathBuf>,
    shutdown: &dyn Fn(&Dirs),
) -> Result<()> {
    let mut plan = removal_plan(dirs);
    if opts.keep_data {
        plan.retain(|item| !item.category.is_data());
    }

    // Report the plan grouped by category.
    println!("Prismattyc uninstall — the following will be removed:\n");
    let mut current: Option<Category> = None;
    for item in &plan {
        if current != Some(item.category) {
            println!("  [{}]", item.category.label());
            current = Some(item.category);
        }
        println!("    {}  ({})", item.path.display(), item.what);
    }
    if opts.keep_data {
        println!("\n  --keep-data: Spaces, layouts, mailbox, session state and config are kept.");
    }
    println!();

    if opts.dry_run {
        println!(
            "Dry run: nothing was removed. {} item(s) listed.",
            plan.len()
        );
        return Ok(());
    }

    // Confirm unless --yes.
    if !opts.yes {
        if !interactive {
            bail!("refusing to remove without confirmation; pass --yes in non-interactive use");
        }
        print!("Remove everything listed above? [y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            println!("Aborted. Nothing was removed.");
            return Ok(());
        }
    }

    // Stop only after the user confirms. An abandoned prompt must not interrupt
    // live sessions or report that nothing changed after stopping the daemon.
    if interactive {
        eprintln!("Warning: this stops the Prismattyc daemon and all sessions.");
        shutdown(dirs);
    }

    // Remove, collecting per-item outcomes.
    let mut removed = 0usize;
    let mut leftovers: Vec<(PathBuf, String, String)> = Vec::new();
    for item in &plan {
        // Safety guard: never delete a runtime dir that still hosts a live
        // pmux daemon. Deleting a live socket forces the daemon to restart
        // and kills every seat. This refuses with an explicit error and
        // leaves the dir as a reported leftover; the rest is still removed.
        if item.category == Category::Runtime {
            if let Some(socket) = live_probe(&item.path) {
                leftovers.push((
                    item.path.clone(),
                    format!(
                        "a Prismattyc daemon is still running (live socket {})",
                        socket.display()
                    ),
                    "stop it first with `pmux stop` (or quit the app), then rerun uninstall"
                        .to_string(),
                ));
                continue;
            }
        }
        match remove_item(item) {
            Outcome::Removed => removed += 1,
            Outcome::Absent => {}
            Outcome::Failed { why, fix } => {
                leftovers.push((item.path.clone(), why, fix));
            }
        }
    }

    println!("\nRemoved {removed} item(s).");
    if leftovers.is_empty() {
        println!("Prismattyc is fully removed.");
        return Ok(());
    }

    eprintln!("\n{} item(s) could not be removed:", leftovers.len());
    for (path, why, fix) in &leftovers {
        eprintln!("  {}\n    why: {why}\n    fix: {fix}", path.display());
    }
    bail!(
        "{} item(s) remain; see the list above. Prismattyc is not fully removed.",
        leftovers.len()
    );
}

/// Remove a single inventory path safely.
///
/// Symlinks are removed as links: [`std::fs::symlink_metadata`] does not
/// follow the final component, so a symlink is deleted with `remove_file`
/// and its target is never touched. A real directory is removed
/// recursively, but only because it is a known product directory in the
/// inventory — there is no traversal outside these paths.
fn remove_path(path: &Path) -> Outcome {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Outcome::Absent,
        Err(e) => {
            return Outcome::Failed {
                why: format!("cannot stat: {e}"),
                fix: "check permissions on the parent directory, then rerun".to_string(),
            };
        }
    };

    let file_type = meta.file_type();
    let result = if file_type.is_symlink() || file_type.is_file() {
        std::fs::remove_file(path)
    } else {
        std::fs::remove_dir_all(path)
    };

    match result {
        Ok(()) => Outcome::Removed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Outcome::Failed {
            why: format!("permission denied: {e}"),
            fix: format!(
                "remove it manually, e.g. `rm -rf {}` (may need sudo)",
                path.display()
            ),
        },
        Err(e) => Outcome::Failed {
            why: e.to_string(),
            fix: format!("remove it manually: {}", path.display()),
        },
    }
}

/// If `dir` (or its `prismattyc` subdir) currently hosts a live pmux control
/// socket, return that socket path. Used to refuse deleting a running
/// daemon's runtime dir on Unix and Windows. Returns `None` when nothing live
/// is found — including for a tempdir in tests, which never holds a live socket.
fn live_socket_under(dir: &Path) -> Option<PathBuf> {
    // live_pmux_sockets_in already scans `dir` and `dir/prismattyc`; its
    // socket transport and ownership check are implemented for Windows too.
    crate::live_pmux_sockets_in(dir).into_iter().next()
}

/// Best-effort clean shutdown of any live daemon whose socket lives in a
/// resolved runtime dir. Never fails the uninstall: a daemon that will not
/// stop shows up later as a leftover socket file. Only the resolved
/// `dirs.runtime_dirs` are scanned, so a redirected (sandboxed) invocation
/// cannot reach the real daemon. The shared local-socket transport works on
/// Unix and Windows.
fn stop_daemons(dirs: &Dirs) {
    let mut dirs_to_scan: BTreeSet<PathBuf> = BTreeSet::new();
    for rt in &dirs.runtime_dirs {
        dirs_to_scan.insert(rt.clone());
        // live_pmux_sockets_in scans `dir` and `dir/prismattyc`; adding the
        // parent recovers the `$XDG_RUNTIME_DIR` level for a `…/prismattyc`
        // runtime dir, without reading outside the resolved set.
        if let Some(parent) = rt.parent() {
            dirs_to_scan.insert(parent.to_path_buf());
        }
    }
    for dir in dirs_to_scan {
        for socket in crate::live_pmux_sockets_in(&dir) {
            eprintln!("  stopping daemon at {}", socket.display());
            let _ = shutdown_socket(&socket);
        }
    }
}

#[cfg(any(unix, windows))]
fn shutdown_socket(socket: &Path) -> Result<()> {
    use std::io::BufReader;
    use std::time::Duration;

    let stream = crate::local_socket::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    shutdown_exchange(&mut reader, &mut writer)
}

/// The pmux shutdown protocol exchange over any reader/writer pair, factored
/// out so it can be tested with in-memory buffers (no real socket).
///
/// Writes `RegisterClient`, reads the `ClientRegistered` reply, then writes
/// `ShutdownServer`. JSON-line framing, matching the pmux control client. Best
/// effort: a closed connection or a non-registration reply is not an error,
/// since a daemon that is already stopping may drop the connection.
fn shutdown_exchange<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    use crate::PROTOCOL_VERSION;
    use crate::{ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData};

    let register = ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id: 1,
    };
    serde_json::to_writer(&mut *writer, &register)?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let response: ControlResponse = serde_json::from_str(&line)?;
    let ControlResponseBody::Ok {
        response: ControlResponseData::ClientRegistered { client_id },
    } = response.body
    else {
        return Ok(());
    };

    let shutdown = ControlRequest::ShutdownServer {
        version: PROTOCOL_VERSION,
        request_id: 2,
        client_id,
    };
    serde_json::to_writer(&mut *writer, &shutdown)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// A convenience for tests and callers: the set of paths the plan would touch.
#[must_use]
pub fn planned_paths(dirs: &Dirs, keep_data: bool) -> Vec<PathBuf> {
    removal_plan(dirs)
        .into_iter()
        .filter(|item| !(keep_data && item.category.is_data()))
        .map(|item| item.path)
        .collect()
}

/// Include only PATH links with a matching Prismattyc ownership record. The
/// ordinary inventory stays pure; this filesystem check is kept at the
/// uninstall boundary and uses only paths derived from `Dirs`.
fn removal_plan(dirs: &Dirs) -> Vec<Item> {
    let mut items = inventory(dirs);
    if dirs.os == Os::Macos {
        #[cfg(any(target_os = "macos", all(test, unix)))]
        {
            let mut candidates = Vec::new();
            if let Some(home) = &dirs.home {
                candidates.push(home.join(".local/bin"));
            }
            if let Some(system_bin) = &dirs.system_bin_dir {
                candidates.push(system_bin.clone());
            }
            candidates.sort();
            candidates.dedup();
            for directory in candidates {
                let link = directory.join("pmux");
                if crate::path_shim::is_managed_link_or_absent(&link) {
                    items.push(Item {
                        path: link,
                        category: Category::Binaries,
                        what: "app-managed pmux PATH link".to_string(),
                    });
                }
            }
        }
    }
    items.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.path.cmp(&right.path))
    });
    items
}

fn remove_item(item: &Item) -> Outcome {
    if item.what == "app-managed pmux PATH link" {
        #[cfg(any(target_os = "macos", all(test, unix)))]
        {
            return match crate::path_shim::remove_owned_link(&item.path) {
                Ok(crate::path_shim::RemoveOutcome::Removed) => Outcome::Removed,
                Ok(crate::path_shim::RemoveOutcome::Absent) => Outcome::Absent,
                Ok(crate::path_shim::RemoveOutcome::Preserved) => Outcome::Failed {
                    why: "the pmux path changed after the plan was printed; it was left untouched"
                        .to_string(),
                    fix: format!("inspect {} and rerun uninstall", item.path.display()),
                },
                Err(error) => Outcome::Failed {
                    why: error.to_string(),
                    fix: format!(
                        "inspect {} and remove the managed link manually",
                        item.path.display()
                    ),
                },
            };
        }
        #[cfg(not(any(target_os = "macos", all(test, unix))))]
        {
            return Outcome::Absent;
        }
    }
    remove_path(&item.path)
}

/// Build a fully sandboxed [`Dirs`] rooted at `root` (a tempdir). Every field,
/// including the system Applications and bin dirs, comes from `root`, so a
/// plan built from it can never name a real machine path. Tests use this
/// exclusively; production uses [`Dirs::from_env`].
#[must_use]
pub fn dirs_for_test(root: impl AsRef<Path>, os: Os) -> Dirs {
    let root = root.as_ref();
    Dirs {
        home: Some(root.join("home")),
        config_home: Some(root.join("config")),
        data_home: Some(root.join("data")),
        runtime_dirs: vec![root.join("runtime")],
        windows_run: Some(root.join("win-run")),
        bin_dirs: vec![root.join("home/.local/bin"), root.join("home/.cargo/bin")],
        // Sandboxed stand-in for /Applications — inside the tempdir root.
        system_app_dir: Some(root.join("Applications")),
        system_bin_dir: Some(root.join("usr-local-bin")),
        os,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A self-cleaning scratch directory under the system temp dir. Matches
    /// the crate's existing test convention (no `tempfile` dependency).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "prism-uninstall-{tag}-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Best-effort: restore perms on any dir we locked down, then rm.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fn chmod_rec(p: &Path) {
                    if let Ok(meta) = std::fs::symlink_metadata(p) {
                        if meta.file_type().is_dir() {
                            let mut perms = meta.permissions();
                            perms.set_mode(0o700);
                            let _ = std::fs::set_permissions(p, perms);
                            if let Ok(rd) = std::fs::read_dir(p) {
                                for e in rd.flatten() {
                                    chmod_rec(&e.path());
                                }
                            }
                        }
                    }
                }
                chmod_rec(&self.0);
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_dirs(root: &Path, os: Os) -> Dirs {
        dirs_for_test(root, os)
    }

    #[test]
    fn inventory_covers_every_category_on_linux() {
        let dirs = fake_dirs(Path::new("/fake"), Os::Linux);
        let items = inventory(&dirs);
        let cats: BTreeSet<Category> = items.iter().map(|i| i.category).collect();
        assert!(cats.contains(&Category::Binaries));
        assert!(cats.contains(&Category::Updates));
        assert!(cats.contains(&Category::Runtime));
        assert!(cats.contains(&Category::Data));
        assert!(cats.contains(&Category::Config));
        assert!(cats.contains(&Category::Integration));
    }

    #[test]
    fn inventory_includes_known_data_paths() {
        let dirs = fake_dirs(Path::new("/fake"), Os::Linux);
        let paths: BTreeSet<PathBuf> = inventory(&dirs).into_iter().map(|i| i.path).collect();
        assert!(paths.contains(&PathBuf::from("/fake/data/prismattyc/mail.db")));
        assert!(paths.contains(&PathBuf::from("/fake/data/prismattyc/spaces")));
        assert!(paths.contains(&PathBuf::from("/fake/data/prismattyc/layouts")));
        assert!(paths.contains(&PathBuf::from("/fake/data/prismattyc/updates")));
        assert!(paths.contains(&PathBuf::from("/fake/config/prismattyc/config.toml")));
    }

    #[test]
    fn macos_only_items_do_not_leak_onto_linux() {
        let linux = inventory(&fake_dirs(Path::new("/fake"), Os::Linux));
        assert!(!linux.iter().any(|i| i.path.ends_with("Prismattyc.app")));
        let mac = inventory(&fake_dirs(Path::new("/fake"), Os::Macos));
        assert!(mac.iter().any(|i| i.path.ends_with("Prismattyc.app")));
        // Linux-only integration (desktop entry) must not appear on macOS.
        assert!(!mac
            .iter()
            .any(|i| i.path.ends_with("prismattyc-host.desktop")));
    }

    #[cfg(unix)]
    #[test]
    fn macos_plan_includes_only_the_owned_pmux_path_link() {
        let tmp = Scratch::new("pmux-shim-plan");
        let dirs = fake_dirs(tmp.path(), Os::Macos);
        let target = tmp
            .path()
            .join("Applications/Prismattyc.app/Contents/MacOS/pmux");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"bundled pmux").unwrap();

        let link = tmp.path().join("home/.local/bin/pmux");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let marker = link.parent().unwrap().join(".pmux-prismattyc-shim.json");
        std::fs::write(
            &marker,
            serde_json::to_vec(&serde_json::json!({"version": 1, "target": target})).unwrap(),
        )
        .unwrap();

        let cargo_pmux = tmp.path().join("home/.cargo/bin/pmux");
        std::fs::create_dir_all(cargo_pmux.parent().unwrap()).unwrap();
        std::fs::write(&cargo_pmux, b"separate Cargo install").unwrap();

        let items = removal_plan(&dirs);
        let shim = items
            .iter()
            .find(|item| item.what == "app-managed pmux PATH link")
            .expect("owned app link belongs in the plan");
        assert_eq!(shim.path, link);
        assert!(!items.iter().any(|item| item.path == cargo_pmux));

        assert!(matches!(remove_item(shim), Outcome::Removed));
        assert!(!link.exists());
        assert!(!marker.exists());
        assert!(target.exists());
        assert!(cargo_pmux.exists());
    }

    #[test]
    fn keep_data_drops_only_data_and_config() {
        let dirs = fake_dirs(Path::new("/fake"), Os::Linux);
        let kept = planned_paths(&dirs, true);
        assert!(!kept.iter().any(|p| p.ends_with("prismattyc/mail.db")));
        assert!(!kept.iter().any(|p| p.ends_with("prismattyc/config.toml")));
        // Non-data survives keep-data.
        assert!(kept.iter().any(|p| p.ends_with("prismattyc/updates")));
    }

    #[test]
    fn options_parse_flags_and_reject_unknown() {
        assert_eq!(
            Options::parse(["--yes"]).unwrap(),
            Options {
                yes: true,
                dry_run: false,
                keep_data: false
            }
        );
        assert_eq!(
            Options::parse(["-n", "--keep-data"]).unwrap(),
            Options {
                yes: false,
                dry_run: true,
                keep_data: true
            }
        );
        let err = Options::parse(["--bogus"]).unwrap_err();
        assert!(format!("{err}").contains("unknown flag --bogus"));
    }

    #[test]
    fn dry_run_removes_nothing() {
        let tmp = Scratch::new("dry");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        // Create a file that is in the inventory.
        let updates = tmp.path().join("data/prismattyc/updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("marker"), b"x").unwrap();

        let opts = Options {
            yes: true,
            dry_run: true,
            keep_data: false,
        };
        run_with(&opts, &dirs, &mut Cursor::new(b""), false, &|_| None).unwrap();
        assert!(updates.exists(), "dry-run must not remove anything");
    }

    #[test]
    fn declining_confirmation_does_not_stop_sessions_or_remove_files() {
        let tmp = Scratch::new("cancel");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        let updates = tmp.path().join("data/prismattyc/updates");
        std::fs::create_dir_all(&updates).unwrap();
        let marker = updates.join("marker");
        std::fs::write(&marker, b"keep until confirmed").unwrap();

        let shutdown_called = std::cell::Cell::new(false);
        let result = run_with_shutdown(
            &Options::default(),
            &dirs,
            &mut Cursor::new(b"n\n"),
            true,
            &|_| None,
            &|_| shutdown_called.set(true),
        );

        result.unwrap();
        assert!(!shutdown_called.get(), "declining must not stop the daemon");
        assert!(marker.exists(), "declining must leave the plan untouched");
    }

    #[test]
    fn keep_data_preserves_data_but_removes_updates() {
        let tmp = Scratch::new("keep");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        let base = tmp.path().join("data/prismattyc");
        std::fs::create_dir_all(base.join("spaces")).unwrap();
        std::fs::create_dir_all(base.join("updates")).unwrap();
        std::fs::write(base.join("mail.db"), b"db").unwrap();

        let opts = Options {
            yes: true,
            dry_run: false,
            keep_data: true,
        };
        run_with(&opts, &dirs, &mut Cursor::new(b""), false, &|_| None).unwrap();
        assert!(base.join("spaces").exists(), "keep-data keeps Spaces");
        assert!(base.join("mail.db").exists(), "keep-data keeps mailbox");
        assert!(!base.join("updates").exists(), "updates still removed");
    }

    #[test]
    fn full_uninstall_removes_everything_and_reports_gone() {
        let tmp = Scratch::new("full");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        let base = tmp.path().join("data/prismattyc");
        std::fs::create_dir_all(base.join("spaces")).unwrap();
        std::fs::create_dir_all(base.join("updates")).unwrap();
        std::fs::write(base.join("mail.db"), b"db").unwrap();
        std::fs::create_dir_all(tmp.path().join("home/.local/bin")).unwrap();
        std::fs::write(tmp.path().join("home/.local/bin/pmux"), b"bin").unwrap();

        let opts = Options {
            yes: true,
            dry_run: false,
            keep_data: false,
        };
        run_with(&opts, &dirs, &mut Cursor::new(b""), false, &|_| None).unwrap();
        assert!(!base.join("spaces").exists());
        assert!(!base.join("mail.db").exists());
        assert!(!base.join("updates").exists());
        assert!(!tmp.path().join("home/.local/bin/pmux").exists());
    }

    #[test]
    fn symlink_is_removed_as_link_target_untouched() {
        // Path safety: removing a symlinked inventory path must delete the
        // link, never the file it points at.
        let tmp = Scratch::new("symlink");
        let outside = tmp.path().join("precious.txt");
        std::fs::write(&outside, b"keep me").unwrap();

        let link = tmp.path().join("data/prismattyc/updates");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let outcome = remove_path(&link);
        assert!(matches!(outcome, Outcome::Removed));
        assert!(!link.exists(), "the symlink itself is gone");
        assert!(outside.exists(), "the link target must be untouched");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
    }

    #[test]
    fn partial_failure_reports_leftover_and_errs() {
        // A path that cannot be removed is reported; the run errors non-zero
        // but other items are still removed.
        let tmp = Scratch::new("partial");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        let base = tmp.path().join("data/prismattyc");
        std::fs::create_dir_all(base.join("updates")).unwrap();
        std::fs::write(base.join("mail.db"), b"db").unwrap();

        // Make the updates dir unremovable by removing write+exec on its
        // parent (POSIX: cannot unlink children without parent write perm).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(base.join("updates/inner")).unwrap();
            std::fs::write(base.join("updates/inner/file"), b"x").unwrap();
            let mut perms = std::fs::metadata(base.join("updates"))
                .unwrap()
                .permissions();
            perms.set_mode(0o500); // r-x: cannot remove children
            std::fs::set_permissions(base.join("updates"), perms).unwrap();
        }

        let opts = Options {
            yes: true,
            dry_run: false,
            keep_data: false,
        };
        let result = run_with(&opts, &dirs, &mut Cursor::new(b""), false, &|_| None);

        // mail.db (removable) is gone even though updates failed.
        assert!(
            !base.join("mail.db").exists(),
            "removable items still removed"
        );

        #[cfg(unix)]
        {
            assert!(result.is_err(), "leftover must make the run error");
            let msg = format!("{}", result.unwrap_err());
            assert!(msg.contains("remain"), "error names leftovers: {msg}");
            // Restore perms so tempdir cleanup works.
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(base.join("updates"))
                .unwrap()
                .permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(base.join("updates"), perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = result;
    }

    #[test]
    fn only_inventory_paths_are_touched() {
        // Path safety: a sibling file outside the inventory is never removed.
        let tmp = Scratch::new("safety");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        let base = tmp.path().join("data/prismattyc");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("mail.db"), b"db").unwrap();
        // A neighbour the product never created.
        let neighbour = tmp.path().join("data/prismattyc/UNRELATED.txt");
        std::fs::write(&neighbour, b"not ours").unwrap();

        let opts = Options {
            yes: true,
            dry_run: false,
            keep_data: false,
        };
        run_with(&opts, &dirs, &mut Cursor::new(b""), false, &|_| None).unwrap();
        assert!(!base.join("mail.db").exists(), "inventory item removed");
        assert!(neighbour.exists(), "non-inventory neighbour untouched");
    }

    #[test]
    fn overridden_home_never_targets_real_applications() {
        // Regression guard: no plan path may reference real host-global dirs.
        // Every path must live under
        // the injected tempdir root.
        let tmp = Scratch::new("noreal");
        let root = tmp.path();
        for os in [Os::Macos, Os::Linux, Os::Windows] {
            let dirs = fake_dirs(root, os);
            for item in inventory(&dirs) {
                assert!(
                    item.path.starts_with(root),
                    "inventory path escapes the sandbox root: {} (os {os:?})",
                    item.path.display()
                );
                assert!(
                    !item.path.starts_with("/Applications"),
                    "inventory must never target the real /Applications: {}",
                    item.path.display()
                );
                assert!(
                    !item.path.starts_with("/tmp/prismattyc-"),
                    "inventory must never target a real /tmp runtime dir: {}",
                    item.path.display()
                );
            }
            for item in removal_plan(&dirs) {
                assert!(
                    item.path.starts_with(root),
                    "removal plan escapes the sandbox root: {} (os {os:?})",
                    item.path.display()
                );
                assert!(
                    !item.path.starts_with("/usr/local/bin"),
                    "redirected removal plan must not reach the real PATH dir: {}",
                    item.path.display()
                );
            }
        }
    }

    /// A redirected (sandboxed) env snapshot: HOME under a tempdir, distinct
    /// from `real_home`, with explicit XDG + CARGO_HOME overrides.
    fn redirected_snapshot(root: &Path, os: Os) -> EnvSnapshot {
        EnvSnapshot {
            home: Some(root.join("home")),
            xdg_config_home: Some(root.join("config")),
            xdg_data_home: Some(root.join("data")),
            xdg_runtime_dir: Some(root.join("run")),
            cargo_home: Some(root.join("cargo")),
            appdata: Some(root.join("appdata")),
            localappdata: Some(root.join("localappdata")),
            real_home: Some(PathBuf::from("/Users/real-user")),
            uid: Some(501),
            os,
        }
    }

    /// A non-redirected env snapshot: HOME equals `real_home`, no XDG or
    /// CARGO_HOME overrides — a real user account.
    fn default_snapshot(os: Os) -> EnvSnapshot {
        let real = PathBuf::from("/Users/real-user");
        EnvSnapshot {
            home: Some(real.clone()),
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            cargo_home: None,
            appdata: None,
            localappdata: None,
            real_home: Some(real),
            uid: Some(501),
            os,
        }
    }

    #[test]
    fn production_resolver_redirected_env_omits_host_global_paths() {
        // Through the production resolver, redirected env must never yield
        // /Applications, /usr/local/bin, or /tmp/prismattyc-<uid>. This is
        // the assertion that runs before any mutating operation.
        let tmp = Scratch::new("prodredir");
        for os in [Os::Macos, Os::Linux] {
            let env = redirected_snapshot(tmp.path(), os);
            assert!(env.is_redirected(), "sandbox env must read as redirected");
            let dirs = Dirs::resolve(&env);

            // No host-global system app or bin dir on a redirected invocation.
            assert!(
                dirs.system_app_dir.is_none(),
                "redirected env must not set system_app_dir (os {os:?})"
            );
            assert!(
                dirs.system_bin_dir.is_none(),
                "redirected env must not set system_bin_dir (os {os:?})"
            );

            for item in inventory(&dirs) {
                assert!(
                    !item.path.starts_with("/Applications"),
                    "redirected resolver leaked /Applications: {} (os {os:?})",
                    item.path.display()
                );
                assert!(
                    !item.path.starts_with("/tmp/prismattyc-"),
                    "redirected resolver leaked /tmp runtime dir: {} (os {os:?})",
                    item.path.display()
                );
                // Every remaining path stays under the sandbox root.
                assert!(
                    item.path.starts_with(tmp.path()),
                    "redirected resolver path escaped the sandbox: {} (os {os:?})",
                    item.path.display()
                );
            }
            for item in removal_plan(&dirs) {
                assert!(
                    item.path.starts_with(tmp.path()),
                    "redirected removal plan escaped the sandbox: {} (os {os:?})",
                    item.path.display()
                );
            }
        }
    }

    #[test]
    fn production_resolver_default_env_includes_host_global_paths() {
        // The complement: on a real (non-redirected) account, host-global
        // paths ARE part of the plan, so a normal uninstall still cleans them.
        let env = default_snapshot(Os::Macos);
        assert!(
            !env.is_redirected(),
            "default env must not read as redirected"
        );
        let dirs = Dirs::resolve(&env);

        assert_eq!(
            dirs.system_app_dir.as_deref(),
            Some(Path::new("/Applications")),
            "default macOS invocation includes /Applications"
        );
        assert_eq!(
            dirs.system_bin_dir.as_deref(),
            Some(Path::new("/usr/local/bin")),
            "default macOS invocation includes the system bin dir"
        );
        let paths: Vec<PathBuf> = inventory(&dirs).into_iter().map(|i| i.path).collect();
        assert!(
            paths
                .iter()
                .any(|p| p == Path::new("/Applications/Prismattyc.app")),
            "default invocation lists the system app bundle"
        );
        assert!(
            paths.iter().any(|p| p == Path::new("/tmp/prismattyc-501")),
            "default invocation lists the shared /tmp runtime dir"
        );
    }

    #[test]
    fn redirection_signals() {
        // A bare set HOME equal to real_home with no XDG overrides is NOT
        // redirected; any explicit override flips it.
        let real = PathBuf::from("/Users/real-user");
        let base = EnvSnapshot {
            home: Some(real.clone()),
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            cargo_home: None,
            appdata: None,
            localappdata: None,
            real_home: Some(real.clone()),
            uid: Some(501),
            os: Os::Macos,
        };
        assert!(!base.is_redirected());

        let mut with_home = base.clone();
        with_home.home = Some(PathBuf::from("/tmp/sandbox/home"));
        assert!(
            with_home.is_redirected(),
            "a HOME != real_home is redirected"
        );

        let mut with_data = base.clone();
        with_data.xdg_data_home = Some(PathBuf::from("/tmp/sandbox/data"));
        assert!(
            with_data.is_redirected(),
            "XDG_DATA_HOME override is redirected"
        );

        let mut with_rt = base.clone();
        with_rt.xdg_runtime_dir = Some(PathBuf::from("/tmp/sandbox/run"));
        assert!(
            with_rt.is_redirected(),
            "XDG_RUNTIME_DIR override is redirected"
        );

        let mut with_cargo = base;
        with_cargo.cargo_home = Some(PathBuf::from("/tmp/sandbox/cargo"));
        assert!(
            with_cargo.is_redirected(),
            "CARGO_HOME override is redirected"
        );
    }

    #[test]
    fn redirected_home_with_unresolved_real_home_is_redirected() {
        // Reviewer's CHANGES case: HOME is redirected but getpwuid could not
        // resolve the real home (real_home = None) and no XDG/CARGO override
        // is set. Previously is_redirected() fell through to false and
        // host-global paths leaked. It must now read as redirected and the
        // inventory must not contain /Applications, /usr/local/bin, or /tmp/prismattyc-<uid>.
        let env = EnvSnapshot {
            home: Some(PathBuf::from("/tmp/sandbox/home")),
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            cargo_home: None,
            appdata: None,
            localappdata: None,
            real_home: None, // getpwuid failed
            uid: Some(501),
            os: Os::Macos,
        };
        assert!(
            env.is_redirected(),
            "a redirected HOME with an unresolved real_home must be redirected"
        );
        let dirs = Dirs::resolve(&env);
        assert!(
            dirs.system_app_dir.is_none(),
            "no system app dir when the real home is unverified"
        );
        assert!(
            dirs.system_bin_dir.is_none(),
            "no system bin dir when the real home is unverified"
        );
        for item in inventory(&dirs) {
            assert!(
                !item.path.starts_with("/Applications"),
                "must not target real /Applications: {}",
                item.path.display()
            );
            assert!(
                !item.path.starts_with("/tmp/prismattyc-"),
                "must not target the shared /tmp runtime dir: {}",
                item.path.display()
            );
        }
    }

    #[test]
    fn unset_home_is_redirected() {
        // HOME unset and real home unknown: cannot confirm the real account,
        // so treat as redirected (fail safe).
        let env = EnvSnapshot {
            home: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            cargo_home: None,
            appdata: None,
            localappdata: None,
            real_home: None,
            uid: Some(501),
            os: Os::Macos,
        };
        assert!(
            env.is_redirected(),
            "unset HOME must be treated as redirected"
        );
        let dirs = Dirs::resolve(&env);
        assert!(dirs.system_app_dir.is_none());
        assert!(dirs.system_bin_dir.is_none());
    }

    #[test]
    fn confirmed_real_home_still_includes_host_global_paths() {
        // The one path that emits host-global paths must still work: HOME set
        // and equal to the resolved real home, no overrides.
        let real = PathBuf::from("/Users/real-user");
        let env = EnvSnapshot {
            home: Some(real.clone()),
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            cargo_home: None,
            appdata: None,
            localappdata: None,
            real_home: Some(real),
            uid: Some(501),
            os: Os::Macos,
        };
        assert!(!env.is_redirected());
        assert_eq!(
            Dirs::resolve(&env).system_app_dir.as_deref(),
            Some(Path::new("/Applications"))
        );
        assert_eq!(
            Dirs::resolve(&env).system_bin_dir.as_deref(),
            Some(Path::new("/usr/local/bin"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_exchange_registers_then_requests_shutdown() {
        use crate::{ControlResponse, ControlResponseBody, ControlResponseData, PROTOCOL_VERSION};

        // The daemon's reply: a ClientRegistered with a known client_id.
        let reply = ControlResponse {
            version: PROTOCOL_VERSION,
            request_id: 1,
            body: ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id: 42 },
            },
        };
        let mut reply_line = serde_json::to_string(&reply).unwrap();
        reply_line.push('\n');

        let mut reader = std::io::Cursor::new(reply_line.into_bytes());
        let mut writer: Vec<u8> = Vec::new();
        shutdown_exchange(&mut reader, &mut writer).unwrap();

        // Two JSON lines were written: RegisterClient, then ShutdownServer
        // carrying the client_id from the reply.
        let written = String::from_utf8(writer).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 2, "wrote register + shutdown: {written}");
        assert!(lines[0].contains("register_client"), "first: {}", lines[0]);
        assert!(lines[1].contains("shutdown_server"), "second: {}", lines[1]);
        assert!(
            lines[1].contains("\"client_id\":42"),
            "shutdown uses the registered client_id: {}",
            lines[1]
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_exchange_tolerates_closed_connection() {
        // A daemon already stopping may close before replying: not an error,
        // and no shutdown request is sent without a client_id.
        let mut reader = std::io::Cursor::new(Vec::new()); // EOF immediately
        let mut writer: Vec<u8> = Vec::new();
        shutdown_exchange(&mut reader, &mut writer).unwrap();
        let written = String::from_utf8(writer).unwrap();
        // Only the register line was written.
        assert_eq!(written.lines().count(), 1, "only register: {written}");
        assert!(written.contains("register_client"));
    }

    #[test]
    fn live_daemon_runtime_dir_is_refused_not_deleted() {
        // The core safety fix: a runtime dir hosting a live daemon must be
        // refused with an explicit error and left in place, while the rest is
        // still removed. The probe stands in for a real live socket.
        let tmp = Scratch::new("live");
        let dirs = fake_dirs(tmp.path(), Os::Linux);
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("pmux.sock"), b"sock").unwrap();
        let base = tmp.path().join("data/prismattyc");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("mail.db"), b"db").unwrap();

        let runtime_for_probe = runtime.clone();
        let probe = move |dir: &Path| -> Option<PathBuf> {
            if dir == runtime_for_probe {
                Some(runtime_for_probe.join("pmux.sock"))
            } else {
                None
            }
        };

        let opts = Options {
            yes: true,
            dry_run: false,
            keep_data: false,
        };
        let result = run_with(&opts, &dirs, &mut Cursor::new(b""), false, &probe);

        assert!(runtime.exists(), "live runtime dir must not be deleted");
        assert!(
            runtime.join("pmux.sock").exists(),
            "live socket must not be deleted"
        );
        assert!(!base.join("mail.db").exists(), "non-runtime items removed");
        assert!(result.is_err(), "a live daemon must make the run error");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("remain"), "error names leftovers: {msg}");
    }

    #[test]
    fn live_windows_runtime_dir_is_refused_not_deleted() {
        let tmp = Scratch::new("live-windows");
        let dirs = fake_dirs(tmp.path(), Os::Windows);
        let runtime = tmp.path().join("win-run");
        std::fs::create_dir_all(&runtime).unwrap();
        let socket = runtime.join("pmux.sock");
        std::fs::write(&socket, b"socket marker").unwrap();

        let runtime_for_probe = runtime.clone();
        let socket_for_probe = socket.clone();
        let probe = move |dir: &Path| -> Option<PathBuf> {
            (dir == runtime_for_probe).then(|| socket_for_probe.clone())
        };
        let opts = Options {
            yes: true,
            dry_run: false,
            keep_data: false,
        };

        let result = run_with(&opts, &dirs, &mut Cursor::new(b""), false, &probe);

        assert!(runtime.exists(), "live Windows runtime dir must be kept");
        assert!(socket.exists(), "live Windows socket must be kept");
        assert!(
            result.is_err(),
            "a live Windows daemon must make the run error"
        );
        assert!(
            result.unwrap_err().to_string().contains("remain"),
            "error should name the leftover runtime dir"
        );
    }
}
