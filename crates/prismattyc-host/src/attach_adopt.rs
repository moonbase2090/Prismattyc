//! Adopt nested TTY attaches (PT-210).
//!
//! `pmux new NAME` or `pmux attach` typed into a host pane runs a
//! `pmux-attach` child under that pane's shell. The host never spawned it,
//! so the pane carried no attach mark: the tab cache omitted it, `pmux
//! space save` recorded no tabs, and `space open` fanned the sessions out
//! one tab per session. The host now walks each unmarked pane's process
//! tree once a second, matches a `pmux-attach` on its own socket, and marks
//! the pane with that session. When the host can open that session's
//! pane log, it promotes the pane to a log replica and kills the nested
//! attach (PT-306). Otherwise the mark is dropped when the attach exits.

use std::collections::HashMap;
#[cfg(not(windows))]
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use prismattyc_mux::procinfo::{children_of, cmdline};
use prismattyc_mux::{parse_attach_client, AttachClient, PaneId};

/// Poll cadence. A /proc walk only runs when an unmarked pane has children.
pub(crate) const POLL: Duration = Duration::from_secs(1);

/// One live mux session: ephemeral id, stable name, pane ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionEntry {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) pane_ids: Vec<u64>,
}

/// Panes this host adopted: pane → nested `pmux-attach` pid.
#[derive(Debug, Default)]
pub(crate) struct Adopted {
    pub(crate) by_pane: HashMap<PaneId, u32>,
    next: Option<Instant>,
}

impl Adopted {
    /// Whether a poll is due; arms the next one when it is.
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        if self.next.is_some_and(|next| now < next) {
            return false;
        }
        self.next = Some(now + POLL);
        true
    }
}

/// The session a `pmux-attach` argv targets: `--pane` wins, then `--session`
/// by name or id, else attach's default (the first session).
pub(crate) fn resolve_target<'a>(
    client: &AttachClient,
    directory: &'a [SessionEntry],
) -> Option<&'a SessionEntry> {
    if let Some(pane) = client.pane {
        return directory
            .iter()
            .find(|session| session.pane_ids.contains(&pane));
    }
    if let Some(key) = client.session.as_deref() {
        return directory
            .iter()
            .find(|session| session.name == key || session.id.to_string() == key);
    }
    directory.first()
}

/// `pmux-attach` clients on `socket` under any of `roots`: a walk of each
/// pane's process subtree, so the cost follows the pane, not the machine.
#[cfg(not(windows))]
pub(crate) fn subtree_attach_clients(roots: &[u32], socket: &Path) -> Vec<AttachClient> {
    let mut seen: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = roots.iter().copied().collect();
    let mut out = Vec::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        queue.extend(children_of(pid));
        if roots.contains(&pid) {
            continue;
        }
        let Some(args) = cmdline(pid) else {
            continue;
        };
        let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        if let Some(mut client) = parse_attach_client(&refs, socket) {
            client.pid = pid;
            out.push(client);
        }
    }
    out
}

#[cfg(windows)]
pub(crate) fn snapshot_attach_clients(
    snapshot: &prismattyc_mux::procinfo::WindowsProcessSnapshot,
    roots: &[u32],
    socket: &Path,
) -> Vec<AttachClient> {
    snapshot
        .pids()
        .filter(|pid| !roots.contains(pid))
        .filter_map(|pid| {
            let args = snapshot.cmdline(pid)?;
            let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            let mut client = parse_attach_client(&refs, socket)?;
            client.pid = pid;
            Some(client)
        })
        .collect()
}

/// One adoption: pane, attach pid, session id, session name.
pub(crate) type Adoption<P> = (P, u32, String, String);

/// Pure matching of `(pane, root pid)` candidates against attach clients.
/// A shell runs one foreground attach, so the first client found inside a
/// pane's process tree wins. `in_tree(root, pid)` is the process-tree test.
pub(crate) fn assign<P: Copy>(
    candidates: &[(P, u32)],
    clients: &[AttachClient],
    directory: &[SessionEntry],
    in_tree: impl Fn(u32, u32) -> bool,
) -> Vec<Adoption<P>> {
    let mut out = Vec::new();
    for &(pane, root) in candidates {
        let hit = clients
            .iter()
            .filter(|client| client.pid != 0 && in_tree(root, client.pid))
            .find_map(|client| resolve_target(client, directory).map(|session| (client, session)));
        if let Some((client, session)) = hit {
            out.push((
                pane,
                client.pid,
                session.id.to_string(),
                session.name.clone(),
            ));
        }
    }
    out
}

/// True when promote replaced the nested PTY with a log replica, so the
/// PT-210 adopted mark must drop.
fn adopt_clears_nested_mark(promoted: bool, log_backed: bool) -> bool {
    promoted && log_backed
}

