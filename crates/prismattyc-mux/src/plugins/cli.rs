// SPDX-License-Identifier: MIT

use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use pmux_plugin_api::host::pmux::view::frame;

use super::{AdminAction, Client, Reply, Request};

pub fn run(socket: &Path, args: Vec<String>) -> Result<()> {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h") {
        println!("pmux plugin discover [--refresh]\npmux plugin list\npmux plugin inspect ID [--version VERSION]\npmux plugin install ID [--version VERSION]\npmux plugin enable ID [--version VERSION] [--allow activity.read:LABEL]\npmux plugin disable ID\npmux plugin invoke ID CONTRIBUTION [JSON]\npmux plugin run ID [VIEW] [--snapshot [--cols N] [--rows N]] [--endpoint LABEL[=DISPLAY_LABEL]] [--theme auto|dark|light] [--no-motion] [--no-pixels]\npmux plugin endpoint list\npmux plugin endpoint add LABEL (--socket PATH | --command-query JSON_ARGV | --command-tail JSON_ARGV)\npmux plugin endpoint remove LABEL\n\nThe local scope is activity.read:local; named scopes require registered endpoints. Installation leaves packages disabled. Verified cached metadata is available offline; changes and refresh require the daemon. Changes preserve sessions. Plug-in updates are explicit; MCP exports require a separate operator allowlist.");
        return Ok(());
    }
    let read = read_request(&args)?;
    let mut client = match Client::connect(socket) {
        Ok(client) => client,
        Err(error) => {
            if let Some(request) = read {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&super::inspect_offline(socket, request)?)?
                );
                return Ok(());
            }
            return Err(error);
        }
    };
    if let Some(request) = read {
        println!(
            "{}",
            serde_json::to_string_pretty(&client.request(request)?)?
        );
        return Ok(());
    }

    if args[0] == "run" {
        return view(client, &args[1..]);
    }
    if args[0] == "endpoint" {
        let request = endpoint(&args[1..])?;
        println!(
            "{}",
            serde_json::to_string_pretty(&client.request(request)?)?
        );
        return Ok(());
    }
    let request = parse(&mut client, &args)?;
    let reply = client.request(request)?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    Ok(())
}

fn read_request(args: &[String]) -> Result<Option<Request>> {
    match args[0].as_str() {
        "discover" => {
            ensure!(
                args.len() == 1 || (args.len() == 2 && args[1] == "--refresh"),
                "usage: pmux plugin discover [--refresh]"
            );
            Ok(Some(Request::Discover {
                refresh: args.len() == 2,
            }))
        }
        "list" => {
            ensure!(args.len() == 1, "usage: pmux plugin list");
            Ok(Some(Request::List))
        }
        "inspect" => {
            ensure!(
                args.len() == 2 || (args.len() == 4 && args[2] == "--version"),
                "usage: pmux plugin inspect ID [--version VERSION]"
            );
            Ok(Some(Request::Inspect {
                id: args[1].clone(),
                version: args.get(3).cloned(),
            }))
        }
        "endpoint" if args.get(1).map(String::as_str) == Some("list") => {
            endpoint(&args[1..]).map(Some)
        }
        _ => Ok(None),
    }
}

fn view(mut client: Client, args: &[String]) -> Result<()> {
    let id = args.first().context("run requires a plug-in ID")?.clone();
    let mut contribution = None;
    let mut columns = 80;
    let mut rows = 24;
    let mut snapshot = false;
    let mut pixels = true;
    let mut preferences = frame::Preferences::default();
    let mut sized = false;
    let mut endpoints = Vec::new();
    let mut args = args[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" if !snapshot => snapshot = true,
            "--cols" => {
                columns = args.next().context("--cols requires a size")?.parse()?;
                sized = true;
            }
            "--rows" => {
                rows = args.next().context("--rows requires a size")?.parse()?;
                sized = true;
            }
            "--no-motion" => preferences.motion = false,
            "--no-pixels" => pixels = false,
            "--endpoint" => {
                let value = args.next().context("--endpoint requires LABEL[=DISPLAY_LABEL]")?;
                let (endpoint, label) = value.split_once('=').unwrap_or((value, value));
                endpoints.push(super::ViewEndpoint { endpoint:endpoint.into(), label:label.into() });
            }
            "--theme" => {
                preferences.palette = match args.next().map(String::as_str) {
                    Some("auto") => frame::Palette::Auto,
                    Some("dark") => frame::Palette::Dark,
                    Some("light") => frame::Palette::Light,
                    _ => bail!("--theme requires auto, dark or light"),
                }
            }
            value if !value.starts_with('-') && contribution.is_none() => {
                contribution = Some(value.to_string())
            }
            _ => bail!("usage: pmux plugin run ID [VIEW] [--snapshot [--cols N] [--rows N]] [--endpoint LABEL[=DISPLAY_LABEL]] [--theme auto|dark|light] [--no-motion] [--no-pixels]"),
        }
    }
    let contribution = contribution.unwrap_or_else(|| "main".into());
    let endpoints = (!endpoints.is_empty()).then_some(endpoints);
    if !snapshot {
        ensure!(
            !sized,
            "--cols and --rows apply to --snapshot; live views follow the terminal"
        );
        return super::presenter::run(client, id, contribution, preferences, pixels, endpoints);
    }
    preferences.snapshot = true;
    preferences.motion = false;
    let viewport = frame::Viewport {
        columns,
        rows,
        pixels: None,
    };
    let Reply::ViewOpened { handle } = client.request(Request::OpenView {
        preferences,
        id,
        contribution,
        viewport: viewport.clone(),
        endpoints,
    })?
    else {
        bail!("unexpected view-open reply");
    };
    let started = std::time::Instant::now();
    let reply = (|| -> Result<Reply> {
        loop {
            let reply = client.request(Request::ViewEvent {
                handle: handle.clone(),
                event: frame::Event::Tick(frame::Clock {
                    wall_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis()
                        .try_into()?,
                    monotonic_ms: started.elapsed().as_millis().try_into()?,
                }),
            })?;
            match reply {
                Reply::Idle | Reply::Retry { .. }
                    if started.elapsed() < std::time::Duration::from_secs(4) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50))
                }
                reply => return Ok(reply),
            }
        }
    })();
    let _ = client.request(Request::CloseView {
        handle: handle.clone(),
    });
    let Reply::Frame {
        handle: frame_handle,
        content,
        ..
    } = reply?
    else {
        bail!("view did not produce a snapshot frame");
    };
    ensure!(frame_handle == handle, "frame belongs to another view");
    print!("{}", super::decode_frame(&content, &viewport)?.text());
    Ok(())
}

