// SPDX-License-Identifier: MIT

//! Viewer state for one or more activity feeds. Pure data: no I/O and no terminal.

use std::collections::{BTreeMap, HashMap, VecDeque};

use prismattyc_core::activity::{
    ActivityKind, ActivityPage, ActivityRecord, ActivitySessions, ActivitySource,
};
use prismattyc_core::AgentId;

/// Seats kept per host before the seat with the oldest event is evicted.
pub const MAX_SEATS: usize = 128;
/// Events kept per seat for the timeline and lanes.
pub const HISTORY: usize = 256;
/// A seat with no event for this long is shown as stale.
pub const STALE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SeatState {
    /// No state-bearing event retained yet (for example, only mail).
    Unknown,
    Working,
    Waiting,
    Idle,
    Stuck,
    Failed,
    Stopped,
}

impl SeatState {
    pub const ALL: [SeatState; 6] = [
        Self::Working,
        Self::Waiting,
        Self::Idle,
        Self::Stuck,
        Self::Failed,
        Self::Stopped,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Self::Unknown => "no state",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Stuck => "stuck",
            Self::Failed => "failed",
            Self::Stopped => "down",
        }
    }

    pub fn glyph(self) -> char {
        match self {
            Self::Unknown => '·',
            Self::Working => '●',
            Self::Waiting => '◆',
            Self::Idle => '○',
            Self::Stuck => '!',
            Self::Failed => '✕',
            Self::Stopped => '■',
        }
    }

    /// Lower sorts first when sorting by state: the seats that need a person lead.
    pub fn severity(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Waiting => 1,
            Self::Stuck => 0,
            Self::Working => 2,
            Self::Idle => 3,
            Self::Stopped => 4,
            Self::Unknown => 5,
        }
    }
}

/// The state a seat is in after `kind`, given the state it was in before.
pub fn next_state(previous: SeatState, kind: ActivityKind) -> SeatState {
    use ActivityKind::*;
    match kind {
        MailSent | MailReceived => previous,
        // A new session waits for its first prompt.
        Waiting | TurnFinished | SessionStarted => SeatState::Waiting,
        Stopped | SessionFinished => SeatState::Stopped,
        ToolFailed | CommandFailed | EditFailed | TestFailed => SeatState::Failed,
        Working | TurnStarted | ToolStarted | ToolFinished | CommandStarted | CommandFinished
        | EditStarted | EditFinished | TestStarted | TestPassed => SeatState::Working,
    }
}

/// Snake-case schema name of a serializable enum, as the daemon spells it.
pub fn label(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

pub struct Seat {
    pub history: VecDeque<ActivityRecord>,
    pub state: SeatState,
    /// Receive time of the event that entered the current state.
    pub state_since_ms: u64,
    /// Local monotonic time (ms) of the last state change, for the row fade.
    pub changed_at: Option<u64>,
    /// Who the seat is, for the details header.
    pub info: SeatInfo,
    discovery: Option<(u64, ActivityRecord)>,
}

/// Seat facts for the details header. Each stays `None` (shown as unknown)
/// until pmux reports it without ambiguity; nothing here is guessed from
/// prompts, paths or command text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeatInfo {
    /// The agent harness running in the seat, such as `claude` or `codex`.
    pub harness: Option<String>,
    pub model: Option<String>,
    pub quota: Option<String>,
    /// Repository name the seat works in, never a path.
    pub project: Option<String>,
}

