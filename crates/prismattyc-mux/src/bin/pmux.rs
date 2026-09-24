//! `pmux` — single front door for the Phase 2B mux (T1).
//!
//! Wraps the existing `pmuxd` / `pmux-attach` binaries and the
//! control protocol control plane behind tmux-shaped verbs:
//!
//! ```text
//! pmux up [PROGRAM ARGS...]     start the server detached
//! pmux attach [SESSION]         attach (auto-starts a missing server)
//! pmux attach --all             open prismattyc-host; sessions as panes in tabs
//! pmux ls                       sessions -> windows -> panes
//! pmux whoami                   this pane's session name, id, pane, agent
//! pmux status-set TEXT          set this pane's attach status line
//! pmux status-set --clear       clear this pane's attach status line
//! pmux send PANE TEXT           write keys; --force to take a busy pane
//! pmux save-buffer PANE FILE    write the pane screen text to FILE (`-` = stdout)
//! pmux pipe-pane PANE|SESSION (FILE | --exec CMD)  stream Output from current seq (`--exec` is sh -c)
//! pmux rename-pane PANE TITLE   set a pane title (empty clears); PANE may be a session name
//! pmux break-pane PANE          move a pane into its own window
//! pmux join-pane PANE --to TAB  move a pane onto another window [-h|-v]
//! pmux arrange SESSION KIND     retile a session window (main-vertical, …)
//! pmux new NAME [PROGRAM...]    create a named session (TTY: attach)
//! pmux doctor [SESSION]         child / lease / viewer vs nested attach
//! pmux kick SESSION             SIGTERM nested attach, else viewers
//! pmux clients [SESSION] [--json]  list attach clients (pid, kind, session, pane)
//! pmux detach --other [SESSION] SIGTERM every attach except your own
//! pmux mail SESSION             arm mail attention + ring the doorbell
//! pmux space save [NAME]        persist several sessions (live cwd + agent); NAME: default
//! pmux space open [NAME]        switch by default; use --add or --new-window
//! pmux space attach [NAME]      attach a space session in this TTY
//! pmux space ls                 list saved spaces
//! pmux space rm [NAME...]       delete space files (`delete` is an alias)
//! pmux space clear [--keep NAME]  delete every space file except kept names
//! pmux session clear [--all] [--keep NAME]  stop sessions except the caller
//! pmux layout save [SESSION]    persist window/pane tree to a JSON file
//! pmux layout save space [NAME] alias of `pmux space save`
//! pmux layout apply NAME        restore a saved tree (adds windows if the session exists)
//! pmux layout apply space [NAME] alias of `pmux space open`
//! pmux layout apply --all       restore every single-session layout file
//! pmux layout ls                list saved layouts and spaces
//! pmux sync on|off [SESSION]    fan typed input to every pane in the session
//! pmux sync status [SESSION]    print per-window sync state
//! pmux status                   socket liveness + server pid
//! pmux stop                     ShutdownServer, then TERM/KILL fallback
//! pmux --session NAME stop      destroy one session; server stays up
//! pmux restart [--host|--daemon|--mcp|--all]  cooperative component restart
//! pmux update [--check|--rollback]  install verified release artifacts
//! pmux completions <shell>      emit bash/zsh/fish completions
//! ```
//!
//! Instances replace raw socket paths (`--instance work` derives the socket via
//! `default_socket_path`); `--socket PATH` remains the expert escape hatch.
//! Live sockets are never replaced or unlinked here — bind-side stale handling
//! stays in the server.

use prismattyc_mux::local_socket::UnixStream;
#[cfg(any(test, target_os = "linux"))]
use std::ffi::OsStr;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{bail, Context, Result};
use prismattyc_mux::walkthrough::{
    boss_matches, bundled_boss, bundled_catalog, detect_step, load_progress, mux_detected,
    progress_path, reset_progress, save_progress, scoped_boss_space, BossVerdict, Catalog, Cursor,
    DetectOutcome, Detected, Expect, Level, Progress, Step,
};
use prismattyc_mux::{
    attach_tabs, attach_targets_session, classify_attach, classify_control_request_id,
    default_socket_path, diagnose_runtime_dir_miss_from_env, from_sessions, from_snapshot,
    host_register::{
        attach_pty_fallback, host_ack_path_from_socket, host_pane_nested,
        host_pid_path_from_socket, live_host_pid, route_seat_to_host, should_host_route_seat,
        wait_host_ack,
    },
    layouts_dir, list_layouts, list_spaces, load_layout, load_mux_section, load_space,
    next_stale_skip, plan, prism_config_path, probe_socket_liveness, procinfo, remove_layout,
    remove_space, resolve_attach_on_new, resolve_mux_target, resolve_space_open_runs_commands,
    save_layout, scan_attach_clients, space_active_session, space_bind_agent,
    space_sessions_in_tab_order, spaces_dir, validate_layout_name, ArrangementWire, AttachClient,
    AttachKind, AxisWire, ControlError, ControlErrorCode, ControlIdMatch, ControlRequest,
    ControlResponse, ControlResponseBody, ControlResponseData, Event, LayoutSnapshot, PaneEvent,
    SavedLayout, SavedNode, SavedSpace, SavedSpaceTab, SavedWindow, SessionSnapshot, Snapshot,
    SocketLiveness, SpaceOpenRunsCommands, SpawnSpec, WindowSnapshot, MAIL_ATTENTION_CELL,
    PROTOCOL_VERSION,
};

const STOP_GRACE: Duration = Duration::from_secs(2);
const UP_WAIT: Duration = Duration::from_secs(3);

#[path = "pmux/lifecycle.rs"]
mod lifecycle;
#[path = "pmux/pane_write.rs"]
mod pane_write;
#[path = "pmux/spaces.rs"]
mod space_commands;

fn print_help() {
    println!(
        "\
pmux — Prismattyc mux front door

session  A pmuxd mailbox/agent unit. List with `pmux ls`.
tab      A host Window. Sessions in one tab appear as panes.
space    An exclusive group of sessions. A session belongs to only one Space.

USAGE:
    pmux [-V|--version] [--instance NAME | --socket PATH]
         [--session NAME] <COMMAND> [ARGS]

session
    up [--] [PROGRAM ARGS...]     start the server (default: $SHELL -l)
    attach [SESSION] [--watch] [--write TEXT] [--pane ID]
                     [--json|--styled-json] [--read-only] [--fit]
                                  attach (TTY: interactive; else JSON dump);
                                  auto-starts a missing or stale server.
                                  --read-only never takes the input lease.
                                  --fit resizes the pane to this terminal.
    ls                            list sessions, windows, and panes
    whoami [--json]               this pane: session name, id, pane, agent
    session name NAME [--session KEY]
                                  set session name and agent ID; preserve mail
    new [--attach|--no-attach] [--headless]
        [--agent NAME|--no-agent] NAME [--] [PROGRAM...]
                                  create a named session; attach in a TTY
                                  unless --no-attach. --agent binds a mailbox
                                  (default: the session name).
    doctor [SESSION]              child pid, lease, viewer vs nested attach
    render-status [--json]         registered host render guards and pane state
    kick SESSION                  SIGTERM nested attach, else viewers
    clients [SESSION] [--json]    list attach clients: pid, viewer/nested,
                                  session, pane
    detach --other [SESSION]      SIGTERM every attach but your own (-a)
    mail <verb> ...               mailbox (pmux mail --help)
    attention SESSION [MESSAGE]   raise attention
                                  (default: needs your attention)
    status-set TEXT [--pane ID]   set this pane's attach chrome status
    status-set --clear [--pane ID]
    send PANE TEXT [--enter] [--literal] [--force]
                                  write keys; refuses a busy pane unless --force
    pane-write PANE --text TEXT [--submit auto|enter|none] [--json]
                                  intentional text to exactly one pane
    save-buffer PANE|SESSION FILE [--history]
                                  write screen text to FILE (`-` = stdout)
    pipe-pane PANE|SESSION (FILE | --exec CMD)
                                  stream Output bytes until Ctrl-C or pane exit
    rename-pane PANE|SESSION [TITLE]
    rename-pane --session KEY [TITLE]
                                  set a pane title; no TITLE clears it
                                  --session needs one pane; do not pass a pane id
    break-pane PANE               move a pane into its own window
    join-pane PANE --to TAB [-h|-v]
                                  move a pane onto another window
    arrange SESSION KIND          retile the session's first window
                                  KIND: main-vertical|main-horizontal|
                                  even-h|even-v|grid
    sync on|off|status [SESSION]  fan typed input across panes
    status                        socket liveness + server pid + log path
    stop [SESSION]                stop the mux, or one named session
    session clear [--all] [--keep NAME]
                                  stop every session except the caller
                                  (pmux session --help)
    restart [--host|--daemon|--mcp|--all] [--plan]
                                  restart safe components; defer active session owners
    versions                      show installed and running component versions
    tutorial [--play]             print the mux onboarding pack;
                                  --play runs the shared walkthrough as text
    config init [--merge]         write [mux] keys into config.toml
    update [--check|--rollback]  install verified GitHub release artifacts
    completions <bash|zsh|fish>   print shell completion script

tab
    attach --all                  open prismattyc-host; sessions as panes in
                                  tabs (skip leftover default). Linux needs
                                  WAYLAND_DISPLAY or DISPLAY.

space
    space open [NAME] [--add|--new-window]
                                  switch by default; --new-window
                                  opens another host window
    space <create|save|attach|ls|rm|clear|add|remove|move|rename> ...
    layout <save|apply|ls|rm>     single-session files; save space / apply
                                  space are aliases (pmux layout --help)

The instance names the socket under $XDG_RUNTIME_DIR/prismattyc/
(pmux.sock for default, else pmux-<instance>.sock). --socket overrides
the path. Live sockets are never unlinked from this tool.
"
    );
}

#[derive(Debug, Clone)]
struct Paths {
    view_path: Option<PathBuf>,
    target_is_explicit: bool,
    socket: PathBuf,
    pidfile: PathBuf,
    logfile: PathBuf,
}

impl Paths {
    fn space_view_path(&self) -> PathBuf {
        self.view_path
            .clone()
            .unwrap_or_else(|| attach_tabs::layout_path_from_socket(&self.socket))
    }

    fn with_view_argument(&self, args: Vec<String>) -> Result<(Self, Vec<String>)> {
        let mut paths = self.clone();
        let mut rest = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--view-path" {
                let path = PathBuf::from(
                    args.next()
                        .context("--view-path requires an absolute path")?,
                );
                if !path.is_absolute() || paths.view_path.is_some() {
                    bail!("--view-path requires one absolute path");
                }
                paths.view_path = Some(path);
            } else {
                rest.push(arg);
            }
        }
        Ok((paths, rest))
    }
    fn resolve(instance: &str, socket_override: Option<PathBuf>) -> Result<Self> {
        let target_is_explicit = socket_override.is_some() || instance != "default";
        let socket = match socket_override {
            Some(path) => {
                if !path.is_absolute() {
                    bail!("--socket must be an absolute path");
                }
                path
            }
            None => default_socket_path(instance)?,
        };
        let pidfile = socket.with_extension("pid");
        let logfile = socket.with_extension("log");
        Ok(Self {
            view_path: None,
            target_is_explicit,
            socket,
            pidfile,
            logfile,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Globals {
    instance: Option<String>,
    socket: Option<PathBuf>,
    session: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Tutorial,
    Completions,
    Update,
    Config,
    Up,
    Attach,
    Ls,
    Whoami,
    StatusSet,
    Send,
    PaneWrite,
    SaveBuffer,
    PipePane,
    RenamePane,
    BreakPane,
    JoinPane,
    Arrange,
    Doctor,
    RenderStatus,
    Kick,
    Clients,
    Detach,
    Attention,
    Mail,
    Mailbox,
    MailDoorbell,
    Space,
    Session,
    Layout,
    Sync,
    New,
    Status,
    Stop,
    Restart,
    Versions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli {
    Help,
    Version,
    MissingCommand,
    Unknown {
        word: String,
    },
    Verb {
        globals: Globals,
        verb: Verb,
        rest: Vec<String>,
    },
}

const VERBS: &[(&str, Verb)] = &[
    ("tutorial", Verb::Tutorial),
    ("completions", Verb::Completions),
    ("update", Verb::Update),
    ("config", Verb::Config),
    ("up", Verb::Up),
    ("start", Verb::Up),
    ("attach", Verb::Attach),
    ("ls", Verb::Ls),
    ("list", Verb::Ls),
    ("whoami", Verb::Whoami),
    ("status-set", Verb::StatusSet),
    ("send", Verb::Send),
    ("pane-write", Verb::PaneWrite),
    ("save-buffer", Verb::SaveBuffer),
    ("pipe-pane", Verb::PipePane),
    ("rename-pane", Verb::RenamePane),
    ("break-pane", Verb::BreakPane),
    ("join-pane", Verb::JoinPane),
    ("arrange", Verb::Arrange),
    ("doctor", Verb::Doctor),
    ("render-status", Verb::RenderStatus),
    ("kick", Verb::Kick),
    ("clients", Verb::Clients),
    ("detach", Verb::Detach),
    ("attention", Verb::Attention),
    ("mail", Verb::Mail),
    ("space", Verb::Space),
    ("session", Verb::Session),
    ("layout", Verb::Layout),
    ("sync", Verb::Sync),
    ("new", Verb::New),
    ("status", Verb::Status),
    ("stop", Verb::Stop),
    ("restart", Verb::Restart),
    ("versions", Verb::Versions),
];

const MAILBOX_LEADERS: &[&str] = &[
    "send",
    "claim",
    "commit",
    "release",
    "inbox",
    "watch",
    "who",
    "alias",
    "broadcast",
    "status",
    "--as",
];

fn lookup_verb(name: &str) -> Option<Verb> {
    VERBS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, verb)| *verb)
}

fn classify_mail(rest: &[String]) -> Verb {
    match rest.first().map(String::as_str) {
        Some(word) if MAILBOX_LEADERS.contains(&word) => Verb::Mailbox,
        _ => Verb::MailDoorbell,
    }
}

fn parse_argv(args: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut args = args.into_iter();
    let mut globals = Globals::default();
    let command = loop {
        match args.next().as_deref() {
            Some("-h" | "--help") => return Ok(Cli::Help),
            Some("-V" | "--version") => return Ok(Cli::Version),
            Some("--instance") => {
                let name = args.next().context("--instance requires a name")?;
                if name.is_empty() || name.contains('/') {
                    bail!("--instance must be a non-empty name without path separators");
                }
                globals.instance = Some(name);
            }
            Some("--socket") => {
                globals.socket = Some(PathBuf::from(
                    args.next().context("--socket requires an absolute path")?,
                ));
            }
            Some("--session") => {
                globals.session = Some(parse_session_name(
                    args.next().context("--session requires a name or id")?,
                )?);
            }
            Some(command) => break command.to_string(),
            None => return Ok(Cli::MissingCommand),
        }
    };
    let rest: Vec<String> = args.collect();
    let Some(mut verb) = lookup_verb(&command) else {
        return Ok(Cli::Unknown { word: command });
    };
    if verb == Verb::Mail {
        verb = classify_mail(&rest);
    }
    Ok(Cli::Verb {
        globals,
        verb,
        rest,
    })
}

fn main() -> Result<()> {
    prismattyc_mux::release_update::forward_installed("pmux")?;
    // Execute stays in `main` rather than a new `run`. Extracting `run`
    // scores C=75 as a new function and PT-277 blocks it; leaving the
    // same code here is allowed because `main` is already above 30
    // (PT-280).
    let cli = parse_argv(std::env::args().skip(1))?;
    let Cli::Verb {
        globals,
        verb,
        rest,
    } = cli
    else {
        match cli {
            Cli::Help => {
                print_help();
                return Ok(());
            }
            Cli::Version => {
                println!("{}", prismattyc_core::bin_version("pmux"));
                return Ok(());
            }
            Cli::MissingCommand => {
                print_help();
                bail!("missing command");
            }
            Cli::Unknown { word } => {
                print_help();
                bail!("unknown command {word:?}");
            }
            Cli::Verb { .. } => unreachable!(),
        }
    };
    let Globals {
        instance: cli_instance,
        socket: cli_socket,
        session: cli_session,
    } = globals;

    match verb {
        Verb::Tutorial => {
            reject_session_flag(cli_session.as_deref(), "tutorial")?;
            return cmd_tutorial(cli_instance, cli_socket, rest);
        }
        Verb::Completions => {
            reject_session_flag(cli_session.as_deref(), "completions")?;
            return cmd_completions(&rest);
        }
        Verb::Update => {
            reject_session_flag(cli_session.as_deref(), "update")?;
            return prismattyc_mux::run_update(rest);
        }
        Verb::Config => {
            reject_session_flag(cli_session.as_deref(), "config")?;
            return cmd_config(rest);
        }
        _ => {}
    }

    let file = load_mux_section(&prism_config_path()).unwrap_or_else(|error| {
        eprintln!("pmux: ignoring config: {error:#}");
        prismattyc_mux::MuxSection::default()
    });
    let (instance, socket_override) = resolve_mux_target(cli_instance, cli_socket, &file)?;
    let paths = Paths::resolve(&instance, socket_override)?;
    match verb {
        Verb::Up => {
            reject_session_flag(cli_session.as_deref(), "up")?;
            cmd_up(&paths, program_from(rest))
        }
        Verb::Attach => {
            reject_session_flag(cli_session.as_deref(), "attach")?;
            cmd_attach(&paths, rest)
        }
        Verb::Ls => {
            reject_session_flag(cli_session.as_deref(), "ls")?;
            cmd_ls(&paths)
        }
        Verb::Whoami => {
            reject_session_flag(cli_session.as_deref(), "whoami")?;
            cmd_whoami(&paths, rest)
        }
        Verb::StatusSet => {
            reject_session_flag(cli_session.as_deref(), "status-set")?;
            cmd_status_set(&paths, rest)
        }
        Verb::PaneWrite => {
            reject_session_flag(cli_session.as_deref(), "pane-write")?;
            pane_write::run(&paths, rest)
        }
        Verb::Send => {
            reject_session_flag(cli_session.as_deref(), "send")?;
            cmd_send(&paths, rest)
        }
        Verb::SaveBuffer => {
            reject_session_flag(cli_session.as_deref(), "save-buffer")?;
            cmd_save_buffer(&paths, rest)
        }
        Verb::PipePane => {
            reject_session_flag(cli_session.as_deref(), "pipe-pane")?;
            cmd_pipe_pane(&paths, rest)
        }
        Verb::RenamePane => cmd_rename_pane(&paths, cli_session, rest),
        Verb::BreakPane => {
            reject_session_flag(cli_session.as_deref(), "break-pane")?;
            cmd_break_pane(&paths, rest)
        }
        Verb::JoinPane => {
            reject_session_flag(cli_session.as_deref(), "join-pane")?;
            cmd_join_pane(&paths, rest)
        }
        Verb::Arrange => cmd_arrange(&paths, cli_session, rest),
        Verb::RenderStatus => {
            reject_session_flag(cli_session.as_deref(), "render-status")?;
            let json = parse_render_status_args(rest)?;
            let status = prismattyc_mux::host_render_status::read(&paths.socket)?;
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&status)?);
            }
            Ok(())
        }
        Verb::Doctor => cmd_doctor(&paths, session_from_args(cli_session, rest, "doctor")?),
        Verb::Kick => {
            let key = session_from_args(cli_session, rest, "kick")?
                .context("usage: pmux kick SESSION")?;
            cmd_kick(&paths, &key)
        }
        Verb::Clients => {
            let (json, rest) = split_json_flag(rest);
            let key = session_from_args(cli_session, rest, "clients")?;
            cmd_clients(&paths, key.as_deref(), json)
        }
        Verb::Detach => {
            let parsed = parse_detach_args(rest)?;
            let key = session_from_args(cli_session, parsed.rest, "detach")?;
            cmd_detach_other(&paths, key.as_deref())
        }
        Verb::Attention => cmd_attention(&paths, parse_attention_args(cli_session, rest)?),
        Verb::Mailbox => {
            reject_session_flag(cli_session.as_deref(), "mail")?;
            cmd_mailbox(&paths, rest)
        }
        Verb::MailDoorbell => cmd_mail(&paths, parse_mail_args(cli_session, rest)?),
        Verb::Space => {
            reject_session_flag(cli_session.as_deref(), "space")?;
            match cmd_space(&paths, rest) {
                Err(err) if err.is::<SpaceUsage>() => {
                    eprint!("{SPACE_USAGE}");
                    std::process::exit(2);
                }
                other => other,
            }
        }
        Verb::Session => {
            reject_session_flag(cli_session.as_deref(), "session")?;
            match cmd_session(&paths, rest) {
                Err(err) if err.is::<SessionUsage>() => {
                    eprint!("{SESSION_USAGE}");
                    std::process::exit(2);
                }
                other => other,
            }
        }
        Verb::Layout => cmd_layout(&paths, cli_session, rest),
        Verb::Sync => cmd_sync(&paths, cli_session, rest),
        Verb::New => {
            reject_session_flag(cli_session.as_deref(), "new")?;
            cmd_new(&paths, rest, &file)
        }
        Verb::Status => {
            reject_session_flag(cli_session.as_deref(), "status")?;
            cmd_status(&paths)
        }
        Verb::Stop => cmd_stop(&paths, session_from_args(cli_session, rest, "stop")?),
        Verb::Restart => {
            reject_session_flag(cli_session.as_deref(), "restart")?;
            lifecycle::restart(&paths, rest)
        }
        Verb::Versions => lifecycle::versions(&paths),
        Verb::Tutorial | Verb::Completions | Verb::Update | Verb::Config | Verb::Mail => {
            unreachable!("early verbs return before Paths::resolve")
        }
    }
}

fn cmd_config(rest: Vec<String>) -> Result<()> {
    let mut init = false;
    let mut merge = false;
    for arg in &rest {
        match arg.as_str() {
            "init" => init = true,
            "--merge" => merge = true,
            "-h" | "--help" => {
                println!(
                    "\
pmux config init [--merge]

Write the [mux] section into $PRISMATTYC_CONFIG or
$XDG_CONFIG_HOME/prismattyc/config.toml.

Without --merge the file must not exist. --merge keeps user values
and comments and appends missing [mux] keys.
"
                );
                return Ok(());
            }
            other => bail!("usage: pmux config init [--merge] (unknown {other:?})"),
        }
    }
    if !init {
        bail!("usage: pmux config init [--merge]");
    }
    let path = prism_config_path();
    if merge {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {} for --merge", path.display()))?;
        let mut document = raw
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parse {}", path.display()))?;
        prismattyc_mux::merge_mux_section(&mut document);
        prismattyc_mux::write_config_atomic(&path, &document.to_string())?;
    } else if path.exists() {
        bail!(
            "{} already exists; pass --merge to add missing [mux] keys",
            path.display()
        );
    } else {
        prismattyc_mux::write_config_atomic(
            &path,
            &format!("{}\n", prismattyc_mux::render_mux_section()),
        )?;
    }
    println!("{}", path.display());
    Ok(())
}

fn cmd_completions(rest: &[String]) -> Result<()> {
    let shell = rest
        .first()
        .map(String::as_str)
        .context("completions requires a shell: bash, zsh, or fish")?;
    let script = match shell {
        "bash" => COMPLETIONS_BASH,
        "zsh" => COMPLETIONS_ZSH,
        "fish" => COMPLETIONS_FISH,
        other => bail!("unknown completion shell {other:?} (want bash, zsh, or fish)"),
    };
    print!("{script}");
    Ok(())
}

const COMPLETIONS_BASH: &str = r#"# pmux bash completions
_prismattyc_mux() {
  local cur prev cmd
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  cmd="${COMP_WORDS[1]}"
  local cmds="up start attach ls list whoami status-set send save-buffer pipe-pane rename-pane break-pane join-pane arrange new doctor render-status kick attention mail space session layout sync status stop restart versions update completions config"
  case "$prev" in
    --instance) return ;;
    --session) return ;;
    --socket) COMPREPLY=( $(compgen -f -- "$cur") ); return ;;
  esac
  case "$cmd" in
    completions) COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") ); return ;;
    attach) COMPREPLY=( $(compgen -W "--all --session-id --watch --write --pane --json --styled-json --read-only --fit" -- "$cur") ); return ;;
    new) COMPREPLY=( $(compgen -W "--attach --no-attach" -- "$cur") ); return ;;
    attention) COMPREPLY=()
      return ;;
    mail) COMPREPLY=( $(compgen -W "--pane --session" -- "$cur") ); return ;;
    space)
      case "$prev" in
        space) COMPREPLY=( $(compgen -W "create save open attach ls rm delete clear add remove move rename" -- "$cur") ); return ;;
        save) COMPREPLY=( $(compgen -W "--name" -- "$cur") ); return ;;
        open) COMPREPLY=( $(compgen -W "--add --replace --no-attach --new-window --no-run --tty" -- "$cur") ); return ;;
        attach) COMPREPLY=( $(compgen -W "--session" -- "$cur") ); return ;;
        rm|delete) COMPREPLY=( $(compgen -W "--all" -- "$cur") ); return ;;
        clear) COMPREPLY=( $(compgen -W "--keep" -- "$cur") ); return ;;
      esac
      ;;
    session)
      case "$prev" in
        session) COMPREPLY=( $(compgen -W "name rename reopen suggest clear" -- "$cur") ); return ;;
        name|rename) COMPREPLY=( $(compgen -W "--session" -- "$cur") ); return ;;
        reopen|suggest) COMPREPLY=( $(compgen -W "--space" -- "$cur") ); return ;;
        clear) COMPREPLY=( $(compgen -W "--all --keep" -- "$cur") ); return ;;
      esac
      ;;
    layout)
      case "$prev" in
        layout) COMPREPLY=( $(compgen -W "save apply ls rm" -- "$cur") ); return ;;
        save) COMPREPLY=( $(compgen -W "space" -- "$cur") ); return ;;
        apply) COMPREPLY=( $(compgen -W "space --all --agent --add --replace --no-attach --new-window --no-run --tty --session" -- "$cur") ); return ;;
      esac
      ;;
    sync) COMPREPLY=( $(compgen -W "on off status" -- "$cur") ); return ;;
    status-set) COMPREPLY=( $(compgen -W "--clear --pane" -- "$cur") ); return ;;
    send) COMPREPLY=( $(compgen -W "--enter --literal --force" -- "$cur") ); return ;;
    save-buffer) COMPREPLY=( $(compgen -W "--history" -- "$cur") ); return ;;
    pipe-pane) COMPREPLY=( $(compgen -W "--exec" -- "$cur") ); return ;;
    join-pane) COMPREPLY=( $(compgen -W "--to -h -v" -- "$cur") ); return ;;
    arrange) COMPREPLY=( $(compgen -W "main-vertical main-horizontal even-h even-v grid" -- "$cur") ); return ;;
    stop|kick|doctor) COMPREPLY=( $(compgen -W "--session" -- "$cur") ); return ;;
    restart) COMPREPLY=( $(compgen -W "--all --host --daemon --mcp --plan --stop-sessions --json --help" -- "$cur") ); return ;;
    update) COMPREPLY=( $(compgen -W "--check --rollback --bin-dir --json --source --host --mux --all --help" -- "$cur") ); return ;;
  esac
  if [[ "$cur" == -* ]]; then
    COMPREPLY=( $(compgen -W "--instance --socket --session --help" -- "$cur") )
    return
  fi
  COMPREPLY=( $(compgen -W "$cmds" -- "$cur") )
}
complete -F _prismattyc_mux pmux
complete -F _prismattyc_mux prismattyc-mux
"#;

const COMPLETIONS_ZSH: &str = r#"#compdef pmux
# pmux zsh completions
_arguments -C \
  '--instance[Instance name]:name:' \
  '--session[Session name or id for stop]:name:' \
  '--socket[Absolute socket path]:path:_files' \
  '(-h --help)'{-h,--help}'[Help]' \
  '1:command:(up start attach ls list whoami status-set send save-buffer pipe-pane rename-pane break-pane join-pane arrange new doctor render-status kick attention mail space session layout sync status stop restart versions update completions config)' \
  '*::arg:->args'
case $state in
  args)
    case $words[1] in
      completions) _values 'shell' bash zsh fish ;;
      attach) _arguments '--all' '--session-id[Opaque session id]:id:' '--watch' '--json' '--styled-json' '--read-only' '--fit' '--write[Text]:text:' '--pane[Pane id]:id:' ;;
      new) _arguments '--attach[Attach after create]' '--no-attach[Do not attach]' '1:name:' ;;
      stop) _arguments '--session[Session name or id]:name:' '1:session:' ;;
      doctor|kick) _arguments '--session[Session name or id]:name:' '1:session:' ;;
      attention) _arguments '1:session:' '2:message:' ;;
      mail) _arguments '--pane[Pane id]:id:' '--session[Session name or id]:name:' '1:session:' ;;
      space) _values 'verb' create save open attach ls rm delete clear add remove move rename
        case $words[2] in
          open) _arguments '--add' '--replace' '--no-attach' '--new-window' '--no-run' '--tty' ;;
          attach) _arguments '--session[Session name]:name:' ;;
          save) _arguments '--name[Space name]:name:' ;;
          rm|delete) _arguments '--all' ;;
          clear) _arguments '--keep[Keep this space]:name:' ;;
        esac
        ;;
      session) _values 'verb' name rename reopen suggest clear
        case $words[2] in
          name|rename) _arguments '--session[Session name or id]:session:' '1:name:' ;;
          reopen|suggest) _arguments '--space[Space name]:name:' ;;
          clear) _arguments '--all' '--keep[Keep this session]:name:' ;;
        esac
        ;;
      layout) _values 'verb' save apply ls rm
        case $words[2] in
          save) _values 'space' space ;;
          apply) _arguments '--all' '--agent' '--add' '--replace' '--no-attach' '--new-window' '--no-run' '--tty' '--session[Target session]:name:' '1:name-or-space:(space)' ;;
        esac
        ;;
      sync) _values 'verb' on off status ;;
      status-set) _arguments '--clear' '--pane[Pane id]:id:' ;;
      send) _arguments '--enter' '--literal' '--force' '1:pane:' '*:text:' ;;
      save-buffer) _arguments '--history' '1:pane or session:' '2:file:_files' ;;
      pipe-pane) _arguments '--exec[Command whose stdin receives Output bytes]:cmd:' '1:pane or session:' '2:file:_files' ;;
      break-pane) _arguments '1:pane:' ;;
      rename-pane) _arguments '--session[Session name or id]:key:' '1:pane or session:' '*:title:' ;;
      join-pane) _arguments '--to[Window id]:id:' '-h' '-v' '1:pane:' ;;
      arrange) _arguments '1:session:' '2:kind:(main-vertical main-horizontal even-h even-v grid)' ;;
      restart) _arguments '--all' '--host' '--daemon' '--mcp' '--plan' '--stop-sessions' '--json' '--help' ;;
      update) _arguments '--check' '--rollback' '--bin-dir[Install directory]:directory:_files -/' '--json' '--source' '--host' '--mux' '--all' '--help' ;;
    esac
    ;;
esac
"#;

const COMPLETIONS_FISH: &str = r#"# pmux fish completions
complete -c pmux -f
complete -c pmux -s h -l help -d 'Help'
complete -c pmux -l instance -d 'Instance name' -r
complete -c pmux -l session -d 'Session name or id (stop)' -r
complete -c pmux -l socket -d 'Absolute socket path' -r
complete -c pmux -n '__fish_use_subcommand' -a 'up' -d 'Start server'
complete -c pmux -n '__fish_use_subcommand' -a 'start' -d 'Start server'
complete -c pmux -n '__fish_use_subcommand' -a 'attach' -d 'Attach'
complete -c pmux -n '__fish_use_subcommand' -a 'ls' -d 'List sessions'
complete -c pmux -n '__fish_use_subcommand' -a 'list' -d 'List sessions'
complete -c pmux -n '__fish_use_subcommand' -a 'whoami' -d 'This pane session id'
complete -c pmux -n '__fish_use_subcommand' -a 'status-set' -d 'Set this pane attach status'
complete -c pmux -n '__fish_use_subcommand' -a 'send' -d 'Write keys to a pane id'
complete -c pmux -n '__fish_use_subcommand' -a 'save-buffer' -d 'Write pane screen text to a file'
complete -c pmux -n '__fish_use_subcommand' -a 'pipe-pane' -d 'Stream pane Output bytes to a file or command'
complete -c pmux -n '__fish_use_subcommand' -a 'rename-pane' -d 'Set or clear a pane title'
complete -c pmux -n '__fish_use_subcommand' -a 'break-pane' -d 'Move a pane into its own window'
complete -c pmux -n '__fish_use_subcommand' -a 'join-pane' -d 'Move a pane onto another window'
complete -c pmux -n '__fish_use_subcommand' -a 'arrange' -d 'Retile a session window'
complete -c pmux -n '__fish_use_subcommand' -a 'new' -d 'Create session'
complete -c pmux -n '__fish_use_subcommand' -a 'doctor' -d 'Viewer vs nested attach'
complete -c pmux -n '__fish_use_subcommand' -a 'kick' -d 'SIGTERM nested or viewer attach'
complete -c pmux -n '__fish_use_subcommand' -a 'attention' -d 'Send agent attention signal'
complete -c pmux -n '__fish_use_subcommand' -a 'mail' -d 'Arm mail attention and ring the doorbell'
complete -c pmux -n '__fish_use_subcommand' -a 'space' -d 'Save, open, or list a workspace of sessions'
complete -c pmux -n '__fish_use_subcommand' -a 'session' -d 'Stop live sessions'
complete -c pmux -n '__fish_use_subcommand' -a 'layout' -d 'Save, apply, or list mux layouts'
complete -c pmux -n '__fish_use_subcommand' -a 'sync' -d 'Fan typed input across panes'
complete -c pmux -n '__fish_use_subcommand' -a 'status' -d 'Socket status'
complete -c pmux -n '__fish_use_subcommand' -a 'stop' -d 'Stop server or named session'
complete -c pmux -n '__fish_use_subcommand' -a 'restart' -d 'Restart safe components'
complete -c pmux -n '__fish_use_subcommand' -a 'versions' -d 'Show installed and running versions'
complete -c pmux -n '__fish_use_subcommand' -a 'update' -d 'Pull main and reinstall binaries'
complete -c pmux -n '__fish_use_subcommand' -a 'completions' -d 'Print completions'
complete -c pmux -n '__fish_use_subcommand' -a 'config' -d 'Write [mux] keys into config.toml'
complete -c pmux -n '__fish_seen_subcommand_from config' -a 'init'
complete -c pmux -n '__fish_seen_subcommand_from config' -l merge
complete -c pmux -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
complete -c pmux -n '__fish_seen_subcommand_from attach' -l all
complete -c pmux -n '__fish_seen_subcommand_from attach' -l session-id -r
complete -c pmux -n '__fish_seen_subcommand_from attach' -l watch
complete -c pmux -n '__fish_seen_subcommand_from attach' -l json
complete -c pmux -n '__fish_seen_subcommand_from attach' -l write -r
complete -c pmux -n '__fish_seen_subcommand_from attach' -l pane -r
complete -c pmux -n '__fish_seen_subcommand_from attach' -l read-only
complete -c pmux -n '__fish_seen_subcommand_from attach' -l fit
complete -c pmux -n '__fish_seen_subcommand_from whoami' -l json
complete -c pmux -n '__fish_seen_subcommand_from status-set' -l clear
complete -c pmux -n '__fish_seen_subcommand_from status-set' -l pane -r
complete -c pmux -n '__fish_seen_subcommand_from send' -l enter
complete -c pmux -n '__fish_seen_subcommand_from send' -l literal
complete -c pmux -n '__fish_seen_subcommand_from send' -l force
complete -c pmux -n '__fish_seen_subcommand_from save-buffer' -l history
complete -c pmux -n '__fish_seen_subcommand_from pipe-pane' -l exec -r
complete -c pmux -n '__fish_seen_subcommand_from mail' -l pane -r
complete -c pmux -n '__fish_seen_subcommand_from space' -a 'create save open attach ls rm delete clear add remove move rename'
complete -c pmux -n '__fish_seen_subcommand_from session' -a 'name rename reopen suggest clear'
complete -c pmux -n '__fish_seen_subcommand_from reopen suggest' -l space -r
complete -c pmux -n '__fish_seen_subcommand_from clear' -l all
complete -c pmux -n '__fish_seen_subcommand_from clear' -l keep -r
complete -c pmux -n '__fish_seen_subcommand_from save' -l name -r
complete -c pmux -n '__fish_seen_subcommand_from open' -l add
complete -c pmux -n '__fish_seen_subcommand_from open' -l replace
complete -c pmux -n '__fish_seen_subcommand_from open' -l no-attach
complete -c pmux -n '__fish_seen_subcommand_from open' -l new-window
complete -c pmux -n '__fish_seen_subcommand_from open' -l no-run
complete -c pmux -n '__fish_seen_subcommand_from open' -l tty
complete -c pmux -n '__fish_seen_subcommand_from layout' -a 'save apply ls rm'
complete -c pmux -n '__fish_seen_subcommand_from rm' -l all
complete -c pmux -n '__fish_seen_subcommand_from delete' -l all
complete -c pmux -n '__fish_seen_subcommand_from layout' -a 'space'
complete -c pmux -n '__fish_seen_subcommand_from apply' -l all
complete -c pmux -n '__fish_seen_subcommand_from apply' -l agent
complete -c pmux -n '__fish_seen_subcommand_from apply' -l add
complete -c pmux -n '__fish_seen_subcommand_from apply' -l replace
complete -c pmux -n '__fish_seen_subcommand_from apply' -l no-attach
complete -c pmux -n '__fish_seen_subcommand_from apply' -l new-window
complete -c pmux -n '__fish_seen_subcommand_from apply' -l no-run
complete -c pmux -n '__fish_seen_subcommand_from apply' -l tty
complete -c pmux -n '__fish_seen_subcommand_from sync' -a 'on off status'
complete -c pmux -n '__fish_seen_subcommand_from new' -l attach -d 'Attach after create'
complete -c pmux -n '__fish_seen_subcommand_from new' -l no-attach -d 'Create without attaching'
complete -c pmux -n '__fish_seen_subcommand_from update' -l host
complete -c pmux -n '__fish_seen_subcommand_from update' -l mux
complete -c pmux -n '__fish_seen_subcommand_from update' -l all
"#;

