// SPDX-License-Identifier: MIT
//! Compatibility launcher. Views and queries run only through the daemon registry.

use anyhow::{bail, ensure, Result};
use prismattyc_core::AgentId;
use prismattyc_mux::plugins::{Client, EndpointSource, GraphStyle, Reply, Request};
use serde_json::{json, Value};
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Clone, Debug)]
enum Source {
    Socket(PathBuf),
    Command(Vec<String>),
}
impl Source {
    fn definition(&self) -> EndpointSource {
        match self {
            Self::Socket(path) => EndpointSource::Socket { path: path.clone() },
            Self::Command(argv) => EndpointSource::CommandTail { argv: argv.clone() },
        }
    }
}

const SNAPSHOT_SIZE: (u16, u16) = (120, 24);
const MONITOR: &str = "org.moonbase.activity-monitor";
const CONNECTOR: &str = "org.moonbase.activity-connector";
const USAGE: &str = "usage: pmux-activity-monitor [--socket PATH] [--host LABEL=PATH]... [--host-cmd LABEL JSON_ARGV]...
  (with no --socket or --host, reads this machine's pmuxd, found the way pmux finds it)
                             [--theme auto|dark|light] [--no-pixels] [--no-motion]
                             [--graph-style braille|block|ascii]
                             [--snapshot [--size COLSxROWS]]
       pmux-activity-monitor --socket PATH (--json [--since CURSOR|UNIX_MS] | --seats | --recent SEAT | --tail [--seat SEAT])
       pmux-activity-monitor --version";

enum Mode {
    Live,
    Snapshot,
    Json(Option<String>),
    Seats,
    Recent(AgentId),
    Tail(Option<AgentId>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeChoice {
    Auto,
    Dark,
    Light,
}

struct Options {
    socket: Option<PathBuf>,
    hosts: Vec<(String, Source)>,
    mode: Mode,
    size: Option<(u16, u16)>,
    theme: ThemeChoice,
    pixels: bool,
    graph_style: Option<GraphStyle>,
    motion: bool,
}

/// A neutral label for the machine this monitor runs on.
fn local_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn parse_label(raw: &str) -> Result<String> {
    let valid = !raw.is_empty()
        && raw.len() <= 24
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        bail!("a host label is 1 to 24 letters, digits, '-', '_' or '.'");
    }
    Ok(raw.to_owned())
}

fn parse_size(raw: &str) -> Result<(u16, u16)> {
    let parsed = raw
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .filter(|(w, h)| {
            (1..=500).contains(w)
                && (1..=200).contains(h)
                && u32::from(*w) * u32::from(*h) <= 60_000
        });
    parsed.ok_or_else(|| anyhow::anyhow!("--size takes COLSxROWS, for example 80x24"))
}

fn parse_argv(raw: &str) -> Result<Vec<String>> {
    let argv: Vec<String> = serde_json::from_str(raw)
        .map_err(|_| anyhow::anyhow!("--host-cmd takes a JSON array of strings, for example '[\"ssh\",\"windows-host\",\"pmux\",\"activity\",\"tail\"]'"))?;
    if argv.first().is_none_or(|program| program.is_empty()) {
        bail!("--host-cmd needs a program as the first array element");
    }
    Ok(argv)
}

/// The socket `pmux` itself would use: `PMUX_INSTANCE` and `PMUX_SOCKET`, then the `[mux]`
/// section of the Prismattyc config, then pmux's default path.
fn local_socket() -> Result<PathBuf> {
    let file =
        prismattyc_mux::load_mux_section(&prismattyc_mux::prism_config_path()).unwrap_or_default();
    let (instance, socket) = prismattyc_mux::resolve_mux_target(None, None, &file)?;
    match socket {
        Some(path) => Ok(path),
        None => Ok(prismattyc_mux::default_socket_path(&instance)?),
    }
}

fn options_from(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Option<Options>> {
    options_with(args, local_socket)
}

fn options_with(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    local: impl FnOnce() -> Result<PathBuf>,
) -> Result<Option<Options>> {
    let mut args = args.into_iter();
    let mut socket = None;
    let mut hosts: Vec<(String, Source)> = Vec::new();
    let mut mode = None;
    let mut seat = None;
    let mut since = None;
    let mut size = None;
    let mut theme = ThemeChoice::Auto;
    let mut pixels = true;
    let mut graph_style = None;
    let mut motion = true;
    let next = |args: &mut dyn Iterator<Item = std::ffi::OsString>| -> Result<String> {
        args.next()
            .and_then(|value| value.into_string().ok())
            .ok_or_else(|| anyhow::anyhow!(USAGE))
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--socket") if socket.is_none() => socket = Some(PathBuf::from(next(&mut args)?)),
            Some("--host") => {
                let raw = next(&mut args)?;
                let (label, path) = raw
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--host takes LABEL=PATH"))?;
                if path.is_empty() {
                    bail!("--host takes LABEL=PATH");
                }
                hosts.push((parse_label(label)?, Source::Socket(PathBuf::from(path))));
            }
            Some("--host-cmd") => {
                let label = parse_label(&next(&mut args)?)?;
                hosts.push((label, Source::Command(parse_argv(&next(&mut args)?)?)));
            }
            Some("--snapshot") if mode.is_none() => mode = Some(Mode::Snapshot),
            Some("--json") if mode.is_none() => mode = Some(Mode::Json(None)),
            Some("--since") if since.is_none() => since = Some(next(&mut args)?),
            Some("--seats") if mode.is_none() => mode = Some(Mode::Seats),
            Some("--recent") if mode.is_none() => {
                mode = Some(Mode::Recent(parse_seat(args.next())?));
            }
            Some("--tail") if mode.is_none() => mode = Some(Mode::Tail(None)),
            Some("--seat") if seat.is_none() => seat = Some(parse_seat(args.next())?),
            Some("--size") if size.is_none() => size = Some(parse_size(&next(&mut args)?)?),
            Some("--theme") => {
                theme = match next(&mut args)?.as_str() {
                    "auto" => ThemeChoice::Auto,
                    "dark" => ThemeChoice::Dark,
                    "light" => ThemeChoice::Light,
                    _ => bail!("--theme takes auto, dark or light"),
                };
            }
            Some("--no-pixels") => pixels = false,
            Some("--graph-style") => {
                graph_style = Some(
                    args.next()
                        .and_then(|arg| arg.into_string().ok())
                        .ok_or_else(|| {
                            anyhow::anyhow!("--graph-style requires braille, block or ascii")
                        })?
                        .parse::<GraphStyle>()
                        .map_err(anyhow::Error::msg)?,
                );
            }
            Some("--no-motion") => motion = false,
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return Ok(None);
            }
            // Release packaging checks that every shipped binary reports the release.
            Some("--version" | "-V") => {
                println!("{}", prismattyc_core::bin_version("pmux-activity-monitor"));
                return Ok(None);
            }
            _ => bail!(USAGE),
        }
    }
    let mode = match (mode, seat, since) {
        (Some(Mode::Tail(_)), seat, None) => Mode::Tail(seat),
        (Some(Mode::Json(_)), None, since) => Mode::Json(since),
        (mode, None, None) => mode.unwrap_or(Mode::Live),
        (Some(Mode::Json(_)), Some(_), _) => bail!("--seat requires --tail"),
        (_, _, Some(_)) => bail!("--since requires --json"),
        _ => bail!("--seat requires --tail"),
    };
    // With no host named, read this machine's own pmuxd, as `pmux` would.
    if socket.is_none() && hosts.is_empty() {
        socket = Some(local()?);
    }
    let query = matches!(
        mode,
        Mode::Json(_) | Mode::Seats | Mode::Recent(_) | Mode::Tail(_)
    );
    if query && !hosts.is_empty() {
        bail!("--json, --seats, --recent and --tail read one feed: use --socket, not --host");
    }
    if size.is_some() && !matches!(mode, Mode::Snapshot) {
        bail!("--size applies to --snapshot; the live view follows the terminal");
    }
    let mut labels: Vec<&str> = hosts.iter().map(|(label, _)| label.as_str()).collect();
    if socket.is_some() {
        labels.push(local_label());
    }
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("each host needs its own label");
    }
    Ok(Some(Options {
        socket,
        hosts,
        mode,
        size,
        theme,
        pixels,
        graph_style,
        motion,
    }))
}

