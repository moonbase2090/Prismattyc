//! Long-lived local Prismattyc mux server (Phase 2B).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use prismattyc_mux::{
    default_socket_path, load_mux_section, prism_config_path, resolve_remote_size, ControlPlane,
    ControlServer, Domain, SpawnSpec, WindowBounds,
};

/// Seat env keys folded from the server process environment into the bootstrap
/// pane's `SpawnSpec` (ADR-0037). Present only when a supervisor injects
/// a minted `hive seat` map before launch — never invents values.
const HIVE_SEAT_ENV_KEYS: &[&str] = &["HIVE_CELL", "HIVE_CELL_SOCKET", "HIVE_CELL_TOKEN"];

/// Copy ambient Hive seat variables into a spawn env map (non-empty values only).
fn fold_hive_seat_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in HIVE_SEAT_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                env.insert((*key).to_string(), value.to_string());
            }
        }
    }
    env
}

struct Cli {
    socket: PathBuf,
    cols: u32,
    rows: u32,
    /// `--experimental-rich` or `PRISMATTYC_EXPERIMENTAL_RICH` was given.
    /// Rich mode is gone; both are still accepted for one release.
    removed_rich_flag: bool,
    experimental_activity: bool,
    restore_workspace: bool,
    /// A live update: the previous server's record is on this descriptor.
    handoff_fd: Option<i32>,
    program: String,
    argv: Vec<String>,
}

impl Cli {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut socket = None;
        let mut cols = 80;
        let mut rows = 24;
        let mut removed_rich_flag = matches!(
            std::env::var("PRISMATTYC_EXPERIMENTAL_RICH")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "on" | "yes"
        );
        let mut experimental_activity = false;
        let mut program = None;
        let mut restore_workspace = false;
        let mut handoff_fd = None;
        let mut argv = Vec::new();
        while let Some(arg) = args.next() {
            if arg == "--" {
                if program.is_none() {
                    program = args.next();
                }
                argv.extend(args);
                break;
            }
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("{}", prismattyc_core::bin_version("pmuxd"));
                    std::process::exit(0);
                }
                "--bundled-plugins" => {
                    // A running daemon asks the installed one which official plug-in
                    // versions it would set up at start.
                    println!(
                        "{}",
                        serde_json::to_string(&prismattyc_mux::plugins::bundled_defaults()?)?
                    );
                    std::process::exit(0);
                }
                "--socket" if program.is_none() => {
                    socket = Some(PathBuf::from(
                        args.next().context("--socket requires an absolute path")?,
                    ));
                }
                "--cols" if program.is_none() => {
                    cols = parse_dimension(args.next(), "--cols")?;
                }
                "--rows" if program.is_none() => {
                    rows = parse_dimension(args.next(), "--rows")?;
                }
                "--experimental-rich" if program.is_none() => {
                    removed_rich_flag = true;
                }
                "--experimental-activity" if program.is_none() => experimental_activity = true,
                "--restore-workspace" if program.is_none() => restore_workspace = true,
                prismattyc_mux::handoff::FD_ARG if program.is_none() => {
                    let value = args.next().context("--handoff-fd requires a descriptor")?;
                    handoff_fd = Some(value.parse().context("--handoff-fd is not a number")?);
                }
                _ if program.is_none() => program = Some(arg),
                _ => argv.push(arg),
            }
        }
        let socket = socket.unwrap_or(default_socket_path("default")?);
        if !socket.is_absolute() {
            bail!("--socket must be an absolute path");
        }
        Ok(Self {
            socket,
            cols,
            rows,
            removed_rich_flag,
            experimental_activity,
            restore_workspace,
            handoff_fd,
            program: program.unwrap_or_else(prismattyc_mux::platform::default_shell),
            argv,
        })
    }
}

fn parse_dimension(value: Option<String>, flag: &str) -> Result<u32> {
    value
        .with_context(|| format!("{flag} requires a positive integer"))?
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0 && *value <= u16::MAX as u32)
        .with_context(|| format!("{flag} must be in 1..={}", u16::MAX))
}

