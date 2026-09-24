//! Reusable definitions. Preview has no daemon or process side effects.

use crate::space_team::TeamMetadata;
use crate::{SavedNode, SavedSpace};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "space_template_tests.rs"]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TeamTemplate {
    pub version: u32,
    pub definition: SavedSpace,
    pub metadata: TeamMetadata,
}

pub fn directory(spaces: &Path) -> PathBuf {
    spaces.join("team-templates")
}

pub fn save(spaces: &Path, name: &str, template: &TeamTemplate) -> Result<PathBuf> {
    let path = crate::layout_path(&directory(spaces), name)?;
    std::fs::create_dir_all(directory(spaces))?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory(spaces).join(".lock"))?;
    crate::platform::lock_exclusive(&lock)?;
    if path.exists() {
        bail!("template {name:?} already exists; choose another name");
    }
    crate::space_team::write_json(&path, template)?;
    Ok(path)
}

pub fn load(spaces: &Path, name: &str) -> Result<TeamTemplate> {
    let value: TeamTemplate = serde_json::from_slice(&std::fs::read(crate::layout_path(
        &directory(spaces),
        name,
    )?)?)?;
    if value.version != 1 {
        bail!("unsupported team template version {}", value.version);
    }
    if value.definition.sessions.len() > 64 {
        bail!("a template may contain at most 64 sessions");
    }
    Ok(value)
}

pub fn list(spaces: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(directory(spaces)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut names = vec![];
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.into());
            }
        }
    }
    names.sort();
    Ok(names)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemplatePreview {
    pub destination: String,
    pub definition: SavedSpace,
    pub metadata: TeamMetadata,
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub notices: Vec<String>,
    pub launches: Vec<LaunchPreview>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchPreview {
    pub session: String,
    pub cwd: Option<PathBuf>,
    pub program: Option<String>,
    pub command: Option<String>,
}

pub fn preview(
    template: &TeamTemplate,
    destination: &str,
    occupied: &BTreeSet<String>,
) -> Result<TemplatePreview> {
    crate::validate_layout_name(destination)?;
    let mut definition = template.definition.clone();
    definition.id = None; // Preview must not allocate a live identity.
    definition.version = crate::OWNED_SPACE_VERSION;
    let prefix: String = destination
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(50)
        .collect();
    let prefix = prefix.trim_matches('-');
    let prefix = if prefix.is_empty() { "team" } else { prefix };
    let mut names = BTreeMap::new();
    let mut conflicts = vec![];
    for (i, session) in definition.sessions.iter_mut().enumerate() {
        let new = format!("{prefix}-{}", i + 1);
        crate::mailbox::AgentId::new(new.clone()).context("invalid generated session name")?;
        if names.insert(session.name.clone(), new.clone()).is_some() {
            bail!("template repeats a session name");
        }
        if occupied.contains(&new) {
            conflicts.push(format!("session or mailbox {new} already exists"));
        }
        session.name.clone_from(&new);
        session.agent = Some(new);
    }
    for tab in &mut definition.tabs {
        for session in &mut tab.sessions {
            *session = names
                .get(session)
                .context("template tab refers to a missing session")?
                .clone();
        }
    }
    if let Some(focused) = &mut definition.focused_session {
        *focused = names
            .get(focused)
            .context("template focus refers to a missing session")?
            .clone();
    }
    let mut metadata = template.metadata.clone();
    metadata.roles = metadata
        .roles
        .into_iter()
        .filter_map(|(old, role)| names.get(&old).cloned().map(|n| (n, role)))
        .collect();
    let mut launches = vec![];
    for session in &definition.sessions {
        for window in &session.windows {
            collect(&window.root, &session.name, &mut launches);
        }
    }
    for launch in &launches {
        if let Some(cwd) = &launch.cwd {
            if !cwd.is_absolute() || !cwd.is_dir() {
                conflicts.push(format!(
                    "{}: directory {} is unavailable",
                    launch.session,
                    cwd.display()
                ));
            }
        }
        for value in [&launch.program, &launch.command].into_iter().flatten() {
            if value.contains(['\n', '\r', '\0']) {
                conflicts.push(format!(
                    "{}: launch recipe contains a line break or NUL",
                    launch.session
                ));
            }
        }
    }
    Ok(TemplatePreview {
        destination: destination.into(),
        definition,
        metadata,
        conflicts,
        notices: Vec::new(),
        launches,
    })
}

fn collect(node: &SavedNode, session: &str, out: &mut Vec<LaunchPreview>) {
    match node {
        SavedNode::Leaf {
            cwd,
            program,
            command,
            ..
        } => out.push(LaunchPreview {
            session: session.into(),
            cwd: cwd.clone(),
            program: program.clone(),
            command: command.clone(),
        }),
        SavedNode::Split { first, second, .. } => {
            collect(first, session, out);
            collect(second, session, out);
        }
    }
}

pub fn commands(node: &SavedNode) -> Vec<Option<String>> {
    match node {
        SavedNode::Leaf {
            program, command, ..
        } => vec![command.clone().or_else(|| {
            program
                .as_ref()
                .map(|program| format!("exec '{}'", program.replace('\'', "'\\''")))
        })],
        SavedNode::Split { first, second, .. } => {
            let mut all = commands(first);
            all.extend(commands(second));
            all
        }
    }
}

/// Create ordinary shells until the operator explicitly chooses launch.
pub fn shells_only(space: &SavedSpace) -> SavedSpace {
    fn clear(node: &mut SavedNode) {
        match node {
            SavedNode::Leaf {
                program, command, ..
            } => {
                *program = None;
                *command = None;
            }
            SavedNode::Split { first, second, .. } => {
                clear(first);
                clear(second);
            }
        }
    }
    let mut value = space.clone();
    for session in &mut value.sessions {
        for window in &mut session.windows {
            clear(&mut window.root);
        }
    }
    value
}

impl TemplatePreview {
    pub fn text(&self) -> String {
        let mut rows = vec![format!(
            "Create {} with {} independent sessions",
            self.destination,
            self.definition.sessions.len()
        )];
        for launch in &self.launches {
            rows.push(format!(
                "{} | directory: {} | program: {} | command: {}",
                launch.session,
                launch
                    .cwd
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "default".into()),
                launch.program.as_deref().unwrap_or("default shell"),
                launch.command.as_deref().unwrap_or("none")
            ));
        }
        for notice in &self.notices {
            rows.push(format!("Note: {notice}"));
        }
        for conflict in &self.conflicts {
            rows.push(format!("Conflict: {conflict}"));
        }
        rows.push("Preview only. No sessions created or commands executed.".into());
        rows.join("\n")
    }
}
