//! Tell a dead pmux daemon apart from a quiet agent mailbox.
//!
//! `pmux mail watch` and MCP agents stay connected for a long time. No
//! letter arriving is the normal case and must not be reported as stalled
//! or dead. A daemon that exits or crashes while that connection is open
//! is a different event: the wait ends immediately and the report names
//! the exit.

use std::io;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::control::{ControlResponse, ControlResponseBody, ControlResponseData};

/// How often a connected mail waiter rechecks a quiet daemon.
pub const MAIL_WATCH_SLICE: Duration = Duration::from_millis(200);

/// Extra time a live daemon gets to deliver the MailWait reply after the
/// caller's budget. A dead daemon does not use this grace.
pub const MAIL_WATCH_GRACE: Duration = Duration::from_secs(1);

/// Who was connected when the daemon exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectedRole {
    Client,
    Agent,
}

/// One observation from the connected mail socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailObservation {
    /// The daemon answered. `open == 0` is a quiet mailbox.
    Depth { open: u32, held: u32 },
    /// The socket closed, reset, or the peer vanished.
    LinkClosed,
    /// The daemon sent the exit report on this connection.
    DaemonReported,
    /// A bounded read expired and the socket is still open.
    ReadTimedOut,
    /// The reply was not a mail result.
    Failed(String),
}

/// Whether the daemon this connection was using is still that process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonPresence {
    Running,
    Exited,
}

/// What a connected waiter should do with one observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailVerdict {
    Arrived {
        open: u32,
        held: u32,
    },
    /// The daemon is alive and the mailbox had nothing new.
    Quiet {
        open: u32,
        held: u32,
    },
    DaemonExited {
        report: String,
    },
    Failed {
        report: String,
    },
    Continue,
}

/// Exit status and text for a finished mail watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailWatchPresented {
    pub code: i32,
    pub stdout: String,
    pub stderr: Option<String>,
}

/// Elapsed time since a wait started. Tests advance [`ManualClock`];
/// production uses [`SystemClock`].
pub trait MailClock {
    fn now(&self) -> Duration;
}

/// Wall clock anchored at construction.
#[derive(Debug)]
pub struct SystemClock(Instant);

impl SystemClock {
    #[must_use]
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl MailClock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Clock whose elapsed time changes only when a test says so.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<Duration>,
}

impl ManualClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: Mutex::new(Duration::ZERO),
        }
    }

    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.now);
        *now = now.saturating_add(by);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MailClock for ManualClock {
    fn now(&self) -> Duration {
        *lock(&self.now)
    }
}

fn lock(mutex: &Mutex<Duration>) -> MutexGuard<'_, Duration> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sentence printed when the daemon exits under a live connection.
#[must_use]
pub fn daemon_exit_report(role: ConnectedRole) -> String {
    match role {
        ConnectedRole::Client => "pmuxd exited while this client was connected".to_string(),
        ConnectedRole::Agent => "pmuxd exited while this agent was connected".to_string(),
    }
}

#[must_use]
pub fn is_daemon_exit_report(message: &str) -> bool {
    message.contains("pmuxd exited while this ")
}

/// Map a transport error for an agent that was already connected.
/// A read timeout is not an exit: the mailbox may simply be quiet.
#[must_use]
pub fn agent_transport_error(error: &io::Error) -> String {
    if transport_link_lost(error) {
        daemon_exit_report(ConnectedRole::Agent)
    } else {
        error.to_string()
    }
}

#[must_use]
pub fn transport_link_lost(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotConnected
    )
}

/// Parse one control reply into a mail observation.
#[must_use]
pub fn observation_from_reply(line: &str, awaited: u64) -> MailObservation {
    let parsed = match serde_json::from_str::<ControlResponse>(line.trim()) {
        Ok(response) => response,
        Err(error) => {
            return MailObservation::Failed(format!("unreadable mail reply: {error}"));
        }
    };
    if parsed.request_id != awaited && parsed.request_id != 0 {
        return MailObservation::Failed(format!(
            "mail reply id {} does not match {awaited}",
            parsed.request_id
        ));
    }
    match parsed.body {
        ControlResponseBody::Ok {
            response: ControlResponseData::MailDepth { open, held },
        } => MailObservation::Depth { open, held },
        ControlResponseBody::Error { error } if is_daemon_exit_report(&error.message) => {
            MailObservation::DaemonReported
        }
        ControlResponseBody::Error { error } => MailObservation::Failed(error.to_string()),
        ControlResponseBody::Ok { response } => {
            MailObservation::Failed(format!("unexpected mail reply: {response:?}"))
        }
    }
}

