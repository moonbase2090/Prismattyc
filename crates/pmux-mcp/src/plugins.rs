// SPDX-License-Identifier: MIT
//! Generic MCP routing through daemon-owned, connection-bound export handles.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use prismattyc_mux::plugins::{Client, Reply, Request, ToolExport};
use rmcp::{
    model::{CallToolResult, ContentBlock, Tool},
    ErrorData,
};

#[derive(Default)]
struct Connection {
    client: Option<Client>,
    exports: BTreeMap<String, ToolExport>,
}

#[derive(Clone)]
pub struct Plugins {
    socket: PathBuf,
    allowlist: Vec<String>,
    state: Arc<Mutex<Connection>>,
}

impl Plugins {
    pub fn new(socket: PathBuf, allowlist: Vec<String>) -> Self {
        Self {
            socket,
            allowlist,
            state: Arc::new(Mutex::new(Connection::default())),
        }
    }

    fn request(&self, state: &mut Connection, operation: Request) -> Result<Reply, String> {
        if state.client.is_none() {
            state.client = Some(Client::connect(&self.socket).map_err(|error| error.to_string())?);
        }
        let reply = state
            .client
            .as_mut()
            .unwrap()
            .request(operation)
            .map_err(|error| error.to_string());
        if reply.is_err() {
            state.client = None;
            state.exports.clear();
        }
        reply
    }

    fn refresh(&self, state: &mut Connection) -> Result<(), String> {
        let Reply::Exports { tools } = self.request(
            state,
            Request::Exports {
                allowlist: self.allowlist.clone(),
            },
        )?
        else {
            return Err("unexpected plug-in export reply".into());
        };
        state.exports = tools
            .into_iter()
            .map(|tool| (tool.name.clone(), tool))
            .collect();
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Tool>, ErrorData> {
        if self.allowlist.is_empty() {
            return Ok(Vec::new());
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = this
                .state
                .lock()
                .map_err(|_| "plug-in connection is unavailable".to_string())?;
            this.refresh(&mut state)?;
            state
                .exports
                .values()
                .map(|export| {
                    let input = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                        &export.input_schema,
                    )
                    .map_err(|error| error.to_string())?;
                    let output =
                        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                            &export.output_schema,
                        )
                        .map_err(|error| error.to_string())?;
                    Ok(Tool::new(
                        export.name.clone(),
                        format!("{} / {}", export.plugin, export.contribution),
                        input,
                    )
                    .with_raw_output_schema(Arc::new(output)))
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .await
        .map_err(|_| ErrorData::internal_error("plug-in discovery was interrupted", None))?
        .map_err(|error| ErrorData::internal_error(error, None))
    }

    pub async fn call(
        &self,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.allowlist.is_empty() {
            return Err(ErrorData::invalid_params("unknown tool", None));
        }
        let this = self.clone();
        let reply = tokio::task::spawn_blocking(move || {
            let mut state = this
                .state
                .lock()
                .map_err(|_| "plug-in connection is unavailable".to_string())?;
            if state.client.is_none() {
                this.refresh(&mut state)?;
            }
            let export = state
                .exports
                .get(&name)
                .ok_or_else(|| "unknown or revoked plug-in tool; refresh tools/list".to_string())?;
            let handle = export.handle.clone();
            this.request(
                &mut state,
                Request::CallExport {
                    handle,
                    arguments: serde_json::Value::Object(arguments).to_string(),
                    min_interval_ms: 1000,
                },
            )
        })
        .await
        .map_err(|_| {
            ErrorData::internal_error("plug-in call was interrupted and was not replayed", None)
        })?;
        Ok(match reply {
            Ok(Reply::Tool { json, is_error }) => {
                let value = serde_json::from_str(&json).map_err(|_| {
                    ErrorData::internal_error("plug-in returned invalid JSON", None)
                })?;
                if is_error {
                    CallToolResult::structured_error(value)
                } else {
                    CallToolResult::structured(value)
                }
            }
            Ok(Reply::Retry { .. }) => CallToolResult::error(vec![ContentBlock::text(
                "activity feed changed; retry the call. The interrupted call was not replayed.",
            )]),
            Ok(_) => CallToolResult::error(vec![ContentBlock::text("unexpected plug-in reply")]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        })
    }
}