fn print_help() {
    eprintln!(
        "\
pmuxd — long-lived local Prismattyc mux server

USAGE:
    pmuxd --bundled-plugins
    pmuxd [--socket PATH] [--cols N] [--rows N] [--experimental-activity] [--restore-workspace] [PROGRAM [ARGS...]]
    pmuxd --socket /absolute/path.sock -- /bin/bash -l

The activity feed is on by default: bounded, process-local activity history
for explicit publishers and observers. experimental_activity = false under
[mux] in the Prismattyc config file turns it off. pmuxd watches the config file
and starts or stops the feed live when the key changes, without a restart.
--experimental-activity keeps the feed on for this daemon's life.

The server owns PTYs, emulators, topology, leases, and authoritative geometry.
Clients attach over a same-user 0600 Unix socket. Exiting a client does not
terminate this server or its children; terminate the server explicitly.

Hive seat: if HIVE_CELL / HIVE_CELL_SOCKET / HIVE_CELL_TOKEN are set in
this process's environment, they are folded into the bootstrap pane spawn env.
Set HIVE_SOCKET to arm supervisor.cell_exited on the pane liveness edge (the
socket path is NOT injected into the child). The supervisor send runs in a
clean-env child so a process-level HIVE_CELL_TOKEN cannot re-attribute the
operator connection as Cell (hived reads /proc/pid/environ at exec time).

Guest discovery: every child receives PRISMATTYC_PANE_ID (decimal, never
reused) and PMUX_SOCKET (this --socket path). A guest can set attach chrome
with `pmux status-set TEXT`. PRISMATTYC_SESSION_ID is not stamped because
MovePane can change session without respawn.
"
    );
}

