// SPDX-License-Identifier: MIT
//! Generic MCP routing through daemon-owned, connection-bound export handles.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

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

struct Admission(Arc<AtomicBool>);
impl Drop for Admission {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Arguments(Vec<u8>);
// JSON arguments are escaped again inside the daemon's 1 MiB control envelope.
const MAX_ARGUMENTS_BYTES: usize = 480 * 1024;
impl std::io::Write for Arguments {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_ARGUMENTS_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "plug-in arguments exceed their byte limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct Plugins {
    socket: PathBuf,
    allowlist: Vec<String>,
    state: Arc<Mutex<Connection>>,
    busy: Arc<AtomicBool>,
}

impl Plugins {
    pub fn new(socket: PathBuf, allowlist: Vec<String>) -> Self {
        Self {
            socket,
            allowlist,
            state: Arc::new(Mutex::new(Connection::default())),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    fn admit(&self) -> Option<Admission> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Admission(self.busy.clone()))
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
        let permit = self.admit().ok_or_else(|| {
            ErrorData::internal_error(
                "another plug-in request is running; retry after it completes",
                None,
            )
        })?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
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
        let Some(permit) = self.admit() else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Another plug-in request is running; retry after it completes.",
            )]));
        };
        if name.len() > 64 {
            return Err(ErrorData::invalid_params("invalid plug-in tool name", None));
        }
        let mut encoded = Arguments(Vec::new());
        serde_json::to_writer(&mut encoded, &arguments).map_err(|_| {
            ErrorData::invalid_params("plug-in arguments exceed their byte limit", None)
        })?;
        let arguments = String::from_utf8(encoded.0)
            .map_err(|_| ErrorData::invalid_params("plug-in arguments are not UTF-8", None))?;
        let this = self.clone();
        let reply = tokio::task::spawn_blocking(move || {
            let _permit = permit;
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
                    arguments,
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_does_not_admit_more_jobs_while_native_work_is_still_running() {
        let Request::Admin { retry_id, .. } =
            prismattyc_mux::plugins::admin_request(prismattyc_mux::plugins::AdminAction::Disable {
                id: "org.moonbase.example".into(),
            })
            .unwrap()
        else {
            unreachable!()
        };
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mc-{}", &retry_id[..16]));
        std::fs::create_dir(&root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("s");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (ready_send, ready_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = BufReader::new(stream);
            for index in 1..=2 {
                let mut request = String::new();
                stream.read_line(&mut request).unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&request).unwrap()["operation"]
                        ["operation"],
                    "exports"
                );
                if index == 1 {
                    ready_send.send(()).unwrap();
                    let _ = release_receive.recv_timeout(Duration::from_secs(3));
                }
                let response = serde_json::json!({"version":1,"request_id":index,"status":"ok","response":{"kind":"plugin","reply":{"result":"exports","tools":[]}}});
                stream
                    .get_mut()
                    .write_all(&serde_json::to_vec(&response).unwrap())
                    .unwrap();
                stream.get_mut().write_all(b"\n").unwrap();
                stream.get_mut().flush().unwrap();
            }
        });
        let plugins = Plugins::new(socket, vec!["org.moonbase.example".into()]);
        let first = plugins.clone();
        let job = tokio::spawn(async move { first.list().await });
        tokio::task::spawn_blocking(move || {
            ready_receive.recv_timeout(Duration::from_secs(2)).unwrap()
        })
        .await
        .unwrap();
        job.abort();
        let _ = job.await;
        let response = tokio::time::timeout(Duration::from_millis(100), plugins.list())
            .await
            .unwrap();
        assert!(response
            .unwrap_err()
            .message
            .contains("another plug-in request is running"));
        let response = plugins
            .call("example".into(), Default::default())
            .await
            .unwrap();
        assert_eq!(response.is_error, Some(true));
        release_send.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match plugins.list().await {
                Ok(tools) => {
                    assert!(tools.is_empty());
                    break;
                }
                Err(error)
                    if error.message.contains("another plug-in request is running")
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("{error}"),
            }
        }
        server.join().unwrap();
        drop(plugins);
        std::fs::remove_dir_all(root).unwrap();
    }
}
