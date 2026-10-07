// SPDX-License-Identifier: MPL-2.0
//! Fixed-size timing and wait counters for the windowed host pump.

use std::cell::Cell;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Phase {
    RestartPoll,
    ConfigReload,
    PollHostAttachTabs,
    AdvanceSpaceOpens,
    RefreshSpaceViews,
    PersistAndRestore,
    ApplyPendingSessionFocus,
    RetryRegisterHostPid,
    RemoteRailPoll,
    SpaceRailPoll,
    SyncRemoteRail,
    AdoptNestedAttaches,
    InferCurrentSpace,
    DrainPty,
    WindowBookkeeping,
    PublishRenderStatus,
    PersistAttachLayoutFromLive,
}

const PHASES: [Phase; 17] = [
    Phase::RestartPoll,
    Phase::ConfigReload,
    Phase::PollHostAttachTabs,
    Phase::AdvanceSpaceOpens,
    Phase::RefreshSpaceViews,
    Phase::PersistAndRestore,
    Phase::ApplyPendingSessionFocus,
    Phase::RetryRegisterHostPid,
    Phase::RemoteRailPoll,
    Phase::SpaceRailPoll,
    Phase::SyncRemoteRail,
    Phase::AdoptNestedAttaches,
    Phase::InferCurrentSpace,
    Phase::DrainPty,
    Phase::WindowBookkeeping,
    Phase::PublishRenderStatus,
    Phase::PersistAttachLayoutFromLive,
];

