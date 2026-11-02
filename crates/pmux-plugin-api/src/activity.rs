// SPDX-License-Identifier: MIT
//! Explicit conversion preserves the existing activity JSON contract in guests.

use crate::guest::pmux::activity::read;
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
        reset: page.reset.map(|reset| match reset {
            read::Reset::Restarted => domain::ActivityReset::Restarted,
            read::Reset::RetentionGap => domain::ActivityReset::RetentionGap,
            read::Reset::AheadOfFeed => domain::ActivityReset::AheadOfFeed,
        }),
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
