//! Exclusive Space operations. The daemon owns live membership; the store
//! serializes definitions and retains an interrupted transaction for recovery.

use super::*;
use serde::{Deserialize, Serialize};

pub(super) fn team(paths: &Paths, args: Vec<String>) -> Result<()> {
    use prismattyc_mux::space_team;
    let op = args.first().context("expected a team operation")?;
    let args = &args[1..];
    if op == "undo" {
        let [path] = args else {
            bail!("usage: pmux space undo RECEIPT");
        };
        return undo(paths, Path::new(path));
    }
    if op == "template" {
        return template_command(paths, args);
    }
    match (op.as_str(), args) {
        ("result", [name, tail @ ..]) if tail.is_empty() || tail == ["--json"] => {
            let space = load_space(&spaces_dir(), name)?;
            let results = space_team::results(&spaces_dir(), &space)?;
            if tail.is_empty() {
                if results.is_empty() { println!("No open result recorded for {name}"); }
                for result in results { println!("{}", space_team::result_text(&result)); }
            } else { println!("{}", serde_json::to_string_pretty(&results)?); }
        }
        ("details" | "attention", [name]) => {
            println!("{}", space_team::inspect(&paths.socket, &spaces_dir(), name)?.text());
        }
        ("details" | "attention", [name, flag]) if flag == "--json" => {
            println!("{}", serde_json::to_string_pretty(&space_team::inspect(&paths.socket, &spaces_dir(), name)?)?);
        }
        ("role", [name, session, role]) => {
            space_team::validate_label(role)?;
            let mut client = connect(paths)?;
            let _store = Store::lock(&mut client)?;
            let space = load_space(&spaces_dir(), name)?;
            if !space.sessions.iter().any(|s| s.name == *session) { bail!("session is not saved in this Space"); }
            space_team::edit_metadata(&spaces_dir(), &space, |meta| {
                meta.roles.insert(session.clone(), role.clone()); Ok(())
            })?;
            println!("{session}: {role}");
        }
        ("link", [name, label, target]) => {
            space_team::validate_label(label)?;
            space_team::validate_link(target)?;
            let mut client = connect(paths)?;
            let _store = Store::lock(&mut client)?;
            let space = load_space(&spaces_dir(), name)?;
            space_team::edit_metadata(&spaces_dir(), &space, |meta| {
                meta.links.insert(label.clone(), target.clone()); Ok(())
            })?;
            println!("{label}: {target}");
        }
        ("attention", [action, name, session, pane, revision]) if action == "resolve" || action == "snooze" => {
            let pane: u64 = pane.parse()?;
            let revision: u64 = revision.parse()?;
            let report = space_team::inspect(&paths.socket, &spaces_dir(), name)?;
            let seat = report.sessions.iter().find(|s| s.name == *session)
                .context("session is not saved in this Space")?;
            if !seat.attention.iter().any(|r| r.pane_id == pane && r.revision == revision) {
                bail!("attention or ownership changed; refresh the session details");
            }
            let action = if action == "resolve" { prismattyc_mux::team_attention::AttentionAction::Resolve }
                else { prismattyc_mux::team_attention::AttentionAction::Snooze { seconds: 600 } };
            space_team::TeamClient::connect(&paths.socket)?.update_attention(pane, revision,
                report.space_id.context("Space has no identity")?, seat.session_id.context("session is unavailable")?, action)?;
            println!("attention updated; mail unchanged");
        }
        _ => bail!("usage: pmux space details NAME [--json]\n       pmux space role NAME SESSION ROLE\n       pmux space link NAME LABEL TARGET\n       pmux space attention NAME [--json]\n       pmux space attention <resolve|snooze> NAME SESSION PANE REQUEST"),
    }
    Ok(())
}

pub(super) struct Store(std::fs::File);

#[derive(Serialize, Deserialize)]
struct TemplateRun {
    source: prismattyc_mux::space_template::TeamTemplate,
    space: SavedSpace,
    launch: bool,
    launches: std::collections::BTreeMap<String, String>,
    complete: bool,
}

fn template_command(paths: &Paths, args: &[String]) -> Result<()> {
    use prismattyc_mux::{space_team, space_template};
    let dir = spaces_dir();
    match args {
        [op] if op == "ls" => {
            for name in space_template::list(&dir)? { println!("{name}"); }
        }
        [op, name, flag, source] if op == "save" && flag == "--space" => {
            let definition = load_space(&dir, source)?;
            let metadata = space_team::metadata(&dir, &definition)?;
            let template = space_template::TeamTemplate { version: 1, definition, metadata };
            println!("{}", space_template::save(&dir, name, &template)?.display());
        }
        [op, name, destination, tail @ ..] if op == "preview" && (tail.is_empty() || tail == ["--json"]) => {
            let template = space_template::load(&dir, name)?;
            let live_names = connect(paths).and_then(|mut client| occupied_names(&mut client));
            let offline = live_names.is_err();
            let occupied = match live_names {
                Ok(names) => names.into_iter().collect(),
                Err(_) => definitions()?.into_iter().flat_map(|(_, space)| space.sessions.into_iter().map(|session| session.name)).collect(),
            };
            let mut preview = space_template::preview(&template, destination, &occupied)?;
            if offline {
                preview.notices.push("Daemon unavailable. Creation checks live session and mailbox names before starting work.".into());
            }
            if prismattyc_mux::layout_path(&dir, destination)?.exists() {
                preview.conflicts.push(format!("Space {destination} already exists"));
            }
            if tail.is_empty() { println!("{}", preview.text()); }
            else { println!("{}", serde_json::to_string_pretty(&preview)?); }
        }
        [op, name, destination, tail @ ..] if op == "create" => {
            if tail.iter().any(|arg| arg != "--launch" && arg != "--no-attach" && arg != "--prepare-only") {
                bail!("usage: pmux space template create TEMPLATE SPACE [--launch] [--no-attach]");
            }
            create_from_template(paths, name, destination, tail.iter().any(|a| a == "--launch"))?;
            if tail.iter().any(|a| a == "--prepare-only") { return Ok(()); }
            let mut open = vec![destination.clone(), "--no-run".into()];
            if tail.iter().any(|a| a == "--no-attach") { open.push("--no-attach".into()); }
            apply_layout_args(paths, parse_space_open_args(open)?)?;
        }
        _ => bail!("usage: pmux space template ls\n       pmux space template save NAME --space SPACE\n       pmux space template preview NAME NEW_SPACE [--json]\n       pmux space template create NAME NEW_SPACE [--launch] [--no-attach]"),
    }
    Ok(())
}

