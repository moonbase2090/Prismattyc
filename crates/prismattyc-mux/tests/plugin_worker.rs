// SPDX-License-Identifier: MIT
//! Exercise the private parent protocol against the actual pmuxd executable.

#[path = "../src/plugins/schema.rs"]
mod schema;
#[path = "../src/plugins/wire.rs"]
mod wire;
#[allow(dead_code)] // The child executes the entry point in the built pmuxd.
#[path = "../src/plugins/worker.rs"]
mod worker;

const INTERNAL_WORKER_ARG: &str = prismattyc_mux::plugins::INTERNAL_WORKER_ARG;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use pmux_plugin_api::host::pmux::activity::manage;
use pmux_plugin_api::host::pmux::{activity::read, plugin::lifecycle, tool::call};
use pmux_plugin_runtime::ActivityProvider;
use prismattyc_core::activity::{ActivityCursor, ActivityPage};

fn executable() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_pmuxd"))
}

fn schemas() -> BTreeMap<String, wire::CommandSchema> {
    ["poll", "loop", "grow"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                wire::CommandSchema {
                    input: r#"{"type":"object","additionalProperties":false}"#.into(),
                    output: r#"{"type":"object"}"#.into(),
                },
            )
        })
        .collect()
}

fn context() -> lifecycle::Context {
    lifecycle::Context {
        api_version: pmux_plugin_api::API_VERSION.into(),
        capabilities: vec![
            lifecycle::Capability {
                name: "pmux:activity/read".into(),
                version: "1.3.0".into(),
                available: true,
            },
            lifecycle::Capability {
                name: "pmux:tool/call".into(),
                version: "1.0.0".into(),
                available: true,
            },
        ],
        limits: lifecycle::Limits {
            memory_bytes: 4 * 1024 * 1024,
            result_bytes: 1024 * 1024,
            frame_bytes: 2 * 1024 * 1024,
        },
        role: lifecycle::Role::ToolService,
    }
}

fn request(contribution: &str) -> lifecycle::Message {
    lifecycle::Message::Tool(call::Request {
        id: 9,
        contribution: contribution.into(),
        arguments: "{}".into(),
    })
}

struct Disabled;
impl ActivityProvider for Disabled {
    fn remove_stopped(&mut self, _: u64, _: u64) -> Result<manage::Removal, String> {
        Err("unavailable".into())
    }
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        Ok(vec![read::Endpoint {
            handle: 3,
            label: "local".into(),
            kind: read::EndpointKind::Local,
            min_interval_ms: 250,
        }])
    }
    fn poll(
        &mut self,
        _: u64,
        _: Option<ActivityCursor>,
        _: u16,
    ) -> Result<ActivityPage, read::Error> {
        Err(read::Error::FeedDisabled)
    }
}

#[test]
fn a_real_daemon_worker_validates_prepares_and_brokers_typed_reads() {
    worker::validate(
        executable(),
        pmux_plugin_fixture::component().to_vec(),
        schemas(),
    )
    .unwrap();
    let worker = worker::PendingWorker::spawn(executable())
        .unwrap()
        .prepare(
            pmux_plugin_fixture::component().to_vec(),
            context(),
            BTreeMap::new(),
            schemas(),
        )
        .unwrap()
        .activate()
        .unwrap();
    let lifecycle::Reply::Tool(reply) = worker.invoke(request("poll"), &mut Disabled).unwrap()
    else {
        panic!("expected tool response");
    };
    assert_eq!(reply.id, 9);
    assert!(reply.is_error);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply.json).unwrap(),
        serde_json::json!({"error":"feed-disabled"})
    );
    worker.cancel();
    assert!(worker
        .invoke(request("poll"), &mut Disabled)
        .unwrap_err()
        .to_string()
        .contains("revoked"));
}

#[test]
fn an_infinite_guest_does_not_stall_the_daemon_process() {
    let worker = worker::PendingWorker::spawn(executable())
        .unwrap()
        .prepare(
            pmux_plugin_fixture::component().to_vec(),
            context(),
            BTreeMap::new(),
            schemas(),
        )
        .unwrap()
        .activate()
        .unwrap();
    let start = Instant::now();
    assert!(worker.invoke(request("loop"), &mut Disabled).is_err());
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(worker.invoke(request("poll"), &mut Disabled).is_err());
}

struct PendingRead {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl ActivityProvider for PendingRead {
    fn remove_stopped(&mut self, _: u64, _: u64) -> Result<manage::Removal, String> {
        unreachable!()
    }
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        Err(read::Error::Revoked)
    }
    fn poll(
        &mut self,
        _: u64,
        _: Option<ActivityCursor>,
        _: u16,
    ) -> Result<ActivityPage, read::Error> {
        unreachable!()
    }
}

#[test]
fn revocation_does_not_wait_for_a_pending_host_import_or_publish_its_reply() {
    let worker = Arc::new(
        worker::PendingWorker::spawn(executable())
            .unwrap()
            .prepare(
                pmux_plugin_fixture::component().to_vec(),
                context(),
                BTreeMap::new(),
                schemas(),
            )
            .unwrap()
            .activate()
            .unwrap(),
    );
    let (entered, receive) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let invoking = worker.clone();
    let call = std::thread::spawn(move || {
        invoking.invoke(
            request("poll"),
            &mut PendingRead {
                entered,
                release: wait,
            },
        )
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    let start = Instant::now();
    worker.cancel();
    assert!(start.elapsed() < Duration::from_secs(1));
    release.send(()).unwrap();
    assert!(call.join().unwrap().is_err());
}

#[test]
fn worker_parent_eof_exits_without_acquiring_a_daemon_lock_or_spawning_a_shell() {
    use std::process::{Command, Stdio};
    let mut child = Command::new(executable())
        .arg(INTERNAL_WORKER_ARG)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            panic!("orphaned worker survived parent EOF");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(child.wait().unwrap().success());
}
