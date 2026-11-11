//! The streamable-HTTP listener, exercised through the real `pmux-mcp` binary.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

const TOKEN: &str = "test-bearer-0123456789-abcdefghijklmnop";
const TOKEN_ENV: &str = "PMUX_MCP_TEST_BEARER";
const WAIT: Duration = Duration::from_secs(20);

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux-mcp"));
    command
        .args(args)
        .env(TOKEN_ENV, TOKEN)
        .env_remove("PMUX_AGENT")
        .env(
            "PMUX_SOCKET",
            std::env::temp_dir().join("pmux-mcp-http-no-daemon.sock"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Run a launch that must fail; returns its stderr.
async fn refused(args: &[&str]) -> String {
    let output = tokio::time::timeout(WAIT, command(args).output())
        .await
        .expect("a refused launch exits promptly")
        .expect("spawn");
    assert!(!output.status.success(), "{args:?} must not start");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stderr.contains(TOKEN), "stderr leaked the token: {stderr}");
    stderr
}

struct Listener {
    _child: Child,
    addr: SocketAddr,
}

async fn listening(extra: &[&str]) -> Listener {
    let mut args = vec![
        "--activity-only",
        "--listen",
        "127.0.0.1:0",
        "--token-env",
        TOKEN_ENV,
    ];
    args.extend_from_slice(extra);
    let mut child = command(&args).spawn().expect("spawn");
    let mut lines = BufReader::new(child.stderr.take().expect("stderr")).lines();
    let line = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .expect("the listener announces itself")
        .expect("read stderr")
        .expect("stderr line");
    assert!(!line.contains(TOKEN));
    let url = line.split("http://").nth(1).expect("announced url");
    let addr: SocketAddr = url
        .split('/')
        .next()
        .unwrap()
        .parse()
        .expect("announced address");
    // Keep draining so the child never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Listener {
        _child: child,
        addr,
    }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

async fn post(
    addr: SocketAddr,
    path: &str,
    bearer: Option<&str>,
    host: Option<&str>,
    body: &str,
) -> Reply {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n",
        host.map_or_else(|| addr.to_string(), str::to_string),
        body.len()
    );
    if let Some(bearer) = bearer {
        request.push_str(&format!("Authorization: {bearer}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut raw))
        .await
        .expect("reply")
        .expect("read");
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    Reply {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"http-test","version":"1"}}}"#;

#[tokio::test]
async fn it_binds_the_loopback_address_it_was_given() {
    let listener = listening(&[]).await;
    assert!(listener.addr.ip().is_loopback(), "{}", listener.addr);
}

#[tokio::test]
async fn a_request_without_the_bearer_is_unauthorized() {
    let listener = listening(&[]).await;
    let reply = post(listener.addr, "/mcp", None, None, INITIALIZE).await;
    assert_eq!(reply.status, 401);
    assert!(
        reply.headers.contains("www-authenticate: bearer"),
        "{}",
        reply.headers
    );
    assert!(!reply.body.contains("serverinfo"), "{}", reply.body);
}

#[tokio::test]
async fn a_wrong_bearer_is_unauthorized() {
    let listener = listening(&[]).await;
    for wrong in [
        format!("Bearer {TOKEN}x"),
        format!("Bearer {}", &TOKEN[1..]),
        "Bearer wrong".to_string(),
        format!("Basic {TOKEN}"),
        TOKEN.to_string(),
    ] {
        let reply = post(listener.addr, "/mcp", Some(&wrong), None, INITIALIZE).await;
        assert_eq!(reply.status, 401, "{wrong}");
    }
}

#[tokio::test]
async fn the_right_bearer_reaches_the_mcp_endpoint_and_nothing_else() {
    let listener = listening(&[]).await;
    let bearer = format!("Bearer {TOKEN}");
    let reply = post(listener.addr, "/mcp", Some(&bearer), None, INITIALIZE).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("\"tools\""), "{}", reply.body);
    // Only the activity connector's own tools exist; no mailbox identity is configured.
    assert!(!reply.body.contains("pmux_send"), "{}", reply.body);
    let other = post(listener.addr, "/elsewhere", Some(&bearer), None, INITIALIZE).await;
    assert_eq!(other.status, 404);
    // Auth comes first, so an unauthenticated probe cannot tell which paths exist.
    let probe = post(listener.addr, "/elsewhere", None, None, INITIALIZE).await;
    assert_eq!(probe.status, 401);
}

#[tokio::test]
async fn the_listener_exports_no_mailbox_or_space_tools() {
    let listener = listening(&[]).await;
    let bearer = format!("Bearer {TOKEN}");
    let reply = post(
        listener.addr,
        "/mcp",
        Some(&bearer),
        None,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"pmux_send","arguments":{"to":"x","summary":"y"}}}"#,
    )
    .await;
    // Without a daemon the call fails, and never as a mailbox send.
    assert!(
        reply.body.contains("\"error\"") || reply.body.contains("\"isError\":true"),
        "a mailbox tool must not be reachable: {}",
        reply.body
    );
    assert!(!reply.body.contains("\"isError\":false"), "{}", reply.body);
}

