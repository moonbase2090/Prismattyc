//! Shared Space details for the desktop, CLI, and MCP.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::team_attention::{AttentionAction, AttentionRequest};
use crate::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, SavedSpace,
    Snapshot, PROTOCOL_VERSION,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamMetadata {
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
    #[serde(default)]
    pub links: BTreeMap<String, String>,
}

fn metadata_path(dir: &Path, id: &str) -> Result<PathBuf> {
    if !crate::valid_space_id(id) {
        bail!("Space has no valid stable identity");
    }
    Ok(dir.join("team-details").join(format!("{id}.json")))
}

pub fn metadata(dir: &Path, space: &SavedSpace) -> Result<TeamMetadata> {
    let Some(id) = &space.id else {
        return Ok(TeamMetadata::default());
    };
    let path = metadata_path(dir, id)?;
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("read team details")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TeamMetadata::default()),
        Err(error) => Err(error.into()),
    }
}

/// Serialize metadata changes. Readers only see a complete document.
pub fn edit_metadata(
    dir: &Path,
    space: &SavedSpace,
    edit: impl FnOnce(&mut TeamMetadata) -> Result<()>,
) -> Result<()> {
    let path = metadata_path(dir, space.id.as_deref().context("Space has no identity")?)?;
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    let mut value = metadata(dir, space)?;
    edit(&mut value)?;
    write_json(&path, &value)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing data directory")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", crate::new_space_id()?));
    let outcome = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(tmp);
    }
    outcome
}

pub fn transfer_roles(dir: &Path, from: &str, to: &str, names: &[String]) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let source_path = metadata_path(dir, from)?;
    let target_path = metadata_path(dir, to)?;
    let parent = source_path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join(".lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    let read = |path: &Path| -> Result<TeamMetadata> {
        match fs::read(path) {
            Ok(raw) => Ok(serde_json::from_slice(&raw)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TeamMetadata::default()),
            Err(e) => Err(e.into()),
        }
    };
    let mut source = read(&source_path)?;
    let mut target = read(&target_path)?;
    for name in names {
        if let Some(role) = source.roles.remove(name) {
            target.roles.insert(name.clone(), role);
        }
    }
    write_json(&target_path, &target)?;
    write_json(&source_path, &source)
}

pub fn validate_label(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > 120 || value.chars().any(char::is_control)
    {
        bail!("use 1 to 120 printable characters");
    }
    Ok(())
}

