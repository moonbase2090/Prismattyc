// SPDX-License-Identifier: MIT

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use prismattyc_mux::mailbox::AgentId;

mod app;
mod feed;
mod model;
mod query;
mod term;
mod ui;

use app::Monitor;
use feed::{read_page, Feed, Source, Update, POLL_INTERVAL};
use ui::Palette;

const SNAPSHOT_SIZE: (u16, u16) = (120, 24);
const SNAPSHOT_WAIT: Duration = Duration::from_secs(3);
const USAGE: &str = "usage: pmux-activity-monitor [--socket PATH] [--host LABEL=PATH]... [--host-cmd LABEL JSON_ARGV]...
  (with no --socket or --host, reads this machine's pmuxd, found the way pmux finds it)
                             [--theme auto|dark|light] [--no-pixels] [--no-motion]
                             [--snapshot [--size COLSxROWS]]
       pmux-activity-monitor --socket PATH (--json | --seats | --recent SEAT | --tail [--seat SEAT])";

enum Mode {
    Live,
    Snapshot,
    Json,
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
        .filter(|(w, h)| (1..=1000).contains(w) && (1..=1000).contains(h));
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
    let mut size = None;
    let mut theme = ThemeChoice::Auto;
    let mut pixels = true;
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
            Some("--json") if mode.is_none() => mode = Some(Mode::Json),
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
            Some("--no-motion") => motion = false,
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return Ok(None);
            }
            _ => bail!(USAGE),
        }
    }
    let mode = match (mode, seat) {
        (Some(Mode::Tail(_)), seat) => Mode::Tail(seat),
        (mode, None) => mode.unwrap_or(Mode::Live),
        _ => bail!("--seat requires --tail"),
    };
    // With no host named, read this machine's own pmuxd, as `pmux` would.
    if socket.is_none() && hosts.is_empty() {
        socket = Some(local()?);
    }
    let query = matches!(
        mode,
        Mode::Json | Mode::Seats | Mode::Recent(_) | Mode::Tail(_)
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
        motion,
    }))
}

fn parse_seat(raw: Option<std::ffi::OsString>) -> Result<AgentId> {
    let raw = raw
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| anyhow::anyhow!("a valid seat identifier is required"))?;
    AgentId::new(raw).map_err(|_| anyhow::anyhow!("a valid seat identifier is required"))
}

fn palette(choice: ThemeChoice, background_dark: Option<bool>) -> Palette {
    match (choice, background_dark) {
        (ThemeChoice::Light, _) | (ThemeChoice::Auto, Some(false)) => Palette::LIGHT,
        _ => Palette::DARK,
    }
}

/// Hosts in display order: the local socket first, then each --host in the order given.
fn sources(options: &Options) -> (Vec<String>, Vec<Source>) {
    let mut labels = Vec::new();
    let mut sources = Vec::new();
    if let Some(socket) = &options.socket {
        labels.push(local_label().to_owned());
        sources.push(Source::Socket(socket.clone()));
    }
    for (label, source) in &options.hosts {
        labels.push(label.clone());
        sources.push(source.clone());
    }
    (labels, sources)
}

/// Waits until every host has caught up or failed, then renders one frame as text.
fn snapshot(options: &Options) -> Result<String> {
    let (labels, sources) = sources(options);
    let count = labels.len();
    let mut monitor = Monitor::new(
        labels,
        palette(options.theme, None),
        Some(Feed::start(sources)),
    );
    let mut settled = vec![false; count];
    let mut reachable = vec![false; count];
    let deadline = Instant::now() + SNAPSHOT_WAIT;
    while settled.iter().any(|done| !done) && Instant::now() < deadline {
        let Some(feed) = monitor.feed.as_ref() else {
            break;
        };
        let Ok(update) = feed.updates.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        let host = update.host;
        match &update.update {
            Update::Page(page) => {
                reachable[host] = true;
                settled[host] |= !page.has_more;
            }
            Update::Disabled | Update::Unsupported => {
                reachable[host] = true;
                settled[host] = true;
            }
            Update::Disconnected => settled[host] = true,
        }
        let wall = app::now_ms();
        let target = &mut monitor.hosts[host];
        match update.update {
            Update::Page(page) => target.apply(page, 0),
            Update::Disconnected => target.disconnect(wall),
            Update::Disabled => target.link = model::Link::Disabled,
            Update::Unsupported => target.link = model::Link::Unsupported,
        }
    }
    // Stop the readers before rendering so the frame shows a fixed state.
    monitor.feed = None;
    if !reachable.iter().any(|ok| *ok) {
        bail!("activity feed unavailable");
    }
    monitor.ui.motion = false;
    let (width, height) = options.size.unwrap_or(SNAPSHOT_SIZE);
    let (grid, clusters) = monitor.frame(width, height, 0);
    Ok(grid.display_text(&clusters))
}

