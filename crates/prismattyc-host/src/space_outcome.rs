//! Phase 1 open feedback. Session facts come from bounded daemon snapshots.

use std::collections::HashMap;

use prismattyc_mux::{SavedSpace, Snapshot};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct LiveSession {
    id: u64,
    windows: usize,
    panes: usize,
    running_panes: usize,
}

pub fn sessions(snapshot: Snapshot) -> HashMap<String, LiveSession> {
    snapshot
        .sessions
        .into_iter()
        .map(|session| {
            let panes = session.windows.iter().flat_map(|window| &window.panes);
            let running_panes = panes
                .clone()
                .filter(|pane| pane.child_pid.is_some())
                .count();
            (
                session.name,
                LiveSession {
                    id: session.id,
                    windows: session.windows.len(),
                    panes: panes.count(),
                    running_panes,
                },
            )
        })
        .collect()
}

pub struct Observation {
    saved: Vec<(String, usize)>,
    before: Option<HashMap<String, LiveSession>>,
}

impl Observation {
    pub fn capture(name: &str) -> Self {
        let saved = prismattyc_mux::load_space(&prismattyc_mux::spaces_dir(), name).ok();
        Self::new(
            saved.as_ref(),
            crate::attach_log::live_snapshot().map(sessions),
        )
    }

