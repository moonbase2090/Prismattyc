// SPDX-License-Identifier: MIT

use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use pmux_plugin_api::host::pmux::view::frame;

use super::{AdminAction, Client, Reply, Request};

pub fn run(socket: &Path, args: Vec<String>) -> Result<()> {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h") {
        println!("pmux plugin discover [--refresh]\npmux plugin list\npmux plugin inspect ID\npmux plugin install ID [--version VERSION]\npmux plugin enable ID [--version VERSION] [--allow activity.read:local]\npmux plugin disable ID\npmux plugin invoke ID CONTRIBUTION [JSON]\npmux plugin run ID [VIEW] --snapshot [--cols N] [--rows N]\n\nInstallation leaves packages disabled. Changes use the running daemon and never restart sessions. Plug-in updates are explicit; MCP exports require a separate operator allowlist.");
        return Ok(());
    }
    let mut client = Client::connect(socket)?;
    if args[0] == "run" {
        return snapshot(&mut client, &args[1..]);
    }
    let request = parse(&mut client, &args)?;
    let reply = client.request(request)?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    Ok(())
}

fn snapshot(client: &mut Client, args: &[String]) -> Result<()> {
    let id = args.first().context("run requires a plug-in ID")?.clone();
    let mut contribution = None;
    let mut columns = 80;
    let mut rows = 24;
    let mut snapshot = false;
    let mut args = args[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" if !snapshot => snapshot = true,
            "--cols" => columns = args.next().context("--cols requires a size")?.parse()?,
            "--rows" => rows = args.next().context("--rows requires a size")?.parse()?,
            value if !value.starts_with('-') && contribution.is_none() => {
                contribution = Some(value.to_string())
            }
            _ => bail!("usage: pmux plugin run ID [VIEW] --snapshot [--cols N] [--rows N]"),
        }
    }
    ensure!(snapshot, "pass --snapshot to render a text frame");
    let viewport = frame::Viewport {
        columns,
        rows,
        pixels: None,
    };
    let Reply::ViewOpened { handle } = client.request(Request::OpenView {
        id,
        contribution: contribution.unwrap_or_else(|| "main".into()),
        viewport: viewport.clone(),
    })?
    else {
        bail!("unexpected view-open reply");
    };
    let reply = client.request(Request::ViewEvent {
        handle: handle.clone(),
        event: frame::Event::Tick(frame::Clock {
            wall_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            monotonic_ms: 0,
        }),
    });
    let _ = client.request(Request::CloseView {
        handle: handle.clone(),
    });
    let Reply::Frame {
        handle: frame_handle,
        content,
    } = reply?
    else {
        bail!("view did not produce a snapshot frame");
    };
    ensure!(frame_handle == handle, "frame belongs to another view");
    print!("{}", super::decode_frame(&content, &viewport)?.text());
    Ok(())
}

fn parse(client: &mut Client, args: &[String]) -> Result<Request> {
    let id = || {
        args.get(1)
            .cloned()
            .context("plug-in command requires an ID")
    };
    match args[0].as_str() {
        "discover" => {
            ensure!(
                args.len() == 1 || (args.len() == 2 && args[1] == "--refresh"),
                "usage: pmux plugin discover [--refresh]"
            );
            Ok(Request::Discover {
                refresh: args.len() == 2,
            })
        }
        "list" => {
            ensure!(args.len() == 1, "usage: pmux plugin list");
            Ok(Request::List)
        }
        "inspect" => {
            ensure!(args.len() == 2, "usage: pmux plugin inspect ID");
            Ok(Request::Inspect { id: id()? })
        }
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
                let Reply::List { installed, .. } = client.request(Request::List)? else {
                    bail!("unexpected installed package reply");
                };
                let package = installed
                    .into_iter()
                    .filter(|package| {
                        package.id == id
                            && version
                                .as_ref()
                                .is_none_or(|version| package.version == *version)
                    })
                    .max_by_key(|package| {
                        semver::Version::parse(&package.version).expect("validated package version")
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
            })
        }
        _ => bail!("unknown plug-in command; use pmux plugin --help"),
    }
}

fn admin(action: AdminAction) -> Result<Request> {
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