impl Seat {
    pub fn latest(&self) -> &ActivityRecord {
        self.history
            .back()
            .or_else(|| self.discovery.as_ref().map(|(_, record)| record))
            .unwrap_or_else(|| unreachable!("seat without history"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// No page received yet.
    Connecting,
    Connected,
    /// The endpoint failed; `since_ms` is when the outage began (wall clock).
    Retrying {
        since_ms: u64,
    },
    /// The daemon answered but the feed is off.
    Disabled,
    /// The daemon answered but predates the activity feed.
    Unsupported,
    PermissionDenied,
}

pub struct Host {
    pub label: String,
    pub link: Link,
    pub epoch: Option<String>,
    pub sequence: u64,
    pub more: bool,
    pub reset: Option<String>,
    pub evicted: u64,
    pub seats: BTreeMap<AgentId, Seat>,
}

impl Host {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            link: Link::Connecting,
            epoch: None,
            sequence: 0,
            more: false,
            reset: None,
            evicted: 0,
            seats: BTreeMap::new(),
        }
    }

    pub fn connected(&self) -> bool {
        self.link == Link::Connected
    }

    /// Applies one page. `local_ms` is the viewer's monotonic clock, used only for motion.
    pub fn apply(&mut self, page: ActivityPage, local_ms: u64) {
        let new_epoch = self.epoch.as_deref() != Some(page.epoch.as_str());
        // Replayed history (a new lifetime, a reset, or a catch-up page) is not live change,
        // so it does not fade. Only pages that follow a caught-up page do.
        let live = !new_epoch && page.reset.is_none() && !self.more;
        if let Some(reset) = page.reset.as_ref() {
            self.seats.clear();
            self.evicted = 0;
            self.reset = Some(label(reset));
        } else if new_epoch && self.epoch.is_some() {
            // A replaying command source can start a new daemon lifetime without a reset.
            self.seats.clear();
            self.evicted = 0;
            self.reset = Some("restarted".to_owned());
        }
        if new_epoch || page.reset.is_some() {
            self.sequence = 0;
        }
        self.epoch = Some(page.epoch.clone());
        self.link = Link::Connected;
        self.more = page.has_more;
        for record in page.records {
            // Records at or below the applied sequence are replays of the same lifetime.
            if record.sequence <= self.sequence {
                continue;
            }
            self.sequence = record.sequence;
            self.insert(record, live.then_some(local_ms));
        }
        self.sequence = self.sequence.max(page.cursor.sequence);
        if let Some(sessions) = page.sessions {
            self.discover(sessions, live.then_some(local_ms));
        }
    }

    fn discover(&mut self, sessions: ActivitySessions, fade_at: Option<u64>) {
        let current = sessions
            .seats
            .iter()
            .map(|session| session.seat.clone())
            .collect::<std::collections::BTreeSet<_>>();
        self.seats.retain(|id, seat| {
            if !current.contains(id) && seat.discovery.take().is_some() {
                seat.state = SeatState::Stopped;
                seat.state_since_ms = sessions.observed_at_ms;
                seat.changed_at = fade_at;
            }
            current.contains(id) || !seat.history.is_empty()
        });
        for session in sessions.seats {
            let observation = session.observation(sessions.observed_at_ms);
            let seat = self.seats.entry(session.seat).or_insert_with(|| Seat {
                history: VecDeque::new(),
                state: SeatState::Unknown,
                state_since_ms: sessions.observed_at_ms,
                changed_at: None,
                info: SeatInfo::default(),
                discovery: None,
            });
            let replaced = seat
                .discovery
                .as_ref()
                .is_some_and(|(id, _)| *id != session.session_id);
            if replaced {
                seat.history.clear();
            }
            let hooked = seat
                .history
                .iter()
                .rev()
                .find(|record| {
                    record.event.source != ActivitySource::Runtime
                        && !matches!(
                            record.event.kind,
                            ActivityKind::MailSent | ActivityKind::MailReceived
                        )
                })
                .is_some_and(|record| {
                    !matches!(
                        record.event.kind,
                        ActivityKind::Stopped | ActivityKind::SessionFinished
                    )
                });
            if !hooked
                || matches!(
                    session.state,
                    prismattyc_core::activity::ActivitySessionState::Idle
                        | prismattyc_core::activity::ActivitySessionState::Stuck
                        | prismattyc_core::activity::ActivitySessionState::Stopped
                )
            {
                let state = match session.state {
                    prismattyc_core::activity::ActivitySessionState::Idle => SeatState::Idle,
                    prismattyc_core::activity::ActivitySessionState::Stuck => SeatState::Stuck,
                    prismattyc_core::activity::ActivitySessionState::Stopped => SeatState::Stopped,
                    prismattyc_core::activity::ActivitySessionState::Working => SeatState::Working,
                    prismattyc_core::activity::ActivitySessionState::Waiting => SeatState::Waiting,
                };
                if state != seat.state {
                    if seat.state != SeatState::Unknown {
                        seat.changed_at = fade_at;
                    }
                    seat.state = state;
                    seat.state_since_ms = sessions.observed_at_ms;
                }
            }
            seat.discovery = Some((session.session_id, observation));
        }
    }

    /// `fade_at` is the viewer time for a fade, or `None` for replayed history.
    fn insert(&mut self, record: ActivityRecord, fade_at: Option<u64>) {
        let seat_id = record.event.seat.clone();
        if !self.seats.contains_key(&seat_id) && self.seats.len() >= MAX_SEATS {
            let oldest = self
                .seats
                .iter()
                .filter(|(_, seat)| seat.discovery.is_none())
                .min_by_key(|(_, seat)| seat.latest().sequence)
                .map(|(seat, _)| seat.clone());
            if let Some(oldest) = oldest {
                self.seats.remove(&oldest);
                self.evicted += 1;
            } else {
                return;
            }
        }
        let seat = self.seats.entry(seat_id).or_insert_with(|| Seat {
            history: VecDeque::new(),
            state: SeatState::Unknown,
            state_since_ms: record.received_at_ms,
            changed_at: None,
            info: SeatInfo::default(),
            discovery: None,
        });
        let state = next_state(seat.state, record.event.kind);
        if state != seat.state {
            // The first state of a newly seen seat is not a transition worth a fade.
            if seat.state != SeatState::Unknown {
                seat.changed_at = fade_at.or(seat.changed_at);
            }
            seat.state = state;
            seat.state_since_ms = record.received_at_ms;
        }
        if seat.history.len() == HISTORY {
            seat.history.pop_front();
        }
        seat.history.push_back(record);
    }

    pub fn disconnect(&mut self, now_ms: u64) {
        if !matches!(self.link, Link::Retrying { .. }) {
            self.link = Link::Retrying { since_ms: now_ms };
        }
    }
}

pub fn is_stale(host: &Host, seat: &Seat, now_ms: u64) -> bool {
    let observed = seat
        .discovery
        .as_ref()
        .map_or(seat.latest().received_at_ms, |(_, record)| {
            record.received_at_ms
        });
    !host.connected() || now_ms.saturating_sub(observed) >= STALE_MS
}

pub fn age_ms(seat: &Seat, now_ms: u64) -> u64 {
    now_ms.saturating_sub(seat.latest().received_at_ms)
}

/// One lane bin: how many events landed in it and the state at its end.
/// `None` state means the bin is older than anything retained for the seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bin {
    pub events: u32,
    pub state: Option<SeatState>,
}

