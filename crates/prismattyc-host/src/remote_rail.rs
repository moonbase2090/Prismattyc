//! Remote destinations in the Space rail (issue #24, option B).
//!
//! One chip per configured `[[remote]]` destination follows the local
//! chips. Selecting a chip connects (or refreshes) that destination and
//! opens its Space list. The catalog comes from [`CatalogFetcher`]; this
//! module turns its state into chip views and list rows. Remote Spaces are
//! never local chips, so local rename and delete never see a remote key.
//! Catalog state is app-wide; which list is open belongs to each window.

use prismattyc_mux::remote_catalog::{
    DestinationId, RemoteSessionId, RemoteSpaceKey, SshDestination, UnavailableReason,
};

use crate::remote_catalog::{CatalogFetcher, CatalogState, Wake};
use crate::space_rail::{DestinationStatus, RailDestinationView};

/// App-wide remote rail: one fetcher shared by every window.
pub struct RemoteRail {
    destinations: Vec<SshDestination>,
    fetcher: CatalogFetcher,
}

/// One row of an open destination's Space list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRow {
    pub name: String,
    pub detail: String,
    pub kind: RemoteRowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRowKind {
    Space {
        key: RemoteSpaceKey,
        session: RemoteSessionId,
        session_name: String,
    },
    Unavailable,
    Retry,
}

impl RemoteRail {
    pub fn new(destinations: Vec<SshDestination>, wake: Wake) -> Self {
        Self::with_fetcher(destinations, CatalogFetcher::new(wake))
    }

    pub fn with_fetcher(destinations: Vec<SshDestination>, fetcher: CatalogFetcher) -> Self {
        Self {
            destinations,
            fetcher,
        }
    }

    /// Replace the configured destinations (config reload). Removed ones are
    /// disconnected; returns whether the chips changed.
    pub fn set_destinations(&mut self, destinations: Vec<SshDestination>) -> bool {
        if destinations == self.destinations {
            return false;
        }
        for old in &self.destinations {
            if !destinations.iter().any(|new| new.id == old.id) {
                self.fetcher.disconnect(&old.id);
            }
        }
        for new in &destinations {
            // A changed alias must not keep a catalog read from another host.
            if self
                .destinations
                .iter()
                .any(|old| old.id == new.id && old.ssh_alias != new.ssh_alias)
            {
                self.fetcher.disconnect(&new.id);
            }
        }
        self.destinations = destinations;
        true
    }

    pub fn destination(&self, id: &DestinationId) -> Option<&SshDestination> {
        self.destinations.iter().find(|dest| &dest.id == id)
    }

    /// Chip views; `open` is the destination whose list this window shows.
    pub fn views(&self, open: Option<&DestinationId>) -> Vec<RailDestinationView> {
        self.destinations
            .iter()
            .map(|dest| {
                let state = self.fetcher.state(&dest.id);
                let label = match state {
                    CatalogState::Ready { catalog, .. } => {
                        format!("{} {}", dest.label, catalog.spaces.len())
                    }
                    _ => dest.label.clone(),
                };
                RailDestinationView {
                    label,
                    status: status_of(state),
                    open: open == Some(&dest.id),
                }
            })
            .collect()
    }

    /// Chip `index` was selected: connect, retry, or refresh it and return
    /// the destination whose list to open. A click while a request runs
    /// starts nothing new.
    pub fn activate(&mut self, index: usize) -> Option<DestinationId> {
        let dest = self.destinations.get(index)?;
        match self.fetcher.state(&dest.id) {
            CatalogState::Loading { .. } => {}
            CatalogState::Disconnected => {
                self.fetcher.connect(dest);
            }
            CatalogState::Ready { .. } | CatalogState::Failed { .. } => {
                self.fetcher.reconnect(dest);
            }
        }
        Some(dest.id.clone())
    }

    /// Apply finished requests. Returns whether any chip or list changed.
    pub fn poll(&mut self) -> bool {
        !self.fetcher.poll().is_empty()
    }

    /// Subtitle of a destination's list: which machine and its state.
    pub fn status_line(&self, id: &DestinationId) -> String {
        let Some(dest) = self.destination(id) else {
            return String::new();
        };
        match self.fetcher.state(&dest.id) {
            CatalogState::Disconnected => format!("{} · not connected", dest.label),
            CatalogState::Loading { .. } => format!("{} · connecting…", dest.label),
            CatalogState::Ready { catalog, .. } => {
                let count = catalog.spaces.len();
                let noun = if count == 1 { "Space" } else { "Spaces" };
                format!("{} · {count} running {noun}", dest.label)
            }
            CatalogState::Failed { error, .. } => format!("{} · {error}", dest.label),
        }
    }