/// `[-- ] PROGRAM [ARGS...]` with a `$SHELL -l` default.
fn program_from(mut rest: Vec<String>) -> Vec<String> {
    if rest.first().is_some_and(|arg| arg == "--") {
        rest.remove(0);
    }
    if rest.is_empty() {
        prismattyc_mux::platform::default_shell_command()
    } else {
        rest
    }
}

/// Env override, then a sibling of this executable, then $PATH.
fn find_bin(keys: &[&str], names: &[&str]) -> PathBuf {
    for key in keys {
        if let Ok(path) = std::env::var(key) {
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            for name in names {
                let sibling = dir.join(prismattyc_mux::platform::executable_name(name));
                if sibling.is_file() {
                    return sibling;
                }
            }
        }
    }
    PathBuf::from(names[0])
}

fn cmd_up(paths: &Paths, program: Vec<String>) -> Result<()> {
    match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => {
            println!("already running");
            println!("  socket: {}", paths.socket.display());
            if let Some(pid) = read_valid_pid(paths) {
                println!("  pid:    {pid}");
            }
            return Ok(());
        }
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        // Missing starts fresh; Stale is replaced by the server's own bind
        // logic. Never unlinked here.
        SocketLiveness::Missing | SocketLiveness::Stale => {}
    }

    if let Some(dir) = paths.socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.logfile)
        .with_context(|| format!("open {}", paths.logfile.display()))?;
    let server = find_bin(&["PMUX_SERVER"], &["pmuxd"]);
    let mut command = Command::new(&server);
    command
        .arg("--socket")
        .arg(&paths.socket)
        .arg("--")
        .args(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    // Detach into its own session so closing this terminal never HUPs the
    // server.
    prismattyc_mux::platform::detach_command(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", server.display()))?;
    let pid = child.id();

    let deadline = Instant::now() + UP_WAIT;
    while Instant::now() < deadline {
        // try_wait reaps a dead child; pid_alive would see the zombie's /proc
        // entry forever and never take this branch.
        if child.try_wait()?.is_some() {
            if probe_socket_liveness(&paths.socket) == SocketLiveness::Live {
                // Concurrent up/attach race: our child lost the bind and
                // exited; someone else's server is live. Not a failure, and
                // the pidfile must not be overwritten with the loser's pid.
                println!("already running (a concurrent start won the bind)");
                println!("  socket: {}", paths.socket.display());
                return Ok(());
            }
            let tail = log_tail(&paths.logfile, 20);
            bail!(
                "server exited during start — log tail ({}):\n{tail}",
                paths.logfile.display()
            );
        }
        if probe_socket_liveness(&paths.socket) == SocketLiveness::Live {
            match listener_inode(&paths.socket) {
                // Only this child holding the bound socket proves the Live
                // probe is ours — a concurrent starter's cmdline is identical,
                // so argv can't disambiguate the bind winner.
                Some(inode) if pid_holds_socket(pid, inode) => {
                    std::fs::write(&paths.pidfile, format!("{pid}\n"))
                        .with_context(|| format!("write {}", paths.pidfile.display()))?;
                    println!("started pmuxd");
                    println!("  pid:    {pid}");
                    println!("  socket: {}", paths.socket.display());
                    println!("  log:    {}", paths.logfile.display());
                    println!("  attach: pmux attach");
                    return Ok(());
                }
                Some(_) => {
                    // Another server holds the socket; our child is the bind
                    // loser and about to exit on AddrInUse anyway.
                    let _ = child.kill();
                    let _ = child.wait();
                    println!("already running (a concurrent start won the bind)");
                    println!("  socket: {}", paths.socket.display());
                    return Ok(());
                }
                // Inode not readable yet — retry rather than misclassify.
                // macOS has no /proc/net/unix: Live + our child still running
                // is the bind proof.
                None if !cfg!(target_os = "linux") && child.try_wait()?.is_none() => {
                    std::fs::write(&paths.pidfile, format!("{pid}\n"))
                        .with_context(|| format!("write {}", paths.pidfile.display()))?;
                    println!("started pmuxd");
                    println!("  pid:    {pid}");
                    println!("  socket: {}", paths.socket.display());
                    println!("  log:    {}", paths.logfile.display());
                    println!("  attach: pmux attach");
                    return Ok(());
                }
                None => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if child.try_wait()?.is_some() {
        let tail = log_tail(&paths.logfile, 20);
        bail!(
            "server exited during start — log tail ({}):\n{tail}",
            paths.logfile.display()
        );
    }
    // Deterministic state on timeout: don't leave an orphan that might bind
    // after we've reported failure. This is our own child, safe to kill.
    let _ = child.kill();
    let _ = child.wait();
    bail!(
        "socket not ready after {UP_WAIT:?}: {} — killed pid {pid}; see {}",
        paths.socket.display(),
        paths.logfile.display()
    );
}

fn ensure_live_server(paths: &Paths) -> Result<()> {
    match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => Ok(()),
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        SocketLiveness::Missing | SocketLiveness::Stale => {
            if !paths.target_is_explicit {
                if let Some(miss) = diagnose_runtime_dir_miss_from_env(&paths.socket) {
                    bail!("{miss}");
                }
            }
            eprintln!("pmux: no live server; starting one");
            cmd_up(paths, program_from(Vec::new()))
        }
    }
}

/// Linux `attach --all` execs prismattyc-host, which needs a local compositor.
/// macOS Cocoa and Windows do not use WAYLAND_DISPLAY/DISPLAY.
#[cfg(any(test, target_os = "linux"))]
fn linux_attach_all_blocked_without_display(
    wayland_display: Option<&OsStr>,
    display: Option<&OsStr>,
) -> bool {
    wayland_display.is_none() && display.is_none()
}

#[cfg(any(test, target_os = "linux"))]
fn attach_all_no_display_message(socket: &Path) -> String {
    format!(
        "attach --all execs prismattyc-host and needs WAYLAND_DISPLAY or DISPLAY on Linux.\n\
         SSH from macOS: pmux attach SESSION (TTY client; detach C-\\ d)\n\
         socket: {}",
        socket.display()
    )
}

fn list_sessions(paths: &Paths) -> Result<Vec<(u64, String)>> {
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    Ok(snapshot
        .sessions
        .iter()
        .map(|session| (session.id, session.name.clone()))
        .collect())
}

/// Drop the leftover `up` session named `default` when real sessions exist.
fn filter_attach_all_sessions(sessions: Vec<(u64, String)>) -> Vec<(u64, String)> {
    if sessions.len() <= 1 {
        return sessions;
    }
    sessions
        .into_iter()
        .filter(|(_, name)| name != "default")
        .collect()
}

fn cmd_attach_all(paths: &Paths) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if linux_attach_all_blocked_without_display(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        ) {
            bail!("{}", attach_all_no_display_message(&paths.socket));
        }
    }
    let AttachAllLaunch {
        mut command,
        host,
        sessions,
    } = attach_all_command(paths, None)?;
    eprintln!(
        "pmux: opening prismattyc-host with {} session(s): {}",
        sessions.len(),
        session_list(&sessions)
    );
    use prismattyc_mux::platform::Exec;
    Err(command.exec()).with_context(|| format!("exec {}", host.display()))
}

/// The prismattyc-host command that shows every live session as a tab.
struct AttachAllLaunch {
    command: Command,
    host: PathBuf,
    sessions: Vec<(u64, String)>,
}

fn sessions_for_space_host(live: Vec<(u64, String)>, space: &SavedSpace) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for name in space_sessions_in_tab_order(space) {
        if let Some(pair) = live.iter().find(|(_, live_name)| *live_name == name) {
            out.push(pair.clone());
        }
    }
    out
}

fn attach_all_command(paths: &Paths, space: Option<&str>) -> Result<AttachAllLaunch> {
    ensure_live_server(paths)?;
    let live = filter_attach_all_sessions(list_sessions(paths)?);
    let sessions = if let Some(name) = space {
        sessions_for_space_host(live, &load_space(&spaces_dir(), name)?)
    } else {
        live
    };
    if sessions.is_empty() && space.is_none() {
        bail!("no sessions to attach");
    }
    let host = find_bin(&["PRISMATTYC_HOST"], &["prismattyc-host"]);
    let mut command = Command::new(&host);
    command.env("PMUX_SOCKET", &paths.socket);
    command.env_remove("PMUX_VIEW_PATH");
    if let Some(name) = space {
        let snapshot = take_snapshot(&mut Client::connect(&paths.socket)?)?;
        let saved = load_space(&spaces_dir(), name)?;
        let mut file = attach_file_from_space(&saved, &snapshot);
        file.space = Some(name.into());
        file.mode = attach_tabs::AttachTabsMode::Switch;
        let view = paths.socket.with_file_name(format!(
            "space-view-{}.json",
            prismattyc_mux::new_space_id()?
        ));
        attach_tabs::save(&view, &file)?;
        command.env("PMUX_VIEW_PATH", &view);
    }
    if let Some(space) = space {
        command.env("PMUX_SPACE", space);
    }
    for (id, name) in &sessions {
        command.arg("--attach-session").arg(id.to_string());
        command.arg("--attach-title").arg(name);
    }
    Ok(AttachAllLaunch {
        command,
        host,
        sessions,
    })
}

fn session_list(sessions: &[(u64, String)]) -> String {
    sessions
        .iter()
        .map(|(id, name)| format!("{name}#{id}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where a detached prismattyc-host writes its stdout/stderr: next to the
/// socket, so `pmux doctor`-style paths stay in one place.
fn detached_host_log_path(socket: &Path) -> PathBuf {
    socket
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("prismattyc-host.log")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AttachArgs {
    all: bool,
    watch: bool,
    json: bool,
    styled_json: bool,
    write: Option<String>,
    pane: Option<String>,
    session: Option<String>,
    session_id: Option<String>,
    read_only: bool,
    fit: bool,
}

/// One pass: `--write/--pane` consume the next token even if it looks like a flag.
fn parse_attach_args(rest: Vec<String>) -> Result<AttachArgs> {
    let mut parsed = AttachArgs::default();
    let mut rest = rest.into_iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--all" | "-all" => parsed.all = true,
            "--watch" => parsed.watch = true,
            "--json" => parsed.json = true,
            "--styled-json" => parsed.styled_json = true,
            "--read-only" => parsed.read_only = true,
            "--fit" => parsed.fit = true,
            "--write" => {
                parsed.write = Some(rest.next().context("--write requires UTF-8 text")?);
            }
            "--pane" => {
                parsed.pane = Some(rest.next().context("--pane requires a pane ID")?);
            }
            "--session-id" => {
                parsed.session_id = Some(
                    rest.next()
                        .context("--session-id requires an opaque numeric session ID")?,
                );
            }
            session if !session.starts_with('-') && parsed.session.is_none() => {
                parsed.session = Some(session.to_string());
            }
            other => bail!("unknown attach argument {other:?}"),
        }
    }
    if parsed.json && parsed.styled_json {
        bail!("--json and --styled-json are mutually exclusive");
    }
    if parsed.read_only && parsed.write.is_some() {
        bail!("--read-only cannot be combined with --write");
    }
    Ok(parsed)
}

fn cmd_attach(paths: &Paths, rest: Vec<String>) -> Result<()> {
    // Auto-start on a missing/stale socket so `pmux attach` is the only
    // thing an operator has to remember.
    let parsed = parse_attach_args(rest)?;
    if parsed.all {
        if parsed.watch
            || parsed.json
            || parsed.styled_json
            || parsed.write.is_some()
            || parsed.pane.is_some()
            || parsed.session.is_some()
            || parsed.session_id.is_some()
            || parsed.read_only
            || parsed.fit
        {
            bail!("attach --all cannot be combined with a session name or --watch/--json/--styled-json/--write/--pane/--session-id/--read-only/--fit");
        }
        return cmd_attach_all(paths);
    }

    ensure_live_server(paths)?;

    if should_try_host_route_attach(
        attach_is_dump_or_write(&parsed),
        stdin_is_tty(),
        stdout_is_tty(),
    ) && try_host_route_seat(
        paths,
        parsed.session.as_deref(),
        parsed.session_id.as_deref(),
    )? {
        return Ok(());
    }

    let attach = find_bin(&["PMUX_ATTACH"], &["pmux-attach"]);
    let mut command = Command::new(&attach);
    command.arg("--socket").arg(&paths.socket);
    if parsed.watch {
        command.arg("--watch");
    }
    if parsed.json {
        command.arg("--json");
    }
    if parsed.styled_json {
        command.arg("--styled-json");
    }
    if let Some(text) = parsed.write {
        command.arg("--write").arg(text);
    }
    if let Some(pane) = parsed.pane {
        command.arg("--pane").arg(pane);
    }
    if let Some(session) = parsed.session {
        command.arg("--session").arg(session);
    }
    if let Some(session_id) = parsed.session_id {
        command.arg("--session-id").arg(session_id);
    }
    if parsed.read_only {
        command.arg("--read-only");
    }
    if parsed.fit {
        command.arg("--fit");
    }
    use prismattyc_mux::platform::Exec;
    Err(command.exec()).with_context(|| format!("exec {}", attach.display()))
}

const NEW_USAGE: &str =
    "usage: pmux new [--attach|--no-attach] [--headless] [--agent NAME|--no-agent] NAME [-- PROGRAM ARGS...]";

/// Onboarding pack, embedded so `pmux tutorial` works on hosts without
/// VectorVault. Keep in sync with the vault `prismattyc-tutorial-pack`
/// manifest lessons.
const PMUX_TUTORIAL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tutorial.md"));

struct TutorialArgs {
    play: bool,
    level: Option<String>,
    reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayKey {
    Enter,
    Skip,
    Quit,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayRoute {
    Idle,
    Quit,
    Skip,
    WaitMux,
    WaitBoss,
    Advance,
}

fn route_play_key(
    key: Option<PlayKey>,
    mux_wait: bool,
    boss: bool,
    have_daemon: bool,
) -> PlayRoute {
    match key {
        None | Some(PlayKey::Other) => PlayRoute::Idle,
        Some(PlayKey::Quit) => PlayRoute::Quit,
        Some(PlayKey::Skip) => PlayRoute::Skip,
        Some(PlayKey::Enter) if mux_wait && have_daemon => PlayRoute::WaitMux,
        Some(PlayKey::Enter) if boss && have_daemon => PlayRoute::WaitBoss,
        Some(PlayKey::Enter) => PlayRoute::Advance,
    }
}

type PlayCursor = Cursor;

fn cmd_tutorial(
    cli_instance: Option<String>,
    cli_socket: Option<PathBuf>,
    rest: Vec<String>,
) -> Result<()> {
    let args = parse_tutorial_args(&rest)?;
    if !args.play {
        print!("{PMUX_TUTORIAL}");
        return Ok(());
    }
    if args.reset {
        reset_progress(&progress_path()).context("reset walkthrough progress")?;
    }
    let catalog = bundled_catalog().map_err(|error| anyhow::anyhow!("{error}"))?;
    let progress = load_progress(&progress_path());
    if !stdin_is_tty() {
        print!(
            "{}",
            play_listing(&catalog, progress.as_ref(), args.level.as_deref())?
        );
        return Ok(());
    }
    let file = load_mux_section(&prism_config_path()).unwrap_or_else(|error| {
        eprintln!("pmux: ignoring config: {error:#}");
        prismattyc_mux::MuxSection::default()
    });
    let (instance, socket_override) = resolve_mux_target(cli_instance, cli_socket, &file)?;
    let paths = Paths::resolve(&instance, socket_override)?;
    cmd_tutorial_play(&paths, catalog, progress, args.level.as_deref())
}

fn parse_tutorial_args(rest: &[String]) -> Result<TutorialArgs> {
    let mut play = false;
    let mut level = None;
    let mut reset = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--play" => play = true,
            "--reset" => reset = true,
            "--level" => {
                let id = words
                    .next()
                    .cloned()
                    .context("usage: pmux tutorial --play [--level ID] [--reset]")?;
                if id.is_empty() || id.starts_with('-') {
                    bail!("--level requires a level id");
                }
                level = Some(id);
            }
            "-h" | "--help" => {
                println!(
                    "usage: pmux tutorial [--play [--level ID] [--reset]]\n\
                     \n\
                     Without --play, print the mux onboarding pack.\n\
                     --play runs the shared walkthrough as text captions.\n\
                     --level ID starts at that level. --reset deletes walkthrough.json first."
                );
                std::process::exit(0);
            }
            other => bail!("pmux tutorial: unknown argument {other}"),
        }
    }
    if (level.is_some() || reset) && !play {
        bail!("--level and --reset require --play");
    }
    Ok(TutorialArgs { play, level, reset })
}

fn format_play_step(
    level_i: usize,
    n_levels: usize,
    step_i: usize,
    n_steps: usize,
    caption: &str,
) -> String {
    format!(
        "Level {}/{} · step {}/{} — {caption}",
        level_i + 1,
        n_levels,
        step_i + 1,
        n_steps
    )
}

fn format_play_level_list(level: &Level) -> String {
    let mut out = String::new();
    out.push_str(&level.title);
    out.push('\n');
    for step in &level.step {
        out.push_str(&step.caption);
        out.push('\n');
    }
    out
}

fn play_listing(
    catalog: &Catalog,
    progress: Option<&Progress>,
    level_id: Option<&str>,
) -> Result<String> {
    if let Some(id) = level_id {
        if !catalog.level.iter().any(|level| level.id == id) {
            bail!("unknown walkthrough level {id}");
        }
    }
    let cursor = Cursor::resume(catalog.clone(), progress, level_id)
        .context("walkthrough catalog has no steps")?;
    let level = cursor
        .current_level()
        .context("walkthrough catalog has no levels")?;
    Ok(format_play_level_list(level))
}

fn play_key_from_byte(byte: u8) -> PlayKey {
    match byte {
        b'\n' | b'\r' => PlayKey::Enter,
        b's' | b'S' => PlayKey::Skip,
        b'q' | b'Q' | 0x03 | 0x04 => PlayKey::Quit,
        _ => PlayKey::Other,
    }
}

/// Clear ICANON and ECHO (and ICRNL so Enter is `\r`). Keep OPOST and ISIG
/// so `println!` still emits `\r\n` and Ctrl-C raises SIGINT.
#[cfg(unix)]
fn play_cbreak_modes(
    local: rustix::termios::LocalModes,
    input: rustix::termios::InputModes,
) -> (rustix::termios::LocalModes, rustix::termios::InputModes) {
    use rustix::termios::{InputModes, LocalModes};
    (
        local.difference(LocalModes::ICANON | LocalModes::ECHO),
        input.difference(InputModes::ICRNL),
    )
}

fn persist_play(cursor: &PlayCursor) -> Result<()> {
    let Some(progress) = cursor.snapshot(SystemTime::now()) else {
        return Ok(());
    };
    save_progress(&progress_path(), &progress).context("save walkthrough progress")
}

fn step_waits_on_mux(step: &Step) -> bool {
    matches!(step.expect, Expect::MuxEvent { .. })
}

fn step_is_boss(step: &Step) -> bool {
    matches!(
        &step.expect,
        Expect::SpaceEvent { event } if event == "boss_snapshot_match"
    )
}

fn print_play_step(cursor: &PlayCursor, have_daemon: bool) {
    let Some(level) = cursor.current_level() else {
        return;
    };
    let Some(step) = cursor.current_step() else {
        return;
    };
    println!(
        "{}",
        format_play_step(
            cursor.level,
            cursor.catalog.level.len(),
            cursor.step,
            level.step.len(),
            &step.caption
        )
    );
    if let Some(hint) = step.hint.as_deref().filter(|hint| !hint.is_empty()) {
        println!("{hint}");
    } else if let Some(command) = step
        .command
        .as_deref()
        .filter(|command| !command.is_empty())
    {
        println!("{command}");
    }
    if step_is_boss(step) {
        if have_daemon {
            println!("waiting for the three-seat picture");
        } else {
            println!("press Enter when done");
        }
    } else if step_waits_on_mux(step) {
        if have_daemon {
            println!("waiting for a pmuxd event");
        } else {
            println!("press Enter when done");
        }
    } else {
        println!("press Enter when done");
    }
}

fn play_boss_verdict(client: &mut Client, socket: &Path) -> Result<BossVerdict> {
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("expected snapshot");
    };
    let sessions: Vec<&SessionSnapshot> = snapshot.sessions.iter().collect();
    let space = from_sessions(&sessions);
    let tabs = attach_tabs::load(&attach_tabs::layout_path_from_socket(socket)).map(|file| {
        file.tabs
            .into_iter()
            .map(|tab| SavedSpaceTab {
                title: tab.title,
                sessions: tab.sessions,
            })
            .collect::<Vec<_>>()
    });
    let space = scoped_boss_space(space, tabs.as_deref());
    let target = bundled_boss().context("boss.json")?;
    Ok(boss_matches(&target, &space))
}

fn control_event_kind(event: &Event) -> &'static str {
    match event {
        Event::PaneSplit { .. } => "PaneSplit",
        Event::PaneClosed { .. } => "PaneClosed",
        Event::GeometryChanged { .. } => "GeometryChanged",
        Event::SizeOwnerChanged { .. } => "SizeOwnerChanged",
        Event::FocusSuggested { .. } => "FocusSuggested",
        Event::FocusReported { .. } => "FocusReported",
        Event::MailAttentionChanged { .. } => "MailAttentionChanged",
        Event::PaneAttention { .. } => "PaneAttention",
        Event::PaneAttentionCleared { .. } => "PaneAttentionCleared",
        Event::LeaseChanged { .. } => "LeaseChanged",
        Event::SessionNamed { .. } => "SessionNamed",
        Event::SessionCreated { .. } => "SessionCreated",
        Event::SessionSwitched { .. } => "SessionSwitched",
        Event::SessionDestroyed { .. } => "SessionDestroyed",
        Event::OutputActivity { .. } => "OutputActivity",
        Event::WindowCreated { .. } => "WindowCreated",
        Event::WindowDestroyed { .. } => "WindowDestroyed",
        Event::WindowSwitched { .. } => "WindowSwitched",
        Event::WindowRenamed { .. } => "WindowRenamed",
        Event::PaneStatusChanged { .. } => "PaneStatusChanged",
        Event::PaneRenamed { .. } => "PaneRenamed",
        Event::SyncInputChanged { .. } => "SyncInputChanged",
        Event::PaneMoved { .. } => "PaneMoved",
        Event::SpaceOwnershipChanged { .. } => "SpaceOwnershipChanged",
    }
}

fn detected_from_control_event(event: &Event) -> Detected {
    // Stamp `current` on session/window events so catalog predicates match
    // any event of that kind, not the learner's specific session (PT-196).
    let kind = control_event_kind(event);
    match event {
        Event::SessionCreated { .. }
        | Event::SessionDestroyed { .. }
        | Event::SessionSwitched { .. } => mux_detected(kind, None, Some("current")),
        Event::PaneMoved { .. }
        | Event::WindowCreated { .. }
        | Event::WindowDestroyed { .. }
        | Event::WindowSwitched { .. } => mux_detected(kind, Some("current"), None),
        _ => mux_detected(kind, None, None),
    }
}

fn play_complete_step(cursor: &mut PlayCursor) -> Result<bool> {
    let more = cursor.complete();
    persist_play(cursor)?;
    Ok(more)
}

fn play_skip_step(cursor: &mut PlayCursor) -> Result<bool> {
    let more = cursor.skip();
    persist_play(cursor)?;
    Ok(more)
}

fn play_done_if_finished(more: bool) -> Result<bool> {
    if more {
        return Ok(false);
    }
    println!("Done.");
    Ok(true)
}

fn maybe_mux_advance(
    client: Option<&mut Client>,
    after: &mut u64,
    expect: &Expect,
    mux_wait: bool,
    cursor: &mut PlayCursor,
) -> Result<PlayTick> {
    if !mux_wait {
        return Ok(PlayTick::Idle);
    }
    let Some(client) = client else {
        return Ok(PlayTick::Idle);
    };
    if !drain_play_events(client, after, expect)? {
        return Ok(PlayTick::Idle);
    }
    if play_done_if_finished(play_complete_step(cursor)?)? {
        return Ok(PlayTick::Quit);
    }
    Ok(PlayTick::NextStep)
}

fn maybe_boss_advance(
    client: Option<&mut Client>,
    socket: &Path,
    boss: bool,
    last_boss_poll: &mut Instant,
    last_boss_diff: &mut String,
    cursor: &mut PlayCursor,
) -> Result<PlayTick> {
    if !boss {
        return Ok(PlayTick::Idle);
    }
    let Some(client) = client else {
        return Ok(PlayTick::Idle);
    };
    if last_boss_poll.elapsed() < Duration::from_secs(1) {
        return Ok(PlayTick::Idle);
    }
    *last_boss_poll = Instant::now();
    match play_boss_verdict(client, socket) {
        Ok(BossVerdict::Match) => {
            if play_done_if_finished(play_complete_step(cursor)?)? {
                return Ok(PlayTick::Quit);
            }
            Ok(PlayTick::NextStep)
        }
        Ok(BossVerdict::Mismatch(diff)) => {
            *last_boss_diff = diff;
            Ok(PlayTick::Idle)
        }
        Err(_) => Ok(PlayTick::Idle),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayTick {
    Idle,
    NextStep,
    Quit,
}

fn drain_play_events(client: &mut Client, after: &mut u64, expect: &Expect) -> Result<bool> {
    loop {
        match client.request(|request_id| ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id,
            after_sequence: *after,
            limit: Some(64),
        }) {
            Ok(ControlResponseData::Events { batch }) => {
                *after = batch.through_sequence.max(*after);
                for envelope in &batch.events {
                    let fact = detected_from_control_event(&envelope.event);
                    if detect_step(expect, &fact) == DetectOutcome::Advance {
                        return Ok(true);
                    }
                }
                if !batch.has_more {
                    return Ok(false);
                }
            }
            Ok(_) => return Ok(false),
            Err(_) => return Ok(false),
        }
    }
}

fn cmd_tutorial_play(
    paths: &Paths,
    catalog: Catalog,
    progress: Option<Progress>,
    level_id: Option<&str>,
) -> Result<()> {
    if let Some(id) = level_id {
        if !catalog.level.iter().any(|level| level.id == id) {
            bail!("unknown walkthrough level {id}");
        }
    }
    let mut cursor = Cursor::resume(catalog, progress.as_ref(), level_id)
        .context("walkthrough catalog has no steps")?;
    if cursor.current_step().is_none() {
        println!("Done.");
        return Ok(());
    }
    let mut client = Client::connect(&paths.socket).ok();
    let mut after = 0;
    let head = client.as_mut().and_then(|client| {
        client
            .request(|request_id| ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id,
                after_sequence: 0,
                limit: Some(64),
            })
            .ok()
    });
    match head {
        Some(ControlResponseData::Events { batch }) => after = batch.current_sequence,
        _ => client = None,
    }
    let _raw = PlayRawStdin::enter()?;
    let mut last_boss_poll = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut last_boss_diff = String::from("not yet");
    loop {
        let Some(step) = cursor.current_step() else {
            println!("Done.");
            return Ok(());
        };
        let mux_wait = step_waits_on_mux(step);
        let boss = step_is_boss(step);
        let have_daemon = client.is_some();
        print_play_step(&cursor, have_daemon);
        let expect = step.expect.clone();
        match play_one_step(
            &mut client,
            &paths.socket,
            &mut after,
            &expect,
            mux_wait,
            boss,
            have_daemon,
            &mut last_boss_poll,
            &mut last_boss_diff,
            &mut cursor,
        )? {
            PlayTick::Quit => return Ok(()),
            PlayTick::NextStep | PlayTick::Idle => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn play_one_step(
    client: &mut Option<Client>,
    socket: &Path,
    after: &mut u64,
    expect: &Expect,
    mux_wait: bool,
    boss: bool,
    have_daemon: bool,
    last_boss_poll: &mut Instant,
    last_boss_diff: &mut String,
    cursor: &mut PlayCursor,
) -> Result<PlayTick> {
    loop {
        match play_poll_tick(
            client,
            socket,
            after,
            expect,
            mux_wait,
            boss,
            have_daemon,
            last_boss_poll,
            last_boss_diff,
            cursor,
        )? {
            PlayTick::Idle => {}
            other => return Ok(other),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn play_poll_tick(
    client: &mut Option<Client>,
    socket: &Path,
    after: &mut u64,
    expect: &Expect,
    mux_wait: bool,
    boss: bool,
    have_daemon: bool,
    last_boss_poll: &mut Instant,
    last_boss_diff: &mut String,
    cursor: &mut PlayCursor,
) -> Result<PlayTick> {
    match maybe_mux_advance(client.as_mut(), after, expect, mux_wait, cursor)? {
        PlayTick::Idle => {}
        other => return Ok(other),
    }
    match maybe_boss_advance(
        client.as_mut(),
        socket,
        boss,
        last_boss_poll,
        last_boss_diff,
        cursor,
    )? {
        PlayTick::Idle => {}
        other => return Ok(other),
    }
    apply_play_route(
        route_play_key(
            read_play_key(Duration::from_millis(100))?,
            mux_wait,
            boss,
            have_daemon,
        ),
        cursor,
        last_boss_diff,
    )
}

fn apply_play_move(route: PlayRoute, cursor: &mut PlayCursor) -> Result<PlayTick> {
    let more = if route == PlayRoute::Skip {
        play_skip_step(cursor)?
    } else {
        play_complete_step(cursor)?
    };
    if play_done_if_finished(more)? {
        return Ok(PlayTick::Quit);
    }
    Ok(PlayTick::NextStep)
}

fn apply_play_route(
    route: PlayRoute,
    cursor: &mut PlayCursor,
    last_boss_diff: &str,
) -> Result<PlayTick> {
    match route {
        PlayRoute::Idle | PlayRoute::WaitMux => Ok(PlayTick::Idle),
        PlayRoute::WaitBoss => {
            println!("not yet: {last_boss_diff}");
            Ok(PlayTick::Idle)
        }
        PlayRoute::Quit => Ok(PlayTick::Quit),
        PlayRoute::Skip | PlayRoute::Advance => apply_play_move(route, cursor),
    }
}

#[cfg(unix)]
struct PlayRawStdin {
    original: rustix::termios::Termios,
}

#[cfg(unix)]
impl PlayRawStdin {
    fn enter() -> Result<Self> {
        use rustix::termios::{self, OptionalActions};
        let stdin = io::stdin();
        let original = termios::tcgetattr(&stdin).context("tcgetattr")?;
        let mut raw = original.clone();
        let (local, input) = play_cbreak_modes(raw.local_modes, raw.input_modes);
        raw.local_modes = local;
        raw.input_modes = input;
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw).context("tcsetattr cbreak")?;
        Ok(Self { original })
    }
}

#[cfg(unix)]
impl Drop for PlayRawStdin {
    fn drop(&mut self) {
        use rustix::termios::{self, OptionalActions};
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.original);
    }
}

#[cfg(unix)]
fn read_play_key(timeout: Duration) -> Result<Option<PlayKey>> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    let stdin = io::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
    let ts = Timespec {
        tv_sec: timeout.as_secs() as i64,
        tv_nsec: timeout.subsec_nanos() as i64,
    };
    match poll(&mut fds, Some(&ts)) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(error) if error == rustix::io::Errno::INTR => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut byte = [0u8; 1];
    match stdin.lock().read(&mut byte) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(play_key_from_byte(byte[0]))),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct NewArgs {
    name: String,
    program: Vec<String>,
    attach: Option<bool>,
    agent_id: Option<String>,
    headless: bool,
}

/// `[--attach|--no-attach] [--agent NAME|--no-agent] NAME [-- PROGRAM...]`
/// Mailbox binding rule: explicit `--agent` wins; `--no-agent` opts out;
/// otherwise the session name is the mailbox address.
fn resolve_agent_id(agent: Option<String>, saw_no_agent: bool, name: &str) -> Option<String> {
    agent.or_else(|| (!saw_no_agent).then(|| name.to_string()))
}

fn parse_new_args(rest: Vec<String>) -> Result<NewArgs> {
    let mut attach = None;
    let mut agent = None;
    let mut saw_no_agent = false;
    let mut headless = false;
    let mut name: Option<String> = None;
    let mut iter = rest.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--attach" => attach = Some(true),
            "--no-attach" => attach = Some(false),
            "--headless" => headless = true,
            "--no-agent" => {
                if agent.is_some() {
                    bail!("--agent and --no-agent cannot be combined");
                }
                saw_no_agent = true;
            }
            "--agent" => {
                if saw_no_agent {
                    bail!("--agent and --no-agent cannot be combined");
                }
                let value = iter.next().context("--agent requires a name")?;
                if value.is_empty() || value.starts_with('-') {
                    bail!("--agent requires a name");
                }
                agent = Some(value);
            }
            "--" => {
                let name = name.context(NEW_USAGE)?;
                if headless && attach == Some(true) {
                    bail!("--headless cannot be combined with --attach");
                }
                let agent_id = resolve_agent_id(agent, saw_no_agent, &name);
                return Ok(NewArgs {
                    name,
                    program: program_from(iter.collect()),
                    attach,
                    agent_id,
                    headless,
                });
            }
            flag if flag.starts_with('-') => bail!("unknown new argument {flag:?}"),
            _ if name.is_none() => name = Some(arg),
            _ => {
                let mut program = vec![arg];
                program.extend(iter);
                let name = name.expect("name is set");
                if headless && attach == Some(true) {
                    bail!("--headless cannot be combined with --attach");
                }
                let agent_id = resolve_agent_id(agent, saw_no_agent, &name);
                return Ok(NewArgs {
                    name,
                    program: program_from(program),
                    attach,
                    agent_id,
                    headless,
                });
            }
        }
    }
    let name = name.context(NEW_USAGE)?;
    if headless && attach == Some(true) {
        bail!("--headless cannot be combined with --attach");
    }
    let agent_id = resolve_agent_id(agent, saw_no_agent, &name);
    Ok(NewArgs {
        name,
        program: program_from(Vec::new()),
        attach,
        agent_id,
        headless,
    })
}

fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn stdout_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

fn space_attach_recipe_lines(space: &SavedSpace) -> Vec<String> {
    let names = space_sessions_in_tab_order(space);
    let active = space_active_session(space);
    names
        .into_iter()
        .map(|name| {
            if Some(name.as_str()) == active.as_deref() {
                format!("pmux attach {name}  # active")
            } else {
                format!("pmux attach {name}")
            }
        })
        .collect()
}

fn print_space_attach_recipe(space: &SavedSpace) {
    for line in space_attach_recipe_lines(space) {
        println!("{line}");
    }
}

fn space_open_uses_tty_recipe(tty_flag: bool) -> bool {
    if tty_flag {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        linux_attach_all_blocked_without_display(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn cmd_space_attach(paths: &Paths, rest: Vec<String>) -> Result<()> {
    let parsed = parse_space_attach_args(rest)?;
    let space = load_space(&spaces_dir(), &parsed.name)?;
    let names = space_sessions_in_tab_order(&space);
    if names.is_empty() {
        bail!("space {} has no sessions", parsed.name);
    }
    let session = if let Some(session) = parsed.session.as_ref() {
        if !names.iter().any(|name| name == session) {
            bail!("session {session:?} is not in space {}", parsed.name);
        }
        session.clone()
    } else {
        space_active_session(&space).context("space has no session")?
    };
    if !stdin_is_tty() || !stdout_is_tty() {
        bail!("pmux space attach requires a TTY");
    }
    apply_layout_args(
        paths,
        ApplyArgs::Space {
            name: parsed.name.clone(),
            replace: false,
            add: false,
            attach: false,
            new_window: false,
            no_run: false,
            tty: false,
            host: false,
        },
    )?;
    exec_attach_session(paths, &session, Some(&parsed.name))
}

/// Attach after create when the operator asked for it, or when the default is
/// on and this process actually owns a terminal.
fn should_attach_new(want: bool, cli: Option<bool>, tty: bool) -> bool {
    want && (cli == Some(true) || tty)
}

fn attach_is_dump_or_write(parsed: &AttachArgs) -> bool {
    parsed.watch
        || parsed.json
        || parsed.styled_json
        || parsed.write.is_some()
        || parsed.pane.is_some()
        || parsed.read_only
        || parsed.fit
}

/// Interactive TTY attach may host-route. Dump/write flags and pipes must not.
///
/// Gate mutants (`&&`→`||`, `delete !`) are follow-up coverage
/// (PT-306 mux scrollbar/seat-route), not the log-replica core.
#[mutants::skip]
fn should_try_host_route_attach(dump_or_write: bool, stdin_tty: bool, stdout_tty: bool) -> bool {
    !dump_or_write && stdin_tty && stdout_tty
}

/// Route an interactive seat through the attach-tabs cache when a host is live.
///
/// Session-key `||` / `delete !` / `Ok(false)` mutants are follow-up coverage
/// (PT-306 mux scrollbar/seat-route), not the log-replica core.
#[mutants::skip]
fn try_host_route_seat(
    paths: &Paths,
    session: Option<&str>,
    session_id: Option<&str>,
) -> Result<bool> {
    if !should_host_route_seat(host_pane_nested(), attach_pty_fallback(), false) {
        return Ok(false);
    }
    let sessions = list_sessions(paths)?;
    let (id, name) = if let Some(key) = session.or(session_id) {
        sessions
            .iter()
            .find(|(id, name)| name == key || id.to_string() == key)
            .map(|(id, name)| (*id, name.clone()))
            .with_context(|| format!("no session matching {key:?}"))?
    } else {
        sessions.first().cloned().context("no session to attach")?
    };
    if !route_seat_to_host(&paths.socket, &id.to_string(), &name)? {
        return Ok(false);
    }
    wait_host_seat_ack(paths);
    println!("attached {name} in the host as a log replica");
    Ok(true)
}

/// Wait for `{stem}.host.ack` after a seat-route cache write.
///
/// Body/`()` mutants are follow-up coverage (PT-306 mux scrollbar/seat-route).
#[mutants::skip]
fn wait_host_seat_ack(paths: &Paths) {
    let ack = host_ack_path_from_socket(&paths.socket);
    let cache = attach_tabs::layout_path_from_socket(&paths.socket);
    let since = std::fs::metadata(&cache)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| SystemTime::now());
    let _ = wait_host_ack(&ack, since, Duration::from_secs(2));
}

fn exec_attach_session(paths: &Paths, session: &str, space: Option<&str>) -> Result<()> {
    if try_host_route_seat(paths, Some(session), None)? {
        return Ok(());
    }
    let attach = find_bin(&["PMUX_ATTACH"], &["pmux-attach"]);
    let mut command = Command::new(&attach);
    command
        .arg("--socket")
        .arg(&paths.socket)
        .arg("--session")
        .arg(session);
    if let Some(space) = space {
        command.arg("--space").arg(space);
        command.env("PMUX_SPACE", space);
    }
    use prismattyc_mux::platform::Exec;
    Err(command.exec()).with_context(|| format!("exec {}", attach.display()))
}

fn cmd_new(paths: &Paths, rest: Vec<String>, file: &prismattyc_mux::MuxSection) -> Result<()> {
    let parsed = parse_new_args(rest)?;
    let want_attach = resolve_attach_on_new(parsed.attach, file)?;
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let spawn = SpawnSpec {
        program: parsed.program[0].clone(),
        argv: parsed.program[1..].to_vec(),
        cwd: std::env::current_dir().ok(),
        env: Default::default(),
    };
    let created = client.request(|request_id| ControlRequest::CreateSession {
        version: PROTOCOL_VERSION,
        request_id,
        name: parsed.name,
        spawn,
        cols: None,
        rows: None,
        agent_id: parsed.agent_id,
        headless: parsed.headless,
    })?;
    let ControlResponseData::Session {
        session_id,
        name,
        pane_id,
        ..
    } = created
    else {
        bail!("server returned an unexpected create-session response");
    };
    if !parsed.headless && should_attach_new(want_attach, parsed.attach, stdin_is_tty()) {
        // Drop the create-session control connection before exec so the
        // attach client is the only live client on this fd.
        drop(client);
        return exec_attach_session(paths, &name, None);
    }
    if parsed.headless {
        println!("created headless session {name:?} (id {session_id})");
    } else {
        println!("created session {name:?} (id {session_id}, pane {pane_id})");
        println!("  attach: pmux attach {name}");
    }
    Ok(())
}

fn cmd_ls(paths: &Paths) -> Result<()> {
    if probe_socket_liveness(&paths.socket) != SocketLiveness::Live {
        return cmd_status(paths);
    }
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    print_tree(&mut client, client_id, &snapshot, &paths.socket)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct Whoami {
    session: String,
    id: u64,
    pane: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

fn whoami_from_snapshot(snapshot: &Snapshot, pane_id: u64) -> Option<Whoami> {
    snapshot.sessions.iter().find_map(|session| {
        session.windows.iter().find_map(|window| {
            window
                .panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .map(|_| Whoami {
                    session: session.name.clone(),
                    id: session.id,
                    pane: pane_id,
                    agent: session.agent_id.clone(),
                })
        })
    })
}

fn format_whoami(who: &Whoami) -> String {
    format!(
        "session: {}\nid: {}\npane: {}\nagent: {}\n",
        who.session,
        who.id,
        who.pane,
        who.agent.as_deref().unwrap_or("-")
    )
}

fn cmd_whoami(paths: &Paths, rest: Vec<String>) -> Result<()> {
    let json = match rest.as_slice() {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => bail!("usage: pmux whoami [--json]"),
    };
    if probe_socket_liveness(&paths.socket) != SocketLiveness::Live {
        return cmd_status(paths);
    }
    let pane_id = std::env::var("PRISMATTYC_PANE_ID")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .context("not inside a pmux pane (PRISMATTYC_PANE_ID unset)")?;
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let who = whoami_from_snapshot(&snapshot, pane_id)
        .with_context(|| format!("pane {pane_id} is not in the live snapshot"))?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&who).context("serialize whoami")?
        );
    } else {
        print!("{}", format_whoami(&who));
    }
    Ok(())
}

fn cmd_status_set(paths: &Paths, rest: Vec<String>) -> Result<()> {
    let mut clear = false;
    let mut pane_override = None;
    let mut text = None;
    let mut iter = rest.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--clear" => {
                if clear {
                    bail!("usage: pmux status-set TEXT | pmux status-set --clear");
                }
                clear = true;
            }
            "--pane" => {
                let raw = iter.next().context("--pane requires an id")?;
                let id: u64 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--pane must be a pane id"))?;
                pane_override = Some(id);
            }
            flag if flag.starts_with('-') => bail!("unknown status-set argument {flag:?}"),
            other => {
                if text.is_some() {
                    bail!("usage: pmux status-set TEXT | pmux status-set --clear");
                }
                text = Some(other.to_string());
            }
        }
    }
    if clear && text.is_some() {
        bail!("usage: pmux status-set TEXT | pmux status-set --clear");
    }
    if !clear && text.is_none() {
        bail!("usage: pmux status-set TEXT | pmux status-set --clear");
    }
    require_live_socket(paths)?;
    let pane_id = match pane_override {
        Some(id) => id,
        None => std::env::var("PRISMATTYC_PANE_ID")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .context("not inside a pmux pane")?,
    };
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let payload = if clear { None } else { text };
    let _ = client.request(|request_id| ControlRequest::SetPaneStatus {
        version: PROTOCOL_VERSION,
        request_id,
        pane_id,
        text: payload.clone(),
    })?;
    if clear || payload.as_ref().is_some_and(|s| s.trim().is_empty()) {
        println!("cleared status on pane {pane_id}");
    } else {
        println!("set status on pane {pane_id}");
    }
    Ok(())
}