fn create_from_template(
    paths: &Paths,
    template_name: &str,
    destination: &str,
    launch: bool,
) -> Result<()> {
    use prismattyc_mux::{space_team, space_template};
    let dir = spaces_dir();
    let template = space_template::load(&dir, template_name)?;
    ensure_live_server(paths)?;
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let journal = prismattyc_mux::layout_path(&dir.join("template-runs"), destination)?;
    let mut run: TemplateRun = if journal.exists() {
        let run: TemplateRun = serde_json::from_slice(&std::fs::read(&journal)?)?;
        if run.source != template || run.launch != launch {
            bail!("creation already started with a different template or launch choice");
        }
        if !prismattyc_mux::layout_path(&dir, destination)?.exists() {
            if run.complete || !run.launches.is_empty() {
                bail!("the destination Space was removed; choose a new name");
            }
            // Resume an interruption between intent and the ownership commit.
            let preview = space_template::preview(
                &run.source,
                destination,
                &occupied_names(&mut client)?.into_iter().collect(),
            )?;
            if !preview.conflicts.is_empty() {
                bail!("{}", preview.conflicts.join("; "));
            }
            store.commit(
                &mut client,
                Transaction {
                    releases: vec![],
                    pane_move: None,
                    deleted: vec![],
                    files: vec![(destination.into(), run.space.clone())],
                    sessions: vec![],
                    from: None,
                    to: run.space.id.clone(),
                },
            )?;
        }
        let saved = load_space(&dir, destination)?;
        if saved.id != run.space.id {
            bail!("the destination Space was replaced; choose a new name");
        }
        if run.complete {
            println!("{destination} already created; no commands repeated");
            return Ok(());
        }
        run
    } else {
        if prismattyc_mux::layout_path(&dir, destination)?.exists() {
            bail!("Space {destination:?} already exists");
        }
        let preview = space_template::preview(
            &template,
            destination,
            &occupied_names(&mut client)?.into_iter().collect(),
        )?;
        if !preview.conflicts.is_empty() {
            bail!("{}", preview.conflicts.join("; "));
        }
        let mut space = preview.definition;
        identify(&mut space)?;
        let run = TemplateRun {
            source: template,
            space,
            launch,
            launches: Default::default(),
            complete: false,
        };
        space_team::write_json(&journal, &run)?;
        store.commit(
            &mut client,
            Transaction {
                releases: vec![],
                pane_move: None,
                deleted: vec![],
                files: vec![(destination.into(), run.space.clone())],
                sessions: vec![],
                from: None,
                to: run.space.id.clone(),
            },
        )?;
        space_team::edit_metadata(&dir, &run.space, |meta| {
            *meta = preview.metadata;
            Ok(())
        })?;
        run
    };
    // Replay metadata after a crash before its initial write.
    let metadata = space_template::preview(&run.source, destination, &Default::default())?.metadata;
    space_team::edit_metadata(&dir, &run.space, |meta| {
        *meta = metadata;
        Ok(())
    })?;
    // The saved recipe is retained, but creating its panes always starts shells.
    let shells = space_template::shells_only(&run.space);
    for session in &shells.sessions {
        instantiate(&mut client, &shells, session)?;
    }
    if launch {
        // Launch writes need the identity returned by their own connection.
        client = Client::connect(&paths.socket)?;
        let registered = client.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })?;
        let ControlResponseData::ClientRegistered { client_id } = registered else {
            bail!("unexpected client registration response");
        };
        let snapshot = take_snapshot(&mut client)?;
        // Validate the entire plan before sending any command. A changed layout
        // must not silently drop recipes through a shorter zip iterator.
        for saved in &run.space.sessions {
            let live = snapshot
                .sessions
                .iter()
                .find(|s| s.name == saved.name && s.space_id == run.space.id)
                .context("template session changed ownership before launch")?;
            if live.windows.len() != saved.windows.len()
                || saved
                    .windows
                    .iter()
                    .zip(&live.windows)
                    .any(|(saved, live)| {
                        saved.root.pane_count() != live_leaf_ids(&live.layout).len()
                    })
            {
                bail!(
                    "{}: session layout changed; inspect it before launching the template",
                    saved.name
                );
            }
        }
        for saved in &run.space.sessions {
            let live = snapshot
                .sessions
                .iter()
                .find(|s| s.name == saved.name && s.space_id == run.space.id)
                .context("template session changed ownership before launch")?;
            for (wi, (saved_window, window)) in saved.windows.iter().zip(&live.windows).enumerate()
            {
                for (pi, (recipe, pane)) in space_template::commands(&saved_window.root)
                    .into_iter()
                    .zip(live_leaf_ids(&window.layout))
                    .enumerate()
                {
                    let Some(command) = recipe else {
                        continue;
                    };
                    let key = format!("{}:{wi}:{pi}", saved.name);
                    match run.launches.get(&key).map(String::as_str) {
                        Some("started") => continue,
                        Some(_) => bail!("{}: previous launch outcome is unknown; inspect that session before another launch", saved.name),
                        None => {}
                    }
                    // Record intent before writing. A crash cannot cause a blind replay.
                    run.launches.insert(key.clone(), "outcome unknown".into());
                    space_team::write_json(&journal, &run)?;
                    match write_pane_command(&mut client, client_id, pane, &command, true)? {
                        WritePaneCommand::Wrote => {
                            run.launches.insert(key, "started".into());
                        }
                        WritePaneCommand::SkipRunning => bail!(
                            "{} already has foreground work; launch was not repeated",
                            saved.name
                        ),
                    }
                    space_team::write_json(&journal, &run)?;
                }
            }
        }
    }
    run.complete = true;
    space_team::write_json(&journal, &run)?;
    println!(
        "created {destination}; {}",
        if launch {
            "requested commands started"
        } else {
            "shells only; saved commands not started"
        }
    );
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct Transaction {
    /// Sessions omitted by Save. Release them from `to` in the same journal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    releases: Vec<String>,
    #[serde(default)]
    pane_move: Option<PaneTransfer>,
    #[serde(default)]
    deleted: Vec<String>,
    files: Vec<(String, SavedSpace)>,
    sessions: Vec<String>,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PaneTransfer {
    pane: u64,
    child_pid: Option<u32>,
    source_session: String,
    target_session: String,
    from_space: String,
    to_space: String,
}

impl Store {
    fn acquire() -> Result<Self> {
        let dir = spaces_dir();
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".ownership.lock"))?;
        prismattyc_mux::platform::lock_exclusive(&file)?;
        Ok(Self(file))
    }

    pub(super) fn lock(client: &mut Client) -> Result<Self> {
        let store = Self::acquire()?;
        store.recover(client)?;
        store.recover_name(client)?;
        Ok(store)
    }

    fn recover(&self, client: &mut Client) -> Result<()> {
        let path = spaces_dir().join(".ownership-transaction");
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut tx: Transaction =
            serde_json::from_slice(&raw).context("read pending Space transfer")?;
        if let Some(moved) = &tx.pane_move {
            let snapshot = take_snapshot(client)?;
            let (current, pane) = snapshot.sessions.iter().find_map(|session| {
                session.windows.iter().flat_map(|w| &w.panes).find(|p| p.id == moved.pane).map(|p| (session, p))
            }).context("pending pane transfer lost its live pane; inspect the retained transaction before recovery")?;
            if pane.child_pid != moved.child_pid {
                bail!(
                    "pending pane transfer has a different process; refusing stale pane identity"
                );
            }
            let destination = snapshot
                .sessions
                .iter()
                .find(|s| s.name == moved.target_session)
                .context("pending pane transfer lost its destination session")?;
            if current.name != moved.target_session {
                if current.name != moved.source_session {
                    bail!("pending pane transfer source changed");
                }
                if let Err(error) = client.request(|request_id| ControlRequest::TransferSpacePane {
                    version: PROTOCOL_VERSION,
                    request_id,
                    pane_id: moved.pane,
                    to_session_id: destination.id,
                    from_space: moved.from_space.clone(),
                    to_space: moved.to_space.clone(),
                }) {
                    discard_unchanged_rejection(client, &path, &tx);
                    return Err(error);
                }
            }
            let after = take_snapshot(client)?;
            for (_, space) in &mut tx.files {
                for name in [&moved.source_session, &moved.target_session] {
                    let Some(index) = space.sessions.iter().position(|s| &s.name == name) else {
                        continue;
                    };
                    match after
                        .sessions
                        .iter()
                        .find(|s| &s.name == name)
                        .filter(|s| !s.windows.is_empty())
                    {
                        Some(live) => {
                            space.sessions[index] = from_sessions(&[live]).sessions.remove(0)
                        }
                        None => remove_record(space, name),
                    }
                }
            }
        }
        let snapshot = take_snapshot(client)?;
        let mut ids = Vec::new();
        for name in &tx.sessions {
            let Some(session) = snapshot.sessions.iter().find(|s| &s.name == name) else {
                continue; // A dead process will be restored from its saved definition.
            };
            if session.space_id == tx.to {
                continue;
            }
            if session.space_id != tx.from {
                bail!(
                    "pending Space transfer for {name:?} conflicts with live owner {:?}",
                    session.space_id
                );
            }
            ids.push(session.id);
        }
        let mut releases = Vec::new();
        for name in &tx.releases {
            let owner = tx.to.as_ref().context("saved Space release has no owner")?;
            if let Some(session) = snapshot.sessions.iter().find(|s| &s.name == name) {
                match session.space_id.as_ref() {
                    None => {} // A prior recovery already released it.
                    Some(current) if current == owner => releases.push(session.id),
                    _ => bail!("pending Space release for {name:?} conflicts with live owner"),
                }
            }
        }
        if let Err(error) = transfer(client, ids, tx.from.clone(), tx.to.clone()) {
            discard_unchanged_rejection(client, &path, &tx);
            return Err(error);
        }
        if let Err(error) = transfer(client, releases, tx.to.clone(), None) {
            discard_unchanged_rejection(client, &path, &tx);
            return Err(error);
        }
        for (name, space) in &tx.files {
            atomic_write(
                &prismattyc_mux::layout_path(&spaces_dir(), name)?,
                &serde_json::to_vec_pretty(space)?,
            )?;
        }
        for name in &tx.deleted {
            let removed = prismattyc_mux::layout_path(&spaces_dir(), name)?;
            match std::fs::remove_file(removed) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let (Some(from), Some(to)) = (&tx.from, &tx.to) {
            prismattyc_mux::space_team::transfer_roles(&spaces_dir(), from, to, &tx.sessions)?;
        }
        std::fs::remove_file(path)?;
        prismattyc_mux::platform::sync_directory(&spaces_dir())?;
        Ok(())
    }

    fn commit(&self, client: &mut Client, tx: Transaction) -> Result<()> {
        // Validate all files together, including unchanged definitions.
        let mut definitions = definitions()?;
        definitions.retain(|(name, _)| !tx.files.iter().any(|(updated, _)| updated == name));
        definitions.retain(|(name, _)| !tx.deleted.contains(name));
        definitions.extend(tx.files.iter().cloned());
        validate_definitions(&definitions)?;
        let snapshot = take_snapshot(client)?;
        for name in &tx.sessions {
            let session = snapshot
                .sessions
                .iter()
                .find(|s| &s.name == name)
                .with_context(|| format!("session {name:?} disappeared before transfer"))?;
            if session.space_id != tx.from && session.space_id != tx.to {
                bail!(
                    "session {name:?} already belongs to Space {:?}; use Move",
                    session.space_id
                );
            }
        }
        for name in &tx.releases {
            let owner = tx.to.as_ref().context("saved Space release has no owner")?;
            let session = snapshot
                .sessions
                .iter()
                .find(|s| &s.name == name)
                .with_context(|| format!("session {name:?} disappeared before release"))?;
            if session.space_id.as_ref() != Some(owner) || tx.sessions.contains(name) {
                bail!("session {name:?} changed ownership before saving");
            }
        }
        atomic_write(
            &spaces_dir().join(".ownership-transaction"),
            &serde_json::to_vec(&tx)?,
        )?;
        self.recover(client)
    }
}