    /// Rows of a destination's list. Running Spaces first, then Spaces that
    /// cannot be attached with the reason, or one Retry row after a failure.
    pub fn rows(&self, id: &DestinationId) -> Vec<RemoteRow> {
        let Some(dest) = self.destination(id) else {
            return Vec::new();
        };
        match self.fetcher.state(&dest.id) {
            CatalogState::Disconnected | CatalogState::Loading { .. } => Vec::new(),
            CatalogState::Failed { error, .. } => vec![RemoteRow {
                name: "Retry connection".into(),
                detail: error.to_string(),
                kind: RemoteRowKind::Retry,
            }],
            CatalogState::Ready { catalog, .. } => {
                let mut rows: Vec<RemoteRow> = catalog
                    .spaces
                    .iter()
                    .map(|space| {
                        let active = space
                            .sessions
                            .iter()
                            .find(|session| session.id == space.active_session)
                            .expect("catalog validation keeps the active session");
                        let count = space.sessions.len();
                        let noun = if count == 1 { "session" } else { "sessions" };
                        RemoteRow {
                            name: space.name.clone(),
                            detail: format!("{count} {noun} · {}", active.name),
                            kind: RemoteRowKind::Space {
                                key: RemoteSpaceKey {
                                    destination: dest.id.clone(),
                                    space: space.id.clone(),
                                },
                                session: active.id,
                                session_name: active.name.clone(),
                            },
                        }
                    })
                    .collect();
                rows.extend(catalog.unavailable.iter().map(|space| RemoteRow {
                    name: space.name.clone(),
                    detail: unavailable_text(space.reason).into(),
                    kind: RemoteRowKind::Unavailable,
                }));
                rows
            }
        }
    }

    /// Row for a list selection by name.
    pub fn row(&self, id: &DestinationId, name: &str) -> Option<RemoteRow> {
        self.rows(id).into_iter().find(|row| row.name == name)
    }

    /// Retry a destination after a failure.
    pub fn retry(&mut self, id: &DestinationId) {
        if let Some(dest) = self.destination(id).cloned() {
            self.fetcher.reconnect(&dest);
        }
    }
}

fn status_of(state: &CatalogState) -> DestinationStatus {
    match state {
        CatalogState::Disconnected => DestinationStatus::Disconnected,
        CatalogState::Loading { .. } => DestinationStatus::Loading,
        CatalogState::Ready { .. } => DestinationStatus::Ready,
        CatalogState::Failed { .. } => DestinationStatus::Failed,
    }
}