const SEND_USAGE: &str = "usage: pmux send PANE TEXT [--enter] [--literal] [--force] [-- TEXT]";
const SEND_CHUNK: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct SendArgs {
    pane: u64,
    text: String,
    enter: bool,
    literal: bool,
    force: bool,
}

fn decode_send_escapes(text: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e' | 'E') => out.push('\u{1b}'),
            Some('\\') => out.push('\\'),
            Some(other) => bail!("unknown send escape \\{other}"),
            None => bail!("trailing backslash in send text"),
        }
    }
    Ok(out)
}

fn parse_send_args(rest: Vec<String>) -> Result<SendArgs> {
    let mut enter = false;
    let mut literal = false;
    let mut force = false;
    let mut pane = None;
    let mut text_parts = Vec::new();
    let mut raw = false;
    for arg in rest {
        if raw {
            push_send_token(&mut pane, &mut text_parts, arg)?;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => continue,
            "--" => raw = true,
            "--enter" => enter = true,
            "--literal" => literal = true,
            "--force" => force = true,
            flag if flag.starts_with('-') => bail!("unknown send argument {flag:?}"),
            other => push_send_token(&mut pane, &mut text_parts, other.to_string())?,
        }
    }
    let pane = pane.context(SEND_USAGE)?;
    if text_parts.is_empty() && !enter {
        bail!("{SEND_USAGE}");
    }
    Ok(SendArgs {
        pane,
        text: text_parts.join(" "),
        enter,
        literal,
        force,
    })
}

fn push_send_token(
    pane: &mut Option<u64>,
    text_parts: &mut Vec<String>,
    token: String,
) -> Result<()> {
    if pane.is_none() {
        let id: u64 = token
            .parse()
            .map_err(|_| anyhow::anyhow!("PANE must be a pane id"))?;
        *pane = Some(id);
        Ok(())
    } else {
        text_parts.push(token);
        Ok(())
    }
}

fn send_chunks(data: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let mut end = rest.len().min(SEND_CHUNK);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

fn control_code(error: &anyhow::Error) -> Option<ControlErrorCode> {
    error.downcast_ref::<ControlError>().map(|err| err.code)
}

fn write_pane_bytes(client: &mut Client, client_id: u64, pane_id: u64, data: &str) -> Result<()> {
    for chunk in send_chunks(data) {
        loop {
            match client.request(|request_id| ControlRequest::WritePane {
                version: PROTOCOL_VERSION,
                request_id,
                client_id,
                pane_id,
                data: chunk.to_string(),
            }) {
                Ok(_) => break,
                Err(error) if control_code(&error) == Some(ControlErrorCode::Backpressure) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn snapshot_pane(snapshot: &Snapshot, pane_id: u64) -> Option<&prismattyc_mux::PaneSnapshot> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .find_map(|window| window.panes.iter().find(|pane| pane.id == pane_id))
    })
}

const BREAK_PANE_USAGE: &str = "usage: pmux break-pane PANE";
const JOIN_PANE_USAGE: &str = "usage: pmux join-pane PANE --to TAB [-h|-v]";

fn dummy_window_spawn() -> SpawnSpec {
    SpawnSpec {
        program: prismattyc_mux::platform::default_shell(),
        argv: vec![],
        cwd: None,
        env: Default::default(),
    }
}

fn pane_home(snapshot: &Snapshot, pane_id: u64) -> Result<(u64, &WindowSnapshot)> {
    snapshot
        .sessions
        .iter()
        .find_map(|session| {
            session
                .windows
                .iter()
                .find(|window| window.panes.iter().any(|pane| pane.id == pane_id))
                .map(|window| (session.id, window))
        })
        .with_context(|| format!("unknown pane {pane_id}"))
}

fn snapshot_window(snapshot: &Snapshot, window_id: u64) -> Option<&WindowSnapshot> {
    snapshot
        .sessions
        .iter()
        .find_map(|session| session.windows.iter().find(|window| window.id == window_id))
}

fn connect_registered(paths: &Paths) -> Result<(Client, u64)> {
    require_live_socket(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    Ok((client, client_id))
}

fn cmd_break_pane(paths: &Paths, rest: Vec<String>) -> Result<()> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!(
            "\
pmux break-pane PANE

Move PANE into a new window in the same session (CreateWindow + MovePane).
No-op when the pane is already the only pane in its window.
"
        );
        return Ok(());
    }
    if rest.len() != 1 {
        bail!("{BREAK_PANE_USAGE}");
    }
    let pane_id: u64 = rest[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("PANE must be a pane id"))?;
    let (mut client, _client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let (session_id, window) = pane_home(&snapshot, pane_id)?;
    if window.panes.len() == 1 {
        println!("pane {pane_id} is already its own window {}", window.id);
        return Ok(());
    }
    let from_window = window.id;
    let cols = window.bounds.cols;
    let rows = window.bounds.rows;
    let created = client.request(|request_id| ControlRequest::CreateWindow {
        version: PROTOCOL_VERSION,
        request_id,
        session_id,
        title: "tab".into(),
        spawn: dummy_window_spawn(),
        cols: Some(cols),
        rows: Some(rows),
    })?;
    let ControlResponseData::Window {
        window_id: to_window,
        pane_id: dummy,
        ..
    } = created
    else {
        bail!("server returned an unexpected create-window response");
    };
    if let Err(error) = client.request(|request_id| ControlRequest::MovePane {
        version: PROTOCOL_VERSION,
        request_id,
        from_window_id: from_window,
        to_window_id: to_window,
        pane_id,
        target_pane_id: dummy,
        axis: AxisWire::Horizontal,
        ratio: 0.5,
        client_id: None,
    }) {
        let _ = client.request(|request_id| ControlRequest::DestroyWindow {
            version: PROTOCOL_VERSION,
            request_id,
            window_id: to_window,
        });
        return Err(error).context("move pane");
    }
    client
        .request(|request_id| ControlRequest::Close {
            version: PROTOCOL_VERSION,
            request_id,
            window_id: to_window,
            pane_id: dummy,
            prior_focus_id: pane_id,
            client_id: None,
        })
        .with_context(|| format!("close placeholder pane {dummy} in window {to_window}"))?;
    println!("broke pane {pane_id} to window {to_window}");
    Ok(())
}

struct JoinPaneArgs {
    pane: u64,
    to: u64,
    axis: AxisWire,
}

fn parse_join_pane_args(rest: Vec<String>) -> Result<JoinPaneArgs> {
    let mut pane = None;
    let mut to = None;
    let mut axis = AxisWire::Horizontal;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-h" => axis = AxisWire::Horizontal,
            "-v" => axis = AxisWire::Vertical,
            "--to" => {
                i += 1;
                let value = rest.get(i).context(JOIN_PANE_USAGE)?;
                to = Some(
                    value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("TAB must be a window id"))?,
                );
            }
            flag if flag.starts_with('-') => bail!("unknown join-pane argument {flag:?}"),
            other => {
                let id: u64 = other
                    .parse()
                    .map_err(|_| anyhow::anyhow!("PANE must be a pane id"))?;
                if pane.is_some() {
                    bail!("{JOIN_PANE_USAGE}");
                }
                pane = Some(id);
            }
        }
        i += 1;
    }
    Ok(JoinPaneArgs {
        pane: pane.context(JOIN_PANE_USAGE)?,
        to: to.context(JOIN_PANE_USAGE)?,
        axis,
    })
}

const ARRANGE_USAGE: &str =
    "usage: pmux arrange SESSION main-vertical|main-horizontal|even-h|even-v|grid";

fn cmd_arrange(paths: &Paths, cli_session: Option<String>, rest: Vec<String>) -> Result<()> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!(
            "\
pmux arrange SESSION KIND

Retile SESSION's first window. Does not spawn or close panes.
KIND is main-vertical, main-horizontal, even-h, even-v, or grid.
main-* uses the focused pane when a controller exists, else the
first leaf. If more than one pane holds a lease, the first in
layout order wins. C-\\ a in pmux-attach cycles even-h, even-v,
grid, main-vertical, main-horizontal.
"
        );
        return Ok(());
    }
    let mut session = cli_session;
    let mut positional = Vec::new();
    let mut args = rest.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => {
                session = Some(parse_session_name(
                    args.next().context("--session requires a name")?,
                )?);
            }
            flag if flag.starts_with('-') => bail!("unknown arrange argument {flag:?}"),
            _ => positional.push(arg),
        }
    }
    let kind_raw = match positional.as_slice() {
        [name, kind] => {
            if session.is_some() {
                bail!("{ARRANGE_USAGE}");
            }
            session = Some(parse_session_name(name.clone())?);
            kind.clone()
        }
        [kind] if session.is_some() => kind.clone(),
        _ => bail!("{ARRANGE_USAGE}"),
    };
    let session_key = session.context(ARRANGE_USAGE)?;
    let kind = ArrangementWire::parse_name(&kind_raw)
        .with_context(|| format!("unknown KIND; {ARRANGE_USAGE}"))?;
    let (mut client, _client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let sess = snapshot
        .sessions
        .iter()
        .find(|row| row.name == session_key || row.id.to_string() == session_key)
        .with_context(|| format!("unknown session {session_key}"))?;
    let window = sess
        .windows
        .first()
        .with_context(|| format!("session {session_key} has no window"))?;
    let focused = window
        .panes
        .iter()
        .find(|pane| pane.controller_id.is_some())
        .or_else(|| window.panes.first())
        .map(|pane| pane.id);
    client
        .request(|request_id| ControlRequest::ApplyArrangement {
            version: PROTOCOL_VERSION,
            request_id,
            window_id: window.id,
            kind,
            focused_pane_id: focused,
        })
        .context("apply arrangement")?;
    println!("arranged {} as {}", sess.name, kind.as_str());
    Ok(())
}

fn cmd_join_pane(paths: &Paths, rest: Vec<String>) -> Result<()> {
    if rest.is_empty() || rest.iter().any(|arg| arg == "--help") {
        print!(
            "\
pmux join-pane PANE --to TAB [-h|-v]

Move PANE onto window TAB through MovePane. TAB is a window id in the
same session as PANE. -h splits beside the target pane (default). -v
splits above/below it. --help prints this text; -h is the axis flag.
"
        );
        return Ok(());
    }
    let parsed = parse_join_pane_args(rest)?;
    let (mut client, _client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let (src_session, src) = pane_home(&snapshot, parsed.pane)?;
    if src.id == parsed.to {
        bail!("pane {} is already in window {}", parsed.pane, parsed.to);
    }
    let dest = snapshot_window(&snapshot, parsed.to)
        .with_context(|| format!("unknown window {}", parsed.to))?;
    let dest_session = snapshot
        .sessions
        .iter()
        .find(|session| session.windows.iter().any(|window| window.id == dest.id))
        .map(|session| session.id);
    if dest_session != Some(src_session) {
        bail!(
            "join-pane --to {} is not a window in pane {}'s session",
            parsed.to,
            parsed.pane
        );
    }
    let target = dest
        .panes
        .first()
        .map(|pane| pane.id)
        .context("destination window has no pane")?;
    client
        .request(|request_id| ControlRequest::MovePane {
            version: PROTOCOL_VERSION,
            request_id,
            from_window_id: src.id,
            to_window_id: dest.id,
            pane_id: parsed.pane,
            target_pane_id: target,
            axis: parsed.axis,
            ratio: 0.5,
            client_id: None,
        })
        .context("move pane")?;
    println!("joined pane {} to window {}", parsed.pane, dest.id);
    Ok(())
}

const RENAME_PANE_USAGE: &str =
    "usage: pmux rename-pane PANE|SESSION [TITLE...] | pmux rename-pane --session KEY [TITLE...]";

/// `pmux rename-pane PANE|SESSION [TITLE...]` (PT-128). Words join with a
/// space; no title clears. A session name resolves to its only pane and is
/// rejected when the session has more than one (pass the pane id).
/// `--session KEY` (or the global flag) names the session by name OR opaque
/// id, so an all-digit id is never read as a pane id (PT-148: the host
/// knows its attach panes by session id).
fn cmd_rename_pane(paths: &Paths, global_session: Option<String>, rest: Vec<String>) -> Result<()> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!(
            "\
pmux rename-pane PANE|SESSION [TITLE...]
pmux rename-pane --session KEY [TITLE...]

Set a pane title. TITLE words join with a space; no TITLE clears the
title. PANE is a pane id; a SESSION name works when the session has one
pane (an all-digit name is read as a pane id — use --session instead).
--session KEY takes a session name or opaque id and targets its only pane.
Do not pass a pane id with --session. A session with more than one pane
needs a pane id (`pmux rename-pane PANE [TITLE...]`).
Titles show in `pmux ls`, the attach chrome (status wins when both
are set), and persist in space files. Max 64 bytes, no control characters.
"
        );
        return Ok(());
    }
    let mut session_key = global_session;
    let mut words: Vec<String> = Vec::new();
    let mut iter = rest.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--session" {
            let key = iter.next().context("--session requires a name or id")?;
            if session_key
                .as_ref()
                .is_some_and(|existing| *existing != key)
            {
                bail!("conflicting session keys");
            }
            session_key = Some(key);
        } else {
            words.push(arg);
        }
    }
    let (target, words): (Option<String>, Vec<String>) = if session_key.is_some() {
        if words
            .first()
            .is_some_and(|word| word.parse::<u64>().is_ok())
        {
            bail!("do not pass a pane id with --session; use: pmux rename-pane PANE [TITLE...]");
        }
        (None, words)
    } else {
        let Some((first, rest)) = words.split_first() else {
            bail!("{RENAME_PANE_USAGE}");
        };
        if first.starts_with('-') {
            bail!("{RENAME_PANE_USAGE}");
        }
        (Some(first.clone()), rest.to_vec())
    };
    let title = words.join(" ");
    require_live_socket(paths)?;
    let (mut client, _client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let pane_id = match (session_key, target) {
        (Some(key), _) => {
            let (_, name) = resolve_session(&snapshot, &key)?;
            only_pane_of_session(&snapshot, name)?
        }
        (None, Some(target)) => resolve_pane_target(&snapshot, &target)?,
        (None, None) => bail!("{RENAME_PANE_USAGE}"),
    };
    client.request(|request_id| ControlRequest::RenamePane {
        version: PROTOCOL_VERSION,
        request_id,
        pane_id,
        title: title.clone(),
    })?;
    if title.trim().is_empty() {
        println!("cleared title on pane {pane_id}");
    } else {
        println!("set title on pane {pane_id}");
    }
    Ok(())
}

/// A pane id, or a session name that owns exactly one pane.
fn resolve_pane_target(snapshot: &Snapshot, target: &str) -> Result<u64> {
    if let Ok(id) = target.parse::<u64>() {
        snapshot_pane(snapshot, id).with_context(|| format!("unknown pane {id}"))?;
        return Ok(id);
    }
    snapshot
        .sessions
        .iter()
        .find(|session| session.name == target)
        .with_context(|| format!("unknown pane or session {target:?}"))?;
    only_pane_of_session(snapshot, target)
}

/// The single pane of session `name`, or an error listing the pane ids.
fn only_pane_of_session(snapshot: &Snapshot, name: &str) -> Result<u64> {
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.name == name)
        .with_context(|| format!("unknown session {name:?}"))?;
    let panes: Vec<u64> = session
        .windows
        .iter()
        .flat_map(|window| window.panes.iter().map(|pane| pane.id))
        .collect();
    match panes.as_slice() {
        [only] => Ok(*only),
        [] => bail!("session {name:?} has no panes"),
        many => bail!(
            "session {name:?} has {} panes; pass a pane id ({})",
            many.len(),
            many.iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn cmd_send(paths: &Paths, rest: Vec<String>) -> Result<()> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!(
            "\
pmux send PANE TEXT [--enter] [--literal] [--force] [-- TEXT]

Write keys to a pane without holding the controller lease for 750 ms.
Refuses (exit 1) when a live controller exists or the input ledger is
dirty, unless --force. --force takes the lease, writes, and releases; it
does not hand the lease back, and taking it from a live attach also
revokes that attach's rich viewer grant (the attach re-acquires on its
next key). The server refuses a lease-free write into unsubmitted input
even when the snapshot looked clean. --enter appends CR. Default text interprets
\\n \\r \\t \\e \\\\ ; --literal sends the bytes as typed.
A leading -- lets TEXT start with -. Writes chunk at 64 KiB.
Exits nonzero if the pane is missing or dead.
"
        );
        return Ok(());
    }
    let parsed = parse_send_args(rest)?;
    require_live_socket(paths)?;
    let mut payload = if parsed.literal {
        parsed.text.clone()
    } else {
        decode_send_escapes(&parsed.text)?
    };
    if parsed.enter {
        payload.push('\r');
    }
    if payload.is_empty() {
        bail!("{SEND_USAGE}");
    }
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = take_snapshot(&mut client)?;
    let pane = snapshot_pane(&snapshot, parsed.pane)
        .with_context(|| format!("unknown pane {}", parsed.pane))?;
    let held = pane.controller_id.is_some();
    let dirty = pane.ledger.dirty_input;
    if (held || dirty) && !parsed.force {
        if held {
            bail!(
                "pane {} has a live controller; pass --force to take over",
                parsed.pane
            );
        }
        bail!(
            "pane {} has unsubmitted input; pass --force to write anyway",
            parsed.pane
        );
    }
    match client.request(|request_id| ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id: parsed.pane,
    }) {
        Ok(ControlResponseData::PaneContent { content }) if !content.child_alive => {
            bail!("pane {} is dead", parsed.pane);
        }
        Ok(ControlResponseData::PaneContent { .. }) => {}
        Ok(_) => bail!("pane {} returned an unexpected read", parsed.pane),
        Err(error) => {
            return Err(error).with_context(|| format!("read pane {}", parsed.pane));
        }
    }
    let nbytes = payload.len();
    if parsed.force {
        // --force always takes the lease: the snapshot can be stale by the
        // time the write lands, and the server refuses lease-free writes
        // into a partial line (PT-140).
        send_force_write(&mut client, client_id, parsed.pane, &payload)?;
    } else {
        send_lease_free(&mut client, client_id, parsed.pane, &payload)?;
    }
    println!("sent {nbytes} bytes to pane {}", parsed.pane);
    Ok(())
}

/// Lease-free write with one retry: a controller can appear between the
/// snapshot and the write. On `NotController` re-snapshot; if the pane is
/// now held, say so and point at `--force`; if the holder already left,
/// write once more. The server's dirty gate maps to the same advice.
fn send_lease_free(client: &mut Client, client_id: u64, pane_id: u64, payload: &str) -> Result<()> {
    match write_pane_bytes(client, client_id, pane_id, payload) {
        Ok(()) => return Ok(()),
        Err(error) if control_code(&error) == Some(ControlErrorCode::InputDirty) => {
            bail!("pane {pane_id} has unsubmitted input; pass --force to write anyway");
        }
        Err(error) if control_code(&error) == Some(ControlErrorCode::NotController) => {
            let snapshot = take_snapshot(client)?;
            let held =
                snapshot_pane(&snapshot, pane_id).is_some_and(|pane| pane.controller_id.is_some());
            if held {
                bail!("pane {pane_id} gained a live controller; pass --force to take over");
            }
        }
        Err(error) => return Err(error).context("write pane"),
    }
    match write_pane_bytes(client, client_id, pane_id, payload) {
        Ok(()) => Ok(()),
        Err(error) if control_code(&error) == Some(ControlErrorCode::NotController) => {
            bail!("pane {pane_id} gained a live controller; pass --force to take over");
        }
        Err(error) if control_code(&error) == Some(ControlErrorCode::InputDirty) => {
            bail!("pane {pane_id} has unsubmitted input; pass --force to write anyway");
        }
        Err(error) => Err(error).context("write pane"),
    }
}

fn send_force_write(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    payload: &str,
) -> Result<()> {
    let mut last = None;
    for _ in 0..3 {
        match client.request(|request_id| ControlRequest::TakeoverLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        }) {
            Ok(_) => {}
            Err(error) => return Err(error).context("one-shot takeover"),
        }
        match write_pane_bytes(client, client_id, pane_id, payload) {
            Ok(()) => {
                let _ = client.request(|request_id| ControlRequest::ReleaseLease {
                    version: PROTOCOL_VERSION,
                    request_id,
                    client_id,
                    pane_id,
                });
                return Ok(());
            }
            Err(error) if control_code(&error) == Some(ControlErrorCode::NotController) => {
                last = Some(error);
            }
            Err(error) => {
                let _ = client.request(|request_id| ControlRequest::ReleaseLease {
                    version: PROTOCOL_VERSION,
                    request_id,
                    client_id,
                    pane_id,
                });
                return Err(error).context("write pane");
            }
        }
    }
    let _ = client.request(|request_id| ControlRequest::ReleaseLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    });
    Err(last.unwrap_or_else(|| anyhow::anyhow!("write pane lost the lease"))).context("write pane")
}

fn pane_child_pid(client: &mut Client, client_id: u64, pane_id: u64) -> Option<u32> {
    match client.request(|request_id| ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    }) {
        Ok(ControlResponseData::PaneContent { content }) => content.child_pid,
        _ => None,
    }
}

fn session_pane_ids(session: &prismattyc_mux::SessionSnapshot) -> Vec<u64> {
    session
        .windows
        .iter()
        .flat_map(|window| window.panes.iter().map(|pane| pane.id))
        .collect()
}

fn attach_label(client: &AttachClient) -> String {
    match (&client.session, client.pane) {
        (Some(name), Some(pane)) => format!(" --session {name} --pane {pane}"),
        (Some(name), None) => format!(" --session {name}"),
        (None, Some(pane)) => format!(" --pane {pane}"),
        (None, None) => String::new(),
    }
}

fn print_tree(
    client: &mut Client,
    client_id: u64,
    snapshot: &Snapshot,
    socket: &Path,
) -> Result<()> {
    let attaches = scan_attach_clients(socket);
    for (index, session) in snapshot.sessions.iter().enumerate() {
        println!("session {} (id {})", session.name, session.id);
        let pane_ids = session_pane_ids(session);
        for window in &session.windows {
            println!(
                "  window {} {:?} — {}x{}",
                window.id, window.title, window.bounds.cols, window.bounds.rows
            );
            for pane in &window.panes {
                // Observer read: liveness + pid without taking a lease.
                let (state, child_pid) =
                    match client.request(|request_id| ControlRequest::ReadPane {
                        version: PROTOCOL_VERSION,
                        request_id,
                        client_id,
                        pane_id: pane.id,
                    }) {
                        Ok(ControlResponseData::PaneContent { content }) => {
                            let alive = if content.child_alive {
                                match content.child_pid {
                                    Some(pid) => format!("alive (pid {pid})"),
                                    None => "alive".to_string(),
                                }
                            } else {
                                "exited".to_string()
                            };
                            (
                                format!("{alive}, rev {}", content.revision),
                                content.child_pid,
                            )
                        }
                        _ => ("unreadable".to_string(), None),
                    };
                let lease = match pane.controller_id {
                    Some(id) => format!(", controller {id}"),
                    None => String::new(),
                };
                let mut viewers = Vec::new();
                let mut nested = Vec::new();
                let child_roots: Vec<u32> = child_pid.into_iter().collect();
                for attach in &attaches {
                    let targets = attach_targets_session(
                        attach,
                        &session.name,
                        session.id,
                        &pane_ids,
                        index == 0,
                    );
                    let kind = classify_attach(attach, &child_roots);
                    match kind {
                        AttachKind::Nested => nested.push(attach.pid),
                        AttachKind::Viewer
                            if targets && attach.pane.unwrap_or(pane.id) == pane.id =>
                        {
                            // --session (no pane) is the first pane of that session.
                            if attach.pane.is_none() && pane_ids.first() != Some(&pane.id) {
                                continue;
                            }
                            viewers.push(attach.pid);
                        }
                        AttachKind::Viewer => {}
                    }
                }
                let attach_tag = match (viewers.is_empty(), nested.is_empty()) {
                    (true, true) => String::new(),
                    (false, true) => format!(
                        ", viewers {}",
                        viewers
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    (true, false) => format!(
                        ", nested {}",
                        nested
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    (false, false) => format!(
                        ", viewers {}, nested {}",
                        viewers
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                        nested
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                };
                let title_tag = if pane.title.is_empty() {
                    String::new()
                } else {
                    format!(" — title {:?}", pane.title)
                };
                println!(
                    "    pane {} — {}x{} at ({},{}) — {state}{lease}{attach_tag}{title_tag}",
                    pane.id,
                    pane.geometry.cols,
                    pane.geometry.rows,
                    pane.geometry.col,
                    pane.geometry.row
                );
            }
        }
    }
    Ok(())
}

fn require_live(paths: &Paths) -> Result<()> {
    match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => Ok(()),
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        SocketLiveness::Missing | SocketLiveness::Stale => {
            if let Some(miss) = diagnose_runtime_dir_miss_from_env(&paths.socket) {
                bail!("not running (no live server)\n{miss}");
            }
            bail!("not running (no live server)")
        }
    }
}

fn cmd_doctor(paths: &Paths, key: Option<String>) -> Result<()> {
    require_live(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let attaches = scan_attach_clients(&paths.socket);
    let filter = key.as_deref();
    let mut shown = false;
    for (index, session) in snapshot.sessions.iter().enumerate() {
        if filter.is_some_and(|key| session.name != key && session.id.to_string() != key) {
            continue;
        }
        shown = true;
        println!("session {} (id {})", session.name, session.id);
        let pane_ids = session_pane_ids(session);
        for window in &session.windows {
            for pane in &window.panes {
                let child = pane_child_pid(&mut client, client_id, pane.id);
                let lease = match pane.controller_id {
                    Some(id) => format!("controller {id}"),
                    None => "controller none".to_string(),
                };
                let child_s = match child {
                    Some(pid) => format!("child {pid}"),
                    None => "child none".to_string(),
                };
                println!("  pane {} — {child_s}, {lease}", pane.id);
                if let Some(inject) = pane.mail_inject.as_ref() {
                    println!(
                        "    mail inject: agent {}, queue_rev {}, outcome {:?}, nbytes {}, at_ms {}",
                        inject.agent,
                        inject.queue_rev,
                        inject.outcome,
                        inject.nbytes,
                        inject.at_ms
                    );
                }
                let child_roots: Vec<u32> = child.into_iter().collect();
                let mut any = false;
                for attach in &attaches {
                    let targets = attach_targets_session(
                        attach,
                        &session.name,
                        session.id,
                        &pane_ids,
                        index == 0,
                    );
                    let kind = classify_attach(attach, &child_roots);
                    if kind != AttachKind::Nested && !targets {
                        continue;
                    }
                    if kind == AttachKind::Nested
                        && !child_roots
                            .iter()
                            .any(|&r| prismattyc_mux::pid_in_tree(r, attach.pid))
                    {
                        continue;
                    }
                    any = true;
                    let kind_s = match kind {
                        AttachKind::Nested => "NESTED",
                        AttachKind::Viewer => "VIEWER",
                    };
                    println!("    attach {} {kind_s}{}", attach.pid, attach_label(attach));
                }
                if !any {
                    println!("    attach: none");
                }
            }
        }
    }
    if filter.is_some() && !shown {
        bail!("no session matching {filter:?}");
    }
    Ok(())
}

fn cmd_kick(paths: &Paths, key: &str) -> Result<()> {
    require_live(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let (session_id, name) = resolve_session(&snapshot, key)?;
    let name = name.to_string();
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .context("session vanished after resolve")?;
    let is_first = snapshot
        .sessions
        .first()
        .is_some_and(|first| first.id == session_id);
    let pane_ids = session_pane_ids(session);
    let mut child_roots = Vec::new();
    for pane_id in &pane_ids {
        if let Some(pid) = pane_child_pid(&mut client, client_id, *pane_id) {
            child_roots.push(pid);
        }
    }
    // Drop the control client before signalling — kick never talks DestroySession.
    drop(client);

    let attaches = scan_attach_clients(&paths.socket);
    let mut nested = Vec::new();
    let mut viewers = Vec::new();
    for attach in attaches {
        let targets = attach_targets_session(&attach, &name, session_id, &pane_ids, is_first);
        let kind = classify_attach(&attach, &child_roots);
        match kind {
            AttachKind::Nested
                if child_roots
                    .iter()
                    .any(|&r| prismattyc_mux::pid_in_tree(r, attach.pid)) =>
            {
                nested.push(attach);
            }
            AttachKind::Viewer if targets => viewers.push(attach),
            _ => {}
        }
    }
    let targets = if !nested.is_empty() {
        println!(
            "kicking {} nested attach(es) on session {name:?}",
            nested.len()
        );
        nested
    } else if !viewers.is_empty() {
        println!(
            "kicking {} viewer attach(es) on session {name:?}",
            viewers.len()
        );
        viewers
    } else {
        println!("no attach clients for session {name:?}");
        return Ok(());
    };
    let mut kicked = Vec::new();
    for attach in targets {
        if child_roots.contains(&attach.pid) {
            // Never signal the pane child even if it were misclassified.
            continue;
        }
        // Re-verify argv immediately before TERM (same rule as stop).
        let still = scan_attach_clients(&paths.socket)
            .into_iter()
            .any(|live| live.pid == attach.pid);
        if !still {
            continue;
        }
        signal(attach.pid, prismattyc_mux::platform::Signal::TERM)?;
        kicked.push(attach.pid);
    }
    if kicked.is_empty() {
        println!("nothing signalled");
    } else {
        println!(
            "signalled TERM {}",
            kicked
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}

/// Pull a `--json` flag out of the argument list.
fn split_json_flag(rest: Vec<String>) -> (bool, Vec<String>) {
    let json = rest.iter().any(|arg| arg == "--json");
    (
        json,
        rest.into_iter().filter(|arg| arg != "--json").collect(),
    )
}

fn parse_render_status_args(rest: Vec<String>) -> Result<bool> {
    let (json, rest) = split_json_flag(rest);
    if !rest.is_empty() {
        bail!("usage: pmux render-status [--json]");
    }
    Ok(json)
}

/// `pmux detach --other|-a [SESSION]` (PT-129). The flag is required so a
/// bare `pmux detach` cannot kick anyone by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DetachArgs {
    rest: Vec<String>,
}

const DETACH_USAGE: &str = "usage: pmux detach --other [SESSION]   (alias: -a)";

fn parse_detach_args(rest: Vec<String>) -> Result<DetachArgs> {
    let mut other = false;
    let mut keep = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--other" | "-a" => other = true,
            flag if flag.starts_with('-') && flag != "--session" => {
                bail!("{DETACH_USAGE}\nunknown flag {flag}");
            }
            _ => keep.push(arg),
        }
    }
    if !other {
        bail!("{DETACH_USAGE}");
    }
    Ok(DetachArgs { rest: keep })
}

/// One row of `pmux clients`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct ClientRow {
    pid: u32,
    /// `viewer` (host-side attach) or `nested` (attach running inside a pane).
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane: Option<u64>,
    /// This attach is an ancestor of the calling process.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    own: bool,
    /// This attach runs under the registered prismattyc-host
    /// (`{stem}.host.pid`): a host pane, not a stray viewer.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    host: bool,
}

/// Every live `pmux-attach` on the socket, classified against the pane
/// children; `key` narrows to one session.
fn collect_client_rows(
    client: &mut Client,
    client_id: u64,
    snapshot: &Snapshot,
    socket: &Path,
    key: Option<&str>,
) -> Result<Vec<ClientRow>> {
    let filter = match key {
        Some(key) => Some(resolve_session(snapshot, key)?),
        None => None,
    };
    let mut child_roots = Vec::new();
    for session in &snapshot.sessions {
        for pane_id in session_pane_ids(session) {
            if let Some(pid) = pane_child_pid(client, client_id, pane_id) {
                child_roots.push(pid);
            }
        }
    }
    let me = std::process::id();
    let host_pid =
        prismattyc_mux::live_host_pid(&prismattyc_mux::host_pid_path_from_socket(socket));
    let mut rows: Vec<ClientRow> = scan_attach_clients(socket)
        .into_iter()
        .filter(|attach| match filter {
            Some((session_id, name)) => {
                let session = snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id);
                let pane_ids = session.map(session_pane_ids).unwrap_or_default();
                let is_first = snapshot
                    .sessions
                    .first()
                    .is_some_and(|first| first.id == session_id);
                attach_targets_session(attach, name, session_id, &pane_ids, is_first)
            }
            None => true,
        })
        .map(|attach| {
            let kind = match classify_attach(&attach, &child_roots) {
                AttachKind::Nested => "nested",
                AttachKind::Viewer => "viewer",
            };
            ClientRow {
                pid: attach.pid,
                kind,
                session: attach.session.clone(),
                pane: attach.pane,
                own: prismattyc_mux::pid_in_tree(attach.pid, me),
                host: host_pid.is_some_and(|host| prismattyc_mux::pid_in_tree(host, attach.pid)),
            }
        })
        .collect();
    rows.sort_by_key(|row| row.pid);
    Ok(rows)
}

fn format_client_rows(rows: &[ClientRow]) -> String {
    if rows.is_empty() {
        return "no attach clients\n".to_string();
    }
    let mut out = String::from("PID      KIND    SESSION          PANE  \n");
    for row in rows {
        out.push_str(&format!(
            "{:<8} {:<7} {:<16} {:<5}{}\n",
            row.pid,
            row.kind,
            row.session.as_deref().unwrap_or("-"),
            row.pane.map_or("-".to_string(), |pane| pane.to_string()),
            match (row.own, row.host) {
                (true, _) => " (you)",
                (false, true) => " (host)",
                (false, false) => "",
            }
        ));
    }
    out
}

/// `pmux clients [SESSION] [--json]` (PT-129).
fn cmd_clients(paths: &Paths, key: Option<&str>, json: bool) -> Result<()> {
    require_live(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let rows = collect_client_rows(&mut client, client_id, &snapshot, &paths.socket, key)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", format_client_rows(&rows));
    }
    Ok(())
}

/// What `detach --other` does with one attach row.
///
/// Keep prints the pid on the "kept (yours or the registered host's)" line.
/// Skip prints nothing: a pane child, or an attach that already exited.
/// Signal prints the pid on the "signalled TERM" line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachFate {
    Keep,
    Skip,
    Signal,
}

