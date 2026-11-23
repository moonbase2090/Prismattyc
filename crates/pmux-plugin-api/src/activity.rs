// SPDX-License-Identifier: MIT
//! Explicit conversion preserves the existing activity JSON contract in guests.

use crate::guest::pmux::activity::{arrivals, read};
use prismattyc_core::activity as domain;

pub fn page(page: read::Page) -> Result<domain::ActivityPage, read::Error> {
    let records = page
        .records
        .into_iter()
        .map(|record| {
            let event = domain::ActivityInput {
                schema_version: record.event.schema_version,
                seat: prismattyc_core::AgentId::new(record.event.seat)
                    .map_err(|_| read::Error::InvalidRequest)?,
                run_id: record.event.run_id,
                event_id: record.event.event_id,
                source: match record.event.source {
                    read::Source::Notify => domain::ActivitySource::Notify,
                    read::Source::Hooks => domain::ActivitySource::Hooks,
                    read::Source::Runtime => domain::ActivitySource::Runtime,
                },
                kind: kind(record.event.kind),
                detail: record.event.detail.map(|detail| domain::ActivityDetail {
                    tool: detail.tool,
                    file: detail.file,
                    command: detail.command,
                    duration_ms: detail.duration_ms,
                    exit_code: detail.exit_code,
                    task: detail.task,
                    operation_id: detail.operation_id,
                    model: detail.model,
                    message_id: None,
                    room_id: None,
                    sender_id: None,
                    count: None,
                }),
                process_ids: Vec::new(),
            };
            event
                .validate_metadata()
                .map_err(|_| read::Error::InvalidRequest)?;
            Ok(domain::ActivityRecord {
                sequence: record.sequence,
                received_at_ms: record.received_at_ms,
                event,
            })
        })
        .collect::<Result<_, read::Error>>()?;
    Ok(domain::ActivityPage {
        epoch: page.epoch,
        oldest_sequence: page.oldest_sequence,
        current_sequence: page.current_sequence,
        records,
        cursor: domain::ActivityCursor {
            epoch: page.cursor.epoch,
            sequence: page.cursor.sequence,
        },
        has_more: page.has_more,
        sessions: None,
        reset: page.reset.map(|reset| match reset {
            read::Reset::Restarted => domain::ActivityReset::Restarted,
            read::Reset::RetentionGap => domain::ActivityReset::RetentionGap,
            read::Reset::AheadOfFeed => domain::ActivityReset::AheadOfFeed,
        }),
    })
}

fn kind_name(kind: &str) -> Result<domain::ActivityKind, read::Error> {
    use domain::ActivityKind as K;
    Ok(match kind {
        "session_started" => K::SessionStarted,
        "session_finished" => K::SessionFinished,
        "turn_started" => K::TurnStarted,
        "turn_finished" => K::TurnFinished,
        "tool_started" => K::ToolStarted,
        "tool_finished" => K::ToolFinished,
        "tool_failed" => K::ToolFailed,
        "command_started" => K::CommandStarted,
        "command_finished" => K::CommandFinished,
        "command_failed" => K::CommandFailed,
        "edit_started" => K::EditStarted,
        "edit_finished" => K::EditFinished,
        "edit_failed" => K::EditFailed,
        "test_started" => K::TestStarted,
        "test_passed" => K::TestPassed,
        "test_failed" => K::TestFailed,
        "working" => K::Working,
        "waiting" => K::Waiting,
        "stopped" => K::Stopped,
        "mail_sent" => K::MailSent,
        "mail_received" => K::MailReceived,
        "room_message" => K::RoomMessage,
        _ => return Err(read::Error::InvalidRequest),
    })
}

fn kind(kind: read::Kind) -> domain::ActivityKind {
    use domain::ActivityKind as K;
    match kind {
        read::Kind::SessionStarted => K::SessionStarted,
        read::Kind::SessionFinished => K::SessionFinished,
        read::Kind::TurnStarted => K::TurnStarted,
        read::Kind::TurnFinished => K::TurnFinished,
        read::Kind::ToolStarted => K::ToolStarted,
        read::Kind::ToolFinished => K::ToolFinished,
        read::Kind::ToolFailed => K::ToolFailed,
        read::Kind::CommandStarted => K::CommandStarted,
        read::Kind::CommandFinished => K::CommandFinished,
        read::Kind::CommandFailed => K::CommandFailed,
        read::Kind::EditStarted => K::EditStarted,
        read::Kind::EditFinished => K::EditFinished,
        read::Kind::EditFailed => K::EditFailed,
        read::Kind::TestStarted => K::TestStarted,
        read::Kind::TestPassed => K::TestPassed,
        read::Kind::TestFailed => K::TestFailed,
        read::Kind::Working => K::Working,
        read::Kind::Waiting => K::Waiting,
        read::Kind::Stopped => K::Stopped,
        read::Kind::MailSent => K::MailSent,
        read::Kind::MailReceived => K::MailReceived,
    }
}