#[derive(Serialize, Deserialize)]
struct SessionNaming {
    id: u64,
    old: String,
    new: String,
    panes: Vec<(u64, Option<u32>)>,
    files: Vec<(String, SavedSpace)>,
}

impl Store {
    fn recover_name(&self, client: &mut Client) -> Result<()> {
        let path = spaces_dir().join(".session-name-transaction");
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let tx: SessionNaming = serde_json::from_slice(&raw)?;
        let snapshot = take_snapshot(client)?;
        let live = snapshot.sessions.iter().find(|s| s.id == tx.id).context(
            "pending session rename lost its live session; inspect .session-name-transaction",
        )?;
        let panes: Vec<_> = live
            .windows
            .iter()
            .flat_map(|w| &w.panes)
            .map(|p| (p.id, p.child_pid))
            .collect();
        if (live.name != tx.old && live.name != tx.new) || panes != tx.panes {
            bail!("pending session rename has a different live seat; refusing stale identity");
        }
        // Idempotent when the daemon committed but the CLI did not save files.
        if let Err(error) = client.request(|request_id| ControlRequest::NameSession {
            version: PROTOCOL_VERSION,
            request_id,
            session_id: tx.id,
            name: tx.new.clone(),
        }) {
            if let Ok(after) = take_snapshot(client) {
                if after
                    .sessions
                    .iter()
                    .any(|s| s.id == tx.id && s.name == tx.old && s.agent_id == live.agent_id)
                {
                    std::fs::remove_file(&path)?;
                }
            }
            return Err(error);
        }
        for (name, space) in tx.files {
            prismattyc_mux::space_team::edit_metadata(&spaces_dir(), &space, |meta| {
                if let Some(role) = meta.roles.remove(&tx.old) {
                    meta.roles.insert(tx.new.clone(), role);
                }
                Ok(())
            })?;
            atomic_write(
                &prismattyc_mux::layout_path(&spaces_dir(), &name)?,
                &serde_json::to_vec_pretty(&space)?,
            )?;
        }
        std::fs::remove_file(path)?;
        prismattyc_mux::platform::sync_directory(&spaces_dir())?;
        Ok(())
    }
}