/// Classify one step. A real mailbox reply wins over a presence probe.
/// Silence while the daemon is running stays quiet, including when the
/// quiet period is longer than the wait. A dead daemon or a closed link
/// ends the wait immediately.
#[must_use]
pub fn judge_mail_wait(
    observation: MailObservation,
    daemon: DaemonPresence,
    elapsed: Duration,
    budget: Duration,
    role: ConnectedRole,
) -> MailVerdict {
    match observation {
        MailObservation::LinkClosed | MailObservation::DaemonReported => {
            MailVerdict::DaemonExited {
                report: daemon_exit_report(role),
            }
        }
        MailObservation::Failed(report) => MailVerdict::Failed { report },
        MailObservation::Depth { open, held } if open > 0 => MailVerdict::Arrived { open, held },
        MailObservation::Depth { open, held } => MailVerdict::Quiet { open, held },
        MailObservation::ReadTimedOut => match daemon {
            DaemonPresence::Exited => MailVerdict::DaemonExited {
                report: daemon_exit_report(role),
            },
            DaemonPresence::Running if elapsed >= budget => MailVerdict::Quiet { open: 0, held: 0 },
            DaemonPresence::Running => MailVerdict::Continue,
        },
    }
}

/// Drive a mail wait until it finishes.
///
/// `poll` is invoked with the next read bound. It must block on a live
/// socket; [`SystemClock`] then moves. A [`ManualClock`] moves only when
/// the test advances it. A read timeout that does not move the clock is
/// treated as no progress and ends in a failure instead of spinning.
pub fn drive_mail_watch(
    budget: Duration,
    grace: Duration,
    slice: Duration,
    clock: &impl MailClock,
    mut poll: impl FnMut(Duration) -> (MailObservation, DaemonPresence),
) -> MailVerdict {
    let origin = clock.now();
    let quiet_at = budget.saturating_add(grace);
    let mut spins = 0u32;
    loop {
        let elapsed = clock.now().saturating_sub(origin);
        let remaining = quiet_at.saturating_sub(elapsed);
        let wait = if remaining.is_zero() {
            Duration::ZERO
        } else {
            slice.min(remaining)
        };
        let before = clock.now();
        let (observation, presence) = poll(wait);
        let after = clock.now();
        if matches!(observation, MailObservation::ReadTimedOut) && after == before {
            spins = spins.saturating_add(1);
            if spins > 8 {
                return MailVerdict::Failed {
                    report: "mail wait made no progress".to_string(),
                };
            }
        } else {
            spins = 0;
        }
        let elapsed = after.saturating_sub(origin);
        match judge_mail_wait(
            observation,
            presence,
            elapsed,
            quiet_at,
            ConnectedRole::Client,
        ) {
            MailVerdict::Continue => {
                if remaining.is_zero() && spins == 0 {
                    return judge_mail_wait(
                        MailObservation::ReadTimedOut,
                        DaemonPresence::Running,
                        quiet_at,
                        quiet_at,
                        ConnectedRole::Client,
                    );
                }
            }
            done => return done,
        }
    }
}