/// Splits `[now - window, now)` into `bins` equal bins, oldest first.
pub fn lane(
    history: &VecDeque<ActivityRecord>,
    now_ms: u64,
    window_ms: u64,
    bins: usize,
) -> Vec<Bin> {
    let mut out = vec![
        Bin {
            events: 0,
            state: None,
        };
        bins
    ];
    if bins == 0 || window_ms == 0 {
        return out;
    }
    let start = now_ms.saturating_sub(window_ms);
    let span = (window_ms / bins as u64).max(1);
    let bin_of =
        |at: u64| -> usize { (at.saturating_sub(start) / span).min(bins as u64 - 1) as usize };
    let mut state = SeatState::Unknown;
    let mut known = false;
    let mut cursor = 0usize;
    for record in history {
        if record.received_at_ms >= start {
            let index = bin_of(record.received_at_ms);
            // Bins between the previous event and this one keep the previous state.
            while cursor < index {
                out[cursor].state = known.then_some(state);
                cursor += 1;
            }
            state = next_state(state, record.event.kind);
            known = true;
            out[index].events += 1;
            out[index].state = Some(state);
        } else {
            state = next_state(state, record.event.kind);
            known = true;
        }
    }
    // Bins after the last event keep the final state; the last event's own bin is already set.
    while cursor < bins {
        if out[cursor].events == 0 {
            out[cursor].state = known.then_some(state);
        }
        cursor += 1;
    }
    out
}

/// Formats a duration as a compact age: `4s`, `12m`, `3h`, `2d`.
pub fn short_age(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Formats the gap between two events: `+0.4s`, `+12s`, `+3m`.
pub fn short_gap(ms: u64) -> String {
    if ms < 10_000 {
        format!("+{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("+{}", short_age(ms))
    }
}

/// What an operation did, from the family of its event kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Edit,
    Command,
    Test,
    Tool,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Command => "command",
            Self::Test => "test",
            Self::Tool => "tool",
        }
    }
}

/// How an activity ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Started, and nothing has ended it yet.
    Running,
    /// Started, but its turn or session ended without a result.
    NoResult,
    Succeeded,
    Failed,
    /// A seat event rather than an operation: session start, waiting, mail.
    Note(&'static str),
}

/// One line of the details view: an operation from start to result, or a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity<'a> {
    pub action: Option<Action>,
    pub outcome: Outcome,
    pub task: Option<&'a str>,
    pub tool: Option<&'a str>,
    /// The file edited, else the command run (already redacted by pmux).
    pub target: Option<&'a str>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    /// When it last changed: its result, or its start while running.
    pub at_ms: u64,
    started_ms: u64,
    pub run_id: &'a str,
    pub operation_id: Option<&'a str>,
}

/// Recent activities that share a task, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskGroup<'a> {
    /// `None` when no event named a task.
    pub task: Option<&'a str>,
    pub activities: Vec<Activity<'a>>,
}

impl TaskGroup<'_> {
    pub fn failed(&self) -> usize {
        self.activities
            .iter()
            .filter(|activity| activity.outcome == Outcome::Failed)
            .count()
    }

    pub fn last_ms(&self) -> u64 {
        self.activities.first().map_or(0, |activity| activity.at_ms)
    }
}

enum Phase {
    Start,
    Done(bool),
}

fn operation(kind: ActivityKind) -> Option<(Action, Phase)> {
    use ActivityKind::*;
    Some(match kind {
        EditStarted => (Action::Edit, Phase::Start),
        EditFinished => (Action::Edit, Phase::Done(true)),
        EditFailed => (Action::Edit, Phase::Done(false)),
        CommandStarted => (Action::Command, Phase::Start),
        CommandFinished => (Action::Command, Phase::Done(true)),
        CommandFailed => (Action::Command, Phase::Done(false)),
        TestStarted => (Action::Test, Phase::Start),
        TestPassed => (Action::Test, Phase::Done(true)),
        TestFailed => (Action::Test, Phase::Done(false)),
        ToolStarted => (Action::Tool, Phase::Start),
        ToolFinished => (Action::Tool, Phase::Done(true)),
        ToolFailed => (Action::Tool, Phase::Done(false)),
        _ => return None,
    })
}

/// Seat events worth a line of their own. Turn starts and plain `working`
/// pings only restate what the operations already show.
fn note(kind: ActivityKind) -> Option<&'static str> {
    use ActivityKind::*;
    match kind {
        SessionStarted => Some("session started"),
        SessionFinished => Some("session ended"),
        TurnFinished | Waiting => Some("waiting for input"),
        Stopped => Some("stopped"),
        MailSent => Some("sent mail"),
        MailReceived => Some("received mail"),
        _ => None,
    }
}

/// True when an event ends every operation still open in its run.
fn closes_run(kind: ActivityKind) -> bool {
    use ActivityKind::*;
    matches!(kind, TurnFinished | Waiting | Stopped | SessionFinished)
}