pub(super) fn name_session(paths: &Paths, args: Vec<String>) -> Result<()> {
    let (name, key) = match args.as_slice() {
        [name] => (name.as_str(), None),
        [name, flag, key] if flag == "--session" => (name.as_str(), Some(key.as_str())),
        _ => bail!("usage: pmux session name NAME [--session KEY]"),
    };
    let name = prismattyc_mux::mailbox::AgentId::new(name.trim().to_string())?.to_string();
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let snapshot = take_snapshot(&mut client)?;
    let id = if let Some(key) = key {
        snapshot
            .sessions
            .iter()
            .find(|s| s.name == key || s.id.to_string() == key)
            .map(|s| s.id)
    } else {
        caller_session_id(paths, &snapshot)
    }
    .context("session not found; use --session NAME or run inside a pmux pane")?;
    let live = snapshot.sessions.iter().find(|s| s.id == id).unwrap();
    let mut files = definitions()?;
    for (_, space) in &mut files {
        for record in &mut space.sessions {
            if record.name != live.name {
                if record.name == name || record.agent.as_deref() == Some(&name) {
                    bail!("name {name:?} belongs to another saved session");
                }
                continue;
            }
            record.name.clone_from(&name);
            record.agent = Some(name.clone());
            for window in &mut record.windows {
                if window.title == live.name || window.title == "shell" {
                    window.title.clone_from(&name);
                }
            }
        }
        for tab in &mut space.tabs {
            if tab.sessions.len() == 1
                && tab.sessions[0] == live.name
                && (tab.title == live.name || tab.title == "shell")
            {
                tab.title.clone_from(&name);
            }
            for session in &mut tab.sessions {
                if session == &live.name {
                    session.clone_from(&name);
                }
            }
        }
        if space.focused_session.as_deref() == Some(&live.name) {
            space.focused_session = Some(name.clone());
        }
    }
    validate_definitions(&files)?;
    files.retain(|(_, space)| space.sessions.iter().any(|record| record.name == name));
    let tx = SessionNaming {
        id,
        old: live.name.clone(),
        new: name.clone(),
        panes: live
            .windows
            .iter()
            .flat_map(|w| &w.panes)
            .map(|p| (p.id, p.child_pid))
            .collect(),
        files,
    };
    atomic_write(
        &spaces_dir().join(".session-name-transaction"),
        &serde_json::to_vec(&tx)?,
    )?;
    store.recover_name(&mut client)?;
    println!("session: {name}\nagent: {name}\nid: {id}");
    Ok(())
}

/// A rejected operation must not block later work. Remove its intent only
/// after a fresh snapshot proves that the live transfer never happened.
/// Uncertain transport failures and partially applied transfers keep the journal.
fn discard_unchanged_rejection(client: &mut Client, path: &Path, tx: &Transaction) {
    let Ok(snapshot) = take_snapshot(client) else {
        return;
    };
    let unchanged = if let Some(moved) = &tx.pane_move {
        snapshot
            .sessions
            .iter()
            .filter(|s| {
                s.name == moved.source_session && s.space_id.as_deref() == Some(&moved.from_space)
            })
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.panes)
            .any(|pane| pane.id == moved.pane && pane.child_pid == moved.child_pid)
    } else {
        tx.sessions.iter().all(|name| {
            snapshot
                .sessions
                .iter()
                .any(|s| &s.name == name && s.space_id == tx.from)
        })
    };
    let releases_unchanged = tx.releases.iter().all(|name| {
        snapshot
            .sessions
            .iter()
            .any(|s| &s.name == name && s.space_id == tx.to)
    });
    if unchanged && releases_unchanged {
        let _ = std::fs::remove_file(path);
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = prismattyc_mux::platform::unlock(&self.0);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("pending");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    prismattyc_mux::platform::replace_file(&temporary, path)?;
    if let Some(parent) = path.parent() {
        prismattyc_mux::platform::sync_directory(parent)?;
    }
    Ok(())
}

fn definitions() -> Result<Vec<(String, SavedSpace)>> {
    list_spaces(&spaces_dir())?
        .into_iter()
        .map(|entry| Ok((entry.name.clone(), load_space(&spaces_dir(), &entry.name)?)))
        .collect()
}

fn validate_definitions(files: &[(String, SavedSpace)]) -> Result<()> {
    let mut sessions = std::collections::HashMap::new();
    let mut ids = std::collections::HashMap::new();
    for (name, space) in files {
        if let Some(id) = &space.id {
            if let Some(other) = ids.insert(id, name) {
                bail!("Spaces {other:?} and {name:?} use the same identity; create a fresh Space");
            }
        }
        for session in &space.sessions {
            if let Some((other, owned)) = sessions.insert(&session.name, (name, space.id.is_some()))
            {
                if !owned && space.id.is_none() {
                    continue;
                }
                bail!("session {:?} is listed by both {other:?} and {name:?}; exclusive ownership requires resolving the legacy conflict", session.name);
            }
        }
    }
    Ok(())
}

fn identify(space: &mut SavedSpace) -> Result<String> {
    if space.id.is_none() {
        space.id = Some(prismattyc_mux::new_space_id()?);
    }
    space.version = prismattyc_mux::OWNED_SPACE_VERSION;
    Ok(space.id.clone().expect("assigned Space identity"))
}

fn transfer(
    client: &mut Client,
    session_ids: Vec<u64>,
    from: Option<String>,
    to: Option<String>,
) -> Result<()> {
    // Even an empty request verifies that this daemon understands ownership.
    client
        .request(|request_id| ControlRequest::TransferSpaceSessions {
            version: PROTOCOL_VERSION,
            request_id,
            session_ids,
            from,
            to,
        })
        .context("daemon does not support this Space transfer; no shared-session fallback")?;
    Ok(())
}

fn connect(paths: &Paths) -> Result<Client> {
    require_live_socket(paths)?;
    let mut client = Client::connect(&paths.socket)?;
    client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    Ok(client)
}

