// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use lunatui_core::{ClusterTable, Grid, Rect, Style};
use lunatui_runtime::{is_quit, render_once, run, App, ControlFlow, RunOptions};
use lunatui_term::Event;
use lunatui_theme::{Theme, Token, Variant};
use lunatui_widgets::{Block, Paragraph, RenderCx, Widget};
use prismattyc_mux::activity::{exchange, ActivityCursor, ActivityPage, ActivityRecord};
use prismattyc_mux::mailbox::AgentId;
use prismattyc_mux::{ControlRequest, ControlResponseData, PROTOCOL_VERSION};

mod query;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 24;
const MAX_SEATS: usize = 128;
const STALE_MS: u64 = 30_000;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const USAGE: &str = "usage: pmux-activity-monitor --socket PATH [--snapshot | --json | --seats | --recent SEAT | --tail [--seat SEAT]]";

enum Mode {
    Live,
    Snapshot,
    Json,
    Seats,
    Recent(AgentId),
    Tail(Option<AgentId>),
}

struct Options {
    socket: PathBuf,
    mode: Mode,
}

fn options() -> Result<Option<Options>> {
    let mut args = env::args_os().skip(1);
    let mut socket = None;
    let mut mode = None;
    let mut seat = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--socket") if socket.is_none() => {
                socket = Some(PathBuf::from(
                    args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?,
                ));
            }
            Some("--snapshot") if mode.is_none() => mode = Some(Mode::Snapshot),
            Some("--json") if mode.is_none() => mode = Some(Mode::Json),
            Some("--seats") if mode.is_none() => mode = Some(Mode::Seats),
            Some("--recent") if mode.is_none() => {
                mode = Some(Mode::Recent(parse_seat(args.next())?));
            }
            Some("--tail") if mode.is_none() => mode = Some(Mode::Tail(None)),
            Some("--seat") if seat.is_none() => seat = Some(parse_seat(args.next())?),
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return Ok(None);
            }
            _ => bail!(USAGE),
        }
    }
    let mode = match (mode, seat) {
        (Some(Mode::Tail(_)), seat) => Mode::Tail(seat),
        (mode, None) => mode.unwrap_or(Mode::Live),
        _ => bail!("--seat requires --tail"),
    };
    Ok(Some(Options {
        socket: socket.ok_or_else(|| anyhow::anyhow!(USAGE))?,
        mode,
    }))
}

fn parse_seat(raw: Option<std::ffi::OsString>) -> Result<AgentId> {
    let raw = raw
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| anyhow::anyhow!("a valid seat identifier is required"))?;
    AgentId::new(raw).map_err(|_| anyhow::anyhow!("a valid seat identifier is required"))
}

fn read_page(socket: &Path, cursor: Option<ActivityCursor>) -> Result<ActivityPage> {
    match exchange(
        socket,
        ControlRequest::ReadActivity {
            version: PROTOCOL_VERSION,
            request_id: 1,
            cursor,
            limit: 256,
        },
    )? {
        ControlResponseData::Activity { page } => Ok(page),
        _ => bail!("unexpected activity response"),
    }
}

enum Update {
    Page(ActivityPage),
    Disconnected,
}

struct Feed {
    updates: mpsc::Receiver<Update>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Feed {
    fn start(socket: PathBuf) -> Self {
        let (sender, updates) = mpsc::sync_channel(4);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            let mut cursor = None;
            while !worker_stop.load(Ordering::Relaxed) {
                let (update, next, has_more) = match read_page(&socket, cursor.clone()) {
                    Ok(page) => {
                        let next = page.cursor.clone();
                        let has_more = page.has_more;
                        (Update::Page(page), Some(next), has_more)
                    }
                    Err(_) => (Update::Disconnected, None, false),
                };
                if !deliver(&sender, update, &worker_stop) {
                    break;
                }
                if let Some(next) = next {
                    cursor = Some(next);
                }
                if !has_more {
                    thread::sleep(POLL_INTERVAL);
                }
            }
        });
        Self {
            updates,
            stopped,
            worker: Some(worker),
        }
    }
}