fn unavailable_text(reason: UnavailableReason) -> &'static str {
    match reason {
        UnavailableReason::MissingIdentity => {
            "saved before Space ids; open it once on that machine"
        }
        UnavailableReason::DuplicateIdentity => "two saved files share its id",
        UnavailableReason::NoLiveSessions => "no running sessions",
        UnavailableReason::Unreadable => "saved file could not be read",
        UnavailableReason::OutOfLimits => "name or size outside catalog limits",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use prismattyc_mux::remote_catalog::SshAlias;
    use std::process::Command;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CATALOG: &str = r#"{"version":1,"producer":"test","spaces":[{"id":"0123456789abcdef0123456789abcdef","name":"work","sessions":[{"id":3,"name":"work-1"},{"id":4,"name":"work-2"}],"active_session":4}],"unavailable":[{"name":"old","reason":"missing_identity"}]}"#;

    fn destination(id: &str, alias: &str) -> SshDestination {
        SshDestination {
            id: DestinationId::from_str(id).unwrap(),
            label: id.into(),
            ssh_alias: SshAlias::from_str(alias).unwrap(),
        }
    }

    fn rail(script: String, destinations: Vec<SshDestination>) -> RemoteRail {
        let fetcher = CatalogFetcher::with_launcher(
            Arc::new(move |_| {
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(&script);
                command
            }),
            Arc::new(|| {}),
            Duration::from_secs(10),
        );
        RemoteRail::with_fetcher(destinations, fetcher)
    }

    fn settle(rail: &mut RemoteRail) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while rail
            .views(None)
            .iter()
            .any(|view| view.status == DestinationStatus::Loading)
        {
            rail.poll();
            assert!(Instant::now() < deadline, "request never settled");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn chips_start_disconnected_and_nothing_runs_until_selected() {
        let rail = rail(
            "exit 99".into(),
            vec![destination("devbox", "devbox"), destination("lab", "lab")],
        );
        let views = rail.views(None);
        assert_eq!(views.len(), 2);
        assert!(views
            .iter()
            .all(|view| view.status == DestinationStatus::Disconnected && !view.open));
        let devbox = DestinationId::from_str("devbox").unwrap();
        assert!(rail.rows(&devbox).is_empty());
        assert_eq!(rail.status_line(&devbox), "devbox · not connected");
        assert_eq!(
            rail.status_line(&DestinationId::from_str("gone").unwrap()),
            ""
        );
    }

    #[test]
    fn selecting_a_chip_connects_and_lists_running_and_unavailable_spaces() {
        let mut rail = rail(
            format!("printf '%s' '{CATALOG}'"),
            vec![destination("devbox", "devbox")],
        );
        let id = rail.activate(0).unwrap();
        assert_eq!(id.as_str(), "devbox");
        assert_eq!(rail.views(None)[0].status, DestinationStatus::Loading);
        assert!(rail.views(Some(&id))[0].open);
        assert!(!rail.views(None)[0].open, "open is per window");
        assert_eq!(rail.status_line(&id), "devbox · connecting…");
        settle(&mut rail);
        let view = &rail.views(None)[0];
        assert_eq!(view.status, DestinationStatus::Ready);
        assert_eq!(view.label, "devbox 1");
        assert_eq!(rail.status_line(&id), "devbox · 1 running Space");
        let rows = rail.rows(&id);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "work");
        assert_eq!(rows[0].detail, "2 sessions · work-2");
        let RemoteRowKind::Space {
            key,
            session,
            session_name,
        } = &rows[0].kind
        else {
            panic!("{rows:?}")
        };
        assert_eq!(key.destination.as_str(), "devbox");
        assert_eq!(key.space.as_str(), "0123456789abcdef0123456789abcdef");
        assert_eq!(*session, RemoteSessionId(4), "the active session, by id");
        assert_eq!(session_name, "work-2");
        assert_eq!(rows[1].kind, RemoteRowKind::Unavailable);
        assert!(rows[1].detail.contains("open it once"));
        assert_eq!(
            rail.row(&id, "old").unwrap().kind,
            RemoteRowKind::Unavailable
        );
        assert!(rail.row(&id, "missing").is_none());
    }

    #[test]
    fn failure_offers_retry_with_the_error_text() {
        let mut rail = rail(
            "echo 'Permission denied (publickey).' >&2; exit 255".into(),
            vec![destination("devbox", "devbox")],
        );
        let id = rail.activate(0).unwrap();
        settle(&mut rail);
        assert_eq!(rail.views(None)[0].status, DestinationStatus::Failed);
        let rows = rail.rows(&id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RemoteRowKind::Retry);
        assert!(rows[0].detail.contains("ssh-agent"), "{rows:?}");
        assert!(rail
            .status_line(&id)
            .starts_with("devbox · SSH authentication failed"));
        rail.retry(&id);
        assert_eq!(rail.views(None)[0].status, DestinationStatus::Loading);
    }

    #[test]
    fn config_reload_drops_removed_destinations() {
        let mut rail = rail(
            format!("printf '%s' '{CATALOG}'"),
            vec![destination("devbox", "devbox"), destination("lab", "lab")],
        );
        let id = rail.activate(0).unwrap();
        settle(&mut rail);
        assert!(!rail.set_destinations(vec![
            destination("devbox", "devbox"),
            destination("lab", "lab")
        ]));
        assert!(rail.set_destinations(vec![destination("lab", "lab")]));
        assert_eq!(rail.views(Some(&id)).len(), 1);
        assert!(rail.destination(&id).is_none());
        assert!(rail.rows(&id).is_empty());
    }

    #[test]
    fn changed_alias_forgets_the_old_catalog() {
        let mut rail = rail(
            format!("printf '%s' '{CATALOG}'"),
            vec![destination("devbox", "devbox")],
        );
        rail.activate(0);
        settle(&mut rail);
        assert_eq!(rail.views(None)[0].status, DestinationStatus::Ready);
        rail.set_destinations(vec![destination("devbox", "other-host")]);
        assert_eq!(rail.views(None)[0].status, DestinationStatus::Disconnected);
    }
}