/// Migrate only an unambiguous definition. Never assign the global `default`
/// session to several Spaces or choose a legacy owner by directory order.
pub(super) fn prepare_open(store: &Store, client: &mut Client, name: &str) -> Result<SavedSpace> {
    let mut all = definitions()?;
    let mut space = load_space(&spaces_dir(), name)?;
    let legacy = space.id.is_none();
    let id = identify(&mut space)?;
    all.retain(|(other, _)| other != name);
    all.push((name.into(), space.clone()));
    validate_definitions(&all)?;
    let snapshot = take_snapshot(client)?;
    let mut claims = Vec::new();
    for saved in &space.sessions {
        if let Some(live) = snapshot.sessions.iter().find(|s| s.name == saved.name) {
            if let Some(owner) = &live.space_id {
                if owner != &id {
                    bail!(
                        "session {:?} belongs to another Space; use Move",
                        saved.name
                    );
                }
            } else {
                claims.push(live.name.clone());
            }
        }
    }
    if legacy {
        let backup_dir = spaces_dir().join("legacy-backups");
        std::fs::create_dir_all(&backup_dir)?;
        let original = prismattyc_mux::layout_path(&spaces_dir(), name)?;
        let backup = prismattyc_mux::layout_path(&backup_dir, name)?;
        if !backup.exists() {
            std::fs::copy(original, backup)?;
        }
    }
    store.commit(
        client,
        Transaction {
            releases: vec![],
            pane_move: None,
            deleted: vec![],
            sessions: claims,
            from: None,
            to: Some(id),
            files: if legacy {
                vec![(name.into(), space.clone())]
            } else {
                vec![]
            },
        },
    )?;
    Ok(space)
}

pub(super) fn claim_saved_session(
    client: &mut Client,
    space: &SavedSpace,
    name: &str,
) -> Result<()> {
    let snapshot = take_snapshot(client)?;
    let session = snapshot
        .sessions
        .iter()
        .find(|s| s.name == name)
        .context("restored session missing")?;
    if session.space_id == space.id {
        return Ok(());
    }
    transfer(client, vec![session.id], None, space.id.clone())
}

pub(super) fn save_owned(
    client: &mut Client,
    name: &str,
    space: &mut SavedSpace,
) -> Result<PathBuf> {
    let store = Store::lock(client)?;
    let path = prismattyc_mux::layout_path(&spaces_dir(), name)?;
    if path.exists() {
        let original = prepare_open(&store, client, name)?;
        space.id = original.id;
        space.created_at_unix_ms = original.created_at_unix_ms;
    }
    let id = identify(space)?;
    let snapshot = take_snapshot(client)?;
    let mut claims = Vec::new();
    for saved in &space.sessions {
        let live = snapshot
            .sessions
            .iter()
            .find(|s| s.name == saved.name)
            .context("session disappeared while saving")?;
        match &live.space_id {
            Some(owner) if owner != &id => bail!(
                "session {:?} belongs to another Space; use Move, or + for a fresh Space",
                saved.name
            ),
            None => claims.push(live.name.clone()),
            _ => {}
        }
    }
    let releases = snapshot
        .sessions
        .iter()
        .filter(|live| {
            live.space_id.as_ref() == Some(&id)
                && !space.sessions.iter().any(|saved| saved.name == live.name)
        })
        .map(|live| live.name.clone())
        .collect();
    store.commit(
        client,
        Transaction {
            releases,
            pane_move: None,
            deleted: vec![],
            files: vec![(name.into(), space.clone())],
            sessions: claims,
            from: None,
            to: Some(id),
        },
    )?;
    Ok(path)
}