const PHASE_COUNT: usize = PHASES.len();

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::RestartPoll => "restart::poll",
            Self::ConfigReload => "poll_config_reload",
            Self::PollHostAttachTabs => "poll_host_attach_tabs",
            Self::AdvanceSpaceOpens => "advance_space_opens",
            Self::RefreshSpaceViews => "refresh_space_views",
            Self::PersistAndRestore => "local_views::persist_and_restore",
            Self::ApplyPendingSessionFocus => "apply_pending_session_focus",
            Self::RetryRegisterHostPid => "retry_register_host_pid",
            Self::RemoteRailPoll => "remote_rail.poll",
            Self::SpaceRailPoll => "space_rail.poll",
            Self::SyncRemoteRail => "sync_remote_rail",
            Self::AdoptNestedAttaches => "adopt_nested_attaches",
            Self::InferCurrentSpace => "infer_current_space",
            Self::DrainPty => "drain_pty",
            Self::WindowBookkeeping => "window_bookkeeping",
            Self::PublishRenderStatus => "publish_render_status",
            Self::PersistAttachLayoutFromLive => "persist_attach_layout_from_live",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PumpSummary {
    pub(crate) total_us: u64,
    pub(crate) slowest_phase: &'static str,
    pub(crate) slowest_us: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PhaseTimes {
    last_us: u64,
    max_1s_us: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PhaseSample {
    last_us: u64,
    max_1s_us: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct IoCounts {
    count: u64,
    time_us: u64,
}

impl IoCounts {
    fn add(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.time_us = self.time_us.saturating_add(other.time_us);
    }

    fn record(&mut self, time_us: u64) {
        self.count = self.count.saturating_add(1);
        self.time_us = self.time_us.saturating_add(time_us);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PumpIoCounts {
    sockets: IoCounts,
    subprocesses: IoCounts,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PumpSnapshot {
    pub(crate) summary: PumpSummary,
    max_total_1s_us: u64,
    phases: [PhaseSample; PHASE_COUNT],
    last_io: PumpIoCounts,
    window_io: PumpIoCounts,
}

#[derive(Debug, Default)]
pub(crate) struct PumpTiming {
    phases: [PhaseTimes; PHASE_COUNT],
    current_phase_us: [u64; PHASE_COUNT],
    current_phase_seen: [bool; PHASE_COUNT],
    pending_phase_us: [u64; PHASE_COUNT],
    pending_phase_seen: [bool; PHASE_COUNT],
    pending_external_total_us: u64,
    last_total_us: u64,
    max_total_1s_us: u64,
    current_slowest: Option<(Phase, u64)>,
    last_slowest: Option<(Phase, u64)>,
    last_io: PumpIoCounts,
    window_io: PumpIoCounts,
    window_started: Option<Instant>,
}

impl PumpTiming {
    /// Carry work done in `about_to_wait` into the next complete pump sample.
    pub(crate) fn record_external(&mut self, phase: Phase, elapsed: Duration) {
        let index = phase as usize;
        let elapsed_us = duration_us(elapsed);
        self.pending_phase_us[index] = self.pending_phase_us[index].saturating_add(elapsed_us);
        self.pending_phase_seen[index] = true;
        self.pending_external_total_us = self.pending_external_total_us.saturating_add(elapsed_us);
    }

    pub(crate) fn begin_pump(&mut self, now: Instant) -> u64 {
        if self.window_started.is_none() {
            self.window_started = Some(now);
        }
        let external_us = std::mem::take(&mut self.pending_external_total_us);
        self.current_phase_us = self.pending_phase_us;
        self.current_phase_seen = self.pending_phase_seen;
        self.pending_phase_us = [0; PHASE_COUNT];
        self.pending_phase_seen = [false; PHASE_COUNT];
        self.current_slowest = None;
        external_us
    }

    pub(crate) fn record_phase(&mut self, phase: Phase, elapsed: Duration) {
        let index = phase as usize;
        self.current_phase_us[index] =
            self.current_phase_us[index].saturating_add(duration_us(elapsed));
        self.current_phase_seen[index] = true;
    }

    pub(crate) fn finish_pump(&mut self, total_us: u64, io: PumpIoCounts, now: Instant) {
        self.roll_window(now);
        self.last_total_us = total_us;
        self.max_total_1s_us = self.max_total_1s_us.max(total_us);
        for phase in PHASES {
            let index = phase as usize;
            if self.current_phase_seen[index] {
                let elapsed_us = self.current_phase_us[index];
                self.phases[index].last_us = elapsed_us;
                self.phases[index].max_1s_us = self.phases[index].max_1s_us.max(elapsed_us);
                if self
                    .current_slowest
                    .is_none_or(|(_, slowest_us)| elapsed_us > slowest_us)
                {
                    self.current_slowest = Some((phase, elapsed_us));
                }
            }
        }
        self.last_slowest = self.current_slowest;
        self.last_io = io;
        self.window_io.sockets.add(io.sockets);
        self.window_io.subprocesses.add(io.subprocesses);
    }

    pub(crate) fn summary(&self) -> PumpSummary {
        let snapshot = self.snapshot();
        snapshot.summary
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        let snapshot = self.snapshot();
        let mut phases = serde_json::Map::with_capacity(PHASE_COUNT);
        for phase in PHASES {
            let sample = snapshot.phases[phase as usize];
            phases.insert(
                phase.name().to_owned(),
                serde_json::json!({
                    "last_us": sample.last_us,
                    "max_1s_us": sample.max_1s_us,
                }),
            );
        }
        serde_json::json!({
            "total_us": snapshot.summary.total_us,
            "max_1s_us": snapshot.max_total_1s_us,
            "slowest_phase": snapshot.summary.slowest_phase,
            "slowest_us": snapshot.summary.slowest_us,
            "phases": phases,
            "socket_round_trips": io_json(snapshot.last_io.sockets, snapshot.window_io.sockets),
            "subprocess_waits": io_json(snapshot.last_io.subprocesses, snapshot.window_io.subprocesses),
        })
    }

    fn snapshot(&self) -> PumpSnapshot {
        let mut phases = [PhaseSample::default(); PHASE_COUNT];
        for phase in PHASES {
            let index = phase as usize;
            phases[index] = PhaseSample {
                last_us: self.phases[index].last_us,
                max_1s_us: self.phases[index].max_1s_us,
            };
        }
        PumpSnapshot {
            summary: PumpSummary {
                total_us: self.last_total_us,
                slowest_phase: self.last_slowest.map_or("-", |(phase, _)| phase.name()),
                slowest_us: self.last_slowest.map_or(0, |(_, elapsed_us)| elapsed_us),
            },
            max_total_1s_us: self.max_total_1s_us,
            phases,
            last_io: self.last_io,
            window_io: self.window_io,
        }
    }

    fn roll_window(&mut self, now: Instant) {
        if self
            .window_started
            .is_some_and(|started| now.duration_since(started) < Duration::from_secs(1))
        {
            return;
        }
        self.window_started = Some(now);
        self.max_total_1s_us = 0;
        self.window_io = PumpIoCounts::default();
        for phase in &mut self.phases {
            phase.max_1s_us = 0;
        }
    }
}

fn io_json(last_pump: IoCounts, last_second: IoCounts) -> serde_json::Value {
    serde_json::json!({
        "last_pump_count": last_pump.count,
        "last_pump_us": last_pump.time_us,
        "count_1s": last_second.count,
        "time_1s_us": last_second.time_us,
    })
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy)]
enum IoKind {
    Socket,
    Subprocess,
}

thread_local! {
    static PUMP_IO_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static PUMP_IO_COUNTS: Cell<PumpIoCounts> = const { Cell::new(PumpIoCounts {
        sockets: IoCounts { count: 0, time_us: 0 },
        subprocesses: IoCounts { count: 0, time_us: 0 },
    }) };
}

pub(crate) struct PumpIoScope {
    active: bool,
}

impl PumpIoScope {
    pub(crate) fn begin() -> Self {
        PUMP_IO_COUNTS.with(|counts| counts.set(PumpIoCounts::default()));
        PUMP_IO_ACTIVE.with(|active| active.set(true));
        Self { active: true }
    }

    pub(crate) fn finish(mut self) -> PumpIoCounts {
        self.active = false;
        PUMP_IO_ACTIVE.with(|active| active.set(false));
        PUMP_IO_COUNTS.with(Cell::take)
    }
}

impl Drop for PumpIoScope {
    fn drop(&mut self) {
        if self.active {
            PUMP_IO_ACTIVE.with(|active| active.set(false));
            PUMP_IO_COUNTS.with(|counts| counts.set(PumpIoCounts::default()));
        }
    }
}

pub(crate) struct PumpIoTimer {
    kind: IoKind,
    started: Option<Instant>,
}

impl PumpIoTimer {
    pub(crate) fn socket_round_trip() -> Self {
        Self::new(IoKind::Socket)
    }

    pub(crate) fn subprocess_wait() -> Self {
        Self::new(IoKind::Subprocess)
    }

    fn new(kind: IoKind) -> Self {
        Self {
            kind,
            started: PUMP_IO_ACTIVE.with(Cell::get).then(Instant::now),
        }
    }
}

pub(crate) fn measure_subprocess_wait<T>(wait: impl FnOnce() -> T) -> T {
    let _timer = PumpIoTimer::subprocess_wait();
    wait()
}

impl Drop for PumpIoTimer {
    fn drop(&mut self) {
        let Some(started) = self.started else { return };
        let elapsed_us = duration_us(started.elapsed());
        PUMP_IO_COUNTS.with(|stored| {
            let mut counts = stored.get();
            match self.kind {
                IoKind::Socket => counts.sockets.record(elapsed_us),
                IoKind::Subprocess => counts.subprocesses.record(elapsed_us),
            }
            stored.set(counts);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_samples_accumulate_per_pump_and_reset_the_one_second_max() {
        let now = Instant::now();
        let mut timing = PumpTiming::default();

        timing.begin_pump(now);
        timing.record_phase(Phase::DrainPty, Duration::from_micros(7));
        timing.record_phase(Phase::DrainPty, Duration::from_micros(8));
        timing.finish_pump(30, PumpIoCounts::default(), now + Duration::from_millis(10));

        timing.begin_pump(now + Duration::from_millis(200));
        timing.record_phase(Phase::DrainPty, Duration::from_micros(9));
        timing.finish_pump(
            25,
            PumpIoCounts::default(),
            now + Duration::from_millis(210),
        );

        let within_window = timing.json();
        assert_eq!(within_window["phases"]["drain_pty"]["last_us"], 9);
        assert_eq!(within_window["phases"]["drain_pty"]["max_1s_us"], 15);
        assert_eq!(within_window["total_us"], 25);
        assert_eq!(within_window["max_1s_us"], 30);

        timing.begin_pump(now + Duration::from_secs(2));
        timing.record_phase(Phase::DrainPty, Duration::from_micros(5));
        timing.finish_pump(12, PumpIoCounts::default(), now + Duration::from_secs(2));

        let next_window = timing.json();
        assert_eq!(next_window["phases"]["drain_pty"]["last_us"], 5);
        assert_eq!(next_window["phases"]["drain_pty"]["max_1s_us"], 5);
        assert_eq!(next_window["max_1s_us"], 12);
    }

    #[test]
    fn pump_io_timers_count_socket_round_trips_and_process_waits() {
        let scope = PumpIoScope::begin();
        {
            let _socket = PumpIoTimer::socket_round_trip();
        }
        {
            let _process = PumpIoTimer::subprocess_wait();
        }
        let io = scope.finish();
        assert_eq!(io.sockets.count, 1);
        assert_eq!(io.subprocesses.count, 1);
    }

    #[test]
    fn status_schema_names_all_pump_phases_and_wait_counters() {
        const EXPECTED_PHASES: &[&str] = &[
            "restart::poll",
            "poll_config_reload",
            "poll_host_attach_tabs",
            "advance_space_opens",
            "refresh_space_views",
            "local_views::persist_and_restore",
            "apply_pending_session_focus",
            "retry_register_host_pid",
            "remote_rail.poll",
            "space_rail.poll",
            "sync_remote_rail",
            "adopt_nested_attaches",
            "infer_current_space",
            "drain_pty",
            "window_bookkeeping",
            "publish_render_status",
            "persist_attach_layout_from_live",
        ];
        let now = Instant::now();
        let mut timing = PumpTiming::default();
        timing.begin_pump(now);
        timing.record_phase(Phase::RestartPoll, Duration::from_micros(14));
        timing.record_phase(Phase::DrainPty, Duration::from_micros(29));
        assert_eq!(timing.json()["total_us"], 0, "status uses completed pumps");
        timing.finish_pump(
            60,
            PumpIoCounts {
                sockets: IoCounts {
                    count: 2,
                    time_us: 31,
                },
                subprocesses: IoCounts {
                    count: 3,
                    time_us: 47,
                },
            },
            now + Duration::from_micros(60),
        );

        let status = timing.json();
        assert_eq!(status["slowest_phase"], "drain_pty");
        assert_eq!(status["slowest_us"], 29);
        assert_eq!(status["phases"]["restart::poll"]["last_us"], 14);
        let phases = status["phases"].as_object().expect("phase map");
        assert_eq!(phases.len(), EXPECTED_PHASES.len());
        for phase in EXPECTED_PHASES {
            assert!(phases.contains_key(*phase), "missing phase {phase}");
            assert!(phases[*phase].get("last_us").is_some());
            assert!(phases[*phase].get("max_1s_us").is_some());
        }
        assert_eq!(
            status["phases"]["persist_attach_layout_from_live"]["max_1s_us"],
            0
        );
        assert_eq!(status["socket_round_trips"]["last_pump_count"], 2);
        assert_eq!(status["socket_round_trips"]["last_pump_us"], 31);
        assert_eq!(status["socket_round_trips"]["count_1s"], 2);
        assert_eq!(status["socket_round_trips"]["time_1s_us"], 31);
        assert_eq!(status["subprocess_waits"]["last_pump_count"], 3);
        assert_eq!(status["subprocess_waits"]["last_pump_us"], 47);
        assert_eq!(status["subprocess_waits"]["count_1s"], 3);
        assert_eq!(status["subprocess_waits"]["time_1s_us"], 47);
    }
}
