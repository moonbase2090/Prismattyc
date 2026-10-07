//! Remote Space catalog schema and SSH destination identities (issue #24).
//!
//! Contract: `docs/design/remote-spaces-ssh.md`. The remote CLI builds a
//! [`Catalog`] from saved Space files joined with a live daemon snapshot;
//! the host parses it with [`parse_catalog`]. Names are display data only.
//! Routing uses [`RemoteSpaceKey`] and [`RemoteSessionId`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::control::Snapshot;
use crate::layout_file::{space_active_session, space_sessions_in_tab_order, SavedSpace};

/// Catalog wire version. Readers reject every other value.
pub const CATALOG_VERSION: u32 = 1;
/// Largest encoded catalog a reader accepts or a producer emits.
pub const MAX_CATALOG_BYTES: usize = 1 << 20;
/// Largest number of Space records (running plus unavailable).
pub const MAX_CATALOG_SPACES: usize = 512;
/// Largest number of live sessions in one Space record.
pub const MAX_SPACE_SESSIONS: usize = 256;
/// Largest display name, label, or producer string in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 256;

const MAX_DESTINATION_ID_BYTES: usize = 64;
const MAX_SSH_ALIAS_BYTES: usize = 255;

/// Config-chosen destination identity. Namespaces remote identifiers.
///
/// 1..=64 bytes of `a-z`, `0-9` and `-`, not starting or ending with `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DestinationId(String);

impl DestinationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DestinationId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        let bytes = value.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_DESTINATION_ID_BYTES {
            bail!("destination id must be 1..={MAX_DESTINATION_ID_BYTES} bytes");
        }
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            || value.starts_with('-')
            || value.ends_with('-')
        {
            bail!("destination id {value:?} must use a-z, 0-9 and inner '-'");
        }
        Ok(Self(value))
    }
}

impl From<DestinationId> for String {
    fn from(value: DestinationId) -> Self {
        value.0
    }
}

impl FromStr for DestinationId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::try_from(value.to_string())
    }
}

impl fmt::Display for DestinationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host alias resolved by the system SSH configuration.
///
/// ASCII letters, digits, `.`, `_` and `-`; never starts with `-`, so it
/// cannot be read as an `ssh` option. User and port belong in SSH config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshAlias(String);

impl SshAlias {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SshAlias {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        let bytes = value.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_SSH_ALIAS_BYTES {
            bail!("ssh alias must be 1..={MAX_SSH_ALIAS_BYTES} bytes");
        }
        if value.starts_with('-') {
            bail!("ssh alias {value:?} must not start with '-'");
        }
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("ssh alias {value:?} must name a Host from SSH config (letters, digits, '.', '_', '-')");
        }
        Ok(Self(value))
    }
}

impl From<SshAlias> for String {
    fn from(value: SshAlias) -> Self {
        value.0
    }
}

impl FromStr for SshAlias {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::try_from(value.to_string())
    }
}

impl fmt::Display for SshAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable remote Space identity: the saved file's 32-hex `id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemoteSpaceId(String);

impl RemoteSpaceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RemoteSpaceId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        if !crate::valid_space_id(&value) {
            bail!("remote Space id must be 32 hex digits");
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl From<RemoteSpaceId> for String {
    fn from(value: RemoteSpaceId) -> Self {
        value.0
    }
}

impl fmt::Display for RemoteSpaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Live session id in the remote daemon. Valid only while that daemon runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteSessionId(pub u64);

/// Routing identity for one remote Space. Equal Space ids on two
/// destinations stay distinct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteSpaceKey {
    pub destination: DestinationId,
    pub space: RemoteSpaceId,
}

/// One configured SSH destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshDestination {
    pub id: DestinationId,
    pub label: String,
    pub ssh_alias: SshAlias,
}

/// `[[remote]]` entry in the shared config file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteDestinationEntry {
    pub id: String,
    pub ssh: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// Validate `[[remote]]` entries. Rejects duplicate ids; the label defaults
/// to the id.
pub fn parse_destinations(entries: &[RemoteDestinationEntry]) -> Result<Vec<SshDestination>> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = DestinationId::from_str(&entry.id).context("[[remote]] id")?;
        if !seen.insert(id.clone()) {
            bail!("[[remote]] id {id:?} appears more than once");
        }
        let ssh_alias =
            SshAlias::from_str(&entry.ssh).with_context(|| format!("[[remote]] {id} ssh"))?;
        let label = entry
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(id.as_str())
            .to_string();
        validate_display("[[remote]] label", &label)?;
        out.push(SshDestination {
            id,
            label,
            ssh_alias,
        });
    }
    Ok(out)
}