fn parse_seat(raw: Option<std::ffi::OsString>) -> Result<AgentId> {
    let raw = raw
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| anyhow::anyhow!("a valid seat identifier is required"))?;
    AgentId::new(raw).map_err(|_| anyhow::anyhow!("a valid seat identifier is required"))
}

fn query(client: &mut Client, contribution: &str, mut arguments: Value) -> Result<Value> {
    arguments["host"] = json!("local");
    match client.request(Request::Invoke {
        id: "org.moonbase.activity-connector".into(),
        contribution: contribution.into(),
        arguments: arguments.to_string(),
        min_interval_ms: 250,
    })? {
        Reply::Tool {
            json,
            is_error: false,
        } => Ok(serde_json::from_str(&json)?),
        Reply::Tool {
            json,
            is_error: true,
        } => {
            let value: Value = serde_json::from_str(&json)?;
            bail!(
                "{}",
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("activity query failed")
            );
        }
        Reply::Retry { .. } => bail!("activity feed changed; retry"),
        _ => bail!("unexpected activity query reply"),
    }
}

fn require_enabled(client: &mut Client, id: &str) -> Result<()> {
    let Reply::List { installed, states } = client.request(Request::List)? else {
        bail!("unexpected plug-in status");
    };
    if states.iter().any(|state| state.id == id && state.enabled) {
        return Ok(());
    }
    let installed = installed.iter().any(|package| package.id == id);
    let state = if installed {
        "installed but not enabled"
    } else {
        "not installed"
    };
    let catalog = if !installed {
        let Reply::Catalog { packages } = client.request(Request::Discover { refresh: false })?
        else {
            bail!("unexpected plug-in catalog");
        };
        if packages.iter().any(|package| package.id == id) {
            ""
        } else {
            "\nThis daemon has no trusted package for it. Use a matched release, or prepare a verified PRISMATTYC_PLUGIN_BUNDLE_DIR and rebuild the source binaries first."
        }
    } else {
        ""
    };
    let install = if installed {
        String::new()
    } else {
        format!("\nOn this daemon, run: pmux plugin install {id}")
    };
    bail!("Plug-in {id} is {state}.{catalog}{install}\nApprove local read access: pmux plugin enable {id} --allow activity.read:local\nUse the same --socket target if one was supplied, or manage the package in Settings > Plug-ins. Activity collection does not enable plug-in access.");
}

