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
        let snapshot = plane.snapshot()?;
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
        })
    }

    pub fn restore(&self, socket: &Path) -> Result<ControlPlane> {
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
        for (session, pane) in panes {
            let resume = policy == crate::SpaceOpenRunsCommands::All
                || (policy == crate::SpaceOpenRunsCommands::Agents && session.agent_id.is_some());
            if let Some(command) = pane.resume_command.as_ref().filter(|_| resume) {
                plane.resume_workspace_command(pane.id, command)?;
            }
        }
        Ok(plane)
    }

    fn refresh_process_metadata(&mut self) -> Result<()> {
        use crate::SpaceOpenRunsCommands;
        let policy = crate::load_mux_section(&crate::prism_config_path())?
            .space_open_runs_commands
            .unwrap_or_default();
        for session in &mut self.sessions {
            let resume = policy == SpaceOpenRunsCommands::All
                || (policy == SpaceOpenRunsCommands::Agents && session.agent_id.is_some());
            for pane in session.windows.iter_mut().flat_map(|w| &mut w.panes) {
                let Some(pid) = pane.observed_pid else {
                    continue;
                };
                if let Some(cwd) = crate::procinfo::cwd_of(pid).filter(|p| p.is_absolute()) {
                    pane.spawn.cwd = Some(cwd);
                }
                // Explicit launch commands already have a complete argv.
                // Only an interactive shell needs its foreground job resumed.
                let shell = Path::new(&pane.spawn.program)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                let interactive_posix =
                    matches!(shell, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh")
                        && pane
                            .spawn
                            .argv
                            .iter()
                            .all(|a| matches!(a.as_str(), "-l" | "--login" | "-i"));
                // The Windows replay formatter uses the configured command
                // shell, so do not feed its syntax into a different shell.
                let interactive_windows = cfg!(windows)
                    && pane
                        .spawn
                        .program
                        .eq_ignore_ascii_case(&crate::platform::default_shell())
                    && pane.spawn.argv.is_empty();
                if resume && (interactive_posix || interactive_windows) {
                    pane.resume_command = crate::procinfo::live_foreground_command(pid);
                }
                if let Ok(path) = std::env::var("PATH") {
                    pane.spawn.env.entry("PATH".into()).or_insert(path);
                }
            }
        }
        Ok(())
    }

    pub fn checkpoint(&self, socket: &Path) -> Result<()> {
        self.save(socket)
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
