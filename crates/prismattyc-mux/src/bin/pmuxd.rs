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
    experimental_rich: bool,
    program: String,
    argv: Vec<String>,
}

impl Cli {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut socket = None;
        let mut cols = 80;
        let mut rows = 24;
        let mut experimental_rich = matches!(
            std::env::var("PRISMATTYC_EXPERIMENTAL_RICH")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "on" | "yes"
        );
        let mut program = None;
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
                    experimental_rich = true;
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
            experimental_rich,
            program: program.unwrap_or_else(|| prismattyc_mux::platform::default_shell()),
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
    pmuxd [--socket PATH] [--cols N] [--rows N] [--experimental-rich] [PROGRAM [ARGS...]]
    pmuxd --socket /absolute/path.sock -- /bin/bash -l

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
    // Clean-env one-shot for supervisor.cell_exited (see prismattyc_mux::supervisor).
    let mut argv = std::env::args();
    let _argv0 = argv.next();
    if argv.next().as_deref() == Some(prismattyc_mux::supervisor::INTERNAL_SEND_ARG) {
        return prismattyc_mux::supervisor::run_internal_send().map_err(anyhow::Error::msg);
    }
    prismattyc_mux::release_update::forward_installed("pmuxd")?;

    let cli = Cli::parse(std::env::args().skip(1))?;
    if cli.experimental_rich {
        std::env::set_var("PRISMATTYC_EXPERIMENTAL_RICH", "1");
    }
    let domain = Domain::bootstrap("default")?;
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
        program: cli.program,
        argv: cli.argv,
        cwd: Some(std::env::current_dir().context("current directory")?),
        env: fold_hive_seat_env(),
    };
    let mut plane = ControlPlane::new_live(
        domain,
        [bounds],
        None,
        [(pane_id.get(), spawn)],
        Some(cli.socket.clone()),
    )
    .map_err(anyhow::Error::new)?;
    let remote_size = load_mux_section(&prism_config_path())
        .ok()
        .and_then(|file| resolve_remote_size(None, &file).ok())
        .unwrap_or_default();
    plane.set_remote_size(remote_size);
    // Durable mailbox. Failure to open is fatal: silently serving mail from
    // an in-memory default would lose letters on exit.
    let mail_db = prismattyc_mux::mailbox::default_mail_db_path();
    let store = prismattyc_mux::mailbox::Store::open(&mail_db)
        .with_context(|| format!("open mailbox {}", mail_db.display()))?;
    plane.set_mail_store(store);
    if let Some(path) = prismattyc_mux::resolve_pane_log_path(&cli.socket) {
        plane
            .set_pane_log_path(path)
            .context("start pane-log checkpoint worker")?;
    }
    let server = ControlServer::bind(&cli.socket, plane)
        .with_context(|| format!("bind {}", cli.socket.display()))?;
    println!("{}", server.path().display());

    server.wait_shutdown();
    drop(server);
    Ok(())
}
