// SPDX-License-Identifier: MIT
//! Optional streamable-HTTP listener for the read-only activity tools.
//!
//! Off unless `--listen` is given. It binds a loopback address only, answers
//! nothing without the operator's bearer token, and serves the MCP transport
//! from the `rmcp` crate at `/mcp`. Reaching it from off the LAN is the
//! operator's tunnel; nothing here opens a public port.

use std::convert::Infallible;
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::header::{HeaderValue, AUTHORIZATION, CONNECTION, WWW_AUTHENTICATE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServerHandler;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_service::Service;

/// The only path that serves MCP.
pub const MCP_PATH: &str = "/mcp";
/// A token shorter than this is guessable enough to refuse outright.
pub const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 512;
const MAX_CONNECTIONS: usize = 16;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// The bearer secret. Never printed: `Debug` and `Display` redact it.
#[derive(Clone)]
pub struct Token(Arc<[u8]>);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    pub fn new(secret: &str) -> Result<Self, String> {
        let secret = secret.trim_end_matches(['\r', '\n']);
        if secret.len() < MIN_TOKEN_BYTES {
            return Err(format!(
                "the bearer token must be at least {MIN_TOKEN_BYTES} bytes"
            ));
        }
        if secret.len() > MAX_TOKEN_BYTES {
            return Err("the bearer token is too long".into());
        }
        if !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(
                "the bearer token must be printable ASCII with no spaces or line breaks".into(),
            );
        }
        Ok(Self(secret.as_bytes().into()))
    }

    /// Compare without stopping at the first differing byte.
    fn matches(&self, candidate: &[u8]) -> bool {
        let mut difference = self.0.len() ^ candidate.len();
        for (index, expected) in self.0.iter().enumerate() {
            difference |= usize::from(expected ^ candidate.get(index).copied().unwrap_or(0));
        }
        difference == 0
    }
}

/// Read the token from the environment variable `env_name` or the file `file`.
/// The value itself never appears in an error.
pub fn load_token(env_name: Option<&str>, file: Option<&Path>) -> Result<Token, String> {
    match (env_name, file) {
        (Some(_), Some(_)) => Err("pass only one of --token-env and --token-file".into()),
        (None, None) => {
            Err("--listen needs a bearer token: pass --token-env NAME or --token-file PATH".into())
        }
        (Some(name), None) => {
            let value = std::env::var(name)
                .map_err(|_| format!("environment variable {name} is not set to a usable value"))?;
            Token::new(&value).map_err(|error| format!("{name}: {error}"))
        }
        (None, Some(path)) => {
            refuse_shared_file(path)?;
            let value = std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read the token file: {}", error.kind()))?;
            Token::new(&value).map_err(|error| format!("token file: {error}"))
        }
    }
}

#[cfg(unix)]
fn refuse_shared_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot read the token file: {}", error.kind()))?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("the token file must not be readable by group or others (chmod 600)".into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_shared_file(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Parse `--listen`. Only a loopback IP literal with a port is accepted.
pub fn parse_listen(value: &str) -> Result<SocketAddr, String> {
    let address: SocketAddr = value
        .parse()
        .map_err(|_| "--listen needs an IP:port such as 127.0.0.1:8765".to_string())?;
    if !address.ip().is_loopback() {
        return Err(
            "--listen must be a loopback address (127.0.0.1 or ::1); expose it with a tunnel, not a public bind"
                .into(),
        );
    }
    Ok(address)
}

