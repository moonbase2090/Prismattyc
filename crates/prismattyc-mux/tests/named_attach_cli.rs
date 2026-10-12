//! Layout removal, attach, source update, and a private control connection.
//! Git, cargo, and the attach binaries are fakes. No checkout is updated.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use prismattyc_mux::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, SpawnSpec,
    PROTOCOL_VERSION,
};

mod support;
use support::clear_command_env;

fn temp_root(kind: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-{kind}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

fn socket_path(root: &Path) -> PathBuf {
    root.join("pmux.sock")
}

struct ServerGuard {
    child: Child,
    socket: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn apply_isolation(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env(
            "PRISMATTYC_CONFIG",
            root.join("config").join("prismattyc.toml"),
        )
        .env_remove("PMUX_SOCKET")
        .env_remove("PMUX_AGENT")
        .env_remove("PRISMATTYC_PANE_ID")
        .env_remove("PRISMATTYC_REPO")
        .env_remove("PMUX_ATTACH")
        .env_remove("PRISMATTYC_HOST");
}

fn start_server(socket: &Path, root: &Path) -> ServerGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    apply_isolation(&mut command, root);
    let guard = ServerGuard {
        child: command
            .arg("--socket")
            .arg(socket)
            .args(["--", "/bin/sh", "-c", "exec sleep 999"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pmuxd"),
        socket: socket.to_path_buf(),
    };
    for _ in 0..100 {
        if socket.exists() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not publish {}", socket.display());
}

fn pmux(socket: &Path, root: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    apply_isolation(&mut command, root);
    command
        .arg("--socket")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null())
        .current_dir(root);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().expect("run pmux")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "pmux failed: {}", text(output));
    text(output)
}

fn fail(output: &Output, needle: &str) {
    assert!(
        !output.status.success(),
        "expected failure: {}",
        text(output)
    );
    let body = text(output);
    assert!(body.contains(needle), "missing {needle} in {body}");
}

fn write_exe(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn layout_file(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        "{\"version\":1,\"saved_at_unix\":1,\"session\":\"demo\",\"windows\":[{\"title\":\"t\",\"cols\":80,\"rows\":24,\"root\":{\"kind\":\"leaf\"}}]}\n",
    )
    .unwrap();
}

#[test]
fn layout_rm_removes_named_files() {
    let root = temp_root("layout-rm");
    let socket = socket_path(&root);
    let layouts = root.join("data/prismattyc/layouts");
    layout_file(&layouts, "one");
    layout_file(&layouts, "two");
    std::fs::write(layouts.join("note.txt"), "keep\n").unwrap();

    fail(
        &pmux(&socket, &root, &[], &["layout", "rm", "--nope"]),
        "usage: pmux layout rm",
    );
    fail(
        &pmux(&socket, &root, &[], &["layout", "rm"]),
        "usage: pmux layout rm",
    );
    fail(
        &pmux(&socket, &root, &[], &["layout", "rm", "--all", "one"]),
        "usage: pmux layout rm",
    );
    assert!(layouts.join("one.json").exists());

    let removed = ok(&pmux(&socket, &root, &[], &["layout", "rm", "one"]));
    assert!(removed.contains("one.json"), "{removed}");
    assert!(!layouts.join("one.json").exists());
    assert!(layouts.join("two.json").exists());

    fail(
        &pmux(&socket, &root, &[], &["layout", "rm", "missing", "two"]),
        "no such file",
    );
    assert!(!layouts.join("two.json").exists());
    assert!(layouts.join("note.txt").exists());

    layout_file(&layouts, "left");
    layout_file(&layouts, "right");
    let all = ok(&pmux(&socket, &root, &[], &["layout", "rm", "--all"]));
    assert!(all.contains("left"), "{all}");
    assert!(all.contains("right"), "{all}");
    assert!(!layouts.join("left.json").exists());
    assert!(!layouts.join("right.json").exists());

    let empty = ok(&pmux(&socket, &root, &[], &["layout", "rm", "--all"]));
    assert!(empty.contains("no layouts to remove"), "{empty}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn attach_refuses_bad_args_and_execs_a_stand_in() {
    let root = temp_root("attach");
    let socket = socket_path(&root);
    let log = root.join("attach.log");
    let attach = root.join("bin/pmux-attach");
    write_exe(
        &attach,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ATTACH_LOG\"\nprintf '%s\\n' \"$*\"\nexit 0\n",
    );
    let host = root.join("bin/prismattyc-host");
    write_exe(
        &host,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOST_LOG\"\nprintf 'host %s\\n' \"$*\"\nexit 0\n",
    );
    let host_log = root.join("host.log");
    let extra = [
        ("PMUX_ATTACH", attach.to_str().unwrap()),
        ("PRISMATTYC_HOST", host.to_str().unwrap()),
        ("ATTACH_LOG", log.to_str().unwrap()),
        ("HOST_LOG", host_log.to_str().unwrap()),
    ];

    fail(
        &pmux(&socket, &root, &extra, &["attach", "--bogus"]),
        "unknown attach argument",
    );
    fail(
        &pmux(
            &socket,
            &root,
            &extra,
            &["attach", "--json", "--styled-json"],
        ),
        "mutually exclusive",
    );
    fail(
        &pmux(
            &socket,
            &root,
            &extra,
            &["attach", "--read-only", "--write", "hi"],
        ),
        "--read-only cannot be combined with --write",
    );
    fail(
        &pmux(&socket, &root, &extra, &["attach", "--write"]),
        "--write requires UTF-8 text",
    );
    fail(
        &pmux(&socket, &root, &extra, &["attach", "--all", "--watch"]),
        "cannot be combined",
    );
    fail(
        &pmux(
            &socket,
            &root,
            &extra,
            &["attach", "--space-id", "abc", "--session-id", "1"],
        ),
        "--space-id requires a 32-hex Space ID",
    );
    let space = "0123456789abcdef0123456789abcdef";
    fail(
        &pmux(
            &socket,
            &root,
            &extra,
            &["attach", "--space-id", space, "--session-id", "1"],
        ),
        "not running",
    );

    std::fs::write(&socket, b"not-a-socket").unwrap();
    fail(
        &pmux(&socket, &root, &extra, &["attach", "--json"]),
        "not a same-uid Prismattyc control socket",
    );
    assert!(
        !log.exists() || std::fs::read_to_string(&log).unwrap().is_empty(),
        "foreign socket must not exec attach"
    );
    std::fs::remove_file(&socket).unwrap();

    let _server = start_server(&socket, &root);
    let dumped = ok(&pmux(
        &socket,
        &root,
        &extra,
        &["attach", "--json", "--pane", "1", "--read-only"],
    ));
    assert!(dumped.contains("--json"), "{dumped}");
    assert!(dumped.contains("--pane"), "{dumped}");
    assert!(dumped.contains("--read-only"), "{dumped}");
    assert!(dumped.contains(socket.to_str().unwrap()), "{dumped}");

    let wrote = ok(&pmux(
        &socket,
        &root,
        &extra,
        &["attach", "--styled-json", "--write", "hello", "--fit"],
    ));
    assert!(wrote.contains("--styled-json"), "{wrote}");
    assert!(wrote.contains("--write"), "{wrote}");
    assert!(wrote.contains("hello"), "{wrote}");
    assert!(wrote.contains("--fit"), "{wrote}");

    fail(
        &pmux(
            &socket,
            &root,
            &extra,
            &["attach", "--space-id", space, "--session-id", "99999"],
        ),
        "no longer running",
    );

    let all = pmux(&socket, &root, &extra, &["attach", "--all"]);
    let all_text = text(&all);
    let host_body = std::fs::read_to_string(&host_log).unwrap_or_default();
    assert!(
        all_text.contains("needs WAYLAND_DISPLAY")
            || all_text.contains("--attach-session")
            || host_body.contains("--attach-session"),
        "{all_text} host={host_body}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn fake_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("crates/prismattyc-host")).unwrap();
    std::fs::create_dir_all(repo.join("crates/prismattyc-mux")).unwrap();
    std::fs::create_dir_all(repo.join("crates/prismattyc")).unwrap();
    std::fs::create_dir_all(repo.join("crates/pmux-mcp")).unwrap();
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    repo
}

fn install_tools(root: &Path) {
    write_exe(
        &root.join("bin/git"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$GIT_LOG\"\nif [ \"$1\" = status ]; then\n  if [ -n \"$GIT_DIRTY\" ]; then printf ' M dirty\\n'; fi\n  exit 0\nfi\nif [ \"$1\" = rev-parse ]; then printf 'abc1234\\n'; exit 0; fi\nif [ \"$GIT_FAIL\" = \"$1\" ]; then exit 1; fi\nexit 0\n",
    );
    write_exe(
        &root.join("bin/cargo"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CARGO_LOG\"\nif [ -n \"$CARGO_FAIL\" ]; then exit 1; fi\nexit 0\n",
    );
}

fn tool_env<'a>(root: &'a Path, repo: &'a Path) -> Vec<(&'a str, String)> {
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    );
    vec![
        ("PATH", path),
        ("PRISMATTYC_REPO", repo.display().to_string()),
        ("GIT_LOG", root.join("git.log").display().to_string()),
        ("CARGO_LOG", root.join("cargo.log").display().to_string()),
    ]
}

fn update(root: &Path, repo: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let owned = tool_env(root, repo);
    let mut pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
    pairs.extend_from_slice(extra);
    pmux(&root.join("missing.sock"), root, &pairs, args)
}

#[test]
fn source_update_uses_the_fake_toolchain() {
    let root = temp_root("update");
    let repo = fake_repo(&root);
    install_tools(&root);
    let missing = root.join("missing-repo");

    fail(
        &update(&root, &missing, &[], &["update", "--source", "--host"]),
        "PRISMATTYC_REPO",
    );
    fail(
        &update(
            &root,
            &repo,
            &[("GIT_DIRTY", "1")],
            &["update", "--source", "--host"],
        ),
        "working tree is dirty",
    );
    assert!(
        !root.join("cargo.log").exists()
            || std::fs::read_to_string(root.join("cargo.log"))
                .unwrap()
                .is_empty()
    );
    fail(
        &update(
            &root,
            &repo,
            &[("GIT_FAIL", "checkout")],
            &["update", "--source", "--mux"],
        ),
        "git checkout main failed",
    );
    fail(
        &update(
            &root,
            &repo,
            &[("CARGO_FAIL", "1")],
            &["update", "--source", "--host"],
        ),
        "cargo install --path crates/prismattyc-host",
    );

    let _ = std::fs::remove_file(root.join("cargo.log"));
    let mux = ok(&update(&root, &repo, &[], &["update", "--source", "--mux"]));
    assert!(mux.contains("done"), "{mux}");
    let cargo_log = std::fs::read_to_string(root.join("cargo.log")).unwrap();
    assert!(cargo_log.contains("crates/prismattyc-mux"), "{cargo_log}");
    assert!(cargo_log.contains("crates/pmux-mcp"), "{cargo_log}");
    assert!(!cargo_log.contains("crates/prismattyc-host"), "{cargo_log}");

    let _ = std::fs::remove_file(root.join("cargo.log"));
    #[cfg(target_os = "macos")]
    {
        fail(
            &update(&root, &repo, &[], &["update", "--source", "--host"]),
            "Prismattyc.app was not rebuilt",
        );
        write_exe(
            &repo.join("scripts/install-prismattyc-host-macos.sh"),
            "#!/bin/sh\nexit 0\n",
        );
    }
    let host = ok(&update(
        &root,
        &repo,
        &[],
        &["update", "--source", "--host"],
    ));
    assert!(host.contains("done"), "{host}");
    let host_log = std::fs::read_to_string(root.join("cargo.log")).unwrap();
    assert!(host_log.contains("crates/prismattyc-host"), "{host_log}");

    write_exe(&repo.join("scripts/install-man.sh"), "#!/bin/sh\nexit 1\n");
    let _ = std::fs::remove_file(root.join("cargo.log"));
    let all = ok(&update(&root, &repo, &[], &["update", "--source"]));
    assert!(all.contains("installing prismattyc"), "{all}");
    assert!(all.contains("done"), "{all}");
    if all.contains("man page") {
        assert!(all.contains("warning"), "{all}");
    }
    let all_log = std::fs::read_to_string(root.join("cargo.log")).unwrap();
    assert!(
        all_log.contains("crates/prismattyc\n") || all_log.contains("crates/prismattyc "),
        "{all_log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

struct Ids {
    next: std::cell::Cell<u64>,
}

fn roundtrip(stream: &mut UnixStream, ids: &Ids, request: ControlRequest) -> ControlResponse {
    serde_json::to_writer(&mut *stream, &request).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).expect("control reply");
    let response: ControlResponse = serde_json::from_str(&line).unwrap_or_else(|error| {
        panic!("bad reply {error}: {line}");
    });
    let expected = ids.next.get();
    if expected != 0 {
        assert_eq!(response.request_id, expected);
    }
    response
}

fn req(ids: &Ids, build: impl FnOnce(u64) -> ControlRequest) -> ControlRequest {
    let request_id = ids.next.get() + 1;
    ids.next.set(request_id);
    build(request_id)
}

fn expect_ok(response: ControlResponse) -> ControlResponseData {
    match response.body {
        ControlResponseBody::Ok { response } => response,
        ControlResponseBody::Error { error } => panic!("{} ({:?})", error.message, error.code),
    }
}

fn expect_err(response: ControlResponse, needle: &str) -> String {
    match response.body {
        ControlResponseBody::Error { error } => {
            assert!(
                error.message.contains(needle),
                "missing {needle} in {}",
                error.message
            );
            error.message
        }
        ControlResponseBody::Ok { response } => panic!("expected {needle}, got {response:?}"),
    }
}

#[test]
fn control_plane_answers_a_private_daemon() {
    let root = temp_root("control");
    let socket = socket_path(&root);
    let _server = start_server(&socket, &root);
    let mut stream = UnixStream::connect(&socket).unwrap();
    let ids = Ids {
        next: std::cell::Cell::new(0),
    };

    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            ControlRequest::Ping {
                version: PROTOCOL_VERSION,
                request_id: 0,
            },
        ),
        "request_id must be non-zero",
    );
    // The zero id is not assigned by `req`, so the assertion above used request_id 0.
    ids.next.set(0);
    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            req(&ids, |request_id| ControlRequest::Ping {
                version: PROTOCOL_VERSION + 1,
                request_id,
            }),
        ),
        "incompatible",
    );

    let info = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::ServerInfo {
            version: PROTOCOL_VERSION,
            request_id,
        }),
    ));
    match info {
        ControlResponseData::ServerInfo {
            package_version,
            pid,
        } => {
            assert!(!package_version.is_empty());
            assert_ne!(pid, 0);
        }
        other => panic!("server info: {other:?}"),
    }
    let pong = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::Ping {
            version: PROTOCOL_VERSION,
            request_id,
        }),
    ));
    assert!(matches!(pong, ControlResponseData::Pong));

    let snapshot = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        }),
    ));
    let ControlResponseData::Snapshot { snapshot } = snapshot else {
        panic!("snapshot");
    };
    let session = snapshot.sessions.first().expect("daemon session");
    let window = session.windows.first().expect("window");
    let pane = window.panes.first().expect("pane").id;
    let session_id = session.id;
    let window_id = window.id;

    let events = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id,
            after_sequence: 0,
            limit: Some(4),
        }),
    ));
    assert!(matches!(events, ControlResponseData::Events { .. }));

    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            req(&ids, |request_id| ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id,
                client_id: 1,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 0,
            }),
        ),
        "client",
    );
    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            req(&ids, |request_id| ControlRequest::ReadPaneState {
                version: PROTOCOL_VERSION,
                request_id,
                client_id: 1,
                pane_id: pane,
            }),
        ),
        "client",
    );
    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            req(&ids, |request_id| ControlRequest::MailWait {
                version: PROTOCOL_VERSION,
                request_id,
                client_id: 1,
                timeout_ms: 1,
            }),
        ),
        "client",
    );

    let registered = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        }),
    ));
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        panic!("register: {registered:?}");
    };
    let names = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::SessionNames {
            version: PROTOCOL_VERSION,
            request_id,
        }),
    ));
    match names {
        ControlResponseData::SessionNames { names } => assert!(!names.is_empty(), "{names:?}"),
        other => panic!("names: {other:?}"),
    }

    let lease = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id: pane,
        }),
    ));
    match lease {
        ControlResponseData::Lease {
            pane_id,
            controller_id,
            ..
        } => {
            assert_eq!(pane_id, pane);
            assert_eq!(controller_id, Some(client_id));
        }
        other => panic!("lease: {other:?}"),
    }
    let wrote = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id: pane,
            data: "printf ready\n".into(),
        }),
    ));
    assert!(
        matches!(wrote, ControlResponseData::WriteQueued { .. }),
        "{wrote:?}"
    );
    let released = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id: pane,
        }),
    ));
    match released {
        ControlResponseData::Lease {
            controller_id: None,
            ..
        } => {}
        other => panic!("release: {other:?}"),
    }
    let took = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::TakeoverLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id: pane,
        }),
    ));
    assert!(
        matches!(took, ControlResponseData::Lease { .. }),
        "{took:?}"
    );

    let named = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::NameSession {
            version: PROTOCOL_VERSION,
            request_id,
            session_id,
            name: "renamed".into(),
        }),
    ));
    assert!(
        matches!(
            named,
            ControlResponseData::Mutation { .. } | ControlResponseData::Session { .. }
        ),
        "{named:?}"
    );

    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            req(&ids, |request_id| ControlRequest::SetClientSpace {
                version: PROTOCOL_VERSION,
                request_id,
                client_id,
                space_id: "not-hex".into(),
            }),
        ),
        "Space context",
    );

    let created = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id,
            name: "extra".into(),
            spawn: SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "exec sleep 30".into()],
                cwd: None,
                env: Default::default(),
            },
            cols: Some(80),
            rows: Some(24),
            agent_id: None,
            headless: false,
        }),
    ));
    let ControlResponseData::Session {
        session_id: extra, ..
    } = created
    else {
        panic!("create: {created:?}");
    };
    let switched = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            session_id: extra,
        }),
    ));
    assert!(
        matches!(
            switched,
            ControlResponseData::Session { .. } | ControlResponseData::Mutation { .. }
        ),
        "{switched:?}"
    );
    expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::DestroySession {
            version: PROTOCOL_VERSION,
            request_id,
            session_id: extra,
        }),
    ));
    expect_err(
        roundtrip(
            &mut stream,
            &ids,
            req(&ids, |request_id| ControlRequest::ShutdownIdle {
                version: PROTOCOL_VERSION,
                request_id,
                client_id,
            }),
        ),
        "sessions are running",
    );

    let focus = expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::SuggestFocus {
            version: PROTOCOL_VERSION,
            request_id,
            window_id,
            pane_id: pane,
            reason: Some("mail".into()),
        }),
    ));
    assert!(
        matches!(focus, ControlResponseData::Mutation { .. }),
        "{focus:?}"
    );
    expect_ok(roundtrip(
        &mut stream,
        &ids,
        req(&ids, |request_id| ControlRequest::DisconnectClient {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
        }),
    ));
    let _ = std::fs::remove_dir_all(&root);
}