fn main() -> Result<()> {
    let Some(options) = options_from(env::args_os().skip(1))? else {
        return Ok(());
    };
    match &options.mode {
        Mode::Live => {
            let probe = term::probe(options.pixels);
            let (labels, sources) = sources(&options);
            let mut monitor = Monitor::new(
                labels,
                palette(options.theme, probe.background_dark),
                Some(Feed::start(sources)),
            );
            monitor.ui.motion = options.motion;
            term::run(monitor, probe)?;
        }
        Mode::Snapshot => io::stdout().write_all(snapshot(&options)?.as_bytes())?,
        Mode::Json => {
            let socket = options.socket.as_deref().unwrap_or_else(|| unreachable!());
            let page = feed::read_page(socket, None)
                .map_err(|_| anyhow::anyhow!("activity feed unavailable"))?;
            serde_json::to_writer_pretty(io::stdout().lock(), &page)?;
            println!();
        }
        Mode::Seats => {
            let socket = options.socket.as_deref().unwrap_or_else(|| unreachable!());
            let observed = query::seats(socket)?;
            serde_json::to_writer_pretty(io::stdout().lock(), &observed)?;
            println!();
        }
        Mode::Recent(seat) => {
            let socket = options.socket.as_deref().unwrap_or_else(|| unreachable!());
            let recent = query::recent(socket, seat)?;
            serde_json::to_writer_pretty(io::stdout().lock(), &recent)?;
            println!();
        }
        Mode::Tail(seat) => {
            let socket = options.socket.as_deref().unwrap_or_else(|| unreachable!());
            query::tail(socket, seat.as_ref())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::activity::{ActivityPage, ActivityRecord};
    use serde_json::json;

    pub(super) fn record(seat: &str, sequence: u64, kind: &str) -> Result<ActivityRecord> {
        Ok(serde_json::from_value(json!({
            "sequence": sequence,
            "received_at_ms": 1000,
            "event": {
                "schema_version": 1,
                "seat": seat,
                "run_id": "run-1",
                "event_id": format!("event-{sequence}"),
                "source": "runtime",
                "kind": kind
            }
        }))?)
    }

    pub(super) fn page(records: Vec<ActivityRecord>, reset: Option<&str>) -> Result<ActivityPage> {
        let sequence = records.last().map_or(0, |record| record.sequence);
        Ok(serde_json::from_value(json!({
            "epoch": "epoch-a",
            "oldest_sequence": 1,
            "current_sequence": sequence,
            "records": records,
            "cursor": {"epoch": "epoch-a", "sequence": sequence},
            "has_more": false,
            "reset": reset
        }))?)
    }

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
        let (labels, sources) = sources(&options);
        assert_eq!(labels, vec![local_label(), "remote", "lab"]);
        assert!(
            matches!(&sources[0], Source::Socket(path) if path == std::path::Path::new("/tmp/a.sock"))
        );
        assert!(
            matches!(&sources[1], Source::Command(argv) if argv == &["ssh", "windows-host", "pmux", "activity", "tail"])
        );
        assert!(
            matches!(&sources[2], Source::Socket(path) if path == std::path::Path::new("/tmp/b.sock"))
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
    fn help_prints_usage_instead_of_running() -> Result<()> {
        assert!(options_from(["--help"].iter().map(std::ffi::OsString::from))?.is_none());
        assert!(options_from(["-h"].iter().map(std::ffi::OsString::from))?.is_none());
        Ok(())
    }

    #[test]
    fn each_mode_and_singular_option_is_accepted_once() -> Result<()> {
        assert!(matches!(
            parse(&["--socket", "/a", "--snapshot"])?.mode,
            Mode::Snapshot
        ));
        assert!(matches!(
            parse(&["--socket", "/a", "--json"])?.mode,
            Mode::Json
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

    #[cfg(unix)]
    #[test]
    fn a_snapshot_waits_for_each_host_and_renders_its_seats() -> Result<()> {
        let line = json!({
            "type": "activity", "seat_filter": null,
            "page": page(vec![record("worker-a", 1, "command_started")?], None)?
        });
        let argv =
            json!(["/bin/sh", "-c", format!("printf '%s\\n' '{line}'; sleep 5")]).to_string();
        let options = parse(&[
            "--host-cmd",
            "remote",
            &argv,
            "--snapshot",
            "--size",
            "80x10",
        ])?;
        let started = Instant::now();
        let text = snapshot(&options)?;
        // A caught-up host ends the wait before the snapshot deadline, and stopping the
        // command does not wait for its sleeping child.
        assert!(
            started.elapsed() < SNAPSHOT_WAIT,
            "a caught-up host ends the wait"
        );
        assert!(text.contains("remote  connected"), "{text}");
        assert!(
            text.contains("worker-a") && text.contains("command_started"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 10);
        Ok(())
    }

    #[test]
    fn theme_follows_the_terminal_background_unless_chosen() {
        assert_eq!(palette(ThemeChoice::Auto, Some(false)), Palette::LIGHT);
        assert_eq!(palette(ThemeChoice::Auto, Some(true)), Palette::DARK);
        assert_eq!(palette(ThemeChoice::Auto, None), Palette::DARK);
        assert_eq!(palette(ThemeChoice::Dark, Some(false)), Palette::DARK);
        assert_eq!(palette(ThemeChoice::Light, Some(true)), Palette::LIGHT);
    }

    #[test]
    fn a_snapshot_of_an_unreachable_feed_fails() -> Result<()> {
        let options = parse(&["--socket", "/nonexistent/feed.sock", "--snapshot"])?;
        let error = snapshot(&options).err().map(|error| error.to_string());
        assert_eq!(error.as_deref(), Some("activity feed unavailable"));
        Ok(())
    }
}