/// Whether the `Authorization` header carries exactly the configured bearer.
pub fn authorized(header: Option<&HeaderValue>, token: &Token) -> bool {
    let Some(value) = header.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some((scheme, secret)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && token.matches(secret.as_bytes())
}

fn reply(status: StatusCode) -> Response<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(Empty::<Bytes>::new().boxed());
    *response.status_mut() = status;
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    if !status.is_success() {
        // Refused callers do not keep a connection (and its slot) open.
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Serve MCP on `listener` until the future is dropped. `make` builds one handler per session.
pub async fn serve<S>(
    listener: TcpListener,
    token: Token,
    extra_hosts: Vec<String>,
    make: impl Fn() -> Result<S, std::io::Error> + Send + Sync + 'static,
) -> std::io::Result<()>
where
    S: ServerHandler + Send + 'static,
{
    let mut hosts = vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()];
    hosts.extend(extra_hosts);
    // Stateless JSON replies: no per-client session is kept between requests.
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        .with_legacy_session_mode(false)
        .with_json_response(true);
    let mcp = StreamableHttpService::new(make, Arc::new(LocalSessionManager::default()), config);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            // Drop the socket: the connection cap is a hard limit.
            continue;
        };
        let token = token.clone();
        let mcp = mcp.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = TowerToHyperService::new(tower_fn(
                move |request: Request<hyper::body::Incoming>| {
                    let token = token.clone();
                    let mut mcp = mcp.clone();
                    async move {
                        if !authorized(request.headers().get(AUTHORIZATION), &token) {
                            return Ok(reply(StatusCode::UNAUTHORIZED));
                        }
                        if request.uri().path() != MCP_PATH {
                            return Ok(reply(StatusCode::NOT_FOUND));
                        }
                        mcp.call(request).await
                    }
                },
            ));
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// A closure as a `tower_service::Service`.
#[derive(Clone)]
struct TowerFn<F>(F);

fn tower_fn<F>(f: F) -> TowerFn<F> {
    TowerFn(f)
}

impl<F, Fut, B> Service<Request<B>> for TowerFn<F>
where
    F: FnMut(Request<B>) -> Fut,
    Fut: std::future::Future<
        Output = Result<
            Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>,
            Infallible,
        >,
    >,
{
    type Response = Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>;
    type Error = Infallible;
    type Future = Fut;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        (self.0)(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef-test";

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    #[test]
    fn only_loopback_addresses_may_listen() {
        assert!(parse_listen("127.0.0.1:8765").is_ok());
        assert!(parse_listen("[::1]:8765").is_ok());
        for refused in [
            "0.0.0.0:8765",
            "192.168.1.5:8765",
            "[::]:8765",
            "localhost:8765",
            "8765",
            "10.0.0.1:1",
        ] {
            assert!(parse_listen(refused).is_err(), "{refused} must be refused");
        }
    }

    #[test]
    fn short_or_malformed_tokens_are_refused() {
        assert!(Token::new("short").is_err());
        assert!(Token::new(&"a".repeat(MIN_TOKEN_BYTES - 1)).is_err());
        assert!(Token::new(&"a".repeat(MIN_TOKEN_BYTES)).is_ok());
        assert!(Token::new(&format!("{} {}", "a".repeat(20), "b".repeat(20))).is_err());
        assert!(Token::new(&"a".repeat(MAX_TOKEN_BYTES + 1)).is_err());
        // A trailing newline from `echo > file` is not part of the secret.
        let token = Token::new(&format!("{SECRET}\n")).unwrap();
        assert!(authorized(
            Some(&header(&format!("Bearer {SECRET}"))),
            &token
        ));
    }

    #[test]
    fn the_bearer_must_match_exactly() {
        let token = Token::new(SECRET).unwrap();
        assert!(authorized(
            Some(&header(&format!("Bearer {SECRET}"))),
            &token
        ));
        assert!(authorized(
            Some(&header(&format!("bearer {SECRET}"))),
            &token
        ));
        for wrong in [
            format!("Bearer {SECRET}x"),
            format!("Bearer {}", &SECRET[..SECRET.len() - 1]),
            format!("Bearer {}", SECRET.to_uppercase()),
            format!("Basic {SECRET}"),
            SECRET.to_string(),
            "Bearer ".to_string(),
            "Bearer".to_string(),
        ] {
            assert!(!authorized(Some(&header(&wrong)), &token), "{wrong:?}");
        }
        assert!(!authorized(None, &token));
    }

    #[test]
    fn the_token_never_prints() {
        let token = Token::new(SECRET).unwrap();
        assert!(!format!("{token:?}").contains(SECRET));
    }

    #[test]
    fn a_token_needs_exactly_one_source() {
        assert!(load_token(None, None).is_err());
        assert!(load_token(Some("A"), Some(Path::new("b"))).is_err());
        let missing = load_token(Some("PMUX_MCP_TEST_TOKEN_NOT_SET"), None).unwrap_err();
        assert!(missing.contains("PMUX_MCP_TEST_TOKEN_NOT_SET"));
    }

    #[cfg(unix)]
    #[test]
    fn a_token_file_open_to_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("pmux-mcp-token-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("token");
        std::fs::write(&path, format!("{SECRET}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_token(None, Some(&path)).unwrap_err();
        assert!(error.contains("readable by group or others"), "{error}");
        assert!(!error.contains(SECRET));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let token = load_token(None, Some(&path)).unwrap();
        assert!(authorized(
            Some(&header(&format!("Bearer {SECRET}"))),
            &token
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