fn endpoint(args: &[String]) -> Result<Request> {
    match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => Ok(Request::Endpoints),
        Some("remove") if args.len() == 2 => admin(AdminAction::RemoveEndpoint { label:args[1].clone() }),
        Some("add") if args.len() == 4 => {
            let source = match args[2].as_str() {
                "--socket" => super::EndpointSource::Socket { path:args[3].clone().into() },
                "--command-query" => super::EndpointSource::CommandQuery { argv:serde_json::from_str(&args[3]).context("command requires a JSON argv array")? },
                "--command-tail" => super::EndpointSource::CommandTail { argv:serde_json::from_str(&args[3]).context("command requires a JSON argv array")? },
                _ => bail!("endpoint add requires --socket, --command-query or --command-tail"),
            };
            admin(AdminAction::AddEndpoint { label:args[1].clone(), source })
        }
        _ => bail!("usage: pmux plugin endpoint list | add LABEL (--socket PATH | --command-query JSON_ARGV | --command-tail JSON_ARGV) | remove LABEL"),
    }
}

fn parse(client: &mut Client, args: &[String]) -> Result<Request> {
    let id = || {
        args.get(1)
            .cloned()
            .context("plug-in command requires an ID")
    };
    match args[0].as_str() {
        "disable" => {
            ensure!(args.len() == 2, "usage: pmux plugin disable ID");
            admin(AdminAction::Disable { id: id()? })
        }
        "install" | "enable" => {
            let id = id()?;
            let (version, grants) = options(&args[2..])?;
            if args[0] == "install" {
                ensure!(
                    grants.is_empty(),
                    "install does not grant permissions; use enable --allow"
                );
                admin(AdminAction::Install { id, version })
            } else {
                let Reply::List { installed, states } = client.request(Request::List)? else {
                    bail!("unexpected installed package reply");
                };
                let selected = version
                    .is_none()
                    .then(|| {
                        states
                            .into_iter()
                            .find(|state| state.id == id)
                            .and_then(|state| state.selected)
                    })
                    .flatten();
                let package = selected
                    .or_else(|| {
                        installed
                            .into_iter()
                            .filter(|package| {
                                package.id == id
                                    && version
                                        .as_ref()
                                        .is_none_or(|version| package.version == *version)
                            })
                            .max_by_key(|package| {
                                semver::Version::parse(&package.version)
                                    .expect("validated package version")
                            })
                    })
                    .context("plug-in version is not installed; install it first")?;
                admin(AdminAction::Enable { package, grants })
            }
        }
        "invoke" => {
            ensure!(
                (3..=4).contains(&args.len()),
                "usage: pmux plugin invoke ID CONTRIBUTION [JSON]"
            );
            Ok(Request::Invoke {
                id: id()?,
                contribution: args[2].clone(),
                arguments: args.get(3).cloned().unwrap_or_else(|| "{}".into()),
                min_interval_ms: super::protocol::default_read_interval(),
            })
        }
        _ => bail!("unknown plug-in command; use pmux plugin --help"),
    }
}

pub fn admin(action: AdminAction) -> Result<Request> {
    Ok(Request::Admin {
        retry_id: super::package::nonce()?,
        action,
    })
}

fn options(args: &[String]) -> Result<(Option<String>, Vec<String>)> {
    let mut version = None;
    let mut grants = Vec::new();
    let mut args = args.iter();
    while let Some(option) = args.next() {
        match option.as_str() {
            "--version" if version.is_none() => {
                version = Some(args.next().context("--version requires a version")?.clone())
            }
            "--allow" => grants.push(args.next().context("--allow requires a scope")?.clone()),
            _ => bail!("unknown or duplicate plug-in option {option:?}"),
        }
    }
    Ok((version, grants))
}