/// Remote catalog: running Spaces plus saved Spaces that cannot be attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub version: u32,
    /// Producing `pmux` version, for diagnostics only.
    pub producer: String,
    pub spaces: Vec<RemoteSpace>,
    pub unavailable: Vec<UnavailableSpace>,
}

/// A saved Space that owns at least one live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSpace {
    pub id: RemoteSpaceId,
    pub name: String,
    /// Live owned sessions: saved tab order, then the rest by id.
    pub sessions: Vec<RemoteSession>,
    pub active_session: RemoteSessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSession {
    pub id: RemoteSessionId,
    pub name: String,
}

/// A saved Space the catalog lists but never offers for attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnavailableSpace {
    /// Display only. Control characters are replaced.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RemoteSpaceId>,
    pub reason: UnavailableReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// Legacy file without a stable id. `pmux space open` assigns one.
    MissingIdentity,
    /// Two files claim the same id.
    DuplicateIdentity,
    /// The daemon has no session owned by this Space.
    NoLiveSessions,
    /// The file did not load.
    Unreadable,
    /// A name or count is outside the catalog limits.
    OutOfLimits,
}

/// Join saved Spaces with the live snapshot. Reads nothing else and starts
/// nothing: a Space is running only when the daemon reports owned sessions.
pub fn build_catalog(saved: Vec<(String, Result<SavedSpace>)>, snapshot: &Snapshot) -> Catalog {
    let mut id_counts: HashMap<String, usize> = HashMap::new();
    for (_, space) in &saved {
        if let Some(id) = space.as_ref().ok().and_then(|space| space.id.as_ref()) {
            *id_counts.entry(id.to_ascii_lowercase()).or_default() += 1;
        }
    }
    let mut spaces = Vec::new();
    let mut unavailable = Vec::new();
    for (name, space) in saved {
        let unavailable_as = |id: Option<RemoteSpaceId>, reason| UnavailableSpace {
            name: display_name(&name),
            id,
            reason,
        };
        let space = match space {
            Ok(space) => space,
            Err(_) => {
                unavailable.push(unavailable_as(None, UnavailableReason::Unreadable));
                continue;
            }
        };
        let Some(id) = space
            .id
            .clone()
            .and_then(|id| RemoteSpaceId::try_from(id).ok())
        else {
            unavailable.push(unavailable_as(None, UnavailableReason::MissingIdentity));
            continue;
        };
        if id_counts.get(id.as_str()).copied().unwrap_or(0) > 1 {
            unavailable.push(unavailable_as(
                Some(id),
                UnavailableReason::DuplicateIdentity,
            ));
            continue;
        }
        let mut owned: Vec<_> = snapshot
            .sessions
            .iter()
            .filter(|session| {
                session
                    .space_id
                    .as_deref()
                    .is_some_and(|owner| owner.eq_ignore_ascii_case(id.as_str()))
            })
            .collect();
        if owned.is_empty() {
            unavailable.push(unavailable_as(Some(id), UnavailableReason::NoLiveSessions));
            continue;
        }
        let order = space_sessions_in_tab_order(&space);
        owned.sort_by_key(|session| {
            (
                order
                    .iter()
                    .position(|name| *name == session.name)
                    .unwrap_or(usize::MAX),
                session.id,
            )
        });
        let active = space_active_session(&space)
            .and_then(|name| owned.iter().find(|session| session.name == name))
            .unwrap_or(&owned[0])
            .id;
        let record = RemoteSpace {
            id: id.clone(),
            name: name.clone(),
            sessions: owned
                .iter()
                .map(|session| RemoteSession {
                    id: RemoteSessionId(session.id),
                    name: session.name.clone(),
                })
                .collect(),
            active_session: RemoteSessionId(active),
        };
        if validate_space(&record).is_err() {
            unavailable.push(unavailable_as(Some(id), UnavailableReason::OutOfLimits));
            continue;
        }
        spaces.push(record);
    }
    Catalog {
        version: CATALOG_VERSION,
        producer: env!("CARGO_PKG_VERSION").to_string(),
        spaces,
        unavailable,
    }
}

/// Validate and serialize a catalog as one JSON line.
pub fn encode_catalog(catalog: &Catalog) -> Result<String> {
    validate_catalog(catalog)?;
    let text = serde_json::to_string(catalog).context("serialize catalog")?;
    if text.len() > MAX_CATALOG_BYTES {
        bail!("catalog is {} bytes (max {MAX_CATALOG_BYTES})", text.len());
    }
    Ok(text)
}