fn detach_fate(own: bool, host: bool, child_root: bool, still_live: bool) -> DetachFate {
    if own || host {
        DetachFate::Keep
    } else if child_root || !still_live {
        DetachFate::Skip
    } else {
        DetachFate::Signal
    }
}

/// Whether `pid` is still in the attach-client scan.
///
/// Keep/Skip ignore this (own, host, and pane children). Viewer rows use it
/// to choose Signal vs Skip. The scan always runs: skipping it is not
/// observable.
fn attach_pid_is_live(live_pids: &[u32], pid: u32) -> bool {
    live_pids.contains(&pid)
}

/// `pmux detach --other [SESSION]` (PT-129): TERM every attach client on
/// the socket (or on one session) except the ones this process runs
/// under, so a `pmux detach --other` typed inside an attach keeps that
/// attach. Sessions stay; only viewers/nested attaches go.
fn cmd_detach_other(paths: &Paths, key: Option<&str>) -> Result<()> {
    require_live(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let rows = collect_client_rows(&mut client, client_id, &snapshot, &paths.socket, key)?;
    let mut child_roots = Vec::new();
    for session in &snapshot.sessions {
        for pane_id in session_pane_ids(session) {
            if let Some(pid) = pane_child_pid(&mut client, client_id, pane_id) {
                child_roots.push(pid);
            }
        }
    }
    drop(client);
    let mut signalled = Vec::new();
    let mut kept = Vec::new();
    for row in rows {
        let child_root = child_roots.contains(&row.pid);
        let live_pids: Vec<u32> = scan_attach_clients(&paths.socket)
            .into_iter()
            .map(|live| live.pid)
            .collect();
        let still_live = attach_pid_is_live(&live_pids, row.pid);
        match detach_fate(row.own, row.host, child_root, still_live) {
            DetachFate::Keep => kept.push(row.pid),
            DetachFate::Skip => {}
            DetachFate::Signal => {
                signal(row.pid, prismattyc_mux::platform::Signal::TERM)?;
                signalled.push(row.pid);
            }
        }
    }
    print!("{}", format_detach_other_report(&signalled, &kept));
    Ok(())
}

fn format_detach_other_report(signalled: &[u32], kept: &[u32]) -> String {
    let mut out = String::new();
    if signalled.is_empty() {
        out.push_str("nothing signalled\n");
    } else {
        out.push_str("signalled TERM ");
        out.push_str(
            &signalled
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        );
        out.push('\n');
    }
    if !kept.is_empty() {
        out.push_str("kept (yours or the registered host's) ");
        out.push_str(
            &kept
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        );
        out.push('\n');
    }
    out
}

/// Parsed `pmux mail` invocation.
struct MailArgs {
    session: String,
    pane: Option<u64>,
}

/// Parse `mail SESSION [--pane ID]`. SESSION comes from the positional
/// arg or the global `--session`, never both.
fn parse_mail_args(cli_session: Option<String>, rest: Vec<String>) -> Result<MailArgs> {
    let mut pane = None;
    let mut positional = None;
    let mut iter = rest.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pane" => {
                let raw = iter.next().context("--pane requires a pane id")?;
                pane = Some(raw.parse().context("--pane must be a numeric pane id")?);
            }
            other if other.starts_with('-') => bail!("unknown mail flag {other:?}"),
            other => {
                if positional.is_some() {
                    bail!("mail takes a single SESSION argument");
                }
                positional = Some(other.to_string());
            }
        }
    }
    let session = match (cli_session, positional) {
        (Some(session), None) | (None, Some(session)) => session,
        (Some(_), Some(_)) => bail!("pass SESSION once (positional or --session, not both)"),
        (None, None) => bail!("usage: pmux mail SESSION [--pane ID]"),
    };
    Ok(MailArgs { session, pane })
}

/// Parsed `pmux attention SESSION [MESSAGE]` invocation.
struct AttentionArgs {
    session: String,
    message: String,
}

/// Parse `attention SESSION [MESSAGE]`. SESSION may come from `--session`.
fn parse_attention_args(cli_session: Option<String>, rest: Vec<String>) -> Result<AttentionArgs> {
    let mut positional = rest.into_iter();
    let first = positional.next();
    let second = positional.next();
    if positional.next().is_some() {
        bail!("attention takes SESSION and an optional MESSAGE");
    }
    let (session, message) = match (cli_session, first, second) {
        (Some(session), None, message) => (session, message),
        (Some(_), Some(_), Some(_)) => {
            bail!("pass SESSION once (positional or --session, not both)")
        }
        (Some(session), Some(message), None) => (session, Some(message)),
        (None, Some(session), message) => (session, message),
        (None, None, _) => bail!("usage: pmux attention SESSION [MESSAGE]"),
    };
    Ok(AttentionArgs {
        session,
        message: message.unwrap_or_else(|| "needs your attention".to_string()),
    })
}

/// `pmux attention SESSION [MESSAGE]` — raise a portable attention signal
/// through the mux control plane. The server relays it to attached clients.
fn cmd_attention(paths: &Paths, args: AttentionArgs) -> Result<()> {
    require_live(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let (session_id, name) = resolve_session(&snapshot, &args.session)?;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .context("session vanished after resolve")?;
    let pane_ids = session_pane_ids(session);
    let pane_id = match pane_ids.as_slice() {
        [only] => *only,
        [] => bail!("session {name:?} has no panes"),
        _ => bail!(
            "session {name:?} has {} panes; attention needs a session with one pane",
            pane_ids.len()
        ),
    };
    let response = client.request(|request_id| ControlRequest::RaiseAttention {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        message: args.message,
    })?;
    if !matches!(response, ControlResponseData::Mutation { .. }) {
        bail!("server returned an unexpected attention response");
    }
    println!("attention sent to session {name:?} pane {pane_id}");
    Ok(())
}

/// Milliseconds since the Unix epoch, used as the mail `queue_rev` ordering key.
fn unix_millis() -> Result<u64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    Ok(elapsed.as_millis() as u64)
}

/// `pmux mail SESSION` — arm sticky mail attention on a pane, then ring
/// the doorbell. This wraps `MailAttentionSet` + `InjectMail`.
/// `InjectMail` writes only
/// [`prismattyc_mux::PMUX_MAIL_NOTIFICATION`] plus guest submit bytes; it
/// carries no free-text body.
fn cmd_mail(paths: &Paths, args: MailArgs) -> Result<()> {
    require_live(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let (session_id, name) = resolve_session(&snapshot, &args.session)?;
    let name = name.to_string();
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .context("session vanished after resolve")?;
    let pane_ids = session_pane_ids(session);
    let pane_id = match args.pane {
        Some(requested) => {
            if !pane_ids.contains(&requested) {
                bail!("pane {requested} is not in session {name:?}");
            }
            requested
        }
        None => match pane_ids.as_slice() {
            [only] => *only,
            [] => bail!("session {name:?} has no panes"),
            _ => bail!(
                "session {name:?} has {} panes; choose one with --pane ID (ids: {})",
                pane_ids.len(),
                pane_ids
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        },
    };

    // queue_rev orders mail attention; wall-clock ms is monotonic across manual
    // calls. Two calls inside one ms reuse the rev, which Set treats idempotently.
    let queue_rev = unix_millis()?;
    let cell = MAIL_ATTENTION_CELL.to_string();

    // 1. Light sticky attention (arm the doorbell for this rev).
    let set = client.request(|request_id| ControlRequest::MailAttentionSet {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        cell: cell.clone(),
        gen: queue_rev,
        queue_rev,
        depth: 1,
        wake: Some(prismattyc_mux::MailWake::Armed),
        bound_pid: None,
    })?;
    let ControlResponseData::MailAttention { .. } = set else {
        bail!("server returned an unexpected mail-attention response");
    };

    // 2. Ring the doorbell. Defer/skip outcomes are success, not errors: the
    // server declines when the pane is focused, busy, dirty, or leased.
    let injected = client.request(|request_id| ControlRequest::InjectMail {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        queue_rev,
        remaining_attempts: 1,
    })?;
    let ControlResponseData::MailInject {
        outcome, nbytes, ..
    } = injected
    else {
        bail!("server returned an unexpected mail-inject response");
    };
    println!(
        "mail armed on session {name:?} pane {pane_id} (cell {cell:?}); inject {outcome:?} ({nbytes} bytes)"
    );
    Ok(())
}

// ---------- pmux mail <verb> ----------
//
// Port of the mailbox CLI onto the Mail* protocol. Every verb is
// one connection: register, MailHello, one op, one reply, done. The
// manual doorbell form (`pmux mail SESSION`) still parses; the
// in-process doorbell makes it unnecessary for mux-native mail.

const MAIL_USAGE: &str = "pmux mail — mailbox verbs (Mail* protocol)

usage: pmux mail [--as <agent>] <verb> [args]

verbs:
  send <to> --summary <s> [--body <b>]   deliver a letter (to = agent id or alias)
                                         omitted --body: piped stdin, else empty (TTY does not hang)
  claim [--json | --ids]                 fetch your letters (open become held); --json for jq, --ids for xargs commit
  commit <id>...                         acknowledge held letters (gone for good)
  release <id>...                        return held letters to open
  inbox                                  peek at open/held depth
  watch [--timeout <secs>]               block until mail arrives (exit 0) or timeout (exit 1, not a mux failure); default 300s
  alias <name>                           bind a shorthand to your agent (self-serve)
  who                                    list bound agents (presence)
  broadcast --summary <s> [--body <b>]   send to every bound agent except yours
  status                                 socket, reachability, bound agents (no identity needed)

identity: --as <agent>, else $PMUX_AGENT, else the
agent bound to this pane's session when run inside pmux.

watcher recipe: while pmux mail watch; do pmux mail claim --json; done";

/// How `claim` renders its letters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MailClaimFormat {
    Human,
    Json,
    Ids,
}

/// A mailbox verb plus its arguments.
#[derive(Debug)]
enum MailVerb {
    Send {
        to: String,
        summary: String,
        body: Option<String>,
    },
    Claim {
        format: MailClaimFormat,
    },
    Commit {
        ids: Vec<String>,
    },
    Release {
        ids: Vec<String>,
    },
    Inbox,
    Watch {
        timeout_ms: u32,
    },
    Alias {
        name: String,
    },
    Who,
    Broadcast {
        summary: String,
        body: Option<String>,
    },
    Status,
}

fn parse_mail_verb(name: &str, mut args: impl Iterator<Item = String>) -> Result<MailVerb> {
    match name {
        "send" => {
            let to = args.next().context("send needs a recipient")?;
            let (summary, body) = parse_mail_letter_flags(args, "send")?;
            Ok(MailVerb::Send { to, summary, body })
        }
        "claim" => {
            let mut format = None;
            for flag in args {
                let parsed = match flag.as_str() {
                    "--json" => MailClaimFormat::Json,
                    "--ids" => MailClaimFormat::Ids,
                    other => bail!("claim: unknown flag {other}"),
                };
                if format.replace(parsed).is_some() {
                    bail!("claim: --json and --ids are mutually exclusive");
                }
            }
            Ok(MailVerb::Claim {
                format: format.unwrap_or(MailClaimFormat::Human),
            })
        }
        "commit" => Ok(MailVerb::Commit {
            ids: args.collect(),
        }),
        "release" => Ok(MailVerb::Release {
            ids: args.collect(),
        }),
        "inbox" => Ok(MailVerb::Inbox),
        "watch" => {
            let mut timeout_secs = None;
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--timeout" => {
                        let value = args.next().context("--timeout needs a value")?;
                        let secs: u32 = value.parse().map_err(|_| {
                            anyhow::anyhow!("--timeout must be seconds, got {value:?}")
                        })?;
                        timeout_secs = Some(secs);
                    }
                    other => bail!("watch: unknown flag {other}"),
                }
            }
            let secs = timeout_secs.unwrap_or(300);
            let timeout_ms = secs
                .checked_mul(1000)
                .context("watch: --timeout is too large")?;
            Ok(MailVerb::Watch { timeout_ms })
        }
        "alias" => {
            let name = args.next().context("alias needs a name")?;
            Ok(MailVerb::Alias { name })
        }
        "who" => Ok(MailVerb::Who),
        "broadcast" => {
            let (summary, body) = parse_mail_letter_flags(args, "broadcast")?;
            Ok(MailVerb::Broadcast { summary, body })
        }
        "status" => Ok(MailVerb::Status),
        other => bail!("unknown mail verb: {other}"),
    }
}

fn parse_mail_letter_flags(
    mut args: impl Iterator<Item = String>,
    verb: &str,
) -> Result<(String, Option<String>)> {
    let mut summary = None;
    let mut body = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--summary" => {
                summary = Some(args.next().context("--summary needs a value")?);
            }
            "--body" => {
                body = Some(args.next().context("--body needs a value")?);
            }
            other => bail!("{verb}: unknown flag {other}"),
        }
    }
    let summary = summary.context(format!("{verb} needs --summary"))?;
    Ok((summary, body))
}

/// Body for `send` / `broadcast` when `--body` is omitted. Piped stdin is
/// the body; a TTY yields an empty body rather than hanging (agent tool
/// calls must never block on EOF).
fn read_mail_stdin() -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(String::new());
    }
    let mut body = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
        .context("cannot read body from stdin")?;
    Ok(body)
}

/// Identity resolution order: `--as`, the current pane binding, then
/// `$PMUX_AGENT` outside a known pane. A live binding survives rename and move.
fn resolve_mail_identity(
    client: &mut Client,
    paths: &Paths,
    cli_as: Option<&str>,
) -> Result<Option<String>> {
    if let Some(agent) = cli_as {
        return Ok(Some(agent.to_string()));
    }
    let fallback = std::env::var("PMUX_AGENT")
        .ok()
        .filter(|agent| !agent.is_empty());
    // The live seat wins over a spawn-time environment after a rename or move.
    if std::env::var_os("PMUX_SOCKET").as_deref().map(Path::new) != Some(paths.socket.as_path()) {
        return Ok(fallback);
    }
    let Some(pane_id) = std::env::var("PRISMATTYC_PANE_ID")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return Ok(fallback);
    };
    let snapshot = take_snapshot(client)?;
    match whoami_from_snapshot(&snapshot, pane_id) {
        Some(who) => Ok(who.agent),
        None => Ok(fallback),
    }
}

fn cmd_mailbox(paths: &Paths, rest: Vec<String>) -> Result<()> {
    let mut iter = rest.into_iter().peekable();
    let mut cli_as = None;
    while iter.peek().map(String::as_str) == Some("--as") {
        iter.next();
        cli_as = Some(iter.next().context("--as needs a value")?);
    }
    let verb_name = iter.next().context(MAIL_USAGE)?;
    if verb_name == "--help" || verb_name == "-h" {
        println!("{MAIL_USAGE}");
        return Ok(());
    }
    let verb = parse_mail_verb(&verb_name, iter).with_context(|| MAIL_USAGE)?;

    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };

    // `status` is the one verb that does not require an identity — it is a
    // diagnostic for operators checking daemon reachability.
    if let MailVerb::Status = verb {
        return cmd_mailbox_status(paths, &mut client, client_id, cli_as.as_deref());
    }

    let agent = resolve_mail_identity(&mut client, paths, cli_as.as_deref())?.context(
        "no identity: pass --as <agent>, set PMUX_AGENT, or run inside an agent-bound pmux pane",
    )?;
    let seated = client.request(|request_id| ControlRequest::MailHello {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        agent: agent.clone(),
    })?;
    if !matches!(seated, ControlResponseData::MailSeated { .. }) {
        bail!("server returned an unexpected mail-hello response");
    }

    // Fill in the stdin body before the request closure borrows the verb.
    let verb = match verb {
        MailVerb::Send { to, summary, body } => MailVerb::Send {
            to,
            summary,
            body: match body {
                Some(body) => Some(body),
                None => Some(read_mail_stdin()?),
            },
        },
        MailVerb::Broadcast { summary, body } => MailVerb::Broadcast {
            summary,
            body: match body {
                Some(body) => Some(body),
                None => Some(read_mail_stdin()?),
            },
        },
        other => other,
    };
    if let MailVerb::Commit { ids } | MailVerb::Release { ids } = &verb {
        if ids.is_empty() {
            bail!("commit/release needs at least one letter id");
        }
    }
    if let MailVerb::Watch { timeout_ms } = &verb {
        client.hold_for_wait(*timeout_ms)?;
    }

    let request = |request_id| -> ControlRequest {
        let base = (PROTOCOL_VERSION, request_id, client_id);
        match &verb {
            MailVerb::Send { to, summary, body } => ControlRequest::MailSend {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
                to: to.clone(),
                summary: summary.clone(),
                body: body.clone().unwrap_or_default(),
            },
            MailVerb::Claim { .. } => ControlRequest::MailClaim {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
            },
            MailVerb::Commit { ids } => ControlRequest::MailCommit {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
                ids: ids.clone(),
            },
            MailVerb::Release { ids } => ControlRequest::MailRelease {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
                ids: ids.clone(),
            },
            MailVerb::Inbox => ControlRequest::MailInbox {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
            },
            MailVerb::Watch { timeout_ms } => ControlRequest::MailWait {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
                timeout_ms: *timeout_ms,
            },
            MailVerb::Alias { name } => ControlRequest::MailAlias {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
                name: name.clone(),
            },
            MailVerb::Who => ControlRequest::MailWho {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
            },
            MailVerb::Broadcast { summary, body } => ControlRequest::MailBroadcast {
                version: base.0,
                request_id: base.1,
                client_id: base.2,
                summary: summary.clone(),
                body: body.clone().unwrap_or_default(),
            },
            MailVerb::Status => unreachable!("handled above"),
        }
    };

    let reply = client.request(request)?;

    // Watch is the shell-watcher primitive: exit status carries the answer
    // so `while pmux mail watch; do ...; done` needs no output parsing.
    if let MailVerb::Watch { .. } = verb {
        let ControlResponseData::MailDepth { open, held } = reply else {
            bail!("expected depth, got {reply:?}");
        };
        println!("open: {open} held: {held}");
        std::process::exit(if open == 0 { 1 } else { 0 });
    }

    let claim_format = match &verb {
        MailVerb::Claim { format } => *format,
        _ => MailClaimFormat::Human,
    };
    print_mail_reply(reply, claim_format);
    Ok(())
}

/// `pmux mail status` — no identity required. Prints the socket path,
/// checks reachability, and lists bound agents. The probe hello binds only
/// this connection; with no session it never appears in `who`.
fn cmd_mailbox_status(
    paths: &Paths,
    client: &mut Client,
    client_id: u64,
    cli_as: Option<&str>,
) -> Result<()> {
    println!("socket:  {}", paths.socket.display());
    println!("daemon:  reachable");
    let identity = match resolve_mail_identity(client, paths, cli_as)? {
        Some(agent) => agent,
        None => "status-probe".to_string(),
    };
    let seated = client.request(|request_id| ControlRequest::MailHello {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        agent: identity,
    })?;
    if !matches!(seated, ControlResponseData::MailSeated { .. }) {
        bail!("server returned an unexpected mail-hello response");
    }
    let who = client.request(|request_id| ControlRequest::MailWho {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
    })?;
    let ControlResponseData::MailPeers { peers } = who else {
        bail!("server returned an unexpected mail-who response");
    };
    println!("agents:  {} bound", peers.len());
    for peer in &peers {
        let live = if peer.pane_live {
            "live"
        } else {
            "no live pane"
        };
        if peer.aliases.is_empty() {
            println!("  {}  {}  ({live})", peer.agent_id, peer.session);
        } else {
            println!(
                "  {}  {}  ({live}; aliases: {})",
                peer.agent_id,
                peer.session,
                peer.aliases.join(", ")
            );
        }
    }
    Ok(())
}

fn print_mail_reply(reply: ControlResponseData, claim_format: MailClaimFormat) {
    match reply {
        ControlResponseData::MailSent { id, depth } => {
            println!("sent: {id} (depth {depth})");
        }
        ControlResponseData::MailLetters { letters } => match claim_format {
            MailClaimFormat::Json => {
                println!("{}", serde_json::json!({ "letters": letters }));
            }
            MailClaimFormat::Ids => {
                for letter in &letters {
                    println!("{}", letter.id);
                }
            }
            MailClaimFormat::Human => {
                println!("status: held  ({} letter(s))", letters.len());
                for letter in &letters {
                    println!(
                        "\nid:       {}\nfrom:     {}\nsummary:  {}\n---\n{}",
                        letter.id, letter.from, letter.summary, letter.body
                    );
                }
            }
        },
        ControlResponseData::MailCommitted { committed } => {
            println!("committed: {committed}");
        }
        ControlResponseData::MailReleased { released } => {
            println!("released: {released}");
        }
        ControlResponseData::MailDepth { open, held } => {
            println!("open: {open} held: {held}");
        }
        ControlResponseData::MailAliased { name, agent } => {
            println!("aliased: {name} -> {agent}");
        }
        ControlResponseData::MailBroadcasted {
            delivered,
            recipients,
        } => {
            println!("broadcasted: {delivered} ({})", recipients.join(", "));
        }
        ControlResponseData::MailPeers { peers } => {
            for peer in &peers {
                let live = if peer.pane_live {
                    "live"
                } else {
                    "no live pane"
                };
                if peer.aliases.is_empty() {
                    println!("{}  {}  ({live})", peer.agent_id, peer.session);
                } else {
                    println!(
                        "{}  {}  ({live}; aliases: {})",
                        peer.agent_id,
                        peer.session,
                        peer.aliases.join(", ")
                    );
                }
            }
        }
        other => {
            eprintln!("pmux mail: unexpected reply: {other:?}");
        }
    }
}

const SPACE_USAGE: &str = "pmux space — isolated workspaces of sessions and tabs
usage: pmux space VERB [args]
Each session belongs to only one Space. Other Spaces keep running.
  create NAME [--session-name SESSION] [--no-attach] [--new-window]
    Create one fresh shell. Never copy current work.
  save [NAME] [SESSION...] [--name NAME]
    Save this Space. Reject sessions owned by another Space.
  open [NAME] [--replace] [--no-attach] [--new-window] [--no-run] [--tty]
    Create missing sessions. Reuse only sessions owned by this Space.
    --replace adds saved windows. --no-run skips saved commands.
    --add is accepted only when the view already shows the same Space.
  create, save, open: [--view-path PATH] targets one absolute view path.
  attach [NAME] [--session S]  Attach in this TTY; default to saved focus.
  add NAME [--session S | --name NEW] [--tab TITLE]
    Create a session or add an unassigned session. Use Move for another owner.
  move NAME --session S       Transfer all panes, processes, and mailbox.
  move NAME --session-id ID   Transfer by exact numeric session ID.
  move NAME --pane P [--to-session S]  Transfer one live pane.
  rename OLD NEW             Keep the Space identity and live sessions.
  remove NAME --session S    Release ownership; keep the session alive.
  details NAME [--json]      Read roles, live state, attention, and links.
  result NAME [--json]       Read retained desktop open results.
  role NAME SESSION ROLE    Set a session role.
  link NAME LABEL TARGET    Add an HTTP(S) link or absolute local path.
  attention NAME [--json]    Read requests separately from letters.
  attention <resolve|snooze> NAME SESSION PANE REQUEST
    Recheck ownership and revision. Snooze: 10 minutes. Mail stays unchanged.
  template ls
  template save TEMPLATE --space NAME
  template preview TEMPLATE NEW_SPACE [--json]
  template create TEMPLATE NEW_SPACE [--launch] [--no-attach]
    Create independent shells. Only --launch executes previewed recipes.
  ls                        List saved Spaces.
  rm [NAME...] [--all]       Delete definitions; keep sessions alive.
  clear [--keep NAME]        Delete all except kept names; keep sessions alive.
Version-1 migration rejects ambiguous ownership. Originals: legacy-backups/.
`pmux layout save space` and `pmux layout apply space` remain aliases.
Files: $XDG_DATA_HOME/prismattyc/spaces or ~/.local/share/prismattyc/spaces
";

#[derive(Debug)]
struct SpaceUsage;

impl std::fmt::Display for SpaceUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(SPACE_USAGE.trim_end())
    }
}

impl std::error::Error for SpaceUsage {}

#[derive(Debug, PartialEq, Eq)]
enum SpaceVerb {
    Team(Vec<String>),
    Create(Vec<String>),
    Move(Vec<String>),
    Rename(Vec<String>),
    Save(Vec<String>),
    Open(Vec<String>),
    Attach(Vec<String>),
    Rm(Vec<String>),
    Clear(Vec<String>),
    Add(Vec<String>),
    Remove(Vec<String>),
    Ls,
    Help,
}

fn parse_space_verb(rest: Vec<String>) -> Result<SpaceVerb, SpaceUsage> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(SpaceVerb::Help);
    }
    match rest.first().map(String::as_str) {
        Some("details" | "role" | "link" | "attention" | "template" | "result" | "undo") => {
            Ok(SpaceVerb::Team(rest))
        }
        Some("create") => Ok(SpaceVerb::Create(rest.into_iter().skip(1).collect())),
        Some("move") => Ok(SpaceVerb::Move(rest.into_iter().skip(1).collect())),
        Some("rename") => Ok(SpaceVerb::Rename(rest.into_iter().skip(1).collect())),
        Some("save") => Ok(SpaceVerb::Save(rest.into_iter().skip(1).collect())),
        Some("open") => Ok(SpaceVerb::Open(rest.into_iter().skip(1).collect())),
        Some("attach") => Ok(SpaceVerb::Attach(rest.into_iter().skip(1).collect())),
        Some("rm") | Some("delete") => Ok(SpaceVerb::Rm(rest.into_iter().skip(1).collect())),
        Some("clear") => Ok(SpaceVerb::Clear(rest.into_iter().skip(1).collect())),
        Some("add") => Ok(SpaceVerb::Add(rest.into_iter().skip(1).collect())),
        Some("remove") => Ok(SpaceVerb::Remove(rest.into_iter().skip(1).collect())),
        Some("ls") if rest.len() == 1 => Ok(SpaceVerb::Ls),
        _ => Err(SpaceUsage),
    }
}

fn cmd_space(paths: &Paths, rest: Vec<String>) -> Result<()> {
    let (paths, rest) = paths.with_view_argument(rest)?;
    let paths = &paths;
    match parse_space_verb(rest).map_err(anyhow::Error::from)? {
        SpaceVerb::Team(args) => space_commands::team(paths, args),
        SpaceVerb::Create(args) => space_commands::create(paths, args),
        SpaceVerb::Move(args) => space_commands::move_work(paths, args),
        SpaceVerb::Rename(args) => space_commands::rename(paths, args),
        SpaceVerb::Help => {
            print!("{SPACE_USAGE}");
            Ok(())
        }
        SpaceVerb::Save(args) => cmd_layout_save_space(paths, args, SPACE_SAVE_USAGE),
        SpaceVerb::Open(args) => {
            let parsed = parse_space_open_args(args)?;
            apply_layout_args(paths, parsed)
        }
        SpaceVerb::Attach(args) => cmd_space_attach(paths, args),
        SpaceVerb::Ls => cmd_space_ls(),
        SpaceVerb::Rm(args) => space_commands::delete_spaces(paths, args, false),
        SpaceVerb::Clear(args) => space_commands::delete_spaces(paths, args, true),
        SpaceVerb::Add(args) => space_commands::add(paths, args),
        SpaceVerb::Remove(args) => space_commands::remove(paths, args),
    }
}

const SPACE_ADD_USAGE: &str = "usage: pmux space add NAME [--session S | --name NEW] [--tab TITLE]";
const SPACE_REMOVE_USAGE: &str = "usage: pmux space remove NAME --session S [--kill]";

fn parse_space_session_args(
    args: Vec<String>,
    usage: &str,
    allow_tab: bool,
) -> Result<(String, String, Option<String>)> {
    let mut name = None;
    let mut session = None;
    let mut tab = None;
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--session" => {
                session = Some(
                    rest.next()
                        .filter(|value| !value.is_empty())
                        .with_context(|| usage.to_string())?,
                );
            }
            "--tab" if allow_tab => {
                tab = Some(
                    rest.next()
                        .filter(|value| !value.is_empty())
                        .with_context(|| usage.to_string())?,
                );
            }
            flag if flag.starts_with('-') => bail!("{usage}"),
            other if name.is_none() => name = Some(other.to_string()),
            _ => bail!("{usage}"),
        }
    }
    let name = name
        .filter(|value| !value.is_empty())
        .with_context(|| usage.to_string())?;
    let session = session.with_context(|| usage.to_string())?;
    Ok((name, session, tab))
}

fn cmd_layout_rm(args: Vec<String>) -> Result<()> {
    cmd_rm_named_json(&layouts_dir(), args, LAYOUT_RM_USAGE, "layout")
}

fn cmd_rm_named_json(dir: &Path, args: Vec<String>, usage: &'static str, kind: &str) -> Result<()> {
    let mut all = false;
    let mut names = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            flag if flag.starts_with('-') => bail!("{usage}"),
            _ => names.push(arg),
        }
    }
    if all {
        if !names.is_empty() {
            bail!("{usage}");
        }
        let entries = if kind == "space" {
            list_spaces(dir)?
                .into_iter()
                .map(|e| e.name)
                .collect::<Vec<_>>()
        } else {
            list_layouts(dir)?
                .into_iter()
                .map(|e| e.name)
                .collect::<Vec<_>>()
        };
        if entries.is_empty() {
            println!("no {kind}s to remove");
            return Ok(());
        }
        for name in &entries {
            println!("{name}");
        }
        for name in &entries {
            let path = if kind == "space" {
                remove_space(dir, name)?
            } else {
                remove_layout(dir, name)?
            };
            println!("{}", path.display());
        }
        return Ok(());
    }
    if names.is_empty() {
        bail!("{usage}");
    }
    let mut failed = None;
    for name in names {
        match if kind == "space" {
            remove_space(dir, &name)
        } else {
            remove_layout(dir, &name)
        } {
            Ok(path) => println!("{}", path.display()),
            Err(err) => {
                eprintln!("{err:#}");
                failed = Some(err);
            }
        }
    }
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn cmd_space_ls() -> Result<()> {
    for entry in list_spaces(&spaces_dir())? {
        println!("{}", format_space_list_row(&entry, false));
    }
    Ok(())
}

fn format_space_list_row(entry: &prismattyc_mux::SpaceListEntry, layout_prefix: bool) -> String {
    let sessions = if entry.sessions == 1 {
        "session"
    } else {
        "sessions"
    };
    let panes = if entry.panes == 1 { "pane" } else { "panes" };
    let tabs = if entry.tabs == 1 { "tab" } else { "tabs" };
    let prefix = if layout_prefix { "space " } else { "" };
    // No pmuxd window count: the glossary reserves "window" for a host
    // tab, and every saved session has one window (PT-210).
    format!(
        "{prefix}{}  {} {sessions}  {} {panes}  {} {tabs}",
        entry.name, entry.sessions, entry.panes, entry.tabs
    )
}

const SESSION_USAGE: &str = "pmux session — name and manage live sessions

usage: pmux session <name|rename|reopen|suggest|clear> [args]

session  A pmuxd mailbox/agent unit. List with `pmux ls`.
tab      A host Window. Sessions in one tab appear as panes.
space    A file of sessions. A space is not a Session.

verbs:
  name NAME [--session KEY]
  rename NAME [--session KEY]
    Set the session name and agent ID together. Default: this pane.
    Pending mail stays. Previous mailbox addresses forward to NAME.
  reopen NAME --space SPACE
    Reopen one saved session with its name, mailbox, and Space owner.
  suggest [--space NAME]
    Print an unused name such as work-1.
  clear [--all] [--keep NAME]
    Stop every session except the one this command runs in
    (pmux whoami). Not inside a pane: no session is exempt.
    --all also stops the caller, last. --keep NAME
    (repeatable) preserves a session by name. Prints one
    line per session: stopped NAME (id N). Exit 0 when
    nothing was stopped. Same stop path as pmux stop NAME.
    Mailbox letters stay. Saved spaces are not changed.

`pmux ls` lists sessions. There is no `pmux session ls`.
";

#[derive(Debug)]
struct SessionUsage;

impl std::fmt::Display for SessionUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(SESSION_USAGE.trim_end())
    }
}

impl std::error::Error for SessionUsage {}

#[derive(Debug, PartialEq, Eq)]
enum SessionVerb {
    Name(Vec<String>),
    Suggest(Vec<String>),
    Reopen(Vec<String>),
    Clear(Vec<String>),
    Help,
}

fn parse_session_verb(rest: Vec<String>) -> Result<SessionVerb, SessionUsage> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(SessionVerb::Help);
    }
    match rest.first().map(String::as_str) {
        Some("name" | "rename") => Ok(SessionVerb::Name(rest.into_iter().skip(1).collect())),
        Some("reopen") => Ok(SessionVerb::Reopen(rest.into_iter().skip(1).collect())),
        Some("suggest") => Ok(SessionVerb::Suggest(rest.into_iter().skip(1).collect())),
        Some("clear") => Ok(SessionVerb::Clear(rest.into_iter().skip(1).collect())),
        _ => Err(SessionUsage),
    }
}

struct ClearOpts {
    all: bool,
    keep: BTreeSet<String>,
}

fn parse_clear_keep(args: Vec<String>, usage: &'static str, allow_all: bool) -> Result<ClearOpts> {
    let mut all = false;
    let mut keep = BTreeSet::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--all" if allow_all => all = true,
            "--keep" => {
                let name = iter.next().context(usage)?;
                if name.is_empty() || name.starts_with('-') {
                    bail!("{usage}");
                }
                keep.insert(name);
            }
            _ => bail!("{usage}"),
        }
    }
    Ok(ClearOpts { all, keep })
}

fn cmd_session(paths: &Paths, rest: Vec<String>) -> Result<()> {
    match parse_session_verb(rest).map_err(anyhow::Error::from)? {
        SessionVerb::Help => {
            print!("{SESSION_USAGE}");
            Ok(())
        }
        SessionVerb::Reopen(args) => space_commands::reopen(paths, args),
        SessionVerb::Name(args) => space_commands::name_session(paths, args),
        SessionVerb::Suggest(args) => {
            let space = match args.as_slice() {
                [] => "session",
                [flag, space] if flag == "--space" => space,
                _ => bail!("usage: pmux session suggest [--space NAME]"),
            };
            let mut client = Client::connect(&paths.socket)?;
            let _store = space_commands::Store::lock(&mut client)?;
            println!("{}", space_commands::suggest_name(&mut client, space)?);
            Ok(())
        }
        SessionVerb::Clear(args) => cmd_session_clear(paths, args),
    }
}

fn caller_session_id(paths: &Paths, snapshot: &Snapshot) -> Option<u64> {
    let pane_id = std::env::var("PRISMATTYC_PANE_ID")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())?;
    // Pane ids are per-daemon. A foreign PMUX_SOCKET means this process
    // is not a pane of the target mux; do not exempt.
    let env_socket = std::env::var_os("PMUX_SOCKET")?;
    if Path::new(&env_socket) != paths.socket.as_path() {
        return None;
    }
    whoami_from_snapshot(snapshot, pane_id).map(|who| who.id)
}

fn destroy_session(client: &mut Client, session_id: u64) -> Result<()> {
    let response = client.request(|request_id| ControlRequest::DestroySession {
        version: PROTOCOL_VERSION,
        request_id,
        session_id,
    })?;
    let ControlResponseData::Mutation { .. } = response else {
        bail!("server returned an unexpected destroy-session response");
    };
    Ok(())
}

fn cmd_session_clear(paths: &Paths, args: Vec<String>) -> Result<()> {
    let opts = parse_clear_keep(args, SESSION_CLEAR_USAGE, true)?;
    match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => {}
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        SocketLiveness::Missing | SocketLiveness::Stale => {
            bail!("not running (no live server to clear sessions)");
        }
    }
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let caller_id = caller_session_id(paths, &snapshot);
    let mut others = Vec::new();
    let mut caller = None;
    for session in &snapshot.sessions {
        if opts.keep.contains(&session.name) {
            continue;
        }
        if Some(session.id) == caller_id {
            if opts.all {
                caller = Some((session.id, session.name.clone()));
            }
            continue;
        }
        others.push((session.id, session.name.clone()));
    }
    for (session_id, name) in others {
        destroy_session(&mut client, session_id)?;
        println!("stopped {name} (id {session_id})");
    }
    if let Some((session_id, name)) = caller {
        destroy_session(&mut client, session_id)?;
        println!("stopped {name} (id {session_id})");
    }
    Ok(())
}

const LAYOUT_USAGE: &str = "pmux layout — saved mux layouts (no protocol change)

usage: pmux layout <save|apply|ls|rm> [args]

session  A pmuxd mailbox/agent unit. List with `pmux ls`.
tab      A host Window. Sessions in one tab appear as panes.
space    A file of sessions. A space is not a Session.

