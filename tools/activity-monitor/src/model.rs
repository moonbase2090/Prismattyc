// SPDX-License-Identifier: MIT

//! Viewer state for one or more activity feeds. Pure data: no I/O and no terminal.

use std::collections::{BTreeMap, VecDeque};

use prismattyc_mux::activity::{ActivityKind, ActivityPage, ActivityRecord};
use prismattyc_mux::mailbox::AgentId;

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
    Failed,
    Stopped,
}

impl SeatState {
    pub const ALL: [SeatState; 4] = [Self::Working, Self::Waiting, Self::Failed, Self::Stopped];

    pub fn word(self) -> &'static str {
        match self {
            Self::Unknown => "no state",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn glyph(self) -> char {
        match self {
            Self::Unknown => '·',
            Self::Working => '●',
            Self::Waiting => '◆',
            Self::Failed => '✕',
            Self::Stopped => '■',
        }
    }

    /// Lower sorts first when sorting by state: the seats that need a person lead.
    pub fn severity(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Waiting => 1,
            Self::Working => 2,
            Self::Stopped => 3,
            Self::Unknown => 4,
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
}

impl Seat {
    pub fn latest(&self) -> &ActivityRecord {
        // A seat is only created with one record and history never drains to empty.
        self.history
            .back()
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
            self.insert(record, local_ms);
        }
        self.sequence = self.sequence.max(page.cursor.sequence);
    }

    fn insert(&mut self, record: ActivityRecord, local_ms: u64) {
        let seat_id = record.event.seat.clone();
        if !self.seats.contains_key(&seat_id) && self.seats.len() == MAX_SEATS {
            let oldest = self
                .seats
                .iter()
                .min_by_key(|(_, seat)| seat.latest().sequence)
                .map(|(seat, _)| seat.clone());
            if let Some(oldest) = oldest {
                self.seats.remove(&oldest);
                self.evicted += 1;
            }
        }
        let seat = self.seats.entry(seat_id).or_insert_with(|| Seat {
            history: VecDeque::new(),
            state: SeatState::Unknown,
            state_since_ms: record.received_at_ms,
            changed_at: None,
        });
        let state = next_state(seat.state, record.event.kind);
        if state != seat.state {
            // The first state of a newly seen seat is not a transition worth a fade.
            if seat.state != SeatState::Unknown {
                seat.changed_at = Some(local_ms);
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
    !host.connected() || age_ms(seat, now_ms) >= STALE_MS
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
}