fn main() -> Result<()> {
    if prismattyc_mux::plugins::print_bundled_identity()? {
        return Ok(());
    }
    // A running server asks the installed binary which live-update records
    // it can read.
    if std::env::args().nth(1).as_deref() == Some(prismattyc_mux::handoff::PROBE_ARG) {
        println!(
            "{}",
            serde_json::to_string(&prismattyc_mux::handoff::ProbeReply::current())?
        );
        return Ok(());
    }
    // This process is a terminal: it gives each pane its own program resume
    // token and never passes on one it was given (OSC 7577).
    prismattyc_mux::program_resume::forget_inherited_token();
    // Clean-env one-shot for supervisor.cell_exited (see prismattyc_mux::supervisor).
    let mut argv = std::env::args();
    let _argv0 = argv.next();
    let internal = argv.next();
    if internal.as_deref() == Some(prismattyc_mux::plugins::INTERNAL_WORKER_ARG) {
        return prismattyc_mux::plugins::run_internal_worker();
    }
    if internal.as_deref() == Some(prismattyc_mux::plugins::INTERNAL_ENDPOINT_ARG) {
        return prismattyc_mux::plugins::run_endpoint_worker();
    }
    if internal.as_deref() == Some(prismattyc_mux::supervisor::INTERNAL_SEND_ARG) {
        return prismattyc_mux::supervisor::run_internal_send().map_err(anyhow::Error::msg);
    }
    prismattyc_mux::release_update::forward_installed("pmuxd")?;

    let cli = Cli::parse(std::env::args().skip(1))?;
    let home = prismattyc_mux::platform::home_dir().map(PathBuf::from);
    let first_pane_dir = prismattyc_mux::platform::first_pane_dir(
        std::env::current_dir().context("current directory")?,
        &std::env::current_exe().context("current executable")?,
        home.clone(),
    );
    // Each pane's console host starts in the daemon's folder and keeps it in
    // use while it runs, so the daemon never stays in the folder it was
    // started from (often the install folder).
    #[cfg(windows)]
    if let Some(home) = &home {
        if let Err(error) = std::env::set_current_dir(home) {
            eprintln!("pmuxd: staying in the start folder: {error}");
        }
    }
    if cli.removed_rich_flag {
        eprintln!("pmuxd: rich mode was removed; --experimental-rich is ignored");
    }
    // A live update hands over the lock, the socket and every running pane.
    #[cfg(unix)]
    let adopted = match cli.handoff_fd {
        Some(fd) => Some(
            prismattyc_mux::handoff::adopt(fd, &cli.socket)
                .context("take over from the previous pmux server")?,
        ),
        None => None,
    };
    #[cfg(not(unix))]
    let adopted: Option<std::convert::Infallible> = {
        if cli.handoff_fd.is_some() {
            bail!("live updates are not available on this platform");
        }
        None
    };
    #[cfg(unix)]
    let (instance, adopted) = match adopted {
        Some(adopted) => {
            eprintln!(
                "pmuxd: took over the running panes from pmuxd {}",
                adopted.from_version
            );
            (
                adopted.lock,
                Some((adopted.plane, adopted.listener, adopted.resumes)),
            )
        }
        None => (
            prismattyc_mux::daemon_lock::DaemonLock::acquire(&cli.socket)
                .context("acquire pmuxd instance lock")?,
            None,
        ),
    };
    #[cfg(not(unix))]
    let (instance, adopted) = (
        prismattyc_mux::daemon_lock::DaemonLock::acquire(&cli.socket)
            .context("acquire pmuxd instance lock")?,
        adopted.map(|never| match never {}),
    );
    prismattyc_mux::mailbox::agents::repair_permissions()
        .context("repair session agent file permissions")?;
    let stopped_path = cli.socket.with_extension("stopped");
    if stopped_path.exists() {
        std::fs::remove_file(&stopped_path)?;
    }
    let login_enabled = prismattyc_mux::login::initialize_default(&cli.socket)?;
    let restore = (cli.restore_workspace || login_enabled) && adopted.is_none();
    let saved = if restore {
        match prismattyc_mux::workspace::load(&cli.socket)? {
            Some(saved) => Some(saved),
            None => prismattyc_mux::workspace::Workspace::migrate_saved_spaces(&cli.socket)?,
        }
    } else {
        None
    };
    // A fresh daemon starts with one session; the default Space adopts it.
    let adopt = (saved.is_none() && adopted.is_none()).then_some(BOOTSTRAP_SESSION);
    #[cfg(unix)]
    let (mut plane, deferred_resumes, listener) = match (adopted, saved.as_ref()) {
        (Some((plane, listener, resumes)), _) => (plane, resumes, Some(listener)),
        (None, Some(saved)) => {
            let (plane, resumes) = saved.restore_plane(&cli.socket)?;
            (plane, resumes, None)
        }
        (None, None) => (bootstrap_plane(&cli, first_pane_dir)?, Vec::new(), None),
    };
    #[cfg(not(unix))]
    let (mut plane, deferred_resumes) = match saved.as_ref() {
        Some(saved) => saved.restore_plane(&cli.socket)?,
        None => (bootstrap_plane(&cli, first_pane_dir)?, Vec::new()),
    };
    if let Some(saved) = &saved {
        saved.checkpoint(&cli.socket)?;
    }
    if let Some(view) = saved.as_ref().and_then(|saved| saved.view.as_ref()) {
        let path = prismattyc_mux::attach_tabs::layout_path_from_socket(&cli.socket);
        if !path.exists() {
            prismattyc_mux::attach_tabs::save(&path, view)?;
        }
    }
    // An unreadable config keeps every [mux] default, as before. The
    // fingerprint comes first so an edit during startup still reaches the
    // activity watcher.
    let config_path = prism_config_path();
    let config_seen = prismattyc_mux::activity_config::Fingerprint::of(&config_path);
    let mux_file = load_mux_section(&config_path).ok();
    if let Some(file) = mux_file.as_ref() {
        prismattyc_mux::activity_webhook::WebhookConfig::from_mux(file)
            .context("invalid activity webhook configuration")?;
    }
    let remote_size = mux_file
        .as_ref()
        .and_then(|file| resolve_remote_size(None, file).ok())
        .unwrap_or_default();
    plane.set_remote_size(remote_size);
    // The mailbox already persists independently of the daemon workspace.
    let mail_db = prismattyc_mux::mailbox::default_mail_db_path();
    let store = prismattyc_mux::mailbox::Store::open(&mail_db)
        .with_context(|| format!("open mailbox {}", mail_db.display()))?;
    plane.set_mail_store(store);
    if let Some(path) = prismattyc_mux::resolve_pane_log_path(&cli.socket) {
        plane
            .set_pane_log_path(path)
            .context("start pane-log checkpoint worker")?;
    }
    if activity_enabled(&cli, mux_file.as_ref()) {
        plane.enable_activity()?;
    }
    #[cfg(unix)]
    let server = match listener {
        Some(listener) => ControlServer::bind_inherited(cli.socket.clone(), listener, plane),
        None => ControlServer::bind(&cli.socket, plane),
    }
    .with_context(|| format!("bind {}", cli.socket.display()))?;
    #[cfg(not(unix))]
    let server = ControlServer::bind(&cli.socket, plane)
        .with_context(|| format!("bind {}", cli.socket.display()))?;
    let identity_path = prismattyc_mux::spaces_daemon_identity_path(&cli.socket);
    if !identity_path.exists() {
        let identity = std::env::var("PRISMATTYC_DAEMON_IDENTITY").unwrap_or_else(|_| {
            if default_socket_path("default").ok().as_ref() == Some(&cli.socket) {
                "default".into()
            } else {
                prismattyc_mux::spaces_socket_identity(&cli.socket)
            }
        });
        prismattyc_mux::private_fs::write(identity_path, format!("{identity}\n"))
            .context("write Spaces daemon identity")?;
    }
    // Clients wait for the published pid, so the default Space exists
    // before any of them can open one.
    ensure_default_space(&cli.socket, adopt);
    instance.publish_pid(&cli.socket)?;
    // Send workspace resume commands only after the control socket is bound and
    // ready for clients such as `pmux-activity-monitor` or agent CLI tools.
    prismattyc_mux::workspace::Workspace::resume_panes(&server.plane(), &deferred_resumes)?;
    // Off the startup path; a failure is logged and the daemon keeps running.
    server.seed_default_plugins();
    let checkpoint =
        prismattyc_mux::workspace::Checkpointer::start(server.plane(), cli.socket.clone())?;
    let space_autosave =
        prismattyc_mux::space_autosave::Worker::start(server.plane(), cli.socket.clone())?;
    // [mux] experimental_activity applies live: no restart, sessions untouched.
    let activity_config = prismattyc_mux::activity_config::Watcher::start(
        server.plane(),
        config_path.clone(),
        cli.experimental_activity,
        config_seen,
        prismattyc_mux::activity_config::POLL,
    )?;
    let activity_webhook =
        prismattyc_mux::activity_webhook::Worker::start(cli.socket.clone(), config_path)?;
    // The ready line for whoever started this server. After a live update the
    // starter may have stopped reading, so a closed stdout is not an error.
    {
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), "{}", server.path().display());
    }
    server.wait_shutdown();
    drop(activity_webhook);
    let handing_over = server.handoff_pending();
    if !handing_over {
        prismattyc_mux::private_fs::write(&stopped_path, b"intentional shutdown\n")?;
    }
    drop(activity_config);
    drop(space_autosave);
    drop(checkpoint);
    #[cfg(unix)]
    if handing_over {
        if let Some((prepared, listener)) = server.finish_for_handoff() {
            let error = prismattyc_mux::handoff::exec(
                prepared,
                listener,
                &instance,
                &cli.socket,
                std::env::args_os().skip(1),
            );
            // Nothing can go back to this server now. Exiting hangs up the
            // panes; the saved workspace restores them as after a restart.
            eprintln!("pmuxd: the live update could not start the new server: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    drop(server);
    Ok(())
}

/// Create the default Space when this start finds none. A failure is
/// logged and never stops the daemon.
fn ensure_default_space(socket: &std::path::Path, adopt: Option<&str>) {
    use prismattyc_mux::default_space::{ensure, Outcome};
    let exe = std::env::current_exe().unwrap_or_default();
    let Some(pmux) = prismattyc_mux::executable::sibling(&exe, &["pmux"]) else {
        eprintln!("pmuxd: default Space skipped: pmux not found next to pmuxd");
        return;
    };
    let dir = prismattyc_mux::spaces_dir_for_socket(socket);
    match ensure(&pmux, socket, &dir, adopt) {
        Ok(Outcome::Created) => eprintln!("pmuxd: created the default Space"),
        Ok(Outcome::Exists | Outcome::Disabled) => {}
        Err(err) => eprintln!("pmuxd: default Space not created: {err:#}"),
    }
}

/// Name of the session a daemon starts with when nothing is restored.
const BOOTSTRAP_SESSION: &str = "default";

/// The feed starts on unless `[mux] experimental_activity = false`; the flag
/// overrides that. A missing config file is the default, on. After start, the
/// activity config watcher applies edits live.
fn activity_enabled(cli: &Cli, file: Option<&prismattyc_mux::MuxSection>) -> bool {
    cli.experimental_activity
        || file.is_some_and(|file| {
            file.experimental_activity()
                || (file.activity_webhook_url.is_some()
                    && file.activity_webhook_secret_file.is_some())
        })
}

fn bootstrap_plane(cli: &Cli, first_pane_dir: PathBuf) -> Result<ControlPlane> {
    let domain = Domain::bootstrap(BOOTSTRAP_SESSION)?;
    let session = domain
        .sessions()
        .next()
        .context("bootstrap session missing")?;
    let window_id = *session
        .windows
        .first()
        .context("bootstrap window missing")?;
    let pane_id = domain
        .window(window_id)
        .and_then(|window| window.layout.panes().first().copied())
        .context("bootstrap pane missing")?;
    let bounds = WindowBounds {
        window_id: window_id.get(),
        cols: cli.cols,
        rows: cli.rows,
    };
    // Fold ambient HIVE_CELL* into the bootstrap pane SpawnSpec for headless dogfood.
    // HIVE_SOCKET arms the supervisor sink only and is never injected into the child
    // (ADR-0017 req 4; stripped in PtySession::spawn_config).
    let spawn = SpawnSpec {
        program: cli.program.clone(),
        argv: cli.argv.clone(),
        cwd: Some(first_pane_dir),
        env: fold_hive_seat_env(),
    };
    let plane = ControlPlane::new_live(
        domain,
        [bounds],
        None,
        [(pane_id.get(), spawn)],
        Some(cli.socket.clone()),
    )
    .map_err(anyhow::Error::new)?;
    Ok(plane)
}

#[cfg(test)]
mod tests {
    use super::Cli;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse(args.iter().map(|arg| (*arg).to_owned())).expect("parse pmuxd arguments")
    }

    #[test]
    fn the_removed_rich_flag_is_still_accepted_and_not_taken_as_the_program() {
        let cli = parse(&[
            "--socket",
            "/tmp/pmuxd-cli-test.sock",
            "--experimental-rich",
            "/bin/sh",
            "-l",
        ]);
        assert!(cli.removed_rich_flag);
        assert_eq!(cli.program, "/bin/sh");
        assert_eq!(cli.argv, ["-l"]);
    }

    #[test]
    fn the_rich_flag_after_the_program_belongs_to_the_program() {
        let cli = parse(&[
            "--socket",
            "/tmp/pmuxd-cli-test.sock",
            "/bin/sh",
            "--experimental-rich",
        ]);
        assert_eq!(cli.program, "/bin/sh");
        assert_eq!(cli.argv, ["--experimental-rich"]);
    }
}
