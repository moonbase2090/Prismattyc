// SPDX-License-Identifier: MIT
//! Backed-up recovery for saved Spaces, independent of definition validation.

use super::*;
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::time::UNIX_EPOCH;

const USAGE: &str = "usage: pmux space reset NAME --confirm NAME | --all --confirm 'RESET ALL'";

pub(in super::super) fn run(paths: &Paths, args: Vec<String>) -> Result<()> {
    let [target, flag, confirmation] = args.as_slice() else {
        bail!("{USAGE}");
    };
    anyhow::ensure!(flag == "--confirm", "{USAGE}");
    let all = target == "--all";
    if !all {
        validate_layout_name(target)?;
    }
    let expected = if all { "RESET ALL" } else { target.as_str() };
    anyhow::ensure!(
        confirmation == expected,
        "confirmation must exactly match {expected:?}"
    );
    let mut client = connect(paths)?;
    let _store = Store::acquire()?;
    let dir = spaces_dir();
    for journal in [".ownership-transaction", ".session-name-transaction"] {
        anyhow::ensure!(
            !dir.join(journal).exists(),
            "finish pending Space transaction before reset"
        );
    }
    let snapshot = take_snapshot(&mut client)?;
    let mut selected = Vec::new();
    let mut selected_owners = HashSet::new();
    let mut retained_owners = HashSet::new();
    let mut unknown_selected = false;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|v| v.to_str())
            .context("Space name is not UTF-8")?;
        validate_layout_name(name)?;
        anyhow::ensure!(
            entry.file_type()?.is_file(),
            "Space definition must be a regular file"
        );
        let mut raw = Vec::new();
        prismattyc_mux::private_fs::open(&path, std::fs::OpenOptions::new().read(true))?
            .read_to_end(&mut raw)?;
        let owner = serde_json::from_slice::<serde_json::Value>(&raw)
            .ok()
            .and_then(|value| value.get("id")?.as_str().map(str::to_owned))
            .filter(|id| prismattyc_mux::valid_space_id(id));
        if all || name == target {
            if let Some(id) = owner {
                selected_owners.insert(id);
            } else {
                unknown_selected = true;
            }
            selected.push((path, raw));
        } else if let Some(id) = owner {
            retained_owners.insert(id);
        }
    }
    if !all {
        anyhow::ensure!(
            selected_owners.is_disjoint(&retained_owners),
            "Space identity is shared by another definition; use reset --all"
        );
        if unknown_selected {
            anyhow::ensure!(
                snapshot.sessions.iter().all(|s| s
                    .space_id
                    .as_ref()
                    .is_none_or(|id| retained_owners.contains(id))),
                "cannot identify ownership of the broken Space; use reset --all"
            );
        }
    }
    selected.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<_> = selected
        .iter()
        .map(|(path, _)| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    let order = dir.join(".space-order");
    if all && order.exists() {
        let mut raw = Vec::new();
        prismattyc_mux::private_fs::open(&order, std::fs::OpenOptions::new().read(true))?
            .read_to_end(&mut raw)?;
        selected.push((order, raw));
    }
    let mut releases: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for session in &snapshot.sessions {
        if let Some(id) = &session.space_id {
            if all || selected_owners.contains(id) {
                releases.entry(id.clone()).or_default().push(session.id);
            }
        }
    }
    let backup = if selected.is_empty() && releases.is_empty() {
        None
    } else {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let root = dir.join("reset-backups");
        prismattyc_mux::private_fs::create_dir(&root).context("create reset backup directory")?;
        let backup = root.join(format!("{stamp}-{}", prismattyc_mux::new_space_id()?));
        prismattyc_mux::private_fs::create_dir(&backup)?;
        for (path, raw) in &selected {
            let mut file = prismattyc_mux::private_fs::open(
                &backup.join(path.file_name().unwrap()),
                std::fs::OpenOptions::new().write(true).create_new(true),
            )?;
            file.write_all(raw)?;
            file.sync_all()?;
        }
        let mut receipt = prismattyc_mux::private_fs::open(
            &backup.join(".reset-receipt.json"),
            std::fs::OpenOptions::new().write(true).create_new(true),
        )?;
        serde_json::to_writer_pretty(
            &mut receipt,
            &serde_json::json!({"spaces": names, "owners": releases}),
        )?;
        receipt.sync_all()?;
        prismattyc_mux::platform::sync_directory(&backup)?;
        prismattyc_mux::platform::sync_directory(&root)?;
        prismattyc_mux::platform::sync_directory(&dir)?;
        Some(backup)
    };
    for (owner, sessions) in &releases {
        transfer(&mut client, sessions.clone(), Some(owner.clone()), None).with_context(|| {
            format!("release ownership; original files remain backed up at {backup:?}")
        })?;
    }
    for (path, raw) in selected {
        anyhow::ensure!(
            std::fs::read(&path)? == raw,
            "Space changed during reset; backup retained at {backup:?}"
        );
        std::fs::remove_file(&path)?;
    }
    prismattyc_mux::platform::sync_directory(&dir)?;
    println!(
        "{}",
        serde_json::json!({"reset": names, "backup": backup, "released_sessions": releases.values().map(Vec::len).sum::<usize>()})
    );
    Ok(())
}
