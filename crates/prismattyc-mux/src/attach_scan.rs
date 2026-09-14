//! Scan live `pmux-attach` clients.
//!
//! Used by `pmux doctor` / `kick` / `ls` to tell a host-side viewer
//! from a nested attach inside a pane child tree. No control-plane verbs.
//! Process listing goes through [`crate::procinfo`] so macOS does not
//! depend on `/proc`.

use std::ffi::OsStr;
use std::path::Path;

use crate::pid_in_tree;

/// A live `pmux-attach` process bound to a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachClient {
    pub pid: u32,
    pub session: Option<String>,
    pub pane: Option<u64>,
}

/// Nested = attach pid lives under a pane child. Otherwise a host-side viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    Nested,
    Viewer,
}

/// Parse a NUL-split attach cmdline. `None` if argv0 is not `pmux-attach`
/// or `--socket` is missing / does not match `socket`.
pub fn parse_attach_client(args: &[&[u8]], socket: &Path) -> Option<AttachClient> {
    let argv0 = std::str::from_utf8(args.first()?).ok()?;
    let name = Path::new(argv0).file_name();
    if name != Some(OsStr::new("pmux-attach")) {
        return None;
    }
    let want = socket.as_os_str().as_encoded_bytes();
    let mut seen_socket = false;
    let mut session = None;
    let mut pane = None;
    let mut i = 1;
    while i < args.len() {
        match args[i] {
            b"--socket" => {
                let value = args.get(i + 1)?;
                if *value != want {
                    return None;
                }
                seen_socket = true;
                i += 2;
            }
            b"--session" => {
                let value = std::str::from_utf8(args.get(i + 1)?).ok()?;
                session = Some(value.to_string());
                i += 2;
            }
            b"--pane" => {
                let value = std::str::from_utf8(args.get(i + 1)?).ok()?;
                pane = Some(value.parse().ok()?);
                i += 2;
            }
            _ => i += 1,
        }
    }
    seen_socket.then_some(AttachClient {
        pid: 0,
        session,
        pane,
    })
}

/// Same-uid process walk. Prefix / wrapper argv never matches: argv0 basename
/// must be exactly `pmux-attach` and `--socket` must be the next token.
pub fn scan_attach_clients(socket: &Path) -> Vec<AttachClient> {
    let mut out = Vec::new();
    for pid in crate::procinfo::pids() {
        let Some(args) = crate::procinfo::cmdline(pid) else {
            continue;
        };
        let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        if let Some(mut client) = parse_attach_client(&refs, socket) {
            client.pid = pid;
            out.push(client);
        }
    }
    out.sort_by_key(|client| client.pid);
    out
}

/// Whether this attach should be attributed to `session`.
///
/// `--pane` wins (must be one of `pane_ids`). `--session` matches name or id.
/// No selector means attach's default: first pane of the first snapshot session.
pub fn attach_targets_session(
    client: &AttachClient,
    session_name: &str,
    session_id: u64,
    pane_ids: &[u64],
    is_first_session: bool,
) -> bool {
    if let Some(pane) = client.pane {
        return pane_ids.contains(&pane);
    }
    if let Some(key) = client.session.as_deref() {
        return key == session_name || key == session_id.to_string();
    }
    is_first_session
}

#[must_use]
pub fn classify_attach(client: &AttachClient, pane_child_pids: &[u32]) -> AttachKind {
    if pane_child_pids
        .iter()
        .any(|&root| pid_in_tree(root, client.pid))
    {
        AttachKind::Nested
    } else {
        AttachKind::Viewer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(parts: &'a [&'a str]) -> Vec<&'a [u8]> {
        parts.iter().map(|s| s.as_bytes()).collect()
    }

    #[test]
    fn rejects_non_attach_and_socket_mismatch() {
        let sock = Path::new("/run/user/1000/prismattyc/pmux.sock");
        assert!(
            parse_attach_client(&args(&["pmuxd", "--socket", sock.to_str().unwrap()]), sock)
                .is_none()
        );
        assert!(parse_attach_client(
            &args(&[
                "pmux-attach",
                "--socket",
                "/tmp/other.sock",
                "--session",
                "work"
            ]),
            sock
        )
        .is_none());
        assert!(parse_attach_client(&args(&["strace", "-f", "pmux-attach"]), sock).is_none());
        assert!(parse_attach_client(
            &args(&["prism-mux-attach", "--socket", sock.to_str().unwrap()]),
            sock
        )
        .is_none());
    }

    #[test]
    fn parses_session_and_pane() {
        let sock = Path::new("/tmp/pmux.sock");
        let client = parse_attach_client(
            &args(&[
                "/home/x/.cargo/bin/pmux-attach",
                "--socket",
                "/tmp/pmux.sock",
                "--session",
                "cursor-la",
            ]),
            sock,
        )
        .unwrap();
        assert_eq!(client.session.as_deref(), Some("cursor-la"));
        assert_eq!(client.pane, None);

        let client = parse_attach_client(
            &args(&["pmux-attach", "--pane", "5", "--socket", "/tmp/pmux.sock"]),
            sock,
        )
        .unwrap();
        assert_eq!(client.pane, Some(5));

        let client = parse_attach_client(
            &args(&[
                "pmux-attach",
                "--socket",
                "/tmp/pmux.sock",
                "--session",
                "work",
            ]),
            sock,
        )
        .unwrap();
        assert_eq!(client.session.as_deref(), Some("work"));
    }

    #[test]
    fn prefix_socket_path_does_not_match() {
        let sock = Path::new("/tmp/foo");
        assert!(
            parse_attach_client(&args(&["pmux-attach", "--socket", "/tmp/foo.sock"]), sock)
                .is_none()
        );
    }

    #[test]
    fn targeting_rules() {
        let by_name = AttachClient {
            pid: 1,
            session: Some("work".into()),
            pane: None,
        };
        let by_id = AttachClient {
            pid: 2,
            session: Some("5".into()),
            pane: None,
        };
        let by_pane = AttachClient {
            pid: 3,
            session: None,
            pane: Some(9),
        };
        let implicit = AttachClient {
            pid: 4,
            session: None,
            pane: None,
        };
        assert!(attach_targets_session(&by_name, "work", 5, &[9], false));
        assert!(attach_targets_session(&by_id, "work", 5, &[9], false));
        assert!(attach_targets_session(&by_pane, "other", 1, &[9], false));
        assert!(!attach_targets_session(&by_pane, "work", 5, &[1], false));
        assert!(attach_targets_session(&implicit, "default", 1, &[1], true));
        assert!(!attach_targets_session(&implicit, "work", 5, &[9], false));
    }

    #[test]
    fn scan_missing_socket_is_empty_without_proc() {
        let clients = scan_attach_clients(Path::new("/tmp/prism-no-such-attach.sock"));
        assert!(clients.is_empty(), "{clients:?}");
    }
}