/// Parse untrusted catalog bytes from a remote CLI.
pub fn parse_catalog(bytes: &[u8]) -> Result<Catalog> {
    if bytes.len() > MAX_CATALOG_BYTES {
        bail!(
            "catalog response is {} bytes (max {MAX_CATALOG_BYTES})",
            bytes.len()
        );
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("catalog response is not JSON")?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .context("catalog response has no version")?;
    if version != u64::from(CATALOG_VERSION) {
        bail!("unsupported catalog version {version} (want {CATALOG_VERSION})");
    }
    let catalog: Catalog = serde_json::from_value(value).context("malformed catalog record")?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    if catalog.version != CATALOG_VERSION {
        bail!(
            "unsupported catalog version {} (want {CATALOG_VERSION})",
            catalog.version
        );
    }
    validate_display("catalog producer", &catalog.producer)?;
    let total = catalog.spaces.len() + catalog.unavailable.len();
    if total > MAX_CATALOG_SPACES {
        bail!("catalog lists {total} Spaces (max {MAX_CATALOG_SPACES})");
    }
    let mut space_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    for space in &catalog.spaces {
        validate_space(space)?;
        if !space_ids.insert(&space.id) {
            bail!("catalog repeats Space id {}", space.id);
        }
        for session in &space.sessions {
            if !session_ids.insert(session.id) {
                bail!("catalog lists session {} in two Spaces", session.id.0);
            }
        }
    }
    for space in &catalog.unavailable {
        validate_display("unavailable Space name", &space.name)?;
        if let Some(id) = &space.id {
            if space_ids.contains(id) {
                bail!("catalog lists Space id {id} as both running and unavailable");
            }
        }
    }
    Ok(())
}

fn validate_space(space: &RemoteSpace) -> Result<()> {
    validate_display("Space name", &space.name)?;
    if space.sessions.is_empty() {
        bail!("running Space {:?} lists no sessions", space.name);
    }
    if space.sessions.len() > MAX_SPACE_SESSIONS {
        bail!(
            "Space {:?} lists {} sessions (max {MAX_SPACE_SESSIONS})",
            space.name,
            space.sessions.len()
        );
    }
    let mut ids = HashSet::new();
    for session in &space.sessions {
        validate_display("session name", &session.name)?;
        if !ids.insert(session.id) {
            bail!("Space {:?} repeats session {}", space.name, session.id.0);
        }
    }
    if !ids.contains(&space.active_session) {
        bail!(
            "Space {:?} active session {} is not one of its sessions",
            space.name,
            space.active_session.0
        );
    }
    Ok(())
}

fn validate_display(what: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_DISPLAY_NAME_BYTES {
        bail!("{what} must be 1..={MAX_DISPLAY_NAME_BYTES} bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("{what} contains a control character");
    }
    Ok(())
}

/// Displayable, bounded form of a file name for an unavailable record.
fn display_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        let ch = if ch.is_control() { '\u{fffd}' } else { ch };
        if out.len() + ch.len_utf8() > MAX_DISPLAY_NAME_BYTES {
            break;
        }
        out.push(ch);
    }
    if out.is_empty() {
        out.push('\u{fffd}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::SessionSnapshot;
    use crate::layout_file::{stub_space_session, SavedSpaceTab, OWNED_SPACE_VERSION};

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    fn space(id: Option<&str>, sessions: &[&str], focus: Option<&str>) -> SavedSpace {
        SavedSpace {
            version: OWNED_SPACE_VERSION,
            id: id.map(str::to_string),
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: sessions
                .iter()
                .map(|name| stub_space_session(*name))
                .collect(),
            tabs: vec![SavedSpaceTab {
                title: "t".into(),
                sessions: sessions.iter().map(|name| name.to_string()).collect(),
                layout: None,
            }],
            active_tab: 0,
            focused_session: focus.map(str::to_string),
        }
    }

    fn live(id: u64, name: &str, owner: Option<&str>) -> SessionSnapshot {
        SessionSnapshot {
            id,
            name: name.into(),
            agent_id: None,
            space_id: owner.map(str::to_string),
            windows: Vec::new(),
        }
    }

    fn snapshot(sessions: Vec<SessionSnapshot>) -> Snapshot {
        Snapshot {
            sequence: 1,
            sessions,
        }
    }

    #[test]
    fn running_space_lists_owned_sessions_in_tab_order_with_saved_focus() {
        let catalog = build_catalog(
            vec![("work".into(), Ok(space(Some(A), &["b", "a"], Some("a"))))],
            &snapshot(vec![
                live(1, "a", Some(A)),
                live(2, "b", Some(A)),
                live(3, "extra", Some(A)),
                live(4, "b", None),
            ]),
        );
        assert!(catalog.unavailable.is_empty());
        let [work] = catalog.spaces.as_slice() else {
            panic!("{catalog:?}")
        };
        assert_eq!(work.id.as_str(), A);
        let ids: Vec<u64> = work.sessions.iter().map(|s| s.id.0).collect();
        assert_eq!(ids, vec![2, 1, 3], "tab order, then unsaved owned sessions");
        assert_eq!(work.active_session, RemoteSessionId(1));
    }

    #[test]
    fn active_session_falls_back_to_first_live_owned_session() {
        let catalog = build_catalog(
            vec![(
                "work".into(),
                Ok(space(Some(A), &["gone", "b"], Some("gone"))),
            )],
            &snapshot(vec![live(7, "b", Some(A)), live(8, "gone", None)]),
        );
        assert_eq!(catalog.spaces[0].active_session, RemoteSessionId(7));
    }

    #[test]
    fn unattachable_spaces_are_reported_with_reasons() {
        let catalog = build_catalog(
            vec![
                ("legacy".into(), Ok(space(None, &["a"], None))),
                ("idle".into(), Ok(space(Some(A), &["a"], None))),
                ("dup-1".into(), Ok(space(Some(B), &["x"], None))),
                ("dup-2".into(), Ok(space(Some(B), &["y"], None))),
                ("bad\u{1b}[2J".into(), Err(anyhow::anyhow!("parse"))),
            ],
            // A same-named but unowned session never makes a Space running.
            &snapshot(vec![live(1, "a", None), live(2, "x", Some(B))]),
        );
        assert!(catalog.spaces.is_empty());
        let reasons: Vec<_> = catalog
            .unavailable
            .iter()
            .map(|space| (space.name.as_str(), space.reason))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("legacy", UnavailableReason::MissingIdentity),
                ("idle", UnavailableReason::NoLiveSessions),
                ("dup-1", UnavailableReason::DuplicateIdentity),
                ("dup-2", UnavailableReason::DuplicateIdentity),
                ("bad\u{fffd}[2J", UnavailableReason::Unreadable),
            ]
        );
    }

    #[test]
    fn control_characters_in_session_names_mark_space_out_of_limits() {
        let catalog = build_catalog(
            vec![("work".into(), Ok(space(Some(A), &["a"], None)))],
            &snapshot(vec![live(1, "a\u{7}", Some(A))]),
        );
        assert!(catalog.spaces.is_empty());
        assert_eq!(
            catalog.unavailable[0].reason,
            UnavailableReason::OutOfLimits
        );
        encode_catalog(&catalog).expect("still encodable");
    }

    #[test]
    fn encode_then_parse_round_trips() {
        let catalog = build_catalog(
            vec![
                ("work".into(), Ok(space(Some(A), &["a"], None))),
                ("legacy".into(), Ok(space(None, &["b"], None))),
            ],
            &snapshot(vec![live(1, "a", Some(A))]),
        );
        let text = encode_catalog(&catalog).unwrap();
        assert!(!text.contains('\n'));
        assert_eq!(parse_catalog(text.as_bytes()).unwrap(), catalog);
    }

    fn valid_json() -> serde_json::Value {
        serde_json::json!({
            "version": CATALOG_VERSION,
            "producer": "0.2.19",
            "spaces": [{
                "id": A,
                "name": "work",
                "sessions": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}],
                "active_session": 2
            }],
            "unavailable": [{"name": "legacy", "reason": "missing_identity"}]
        })
    }

    fn parse_value(value: &serde_json::Value) -> Result<Catalog> {
        parse_catalog(serde_json::to_string(value).unwrap().as_bytes())
    }

    #[test]
    fn parse_accepts_valid_catalog() {
        let catalog = parse_value(&valid_json()).unwrap();
        assert_eq!(catalog.spaces[0].active_session, RemoteSessionId(2));
        assert_eq!(
            catalog.unavailable[0].reason,
            UnavailableReason::MissingIdentity
        );
    }

    #[test]
    fn parse_rejects_malformed_and_unsupported_catalogs() {
        type Edit = fn(&mut serde_json::Value);
        let cases: &[(&str, Edit)] = &[
            ("future version", |v| v["version"] = 2.into()),
            ("missing version", |v| {
                v.as_object_mut().unwrap().remove("version");
            }),
            ("unknown field", |v| v["extra"] = true.into()),
            ("bad space id", |v| v["spaces"][0]["id"] = "work".into()),
            ("active not listed", |v| {
                v["spaces"][0]["active_session"] = 9.into()
            }),
            ("no sessions", |v| {
                v["spaces"][0]["sessions"] = serde_json::json!([]);
                v["spaces"][0]["active_session"] = 1.into();
            }),
            ("control char name", |v| {
                v["spaces"][0]["name"] = "a\u{1b}]0;x\u{7}".into()
            }),
            ("empty name", |v| {
                v["spaces"][0]["sessions"][0]["name"] = "".into()
            }),
            ("long name", |v| {
                v["spaces"][0]["name"] = "x".repeat(MAX_DISPLAY_NAME_BYTES + 1).into()
            }),
            ("repeated session", |v| {
                v["spaces"][0]["sessions"][1]["id"] = 1.into();
                v["spaces"][0]["active_session"] = 1.into();
            }),
            ("session in two spaces", |v| {
                let mut other = v["spaces"][0].clone();
                other["id"] = B.into();
                v["spaces"].as_array_mut().unwrap().push(other);
            }),
            ("repeated space id", |v| {
                let mut other = v["spaces"][0].clone();
                other["sessions"] = serde_json::json!([{"id": 5, "name": "c"}]);
                other["active_session"] = 5.into();
                v["spaces"].as_array_mut().unwrap().push(other);
            }),
            ("running and unavailable", |v| {
                v["unavailable"][0]["id"] = A.into()
            }),
            ("unknown reason", |v| {
                v["unavailable"][0]["reason"] = "gone".into()
            }),
        ];
        for (name, edit) in cases {
            let mut value = valid_json();
            edit(&mut value);
            assert!(parse_value(&value).is_err(), "{name} must be rejected");
        }
        assert!(parse_catalog(b"not json").is_err());
    }

    #[test]
    fn parse_rejects_oversized_input_before_decoding() {
        let big = vec![b' '; MAX_CATALOG_BYTES + 1];
        let error = parse_catalog(&big).unwrap_err().to_string();
        assert!(error.contains("max"), "{error}");
    }

    #[test]
    fn parse_rejects_too_many_spaces() {
        let mut value = valid_json();
        value["unavailable"] = serde_json::Value::Array(
            (0..MAX_CATALOG_SPACES)
                .map(|i| serde_json::json!({"name": format!("s{i}"), "reason": "no_live_sessions"}))
                .collect(),
        );
        assert!(parse_value(&value).is_err());
    }

    #[test]
    fn destination_and_alias_parsing_rejects_option_like_and_shell_text() {
        for good in ["devbox", "a", "linux-box-2"] {
            DestinationId::from_str(good).unwrap();
        }
        for bad in ["", "-x", "x-", "Dev", "a b", "a/b", &"a".repeat(65)] {
            assert!(DestinationId::from_str(bad).is_err(), "{bad:?}");
        }
        for good in ["devbox", "dev.example.com", "host_1", "h-2"] {
            SshAlias::from_str(good).unwrap();
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "user@host",
            "host;rm",
            "a b",
            "$(x)",
            "host:22",
        ] {
            assert!(SshAlias::from_str(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn destinations_parse_from_config_entries() {
        let parsed: toml::Value = toml::from_str(
            r#"
            [[remote]]
            id = "devbox"
            ssh = "devbox"
            label = "Dev box"

            [[remote]]
            id = "lab"
            ssh = "lab.internal"
            "#,
        )
        .unwrap();
        let entries: Vec<RemoteDestinationEntry> = parsed["remote"].clone().try_into().unwrap();
        let destinations = parse_destinations(&entries).unwrap();
        assert_eq!(destinations[0].label, "Dev box");
        assert_eq!(destinations[1].label, "lab", "label defaults to the id");
        assert_eq!(destinations[1].ssh_alias.as_str(), "lab.internal");

        let duplicate = vec![entries[0].clone(), entries[0].clone()];
        assert!(parse_destinations(&duplicate).is_err());
        let bad_alias = vec![RemoteDestinationEntry {
            id: "x".into(),
            ssh: "-oProxyCommand=sh".into(),
            label: None,
        }];
        assert!(parse_destinations(&bad_alias).is_err());
    }

    #[test]
    fn remote_space_keys_namespace_equal_space_ids() {
        let key = |destination: &str| RemoteSpaceKey {
            destination: DestinationId::from_str(destination).unwrap(),
            space: RemoteSpaceId::try_from(A.to_string()).unwrap(),
        };
        assert_ne!(key("devbox"), key("lab"));
    }

    #[test]
    fn display_name_bounds_and_scrubs() {
        assert_eq!(display_name("a\nb"), "a\u{fffd}b");
        assert_eq!(display_name(""), "\u{fffd}");
        let long = display_name(&"é".repeat(MAX_DISPLAY_NAME_BYTES));
        assert!(long.len() <= MAX_DISPLAY_NAME_BYTES);
    }
}