fn tail(mut client: Client, socket: &std::path::Path, seat: Option<&AgentId>) -> Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Release))?;
    let mut cursor = None::<String>;
    let mut last = Value::Null;
    let mut disconnected = false;
    let mut output = io::stdout().lock();
    while !stopped.load(Ordering::Acquire) {
        let response = query(&mut client, "poll", json!({"cursor":cursor,"seat":seat}));
        let message = match response {
            Ok(result) => {
                let page = &result["page"];
                let changed = last != page["cursor"] || !page["reset"].is_null() || disconnected;
                cursor = result["next_cursor"].as_str().map(str::to_owned);
                last = page["cursor"].clone();
                disconnected = false;
                changed.then(|| json!({"type":"activity","seat_filter":seat,"page":page}))
            }
            Err(_) => {
                if let Ok(reconnected) = Client::connect(socket) {
                    client = reconnected;
                }
                if disconnected {
                    None
                } else {
                    disconnected = true;
                    Some(json!({"type":"disconnected","retrying":true,"cursor":last}))
                }
            }
        };
        if let Some(message) = message {
            if let Err(error) = serde_json::to_writer(&mut output, &message)
                .map_err(io::Error::other)
                .and_then(|()| output.write_all(b"\n"))
                .and_then(|()| output.flush())
            {
                if error.kind() == io::ErrorKind::BrokenPipe || stopped.load(Ordering::Acquire) {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        for _ in 0..5 {
            if stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

fn graph_style(
    cli: Option<GraphStyle>,
    configured: Option<GraphStyle>,
    term: Option<&str>,
) -> GraphStyle {
    cli.or(configured)
        .unwrap_or_default()
        .for_terminal(term == Some("dumb"))
}

fn main() -> Result<()> {
    if prismattyc_mux::plugins::print_bundled_identity()? {
        return Ok(());
    }
    let Some(options) = options_from(env::args_os().skip(1))? else {
        return Ok(());
    };
    let socket = options
        .socket
        .clone()
        .map(Ok)
        .unwrap_or_else(local_socket)?;
    let mut client = Client::connect(&socket)?;
    require_enabled(
        &mut client,
        if matches!(options.mode, Mode::Live | Mode::Snapshot) {
            MONITOR
        } else {
            CONNECTOR
        },
    )?;
    match &options.mode {
        Mode::Live | Mode::Snapshot => {
            let config = prismattyc_mux::load_mux_section(&prismattyc_mux::prism_config_path())?;
            let style = graph_style(
                options.graph_style,
                config.activity_graph_style,
                env::var("TERM").ok().as_deref(),
            );
            let mut args = vec!["run".into(), MONITOR.into(), style.view().into()];
            if options.socket.is_some() {
                args.extend(["--endpoint".into(), format!("local={}", local_label())]);
            }
            if !options.hosts.is_empty() {
                let Reply::Endpoints { endpoints } = client.request(Request::Endpoints)? else {
                    bail!("unexpected endpoint list");
                };
                for (label, source) in &options.hosts {
                    ensure!(endpoints.iter().any(|endpoint| endpoint.label == *label && endpoint.source == source.definition()),
                        "host {label} is not registered with this transport; use pmux plugin endpoint add, then approve --allow activity.read:{label} for org.moonbase.activity-monitor");
                    args.extend(["--endpoint".into(), label.clone()]);
                }
            }
            drop(client);
            args.extend([
                "--theme".into(),
                match options.theme {
                    ThemeChoice::Auto => "auto",
                    ThemeChoice::Dark => "dark",
                    ThemeChoice::Light => "light",
                }
                .into(),
            ]);
            if !options.pixels {
                args.push("--no-pixels".into());
            }
            if !options.motion {
                args.push("--no-motion".into());
            }
            if matches!(options.mode, Mode::Snapshot) {
                let (columns, rows) = options.size.unwrap_or(SNAPSHOT_SIZE);
                args.extend([
                    "--snapshot".into(),
                    "--cols".into(),
                    columns.to_string(),
                    "--rows".into(),
                    rows.to_string(),
                ]);
            }
            prismattyc_mux::plugins::run_cli(&socket, args)
        }
        Mode::Tail(seat) => tail(client, &socket, seat.as_ref()),
        mode => {
            let mut result = match mode {
                Mode::Json(since) => {
                    let arguments = match since {
                        Some(value) if value.parse::<u64>().is_ok() => {
                            json!({"since_ms":value.parse::<u64>()?})
                        }
                        Some(cursor) => json!({"cursor":cursor}),
                        None => json!({}),
                    };
                    query(&mut client, "poll", arguments)?["page"].clone()
                }
                Mode::Seats => query(&mut client, "seats", json!({}))?,
                Mode::Recent(seat) => query(&mut client, "recent", json!({"seat":seat}))?,
                _ => unreachable!(),
            };
            if let Some(result) = result.as_object_mut() {
                result.remove("host");
            }
            serde_json::to_writer_pretty(io::stdout().lock(), &result)?;
            println!();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LOCAL: &str = "/run/test/pmux.sock";

    fn parse(args: &[&str]) -> Result<Options> {
        options_with(args.iter().map(std::ffi::OsString::from), || {
            Ok(PathBuf::from(LOCAL))
        })?
        .ok_or_else(|| anyhow::anyhow!("help"))
    }

    #[test]
    fn with_no_host_named_the_monitor_reads_the_local_pmuxd() -> Result<()> {
        let local = Some(std::path::Path::new(LOCAL));
        let plain = parse(&[])?;
        assert_eq!(plain.socket.as_deref(), local);
        assert!(matches!(plain.mode, Mode::Live));
        assert_eq!(parse(&["--seats"])?.socket.as_deref(), local);
        // A named host replaces the default; --socket overrides it.
        assert!(parse(&["--host", "lab=/tmp/b"])?.socket.is_none());
        assert_eq!(
            parse(&["--socket", "/tmp/a"])?.socket.as_deref(),
            Some(std::path::Path::new("/tmp/a"))
        );
        Ok(())
    }

    #[test]
    fn the_local_socket_is_pmux_socket_when_a_pane_sets_it() -> Result<()> {
        // Every pmux pane exports PMUX_SOCKET for its own daemon; pmux uses it unless an
        // instance is named. Outside a pane the default is an absolute socket path.
        let path = local_socket()?;
        match (
            std::env::var_os("PMUX_SOCKET"),
            std::env::var_os("PMUX_INSTANCE"),
        ) {
            (Some(env), None) if !env.is_empty() => assert_eq!(path, PathBuf::from(env)),
            _ => {
                assert!(path.is_absolute(), "{}", path.display());
                assert_eq!(path.extension().and_then(|e| e.to_str()), Some("sock"));
            }
        }
        Ok(())
    }

    #[test]
    fn hosts_keep_the_local_socket_first_then_the_order_given() -> Result<()> {
        let options = parse(&[
            "--host-cmd",
            "remote",
            r#"["ssh","windows-host","pmux","activity","tail"]"#,
            "--socket",
            "/tmp/a.sock",
            "--host",
            "lab=/tmp/b.sock",
        ])?;
        assert_eq!(
            options.socket.as_deref(),
            Some(std::path::Path::new("/tmp/a.sock"))
        );
        assert_eq!(options.hosts[0].0, "remote");
        assert!(
            matches!(&options.hosts[0].1, Source::Command(argv) if argv == &["ssh", "windows-host", "pmux", "activity", "tail"])
        );
        assert_eq!(options.hosts[1].0, "lab");
        assert!(
            matches!(&options.hosts[1].1, Source::Socket(path) if path == std::path::Path::new("/tmp/b.sock"))
        );
        Ok(())
    }

    #[test]
    fn host_commands_must_be_a_json_argv_with_a_program() {
        for bad in [
            "ssh windows pmux activity tail",
            "[]",
            r#"[""]"#,
            r#"{"a":1}"#,
        ] {
            assert!(parse(&["--host-cmd", "remote", bad]).is_err(), "{bad}");
        }
    }

    #[test]
    fn labels_are_validated_and_must_be_distinct() {
        assert!(parse(&["--host", "=/tmp/a"]).is_err());
        assert!(parse(&["--host", "bad label=/tmp/a"]).is_err());
        assert!(parse(&["--host", "a=/tmp/a", "--host", "a=/tmp/b"]).is_err());
        assert!(parse(&[
            "--socket",
            "/tmp/x",
            "--host",
            &format!("{}=/tmp/b", local_label())
        ])
        .is_err());
        assert!(parse(&["--host", "a=/tmp/a", "--host", "b=/tmp/b"]).is_ok());
    }

    #[test]
    fn query_modes_read_exactly_one_socket() {
        assert!(parse(&["--socket", "/tmp/a", "--seats"]).is_ok());
        assert!(parse(&["--socket", "/tmp/a", "--host", "b=/tmp/b", "--seats"]).is_err());
        assert!(parse(&["--host", "b=/tmp/b", "--tail"]).is_err());
        assert!(
            parse(&["--tail"]).is_ok(),
            "query modes default to the local pmuxd"
        );
        assert!(parse(&["--socket", "/tmp/a", "--seat", "worker-a"]).is_err());
    }

    #[test]
    fn size_parses_only_for_snapshots() -> Result<()> {
        assert_eq!(
            parse(&["--socket", "/a", "--snapshot", "--size", "40x12"])?.size,
            Some((40, 12))
        );
        assert!(parse(&["--socket", "/a", "--size", "40x12"]).is_err());
        assert!(parse(&["--socket", "/a", "--snapshot", "--size", "40by12"]).is_err());
        assert!(parse(&["--socket", "/a", "--snapshot", "--size", "0x12"]).is_err());
        Ok(())
    }

    #[test]
    fn display_flags_parse_and_default_on() -> Result<()> {
        let plain = parse(&["--socket", "/a"])?;
        assert!(plain.pixels && plain.motion && plain.theme == ThemeChoice::Auto);
        assert_eq!(
            graph_style(plain.graph_style, None, None),
            GraphStyle::Braille
        );
        assert!(matches!(plain.mode, Mode::Live));
        let set = parse(&[
            "--socket",
            "/a",
            "--no-pixels",
            "--no-motion",
            "--theme",
            "light",
        ])?;
        assert!(!set.pixels && !set.motion && set.theme == ThemeChoice::Light);
        assert_eq!(
            parse(&["--socket", "/a", "--theme", "dark"])?.theme,
            ThemeChoice::Dark
        );
        assert_eq!(
            parse(&["--socket", "/a", "--theme", "light", "--theme", "auto"])?.theme,
            ThemeChoice::Auto
        );
        assert!(parse(&["--socket", "/a", "--theme", "sepia"]).is_err());
        Ok(())
    }

    #[test]
    fn graph_style_honors_cli_config_and_only_known_ascii_fallback() -> Result<()> {
        for (value, style) in [
            ("braille", GraphStyle::Braille),
            ("block", GraphStyle::Block),
            ("ascii", GraphStyle::Ascii),
        ] {
            assert_eq!(
                parse(&["--socket", "/a", "--graph-style", value])?.graph_style,
                Some(style)
            );
        }
        assert_eq!(
            graph_style(None, Some(GraphStyle::Block), None),
            GraphStyle::Block
        );
        assert_eq!(
            graph_style(
                Some(GraphStyle::Braille),
                Some(GraphStyle::Block),
                Some("xterm-256color")
            ),
            GraphStyle::Braille
        );
        assert_eq!(graph_style(None, None, Some("dumb")), GraphStyle::Ascii);
        assert_eq!(
            graph_style(None, None, Some("unknown-terminal")),
            GraphStyle::Braille
        );
        assert!(parse(&["--socket", "/a", "--graph-style", "pixels"]).is_err());
        assert!(parse(&["--socket", "/a", "--graph-style"]).is_err());
        Ok(())
    }

    #[test]
    fn help_prints_usage_instead_of_running() -> Result<()> {
        assert!(options_from(["--help"].iter().map(std::ffi::OsString::from))?.is_none());
        assert!(options_from(["-h"].iter().map(std::ffi::OsString::from))?.is_none());
        assert!(options_from(["--version"].iter().map(std::ffi::OsString::from))?.is_none());
        Ok(())
    }

    #[test]
    fn version_names_the_binary_and_the_prismattyc_release() {
        let line = prismattyc_core::bin_version("pmux-activity-monitor");
        assert!(
            line.starts_with(&format!(
                "pmux-activity-monitor {}",
                prismattyc_core::package_version()
            )),
            "{line}"
        );
    }

    #[test]
    fn each_mode_and_singular_option_is_accepted_once() -> Result<()> {
        assert!(matches!(
            parse(&["--socket", "/a", "--snapshot"])?.mode,
            Mode::Snapshot
        ));
        assert!(matches!(
            parse(&["--socket", "/a", "--json"])?.mode,
            Mode::Json(None)
        ));
        assert!(matches!(
            parse(&["--socket", "/a", "--json", "--since", "0123abcd:7"])?.mode,
            Mode::Json(Some(ref cursor)) if cursor == "0123abcd:7"
        ));
        assert!(matches!(
            parse(&["--socket", "/a", "--json", "--since", "1730000000000"])?.mode,
            Mode::Json(Some(ref timestamp)) if timestamp == "1730000000000"
        ));
        assert!(matches!(
            parse(&["--socket", "/a", "--seats"])?.mode,
            Mode::Seats
        ));
        assert!(
            matches!(parse(&["--socket", "/a", "--recent", "worker-a"])?.mode, Mode::Recent(ref s) if s.as_str() == "worker-a")
        );
        assert!(
            matches!(parse(&["--socket", "/a", "--tail", "--seat", "worker-a"])?.mode, Mode::Tail(Some(ref s)) if s.as_str() == "worker-a")
        );
        assert!(matches!(
            parse(&["--socket", "/a", "--seat", "worker-a", "--tail"])?.mode,
            Mode::Tail(Some(_))
        ));
        for twice in [
            &["--socket", "/a", "--socket", "/b"][..],
            &["--socket", "/a", "--json", "--json"],
            &["--socket", "/a", "--json", "--since", "x", "--since", "y"],
            &["--socket", "/a", "--seats", "--json"],
            &["--socket", "/a", "--snapshot", "--tail"],
            &["--socket", "/a", "--recent", "worker-a", "--seats"],
            &[
                "--socket", "/a", "--tail", "--seat", "worker-a", "--seat", "worker-b",
            ],
            &[
                "--socket",
                "/a",
                "--snapshot",
                "--size",
                "10x10",
                "--size",
                "20x20",
            ],
        ] {
            assert!(parse(twice).is_err(), "{twice:?}");
        }
        Ok(())
    }

    #[test]
    fn this_machine_is_labelled_by_its_platform() {
        let expected = if cfg!(target_os = "macos") {
            "mac"
        } else if cfg!(windows) {
            "windows"
        } else {
            "linux"
        };
        assert_eq!(local_label(), expected);
    }
}
