// SPDX-License-Identifier: MIT

use crate::service::{self, Metadata};
use pmux_plugin_api::{
    activity,
    guest::{
        pmux::{activity::read, plugin::lifecycle, tool::call},
        Guest,
    },
    API_VERSION,
};
use prismattyc_core::{
    activity::{ActivityCursor, ActivityKind, ActivityPage},
    AgentId,
};
use std::cell::Cell;

struct NativeMetadata;
impl Metadata for NativeMetadata {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        read::endpoints()
    }
    fn poll(
        &mut self,
        endpoint: u64,
        cursor: Option<ActivityCursor>,
        limit: u16,
    ) -> Result<ActivityPage, read::Error> {
        let cursor = cursor.map(|cursor| read::Cursor {
            epoch: cursor.epoch,
            sequence: cursor.sequence,
        });
        activity::poll(endpoint, cursor.as_ref(), limit)
    }

    fn wait(
        &mut self,
        endpoint: u64,
        seat: &AgentId,
        kinds: &[ActivityKind],
        after: Option<&ActivityCursor>,
        timeout_ms: u64,
    ) -> Result<ActivityPage, read::Error> {
        let names = kinds
            .iter()
            .map(|kind| {
                serde_json::to_value(kind)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let timeout =
            u32::try_from(timeout_ms.min(prismattyc_core::activity::PLUGIN_ACTIVITY_WAIT_MS))
                .unwrap_or(u32::MAX);
        let cursor = after.map(|cursor| read::Cursor {
            epoch: cursor.epoch.clone(),
            sequence: cursor.sequence,
        });
        activity::wait(endpoint, seat.as_str(), &names, timeout, cursor.as_ref())
    }
}

thread_local! { static READY: Cell<bool> = const { Cell::new(false) }; }
struct Plugin;
impl Guest for Plugin {
    fn handle(message: lifecycle::Message) -> Result<lifecycle::Reply, String> {
        READY.with(|ready| match message {
            lifecycle::Message::Start(context) => {
                if context.api_version != API_VERSION
                    || !matches!(context.role, lifecycle::Role::ToolService)
                {
                    return Err("connector requires a compatible tool service".into());
                }
                ready.set(true);
                Ok(lifecycle::Reply::Ready(API_VERSION.into()))
            }
            lifecycle::Message::Tool(request) if ready.get() => {
                let (result, is_error) = service::execute(
                    &mut NativeMetadata,
                    &request.contribution,
                    &request.arguments,
                );
                Ok(lifecycle::Reply::Tool(call::Response {
                    id: request.id,
                    json: result.to_string(),
                    is_error,
                }))
            }
            lifecycle::Message::Stop => {
                ready.set(false);
                Ok(lifecycle::Reply::Idle)
            }
            _ => Err("connector has no view contributions".into()),
        })
    }
}
pmux_plugin_api::guest::export_plugin!(Plugin);