#[must_use]
pub fn present_mail_watch(verdict: &MailVerdict) -> MailWatchPresented {
    match verdict {
        MailVerdict::Arrived { open, held } => MailWatchPresented {
            code: 0,
            stdout: format!("open: {open} held: {held}"),
            stderr: None,
        },
        MailVerdict::Quiet { open, held } => MailWatchPresented {
            code: 1,
            stdout: format!("open: {open} held: {held}"),
            stderr: None,
        },
        MailVerdict::DaemonExited { report } => MailWatchPresented {
            code: 3,
            stdout: String::new(),
            stderr: Some(report.clone()),
        },
        MailVerdict::Failed { report } => MailWatchPresented {
            code: 2,
            stdout: String::new(),
            stderr: Some(report.clone()),
        },
        MailVerdict::Continue => MailWatchPresented {
            code: 2,
            stdout: String::new(),
            stderr: Some("mail wait ended without a result".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet_words(text: &str) {
        let lower = text.to_ascii_lowercase();
        assert!(
            !lower.contains("stalled") && !lower.contains("dead"),
            "quiet mail must not be called stalled or dead: {text}"
        );
    }

    #[test]
    fn a_closed_link_is_daemon_exit_before_the_budget() {
        let verdict = judge_mail_wait(
            MailObservation::LinkClosed,
            DaemonPresence::Running,
            Duration::ZERO,
            Duration::from_secs(300),
            ConnectedRole::Client,
        );
        let MailVerdict::DaemonExited { report } = verdict else {
            panic!("expected exit, got {verdict:?}");
        };
        assert_eq!(report, daemon_exit_report(ConnectedRole::Client));
        let presented = present_mail_watch(&MailVerdict::DaemonExited {
            report: report.clone(),
        });
        assert_eq!(presented.code, 3);
        assert_eq!(presented.stderr.as_deref(), Some(report.as_str()));
        assert!(presented.stdout.is_empty());
    }

    #[test]
    fn a_read_timeout_while_the_daemon_is_gone_does_not_wait_out_the_budget() {
        let clock = ManualClock::new();
        let polls = std::cell::Cell::new(0u32);
        let verdict = drive_mail_watch(
            Duration::from_secs(3600),
            Duration::ZERO,
            Duration::from_secs(30),
            &clock,
            |_wait| {
                polls.set(polls.get() + 1);
                (MailObservation::ReadTimedOut, DaemonPresence::Exited)
            },
        );
        assert_eq!(
            polls.get(),
            1,
            "a dead daemon must not be polled until the budget"
        );
        let MailVerdict::DaemonExited { report } = verdict else {
            panic!("expected exit, got {verdict:?}");
        };
        assert_eq!(report, "pmuxd exited while this client was connected");
        assert_eq!(
            present_mail_watch(&MailVerdict::DaemonExited { report }).code,
            3
        );
    }

    #[test]
    fn a_long_quiet_mailbox_stays_quiet_while_the_daemon_runs() {
        let clock = ManualClock::new();
        let polls = std::cell::Cell::new(0u32);
        let verdict = drive_mail_watch(
            Duration::from_secs(300),
            Duration::ZERO,
            Duration::from_secs(60),
            &clock,
            |_wait| {
                polls.set(polls.get() + 1);
                // An hour of silence in one step, and the daemon is still up.
                clock.advance(Duration::from_secs(3600));
                (MailObservation::ReadTimedOut, DaemonPresence::Running)
            },
        );
        assert_eq!(polls.get(), 1);
        let MailVerdict::Quiet { open: 0, held: 0 } = verdict else {
            panic!("silence must stay quiet, got {verdict:?}");
        };
        let presented = present_mail_watch(&verdict);
        assert_eq!(presented.code, 1);
        assert_eq!(presented.stdout, "open: 0 held: 0");
        assert!(presented.stderr.is_none());
        quiet_words(&presented.stdout);
    }

    #[test]
    fn quiet_slices_do_not_become_a_failure_before_the_budget() {
        let clock = ManualClock::new();
        let polls = std::cell::Cell::new(0u32);
        let verdict = drive_mail_watch(
            Duration::from_secs(300),
            Duration::ZERO,
            Duration::from_secs(60),
            &clock,
            |_wait| {
                let n = polls.get();
                polls.set(n + 1);
                clock.advance(Duration::from_secs(60));
                if n < 4 {
                    (MailObservation::ReadTimedOut, DaemonPresence::Running)
                } else {
                    (
                        MailObservation::Depth { open: 0, held: 2 },
                        DaemonPresence::Running,
                    )
                }
            },
        );
        assert_eq!(polls.get(), 5, "four quiet minutes must keep waiting");
        let MailVerdict::Quiet { open: 0, held: 2 } = verdict else {
            panic!("a finished quiet wait must report depth, got {verdict:?}");
        };
        let presented = present_mail_watch(&verdict);
        assert_eq!(presented.code, 1);
        quiet_words(&presented.stdout);
        assert!(!presented.stdout.contains("exited"));
    }

    #[test]
    fn an_early_timeout_is_not_an_exit_or_a_quiet_finish() {
        let verdict = judge_mail_wait(
            MailObservation::ReadTimedOut,
            DaemonPresence::Running,
            Duration::from_secs(10),
            Duration::from_secs(300),
            ConnectedRole::Agent,
        );
        assert_eq!(verdict, MailVerdict::Continue);
    }

    #[test]
    fn a_mailbox_reply_is_not_overridden_by_a_stale_exit_probe() {
        let verdict = judge_mail_wait(
            MailObservation::Depth { open: 0, held: 0 },
            DaemonPresence::Exited,
            Duration::ZERO,
            Duration::from_secs(300),
            ConnectedRole::Client,
        );
        let MailVerdict::Quiet { open: 0, held: 0 } = verdict else {
            panic!("a delivered quiet reply must stay quiet, got {verdict:?}");
        };
        quiet_words(&present_mail_watch(&verdict).stdout);
    }

    #[test]
    fn mail_arriving_during_a_quiet_period_is_not_an_exit() {
        let verdict = judge_mail_wait(
            MailObservation::Depth { open: 2, held: 1 },
            DaemonPresence::Running,
            Duration::from_secs(299),
            Duration::from_secs(300),
            ConnectedRole::Client,
        );
        let presented = present_mail_watch(&verdict);
        assert_eq!(presented.code, 0);
        assert_eq!(presented.stdout, "open: 2 held: 1");
        quiet_words(&presented.stdout);
    }

    #[test]
    fn a_clock_that_does_not_move_fails_instead_of_hanging() {
        let clock = ManualClock::new();
        let polls = std::cell::Cell::new(0u32);
        let verdict = drive_mail_watch(
            Duration::from_secs(3600),
            Duration::ZERO,
            Duration::from_secs(30),
            &clock,
            |_wait| {
                polls.set(polls.get() + 1);
                (MailObservation::ReadTimedOut, DaemonPresence::Running)
            },
        );
        assert!(polls.get() <= 9, "spun {} times", polls.get());
        let MailVerdict::Failed { report } = verdict else {
            panic!("expected a finished failure, got {verdict:?}");
        };
        assert_eq!(report, "mail wait made no progress");
        quiet_words(&report);
        assert!(!report.contains("exited"));
        assert_eq!(present_mail_watch(&MailVerdict::Failed { report }).code, 2);
    }

    #[test]
    fn agent_reset_is_an_exit_and_a_timeout_is_not() {
        let reset = io::Error::new(io::ErrorKind::ConnectionReset, "reset by peer");
        assert_eq!(
            agent_transport_error(&reset),
            daemon_exit_report(ConnectedRole::Agent)
        );
        let timed_out = io::Error::new(io::ErrorKind::TimedOut, "mailbox still quiet");
        let text = agent_transport_error(&timed_out);
        assert_eq!(text, "mailbox still quiet");
        quiet_words(&text);
        assert!(!is_daemon_exit_report(&text));
    }

    #[test]
    fn reply_lines_keep_exit_and_quiet_apart() {
        let quiet = r#"{"version":1,"request_id":7,"status":"ok","response":{"kind":"mail_depth","open":0,"held":0}}"#;
        assert_eq!(
            observation_from_reply(quiet, 7),
            MailObservation::Depth { open: 0, held: 0 }
        );
        let exit = r#"{"version":1,"request_id":7,"status":"error","error":{"code":"internal","message":"pmuxd exited while this client was connected"}}"#;
        assert_eq!(
            observation_from_reply(exit, 7),
            MailObservation::DaemonReported
        );
        let wrong_id = observation_from_reply(quiet, 8);
        let MailObservation::Failed(report) = wrong_id else {
            panic!("a mismatched reply must not become quiet mail: {wrong_id:?}");
        };
        quiet_words(&report);
        assert_eq!(present_mail_watch(&MailVerdict::Failed { report }).code, 2);
    }
}