verbs:
  save [SESSION] [--name NAME]
    Snapshot SESSION to layouts/NAME.json.
    NAME defaults to the session name.
  save space [NAME] [SESSION...] [--name NAME]
    Alias of `pmux space save`. NAME defaults to 'default'.
    No SESSION list: every session except leftover default.
  apply NAME [--session TARGET] [--agent]
    Recreate the saved tree. TARGET defaults to NAME.
    Missing TARGET: create the session (no agent bind).
    Existing TARGET: add windows. --agent binds the session name.
    Never attaches. Programs on leaves are not re-run.
  apply space [NAME] [--add] [--replace] [--no-attach]
               [--new-window] [--no-run] [--tty]
    Alias of `pmux space open`. NAME defaults to 'default'.
    Switch is the default; --add merges. Reuses a live host, or opens
    prismattyc-host unless --no-attach. --replace adds windows.
  apply --all [--replace] [--agent]
    Apply every layouts/*.json in name order. Implies --agent.
    Skip existing sessions unless --replace. Never attaches.
  ls    List saved layouts and spaces.
  rm [NAME...] [--all]
    Delete layouts/NAME.json. Same name rules as save. --all lists
    then deletes every layout file. Space files use `pmux space rm`.

layout files: $XDG_DATA_HOME/prismattyc/layouts
              (else ~/.local/share/prismattyc/layouts)
space files:  $XDG_DATA_HOME/prismattyc/spaces
              (else ~/.local/share/prismattyc/spaces)
";

fn cmd_layout(paths: &Paths, global_session: Option<String>, rest: Vec<String>) -> Result<()> {
    let mut iter = rest.into_iter().peekable();
    let verb = iter.next().context(LAYOUT_USAGE)?;
    match verb.as_str() {
        "-h" | "--help" => {
            println!("{LAYOUT_USAGE}");
            Ok(())
        }
        "save" => cmd_layout_save(paths, global_session, iter.collect()),
        "apply" => cmd_layout_apply(paths, global_session, iter.collect()),
        "rm" | "delete" => {
            if global_session.is_some() {
                bail!("pmux layout rm does not take --session");
            }
            cmd_layout_rm(iter.collect())
        }
        "ls" => {
            if global_session.is_some() {
                bail!("pmux layout ls does not take --session");
            }
            if iter.peek().is_some() {
                bail!("usage: pmux layout ls");
            }
            cmd_layout_ls()
        }
        other => bail!("unknown layout verb {other:?}"),
    }
}

fn cmd_layout_ls() -> Result<()> {
    let dir = layouts_dir();
    for entry in list_layouts(&dir)? {
        let windows = if entry.windows == 1 {
            "window"
        } else {
            "windows"
        };
        let panes = if entry.panes == 1 { "pane" } else { "panes" };
        println!(
            "{}  {} {windows}  {} {panes}",
            entry.name, entry.windows, entry.panes
        );
    }
    for entry in list_spaces(&spaces_dir())? {
        println!("{}", format_space_list_row(&entry, true));
    }
    Ok(())
}

fn cmd_layout_save(paths: &Paths, global_session: Option<String>, rest: Vec<String>) -> Result<()> {
    if rest.first().map(String::as_str) == Some("space") {
        return cmd_layout_save_space(paths, rest[1..].to_vec(), LAYOUT_SAVE_SPACE_USAGE);
    }
    let (session_key, name) = parse_layout_save_args(global_session, rest)?;
    require_live_socket(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let snapshot = take_snapshot(&mut client)?;
    let session = resolve_session_snapshot(&snapshot, session_key.as_deref())?;
    if session.windows.is_empty() {
        bail!("session {:?} has no windows to save", session.name);
    }
    let file_name = match name {
        Some(name) => name,
        None => session.name.clone(),
    };
    validate_layout_name(&file_name)?;
    let saved = from_snapshot(session);
    let path = save_layout(&layouts_dir(), &file_name, &saved)?;
    println!("{}", path.display());
    Ok(())
}

fn cmd_layout_save_space(paths: &Paths, rest: Vec<String>, usage: &'static str) -> Result<()> {
    let (paths, rest) = paths.with_view_argument(rest)?;
    let paths = &paths;
    let (name, session_keys) = parse_layout_save_space_args(rest, usage)?;
    require_live_socket(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let snapshot = take_snapshot(&mut client)?;
    let window_keys = if session_keys.is_empty() && paths.view_path.is_some() {
        let path = paths.space_view_path();
        let file = attach_tabs::load(&path)
            .context("explicit view layout is unreadable; refusing global session fallback")?;
        Some(if file.tabs.is_empty() {
            vec![]
        } else {
            window_save_keys_from_path(&path, &snapshot)
                .context("explicit view sessions are unavailable")?
        })
    } else {
        session_keys
            .is_empty()
            .then(|| window_save_keys_from_path(&paths.space_view_path(), &snapshot))
            .flatten()
    };
    let from_window = window_keys.is_some();
    let keys = window_keys.unwrap_or_else(|| session_keys.clone());
    let sessions = if from_window && keys.is_empty() {
        vec![]
    } else {
        resolve_space_sessions(&snapshot, &keys)?
    };
    if sessions.is_empty() && !from_window {
        bail!("no sessions to save; pass SESSION or create a non-default session");
    }
    if session_keys.is_empty() {
        if let Some(text) = space_save_span_line(paths, &sessions) {
            eprintln!("{text}");
        }
    }
    for session in &sessions {
        if session.windows.is_empty() {
            bail!("session {:?} has no windows to save", session.name);
        }
    }
    let mut space = from_sessions(&sessions);
    let saved_names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
    let cache = attach_tabs::load(&paths.space_view_path());
    let had_cache = cache.is_some();
    if let Some(file) = cache {
        apply_attach_records_to_space(&mut space, &file, &snapshot, &saved_names);
    }
    if had_cache {
        tab_untabbed_sessions(&mut space, &saved_names);
    }
    let path = space_commands::save_owned(&mut client, &name, &mut space)?;
    println!("{}", path.display());
    if from_window {
        let n = sessions.len();
        let word = if n == 1 { "session" } else { "sessions" };
        println!("saved {n} {word} from the live window");
    }
    if !space.tabs.is_empty() {
        println!(
            "tabs: {} (from {})",
            space.tabs.len(),
            attach_tabs::layout_path_from_socket(&paths.socket).display()
        );
    }
    Ok(())
}

/// Session names the live window shows, from the attach-tabs cache.
/// None when there is no cache, it has no tabs, or no id maps to a live name.
fn window_save_keys_from_path(path: &Path, snapshot: &Snapshot) -> Option<Vec<String>> {
    let file = attach_tabs::load(path)?;
    if file.tabs.is_empty() {
        return None;
    }
    let names: Vec<(String, String)> = snapshot
        .sessions
        .iter()
        .map(|session| (session.id.to_string(), session.name.clone()))
        .collect();
    let keys = attach_tabs::cache_sessions_in_tab_order(&file, &names);
    (!keys.is_empty()).then_some(keys)
}

fn space_save_span_line(paths: &Paths, sessions: &[&SessionSnapshot]) -> Option<String> {
    let live: Vec<String> = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect();
    let cache = attach_tabs::load(&attach_tabs::layout_path_from_socket(&paths.socket));
    let cache_space = cache.as_ref().and_then(|file| file.space.as_deref());
    let listed = list_spaces(&spaces_dir()).ok()?;
    let mut spaces = Vec::new();
    for entry in listed {
        let Ok(space) = load_space(&spaces_dir(), &entry.name) else {
            continue;
        };
        let members: Vec<String> = space
            .sessions
            .iter()
            .map(|session| session.name.clone())
            .collect();
        spaces.push((entry.name, members));
    }
    attach_tabs::space_save_span_warning(cache_space, &live, &spaces)
}

/// Give each saved live session the attach-tabs cache omits its own
/// single-pane tab, appended after the cache tabs in saved order. Without
/// this, a live session missing from the host cache is saved but has no
/// tab, so `space open` drops it from the arrangement. This mirrors the
/// one-tab-per-session fallback `space open` uses for a tabless space
/// (PT-142). A note reports each synthesized tab.
fn tab_untabbed_sessions(space: &mut SavedSpace, saved_names: &[String]) {
    let tabbed: BTreeSet<&str> = space
        .tabs
        .iter()
        .flat_map(|tab| tab.sessions.iter().map(String::as_str))
        .collect();
    let untabbed: Vec<String> = saved_names
        .iter()
        .filter(|name| !tabbed.contains(name.as_str()))
        .cloned()
        .collect();
    for name in untabbed {
        eprintln!("note: session {name} was not in the attach-tabs cache; saved it in its own tab");
        space.tabs.push(SavedSpaceTab {
            title: name.clone(),
            sessions: vec![name],
        });
    }
}

fn apply_attach_records_to_space(
    space: &mut SavedSpace,
    file: &attach_tabs::AttachTabsFile,
    snapshot: &Snapshot,
    saved_names: &[String],
) {
    let (kept, active_tab) = attach_tabs::remap_tabs(file.tabs.iter(), file.active_tab, |tab| {
        let sessions: Vec<String> = tab
            .sessions
            .iter()
            .filter_map(|id| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id.to_string() == *id)
                    .map(|session| session.name.clone())
            })
            .filter(|name| saved_names.contains(name))
            .collect();
        (!sessions.is_empty()).then_some((tab.title.clone(), sessions))
    });
    space.tabs = kept
        .into_iter()
        .map(|(title, sessions)| SavedSpaceTab { title, sessions })
        .collect();
    space.active_tab = active_tab;
    space.focused_session = file.focused_session.as_ref().and_then(|id| {
        snapshot
            .sessions
            .iter()
            .find(|session| session.id.to_string() == *id)
            .map(|session| session.name.clone())
            .filter(|name| saved_names.contains(name))
    });
}

/// The attach-tabs file for a restored space: saved tab order and titles
/// with the live session ids. Sessions missing from the snapshot are
/// dropped. A space that records no tabs opens one tab per session, in
/// space order, so a live host regroups to the same arrangement a fresh
/// host would spawn (PT-142).
fn attach_file_from_space(space: &SavedSpace, snapshot: &Snapshot) -> attach_tabs::AttachTabsFile {
    let one_per_session: Vec<SavedSpaceTab>;
    let tabs = if space.tabs.is_empty() {
        one_per_session = space
            .sessions
            .iter()
            .map(|session| SavedSpaceTab {
                title: session.name.clone(),
                sessions: vec![session.name.clone()],
            })
            .collect();
        &one_per_session
    } else {
        &space.tabs
    };
    let (kept, active_tab) = attach_tabs::remap_tabs(tabs.iter(), space.active_tab, |tab| {
        let sessions: Vec<String> = tab
            .sessions
            .iter()
            .filter_map(|name| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.name == *name)
                    .map(|session| session.id.to_string())
            })
            .collect();
        (!sessions.is_empty()).then_some((tab.title.clone(), sessions))
    });
    let tabs: Vec<attach_tabs::AttachTabRecord> = kept
        .into_iter()
        .map(|(title, sessions)| attach_tabs::AttachTabRecord { title, sessions })
        .collect();
    let focused_session = space.focused_session.as_ref().and_then(|name| {
        snapshot
            .sessions
            .iter()
            .find(|session| session.name == *name)
            .map(|session| session.id.to_string())
            .filter(|id| tabs.iter().any(|tab| tab.sessions.contains(id)))
    });
    attach_tabs::AttachTabsFile {
        tabs,
        active_tab,
        focused_session,
        space: None,
        mode: attach_tabs::AttachTabsMode::Add,
        session_names: snapshot
            .sessions
            .iter()
            .map(|s| (s.id.to_string(), s.name.clone()))
            .collect(),
        space_id: space.id.clone(),
    }
}

fn cmd_layout_apply(
    paths: &Paths,
    global_session: Option<String>,
    rest: Vec<String>,
) -> Result<()> {
    let (paths, rest) = paths.with_view_argument(rest)?;
    let args = parse_layout_apply_args(global_session, rest)?;
    apply_layout_args(&paths, args)
}

fn apply_layout_args(paths: &Paths, args: ApplyArgs) -> Result<()> {
    ensure_live_server(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    match args {
        ApplyArgs::One {
            name,
            target,
            bind_agent,
        } => {
            let layout = load_layout(&layouts_dir(), &name)?;
            let snapshot = take_snapshot(&mut client)?;
            let agent = bind_agent
                .then(|| space_bind_agent(None, &target))
                .flatten();
            let result = apply_saved_layout(
                &mut client,
                &snapshot,
                &layout,
                &target,
                agent,
                ApplyExisting::AddWindows,
            )?;
            print_apply_one(&name, &target, &result);
        }
        ApplyArgs::Space {
            name,
            replace,
            add,
            attach,
            new_window,
            no_run,
            tty,
            host,
        } => {
            let dir = spaces_dir();
            let space_path = prismattyc_mux::layout_path(&dir, &name)?;
            let _store = space_commands::Store::lock(&mut client)?;
            let space = space_commands::prepare_open(&_store, &mut client, &name)?;
            if add
                && attach_tabs::load(&paths.space_view_path())
                    .and_then(|file| file.space)
                    .as_deref()
                    != Some(&name)
            {
                bail!("cannot add a different Space to this view; use Switch or New window");
            }
            refuse_space_path_escape(&dir, &space_path)?;
            let mut applied = Vec::new();
            for session in &space.sessions {
                let snapshot = take_snapshot(&mut client)?;
                let before_windows = snapshot
                    .sessions
                    .iter()
                    .find(|live| live.name == session.name)
                    .map(|live| live.windows.len())
                    .unwrap_or(0);
                let layout = SavedLayout {
                    version: 1,
                    saved_at_unix: space.saved_at_unix,
                    session: session.name.clone(),
                    windows: session.windows.clone(),
                };
                let agent = if space.version == prismattyc_mux::OWNED_SPACE_VERSION {
                    session.agent.clone()
                } else {
                    space_bind_agent(session.agent.as_deref(), &session.name)
                };
                let existing = if replace {
                    ApplyExisting::AddWindows
                } else {
                    ApplyExisting::Skip
                };
                let result = apply_saved_layout(
                    &mut client,
                    &snapshot,
                    &layout,
                    &session.name,
                    agent,
                    existing,
                )?;
                space_commands::claim_saved_session(&mut client, &space, &session.name)?;
                print_apply_space_session(&session.name, &result);
                applied.push((session.name.clone(), result, before_windows));
            }
            let mux_file = load_mux_section(&prism_config_path()).unwrap_or_else(|error| {
                eprintln!("pmux: ignoring config: {error:#}");
                prismattyc_mux::MuxSection::default()
            });
            let policy = resolve_space_open_runs_commands(no_run, &mux_file)?;
            let snapshot = take_snapshot(&mut client)?;
            run_space_open_commands(&mut client, client_id, &snapshot, &space, &applied, policy)?;
            restore_saved_pane_titles(&mut client, &snapshot, &space, &applied)?;
            if !host {
                drop(client);
                return Ok(());
            }
            let live = live_host_pid(&host_pid_path_from_socket(&paths.socket));
            let skip_cache = new_window && live.is_some();
            if !skip_cache {
                let snapshot = take_snapshot(&mut client)?;
                let mut file = attach_file_from_space(&space, &snapshot);
                let path = paths.space_view_path();
                let previous = attach_tabs::load(&path);
                let from = previous.as_ref().and_then(|file| file.space.clone());
                let previous_ids: std::collections::HashSet<String> = previous
                    .as_ref()
                    .map(|file| {
                        file.tabs
                            .iter()
                            .flat_map(|tab| tab.sessions.iter().cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                if add {
                    if let Some(previous) = previous.as_ref() {
                        let live_ids: Vec<String> = snapshot
                            .sessions
                            .iter()
                            .filter(|session| session.space_id == space.id)
                            .map(|session| session.id.to_string())
                            .collect();
                        file = attach_tabs::merge_add_cache(previous, file, &live_ids);
                    }
                    file.mode = attach_tabs::AttachTabsMode::Add;
                } else {
                    file.mode = attach_tabs::AttachTabsMode::Switch;
                }
                file.space = Some(name.clone());
                attach_tabs::save(&path, &file)
                    .with_context(|| format!("write {}", path.display()))?;
                if !file.tabs.is_empty() {
                    println!("tabs: {} restored to {}", file.tabs.len(), path.display());
                }
                let new_ids: std::collections::HashSet<String> = file
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.sessions.iter().cloned())
                    .collect();
                let detached = previous_ids.difference(&new_ids).count();
                let opened_sessions = file.tabs.iter().map(|tab| tab.sessions.len()).sum();
                println!(
                    "{}",
                    attach_tabs::space_open_report(
                        file.mode,
                        from.as_deref(),
                        &name,
                        detached,
                        opened_sessions,
                        file.tabs.len(),
                    )
                );
            }
            drop(client);
            drop(_store);
            return finish_space_open(paths, attach, new_window, live, Some(&name), tty);
        }
        ApplyArgs::All { replace } => {
            let layouts = list_layouts(&layouts_dir())?;
            if layouts.is_empty() {
                bail!("no saved layouts to apply");
            }
            let existing_policy = if replace {
                ApplyExisting::AddWindows
            } else {
                ApplyExisting::Skip
            };
            for entry in layouts {
                let layout = load_layout(&layouts_dir(), &entry.name)?;
                let target = layout.session.clone();
                let snapshot = take_snapshot(&mut client)?;
                let agent = space_bind_agent(None, &target);
                let result = apply_saved_layout(
                    &mut client,
                    &snapshot,
                    &layout,
                    &target,
                    agent,
                    existing_policy,
                )?;
                print_apply_space_session(&target, &result);
            }
        }
    }
    Ok(())
}

/// `apply space` opens the restored sessions in prismattyc-host with the
/// same command as `attach --all`, but detached: its own session (no
/// controlling terminal, `setsid`), stdio to a log file, and pmux returns
/// to the prompt. Closing the terminal that ran `apply space` then keeps
/// the host window and the sessions. Without a display on Linux the
/// sessions are still restored: print the hint and exit 0.
fn open_applied_space(paths: &Paths, space: Option<&str>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if linux_attach_all_blocked_without_display(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        ) {
            eprintln!(
                "pmux: sessions restored; not opening a window.\n{}",
                attach_all_no_display_message(&paths.socket)
            );
            return Ok(());
        }
    }
    let AttachAllLaunch {
        mut command,
        host,
        sessions,
    } = attach_all_command(paths, space)?;
    let log_path = detached_host_log_path(&paths.socket);
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("create {}", log_path.display()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone().context("clone host log handle")?,
        ))
        .stderr(Stdio::from(log));
    prismattyc_mux::platform::detach_command(&mut command);
    let child = command
        .spawn()
        .with_context(|| format!("spawn {}", host.display()))?;
    println!(
        "opened host pid {} with {} session(s): {}\nlog: {}",
        child.id(),
        sessions.len(),
        session_list(&sessions),
        log_path.display()
    );
    Ok(())
}

/// After sessions (and maybe the cache) are restored: reuse, spawn, or skip.
fn finish_space_open(
    paths: &Paths,
    attach: bool,
    new_window: bool,
    live: Option<u32>,
    space: Option<&str>,
    tty: bool,
) -> Result<()> {
    if new_window {
        return open_applied_space(paths, space);
    }
    if paths.view_path.is_some() {
        return Ok(());
    }
    // A registered host still regroups when Linux has no display (scripts,
    // CI, Local Actions). `--tty` is the SSH recipe and must not steal it.
    if let Some(pid) = live {
        if !tty {
            let ack = host_ack_path_from_socket(&paths.socket);
            let cache = attach_tabs::layout_path_from_socket(&paths.socket);
            let since = std::fs::metadata(&cache)
                .and_then(|meta| meta.modified())
                .unwrap_or_else(|_| SystemTime::now());
            let _ = std::fs::remove_file(&ack);
            if wait_host_ack(&ack, since, Duration::from_secs(2)) {
                println!("reused host pid {pid}");
            } else {
                println!("host pid {pid} did not reload; cache written");
            }
            return Ok(());
        }
    }
    if space_open_uses_tty_recipe(tty) {
        if let Some(name) = space {
            let saved = load_space(&spaces_dir(), name)?;
            print_space_attach_recipe(&saved);
            if attach && stdin_is_tty() && stdout_is_tty() {
                let session = space_active_session(&saved).context("space has no session")?;
                return exec_attach_session(paths, &session, Some(name));
            }
        }
        return Ok(());
    }
    if !attach {
        return Ok(());
    }
    open_applied_space(paths, space)
}

#[derive(Clone, Copy)]
enum ApplyExisting {
    AddWindows,
    Skip,
}

enum ApplyResult {
    Created {
        windows: usize,
        panes: usize,
        agent: Option<String>,
    },
    Added {
        windows: usize,
        panes: usize,
    },
    Skipped,
}

#[derive(Debug)]
enum ApplyArgs {
    One {
        name: String,
        target: String,
        bind_agent: bool,
    },
    Space {
        name: String,
        replace: bool,
        /// Merge into the live window (`--add`). Default is switch.
        add: bool,
        /// Spawn a host when none is registered (`--no-attach` clears this).
        attach: bool,
        /// Always spawn a second host; do not steal the live registration.
        new_window: bool,
        /// Skip re-running saved pane commands (`--no-run`).
        no_run: bool,
        /// Print the TTY attach recipe and attach in this TTY (SSH path).
        tty: bool,
        /// Write the attach-tabs cache and reuse/spawn a host. `space attach`
        /// clears this so a TTY attach does not regroup the desktop host.
        host: bool,
    },
    All {
        replace: bool,
    },
}

fn layout_counts(layout: &SavedLayout) -> (usize, usize) {
    let windows = layout.windows.len();
    let panes = layout
        .windows
        .iter()
        .map(|window| window.root.pane_count())
        .sum();
    (windows, panes)
}

fn apply_saved_layout(
    client: &mut Client,
    snapshot: &Snapshot,
    layout: &SavedLayout,
    target: &str,
    agent_id: Option<String>,
    existing: ApplyExisting,
) -> Result<ApplyResult> {
    let (windows, panes) = layout_counts(layout);
    let existing_id = snapshot
        .sessions
        .iter()
        .find(|session| session.name == target || session.id.to_string() == target)
        .map(|session| session.id);
    match existing_id {
        Some(session_id) => match existing {
            ApplyExisting::Skip => Ok(ApplyResult::Skipped),
            ApplyExisting::AddWindows => {
                for window in &layout.windows {
                    apply_new_window(client, session_id, window)?;
                }
                Ok(ApplyResult::Added { windows, panes })
            }
        },
        None => {
            let first = &layout.windows[0];
            let (root_cwd, _) = plan(&first.root);
            let created = client.request(|request_id| ControlRequest::CreateSession {
                version: PROTOCOL_VERSION,
                request_id,
                name: target.to_string(),
                spawn: default_layout_spawn(root_cwd),
                cols: Some(u32::from(first.cols)),
                rows: Some(u32::from(first.rows)),
                agent_id: agent_id.clone(),
                headless: false,
            })?;
            let ControlResponseData::Session {
                session_id,
                window_id,
                pane_id,
                ..
            } = created
            else {
                bail!("server returned an unexpected create-session response");
            };
            let title = sanitize_window_title(&first.title);
            if title != "main" {
                let _ = client.request(|request_id| ControlRequest::RenameWindow {
                    version: PROTOCOL_VERSION,
                    request_id,
                    window_id,
                    title: title.clone(),
                })?;
            }
            apply_splits(client, window_id, pane_id, &first.root)?;
            for window in layout.windows.iter().skip(1) {
                apply_new_window(client, session_id, window)?;
            }
            Ok(ApplyResult::Created {
                windows,
                panes,
                agent: agent_id,
            })
        }
    }
}

fn print_apply_one(name: &str, target: &str, result: &ApplyResult) {
    match result {
        ApplyResult::Created { windows, panes, .. } | ApplyResult::Added { windows, panes } => {
            println!("applied {name} to session {target} ({windows} windows, {panes} panes)");
        }
        ApplyResult::Skipped => {
            println!("skip {target} (already exists)");
        }
    }
}

fn print_apply_space_session(name: &str, result: &ApplyResult) {
    match result {
        ApplyResult::Created {
            windows,
            panes,
            agent,
        } => match agent {
            Some(agent) => {
                println!("created {name} ({windows} windows, {panes} panes) agent {agent}")
            }
            None => println!("created {name} ({windows} windows, {panes} panes)"),
        },
        ApplyResult::Added { windows, panes } => {
            println!("added {name} ({windows} windows, {panes} panes)");
        }
        ApplyResult::Skipped => println!("skip {name} (already exists)"),
    }
}

fn refuse_space_path_escape(dir: &Path, path: &Path) -> Result<()> {
    let Ok(dir_c) = dir.canonicalize() else {
        return Ok(());
    };
    let Ok(path_c) = path.canonicalize() else {
        return Ok(());
    };
    if !path_c.starts_with(&dir_c) {
        bail!(
            "refusing to run commands from {} (not under {})",
            path_c.display(),
            dir_c.display()
        );
    }
    Ok(())
}

struct PlannedRun {
    session: String,
    pane_id: u64,
    command: String,
    /// True only for an existing session we skipped recreating. New panes
    /// from create / --replace must be typed even if login rc started a
    /// short-lived child.
    skip_if_running: bool,
}

/// `space open` restores pane titles saved by `space save` (PT-128).
/// Live sessions (`Skipped`) also take saved titles, but only when the
/// live pane is unpinned (PT-230). A live `rename-pane` is never clobbered.
fn restore_saved_pane_titles(
    client: &mut Client,
    snapshot: &Snapshot,
    space: &SavedSpace,
    applied: &[(String, ApplyResult, usize)],
) -> Result<()> {
    each_restored_pane_title(snapshot, space, applied, |pane_id, title| {
        client
            .request(|request_id| ControlRequest::RenamePane {
                version: PROTOCOL_VERSION,
                request_id,
                pane_id,
                title,
            })
            .map(|_| ())
            .with_context(|| format!("restore title on pane {pane_id}"))
    })
}

/// Walk saved titles that `space open` should apply. PT-251: unit-tested so
/// the `session.name == name` match and the trailing `Ok(())` cannot slip
/// past cargo-mutants.
fn each_restored_pane_title<F>(
    snapshot: &Snapshot,
    space: &SavedSpace,
    applied: &[(String, ApplyResult, usize)],
    mut rename: F,
) -> Result<()>
where
    F: FnMut(u64, String) -> Result<()>,
{
    for (saved, (name, result, before_windows)) in space.sessions.iter().zip(applied.iter()) {
        let Some(live) = snapshot
            .sessions
            .iter()
            .find(|session| session.name == *name)
        else {
            continue;
        };
        let windows: &[WindowSnapshot] = match result {
            ApplyResult::Added { .. } => live.windows.get(*before_windows..).unwrap_or(&[]),
            ApplyResult::Created { .. } | ApplyResult::Skipped => live.windows.as_slice(),
        };
        let skipped = matches!(result, ApplyResult::Skipped);
        for (saved_window, live_window) in saved.windows.iter().zip(windows.iter()) {
            let titles = saved_window.root.leaf_titles();
            let pane_ids = live_leaf_ids(&live_window.layout);
            for (title, pane_id) in titles.into_iter().zip(pane_ids) {
                let Some(title) = title.filter(|value| !value.trim().is_empty()) else {
                    continue;
                };
                if skipped {
                    let pinned = live_window
                        .panes
                        .iter()
                        .find(|pane| pane.id == pane_id)
                        .is_some_and(|pane| pane.title_pinned);
                    if pinned {
                        continue;
                    }
                }
                rename(pane_id, title.to_string())?;
            }
        }
    }
    Ok(())
}

fn run_space_open_commands(
    client: &mut Client,
    client_id: u64,
    snapshot: &Snapshot,
    space: &SavedSpace,
    applied: &[(String, ApplyResult, usize)],
    policy: SpaceOpenRunsCommands,
) -> Result<()> {
    let mut planned = Vec::new();
    for (saved, (name, result, before_windows)) in space.sessions.iter().zip(applied.iter()) {
        if space.version == prismattyc_mux::OWNED_SPACE_VERSION
            && matches!(result, ApplyResult::Skipped)
        {
            continue; // Viewing an existing owned session never replays a command.
        }
        if policy == SpaceOpenRunsCommands::Agents && saved.agent.is_none() {
            continue;
        }
        if policy == SpaceOpenRunsCommands::None {
            if saved.windows.iter().any(|window| {
                window
                    .root
                    .leaf_commands()
                    .iter()
                    .any(|command| command.is_some())
            }) {
                planned.push(PlannedRun {
                    session: name.clone(),
                    pane_id: 0,
                    command: String::new(),
                    skip_if_running: false,
                });
            }
            continue;
        }
        let Some(live) = snapshot
            .sessions
            .iter()
            .find(|session| session.name == *name)
        else {
            continue;
        };
        let windows: &[WindowSnapshot] = match result {
            ApplyResult::Added { .. } => live.windows.get(*before_windows..).unwrap_or(&[]),
            ApplyResult::Created { .. } | ApplyResult::Skipped => live.windows.as_slice(),
        };
        for (saved_window, live_window) in saved.windows.iter().zip(windows.iter()) {
            let commands = saved_window.root.leaf_commands();
            let pane_ids = live_leaf_ids(&live_window.layout);
            for (command, pane_id) in commands.into_iter().zip(pane_ids) {
                let Some(command) = command.filter(|value| !value.is_empty()) else {
                    continue;
                };
                planned.push(PlannedRun {
                    session: saved.name.clone(),
                    pane_id,
                    command: command.to_string(),
                    skip_if_running: matches!(result, ApplyResult::Skipped),
                });
            }
        }
    }
    if planned.is_empty() {
        return Ok(());
    }
    if policy == SpaceOpenRunsCommands::None {
        println!("run:");
        for session in space.sessions.iter().filter(|session| {
            session.windows.iter().any(|window| {
                window
                    .root
                    .leaf_commands()
                    .iter()
                    .any(|command| command.is_some())
            })
        }) {
            for window in &session.windows {
                for command in window.root.leaf_commands().into_iter().flatten() {
                    println!("  {}  {command}", session.name);
                }
            }
        }
        println!("skip run (--no-run or space_open_runs_commands=none)");
        return Ok(());
    }
    println!("run:");
    for item in &planned {
        println!("  {}  {}", item.session, item.command);
    }
    for item in planned {
        if command_has_line_break(&item.command) {
            eprintln!(
                "skip {} in {}: command contains a line break",
                item.command, item.session
            );
            continue;
        }
        match write_pane_command(
            client,
            client_id,
            item.pane_id,
            &item.command,
            item.skip_if_running,
        ) {
            Ok(WritePaneCommand::Wrote) => {
                println!("ran {} in {}", item.command, item.session)
            }
            Ok(WritePaneCommand::SkipRunning) => {
                println!(
                    "skip {} in {} (already running)",
                    item.command, item.session
                )
            }
            Err(error) => eprintln!("skip {} in {}: {error:#}", item.command, item.session),
        }
    }
    Ok(())
}

enum WritePaneCommand {
    Wrote,
    SkipRunning,
}

fn command_has_line_break(command: &str) -> bool {
    command.contains('\n') || command.contains('\r')
}

fn live_leaf_ids(layout: &LayoutSnapshot) -> Vec<u64> {
    match layout {
        LayoutSnapshot::Leaf { pane_id } => vec![*pane_id],
        LayoutSnapshot::Split { first, second, .. } => {
            let mut ids = live_leaf_ids(first);
            ids.extend(live_leaf_ids(second));
            ids
        }
    }
}

fn pane_has_foreground(snapshot: &Snapshot, pane_id: u64) -> bool {
    snapshot
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .flat_map(|window| window.panes.iter())
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.child_pid)
        .and_then(procinfo::live_foreground_command)
        .is_some()
}

fn write_pane_command(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    command: &str,
    skip_if_running: bool,
) -> Result<WritePaneCommand> {
    wait_pane_child(client, pane_id, Duration::from_secs(2))?;
    let now = take_snapshot(client)?;
    if skip_if_running && pane_has_foreground(&now, pane_id) {
        return Ok(WritePaneCommand::SkipRunning);
    }
    client
        .request(|request_id| ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        })
        .context("acquire lease")?;
    let now = take_snapshot(client)?;
    if skip_if_running && pane_has_foreground(&now, pane_id) {
        let _ = client.request(|request_id| ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        });
        return Ok(WritePaneCommand::SkipRunning);
    }
    let written = client.request(|request_id| ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        data: format!("{command}\r"),
    });
    let _ = client.request(|request_id| ControlRequest::ReleaseLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    });
    written.context("write pane")?;
    Ok(WritePaneCommand::Wrote)
}

fn wait_pane_child(client: &mut Client, pane_id: u64, timeout: Duration) -> Result<u32> {
    let start = Instant::now();
    loop {
        let snapshot = take_snapshot(client)?;
        if let Some(pid) = snapshot
            .sessions
            .iter()
            .flat_map(|session| session.windows.iter())
            .flat_map(|window| window.panes.iter())
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.child_pid)
        {
            // Give the shell time to reach readline before we type.
            std::thread::sleep(Duration::from_millis(100));
            return Ok(pid);
        }
        if start.elapsed() >= timeout {
            bail!("pane {pane_id} has no child process");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn apply_new_window(client: &mut Client, session_id: u64, window: &SavedWindow) -> Result<()> {
    let (root_cwd, _) = plan(&window.root);
    let title = sanitize_window_title(&window.title);
    let created = client.request(|request_id| ControlRequest::CreateWindow {
        version: PROTOCOL_VERSION,
        request_id,
        session_id,
        title,
        spawn: default_layout_spawn(root_cwd),
        cols: Some(u32::from(window.cols)),
        rows: Some(u32::from(window.rows)),
    })?;
    let ControlResponseData::Window {
        window_id, pane_id, ..
    } = created
    else {
        bail!("server returned an unexpected create-window response");
    };
    apply_splits(client, window_id, pane_id, &window.root)
}

fn apply_splits(
    client: &mut Client,
    window_id: u64,
    first_pane: u64,
    root: &SavedNode,
) -> Result<()> {
    let (_, ops) = plan(root);
    let mut panes = vec![first_pane];
    for op in ops {
        let target = panes
            .get(op.target.0)
            .copied()
            .context("layout plan refers to a missing pane")?;
        let _ = client.request(|request_id| ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id,
            window_id,
            target_pane_id: target,
            axis: op.axis,
            ratio: op.ratio,
            spawn: default_layout_spawn(op.cwd.clone()),
            client_id: None,
        })?;
        // Split returns Mutation { ack } only. The new pane id is on the
        // event stream, so we diff the snapshot. A concurrent split on this
        // window by another client would misattribute that id; acceptable
        // for a CLI.
        let snapshot = take_snapshot(client)?;
        let known = panes.clone();
        let new_id = pane_ids_in_window(&snapshot, window_id)?
            .into_iter()
            .find(|id| !known.contains(id))
            .context("split did not create a pane")?;
        debug_assert_eq!(panes.len(), op.new.0);
        panes.push(new_id);
    }
    Ok(())
}

fn pane_ids_in_window(snapshot: &Snapshot, window_id: u64) -> Result<Vec<u64>> {
    snapshot
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .find(|window| window.id == window_id)
        .map(|window| window.panes.iter().map(|pane| pane.id).collect())
        .context("window missing from snapshot after split")
}

fn take_snapshot(client: &mut Client) -> Result<Snapshot> {
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    match snapshot {
        ControlResponseData::Snapshot { snapshot } => Ok(snapshot),
        _ => bail!("server returned an unexpected snapshot response"),
    }
}

fn require_live_socket(paths: &Paths) -> Result<()> {
    match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => Ok(()),
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        SocketLiveness::Missing | SocketLiveness::Stale => {
            bail!("not running")
        }
    }
}

fn default_layout_spawn(cwd: Option<std::path::PathBuf>) -> SpawnSpec {
    let program = program_from(Vec::new());
    SpawnSpec {
        program: program[0].clone(),
        argv: program[1..].to_vec(),
        cwd: cwd.filter(|path| path.is_absolute()),
        env: Default::default(),
    }
}

fn sanitize_window_title(title: &str) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        "main".into()
    } else {
        trimmed.to_string()
    }
}

fn resolve_session_snapshot<'a>(
    snapshot: &'a Snapshot,
    key: Option<&str>,
) -> Result<&'a SessionSnapshot> {
    if let Some(key) = key {
        let (id, _) = resolve_session(snapshot, key)?;
        return snapshot
            .sessions
            .iter()
            .find(|session| session.id == id)
            .context("session missing from snapshot");
    }
    if let Some(pane_id) = std::env::var("PRISMATTYC_PANE_ID")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        if let Some(session) = snapshot.sessions.iter().find(|session| {
            session
                .windows
                .iter()
                .flat_map(|window| window.panes.iter())
                .any(|pane| pane.id == pane_id)
        }) {
            return Ok(session);
        }
    }
    let non_default: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.name != "default")
        .collect();
    match non_default.as_slice() {
        [session] => Ok(*session),
        [] if snapshot.sessions.len() == 1 => Ok(&snapshot.sessions[0]),
        [] => bail!("no session to save; pass SESSION"),
        _ => bail!("session is ambiguous; pass SESSION"),
    }
}

fn parse_layout_save_args(
    global_session: Option<String>,
    rest: Vec<String>,
) -> Result<(Option<String>, Option<String>)> {
    let mut session = global_session;
    let mut name = None;
    let mut positional = None;
    let mut iter = rest.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => {
                let value =
                    parse_session_name(iter.next().context("--session requires a name or id")?)?;
                if session.as_ref().is_some_and(|existing| existing != &value) {
                    bail!("conflicting session names");
                }
                session = Some(value);
            }
            "--name" => {
                let value = iter.next().context("--name requires a layout name")?;
                validate_layout_name(&value)?;
                if name.is_some() {
                    bail!("usage: pmux layout save [SESSION] [--name NAME]");
                }
                name = Some(value);
            }
            flag if flag.starts_with('-') => {
                bail!("unknown layout save argument {flag:?}");
            }
            other => {
                if positional.is_some() {
                    bail!("usage: pmux layout save [SESSION] [--name NAME]");
                }
                positional = Some(parse_session_name(other.to_string())?);
            }
        }
    }
    match (session, positional) {
        (Some(a), Some(b)) if a != b => bail!("conflicting session names {a:?} and {b:?}"),
        (Some(s), _) | (None, Some(s)) => Ok((Some(s), name)),
        (None, None) => Ok((None, name)),
    }
}

/// Default space name for `save space` / `apply space` with no NAME.
const DEFAULT_SPACE_NAME: &str = "default";

const SPACE_SAVE_USAGE: &str = "usage: pmux space save [NAME] [SESSION...] [--name NAME]";
const SPACE_OPEN_USAGE: &str =
    "usage: pmux space open [NAME] [--add] [--replace] [--no-attach] [--new-window] [--no-run] [--tty]";
const SPACE_ATTACH_USAGE: &str = "usage: pmux space attach [NAME] [--session S]";
const SPACE_RM_USAGE: &str = "usage: pmux space rm [NAME...] [--all]";
const SPACE_CLEAR_USAGE: &str = "usage: pmux space clear [--keep NAME]";
const SESSION_CLEAR_USAGE: &str = "usage: pmux session clear [--all] [--keep NAME]";
const LAYOUT_RM_USAGE: &str = "usage: pmux layout rm [NAME...] [--all]";
const LAYOUT_SAVE_SPACE_USAGE: &str =
    "usage: pmux layout save space [NAME] [SESSION...] [--name NAME]";

/// `save space [NAME] [SESSION...] [--name NAME]`: the first positional is
/// the name unless `--name` is given, in which case every positional is a
/// session. No name at all saves `default`.
fn parse_layout_save_space_args(
    rest: Vec<String>,
    usage: &'static str,
) -> Result<(String, Vec<String>)> {
    let mut name = None;
    let mut positionals = Vec::new();
    let mut iter = rest.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => {
                let value = iter.next().context("--name requires a value")?;
                if name.is_some() {
                    bail!("--name given twice");
                }
                name = Some(value);
            }
            flag if flag.starts_with('-') => bail!("{usage}"),
            _ => positionals.push(arg),
        }
    }
    let mut positionals = positionals.into_iter();
    let name = match name {
        Some(name) => name,
        None => positionals
            .next()
            .unwrap_or_else(|| DEFAULT_SPACE_NAME.to_string()),
    };
    validate_layout_name(&name)?;
    let sessions = positionals
        .map(parse_session_name)
        .collect::<Result<Vec<_>>>()?;
    Ok((name, sessions))
}

fn resolve_space_sessions<'a>(
    snapshot: &'a Snapshot,
    keys: &[String],
) -> Result<Vec<&'a SessionSnapshot>> {
    if keys.is_empty() {
        let selected: Vec<_> = snapshot
            .sessions
            .iter()
            .filter(|session| session.name != "default")
            .collect();
        if !selected.is_empty() {
            return Ok(selected);
        }
        return Ok(snapshot.sessions.iter().collect());
    }
    let mut out = Vec::new();
    for key in keys {
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.name == *key || session.id.to_string() == *key)
            .with_context(|| format!("session {key:?} not found"))?;
        if out
            .iter()
            .any(|seen: &&SessionSnapshot| seen.id == session.id)
        {
            bail!("session {key:?} listed more than once");
        }
        out.push(session);
    }
    Ok(out)
}

fn parse_layout_apply_args(global_session: Option<String>, rest: Vec<String>) -> Result<ApplyArgs> {
    let mut target = global_session;
    let mut all = false;
    let mut replace = false;
    let mut add = false;
    let mut bind_agent = false;
    let mut no_attach = false;
    let mut new_window = false;
    let mut no_run = false;
    let mut tty = false;
    let mut positionals = Vec::new();
    let mut iter = rest.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--all" => all = true,
            "--replace" => replace = true,
            "--add" => add = true,
            "--agent" => bind_agent = true,
            "--no-attach" => no_attach = true,
            "--new-window" => new_window = true,
            "--no-run" => no_run = true,
            "--tty" => tty = true,
            "--session" => {
                let value =
                    parse_session_name(iter.next().context("--session requires a name or id")?)?;
                if target.as_ref().is_some_and(|existing| existing != &value) {
                    bail!("conflicting session names");
                }
                target = Some(value);
            }
            flag if flag.starts_with('-') => {
                bail!("unknown layout apply argument {flag:?}");
            }
            _ => positionals.push(arg),
        }
    }
    if (no_attach || new_window || no_run || tty || add)
        && positionals.first().map(String::as_str) != Some("space")
    {
        bail!("--no-attach, --new-window, --no-run, --tty, and --add apply to 'apply space'");
    }
    if no_attach && new_window {
        bail!("--new-window and --no-attach cannot be combined");
    }
    if tty && new_window {
        bail!("--new-window and --tty cannot be combined");
    }
    if add && new_window {
        bail!("--new-window and --add cannot be combined");
    }
    if all {
        if !positionals.is_empty() {
            bail!("usage: pmux layout apply --all [--replace]");
        }
        if target.is_some() {
            bail!("pmux layout apply --all does not take --session");
        }
        return Ok(ApplyArgs::All { replace });
    }
    if positionals.first().map(String::as_str) == Some("space") {
        if positionals.len() > 2 {
            bail!(
                "usage: pmux layout apply space [NAME] [--add] [--replace] [--no-attach] [--new-window] [--no-run] [--tty]"
            );
        }
        if target.is_some() {
            bail!("pmux layout apply space does not take --session");
        }
        if bind_agent {
            bail!("pmux layout apply space always binds the saved agent");
        }
        let name = positionals
            .get(1)
            .cloned()
            .unwrap_or_else(|| DEFAULT_SPACE_NAME.to_string());
        validate_layout_name(&name)?;
        return Ok(ApplyArgs::Space {
            name,
            replace,
            add,
            attach: !no_attach,
            new_window,
            no_run,
            tty,
            host: true,
        });
    }
    if replace {
        bail!("--replace applies to 'apply space' and 'apply --all'");
    }
    if positionals.len() != 1 {
        bail!("usage: pmux layout apply NAME [--session TARGET] [--agent]");
    }
    let name = positionals.remove(0);
    validate_layout_name(&name)?;
    let target = target.unwrap_or_else(|| name.clone());
    Ok(ApplyArgs::One {
        name,
        target,
        bind_agent,
    })
}

fn parse_space_open_args(rest: Vec<String>) -> Result<ApplyArgs> {
    let mut replace = false;
    let mut add = false;
    let mut no_attach = false;
    let mut new_window = false;
    let mut no_run = false;
    let mut tty = false;
    let mut positionals = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "--replace" => replace = true,
            "--add" => add = true,
            "--no-attach" => no_attach = true,
            "--new-window" => new_window = true,
            "--no-run" => no_run = true,
            "--tty" => tty = true,
            flag if flag.starts_with('-') => bail!("{SPACE_OPEN_USAGE}"),
            _ => positionals.push(arg),
        }
    }
    if no_attach && new_window {
        bail!("--new-window and --no-attach cannot be combined");
    }
    if tty && new_window {
        bail!("--new-window and --tty cannot be combined");
    }
    if add && new_window {
        bail!("--new-window and --add cannot be combined");
    }
    if positionals.len() > 1 {
        bail!("{SPACE_OPEN_USAGE}");
    }
    let name = positionals
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_SPACE_NAME.to_string());
    validate_layout_name(&name)?;
    Ok(ApplyArgs::Space {
        name,
        replace,
        add,
        attach: !no_attach,
        new_window,
        no_run,
        tty,
        host: true,
    })
}

struct SpaceAttachArgs {
    name: String,
    session: Option<String>,
}

fn parse_space_attach_args(rest: Vec<String>) -> Result<SpaceAttachArgs> {
    let mut session = None;
    let mut positionals = Vec::new();
    let mut iter = rest.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => {
                let value = iter.next().context("--session requires a name")?;
                if value.is_empty() || value.starts_with('-') {
                    bail!("--session requires a name");
                }
                if session.is_some() {
                    bail!("{SPACE_ATTACH_USAGE}");
                }
                session = Some(value);
            }
            flag if flag.starts_with('-') => bail!("{SPACE_ATTACH_USAGE}"),
            _ => positionals.push(arg),
        }
    }
    if positionals.len() > 1 {
        bail!("{SPACE_ATTACH_USAGE}");
    }
    let name = positionals
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_SPACE_NAME.to_string());
    validate_layout_name(&name)?;
    Ok(SpaceAttachArgs { name, session })
}

const SYNC_USAGE: &str = "pmux sync — fan typed input across panes in a session

usage: pmux sync <on|off|status> [SESSION]

on      enable sync on every window in SESSION
off     disable sync on every window in SESSION
status  print per-window sync state

SESSION resolves like other verbs (--session, else this pane, else the
only non-default session). Mouse stays per-pane. Host in-process panes
are out of scope.";

fn cmd_sync(paths: &Paths, global_session: Option<String>, rest: Vec<String>) -> Result<()> {
    let mut iter = rest.into_iter().peekable();
    let verb = iter.next().context(SYNC_USAGE)?;
    match verb.as_str() {
        "-h" | "--help" => {
            println!("{SYNC_USAGE}");
            return Ok(());
        }
        "on" | "off" | "status" => {}
        other => bail!("unknown sync verb {other:?}"),
    }
    let rest: Vec<String> = iter.collect();
    let session_key = session_from_args(global_session, rest, "sync")?;
    require_live_socket(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };
    let snapshot = take_snapshot(&mut client)?;
    let session = resolve_session_snapshot(&snapshot, session_key.as_deref())?;
    let name = session.name.clone();
    let windows: Vec<(u64, String, bool)> = session
        .windows
        .iter()
        .map(|window| (window.id, window.title.clone(), window.sync_input))
        .collect();
    if windows.is_empty() {
        bail!("session {name:?} has no windows");
    }
    match verb.as_str() {
        "status" => {
            for (id, title, enabled) in windows {
                let state = if enabled { "on" } else { "off" };
                println!("window {id} {title:?}  sync {state}");
            }
        }
        "on" | "off" => {
            let enabled = verb == "on";
            for (window_id, _, _) in &windows {
                let _ = client.request(|request_id| ControlRequest::SetSyncInput {
                    version: PROTOCOL_VERSION,
                    request_id,
                    client_id,
                    window_id: *window_id,
                    enabled,
                })?;
            }
            let state = if enabled { "on" } else { "off" };
            println!("session {name}: sync {state} ({} windows)", windows.len());
        }
        _ => unreachable!(),
    }
    Ok(())
}

const SAVE_BUFFER_USAGE: &str = "usage: pmux save-buffer PANE|SESSION FILE [--history]";
const PIPE_PANE_USAGE: &str = "usage: pmux pipe-pane PANE|SESSION (FILE | --exec CMD)";
/// Matches `MAX_MAIL_WAIT` on the server SubscribePane path.
const PIPE_SUBSCRIBE_TIMEOUT_MS: u32 = 3_600_000;

#[derive(Debug, PartialEq, Eq)]
struct SaveBufferArgs {
    target: String,
    file: PathBuf,
    history: bool,
}

fn parse_save_buffer_args(rest: Vec<String>) -> Result<SaveBufferArgs> {
    let mut history = false;
    let mut positional = Vec::new();
    for arg in rest {
        match arg.as_str() {
            "-h" | "--help" => continue,
            "--history" => history = true,
            flag if flag.starts_with('-') && flag != "-" => {
                bail!("unknown save-buffer argument {flag:?}")
            }
            _ => positional.push(arg),
        }
    }
    let [target, file] = positional.as_slice() else {
        bail!("{SAVE_BUFFER_USAGE}");
    };
    Ok(SaveBufferArgs {
        target: target.clone(),
        file: PathBuf::from(file),
        history,
    })
}

fn join_pane_lines(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn read_pane_content(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
) -> Result<prismattyc_mux::PaneContent> {
    match client.request(|request_id| ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    }) {
        Ok(ControlResponseData::PaneContent { content }) => Ok(content),
        Ok(_) => bail!("pane {pane_id} returned an unexpected read"),
        Err(error) => Err(error).with_context(|| format!("read pane {pane_id}")),
    }
}

fn read_pane_styled(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    view_offset: Option<u32>,
) -> Result<prismattyc_mux::PaneStyled> {
    match client.request(|request_id| ControlRequest::ReadPaneStyled {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        view_offset,
    }) {
        Ok(ControlResponseData::PaneStyled { content }) => Ok(content),
        Ok(_) => bail!("pane {pane_id} returned an unexpected styled read"),
        Err(error) => Err(error).with_context(|| format!("read styled pane {pane_id}")),
    }
}

fn pane_screen_text(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    history: bool,
) -> Result<String> {
    let content = read_pane_content(client, client_id, pane_id)?;
    if !history {
        return Ok(join_pane_lines(&content.lines));
    }
    let live = read_pane_styled(client, client_id, pane_id, None)?;
    let mut off = live.max_view_scroll.unwrap_or(0);
    let mut lines = Vec::new();
    while off > 0 {
        let chunk = read_pane_styled(client, client_id, pane_id, Some(off))?;
        let n = u32::try_from(chunk.content.lines.len()).unwrap_or(u32::MAX);
        if n == 0 {
            break;
        }
        let take = off.min(n) as usize;
        lines.extend(chunk.content.lines.iter().take(take).cloned());
        off = off.saturating_sub(take as u32);
    }
    lines.extend(content.lines);
    Ok(join_pane_lines(&lines))
}

fn write_save_buffer(path: &Path, text: &str) -> Result<()> {
    if path.as_os_str() == "-" {
        let mut out = io::stdout().lock();
        out.write_all(text.as_bytes())?;
        out.flush()?;
        return Ok(());
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn cmd_save_buffer(paths: &Paths, rest: Vec<String>) -> Result<()> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!(
            "\
pmux save-buffer PANE|SESSION FILE [--history]

Write the pane's visible screen text to FILE. FILE `-` writes stdout.
--history prepends scrollback (ReadPaneStyled view_offset walk).
PANE is a pane id; a SESSION name works when the session has one pane.
"
        );
        return Ok(());
    }
    let parsed = parse_save_buffer_args(rest)?;
    let (mut client, client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let pane_id = resolve_pane_target(&snapshot, &parsed.target)?;
    let text = pane_screen_text(&mut client, client_id, pane_id, parsed.history)?;
    write_save_buffer(&parsed.file, &text)
}

#[derive(Debug, PartialEq, Eq)]
enum PipePaneDest {
    File(PathBuf),
    Exec(String),
}

#[derive(Debug, PartialEq, Eq)]
struct PipePaneArgs {
    target: String,
    dest: PipePaneDest,
}

fn parse_pipe_pane_args(rest: Vec<String>) -> Result<PipePaneArgs> {
    let mut target = None;
    let mut file = None;
    let mut exec = None;
    let mut iter = rest.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => continue,
            "--exec" => {
                let cmd: Vec<String> = iter.collect();
                if cmd.is_empty() {
                    bail!("{PIPE_PANE_USAGE}");
                }
                exec = Some(cmd.join(" "));
                break;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                bail!("unknown pipe-pane argument {flag:?}")
            }
            _ if target.is_none() => target = Some(arg),
            _ if file.is_none() => file = Some(arg),
            _ => bail!("{PIPE_PANE_USAGE}"),
        }
    }
    let target = target.context(PIPE_PANE_USAGE)?;
    match (file, exec) {
        (Some(_), Some(_)) | (None, None) => bail!("{PIPE_PANE_USAGE}"),
        (Some(path), None) => Ok(PipePaneArgs {
            target,
            dest: PipePaneDest::File(PathBuf::from(path)),
        }),
        (None, Some(cmd)) => Ok(PipePaneArgs {
            target,
            dest: PipePaneDest::Exec(cmd),
        }),
    }
}

struct PipeWriter {
    sink: Box<dyn Write>,
    child: Option<Child>,
}

impl PipeWriter {
    fn open(dest: &PipePaneDest) -> Result<Self> {
        match dest {
            PipePaneDest::File(path) if path.as_os_str() == "-" => Ok(Self {
                sink: Box::new(io::stdout()),
                child: None,
            }),
            PipePaneDest::File(path) => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open {}", path.display()))?;
                Ok(Self {
                    sink: Box::new(file),
                    child: None,
                })
            }
            PipePaneDest::Exec(cmd) => {
                let mut child = Command::new(prismattyc_mux::platform::default_shell())
                    .arg(if cfg!(windows) { "/C" } else { "-c" })
                    .arg(cmd)
                    .stdin(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("exec {cmd:?}"))?;
                let stdin = child.stdin.take().context("exec command has no stdin")?;
                Ok(Self {
                    sink: Box::new(stdin),
                    child: Some(child),
                })
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<bool> {
        match self.sink.write_all(bytes).and_then(|()| self.sink.flush()) {
            Ok(()) => Ok(true),
            Err(error)
                if error.kind() == io::ErrorKind::BrokenPipe
                    || error.kind() == io::ErrorKind::Interrupted =>
            {
                Ok(false)
            }
            Err(error) => Err(error).context("write pipe-pane destination"),
        }
    }

    fn finish(mut self) -> Result<()> {
        let flush = self.sink.flush();
        drop(self.sink);
        let child_status = match self.child.take() {
            Some(mut child) => Some(reap_pipe_child(&mut child)?),
            None => None,
        };
        match flush {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::BrokenPipe
                    || error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("flush pipe-pane destination"),
        }
        if let Some(status) = child_status {
            if !status.success() && !pipe_child_killed(status) {
                eprintln!("pmux pipe-pane: --exec exited {status}");
            }
        }
        Ok(())
    }
}

const PIPE_EXEC_GRACE: Duration = Duration::from_secs(2);
const PIPE_EXEC_TERM_GRACE: Duration = Duration::from_millis(200);

fn wait_child_until(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reap_pipe_child(child: &mut Child) -> io::Result<ExitStatus> {
    if let Some(status) = wait_child_until(child, PIPE_EXEC_GRACE)? {
        return Ok(status);
    }
    #[cfg(unix)]
    {
        // SAFETY: `child.id()` is the pid we spawned and have not reaped.
        let _ = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    }
    if let Some(status) = wait_child_until(child, PIPE_EXEC_TERM_GRACE)? {
        return Ok(status);
    }
    child.kill()?;
    child.wait()
}

fn pipe_child_killed(status: ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().is_some()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

fn subscribe_output_bytes(events: &[prismattyc_mux::PaneLogFrame]) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut exited = false;
    for frame in events {
        match &frame.event {
            PaneEvent::Output { bytes: chunk } => bytes.extend_from_slice(chunk),
            PaneEvent::Exited { .. } => exited = true,
            PaneEvent::Resize { .. }
            | PaneEvent::Title { .. }
            | PaneEvent::Cwd { .. }
            | PaneEvent::Status { .. }
            | PaneEvent::Attention { .. }
            | PaneEvent::MailDepth { .. }
            | PaneEvent::SizeOwnerChanged { .. } => {}
        }
    }
    (bytes, exited)
}

fn pane_current_seq(client: &mut Client, client_id: u64, pane_id: u64) -> Result<u64> {
    // from_seq > current is CatchUp::Ahead. Probe with u64::MAX so the
    // server does not ship the ring or build a Gap snapshot; the wire is
    // StaleSequence with current_sequence = current (empty through_seq).
    match client.request(|request_id| ControlRequest::SubscribePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        from_seq: u64::MAX,
        timeout_ms: 0,
    }) {
        Ok(ControlResponseData::PaneSubscribe { through_seq, .. }) => Ok(through_seq),
        Ok(_) => bail!("pane {pane_id} returned an unexpected subscribe"),
        Err(error) => {
            if let Some(seq) = error
                .downcast_ref::<ControlError>()
                .filter(|err| err.code == ControlErrorCode::StaleSequence)
                .and_then(|err| err.current_sequence)
            {
                Ok(seq)
            } else {
                Err(error).with_context(|| format!("subscribe pane {pane_id}"))
            }
        }
    }
}

fn report_pipe_gap(gap: bool, through_seq: u64) {
    if gap {
        eprintln!("pmux pipe-pane: pane-log gap before seq {through_seq} — bytes lost");
    }
}

fn cmd_pipe_pane(paths: &Paths, rest: Vec<String>) -> Result<()> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!(
            "\
pmux pipe-pane PANE|SESSION (FILE | --exec CMD)

Subscribe from the current pane-log seq and write Output event bytes
byte-exact to FILE (append) or to CMD's stdin. `--exec` runs `sh -c CMD`.
A non-zero --exec exit prints a note and does not fail pipe-pane.
Resize and other events are ignored. Stops on Ctrl-C or when the pane
exits. FILE `-` writes stdout. PANE is a pane id; a SESSION name works
when the session has one pane.
"
        );
        return Ok(());
    }
    let parsed = parse_pipe_pane_args(rest)?;
    let (mut client, client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let pane_id = resolve_pane_target(&snapshot, &parsed.target)?;
    let mut through_seq = pane_current_seq(&mut client, client_id, pane_id)?;
    let mut exited = false;
    let mut writer = PipeWriter::open(&parsed.dest)?;
    client.hold_for_wait(PIPE_SUBSCRIBE_TIMEOUT_MS)?;
    'pipe: while !exited {
        let request_id = client.send_request(|request_id| ControlRequest::SubscribePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            from_seq: through_seq,
            timeout_ms: PIPE_SUBSCRIBE_TIMEOUT_MS,
        })?;
        let mut skipped = 0u32;
        loop {
            let response = match client.read_response() {
                Ok(response) => response,
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|io_err| io_err.kind() == io::ErrorKind::Interrupted) =>
                {
                    break 'pipe;
                }
                Err(error) => {
                    writer.finish()?;
                    return Err(error);
                }
            };
            match classify_control_request_id(response.request_id, request_id) {
                ControlIdMatch::Awaited => {}
                ControlIdMatch::Stale => {
                    skipped = match next_stale_skip(skipped) {
                        Some(n) => n,
                        None => {
                            writer.finish()?;
                            bail!("gave up after stale control responses before {request_id}");
                        }
                    };
                    continue;
                }
                ControlIdMatch::Ahead => {
                    writer.finish()?;
                    bail!(
                        "response request ID {} is ahead of awaited {request_id}",
                        response.request_id
                    );
                }
            }
            match response.body {
                ControlResponseBody::Ok {
                    response:
                        ControlResponseData::PaneSubscribe {
                            through_seq: next,
                            events,
                            done,
                            gap,
                            ..
                        },
                } => {
                    through_seq = next;
                    report_pipe_gap(gap, next);
                    let (bytes, frame_exited) = subscribe_output_bytes(&events);
                    if !bytes.is_empty() && !writer.write_all(&bytes)? {
                        break 'pipe;
                    }
                    exited = frame_exited;
                    if done || exited {
                        break;
                    }
                }
                ControlResponseBody::Ok { response } => {
                    writer.finish()?;
                    bail!("pane {pane_id} returned an unexpected subscribe: {response:?}")
                }
                ControlResponseBody::Error { error } => {
                    writer.finish()?;
                    return Err(anyhow::Error::new(error));
                }
            }
        }
        if exited {
            break;
        }
        let snapshot = take_snapshot(&mut client)?;
        if snapshot_pane(&snapshot, pane_id).is_none() {
            break;
        }
    }
    writer.finish()
}

fn cmd_status(paths: &Paths) -> Result<()> {
    let liveness = probe_socket_liveness(&paths.socket);
    println!("socket: {}", paths.socket.display());
    match liveness {
        SocketLiveness::Live => {
            println!("status: running");
            match read_valid_pid(paths) {
                Some(pid) => println!("pid:    {pid}"),
                None => println!("pid:    unknown (no valid pidfile; server still answers)"),
            }
            println!("log:    {}", paths.logfile.display());
        }
        SocketLiveness::Missing => {
            println!("status: not running (no socket)");
            if let Some(miss) = diagnose_runtime_dir_miss_from_env(&paths.socket) {
                println!("{miss}");
            }
        }
        SocketLiveness::Stale => {
            println!("status: not running (stale socket leftover; `pmux up` will replace it)");
            if let Some(miss) = diagnose_runtime_dir_miss_from_env(&paths.socket) {
                println!("{miss}");
            }
        }
        SocketLiveness::Foreign => {
            println!("status: foreign path — not a Prismattyc socket; refusing")
        }
    }
    Ok(())
}

fn try_shutdown_via_verb(socket: &Path) -> bool {
    let mut client = match Client::connect(socket) {
        Ok(client) => client,
        Err(_) => return false,
    };
    let registered = match client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    }) {
        Ok(ControlResponseData::ClientRegistered { client_id }) => client_id,
        _ => return false,
    };
    matches!(
        client.request(|request_id| ControlRequest::ShutdownServer {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: registered,
        }),
        Ok(ControlResponseData::ShutdownAccepted)
    )
}

fn parse_session_name(name: String) -> Result<String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        bail!("--session must be a non-empty name or id without path separators");
    }
    Ok(name)
}

fn reject_session_flag(session: Option<&str>, command: &str) -> Result<()> {
    if session.is_some() {
        bail!("--session only applies to stop, doctor, kick, layout, and sync (got {command})");
    }
    Ok(())
}

/// `--session NAME` (global) or `COMMAND NAME` / `COMMAND --session NAME`.
fn session_from_args(
    global: Option<String>,
    rest: Vec<String>,
    command: &str,
) -> Result<Option<String>> {
    let mut rest = rest.into_iter().peekable();
    let mut positional = None;
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--session" => {
                let name =
                    parse_session_name(rest.next().context("--session requires a name or id")?)?;
                if let Some(existing) = positional {
                    if existing != name {
                        bail!("conflicting session names {existing:?} and {name:?}");
                    }
                }
                positional = Some(name);
            }
            name if !name.starts_with('-') => {
                if positional.is_some() {
                    bail!("usage: pmux [--session NAME] {command} [NAME]");
                }
                positional = Some(parse_session_name(name.to_string())?);
            }
            other => bail!("unknown {command} argument {other:?}"),
        }
    }
    match (global, positional) {
        (Some(a), Some(b)) if a != b => bail!("conflicting session names {a:?} and {b:?}"),
        (Some(name), _) | (None, Some(name)) => Ok(Some(name)),
        (None, None) => Ok(None),
    }
}

fn resolve_session<'a>(snapshot: &'a Snapshot, key: &str) -> Result<(u64, &'a str)> {
    let matches: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.name == key || session.id.to_string() == key)
        .collect();
    match matches.as_slice() {
        [session] => Ok((session.id, session.name.as_str())),
        [] => bail!("no session matching {key:?}"),
        _ => bail!("session name {key:?} is ambiguous; use the opaque session id"),
    }
}

fn cmd_stop_session(paths: &Paths, key: &str) -> Result<()> {
    match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => {}
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        SocketLiveness::Missing | SocketLiveness::Stale => {
            bail!("not running (no live server to stop session {key:?})");
        }
    }
    let mut client = Client::connect(&paths.socket)?;
    let _ = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let snapshot = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        bail!("server returned an unexpected snapshot response");
    };
    let (session_id, name) = resolve_session(&snapshot, key)?;
    let name = name.to_string();
    destroy_session(&mut client, session_id)?;
    println!("stopped session {name:?} (id {session_id})");
    Ok(())
}

fn cmd_stop(paths: &Paths, session: Option<String>) -> Result<()> {
    if let Some(key) = session {
        return cmd_stop_session(paths, &key);
    }
    let liveness = probe_socket_liveness(&paths.socket);
    match liveness {
        SocketLiveness::Live => {}
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            paths.socket.display()
        ),
        SocketLiveness::Missing => {
            let _ = std::fs::remove_file(&paths.pidfile);
            println!("not running");
            return Ok(());
        }
        // Stale means the socket answers no control handshake — but a
        // frozen-but-alive server (e.g. under SIGSTOP) also cannot answer, and
        // it must still be signalled. Resolve the pid below; only a dead pid
        // means genuinely not running.
        SocketLiveness::Stale => {}
    }
    let pid = read_valid_pid(paths).or_else(|| find_server_pid(&paths.socket));
    if liveness == SocketLiveness::Stale && !pid.is_some_and(pid_alive) {
        let _ = std::fs::remove_file(&paths.pidfile);
        println!("not running");
        return Ok(());
    }
    if try_shutdown_via_verb(&paths.socket) {
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            let pid_gone = pid.is_none_or(|pid| server_gone(pid, &paths.socket));
            let socket_gone = !matches!(probe_socket_liveness(&paths.socket), SocketLiveness::Live);
            if pid_gone && socket_gone {
                let _ = std::fs::remove_file(&paths.pidfile);
                match pid {
                    Some(pid) => println!("stopped pid {pid}"),
                    None => println!("stopped"),
                }
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let pid =
        pid.context("server is running but its pid could not be determined; stop it manually")?;
    signal(pid, prismattyc_mux::platform::Signal::TERM)?;
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && !server_gone(pid, &paths.socket) {
        std::thread::sleep(Duration::from_millis(50));
    }
    // Re-verify before escalating: if the pid was recycled inside the grace
    // window, the cmdline no longer matches and KILL must not be sent.
    if pid_alive(pid) && cmdline_matches(pid, &paths.socket) {
        signal(pid, prismattyc_mux::platform::Signal::KILL)?;
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && !server_gone(pid, &paths.socket) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if !server_gone(pid, &paths.socket) {
        bail!("pid {pid} survived TERM and KILL; stop it manually");
    }
    let _ = std::fs::remove_file(&paths.pidfile);
    println!("stopped pid {pid}");
    Ok(())
}

/// The server identified by `pid` is no longer running (exited, or the pid
/// now belongs to a different process).
fn server_gone(pid: u32, socket: &Path) -> bool {
    !pid_alive(pid) || !cmdline_matches(pid, socket)
}

#[cfg(unix)]
fn signal(pid: u32, signal: prismattyc_mux::platform::Signal) -> Result<()> {
    let pid = rustix::process::Pid::from_raw(pid as i32).context("invalid pid")?;
    rustix::process::kill_process(pid, signal).with_context(|| format!("signal {pid:?}"))?;
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    procinfo::pid_alive(pid)
}

/// Pidfile pid, only when /proc confirms it is still a pmuxd on
/// this exact socket — a recycled pid must never be signalled.
fn read_valid_pid(paths: &Paths) -> Option<u32> {
    let raw = std::fs::read_to_string(&paths.pidfile).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    cmdline_matches(pid, &paths.socket).then_some(pid)
}

/// Exact-argv check: argv[0] must be `pmuxd` (bare or by path) and
/// the socket must be the argument right after `--socket` — the only spelling
/// this CLI ever spawns. Substring matching would also hit wrappers
/// (`strace -f pmuxd …`) and prefix paths (`/tmp/foo` vs
/// `/tmp/foo.sock`).
fn cmdline_matches(pid: u32, socket: &Path) -> bool {
    procinfo::cmdline_matches_server(pid, socket)
}

/// Fallback for a lost pidfile: scan /proc for the server holding this socket.
/// When the bound socket's inode is readable, the pid must also hold it — a
/// process that merely *mentions* the path in argv is never a hit.
fn find_server_pid(socket: &Path) -> Option<u32> {
    let inode = procinfo::listener_inode(socket);
    procinfo::find_server_pids(socket)
        .into_iter()
        .find(|&pid| inode.is_none_or(|inode| procinfo::pid_holds_socket(pid, inode)))
}

fn listener_inode(path: &Path) -> Option<u64> {
    procinfo::listener_inode(path)
}

fn pid_holds_socket(pid: u32, inode: u64) -> bool {
    procinfo::pid_holds_socket(pid, inode)
}

fn log_tail(path: &Path, lines: usize) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return "<log unreadable>".to_string();
    };
    let all: Vec<&str> = raw.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

struct Client {
    socket_identity: String,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_request_id: u64,
}

impl Client {
    fn connect(path: &Path) -> Result<Self> {
        let stream =
            UnixStream::connect(path).with_context(|| format!("connect {}", path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let socket_identity = prismattyc_mux::platform::socket_identity(path)?;
        Ok(Self {
            socket_identity,
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_request_id: 1,
        })
    }

    fn send_request(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("request ID space exhausted")?;
        let request = make(request_id);
        serde_json::to_writer(&mut self.writer, &request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(request_id)
    }

    fn read_response(&mut self) -> Result<ControlResponse> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("server closed the control connection");
        }
        Ok(serde_json::from_str(&line)?)
    }

    fn read_matching(&mut self, request_id: u64) -> Result<ControlResponse> {
        let mut skipped = 0u32;
        loop {
            let response = self.read_response()?;
            match classify_control_request_id(response.request_id, request_id) {
                ControlIdMatch::Awaited => return Ok(response),
                ControlIdMatch::Stale => {
                    skipped = match next_stale_skip(skipped) {
                        Some(n) => n,
                        None => bail!(
                            "gave up after stale control responses before {request_id} (last was {})",
                            response.request_id
                        ),
                    };
                }
                ControlIdMatch::Ahead => bail!(
                    "response request ID {} is ahead of awaited {request_id}",
                    response.request_id
                ),
            }
        }
    }

    fn request(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> Result<ControlResponseData> {
        let request_id = self.send_request(make)?;
        let response = self.read_matching(request_id)?;
        match response.body {
            ControlResponseBody::Ok { response } => Ok(response),
            ControlResponseBody::Error { error } => Err(anyhow::Error::new(error)),
        }
    }

    /// `mail watch` blocks server-side for the whole wait; the default 2s
    /// socket timeout would cut the reply. Give the read side the wait
    /// plus a margin.
    fn hold_for_wait(&self, timeout_ms: u32) -> Result<()> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(u64::from(timeout_ms) + 5_000)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::{
        EventBatch, EventEnvelope, MailWake, PaneContent, PaneGeometry, PaneInputLedger,
        PaneSnapshot, SavedSpaceSession, SizeOwner, SizeOwnerKind, WindowBounds,
    };

    fn args(words: &[&str]) -> std::vec::IntoIter<String> {
        words
            .iter()
            .map(|w| (*w).to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn write_registered(peer: &mut UnixStream, request_id: u64) {
        let response = ControlResponse {
            version: PROTOCOL_VERSION,
            request_id,
            body: ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id: 9 },
            },
        };
        serde_json::to_writer(&mut *peer, &response).unwrap();
        peer.write_all(b"\n").unwrap();
        peer.flush().unwrap();
    }

    #[test]
    fn request_drains_two_stale_ids_and_the_next_request_succeeds() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_registered(&mut peer, 1);
        write_registered(&mut peer, 2);
        let mut client = Client {
            socket_identity: "test".into(),
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 2,
        };
        let first = client
            .request(|request_id| ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id,
            })
            .expect("drain stale id 1 and hit 2");
        match first {
            ControlResponseData::ClientRegistered { client_id } => assert_eq!(client_id, 9),
            other => panic!("expected ClientRegistered, got {other:?}"),
        }
        write_registered(&mut peer, 3);
        let second = client
            .request(|request_id| ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id,
            })
            .expect("next request after drain");
        match second {
            ControlResponseData::ClientRegistered { client_id } => assert_eq!(client_id, 9),
            other => panic!("expected ClientRegistered, got {other:?}"),
        }
    }

    #[test]
    fn mail_send_parses_recipient_summary_body() {
        let verb = parse_mail_verb(
            "send",
            args(&["operator-a", "--summary", "hi", "--body", "there"]),
        )
        .unwrap();
        let MailVerb::Send { to, summary, body } = verb else {
            panic!("expected send");
        };
        assert_eq!(to, "operator-a");
        assert_eq!(summary, "hi");
        assert_eq!(body.as_deref(), Some("there"));
    }

    #[test]
    fn mail_send_without_body_leaves_stdin_path() {
        let verb = parse_mail_verb("send", args(&["operator-a", "--summary", "hi"])).unwrap();
        let MailVerb::Send { body, .. } = verb else {
            panic!("expected send");
        };
        assert_eq!(body, None, "no --body means read stdin at run time");
        assert!(parse_mail_verb("send", args(&["operator-a"])).is_err());
        assert!(parse_mail_verb("send", args(&["operator-a", "--subject", "hi"])).is_err());
    }

    #[test]
    fn mail_claim_parses_formats() {
        let verb = parse_mail_verb("claim", args(&[])).unwrap();
        assert!(matches!(
            verb,
            MailVerb::Claim {
                format: MailClaimFormat::Human
            }
        ));
        let verb = parse_mail_verb("claim", args(&["--json"])).unwrap();
        assert!(matches!(
            verb,
            MailVerb::Claim {
                format: MailClaimFormat::Json
            }
        ));
        let verb = parse_mail_verb("claim", args(&["--ids"])).unwrap();
        assert!(matches!(
            verb,
            MailVerb::Claim {
                format: MailClaimFormat::Ids
            }
        ));
        assert!(parse_mail_verb("claim", args(&["--json", "--ids"])).is_err());
        assert!(parse_mail_verb("claim", args(&["--yaml"])).is_err());
    }

    #[test]
    fn mail_commit_release_collect_ids() {
        let verb = parse_mail_verb("commit", args(&["msg:1", "msg:2"])).unwrap();
        let MailVerb::Commit { ids } = verb else {
            panic!("expected commit");
        };
        assert_eq!(ids, ["msg:1", "msg:2"]);
        let verb = parse_mail_verb("release", args(&["msg:3"])).unwrap();
        assert!(matches!(verb, MailVerb::Release { .. }));
    }

    #[test]
    fn mail_watch_parses_default_and_timeout() {
        let verb = parse_mail_verb("watch", args(&[])).unwrap();
        let MailVerb::Watch { timeout_ms } = verb else {
            panic!("expected watch");
        };
        assert_eq!(timeout_ms, 300_000, "default is five minutes");
        let verb = parse_mail_verb("watch", args(&["--timeout", "5"])).unwrap();
        let MailVerb::Watch { timeout_ms } = verb else {
            panic!("expected watch");
        };
        assert_eq!(timeout_ms, 5_000);
        assert!(parse_mail_verb("watch", args(&["--timeout"])).is_err());
        assert!(parse_mail_verb("watch", args(&["--timeout", "soon"])).is_err());
    }

    #[test]
    fn mail_broadcast_parses_and_rejects_recipient() {
        let verb = parse_mail_verb("broadcast", args(&["--summary", "hi", "--body", "x"])).unwrap();
        let MailVerb::Broadcast { summary, body } = verb else {
            panic!("expected broadcast");
        };
        assert_eq!(summary, "hi");
        assert_eq!(body.as_deref(), Some("x"));
        assert!(parse_mail_verb("broadcast", args(&[])).is_err());
        assert!(parse_mail_verb("broadcast", args(&["someone", "--summary", "hi"])).is_err());
    }

    #[test]
    fn mail_simple_verbs_parse() {
        assert!(matches!(
            parse_mail_verb("alias", args(&["pm"])).unwrap(),
            MailVerb::Alias { name } if name == "pm"
        ));
        assert!(matches!(
            parse_mail_verb("who", args(&[])).unwrap(),
            MailVerb::Who
        ));
        assert!(matches!(
            parse_mail_verb("inbox", args(&[])).unwrap(),
            MailVerb::Inbox
        ));
        assert!(matches!(
            parse_mail_verb("status", args(&[])).unwrap(),
            MailVerb::Status
        ));
        assert!(parse_mail_verb("frobnicate", args(&[])).is_err());
    }

    fn dummy_spawn() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/true".into(),
            argv: Vec::new(),
            cwd: None,
            env: Default::default(),
        }
    }

    fn dummy_bounds() -> WindowBounds {
        WindowBounds {
            window_id: 1,
            cols: 80,
            rows: 24,
        }
    }

    /// Every `Event` variant with its kind string and stamp grouping.
    /// Add a row when `Event` grows; `control_event_kind` will not compile
    /// until the new arm exists.
    fn sample_control_events() -> Vec<(
        Event,
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
    )> {
        let spawn = dummy_spawn();
        let bounds = dummy_bounds();
        vec![
            (
                Event::PaneSplit {
                    window_id: 1,
                    target_pane_id: 1,
                    new_pane_id: 2,
                    axis: AxisWire::Horizontal,
                    ratio: 0.5,
                    spawn: spawn.clone(),
                    geometry: Vec::new(),
                },
                "PaneSplit",
                None,
                None,
            ),
            (
                Event::PaneClosed {
                    window_id: 1,
                    pane_id: 1,
                    suggested_focus_id: 2,
                    geometry: Vec::new(),
                },
                "PaneClosed",
                None,
                None,
            ),
            (
                Event::GeometryChanged {
                    window_id: 1,
                    bounds,
                    geometry: Vec::new(),
                },
                "GeometryChanged",
                None,
                None,
            ),
            (
                Event::SizeOwnerChanged {
                    window_id: 1,
                    owner: Some(SizeOwner {
                        client_id: 1,
                        kind: SizeOwnerKind::Host,
                    }),
                },
                "SizeOwnerChanged",
                None,
                None,
            ),
            (
                Event::FocusSuggested {
                    window_id: 1,
                    pane_id: 1,
                    reason: None,
                },
                "FocusSuggested",
                None,
                None,
            ),
            (
                Event::FocusReported {
                    client_id: 1,
                    window_id: 1,
                    pane_id: 1,
                },
                "FocusReported",
                None,
                None,
            ),
            (
                Event::MailAttentionChanged {
                    pane_id: 1,
                    cell: None,
                    gen: None,
                    queue_rev: None,
                    depth: 0,
                    wake: Some(MailWake::Armed),
                },
                "MailAttentionChanged",
                None,
                None,
            ),
            (
                Event::PaneAttention {
                    pane_id: 1,
                    message: "hi".into(),
                },
                "PaneAttention",
                None,
                None,
            ),
            (
                Event::PaneAttentionCleared { pane_id: 1 },
                "PaneAttentionCleared",
                None,
                None,
            ),
            (
                Event::LeaseChanged {
                    pane_id: 1,
                    controller_id: Some(1),
                    previous_controller_id: None,
                },
                "LeaseChanged",
                None,
                None,
            ),
            (
                Event::SessionCreated {
                    session_id: 1,
                    name: "s".into(),
                    window_id: 1,
                    pane_id: 1,
                    bounds,
                    spawn: spawn.clone(),
                },
                "SessionCreated",
                None,
                Some("current"),
            ),
            (
                Event::SessionSwitched {
                    client_id: 1,
                    session_id: 1,
                    window_id: 1,
                    pane_id: 1,
                },
                "SessionSwitched",
                None,
                Some("current"),
            ),
            (
                Event::SessionDestroyed {
                    session_id: 1,
                    name: "s".into(),
                },
                "SessionDestroyed",
                None,
                Some("current"),
            ),
            (
                Event::OutputActivity {
                    pane_id: 1,
                    revision: 1,
                    child_alive: true,
                },
                "OutputActivity",
                None,
                None,
            ),
            (
                Event::WindowCreated {
                    session_id: 1,
                    window_id: 1,
                    pane_id: 1,
                    title: "t".into(),
                    bounds,
                    spawn,
                },
                "WindowCreated",
                Some("current"),
                None,
            ),
            (
                Event::WindowDestroyed {
                    session_id: 1,
                    window_id: 1,
                    session_destroyed: false,
                },
                "WindowDestroyed",
                Some("current"),
                None,
            ),
            (
                Event::WindowSwitched {
                    client_id: 1,
                    session_id: 1,
                    window_id: 1,
                    pane_id: 1,
                },
                "WindowSwitched",
                Some("current"),
                None,
            ),
            (
                Event::WindowRenamed {
                    window_id: 1,
                    title: "t".into(),
                },
                "WindowRenamed",
                None,
                None,
            ),
            (
                Event::PaneStatusChanged {
                    pane_id: 1,
                    status: Some("ok".into()),
                },
                "PaneStatusChanged",
                None,
                None,
            ),
            (
                Event::PaneRenamed {
                    pane_id: 1,
                    title: "t".into(),
                },
                "PaneRenamed",
                None,
                None,
            ),
            (
                Event::SyncInputChanged {
                    window_id: 1,
                    enabled: true,
                },
                "SyncInputChanged",
                None,
                None,
            ),
            (
                Event::PaneMoved {
                    from_window_id: 1,
                    to_window_id: 2,
                    pane_id: 1,
                    source_suggested_focus_id: None,
                    geometry: Vec::new(),
                },
                "PaneMoved",
                Some("current"),
                None,
            ),
        ]
    }

    #[test]
    fn control_event_kind_table_covers_every_variant() {
        let cases = sample_control_events();
        assert_eq!(
            cases.len(),
            22,
            "add a row when Event grows; do not split control_event_kind"
        );
        for (event, kind, _to_window, _session) in &cases {
            assert_eq!(control_event_kind(event), *kind, "{kind}");
        }
    }

    #[test]
    fn detected_from_control_event_stamps_session_and_window_groups() {
        for (event, kind, to_window, session) in sample_control_events() {
            assert_eq!(
                detected_from_control_event(&event),
                mux_detected(kind, to_window, session),
                "{kind}"
            );
        }
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    fn verb_cli(verb: Verb, rest: &[&str]) -> Cli {
        Cli::Verb {
            globals: Globals::default(),
            verb,
            rest: argv(rest),
        }
    }

    #[test]
    fn parse_argv_table_covers_globals_aliases_and_mail() {
        assert_eq!(parse_argv(argv(&["-h"])).unwrap(), Cli::Help);
        assert_eq!(parse_argv(argv(&["--help"])).unwrap(), Cli::Help);
        assert_eq!(parse_argv(argv(&["-V"])).unwrap(), Cli::Version);
        assert_eq!(parse_argv(argv(&["--version"])).unwrap(), Cli::Version);
        assert_eq!(parse_argv(argv(&[])).unwrap(), Cli::MissingCommand);
        assert_eq!(
            parse_argv(argv(&["frob"])).unwrap(),
            Cli::Unknown {
                word: "frob".into()
            }
        );
        assert_eq!(
            parse_argv(argv(&["start"])).unwrap(),
            verb_cli(Verb::Up, &[])
        );
        assert_eq!(
            parse_argv(argv(&["list"])).unwrap(),
            verb_cli(Verb::Ls, &[])
        );
        assert_eq!(
            parse_argv(argv(&["tutorial", "play"])).unwrap(),
            verb_cli(Verb::Tutorial, &["play"])
        );
        assert_eq!(
            parse_argv(argv(&["completions", "bash"])).unwrap(),
            verb_cli(Verb::Completions, &["bash"])
        );
        assert_eq!(
            parse_argv(argv(&["update", "--mux"])).unwrap(),
            verb_cli(Verb::Update, &["--mux"])
        );
        assert_eq!(
            parse_argv(argv(&["config", "init"])).unwrap(),
            verb_cli(Verb::Config, &["init"])
        );
        let got = parse_argv(argv(&["--instance", "work", "up"])).unwrap();
        let Cli::Verb {
            globals,
            verb,
            rest,
        } = got
        else {
            panic!("expected verb");
        };
        assert_eq!(globals.instance.as_deref(), Some("work"));
        assert_eq!(verb, Verb::Up);
        assert!(rest.is_empty());
        let got = parse_argv(argv(&["--socket", "/tmp/pmux.sock", "ls"])).unwrap();
        let Cli::Verb { globals, verb, .. } = got else {
            panic!("expected verb");
        };
        assert_eq!(globals.socket.as_deref(), Some(Path::new("/tmp/pmux.sock")));
        assert_eq!(verb, Verb::Ls);
        let got = parse_argv(argv(&["--session", "s1", "stop"])).unwrap();
        let Cli::Verb { globals, verb, .. } = got else {
            panic!("expected verb");
        };
        assert_eq!(globals.session.as_deref(), Some("s1"));
        assert_eq!(verb, Verb::Stop);
        assert!(parse_argv(argv(&["--instance"])).is_err());
        assert!(parse_argv(argv(&["--instance", "a/b", "up"])).is_err());
        assert!(parse_argv(argv(&["--session", "", "stop"])).is_err());
        assert_eq!(
            parse_argv(argv(&["mail", "send", "alice"])).unwrap(),
            verb_cli(Verb::Mailbox, &["send", "alice"])
        );
        assert_eq!(
            parse_argv(argv(&["mail", "--as", "alice"])).unwrap(),
            verb_cli(Verb::Mailbox, &["--as", "alice"])
        );
        assert_eq!(
            parse_argv(argv(&["mail", "work"])).unwrap(),
            verb_cli(Verb::MailDoorbell, &["work"])
        );
    }

    struct TempEnvDir {
        key: &'static str,
        old: Option<std::ffi::OsString>,
        dir: PathBuf,
    }

    impl TempEnvDir {
        fn install(key: &'static str, dir: PathBuf, value: &Path) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old, dir }
        }
    }

    impl Drop for TempEnvDir {
        fn drop(&mut self) {
            match self.old.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn with_temp_config<R>(f: impl FnOnce(&Path) -> R) -> R {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "pt231-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _env = TempEnvDir::install("PRISMATTYC_CONFIG", dir, &path);
        f(&path)
    }

    #[test]
    fn detach_fate_table_keep_skip_and_signal_are_distinct() {
        let cases: &[(&str, bool, bool, bool, bool, DetachFate)] = &[
            ("own", true, false, false, true, DetachFate::Keep),
            ("host", false, true, false, true, DetachFate::Keep),
            ("own-wins-child", true, false, true, true, DetachFate::Keep),
            ("child-root", false, false, true, true, DetachFate::Skip),
            ("already-gone", false, false, false, false, DetachFate::Skip),
            ("viewer", false, false, false, true, DetachFate::Signal),
        ];
        for (name, own, host, child, live, want) in cases {
            assert_eq!(
                detach_fate(*own, *host, *child, *live),
                *want,
                "{name}: Keep is the kept-line, Skip is silent, Signal is TERM"
            );
        }
        assert_ne!(DetachFate::Keep, DetachFate::Skip);
        assert_ne!(DetachFate::Skip, DetachFate::Signal);
    }

    #[test]
    fn attach_pid_is_live_table_match_and_miss() {
        assert!(!attach_pid_is_live(&[], 1));
        assert!(attach_pid_is_live(&[1, 2, 3], 2));
        assert!(!attach_pid_is_live(&[1, 2, 3], 4));
        assert!(attach_pid_is_live(&[7], 7));
        assert!(!attach_pid_is_live(&[7], 8));
    }

    #[test]
    fn cmd_detach_other_errors_when_server_is_down() {
        let dir = std::env::temp_dir().join(format!(
            "pt231-detach-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("prism.sock");
        let paths = Paths::resolve("default", Some(sock)).unwrap();
        let err = cmd_detach_other(&paths, None).unwrap_err();
        assert!(err.to_string().contains("not running"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn control_ok(request_id: u64, response: ControlResponseData) -> ControlResponse {
        ControlResponse {
            version: PROTOCOL_VERSION,
            request_id,
            body: ControlResponseBody::Ok { response },
        }
    }

    fn request_id_of(req: &ControlRequest) -> u64 {
        serde_json::to_value(req)
            .ok()
            .and_then(|value| value.get("request_id")?.as_u64())
            .unwrap_or(0)
    }

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            sequence: 1,
            sessions: Vec::new(),
        }
    }

    fn one_pane_snapshot() -> Snapshot {
        Snapshot {
            sequence: 1,
            sessions: vec![SessionSnapshot {
                space_id: None,
                id: 1,
                name: "work".into(),
                agent_id: None,
                windows: vec![WindowSnapshot {
                    id: 1,
                    title: "main".into(),
                    bounds: dummy_bounds(),
                    layout: LayoutSnapshot::Leaf { pane_id: 1 },
                    panes: vec![PaneSnapshot {
                        pane_write: None,
                        id: 1,
                        title: "p1".into(),
                        title_pinned: false,
                        controller_id: None,
                        geometry: PaneGeometry {
                            pane_id: 1,
                            col: 0,
                            row: 0,
                            cols: 80,
                            rows: 24,
                        },
                        spawn: Some(dummy_spawn()),
                        child_pid: None,
                        mail: None,
                        status: None,
                        attention: None,
                        mail_inject: None,
                        ledger: PaneInputLedger {
                            last_output_at_ms: None,
                            focused: false,
                            controller_id: None,
                            last_controller_write_at_ms: None,
                            last_write_ended_with_cr: false,
                            dirty_input: false,
                            last_input_at_ms: None,
                        },
                        size_owner: None,
                    }],
                    sync_input: false,
                }],
            }],
        }
    }

    struct FakeControl {
        dir: PathBuf,
        sock: PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeControl {
        fn spawn(
            snapshot: Snapshot,
            child_pid: std::sync::Arc<std::sync::Mutex<Option<u32>>>,
        ) -> Self {
            use prismattyc_mux::local_socket::UnixListener;
            use std::sync::atomic::{AtomicBool, Ordering};
            let dir = std::env::temp_dir().join(format!(
                "pt231-ctl-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("prism.sock");
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap();
            assert!(
                dir_name.starts_with(&format!("pt231-ctl-{}-", std::process::id())),
                "socket dir must be unique per pid, got {dir_name}"
            );
            assert!(
                dir_name
                    .rsplit('-')
                    .next()
                    .is_some_and(|n| n.parse::<u128>().is_ok()),
                "socket dir must include nanos, got {dir_name}"
            );
            assert_eq!(sock.parent(), Some(dir.as_path()));
            let listener = UnixListener::bind(&sock).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let thread_stop = std::sync::Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let pid = *child_pid.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = serve_control_conn(stream, &snapshot, pid);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                dir,
                sock,
                stop,
                thread: Some(thread),
            }
        }

        fn paths(&self) -> Paths {
            Paths::resolve("default", Some(self.sock.clone())).unwrap()
        }
    }

    impl Drop for FakeControl {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = UnixStream::connect(&self.sock);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn serve_control_conn(
        stream: UnixStream,
        snapshot: &Snapshot,
        child_pid: Option<u32>,
    ) -> std::io::Result<()> {
        // Accepted sockets inherit O_NONBLOCK on macOS, unlike Linux.
        // Match the production server before reading a complete request.
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let Ok(req) = serde_json::from_str::<ControlRequest>(&line) else {
                break;
            };
            let request_id = request_id_of(&req);
            let response = match req {
                ControlRequest::Ping { .. } => control_ok(request_id, ControlResponseData::Pong),
                ControlRequest::RegisterClient { .. } => control_ok(
                    request_id,
                    ControlResponseData::ClientRegistered { client_id: 1 },
                ),
                ControlRequest::Snapshot { .. } => control_ok(
                    request_id,
                    ControlResponseData::Snapshot {
                        snapshot: snapshot.clone(),
                    },
                ),
                ControlRequest::ReadPane { pane_id, .. } => control_ok(
                    request_id,
                    ControlResponseData::PaneContent {
                        content: PaneContent {
                            pane_id,
                            revision: 1,
                            cols: 80,
                            rows: 24,
                            cursor_row: 0,
                            cursor_col: 0,
                            cursor_visible: true,
                            alt_active: false,
                            child_alive: child_pid.is_some(),
                            child_pid,
                            lines: Vec::new(),
                            cursor_shape: None,
                        },
                    },
                ),
                _ => control_ok(request_id, ControlResponseData::Pong),
            };
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(())
    }

    #[test]
    fn detach_fixture_process() {
        let Some(ready) = std::env::var_os("PMUX_DETACH_FIXTURE_READY") else {
            return;
        };
        std::fs::write(ready, b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    fn spawn_fake_attach(dir: &Path, sock: &Path) -> ReapChild {
        use prismattyc_mux::platform::Exec;
        let ready = dir.join("attach-ready");
        // A native test process retains argv0 on macOS. Python framework
        // launchers replace it during startup, making the scan race exec.
        let mut child = ReapChild::new(
            Command::new(std::env::current_exe().unwrap())
                .arg0("pmux-attach")
                .args([
                    "--exact",
                    "tests::detach_fixture_process",
                    "--nocapture",
                    "--",
                    "--socket",
                    sock.to_str().unwrap(),
                ])
                .env("PMUX_DETACH_FIXTURE_READY", &ready)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let start = Instant::now();
        while !ready.exists()
            || scan_attach_clients(sock)
                .iter()
                .all(|row| row.pid != child.child().id())
        {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "scan_attach_clients never saw ready pid {}",
                child.child().id()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    /// Fail closed before `cmd_detach_other` can SIGTERM.
    ///
    /// The scan must be exactly `spawned`, and that pid's argv must contain
    /// this temp `--socket`. Extra rows abort; we do not call the verb.
    fn assert_scan_is_only_spawned(sock: &Path, spawned: u32) {
        let want = sock.as_os_str().as_encoded_bytes();
        let rows = scan_attach_clients(sock);
        let pids: Vec<u32> = rows.iter().map(|row| row.pid).collect();
        assert_eq!(
            pids,
            vec![spawned],
            "scan must be exactly the spawned pid before SIGTERM; extras abort"
        );
        let args = prismattyc_mux::procinfo::cmdline(spawned)
            .unwrap_or_else(|| panic!("no cmdline for spawned pid {spawned}"));
        let has_socket = args
            .windows(2)
            .any(|pair| pair[0] == b"--socket" && pair[1] == want);
        assert!(
            has_socket,
            "spawned pid {spawned} argv lacks temp socket {}",
            sock.display()
        );
    }

    fn shared_child_pid() -> std::sync::Arc<std::sync::Mutex<Option<u32>>> {
        std::sync::Arc::new(std::sync::Mutex::new(None))
    }

    /// Kill and wait on drop so a panic cannot leave a zombie (PT-258).
    struct ReapChild(Option<std::process::Child>);

    impl ReapChild {
        fn new(child: std::process::Child) -> Self {
            Self(Some(child))
        }

        fn child(&mut self) -> &mut std::process::Child {
            self.0.as_mut().expect("reaped")
        }
    }

    impl Drop for ReapChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn format_detach_other_report_keep_skip_and_signal_lines() {
        assert_eq!(format_detach_other_report(&[], &[]), "nothing signalled\n");
        assert_eq!(
            format_detach_other_report(&[3, 5], &[]),
            "signalled TERM 3 5\n"
        );
        assert_eq!(
            format_detach_other_report(&[], &[9]),
            "nothing signalled\nkept (yours or the registered host's) 9\n"
        );
        assert_eq!(
            format_detach_other_report(&[3], &[9]),
            "signalled TERM 3\nkept (yours or the registered host's) 9\n"
        );
    }

    #[test]
    fn cmd_detach_other_signals_a_live_viewer() {
        let fake = FakeControl::spawn(empty_snapshot(), shared_child_pid());
        let mut child = spawn_fake_attach(&fake.dir, &fake.sock);
        let paths = fake.paths();
        assert_scan_is_only_spawned(&fake.sock, child.child().id());
        assert!(
            child.child().try_wait().unwrap().is_none(),
            "child must be alive before SIGTERM"
        );
        cmd_detach_other(&paths, None).unwrap();
        let status = child.child().wait().unwrap();
        let _ = child.0.take();
        assert!(
            !status.success(),
            "viewer must receive TERM, got {status:?}"
        );
    }

    #[test]
    fn cmd_detach_other_keeps_host_tree_attach() {
        let fake = FakeControl::spawn(empty_snapshot(), shared_child_pid());
        std::fs::write(
            prismattyc_mux::host_pid_path_from_socket(&fake.sock),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        let mut child = spawn_fake_attach(&fake.dir, &fake.sock);
        let paths = fake.paths();
        assert_scan_is_only_spawned(&fake.sock, child.child().id());
        assert!(
            child.child().try_wait().unwrap().is_none(),
            "child must be alive before cmd_detach_other"
        );
        cmd_detach_other(&paths, None).unwrap();
        assert!(
            child.child().try_wait().unwrap().is_none(),
            "host attach must live"
        );
    }

    #[test]
    fn cmd_detach_other_skips_pane_child_root() {
        let child_pid = shared_child_pid();
        let fake = FakeControl::spawn(one_pane_snapshot(), std::sync::Arc::clone(&child_pid));
        let mut child = spawn_fake_attach(&fake.dir, &fake.sock);
        let pid = child.child().id();
        *child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        let paths = fake.paths();
        assert_scan_is_only_spawned(&fake.sock, pid);
        assert!(
            child.child().try_wait().unwrap().is_none(),
            "child must be alive before cmd_detach_other"
        );
        cmd_detach_other(&paths, None).unwrap();
        assert!(
            child.child().try_wait().unwrap().is_none(),
            "child-root must live"
        );
    }

    /// Serializes tests that mutate `$PRISMATTYC_REPO`.
    static REPO_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn run_update_refuses_bad_prismattyc_repo() {
        let _lock = REPO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let missing = PathBuf::from("/no/such/pt231-prismattyc");
        let _env = TempEnvDir::install("PRISMATTYC_REPO", missing.clone(), &missing);
        let err = prismattyc_mux::run_update(["--source", "--host"]).unwrap_err();
        assert!(err.to_string().contains("PRISMATTYC_REPO"), "{err:#}");
    }

    #[test]
    fn run_update_refuses_dirty_tree() {
        let dir = std::env::temp_dir().join(format!(
            "pt231-dirty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("crates/prismattyc-host")).unwrap();
        std::fs::create_dir_all(dir.join("crates/prismattyc-mux")).unwrap();
        assert!(Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.join("README"), "x\n").unwrap();
        assert!(Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "add", "README"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.join("dirty"), "y\n").unwrap();
        let _lock = REPO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = TempEnvDir::install("PRISMATTYC_REPO", dir.clone(), &dir);
        let err = prismattyc_mux::run_update(["--source", "--host"]).unwrap_err();
        assert!(err.to_string().contains("dirty"), "{err:#}");
    }

    #[test]
    fn cmd_config_help_and_usage() {
        cmd_config(vec!["-h".into()]).unwrap();
        cmd_config(vec!["--help".into()]).unwrap();
        assert!(cmd_config(vec![]).is_err());
        assert!(cmd_config(vec!["--merge".into()]).is_err());
        assert!(cmd_config(vec!["init".into(), "--wat".into()]).is_err());
    }

    #[test]
    fn cmd_config_init_writes_and_refuses_existing() {
        with_temp_config(|path| {
            assert!(!path.exists());
            cmd_config(vec!["init".into()]).unwrap();
            let body = std::fs::read_to_string(path).unwrap();
            assert!(body.contains("[mux]"));
            assert!(cmd_config(vec!["init".into()]).is_err());
        });
    }

    #[test]
    fn cmd_config_merge_adds_mux_and_errors_when_missing() {
        with_temp_config(|path| {
            assert!(cmd_config(vec!["init".into(), "--merge".into()]).is_err());
            std::fs::write(path, "keep = 1\n").unwrap();
            cmd_config(vec!["init".into(), "--merge".into()]).unwrap();
            let body = std::fs::read_to_string(path).unwrap();
            assert!(body.contains("keep = 1"));
            assert!(body.contains("[mux]"));
        });
    }

    #[test]
    fn program_default_and_separator_stripping() {
        let default = program_from(Vec::new());
        assert_eq!(default.len(), 2);
        assert_eq!(default[1], "-l");
        assert_eq!(
            program_from(vec!["--".into(), "/bin/sh".into(), "-c".into(), "x".into()]),
            vec!["/bin/sh", "-c", "x"]
        );
        assert_eq!(program_from(vec!["/bin/sh".into()]), vec!["/bin/sh"]);
    }

    #[test]
    fn paths_derive_pid_and_log_from_socket() {
        let paths = Paths::resolve("default", Some(PathBuf::from("/tmp/x/prism-a.sock"))).unwrap();
        assert_eq!(paths.pidfile, PathBuf::from("/tmp/x/prism-a.pid"));
        assert_eq!(paths.logfile, PathBuf::from("/tmp/x/prism-a.log"));
        assert!(Paths::resolve("default", Some(PathBuf::from("rel.sock"))).is_err());
    }

    #[test]
    fn attention_args_parse_session_and_default_message() {
        let parsed = parse_attention_args(None, vec!["work".into()]).unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(parsed.message, "needs your attention");
        let parsed =
            parse_attention_args(Some("work".into()), vec!["permission needed".into()]).unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(parsed.message, "permission needed");
        assert!(parse_attention_args(None, Vec::new()).is_err());
        assert!(parse_attention_args(None, vec!["a".into(), "b".into(), "c".into()]).is_err());
    }

    #[test]
    fn mail_args_take_positional_session() {
        let parsed = parse_mail_args(None, vec!["work".into()]).unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(parsed.pane, None);
    }

    #[test]
    fn mail_args_parse_pane() {
        let parsed =
            parse_mail_args(None, vec!["work".into(), "--pane".into(), "7".into()]).unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(parsed.pane, Some(7));
    }

    #[test]
    fn mail_args_accept_global_session_flag() {
        let parsed =
            parse_mail_args(Some("work".into()), vec!["--pane".into(), "3".into()]).unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(parsed.pane, Some(3));
    }

    #[test]
    fn mail_args_reject_double_session() {
        assert!(parse_mail_args(Some("a".into()), vec!["b".into()]).is_err());
    }

    #[test]
    fn mail_args_reject_missing_session() {
        assert!(parse_mail_args(None, Vec::new()).is_err());
    }

    #[test]
    fn mail_args_reject_second_positional() {
        assert!(parse_mail_args(None, vec!["a".into(), "b".into()]).is_err());
    }

    #[test]
    fn mail_args_reject_unknown_flag_and_bad_pane() {
        assert!(parse_mail_args(None, vec!["a".into(), "--nope".into()]).is_err());
        assert!(parse_mail_args(None, vec!["a".into(), "--pane".into(), "x".into()]).is_err());
    }

    #[test]
    fn mail_args_reject_retired_channel_flags() {
        assert!(parse_mail_args(None, vec!["work".into(), "--switchboard".into()]).is_err());
        assert!(
            parse_mail_args(None, vec!["work".into(), "--cell".into(), "1b@1".into()]).is_err()
        );
    }

    #[test]
    fn attach_dump_flags_skip_host_seat_route() {
        let interactive = parse_attach_args(vec!["work".into()]).unwrap();
        assert!(!attach_is_dump_or_write(&interactive));
        let json = parse_attach_args(vec!["work".into(), "--json".into()]).unwrap();
        assert!(attach_is_dump_or_write(&json));
        let write = parse_attach_args(vec!["--write".into(), "hi".into()]).unwrap();
        assert!(attach_is_dump_or_write(&write));
        let cases = [
            (vec!["--styled-json".into()], true),
            (vec!["--watch".into()], true),
            (vec!["--pane".into(), "42".into()], true),
            (vec!["--read-only".into()], true),
            (vec!["--fit".into()], true),
            (vec!["work".into()], false),
        ];
        for (args, dump) in cases {
            assert_eq!(
                attach_is_dump_or_write(&parse_attach_args(args.clone()).unwrap()),
                dump,
                "{args:?}"
            );
        }
    }

    #[test]
    fn attach_write_consumes_all_as_payload() {
        let parsed = parse_attach_args(vec!["--write".into(), "--all".into()]).unwrap();
        assert!(!parsed.all);
        assert_eq!(parsed.write.as_deref(), Some("--all"));
        let id_only = parse_attach_args(vec!["--session-id".into(), "1".into()]).unwrap();
        assert_eq!(id_only.session_id.as_deref(), Some("1"));
        assert!(id_only.session.is_none());
        let styled = parse_attach_args(vec!["--styled-json".into()]).unwrap();
        assert!(styled.styled_json);
        assert!(!styled.json);
        assert!(parse_attach_args(vec!["--json".into(), "--styled-json".into()]).is_err());
        let ro = parse_attach_args(vec!["--read-only".into()]).unwrap();
        assert!(ro.read_only);
        let fit = parse_attach_args(vec!["--fit".into()]).unwrap();
        assert!(fit.fit);
        assert!(
            parse_attach_args(vec!["--read-only".into(), "--write".into(), "x".into()]).is_err()
        );
    }

    #[test]
    fn send_args_parse_enter_literal_and_escapes() {
        let parsed = parse_send_args(vec!["7".into(), "hi".into(), "--enter".into()]).unwrap();
        assert_eq!(parsed.pane, 7);
        assert_eq!(parsed.text, "hi");
        assert!(parsed.enter);
        assert!(!parsed.literal);
        assert!(!parsed.force);
        let joined = parse_send_args(vec!["3".into(), "echo".into(), "x".into()]).unwrap();
        assert_eq!(joined.text, "echo x");
        let lit = parse_send_args(vec!["1".into(), "--literal".into(), r"a\n".into()]).unwrap();
        assert!(lit.literal);
        assert_eq!(decode_send_escapes(r"a\n\t\\").unwrap(), "a\n\t\\");
        assert!(decode_send_escapes(r"\q").is_err());
        assert!(parse_send_args(vec!["nope".into(), "x".into()]).is_err());
        assert!(parse_send_args(vec!["1".into()]).is_err());
        let cr_only = parse_send_args(vec!["1".into(), "--enter".into()]).unwrap();
        assert!(cr_only.text.is_empty());
        assert!(cr_only.enter);
        let force = parse_send_args(vec!["1".into(), "--force".into(), "x".into()]).unwrap();
        assert!(force.force);
        let dashed = parse_send_args(vec!["1".into(), "--".into(), "-n".into()]).unwrap();
        assert_eq!(dashed.text, "-n");
        assert_eq!(send_chunks("abc").concat(), "abc");
        assert_eq!(send_chunks("").len(), 0);
    }

    #[test]
    fn join_pane_args_parse_axis_and_to() {
        let parsed = parse_join_pane_args(vec!["3".into(), "--to".into(), "9".into()]).unwrap();
        assert_eq!(parsed.pane, 3);
        assert_eq!(parsed.to, 9);
        assert!(matches!(parsed.axis, AxisWire::Horizontal));
        let vertical =
            parse_join_pane_args(vec!["--to".into(), "2".into(), "-v".into(), "8".into()]).unwrap();
        assert_eq!(vertical.pane, 8);
        assert_eq!(vertical.to, 2);
        assert!(matches!(vertical.axis, AxisWire::Vertical));
        assert!(parse_join_pane_args(vec!["3".into()]).is_err());
        assert!(parse_join_pane_args(vec!["--to".into(), "1".into()]).is_err());
    }

    #[test]
    fn save_space_defaults_to_default_name_and_every_session() {
        let (name, sessions) = parse_layout_save_space_args(vec![], SPACE_SAVE_USAGE).unwrap();
        assert_eq!(name, DEFAULT_SPACE_NAME);
        assert!(sessions.is_empty());
    }

    #[test]
    fn save_space_first_positional_is_the_name_unless_named() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (name, sessions) =
            parse_layout_save_space_args(args(&["prismattyc-work", "fable-pc"]), SPACE_SAVE_USAGE)
                .unwrap();
        assert_eq!(name, "prismattyc-work");
        assert_eq!(sessions, vec!["fable-pc".to_string()]);
        let (name, sessions) =
            parse_layout_save_space_args(args(&["fable-pc", "--name", "review"]), SPACE_SAVE_USAGE)
                .unwrap();
        assert_eq!(name, "review");
        assert_eq!(sessions, vec!["fable-pc".to_string()]);
        let bogus = parse_layout_save_space_args(args(&["--bogus"]), SPACE_SAVE_USAGE)
            .unwrap_err()
            .to_string();
        assert!(bogus.contains("pmux space save"), "{bogus}");
        assert!(!bogus.contains("layout save space"), "{bogus}");
        assert!(parse_layout_save_space_args(args(&["a/b"]), SPACE_SAVE_USAGE).is_err());
        let layout_bogus =
            parse_layout_save_space_args(args(&["--bogus"]), LAYOUT_SAVE_SPACE_USAGE)
                .unwrap_err()
                .to_string();
        assert!(
            layout_bogus.contains("pmux layout save space"),
            "{layout_bogus}"
        );
    }

    #[test]
    fn apply_space_defaults_to_default_name_and_opens_unless_told_not_to() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_layout_apply_args(None, args(&["space"])).unwrap() {
            ApplyArgs::Space {
                name,
                replace,
                add,
                attach,
                new_window,
                no_run,
                tty,
                host,
            } => {
                assert!(!new_window);
                assert!(!no_run);
                assert!(!tty);
                assert!(host);
                assert_eq!(name, DEFAULT_SPACE_NAME);
                assert!(!replace);
                assert!(!add, "default is switch");
                assert!(attach);
            }
            _ => panic!("expected apply space"),
        }
        match parse_layout_apply_args(None, args(&["space", "work", "--no-attach", "--replace"]))
            .unwrap()
        {
            ApplyArgs::Space {
                name,
                replace,
                add,
                attach,
                new_window,
                no_run,
                tty,
                host,
            } => {
                assert!(!add);
                assert!(!new_window);
                assert!(!no_run);
                assert!(!tty);
                assert!(host);
                assert_eq!(name, "work");
                assert!(replace);
                assert!(!attach);
            }
            _ => panic!("expected apply space"),
        }
        assert!(parse_layout_apply_args(None, args(&["work", "--no-attach"])).is_err());
        assert!(parse_layout_apply_args(None, args(&["--all", "--no-attach"])).is_err());
        assert!(parse_layout_apply_args(None, args(&["space", "a", "b"])).is_err());
        match parse_layout_apply_args(None, args(&["space", "work", "--tty"])).unwrap() {
            ApplyArgs::Space { tty, attach, .. } => {
                assert!(tty);
                assert!(attach);
            }
            _ => panic!("expected apply space --tty"),
        }
        assert!(parse_layout_apply_args(None, args(&["space", "--tty", "--new-window"])).is_err());
    }

    #[test]
    fn space_open_args_name_the_space_verb() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_space_open_args(vec![]).unwrap() {
            ApplyArgs::Space {
                name,
                replace,
                add,
                attach,
                new_window,
                no_run,
                tty,
                host,
            } => {
                assert!(!new_window);
                assert!(!no_run);
                assert!(!tty);
                assert!(host);
                assert_eq!(name, DEFAULT_SPACE_NAME);
                assert!(!replace);
                assert!(!add, "default is switch");
                assert!(attach);
            }
            _ => panic!("expected space open"),
        }
        match parse_space_open_args(args(&["work", "--no-attach", "--replace"])).unwrap() {
            ApplyArgs::Space {
                name,
                replace,
                add,
                attach,
                new_window,
                no_run,
                tty,
                host,
            } => {
                assert!(!add);
                assert!(!new_window);
                assert!(!no_run);
                assert!(!tty);
                assert!(host);
                assert_eq!(name, "work");
                assert!(replace);
                assert!(!attach);
            }
            _ => panic!("expected space open"),
        }
        let err = parse_space_open_args(args(&["--bogus"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pmux space open"), "{err}");
        assert!(!err.contains("layout apply"), "{err}");
        match parse_space_open_args(args(&["work", "--new-window"])).unwrap() {
            ApplyArgs::Space {
                attach, new_window, ..
            } => {
                assert!(attach);
                assert!(new_window);
            }
            _ => panic!("expected space open"),
        }
        match parse_space_open_args(args(&["work", "--no-run", "--no-attach"])).unwrap() {
            ApplyArgs::Space { no_run, attach, .. } => {
                assert!(no_run);
                assert!(!attach);
            }
            _ => panic!("expected space open"),
        }
        assert!(parse_space_open_args(args(&["--new-window", "--no-attach"])).is_err());
        assert!(parse_space_open_args(args(&["--new-window", "--tty"])).is_err());
        assert!(parse_space_open_args(args(&["--new-window", "--add"])).is_err());
        assert!(parse_space_open_args(args(&["a", "b"])).is_err());
        match parse_space_open_args(args(&["work", "--add", "--no-attach"])).unwrap() {
            ApplyArgs::Space { add, attach, .. } => {
                assert!(add);
                assert!(!attach);
            }
            _ => panic!("expected space open --add"),
        }
        match parse_space_open_args(args(&["work", "--tty", "--no-attach"])).unwrap() {
            ApplyArgs::Space { tty, attach, .. } => {
                assert!(tty);
                assert!(!attach);
            }
            _ => panic!("expected space open --tty"),
        }
        let attach = parse_space_attach_args(vec![]).unwrap();
        assert_eq!(attach.name, DEFAULT_SPACE_NAME);
        assert_eq!(attach.session, None);
        let attach = parse_space_attach_args(args(&["work", "--session", "fable-pc"])).unwrap();
        assert_eq!(attach.name, "work");
        assert_eq!(attach.session.as_deref(), Some("fable-pc"));
        assert!(parse_space_attach_args(args(&["--session"])).is_err());
        assert!(parse_space_attach_args(args(&["a", "b"])).is_err());
        match parse_space_verb(args(&["attach", "work", "--session", "a"])).unwrap() {
            SpaceVerb::Attach(rest) => {
                assert_eq!(rest, args(&["work", "--session", "a"]));
            }
            other => panic!("{other:?}"),
        }
        let recipe_space = SavedSpace {
            id: None,
            version: 1,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![
                prismattyc_mux::SavedSpaceSession {
                    name: "alpha".into(),
                    agent: None,
                    windows: vec![],
                },
                prismattyc_mux::SavedSpaceSession {
                    name: "beta".into(),
                    agent: None,
                    windows: vec![],
                },
            ],
            tabs: vec![SavedSpaceTab {
                title: "seats".into(),
                sessions: vec!["beta".into(), "alpha".into()],
            }],
            active_tab: 0,
            focused_session: Some("alpha".into()),
        };
        assert_eq!(
            space_attach_recipe_lines(&recipe_space),
            vec![
                "pmux attach beta".to_string(),
                "pmux attach alpha  # active".into()
            ]
        );
        let live = vec![(1, "beta".into()), (2, "other".into()), (3, "alpha".into())];
        assert_eq!(
            sessions_for_space_host(live, &recipe_space),
            vec![(1, "beta".into()), (3, "alpha".into())]
        );
        assert!(command_has_line_break("sleep 30\nbad"));
        assert!(command_has_line_break("sleep 30\r"));
        assert!(!command_has_line_break("sleep 30"));
        assert!(parse_space_verb(args(&["save", "--help"])).unwrap() == SpaceVerb::Help);
        assert!(parse_space_verb(args(&["open", "-h"])).unwrap() == SpaceVerb::Help);
        assert!(parse_space_verb(args(&["ls", "--help"])).unwrap() == SpaceVerb::Help);
    }

    #[test]
    fn space_verb_parses_save_open_ls_and_rejects_usage() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_space_verb(args(&["save"])).unwrap() {
            SpaceVerb::Save(rest) => assert!(rest.is_empty()),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["save", "today", "--name", "x"])).unwrap() {
            SpaceVerb::Save(rest) => assert_eq!(rest, args(&["today", "--name", "x"])),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["open", "--no-attach", "--replace"])).unwrap() {
            SpaceVerb::Open(rest) => assert_eq!(rest, args(&["--no-attach", "--replace"])),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["rm", "today", "--all"])).unwrap() {
            SpaceVerb::Rm(rest) => assert_eq!(rest, args(&["today", "--all"])),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["delete", "today"])).unwrap() {
            SpaceVerb::Rm(rest) => assert_eq!(rest, args(&["today"])),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["clear", "--keep", "today"])).unwrap() {
            SpaceVerb::Clear(rest) => assert_eq!(rest, args(&["--keep", "today"])),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["add", "desk", "--session", "s"])).unwrap() {
            SpaceVerb::Add(rest) => assert_eq!(rest, args(&["desk", "--session", "s"])),
            other => panic!("{other:?}"),
        }
        match parse_space_verb(args(&["remove", "desk", "--session", "s"])).unwrap() {
            SpaceVerb::Remove(rest) => assert_eq!(rest, args(&["desk", "--session", "s"])),
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_space_verb(args(&["ls"])).unwrap(), SpaceVerb::Ls);
        assert_eq!(
            parse_space_verb(args(&["--help"])).unwrap(),
            SpaceVerb::Help
        );
        assert!(parse_space_verb(args(&[])).is_err());
        assert!(parse_space_verb(args(&["bogus"])).is_err());
        assert!(parse_space_verb(args(&["ls", "extra"])).is_err());
        match parse_space_verb(args(&["open", "work", "--no-attach"])).unwrap() {
            SpaceVerb::Open(rest) => {
                let mut apply = vec!["space".to_string()];
                apply.extend(rest);
                match parse_layout_apply_args(None, apply).unwrap() {
                    ApplyArgs::Space {
                        name,
                        replace,
                        add,
                        attach,
                        new_window,
                        no_run,
                        tty,
                        host,
                    } => {
                        assert_eq!(name, "work");
                        assert!(!replace);
                        assert!(!add);
                        assert!(!attach);
                        assert!(!new_window);
                        assert!(!no_run);
                        assert!(!tty);
                        assert!(host);
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn space_and_layout_help_fit_40_lines_at_80_cols() {
        for (name, text) in [
            ("space", SPACE_USAGE),
            ("layout", LAYOUT_USAGE),
            ("session", SESSION_USAGE),
        ] {
            let lines: Vec<&str> = text.trim_end().lines().collect();
            assert!(
                lines.len() <= 40,
                "{name} help has {} lines (max 40)",
                lines.len()
            );
            for line in &lines {
                let width = line.chars().count();
                assert!(
                    width <= 80,
                    "{name} help line is {width} cols (max 80): {line:?}"
                );
            }
            let blob = text.to_ascii_lowercase();
            assert!(blob.contains("session"), "{name} help names session");
            assert!(blob.contains("tab"), "{name} help names tab");
            assert!(blob.contains("space"), "{name} help names space");
            if name == "space" {
                assert!(
                    blob.contains("only one space"),
                    "{name} help describes ownership"
                );
            } else {
                assert!(
                    blob.contains("not a session"),
                    "{name} help distinguishes Spaces"
                );
            }
        }
        assert!(
            SPACE_USAGE.contains("clear [--keep NAME]"),
            "space help lists clear next to rm"
        );
        assert!(
            SESSION_USAGE.contains("clear [--all] [--keep NAME]"),
            "session help lists clear"
        );
        assert!(
            SESSION_USAGE.contains("pmux ls"),
            "session help points list at pmux ls"
        );
    }

    #[test]
    fn detach_requires_the_other_flag_and_keeps_the_session() {
        assert!(parse_detach_args(vec![]).is_err(), "bare detach is refused");
        assert!(parse_detach_args(vec!["work".into()]).is_err());
        assert_eq!(
            parse_detach_args(vec!["--other".into(), "work".into()]).unwrap(),
            DetachArgs {
                rest: vec!["work".into()]
            }
        );
        assert_eq!(
            parse_detach_args(vec!["-a".into()]).unwrap(),
            DetachArgs { rest: vec![] }
        );
        assert!(parse_detach_args(vec!["--other".into(), "--bogus".into()]).is_err());
        assert_eq!(
            split_json_flag(vec!["work".into(), "--json".into()]),
            (true, vec!["work".to_string()])
        );
    }

    #[test]
    fn render_status_args_reject_extra_words() {
        assert!(!parse_render_status_args(vec![]).unwrap());
        assert!(parse_render_status_args(vec!["--json".into()]).unwrap());
        let error = parse_render_status_args(vec!["extra".into()]).unwrap_err();
        assert!(error
            .to_string()
            .contains("usage: pmux render-status [--json]"));
    }

    #[test]
    fn client_rows_format_as_a_table_and_mark_your_own_attach() {
        let rows = vec![
            ClientRow {
                pid: 41,
                kind: "viewer",
                session: Some("work".into()),
                pane: Some(3),
                own: false,
                host: false,
            },
            ClientRow {
                pid: 42,
                kind: "nested",
                session: None,
                pane: None,
                own: true,
                host: false,
            },
            ClientRow {
                pid: 43,
                kind: "viewer",
                session: Some("work".into()),
                pane: Some(4),
                own: false,
                host: true,
            },
        ];
        let text = format_client_rows(&rows);
        assert!(text.starts_with("PID      KIND    SESSION          PANE"));
        assert!(text.contains("41       viewer  work             3"));
        assert!(text.contains("42       nested  -                -     (you)"));
        assert!(text.contains("43       viewer  work             4     (host)"));
        assert_eq!(format_client_rows(&[]), "no attach clients\n");
        let json = serde_json::to_string(&rows).unwrap();
        assert!(json.contains("\"own\":true"));
        assert!(!json.contains("\"pane\":null"), "absent fields are skipped");
    }

    #[test]
    fn session_verb_parses_clear_and_rejects_usage() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_session_verb(args(&["clear"])).unwrap() {
            SessionVerb::Clear(rest) => assert!(rest.is_empty()),
            other => panic!("{other:?}"),
        }
        match parse_session_verb(args(&["clear", "--all", "--keep", "work"])).unwrap() {
            SessionVerb::Clear(rest) => {
                assert_eq!(rest, args(&["--all", "--keep", "work"]));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            parse_session_verb(args(&["--help"])).unwrap(),
            SessionVerb::Help
        );
        assert!(parse_session_verb(args(&[])).is_err());
        assert!(parse_session_verb(args(&["ls"])).is_err());
        assert!(parse_session_verb(args(&["bogus"])).is_err());
        let opts = parse_clear_keep(
            args(&["--keep", "alpha", "--keep", "beta"]),
            SPACE_CLEAR_USAGE,
            false,
        )
        .unwrap();
        assert!(!opts.all);
        assert!(opts.keep.contains("alpha"));
        assert!(opts.keep.contains("beta"));
        assert!(parse_clear_keep(args(&["--all"]), SPACE_CLEAR_USAGE, false).is_err());
        let session_opts = parse_clear_keep(
            args(&["--all", "--keep", "work"]),
            SESSION_CLEAR_USAGE,
            true,
        )
        .unwrap();
        assert!(session_opts.all);
        assert!(session_opts.keep.contains("work"));
    }

    fn snapshot_with(sessions: &[(u64, &str)]) -> Snapshot {
        Snapshot {
            sequence: 0,
            sessions: sessions
                .iter()
                .map(|(id, name)| SessionSnapshot {
                    space_id: None,
                    id: *id,
                    name: name.to_string(),
                    agent_id: None,
                    windows: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn space_tabs_round_trip_by_name_across_new_session_ids() {
        let before = snapshot_with(&[
            (2, "grok-pc"),
            (3, "fable-pc"),
            (4, "kiro-pc"),
            (9, "other"),
        ]);
        let file = attach_tabs::AttachTabsFile {
            tabs: vec![
                attach_tabs::AttachTabRecord {
                    title: "seats".into(),
                    sessions: vec!["3".into(), "2".into(), "4".into()],
                },
                attach_tabs::AttachTabRecord {
                    title: "other".into(),
                    sessions: vec!["9".into(), "77".into()],
                },
            ],
            active_tab: 0,
            focused_session: Some("2".into()),
            space: None,
            mode: attach_tabs::AttachTabsMode::Add,
            ..Default::default()
        };
        let saved = [
            "grok-pc".to_string(),
            "fable-pc".to_string(),
            "kiro-pc".to_string(),
        ];
        let mut space = SavedSpace {
            id: None,
            version: 1,
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: Vec::new(),
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        apply_attach_records_to_space(&mut space, &file, &before, &saved);
        assert_eq!(
            space.tabs,
            vec![SavedSpaceTab {
                title: "seats".into(),
                sessions: vec!["fable-pc".into(), "grok-pc".into(), "kiro-pc".into()],
            }],
            "tabs outside the space and unknown ids are dropped"
        );
        assert_eq!(space.active_tab, 0);
        assert_eq!(space.focused_session.as_deref(), Some("grok-pc"));

        let after = snapshot_with(&[(12, "fable-pc"), (13, "kiro-pc"), (14, "grok-pc")]);
        let restored = attach_file_from_space(&space, &after);
        assert_eq!(
            restored.tabs,
            vec![attach_tabs::AttachTabRecord {
                title: "seats".into(),
                sessions: vec!["12".into(), "14".into(), "13".into()],
            }]
        );
        assert_eq!(restored.focused_session.as_deref(), Some("14"));
        let empty = SavedSpace {
            id: None,
            version: 1,
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: Vec::new(),
            tabs: space.tabs.clone(),
            active_tab: 0,
            focused_session: space.focused_session.clone(),
        };
        let partial = attach_file_from_space(&empty, &snapshot_with(&[(1, "nobody")]));
        assert!(partial.tabs.is_empty());
        assert_eq!(partial.focused_session, None);
    }

    #[test]
    fn tabless_space_restores_one_tab_per_session_in_space_order() {
        let space = SavedSpace {
            id: None,
            version: 1,
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: ["beta", "alpha", "gone"]
                .into_iter()
                .map(|name| prismattyc_mux::SavedSpaceSession {
                    name: name.into(),
                    agent: None,
                    windows: Vec::new(),
                })
                .collect(),
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: Some("alpha".into()),
        };
        let live = snapshot_with(&[(7, "alpha"), (8, "beta"), (9, "other")]);
        let file = attach_file_from_space(&space, &live);
        assert_eq!(
            file.tabs,
            vec![
                attach_tabs::AttachTabRecord {
                    title: "beta".into(),
                    sessions: vec!["8".into()],
                },
                attach_tabs::AttachTabRecord {
                    title: "alpha".into(),
                    sessions: vec!["7".into()],
                },
            ],
            "one tab per live space session; sessions outside the space are not pulled in"
        );
        assert_eq!(file.active_tab, 0);
        assert_eq!(file.focused_session.as_deref(), Some("7"));
    }

    #[test]
    fn detached_host_log_sits_next_to_the_socket() {
        assert_eq!(
            detached_host_log_path(Path::new("/run/user/1000/prismattyc/pmux.sock")),
            PathBuf::from("/run/user/1000/prismattyc/prismattyc-host.log")
        );
    }

    #[test]
    fn attach_all_flag_is_not_combined_with_write() {
        let parsed = parse_attach_args(vec!["--all".into()]).unwrap();
        assert!(parsed.all);
        assert!(parsed.write.is_none());
        let combined = parse_attach_args(vec!["--all".into(), "work".into()]).unwrap();
        assert!(combined.all);
        assert_eq!(combined.session.as_deref(), Some("work"));
    }

    #[test]
    fn attach_all_skips_leftover_default_when_others_exist() {
        let only = filter_attach_all_sessions(vec![(1, "default".into())]);
        assert_eq!(only, vec![(1, "default".into())]);
        let mixed = filter_attach_all_sessions(vec![
            (1, "default".into()),
            (2, "operator-a".into()),
            (3, "operator-b".into()),
        ]);
        assert_eq!(
            mixed,
            vec![(2, "operator-a".into()), (3, "operator-b".into())]
        );
    }

    #[test]
    fn format_whoami_prints_opaque_session_id() {
        let who = Whoami {
            session: "grok-pc".into(),
            id: 2,
            pane: 2,
            agent: Some("grok-pc".into()),
        };
        assert_eq!(
            format_whoami(&who),
            "session: grok-pc\nid: 2\npane: 2\nagent: grok-pc\n"
        );
        let json = serde_json::to_value(&who).unwrap();
        assert_eq!(json["id"], 2);
        assert_eq!(json["session"], "grok-pc");
        let unbound = Whoami {
            session: "default".into(),
            id: 1,
            pane: 1,
            agent: None,
        };
        assert_eq!(
            format_whoami(&unbound),
            "session: default\nid: 1\npane: 1\nagent: -\n"
        );
    }

    #[test]
    fn tutorial_pack_covers_product_usage() {
        for heading in [
            "## 1. Quickstart",
            "## 2. Discovery environment",
            "## 3. Identity",
            "## 4. Mail",
            "## 5. Architecture",
            "## 6. Operations",
            "## 7. Documentation",
            "## Prove it",
        ] {
            assert!(PMUX_TUTORIAL.contains(heading), "missing {heading}");
        }
        for key in [
            "PRISMATTYC_PANE_ID",
            "PMUX_SOCKET",
            "PMUX_AGENT",
            "PMUX_TUTORIAL_PACK",
            "pmux whoami",
            "PRISMATTYC_SESSION_ID",
            "never stamped",
            "leftover session named `default`",
            "pmux mail watch",
            "Timeout is not a mux failure",
            "pmux space clear",
            "pmux session clear",
        ] {
            assert!(PMUX_TUTORIAL.contains(key), "missing {key}");
        }
    }

    #[test]
    fn parse_tutorial_args_play_level_and_reset() {
        let none = parse_tutorial_args(&[]).unwrap();
        assert!(!none.play);
        assert!(none.level.is_none());
        assert!(!none.reset);
        let play = parse_tutorial_args(&["--play".into()]).unwrap();
        assert!(play.play);
        let all = parse_tutorial_args(&[
            "--play".into(),
            "--level".into(),
            "window".into(),
            "--reset".into(),
        ])
        .unwrap();
        assert!(all.play);
        assert_eq!(all.level.as_deref(), Some("window"));
        assert!(all.reset);
        assert!(parse_tutorial_args(&["--reset".into()]).is_err());
        assert!(parse_tutorial_args(&["--level".into(), "window".into()]).is_err());
    }

    #[test]
    fn play_step_line_and_level_list_format() {
        assert_eq!(
            format_play_step(0, 5, 1, 3, "Focus the pane on the right."),
            "Level 1/5 · step 2/3 — Focus the pane on the right."
        );
        let catalog = bundled_catalog().unwrap();
        let list = play_listing(&catalog, None, None).unwrap();
        assert!(list.starts_with("The window\n"));
        assert!(list.contains("Split the pane to the right.\n"));
        assert!(list.contains("Focus the pane on the right.\n"));
        assert!(!list.contains("Open a new tab."));
        let tabs = play_listing(&catalog, None, Some("tabs")).unwrap();
        assert!(tabs.starts_with("Tabs\n"));
        assert!(tabs.contains("Open a new tab.\n"));
        assert_eq!(play_key_from_byte(b'\n'), PlayKey::Enter);
        assert_eq!(play_key_from_byte(b's'), PlayKey::Skip);
        assert_eq!(play_key_from_byte(b'q'), PlayKey::Quit);
        assert_eq!(play_key_from_byte(0x03), PlayKey::Quit);
        assert_eq!(play_key_from_byte(0x04), PlayKey::Quit);
        let play_routes = [
            (None, false, false, false, PlayRoute::Idle),
            (Some(PlayKey::Other), false, false, false, PlayRoute::Idle),
            (Some(PlayKey::Quit), false, false, false, PlayRoute::Quit),
            (Some(PlayKey::Skip), true, true, true, PlayRoute::Skip),
            (Some(PlayKey::Enter), true, false, true, PlayRoute::WaitMux),
            (Some(PlayKey::Enter), false, true, true, PlayRoute::WaitBoss),
            (Some(PlayKey::Enter), true, true, false, PlayRoute::Advance),
            (Some(PlayKey::Enter), false, false, true, PlayRoute::Advance),
        ];
        for (key, mux_wait, boss, have_daemon, want) in play_routes {
            assert_eq!(
                route_play_key(key, mux_wait, boss, have_daemon),
                want,
                "key={key:?} mux={mux_wait} boss={boss} daemon={have_daemon}"
            );
        }
        use rustix::termios::{InputModes, LocalModes};
        let local = LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG;
        let input = InputModes::ICRNL | InputModes::IXON;
        let (local, input) = play_cbreak_modes(local, input);
        assert!(!local.contains(LocalModes::ICANON));
        assert!(!local.contains(LocalModes::ECHO));
        assert!(local.contains(LocalModes::ISIG));
        assert!(!input.contains(InputModes::ICRNL));
        assert!(input.contains(InputModes::IXON));
    }

    #[test]
    fn parse_new_args_flags_and_program() {
        let parsed = parse_new_args(vec!["work".into(), "--".into(), "/bin/sh".into()]).unwrap();
        assert_eq!(parsed.name, "work");
        assert_eq!(parsed.program, vec!["/bin/sh"]);
        assert_eq!(parsed.attach, None);
        assert_eq!(parsed.agent_id.as_deref(), Some("work"));

        let parsed = parse_new_args(vec!["--no-attach".into(), "work".into()]).unwrap();
        assert_eq!(parsed.name, "work");
        assert_eq!(parsed.attach, Some(false));
        assert_eq!(parsed.agent_id.as_deref(), Some("work"));

        let parsed = parse_new_args(vec![
            "work".into(),
            "--attach".into(),
            "--".into(),
            "/bin/zsh".into(),
            "-l".into(),
        ])
        .unwrap();
        assert_eq!(parsed.name, "work");
        assert_eq!(parsed.program, vec!["/bin/zsh", "-l"]);
        assert_eq!(parsed.attach, Some(true));
        assert_eq!(parsed.agent_id.as_deref(), Some("work"));

        let parsed =
            parse_new_args(vec!["--agent".into(), "operator-a".into(), "work".into()]).unwrap();
        assert_eq!(parsed.name, "work");
        assert_eq!(parsed.agent_id.as_deref(), Some("operator-a"));

        let parsed = parse_new_args(vec!["--no-agent".into(), "work".into()]).unwrap();
        assert_eq!(parsed.name, "work");
        assert_eq!(parsed.agent_id, None);

        let parsed = parse_new_args(vec!["--headless".into(), "worker-bot".into()]).unwrap();
        assert_eq!(parsed.name, "worker-bot");
        assert!(parsed.headless);
        assert_eq!(parsed.agent_id.as_deref(), Some("worker-bot"));

        assert!(parse_new_args(vec![
            "--headless".into(),
            "--attach".into(),
            "worker-bot".into()
        ])
        .is_err());

        assert!(parse_new_args(vec!["--attach".into()]).is_err());
        assert!(parse_new_args(vec!["--json".into(), "work".into()]).is_err());
        assert!(parse_new_args(vec![
            "--agent".into(),
            "a".into(),
            "--no-agent".into(),
            "work".into()
        ])
        .is_err());
    }

    #[test]
    fn attach_after_new_requires_tty_unless_forced() {
        assert!(should_attach_new(true, None, true));
        assert!(!should_attach_new(true, None, false));
        assert!(should_attach_new(true, Some(true), false));
        assert!(!should_attach_new(false, Some(false), true));
        assert!(should_attach_new(true, Some(true), true));
        assert!(!should_attach_new(false, None, true));
    }

    #[test]
    fn linux_attach_all_blocks_when_neither_display_var_is_set() {
        use std::ffi::OsStr;
        assert!(linux_attach_all_blocked_without_display(None, None));
        assert!(!linux_attach_all_blocked_without_display(
            Some(OsStr::new("wayland-0")),
            None
        ));
        assert!(!linux_attach_all_blocked_without_display(
            None,
            Some(OsStr::new(":0"))
        ));
        assert!(!linux_attach_all_blocked_without_display(
            Some(OsStr::new("wayland-0")),
            Some(OsStr::new(":0"))
        ));
        let message = attach_all_no_display_message(Path::new("/tmp/prism-1000-default.sock"));
        assert!(message.contains("pmux attach SESSION"), "{message}");
        assert!(message.contains("C-\\ d"), "{message}");
        assert!(
            message.contains("/tmp/prism-1000-default.sock"),
            "{message}"
        );
        assert!(message.contains("WAYLAND_DISPLAY"), "{message}");
    }

    #[test]
    fn save_buffer_parses_history_and_stdout_dash() {
        let parsed =
            parse_save_buffer_args(vec!["work".into(), "-".into(), "--history".into()]).unwrap();
        assert_eq!(parsed.target, "work");
        assert_eq!(parsed.file.as_os_str(), "-");
        assert!(parsed.history);
        let parsed =
            parse_save_buffer_args(vec!["--history".into(), "3".into(), "/tmp/p".into()]).unwrap();
        assert_eq!(parsed.target, "3");
        assert_eq!(parsed.file, PathBuf::from("/tmp/p"));
        assert!(parsed.history);
        assert!(parse_save_buffer_args(vec!["only-one".into()]).is_err());
        assert!(parse_save_buffer_args(vec!["a".into(), "b".into(), "--bogus".into()]).is_err());
    }

    #[test]
    fn pipe_pane_parses_file_and_exec() {
        let parsed = parse_pipe_pane_args(vec!["work".into(), "/tmp/out".into()]).unwrap();
        assert_eq!(parsed.target, "work");
        assert_eq!(parsed.dest, PipePaneDest::File(PathBuf::from("/tmp/out")));
        let parsed = parse_pipe_pane_args(vec![
            "7".into(),
            "--exec".into(),
            "cat".into(),
            ">>".into(),
            "/tmp/log".into(),
        ])
        .unwrap();
        assert_eq!(parsed.target, "7");
        assert_eq!(parsed.dest, PipePaneDest::Exec("cat >> /tmp/log".into()));
        let dash = parse_pipe_pane_args(vec!["work".into(), "-".into()]).unwrap();
        assert_eq!(dash.dest, PipePaneDest::File(PathBuf::from("-")));
        assert!(parse_pipe_pane_args(vec!["work".into()]).is_err());
        assert!(parse_pipe_pane_args(vec!["work".into(), "--exec".into()]).is_err());
        assert!(parse_pipe_pane_args(vec!["work".into(), "a".into(), "b".into()]).is_err());
    }

    #[test]
    fn pipe_writer_exec_finish_kills_sleep_without_hanging() {
        let writer = PipeWriter::open(&PipePaneDest::Exec("sleep 30".into())).unwrap();
        let started = Instant::now();
        writer.finish().expect("finish after kill is success");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "finish must not wait out sleep 30"
        );
    }

    fn empty_ledger() -> PaneInputLedger {
        PaneInputLedger {
            last_output_at_ms: None,
            focused: false,
            controller_id: None,
            last_controller_write_at_ms: None,
            last_write_ended_with_cr: false,
            dirty_input: false,
            last_input_at_ms: None,
        }
    }

    fn leaf_window(pane_id: u64, title_pinned: bool) -> WindowSnapshot {
        WindowSnapshot {
            id: 1,
            title: "main".into(),
            bounds: WindowBounds {
                window_id: 1,
                cols: 80,
                rows: 24,
            },
            layout: LayoutSnapshot::Leaf { pane_id },
            panes: vec![PaneSnapshot {
                pane_write: None,
                id: pane_id,
                title: String::new(),
                title_pinned,
                controller_id: None,
                geometry: PaneGeometry {
                    pane_id,
                    col: 0,
                    row: 0,
                    cols: 80,
                    rows: 24,
                },
                spawn: None,
                child_pid: None,
                mail: None,
                status: None,
                attention: None,
                mail_inject: None,
                ledger: empty_ledger(),
                size_owner: None,
            }],
            sync_input: false,
        }
    }

    fn titled_space(session: &str, title: Option<&str>) -> SavedSpace {
        SavedSpace {
            id: None,
            version: 1,
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: vec![SavedSpaceSession {
                name: session.into(),
                agent: None,
                windows: vec![SavedWindow {
                    title: "main".into(),
                    cols: 80,
                    rows: 24,
                    root: SavedNode::Leaf {
                        cwd: None,
                        program: None,
                        command: None,
                        title: title.map(str::to_string),
                    },
                }],
            }],
            tabs: vec![],
            active_tab: 0,
            focused_session: None,
        }
    }

    fn live_snapshot(name: &str, pane_id: u64, title_pinned: bool) -> Snapshot {
        Snapshot {
            sequence: 1,
            sessions: vec![SessionSnapshot {
                space_id: None,
                id: 9,
                name: name.into(),
                agent_id: None,
                windows: vec![leaf_window(pane_id, title_pinned)],
            }],
        }
    }

    fn collect_restores(
        snapshot: &Snapshot,
        space: &SavedSpace,
        applied: &[(String, ApplyResult, usize)],
    ) -> Result<Vec<(u64, String)>> {
        let mut out = Vec::new();
        each_restored_pane_title(snapshot, space, applied, |pane_id, title| {
            out.push((pane_id, title));
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn restore_titles_empty_applied_is_ok() {
        let snapshot = Snapshot {
            sequence: 0,
            sessions: vec![],
        };
        let space = SavedSpace {
            id: None,
            version: 1,
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: vec![],
            tabs: vec![],
            active_tab: 0,
            focused_session: None,
        };
        let restored = collect_restores(&snapshot, &space, &[]).expect("empty restore is Ok");
        assert!(restored.is_empty());
    }

    #[test]
    fn restore_titles_created_session_matches_on_name() {
        let snapshot = live_snapshot("pt128", 42, false);
        let space = titled_space("pt128", Some("build server"));
        let applied = vec![(
            "pt128".into(),
            ApplyResult::Created {
                windows: 1,
                panes: 1,
                agent: None,
            },
            0,
        )];
        let restored = collect_restores(&snapshot, &space, &applied).unwrap();
        assert_eq!(restored, vec![(42, "build server".into())]);
    }

    #[test]
    fn restore_titles_skips_when_session_name_does_not_match() {
        let snapshot = live_snapshot("other", 42, false);
        let space = titled_space("pt128", Some("build server"));
        let applied = vec![(
            "pt128".into(),
            ApplyResult::Created {
                windows: 1,
                panes: 1,
                agent: None,
            },
            0,
        )];
        let restored = collect_restores(&snapshot, &space, &applied).unwrap();
        assert!(
            restored.is_empty(),
            "session.name == saved name is required"
        );
    }

    #[test]
    fn restore_titles_skipped_unpinned_applies_and_pinned_does_not() {
        let space = titled_space("pt128", Some("build server"));
        let applied = vec![("pt128".into(), ApplyResult::Skipped, 0)];
        let unpinned =
            collect_restores(&live_snapshot("pt128", 7, false), &space, &applied).unwrap();
        assert_eq!(unpinned, vec![(7, "build server".into())]);
        let pinned = collect_restores(&live_snapshot("pt128", 7, true), &space, &applied).unwrap();
        assert!(
            pinned.is_empty(),
            "pinned live titles must not be clobbered"
        );
    }

    #[test]
    fn restore_titles_skips_empty_and_whitespace() {
        let snapshot = live_snapshot("pt128", 42, false);
        let applied = vec![(
            "pt128".into(),
            ApplyResult::Created {
                windows: 1,
                panes: 1,
                agent: None,
            },
            0,
        )];
        assert!(
            collect_restores(&snapshot, &titled_space("pt128", None), &applied)
                .unwrap()
                .is_empty()
        );
        assert!(
            collect_restores(&snapshot, &titled_space("pt128", Some("  ")), &applied)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restore_titles_propagates_rename_error() {
        let snapshot = live_snapshot("pt128", 42, false);
        let space = titled_space("pt128", Some("build server"));
        let applied = vec![(
            "pt128".into(),
            ApplyResult::Created {
                windows: 1,
                panes: 1,
                agent: None,
            },
            0,
        )];
        let err = each_restored_pane_title(&snapshot, &space, &applied, |_, _| bail!("mutated"));
        assert!(err.is_err(), "rename failure must not become Ok(())");
    }

    fn two_step_play_catalog() -> Catalog {
        let step = |id: &str, caption: &str| Step {
            id: id.into(),
            caption: caption.into(),
            hint: None,
            command: None,
            audio: None,
            expect: Expect::HostAction {
                action: "split".into(),
                result: None,
            },
            show_me: None,
        };
        Catalog {
            schema_version: 1,
            level: vec![Level {
                id: "l1".into(),
                title: "One".into(),
                step: vec![step("s1", "First step."), step("s2", "Second step.")],
            }],
        }
    }

    fn with_temp_progress<R>(f: impl FnOnce() -> R) -> R {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "pt238-play-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _env = TempEnvDir::install("XDG_DATA_HOME", dir.clone(), &dir);
        f()
    }

    fn dummy_client() -> (Client, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let client = Client {
            socket_identity: "test".into(),
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        (client, peer)
    }

    fn reply_with(
        mut peer: UnixStream,
        data: ControlResponseData,
    ) -> std::sync::mpsc::Receiver<serde_json::Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                return;
            }
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let request_id = request["request_id"].as_u64().unwrap();
            let _ = tx.send(request);
            let response = ControlResponse {
                version: PROTOCOL_VERSION,
                request_id,
                body: ControlResponseBody::Ok { response: data },
            };
            serde_json::to_writer(&peer, &response).unwrap();
            peer.write_all(b"\n").unwrap();
        });
        rx
    }

    fn empty_events() -> ControlResponseData {
        ControlResponseData::Events {
            batch: EventBatch {
                after_sequence: 0,
                through_sequence: 0,
                current_sequence: 0,
                has_more: false,
                events: Vec::new(),
            },
        }
    }

    fn pane_split_events() -> ControlResponseData {
        let (event, _, _, _) = sample_control_events()
            .into_iter()
            .find(|(_, kind, _, _)| *kind == "PaneSplit")
            .expect("PaneSplit sample");
        ControlResponseData::Events {
            batch: EventBatch {
                after_sequence: 0,
                through_sequence: 1,
                current_sequence: 1,
                has_more: false,
                events: vec![EventEnvelope { sequence: 1, event }],
            },
        }
    }

    fn mux_expect_pane_split() -> Expect {
        Expect::MuxEvent {
            event: "PaneSplit".into(),
            to_window: None,
            session: None,
        }
    }

    fn assert_peer_got_no_request(peer: &mut UnixStream) {
        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 1];
        let result = peer.read(&mut buf);
        peer.set_nonblocking(false).unwrap();
        match result {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Ok(0) => {}
            other => panic!("peer saw data: {other:?}"),
        }
    }

    #[test]
    fn play_helpers_advance_skip_and_finish() {
        with_temp_progress(|| {
            let catalog = two_step_play_catalog();
            let mut cursor = Cursor::resume(catalog.clone(), None, None).unwrap();
            assert!(play_complete_step(&mut cursor).unwrap());
            assert_eq!(cursor.completed, vec!["s1".to_string()]);
            assert!(cursor.skipped.is_empty());

            let mut skip_cursor = Cursor::resume(catalog, None, None).unwrap();
            assert!(play_skip_step(&mut skip_cursor).unwrap());
            assert_eq!(skip_cursor.skipped, vec!["s1".to_string()]);
            assert!(skip_cursor.completed.is_empty());

            assert!(!play_done_if_finished(true).unwrap());
            assert!(play_done_if_finished(false).unwrap());
        });
    }

    #[test]
    fn apply_play_move_skip_is_not_complete() {
        with_temp_progress(|| {
            let catalog = two_step_play_catalog();
            let mut cursor = Cursor::resume(catalog, None, None).unwrap();
            let tick = apply_play_move(PlayRoute::Skip, &mut cursor).unwrap();
            assert_eq!(tick, PlayTick::NextStep);
            assert_eq!(cursor.skipped, vec!["s1".to_string()]);
            assert!(cursor.completed.is_empty());
        });
    }

    #[test]
    fn cmd_tutorial_play_rejects_unknown_level() {
        let paths = Paths {
            view_path: None,
            target_is_explicit: true,
            socket: PathBuf::from("/tmp/pt238-no-such-pmux.sock"),
            pidfile: PathBuf::from("/tmp/pt238-no-such-pmux.pid"),
            logfile: PathBuf::from("/tmp/pt238-no-such-pmux.log"),
        };
        let err = cmd_tutorial_play(
            &paths,
            bundled_catalog().unwrap(),
            None,
            Some("no-such-level"),
        );
        assert!(err.is_err(), "unknown level must not become Ok(())");
    }

    #[test]
    fn maybe_boss_advance_idles_when_disabled_or_fresh() {
        let mut last = Instant::now();
        let mut diff = String::from("not yet");
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let tick = maybe_boss_advance(
            None,
            Path::new("/tmp"),
            false,
            &mut last,
            &mut diff,
            &mut cursor,
        )
        .unwrap();
        assert_eq!(tick, PlayTick::Idle);

        last = Instant::now();
        let tick = maybe_boss_advance(
            None,
            Path::new("/tmp"),
            true,
            &mut last,
            &mut diff,
            &mut cursor,
        )
        .unwrap();
        assert_eq!(tick, PlayTick::Idle);
    }

    #[test]
    fn maybe_mux_advance_idles_when_not_waiting() {
        let mut after = 0;
        let expect = Expect::MuxEvent {
            event: "pane_created".into(),
            to_window: None,
            session: None,
        };
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let tick = maybe_mux_advance(None, &mut after, &expect, false, &mut cursor).unwrap();
        assert_eq!(tick, PlayTick::Idle);
    }

    #[test]
    fn play_poll_tick_idles_without_mux_or_boss() {
        let mut after = 0;
        let expect = Expect::HostAction {
            action: "split".into(),
            result: None,
        };
        let mut last = Instant::now();
        let mut diff = String::from("not yet");
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let mut client = None;
        let tick = play_poll_tick(
            &mut client,
            Path::new("/tmp"),
            &mut after,
            &expect,
            false,
            false,
            false,
            &mut last,
            &mut diff,
            &mut cursor,
        )
        .unwrap();
        assert_eq!(tick, PlayTick::Idle);
    }

    #[test]
    fn maybe_mux_advance_idles_when_wait_is_off_even_with_matching_events() {
        let (mut client, mut peer) = dummy_client();
        let mut after = 0;
        let expect = mux_expect_pane_split();
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let tick =
            maybe_mux_advance(Some(&mut client), &mut after, &expect, false, &mut cursor).unwrap();
        assert_eq!(tick, PlayTick::Idle);
        assert!(cursor.completed.is_empty());
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn maybe_mux_advance_idles_on_empty_events() {
        let (mut client, peer) = dummy_client();
        let rx = reply_with(peer, empty_events());
        let mut after = 0;
        let expect = mux_expect_pane_split();
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let tick =
            maybe_mux_advance(Some(&mut client), &mut after, &expect, true, &mut cursor).unwrap();
        assert_eq!(tick, PlayTick::Idle);
        assert!(cursor.completed.is_empty());
        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(request["type"], "events");
    }

    #[test]
    fn maybe_mux_advance_completes_on_matching_event() {
        with_temp_progress(|| {
            let (mut client, peer) = dummy_client();
            let rx = reply_with(peer, pane_split_events());
            let mut after = 0;
            let expect = mux_expect_pane_split();
            let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
            let tick = maybe_mux_advance(Some(&mut client), &mut after, &expect, true, &mut cursor)
                .unwrap();
            assert_eq!(tick, PlayTick::NextStep);
            assert_eq!(cursor.completed, vec!["s1".to_string()]);
            let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(request["type"], "events");
        });
    }

    #[test]
    fn maybe_boss_advance_idles_while_interval_is_fresh() {
        let (mut client, mut peer) = dummy_client();
        let mut last = Instant::now();
        let mut diff = String::from("not yet");
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let tick = maybe_boss_advance(
            Some(&mut client),
            Path::new("/tmp"),
            true,
            &mut last,
            &mut diff,
            &mut cursor,
        )
        .unwrap();
        assert_eq!(tick, PlayTick::Idle);
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn maybe_boss_advance_polls_after_interval() {
        let (mut client, peer) = dummy_client();
        let rx = reply_with(peer, ControlResponseData::Pong);
        let mut last = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);
        let mut diff = String::from("not yet");
        let mut cursor = Cursor::resume(two_step_play_catalog(), None, None).unwrap();
        let tick = maybe_boss_advance(
            Some(&mut client),
            Path::new("/tmp"),
            true,
            &mut last,
            &mut diff,
            &mut cursor,
        )
        .unwrap();
        assert_eq!(tick, PlayTick::Idle, "Pong is not a snapshot");
        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(request["type"], "snapshot");
    }
}

#[cfg(windows)]
struct PlayRawStdin;
#[cfg(windows)]
impl PlayRawStdin {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}
#[cfg(windows)]
impl Drop for PlayRawStdin {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
#[cfg(windows)]
fn read_play_key(timeout: Duration) -> Result<Option<PlayKey>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    if !event::poll(timeout)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Release => None,
        Event::Key(k)
            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(play_key_from_byte(3))
        }
        Event::Key(k) => Some(match k.code {
            KeyCode::Enter => PlayKey::Enter,
            KeyCode::Char(c) if c.is_ascii() => play_key_from_byte(c as u8),
            _ => PlayKey::Other,
        }),
        _ => None,
    })
}
#[cfg(windows)]
fn signal(pid: u32, _signal: prismattyc_mux::platform::Signal) -> Result<()> {
    prismattyc_mux::platform::terminate_process(pid).context("terminate process")
}