/// The seat's retained events as activities grouped by task. Each operation's
/// start and result become one activity, paired by run and operation id (or,
/// without one, by run and kind). Groups are ordered by their latest activity.
pub fn task_groups(seat: &Seat) -> Vec<TaskGroup<'_>> {
    let mut activities: Vec<Activity<'_>> = Vec::new();
    let mut open: HashMap<(&str, Action, Option<&str>), usize> = HashMap::new();
    // The last task named in each run; notes and bare results inherit it.
    let mut run_task: HashMap<&str, &str> = HashMap::new();
    for record in &seat.history {
        let event = &record.event;
        let run = event.run_id.as_str();
        let detail = event.detail.as_ref();
        let task = detail.and_then(|d| d.task.as_deref());
        if let Some(task) = task {
            run_task.insert(run, task);
        }
        let task = task.or_else(|| run_task.get(run).copied());
        let tool = detail.and_then(|d| d.tool.as_deref());
        let target = detail.and_then(|d| d.file.as_deref().or(d.command.as_deref()));
        let operation_id = detail.and_then(|d| d.operation_id.as_deref());
        let at_ms = record.received_at_ms;
        if let Some((action, phase)) = operation(event.kind) {
            let key = (run, action, operation_id);
            match phase {
                Phase::Start => {
                    if let Some(previous) = open.insert(key, activities.len()) {
                        activities[previous].outcome = Outcome::NoResult;
                    }
                    activities.push(Activity {
                        action: Some(action),
                        outcome: Outcome::Running,
                        task,
                        tool,
                        target,
                        duration_ms: None,
                        exit_code: None,
                        at_ms,
                        started_ms: at_ms,
                        run_id: run,
                        operation_id,
                    });
                }
                Phase::Done(ok) => {
                    let outcome = if ok {
                        Outcome::Succeeded
                    } else {
                        Outcome::Failed
                    };
                    let exit_code = detail.and_then(|d| d.exit_code);
                    let duration = detail.and_then(|d| d.duration_ms);
                    if let Some(index) = open.remove(&key) {
                        let activity = &mut activities[index];
                        activity.outcome = outcome;
                        activity.exit_code = exit_code;
                        activity.duration_ms =
                            duration.or(Some(at_ms.saturating_sub(activity.started_ms)));
                        activity.at_ms = at_ms;
                        activity.task = activity.task.or(task);
                        activity.tool = activity.tool.or(tool);
                        activity.target = activity.target.or(target);
                    } else {
                        activities.push(Activity {
                            action: Some(action),
                            outcome,
                            task,
                            tool,
                            target,
                            duration_ms: duration,
                            exit_code,
                            at_ms,
                            started_ms: at_ms,
                            run_id: run,
                            operation_id,
                        });
                    }
                }
            }
            continue;
        }
        if closes_run(event.kind) {
            open.retain(|(open_run, ..), index| {
                if *open_run == run {
                    activities[*index].outcome = Outcome::NoResult;
                    false
                } else {
                    true
                }
            });
        }
        if let Some(text) = note(event.kind) {
            activities.push(Activity {
                action: None,
                outcome: Outcome::Note(text),
                task,
                tool: None,
                target: None,
                duration_ms: None,
                exit_code: None,
                at_ms,
                started_ms: at_ms,
                run_id: run,
                operation_id: None,
            });
        }
    }
    let mut groups: Vec<TaskGroup<'_>> = Vec::new();
    // Newest first, so each group's activities come out newest first too.
    // Reversing first keeps the later of two same-millisecond events on top.
    activities.reverse();
    activities.sort_by_key(|activity| std::cmp::Reverse(activity.at_ms));
    for activity in activities {
        match groups.iter_mut().find(|group| group.task == activity.task) {
            Some(group) => group.activities.push(activity),
            None => groups.push(TaskGroup {
                task: activity.task,
                activities: vec![activity],
            }),
        }
    }
    groups
}