    fn new(saved: Option<&SavedSpace>, before: Option<HashMap<String, LiveSession>>) -> Self {
        let saved = saved
            .map(|space| {
                space
                    .sessions
                    .iter()
                    .map(|session| {
                        (
                            session.name.clone(),
                            session
                                .windows
                                .iter()
                                .map(|window| window.root.pane_count())
                                .sum(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { saved, before }
    }

    pub fn finish(
        self,
        name: String,
        view: View,
        error: Option<String>,
        after: Option<HashMap<String, LiveSession>>,
    ) -> Report {
        let seats = self
            .saved
            .into_iter()
            .map(|(name, saved_panes)| {
                let live = after
                    .as_ref()
                    .and_then(|sessions| sessions.get(&name))
                    .cloned();
                let state = seat_state(self.before.as_ref(), after.as_ref(), &name);
                Seat {
                    name,
                    state,
                    saved_panes,
                    live,
                }
            })
            .collect();
        Report {
            name,
            view,
            error,
            sequence: 0,
            mode: "unknown",
            seats,
            launch: "not observed",
            session_basis: "before and after daemon snapshots",
            target: if view == View::NewWindowRequested {
                "new window"
            } else {
                "this window"
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Applied,
    Partial,
    NotApplied,
    NewWindowRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatState {
    Created,
    Reused,
    Unavailable,
    Unknown,
}

fn seat_state(
    before: Option<&HashMap<String, LiveSession>>,
    after: Option<&HashMap<String, LiveSession>>,
    name: &str,
) -> SeatState {
    let Some(after) = after else {
        return SeatState::Unknown;
    };
    let Some(live) = after.get(name) else {
        return SeatState::Unavailable;
    };
    if live.running_panes == 0 {
        return SeatState::Unavailable;
    }
    let Some(before) = before else {
        return SeatState::Unknown;
    };
    match before.get(name) {
        Some(old) if old.id == live.id => SeatState::Reused,
        _ => SeatState::Created,
    }
}

#[derive(Debug, Serialize)]
pub struct Seat {
    name: String,
    state: SeatState,
    saved_panes: usize,
    live: Option<LiveSession>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub name: String,
    pub sequence: u64,
    pub mode: &'static str,
    pub view: View,
    pub target: &'static str,
    pub error: Option<String>,
    seats: Vec<Seat>,
    /// A live PTY does not prove that a saved command or an agent is ready.
    launch: &'static str,
    session_basis: &'static str,
}

impl Report {
    pub fn label(&self) -> String {
        if let Some(error) = &self.error {
            let view = match self.view {
                View::Applied => "view applied; helper failed",
                View::Partial => "view incomplete; no current Space",
                View::NotApplied => "requested view not applied",
                View::NewWindowRequested => "new window failed",
            };
            return format!("{}: {view}: {error}", self.name);
        }
        if self.view == View::NewWindowRequested {
            return format!(
                "{}: new window requested; view not confirmed here",
                self.name
            );
        }
        let count = |state| self.seats.iter().filter(|seat| seat.state == state).count();
        let mut details = Vec::new();
        for (state, label) in [
            (SeatState::Created, "new"),
            (SeatState::Reused, "reused"),
            (SeatState::Unavailable, "unavailable"),
            (SeatState::Unknown, "unknown"),
        ] {
            let count = count(state);
            if count > 0 {
                let unit = if count == 1 { "session" } else { "sessions" };
                let retained = if state == SeatState::Reused {
                    " (live layouts retained)"
                } else {
                    ""
                };
                details.push(format!("{count} {label} {unit}{retained}"));
            }
        }
        let view = format!("{}: view applied", self.name);
        if details.is_empty() {
            view
        } else {
            format!("{view}; {}", details.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: u64, panes: usize, running_panes: usize) -> HashMap<String, LiveSession> {
        HashMap::from([(
            "worker".into(),
            LiveSession {
                id,
                panes,
                running_panes,
                windows: 1,
            },
        )])
    }

    #[test]
    fn retained_three_pane_session_is_not_reported_as_saved_two_pane_layout() {
        let observation = Observation {
            saved: vec![("worker".into(), 2)],
            before: Some(live(7, 3, 3)),
        };
        let report = observation.finish("team".into(), View::Applied, None, Some(live(7, 3, 3)));
        assert_eq!(report.seats[0].state, SeatState::Reused);
        assert_eq!(report.seats[0].saved_panes, 2);
        assert_eq!(report.seats[0].live.as_ref().unwrap().panes, 3);
        assert!(report
            .label()
            .contains("1 reused session (live layouts retained)"));
        assert_eq!(
            serde_json::to_value(report).unwrap()["launch"],
            "not observed"
        );
    }

    #[test]
    fn missing_exited_new_rebound_and_unknown_sessions_stay_distinct() {
        let cases = [
            (
                Some(live(7, 3, 3)),
                Some(HashMap::new()),
                SeatState::Unavailable,
            ),
            (
                Some(live(7, 3, 3)),
                Some(live(7, 3, 0)),
                SeatState::Unavailable,
            ),
            (
                Some(HashMap::new()),
                Some(live(7, 1, 1)),
                SeatState::Created,
            ),
            (Some(live(7, 3, 3)), Some(live(8, 2, 2)), SeatState::Created),
            (None, Some(live(7, 3, 3)), SeatState::Unknown),
            (Some(live(7, 3, 3)), None, SeatState::Unknown),
        ];
        for (before, after, expected) in cases {
            assert_eq!(
                seat_state(before.as_ref(), after.as_ref(), "worker"),
                expected
            );
        }
    }

    #[test]
    fn failed_helper_keeps_seat_changes_separate_from_view_result() {
        for (view, message) in [
            (View::NotApplied, "requested view not applied"),
            (View::Partial, "view incomplete; no current Space"),
            (View::Applied, "view applied; helper failed"),
            (View::NewWindowRequested, "new window failed"),
        ] {
            let observation = Observation {
                saved: vec![("worker".into(), 2)],
                before: Some(HashMap::new()),
            };
            let report = observation.finish(
                "team".into(),
                view,
                Some("apply timeout".into()),
                Some(live(7, 2, 2)),
            );
            assert_eq!(report.seats[0].state, SeatState::Created);
            assert!(report.label().contains(message));
            assert!(report.label().contains("apply timeout"));
        }
    }

    #[test]
    fn applied_view_discloses_unavailable_seats_and_keeps_result_after_toast() {
        let observation = Observation {
            saved: vec![("worker".into(), 2)],
            before: Some(live(7, 2, 2)),
        };
        let report = observation.finish("team".into(), View::Applied, None, Some(HashMap::new()));
        assert!(report.label().contains("1 unavailable"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["view"], "applied");
        assert_eq!(json["seats"][0]["state"], "unavailable");
        assert!(!report.label().contains("ready"));
    }
}