/// Discover and mark real nested attaches for the host adoption path.
///
/// Keep process-tree walking and mark mutation together so seam tests can
/// drive the same operation as the host instead of setting state themselves.
pub(crate) fn adopt_candidates(
    mux: &mut crate::mux::MuxRuntime,
    adopted: &mut Adopted,
    candidates: &[(PaneId, u32)],
    socket: &Path,
    directory: &[SessionEntry],
) -> Vec<Adoption<PaneId>> {
    let roots: Vec<u32> = candidates.iter().map(|(_, pid)| *pid).collect();
    #[cfg(not(windows))]
    let assignments = {
        let clients = subtree_attach_clients(&roots, socket);
        assign(candidates, &clients, directory, prismattyc_mux::pid_in_tree)
    };
    #[cfg(windows)]
    let assignments = {
        let Some(snapshot) = prismattyc_mux::procinfo::WindowsProcessSnapshot::capture(&roots) else {
            return Vec::new();
        };
        let clients = snapshot_attach_clients(&snapshot, &roots, socket);
        assign(candidates, &clients, directory, |root, pid| {
            snapshot.contains(root, pid) && snapshot.in_terminal_foreground(root, pid) == Some(true)
        })
    };
    for (pane, pid, id, name) in &assignments {
        mux.mark_attach_session(*pane, id.clone(), name.clone());
        let promoted = match mux.promote_to_log_replica(*pane, id, name, socket) {
            Ok(ok) => ok,
            Err(error) => {
                eprintln!("prismattyc-host: promote session {id} failed: {error:#}");
                adopted.by_pane.insert(*pane, *pid);
                continue;
            }
        };
        if adopt_clears_nested_mark(promoted, mux.is_log_backed(*pane)) {
            adopted.by_pane.remove(pane);
        } else {
            adopted.by_pane.insert(*pane, *pid);
        }
    }
    assignments
}

/// Clear adopted marks whose pane or real attach process has gone away.
pub(crate) fn clear_gone(
    mux: &mut crate::mux::MuxRuntime,
    adopted: &mut Adopted,
    live_panes: &[PaneId],
) -> Vec<PaneId> {
    let gone: Vec<PaneId> = adopted
        .by_pane
        .iter()
        .filter(|(pane, pid)| {
            !live_panes.contains(pane) || !prismattyc_mux::procinfo::pid_alive(**pid)
        })
        .map(|(pane, _)| *pane)
        .collect();
    for pane in &gone {
        adopted.by_pane.remove(pane);
        mux.clear_attach_session(*pane);
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(pid: u32, session: Option<&str>, pane: Option<u64>) -> AttachClient {
        AttachClient {
            pid,
            session: session.map(str::to_string),
            pane,
        }
    }

    fn directory() -> Vec<SessionEntry> {
        vec![
            SessionEntry {
                id: 5,
                name: "claude".into(),
                pane_ids: vec![5],
            },
            SessionEntry {
                id: 6,
                name: "kiro".into(),
                pane_ids: vec![6],
            },
            SessionEntry {
                id: 7,
                name: "grok".into(),
                pane_ids: vec![7, 8],
            },
        ]
    }

    fn tree(root: u32, pid: u32) -> bool {
        // shell 100 → attach 336; shell 200 → attach 396; shell 300 → attach 546
        matches!((root, pid), (100, 336) | (200, 396) | (300, 546))
    }

    #[test]
    fn assigns_each_pane_its_nested_attach_by_session_name() {
        let candidates = [(1u8, 100), (2u8, 200), (3u8, 300)];
        let clients = [
            client(336, Some("claude"), None),
            client(396, Some("kiro"), None),
            client(546, Some("grok"), None),
        ];
        let got = assign(&candidates, &clients, &directory(), tree);
        assert_eq!(
            got,
            vec![
                (1u8, 336, "5".into(), "claude".into()),
                (2u8, 396, "6".into(), "kiro".into()),
                (3u8, 546, "7".into(), "grok".into()),
            ]
        );
    }

    #[test]
    fn session_id_pane_and_default_selectors_resolve() {
        let candidates = [(1u8, 100), (2u8, 200), (3u8, 300)];
        let clients = [
            client(336, Some("6"), None),
            client(396, None, Some(8)),
            client(546, None, None),
        ];
        let got = assign(&candidates, &clients, &directory(), tree);
        assert_eq!(got[0].3, "kiro");
        assert_eq!(got[1].3, "grok");
        assert_eq!(got[2].3, "claude", "no selector is attach's default");
    }

    #[test]
    fn bare_shell_unknown_session_and_zero_pid_are_skipped() {
        let candidates = [(1u8, 100), (2u8, 200), (9u8, 900)];
        let clients = [
            client(336, Some("nope"), None),
            client(0, Some("kiro"), None),
        ];
        let got = assign(&candidates, &clients, &directory(), tree);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn adopt_clears_nested_mark_only_when_promoted_and_log_backed() {
        let cases = [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ];
        for (promoted, log_backed, clears) in cases {
            assert_eq!(
                adopt_clears_nested_mark(promoted, log_backed),
                clears,
                "promoted={promoted} log_backed={log_backed}"
            );
        }
    }

    #[test]
    fn due_arms_one_poll_per_second() {
        let mut adopted = Adopted::default();
        let t0 = Instant::now();
        assert!(adopted.due(t0));
        assert!(!adopted.due(t0 + Duration::from_millis(500)));
        assert!(adopted.due(t0 + POLL));
    }
}
