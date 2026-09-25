//! Freeze the visible terminal's identity, including nested attaches, before a move.
use super::*;
type ViewerChain = Vec<(u32, u64)>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Remote {
    pub pane: u64,
    pub session: u64,
    pub name: String,
    pub owner: Option<String>,
    pub child: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub host_pane: PaneId,
    owner: Option<String>,
    local_pid: Option<u32>,
    root: Option<Remote>,
    pub remote: Option<Remote>,
    pub viewers: ViewerChain,
}
fn remote(snapshot: &prismattyc_mux::Snapshot, pane: u64) -> Result<Remote> {
    snapshot
        .sessions
        .iter()
        .find_map(|session| {
            session
                .windows
                .iter()
                .flat_map(|w| &w.panes)
                .find(|p| p.id == pane)
                .map(|p| Remote {
                    pane,
                    session: session.id,
                    name: session.name.clone(),
                    owner: session.space_id.clone(),
                    child: p.child_pid,
                })
        })
        .context("terminal is no longer available")
}
/// Resolve only foreground viewers. Their live report wins over startup argv.
fn descend(
    socket: &Path,
    snapshot: &prismattyc_mux::Snapshot,
    mut root_pid: Option<u32>,
    mut target: Option<Remote>,
) -> Result<(Option<Remote>, ViewerChain)> {
    let mut viewers = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(root) = root_pid {
        anyhow::ensure!(
            seen.insert(root) && seen.len() <= 16,
            "nested terminal cycle"
        );
        #[cfg(windows)]
        let processes = prismattyc_mux::procinfo::WindowsProcessSnapshot::capture(&[root])
            .context("cannot inspect the nested terminal process tree")?;
        #[cfg(windows)]
        let mut clients = attach_adopt::snapshot_attach_clients(&processes, &[root], socket);
        #[cfg(not(windows))]
        let mut clients = attach_adopt::subtree_attach_clients(&[root], socket);
        #[cfg(windows)]
        let root_args = processes.cmdline(root);
        #[cfg(not(windows))]
        let root_args = prismattyc_mux::procinfo::cmdline(root);
        if let Some(args) = root_args {
            let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            if let Some(mut client) = prismattyc_mux::parse_attach_client(&refs, socket) {
                client.pid = root;
                clients.push(client);
            }
        }
        let mut foreground = Vec::new();
        for client in clients {
            #[cfg(windows)]
            let is_foreground = processes.in_terminal_foreground(root, client.pid);
            #[cfg(not(windows))]
            let is_foreground = prismattyc_mux::procinfo::in_terminal_foreground(root, client.pid);
            match is_foreground {
                Some(true) => foreground.push(client),
                Some(false) => {}
                None => anyhow::bail!("cannot identify the foreground nested terminal"),
            }
        }
        anyhow::ensure!(
            foreground.len() <= 1,
            "more than one foreground nested terminal"
        );
        let Some(client) = foreground.first() else {
            break;
        };
        let pane = prismattyc_mux::attach_focus::read(socket, client.pid)
            .context("reattach this nested session once to update its move controls")?;
        let next = remote(snapshot, pane)?;
        viewers.push((client.pid, pane));
        root_pid = next.child;
        target = Some(next);
    }
    Ok((target, viewers))
}
pub(super) fn resolve(host: &HostState) -> Result<Target> {
    let pane = host.mux.focused_id();
    let socket = host_mux_socket().context("mux socket is unavailable")?;
    let snapshot = match attach_log::live_snapshot() {
        Some(snapshot) => snapshot,
        None if host.mux.remote_pane_id(pane).is_none() => prismattyc_mux::Snapshot {
            sequence: 0,
            sessions: Vec::new(),
        },
        None => anyhow::bail!("daemon unavailable"),
    };
    let root = host
        .mux
        .remote_pane_id(pane)
        .map(|id| remote(&snapshot, id))
        .transpose()?;
    let local_pid = host.mux.pane(pane).and_then(|p| p.child_pid());
    let pid = root.as_ref().and_then(|r| r.child).or(local_pid);
    let (effective, viewers) = descend(&socket, &snapshot, pid, root.clone())?;
    anyhow::ensure!(
        effective.is_some() || host.mux.is_retained_local_terminal(pane),
        "attach this pane to a session first"
    );
    Ok(Target {
        host_pane: pane,
        owner: host.mux.space_id.clone(),
        local_pid,
        root,
        remote: effective,
        viewers,
    })
}
pub(super) fn begin(host: &mut HostState) -> bool {
    host.move_target = None;
    match resolve(host) {
        Ok(target) => {
            host.move_target = Some(target);
            true
        }
        Err(error) => {
            rail_toast(host, &format!("Move unavailable: {error}"));
            false
        }
    }
}
pub(super) fn take_valid(host: &mut HostState) -> Result<Target> {
    let pinned = host.move_target.take();
    let current = resolve(host)?;
    if let Some(pinned) = pinned {
        anyhow::ensure!(
            pinned == current,
            "the terminal changed; reopen Move to Space"
        );
    }
    Ok(current)
}
pub(super) fn detach_viewer(target: &Target) -> Result<()> {
    if let Some((pid, pane)) = target.viewers.last() {
        let socket = host_mux_socket().context("mux socket is unavailable")?;
        prismattyc_mux::attach_focus::request_detach(&socket, *pid, *pane)?;
    }
    Ok(())
}
