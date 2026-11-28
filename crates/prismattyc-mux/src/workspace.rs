//! Durable daemon topology and the last host view. Runtime leases and PIDs
//! deliberately have no representation in this file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::attach_tabs::AttachTabsFile;
use crate::{ControlPlane, Domain, LayoutSnapshot, SpawnSpec, WindowBounds};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct NextIds {
    pub session: u64,
    pub window: u64,
    pub pane: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SavedPane {
    pub id: u64,
    pub title: String,
    pub title_pinned: bool,
    pub spawn: SpawnSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    /// What the pane's program reported it should be resumed as (OSC 7577).
    /// Preferred over `resume_command` when it may be resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_record: Option<crate::program_resume::ResumeRecord>,
    #[serde(skip)]
    observed_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SavedWindow {
    pub id: u64,
    pub title: String,
    pub bounds: WindowBounds,
    pub layout: LayoutSnapshot,
    pub panes: Vec<SavedPane>,
    pub sync_input: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SavedSession {
    pub id: u64,
    pub name: String,
    pub agent_id: Option<String>,
    pub space_id: Option<String>,
    pub windows: Vec<SavedWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    version: u32,
    pub(crate) next_ids: NextIds,
    pub(crate) sessions: Vec<SavedSession>,
    pub view: Option<AttachTabsFile>,
    /// Set only by [`Workspace::for_update`]: the user agreed to relaunch the
    /// saved commands, so the next restore runs them whatever
    /// `space_open_runs_commands` says. Cleared by [`Workspace::checkpoint`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    resume_commands: bool,
}

pub fn path(socket: &Path) -> PathBuf {
    crate::mailbox::default_mail_db_path()
        .with_file_name("workspaces")
        .join(socket_key(socket))
        .join("workspace.json")
}

/// Resolve the nearest existing ancestor, including before the runtime
/// directory exists after reboot. This keeps /tmp aliases from changing keys.
pub(crate) fn socket_key(socket: &Path) -> String {
    let mut existing = socket;
    let mut suffix = Vec::new();
    let stable = loop {
        if let Ok(mut resolved) = existing.canonicalize() {
            for part in suffix.iter().rev() {
                resolved.push(part);
            }
            break resolved;
        }
        let Some(name) = existing.file_name() else {
            break socket.to_owned();
        };
        suffix.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            break socket.to_owned();
        };
        existing = parent;
    };
    format!(
        "{:x}",
        Sha256::digest(stable.as_os_str().as_encoded_bytes())
    )
}

pub fn load(socket: &Path) -> Result<Option<Workspace>> {
    let path = path(socket);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read saved workspace"),
    };
    let saved: Workspace = serde_json::from_slice(&bytes).context("parse saved workspace")?;
    ensure!(
        saved.version == 1,
        "unsupported workspace version {}",
        saved.version
    );
    Ok(Some(saved))
}

impl Workspace {
    /// The daemon calls this while holding its instance lock, before binding.
    /// Use the same ownership lock as Space mutations and persist identities
    /// before a host can attach. Previewing a workspace stays read-only.
    pub fn migrate_saved_spaces(socket: &Path) -> Result<Option<Self>> {
        let directory = crate::layout_file::spaces_dir_for_socket(socket);
        if !directory.is_dir() {
            return Ok(None);
        }
        let lock = crate::platform::private_options()
            .create(true)
            .read(true)
            .write(true)
            .open(directory.join(".ownership.lock"))?;
        crate::platform::lock_exclusive(&lock)?;
        // Check the complete set before changing any files.
        Self::from_saved_spaces(socket)?;
        for entry in crate::list_spaces(&directory)? {
            let mut space = crate::load_space(&directory, &entry.name)?;
            if space.id.is_none() {
                space.id = Some(crate::new_space_id()?);
                space.version = crate::OWNED_SPACE_VERSION;
                crate::save_space(&directory, &entry.name, &space)?;
            }
        }
        Self::from_saved_spaces(socket)
    }

    /// First upgrade from Space files. Later starts use the exact checkpoint,
    /// including an intentionally empty workspace, instead of reopening archives.
    pub fn from_saved_spaces(socket: &Path) -> Result<Option<Self>> {
        use crate::attach_tabs::{AttachTabRecord, AttachTabsMode};
        use crate::{SavedNode, SpaceOpenRunsCommands};
        let directory = crate::layout_file::spaces_dir_for_socket(socket);
        let entries = crate::list_spaces(&directory)?;
        if entries.is_empty() {
            return Ok(None);
        }
        let policy = crate::load_mux_section(&crate::prism_config_path())?
            .space_open_runs_commands
            .unwrap_or_default();
        let mut saved = Self {
            version: 1,
            next_ids: NextIds {
                session: 1,
                window: 1,
                pane: 1,
            },
            sessions: Vec::new(),
            view: None,
            resume_commands: false,
        };
        let mut names = HashMap::new();
        let mut latest = None;
        fn node(
            source: &SavedNode,
            next: &mut u64,
            panes: &mut Vec<SavedPane>,
            run: bool,
        ) -> LayoutSnapshot {
            match source {
                SavedNode::Leaf {
                    cwd,
                    command,
                    title,
                    ..
                } => {
                    let id = *next;
                    *next += 1;
                    let mut command_line = crate::platform::default_shell_command();
                    let program = command_line.remove(0);
                    panes.push(SavedPane {
                        id,
                        title: title.clone().unwrap_or_default(),
                        title_pinned: title.is_some(),
                        spawn: SpawnSpec {
                            program,
                            argv: command_line,
                            cwd: cwd.clone(),
                            env: Default::default(),
                        },
                        resume_command: command
                            .as_ref()
                            .filter(|c| run && !c.trim().is_empty())
                            .cloned(),
                        resume_record: None,
                        observed_pid: None,
                    });
                    LayoutSnapshot::Leaf { pane_id: id }
                }
                SavedNode::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => LayoutSnapshot::Split {
                    axis: *axis,
                    ratio: crate::clamp_ratio(*ratio),
                    first: Box::new(node(first, next, panes, run)),
                    second: Box::new(node(second, next, panes, run)),
                },
            }
        }
        for entry in entries {
            let space = crate::load_space(&directory, &entry.name)?;
            for session in &space.sessions {
                ensure!(
                    !names.contains_key(&session.name),
                    "session appears in multiple saved Spaces: {}",
                    session.name
                );
                let id = saved.next_ids.session;
                saved.next_ids.session += 1;
                names.insert(session.name.clone(), id.to_string());
                let run = match policy {
                    SpaceOpenRunsCommands::All => true,
                    SpaceOpenRunsCommands::Agents => session.agent.is_some(),
                    SpaceOpenRunsCommands::None => false,
                };
                let mut windows = Vec::new();
                for window in &session.windows {
                    let id = saved.next_ids.window;
                    saved.next_ids.window += 1;
                    let mut panes = Vec::new();
                    let layout = node(&window.root, &mut saved.next_ids.pane, &mut panes, run);
                    windows.push(SavedWindow {
                        id,
                        title: window.title.clone(),
                        layout,
                        panes,
                        sync_input: false,
                        bounds: WindowBounds {
                            window_id: id,
                            cols: window.cols.into(),
                            rows: window.rows.into(),
                        },
                    });
                }
                saved.sessions.push(SavedSession {
                    id,
                    name: session.name.clone(),
                    agent_id: session.agent.clone(),
                    space_id: space.id.clone(),
                    windows,
                });
            }
            if latest
                .as_ref()
                .is_none_or(|(_, prior): &(String, crate::SavedSpace)| {
                    prior.saved_at_unix <= space.saved_at_unix
                })
            {
                latest = Some((entry.name, space));
            }
        }
        if let Some((name, space)) = latest {
            let tabs = if space.tabs.is_empty() {
                space
                    .sessions
                    .iter()
                    .map(|s| AttachTabRecord {
                        title: s.name.clone(),
                        fixed_name: None,
                        sessions: vec![names[&s.name].clone()],
                        layout: None,
                    })
                    .collect()
            } else {
                space
                    .tabs
                    .iter()
                    .map(|tab| AttachTabRecord {
                        title: tab.title.clone(),
                        fixed_name: tab.fixed_name.clone(),
                        sessions: tab
                            .sessions
                            .iter()
                            .filter_map(|s| names.get(s).cloned())
                            .collect(),
                        layout: tab.layout.as_ref().and_then(|l| {
                            crate::attach_tabs::remap_layout(l, |s| names.get(s).cloned())
                        }),
                    })
                    .collect()
            };
            saved.view = Some(AttachTabsFile {
                tabs,
                active_tab: space.active_tab,
                focused_session: space
                    .focused_session
                    .as_ref()
                    .and_then(|s| names.get(s).cloned()),
                space: Some(name),
                space_id: space.id,
                mode: AttachTabsMode::Switch,
                session_names: names.into_iter().map(|(name, id)| (id, name)).collect(),
            });
        }
        Ok(Some(saved))
    }

    pub(crate) fn capture(plane: &ControlPlane, next_ids: NextIds) -> Result<Self> {
        Self::from_snapshot(plane.snapshot()?, next_ids)
    }

    /// The workspace a consented update restores: the running daemon's
    /// sessions, windows, splits, pane IDs, Space ownership and environment,
    /// read from its snapshot before it stops. Each pane restarts in the
    /// default shell in its current directory and then runs what it was
    /// running: an interactive shell's foreground command, or the pane's own
    /// launch command (agents resume without their startup prompt).
    pub fn for_update(snapshot: crate::Snapshot, view: Option<AttachTabsFile>) -> Result<Self> {
        let next_ids = NextIds {
            session: snapshot.sessions.iter().map(|s| s.id).max().unwrap_or(0) + 1,
            window: snapshot
                .sessions
                .iter()
                .flat_map(|s| &s.windows)
                .map(|w| w.id)
                .max()
                .unwrap_or(0)
                + 1,
            pane: snapshot
                .sessions
                .iter()
                .flat_map(|s| &s.windows)
                .flat_map(|w| &w.panes)
                .map(|p| p.id)
                .max()
                .unwrap_or(0)
                + 1,
        };
        let mut saved = Self::from_snapshot(snapshot, next_ids)?;
        for pane in saved
            .sessions
            .iter_mut()
            .flat_map(|s| &mut s.windows)
            .flat_map(|w| &mut w.panes)
        {
            let pid = pane.observed_pid;
            if let Some(cwd) = pid
                .and_then(crate::procinfo::cwd_of)
                .filter(|p| p.is_absolute())
            {
                pane.spawn.cwd = Some(cwd);
            }
            pane.resume_command = if crate::procinfo::interactive_shell(&pane.spawn) {
                pid.and_then(crate::procinfo::live_foreground_command)
            } else {
                let command = crate::procinfo::replay_spawn_command(&pane.spawn, pid);
                let mut shell = crate::platform::default_shell_command();
                pane.spawn.program = shell.remove(0);
                pane.spawn.argv = shell;
                command
            }
            .filter(|command| !command.trim().is_empty());
            if let Ok(path) = std::env::var("PATH") {
                pane.spawn.env.entry("PATH".into()).or_insert(path);
            }
        }
        saved.view = view;
        saved.resume_commands = true;
        Ok(saved)
    }

    fn from_snapshot(snapshot: crate::Snapshot, next_ids: NextIds) -> Result<Self> {
        let mut sessions = Vec::new();
        for session in snapshot.sessions {
            let mut windows = Vec::new();
            for window in session.windows {
                let panes = window
                    .panes
                    .into_iter()
                    .map(|pane| {
                        Ok(SavedPane {
                            id: pane.id,
                            title: pane.title,
                            title_pinned: pane.title_pinned,
                            spawn: pane
                                .spawn
                                .context("workspace pane has no spawn specification")?,
                            resume_command: None,
                            resume_record: pane.resume,
                            observed_pid: pane.child_pid,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                windows.push(SavedWindow {
                    id: window.id,
                    title: window.title,
                    bounds: window.bounds,
                    layout: window.layout,
                    panes,
                    sync_input: window.sync_input,
                });
            }
            sessions.push(SavedSession {
                id: session.id,
                name: session.name,
                agent_id: session.agent_id,
                space_id: session.space_id,
                windows,
            });
        }
        Ok(Self {
            version: 1,
            next_ids,
            sessions,
            view: None,
            resume_commands: false,
        })
    }

    pub fn restore(&self, socket: &Path) -> Result<ControlPlane> {
        let (mut plane, commands) = self.restore_plane(socket)?;
        for (pane_id, command) in commands {
            plane.resume_workspace_command(pane_id, &command)?;
        }
        Ok(plane)
    }

    pub fn restore_plane(&self, socket: &Path) -> Result<(ControlPlane, Vec<(u64, String)>)> {
        let domain = Domain::from_workspace(self)?;
        let bounds = self
            .sessions
            .iter()
            .flat_map(|s| s.windows.iter())
            .map(|w| w.bounds);
        let policy = crate::load_mux_section(&crate::prism_config_path())?
            .space_open_runs_commands
            .unwrap_or_default();
        let panes: Vec<_> = self
            .sessions
            .iter()
            .flat_map(|s| {
                s.windows
                    .iter()
                    .flat_map(move |w| w.panes.iter().map(move |p| (s, p)))
            })
            .collect();
        let mut plane = ControlPlane::new_live(
            domain,
            bounds,
            None,
            panes.iter().map(|(_, p)| (p.id, p.spawn.clone())),
            Some(socket.to_owned()),
        )?;
        // Replay into the original interactive shell. Its launch spec stays
        // unchanged across checkpoints, and the shell remains after the job
        // exits. Replacing argv with -c would permanently bake in old jobs.
        let commands = self.resume_commands(&mut plane, policy, |_| true);
        Ok((plane, commands))
    }

    /// The resume command for each pane `wanted` picks, by the pane's OSC 7577
    /// record or its saved command, as the restore policy allows.
    pub(crate) fn resume_commands(
        &self,
        plane: &mut ControlPlane,
        policy: crate::SpaceOpenRunsCommands,
        wanted: impl Fn(u64) -> bool,
    ) -> Vec<(u64, String)> {
        let panes = self.sessions.iter().flat_map(|s| {
            s.windows
                .iter()
                .flat_map(move |w| w.panes.iter().map(move |p| (s, p)))
        });
        let mut commands = Vec::new();
        let resume_policy = crate::program_resume::policy();
        for (session, pane) in panes.filter(|(_, pane)| wanted(pane.id)) {
            let resume = self.resume_commands
                || policy == crate::SpaceOpenRunsCommands::All
                || (policy == crate::SpaceOpenRunsCommands::Agents && session.agent_id.is_some());
            if let Some(record) = pane.resume_record.clone() {
                // The pane's shell resumes with the PATH it is given here.
                let path = pane
                    .spawn
                    .env
                    .get("PATH")
                    .map(std::ffi::OsString::from)
                    .or_else(|| std::env::var_os("PATH"));
                let decision = crate::program_resume::restore_decision(
                    &record,
                    resume_policy,
                    resume_policy.enabled(),
                    crate::program_resume::server_uid(),
                    |program| program.canonicalize().ok(),
                    |name| crate::program_resume::resolve_on_path(name, path.as_deref()),
                    Path::is_dir,
                );
                if let (true, Err(reason)) = (resume, &decision) {
                    eprintln!(
                        "pmuxd: pane {}: not resuming {}: {reason}",
                        pane.id,
                        record.display()
                    );
                }
                let verified = decision.as_ref().ok().filter(|_| resume).cloned();
                let reason = decision.err().filter(|_| resume);
                if let Some(command) = plane.restore_resume(pane.id, record, verified, reason) {
                    commands.push((pane.id, command));
                    continue;
                }
            }
            if let Some(command) = pane.resume_command.as_ref().filter(|_| resume) {
                commands.push((pane.id, command.clone()));
            }
        }
        commands
    }

    pub fn resume_panes(
        plane: &std::sync::Arc<std::sync::Mutex<ControlPlane>>,
        commands: &[(u64, String)],
    ) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }
        let mut guard = plane
            .lock()
            .map_err(|_| anyhow::anyhow!("control plane mutex poisoned"))?;
        for (pane_id, command) in commands {
            guard.resume_workspace_command(*pane_id, command)?;
        }
        Ok(())
    }

    fn refresh_process_metadata(&mut self) -> Result<()> {
        use crate::SpaceOpenRunsCommands;
        let policy = crate::load_mux_section(&crate::prism_config_path())?
            .space_open_runs_commands
            .unwrap_or_default();
        for session in &mut self.sessions {
            let resume = self.resume_commands
                || policy == SpaceOpenRunsCommands::All
                || (policy == SpaceOpenRunsCommands::Agents && session.agent_id.is_some());
            for pane in session.windows.iter_mut().flat_map(|w| &mut w.panes) {
                let Some(pid) = pane.observed_pid else {
                    continue;
                };
                if let Some(cwd) = crate::procinfo::cwd_of(pid).filter(|p| p.is_absolute()) {
                    pane.spawn.cwd = Some(cwd);
                }
                if resume {
                    if crate::procinfo::interactive_shell(&pane.spawn) {
                        pane.resume_command = crate::procinfo::live_foreground_command(pid);
                    } else if crate::procinfo::spawn_needs_resume_rewrite(&pane.spawn, pid) {
                        pane.resume_command =
                            crate::procinfo::replay_spawn_command(&pane.spawn, Some(pid));
                        let mut shell = crate::platform::default_shell_command();
                        if !shell.is_empty() {
                            pane.spawn.program = shell.remove(0);
                            pane.spawn.argv = shell;
                        }
                    }
                }
                if let Ok(path) = std::env::var("PATH") {
                    pane.spawn.env.entry("PATH".into()).or_insert(path);
                }
            }
        }
        Ok(())
    }

    /// Write the restored workspace back. A consent to relaunch commands
    /// applies to one restore only, so the written copy drops it.
    pub fn checkpoint(&self, socket: &Path) -> Result<()> {
        if self.resume_commands {
            let mut once = self.clone();
            once.resume_commands = false;
            return once.save(socket);
        }
        self.save(socket)
    }

    /// Replace the saved workspace with this one, for the next daemon start.
    pub fn write(&self, socket: &Path) -> Result<()> {
        self.save(socket)
    }

    /// Session names, in order.
    pub fn session_names(&self) -> Vec<String> {
        self.sessions.iter().map(|s| s.name.clone()).collect()
    }

    fn save(&self, socket: &Path) -> Result<()> {
        write_private(
            &path(socket),
            serde_json::to_string_pretty(self)?.as_bytes(),
        )
    }
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("workspace path needs a parent")?;
    std::fs::create_dir_all(parent)?;
    crate::platform::set_mode(parent, 0o700)?;
    #[cfg(windows)]
    crate::platform::require_private_directory(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = crate::platform::private_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        crate::platform::replace_file(&temporary, path)?;
        crate::platform::sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The worker copies metadata under the plane lock, then serializes and writes
/// outside it. No filesystem work is added to the render or PTY drain paths.
pub struct Checkpointer {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Checkpointer {
    pub fn start(plane: Arc<Mutex<ControlPlane>>, socket: PathBuf) -> Result<Self> {
        let (stop, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pmux-workspace".into())
            .spawn(move || {
                let mut previous = String::new();
                loop {
                    let done = !matches!(
                        receiver.recv_timeout(Duration::from_secs(1)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    );
                    let result = (|| -> Result<()> {
                        if !crate::login::enabled(&socket)? {
                            return Ok(());
                        }
                        let mut saved = plane
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .saved_workspace()?;
                        saved.refresh_process_metadata()?;
                        saved.view = crate::attach_tabs::load(
                            &crate::attach_tabs::layout_path_from_socket(&socket),
                        );
                        let bytes = serde_json::to_string(&saved)?;
                        if previous != bytes {
                            saved.save(&socket)?;
                            previous = bytes;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        eprintln!("pmuxd: workspace checkpoint failed: {error:#}");
                    }
                    if done {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_unbound_record_falls_back_to_the_inspected_command_and_says_why() {
        let uid = crate::program_resume::server_uid().unwrap();
        let saved: Workspace = serde_json::from_value(serde_json::json!({
            "version": 1,
            "next_ids": {"session": 2, "window": 2, "pane": 2},
            "sessions": [{
                "id": 1, "name": "resume", "agent_id": null, "space_id": null,
                "windows": [{
                    "id": 1, "title": "w",
                    "bounds": {"window_id": 1, "cols": 80, "rows": 24},
                    "layout": {"kind": "leaf", "pane_id": 1},
                    "sync_input": false,
                    "panes": [{
                        "id": 1, "title": "", "title_pinned": false,
                        "spawn": {"program": "/bin/sh", "argv": ["-c", "exec sleep 30"], "cwd": null, "env": {}},
                        "resume_command": "echo fallback",
                        "resume_record": {
                            "argv": ["nvim", "-c", "!touch /tmp/owned"],
                            "reported_at": 1,
                            "uid": uid,
                            "owner_exe": "/bin/sh"
                        }
                    }]
                }]
            }],
            "view": null,
            "resume_commands": true
        }))
        .unwrap();
        crate::program_resume::TEST_PMUX
            .with(|path| *path.borrow_mut() = Some(PathBuf::from("/opt/pmux")));
        // A real `nvim` on the resume PATH, so only the binding can refuse it.
        let bin = std::env::temp_dir().join(format!("resume-ws-bin-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("nvim"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            bin.join("nvim"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mut saved = saved;
        saved.sessions[0].windows[0].panes[0]
            .spawn
            .env
            .insert("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()));
        let socket = std::env::temp_dir().join(format!("rs-{}.sock", std::process::id()));
        let (plane, commands) = saved.restore_plane(&socket).unwrap();
        assert_eq!(commands, [(1, "echo fallback".to_string())]);
        let snapshot = plane.snapshot().unwrap();
        let record = snapshot.sessions[0].windows[0].panes[0]
            .resume
            .clone()
            .expect("the record stays visible");
        assert_eq!(
            record.not_resumed.as_deref(),
            Some(crate::program_resume::UNBOUND)
        );
    }

    #[test]
    fn socket_key_survives_runtime_directory_loss_and_symlink_aliases() {
        let root = std::env::temp_dir().join(format!("pmux-workspace-key-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(root.canonicalize().unwrap(), &alias).unwrap();
        let missing = socket_key(&alias.join("run/pmux.sock"));
        std::fs::create_dir_all(root.join("run")).unwrap();
        let bound = crate::local_socket::UnixListener::bind(root.join("run/pmux.sock")).unwrap();
        let live = socket_key(&root.join("run/pmux.sock"));
        drop(bound);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(missing, live);
    }
}
