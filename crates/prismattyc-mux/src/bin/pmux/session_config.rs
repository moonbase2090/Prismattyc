// SPDX-License-Identifier: MIT
//! Explicit save/restore of all sessions for a complete update.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Configuration {
    version: u32,
    space: SavedSpace,
    owners: Vec<(String, Option<String>)>,
    #[serde(default)]
    view: Option<attach_tabs::AttachTabsFile>,
}

pub(super) fn run(paths: &Paths, args: Vec<String>) -> Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("pmux session-config save FILE\npmux session-config restore FILE\n\nSave all running sessions, directories, commands and layouts.\nRestore recreates missing sessions and relaunches their saved commands.\nExisting sessions are left untouched. Restore only a file you trust.");
        return Ok(());
    }
    let [verb, file] = args.as_slice() else {
        bail!("use pmux session-config save|restore FILE");
    };
    ensure!(
        matches!(verb.as_str(), "save" | "restore"),
        "use save or restore"
    );
    let mut client = Client::connect(&paths.socket)?;
    let ControlResponseData::ClientRegistered { client_id } =
        client.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })?
    else {
        bail!("could not register session configuration client");
    };
    if verb == "save" {
        save(&mut client, paths, Path::new(file))
    } else {
        restore(&mut client, client_id, paths, Path::new(file))
    }
}

fn save(client: &mut Client, paths: &Paths, file: &Path) -> Result<()> {
    let snapshot = take_snapshot(client)?;
    let sessions = snapshot.sessions.iter().collect::<Vec<_>>();
    let config = Configuration {
        version: 1,
        view: attach_tabs::load(&paths.space_view_path()),
        space: from_sessions(&sessions),
        owners: sessions
            .iter()
            .map(|s| (s.name.clone(), s.space_id.clone()))
            .collect(),
    };
    if let Some(parent) = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty() && !p.exists())
    {
        prismattyc_mux::private_fs::create_dir(parent)?;
    }
    let mut output =
        prismattyc_mux::private_fs::open(file, OpenOptions::new().write(true).create_new(true))
            .context(
                "create session configuration; choose a new filename to keep an earlier backup",
            )?;
    output.write_all(&serde_json::to_vec_pretty(&config)?)?;
    output.sync_all()?;
    println!("Saved {} sessions to {}.", sessions.len(), file.display());
    Ok(())
}

fn restore(client: &mut Client, client_id: u64, paths: &Paths, file: &Path) -> Result<()> {
    let config: Configuration =
        serde_json::from_slice(&std::fs::read(file)?).context("read session configuration")?;
    ensure!(
        config.version == 1,
        "unsupported session configuration version"
    );
    // Validate every tree before creating anything. Never index an empty layout.
    for saved in &config.space.sessions {
        for window in &saved.windows {
            ensure!(
                window.cols > 0 && window.rows > 0,
                "saved window has no size"
            );
            for command in window.root.leaf_commands().into_iter().flatten() {
                ensure!(
                    !command_has_line_break(command),
                    "saved command contains a line break"
                );
            }
        }
    }
    let mut restored = 0;
    for saved in &config.space.sessions {
        let before = take_snapshot(client)?;
        if before.sessions.iter().any(|s| s.name == saved.name) {
            println!("Kept existing session {}.", saved.name);
            continue;
        }
        let result = if saved.windows.is_empty() {
            client.request(|request_id| ControlRequest::CreateSession {
                version: PROTOCOL_VERSION,
                request_id,
                name: saved.name.clone(),
                spawn: default_layout_spawn(None),
                cols: None,
                rows: None,
                agent_id: saved.agent.clone(),
                headless: true,
            })?;
            ApplyResult::Created {
                windows: 0,
                panes: 0,
                agent: saved.agent.clone(),
            }
        } else {
            let layout = SavedLayout {
                version: 1,
                saved_at_unix: config.space.saved_at_unix,
                session: saved.name.clone(),
                windows: saved.windows.clone(),
            };
            apply_saved_layout(
                client,
                &before,
                &layout,
                &saved.name,
                saved.agent.clone(),
                ApplyExisting::Skip,
            )?
        };
        let after = take_snapshot(client)?;
        let live = after
            .sessions
            .iter()
            .find(|s| s.name == saved.name)
            .context("restored session missing")?;
        if let Some(owner) = config
            .owners
            .iter()
            .find(|(name, _)| name == &saved.name)
            .and_then(|(_, owner)| owner.clone())
        {
            if live.space_id.as_ref() != Some(&owner) {
                client.request(|request_id| ControlRequest::TransferSpaceSessions {
                    version: PROTOCOL_VERSION,
                    request_id,
                    session_ids: vec![live.id],
                    from: live.space_id.clone(),
                    to: Some(owner),
                })?;
            }
        }
        let one = SavedSpace {
            sessions: vec![saved.clone()],
            ..config.space.clone()
        };
        let applied = vec![(saved.name.clone(), result, 0)];
        restore_saved_pane_titles(client, &after, &one, &applied)?;
        // Only newly created panes receive commands. A retry never replays a
        // command into a session that already exists.
        for (window, live_window) in saved.windows.iter().zip(&live.windows) {
            for (command, pane) in window
                .root
                .leaf_commands()
                .into_iter()
                .zip(live_leaf_ids(&live_window.layout))
            {
                if let Some(command) = command.filter(|value| !value.is_empty()) {
                    match write_pane_command(client, client_id, pane, command, true)? {
                        WritePaneCommand::Wrote => {}
                        WritePaneCommand::SkipRunning => println!("Kept the program already running in session {}; its saved command was not replayed.", saved.name),
                    }
                }
            }
        }
        restored += 1;
    }
    if let Some(view) = &config.view {
        attach_tabs::save(&paths.space_view_path(), view)?;
    }
    println!("Restored {restored} sessions.");
    Ok(())
}