fn occupied_names(client: &mut Client) -> Result<Vec<String>> {
    let response = client.request(|request_id| ControlRequest::SessionNames {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::SessionNames { mut names } = response else {
        bail!("unexpected session names response");
    };
    for (_, space) in definitions()? {
        names.extend(
            space
                .sessions
                .into_iter()
                .flat_map(|s| std::iter::once(s.name).chain(s.agent)),
        );
    }
    Ok(names)
}

pub(super) fn suggest_name(client: &mut Client, space: &str) -> Result<String> {
    Ok(prismattyc_mux::session_name::suggest(
        space,
        &occupied_names(client)?,
    ))
}

fn fresh_record(
    client: &mut Client,
    space: &str,
    requested: Option<&str>,
) -> Result<prismattyc_mux::SavedSpaceSession> {
    let name = match requested {
        Some(name) => prismattyc_mux::mailbox::AgentId::new(name.trim().to_string())?.to_string(),
        None => suggest_name(client, space)?,
    };
    if occupied_names(client)?.contains(&name) {
        bail!("session name {name:?} is already in use");
    }
    let mut record = prismattyc_mux::stub_space_session(name.clone());
    record.agent = Some(name.clone());
    record.windows[0].title = name;
    if let SavedNode::Leaf { cwd, .. } = &mut record.windows[0].root {
        *cwd = prismattyc_mux::platform::home_dir()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
    }
    Ok(record)
}

fn instantiate(
    client: &mut Client,
    space: &SavedSpace,
    record: &prismattyc_mux::SavedSpaceSession,
) -> Result<()> {
    let layout = SavedLayout {
        version: 1,
        saved_at_unix: space.saved_at_unix,
        session: record.name.clone(),
        windows: record.windows.clone(),
    };
    let snapshot = take_snapshot(client)?;
    apply_saved_layout(
        client,
        &snapshot,
        &layout,
        &record.name,
        record.agent.clone(),
        ApplyExisting::Skip,
    )?;
    claim_saved_session(client, space, &record.name)
}

/// Recreate one saved seat without opening its other sessions or changing views.
pub(super) fn reopen(paths: &Paths, args: Vec<String>) -> Result<()> {
    let [name, flag, space_name] = args.as_slice() else {
        bail!("usage: pmux session reopen NAME --space SPACE");
    };
    if flag != "--space" {
        bail!("usage: pmux session reopen NAME --space SPACE");
    }
    ensure_live_server(paths)?;
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let space = prepare_open(&store, &mut client, space_name)?;
    let record = space
        .sessions
        .iter()
        .find(|record| record.name == *name)
        .context("session is not saved in this Space")?;
    instantiate(&mut client, &space, record)?;
    println!("reopened {name} in {space_name}");
    Ok(())
}

pub(super) fn create(paths: &Paths, args: Vec<String>) -> Result<()> {
    let mut args = args;
    let requested = if let Some(index) = args.iter().position(|arg| arg == "--session-name") {
        args.remove(index);
        if index >= args.len() {
            bail!("--session-name needs a name");
        }
        Some(args.remove(index))
    } else {
        None
    };
    let mut parsed = parse_space_open_args(args)?;
    let ApplyArgs::Space {
        name,
        no_run,
        replace,
        add,
        ..
    } = &mut parsed
    else {
        unreachable!()
    };
    if *replace || *add {
        bail!("space create does not accept --replace or --add");
    }
    *no_run = true;
    ensure_live_server(paths)?;
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let path = prismattyc_mux::layout_path(&spaces_dir(), name)?;
    if path.exists() {
        bail!("Space {name:?} already exists");
    }
    let record = fresh_record(&mut client, name, requested.as_deref())?;
    let mut space = from_sessions(&[]);
    identify(&mut space)?;
    prismattyc_mux::space_add_session(&mut space, record.clone(), Some(&record.name))?;
    // Persist intent before spawn. An interrupted create can be resumed by open.
    store.commit(
        &mut client,
        Transaction {
            releases: vec![],
            pane_move: None,
            deleted: vec![],
            files: vec![(name.clone(), space.clone())],
            sessions: vec![],
            from: None,
            to: space.id.clone(),
        },
    )?;
    instantiate(&mut client, &space, &record)?;
    drop(store);
    drop(client);
    apply_layout_args(paths, parsed)
}

pub(super) fn add(paths: &Paths, args: Vec<String>) -> Result<()> {
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let mut args = args.into_iter();
    let name = args.next().context(SPACE_ADD_USAGE)?;
    validate_layout_name(&name)?;
    let mut session = None;
    let mut tab = None;
    let mut requested = None;
    while let Some(arg) = args.next() {
        let value = args.next().context(SPACE_ADD_USAGE)?;
        match arg.as_str() {
            "--session" if session.is_none() => session = Some(value),
            "--tab" if tab.is_none() => tab = Some(value),
            "--name" if requested.is_none() => requested = Some(value),
            _ => bail!("{SPACE_ADD_USAGE}"),
        }
    }
    if session.is_some() && requested.is_some() {
        bail!("use --session for an existing session or --name for a new one");
    }
    let mut space = prepare_open(&store, &mut client, &name)?;
    let record = if let Some(key) = session {
        let snapshot = take_snapshot(&mut client)?;
        let live = resolve_space_sessions(&snapshot, &[key])?[0];
        if live.space_id.is_some() {
            bail!(
                "session {:?} already has a Space owner; use Move",
                live.name
            );
        }
        from_sessions(&[live]).sessions.remove(0)
    } else {
        fresh_record(&mut client, &name, requested.as_deref())?
    };
    let exists = take_snapshot(&mut client)?
        .sessions
        .iter()
        .any(|s| s.name == record.name);
    let title = tab
        .as_deref()
        .or_else(|| record.windows.first().map(|window| window.title.as_str()));
    prismattyc_mux::space_add_session(&mut space, record.clone(), title)?;
    store.commit(
        &mut client,
        Transaction {
            releases: vec![],
            pane_move: None,
            deleted: vec![],
            files: vec![(name.clone(), space.clone())],
            sessions: if exists {
                vec![record.name.clone()]
            } else {
                vec![]
            },
            from: None,
            to: space.id.clone(),
        },
    )?;
    if !exists {
        instantiate(&mut client, &space, &record)?;
    }
    println!("added {} to {name}", record.name);
    Ok(())
}

fn remove_record(space: &mut SavedSpace, name: &str) {
    space.sessions.retain(|s| s.name != name);
    for tab in &mut space.tabs {
        tab.sessions.retain(|s| s != name);
    }
    space.tabs.retain(|t| !t.sessions.is_empty());
    space.active_tab = space.active_tab.min(space.tabs.len().saturating_sub(1));
    if space.focused_session.as_deref() == Some(name) {
        space.focused_session = None;
    }
}

pub(super) fn rename(paths: &Paths, args: Vec<String>) -> Result<()> {
    let [old, new] = args.as_slice() else {
        bail!("usage: pmux space rename OLD NEW");
    };
    validate_layout_name(old)?;
    validate_layout_name(new)?;
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let space = prepare_open(&store, &mut client, old)?;
    if old == new {
        return Ok(());
    }
    if prismattyc_mux::layout_path(&spaces_dir(), new)?.exists() {
        bail!("Space {new:?} already exists");
    }
    store.commit(
        &mut client,
        Transaction {
            releases: vec![],
            pane_move: None,
            deleted: vec![old.clone()],
            files: vec![(new.clone(), space.clone())],
            sessions: vec![],
            from: space.id.clone(),
            to: space.id,
        },
    )?;
    println!("renamed {old} to {new}");
    Ok(())
}

pub(super) fn remove(paths: &Paths, args: Vec<String>) -> Result<()> {
    let (args, undo_file) = undo_argument(args)?;
    let kill = args.iter().any(|arg| arg == "--kill");
    if args.iter().filter(|arg| *arg == "--kill").count() > 1 {
        bail!("--kill may be specified only once");
    }
    let args = args.into_iter().filter(|arg| arg != "--kill").collect();
    let (name, key, _) = parse_space_session_args(args, SPACE_REMOVE_USAGE, false)?;
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let mut space = prepare_open(&store, &mut client, &name)?;
    let snapshot = take_snapshot(&mut client)?;
    let live = snapshot
        .sessions
        .iter()
        .find(|s| s.name == key || s.id.to_string() == key);
    let session = live.map_or(key.as_str(), |s| s.name.as_str());
    if !space.sessions.iter().any(|s| s.name == session) {
        bail!("session {session:?} is not in Space {name:?}");
    }
    remove_record(&mut space, session);
    store.commit_reversible(
        &mut client,
        if kill { None } else { undo_file.as_deref() },
        Transaction {
            releases: vec![],
            pane_move: None,
            deleted: vec![],
            files: vec![(name, space.clone())],
            sessions: live.map(|s| vec![s.name.clone()]).unwrap_or_default(),
            from: space.id,
            to: None,
        },
    )?;
    if kill {
        if let Some(live) = live {
            // Keep the original connection and numeric identity. A renamed or
            // replaced name must never select another session for destruction.
            destroy_session(&mut client, live.id)
                .context("membership removed, but killing the session failed")?;
        }
    }
    Ok(())
}

pub(super) fn move_work(paths: &Paths, args: Vec<String>) -> Result<()> {
    let (args, undo_file) = undo_argument(args)?;
    let mut rest = args.into_iter();
    let target = rest.next().context(
        "usage: pmux space move NAME (--session S | --session-id ID | --pane P [--to-session S])",
    )?;
    let mut session = None;
    let mut session_id = None;
    let mut pane = None;
    let mut destination = None;
    while let Some(arg) = rest.next() {
        let value = rest
            .next()
            .with_context(|| format!("{arg} requires a value"))?;
        match arg.as_str() {
            "--session" if session.is_none() => session = Some(value),
            "--session-id" if session_id.is_none() => {
                let id = value
                    .parse::<u64>()
                    .context("--session-id requires a positive numeric ID")?;
                anyhow::ensure!(id != 0, "--session-id requires a positive numeric ID");
                session_id = Some(id);
            }
            "--pane" if pane.is_none() => pane = Some(value.parse::<u64>()?),
            "--to-session" if destination.is_none() => destination = Some(value),
            _ => bail!("unknown or repeated move argument {arg:?}"),
        }
    }
    let whole_session = session.is_some() || session_id.is_some();
    if usize::from(session.is_some())
        + usize::from(session_id.is_some())
        + usize::from(pane.is_some())
        != 1
        || (whole_session && destination.is_some())
    {
        bail!("specify exactly one of --session S, --session-id ID, or --pane P [--to-session S]");
    }
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let target_space = prepare_open(&store, &mut client, &target)?;
    let snapshot = take_snapshot(&mut client)?;
    let source = snapshot
        .sessions
        .iter()
        .find(|s| match (&session, session_id, pane) {
            (_, Some(id), _) => s.id == id,
            (Some(key), _, _) => s.name == *key || s.id.to_string() == *key,
            (_, _, Some(pane)) => s
                .windows
                .iter()
                .any(|w| w.panes.iter().any(|p| p.id == pane)),
            _ => false,
        })
        .context("source session or pane is not live")?;
    // A newly nested session may not belong to a Space yet. Claim its whole
    // seat directly, without first changing the parent Space or restarting it.
    if source.space_id.is_none() {
        let pane_count = source.windows.iter().map(|w| w.panes.len()).sum::<usize>();
        anyhow::ensure!(
            destination.is_none() && (whole_session || pane_count == 1),
            "unassigned session has multiple panes; use Move session to space"
        );
        let mut to = target_space;
        let record = from_sessions(&[source]).sessions.remove(0);
        let title = source.windows.first().map(|window| window.title.as_str());
        prismattyc_mux::space_add_session(&mut to, record, title)?;
        store.commit_reversible(
            &mut client,
            undo_file.as_deref(),
            Transaction {
                releases: vec![],
                pane_move: None,
                deleted: vec![],
                sessions: vec![source.name.clone()],
                from: None,
                to: to.id.clone(),
                files: vec![(target.clone(), to)],
            },
        )?;
        println!("moved session {} to {target}", source.name);
        return Ok(());
    }
    let (source_name, source_space) = definitions()?
        .into_iter()
        .find(|(_, s)| s.id.is_some() && s.id == source.space_id)
        .context("source session has no owning Space; use Add for an unassigned session")?;
    if source_space.id == target_space.id {
        bail!("source already belongs to {target:?}");
    }
    // With one pane, the session is the seat. Moving it as a whole keeps
    // the shared session/mailbox name and every live identity unchanged.
    let whole_seat = destination.is_none()
        && source
            .windows
            .iter()
            .map(|window| window.panes.len())
            .sum::<usize>()
            == 1;
    if let Some(pane) = pane.filter(|_| !whole_seat) {
        return move_pane(
            &store,
            &mut client,
            source_name,
            source_space,
            target,
            target_space,
            source,
            pane,
            destination,
            undo_file.as_deref(),
        );
    }
    let mut from = source_space;
    let mut to = target_space;
    let record = from_sessions(&[source]).sessions.remove(0);
    let title = from
        .tabs
        .iter()
        .find(|tab| tab.sessions.contains(&source.name))
        .map(|tab| tab.title.clone())
        .or_else(|| source.windows.first().map(|window| window.title.clone()));
    remove_record(&mut from, &source.name);
    prismattyc_mux::space_add_session(&mut to, record, title.as_deref())?;
    store.commit_reversible(
        &mut client,
        undo_file.as_deref(),
        Transaction {
            releases: vec![],
            pane_move: None,
            deleted: vec![],
            sessions: vec![source.name.clone()],
            from: from.id.clone(),
            to: to.id.clone(),
            files: vec![(source_name, from), (target.clone(), to)],
        },
    )?;
    println!("moved session {} to {target}", source.name);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn move_pane(
    store: &Store,
    client: &mut Client,
    source_name: String,
    source_space: SavedSpace,
    target: String,
    mut target_space: SavedSpace,
    source: &SessionSnapshot,
    pane: u64,
    destination: Option<String>,
    undo_file: Option<&Path>,
) -> Result<()> {
    let live_pane = source
        .windows
        .iter()
        .flat_map(|w| &w.panes)
        .find(|p| p.id == pane)
        .context("source pane disappeared")?;
    let snapshot = take_snapshot(client)?;
    let created_destination = destination.is_none();
    let destination_name = if let Some(key) = destination {
        let session = resolve_space_sessions(&snapshot, &[key])?[0];
        if session.space_id != target_space.id {
            bail!("destination session is not owned by {target:?}");
        }
        let moving_agent = source
            .windows
            .first()
            .and_then(|window| window.panes.first())
            .is_some_and(|first| first.id == pane)
            && source.agent_id.is_some();
        if moving_agent && session.agent_id.is_some() {
            bail!("destination session already has an agent; omit --to-session to preserve the moved agent in a new session");
        }
        session.name.clone()
    } else {
        let mut record = fresh_record(client, &target, None)?;
        record.agent = None; // TransferSpacePane carries the source binding.
        if !live_pane.title.is_empty() {
            record.windows[0].title = live_pane.title.clone();
        }
        // A headless destination has no temporary shell or PTY to discard.
        let response = client.request(|request_id| ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id,
            name: record.name.clone(),
            spawn: default_layout_spawn(None),
            cols: None,
            rows: None,
            agent_id: None,
            headless: true,
        })?;
        let ControlResponseData::Session { session_id, .. } = response else {
            bail!("unexpected create session response");
        };
        transfer(client, vec![session_id], None, target_space.id.clone())?;
        prismattyc_mux::space_add_session(
            &mut target_space,
            record.clone(),
            Some(&record.windows[0].title),
        )?;
        record.name
    };
    let transfer = PaneTransfer {
        pane,
        child_pid: live_pane.child_pid,
        source_session: source.name.clone(),
        target_session: destination_name.clone(),
        from_space: source_space
            .id
            .clone()
            .context("source Space identity missing")?,
        to_space: target_space
            .id
            .clone()
            .context("destination Space identity missing")?,
    };
    let result = store.commit_reversible(
        client,
        undo_file,
        Transaction {
            releases: vec![],
            pane_move: Some(transfer),
            deleted: vec![],
            sessions: vec![],
            from: None,
            to: None,
            files: vec![(source_name, source_space), (target.clone(), target_space)],
        },
    );
    if result.is_err()
        && created_destination
        && !spaces_dir().join(".ownership-transaction").exists()
    {
        if let Ok(snapshot) = take_snapshot(client) {
            if let Some(empty) = snapshot
                .sessions
                .iter()
                .find(|session| session.name == destination_name && session.windows.is_empty())
            {
                let _ = client.request(|request_id| ControlRequest::DestroySession {
                    version: PROTOCOL_VERSION,
                    request_id,
                    session_id: empty.id,
                });
            }
        }
    }
    result?;
    println!("moved pane {pane} to {target}");
    Ok(())
}

pub(super) fn delete_spaces(paths: &Paths, args: Vec<String>, clear: bool) -> Result<()> {
    let names = if clear {
        let keep = parse_clear_keep(args, SPACE_CLEAR_USAGE, false)?.keep;
        list_spaces(&spaces_dir())?
            .into_iter()
            .map(|s| s.name)
            .filter(|name| !keep.contains(name))
            .collect::<Vec<_>>()
    } else if args == ["--all"] {
        list_spaces(&spaces_dir())?
            .into_iter()
            .map(|s| s.name)
            .collect()
    } else {
        if args.is_empty() || args.iter().any(|arg| arg.starts_with('-')) {
            bail!("{SPACE_RM_USAGE}");
        }
        args
    };
    for name in &names {
        validate_layout_name(name)?;
    }
    if names.is_empty() {
        return Ok(());
    }
    if !matches!(probe_socket_liveness(&paths.socket), SocketLiveness::Live) {
        let _store = Store::acquire()?;
        if spaces_dir().join(".ownership-transaction").exists() {
            bail!("finish pending Space transfer before offline deletion");
        }
        for name in names {
            remove_space(&spaces_dir(), &name)?;
            println!("removed {name}");
        }
        return Ok(());
    }
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    for name in names {
        let space = load_space(&spaces_dir(), &name)?;
        let snapshot = take_snapshot(&mut client)?;
        let owned = space
            .id
            .as_ref()
            .map(|id| {
                snapshot
                    .sessions
                    .iter()
                    .filter(|s| s.space_id.as_ref() == Some(id))
                    .map(|s| s.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        store.commit(
            &mut client,
            Transaction {
                releases: vec![],
                pane_move: None,
                deleted: vec![name.clone()],
                files: vec![],
                sessions: owned,
                from: space.id,
                to: None,
            },
        )?;
        println!("removed {name}; sessions remain alive and unassigned");
    }
    Ok(())
}

/// The receipt is local to one window. The store lock protects the compare and
/// inverse transaction; concurrent changes invalidate undo rather than being lost.
#[derive(Serialize, Deserialize)]
struct UndoReceipt {
    socket_identity: String,
    inverse: Transaction,
    after_files: Vec<(String, SavedSpace)>,
    identities: serde_json::Value,
    names: Vec<String>,
}

fn undo_argument(mut args: Vec<String>) -> Result<(Vec<String>, Option<PathBuf>)> {
    let path = if let Some(index) = args.iter().position(|arg| arg == "--undo-file") {
        args.remove(index);
        if index >= args.len() {
            bail!("--undo-file requires a path");
        }
        Some(PathBuf::from(args.remove(index)))
    } else {
        None
    };
    Ok((args, path))
}

fn undo_identities(snapshot: &Snapshot, names: &[String]) -> serde_json::Value {
    serde_json::Value::Array(names.iter().map(|name| {
        snapshot.sessions.iter().find(|s| &s.name == name).map(|s| serde_json::json!({
            "name":s.name,"id":s.id,"owner":s.space_id,
            "panes":s.windows.iter().flat_map(|w| &w.panes).map(|p| (p.id,p.child_pid)).collect::<Vec<_>>()
        })).unwrap_or(serde_json::Value::Null)
    }).collect())
}

impl Store {
    fn commit_reversible(
        &self,
        client: &mut Client,
        path: Option<&Path>,
        tx: Transaction,
    ) -> Result<()> {
        let receipt = path
            .map(|_| -> Result<_> {
                let mut inverse = tx.clone();
                inverse.from = tx.to.clone();
                inverse.to = tx.from.clone();
                inverse.files = tx
                    .files
                    .iter()
                    .map(|(name, _)| Ok((name.clone(), load_space(&spaces_dir(), name)?)))
                    .collect::<Result<_>>()?;
                if let Some(moved) = inverse.pane_move.as_mut() {
                    std::mem::swap(&mut moved.source_session, &mut moved.target_session);
                    std::mem::swap(&mut moved.from_space, &mut moved.to_space);
                    if !inverse.files.iter().any(|(_, space)| {
                        space
                            .sessions
                            .iter()
                            .any(|s| s.name == moved.source_session)
                    }) {
                        inverse.releases.push(moved.source_session.clone());
                        inverse.to = Some(moved.from_space.clone());
                    }
                }
                let mut names = tx.sessions.clone();
                if let Some(moved) = &tx.pane_move {
                    names.extend([moved.source_session.clone(), moved.target_session.clone()]);
                }
                Ok((inverse, names))
            })
            .transpose()?;
        self.commit(client, tx)?;
        if let (Some(path), Some((inverse, names))) = (path, receipt) {
            let result = (|| -> Result<()> {
                let after_files = inverse
                    .files
                    .iter()
                    .map(|(name, _)| Ok((name.clone(), load_space(&spaces_dir(), name)?)))
                    .collect::<Result<_>>()?;
                let identities = undo_identities(&take_snapshot(client)?, &names);
                atomic_write(
                    path,
                    &serde_json::to_vec(&UndoReceipt {
                        socket_identity: client.socket_identity.clone(),
                        inverse,
                        after_files,
                        identities,
                        names,
                    })?,
                )
            })();
            if let Err(error) = result {
                eprintln!("change completed, but undo could not be recorded: {error}");
            }
        }
        Ok(())
    }
}

fn undo(paths: &Paths, path: &Path) -> Result<()> {
    let mut client = connect(paths)?;
    let store = Store::lock(&mut client)?;
    let receipt: UndoReceipt = serde_json::from_slice(&std::fs::read(path)?)?;
    if receipt.socket_identity != client.socket_identity {
        bail!("daemon restarted; this undo belongs to the previous daemon");
    }
    for (name, expected) in &receipt.after_files {
        let current = load_space(&spaces_dir(), name)?;
        if serde_json::to_value(current)? != serde_json::to_value(expected)? {
            bail!("Space {name:?} changed after this action; undo would replace newer changes");
        }
    }
    if undo_identities(&take_snapshot(&mut client)?, &receipt.names) != receipt.identities {
        bail!(
            "session moved, changed, or restarted after this action; undo is no longer available"
        );
    }
    if let Some(moved) = &receipt.inverse.pane_move {
        if !take_snapshot(&mut client)?
            .sessions
            .iter()
            .any(|s| s.name == moved.target_session)
        {
            bail!("original session no longer exists; undo cannot restore the pane safely");
        }
    }
    store.commit(&mut client, receipt.inverse)?;
    std::fs::remove_file(path)?;
    println!("Previous Space membership restored; processes kept running");
    Ok(())
}