#[tokio::test]
async fn a_forwarded_hostname_needs_an_explicit_allowance() {
    let bearer = format!("Bearer {TOKEN}");
    let closed = listening(&[]).await;
    let reply = post(
        closed.addr,
        "/mcp",
        Some(&bearer),
        Some("activity.example.test"),
        INITIALIZE,
    )
    .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    let open = listening(&["--allow-host", "activity.example.test"]).await;
    let reply = post(
        open.addr,
        "/mcp",
        Some(&bearer),
        Some("activity.example.test"),
        INITIALIZE,
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}

#[tokio::test]
async fn listen_is_refused_without_a_token() {
    let stderr = refused(&["--activity-only", "--listen", "127.0.0.1:0"]).await;
    assert!(stderr.contains("bearer token"), "{stderr}");
}

#[tokio::test]
async fn listen_is_refused_with_a_mailbox_identity() {
    let stderr = refused(&[
        "--activity-only",
        "--as",
        "someone",
        "--listen",
        "127.0.0.1:0",
        "--token-env",
        TOKEN_ENV,
    ])
    .await;
    assert!(stderr.contains("--as"), "{stderr}");
    // The ambient agent variable is no way around it either.
    let stderr = refused(&[
        "--as",
        "someone",
        "--listen",
        "127.0.0.1:0",
        "--token-env",
        TOKEN_ENV,
    ])
    .await;
    assert!(stderr.contains("--activity-only"), "{stderr}");
}

#[tokio::test]
async fn listen_is_refused_without_activity_only_or_with_other_exports() {
    let stderr = refused(&["--listen", "127.0.0.1:0", "--token-env", TOKEN_ENV]).await;
    assert!(stderr.contains("--activity-only"), "{stderr}");
    let stderr = refused(&[
        "--activity-only",
        "--plugin",
        "x.y",
        "--listen",
        "127.0.0.1:0",
        "--token-env",
        TOKEN_ENV,
    ])
    .await;
    assert!(stderr.contains("--plugin"), "{stderr}");
    let stderr = refused(&[
        "--activity-only",
        "--supervise",
        "--listen",
        "127.0.0.1:0",
        "--token-env",
        TOKEN_ENV,
    ])
    .await;
    assert!(stderr.contains("--supervise"), "{stderr}");
}

#[tokio::test]
async fn listen_is_refused_on_a_public_address_or_a_weak_token() {
    for address in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0"] {
        let stderr = refused(&[
            "--activity-only",
            "--listen",
            address,
            "--token-env",
            TOKEN_ENV,
        ])
        .await;
        assert!(stderr.contains("loopback"), "{address}: {stderr}");
    }
    let output = command(&[
        "--activity-only",
        "--listen",
        "127.0.0.1:0",
        "--token-env",
        TOKEN_ENV,
    ])
    .env(TOKEN_ENV, "short")
    .output()
    .await
    .expect("spawn");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("at least"), "{stderr}");
    assert!(
        !stderr.contains("short\n"),
        "the secret must not be echoed: {stderr}"
    );
}

#[tokio::test]
async fn token_flags_without_listen_are_refused() {
    let stderr = refused(&["--activity-only", "--token-env", TOKEN_ENV]).await;
    assert!(stderr.contains("--listen"), "{stderr}");
}