pub fn validate_link(value: &str) -> Result<()> {
    if value.len() > 4096
        || value.chars().any(char::is_control)
        || !(value.starts_with("https://")
            || value.starts_with("http://")
            || Path::new(value).is_absolute())
    {
        bail!("use an HTTP(S) link or an absolute local path");
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionDetails {
    pub name: String,
    pub role: Option<String>,
    pub session_id: Option<u64>,
    pub panes: Vec<u64>,
    pub state: String,
    pub letters: u32,
    pub attention: Vec<AttentionRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamDetails {
    pub name: String,
    pub space_id: Option<String>,
    pub observed_at_ms: u64,
    pub source: String,
    pub sessions: Vec<SessionDetails>,
    pub sessions_needing_input: usize,
    pub letters: u32,
    pub links: BTreeMap<String, String>,
}

pub fn describe(
    name: &str,
    space: &SavedSpace,
    metadata: TeamMetadata,
    snapshot: Option<&Snapshot>,
    requests: &[AttentionRequest],
    now_ms: u64,
) -> TeamDetails {
    let sessions: Vec<_> = crate::space_sessions_in_tab_order(space)
        .into_iter()
        .map(|name| {
            let live = snapshot.and_then(|s| s.sessions.iter().find(|s| s.name == name));
            let owned = live.filter(|s| space.id.is_some() && s.space_id == space.id);
            let panes: Vec<_> = owned
                .into_iter()
                .flat_map(|s| &s.windows)
                .flat_map(|w| &w.panes)
                .collect();
            let running = panes.iter().any(|p| p.child_pid.is_some());
            let state = if snapshot.is_none() {
                "unknown: daemon unavailable"
            } else if live.is_some() && owned.is_none() {
                "conflict: another Space owns this name"
            } else if running {
                "running"
            } else {
                "stopped"
            };
            let attention = requests
                .iter()
                .filter(|r| panes.iter().any(|p| p.id == r.pane_id))
                .cloned()
                .collect();
            // Mail depth is session-addressed. Do not sum it across multiple panes.
            let letters = panes
                .iter()
                .filter_map(|p| p.mail.as_ref().map(|m| m.depth))
                .max()
                .unwrap_or(0);
            SessionDetails {
                role: metadata.roles.get(&name).cloned(),
                name,
                session_id: owned.map(|s| s.id),
                panes: panes.iter().map(|p| p.id).collect(),
                state: state.into(),
                letters,
                attention,
            }
        })
        .collect();
    TeamDetails {
        name: name.into(),
        space_id: space.id.clone(),
        observed_at_ms: now_ms,
        source: if snapshot.is_some() {
            "live daemon snapshot"
        } else {
            "saved definition; live state unknown"
        }
        .into(),
        sessions_needing_input: sessions
            .iter()
            .filter(|s| s.state == "running" && s.attention.iter().any(|r| r.needs_input(now_ms)))
            .count(),
        letters: sessions.iter().map(|s| s.letters).sum(),
        sessions,
        links: metadata.links,
    }
}

impl TeamDetails {
    pub fn text(&self) -> String {
        let mut lines = vec![
            format!(
                "{} — {} sessions need you; {} letters",
                self.name, self.sessions_needing_input, self.letters
            ),
            self.source.clone(),
        ];
        for session in &self.sessions {
            lines.push(format!(
                "{} | {} | {} | {} letters",
                session.name,
                session.role.as_deref().unwrap_or("no role"),
                session.state,
                session.letters
            ));
            for request in &session.attention {
                let state = if session.state != "running" {
                    "stale"
                } else if request.needs_input(self.observed_at_ms) {
                    "needs input"
                } else {
                    "snoozed"
                };
                lines.push(format!(
                    "  {state}: {} ({})",
                    request.message, request.source
                ));
            }
        }
        for (label, target) in &self.links {
            lines.push(format!("{label}: {target}"));
        }
        lines.join("\n")
    }
}

pub struct TeamClient {
    stream: BufReader<UnixStream>,
    next_id: u64,
    pub client_id: u64,
}

impl TeamClient {
    pub fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut client = Self {
            stream: BufReader::new(stream),
            next_id: 0,
            client_id: 0,
        };
        let data = client.call(|request_id, _| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })?;
        let ControlResponseData::ClientRegistered { client_id } = data else {
            bail!("unexpected registration response");
        };
        client.client_id = client_id;
        Ok(client)
    }

    pub fn call(
        &mut self,
        make: impl FnOnce(u64, u64) -> ControlRequest,
    ) -> Result<ControlResponseData> {
        self.next_id += 1;
        let request = make(self.next_id, self.client_id);
        serde_json::to_writer(self.stream.get_mut(), &request)?;
        self.stream.get_mut().write_all(b"\n")?;
        let mut bytes = Vec::new();
        self.stream
            .by_ref()
            .take(16 * 1024 * 1024)
            .read_until(b'\n', &mut bytes)?;
        if bytes.last() != Some(&b'\n') {
            bail!("incomplete or oversized daemon response");
        }
        let reply: ControlResponse = serde_json::from_slice(&bytes)?;
        if reply.request_id != self.next_id {
            bail!("daemon response does not match request");
        }
        match reply.body {
            ControlResponseBody::Ok { response } => Ok(response),
            ControlResponseBody::Error { error } => Err(error.into()),
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        match self.call(|request_id, _| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        })? {
            ControlResponseData::Snapshot { snapshot } => Ok(snapshot),
            _ => bail!("unexpected snapshot response"),
        }
    }

    pub fn requests(&mut self) -> Result<Vec<AttentionRequest>> {
        match self.call(|request_id, client_id| ControlRequest::AttentionRequests {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
        })? {
            ControlResponseData::AttentionRequests { requests } => Ok(requests),
            _ => bail!("unexpected attention response"),
        }
    }

    pub fn update_attention(
        &mut self,
        pane_id: u64,
        revision: u64,
        expected_space: String,
        expected_session: u64,
        action: AttentionAction,
    ) -> Result<()> {
        self.call(|request_id, client_id| ControlRequest::UpdateAttention {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            revision,
            expected_space,
            expected_session,
            action,
        })?;
        Ok(())
    }
}

pub fn inspect(socket: &Path, dir: &Path, name: &str) -> Result<TeamDetails> {
    let space = crate::load_space(dir, name)?;
    let meta = metadata(dir, &space)?;
    let mut client = TeamClient::connect(socket).ok();
    let snapshot = client.as_mut().and_then(|c| c.snapshot().ok());
    let requests = client.as_mut().and_then(|c| c.requests().ok());
    let mut report = describe(
        name,
        &space,
        meta,
        snapshot.as_ref(),
        requests.as_deref().unwrap_or_default(),
        crate::host_render_status::unix_ms(),
    );
    let cache = space
        .id
        .as_ref()
        .filter(|id| crate::valid_space_id(id))
        .map(|id| dir.join("team-observations").join(format!("{id}.json")));
    if let Some(cache) = cache {
        if snapshot.is_some() && requests.is_some() {
            // A read remains useful when its optional offline cache is unwritable.
            let _ = write_json(&cache, &report);
        } else {
            if let Ok(raw) = fs::read(cache) {
                if let Ok(old) = serde_json::from_slice::<TeamDetails>(&raw) {
                    for session in &mut report.sessions {
                        if let Some(previous) = old.sessions.iter().find(|s| s.name == session.name)
                        {
                            session.attention = previous.attention.clone();
                        }
                        if requests.is_none() && snapshot.is_some() {
                            session
                                .state
                                .push_str("; attention unavailable (saved reasons are stale)");
                        }
                    }
                }
            }
            report.source =
                "Live attention unavailable; retained reasons are stale. Refresh before acting."
                    .into();
        }
    }
    Ok(report)
}

pub fn record_result(
    dir: &Path,
    space: &SavedSpace,
    view: &Path,
    mut result: serde_json::Value,
) -> Result<()> {
    use std::hash::{Hash, Hasher};
    let id = space.id.as_deref().context("Space has no identity")?;
    if !crate::valid_space_id(id) {
        bail!("invalid Space identity");
    }
    let mut key = std::collections::hash_map::DefaultHasher::new();
    view.hash(&mut key);
    let parent = dir.join("team-results").join(id);
    fs::create_dir_all(&parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join(".lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    let path = parent.join(format!("{:016x}.json", key.finish()));
    if let Ok(raw) = fs::read(&path) {
        let old: serde_json::Value = serde_json::from_slice(&raw)?;
        let order = |v: &serde_json::Value| {
            (
                v["recorded_at_ms"].as_u64().unwrap_or(0),
                v["sequence"].as_u64().unwrap_or(0),
            )
        };
        if order(&old) > order(&result) {
            return Ok(());
        }
    }
    result["view_path"] = view.display().to_string().into();
    write_json(&path, &result)
}

pub fn results(dir: &Path, space: &SavedSpace) -> Result<Vec<serde_json::Value>> {
    let Some(id) = &space.id else {
        return Ok(vec![]);
    };
    if !crate::valid_space_id(id) {
        bail!("invalid Space identity");
    }
    let entries = match fs::read_dir(dir.join("team-results").join(id)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut results: Vec<serde_json::Value> = vec![];
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            results.push(serde_json::from_slice(&fs::read(path)?)?);
        }
    }
    results.sort_by_key(|value| std::cmp::Reverse(value["recorded_at_ms"].as_u64().unwrap_or(0)));
    Ok(results)
}

pub fn result_text(result: &serde_json::Value) -> String {
    let s = |key: &str| result[key].as_str().unwrap_or("unknown");
    let mut lines = vec![format!("{}: {}", s("name"), s("view"))];
    if let Some(error) = result["error"].as_str() {
        lines.push(format!("Error: {error}"));
    }
    if let Some(seats) = result["seats"].as_array() {
        for seat in seats {
            lines.push(format!(
                "{}: {}",
                seat["name"].as_str().unwrap_or("session"),
                seat["state"].as_str().unwrap_or("unknown")
            ));
        }
    }
    lines.push(format!("Launch: {}", s("launch")));
    lines.push(
        "This records the last open result. Check team details for current session state.".into(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_links_accept_web_and_absolute_paths_but_reject_control_text() {
        for link in [
            "https://example.com/review",
            "http://localhost:8080",
            "/work/design notes.md",
        ] {
            assert!(validate_link(link).is_ok(), "{link}");
        }
        for link in [
            "relative/path",
            "ftp://example.com",
            "",
            "https://example.com/\nnext",
            "/work/\0bad",
        ] {
            assert!(validate_link(link).is_err(), "{link:?}");
        }
        assert!(validate_link(&format!("https://example.com/{}", "x".repeat(4096))).is_err());
    }

    #[test]
    fn team_text_distinguishes_active_snoozed_and_stale_requests() {
        let request = |message: &str, snoozed_until_ms| AttentionRequest {
            pane_id: 1,
            revision: 1,
            message: message.into(),
            source: "agent".into(),
            raised_at_ms: 1,
            snoozed_until_ms,
        };
        let details = TeamDetails {
            name: "Review".into(),
            space_id: None,
            observed_at_ms: 100,
            source: "live daemon snapshot".into(),
            sessions_needing_input: 1,
            letters: 4,
            sessions: vec![
                SessionDetails {
                    name: "reviewer".into(),
                    role: Some("review".into()),
                    session_id: Some(1),
                    panes: vec![1],
                    state: "running".into(),
                    letters: 3,
                    attention: vec![request("approve patch", 0), request("check later", 200)],
                },
                SessionDetails {
                    name: "builder".into(),
                    role: None,
                    session_id: None,
                    panes: vec![],
                    state: "stopped".into(),
                    letters: 1,
                    attention: vec![request("old question", 0)],
                },
            ],
            links: BTreeMap::from([("design".into(), "https://example.com/design".into())]),
        };
        assert_eq!(
            details.text(),
            concat!(
                "Review — 1 sessions need you; 4 letters\n",
                "live daemon snapshot\n",
                "reviewer | review | running | 3 letters\n",
                "  needs input: approve patch (agent)\n",
                "  snoozed: check later (agent)\n",
                "builder | no role | stopped | 1 letters\n",
                "  stale: old question (agent)\n",
                "design: https://example.com/design"
            )
        );
    }

    #[test]
    fn retained_results_keep_latest_completion_per_view_and_survive_rename() {
        let dir = std::env::temp_dir().join(format!(
            "pmux-result-test-{}",
            crate::new_space_id().unwrap()
        ));
        let space: SavedSpace = serde_json::from_value(serde_json::json!({
            "version":2,"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","saved_at_unix":0,
            "sessions":[],"tabs":[],"active_tab":0,"focused_session":null
        }))
        .unwrap();
        let result = |time, sequence| serde_json::json!({"name":"before-rename", "recorded_at_ms":time,"sequence":sequence,"view":"partial"});
        record_result(&dir, &space, Path::new("/view/a"), result(30, 3)).unwrap();
        record_result(&dir, &space, Path::new("/view/a"), result(20, 2)).unwrap();
        record_result(&dir, &space, Path::new("/view/a"), result(30, 2)).unwrap();
        record_result(&dir, &space, Path::new("/view/b"), result(25, 1)).unwrap();
        let found = results(&dir, &space).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["sequence"], 3);
        assert_eq!(found[0]["view_path"], "/view/a");
        assert_eq!(found[1]["view_path"], "/view/b");
        fs::remove_dir_all(dir).unwrap();
    }
}