/// Formats how long an operation took: `850ms`, `1.2s`, `34s`, `2m05s`, `1h02m`.
pub fn duration_text(ms: u64) -> String {
    let secs = ms / 1000;
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=9_999 => format!("{}.{}s", secs, (ms % 1000) / 100),
        10_000..=59_999 => format!("{secs}s"),
        60_000..=3_599_999 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::json;

    pub fn record_at(seat: &str, sequence: u64, kind: &str, at: u64) -> Result<ActivityRecord> {
        record_run(seat, sequence, kind, at, "run-1")
    }

    pub fn record_run(
        seat: &str,
        sequence: u64,
        kind: &str,
        at: u64,
        run: &str,
    ) -> Result<ActivityRecord> {
        Ok(serde_json::from_value(json!({
            "sequence": sequence,
            "received_at_ms": at,
            "event": {
                "schema_version": 1,
                "seat": seat,
                "run_id": run,
                "event_id": format!("event-{sequence}"),
                "source": "hooks",
                "kind": kind
            }
        }))?)
    }

    /// A record whose event carries the safe `detail` fields pmux publishes.
    pub fn record_detail(
        seat: &str,
        sequence: u64,
        kind: &str,
        at: u64,
        run: &str,
        detail: serde_json::Value,
    ) -> Result<ActivityRecord> {
        let mut record = record_run(seat, sequence, kind, at, run)?;
        record.event.detail = Some(serde_json::from_value(detail)?);
        Ok(record)
    }

    pub fn page_in(
        epoch: &str,
        records: Vec<ActivityRecord>,
        reset: Option<&str>,
    ) -> Result<ActivityPage> {
        let sequence = records.last().map_or(0, |record| record.sequence);
        Ok(serde_json::from_value(json!({
            "epoch": epoch,
            "oldest_sequence": 1,
            "current_sequence": sequence,
            "records": records,
            "cursor": {"epoch": epoch, "sequence": sequence},
            "has_more": false,
            "reset": reset
        }))?)
    }

    fn seat<'a>(host: &'a Host, id: &str) -> &'a Seat {
        &host.seats[&AgentId::new(id).unwrap()]
    }

    fn discovered(names: &[&str], at: u64) -> prismattyc_core::activity::ActivitySessions {
        use prismattyc_core::activity::{ActivitySession, ActivitySessionState, ActivitySessions};
        ActivitySessions {
            observed_at_ms: at,
            seats: names
                .iter()
                .enumerate()
                .map(|(id, name)| ActivitySession {
                    seat: AgentId::new(*name).unwrap(),
                    session_id: id as u64 + 1,
                    state: ActivitySessionState::Waiting,
                    last_output_at_ms: None,
                    harness: None,
                    model: None,
                    quota: None,
                    project: None,
                })
                .collect(),
        }
    }

    #[test]
    fn discovery_survives_empty_history_retention_reset_and_repoll() -> Result<()> {
        let mut host = Host::new("local");
        let mut first = page_in("e0", vec![], None)?;
        first.sessions = Some(discovered(&["quiet", "unhooked"], 1_000));
        host.apply(first.clone(), 0);
        assert_eq!(host.seats.len(), 2);
        assert_eq!(seat(&host, "quiet").state, SeatState::Waiting);
        assert!(seat(&host, "quiet").history.is_empty());
        assert_eq!(host.sequence, 0);
        first.sessions = Some(discovered(&["quiet", "unhooked"], 60_000));
        first.reset = Some(prismattyc_core::activity::ActivityReset::RetentionGap);
        host.apply(first.clone(), 10);
        assert_eq!(host.seats.len(), 2);
        assert!(!is_stale(&host, seat(&host, "quiet"), 60_001));
        let mut restarted = Host::new("local");
        restarted.apply(first, 0);
        assert_eq!(restarted.seats.len(), 2);
        assert_eq!(restarted.sequence, 0);
        Ok(())
    }

    #[test]
    fn discovery_exposes_idle_stuck_and_down_health() -> Result<()> {
        use prismattyc_core::activity::ActivitySessionState;

        let mut sessions = discovered(&["shell", "agent", "finished"], 10_000);
        sessions.seats[0].state = ActivitySessionState::Idle;
        sessions.seats[1].state = ActivitySessionState::Stuck;
        sessions.seats[2].state = ActivitySessionState::Stopped;
        let mut page = page_in("e0", vec![], None)?;
        page.sessions = Some(sessions);
        let mut host = Host::new("local");
        host.apply(page, 0);

        assert_eq!(seat(&host, "shell").state, SeatState::Idle);
        assert_eq!(seat(&host, "agent").state, SeatState::Stuck);
        assert_eq!(seat(&host, "finished").state, SeatState::Stopped);
        assert_eq!(seat(&host, "finished").state.word(), "down");
        Ok(())
    }

    #[test]
    fn discovery_keeps_hook_state_and_removes_departed_unhooked_seats() -> Result<()> {
        let mut host = Host::new("local");
        let mut page = page_in(
            "e0",
            vec![record_at("hooked", 1, "tool_failed", 1_000)?],
            None,
        )?;
        page.sessions = Some(discovered(&["hooked", "quiet"], 2_000));
        host.apply(page, 0);
        assert_eq!(seat(&host, "hooked").state, SeatState::Failed);
        let mut page = page_in("e0", vec![], None)?;
        page.sessions = Some(discovered(&[], 3_000));
        host.apply(page, 10);
        assert_eq!(host.seats.len(), 1);
        assert_eq!(seat(&host, "hooked").state, SeatState::Stopped);
        assert_eq!(seat(&host, "hooked").history.len(), 1);
        Ok(())
    }

    #[test]
    fn discovery_derives_state_when_history_has_only_mail_or_finished_hooks() -> Result<()> {
        let mut host = Host::new("local");
        let mut page = page_in(
            "e0",
            vec![
                record_at("mail-only", 1, "mail_received", 1000)?,
                record_at("finished", 2, "session_finished", 1000)?,
            ],
            None,
        )?;
        page.sessions = Some(discovered(&["mail-only", "finished"], 2000));
        host.apply(page, 0);
        assert_eq!(seat(&host, "mail-only").state, SeatState::Waiting);
        assert_eq!(seat(&host, "finished").state, SeatState::Waiting);
        Ok(())
    }

    #[test]
    fn discovery_does_not_evict_live_seats_at_the_history_limit() -> Result<()> {
        let names = (0..MAX_SEATS + 2)
            .map(|n| format!("live-{n}"))
            .collect::<Vec<_>>();
        let refs = names.iter().map(String::as_str).collect::<Vec<_>>();
        let mut host = Host::new("local");
        let mut page = page_in("e0", vec![], None)?;
        page.sessions = Some(discovered(&refs, 1_000));
        host.apply(page, 0);
        host.apply(
            page_in(
                "e0",
                vec![record_at("historical", 1, "waiting", 1_100)?],
                None,
            )?,
            0,
        );
        assert_eq!(host.seats.len(), MAX_SEATS + 2);
        assert!(host.seats.contains_key(&AgentId::new("live-0")?));
        Ok(())
    }

    #[test]
    fn every_kind_maps_to_the_documented_state() -> Result<()> {
        let cases = [
            ("working", SeatState::Working),
            ("session_started", SeatState::Waiting),
            ("turn_started", SeatState::Working),
            ("tool_started", SeatState::Working),
            ("tool_finished", SeatState::Working),
            ("command_finished", SeatState::Working),
            ("edit_started", SeatState::Working),
            ("test_passed", SeatState::Working),
            ("waiting", SeatState::Waiting),
            ("turn_finished", SeatState::Waiting),
            ("tool_failed", SeatState::Failed),
            ("command_failed", SeatState::Failed),
            ("edit_failed", SeatState::Failed),
            ("test_failed", SeatState::Failed),
            ("stopped", SeatState::Stopped),
            ("session_finished", SeatState::Stopped),
        ];
        for (kind, expected) in cases {
            let kind: ActivityKind = serde_json::from_value(json!(kind))?;
            assert_eq!(next_state(SeatState::Stopped, kind), expected, "{kind:?}");
        }
        Ok(())
    }

    #[test]
    fn mail_keeps_the_previous_state() -> Result<()> {
        for kind in ["mail_sent", "mail_received"] {
            let kind: ActivityKind = serde_json::from_value(json!(kind))?;
            assert_eq!(next_state(SeatState::Failed, kind), SeatState::Failed);
            assert_eq!(next_state(SeatState::Unknown, kind), SeatState::Unknown);
        }
        Ok(())
    }

    #[test]
    fn a_failure_stays_until_the_seat_reports_something_else() -> Result<()> {
        let mut host = Host::new("mac");
        host.apply(
            page_in(
                "e",
                vec![
                    record_at("sa", 1, "command_started", 1_000)?,
                    record_at("sa", 2, "command_failed", 2_000)?,
                    record_at("sa", 3, "mail_sent", 3_000)?,
                ],
                None,
            )?,
            0,
        );
        assert_eq!(seat(&host, "sa").state, SeatState::Failed);
        assert_eq!(seat(&host, "sa").state_since_ms, 2_000);
        host.apply(
            page_in("e", vec![record_at("sa", 4, "turn_started", 4_000)?], None)?,
            0,
        );
        assert_eq!(seat(&host, "sa").state, SeatState::Working);
        Ok(())
    }

    #[test]
    fn a_state_change_records_the_local_time_for_the_fade_but_the_first_state_does_not(
    ) -> Result<()> {
        let mut host = Host::new("mac");
        host.apply(
            page_in("e", vec![record_at("sa", 1, "working", 1_000)?], None)?,
            50,
        );
        assert_eq!(seat(&host, "sa").changed_at, None);
        host.apply(
            page_in("e", vec![record_at("sa", 2, "tool_started", 2_000)?], None)?,
            60,
        );
        assert_eq!(
            seat(&host, "sa").changed_at,
            None,
            "same state is not a change"
        );
        host.apply(
            page_in("e", vec![record_at("sa", 3, "waiting", 3_000)?], None)?,
            70,
        );
        assert_eq!(seat(&host, "sa").changed_at, Some(70));
        Ok(())
    }

    #[test]
    fn replayed_history_does_not_fade_but_later_live_pages_do() -> Result<()> {
        let mut host = Host::new("mac");
        // The first page of a lifetime is retained history, even when it holds a change.
        let mut first = page_in(
            "e",
            vec![
                record_at("sa", 1, "working", 1_000)?,
                record_at("sa", 2, "waiting", 2_000)?,
            ],
            None,
        )?;
        first.has_more = true;
        host.apply(first, 10);
        assert_eq!(seat(&host, "sa").changed_at, None);
        // A catch-up page after has_more is still history.
        host.apply(
            page_in("e", vec![record_at("sa", 3, "working", 3_000)?], None)?,
            20,
        );
        assert_eq!(seat(&host, "sa").changed_at, None);
        // Once caught up, a change is live.
        host.apply(
            page_in("e", vec![record_at("sa", 4, "stopped", 4_000)?], None)?,
            30,
        );
        assert_eq!(seat(&host, "sa").changed_at, Some(30));
        // A restart replays the new lifetime's history without fading.
        host.apply(
            page_in(
                "e2",
                vec![
                    record_at("sb", 1, "working", 5_000)?,
                    record_at("sb", 2, "tool_failed", 6_000)?,
                ],
                Some("restarted"),
            )?,
            40,
        );
        assert_eq!(seat(&host, "sb").changed_at, None);
        Ok(())
    }

    #[test]
    fn replayed_records_from_the_same_epoch_are_applied_once() -> Result<()> {
        let mut host = Host::new("windows");
        let first = vec![
            record_at("sa", 1, "working", 1_000)?,
            record_at("sa", 2, "waiting", 2_000)?,
        ];
        host.apply(page_in("e", first.clone(), None)?, 0);
        host.apply(page_in("e", first, None)?, 0);
        assert_eq!(seat(&host, "sa").history.len(), 2);
        assert_eq!(seat(&host, "sa").state, SeatState::Waiting);
        Ok(())
    }

    #[test]
    fn a_new_epoch_without_a_reset_still_clears_the_previous_lifetime() -> Result<()> {
        let mut host = Host::new("windows");
        host.apply(
            page_in("e1", vec![record_at("old", 9, "working", 1_000)?], None)?,
            0,
        );
        host.apply(
            page_in("e2", vec![record_at("new", 1, "waiting", 2_000)?], None)?,
            0,
        );
        assert_eq!(
            host.seats.keys().map(AgentId::as_str).collect::<Vec<_>>(),
            vec!["new"]
        );
        assert_eq!(host.reset.as_deref(), Some("restarted"));
        Ok(())
    }

    #[test]
    fn restart_reset_clears_seats_and_reports_the_reason() -> Result<()> {
        let mut host = Host::new("mac");
        host.apply(
            page_in("e1", vec![record_at("sa", 10, "working", 1_000)?], None)?,
            0,
        );
        host.apply(
            page_in(
                "e2",
                vec![record_at("sb", 1, "waiting", 2_000)?],
                Some("restarted"),
            )?,
            0,
        );
        assert_eq!(
            host.seats.keys().map(AgentId::as_str).collect::<Vec<_>>(),
            vec!["sb"]
        );
        assert_eq!(host.sequence, 1);
        assert_eq!(host.reset.as_deref(), Some("restarted"));
        Ok(())
    }

    #[test]
    fn retention_gap_in_the_same_epoch_clears_seats_and_accepts_lower_sequences() -> Result<()> {
        let mut host = Host::new("mac");
        host.apply(
            page_in("e", vec![record_at("sa", 600, "working", 1_000)?], None)?,
            0,
        );
        host.apply(
            page_in(
                "e",
                vec![record_at("sb", 300, "working", 2_000)?],
                Some("retention_gap"),
            )?,
            0,
        );
        assert_eq!(
            host.seats.keys().map(AgentId::as_str).collect::<Vec<_>>(),
            vec!["sb"]
        );
        Ok(())
    }

    #[test]
    fn the_129th_seat_evicts_the_seat_with_the_oldest_event() -> Result<()> {
        let records = (0..129)
            .map(|index| record_at(&format!("worker-{index:03}"), index + 1, "working", 1_000))
            .collect::<Result<Vec<_>>>()?;
        let mut host = Host::new("mac");
        host.apply(page_in("e", records, None)?, 0);
        assert_eq!(host.seats.len(), MAX_SEATS);
        assert!(!host.seats.contains_key(&AgentId::new("worker-000")?));
        assert!(host.seats.contains_key(&AgentId::new("worker-128")?));
        assert_eq!(host.evicted, 1);
        Ok(())
    }

    #[test]
    fn history_keeps_the_newest_events_per_seat() -> Result<()> {
        let records = (1..=(HISTORY as u64 + 5))
            .map(|sequence| record_at("sa", sequence, "tool_started", sequence))
            .collect::<Result<Vec<_>>>()?;
        let mut host = Host::new("mac");
        host.apply(page_in("e", records, None)?, 0);
        let history = &seat(&host, "sa").history;
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history.front().map(|r| r.sequence), Some(6));
        assert_eq!(history.back().map(|r| r.sequence), Some(HISTORY as u64 + 5));
        Ok(())
    }

    #[test]
    fn staleness_follows_receive_age_and_the_host_link() -> Result<()> {
        let mut host = Host::new("mac");
        host.apply(
            page_in("e", vec![record_at("sa", 1, "working", 1_000)?], None)?,
            0,
        );
        assert!(!is_stale(&host, seat(&host, "sa"), 30_999));
        assert!(is_stale(&host, seat(&host, "sa"), 31_000));
        host.disconnect(2_000);
        assert!(is_stale(&host, seat(&host, "sa"), 1_500));
        assert_eq!(host.link, Link::Retrying { since_ms: 2_000 });
        host.disconnect(9_000);
        assert_eq!(
            host.link,
            Link::Retrying { since_ms: 2_000 },
            "outage start is kept"
        );
        Ok(())
    }

    #[test]
    fn lane_bins_count_events_and_carry_state_forward() -> Result<()> {
        // Window 0..10s in 10 bins of 1s each.
        let history: VecDeque<_> = [
            record_at("sa", 1, "turn_started", 1_200)?,
            record_at("sa", 2, "tool_started", 1_700)?,
            record_at("sa", 3, "tool_failed", 4_100)?,
            record_at("sa", 4, "turn_finished", 6_900)?,
        ]
        .into_iter()
        .collect();
        let bins = lane(&history, 10_000, 10_000, 10);
        let counts: Vec<u32> = bins.iter().map(|bin| bin.events).collect();
        assert_eq!(counts, vec![0, 2, 0, 0, 1, 0, 1, 0, 0, 0]);
        let states: Vec<Option<SeatState>> = bins.iter().map(|bin| bin.state).collect();
        use SeatState::*;
        assert_eq!(
            states,
            vec![
                None,
                Some(Working),
                Some(Working),
                Some(Working),
                Some(Failed),
                Some(Failed),
                Some(Waiting),
                Some(Waiting),
                Some(Waiting),
                Some(Waiting),
            ]
        );
        Ok(())
    }

    #[test]
    fn lane_starts_from_the_state_before_the_window() -> Result<()> {
        let history: VecDeque<_> = [record_at("sa", 1, "waiting", 500)?].into_iter().collect();
        let bins = lane(&history, 20_000, 10_000, 4);
        assert!(bins
            .iter()
            .all(|bin| bin.events == 0 && bin.state == Some(SeatState::Waiting)));
        Ok(())
    }

    #[test]
    fn ages_and_gaps_use_compact_units() {
        assert_eq!(short_age(4_900), "4s");
        assert_eq!(short_age(61_000), "1m");
        assert_eq!(short_age(7_200_000), "2h");
        assert_eq!(short_age(90_000_000), "1d");
        assert_eq!(short_gap(400), "+0.4s");
        assert_eq!(short_gap(9_950), "+9.9s");
        assert_eq!(short_gap(12_000), "+12s");
    }

    fn seat_with(records: Vec<ActivityRecord>) -> Result<Host> {
        let mut host = Host::new("mac");
        host.apply(page_in("e1", records, None)?, 0);
        Ok(host)
    }

    #[test]
    fn an_operation_start_and_result_become_one_activity() -> Result<()> {
        let host = seat_with(vec![
            record_detail(
                "builder",
                1,
                "command_started",
                1_000,
                "run-1",
                json!({"tool": "Bash", "command": "cargo test", "task": "PT-111", "operation_id": "op-1"}),
            )?,
            record_detail(
                "builder",
                2,
                "command_failed",
                35_000,
                "run-1",
                json!({"exit_code": 101, "duration_ms": 34_000, "operation_id": "op-1"}),
            )?,
        ])?;
        let groups = task_groups(seat(&host, "builder"));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].task, Some("PT-111"));
        let [activity] = groups[0].activities.as_slice() else {
            panic!("one activity: {groups:?}");
        };
        assert_eq!(activity.action, Some(Action::Command));
        assert_eq!(activity.outcome, Outcome::Failed);
        assert_eq!(activity.tool, Some("Bash"));
        assert_eq!(activity.target, Some("cargo test"));
        assert_eq!(activity.duration_ms, Some(34_000));
        assert_eq!(activity.exit_code, Some(101));
        assert_eq!(activity.at_ms, 35_000);
        assert_eq!(groups[0].failed(), 1);
        Ok(())
    }

    #[test]
    fn a_result_without_a_reported_duration_is_timed_from_its_start() -> Result<()> {
        let host = seat_with(vec![
            record_detail(
                "builder",
                1,
                "edit_started",
                1_000,
                "run-1",
                json!({"file": "ui.rs"}),
            )?,
            record_at("builder", 2, "edit_finished", 2_500)?,
        ])?;
        let groups = task_groups(seat(&host, "builder"));
        let activity = &groups[0].activities[0];
        assert_eq!(activity.outcome, Outcome::Succeeded);
        assert_eq!(activity.target, Some("ui.rs"));
        assert_eq!(activity.duration_ms, Some(1_500));
        Ok(())
    }

    #[test]
    fn open_operations_are_running_until_their_turn_ends() -> Result<()> {
        let host = seat_with(vec![
            record_at("builder", 1, "tool_started", 1_000)?,
            record_run("builder", 2, "test_started", 2_000, "run-2")?,
            record_at("builder", 3, "turn_finished", 3_000)?,
        ])?;
        let groups = task_groups(seat(&host, "builder"));
        let outcomes: Vec<_> = groups[0]
            .activities
            .iter()
            .map(|activity| (activity.action, activity.outcome))
            .collect();
        // Newest first: the turn's note, run-2's test still running, run-1's
        // tool closed by the turn end without a result.
        assert_eq!(
            outcomes,
            vec![
                (None, Outcome::Note("waiting for input")),
                (Some(Action::Test), Outcome::Running),
                (Some(Action::Tool), Outcome::NoResult),
            ]
        );
        Ok(())
    }

    #[test]
    fn activities_group_by_task_with_the_latest_group_first() -> Result<()> {
        let task = |t: &str| json!({ "task": t });
        let host = seat_with(vec![
            record_detail("builder", 1, "edit_finished", 1_000, "run-1", task("PT-1"))?,
            record_detail("builder", 2, "edit_finished", 2_000, "run-2", task("PT-2"))?,
            // Same run as PT-1, no task of its own: it inherits PT-1.
            record_at("builder", 3, "command_finished", 3_000)?,
            record_run("builder", 4, "mail_received", 4_000, "run-3")?,
        ])?;
        let groups = task_groups(seat(&host, "builder"));
        let summary: Vec<_> = groups
            .iter()
            .map(|group| (group.task, group.activities.len()))
            .collect();
        assert_eq!(
            summary,
            vec![(None, 1), (Some("PT-1"), 2), (Some("PT-2"), 1)]
        );
        assert_eq!(groups[1].activities[0].action, Some(Action::Command));
        Ok(())
    }

    #[test]
    fn turn_starts_and_working_pings_add_no_lines() -> Result<()> {
        let host = seat_with(vec![
            record_at("builder", 1, "turn_started", 1_000)?,
            record_at("builder", 2, "working", 2_000)?,
        ])?;
        assert!(task_groups(seat(&host, "builder")).is_empty());
        Ok(())
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(duration_text(850), "850ms");
        assert_eq!(duration_text(1_249), "1.2s");
        assert_eq!(duration_text(34_000), "34s");
        assert_eq!(duration_text(125_000), "2m05s");
        assert_eq!(duration_text(3_720_000), "1h02m");
    }
}