pub fn poll(
    endpoint: u64,
    after: Option<&read::Cursor>,
    limit: u16,
) -> Result<domain::ActivityPage, read::Error> {
    let mut result = page(read::poll(endpoint, after, limit)?)?;
    result.sessions = session_snapshot(read::sessions(endpoint)?)?;
    Ok(result)
}

pub fn wait(
    endpoint: u64,
    seat: &str,
    kinds: &[String],
    timeout_ms: u32,
    after: Option<&read::Cursor>,
) -> Result<domain::ActivityPage, read::Error> {
    let batch = arrivals::wait(endpoint, seat, kinds, timeout_ms, after)?;
    let records = batch
        .notices
        .into_iter()
        .map(|notice| {
            let kind = kind_name(&notice.kind)?;
            let event = domain::ActivityInput {
                schema_version: notice.schema_version,
                seat: prismattyc_core::AgentId::new(notice.seat)
                    .map_err(|_| read::Error::InvalidRequest)?,
                run_id: notice.run_id,
                event_id: notice.event_id,
                source: match notice.source {
                    arrivals::Source::Notify => domain::ActivitySource::Notify,
                    arrivals::Source::Hooks => domain::ActivitySource::Hooks,
                    arrivals::Source::Runtime => domain::ActivitySource::Runtime,
                },
                kind,
                detail: notice.detail.map(|detail| domain::ActivityDetail {
                    tool: detail.tool,
                    file: detail.file,
                    command: detail.command,
                    duration_ms: detail.duration_ms,
                    exit_code: detail.exit_code,
                    task: detail.task,
                    operation_id: detail.operation_id,
                    model: detail.model,
                    message_id: detail.message_id,
                    room_id: detail.room_id,
                    sender_id: detail.sender_id,
                    count: detail.count,
                }),
                process_ids: Vec::new(),
            };
            event
                .validate_metadata()
                .map_err(|_| read::Error::InvalidRequest)?;
            Ok(domain::ActivityRecord {
                sequence: notice.sequence,
                received_at_ms: notice.received_at_ms,
                event,
            })
        })
        .collect::<Result<Vec<_>, read::Error>>()?;
    Ok(domain::ActivityPage {
        epoch: batch.epoch.clone(),
        oldest_sequence: batch.oldest_sequence,
        current_sequence: batch.current_sequence,
        records,
        cursor: domain::ActivityCursor {
            epoch: batch.epoch,
            sequence: batch.cursor_sequence,
        },
        has_more: batch.has_more,
        sessions: None,
        reset: batch.reset.map(|reset| match reset {
            arrivals::Reset::Restarted => domain::ActivityReset::Restarted,
            arrivals::Reset::RetentionGap => domain::ActivityReset::RetentionGap,
            arrivals::Reset::AheadOfFeed => domain::ActivityReset::AheadOfFeed,
        }),
    })
}

fn session_snapshot(
    snapshot: Option<read::SessionSnapshot>,
) -> Result<Option<domain::ActivitySessions>, read::Error> {
    snapshot
        .map(|snapshot| {
            Ok(domain::ActivitySessions {
                observed_at_ms: snapshot.observed_at_ms,
                seats: snapshot
                    .seats
                    .into_iter()
                    .map(|session| {
                        Ok(domain::ActivitySession {
                            seat: prismattyc_core::AgentId::new(session.seat)
                                .map_err(|_| read::Error::InvalidRequest)?,
                            session_id: session.session_id,
                            state: match session.state {
                                read::SessionState::Working => {
                                    domain::ActivitySessionState::Working
                                }
                                read::SessionState::Waiting => {
                                    domain::ActivitySessionState::Waiting
                                }
                                read::SessionState::Idle => domain::ActivitySessionState::Idle,
                                read::SessionState::Stuck => domain::ActivitySessionState::Stuck,
                                read::SessionState::Stopped => {
                                    domain::ActivitySessionState::Stopped
                                }
                            },
                            last_output_at_ms: session.last_output_at_ms,
                            harness: session.harness,
                            model: session.model,
                            quota: session.quota,
                            project: session.project,
                        })
                    })
                    .collect::<Result<_, read::Error>>()?,
            })
        })
        .transpose()
}
