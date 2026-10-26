// SPDX-License-Identifier: MIT
//! Long-running demonstrations complete from daemon replies or a constructed host view.
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use prismattyc_mux::{attach_tabs, platform};

pub(super) struct Job {
    pub command: &'static str,
    result: Receiver<Result<String, String>>,
}

impl Job {
    pub fn start(command: &str, socket: PathBuf) -> Result<Self> {
        let command = match command {
            "attach_all" => "attach_all",
            "mail_commit" => "mail_commit",
            _ => bail!("unknown demonstration"),
        };
        let (sender, result) = mpsc::channel();
        let executable = std::env::current_exe()?;
        std::thread::spawn(move || {
            if command == "attach_all" {
                match attach_all(&executable, &socket) {
                    Ok((mut child, view, count)) => {
                        let _ = sender.send(Ok(format!("Attached {count} seats in a new window. Return to this window to continue the walkthrough.")));
                        let _ = child.wait();
                        let _ = std::fs::remove_file(view);
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                    }
                }
            } else {
                let _ = sender.send(
                    crate::attach_log::walkthrough_mail_demo(&socket)
                        .map_err(|error| error.to_string()),
                );
            }
        });
        Ok(Self { command, result })
    }

    pub fn poll(&self) -> Option<Result<String, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "The demonstration stopped before completing. Try again.".into(),
            )),
        }
    }
}

fn attached(view: &attach_tabs::AttachTabsFile, sessions: &[(String, String)]) -> bool {
    sessions.iter().all(|(id, name)| {
        view.tabs.iter().any(|tab| tab.sessions.contains(id))
            && view.session_names.get(id) == Some(name)
    })
}

fn attach_all(executable: &Path, socket: &Path) -> Result<(std::process::Child, PathBuf, usize)> {
    let snapshot = crate::attach_log::live_snapshot_at(socket).context("daemon unavailable")?;
    let hide_default = snapshot
        .sessions
        .iter()
        .any(|session| session.name != "default");
    let sessions: Vec<_> = snapshot
        .sessions
        .into_iter()
        .filter(|session| !hide_default || session.name != "default")
        .map(|session| (session.id.to_string(), session.name))
        .collect();
    anyhow::ensure!(!sessions.is_empty(), "no sessions to attach");
    let view = socket.with_file_name(format!(
        "walkthrough-{}.attach-tabs.json",
        prismattyc_mux::new_space_id()?
    ));
    let mut command = platform::hidden_command(executable);
    command
        .arg("--no-splash")
        .env("PMUX_SOCKET", socket)
        .env("PMUX_VIEW_PATH", &view)
        .env_remove("PMUX_SPACE")
        .env_remove("PRISMATTYC_DUMP_PRESENT")
        .env_remove("PRISMATTYC_WALKTHROUGH_DUMP")
        .stdin(std::process::Stdio::null());
    for (id, name) in &sessions {
        command.args(["--attach-session", id, "--attach-title", name]);
    }
    let mut child = command
        .spawn()
        .context("could not open the attached window")?;
    let started = Instant::now();
    let ready = loop {
        if let Some(status) = child.try_wait()? {
            break Err(anyhow::anyhow!(
                "attached window exited before opening: {status}"
            ));
        }
        if attach_tabs::load(&view).is_some_and(|file| attached(&file, &sessions)) {
            break Ok(());
        }
        if started.elapsed() >= Duration::from_secs(20) {
            break Err(anyhow::anyhow!(
                "attached window did not publish its session layout within 20 seconds"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(view);
        return Err(error);
    }
    Ok((child, view, sessions.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_requires_every_live_identity_in_the_published_view() {
        let mut view: attach_tabs::AttachTabsFile = serde_json::from_value(serde_json::json!({
            "tabs": [{"title": "one", "sessions": ["7"]}],
            "session_names": {"7": "one"}
        }))
        .unwrap();
        let targets = vec![("7".into(), "one".into()), ("8".into(), "two".into())];
        assert!(!attached(&view, &targets));
        view.tabs[0].sessions.push("8".into());
        view.session_names.insert("8".into(), "stale-name".into());
        assert!(!attached(&view, &targets));
        view.session_names.insert("8".into(), "two".into());
        assert!(attached(&view, &targets));
    }
}