fn deliver(sender: &mpsc::SyncSender<Update>, mut update: Update, stopped: &AtomicBool) -> bool {
    while !stopped.load(Ordering::Relaxed) {
        match sender.try_send(update) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(pending)) => {
                update = pending;
                thread::sleep(Duration::from_millis(20));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
    false
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct ViewState {
    seats: BTreeMap<AgentId, ActivityRecord>,
    connected: bool,
    sequence: u64,
    reset: String,
    more: bool,
    evicted: u64,
}

impl ViewState {
    fn apply(&mut self, page: ActivityPage) {
        if let Some(reset) = page.reset.as_ref() {
            self.seats.clear();
            self.evicted = 0;
            self.reset = label(reset);
        }
        self.connected = true;
        self.sequence = page.cursor.sequence;
        self.more = page.has_more;
        for record in page.records {
            if !self.seats.contains_key(&record.event.seat) && self.seats.len() == MAX_SEATS {
                let oldest = self
                    .seats
                    .iter()
                    .min_by_key(|(_, event)| event.sequence)
                    .map(|(seat, _)| seat.clone());
                if let Some(oldest) = oldest {
                    self.seats.remove(&oldest);
                    self.evicted += 1;
                }
            }
            self.seats.insert(record.event.seat.clone(), record);
        }
    }

    fn lines(&self, now: u64, rows: usize) -> String {
        let mut lines = vec![format!(
            "{} | seats {} / {MAX_SEATS} | cursor {} | {}",
            if self.connected {
                "connected"
            } else {
                "disconnected (retrying)"
            },
            self.seats.len(),
            self.sequence,
            if self.more {
                "history catch-up pending"
            } else {
                "caught up"
            }
        )];
        if !self.connected {
            lines.push("Cached activity may be stale. Waiting for the local feed.".to_owned());
        }
        if !self.reset.is_empty() {
            lines.push(format!(
                "Feed reset: {}. Cache cleared before replay.",
                self.reset
            ));
        }
        if self.evicted > 0 {
            lines.push(format!(
                "{} older seat entries evicted from this view.",
                self.evicted
            ));
        }
        lines.push(
            "SEAT                           SOURCE    ACTIVITY                AGE        STATE"
                .to_owned(),
        );
        if self.seats.is_empty() {
            lines.push("No activity received.".to_owned());
        }
        let available = rows.saturating_sub(lines.len() + 1);
        for record in self.seats.values().take(available) {
            let age = now.saturating_sub(record.received_at_ms);
            lines.push(format!(
                "{:<30} {:<9} {:<23} {:>6}s    {}",
                record.event.seat.as_str(),
                label(&record.event.source),
                label(&record.event.kind),
                age / 1000,
                if age >= STALE_MS || !self.connected {
                    "stale"
                } else {
                    "recent"
                }
            ));
        }
        if self.seats.len() > available {
            lines.push(format!(
                "{} additional seats outside this viewport.",
                self.seats.len() - available
            ));
        }
        lines.join("\n")
    }
}

fn label(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

struct Monitor {
    state: ViewState,
    feed: Option<Feed>,
}

impl App for Monitor {
    fn update(&mut self, event: &Event) -> ControlFlow {
        if is_quit(event) {
            ControlFlow::Break
        } else {
            ControlFlow::Continue
        }
    }

    fn needs_animation_frames(&self) -> bool {
        self.feed.is_some()
    }

    fn render(&mut self, grid: &mut Grid, area: Rect, clusters: &mut ClusterTable) {
        if let Some(feed) = self.feed.as_ref() {
            while let Ok(update) = feed.updates.try_recv() {
                match update {
                    Update::Page(page) => self.state.apply(page),
                    Update::Disconnected => self.state.connected = false,
                }
            }
        }
        let theme = Theme::builtin(Variant::Dark);
        let mut cx = RenderCx::new(grid, clusters, &theme);
        let style = Style::new()
            .fg(cx.token(Token::TextPrimary))
            .bg(cx.token(Token::Surface0));
        let block = Block::bordered()
            .title(" Activity monitor | q quit ")
            .style(style);
        block.render(area, &mut cx);
        let inner = block.inner(area);
        Paragraph::new(self.state.lines(now_ms(), usize::from(inner.height)))
            .style(style)
            .render(inner, &mut cx);
    }
}

fn main() -> Result<()> {
    let Some(options) = options()? else {
        return Ok(());
    };
    match options.mode {
        Mode::Live => run(
            Monitor {
                state: ViewState::default(),
                feed: Some(Feed::start(options.socket)),
            },
            RunOptions::new(WIDTH, HEIGHT).pixels(false),
        )?,
        Mode::Snapshot | Mode::Json => {
            let page = read_page(&options.socket, None)
                .map_err(|_| anyhow::anyhow!("activity feed unavailable"))?;
            if matches!(options.mode, Mode::Json) {
                serde_json::to_writer_pretty(io::stdout().lock(), &page)?;
                println!();
            } else {
                let mut monitor = Monitor {
                    state: ViewState::default(),
                    feed: None,
                };
                monitor.state.apply(page);
                let (grid, clusters) = render_once(&mut monitor, WIDTH, HEIGHT);
                io::stdout().write_all(grid.display_text(&clusters).as_bytes())?;
            }
        }
        Mode::Seats => {
            let observed = query::seats(&options.socket)?;
            serde_json::to_writer_pretty(io::stdout().lock(), &observed)?;
            println!();
        }
        Mode::Recent(seat) => {
            let recent = query::recent(&options.socket, &seat)?;
            serde_json::to_writer_pretty(io::stdout().lock(), &recent)?;
            println!();
        }
        Mode::Tail(seat) => query::tail(&options.socket, seat.as_ref())?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn record(seat: &str, sequence: u64, kind: &str) -> Result<ActivityRecord> {
        Ok(serde_json::from_value(json!({
            "sequence": sequence,
            "received_at_ms": 1000,
            "event": {
                "schema_version": 1,
                "seat": seat,
                "run_id": "run-1",
                "event_id": format!("event-{sequence}"),
                "source": "runtime",
                "kind": kind
            }
        }))?)
    }

    pub(super) fn page(records: Vec<ActivityRecord>, reset: Option<&str>) -> Result<ActivityPage> {
        let sequence = records.last().map_or(0, |record| record.sequence);
        Ok(serde_json::from_value(json!({
            "epoch": "epoch-a",
            "oldest_sequence": 1,
            "current_sequence": sequence,
            "records": records,
            "cursor": {"epoch": "epoch-a", "sequence": sequence},
            "has_more": false,
            "reset": reset
        }))?)
    }

    #[test]
    fn latest_event_replaces_the_previous_activity_for_a_seat() -> Result<()> {
        let mut state = ViewState::default();
        state.apply(page(vec![record("worker-a", 1, "working")?], None)?);
        state.apply(page(vec![record("worker-a", 2, "waiting")?], None)?);
        assert_eq!(state.seats.len(), 1);
        assert_eq!(
            label(&state.seats[&AgentId::new("worker-a")?].event.kind),
            "waiting"
        );
        assert_eq!(state.sequence, 2);
        Ok(())
    }

    #[test]
    fn restart_replay_removes_seats_from_the_previous_daemon() -> Result<()> {
        let mut state = ViewState::default();
        state.apply(page(vec![record("worker-a", 10, "working")?], None)?);
        state.apply(page(
            vec![record("worker-b", 1, "waiting")?],
            Some("restarted"),
        )?);
        assert_eq!(
            state.seats.keys().map(AgentId::as_str).collect::<Vec<_>>(),
            vec!["worker-b"]
        );
        assert_eq!(state.sequence, 1);
        assert!(state.lines(1000, 22).contains("Feed reset: restarted"));
        Ok(())
    }

    #[test]
    fn retention_gap_discards_activity_that_cannot_be_replayed() -> Result<()> {
        let mut state = ViewState::default();
        state.apply(page(vec![record("worker-a", 1, "working")?], None)?);
        state.apply(page(Vec::new(), Some("retention_gap"))?);
        assert!(state.seats.is_empty());
        assert!(state.lines(1000, 22).contains("No activity received."));
        Ok(())
    }

    #[test]
    fn one_hundred_twenty_ninth_seat_evicts_the_oldest_activity() -> Result<()> {
        let records = (0..129)
            .map(|index| record(&format!("worker-{index:03}"), index + 1, "working"))
            .collect::<Result<Vec<_>>>()?;
        let mut state = ViewState::default();
        state.apply(page(records, None)?);
        assert_eq!(state.seats.len(), 128);
        assert!(!state.seats.contains_key(&AgentId::new("worker-000")?));
        assert!(state.seats.contains_key(&AgentId::new("worker-128")?));
        assert_eq!(state.evicted, 1);
        Ok(())
    }

    #[test]
    fn elapsed_receive_time_marks_a_seat_stale_without_another_event() -> Result<()> {
        let mut state = ViewState::default();
        state.apply(page(vec![record("worker-a", 1, "working")?], None)?);
        assert!(state.lines(30_999, 22).contains("recent"));
        assert!(state.lines(31_000, 22).contains("stale"));
        Ok(())
    }

    #[test]
    fn disconnect_is_visible_and_a_successful_page_restores_connected_state() -> Result<()> {
        let mut state = ViewState::default();
        assert!(state.lines(1000, 22).contains("disconnected (retrying)"));
        state.apply(page(vec![record("worker-a", 1, "working")?], None)?);
        assert!(state.lines(1000, 22).starts_with("connected |"));
        Ok(())
    }

    #[test]
    fn snapshot_paints_the_seat_and_event_through_the_terminal_grid() -> Result<()> {
        let mut monitor = Monitor {
            state: ViewState::default(),
            feed: None,
        };
        monitor
            .state
            .apply(page(vec![record("worker-a", 1, "tool_started")?], None)?);
        let (grid, clusters) = render_once(&mut monitor, WIDTH, HEIGHT);
        let text = grid.display_text(&clusters);
        assert!(text.contains("Activity monitor | q quit"));
        assert!(text.contains("worker-a"));
        assert!(text.contains("tool_started"));
        assert!(text.contains("connected | seats 1 / 128 | cursor 1"));
        Ok(())
    }
}
